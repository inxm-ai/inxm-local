//! Desktop app layer — a chat-first surface over the workflow core.
//!
//! Structure (Atomic-Design-inspired, adapted to immediate-mode egui):
//! - `theme`    — design tokens, the only source of colors/spacing
//! - `anim`     — entrance/pulse animation helpers
//! - `widgets`  — atoms: badges, dots, buttons
//! - `views`    — organisms: chat, plan cards, plans index, MCP manager
//! - `engine`   — async bridge to the compiler/executor core
//! - this file  — the shell: navigation, event routing, layout

pub mod activity;
pub mod anim;
pub mod chat_store;
pub mod commands;
pub mod console;
mod debug_shot;
mod demo;
pub mod engine;
pub mod mcp_server;
pub mod mutation;
pub mod schedule_store;
pub mod scheduler_lock;
pub mod single_instance;
pub mod support;
pub mod theme;
pub mod time;
mod tray;
pub mod views;
pub mod widgets;

use egui::{Align, Id, Layout, RichText, Sense, Ui, vec2};

use activity::ActivityRegistry;
use engine::{DataPaths, EngineCommand, EngineEvent, EngineHandle, RoutedEngineEvent};
use views::chat::{self, ChatState, MessageBody, Role};
use views::mcp::{self, McpState};
use views::onboarding::{self, OnboardingAction, OnboardingState};
use views::plans::{self, PlansAction, PlansState};
use views::runs::{self, RunsAction, RunsState};
use views::schedules::{self, SchedulesState};
use views::settings::{self, SettingsAction, SettingsState};
use widgets::Icon;

const NAV_ITEM_HEIGHT: f32 = 36.0;
const NAV_ANIM_SECS: f32 = 0.15;
const NAV_INDICATOR_SLIDE_SECS: f32 = 0.22;
const VIEW_FADE_SECS: f64 = 0.28;
/// Vertical space (px) reserved at the bottom of the sidebar for the footer
/// (theme toggle, backend label, MCP label, data-dir path, paddings).
/// Prevents the chat list from overflowing into the footer area.
const SIDEBAR_FOOTER_HEIGHT: f32 = 130.0;

/// How far the left nav sidebar can be dragged — narrow enough
/// to reclaim space, wide enough that nav labels and chat titles stay legible.
const NAV_PANEL_WIDTH_RANGE: std::ops::RangeInclusive<f32> = 180.0..=420.0;
/// How often background data is re-fetched: the run list while a plan chat
/// is open (so its workspace history reflects runs finishing in the
/// background), and the whole Plans overview while it is visible (so its
/// stats, chart, and run list reflect scheduled runs and activity from
/// other sessions without a manual refresh).
const RUN_LIST_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

// ─── Headless entry point ───────────────────────────────────────────────────────

/// Handles returned by [`run_headless`], so the caller (main.rs) can report
/// status and keep the process alive. The engine keeps its worker threads
/// running independently of these handles.
pub struct HeadlessHandles {
    /// Status stream of the local MCP server (same channel as the desktop).
    pub mcp_status: std::sync::mpsc::Receiver<mcp_server::ServerStatus>,
    /// Whether this process started the scheduler loop or deferred to a live
    /// holder of the lock.
    pub scheduler: engine::SchedulerOutcome,
}

/// Start the MCP server and the cron scheduler without any egui context.
///
/// Intended for a `--headless` process so schedules keep firing while the
/// desktop window is closed. Console reporting is left to the caller: MCP
/// status arrives over `mcp_status`, and the scheduler outcome is returned
/// directly. This does not block; the servers run on their own threads.
///
/// Scheduled-run chat messages are persisted to the same chat store the
/// desktop uses (best effort — a plan without an existing conversation simply
/// records its run in history without a chat transcript).
pub fn run_headless(paths: DataPaths, settings: engine::AppSettings) -> HeadlessHandles {
    let activities = ActivityRegistry::default();
    let mcp_status =
        mcp_server::spawn_with_activities(paths.clone(), settings.mcp_port, activities);
    let (scheduler, events) = engine::start_scheduler_headless(paths.clone());
    if let Some(events) = events {
        spawn_headless_chat_writer(paths, events);
    }
    HeadlessHandles {
        mcp_status,
        scheduler,
    }
}

/// Drain engine events emitted by the headless scheduler and persist the
/// scheduled-run messages into the owning plan's chat session, mirroring what
/// the desktop UI writes. Only events already routed to a session (the
/// scheduler resolves this from the plan's conversation) are stored.
fn spawn_headless_chat_writer(
    paths: DataPaths,
    events: std::sync::mpsc::Receiver<RoutedEngineEvent>,
) {
    std::thread::Builder::new()
        .name("inxm-headless-chat".to_owned())
        .spawn(move || {
            while let Ok(routed) = events.recv() {
                let Some(session_id) = routed.session_id else {
                    continue;
                };
                let Some((role, body)) = headless_message_body(routed.event) else {
                    continue;
                };
                if chat_store::append(&paths.data_dir, &session_id, role, body).is_err() {
                    tracing::warn!(
                        session_id,
                        operation = "scheduled_chat.persist",
                        app_version = env!("CARGO_PKG_VERSION"),
                        triggered_by = "scheduler",
                        outcome = "failure",
                        "scheduled-run chat persistence failed"
                    );
                }
            }
        })
        .expect("failed to spawn headless chat writer thread");
}

