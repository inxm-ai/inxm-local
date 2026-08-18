//! Patch proposal storage and types.

use crate::error::StorageError;
use crate::plan::types::{PlanStep, StepConfig};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ─── Patch types ──────────────────────────────────────────────────────────────

/// What a patch does to the plan.
///
/// Repair patches are constrained edits, never full plan rewrites. The legacy
/// step-scoped operations target `Patch::failing_step_id`; newer JSON-pointer
/// operations carry an explicit target so multi-step repairs can still be small.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PatchOperation {
    /// Apply several constrained operations in order.
    Batch { operations: Vec<PatchOperation> },
    /// Replace the entire failing step with a new definition.
    ReplaceStep { new_step: PlanStep },
    /// Update only the config of the failing step, preserving its ID and metadata.
    UpdateStepConfig { new_config: StepConfig },
    /// Insert a new step immediately before the target step.
    InsertBefore { step: PlanStep },
    /// Insert a new step immediately after the target step.
    InsertAfter { step: PlanStep },
    /// Replace one JSON-tree value inside a specific step using an RFC 6901 JSON pointer.
    SetStepField {
        step_id: String,
        pointer: String,
        value: serde_json::Value,
    },
    /// Remove one JSON-tree value inside a specific step using an RFC 6901 JSON pointer.
    RemoveStepField { step_id: String, pointer: String },
    /// Replace one JSON-tree value inside the plan using an RFC 6901 JSON pointer.
    SetPlanField {
        pointer: String,
        value: serde_json::Value,
    },
    /// Remove one JSON-tree value inside the plan using an RFC 6901 JSON pointer.
    RemovePlanField { pointer: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PatchStatus {
    Pending,
    Approved,
    Rejected,
    Applied,
}

impl std::fmt::Display for PatchStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            PatchStatus::Pending => "pending",
            PatchStatus::Approved => "approved",
            PatchStatus::Rejected => "rejected",
            PatchStatus::Applied => "applied",
        };
        write!(f, "{s}")
    }
}

/// A proposed repair to a plan after a step failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Patch {
    pub id: String,
    pub plan_id: String,
    pub plan_version: u32,
    pub run_id: String,
    pub failing_step_id: String,
    /// The operation to apply to the plan.
    pub operation: PatchOperation,
    /// Explanation from the compiler backend — why this patch was proposed.
    pub rationale: String,
    pub proposed_at: DateTime<Utc>,
    pub status: PatchStatus,
    pub approved_at: Option<DateTime<Utc>>,
    pub rejected_at: Option<DateTime<Utc>>,
    pub rejection_reason: Option<String>,
}

impl Patch {
    pub fn new(
        plan_id: impl Into<String>,
        plan_version: u32,
        run_id: impl Into<String>,
        failing_step_id: impl Into<String>,
        operation: PatchOperation,
        rationale: impl Into<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            plan_id: plan_id.into(),
            plan_version,
            run_id: run_id.into(),
            failing_step_id: failing_step_id.into(),
            operation,
            rationale: rationale.into(),
            proposed_at: Utc::now(),
            status: PatchStatus::Pending,
            approved_at: None,
            rejected_at: None,
            rejection_reason: None,
        }
    }
}

// ─── Patch store ──────────────────────────────────────────────────────────────

/// On-disk file suffix for stored patches — a persistence contract.
const PATCH_FILE_SUFFIX: &str = ".json";

/// `kind` reported in [`StorageError::NotFound`] for this store.
const NOT_FOUND_KIND: &str = "patch";

pub struct PatchStore {
    dir: PathBuf,
}

