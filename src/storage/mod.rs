//! File-based local storage for plans, runs, and patches.
//!
//! Storage layout under the data root (default: `.inxm/` in the working directory):
//!
//! ```text
//! .inxm/
//!   plans/
//!     {plan-id}/
//!       v1.json        — version snapshot
//!       v2.json
//!       current.json   — copy of the latest version
//!   runs/
//!     {run-id}.json    — full run state
//!   patches/
//!     {patch-id}.json  — patch proposal + approval status
//! ```

pub mod patches;
pub mod plan_edits;
pub mod plans;
pub mod runs;
pub mod world_fixes;

use crate::error::StorageError;
use crate::plan::types::Plan;
use crate::storage::patches::PatchStatus;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Subdirectory names under the storage root. These are a persistence
/// contract — changing them orphans existing user data.
const PLANS_SUBDIR: &str = "plans";
const RUNS_SUBDIR: &str = "runs";
const PATCHES_SUBDIR: &str = "patches";
const PLAN_EDITS_SUBDIR: &str = "plan_edits";
const REPAIR_TRANSACTIONS_SUBDIR: &str = "repair-transactions";
const WORLD_FIXES_SUBDIR: &str = "world-fixes";

/// Suffix appended to a unique sibling file while new content is being written.
/// The finished file is renamed over the target, so readers only ever see a
/// complete document. Stray `*.tmp` files are leftovers from a crash and are
/// ignored by every store's directory listing (they don't end in `.json`).
const TEMP_FILE_SUFFIX: &str = ".tmp";

/// Reject IDs that could resolve anywhere except one direct child of a store.
///
/// Checking both slash styles keeps the persistence boundary safe when data or
/// requests move between Unix and Windows hosts.
pub(crate) fn validate_record_id(id: &str) -> Result<(), StorageError> {
    let mut components = Path::new(id).components();
    let is_single_normal_component =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if id.is_empty() || id.contains('/') || id.contains('\\') || !is_single_normal_component {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid storage record id: {id:?}"),
        )
        .into());
    }
    Ok(())
}

/// Write `content` to `path` atomically: write a sibling temp file first,
/// then rename it over the target. A crash mid-write leaves the previous
/// file intact instead of a truncated one.
pub(crate) fn write_atomically(path: &Path, content: &str) -> Result<(), StorageError> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp_path = path.with_file_name(format!(
        "{file_name}.{}{TEMP_FILE_SUFFIX}",
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<(), std::io::Error> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        replace_file_with_retry(&temp_path, path)?;
        sync_published_file(path)?;
        sync_parent_directory(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result.map_err(StorageError::from)
}

pub(crate) enum ExclusiveWrite {
    Created,
    AlreadyExists,
}

/// Publish fully written immutable content under `path` only if it is absent.
///
/// The hard-link operation is the CAS claim: the destination either names the
/// already-synced temp inode in full or remains unchanged.
pub(crate) fn write_exclusively(
    path: &Path,
    content: &str,
) -> Result<ExclusiveWrite, StorageError> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp_path = path.with_file_name(format!(
        "{file_name}.{}{TEMP_FILE_SUFFIX}",
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<ExclusiveWrite, std::io::Error> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        match std::fs::hard_link(&temp_path, path) {
            Ok(()) => {
                sync_published_file(path)?;
                sync_parent_directory(path)?;
                Ok(ExclusiveWrite::Created)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Ok(ExclusiveWrite::AlreadyExists)
            }
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
                // Filesystems without link(2) — notably gcsfuse behind Cloud
                // Run GCS volume mounts — cannot run the hard-link CAS. Fall
                // back to claiming the destination directly with `create_new`,
                // which is still an atomic exists-check-and-create; the file
                // is fully written and synced before the claim is observable
                // there because gcsfuse publishes objects on close.
                write_exclusively_direct(path, content)
            }
            Err(error) => Err(error),
        }
    })();
    let _ = std::fs::remove_file(&temp_path);
    result.map_err(StorageError::from)
}

