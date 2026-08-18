//! Plan card — renders a compiled plan as a vertical dependency graph with
//! live run status, staggered entrance animation, and painted connector
//! curves between dependent steps.

use std::collections::{HashMap, HashSet};

use egui::{Align, Color32, Id, Key, Layout, Order, Pos2, RichText, Stroke, Ui};

use crate::executor::{FanOutProgress, RunStatus, StepRunIteration, StepRunStatus};
use crate::plan::types::{InputKind, Plan, PlanInput, PlanStep, StepConfig, StepType};

use crate::app::engine::RunListItem;
use crate::app::{anim, theme, time, widgets};

const INDENT_PER_LEVEL: f32 = 22.0;
const CONNECTOR_CURVE: f32 = 14.0;
const MAX_INLINE_ERROR_CHARS: usize = 240;

// ─── Run binding ──────────────────────────────────────────────────────────────

/// Live (or final) run state attached to a plan card.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunBinding {
    pub run_id: String,
    #[serde(default)]
    pub inputs: indexmap::IndexMap<String, serde_json::Value>,
    pub statuses: HashMap<String, StepRunStatus>,
    pub errors: HashMap<String, String>,
    pub durations_ms: HashMap<String, u64>,
    /// Captured stdout per step — the visible result of a run.
    #[serde(default)]
    pub stdouts: HashMap<String, String>,
    /// Captured stderr per step (debugging detail).
    #[serde(default)]
    pub stderrs: HashMap<String, String>,
    /// Named outputs per step, shown when a step produced no stdout.
    #[serde(default)]
    pub outputs: HashMap<String, indexmap::IndexMap<String, serde_json::Value>>,
    /// Per-item executions for templates owned by a FAN_OUT step.
    #[serde(default)]
    pub iterations: HashMap<String, Vec<StepRunIteration>>,
    /// Current live position of each running FAN_OUT step.
    #[serde(skip)]
    pub fan_out_progress: HashMap<String, FanOutProgress>,
    /// Terminal run status once known.
    pub finished: Option<RunStatus>,
}

impl RunBinding {
    pub fn is_active(&self) -> bool {
        self.finished.is_none()
    }
}

/// Action requested by the user from a plan card.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanCardAction {
    Run {
        plan_id: String,
        inputs: indexmap::IndexMap<String, serde_json::Value>,
    },
    Edit {
        plan_id: String,
        instruction: String,
    },
    /// Re-execute a failed run in place, starting from its failed step and
    /// everything downstream of it in the current plan version's DAG. The
    /// engine resolves the actual resume point server-side; `from_step` is
    /// carried only for the button's own label.
    Resume {
        plan_id: String,
        run_id: String,
        from_step: Option<String>,
        inputs: indexmap::IndexMap<String, serde_json::Value>,
    },
    /// Jump to the Schedules view with this plan preselected.
    Schedule { plan_id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkspaceAction {
    Plan(PlanCardAction),
    InspectRun(String),
}

// ─── Rendering ────────────────────────────────────────────────────────────────

/// Render the card; returns an action if the user clicked one.
pub fn show(
    ui: &mut Ui,
    card_id: Id,
    plan: &Plan,
    run: Option<&RunBinding>,
) -> Option<PlanCardAction> {
    let mut action = None;

    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());

        header(ui, plan, run);
        ui.add_space(2.0);
        if let Some(intent) = &plan.metadata.intent {
            widgets::clamped_label(
                ui,
                card_id.with("intent"),
                RichText::new(format!("“{intent}”"))
                    .italics()
                    .color(theme::text_muted())
                    .size(theme::FONT_SMALL),
                theme::INTENT_MAX_HEIGHT,
            );
        }
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(4.0);

        if !plan.inputs.is_empty() {
            input_fields(ui, card_id, &plan.inputs);
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(4.0);
        }

        steps_graph(ui, card_id, plan, run);

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(2.0);
        action = footer(ui, card_id, plan, run);
    });

    action
}

/// Compact, fixed plan workspace used above a plan-owned conversation.
/// Details and execution history expand inside the header without moving the
/// plan out of view while the transcript scrolls.
pub fn show_workspace(
    ui: &mut Ui,
    card_id: Id,
    plan: &Plan,
    run: Option<&RunBinding>,
    runs: &[RunListItem],
) -> Option<WorkspaceAction> {
    let mut action = None;
    // Stores the id of the run the full-screen takeover was opened for (see
    // `show_expanded_workspace`), not a bare bool. Keying on the run — not
    // just the plan — means a takeover requested for a run that is no longer
    // the one bound to this workspace can never resurrect itself: it simply
    // stops matching. A plain bool here previously survived indefinitely
    // (egui's temp-memory has no expiry within a running session) and was
    // keyed only by the plan's stable id, so inspecting any one run, ever,
    // would re-open the full-screen view on every later visit to that plan —
    // including right after clicking "Edit", which shares no code path with
    // this flag at all.
    let expanded_id = card_id.with("workspace_expanded");

    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        // The card lives in its own side panel with nothing below it, so it
        // claims the full panel height; overflowing sections (history,
        // inspection, edit panel) scroll inside the shared scroll area below.
        let available_height = ui.available_height();
        ui.set_min_height(available_height);
        ui.set_max_height(available_height);

        // All action buttons share one row docked to the bottom of the panel;
        // the edit/JSON sections a button opens grow upward from
        // that row instead of scrolling with the body above.
        let mut footer_action = None;
        egui::TopBottomPanel::bottom(card_id.with("workspace_actions"))
            .frame(egui::Frame::new())
            .show_separator_line(true)
            .show_inside(ui, |ui| {
                ui.add_space(6.0);
                footer_action = footer(ui, card_id, plan, run);
            });
        if let Some(plan_action) = footer_action {
            action = Some(WorkspaceAction::Plan(plan_action));
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new())
            .show_inside(ui, |ui| {
                header(ui, plan, run);
                if let Some(intent) = &plan.metadata.intent {
                    widgets::clamped_label(
                        ui,
                        card_id.with("workspace_intent"),
                        RichText::new(intent)
                            .italics()
                            .color(theme::text_muted())
                            .size(theme::FONT_SMALL),
                        theme::INTENT_MAX_HEIGHT,
                    );
                }

                // Inputs and history share one scroll area that fills the
                // space between the header above and the docked action row
                // below; history is always open.
                egui::ScrollArea::vertical()
                    .id_salt(card_id.with("card_body"))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if !plan.inputs.is_empty() {
                            ui.add_space(4.0);
                            input_fields(ui, card_id, &plan.inputs);
                        }

                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(format!("History ({})", runs.len()))
                                .size(theme::FONT_SMALL)
                                .color(theme::text_muted()),
                        );
                        ui.separator();
                        if runs.is_empty() {
                            ui.label(
                                RichText::new("No executions yet.").color(theme::text_muted()),
                            );
                        }
                        for item in runs {
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
                                    RichText::new(short_id(&item.id))
                                        .monospace()
                                        .size(theme::FONT_SMALL)
                                        .color(theme::text_faint()),
                                );
                                ui.label(
                                    RichText::new(item.status.to_string())
                                        .size(theme::FONT_SMALL)
                                        .color(color),
                                );
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if widgets::ghost_button(ui, "Inspect").clicked() {
                                        action = Some(WorkspaceAction::InspectRun(item.id.clone()));
                                    }
                                    ui.label(
                                        RichText::new(time::format_local(
                                            &item.started_at,
                                            "%b %d, %H:%M",
                                        ))
                                        .size(theme::FONT_SMALL)
                                        .color(theme::text_faint()),
                                    );
                                });
                            });
                        }
                    });
            });
    });

    let expanded_for_run = ui.ctx().data(|data| data.get_temp::<String>(expanded_id));
    let expanded =
        run.is_some_and(|binding| expanded_for_run.as_deref() == Some(binding.run_id.as_str()));
    if expanded {
        show_expanded_workspace(ui, card_id.with("expanded"), expanded_id, plan, run);
    }
    action
}

