//! Plans view — a browsable index of compiled plans and recent runs.
//! Opening a plan or run drops it into the chat as a card.
//!
//! Once at least one plan exists, this is also the app's landing page (see
//! `has_compiled_plans` in `mod.rs`), so it leads with an activity summary
//! (stat tiles, a runs-over-time chart, a schedules digest) above the
//! existing plan-card and recent-runs lists.

use egui::{
    Align, Color32, CornerRadius, Id, Layout, Pos2, Rect, RichText, Sense, Stroke, Ui, vec2,
};

use crate::executor::RunStatus;

use crate::app::engine::{EngineCommand, EngineHandle, PlanListItem, RunListItem, ScheduleItem};
use crate::app::views::plan_card;
use crate::app::{anim, theme, time, widgets};

/// How many trailing days the "runs over time" chart covers. Fixed rather
/// than data-driven so the chart always shows a stable window, including
/// days with zero runs.
const CHART_WINDOW_DAYS: i64 = 14;

#[derive(Default)]
pub struct PlansState {
    pub plans: Vec<PlanListItem>,
    pub runs: Vec<RunListItem>,
    /// Plan id awaiting delete confirmation (two-click delete).
    pub confirm_delete: Option<String>,
    /// Prompt-only preflight; the engine performs the definitive collision
    /// lookup and save under its shared mutation boundary.
    pub import_collision: Option<ImportCollision>,
}

#[derive(Debug, Clone)]
pub struct ImportCollision {
    path: std::path::PathBuf,
    name: String,
}

/// Actions the shell must react to (view switches).
#[derive(Debug, Clone, PartialEq)]
pub enum PlansAction {
    /// Navigate to the conversation owned by this plan.
    OpenPlan(String),
    /// Navigate to the plan conversation and inspect this execution.
    OpenRun { plan_id: String, run_id: String },
    /// Navigate to the plan conversation and start an execution there.
    RunPlan { plan_id: String },
    /// Open the Schedules view with this plan preselected.
    SchedulePlan(String),
    /// Open the cross-plan Runs view ("View all →" under Recent runs).
    ViewAllRuns,
}

/// How many runs the "Recent runs" teaser at the bottom of this page shows —
/// the full, filterable history lives in the Runs view.
const RECENT_RUNS_TEASER: usize = 5;

