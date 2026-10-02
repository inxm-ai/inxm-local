//! Shared child-process lifecycle helpers for tool adapters.

use regex::Regex;
use std::io;
use std::process::ExitStatus;
use std::sync::LazyLock;
use tokio::process::{Child, Command};

#[cfg(unix)]
const SIGKILL: i32 = 9;

/// Error codes Node.js and Python print when a TLS certificate chain cannot
/// be verified, typically because a custom CA is not configured.
const CERTIFICATE_ERRORS: &[&str] = &[
    "UNABLE_TO_GET_ISSUER_CERT",
    "UNABLE_TO_VERIFY_LEAF_SIGNATURE",
    "SELF_SIGNED_CERT_IN_CHAIN",
    "DEPTH_ZERO_SELF_SIGNED_CERT",
    "CERTIFICATE_VERIFY_FAILED",
];

/// `env` failing to find a shebang interpreter, in BSD/macOS
/// (`env: node: No such…`) and GNU (`/usr/bin/env: ‘node’: No such…`) forms.
static MISSING_INTERPRETER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"env: ['‘"]?([^\s'’":]+)['’"]?: No such file or directory"#).expect("valid regex")
});

/// An actionable hint when a child's stderr shows it is missing part of the
/// user's environment.
pub(super) fn environment_hint(stderr: &str) -> Option<String> {
    if let Some(captures) = MISSING_INTERPRETER.captures(stderr) {
        return Some(format!(
            "hint: `{}` is not on the PATH this tool runs with. Add its directory to PATH in \
             your shell profile and restart INXM Local, or set PATH in this tool's environment \
             variables.",
            &captures[1]
        ));
    }
    CERTIFICATE_ERRORS
        .iter()
        .any(|code| stderr.contains(code))
        .then(|| {
            "hint: the TLS certificate could not be verified. If your network uses a custom \
             certificate authority, export NODE_EXTRA_CA_CERTS (Node.js) or SSL_CERT_FILE \
             (Python) in your shell profile and restart INXM Local, or set it in this tool's \
             environment variables."
                .to_owned()
        })
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_missing_shebang_interpreter_in_bsd_and_gnu_forms() {
        for stderr in [
            "env: node: No such file or directory\n",
            "/usr/bin/env: ‘node’: No such file or directory\n",
            "/usr/bin/env: 'node': No such file or directory\n",
        ] {
            let hint = environment_hint(stderr).expect(stderr);
            assert!(hint.contains("`node` is not on the PATH"), "{hint}");
        }
    }

    #[test]
    fn hints_certificate_errors() {
        let stderr = "Error: unable to get local issuer certificate\n  code: 'UNABLE_TO_GET_ISSUER_CERT_LOCALLY'";
        let hint = environment_hint(stderr).unwrap();
        assert!(hint.contains("NODE_EXTRA_CA_CERTS"), "{hint}");
    }

    #[test]
    fn unrelated_failures_get_no_hint() {
        assert_eq!(
            environment_hint("Error: ENOENT: no such file or directory, open 'x'"),
            None
        );
        assert_eq!(environment_hint(""), None);
    }
}
