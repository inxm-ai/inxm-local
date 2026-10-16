//! One read-only, selectable inspector shared by plans, runs and step fields.
use super::{theme, widgets};
use egui::{Context, Id, TextFormat, Ui, text::LayoutJob};
const DEFAULT_WIDTH: f32 = 480.0;
const STATE_ID: &str = "structured_inspector";
#[derive(Clone)]
struct Inspector {
    title: String,
    text: String,
    job: LayoutJob,
    dark: bool,
    json: bool,
    wrap: bool,
    expanded: bool,
}
pub fn open(ctx: &Context, title: impl Into<String>, content: &str) {
    let json = serde_json::from_str::<&serde_json::value::RawValue>(content).is_ok();
    let text = if json {
        pretty_preserving_numbers(content)
    } else {
        content.to_owned()
    };
    let state = Inspector {
        title: title.into(),
        job: highlight(&text, json),
        text,
        dark: theme::is_dark(),
        json,
        wrap: true,
        expanded: false,
    };
    ctx.data_mut(|data| data.insert_temp(Id::new(STATE_ID), state));
}
// Format token whitespace without round-tripping through floating-point values.
fn pretty_preserving_numbers(input: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for c in input.chars() {
        if quoted {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                quoted = false;
            }
            continue;
        }
        match c {
            '"' => {
                quoted = true;
                out.push(c);
            }
            '{' | '[' => {
                depth += 1;
                out.push(c);
                out.push('\n');
                out.push_str(&"  ".repeat(depth));
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                out.push('\n');
                out.push_str(&"  ".repeat(depth));
                out.push(c);
            }
            ',' => {
                out.push(c);
                out.push('\n');
                out.push_str(&"  ".repeat(depth));
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}
fn highlight(text: &str, json: bool) -> LayoutJob {
    let mut job = LayoutJob::default();
    let tokens = regex::Regex::new(
        r#""(?:\\.|[^"\\])*"|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?|true|false|null|[{}\[\],:]|\s+|."#,
    )
    .expect("static token regex");
    for token in tokens.find_iter(text) {
        let value = token.as_str();
        let color = if !json {
            theme::text()
        } else if value.starts_with('"') {
            if text[token.end()..].trim_start().starts_with(':') {
                theme::source_mcp()
            } else {
                theme::ok()
            }
        } else if value == "true" || value == "false" {
            theme::source_schedule()
        } else if value == "null" {
            theme::text_faint()
        } else if value.starts_with(|c: char| c.is_ascii_digit() || c == '-') {
            theme::accent()
        } else {
            theme::text_muted()
        };
        job.append(
            value,
            0.0,
            TextFormat {
                font_id: egui::FontId::monospace(theme::FONT_SMALL),
                color,
                ..Default::default()
            },
        );
    }
    job
}
fn contents(ui: &mut Ui, state: &mut Inspector) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        ui.label(theme::title(&state.title, theme::FONT_BODY));
        close = widgets::ghost_button(ui, "Close").clicked();
    });
    ui.horizontal(|ui| {
        if widgets::ghost_button(ui, "Copy").clicked() {
            ui.ctx().copy_text(state.text.clone());
        }
        if widgets::ghost_button(ui, if state.expanded { "Collapse" } else { "Expand" }).clicked() {
            state.expanded = !state.expanded;
        }
        ui.checkbox(&mut state.wrap, "Wrap");
    });
    if state.dark != theme::is_dark() {
        state.dark = theme::is_dark();
        state.job = highlight(&state.text, state.json);
    }
    egui::ScrollArea::both()
        .id_salt("inspector_content")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut layout = |ui: &Ui, _: &dyn egui::TextBuffer, width: f32| {
                let mut job = state.job.clone();
                job.wrap.max_width = if state.wrap { width } else { f32::INFINITY };
                ui.fonts(|fonts| fonts.layout_job(job))
            };
            ui.add(
                egui::TextEdit::multiline(&mut state.text.as_str())
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .layouter(&mut layout),
            );
        });
    close
}
/// Renders before the shell's central panel. True means the inspector fills it.
pub fn is_open(ctx: &Context) -> bool {
    ctx.data(|data| data.get_temp::<Inspector>(Id::new(STATE_ID)).is_some())
}

pub fn show(ctx: &Context) -> bool {
    let Some(mut state) = ctx.data(|data| data.get_temp::<Inspector>(Id::new(STATE_ID))) else {
        return false;
    };
    let expanded = state.expanded;
    let close = if expanded {
        egui::CentralPanel::default()
            .show(ctx, |ui| contents(ui, &mut state))
            .inner
    } else {
        egui::SidePanel::right("json_inspector")
            .resizable(true)
            .default_width(DEFAULT_WIDTH)
            .show(ctx, |ui| contents(ui, &mut state))
            .inner
    };
    ctx.data_mut(|data| {
        if close {
            data.remove::<Inspector>(Id::new(STATE_ID));
        } else {
            data.insert_temp(Id::new(STATE_ID), state);
        }
    });
    expanded
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formatting_preserves_escaped_strings_and_large_numbers() {
        let raw = r#"{"n":123456789012345678901234567890,"s":"escaped \\\" string"}"#;
        let pretty = pretty_preserving_numbers(raw);
        assert!(pretty.contains("123456789012345678901234567890"));
        assert!(pretty.contains(r#""escaped \\\" string""#));
        assert!(serde_json::from_str::<serde_json::Value>(&pretty).is_ok());
    }
    #[test]
    fn valid_json_with_out_of_range_exponent_is_highlighted_without_loss() {
        let ctx = Context::default();
        open(&ctx, "numbers", r#"{"value":1e400}"#);
        let state = ctx
            .data(|data| data.get_temp::<Inspector>(Id::new(STATE_ID)))
            .unwrap();
        assert!(state.json);
        assert!(state.text.contains("1e400"));
    }
    #[test]
    fn text_fallback_and_complete_copy_content_are_retained() {
        let ctx = Context::default();
        let raw = "plain text\n".repeat(10000);
        open(&ctx, "output", &raw);
        let state = ctx
            .data(|data| data.get_temp::<Inspector>(Id::new(STATE_ID)))
            .unwrap();
        assert_eq!(state.text, raw);
        assert!(!state.json);
        assert_eq!(state.job.text, raw);
    }
}