impl PatchStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn save(&self, patch: &Patch) -> Result<(), StorageError> {
        super::validate_record_id(&patch.id)?;
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(patch_file_name(&patch.id));
        let json = serde_json::to_string_pretty(patch)?;
        super::write_atomically(&path, &json)?;
        Ok(())
    }

    /// List all patches, most recent first.
    pub fn list(&self) -> Result<Vec<Patch>, StorageError> {
        let mut patches = Vec::new();
        if !self.dir.exists() {
            return Ok(patches);
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
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(PATCH_FILE_SUFFIX) {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => {
                    tracing::warn!(
                        "storage.event" = "list_entry_skipped",
                        "storage.store.kind" = NOT_FOUND_KIND,
                        "storage.entry.name" = %name,
                        "storage.skip.reason" = "file_type_unreadable",
                        "storage list entry skipped"
                    );
                    continue;
                }
            };
            if !file_type.is_file() {
                tracing::warn!(
                    "storage.event" = "list_entry_skipped",
                    "storage.store.kind" = NOT_FOUND_KIND,
                    "storage.entry.name" = %name,
                    "storage.skip.reason" = "not_a_record_file",
                    "storage list entry skipped"
                );
                continue;
            }
            let raw = match std::fs::read_to_string(entry.path()) {
                Ok(raw) => raw,
                Err(_) => {
                    tracing::warn!(
                        "storage.event" = "list_entry_skipped",
                        "storage.store.kind" = NOT_FOUND_KIND,
                        "storage.entry.name" = %name,
                        "storage.skip.reason" = "record_unreadable",
                        "storage list entry skipped"
                    );
                    continue;
                }
            };
            let expected_id = name.trim_end_matches(PATCH_FILE_SUFFIX);
            match serde_json::from_str::<Patch>(&raw) {
                Ok(patch) if patch.id == expected_id => patches.push(patch),
                Ok(patch) => tracing::warn!(
                    "storage.event" = "embedded_identity_mismatch",
                    "storage.store.kind" = NOT_FOUND_KIND,
                    "patch.id.requested" = %expected_id,
                    "patch.id.stored" = %patch.id,
                    "stored patch identity does not match its record key"
                ),
                Err(_) => tracing::warn!(
                    "storage.event" = "list_entry_skipped",
                    "storage.store.kind" = NOT_FOUND_KIND,
                    "storage.entry.name" = %name,
                    "storage.skip.reason" = "record_corrupt",
                    "storage list entry skipped"
                ),
            }
        }
        patches.sort_by_key(|p| std::cmp::Reverse(p.proposed_at));
        Ok(patches)
    }

    pub fn load(&self, patch_id: &str) -> Result<Patch, StorageError> {
        super::validate_record_id(patch_id)?;
        let path = self.dir.join(patch_file_name(patch_id));
        if !path.exists() {
            return Err(StorageError::NotFound {
                kind: NOT_FOUND_KIND,
                id: patch_id.to_owned(),
            });
        }
        let raw = std::fs::read_to_string(path)?;
        let patch: Patch = serde_json::from_str(&raw)?;
        if patch.id != patch_id {
            tracing::warn!(
                "storage.event" = "embedded_identity_mismatch",
                "storage.store.kind" = NOT_FOUND_KIND,
                "patch.id.requested" = %patch_id,
                "patch.id.stored" = %patch.id,
                "stored patch identity does not match its record key"
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "patch identity mismatch: requested '{patch_id}', stored '{}'",
                    patch.id
                ),
            )
            .into());
        }
        Ok(patch)
    }
}

fn patch_file_name(patch_id: &str) -> String {
    format!("{patch_id}{PATCH_FILE_SUFFIX}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch_for_run(run_id: &str) -> Patch {
        Patch::new(
            "plan-1",
            1,
            run_id,
            "step-1",
            PatchOperation::RemovePlanField {
                pointer: "/config/obsolete".to_owned(),
            },
            "test rationale",
        )
    }

    fn store_in(dir: &tempfile::TempDir) -> PatchStore {
        PatchStore::new(dir.path().join("patches"))
    }

    fn assert_invalid_id<T>(result: Result<T, StorageError>) {
        assert!(matches!(
            result,
            Err(StorageError::Io(ref error))
                if error.kind() == std::io::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn save_then_load_round_trips_the_patch() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let patch = patch_for_run("run-1");
        store.save(&patch).unwrap();

        let loaded = store.load(&patch.id).unwrap();
        assert_eq!(loaded, patch);
    }

    #[test]
    fn load_of_unknown_patch_reports_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let err = store_in(&tmp).load("no-such-patch").unwrap_err();
        assert!(
            matches!(err, StorageError::NotFound { kind: "patch", ref id } if id == "no-such-patch"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn load_of_corrupt_patch_file_reports_json_error() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let dir = tmp.path().join("patches");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bad.json"), "{ not json").unwrap();

        let err = store.load("bad").unwrap_err();
        assert!(
            matches!(err, StorageError::Json(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn list_returns_newest_first_and_skips_unreadable_files() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);

        let mut older = patch_for_run("run-1");
        older.proposed_at = Utc::now() - chrono::Duration::hours(1);
        store.save(&older).unwrap();
        let newer = patch_for_run("run-2");
        store.save(&newer).unwrap();

        let dir = tmp.path().join("patches");
        std::fs::write(dir.join("corrupt.json"), "{ not json").unwrap();
        std::fs::write(dir.join("readme.txt"), "not a patch").unwrap();
        std::fs::create_dir(dir.join("not-a-file.json")).unwrap();

        let ids: Vec<String> = store.list().unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![newer.id, older.id]);
    }

    #[test]
    fn list_of_missing_storage_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(store_in(&tmp).list().unwrap().is_empty());
    }

    #[test]
    fn patch_load_and_save_reject_ids_that_can_escape_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let absolute_id = tmp.path().join("escape").to_string_lossy().into_owned();
        let invalid_ids = [
            "../escape",
            "/tmp/escape",
            "nested/escape",
            r"..\escape",
            r"C:\escape",
            ".",
            "..",
            "",
            &absolute_id,
        ];

        for invalid_id in invalid_ids {
            let mut patch = patch_for_run("run-1");
            patch.id = invalid_id.to_owned();

            assert_invalid_id(store.save(&patch));
            assert_invalid_id(store.load(invalid_id));
        }
    }

    #[test]
    fn load_rejects_an_embedded_patch_identity_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let mut patch = patch_for_run("run-1");
        let requested_id = patch.id.clone();
        patch.id = "different-id".to_owned();
        let dir = tmp.path().join("patches");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(patch_file_name(&requested_id)),
            serde_json::to_string(&patch).unwrap(),
        )
        .unwrap();

        assert!(store.load(&requested_id).is_err());
    }
}