/// App-sized plan view layered above the current conversation. This is kept
/// inside the native window so closing it restores the exact chat position.
fn show_expanded_workspace(
    ui: &mut Ui,
    card_id: Id,
    expanded_id: Id,
    plan: &Plan,
    run: Option<&RunBinding>,
) {
    let ctx = ui.ctx().clone();
    let screen = ctx.screen_rect();
    let mut close = ctx.input(|input| input.key_pressed(Key::Escape));

    egui::Area::new(card_id.with("area"))
        .order(Order::Foreground)
        .fixed_pos(screen.min)
        .show(&ctx, |ui| {
            ui.set_width(screen.width());
            ui.set_height(screen.height());
            egui::Frame::new()
                .fill(theme::bg())
                .inner_margin(egui::Margin::same(24))
                .show(ui, |ui| {
                    ui.set_width(screen.width() - 48.0);
                    ui.set_height(screen.height() - 48.0);
                    ui.horizontal(|ui| {
                        if widgets::ghost_button(ui, "← Back").clicked() {
                            close = true;
                        }
                        ui.add_space(8.0);
                        ui.heading(RichText::new("Run details").color(theme::text()));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if widgets::ghost_icon_button(ui, widgets::Icon::Collapse)
                                .on_hover_text("Collapse plan")
                                .clicked()
                            {
                                close = true;
                            }
                            ui.label(
                                RichText::new("Esc")
                                    .size(theme::FONT_SMALL)
                                    .color(theme::text_faint()),
                            );
                        });
                    });
                    ui.separator();
                    let body_height = ui.available_height();
                    egui::ScrollArea::vertical()
                        .id_salt(card_id.with("inspection_scroll"))
                        .auto_shrink([false, false])
                        .max_height(body_height)
                        .show(ui, |ui| {
                            ui.set_max_width(ui.available_width());
                            header(ui, plan, run);
                            if let Some(intent) = &plan.metadata.intent {
                                widgets::wrapped_label(
                                    ui,
                                    RichText::new(intent)
                                        .italics()
                                        .color(theme::text_muted())
                                        .size(theme::FONT_SMALL),
                                );
                            }
                            if !plan.inputs.is_empty() {
                                ui.add_space(6.0);
                                readonly_input_fields(ui, plan, run);
                            }
                            ui.add_space(6.0);
                            ui.separator();
                            steps_graph(ui, card_id.with("graph"), plan, run);
                            ui.add_space(12.0);
                        });
                });
        });

    if close {
        ctx.data_mut(|data| data.remove_temp::<String>(expanded_id));
    }
}

fn header(ui: &mut Ui, plan: &Plan, run: Option<&RunBinding>) {
    ui.horizontal(|ui| {
        widgets::truncated_label(ui, theme::title(&plan.name, 16.0), 220.0);
        widgets::badge(
            ui,
            &format!("v{}", plan.metadata.version),
            theme::text_muted(),
        );
        if let Some(binding) = run {
            run_status_chip(ui, binding);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(short_id(&plan.metadata.id))
                    .monospace()
                    .size(theme::FONT_SMALL)
                    .color(theme::text_faint()),
            );
        });
    });
}

fn run_status_chip(ui: &mut Ui, binding: &RunBinding) {
    match &binding.finished {
        None => {
            widgets::badge(ui, "running", theme::active());
        }
        Some(RunStatus::Succeeded) => {
            widgets::badge(ui, "succeeded", theme::ok());
        }
        Some(RunStatus::Failed { .. }) => {
            widgets::badge(ui, "failed", theme::err());
        }
        Some(RunStatus::WaitingForHuman { .. }) => {
            widgets::badge(ui, "waiting for input", theme::warn());
        }
        Some(other) => {
            widgets::badge(ui, &other.to_string(), theme::text_muted());
        }
    }
}

fn steps_graph(ui: &mut Ui, card_id: Id, plan: &Plan, run: Option<&RunBinding>) {
    let owners = fan_out_owner_of(plan);
    let depths = topo_depths(plan, &owners);
    let order = display_order(plan, &owners);
    let mut dot_centers: HashMap<&str, Pos2> = HashMap::new();

    for (index, step) in order.iter().enumerate() {
        let step = *step;
        let depth = *depths.get(step.id.as_str()).unwrap_or(&0);
        let row_id = card_id.with("step").with(&step.id);

        anim::entrance(ui, row_id, index as f32 * anim::STAGGER_SECS, |ui| {
            let detail_id = row_id.with("detail_open");
            let detail_open: bool = ui.ctx().data_mut(|d| *d.get_temp_mut_or(detail_id, false));

            let row = ui.horizontal(|ui| {
                ui.add_space(depth as f32 * INDENT_PER_LEVEL);

                let status = run.and_then(|r| r.statuses.get(&step.id));
                let (dot_color, pulsing) = step_dot(step, status);
                let dot = widgets::status_dot(ui, dot_color, pulsing);
                dot_centers.insert(step.id.as_str(), dot.rect.center());

                let (type_label, type_color) = step_type_style(step.step_type());
                widgets::badge(ui, type_label, type_color);

                let name = RichText::new(&step.name).size(theme::FONT_BODY);
                let name = match status {
                    Some(StepRunStatus::Skipped | StepRunStatus::Cancelled) => {
                        name.strikethrough().color(theme::text_faint())
                    }
                    Some(StepRunStatus::Failed) => name.color(theme::err()),
                    _ => name,
                };
                let after = step_after_label(step, plan, &owners);
                let after_reserve = after.as_ref().map_or(0.0, |_| 120.0);
                let name_width = (ui.available_width() - 70.0 - after_reserve).max(60.0);
                let label = ui
                    .scope(|ui| {
                        ui.set_max_width(name_width);
                        ui.add(egui::Label::new(name).truncate())
                    })
                    .inner;
                if let Some(hover) = step_hover_text(step) {
                    label.on_hover_text(hover);
                }

                if let Some(after) = after {
                    widgets::truncated_label(
                        ui,
                        RichText::new(after)
                            .size(theme::FONT_SMALL)
                            .color(theme::text_faint()),
                        70.0,
                    );
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Chevron hints that the row expands into a detail view.
                    ui.label(
                        RichText::new(if detail_open { "▾" } else { "▸" })
                            .size(theme::FONT_SMALL)
                            .color(theme::text_faint()),
                    );
                    if let Some(progress) = run
                        .filter(|_| matches!(step.config, StepConfig::FanOut(_)))
                        .and_then(|r| r.fan_out_progress.get(&step.id))
                        .filter(|_| matches!(status, Some(StepRunStatus::Running)))
                    {
                        ui.label(
                            RichText::new(format!(
                                "Iteration {} of {}",
                                progress.iteration + 1,
                                progress.total_iterations
                            ))
                            .size(theme::FONT_SMALL)
                            .color(theme::text_faint()),
                        );
                    } else if let Some(ms) = run.and_then(|r| r.durations_ms.get(&step.id)) {
                        ui.label(
                            RichText::new(format_duration(*ms))
                                .size(theme::FONT_SMALL)
                                .color(theme::text_faint()),
                        );
                    }
                });
            });

            let row_clicked = row
                .response
                .interact(egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked();
            if row_clicked {
                ui.ctx()
                    .data_mut(|d| d.insert_temp(detail_id, !detail_open));
            }

            if detail_open {
                step_detail(ui, row_id, depth, plan, step, run);
            }

            if let Some(error) = run.and_then(|r| r.errors.get(&step.id)) {
                ui.horizontal_top(|ui| {
                    ui.add_space(depth as f32 * INDENT_PER_LEVEL + 24.0);
                    ui.vertical(|ui| {
                        ui.set_max_width(ui.available_width());
                        expandable_text(
                            ui,
                            row_id.with("inline_error"),
                            error,
                            MAX_INLINE_ERROR_CHARS,
                            theme::err(),
                            false,
                        );
                    });
                });
            }

            let hide_aggregate = matches!(step.config, StepConfig::FanOut(_))
                && run.is_some_and(|binding| fan_out_has_iteration_results(step, binding));
            if !hide_aggregate && let Some(result) = run.and_then(|r| step_result_text(r, &step.id))
            {
                step_result_block(ui, row_id.with("result"), depth, &result);
            }
        });
    }

    paint_connectors(ui, plan, &dot_centers, &owners);
}

