//! MCP Tools view — browse the tool catalog and add, edit, or remove tools
//! (MCP servers first-class; subprocess and HTTP tools too). Changes persist
//! to `tools.yaml` through the engine.

use egui::{Align, Color32, Id, Layout, RichText, Ui};
use indexmap::IndexMap;

use crate::tools::catalog::{
    HttpConfig, McpAuth, McpConfig, McpDiscoveredTool, McpTransport, SubprocessConfig, ToolConfig,
    ToolEntry,
};
use crate::tools::oauth::OAuthConnectionStatus;

use crate::app::engine::{EngineCommand, EngineHandle};
use crate::app::{anim, theme, widgets};

const LIST_WIDTH: f32 = 300.0;
const DEFAULT_SCHEMA: &str = "{\n  \"type\": \"object\"\n}";
const HTTP_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE"];

// ─── Draft model ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DraftKind {
    Mcp,
    Subprocess,
    Http,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpDraftTransport {
    #[default]
    LocalStdio,
    RemoteHttp,
}

impl DraftKind {
    fn label(self) -> &'static str {
        match self {
            DraftKind::Mcp => "MCP server",
            DraftKind::Subprocess => "Subprocess",
            DraftKind::Http => "HTTP",
        }
    }
}

/// Editable form state for one tool. Text-based; converted to a validated
/// [`ToolEntry`] on save.
#[derive(Clone)]
pub struct ToolDraft {
    /// `Some(name)` when editing an existing entry.
    pub editing: Option<String>,
    pub kind: DraftKind,
    pub name: String,
    pub description: String,
    // MCP
    pub server_command: String,
    pub server_args: String,
    pub tool_name: String,
    pub mcp_transport: McpDraftTransport,
    pub endpoint: String,
    pub oauth_enabled: bool,
    pub oauth_client_id: String,
    // Subprocess
    pub command: String,
    pub args: String,
    // HTTP
    pub base_url: String,
    pub method: String,
    pub path_template: String,
    pub headers: String,
    // Shared
    pub env: String,
    pub input_schema: String,
    pub allowlisted: bool,
    pub timeout_secs: String,
}

impl ToolDraft {
    pub fn new(kind: DraftKind) -> Self {
        Self {
            editing: None,
            kind,
            name: String::new(),
            description: String::new(),
            server_command: String::new(),
            server_args: String::new(),
            tool_name: String::new(),
            mcp_transport: McpDraftTransport::LocalStdio,
            endpoint: String::new(),
            oauth_enabled: false,
            oauth_client_id: String::new(),
            command: String::new(),
            args: String::new(),
            base_url: String::new(),
            method: "GET".to_owned(),
            path_template: String::new(),
            headers: String::new(),
            env: String::new(),
            input_schema: DEFAULT_SCHEMA.to_owned(),
            allowlisted: true,
            timeout_secs: String::new(),
        }
    }

    pub fn from_entry(entry: &ToolEntry) -> Self {
        let mut draft = Self::new(DraftKind::Mcp);
        draft.editing = Some(entry.name.clone());
        draft.name = entry.name.clone();
        draft.description = entry.description.clone();
        draft.allowlisted = entry.allowlisted;
        draft.timeout_secs = entry
            .timeout_secs
            .map(|t| t.to_string())
            .unwrap_or_default();
        draft.input_schema = serde_json::to_string_pretty(&entry.input_schema)
            .unwrap_or_else(|_| DEFAULT_SCHEMA.to_owned());
        match &entry.config {
            ToolConfig::Mcp(c) => {
                draft.kind = DraftKind::Mcp;
                draft.tool_name = c.tool_name.clone();
                match &c.transport {
                    McpTransport::Stdio {
                        server_command,
                        server_args,
                        server_env,
                    } => {
                        draft.mcp_transport = McpDraftTransport::LocalStdio;
                        draft.server_command = server_command.clone();
                        draft.server_args = server_args.join(" ");
                        draft.env = format_map(server_env);
                    }
                    McpTransport::StreamableHttp { endpoint, auth } => {
                        draft.mcp_transport = McpDraftTransport::RemoteHttp;
                        draft.endpoint = endpoint.clone();
                        if let McpAuth::OAuth { client_id } = auth {
                            draft.oauth_enabled = true;
                            draft.oauth_client_id = client_id.clone().unwrap_or_default();
                        }
                    }
                }
            }
            ToolConfig::Subprocess(c) => {
                draft.kind = DraftKind::Subprocess;
                draft.command = c.command.clone();
                draft.args = c.args.join(" ");
                draft.env = format_map(&c.env);
            }
            ToolConfig::Http(c) => {
                draft.kind = DraftKind::Http;
                draft.base_url = c.base_url.clone();
                draft.method = c.method.clone();
                draft.path_template = c.path_template.clone();
                draft.headers = format_map(&c.headers);
            }
        }
        draft
    }

