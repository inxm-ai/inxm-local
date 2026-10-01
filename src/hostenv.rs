//! Host environment detection and cross-platform program resolution.
//!
//! Three jobs:
//!
//! 1. **Resolve programs portably.** On Windows, `Command::new("npx")` fails
//!    because `npx` is `npx.cmd` and `CreateProcess` does not apply
//!    `PATHEXT`. [`resolve_program`] searches `PATH` (honouring `PATHEXT` on
//!    Windows) and returns a spawnable path.
//! 2. **Describe the environment to the compiler.** [`EnvProbe`] detects the
//!    OS and which interpreters/runners exist, so compiled plans only use
//!    what is actually available (no `bash` steps on a bash-less Windows).
//! 3. **Adopt the user's shell environment.** [`import_login_shell_env`]
//!    gives a desktop-launched app the `PATH`, certificate, proxy, and
//!    runtime variables the user's shell profile exports, but no credentials.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Interpreters probed for CODE_CALL support, in the order they are
/// preferred as aliases of each other (e.g. `python3` before `python`).
const PROBED_INTERPRETERS: &[&str] = &[
    "bash",
    "sh",
    "python3",
    "python",
    "node",
    "pwsh",
    "powershell",
    "cmd",
];

/// External commands that plans commonly reach for from shell scripts or
/// seeded tools. The compiler sees both the available and missing lists so it
/// does not assume Unix-y helpers (notably `curl`) exist inside `cmd`.
const PROBED_RUNNERS: &[&str] = &[
    "npx", "uvx", "curl", "wget", "git", "cargo", "gh", "codex", "claude",
];

const DEFAULT_WINDOWS_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

// ─── Program resolution ───────────────────────────────────────────────────────

/// The candidate file names for `program` in one directory: on Windows,
/// each `PATHEXT` extension (in order) followed by the bare name as a last
/// resort; elsewhere, just the bare name.
///
/// `PATHEXT` candidates must come before the bare name: npm installs ship
/// both `npx.cmd` (a real Windows launcher) and an extensionless `npx`
/// POSIX shim in the same directory, and the latter is not a valid Win32
/// executable. Checking it first causes `CreateProcess` to fail with
/// "%1 is not a valid Win32 application" instead of finding `npx.cmd`.
fn candidate_names(program: &str, pathext: Option<&str>) -> Vec<String> {
    match pathext {
        None => vec![program.to_owned()],
        Some(exts) => exts
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|ext| format!("{program}{}", ext.to_lowercase()))
            .chain(std::iter::once(program.to_owned()))
            .collect(),
    }
}