pub fn show(
    ui: &mut Ui,
    state: &mut PlansState,
    schedules: &[ScheduleItem],
    engine: &EngineHandle,
    agent_mode: bool,
) -> Option<PlansAction> {
    let mut action = None;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(theme::title("Plans", theme::FONT_HEADING));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::ghost_button(ui, "⟳ Refresh").clicked() {
                        engine.send(EngineCommand::ListPlans);
                        engine.send(EngineCommand::ListRuns);
                        engine.send(EngineCommand::ListSchedules);
                    }
                    if widgets::ghost_button(ui, "⇧ Import plan").clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .add_filter("INXM plan bundle", &["json"])
                            .set_title("Import a plan")
                            .pick_file()
                    {
                        let collision_name = crate::plan::bundle::PlanBundle::load_from_file(&path)
                            .ok()
                            .map(|bundle| bundle.plan.name)
                            .filter(|name| {
                                state
                                    .plans
                                    .iter()
                                    .any(|plan| plan.name.eq_ignore_ascii_case(name))
                            });
                        if let Some(name) = collision_name {
                            state.import_collision = Some(ImportCollision { path, name });
                        } else {
                            engine.send(EngineCommand::ImportPlan { path });
                        }
                    }
                });
            });
            ui.add_space(8.0);

            if state.plans.is_empty() {
                ui.label(
                    RichText::new(match agent_mode {
                        true => {
                            "Nothing compiled yet — register a plan from your agent \
                             through the local MCP server."
                        }
                        false => "Nothing compiled yet — go to Chat and describe the work.",
                    })
                    .color(theme::text_muted()),
                );
            } else {
                stats_summary(ui, &state.plans, &state.runs, schedules);
                ui.add_space(12.0);
                runs_chart_section(ui, &state.runs);
                ui.add_space(12.0);
            }

            let confirm_delete = state.confirm_delete.clone();
            let mut next_confirm = confirm_delete.clone();
            for (index, plan) in plans_by_latest_activity(&state.plans, &state.runs)
                .into_iter()
                .enumerate()
            {
                let card_id = Id::new("plans_view").with(&plan.id).with(plan.version);
                anim::entrance(ui, card_id, index as f32 * anim::STAGGER_SECS, |ui| {
                    theme::card_frame().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            widgets::truncated_label(
                                ui,
                                theme::title(&plan.name, theme::FONT_BODY),
                                420.0,
                            );
                            widgets::badge(ui, &format!("v{}", plan.version), theme::text_muted());
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let run_label = if plan.inputs.is_empty() {
                                    "▶ Run"
                                } else {
                                    "▶ Set inputs"
                                };
                                // Collecting inputs happens in the plan's
                                // chat, so plans with inputs get no run
                                // affordance in agent mode.
                                if (plan.inputs.is_empty() || !agent_mode)
                                    && widgets::primary_button(ui, run_label).clicked()
                                {
                                    if plan.inputs.is_empty() {
                                        action = Some(PlansAction::RunPlan {
                                            plan_id: plan.id.clone(),
                                        });
                                    } else {
                                        action = Some(PlansAction::OpenPlan(plan.id.clone()));
                                    }
                                }
                                if !agent_mode && widgets::ghost_button(ui, "⏱ Schedule").clicked()
                                {
                                    action = Some(PlansAction::SchedulePlan(plan.id.clone()));
                                }
                                if !agent_mode
                                    && widgets::ghost_button(ui, "Open in chat").clicked()
                                {
                                    action = Some(PlansAction::OpenPlan(plan.id.clone()));
                                }
                                if widgets::ghost_button(ui, "⇩ Export").clicked()
                                    && let Some(dest_path) = rfd::FileDialog::new()
                                        .add_filter("INXM plan bundle", &["json"])
                                        .set_file_name(default_export_filename(&plan.name))
                                        .set_title("Export plan")
                                        .save_file()
                                {
                                    engine.send(EngineCommand::ExportPlan {
                                        plan_ref: plan.id.clone(),
                                        dest_path,
                                    });
                                }
                                if confirm_delete.as_deref() == Some(plan.id.as_str()) {
                                    if widgets::danger_button(ui, "Really delete?").clicked() {
                                        engine.send(EngineCommand::DeletePlan {
                                            plan_id: plan.id.clone(),
                                        });
                                        next_confirm = None;
                                    }
                                } else if widgets::ghost_button(ui, "✕").clicked() {
                                    next_confirm = Some(plan.id.clone());
                                }
                            });
                        });
                        if let Some(intent) = &plan.intent {
                            widgets::clamped_label(
                                ui,
                                card_id.with("intent"),
                                RichText::new(format!("“{intent}”"))
                                    .italics()
                                    .size(theme::FONT_SMALL)
                                    .color(theme::text_muted()),
                                theme::INTENT_MAX_HEIGHT,
                            );
                        }
                        plan_run_summary(ui, &plan.id, &state.runs);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(plan_card::short_id(&plan.id))
                                    .monospace()
                                    .size(theme::FONT_SMALL)
                                    .color(theme::text_faint()),
                            );
                            ui.label(
                                RichText::new(time::format_local(&plan.updated_at, "%b %d, %H:%M"))
                                    .size(theme::FONT_SMALL)
                                    .color(theme::text_faint()),
                            );
                        });
                    });
                    ui.add_space(4.0);
                });
            }
            state.confirm_delete = next_confirm;

            if !state.plans.is_empty() {
                ui.add_space(12.0);
                schedules_summary(ui, schedules, &state.runs, agent_mode);
            }

            ui.add_space(16.0);
            ui.horizontal(|ui| {
                widgets::section_label(ui, "Recent runs");
                // The teaser stays deliberately short — everything that ran,
                // filterable, lives one click away in the Runs view.
                if !state.runs.is_empty() {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::ghost_button(ui, "View all →").clicked() {
                            action = Some(PlansAction::ViewAllRuns);
                        }
                    });
                }
            });
            ui.add_space(4.0);

            if state.runs.is_empty() {
                ui.label(RichText::new("No runs yet.").color(theme::text_muted()));
            }

            for run in recent_runs_teaser(&state.runs) {
                ui.horizontal(|ui| {
                    let (color, pulsing) = run_status_color(&run.status);
                    widgets::status_dot(ui, color, pulsing);
                    ui.label(
                        RichText::new(plan_card::short_id(&run.id))
                            .monospace()
                            .size(theme::FONT_SMALL)
                            .color(theme::text_faint()),
                    );
                    widgets::truncated_label(ui, RichText::new(&run.plan_name), 220.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        // Inspection opens the run inside the plan's chat,
                        // which doesn't exist in agent mode.
                        if !agent_mode && widgets::ghost_button(ui, "Inspect").clicked() {
                            action = Some(PlansAction::OpenRun {
                                plan_id: run.plan_id.clone(),
                                run_id: run.id.clone(),
                            });
                        }
                        ui.label(
                            RichText::new(time::format_local(&run.started_at, "%b %d, %H:%M"))
                                .size(theme::FONT_SMALL)
                                .color(theme::text_faint()),
                        );
                    });
                });
            }
            ui.add_space(12.0);
        });

    let mut import_choice = None;
    if let Some(collision) = state.import_collision.as_ref() {
        egui::Window::new("A plan with this name exists")
            .collapsible(false)
            .resizable(false)
            .show(ui.ctx(), |ui| {
                ui.label(format!(
                    "\u{201c}{}\u{201d} already exists locally.",
                    collision.name
                ));
                ui.label("Import it as a new version, make a separate copy, or cancel.");
                ui.horizontal(|ui| {
                    if widgets::primary_button(ui, "New version").clicked() {
                        import_choice = Some(crate::app::engine::ImportConflictPolicy::NewVersion);
                    }
                    if widgets::ghost_button(ui, "Copy").clicked() {
                        import_choice = Some(crate::app::engine::ImportConflictPolicy::Duplicate);
                    }
                    if widgets::ghost_button(ui, "Cancel").clicked() {
                        import_choice = Some(crate::app::engine::ImportConflictPolicy::Reject);
                    }
                });
            });
    }
    if let Some(policy) = import_choice
        && let Some(collision) = state.import_collision.take()
        && policy != crate::app::engine::ImportConflictPolicy::Reject
    {
        engine.send(EngineCommand::ImportPlanWithPolicy {
            path: collision.path,
            conflict_policy: policy,
        });
    }

    action
}

