//! Step-runner dispatch and supporting types.
//!
//! `run_step` is the single entry point: it owns the `StepContext`, selects
//! the appropriate runner by step type, and returns a boxed future.  The
//! boxing is necessary because `FAN_OUT` runners call back into `run_step`
//! for spawn steps, creating a recursive async call graph; boxing breaks the
//! infinite-type cycle at compile time.

pub mod agent_call;
pub mod code_call;
pub mod condition;
pub mod fan;
pub mod human;
pub mod prompt_call;
pub mod tool_call;

use crate::error::ExecutorError;
use crate::plan::types::{Plan, PlanStep, StepType};
use crate::storage::runs::StepRunIteration;
use crate::tools::catalog::ToolCatalog;
use indexmap::IndexMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

// ─── Shared constants ─────────────────────────────────────────────────────────

/// Synthetic `step_outputs` key under which the FAN_OUT runner injects the
/// current iteration's item, making `${item.*}` placeholders resolvable
/// inside spawn steps. Double-underscored so it can never collide with a
/// real step id.
pub(crate) const ITEM_SCOPE_KEY: &str = "__item__";

/// Canonical response value recorded when an operator approves a
/// HUMAN_INTERACTION step. CONDITION evaluation treats it as equivalent to
/// boolean `true`.
pub(crate) const APPROVED_RESPONSE: &str = "approved";

// ─── Context and result types ─────────────────────────────────────────────────

/// Everything a step runner needs to execute one step.
#[derive(Clone)]
pub struct StepContext {
    /// Shared, immutable during a run. `Arc` keeps the per-step (and
    /// per-fan-out-spawn) context clones O(1) instead of copying the whole
    /// plan each time.
    pub plan: std::sync::Arc<Plan>,
    pub step: PlanStep,
    /// Outputs of all previously completed steps, keyed by step_id → output_name → value.
    pub step_outputs: StepOutputs,
    pub catalog: ToolCatalog,
    /// Global per-step timeout; individual steps may override via `step.timeout_secs`.
    pub global_timeout_secs: Option<u64>,
    /// UI channel for HUMAN_INTERACTION prompts; stdin fallback when `None`.
    pub human: Option<tokio::sync::mpsc::UnboundedSender<crate::executor::HumanRequest>>,
    /// Run identity and progress stream used by nested FAN_OUT executions.
    pub run_id: String,
    pub progress: Option<tokio::sync::mpsc::UnboundedSender<crate::executor::ProgressEvent>>,
    pub child_progress: Option<tokio::sync::mpsc::UnboundedSender<ChildRunEvent>>,
    /// Credentials for PROMPT_CALL steps (env-var fallback when unset).
    pub llm_keys: crate::executor::LlmKeys,
    /// Root of the local data directory (default `.inxm/`), used to derive
    /// a per-run scratch workspace for CODE_CALL steps whose optional
    /// `root_directory` input was not supplied at runtime.
    pub storage_root: std::path::PathBuf,
    /// Executor-local transcript buffer used to persist partial output when
    /// an agent exits unsuccessfully or is killed by a timeout.
    pub(crate) agent_audit: Arc<std::sync::Mutex<(String, String)>>,
}

type NamedOutputs = IndexMap<String, serde_json::Value>;
type OutputMap = IndexMap<String, NamedOutputs>;

/// Read-only outputs available to a step.
///
/// Main-flow outputs live in one shared base. FAN_OUT adds only its current
/// item and completed body steps as Arc-backed overlays, so large upstream
/// payloads are never cloned per iteration or spawn step.
#[derive(Clone, Default)]
pub struct StepOutputs {
    base: Arc<OutputMap>,
    overlay: IndexMap<String, Arc<NamedOutputs>>,
}

impl StepOutputs {
    pub fn from_map(outputs: OutputMap) -> Self {
        Self {
            base: Arc::new(outputs),
            overlay: IndexMap::new(),
        }
    }

