//! Run state persistence.

use crate::error::StorageError;
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ─── Run state types ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed {
        failed_step_id: String,
        message: String,
    },
    WaitingForHuman {
        step_id: String,
    },
    Cancelled,
}

impl RunStatus {
    pub fn is_failed(&self) -> bool {
        matches!(self, RunStatus::Failed { .. })
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunStatus::Running => write!(f, "running"),
            RunStatus::Succeeded => write!(f, "succeeded"),
            RunStatus::Failed { failed_step_id, .. } => {
                write!(f, "failed (step: {failed_step_id})")
            }
            RunStatus::WaitingForHuman { step_id } => {
                write!(f, "waiting for human (step: {step_id})")
            }
            RunStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StepRunStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Skipped,
    Cancelled,
    WaitingForHuman,
    /// Any status value this build does not recognize. Forward-compatibility
    /// hatch: a future release may add step statuses that an older build
    /// reading the same data directory does not know about yet. Falling back
    /// to this variant means one unrecognized status does not discard the
    /// entire run record when it is read back.
    #[serde(other)]
    Unknown,
}

impl std::fmt::Display for StepRunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            StepRunStatus::Pending => "pending",
            StepRunStatus::Running => "running",
            StepRunStatus::Succeeded => "succeeded",
            StepRunStatus::Failed => "failed",
            StepRunStatus::Skipped => "skipped",
            StepRunStatus::Cancelled => "cancelled",
            StepRunStatus::WaitingForHuman => "waiting_for_human",
            StepRunStatus::Unknown => "unknown",
        };
        write!(f, "{s}")
    }
}

/// Which surface started a run. Recorded once at creation time.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunSource {
    Chat,
    Mcp,
    Schedule,
}

/// Token usage reported by an LLM API or agent CLI call.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    pub fn add(&mut self, other: TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

/// One execution of a step template inside a FAN_OUT iteration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StepRunIteration {
    pub iteration: usize,
    pub status: StepRunStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_ms: u64,
    #[serde(default)]
    pub outputs: IndexMap<String, serde_json::Value>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub token_usage: Option<TokenUsage>,
}

/// Per-step execution record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StepRun {
    pub step_id: String,
    pub status: StepRunStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub attempt: u32,
    /// Outputs produced by the step (keyed by output name).
    #[serde(default)]
    pub outputs: IndexMap<String, serde_json::Value>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    /// Human-readable error message if the step failed.
    pub error: Option<String>,
    pub duration_ms: Option<u64>,
    /// Tokens consumed by this step's LLM work, when reported by its provider.
    #[serde(default)]
    pub token_usage: Option<TokenUsage>,
    /// Executions of this template when it is owned by a FAN_OUT step.
    #[serde(default)]
    pub iterations: Vec<StepRunIteration>,
}

impl StepRun {
    pub fn new(step_id: impl Into<String>) -> Self {
        Self {
            step_id: step_id.into(),
            status: StepRunStatus::Pending,
            started_at: None,
            finished_at: None,
            attempt: 0,
            outputs: IndexMap::new(),
            stdout: None,
            stderr: None,
            error: None,
            duration_ms: None,
            token_usage: None,
            iterations: Vec::new(),
        }
    }
}

/// The full state of a workflow run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Run {
    pub id: String,
    pub plan_id: String,
    pub plan_version: u32,
    pub status: RunStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Which surface started this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<RunSource>,
    /// Validated values supplied for this invocation, including applied defaults.
    #[serde(default)]
    pub inputs: IndexMap<String, serde_json::Value>,
    /// Step runs keyed by step ID, in insertion order.
    #[serde(default)]
    pub step_runs: IndexMap<String, StepRun>,
    /// The plan's published outputs, resolved from `plan.outputs` against the
    /// completed run's step outputs. Populated once the run succeeds; shown
    /// to the user as the run's "final result".
    #[serde(default)]
    pub outputs: IndexMap<String, serde_json::Value>,
}

