//! Small reusable widgets — the "atoms" of the UI.
//!
//! Nothing here knows about plans, runs, or the engine; these are primitive
//! visual elements parameterised by text and color only.

use egui::{
    Color32, CornerRadius, Pos2, Rect, Response, RichText, Sense, Stroke, StrokeKind, TextStyle,
    Ui, vec2,
};

use super::{anim, theme};

const BADGE_PADDING: egui::Vec2 = egui::vec2(7.0, 3.0);
const BUTTON_PADDING: egui::Vec2 = egui::vec2(12.0, 6.0);
const BUTTON_HOVER_SECS: f32 = 0.12;
const DOT_RADIUS: f32 = 4.0;
const DOT_HALO_EXTRA: f32 = 4.0;
const TYPING_DOT_RADIUS: f32 = 3.0;
const TYPING_DOT_GAP: f32 = 10.0;
const TYPING_PHASE_OFFSET: f64 = 0.18;
const MIN_TRUNCATED_WIDTH: f32 = 48.0;

// ─── Labels ───────────────────────────────────────────────────────────────────

/// A label that truncates with `…` instead of overflowing, keeping
/// `reserved` points free for trailing widgets in the same row.
pub fn truncated_label(ui: &mut Ui, text: RichText, reserved: f32) {
    let width = (ui.available_width() - reserved).max(MIN_TRUNCATED_WIDTH);
    ui.scope(|ui| {
        ui.set_max_width(width);
        ui.add(egui::Label::new(text).truncate());
    });
}

/// A label that wraps within the available width instead of overflowing.
pub fn wrapped_label(ui: &mut Ui, text: RichText) {
    ui.add(egui::Label::new(text).wrap());
}

/// A wrapped label capped at `max_height`: text taller than the cap scrolls
/// inside its own area instead of stretching the surrounding layout.
pub fn clamped_label(ui: &mut Ui, id: egui::Id, text: RichText, max_height: f32) {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(max_height)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            wrapped_label(ui, text);
        });
}

/// Muted uppercase section label.
pub fn section_label(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text.to_uppercase())
            .size(theme::FONT_SMALL)
            .color(theme::text_faint()),
    );
}

// ── Text fields ──────────────────────────────────────────────────────────────

/// A consistently padded text field with the shared neutral focus halo.
/// Works for single-line, multiline, editable, and read-only `TextEdit`s.
pub fn text_edit(ui: &mut Ui, edit: egui::TextEdit<'_>) -> Response {
    text_edit_with_validation(ui, edit, false)
}

/// A text field that uses the destructive border token while invalid.
pub fn text_edit_with_validation(ui: &mut Ui, edit: egui::TextEdit<'_>, invalid: bool) -> Response {
    let response = ui
        .scope(|ui| {
            let visuals = &mut ui.style_mut().visuals.widgets;
            // A read-only (`.interactive(false)`) `TextEdit` takes its frame
            // from `widgets.noninteractive`, which the global style points at
            // `theme::separator_stroke()` so that separators and panel
            // boundaries share one grey (#62). A read-only field is a disabled
            // control, not a boundary, so restore the disabled-control border
            // here — this wrapper is the single place every text field in the
            // app goes through, versus a dozen `ui.separator()` call sites, so
            // it is the smaller and better-anchored override. Both invariants
            // then hold: separators paint `divider()`, read-only and disabled
            // controls paint `disabled_border()` (the same token
            // `paint_styled_button` uses for disabled buttons).
            visuals.noninteractive.bg_stroke = theme::readonly_control_stroke();
            if invalid {
                let err = Stroke::new(1.0_f32, theme::err());
                visuals.inactive.bg_stroke = err;
                visuals.hovered.bg_stroke = err;
                visuals.noninteractive.bg_stroke = err;
            }
            ui.add(edit.margin(theme::control_margin()))
        })
        .inner;

    if response.has_focus() && ui.is_rect_visible(response.rect) {
        ui.painter().rect_stroke(
            response.rect.expand(3.0),
            CornerRadius::same(theme::RADIUS_WIDGET + 2),
            Stroke::new(3.0_f32, theme::focus_halo()),
            StrokeKind::Inside,
        );
    }
    response
}