/// Map the subset of engine events a scheduled run produces to durable chat
/// messages. Everything else (progress ticks, list refreshes) is dropped —
/// it is either transient or has no headless meaning.
fn headless_message_body(event: EngineEvent) -> Option<(Role, MessageBody)> {
    match event {
        EngineEvent::Assistant(text) => Some((Role::Assistant, MessageBody::Text(text))),
        EngineEvent::Failure(text) => Some((Role::Assistant, MessageBody::Error(text))),
        EngineEvent::RunStarted { run_id, .. } => Some((
            Role::Assistant,
            MessageBody::RunStarted {
                run_id,
                text: "Scheduled run started.".to_owned(),
                active: false,
            },
        )),
        EngineEvent::StepProgress(progress) if progress.transcript.is_some() => {
            let event = progress.transcript.expect("checked above");
            let run_id = event.run_id.clone();
            let step_id = event.step_id.clone();
            Some((
                Role::Assistant,
                MessageBody::AgentTranscript {
                    run_id,
                    step_id,
                    lines: vec![chat::AgentTranscriptLine::from(event)],
                },
            ))
        }
        EngineEvent::RunFinished { run } => {
            let text = chat::run_summary_text(&run);
            let body = if run.status.is_failed() {
                MessageBody::RunFailed {
                    run_id: run.id.clone(),
                    text,
                    repair_requested: false,
                }
            } else {
                MessageBody::RunCompleted {
                    run_id: run.id.clone(),
                    text,
                }
            };
            Some((Role::Assistant, body))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum View {
    Chat,
    Plans,
    Runs,
    Schedules,
    Mcp,
    Settings,
}

/// A single entry in the navigation back-stack: which view, and — for
/// `View::Chat` — which conversation was open, so `go_back` can restore the
/// exact chat rather than blindly landing on whatever chat is current.
#[derive(Debug, Clone, PartialEq)]
struct NavTarget {
    view: View,
    session_id: Option<String>,
}

/// Depth cap for the back-stack: old entries fall off the front (oldest
/// forgotten first) once this many locations have been visited.
const NAV_HISTORY_CAP: usize = 10;

/// Push `target` onto the capped history stack, dropping the oldest entry
/// once the cap is exceeded. Factored out as a free function so the stack
/// behavior is unit-testable without spinning up an `InxmApp`.
fn push_nav(history: &mut Vec<NavTarget>, target: NavTarget) {
    history.push(target);
    if history.len() > NAV_HISTORY_CAP {
        history.remove(0);
    }
}

/// Pop the most recent entry off the history stack (LIFO).
fn pop_nav(history: &mut Vec<NavTarget>) -> Option<NavTarget> {
    history.pop()
}

/// Whether the user has compiled at least one plan yet. Read synchronously
/// off disk (the same `StorageRoot` sync-read pattern already used elsewhere
/// at startup, e.g. `load_chat_session`'s plan attach) rather than deferred
/// to an async `ListPlans` round-trip, since the very first frame needs an
/// answer before the engine thread has had a chance to reply. Drives the
/// startup view (`InxmApp::new`): the Plans overview once it has something
/// to show, otherwise a fresh chat.
fn has_compiled_plans(paths: &DataPaths) -> bool {
    crate::storage::StorageRoot::open(&paths.data_dir)
        .and_then(|storage| storage.plans().list())
        .map(|plans| !plans.is_empty())
        .unwrap_or(false)
}

/// The view a fresh launch lands on, absent an `INXM_VIEW` dev override:
/// the Plans overview once there's at least one compiled plan to show
/// stats/activity for, otherwise Chat (an empty Plans page is useless to a
/// first-time user).
fn default_start_view(paths: &DataPaths) -> View {
    if has_compiled_plans(paths) {
        View::Plans
    } else {
        View::Chat
    }
}

/// Whether this launch runs in agent mode: a reduced surface for installs
/// that are driven almost entirely by external agents over the MCP server
/// rather than by a human chatting. Enabled with `INXM_AGENT_MODE=1` (any
/// value other than `0`/empty) or the `--agent-mode` CLI flag.
fn agent_mode_enabled() -> bool {
    std::env::var("INXM_AGENT_MODE").is_ok_and(|v| !v.is_empty() && v != "0")
        || std::env::args().any(|arg| arg == "--agent-mode")
}

/// Which views exist in the current mode. Agent mode exposes the Plans
/// overview, the Runs list (read-only there — Inspect lives in plan chats,
/// which don't exist in that mode), the MCP manager, and Settings — Chat and
/// Schedules (both chat-centric surfaces) are unreachable, and `navigate`
/// enforces this for every code path, not just the nav items that render.
fn view_available(view: View, agent_mode: bool) -> bool {
    !agent_mode || matches!(view, View::Plans | View::Runs | View::Mcp | View::Settings)
}

/// Whether this launch should show the first-run setup assistant.
///
/// The persisted `onboarding_completed` flag alone isn't quite enough: an
/// existing install that never wrote its own `settings.json` (nothing has
/// ever called `SaveSettings` — no theme change, no schedule, no manual
/// Settings save) would load `AppSettings::default()`, whose
/// `onboarding_completed` is `false` by construction, exactly like a true
/// first run. Existing chat sessions are the most robust "this isn't a fresh
/// install" signal available this early: a genuinely new user has zero of
/// them, while anyone who has ever sent a message has created at least one,
/// whether or not `settings.json` exists yet. Requiring both keeps an
/// existing user from ever seeing this, while still showing it to a fresh
/// install regardless of which of the two signals happens to be present.
fn should_show_onboarding(
    settings: &engine::AppSettings,
    sessions: &[chat_store::SessionSummary],
) -> bool {
    !settings.onboarding_completed && sessions.is_empty()
}

fn view_time_bucket(view: View) -> crate::telemetry::usage::ViewKind {
    use crate::telemetry::usage::ViewKind;
    match view {
        View::Chat => ViewKind::Chat,
        // Runs shares the Plans bucket until telemetry grows a dedicated
        // `ViewKind::Runs` (owned by src/telemetry).
        View::Plans | View::Runs => ViewKind::Plans,
        View::Schedules => ViewKind::Schedules,
        View::Mcp => ViewKind::McpTools,
        View::Settings => ViewKind::Settings,
    }
}

const NAV_ITEMS: &[(View, Icon, &str)] = &[
    (View::Chat, Icon::Chat, "Plan-Chat"),
    (View::Plans, Icon::Plans, "Plans"),
    (View::Runs, Icon::Runs, "Runs"),
    (View::Schedules, Icon::Schedules, "Schedules"),
    (View::Mcp, Icon::Tools, "MCP Tools"),
    (View::Settings, Icon::Settings, "Settings"),
];

pub struct InxmApp {
    engine: EngineHandle,
    activities: ActivityRegistry,
    events: std::sync::mpsc::Receiver<RoutedEngineEvent>,
    paths: DataPaths,
    view: View,
    last_view: View,
    view_switched_at: f64,
    /// Back-stack of previous locations. Non-empty ⇒ the Back affordance is
    /// shown; capped at `NAV_HISTORY_CAP`.
    nav_history: Vec<NavTarget>,
    chat: ChatState,
    session_id: String,
    session_created_at: chrono::DateTime<chrono::Utc>,
    /// Live states for chats opened during this process but not currently
    /// visible. The durable chat store intentionally degrades transient state
    /// (busy indicators, live responders, active run bindings) for restart
    /// safety, so using it for in-process navigation would make background
    /// work appear interrupted.
    parked_chats: std::collections::HashMap<String, ParkedChat>,
    sessions: Vec<chat_store::SessionSummary>,
    last_saved_fingerprint: u64,
    run_sessions: std::collections::HashMap<String, String>,
    last_run_refresh: std::time::Instant,
    plans: PlansState,
    runs_view: RunsState,
    schedules_view: SchedulesState,
    mcp: McpState,
    settings: SettingsState,
    mcp_status: mcp_server::ServerStatus,
    mcp_status_rx: std::sync::mpsc::Receiver<mcp_server::ServerStatus>,
    tools: Vec<crate::tools::catalog::ToolEntry>,
    patches: Vec<engine::PatchListItem>,
    schedules: Vec<engine::ScheduleItem>,
    data_dir: String,
    backend: Option<String>,
    environment: String,
    shot: debug_shot::ShotState,
    /// Set once a `CheckForUpdates` round-trip finds a newer GitHub release.
    update_available: Option<(String, String)>,
    tray: Option<tray::TrayController>,
    /// Theme the tray icon currently shows; compared against `theme::is_dark`
    /// every frame so the icon follows theme changes from any source (the
    /// Settings picker or an OS theme switch under the System preference).
    tray_icon_dark: bool,
    /// Shared with `TrayController` so the tray-menu event handler can set
    /// this to `true` directly (from the OS thread that delivers menu
    /// events) before sending a `Close` viewport command straight through
    /// the cloned `egui::Context`. That direct path is necessary because a
    /// hidden window produces no frames on Windows, so the mpsc-based
    /// channel drain in `handle_tray_actions` would never run; without
    /// `quit_requested` already being `true` by the time the close request
    /// is observed, `handle_close_request` would just cancel the close and
    /// re-hide the window to the tray instead of quitting.
    quit_requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Set for the lifetime of a genuine first run (see
    /// `should_show_onboarding`); once dismissed it stays `false` for the
    /// rest of the process, and the persisted `onboarding_completed` flag
    /// keeps it from ever being `true` again on a later launch.
    onboarding_active: bool,
    /// Refreshable environment probe and connection status for the first-run
    /// assistant. Kept separate from Settings so installs completed while this
    /// view is open can be detected without restarting the app.
    onboarding: OnboardingState,
    /// Draft state of the first-run telemetry checkbox. Starts checked
    /// (telemetry is on by default at setup); persisted as-shown by every
    /// dismissal path. Until the card is dismissed no consent value exists
    /// and nothing is sent.
    onboarding_telemetry_opt_in: bool,
    /// Whether the left navigation sidebar is collapsed to reclaim
    /// horizontal space for the current view (session-only).
    sidebar_collapsed: bool,
    /// Agent-mode launch (see `agent_mode_enabled`): only Plans, MCP Tools,
    /// and Settings exist, and every chat-opening affordance is hidden.
    agent_mode: bool,
    /// Which view is currently accruing foreground time for the telemetry
    /// where-time-is-spent buckets, and since when. `None` while the window
    /// is unfocused so background/tray time never counts (see
    /// `track_view_time`).
    view_time_anchor: Option<(View, std::time::Instant)>,
}

struct ParkedChat {
    chat: ChatState,
    created_at: chrono::DateTime<chrono::Utc>,
    saved_fingerprint: u64,
}

impl InxmApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let paths = DataPaths::resolve();
        let mut settings = engine::AppSettings::load(&paths.settings_path);
        let has_enabled_schedule = schedule_store::load(&paths.schedules_path)
            .unwrap_or_else(|_| {
                tracing::error!(
                    operation = "schedule.load",
                    app_version = env!("CARGO_PKG_VERSION"),
                    triggered_by = "application",
                    outcome = "failure",
                    "schedule store unavailable; preserving it without changes"
                );
                Vec::new()
            })
            .iter()
            .any(|schedule| schedule.enabled);
        if settings.keep_running_in_background.is_none() && has_enabled_schedule {
            settings.keep_running_in_background = Some(true);
            if settings.save(&paths.settings_path).is_err() {
                tracing::warn!(
                    operation = "settings.save_background_mode",
                    app_version = env!("CARGO_PKG_VERSION"),
                    triggered_by = "application",
                    outcome = "failure",
                    "automatic background-mode setting persistence failed"
                );
            }
        }
        theme::apply(
            &cc.egui_ctx,
            engine::resolve_dark_mode(settings.theme_preference, cc.egui_ctx.system_theme()),
        );
        let quit_requested = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Raw HWND of the main window, used by the tray's `Open` action to
        // restore it via direct Win32 calls (bypassing egui's viewport-
        // command queue, which a hidden window never flushes on its own
        // -- see `tray::TrayController::new`). `None` off Windows, where
        // that bypass isn't needed.
        let window_hwnd: Option<isize> = {
            #[cfg(windows)]
            {
                use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                match cc.window_handle() {
                    Ok(handle) => match handle.as_raw() {
                        RawWindowHandle::Win32(win32) => Some(win32.hwnd.get()),
                        _ => None,
                    },
                    Err(_) => None,
                }
            }
            #[cfg(not(windows))]
            {
                None
            }
        };
        let tray = match tray::TrayController::new(
            &cc.egui_ctx,
            settings.schedules_paused,
            quit_requested.clone(),
            paths.settings_path.clone(),
            window_hwnd,
        ) {
            Ok(tray) => Some(tray),
            Err(_) => {
                tracing::warn!(
                    operation = "system_tray.initialize",
                    app_version = env!("CARGO_PKG_VERSION"),
                    triggered_by = "application",
                    outcome = "failure",
                    "system tray unavailable; close-to-tray disabled"
                );
                None
            }
        };
        let activity_repaint: engine::RepaintHook = {
            let ctx = cc.egui_ctx.clone();
            std::sync::Arc::new(move || ctx.request_repaint())
        };
        let activities = ActivityRegistry::new(Some(activity_repaint));
        let mcp_status_rx =
            mcp_server::spawn_with_activities(paths.clone(), settings.mcp_port, activities.clone());
        let (engine, events) =
            engine::spawn_with_activities(cc.egui_ctx.clone(), paths.clone(), activities.clone());
        engine.send(EngineCommand::Bootstrap);
        if settings.check_updates_on_startup {
            engine.send(EngineCommand::CheckForUpdates);
        }

        let agent_mode = agent_mode_enabled();
        let view = demo::initial_view_override()
            .filter(|view| view_available(*view, agent_mode))
            .unwrap_or_else(|| match agent_mode {
                // Agents land on the overview even when it's still empty —
                // the Chat fallback doesn't exist in this mode.
                true => View::Plans,
                false => default_start_view(&paths),
            });
        let chat = demo::initial_chat();
        demo::preopen_demo_detail(&cc.egui_ctx);
        let sidebar_collapsed = demo::initial_panels_collapsed(&cc.egui_ctx);
        let sessions = chat_store::list(&paths.data_dir);
        let last_saved_fingerprint = chat_store::fingerprint(&chat);
        // The first-run assistant funnels toward the chat flow; an
        // agent-driven install is configured through Settings instead.
        let onboarding_active = !agent_mode && should_show_onboarding(&settings, &sessions);

        // One anonymous ping per desktop start, plus the batched usage
        // summary accumulated since the last start — both no-ops unless the
        // user explicitly opted in (see `crate::telemetry` and
        // docs/telemetry.md).
        crate::telemetry::record_app_started(
            settings.telemetry_enabled,
            crate::telemetry::Channel::Desktop,
        );
        crate::telemetry::usage::flush(&paths.data_dir, &paths.settings_path);

        Self {
            engine,
            activities,
            events,
            paths,
            view,
            last_view: view,
            view_switched_at: 0.0,
            nav_history: Vec::new(),
            chat,
            session_id: uuid::Uuid::new_v4().to_string(),
            session_created_at: chrono::Utc::now(),
            parked_chats: Default::default(),
            sessions,
            last_saved_fingerprint,
            run_sessions: Default::default(),
            last_run_refresh: std::time::Instant::now(),
            plans: PlansState::default(),
            runs_view: RunsState::default(),
            schedules_view: SchedulesState::default(),
            mcp: McpState::default(),
            settings: SettingsState {
                draft: settings.clone(),
                codex_sandbox_test: None,
            },
            mcp_status: mcp_server::ServerStatus::Starting {
                port: settings.mcp_port,
            },
            mcp_status_rx,
            tools: Vec::new(),
            patches: Vec::new(),
            schedules: Vec::new(),
            data_dir: String::new(),
            backend: None,
            environment: String::new(),
            shot: debug_shot::ShotState::from_env(),
            update_available: None,
            tray,
            tray_icon_dark: theme::is_dark(),
            quit_requested,
            onboarding_active,
            onboarding: OnboardingState::new(),
            onboarding_telemetry_opt_in: true,
            sidebar_collapsed,
            agent_mode,
            view_time_anchor: None,
        }
    }

    fn save_settings(&self) {
        self.engine.send(EngineCommand::SaveSettings {
            settings: Box::new(self.settings.draft.clone()),
        });
    }

    // ─── First-run assistant ──────────────────────────────────────────────────

    /// Persist whatever backend is currently drafted, mark onboarding
    /// complete for good, and stop showing the assistant for the rest of
    /// this process. Used by both "Get started" and "Skip for now" — the
    /// only difference between them is whether the user picked a backend
    /// first, not whether the flag gets set.
    ///
    /// Telemetry follows the checkbox as shown on the card, which starts
    /// checked: every dismissal — "Get started", "Skip for now", or a nav
    /// click — persists its current state, so opting out means unchecking
    /// it before moving on. The card is the disclosure; nothing is sent
    /// while it is still open (see `docs/telemetry.md`).
    fn dismiss_onboarding(&mut self) {
        self.settings.draft.telemetry_enabled = Some(self.onboarding_telemetry_opt_in);
        self.settings.draft.onboarding_completed = true;
        self.save_settings();
        self.onboarding_active = false;
        // Startup skipped its ping because consent was still `None`; if the
        // user just opted in, this session's ping is sent now instead.
        crate::telemetry::record_app_started(
            self.settings.draft.telemetry_enabled,
            crate::telemetry::Channel::Desktop,
        );
    }

    fn restart_onboarding(&mut self) {
        self.settings.draft.onboarding_completed = false;
        self.save_settings();
        self.onboarding = OnboardingState::new();
        self.onboarding_telemetry_opt_in = self.settings.draft.telemetry_enabled.unwrap_or(true);
        self.onboarding_active = true;
    }

    /// Accrue foreground wall-clock time to the current view for the
    /// telemetry where-time-is-spent buckets (consent-gated inside
    /// `usage::count_view_seconds` — with telemetry off this whole path is
    /// a cheap no-op). Time is attributed when the view changes, when focus
    /// is lost, or after at most a minute of sitting in one view, so a
    /// crash loses no more than that. Unfocused/tray time never counts.
    fn track_view_time(&mut self, focused: bool) {
        const ATTRIBUTE_AFTER: std::time::Duration = std::time::Duration::from_secs(60);
        let now = std::time::Instant::now();
        match self.view_time_anchor {
            Some((view, since))
                if !focused
                    || view != self.view
                    || now.duration_since(since) >= ATTRIBUTE_AFTER =>
            {
                crate::telemetry::usage::count_view_seconds(
                    &self.paths.data_dir,
                    &self.paths.settings_path,
                    view_time_bucket(view),
                    now.duration_since(since).as_secs(),
                );
                self.view_time_anchor = focused.then_some((self.view, now));
            }
            None if focused => self.view_time_anchor = Some((self.view, now)),
            _ => {}
        }
    }

    fn handle_tray_actions(&mut self, ctx: &egui::Context) {
        let actions = self
            .tray
            .as_ref()
            .map(|tray| tray.drain_actions().collect::<Vec<_>>())
            .unwrap_or_default();
        for action in actions {
            match action {
                tray::TrayAction::Open => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                tray::TrayAction::TogglePause => {
                    // The tray thread already flipped and persisted
                    // `schedules_paused` to disk before this action ever
                    // reached the channel (it has to, since a hidden window
                    // produces no frames on Windows and this drain might not
                    // run for a long time). Re-read the file here instead of
                    // flipping again, so the app's in-memory draft — which
                    // the Settings view reads and edits — catches up to
                    // what is now on disk rather than double-toggling it
                    // back to the pre-click state once frames resume.
                    self.settings.draft.schedules_paused =
                        engine::AppSettings::load(&self.paths.settings_path).schedules_paused;
                }
                tray::TrayAction::Quit => {
                    self.quit_requested
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    // Drop the tray now, removing its icon immediately via
                    // `TrayIcon`'s own `Drop`, rather than waiting for the
                    // whole `InxmApp` to be dropped once the viewport
                    // actually finishes closing. This frame-driven path is
                    // a bonus, not the primary fix: the OS-thread handler
                    // in `tray::TrayController::new` already hides the icon
                    // and, if graceful shutdown stalls, force-exits the
                    // process shortly after a tray quit regardless.
                    self.tray = None;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|input| input.viewport().close_requested())
            || self
                .quit_requested
                .load(std::sync::atomic::Ordering::SeqCst)
            || self.tray.is_none()
            || self.settings.draft.keep_running_in_background == Some(false)
        {
            return;
        }

        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        // Wayland cannot hide top-level windows (`set_visible(false)` is a
        // no-op in winit), so on Linux we additionally minimize as a
        // compositor-managed fallback. Skip that on Windows/macOS: a window
        // that is both minimized and hidden stops producing frames on
        // Windows, which starves the tray-menu channel drain in `update()`
        // and makes "Open" from the tray unresponsive.
        #[cfg(target_os = "linux")]
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    // ─── Navigation ───────────────────────────────────────────────────────────

    /// Move to `target_view` (and, for `View::Chat`, `target_session` if
    /// known), remembering the current location so `go_back` can retrace it.
    /// A no-op if we're already exactly there. When `target_session` is
    /// `None` for a `View::Chat` target, the caller is responsible for
    /// switching the actual session afterwards (used when the target session
    /// doesn't exist yet, e.g. a brand new chat).
    fn navigate(&mut self, target_view: View, target_session: Option<&str>) {
        // Central agent-mode guard: engine events (PlanLoaded, RunInspected,
        // HumanNeeded, …) and leftover actions must not be able to surface
        // the chat-centric views, no matter which code path asks.
        if !view_available(target_view, self.agent_mode) {
            return;
        }
        let already_there = target_view == self.view
            && (target_view != View::Chat || target_session == Some(self.session_id.as_str()));
        if already_there {
            return;
        }
        // Persist the conversation before leaving it. Without this, a brand
        // new session that only exists in memory has no file on disk, and any
        // engine event routed to it after navigation is dropped by
        // `handle_routed_event` (which loads the target session from disk).
        if self.view == View::Chat {
            self.autosave_chat();
        }
        let from_session = (self.view == View::Chat).then(|| self.session_id.clone());
        push_nav(
            &mut self.nav_history,
            NavTarget {
                view: self.view,
                session_id: from_session,
            },
        );
        self.view = target_view;
        if target_view == View::Chat
            && let Some(session_id) = target_session
        {
            self.load_chat_session(session_id);
        }
    }

    /// Retrace the last navigation, restoring the previous view (and chat
    /// session, if the previous location was a specific conversation).
    fn go_back(&mut self) {
        let Some(target) = pop_nav(&mut self.nav_history) else {
            return;
        };
        self.view = target.view;
        if target.view == View::Chat
            && let Some(session_id) = target.session_id
        {
            self.load_chat_session(&session_id);
        }
    }

    /// Apply a navigation action requested by the chat view or its side
    /// panel — shared between the ctx-level panel and the central content.
    fn handle_chat_action(&mut self, action: Option<chat::ChatViewAction>) {
        match action {
            Some(chat::ChatViewAction::GoToPlans) => {
                self.navigate(View::Plans, None);
                self.refresh_plans_overview();
            }
            Some(chat::ChatViewAction::GoToSchedule(plan_id)) => {
                self.schedules_view.start_for_plan(&plan_id);
                self.navigate(View::Schedules, None);
            }
            None => {}
        }
    }

    /// Re-fetch everything the Plans overview shows (plan list, run
    /// history, schedules) — sent whenever the app lands on that view, so
    /// its stats/chart reflect activity that happened elsewhere.
    fn refresh_plans_overview(&self) {
        self.engine.send(EngineCommand::ListPlans);
        self.engine.send(EngineCommand::ListRuns);
        self.engine.send(EngineCommand::ListSchedules);
    }

    /// Whether the app is currently sitting on a brand-new, empty chat —
    /// starting "another" new one from here would be a no-op.
    fn on_blank_chat(&self) -> bool {
        self.view == View::Chat && self.chat.is_blank()
    }

    /// The shared top-bar row rendered above every view's own content: the
    /// sidebar toggle, plus Back once there is history to retrace. Everything
    /// else (new chat, overview, close) lives in the sidebar nav — duplicating
    /// it here only cluttered the column. Rendered inside the
    /// same margin frame as each view's content so it lines up with a page's
    /// own header row instead of sitting in a separate, differently inset
    /// strip above it.
    fn top_bar(&mut self, ui: &mut Ui) {
        let has_back = !self.nav_history.is_empty();
        ui.horizontal(|ui| {
            let sidebar_hover = match self.sidebar_collapsed {
                true => "Show sidebar",
                false => "Hide sidebar",
            };
            if widgets::ghost_icon_button(ui, Icon::PanelLeft)
                .on_hover_text(sidebar_hover)
                .clicked()
            {
                self.sidebar_collapsed = !self.sidebar_collapsed;
            }
            ui.add_space(theme::GAP_SMALL);
            if has_back && widgets::ghost_button(ui, "← Back").clicked() {
                self.go_back();
            }
        });
        activity::show(ui, &self.activities);
        ui.add_space(theme::GAP);
    }

    // ─── Chat sessions ────────────────────────────────────────────────────────

    /// Persist the conversation when it changed since the last save.
    fn autosave_chat(&mut self) {
        let fingerprint = chat_store::fingerprint(&self.chat);
        if fingerprint == self.last_saved_fingerprint {
            return;
        }
        let saved = chat_store::save(
            &self.paths.data_dir,
            &self.session_id,
            self.session_created_at,
            &self.chat,
        );
        if saved.is_ok() {
            self.last_saved_fingerprint = fingerprint;
            self.sessions = chat_store::list(&self.paths.data_dir);
        }
    }

    fn new_chat(&mut self) {
        // Target session is unknown ahead of time (fresh uuid below), so
        // `navigate` always treats this as a genuine move — correct, since a
        // brand new chat is always a distinct location worth remembering.
        self.navigate(View::Chat, None);
        self.autosave_chat();
        self.park_current_chat();
        self.chat = ChatState::default();
        self.session_id = uuid::Uuid::new_v4().to_string();
        self.session_created_at = chrono::Utc::now();
        self.last_saved_fingerprint = chat_store::fingerprint(&self.chat);
    }

    fn open_chat(&mut self, session_id: &str) {
        self.navigate(View::Chat, Some(session_id));
    }

    /// Load `session_id`'s conversation into the active chat state. Pure
    /// state-swap, no navigation bookkeeping — used both by `navigate` (when
    /// it knows the target session) and by `go_back` (which must restore the
    /// session without pushing a new history entry).
    fn load_chat_session(&mut self, session_id: &str) {
        if session_id == self.session_id {
            return;
        }
        self.autosave_chat();
        let target = self
            .parked_chats
            .remove(session_id)
            .or_else(|| self.load_parked_chat(session_id).ok());
        let Some(target) = target else {
            return;
        };

        self.park_current_chat();
        self.chat = target.chat;
        self.session_id = session_id.to_owned();
        self.session_created_at = target.created_at;
        self.last_saved_fingerprint = target.saved_fingerprint;
    }

    /// Move the current chat into the in-process cache without degrading its
    /// live state. Empty chats are cheap and have no background work to keep.
    fn park_current_chat(&mut self) {
        if self.chat.is_blank() {
            return;
        }
        let chat = std::mem::take(&mut self.chat);
        self.parked_chats.insert(
            self.session_id.clone(),
            ParkedChat {
                chat,
                created_at: self.session_created_at,
                saved_fingerprint: self.last_saved_fingerprint,
            },
        );
    }

    fn load_parked_chat(&self, session_id: &str) -> std::io::Result<ParkedChat> {
        let (mut chat, created_at, plan_id) = chat_store::load(&self.paths.data_dir, session_id)?;
        if let Some(plan_id) = plan_id
            && let Ok(storage) = crate::storage::StorageRoot::open(&self.paths.data_dir)
            && let Ok(plan) = storage.plans().load_current(&plan_id)
        {
            chat.attach_plan(Box::new(plan));
        }
        Ok(ParkedChat {
            saved_fingerprint: chat_store::fingerprint(&chat),
            chat,
            created_at,
        })
    }

    /// Open the one conversation owned by a plan, creating it on first use.
    fn open_plan_chat(&mut self, plan_id: &str) {
        if self.chat.plan_id() == Some(plan_id) {
            let current_session = self.session_id.clone();
            self.navigate(View::Chat, Some(&current_session));
            return;
        }
        if let Some(session) = chat_store::find_by_plan(&self.paths.data_dir, plan_id) {
            self.open_chat(&session.id);
            return;
        }

        let plan = crate::storage::StorageRoot::open(&self.paths.data_dir)
            .ok()
            .and_then(|storage| storage.plans().load_current(plan_id).ok());
        let Some(plan) = plan else {
            return;
        };

        // Target session is a fresh uuid, unknown ahead of time — same
        // reasoning as `new_chat`.
        self.navigate(View::Chat, None);
        self.autosave_chat();
        self.park_current_chat();
        self.chat = ChatState::default();
        self.chat.attach_plan(Box::new(plan));
        self.session_id = uuid::Uuid::new_v4().to_string();
        self.session_created_at = chrono::Utc::now();
        self.last_saved_fingerprint = 0;
        self.autosave_chat();
        self.engine.send(EngineCommand::ListRuns);
    }

    fn ensure_plan_chat(&mut self, plan: &crate::plan::types::Plan) -> Option<String> {
        if let Some(session) = chat_store::find_by_plan(&self.paths.data_dir, &plan.metadata.id) {
            return Some(session.id);
        }
        let session_id = uuid::Uuid::new_v4().to_string();
        let mut chat = ChatState::default();
        chat.attach_plan(Box::new(plan.clone()));
        chat_store::save(&self.paths.data_dir, &session_id, chrono::Utc::now(), &chat).ok()?;
        self.sessions = chat_store::list(&self.paths.data_dir);
        Some(session_id)
    }

    /// Find or create the conversation owned by a plan without changing the
    /// currently visible view. Used to route background schedule messages.
    fn ensure_plan_chat_for_id(&mut self, plan_id: &str) -> Option<String> {
        if self.chat.plan_id() == Some(plan_id) {
            return Some(self.session_id.clone());
        }
        if let Some(session) = chat_store::find_by_plan(&self.paths.data_dir, plan_id) {
            return Some(session.id);
        }
        let storage = crate::storage::StorageRoot::open(&self.paths.data_dir).ok()?;
        let plan = storage.plans().load_current(plan_id).ok()?;
        self.ensure_plan_chat(&plan)
    }

    fn delete_chat(&mut self, session_id: &str) {
        let can_delete = self
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .is_some_and(|session| sidebar_chat_can_delete(session.plan_id.as_deref()));
        if !can_delete {
            return;
        }
        chat_store::delete(&self.paths.data_dir, session_id);
        self.parked_chats.remove(session_id);
        self.sessions = chat_store::list(&self.paths.data_dir);
        if session_id == self.session_id {
            self.new_chat();
        }
    }

    // ─── Event routing ────────────────────────────────────────────────────────

    fn handle_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::Ready {
                data_dir,
                backend,
                environment,
            } => {
                self.data_dir = data_dir;
                self.backend = backend;
                self.environment = environment;
            }
            EngineEvent::Assistant(text) => {
                self.chat.push(Role::Assistant, MessageBody::Text(text));
            }
            EngineEvent::InsightStarted => {
                self.chat.busy = Some(chat::BusyState::new("Thinking…", None));
            }
            EngineEvent::InsightAnswer {
                answer,
                suggested_action,
            } => {
                self.chat.busy = None;
                self.chat.push(
                    Role::Assistant,
                    MessageBody::Insight {
                        answer,
                        action: suggested_action,
                        resolution: None,
                    },
                );
            }
            EngineEvent::SupportTicketReady {
                issue_url,
                report_path,
                message,
            } => {
                self.chat.push(
                    Role::Assistant,
                    MessageBody::SupportTicket {
                        issue_url,
                        report_path,
                        text: message,
                    },
                );
            }
            EngineEvent::Failure(text) => {
                self.chat.busy = None;
                // A failed assess/design/compile call must not leave the
                // guided flow stuck in a pending state.
                if let Some(flow) = self.chat.flow.as_mut() {
                    flow.design_pending = false;
                    flow.awaiting_compile = false;
                }
                self.chat.push(Role::Assistant, MessageBody::Error(text));
            }
            EngineEvent::CompileStarted { intent } => {
                self.chat.busy = Some(chat::BusyState::new(
                    "Compiling your intent into a plan…",
                    Some(format!("Intent: “{intent}”")),
                ));
            }
            EngineEvent::AssessStarted { intent } => {
                self.chat.busy = Some(chat::BusyState::new(
                    "Refining your request into a spec…",
                    Some(format!("Intent: “{intent}”")),
                ));
            }
            EngineEvent::AssessmentReady { assessment } => {
                self.chat.busy = None;
                let assessment = *assessment;
                // Every assessment renders its result and waits: NO phase
                // transition ever happens without an explicit user action.
                // A confident spec (even on the first turn) only means the
                // continue affordance appears immediately.
                let mut prompt = None;
                if let Some(flow) = self.chat.flow.as_mut() {
                    flow.assess_turns += 1;
                    if flow.phase == chat::FlowPhase::Refine {
                        let confident = assessment.confidence >= chat::CONFIDENCE_THRESHOLD;
                        let question = assessment.question.clone();
                        if let Some(question) = &question {
                            flow.conversation.push(crate::compiler::SpecTurn {
                                role: "assistant".to_owned(),
                                content: question.clone(),
                            });
                        }
                        prompt = Some(match confident {
                            true => (
                                chat::FlowPromptKind::ContinueGate,
                                question.unwrap_or_else(|| {
                                    "The spec looks complete enough to design a solution."
                                        .to_owned()
                                }),
                            ),
                            false => (
                                chat::FlowPromptKind::Question,
                                question.unwrap_or_else(|| {
                                    "Tell me more about what you need — the spec isn't \
                                     specific enough yet."
                                        .to_owned()
                                }),
                            ),
                        });
                    }
                    flow.assessment = Some(assessment);
                }
                if let Some((kind, prompt)) = prompt {
                    // A fresh assessment supersedes any still-open prompt so
                    // exactly one reply affordance is live at a time.
                    self.chat.resolve_open_flow_prompts("(superseded)");
                    self.chat.push(
                        Role::Assistant,
                        MessageBody::FlowPrompt {
                            kind,
                            prompt,
                            resolution: None,
                        },
                    );
                }
            }
            EngineEvent::DesignStarted => {
                if let Some(flow) = self.chat.flow.as_mut() {
                    flow.phase = chat::FlowPhase::Design;
                    flow.design_pending = true;
                }
            }
            EngineEvent::DesignReady { design } => {
                self.chat.busy = None;
                let auto_mode = self.settings.draft.auto_mode;
                let mut announce = false;
                // Auto mode dispatches the very command the approve button
                // sends, but only after the flow is updated, so the borrow
                // ends before the send.
                let mut auto_approved = None;
                if let Some(flow) = self.chat.flow.as_mut() {
                    announce = flow.design.is_none();
                    flow.phase = chat::FlowPhase::Design;
                    flow.design_pending = false;
                    flow.design = Some(*design);
                    // Same readiness gate as the approve button: never start
                    // a second compile while one is already in flight.
                    if auto_mode
                        && !flow.awaiting_compile
                        && let Some(design) = flow.design.clone()
                    {
                        flow.awaiting_compile = true;
                        auto_approved = Some(EngineCommand::CompileFromSpec {
                            intent: flow.intent.clone(),
                            spec: flow.spec(),
                            design: Some(Box::new(design)),
                            conversation: flow.conversation.clone(),
                        });
                    }
                }
                if let Some(command) = auto_approved {
                    // Background sessions keep their own compile: this event
                    // may have been routed to a conversation the user is not
                    // currently reading.
                    self.engine.send_from(self.session_id.clone(), command);
                }
                if announce {
                    self.chat.push(
                        Role::Assistant,
                        MessageBody::Text(design_ready_announcement(auto_mode).to_owned()),
                    );
                }
            }
            EngineEvent::EditStarted {
                plan_name,
                instruction,
            } => {
                self.chat.busy = Some(chat::BusyState::new(
                    "Editing the plan…",
                    Some(format!(
                        "Plan: “{plan_name}”\nRequested change: “{instruction}”"
                    )),
                ));
            }
            EngineEvent::CompileConsole { console } => {
                // Attach the live console to the busy row announced just
                // before it, and keep it around for post-mortem reading
                // after the operation finishes.
                if let Some(busy) = self.chat.busy.as_mut() {
                    busy.console = Some(console.clone());
                }
                self.chat.last_console = Some(console);
            }
            EngineEvent::RepairStarted {
                run_id,
                failing_step_id,
            } => {
                self.chat.busy = Some(chat::BusyState::new(
                    "Analyzing the failure and proposing a patch…",
                    Some(format!(
                        "Run {run_id}, failing step “{failing_step_id}” — calling the LLM backend, this can take a while."
                    )),
                ));
            }
            EngineEvent::PlanCompiled { plan } => {
                self.chat.busy = None;
                // The guided flow ends once the artifact exists; the design
                // lives on in plan.metadata.solution_design.
                self.chat.flow = None;
                let text = format!(
                    "Compiled “{}” — {} steps, validated and saved. Run it when ready.",
                    plan.name,
                    plan.steps.len()
                );
                self.chat.push(Role::Assistant, MessageBody::Text(text));
                self.chat.attach_plan(plan);
                self.engine.send(EngineCommand::ListPlans);
            }
            EngineEvent::EditProposed { edit } => {
                self.chat.busy = None;
                self.chat.push(
                    Role::Assistant,
                    MessageBody::Edit {
                        edit,
                        resolution: None,
                    },
                );
            }
            EngineEvent::PlanDeleted { plan_id, message } => {
                if let Some(session) = chat_store::find_by_plan(&self.paths.data_dir, &plan_id) {
                    chat_store::delete(&self.paths.data_dir, &session.id);
                    if session.id == self.session_id {
                        self.new_chat();
                    }
                    self.sessions = chat_store::list(&self.paths.data_dir);
                }
                self.chat.push(Role::Assistant, MessageBody::Text(message));
            }
            EngineEvent::PlanLoaded { plan } => {
                self.chat.attach_plan(plan);
                let session_id = self.session_id.clone();
                self.navigate(View::Chat, Some(&session_id));
            }
            EngineEvent::PlanList(items) => {
                if self.chat.expect_plan_index {
                    self.chat.expect_plan_index = false;
                    self.chat
                        .push(Role::Assistant, MessageBody::PlanIndex(items.clone()));
                }
                self.plans.plans = items;
            }
            EngineEvent::RunStarted {
                run_id,
                plan,
                inputs,
            } => {
                self.chat.push(
                    Role::Assistant,
                    MessageBody::RunStarted {
                        run_id: run_id.clone(),
                        text: format!("Run of “{}” has started.", plan.name),
                        active: true,
                    },
                );
                self.chat
                    .bind_run(&plan, chat::new_binding(&run_id, inputs));
            }
            EngineEvent::StepProgress(progress) => {
                if let Some(event) = progress.transcript.clone() {
                    self.chat.append_agent_transcript(event);
                }
                if let Some(binding) = self.chat.binding_mut(&progress.run_id) {
                    binding
                        .statuses
                        .insert(progress.step_id.clone(), progress.status);
                    if let Some(iteration) = progress.iteration {
                        binding
                            .iterations
                            .entry(progress.step_id.clone())
                            .or_default()
                            .push(iteration);
                    }
                    if let Some(fan_out_progress) = progress.fan_out_progress {
                        binding
                            .fan_out_progress
                            .insert(progress.step_id.clone(), fan_out_progress);
                    }
                    if let Some(error) = progress.error {
                        binding.errors.insert(progress.step_id, error);
                    }
                }
            }
            EngineEvent::RunFinished { run } => {
                self.chat.apply_finished_run(&run);
                let summary = chat::run_summary_text(&run);
                let body = if run.status.is_failed() {
                    MessageBody::RunFailed {
                        run_id: run.id.clone(),
                        text: summary,
                        repair_requested: false,
                    }
                } else {
                    MessageBody::RunCompleted {
                        run_id: run.id.clone(),
                        text: summary,
                    }
                };
                self.chat.push(Role::Assistant, body);
                self.engine.send(EngineCommand::ListRuns);
            }
            EngineEvent::RunList(items) => {
                if self.chat.expect_run_index {
                    self.chat.expect_run_index = false;
                    self.chat
                        .push(Role::Assistant, MessageBody::RunIndex(items.clone()));
                }
                self.plans.runs = items;
            }
            EngineEvent::RunInspected { run, plan } => {
                let binding = chat::binding_from_run(&run);
                self.chat.attach_plan(plan);
                self.chat.workspace_run = Some(binding);
                self.chat.reveal_run_details = true;
                let session_id = self.session_id.clone();
                self.navigate(View::Chat, Some(&session_id));
            }
            EngineEvent::RunReadOnlyInspected { run } => {
                self.runs_view.inspected = Some(run);
            }
            EngineEvent::HumanNeeded { request, .. } => {
                self.chat
                    .push(Role::Assistant, chat::human_message(request));
                let session_id = self.session_id.clone();
                self.navigate(View::Chat, Some(&session_id));
            }
            EngineEvent::PatchProposed { patch } => {
                self.chat.busy = None;
                self.chat.push(
                    Role::Assistant,
                    MessageBody::Patch {
                        patch,
                        resolution: None,
                        resume_requested: false,
                    },
                );
            }
            EngineEvent::WorldFixProposed { fix } => {
                self.chat.busy = None;
                self.chat
                    .push(Role::Assistant, MessageBody::Text(world_fix_message(&fix)));
            }
            EngineEvent::PatchResolved { patch_id, message } => {
                self.chat.resolve_patch(&patch_id, &message);
            }
            EngineEvent::EditResolved { edit_id, message } => {
                self.chat.resolve_edit(&edit_id, &message);
            }
            EngineEvent::Catalog(tools) => {
                if self.chat.expect_tool_index {
                    self.chat.expect_tool_index = false;
                    self.chat
                        .push(Role::Assistant, MessageBody::ToolIndex(tools.clone()));
                }
                self.tools = tools;
            }
            EngineEvent::ToolSynthesized { entry } => {
                self.mcp.synthesizing = false;
                self.mcp.synth_error = None;
                self.mcp.describe_text.clear();
                let mut draft = mcp::ToolDraft::from_entry(&entry);
                // Not yet saved — treat it as a new tool, not an edit of an
                // existing catalog entry, so Save always inserts rather than
                // trying to rename a catalog entry that doesn't exist yet.
                draft.editing = None;
                self.mcp.draft = Some(draft);
                self.mcp.oauth = mcp::OAuthEditorState::default();
            }
            EngineEvent::ToolSynthesisFailed { message } => {
                self.mcp.synthesizing = false;
                self.mcp.synth_error = Some(message);
            }
            EngineEvent::PatchList(patches) => {
                self.patches = patches;
            }
            EngineEvent::ScheduleSaved { message, .. } => {
                self.schedules_view.creating = false;
                self.schedules_view.expression.clear();
                self.chat.push(Role::Assistant, MessageBody::Text(message));
            }
            EngineEvent::ScheduleSaveFailed { message, .. } => {
                self.schedules_view.creating = false;
                self.chat.push(Role::Assistant, MessageBody::Error(message));
                let session_id = self.session_id.clone();
                self.navigate(View::Chat, Some(&session_id));
            }
            EngineEvent::ScheduleList(schedules) => {
                if self.settings.draft.keep_running_in_background.is_none()
                    && schedules.iter().any(|schedule| schedule.enabled)
                {
                    self.settings.draft.keep_running_in_background = Some(true);
                    self.save_settings();
                }
                if self.chat.expect_schedule_index {
                    self.chat.expect_schedule_index = false;
                    self.chat.push(
                        Role::Assistant,
                        MessageBody::ScheduleIndex(schedules.clone()),
                    );
                }
                self.schedules = schedules;
            }
            EngineEvent::Settings(settings) => {
                self.settings.draft = settings;
            }
            EngineEvent::UpdateAvailable { version, url } => {
                self.update_available = Some((version, url));
            }
            EngineEvent::CodexSandboxTestResult(result) => {
                self.settings.codex_sandbox_test = Some(result);
            }
            EngineEvent::McpOAuthStatus { tool_name, status } => {
                if !mcp_event_targets_draft(&self.mcp, &tool_name) {
                    return;
                }
                self.mcp.oauth.status = Some(status);
                self.mcp.oauth.connecting = false;
                self.mcp.oauth.authorization_url = None;
            }
            EngineEvent::McpAuthorizationStarted {
                tool_name,
                authorization_url,
            } => {
                if !mcp_event_targets_draft(&self.mcp, &tool_name) {
                    return;
                }
                self.mcp.oauth.status =
                    Some(crate::tools::oauth::OAuthConnectionStatus::AuthorizationPending);
                self.mcp.oauth.authorization_url = Some(authorization_url);
                self.mcp.oauth.connecting = true;
            }
            EngineEvent::McpAuthorizationFinished { tool_name, result } => {
                if !mcp_event_targets_draft(&self.mcp, &tool_name) {
                    return;
                }
                self.mcp.oauth.connecting = false;
                self.mcp.oauth.authorization_url = None;
                match result {
                    Ok(status) => {
                        self.mcp.oauth.status = Some(status);
                        self.mcp.notice = Some("MCP authorization connected.".to_owned());
                    }
                    Err(message) => {
                        self.mcp.oauth.status =
                            Some(crate::tools::oauth::OAuthConnectionStatus::Disconnected);
                        self.mcp.error = Some(message);
                    }
                }
            }
            EngineEvent::McpServerToolsListed { result } => {
                self.mcp.discovering = false;
                self.mcp.discovery = Some(match result {
                    Ok(tools) => mcp::McpDiscoveryState {
                        tools: tools
                            .into_iter()
                            .map(|tool| mcp::DiscoveredToolDraft {
                                tool,
                                selected: true,
                            })
                            .collect(),
                        error: None,
                    },
                    Err(message) => mcp::McpDiscoveryState {
                        tools: Vec::new(),
                        error: Some(message),
                    },
                });
            }
            EngineEvent::SchedulerUnavailable { holder_pid } => {
                // Another live instance owns the scheduler loop. Purely
                // informational for the desktop — schedules still fire, just
                // from the holder. Logged so the situation is diagnosable.
                tracing::warn!(
                    ?holder_pid,
                    "scheduler is running in another instance; this window will not fire schedules"
                );
            }
        }
    }

    fn handle_routed_event(&mut self, routed: RoutedEngineEvent) {
        let focus_target = routed.session_id.is_some()
            && matches!(
                &routed.event,
                EngineEvent::PlanLoaded { .. }
                    | EngineEvent::RunInspected { .. }
                    | EngineEvent::ScheduleSaveFailed { .. }
            );
        let mut target_session = routed.session_id;
        match &routed.event {
            EngineEvent::PlanLoaded { plan } | EngineEvent::RunInspected { plan, .. } => {
                target_session = self.ensure_plan_chat(plan);
            }
            EngineEvent::ScheduleSaved { plan_id, .. }
            | EngineEvent::ScheduleSaveFailed { plan_id, .. } => {
                target_session = self.ensure_plan_chat_for_id(plan_id);
            }
            EngineEvent::RunStarted { run_id, plan, .. } => {
                if target_session.is_none() {
                    target_session = self.ensure_plan_chat(plan);
                }
                if let Some(session_id) = &target_session {
                    self.run_sessions.insert(run_id.clone(), session_id.clone());
                }
            }
            EngineEvent::StepProgress(progress) => {
                if target_session.is_none() {
                    target_session = self.run_sessions.get(&progress.run_id).cloned();
                }
            }
            EngineEvent::RunFinished { run } => {
                if target_session.is_none() {
                    target_session = self.run_sessions.get(&run.id).cloned().or_else(|| {
                        chat_store::find_by_plan(&self.paths.data_dir, &run.plan_id)
                            .map(|session| session.id)
                    });
                }
            }
            EngineEvent::HumanNeeded { run_id, .. } if target_session.is_none() => {
                target_session = self.run_sessions.get(run_id).cloned();
            }
            _ => {}
        }

        let Some(target_session) = target_session else {
            self.handle_event(routed.event);
            return;
        };
        if target_session == self.session_id {
            self.handle_event(routed.event);
            return;
        }
        if focus_target {
            self.open_chat(&target_session);
            self.handle_event(routed.event);
            return;
        }

        // Apply background results to their owning conversation without
        // stealing focus from the chat the user is currently reading.
        self.autosave_chat();
        let target = self
            .parked_chats
            .remove(&target_session)
            .or_else(|| self.load_parked_chat(&target_session).ok());
        let Some(target) = target else {
            // Should not happen: sessions are persisted before navigation and
            // after every chat frame. If it does, make the loss visible.
            tracing::warn!(
                session = %target_session,
                event = ?routed.event,
                "dropping engine event for a session that is not on disk"
            );
            return;
        };

        let current_chat = std::mem::replace(&mut self.chat, target.chat);
        let current_session = std::mem::replace(&mut self.session_id, target_session);
        let current_created = std::mem::replace(&mut self.session_created_at, target.created_at);
        let current_fingerprint =
            std::mem::replace(&mut self.last_saved_fingerprint, target.saved_fingerprint);
        let current_view = self.view;

        self.handle_event(routed.event);
        self.autosave_chat();

        let updated_target = ParkedChat {
            chat: std::mem::replace(&mut self.chat, current_chat),
            created_at: std::mem::replace(&mut self.session_created_at, current_created),
            saved_fingerprint: std::mem::replace(
                &mut self.last_saved_fingerprint,
                current_fingerprint,
            ),
        };
        let target_session = std::mem::replace(&mut self.session_id, current_session);
        self.parked_chats.insert(target_session, updated_target);
        self.view = current_view;
    }

    // ─── Sidebar ──────────────────────────────────────────────────────────────

    fn sidebar(&mut self, ui: &mut Ui) {
        ui.add_space(18.0);
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.label(theme::title("INXM", 16.0));
            ui.label(theme::title("// local", 16.0).color(theme::accent()));
        });
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.label(
                RichText::new("COMPILED AI PLANS")
                    .size(theme::FONT_SMALL)
                    .color(theme::text_faint()),
            );
        });
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.label(
                RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                    .size(theme::FONT_SMALL)
                    .color(theme::text_faint()),
            );
        });
        ui.add_space(20.0);

        // fold (not filter+find): every item must render each frame, in order.
        let update_available = self.update_available.is_some();
        let running_count = runs::count_running(&self.plans.runs);
        let on_chat = self.view == View::Chat;
        let (switch_to, selected_rect) = NAV_ITEMS
            .iter()
            .filter(|(view, _, _)| view_available(*view, self.agent_mode))
            .fold(
                (None, None),
                |(clicked, selected_rect), (view, icon, label)| {
                    let selected = self.view == *view;
                    let badge = match view {
                        View::Settings if update_available => NavBadge::Dot,
                        // Hidden entirely while nothing runs — the pill only
                        // exists to answer "is something running right now?".
                        View::Runs if running_count > 0 => NavBadge::Running(running_count),
                        _ => NavBadge::None,
                    };
                    let response = nav_item(ui, *icon, label, selected, badge);
                    // When the Chat item is already selected, advertise that a
                    // click starts a new conversation.
                    let response = if *view == View::Chat && on_chat {
                        response.on_hover_text("New chat")
                    } else {
                        response
                    };
                    (
                        if response.clicked() {
                            Some(*view)
                        } else {
                            clicked
                        },
                        if selected {
                            Some(response.rect)
                        } else {
                            selected_rect
                        },
                    )
                },
            );

        // One shared indicator that slides between the nav items.
        if let Some(rect) = selected_rect {
            let y = ui.ctx().animate_value_with_time(
                Id::new("nav_indicator_y"),
                rect.center().y,
                NAV_INDICATOR_SLIDE_SECS,
            );
            let bar = egui::Rect::from_center_size(
                egui::pos2(rect.left() + 10.0, y),
                vec2(2.0, rect.height() - 12.0),
            );
            ui.painter()
                .rect_filled(bar, egui::CornerRadius::same(2), theme::accent());
        }

        if let Some(view) = switch_to {
            if self.onboarding_active {
                // Using the nav at all is as deliberate a choice to move on
                // as the "Skip for now" button — let the click through
                // rather than swallowing it behind a still-showing card.
                self.dismiss_onboarding();
            }
            if view == View::Chat && self.view == View::Chat {
                // Re-clicking the Chat nav item while already on Chat starts a
                // new conversation — the same action as the "+ New" button.
                self.new_chat();
            } else if view != self.view {
                self.navigate(view, None);
                if view == View::Plans || view == View::Runs || view == View::Schedules {
                    self.refresh_plans_overview();
                }
                if view == View::Mcp {
                    self.engine.send(EngineCommand::ListTools);
                }
            }
        }

        // Cap the chats section to leave a guaranteed gap for the footer.
        // Without this bound, a `bottom_up` footer and a growing chat list can
        // occupy the same vertical range and visually overlap.
        if !self.agent_mode {
            let chats_max_h = (ui.available_height() - SIDEBAR_FOOTER_HEIGHT).max(0.0);
            ui.allocate_ui(egui::vec2(ui.available_width(), chats_max_h), |ui| {
                self.chats_section(ui);
            });
        }

        self.footer(ui);
    }

    /// Recent conversations with a "new chat" action.
    fn chats_section(&mut self, ui: &mut Ui) {
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            widgets::section_label(ui, "Plan-Chats");
            if !self.on_blank_chat() {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(12.0);
                    if widgets::primary_icon_button(ui, widgets::Icon::Plus)
                        .on_hover_text("New chat")
                        .clicked()
                    {
                        self.new_chat();
                    }
                });
            }
        });
        ui.add_space(4.0);

        enum ChatAction {
            Open(String),
            Delete(String),
        }
        let action = egui::ScrollArea::vertical()
            .id_salt("sidebar_chats")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                self.sessions.iter().fold(None, |action, session| {
                    let current = session.id == self.session_id;
                    let parked = self.parked_chats.get(&session.id);
                    let active = if current {
                        self.chat.is_active()
                    } else {
                        parked.is_some_and(|parked| parked.chat.is_active())
                    };
                    ui.horizontal(|ui| {
                        ui.add_space(16.0);
                        if sidebar_chat_awaiting_input(
                            current,
                            session.awaiting_input,
                            self.chat.awaiting_input(),
                            parked.map(|parked| parked.chat.awaiting_input()),
                        ) {
                            widgets::status_dot(ui, theme::warn(), true);
                        } else if active {
                            widgets::status_dot(ui, theme::active(), true);
                        } else {
                            widgets::status_dot(ui, theme::ok(), false);
                        }
                        ui.add_space(6.0);
                        let title = RichText::new(&session.title).size(theme::FONT_SMALL).color(
                            match current {
                                true => theme::text(),
                                false => theme::text_muted(),
                            },
                        );
                        let can_delete = sidebar_chat_can_delete(session.plan_id.as_deref());
                        let width = ui.available_width() - if can_delete { 40.0 } else { 0.0 };
                        let label = ui
                            .scope(|ui| {
                                ui.set_max_width(width.max(48.0));
                                ui.add(egui::Label::new(title).truncate().sense(Sense::click()))
                            })
                            .inner;
                        let delete = can_delete.then(|| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.add_space(10.0);
                                ui.add(
                                    egui::Label::new(
                                        RichText::new("✕")
                                            .size(theme::FONT_SMALL)
                                            .color(theme::text_faint()),
                                    )
                                    .sense(Sense::click()),
                                )
                            })
                            .inner
                        });
                        if label.hovered() || delete.as_ref().is_some_and(egui::Response::hovered) {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        match (
                            delete.as_ref().is_some_and(egui::Response::clicked),
                            label.clicked(),
                        ) {
                            (true, _) => Some(ChatAction::Delete(session.id.clone())),
                            (_, true) => Some(ChatAction::Open(session.id.clone())),
                            _ => action,
                        }
                    })
                    .inner
                })
            })
            .inner;
        match action {
            Some(ChatAction::Open(id)) => self.open_chat(&id),
            Some(ChatAction::Delete(id)) => self.delete_chat(&id),
            None => {}
        }
    }

    fn footer(&mut self, ui: &mut Ui) {
        // Footer. Labels must be truncated: content wider than the panel
        // expands the panel's occupied rect and shifts the whole layout.
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                widgets::truncated_label(
                    ui,
                    RichText::new(short_path(&self.data_dir))
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                    12.0,
                );
            })
            .response
            .on_hover_text(format!(
                "Data directory: plans, runs, patches, tools.yaml\n{}",
                self.data_dir
            ));
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                match &self.mcp_status {
                    mcp_server::ServerStatus::Running { .. } => {
                        widgets::status_dot(ui, theme::ok(), false);
                        widgets::truncated_label(
                            ui,
                            RichText::new(self.mcp_status.label())
                                .size(theme::FONT_SMALL)
                                .color(theme::text_muted()),
                            12.0,
                        );
                    }
                    mcp_server::ServerStatus::Starting { .. } => {
                        widgets::status_dot(ui, theme::warn(), true);
                        widgets::truncated_label(
                            ui,
                            RichText::new(self.mcp_status.label())
                                .size(theme::FONT_SMALL)
                                .color(theme::text_muted()),
                            12.0,
                        );
                    }
                    mcp_server::ServerStatus::Failed { .. } => {
                        widgets::status_dot(ui, theme::warn(), false);
                        ui.label(
                            RichText::new(self.mcp_status.label())
                                .size(theme::FONT_SMALL)
                                .color(theme::warn()),
                        )
                        .on_hover_text(format!(
                            "{}\nChange the MCP port under Settings and restart the app.",
                            self.mcp_status.error().unwrap_or("port bind failed")
                        ));
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                match &self.backend {
                    Some(name) => {
                        widgets::status_dot(ui, theme::ok(), false);
                        widgets::truncated_label(
                            ui,
                            RichText::new(name)
                                .size(theme::FONT_SMALL)
                                .color(theme::text_muted()),
                            12.0,
                        );
                    }
                    None => {
                        widgets::status_dot(ui, theme::warn(), false);
                        ui.label(
                            RichText::new("no compiler set")
                                .size(theme::FONT_SMALL)
                                .color(theme::warn()),
                        )
                        .on_hover_text(
                            "Choose an LLM connection under Settings: API key, \
                             signed-in account CLI, or compatible endpoint URL.",
                        );
                    }
                }
            });
        });
    }
}

