//! Shared, in-process activity ledger for compiler-backed work.
//!
//! Chat commands execute through the desktop engine while MCP requests execute
//! on the local HTTP runtime.  Both surfaces write to this registry so the UI
//! can honestly report work that was started somewhere other than the open
//! chat.  Entries are deliberately process-local: plans and runs remain the
//! durable source of truth; this is only the live operator-facing window.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use egui::{Align, Layout, RichText, Ui};

use super::console::{CompileConsole, ConsoleLine};
use super::{theme, widgets};

/// The number of terminal console lines retained after an activity completes.
/// Full compiler logs remain on disk; this is the bounded diagnostic tail the
/// desktop can show after the original live buffer has moved on.
const TERMINAL_TAIL_LINES: usize = 80;
/// Keep a compact recent history rather than growing the UI state forever.
const TERMINAL_ACTIVITY_LIMIT: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityOrigin {
    Chat,
    Mcp,
}

impl ActivityOrigin {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Mcp => "MCP",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    Compile,
    Edit,
    Repair,
}

impl ActivityKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Compile => "Compile",
            Self::Edit => "Edit",
            Self::Repair => "Repair",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityState {
    Running,
    Succeeded,
    Failed(String),
    /// The task was dropped before it reported an outcome. This includes a
    /// cancellation or panic, neither of which can truthfully be inferred by
    /// a synchronous `Drop` implementation.
    Interrupted,
}

impl ActivityState {
    fn label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "completed",
            Self::Failed(_) => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActivitySnapshot {
    pub id: u64,
    pub origin: ActivityOrigin,
    pub kind: ActivityKind,
    pub state: ActivityState,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub console: Option<CompileConsole>,
    pub terminal_tail: Vec<ConsoleLine>,
}

#[derive(Default)]
struct RegistryState {
    next_id: u64,
    entries: VecDeque<ActivitySnapshot>,
}

/// Shared activity store used by both the desktop engine and the MCP server.
#[derive(Clone, Default)]
pub struct ActivityRegistry {
    state: Arc<Mutex<RegistryState>>,
    repaint: Option<super::engine::RepaintHook>,
    mcp_completions: Arc<std::sync::atomic::AtomicUsize>,
}

impl ActivityRegistry {
    pub fn new(repaint: Option<super::engine::RepaintHook>) -> Self {
        Self {
            state: Arc::default(),
            repaint,
            mcp_completions: Arc::default(),
        }
    }

    /// Start an activity. Its guard makes terminal cleanup structural: an
    /// error, cancellation, or panic cannot leave a phantom running row.
    pub fn start(&self, origin: ActivityOrigin, kind: ActivityKind) -> ActivityGuard {
        let id = self.with_state(|state| {
            state.next_id += 1;
            let id = state.next_id;
            state.entries.push_front(ActivitySnapshot {
                id,
                origin,
                kind,
                state: ActivityState::Running,
                started_at: chrono::Utc::now(),
                console: None,
                terminal_tail: Vec::new(),
            });
            id
        });
        self.repaint();
        ActivityGuard {
            registry: self.clone(),
            id,
            origin,
            completed: false,
        }
    }

    pub fn attach_console(&self, id: u64, console: CompileConsole) {
        self.with_state(|state| {
            if let Some(entry) = state.entries.iter_mut().find(|entry| entry.id == id) {
                entry.console = Some(console);
            }
        });
        self.repaint();
    }

    pub fn snapshot(&self) -> Vec<ActivitySnapshot> {
        self.with_state(|state| state.entries.iter().cloned().collect())
    }

    pub fn running_count(&self) -> usize {
        self.with_state(|state| {
            state
                .entries
                .iter()
                .filter(|entry| entry.state == ActivityState::Running)
                .count()
        })
    }

    /// Coalescing signal consumed by the UI once per frame. Several MCP tasks
    /// may finish between frames; their plan-list refresh is then one batch.
    pub fn take_mcp_completions(&self) -> usize {
        self.mcp_completions
            .swap(0, std::sync::atomic::Ordering::AcqRel)
    }

    /// Repaint hook for a live console owned by this registry. MCP work has no
    /// engine event stream, so its console must wake the desktop directly.
    pub fn console_notify(&self) -> Option<super::console::ConsoleNotify> {
        self.repaint.clone()
    }

    fn finish(&self, id: u64, origin: ActivityOrigin, state: ActivityState) {
        self.with_state(|registry| {
            if let Some(entry) = registry.entries.iter_mut().find(|entry| entry.id == id) {
                if entry.state != ActivityState::Running {
                    return;
                }
                if let Some(console) = &entry.console {
                    let note = match &state {
                        ActivityState::Succeeded => "✓ activity completed".to_owned(),
                        ActivityState::Failed(error) => format!("✗ activity failed: {error}"),
                        ActivityState::Interrupted => {
                            "! activity interrupted before an outcome was reported".to_owned()
                        }
                        ActivityState::Running => String::new(),
                    };
                    console.close(note);
                }
                entry.terminal_tail = entry
                    .console
                    .as_ref()
                    .map(|console| {
                        let snapshot = console.snapshot();
                        let first = snapshot.lines.len().saturating_sub(TERMINAL_TAIL_LINES);
                        snapshot.lines.into_iter().skip(first).collect()
                    })
                    .unwrap_or_default();
                entry.state = state;
            }
            while registry.entries.len() > TERMINAL_ACTIVITY_LIMIT {
                let Some(last) = registry.entries.back() else {
                    break;
                };
                if last.state == ActivityState::Running {
                    break;
                }
                registry.entries.pop_back();
            }
        });
        if origin == ActivityOrigin::Mcp {
            self.mcp_completions
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }
        self.repaint();
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut RegistryState) -> T) -> T {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut state)
    }

    fn repaint(&self) {
        if let Some(repaint) = &self.repaint {
            repaint();
        }
    }
}

