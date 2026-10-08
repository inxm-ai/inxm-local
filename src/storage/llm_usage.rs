//! Append-only ledger of provider-reported Claude API usage.
//!
//! Layout under the storage root for new records:
//!
//! ```text
//! llm-usage/
//!   plans/{plan-or-creation-id}.jsonl — every plan workflow ledger
//!   tools.jsonl                       — unscoped tool-synthesis calls
//! ```
//!
//! A stored call keeps its optional request and response bodies inline, so a
//! plan's complete Claude API history can be inspected in one file. Legacy
//! monthly ledgers and body sidecars remain readable for compatibility.

use crate::error::StorageError;
use crate::llm::{ClaudeCall, ClaudeCallOutcome, ClaudeUsage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Version of the ledger line format. Version 1 monthly ledgers remain readable.
pub const LEDGER_SCHEMA: u32 = 2;

/// Provider name written on every Claude API entry.
pub const ANTHROPIC_PROVIDER: &str = "anthropic";

/// Legacy monthly ledger naming, retained for reads of existing data.
const LEGACY_LEDGER_FILE_PREFIX: &str = "claude-";
const LEDGER_FILE_SUFFIX: &str = ".jsonl";
const PLANS_SUBDIR: &str = "plans";
const TOOLS_LEDGER_FILE: &str = "tools.jsonl";
const RESPONSES_SUBDIR: &str = "responses";
const REQUESTS_SUBDIR: &str = "requests";
const BODY_FILE_SUFFIX: &str = ".json";

/// `kind` reported in storage warnings and [`StorageError::NotFound`].
const STORE_KIND: &str = "llm_usage";

/// Serializes appends across every `LlmUsageLog` handle in the process. The
/// desktop engine and the MCP server share the process, so this covers both.
static APPEND_LOCK: Mutex<()> = Mutex::new(());

/// Plan-creation step that made a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreationPhase {
    Assess,
    Design,
    DesignCorrection,
    Compile,
    CompileRegenerate,
    CompileValidationRetry,
    Edit,
    /// A phase written by a newer build.
    #[serde(other)]
    Unknown,
}

/// Surface that started a plan-creation session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageOrigin {
    Chat,
    Mcp,
}

/// Plan support work that is counted separately from creation and runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupportKind {
    ScheduleCron,
    ToolSynthesis,
    #[serde(other)]
    Unknown,
}

/// What a recorded call was made for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum UsageContext {
    PlanCreation {
        creation_session_id: String,
        phase: CreationPhase,
        /// Set when the plan is already known (e.g. `Edit`); otherwise the
        /// session is tied to its plan through a [`PlanCreationLink`].
        #[serde(default)]
        plan_id: Option<String>,
        #[serde(default)]
        plan_version: Option<u32>,
        origin: UsageOrigin,
    },
    PlanRun {
        run_id: String,
        plan_id: String,
        plan_version: u32,
        step_id: String,
        attempt: u32,
        /// FAN_OUT iteration index.
        #[serde(default)]
        iteration: Option<usize>,
    },
    PlanSupport {
        kind: SupportKind,
        /// `None` for tool synthesis started from the Tools page.
        #[serde(default)]
        plan_id: Option<String>,
    },
}

/// One recorded Claude API call. Request and response bodies, when enabled,
/// live in the same plan/session ledger line as their usage metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmUsageEntry {
    pub schema: u32,
    pub call_id: String,
    pub provider: String,
    pub context: UsageContext,
    /// Whether this inline call record includes its request body.
    #[serde(default)]
    pub prompt_stored: bool,
    /// Whether this inline call record includes its response body.
    #[serde(default)]
    pub response_stored: bool,
    pub call: ClaudeCall,
}

impl LlmUsageEntry {
    /// Compact form stored in run records; `None` outside a plan run.
    pub fn run_summary(&self) -> Option<ClaudeCallSummary> {
        match &self.context {
            UsageContext::PlanRun {
                attempt, iteration, ..
            } => Some(ClaudeCallSummary {
                call_id: self.call_id.clone(),
                attempt: *attempt,
                iteration: *iteration,
                outcome: self.call.outcome.clone(),
                usage: self.call.usage.clone(),
            }),
            _ => None,
        }
    }
}

