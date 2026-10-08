//! Attribution of Claude API calls made while a run executes.
//!
//! Each step attempt runs inside a [`RunUsageRecorder`] scope. Every call the
//! LLM transport records is appended to the storage ledger right away and
//! buffered for the run record; the executor drains the buffer into the step
//! runs after each attempt, so calls of failed and timed-out attempts are
//! kept too. FAN_OUT spawn steps open a nested scope carrying their own step
//! id and iteration index.

use crate::llm::{ClaudeCall, UsageRecorder};
use crate::storage::StorageRoot;
use crate::storage::llm_usage::{ClaudeCallSummary, UsageContext};
use crate::storage::runs::Run;
use std::sync::{Arc, Mutex};

/// Calls recorded during one attempt, keyed by the step run they belong to.
type CallBuffer = Arc<Mutex<Vec<(String, ClaudeCallSummary)>>>;

tokio::task_local! {
    /// The recorder of the step attempt currently executing.
    static RUN_USAGE: Arc<RunUsageRecorder>;
}

/// Effective usage-recording setting for the current step attempt.
pub(crate) fn current_usage_enabled() -> Option<bool> {
    RUN_USAGE.try_with(|recorder| recorder.enabled).ok()
}

pub(crate) struct RunUsageRecorder {
    storage: Arc<StorageRoot>,
    enabled: bool,
    store_prompts: bool,
    run_id: String,
    plan_id: String,
    plan_version: u32,
    step_id: String,
    attempt: u32,
    iteration: Option<usize>,
    buffer: CallBuffer,
}

impl RunUsageRecorder {
    pub(crate) fn for_attempt(
        storage: Arc<StorageRoot>,
        enabled: bool,
        store_prompts: bool,
        run: &Run,
        step_id: &str,
        attempt: u32,
    ) -> Arc<Self> {
        Arc::new(Self {
            storage,
            enabled,
            store_prompts,
            run_id: run.id.clone(),
            plan_id: run.plan_id.clone(),
            plan_version: run.plan_version,
            step_id: step_id.to_owned(),
            attempt,
            iteration: None,
            buffer: Arc::default(),
        })
    }

    /// Scope for a FAN_OUT spawn step; shares this attempt's buffer.
    fn child(&self, step_id: &str, iteration: usize) -> Arc<Self> {
        Arc::new(Self {
            storage: Arc::clone(&self.storage),
            enabled: self.enabled,
            store_prompts: self.store_prompts,
            run_id: self.run_id.clone(),
            plan_id: self.plan_id.clone(),
            plan_version: self.plan_version,
            step_id: step_id.to_owned(),
            attempt: self.attempt,
            iteration: Some(iteration),
            buffer: Arc::clone(&self.buffer),
        })
    }

    /// Move every buffered call into its step run's `claude_calls`.
    pub(crate) fn drain_into(&self, run: &mut Run) {
        let calls = std::mem::take(
            &mut *self
                .buffer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for (step_id, summary) in calls {
            if let Some(step_run) = run.step_runs.get_mut(&step_id) {
                step_run.claude_calls.push(summary);
            }
        }
    }
}

impl UsageRecorder for RunUsageRecorder {
    fn record(&self, call: ClaudeCall) {
        if !self.enabled {
            return;
        }
        let context = UsageContext::PlanRun {
            run_id: self.run_id.clone(),
            plan_id: self.plan_id.clone(),
            plan_version: self.plan_version,
            step_id: self.step_id.clone(),
            attempt: self.attempt,
            iteration: self.iteration,
        };
        let entry = match self.storage.llm_usage().append_call(context, call) {
            Ok(entry) => entry,
            Err(error) => {
                // Usage bookkeeping must never fail the step itself.
                tracing::warn!(
                    name: "inxm.executor.claude_usage.record_failed",
                    run_id = %self.run_id,
                    step_id = %self.step_id,
                    step_attempt = self.attempt,
                    error = %error,
                    "could not write Claude usage ledger entry"
                );
                return;
            }
        };
        if let Some(summary) = entry.run_summary() {
            self.buffer
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push((self.step_id.clone(), summary));
        }
    }

