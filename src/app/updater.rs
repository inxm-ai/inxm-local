//! Desktop-initiated, verified update staging. The caller owns work quiescence
//! and durable draft saving; no installation starts from a release check.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const RELEASES_URL: &str = "https://github.com/inxm-ai/inxm-local/releases/latest";
const DOWNLOAD_BUFFER_SIZE: usize = 64 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_DOWNLOAD_REDIRECTS: usize = 10;
const HANDOFF_ACK_TIMEOUT: Duration = Duration::from_secs(5);
const CANCELLED: &str = "Update cancelled";
const BUNDLE_NAME: &str = "INXM Local.app";
const BINARY_NAME: &str = "inxm-local";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    #[serde(rename = "browser_download_url", alias = "url")]
    pub url: String,
    #[serde(default)]
    pub digest: Option<String>,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn from_github_value(value: &serde_json::Value) -> Result<Self, String> {
        if value.get("draft").and_then(serde_json::Value::as_bool) != Some(false)
            || value.get("prerelease").and_then(serde_json::Value::as_bool) != Some(false)
        {
            return Err("Only published stable releases can be installed".to_owned());
        }
        let version = value
            .get("tag_name")
            .and_then(serde_json::Value::as_str)
            .ok_or("Release has no version")?
            .trim_start_matches('v')
            .to_owned();
        validate_version(&version)?;
        let url = value
            .get("html_url")
            .and_then(serde_json::Value::as_str)
            .ok_or("Release has no page URL")?
            .to_owned();
        let assets = serde_json::from_value(
            value
                .get("assets")
                .cloned()
                .ok_or("Release has no assets")?,
        )
        .map_err(|error| error.to_string())?;
        Ok(Self {
            version,
            url,
            assets,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    Downloading { bytes: u64, total: Option<u64> },
    Verifying,
    Ready { version: String },
    Installing,
    Installed { restart_required: bool },
    Cancelled,
    Failed(String),
}

#[derive(Debug)]
pub struct InstallOutcome {
    pub restart_required: bool,
    /// A system installer was started; completion belongs to its UI/process.
    pub installer_handoff: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Installation {
    MacBundle(PathBuf),
    WindowsSetup { directory: PathBuf, all_users: bool },
    LinuxUser(PathBuf),
    LinuxDeb,
}

pub struct PreparedUpdate {
    staging: tempfile::TempDir,
    archive: PathBuf,
    installation: Installation,
    executable: PathBuf,
    version: String,
    digest: String,
}

impl std::fmt::Debug for PreparedUpdate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedUpdate")
            .field("version", &self.version)
            .field("installation", &self.installation)
            .finish_non_exhaustive()
    }
}

/// Downloads and verifies, without modifying the installed application.
pub fn download_and_stage(
    release: Release,
    cancel: Arc<AtomicBool>,
    emit: impl Fn(UpdateStatus),
) -> Result<PreparedUpdate, String> {
    let result = stage_release(&release, &cancel, &emit);
    match &result {
        Ok(_) => emit(UpdateStatus::Ready {
            version: release.version,
        }),
        Err(error) if error == CANCELLED => emit(UpdateStatus::Cancelled),
        Err(error) => emit(UpdateStatus::Failed(error.clone())),
    }
    result
}

fn stage_release(
    release: &Release,
    cancel: &AtomicBool,
    emit: &impl Fn(UpdateStatus),
) -> Result<PreparedUpdate, String> {
    check_cancel(cancel)?;
    validate_version(&release.version)?;
    let executable = std::env::current_exe()
        .map_err(update_error)?
        .canonicalize()
        .map_err(update_error)?;
    let installation = detect_installation(&executable)?;
    let asset = select_asset(release, &installation, std::env::consts::ARCH)?;
    let digest = required_digest(asset)?;
    let url = reqwest::Url::parse(&asset.url).map_err(update_error)?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !url
            .path()
            .starts_with("/inxm-ai/inxm-local/releases/download/")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(manual_error(
            "The release asset is not a GitHub HTTPS download",
        ));
    }
    let staging = tempfile::Builder::new()
        .prefix("inxm-update-")
        .tempdir()
        .map_err(update_error)?;
    let archive = staging.path().join(&asset.name);
    download_https(url, &archive, asset.size, cancel, emit)?;
    emit(UpdateStatus::Verifying);
    verify_digest(&archive, &digest, cancel)?;
    Ok(PreparedUpdate {
        staging,
        archive,
        installation,
        executable,
        version: release.version.clone(),
        digest,
    })
}

fn download_https(
    url: reqwest::Url,
    archive: &Path,
    expected_size: u64,
    cancel: &AtomicBool,
    emit: &impl Fn(UpdateStatus),
) -> Result<(), String> {
    // This synchronous API is called on the engine's blocking pool. A small
    // async runtime allows cancellation to interrupt stalled HTTP reads too.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(update_error)?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(DOWNLOAD_TIMEOUT)
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= MAX_DOWNLOAD_REDIRECTS {
                    return attempt.error("Too many redirects");
                }
                if attempt.url().scheme() != "https" {
                    return attempt.error("Non-HTTPS redirect refused");
                }
                attempt.follow()
            }))
            .user_agent(format!("inxm-local/{}", crate::version::DISPLAY))
            .build()
            .map_err(update_error)?;
        let mut response = tokio::select! {
            result = client.get(url).send() => result.map_err(update_error)?,
            _ = wait_for_cancel(cancel) => return Err(CANCELLED.to_owned()),
        }
        .error_for_status()
        .map_err(update_error)?;
        check_cancel(cancel)?;
        if response
            .content_length()
            .is_some_and(|length| length != expected_size)
        {
            return Err(manual_error(
                "Release asset size disagrees with the download",
            ));
        }
        let mut file = File::create(archive).map_err(update_error)?;
        let mut sink = DownloadSink::new(&mut file, expected_size, cancel, emit);
        loop {
            let chunk = tokio::select! {
                result = response.chunk() => result.map_err(update_error)?,
                _ = wait_for_cancel(cancel) => return Err(CANCELLED.to_owned()),
            };
            check_cancel(cancel)?;
            let Some(chunk) = chunk else {
                break;
            };
            sink.write_chunk(&chunk)?;
        }
        sink.finish()?;
        file.sync_all().map_err(update_error)?;
        Ok(())
    })
}

