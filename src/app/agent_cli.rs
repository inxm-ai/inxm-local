use std::path::PathBuf;
use std::time::Duration;

use crate::hostenv;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCli {
    Claude,
    Codex,
}

impl AgentCli {
    pub fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn package(self) -> &'static str {
        match self {
            Self::Claude => "@anthropic-ai/claude-code",
            Self::Codex => "@openai/codex",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }
}

fn npm_command(npm: &std::path::Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(npm);
    command.kill_on_drop(true);
    for key in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "INXM_AGENT_API_KEY",
    ] {
        command.env_remove(key);
    }
    if let Some(path) = hostenv::augmented_path() {
        command.env("PATH", path);
    }
    command
}

pub async fn install(cli: AgentCli) -> Result<PathBuf, String> {
    let npm = hostenv::find_on_path("npm").ok_or_else(|| {
        "Installing an agent CLI requires Node.js and npm. Install Node.js, then retry, or install the CLI manually and set its executable path."
            .to_owned()
    })?;
    install_with_npm(cli, &npm).await
}

async fn install_with_npm(cli: AgentCli, npm: &std::path::Path) -> Result<PathBuf, String> {
    let output = tokio::time::timeout(
        Duration::from_secs(300),
        npm_command(npm)
            .args(["install", "--global", cli.package()])
            .output(),
    )
    .await
    .map_err(|_| {
        "CLI installation timed out after 5 minutes; check npm before retrying.".to_owned()
    })?
    .map_err(|error| format!("Could not start npm: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{} installation failed ({}): {}",
            cli.label(),
            output.status,
            detail.chars().take(2000).collect::<String>()
        ));
    }
    let prefix = tokio::time::timeout(
        Duration::from_secs(15),
        npm_command(npm).args(["prefix", "--global"]).output(),
    )
    .await
    .map_err(|_| "npm prefix lookup timed out.".to_owned())?
    .map_err(|error| format!("Could not read npm's install location: {error}"))?;
    if !prefix.status.success() {
        return Err(format!(
            "CLI installation finished, but npm prefix failed: {}",
            String::from_utf8_lossy(&prefix.stderr)
        ));
    }
    let prefix = PathBuf::from(String::from_utf8_lossy(&prefix.stdout).trim());
    if !prefix.is_absolute() {
        return Err(
            "npm returned an invalid install location; set the CLI executable path manually."
                .to_owned(),
        );
    }
    let candidate = if cfg!(windows) {
        prefix.join(format!("{}.cmd", cli.program()))
    } else {
        prefix.join("bin").join(cli.program())
    };
    let executable =
        hostenv::find_on_path(candidate.to_string_lossy().as_ref()).ok_or_else(|| {
            format!(
                "Installation finished, but {} was not found. Set its executable path manually.",
                candidate.display()
            )
        })?;
    let mut probe = tokio::process::Command::new(&executable);
    probe.arg("--version").kill_on_drop(true);
    if let Some(path) = hostenv::augmented_path() {
        probe.env("PATH", path);
    }
    let output = tokio::time::timeout(Duration::from_secs(15), probe.output())
        .await
        .map_err(|_| {
            "The installed CLI did not respond to --version within 15 seconds.".to_owned()
        })?
        .map_err(|error| format!("Could not start the installed CLI: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "The installed CLI failed its version check: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(executable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installation_targets_only_the_selected_provider_package() {
        assert_eq!(AgentCli::Claude.program(), "claude");
        assert_eq!(AgentCli::Claude.package(), "@anthropic-ai/claude-code");
        assert_eq!(AgentCli::Codex.program(), "codex");
        assert_eq!(AgentCli::Codex.package(), "@openai/codex");
    }

    #[test]
    fn installer_does_not_forward_provider_credentials_to_package_scripts() {
        let command = npm_command(std::path::Path::new("npm"));
        let env = command
            .as_std()
            .get_envs()
            .collect::<std::collections::BTreeMap<_, _>>();
        for key in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "INXM_AGENT_API_KEY",
        ] {
            assert_eq!(env.get(std::ffi::OsStr::new(key)), Some(&None));
        }
    }

    #[cfg(unix)]
    fn script(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn installer_checks_selected_package_location_and_executable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("bin")).unwrap();
        let npm = dir.path().join("npm");
        script(
            &npm,
            "#!/bin/sh\ncase \"$1\" in\ninstall) printf '%s\\n' \"$@\" > \"$(dirname \"$0\")/installed-args\";;\nprefix) dirname \"$0\";;\n*) exit 2;;\nesac\n",
        );
        for cli in [AgentCli::Claude, AgentCli::Codex] {
            let executable = dir.path().join("bin").join(cli.program());
            script(
                &executable,
                "#!/bin/sh\n[ \"$1\" = --version ] || exit 3\nprintf '1.2.3\\n'\n",
            );
            assert_eq!(install_with_npm(cli, &npm).await.unwrap(), executable);
            let args = std::fs::read_to_string(dir.path().join("installed-args")).unwrap();
            assert_eq!(args, format!("install\n--global\n{}\n", cli.package()));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn installer_reports_package_manager_failure() {
        let dir = tempfile::tempdir().unwrap();
        let npm = dir.path().join("npm");
        script(
            &npm,
            "#!/bin/sh\nprintf 'permission denied\\n' >&2\nexit 1\n",
        );
        let error = install_with_npm(AgentCli::Codex, &npm).await.unwrap_err();
        assert!(error.contains("installation failed"));
        assert!(error.contains("permission denied"));
    }
}