/// Trailing indicator on a navigation row.
#[derive(Debug, Clone, Copy, PartialEq)]
enum NavBadge {
    None,
    /// Unobtrusive dot — an update is available (Settings).
    Dot,
    /// "N running" pill with a pulsing dot (Runs). Only constructed while
    /// N > 0; the badge disappears entirely when nothing is running.
    Running(usize),
}

fn mcp_event_targets_draft(mcp: &McpState, tool_name: &str) -> bool {
    mcp.draft
        .as_ref()
        .is_some_and(|draft| draft.name.trim() == tool_name)
}

/// A full-width navigation row with animated hover/selection state and a
/// painted vector icon (font-independent).
fn nav_item(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    selected: bool,
    badge: NavBadge,
) -> egui::Response {
    let id = Id::new("nav_item").with(label);
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, NAV_ITEM_HEIGHT), Sense::click());

    let t_selected = ui
        .ctx()
        .animate_bool_with_time(id.with("sel"), selected, NAV_ANIM_SECS);
    let t_hover =
        ui.ctx()
            .animate_bool_with_time(id.with("hov"), response.hovered(), NAV_ANIM_SECS);

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let fill = theme::mix(
            theme::panel(),
            theme::selected(),
            t_selected.max(t_hover * 0.65),
        );
        painter.rect_filled(
            rect.shrink2(vec2(8.0, 2.0)),
            egui::CornerRadius::same(theme::RADIUS_WIDGET),
            fill,
        );

        // Accent bar that grows in when selected.
        let bar_height = (rect.height() - 14.0) * t_selected;
        if bar_height > 0.5 {
            let bar = egui::Rect::from_center_size(
                egui::pos2(rect.left() + 10.0, rect.center().y),
                vec2(2.0, bar_height),
            );
            painter.rect_filled(bar, egui::CornerRadius::same(2), theme::accent());
        }

        let icon_color = theme::mix(theme::icon(), theme::accent(), t_selected);
        let icon_rect = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 30.0, rect.center().y),
            vec2(16.0, 16.0),
        );
        widgets::paint_icon(painter, icon_rect, icon, icon_color);

        let text_color = theme::mix(theme::text_muted(), theme::text(), t_selected);
        painter.text(
            egui::pos2(rect.left() + 50.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(theme::FONT_BODY),
            text_color,
        );
        match badge {
            NavBadge::None => {}
            NavBadge::Dot => {
                // Unobtrusive dot signaling an available update — deliberately
                // not a pulsing/attention-grabbing indicator.
                let dot_center = egui::pos2(rect.right() - 14.0, rect.top() + 10.0);
                painter.circle_filled(dot_center, 3.5, theme::accent());
            }
            NavBadge::Running(count) => {
                // "N running" pill: pulsing amber dot + count, right-aligned.
                // This one *is* attention-grabbing on purpose — it answers
                // "is something running?" from any screen.
                const DOT_R: f32 = 3.0;
                let galley = painter.layout_no_wrap(
                    format!("{count} running"),
                    egui::FontId::proportional(theme::FONT_SMALL),
                    theme::text_muted(),
                );
                let pill_h = galley.size().y + 6.0;
                let pill_w = galley.size().x + DOT_R * 2.0 + 20.0;
                let pill = egui::Rect::from_min_size(
                    egui::pos2(rect.right() - 14.0 - pill_w, rect.center().y - pill_h / 2.0),
                    vec2(pill_w, pill_h),
                );
                painter.rect(
                    pill,
                    egui::CornerRadius::same(theme::RADIUS_WIDGET),
                    theme::with_alpha(theme::warn(), 0.14),
                    egui::Stroke::new(1.0_f32, theme::with_alpha(theme::warn(), 0.35)),
                    egui::StrokeKind::Inside,
                );
                let t = anim::pulse(ui.input(|i| i.time), anim::PULSE_SECS);
                let dot_center = egui::pos2(pill.left() + 7.0 + DOT_R, pill.center().y);
                painter.circle_filled(
                    dot_center,
                    DOT_R + 2.5 * t,
                    theme::with_alpha(theme::warn(), 0.25 * (1.0 - t) + 0.05),
                );
                painter.circle_filled(dot_center, DOT_R, theme::warn());
                painter.galley(
                    egui::pos2(
                        dot_center.x + DOT_R + 5.0,
                        pill.center().y - galley.size().y / 2.0,
                    ),
                    galley,
                    theme::text_muted(),
                );
                // The pulse animates continuously while visible.
                ui.ctx().request_repaint();
            }
        }
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }

    response
}

