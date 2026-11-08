//! The selection: one plate under the glyphs for every surface that selects (a chat's words and input through `TextEdit`,
//! a table's copied row, the terminal), and the pulse a copy plays over it. Copied from den-native (ui.rs `emit_selection`
//! and `Flash`, select.rs `join_plates`, github.com/andrewgazelka/den); its look is the scene's `ui.selection` (prims.flecs)
//! and its clock a `CopyFlash` on what was copied. den's pulse grows the shape and lays a faint band of the plate's own
//! colour along each row; here the band is a light of its own (the theme's `tone_copy_sheen`) that sweeps the whole shape
//! on a tilt, adds to the plate on a dark page, and lights the glyphs it crosses ([`Pulse`]).
use harness_native_flecs::prelude::*;
use harness_native_ui_look_api::{CopyLook, Fill, Shape};
use harness_native_ui_motion_api::CopyFlash;

use crate::draw::Draw;
use crate::pack::GlyphInst;
use crate::paint::{panel_from, rgba, snap_rect};

/// The scene prefab every selection draws from.
pub const LOOK: &str = "ui::selection";

/// The panel kind (harness.slang `panel_shade`) that draws a copy's light over a selection plate.
const LIGHT_KIND: f32 = 5.0;

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

    fn holds(&self, (x, y): (f32, f32)) -> bool {
        (self.x0..self.x1).contains(&x) && (self.y0..self.y1).contains(&y)
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

/// The selection's look, read from the scene's `ui.selection`, and whether the system's Reduce Motion is on (`still`: a
/// copy then glows in place instead of growing and sweeping).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub fill: Fill,
    pub shape: Shape,
    pub copy: CopyLook,
    pub still: bool,
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
            still: harness_native_api::reduced(w),
        }
    }

    /// The plate's colour in one appearance.
    #[must_use]
    pub fn colour(&self, dark: bool) -> [f32; 4] {
        rgba(if dark { self.fill.dark } else { self.fill.light })
    }

    /// A copy's pulse starting now: as long as this look's grow and sweep (or, still, its glow).
    #[must_use]
    pub fn kick(&self) -> CopyFlash {
        CopyFlash::kicked(self.copy.length(self.still))
    }

    /// How much of a plate that shows only while a copy plays (a table's copied row) is up `flash.age` in: as much as the
    /// grow or the light is.
    #[must_use]
    pub fn shown(&self, flash: CopyFlash) -> f32 {
        let c = &self.copy;
        if self.still {
            return c.glow_at(flash.age);
        }
        c.grow_at(flash.age).max(c.sweep_at(flash.age).map_or(0.0, |(_, lit)| lit))
    }
}

/// The bounds of the plates: `(x0, y0, x1, y1)`.
fn bounds(plates: &[Plate]) -> (f32, f32, f32, f32) {
    plates.iter().fold((f32::MAX, f32::MAX, f32::MIN, f32::MIN), |(x0, y0, x1, y1), p| {
        (x0.min(p.x0), y0.min(p.y0), x1.max(p.x1), y1.max(p.y1))
    })
}

/// A copy's grow at this moment: about the selection's middle by `scale`. The plates and the glyphs they hold grow
/// together (den grows the copied words with their plate), so the words stay on their plate through the pulse.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grow {
    pub cx: f32,
    pub cy: f32,
    pub scale: f32,
}

impl Grow {
    /// Rect `[x, y, w, h]` (points) grown about the middle.
    #[must_use]
    pub fn rect(&self, r: [f32; 4]) -> [f32; 4] {
        [self.cx + (r[0] - self.cx) * self.scale, self.cy + (r[1] - self.cy) * self.scale, r[2] * self.scale, r[3] * self.scale]
    }

    /// Where a grown point `(x, y)` was before the grow.
    fn back(&self, (x, y): (f32, f32)) -> (f32, f32) {
        (self.cx + (x - self.cx) / self.scale, self.cy + (y - self.cy) / self.scale)
    }
}

/// A copy's light at this moment: colour `rgb` at strength `a` (its look's peak times `now`, how far up it is, 0 to 1),
/// adding the share `add` of itself to what is under; the band's middle at `x` on the shape's top edge `y`, leaning back
/// `tilt` points for every point down, `band` points either side of its middle before it is gone (0: everywhere at once,
/// the glow of Reduce Motion). Glyphs it crosses take the share `ink` of its colour where it is brightest.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Light {
    rgb: [f32; 3],
    a: f32,
    now: f32,
    add: f32,
    ink: f32,
    x: f32,
    y: f32,
    band: f32,
    tilt: f32,
}

