//! Schedules view — create and manage cron schedules for plans.
//!
//! The schedule expression accepts plain crontab syntax or natural language
//! ("every morning at 8"); the engine asks the compiler to convert the
//! latter and confirms the interpretation in chat.

use egui::{Align, Id, Layout, RichText, Ui};

use crate::app::engine::{EngineCommand, EngineHandle, PlanListItem, ScheduleItem};
use crate::app::views::plan_card;
use crate::app::{theme, widgets};

const EXPRESSION_HINT: &str = "*/15 * * * *   or   “every morning at 8”";
const CREATE_ACTION_WIDTH: f32 = 84.0;
const CREATE_ACTION_HEIGHT: f32 = 30.0;

#[derive(Default)]
pub struct SchedulesState {
    /// Plan id preselected in the create form.
    pub selected_plan: Option<String>,
    pub expression: String,
    pub input_error: Option<String>,
    pub creating: bool,
}

/// Namespaces the typed input drafts (see `plan_card::input_fields`) for the
/// schedule form, distinct from any plan card's own `card_id`. Including the
/// plan id means switching the selected plan lands on a fresh id, so a
/// previously selected plan's drafts never leak into the newly selected
/// plan's fields.
fn schedule_inputs_id(plan_id: &str) -> Id {
    Id::new("schedule_form_inputs").with(plan_id)
}

/// Keep the persisted plan id as the picker value while presenting the
/// human-readable plan name to the user.
fn plan_picker_label(plan: &PlanListItem) -> &str {
    &plan.name
}

/// Preselect a plan and reset the form — used by the "Schedule" button on
/// plan cards.
impl SchedulesState {
    pub fn start_for_plan(&mut self, plan_id: &str) {
        self.selected_plan = Some(plan_id.to_owned());
        self.expression.clear();
        self.input_error = None;
        self.creating = false;
    }
}

pub fn show(
    ui: &mut Ui,
    state: &mut SchedulesState,
    schedules: &[ScheduleItem],
    plans: &[PlanListItem],
    engine: &EngineHandle,
) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(8.0);
            ui.label(theme::title("Schedules", theme::FONT_HEADING));
            ui.add_space(12.0);

            create_form(ui, state, plans, engine);
            ui.add_space(16.0);

            widgets::section_label(ui, "Active");
            ui.add_space(4.0);
            if schedules.is_empty() {
                ui.label(
                    RichText::new("Nothing scheduled yet — pick a plan above.")
                        .color(theme::text_muted()),
                );
            }
            for item in schedules {
                schedule_row(ui, item, engine);
            }
            ui.add_space(12.0);
        });
}

fn create_form(
    ui: &mut Ui,
    state: &mut SchedulesState,
    plans: &[PlanListItem],
    engine: &EngineHandle,
) {
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        widgets::section_label(ui, "New schedule");
        ui.add_space(6.0);

        if plans.is_empty() {
            ui.label(
                RichText::new("Nothing compiled yet — go to Chat and describe the work.")
                    .color(theme::text_muted()),
            );
            return;
        }

        // A usable form should never sit in an implicit "no plan" state.
        // This also covers the normal async load: the first plan is
        // selected as soon as the plan list arrives.
        let selection_points_at_known_plan = state
            .selected_plan
            .as_ref()
            .is_some_and(|id| plans.iter().any(|plan| &plan.id == id));
        if !selection_points_at_known_plan {
            ensure_selected_plan(state, &plans.iter().collect::<Vec<_>>());
        }

        let selected_plan = state
            .selected_plan
            .as_ref()
            .and_then(|id| plans.iter().find(|p| &p.id == id));
        let selected_name = match selected_plan {
            Some(plan) => plan_picker_label(plan),
            None => "choose a plan…",
        };

        ui.horizontal(|ui| {
            ui.label(RichText::new("Plan").color(theme::text_muted()));
            egui::ComboBox::from_id_salt("schedule_plan_picker")
                .selected_text(selected_name)
                .width(320.0)
                .show_ui(ui, |ui| {
                    for plan in plans {
                        let checked = state.selected_plan.as_deref() == Some(plan.id.as_str());
                        if ui
                            .selectable_label(checked, plan_picker_label(plan))
                            .clicked()
                        {
                            state.selected_plan = Some(plan.id.clone());
                        }
                    }
                });
        });
        ui.add_space(4.0);

        let selected_inputs = state
            .selected_plan
            .as_ref()
            .and_then(|id| plans.iter().find(|plan| &plan.id == id))
            .map(|plan| plan.inputs.as_slice())
            .unwrap_or_default();
        if !selected_inputs.is_empty() {
            // Same one-field-per-input UI as the plan-trigger ("Run") flow,
            // namespaced by plan id so its drafts never collide with a plan
            // card's own drafts for the same plan.
            let inputs_id = schedule_inputs_id(
                state
                    .selected_plan
                    .as_deref()
                    .expect("selected_inputs non-empty implies a selected plan"),
            );
            plan_card::input_fields(ui, inputs_id, selected_inputs);
            if let Some(error) = &state.input_error {
                widgets::wrapped_label(
                    ui,
                    RichText::new(error)
                        .size(theme::FONT_SMALL)
                        .color(theme::err()),
                );
            }
            ui.add_space(4.0);
        }

        ui.horizontal(|ui| {
            ui.label(RichText::new("When").color(theme::text_muted()));
            let expression_width = (ui.available_width() - CREATE_ACTION_WIDTH).max(120.0);
            widgets::text_edit(
                ui,
                egui::TextEdit::singleline(&mut state.expression)
                    .hint_text(EXPRESSION_HINT)
                    .desired_width(expression_width),
            );
            let ready =
                selected_plan.is_some() && !state.expression.trim().is_empty() && !state.creating;
            let create_clicked = ui
                .allocate_ui_with_layout(
                    egui::vec2(CREATE_ACTION_WIDTH, CREATE_ACTION_HEIGHT),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        if state.creating {
                            ui.spinner();
                            false
                        } else {
                            ui.add_enabled_ui(ready, |ui| {
                                widgets::primary_button(ui, "Create").clicked()
                            })
                            .inner
                        }
                    },
                )
                .inner;
            if create_clicked && let Some(plan_id) = &state.selected_plan {
                let inputs_id = schedule_inputs_id(plan_id);
                match plan_card::input_values(ui, inputs_id, selected_inputs) {
                    Ok(inputs) => {
                        engine.send(EngineCommand::SaveSchedule {
                            plan_ref: plan_id.clone(),
                            expression: state.expression.trim().to_owned(),
                            inputs,
                        });
                        state.input_error = None;
                        state.creating = true;
                    }
                    Err(error) => {
                        state.input_error = Some(error);
                    }
                }
            }
        });
        widgets::wrapped_label(
            ui,
            RichText::new(
                "Crontab syntax or plain language — the compiler converts phrases like \
                 “weekdays at 7:30” and confirms the interpretation in chat. Local time.",
            )
            .size(theme::FONT_SMALL)
            .color(theme::text_faint()),
        );
    });
}