/// Ties a plan-creation session to the plan it produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanCreationLink {
    pub schema: u32,
    pub creation_session_id: String,
    pub plan_id: String,
    pub plan_version: u32,
    pub linked_at: DateTime<Utc>,
}

impl PlanCreationLink {
    pub fn new(
        creation_session_id: impl Into<String>,
        plan_id: impl Into<String>,
        plan_version: u32,
    ) -> Self {
        Self {
            schema: LEDGER_SCHEMA,
            creation_session_id: creation_session_id.into(),
            plan_id: plan_id.into(),
            plan_version,
            linked_at: Utc::now(),
        }
    }
}

/// One line of the JSONL ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum LedgerLine {
    Call(Box<LlmUsageEntry>),
    Link(PlanCreationLink),
}

/// A recorded call as kept on a run's step record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaudeCallSummary {
    pub call_id: String,
    pub attempt: u32,
    /// FAN_OUT iteration index; `None` for a plain step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iteration: Option<usize>,
    pub outcome: ClaudeCallOutcome,
    #[serde(default)]
    pub usage: Option<ClaudeUsage>,
}

/// Sum of provider-reported usage over a set of calls.
///
/// Only calls whose input and output counts were both reported contribute
/// tokens. HTTP errors are not billed and count as zero; calls whose usage is
/// unknown (transport errors, unparseable bodies, missing fields) are counted
/// in `unknown_calls` and never estimated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTotals {
    pub calls: u64,
    /// Uncached input tokens.
    pub input: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    /// Inclusive output tokens (thinking included).
    pub output: u64,
    pub total: u64,
    pub unknown_calls: u64,
}

impl TokenTotals {
    pub fn add(&mut self, outcome: &ClaudeCallOutcome, usage: Option<&ClaudeUsage>) {
        self.calls += 1;
        if matches!(outcome, ClaudeCallOutcome::HttpError { .. }) {
            return;
        }
        let Some((usage, total)) = usage.and_then(|usage| Some((usage, usage.total()?))) else {
            self.unknown_calls += 1;
            return;
        };
        self.input += usage.input_tokens.unwrap_or(0);
        self.cache_creation += usage.cache_creation_input_tokens.unwrap_or(0);
        self.cache_read += usage.cache_read_input_tokens.unwrap_or(0);
        self.output += usage.output_tokens.unwrap_or(0);
        self.total += total;
    }

    /// Uncached input plus cache writes plus cache reads.
    pub fn total_input(&self) -> u64 {
        self.input + self.cache_creation + self.cache_read
    }

    pub fn is_empty(&self) -> bool {
        self.calls == 0
    }

    pub fn from_entries<'a>(entries: impl IntoIterator<Item = &'a LlmUsageEntry>) -> Self {
        let mut totals = Self::default();
        for entry in entries {
            totals.add(&entry.call.outcome, entry.call.usage.as_ref());
        }
        totals
    }

    pub fn from_summaries<'a>(summaries: impl IntoIterator<Item = &'a ClaudeCallSummary>) -> Self {
        let mut totals = Self::default();
        for summary in summaries {
            totals.add(&summary.outcome, summary.usage.as_ref());
        }
        totals
    }
}

// ─── Ledger store ────────────────────────────────────────────────────────────

pub struct LlmUsageLog {
    dir: PathBuf,
}

impl LlmUsageLog {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Record one call in the ledger for its plan or creation session.
    pub fn append_call(
        &self,
        context: UsageContext,
        call: ClaudeCall,
    ) -> Result<LlmUsageEntry, StorageError> {
        let call_id = uuid::Uuid::new_v4().to_string();
        let entry = LlmUsageEntry {
            schema: LEDGER_SCHEMA,
            call_id,
            provider: ANTHROPIC_PROVIDER.to_owned(),
            context: context.clone(),
            prompt_stored: call.request_body.is_some(),
            response_stored: call.response_body.is_some(),
            call,
        };
        self.append_line(
            &self.ledger_path_for_context(&context)?,
            &LedgerLine::Call(Box::new(entry.clone())),
        )?;
        Ok(entry)
    }