    /// Build the MCP transport from the connection fields alone (server
    /// command/args/env, or remote endpoint/auth) — independent of `name`
    /// and `tool_name`, which a specific tool call needs but discovering a
    /// server's tool list does not.
    pub fn to_mcp_transport(&self) -> Result<McpTransport, String> {
        Ok(match self.mcp_transport {
            McpDraftTransport::LocalStdio => {
                if self.server_command.trim().is_empty() {
                    return Err("server command is required".to_owned());
                }
                McpTransport::Stdio {
                    server_command: self.server_command.trim().to_owned(),
                    server_args: split_args(&self.server_args)?,
                    server_env: parse_map(&self.env)?,
                }
            }
            McpDraftTransport::RemoteHttp => {
                if self.endpoint.trim().is_empty() {
                    return Err("remote endpoint is required".to_owned());
                }
                McpTransport::StreamableHttp {
                    endpoint: self.endpoint.trim().to_owned(),
                    auth: if self.oauth_enabled {
                        McpAuth::OAuth {
                            client_id: (!self.oauth_client_id.trim().is_empty())
                                .then(|| self.oauth_client_id.trim().to_owned()),
                        }
                    } else {
                        McpAuth::None
                    },
                }
            }
        })
    }

    /// Validate and convert to a catalog entry. Errors are user-facing.
    pub fn to_entry(&self) -> Result<ToolEntry, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("name is required".to_owned());
        }
        if name.contains(char::is_whitespace) {
            return Err("name must not contain whitespace".to_owned());
        }

        let config = match self.kind {
            DraftKind::Mcp => {
                if self.tool_name.trim().is_empty() {
                    return Err("tool name on the server is required".to_owned());
                }
                let transport = self.to_mcp_transport()?;
                ToolConfig::Mcp(McpConfig {
                    tool_name: self.tool_name.trim().to_owned(),
                    transport,
                })
            }
            DraftKind::Subprocess => {
                if self.command.trim().is_empty() {
                    return Err("command is required".to_owned());
                }
                ToolConfig::Subprocess(SubprocessConfig {
                    command: self.command.trim().to_owned(),
                    args: split_args(&self.args)?,
                    env: parse_map(&self.env)?,
                    working_dir: None,
                })
            }
            DraftKind::Http => {
                if self.base_url.trim().is_empty() {
                    return Err("base URL is required".to_owned());
                }
                ToolConfig::Http(HttpConfig {
                    base_url: self.base_url.trim().to_owned(),
                    method: self.method.clone(),
                    path_template: self.path_template.trim().to_owned(),
                    headers: parse_map(&self.headers)?,
                    timeout_secs: None,
                })
            }
        };

        let input_schema: serde_json::Value = serde_json::from_str(&self.input_schema)
            .map_err(|e| format!("input schema is not valid JSON: {e}"))?;

        let timeout_secs = match self.timeout_secs.trim() {
            "" => None,
            text => Some(
                text.parse::<u64>()
                    .map_err(|_| "timeout must be a whole number of seconds".to_owned())?,
            ),
        };

        Ok(ToolEntry {
            name: name.to_owned(),
            description: self.description.trim().to_owned(),
            config,
            input_schema,
            output_schema: serde_json::json!({ "type": "object" }),
            allowlisted: self.allowlisted,
            timeout_secs,
        })
    }
}

/// Split a whitespace/quote-aware argument string.
fn split_args(text: &str) -> Result<Vec<String>, String> {
    if text.trim().is_empty() {
        return Ok(vec![]);
    }
    shlex::split(text.trim()).ok_or_else(|| "unmatched quotes in arguments".to_owned())
}

/// Parse `KEY=VALUE` lines into an ordered map.
fn parse_map(text: &str) -> Result<IndexMap<String, String>, String> {
    let mut map = IndexMap::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("'{line}' is not KEY=VALUE"))?;
        map.insert(key.trim().to_owned(), value.trim().to_owned());
    }
    Ok(map)
}