/// Draw a soft curve from each dependency's dot to the dependent step's dot.
/// FAN_OUT-owned steps with no real `depends_on` get a synthetic edge from
/// their owner so the graph shows they run inside it, rather than floating
/// disconnected at the top of the card.
fn paint_connectors(
    ui: &Ui,
    plan: &Plan,
    dot_centers: &HashMap<&str, Pos2>,
    owners: &HashMap<&str, &str>,
) {
    let painter = ui.painter();
    let stroke = Stroke::new(1.0_f32, theme::with_alpha(theme::accent(), 0.28));

    for step in &plan.steps {
        let Some(&to) = dot_centers.get(step.id.as_str()) else {
            continue;
        };
        for dep in effective_dependency_ids(step, owners) {
            let Some(&from) = dot_centers.get(dep) else {
                continue;
            };
            let dy = ((to.y - from.y) * 0.5).clamp(0.0, CONNECTOR_CURVE * 2.0);
            let shape = egui::epaint::CubicBezierShape::from_points_stroke(
                [
                    from,
                    Pos2::new(from.x, from.y + dy),
                    Pos2::new(to.x, to.y - dy),
                    to,
                ],
                false,
                Color32::TRANSPARENT,
                stroke,
            );
            painter.add(shape);
        }
    }
}

/// An optional `root_directory` means "no filesystem access needed — the
/// runtime uses a managed scratch workspace". Most users never need to touch
/// it, so it hides behind the collapsed "Advanced" panel. A *required*
/// `root_directory` stays with the regular inputs.
fn is_advanced_input(input: &PlanInput) -> bool {
    input.name == crate::plan::types::ROOT_DIRECTORY_INPUT && !input.required
}

/// Render one typed row per declared input — label, hint by `value_type`,
/// required marker, description, and a directory-browse button for root-dir
/// inputs. `card_id` namespaces the per-field editing drafts in egui temp
/// storage, so any caller with its own distinct id (a plan card, a schedule
/// form, …) can use this without colliding with another caller's drafts.
pub(crate) fn input_fields(ui: &mut Ui, card_id: Id, inputs: &[PlanInput]) {
    input_fields_seeded(ui, card_id, inputs, &indexmap::IndexMap::new());
}

/// Render typed input fields using supplied values as the initial drafts.
/// This is used by repair-resume forms, whose safe baseline is the failed
/// run's captured invocation rather than a plan's defaults.
pub(crate) fn input_fields_seeded(
    ui: &mut Ui,
    card_id: Id,
    inputs: &[PlanInput],
    initial_values: &indexmap::IndexMap<String, serde_json::Value>,
) {
    widgets::section_label(ui, "Inputs");
    for input in inputs.iter().filter(|input| !is_advanced_input(input)) {
        input_field(ui, card_id, input, initial_values.get(&input.name));
    }
    let advanced: Vec<&PlanInput> = inputs
        .iter()
        .filter(|input| is_advanced_input(input))
        .collect();
    if !advanced.is_empty() {
        egui::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            card_id.with("advanced_inputs"),
            false,
        )
        .show_header(ui, |ui| {
            ui.label(
                RichText::new("Advanced")
                    .size(theme::FONT_SMALL)
                    .color(theme::text_muted()),
            );
        })
        .body(|ui| {
            for input in advanced {
                input_field(ui, card_id, input, initial_values.get(&input.name));
            }
        });
    }
}

fn input_field(
    ui: &mut Ui,
    card_id: Id,
    input: &PlanInput,
    initial_value: Option<&serde_json::Value>,
) {
    let draft_id = card_id.with("input").with(&input.name);
    let mut draft = ui.ctx().data_mut(|data| {
        data.get_temp::<String>(draft_id).unwrap_or_else(|| {
            initial_value
                .or(input.default.as_ref())
                .map(input_value_text)
                .unwrap_or_default()
        })
    });
    let path_picker = input_path_picker(input);
    let mut browse_clicked = false;
    ui.horizontal(|ui| {
        let required = if input.required && input.default.is_none() {
            " *"
        } else {
            ""
        };
        ui.label(
            RichText::new(format!("{}{required}", input.name))
                .size(theme::FONT_SMALL)
                .color(theme::text_muted()),
        );
        let hint = format!("{} value", input.value_type);
        let response = if path_picker.is_some() {
            // Lay the fixed-size trailing action out first, then give the
            // editor exactly the remaining width. Guessing the button width
            // here created a small overflow on every frame; because the row
            // lives in a resizable SidePanel, egui persisted that expanded
            // response rect and the panel grew again on the next frame until
            // it consumed the entire chat.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                browse_clicked = widgets::ghost_button(ui, "Browse…").clicked();
                let edit_width = ui.available_width().max(0.0);
                widgets::text_edit(
                    ui,
                    egui::TextEdit::singleline(&mut draft)
                        .hint_text(hint)
                        .desired_width(edit_width),
                )
            })
            .inner
        } else {
            let edit_width = ui.available_width().max(0.0);
            widgets::text_edit(
                ui,
                egui::TextEdit::singleline(&mut draft)
                    .hint_text(hint)
                    .desired_width(edit_width),
            )
        };
        if response.changed() {
            clear_input_error(ui, card_id);
        }
    });
    if browse_clicked
        && let Some(path) = pick_path(path_picker.expect("button only exists for path inputs"))
    {
        draft = path.display().to_string();
        clear_input_error(ui, card_id);
    }
    if let Some(description) = &input.description {
        widgets::wrapped_label(
            ui,
            RichText::new(description)
                .size(theme::FONT_SMALL)
                .color(theme::text_faint()),
        );
    }
    ui.ctx().data_mut(|data| data.insert_temp(draft_id, draft));
}

/// The native picker appropriate for an input's effective semantic kind.
/// `effective_input_kind` deliberately keeps legacy `root_directory` plans
/// on the directory picker without requiring a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathPicker {
    OpenFile,
    SaveFile,
    Folder,
}