    pub fn append_link(&self, link: PlanCreationLink) -> Result<(), StorageError> {
        super::validate_record_id(&link.creation_session_id)?;
        super::validate_record_id(&link.plan_id)?;
        let session = self.session_ledger_path(&link.creation_session_id);
        let plan = self.plan_ledger_path(&link.plan_id);
        let _guard = APPEND_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if session.exists() {
            if plan.exists() {
                merge_session_ledger(&session, &plan)?;
            } else {
                let parent = plan.parent().expect("plan ledger path has a parent");
                std::fs::create_dir_all(parent)?;
                std::fs::rename(&session, &plan)?;
            }
        }
        if plan.exists() {
            append_line_locked(&plan, &LedgerLine::Link(link))?;
        }
        Ok(())
    }

    /// Every readable line of one month's ledger, in file order.
    pub fn read_month(&self, year: i32, month: u32) -> Result<Vec<LedgerLine>, StorageError> {
        read_ledger_file(
            &self
                .dir
                .join(ledger_file_name(&format!("{year:04}-{month:02}"))),
        )
    }

    /// Every readable line from current per-plan/session ledgers and legacy
    /// monthly ledgers. New records are never written to the legacy files.
    pub fn read_all(&self) -> Result<Vec<LedgerLine>, StorageError> {
        let mut files = Vec::new();
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        for entry in std::fs::read_dir(&self.dir)? {
            let Ok(entry) = entry else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(LEGACY_LEDGER_FILE_PREFIX)
                && name.ends_with(LEDGER_FILE_SUFFIX)
                && entry.file_type().is_ok_and(|kind| kind.is_file())
            {
                files.push(entry.path());
            }
        }
        files.extend(ledger_files_in(&self.dir.join(PLANS_SUBDIR))?);
        let tools = self.dir.join(TOOLS_LEDGER_FILE);
        if tools.is_file() {
            files.push(tools);
        }
        files.sort();
        let mut lines = Vec::new();
        for file in files {
            lines.extend(read_ledger_file(&file)?);
        }
        // A crash during a session merge can leave a call in two files.
        let mut seen = HashSet::new();
        lines.retain(|line| match line {
            LedgerLine::Call(entry) => seen.insert(entry.call_id.clone()),
            LedgerLine::Link(_) => true,
        });
        Ok(lines)
    }

    /// Every call made during the given run, in ledger order.
    pub fn calls_for_run(&self, run_id: &str) -> Result<Vec<LlmUsageEntry>, StorageError> {
        Ok(calls(self.read_all()?)
            .filter(|entry| {
                matches!(&entry.context, UsageContext::PlanRun { run_id: id, .. } if id == run_id)
            })
            .collect())
    }

    pub fn run_total(&self, run_id: &str) -> Result<TokenTotals, StorageError> {
        Ok(TokenTotals::from_entries(&self.calls_for_run(run_id)?))
    }

    /// Creation calls of a plan: every call of a session linked to the plan,
    /// plus creation calls that name the plan directly (e.g. edits).
    pub fn plan_creation_calls(&self, plan_id: &str) -> Result<Vec<LlmUsageEntry>, StorageError> {
        let lines = self.read_all()?;
        let sessions = linked_sessions(&lines, plan_id);
        Ok(calls(lines)
            .filter(|entry| is_creation_call_for(entry, plan_id, &sessions))
            .collect())
    }

    pub fn plan_creation_total(&self, plan_id: &str) -> Result<TokenTotals, StorageError> {
        Ok(TokenTotals::from_entries(
            &self.plan_creation_calls(plan_id)?,
        ))
    }

    /// Every call attributable to a plan: creation, runs, and support work.
    pub fn calls_for_plan(&self, plan_id: &str) -> Result<Vec<LlmUsageEntry>, StorageError> {
        let lines = self.read_all()?;
        let sessions = linked_sessions(&lines, plan_id);
        Ok(calls(lines)
            .filter(|entry| {
                is_creation_call_for(entry, plan_id, &sessions)
                    || match &entry.context {
                        UsageContext::PlanRun { plan_id: id, .. } => id == plan_id,
                        UsageContext::PlanSupport { plan_id: id, .. } => {
                            id.as_deref() == Some(plan_id)
                        }
                        UsageContext::PlanCreation { .. } => false,
                    }
            })
            .collect())
    }

    /// Verbatim response body of a recorded call, if one was stored. New
    /// records keep it inline; legacy records fall back to their sidecar file.
    pub fn load_response_body(
        &self,
        call_id: &str,
    ) -> Result<Option<serde_json::Value>, StorageError> {
        self.load_inline_body(call_id, |call| call.response_body.clone(), RESPONSES_SUBDIR)
    }