/// Compact tail of a path for the sidebar footer, e.g. `…/inxm/inxm-local`.
fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    match parts.len() {
        0..=2 => path.to_owned(),
        n => format!("…/{}/{}", parts[n - 2], parts[n - 1]),
    }
}

/// Live state wins over the persisted summary wherever it exists: the
/// current chat's, so an approval click clears its attention indicator
/// immediately, and a parked chat's, because the store deliberately drops
/// transient in-flight flags (`design_pending`, `awaiting_compile`) — a
/// conversation still compiling its approved design would otherwise show
/// the orange "needs input" dot while the app is actually working (#44).
/// The stored summary remains the fallback for sessions not held in memory
/// (e.g. after a restart), where a lost in-flight compile really does leave
/// the design awaiting approval again.
fn sidebar_chat_awaiting_input(
    current: bool,
    stored_awaiting_input: bool,
    live_awaiting_input: bool,
    parked_awaiting_input: Option<bool>,
) -> bool {
    if current {
        live_awaiting_input
    } else {
        parked_awaiting_input.unwrap_or(stored_awaiting_input)
    }
}

/// What the assistant says when a solution design arrives and the DESIGN
/// phase waits for the approve button.
const DESIGN_READY_ANNOUNCEMENT: &str = "Here's a proposed solution design — review it in the \
     panel next to the chat. Type to give feedback, or approve it to compile the plan. The plan \
     may still change during testing and verification.";
