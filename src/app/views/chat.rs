//! Chat view — the primary surface. Natural language compiles plans; slash
//! commands drive everything else. Messages animate in, plan cards render
//! inline, and human-interaction steps pause here for an answer.

use egui::{Align, Id, Key, Layout, Modifiers, RichText, Ui};

use crate::compiler::{IntentAssessment, SolutionDesign, SpecDraft, SpecTurn};
use crate::executor::{
    AgentTranscriptEvent, AgentTranscriptStream, HumanDecision, HumanRequest, Run, RunStatus,
    StepRunStatus,
};
use crate::plan::types::Plan;
use crate::storage::patches::{Patch, PatchOperation, PatchStatus};
use crate::storage::plan_edits::PlanEdit;
use crate::tools::catalog::ToolEntry;

use crate::app::commands::{self, COMMANDS};
use crate::app::console::{CompileConsole, ConsoleStream};
use crate::app::engine::{
    EngineCommand, EngineHandle, PatchListItem, PlanListItem, RunListItem, ScheduleItem,
    SuggestedAction,
};
use crate::app::views::plan_card::{self, PlanCardAction, RunBinding, WorkspaceAction};
use crate::app::{anim, theme, time, widgets};

const PALETTE_MAX_ROWS: usize = 8;
/// Spec confidence required before the DESIGN phase unlocks (and, on the
/// very first assessment, before a simple prompt skips straight to a
/// compile).
pub const CONFIDENCE_THRESHOLD: f32 = 0.75;
const DESIGN_PANEL_DEFAULT_WIDTH: f32 = 340.0;
/// Keep enough of a plan-owned conversation visible that the resizable plan
/// workspace can never swallow the chat completely.
const CHAT_SPLIT_MIN_CHAT_WIDTH: f32 = 320.0;
const CHAT_SPLIT_MIN_PANEL_WIDTH: f32 = 240.0;
const DEFAULT_COMPOSER_HINT: &str = "Describe a plan, or type / for commands…";
/// Replaces the approve button while auto mode is on, so the DESIGN phase
/// still shows what is happening to the design on screen. Phrased as the
/// standing rule rather than a live progress report: a design restored from
/// disk (the `awaiting_compile` flag is transient) is compiled the same way,
/// by the next `DesignReady` its feedback produces.
const AUTO_MODE_DESIGN_STATUS: &str =
    "Auto mode is on — this design compiles into a plan without approval.";
const QUESTION_COMPOSER_HINT: &str = "Type your answer…";
const CONTINUE_COMPOSER_HINT: &str = "...or answer with more detail";
const HERO_SUBTEXT: &str = "Describe the work in plain language — it compiles into a \
deterministic plan you can inspect, run, and repair. Type / for commands.";
const HERO_SUGGESTIONS: &[&str] = &[
    "Fetch the current Bitcoin price and print it",
    "Get the current time in Tokyo and echo it",
    "Fetch example.com, summarize it in two sentences, ask me before writing it to a file",
    "Check git status and list all uncommitted changes",
    "Create a plan to add a new feature to a rust project, then run check, build, test, fmt and clippy and loop until all problems are fixed",
    "Build a reusable executive research dossier: accept an article-listing URL (the page containing the article links, not a general site homepage), and optional path prefix to filter articles under that path, a plain-language research topic (not another URL), maximum article count, output path, and root directory; fetch the listing; deterministically resolve, filter, deduplicate, and cap same-origin HTML article links under the listing URL's path prefix while rejecting site navigation, images, stylesheets, scripts, feeds, and other static assets; fan out to fetch each article and produce a compact evidence-backed summary with its source URL and risk signals; synthesize the summaries into a cross-source brief without sending raw pages to the final model call; ask for approval; then branch so approval writes the brief to disk while rejection emits a cancellation receipt",
    "Build a reusable release-readiness gate for this repository: accept the root directory, target branch, minimum coverage, and whether warnings are fatal; execute independent deterministic checks using git status and diff, cargo test, cargo clippy, and a git diff of Cargo dependency files (run the commands now - do not substitute pre-existing report files); turn their bounded evidence into compact structured findings and fan them in; use one bounded model call to return strict JSON with a risk score, blockers, and a GO or NO_GO decision; deterministically parse that JSON and branch on the decision; require human approval only on GO before writing a release report, and write a blocker report on NO_GO",
    "Build a reusable software-supply-chain compliance plan: accept a list of SBOM URLs, a policy object containing allowed licenses and forbidden packages, an output path, and a root directory; fan out over the bounded URL list so each SBOM is fetched, deterministically normalized, and semantically reviewed into compact JSON; aggregate all reviews, deterministically compute violations and a pass or fail verdict, and branch on it; for pass, write a signed attestation; for fail, produce a remediation dossier, ask a human whether to grant an exception, and write either a time-limited exception record or a rejection record",
];

// ─── State ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

// Chat holds at most a few hundred messages; variant size differences don't
// matter at that scale.
#[allow(clippy::large_enum_variant)]
pub enum MessageBody {
    Text(String),
    Error(String),
    /// A run has begun, with a direct route to its live details.
    RunStarted {
        run_id: String,
        text: String,
        active: bool,
    },
    /// A failed run with a one-click repair action.
    RunFailed {
        run_id: String,
        text: String,
        repair_requested: bool,
    },
    /// A completed non-failing run with a direct route to its outputs.
    RunCompleted {
        run_id: String,
        text: String,
    },
    Plan {
        plan: Box<Plan>,
        run: Option<RunBinding>,
    },
    Patch {
        patch: Box<Patch>,
        resolution: Option<String>,
        /// One-click resume of the patched run, sent once and then latched so
        /// the button can show a pending state (mirrors `RunFailed`'s
        /// `repair_requested`). Not persisted across sessions.
        resume_requested: bool,
    },
    /// An LLM-compiled `/edit` result, awaiting the user's approval before
    /// it becomes the plan's current version.
    Edit {
        edit: Box<PlanEdit>,
        resolution: Option<String>,
    },
    Human {
        prompt: String,
        approval_required: bool,
        responder: Option<tokio::sync::oneshot::Sender<HumanDecision>>,
        draft: String,
        resolution: Option<String>,
    },
    /// Live, lossless output from one AGENT_CALL step. Consecutive events for
    /// the same step are appended in arrival order so stdout and stderr remain
    /// auditable in the conversation rather than disappearing into run detail.
    AgentTranscript {
        run_id: String,
        step_id: String,
        lines: Vec<AgentTranscriptLine>,
    },
    /// A guided-flow elicitation: the assessment's clarifying question or
    /// the continue-to-design gate. Rendered like a HUMAN_INTERACTION card
    /// but with button actions only — every text reply goes through the ONE
    /// main composer (which re-assesses during refine). No run or responder
    /// channel exists; answers dispatch engine commands.
    FlowPrompt {
        kind: FlowPromptKind,
        prompt: String,
        resolution: Option<String>,
    },
    /// An anonymized support report is ready: a link opens GitHub's
    /// new-issue form prefilled with it, and the saved file lets the user
    /// review the exact content first.
    SupportTicket {
        issue_url: String,
        report_path: String,
        text: String,
    },
    PlanIndex(Vec<PlanListItem>),
    RunIndex(Vec<RunListItem>),
    ToolIndex(Vec<ToolEntry>),
    ScheduleIndex(Vec<ScheduleItem>),
    Help,
    /// An answer to a plain-text question, with an optional one-click
    /// follow-up action. Deliberately not counted by `awaiting_input` — an
    /// unresolved suggested action is a hint, never a blocking prompt.
    Insight {
        answer: String,
        action: Option<SuggestedAction>,
        resolution: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentTranscriptLine {
    pub stream: AgentTranscriptLineStream,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTranscriptLineStream {
    Input,
    Output,
    Error,
}

impl From<AgentTranscriptEvent> for AgentTranscriptLine {
    fn from(event: AgentTranscriptEvent) -> Self {
        Self {
            stream: match event.stream {
                AgentTranscriptStream::Stdin => AgentTranscriptLineStream::Input,
                AgentTranscriptStream::Stdout => AgentTranscriptLineStream::Output,
                AgentTranscriptStream::Stderr => AgentTranscriptLineStream::Error,
            },
            content: event.content,
        }
    }
}

/// An action the chat view wants the app shell to perform (navigation that
/// crosses views, which `chat::show` cannot do on its own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatViewAction {
    /// The `Plans ›` breadcrumb segment was clicked.
    GoToPlans,
    /// The user asked to schedule this plan — navigate to Schedules with it
    /// preselected.
    GoToSchedule(String),
}

pub struct ChatMessage {
    pub id: u64,
    pub role: Role,
    pub body: MessageBody,
}

/// State for the animated "busy" row shown while a slow, silent backend
/// call (compiling, repairing…) is in flight. Rendered as a collapsible row
/// so the detail — which can be verbose — stays out of the way by default.
pub struct BusyState {
    pub label: String,
    pub detail: Option<String>,
    pub started_at: std::time::Instant,
    /// Live compiler console streamed under this row while the backend call
    /// runs. Attached by the `CompileConsole` engine event that
    /// follows `CompileStarted`/`EditStarted`.
    pub console: Option<CompileConsole>,
}

impl BusyState {
    pub fn new(label: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            label: label.into(),
            detail,
            started_at: std::time::Instant::now(),
            console: None,
        }
    }
}

/// What a guided-flow elicitation is asking for.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowPromptKind {
    /// A clarifying question — answered through the main composer.
    Question,
    /// The spec passed the confidence threshold — the card's primary action
    /// continues to solution design; a composer reply refines further
    /// instead.
    ContinueGate,
}

/// Which phase of the guided create-a-plan flow a conversation is in.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowPhase {
    /// Clarifying the intent into a spec (chat + live spec card).
    Refine,
    /// Reviewing the solution design in the side panel.
    Design,
}

/// Per-conversation state of the guided flow (REFINE → DESIGN → COMPILE).
/// Lives entirely in the UI session — commands carry it fully so the engine
/// stays stateless — and is persisted with the chat so reopening the app
/// resumes where the user left off.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GuidedFlow {
    /// The original first message, kept verbatim for the eventual compile.
    pub intent: String,
    /// Full clarification history; the intent is the first user turn and
    /// every assistant question is appended as an assistant turn.
    pub conversation: Vec<SpecTurn>,
    pub assessment: Option<IntentAssessment>,
    pub design: Option<SolutionDesign>,
    pub phase: FlowPhase,
    /// Completed AssessIntent round-trips — turn 1 decides the simple-prompt
    /// fast path.
    #[serde(default)]
    pub assess_turns: u32,
    /// A GenerateDesign call is in flight (transient, not persisted).
    #[serde(skip)]
    pub design_pending: bool,
    /// A CompileFromSpec call is in flight (transient, not persisted).
    #[serde(skip)]
    pub awaiting_compile: bool,
}

impl GuidedFlow {
    pub fn new(intent: &str) -> Self {
        Self {
            intent: intent.to_owned(),
            conversation: vec![SpecTurn {
                role: "user".to_owned(),
                content: intent.to_owned(),
            }],
            assessment: None,
            design: None,
            phase: FlowPhase::Refine,
            assess_turns: 0,
            design_pending: false,
            awaiting_compile: false,
        }
    }

    /// Best-known spec — falls back to the raw intent before the first
    /// assessment has landed.
    pub fn spec(&self) -> SpecDraft {
        self.assessment
            .as_ref()
            .map(|assessment| assessment.spec.clone())
            .unwrap_or_else(|| SpecDraft {
                desired_outcome: self.intent.clone(),
                acceptance_criteria: Vec::new(),
                inputs: Vec::new(),
            })
    }

    /// True when a generated design is ready for the user to approve or
    /// revise. Pending generation and compilation are backend work instead.
    pub(crate) fn awaiting_design_approval(&self) -> bool {
        self.phase == FlowPhase::Design
            && self.design.is_some()
            && !self.design_pending
            && !self.awaiting_compile
    }
}

#[derive(Default)]
pub struct ChatState {
    pub input: String,
    pub messages: Vec<ChatMessage>,
    /// The single plan owned by this conversation. Plan cards are rendered in
    /// the fixed workspace header instead of being inserted into the transcript.
    pub plan: Option<Box<Plan>>,
    /// Active guided create-a-plan flow; cleared once the plan exists.
    pub flow: Option<GuidedFlow>,
    /// The run currently shown in the workspace header (live or inspected).
    pub workspace_run: Option<RunBinding>,
    /// Open the full-screen run inspection after its data has loaded.
    pub reveal_run_details: bool,
    next_id: u64,
    pub palette_index: usize,
    /// Shown with the typing indicator while a slow backend call is in
    /// flight (compiling, repairing…).
    pub busy: Option<BusyState>,
    /// Console of the most recent compile/edit, kept after the busy row is
    /// gone so failed or finished compiles stay inspectable.
    /// In-memory only — the persisted trace is the console's log file.
    pub last_console: Option<CompileConsole>,
    /// Set when a `/plans`, `/runs`, or `/tools` listing was requested from
    /// chat, so the next matching event is rendered here too.
    pub expect_plan_index: bool,
    pub expect_run_index: bool,
    pub expect_tool_index: bool,
    pub expect_schedule_index: bool,
    focus_input: bool,
    /// The command token argument suggestions were last fetched for.
    suggestions_fetched_for: Option<String>,
    /// True once ↑/↓ steered the palette — Enter then applies the selection.
    palette_navigated: bool,
}

impl ChatState {
    /// True for a freshly started conversation — no messages yet and no plan
    /// attached. Used to hide "new chat" affordances that would be a no-op.
    pub fn is_blank(&self) -> bool {
        self.messages.is_empty() && self.plan.is_none()
    }

    pub fn push(&mut self, role: Role, body: MessageBody) {
        self.next_id += 1;
        self.messages.push(ChatMessage {
            id: self.next_id,
            role,
            body,
        });
    }

    pub fn attach_plan(&mut self, plan: Box<Plan>) {
        let changed_plan = self
            .plan
            .as_ref()
            .is_some_and(|current| current.metadata.id != plan.metadata.id);
        self.plan = Some(plan);
        if changed_plan {
            self.workspace_run = None;
        }
    }

    pub fn plan_id(&self) -> Option<&str> {
        self.plan.as_ref().map(|plan| plan.metadata.id.as_str())
    }

