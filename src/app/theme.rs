//! INXM design tokens and egui style setup.
//!
//! The interface intentionally uses a restrained, warm-charcoal palette. The
//! orange brand color is reserved for selection and attention; primary actions
//! use the high-contrast inverted treatment from the product design system.

use egui::{
    Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, TextStyle, epaint::AlphaFromCoverage,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

static DARK_MODE: AtomicBool = AtomicBool::new(true);

/// Window/taskbar icon matching the palette: dark tile with orange slashes in
/// dark mode, orange tile with black slashes in light mode.
pub fn window_icon(dark_mode: bool) -> Arc<egui::IconData> {
    static DARK: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    static LIGHT: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    let (cell, bytes): (&OnceLock<_>, &[u8]) = match dark_mode {
        true => (&DARK, include_bytes!("../../assets/favicon512-dark.png")),
        false => (&LIGHT, include_bytes!("../../assets/favicon512-light.png")),
    };
    cell.get_or_init(|| {
        Arc::new(eframe::icon_data::from_png_bytes(bytes).expect("bundled app icon is a valid PNG"))
    })
    .clone()
}

fn token(dark: Color32, light: Color32) -> Color32 {
    match DARK_MODE.load(Ordering::Relaxed) {
        true => dark,
        false => light,
    }
}

pub fn is_dark() -> bool {
    DARK_MODE.load(Ordering::Relaxed)
}

/// Serialises tests that flip the palette: `apply` stores the mode in the
/// process-wide `DARK_MODE` flag, so two tests rendering in different modes at
/// the same time would read each other's colors.
#[cfg(test)]
pub(crate) fn test_mode_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// Palette — every custom-painted component reads these live tokens.
pub fn bg() -> Color32 {
    token(
        Color32::from_rgb(0x14, 0x14, 0x14),
        Color32::from_rgb(0xf0, 0xf0, 0xf0),
    )
}
pub fn panel() -> Color32 {
    token(
        Color32::from_rgb(0x1f, 0x1f, 0x1f),
        Color32::from_rgb(0xf5, 0xf5, 0xf5),
    )
}
pub fn surface() -> Color32 {
    panel()
}
pub fn input() -> Color32 {
    panel()
}
pub fn user_bubble() -> Color32 {
    token(
        Color32::from_rgb(0x26, 0x26, 0x26),
        Color32::from_rgb(0xfa, 0xfa, 0xfa),
    )
}
pub fn selected() -> Color32 {
    token(
        Color32::from_rgb(0x2c, 0x2c, 0x2c),
        Color32::from_rgb(0xf2, 0xf2, 0xf2),
    )
}
pub fn divider() -> Color32 {
    token(
        Color32::from_rgb(0x3e, 0x3e, 0x3e),
        Color32::from_rgb(0xde, 0xde, 0xde),
    )
}
pub fn border() -> Color32 {
    token(
        Color32::from_rgb(0x3e, 0x3e, 0x3e),
        Color32::from_rgb(0xde, 0xde, 0xde),
    )
}
pub fn border_inner() -> Color32 {
    token(
        Color32::from_rgba_premultiplied(18, 18, 18, 18),
        Color32::from_rgba_premultiplied(128, 128, 128, 128),
    )
}
pub fn text() -> Color32 {
    token(
        Color32::from_rgb(0xf2, 0xf2, 0xf2),
        Color32::from_rgb(0x1f, 0x1f, 0x1f),
    )
}
pub fn text_muted() -> Color32 {
    token(
        Color32::from_rgb(0xbd, 0xbd, 0xbd),
        Color32::from_rgb(0x88, 0x88, 0x88),
    )
}
pub fn text_faint() -> Color32 {
    token(
        Color32::from_rgb(0x6f, 0x6f, 0x6f),
        Color32::from_rgb(0xa0, 0xa0, 0xa0),
    )
}
pub fn icon() -> Color32 {
    token(
        Color32::from_rgb(0xbd, 0xbd, 0xbd),
        Color32::from_rgb(0x5f, 0x5f, 0x5f),
    )
}
pub fn accent() -> Color32 {
    Color32::from_rgb(0xff, 0x59, 0x00)
}
pub fn accent_dim() -> Color32 {
    token(
        Color32::from_rgb(0x59, 0x28, 0x0e),
        Color32::from_rgb(0xff, 0xdf, 0xcf),
    )
}
pub fn primary() -> Color32 {
    token(
        Color32::from_rgb(0xf2, 0xf2, 0xf2),
        Color32::from_rgb(0x1f, 0x1f, 0x1f),
    )
}
pub fn primary_hover() -> Color32 {
    token(
        Color32::from_rgb(0xdb, 0xdb, 0xdb),
        Color32::from_rgb(0x3e, 0x3e, 0x3e),
    )
}
pub fn primary_focus() -> Color32 {
    token(
        Color32::from_rgb(0xf5, 0xf5, 0xf5),
        Color32::from_rgb(0x18, 0x18, 0x1c),
    )
}
pub fn primary_pressed() -> Color32 {
    token(
        Color32::from_rgb(0xc9, 0xc9, 0xc9),
        Color32::from_rgb(0x53, 0x53, 0x53),
    )
}
pub fn primary_text() -> Color32 {
    token(
        Color32::from_rgb(0x1f, 0x1f, 0x1f),
        Color32::from_rgb(0xf2, 0xf2, 0xf2),
    )
}
pub fn control_hover() -> Color32 {
    token(
        Color32::from_rgb(0x2c, 0x2c, 0x2c),
        Color32::from_rgb(0xf2, 0xf2, 0xf2),
    )
}
pub fn focus() -> Color32 {
    token(
        Color32::from_rgb(0xf2, 0xf2, 0xf2),
        Color32::from_rgb(0x1f, 0x1f, 0x1f),
    )
}
pub fn focus_halo() -> Color32 {
    token(
        Color32::from_rgb(0x53, 0x53, 0x53),
        Color32::from_rgb(0xdb, 0xdb, 0xdb),
    )
}
pub fn disabled_bg() -> Color32 {
    token(
        Color32::from_rgb(0x1f, 0x1f, 0x1f),
        Color32::from_rgb(0xf2, 0xf2, 0xf2),
    )
}
pub fn disabled_border() -> Color32 {
    token(
        Color32::from_rgb(0x2c, 0x2c, 0x2c),
        Color32::from_rgb(0xdb, 0xdb, 0xdb),
    )
}
pub fn disabled_text() -> Color32 {
    token(
        Color32::from_rgb(0x6f, 0x6f, 0x6f),
        Color32::from_rgb(0xc9, 0xc9, 0xc9),
    )
}
pub fn ok() -> Color32 {
    Color32::from_rgb(0x21, 0xb0, 0x59)
}
/// Neutral high-contrast color for work that is currently in progress.
/// Orange remains reserved for states that need the user's attention.
pub fn active() -> Color32 {
    text()
}
pub fn warn() -> Color32 {
    accent()
}
pub fn err() -> Color32 {
    Color32::from_rgb(0xa1, 0x0b, 0x0b)
}

/// Run-source badge hues (Runs view): green = started from Chat, blue =
/// triggered by an agent over MCP, purple = fired by the scheduler. Mid-tone
/// hues that hold up on both palettes; the badge widget tints them down.
pub fn source_chat() -> Color32 {
    ok()
}
pub fn source_mcp() -> Color32 {
    Color32::from_rgb(0x3b, 0x82, 0xf6)
}
pub fn source_schedule() -> Color32 {
    Color32::from_rgb(0x8b, 0x5c, 0xf6)
}

pub fn step_tool() -> Color32 {
    accent()
}
pub fn step_code() -> Color32 {
    icon()
}
pub fn step_human() -> Color32 {
    warn()
}
pub fn step_fan() -> Color32 {
    text_muted()
}
pub fn step_prompt() -> Color32 {
    icon()
}
pub fn step_other() -> Color32 {
    text_muted()
}

// Metrics — 4 px base grid
pub const RADIUS_CARD: u8 = 8;
pub const RADIUS_WIDGET: u8 = 6;
pub const RADIUS_BUBBLE: u8 = 8;
pub const CARD_MARGIN: i8 = 12;
pub const CONTROL_MARGIN_X: i8 = 12;
pub const CONTROL_MARGIN_Y: i8 = 6;
pub const GAP: f32 = 8.0;
pub const GAP_SMALL: f32 = 6.0;
pub const SIDEBAR_WIDTH: f32 = 224.0;
pub const CHAT_MAX_WIDTH: f32 = 760.0;
pub const INTENT_MAX_HEIGHT: f32 = 120.0;

pub const FONT_BODY: f32 = 14.0;
pub const FONT_SMALL: f32 = 12.0;
pub const FONT_HEADING: f32 = 24.0;
pub const FONT_MONO: f32 = 12.0;
pub const FAMILY_MEDIUM: &str = "geist-medium";

pub fn control_margin() -> Margin {
    Margin::symmetric(CONTROL_MARGIN_X, CONTROL_MARGIN_Y)
}

pub fn with_alpha(color: Color32, alpha: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(
        color.r(),
        color.g(),
        color.b(),
        (alpha.clamp(0.0, 1.0) * 255.0) as u8,
    )
}

pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(lerp(a.r(), b.r()), lerp(a.g(), b.g()), lerp(a.b(), b.b()))
}