/// The same moment with auto mode on: the design is already being compiled,
/// so nothing is asked of the user beyond reading it.
const DESIGN_READY_AUTO_ANNOUNCEMENT: &str = "Here's the solution design — auto mode is \
     compiling it into a plan right away. Review it in the panel next to the chat; the plan may \
     still change during testing and verification.";

/// Pick the design announcement that matches what actually happens next, so
/// auto mode never invites an approval it has already skipped.
fn design_ready_announcement(auto_mode: bool) -> &'static str {
    match auto_mode {
        true => DESIGN_READY_AUTO_ANNOUNCEMENT,
        false => DESIGN_READY_ANNOUNCEMENT,
    }
}

/// Chats become part of a plan's durable lifecycle once a plan is materialized.
/// Those chats are removed only as part of deleting that plan from the overview.
fn sidebar_chat_can_delete(plan_id: Option<&str>) -> bool {
    plan_id.is_none()
}

/// What the background poll should re-fetch, given where the user is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackgroundRefresh {
    /// Run history only — the minimum poll that keeps the nav's "N running"
    /// badge truthful from any screen: MCP- and scheduler-
    /// triggered runs start with no UI interaction at all, so without a
    /// standing poll the badge could not notice them.
    RunningBadge,
    /// A chat with an attached plan polls run history so its workspace
    /// reflects runs finishing in the background.
    ChatRuns,
    /// The Plans overview and the Runs view re-fetch everything they render
    /// (plans, runs, schedules) so they track scheduled runs and activity
    /// from other sessions.
    PlansOverview,
}