    /// Resolve the failed run that `/repair` should target when no explicit
    /// run id was supplied. An inspected failed run is the strongest context;
    /// otherwise the latest failure recorded in this conversation wins.
    fn contextual_failed_run_id(&self, runs: &[RunListItem]) -> Option<String> {
        self.workspace_run
            .as_ref()
            .filter(|run| run.finished.as_ref().is_some_and(RunStatus::is_failed))
            .map(|run| run.run_id.clone())
            .or_else(|| {
                self.plan_id().and_then(|plan_id| {
                    runs.iter()
                        .filter(|run| run.plan_id == plan_id && run.status.is_failed())
                        .max_by_key(|run| run.started_at)
                        .map(|run| run.id.clone())
                })
            })
            .or_else(|| {
                self.messages
                    .iter()
                    .rev()
                    .find_map(|message| match &message.body {
                        MessageBody::RunFailed { run_id, .. } => Some(run_id.clone()),
                        _ => None,
                    })
            })
    }

    /// Resolve the run that `/inspect` should open when no explicit run id
    /// was supplied. The workspace is the user's current run context;
    /// otherwise use the newest persisted run for this chat's plan.
    fn contextual_run_id(&self, runs: &[RunListItem]) -> Option<String> {
        self.workspace_run
            .as_ref()
            .map(|run| run.run_id.clone())
            .or_else(|| {
                self.plan_id().and_then(|plan_id| {
                    runs.iter()
                        .filter(|run| run.plan_id == plan_id)
                        .max_by_key(|run| run.started_at)
                        .map(|run| run.id.clone())
                })
            })
            .or_else(|| {
                self.messages
                    .iter()
                    .rev()
                    .find_map(|message| match &message.body {
                        MessageBody::RunStarted { run_id, .. }
                        | MessageBody::RunFailed { run_id, .. }
                        | MessageBody::RunCompleted { run_id, .. } => Some(run_id.clone()),
                        _ => None,
                    })
            })
    }

    /// Attach or refresh the run binding on the most recent card for `plan`.
    /// Creates a new card when none exists yet.
    pub fn bind_run(&mut self, plan: &Plan, binding: RunBinding) {
        if self.plan_id() == Some(plan.metadata.id.as_str()) {
            self.workspace_run = Some(binding);
            return;
        }
        let existing = self
            .messages
            .iter_mut()
            .rev()
            .find_map(|m| match &mut m.body {
                MessageBody::Plan { plan: p, run }
                    if p.metadata.id == plan.metadata.id
                        && (run.is_none()
                            || run
                                .as_ref()
                                .is_some_and(|r| r.run_id == binding.run_id || !r.is_active())) =>
                {
                    Some(run)
                }
                _ => None,
            });
        match existing {
            Some(slot) => *slot = Some(binding),
            None => self.push(
                Role::Assistant,
                MessageBody::Plan {
                    plan: Box::new(plan.clone()),
                    run: Some(binding),
                },
            ),
        }
    }

    /// Find the live binding for `run_id` across all plan cards.
    pub fn binding_mut(&mut self, run_id: &str) -> Option<&mut RunBinding> {
        if self
            .workspace_run
            .as_ref()
            .is_some_and(|binding| binding.run_id == run_id)
        {
            return self.workspace_run.as_mut();
        }
        self.messages
            .iter_mut()
            .rev()
            .find_map(|m| match &mut m.body {
                MessageBody::Plan { run: Some(b), .. } if b.run_id == run_id => Some(b),
                _ => None,
            })
    }

    /// Fill a binding from a completed run record (authoritative state).
    pub fn apply_finished_run(&mut self, run: &Run) {
        if let Some(binding) = self.binding_mut(&run.id) {
            fill_binding(binding, run);
        }
        if let Some(message) = self.messages.iter_mut().rev().find(|message| {
            matches!(
                &message.body,
                MessageBody::RunStarted { run_id, active: true, .. } if run_id == &run.id
            )
        }) && let MessageBody::RunStarted { active, .. } = &mut message.body
        {
            *active = false;
        }
    }

    /// Close every still-open guided-flow prompt with `note`. Keeps the
    /// transcript at exactly one live reply affordance: called before a new
    /// prompt is pushed, when the composer answers instead of the inline
    /// field, and when the flow is no longer refining.
    pub fn resolve_open_flow_prompts(&mut self, note: &str) {
        for message in &mut self.messages {
            if let MessageBody::FlowPrompt { resolution, .. } = &mut message.body
                && resolution.is_none()
            {
                *resolution = Some(note.to_owned());
            }
        }
    }

    pub fn resolve_patch(&mut self, patch_id: &str, message: &str) {
        for m in self.messages.iter_mut().rev() {
            if let MessageBody::Patch {
                patch, resolution, ..
            } = &mut m.body
                && patch.id == patch_id
                && resolution.is_none()
            {
                *resolution = Some(message.to_owned());
                return;
            }
        }
    }

    pub fn resolve_edit(&mut self, edit_id: &str, message: &str) {
        for m in self.messages.iter_mut().rev() {
            if let MessageBody::Edit { edit, resolution } = &mut m.body
                && edit.id == edit_id
                && resolution.is_none()
            {
                *resolution = Some(message.to_owned());
                return;
            }
        }
    }

    pub fn append_agent_transcript(&mut self, event: AgentTranscriptEvent) {
        let target_run_id = event.run_id.clone();
        let target_step_id = event.step_id.clone();
        let line = AgentTranscriptLine::from(event);
        if let Some(message) = self.messages.last_mut()
            && let MessageBody::AgentTranscript {
                run_id,
                step_id,
                lines,
            } = &mut message.body
            && run_id == &target_run_id
            && step_id == &target_step_id
        {
            lines.push(line);
            return;
        }
        self.push(
            Role::Assistant,
            MessageBody::AgentTranscript {
                run_id: target_run_id,
                step_id: target_step_id,
                lines: vec![line],
            },
        );
    }

    /// True while the conversation has something pending your response — an
    /// open guided-flow prompt, a design awaiting approval, a pending patch,
    /// or a human-interaction card — so the status strip can show "waiting
    /// for you" rather than "idle".
    /// Message-backed cards use their unresolved state; guided design uses
    /// the same readiness conditions as its approve action.
    pub fn awaiting_input(&self) -> bool {
        self.flow
            .as_ref()
            .is_some_and(GuidedFlow::awaiting_design_approval)
            || self.messages.iter().rev().any(|m| match &m.body {
                MessageBody::FlowPrompt { resolution, .. }
                | MessageBody::Human { resolution, .. } => resolution.is_none(),
                MessageBody::Patch {
                    patch, resolution, ..
                } => resolution.is_none() && patch.status == PatchStatus::Pending,
                MessageBody::Edit { edit, resolution } => {
                    resolution.is_none() && edit.status == PatchStatus::Pending
                }
                _ => false,
            })
    }

    /// True while this conversation has backend work or a plan run in
    /// progress. Human-input waits are represented separately by
    /// `awaiting_input`, even though their run binding remains live.
    pub fn is_active(&self) -> bool {
        self.busy.is_some()
            || self.flow.as_ref().is_some_and(|flow| {
                // These flags are set synchronously by the UI actions, before
                // the engine's corresponding Started event can arrive.
                flow.design_pending || flow.awaiting_compile
            })
            || self
                .workspace_run
                .as_ref()
                .is_some_and(RunBinding::is_active)
            || self
                .messages
                .iter()
                .rev()
                .any(|message| match &message.body {
                    MessageBody::Plan { run: Some(run), .. } => run.is_active(),
                    MessageBody::RunStarted { active, .. } => *active,
                    _ => false,
                })
    }
}

// ─── View ─────────────────────────────────────────────────────────────────────

/// Session-wide collapsed state of the right-hand plan/design panel, shared
/// across conversations so the choice to reclaim the middle column sticks
/// while navigating between chats.
fn right_panel_collapsed(ctx: &egui::Context) -> bool {
    ctx.data_mut(|data| *data.get_temp_mut_or(Id::new("chat_right_panel_collapsed"), false))
}

fn set_right_panel_collapsed(ctx: &egui::Context, collapsed: bool) {
    ctx.data_mut(|data| data.insert_temp(Id::new("chat_right_panel_collapsed"), collapsed));
}

/// Right-aligned icon toggle for the plan/design side panel. Returns `true`
/// when the collapsed state changed this frame.
fn right_panel_toggle(ui: &mut Ui, collapsed: bool) -> bool {
    let hover_text = match collapsed {
        true => "Show side panel",
        false => "Hide side panel",
    };
    let clicked = widgets::ghost_icon_button(ui, widgets::Icon::PanelRight)
        .on_hover_text(hover_text)
        .clicked();
    if clicked {
        set_right_panel_collapsed(ui.ctx(), !collapsed);
    }
    clicked
}

/// The plan-workspace / solution-design sidebar, docked full height to the
/// window edge. Rendered at ctx level — before the central
/// panel — so it spans the whole window like a standard artifact sidebar
/// instead of sitting inset below the top bar. Returns the same navigation
/// actions as [`show`].
pub fn show_side_panel(
    ctx: &egui::Context,
    state: &mut ChatState,
    sources: &SuggestionSources,
    engine: &EngineHandle,
) -> Option<ChatViewAction> {
    let mut chat_action = None;
    let reveal_run_details = std::mem::take(&mut state.reveal_run_details);
    // DESIGN phase: the solution design opens next to the chat, which stays
    // interactive so further messages become design feedback.
    let design_panel_open = state.plan.is_none()
        && state
            .flow
            .as_ref()
            .is_some_and(|flow| flow.phase == FlowPhase::Design);
    if state.plan.is_none() && !design_panel_open {
        return None;
    }

    let panel_collapsed = right_panel_collapsed(ctx);
    // Both the workspace and the design panel share one dragged width, so
    // the sidebar keeps its size when the flow compiles into a plan.
    let width_id = Id::new("chat_right_panel_width");
    let max_panel_width =
        (ctx.available_rect().width() - CHAT_SPLIT_MIN_CHAT_WIDTH).max(CHAT_SPLIT_MIN_PANEL_WIDTH);
    let panel_width = widgets::panel_width(ctx, width_id, DESIGN_PANEL_DEFAULT_WIDTH)
        .clamp(CHAT_SPLIT_MIN_PANEL_WIDTH, max_panel_width);
    let panel_frame = egui::Frame::new()
        .fill(theme::panel())
        .stroke(egui::Stroke::new(1.0_f32, theme::divider()))
        .inner_margin(egui::Margin::symmetric(16, 10));

    let mut panel_rect = None;
    if let Some(plan) = state.plan.as_deref() {
        let workspace_id = Id::new("owned_plan_workspace").with(&plan.metadata.id);
        if reveal_run_details
            && let Some(run_id) = state
                .workspace_run
                .as_ref()
                .map(|binding| binding.run_id.clone())
        {
            // Keyed by the specific run being revealed, not a bare bool, so
            // this can never re-open the takeover for a run that is no
            // longer the one bound to this workspace (see plan_card.rs).
            ctx.data_mut(|data| data.insert_temp(workspace_id.with("workspace_expanded"), run_id));
        }
        let plan_runs: Vec<RunListItem> = sources
            .runs
            .iter()
            .filter(|run| run.plan_id == plan.metadata.id)
            .cloned()
            .collect();
        let response = egui::SidePanel::right("chat_plan_workspace")
            .resizable(false)
            .exact_width(panel_width)
            .frame(panel_frame)
            .show_animated(ctx, !panel_collapsed, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        right_panel_toggle(ui, false);
                    });
                });
                anim::entrance(ui, workspace_id, 0.0, |ui| {
                    plan_card::show_workspace(
                        ui,
                        workspace_id,
                        plan,
                        state.workspace_run.as_ref(),
                        &plan_runs,
                    )
                })
            });
        panel_rect = response.as_ref().map(|inner| inner.response.rect);
        let workspace_action = response.and_then(|inner| inner.inner);
        if let Some(action) = workspace_action {
            match action {
                WorkspaceAction::Plan(PlanCardAction::Run { plan_id, inputs }) => {
                    engine.send(EngineCommand::RunPlan {
                        plan_ref: plan_id,
                        inputs,
                    })
                }
                WorkspaceAction::Plan(PlanCardAction::Edit {
                    plan_id,
                    instruction,
                }) => engine.send(EngineCommand::EditPlan {
                    plan_ref: plan_id,
                    instruction,
                }),
                WorkspaceAction::Plan(PlanCardAction::Resume {
                    plan_id,
                    run_id,
                    inputs,
                    ..
                }) => engine.send(EngineCommand::ResumeRun {
                    plan_id,
                    run_id,
                    inputs,
                }),
                WorkspaceAction::Plan(PlanCardAction::Schedule { plan_id }) => {
                    chat_action = Some(ChatViewAction::GoToSchedule(plan_id));
                }
                WorkspaceAction::InspectRun(run_id) => {
                    engine.send(EngineCommand::InspectRun { run_id })
                }
            }
        }
    } else if design_panel_open {
        let response = egui::SidePanel::right("chat_design_panel")
            .resizable(false)
            .exact_width(panel_width)
            .frame(panel_frame)
            .show_animated(ctx, !panel_collapsed, |ui| {
                if let Some(flow) = state.flow.as_mut() {
                    anim::entrance(ui, Id::new("chat_design_panel_body"), 0.0, |ui| {
                        design_panel(ui, flow)
                    });
                }
            });
        panel_rect = response.map(|inner| inner.response.rect);
    }

    if !panel_collapsed && let Some(rect) = panel_rect {
        widgets::panel_resize_handle(
            ctx,
            width_id,
            rect,
            widgets::PanelEdge::Left,
            CHAT_SPLIT_MIN_PANEL_WIDTH..=max_panel_width,
        );
    }

    // Re-open affordance for a collapsed plan/design panel: floated over the
    // top-right corner of the chat area (where the panel would reappear), in
    // a fixed screen position so it stays put while the transcript scrolls.
    if panel_collapsed {
        let chat_area = ctx.available_rect();
        egui::Area::new(Id::new("chat_right_panel_expand"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(chat_area.right() - 40.0, chat_area.top() + 6.0))
            .show(ctx, |ui| {
                right_panel_toggle(ui, true);
            });
    }

    chat_action
}

