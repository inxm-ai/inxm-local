//! Single-instance guard for the desktop app.
//!
//! Launching `INXM // Local` a second time (double-click, "start
//! on login" plus a manual launch, etc.) used to spawn a whole new process
//! and window rather than surfacing the one already running. There was no
//! guard at all — only the cron scheduler had a single-writer lock
//! ([`super::scheduler_lock`]), and that only protects the schedule loop, not
//! the UI.
//!
//! The fix is a per-data-dir local socket (a named pipe on Windows, a Unix
//! domain socket elsewhere). The first process to start binds it and becomes
//! [`InstanceGuard::Primary`]; every later process observes the bind fail
//! with `AddrInUse`, connects instead, sends it a `show\n` request, and
//! reports back as [`InstanceGuard::Secondary`] so its caller can exit
//! immediately rather than opening a second window.
//!
//! The socket name is derived from the data dir so that two instances
//! pointed at different `$INXM_LOCAL_DATA_DIR` values (as the test suite
//! does) never contend with each other.
//!
//! ## Why Windows needs a different primary/secondary decision
//!
//! The scheme above relies on the *bind* of the socket failing with
//! `AddrInUse` when a second process tries to claim a name that is already
//! held. That is true for Unix domain sockets and for Linux's abstract
//! socket namespace, but it is **not** true for Windows named pipes:
//! `CreateNamedPipe` only rejects a second server on the same pipe name when
//! the *first* instance was created with `FIRST_PIPE_INSTANCE`, and the
//! `interprocess` crate does not set that flag (its own docs say
//! `AddrInUse` "goes unhandled" for named pipes on Windows). Left alone, a
//! second launch would happily bind its own pipe server of the same name
//! and become a second primary, which is exactly the duplicate-window bug
//! this module exists to prevent.
//!
//! So on Windows, instance detection is done with a named
//! [`CreateMutexW`](https://learn.microsoft.com/windows/win32/api/synchapi/nf-synchapi-createmutexw)
//! instead, which *is* guaranteed by the OS to be exclusive: only the first
//! caller for a given name gets a "new" result, every later caller for the
//! same name gets `ERROR_ALREADY_EXISTS`. The named pipe is still created by
//! the primary and is still what secondaries connect to — it just no longer
//! decides who is primary, it is purely the `show\n` transport. The mutex
//! handle is kept open for the lifetime of the primary process (closing it
//! would let a later launch also become primary); the OS releases it
//! automatically on process exit, including a crash, so there is no stale
//! state to clean up on Windows the way there is for the Unix socket-file
//! fallback.
//!
//! Because the primary creates its mutex before it creates its pipe
//! listener, a secondary that wins the mutex race can briefly find no pipe
//! to connect to; `acquire` retries the connect a few times before treating
//! that as "no one is listening yet, but someone is definitely primary" and
//! exiting anyway rather than double-guessing the mutex and starting a
//! second UI.

use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use interprocess::local_socket::prelude::*;
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
use interprocess::local_socket::{GenericFilePath, ToFsName as _};
use interprocess::local_socket::{GenericNamespaced, ListenerOptions};

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::CreateMutexW;

/// Outcome of trying to claim the single-instance socket for a data dir.
pub enum InstanceGuard {
    /// This process bound the socket first. Holds the listener so it can be
    /// handed to [`PrimaryHandle::spawn_listener`] once the egui context
    /// exists.
    Primary(PrimaryHandle),
    /// Another process already holds the socket and was asked to show its
    /// window. The caller should exit without creating a UI.
    Secondary,
}

/// The listener half of a claimed single-instance socket, not yet listening
/// on a background thread.
pub struct PrimaryHandle {
    listener: LocalSocketListener,
    /// The handle to the named mutex that made this process primary. Only
    /// present on Windows, where the mutex (not the pipe) is what decides
    /// primary vs. secondary — see the module docs. Held for the lifetime of
    /// this struct so the mutex stays alive as long as this process is
    /// primary; dropped (closing the handle) only when the process exits,
    /// at which point the OS also releases it on our behalf regardless.
    #[cfg(windows)]
    _mutex: WindowsMutexHandle,
}

/// A `CreateMutexW` handle, kept alive for the process lifetime so a later
/// launch's `CreateMutexW` call on the same name reliably reports
/// `ERROR_ALREADY_EXISTS`. Closing it early would let a second process also
/// become primary, reintroducing the duplicate-window bug this module
/// exists to fix.
///
/// `HANDLE` is a raw pointer and so is not `Send` by default, but a Windows
/// object handle is process-wide (not thread-affine) and safe to close from
/// any thread, so it is sound to move this across threads and to drop it
/// from whichever thread happens to hold the last owner.
#[cfg(windows)]
struct WindowsMutexHandle(HANDLE);

#[cfg(windows)]
unsafe impl Send for WindowsMutexHandle {}