/// `write_exclusively` fallback for filesystems without hard links: create
/// the destination itself with `create_new` and write the content in place.
fn write_exclusively_direct(path: &Path, content: &str) -> Result<ExclusiveWrite, std::io::Error> {
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Ok(ExclusiveWrite::AlreadyExists);
        }
        Err(error) => return Err(error),
    };
    let result = (|| {
        file.write_all(content.as_bytes())?;
        file.sync_all()
    })();
    drop(file);
    if let Err(error) = result {
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    sync_parent_directory(path)?;
    Ok(ExclusiveWrite::Created)
}

fn sync_published_file(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(windows)]
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    #[cfg(not(windows))]
    let file = std::fs::File::open(path)?;
    file.sync_all()
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    unsafe extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    fn wide_path(path: &Path) -> Result<Vec<u16>, std::io::Error> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "storage path contains an embedded NUL",
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    let source = wide_path(source)?;
    let destination = wide_path(destination)?;
    // SAFETY: both pointers reference NUL-terminated UTF-16 buffers that live
    // through the call. Flags request documented replace + write-through
    // semantics, and the return value is checked before exposing success.
    let replaced = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `replace_file`, retried on Windows for the transient sharing violation a
/// concurrent reader of `destination` causes.
///
/// `MoveFileExW(..., MOVEFILE_REPLACE_EXISTING)` fails with
/// `ERROR_ACCESS_DENIED` (5) or `ERROR_SHARING_VIOLATION` (32) if any other
/// handle has the destination open without `FILE_SHARE_DELETE` — which is
/// what `std::fs` opens use by default. The background run-list poll and any
/// open run-inspector both briefly `read_to_string`/`open` the same run file
/// this store is about to replace, so on a busy run (e.g. right after
/// aborting one) this collision is routine rather than exceptional. Those
/// readers close their handle in microseconds, so a handful of short retries
/// clears it without surfacing a hard failure — and without it, a run whose
/// terminal status fails to persist here is stuck showing "Running" forever.
#[cfg(windows)]
fn replace_file_with_retry(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    const MAX_ATTEMPTS: u32 = 10;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(5);
    let mut last_error = None;
    for attempt in 0..MAX_ATTEMPTS {
        match replace_file(source, destination) {
            Ok(()) => return Ok(()),
            Err(error) if matches!(error.raw_os_error(), Some(5) | Some(32)) => {
                last_error = Some(error);
                if attempt + 1 < MAX_ATTEMPTS {
                    std::thread::sleep(RETRY_DELAY);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.expect("loop only exits via an Ok return or after recording an error"))
}

#[cfg(not(windows))]
fn replace_file_with_retry(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    replace_file(source, destination)
}

pub(crate) fn sync_parent_directory(_path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    if let Some(parent) = _path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn consistency_error(message: impl Into<String>) -> StorageError {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into()).into()
}

/// Move an unreadable or malformed repair journal out of the way so it stops
/// blocking every future `StorageRoot::open` and log why. Best-effort: if the
/// rename itself fails there is nothing safer left to do than leave the file
/// in place and move on to the next journal.
fn quarantine_unreadable_journal(
    path: &Path,
    journal_name: &str,
    reason: &'static str,
    error: &dyn std::fmt::Display,
) {
    let quarantined = path.with_extension("json.corrupt");
    tracing::warn!(
        "storage.event" = "repair_journal_recovery",
        "storage.recovery.outcome" = "quarantined",
        "storage.entry.name" = %journal_name,
        "storage.recovery.reason" = reason,
        "storage.recovery.error" = %error,
        "incomplete repair commit recovery skipped a bad journal"
    );
    if let Err(rename_error) = std::fs::rename(path, &quarantined) {
        tracing::warn!(
            "storage.event" = "repair_journal_recovery",
            "storage.recovery.outcome" = "quarantine_failed",
            "storage.entry.name" = %journal_name,
            "storage.recovery.error" = %rename_error,
            "could not quarantine a bad repair journal; leaving it in place"
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RepairTransaction {
    patch_id: String,
    plan_id: String,
    base_version: u32,
    next_plan: Plan,
}

/// Root handle for all local state.
#[derive(Debug, Clone)]
pub struct StorageRoot {
    root: PathBuf,
    repair_commit_lock: Arc<Mutex<()>>,
}

impl StorageRoot {
    /// Open (or create) a storage root at the given path.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StorageError> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(root.join(PLANS_SUBDIR))?;
        std::fs::create_dir_all(root.join(RUNS_SUBDIR))?;
        std::fs::create_dir_all(root.join(PATCHES_SUBDIR))?;
        std::fs::create_dir_all(root.join(PLAN_EDITS_SUBDIR))?;
        std::fs::create_dir_all(root.join(REPAIR_TRANSACTIONS_SUBDIR))?;
        std::fs::create_dir_all(root.join(WORLD_FIXES_SUBDIR))?;
        let storage = Self {
            root,
            repair_commit_lock: Arc::new(Mutex::new(())),
        };
        storage.recover_repair_transactions()?;
        Ok(storage)
    }

    pub fn plans_dir(&self) -> PathBuf {
        self.root.join(PLANS_SUBDIR)
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.root.join(RUNS_SUBDIR)
    }

    pub fn patches_dir(&self) -> PathBuf {
        self.root.join(PATCHES_SUBDIR)
    }

    pub fn plan_edits_dir(&self) -> PathBuf {
        self.root.join(PLAN_EDITS_SUBDIR)
    }

    fn repair_transactions_dir(&self) -> PathBuf {
        self.root.join(REPAIR_TRANSACTIONS_SUBDIR)
    }

    pub fn root_path(&self) -> &Path {
        &self.root
    }

    // ── Convenience accessors ─────────────────────────────────────────────────

    pub fn plans(&self) -> plans::PlanStore {
        plans::PlanStore::new(self.plans_dir())
    }

    pub fn runs(&self) -> runs::RunStore {
        runs::RunStore::new(self.runs_dir())
    }

    pub fn patches(&self) -> patches::PatchStore {
        patches::PatchStore::new(self.patches_dir())
    }

    pub fn plan_edits(&self) -> plan_edits::PlanEditStore {
        plan_edits::PlanEditStore::new(self.plan_edits_dir())
    }

    pub fn world_fixes_dir(&self) -> PathBuf {
        self.root.join(WORLD_FIXES_SUBDIR)
    }

    pub fn world_fixes(&self) -> world_fixes::WorldFixStore {
        world_fixes::WorldFixStore::new(self.world_fixes_dir())
    }

    /// Commit the persistence portion of an approved repair.
    ///
    /// The caller remains responsible for validating the patch's domain
    /// semantics. This boundary only verifies identities, versions, approval
    /// state, immutable snapshot ownership, and crash-consistent publication.
    pub fn commit_repair(&self, patch_id: &str, next_plan: &Plan) -> Result<(), StorageError> {
        validate_record_id(patch_id)?;
        validate_record_id(&next_plan.metadata.id)?;
        let commit_lock = Arc::clone(&self.repair_commit_lock);
        let _guard = commit_lock
            .lock()
            .map_err(|_| consistency_error("repair commit lock is poisoned"))?;

        self.recover_repair_transactions_inner()?;
        let patch = self.patches().load(patch_id)?;
        if patch.status == PatchStatus::Applied {
            let current = self.plans().load_current(&patch.plan_id)?;
            if current == *next_plan {
                return Ok(());
            }
            return Err(consistency_error(format!(
                "patch '{patch_id}' is already applied to different plan content"
            )));
        }
        if patch.status != PatchStatus::Approved {
            return Err(consistency_error(format!(
                "patch '{patch_id}' must be approved before commit; found {}",
                patch.status
            )));
        }
        if patch.plan_id != next_plan.metadata.id {
            return Err(consistency_error(format!(
                "repair plan identity mismatch for patch '{patch_id}': expected '{}', found '{}'",
                patch.plan_id, next_plan.metadata.id
            )));
        }
        let expected_next_version = patch.plan_version.checked_add(1).ok_or_else(|| {
            consistency_error(format!("plan version overflow for patch '{patch_id}'"))
        })?;
        if next_plan.metadata.version != expected_next_version {
            return Err(consistency_error(format!(
                "repair version mismatch for patch '{patch_id}': base is v{}, candidate is v{}",
                patch.plan_version, next_plan.metadata.version
            )));
        }
        let current = self.plans().load_current(&patch.plan_id)?;
        if current.metadata.version != patch.plan_version {
            return Err(consistency_error(format!(
                "stale repair patch '{patch_id}': current plan is v{}, patch targets v{}",
                current.metadata.version, patch.plan_version
            )));
        }

        let transaction = RepairTransaction {
            patch_id: patch_id.to_owned(),
            plan_id: patch.plan_id,
            base_version: patch.plan_version,
            next_plan: next_plan.clone(),
        };
        self.plans().claim_snapshot(next_plan)?;
        self.persist_repair_transaction(&transaction)?;
        self.complete_repair_transaction(&transaction)
    }

    fn recover_repair_transactions(&self) -> Result<(), StorageError> {
        let commit_lock = Arc::clone(&self.repair_commit_lock);
        let _guard = commit_lock
            .lock()
            .map_err(|_| consistency_error("repair commit lock is poisoned"))?;
        self.recover_repair_transactions_inner()
    }

    fn recover_repair_transactions_inner(&self) -> Result<(), StorageError> {
        let mut journals = Vec::new();
        for entry in std::fs::read_dir(self.repair_transactions_dir())? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && entry.path().extension().and_then(|value| value.to_str()) == Some("json")
            {
                journals.push(entry.path());
            }
        }
        journals.sort();
        for path in journals {
            let journal_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let raw = match crate::support::Presence::from_io_result(std::fs::read_to_string(&path))
            {
                crate::support::Presence::Found(raw) => raw,
                // Vanished between listing and reading (e.g. a concurrent
                // recovery already cleaned it up) — nothing left to recover.
                crate::support::Presence::Absent => continue,
                crate::support::Presence::Broken(err) => {
                    quarantine_unreadable_journal(&path, &journal_name, "journal_unreadable", &err);
                    continue;
                }
            };
            let transaction: RepairTransaction = match serde_json::from_str(&raw) {
                Ok(transaction) => transaction,
                Err(err) => {
                    quarantine_unreadable_journal(&path, &journal_name, "journal_corrupt", &err);
                    continue;
                }
            };
            tracing::warn!(
                "storage.event" = "repair_journal_recovery",
                "storage.recovery.outcome" = "started",
                "patch.id" = %transaction.patch_id,
                "plan.id" = %transaction.plan_id,
                "plan.version.base" = transaction.base_version,
                "recovering incomplete repair commit"
            );
            if let Err(error) = self.complete_repair_transaction(&transaction) {
                tracing::warn!(
                    "storage.event" = "repair_journal_recovery",
                    "storage.recovery.outcome" = "failed",
                    "patch.id" = %transaction.patch_id,
                    "plan.id" = %transaction.plan_id,
                    "plan.version.base" = transaction.base_version,
                    "incomplete repair commit recovery failed"
                );
                return Err(error);
            }
            tracing::warn!(
                "storage.event" = "repair_journal_recovery",
                "storage.recovery.outcome" = "completed",
                "patch.id" = %transaction.patch_id,
                "plan.id" = %transaction.plan_id,
                "plan.version.base" = transaction.base_version,
                "incomplete repair commit recovered"
            );
        }
        Ok(())
    }

    fn persist_repair_transaction(
        &self,
        transaction: &RepairTransaction,
    ) -> Result<(), StorageError> {
        validate_record_id(&transaction.patch_id)?;
        let path = self
            .repair_transactions_dir()
            .join(format!("{}.json", transaction.patch_id));
        let json = serde_json::to_string_pretty(transaction)?;
        match write_exclusively(&path, &json)? {
            ExclusiveWrite::Created => Ok(()),
            ExclusiveWrite::AlreadyExists => {
                let existing: RepairTransaction =
                    serde_json::from_str(&std::fs::read_to_string(&path)?)?;
                if existing == *transaction {
                    Ok(())
                } else {
                    Err(consistency_error(format!(
                        "conflicting repair journal for patch '{}'",
                        transaction.patch_id
                    )))
                }
            }
        }
    }

    fn complete_repair_transaction(
        &self,
        transaction: &RepairTransaction,
    ) -> Result<(), StorageError> {
        validate_record_id(&transaction.patch_id)?;
        validate_record_id(&transaction.plan_id)?;
        if transaction.next_plan.metadata.id != transaction.plan_id
            || transaction.next_plan.metadata.version != transaction.base_version.saturating_add(1)
        {
            return Err(consistency_error(format!(
                "invalid repair journal for patch '{}'",
                transaction.patch_id
            )));
        }
        self.plans().claim_snapshot(&transaction.next_plan)?;
        self.plans()
            .advance_current(&transaction.next_plan, Some(transaction.base_version))?;

        let mut patch = self.patches().load(&transaction.patch_id)?;
        if patch.plan_id != transaction.plan_id || patch.plan_version != transaction.base_version {
            return Err(consistency_error(format!(
                "persisted patch '{}' does not match its repair journal",
                transaction.patch_id
            )));
        }
        match patch.status {
            PatchStatus::Approved => {
                patch.status = PatchStatus::Applied;
                self.patches().save(&patch)?;
            }
            PatchStatus::Applied => {}
            _ => {
                return Err(consistency_error(format!(
                    "patch '{}' changed to {} while its repair commit was pending",
                    transaction.patch_id, patch.status
                )));
            }
        }

        let journal_path = self
            .repair_transactions_dir()
            .join(format!("{}.json", transaction.patch_id));
        if journal_path.exists() {
            std::fs::remove_file(&journal_path)?;
            sync_parent_directory(&journal_path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{Plan, PlanMetadata};
    use crate::storage::patches::{Patch, PatchOperation};
    use crate::storage::runs::{Run, StepRun};
    use indexmap::IndexMap;

    fn plan_named(name: &str) -> Plan {
        Plan {
            metadata: PlanMetadata::new(None),
            name: name.to_owned(),
            description: None,
            inputs: Vec::new(),
            config: IndexMap::new(),
            steps: Vec::new(),
            outputs: Vec::new(),
        }
    }

    fn approved_patch(plan: &Plan) -> Patch {
        let mut patch = Patch::new(
            &plan.metadata.id,
            plan.metadata.version,
            "run-1",
            "step-1",
            PatchOperation::RemovePlanField {
                pointer: "/config/obsolete".to_owned(),
            },
            "test repair",
        );
        patch.status = PatchStatus::Approved;
        patch
    }

    fn next_plan(plan: &Plan, name: &str) -> Plan {
        let mut next = plan.clone();
        next.metadata = plan.metadata.next_version();
        next.name = name.to_owned();
        next
    }

    fn transaction_for(patch: &Patch, next_plan: &Plan) -> RepairTransaction {
        RepairTransaction {
            patch_id: patch.id.clone(),
            plan_id: patch.plan_id.clone(),
            base_version: patch.plan_version,
            next_plan: next_plan.clone(),
        }
    }

    #[test]
    fn write_atomically_creates_and_replaces_without_leaving_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("doc.json");

        write_atomically(&target, "first").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first");

        write_atomically(&target, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second");

        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["doc.json".to_owned()]);
    }

    #[test]
    fn stray_temp_files_from_a_crash_are_invisible_to_listings() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(tmp.path()).unwrap();

        // Simulate a crash that left temp files behind in each store.
        let plan_dir = storage.plans_dir().join("wounded");
        std::fs::create_dir_all(&plan_dir).unwrap();
        std::fs::write(plan_dir.join("current.json.tmp"), "{ partial").unwrap();
        std::fs::write(plan_dir.join("v1.json.tmp"), "{ partial").unwrap();
        std::fs::write(storage.runs_dir().join("r1.json.tmp"), "{ partial").unwrap();
        std::fs::write(storage.patches_dir().join("p1.json.tmp"), "{ partial").unwrap();

        assert!(storage.plans().list().unwrap().is_empty());
        assert!(storage.runs().list().unwrap().is_empty());
        assert!(storage.patches().list().unwrap().is_empty());
    }

    #[test]
    fn opening_storage_completes_repair_journal_before_current_update() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(tmp.path()).unwrap();
        let base = plan_named("base");
        storage.plans().save(&base).unwrap();
        let patch = approved_patch(&base);
        storage.patches().save(&patch).unwrap();
        let next = next_plan(&base, "repaired");
        let transaction = transaction_for(&patch, &next);

        storage.plans().claim_snapshot(&next).unwrap();
        storage.persist_repair_transaction(&transaction).unwrap();
        drop(storage);

        let reopened = StorageRoot::open(tmp.path()).unwrap();
        assert_eq!(
            reopened.plans().load_current(&base.metadata.id).unwrap(),
            next
        );
        assert_eq!(
            reopened.patches().load(&patch.id).unwrap().status,
            PatchStatus::Applied
        );
        assert!(
            std::fs::read_dir(reopened.repair_transactions_dir())
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn opening_storage_completes_repair_journal_after_current_update() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(tmp.path()).unwrap();
        let base = plan_named("base");
        storage.plans().save(&base).unwrap();
        let patch = approved_patch(&base);
        storage.patches().save(&patch).unwrap();
        let next = next_plan(&base, "repaired");
        let transaction = transaction_for(&patch, &next);

        storage.plans().claim_snapshot(&next).unwrap();
        storage.persist_repair_transaction(&transaction).unwrap();
        storage
            .plans()
            .advance_current(&next, Some(base.metadata.version))
            .unwrap();
        drop(storage);

        let reopened = StorageRoot::open(tmp.path()).unwrap();
        assert_eq!(
            reopened.plans().load_current(&base.metadata.id).unwrap(),
            next
        );
        assert_eq!(
            reopened.patches().load(&patch.id).unwrap().status,
            PatchStatus::Applied
        );
        assert!(
            std::fs::read_dir(reopened.repair_transactions_dir())
                .unwrap()
                .next()
                .is_none()
        );
    }

    /// Regression test for #108 (formerly #114): one corrupt repair journal
    /// must not brick `StorageRoot::open` for the whole data directory — it
    /// should be quarantined and recovery should carry on.
    #[test]
    fn a_corrupt_repair_journal_is_quarantined_instead_of_blocking_open() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(tmp.path()).unwrap();
        std::fs::create_dir_all(storage.repair_transactions_dir()).unwrap();
        let bad_journal = storage.repair_transactions_dir().join("bad.json");
        std::fs::write(&bad_journal, b"{ not-json").unwrap();
        drop(storage);

        let reopened = StorageRoot::open(tmp.path()).expect("open must survive a bad journal");

        assert!(
            !bad_journal.exists(),
            "the corrupt journal should have been moved out of the way"
        );
        assert!(
            reopened
                .repair_transactions_dir()
                .join("bad.json.corrupt")
                .exists(),
            "the corrupt journal should be quarantined, not deleted"
        );
    }

    #[test]
    fn competing_repair_patches_from_one_base_have_one_winner() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(tmp.path()).unwrap();
        let base = plan_named("base");
        storage.plans().save(&base).unwrap();
        let left_patch = approved_patch(&base);
        let right_patch = approved_patch(&base);
        storage.patches().save(&left_patch).unwrap();
        storage.patches().save(&right_patch).unwrap();
        let left_plan = next_plan(&base, "left");
        let right_plan = next_plan(&base, "right");
        let barrier = Arc::new(std::sync::Barrier::new(3));

        let handles: Vec<_> = [
            (left_patch.id.clone(), left_plan.clone()),
            (right_patch.id.clone(), right_plan.clone()),
        ]
        .into_iter()
        .map(|(patch_id, plan)| {
            let storage = storage.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                storage.commit_repair(&patch_id, &plan).map(|_| patch_id)
            })
        })
        .collect();
        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let winning_patch_id = results.into_iter().find_map(Result::ok).unwrap();
        let expected_plan = if winning_patch_id == left_patch.id {
            left_plan
        } else {
            right_plan
        };
        assert_eq!(
            storage.plans().load_current(&base.metadata.id).unwrap(),
            expected_plan
        );
        assert_eq!(
            storage.patches().load(&winning_patch_id).unwrap().status,
            PatchStatus::Applied
        );
        let losing_patch_id = if winning_patch_id == left_patch.id {
            right_patch.id
        } else {
            left_patch.id
        };
        assert_eq!(
            storage.patches().load(&losing_patch_id).unwrap().status,
            PatchStatus::Approved
        );
    }

    #[test]
    fn serialized_records_remain_compatible_when_defaulted_fields_are_omitted() {
        let mut plan = plan_named("legacy");
        plan.metadata.status = crate::plan::types::PlanStatus::Draft;
        plan.metadata.solution_design = Some("legacy design".to_owned());
        let mut plan_json = serde_json::to_value(&plan).unwrap();
        let plan_object = plan_json.as_object_mut().unwrap();
        plan_object.remove("inputs");
        plan_object.remove("config");
        plan_object.remove("outputs");
        let metadata = plan_object
            .get_mut("metadata")
            .unwrap()
            .as_object_mut()
            .unwrap();
        metadata.remove("status");
        metadata.remove("solution_design");
        let compatible_plan: Plan = serde_json::from_value(plan_json).unwrap();
        assert!(compatible_plan.inputs.is_empty());
        assert!(compatible_plan.config.is_empty());
        assert!(compatible_plan.outputs.is_empty());
        assert_eq!(
            compatible_plan.metadata.status,
            crate::plan::types::PlanStatus::Published
        );
        assert!(compatible_plan.metadata.solution_design.is_none());

        let mut run = Run::new(&plan.metadata.id, 1);
        run.step_runs
            .insert("legacy-step".to_owned(), StepRun::new("legacy-step"));
        let mut run_json = serde_json::to_value(&run).unwrap();
        let run_object = run_json.as_object_mut().unwrap();
        run_object.remove("inputs");
        run_object.remove("outputs");
        let step_run = run_object
            .get_mut("step_runs")
            .unwrap()
            .get_mut("legacy-step")
            .unwrap()
            .as_object_mut()
            .unwrap();
        step_run.remove("outputs");
        step_run.remove("token_usage");
        step_run.remove("iterations");
        let compatible_run: Run = serde_json::from_value(run_json).unwrap();
        assert!(compatible_run.inputs.is_empty());
        assert!(compatible_run.outputs.is_empty());
        let compatible_step = &compatible_run.step_runs["legacy-step"];
        assert!(compatible_step.outputs.is_empty());
        assert!(compatible_step.token_usage.is_none());
        assert!(compatible_step.iterations.is_empty());

        let patch = approved_patch(&plan);
        let compatible_patch: Patch =
            serde_json::from_value(serde_json::to_value(&patch).unwrap()).unwrap();
        assert_eq!(compatible_patch, patch);
    }
}
