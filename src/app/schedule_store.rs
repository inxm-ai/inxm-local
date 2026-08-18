//! Schedule persistence and cron evaluation.
//!
//! Schedules live in `schedules.json` in the data dir and fire in **local
//! time**. Users write ordinary 5-field crontab expressions
//! (`min hour day month weekday`); the seconds field required by the `cron`
//! crate is prepended automatically.

use std::path::Path;
use std::str::FromStr;

use chrono::{DateTime, Local, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ScheduleStoreError {
    #[error("could not read schedule store '{}': {source}", path.display())]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("schedule store '{}' is malformed: {source}", path.display())]
    Malformed {
        path: std::path::PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not serialize schedules: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("could not atomically write schedule store: {0}")]
    Write(#[from] crate::error::StorageError),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub plan_id: String,
    /// Normalised (6-field) cron expression.
    pub cron: String,
    pub enabled: bool,
    /// Validated invocation values captured when the schedule is created.
    #[serde(default)]
    pub inputs: IndexMap<String, serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub last_run: Option<DateTime<Utc>>,
}

// ─── Cron helpers ─────────────────────────────────────────────────────────────

/// Validate a user-supplied cron expression and normalise it to the 6-field
/// form. Plain 5-field crontab syntax gets `0` seconds prepended.
pub fn normalize_cron(expr: &str) -> Result<String, String> {
    let fields = expr.split_whitespace().count();
    let normalized = match fields {
        5 => format!("0 {}", expr.trim()),
        6 | 7 => expr.trim().to_owned(),
        n => {
            return Err(format!(
                "cron expression must have 5 fields (min hour day month weekday), \
                 6 (leading seconds), or 7 (plus trailing year), got {n}"
            ));
        }
    };
    cron::Schedule::from_str(&normalized)
        .map(|_| normalized)
        .map_err(|e| format!("invalid cron expression: {e}"))
}

/// Next local-time occurrence strictly after `after`.
pub fn next_occurrence(cron_expr: &str, after: DateTime<Local>) -> Option<DateTime<Local>> {
    cron::Schedule::from_str(cron_expr)
        .ok()?
        .after(&after)
        .next()
}

// ─── Persistence ──────────────────────────────────────────────────────────────

pub fn load(path: &Path) -> Result<Vec<Schedule>, ScheduleStoreError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(ScheduleStoreError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    serde_json::from_str(&raw).map_err(|source| ScheduleStoreError::Malformed {
        path: path.to_owned(),
        source,
    })
}

pub fn save(path: &Path, schedules: &[Schedule]) -> Result<(), ScheduleStoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ScheduleStoreError::Read {
            path: parent.to_owned(),
            source,
        })?;
    }
    let json = serde_json::to_string_pretty(schedules)?;
    crate::storage::write_atomically(path, &json)?;
    Ok(())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_field_crontab_gets_seconds_prepended() {
        assert_eq!(
            normalize_cron("*/5 * * * *").unwrap(),
            "0 */5 * * * *".to_owned()
        );
        assert_eq!(
            normalize_cron("30 6 * * 1").unwrap(),
            "0 30 6 * * 1".to_owned()
        );
    }

    #[test]
    fn six_field_expressions_pass_through() {
        assert_eq!(
            normalize_cron("0 0 12 * * *").unwrap(),
            "0 0 12 * * *".to_owned()
        );
    }

    #[test]
    fn invalid_expressions_are_rejected() {
        assert!(normalize_cron("not a cron").is_err());
        assert!(normalize_cron("99 * * * *").is_err());
        assert!(normalize_cron("* *").is_err());
    }

    #[test]
    fn next_occurrence_moves_forward() {
        let normalized = normalize_cron("*/5 * * * *").unwrap();
        let now = Local::now();
        let next = next_occurrence(&normalized, now).expect("always a next slot");
        assert!(next > now);
        assert!(next - now <= chrono::Duration::minutes(5));
    }

    #[test]
    fn save_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("schedules.json");
        let schedules = vec![Schedule {
            id: "s1".to_owned(),
            plan_id: "p1".to_owned(),
            cron: "0 0 8 * * *".to_owned(),
            enabled: true,
            inputs: IndexMap::new(),
            created_at: Utc::now(),
            last_run: None,
        }];
        save(&path, &schedules).unwrap();
        assert_eq!(load(&path).unwrap(), schedules);
        assert!(load(&tmp.path().join("missing.json")).unwrap().is_empty());
    }

    #[test]
    fn malformed_store_is_reported_and_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("schedules.json");
        std::fs::write(&path, "[{\"id\":").unwrap();

        assert!(matches!(
            load(&path),
            Err(ScheduleStoreError::Malformed { .. })
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[{\"id\":");
    }

    #[test]
    fn atomic_save_replaces_complete_documents_without_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("schedules.json");
        save(&path, &[]).unwrap();
        let schedule = Schedule {
            id: "s1".to_owned(),
            plan_id: "p1".to_owned(),
            cron: "0 0 8 * * *".to_owned(),
            enabled: true,
            inputs: IndexMap::new(),
            created_at: Utc::now(),
            last_run: None,
        };
        save(&path, std::slice::from_ref(&schedule)).unwrap();

        assert_eq!(load(&path).unwrap(), vec![schedule]);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }
}