// ─── Badges and dots ──────────────────────────────────────────────────────────

/// A small tinted pill label, e.g. a step-type badge or version chip.
pub fn badge(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let font = TextStyle::Small.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, color);
    let size = galley.size() + BADGE_PADDING * 2.0;
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect(
            rect,
            CornerRadius::same(theme::RADIUS_WIDGET),
            theme::with_alpha(color, 0.14),
            Stroke::new(1.0_f32, theme::with_alpha(color, 0.35)),
            StrokeKind::Inside,
        );
        ui.painter().galley(rect.min + BADGE_PADDING, galley, color);
    }
    response
}

/// A status dot; when `pulsing` a soft halo breathes around it.
pub fn status_dot(ui: &mut Ui, color: Color32, pulsing: bool) -> Response {
    let side = (DOT_RADIUS + DOT_HALO_EXTRA) * 2.0;
    let (rect, response) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
    if ui.is_rect_visible(rect) {
        let center = rect.center();
        if pulsing {
            let t = anim::pulse(ui.input(|i| i.time), anim::PULSE_SECS);
            ui.painter().circle_filled(
                center,
                DOT_RADIUS + DOT_HALO_EXTRA * t,
                theme::with_alpha(color, 0.25 * (1.0 - t) + 0.05),
            );
            ui.ctx().request_repaint();
        }
        ui.painter().circle_filled(center, DOT_RADIUS, color);
    }
    response
}

/// Three softly bouncing dots — shown while the compiler is thinking.
pub fn typing_indicator(ui: &mut Ui) {
    let width = TYPING_DOT_GAP * 2.0 + TYPING_DOT_RADIUS * 4.0;
    let height = TYPING_DOT_RADIUS * 6.0;
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    if ui.is_rect_visible(rect) {
        let time = ui.input(|i| i.time);
        let baseline = rect.center().y;
        for i in 0..3 {
            let t = anim::pulse(time - i as f64 * TYPING_PHASE_OFFSET, anim::PULSE_SECS);
            let x = rect.left() + TYPING_DOT_RADIUS + i as f32 * TYPING_DOT_GAP;
            let y = baseline - t * TYPING_DOT_RADIUS * 1.8;
            ui.painter().circle_filled(
                Pos2::new(x, y),
                TYPING_DOT_RADIUS,
                theme::with_alpha(theme::text_muted(), 0.4 + 0.6 * t),
            );
        }
        ui.ctx().request_repaint();
    }
}

// ─── Buttons ──────────────────────────────────────────────────────────────────

struct ButtonStyle {
    fill: Color32,
    fill_hover: Color32,
    fill_focus: Color32,
    fill_pressed: Color32,
    text: Color32,
    stroke: Option<Stroke>,
}

/// A filled accent button — the primary action in a group.
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    styled_button(
        ui,
        text,
        ButtonStyle {
            fill: theme::primary(),
            fill_hover: theme::primary_hover(),
            fill_focus: theme::primary_focus(),
            fill_pressed: theme::primary_pressed(),
            text: theme::primary_text(),
            stroke: None,
        },
    )
}

/// A quiet outlined button for secondary actions.
pub fn ghost_button(ui: &mut Ui, text: &str) -> Response {
    styled_button(
        ui,
        text,
        ButtonStyle {
            fill: theme::surface(),
            fill_hover: theme::control_hover(),
            fill_focus: theme::surface(),
            fill_pressed: theme::control_hover(),
            text: theme::text(),
            stroke: Some(Stroke::new(1.0_f32, theme::border())),
        },
    )
}

