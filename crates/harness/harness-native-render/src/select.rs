//! The selection: one plate under the glyphs for every surface that selects (a chat's words and input through `TextEdit`,
//! a table's copied row, the terminal), and the pulse a copy plays over it. Copied from den-native (ui.rs `emit_selection`
//! and `Flash`, select.rs `join_plates`, github.com/andrewgazelka/den); its look is the scene's `ui.selection` (prims.flecs)
//! and its pulse a `CopyFlash` spring on what was copied.
use harness_native_flecs::prelude::*;
use harness_native_ui_look_api::{CopyLook, Fill, Shape};
use harness_native_ui_motion_api::CopyFlash;

use crate::draw::Draw;
use crate::paint::{panel_from, rgba, snap_rect};

/// The scene prefab every selection draws from.
pub const LOOK: &str = "ui::selection";

/// One row of a selection: a rectangle in points (`x0..x1` across, `y0..y1` down).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plate {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    /// Corners that stay square (1 top-left, 2 top-right, 4 bottom-right, 8 bottom-left): those a touching neighbour row
    /// covers, so the rows read as one shape. [`join`] sets it.
    pub square: u8,
}

impl Plate {
    #[must_use]
    pub fn of(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x0: x, y0: y, x1: x + w, y1: y + h, square: 0 }
    }
}

/// Join a selection's rows (in reading order) into one shape, the site's rule (den select.rs `join_plates`): a corner is
/// square when the row above or below touches this one and reaches at least as far out on that side.
pub fn join(plates: &mut [Plate]) {
    let touches = |a: &Plate, b: &Plate| (a.y1 - b.y0).abs() < 0.5;
    for i in 0..plates.len() {
        let row = plates[i];
        let above = (i > 0 && touches(&plates[i - 1], &row)).then(|| plates[i - 1]);
        let below = plates.get(i + 1).filter(|b| touches(&row, b)).copied();
        let covers = |n: Option<Plate>, left: bool| n.is_some_and(|n| if left { n.x0 <= row.x0 + 0.5 } else { n.x1 >= row.x1 - 0.5 });
        plates[i].square = u8::from(covers(above, true))
            | u8::from(covers(above, false)) << 1
            | u8::from(covers(below, false)) << 2
            | u8::from(covers(below, true)) << 3;
    }
}

/// The selection's look, read from the scene's `ui.selection`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub fill: Fill,
    pub shape: Shape,
    pub copy: CopyLook,
}

impl Look {
    /// `ui.selection`'s look. The scene is compiled in, so one without it (or a part of it) is a bug: this panics naming it.
    #[must_use]
    pub fn of(w: WorldRef<'_>) -> Self {
        let e = w.try_lookup(LOOK).unwrap_or_else(|| panic!("the scene has no {LOOK} (prims.flecs)"));
        Self {
            fill: e.try_cloned::<&Fill>().unwrap_or_else(|| panic!("{LOOK} has no Fill")),
            shape: e.try_cloned::<&Shape>().unwrap_or_else(|| panic!("{LOOK} has no Shape")),
            copy: e.try_cloned::<&CopyLook>().unwrap_or_else(|| panic!("{LOOK} has no CopyLook")),
        }
    }

    /// The plate's colour in one appearance.
    #[must_use]
    pub fn colour(&self, dark: bool) -> [f32; 4] {
        rgba(if dark { self.fill.dark } else { self.fill.light })
    }
}

/// The middle of the plates' bounds: what a copy's pulse grows from.
fn centre(plates: &[Plate]) -> (f32, f32) {
    let (x0, y0) = plates.iter().fold((f32::MAX, f32::MAX), |(x, y), p| (x.min(p.x0), y.min(p.y0)));
    let (x1, y1) = plates.iter().fold((f32::MIN, f32::MIN), |(x, y), p| (x.max(p.x1), y.max(p.y1)));
    ((x0 + x1) * 0.5, (y0 + y1) * 0.5)
}