pub fn show(
    ui: &mut Ui,
    state: &mut ChatState,
    sources: &SuggestionSources,
    engine: &EngineHandle,
    auto_mode: bool,
) -> Option<ChatViewAction> {
    let mut chat_action = None;

    egui::TopBottomPanel::bottom("chat_input_panel")
        .frame(
            egui::Frame::new()
                .fill(theme::bg())
                .inner_margin(egui::Margin::symmetric(16, 12)),
        )
        .show_separator_line(false)
        .show_inside(ui, |ui| {
            centered_column(ui, |ui| {
                if flow_elicitation(ui, state) {
                    continue_to_design(state, engine);
                    ui.add_space(theme::GAP);
                }
                design_action_bar(ui, state, engine, auto_mode);
                input_area(ui, state, sources, engine);
            });
        });

    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(theme::bg()))
        .show_inside(ui, |ui| {
            let showing_hero = state.messages.is_empty() && state.busy.is_none();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(!showing_hero)
                .show(ui, |ui| {
                    centered_column(ui, |ui| {
                        if let Some(action) = breadcrumb(ui, state) {
                            chat_action = Some(action);
                        }
                        if showing_hero {
                            hero(ui, state);
                            return;
                        }
                        ui.add_space(12.0);
                        if state.plan.is_none()
                            && let Some(flow) = &state.flow
                        {
                            let working = state.busy.is_some()
                                || flow.design_pending
                                || flow.awaiting_compile;
                            flow_progress(ui, flow, working);
                        }
                        messages(ui, state, sources, engine, &mut chat_action);
                        status_row(ui, state);
                        if state.plan.is_none()
                            && let Some(flow) = &state.flow
                            && flow.phase == FlowPhase::Refine
                        {
                            spec_card(ui, flow);
                        }
                        ui.add_space(8.0);
                    });
                });
        });

    chat_action
}

/// Slim orientation strip shown above the transcript when the current
/// conversation is owned by a plan — otherwise it's easy to forget which
/// plan-chat a Plans → Inspect/Run/Open action dropped you into. The `Plans`
/// segment is clickable and quiet, matching the rest of the chrome rather
/// than calling attention to itself.
fn breadcrumb(ui: &mut Ui, state: &ChatState) -> Option<ChatViewAction> {
    let plan = state.plan.as_deref()?;
    let mut action = None;
    ui.horizontal(|ui| {
        let crumb = |ui: &mut Ui, text: &str, muted: bool| {
            ui.label(RichText::new(text).size(theme::FONT_SMALL).color(if muted {
                theme::text_faint()
            } else {
                theme::text_muted()
            }))
        };
        let plans_response = ui
            .add(
                egui::Label::new(
                    RichText::new("Plans")
                        .size(theme::FONT_SMALL)
                        .color(theme::text_muted()),
                )
                .sense(egui::Sense::click()),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if plans_response.clicked() {
            action = Some(ChatViewAction::GoToPlans);
        }
        crumb(ui, "›", true);
        widgets::truncated_label(
            ui,
            RichText::new(&plan.name)
                .size(theme::FONT_SMALL)
                .color(theme::text_faint()),
            200.0,
        );
        crumb(ui, "›", true);
        crumb(ui, "chat", true);
    });
    ui.add_space(4.0);
    action
}

/// Empty-state welcome: breathing brand mark, headline, and suggestion
/// chips that pre-fill the input.
fn hero(ui: &mut Ui, state: &mut ChatState) {
    ui.add_space((ui.available_height() * 0.22).max(24.0));
    ui.vertical_centered(|ui| {
        hero_mark(ui);
        ui.add_space(14.0);
        ui.label(theme::title("Which work should we finish next?", 24.0));
        ui.add_space(6.0);
        ui.scope(|ui| {
            ui.set_max_width(440.0);
            ui.label(
                RichText::new(HERO_SUBTEXT)
                    .size(theme::FONT_SMALL)
                    .color(theme::text_muted()),
            );
        });
        ui.add_space(18.0);
        for (index, suggestion) in HERO_SUGGESTIONS.iter().enumerate() {
            anim::entrance(
                ui,
                Id::new("hero_suggestion").with(index),
                0.15 + index as f32 * 0.07,
                |ui| {
                    let display = suggestion.replace("; ", ";\n");
                    if widgets::wrapped_ghost_button(ui, &display, 720.0).clicked() {
                        state.input = (*suggestion).to_owned();
                        state.focus_input = true;
                    }
                },
            );
        }
    });
}

/// The app mark — the brand circle-with-dot, with a slow breathing glow.
fn hero_mark(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(56.0, 56.0), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let center = rect.center();
    let breath = anim::pulse(ui.input(|i| i.time), 3.2);
    let painter = ui.painter();
    // Soft breathing halo behind the mark.
    painter.circle_filled(
        center,
        22.0 + 3.0 * breath,
        theme::with_alpha(theme::accent(), 0.10 + 0.06 * breath),
    );
    // The brand ring with its centered dot.
    painter.circle_stroke(
        center,
        14.0,
        egui::Stroke::new(3.0_f32, theme::with_alpha(theme::accent(), 0.85)),
    );
    painter.circle_filled(center, 5.0, theme::accent());
    ui.ctx().request_repaint();
}

fn centered_column<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let available = ui.available_width();
    let width = available.min(theme::CHAT_MAX_WIDTH);
    let margin = ((available - width) / 2.0).max(0.0);
    ui.horizontal(|ui| {
        ui.add_space(margin);
        ui.vertical(|ui| {
            ui.set_width(width);
            add(ui)
        })
        .inner
    })
    .inner
}

// ─── Messages ─────────────────────────────────────────────────────────────────

fn messages(
    ui: &mut Ui,
    state: &mut ChatState,
    sources: &SuggestionSources,
    engine: &EngineHandle,
    chat_action: &mut Option<ChatViewAction>,
) {
    let mut deferred: Vec<EngineCommand> = Vec::new();
    let mut deferred_insight_commands: Vec<String> = Vec::new();
    let owned_plan_id = state.plan_id().map(str::to_owned);

    // Flow prompts are only answerable while the flow is still refining;
    // anything left open after the flow moved on (or ended) is expired.
    let refine_active = owned_plan_id.is_none()
        && state
            .flow
            .as_ref()
            .is_some_and(|flow| flow.phase == FlowPhase::Refine);
    if !refine_active {
        state.resolve_open_flow_prompts("(expired)");
    }

    // AGENT_CALL output arrives after the run-started row was inserted. Keep
    // that live indicator at the current edge of its run's transcript instead
    // of leaving it above work that has already happened. This is render-only:
    // once the run finishes, its durable chronological order is unchanged.
    let render_order = message_render_order(&state.messages);
    for message_index in render_order {
        let message = &mut state.messages[message_index];
        // Guided-flow elicitations stay pinned directly above the composer,
        // where the action and its single reply field remain together.
        if matches!(
            message.body,
            MessageBody::FlowPrompt {
                resolution: None,
                ..
            }
        ) {
            continue;
        }
        let msg_id = Id::new("chat_msg").with(message.id);
        anim::entrance(ui, msg_id, 0.0, |ui| {
            match &mut message.body {
                MessageBody::Text(text) => text_bubble(ui, message.role, text),
                MessageBody::Error(text) => error_bubble(ui, text),
                MessageBody::RunStarted {
                    run_id,
                    text,
                    active,
                } => {
                    if let Some(cmd) = run_started_card(ui, run_id, text, *active) {
                        deferred.push(cmd);
                    }
                }
                MessageBody::RunFailed {
                    run_id,
                    text,
                    repair_requested,
                } => {
                    if let Some(cmd) = run_failed_card(ui, run_id, text, repair_requested) {
                        deferred.push(cmd);
                    }
                }
                MessageBody::RunCompleted { run_id, text } => {
                    if let Some(cmd) = run_completed_card(ui, run_id, text) {
                        deferred.push(cmd);
                    }
                }
                MessageBody::Plan { plan, run } => {
                    // New conversations render their owned plan in the fixed
                    // workspace above. Keep this branch for demo/legacy
                    // transient cards that are not the owned plan.
                    if owned_plan_id.as_deref() == Some(plan.metadata.id.as_str()) {
                        return;
                    }
                    if let Some(action) = plan_card::show(ui, msg_id, plan, run.as_ref()) {
                        match action {
                            PlanCardAction::Run { plan_id, inputs } => {
                                deferred.push(EngineCommand::RunPlan {
                                    plan_ref: plan_id,
                                    inputs,
                                });
                            }
                            PlanCardAction::Edit {
                                plan_id,
                                instruction,
                            } => {
                                deferred.push(EngineCommand::EditPlan {
                                    plan_ref: plan_id,
                                    instruction,
                                });
                            }
                            PlanCardAction::Resume {
                                plan_id,
                                run_id,
                                inputs,
                                ..
                            } => {
                                deferred.push(EngineCommand::ResumeRun {
                                    plan_id,
                                    run_id,
                                    inputs,
                                });
                            }
                            PlanCardAction::Schedule { plan_id } => {
                                *chat_action = Some(ChatViewAction::GoToSchedule(plan_id));
                            }
                        }
                    }
                }
                MessageBody::Patch {
                    patch,
                    resolution,
                    resume_requested,
                } => {
                    if let Some(cmd) =
                        patch_card(ui, patch, resolution.as_deref(), resume_requested)
                    {
                        deferred.push(cmd);
                    }
                }
                MessageBody::Edit { edit, resolution } => {
                    if let Some(cmd) = edit_card(ui, edit, resolution.as_deref()) {
                        deferred.push(cmd);
                    }
                }
                MessageBody::Human {
                    prompt,
                    approval_required,
                    responder,
                    draft,
                    resolution,
                } => human_card(
                    ui,
                    msg_id,
                    prompt,
                    *approval_required,
                    responder,
                    draft,
                    resolution,
                ),
                MessageBody::AgentTranscript { step_id, lines, .. } => {
                    agent_transcript_card(ui, step_id, lines)
                }
                MessageBody::FlowPrompt {
                    kind,
                    prompt,
                    resolution,
                } => flow_prompt_history_card(ui, msg_id, *kind, prompt, resolution),
                MessageBody::SupportTicket {
                    issue_url,
                    report_path,
                    text,
                } => support_ticket_card(ui, issue_url, report_path, text),
                MessageBody::PlanIndex(items) => {
                    if let Some(cmd) = plan_index(ui, items) {
                        deferred.push(cmd);
                    }
                }
                MessageBody::RunIndex(items) => {
                    if let Some(cmd) = run_index(ui, items) {
                        deferred.push(cmd);
                    }
                }
                MessageBody::ToolIndex(tools) => tool_index(ui, tools),
                MessageBody::ScheduleIndex(items) => schedule_index(ui, items),
                MessageBody::Help => help_card(ui),
                MessageBody::Insight {
                    answer,
                    action,
                    resolution,
                } => {
                    if let Some(command_text) =
                        insight_card(ui, answer, action.as_ref(), resolution)
                    {
                        deferred_insight_commands.push(command_text);
                    }
                }
            }
            ui.add_space(theme::GAP);
        });
    }

    for cmd in deferred {
        engine.send(cmd);
    }
    // Re-parses each suggested command through the ordinary slash-command
    // path, so a click can never do anything a typed command could not (and
    // a stale/invalid suggestion is simply dropped instead of acted on).
    for command_text in deferred_insight_commands {
        if let Ok(commands::ChatInput::Command(command)) = commands::parse(&command_text) {
            dispatch_command(state, sources, engine, command);
        }
    }
}

/// Return message indices in display order, moving a still-active run marker
/// directly below the latest agent transcript emitted by that run.
fn message_render_order(messages: &[ChatMessage]) -> Vec<usize> {
    let mut indicator_after = vec![None; messages.len()];
    for (indicator_index, message) in messages.iter().enumerate() {
        let MessageBody::RunStarted {
            run_id,
            active: true,
            ..
        } = &message.body
        else {
            continue;
        };
        if let Some((transcript_index, _)) = messages
            .iter()
            .enumerate()
            .rev()
            .find(|(_, candidate)| {
                matches!(
                    &candidate.body,
                    MessageBody::AgentTranscript {
                        run_id: transcript_run_id,
                        ..
                    } if transcript_run_id == run_id
                )
            })
            .filter(|(transcript_index, _)| *transcript_index > indicator_index)
        {
            indicator_after[indicator_index] = Some(transcript_index);
        }
    }

    let mut order = Vec::with_capacity(messages.len());
    for index in 0..messages.len() {
        if indicator_after[index].is_some() {
            continue;
        }
        order.push(index);
        order.extend(indicator_after.iter().enumerate().filter_map(
            |(indicator_index, &transcript_index)| {
                (transcript_index == Some(index)).then_some(indicator_index)
            },
        ));
    }
    order
}

fn agent_transcript_card(ui: &mut Ui, step_id: &str, lines: &[AgentTranscriptLine]) {
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            widgets::status_dot(ui, theme::accent(), false);
            ui.label(
                RichText::new(format!("Agent transcript · {step_id}"))
                    .strong()
                    .color(theme::text()),
            );
        });
        ui.add_space(6.0);
        for line in lines {
            let (prefix, color) = match line.stream {
                AgentTranscriptLineStream::Input => ("> ", theme::accent()),
                AgentTranscriptLineStream::Output => ("", theme::text_muted()),
                AgentTranscriptLineStream::Error => ("! ", theme::warn()),
            };
            ui.label(
                RichText::new(format!("{prefix}{}", line.content))
                    .monospace()
                    .size(theme::FONT_SMALL)
                    .color(color),
            );
        }
    });
}

fn text_bubble(ui: &mut Ui, role: Role, text: &str) {
    let (layout, fill, width_share) = match role {
        Role::User => (Layout::right_to_left(Align::TOP), theme::user_bubble(), 0.8),
        Role::Assistant => (Layout::left_to_right(Align::TOP), theme::surface(), 0.9),
    };
    ui.with_layout(layout, |ui| {
        ui.set_max_width(ui.available_width() * width_share);
        theme::bubble_frame(fill).show(ui, |ui| {
            widgets::wrapped_label(ui, RichText::new(text).color(theme::text()));
        });
    });
}

fn error_bubble(ui: &mut Ui, text: &str) {
    theme::bubble_frame(theme::with_alpha(theme::err(), 0.10)).show(ui, |ui| {
        ui.set_max_width(ui.available_width());
        ui.horizontal_top(|ui| {
            ui.label(RichText::new("⚠").color(theme::err()));
            widgets::wrapped_label(ui, RichText::new(text).color(theme::text()));
        });
    });
}

/// In-progress bubble with a link-like action to the live run details.
fn run_started_card(ui: &mut Ui, run_id: &str, text: &str, active: bool) -> Option<EngineCommand> {
    let mut command = None;
    theme::bubble_frame(theme::with_alpha(theme::accent(), 0.08)).show(ui, |ui| {
        ui.set_max_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            if active {
                widgets::typing_indicator(ui);
            }
            ui.label(RichText::new(text).color(theme::text()));
            if ui.link("Open run details").clicked() {
                command = Some(EngineCommand::InspectRun {
                    run_id: run_id.to_owned(),
                });
            }
        });
    });
    command
}

