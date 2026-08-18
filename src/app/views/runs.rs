//! Runs view — one flat, filterable list of every execution across all
//! plans. Answers "what ran, what is running right now?" without
//! scrolling through the Plans overview: running rows sort first and get a
//! highlighted row plus a live elapsed counter, and every run carries its
//! origin (Chat / MCP / Schedule) as a badge so agent-triggered work — the
//! runs the user did *not* watch happen — is recognizable at a glance.
//!
//! Inspect keeps today's semantics (open the run inside the plan's chat), so
//! it is hidden in agent mode just like the Plans view's Inspect.

use egui::{Align, Layout, RichText, Sense, Ui};

use crate::executor::RunStatus;

use crate::app::engine::{EngineCommand, EngineHandle, PlanListItem, RunListItem, RunSource};
use crate::app::views::{plan_card, plans::run_status_color};
use crate::app::{theme, time, widgets};

// Fixed column widths (px). PLAN takes the remaining flexible space.
const COL_RUN_ID: f32 = 78.0;
const COL_SOURCE: f32 = 82.0;
const COL_STARTED: f32 = 104.0;
const COL_DURATION: f32 = 170.0;
/// Space the flexible PLAN cell must leave for everything to its right
/// (source, started, duration, Inspect button, spacings).
const PLAN_RESERVED: f32 = COL_SOURCE + COL_STARTED + COL_DURATION + 130.0;
/// Failed rows show a short error summary in the duration column; the full
/// message stays reachable on hover.
const ERROR_SUMMARY_MAX_CHARS: usize = 24;

/// Session-only filter state — deliberately not persisted (issue spec:
/// "filters persist for the session").
#[derive(Default)]
pub struct RunsState {
    pub status_filter: StatusFilter,
    /// Restrict to a single plan (by id).
    pub plan_filter: Option<String>,
    /// Restrict to a single origin. Runs without a recorded source only
    /// match "all".
    pub source_filter: Option<RunSource>,
    /// Agent mode's read-only inspector, intentionally separate from chats.
    pub inspected: Option<Box<crate::executor::Run>>,
}

/// Actions the shell must react to (navigation / engine commands).
#[derive(Debug, Clone, PartialEq)]
pub enum RunsAction {
    /// Open the run inside its plan's chat (same semantics as the Plans
    /// view's Inspect / `PlansAction::OpenRun`).
    Inspect { plan_id: String, run_id: String },
    /// Navigate to the plan's conversation (click on the plan name).
    OpenPlan(String),
    /// Inspect a stored run in place when Chat is unavailable.
    InspectReadOnly { run_id: String },
    /// Abort an in-progress run.
    Abort { run_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusFilter {
    #[default]
    All,
    Running,
    Succeeded,
    Failed,
}

impl StatusFilter {
    fn matches(self, status: &RunStatus) -> bool {
        match self {
            StatusFilter::All => true,
            StatusFilter::Running => matches!(status, RunStatus::Running),
            StatusFilter::Succeeded => matches!(status, RunStatus::Succeeded),
            StatusFilter::Failed => status.is_failed(),
        }
    }
}

pub fn show(
    ui: &mut Ui,
    state: &mut RunsState,
    runs: &[RunListItem],
    plans: &[PlanListItem],
    engine: &EngineHandle,
    agent_mode: bool,
) -> Option<RunsAction> {
    let mut action = None;
    let running_count = count_running(runs);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(theme::title("Runs", theme::FONT_HEADING));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::ghost_button(ui, "⟳ Refresh").clicked() {
                        engine.send(EngineCommand::ListRuns);
                        engine.send(EngineCommand::ListPlans);
                    }
                });
            });
            ui.label(
                RichText::new(
                    "Everything that ran or is running — from Chat, Claude Code (MCP) \
                     and Schedules.",
                )
                .size(theme::FONT_SMALL)
                .color(theme::text_muted()),
            );
            ui.add_space(10.0);

            if let Some(run) = &state.inspected {
                read_only_inspector(ui, run);
                ui.add_space(10.0);
            }

            filter_row(ui, state, plans, running_count);
            ui.add_space(10.0);

            if runs.is_empty() {
                ui.label(
                    RichText::new(
                        "No runs yet — run a plan from Chat, a schedule, or your agent \
                         via MCP.",
                    )
                    .color(theme::text_muted()),
                );
                return;
            }

            let visible = visible_runs(runs, state);
            if visible.is_empty() {
                ui.label(
                    RichText::new("No runs match the current filters.").color(theme::text_muted()),
                );
            } else {
                header_row(ui);
                let now = chrono::Utc::now();
                for run in &visible {
                    if let Some(row_action) = run_row(ui, run, now, agent_mode) {
                        action = Some(row_action);
                    }
                }
            }

            // Live elapsed counters tick once a second; without this an
            // untouched window would only advance on the 2 s data poll.
            if running_count > 0 {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_secs(1));
            }

            ui.add_space(10.0);
            ui.label(
                RichText::new(footer_text(visible.len(), runs.len()))
                    .size(theme::FONT_SMALL)
                    .color(theme::text_faint()),
            );
            ui.add_space(12.0);
        });

    action
}