pub(crate) fn input_path_picker(input: &PlanInput) -> Option<PathPicker> {
    match input.effective_input_kind() {
        InputKind::Value => None,
        InputKind::FilePath => Some(PathPicker::OpenFile),
        InputKind::OutputFilePath => Some(PathPicker::SaveFile),
        InputKind::DirectoryPath => Some(PathPicker::Folder),
    }
}

fn pick_path(picker: PathPicker) -> Option<std::path::PathBuf> {
    let dialog = rfd::FileDialog::new().set_title(match picker {
        PathPicker::OpenFile => "Select a file",
        PathPicker::SaveFile => "Choose where to save the file",
        PathPicker::Folder => "Select the plan's root directory",
    });
    match picker {
        PathPicker::OpenFile => dialog.pick_file(),
        PathPicker::SaveFile => dialog.save_file(),
        PathPicker::Folder => dialog.pick_folder(),
    }
}

/// Inputs captured for the inspected execution. These deliberately do not use
/// the editable workspace drafts, which may have changed since the run began.
fn readonly_input_fields(ui: &mut Ui, plan: &Plan, run: Option<&RunBinding>) {
    widgets::section_label(ui, "Run inputs");
    for input in &plan.inputs {
        let mut value = run
            .and_then(|binding| binding.inputs.get(&input.name))
            .map(input_value_text)
            .unwrap_or_default();
        let required = if input.required && input.default.is_none() {
            " *"
        } else {
            ""
        };
        widgets::wrapped_label(
            ui,
            RichText::new(format!("{}{required}", input.name))
                .size(theme::FONT_SMALL)
                .color(theme::text_muted()),
        );
        widgets::text_edit(
            ui,
            egui::TextEdit::singleline(&mut value)
                .desired_width(ui.available_width())
                .interactive(false),
        );
        if let Some(description) = &input.description {
            widgets::wrapped_label(
                ui,
                RichText::new(description)
                    .size(theme::FONT_SMALL)
                    .color(theme::text_faint()),
            );
        }
        ui.add_space(2.0);
    }
}

/// Coerce the typed drafts rendered by [`input_fields`] under `card_id` into
/// the final JSON value map, applying each input's declared type.
pub(crate) fn input_values(
    ui: &Ui,
    card_id: Id,
    inputs: &[PlanInput],
) -> Result<indexmap::IndexMap<String, serde_json::Value>, String> {
    let mut values = indexmap::IndexMap::new();
    for input in inputs {
        let draft = ui.ctx().data(|data| {
            data.get_temp::<String>(card_id.with("input").with(&input.name))
                .unwrap_or_default()
        });
        let trimmed = draft.trim();
        if trimmed.is_empty() {
            if input.required && input.default.is_none() {
                return Err(format!("Input '{}' is required", input.name));
            }
            continue;
        }
        let value = parse_input_value(input, trimmed)?;
        values.insert(input.name.clone(), value);
    }
    Ok(values)
}

fn parse_input_value(input: &PlanInput, text: &str) -> Result<serde_json::Value, String> {
    if input.value_type == "string" {
        return Ok(serde_json::Value::String(text.to_owned()));
    }
    serde_json::from_str(text).map_err(|error| {
        format!(
            "Input '{}' must be valid {} JSON: {error}",
            input.name, input.value_type
        )
    })
}

fn input_value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

fn clear_input_error(ui: &Ui, card_id: Id) {
    ui.ctx()
        .data_mut(|data| data.remove_temp::<String>(card_id.with("input_error")));
}

fn footer(
    ui: &mut Ui,
    card_id: Id,
    plan: &Plan,
    run: Option<&RunBinding>,
) -> Option<PlanCardAction> {
    let mut action = None;
    let run_active = run.is_some_and(|r| r.is_active());
    let failed_step = run.and_then(|r| match &r.finished {
        Some(RunStatus::Failed { failed_step_id, .. }) => Some(failed_step_id.as_str()),
        _ => None,
    });

    // A repaired plan may introduce inputs or change defaults. Keep this
    // draft isolated from the ordinary "Run" form and seed it from the
    // failed execution, so operators can correct the failure without
    // accidentally losing values that were valid for the original run.
    let resume_inputs_id = run.map(|binding| card_id.with("resume_inputs").with(&binding.run_id));
    if let (Some(_), Some(binding), Some(inputs_id)) = (failed_step, run, resume_inputs_id) {
        ui.add_space(6.0);
        widgets::section_label(ui, "Resume inputs");
        widgets::wrapped_label(
            ui,
            RichText::new("Update values if needed, then resume the repaired run. Values used by completed steps cannot be changed.")
                .size(theme::FONT_SMALL)
                .color(theme::text_muted()),
        );
        input_fields_seeded(ui, inputs_id, &plan.inputs, &binding.inputs);
        if let Some(error) = ui
            .ctx()
            .data(|data| data.get_temp::<String>(inputs_id.with("input_error")))
        {
            widgets::wrapped_label(
                ui,
                RichText::new(error)
                    .size(theme::FONT_SMALL)
                    .color(theme::err()),
            );
        }
    }

    ui.horizontal_wrapped(|ui| {
        ui.add_enabled_ui(!run_active, |ui| {
            if widgets::primary_button(ui, "▶ Run").clicked() {
                match input_values(ui, card_id, &plan.inputs) {
                    Ok(inputs) => {
                        clear_input_error(ui, card_id);
                        action = Some(PlanCardAction::Run {
                            plan_id: plan.metadata.id.clone(),
                            inputs,
                        })
                    }
                    Err(error) => ui
                        .ctx()
                        .data_mut(|data| data.insert_temp(card_id.with("input_error"), error)),
                }
            }
            if let (Some(step_id), Some(binding), Some(inputs_id)) =
                (failed_step, run, resume_inputs_id)
            {
                let step_name = plan.step(step_id).map_or(step_id, |s| s.name.as_str());
                let label = format!("▶ Resume from “{step_name}”");
                if widgets::primary_button(ui, &label).clicked() {
                    match resume_input_overrides(ui, inputs_id, &plan.inputs, &binding.inputs) {
                        Ok(inputs) => {
                            ui.ctx().data_mut(|data| {
                                data.remove_temp::<String>(inputs_id.with("input_error"));
                            });
                            action = Some(PlanCardAction::Resume {
                                plan_id: plan.metadata.id.clone(),
                                run_id: binding.run_id.clone(),
                                from_step: Some(step_id.to_owned()),
                                inputs,
                            });
                        }
                        Err(error) => ui.ctx().data_mut(|data| {
                            data.insert_temp(inputs_id.with("input_error"), error);
                        }),
                    }
                }
            }
        });

        if widgets::ghost_button(ui, "Schedule…").clicked() {
            action = Some(PlanCardAction::Schedule {
                plan_id: plan.metadata.id.clone(),
            });
        }

        let edit_open_id = card_id.with("edit_open");
        let mut edit_open: bool = ui
            .ctx()
            .data_mut(|d| *d.get_temp_mut_or(edit_open_id, false));
        if widgets::ghost_button(ui, if edit_open { "Cancel edit" } else { "✎ Edit" }).clicked() {
            edit_open = !edit_open;
            ui.ctx().data_mut(|d| {
                d.insert_temp(edit_open_id, edit_open);
                if edit_open {
                    // Flag this open so the panel scrolls into view and
                    // grabs focus once it renders below.
                    d.insert_temp(card_id.with("edit_just_opened"), true);
                }
            });
        }

        let json_open_id = card_id.with("json_open");
        let mut json_open: bool = ui
            .ctx()
            .data_mut(|d| *d.get_temp_mut_or(json_open_id, false));
        if widgets::ghost_button(ui, if json_open { "Hide JSON" } else { "JSON" }).clicked() {
            json_open = !json_open;
            ui.ctx()
                .data_mut(|d| d.insert_temp(json_open_id, json_open));
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(format!("{} steps", plan.steps.len()))
                    .size(theme::FONT_SMALL)
                    .color(theme::text_faint()),
            );
        });
    });

    if let Some(error) = ui
        .ctx()
        .data(|data| data.get_temp::<String>(card_id.with("input_error")))
    {
        widgets::wrapped_label(
            ui,
            RichText::new(error)
                .size(theme::FONT_SMALL)
                .color(theme::err()),
        );
    }

    let edit_open: bool = ui
        .ctx()
        .data_mut(|d| *d.get_temp_mut_or(card_id.with("edit_open"), false));
    if edit_open {
        ui.add_space(6.0);
        let just_opened = ui
            .ctx()
            .data_mut(|d| d.remove_temp::<bool>(card_id.with("edit_just_opened")))
            .unwrap_or(false);
        let panel_response = ui.scope(|ui| edit_panel(ui, card_id));
        if just_opened {
            // Bring the panel into view (it may be below the fold in the
            // shared scroll area) and focus the input immediately so typing
            // works without an extra click.
            panel_response.response.scroll_to_me(Some(egui::Align::Min));
            ui.ctx()
                .memory_mut(|mem| mem.request_focus(card_id.with("edit_input")));
        }
        if let Some(instruction) = panel_response.inner {
            ui.ctx().data_mut(|d| {
                d.insert_temp(card_id.with("edit_open"), false);
                d.insert_temp(card_id.with("edit_draft"), String::new());
            });
            action = Some(PlanCardAction::Edit {
                plan_id: plan.metadata.id.clone(),
                instruction,
            });
        }
    }

    let json_open: bool = ui
        .ctx()
        .data_mut(|d| *d.get_temp_mut_or(card_id.with("json_open"), false));
    if json_open {
        ui.add_space(4.0);
        let json =
            crate::plan::to_json(plan).unwrap_or_else(|e| format!("failed to serialise: {e}"));
        egui::ScrollArea::vertical()
            .id_salt(card_id.with("json_scroll"))
            .max_height(260.0)
            .show(ui, |ui| {
                widgets::text_edit(
                    ui,
                    egui::TextEdit::multiline(&mut json.as_str())
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY),
                );
            });
    }

    action
}

