//! Versioned plan storage.

use crate::error::StorageError;
use crate::plan::types::Plan;
use std::path::{Path, PathBuf};

/// On-disk file names — a persistence contract, do not change.
const CURRENT_FILE_NAME: &str = "current.json";
const VERSION_FILE_PREFIX: &str = "v";
const VERSION_FILE_SUFFIX: &str = ".json";

/// `kind` reported in [`StorageError::NotFound`] for this store.
const NOT_FOUND_KIND: &str = "plan";

fn version_file_name(version: u32) -> String {
    format!("{VERSION_FILE_PREFIX}{version}{VERSION_FILE_SUFFIX}")
}

fn not_found(plan_id: &str) -> StorageError {
    StorageError::NotFound {
        kind: NOT_FOUND_KIND,
        id: plan_id.to_owned(),
    }
}

fn consistency_error(message: impl Into<String>) -> StorageError {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into()).into()
}

pub struct PlanStore {
    dir: PathBuf,
}

impl PlanStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Persist a plan version and update `current.json`.
    pub fn save(&self, plan: &Plan) -> Result<(), StorageError> {
        super::validate_record_id(&plan.metadata.id)?;
        let current = self.load_current_if_present(&plan.metadata.id)?;
        let expected_previous_version = match current {
            None if plan.metadata.version == 1 => None,
            None => {
                return Err(consistency_error(format!(
                    "cannot save plan '{}' at v{} without v1 as current",
                    plan.metadata.id, plan.metadata.version
                )));
            }
            Some(ref current) if current == plan => Some(current.metadata.version),
            Some(current) => {
                let expected_version =
                    current.metadata.version.checked_add(1).ok_or_else(|| {
                        consistency_error(format!("plan '{}' version overflow", plan.metadata.id))
                    })?;
                if plan.metadata.version != expected_version {
                    return Err(consistency_error(format!(
                        "stale or out-of-order plan save for '{}': current is v{}, candidate is v{}",
                        plan.metadata.id, current.metadata.version, plan.metadata.version
                    )));
                }
                Some(current.metadata.version)
            }
        };

        self.claim_snapshot(plan)?;
        self.advance_current(plan, expected_previous_version)
    }

    /// Load the current (latest) version of a plan.
    pub fn load_current(&self, plan_id: &str) -> Result<Plan, StorageError> {
        super::validate_record_id(plan_id)?;
        let path = self.dir.join(plan_id).join(CURRENT_FILE_NAME);
        self.load_from_path(&path, plan_id)
    }

    /// Load a specific version of a plan.
    pub fn load_version(&self, plan_id: &str, version: u32) -> Result<Plan, StorageError> {
        super::validate_record_id(plan_id)?;
        let path = self.dir.join(plan_id).join(version_file_name(version));
        let plan = self.load_from_path(&path, plan_id)?;
        if plan.metadata.version != version {
            tracing::warn!(
                "storage.event" = "embedded_version_mismatch",
                "storage.store.kind" = NOT_FOUND_KIND,
                "plan.id" = %plan_id,
                "plan.version.requested" = version,
                "plan.version.stored" = plan.metadata.version,
                "stored plan version does not match its snapshot filename"
            );
            return Err(consistency_error(format!(
                "plan version mismatch for '{plan_id}': requested v{version}, stored v{}",
                plan.metadata.version
            )));
        }
        Ok(plan)
    }

    fn load_from_path(&self, path: &Path, plan_id: &str) -> Result<Plan, StorageError> {
        if !path.exists() {
            return Err(not_found(plan_id));
        }
        let raw = std::fs::read_to_string(path)?;
        let plan: Plan = serde_json::from_str(&raw)?;
        if plan.metadata.id != plan_id {
            tracing::warn!(
                "storage.event" = "embedded_identity_mismatch",
                "storage.store.kind" = NOT_FOUND_KIND,
                "plan.id.requested" = %plan_id,
                "plan.id.stored" = %plan.metadata.id,
                "stored plan identity does not match its record key"
            );
            return Err(consistency_error(format!(
                "plan identity mismatch: requested '{plan_id}', stored '{}'",
                plan.metadata.id
            )));
        }
        Ok(plan)
    }

    fn load_current_if_present(&self, plan_id: &str) -> Result<Option<Plan>, StorageError> {
        let path = self.dir.join(plan_id).join(CURRENT_FILE_NAME);
        if !path.exists() {
            return Ok(None);
        }
        self.load_from_path(&path, plan_id).map(Some)
    }

    /// Exclusively claim an immutable version snapshot. An identical existing
    /// snapshot is an idempotent retry; different content is a conflict.
    pub(crate) fn claim_snapshot(&self, plan: &Plan) -> Result<(), StorageError> {
        super::validate_record_id(&plan.metadata.id)?;
        let plan_dir = self.dir.join(&plan.metadata.id);
        std::fs::create_dir_all(&plan_dir)?;
        let path = plan_dir.join(version_file_name(plan.metadata.version));
        let json = serde_json::to_string_pretty(plan)?;
        match super::write_exclusively(&path, &json)? {
            super::ExclusiveWrite::Created => Ok(()),
            super::ExclusiveWrite::AlreadyExists => {
                let existing = self.load_version(&plan.metadata.id, plan.metadata.version)?;
                if existing == *plan {
                    Ok(())
                } else {
                    Err(consistency_error(format!(
                        "conflicting immutable snapshot for plan '{}' v{}",
                        plan.metadata.id, plan.metadata.version
                    )))
                }
            }
        }
    }

    /// Advance `current.json` only while it still names the expected predecessor.
    pub(crate) fn advance_current(
        &self,
        plan: &Plan,
        expected_previous_version: Option<u32>,
    ) -> Result<(), StorageError> {
        let current = self.load_current_if_present(&plan.metadata.id)?;
        match current {
            Some(ref current) if current == plan => return Ok(()),
            Some(current)
                if expected_previous_version == Some(current.metadata.version)
                    && plan.metadata.version == current.metadata.version.saturating_add(1) => {}
            None if expected_previous_version.is_none() && plan.metadata.version == 1 => {}
            Some(current) => {
                return Err(consistency_error(format!(
                    "current plan compare-and-swap failed for '{}': expected {:?}, found v{}",
                    plan.metadata.id, expected_previous_version, current.metadata.version
                )));
            }
            None => {
                return Err(consistency_error(format!(
                    "current plan compare-and-swap failed for '{}': expected {:?}, found no current plan",
                    plan.metadata.id, expected_previous_version
                )));
            }
        }

        let current_path = self.dir.join(&plan.metadata.id).join(CURRENT_FILE_NAME);
        let json = serde_json::to_string_pretty(plan)?;
        super::write_atomically(&current_path, &json)
    }

    /// List all stored plan IDs with their latest version numbers.
    pub fn list(&self) -> Result<Vec<PlanSummary>, StorageError> {
        let mut summaries = Vec::new();

        if !self.dir.exists() {
            return Ok(summaries);
        }

        for entry in std::fs::read_dir(&self.dir)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    tracing::warn!(
                        "storage.event" = "list_entry_skipped",
                        "storage.store.kind" = NOT_FOUND_KIND,
                        "storage.skip.reason" = "directory_entry_unreadable",
                        "storage list entry skipped"
                    );
                    continue;
                }
            };
            let entry_name = entry.file_name().to_string_lossy().into_owned();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => {
                    tracing::warn!(
                        "storage.event" = "list_entry_skipped",
                        "storage.store.kind" = NOT_FOUND_KIND,
                        "storage.entry.name" = %entry_name,
                        "storage.skip.reason" = "file_type_unreadable",
                        "storage list entry skipped"
                    );
                    continue;
                }
            };
            if !file_type.is_dir() {
                tracing::warn!(
                    "storage.event" = "list_entry_skipped",
                    "storage.store.kind" = NOT_FOUND_KIND,
                    "storage.entry.name" = %entry_name,
                    "storage.skip.reason" = "not_a_plan_directory",
                    "storage list entry skipped"
                );
                continue;
            }
            let plan_id = entry_name;
            let current_path = entry.path().join(CURRENT_FILE_NAME);
            if !current_path.exists() {
                continue;
            }
            match self.load_current(&plan_id) {
                Ok(plan) => summaries.push(PlanSummary {
                    id: plan.metadata.id,
                    name: plan.name,
                    version: plan.metadata.version,
                    intent: plan.metadata.intent,
                    updated_at: plan.metadata.updated_at,
                    status: plan.metadata.status,
                }),
                Err(_) => tracing::warn!(
                    "storage.event" = "list_entry_skipped",
                    "storage.store.kind" = NOT_FOUND_KIND,
                    "plan.id" = %plan_id,
                    "storage.entry.name" = CURRENT_FILE_NAME,
                    "storage.skip.reason" = "current_record_unreadable_or_corrupt",
                    "storage list entry skipped"
                ),
            }
        }

        summaries.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(summaries)
    }

    /// Delete a plan and all its stored versions.
    pub fn delete(&self, plan_id: &str) -> Result<(), StorageError> {
        super::validate_record_id(plan_id)?;
        let plan_dir = self.dir.join(plan_id);
        if !plan_dir.exists() {
            return Err(not_found(plan_id));
        }
        std::fs::remove_dir_all(plan_dir)?;
        Ok(())
    }

    /// List all stored versions of a plan.
    pub fn list_versions(&self, plan_id: &str) -> Result<Vec<u32>, StorageError> {
        super::validate_record_id(plan_id)?;
        let plan_dir = self.dir.join(plan_id);
        if !plan_dir.exists() {
            return Err(not_found(plan_id));
        }

        let mut versions = Vec::new();
        for entry in std::fs::read_dir(&plan_dir)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    tracing::warn!(
                        "storage.event" = "list_entry_skipped",
                        "storage.store.kind" = "plan_version",
                        "plan.id" = %plan_id,
                        "storage.skip.reason" = "directory_entry_unreadable",
                        "storage list entry skipped"
                    );
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(rest) = name.strip_prefix(VERSION_FILE_PREFIX)
                && let Some(ver_str) = rest.strip_suffix(VERSION_FILE_SUFFIX)
                && let Ok(v) = ver_str.parse::<u32>()
            {
                let is_file = match entry.file_type() {
                    Ok(file_type) => file_type.is_file(),
                    Err(_) => false,
                };
                if !is_file {
                    tracing::warn!(
                        "storage.event" = "list_entry_skipped",
                        "storage.store.kind" = "plan_version",
                        "plan.id" = %plan_id,
                        "storage.entry.name" = %name,
                        "storage.skip.reason" = "not_a_readable_record_file",
                        "storage list entry skipped"
                    );
                    continue;
                }
                match self.load_version(plan_id, v) {
                    Ok(_) => versions.push(v),
                    Err(_) => tracing::warn!(
                        "storage.event" = "list_entry_skipped",
                        "storage.store.kind" = "plan_version",
                        "plan.id" = %plan_id,
                        "plan.version" = v,
                        "storage.entry.name" = %name,
                        "storage.skip.reason" = "record_unreadable_or_corrupt",
                        "storage list entry skipped"
                    ),
                }
            }
        }
        versions.sort();
        Ok(versions)
    }
}

