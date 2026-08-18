//! Shared child-process lifecycle helpers for tool adapters.

use std::io;
use std::process::ExitStatus;
use tokio::process::{Child, Command};

#[cfg(unix)]
const SIGKILL: i32 = 9;

/// Put the child in an isolated process group where the platform supports it.
///
/// This lets timeout cleanup terminate descendants spawned by shell and MCP
/// tools instead of killing only the immediate child.
pub(super) fn isolate_process_group(command: &mut Command) {
    command.kill_on_drop(true);

    #[cfg(unix)]
    {
        command.process_group(0);
    }
}

/// Tracks the isolated process group so cancellation kills descendants even
/// when the adapter future is dropped before its async cleanup can run.
pub(super) struct ProcessGroupGuard {
    #[cfg(unix)]
    process_group_id: Option<i32>,
}

impl ProcessGroupGuard {
    pub(super) fn for_child(_child: &Child) -> Self {
        Self {
            #[cfg(unix)]
            process_group_id: _child.id().and_then(|id| i32::try_from(id).ok()),
        }
    }

    pub(super) fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.process_group_id = None;
        }
    }

    fn kill_group(&mut self) {
        #[cfg(unix)]
        if let Some(process_group_id) = self.process_group_id.take() {
            // The child was placed in a fresh group whose id equals its pid.
            // A negative pid asks POSIX kill(2) to signal the entire group.
            unsafe {
                kill(-process_group_id, SIGKILL);
            }
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill_group();
    }
}

/// Kill the full process group, terminate the immediate child as a portable
/// fallback, and reap it before returning.
pub(super) async fn kill_and_reap(
    child: &mut Child,
    process_group: &mut ProcessGroupGuard,
) -> io::Result<ExitStatus> {
    process_group.kill_group();

    match child.start_kill() {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => {}
        Err(error) => return Err(error),
    }

    let status = child.wait().await?;
    process_group.disarm();
    Ok(status)
}

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}
