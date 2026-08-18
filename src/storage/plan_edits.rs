//! Plan-edit proposal storage — pending `/edit` results awaiting approval.
//!
//! Mirrors `storage::patches`: an `/edit` compiles a new plan version but
//! does not save it, instead storing it here as a reviewable proposal. The
//! plan is only written to the plan store once the user applies it.

use crate::error::StorageError;
use crate::plan::types::Plan;
use crate::storage::patches::PatchStatus;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A proposed LLM-compiled edit to an existing plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanEdit {
    pub id: String,
    pub plan_id: String,
    pub base_version: u32,
    /// The user's free-text description of the requested change.
    pub instruction: String,
    /// Snapshot of the plan as it stood when the edit was requested — used
    /// to render a diff summary in the review UI.
    pub previous_plan: Plan,
    /// The compiled, validated replacement plan. Not yet the plan store's
    /// current version until this proposal is applied.
    pub proposed_plan: Plan,
    pub proposed_at: DateTime<Utc>,
    pub status: PatchStatus,
    pub approved_at: Option<DateTime<Utc>>,
    pub rejected_at: Option<DateTime<Utc>>,
    pub rejection_reason: Option<String>,
}

impl PlanEdit {
    pub fn new(
        plan_id: impl Into<String>,
        base_version: u32,
        instruction: impl Into<String>,
        previous_plan: Plan,
        proposed_plan: Plan,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            plan_id: plan_id.into(),
            base_version,
            instruction: instruction.into(),
            previous_plan,
            proposed_plan,
            proposed_at: Utc::now(),
            status: PatchStatus::Pending,
            approved_at: None,
            rejected_at: None,
            rejection_reason: None,
        }
    }
}

// ─── Plan-edit store ────────────────────────────────────────────────────────────

const PLAN_EDIT_FILE_SUFFIX: &str = ".json";

/// `kind` reported in [`StorageError::NotFound`] for this store.
const NOT_FOUND_KIND: &str = "plan_edit";

pub struct PlanEditStore {
    dir: PathBuf,
}

impl PlanEditStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn save(&self, edit: &PlanEdit) -> Result<(), StorageError> {
        super::validate_record_id(&edit.id)?;
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(plan_edit_file_name(&edit.id));
        let json = serde_json::to_string_pretty(edit)?;
        super::write_atomically(&path, &json)?;
        Ok(())
    }

    /// List all plan-edit proposals, most recent first.
    pub fn list(&self) -> Result<Vec<PlanEdit>, StorageError> {
        let mut edits = Vec::new();
        if !self.dir.exists() {
            return Ok(edits);
        }
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(PLAN_EDIT_FILE_SUFFIX) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let expected_id = name.trim_end_matches(PLAN_EDIT_FILE_SUFFIX);
            match serde_json::from_str::<PlanEdit>(&raw) {
                Ok(edit) if edit.id == expected_id => edits.push(edit),
                Ok(edit) => tracing::warn!(
                    "storage.event" = "embedded_identity_mismatch",
                    "storage.store.kind" = NOT_FOUND_KIND,
                    "plan_edit.id.requested" = %expected_id,
                    "plan_edit.id.stored" = %edit.id,
                    "stored plan edit identity does not match its record key"
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
        edits.sort_by_key(|e| std::cmp::Reverse(e.proposed_at));
        Ok(edits)
    }

    pub fn load(&self, edit_id: &str) -> Result<PlanEdit, StorageError> {
        super::validate_record_id(edit_id)?;
        let path = self.dir.join(plan_edit_file_name(edit_id));
        if !path.exists() {
            return Err(StorageError::NotFound {
                kind: NOT_FOUND_KIND,
                id: edit_id.to_owned(),
            });
        }
        let raw = std::fs::read_to_string(path)?;
        let edit: PlanEdit = serde_json::from_str(&raw)?;
        if edit.id != edit_id {
            tracing::warn!(
                "storage.event" = "embedded_identity_mismatch",
                "storage.store.kind" = NOT_FOUND_KIND,
                "plan_edit.id.requested" = %edit_id,
                "plan_edit.id.stored" = %edit.id,
                "stored plan edit identity does not match its record key"
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "plan edit identity mismatch: requested '{edit_id}', stored '{}'",
                    edit.id
                ),
            )
            .into());
        }
        Ok(edit)
    }
}

fn plan_edit_file_name(edit_id: &str) -> String {
    format!("{edit_id}{PLAN_EDIT_FILE_SUFFIX}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{Plan, PlanMetadata, PlanStatus};

    fn plan_for(id: &str, version: u32) -> Plan {
        Plan {
            metadata: PlanMetadata {
                id: id.to_owned(),
                version,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                compiled_by: None,
                intent: None,
                parent_plan_id: None,
                parent_version: None,
                status: PlanStatus::default(),
                solution_design: None,
            },
            name: "demo".to_owned(),
            description: None,
            inputs: Vec::new(),
            config: Default::default(),
            steps: Vec::new(),
            outputs: Vec::new(),
        }
    }

    fn edit_for(id: &str) -> PlanEdit {
        PlanEdit::new(id, 1, "add a step", plan_for(id, 1), plan_for(id, 2))
    }

    fn store_in(dir: &tempfile::TempDir) -> PlanEditStore {
        PlanEditStore::new(dir.path().join("plan_edits"))
    }

    #[test]
    fn save_then_load_round_trips_the_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let edit = edit_for("plan-1");
        store.save(&edit).unwrap();

        let loaded = store.load(&edit.id).unwrap();
        assert_eq!(loaded, edit);
    }

    #[test]
    fn load_of_unknown_edit_reports_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let err = store_in(&tmp).load("no-such-edit").unwrap_err();
        assert!(
            matches!(err, StorageError::NotFound { kind: "plan_edit", ref id } if id == "no-such-edit"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn list_returns_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);

        let mut older = edit_for("plan-1");
        older.proposed_at = Utc::now() - chrono::Duration::hours(1);
        store.save(&older).unwrap();
        let newer = edit_for("plan-2");
        store.save(&newer).unwrap();

        let ids: Vec<String> = store.list().unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![newer.id, older.id]);
    }
}