impl Light {
    /// How lit the point `(x, y)` is, 0 to 1: a smooth falloff from the band's middle to its edge.
    fn at(&self, (x, y): (f32, f32)) -> f32 {
        if self.band <= 0.0 {
            return 1.0;
        }
        let d = ((x - (self.x - self.tilt * (y - self.y))) / self.band).abs().min(1.0);
        1.0 - d * d * (3.0 - 2.0 * d)
    }
}

/// What a copy's pulse draws at this moment over one selection: its grow (also for the glyphs on it, which the caller
/// grows), and its light, laid over the plate by [`emit`] and caught by the glyphs through [`Pulse::light_glyphs`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pulse {
    pub grow: Option<Grow>,
    light: Option<Light>,
}

impl Pulse {
    /// The pulse of `flash` over a selection of `plates` (points, before the grow) in `look`, on a `dark` page or not;
    /// at rest (nothing grows, nothing lit) when it is not playing.
    #[must_use]
    pub fn of(plates: &[Plate], look: &Look, flash: Option<CopyFlash>, dark: bool) -> Self {
        let Some(f) = flash.filter(CopyFlash::live).filter(|_| !plates.is_empty()) else { return Self::default() };
        let c = &look.copy;
        let (x0, y0, x1, y1) = bounds(plates);
        let colour = rgba(if dark { c.dark } else { c.light });
        let lit_by = |now: f32, x: f32, band: f32, tilt: f32| {
            (now > 0.0).then_some(Light {
                rgb: [colour[0], colour[1], colour[2]],
                a: colour[3] * now,
                now,
                add: if dark { c.add_dark } else { c.add_light },
                ink: c.ink,
                x,
                y: y0,
                band,
                tilt,
            })
        };
        if look.still {
            return Self { grow: None, light: lit_by(c.glow_at(f.age), 0.0, 0.0, 0.0) };
        }
        // Once the grow is over (the light still crossing) the plates are back on the pixel grid, as at rest.
        let up = c.grow_at(f.age);
        let grow = (up > 0.0).then(|| Grow { cx: (x0 + x1) * 0.5, cy: (y0 + y1) * 0.5, scale: 1.0 + c.grow * up });
        // The band's middle on the top edge runs from a band before the shape's left to a band past its right plus the
        // lean, so on every row it comes in from nothing and leaves to nothing.
        let light = c.sweep_at(f.age).and_then(|(across, lit)| {
            let band = c.band.max(1.0);
            let (from, to) = (x0 - band, x1 + band + c.tilt * (y1 - y0));
            lit_by(lit, from + (to - from) * across, band, c.tilt)
        });
        Self { grow, light }
    }

    /// Whether anything plays (a grow or a light); false at rest.
    #[must_use]
    pub fn live(&self) -> bool {
        self.grow.is_some() || self.light.is_some()
    }

    /// The glyphs among `glyphs` (as drawn, grown) whose middle lies on one of `plates` (before the grow) catch the light
    /// where they are: their ink goes toward its colour by its `ink` share where it is brightest. Colour glyphs (emoji)
    /// keep their own colours.
    pub fn light_glyphs(&self, glyphs: &mut [GlyphInst], plates: &[Plate]) {
        if self.light.is_none() {
            return;
        }
        for g in glyphs {
            let at = (g.rect[0] + g.rect[2] * 0.5, g.rect[1] + g.rect[3] * 0.5);
            let was = self.grow.map_or(at, |gr| gr.back(at));
            if plates.iter().any(|p| p.holds(was)) {
                self.light_glyph(g);
            }
        }
    }

    /// One glyph (as drawn) catches the light at its middle.
    pub fn light_glyph(&self, g: &mut GlyphInst) {
        let Some(l) = self.light.filter(|_| g.kind[0] < 0.5) else { return };
        let k = l.ink * l.now * l.at((g.rect[0] + g.rect[2] * 0.5, g.rect[1] + g.rect[3] * 0.5));
        for col in [&mut g.col_d, &mut g.col_l] {
            for (c, to) in col.iter_mut().zip(l.rgb) {
                *c += (to - *c) * k;
            }
        }
    }
}