pub fn title(content: impl Into<String>, size: f32) -> egui::RichText {
    egui::RichText::new(content)
        .size(size)
        .family(FontFamily::Name(FAMILY_MEDIUM.into()))
        .color(text())
}

/// Soft two-layer surface: a translucent light bevel plus a near-black edge.
/// Egui exposes one frame stroke, so the 1px inner highlight is represented by
/// the stroke and the outer edge by a tight, zero-offset shadow.
pub fn card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(surface())
        .stroke(Stroke::new(1.0_f32, border_inner()))
        .corner_radius(CornerRadius::same(RADIUS_CARD))
        .inner_margin(Margin::same(CARD_MARGIN))
        .shadow(egui::epaint::Shadow {
            offset: [0, 0],
            blur: 1,
            spread: 1,
            color: token(Color32::from_black_alpha(34), Color32::from_black_alpha(10)),
        })
}

pub fn bubble_frame(fill: Color32) -> egui::Frame {
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, border_inner()))
        .corner_radius(CornerRadius::same(RADIUS_BUBBLE))
        .inner_margin(Margin::symmetric(12, 8))
}

/// Stroke for separators and panel boundaries: `ui.separator()`, the side- and
/// bottom-panel separator lines, and the frame strokes of the nav sidebar and
/// the chat workspace — those greys must be the same.
///
/// This is also what `apply` installs as `widgets.noninteractive.bg_stroke`,
/// because egui has no separate "separator stroke" knob; see
/// [`readonly_control_stroke`] for the other half of that trade.
pub fn separator_stroke() -> Stroke {
    Stroke::new(1.0_f32, divider())
}