/// Build a filesystem-safe default filename for an exported plan bundle.
fn default_export_filename(plan_name: &str) -> String {
    let slug: String = plan_name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let slug = slug.trim_matches('_');
    let slug = if slug.is_empty() { "plan" } else { slug };
    format!("{slug}.inxmplan.json")
}

/// The `RECENT_RUNS_TEASER` most recent runs, newest first regardless of the
/// input order (the engine sorts newest-first today, but the teaser must not
/// silently show the *oldest* five if that ever changes).
fn recent_runs_teaser(runs: &[RunListItem]) -> Vec<&RunListItem> {
    let mut ordered: Vec<_> = runs.iter().collect();
    ordered.sort_by_key(|run| std::cmp::Reverse(run.started_at));
    ordered.truncate(RECENT_RUNS_TEASER);
    ordered
}

// ─── Status → color/label ──────────────────────────────────────────────────

/// The dot color (and whether it pulses) for a run status, shared by the
/// recent-runs list, the activity sections below, and the Runs view — one
/// status palette across pages.
pub(crate) fn run_status_color(status: &RunStatus) -> (Color32, bool) {
    match status {
        RunStatus::Succeeded => (theme::ok(), false),
        RunStatus::Failed { .. } => (theme::err(), false),
        RunStatus::Running => (theme::active(), true),
        RunStatus::WaitingForHuman { .. } => (theme::warn(), true),
        _ => (theme::text_muted(), false),
    }
}

/// Short, human status word — deliberately terser than `RunStatus`'s
/// `Display` (which embeds the failing step id), since it's used inline
/// next to a plan name rather than as a standalone error message.
fn run_status_label(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed { .. } => "failed",
        RunStatus::Running => "running",
        RunStatus::WaitingForHuman { .. } => "waiting for input",
        RunStatus::Cancelled => "cancelled",
    }
}

// ─── Per-plan run summary ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PlanRunSummary {
    total: usize,
    successful: usize,
    failing: usize,
    last_was_successful: Option<bool>,
}

fn summarize_plan_runs(runs: &[RunListItem], plan_id: &str) -> PlanRunSummary {
    let mut summary = PlanRunSummary::default();
    let mut latest = None;

    for run in runs.iter().filter(|run| run.plan_id == plan_id) {
        summary.total += 1;
        match &run.status {
            RunStatus::Succeeded => summary.successful += 1,
            RunStatus::Failed { .. } => summary.failing += 1,
            _ => {}
        }
        if latest.is_none_or(|started_at| run.started_at > started_at) {
            latest = Some(run.started_at);
            summary.last_was_successful = Some(matches!(&run.status, RunStatus::Succeeded));
        }
    }

    summary
}

fn plans_by_latest_activity<'a>(
    plans: &'a [PlanListItem],
    runs: &[RunListItem],
) -> Vec<&'a PlanListItem> {
    let mut ordered: Vec<_> = plans.iter().collect();
    ordered.sort_by_key(|plan| {
        let latest_run = runs
            .iter()
            .filter(|run| run.plan_id == plan.id)
            .map(|run| run.started_at)
            .max();
        std::cmp::Reverse(latest_run.map_or(plan.updated_at, |run| run.max(plan.updated_at)))
    });
    ordered
}

fn plan_run_summary(ui: &mut Ui, plan_id: &str, runs: &[RunListItem]) {
    let summary = summarize_plan_runs(runs, plan_id);
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!("{} runs", summary.total))
                .size(theme::FONT_SMALL)
                .color(theme::text_muted()),
        );
        ui.label(
            RichText::new(format!("{} successful", summary.successful))
                .size(theme::FONT_SMALL)
                .color(theme::ok()),
        );
        ui.label(
            RichText::new(format!("{} failing", summary.failing))
                .size(theme::FONT_SMALL)
                .color(theme::err()),
        );

        let (last_label, last_color) = match summary.last_was_successful {
            Some(true) => ("Last run successful: Yes", theme::ok()),
            Some(false) => ("Last run successful: No", theme::err()),
            None => ("Last run successful: —", theme::text_faint()),
        };
        ui.label(
            RichText::new(last_label)
                .size(theme::FONT_SMALL)
                .color(last_color),
        );
    });
}

// ─── Stats summary ──────────────────────────────────────────────────────────

