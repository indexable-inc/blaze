//! The host's side of text layout: write `Intrinsic` on every `Text` from a typesetter, so the layout module (which cannot
//! shape text) finds each text's size in the world. Wrapping depends on the width layout gives, so a text is measured at
//! the width it was last laid out at; [`typeset`] says how many changed so the host runs one more frame until none do.
use harness_native_flecs::prelude::*;
use harness_native_ui_layout_api::{Intrinsic, Padding};
use harness_native_ui_look_api::TextStyle;
use harness_native_ui_node_api::Laid;
use harness_native_ui_text_api::{OneLine, Text};

/// The host's typesetter, seen from layout: how wide a string runs on one line and how tall it is wrapped at a width. `node`
/// is the entity the text is on, for a typesetter that keeps its shaping per node.
pub trait Measure {
    fn width(&mut self, node: u64, text: &str, style: &TextStyle) -> f32;
    fn height(&mut self, node: u64, text: &str, style: &TextStyle, width: f32) -> f32;
}

/// A monospace estimate, for tests and for a host with no typesetter yet.
#[derive(Default)]
pub struct Estimate;

impl Estimate {
    fn em(style: &TextStyle) -> f32 {
        if style.size > 0.0 { style.size } else { 14.0 }
    }

    fn line(style: &TextStyle) -> f32 {
        if style.line > 0.0 { style.line } else { Self::em(style) * 1.3 }
    }
}

impl Measure for Estimate {
    fn width(&mut self, _: u64, text: &str, style: &TextStyle) -> f32 {
        text.lines().map(|l| l.chars().count()).max().unwrap_or(0) as f32 * Self::em(style) * 0.55
    }

    fn height(&mut self, _: u64, text: &str, style: &TextStyle, width: f32) -> f32 {
        let per = (width / (Self::em(style) * 0.55)).floor().max(1.0) as usize;
        let rows: usize = text.split('\n').map(|l| l.chars().count().div_ceil(per).max(1)).sum();
        rows as f32 * Self::line(style)
    }
}

/// A `Text` node and what the typesetter needs to size it, read from the world.
pub struct Wanted {
    pub id: u64,
    pub text: String,
    pub style: TextStyle,
    /// The width layout last gave the node, less its horizontal padding (none before layout has run).
    pub at: Option<f32>,
    /// A `OneLine` text: one line tall at any width (the host cuts it with an ellipsis when it draws).
    pub one_line: bool,
    pub now: Intrinsic,
}

impl Wanted {
    /// The node's `Intrinsic`: its one-line width, and its height at the width layout last gave it (at its own one-line
    /// width before layout has run).
    pub fn measure(&self, m: &mut dyn Measure) -> Intrinsic {
        let w = m.width(self.id, &self.text, &self.style);
        let at = self.at.unwrap_or(w);
        let h = m.height(self.id, &self.text, &self.style, if self.one_line { f32::MAX } else { at });
        Intrinsic { w, h, at }
    }
}

/// Every `Text` node (one with an `Intrinsic`) as [`Wanted`]. Read first, write after with [`apply`], so a typesetter that
/// borrows the host mutably is not held across the world.
#[must_use]
pub fn wanted(world: WorldRef<'_>) -> Vec<Wanted> {
    let mut out = Vec::new();
    world.new_query::<(&Text, &Intrinsic)>().each_entity(|e, (t, now)| {
        let pad = e.try_cloned::<&Padding>().map_or(0.0, |p| p.l + p.r);
        out.push(Wanted {
            id: *e.id(),
            text: t.s.as_str().to_owned(),
            style: e.try_cloned::<&TextStyle>().unwrap_or_default(),
            at: e.try_cloned::<&Laid>().filter(|l| l.w > 0.0).map(|l| (l.w - pad).max(0.0)),
            one_line: e.has(OneLine::id()),
            now: *now,
        });
    });
    out
}

/// Write the measured `sizes` (one per [`Wanted`], in order) where they differ. Returns how many changed; the host runs
/// layout again after `progress` while it is not 0.
pub fn apply(world: WorldRef<'_>, wanted: &[Wanted], sizes: &[Intrinsic]) -> usize {
    let mut changed = 0;
    for (w, next) in wanted.iter().zip(sizes) {
        if *next != w.now {
            world.entity_from_id(w.id).set(*next);
            changed += 1;
        }
    }
    changed
}

/// [`wanted`], measured by `m`, [`apply`]ed: one call for a host whose typesetter does not borrow the host.
pub fn typeset(world: WorldRef<'_>, m: &mut dyn Measure) -> usize {
    let wanted = wanted(world);
    let sizes: Vec<Intrinsic> = wanted.iter().map(|w| w.measure(m)).collect();
    apply(world, &wanted, &sizes)
}
