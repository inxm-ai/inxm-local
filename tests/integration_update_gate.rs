//! Desktop and MCP processes must share the same update/work admission lock.

use std::path::Path;
use std::process::Command;

use inxm_local::app::update_gate;

const GATE_DIRECTORY_ENV: &str = "INXM_TEST_GATE_DIRECTORY";
const GATE_ACTION_ENV: &str = "INXM_TEST_GATE_ACTION";

#[test]
fn gate_child() {
    let Some(directory) = std::env::var_os(GATE_DIRECTORY_ENV) else {
        return;
    };
    let action = std::env::var(GATE_ACTION_ENV).unwrap();
    let directory = Path::new(&directory);
    match action.as_str() {
        "work_denied" => assert!(update_gate::admit(directory).is_err()),
        "install_denied" => {
            let pending = update_gate::begin(directory).unwrap();
            assert!(pending.quiescent(directory).is_err());
        }
        "work_allowed" => assert!(update_gate::admit(directory).is_ok()),
        _ => panic!("unknown gate test action"),
    }
}

fn child(directory: &Path, action: &str) {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "gate_child", "--nocapture"])
        .env(GATE_DIRECTORY_ENV, directory)
        .env(GATE_ACTION_ENV, action)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "cross-process gate {action} failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn active_work_and_installation_exclude_each_other_across_processes() {
    let directory = tempfile::tempdir().unwrap();
    let work = update_gate::admit(directory.path()).unwrap();
    child(directory.path(), "install_denied");
    drop(work);
    let pending = update_gate::begin(directory.path()).unwrap();
    child(directory.path(), "work_denied");
    let work = pending.quiescent(directory.path()).unwrap();
    let installation = pending.acquire(work);
    child(directory.path(), "work_denied");
    drop(installation);
    child(directory.path(), "work_allowed");
}