// ─── Filters ────────────────────────────────────────────────────────────────

fn filter_row(ui: &mut Ui, state: &mut RunsState, plans: &[PlanListItem], running_count: usize) {
    ui.horizontal_wrapped(|ui| {
        let chips = [
            (StatusFilter::All, "All".to_owned()),
            (StatusFilter::Running, format!("Running ({running_count})")),
            (StatusFilter::Succeeded, "Succeeded".to_owned()),
            (StatusFilter::Failed, "Failed".to_owned()),
        ];
        for (filter, label) in chips {
            if widgets::filter_chip(ui, &label, state.status_filter == filter).clicked() {
                state.status_filter = filter;
            }
        }

        ui.add_space(theme::GAP);
        ui.label(
            RichText::new("Plan:")
                .size(theme::FONT_SMALL)
                .color(theme::text_faint()),
        );
        let plan_label = state
            .plan_filter
            .as_ref()
            .and_then(|id| plans.iter().find(|plan| &plan.id == id))
            .map_or_else(|| "all".to_owned(), |plan| plan.name.clone());
        egui::ComboBox::from_id_salt("runs_plan_filter")
            .selected_text(plan_label)
            .width(160.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut state.plan_filter, None, "all");
                for plan in plans {
                    ui.selectable_value(&mut state.plan_filter, Some(plan.id.clone()), &plan.name);
                }
            });

        ui.add_space(theme::GAP);
        ui.label(
            RichText::new("Source:")
                .size(theme::FONT_SMALL)
                .color(theme::text_faint()),
        );
        egui::ComboBox::from_id_salt("runs_source_filter")
            .selected_text(
                state
                    .source_filter
                    .map_or("all", |source| source.label())
                    .to_owned(),
            )
            .width(110.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut state.source_filter, None, "all");
                for source in [RunSource::Chat, RunSource::Mcp, RunSource::Schedule] {
                    ui.selectable_value(&mut state.source_filter, Some(source), source.label());
                }
            });
    });
}

// ─── Rows ───────────────────────────────────────────────────────────────────

/// Muted uppercase column captions, aligned with the fixed cell widths of
/// [`run_row`] (LTR: status/run/plan — RTL: action/duration/started/source).
fn header_row(ui: &mut Ui) {
    let caption = |ui: &mut Ui, text: &str| {
        ui.label(
            RichText::new(text)
                .size(theme::FONT_SMALL)
                .color(theme::text_faint()),
        );
    };
    ui.horizontal(|ui| {
        // Row frame margin + status-dot width + item spacing, so the RUN
        // caption starts where the ids below do.
        ui.add_space(28.0);
        ui.scope(|ui| {
            ui.set_width(COL_RUN_ID);
            caption(ui, "RUN");
        });
        caption(ui, "PLAN");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // Blank space over the Inspect column keeps DURATION centered
            // over its cells.
            ui.add_space(76.0);
            ui.scope(|ui| {
                ui.set_width(COL_DURATION);
                caption(ui, "DURATION");
            });
            ui.scope(|ui| {
                ui.set_width(COL_STARTED);
                caption(ui, "STARTED");
            });
            ui.scope(|ui| {
                ui.set_width(COL_SOURCE);
                caption(ui, "SOURCE");
            });
        });
    });
    ui.separator();
}