    /// Request body of a recorded call, if message storage kept one.
    pub fn load_request_body(
        &self,
        call_id: &str,
    ) -> Result<Option<serde_json::Value>, StorageError> {
        self.load_inline_body(call_id, |call| call.request_body.clone(), REQUESTS_SUBDIR)
    }

    fn load_inline_body(
        &self,
        call_id: &str,
        body: impl Fn(&ClaudeCall) -> Option<serde_json::Value>,
        legacy_subdir: &str,
    ) -> Result<Option<serde_json::Value>, StorageError> {
        super::validate_record_id(call_id)?;
        if let Some(entry) = calls(self.read_all()?).find(|entry| entry.call_id == call_id)
            && let Some(inline_body) = body(&entry.call)
        {
            return Ok(Some(inline_body));
        }
        let path = self
            .dir
            .join(legacy_subdir)
            .join(format!("{call_id}{BODY_FILE_SUFFIX}"));
        match std::fs::read_to_string(path) {
            Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn ledger_path_for_context(&self, context: &UsageContext) -> Result<PathBuf, StorageError> {
        match context {
            UsageContext::PlanCreation {
                creation_session_id: _,
                plan_id: Some(plan_id),
                ..
            } => {
                super::validate_record_id(plan_id)?;
                Ok(self.plan_ledger_path(plan_id))
            }
            UsageContext::PlanCreation {
                creation_session_id,
                plan_id: None,
                ..
            } => {
                super::validate_record_id(creation_session_id)?;
                Ok(self.session_ledger_path(creation_session_id))
            }
            UsageContext::PlanRun { plan_id, .. }
            | UsageContext::PlanSupport {
                plan_id: Some(plan_id),
                ..
            } => {
                super::validate_record_id(plan_id)?;
                Ok(self.plan_ledger_path(plan_id))
            }
            UsageContext::PlanSupport { plan_id: None, .. } => Ok(self.dir.join(TOOLS_LEDGER_FILE)),
        }
    }

    fn plan_ledger_path(&self, plan_id: &str) -> PathBuf {
        self.dir
            .join(PLANS_SUBDIR)
            .join(format!("{plan_id}{LEDGER_FILE_SUFFIX}"))
    }

    fn session_ledger_path(&self, session_id: &str) -> PathBuf {
        self.dir
            .join(PLANS_SUBDIR)
            .join(format!("{session_id}{LEDGER_FILE_SUFFIX}"))
    }

    /// Append one complete line with a single write. If a previous crash left
    /// a partial trailing line, a newline is written first so the new line is
    /// never glued onto it.
    fn append_line(&self, path: &Path, line: &LedgerLine) -> Result<(), StorageError> {
        let _guard = APPEND_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        append_line_locked(path, line)
    }
}

fn append_line_locked(path: &Path, line: &LedgerLine) -> Result<(), StorageError> {
    let mut text = serde_json::to_string(line)?;
    text.push('\n');
    let parent = path.parent().expect("ledger path has a parent");
    std::fs::create_dir_all(parent)?;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)?;
    if !ends_with_newline_or_empty(&mut file)? {
        text.insert(0, '\n');
    }
    file.write_all(text.as_bytes())?;
    file.flush()?;
    Ok(())
}

/// Fold a creation-session ledger into the plan ledger. The merged file is
/// swapped in atomically before the session file is removed, and calls the
/// plan ledger already holds are skipped, so a crash at any point loses
/// nothing and a retry cannot duplicate lines. Caller holds `APPEND_LOCK`.
fn merge_session_ledger(session: &Path, plan: &Path) -> Result<(), StorageError> {
    let mut merged = std::fs::read_to_string(plan)?;
    if !merged.is_empty() && !merged.ends_with('\n') {
        merged.push('\n');
    }
    let known: HashSet<String> = read_ledger_file(plan)?
        .into_iter()
        .filter_map(|line| match line {
            LedgerLine::Call(entry) => Some(entry.call_id),
            LedgerLine::Link(_) => None,
        })
        .collect();
    for line in read_ledger_file(session)? {
        if matches!(&line, LedgerLine::Call(entry) if known.contains(&entry.call_id)) {
            continue;
        }
        merged.push_str(&serde_json::to_string(&line)?);
        merged.push('\n');
    }
    super::write_atomically(plan, &merged)?;
    std::fs::remove_file(session)?;
    Ok(())
}

fn ledger_file_name(month: &str) -> String {
    format!("{LEGACY_LEDGER_FILE_PREFIX}{month}{LEDGER_FILE_SUFFIX}")
}

fn ledger_files_in(dir: &Path) -> Result<Vec<PathBuf>, StorageError> {
    let mut files = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(error) => return Err(error.into()),
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_file())
            && entry
                .file_name()
                .to_string_lossy()
                .ends_with(LEDGER_FILE_SUFFIX)
        {
            files.push(entry.path());
        }
    }
    Ok(files)
}