/// Failure bubble with a one-click repair action.
fn run_failed_card(
    ui: &mut Ui,
    run_id: &str,
    text: &str,
    repair_requested: &mut bool,
) -> Option<EngineCommand> {
    let mut command = None;
    theme::bubble_frame(theme::with_alpha(theme::err(), 0.10)).show(ui, |ui| {
        ui.set_max_width(ui.available_width());
        ui.horizontal_top(|ui| {
            ui.label(RichText::new("⚠").color(theme::err()));
            widgets::wrapped_label(ui, RichText::new(text).color(theme::text()));
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            match *repair_requested {
                false => {
                    if widgets::primary_button(ui, "🔧 Repair now").clicked() {
                        *repair_requested = true;
                        command = Some(EngineCommand::Repair {
                            run_id: run_id.to_owned(),
                        });
                    }
                }
                true => {
                    ui.label(
                        RichText::new("Repair requested.")
                            .size(theme::FONT_SMALL)
                            .color(theme::text_muted()),
                    );
                }
            }
            if widgets::ghost_button(ui, "Create support ticket").clicked() {
                command = Some(EngineCommand::CreateSupportTicket {
                    run_id: Some(run_id.to_owned()),
                    plan_ref: None,
                });
            }
        });
    });
    command
}

/// Support-report bubble: the one-click link to the prefilled GitHub issue,
/// plus where the full report was saved for review.
fn support_ticket_card(ui: &mut Ui, issue_url: &str, report_path: &str, text: &str) {
    theme::bubble_frame(theme::with_alpha(theme::accent(), 0.08)).show(ui, |ui| {
        ui.set_max_width(ui.available_width());
        widgets::wrapped_label(ui, RichText::new(text).color(theme::text()));
        ui.add_space(6.0);
        ui.hyperlink_to("Open prefilled GitHub issue ↗", issue_url);
        // Explicit wrap: the path is one long word-poor token, and an
        // unwrapped label would widen the bubble past the chat column and
        // get clipped by the side panel (issues #42/#43).
        widgets::wrapped_label(
            ui,
            RichText::new(format!("Full report saved to {report_path}"))
                .size(theme::FONT_SMALL)
                .color(theme::text_muted()),
        );
    });
}

/// Successful completion bubble with one-click access to step outputs.
fn run_completed_card(ui: &mut Ui, run_id: &str, text: &str) -> Option<EngineCommand> {
    let mut command = None;
    theme::bubble_frame(theme::with_alpha(theme::ok(), 0.08)).show(ui, |ui| {
        ui.set_max_width(ui.available_width());
        // Text and action on separate rows: a wrapped label next to a button
        // in one `horizontal` fills the full width first and pushes the
        // button past the bubble, where the side panel clips it.
        widgets::wrapped_label(ui, RichText::new(text).color(theme::text()));
        ui.add_space(6.0);
        if widgets::ghost_button(ui, "Show details").clicked() {
            command = Some(EngineCommand::InspectRun {
                run_id: run_id.to_owned(),
            });
        }
    });
    command
}

/// Contextual status strip shown only while working or waiting for the user.
/// The working row is collapsed by default — expand it to see what the
/// backend call is actually doing, since it can run for a while with no
/// other feedback.
fn status_row(ui: &mut Ui, state: &ChatState) {
    if state.awaiting_input() {
        ui.horizontal(|ui| {
            widgets::status_dot(ui, theme::warn(), true);
            ui.label(
                RichText::new("Waiting for your input")
                    .size(theme::FONT_SMALL)
                    .color(theme::text_muted()),
            );
        });
        return;
    }

    if let Some(busy) = &state.busy {
        let elapsed = busy.started_at.elapsed().as_secs();
        let id = ui.make_persistent_id("chat_busy_row");
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
            .show_header(ui, |ui| {
                widgets::status_dot(ui, theme::active(), true);
                ui.label(
                    RichText::new(format!("{} ({elapsed}s)", busy.label))
                        .size(theme::FONT_SMALL)
                        .color(theme::text_muted()),
                );
            })
            .body(|ui| {
                ui.label(
                    RichText::new(busy.detail.as_deref().unwrap_or("Waiting on the backend…"))
                        .size(theme::FONT_SMALL)
                        .color(theme::text_muted()),
                );
                if let Some(console) = &busy.console {
                    ui.add_space(6.0);
                    console_panel(ui, console);
                }
            });
        // Keep the elapsed-time label ticking even while the row itself is
        // collapsed and not otherwise requesting repaints.
        ui.ctx().request_repaint();
        return;
    }

    // Post-mortem access to the last compile's console once nothing is
    // running anymore — failed compiles must not vanish without a trace.
    if let Some(console) = &state.last_console {
        let id = ui.make_persistent_id("chat_last_console_row");
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
            .show_header(ui, |ui| {
                ui.label(
                    RichText::new("Compiler console")
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                );
            })
            .body(|ui| console_panel(ui, console));
    }
}

/// How many console lines the live view renders; everything older stays in
/// the in-memory scrollback's log file. Keeps per-frame label cost bounded.
const CONSOLE_VIEW_LINES: usize = 250;
const CONSOLE_MAX_HEIGHT: f32 = 220.0;

/// Streamed compiler output: heartbeat line, monospace scrollback that
/// follows the newest line, and the on-disk log location.
fn console_panel(ui: &mut Ui, console: &CompileConsole) {
    let snapshot = console.snapshot();

    // Heartbeat — silence must be distinguishable from progress.
    ui.horizontal(|ui| match &snapshot.closed {
        Some(note) => {
            widgets::status_dot(ui, theme::text_faint(), false);
            widgets::truncated_label(
                ui,
                RichText::new(note.clone())
                    .size(theme::FONT_SMALL)
                    .color(theme::text_muted()),
                0.0,
            );
        }
        None => {
            widgets::status_dot(ui, theme::active(), true);
            let heartbeat = match snapshot.last_output {
                Some(at) => format!("last output {}s ago", at.elapsed().as_secs()),
                None => "no output yet".to_owned(),
            };
            ui.label(
                RichText::new(heartbeat)
                    .size(theme::FONT_SMALL)
                    .monospace()
                    .color(theme::text_muted()),
            );
        }
    });

    let hidden = snapshot.dropped + snapshot.lines.len().saturating_sub(CONSOLE_VIEW_LINES);
    if hidden > 0 {
        ui.label(
            RichText::new(format!("… {hidden} earlier lines in the log file"))
                .size(theme::FONT_SMALL)
                .color(theme::text_faint()),
        );
    }
    let start = snapshot.lines.len().saturating_sub(CONSOLE_VIEW_LINES);
    egui::ScrollArea::vertical()
        .id_salt("compile_console_scroll")
        .max_height(CONSOLE_MAX_HEIGHT)
        .auto_shrink([false, true])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for line in &snapshot.lines[start..] {
                let (prefix, color) = match line.stream {
                    ConsoleStream::Stdout => ("", theme::text_muted()),
                    ConsoleStream::Stderr => ("! ", theme::warn()),
                    ConsoleStream::Info => ("· ", theme::text_faint()),
                };
                ui.label(
                    RichText::new(format!("{prefix}{}", line.text))
                        .monospace()
                        .size(theme::FONT_SMALL)
                        .color(color),
                );
            }
        });
    if let Some(path) = &snapshot.log_path {
        widgets::truncated_label(
            ui,
            RichText::new(format!("Log: {}", path.display()))
                .size(theme::FONT_SMALL)
                .color(theme::text_faint()),
            0.0,
        );
    }
}

// ─── Guided flow (refine → design → artifact) ────────────────────────────────

/// Compact three-phase progress indicator shown while a guided flow is
/// active: 01 Refine › 02 Design › 03 Artifact.
///
/// The current phase is orange only while the flow actually waits on the
/// user; while the backend is working (drafting a design, compiling the
/// approved plan) it uses the neutral in-progress color — orange is reserved
/// for attention (see `theme::active`).
fn flow_progress(ui: &mut Ui, flow: &GuidedFlow, working: bool) {
    let current = if flow.awaiting_compile {
        2
    } else {
        match flow.phase {
            FlowPhase::Refine => 0,
            FlowPhase::Design => 1,
        }
    };
    ui.horizontal(|ui| {
        for (index, label) in ["01 REFINE", "02 DESIGN", "03 ARTIFACT"].iter().enumerate() {
            let color = match index.cmp(&current) {
                std::cmp::Ordering::Less => theme::text_muted(),
                std::cmp::Ordering::Equal => match working {
                    true => theme::active(),
                    false => theme::accent(),
                },
                std::cmp::Ordering::Greater => theme::text_faint(),
            };
            ui.label(
                RichText::new(*label)
                    .size(theme::FONT_SMALL)
                    .monospace()
                    .color(color),
            );
            if index < 2 {
                ui.label(
                    RichText::new("›")
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                );
            }
        }
    });
    ui.add_space(6.0);
}

/// Passive live spec summary for the REFINE phase: desired outcome,
/// acceptance criteria, and the confidence meter. All replies and the
/// continue-to-design action live in the pinned elicitation above the
/// composer — this card carries no inputs of its own.
fn spec_card(ui: &mut Ui, flow: &GuidedFlow) {
    let Some(assessment) = flow.assessment.clone() else {
        return;
    };
    anim::entrance(ui, Id::new("chat_spec_card"), 0.0, |ui| {
        theme::card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                widgets::section_label(ui, "Spec draft");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{:.0}%", assessment.confidence * 100.0))
                            .monospace()
                            .size(theme::FONT_SMALL)
                            .color(confidence_color(assessment.confidence)),
                    );
                });
            });
            ui.add_space(4.0);
            widgets::wrapped_label(
                ui,
                RichText::new(&assessment.spec.desired_outcome).color(theme::text()),
            );
            if !assessment.spec.acceptance_criteria.is_empty() {
                ui.add_space(6.0);
                widgets::section_label(ui, "Acceptance criteria");
                for criterion in &assessment.spec.acceptance_criteria {
                    widgets::wrapped_label(
                        ui,
                        RichText::new(format!("• {criterion}"))
                            .size(theme::FONT_SMALL)
                            .color(theme::text_muted()),
                    );
                }
            }
            if !assessment.spec.inputs.is_empty() {
                ui.add_space(6.0);
                widgets::section_label(ui, "Invocation inputs");
                for input in &assessment.spec.inputs {
                    let requirement = if input.required {
                        "required"
                    } else {
                        "optional"
                    };
                    widgets::wrapped_label(
                        ui,
                        RichText::new(format!(
                            "• {} ({}, {requirement}) — {}",
                            input.name, input.value_type, input.description
                        ))
                        .size(theme::FONT_SMALL)
                        .color(theme::text_muted()),
                    );
                }
            }
            ui.add_space(8.0);
            confidence_meter(ui, assessment.confidence);
        });
        ui.add_space(theme::GAP);
    });
}

fn confidence_color(confidence: f32) -> egui::Color32 {
    match confidence >= CONFIDENCE_THRESHOLD {
        true => theme::ok(),
        false => theme::accent(),
    }
}

/// A slim 0–100% bar for the spec confidence.
fn confidence_meter(ui: &mut Ui, confidence: f32) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Confidence")
                .size(theme::FONT_SMALL)
                .color(theme::text_muted()),
        );
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2((ui.available_width() - 8.0).max(60.0), 6.0),
            egui::Sense::hover(),
        );
        if ui.is_rect_visible(rect) {
            let radius = egui::CornerRadius::same(3);
            ui.painter().rect_filled(rect, radius, theme::selected());
            let fill_width = rect.width() * confidence.clamp(0.0, 1.0);
            if fill_width > 1.0 {
                let fill =
                    egui::Rect::from_min_size(rect.min, egui::vec2(fill_width, rect.height()));
                ui.painter()
                    .rect_filled(fill, radius, confidence_color(confidence));
            }
            // Threshold tick.
            let tick_x = rect.left() + rect.width() * CONFIDENCE_THRESHOLD;
            ui.painter().line_segment(
                [
                    egui::pos2(tick_x, rect.top() - 1.0),
                    egui::pos2(tick_x, rect.bottom() + 1.0),
                ],
                egui::Stroke::new(1.0_f32, theme::text_faint()),
            );
        }
    });
}

/// Side-panel body for the DESIGN phase: the proposed solution design with a
/// regenerating state. The approve action lives in the main column via
/// `design_action_bar` so it is always visible without scrolling the panel.
fn design_panel(ui: &mut Ui, flow: &GuidedFlow) {
    ui.horizontal(|ui| {
        widgets::section_label(ui, "Solution design");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            right_panel_toggle(ui, false);
            widgets::badge(ui, "02 · DESIGN", theme::accent());
        });
    });
    ui.add_space(6.0);

    if flow.design_pending {
        ui.horizontal(|ui| {
            widgets::typing_indicator(ui);
            ui.label(
                RichText::new(match flow.design.is_some() {
                    true => "Reworking the design from your feedback…",
                    false => "Drafting the solution design…",
                })
                .size(theme::FONT_SMALL)
                .color(theme::text_muted()),
            );
        });
        ui.add_space(6.0);
    }

    let Some(design) = flow.design.as_ref() else {
        return;
    };

    egui::ScrollArea::vertical()
        .id_salt("chat_design_panel_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.scope(|ui| {
                if flow.design_pending {
                    ui.set_opacity(0.45);
                }
                ui.label(theme::title(&design.title, 16.0));
                ui.add_space(4.0);
                widgets::wrapped_label(ui, RichText::new(&design.summary).color(theme::text()));

                if !design.recommended_tools.is_empty() {
                    ui.add_space(10.0);
                    widgets::section_label(ui, "Recommended tools");
                    ui.add_space(2.0);
                    for tool in &design.recommended_tools {
                        ui.horizontal(|ui| {
                            widgets::badge(ui, &tool.name, theme::step_tool());
                        });
                        widgets::wrapped_label(
                            ui,
                            RichText::new(&tool.reason)
                                .size(theme::FONT_SMALL)
                                .color(theme::text_muted()),
                        );
                        ui.add_space(4.0);
                    }
                }

                if !design.execution_outline.is_empty() {
                    ui.add_space(8.0);
                    widgets::section_label(ui, "Execution outline");
                    ui.add_space(2.0);
                    for (index, step) in design.execution_outline.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("{}", index + 1))
                                    .monospace()
                                    .size(theme::FONT_SMALL)
                                    .color(theme::text_faint()),
                            );
                            widgets::badge(ui, &step.step_kind, step_kind_color(&step.step_kind));
                            widgets::truncated_label(ui, RichText::new(&step.name).strong(), 0.0);
                        });
                        ui.horizontal_top(|ui| {
                            ui.add_space(16.0);
                            widgets::wrapped_label(
                                ui,
                                RichText::new(&step.description)
                                    .size(theme::FONT_SMALL)
                                    .color(theme::text_muted()),
                            );
                        });
                        ui.add_space(4.0);
                    }
                }
            });
        });
}