fn background_refresh(view: View, chat_has_plan: bool) -> BackgroundRefresh {
    if view == View::Plans || view == View::Runs {
        BackgroundRefresh::PlansOverview
    } else if chat_has_plan {
        // Matches the pre-#46 behavior: a session with an attached plan keeps
        // its run history fresh even while the user is on another view.
        BackgroundRefresh::ChatRuns
    } else {
        BackgroundRefresh::RunningBadge
    }
}

/// Render a world-fix proposal as a chat message: what is wrong in the
/// environment, how the human can fix it, and how to resume afterwards.
fn world_fix_message(fix: &crate::storage::world_fixes::WorldFix) -> String {
    let mut text = format!(
        "🌍 Repair diagnosis: the plan is fine — the environment caused the failure.\n\n\
         Step `{}` failed because: {}\n\nTo fix the environment:\n",
        fix.failing_step_id, fix.diagnosis
    );
    for (index, action) in fix.remediation.iter().enumerate() {
        text.push_str(&format!("{}. {}\n", index + 1, action.description));
        if let Some(command) = &action.command {
            text.push_str(&format!("   $ {command}\n"));
        }
    }
    text.push_str(&format!(
        "\nOnce done, run /resume {} — the plan stays unchanged at v{}.",
        fix.run_id, fix.plan_version
    ));
    text
}