fn ends_with_newline_or_empty(file: &mut std::fs::File) -> Result<bool, std::io::Error> {
    if file.metadata()?.len() == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

/// Parse a ledger file, skipping blank, partial, and unrecognized lines.
fn read_ledger_file(path: &Path) -> Result<Vec<LedgerLine>, StorageError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut lines = Vec::new();
    for (index, text) in raw.lines().enumerate() {
        if text.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<LedgerLine>(text) {
            Ok(line) => lines.push(line),
            Err(_) => tracing::warn!(
                "storage.event" = "list_entry_skipped",
                "storage.store.kind" = STORE_KIND,
                "storage.entry.name" = %name,
                "storage.entry.line" = index + 1,
                "storage.skip.reason" = "record_corrupt",
                "storage ledger line skipped"
            ),
        }
    }
    Ok(lines)
}

fn calls(lines: Vec<LedgerLine>) -> impl Iterator<Item = LlmUsageEntry> {
    lines.into_iter().filter_map(|line| match line {
        LedgerLine::Call(entry) => Some(*entry),
        LedgerLine::Link(_) => None,
    })
}

fn linked_sessions(lines: &[LedgerLine], plan_id: &str) -> HashSet<String> {
    lines
        .iter()
        .filter_map(|line| match line {
            LedgerLine::Link(link) if link.plan_id == plan_id => {
                Some(link.creation_session_id.clone())
            }
            _ => None,
        })
        .collect()
}