fn run_row(
    ui: &mut Ui,
    run: &RunListItem,
    now: chrono::DateTime<chrono::Utc>,
    agent_mode: bool,
) -> Option<RunsAction> {
    let mut action = None;
    let running = matches!(run.status, RunStatus::Running);
    // Running rows get a tinted highlight so in-flight work stands out even
    // while scrolled deep into history.
    let frame = if running {
        egui::Frame::new()
            .fill(theme::with_alpha(theme::warn(), 0.07))
            .stroke(egui::Stroke::new(
                1.0_f32,
                theme::with_alpha(theme::warn(), 0.35),
            ))
            .corner_radius(egui::CornerRadius::same(theme::RADIUS_WIDGET))
            .inner_margin(egui::Margin::symmetric(4, 2))
    } else {
        egui::Frame::new().inner_margin(egui::Margin::symmetric(4, 2))
    };
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let (color, pulsing) = run_status_color(&run.status);
            widgets::status_dot(ui, color, pulsing);
            ui.scope(|ui| {
                ui.set_width(COL_RUN_ID);
                ui.label(
                    RichText::new(plan_card::short_id(&run.id))
                        .monospace()
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                );
            });
            // Plan name navigates to the plan's conversation — the same
            // destination as "Open in chat" on the Plans view, so it is
            // withheld in agent mode where that chat does not exist.
            let plan_width = (ui.available_width() - PLAN_RESERVED).max(theme::GAP * 6.0);
            let plan_label = ui
                .scope(|ui| {
                    ui.set_max_width(plan_width);
                    ui.add(
                        egui::Label::new(RichText::new(&run.plan_name))
                            .truncate()
                            .sense(if agent_mode {
                                Sense::hover()
                            } else {
                                Sense::click()
                            }),
                    )
                })
                .inner;
            if !agent_mode {
                if plan_label.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if plan_label.clicked() {
                    action = Some(RunsAction::OpenPlan(run.plan_id.clone()));
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::ghost_button(ui, "Inspect").clicked() {
                    action = Some(match agent_mode {
                        true => RunsAction::InspectReadOnly {
                            run_id: run.id.clone(),
                        },
                        false => RunsAction::Inspect {
                            plan_id: run.plan_id.clone(),
                            run_id: run.id.clone(),
                        },
                    });
                }
                if running && widgets::danger_button(ui, "Abort").clicked() {
                    action = Some(RunsAction::Abort {
                        run_id: run.id.clone(),
                    });
                }
                ui.scope(|ui| {
                    ui.set_width(COL_DURATION);
                    duration_label(ui, run, now);
                });
                ui.scope(|ui| {
                    ui.set_width(COL_STARTED);
                    ui.label(
                        RichText::new(time::format_local(&run.started_at, "%b %d, %H:%M"))
                            .size(theme::FONT_SMALL)
                            .color(theme::text_faint()),
                    );
                });
                ui.scope(|ui| {
                    ui.set_width(COL_SOURCE);
                    match run.source {
                        Some(source) => {
                            widgets::badge(ui, source.label(), source_color(source));
                        }
                        // Runs recorded before the source field existed.
                        None => {
                            ui.label(
                                RichText::new("—")
                                    .size(theme::FONT_SMALL)
                                    .color(theme::text_faint()),
                            );
                        }
                    }
                });
            });
        });
    });
    action
}

fn source_color(source: RunSource) -> egui::Color32 {
    match source {
        RunSource::Chat => theme::source_chat(),
        RunSource::Mcp => theme::source_mcp(),
        RunSource::Schedule => theme::source_schedule(),
    }
}

fn read_only_inspector(ui: &mut Ui, run: &crate::executor::Run) {
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            widgets::section_label(ui, "Run inspection");
            ui.label(
                RichText::new(plan_card::short_id(&run.id))
                    .monospace()
                    .size(theme::FONT_SMALL)
                    .color(theme::text_faint()),
            );
        });
        ui.label(
            RichText::new(format!("Status: {}", run.status))
                .size(theme::FONT_SMALL)
                .color(theme::text_muted()),
        );
        for step in run.step_runs.values() {
            ui.label(
                RichText::new(format!("{} · {}", step.step_id, step.status))
                    .monospace()
                    .size(theme::FONT_SMALL)
                    .color(theme::text_muted()),
            );
            if let Some(error) = &step.error {
                ui.label(
                    RichText::new(error)
                        .size(theme::FONT_SMALL)
                        .color(theme::err()),
                );
            }
        }
    });
}

