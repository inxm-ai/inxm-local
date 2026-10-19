//! Offline documentation comes directly from the website's Markdown and nav.
use super::{theme, widgets};
use egui::{RichText, Ui};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::collections::HashMap;
include!(concat!(env!("OUT_DIR"), "/documentation.rs"));

const SNIPPET_CHARACTERS: usize = 180;
const SNIPPET_CONTEXT: usize = 60;
const BLOCK_SPACING: f32 = 6.0;
const HEADING_SIZE_STEP: f32 = 2.0;

#[derive(Clone, Default, Debug, PartialEq, Eq)]
struct Destination {
    page: usize,
    anchor: String,
}
#[derive(Default)]
pub struct DocumentationState {
    page: usize,
    query: String,
    history: Vec<Destination>,
    location: Destination,
    pending_scroll: Option<String>,
}
impl DocumentationState {
    fn navigate(&mut self, destination: Destination) {
        if self.location != destination {
            self.history.push(self.location.clone());
        }
        self.page = destination.page;
        self.pending_scroll = Some(destination.anchor.clone());
        self.location = destination;
    }
}
pub fn online_url(path: &str) -> String {
    let path = path.strip_suffix(".md").unwrap_or(path);
    let path = path
        .strip_suffix("index")
        .unwrap_or(path)
        .trim_end_matches('/');
    let base = DOC_ONLINE_BASE.trim_end_matches('/');
    if path.is_empty() {
        format!("{base}/")
    } else {
        format!("{base}/{path}/")
    }
}
/// Resolve a relative website Markdown link without using the network.
pub fn local_link(current: &str, href: &str) -> Option<usize> {
    if href.contains("://") || href.starts_with("mailto:") {
        return None;
    }
    let href = href
        .split('#')
        .next()
        .unwrap_or(href)
        .split('?')
        .next()
        .unwrap_or(href);
    if href.is_empty() {
        return DOC_PAGES.iter().position(|(_, path, _)| *path == current);
    }
    let mut parts: Vec<&str> = if href.starts_with('/') {
        vec![]
    } else {
        current.split('/').collect()
    };
    if !href.starts_with('/') {
        parts.pop();
    }
    for part in href.trim_start_matches('/').split('/') {
        match part {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            part => parts.push(part),
        }
    }
    let target = parts.join("/");
    DOC_PAGES.iter().position(|(_, path, _)| {
        *path == target
            || path.strip_suffix(".md") == Some(target.trim_end_matches('/'))
            || *path == format!("{}/index.md", target.trim_end_matches('/'))
    })
}
fn link_destination(current: &str, href: &str) -> Option<Destination> {
    let page = local_link(current, href)?;
    let anchor = href.split_once('#').map(|(_, anchor)| anchor).unwrap_or("");
    let anchor = urlencoding::decode(anchor).ok()?.into_owned();
    Some(Destination { page, anchor })
}
fn search_snippet(line: &str, query: &str) -> String {
    let characters: Vec<char> = line.chars().collect();
    // Search character positions, not lowercase byte offsets (Unicode case folding
    // can change byte lengths).
    let matched = (0..characters.len())
        .find(|start| {
            characters[*start..]
                .iter()
                .collect::<String>()
                .to_lowercase()
                .starts_with(query)
        })
        .unwrap_or_default();
    let start = matched.saturating_sub(SNIPPET_CONTEXT);
    let end = (start + SNIPPET_CHARACTERS.max(query.chars().count() + SNIPPET_CONTEXT))
        .min(characters.len());
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        characters[start..end].iter().collect::<String>(),
        if end < characters.len() { "…" } else { "" }
    )
}
pub fn search(query: &str) -> Vec<(usize, String)> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return vec![];
    }
    DOC_PAGES
        .iter()
        .enumerate()
        .filter_map(|(index, (title, _, content))| {
            let matched = content
                .lines()
                .find(|line| line.to_lowercase().contains(&query));
            if !title.to_lowercase().contains(&query) && matched.is_none() {
                return None;
            }
            Some((index, search_snippet(matched.unwrap_or(title), &query)))
        })
        .collect()
}
pub fn show(ui: &mut Ui, state: &mut DocumentationState) {
    ui.label(theme::title("Documentation", theme::FONT_HEADING));
    ui.add(
        egui::TextEdit::singleline(&mut state.query)
            .hint_text("Search all offline documentation")
            .desired_width(f32::INFINITY),
    );
    if !state.query.trim().is_empty() {
        for (page, snippet) in search(&state.query) {
            if ui.button(DOC_PAGES[page].0).clicked() {
                state.navigate(Destination {
                    page,
                    anchor: String::new(),
                });
                state.query.clear();
            }
            widgets::wrapped_label(ui, RichText::new(snippet).color(theme::text_muted()));
        }
        ui.separator();
    }
    // A fixed-height, vertically centered row so Back, the dropdown and the
    // link share one line instead of drifting by their differing heights.
    let row_height = (ui.text_style_height(&egui::TextStyle::Button)
        + ui.spacing().button_padding.y * 2.0)
        .max(ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), row_height),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            // ComboBox centers a box of this height; a smaller one pushes the
            // taller dropdown button below the row.
            ui.spacing_mut().interact_size.y = row_height;
            if ui
                .add_enabled(!state.history.is_empty(), egui::Button::new("Back"))
                .clicked()
                && let Some(destination) = state.history.pop()
            {
                state.page = destination.page;
                state.pending_scroll = Some(destination.anchor.clone());
                state.location = destination;
            }
            egui::ComboBox::from_id_salt("documentation_navigation")
                .selected_text(DOC_PAGES[state.page].0)
                .show_ui(ui, |ui| {
                    for (page, (title, _, _)) in DOC_PAGES.iter().enumerate() {
                        if ui.selectable_label(page == state.page, *title).clicked() {
                            state.navigate(Destination {
                                page,
                                anchor: String::new(),
                            });
                        }
                    }
                });
            ui.hyperlink_to("Open online", online_url(DOC_PAGES[state.page].1));
        },
    );
    let (_, path, content) = DOC_PAGES[state.page];
    let pending = state.pending_scroll.take();
    widgets::scroll_area_vertical()
        .id_salt(("docs_page", state.page))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if let Some(destination) = render(ui, path, content, pending.as_deref()) {
                state.navigate(destination);
                ui.ctx().request_repaint();
            }
        });
}
fn normalize(content: &str) -> String {
    let mut admonition_body = false;
    content
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if let Some(admonition) = trimmed
                .strip_prefix("!!! ")
                .or_else(|| trimmed.strip_prefix("??? "))
            {
                admonition_body = true;
                format!("**{}**\n", admonition.trim_start_matches('+').trim())
            } else if admonition_body && (line.starts_with("    ") || line.starts_with('\t')) {
                line.strip_prefix("    ")
                    .or_else(|| line.strip_prefix('\t'))
                    .unwrap_or(line)
                    .to_owned()
            } else {
                if !trimmed.is_empty() {
                    admonition_body = false;
                }
                let html = (1..=6).find_map(|level| {
                    let open = format!("<h{level}>");
                    let close = format!("</h{level}>");
                    trimmed
                        .strip_prefix(&open)?
                        .strip_suffix(&close)
                        .map(|title| format!("{} {title}", "#".repeat(level)))
                });
                html.unwrap_or_else(|| line.to_owned())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
#[derive(Default, Debug)]
struct Block {
    pieces: Vec<(String, bool, bool, Option<String>)>,
    heading: Option<HeadingLevel>,
    anchor: String,
    code: bool,
    language: String,
    list: bool,
}
#[derive(Default, Debug)]
struct Table {
    rows: Vec<Vec<Block>>,
    row: Vec<Block>,
}
#[derive(Debug)]
enum DocumentBlock {
    Text(Block),
    Table(Table),
    Rule,
}
fn heading_anchor(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter_map(|character| {
            if character.is_alphanumeric() || character == '_' || character == '-' {
                Some(character)
            } else if character.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}
fn parsed_blocks(content: &str) -> Vec<DocumentBlock> {
    let normalized = normalize(content);
    let mut blocks = Vec::new();
    let mut block = Block::default();
    let mut table = None::<Table>;
    let mut bold = false;
    let mut italic = false;
    let mut link = None;
    let mut anchors = HashMap::<String, usize>::new();
    for event in Parser::new_ext(
        &normalized,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    ) {
        match event {
            Event::Start(Tag::Table(_)) => table = Some(Table::default()),
            Event::Start(Tag::TableHead) => bold = true,
            Event::Start(Tag::Heading { level, .. }) => block.heading = Some(level),
            Event::Start(Tag::Strong) => bold = true,
            Event::Start(Tag::Emphasis) => italic = true,
            Event::Start(Tag::Link { dest_url, .. }) => link = Some(dest_url.into_string()),
            Event::Start(Tag::CodeBlock(kind)) => {
                block.code = true;
                block.language = match kind {
                    CodeBlockKind::Fenced(language) => language.into_string(),
                    _ => String::new(),
                };
            }
            Event::Start(Tag::Item) => block.list = true,
            Event::Text(text) | Event::Code(text) => {
                block
                    .pieces
                    .push((text.into_string(), bold, italic, link.clone()))
            }
            Event::SoftBreak => block.pieces.push((" ".to_owned(), false, false, None)),
            Event::HardBreak => block.pieces.push(("\n".to_owned(), false, false, None)),
            Event::End(TagEnd::Strong) => bold = false,
            Event::End(TagEnd::Emphasis) => italic = false,
            Event::End(TagEnd::Link) => link = None,
            Event::End(TagEnd::TableCell) => {
                if let Some(table) = &mut table {
                    table.row.push(std::mem::take(&mut block));
                }
            }
            Event::End(TagEnd::TableRow | TagEnd::TableHead) => {
                if let Some(table) = &mut table {
                    table.rows.push(std::mem::take(&mut table.row));
                }
                bold = false;
            }
            Event::End(TagEnd::Table) => {
                if let Some(table) = table.take() {
                    blocks.push(DocumentBlock::Table(table));
                }
            }
            Event::End(
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::CodeBlock | TagEnd::Item,
            ) => {
                if !block.pieces.is_empty() {
                    if block.heading.is_some() {
                        let title = block
                            .pieces
                            .iter()
                            .map(|(text, _, _, _)| text.as_str())
                            .collect::<String>();
                        let anchor = heading_anchor(&title);
                        let count = anchors.entry(anchor.clone()).or_default();
                        block.anchor = if *count == 0 {
                            anchor
                        } else {
                            format!("{anchor}-{count}")
                        };
                        *count += 1;
                    }
                    blocks.push(DocumentBlock::Text(std::mem::take(&mut block)));
                }
            }
            Event::Rule => blocks.push(DocumentBlock::Rule),
            _ => {}
        }
    }
    if !block.pieces.is_empty() {
        blocks.push(DocumentBlock::Text(block));
    }
    blocks
}
fn draw_block(ui: &mut Ui, block: &Block, path: &str) -> Option<Destination> {
    let mut destination = None;
    if block.code {
        let text = block
            .pieces
            .iter()
            .map(|(text, _, _, _)| text.as_str())
            .collect::<String>();
        if block.language == "mermaid" {
            ui.label(RichText::new("Mermaid source").color(theme::text_muted()));
            ui.hyperlink_to("View diagram online", online_url(path));
        }
        theme::card_frame().show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut text.as_str())
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY),
            );
        });
    } else {
        ui.horizontal_wrapped(|ui| {
            // Pieces are separate widgets but already carry their own spaces;
            // egui's default item spacing would double every gap around a
            // link, italic or inline code span.
            ui.spacing_mut().item_spacing.x = 0.0;
            if block.list {
                ui.label("•");
                ui.add_space(theme::GAP_SMALL);
            }
            for (text, bold, italic, link) in &block.pieces {
                let mut rich = RichText::new(text).color(theme::text());
                if *bold {
                    rich = rich.strong();
                }
                if *italic {
                    rich = rich.italics();
                }
                if let Some(level) = block.heading {
                    let depth = match level {
                        HeadingLevel::H1 => 0,
                        HeadingLevel::H2 => 1,
                        HeadingLevel::H3 => 2,
                        HeadingLevel::H4 => 3,
                        HeadingLevel::H5 => 4,
                        HeadingLevel::H6 => 5,
                    };
                    rich = rich.strong().size(
                        (theme::FONT_HEADING - depth as f32 * HEADING_SIZE_STEP)
                            .max(theme::FONT_BODY),
                    );
                }
                if let Some(href) = link {
                    rich = rich.color(theme::accent()).underline();
                    if let Some(target) = link_destination(path, href) {
                        if ui.link(rich).clicked() {
                            destination = Some(target);
                        }
                    } else {
                        ui.hyperlink_to(rich, href);
                    }
                } else {
                    ui.label(rich);
                }
            }
        });
    }
    destination
}
fn render(ui: &mut Ui, path: &str, content: &str, scroll: Option<&str>) -> Option<Destination> {
    let mut destination = None;
    let mut scroll_applied = false;
    for (index, block) in parsed_blocks(content).iter().enumerate() {
        let response = ui
            .scope(|ui| match block {
                DocumentBlock::Text(block) => {
                    destination = draw_block(ui, block, path).or(destination.take());
                }
                DocumentBlock::Table(table) => {
                    let columns = table.rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
                    let cell_width = ui.available_width() / columns as f32;
                    egui::Grid::new(("doc_table", path, index))
                        .num_columns(columns)
                        .max_col_width(cell_width)
                        .striped(true)
                        .show(ui, |ui| {
                            for row in &table.rows {
                                for cell in row {
                                    ui.scope(|ui| {
                                        ui.set_max_width(cell_width);
                                        destination =
                                            draw_block(ui, cell, path).or(destination.take());
                                    });
                                }
                                ui.end_row();
                            }
                        });
                }
                DocumentBlock::Rule => {
                    ui.separator();
                }
            })
            .response;
        let matches = match block {
            DocumentBlock::Text(block) => scroll == Some(block.anchor.as_str()),
            _ => false,
        };
        if !scroll_applied && (scroll == Some("") || matches) {
            response.scroll_to_me(Some(egui::Align::TOP));
            scroll_applied = true;
        }
        ui.add_space(BLOCK_SPACING);
    }
    destination
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bundled_pages_and_search_work_offline() {
        assert!(!DOC_PAGES.is_empty());
        assert!(DOC_PAGES.iter().all(|(_, _, content)| !content.is_empty()));
        assert_eq!(
            search("COMPILE")
                .iter()
                .map(|(index, _)| index)
                .collect::<Vec<_>>(),
            search("compile")
                .iter()
                .map(|(index, _)| index)
                .collect::<Vec<_>>()
        );
    }
    #[test]
    fn soft_wrapped_source_lines_join_into_one_paragraph_line() {
        let blocks = parsed_blocks("Send with\nCmd+Enter, or a [link](a.md)  \nnext");
        let [DocumentBlock::Text(block)] = blocks.as_slice() else {
            panic!("expected one paragraph, got {blocks:?}");
        };
        let text = block
            .pieces
            .iter()
            .map(|(text, _, _, _)| text.as_str())
            .collect::<String>();
        // A soft break is a space; only an explicit hard break forces a newline.
        assert_eq!(text, "Send with Cmd+Enter, or a link\nnext");
    }
    #[test]
    fn relative_navigation_and_fragments_stay_local() {
        let (_, path, _) = DOC_PAGES[0];
        assert_eq!(local_link(path, "#heading"), Some(0));
        assert_eq!(
            link_destination(path, "#heading").unwrap().anchor,
            "heading"
        );
        assert_eq!(local_link(path, "https://example.com"), None);
        let mut state = DocumentationState::default();
        state.navigate(Destination {
            page: 0,
            anchor: "heading".to_owned(),
        });
        assert_eq!(state.pending_scroll.as_deref(), Some("heading"));
        assert_eq!(state.history.len(), 1);
    }
    #[test]
    fn normalization_preserves_headings_and_admonition_body() {
        let input = "!!! note\n    First paragraph\n\n\tSecond paragraph\n\nOutside\n    code\n<h1>Title</h1>\n<h6>Small</h6>";
        let normalized = normalize(input);
        assert!(normalized.contains("First paragraph\n\nSecond paragraph"));
        assert!(normalized.contains("Outside\n    code"));
        assert!(normalized.contains("# Title\n###### Small"));
        assert!(
            !parsed_blocks("??? tip\n    Text")
                .iter()
                .any(|block| matches!(block, DocumentBlock::Text(Block { code: true, .. })))
        );
    }
    #[test]
    fn tables_keep_cells_rows_and_inline_links() {
        let blocks =
            parsed_blocks("| Name | Link |\n| --- | --- |\n| Hello | [guide](index.md#start) |");
        let DocumentBlock::Table(table) = &blocks[0] else {
            panic!("expected table")
        };
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[0].len(), 2);
        assert!(table.rows[0][0].pieces[0].1);
        assert_eq!(
            table.rows[1][1].pieces[0].3.as_deref(),
            Some("index.md#start")
        );
    }
    #[test]
    fn search_snippets_include_matches_far_into_unicode_lines() {
        let line = format!("{} unique-match suffix", "é".repeat(300));
        let snippet = search_snippet(&line, "unique-match");
        assert!(snippet.contains("unique-match"));
        assert!(snippet.starts_with('…'));
    }
    #[test]
    fn heading_anchors_include_markup_and_duplicates() {
        let blocks = parsed_blocks("## Some **Title**\n\n## Some Title");
        let anchors: Vec<_> = blocks
            .iter()
            .filter_map(|block| match block {
                DocumentBlock::Text(block) if block.heading.is_some() => {
                    Some(block.anchor.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(anchors, ["some-title", "some-title-1"]);
    }
    #[test]
    fn native_renderer_accepts_representative_markdown() {
        let context = egui::Context::default();
        let _ = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| {
                render(ui, "index.md", "# Heading\n\n- Item\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n```mermaid\na-->b\n```", Some("heading"));
            });
        });
    }

    #[test]
    fn same_page_anchor_scrolls_to_heading() {
        let context = egui::Context::default();
        context.style_mut(|style| style.scroll_animation = egui::style::ScrollAnimation::none());
        let content = format!("{}## Target heading\n\nEnd", "Paragraph\n\n".repeat(40));
        let mut offset = 0.0;
        for frame in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 240.0),
                )),
                ..Default::default()
            };
            let _ = context.run(input, |context| {
                egui::CentralPanel::default().show(context, |ui| {
                    let result = egui::ScrollArea::vertical()
                        .max_height(100.0)
                        .show(ui, |ui| {
                            render(
                                ui,
                                "index.md",
                                &content,
                                (frame == 1).then_some("target-heading"),
                            );
                        });
                    offset = result.state.offset.y;
                });
            });
        }
        assert!(
            offset > 100.0,
            "the anchor should scroll beyond the first page, got {offset}"
        );
    }
}
