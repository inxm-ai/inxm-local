//! Settings view — choose and persist the LLM connection used for compiling
//! and executing prompt steps.

use egui::{RichText, Ui};

use crate::app::engine::{
    self, AppSettings, BackendChoice, EngineCommand, EngineHandle, ThemePreference,
};
use crate::app::mcp_server::ServerStatus;
use crate::app::{theme, widgets};
use crate::llm::CodexSandboxMode;

const RELEASES_URL: &str = "https://github.com/inxm-ai/matthias-hackathon-inxm/releases";
const MODEL_HINT: &str = "empty = connection default";
const KEY_HINT: &str = "empty = use ANTHROPIC_API_KEY / OPENAI_API_KEY";
const AUTO_MODE_DESCRIPTION: &str = "Skips the design-approval step when creating plans: a \
     solution design compiles into a plan as soon as it is ready, instead of waiting for you to \
     approve it.";
const THEME_CHOICES: &[(ThemePreference, &str)] = &[
    (ThemePreference::System, "System"),
    (ThemePreference::Light, "Light"),
    (ThemePreference::Dark, "Dark"),
];
const BACKEND_CHOICES: &[(BackendChoice, &str, &str)] = &[
    (
        BackendChoice::Auto,
        "Auto",
        "Pick from available environment keys (Claude first)",
    ),
    (
        BackendChoice::Claude,
        "Claude API",
        "Anthropic Messages API with an API key",
    ),
    (
        BackendChoice::OpenAi,
        "OpenAI API",
        "OpenAI Chat Completions with an API key",
    ),
    (
        BackendChoice::Codex,
        "OpenAI account",
        "Use your existing Codex CLI login",
    ),
    (
        BackendChoice::ClaudeCode,
        "Claude account",
        "Use your existing Claude Code login",
    ),
    (
        BackendChoice::GoogleVertex,
        "Google Vertex AI",
        "Gemini on Vertex AI via gcloud identity (CLI login or GCP workload)",
    ),
    (
        BackendChoice::OpenAiCompatible,
        "Custom OpenAI URL",
        "Local or hosted OpenAI-compatible endpoint",
    ),
    (
        BackendChoice::AnthropicCompatible,
        "Custom Anthropic URL",
        "Local or hosted Anthropic-compatible endpoint",
    ),
    (
        BackendChoice::CustomCli,
        "Custom CLI",
        "Run any CLI agent (e.g. cline, opencode) via your own command",
    ),
];
const CODEX_SANDBOX_MODE_CHOICES: &[(CodexSandboxMode, &str, &str)] = &[
    (
        CodexSandboxMode::Auto,
        "Auto",
        "Try Codex's own sandbox; if it fails to start, fall back once to unsandboxed and note it in the run's audit log",
    ),
    (
        CodexSandboxMode::Strict,
        "Strict",
        "Never fall back — a sandbox failure stops the step with guidance for this OS",
    ),
    (
        CodexSandboxMode::Unsandboxed,
        "Unsandboxed",
        "Skip Codex's own sandbox entirely; rely on this app's workspace and process containment",
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsAction {
    RestartOnboarding,
}

#[derive(Default)]
pub struct SettingsState {
    pub draft: AppSettings,
    /// Result of the last "Test sandbox" probe, if one has been run this session.
    pub codex_sandbox_test: Option<Result<(), String>>,
}

/// The backend picker (label + one-line description per [`BackendChoice`]),
/// shared between the full Settings page and the first-run assistant so the
/// list and its behavior exist in exactly one place. Returns whether the
/// selection changed.
pub fn render_backend_choices(ui: &mut Ui, settings: &mut AppSettings) -> bool {
    let mut changed = false;
    for (choice, label, description) in BACKEND_CHOICES {
        let selected = settings.backend == *choice;
        ui.horizontal(|ui| {
            if ui.selectable_label(selected, *label).clicked() {
                settings.select_backend(*choice);
                changed = true;
            }
            widgets::truncated_label(
                ui,
                RichText::new(*description)
                    .size(theme::FONT_SMALL)
                    .color(theme::text_faint()),
                0.0,
            );
        });
    }
    changed
}

pub fn show(
    ui: &mut Ui,
    state: &mut SettingsState,
    environment: &str,
    mcp_status: &ServerStatus,
    engine: &EngineHandle,
    update_available: Option<&(String, String)>,
) -> Option<SettingsAction> {
    let mut action = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(8.0);
            ui.label(theme::title("Settings", theme::FONT_HEADING));
            ui.add_space(12.0);

            theme::card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section_label(ui, "Theme");
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    for (choice, label) in THEME_CHOICES {
                        let selected = state.draft.theme_preference == *choice;
                        if ui.selectable_label(selected, *label).clicked() && !selected {
                            state.draft.theme_preference = *choice;
                            let dark_mode =
                                engine::resolve_dark_mode(*choice, ui.ctx().system_theme());
                            theme::apply(ui.ctx(), dark_mode);
                            engine.send(EngineCommand::SaveSettings {
                                settings: Box::new(state.draft.clone()),
                            });
                        }
                    }
                });
            });

            ui.add_space(12.0);
            theme::card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section_label(ui, "Compiler");
                ui.add_space(2.0);
                widgets::wrapped_label(
                    ui,
                    RichText::new(if state.draft.experimental_agent_calls {
                        "The LLM that turns your intent into a plan. Ordinary steps stay \
                         deterministic; opted-in AGENT_CALL steps can run commands and modify \
                         their workspace."
                    } else {
                        "The LLM that turns your intent into a plan. Execution stays fully \
                         deterministic."
                    })
                    .size(theme::FONT_SMALL)
                    .color(theme::text_muted()),
                );
                ui.add_space(10.0);

                if render_backend_choices(ui, &mut state.draft) {
                    engine.send(EngineCommand::SaveSettings {
                        settings: Box::new(state.draft.clone()),
                    });
                }

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(8.0);

                ui.label(RichText::new("Model").color(theme::text_muted()));
                if widgets::text_edit(
                    ui,
                    egui::TextEdit::singleline(&mut state.draft.model)
                        .hint_text(MODEL_HINT)
                        .desired_width(ui.available_width()),
                )
                .changed()
                {
                    engine.send(EngineCommand::SaveSettings {
                        settings: Box::new(state.draft.clone()),
                    });
                }
                ui.add_space(8.0);

                if matches!(
                    state.draft.backend,
                    BackendChoice::GoogleVertex
                        | BackendChoice::OpenAiCompatible
                        | BackendChoice::AnthropicCompatible
                ) {
                    ui.label(RichText::new("Base URL").color(theme::text_muted()));
                    let hint = if state.draft.backend == BackendChoice::GoogleVertex {
                        "https://<region>-aiplatform.googleapis.com/v1/projects/<project>/locations/<region>/publishers/google/models"
                    } else {
                        "e.g. http://localhost:11434/v1"
                    };
                    if widgets::text_edit(
                        ui,
                        egui::TextEdit::singleline(&mut state.draft.api_base)
                            .hint_text(hint)
                            .desired_width(ui.available_width()),
                    )
                    .changed()
                    {
                        engine.send(EngineCommand::SaveSettings {
                            settings: Box::new(state.draft.clone()),
                        });
                    }
                    ui.add_space(8.0);
                }

                if state.draft.backend == BackendChoice::GoogleVertex {
                    ui.label(RichText::new("gcloud executable").color(theme::text_muted()));
                    if widgets::text_edit(
                        ui,
                        egui::TextEdit::singleline(&mut state.draft.executable)
                            .hint_text("empty = gcloud from PATH")
                            .desired_width(ui.available_width()),
                    )
                    .changed()
                    {
                        engine.send(EngineCommand::SaveSettings {
                            settings: Box::new(state.draft.clone()),
                        });
                    }
                    widgets::wrapped_label(
                        ui,
                        RichText::new(
                            "Authenticates with your Google Cloud identity: `gcloud auth login` \
                             locally, or the workload's service account when running on GCP. \
                             No API key is stored.",
                        )
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                    );
                    ui.add_space(8.0);
                }

                if matches!(
                    state.draft.backend,
                    BackendChoice::Codex | BackendChoice::ClaudeCode
                ) {
                    ui.label(RichText::new("CLI executable").color(theme::text_muted()));
                    if widgets::text_edit(
                        ui,
                        egui::TextEdit::singleline(&mut state.draft.executable)
                            .hint_text("empty = auto-detect from PATH and common install locations")
                            .desired_width(ui.available_width()),
                    )
                    .changed()
                    {
                        engine.send(EngineCommand::SaveSettings {
                            settings: Box::new(state.draft.clone()),
                        });
                    }
                    widgets::wrapped_label(
                        ui,
                        RichText::new("Sign in with the selected CLI before using this connection.")
                            .size(theme::FONT_SMALL)
                            .color(theme::text_faint()),
                    );
                    ui.add_space(8.0);

                    if state.draft.backend == BackendChoice::Codex {
                        ui.separator();
                        ui.add_space(8.0);
                        ui.label(RichText::new("Sandbox mode").color(theme::text_muted()));
                        ui.add_space(2.0);
                        let mut mode_changed = false;
                        for (mode, label, description) in CODEX_SANDBOX_MODE_CHOICES {
                            let selected = state.draft.codex_sandbox_mode == *mode;
                            ui.horizontal(|ui| {
                                if ui.selectable_label(selected, *label).clicked() && !selected {
                                    state.draft.codex_sandbox_mode = *mode;
                                    mode_changed = true;
                                }
                                widgets::truncated_label(
                                    ui,
                                    RichText::new(*description)
                                        .size(theme::FONT_SMALL)
                                        .color(theme::text_faint()),
                                    0.0,
                                );
                            });
                        }
                        if mode_changed {
                            engine.send(EngineCommand::SaveSettings {
                                settings: Box::new(state.draft.clone()),
                            });
                        }
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if widgets::ghost_button(ui, "Test sandbox").clicked() {
                                engine.send(EngineCommand::TestCodexSandbox {
                                    executable: state.draft.executable.clone(),
                                });
                            }
                            match &state.codex_sandbox_test {
                                Some(Ok(())) => {
                                    ui.label(
                                        RichText::new("✓ Codex's sandbox initializes correctly")
                                            .size(theme::FONT_SMALL)
                                            .color(theme::ok()),
                                    );
                                }
                                Some(Err(_)) => {
                                    ui.label(
                                        RichText::new("✗ Codex's sandbox failed to start — see below")
                                            .size(theme::FONT_SMALL)
                                            .color(theme::warn()),
                                    );
                                }
                                None => {}
                            }
                        });
                        if let Some(Err(message)) = &state.codex_sandbox_test {
                            widgets::wrapped_label(
                                ui,
                                RichText::new(message)
                                    .size(theme::FONT_SMALL)
                                    .color(theme::warn()),
                            );
                        }
                        ui.add_space(8.0);
                    }
                } else if state.draft.backend == BackendChoice::CustomCli {
                    ui.label(RichText::new("Command").color(theme::text_muted()));
                    if widgets::text_edit(
                        ui,
                        egui::TextEdit::multiline(&mut state.draft.command_template)
                            .hint_text("e.g. opencode run --print \"{{PROMPT}}\"")
                            .desired_width(ui.available_width())
                            .desired_rows(2),
                    )
                    .changed()
                    {
                        engine.send(EngineCommand::SaveSettings {
                            settings: Box::new(state.draft.clone()),
                        });
                    }
                    widgets::wrapped_label(
                        ui,
                        RichText::new(
                            "Runs this command as the compiler. Include {{PROMPT}} where \
                             the conversation should be inserted; leave it out and the \
                             prompt is piped to the process's stdin instead, like the \
                             account-based CLI connections above.",
                        )
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                    );
                    if ui
                        .checkbox(
                            &mut state.draft.custom_cli_agentic,
                            "This CLI is a real tool-using agent runtime",
                        )
                        .changed()
                    {
                        engine.send(EngineCommand::SaveSettings {
                            settings: Box::new(state.draft.clone()),
                        });
                    }
                    widgets::wrapped_label(
                        ui,
                        RichText::new(
                            "Enable only when the command launches an agent that can use tools, \
                             run commands, and work inside a supplied directory. A command that \
                             only returns text is not agent-shaped. Agent objectives are passed \
                             through {{PROMPT}} or stdin (which is then closed); plain text and \
                             common JSON/JSONL result fields are detected automatically while \
                             the complete output remains available as the audit transcript.",
                        )
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                    );
                    ui.add_space(8.0);
                } else if !matches!(
                    state.draft.backend,
                    BackendChoice::Auto | BackendChoice::GoogleVertex | BackendChoice::CustomCli
                ) {
                    ui.label(RichText::new("API key").color(theme::text_muted()));
                    let hint = if matches!(
                        state.draft.backend,
                        BackendChoice::OpenAiCompatible | BackendChoice::AnthropicCompatible
                    ) {
                        "optional for endpoints without authentication"
                    } else {
                        KEY_HINT
                    };
                    if widgets::text_edit(
                        ui,
                        egui::TextEdit::singleline(&mut state.draft.api_key)
                            .password(true)
                            .hint_text(hint)
                            .desired_width(ui.available_width()),
                    )
                    .changed()
                    {
                        engine.send(EngineCommand::SaveSettings {
                            settings: Box::new(state.draft.clone()),
                        });
                    }
                    widgets::wrapped_label(
                        ui,
                        RichText::new(
                            "Stored in plain text in settings.json inside the data dir — \
                             prefer environment variables on shared machines.",
                        )
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                    );
                    ui.add_space(8.0);
                }

                ui.label(RichText::new("Max tokens").color(theme::text_muted()));
                if ui
                    .add(
                        egui::DragValue::new(&mut state.draft.max_tokens)
                            .range(0..=200_000)
                            .speed(64),
                    )
                    .changed()
                {
                    engine.send(EngineCommand::SaveSettings {
                        settings: Box::new(state.draft.clone()),
                    });
                }
                widgets::wrapped_label(
                    ui,
                    RichText::new(format!(
                        "Max output tokens per compiler request. 0 = backend default \
                         ({} tokens). Extended-thinking models spend part of this budget \
                         on reasoning before producing any output — raise it if compiling \
                         or repairing plans fails with ‘no text block in response content’.",
                        crate::compiler::DEFAULT_MAX_TOKENS
                    ))
                    .size(theme::FONT_SMALL)
                    .color(theme::text_faint()),
                );
            });

            ui.add_space(12.0);
            theme::card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section_label(ui, "Plan creation");
                ui.add_space(2.0);
                if ui
                    .checkbox(&mut state.draft.auto_mode, "Auto mode")
                    .changed()
                {
                    engine.send(EngineCommand::SaveSettings {
                        settings: Box::new(state.draft.clone()),
                    });
                }
                widgets::wrapped_label(
                    ui,
                    RichText::new(AUTO_MODE_DESCRIPTION)
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                );
            });

            ui.add_space(12.0);
            theme::card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section_label(ui, "Experimental execution");
                ui.add_space(2.0);
                if ui
                    .checkbox(
                        &mut state.draft.experimental_agent_calls,
                        "Enable experimental agent steps",
                    )
                    .changed()
                {
                    engine.send(EngineCommand::SaveSettings {
                        settings: Box::new(state.draft.clone()),
                    });
                }
                widgets::wrapped_label(
                    ui,
                    RichText::new(
                        "AGENT_CALL gives the selected CLI workspace-write access and permission \
                         to run arbitrary commands in a plan-specified directory. Its complete \
                         transcript is captured for audit. Use it only with plans and workspaces \
                         you trust.",
                    )
                    .size(theme::FONT_SMALL)
                    .color(theme::warn()),
                );
                ui.add_space(6.0);
                let capability = if state.draft.supports_agent_call() {
                    "Agent steps are available with this compiler connection."
                } else if !state.draft.experimental_agent_calls {
                    "Agent steps are disabled."
                } else {
                    "This connection is completion-only. Select OpenAI account, Claude account, \
                     or an agent-shaped Custom CLI to use agent steps."
                };
                widgets::wrapped_label(
                    ui,
                    RichText::new(capability)
                        .size(theme::FONT_SMALL)
                        .color(if state.draft.supports_agent_call() {
                            theme::ok()
                        } else {
                            theme::text_faint()
                        }),
                );
            });

            ui.add_space(12.0);
            theme::card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section_label(ui, "Local MCP server");
                ui.add_space(2.0);
                widgets::wrapped_label(
                    ui,
                    RichText::new(
                        "INXM exposes planning, plan lookup, execution, repair, editing, scheduling, and run inspection as local HTTP MCP tools.",
                    )
                    .size(theme::FONT_SMALL)
                    .color(theme::text_muted()),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Port").color(theme::text_muted()));
                    if ui
                        .add(
                            egui::DragValue::new(&mut state.draft.mcp_port)
                                .range(1024..=65535)
                                .speed(1),
                        )
                        .changed()
                    {
                        engine.send(EngineCommand::SaveSettings {
                            settings: Box::new(state.draft.clone()),
                        });
                    }
                    ui.label(
                        RichText::new("applies on next app start")
                            .size(theme::FONT_SMALL)
                            .color(theme::text_faint()),
                    );
                });
                let (status_color, status_text) = match mcp_status {
                    ServerStatus::Running { .. } => (theme::ok(), mcp_status.label()),
                    ServerStatus::Starting { .. } => (theme::text_muted(), mcp_status.label()),
                    ServerStatus::Failed { .. } => (theme::warn(), mcp_status.label()),
                };
                ui.label(
                    RichText::new(status_text)
                        .size(theme::FONT_SMALL)
                        .color(status_color),
                );
                if let Some(error) = mcp_status.error() {
                    widgets::wrapped_label(
                        ui,
                        RichText::new(format!(
                            "Startup failed: {error}. Choose a different port and restart."
                        ))
                        .size(theme::FONT_SMALL)
                        .color(theme::warn()),
                    );
                }
            });

            ui.add_space(12.0);
            theme::card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section_label(ui, "Environment");
                ui.add_space(2.0);
                widgets::wrapped_label(
                    ui,
                    RichText::new(match environment.is_empty() {
                        true => "detecting…",
                        false => environment,
                    })
                    .size(theme::FONT_SMALL)
                    .color(theme::text_muted()),
                );
                widgets::wrapped_label(
                    ui,
                    RichText::new(
                        "Detected from PATH at startup and passed to the compiler, so plans \
                         only use interpreters and runners that exist on this machine.",
                    )
                    .size(theme::FONT_SMALL)
                    .color(theme::text_faint()),
                );
            });

            ui.add_space(12.0);
            theme::card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section_label(ui, "Background mode");
                ui.add_space(2.0);
                let mut keep_running = state
                    .draft
                    .keep_running_in_background
                    .unwrap_or(true);
                if ui
                    .checkbox(
                        &mut keep_running,
                        "Keep schedules running in the background",
                    )
                    .changed()
                {
                    state.draft.keep_running_in_background = Some(keep_running);
                    engine.send(EngineCommand::SaveSettings {
                        settings: Box::new(state.draft.clone()),
                    });
                }
                widgets::wrapped_label(
                    ui,
                    RichText::new(
                        "On by default: closing the window hides INXM Local in the system \
                         tray instead of quitting, so a schedule can keep running. Use the \
                         tray menu to reopen the window, pause all schedules, or quit. \
                         Uncheck this if you'd rather the window's close button quit the app \
                         outright.",
                    )
                    .size(theme::FONT_SMALL)
                    .color(theme::text_faint()),
                );
            });

            ui.add_space(12.0);
            theme::card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section_label(ui, "Anonymous usage statistics");
                ui.add_space(2.0);
                let mut telemetry = state.draft.telemetry_enabled.unwrap_or(false);
                if ui
                    .checkbox(
                        &mut telemetry,
                        "Share anonymous usage statistics",
                    )
                    .changed()
                {
                    state.draft.telemetry_enabled = Some(telemetry);
                    engine.send(EngineCommand::SaveSettings {
                        settings: Box::new(state.draft.clone()),
                    });
                }
                widgets::wrapped_label(
                    ui,
                    RichText::new(
                        "On by default at first-run setup; uncheck to opt out. Sent only \
                         at app start: a launch ping (app version, OS, launch mode) and \
                         batched counters — plans created/edited/run/failed/healed (app \
                         vs MCP), backend and model name (never a custom command), \
                         experimental mode, and seconds per view. No identifiers, no \
                         timestamps, nothing about your plans' content; the pending \
                         batch is inspectable in telemetry-usage.json in the data dir. \
                         See docs/telemetry.md for the exact schema, destination, and \
                         retention. It can also be forced off with INXM_TELEMETRY=off \
                         or the --no-telemetry flag.",
                    )
                    .size(theme::FONT_SMALL)
                    .color(theme::text_faint()),
                );
                if crate::telemetry::runtime_disabled() {
                    ui.add_space(4.0);
                    widgets::wrapped_label(
                        ui,
                        RichText::new(
                            "Currently forced off by INXM_TELEMETRY or --no-telemetry — \
                             nothing is sent regardless of this checkbox.",
                        )
                        .size(theme::FONT_SMALL)
                        .color(theme::warn()),
                    );
                }
            });

            ui.add_space(12.0);
            theme::card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section_label(ui, "Setup assistant");
                ui.add_space(2.0);
                widgets::wrapped_label(
                    ui,
                    RichText::new(
                        "Run onboarding again to detect Claude Code, Codex, and API-key connections or review their installation steps.",
                    )
                    .size(theme::FONT_SMALL)
                    .color(theme::text_muted()),
                );
                ui.add_space(8.0);
                if widgets::ghost_button(ui, "Restart onboarding").clicked() {
                    action = Some(SettingsAction::RestartOnboarding);
                }
            });

            ui.add_space(12.0);
            theme::card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::section_label(ui, "About");
                ui.add_space(2.0);
                ui.label(
                    RichText::new(format!("inxm-local v{}", env!("CARGO_PKG_VERSION")))
                        .color(theme::text_muted()),
                );
                ui.hyperlink_to("Releases on GitHub", RELEASES_URL);
                ui.add_space(8.0);

                if ui
                    .checkbox(
                        &mut state.draft.check_updates_on_startup,
                        "Check for updates on startup",
                    )
                    .changed()
                {
                    engine.send(EngineCommand::SaveSettings {
                        settings: Box::new(state.draft.clone()),
                    });
                }
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    if widgets::ghost_button(ui, "Check for updates").clicked() {
                        engine.send(EngineCommand::CheckForUpdates);
                    }
                    if let Some((version, url)) = update_available {
                        ui.hyperlink_to(
                            format!("v{version} available — open download page"),
                            url,
                        );
                    }
                });
            });

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Changes are saved automatically.")
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                );
                match state.draft.status_label() {
                    Some(label) => {
                        ui.label(
                            RichText::new(format!("→ will compile with {label}"))
                                .size(theme::FONT_SMALL)
                                .color(theme::text_muted()),
                        );
                    }
                    None => {
                        ui.label(
                            RichText::new("→ connection settings are incomplete")
                                .size(theme::FONT_SMALL)
                                .color(theme::warn()),
                        );
                    }
                }
            });
            ui.add_space(12.0);
        });
    action
}