async fn wait_for_cancel(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Acquire) {
        tokio::time::sleep(CANCEL_POLL_INTERVAL).await;
    }
}

fn validate_version(version: &str) -> Result<(), String> {
    let core = version.split('+').next().unwrap_or(version);
    let components: Vec<_> = core.split('.').collect();
    if components.len() != 3
        || components.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
        })
    {
        return Err("Only stable release versions can be installed".to_owned());
    }
    Ok(())
}

fn detect_installation(executable: &Path) -> Result<Installation, String> {
    match std::env::consts::OS {
        "macos" => executable
            .ancestors()
            .find(|path| path.extension().is_some_and(|extension| extension == "app"))
            .map(|path| Installation::MacBundle(path.to_owned()))
            .ok_or_else(|| manual_error("This executable is not inside an application bundle")),
        "windows" => {
            let directory = executable
                .parent()
                .ok_or_else(|| manual_error("Unknown install directory"))?;
            if !directory.join("unins000.exe").is_file() {
                return Err(manual_error(
                    "This is not an installed Windows setup installation",
                ));
            }
            let all_users = windows_install_scope(directory)?;
            Ok(Installation::WindowsSetup {
                directory: directory.to_owned(),
                all_users,
            })
        }
        "linux" => {
            if executable == Path::new("/usr/bin/inxm-local") {
                let owned = Command::new("dpkg-query")
                    .args(["-S", "/usr/bin/inxm-local"])
                    .output()
                    .map_err(update_error)?;
                if owned.status.success()
                    && String::from_utf8_lossy(&owned.stdout)
                        .lines()
                        .any(|line| line.starts_with("inxm-local: "))
                {
                    return Ok(Installation::LinuxDeb);
                }
                return Err(manual_error(
                    "The system executable is not owned by the INXM Debian package",
                ));
            }
            let prefix = executable
                .parent()
                .filter(|path| path.file_name().is_some_and(|name| name == "bin"))
                .and_then(Path::parent)
                .ok_or_else(|| manual_error("Unsupported Linux installation layout"))?;
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| manual_error("Cannot identify the per-user installation"))?
                .canonicalize()
                .map_err(update_error)?;
            if !prefix.starts_with(&home)
                || !prefix.join("share/doc/inxm-local").is_dir()
                || !prefix
                    .join("share/applications/ai.inxm.local.desktop")
                    .is_file()
            {
                return Err(manual_error(
                    "Only existing per-user or Debian installations can be updated",
                ));
            }
            Ok(Installation::LinuxUser(prefix.to_owned()))
        }
        _ => Err(manual_error(
            "This operating system does not support update installation",
        )),
    }
}

fn select_asset<'a>(
    release: &'a Release,
    installation: &Installation,
    arch: &str,
) -> Result<&'a Asset, String> {
    let name = match (installation, arch) {
        (Installation::MacBundle(_), "aarch64") => "inxm-local-aarch64-apple-darwin.app.zip",
        (Installation::MacBundle(_), "x86_64") => "inxm-local-x86_64-apple-darwin.app.zip",
        (Installation::WindowsSetup { .. }, "x86_64") => {
            "inxm-local-x86_64-pc-windows-msvc-setup.exe"
        }
        (Installation::LinuxUser(_), "x86_64") => "inxm-local-x86_64-unknown-linux-gnu.tar.gz",
        (Installation::LinuxDeb, "x86_64") => "inxm-local-x86_64-unknown-linux-gnu.deb",
        _ => {
            return Err(manual_error(
                "No supported release asset for this architecture and installation",
            ));
        }
    };
    let mut matches = release.assets.iter().filter(|asset| asset.name == name);
    let asset = matches
        .next()
        .ok_or_else(|| manual_error("The release has no matching installation asset"))?;
    if matches.next().is_some() {
        return Err(manual_error(
            "Release contains ambiguous installation assets",
        ));
    }
    Ok(asset)
}

fn windows_install_scope(directory: &Path) -> Result<bool, String> {
    let system_root = std::env::var_os("SystemRoot")
        .ok_or_else(|| manual_error("Cannot locate Windows registry tools"))?;
    let registry_tool = PathBuf::from(system_root).join("System32/reg.exe");
    let expected = normalize_windows_directory(&directory.to_string_lossy());
    let mut matches = Vec::new();
    for (hive, all_users) in [("HKCU", false), ("HKLM", true)] {
        let key = format!(
            "{hive}\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\ai.inxm.local_is1"
        );
        let output = Command::new(&registry_tool)
            .args(["query", &key, "/v", "InstallLocation", "/reg:64"])
            .output()
            .map_err(update_error)?;
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            if text
                .lines()
                .filter_map(|line| line.split_once("REG_SZ"))
                .any(|(_, path)| normalize_windows_directory(path.trim()) == expected)
            {
                matches.push(all_users);
            }
        }
    }
    match matches.as_slice() {
        [all_users] => Ok(*all_users),
        _ => Err(manual_error(
            "Cannot unambiguously identify the existing Windows installation scope",
        )),
    }
}

fn normalize_windows_directory(directory: &str) -> String {
    directory
        .trim_end_matches(['\\', '/'])
        .replace('/', "\\")
        .to_lowercase()
}

fn required_digest(asset: &Asset) -> Result<String, String> {
    let hex = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| manual_error("The selected asset has no valid GitHub SHA-256 digest"))?;
    Ok(hex.to_ascii_lowercase())
}

