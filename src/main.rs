#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! `inxm-local` — desktop entry point.

use inxm_local::app::{InxmApp, engine, mcp_server, single_instance};
use inxm_local::plan::types::{
    Plan, PlanInput, PlanMetadata, PlanOutput, PlanStep, StepConfig, ToolCallConfig,
};
use inxm_local::storage::StorageRoot;
#[cfg(target_os = "linux")]
use winit::platform::x11::EventLoopBuilderExtX11 as _;

const APP_ID: &str = "ai.inxm.local";
const APP_TITLE: &str = "INXM // Local";
const INITIAL_WINDOW_SIZE: [f32; 2] = [1180.0, 780.0];
const MIN_WINDOW_SIZE: [f32; 2] = [860.0, 560.0];

fn main() -> eframe::Result<()> {
    if let Some(value) = std::env::args().find_map(|arg| {
        arg.strip_prefix("--set-telemetry=")
            .map(|value| value.to_owned())
    }) {
        std::process::exit(run_set_telemetry(&value));
    }
    if std::env::var("INXM_MCP_SELF_TEST").is_ok() {
        run_mcp_self_test();
        return Ok(());
    }
    if std::env::var("INXM_MCP_ONLY").is_ok() {
        run_mcp_only();
        return Ok(());
    }
    if std::env::var("INXM_HEADLESS").is_ok() || std::env::args().any(|arg| arg == "--headless") {
        run_headless();
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Opening `INXM // Local` while it is already running used to
    // spawn a second process and window instead of surfacing the existing
    // one. Claim a per-data-dir socket before doing anything else UI-related;
    // if another process already holds it, ask it to show its window and
    // exit instead of starting a second instance. The socket is not claimed
    // for the headless/MCP-only/self-test paths above, since those are meant
    // to run alongside (or instead of) the desktop window.
    let paths = engine::DataPaths::resolve();
    let primary = match single_instance::InstanceGuard::acquire(&paths.data_dir) {
        Ok(single_instance::InstanceGuard::Secondary) => {
            println!(
                "INXM // Local is already running — asked the existing instance to show its window."
            );
            return Ok(());
        }
        Ok(single_instance::InstanceGuard::Primary(primary)) => Some(primary),
        Err(error) => {
            tracing::warn!(
                operation = "single_instance.acquire",
                app_version = env!("CARGO_PKG_VERSION"),
                triggered_by = "application",
                outcome = "failure",
                error = %error,
                "single-instance guard unavailable; a second launch may open a second window"
            );
            None
        }
    };

    // `--start-hidden` is what login/startup entries pass: create the window
    // invisible so the app comes up in the system tray only — the same state
    // close-to-tray leaves it in. The tray "Open" action, or launching the
    // app a second time (via the single-instance listener above), shows it.
    let start_hidden = std::env::var("INXM_START_HIDDEN").is_ok()
        || std::env::args().any(|arg| arg == "--start-hidden");

    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/favicon512.png"))
        .expect("bundled app icon is a valid PNG");
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(INITIAL_WINDOW_SIZE)
            .with_min_inner_size(MIN_WINDOW_SIZE)
            .with_app_id(APP_ID)
            .with_title(APP_TITLE)
            .with_visible(!start_hidden)
            .with_icon(icon),
        ..Default::default()
    };
    prefer_x11_for_tray_lifecycle(&mut options);

    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(move |cc| {
            // The listener thread is spawned here (rather than right after
            // `acquire`) because it needs `cc.egui_ctx` to restore the
            // window when a later instance asks us to show it. On Windows,
            // OS foreground-focus restrictions mean the window may only
            // flash in the taskbar instead of taking focus outright — that
            // is accepted behavior, matching the tray's own restore path.
            if let Some(primary) = primary {
                let ctx = cc.egui_ctx.clone();
                primary.spawn_listener(move || {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    ctx.request_repaint();
                });
            }
            Ok(Box::new(InxmApp::new(cc)))
        }),
    )
}