/// RAII lifecycle token for one activity.
pub struct ActivityGuard {
    registry: ActivityRegistry,
    id: u64,
    origin: ActivityOrigin,
    completed: bool,
}

impl ActivityGuard {
    pub const fn id(&self) -> u64 {
        self.id
    }

    pub fn succeeded(mut self) {
        self.registry
            .finish(self.id, self.origin, ActivityState::Succeeded);
        self.completed = true;
    }

    pub fn failed(mut self, error: impl Into<String>) {
        self.registry
            .finish(self.id, self.origin, ActivityState::Failed(error.into()));
        self.completed = true;
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.registry
                .finish(self.id, self.origin, ActivityState::Interrupted);
        }
    }
}

/// Render a compact live activity inspector. It is intentionally usable from
/// every view, including agent mode, rather than being hidden inside Chat.
pub fn show(ui: &mut Ui, registry: &ActivityRegistry) {
    let entries = registry.snapshot();
    if entries.is_empty() {
        return;
    }
    let running = registry.running_count();
    ui.collapsing(
        RichText::new(format!("Agent activity · {running} running"))
            .size(theme::FONT_SMALL)
            .color(theme::text_muted()),
        |ui| {
            for entry in entries {
                theme::card_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        widgets::badge(ui, entry.origin.label(), theme::source_mcp());
                        widgets::badge(ui, entry.kind.label(), theme::accent());
                        let state_color = match &entry.state {
                            ActivityState::Running => theme::warn(),
                            ActivityState::Succeeded => theme::ok(),
                            ActivityState::Failed(_) => theme::err(),
                            ActivityState::Interrupted => theme::text_muted(),
                        };
                        widgets::badge(ui, entry.state.label(), state_color);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(
                                RichText::new(activity_elapsed(entry.started_at))
                                    .monospace()
                                    .size(theme::FONT_SMALL)
                                    .color(theme::text_faint()),
                            );
                        });
                    });
                    if let ActivityState::Failed(error) = &entry.state {
                        ui.label(
                            RichText::new(error)
                                .size(theme::FONT_SMALL)
                                .color(theme::err()),
                        );
                    }
                    let lines = if entry.state == ActivityState::Running {
                        entry
                            .console
                            .as_ref()
                            .map(|console| console.snapshot().lines)
                    } else {
                        Some(entry.terminal_tail)
                    };
                    if let Some(lines) = lines.filter(|lines| !lines.is_empty()) {
                        egui::ScrollArea::vertical()
                            .id_salt(("activity-console", entry.id))
                            .max_height(120.0)
                            .show(ui, |ui| {
                                for line in lines {
                                    ui.label(
                                        RichText::new(line.text)
                                            .monospace()
                                            .size(theme::FONT_SMALL)
                                            .color(theme::text_faint()),
                                    );
                                }
                            });
                    }
                });
                ui.add_space(4.0);
            }
            if running > 0 {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_secs(1));
            }
        },
    );
}

fn activity_elapsed(started_at: chrono::DateTime<chrono::Utc>) -> String {
    let seconds = (chrono::Utc::now() - started_at).num_seconds().max(0);
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3_599 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3_600, (seconds / 60) % 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_guard_becomes_terminal_instead_of_staying_running() {
        let registry = ActivityRegistry::default();
        let guard = registry.start(ActivityOrigin::Chat, ActivityKind::Compile);
        drop(guard);
        let entries = registry.snapshot();
        assert_eq!(entries[0].state, ActivityState::Interrupted);
        assert_eq!(registry.running_count(), 0);
    }

    #[test]
    fn mcp_completions_are_coalesced_for_one_ui_refresh_batch() {
        let registry = ActivityRegistry::default();
        registry
            .start(ActivityOrigin::Mcp, ActivityKind::Compile)
            .succeeded();
        registry
            .start(ActivityOrigin::Mcp, ActivityKind::Repair)
            .succeeded();
        assert_eq!(registry.take_mcp_completions(), 2);
        assert_eq!(registry.take_mcp_completions(), 0);
    }

    #[test]
    fn terminal_tail_is_bounded_and_keeps_the_latest_diagnostic() {
        let registry = ActivityRegistry::default();
        let guard = registry.start(ActivityOrigin::Mcp, ActivityKind::Compile);
        let console = CompileConsole::new("test", None, None);
        for number in 0..(TERMINAL_TAIL_LINES + 10) {
            console.info(format!("line {number}"));
        }
        registry.attach_console(guard.id(), console);
        guard.failed("backend unavailable");
        let entry = registry.snapshot().remove(0);
        assert_eq!(entry.terminal_tail.len(), TERMINAL_TAIL_LINES);
        assert_eq!(
            entry.terminal_tail.last().map(|line| line.text.as_str()),
            Some("✗ activity failed: backend unavailable")
        );
    }
}