/// Run outcome counts, aggregated client-side from the already-loaded run
/// list — no new persistence/engine plumbing, just a fold over what
/// `ListRuns` already returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct RunCounts {
    succeeded: usize,
    failed: usize,
    running: usize,
    /// Waiting-for-human or cancelled — states that are neither a clean
    /// success nor a failure.
    other: usize,
}

impl RunCounts {
    fn total(&self) -> usize {
        self.succeeded + self.failed + self.running + self.other
    }

    /// Percentage of *finished* runs (succeeded or failed) that succeeded.
    /// `None` when nothing has finished yet, so the caller can render "—"
    /// instead of a misleading 0%.
    fn success_rate_percent(&self) -> Option<u32> {
        let finished = self.succeeded + self.failed;
        (finished > 0).then(|| ((self.succeeded * 100) / finished) as u32)
    }
}

fn count_runs(runs: &[RunListItem]) -> RunCounts {
    let mut counts = RunCounts::default();
    for run in runs {
        match &run.status {
            RunStatus::Succeeded => counts.succeeded += 1,
            RunStatus::Failed { .. } => counts.failed += 1,
            RunStatus::Running => counts.running += 1,
            RunStatus::WaitingForHuman { .. } | RunStatus::Cancelled => counts.other += 1,
        }
    }
    counts
}

fn stats_summary(
    ui: &mut Ui,
    plans: &[PlanListItem],
    runs: &[RunListItem],
    schedules: &[ScheduleItem],
) {
    let counts = count_runs(runs);
    let active_schedules = schedules.iter().filter(|s| s.enabled).count();

    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        // A grid (rather than a row of independently-sized boxes) so each
        // value lines up under its own label — columns are sized from the
        // widest cell across both rows instead of a fixed guess.
        egui::Grid::new("plans_stats_grid")
            .num_columns(7)
            .spacing(vec2(24.0, 4.0))
            .show(ui, |ui| {
                stat_label(ui, "Plans", None);
                stat_label(ui, "Runs", None);
                stat_label(ui, "Succeeded", Some(theme::ok()));
                stat_label(ui, "Failed", Some(theme::err()));
                stat_label(ui, "Running", Some(theme::accent()));
                stat_label(ui, "Success rate", None);
                stat_label(ui, "Schedules", (active_schedules > 0).then(theme::ok));
                ui.end_row();

                stat_value(ui, plans.len().to_string());
                stat_value(ui, counts.total().to_string());
                stat_value(ui, counts.succeeded.to_string());
                stat_value(ui, counts.failed.to_string());
                stat_value(ui, counts.running.to_string());
                stat_value(
                    ui,
                    match counts.success_rate_percent() {
                        Some(rate) => format!("{rate}%"),
                        None => "—".to_owned(),
                    },
                );
                stat_value(ui, format!("{active_schedules}/{}", schedules.len()));
                ui.end_row();
            });
    });
}

/// A KPI label: a small caption with an optional status dot carrying
/// identity, since the value text itself stays in a neutral ink.
fn stat_label(ui: &mut Ui, label: &str, dot: Option<Color32>) {
    ui.horizontal(|ui| {
        if let Some(color) = dot {
            widgets::status_dot(ui, color, false);
        }
        ui.label(
            RichText::new(label)
                .size(theme::FONT_SMALL)
                .color(theme::text_faint()),
        );
    });
}

/// A KPI value, in the heading face, directly below its label's grid cell.
fn stat_value(ui: &mut Ui, value: String) {
    ui.label(theme::title(value, 20.0));
}

// ─── Runs-over-time chart ───────────────────────────────────────────────────

/// One calendar day's outcome counts, for the chart below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DayBucket {
    date: chrono::NaiveDate,
    counts: RunCounts,
}

/// Bucket runs into `window_days` fixed daily slots ending on `today`
/// (inclusive), oldest first, so the chart always shows a stable window —
/// including days with zero runs — rather than only the days that happen to
/// have data. Runs outside the window are ignored.
fn bucket_runs_by_day(
    runs: &[RunListItem],
    today: chrono::NaiveDate,
    window_days: i64,
) -> Vec<DayBucket> {
    let window_days = window_days.max(1);
    let start = today - chrono::Duration::days(window_days - 1);
    let mut buckets: Vec<DayBucket> = (0..window_days)
        .map(|offset| DayBucket {
            date: start + chrono::Duration::days(offset),
            counts: RunCounts::default(),
        })
        .collect();

    for run in runs {
        let date = time::local_date(&run.started_at);
        if date < start || date > today {
            continue;
        }
        let index = (date - start).num_days() as usize;
        let Some(bucket) = buckets.get_mut(index) else {
            continue;
        };
        match &run.status {
            RunStatus::Succeeded => bucket.counts.succeeded += 1,
            RunStatus::Failed { .. } => bucket.counts.failed += 1,
            RunStatus::Running => bucket.counts.running += 1,
            RunStatus::WaitingForHuman { .. } | RunStatus::Cancelled => bucket.counts.other += 1,
        }
    }
    buckets
}

type SeriesGetter = fn(&RunCounts) -> usize;