fn format_map(map: &IndexMap<String, String>) -> String {
    map.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ─── View state ───────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct McpState {
    pub draft: Option<ToolDraft>,
    pub error: Option<String>,
    pub confirm_delete: Option<String>,
    pub notice: Option<String>,
    /// Free text for the "describe what you need" box.
    pub describe_text: String,
    /// A `SynthesizeTool` request is in flight.
    pub synthesizing: bool,
    /// Error from the last synthesis attempt — kept separate from `error`,
    /// which is reserved for the manual editor's field-validation messages.
    pub synth_error: Option<String>,
    pub oauth: OAuthEditorState,
    /// A `ListMcpServerTools` request is in flight.
    pub discovering: bool,
    /// Result of the last "List tools on server" request, kept until the
    /// user imports, dismisses, or closes the draft.
    pub discovery: Option<McpDiscoveryState>,
}

#[derive(Default)]
pub struct OAuthEditorState {
    pub status: Option<OAuthConnectionStatus>,
    pub authorization_url: Option<String>,
    pub connecting: bool,
    /// Set once we've auto-fired "List tools on server" for the current
    /// `Connected` status, so reaching `Connected` kicks off exactly one
    /// listing instead of re-firing on every redraw. Cleared whenever the
    /// status drops out of `Connected`, so a disconnect/reconnect cycle
    /// triggers a fresh auto-listing.
    pub auto_listed: bool,
}

/// Tools discovered from a server's `tools/list`, offered as a checklist for
/// bulk import into the catalog.
pub struct McpDiscoveryState {
    pub tools: Vec<DiscoveredToolDraft>,
    pub error: Option<String>,
}

pub struct DiscoveredToolDraft {
    pub tool: McpDiscoveredTool,
    pub selected: bool,
}

// ─── Rendering ────────────────────────────────────────────────────────────────

pub fn kind_style(config: &ToolConfig) -> (&'static str, Color32) {
    match config {
        ToolConfig::Mcp(_) => ("mcp", theme::accent()),
        ToolConfig::Subprocess(_) => ("subprocess", theme::step_code()),
        ToolConfig::Http(_) => ("http", theme::warn()),
    }
}

pub fn show(ui: &mut Ui, state: &mut McpState, tools: &[ToolEntry], engine: &EngineHandle) {
    egui::SidePanel::left("mcp_tool_list")
        .exact_width(LIST_WIDTH)
        .frame(
            egui::Frame::new()
                .fill(theme::bg())
                .inner_margin(egui::Margin::same(12)),
        )
        .show_separator_line(true)
        .show_inside(ui, |ui| tool_list(ui, state, tools, engine));

    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::bg())
                .inner_margin(egui::Margin::same(16)),
        )
        .show_inside(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| editor(ui, state, engine));
        });
}

fn tool_list(ui: &mut Ui, state: &mut McpState, tools: &[ToolEntry], engine: &EngineHandle) {
    ui.horizontal(|ui| {
        widgets::section_label(ui, "Catalog");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::primary_button(ui, "+ Add").clicked() {
                state.draft = Some(ToolDraft::new(DraftKind::Mcp));
                state.error = None;
                state.notice = None;
                state.confirm_delete = None;
                state.oauth = OAuthEditorState::default();
            }
        });
    });
    ui.add_space(6.0);

    describe_section(ui, state, engine);
    ui.add_space(6.0);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if tools.is_empty() {
                ui.label(
                    RichText::new("No tools yet — add your first MCP server.")
                        .color(theme::text_muted()),
                );
            }
            for (index, tool) in tools.iter().enumerate() {
                let card_id = Id::new("tool_card").with(&tool.name);
                anim::entrance(ui, card_id, index as f32 * anim::STAGGER_SECS, |ui| {
                    let selected = state
                        .draft
                        .as_ref()
                        .and_then(|d| d.editing.as_deref())
                        .is_some_and(|n| n == tool.name);
                    let frame = if selected {
                        theme::card_frame().stroke(egui::Stroke::new(1.0_f32, theme::accent()))
                    } else {
                        theme::card_frame()
                    };
                    let response = frame
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let (label, color) = kind_style(&tool.config);
                                widgets::badge(ui, label, color);
                                let reserve = if tool.allowlisted { 0.0 } else { 70.0 };
                                widgets::truncated_label(
                                    ui,
                                    RichText::new(&tool.name).strong(),
                                    reserve,
                                );
                                if !tool.allowlisted {
                                    widgets::badge(ui, "blocked", theme::err());
                                }
                            });
                            if !tool.description.is_empty() {
                                widgets::wrapped_label(
                                    ui,
                                    RichText::new(&tool.description)
                                        .size(theme::FONT_SMALL)
                                        .color(theme::text_muted()),
                                );
                            }
                        })
                        .response;
                    if response.interact(egui::Sense::click()).clicked() {
                        let draft = ToolDraft::from_entry(tool);
                        let oauth_status_request = (draft.kind == DraftKind::Mcp
                            && draft.mcp_transport == McpDraftTransport::RemoteHttp
                            && draft.oauth_enabled)
                            .then(|| {
                                (
                                    draft.name.clone(),
                                    draft.endpoint.clone(),
                                    (!draft.oauth_client_id.trim().is_empty())
                                        .then(|| draft.oauth_client_id.trim().to_owned()),
                                )
                            });
                        state.draft = Some(draft);
                        state.error = None;
                        state.notice = None;
                        state.confirm_delete = None;
                        state.oauth = OAuthEditorState::default();
                        if let Some((tool_name, endpoint, client_id)) = oauth_status_request {
                            engine.send(EngineCommand::CheckMcpOAuthStatus {
                                tool_name,
                                endpoint,
                                client_id,
                            });
                        }
                    }
                    ui.add_space(4.0);
                });
            }
        });
}