    pub fn get(&self, step_id: &str) -> Option<&NamedOutputs> {
        self.overlay
            .get(step_id)
            .map(Arc::as_ref)
            .or_else(|| self.base.get(step_id))
    }

    pub fn is_empty(&self) -> bool {
        self.base.is_empty() && self.overlay.is_empty()
    }

    pub fn with_output(&self, step_id: String, outputs: Arc<NamedOutputs>) -> Self {
        let mut overlay = self.overlay.clone();
        overlay.insert(step_id, outputs);
        Self {
            base: Arc::clone(&self.base),
            overlay,
        }
    }

    pub fn with_outputs(&self, outputs: &IndexMap<String, Arc<NamedOutputs>>) -> Self {
        let mut overlay = self.overlay.clone();
        overlay.extend(
            outputs
                .iter()
                .map(|(step_id, values)| (step_id.clone(), Arc::clone(values))),
        );
        Self {
            base: Arc::clone(&self.base),
            overlay,
        }
    }

    pub fn insert_base(&mut self, step_id: String, outputs: NamedOutputs) {
        Arc::make_mut(&mut self.base).insert(step_id, outputs);
    }

    fn entries(&self) -> impl Iterator<Item = (&String, &NamedOutputs)> {
        self.overlay
            .iter()
            .map(|(step_id, outputs)| (step_id, outputs.as_ref()))
            .chain(
                self.base
                    .iter()
                    .filter(|(step_id, _)| !self.overlay.contains_key(*step_id)),
            )
    }

    #[cfg(test)]
    fn shares_base_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.base, &other.base)
    }
}

impl From<OutputMap> for StepOutputs {
    fn from(outputs: OutputMap) -> Self {
        Self::from_map(outputs)
    }
}

pub trait StepOutputLookup {
    fn get_step_outputs(&self, step_id: &str) -> Option<&NamedOutputs>;
}

impl StepOutputLookup for StepOutputs {
    fn get_step_outputs(&self, step_id: &str) -> Option<&NamedOutputs> {
        self.get(step_id)
    }
}

impl StepOutputLookup for OutputMap {
    fn get_step_outputs(&self, step_id: &str) -> Option<&NamedOutputs> {
        self.get(step_id)
    }
}

/// A completed FAN_OUT child execution sent back to the owning executor.
#[derive(Debug)]
pub struct ChildRunEvent {
    pub step_id: String,
    pub status: crate::executor::StepRunStatus,
    pub run: StepRunIteration,
}

/// The successful result of executing a step.
pub struct StepResult {
    pub outputs: IndexMap<String, serde_json::Value>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    /// Tokens consumed by this step's LLM work, when reported by its provider.
    pub usage: Option<crate::storage::runs::TokenUsage>,
    /// Per-template executions produced by a FAN_OUT step.
    pub child_runs: IndexMap<String, Vec<StepRunIteration>>,
}

// ─── Dispatch ─────────────────────────────────────────────────────────────────

/// Dispatch to the appropriate runner for the step's type.
///
/// Returns a heap-allocated (`Box::pin`) future so that FAN_OUT runners can
/// recursively call this function for spawn steps without producing an
/// infinitely-sized future type.
pub fn run_step(
    ctx: StepContext,
) -> Pin<Box<dyn Future<Output = Result<StepResult, ExecutorError>> + Send>> {
    Box::pin(async move {
        // Fail fast on dangling references instead of silently substituting
        // null — a step that runs with missing inputs produces garbage that
        // is much harder to diagnose (and repair) than this error.
        let config_json = crate::plan::steps::runtime_preflight_config(&ctx.step);
        let missing = missing_placeholders(&config_json, &ctx.plan.config, &ctx.step_outputs);
        if !missing.is_empty() {
            return Err(ExecutorError::StepFailed {
                step_id: ctx.step.id.clone(),
                message: format!(
                    "unresolved placeholder(s): {} — value is null or does not exist. {} \
                     Repair the plan to reference an existing non-null output.",
                    missing.join(", "),
                    available_outputs_hint(&missing, &ctx.step_outputs),
                ),
            });
        }

        match ctx.step.step_type() {
            StepType::ToolCall => tool_call::run(&ctx).await,
            StepType::CodeCall => code_call::run(&ctx).await,
            StepType::HumanInteraction => human::run(&ctx).await,
            StepType::FanOut => fan::run_fan_out(&ctx).await,
            StepType::FanIn => fan::run_fan_in(&ctx).await,
            StepType::PromptCall => prompt_call::run(&ctx).await,
            StepType::Condition => condition::run(&ctx).await,
            StepType::AgentCall => agent_call::run(&ctx).await,
        }
    })
}