struct DownloadSink<'a, W, F> {
    writer: &'a mut W,
    expected_size: u64,
    bytes: u64,
    cancel: &'a AtomicBool,
    emit: &'a F,
}

impl<'a, W: Write, F: Fn(UpdateStatus)> DownloadSink<'a, W, F> {
    fn new(writer: &'a mut W, expected_size: u64, cancel: &'a AtomicBool, emit: &'a F) -> Self {
        emit(UpdateStatus::Downloading {
            bytes: 0,
            total: Some(expected_size),
        });
        Self {
            writer,
            expected_size,
            bytes: 0,
            cancel,
            emit,
        }
    }

    fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), String> {
        check_cancel(self.cancel)?;
        self.bytes += chunk.len() as u64;
        if self.bytes > self.expected_size {
            return Err(manual_error("Download exceeds the release asset size"));
        }
        self.writer.write_all(chunk).map_err(update_error)?;
        (self.emit)(UpdateStatus::Downloading {
            bytes: self.bytes,
            total: Some(self.expected_size),
        });
        Ok(())
    }

    fn finish(self) -> Result<(), String> {
        check_cancel(self.cancel)?;
        if self.bytes != self.expected_size {
            return Err(manual_error(
                "Incomplete update download; retry the download",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
fn stream_download(
    reader: &mut impl Read,
    writer: &mut impl Write,
    expected_size: u64,
    cancel: &AtomicBool,
    emit: &impl Fn(UpdateStatus),
    _total: Option<u64>,
) -> Result<(), String> {
    let mut buffer = [0; DOWNLOAD_BUFFER_SIZE];
    let mut sink = DownloadSink::new(writer, expected_size, cancel, emit);
    loop {
        check_cancel(cancel)?;
        let count = reader.read(&mut buffer).map_err(update_error)?;
        check_cancel(cancel)?;
        if count == 0 {
            break;
        }
        sink.write_chunk(&buffer[..count])?;
    }
    sink.finish()
}

fn verify_digest(path: &Path, expected: &str, cancel: &AtomicBool) -> Result<(), String> {
    let mut file = File::open(path).map_err(update_error)?;
    let mut buffer = [0; DOWNLOAD_BUFFER_SIZE];
    let mut hash = Sha256::new();
    loop {
        check_cancel(cancel)?;
        let count = file.read(&mut buffer).map_err(update_error)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    if format!("{:x}", hash.finalize()) != expected {
        return Err(manual_error(
            "SHA-256 verification failed; the installation was left untouched",
        ));
    }
    Ok(())
}

impl PreparedUpdate {
    /// Requires the desktop coordinator's exclusive update gate and saved drafts.
    pub fn install(self) -> Result<InstallOutcome, String> {
        // Verify again: a staged download may have waited while work finished.
        verify_digest(&self.archive, &self.digest, &AtomicBool::new(false))?;
        match &self.installation {
            Installation::MacBundle(destination) => {
                let parent = destination
                    .parent()
                    .ok_or_else(|| manual_error("Unknown bundle directory"))?;
                let stage = adjacent_stage(parent)?;
                self_update::Extract::from_source(&self.archive)
                    .extract_into(stage.path())
                    .map_err(update_error)?;
                let bundle = stage.path().join(BUNDLE_NAME);
                validate_bundle(&bundle)?;
                replace_bundle(&bundle, destination, &self.executable, stage.path())?;
                Ok(InstallOutcome {
                    restart_required: true,
                    installer_handoff: false,
                })
            }
            Installation::LinuxUser(prefix) => {
                let stage = adjacent_stage(prefix)?;
                self_update::Extract::from_source(&self.archive)
                    .extract_into(stage.path())
                    .map_err(update_error)?;
                install_linux_tree(&stage.path().join(BINARY_NAME), prefix, stage.path())?;
                Ok(InstallOutcome {
                    restart_required: true,
                    installer_handoff: false,
                })
            }
            Installation::WindowsSetup { .. } => {
                // No /TASKS override: Inno's UsePreviousTasks=yes preserves both
                // startup and desktop-icon choices. /DIR preserves custom installs.
                self.start_installer_worker()?;
                // The detached setup may still be reading the download after exit.
                let _installer_directory = self.staging.keep();
                Ok(InstallOutcome {
                    restart_required: true,
                    installer_handoff: true,
                })
            }
            Installation::LinuxDeb => {
                self.start_installer_worker()?;
                let _installer_directory = self.staging.keep();
                Ok(InstallOutcome {
                    restart_required: true,
                    installer_handoff: true,
                })
            }
        }
    }

    fn start_installer_worker(&self) -> Result<(), String> {
        let marker = super::update_gate::handoff_marker()?;
        let parent_pid = std::process::id();
        let mut marker_file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&marker)
            .map_err(update_error)?;
        let marker_written =
            writeln!(marker_file, "{parent_pid}").and_then(|()| marker_file.sync_all());
        drop(marker_file);
        if let Err(error) = marker_written {
            let _ = fs::remove_file(&marker);
            return Err(update_error(error));
        }
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        let command = installer_worker(InstallerWorkerRequest {
            installation: &self.installation,
            archive: &self.archive,
            executable: &self.executable,
            original_args: &args,
            parent_pid,
            staging: self.staging.path(),
            marker: &marker,
            digest: &self.digest,
        });
        let result = command.and_then(|(program, arguments)| {
            let mut child = Command::new(program)
                .args(arguments)
                .spawn()
                .map_err(update_error)?;
            let deadline = std::time::Instant::now() + HANDOFF_ACK_TIMEOUT;
            loop {
                let acknowledged = fs::read_to_string(&marker)
                    .ok()
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
                    .is_some_and(|pid| pid != 0 && pid != parent_pid);
                if acknowledged {
                    return Ok(());
                }
                if child.try_wait().map_err(update_error)?.is_some() {
                    return Err(manual_error(
                        "The installer supervisor exited before acknowledging the update gate",
                    ));
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(manual_error(
                        "The installer supervisor did not acknowledge the update gate",
                    ));
                }
                std::thread::sleep(CANCEL_POLL_INTERVAL);
            }
        });
        if result.is_err() {
            let _ = fs::remove_file(marker);
        }
        result
    }
}

struct InstallerWorkerRequest<'a> {
    installation: &'a Installation,
    archive: &'a Path,
    executable: &'a Path,
    original_args: &'a [std::ffi::OsString],
    parent_pid: u32,
    staging: &'a Path,
    marker: &'a Path,
    digest: &'a str,
}

fn installer_worker(
    request: InstallerWorkerRequest<'_>,
) -> Result<(PathBuf, Vec<std::ffi::OsString>), String> {
    let InstallerWorkerRequest {
        installation,
        archive,
        executable,
        original_args,
        parent_pid,
        staging,
        marker,
        digest,
    } = request;
    let (installer, install_args) = handoff_command(installation, archive)?;
    let log = staging.join("installer.log");
    match installation {
        Installation::WindowsSetup { .. } => {
            let script = staging.join("install-and-restart.ps1");
            let install_line = windows_argument_line(&install_args)?;
            let original_line = windows_argument_line(original_args)?;
            let installer = powershell_literal(&installer)?;
            let executable = powershell_literal(executable.as_os_str())?;
            let install_line = powershell_literal(std::ffi::OsStr::new(&install_line))?;
            let original_line = powershell_literal(std::ffi::OsStr::new(&original_line))?;
            let log = powershell_literal(log.as_os_str())?;
            let cleanup = powershell_literal(staging.as_os_str())?;
            let marker = powershell_literal(marker.as_os_str())?;
            let archive = powershell_literal(archive.as_os_str())?;
            let content = format!(
                "[IO.File]::WriteAllText({marker}, [string]$PID)\n$parent = Get-Process -Id {parent_pid} -ErrorAction SilentlyContinue\nif ($parent) {{ $parent.WaitForExit() }}\n$result = 1\n$setup = $null\ntry {{\n  $hash = (Get-FileHash -LiteralPath {archive} -Algorithm SHA256 -ErrorAction Stop).Hash.ToLowerInvariant()\n  if ($hash -ne '{digest}') {{ throw 'SHA-256 verification failed; installation left untouched' }}\n  $setup = Start-Process -FilePath {installer} -ArgumentList {install_line} -PassThru\n  [IO.File]::WriteAllText({marker}, [string]$setup.Id)\n}} catch {{ $_ | Out-File -FilePath {log} }}\nif ($setup) {{ $setup.WaitForExit(); $result = $setup.ExitCode }}\nRemove-Item -LiteralPath {marker} -Force -ErrorAction SilentlyContinue\nStart-Process -FilePath {executable}{}\nif ($result -eq 0) {{ Remove-Item -LiteralPath {cleanup} -Recurse -Force }}\n",
                if original_args.is_empty() {
                    String::new()
                } else {
                    format!(" -ArgumentList {original_line}")
                }
            );
            // Windows PowerShell 5 reads BOM-marked UTF-8 scripts reliably.
            fs::write(&script, format!("\u{feff}{content}")).map_err(update_error)?;
            let root = std::env::var_os("SystemRoot")
                .ok_or_else(|| manual_error("Cannot locate Windows PowerShell"))?;
            let program =
                PathBuf::from(root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
            if !program.is_file() {
                return Err(manual_error(
                    "Windows PowerShell is unavailable for installer handoff",
                ));
            }
            Ok((
                program,
                vec![
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-WindowStyle".into(),
                    "Hidden".into(),
                    "-ExecutionPolicy".into(),
                    "Bypass".into(),
                    "-File".into(),
                    script.into_os_string(),
                ],
            ))
        }
        Installation::LinuxDeb => {
            if !Path::new("/usr/bin/pkexec").is_file()
                || !Path::new("/usr/bin/dpkg").is_file()
                || !Path::new("/usr/bin/sha256sum").is_file()
            {
                return Err(manual_error(
                    "The Debian installer or graphical authorization helper is unavailable",
                ));
            }
            let script = staging.join("install-and-restart.sh");
            let mut installer_line = shell_literal(&installer)?;
            for arg in install_args {
                installer_line.push(' ');
                installer_line.push_str(&shell_literal(&arg)?);
            }
            let mut restart_line = shell_literal(executable.as_os_str())?;
            for arg in original_args {
                restart_line.push(' ');
                restart_line.push_str(&shell_literal(arg)?);
            }
            let log = shell_literal(log.as_os_str())?;
            let cleanup = shell_literal(staging.as_os_str())?;
            let marker = shell_literal(marker.as_os_str())?;
            let archive = shell_literal(archive.as_os_str())?;
            let verify = format!(
                "hash=$(/usr/bin/sha256sum -- {archive} 2>>{log}); hash=${{hash%% *}}; [ \"$hash\" = '{digest}' ]"
            );
            let content = linux_supervisor_script(
                parent_pid,
                &installer_line,
                &restart_line,
                &log,
                &cleanup,
                &marker,
                &verify,
            );
            fs::write(&script, content).map_err(update_error)?;
            Ok((PathBuf::from("/bin/sh"), vec![script.into_os_string()]))
        }
        _ => Err(manual_error(
            "This installation does not use an installer worker",
        )),
    }
}

fn linux_supervisor_script(
    parent_pid: u32,
    installer_line: &str,
    restart_line: &str,
    log: &str,
    cleanup: &str,
    marker: &str,
    verify: &str,
) -> String {
    format!(
        "#!/bin/sh\nprintf '%s\\n' \"$$\" >{marker}\nwhile kill -0 {parent_pid} 2>/dev/null; do sleep 1; done\ninstaller_result=1\nif {verify}; then\n  {installer_line} >{log} 2>&1 &\n  installer_pid=$!\n  printf '%s\\n' \"$installer_pid\" >{marker}\n  wait \"$installer_pid\"\n  installer_result=$?\nelse\n  printf '%s\\n' 'SHA-256 verification failed; installation left untouched' >>{log}\nfi\nrm -f -- {marker}\n{restart_line} &\nif [ \"$installer_result\" -eq 0 ]; then rm -rf -- {cleanup}; fi\n"
    )
}

fn shell_literal(value: &std::ffi::OsStr) -> Result<String, String> {
    let value = value
        .to_str()
        .ok_or_else(|| manual_error("Installer paths or arguments have unsupported encoding"))?;
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

fn powershell_literal(value: &std::ffi::OsStr) -> Result<String, String> {
    let value = value
        .to_str()
        .ok_or_else(|| manual_error("Installer paths or arguments have unsupported encoding"))?;
    Ok(format!("'{}'", value.replace('\'', "''")))
}

fn windows_argument_line(args: &[std::ffi::OsString]) -> Result<String, String> {
    args.iter()
        .map(|arg| {
            let value = arg
                .to_str()
                .ok_or_else(|| manual_error("Installer arguments have unsupported encoding"))?;
            let mut quoted = String::from("\"");
            let mut backslashes = 0;
            for character in value.chars() {
                if character == '\\' {
                    backslashes += 1;
                    continue;
                }
                if character == '"' {
                    quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                } else {
                    quoted.extend(std::iter::repeat_n('\\', backslashes));
                }
                backslashes = 0;
                quoted.push(character);
            }
            quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
            quoted.push('"');
            Ok(quoted)
        })
        .collect::<Result<Vec<_>, String>>()
        .map(|args| args.join(" "))
}

fn adjacent_stage(parent: &Path) -> Result<tempfile::TempDir, String> {
    tempfile::Builder::new()
        .prefix(".inxm-update-")
        .tempdir_in(parent)
        .map_err(update_error)
}

fn validate_bundle(bundle: &Path) -> Result<(), String> {
    let binary = bundle.join("Contents/MacOS/inxm-local");
    let plist = bundle.join("Contents/Info.plist");
    if !bundle.is_dir()
        || !binary.is_file()
        || !plist.is_file()
        || !bundle.join("Contents/Resources").is_dir()
    {
        return Err(manual_error(
            "The downloaded archive is not a complete INXM application bundle",
        ));
    }
    ensure_tree_contained(bundle)?;
    ensure_executable(&binary)?;
    Ok(())
}

fn ensure_tree_contained(root: &Path) -> Result<(), String> {
    if !fs::symlink_metadata(root).map_err(update_error)?.is_dir() {
        return Err(manual_error(
            "Archive application root must be a real directory",
        ));
    }
    let canonical_root = root.canonicalize().map_err(update_error)?;
    let mut directories = vec![root.to_owned()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory).map_err(update_error)? {
            let entry = entry.map_err(update_error)?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(update_error)?;
            if metadata.file_type().is_symlink() {
                if !entry
                    .path()
                    .canonicalize()
                    .map_err(update_error)?
                    .starts_with(&canonical_root)
                {
                    return Err(manual_error(
                        "Archive contains a link outside the application tree",
                    ));
                }
            } else if metadata.is_dir() {
                directories.push(entry.path());
            } else if !metadata.is_file() {
                return Err(manual_error("Archive contains a special file"));
            }
        }
    }
    Ok(())
}

fn ensure_executable(binary: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(binary)
            .map_err(update_error)?
            .permissions()
            .mode()
            & 0o111
            == 0
        {
            return Err(manual_error("The staged application is not executable"));
        }
    }
    #[cfg(not(unix))]
    let _ = binary;
    Ok(())
}

fn replace_bundle(
    source: &Path,
    destination: &Path,
    executable: &Path,
    stage: &Path,
) -> Result<(), String> {
    let stash = adjacent_stage(
        destination
            .parent()
            .ok_or_else(|| manual_error("Unknown bundle directory"))?,
    )?;
    let aside = stage.join("running-image");
    let moved_image = executable.starts_with(destination);
    if moved_image {
        fs::rename(executable, &aside).map_err(update_error)?;
    }
    let result = self_update::MoveAll::from_temp(stash.path())
        .add(source, destination)
        .commit();
    if let Err(error) = result {
        if moved_image && let Err(restore_error) = fs::rename(&aside, executable) {
            let recovery_image = stash.path().join("running-image");
            let preserve = fs::rename(&aside, &recovery_image);
            let recovery = stash.keep();
            return Err(manual_error(&format!(
                "Update failed ({error}) and executable restore failed ({restore_error}); recovery retained at {} (image preservation: {preserve:?})",
                recovery.display()
            )));
        }
        return Err(update_error(error));
    }
    Ok(())
}

fn install_linux_tree(source: &Path, prefix: &Path, stage: &Path) -> Result<(), String> {
    ensure_tree_contained(source)?;
    let binary = source.join(BINARY_NAME);
    let icon = source.join("ai.inxm.local.png");
    let documents = ["LICENSE", "NOTICE", "THIRD_PARTY_LICENSES.md"];
    ensure_executable(&binary)?;
    if !binary.is_file()
        || !icon.is_file()
        || documents.iter().any(|name| !source.join(name).is_file())
    {
        return Err(manual_error(
            "The downloaded archive is not a complete per-user installation",
        ));
    }
    let doc_stage = stage.join("documents");
    fs::create_dir(&doc_stage).map_err(update_error)?;
    for name in documents {
        fs::rename(source.join(name), doc_stage.join(name)).map_err(update_error)?;
    }
    let stash = adjacent_stage(prefix)?;
    let mut moves = self_update::MoveAll::from_temp(stash.path());
    moves
        .add(binary, prefix.join("bin/inxm-local"))
        .add(
            icon,
            prefix.join("share/icons/hicolor/512x512/apps/ai.inxm.local.png"),
        )
        .add(doc_stage, prefix.join("share/doc/inxm-local"))
        .commit()
        .map_err(update_error)?;
    // Existing launcher/autostart files carry the install path and launch mode;
    // leave those preferences intact. This archive has no changed launcher template.
    Ok(())
}

fn handoff_command(
    installation: &Installation,
    archive: &Path,
) -> Result<(std::ffi::OsString, Vec<std::ffi::OsString>), String> {
    match installation {
        Installation::WindowsSetup {
            directory,
            all_users,
        } => {
            let mut install_dir = std::ffi::OsString::from("/DIR=");
            install_dir.push(directory.as_os_str());
            Ok((
                archive.as_os_str().to_owned(),
                vec![
                    "/NORESTART".into(),
                    "/CLOSEAPPLICATIONS".into(),
                    "/NORESTARTAPPLICATIONS".into(),
                    if *all_users {
                        "/ALLUSERS".into()
                    } else {
                        "/CURRENTUSER".into()
                    },
                    install_dir,
                ],
            ))
        }
        Installation::LinuxDeb => Ok((
            "/usr/bin/pkexec".into(),
            vec![
                "/usr/bin/dpkg".into(),
                "-i".into(),
                archive.as_os_str().to_owned(),
            ],
        )),
        _ => Err(manual_error(
            "This installation does not use a system installer",
        )),
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err(CANCELLED.to_owned())
    } else {
        Ok(())
    }
}

fn update_error(error: impl std::fmt::Display) -> String {
    manual_error(&error.to_string())
}
fn manual_error(message: &str) -> String {
    format!("{message}. Manual update: {RELEASES_URL}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::Cursor;

    fn asset(name: &str, bytes: &[u8]) -> Asset {
        Asset {
            name: name.to_owned(),
            url: format!("https://github.com/inxm-ai/inxm-local/releases/download/v1.2.3/{name}"),
            digest: Some(format!("sha256:{:x}", Sha256::digest(bytes))),
            size: bytes.len() as u64,
        }
    }

    fn release() -> Release {
        Release {
            version: "1.2.3".to_owned(),
            url: RELEASES_URL.to_owned(),
            assets: [
                "inxm-local-aarch64-apple-darwin.app.zip",
                "inxm-local-x86_64-apple-darwin.app.zip",
                "inxm-local-x86_64-pc-windows-msvc-setup.exe",
                "inxm-local-x86_64-unknown-linux-gnu.tar.gz",
                "inxm-local-x86_64-unknown-linux-gnu.deb",
            ]
            .iter()
            .map(|name| asset(name, b"archive"))
            .collect(),
        }
    }

    fn file(path: &Path, contents: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn executable(path: &Path, contents: &[u8]) {
        file(path, contents);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn selects_exact_architecture_and_installation_assets() {
        let release = release();
        for (installation, arch, suffix) in [
            (
                Installation::MacBundle("app".into()),
                "aarch64",
                "aarch64-apple-darwin.app.zip",
            ),
            (
                Installation::MacBundle("app".into()),
                "x86_64",
                "x86_64-apple-darwin.app.zip",
            ),
            (
                Installation::WindowsSetup {
                    directory: "app".into(),
                    all_users: false,
                },
                "x86_64",
                "msvc-setup.exe",
            ),
            (
                Installation::LinuxUser("prefix".into()),
                "x86_64",
                "gnu.tar.gz",
            ),
            (Installation::LinuxDeb, "x86_64", "gnu.deb"),
        ] {
            assert!(
                select_asset(&release, &installation, arch)
                    .unwrap()
                    .name
                    .ends_with(suffix)
            );
        }
        assert!(select_asset(&release, &Installation::LinuxDeb, "aarch64").is_err());
        assert!(
            select_asset(
                &Release {
                    assets: vec![],
                    ..release
                },
                &Installation::LinuxDeb,
                "x86_64"
            )
            .is_err()
        );
    }

    #[test]
    fn published_release_response_requires_stable_version_and_digest() {
        let mut value = serde_json::json!({"draft":false,"prerelease":false,"tag_name":"v1.2.3",
            "html_url":RELEASES_URL,"assets":release().assets});
        let parsed = Release::from_github_value(&value).unwrap();
        assert_eq!(parsed.version, "1.2.3");
        value["prerelease"] = true.into();
        assert!(Release::from_github_value(&value).is_err());
        value["prerelease"] = false.into();
        value["draft"] = true.into();
        assert!(Release::from_github_value(&value).is_err());
        for version in ["1.2.3-rc.1", "latest", "1.2", "01.2.3", "1.2.3.4"] {
            assert!(validate_version(version).is_err(), "{version}");
        }
        for digest in [
            None,
            Some("sha512:000".to_owned()),
            Some("sha256:wrong".to_owned()),
        ] {
            assert!(
                required_digest(&Asset {
                    digest,
                    ..asset("x", b"abc")
                })
                .is_err()
            );
        }
    }

    #[test]
    fn verifies_download_and_rejects_corruption_without_touching_install() {
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("installed");
        file(&installed, b"old application");
        let archive = directory.path().join("archive");
        let expected = required_digest(&asset("x", b"valid archive")).unwrap();
        file(&archive, b"valid archive");
        verify_digest(&archive, &expected, &AtomicBool::new(false)).unwrap();
        file(&archive, b"corrupt archive");
        assert!(verify_digest(&archive, &expected, &AtomicBool::new(false)).is_err());
        assert_eq!(fs::read(installed).unwrap(), b"old application");
    }

    #[test]
    fn reports_progress_and_rejects_truncated_or_oversized_downloads() {
        let statuses = RefCell::new(Vec::new());
        let emit = |status| statuses.borrow_mut().push(status);
        let mut output = Vec::new();
        stream_download(
            &mut Cursor::new(b"abc"),
            &mut output,
            3,
            &AtomicBool::new(false),
            &emit,
            Some(3),
        )
        .unwrap();
        assert_eq!(output, b"abc");
        assert_eq!(
            statuses.borrow().last(),
            Some(&UpdateStatus::Downloading {
                bytes: 3,
                total: Some(3)
            })
        );
        assert!(
            stream_download(
                &mut Cursor::new(b"ab"),
                &mut Vec::new(),
                3,
                &AtomicBool::new(false),
                &emit,
                Some(3)
            )
            .is_err()
        );
        assert!(
            stream_download(
                &mut Cursor::new(b"abcd"),
                &mut Vec::new(),
                3,
                &AtomicBool::new(false),
                &emit,
                Some(3)
            )
            .is_err()
        );
    }

    #[test]
    fn cancellation_before_or_during_read_does_not_write_the_chunk() {
        struct CancellingReader<'a>(&'a AtomicBool);
        impl Read for CancellingReader<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                buffer[0] = b'x';
                self.0.store(true, Ordering::Release);
                Ok(1)
            }
        }
        let cancel = AtomicBool::new(false);
        let mut output = Vec::new();
        assert_eq!(
            stream_download(
                &mut CancellingReader(&cancel),
                &mut output,
                1,
                &cancel,
                &|_| {},
                Some(1)
            ),
            Err(CANCELLED.to_owned())
        );
        assert!(output.is_empty());
        assert_eq!(
            stream_download(
                &mut Cursor::new(b"x"),
                &mut output,
                1,
                &cancel,
                &|_| {},
                Some(1)
            ),
            Err(CANCELLED.to_owned())
        );
    }

    #[test]
    fn cancellation_interrupts_a_server_that_never_sends_headers() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let server_cancel = cancelled.clone();
        let (finish, finished) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            server_cancel.store(true, Ordering::Release);
            let _ = finished.recv_timeout(Duration::from_secs(2));
        });
        let directory = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let result = download_https(
            reqwest::Url::parse(&format!("http://{address}/fixture")).unwrap(),
            &directory.path().join("archive"),
            3,
            &cancelled,
            &|_| {},
        );
        finish.send(()).unwrap();
        server.join().unwrap();
        assert_eq!(result, Err(CANCELLED.to_owned()));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn whole_bundle_replacement_preserves_install_path_and_replaces_resources() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("Custom Name.app");
        let stage = adjacent_stage(directory.path()).unwrap();
        let source = stage.path().join(BUNDLE_NAME);
        let running = target.join("Contents/MacOS/inxm-local");
        executable(&running, b"old");
        file(&target.join("Contents/Resources/stale"), b"stale");
        executable(&source.join("Contents/MacOS/inxm-local"), b"new");
        file(&source.join("Contents/Info.plist"), b"plist");
        file(&source.join("Contents/Resources/fresh"), b"fresh");
        validate_bundle(&source).unwrap();
        replace_bundle(&source, &target, &running, stage.path()).unwrap();
        assert_eq!(fs::read(running).unwrap(), b"new");
        assert!(!target.join("Contents/Resources/stale").exists());
        assert!(target.join("Contents/Resources/fresh").exists());
    }

    #[test]
    fn failed_bundle_move_rolls_back_the_running_executable_and_bundle() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join(BUNDLE_NAME);
        let running = target.join("Contents/MacOS/inxm-local");
        executable(&running, b"old");
        let stage = adjacent_stage(directory.path()).unwrap();
        assert!(
            replace_bundle(
                &stage.path().join("missing"),
                &target,
                &running,
                stage.path()
            )
            .is_err()
        );
        assert_eq!(fs::read(running).unwrap(), b"old");
        assert!(target.is_dir());
    }

    #[test]
    fn incomplete_bundle_and_unavailable_install_parent_fail_before_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join(BUNDLE_NAME);
        executable(&bundle.join("Contents/MacOS/inxm-local"), b"exe");
        assert!(validate_bundle(&bundle).is_err());
        let file_parent = directory.path().join("read-only-or-invalid");
        file(&file_parent, b"not a directory");
        assert!(
            adjacent_stage(&file_parent)
                .unwrap_err()
                .contains("Manual update")
        );
    }

    #[test]
    fn linux_updates_all_shipped_files_and_preserves_launch_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let prefix = directory.path().join("prefix with spaces");
        let stage = adjacent_stage(directory.path()).unwrap();
        let source = stage.path().join(BINARY_NAME);
        executable(&prefix.join("bin/inxm-local"), b"old");
        file(
            &prefix.join("share/icons/hicolor/512x512/apps/ai.inxm.local.png"),
            b"old icon",
        );
        file(&prefix.join("share/doc/inxm-local/stale"), b"stale");
        let launcher = prefix.join("share/applications/ai.inxm.local.desktop");
        file(&launcher, b"custom launch flags");
        executable(&source.join(BINARY_NAME), b"new");
        file(&source.join("ai.inxm.local.png"), b"new icon");
        for name in ["LICENSE", "NOTICE", "THIRD_PARTY_LICENSES.md"] {
            file(&source.join(name), b"doc");
        }
        install_linux_tree(&source, &prefix, stage.path()).unwrap();
        assert_eq!(fs::read(prefix.join("bin/inxm-local")).unwrap(), b"new");
        assert_eq!(fs::read(launcher).unwrap(), b"custom launch flags");
        assert!(!prefix.join("share/doc/inxm-local/stale").exists());
        assert!(prefix.join("share/doc/inxm-local/NOTICE").is_file());
    }

    #[test]
    fn failed_linux_icon_move_restores_the_old_binary_and_documents() {
        let directory = tempfile::tempdir().unwrap();
        let prefix = directory.path().join("prefix");
        let stage = adjacent_stage(directory.path()).unwrap();
        let source = stage.path().join(BINARY_NAME);
        executable(&prefix.join("bin/inxm-local"), b"old");
        file(&prefix.join("share/doc/inxm-local/NOTICE"), b"old notice");
        executable(&source.join(BINARY_NAME), b"new");
        file(&source.join("ai.inxm.local.png"), b"new icon");
        for name in ["LICENSE", "NOTICE", "THIRD_PARTY_LICENSES.md"] {
            file(&source.join(name), b"doc");
        }
        // The icon's destination parent is absent: fail after moving the binary.
        assert!(install_linux_tree(&source, &prefix, stage.path()).is_err());
        assert_eq!(fs::read(prefix.join("bin/inxm-local")).unwrap(), b"old");
        assert_eq!(
            fs::read(prefix.join("share/doc/inxm-local/NOTICE")).unwrap(),
            b"old notice"
        );
    }

    #[test]
    fn installer_commands_preserve_windows_directory_and_remembered_tasks() {
        let directory = PathBuf::from("C:\\Users\\Example\\Custom Install");
        let archive = PathBuf::from("C:\\Temp\\setup.exe");
        let (program, args) = handoff_command(
            &Installation::WindowsSetup {
                directory: directory.clone(),
                all_users: false,
            },
            &archive,
        )
        .unwrap();
        assert_eq!(program, archive.as_os_str());
        assert!(args.iter().any(|arg| arg == "/NORESTARTAPPLICATIONS"));
        assert!(
            !args
                .iter()
                .any(|arg| arg.to_string_lossy().starts_with("/TASKS="))
        );
        assert!(
            args.iter()
                .any(|arg| arg == std::ffi::OsStr::new(&format!("/DIR={}", directory.display())))
        );
        let (program, args) =
            handoff_command(&Installation::LinuxDeb, Path::new("/tmp/update.deb")).unwrap();
        assert_eq!(program, "/usr/bin/pkexec");
        assert_eq!(args, ["/usr/bin/dpkg", "-i", "/tmp/update.deb"]);
    }

    #[test]
    fn quotes_windows_restart_arguments_without_losing_spaces_quotes_or_backslashes() {
        let args =
            ["--agent-mode", "space here", "a\"b", "C:\\tail\\", ""].map(std::ffi::OsString::from);
        assert_eq!(
            windows_argument_line(&args).unwrap(),
            "\"--agent-mode\" \"space here\" \"a\\\"b\" \"C:\\tail\\\\\" \"\""
        );
        assert_eq!(
            powershell_literal(std::ffi::OsStr::new("path's $value")).unwrap(),
            "'path''s $value'"
        );
    }

    #[cfg(unix)]
    #[test]
    fn shell_restart_quoting_preserves_metacharacters_as_literal_arguments() {
        let value = "spaces 'quotes' $HOME `command` $(command) \\\nnext line";
        let script = format!(
            "printf '%s' {}",
            shell_literal(std::ffi::OsStr::new(value)).unwrap()
        );
        let output = Command::new("/bin/sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, value.as_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn linux_supervisor_smoke_restarts_original_arguments_and_clears_gate() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging");
        fs::create_dir(&staging).unwrap();
        let marker = directory.path().join("handoff.pid");
        let result = directory.path().join("restart-args");
        let restarted = directory.path().join("fixture app's.sh");
        file(
            &restarted,
            b"#!/bin/sh\nprintf '%s\\n' \"$@\" >\"$FIXTURE_RESULT\"\n",
        );
        let args = ["--agent-mode", "one two", "quote's", "$(literal)"];
        let mut restart_line = format!("/bin/sh {}", shell_literal(restarted.as_os_str()).unwrap());
        for arg in args {
            restart_line.push(' ');
            restart_line.push_str(&shell_literal(std::ffi::OsStr::new(arg)).unwrap());
        }
        let script = staging.join("worker.sh");
        let content = linux_supervisor_script(
            u32::MAX,
            "/bin/sh -c 'exit 0'",
            &restart_line,
            &shell_literal(staging.join("log").as_os_str()).unwrap(),
            &shell_literal(staging.as_os_str()).unwrap(),
            &shell_literal(marker.as_os_str()).unwrap(),
            "true",
        );
        file(&script, content.as_bytes());
        let status = Command::new("/bin/sh")
            .arg(script)
            .env("FIXTURE_RESULT", &result)
            .status()
            .unwrap();
        assert!(status.success());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !result.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            fs::read_to_string(result).unwrap(),
            format!("{}\n", args.join("\n"))
        );
        assert!(!marker.exists());
        assert!(!staging.exists());
    }

    #[cfg(unix)]
    #[test]
    fn installer_supervisor_rejects_a_failed_last_minute_digest_check() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("stage");
        fs::create_dir(&staging).unwrap();
        let marker = directory.path().join("gate");
        let touched = directory.path().join("installed");
        let log = staging.join("log");
        let installer = format!("touch {}", shell_literal(touched.as_os_str()).unwrap());
        let content = linux_supervisor_script(
            u32::MAX,
            &installer,
            "true",
            &shell_literal(log.as_os_str()).unwrap(),
            &shell_literal(staging.as_os_str()).unwrap(),
            &shell_literal(marker.as_os_str()).unwrap(),
            "false",
        );
        let script = staging.join("worker");
        file(&script, content.as_bytes());
        Command::new("/bin/sh").arg(script).status().unwrap();
        assert!(!touched.exists());
        assert!(!marker.exists());
        assert!(
            fs::read_to_string(log)
                .unwrap()
                .contains("SHA-256 verification failed")
        );
        assert!(staging.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_bundle_links_outside_the_application_tree() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("bundle");
        fs::create_dir(&root).unwrap();
        file(&directory.path().join("outside"), b"secret");
        std::os::unix::fs::symlink("../outside", root.join("escape")).unwrap();
        assert!(ensure_tree_contained(&root).is_err());
    }
}