fn ensure_selected_plan(state: &mut SchedulesState, plans: &[&PlanListItem]) {
    let selection_is_valid = state
        .selected_plan
        .as_ref()
        .is_some_and(|id| plans.iter().any(|plan| &plan.id == id));
    if !selection_is_valid {
        state.selected_plan = plans.first().map(|plan| plan.id.clone());
    }
}

fn schedule_row(ui: &mut Ui, item: &ScheduleItem, engine: &EngineHandle) {
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            widgets::status_dot(
                ui,
                if item.enabled {
                    theme::ok()
                } else {
                    theme::text_faint()
                },
                item.enabled,
            );
            widgets::truncated_label(ui, RichText::new(&item.plan_name).strong(), 300.0);
            if !item.inputs.is_empty() {
                widgets::badge(
                    ui,
                    &format!("{} inputs", item.inputs.len()),
                    theme::text_muted(),
                );
            }
            ui.label(
                RichText::new(&item.cron)
                    .monospace()
                    .size(theme::FONT_SMALL)
                    .color(theme::accent()),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::ghost_button(ui, "✕").clicked() {
                    engine.send(EngineCommand::DeleteSchedule {
                        id: item.id.clone(),
                    });
                }
                let toggle = if item.enabled { "Pause" } else { "Resume" };
                if widgets::ghost_button(ui, toggle).clicked() {
                    engine.send(EngineCommand::ToggleSchedule {
                        id: item.id.clone(),
                    });
                }
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
    });
    ui.add_space(4.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::PlanStatus;

    fn plan(id: &str) -> PlanListItem {
        PlanListItem {
            id: id.to_owned(),
            name: format!("Plan {id}"),
            version: 1,
            intent: None,
            inputs: Vec::new(),
            updated_at: chrono::Utc::now(),
            status: PlanStatus::Published,
        }
    }

    #[test]
    fn first_available_plan_is_selected_by_default() {
        let mut state = SchedulesState::default();
        let (first, second) = (plan("first"), plan("second"));
        ensure_selected_plan(&mut state, &[&first, &second]);
        assert_eq!(state.selected_plan.as_deref(), Some("first"));
    }

    #[test]
    fn an_existing_valid_selection_is_preserved() {
        let mut state = SchedulesState {
            selected_plan: Some("second".to_owned()),
            ..Default::default()
        };
        let (first, second) = (plan("first"), plan("second"));
        ensure_selected_plan(&mut state, &[&first, &second]);
        assert_eq!(state.selected_plan.as_deref(), Some("second"));
    }

    #[test]
    fn plan_picker_uses_the_name_instead_of_the_id() {
        let mut plan = plan("internal-plan-id");
        plan.name = "Monthly report".to_owned();

        assert_eq!(plan_picker_label(&plan), "Monthly report");
        assert_ne!(plan_picker_label(&plan), plan.id);
    }
}