// ─── Placeholder validation ───────────────────────────────────────────────────

/// `${input.*}`, `${conf.*}`, and `${step.*}` references in `value` that cannot be resolved
/// against the invocation config and outputs recorded so far. `${env.*}` and
/// `${item.*}` are runtime-scoped and not checked here.
pub fn missing_placeholders<L: StepOutputLookup + ?Sized>(
    value: &serde_json::Value,
    plan_config: &IndexMap<String, serde_json::Value>,
    step_outputs: &L,
) -> Vec<String> {
    let unresolvable = |key: &str| -> bool {
        if key.starts_with("input.") {
            return !plan_config.contains_key(key);
        }
        if let Some(conf_key) = key.strip_prefix("conf.") {
            return !plan_config.contains_key(conf_key);
        }
        if let Some(rest) = key.strip_prefix("step.") {
            return match rest.split_once('.') {
                Some((step_id, output)) => step_outputs
                    .get_step_outputs(step_id)
                    .is_none_or(|outs| outs.get(output).is_none_or(|v| v.is_null())),
                None => true,
            };
        }
        false
    };

    let keys = collect_placeholder_keys(value);
    let mut missing: Vec<String> = keys
        .into_iter()
        .filter(|key| unresolvable(key))
        .map(|key| format!("${{{key}}}"))
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

/// Ground truth for the error message (and the repair loop): what the steps
/// referenced by the dangling placeholders actually produced.
fn available_outputs_hint<L: StepOutputLookup + ?Sized>(
    missing: &[String],
    step_outputs: &L,
) -> String {
    let mut step_ids: Vec<&str> = missing
        .iter()
        .filter_map(|key| {
            key.strip_prefix("${step.")
                .and_then(|rest| rest.split_once('.'))
                .map(|(step_id, _)| step_id)
        })
        .collect();
    step_ids.sort_unstable();
    step_ids.dedup();

    let hints: Vec<String> = step_ids
        .iter()
        .map(|step_id| match step_outputs.get_step_outputs(step_id) {
            Some(outs) if !outs.is_empty() => format!(
                "Step '{step_id}' actually produced: {}.",
                outs.iter()
                    .map(|(k, v)| {
                        if v.is_null() {
                            format!("{k}=null")
                        } else {
                            k.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ => format!("Step '{step_id}' produced no named outputs."),
        })
        .collect();
    hints.join(" ")
}

fn collect_placeholder_keys(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(s) => placeholder_regex()
            .captures_iter(s)
            .map(|caps| caps[1].to_owned())
            .collect(),
        serde_json::Value::Object(map) => map.values().flat_map(collect_placeholder_keys).collect(),
        serde_json::Value::Array(items) => {
            items.iter().flat_map(collect_placeholder_keys).collect()
        }
        _ => Vec::new(),
    }
}

// ─── Placeholder resolution ───────────────────────────────────────────────────

/// Resolve all `${…}` placeholders in a JSON value, recursing through
/// objects and arrays.
///
/// Supported patterns inside `${…}`:
///
/// | Pattern                     | Resolves to                              |
/// |-----------------------------|------------------------------------------|
/// | `input.name`               | supplied invocation input                  |
/// | `conf.key`                  | `plan_config["key"]`                     |
/// | `step.step-id.output-name`  | `step_outputs["step-id"]["output-name"]` |
/// | `env.VAR`                   | `std::env::var("VAR")`                   |
/// | `item.var`                  | `step_outputs["__item__"]["var"]`        |
///
/// **Whole-string rule**: if the *entire* string value is a single placeholder
/// (e.g. `"${conf.count}"`) the native JSON type is returned rather than a
/// string.  For partial matches (e.g. `"prefix-${conf.id}"`) every placeholder
/// is replaced with its string representation.
pub fn resolve_placeholders<L: StepOutputLookup + ?Sized>(
    value: &serde_json::Value,
    plan_config: &IndexMap<String, serde_json::Value>,
    step_outputs: &L,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => resolve_string(s, plan_config, step_outputs),
        serde_json::Value::Object(obj) => {
            let resolved: serde_json::Map<String, serde_json::Value> = obj
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        resolve_placeholders(v, plan_config, step_outputs),
                    )
                })
                .collect();
            serde_json::Value::Object(resolved)
        }
        serde_json::Value::Array(arr) => {
            let resolved: Vec<serde_json::Value> = arr
                .iter()
                .map(|v| resolve_placeholders(v, plan_config, step_outputs))
                .collect();
            serde_json::Value::Array(resolved)
        }
        other => other.clone(),
    }
}