/// Fixed draw order (bottom of the stack first) — status color, not an
/// arbitrary categorical hue, since each series *is* a run outcome.
const RUN_SERIES: [(&str, SeriesGetter); 4] = [
    ("Succeeded", |c| c.succeeded),
    ("Failed", |c| c.failed),
    ("Running", |c| c.running),
    ("Other", |c| c.other),
];

fn series_color(label: &str) -> Color32 {
    match label {
        "Succeeded" => theme::ok(),
        "Failed" => theme::err(),
        "Running" => theme::accent(),
        _ => theme::text_muted(),
    }
}

fn runs_chart_section(ui: &mut Ui, runs: &[RunListItem]) {
    widgets::section_label(ui, "Runs over time");
    ui.add_space(4.0);
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        if runs.is_empty() {
            ui.label(
                RichText::new("No runs yet — run a plan to see activity here.")
                    .color(theme::text_muted()),
            );
            return;
        }
        let today = chrono::Local::now().date_naive();
        let buckets = bucket_runs_by_day(runs, today, CHART_WINDOW_DAYS);
        runs_chart(ui, &buckets);
    });
}

/// A stacked bar per day, one segment per outcome. Deliberately not a pie —
/// part-to-whole read at a glance over many days needs bars, not slices.
fn runs_chart(ui: &mut Ui, buckets: &[DayBucket]) {
    const BAR_MAX_WIDTH: f32 = 18.0;
    const BAR_MIN_WIDTH: f32 = 3.0;
    const BAR_GAP: f32 = 4.0;
    const SEGMENT_GAP: f32 = 2.0;
    const CHART_HEIGHT: f32 = 90.0;
    const TOP_HEADROOM: f32 = 8.0;
    const LABEL_HEIGHT: f32 = 16.0;

    // Only the outcomes that actually occur in this window get a legend
    // entry / stack segment — an "Other" bucket that's always zero here
    // would just be dead ink.
    let present: Vec<(&str, SeriesGetter)> = RUN_SERIES
        .into_iter()
        .filter(|(_, get)| buckets.iter().any(|b| get(&b.counts) > 0))
        .collect();

    // A single series needs no legend box — the section title already says
    // what's plotted.
    if present.len() > 1 {
        ui.horizontal(|ui| {
            for (label, _) in &present {
                widgets::status_dot(ui, series_color(label), false);
                ui.label(
                    RichText::new(*label)
                        .size(theme::FONT_SMALL)
                        .color(theme::text_muted()),
                );
                ui.add_space(theme::GAP);
            }
        });
        ui.add_space(6.0);
    }

    let n = buckets.len().max(1) as f32;
    let available = ui.available_width();
    let bar_w = ((available - BAR_GAP * (n - 1.0)) / n).clamp(BAR_MIN_WIDTH, BAR_MAX_WIDTH);
    let content_w = bar_w * n + BAR_GAP * (n - 1.0);
    let max_total = buckets.iter().map(|b| b.counts.total()).max().unwrap_or(0);

    let (rect, _) =
        ui.allocate_exact_size(vec2(available, CHART_HEIGHT + LABEL_HEIGHT), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }

    let baseline_y = rect.top() + CHART_HEIGHT;
    ui.painter().line_segment(
        [
            Pos2::new(rect.left(), baseline_y),
            Pos2::new(rect.left() + content_w, baseline_y),
        ],
        Stroke::new(1.0_f32, theme::divider()),
    );

    for (index, bucket) in buckets.iter().enumerate() {
        let x = rect.left() + index as f32 * (bar_w + BAR_GAP);
        let last_nonzero = present.iter().rposition(|(_, get)| get(&bucket.counts) > 0);
        let mut y = baseline_y;
        for (seg_index, (label, get)) in present.iter().enumerate() {
            let count = get(&bucket.counts);
            if count == 0 {
                continue;
            }
            let height = if max_total == 0 {
                0.0
            } else {
                (count as f32 / max_total as f32) * (CHART_HEIGHT - TOP_HEADROOM)
            };
            let top = (y - height).max(rect.top());
            // Only the outermost (topmost) segment is the mark's exposed
            // "data end" and gets rounded; the baseline stays square, and
            // interior segments between two other segments stay square too.
            let radius = if Some(seg_index) == last_nonzero {
                CornerRadius {
                    nw: 3,
                    ne: 3,
                    sw: 0,
                    se: 0,
                }
            } else {
                CornerRadius::ZERO
            };
            ui.painter().rect_filled(
                Rect::from_min_max(Pos2::new(x, top), Pos2::new(x + bar_w, y)),
                radius,
                series_color(label),
            );
            // A surface-color gap separates stacked segments instead of a
            // border drawn around them.
            y = top - SEGMENT_GAP;
        }

        // The whole day column is the hover target — bigger than the
        // painted bar, and it works even on a day with no runs at all.
        let hover_rect =
            Rect::from_min_max(Pos2::new(x, rect.top()), Pos2::new(x + bar_w, baseline_y));
        let hover_id = Id::new("plans_runs_chart_bar").with(bucket.date);
        ui.interact(hover_rect, hover_id, Sense::hover())
            .on_hover_text(bucket_tooltip(bucket));

        // Sparse x-axis labels (first / middle / last) rather than one per
        // bar — with a 14-day window at a few px per bar there's no room for
        // more without collisions, and every count is already reachable via
        // hover and the recent-runs list below.
        let is_labeled_index =
            index == 0 || index == buckets.len() - 1 || index == buckets.len() / 2;
        if is_labeled_index && bar_w >= 12.0 {
            ui.painter().text(
                Pos2::new(x + bar_w / 2.0, baseline_y + LABEL_HEIGHT / 2.0),
                egui::Align2::CENTER_CENTER,
                bucket.date.format("%b %d").to_string(),
                egui::FontId::proportional(theme::FONT_SMALL),
                theme::text_faint(),
            );
        }
    }
}