impl Run {
    pub fn new(plan_id: impl Into<String>, plan_version: u32) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            plan_id: plan_id.into(),
            plan_version,
            status: RunStatus::Running,
            started_at: Utc::now(),
            finished_at: None,
            source: None,
            inputs: IndexMap::new(),
            step_runs: IndexMap::new(),
            outputs: IndexMap::new(),
        }
    }

    pub fn failed_step(&self) -> Option<&StepRun> {
        self.step_runs
            .values()
            .find(|sr| sr.status == StepRunStatus::Failed)
    }

    /// Aggregate token usage across every step that reported it, plus how many
    /// prompt or agent step executions contributed.
    pub fn token_usage_summary(&self) -> TokenUsageSummary {
        let mut summary = TokenUsageSummary::default();
        for sr in self.step_runs.values() {
            if sr.iterations.is_empty() {
                if let Some(usage) = sr.token_usage {
                    summary.prompt_call_steps += 1;
                    summary.usage.add(usage);
                }
            } else {
                for iteration in &sr.iterations {
                    if let Some(usage) = iteration.token_usage {
                        summary.prompt_call_steps += 1;
                        summary.usage.add(usage);
                    }
                }
            }
        }
        summary
    }
}

/// Aggregate token usage for a run: total tokens plus how many AI-backed step
/// executions actually reported usage.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokenUsageSummary {
    /// Number of AI-backed step executions that reported usage.
    ///
    /// The field keeps its original name for API compatibility; it now also
    /// includes AGENT_CALL executions.
    pub prompt_call_steps: u32,
    pub usage: TokenUsage,
}

impl TokenUsageSummary {
    pub fn is_empty(&self) -> bool {
        self.prompt_call_steps == 0
    }

    pub fn ai_steps(&self) -> u32 {
        self.prompt_call_steps
    }
}

// ─── Run store ────────────────────────────────────────────────────────────────

/// On-disk file suffix for stored runs — a persistence contract.
const RUN_FILE_SUFFIX: &str = ".json";

/// `kind` reported in [`StorageError::NotFound`] for this store.
const NOT_FOUND_KIND: &str = "run";

pub struct RunStore {
    dir: PathBuf,
}