// ─── Private helpers ──────────────────────────────────────────────────────────

fn placeholder_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"\$\{([^}]+)\}").expect("placeholder regex is always valid")
    })
}

/// The namespaces reserved for plan placeholders. Any `${...}` whose content
/// does not start with one of these is the embedded script's own syntax
/// (bash parameter expansion, JavaScript template literals) and is passed
/// through verbatim rather than resolved.
fn is_plan_placeholder_key(key: &str) -> bool {
    ["input.", "conf.", "step.", "env.", "item."]
        .iter()
        .any(|ns| key.starts_with(ns))
}

pub(super) fn contains_plan_placeholder(value: &str) -> bool {
    placeholder_regex()
        .captures_iter(value)
        .any(|caps| is_plan_placeholder_key(&caps[1]))
}

fn resolve_string<L: StepOutputLookup + ?Sized>(
    s: &str,
    plan_config: &IndexMap<String, serde_json::Value>,
    step_outputs: &L,
) -> serde_json::Value {
    let re = placeholder_regex();

    // Whole-string case: return the native JSON type instead of a string.
    if let Some(cap) = re.captures(s)
        && cap.get(0).map(|m| m.as_str()) == Some(s)
        && is_plan_placeholder_key(&cap[1])
    {
        return lookup(&cap[1], plan_config, step_outputs);
    }

    // Partial case: replace each plan placeholder with its string
    // representation; non-plan `${...}` (script syntax) stays verbatim.
    // When the template is a URL, values landing in the query component are
    // percent-encoded so an input value cannot inject extra query parameters.
    // Host/path positions stay raw so templates like
    // `http://${conf.host}/api` keep working.
    let url_template = is_url_template(s);
    let mut replaced = String::with_capacity(s.len());
    let mut url_component = UrlComponent::Path;
    let mut last = 0;
    for caps in re.captures_iter(s) {
        let matched = caps.get(0).expect("capture 0 always exists");
        let literal = &s[last..matched.start()];
        url_component = advance_url_component(url_component, literal);
        replaced.push_str(literal);
        if is_plan_placeholder_key(&caps[1]) {
            let value = value_to_string(lookup(&caps[1], plan_config, step_outputs));
            if url_template && url_component == UrlComponent::Query {
                replaced.push_str(&percent_encode_component(&value));
            } else {
                url_component = advance_url_component(url_component, &value);
                replaced.push_str(&value);
            }
        } else {
            replaced.push_str(matched.as_str());
            url_component = advance_url_component(url_component, matched.as_str());
        }
        last = matched.end();
    }
    replaced.push_str(&s[last..]);
    serde_json::Value::String(replaced)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UrlComponent {
    Path,
    Query,
    Fragment,
}

/// Advance through delimiters in a URL segment. A fragment is terminal: a `?`
/// inside it is fragment data and must not re-enable query encoding.
fn advance_url_component(mut component: UrlComponent, segment: &str) -> UrlComponent {
    for byte in segment.bytes() {
        match byte {
            b'#' => return UrlComponent::Fragment,
            b'?' if component == UrlComponent::Path => component = UrlComponent::Query,
            _ => {}
        }
    }
    component
}

/// A template string that will be consumed as a URL: an http(s) scheme and
/// no whitespace anywhere in the template.
fn is_url_template(s: &str) -> bool {
    let lower = s.get(..8).map(str::to_ascii_lowercase).unwrap_or_default();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && !s.chars().any(char::is_whitespace)
}

/// RFC 3986 unreserved-only percent encoding, for values interpolated into a
/// URL query component.
fn percent_encode_component(value: &str) -> String {
    const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

fn lookup<L: StepOutputLookup + ?Sized>(
    key: &str,
    plan_config: &IndexMap<String, serde_json::Value>,
    step_outputs: &L,
) -> serde_json::Value {
    // Runtime inputs are injected under their fully-qualified placeholder key.
    if key.starts_with("input.") {
        return plan_config
            .get(key)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
    }

    // ${conf.key}
    if let Some(conf_key) = key.strip_prefix("conf.") {
        return plan_config
            .get(conf_key)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
    }

    // ${step.step-id.output-name}  (step-id must not contain dots)
    if let Some(rest) = key.strip_prefix("step.") {
        return match rest.split_once('.') {
            Some((step_id, output_name)) => step_outputs
                .get_step_outputs(step_id)
                .and_then(|outs| outs.get(output_name))
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        };
    }

    // ${env.VAR}
    if let Some(var_name) = key.strip_prefix("env.") {
        return serde_json::Value::String(std::env::var(var_name).unwrap_or_default());
    }

    // ${item.var}  — injected by the FAN_OUT runner per iteration
    if let Some(item_key) = key.strip_prefix("item.") {
        return step_outputs
            .get_step_outputs(ITEM_SCOPE_KEY)
            .and_then(|item| item.get(item_key))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
    }

    serde_json::Value::Null
}

fn value_to_string(v: serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s,
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Resolve placeholders in a runtime-configurable string field and flatten
/// the result back to a string (non-string JSON values keep their compact
/// JSON representation; note `null` renders as `"null"` here, unlike the
/// partial-substitution rule inside [`resolve_placeholders`]).
///
/// Shared by the CODE_CALL, PROMPT_CALL, and HUMAN_INTERACTION runners.
pub(super) fn resolve_to_string(value: &str, ctx: &StepContext) -> String {
    match resolve_placeholders(
        &serde_json::Value::String(value.to_owned()),
        &ctx.plan.config,
        &ctx.step_outputs,
    ) {
        serde_json::Value::String(resolved) => resolved,
        other => other.to_string(),
    }
}

/// Like [`resolve_to_string`], but a value that resolves entirely to JSON
/// `null` becomes `None` instead of the string `"null"`. Use this for
/// optional fields (e.g. a CODE_CALL `working_dir` of
/// `"${input.root_directory}"` with no input supplied) where "unset" must
/// fall through to the caller's default rather than become a bogus literal.
pub(super) fn resolve_to_optional_string(value: &str, ctx: &StepContext) -> Option<String> {
    match resolve_placeholders(
        &serde_json::Value::String(value.to_owned()),
        &ctx.plan.config,
        &ctx.step_outputs,
    ) {
        serde_json::Value::Null => None,
        serde_json::Value::String(resolved) => Some(resolved),
        other => Some(other.to_string()),
    }
}

/// `None` for an empty captured stream, so empty stdout/stderr are stored as
/// absent rather than as empty strings.
pub(super) fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use serde_json::json;

    fn cfg(pairs: &[(&str, serde_json::Value)]) -> IndexMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn outs(
        pairs: &[(&str, &[(&str, serde_json::Value)])],
    ) -> IndexMap<String, IndexMap<String, serde_json::Value>> {
        pairs
            .iter()
            .map(|(step_id, o)| {
                let m = o.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
                (step_id.to_string(), m)
            })
            .collect()
    }

    #[test]
    fn no_placeholders_unchanged() {
        let v = json!("hello world");
        assert_eq!(resolve_placeholders(&v, &cfg(&[]), &outs(&[])), v);
    }

    #[test]
    fn conf_partial_substitution() {
        let v = json!("hello ${conf.name}");
        let result = resolve_placeholders(&v, &cfg(&[("name", json!("alice"))]), &outs(&[]));
        assert_eq!(result, json!("hello alice"));
    }

    #[test]
    fn conf_whole_string_returns_native_type() {
        let v = json!("${conf.count}");
        let result = resolve_placeholders(&v, &cfg(&[("count", json!(42))]), &outs(&[]));
        assert_eq!(result, json!(42));
    }

    #[test]
    fn conf_whole_string_bool_native_type() {
        let v = json!("${conf.flag}");
        let result = resolve_placeholders(&v, &cfg(&[("flag", json!(true))]), &outs(&[]));
        assert_eq!(result, json!(true));
    }

    #[test]
    fn step_output_whole_string() {
        let v = json!("${step.fetch.url}");
        let result = resolve_placeholders(
            &v,
            &cfg(&[]),
            &outs(&[("fetch", &[("url", json!("https://example.com"))])]),
        );
        assert_eq!(result, json!("https://example.com"));
    }

    #[test]
    fn step_output_partial_substitution() {
        let v = json!("got: ${step.s1.val}");
        let result = resolve_placeholders(&v, &cfg(&[]), &outs(&[("s1", &[("val", json!("ok"))])]));
        assert_eq!(result, json!("got: ok"));
    }

    #[test]
    fn non_plan_placeholder_passes_through_verbatim() {
        let v = json!("datetime=${BODY#*x}; echo \"${datetime}\" ${step.s1.val}");
        let result = resolve_placeholders(&v, &cfg(&[]), &outs(&[("s1", &[("val", json!("ok"))])]));
        assert_eq!(
            result,
            json!("datetime=${BODY#*x}; echo \"${datetime}\" ok")
        );
    }

    #[test]
    fn whole_string_non_plan_placeholder_stays_a_string() {
        let v = json!("${HOME}");
        let result = resolve_placeholders(&v, &cfg(&[]), &outs(&[]));
        assert_eq!(result, json!("${HOME}"));
    }

    #[test]
    fn missing_conf_whole_string_is_null() {
        let v = json!("${conf.missing}");
        let result = resolve_placeholders(&v, &cfg(&[]), &outs(&[]));
        assert_eq!(result, serde_json::Value::Null);
    }

    #[test]
    fn missing_conf_partial_is_empty_string() {
        let v = json!("prefix-${conf.missing}");
        let result = resolve_placeholders(&v, &cfg(&[]), &outs(&[]));
        assert_eq!(result, json!("prefix-"));
    }

    #[test]
    fn nested_object_resolved() {
        let v = json!({ "url": "http://${conf.host}/api" });
        let result = resolve_placeholders(&v, &cfg(&[("host", json!("localhost"))]), &outs(&[]));
        assert_eq!(result, json!({ "url": "http://localhost/api" }));
    }

    #[test]
    fn array_elements_resolved() {
        let v = json!(["release-${conf.tag}", "${conf.tag}"]);
        let result = resolve_placeholders(&v, &cfg(&[("tag", json!("v1"))]), &outs(&[]));
        assert_eq!(result, json!(["release-v1", "v1"]));
    }

    #[test]
    fn non_string_values_pass_through() {
        assert_eq!(
            resolve_placeholders(&json!(123), &cfg(&[]), &outs(&[])),
            json!(123)
        );
        assert_eq!(
            resolve_placeholders(&json!(true), &cfg(&[]), &outs(&[])),
            json!(true)
        );
        assert_eq!(
            resolve_placeholders(&json!(null), &cfg(&[]), &outs(&[])),
            json!(null)
        );
    }

    #[test]
    fn item_placeholder_resolved() {
        let item_map: IndexMap<String, serde_json::Value> =
            [("name".to_string(), json!("bob"))].into_iter().collect();
        let mut step_outputs: IndexMap<String, IndexMap<String, serde_json::Value>> =
            IndexMap::new();
        step_outputs.insert(ITEM_SCOPE_KEY.to_string(), item_map);

        let v = json!("hello ${item.name}");
        let result = resolve_placeholders(&v, &cfg(&[]), &step_outputs);
        assert_eq!(result, json!("hello bob"));
    }

    #[test]
    fn multiple_placeholders_in_one_string() {
        let v = json!("${conf.a}-${conf.b}");
        let result = resolve_placeholders(
            &v,
            &cfg(&[("a", json!("foo")), ("b", json!("bar"))]),
            &outs(&[]),
        );
        assert_eq!(result, json!("foo-bar"));
    }

    #[test]
    fn url_query_values_are_percent_encoded_against_injection() {
        // An input value must not be able to inject extra
        // query parameters into an outbound URL.
        let v = json!("https://api.example.com/latest?base=${input.base}");
        let result = resolve_placeholders(
            &v,
            &cfg(&[("input.base", json!("EUR&symbols=USD HTTP/1.1"))]),
            &outs(&[]),
        );
        assert_eq!(
            result,
            json!("https://api.example.com/latest?base=EUR%26symbols%3DUSD%20HTTP%2F1.1")
        );
    }

    #[test]
    fn url_host_and_path_placeholders_stay_raw() {
        // Values before the query component keep working unescaped.
        let v = json!("http://${conf.host}/api?q=${conf.term}");
        let result = resolve_placeholders(
            &v,
            &cfg(&[("host", json!("example.com")), ("term", json!("a b"))]),
            &outs(&[]),
        );
        assert_eq!(result, json!("http://example.com/api?q=a%20b"));
    }

    #[test]
    fn url_fragment_placeholders_are_not_query_encoded() {
        let v = json!("https://example.com/?q=fixed#/${conf.route}?tab=${conf.tab}");
        let result = resolve_placeholders(
            &v,
            &cfg(&[("route", json!("users/42")), ("tab", json!("open items"))]),
            &outs(&[]),
        );

        assert_eq!(
            result,
            json!("https://example.com/?q=fixed#/users/42?tab=open items")
        );
    }

    #[test]
    fn non_url_strings_are_never_encoded() {
        // A bash snippet with `?` and `&` must pass through verbatim.
        let v = json!("echo ${conf.msg} && ls?");
        let result = resolve_placeholders(&v, &cfg(&[("msg", json!("hi&bye"))]), &outs(&[]));
        assert_eq!(result, json!("echo hi&bye && ls?"));
    }

    #[test]
    fn overlays_share_large_base_and_resolve_both_layers() {
        let large_payload = "x".repeat(1_000_000);
        let base = StepOutputs::from_map(IndexMap::from([(
            "fetch".to_owned(),
            IndexMap::from([("body".to_owned(), json!(large_payload))]),
        )]));
        let overlay = base.with_output(
            ITEM_SCOPE_KEY.to_owned(),
            std::sync::Arc::new(IndexMap::from([("name".to_owned(), json!("alice"))])),
        );

        assert!(
            base.shares_base_with(&overlay),
            "adding an overlay must retain the shared accumulated-output base"
        );
        assert_eq!(
            resolve_placeholders(&json!("${item.name}"), &cfg(&[]), &overlay),
            json!("alice")
        );
        assert_eq!(
            resolve_placeholders(&json!("${step.fetch.body}"), &cfg(&[]), &overlay)
                .as_str()
                .map(str::len),
            Some(1_000_000)
        );
    }
}

#[cfg(test)]
mod placeholder_validation_tests {
    use super::*;
    use serde_json::json;

    fn outputs_with(
        step: &str,
        field: &str,
    ) -> IndexMap<String, IndexMap<String, serde_json::Value>> {
        let mut inner = IndexMap::new();
        inner.insert(field.to_owned(), json!("value"));
        let mut outer = IndexMap::new();
        outer.insert(step.to_owned(), inner);
        outer
    }

    #[test]
    fn resolvable_references_pass() {
        let config = json!({ "type": "TOOL_CALL", "tool": "echo",
            "arguments": { "message": "${step.fetch.body} in ${conf.region}" } });
        let mut plan_config = IndexMap::new();
        plan_config.insert("region".to_owned(), json!("eu"));
        let missing = missing_placeholders(&config, &plan_config, &outputs_with("fetch", "body"));
        assert!(missing.is_empty(), "unexpected: {missing:?}");
    }

    #[test]
    fn dangling_step_output_is_reported() {
        let config = json!({ "arguments": { "message": "${step.fetch.content}" } });
        let missing =
            missing_placeholders(&config, &IndexMap::new(), &outputs_with("fetch", "body"));
        assert_eq!(missing, vec!["${step.fetch.content}".to_owned()]);
    }

    #[test]
    fn missing_conf_key_and_duplicates_deduped() {
        let config = json!(["${conf.nope}", "${conf.nope}", "${env.HOME}", "${item.x}"]);
        let missing = missing_placeholders(&config, &IndexMap::new(), &IndexMap::new());
        assert_eq!(missing, vec!["${conf.nope}".to_owned()]);
    }

    /// A step output key that exists but holds `null` must be flagged as
    /// unresolvable — a null URL (or any null required field) will fail
    /// downstream schema validation, and the repair loop needs this early
    /// signal to understand what went wrong.
    #[test]
    fn null_valued_step_output_is_reported_as_missing() {
        let mut inner = IndexMap::new();
        inner.insert("url".to_owned(), serde_json::Value::Null);
        let mut outputs: IndexMap<String, IndexMap<String, serde_json::Value>> = IndexMap::new();
        outputs.insert("get_location".to_owned(), inner);

        let config = json!({ "arguments": { "url": "${step.get_location.url}" } });
        let missing = missing_placeholders(&config, &IndexMap::new(), &outputs);
        assert_eq!(missing, vec!["${step.get_location.url}".to_owned()]);
    }

    /// A step output key that exists with a non-null value must NOT be flagged.
    #[test]
    fn non_null_step_output_is_not_reported() {
        let mut inner = IndexMap::new();
        inner.insert(
            "url".to_owned(),
            json!("https://api.weather.example/v1/forecast"),
        );
        let mut outputs: IndexMap<String, IndexMap<String, serde_json::Value>> = IndexMap::new();
        outputs.insert("get_location".to_owned(), inner);

        let config = json!({ "arguments": { "url": "${step.get_location.url}" } });
        let missing = missing_placeholders(&config, &IndexMap::new(), &outputs);
        assert!(
            missing.is_empty(),
            "non-null output must pass preflight: {missing:?}"
        );
    }

    /// The available-outputs hint must annotate null-valued keys so the repair
    /// loop can distinguish "step produced the key with null" from "step never
    /// produced the key at all".
    #[test]
    fn hint_annotates_null_outputs() {
        let mut inner = IndexMap::new();
        inner.insert("url".to_owned(), serde_json::Value::Null);
        inner.insert("status".to_owned(), json!(200));
        let mut outputs: IndexMap<String, IndexMap<String, serde_json::Value>> = IndexMap::new();
        outputs.insert("get_location".to_owned(), inner);

        let missing = vec!["${step.get_location.url}".to_owned()];
        let hint = super::available_outputs_hint(&missing, &outputs);
        assert!(
            hint.contains("url=null"),
            "hint must mark null output as null; got: {hint}"
        );
        assert!(
            hint.contains("status"),
            "hint must still list non-null outputs; got: {hint}"
        );
        assert!(
            !hint.contains("status=null"),
            "non-null output must not be annotated as null; got: {hint}"
        );
    }
}