/// The DURATION cell — live elapsed for running work, a short error for
/// failures (full message on hover), a status word for paused/cancelled
/// runs. Succeeded runs render "—" until the run record carries a
/// `finished_at` in its list summary; wall time then lands
/// here without touching the layout.
fn duration_label(ui: &mut Ui, run: &RunListItem, now: chrono::DateTime<chrono::Utc>) {
    match &run.status {
        RunStatus::Running => {
            ui.label(
                RichText::new(format!("{}…", format_elapsed(&run.started_at, now)))
                    .monospace()
                    .size(theme::FONT_SMALL)
                    .color(theme::text()),
            );
        }
        RunStatus::Failed { message, .. } => {
            ui.label(
                RichText::new(error_summary(message))
                    .size(theme::FONT_SMALL)
                    .color(theme::err()),
            )
            .on_hover_text(message);
        }
        RunStatus::WaitingForHuman { .. } => {
            ui.label(
                RichText::new("waiting for input")
                    .size(theme::FONT_SMALL)
                    .color(theme::warn()),
            );
        }
        RunStatus::Cancelled => {
            ui.label(
                RichText::new("cancelled")
                    .size(theme::FONT_SMALL)
                    .color(theme::text_muted()),
            );
        }
        RunStatus::Succeeded => {
            // Wall time once the record carries a finish timestamp; "—" for
            // legacy records that never persisted one.
            let text = match run.finished_at {
                Some(finished) => format_wall_time(&run.started_at, &finished),
                None => "—".to_owned(),
            };
            ui.label(
                RichText::new(text)
                    .monospace()
                    .size(theme::FONT_SMALL)
                    .color(theme::text_faint()),
            );
        }
    }
}

/// Wall-clock duration between start and finish, rendered compactly. Sub-minute
/// runs (the common case for compiled plans) show fractional seconds.
fn format_wall_time(
    started_at: &chrono::DateTime<chrono::Utc>,
    finished_at: &chrono::DateTime<chrono::Utc>,
) -> String {
    let ms = (*finished_at - *started_at).num_milliseconds().max(0);
    match ms {
        0..=59_999 => format!("{:.2}s", ms as f64 / 1000.0),
        60_000..=3_599_999 => format!("{}m {:02}s", ms / 60_000, (ms / 1000) % 60),
        _ => format!("{}h {:02}m", ms / 3_600_000, (ms / 60_000) % 60),
    }
}

// ─── Pure helpers (unit-tested below) ───────────────────────────────────────

/// How many runs are executing right now — drives the "Running (N)" chip
/// here and the pulsing nav badge in the shell. A run paused on a human
/// answer deliberately does not count: it is waiting, not running.
pub(crate) fn count_running(runs: &[RunListItem]) -> usize {
    runs.iter()
        .filter(|run| matches!(run.status, RunStatus::Running))
        .count()
}

/// Apply every active filter (they combine as AND), then order: running
/// first, then by `started_at` descending.
fn visible_runs<'a>(runs: &'a [RunListItem], state: &RunsState) -> Vec<&'a RunListItem> {
    let mut visible: Vec<_> = runs
        .iter()
        .filter(|run| state.status_filter.matches(&run.status))
        .filter(|run| {
            state
                .plan_filter
                .as_ref()
                .is_none_or(|plan_id| &run.plan_id == plan_id)
        })
        .filter(|run| {
            state
                .source_filter
                .is_none_or(|source| run.source == Some(source))
        })
        .collect();
    visible.sort_by_key(|run| {
        (
            !matches!(run.status, RunStatus::Running),
            std::cmp::Reverse(run.started_at),
        )
    });
    visible
}

