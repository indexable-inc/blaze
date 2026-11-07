//! Words re-wrapping live as their width moves (a cmd+S fold, a resize, a column growing). Layout gives a text the width
//! its box has every frame, and the painter only breaks it again there ([`Layout::wrap`]: a greedy fit over advances shaped
//! once, the lines cut from their paragraphs' glyphs), so the words follow the box on every frame of the motion. Here the
//! glyphs a new break moves glide instead of jumping (FLIP over glyphs): one that stays on its line (the same line of its
//! paragraph) but moves along it, because a word came onto or left the line before it, slides there from where it was
//! drawn; one that a break moved to another line fades in where it now is. Nothing glides up or down: the lines' places
//! are layout's, and a glyph sliding between lines would cross the words already on them. A word's glyphs move by the
//! same amount at once, so it slides as one.
//!
//! A node's glyphs carry over only while its text is the one it was drawn with, or that text with more after it (words
//! streaming in); any other edit lands at once. The glides run on `ui.spring.reflow` (prims.flecs); under Reduce Motion
//! the words land at once. Sans IO: times are the frame clock's, handed in.
use std::collections::HashMap;
use std::rc::Rc;

use harness_native_flecs::prelude::*;
use harness_native_ui_motion_api::Motion;
use harness_native_ui_spring_api::{Spring, Tune};

use crate::text::{Layout, Shaped};

/// The scene's reflow spring (prims.flecs).
pub const SPRING: &str = "ui::spring::reflow";

/// The spring the glides run on, or None under Reduce Motion (the words land at once). The scene is compiled in, so one
/// without the spring is a bug: this panics naming it.
#[must_use]
pub fn tune(w: WorldRef<'_>) -> Option<Tune> {
    if harness_native_api::reduced(w) {
        return None;
    }
    let m = w
        .try_lookup(SPRING)
        .unwrap_or_else(|| panic!("the scene has no {SPRING} (prims.flecs)"))
        .try_cloned::<&Motion>()
        .unwrap_or_else(|| panic!("{SPRING} has no Motion"));
    Some(Tune { omega: m.omega, zeta: m.zeta })
}

/// The longest step a glide takes in one frame: a node first painted again after a stall lands rather than leaps.
const MAX_DT: f32 = 0.1;

/// Where a glyph is drawn off its laid place: `dx` points along its line, and the share `fade` of its ink still to come (0:
/// all of it).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Shift {
    pub dx: f32,
    pub fade: f32,
}

/// A glyph where its layout puts it: the UTF-16 unit it is set from, its x along its line, and which line of its paragraph
/// that is (0 the paragraph's first).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rest {
    pub unit: usize,
    pub x: f32,
    pub row: u32,
}

/// The glyphs of `lay` where it puts them, in unit order.
#[must_use]
pub fn rests(lay: &Layout) -> Vec<Rest> {
    let n = lay.lines.iter().map(|l| l.glyphs.len()).sum();
    let mut out = Vec::with_capacity(n); // alloc: one entry a glyph, each time a node's words are laid out anew
    let mut row = 0;
    for l in &lay.lines {
        out.extend(l.glyphs.iter().map(|g| Rest { unit: g.idx, x: g.x, row }));
        row = if l.hard_break { 0 } else { row + 1 };
    }
    // Right-to-left runs set their glyphs right to left; the lookups go by unit.
    out.sort_by_key(|r| r.unit);
    out
}

/// A glyph as last drawn: where it rests, and its glide toward there.
#[derive(Clone, Copy, Debug)]
struct Glyph {
    rest: Rest,
    dx: Spring,
    fade: Spring,
}

impl Glyph {
    fn at(rest: Rest) -> Self {
        Self { rest, dx: Spring::at(0.0), fade: Spring::at(0.0) }
    }

    fn moving(&self) -> bool {
        self.dx != Spring::at(0.0) || self.fade != Spring::at(0.0)
    }
}