/// A compact secondary button containing a painted, font-independent icon.
pub fn ghost_icon_button(ui: &mut Ui, icon: Icon) -> Response {
    let font = TextStyle::Button.resolve(ui.style());
    let galley = ui
        .painter()
        .layout_no_wrap(String::new(), font, theme::text());
    let response = paint_styled_button(
        ui,
        galley,
        vec2(30.0, 30.0),
        ButtonStyle {
            fill: theme::surface(),
            fill_hover: theme::control_hover(),
            fill_focus: theme::surface(),
            fill_pressed: theme::control_hover(),
            text: theme::text(),
            stroke: Some(Stroke::new(1.0_f32, theme::border())),
        },
    );
    if ui.is_rect_visible(response.rect) {
        paint_icon(ui.painter(), response.rect, icon, theme::text());
    }
    response
}

/// A compact, prominent icon-only button using the filled accent style —
/// for primary actions that don't need a text label (e.g. "new chat").
pub fn primary_icon_button(ui: &mut Ui, icon: Icon) -> Response {
    let font = TextStyle::Button.resolve(ui.style());
    let galley = ui
        .painter()
        .layout_no_wrap(String::new(), font, theme::primary_text());
    let response = paint_styled_button(
        ui,
        galley,
        vec2(30.0, 30.0),
        ButtonStyle {
            fill: theme::primary(),
            fill_hover: theme::primary_hover(),
            fill_focus: theme::primary_focus(),
            fill_pressed: theme::primary_pressed(),
            text: theme::primary_text(),
            stroke: None,
        },
    );
    if ui.is_rect_visible(response.rect) {
        paint_icon(ui.painter(), response.rect, icon, theme::primary_text());
    }
    response
}

/// A quiet outlined button whose label wraps within `max_width`.
///
/// This is intended for sentence-length actions such as example prompts. Short
/// navigation and toolbar actions should continue to use [`ghost_button`].
/// Unlike [`ghost_button`], the label uses the regular body weight at a muted
/// tone rather than the medium-weight button face — a paragraph of bold text
/// reads as shouting, so these sit quieter in the hierarchy than real actions.
pub fn wrapped_ghost_button(ui: &mut Ui, text: &str, max_width: f32) -> Response {
    let style = ButtonStyle {
        fill: theme::surface(),
        fill_hover: theme::control_hover(),
        fill_focus: theme::surface(),
        fill_pressed: theme::control_hover(),
        text: theme::text_muted(),
        stroke: Some(Stroke::new(1.0_f32, theme::border())),
    };
    let width = ui
        .available_width()
        .min(max_width)
        .max(BUTTON_PADDING.x * 2.0);
    let font = TextStyle::Body.resolve(ui.style());
    let galley = ui.painter().layout(
        text.to_owned(),
        font,
        style.text,
        width - BUTTON_PADDING.x * 2.0,
    );
    let size = vec2(width, galley.size().y + BUTTON_PADDING.y * 2.0);
    paint_styled_button(ui, galley, size, style)
}

/// A small filter chip: accent-filled while selected, quiet outline otherwise.
/// Used for the status filters on the Runs view.
pub fn filter_chip(ui: &mut Ui, text: &str, selected: bool) -> Response {
    let style = if selected {
        ButtonStyle {
            fill: theme::accent(),
            fill_hover: theme::mix(theme::accent(), Color32::WHITE, 0.12),
            fill_focus: theme::accent(),
            fill_pressed: theme::mix(theme::accent(), Color32::BLACK, 0.15),
            text: Color32::WHITE,
            stroke: None,
        }
    } else {
        ButtonStyle {
            fill: theme::surface(),
            fill_hover: theme::control_hover(),
            fill_focus: theme::surface(),
            fill_pressed: theme::control_hover(),
            text: theme::text_muted(),
            stroke: Some(Stroke::new(1.0_f32, theme::border())),
        }
    };
    styled_button(ui, text, style)
}

/// A destructive-action button.
pub fn danger_button(ui: &mut Ui, text: &str) -> Response {
    styled_button(
        ui,
        text,
        ButtonStyle {
            fill: theme::err(),
            fill_hover: theme::mix(theme::err(), Color32::WHITE, 0.15),
            fill_focus: theme::err(),
            fill_pressed: theme::mix(theme::err(), Color32::BLACK, 0.18),
            text: Color32::WHITE,
            stroke: None,
        },
    )
}