/// Stroke for controls that are read-only or disabled — the border of the
/// custom disabled buttons (`widgets::paint_styled_button`) and of read-only
/// `TextEdit`s (`widgets::text_edit`).
///
/// egui paints a `.interactive(false)` `TextEdit` frame with
/// `widgets.noninteractive.bg_stroke`, i.e. with the very stroke that separators
/// use. Since the global has to stay on [`separator_stroke`] for #62, read-only
/// fields re-apply this token locally so that disabled controls keep one border
/// grey across the app instead of trading one inconsistency for another.
pub fn readonly_control_stroke() -> Stroke {
    Stroke::new(1.0_f32, disabled_border())
}

pub fn apply(ctx: &egui::Context, dark_mode: bool) {
    DARK_MODE.store(dark_mode, Ordering::Relaxed);
    // Keep the window/taskbar icon in sync with the palette.
    ctx.send_viewport_cmd(egui::ViewportCommand::Icon(Some(window_icon(dark_mode))));
    install_fonts(ctx);
    let mut style = (*ctx.style()).clone();

    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(FONT_HEADING, FontFamily::Name(FAMILY_MEDIUM.into())),
        ),
        (
            TextStyle::Body,
            FontId::new(FONT_BODY, FontFamily::Proportional),
        ),
        (
            TextStyle::Button,
            FontId::new(FONT_BODY, FontFamily::Name(FAMILY_MEDIUM.into())),
        ),
        (
            TextStyle::Small,
            FontId::new(FONT_SMALL, FontFamily::Monospace),
        ),
        (
            TextStyle::Monospace,
            FontId::new(FONT_MONO, FontFamily::Monospace),
        ),
    ]
    .into();

    style.spacing.item_spacing = egui::vec2(GAP, GAP_SMALL);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.menu_margin = Margin::same(8);
    style.spacing.indent = 16.0;

    let visuals = &mut style.visuals;
    visuals.dark_mode = dark_mode;
    // Glyph coverage needs to follow the palette too. The dark-mode curve
    // intentionally thickens light-on-dark text; keeping it in light mode
    // makes dark glyphs look harsh and over-sharpened.
    visuals.text_alpha_from_coverage = if dark_mode {
        AlphaFromCoverage::DARK_MODE_DEFAULT
    } else {
        AlphaFromCoverage::LIGHT_MODE_DEFAULT
    };
    visuals.override_text_color = Some(text());
    visuals.panel_fill = panel();
    visuals.window_fill = surface();
    visuals.extreme_bg_color = input();
    visuals.faint_bg_color = selected();
    visuals.code_bg_color = input();
    visuals.hyperlink_color = accent();
    visuals.window_stroke = Stroke::new(1.0_f32, border());
    visuals.menu_corner_radius = CornerRadius::same(8);

    visuals.selection.bg_fill = accent_dim();
    // TextEdit uses this stroke for its focused frame. The neutral 2 px
    // treatment mirrors the control spec; orange remains reserved for brand
    // selection and attention states.
    visuals.selection.stroke = Stroke::new(2.0_f32, focus());

    let radius = CornerRadius::same(RADIUS_WIDGET);
    visuals.widgets.noninteractive.bg_fill = disabled_bg();
    // This stroke is what `ui.separator()` and the side-panel separator
    // lines are painted with. It must match `divider()` — the boundary the
    // nav sidebar and the chat workspace draw via their frame strokes —
    // or the app ends up with two different vertical divider grays next to
    // each other ("the nav bar has a dark one").
    //
    // It is *also* the frame stroke egui gives non-interactive widgets, so
    // read-only `TextEdit`s would inherit the divider grey here; they opt back
    // out in `widgets::text_edit` via [`readonly_control_stroke`].
    visuals.widgets.noninteractive.bg_stroke = separator_stroke();
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, disabled_text());
    visuals.widgets.noninteractive.corner_radius = radius;

    visuals.widgets.inactive.bg_fill = surface();
    visuals.widgets.inactive.weak_bg_fill = surface();
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, border());
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, text());
    visuals.widgets.inactive.corner_radius = radius;

    visuals.widgets.hovered.bg_fill = control_hover();
    visuals.widgets.hovered.weak_bg_fill = control_hover();
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, border());
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, text());
    visuals.widgets.hovered.corner_radius = radius;

    visuals.widgets.active.bg_fill = input();
    visuals.widgets.active.weak_bg_fill = input();
    visuals.widgets.active.bg_stroke = Stroke::new(2.0_f32, focus());
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, text());
    visuals.widgets.active.corner_radius = radius;

    visuals.widgets.open.bg_fill = selected();
    visuals.widgets.open.weak_bg_fill = selected();
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, with_alpha(text(), 0.18));
    visuals.widgets.open.corner_radius = radius;

    ctx.set_style(style);
}