/// Hover readout for one day's column — every value the chart draws is also
/// reachable here (and in the recent-runs list below), so the tooltip
/// enhances rather than gates.
fn bucket_tooltip(bucket: &DayBucket) -> String {
    let c = &bucket.counts;
    let mut parts = Vec::new();
    if c.succeeded > 0 {
        parts.push(format!("{} succeeded", c.succeeded));
    }
    if c.failed > 0 {
        parts.push(format!("{} failed", c.failed));
    }
    if c.running > 0 {
        parts.push(format!("{} running", c.running));
    }
    if c.other > 0 {
        parts.push(format!("{} other", c.other));
    }
    let date = bucket.date.format("%b %d");
    if parts.is_empty() {
        format!("{date}: no runs")
    } else {
        format!("{date}: {}", parts.join(", "))
    }
}

// ─── Schedules summary ──────────────────────────────────────────────────────

/// The most recent run of the schedule's plan, joined by `plan_id` — the
/// data has no "this run was schedule-triggered" flag, so this is
/// deliberately framed as "last run of this plan" rather than "last
/// scheduled execution" (which would claim more precision than the data
/// supports).
fn last_run_for_plan<'a>(runs: &'a [RunListItem], plan_id: &str) -> Option<&'a RunListItem> {
    runs.iter()
        .filter(|run| run.plan_id == plan_id)
        .max_by_key(|run| run.started_at)
}