/// Pure search across explicit directories — testable without touching the
/// process environment.
fn find_in_dirs(program: &str, dirs: &[PathBuf], pathext: Option<&str>) -> Option<PathBuf> {
    let names = candidate_names(program, pathext);
    dirs.iter()
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

fn platform_pathext() -> Option<String> {
    cfg!(windows)
        .then(|| std::env::var("PATHEXT").unwrap_or_else(|_| DEFAULT_WINDOWS_PATHEXT.to_owned()))
}

/// Well-known per-user and system install directories that hold CLI tools
/// (`claude`, `codex`, `gh`, `uvx`, …) but are frequently *absent from a
/// GUI/tray-launched app's `PATH`*.
///
/// Desktop environments start apps with a minimal login `PATH` that omits the
/// shell-rc additions (`~/.local/bin`, nvm/volta node bins, Homebrew, cargo,
/// deno). So `which claude` succeeds in the user's terminal while
/// `Command::new("claude")` inside the app fails with `ENOENT`. Searching
/// these locations as a fallback closes that gap without requiring the user to
/// hand-configure an absolute path.
///
/// Directories that do not exist are skipped by the caller. Honours the env
/// vars tool managers export (`NVM_BIN`, `VOLTA_HOME`, `npm_config_prefix`,
/// `PNPM_HOME`, `BUN_INSTALL`, `CARGO_HOME`) before falling back to their
/// conventional locations.
fn well_known_bin_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push_env = |var: &str, suffix: Option<&str>| {
        if let Some(base) = std::env::var_os(var) {
            let path = PathBuf::from(base);
            dirs.push(suffix.map_or(path.clone(), |s| path.join(s)));
        }
    };
    // Tool-manager-exported locations (most reliable when present).
    push_env("NVM_BIN", None);
    push_env("VOLTA_HOME", Some("bin"));
    push_env("PNPM_HOME", None);
    push_env("BUN_INSTALL", Some("bin"));
    push_env("npm_config_prefix", Some("bin"));
    push_env("CARGO_HOME", Some("bin"));

    if let Some(home) = home_dir() {
        for rel in [
            ".local/bin",
            "bin",
            ".cargo/bin",
            ".deno/bin",
            ".bun/bin",
            ".npm-global/bin",
            ".npm-packages/bin",
            ".yarn/bin",
            ".local/state/fnm_multishells", // fnm current shell (best-effort)
            ".claude/local",                // legacy claude local install
        ] {
            dirs.push(home.join(rel));
        }
        // Enumerate nvm-managed node versions: their global npm bins live at
        // ~/.nvm/versions/node/<version>/bin and none of them are on a GUI PATH.
        let nvm_versions = home.join(".nvm/versions/node");
        if let Ok(entries) = std::fs::read_dir(&nvm_versions) {
            for entry in entries.flatten() {
                dirs.push(entry.path().join("bin"));
            }
        }
    }

    // System locations. Homebrew on Apple Silicon (`/opt/homebrew`) is not on
    // the default `/usr/bin:/bin` PATH a GUI app inherits.
    for sys in [
        "/usr/local/bin",
        "/opt/homebrew/bin",
        "/opt/local/bin",
        "/snap/bin",
    ] {
        dirs.push(PathBuf::from(sys));
    }
    dirs
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Locate `program` on `PATH`, falling back to [`well_known_bin_dirs`] so a
/// tool installed in a shell-rc location is still found when the app inherits
/// a minimal GUI/tray `PATH`. Names containing a path separator are checked
/// directly (still applying `PATHEXT` on Windows).
pub fn find_on_path(program: &str) -> Option<PathBuf> {
    let pathext = platform_pathext();
    if program.contains(['/', '\\']) {
        let base = Path::new(program);
        let dir = base.parent().unwrap_or(Path::new(".")).to_path_buf();
        let name = base.file_name()?.to_string_lossy().into_owned();
        return find_in_dirs(&name, std::slice::from_ref(&dir), pathext.as_deref());
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path_var| std::env::split_paths(&path_var).collect())
        .unwrap_or_default();
    if let Some(found) = find_in_dirs(program, &dirs, pathext.as_deref()) {
        return Some(found);
    }
    // Fallback: shell-rc / tool-manager locations absent from a GUI PATH.
    dirs = well_known_bin_dirs();
    find_in_dirs(program, &dirs, pathext.as_deref())
}

/// A spawnable form of `program`: the resolved absolute path when found on
/// `PATH`, otherwise the input unchanged (so the OS error stays meaningful).
pub fn resolve_program(program: &str) -> PathBuf {
    find_on_path(program).unwrap_or_else(|| PathBuf::from(program))
}

/// A `PATH` value for spawned children: the current `PATH` entries as-is,
/// plus any [`well_known_bin_dirs`] that both exist on disk and are not
/// already present.
///
/// Resolving a CLI's own binary (via [`resolve_program`]) is not enough when
/// that binary is itself a script with an `env`-based shebang (e.g. the npm
/// `codex`/`claude` CLIs use `#!/usr/bin/env node`): the *child's* `PATH`
/// also needs the node/nvm/volta bin directory, or `env` fails with `node:
/// No such file or directory` even though the app found `codex` fine. Pass
/// this to `Command::env("PATH", ..)` on every spawn that resolves through
/// `hostenv`.
///
/// Returns `None` only if the resulting `PATH` cannot be encoded — e.g. a
/// directory contains a NUL byte or the platform path-list separator (`:`
/// on Unix, `;` on Windows), which `std::env::join_paths` rejects — callers
/// should leave the child's `PATH` untouched in that case.
pub fn augmented_path() -> Option<std::ffi::OsString> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path_var| std::env::split_paths(&path_var).collect())
        .unwrap_or_default();
    for dir in well_known_bin_dirs() {
        if dir.is_dir() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    std::env::join_paths(dirs).ok()
}

// ─── Login-shell environment ──────────────────────────────────────────────────