impl eframe::App for InxmApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.settings.draft.theme_preference == engine::ThemePreference::System {
            let dark_mode =
                engine::resolve_dark_mode(engine::ThemePreference::System, ctx.system_theme());
            if dark_mode != theme::is_dark() {
                theme::apply(ctx, dark_mode);
            }
        }
        if self.tray_icon_dark != theme::is_dark() {
            self.tray_icon_dark = theme::is_dark();
            if let Some(tray) = &self.tray {
                tray.set_dark_mode(self.tray_icon_dark);
            }
        }
        self.handle_tray_actions(ctx);
        self.handle_close_request(ctx);
        self.track_view_time(ctx.input(|input| input.focused));
        self.shot.tick(ctx);
        if self.onboarding_active {
            self.onboarding.poll(ctx);
        }
        let refresh = background_refresh(self.view, self.chat.plan.is_some());
        if self.last_run_refresh.elapsed() >= RUN_LIST_REFRESH_INTERVAL {
            match refresh {
                BackgroundRefresh::PlansOverview => self.refresh_plans_overview(),
                BackgroundRefresh::ChatRuns | BackgroundRefresh::RunningBadge => {
                    self.engine.send(EngineCommand::ListRuns)
                }
            }
            self.last_run_refresh = std::time::Instant::now();
        }
        // egui only repaints on input; without this, an untouched window
        // never re-enters `update`, so the poll above would never fire
        // and the view (and the "N running" nav badge) would sit stale
        // until the user moves the mouse.
        ctx.request_repaint_after(RUN_LIST_REFRESH_INTERVAL);
        while let Ok(status) = self.mcp_status_rx.try_recv() {
            self.mcp_status = status;
        }
        while let Ok(event) = self.events.try_recv() {
            self.handle_routed_event(event);
        }
        // MCP work has no desktop-engine event to trigger a refresh. Drain
        // the counter once per frame so a burst of completed detached tasks
        // performs one complete overview refresh, not one per task.
        if self.activities.take_mcp_completions() > 0 {
            self.refresh_plans_overview();
        }
        self.autosave_chat();

        // Cross-view fade: restart the clock whenever the view changes.
        let now = ctx.input(|i| i.time);
        if self.view != self.last_view {
            self.last_view = self.view;
            self.view_switched_at = now;
        }
        let fade = anim::ease_out_cubic(
            (((now - self.view_switched_at) / VIEW_FADE_SECS).clamp(0.0, 1.0)) as f32,
        );
        if fade < 1.0 {
            ctx.request_repaint();
        }

        // Alt+Left or the mouse "back" button retrace the nav history from
        // anywhere, matching the ← button rendered at the top of the
        // content area.
        let back_requested = ctx.input(|i| {
            (i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft))
                || i.pointer.button_pressed(egui::PointerButton::Extra1)
        });
        if back_requested && !self.nav_history.is_empty() {
            self.go_back();
        }

        // Both sidebars resize through the same custom handle:
        // egui's built-in panel resizing is disabled and the width is kept in
        // persisted memory instead, so the nav and the plan panel drag the
        // same way with the same, quieter visuals.
        let nav_width_id = egui::Id::new("nav_panel_width");
        let nav_width = widgets::panel_width(ctx, nav_width_id, theme::SIDEBAR_WIDTH);
        let nav_panel = egui::SidePanel::left("nav_panel")
            .exact_width(nav_width)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::panel())
                    .stroke(egui::Stroke::new(1.0_f32, theme::divider())),
            )
            .show_animated(ctx, !self.sidebar_collapsed, |ui| self.sidebar(ui));
        if !self.sidebar_collapsed
            && let Some(nav_panel) = &nav_panel
        {
            widgets::panel_resize_handle(
                ctx,
                nav_width_id,
                nav_panel.response.rect,
                widgets::PanelEdge::Right,
                NAV_PANEL_WIDTH_RANGE,
            );
        }

        // The plan/design workspace is a full-height sidebar docked to the
        // window edge, so it renders at ctx level before the
        // central panel instead of inside the chat view's margin frame.
        if self.view == View::Chat && !self.onboarding_active {
            let chat_engine = self.engine.scoped(self.session_id.clone());
            let sources = chat::SuggestionSources {
                plans: &self.plans.plans,
                runs: &self.plans.runs,
                patches: &self.patches,
            };
            let action = chat::show_side_panel(ctx, &mut self.chat, &sources, &chat_engine);
            self.handle_chat_action(action);
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::bg()))
            .show(ctx, |ui| {
                ui.set_opacity(fade);
                // Every view's top bar and content share this one margin
                // frame, so the Back/Close row and a page's own left title /
                // right-aligned actions line up on the same horizontal inset
                // instead of drifting between differently-margined containers.
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(20, 8))
                    .show(ui, |ui| {
                        if self.onboarding_active {
                            let action = onboarding::show(
                                ui,
                                &mut self.onboarding,
                                &mut self.settings.draft,
                                &mut self.onboarding_telemetry_opt_in,
                            );
                            if matches!(
                                action,
                                Some(OnboardingAction::Complete | OnboardingAction::Skip)
                            ) {
                                self.dismiss_onboarding();
                            }
                            return;
                        }
                        self.top_bar(ui);
                        match self.view {
                            View::Chat => {
                                let chat_engine = self.engine.scoped(self.session_id.clone());
                                let sources = chat::SuggestionSources {
                                    plans: &self.plans.plans,
                                    runs: &self.plans.runs,
                                    patches: &self.patches,
                                };
                                let action = chat::show(
                                    ui,
                                    &mut self.chat,
                                    &sources,
                                    &chat_engine,
                                    self.settings.draft.auto_mode,
                                );
                                self.handle_chat_action(action);
                                // Persist in the same frame the message is
                                // pushed and its engine command fired. The
                                // next frame may start with navigation
                                // (sidebar and Alt+Left both run before this
                                // panel) that switches the session; a session
                                // that never reached disk would then lose
                                // every engine event routed to it.
                                self.autosave_chat();
                            }
                            View::Plans => {
                                let action = plans::show(
                                    ui,
                                    &mut self.plans,
                                    &self.schedules,
                                    &self.engine,
                                    self.agent_mode,
                                );
                                match action {
                                    Some(PlansAction::OpenPlan(plan_id)) if !self.agent_mode => {
                                        self.open_plan_chat(&plan_id);
                                    }
                                    Some(PlansAction::OpenRun { plan_id, run_id })
                                        if !self.agent_mode =>
                                    {
                                        self.open_plan_chat(&plan_id);
                                        self.engine.send_from(
                                            self.session_id.clone(),
                                            EngineCommand::InspectRun { run_id },
                                        );
                                    }
                                    Some(PlansAction::RunPlan { plan_id }) => {
                                        if self.agent_mode {
                                            // Fire the run without surfacing
                                            // the plan's chat — its progress
                                            // still lands in the overview's
                                            // stats and recent-runs list.
                                            self.engine.send(EngineCommand::RunPlan {
                                                plan_ref: plan_id,
                                                inputs: Default::default(),
                                            });
                                        } else {
                                            self.open_plan_chat(&plan_id);
                                            self.engine.send_from(
                                                self.session_id.clone(),
                                                EngineCommand::RunPlan {
                                                    plan_ref: plan_id,
                                                    inputs: Default::default(),
                                                },
                                            );
                                        }
                                    }
                                    Some(PlansAction::SchedulePlan(plan_id))
                                        if !self.agent_mode =>
                                    {
                                        self.schedules_view.start_for_plan(&plan_id);
                                        self.navigate(View::Schedules, None);
                                    }
                                    Some(PlansAction::ViewAllRuns) => {
                                        // `navigate` records the origin, so
                                        // Back returns to this overview.
                                        self.navigate(View::Runs, None);
                                        self.refresh_plans_overview();
                                    }
                                    _ => {}
                                }
                            }
                            View::Runs => {
                                let action = runs::show(
                                    ui,
                                    &mut self.runs_view,
                                    &self.plans.runs,
                                    &self.plans.plans,
                                    &self.engine,
                                    self.agent_mode,
                                );
                                match action {
                                    // Same semantics as the Plans view's
                                    // Inspect: open the run inside its plan's
                                    // chat — which doesn't exist in agent mode.
                                    Some(RunsAction::Inspect { plan_id, run_id })
                                        if !self.agent_mode =>
                                    {
                                        self.open_plan_chat(&plan_id);
                                        self.engine.send_from(
                                            self.session_id.clone(),
                                            EngineCommand::InspectRun { run_id },
                                        );
                                    }
                                    Some(RunsAction::OpenPlan(plan_id)) if !self.agent_mode => {
                                        self.open_plan_chat(&plan_id);
                                    }
                                    Some(RunsAction::InspectReadOnly { run_id })
                                        if self.agent_mode =>
                                    {
                                        self.engine
                                            .send(EngineCommand::InspectRunReadOnly { run_id });
                                    }
                                    Some(RunsAction::Abort { run_id }) => {
                                        self.engine.send(EngineCommand::AbortRun { run_id });
                                    }
                                    _ => {}
                                }
                            }
                            View::Schedules => {
                                let schedule_engine = self.engine.scoped(self.session_id.clone());
                                schedules::show(
                                    ui,
                                    &mut self.schedules_view,
                                    &self.schedules,
                                    &self.plans.plans,
                                    &schedule_engine,
                                );
                            }
                            View::Mcp => mcp::show(ui, &mut self.mcp, &self.tools, &self.engine),
                            View::Settings => {
                                let action = settings::show(
                                    ui,
                                    &mut self.settings,
                                    &self.environment,
                                    &self.mcp_status,
                                    &self.engine,
                                    self.update_available.as_ref(),
                                );
                                if action == Some(SettingsAction::RestartOnboarding) {
                                    self.restart_onboarding();
                                }
                            }
                        }
                    });
            });
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod background_refresh_tests {
    use super::*;

    #[test]
    fn plans_overview_polls_everything_it_renders() {
        // The overview must autorefresh while
        // visible, with or without a plan chat in the background.
        for view in [View::Plans, View::Runs] {
            assert_eq!(
                background_refresh(view, false),
                BackgroundRefresh::PlansOverview,
                "view {view:?}"
            );
            assert_eq!(
                background_refresh(view, true),
                BackgroundRefresh::PlansOverview,
                "view {view:?}"
            );
        }
    }

    #[test]
    fn a_plan_chat_keeps_polling_runs_from_any_other_view() {
        for view in [View::Chat, View::Schedules, View::Mcp, View::Settings] {
            assert_eq!(
                background_refresh(view, true),
                BackgroundRefresh::ChatRuns,
                "view {view:?}"
            );
        }
    }

    #[test]
    fn every_other_view_still_polls_runs_for_the_nav_badge() {
        // The "N running" badge must stay truthful from any screen — MCP-
        // and scheduler-triggered runs start without any UI interaction.
        for view in [View::Chat, View::Schedules, View::Mcp, View::Settings] {
            assert_eq!(
                background_refresh(view, false),
                BackgroundRefresh::RunningBadge,
                "view {view:?}"
            );
        }
    }
}

