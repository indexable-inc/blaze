//! The sidebar's tree scroll: `hn.Tree` is a `ui.Scroll` driven by AppKit's scroll physics (harness-native-scroll-physics,
//! den's copy of a default `NSScrollView`'s animator and momentum), an overlay knob that shows only while it moves, a soft
//! fade at an edge with more beyond it, and the selected row kept in view (on a spring) whichever way it was selected.
use super::*;
use harness_native_scroll_physics::{Engine, Input, Mutation, Phase, SurfaceConfig, Vec2};
use harness_native_ui_draw_api::Painted;
use harness_native_ui_layout_api::ScrollState;

/// A scroll event as AppKit gives it: deltas (points when `precise`, else lines), and its gesture and momentum phases
/// (`NSEventPhase` bits).
#[derive(Clone, Copy, Debug, Default)]
pub struct Wheel {
    pub dx: f32,
    pub dy: f32,
    pub precise: bool,
    pub phase: usize,
    pub momentum: usize,
}

/// The tree's scroll: its engine, the origin it last reported, a glide to the selected row, and when it last moved.
pub struct TreeScroll {
    engine: Engine,
    clock: std::time::Instant,
    /// The origin shown (points down from the top; past 0 or the end while a rubber band holds or springs back).
    y: f32,
    /// Bumped for each origin the app sets (a glide's step): the engine takes it as a command.
    rev: usize,
    /// A glide to keep the selected row in view: its spring and where it goes.
    glide: Option<(Spring, f32)>,
    /// The selected object last kept in view.
    shown_sel: Option<u64>,
    /// App time it last moved (the knob shows for a while after).
    active_at: f32,
    viewport: f32,
    content: f32,
}

impl Default for TreeScroll {
    fn default() -> Self {
        Self {
            engine: Engine::new(config(0.0, 0.0, 0.0, 0.0, 0, 0.5), Mutation::None),
            clock: std::time::Instant::now(),
            y: 0.0,
            rev: 0,
            glide: None,
            shown_sel: None,
            active_at: -1.0e9,
            viewport: 0.0,
            content: 0.0,
        }
    }
}

fn config(
    w: f32,
    viewport: f32,
    content: f32,
    origin: f32,
    rev: usize,
    points_per_pixel: f64,
) -> SurfaceConfig {
    SurfaceConfig {
        viewport: Vec2::new(f64::from(w), f64::from(viewport)),
        content: Vec2::new(f64::from(w), f64::from(content)),
        axes: [false, true],
        predominant_axis: true,
        origin: Vec2::new(0.0, f64::from(origin)),
        command_revision: rev,
        points_per_pixel,
        frame_interval: 1.0 / 120.0,
    }
}

/// How long the knob stays after the tree last moved, then how long it takes to fade (seconds).
const KNOB_HOLD: f32 = 0.7;
const KNOB_FADE: f32 = 0.3;

impl App {
    /// A scroll: over the sidebar's tree it scrolls the tree, else it goes to the terminal under the pointer.
    pub fn scroll_wheel(&mut self, ev: Wheel, m: KeyMods) {
        let z = self.zoom();
        if self.over_tree() {
            let t = self.tree.clock.elapsed().as_secs_f64();
            let k = if ev.precise { 1.0 / z } else { 1.0 };
            let input = Input {
                delta: Vec2::new(f64::from(ev.dx * k), f64::from(ev.dy * k)),
                precise: ev.precise,
                direct: Phase::from_ns(ev.phase),
                momentum: Phase::from_ns(ev.momentum),
                timestamp: t,
            };
            self.tree.glide = None;
            self.tree.engine.wheel(input, t);
            self.tree.active_at = self.time;
            self.poke();
            return;
        }
        if self.archive_wheel(if ev.precise { ev.dy / z } else { ev.dy }, ev.precise) {
            return;
        }
        self.wheel(if ev.precise { ev.dy / z } else { ev.dy }, ev.precise, m);
    }

    /// Whether the pointer is over the sidebar's tree (and the sidebar is open).
    fn over_tree(&self) -> bool {
        let Some(l) = self.world.entity_from_id(self.ids.tree).try_cloned::<&Laid>() else {
            return false;
        };
        let (x, y) = (self.pointer.x, self.pointer.y);
        self.side_open && x >= l.x && x < l.x + l.w && y >= l.y && y < l.y + l.h
    }