/// Above-composer gate for the DESIGN phase: approve button and context label,
/// rendered in the main column so they are always visible without scrolling
/// the side panel. No-ops when the flow is not in the Design phase or no
/// design has arrived yet.
///
/// With `auto_mode` the shell has already approved the design (see the
/// `DesignReady` handler in `app::mod`), so the gate becomes a status line —
/// the skipped click must never be silent.
fn design_action_bar(ui: &mut Ui, state: &mut ChatState, engine: &EngineHandle, auto_mode: bool) {
    let Some(flow) = state.flow.as_mut() else {
        return;
    };
    if flow.phase != FlowPhase::Design {
        return;
    }
    let Some(design) = flow.design.clone() else {
        return;
    };

    let ready = !flow.design_pending && !flow.awaiting_compile;

    // Auto mode compiles from the `DesignReady` event, so in the common path
    // there is nothing to press. The button still has to exist for the states
    // no further `DesignReady` will reach: a failed compile, a restart (which
    // clears the transient `awaiting_compile`), and auto mode switched on
    // while a design was already waiting. Without it those designs would sit
    // on screen for ever under a status line promising a compile.
    if auto_mode && !ready {
        ui.label(
            RichText::new(AUTO_MODE_DESIGN_STATUS)
                .size(theme::FONT_SMALL)
                .color(theme::accent()),
        );
        ui.add_space(theme::GAP);
        return;
    }

    let label = match auto_mode {
        true => "▶ Compile now",
        false => "✓ Approve design",
    };
    let approved = ui
        .add_enabled_ui(ready, |ui| widgets::primary_button(ui, label).clicked())
        .inner;
    if approved {
        flow.awaiting_compile = true;
        engine.send(EngineCommand::CompileFromSpec {
            intent: flow.intent.clone(),
            spec: flow.spec(),
            design: Some(Box::new(design)),
            conversation: flow.conversation.clone(),
        });
    }
    ui.add_space(2.0);
    ui.label(
        RichText::new(match flow.awaiting_compile {
            true => "Compiling the plan…",
            false => "Chat messages are treated as design feedback.",
        })
        .size(theme::FONT_SMALL)
        .color(theme::text_faint()),
    );
    ui.add_space(theme::GAP);
}

/// Map a design outline `step_kind` hint onto the step-type color tokens.
fn step_kind_color(step_kind: &str) -> egui::Color32 {
    let kind = step_kind.to_ascii_lowercase();
    if kind.contains("tool") {
        theme::step_tool()
    } else if kind.contains("prompt") {
        theme::step_prompt()
    } else if kind.contains("code") {
        theme::step_code()
    } else if kind.contains("human") {
        theme::step_human()
    } else if kind.contains("fan") {
        theme::step_fan()
    } else {
        theme::step_other()
    }
}

// ─── Patch card ───────────────────────────────────────────────────────────────

fn patch_card(
    ui: &mut Ui,
    patch: &Patch,
    resolution: Option<&str>,
    resume_requested: &mut bool,
) -> Option<EngineCommand> {
    let mut command = None;
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(theme::title("Proposed patch", 16.0));
            widgets::badge(ui, &patch.status.to_string(), theme::warn());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(plan_card::short_id(&patch.id))
                        .monospace()
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                );
            });
        });
        ui.add_space(4.0);
        widgets::wrapped_label(
            ui,
            RichText::new(format!(
                "{} (failing step: {})",
                describe_operation(&patch.operation),
                patch.failing_step_id
            ))
            .color(theme::text()),
        );
        ui.add_space(4.0);
        widgets::wrapped_label(
            ui,
            RichText::new(&patch.rationale)
                .size(theme::FONT_SMALL)
                .color(theme::text_muted()),
        );
        ui.add_space(8.0);

        match resolution {
            Some(text) => {
                ui.label(
                    RichText::new(text)
                        .size(theme::FONT_SMALL)
                        .color(theme::ok()),
                );
                // Only a successfully applied patch has anything to resume —
                // a rejected patch left the run's failure untouched.
                if text.starts_with("Patch applied") {
                    ui.add_space(6.0);
                    match *resume_requested {
                        false => {
                            let label = format!("▶ Resume from step “{}”", patch.failing_step_id);
                            if widgets::primary_button(ui, &label).clicked() {
                                *resume_requested = true;
                                command = Some(EngineCommand::ResumeRun {
                                    plan_id: patch.plan_id.clone(),
                                    run_id: patch.run_id.clone(),
                                    inputs: Default::default(),
                                });
                            }
                        }
                        true => {
                            ui.label(
                                RichText::new("Resume requested.")
                                    .size(theme::FONT_SMALL)
                                    .color(theme::text_muted()),
                            );
                        }
                    }
                }
            }
            None if patch.status == PatchStatus::Pending => {
                ui.horizontal(|ui| {
                    if widgets::primary_button(ui, "✓ Apply").clicked() {
                        command = Some(EngineCommand::ApplyPatch {
                            patch_id: patch.id.clone(),
                        });
                    }
                    if widgets::ghost_button(ui, "Reject").clicked() {
                        command = Some(EngineCommand::RejectPatch {
                            patch_id: patch.id.clone(),
                            reason: None,
                        });
                    }
                });
            }
            None => {}
        }
    });
    command
}

fn describe_operation(op: &PatchOperation) -> String {
    match op {
        PatchOperation::Batch { operations } => {
            format!("Apply {} targeted repair operations", operations.len())
        }
        PatchOperation::ReplaceStep { new_step } => {
            format!("Replace the step with a new definition ('{}')", new_step.id)
        }
        PatchOperation::UpdateStepConfig { .. } => {
            "Update the failing step's configuration".to_owned()
        }
        PatchOperation::InsertBefore { step } => {
            format!("Insert new step '{}' before the failing step", step.id)
        }
        PatchOperation::InsertAfter { step } => {
            format!("Insert new step '{}' after the failing step", step.id)
        }
        PatchOperation::SetStepField {
            step_id, pointer, ..
        } => {
            format!("Set '{}' on step '{}'", pointer, step_id)
        }
        PatchOperation::RemoveStepField { step_id, pointer } => {
            format!("Remove '{}' from step '{}'", pointer, step_id)
        }
        PatchOperation::SetPlanField { pointer, .. } => {
            format!("Set plan field '{}'", pointer)
        }
        PatchOperation::RemovePlanField { pointer } => {
            format!("Remove plan field '{}'", pointer)
        }
    }
}

// ─── Edit card ────────────────────────────────────────────────────────────────

fn edit_card(ui: &mut Ui, edit: &PlanEdit, resolution: Option<&str>) -> Option<EngineCommand> {
    let mut command = None;
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(theme::title("Proposed edit", 16.0));
            widgets::badge(ui, &edit.status.to_string(), theme::warn());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(plan_card::short_id(&edit.id))
                        .monospace()
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                );
            });
        });
        ui.add_space(4.0);
        widgets::wrapped_label(
            ui,
            RichText::new(format!("Requested change: “{}”", edit.instruction)).color(theme::text()),
        );
        ui.add_space(4.0);
        for line in describe_plan_diff(&edit.previous_plan, &edit.proposed_plan) {
            widgets::wrapped_label(
                ui,
                RichText::new(format!("• {line}"))
                    .size(theme::FONT_SMALL)
                    .color(theme::text_muted()),
            );
        }
        ui.add_space(8.0);

        match resolution {
            Some(text) => {
                ui.label(
                    RichText::new(text)
                        .size(theme::FONT_SMALL)
                        .color(theme::ok()),
                );
            }
            None if edit.status == PatchStatus::Pending => {
                ui.horizontal(|ui| {
                    if widgets::primary_button(ui, "✓ Apply").clicked() {
                        command = Some(EngineCommand::ApplyPatch {
                            patch_id: edit.id.clone(),
                        });
                    }
                    if widgets::ghost_button(ui, "Reject").clicked() {
                        command = Some(EngineCommand::RejectPatch {
                            patch_id: edit.id.clone(),
                            reason: None,
                        });
                    }
                });
            }
            None => {}
        }
    });
    command
}

/// Summarize the differences between two versions of a plan as short,
/// human-readable lines — added/removed/changed steps and any rename.
/// Mirrors `describe_operation`, but for a whole-plan edit rather than a
/// single constrained patch operation.
fn describe_plan_diff(previous: &Plan, proposed: &Plan) -> Vec<String> {
    let mut lines = Vec::new();
    if previous.name != proposed.name {
        lines.push(format!("Renamed “{}” → “{}”", previous.name, proposed.name));
    }

    let before_ids: std::collections::HashSet<&str> =
        previous.steps.iter().map(|s| s.id.as_str()).collect();
    let after_ids: std::collections::HashSet<&str> =
        proposed.steps.iter().map(|s| s.id.as_str()).collect();

    let added: Vec<&str> = proposed
        .steps
        .iter()
        .filter(|s| !before_ids.contains(s.id.as_str()))
        .map(|s| s.name.as_str())
        .collect();
    if !added.is_empty() {
        lines.push(format!("Added step(s): {}", added.join(", ")));
    }

    let removed: Vec<&str> = previous
        .steps
        .iter()
        .filter(|s| !after_ids.contains(s.id.as_str()))
        .map(|s| s.name.as_str())
        .collect();
    if !removed.is_empty() {
        lines.push(format!("Removed step(s): {}", removed.join(", ")));
    }

    let changed: Vec<&str> = proposed
        .steps
        .iter()
        .filter(|new_step| {
            previous
                .step(&new_step.id)
                .is_some_and(|old_step| old_step != *new_step)
        })
        .map(|s| s.name.as_str())
        .collect();
    if !changed.is_empty() {
        lines.push(format!("Changed step(s): {}", changed.join(", ")));
    }

    if lines.is_empty() {
        lines.push(format!(
            "{} steps, no structural change detected",
            proposed.steps.len()
        ));
    }
    lines
}

// ─── Elicitation cards (HUMAN_INTERACTION + guided-flow prompts) ─────────────

/// What the user did on an elicitation card this frame.
enum ElicitationReply {
    /// The primary action (approve / continue).
    Primary,
    /// The secondary action (reject).
    Secondary,
    /// The inline text reply.
    Text(String),
}

/// Declarative description of one elicitation card. Shared between real
/// HUMAN_INTERACTION prompts (answered over a responder channel) and
/// guided-flow prompts (answered by dispatching engine commands) so both
/// render identically.
struct ElicitationCard<'a> {
    title: &'a str,
    prompt: &'a str,
    /// Label for the filled primary action; `None` hides it.
    primary: Option<&'a str>,
    /// Label for the quiet secondary action; `None` hides it.
    secondary: Option<&'a str>,
    /// Hint for the inline text reply; `None` hides the field.
    text_hint: Option<&'a str>,
}

/// Render an elicitation card: pulsing dot + title, prompt, then either the
/// resolution line or the reply affordances. Returns what the user chose.
fn elicitation_card(
    ui: &mut Ui,
    msg_id: Id,
    card: &ElicitationCard<'_>,
    draft: &mut String,
    resolution: &Option<String>,
) -> Option<ElicitationReply> {
    let mut reply = None;
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            widgets::status_dot(ui, theme::warn(), resolution.is_none());
            ui.label(RichText::new(card.title).strong());
        });
        ui.add_space(4.0);
        widgets::wrapped_label(ui, RichText::new(card.prompt).color(theme::text()));
        ui.add_space(8.0);

        if let Some(text) = resolution.as_ref() {
            ui.label(
                RichText::new(text)
                    .size(theme::FONT_SMALL)
                    .color(theme::text_muted()),
            );
            return;
        }

        if card.primary.is_some() || card.secondary.is_some() {
            ui.horizontal(|ui| {
                if let Some(label) = card.primary
                    && widgets::primary_button(ui, label).clicked()
                {
                    reply = Some(ElicitationReply::Primary);
                }
                if let Some(label) = card.secondary
                    && widgets::ghost_button(ui, label).clicked()
                {
                    reply = Some(ElicitationReply::Secondary);
                }
            });
            if card.text_hint.is_some() {
                ui.add_space(6.0);
            }
        }
        if let Some(hint) = card.text_hint {
            ui.horizontal(|ui| {
                let edit = widgets::text_edit(
                    ui,
                    egui::TextEdit::singleline(draft)
                        .hint_text(hint)
                        .desired_width(ui.available_width() - 80.0)
                        .id(msg_id.with("elicitation_input")),
                );
                let submitted = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                // When a primary action exists, the text send demotes to the
                // quiet style so the card keeps exactly one primary action.
                let send_clicked = match card.primary.is_some() {
                    true => widgets::ghost_button(ui, "Send").clicked(),
                    false => widgets::primary_button(ui, "Send").clicked(),
                };
                if (send_clicked || submitted) && !draft.trim().is_empty() {
                    reply = Some(ElicitationReply::Text(draft.trim().to_owned()));
                }
            });
        }
    });
    reply
}

/// A HUMAN_INTERACTION prompt from a live run, answered over the responder
/// channel.
#[allow(clippy::too_many_arguments)]
fn human_card(
    ui: &mut Ui,
    msg_id: Id,
    prompt: &str,
    approval_required: bool,
    responder: &mut Option<tokio::sync::oneshot::Sender<HumanDecision>>,
    draft: &mut String,
    resolution: &mut Option<String>,
) {
    let card = match approval_required {
        true => ElicitationCard {
            title: "The plan is asking you",
            prompt,
            primary: Some("✓ Approve"),
            secondary: Some("Reject"),
            text_hint: None,
        },
        false => ElicitationCard {
            title: "The plan is asking you",
            prompt,
            primary: None,
            secondary: None,
            text_hint: Some("Type your answer…"),
        },
    };
    match elicitation_card(ui, msg_id, &card, draft, resolution) {
        Some(ElicitationReply::Primary) => {
            if let Some(tx) = responder.take() {
                let _ = tx.send(HumanDecision::Approve);
            }
            *resolution = Some("You approved.".to_owned());
        }
        Some(ElicitationReply::Secondary) => {
            if let Some(tx) = responder.take() {
                let _ = tx.send(HumanDecision::Reject);
            }
            *resolution = Some("You rejected.".to_owned());
        }
        Some(ElicitationReply::Text(text)) => {
            if let Some(tx) = responder.take() {
                let _ = tx.send(HumanDecision::Text(text.clone()));
            }
            *resolution = Some(format!("You answered: “{text}”"));
        }
        None => {}
    }
}

