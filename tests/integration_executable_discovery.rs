//! Isolate PATH mutation in a process with no test-runner or runtime threads.

use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use inxm_local::app::engine::AppSettings;
use inxm_local::executor::{ExecutorConfig, execute};
use inxm_local::hostenv::{self, EnvProbe};
use inxm_local::plan::types::{CodeCallConfig, Plan, PlanMetadata, PlanStep, StepConfig};
use inxm_local::storage::{StorageRoot, runs::RunStatus};
use inxm_local::tools::adapters::{mcp, subprocess};
use inxm_local::tools::catalog::{McpConfig, McpTransport, SubprocessConfig, ToolCatalog};
use serde_json::json;

const FIXTURE_VALUE: &str = "configured-folder";
const CHILD_FLAG: &str = "--discovery-child";
const COMMAND_FLAG: &str = "--fixture-command";
const MCP_FLAG: &str = "--fixture-mcp";
const FIXTURE_TIMEOUT_SECS: u64 = 15;

fn main() {
    let arguments = std::env::args().collect::<Vec<_>>();
    match arguments.get(1).map(String::as_str) {
        Some(COMMAND_FLAG) => println!("{}", json!({"source": FIXTURE_VALUE})),
        Some(MCP_FLAG) => serve_fixture_mcp(),
        Some(CHILD_FLAG) => verify_child(Path::new(&arguments[2])),
        _ => verify_isolated_discovery(),
    }
}

fn serve_fixture_mcp() {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let request: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        let Some(id) = request.get("id") else {
            continue;
        };
        let result = match request["method"].as_str().unwrap() {
            "initialize" => json!({
                "protocolVersion":"2024-11-05", "capabilities":{"tools":{}},
                "serverInfo":{"name":"discovery-fixture","version":"1"}
            }),
            "tools/call" => json!({
                "content":[{"type":"text","text":json!({"source":FIXTURE_VALUE}).to_string()}],
                "isError":false
            }),
            other => panic!("unexpected fixture method {other}"),
        };
        println!("{}", json!({"jsonrpc":"2.0","id":id,"result":result}));
        std::io::stdout().flush().unwrap();
    }
}

fn verify_isolated_discovery() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("extra executables with spaces");
    std::fs::create_dir(&folder).unwrap();
    let fixture = folder.join(if cfg!(windows) { "gh.exe" } else { "gh" });
    std::fs::copy(std::env::current_exe().unwrap(), &fixture).unwrap();
    let settings = AppSettings {
        extra_executable_dirs: vec![folder.to_string_lossy().into_owned()],
        ..AppSettings::default()
    };
    let settings_path = root.path().join("settings.json");
    settings.save(&settings_path).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .arg(CHILD_FLAG)
        .arg(&settings_path)
        .env("INXM_TELEMETRY", "off")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated discovery failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!(
        "Configured-folder compiler context, subprocess, stdio MCP, and generated-code discovery passed."
    );
}

fn verify_child(settings_path: &Path) {
    let settings = AppSettings::load(settings_path);
    assert!(
        hostenv::configure_executable_dirs(&settings.extra_executable_dirs)
            .unwrap()
            .is_empty()
    );
    let expected = Path::new(&settings.extra_executable_dirs[0]).join(if cfg!(windows) {
        "gh.exe"
    } else {
        "gh"
    });
    assert_eq!(hostenv::resolve_program("gh"), expected);
    let environment = EnvProbe::detect();
    assert!(environment.runners.contains(&"gh"));
    assert!(
        environment
            .compiler_context()
            .contains("Extra executable folders")
    );

    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let config = SubprocessConfig {
            command: "gh".to_owned(),
            args: vec![COMMAND_FLAG.to_owned()],
            env: Default::default(),
            working_dir: None,
        };
        let output = subprocess::run(&config, &Default::default(), Some(FIXTURE_TIMEOUT_SECS))
            .await
            .unwrap();
        assert_eq!(output.data["source"], FIXTURE_VALUE);

        let mcp_config = McpConfig {
            tool_name: "fixture".to_owned(),
            transport: McpTransport::Stdio {
                server_command: "gh".to_owned(),
                server_args: vec![MCP_FLAG.to_owned()],
                server_env: Default::default(),
            },
        };
        let output = mcp::run(&mcp_config, &Default::default(), Some(FIXTURE_TIMEOUT_SECS))
            .await
            .unwrap();
        assert!(output.stdout.contains(FIXTURE_VALUE));

        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "Configured executable in generated code".to_owned(),
            description: None,
            inputs: vec![],
            config: Default::default(),
            steps: vec![PlanStep {
                id: "code".to_owned(),
                name: "Invoke gh by name".to_owned(),
                description: None,
                config: StepConfig::CodeCall(CodeCallConfig {
                    language: if cfg!(windows) { "powershell" } else { "sh" }.to_owned(),
                    inline: Some(format!("gh {COMMAND_FLAG}")),
                    file: None,
                    args: vec![],
                    stdin: None,
                    env: Default::default(),
                    working_dir: None,
                    timeout_secs: Some(FIXTURE_TIMEOUT_SECS),
                }),
                depends_on: vec![],
                outputs: vec![],
                timeout_secs: Some(FIXTURE_TIMEOUT_SECS),
                retry: None,
            }],
            outputs: vec![],
        };
        let root = tempfile::tempdir().unwrap();
        let run = execute(
            plan,
            ExecutorConfig {
                inputs: Default::default(),
                timeout_secs: Some(FIXTURE_TIMEOUT_SECS),
                storage: Arc::new(StorageRoot::open(root.path()).unwrap()),
                catalog: ToolCatalog::new(vec![]),
                progress: None,
                human: None,
                llm_keys: Default::default(),
                source: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(run.status, RunStatus::Succeeded, "{run:?}");
        assert!(
            run.step_runs["code"]
                .stdout
                .as_deref()
                .unwrap()
                .contains(FIXTURE_VALUE)
        );

        // An explicit tool PATH must remain authoritative over configured dirs.
        let empty = tempfile::tempdir().unwrap();
        let overridden = SubprocessConfig {
            env: [(
                "PATH".to_owned(),
                empty.path().to_string_lossy().into_owned(),
            )]
            .into_iter()
            .collect(),
            ..config
        };
        assert!(
            subprocess::run(&overridden, &Default::default(), Some(FIXTURE_TIMEOUT_SECS))
                .await
                .is_err()
        );
    });
}