/// A node's glyphs, as they glide: `was` (in unit order, as last drawn and stepped to now) placed again at `now` (in unit
/// order): one whose unit was drawn on the same line of its paragraph keeps where it was drawn, its glide going on from
/// there; one drawn on another line starts unseen where it now is and fades in; one not drawn before (`carries` false:
/// the text is another) rests.
fn place(was: &[Glyph], now: &[Rest], carries: bool) -> Vec<Glyph> {
    let find = |unit: usize| {
        let k = was.partition_point(|g| g.rest.unit < unit);
        was.get(k).filter(|g| g.rest.unit == unit)
    };
    now.iter() // alloc: the node's glyphs, each time its words are laid out anew
        .map(|&rest| match find(rest.unit).filter(|_| carries) {
            Some(o) if o.rest.row == rest.row => {
                let dx = Spring { x: o.rest.x + o.dx.x - rest.x, v: o.dx.v };
                let dx = if dx.settled(0.0) { Spring::at(0.0) } else { dx };
                Glyph { rest, dx, fade: o.fade }
            }
            Some(_) => Glyph { rest, dx: Spring::at(0.0), fade: Spring::at(1.0) },
            None => Glyph::at(rest),
        })
        .collect()
}

/// Every glide in `glyphs` a frame `dt` seconds on toward rest; whether one still moves.
fn step(glyphs: &mut [Glyph], tune: Tune, dt: f32) -> bool {
    let mut moving = false;
    for g in glyphs.iter_mut().filter(|g| g.moving()) {
        for s in [&mut g.dx, &mut g.fade] {
            s.step(0.0, tune, dt);
            if s.settled(0.0) {
                *s = Spring::at(0.0);
            }
        }
        moving |= g.moving();
    }
    moving
}

/// One node's words as last drawn.
struct Node {
    shaped: Rc<Shaped>,
    /// The width they were broken at, as bits.
    width: u32,
    /// Its glyphs as last drawn, in unit order: where the next layout's glides start from.
    glyphs: Vec<Glyph>,
    /// Whether every glyph rests where `shaped` broken at `width` puts it.
    rest: bool,
    time: f32,
    tick: u64,
}

/// Every words node painted lately, by entity: what it was drawn as, and its glyphs' glides.
#[derive(Default)]
pub struct Reflows {
    nodes: HashMap<u64, Node>,
    /// Frames ended ([`Reflows::end_frame`]): a node not painted in the last one is forgotten.
    tick: u64,
}

/// A node's glyphs' shifts this frame, in unit order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shifts(Vec<(usize, Shift)>);

impl Shifts {
    /// The shift of the glyph set from `unit`, or of the one before it (a decoration starting on a unit with no glyph of
    /// its own, inside a ligature, goes with its glyph).
    #[must_use]
    pub fn at(&self, unit: usize) -> Shift {
        let k = self.0.partition_point(|g| g.0 <= unit);
        k.checked_sub(1).map_or_else(Shift::default, |k| self.0[k].1)
    }
}

impl Reflows {
    /// Node `id` painted as `lay` at frame time `time`: how its glyphs are shifted off their laid places, or None while
    /// they all rest there. `tune` (read only when the words moved: [`tune`]) is None under Reduce Motion. A node first
    /// seen, or drawn with other words, rests.
    pub fn note(&mut self, id: u64, lay: &Layout, time: f32, tune: impl FnOnce() -> Option<Tune>) -> Option<Shifts> {
        let tick = self.tick;
        let (shaped, width) = lay.source();
        let width = width.to_bits();
        let Some(n) = self.nodes.get_mut(&id) else {
            let glyphs = rests(lay).into_iter().map(Glyph::at).collect();
            self.nodes.insert(id, Node { shaped, width, glyphs, rest: true, time, tick });
            return None;
        };
        n.tick = tick;
        let dt = (time - n.time).clamp(0.0, MAX_DT);
        n.time = time;
        let same = Rc::ptr_eq(&n.shaped, &shaped) && n.width == width;
        if same && n.rest {
            return None;
        }
        let Some(tune) = tune() else {
            n.glyphs = rests(lay).into_iter().map(Glyph::at).collect();
            (n.shaped, n.width, n.rest) = (shaped, width, true);
            return None;
        };
        let mut moving = step(&mut n.glyphs, tune, dt);
        if !same {
            let carries = Rc::ptr_eq(&n.shaped, &shaped) || shaped.extends(&n.shaped);
            n.glyphs = place(&n.glyphs, &rests(lay), carries);
            moving = n.glyphs.iter().any(Glyph::moving);
            (n.shaped, n.width) = (shaped, width);
        }
        n.rest = !moving;
        if !moving {
            return None;
        }
        Some(Shifts(n.glyphs.iter().map(|g| (g.rest.unit, Shift { dx: g.dx.x, fade: g.fade.x })).collect())) // alloc: the glyphs' shifts, handed to the glyph loop while they move
    }