#[cfg(test)]
mod nav_tests {
    use super::*;

    fn target(view: View, session: Option<&str>) -> NavTarget {
        NavTarget {
            view,
            session_id: session.map(str::to_owned),
        }
    }

    #[test]
    fn pop_returns_most_recently_pushed() {
        let mut history = Vec::new();
        push_nav(&mut history, target(View::Plans, None));
        push_nav(&mut history, target(View::Chat, Some("a")));
        assert_eq!(pop_nav(&mut history), Some(target(View::Chat, Some("a"))));
        assert_eq!(pop_nav(&mut history), Some(target(View::Plans, None)));
        assert_eq!(pop_nav(&mut history), None);
    }

    #[test]
    fn depth_is_capped_and_oldest_entries_are_forgotten() {
        let mut history = Vec::new();
        for i in 0..(NAV_HISTORY_CAP + 5) {
            push_nav(&mut history, target(View::Chat, Some(&i.to_string())));
        }
        assert_eq!(history.len(), NAV_HISTORY_CAP);
        // The oldest surviving entry is the 6th pushed (indices 0..=4 fell
        // off the front), and the newest is the last one pushed.
        assert_eq!(
            history.first(),
            Some(&target(View::Chat, Some(&5.to_string())))
        );
        assert_eq!(
            history.last(),
            Some(&target(
                View::Chat,
                Some(&(NAV_HISTORY_CAP + 4).to_string())
            ))
        );
    }

    #[test]
    fn empty_history_pops_none() {
        let mut history: Vec<NavTarget> = Vec::new();
        assert_eq!(pop_nav(&mut history), None);
    }
}

#[cfg(test)]
mod shell_tests {
    use super::*;
    use crate::executor::{
        AgentTranscriptEvent, AgentTranscriptStream, ProgressEvent, Run, RunStatus, StepRunStatus,
    };

    #[test]
    fn short_path_keeps_only_the_last_two_segments() {
        assert_eq!(
            short_path("/home/user/projects/inxm/inxm-local"),
            "…/inxm/inxm-local"
        );
        assert_eq!(
            short_path(r"C:\Users\user\AppData\inxm\inxm-local"),
            "…/inxm/inxm-local"
        );
    }

    #[test]
    fn short_paths_pass_through_unshortened() {
        assert_eq!(short_path(".inxm-local"), ".inxm-local");
        assert_eq!(short_path("/data/inxm"), "/data/inxm");
        assert_eq!(short_path(""), "");
    }

    #[test]
    fn current_sidebar_chat_uses_live_attention_state() {
        assert!(!sidebar_chat_awaiting_input(true, true, false, None));
        assert!(sidebar_chat_awaiting_input(true, false, true, None));
        assert!(sidebar_chat_awaiting_input(false, true, false, None));
    }

    #[test]
    fn parked_sidebar_chat_prefers_live_attention_state_over_the_store() {
        // A parked chat compiling its approved design: the store (which
        // drops the transient `awaiting_compile` flag) claims it awaits
        // input, but the live in-process state knows it is working (#44).
        assert!(!sidebar_chat_awaiting_input(
            false,
            true,
            false,
            Some(false)
        ));
        assert!(sidebar_chat_awaiting_input(false, false, false, Some(true)));
        // Sessions not held in memory still fall back to the store.
        assert!(sidebar_chat_awaiting_input(false, true, false, None));
    }

    #[test]
    fn only_chats_without_a_materialized_plan_can_be_deleted() {
        assert!(sidebar_chat_can_delete(None));
        assert!(!sidebar_chat_can_delete(Some("plan-1")));
    }

    #[test]
    fn auto_mode_never_announces_an_approval_it_has_already_skipped() {
        assert!(design_ready_announcement(false).contains("approve"));
        let auto = design_ready_announcement(true);
        assert!(
            auto.contains("auto mode"),
            "the skipped click stays visible"
        );
        assert!(!auto.contains("approve"));
    }

    fn finished_run(status: RunStatus) -> Run {
        let mut run = Run::new("plan-1", 1);
        run.status = status;
        run
    }

    #[test]
    fn headless_writer_persists_assistant_failure_and_run_lifecycle_events() {
        assert!(matches!(
            headless_message_body(EngineEvent::Assistant("hi".to_owned())),
            Some((Role::Assistant, MessageBody::Text(text))) if text == "hi"
        ));
        assert!(matches!(
            headless_message_body(EngineEvent::Failure("boom".to_owned())),
            Some((Role::Assistant, MessageBody::Error(text))) if text == "boom"
        ));

        let failed = finished_run(RunStatus::Failed {
            failed_step_id: "step".to_owned(),
            message: "boom".to_owned(),
        });
        let failed_id = failed.id.clone();
        assert!(matches!(
            headless_message_body(EngineEvent::RunFinished { run: Box::new(failed) }),
            Some((Role::Assistant, MessageBody::RunFailed { run_id, .. })) if run_id == failed_id
        ));

        let succeeded = finished_run(RunStatus::Succeeded);
        let succeeded_id = succeeded.id.clone();
        assert!(matches!(
            headless_message_body(EngineEvent::RunFinished { run: Box::new(succeeded) }),
            Some((Role::Assistant, MessageBody::RunCompleted { run_id, .. }))
                if run_id == succeeded_id
        ));
    }

    #[test]
    fn headless_writer_drops_transient_events() {
        assert!(headless_message_body(EngineEvent::PlanList(Vec::new())).is_none());
        assert!(headless_message_body(EngineEvent::DesignStarted).is_none());
    }

    #[test]
    fn headless_writer_keeps_agent_transcript_progress() {
        let event = ProgressEvent {
            run_id: "run-agent".to_owned(),
            step_id: "code".to_owned(),
            status: StepRunStatus::Running,
            error: None,
            iteration: None,
            fan_out_progress: None,
            transcript: Some(AgentTranscriptEvent {
                run_id: "run-agent".to_owned(),
                step_id: "code".to_owned(),
                stream: AgentTranscriptStream::Stdin,
                content: "May I continue?".to_owned(),
            }),
        };
        assert!(matches!(
            headless_message_body(EngineEvent::StepProgress(Box::new(event))),
            Some((Role::Assistant, MessageBody::AgentTranscript { lines, .. }))
                if lines[0].stream == chat::AgentTranscriptLineStream::Input
        ));
    }

    fn minimal_plan(name: &str) -> crate::plan::types::Plan {
        crate::plan::types::Plan {
            metadata: crate::plan::types::PlanMetadata::new(None),
            name: name.to_owned(),
            description: None,
            inputs: Vec::new(),
            config: Default::default(),
            steps: Vec::new(),
            outputs: Vec::new(),
        }
    }

    #[test]
    fn has_compiled_plans_is_false_for_a_fresh_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = engine::DataPaths::at(tmp.path().to_path_buf());
        assert!(!has_compiled_plans(&paths));
    }

    #[test]
    fn has_compiled_plans_is_true_once_a_plan_is_saved() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = engine::DataPaths::at(tmp.path().to_path_buf());
        let storage = crate::storage::StorageRoot::open(&paths.data_dir).unwrap();
        storage.plans().save(&minimal_plan("test-plan")).unwrap();
        assert!(has_compiled_plans(&paths));
    }

    fn session(id: &str) -> chat_store::SessionSummary {
        chat_store::SessionSummary {
            id: id.to_owned(),
            title: "chat".to_owned(),
            plan_id: None,
            updated_at: chrono::Utc::now(),
            awaiting_input: false,
        }
    }

    #[test]
    fn onboarding_shows_on_a_genuinely_fresh_install() {
        // No settings.json on disk at all → AppSettings::default(), and no
        // chat history — the only combination that should trigger it.
        assert!(should_show_onboarding(&engine::AppSettings::default(), &[]));
    }

    #[test]
    fn onboarding_stays_hidden_for_settings_json_that_predates_the_flag() {
        // Deserializing a settings.json written before `onboarding_completed`
        // existed defaults the field to `true` (see the engine::tests
        // deserialization test) — an existing install must never see the
        // assistant just because it upgraded.
        let legacy: engine::AppSettings = serde_json::from_str(r#"{"backend":"claude"}"#).unwrap();
        assert!(!should_show_onboarding(&legacy, &[]));
    }

    #[test]
    fn onboarding_stays_hidden_when_chat_history_exists_even_without_a_settings_file() {
        // Covers an existing install that never happened to write
        // settings.json (no theme change, no schedule, no manual Settings
        // save): AppSettings::default() alone would look identical to a
        // fresh install, but its chat history gives it away.
        let fresh_default = engine::AppSettings::default();
        assert!(!should_show_onboarding(&fresh_default, &[session("a")]));
    }

    #[test]
    fn agent_mode_exposes_only_plans_runs_mcp_and_settings() {
        for view in [View::Plans, View::Runs, View::Mcp, View::Settings] {
            assert!(view_available(view, true), "view {view:?}");
        }
        for view in [View::Chat, View::Schedules] {
            assert!(!view_available(view, true), "view {view:?}");
        }
    }

    #[test]
    fn normal_mode_exposes_every_view() {
        for view in [
            View::Chat,
            View::Plans,
            View::Runs,
            View::Schedules,
            View::Mcp,
            View::Settings,
        ] {
            assert!(view_available(view, false), "view {view:?}");
        }
    }

    #[test]
    fn onboarding_stays_hidden_once_already_completed() {
        let settings = engine::AppSettings {
            onboarding_completed: true,
            ..engine::AppSettings::default()
        };
        assert!(!should_show_onboarding(&settings, &[]));
    }
}