/// Keep only values that differ from the failed execution. The executor owns
/// safety checks and rebasing; this helper merely gives it a precise override
/// map while still validating the repaired plan's current input contract.
fn resume_input_overrides(
    ui: &Ui,
    card_id: Id,
    inputs: &[PlanInput],
    original_values: &indexmap::IndexMap<String, serde_json::Value>,
) -> Result<indexmap::IndexMap<String, serde_json::Value>, String> {
    let submitted = input_values(ui, card_id, inputs)?;
    Ok(submitted
        .into_iter()
        .filter(|(name, value)| original_values.get(name) != Some(value))
        .collect())
}

fn edit_panel(ui: &mut Ui, card_id: Id) -> Option<String> {
    let draft_id = card_id.with("edit_draft");
    let mut draft = ui
        .ctx()
        .data_mut(|d| d.get_temp::<String>(draft_id).unwrap_or_default());
    let mut submitted = false;

    egui::Frame::new()
        .fill(theme::input())
        .stroke(egui::Stroke::new(1.0_f32, theme::divider()))
        .corner_radius(egui::CornerRadius::same(theme::RADIUS_WIDGET))
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            widgets::wrapped_label(
                ui,
                RichText::new("Describe how this plan should change. The LLM will save a new validated version.")
                    .size(theme::FONT_SMALL)
                    .color(theme::text_muted()),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let edit = widgets::text_edit(
                    ui,
                    egui::TextEdit::singleline(&mut draft)
                        .hint_text("e.g. ask for approval before writing the file…")
                        .desired_width((ui.available_width() - 96.0).max(120.0))
                        .id(card_id.with("edit_input")),
                );
                let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if widgets::primary_button(ui, "Apply edit").clicked() || enter {
                    submitted = true;
                }
            });
        });

    let instruction = draft.trim().to_owned();
    if submitted && !instruction.is_empty() {
        Some(instruction)
    } else {
        ui.ctx().data_mut(|d| d.insert_temp(draft_id, draft));
        None
    }
}

// ─── Step results ─────────────────────────────────────────────────────────────

const MAX_RESULT_CHARS: usize = 700;
const RESULT_INDENT: f32 = 24.0;
const EXPANDED_RESULT_MAX_HEIGHT: f32 = 360.0;

/// The visible result of a step: stdout when present, otherwise its named
/// outputs rendered as `name: value` lines.
fn step_result_text(run: &RunBinding, step_id: &str) -> Option<String> {
    if let Some(iterations) = run.iterations.get(step_id).filter(|runs| !runs.is_empty()) {
        return iterations.last().map(iteration_result_text);
    }

    let stdout = run
        .stdouts
        .get(step_id)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    match stdout {
        Some(text) => Some(text.to_owned()),
        None => run.outputs.get(step_id).and_then(|outputs| {
            let lines: Vec<String> = outputs
                .iter()
                .map(|(name, value)| format!("{name}: {}", compact_json(value)))
                .collect();
            (!lines.is_empty()).then(|| lines.join("\n"))
        }),
    }
}

fn iteration_result_text(run: &StepRunIteration) -> String {
    let result = run
        .stdout
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            let lines = run
                .outputs
                .iter()
                .map(|(name, value)| format!("{name}: {}", compact_json(value)))
                .collect::<Vec<_>>();
            (!lines.is_empty()).then(|| lines.join("\n"))
        })
        .or_else(|| run.error.clone())
        .unwrap_or_else(|| run.status.to_string());
    format!(
        "Iteration {} · {}\n{result}",
        run.iteration + 1,
        format_duration(run.duration_ms)
    )
}

fn fan_out_has_iteration_results(step: &PlanStep, run: &RunBinding) -> bool {
    let StepConfig::FanOut(config) = &step.config else {
        return false;
    };
    config.spawn_steps.iter().any(|step_id| {
        run.iterations
            .get(step_id)
            .is_some_and(|runs| !runs.is_empty())
    })
}

fn compact_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A quiet result block under a finished step.
fn step_result_block(ui: &mut Ui, id: Id, depth: usize, result: &str) {
    ui.horizontal_top(|ui| {
        ui.add_space(depth as f32 * INDENT_PER_LEVEL + RESULT_INDENT);
        let width = (ui.available_width() - 18.0).max(80.0);
        egui::Frame::new()
            .fill(theme::with_alpha(theme::ok(), 0.05))
            .stroke(egui::Stroke::new(
                1.0_f32,
                theme::with_alpha(theme::ok(), 0.18),
            ))
            .corner_radius(egui::CornerRadius::same(theme::RADIUS_WIDGET))
            .inner_margin(egui::Margin::symmetric(9, 6))
            .show(ui, |ui| {
                ui.set_width(width);
                expandable_text(ui, id, result, MAX_RESULT_CHARS, theme::text(), true);
            });
    });
}