    /// After a frame: nodes it did not paint are forgotten (a row scrolled out of view comes back at rest), and whether a
    /// glyph still glides (the host draws another frame while one does).
    pub fn end_frame(&mut self) -> bool {
        let tick = self.tick;
        self.nodes.retain(|_, n| n.tick == tick);
        self.tick = tick.wrapping_add(1);
        self.nodes.values().any(|n| !n.rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TUNE: Tune = Tune { omega: 34.0, zeta: 1.0 };

    fn rest(unit: usize, x: f32, row: u32) -> Rest {
        Rest { unit, x, row }
    }

    /// "ab cd" on one line, narrowed so "cd" breaks onto a second line, while the paragraph after it ("ef") gains a word
    /// before it on its own first line: the "cd" glyphs fade in where they now are, the "ef" glyphs slide from where they
    /// were drawn (30 points left of their new place), and "ab" stays put. A glide that was running goes on from where it
    /// is drawn, not from where its glyph rested.
    #[test]
    fn a_glyph_on_its_line_slides_and_one_on_another_line_fades_in() {
        let was: Vec<Glyph> =
            [rest(0, 0.0, 0), rest(1, 8.0, 0), rest(3, 24.0, 0), rest(4, 32.0, 0), rest(6, 0.0, 0), rest(7, 8.0, 0)]
                .into_iter()
                .map(Glyph::at)
                .collect();
        let now = [rest(0, 0.0, 0), rest(1, 8.0, 0), rest(3, 0.0, 1), rest(4, 8.0, 1), rest(6, 30.0, 0), rest(7, 38.0, 0)];
        let g = place(&was, &now, true);
        let shift = |k: usize| (g[k].dx.x, g[k].fade.x);
        assert_eq!([shift(0), shift(1)], [(0.0, 0.0); 2]);
        assert_eq!([shift(2), shift(3)], [(0.0, 1.0); 2]);
        assert_eq!([shift(4), shift(5)], [(-30.0, 0.0); 2]);
        // Halfway there and moving, then placed again: it carries on from where it is drawn, as fast.
        let mut mid = g[4];
        mid.dx = Spring { x: -12.0, v: 90.0 };
        let again = place(&[mid], &[rest(6, 40.0, 0)], true);
        assert_eq!(again[0].dx, Spring { x: 30.0 + -12.0 - 40.0, v: 90.0 });
        // Other words (an edit, not more of the same text): every glyph rests where it is laid.
        assert!(place(&was, &now, false).iter().all(|g| !g.moving()));
    }

    /// A slide and a fade-in settle on the spring in a few frames, ending exactly at rest.
    #[test]
    fn glides_settle_at_rest() {
        let mut g = vec![Glyph { rest: rest(0, 0.0, 0), dx: Spring::at(-30.0), fade: Spring::at(1.0) }];
        let mut frames = 0;
        while step(&mut g, TUNE, 1.0 / 120.0) {
            frames += 1;
            assert!(frames < 120, "still gliding after a second: {:?}", g[0]);
        }
        assert!(!g[0].moving());
    }

    #[test]
    fn a_shift_belongs_to_the_glyph_at_or_before_its_unit() {
        let s = Shifts(vec![(0, Shift { dx: 1.0, fade: 0.0 }), (3, Shift { dx: 2.0, fade: 0.5 })]);
        assert_eq!((s.at(0).dx, s.at(2).dx, s.at(3).dx, s.at(9).fade), (1.0, 1.0, 2.0, 0.5));
    }
}