/// "Describe what you need" — a free-text alternative to filling in the raw
/// per-kind fields by hand. Sends `SynthesizeTool` and, once the LLM call
/// returns, opens the result in the same manual editor used for hand-built
/// tools, so the user always reviews/adjusts before saving.
fn describe_section(ui: &mut Ui, state: &mut McpState, engine: &EngineHandle) {
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        widgets::section_label(ui, "Describe what you need");
        ui.add_space(4.0);
        widgets::text_edit(
            ui,
            egui::TextEdit::multiline(&mut state.describe_text)
                .desired_rows(3)
                .desired_width(ui.available_width())
                .hint_text("e.g. \"look up the current weather for a city\""),
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let can_submit = !state.synthesizing && !state.describe_text.trim().is_empty();
            ui.add_enabled_ui(can_submit, |ui| {
                if widgets::primary_button(ui, "Generate with AI").clicked() {
                    engine.send(EngineCommand::SynthesizeTool {
                        description: state.describe_text.trim().to_owned(),
                    });
                    state.synthesizing = true;
                    state.synth_error = None;
                    state.notice = None;
                }
            });
            if state.synthesizing {
                ui.add_space(6.0);
                widgets::status_dot(ui, theme::warn(), true);
                ui.label(
                    RichText::new("Generating…")
                        .size(theme::FONT_SMALL)
                        .color(theme::text_muted()),
                );
            }
        });
        if let Some(error) = &state.synth_error {
            ui.add_space(4.0);
            widgets::wrapped_label(ui, RichText::new(error).color(theme::err()));
        }
    });
}