/// Draw a selection's rows (in reading order, points) as one shape of `look` in `colour` at `alpha`, joined ([`join`]).
/// Under a copy's `pulse` (from [`Pulse::of`] over the same rows) the shape grows and its light lies over each row: the
/// sweeping band, clipped to the row's corners, added to the plate or laid over it as the look says. At rest the plates
/// sit on whole device pixels at scale `sc`. Draw it before the glyphs it is under; grow those glyphs by the pulse's
/// [`Grow`] and light them with [`Pulse::light_glyphs`].
pub fn emit(d: &mut Draw, plates: &[Plate], look: &Look, colour: [f32; 4], pulse: &Pulse, alpha: f32, sc: f32) {
    if plates.is_empty() || alpha <= 0.0 {
        return;
    }
    let mut plates = plates.to_vec(); // alloc: a selection's rows, a handful a frame
    join(&mut plates);
    let scale = pulse.grow.map_or(1.0, |g| g.scale);
    for p in &plates {
        let (w, h) = (p.x1 - p.x0, p.y1 - p.y0);
        let rect = [p.x0, p.y0, w, h];
        let rect = match pulse.grow {
            Some(g) => g.rect(rect),
            None => snap_rect(rect, sc),
        };
        let shape = Shape { radius: look.shape.radius.min(h / 4.0) * scale, ..look.shape };
        let mut pi = panel_from(colour, [0.0; 4], shape, rect, alpha);
        pi.fx2[3] = f32::from(p.square);
        d.panel(pi);
        if let Some(l) = pulse.light {
            // The light (harness.slang's kind 5): the band in view points, so every row's piece is one band.
            let fill = [l.rgb[0], l.rgb[1], l.rgb[2], l.a];
            pi.fill_d = fill;
            pi.fill_l = fill;
            pi.fx[3] = LIGHT_KIND;
            pi.fx2[2] = l.add;
            pi.lens = [l.x, l.y, l.band, l.tilt];
            d.panel(pi);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_native_ui_look_api::Rgba;

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

    fn look(still: bool) -> Look {
        let white = Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.4 };
        Look {
            fill: Fill::default(),
            shape: Shape::default(),
            copy: CopyLook {
                secs: 0.3,
                rise: 0.18,
                grow: 0.025,
                sweep: 0.6,
                band: 20.0,
                tilt: 0.5,
                ink: 0.5,
                dark: white,
                light: white,
                add_dark: 1.0,
                add_light: 0.0,
            },
            still,
        }
    }

    /// A copy's grow is den's (peaking `rise` of the way into `secs`), and the whole pulse is exactly at rest at the end of
    /// the longer of the grow and the sweep, so the frame goes idle after it.
    #[test]
    fn a_copy_pulse_grows_as_dens_and_rests_at_its_end() {
        let l = look(false);
        assert!((l.copy.grow_at(0.054) - 1.0).abs() < 1e-3, "the grow peaks at 54 ms");
        let mut f = l.kick();
        assert_eq!(f.secs, 0.6, "it lasts as long as the sweep");
        f.age = 0.6;
        assert!(!f.live() && l.shown(f) == 0.0, "at rest at 600 ms");
        assert_eq!(Pulse::of(&[Plate::of(0.0, 0.0, 100.0, 20.0)], &l, Some(f), true), Pulse::default());
    }

    /// The light sweeps the whole shape: on every row it comes in from nothing at the start and leaves to nothing at the
    /// end (never popping on or off), and the rows below are lit after the ones above.
    #[test]
    fn the_light_crosses_every_row_from_nothing_to_nothing() {
        let l = look(false);
        let rows = [Plate::of(0.0, 0.0, 200.0, 20.0), Plate::of(0.0, 20.0, 200.0, 20.0), Plate::of(0.0, 40.0, 120.0, 20.0)];
        let lit = |age: f32, at: (f32, f32)| {
            let mut f = CopyFlash::kicked(0.6);
            f.age = age;
            Pulse::of(&rows, &l, Some(f), true).light.map_or(0.0, |light| light.at(at))
        };
        for at in [(0.0, 0.0), (0.0, 60.0), (200.0, 0.0), (120.0, 60.0)] {
            assert_eq!(lit(1e-4, at), 0.0, "nothing lit at the start: {at:?}");
            assert!(lit(0.6 - 1e-4, at) < 1e-3, "nothing lit at the end: {at:?}");
        }
        // Where the band's middle is on the top row a moment in, the row below is still dark.
        let first = (0..60).map(|x| x as f32).find(|&x| lit(0.2, (x, 0.0)) > 0.99);
        let x = first.expect("the band is on the top row at 200 ms");
        assert!(lit(0.2, (x, 50.0)) < lit(0.2, (x, 0.0)), "the bottom row lights after the top");
    }

    /// Under Reduce Motion nothing grows or moves: the whole shape glows at once and fades back.
    #[test]
    fn reduce_motion_glows_in_place() {
        let l = look(true);
        let mut f = l.kick();
        assert_eq!(f.secs, 0.3, "the glow lasts the grow's time");
        f.age = 0.15;
        let p = Pulse::of(&[Plate::of(0.0, 0.0, 100.0, 20.0)], &l, Some(f), true);
        assert_eq!(p.grow, None, "nothing grows");
        let light = p.light.expect("lit halfway");
        assert_eq!((light.at((0.0, 0.0)), light.at((100.0, 20.0))), (1.0, 1.0), "lit everywhere at once");
        assert!((light.a - 0.4).abs() < 1e-3, "at the look's full strength halfway");
    }
}