/// Wayland does not allow winit to hide or restore a top-level window. Prefer
/// X11 through XWayland when it is available so close-to-tray can both remove
/// the window from the dock and restore it from the tray menu.
#[cfg(target_os = "linux")]
fn prefer_x11_for_tray_lifecycle(options: &mut eframe::NativeOptions) {
    let x11_available = std::env::var_os("DISPLAY").is_some_and(|display| !display.is_empty());
    let force_wayland = std::env::var_os("INXM_FORCE_WAYLAND").is_some();
    if x11_available && !force_wayland {
        options.event_loop_builder = Some(Box::new(|builder| {
            builder.with_x11();
        }));
    }
}

#[cfg(not(target_os = "linux"))]
fn prefer_x11_for_tray_lifecycle(_options: &mut eframe::NativeOptions) {}

/// Run the MCP server and the cron scheduler without a desktop window, so
/// schedules keep firing after the window would normally close. A lock file
/// next to the schedules store guarantees only one scheduler per data dir.
fn run_headless() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let paths = engine::DataPaths::resolve();
    let settings = engine::AppSettings::load(&paths.settings_path);
    // Opt-in only; a no-op unless the user enabled it (docs/telemetry.md).
    inxm_local::telemetry::record_app_started(
        settings.telemetry_enabled,
        inxm_local::telemetry::Channel::Headless,
    );
    inxm_local::telemetry::usage::flush(&paths.data_dir, &paths.settings_path);
    let handles = inxm_local::app::run_headless(paths, settings);

    match &handles.scheduler {
        engine::SchedulerOutcome::Running { pid } => {
            println!("INXM scheduler running headless (pid {pid})");
        }
        engine::SchedulerOutcome::Blocked { holder_pid } => match holder_pid {
            Some(pid) => println!(
                "INXM scheduler already running in another instance (pid {pid}); \
                 not started here"
            ),
            None => {
                println!("INXM scheduler already running in another instance; not started here")
            }
        },
        engine::SchedulerOutcome::Failed { error } => {
            eprintln!("INXM scheduler failed to start: {error}");
            std::process::exit(1);
        }
    }

    loop {
        match handles.mcp_status.recv() {
            Ok(mcp_server::ServerStatus::Starting { .. }) => {}
            Ok(mcp_server::ServerStatus::Running {
                port,
                fallback_from,
            }) => {
                if let Some(requested) = fallback_from {
                    eprintln!(
                        "INXM MCP server: configured port {requested} was unavailable, \
                         falling back to an ephemeral port"
                    );
                }
                println!("INXM MCP server listening on http://127.0.0.1:{port}/mcp");
            }
            Ok(mcp_server::ServerStatus::Failed { port, error }) => {
                eprintln!("INXM MCP server failed on 127.0.0.1:{port}: {error}");
                std::process::exit(1);
            }
            Err(error) => {
                eprintln!("INXM MCP server stopped: {error}");
                std::process::exit(1);
            }
        }
    }
}