fn schedules_summary(
    ui: &mut Ui,
    schedules: &[ScheduleItem],
    runs: &[RunListItem],
    agent_mode: bool,
) {
    widgets::section_label(ui, "Schedules");
    ui.add_space(4.0);

    if schedules.is_empty() {
        ui.label(
            RichText::new(match agent_mode {
                // The “⏱ Schedule” button doesn't render in agent mode.
                true => "Nothing scheduled yet.",
                false => "Nothing scheduled yet — use “⏱ Schedule” on a plan above.",
            })
            .color(theme::text_muted()),
        );
        return;
    }

    let active = schedules.iter().filter(|s| s.enabled).count();
    ui.label(
        RichText::new(format!("{active} of {} active", schedules.len()))
            .size(theme::FONT_SMALL)
            .color(theme::text_muted()),
    );
    ui.add_space(4.0);

    for schedule in schedules {
        theme::card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                widgets::status_dot(
                    ui,
                    if schedule.enabled {
                        theme::ok()
                    } else {
                        theme::text_faint()
                    },
                    false,
                );
                widgets::truncated_label(ui, RichText::new(&schedule.plan_name).strong(), 260.0);
                ui.label(
                    RichText::new(&schedule.cron)
                        .monospace()
                        .size(theme::FONT_SMALL)
                        .color(theme::accent()),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(match &schedule.next_run_display {
                            Some(next) => format!("next {next}"),
                            None => "paused".to_owned(),
                        })
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                    );
                });
            });
            match last_run_for_plan(runs, &schedule.plan_id) {
                Some(run) => {
                    ui.horizontal(|ui| {
                        let (color, pulsing) = run_status_color(&run.status);
                        widgets::status_dot(ui, color, pulsing);
                        ui.label(
                            RichText::new(format!(
                                "Last run of this plan: {} · {}",
                                run_status_label(&run.status),
                                time::format_local(&run.started_at, "%b %d, %H:%M")
                            ))
                            .size(theme::FONT_SMALL)
                            .color(theme::text_muted()),
                        );
                    });
                }
                None => {
                    ui.label(
                        RichText::new("No runs of this plan yet.")
                            .size(theme::FONT_SMALL)
                            .color(theme::text_faint()),
                    );
                }
            }
        });
        ui.add_space(4.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_at(
        plan_id: &str,
        status: RunStatus,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> RunListItem {
        RunListItem {
            id: uuid::Uuid::new_v4().to_string(),
            plan_id: plan_id.to_owned(),
            plan_name: format!("Plan {plan_id}"),
            status,
            started_at,
            finished_at: None,
            source: None,
        }
    }

    fn plan_at(id: &str, updated_at: chrono::DateTime<chrono::Utc>) -> PlanListItem {
        PlanListItem {
            id: id.to_owned(),
            name: format!("Plan {id}"),
            version: 1,
            intent: None,
            inputs: Vec::new(),
            updated_at,
            status: crate::plan::types::PlanStatus::Published,
        }
    }

    #[test]
    fn plans_are_ordered_by_latest_plan_or_run_activity() {
        let now = chrono::Utc::now();
        let recently_compiled = plan_at("recently-compiled", now - chrono::Duration::hours(1));
        let recently_run = plan_at("recently-run", now - chrono::Duration::days(30));
        let inactive = plan_at("inactive", now - chrono::Duration::days(2));
        let plans = vec![recently_compiled, recently_run, inactive];
        let runs = vec![run_at("recently-run", RunStatus::Succeeded, now)];

        let ordered = plans_by_latest_activity(&plans, &runs);
        assert_eq!(
            ordered
                .iter()
                .map(|plan| plan.id.as_str())
                .collect::<Vec<_>>(),
            vec!["recently-run", "recently-compiled", "inactive"]
        );
    }

    #[test]
    fn plan_activity_order_handles_unsorted_runs() {
        let now = chrono::Utc::now();
        let plans = vec![
            plan_at("first", now - chrono::Duration::days(3)),
            plan_at("second", now - chrono::Duration::days(3)),
        ];
        let runs = vec![
            run_at("first", RunStatus::Succeeded, now),
            run_at(
                "second",
                RunStatus::Succeeded,
                now - chrono::Duration::hours(1),
            ),
            run_at(
                "first",
                RunStatus::Succeeded,
                now - chrono::Duration::days(1),
            ),
        ];

        let ordered = plans_by_latest_activity(&plans, &runs);
        assert_eq!(ordered[0].id, "first");
        assert_eq!(ordered[1].id, "second");
    }

    #[test]
    fn count_runs_buckets_every_status_variant() {
        let now = chrono::Utc::now();
        let runs = vec![
            run_at("p", RunStatus::Succeeded, now),
            run_at("p", RunStatus::Succeeded, now),
            run_at(
                "p",
                RunStatus::Failed {
                    failed_step_id: "s".to_owned(),
                    message: "boom".to_owned(),
                },
                now,
            ),
            run_at("p", RunStatus::Running, now),
            run_at("p", RunStatus::Cancelled, now),
            run_at(
                "p",
                RunStatus::WaitingForHuman {
                    step_id: "s".to_owned(),
                },
                now,
            ),
        ];
        let counts = count_runs(&runs);
        assert_eq!(counts.succeeded, 2);
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.running, 1);
        assert_eq!(counts.other, 2);
        assert_eq!(counts.total(), 6);
    }

    #[test]
    fn running_and_human_wait_runs_use_distinct_status_colors() {
        assert_eq!(
            run_status_color(&RunStatus::Running),
            (theme::active(), true)
        );
        assert_eq!(
            run_status_color(&RunStatus::WaitingForHuman {
                step_id: "s".to_owned(),
            }),
            (theme::warn(), true)
        );
    }

    #[test]
    fn success_rate_ignores_unfinished_runs() {
        let now = chrono::Utc::now();
        let runs = vec![
            run_at("p", RunStatus::Succeeded, now),
            run_at("p", RunStatus::Succeeded, now),
            run_at(
                "p",
                RunStatus::Failed {
                    failed_step_id: "s".to_owned(),
                    message: "boom".to_owned(),
                },
                now,
            ),
            run_at("p", RunStatus::Running, now),
        ];
        let counts = count_runs(&runs);
        // 2 of 3 finished runs succeeded; the still-running one doesn't count.
        assert_eq!(counts.success_rate_percent(), Some(66));
    }

    #[test]
    fn success_rate_is_none_with_no_finished_runs() {
        let counts = count_runs(&[run_at("p", RunStatus::Running, chrono::Utc::now())]);
        assert_eq!(counts.success_rate_percent(), None);
    }

    #[test]
    fn bucket_runs_by_day_produces_a_stable_window_oldest_first() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        let buckets = bucket_runs_by_day(&[], today, 5);
        assert_eq!(buckets.len(), 5);
        assert_eq!(
            buckets.first().unwrap().date,
            today - chrono::Duration::days(4)
        );
        assert_eq!(buckets.last().unwrap().date, today);
    }

    #[test]
    fn bucket_runs_by_day_sorts_runs_into_the_right_day() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        let two_days_ago = (today - chrono::Duration::days(2))
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .single()
            .unwrap()
            .with_timezone(&chrono::Utc);
        let runs = vec![run_at("p", RunStatus::Succeeded, two_days_ago)];
        let buckets = bucket_runs_by_day(&runs, today, 5);
        let bucket = buckets
            .iter()
            .find(|b| b.date == today - chrono::Duration::days(2))
            .unwrap();
        assert_eq!(bucket.counts.succeeded, 1);
        assert_eq!(bucket.counts.total(), 1);
        // Every other day stays empty.
        assert_eq!(buckets.iter().map(|b| b.counts.total()).sum::<usize>(), 1);
    }

    #[test]
    fn bucket_runs_by_day_ignores_runs_outside_the_window() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        let long_ago = (today - chrono::Duration::days(30))
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .single()
            .unwrap()
            .with_timezone(&chrono::Utc);
        let runs = vec![run_at("p", RunStatus::Succeeded, long_ago)];
        let buckets = bucket_runs_by_day(&runs, today, 5);
        assert_eq!(buckets.iter().map(|b| b.counts.total()).sum::<usize>(), 0);
    }

    #[test]
    fn last_run_for_plan_picks_the_most_recent_regardless_of_input_order() {
        let now = chrono::Utc::now();
        let older = run_at(
            "p",
            RunStatus::Failed {
                failed_step_id: "s".to_owned(),
                message: "m".to_owned(),
            },
            now - chrono::Duration::hours(2),
        );
        let newer = run_at("p", RunStatus::Succeeded, now);
        let runs = vec![older.clone(), newer.clone()];
        let found = last_run_for_plan(&runs, "p").unwrap();
        assert_eq!(found.id, newer.id);

        let reversed = vec![newer, older];
        let found = last_run_for_plan(&reversed, "p").unwrap();
        assert_eq!(found.status, RunStatus::Succeeded);
    }

    #[test]
    fn last_run_for_plan_is_none_without_a_matching_run() {
        let runs = vec![run_at(
            "other-plan",
            RunStatus::Succeeded,
            chrono::Utc::now(),
        )];
        assert!(last_run_for_plan(&runs, "p").is_none());
    }

    #[test]
    fn plan_run_summary_counts_only_the_selected_plans_runs() {
        let now = chrono::Utc::now();
        let runs = vec![
            run_at("p", RunStatus::Succeeded, now),
            run_at(
                "p",
                RunStatus::Failed {
                    failed_step_id: "s".to_owned(),
                    message: "boom".to_owned(),
                },
                now - chrono::Duration::minutes(1),
            ),
            run_at("p", RunStatus::Running, now - chrono::Duration::minutes(2)),
            run_at("other", RunStatus::Succeeded, now),
        ];

        assert_eq!(
            summarize_plan_runs(&runs, "p"),
            PlanRunSummary {
                total: 3,
                successful: 1,
                failing: 1,
                last_was_successful: Some(true),
            }
        );
    }

    #[test]
    fn plan_run_summary_finds_the_last_run_regardless_of_input_order() {
        let now = chrono::Utc::now();
        let latest_failure = run_at(
            "p",
            RunStatus::Failed {
                failed_step_id: "s".to_owned(),
                message: "boom".to_owned(),
            },
            now,
        );
        let older_success = run_at(
            "p",
            RunStatus::Succeeded,
            now - chrono::Duration::minutes(1),
        );

        let runs = vec![latest_failure.clone(), older_success.clone()];
        assert_eq!(
            summarize_plan_runs(&runs, "p").last_was_successful,
            Some(false)
        );

        let reversed = vec![older_success, latest_failure];
        assert_eq!(
            summarize_plan_runs(&reversed, "p").last_was_successful,
            Some(false)
        );
    }

    #[test]
    fn plan_run_summary_has_no_last_result_without_runs() {
        assert_eq!(summarize_plan_runs(&[], "p"), PlanRunSummary::default());
    }

    #[test]
    fn stats_summary_and_chart_do_not_panic_on_empty_input() {
        // Regression guard: the sections must degrade gracefully rather than
        // dividing by zero or indexing past the plan/run lists.
        let counts = count_runs(&[]);
        assert_eq!(counts.total(), 0);
        assert_eq!(counts.success_rate_percent(), None);
        let today = chrono::Local::now().date_naive();
        let buckets = bucket_runs_by_day(&[], today, CHART_WINDOW_DAYS);
        assert_eq!(buckets.len(), CHART_WINDOW_DAYS as usize);
    }

    #[test]
    fn recent_runs_teaser_keeps_the_five_newest_regardless_of_input_order() {
        let now = chrono::Utc::now();
        // Oldest-first input — the teaser must still surface the newest five.
        let runs: Vec<RunListItem> = (0..8)
            .map(|age| {
                run_at(
                    "p",
                    RunStatus::Succeeded,
                    now - chrono::Duration::hours(8 - age),
                )
            })
            .collect();

        let teaser = recent_runs_teaser(&runs);
        assert_eq!(teaser.len(), RECENT_RUNS_TEASER);
        assert_eq!(teaser.first().unwrap().started_at, runs[7].started_at);
        // Newest first within the teaser.
        assert!(
            teaser
                .windows(2)
                .all(|pair| pair[0].started_at >= pair[1].started_at)
        );
    }

    #[test]
    fn recent_runs_teaser_shows_everything_when_fewer_than_the_cap() {
        let runs = vec![run_at("p", RunStatus::Succeeded, chrono::Utc::now())];
        assert_eq!(recent_runs_teaser(&runs).len(), 1);
    }

    #[test]
    fn default_export_filename_slugs_special_characters() {
        assert_eq!(
            default_export_filename("hn digest!"),
            "hn_digest.inxmplan.json"
        );
    }
}