#[cfg(windows)]
impl Drop for WindowsMutexHandle {
    fn drop(&mut self) {
        // Best-effort: nothing useful can be done if this fails, and the OS
        // releases the mutex on process exit regardless.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

impl PrimaryHandle {
    /// Spawns a background thread that accepts connections and invokes
    /// `on_show` whenever a later instance asks to be shown. Errors on
    /// individual connections are logged and otherwise ignored — a
    /// misbehaving second instance must never take down the primary one.
    pub fn spawn_listener(self, on_show: impl Fn() + Send + 'static) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("inxm-single-instance".to_owned())
            .spawn(move || {
                for connection in self.listener {
                    match connection {
                        Ok(stream) => {
                            if let Err(error) = handle_connection(stream, &on_show) {
                                tracing::debug!(
                                    operation = "single_instance.handle_connection",
                                    app_version = env!("CARGO_PKG_VERSION"),
                                    triggered_by = "peer_process",
                                    outcome = "failure",
                                    error = %error,
                                    "single-instance connection failed"
                                );
                            }
                        }
                        Err(error) => {
                            tracing::debug!(
                                operation = "single_instance.accept",
                                app_version = env!("CARGO_PKG_VERSION"),
                                triggered_by = "peer_process",
                                outcome = "failure",
                                error = %error,
                                "single-instance accept failed"
                            );
                        }
                    }
                }
            })
            .expect("spawn inxm-single-instance thread")
    }
}

fn handle_connection(
    stream: LocalSocketStream,
    on_show: &(impl Fn() + Send + 'static),
) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim() == "show" {
        on_show();
    }
    Ok(())
}

impl InstanceGuard {
    /// Claims (or contacts) the single-instance socket for `data_dir`.
    ///
    /// Returns [`InstanceGuard::Secondary`] once a `show` request has been
    /// sent to the existing primary. A best-effort attempt is made to
    /// reclaim a stale socket (left behind by a crash, filesystem-backed
    /// sockets only) before giving up.
    pub fn acquire(data_dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(data_dir)
            .map_err(|error| format!("create data dir for single-instance socket: {error}"))?;
        let short_name = socket_short_name(data_dir);

        #[cfg(windows)]
        {
            return Self::acquire_windows(&short_name);
        }

        #[cfg(not(windows))]
        {
            Self::acquire_unix(&short_name)
        }
    }

    /// Windows instance detection: see the module docs for why this can't
    /// be done with the pipe's bind result the way it is on Unix. The pipe
    /// is still created below and is still what secondaries connect to —
    /// it is just no longer what decides who is primary.
    #[cfg(windows)]
    fn acquire_windows(short_name: &str) -> Result<Self, String> {
        let mutex_name = to_wide_null(&format!(r"Local\{short_name}"));
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr()) };
        if handle.is_null() {
            let last_error = unsafe { GetLastError() };
            return Err(format!("create single-instance mutex: {last_error}"));
        }

        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // Another process already owns the mutex and is (or is about
            // to be) primary. We don't need our own handle to it.
            unsafe {
                CloseHandle(handle);
            }

            let (name, _) =
                build_name(short_name).map_err(|error| format!("build socket name: {error}"))?;

            // The primary creates its mutex before its pipe listener, so
            // there is a small window right after losing the mutex race
            // where the pipe doesn't exist yet. Retry briefly rather than
            // giving up immediately.
            const CONNECT_ATTEMPTS: u32 = 5;
            const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(200);
            for attempt in 0..CONNECT_ATTEMPTS {
                match LocalSocketStream::connect(name.borrow()) {
                    Ok(mut stream) => {
                        // Best-effort: if the primary is mid-shutdown and
                        // the write fails, there is nothing more useful
                        // this process can do than exit anyway.
                        let _ = stream.write_all(b"show\n");
                        return Ok(InstanceGuard::Secondary);
                    }
                    Err(_) if attempt + 1 < CONNECT_ATTEMPTS => {
                        std::thread::sleep(RETRY_DELAY);
                    }
                    Err(_) => {
                        // A live primary owns the mutex, so exiting without
                        // ever showing its window still beats opening a
                        // duplicate one.
                        return Ok(InstanceGuard::Secondary);
                    }
                }
            }
            unreachable!("loop above always returns")
        } else {
            // We created the mutex: we are primary. Create the pipe
            // listener now; a failure here is still ours to report, but we
            // remain logically primary (we hold the mutex), so the caller's
            // existing degrade-with-warning path is the right response, not
            // falling back to Secondary.
            let (name, _) =
                build_name(short_name).map_err(|error| format!("build socket name: {error}"))?;
            ListenerOptions::new()
                .name(name.borrow())
                .create_sync()
                .map(|listener| {
                    InstanceGuard::Primary(PrimaryHandle {
                        listener,
                        _mutex: WindowsMutexHandle(handle),
                    })
                })
                .map_err(|error| format!("bind single-instance socket: {error}"))
        }
    }

    #[cfg(not(windows))]
    fn acquire_unix(short_name: &str) -> Result<Self, String> {
        let (name, stale_path) =
            build_name(short_name).map_err(|error| format!("build socket name: {error}"))?;

        match ListenerOptions::new().name(name.borrow()).create_sync() {
            Ok(listener) => Ok(InstanceGuard::Primary(PrimaryHandle { listener })),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                match LocalSocketStream::connect(name.borrow()) {
                    Ok(mut stream) => {
                        // Best-effort: if the existing primary is mid-shutdown
                        // and the write fails, there is nothing more useful
                        // this process can do than exit anyway.
                        let _ = stream.write_all(b"show\n");
                        Ok(InstanceGuard::Secondary)
                    }
                    Err(_connect_error) => {
                        // The previous owner crashed without cleaning up its
                        // socket file. Only filesystem-backed sockets (the
                        // non-Linux, non-Windows fallback) can go stale like
                        // this — namespaced sockets are released by the OS
                        // when the owning process exits.
                        if let Some(path) = &stale_path {
                            let _ = std::fs::remove_file(path);
                        }
                        ListenerOptions::new()
                            .name(name.borrow())
                            .create_sync()
                            .map(|listener| InstanceGuard::Primary(PrimaryHandle { listener }))
                            .map_err(|error| {
                                format!(
                                    "bind single-instance socket after stale-socket cleanup: {error}"
                                )
                            })
                    }
                }
            }
            Err(error) => Err(format!("bind single-instance socket: {error}")),
        }
    }
}