/// Run the local MCP endpoint without creating a desktop window. This is
/// useful for automation, CI, and recovery when the native window cannot be
/// initialized. Unlike the desktop status badge, startup failures are printed
/// and terminate the process with a non-zero exit code.
fn run_mcp_only() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let paths = engine::DataPaths::resolve();
    let settings = engine::AppSettings::load(&paths.settings_path);
    // Opt-in only; a no-op unless the user enabled it (docs/telemetry.md).
    inxm_local::telemetry::record_app_started(
        settings.telemetry_enabled,
        inxm_local::telemetry::Channel::McpOnly,
    );
    inxm_local::telemetry::usage::flush(&paths.data_dir, &paths.settings_path);
    let status_rx = mcp_server::spawn(paths, settings.mcp_port);
    loop {
        match status_rx.recv() {
            Ok(mcp_server::ServerStatus::Starting { .. }) => {}
            Ok(mcp_server::ServerStatus::Running {
                port,
                fallback_from,
            }) => {
                if let Some(requested) = fallback_from {
                    eprintln!(
                        "INXM MCP server: configured port {requested} was unavailable, \
                         falling back to an ephemeral port"
                    );
                }
                println!("INXM MCP server listening on http://127.0.0.1:{port}/mcp");
                break;
            }
            Ok(mcp_server::ServerStatus::Failed { port, error }) => {
                eprintln!("INXM MCP server failed on 127.0.0.1:{port}: {error}");
                std::process::exit(1);
            }
            Err(error) => {
                eprintln!("INXM MCP server stopped before startup: {error}");
                std::process::exit(1);
            }
        }
    }

    // The server owns its own Tokio runtime thread. Keep the process alive
    // while surfacing any later serve failure instead of parking forever.
    match status_rx.recv() {
        Ok(mcp_server::ServerStatus::Failed { port, error }) => {
            eprintln!("INXM MCP server failed on 127.0.0.1:{port}: {error}");
            std::process::exit(1);
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!("INXM MCP server stopped: {error}");
            std::process::exit(1);
        }
    }
}

/// One-shot settings write for `--set-telemetry=on|off`, used by the
/// installers (`packaging/install.sh`, `packaging/linux/install.sh`) to
/// record the disclosure prompt they show on a headless/CLI-only install —
/// the only entry points that never render the desktop onboarding card
/// (see `should_show_onboarding` in `src/app/mod.rs`). Never overwrites a
/// choice the user already made, so re-running an installer to update
/// never resets it. Exits without starting any server.
fn run_set_telemetry(value: &str) -> i32 {
    let enabled = match value {
        "on" => true,
        "off" => false,
        other => {
            eprintln!("--set-telemetry expects \"on\" or \"off\", got {other:?}");
            return 2;
        }
    };

    let paths = engine::DataPaths::resolve();
    let mut settings = engine::AppSettings::load(&paths.settings_path);
    if settings.onboarding_completed {
        // Already asked (by the desktop onboarding card or a previous
        // installer run) — leave the existing choice untouched.
        return 0;
    }
    settings.telemetry_enabled = Some(enabled);
    settings.onboarding_completed = true;
    if let Err(error) = settings.save(&paths.settings_path) {
        eprintln!("Could not save telemetry preference: {error}");
        return 1;
    }
    0
}