/// Draw a selection's rows (in reading order, points) as one shape of `look` in `colour` at `alpha`, joined ([`join`]).
/// Under a copy's `flash` the shape grows by `look.copy.grow` at the pulse's peak and a light crosses each row (den's
/// `emit_selection`); at rest the plates sit on whole device pixels at scale `sc`. Draw it before the glyphs it is under.
pub fn emit(d: &mut Draw, plates: &[Plate], look: &Look, colour: [f32; 4], flash: Option<CopyFlash>, alpha: f32, sc: f32) {
    if plates.is_empty() || alpha <= 0.0 {
        return;
    }
    let mut plates = plates.to_vec(); // alloc: a selection's rows, a handful a frame
    join(&mut plates);
    let pulse = flash.map_or(0.0, |f| f.amount());
    let scale = 1.0 + look.copy.grow * pulse;
    let (cx, cy) = centre(&plates);
    let sweep = look.copy.sweep.max(1e-3);
    for (i, p) in plates.iter().enumerate() {
        let (w, h) = (p.x1 - p.x0, p.y1 - p.y0);
        let rect = [cx + (p.x0 - cx) * scale, cy + (p.y0 - cy) * scale, w * scale, h * scale];
        let rect = if pulse > 0.0 { rect } else { snap_rect(rect, sc) };
        let shape = Shape { radius: look.shape.radius.min(h / 4.0) * scale, ..look.shape };
        let mut pi = panel_from(colour, [0.0; 4], shape, rect, alpha);
        pi.fx2[3] = f32::from(p.square);
        d.panel(pi);
        if let Some(f) = flash.filter(|_| pulse > 0.0) {
            // The light: the plate's colour as a soft band crossing the row (shader kind 5), the rows coming in one after
            // another, as bright as the pulse is high.
            let enter = ((f.age - i as f32 * look.copy.stagger) / look.copy.row_in.max(1e-3)).clamp(0.0, 1.0);
            let lit = look.copy.light * pulse * enter;
            if lit > 0.0 {
                pi.fill_d[3] = lit;
                pi.fill_l[3] = lit;
                pi.fx[3] = 5.0;
                pi.fx2[2] = (f.age / sweep).clamp(0.0, 1.0);
                d.panel(pi);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the corners a touching neighbour row covers go square, so stacked rows read as one shape and a row across a gap
    /// (a paragraph break) stands alone.
    #[test]
    fn only_corners_a_touching_neighbour_covers_are_square() {
        let p = |y: f32, x0, x1| Plate { x0, y0: y, x1, y1: y + 1.0, square: 0 };
        let mut v = [p(0.0, 0.0, 10.0), p(1.0, 0.0, 5.0), p(3.0, 0.0, 30.0)];
        join(&mut v);
        assert_eq!(v[0].square, 8, "only the bottom-left: the row below starts as far left but ends short");
        assert_eq!(v[1].square, 1 | 2, "both top corners sit under the wider row");
        assert_eq!(v[2].square, 0, "a row across a gap keeps all four corners round");
    }

    /// A copy's pulse peaks at 1 and is back exactly at rest within a second on the scene's spring (omega 18, zeta 1), so
    /// the frame goes idle after it.
    #[test]
    fn a_copy_pulse_peaks_at_one_and_settles() {
        use harness_native_api::spring::Tune;
        let tune = Tune { omega: 18.0, zeta: 1.0 };
        let mut f = CopyFlash::kicked(tune.omega);
        let (mut peak, mut t, dt) = (0.0f32, 0.0f32, 1.0 / 120.0);
        while !f.spring.settled(0.0) && t < 2.0 {
            f.spring.step(0.0, tune, dt);
            peak = peak.max(f.amount());
            t += dt;
        }
        assert!((peak - 1.0).abs() < 0.01, "peak {peak}");
        assert!(t < 1.0, "settled after {t}s");
    }
}