/// A plain-language answer to a question typed without a leading `/`,
/// optionally paired with one concrete follow-up action. Returns the raw
/// slash command once the user confirms it; the caller re-parses and
/// dispatches it exactly like a typed command, so a click can never do
/// anything a typed command could not.
fn insight_card(
    ui: &mut Ui,
    answer: &str,
    action: Option<&SuggestedAction>,
    resolution: &mut Option<String>,
) -> Option<String> {
    // No outer `with_layout` here (unlike `text_bubble`'s side-alignment
    // trick) — that would make the frame's interior stack horizontally too,
    // pushing the button beside the text instead of below it. Matches
    // `run_completed_card`'s proven vertical layout instead.
    let mut confirmed_command = None;
    theme::bubble_frame(theme::surface()).show(ui, |ui| {
        ui.set_max_width(ui.available_width());
        widgets::wrapped_label(ui, RichText::new(answer).color(theme::text()));
        let Some(action) = action else {
            return;
        };
        ui.add_space(8.0);
        if let Some(text) = resolution.as_ref() {
            ui.label(
                RichText::new(text)
                    .size(theme::FONT_SMALL)
                    .color(theme::text_muted()),
            );
            return;
        }
        if widgets::primary_button(ui, &action.label).clicked() {
            confirmed_command = Some(action.command.clone());
            *resolution = Some(format!("Ran “{}”.", action.command));
        }
    });
    confirmed_command
}

/// Render the most recent open guided-flow prompt directly above the main
/// composer. The prompt owns only its explicit action; detailed answers go
/// through the composer so the UI never presents two text fields.
fn flow_elicitation(ui: &mut Ui, state: &mut ChatState) -> bool {
    if state.plan.is_some()
        || !state
            .flow
            .as_ref()
            .is_some_and(|flow| flow.phase == FlowPhase::Refine)
    {
        return false;
    }
    let message = state.messages.iter_mut().rev().find(|message| {
        matches!(
            message.body,
            MessageBody::FlowPrompt {
                resolution: None,
                ..
            }
        )
    });
    let Some(message) = message else {
        return false;
    };
    let MessageBody::FlowPrompt {
        kind,
        prompt,
        resolution,
    } = &mut message.body
    else {
        return false;
    };
    let card = match kind {
        FlowPromptKind::Question => ElicitationCard {
            title: "Refining the spec",
            prompt,
            primary: None,
            secondary: None,
            text_hint: None,
        },
        FlowPromptKind::ContinueGate => ElicitationCard {
            title: "The spec is ready",
            prompt,
            primary: Some("Continue to solution design"),
            secondary: None,
            text_hint: None,
        },
    };
    let mut unused_draft = String::new();
    match elicitation_card(
        ui,
        Id::new("guided_flow_elicitation").with(message.id),
        &card,
        &mut unused_draft,
        resolution,
    ) {
        Some(ElicitationReply::Primary) => {
            *resolution = Some("Continuing to solution design.".to_owned());
            true
        }
        Some(ElicitationReply::Secondary | ElicitationReply::Text(_)) | None => false,
    }
}

/// Keep resolved guided-flow prompts in the transcript for conversational
/// context after the active elicitation has moved on.
fn flow_prompt_history_card(
    ui: &mut Ui,
    msg_id: Id,
    kind: FlowPromptKind,
    prompt: &str,
    resolution: &Option<String>,
) {
    let title = match kind {
        FlowPromptKind::Question => "Refining the spec",
        FlowPromptKind::ContinueGate => "The spec is ready",
    };
    let card = ElicitationCard {
        title,
        prompt,
        primary: None,
        secondary: None,
        text_hint: None,
    };
    let mut unused_draft = String::new();
    let _ = elicitation_card(ui, msg_id, &card, &mut unused_draft, resolution);
}

/// The ONLY places a guided flow ever advances are here and the design
/// panel's explicit approve — never automatically.
fn continue_to_design(state: &mut ChatState, engine: &EngineHandle) {
    let Some(flow) = state.flow.as_mut() else {
        return;
    };
    if flow.phase != FlowPhase::Refine {
        return;
    }
    flow.phase = FlowPhase::Design;
    flow.design_pending = true;
    engine.send(EngineCommand::GenerateDesign {
        spec: flow.spec(),
        conversation: flow.conversation.clone(),
        previous_design: None,
        feedback: None,
    });
}

// ─── Index cards (/plans, /runs, /tools) ─────────────────────────────────────

fn plan_index(ui: &mut Ui, items: &[PlanListItem]) -> Option<EngineCommand> {
    let mut command = None;
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        widgets::section_label(ui, "Plans");
        if items.is_empty() {
            ui.label(
                RichText::new("No plans yet — describe the work to compile one.")
                    .color(theme::text_muted()),
            );
        }
        for item in items {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(plan_card::short_id(&item.id))
                        .monospace()
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                );
                widgets::truncated_label(ui, RichText::new(&item.name).strong(), 130.0);
                widgets::badge(ui, &format!("v{}", item.version), theme::text_muted());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::ghost_button(ui, "Show").clicked() {
                        command = Some(EngineCommand::ShowPlan {
                            plan_ref: item.id.clone(),
                        });
                    }
                });
            });
        }
    });
    command
}

fn run_index(ui: &mut Ui, items: &[RunListItem]) -> Option<EngineCommand> {
    let mut command = None;
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        widgets::section_label(ui, "Recent runs");
        if items.is_empty() {
            ui.label(RichText::new("No runs yet.").color(theme::text_muted()));
        }
        for item in items {
            ui.horizontal(|ui| {
                let (color, pulsing) = match &item.status {
                    RunStatus::Succeeded => (theme::ok(), false),
                    RunStatus::Failed { .. } => (theme::err(), false),
                    RunStatus::Running => (theme::active(), true),
                    RunStatus::WaitingForHuman { .. } => (theme::warn(), true),
                    _ => (theme::text_muted(), false),
                };
                widgets::status_dot(ui, color, pulsing);
                ui.label(
                    RichText::new(plan_card::short_id(&item.id))
                        .monospace()
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                );
                widgets::truncated_label(ui, RichText::new(&item.plan_name), 220.0);

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::ghost_button(ui, "Inspect").clicked() {
                        command = Some(EngineCommand::InspectRun {
                            run_id: item.id.clone(),
                        });
                    }
                    ui.label(
                        RichText::new(time::format_local(&item.started_at, "%b %d %H:%M"))
                            .size(theme::FONT_SMALL)
                            .color(theme::text_faint()),
                    );
                });
            });
        }
    });
    command
}

fn tool_index(ui: &mut Ui, tools: &[ToolEntry]) {
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        widgets::section_label(ui, "Tool catalog");
        if tools.is_empty() {
            ui.label(
                RichText::new("Catalog is empty — add tools under MCP Tools.")
                    .color(theme::text_muted()),
            );
        }
        for tool in tools {
            ui.horizontal(|ui| {
                let kind = crate::app::views::mcp::kind_style(&tool.config);
                widgets::badge(ui, kind.0, kind.1);
                ui.label(RichText::new(&tool.name).strong());
                widgets::truncated_label(
                    ui,
                    RichText::new(&tool.description)
                        .size(theme::FONT_SMALL)
                        .color(theme::text_muted()),
                    0.0,
                );
            });
        }
    });
}

fn schedule_index(ui: &mut Ui, items: &[ScheduleItem]) {
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        widgets::section_label(ui, "Schedules");
        if items.is_empty() {
            ui.label(
                RichText::new("No schedules — try /schedule <plan> */15 * * * *")
                    .color(theme::text_muted()),
            );
        }
        for item in items {
            ui.horizontal(|ui| {
                widgets::status_dot(
                    ui,
                    if item.enabled {
                        theme::ok()
                    } else {
                        theme::text_faint()
                    },
                    false,
                );
                widgets::truncated_label(ui, RichText::new(&item.plan_name).strong(), 260.0);
                ui.label(
                    RichText::new(&item.cron)
                        .monospace()
                        .size(theme::FONT_SMALL)
                        .color(theme::accent()),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(match &item.next_run_display {
                            Some(next) => format!("next {next}"),
                            None => "paused".to_owned(),
                        })
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                    );
                });
            });
        }
        ui.add_space(2.0);
        ui.label(
            RichText::new("Pause or delete schedules under Plans.")
                .size(theme::FONT_SMALL)
                .color(theme::text_faint()),
        );
    });
}

fn help_card(ui: &mut Ui) {
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        widgets::section_label(ui, "Commands");
        ui.add_space(4.0);
        for spec in COMMANDS {
            ui.label(RichText::new(spec.usage).monospace().color(theme::accent()));
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                widgets::wrapped_label(
                    ui,
                    RichText::new(spec.description)
                        .size(theme::FONT_SMALL)
                        .color(theme::text_muted()),
                );
            });
            ui.add_space(2.0);
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new("Anything without a leading / is compiled into a plan.")
                .size(theme::FONT_SMALL)
                .color(theme::text_faint()),
        );
    });
}

// ─── Input area with command palette ─────────────────────────────────────────

fn input_area(
    ui: &mut Ui,
    state: &mut ChatState,
    sources: &SuggestionSources,
    engine: &EngineHandle,
) {
    refresh_suggestions(state, engine);
    let input_hint = composer_hint(state);

    let palette = build_palette(&state.input, state.plan.is_some(), sources);
    if !palette.is_empty() {
        state.palette_index = state.palette_index.min(palette.len() - 1);

        // Arrow keys steer the palette instead of the caret while it is open.
        let (up, down, tab) = ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::Tab),
            )
        });
        if up {
            state.palette_index = state.palette_index.saturating_sub(1);
            state.palette_navigated = true;
        }
        if down {
            state.palette_index = (state.palette_index + 1).min(palette.len() - 1);
            state.palette_navigated = true;
        }
        if tab {
            state.input = palette[state.palette_index].insert.clone();
            state.focus_input = true;
            state.palette_navigated = false;
        }

        if let Some(clicked) = palette_popup(ui, &palette, state.palette_index) {
            state.input = palette[clicked].insert.clone();
            state.focus_input = true;
            state.palette_navigated = false;
        }
    } else {
        state.palette_index = 0;
        state.palette_navigated = false;
    }

    // Accent ring while the input has focus (focus state is from last frame,
    // which is fine at 60 fps).
    let input_id = Id::new("chat_main_input");
    let focused = ui.ctx().memory(|m| m.has_focus(input_id));
    let ring = ui
        .ctx()
        .animate_bool_with_time(input_id.with("ring"), focused, 0.15);
    let input_frame = egui::Frame::new()
        .fill(theme::input())
        .stroke(egui::Stroke::new(
            1.0_f32 + ring,
            theme::mix(theme::border(), theme::focus(), ring),
        ))
        .corner_radius(egui::CornerRadius::same(theme::RADIUS_CARD))
        .inner_margin(egui::Margin::symmetric(10, 8));

    let mut submitted = false;
    input_frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            // Keep Enter as the quick-submit shortcut while allowing the
            // multiline editor to wrap and Shift+Enter to insert a newline.
            let enter = focused && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
            let edit = ui.add(
                egui::TextEdit::multiline(&mut state.input)
                    .hint_text(input_hint)
                    .frame(false)
                    .desired_rows(1)
                    .desired_width(ui.available_width() - 76.0)
                    .return_key(egui::KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
                    .id(input_id),
            );
            if state.focus_input {
                edit.request_focus();
                state.focus_input = false;
            }
            if edit.changed() {
                // Typing invalidates a previous arrow-key selection.
                state.palette_navigated = false;
            }

            let send = widgets::primary_button(ui, "Send").clicked();

            if enter || send {
                // Enter applies the palette selection when the user steered
                // to it (↑/↓) or a command name is still partial; otherwise
                // it submits. Tab/click always apply.
                let completing = palette.get(state.palette_index).is_some_and(|row| {
                    let differs = row.insert.trim() != state.input.trim();
                    differs && (state.palette_navigated || row.enter_completes)
                });
                if completing {
                    state.input = palette[state.palette_index].insert.clone();
                    state.palette_navigated = false;
                } else {
                    submitted = true;
                }
                state.focus_input = true;
            }
        });
    });
    ui.label(
        RichText::new("Enter to send · Shift+Enter for a new line · Tab or click to complete")
            .size(theme::FONT_SMALL)
            .color(theme::text_faint()),
    );

    if submitted {
        let line = state.input.trim().to_owned();
        state.input.clear();
        if !line.is_empty() {
            submit_line(state, sources, engine, &line);
        }
    }
}

fn composer_hint(state: &ChatState) -> &'static str {
    let refine_active = state.plan.is_none()
        && state
            .flow
            .as_ref()
            .is_some_and(|flow| flow.phase == FlowPhase::Refine);
    if !refine_active {
        return DEFAULT_COMPOSER_HINT;
    }
    state
        .messages
        .iter()
        .rev()
        .find_map(|message| match &message.body {
            MessageBody::FlowPrompt {
                kind,
                resolution: None,
                ..
            } => Some(match kind {
                FlowPromptKind::Question => QUESTION_COMPOSER_HINT,
                FlowPromptKind::ContinueGate => CONTINUE_COMPOSER_HINT,
            }),
            _ => None,
        })
        .unwrap_or(DEFAULT_COMPOSER_HINT)
}

/// Fetch fresh plan/run/patch listings the moment the input enters an
/// argument position (once per command token, not per keystroke).
fn refresh_suggestions(state: &mut ChatState, engine: &EngineHandle) {
    let trimmed = state.input.trim_start();
    let arg_command = trimmed
        .starts_with('/')
        .then(|| trimmed.split_once(char::is_whitespace))
        .flatten()
        .map(|(command, _)| command.to_owned())
        .filter(|c| {
            commands::wants_plan_arg(c)
                || commands::wants_run_arg(c)
                || commands::wants_patch_arg(c)
        });

    match arg_command {
        Some(command) if state.suggestions_fetched_for.as_deref() != Some(&command) => {
            engine.send(EngineCommand::ListPlans);
            engine.send(EngineCommand::ListRuns);
            engine.send(EngineCommand::ListPatches);
            state.suggestions_fetched_for = Some(command);
        }
        Some(_) => {}
        None => state.suggestions_fetched_for = None,
    }
}

// ─── Palette model ────────────────────────────────────────────────────────────

/// Data the palette suggests arguments from (owned by the app shell).
pub struct SuggestionSources<'a> {
    pub plans: &'a [PlanListItem],
    pub runs: &'a [RunListItem],
    pub patches: &'a [PatchListItem],
}

/// One selectable palette row.
struct PaletteRow {
    /// Full input-line replacement when this row is chosen.
    insert: String,
    title: String,
    detail: String,
    /// Command-name rows complete on Enter; argument rows only on Tab/click,
    /// so Enter still submits an already-typed argument.
    enter_completes: bool,
}

