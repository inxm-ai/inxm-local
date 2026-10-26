//! Claude usage recorders for plan creation and plan support work.
//!
//! Engine and MCP handlers wrap their compiler calls in [`scoped`] with a
//! [`LedgerRecorder`]; every Claude API call made inside is appended to the
//! storage ledger with the right attribution and, when a compile console is
//! attached, summarised there as one line.

use super::console::CompileConsole;
use crate::llm::{ClaudeCall, ClaudeCallOutcome, ClaudeUsage, UsageRecorder};
use crate::storage::StorageRoot;
use crate::storage::llm_usage::{
    ClaudeCallSummary, CreationPhase, PlanCreationLink, SupportKind, TokenTotals, UsageContext,
    UsageOrigin,
};
use crate::storage::runs::{Run, TokenUsage};
use std::sync::{Arc, Mutex};

/// Phase labels `compile_validate_normalize` sets before its retries.
pub(crate) const PHASE_COMPILE_REGENERATE: &str = "compile_regenerate";
pub(crate) const PHASE_COMPILE_VALIDATION_RETRY: &str = "compile_validation_retry";

pub(crate) struct LedgerRecorder {
    /// `None` when storage could not be opened; calls are then only
    /// summarised on the console. Usage bookkeeping never fails a command.
    storage: Option<StorageRoot>,
    enabled: bool,
    store_prompts: bool,
    context: Mutex<UsageContext>,
    console: Option<CompileConsole>,
}

pub(crate) fn new_creation_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

impl LedgerRecorder {
    fn new(
        storage: Option<StorageRoot>,
        enabled: bool,
        store_prompts: bool,
        context: UsageContext,
        console: Option<CompileConsole>,
    ) -> Arc<Self> {
        Arc::new(Self {
            storage,
            enabled,
            store_prompts,
            context: Mutex::new(context),
            console,
        })
    }

    /// Recorder for one plan-creation command of `creation_session_id`.
    pub(crate) fn creation(
        storage: Option<StorageRoot>,
        enabled: bool,
        store_prompts: bool,
        creation_session_id: &str,
        phase: CreationPhase,
        origin: UsageOrigin,
        console: Option<CompileConsole>,
    ) -> Arc<Self> {
        Self::new(
            storage,
            enabled,
            store_prompts,
            UsageContext::PlanCreation {
                creation_session_id: creation_session_id.to_owned(),
                phase,
                plan_id: None,
                plan_version: None,
                origin,
            },
            console,
        )
    }

    /// Recorder for an edit of a known plan; every call is labelled `Edit`.
    pub(crate) fn edit(
        storage: Option<StorageRoot>,
        enabled: bool,
        store_prompts: bool,
        plan_id: &str,
        plan_version: u32,
        origin: UsageOrigin,
        console: Option<CompileConsole>,
    ) -> Arc<Self> {
        Self::new(
            storage,
            enabled,
            store_prompts,
            UsageContext::PlanCreation {
                creation_session_id: new_creation_session_id(),
                phase: CreationPhase::Edit,
                plan_id: Some(plan_id.to_owned()),
                plan_version: Some(plan_version),
                origin,
            },
            console,
        )
    }

    pub(crate) fn support(
        storage: Option<StorageRoot>,
        enabled: bool,
        store_prompts: bool,
        kind: SupportKind,
        plan_id: Option<&str>,
    ) -> Arc<Self> {
        Self::new(
            storage,
            enabled,
            store_prompts,
            UsageContext::PlanSupport {
                kind,
                plan_id: plan_id.map(str::to_owned),
            },
            None,
        )
    }

    /// Tie this creation session to the plan it produced.
    pub(crate) fn link_plan(&self, plan_id: &str, plan_version: u32) {
        if !self.enabled {
            return;
        }
        let UsageContext::PlanCreation {
            creation_session_id,
            ..
        } = lock(&self.context).clone()
        else {
            return;
        };
        let Some(storage) = &self.storage else {
            return;
        };
        let link = PlanCreationLink::new(creation_session_id, plan_id, plan_version);
        if let Err(error) = storage.llm_usage().append_link(link) {
            tracing::warn!(
                name: "inxm.app.claude_usage.link_failed",
                plan_id = %plan_id,
                error = %error,
                "could not write Claude usage creation link"
            );
        }
    }

