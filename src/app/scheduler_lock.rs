//! Atomic, cross-platform single-writer guard for the cron scheduler.
//!
//! Ownership is claimed with `create_new`, so concurrent starters cannot both
//! win. A heartbeat-backed lease makes crash leftovers reclaimable without
//! platform-specific PID APIs.

use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

const LOCK_LEASE: std::time::Duration = std::time::Duration::from_secs(30);
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const ACQUIRE_ATTEMPTS: usize = 8;

#[derive(Debug)]
pub enum LockAcquisition {
    Acquired(SchedulerLock),
    Held { holder_pid: Option<u32> },
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct LockRecord {
    token: String,
    pid: u32,
    heartbeat_ms: u64,
}

#[derive(Debug)]
pub struct SchedulerLock {
    path: PathBuf,
    token: String,
    stop: Arc<AtomicBool>,
    wake: Option<mpsc::Sender<()>>,
    heartbeat: Option<std::thread::JoinHandle<()>>,
}

impl SchedulerLock {
    pub fn acquire(path: &Path) -> std::io::Result<LockAcquisition> {
        Self::acquire_with_timing(path, LOCK_LEASE, HEARTBEAT_INTERVAL)
    }

    fn acquire_with_timing(
        path: &Path,
        lease: std::time::Duration,
        heartbeat_interval: std::time::Duration,
    ) -> std::io::Result<LockAcquisition> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        for _ in 0..ACQUIRE_ATTEMPTS {
            match create_lock(path, heartbeat_interval) {
                Ok(lock) => return Ok(LockAcquisition::Acquired(lock)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let record = read_record(path);
                    if record
                        .as_ref()
                        .is_some_and(|record| record_is_fresh(record, lease))
                        || record.is_none() && file_is_fresh(path, lease)
                    {
                        return Ok(LockAcquisition::Held {
                            holder_pid: record.map(|record| record.pid),
                        });
                    }
                    let stale_path = path.with_extension(format!("stale.{}", uuid::Uuid::new_v4()));
                    match std::fs::rename(path, &stale_path) {
                        Ok(()) => {
                            // A suspended old owner may retain this quarantined
                            // file. It cannot affect the canonical lock path.
                            let _ = std::fs::remove_file(stale_path);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "scheduler lock changed repeatedly during acquisition",
        ))
    }
}

fn create_lock(
    path: &Path,
    heartbeat_interval: std::time::Duration,
) -> std::io::Result<SchedulerLock> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    let token = uuid::Uuid::new_v4().to_string();
    let pid = std::process::id();
    write_record(
        &mut file,
        &LockRecord {
            token: token.clone(),
            pid,
            heartbeat_ms: epoch_millis(),
        },
    )?;
    let mut heartbeat_file = file.try_clone()?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread_token = token.clone();
    let (wake, wait) = mpsc::channel();
    let heartbeat = std::thread::Builder::new()
        .name("inxm-scheduler-heartbeat".to_owned())
        .spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                if wait.recv_timeout(heartbeat_interval).is_ok() {
                    break;
                }
                if write_record(
                    &mut heartbeat_file,
                    &LockRecord {
                        token: thread_token.clone(),
                        pid,
                        heartbeat_ms: epoch_millis(),
                    },
                )
                .is_err()
                {
                    break;
                }
            }
        })?;
    Ok(SchedulerLock {
        path: path.to_owned(),
        token,
        stop,
        wake: Some(wake),
        heartbeat: Some(heartbeat),
    })
}

fn write_record(file: &mut std::fs::File, record: &LockRecord) -> std::io::Result<()> {
    let json = serde_json::to_vec(record)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    file.rewind()?;
    file.set_len(0)?;
    file.write_all(&json)?;
    file.sync_data()
}

fn read_record(path: &Path) -> Option<LockRecord> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
fn read_pid(path: &Path) -> Option<u32> {
    read_record(path).map(|record| record.pid)
}

fn record_is_fresh(record: &LockRecord, lease: std::time::Duration) -> bool {
    epoch_millis().saturating_sub(record.heartbeat_ms) <= lease.as_millis() as u64
}

fn file_is_fresh(path: &Path, lease: std::time::Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age <= lease)
}

fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl Drop for SchedulerLock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(wake) = self.wake.take() {
            let _ = wake.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
        if read_record(&self.path).is_some_and(|record| record.token == self.token) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simultaneous_acquisition_has_exactly_one_winner() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Arc::new(tmp.path().join("scheduler.lock"));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let handles = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    SchedulerLock::acquire(&path).unwrap()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, LockAcquisition::Acquired(_)))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, LockAcquisition::Held { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn stale_crash_record_is_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("scheduler.lock");
        let stale = LockRecord {
            token: "crashed".to_owned(),
            pid: 42,
            heartbeat_ms: 0,
        };
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();

        let acquired = SchedulerLock::acquire_with_timing(
            &path,
            std::time::Duration::from_millis(1),
            std::time::Duration::from_secs(1),
        )
        .unwrap();
        assert!(matches!(acquired, LockAcquisition::Acquired(_)));
        assert_eq!(read_pid(&path), Some(std::process::id()));
    }

    #[test]
    fn live_owner_blocks_and_drop_releases() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("scheduler.lock");
        let first = SchedulerLock::acquire(&path).unwrap();
        assert!(matches!(
            SchedulerLock::acquire(&path).unwrap(),
            LockAcquisition::Held {
                holder_pid: Some(pid)
            } if pid == std::process::id()
        ));
        drop(first);
        assert!(!path.exists());
        assert!(matches!(
            SchedulerLock::acquire(&path).unwrap(),
            LockAcquisition::Acquired(_)
        ));
    }

    #[test]
    fn stale_owner_drop_does_not_remove_successor() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("scheduler.lock");
        let first = match SchedulerLock::acquire(&path).unwrap() {
            LockAcquisition::Acquired(lock) => lock,
            LockAcquisition::Held { .. } => panic!("expected first owner"),
        };
        let successor = LockRecord {
            token: "successor".to_owned(),
            pid: 99,
            heartbeat_ms: epoch_millis(),
        };
        let quarantined = path.with_extension("old");
        std::fs::rename(&path, &quarantined).unwrap();
        std::fs::write(&path, serde_json::to_vec(&successor).unwrap()).unwrap();
        drop(first);
        assert_eq!(read_pid(&path), Some(99));
    }
}
