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

/// Windows launcher extensions used when PATHEXT is not configured.
const DEFAULT_WINDOWS_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// Select a launcher using the same PATH/PATHEXT and working directory as its child.
pub(super) fn resolve_command(
    program: &str,
    env: &indexmap::IndexMap<String, String>,
    working_dir: Option<&std::path::Path>,
    tool_label: &str,
) -> Result<std::ffi::OsString, crate::error::ToolError> {
    let path = std::path::Path::new(program);
    if path.is_absolute() {
        return Ok(path.as_os_str().to_owned());
    }
    let override_value = |name: &str| {
        env.iter()
            .rev()
            .find(|(key, _)| {
                if cfg!(windows) {
                    key.eq_ignore_ascii_case(name)
                } else {
                    key.as_str() == name
                }
            })
            .map(|(_, value)| value)
    };
    let path_override = override_value("PATH");
    let pathext_override = if cfg!(windows) {
        override_value("PATHEXT")
    } else {
        None
    };
    let is_relative_path = program.contains('/') || (cfg!(windows) && program.contains('\\'));
    if path_override.is_none() && pathext_override.is_none() && !is_relative_path {
        return Ok(crate::hostenv::resolve_program(program).into_os_string());
    }
    let current_dir = std::env::current_dir().map_err(crate::error::ToolError::Io)?;
    let child_dir = working_dir.map_or(current_dir.clone(), |dir| current_dir.join(dir));
    if is_relative_path {
        return Ok(child_dir.join(path).into_os_string());
    }
    let effective_path = path_override
        .map(std::ffi::OsString::from)
        .or_else(crate::hostenv::augmented_path)
        .unwrap_or_default();
    // Relative PATH entries (including empty entries) are relative to the child's directory.
    let absolute_dirs = std::env::split_paths(&effective_path).map(|dir| child_dir.join(dir));
    let effective_path = std::env::join_paths(absolute_dirs).map_err(|error| {
        crate::error::ToolError::Execution {
            tool: tool_label.to_owned(),
            message: format!("cannot encode the tool's PATH: {error}"),
        }
    })?;
    let pathext = if cfg!(windows) {
        Some(
            pathext_override
                .map(std::ffi::OsString::from)
                .or_else(|| std::env::var_os("PATHEXT"))
                .unwrap_or_else(|| DEFAULT_WINDOWS_PATHEXT.into()),
        )
    } else {
        None
    };
    crate::hostenv::find_program_in_path(program, &effective_path, pathext.as_deref())
        .map(std::path::PathBuf::into_os_string)
        .ok_or_else(|| crate::error::ToolError::Execution {
            tool: tool_label.to_owned(),
            message: format!("executable '{program}' was not found in the tool's explicit PATH/PATHEXT; check its environment overrides"),
        })
}

/// An actionable hint when a child's stderr shows it is missing part of the
/// user's environment.
pub(super) fn environment_hint(stderr: &str) -> Option<String> {
    if let Some(captures) = MISSING_INTERPRETER.captures(stderr) {
        return Some(format!(
            "hint: `{}` is not on the PATH this tool runs with. Add its directory in Settings → Extra executable folders, \
             fully quit and reopen INXM Local, or set PATH in this tool's environment variables.",
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
    fn explicit_path_selects_only_configured_directory() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin with spaces");
        std::fs::create_dir(&bin).unwrap();
        let program = if cfg!(windows) {
            "override-tool.exe"
        } else {
            "override-tool"
        };
        let executable = bin.join(program);
        std::fs::write(&executable, "").unwrap();
        let env = [("PATH".to_owned(), bin.to_string_lossy().into_owned())]
            .into_iter()
            .collect();
        assert_eq!(
            resolve_command(program, &env, None, "test").unwrap(),
            executable.into_os_string()
        );
        let global_program = if cfg!(windows) { "cmd.exe" } else { "sh" };
        assert!(
            resolve_command(global_program, &env, None, "test")
                .unwrap_err()
                .to_string()
                .contains("explicit PATH")
        );
    }

    #[test]
    fn relative_path_entries_use_child_working_directory() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let executable = bin.join("relative-tool");
        std::fs::write(&executable, "").unwrap();
        let env = [("PATH".to_owned(), "bin".to_owned())]
            .into_iter()
            .collect();
        assert_eq!(
            resolve_command("relative-tool", &env, Some(temp.path()), "test").unwrap(),
            executable.into_os_string()
        );
        assert_eq!(
            resolve_command("./local-tool", &env, Some(temp.path()), "test").unwrap(),
            temp.path().join("./local-tool").into_os_string()
        );
    }

    #[test]
    fn empty_override_keeps_absolute_launcher_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("absolute-tool");
        let env = [("PATH".to_owned(), String::new())].into_iter().collect();
        assert_eq!(
            resolve_command(executable.to_str().unwrap(), &env, None, "test").unwrap(),
            executable.into_os_string()
        );
        let global_program = if cfg!(windows) { "cmd.exe" } else { "sh" };
        assert!(resolve_command(global_program, &env, None, "test").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_and_pathext_overrides_are_case_insensitive() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("override-tool.cmd");
        std::fs::write(&executable, "").unwrap();
        let env = [
            (
                "Path".to_owned(),
                temp.path().to_string_lossy().into_owned(),
            ),
            ("PathExt".to_owned(), ".CMD".to_owned()),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            resolve_command("override-tool", &env, None, "test").unwrap(),
            executable.into_os_string()
        );
    }

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