fn editor(ui: &mut Ui, state: &mut McpState, engine: &EngineHandle) {
    if let Some(notice) = &state.notice {
        ui.label(RichText::new(notice).color(theme::ok()));
        ui.add_space(6.0);
    }

    let Some(draft) = &mut state.draft else {
        empty_editor_hint(ui);
        return;
    };

    let heading = match &draft.editing {
        Some(name) => format!("Edit “{name}”"),
        None => "New tool".to_owned(),
    };
    widgets::truncated_label(ui, theme::title(heading, theme::FONT_HEADING), 0.0);
    ui.add_space(8.0);

    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());

        // Kind selector — locked while editing (changing kind is a re-create).
        ui.horizontal(|ui| {
            ui.label(RichText::new("Kind").color(theme::text_muted()));
            ui.add_enabled_ui(draft.editing.is_none(), |ui| {
                for kind in [DraftKind::Mcp, DraftKind::Subprocess, DraftKind::Http] {
                    if ui
                        .selectable_label(draft.kind == kind, kind.label())
                        .clicked()
                    {
                        draft.kind = kind;
                    }
                }
            });
        });
        ui.add_space(6.0);

        // Remote OAuth needs a name to key the stored connection by, so it's
        // the one MCP case where this otherwise-optional field is actually
        // required — reflected below in both the hint and the red border.
        let name_required_for_oauth =
            draft.mcp_transport == McpDraftTransport::RemoteHttp && draft.oauth_enabled;
        text_field(
            ui,
            "Name",
            &mut draft.name,
            match draft.kind {
                DraftKind::Mcp if name_required_for_oauth => {
                    "e.g. granola — needed to connect via OAuth"
                }
                DraftKind::Mcp => "e.g. granola — used as a prefix when importing multiple tools",
                _ => "e.g. github-search",
            },
            draft.kind != DraftKind::Mcp || name_required_for_oauth,
        );
        text_field(
            ui,
            "Description",
            &mut draft.description,
            "What does it do?",
            false,
        );

        ui.add_space(4.0);
        ui.separator();
        ui.add_space(4.0);

        match draft.kind {
            DraftKind::Mcp => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Transport").color(theme::text_muted()));
                    ui.selectable_value(
                        &mut draft.mcp_transport,
                        McpDraftTransport::LocalStdio,
                        "Local stdio",
                    );
                    ui.selectable_value(
                        &mut draft.mcp_transport,
                        McpDraftTransport::RemoteHttp,
                        "Remote HTTP",
                    );
                });
                match draft.mcp_transport {
                    McpDraftTransport::LocalStdio => {
                        text_field(
                            ui,
                            "Server command",
                            &mut draft.server_command,
                            "e.g. npx",
                            true,
                        );
                        text_field(
                            ui,
                            "Server args",
                            &mut draft.server_args,
                            "e.g. -y @modelcontextprotocol/server-github",
                            false,
                        );
                        multiline_field(
                            ui,
                            "Server env (KEY=VALUE per line)",
                            &mut draft.env,
                            false,
                        );
                    }
                    McpDraftTransport::RemoteHttp => remote_oauth_fields(
                        ui,
                        draft,
                        &mut state.oauth,
                        &mut state.error,
                        &mut state.notice,
                        engine,
                    ),
                }

                // Once OAuth reaches `Connected`, list the server's tools
                // right away instead of waiting for a manual click — this
                // covers both just finishing the OAuth dance and reopening a
                // draft that was already connected.
                let oauth_connected = draft.mcp_transport == McpDraftTransport::RemoteHttp
                    && draft.oauth_enabled
                    && state.oauth.status == Some(OAuthConnectionStatus::Connected);
                if oauth_connected {
                    if !state.oauth.auto_listed && !state.discovering && state.discovery.is_none()
                    {
                        if let Ok(transport) = draft.to_mcp_transport() {
                            engine.send(EngineCommand::ListMcpServerTools { transport });
                            state.discovering = true;
                            state.discovery = None;
                            state.error = None;
                        }
                        state.oauth.auto_listed = true;
                    }
                } else {
                    state.oauth.auto_listed = false;
                }

                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);

                // Discovering and importing tools from the server is the
                // primary way to add MCP tools — it needs only the
                // connection fields above, not a specific tool name.
                bulk_import_section(
                    ui,
                    draft,
                    &mut state.discovering,
                    &mut state.discovery,
                    &mut state.error,
                    &mut state.notice,
                    engine,
                );

                ui.add_space(6.0);
                egui::CollapsingHeader::new("Add a single tool manually")
                    .id_salt("mcp-manual-tool")
                    .default_open(draft.editing.is_some())
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(
                                "Only needed to reference one specific tool by name — \
                                 use \u{201c}List tools on server\u{201d} above to import several at once.",
                            )
                            .size(theme::FONT_SMALL)
                            .color(theme::text_faint()),
                        );
                        text_field(
                            ui,
                            "Tool on server",
                            &mut draft.tool_name,
                            "tool name exposed by the MCP server",
                            true,
                        );
                    });
            }
            DraftKind::Subprocess => {
                text_field(ui, "Command", &mut draft.command, "e.g. curl", true);
                text_field(
                    ui,
                    "Fixed args",
                    &mut draft.args,
                    "prefix arguments",
                    false,
                );
                multiline_field(ui, "Env (KEY=VALUE per line)", &mut draft.env, false);
            }
            DraftKind::Http => {
                text_field(
                    ui,
                    "Base URL",
                    &mut draft.base_url,
                    "https://api.example.com",
                    true,
                );
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Method").color(theme::text_muted()));
                    for method in HTTP_METHODS {
                        if ui
                            .selectable_label(draft.method == *method, *method)
                            .clicked()
                        {
                            draft.method = (*method).to_owned();
                        }
                    }
                });
                text_field(
                    ui,
                    "Path template",
                    &mut draft.path_template,
                    "/v1/things/{id}",
                    false,
                );
                multiline_field(ui, "Headers (KEY=VALUE per line)", &mut draft.headers, false);
            }
        }

        ui.add_space(4.0);
        ui.separator();
        ui.add_space(4.0);

        multiline_field(ui, "Input schema (JSON Schema)", &mut draft.input_schema, true);
        ui.horizontal(|ui| {
            ui.checkbox(
                &mut draft.allowlisted,
                "Allowlisted (plans may call this tool)",
            );
            ui.add_space(12.0);
            ui.label(RichText::new("Timeout (s)").color(theme::text_muted()));
            widgets::text_edit(
                ui,
                egui::TextEdit::singleline(&mut draft.timeout_secs)
                    .desired_width(60.0)
                    .hint_text("none"),
            );
        });
    });

    if let Some(error) = &state.error {
        ui.add_space(6.0);
        widgets::wrapped_label(ui, RichText::new(error).color(theme::err()));
        if error.starts_with("MCP OAuth") {
            ui.add_space(4.0);
            if widgets::ghost_button(ui, "Create support ticket").clicked() {
                engine.send(EngineCommand::CreateSupportTicket {
                    run_id: None,
                    plan_ref: None,
                });
                state.notice = Some(
                    "Support ticket created — see the Plan-Chat panel for the link.".to_owned(),
                );
            }
        }
    }

    ui.add_space(10.0);
    let mut close_draft = false;
    ui.horizontal(|ui| {
        if widgets::primary_button(ui, "Save tool").clicked() {
            match draft.to_entry() {
                Ok(entry) => {
                    state.notice = Some(format!("Saved “{}”.", entry.name));
                    match &draft.editing {
                        Some(old) if *old != entry.name => {
                            engine.send(EngineCommand::RenameTool {
                                old_name: old.clone(),
                                entry: Box::new(entry),
                            });
                        }
                        _ => engine.send(EngineCommand::SaveTool {
                            entry: Box::new(entry),
                        }),
                    }
                    state.error = None;
                    close_draft = true;
                }
                Err(message) => state.error = Some(message),
            }
        }
        if widgets::ghost_button(ui, "Cancel").clicked() {
            close_draft = true;
            state.error = None;
        }

        if let Some(editing) = draft.editing.clone() {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if state.confirm_delete.as_deref() == Some(editing.as_str()) {
                    if widgets::danger_button(ui, "Really delete?").clicked() {
                        engine.send(EngineCommand::DeleteTool {
                            name: editing.clone(),
                        });
                        state.notice = Some(format!("Deleted “{editing}”."));
                        state.confirm_delete = None;
                        close_draft = true;
                    }
                } else if widgets::ghost_button(ui, "Delete").clicked() {
                    state.confirm_delete = Some(editing.clone());
                }
            });
        }
    });

    if close_draft {
        state.draft = None;
        state.confirm_delete = None;
    }
}

fn empty_editor_hint(ui: &mut Ui) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new("Select a tool to edit it, or add a new one.").color(theme::text_muted()),
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "MCP tools can use a local stdio server or a remote Streamable HTTP endpoint; \
                 plans reference tools by name.",
            )
            .size(theme::FONT_SMALL)
            .color(theme::text_faint()),
        );
    });
}