fn expandable_text(
    ui: &mut Ui,
    id: Id,
    text: &str,
    collapsed_chars: usize,
    color: Color32,
    monospace: bool,
) {
    let is_long = text.chars().count() > collapsed_chars;
    let mut expanded = ui
        .ctx()
        .data_mut(|data| *data.get_temp_mut_or(id.with("expanded"), false));

    if is_long && expanded {
        // Keep the collapse action outside the result's own scroll area so it
        // remains visible no matter how far the content is scrolled.
        if widgets::ghost_button(ui, "Show less").clicked() {
            expanded = false;
            ui.ctx()
                .data_mut(|data| data.insert_temp(id.with("expanded"), expanded));
        }
        egui::ScrollArea::vertical()
            .id_salt(id.with("content_scroll"))
            .auto_shrink([false, false])
            .max_height(EXPANDED_RESULT_MAX_HEIGHT)
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width());
                wrapped_result_text(ui, text, color, monospace);
            });
    } else {
        let visible = if is_long {
            truncate(text, collapsed_chars)
        } else {
            text.to_owned()
        };
        wrapped_result_text(ui, &visible, color, monospace);
        if is_long && widgets::ghost_button(ui, "Show full content").clicked() {
            expanded = true;
            ui.ctx()
                .data_mut(|data| data.insert_temp(id.with("expanded"), expanded));
        }
    }
}

fn wrapped_result_text(ui: &mut Ui, text: &str, color: Color32, monospace: bool) {
    let rich_text = RichText::new(text).size(theme::FONT_SMALL).color(color);
    let rich_text = if monospace {
        rich_text.monospace()
    } else {
        rich_text
    };
    widgets::wrapped_label(ui, rich_text);
}

// ─── Step detail (debugging view) ─────────────────────────────────────────────

const DETAIL_BLOCK_MAX_HEIGHT: f32 = 180.0;

/// Expanded inspection panel for one step: the exact call with dependency
/// inputs resolved, and the full recorded result.
fn step_detail(
    ui: &mut Ui,
    row_id: Id,
    depth: usize,
    plan: &Plan,
    step: &PlanStep,
    run: Option<&RunBinding>,
) {
    ui.horizontal_top(|ui| {
        ui.add_space(depth as f32 * INDENT_PER_LEVEL + RESULT_INDENT);
        let panel_width = ui.available_width();
        egui::Frame::new()
            .fill(theme::input())
            .stroke(egui::Stroke::new(1.0_f32, theme::divider()))
            .corner_radius(egui::CornerRadius::same(theme::RADIUS_WIDGET))
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                // The frame inherits the horizontal row layout — stack the
                // sections vertically and pin the width so wide JSON cannot
                // blow up the card.
                ui.vertical(|ui| {
                    ui.set_width((panel_width - 22.0).max(120.0));

                    let config_json =
                        serde_json::to_value(&step.config).unwrap_or(serde_json::Value::Null);

                    if let Some(binding) = run {
                        let resolved = resolve_for_display(
                            &config_json,
                            &plan.config,
                            &binding.inputs,
                            &binding.outputs,
                        );
                        if resolved != config_json {
                            detail_block(
                                ui,
                                row_id.with("resolved"),
                                "Call (inputs resolved)",
                                &pretty(&resolved),
                            );
                        }
                    }
                    detail_block(
                        ui,
                        row_id.with("call"),
                        "Call (as compiled)",
                        &pretty(&config_json),
                    );

                    if let Some(binding) = run {
                        if let Some(outputs) = binding.outputs.get(&step.id) {
                            let value =
                                serde_json::to_value(outputs).unwrap_or(serde_json::Value::Null);
                            detail_block(ui, row_id.with("outputs"), "Outputs", &pretty(&value));
                        }
                        if let Some(stdout) = binding.stdouts.get(&step.id) {
                            detail_block(ui, row_id.with("stdout"), "Stdout", stdout);
                        }
                        if let Some(stderr) = binding.stderrs.get(&step.id) {
                            detail_block(ui, row_id.with("stderr"), "Stderr", stderr);
                        }
                        if let Some(iterations) = binding.iterations.get(&step.id) {
                            let value =
                                serde_json::to_value(iterations).unwrap_or(serde_json::Value::Null);
                            detail_block(
                                ui,
                                row_id.with("iterations"),
                                "Fan-out iterations",
                                &pretty(&value),
                            );
                        }
                        if let Some(error) = binding.errors.get(&step.id) {
                            detail_block(ui, row_id.with("error"), "Error", error);
                        }
                    }
                });
            });
    });
}

/// One labelled, selectable, scrollable text block.
fn detail_block(ui: &mut Ui, id: Id, label: &str, mut text: &str) {
    widgets::section_label(ui, label);
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(DETAIL_BLOCK_MAX_HEIGHT)
        .show(ui, |ui| {
            // `&mut &str` renders a selectable, read-only text area.
            let width = ui.available_width();
            widgets::text_edit(
                ui,
                egui::TextEdit::multiline(&mut text)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(width),
            );
        });
    ui.add_space(6.0);
}

fn pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// Display-side placeholder resolution against the recorded run outputs.
/// `${step.*}`, `${input.*}`, and `${conf.*}` are substituted; `${env.*}` and `${item.*}`
/// are deliberately left as-is (env values may be secrets, item values are
/// per-iteration).
fn resolve_for_display(
    value: &serde_json::Value,
    plan_config: &indexmap::IndexMap<String, serde_json::Value>,
    inputs: &indexmap::IndexMap<String, serde_json::Value>,
    outputs: &HashMap<String, indexmap::IndexMap<String, serde_json::Value>>,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            serde_json::Value::String(resolve_str_for_display(s, plan_config, inputs, outputs))
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        resolve_for_display(v, plan_config, inputs, outputs),
                    )
                })
                .collect(),
        ),
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|v| resolve_for_display(v, plan_config, inputs, outputs))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn resolve_str_for_display(
    s: &str,
    plan_config: &indexmap::IndexMap<String, serde_json::Value>,
    inputs: &indexmap::IndexMap<String, serde_json::Value>,
    outputs: &HashMap<String, indexmap::IndexMap<String, serde_json::Value>>,
) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\$\{([^}]+)\}").expect("placeholder regex is always valid")
    });

    re.replace_all(s, |caps: &regex::Captures| {
        let key = &caps[1];
        let looked_up = key
            .strip_prefix("input.")
            .and_then(|name| inputs.get(name))
            .or_else(|| key.strip_prefix("conf.").and_then(|k| plan_config.get(k)))
            .or_else(|| {
                key.strip_prefix("step.").and_then(|rest| {
                    let (step_id, output) = rest.split_once('.')?;
                    outputs.get(step_id)?.get(output)
                })
            });
        match looked_up {
            Some(serde_json::Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
            None => caps[0].to_owned(),
        }
    })
    .into_owned()
}

// ─── Step styling ─────────────────────────────────────────────────────────────