/// Setting this variable (to any value) disables [`import_login_shell_env`].
pub const SKIP_SHELL_ENV_VAR: &str = "INXM_SKIP_SHELL_ENV";

/// Longest startup wait for the user's shell profile (VS Code's default).
const SHELL_ENV_TIMEOUT: Duration = Duration::from_secs(10);

/// Variables copied from the login shell besides `PATH`: certificate, proxy,
/// locale, and runtime locations that decide whether tools can run at all.
/// Credentials are deliberately absent, so profile secrets never reach
/// AI-generated steps; tools that need one get it from their own env.
const IMPORTED_SHELL_VARS: &[&str] = &[
    // TLS trust
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    // Proxies
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "all_proxy",
    // Locale
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    // Runtime and version-manager locations
    "JAVA_HOME",
    "GOPATH",
    "GOROOT",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "DOTNET_ROOT",
    "NVM_DIR",
    "NVM_BIN",
    "VOLTA_HOME",
    "PNPM_HOME",
    "BUN_INSTALL",
    "PYENV_ROOT",
    "ASDF_DIR",
    "ASDF_DATA_DIR",
    "MISE_DATA_DIR",
];

/// Adopt the parts of the user's shell profile environment that tools need
/// to run: `PATH` plus [`IMPORTED_SHELL_VARS`].
///
/// Desktop launches (Finder, Dock, Start menu, login items, `.desktop` files,
/// GUI MCP clients) do not run shell startup files, so values such as
/// Homebrew or nvm `PATH` entries and `NODE_EXTRA_CA_CERTS` are otherwise
/// missing. Variables the process already has always win, and only `PATH`
/// is merged. On Unix the shell's `PATH` order comes first because the
/// inherited desktop `PATH` is a bare system default. On Windows the
/// inherited `PATH` comes first because it is the user's registry
/// configuration.
///
/// Must run before any other thread starts, because it writes the process
/// environment.
pub fn import_login_shell_env() {
    if std::env::var_os(SKIP_SHELL_ENV_VAR).is_some() {
        tracing::info!(
            operation = "hostenv.shell_env",
            outcome = "skipped",
            reason = SKIP_SHELL_ENV_VAR,
            "login shell environment not imported"
        );
        return;
    }
    // A terminal launch already carries the shell's environment.
    if cfg!(unix) && std::env::var_os("TERM").is_some() {
        tracing::debug!(
            operation = "hostenv.shell_env",
            outcome = "skipped",
            reason = "launched from a terminal",
            "login shell environment not imported"
        );
        return;
    }

    let marker = format!("INXM_ENV_{}", uuid::Uuid::new_v4().simple());
    let shell_vars = match capture_shell_env(
        login_shell_command(&marker),
        &marker,
        SHELL_ENV_TIMEOUT,
    ) {
        Ok(vars) => vars,
        Err(error) => {
            tracing::warn!(
                operation = "hostenv.shell_env",
                outcome = "failure",
                error = %error,
                "could not import the login shell environment; launched tools keep the app's environment"
            );
            return;
        }
    };
    let inherited: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    let updates = merge_shell_env(&inherited, &shell_vars, cfg!(unix));
    let variables = updates
        .iter()
        .map(|(key, _)| key.as_str())
        .collect::<Vec<_>>()
        .join(",");
    for (key, value) in &updates {
        // SAFETY: `main` calls this before starting any thread that touches
        // the environment; the capture threads only read a pipe or reap.
        unsafe { std::env::set_var(key, value) };
    }
    tracing::info!(
        operation = "hostenv.shell_env",
        outcome = "success",
        variables = %variables,
        "imported login shell environment"
    );
}

/// `$SHELL` as an interactive login shell, printing its environment
/// NUL-delimited between two `marker` lines.
#[cfg(unix)]
fn login_shell_command(marker: &str) -> Command {
    let shell = std::env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".into());
    let name = Path::new(&shell)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut command = Command::new(&shell);
    command.args(login_shell_flags(&name));
    command.arg(format!("echo {marker}; /usr/bin/env -0; echo {marker}"));
    command
}

/// csh and tcsh accept `-l` only as their sole flag.
#[cfg(unix)]
fn login_shell_flags(shell_name: &str) -> &'static [&'static str] {
    match shell_name {
        "csh" | "tcsh" => &["-i", "-c"],
        _ => &["-i", "-l", "-c"],
    }
}

