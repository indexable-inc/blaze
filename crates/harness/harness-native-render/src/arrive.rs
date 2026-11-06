//! When each glyph of a `GlyphsArrive` node came (harness-native-ui-timeline-api): the paint pass notes each such node's
//! glyph count every frame it paints it, and a count that grew is a step, the glyphs past the old count coming at that
//! frame's time. A glyph's time is its step's; one whose step is older than the fade is at rest. Sans IO: times are the
//! frame clock's, handed in.
use std::collections::HashMap;

use harness_native_ui_timeline_api::GlyphsArrive;

/// Every `GlyphsArrive` node painted lately: its glyph count and the steps it grew by.
#[derive(Default)]
pub struct Arrivals {
    nodes: HashMap<u64, Node>,
    /// Frames ended ([`Arrivals::end_frame`]): a node not painted in the last one is forgotten.
    tick: u64,
    /// The frame time the last note was at.
    now: f32,
}

struct Node {
    seen: usize,
    /// (first glyph, when it came), in glyph order; each step runs to the next one's first glyph.
    steps: Vec<(usize, f32)>,
    dur: f32,
    tick: u64,
}

/// When a node's glyphs came, as [`Arrivals::note`] found them this frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Came(Vec<(usize, f32)>);

impl Came {
    /// When glyph `n` (reading order) came; None: long enough ago that it is at rest.
    #[must_use]
    pub fn of(&self, n: usize) -> Option<f32> {
        let k = self.0.partition_point(|s| s.0 <= n);
        k.checked_sub(1).map(|k| self.0[k].1)
    }
}

impl Arrivals {
    /// Node `id` painted with `total` glyphs at frame time `time`, fading in on `g`: when each of them came. First seen
    /// within `g.dur` of `g.born`, its glyphs came at `born`; first seen later, they are at rest. A count that shrank (its
    /// words were cut back) forgets the steps past it.
    pub fn note(&mut self, id: u64, total: usize, time: f32, g: &GlyphsArrive) -> Came {
        let tick = self.tick;
        self.now = time;
        let node = self.nodes.entry(id).or_insert_with(|| Node {
            seen: total,
            steps: if time - g.born < g.dur { vec![(0, g.born)] } else { Vec::new() },
            dur: g.dur,
            tick,
        });
        node.tick = tick;
        node.dur = g.dur;
        if total < node.seen {
            node.steps.retain(|s| s.0 < total);
        } else if total > node.seen {
            node.steps.push((node.seen, time));
        }
        node.seen = total;
        // A step at rest leaves its glyphs at rest: only the next step (or none) says when later glyphs came.
        let settled = node.steps.iter().take_while(|s| time - s.1 >= node.dur).count();
        node.steps.drain(..settled);
        Came(node.steps.clone()) // alloc: a few steps a node a frame, handed to the glyph loop
    }

    /// After a frame: nodes it did not paint are forgotten (a row scrolled out of view starts at rest when it comes back),
    /// and whether any glyph is still fading in (the host draws another frame while one is).
    pub fn end_frame(&mut self) -> bool {
        let (tick, time) = (self.tick, self.now);
        self.nodes.retain(|_, n| n.tick == tick);
        self.tick = tick.wrapping_add(1);
        self.nodes.values().any(|n| n.steps.last().is_some_and(|s| time - s.1 < n.dur))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Words growing over three frames: the glyphs each frame added came at that frame, the earlier ones keep their own
    /// time, and once the fade is past they are all at rest. A node first painted long after it began shows at rest.
    #[test]
    fn each_glyph_comes_when_its_frame_added_it() {
        let g = GlyphsArrive { dur: 0.2, by: 2.0, ease: harness_native_ui_timeline_api::Ease::Out, born: 0.0 };
        let mut a = Arrivals::default();
        let c = a.note(1, 5, 0.0, &g);
        assert_eq!(c.of(0), Some(0.0));
        a.end_frame();
        a.note(1, 9, 0.05, &g);
        a.end_frame();
        let c = a.note(1, 12, 0.1, &g);
        assert_eq!((c.of(4), c.of(5), c.of(9), c.of(11)), (Some(0.0), Some(0.05), Some(0.1), Some(0.1)));
        assert!(a.end_frame());
        let c = a.note(1, 12, 0.5, &g);
        assert_eq!(c.of(11), None);
        assert!(!a.end_frame());
        let late = a.note(2, 40, 3.0, &g);
        assert_eq!(late.of(39), None);
    }
}