#[derive(Debug)]
pub struct PlanSummary {
    pub id: String,
    pub name: String,
    pub version: u32,
    pub intent: Option<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub status: crate::plan::types::PlanStatus,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::PlanMetadata;
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

    fn store_in(dir: &tempfile::TempDir) -> PlanStore {
        PlanStore::new(dir.path().join("plans"))
    }

    fn assert_invalid_id<T>(result: Result<T, StorageError>) {
        assert!(matches!(
            result,
            Err(StorageError::Io(ref error))
                if error.kind() == std::io::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn save_then_load_round_trips_current_and_versioned_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let v1 = plan_named("original");
        store.save(&v1).unwrap();

        let mut v2 = v1.clone();
        v2.metadata = v1.metadata.next_version();
        v2.name = "renamed".to_owned();
        store.save(&v2).unwrap();

        assert_eq!(store.load_current(&v1.metadata.id).unwrap(), v2);
        assert_eq!(store.load_version(&v1.metadata.id, 1).unwrap(), v1);
        assert_eq!(store.load_version(&v1.metadata.id, 2).unwrap(), v2);
    }

    #[test]
    fn load_current_of_unknown_plan_reports_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let err = store_in(&tmp).load_current("no-such-plan").unwrap_err();
        assert!(
            matches!(err, StorageError::NotFound { kind: "plan", ref id } if id == "no-such-plan"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn load_of_corrupt_plan_file_reports_json_error() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let plan_dir = tmp.path().join("plans").join("broken");
        std::fs::create_dir_all(&plan_dir).unwrap();
        std::fs::write(plan_dir.join(CURRENT_FILE_NAME), "{ not json").unwrap();

        let err = store.load_current("broken").unwrap_err();
        assert!(
            matches!(err, StorageError::Json(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn list_returns_newest_first_and_skips_broken_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);

        let mut older = plan_named("older");
        older.metadata.updated_at = chrono::Utc::now() - chrono::Duration::hours(1);
        store.save(&older).unwrap();
        let newer = plan_named("newer");
        store.save(&newer).unwrap();

        // A directory without current.json and one with corrupt JSON must
        // both be skipped without failing the listing.
        std::fs::create_dir_all(tmp.path().join("plans").join("incomplete")).unwrap();
        let corrupt_dir = tmp.path().join("plans").join("corrupt");
        std::fs::create_dir_all(&corrupt_dir).unwrap();
        std::fs::write(corrupt_dir.join(CURRENT_FILE_NAME), "{ not json").unwrap();
        std::fs::write(tmp.path().join("plans").join("not-a-plan"), "ignore").unwrap();
        let non_file_current = tmp.path().join("plans").join("non-file-current");
        std::fs::create_dir_all(non_file_current.join(CURRENT_FILE_NAME)).unwrap();

        let names: Vec<String> = store.list().unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["newer".to_owned(), "older".to_owned()]);
    }

    #[test]
    fn list_of_missing_storage_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(store_in(&tmp).list().unwrap().is_empty());
    }

    #[test]
    fn delete_removes_every_version() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let plan = plan_named("doomed");
        store.save(&plan).unwrap();

        store.delete(&plan.metadata.id).unwrap();

        assert!(matches!(
            store.load_current(&plan.metadata.id),
            Err(StorageError::NotFound { .. })
        ));
        // Deleting again reports not-found rather than succeeding silently.
        assert!(matches!(
            store.delete(&plan.metadata.id),
            Err(StorageError::NotFound { .. })
        ));
    }

    #[test]
    fn list_versions_sorts_numerically_and_ignores_other_files() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let base = plan_named("versioned");
        store.save(&base).unwrap();

        // v10 before v2 catches lexicographic-vs-numeric sorting bugs.
        for version in [10, 2] {
            let mut plan = base.clone();
            plan.metadata.version = version;
            store.claim_snapshot(&plan).unwrap();
        }
        let plan_dir = tmp.path().join("plans").join(&base.metadata.id);
        std::fs::write(plan_dir.join("notes.txt"), "not a version").unwrap();
        std::fs::write(plan_dir.join("vNaN.json"), "{}").unwrap();
        std::fs::write(plan_dir.join("v3.json"), "{ not json").unwrap();
        std::fs::create_dir(plan_dir.join("v4.json")).unwrap();

        assert_eq!(
            store.list_versions(&base.metadata.id).unwrap(),
            vec![1, 2, 10]
        );
    }

    #[test]
    fn list_versions_of_unknown_plan_reports_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            store_in(&tmp).list_versions("nope"),
            Err(StorageError::NotFound { kind: "plan", .. })
        ));
    }

    #[test]
    fn every_plan_operation_rejects_ids_that_can_escape_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let victim_dir = tmp.path().join("victim");
        std::fs::create_dir_all(&victim_dir).unwrap();
        let marker = victim_dir.join("keep.txt");
        std::fs::write(&marker, "keep").unwrap();
        let absolute_victim = victim_dir.to_string_lossy().into_owned();
        let invalid_ids = [
            "../victim",
            "/tmp/escape",
            "nested/escape",
            r"..\victim",
            r"C:\escape",
            ".",
            "..",
            "",
            &absolute_victim,
        ];

        for invalid_id in invalid_ids {
            let mut plan = plan_named("unsafe");
            plan.metadata.id = invalid_id.to_owned();

            assert_invalid_id(store.save(&plan));
            assert_invalid_id(store.load_current(invalid_id));
            assert_invalid_id(store.load_version(invalid_id, 1));
            assert_invalid_id(store.list_versions(invalid_id));
            assert_invalid_id(store.delete(invalid_id));
            assert!(
                marker.exists(),
                "delete escaped the plan store for {invalid_id:?}"
            );
        }
    }

    #[test]
    fn stale_out_of_order_and_conflicting_same_version_saves_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let v1 = plan_named("v1");
        store.save(&v1).unwrap();

        let mut conflicting_v1 = v1.clone();
        conflicting_v1.name = "conflict".to_owned();
        assert!(store.save(&conflicting_v1).is_err());
        assert_eq!(store.load_version(&v1.metadata.id, 1).unwrap(), v1);

        let mut v3 = v1.clone();
        v3.metadata.version = 3;
        v3.name = "out of order".to_owned();
        assert!(store.save(&v3).is_err());

        let mut v2 = v1.clone();
        v2.metadata = v1.metadata.next_version();
        v2.name = "v2".to_owned();
        store.save(&v2).unwrap();
        assert!(store.save(&v1).is_err());
        store.save(&v2).unwrap();
        assert_eq!(store.load_current(&v1.metadata.id).unwrap(), v2);
    }

    #[test]
    fn concurrent_next_version_saves_have_exactly_one_winner() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let v1 = plan_named("v1");
        store.save(&v1).unwrap();

        let mut left = v1.clone();
        left.metadata = v1.metadata.next_version();
        left.name = "left".to_owned();
        let mut right = left.clone();
        right.name = "right".to_owned();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

        let handles: Vec<_> = [left.clone(), right.clone()]
            .into_iter()
            .map(|candidate| {
                let dir = tmp.path().join("plans");
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let store = PlanStore::new(dir);
                    barrier.wait();
                    store.save(&candidate).map(|_| candidate)
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let winner = results.into_iter().find_map(Result::ok).unwrap();
        assert_eq!(store.load_current(&v1.metadata.id).unwrap(), winner);
        assert_eq!(store.load_version(&v1.metadata.id, 2).unwrap(), winner);
    }

    #[test]
    fn loads_reject_embedded_plan_identity_and_version_mismatches() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let plan = plan_named("stored");
        store.save(&plan).unwrap();
        let plan_dir = tmp.path().join("plans").join(&plan.metadata.id);

        let mut wrong_id = plan.clone();
        wrong_id.metadata.id = "different-id".to_owned();
        std::fs::write(
            plan_dir.join(CURRENT_FILE_NAME),
            serde_json::to_string(&wrong_id).unwrap(),
        )
        .unwrap();
        assert!(store.load_current(&plan.metadata.id).is_err());

        let mut wrong_version = plan.clone();
        wrong_version.metadata.version = 3;
        std::fs::write(
            plan_dir.join(version_file_name(2)),
            serde_json::to_string(&wrong_version).unwrap(),
        )
        .unwrap();
        assert!(store.load_version(&plan.metadata.id, 2).is_err());
    }
}