/// PowerShell with the user's profile, printing its environment
/// NUL-delimited between two `marker` strings.
#[cfg(windows)]
fn login_shell_command(marker: &str) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let shell = find_on_path("pwsh").unwrap_or_else(|| PathBuf::from("powershell.exe"));
    let mut command = Command::new(shell);
    command.args(["-NoLogo", "-NonInteractive", "-Command"]);
    command.arg(format!(
        "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false; \
         [Console]::Out.Write('{marker}'); \
         Get-ChildItem env: | ForEach-Object {{ [Console]::Out.Write($_.Name + '=' + $_.Value + [char]0) }}; \
         [Console]::Out.Write('{marker}')"
    ));
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// Run `command` and parse the environment it prints between two `marker`
/// strings, killing it if that takes longer than `timeout`.
fn capture_shell_env(
    mut command: Command,
    marker: &str,
    timeout: Duration,
) -> Result<Vec<(String, String)>, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid(2) is async-signal-safe. A new session has no
        // controlling terminal, so an interactive shell cannot stop on terminal
        // I/O, and it leads its own process group for `kill_shell`.
        unsafe {
            command.pre_exec(|| {
                if setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start the login shell: {error}"))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let end_marker = marker.as_bytes().to_vec();
    let (sender, receiver) = std::sync::mpsc::channel();
    // Stop at the closing marker rather than EOF: a background job started by
    // the profile can hold stdout open indefinitely.
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut buffer = [0_u8; 8192];
        let result = loop {
            match stdout.read(&mut buffer) {
                Ok(0) => break Ok(output),
                Ok(read) => {
                    output.extend_from_slice(&buffer[..read]);
                    if occurrences(&output, &end_marker) >= 2 {
                        break Ok(output);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => break Err(error),
            }
        };
        let _ = sender.send(result);
    });

    match receiver.recv_timeout(timeout) {
        Ok(Ok(output)) => {
            // Reap without killing, so jobs the profile backgrounds keep running.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            parse_shell_env_output(&output, marker)
        }
        Ok(Err(error)) => {
            kill_shell(&mut child);
            Err(format!("could not read the login shell output: {error}"))
        }
        Err(_) => {
            kill_shell(&mut child);
            Err(format!("the login shell did not finish within {timeout:?}"))
        }
    }
}

fn kill_shell(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(group) = i32::try_from(child.id()) {
        // SAFETY: kill(2) has no memory-safety preconditions. The shell leads
        // its own process group, so a negative pid also stops its children.
        unsafe {
            kill(-group, SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
const SIGKILL: i32 = 9;

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
    fn setsid() -> i32;
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

/// Extract `KEY=VALUE` pairs from the NUL-delimited block between the first
/// two `marker`s, ignoring anything the profile printed around it.
fn parse_shell_env_output(output: &[u8], marker: &str) -> Result<Vec<(String, String)>, String> {
    let marker = marker.as_bytes();
    let start =
        find_bytes(output, marker).ok_or("the login shell printed no environment")? + marker.len();
    let rest = &output[start..];
    let end =
        find_bytes(rest, marker).ok_or("the login shell environment output was incomplete")?;
    let block = &rest[..end];
    if !block.contains(&0) {
        return Err("the login shell environment output was not NUL-delimited".to_owned());
    }
    let leading_newlines = block
        .iter()
        .take_while(|byte| matches!(byte, b'\r' | b'\n'))
        .count();
    Ok(block[leading_newlines..]
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            let (key, value) = std::str::from_utf8(entry).ok()?.split_once('=')?;
            (!key.is_empty()).then(|| (key.to_owned(), value.to_owned()))
        })
        .collect())
}

fn env_key_eq(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// The variables to set so the process gains what `shell` adds to
/// `inherited`: missing [`IMPORTED_SHELL_VARS`] are added, existing ones are
/// kept, and `PATH` becomes the de-duplicated union with `shell` entries
/// first when `shell_path_first` is true. Everything else is ignored.
fn merge_shell_env(
    inherited: &[(String, String)],
    shell: &[(String, String)],
    shell_path_first: bool,
) -> Vec<(String, String)> {
    let lookup = |vars: &[(String, String)], wanted: &str| {
        vars.iter()
            .find(|(key, _)| env_key_eq(key, wanted))
            .map(|(_, value)| value.clone())
    };
    let mut updates: Vec<(String, String)> = shell
        .iter()
        .filter(|(key, _)| {
            IMPORTED_SHELL_VARS
                .iter()
                .any(|allowed| env_key_eq(key, allowed))
        })
        .filter(|(key, _)| lookup(inherited, key).is_none())
        .cloned()
        .collect();

    if let Some(shell_path) = lookup(shell, "PATH") {
        let inherited_path = lookup(inherited, "PATH");
        let inherited_value = inherited_path.clone().unwrap_or_default();
        let (first, second) = if shell_path_first {
            (shell_path, inherited_value)
        } else {
            (inherited_value, shell_path)
        };
        let mut dirs: Vec<PathBuf> = Vec::new();
        for dir in std::env::split_paths(&first).chain(std::env::split_paths(&second)) {
            if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        if let Some(merged) = std::env::join_paths(dirs)
            .ok()
            .and_then(|merged| merged.into_string().ok())
            && inherited_path.as_ref() != Some(&merged)
        {
            updates.push(("PATH".to_owned(), merged));
        }
    }
    updates
}

/// Verify that an interpreter can actually be spawned by attempting to start
/// and immediately kill a process.
///
/// Returns `false` when the spawn itself fails (e.g. `ENOENT` when the file
/// exists on disk but has a broken shebang, is a macOS CLT stub pointing to
/// a missing runtime, or is a wrapper script whose shebang interpreter is
/// absent). Finding a file via `find_on_path` is not enough — the kernel
/// must be able to `exec` it too.
///
/// The kill is best-effort; if the child exits on its own that is fine.
fn interpreter_actually_spawns(path: &Path) -> bool {
    match std::process::Command::new(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            let _ = child.kill();
            let _ = child.wait();
            true
        }
        Err(_) => false,
    }
}

// ─── Environment probe ────────────────────────────────────────────────────────

/// What this machine offers: OS, architecture, and which interpreters and
/// tool runners are on `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvProbe {
    pub os: &'static str,
    pub arch: &'static str,
    pub interpreters: Vec<&'static str>,
    pub runners: Vec<&'static str>,
}

impl EnvProbe {
    /// Probe once per process and cache: `PATH` scans can be slow on some
    /// setups (e.g. WSL with Windows mounts on `PATH`), and the environment
    /// does not change while the app runs.
    pub fn detect() -> &'static Self {
        static PROBE: std::sync::OnceLock<EnvProbe> = std::sync::OnceLock::new();
        PROBE.get_or_init(Self::detect_uncached)
    }

    fn detect_uncached() -> Self {
        // Runners are checked by presence only — they are invoked from shell
        // scripts written by the compiler, not spawned directly by the executor.
        let on_path = |names: &[&'static str]| -> Vec<&'static str> {
            names
                .iter()
                .copied()
                .filter(|name| find_on_path(name).is_some())
                .collect()
        };
        // Interpreters must be both found on PATH *and* actually spawnable.
        // A file can exist (e.g. a wrapper script, a macOS CLT stub) yet fail
        // to exec at runtime. Filtering here ensures the compiler never emits
        // a CODE_CALL step for an interpreter that cannot run on this machine.
        let spawnable = |names: &[&'static str]| -> Vec<&'static str> {
            names
                .iter()
                .copied()
                .filter(|name| {
                    find_on_path(name)
                        .map(|path| interpreter_actually_spawns(&path))
                        .unwrap_or(false)
                })
                .collect()
        };
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            interpreters: spawnable(PROBED_INTERPRETERS),
            runners: on_path(PROBED_RUNNERS),
        }
    }

    /// Text block appended to compiler prompts so generated plans only rely
    /// on what exists here.
    pub fn compiler_context(&self) -> String {
        let missing_interpreters: Vec<&str> = PROBED_INTERPRETERS
            .iter()
            .copied()
            .filter(|name| !self.interpreters.contains(name))
            .collect();
        let missing_runners: Vec<&str> = PROBED_RUNNERS
            .iter()
            .copied()
            .filter(|name| !self.runners.contains(name))
            .collect();
        format!(
            "## Execution environment\n\
             - Operating system: {} ({})\n\
             - Script interpreters available for CODE_CALL steps: {}\n\
             - Interpreters NOT available (never generate steps that need these): {}\n\
             - External commands available to shell scripts/tools: {}\n\
             - External commands NOT available (never call these from CODE_CALL): {}\n\
             Generate CODE_CALL steps only for the available interpreters, and use \
             shell syntax native to this operating system. A shell language such \
             as `cmd` only means that interpreter exists; it does not imply CLI \
             helpers like `curl`, `wget`, or `git` exist. Prefer TOOL_CALL steps \
             over CODE_CALL when a catalog tool covers the need.",
            self.os,
            self.arch,
            join_or_none(&self.interpreters),
            join_or_none(&missing_interpreters),
            join_or_none(&self.runners),
            join_or_none(&missing_runners),
        )
    }

    /// One-line summary for the UI.
    pub fn summary(&self) -> String {
        format!(
            "{} ({}) · interpreters: {} · runners: {}",
            self.os,
            self.arch,
            join_or_none(&self.interpreters),
            join_or_none(&self.runners),
        )
    }
}

fn join_or_none(items: &[&str]) -> String {
    match items.is_empty() {
        true => "none".to_owned(),
        false => items.join(", "),
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // `HOME`/`PATH` are process-global; tests run in parallel by default, so
    // any test that mutates them must hold this lock for its whole
    // read-mutate-restore span to avoid racing another such test.
    static ENV_MUTATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn fake_bin(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "").unwrap();
        path
    }

    #[test]
    fn finds_bare_name_in_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let expected = fake_bin(tmp.path(), "mytool");
        let found = find_in_dirs("mytool", &[tmp.path().to_path_buf()], None);
        assert_eq!(found, Some(expected));
    }

    #[test]
    fn windows_pathext_finds_cmd_shims() {
        let tmp = tempfile::tempdir().unwrap();
        let expected = fake_bin(tmp.path(), "npx.cmd");
        let found = find_in_dirs(
            "npx",
            &[tmp.path().to_path_buf()],
            Some(".COM;.EXE;.BAT;.CMD"),
        );
        assert_eq!(found, Some(expected));
    }

    #[test]
    fn windows_pathext_prefers_cmd_over_bare_posix_shim() {
        // Regression test: npm installs ship both `npx` (an extensionless
        // POSIX shim) and `npx.cmd` (the real Windows launcher) in the same
        // directory. The `.cmd` file must win.
        let tmp = tempfile::tempdir().unwrap();
        fake_bin(tmp.path(), "npx");
        let expected = fake_bin(tmp.path(), "npx.cmd");
        let found = find_in_dirs(
            "npx",
            &[tmp.path().to_path_buf()],
            Some(".COM;.EXE;.BAT;.CMD"),
        );
        assert_eq!(found, Some(expected));
    }

    #[test]
    fn missing_program_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            find_in_dirs("nope", &[tmp.path().to_path_buf()], Some(".EXE")),
            None
        );
    }

    #[test]
    fn well_known_dirs_include_user_local_bin() {
        // GUI/tray-launched apps inherit a minimal PATH; ~/.local/bin (the
        // native `claude` installer's target) must be in the fallback set.
        let dirs = well_known_bin_dirs();
        assert!(
            dirs.iter().any(|d| d.ends_with(".local/bin")),
            "expected ~/.local/bin among fallback dirs, got {dirs:?}"
        );
    }

    #[test]
    fn find_on_path_falls_back_to_well_known_dir() {
        // A tool present in a well-known dir but absent from PATH is still
        // found — the core GUI-PATH robustness fix.
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let program = "inxm-fake-cli-xyz";
        fake_bin(&bin, program);

        // Held for the whole read-mutate-restore span so this can't race
        // `augmented_path_includes_well_known_dir_missing_from_path`, the
        // other test that mutates HOME/PATH.
        let _guard = ENV_MUTATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var_os("HOME");
        let prev_path = std::env::var_os("PATH");
        // SAFETY: single-threaded test body; restored before returning.
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::set_var("PATH", "");
        }
        let found = find_on_path(program);
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }
        assert_eq!(found.as_deref(), Some(bin.join(program).as_path()));
    }

    #[test]
    fn augmented_path_includes_well_known_dir_missing_from_path() {
        // A well-known dir (e.g. a node-manager bin dir) absent from a
        // minimal PATH must still show up in the child-process PATH, so a
        // resolved CLI's own `env`-based shebang can find its interpreter.
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();

        // Held for the whole read-mutate-restore span so this can't race
        // `find_on_path_falls_back_to_well_known_dir`, the other test that
        // mutates HOME/PATH.
        let _guard = ENV_MUTATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var_os("HOME");
        let prev_path = std::env::var_os("PATH");
        // SAFETY: single-threaded test body; restored before returning.
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::set_var("PATH", "/usr/bin");
        }
        let augmented = augmented_path();
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }
        let augmented = augmented.expect("PATH should encode cleanly");
        let dirs: Vec<PathBuf> = std::env::split_paths(&augmented).collect();
        assert!(
            dirs.contains(&PathBuf::from("/usr/bin")),
            "expected original PATH entry preserved, got {dirs:?}"
        );
        assert!(
            dirs.contains(&bin),
            "expected well-known dir {bin:?} appended, got {dirs:?}"
        );
    }

    #[test]
    fn resolve_program_falls_back_to_input() {
        assert_eq!(
            resolve_program("definitely-not-a-real-program-xyz"),
            PathBuf::from("definitely-not-a-real-program-xyz")
        );
    }

    #[test]
    fn probe_detects_something_sane() {
        let probe = EnvProbe::detect();
        assert!(!probe.os.is_empty());
        // Every CI/dev box has at least one of these.
        assert!(
            !probe.interpreters.is_empty(),
            "no interpreters found at all — probe is broken"
        );
        let context = probe.compiler_context();
        assert!(context.contains("Execution environment"));
        assert!(context.contains(probe.os));
    }

    #[test]
    fn interpreter_actually_spawns_returns_false_for_nonexistent_path() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("no_such_program");
        assert!(
            !interpreter_actually_spawns(&nonexistent),
            "a non-existent path must not be reported as spawnable"
        );
    }

    /// A plain empty file (no shebang, not executable on macOS/Linux) should
    /// fail to spawn — it is on disk but cannot be exec'd by the kernel.
    #[cfg(unix)]
    #[test]
    fn interpreter_actually_spawns_returns_false_for_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("empty_interpreter");
        std::fs::write(&path, "").unwrap();
        // Set execute bit so the kernel tries to exec it.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The kernel rejects an empty executable with ENOEXEC.
        assert!(
            !interpreter_actually_spawns(&path),
            "an empty file must not be reported as spawnable"
        );
    }

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn parse_shell_env_output_ignores_profile_noise() {
        let output = b"Welcome!\nMARK\nA=1\0NODE_EXTRA_CA_CERTS=/etc/ssl/cert.pem\0MULTI=x\ny\0=C:=C:\\\0MARK\nbye\n";
        let parsed = parse_shell_env_output(output, "MARK").unwrap();
        assert_eq!(
            parsed,
            vars(&[
                ("A", "1"),
                ("NODE_EXTRA_CA_CERTS", "/etc/ssl/cert.pem"),
                ("MULTI", "x\ny"),
            ])
        );
    }

    #[test]
    fn parse_shell_env_output_rejects_missing_or_malformed_blocks() {
        assert!(parse_shell_env_output(b"no markers", "MARK").is_err());
        assert!(parse_shell_env_output(b"MARK\nA=1\0", "MARK").is_err());
        assert!(parse_shell_env_output(b"MARK\nA=1\nB=2\nMARK\n", "MARK").is_err());
    }

    #[test]
    fn merge_imports_only_allowlisted_variables_and_keeps_inherited_ones() {
        let inherited = vars(&[("HOME", "/Users/me"), ("LANG", "de_DE.UTF-8")]);
        let shell = vars(&[
            ("HOME", "/Users/me"),
            ("LANG", "en_US.UTF-8"),
            ("NODE_EXTRA_CA_CERTS", "/etc/ssl/cert.pem"),
            ("HTTPS_PROXY", "http://proxy:8080"),
            ("GITHUB_TOKEN", "ghp_secret"),
            ("OPENAI_API_KEY", "sk-secret"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("PWD", "/Users/me"),
        ]);
        let updates = merge_shell_env(&inherited, &shell, true);
        assert_eq!(
            updates,
            vars(&[
                ("NODE_EXTRA_CA_CERTS", "/etc/ssl/cert.pem"),
                ("HTTPS_PROXY", "http://proxy:8080"),
            ])
        );
    }

    #[test]
    fn imported_variables_exclude_credentials() {
        for var in IMPORTED_SHELL_VARS {
            let upper = var.to_ascii_uppercase();
            assert!(
                ["TOKEN", "KEY", "SECRET", "PASSWORD", "AUTH"]
                    .iter()
                    .all(|word| !upper.contains(word)),
                "{var} looks like a credential"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn merge_puts_shell_path_first_without_duplicates() {
        let inherited = vars(&[("PATH", "/usr/bin:/bin:/custom")]);
        let shell = vars(&[("PATH", "/opt/homebrew/bin:/usr/bin:/bin")]);
        let updates = merge_shell_env(&inherited, &shell, true);
        assert_eq!(
            updates,
            vars(&[("PATH", "/opt/homebrew/bin:/usr/bin:/bin:/custom")])
        );
    }

    #[cfg(unix)]
    #[test]
    fn merge_can_keep_inherited_path_first() {
        let inherited = vars(&[("PATH", "/registry/bin:/usr/bin")]);
        let shell = vars(&[("PATH", "/profile/bin:/usr/bin")]);
        let updates = merge_shell_env(&inherited, &shell, false);
        assert_eq!(
            updates,
            vars(&[("PATH", "/registry/bin:/usr/bin:/profile/bin")])
        );
    }

    #[cfg(unix)]
    #[test]
    fn merge_leaves_identical_path_alone() {
        let path = vars(&[("PATH", "/usr/bin:/bin")]);
        assert!(merge_shell_env(&path, &path, true).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_flags_avoid_combined_login_flag_for_csh() {
        assert_eq!(login_shell_flags("zsh"), &["-i", "-l", "-c"]);
        assert_eq!(login_shell_flags("fish"), &["-i", "-l", "-c"]);
        assert_eq!(login_shell_flags("tcsh"), &["-i", "-c"]);
    }

    #[cfg(unix)]
    fn fake_shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(script);
        command
    }

    #[cfg(unix)]
    #[test]
    fn capture_reads_environment_printed_between_markers() {
        let script = "echo 'profile banner'; echo MARK; \
                      printf 'A=1\\000NODE_EXTRA_CA_CERTS=/etc/ssl/cert.pem\\000'; echo MARK";
        let parsed = capture_shell_env(fake_shell(script), "MARK", Duration::from_secs(5)).unwrap();
        assert_eq!(
            parsed,
            vars(&[("A", "1"), ("NODE_EXTRA_CA_CERTS", "/etc/ssl/cert.pem")])
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_does_not_wait_for_background_jobs_holding_stdout() {
        let script = "sleep 5 & echo MARK; printf 'A=1\\000'; echo MARK; wait";
        let started = std::time::Instant::now();
        let parsed = capture_shell_env(fake_shell(script), "MARK", Duration::from_secs(4)).unwrap();
        assert_eq!(parsed, vars(&[("A", "1")]));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn capture_detaches_the_shell_from_the_terminal() {
        // Under a terminal, reading /dev/tty from a background group would stop the shell.
        let script = "cat /dev/tty >/dev/null 2>&1; echo MARK; printf 'A=1\\000'; echo MARK";
        let parsed = capture_shell_env(fake_shell(script), "MARK", Duration::from_secs(3)).unwrap();
        assert_eq!(parsed, vars(&[("A", "1")]));
    }

    #[cfg(unix)]
    #[test]
    fn capture_times_out_and_kills_a_hanging_shell() {
        let started = std::time::Instant::now();
        let error = capture_shell_env(fake_shell("sleep 30"), "MARK", Duration::from_millis(300))
            .unwrap_err();
        assert!(error.contains("did not finish"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn capture_reports_a_failing_shell() {
        let error =
            capture_shell_env(fake_shell("exit 3"), "MARK", Duration::from_secs(5)).unwrap_err();
        assert!(error.contains("printed no environment"), "{error}");
    }
}