/// "List tools on server" button plus, once a discovery result has arrived,
/// a checklist for bulk-importing the tools it advertises. Applies to both
/// transports — it only needs the connection fields, not `tool_name`. If
/// `draft.name` is set it's used to namespace imported tool names (see
/// [`discovered_tool_to_entry`]).
fn bulk_import_section(
    ui: &mut Ui,
    draft: &ToolDraft,
    discovering: &mut bool,
    discovery: &mut Option<McpDiscoveryState>,
    error: &mut Option<String>,
    notice: &mut Option<String>,
    engine: &EngineHandle,
) {
    ui.horizontal(|ui| {
        ui.add_enabled_ui(!*discovering, |ui| {
            if widgets::ghost_button(ui, "List tools on server").clicked() {
                match draft.to_mcp_transport() {
                    Ok(transport) => {
                        engine.send(EngineCommand::ListMcpServerTools { transport });
                        *discovering = true;
                        *discovery = None;
                        *error = None;
                    }
                    Err(message) => *error = Some(message),
                }
            }
        });
        if *discovering {
            ui.label(RichText::new("Listing…").color(theme::text_muted()));
        }
    });

    let Some(found) = discovery.as_mut() else {
        return;
    };

    if let Some(message) = &found.error {
        ui.add_space(4.0);
        widgets::wrapped_label(ui, RichText::new(message).color(theme::err()));
        return;
    }

    if found.tools.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new("The server advertised no tools.").color(theme::text_muted()));
        return;
    }

    ui.add_space(6.0);
    ui.label(
        RichText::new(format!(
            "{} tool(s) found — select which to import:",
            found.tools.len()
        ))
        .color(theme::text_muted()),
    );
    let namespace = draft.name.trim();
    if !namespace.is_empty() {
        ui.label(
            RichText::new(format!(
                "Imported as \"{namespace}::<tool>\", e.g. \"{namespace}::{}\".",
                found
                    .tools
                    .first()
                    .map(|t| t.tool.name.as_str())
                    .unwrap_or("tool")
            ))
            .size(theme::FONT_SMALL)
            .color(theme::text_faint()),
        );
    }
    for discovered in found.tools.iter_mut() {
        let label = if discovered.tool.description.is_empty() {
            discovered.tool.name.clone()
        } else {
            format!("{} — {}", discovered.tool.name, discovered.tool.description)
        };
        ui.checkbox(&mut discovered.selected, label);
    }

    let selected_count = found.tools.iter().filter(|t| t.selected).count();
    let built: Result<Vec<ToolEntry>, String> = found
        .tools
        .iter()
        .filter(|t| t.selected)
        .map(|discovered| discovered_tool_to_entry(draft, &discovered.tool))
        .collect();
    // `found`'s borrow of `*discovery` ends here — nothing below reads it,
    // so the button handler below is free to reset `*discovery`.

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_enabled_ui(selected_count > 0, |ui| {
            if widgets::primary_button(ui, &format!("Import {selected_count} selected")).clicked() {
                match built {
                    Ok(entries) => {
                        let count = entries.len();
                        engine.send(EngineCommand::BulkSaveTools { entries });
                        *notice = Some(format!("Imported {count} tool(s)."));
                        *discovery = None;
                    }
                    Err(message) => *error = Some(message),
                }
            }
        });
        if widgets::ghost_button(ui, "Dismiss").clicked() {
            *discovery = None;
        }
    });
}

/// Builds a catalog entry for one discovered tool, using the draft's own
/// connection fields (server command/args/env, or endpoint/auth) and the
/// tool's own name, description, and input schema from the server.
///
/// If the draft's `name` field is set, it's used to namespace the imported
/// tool (e.g. name `granola` + server tool `get_meeting` becomes catalog
/// entry `granola::get_meeting`), so bulk-importing a whole server's tools
/// doesn't collide with tools imported from another server.
fn discovered_tool_to_entry(
    draft: &ToolDraft,
    tool: &McpDiscoveredTool,
) -> Result<ToolEntry, String> {
    let transport = draft.to_mcp_transport()?;
    let namespace = draft.name.trim();
    let name = if namespace.is_empty() {
        tool.name.clone()
    } else {
        format!("{namespace}::{}", tool.name)
    };
    Ok(ToolEntry {
        name,
        description: tool.description.clone(),
        config: ToolConfig::Mcp(McpConfig {
            tool_name: tool.name.clone(),
            transport,
        }),
        input_schema: tool.input_schema.clone(),
        output_schema: serde_json::json!({ "type": "object" }),
        allowlisted: true,
        timeout_secs: None,
    })
}

