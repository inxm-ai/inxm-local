//! Host environment detection and cross-platform program resolution.
//!
//! Two jobs:
//!
//! 1. **Resolve programs portably.** On Windows, `Command::new("npx")` fails
//!    because `npx` is `npx.cmd` and `CreateProcess` does not apply
//!    `PATHEXT`. [`resolve_program`] searches `PATH` (honouring `PATHEXT` on
//!    Windows) and returns a spawnable path.
//! 2. **Describe the environment to the compiler.** [`EnvProbe`] detects the
//!    OS and which interpreters/runners exist, so compiled plans only use
//!    what is actually available (no `bash` steps on a bash-less Windows).

use std::path::{Path, PathBuf};

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

        // Scope env mutation to this test; other tests do not touch HOME/PATH.
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
}