fn styled_button(ui: &mut Ui, text: &str, style: ButtonStyle) -> Response {
    let font = TextStyle::Button.resolve(ui.style());
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font, style.text);
    let size = galley.size() + BUTTON_PADDING * 2.0;
    paint_styled_button(ui, galley, size, style)
}

fn paint_styled_button(
    ui: &mut Ui,
    galley: std::sync::Arc<egui::Galley>,
    size: egui::Vec2,
    style: ButtonStyle,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());

    if ui.is_rect_visible(rect) {
        let enabled = response.enabled();
        let hover = ui.ctx().animate_bool_with_time(
            response.id.with("hover"),
            enabled && response.hovered(),
            BUTTON_HOVER_SECS,
        );
        let hover_fill = theme::mix(style.fill, style.fill_hover, hover);
        let fill = if !enabled {
            theme::disabled_bg()
        } else if response.is_pointer_button_down_on() {
            style.fill_pressed
        } else if response.has_focus() {
            style.fill_focus
        } else {
            hover_fill
        };
        let stroke = if !enabled {
            Stroke::new(1.0_f32, theme::disabled_border())
        } else if response.has_focus() {
            Stroke::new(2.0_f32, theme::focus())
        } else {
            match style.stroke {
                Some(s) => Stroke::new(
                    s.width,
                    theme::mix(s.color, theme::with_alpha(theme::text(), 0.24), hover),
                ),
                None => Stroke::NONE,
            }
        };
        if enabled && response.has_focus() {
            ui.painter().rect_stroke(
                rect.expand(3.0),
                CornerRadius::same(theme::RADIUS_WIDGET + 2),
                Stroke::new(3.0_f32, theme::focus_halo()),
                StrokeKind::Inside,
            );
        }
        ui.painter().rect(
            rect,
            CornerRadius::same(theme::RADIUS_WIDGET),
            fill,
            stroke,
            StrokeKind::Inside,
        );
        let text_color = if !enabled {
            theme::disabled_text()
        } else {
            match style.fill == Color32::TRANSPARENT {
                true => theme::mix(style.text, theme::text(), hover),
                false => style.text,
            }
        };
        ui.painter()
            .galley(rect.min + BUTTON_PADDING, galley, text_color);
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }
    response
}

// ─── Vector icons ─────────────────────────────────────────────────────────────

/// Painted (font-independent) icons used in navigation and headers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Icon {
    /// Speech bubble.
    Chat,
    /// Mini DAG — one root, two children.
    Plans,
    /// Play triangle — executions.
    Runs,
    /// Plug with two pins.
    Tools,
    /// Clock face.
    Schedules,
    /// Three slider rows.
    Settings,
    /// Diagonal arrows pointing away from the center.
    Expand,
    /// Diagonal arrows pointing toward the center.
    Collapse,
    /// Plus sign — add / create action.
    Plus,
    /// Three ascending bars — a compact dashboard/chart glyph, distinct from
    /// the `Plans` DAG icon; used for the top-bar "Overview" shortcut.
    Overview,
    /// Window outline with the left column filled — toggles the left sidebar.
    PanelLeft,
    /// Window outline with the right column filled — toggles the right panel.
    PanelRight,
}