/// Compact live-elapsed readout: `12s`, `4m 09s`, `1h 02m`. Clock skew
/// (a run "started" in the future) clamps to zero instead of underflowing.
fn format_elapsed(
    started_at: &chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let secs = (now - *started_at).num_seconds().max(0);
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// First line of a failure message, hard-capped so the column never grows —
/// the full text stays reachable via hover.
fn error_summary(message: &str) -> String {
    let first_line = message.lines().next().unwrap_or("failed").trim();
    match first_line.chars().count() <= ERROR_SUMMARY_MAX_CHARS {
        true => first_line.to_owned(),
        false => format!(
            "{}…",
            first_line
                .chars()
                .take(ERROR_SUMMARY_MAX_CHARS)
                .collect::<String>()
        ),
    }
}

fn footer_text(visible: usize, total: usize) -> String {
    match visible == total {
        true => format!("{total} runs total · auto-refresh on"),
        false => format!("{visible} of {total} runs shown · auto-refresh on"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        plan_id: &str,
        status: RunStatus,
        started_at: chrono::DateTime<chrono::Utc>,
        source: Option<RunSource>,
    ) -> RunListItem {
        RunListItem {
            id: uuid::Uuid::new_v4().to_string(),
            plan_id: plan_id.to_owned(),
            plan_name: format!("Plan {plan_id}"),
            status,
            started_at,
            finished_at: None,
            source,
        }
    }

    fn failed(message: &str) -> RunStatus {
        RunStatus::Failed {
            failed_step_id: "step".to_owned(),
            message: message.to_owned(),
        }
    }

    #[test]
    fn default_filters_show_everything_running_first_then_newest() {
        let now = chrono::Utc::now();
        let old_running = run(
            "a",
            RunStatus::Running,
            now - chrono::Duration::hours(3),
            None,
        );
        let newest_finished = run("a", RunStatus::Succeeded, now, None);
        let older_finished = run("b", failed("boom"), now - chrono::Duration::hours(1), None);
        let runs = vec![
            newest_finished.clone(),
            old_running.clone(),
            older_finished.clone(),
        ];

        let visible = visible_runs(&runs, &RunsState::default());
        assert_eq!(
            visible
                .iter()
                .map(|run| run.id.as_str())
                .collect::<Vec<_>>(),
            // The hours-old running run still sorts above every finished one.
            vec![
                old_running.id.as_str(),
                newest_finished.id.as_str(),
                older_finished.id.as_str()
            ]
        );
    }

    #[test]
    fn status_plan_and_source_filters_combine_as_and() {
        let now = chrono::Utc::now();
        let matching = run("a", RunStatus::Succeeded, now, Some(RunSource::Mcp));
        let wrong_status = run("a", failed("x"), now, Some(RunSource::Mcp));
        let wrong_plan = run("b", RunStatus::Succeeded, now, Some(RunSource::Mcp));
        let wrong_source = run("a", RunStatus::Succeeded, now, Some(RunSource::Chat));
        let unknown_source = run("a", RunStatus::Succeeded, now, None);
        let runs = vec![
            matching.clone(),
            wrong_status,
            wrong_plan,
            wrong_source,
            unknown_source,
        ];

        let state = RunsState {
            status_filter: StatusFilter::Succeeded,
            plan_filter: Some("a".to_owned()),
            source_filter: Some(RunSource::Mcp),
            inspected: None,
        };
        let visible = visible_runs(&runs, &state);
        assert_eq!(
            visible
                .iter()
                .map(|run| run.id.as_str())
                .collect::<Vec<_>>(),
            vec![matching.id.as_str()]
        );
    }

    #[test]
    fn running_filter_excludes_waiting_for_human() {
        // The "Running (N)" chip and nav badge answer "is work executing
        // right now" — a run paused on a human answer is not executing.
        let now = chrono::Utc::now();
        let runs = vec![
            run("a", RunStatus::Running, now, None),
            run(
                "a",
                RunStatus::WaitingForHuman {
                    step_id: "s".to_owned(),
                },
                now,
                None,
            ),
        ];
        assert_eq!(count_running(&runs), 1);
        let state = RunsState {
            status_filter: StatusFilter::Running,
            ..Default::default()
        };
        assert_eq!(visible_runs(&runs, &state).len(), 1);
    }

    #[test]
    fn failed_filter_matches_only_failures() {
        let now = chrono::Utc::now();
        let runs = vec![
            run("a", failed("boom"), now, None),
            run("a", RunStatus::Succeeded, now, None),
            run("a", RunStatus::Cancelled, now, None),
        ];
        let state = RunsState {
            status_filter: StatusFilter::Failed,
            ..Default::default()
        };
        assert_eq!(visible_runs(&runs, &state).len(), 1);
    }

    #[test]
    fn elapsed_formats_seconds_minutes_and_hours() {
        let now = chrono::Utc::now();
        let at = |secs: i64| now - chrono::Duration::seconds(secs);
        assert_eq!(format_elapsed(&at(12), now), "12s");
        assert_eq!(format_elapsed(&at(249), now), "4m 09s");
        assert_eq!(format_elapsed(&at(3_720), now), "1h 02m");
        // Clock skew must not underflow.
        assert_eq!(format_elapsed(&at(-30), now), "0s");
    }

    #[test]
    fn error_summary_keeps_the_first_line_and_caps_length() {
        assert_eq!(error_summary("HTTP 404"), "HTTP 404");
        assert_eq!(error_summary("HTTP 404\nlong trace below"), "HTTP 404");
        let long = "x".repeat(ERROR_SUMMARY_MAX_CHARS + 10);
        assert_eq!(
            error_summary(&long).chars().count(),
            ERROR_SUMMARY_MAX_CHARS + 1 // + ellipsis
        );
    }

    #[test]
    fn footer_reports_filtering() {
        assert_eq!(footer_text(3, 3), "3 runs total · auto-refresh on");
        assert_eq!(footer_text(1, 3), "1 of 3 runs shown · auto-refresh on");
    }
}
