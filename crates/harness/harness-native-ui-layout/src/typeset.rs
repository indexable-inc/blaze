//! The host's side of text layout: write `Intrinsic` on every `Text` from a typesetter, so the layout module (which cannot
//! shape text) finds each text's size in the world. Wrapping depends on the width layout gives, so a text is measured at
//! the width it was last laid out at; [`typeset`] says how many changed so the host runs one more frame until none do.
use harness_native_flecs::prelude::*;
use harness_native_ui_layout_api::{Intrinsic, Padding};
use harness_native_ui_look_api::TextStyle;
use harness_native_ui_node_api::Laid;
use harness_native_ui_text_api::{MarkdownText, OneLine, Text, Truncate, TruncateMode};

/// The host's typesetter, seen from layout: how wide a string runs on one line and how tall it is wrapped at a width. `node`
/// is the entity the text is on, for a typesetter that keeps its shaping per node.
pub trait Measure {
    fn width(&mut self, node: u64, text: &str, style: &TextStyle) -> f32;
    fn height(&mut self, node: u64, text: &str, style: &TextStyle, width: f32) -> f32;
    /// The first and last lines' baselines from the top, wrapped at `width` (0: the text has none).
    fn baselines(&mut self, _node: u64, _text: &str, _style: &TextStyle, _width: f32) -> [f32; 2] {
        [0.0; 2]
    }
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

    fn baselines(&mut self, node: u64, text: &str, style: &TextStyle, width: f32) -> [f32; 2] {
        let h = self.height(node, text, style, width);
        let line = Self::line(style);
        [line * 0.78, h - line * 0.22]
    }
}

/// A `Text` node and what the typesetter needs to size it, read from the world.
pub struct Wanted {
    pub id: u64,
    pub text: String,
    pub style: TextStyle,
    /// The width layout last gave the node, less its horizontal padding (none before layout has run).
    pub at: Option<f32>,
    pub now: Intrinsic,
    /// It is one line cut to fit (`Truncate`, or `OneLine` as a `Tail` cut): measured at its one-line width whatever
    /// width layout gave it.
    pub truncate: Option<TruncateMode>,
    /// `truncate.is_some()`: one line tall at any width (the host cuts it with an ellipsis when it draws).
    pub one_line: bool,
    /// Its words are inline markdown (`MarkdownText`): the host sets them without their markers.
    pub markdown: bool,
}

impl Wanted {
    /// The node's `Intrinsic`: its one-line width, and its height at the width layout last gave it (at its own one-line
    /// width before layout has run).
    pub fn measure(&self, m: &mut dyn Measure) -> Intrinsic {
        let w = m.width(self.id, &self.text, &self.style);
        let at = if self.truncate.is_some() { w } else { self.at.unwrap_or(w) };
        let [base, last] = m.baselines(self.id, &self.text, &self.style, at);
        Intrinsic { w, h: m.height(self.id, &self.text, &self.style, at), at, base, last }
    }
}

/// Every `Text` node (one with an `Intrinsic`) as [`Wanted`]. Read first, write after with [`apply`], so a typesetter that
/// borrows the host mutably is not held across the world.
#[must_use]
pub fn wanted(world: WorldRef<'_>) -> Vec<Wanted> {
    let mut out = Vec::new();
    world.new_query::<(&Text, &Intrinsic)>().each_entity(|e, (t, now)| {
        let pad = e.try_cloned::<&Padding>().map_or(0.0, |p| p.l + p.r);
        let one = e.has(OneLine::id()) || e.has(Truncate::id());
        out.push(Wanted {
            id: *e.id(),
            // A one-line node is measured as it draws: its first line only (`harness_native_ui_text_api::one_line`).
            text: if one { harness_native_ui_text_api::one_line(t.s.as_str()).into_owned() } else { t.s.as_str().to_owned() },
            style: e.try_cloned::<&TextStyle>().unwrap_or_default(),
            at: e.try_cloned::<&Laid>().filter(|l| l.w > 0.0).map(|l| (l.w - pad).max(0.0)),
            now: *now,
            truncate: e.try_cloned::<&Truncate>().map(|t| t.mode).or_else(|| e.has(OneLine::id()).then_some(TruncateMode::Tail)),
            one_line: one,
            markdown: e.has(MarkdownText::id()),
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

/// `text` cut to fit `room` points with an ellipsis where `mode` says, as wide as `width` measures (the host's typesetter):
/// the most characters that still fit, found by bisection. Text that fits is returned as it is.
pub fn elide(text: &str, mode: TruncateMode, room: f32, mut width: impl FnMut(&str) -> f32) -> String {
    if width(text) <= room {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let cut = |keep: usize| -> String {
        let keep = keep.min(chars.len());
        let s = |r: &[char]| r.iter().collect::<String>();
        match mode {
            TruncateMode::Tail => format!("{}\u{2026}", s(&chars[..keep]).trim_end()),
            TruncateMode::Head => format!("\u{2026}{}", s(&chars[chars.len() - keep..]).trim_start()),
            TruncateMode::Middle => {
                let front = keep.div_ceil(2);
                format!("{}\u{2026}{}", s(&chars[..front]).trim_end(), s(&chars[chars.len() - (keep - front)..]).trim_start())
            }
        }
    };
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if width(&cut(mid)) <= room {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    // Not one character fits: nothing, rather than an ellipsis standing for all of it.
    if lo == 0 { String::new() } else { cut(lo) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten points a character: in 60 points, five characters and the ellipsis.
    fn w(s: &str) -> f32 {
        s.chars().count() as f32 * 10.0
    }

    #[test]
    fn elide_keeps_what_fits_at_each_end() {
        assert_eq!(elide("harness-native", TruncateMode::Tail, 60.0, w), "harne\u{2026}");
        assert_eq!(elide("harness-native", TruncateMode::Head, 60.0, w), "\u{2026}ative");
        assert_eq!(elide("harness-native", TruncateMode::Middle, 60.0, w), "har\u{2026}ve");
        assert_eq!(elide("short", TruncateMode::Tail, 60.0, w), "short");
        assert_eq!(elide("harness", TruncateMode::Tail, 15.0, w), "");
        // Spaces at the cut are dropped, not counted against the room.
        assert_eq!(elide("ab cdefgh", TruncateMode::Tail, 40.0, w), "ab\u{2026}");
    }
}