fn step_dot(step: &PlanStep, status: Option<&StepRunStatus>) -> (Color32, bool) {
    match status {
        None => (
            theme::with_alpha(step_type_style(step.step_type()).1, 0.6),
            false,
        ),
        Some(StepRunStatus::Pending) => (theme::text_faint(), false),
        Some(StepRunStatus::Running) => (theme::active(), true),
        Some(StepRunStatus::WaitingForHuman) => (theme::warn(), true),
        Some(StepRunStatus::Succeeded) => (theme::ok(), false),
        Some(StepRunStatus::Failed) => (theme::err(), false),
        Some(StepRunStatus::Skipped | StepRunStatus::Cancelled) => (theme::text_faint(), false),
        // A status this build does not recognize (e.g. one added by a newer
        // release) — neutral, no pulse.
        Some(StepRunStatus::Unknown) => (theme::text_muted(), false),
    }
}

pub fn step_type_style(step_type: StepType) -> (&'static str, Color32) {
    match step_type {
        StepType::ToolCall => ("tool", theme::step_tool()),
        StepType::CodeCall => ("code", theme::step_code()),
        StepType::HumanInteraction => ("human", theme::step_human()),
        StepType::FanOut => ("fan-out", theme::step_fan()),
        StepType::FanIn => ("fan-in", theme::step_fan()),
        StepType::PromptCall => ("prompt", theme::step_prompt()),
        StepType::Condition => ("condition", theme::step_other()),
        StepType::AgentCall => ("agent", theme::err()),
    }
}

fn step_hover_text(step: &PlanStep) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(description) = &step.description {
        parts.push(description.clone());
    }
    match &step.config {
        StepConfig::ToolCall(c) => parts.push(format!("tool: {}", c.tool)),
        StepConfig::CodeCall(c) => parts.push(format!("language: {}", c.language)),
        StepConfig::PromptCall(c) => parts.push(format!("model: {}", c.model)),
        StepConfig::AgentCall(c) => {
            parts.push(format!("objective: {}", c.objective));
            parts.push(format!("working directory: {}", c.working_dir));
        }
        _ => {}
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

// ─── Layout helpers ───────────────────────────────────────────────────────────

/// Depth of each step = longest dependency chain above it. Cycles are cut off
/// by the iteration cap (the validator rejects cyclic plans anyway).
///
/// FAN_OUT-owned steps with no real `depends_on` (the head of a per-item
/// body) are treated as depending on their owning FAN_OUT step, so they are
/// indented one level deeper than it instead of floating at depth 0.
fn topo_depths<'a>(plan: &'a Plan, owners: &HashMap<&'a str, &'a str>) -> HashMap<&'a str, usize> {
    let mut depths: HashMap<&str, usize> = plan.steps.iter().map(|s| (s.id.as_str(), 0)).collect();
    for _ in 0..plan.steps.len() {
        let mut changed = false;
        for step in &plan.steps {
            let dep_max = effective_dependency_ids(step, owners)
                .into_iter()
                .filter_map(|d| depths.get(d).copied())
                .max()
                .map(|m| m + 1)
                .unwrap_or(0);
            let entry = depths.get_mut(step.id.as_str()).expect("seeded above");
            if dep_max > *entry {
                *entry = dep_max;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    depths
}

/// Map each FAN_OUT-owned spawn step id to the id of its owning FAN_OUT step.
fn fan_out_owner_of(plan: &Plan) -> HashMap<&str, &str> {
    let mut owners = HashMap::new();
    for step in &plan.steps {
        if let StepConfig::FanOut(cfg) = &step.config {
            for spawn_id in &cfg.spawn_steps {
                owners.insert(spawn_id.as_str(), step.id.as_str());
            }
        }
    }
    owners
}

/// Dependency ids to use for depth/connector layout. A FAN_OUT-owned step
/// with no real `depends_on` (the head of its per-item body) is treated as
/// depending on its owner, since it has no other visible entry point into
/// the graph.
fn effective_dependency_ids<'a>(
    step: &'a PlanStep,
    owners: &HashMap<&str, &'a str>,
) -> Vec<&'a str> {
    if step.depends_on.is_empty()
        && let Some(&owner) = owners.get(step.id.as_str())
    {
        return vec![owner];
    }
    step.depends_on.iter().map(String::as_str).collect()
}

/// Label shown next to a step's name. Falls back to naming the owning
/// FAN_OUT step when a step has no real dependencies of its own but runs
/// inside a fan-out body (otherwise it would show no context at all).
fn step_after_label(step: &PlanStep, plan: &Plan, owners: &HashMap<&str, &str>) -> Option<String> {
    if !step.depends_on.is_empty() {
        return Some(format!("after {}", step.depends_on.join(", ")));
    }
    let owner_id = *owners.get(step.id.as_str())?;
    let owner_name = plan.step(owner_id).map_or(owner_id, |s| s.name.as_str());
    Some(format!("owned by {owner_name}"))
}

/// Step render order: FAN_OUT-owned steps are moved out of their original
/// array position and nested immediately after their owning FAN_OUT step (in
/// `spawn_steps` order), so the card visually shows them as part of the
/// fan-out body instead of as unrelated top-level steps.
fn display_order<'a>(plan: &'a Plan, owners: &HashMap<&str, &str>) -> Vec<&'a PlanStep> {
    fn append_with_children<'a>(
        plan: &'a Plan,
        step: &'a PlanStep,
        seen: &mut HashSet<&'a str>,
        order: &mut Vec<&'a PlanStep>,
    ) {
        if !seen.insert(step.id.as_str()) {
            return;
        }
        order.push(step);
        if let StepConfig::FanOut(config) = &step.config {
            for child_id in &config.spawn_steps {
                if let Some(child) = plan.step(child_id) {
                    append_with_children(plan, child, seen, order);
                }
            }
        }
    }

    let mut order = Vec::with_capacity(plan.steps.len());
    let mut seen = HashSet::with_capacity(plan.steps.len());
    for step in &plan.steps {
        if !owners.contains_key(step.id.as_str()) {
            append_with_children(plan, step, &mut seen, &mut order);
        }
    }
    order
}

pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