fn remote_oauth_fields(
    ui: &mut Ui,
    draft: &mut ToolDraft,
    oauth: &mut OAuthEditorState,
    error: &mut Option<String>,
    notice: &mut Option<String>,
    engine: &EngineHandle,
) {
    text_field(
        ui,
        "Remote endpoint",
        &mut draft.endpoint,
        "https://mcp.example.com/mcp",
        true,
    );
    ui.checkbox(&mut draft.oauth_enabled, "Connect with OAuth");
    if !draft.oauth_enabled {
        return;
    }
    text_field(
        ui,
        "Public client ID",
        &mut draft.oauth_client_id,
        "registered public client ID",
        false,
    );
    let status = match oauth.status.unwrap_or(OAuthConnectionStatus::Disconnected) {
        OAuthConnectionStatus::Disconnected => "Disconnected",
        OAuthConnectionStatus::AuthorizationPending => "Authorization pending",
        OAuthConnectionStatus::Connected => "Connected",
    };
    ui.label(RichText::new(format!("Connection: {status}")).color(theme::text_muted()));
    let configured = !draft.name.trim().is_empty() && !draft.endpoint.trim().is_empty();
    ui.horizontal(|ui| {
        if widgets::ghost_button(ui, "Refresh status").clicked() && configured {
            engine.send(EngineCommand::CheckMcpOAuthStatus {
                tool_name: draft.name.trim().to_owned(),
                endpoint: draft.endpoint.trim().to_owned(),
                client_id: (!draft.oauth_client_id.trim().is_empty())
                    .then(|| draft.oauth_client_id.trim().to_owned()),
            });
        }
        // Once connected, "Connect" isn't the action to draw the eye to
        // anymore — demote it to a ghost "Reconnect" so the button doesn't
        // read as if something still needs doing.
        let already_connected = oauth.status == Some(OAuthConnectionStatus::Connected);
        ui.add_enabled_ui(configured && !oauth.connecting, |ui| {
            let label = if already_connected {
                "Reconnect"
            } else {
                "Connect"
            };
            let clicked = if already_connected {
                widgets::ghost_button(ui, label).clicked()
            } else {
                widgets::primary_button(ui, label).clicked()
            };
            if clicked {
                engine.send(EngineCommand::BeginMcpOAuth {
                    tool_name: draft.name.trim().to_owned(),
                    endpoint: draft.endpoint.trim().to_owned(),
                    client_id: (!draft.oauth_client_id.trim().is_empty())
                        .then(|| draft.oauth_client_id.trim().to_owned()),
                });
                oauth.connecting = true;
                *error = None;
            }
        });
        if widgets::ghost_button(ui, "Disconnect").clicked() && configured {
            engine.send(EngineCommand::DisconnectMcpOAuth {
                tool_name: draft.name.trim().to_owned(),
                endpoint: draft.endpoint.trim().to_owned(),
                client_id: (!draft.oauth_client_id.trim().is_empty())
                    .then(|| draft.oauth_client_id.trim().to_owned()),
            });
            oauth.authorization_url = None;
        }
        if oauth.connecting && widgets::ghost_button(ui, "Cancel").clicked() {
            engine.send(EngineCommand::CancelMcpOAuth {
                tool_name: draft.name.trim().to_owned(),
            });
        }
    });
    if let Some(url) = &mut oauth.authorization_url {
        ui.label(
            RichText::new("Open this authorization URL in a browser:").color(theme::text_muted()),
        );
        ui.hyperlink_to("Open authorization page ↗", url.as_str());
        if widgets::ghost_button(ui, "Copy authorization URL").clicked() {
            ui.ctx().copy_text(url.clone());
            *notice = Some("Authorization URL copied.".to_owned());
        }
    }
}

/// Renders the "optional" marker that follows an optional field's label.
/// Required fields carry no marker at all — missing-ness is instead shown
/// on the input itself (a red border) by [`text_field`]/[`multiline_field`],
/// so there's one visual language for "this still needs a value" no matter
/// where the requirement comes from.
fn field_marker(ui: &mut Ui, required: bool) {
    if required {
        return;
    }
    ui.label(
        RichText::new("optional")
            .color(theme::text_muted())
            .size(theme::FONT_SMALL)
            .italics(),
    );
    ui.add_space(6.0);
}

fn text_field(ui: &mut Ui, label: &str, value: &mut String, hint: &str, required: bool) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [150.0, 20.0],
            egui::Label::new(RichText::new(label).color(theme::text_muted())),
        );
        field_marker(ui, required);
        let missing = required && value.trim().is_empty();
        widgets::text_edit_with_validation(
            ui,
            egui::TextEdit::singleline(value)
                .hint_text(hint)
                .desired_width(ui.available_width()),
            missing,
        );
    });
}