fn build_palette(
    input: &str,
    has_attached_plan: bool,
    sources: &SuggestionSources,
) -> Vec<PaletteRow> {
    let trimmed = input.trim_start();
    if !trimmed.starts_with('/') {
        return Vec::new();
    }
    match trimmed.split_once(char::is_whitespace) {
        None => command_rows(trimmed, has_attached_plan),
        Some((command, arg)) => argument_rows(command, arg.trim(), sources),
    }
}

fn command_rows(prefix: &str, has_attached_plan: bool) -> Vec<PaletteRow> {
    let matches = commands::completions(prefix);
    // Fully typed exact command alone → nothing left to complete.
    if matches.len() == 1 && matches[0].name == prefix {
        return Vec::new();
    }
    matches
        .into_iter()
        .filter(|spec| !has_attached_plan || spec.name != "/compile")
        .take(PALETTE_MAX_ROWS)
        .map(|spec| PaletteRow {
            insert: format!("{} ", spec.name),
            title: spec.usage.to_owned(),
            detail: spec.description.to_owned(),
            enter_completes: true,
        })
        .collect()
}

/// Human-friendly argument suggestions: runs/plans/patches by name and date,
/// so nobody has to remember UUIDs.
fn argument_rows(command: &str, arg: &str, sources: &SuggestionSources) -> Vec<PaletteRow> {
    let when = |timestamp| time::format_local(timestamp, "%b %d %H:%M");

    let rows: Vec<PaletteRow> = match command {
        "/show" | "/schedule" => sources
            .plans
            .iter()
            .map(|p| PaletteRow {
                insert: format!("{command} {} ", p.id),
                title: format!("{} v{}", p.name, p.version),
                detail: format!("{} · {}", when(&p.updated_at), plan_card::short_id(&p.id)),
                enter_completes: false,
            })
            .collect(),
        "/inspect" | "/repair" => sources
            .runs
            .iter()
            .filter(|r| command != "/repair" || r.status.is_failed())
            .map(|r| PaletteRow {
                insert: format!("{command} {} ", r.id),
                title: format!("{} · {}", r.plan_name, run_status_word(&r.status)),
                detail: format!("{} · {}", when(&r.started_at), plan_card::short_id(&r.id)),
                enter_completes: false,
            })
            .collect(),
        "/apply" | "/reject" => sources
            .patches
            .iter()
            .filter(|p| p.status == PatchStatus::Pending)
            .map(|p| PaletteRow {
                insert: format!("{command} {} ", p.id),
                title: format!("{} · step {}", p.plan_name, p.failing_step_id),
                detail: format!("{} · {}", when(&p.proposed_at), plan_card::short_id(&p.id)),
                enter_completes: false,
            })
            .collect(),
        _ => Vec::new(),
    };

    let needle = arg.to_lowercase();
    rows.into_iter()
        .filter(|row| {
            needle.is_empty()
                || row.title.to_lowercase().contains(&needle)
                || row.insert.contains(arg)
        })
        // The exact id is already typed → nothing left to complete.
        .filter(|row| row.insert.trim() != format!("{command} {arg}"))
        .take(PALETTE_MAX_ROWS)
        .collect()
}

fn run_status_word(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed { .. } => "failed",
        RunStatus::Running => "running",
        RunStatus::WaitingForHuman { .. } => "waiting",
        RunStatus::Cancelled => "cancelled",
    }
}

/// Render the palette; returns the index of a clicked row.
fn palette_popup(ui: &mut Ui, rows: &[PaletteRow], selected: usize) -> Option<usize> {
    let popup_id = Id::new("command_palette");
    anim::entrance(ui, popup_id.with(rows.len()), 0.0, |ui| {
        let clicked = theme::card_frame()
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                rows.iter().enumerate().fold(None, |clicked, (i, row)| {
                    let is_selected = i == selected;
                    let fill = match is_selected {
                        true => theme::with_alpha(theme::accent(), 0.12),
                        false => egui::Color32::TRANSPARENT,
                    };
                    let response = egui::Frame::new()
                        .fill(fill)
                        .corner_radius(egui::CornerRadius::same(theme::RADIUS_WIDGET))
                        .inner_margin(egui::Margin::symmetric(8, 4))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.set_width(ui.available_width());
                                widgets::truncated_label(
                                    ui,
                                    RichText::new(&row.title).monospace().color(
                                        match is_selected {
                                            true => theme::accent(),
                                            false => theme::text(),
                                        },
                                    ),
                                    180.0,
                                );
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    // Truncate to the space left of the title:
                                    // an unconstrained right-aligned label
                                    // paints leftward across the command
                                    // token when the description is long.
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&row.detail)
                                                .size(theme::FONT_SMALL)
                                                .color(theme::text_muted()),
                                        )
                                        .truncate(),
                                    );
                                });
                            });
                        })
                        .response
                        .interact(egui::Sense::click());
                    if response.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    match response.clicked() {
                        true => Some(i),
                        false => clicked,
                    }
                })
            })
            .inner;
        ui.add_space(6.0);
        clicked
    })
}

// ─── Submission ───────────────────────────────────────────────────────────────

fn submit_line(
    state: &mut ChatState,
    sources: &SuggestionSources,
    engine: &EngineHandle,
    line: &str,
) {
    // Captured before the push below: once the user's own message lands,
    // the chat is never blank again.
    let was_blank = state.is_blank();
    state.push(Role::User, MessageBody::Text(line.to_owned()));

    match commands::parse(line) {
        Err(error) => state.push(Role::Assistant, MessageBody::Error(error.to_string())),
        Ok(commands::ChatInput::Intent(intent)) => {
            handle_intent_message(state, was_blank, engine, intent)
        }
        Ok(commands::ChatInput::Command(command)) => {
            dispatch_command(state, sources, engine, command)
        }
    }
}

/// Run one parsed slash command. Shared between messages typed into the
/// composer and a suggested action confirmed from an `Insight` card — both
/// go through the same parser and the same dispatch, so a click can never
/// do anything a typed command could not.
fn dispatch_command(
    state: &mut ChatState,
    sources: &SuggestionSources,
    engine: &EngineHandle,
    command: commands::Command,
) {
    use commands::Command;
    match command {
        Command::Compile { intent } => match state.plan_id() {
                    Some(_) => state.push(
                        Role::Assistant,
                        MessageBody::Error(
                            "This chat already has a plan. Describe the change normally or use /edit <change…>."
                                .to_owned(),
                        ),
                    ),
                    None => engine.send(EngineCommand::Compile { intent }),
                },
                Command::Plans => {
                    state.expect_plan_index = true;
                    engine.send(EngineCommand::ListPlans);
                }
                Command::Show { plan_ref } => engine.send(EngineCommand::ShowPlan { plan_ref }),
                Command::Edit { instruction } => match state.plan_id() {
                    Some(plan_id) => engine.send(EngineCommand::EditPlan {
                        plan_ref: plan_id.to_owned(),
                        instruction,
                    }),
                    None => state.push(
                        Role::Assistant,
                        MessageBody::Error(
                            "There is no plan attached to this chat. Open a plan first, then use /edit <change…>."
                                .to_owned(),
                        ),
                    ),
                },
                Command::Run { inputs } => match state.plan_id() {
                    Some(plan_id) => engine.send(EngineCommand::RunPlan {
                        plan_ref: plan_id.to_owned(),
                        inputs,
                    }),
                    None => state.push(
                        Role::Assistant,
                        MessageBody::Error(
                            "There is no plan attached to this chat. Open a plan first, then use /run."
                                .to_owned(),
                        ),
                    ),
                },
                Command::Runs => {
                    state.expect_run_index = true;
                    engine.send(EngineCommand::ListRuns);
                }
                Command::Inspect { run_id } => {
                    match run_id.or_else(|| state.contextual_run_id(sources.runs)) {
                        Some(run_id) => engine.send(EngineCommand::InspectRun { run_id }),
                        None => state.push(
                            Role::Assistant,
                            MessageBody::Error(
                                "There is no run in this chat to inspect. Run the plan first or use /inspect <run-id>."
                                    .to_owned(),
                            ),
                        ),
                    }
                }
                Command::Repair { run_id } => {
                    match run_id.or_else(|| state.contextual_failed_run_id(sources.runs)) {
                        Some(run_id) => engine.send(EngineCommand::Repair { run_id }),
                        None => state.push(
                            Role::Assistant,
                            MessageBody::Error(
                                "There is no failed run in this chat to repair. Use /runs to find one, then /repair <run-id>."
                                    .to_owned(),
                            ),
                        ),
                    }
                }
                // `plan_id` is left empty: the chat parser only knows the run
                // id, and `ResumeRun`'s handler derives the plan id from the
                // run itself when this field is blank. Call sites that
                // already have both (e.g. a "Resume" action rendered from a
                // loaded run + plan) should pass the real `plan_id` instead.
                Command::Resume { run_id, inputs } => engine.send(EngineCommand::ResumeRun {
                    plan_id: String::new(),
                    run_id,
                    inputs,
                }),
                Command::Apply { patch_id } => engine.send(EngineCommand::ApplyPatch { patch_id }),
                Command::Reject { patch_id, reason } => {
                    engine.send(EngineCommand::RejectPatch { patch_id, reason })
                }
                Command::Schedule {
                    plan_ref,
                    cron,
                    inputs,
                } => engine.send(EngineCommand::SaveSchedule {
                    plan_ref,
                    expression: cron,
                    inputs,
                }),
                Command::Schedules => {
                    state.expect_schedule_index = true;
                    engine.send(EngineCommand::ListSchedules);
                }
                Command::Tools => {
                    state.expect_tool_index = true;
                    engine.send(EngineCommand::ListTools);
                }
                Command::Support { run_id } => {
                    // Prefer the chat's latest failed run — that's the one a
                    // ticket is usually about — then any contextual run; a
                    // ticket with plan or environment info alone still helps.
                    let run_id = run_id
                        .or_else(|| state.contextual_failed_run_id(sources.runs))
                        .or_else(|| state.contextual_run_id(sources.runs));
                    engine.send(EngineCommand::CreateSupportTicket {
                        run_id,
                        plan_ref: state.plan_id().map(str::to_owned),
                    });
                }
        Command::Help => state.push(Role::Assistant, MessageBody::Help),
        Command::Clear => state.messages.clear(),
    }
}

/// Route a plain (non-slash) chat message.
///
/// A completely fresh chat (`was_blank`: no messages, no plan, no flow yet)
/// always starts the guided create-a-plan flow, exactly as before — that is
/// the one implicit action a brand-new chat still assumes. Continuing an
/// already in-flight guided flow (the assistant's own pending clarifying
/// question, or a design revised from feedback) likewise treats a plain
/// reply as part of an action already under way: the user is answering a
/// question the app asked, not opening a new one. Every other plain
/// message — anything typed after that first message, including into a chat
/// that already owns a plan — is an INSIGHT question instead: it never
/// edits, runs, or compiles anything by itself. `/compile` and
/// `/edit <change…>` remain the explicit entry points for those actions; the
/// insight answer may suggest one as a one-click follow-up (see
/// `MessageBody::Insight`).
fn handle_intent_message(
    state: &mut ChatState,
    was_blank: bool,
    engine: &EngineHandle,
    text: String,
) {
    match state.flow.as_ref().map(|flow| flow.phase) {
        Some(FlowPhase::Refine) => {
            // The composer answers the pinned elicitation. Close it before
            // starting the next assessment so only one live prompt exists.
            state.resolve_open_flow_prompts("(answered in chat)");
            let flow = state.flow.as_mut().expect("checked above");
            flow.conversation.push(SpecTurn {
                role: "user".to_owned(),
                content: text,
            });
            engine.send(EngineCommand::AssessIntent {
                intent: flow.intent.clone(),
                conversation: flow.conversation.clone(),
            });
        }
        Some(FlowPhase::Design) => {
            let flow = state.flow.as_mut().expect("checked above");
            flow.design_pending = true;
            engine.send(EngineCommand::GenerateDesign {
                spec: flow.spec(),
                conversation: flow.conversation.clone(),
                previous_design: flow.design.clone().map(Box::new),
                feedback: Some(text),
            });
        }
        None if was_blank => {
            let flow = GuidedFlow::new(&text);
            engine.send(EngineCommand::AssessIntent {
                intent: text,
                conversation: flow.conversation.clone(),
            });
            state.flow = Some(flow);
        }
        None => engine.send(EngineCommand::AnswerInsight {
            question: text,
            plan_id: state.plan_id().map(str::to_owned),
        }),
    }
}

// ─── Event application (called from the app shell) ───────────────────────────

/// Turn a finished run into a short summary line for the conversation.
pub fn run_summary_text(run: &Run) -> String {
    let duration = run
        .finished_at
        .map(|end| end - run.started_at)
        .map(|d| plan_card::format_duration(d.num_milliseconds().max(0) as u64))
        .unwrap_or_else(|| "?".to_owned());
    let headline = match &run.status {
        RunStatus::Succeeded => format!("Run succeeded in {duration}."),
        RunStatus::Failed {
            failed_step_id,
            message,
        } => format!(
            "Run failed at step “{failed_step_id}” after {duration}: {message}\nUse /repair to ask the compiler for a fix."
        ),
        RunStatus::Cancelled => {
            format!(
                "Run cancelled after {duration} — you declined the approval. Nothing was written."
            )
        }
        other => format!("Run ended with status: {other}."),
    };

    let mut text = match token_usage_text(run) {
        Some(tokens) => format!("{headline}\n{tokens}"),
        None => headline,
    };

    if run.status == RunStatus::Succeeded
        && let Some(final_result) = final_result_text(run)
    {
        text.push_str("\n\n");
        text.push_str(&final_result);
    }

    text
}

/// Render the plan's published `outputs` (if any) as a short "Final result"
/// block for the chat transcript, once a run has succeeded.
fn final_result_text(run: &Run) -> Option<String> {
    if run.outputs.is_empty() {
        return None;
    }
    let lines: Vec<String> = run
        .outputs
        .iter()
        .map(|(name, value)| format!("- {name}: {}", format_output_value(value)))
        .collect();
    Some(format!("Final result:\n{}", lines.join("\n")))
}

