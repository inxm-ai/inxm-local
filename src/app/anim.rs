//! Small animation helpers built on egui's per-frame clock.
//!
//! Everything here is time-based and stateless from the caller's point of
//! view: the first time an `Id` is seen, its birth time is remembered in the
//! egui temp store; animations are pure functions of elapsed time after that.

use egui::{Id, Ui};

/// Default entrance duration for chat messages and cards.
pub const APPEAR_SECS: f32 = 0.24;
/// Delay between successive items in a staggered list (plan steps).
pub const STAGGER_SECS: f32 = 0.045;
/// Period of the "running" pulse.
pub const PULSE_SECS: f32 = 1.2;
/// Vertical settle distance for entrance animations, in points.
pub const SLIDE_DISTANCE: f32 = 8.0;

/// Entrance progress for `id`: 0.0 the first frame it is seen, easing to 1.0
/// over `duration` seconds after `delay` seconds. Requests a repaint while
/// still animating.
pub fn appear(ui: &Ui, id: Id, delay: f32, duration: f32) -> f32 {
    let now = ui.input(|i| i.time);
    let birth = ui
        .ctx()
        .data_mut(|d| *d.get_temp_mut_or::<f64>(id.with("birth"), now));
    let raw = ((now - birth - delay as f64) / duration as f64).clamp(0.0, 1.0) as f32;
    if raw < 1.0 {
        ui.ctx().request_repaint();
    }
    ease_out_cubic(raw)
}

/// A 0→1→0 pulse with the given period; callers must request repaints while
/// they want it to keep moving.
pub fn pulse(time: f64, period_secs: f32) -> f32 {
    let phase = (time % period_secs as f64) / period_secs as f64;
    (0.5 - 0.5 * (phase * std::f64::consts::TAU).cos()) as f32
}

pub fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// Apply a fade + upward-settle entrance to the content drawn in `add`.
pub fn entrance<R>(ui: &mut Ui, id: Id, delay: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let t = appear(ui, id, delay, APPEAR_SECS);
    ui.add_space(SLIDE_DISTANCE * (1.0 - t));
    ui.scope(|ui| {
        ui.set_opacity(t);
        add(ui)
    })
    .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_out_cubic_hits_endpoints() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
    }

    #[test]
    fn pulse_stays_in_unit_range() {
        for i in 0..100 {
            let v = pulse(i as f64 * 0.037, PULSE_SECS);
            assert!((0.0..=1.0).contains(&v), "pulse out of range: {v}");
        }
    }
}