    fn store_prompts(&self) -> bool {
        self.store_prompts
    }
}

/// Run one step attempt with `recorder` receiving its Claude API calls.
pub(crate) async fn with_attempt_usage<F: std::future::Future>(
    recorder: Arc<RunUsageRecorder>,
    future: F,
) -> F::Output {
    let llm_recorder: Arc<dyn UsageRecorder> = recorder.clone();
    RUN_USAGE
        .scope(
            recorder,
            crate::llm::with_usage_recorder(llm_recorder, future),
        )
        .await
}

/// Run a FAN_OUT spawn step, attributing its calls to `step_id` and
/// `iteration`. Without an enclosing attempt scope this is a plain await.
pub(crate) async fn with_spawn_usage<F: std::future::Future>(
    step_id: &str,
    iteration: usize,
    future: F,
) -> F::Output {
    match RUN_USAGE.try_with(|parent| parent.child(step_id, iteration)) {
        Ok(child) => with_attempt_usage(child, future).await,
        Err(_) => future.await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ClaudeCallOutcome;
    use crate::storage::runs::{RunStore, StepRun};

    fn call(input: u64, output: u64) -> ClaudeCall {
        ClaudeCall {
            message_id: Some("msg".to_owned()),
            request_id: None,
            model_requested: "claude-test".to_owned(),
            model_reported: None,
            http_status: Some(200),
            stop_reason: Some("end_turn".to_owned()),
            response_headers: Default::default(),
            response_body: None,
            request_body: None,
            outcome: ClaudeCallOutcome::Ok,
            usage: crate::llm::parse_claude_usage(
                &serde_json::json!({"input_tokens": input, "output_tokens": output}),
            ),
            latency_ms: 1,
            started_at: chrono::Utc::now(),
        }
    }

    fn current() -> Arc<RunUsageRecorder> {
        RUN_USAGE.with(Arc::clone)
    }

    fn setup() -> (tempfile::TempDir, Arc<StorageRoot>, Run) {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(StorageRoot::open(dir.path()).unwrap());
        let mut run = Run::new("plan-a", 3);
        for step_id in ["summarize", "fan", "child"] {
            run.step_runs
                .insert(step_id.to_owned(), StepRun::new(step_id));
        }
        (dir, storage, run)
    }

    #[tokio::test]
    async fn each_retry_attempt_is_recorded_and_persisted_with_the_run() {
        let (_dir, storage, mut run) = setup();
        for attempt in 1..=2 {
            let recorder = RunUsageRecorder::for_attempt(
                Arc::clone(&storage),
                true,
                false,
                &run,
                "summarize",
                attempt,
            );
            // Attempt 1 fails after its call; the call must still be kept.
            let outcome: Result<(), &str> = with_attempt_usage(Arc::clone(&recorder), async move {
                current().record(call(100 * u64::from(attempt), 10));
                if attempt == 1 { Err("failed") } else { Ok(()) }
            })
            .await;
            recorder.drain_into(&mut run);
            assert_eq!(outcome.is_ok(), attempt == 2);
        }
        let runs = RunStore::new(storage.runs_dir());
        runs.save(&run).unwrap();
        let loaded = runs.load(&run.id).unwrap();

        let calls = &loaded.step_runs["summarize"].claude_calls;
        assert_eq!(
            calls.iter().map(|c| c.attempt).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(loaded.claude_totals().total, 110 + 210);
        let ledger = storage.llm_usage().calls_for_run(&run.id).unwrap();
        assert_eq!(ledger.len(), 2);
        assert!(matches!(
            &ledger[0].context,
            UsageContext::PlanRun { plan_id, plan_version: 3, step_id, attempt: 1, iteration: None, .. }
                if plan_id == "plan-a" && step_id == "summarize"
        ));
        assert_eq!(storage.llm_usage().run_total(&run.id).unwrap().total, 320);
    }

    #[tokio::test]
    async fn fan_out_spawn_calls_carry_child_step_and_iteration() {
        let (_dir, storage, mut run) = setup();
        let recorder =
            RunUsageRecorder::for_attempt(Arc::clone(&storage), true, false, &run, "fan", 1);
        with_attempt_usage(Arc::clone(&recorder), async {
            for iteration in 0..2 {
                with_spawn_usage("child", iteration, async {
                    current().record(call(1, 1));
                })
                .await;
            }
        })
        .await;
        recorder.drain_into(&mut run);

        assert!(run.step_runs["fan"].claude_calls.is_empty());
        let child = &run.step_runs["child"].claude_calls;
        assert_eq!(
            child.iter().map(|c| c.iteration).collect::<Vec<_>>(),
            vec![Some(0), Some(1)]
        );
        let ledger = storage.llm_usage().calls_for_run(&run.id).unwrap();
        assert!(ledger.iter().all(|entry| matches!(
            &entry.context,
            UsageContext::PlanRun { step_id, iteration: Some(_), .. } if step_id == "child"
        )));
    }

    #[tokio::test]
    async fn spawn_scope_without_attempt_scope_is_a_plain_await() {
        assert_eq!(with_spawn_usage("child", 0, async { 7 }).await, 7);
    }

    #[tokio::test]
    async fn disabled_attempt_does_not_write_or_buffer_calls() {
        let (_dir, storage, mut run) = setup();
        let recorder =
            RunUsageRecorder::for_attempt(Arc::clone(&storage), false, false, &run, "summarize", 1);
        with_attempt_usage(Arc::clone(&recorder), async {
            current().record(call(100, 10));
        })
        .await;
        recorder.drain_into(&mut run);

        assert!(run.step_runs["summarize"].claude_calls.is_empty());
        assert!(storage.llm_usage().read_all().unwrap().is_empty());
    }
}