fn format_output_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "(none)".to_owned(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// A one-line token-consumption summary for a finished run, or `None` when
/// no prompt or agent step reported usage.
fn token_usage_text(run: &Run) -> Option<String> {
    let summary = run.token_usage_summary();
    if summary.is_empty() {
        return None;
    }
    let step_word = if summary.ai_steps() == 1 {
        "AI step"
    } else {
        "AI steps"
    };
    Some(format!(
        "Tokens: {} {step_word} — {} in / {} out ({} total).",
        summary.ai_steps(),
        summary.usage.input_tokens,
        summary.usage.output_tokens,
        summary.usage.total()
    ))
}

/// Create a `RunBinding` from a `HumanRequest`-carrying event or a fresh run.
pub fn new_binding(
    run_id: &str,
    inputs: indexmap::IndexMap<String, serde_json::Value>,
) -> RunBinding {
    RunBinding {
        run_id: run_id.to_owned(),
        inputs,
        ..Default::default()
    }
}

/// Build a fully-populated binding from a stored run (for /inspect).
pub fn binding_from_run(run: &Run) -> RunBinding {
    let mut binding = new_binding(&run.id, run.inputs.clone());
    fill_binding(&mut binding, run);
    binding
}

/// Copy statuses, errors, timings, and results from a run record into a
/// binding — everything the plan card displays.
fn fill_binding(binding: &mut RunBinding, run: &Run) {
    binding.inputs = run.inputs.clone();
    for (step_id, sr) in &run.step_runs {
        if sr.status != StepRunStatus::Pending || !binding.iterations.contains_key(step_id) {
            binding.statuses.insert(step_id.clone(), sr.status.clone());
        }
        if let Some(err) = &sr.error {
            binding.errors.insert(step_id.clone(), err.clone());
        }
        if let Some(ms) = sr.duration_ms {
            binding.durations_ms.insert(step_id.clone(), ms);
        }
        if let Some(stdout) = &sr.stdout {
            binding.stdouts.insert(step_id.clone(), stdout.clone());
        }
        if let Some(stderr) = &sr.stderr {
            binding.stderrs.insert(step_id.clone(), stderr.clone());
        }
        if !sr.outputs.is_empty() {
            binding.outputs.insert(step_id.clone(), sr.outputs.clone());
        }
        if !sr.iterations.is_empty() {
            binding
                .iterations
                .insert(step_id.clone(), sr.iterations.clone());
        }
    }
    binding.finished = Some(run.status.clone());
}

/// Wrap an incoming human request as a chat message.
pub fn human_message(request: HumanRequest) -> MessageBody {
    MessageBody::Human {
        prompt: request.prompt,
        approval_required: request.approval_required,
        responder: Some(request.respond),
        draft: String::new(),
        resolution: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{PlanMetadata, PlanStatus};

    fn demo_plan(version: u32) -> Plan {
        Plan {
            metadata: PlanMetadata {
                id: "plan-1".to_owned(),
                version,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
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

    #[test]
    fn agent_transcript_lines_append_in_order_and_keep_stream_identity() {
        let mut chat = ChatState::default();
        for (stream, content) in [
            (AgentTranscriptStream::Stdout, "first"),
            (AgentTranscriptStream::Stderr, "warning"),
        ] {
            chat.append_agent_transcript(AgentTranscriptEvent {
                run_id: "run-1".to_owned(),
                step_id: "agent".to_owned(),
                stream,
                content: content.to_owned(),
            });
        }

        assert_eq!(chat.messages.len(), 1);
        let MessageBody::AgentTranscript { lines, .. } = &chat.messages[0].body else {
            panic!("expected transcript")
        };
        assert_eq!(lines[0].content, "first");
        assert_eq!(lines[0].stream, AgentTranscriptLineStream::Output);
        assert_eq!(lines[1].content, "warning");
        assert_eq!(lines[1].stream, AgentTranscriptLineStream::Error);
    }

    #[test]
    fn continuing_run_indicator_renders_below_its_agent_transcript() {
        let mut chat = ChatState::default();
        chat.push(
            Role::Assistant,
            MessageBody::RunStarted {
                run_id: "run-1".to_owned(),
                text: "Run started.".to_owned(),
                active: true,
            },
        );
        chat.append_agent_transcript(AgentTranscriptEvent {
            run_id: "run-1".to_owned(),
            step_id: "agent".to_owned(),
            stream: AgentTranscriptStream::Stdout,
            content: "finished the agent step".to_owned(),
        });

        assert_eq!(message_render_order(&chat.messages), vec![1, 0]);
    }

    #[test]
    fn completed_or_unrelated_run_indicators_keep_chronological_order() {
        let completed_run_messages = vec![
            ChatMessage {
                id: 1,
                role: Role::Assistant,
                body: MessageBody::RunStarted {
                    run_id: "run-1".to_owned(),
                    text: "Run started.".to_owned(),
                    active: false,
                },
            },
            ChatMessage {
                id: 2,
                role: Role::Assistant,
                body: MessageBody::AgentTranscript {
                    run_id: "run-1".to_owned(),
                    step_id: "agent".to_owned(),
                    lines: vec![],
                },
            },
        ];
        assert_eq!(message_render_order(&completed_run_messages), vec![0, 1]);

        let unrelated_run_messages = vec![
            ChatMessage {
                id: 1,
                role: Role::Assistant,
                body: MessageBody::RunStarted {
                    run_id: "run-1".to_owned(),
                    text: "Run started.".to_owned(),
                    active: true,
                },
            },
            ChatMessage {
                id: 2,
                role: Role::Assistant,
                body: MessageBody::AgentTranscript {
                    run_id: "run-2".to_owned(),
                    step_id: "agent".to_owned(),
                    lines: vec![],
                },
            },
        ];
        assert_eq!(message_render_order(&unrelated_run_messages), vec![0, 1]);
    }

    #[test]
    fn resolve_open_flow_prompts_only_touches_unresolved_prompts() {
        let mut chat = ChatState::default();
        chat.push(
            Role::Assistant,
            MessageBody::FlowPrompt {
                kind: FlowPromptKind::Question,
                prompt: "Which currency?".to_owned(),
                resolution: Some("You answered: “USD”".to_owned()),
            },
        );
        chat.push(
            Role::Assistant,
            MessageBody::FlowPrompt {
                kind: FlowPromptKind::ContinueGate,
                prompt: "Ready.".to_owned(),
                resolution: None,
            },
        );
        chat.push(Role::Assistant, MessageBody::Text("unrelated".to_owned()));

        chat.resolve_open_flow_prompts("(superseded)");

        match &chat.messages[0].body {
            MessageBody::FlowPrompt { resolution, .. } => {
                assert_eq!(resolution.as_deref(), Some("You answered: “USD”"));
            }
            _ => unreachable!(),
        }
        match &chat.messages[1].body {
            MessageBody::FlowPrompt { resolution, .. } => {
                assert_eq!(resolution.as_deref(), Some("(superseded)"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn awaiting_input_reflects_unresolved_prompts_and_pending_patches() {
        let mut chat = ChatState::default();
        assert!(!chat.awaiting_input(), "empty chat is idle");

        chat.push(Role::Assistant, MessageBody::Text("hi".to_owned()));
        assert!(!chat.awaiting_input(), "plain text has nothing to wait on");

        chat.push(
            Role::Assistant,
            MessageBody::FlowPrompt {
                kind: FlowPromptKind::Question,
                prompt: "Which currency?".to_owned(),
                resolution: None,
            },
        );
        assert!(chat.awaiting_input(), "an unanswered prompt is waiting");

        chat.resolve_open_flow_prompts("(answered in chat)");
        assert!(!chat.awaiting_input(), "an answered prompt is not waiting");

        let patch = Patch::new(
            "plan-1",
            1,
            "run-1",
            "step-1",
            PatchOperation::RemoveStepField {
                step_id: "step-1".to_owned(),
                pointer: "/x".to_owned(),
            },
            "because",
        );
        let patch_id = patch.id.clone();
        chat.push(
            Role::Assistant,
            MessageBody::Patch {
                patch: Box::new(patch),
                resolution: None,
                resume_requested: false,
            },
        );
        assert!(chat.awaiting_input(), "a pending patch is waiting");

        chat.resolve_patch(&patch_id, "Patch applied.");
        assert!(!chat.awaiting_input(), "a resolved patch is not waiting");

        let edit = PlanEdit::new("plan-1", 1, "add a step", demo_plan(1), demo_plan(2));
        let edit_id = edit.id.clone();
        chat.push(
            Role::Assistant,
            MessageBody::Edit {
                edit: Box::new(edit),
                resolution: None,
            },
        );
        assert!(chat.awaiting_input(), "a pending edit proposal is waiting");

        chat.resolve_edit(&edit_id, "Edit applied.");
        assert!(!chat.awaiting_input(), "a resolved edit is not waiting");
    }

    #[test]
    fn awaiting_input_includes_a_design_ready_for_approval() {
        let mut flow = GuidedFlow::new("do a thing");
        flow.phase = FlowPhase::Design;
        flow.design = Some(SolutionDesign {
            title: "A design".to_owned(),
            summary: "Do the thing".to_owned(),
            recommended_tools: vec![],
            execution_outline: vec![],
        });
        let mut chat = ChatState {
            flow: Some(flow),
            ..Default::default()
        };

        assert!(chat.awaiting_input(), "approval needs the user's attention");

        chat.flow.as_mut().unwrap().awaiting_compile = true;
        assert!(!chat.awaiting_input(), "compilation is backend work");

        let flow = chat.flow.as_mut().unwrap();
        flow.awaiting_compile = false;
        flow.design_pending = true;
        assert!(!chat.awaiting_input(), "design generation is backend work");
    }

    #[test]
    fn active_state_includes_guided_flow_work_and_live_plan_runs() {
        let mut chat = ChatState::default();
        assert!(!chat.is_active());

        chat.flow = Some(GuidedFlow::new("do a thing"));
        chat.flow.as_mut().unwrap().design_pending = true;
        assert!(chat.is_active(), "design generation is active immediately");

        let flow = chat.flow.as_mut().unwrap();
        flow.design_pending = false;
        flow.awaiting_compile = true;
        assert!(chat.is_active(), "design approval starts compilation");

        chat.flow = None;

        chat.workspace_run = Some(RunBinding::default());
        assert!(chat.is_active(), "an unfinished plan run remains active");

        chat.workspace_run.as_mut().unwrap().finished = Some(RunStatus::Succeeded);
        assert!(!chat.is_active(), "a finished plan run is no longer active");

        chat.busy = Some(BusyState::new("Compiling…", None));
        assert!(chat.is_active(), "backend work is active without a run");
    }

    #[test]
    fn repair_context_prefers_an_inspected_failure_then_the_latest_chat_failure() {
        let mut chat = ChatState::default();
        chat.push(
            Role::Assistant,
            MessageBody::RunFailed {
                run_id: "older-failure".to_owned(),
                text: "failed".to_owned(),
                repair_requested: false,
            },
        );
        chat.push(
            Role::Assistant,
            MessageBody::RunFailed {
                run_id: "newer-failure".to_owned(),
                text: "failed again".to_owned(),
                repair_requested: false,
            },
        );

        assert_eq!(
            chat.contextual_failed_run_id(&[]).as_deref(),
            Some("newer-failure")
        );

        chat.workspace_run = Some(RunBinding {
            run_id: "inspected-failure".to_owned(),
            finished: Some(RunStatus::Failed {
                failed_step_id: "step".to_owned(),
                message: "boom".to_owned(),
            }),
            ..Default::default()
        });

        assert_eq!(
            chat.contextual_failed_run_id(&[]).as_deref(),
            Some("inspected-failure")
        );
    }

    #[test]
    fn inspect_context_uses_the_current_workspace_or_latest_chat_run() {
        let mut chat = ChatState::default();
        chat.push(
            Role::Assistant,
            MessageBody::RunCompleted {
                run_id: "latest-run".to_owned(),
                text: "done".to_owned(),
            },
        );

        assert_eq!(chat.contextual_run_id(&[]).as_deref(), Some("latest-run"));

        chat.workspace_run = Some(RunBinding {
            run_id: "workspace-run".to_owned(),
            ..Default::default()
        });

        assert_eq!(
            chat.contextual_run_id(&[]).as_deref(),
            Some("workspace-run")
        );
    }

    #[test]
    fn assigned_plan_command_palette_hides_compile() {
        let commands: Vec<String> = command_rows("/", true)
            .into_iter()
            .map(|row| {
                row.title
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();

        assert!(!commands.iter().any(|command| command == "/compile"));
        assert!(commands.iter().any(|command| command == "/edit"));
    }

    #[test]
    fn continue_gate_uses_the_main_composer_for_more_detail() {
        let mut chat = ChatState {
            flow: Some(GuidedFlow::new("do a thing")),
            ..Default::default()
        };
        chat.push(
            Role::Assistant,
            MessageBody::FlowPrompt {
                kind: FlowPromptKind::ContinueGate,
                prompt: "Ready.".to_owned(),
                resolution: None,
            },
        );

        assert_eq!(composer_hint(&chat), CONTINUE_COMPOSER_HINT);

        chat.resolve_open_flow_prompts("(answered in chat)");
        assert_eq!(composer_hint(&chat), DEFAULT_COMPOSER_HINT);
    }

    fn succeeded_run() -> Run {
        let mut run = Run::new("plan-1", 1);
        run.status = RunStatus::Succeeded;
        run.started_at = chrono::Utc::now();
        run.finished_at = Some(run.started_at);
        run
    }

    #[test]
    fn run_summary_omits_final_result_when_plan_has_no_outputs() {
        let run = succeeded_run();
        let summary = run_summary_text(&run);
        assert!(!summary.contains("Final result"), "got: {summary}");
    }

    #[test]
    fn run_summary_includes_final_result_from_plan_outputs() {
        let mut run = succeeded_run();
        run.outputs
            .insert("summary".to_owned(), serde_json::json!("all good"));
        let summary = run_summary_text(&run);
        assert!(
            summary.contains("Final result:\n- summary: all good"),
            "got: {summary}"
        );
    }

    #[test]
    fn run_summary_labels_reported_usage_as_ai_steps() {
        let mut run = succeeded_run();
        let mut agent = crate::executor::StepRun::new("agent");
        agent.token_usage = Some(crate::storage::runs::TokenUsage {
            input_tokens: 120,
            output_tokens: 30,
        });
        run.step_runs.insert("agent".to_owned(), agent);

        assert!(
            run_summary_text(&run).contains("Tokens: 1 AI step — 120 in / 30 out (150 total).")
        );
    }

    #[test]
    fn final_result_is_not_shown_for_failed_runs() {
        let mut run = succeeded_run();
        run.status = RunStatus::Failed {
            failed_step_id: "step".to_owned(),
            message: "boom".to_owned(),
        };
        run.outputs
            .insert("summary".to_owned(), serde_json::json!("should not show"));
        let summary = run_summary_text(&run);
        assert!(!summary.contains("Final result"), "got: {summary}");
    }
}