fn run_mcp_self_test() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let paths = engine::DataPaths::resolve();
    seed_self_test_plan(&paths);

    // The configured desktop port may already be occupied by a running app.
    // Use an ephemeral loopback port so this independent self-test is reliable.
    let port = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind ephemeral MCP self-test port")
        .local_addr()
        .expect("read ephemeral MCP self-test address")
        .port();
    let status_rx = mcp_server::spawn(paths, port);
    let port = loop {
        match status_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(mcp_server::ServerStatus::Running { port, .. }) => break port,
            Ok(mcp_server::ServerStatus::Failed { port, error }) => {
                panic!("MCP self-test server failed on 127.0.0.1:{port}: {error}");
            }
            Ok(mcp_server::ServerStatus::Starting { .. }) => {}
            Err(error) => panic!("MCP self-test timed out waiting for server: {error}"),
        }
    };

    let runtime = tokio::runtime::Runtime::new().expect("self-test tokio runtime");
    runtime.block_on(async move {
        let endpoint = format!("http://127.0.0.1:{port}/mcp");
        let client = reqwest::Client::new();
        rpc(&client, &endpoint, 1, "initialize", serde_json::json!({})).await;
        rpc(&client, &endpoint, 2, "tools/list", serde_json::json!({})).await;
        rpc_tool(&client, &endpoint, 3, "list_plans", serde_json::json!({})).await;
        rpc_tool(
            &client,
            &endpoint,
            4,
            "show_plan",
            serde_json::json!({ "plan_ref": "mcp-self-test" }),
        )
        .await;
        let execution = rpc_tool(
            &client,
            &endpoint,
            5,
            "execute_plan",
            serde_json::json!({
                "plan_ref": "mcp-self-test",
                "inputs": { "message": "hello through MCP inputs" }
            }),
        )
        .await;
        let run = &execution["result"]["structuredContent"]["run"];
        assert_eq!(run["inputs"]["message"], "hello through MCP inputs");
        assert!(
            run["step_runs"]["echo_input"]["stdout"]
                .as_str()
                .is_some_and(|stdout| stdout.contains("hello through MCP inputs")),
            "execute_plan did not resolve the MCP input: {run}"
        );
        let run_id = run["id"]
            .as_str()
            .expect("execute_plan returned run id")
            .to_owned();
        rpc_tool(
            &client,
            &endpoint,
            6,
            "inspect_run",
            serde_json::json!({ "run_id": run_id }),
        )
        .await;
        rpc_tool(
            &client,
            &endpoint,
            7,
            "schedule_plan",
            serde_json::json!({
                "plan_ref": "mcp-self-test",
                "cron": "*/5 * * * *",
                "inputs": { "message": "scheduled MCP input" }
            }),
        )
        .await;
        let schedules = rpc_tool(
            &client,
            &endpoint,
            8,
            "list_schedules",
            serde_json::json!({}),
        )
        .await;
        assert!(
            schedules["result"]["structuredContent"]["schedules"]
                .as_array()
                .is_some_and(|items| items
                    .iter()
                    .any(|item| { item["inputs"]["message"] == "scheduled MCP input" })),
            "schedule_plan did not persist inputs: {schedules}"
        );
        println!("MCP self-test passed on {endpoint}");
    });
}

fn seed_self_test_plan(paths: &engine::DataPaths) {
    let storage = StorageRoot::open(&paths.data_dir).expect("open self-test storage");
    let mut metadata = PlanMetadata::new(Some("MCP self-test plan".to_owned()));
    metadata.id = "mcp-self-test".to_owned();
    let plan = Plan {
        metadata,
        name: "mcp-self-test".to_owned(),
        description: Some("Deterministic input plan used by the MCP startup self-test.".to_owned()),
        inputs: vec![PlanInput {
            name: "message".to_owned(),
            description: Some("Message supplied by the caller or schedule".to_owned()),
            value_type: "string".to_owned(),
            required: true,
            default: None,
            input_kind: inxm_local::plan::types::InputKind::Value,
        }],
        config: Default::default(),
        steps: vec![PlanStep {
            id: "echo_input".to_owned(),
            name: "Echo input".to_owned(),
            description: None,
            config: StepConfig::ToolCall(ToolCallConfig {
                tool: "echo".to_owned(),
                arguments: [("message".to_owned(), serde_json::json!("${input.message}"))]
                    .into_iter()
                    .collect(),
            }),
            depends_on: vec![],
            outputs: vec![PlanOutput {
                name: "stdout".to_owned(),
                description: None,
                value_type: "string".to_owned(),
            }],
            timeout_secs: None,
            retry: None,
        }],
        outputs: Vec::new(),
    };
    storage.plans().save(&plan).expect("save self-test plan");
}

async fn rpc(
    client: &reqwest::Client,
    endpoint: &str,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let response = client
        .post(endpoint)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .send()
        .await
        .expect("send MCP request")
        .error_for_status()
        .expect("MCP HTTP success")
        .json::<serde_json::Value>()
        .await
        .expect("MCP JSON response");
    if response.get("error").is_some() {
        panic!("MCP call {method} failed: {response}");
    }
    response
}

async fn rpc_tool(
    client: &reqwest::Client,
    endpoint: &str,
    id: u64,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    rpc(
        client,
        endpoint,
        id,
        "tools/call",
        serde_json::json!({ "name": name, "arguments": arguments }),
    )
    .await
}