fn multiline_field(ui: &mut Ui, label: &str, value: &mut String, required: bool) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(theme::text_muted()));
        field_marker(ui, required);
    });
    let missing = required && value.trim().is_empty();
    widgets::text_edit_with_validation(
        ui,
        egui::TextEdit::multiline(value)
            .font(egui::TextStyle::Monospace)
            .desired_rows(3)
            .desired_width(ui.available_width()),
        missing,
    );
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_draft_round_trips_to_entry() {
        let mut draft = ToolDraft::new(DraftKind::Mcp);
        draft.name = "github".to_owned();
        draft.description = "GitHub MCP".to_owned();
        draft.server_command = "npx".to_owned();
        draft.server_args = "-y @modelcontextprotocol/server-github".to_owned();
        draft.tool_name = "search_repositories".to_owned();
        draft.env = "GITHUB_TOKEN=abc".to_owned();

        let entry = draft.to_entry().expect("valid draft");
        assert_eq!(entry.name, "github");
        match &entry.config {
            ToolConfig::Mcp(McpConfig {
                tool_name,
                transport:
                    McpTransport::Stdio {
                        server_command,
                        server_args,
                        server_env,
                    },
            }) => {
                assert_eq!(server_command, "npx");
                assert_eq!(server_args.len(), 2);
                assert_eq!(tool_name, "search_repositories");
                assert_eq!(
                    server_env.get("GITHUB_TOKEN").map(String::as_str),
                    Some("abc")
                );
            }
            other => panic!("expected MCP config, got {other:?}"),
        }

        // Round-trip back into a draft.
        let round = ToolDraft::from_entry(&entry);
        assert_eq!(round.kind, DraftKind::Mcp);
        assert_eq!(round.server_command, "npx");
        assert_eq!(round.env, "GITHUB_TOKEN=abc");
    }

    #[test]
    fn remote_mcp_drafts_round_trip_with_none_and_oauth_authentication() {
        let mut none = ToolDraft::new(DraftKind::Mcp);
        none.name = "remote-none".to_owned();
        none.tool_name = "search".to_owned();
        none.mcp_transport = McpDraftTransport::RemoteHttp;
        none.endpoint = "https://mcp.example.com/mcp".to_owned();
        let none_entry = none.to_entry().expect("valid remote draft");
        assert!(matches!(
            none_entry.config,
            ToolConfig::Mcp(McpConfig {
                transport: McpTransport::StreamableHttp {
                    auth: McpAuth::None,
                    ..
                },
                ..
            })
        ));

        let mut oauth = none.clone();
        oauth.oauth_enabled = true;
        oauth.oauth_client_id = "public-client".to_owned();
        let oauth_entry = oauth.to_entry().expect("valid OAuth draft");
        assert!(matches!(
            oauth_entry.config,
            ToolConfig::Mcp(McpConfig { transport: McpTransport::StreamableHttp { auth: McpAuth::OAuth { client_id: Some(ref value) }, .. }, .. }) if value == "public-client"
        ));
        let round = ToolDraft::from_entry(&oauth_entry);
        assert_eq!(round.mcp_transport, McpDraftTransport::RemoteHttp);
        assert!(round.oauth_enabled);
        assert_eq!(round.oauth_client_id, "public-client");
    }

    #[test]
    fn discovered_tool_is_namespaced_by_draft_name() {
        let mut draft = ToolDraft::new(DraftKind::Mcp);
        draft.server_command = "npx".to_owned();
        draft.server_args = "-y granola-mcp".to_owned();

        let tool = McpDiscoveredTool {
            name: "get_meeting".to_owned(),
            description: "Fetch a meeting".to_owned(),
            input_schema: serde_json::json!({ "type": "object" }),
        };

        // Without a namespace, the imported tool keeps the server's name.
        let entry = discovered_tool_to_entry(&draft, &tool).expect("valid entry");
        assert_eq!(entry.name, "get_meeting");

        // With a namespace, imported tools are prefixed with "<name>::".
        draft.name = "granola".to_owned();
        let entry = discovered_tool_to_entry(&draft, &tool).expect("valid entry");
        assert_eq!(entry.name, "granola::get_meeting");
        match &entry.config {
            ToolConfig::Mcp(McpConfig { tool_name, .. }) => {
                assert_eq!(tool_name, "get_meeting");
            }
            other => panic!("expected MCP config, got {other:?}"),
        }
    }

    #[test]
    fn draft_rejects_missing_fields() {
        let draft = ToolDraft::new(DraftKind::Mcp);
        assert!(draft.to_entry().is_err());

        let mut named = ToolDraft::new(DraftKind::Http);
        named.name = "api".to_owned();
        assert_eq!(named.to_entry().unwrap_err(), "base URL is required");
    }

    #[test]
    fn draft_rejects_invalid_schema_and_env() {
        let mut draft = ToolDraft::new(DraftKind::Subprocess);
        draft.name = "x".to_owned();
        draft.command = "echo".to_owned();
        draft.input_schema = "{ not json".to_owned();
        assert!(draft.to_entry().unwrap_err().contains("input schema"));

        draft.input_schema = DEFAULT_SCHEMA.to_owned();
        draft.env = "NOEQUALS".to_owned();
        assert!(draft.to_entry().unwrap_err().contains("KEY=VALUE"));
    }

    #[test]
    fn name_with_whitespace_is_rejected() {
        let mut draft = ToolDraft::new(DraftKind::Subprocess);
        draft.name = "bad name".to_owned();
        draft.command = "echo".to_owned();
        assert!(draft.to_entry().unwrap_err().contains("whitespace"));
    }
}