pub fn format_duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1_000.0)
    } else {
        format!("{:.1} min", ms as f64 / 60_000.0)
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        let cut: String = text.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{FanOutConfig, PlanMetadata, ToolCallConfig};

    fn input(name: &str, kind: InputKind) -> PlanInput {
        PlanInput {
            name: name.to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: true,
            default: None,
            input_kind: kind,
        }
    }

    #[test]
    fn path_input_kinds_select_the_matching_native_picker() {
        assert_eq!(
            input_path_picker(&input("source", InputKind::FilePath)),
            Some(PathPicker::OpenFile)
        );
        assert_eq!(
            input_path_picker(&input("output", InputKind::OutputFilePath)),
            Some(PathPicker::SaveFile)
        );
        assert_eq!(
            input_path_picker(&input("directory", InputKind::DirectoryPath)),
            Some(PathPicker::Folder)
        );
        assert_eq!(input_path_picker(&input("label", InputKind::Value)), None);
    }

    #[test]
    fn legacy_root_directory_selects_a_folder_picker() {
        assert_eq!(
            input_path_picker(&input(
                crate::plan::types::ROOT_DIRECTORY_INPUT,
                InputKind::Value,
            )),
            Some(PathPicker::Folder)
        );
    }

    fn step(id: &str, deps: &[&str]) -> PlanStep {
        PlanStep {
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            config: StepConfig::ToolCall(ToolCallConfig {
                tool: "echo".to_owned(),
                arguments: Default::default(),
            }),
            depends_on: deps.iter().map(|s| (*s).to_owned()).collect(),
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        }
    }

    fn fan_out_step(id: &str, deps: &[&str], over: &str, spawn_steps: &[&str]) -> PlanStep {
        let mut s = step(id, deps);
        s.config = StepConfig::FanOut(FanOutConfig {
            over: over.to_owned(),
            item_var: "item".to_owned(),
            spawn_steps: spawn_steps.iter().map(|s| (*s).to_owned()).collect(),
            until: None,
        });
        s
    }

    fn plan(steps: Vec<PlanStep>) -> Plan {
        Plan {
            metadata: PlanMetadata::new(None),
            name: "t".to_owned(),
            description: None,
            inputs: vec![],
            config: Default::default(),
            steps,
            outputs: vec![],
        }
    }

    #[test]
    fn topo_depths_follow_longest_chain() {
        let p = plan(vec![
            step("a", &[]),
            step("b", &["a"]),
            step("c", &["a", "b"]),
            step("d", &[]),
        ]);
        let owners = fan_out_owner_of(&p);
        let depths = topo_depths(&p, &owners);
        assert_eq!(depths["a"], 0);
        assert_eq!(depths["b"], 1);
        assert_eq!(depths["c"], 2);
        assert_eq!(depths["d"], 0);
    }

    #[test]
    fn running_and_human_wait_steps_use_distinct_status_colors() {
        let plan_step = step("a", &[]);
        assert_eq!(
            step_dot(&plan_step, Some(&StepRunStatus::Running)),
            (theme::active(), true)
        );
        assert_eq!(
            step_dot(&plan_step, Some(&StepRunStatus::WaitingForHuman)),
            (theme::warn(), true)
        );
    }

    #[test]
    fn fan_out_owner_of_maps_spawn_steps_to_owner() {
        let p = plan(vec![
            step("extract", &[]),
            fan_out_step(
                "process_posts",
                &["extract"],
                "extract.urls",
                &["fetch_post", "summarize_post"],
            ),
            step("fetch_post", &[]),
            step("summarize_post", &["fetch_post"]),
        ]);
        let owners = fan_out_owner_of(&p);
        assert_eq!(owners.get("fetch_post"), Some(&"process_posts"));
        assert_eq!(owners.get("summarize_post"), Some(&"process_posts"));
        assert_eq!(owners.get("extract"), None);
    }

    #[test]
    fn owned_head_of_chain_step_is_indented_one_level_deeper_than_owner() {
        // fetch_post has no real depends_on but is owned by process_posts, so
        // it must be treated as depending on it for depth purposes instead of
        // floating at depth 0 alongside the owner's own dependencies.
        let p = plan(vec![
            step("extract", &[]),
            fan_out_step(
                "process_posts",
                &["extract"],
                "extract.urls",
                &["fetch_post", "summarize_post"],
            ),
            step("fetch_post", &[]),
            step("summarize_post", &["fetch_post"]),
        ]);
        let owners = fan_out_owner_of(&p);
        let depths = topo_depths(&p, &owners);
        assert_eq!(depths["process_posts"], 1);
        assert_eq!(depths["fetch_post"], 2);
        assert_eq!(depths["summarize_post"], 3);
    }

    #[test]
    fn display_order_nests_owned_steps_directly_after_their_owner() {
        let p = plan(vec![
            step("extract", &[]),
            fan_out_step(
                "process_posts",
                &["extract"],
                "extract.urls",
                &["fetch_post", "summarize_post"],
            ),
            step("fetch_post", &[]),
            step("summarize_post", &["fetch_post"]),
            step("final_summary", &["process_posts"]),
        ]);
        let owners = fan_out_owner_of(&p);
        let order = display_order(&p, &owners);
        let ids: Vec<&str> = order.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "extract",
                "process_posts",
                "fetch_post",
                "summarize_post",
                "final_summary",
            ]
        );
    }

    #[test]
    fn step_after_label_names_owner_when_no_real_dependency() {
        let p = plan(vec![
            fan_out_step("process_posts", &[], "extract.urls", &["fetch_post"]),
            step("fetch_post", &[]),
        ]);
        let owners = fan_out_owner_of(&p);
        let fetch_post = p.step("fetch_post").unwrap();
        assert_eq!(
            step_after_label(fetch_post, &p, &owners),
            Some("owned by process_posts".to_owned())
        );
    }

    #[test]
    fn step_after_label_prefers_real_dependencies_over_ownership() {
        let p = plan(vec![
            fan_out_step(
                "process_posts",
                &[],
                "extract.urls",
                &["fetch_post", "summarize_post"],
            ),
            step("fetch_post", &[]),
            step("summarize_post", &["fetch_post"]),
        ]);
        let owners = fan_out_owner_of(&p);
        let summarize_post = p.step("summarize_post").unwrap();
        assert_eq!(
            step_after_label(summarize_post, &p, &owners),
            Some("after fetch_post".to_owned())
        );
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(format_duration(250), "250 ms");
        assert_eq!(format_duration(1_500), "1.5 s");
        assert_eq!(format_duration(90_000), "1.5 min");
    }

    #[test]
    fn fan_out_items_render_only_the_latest_iteration_result() {
        let now = chrono::Utc::now();
        let iteration = StepRunIteration {
            iteration: 1,
            status: StepRunStatus::Succeeded,
            started_at: now,
            finished_at: now,
            duration_ms: 1_250,
            outputs: [("summary".to_owned(), serde_json::json!("second post"))]
                .into_iter()
                .collect(),
            stdout: None,
            stderr: None,
            error: None,
            token_usage: None,
        };
        let first_iteration = StepRunIteration {
            iteration: 0,
            status: StepRunStatus::Succeeded,
            started_at: now,
            finished_at: now,
            duration_ms: 250,
            outputs: [("summary".to_owned(), serde_json::json!("first post"))]
                .into_iter()
                .collect(),
            stdout: None,
            stderr: None,
            error: None,
            token_usage: None,
        };
        let mut binding = RunBinding::default();
        binding.iterations.insert(
            "summarize_post".to_owned(),
            vec![first_iteration, iteration],
        );
        let fan_out = fan_out_step("process_posts", &[], "extract.urls", &["summarize_post"]);

        assert_eq!(
            step_result_text(&binding, "summarize_post"),
            Some("Iteration 2 · 1.2 s\nsummary: second post".to_owned())
        );
        assert!(fan_out_has_iteration_results(&fan_out, &binding));
    }

    #[test]
    fn display_resolution_substitutes_steps_and_conf_but_not_env() {
        let plan_config: indexmap::IndexMap<String, serde_json::Value> =
            [("region".to_owned(), serde_json::json!("eu-west-1"))]
                .into_iter()
                .collect();
        let inputs: indexmap::IndexMap<String, serde_json::Value> =
            [("topic".to_owned(), serde_json::json!("BTC"))]
                .into_iter()
                .collect();
        let outputs: HashMap<String, indexmap::IndexMap<String, serde_json::Value>> = [(
            "fetch".to_owned(),
            [("price".to_owned(), serde_json::json!(42_000))]
                .into_iter()
                .collect(),
        )]
        .into_iter()
        .collect();

        let resolved = resolve_str_for_display(
            "${input.topic}: ${step.fetch.price} in ${conf.region}, key ${env.SECRET}, missing ${step.x.y}",
            &plan_config,
            &inputs,
            &outputs,
        );
        assert_eq!(
            resolved,
            "BTC: 42000 in eu-west-1, key ${env.SECRET}, missing ${step.x.y}"
        );
    }
}