fn is_creation_call_for(entry: &LlmUsageEntry, plan_id: &str, sessions: &HashSet<String>) -> bool {
    match &entry.context {
        UsageContext::PlanCreation {
            creation_session_id,
            plan_id: direct,
            ..
        } => sessions.contains(creation_session_id) || direct.as_deref() == Some(plan_id),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn usage(input: u64, cache_read: Option<u64>, output: u64) -> ClaudeUsage {
        ClaudeUsage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_creation_input_tokens: Some(0),
            cache_read_input_tokens: cache_read,
            cache_creation_5m_input_tokens: None,
            cache_creation_1h_input_tokens: None,
            thinking_tokens: Some(output / 2),
            web_search_requests: None,
            web_fetch_requests: None,
            service_tier: Some("standard".to_owned()),
            inference_geo: None,
            raw: serde_json::json!({"input_tokens": input, "output_tokens": output}),
        }
    }

    fn call(outcome: ClaudeCallOutcome, usage: Option<ClaudeUsage>) -> ClaudeCall {
        ClaudeCall {
            message_id: Some("msg_1".to_owned()),
            request_id: Some("req_1".to_owned()),
            model_requested: "claude-test".to_owned(),
            model_reported: Some("claude-test".to_owned()),
            http_status: Some(200),
            stop_reason: Some("end_turn".to_owned()),
            response_headers: IndexMap::from([("request-id".to_owned(), "req_1".to_owned())]),
            response_body: Some(serde_json::json!({"id": "msg_1", "content": []})),
            request_body: None,
            outcome,
            usage,
            latency_ms: 42,
            started_at: Utc::now(),
        }
    }

    fn creation(session: &str, phase: CreationPhase) -> UsageContext {
        UsageContext::PlanCreation {
            creation_session_id: session.to_owned(),
            phase,
            plan_id: None,
            plan_version: None,
            origin: UsageOrigin::Chat,
        }
    }

    fn run_step(run_id: &str, attempt: u32, iteration: Option<usize>) -> UsageContext {
        UsageContext::PlanRun {
            run_id: run_id.to_owned(),
            plan_id: "plan-a".to_owned(),
            plan_version: 1,
            step_id: "summarize".to_owned(),
            attempt,
            iteration,
        }
    }

    fn log() -> (tempfile::TempDir, LlmUsageLog) {
        let dir = tempfile::tempdir().unwrap();
        let log = LlmUsageLog::new(dir.path().join("llm-usage"));
        (dir, log)
    }

    #[test]
    fn creation_session_moves_into_one_plan_file_with_inline_bodies() {
        let (_dir, log) = log();
        let mut recorded = call(ClaudeCallOutcome::Ok, Some(usage(100, Some(50), 20)));
        recorded.request_body = Some(serde_json::json!({"messages": []}));
        let entry = log
            .append_call(creation("s1", CreationPhase::Assess), recorded.clone())
            .unwrap();

        assert!(entry.response_stored);
        assert!(entry.prompt_stored);
        assert_eq!(entry.call.response_body, recorded.response_body);
        assert_eq!(entry.call.request_body, recorded.request_body);
        assert!(log.session_ledger_path("s1").is_file());
        log.append_link(PlanCreationLink::new("s1", "plan-a", 1))
            .unwrap();
        assert!(!log.session_ledger_path("s1").exists());
        assert!(log.plan_ledger_path("plan-a").is_file());
        let lines = log.read_all().unwrap();
        assert_eq!(lines.len(), 2, "call plus its creation link");
        assert_eq!(
            log.load_response_body(&entry.call_id).unwrap(),
            recorded.response_body
        );
        assert_eq!(
            log.load_request_body(&entry.call_id).unwrap(),
            recorded.request_body
        );
    }

    #[test]
    fn link_merges_session_into_an_existing_plan_ledger() {
        let (_dir, log) = log();
        log.append_call(
            run_step("run-1", 1, None),
            call(ClaudeCallOutcome::Ok, Some(usage(1, None, 1))),
        )
        .unwrap();
        log.append_call(
            creation("s1", CreationPhase::Compile),
            call(ClaudeCallOutcome::Ok, Some(usage(2, None, 2))),
        )
        .unwrap();

        log.append_link(PlanCreationLink::new("s1", "plan-a", 1))
            .unwrap();

        assert!(!log.session_ledger_path("s1").exists());
        assert_eq!(log.plan_creation_total("plan-a").unwrap().calls, 1);
        assert_eq!(log.calls_for_plan("plan-a").unwrap().len(), 2);
        let leftovers: Vec<_> = std::fs::read_dir(log.dir.join(PLANS_SUBDIR))
            .unwrap()
            .flatten()
            .filter(|entry| {
                !entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(LEDGER_FILE_SUFFIX)
            })
            .collect();
        assert!(leftovers.is_empty(), "no temp files may remain");
    }

    #[test]
    fn interrupted_merge_is_neither_lost_nor_duplicated() {
        let (_dir, log) = log();
        log.append_call(
            run_step("run-1", 1, None),
            call(ClaudeCallOutcome::Ok, Some(usage(1, None, 1))),
        )
        .unwrap();
        log.append_call(
            creation("s1", CreationPhase::Compile),
            call(ClaudeCallOutcome::Ok, Some(usage(2, None, 2))),
        )
        .unwrap();
        // Crash after the merged plan ledger was swapped in, before the
        // session file was removed.
        let session = log.session_ledger_path("s1");
        let plan = log.plan_ledger_path("plan-a");
        let mut merged = std::fs::read_to_string(&plan).unwrap();
        merged.push_str(&std::fs::read_to_string(&session).unwrap());
        std::fs::write(&plan, merged).unwrap();

        assert_eq!(log.read_all().unwrap().len(), 2, "reads dedupe by call id");
        log.append_link(PlanCreationLink::new("s1", "plan-a", 1))
            .unwrap();

        assert!(!session.exists());
        assert_eq!(log.read_all().unwrap().len(), 3, "two calls plus the link");
        assert_eq!(
            std::fs::read_to_string(&plan).unwrap().lines().count(),
            3,
            "the retry must not append the call again"
        );
    }

    #[test]
    fn ledger_line_uses_flat_jq_friendly_shape() {
        let (_dir, log) = log();
        let _entry = log
            .append_call(
                run_step("run-1", 1, None),
                call(ClaudeCallOutcome::Ok, Some(usage(1, None, 1))),
            )
            .unwrap();
        let file = log.plan_ledger_path("plan-a");
        let line: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(file).unwrap().trim()).unwrap();
        assert_eq!(line["record"], "call");
        assert_eq!(line["context"]["scope"], "plan_run");
        assert_eq!(line["context"]["run_id"], "run-1");
        assert_eq!(line["call"]["usage"]["input_tokens"], 1);
        assert_eq!(line["call"]["outcome"]["kind"], "ok");
        assert_eq!(line["call"]["response_body"]["id"], "msg_1");
    }

    #[test]
    fn partial_trailing_line_is_skipped_and_not_glued_to_next_append() {
        let (_dir, log) = log();
        let _first = log
            .append_call(
                run_step("run-1", 1, None),
                call(ClaudeCallOutcome::Ok, Some(usage(10, None, 5))),
            )
            .unwrap();
        let file = log.plan_ledger_path("plan-a");
        let mut handle = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        handle.write_all(b"{\"record\":\"call\",\"sch").unwrap();
        drop(handle);

        assert_eq!(log.read_all().unwrap().len(), 1);
        log.append_call(
            run_step("run-1", 2, None),
            call(ClaudeCallOutcome::Ok, Some(usage(20, None, 5))),
        )
        .unwrap();
        let totals = log.run_total("run-1").unwrap();
        assert_eq!(totals.calls, 2);
        assert_eq!(totals.input, 30);
    }

    #[test]
    fn totals_sum_known_usage_and_count_unknown_calls() {
        let (_dir, log) = log();
        log.append_call(
            run_step("run-1", 1, None),
            call(ClaudeCallOutcome::Ok, Some(usage(100, Some(40), 10))),
        )
        .unwrap();
        log.append_call(
            run_step("run-1", 2, None),
            call(ClaudeCallOutcome::NoTextBlock, Some(usage(100, None, 30))),
        )
        .unwrap();
        log.append_call(
            run_step("run-1", 3, None),
            call(ClaudeCallOutcome::TransportError, None),
        )
        .unwrap();
        log.append_call(
            run_step("run-1", 4, None),
            call(ClaudeCallOutcome::HttpError { status: 529 }, None),
        )
        .unwrap();
        log.append_call(
            run_step("run-other", 1, None),
            call(ClaudeCallOutcome::Ok, Some(usage(999, None, 999))),
        )
        .unwrap();

        let totals = log.run_total("run-1").unwrap();
        assert_eq!(
            totals,
            TokenTotals {
                calls: 4,
                input: 200,
                cache_creation: 0,
                cache_read: 40,
                output: 40,
                total: 280,
                unknown_calls: 1,
            }
        );
        assert_eq!(totals.total_input(), 240);
    }

    #[test]
    fn missing_output_count_is_unknown_not_zero() {
        let mut partial = usage(10, None, 0);
        partial.output_tokens = None;
        let mut totals = TokenTotals::default();
        totals.add(&ClaudeCallOutcome::Ok, Some(&partial));
        assert_eq!(totals.unknown_calls, 1);
        assert_eq!(totals.total, 0);
        assert_eq!(totals.input, 0, "partial usage must not leak into sums");
    }

    #[test]
    fn plan_creation_total_joins_sessions_through_links() {
        let (_dir, log) = log();
        log.append_call(
            creation("s1", CreationPhase::Compile),
            call(ClaudeCallOutcome::Ok, Some(usage(100, None, 10))),
        )
        .unwrap();
        log.append_call(
            creation("s1", CreationPhase::CompileRegenerate),
            call(ClaudeCallOutcome::Ok, Some(usage(200, None, 20))),
        )
        .unwrap();
        log.append_call(
            creation("s-unlinked", CreationPhase::Assess),
            call(ClaudeCallOutcome::Ok, Some(usage(5000, None, 5))),
        )
        .unwrap();
        log.append_call(
            UsageContext::PlanCreation {
                creation_session_id: "s-edit".to_owned(),
                phase: CreationPhase::Edit,
                plan_id: Some("plan-a".to_owned()),
                plan_version: Some(1),
                origin: UsageOrigin::Mcp,
            },
            call(ClaudeCallOutcome::Ok, Some(usage(3, None, 1))),
        )
        .unwrap();
        log.append_link(PlanCreationLink::new("s1", "plan-a", 1))
            .unwrap();

        let totals = log.plan_creation_total("plan-a").unwrap();
        assert_eq!(totals.calls, 3);
        assert_eq!(totals.total, 110 + 220 + 4);
        assert!(log.plan_creation_total("plan-b").unwrap().is_empty());
    }

    #[test]
    fn calls_for_plan_include_runs_and_support_work() {
        let (_dir, log) = log();
        log.append_call(
            creation("s1", CreationPhase::Design),
            call(ClaudeCallOutcome::Ok, Some(usage(1, None, 1))),
        )
        .unwrap();
        log.append_link(PlanCreationLink::new("s1", "plan-a", 1))
            .unwrap();
        log.append_call(
            run_step("run-1", 1, Some(0)),
            call(ClaudeCallOutcome::Ok, Some(usage(1, None, 1))),
        )
        .unwrap();
        log.append_call(
            UsageContext::PlanSupport {
                kind: SupportKind::ScheduleCron,
                plan_id: Some("plan-a".to_owned()),
            },
            call(ClaudeCallOutcome::Ok, Some(usage(1, None, 1))),
        )
        .unwrap();
        log.append_call(
            UsageContext::PlanSupport {
                kind: SupportKind::ToolSynthesis,
                plan_id: None,
            },
            call(ClaudeCallOutcome::Ok, Some(usage(1, None, 1))),
        )
        .unwrap();

        assert_eq!(log.calls_for_plan("plan-a").unwrap().len(), 3);
        let runs = log.calls_for_run("run-1").unwrap();
        let summary = runs[0].run_summary().unwrap();
        assert_eq!(summary.attempt, 1);
        assert_eq!(summary.iteration, Some(0));
    }

    #[test]
    fn unknown_lines_and_phases_do_not_break_reading() {
        let (_dir, log) = log();
        let entry = log
            .append_call(
                creation("s1", CreationPhase::Assess),
                call(ClaudeCallOutcome::Ok, Some(usage(1, None, 1))),
            )
            .unwrap();
        let file = log.session_ledger_path("s1");
        let mut raw = std::fs::read_to_string(&file).unwrap();
        raw.push_str("{\"record\":\"future_thing\",\"x\":1}\n");
        let future_phase = raw
            .lines()
            .next()
            .unwrap()
            .replace("\"assess\"", "\"plan_review\"")
            .replace(&entry.call_id, "future-call");
        raw.push_str(&future_phase);
        raw.push('\n');
        std::fs::write(&file, raw).unwrap();

        let lines = log.read_all().unwrap();
        assert_eq!(lines.len(), 2);
        let LedgerLine::Call(future) = &lines[1] else {
            panic!("expected a call line");
        };
        assert!(matches!(
            future.context,
            UsageContext::PlanCreation {
                phase: CreationPhase::Unknown,
                ..
            }
        ));
    }

    #[test]
    fn legacy_monthly_entry_loads_its_response_sidecar() {
        let (_dir, log) = log();
        let call_id = "legacy-call";
        let response_body = serde_json::json!({"content": [{"text": "legacy"}]});
        let mut legacy_call = call(ClaudeCallOutcome::Ok, Some(usage(1, None, 1)));
        legacy_call.response_body = None;
        let entry = LlmUsageEntry {
            schema: 1,
            call_id: call_id.to_owned(),
            provider: ANTHROPIC_PROVIDER.to_owned(),
            context: run_step("run-legacy", 1, None),
            prompt_stored: false,
            response_stored: true,
            call: legacy_call,
        };
        std::fs::create_dir_all(log.dir.join(RESPONSES_SUBDIR)).unwrap();
        std::fs::write(
            log.dir
                .join(RESPONSES_SUBDIR)
                .join(format!("{call_id}{BODY_FILE_SUFFIX}")),
            serde_json::to_string(&response_body).unwrap(),
        )
        .unwrap();
        std::fs::write(
            log.dir.join(ledger_file_name("2026-10")),
            format!(
                "{}\n",
                serde_json::to_string(&LedgerLine::Call(Box::new(entry))).unwrap()
            ),
        )
        .unwrap();

        assert_eq!(
            log.load_response_body(call_id).unwrap(),
            Some(response_body)
        );
    }

    #[test]
    fn empty_ledger_reads_as_nothing() {
        let (_dir, log) = log();
        assert!(log.read_all().unwrap().is_empty());
        assert!(log.read_month(2026, 1).unwrap().is_empty());
        assert!(log.run_total("run-1").unwrap().is_empty());
        assert_eq!(log.load_response_body("missing").unwrap(), None);
    }
}