impl RunStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Persist (or update) a run.
    pub fn save(&self, run: &Run) -> Result<(), StorageError> {
        super::validate_record_id(&run.id)?;
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(run_file_name(&run.id));
        let json = serde_json::to_string_pretty(run)?;
        super::write_atomically(&path, &json)?;
        Ok(())
    }

    /// Load a run by ID.
    pub fn load(&self, run_id: &str) -> Result<Run, StorageError> {
        super::validate_record_id(run_id)?;
        let path = self.dir.join(run_file_name(run_id));
        if !path.exists() {
            return Err(StorageError::NotFound {
                kind: NOT_FOUND_KIND,
                id: run_id.to_owned(),
            });
        }
        let raw = std::fs::read_to_string(path)?;
        let run: Run = serde_json::from_str(&raw)?;
        if run.id != run_id {
            tracing::warn!(
                "storage.event" = "embedded_identity_mismatch",
                "storage.store.kind" = NOT_FOUND_KIND,
                "run.id.requested" = %run_id,
                "run.id.stored" = %run.id,
                "stored run identity does not match its record key"
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "run identity mismatch: requested '{run_id}', stored '{}'",
                    run.id
                ),
            )
            .into());
        }
        Ok(run)
    }

    /// List all runs, most recent first.
    pub fn list(&self) -> Result<Vec<RunSummary>, StorageError> {
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
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(RUN_FILE_SUFFIX) {
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
            let expected_id = name.trim_end_matches(RUN_FILE_SUFFIX);
            match serde_json::from_str::<Run>(&raw) {
                Ok(run) if run.id == expected_id => summaries.push(RunSummary {
                    id: run.id,
                    plan_id: run.plan_id,
                    plan_version: run.plan_version,
                    status: run.status,
                    started_at: run.started_at,
                    finished_at: run.finished_at,
                    source: run.source,
                }),
                Ok(run) => tracing::warn!(
                    "storage.event" = "embedded_identity_mismatch",
                    "storage.store.kind" = NOT_FOUND_KIND,
                    "run.id.requested" = %expected_id,
                    "run.id.stored" = %run.id,
                    "stored run identity does not match its record key"
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
        summaries.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        Ok(summaries)
    }
}

fn run_file_name(run_id: &str) -> String {
    format!("{run_id}{RUN_FILE_SUFFIX}")
}

#[derive(Debug)]
pub struct RunSummary {
    pub id: String,
    pub plan_id: String,
    pub plan_version: u32,
    pub status: RunStatus,
    pub started_at: DateTime<Utc>,
    /// When the run finished, if it has. Lets the Runs view show wall time
    /// for completed runs.
    pub finished_at: Option<DateTime<Utc>>,
    /// Which surface started the run.
    pub source: Option<RunSource>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_usage_summary_ignores_steps_without_usage() {
        let mut run = Run::new("plan-1", 1);

        let mut prompt_step = StepRun::new("summarize");
        prompt_step.token_usage = Some(TokenUsage {
            input_tokens: 100,
            output_tokens: 40,
        });
        run.step_runs.insert("summarize".to_owned(), prompt_step);

        let mut other_step = StepRun::new("fetch");
        other_step.status = StepRunStatus::Succeeded;
        run.step_runs.insert("fetch".to_owned(), other_step);

        let summary = run.token_usage_summary();
        assert_eq!(summary.ai_steps(), 1);
        assert_eq!(summary.usage.input_tokens, 100);
        assert_eq!(summary.usage.output_tokens, 40);
        assert_eq!(summary.usage.total(), 140);
        assert!(!summary.is_empty());
    }

    #[test]
    fn token_usage_summary_sums_multiple_prompt_calls() {
        let mut run = Run::new("plan-1", 1);

        for (id, input, output) in [("a", 10, 5), ("b", 20, 8)] {
            let mut step = StepRun::new(id);
            step.token_usage = Some(TokenUsage {
                input_tokens: input,
                output_tokens: output,
            });
            run.step_runs.insert(id.to_owned(), step);
        }

        let summary = run.token_usage_summary();
        assert_eq!(summary.ai_steps(), 2);
        assert_eq!(summary.usage.total(), 43);
    }

    #[test]
    fn token_usage_summary_empty_when_no_prompt_calls() {
        let mut run = Run::new("plan-1", 1);
        run.step_runs
            .insert("fetch".to_owned(), StepRun::new("fetch"));

        assert!(run.token_usage_summary().is_empty());
    }

    #[test]
    fn token_usage_summary_counts_fan_out_iterations_instead_of_the_template() {
        let mut run = Run::new("plan-1", 1);

        let mut fanned = StepRun::new("summarize-each");
        // The template-level usage must be ignored once iterations exist —
        // only the per-iteration usage counts.
        fanned.token_usage = Some(TokenUsage {
            input_tokens: 999,
            output_tokens: 999,
        });
        for (index, usage) in [Some((10, 2)), None, Some((30, 4))].iter().enumerate() {
            let now = Utc::now();
            fanned.iterations.push(StepRunIteration {
                iteration: index,
                status: StepRunStatus::Succeeded,
                started_at: now,
                finished_at: now,
                duration_ms: 0,
                outputs: IndexMap::new(),
                stdout: None,
                stderr: None,
                error: None,
                token_usage: usage.map(|(input_tokens, output_tokens)| TokenUsage {
                    input_tokens,
                    output_tokens,
                }),
            });
        }
        run.step_runs.insert("summarize-each".to_owned(), fanned);

        let summary = run.token_usage_summary();
        assert_eq!(summary.ai_steps(), 2);
        assert_eq!(summary.usage.input_tokens, 40);
        assert_eq!(summary.usage.output_tokens, 6);
    }

    fn store_in(dir: &tempfile::TempDir) -> RunStore {
        RunStore::new(dir.path().join("runs"))
    }

    fn assert_invalid_id<T>(result: Result<T, StorageError>) {
        assert!(matches!(
            result,
            Err(StorageError::Io(ref error))
                if error.kind() == std::io::ErrorKind::InvalidInput
        ));
    }

    #[test]
    fn save_then_load_round_trips_the_run() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);

        let mut run = Run::new("plan-1", 3);
        run.inputs
            .insert("topic".to_owned(), serde_json::json!("storage"));
        let mut step = StepRun::new("fetch");
        step.status = StepRunStatus::Succeeded;
        step.outputs
            .insert("body".to_owned(), serde_json::json!({"ok": true}));
        run.step_runs.insert("fetch".to_owned(), step);
        store.save(&run).unwrap();

        let loaded = store.load(&run.id).unwrap();
        assert_eq!(loaded, run);
    }

    #[test]
    fn load_of_unknown_run_reports_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let err = store_in(&tmp).load("no-such-run").unwrap_err();
        assert!(
            matches!(err, StorageError::NotFound { kind: "run", ref id } if id == "no-such-run"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn load_of_corrupt_run_file_reports_json_error() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let dir = tmp.path().join("runs");
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

        let mut older = Run::new("plan-1", 1);
        older.started_at = Utc::now() - chrono::Duration::hours(1);
        store.save(&older).unwrap();
        let newer = Run::new("plan-1", 1);
        store.save(&newer).unwrap();

        let dir = tmp.path().join("runs");
        std::fs::write(dir.join("corrupt.json"), "{ not json").unwrap();
        std::fs::write(dir.join("readme.txt"), "not a run").unwrap();
        std::fs::create_dir(dir.join("not-a-file.json")).unwrap();

        let ids: Vec<String> = store.list().unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![newer.id, older.id]);
    }

    #[test]
    fn list_of_missing_storage_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(store_in(&tmp).list().unwrap().is_empty());
    }

    /// Forward compatibility: a run written by a future build may contain a
    /// step status this build does not know about yet. It must fall back to
    /// `StepRunStatus::Unknown` instead of failing to deserialize the whole
    /// record, so the run still shows up in the list.
    #[test]
    fn step_status_unrecognized_by_this_build_falls_back_to_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);

        let mut run = Run::new("plan-1", 1);
        let mut step = StepRun::new("do_thing");
        step.status = StepRunStatus::Succeeded;
        run.step_runs.insert("do_thing".to_owned(), step);
        store.save(&run).unwrap();

        // Simulate a future build having written a step status this one does
        // not recognize.
        let dir = tmp.path().join("runs");
        let raw = std::fs::read_to_string(dir.join(format!("{}.json", run.id))).unwrap();
        let raw = raw.replace("\"succeeded\"", "\"some_future_status\"");
        std::fs::write(dir.join(format!("{}.json", run.id)), raw).unwrap();

        let ids: Vec<String> = store.list().unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![run.id.clone()]);

        let loaded = store.load(&run.id).unwrap();
        assert_eq!(loaded.step_runs["do_thing"].status, StepRunStatus::Unknown);
    }

    #[test]
    fn run_load_and_save_reject_ids_that_can_escape_the_store() {
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
            let mut run = Run::new("plan-1", 1);
            run.id = invalid_id.to_owned();

            assert_invalid_id(store.save(&run));
            assert_invalid_id(store.load(invalid_id));
        }
    }

    #[test]
    fn load_rejects_an_embedded_run_identity_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(&tmp);
        let mut run = Run::new("plan-1", 1);
        let requested_id = run.id.clone();
        run.id = "different-id".to_owned();
        let dir = tmp.path().join("runs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(run_file_name(&requested_id)),
            serde_json::to_string(&run).unwrap(),
        )
        .unwrap();

        assert!(store.load(&requested_id).is_err());
    }
}
