//! OS advisory locks coordinate work admission across desktop and MCP processes.
use fs2::FileExt;
use std::{
    fs::{File, OpenOptions},
    path::Path,
    sync::Mutex,
};
static ADMISSION: Mutex<()> = Mutex::new(());
pub struct WorkGuard(File);
pub struct PendingInstall {
    intent: File,
}
pub struct InstallGuard {
    work: File,
    _intent: PendingInstall,
}
fn coordination_root() -> Result<std::path::PathBuf, String> {
    use sha2::{Digest, Sha256};
    let executable = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| e.to_string())?;
    let identity = Sha256::digest(executable.as_os_str().as_encoded_bytes());
    let cache = directories::ProjectDirs::from("ai", "inxm", "inxm-local")
        .ok_or_else(|| "Could not locate updater coordination directory".to_owned())?;
    let data_dir = cache.cache_dir().join(format!("update-{identity:x}"));
    std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
    Ok(data_dir)
}
pub fn handoff_marker() -> Result<std::path::PathBuf, String> {
    Ok(coordination_root()?.join("installer.pid"))
}
fn lock_file(_data_dir: &Path, name: &str) -> Result<File, String> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(coordination_root()?.join(name))
        .map_err(|e| e.to_string())
}
fn check_handoff() -> Result<(), String> {
    let marker = handoff_marker()?;
    let content = match std::fs::read_to_string(&marker) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("Could not check installer handoff: {error}")),
    };
    let pid = content.trim().parse::<u32>().map_err(|_| {
        "Installer handoff marker is invalid; inspect the updater coordination directory."
            .to_owned()
    })?;
    if pid == 0 || pid > i32::MAX as u32 {
        return Err("Installer handoff marker contains an invalid process identifier.".to_owned());
    }
    if process_alive(pid) {
        return Err(
            "The update installer is running. New work is paused until it finishes.".to_owned(),
        );
    }
    std::fs::remove_file(marker).map_err(|e| e.to_string())
}
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    // A successful signal-zero probe or EPERM both indicate an existing PID.
    unsafe { kill(pid as i32, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(1) }
}
// Win32 error/status codes are stable across supported Windows versions.
#[cfg(any(windows, test))]
const WINDOWS_NO_PROCESS_ERROR: u32 = 87; // ERROR_INVALID_PARAMETER
#[cfg(any(windows, test))]
const WINDOWS_STILL_ACTIVE: u32 = 259;
#[cfg(any(windows, test))]
fn windows_process_probe_is_alive(open_error: Option<u32>, exit_status: Option<u32>) -> bool {
    match open_error {
        Some(error) => error != WINDOWS_NO_PROCESS_ERROR,
        None => exit_status.is_none_or(|status| status == WINDOWS_STILL_ACTIVE),
    }
}
#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError},
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return windows_process_probe_is_alive(Some(GetLastError()), None);
        }
        let mut status = 0;
        let success = GetExitCodeProcess(process, &mut status) != 0;
        CloseHandle(process);
        windows_process_probe_is_alive(None, success.then_some(status))
    }
}
pub fn admit(data_dir: &Path) -> Result<WorkGuard, String> {
    let _admission = ADMISSION
        .lock()
        .map_err(|_| "Work admission unavailable".to_owned())?;
    check_handoff()?;
    let intent = lock_file(data_dir, "update-intent.lock")?;
    intent
        .try_lock_shared()
        .map_err(|_| "Update installation is pending. New work is paused.".to_owned())?;
    let work = lock_file(data_dir, "update-work.lock")?;
    work.try_lock_shared()
        .map_err(|_| "Update installation is in progress. New work is paused.".to_owned())?;
    let _ = FileExt::unlock(&intent);
    Ok(WorkGuard(work))
}
pub fn begin(data_dir: &Path) -> Result<PendingInstall, String> {
    let _admission = ADMISSION
        .lock()
        .map_err(|_| "Work admission unavailable".to_owned())?;
    check_handoff()?;
    let intent = lock_file(data_dir, "update-intent.lock")?;
    intent
        .try_lock_exclusive()
        .map_err(|_| "Another process is preparing an update.".to_owned())?;
    Ok(PendingInstall { intent })
}
impl PendingInstall {
    pub fn quiescent(&self, data_dir: &Path) -> Result<File, String> {
        let file = lock_file(data_dir, "update-work.lock")?;
        file.try_lock_exclusive().map_err(|_| "Active work remains. New work is paused; retry when it finishes or cancel the update.".to_owned())?;
        Ok(file)
    }
    pub fn acquire(self, work: File) -> InstallGuard {
        InstallGuard {
            work,
            _intent: self,
        }
    }
}
impl Drop for WorkGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}
impl Drop for PendingInstall {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.intent);
    }
}
impl Drop for InstallGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.work);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_liveness_failures_keep_installation_blocked() {
        assert!(!windows_process_probe_is_alive(
            Some(WINDOWS_NO_PROCESS_ERROR),
            None
        ));
        assert!(windows_process_probe_is_alive(Some(5), None)); // ERROR_ACCESS_DENIED
        assert!(windows_process_probe_is_alive(Some(1), None)); // unexpected API failure
        assert!(windows_process_probe_is_alive(None, None)); // GetExitCodeProcess failed
        assert!(windows_process_probe_is_alive(
            None,
            Some(WINDOWS_STILL_ACTIVE)
        ));
        assert!(!windows_process_probe_is_alive(None, Some(0)));
    }
    #[test]
    fn pending_install_defers_for_active_work_and_blocks_new_work() {
        let tmp = tempfile::tempdir().unwrap();
        let work = admit(tmp.path()).unwrap();
        let pending = begin(tmp.path()).unwrap();
        assert!(pending.quiescent(tmp.path()).is_err());
        assert!(admit(tmp.path()).is_err());
        let different_data = tempfile::tempdir().unwrap();
        assert!(admit(different_data.path()).is_err());
        drop(work);
        let exclusive = pending.quiescent(tmp.path()).unwrap();
        let installation = pending.acquire(exclusive);
        assert!(admit(tmp.path()).is_err());
        drop(installation);
        assert!(admit(tmp.path()).is_ok());
        let marker = handoff_marker().unwrap();
        std::fs::write(&marker, std::process::id().to_string()).unwrap();
        assert!(admit(different_data.path()).is_err());
        std::fs::remove_file(marker).unwrap();
        assert!(admit(different_data.path()).is_ok());
    }
}