/// Encodes `s` as a null-terminated UTF-16 string, the form Windows'
/// `*W` APIs (like `CreateMutexW`) expect for their string arguments.
#[cfg(windows)]
fn to_wide_null(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// A short, stable name derived from the data dir so unrelated data dirs
/// (e.g. separate `$INXM_LOCAL_DATA_DIR` values used by tests) never share a
/// socket. `DefaultHasher` is not cryptographically stable across Rust
/// versions, but it only needs to be stable for the lifetime of one process
/// pair racing to open the same data dir, which it is.
fn socket_short_name(data_dir: &Path) -> String {
    let canonical = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("inxm-local-{:016x}", hasher.finish())
}

/// Windows named pipes and Linux's abstract socket namespace are both
/// addressed through [`GenericNamespaced`] and are cleaned up by the OS when
/// the owning process exits, so there is no stale-file case to handle for
/// them. Other Unix platforms (macOS, BSD) fall back to a real socket file
/// under a short base dir — `XDG_RUNTIME_DIR` if set, else the system temp
/// dir — to stay well under macOS's ~104-byte `sockaddr_un` path limit, and
/// so `acquire` can find and remove it if it goes stale.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn build_name(
    short_name: &str,
) -> io::Result<(interprocess::local_socket::Name<'static>, Option<PathBuf>)> {
    Ok((
        short_name.to_ns_name::<GenericNamespaced>()?.into_owned(),
        None,
    ))
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn build_name(
    short_name: &str,
) -> io::Result<(interprocess::local_socket::Name<'static>, Option<PathBuf>)> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = base.join(format!("{short_name}.sock"));
    let name = path.clone().to_fs_name::<GenericFilePath>()?.into_owned();
    Ok((name, Some(path)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A fresh, never-before-used data dir per test so parallel test runs
    /// (and repeated runs against the same temp dir) never contend over the
    /// same socket name.
    fn unique_data_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "inxm-single-instance-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            n
        ))
    }

    #[test]
    fn first_acquire_is_primary() {
        let dir = unique_data_dir();
        match InstanceGuard::acquire(&dir).expect("acquire") {
            InstanceGuard::Primary(_) => {}
            InstanceGuard::Secondary => panic!("expected the first acquire to be primary"),
        }
    }

    #[test]
    fn second_acquire_while_first_is_held_is_secondary() {
        let dir = unique_data_dir();
        let first = InstanceGuard::acquire(&dir).expect("first acquire");
        let _first = match first {
            InstanceGuard::Primary(primary) => primary,
            InstanceGuard::Secondary => panic!("expected the first acquire to be primary"),
        };

        match InstanceGuard::acquire(&dir).expect("second acquire") {
            InstanceGuard::Secondary => {}
            InstanceGuard::Primary(_) => {
                panic!("expected the second acquire to observe the first as primary")
            }
        };
    }

    #[test]
    fn show_request_reaches_the_listener_callback() {
        let dir = unique_data_dir();
        let primary = match InstanceGuard::acquire(&dir).expect("first acquire") {
            InstanceGuard::Primary(primary) => primary,
            InstanceGuard::Secondary => panic!("expected the first acquire to be primary"),
        };

        let (shown_tx, shown_rx) = mpsc::channel();
        let _thread = primary.spawn_listener(move || {
            let _ = shown_tx.send(());
        });

        match InstanceGuard::acquire(&dir).expect("second acquire") {
            InstanceGuard::Secondary => {}
            InstanceGuard::Primary(_) => {
                panic!("expected the second acquire to observe the first as primary")
            }
        };

        shown_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("listener callback fires after a show request");
    }
}