/// Paint `icon` centred in `rect` with the given color.
pub fn paint_icon(painter: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let stroke = Stroke::new(1.6_f32, color);
    let r = Rect::from_center_size(rect.center(), vec2(14.0, 14.0));
    match icon {
        Icon::Chat => {
            let bubble = Rect::from_min_max(r.min, Pos2::new(r.max.x, r.max.y - 3.5));
            painter.rect(
                bubble,
                CornerRadius::same(4),
                Color32::TRANSPARENT,
                stroke,
                StrokeKind::Inside,
            );
            // Tail.
            painter.line_segment(
                [
                    Pos2::new(r.min.x + 4.0, bubble.max.y),
                    Pos2::new(r.min.x + 3.0, r.max.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    Pos2::new(r.min.x + 3.0, r.max.y),
                    Pos2::new(r.min.x + 8.0, bubble.max.y),
                ],
                stroke,
            );
        }
        Icon::Plans => {
            let root = Pos2::new(r.min.x + 3.0, r.min.y + 2.5);
            let mid = Pos2::new(r.max.x - 3.0, r.center().y);
            let leaf = Pos2::new(r.min.x + 3.0, r.max.y - 2.5);
            painter.line_segment(
                [root, mid],
                Stroke::new(1.2_f32, theme::with_alpha(color, 0.7)),
            );
            painter.line_segment(
                [mid, leaf],
                Stroke::new(1.2_f32, theme::with_alpha(color, 0.7)),
            );
            for p in [root, mid, leaf] {
                painter.circle_filled(p, 2.6, color);
            }
        }
        Icon::Runs => {
            // Outlined play triangle, visually weighted like the other nav
            // glyphs (stroke, not fill).
            let points = vec![
                Pos2::new(r.min.x + 3.5, r.min.y + 1.5),
                Pos2::new(r.max.x - 2.0, r.center().y),
                Pos2::new(r.min.x + 3.5, r.max.y - 1.5),
            ];
            painter.add(egui::Shape::convex_polygon(
                points,
                Color32::TRANSPARENT,
                stroke,
            ));
        }
        Icon::Tools => {
            // Plug body.
            let body = Rect::from_min_max(
                Pos2::new(r.min.x + 2.0, r.min.y + 4.5),
                Pos2::new(r.max.x - 2.0, r.max.y - 3.5),
            );
            painter.rect(
                body,
                CornerRadius::same(3),
                Color32::TRANSPARENT,
                stroke,
                StrokeKind::Inside,
            );
            // Pins.
            for x in [body.min.x + 3.0, body.max.x - 3.0] {
                painter.line_segment([Pos2::new(x, r.min.y), Pos2::new(x, body.min.y)], stroke);
            }
            // Cable.
            painter.line_segment(
                [
                    Pos2::new(r.center().x, body.max.y),
                    Pos2::new(r.center().x, r.max.y),
                ],
                stroke,
            );
        }
        Icon::Schedules => {
            let center = r.center();
            painter.circle_stroke(center, 6.5, stroke);
            // Hands at ten past ten.
            painter.line_segment(
                [center, Pos2::new(center.x, center.y - 4.0)],
                Stroke::new(1.4_f32, color),
            );
            painter.line_segment(
                [center, Pos2::new(center.x + 3.0, center.y + 1.5)],
                Stroke::new(1.4_f32, color),
            );
        }
        Icon::Settings => {
            let rows = [r.min.y + 2.5, r.center().y, r.max.y - 2.5];
            let knobs = [r.max.x - 4.0, r.min.x + 4.0, r.center().x];
            for (y, knob_x) in rows.iter().zip(knobs.iter()) {
                painter.line_segment(
                    [Pos2::new(r.min.x, *y), Pos2::new(r.max.x, *y)],
                    Stroke::new(1.2_f32, theme::with_alpha(color, 0.7)),
                );
                painter.circle_filled(Pos2::new(*knob_x, *y), 2.4, color);
            }
        }
        Icon::Expand | Icon::Collapse => {
            let outward = icon == Icon::Expand;
            let (tail_a, head_a, tail_b, head_b) = if outward {
                (
                    Pos2::new(r.center().x - 1.0, r.center().y + 1.0),
                    Pos2::new(r.min.x + 1.0, r.min.y + 1.0),
                    Pos2::new(r.center().x + 1.0, r.center().y - 1.0),
                    Pos2::new(r.max.x - 1.0, r.max.y - 1.0),
                )
            } else {
                (
                    Pos2::new(r.min.x + 1.0, r.min.y + 1.0),
                    Pos2::new(r.center().x - 1.0, r.center().y + 1.0),
                    Pos2::new(r.max.x - 1.0, r.max.y - 1.0),
                    Pos2::new(r.center().x + 1.0, r.center().y - 1.0),
                )
            };
            for (tail, head) in [(tail_a, head_a), (tail_b, head_b)] {
                painter.line_segment([tail, head], stroke);
                let direction = (tail - head).normalized() * 3.5;
                let normal = vec2(-direction.y, direction.x) * 0.65;
                painter.line_segment([head, head + direction + normal], stroke);
                painter.line_segment([head, head + direction - normal], stroke);
            }
        }
        Icon::Plus => {
            painter.line_segment(
                [
                    Pos2::new(r.center().x, r.min.y),
                    Pos2::new(r.center().x, r.max.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    Pos2::new(r.min.x, r.center().y),
                    Pos2::new(r.max.x, r.center().y),
                ],
                stroke,
            );
        }
        Icon::PanelLeft | Icon::PanelRight => {
            let frame = Rect::from_center_size(r.center(), vec2(13.0, 11.0));
            painter.rect(
                frame,
                CornerRadius::same(2),
                Color32::TRANSPARENT,
                Stroke::new(1.4_f32, color),
                StrokeKind::Inside,
            );
            let divider_x = match icon {
                Icon::PanelLeft => frame.min.x + 4.5,
                _ => frame.max.x - 4.5,
            };
            painter.line_segment(
                [
                    Pos2::new(divider_x, frame.min.y + 1.0),
                    Pos2::new(divider_x, frame.max.y - 1.0),
                ],
                Stroke::new(1.4_f32, color),
            );
        }
        Icon::Overview => {
            let baseline = r.max.y - 2.0;
            let bar_stroke = Stroke::new(2.2_f32, color);
            for (x, height) in [
                (r.min.x + 2.0, 4.0),
                (r.center().x, 6.5),
                (r.max.x - 2.0, 9.0),
            ] {
                painter.line_segment(
                    [Pos2::new(x, baseline), Pos2::new(x, baseline - height)],
                    bar_stroke,
                );
            }
        }
    }
}

// ─── Panel resize handle ──────────────────────────────────────────────────────

/// Which edge of a side panel its resize handle sits on: `Right` for a
/// left-docked panel, `Left` for a right-docked one.
#[derive(Clone, Copy, PartialEq)]
pub enum PanelEdge {
    Right,
    Left,
}

/// The stored width of a custom-resizable side panel, persisted under `id`.
pub fn panel_width(ctx: &egui::Context, id: egui::Id, default: f32) -> f32 {
    ctx.data_mut(|data| *data.get_persisted_mut_or(id, default))
}

/// Drag handle replacing egui's built-in side-panel resizing, which had a
/// hit target so narrow it flickered and highlighted in full text color.
/// This one floats a wider strip over the panel boundary and
/// stays in the divider palette: a 1px line in the middle, with a soft 3px
/// strip behind it while hovered or dragged. Writes the new width back to
/// the same persisted `id` that [`panel_width`] reads.
pub fn panel_resize_handle(
    ctx: &egui::Context,
    id: egui::Id,
    panel_rect: Rect,
    edge: PanelEdge,
    width_range: std::ops::RangeInclusive<f32>,
) {
    const HIT_HALF_WIDTH: f32 = 4.0;
    const STRIP_WIDTH: f32 = 3.0;
    let boundary_x = match edge {
        PanelEdge::Right => panel_rect.right(),
        PanelEdge::Left => panel_rect.left(),
    };
    let hit = Rect::from_x_y_ranges(
        (boundary_x - HIT_HALF_WIDTH)..=(boundary_x + HIT_HALF_WIDTH),
        panel_rect.y_range(),
    );
    egui::Area::new(id.with("handle"))
        .order(egui::Order::Foreground)
        .fixed_pos(hit.min)
        .show(ctx, |ui| {
            let (rect, response) = ui.allocate_exact_size(hit.size(), Sense::drag());
            let response = response.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
            if response.dragged()
                && let Some(pointer) = response.interact_pointer_pos()
            {
                let width = match edge {
                    PanelEdge::Right => pointer.x - panel_rect.left(),
                    PanelEdge::Left => panel_rect.right() - pointer.x,
                }
                .clamp(*width_range.start(), *width_range.end());
                ui.ctx().data_mut(|data| data.insert_persisted(id, width));
            }
            let active = response.hovered() || response.dragged();
            let painter = ui.painter();
            let center_x = rect.center().x;
            if active {
                painter.rect_filled(
                    Rect::from_center_size(rect.center(), vec2(STRIP_WIDTH, rect.height())),
                    0,
                    theme::with_alpha(theme::divider(), 0.6),
                );
            }
            painter.vline(
                center_x,
                rect.y_range(),
                Stroke::new(
                    1.0_f32,
                    if active {
                        theme::border()
                    } else {
                        theme::divider()
                    },
                ),
            );
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render `add` into a headless egui frame with the theme applied and
    /// return every painted shape, flattened.
    fn painted_shapes(dark_mode: bool, mut add: impl FnMut(&mut Ui)) -> Vec<egui::Shape> {
        let _guard = theme::test_mode_lock();
        let ctx = egui::Context::default();
        theme::apply(&ctx, dark_mode);
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| add(ui));
        });
        let mut flat = Vec::new();
        for clipped in output.shapes {
            flatten(clipped.shape, &mut flat);
        }
        flat
    }

    fn flatten(shape: egui::Shape, out: &mut Vec<egui::Shape>) {
        match shape {
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    flatten(shape, out);
                }
            }
            other => out.push(other),
        }
    }

    fn rect_stroke_colors(shapes: &[egui::Shape]) -> Vec<Color32> {
        shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Rect(rect) if rect.stroke.width > 0.0 => Some(rect.stroke.color),
                _ => None,
            })
            .collect()
    }

    /// The read-only run-input fields (`plan_card::readonly_input_fields`) are
    /// disabled controls, so their border must be the disabled-control token —
    /// not the divider grey the global non-interactive stroke carries for
    /// separators (#62).
    #[test]
    fn readonly_text_field_border_uses_the_disabled_control_token() {
        for dark_mode in [true, false] {
            let shapes = painted_shapes(dark_mode, |ui| {
                let mut value = String::from("captured input");
                text_edit(
                    ui,
                    egui::TextEdit::singleline(&mut value)
                        .desired_width(120.0)
                        .interactive(false),
                );
            });
            let strokes = rect_stroke_colors(&shapes);
            let expected = theme::readonly_control_stroke().color;
            assert!(
                strokes.contains(&expected),
                "dark_mode={dark_mode}: read-only field should be stroked with \
                 disabled_border() {expected:?}, painted strokes: {strokes:?}"
            );
            assert!(
                !strokes.contains(&theme::separator_stroke().color),
                "dark_mode={dark_mode}: read-only field must not use the divider \
                 stroke {:?}, painted strokes: {strokes:?}",
                theme::separator_stroke().color
            );
        }
    }

    /// The other half of #62: `ui.separator()` reads
    /// `widgets.noninteractive.bg_stroke`, which must stay on the divider grey
    /// the nav sidebar and the chat workspace stroke their frames with.
    #[test]
    fn separator_uses_the_divider_stroke() {
        for dark_mode in [true, false] {
            let shapes = painted_shapes(dark_mode, |ui| {
                ui.separator();
            });
            let lines: Vec<Color32> = shapes
                .iter()
                .filter_map(|shape| match shape {
                    egui::Shape::LineSegment { stroke, .. } => Some(stroke.color),
                    _ => None,
                })
                .collect();
            assert!(
                lines.contains(&theme::separator_stroke().color),
                "dark_mode={dark_mode}: separator should paint divider() {:?}, \
                 painted lines: {lines:?}",
                theme::separator_stroke().color
            );
        }
    }

    /// The editable fields keep the ordinary control border; the read-only
    /// override must not leak into them.
    #[test]
    fn editable_text_field_border_is_unchanged() {
        let shapes = painted_shapes(true, |ui| {
            let mut value = String::from("editable");
            text_edit(
                ui,
                egui::TextEdit::singleline(&mut value).desired_width(120.0),
            );
        });
        let strokes = rect_stroke_colors(&shapes);
        assert!(
            strokes.contains(&theme::border()),
            "editable field should keep border(), painted strokes: {strokes:?}"
        );
    }
}