    fn current_phase(&self) -> Option<CreationPhase> {
        match &*lock(&self.context) {
            UsageContext::PlanCreation { phase, .. } => Some(*phase),
            _ => None,
        }
    }
}

impl UsageRecorder for LedgerRecorder {
    fn record(&self, call: ClaudeCall) {
        if !self.enabled {
            return;
        }
        let context = lock(&self.context).clone();
        let outcome = call.outcome.clone();
        let usage = call.usage.clone();
        if let Some(storage) = &self.storage
            && let Err(error) = storage.llm_usage().append_call(context, call)
        {
            tracing::warn!(
                name: "inxm.app.claude_usage.record_failed",
                error = %error,
                "could not write Claude usage ledger entry"
            );
        }
        if let (Some(console), Some(phase)) = (&self.console, self.current_phase()) {
            console.info(call_line(phase, &outcome, usage.as_ref()));
        }
    }

    fn store_prompts(&self) -> bool {
        self.store_prompts
    }

    /// Hints come from the compiler (`assess`, `design`, `design_correction`,
    /// `compile`) and from `compile_validate_normalize` (the compile retry
    /// labels). The compiler's plain `compile` never overrides a retry label,
    /// and edits stay `Edit` throughout.
    fn set_phase(&self, label: &str) {
        if !self.enabled {
            return;
        }
        let next = match label {
            "assess" => CreationPhase::Assess,
            "design" => CreationPhase::Design,
            "design_correction" => CreationPhase::DesignCorrection,
            PHASE_COMPILE_REGENERATE => CreationPhase::CompileRegenerate,
            PHASE_COMPILE_VALIDATION_RETRY => CreationPhase::CompileValidationRetry,
            _ => return,
        };
        if let UsageContext::PlanCreation { phase, .. } = &mut *lock(&self.context)
            && *phase != CreationPhase::Edit
        {
            *phase = next;
        }
    }
}

/// Run `future` with `recorder` receiving its Claude API calls.
pub(crate) async fn scoped<F: std::future::Future>(
    recorder: &Arc<LedgerRecorder>,
    future: F,
) -> F::Output {
    let recorder: Arc<dyn UsageRecorder> = recorder.clone();
    crate::llm::with_usage_recorder(recorder, future).await
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn phase_label(phase: CreationPhase) -> &'static str {
    match phase {
        CreationPhase::Assess => "assess",
        CreationPhase::Design => "design",
        CreationPhase::DesignCorrection => "design correction",
        CreationPhase::Compile => "compile",
        CreationPhase::CompileRegenerate => "compile regenerate",
        CreationPhase::CompileValidationRetry => "compile validation retry",
        CreationPhase::Edit => "edit",
        CreationPhase::Unknown => "unknown",
    }
}

/// e.g. `Claude token usage: in 12,431 (cache r 8,120 / w 0), out 1,203, total 13,634`
fn call_line(
    phase: CreationPhase,
    outcome: &ClaudeCallOutcome,
    usage: Option<&ClaudeUsage>,
) -> String {
    let label = format!("Claude token usage ({}):", phase_label(phase));
    match outcome {
        ClaudeCallOutcome::HttpError { status } => {
            format!("{label} HTTP {status}, no usage")
        }
        _ => format!("{label} {}", usage_text(outcome, usage)),
    }
}

/// Total Claude usage of a plan's creation sessions, read from the ledger.
/// `None` when nothing was recorded or the ledger is unreadable.
pub(crate) fn plan_creation_totals(storage: &StorageRoot, plan_id: &str) -> Option<TokenTotals> {
    storage
        .llm_usage()
        .plan_creation_total(plan_id)
        .inspect_err(|error| {
            tracing::warn!(
                name: "inxm.app.claude_usage.read_failed",
                plan_id = %plan_id,
                error = %error,
                "could not read Claude usage ledger"
            );
        })
        .ok()
        .filter(|totals| !totals.is_empty())
}

