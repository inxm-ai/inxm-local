//! World-fix proposal storage and types.
//!
//! A repair diagnosis can land on two very different causes: the plan is
//! wrong, or the world is wrong. A `WorldFix` records the second case — the
//! plan step was reasonable, but the runtime environment violated one of its
//! assumptions (a commit step with nothing to commit, a missing file, expired
//! credentials, a down service). It carries no plan mutation and therefore no
//! approval lifecycle; it asks the human to change the world, then authorises
//! resuming the failed run against the SAME plan version.

use crate::error::StorageError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One concrete action to repair the runtime environment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemediationAction {
    /// Human-readable description of what to change in the environment.
    pub description: String,
    /// Optional command the human can run to perform the change. Never
    /// executed by the orchestrator — the world belongs to the human.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// A proposed repair of the runtime environment (not the plan) after a step
/// failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorldFix {
    pub id: String,
    pub plan_id: String,
    pub plan_version: u32,
    pub run_id: String,
    pub failing_step_id: String,
    /// Why the failure is a world problem rather than a plan problem.
    pub diagnosis: String,
    /// Concrete environment changes proposed to the human.
    pub remediation: Vec<RemediationAction>,
    pub proposed_at: DateTime<Utc>,
}

impl WorldFix {
    pub fn new(
        plan_id: impl Into<String>,
        plan_version: u32,
        run_id: impl Into<String>,
        failing_step_id: impl Into<String>,
        diagnosis: impl Into<String>,
        remediation: Vec<RemediationAction>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            plan_id: plan_id.into(),
            plan_version,
            run_id: run_id.into(),
            failing_step_id: failing_step_id.into(),
            diagnosis: diagnosis.into(),
            remediation,
            proposed_at: Utc::now(),
        }
    }
}

// ─── World-fix store ─────────────────────────────────────────────────────────

/// On-disk file suffix for stored world fixes — a persistence contract.
const WORLD_FIX_FILE_SUFFIX: &str = ".json";

/// `kind` reported in [`StorageError::NotFound`] for this store.
const NOT_FOUND_KIND: &str = "world_fix";

pub struct WorldFixStore {
    dir: PathBuf,
}

impl WorldFixStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn save(&self, fix: &WorldFix) -> Result<(), StorageError> {
        super::validate_record_id(&fix.id)?;
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(world_fix_file_name(&fix.id));
        let json = serde_json::to_string_pretty(fix)?;
        super::write_atomically(&path, &json)?;
        Ok(())
    }

    /// List all world fixes, most recent first.
    pub fn list(&self) -> Result<Vec<WorldFix>, StorageError> {
        let mut fixes = Vec::new();
        if !self.dir.exists() {
            return Ok(fixes);
        }
        for entry in std::fs::read_dir(&self.dir)? {
            let Ok(entry) = entry else {
                tracing::warn!(
                    "storage.event" = "list_entry_skipped",
                    "storage.store.kind" = NOT_FOUND_KIND,
                    "storage.skip.reason" = "directory_entry_unreadable",
                    "storage list entry skipped"
                );
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(WORLD_FIX_FILE_SUFFIX)
                || !entry.file_type().is_ok_and(|kind| kind.is_file())
            {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(entry.path()) else {
                tracing::warn!(
                    "storage.event" = "list_entry_skipped",
                    "storage.store.kind" = NOT_FOUND_KIND,
                    "storage.entry.name" = %name,
                    "storage.skip.reason" = "record_unreadable",
                    "storage list entry skipped"
                );
                continue;
            };
            let expected_id = name.trim_end_matches(WORLD_FIX_FILE_SUFFIX);
            match serde_json::from_str::<WorldFix>(&raw) {
                Ok(fix) if fix.id == expected_id => fixes.push(fix),
                Ok(fix) => tracing::warn!(
                    "storage.event" = "embedded_identity_mismatch",
                    "storage.store.kind" = NOT_FOUND_KIND,
                    "world_fix.id.requested" = %expected_id,
                    "world_fix.id.stored" = %fix.id,
                    "stored world fix identity does not match its record key"
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
        fixes.sort_by_key(|fix| std::cmp::Reverse(fix.proposed_at));
        Ok(fixes)
    }

    /// The most recent world fix proposed for one run, if any.
    pub fn latest_for_run(&self, run_id: &str) -> Result<Option<WorldFix>, StorageError> {
        Ok(self.list()?.into_iter().find(|fix| fix.run_id == run_id))
    }

    pub fn load(&self, fix_id: &str) -> Result<WorldFix, StorageError> {
        super::validate_record_id(fix_id)?;
        let path = self.dir.join(world_fix_file_name(fix_id));
        if !path.exists() {
            return Err(StorageError::NotFound {
                kind: NOT_FOUND_KIND,
                id: fix_id.to_owned(),
            });
        }
        let raw = std::fs::read_to_string(path)?;
        let fix: WorldFix = serde_json::from_str(&raw)?;
        if fix.id != fix_id {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "world fix identity mismatch: requested '{fix_id}', stored '{}'",
                    fix.id
                ),
            )
            .into());
        }
        Ok(fix)
    }
}

fn world_fix_file_name(fix_id: &str) -> String {
    format!("{fix_id}{WORLD_FIX_FILE_SUFFIX}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix_for_run(run_id: &str) -> WorldFix {
        WorldFix::new(
            "plan-1",
            1,
            run_id,
            "commit_changes",
            "The branch has no staged changes; the commit step's precondition is unmet.",
            vec![RemediationAction {
                description: "Stage the intended files before resuming".to_owned(),
                command: Some("git add -A".to_owned()),
            }],
        )
    }

    fn store_in(dir: &tempfile::TempDir) -> WorldFixStore {
        WorldFixStore::new(dir.path().join("world-fixes"))
    }

    #[test]
    fn save_then_load_round_trips_the_world_fix() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let fix = fix_for_run("run-1");
        store.save(&fix).unwrap();

        assert_eq!(store.load(&fix.id).unwrap(), fix);
    }

    #[test]
    fn latest_for_run_picks_the_newest_matching_fix() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let mut older = fix_for_run("run-1");
        older.proposed_at = Utc::now() - chrono::Duration::hours(1);
        store.save(&older).unwrap();
        let newer = fix_for_run("run-1");
        store.save(&newer).unwrap();
        store.save(&fix_for_run("run-2")).unwrap();

        assert_eq!(store.latest_for_run("run-1").unwrap().unwrap().id, newer.id);
        assert!(store.latest_for_run("run-3").unwrap().is_none());
    }

    #[test]
    fn load_of_unknown_world_fix_reports_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let err = store_in(&tmp).load("no-such-fix").unwrap_err();
        assert!(
            matches!(err, StorageError::NotFound { kind: "world_fix", ref id } if id == "no-such-fix"),
            "unexpected error: {err:?}"
        );
    }
}
