//! First-run connection assistant.
//!
//! Probes account-backed CLIs off the UI thread because PATH scans can be slow
//! for GUI-launched apps (especially with network or WSL paths). The refresh
//! action intentionally bypasses `EnvProbe::detect`'s process cache so a CLI
//! installed while this screen is open becomes available immediately.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use egui::{RichText, Ui};

use crate::app::engine::{AppSettings, BackendChoice};
use crate::app::{theme, widgets};

const CLAUDE_DOCS_URL: &str = "https://docs.anthropic.com/en/docs/claude-code/setup";
const CODEX_DOCS_URL: &str = "https://developers.openai.com/codex/cli/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    Checking,
    Available(PathBuf),
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliConnection {
    Claude,
    Codex,
}

struct ProbeResult {
    connection: CliConnection,
    status: ConnectionStatus,
}

pub struct OnboardingState {
    pub claude: ConnectionStatus,
    pub codex: ConnectionStatus,
    probe_rx: Option<Receiver<ProbeResult>>,
}

impl Default for OnboardingState {
    fn default() -> Self {
        Self::new()
    }
}

impl OnboardingState {
    pub fn new() -> Self {
        let mut state = Self {
            claude: ConnectionStatus::Checking,
            codex: ConnectionStatus::Checking,
            probe_rx: None,
        };
        state.refresh();
        state
    }

    pub fn refresh(&mut self) {
        self.claude = ConnectionStatus::Checking;
        self.codex = ConnectionStatus::Checking;
        let (tx, rx) = mpsc::channel();
        self.probe_rx = Some(rx);
        std::thread::Builder::new()
            .name("inxm-onboarding-probe".to_owned())
            .spawn(move || {
                for (connection, program) in [
                    (CliConnection::Claude, "claude"),
                    (CliConnection::Codex, "codex"),
                ] {
                    let status = crate::hostenv::find_on_path(program)
                        .map(ConnectionStatus::Available)
                        .unwrap_or(ConnectionStatus::Missing);
                    if tx.send(ProbeResult { connection, status }).is_err() {
                        break;
                    }
                }
            })
            .expect("failed to spawn onboarding environment probe");
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        let mut disconnected = false;
        if let Some(rx) = &self.probe_rx {
            loop {
                match rx.try_recv() {
                    Ok(result) => match result.connection {
                        CliConnection::Claude => self.claude = result.status,
                        CliConnection::Codex => self.codex = result.status,
                    },
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if disconnected {
            self.probe_rx = None;
        }
        if self.probe_rx.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn selected_connection_available(&self, settings: &AppSettings) -> bool {
        match settings.backend {
            BackendChoice::Claude | BackendChoice::OpenAi | BackendChoice::Auto => {
                settings.has_key()
            }
            BackendChoice::ClaudeCode => {
                matches!(self.claude, ConnectionStatus::Available(_))
            }
            BackendChoice::Codex => matches!(self.codex, ConnectionStatus::Available(_)),
            _ => settings.has_key(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingAction {
    Complete,
    Skip,
}

pub fn show(
    ui: &mut Ui,
    state: &mut OnboardingState,
    settings: &mut AppSettings,
    telemetry_opt_in: &mut bool,
) -> Option<OnboardingAction> {
    let mut action = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(20.0);
                ui.set_max_width(640.0);
                ui.label(theme::title("Connect your assistant", theme::FONT_HEADING));
                ui.add_space(6.0);
                widgets::wrapped_label(
                    ui,
                    RichText::new(
                        "INXM can use an existing Claude Code or Codex login. Pick a detected connection, or follow the setup steps and refresh.",
                    )
                    .color(theme::text_muted()),
                );
                ui.add_space(18.0);

                ui.horizontal(|ui| {
                    widgets::section_label(ui, "Connections on this device");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if widgets::ghost_button(ui, "⟳ Refresh").clicked() {
                            state.refresh();
                        }
                    });
                });
                ui.add_space(8.0);

                let claude_selected = settings.backend == BackendChoice::ClaudeCode;
                if cli_connection_card(
                    ui,
                    "Claude account",
                    "Use your Claude Code installation and existing Anthropic login.",
                    "Claude Code",
                    &state.claude,
                    claude_selected,
                    claude_install_command(),
                    "claude",
                    CLAUDE_DOCS_URL,
                ) {
                    settings.select_backend(BackendChoice::ClaudeCode);
                }
                ui.add_space(10.0);

                let codex_selected = settings.backend == BackendChoice::Codex;
                if cli_connection_card(
                    ui,
                    "OpenAI account",
                    "Use your Codex CLI installation and existing OpenAI login.",
                    "Codex",
                    &state.codex,
                    codex_selected,
                    "npm install -g @openai/codex",
                    "codex login",
                    CODEX_DOCS_URL,
                ) {
                    settings.select_backend(BackendChoice::Codex);
                }

                let anthropic_key = std::env::var_os("ANTHROPIC_API_KEY").is_some();
                let openai_key = std::env::var_os("OPENAI_API_KEY").is_some();
                if anthropic_key || openai_key {
                    ui.add_space(18.0);
                    widgets::section_label(ui, "API keys detected");
                    ui.add_space(8.0);
                    if anthropic_key
                        && api_connection_row(
                            ui,
                            "Anthropic API",
                            "ANTHROPIC_API_KEY is available",
                            settings.backend == BackendChoice::Claude,
                        )
                    {
                        settings.select_backend(BackendChoice::Claude);
                    }
                    if openai_key
                        && api_connection_row(
                            ui,
                            "OpenAI API",
                            "OPENAI_API_KEY is available",
                            settings.backend == BackendChoice::OpenAi,
                        )
                    {
                        settings.select_backend(BackendChoice::OpenAi);
                    }
                }

                ui.add_space(18.0);
                theme::card_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.checkbox(
                        telemetry_opt_in,
                        "Share anonymous usage statistics (uncheck to opt out)",
                    );
                    widgets::wrapped_label(
                        ui,
                        RichText::new(
                            "Only app/OS version, backend/model names, feature counts, and time per view are collected—never plan or chat content. You can change this later in Settings.",
                        )
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                    );
                });

                ui.add_space(16.0);
                let ready = state.selected_connection_available(settings);
                ui.horizontal(|ui| {
                    let get_started = ui
                        .add_enabled_ui(ready, |ui| widgets::primary_button(ui, "Get started"))
                        .inner
                        .on_disabled_hover_text("Select an available connection first");
                    if get_started.clicked() {
                        action = Some(OnboardingAction::Complete);
                    }
                    if widgets::ghost_button(ui, "Skip for now").clicked() {
                        action = Some(OnboardingAction::Skip);
                    }
                    if !ready {
                        ui.label(
                            RichText::new("Select a green connection to continue")
                                .size(theme::FONT_SMALL)
                                .color(theme::text_faint()),
                        );
                    }
                });
                ui.add_space(20.0);
            });
        });
    action
}