fn count_text(count: u64, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{} {singular}s", thousands(count))
    }
}

/// `, +1 call with unknown usage` — empty when every call reported usage.
fn unknown_suffix(totals: &TokenTotals) -> String {
    if totals.unknown_calls == 0 {
        String::new()
    } else {
        format!(
            ", +{} with unknown usage",
            count_text(totals.unknown_calls, "call")
        )
    }
}

/// `Claude tokens: 3 calls — 12,450 in (8,120 cached) / 1,320 out (13,770 total).`
pub(crate) fn run_totals_text(totals: &TokenTotals) -> String {
    format!(
        "Claude tokens: {} — {} in ({} cached) / {} out ({} total){}.",
        count_text(totals.calls, "call"),
        thousands(totals.total_input()),
        thousands(totals.cache_read),
        thousands(totals.output),
        thousands(totals.total),
        unknown_suffix(totals),
    )
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct RunTokenUsage {
    claude: TokenTotals,
    additional: TokenUsage,
    additional_steps: u32,
}

impl RunTokenUsage {
    pub(crate) fn from_run(run: &Run, show_claude_usage: bool) -> Self {
        let mut summary = Self {
            claude: if show_claude_usage {
                run.claude_totals()
            } else {
                TokenTotals::default()
            },
            ..Self::default()
        };
        for step in run.step_runs.values() {
            if step.iterations.is_empty() {
                if !show_claude_usage || step.claude_calls.is_empty() {
                    summary.add_basic(step.token_usage);
                }
            } else {
                for iteration in &step.iterations {
                    if !show_claude_usage
                        || !step
                            .claude_calls
                            .iter()
                            .any(|call| call.iteration == Some(iteration.iteration))
                    {
                        summary.add_basic(iteration.token_usage);
                    }
                }
            }
        }
        summary
    }

    fn add_basic(&mut self, usage: Option<TokenUsage>) {
        if let Some(usage) = usage {
            self.additional.add(usage);
            self.additional_steps += 1;
        }
    }

    pub(crate) fn totals(&self) -> (Option<u64>, u64) {
        let reported = !self.claude.is_empty() || self.additional_steps > 0;
        (
            reported.then(|| self.claude.total + self.additional.total()),
            self.claude.unknown_calls,
        )
    }

    pub(crate) fn text(&self) -> Option<String> {
        if self.claude.is_empty() {
            return (self.additional_steps > 0).then(|| {
                let step_word = if self.additional_steps == 1 {
                    "AI step"
                } else {
                    "AI steps"
                };
                format!(
                    "Tokens: {} {step_word} — {} in / {} out ({} total).",
                    self.additional_steps,
                    self.additional.input_tokens,
                    self.additional.output_tokens,
                    self.additional.total(),
                )
            });
        }
        if self.additional_steps == 0 {
            return Some(run_totals_text(&self.claude));
        }
        Some(format!(
            "Tokens: {} + {} — {} in / {} out ({} total){}.",
            count_text(self.claude.calls, "Claude API call"),
            count_text(u64::from(self.additional_steps), "AI step"),
            thousands(self.claude.total_input() + self.additional.input_tokens),
            thousands(self.claude.output + self.additional.output_tokens),
            thousands(self.claude.total + self.additional.total()),
            unknown_suffix(&self.claude),
        ))
    }
}

pub(crate) fn creation_usage_label(totals: &TokenTotals) -> Option<String> {
    if totals.is_empty() {
        return None;
    }
    let unknown = if totals.unknown_calls > 0 { "+" } else { "" };
    Some(format!(
        "Plan creation: {}{unknown} tokens spent",
        thousands(totals.total),
    ))
}

pub(crate) fn token_count_label(total: u64, unknown_calls: u64) -> String {
    let unknown = if unknown_calls > 0 { "+" } else { "" };
    format!("{}{unknown} tokens", thousands(total))
}

/// Compact per-step label shown next to the step duration.
pub(crate) fn step_tokens_label(calls: &[ClaudeCallSummary]) -> Option<String> {
    if calls.is_empty() {
        return None;
    }
    let totals = TokenTotals::from_summaries(calls);
    Some(token_count_label(totals.total, totals.unknown_calls))
}

/// One line per recorded call for the step detail view.
pub(crate) fn step_calls_text(calls: &[ClaudeCallSummary]) -> String {
    calls
        .iter()
        .map(|call| {
            let mut label = format!("attempt {}", call.attempt);
            if let Some(iteration) = call.iteration {
                label.push_str(&format!(" · item {}", iteration + 1));
            }
            format!(
                "{label} · {} · {}",
                outcome_label(&call.outcome),
                usage_text(&call.outcome, call.usage.as_ref())
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn outcome_label(outcome: &ClaudeCallOutcome) -> String {
    match outcome {
        ClaudeCallOutcome::Ok => "ok".to_owned(),
        ClaudeCallOutcome::NoTextBlock => "no text (billed)".to_owned(),
        ClaudeCallOutcome::HttpError { status } => format!("HTTP {status}"),
        ClaudeCallOutcome::TransportError => "connection failed".to_owned(),
        ClaudeCallOutcome::InvalidJson => "unreadable response".to_owned(),
    }
}

/// `in 12,431 (cache r 8,120 / w 0), out 1,203, total 13,634`
fn usage_text(outcome: &ClaudeCallOutcome, usage: Option<&ClaudeUsage>) -> String {
    if matches!(outcome, ClaudeCallOutcome::HttpError { .. }) {
        return "no usage".to_owned();
    }
    match usage.and_then(|usage| Some((usage, usage.total()?))) {
        Some((usage, total)) => format!(
            "in {} (cache r {} / w {}), out {}, total {}",
            thousands(usage.input_tokens.unwrap_or(0)),
            thousands(usage.cache_read_input_tokens.unwrap_or(0)),
            thousands(usage.cache_creation_input_tokens.unwrap_or(0)),
            thousands(usage.output_tokens.unwrap_or(0)),
            thousands(total),
        ),
        None => "usage unknown".to_owned(),
    }
}

pub(crate) fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::backend::{CompletionFuture, CompletionPort};
    use crate::llm::{CompletionRequest, CompletionResponse, LlmProfile};

    fn claude_call(input: u64, output: u64) -> ClaudeCall {
        ClaudeCall {
            message_id: Some("msg".to_owned()),
            request_id: Some("req".to_owned()),
            model_requested: "claude-test".to_owned(),
            model_reported: Some("claude-test".to_owned()),
            http_status: Some(200),
            stop_reason: Some("end_turn".to_owned()),
            response_headers: Default::default(),
            response_body: None,
            request_body: None,
            outcome: ClaudeCallOutcome::Ok,
            usage: crate::llm::parse_claude_usage(&serde_json::json!({
                "input_tokens": input,
                "output_tokens": output,
                "cache_read_input_tokens": 1000
            })),
            latency_ms: 1,
            started_at: chrono::Utc::now(),
        }
    }

    /// Returns scripted texts and reports each as an Anthropic API call, the
    /// way the real transport does inside a recorder scope.
    struct AnthropicShapedPort {
        texts: Mutex<std::collections::VecDeque<&'static str>>,
    }

    impl CompletionPort for AnthropicShapedPort {
        fn complete<'a>(
            &'a self,
            _profile: &'a LlmProfile,
            _request: CompletionRequest<'a>,
        ) -> CompletionFuture<'a> {
            Box::pin(async move {
                let text = lock(&self.texts).pop_front().expect("scripted text");
                crate::llm::record_in_active_scope(claude_call(100, 10));
                Ok(CompletionResponse {
                    text: text.to_owned(),
                    input_tokens: Some(100),
                    output_tokens: Some(10),
                    usage: None,
                })
            })
        }
    }

    fn backend(texts: Vec<&'static str>) -> crate::compiler::Backend {
        crate::compiler::Backend::from_profile_with_port(
            LlmProfile {
                id: "test".to_owned(),
                name: "Test".to_owned(),
                protocol: crate::llm::LlmProtocol::AnthropicMessages,
                model: "claude-test".to_owned(),
                base_url: "https://api.anthropic.com/v1".to_owned(),
                api_key: "not-used".to_owned(),
                auth: crate::llm::LlmAuth::AnthropicKey,
                headers: Default::default(),
                executable: String::new(),
                command_template: String::new(),
                max_tokens: Some(1_024),
                temperature: None,
                timeout_secs: 30,
                codex_sandbox_mode: crate::llm::CodexSandboxMode::default(),
            },
            Arc::new(AnthropicShapedPort {
                texts: Mutex::new(texts.into()),
            }),
        )
        .unwrap()
    }

    fn ledger_phases(storage: &StorageRoot) -> Vec<CreationPhase> {
        storage
            .llm_usage()
            .read_all()
            .unwrap()
            .into_iter()
            .filter_map(|line| match line {
                crate::storage::llm_usage::LedgerLine::Call(entry) => match entry.context {
                    UsageContext::PlanCreation { phase, .. } => Some(phase),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn json_regeneration_is_recorded_as_its_own_phase_and_linked() {
        let dir = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(dir.path()).unwrap();
        let console = CompileConsole::new("compile", None, None);
        let recorder = LedgerRecorder::creation(
            Some(storage.clone()),
            true,
            false,
            "session-1",
            CreationPhase::Compile,
            UsageOrigin::Chat,
            Some(console.clone()),
        );
        let catalog = crate::tools::catalog::ToolCatalog::default();
        let request = crate::compiler::CompileRequest {
            intent: "do something".to_owned(),
            allowed_step_types: vec![crate::plan::types::StepType::HumanInteraction],
            tool_catalog: vec![],
            existing_plan: None,
            run_history: vec![],
            extra_context: None,
        };

        let result = scoped(
            &recorder,
            super::super::engine::compile_validate_normalize(
                &backend(vec!["not a plan", "still not a plan"]),
                request,
                &catalog,
                "compilation failed",
                Some(&console),
            ),
        )
        .await;

        assert!(result.is_err(), "both responses are unparseable");
        assert_eq!(
            ledger_phases(&storage),
            vec![CreationPhase::Compile, CreationPhase::CompileRegenerate]
        );
        recorder.link_plan("plan-a", 1);
        let totals = storage.llm_usage().plan_creation_total("plan-a").unwrap();
        assert_eq!(totals.calls, 2);
        assert_eq!(totals.total, 2 * (100 + 1000 + 10));
        let lines = console.snapshot().lines;
        assert!(lines.iter().any(|line| line.text
            == "Claude token usage (compile): in 100 (cache r 1,000 / w 0), out 10, total 1,110"));
        assert!(lines.iter().any(|line| {
            line.text
                .starts_with("Claude token usage (compile regenerate):")
        }));
    }

    #[tokio::test]
    async fn compiler_compile_hint_does_not_override_retry_label_and_edits_stay_edits() {
        let recorder = LedgerRecorder::creation(
            None,
            true,
            false,
            "session-1",
            CreationPhase::Compile,
            UsageOrigin::Mcp,
            None,
        );
        recorder.set_phase(PHASE_COMPILE_VALIDATION_RETRY);
        recorder.set_phase("compile");
        assert_eq!(
            recorder.current_phase(),
            Some(CreationPhase::CompileValidationRetry)
        );

        let edit = LedgerRecorder::edit(None, true, false, "plan-a", 2, UsageOrigin::Chat, None);
        edit.set_phase(PHASE_COMPILE_REGENERATE);
        edit.set_phase("design_correction");
        assert_eq!(edit.current_phase(), Some(CreationPhase::Edit));
    }

    #[tokio::test]
    async fn support_calls_are_attributed_to_the_plan() {
        let dir = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(dir.path()).unwrap();
        let recorder = LedgerRecorder::support(
            Some(storage.clone()),
            true,
            false,
            SupportKind::ScheduleCron,
            Some("plan-a"),
        );
        scoped(&recorder, async {
            crate::llm::record_in_active_scope(claude_call(5, 1));
        })
        .await;
        let calls = storage.llm_usage().calls_for_plan("plan-a").unwrap();
        assert_eq!(calls.len(), 1);
        assert!(matches!(
            calls[0].context,
            UsageContext::PlanSupport {
                kind: SupportKind::ScheduleCron,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn disabled_recorder_does_not_write_or_summarize_calls() {
        let dir = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(dir.path()).unwrap();
        let console = CompileConsole::new("compile", None, None);
        let recorder = LedgerRecorder::creation(
            Some(storage.clone()),
            false,
            false,
            "session-1",
            CreationPhase::Compile,
            UsageOrigin::Chat,
            Some(console.clone()),
        );

        scoped(&recorder, async {
            crate::llm::record_in_active_scope(claude_call(100, 10));
        })
        .await;
        recorder.link_plan("plan-a", 1);

        assert!(storage.llm_usage().read_all().unwrap().is_empty());
        assert!(
            console
                .snapshot()
                .lines
                .iter()
                .all(|line| !line.text.starts_with("Claude token usage"))
        );
    }

    #[test]
    fn prompt_capture_follows_the_recorder_setting() {
        let enabled = LedgerRecorder::support(None, true, true, SupportKind::ToolSynthesis, None);
        let disabled = LedgerRecorder::support(None, true, false, SupportKind::ToolSynthesis, None);

        assert!(UsageRecorder::store_prompts(&*enabled));
        assert!(!UsageRecorder::store_prompts(&*disabled));
    }

    fn summary(
        attempt: u32,
        iteration: Option<usize>,
        usage: Option<(u64, u64)>,
    ) -> ClaudeCallSummary {
        ClaudeCallSummary {
            call_id: format!("call-{attempt}"),
            attempt,
            iteration,
            outcome: if usage.is_some() {
                ClaudeCallOutcome::Ok
            } else {
                ClaudeCallOutcome::TransportError
            },
            usage: usage.map(|(input, output)| claude_call(input, output).usage.unwrap()),
        }
    }

    #[test]
    fn totals_texts_match_the_documented_shapes() {
        let totals = TokenTotals {
            calls: 3,
            input: 4_330,
            cache_creation: 0,
            cache_read: 8_120,
            output: 1_320,
            total: 13_770,
            unknown_calls: 0,
        };
        assert_eq!(
            run_totals_text(&totals),
            "Claude tokens: 3 calls — 12,450 in (8,120 cached) / 1,320 out (13,770 total)."
        );
        assert_eq!(
            run_totals_text(&TokenTotals {
                calls: 2,
                total: 10,
                unknown_calls: 1,
                ..TokenTotals::default()
            }),
            "Claude tokens: 2 calls — 0 in (0 cached) / 0 out (10 total), +1 call with unknown usage."
        );
    }

    #[test]
    fn creation_usage_label_is_compact_and_flags_unknown_usage() {
        assert_eq!(
            creation_usage_label(&TokenTotals {
                calls: 3,
                total: 13_770,
                ..TokenTotals::default()
            })
            .as_deref(),
            Some("Plan creation: 13,770 tokens spent")
        );
        assert_eq!(
            creation_usage_label(&TokenTotals {
                calls: 2,
                total: 10,
                unknown_calls: 1,
                ..TokenTotals::default()
            })
            .as_deref(),
            Some("Plan creation: 10+ tokens spent")
        );
        assert_eq!(creation_usage_label(&TokenTotals::default()), None);
    }

    #[test]
    fn token_count_label_flags_unknown_usage() {
        assert_eq!(token_count_label(1110, 1), "1,110+ tokens");
        assert_eq!(token_count_label(0, 0), "0 tokens");
        assert_eq!(token_count_label(235, 0), "235 tokens");
    }

    #[test]
    fn run_totals_combine_api_retries_archived_calls_and_basic_agent_usage() {
        use crate::storage::runs::StepRun;

        let mut run = Run::new("plan", 1);
        let mut prompt = StepRun::new("prompt");
        prompt.token_usage = Some(TokenUsage {
            input_tokens: 1100,
            output_tokens: 10,
        });
        prompt.claude_calls = vec![
            summary(1, None, Some((100, 10))),
            summary(2, None, Some((100, 10))),
        ];
        run.step_runs.insert("prompt".to_owned(), prompt);
        let mut agent = StepRun::new("agent");
        agent.token_usage = Some(TokenUsage {
            input_tokens: 800,
            output_tokens: 200,
        });
        run.step_runs.insert("agent".to_owned(), agent);
        run.archived_claude_calls
            .insert("agent".to_owned(), vec![summary(1, None, Some((50, 5)))]);

        let detailed = RunTokenUsage::from_run(&run, true);
        assert_eq!(detailed.totals(), (Some(4275), 0));
        assert_eq!(
            detailed.text().as_deref(),
            Some("Tokens: 3 Claude API calls + 1 AI step — 4,050 in / 225 out (4,275 total).")
        );
        let basic = RunTokenUsage::from_run(&run, false);
        assert_eq!(basic.totals(), (Some(2110), 0));
        assert_eq!(
            basic.text().as_deref(),
            Some("Tokens: 2 AI steps — 1900 in / 210 out (2110 total).")
        );
    }

    #[test]
    fn run_totals_use_basic_usage_only_for_unrecorded_fan_out_iterations() {
        use crate::storage::runs::{StepRun, StepRunIteration, StepRunStatus};

        let mut run = Run::new("plan", 1);
        let mut child = StepRun::new("child");
        child.token_usage = Some(TokenUsage {
            input_tokens: 9999,
            output_tokens: 9999,
        });
        for iteration in 0..3 {
            child.iterations.push(StepRunIteration {
                iteration,
                status: StepRunStatus::Succeeded,
                started_at: chrono::Utc::now(),
                finished_at: chrono::Utc::now(),
                duration_ms: 1,
                outputs: Default::default(),
                stdout: None,
                stderr: None,
                error: None,
                token_usage: Some(TokenUsage {
                    input_tokens: 1100,
                    output_tokens: 10,
                }),
            });
        }
        child.claude_calls = vec![summary(1, Some(0), Some((100, 10)))];
        run.step_runs.insert("child".to_owned(), child);
        run.archived_claude_calls
            .insert("removed".to_owned(), vec![summary(1, None, None)]);

        let detailed = RunTokenUsage::from_run(&run, true);
        assert_eq!(detailed.totals(), (Some(3330), 1));
        assert_eq!(
            detailed.text().as_deref(),
            Some(
                "Tokens: 2 Claude API calls + 2 AI steps — 3,300 in / 30 out (3,330 total), +1 call with unknown usage."
            )
        );
        assert_eq!(
            RunTokenUsage::from_run(&run, false).totals(),
            (Some(3330), 0)
        );
    }

    #[test]
    fn run_totals_distinguish_missing_usage_reported_zero_and_unknown_calls() {
        use crate::storage::runs::StepRun;

        let mut run = Run::new("plan", 1);
        let empty = RunTokenUsage::from_run(&run, true);
        assert_eq!(empty.totals(), (None, 0));
        assert_eq!(empty.text(), None);

        let mut step = StepRun::new("agent");
        step.token_usage = Some(TokenUsage::default());
        run.step_runs.insert("agent".to_owned(), step);
        assert_eq!(RunTokenUsage::from_run(&run, true).totals(), (Some(0), 0));

        run.step_runs["agent"].claude_calls = vec![summary(1, None, None)];
        let unknown = RunTokenUsage::from_run(&run, true);
        assert_eq!(unknown.totals(), (Some(0), 1));
        assert!(unknown.text().unwrap().contains("unknown usage"));
    }

    #[test]
    fn step_texts_list_every_attempt_and_flag_unknown_usage() {
        let calls = vec![summary(1, None, None), summary(2, Some(0), Some((100, 10)))];
        assert_eq!(step_tokens_label(&calls).as_deref(), Some("1,110+ tokens"));
        assert_eq!(
            step_calls_text(&calls),
            "attempt 1 · connection failed · usage unknown\n\
             attempt 2 · item 1 · ok · in 100 (cache r 1,000 / w 0), out 10, total 1,110"
        );
        assert_eq!(step_tokens_label(&[]), None);
    }

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(13_634), "13,634");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