fn install_fonts(ctx: &egui::Context) {
    const GEIST: &[u8] = include_bytes!("../../assets/fonts/Geist-Regular.ttf");
    const GEIST_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/Geist-SemiBold.ttf");
    const GEIST_MONO: &[u8] = include_bytes!("../../assets/fonts/GeistMono-Regular.ttf");
    const DEJAVU_SANS: &[u8] = include_bytes!("../../assets/fonts/DejaVuSans.ttf");

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "geist".into(),
        std::sync::Arc::new(egui::FontData::from_static(GEIST)),
    );
    fonts.font_data.insert(
        "geist-mono".into(),
        std::sync::Arc::new(egui::FontData::from_static(GEIST_MONO)),
    );
    fonts.font_data.insert(
        "geist-medium".into(),
        std::sync::Arc::new(egui::FontData::from_static(GEIST_MEDIUM)),
    );
    fonts.font_data.insert(
        "dejavu".into(),
        std::sync::Arc::new(egui::FontData::from_static(DEJAVU_SANS)),
    );

    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    proportional.insert(0, "dejavu".into());
    proportional.insert(0, "geist".into());

    let monospace = fonts.families.entry(FontFamily::Monospace).or_default();
    monospace.insert(0, "dejavu".into());
    monospace.insert(0, "geist-mono".into());

    fonts.families.insert(
        FontFamily::Name(FAMILY_MEDIUM.into()),
        vec![
            "geist-medium".into(),
            "dejavu".into(),
            "NotoEmoji-Regular".into(),
            "emoji-icon-font".into(),
        ],
    );
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two strokes involved in #62 must stay distinct tokens: separators
    /// and panel boundaries paint `divider()`, read-only and disabled controls
    /// paint `disabled_border()`. If these ever collapsed into the same color
    /// the local override in `widgets::text_edit` would be pointless.
    #[test]
    fn divider_and_disabled_border_are_distinct_in_both_modes() {
        let _guard = test_mode_lock();
        for dark_mode in [true, false] {
            DARK_MODE.store(dark_mode, Ordering::Relaxed);
            assert_eq!(separator_stroke().color, divider(), "dark_mode={dark_mode}");
            assert_eq!(
                readonly_control_stroke().color,
                disabled_border(),
                "dark_mode={dark_mode}"
            );
            assert_ne!(
                divider(),
                disabled_border(),
                "dark_mode={dark_mode}: the divider and disabled-control greys \
                 must differ, otherwise #62 has no visible distinction"
            );
        }
        DARK_MODE.store(true, Ordering::Relaxed);
    }

    /// egui has no dedicated separator stroke, so `apply` parks the divider
    /// stroke on `widgets.noninteractive.bg_stroke` — the value
    /// `Separator`, the panel separator lines and `Frame::group` read.
    #[test]
    fn global_noninteractive_stroke_is_the_separator_stroke() {
        let _guard = test_mode_lock();
        let ctx = egui::Context::default();
        apply(&ctx, true);
        assert_eq!(
            ctx.style().visuals.widgets.noninteractive.bg_stroke,
            separator_stroke()
        );
    }
}