#[allow(clippy::too_many_arguments)]
fn cli_connection_card(
    ui: &mut Ui,
    title: &str,
    description: &str,
    product: &str,
    status: &ConnectionStatus,
    selected: bool,
    install_command: &str,
    login_command: &str,
    docs_url: &str,
) -> bool {
    let mut choose = false;
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let (color, label, pulsing) = match status {
                ConnectionStatus::Checking => (theme::text_muted(), "Checking…", true),
                ConnectionStatus::Available(_) => (theme::ok(), "Available", false),
                ConnectionStatus::Missing => (theme::text_faint(), "Not detected", false),
            };
            widgets::status_dot(ui, color, pulsing);
            ui.label(RichText::new(title).strong().color(theme::text()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(label).size(theme::FONT_SMALL).color(color));
            });
        });
        widgets::wrapped_label(
            ui,
            RichText::new(description)
                .size(theme::FONT_SMALL)
                .color(theme::text_muted()),
        );
        ui.add_space(8.0);

        match status {
            ConnectionStatus::Available(path) => {
                widgets::wrapped_label(
                    ui,
                    RichText::new(format!("Detected at {}", path.display()))
                        .size(theme::FONT_SMALL)
                        .color(theme::ok()),
                );
                ui.add_space(6.0);
                if widgets::primary_button(
                    ui,
                    if selected {
                        "✓ Selected"
                    } else {
                        "Use this connection"
                    },
                )
                .clicked()
                {
                    choose = true;
                }
            }
            ConnectionStatus::Checking => {
                ui.label(
                    RichText::new(format!("Looking for {product}…"))
                        .size(theme::FONT_SMALL)
                        .color(theme::text_faint()),
                );
            }
            ConnectionStatus::Missing => {
                ui.label(
                    RichText::new(format!("Install {product}"))
                        .size(theme::FONT_SMALL)
                        .color(theme::text_muted()),
                );
                command_row(ui, install_command);
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Then sign in")
                        .size(theme::FONT_SMALL)
                        .color(theme::text_muted()),
                );
                command_row(ui, login_command);
                ui.add_space(6.0);
                ui.hyperlink_to(format!("Open {product} setup guide ↗"), docs_url);
            }
        }
    });
    choose
}

fn command_row(ui: &mut Ui, command: &str) {
    ui.horizontal(|ui| {
        widgets::truncated_label(
            ui,
            RichText::new(command)
                .monospace()
                .size(theme::FONT_MONO)
                .color(theme::text()),
            64.0,
        );
        if widgets::ghost_button(ui, "Copy").clicked() {
            ui.ctx().copy_text(command.to_owned());
        }
    });
}

fn api_connection_row(ui: &mut Ui, title: &str, detail: &str, selected: bool) -> bool {
    let mut clicked = false;
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            widgets::status_dot(ui, theme::ok(), false);
            ui.vertical(|ui| {
                ui.label(RichText::new(title).strong().color(theme::text()));
                ui.label(
                    RichText::new(detail)
                        .size(theme::FONT_SMALL)
                        .color(theme::ok()),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                clicked = widgets::ghost_button(ui, if selected { "✓ Selected" } else { "Use" })
                    .clicked();
            });
        });
    });
    clicked
}

fn claude_install_command() -> &'static str {
    if cfg!(windows) {
        "irm https://claude.ai/install.ps1 | iex"
    } else {
        "curl -fsSL https://claude.ai/install.sh | bash"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_connection_requires_a_successful_probe() {
        let mut state = OnboardingState {
            claude: ConnectionStatus::Missing,
            codex: ConnectionStatus::Missing,
            probe_rx: None,
        };
        let mut settings = AppSettings::default();
        settings.select_backend(BackendChoice::ClaudeCode);
        assert!(!state.selected_connection_available(&settings));

        state.claude = ConnectionStatus::Available(PathBuf::from("claude"));
        assert!(state.selected_connection_available(&settings));
    }
}