    /// Step the tree's scroll one frame: measure it from the last layout, keep a newly selected row in view, advance the
    /// physics and show its origin.
    pub(super) fn step_tree_scroll(&mut self, dt: f32) {
        let w = self.world.world_ref();
        let tree = w.entity_from_id(self.ids.tree);
        let Some(l) = tree.try_cloned::<&Laid>().filter(|l| l.h > 0.0) else { return };
        let kids = harness_native_ui_node_api::children(tree);
        let rows: Vec<Laid> =
            kids.iter().filter_map(|c| c.try_cloned::<&Laid>()).filter(|r| r.h > 0.0).collect();
        let content = match (rows.first(), rows.last()) {
            (Some(a), Some(b)) => b.y + b.h - a.y,
            _ => 0.0,
        };
        let max = (content - l.h).max(0.0);
        // The selected row, kept in view: a glide to it when it lies past either edge (once it is laid out).
        let sel = self.selected();
        if sel != self.tree.shown_sel {
            let name = sel.map(|e| format!("select:{e}"));
            let row = kids.iter().find(|c| {
                c.try_cloned::<&Action>().is_some_and(|a| Some(a.name.as_str()) == name.as_deref())
            });
            match row.and_then(|r| r.try_cloned::<&Laid>()).filter(|r| r.h > 0.0) {
                Some(r) => {
                    self.tree.shown_sel = sel;
                    let margin = crate::look::width(w, "tree_scroll");
                    // In content points (from the tree's top, unscrolled): the row's top and foot.
                    let rt = r.y - (l.y - self.tree.y);
                    let rb = rt + r.h;
                    let at = self.tree.y;
                    let goal = if rt < at + margin {
                        Some(rt - margin)
                    } else if rb > at + l.h - margin {
                        Some(rb - l.h + margin)
                    } else {
                        None
                    };
                    if let Some(g) = goal {
                        self.tree.glide = Some((Spring::at(at), g.clamp(0.0, max)));
                    }
                }
                None if sel.is_none() => self.tree.shown_sel = None,
                None => {}
            }
        }
        let mut origin = self.tree.y;
        if let Some((mut s, goal)) = self.tree.glide.take() {
            let tune = crate::look::tune(w, "tree_scroll");
            let moving = harness_native_api::settle(&mut s, goal, tune, dt);
            origin = if moving { s.x } else { goal };
            self.tree.rev += 1;
            if moving {
                self.tree.glide = Some((s, goal));
            }
        } else if self.tree.y > max && !self.tree.engine.needs_frame() {
            // The tree got shorter under the origin: back inside it.
            origin = max;
            self.tree.rev += 1;
        }
        self.tree.viewport = l.h;
        self.tree.content = content;
        let t = self.tree.clock.elapsed().as_secs_f64();
        self.tree.engine.configure(config(
            l.w,
            l.h,
            content,
            origin,
            self.tree.rev,
            1.0 / f64::from(self.grid.scale.max(0.5)),
        ));
        self.tree.engine.advance(t);
        let y = self.tree.engine.report().y as f32;
        let moving = self.tree.engine.needs_frame() || self.tree.glide.is_some();
        if moving || (y - self.tree.y).abs() > 0.01 {
            self.tree.active_at = self.time;
        }
        self.tree.y = y;
        if tree.try_cloned::<&ScrollState>().is_none_or(|s| (s.y - y).abs() > 0.001 || s.x != 0.0) {
            tree.set(ScrollState { x: 0.0, y });
        }
        if moving || self.time - self.tree.active_at < KNOB_HOLD + KNOB_FADE {
            self.poke();
        }
    }

    /// The fades (top, foot; points) for what `p` paints when it is cut to the tree's band: soft at an edge with more of
    /// the tree beyond it.
    pub(super) fn tree_fade(&self, p: &Painted) -> Option<[f32; 2]> {
        let l = self.world.entity_from_id(self.ids.tree).try_cloned::<&Laid>()?;
        if !p.clipped || (p.clip_y - l.y).abs() > 0.5 || (p.clip_h - l.h).abs() > 0.5 {
            return None;
        }
        let fade = crate::look::width(self.world.world_ref(), "tree_fade");
        let max = (self.tree.content - self.tree.viewport).max(0.0);
        let k = |v: f32| (v / fade).clamp(0.0, 1.0) * fade;
        Some([k(self.tree.y), k(max - self.tree.y)])
    }

    /// The tree's overlay knob, while it moves and for a moment after, at its right edge.
    pub(super) fn emit_tree_knob(&self, d: &mut Draw) {
        let w = self.world.world_ref();
        let Some(l) = w.entity_from_id(self.ids.tree).try_cloned::<&Laid>() else { return };
        let (viewport, content) = (self.tree.viewport, self.tree.content);
        if content <= viewport + 0.5 || viewport <= 0.0 || !self.side_open {
            return;
        }
        let since = self.time - self.tree.active_at;
        let alpha = if since < KNOB_HOLD {
            1.0
        } else {
            1.0 - ((since - KNOB_HOLD) / KNOB_FADE).clamp(0.0, 1.0)
        };
        if alpha <= 0.0 {
            return;
        }
        let max = content - viewport;
        let knob_w = crate::look::width(w, "knob");
        let mut len = (viewport * viewport / content).max(28.0);
        // Rubber band: the knob shortens by the part pulled past an end.
        let over = if self.tree.y < 0.0 { -self.tree.y } else { (self.tree.y - max).max(0.0) };
        len = (len - over).max(knob_w * 2.0);
        let at = (self.tree.y / max).clamp(0.0, 1.0) * (viewport - len);
        let fill = crate::look::tone(w, "knob");
        let ink = rgba(if self.dark { fill.dark } else { fill.light });
        let rect = [l.x + l.w - knob_w - 1.0, l.y + at, knob_w, len];
        d.panel(panel_from(
            ink,
            [0.0; 4],
            Shape { radius: knob_w * 0.5, n: 2.0, border: 0.0, feather: 0.0 },
            rect,
            alpha,
        ));
    }
}
