//! The one layout pass, a hot-swappable module. A single system runs on every root `Node` (a node whose parent is not a
//! Node), gathers its subtree into the pure solver ([`solve`]), and writes `Laid` back where it changed; a root whose inputs
//! are what they were when it was last solved is left as it is. It keeps nothing else: text sizes come from the `Intrinsic`
//! component the host's typesetter writes ([`typeset`]), the room from the `Viewport` singleton. Overlays (`(AnchoredTo, node)`) are laid out as roots and placed against the anchor's `Laid` of the last
//! frame (one frame of lag while the anchor moves).
use std::collections::HashMap;

use harness_native_flecs::prelude::*;
use harness_native_ui_layout_api::{
    AlignSelf, AlignmentGuide, AnchoredTo, Aspect, Background, Centre, Collapse, FixedSize, Flex, FlexAxis, Grid, GridColumn, GridColumns, GridRow, Intrinsic, Lazy,
    LazyIndex, LazyRange, LayoutPriority, Overlay, Padding, SafeAreaInset, SafeAreaPadding, ScrollState, ShrinkFirst, Side, Size, Sizing, Viewport,
    ViewThatFits,
};
use harness_native_ui_node_api::{Clip, Hidden, Hosted, Laid, Node, Offscreen, Placed};

pub mod solve;
pub mod typeset;
pub use solve::{GridSpec, Item, LazySpec, Role, anchored, lazy_range, shift, solve};
pub use typeset::{Estimate, Measure, elide, typeset};

/// Read one node's layout components into an [`Item`] (inherited values included).
fn read(e: EntityView<'_>) -> Item {
    let size = e.try_cloned::<&Size>().unwrap_or_default();
    let pad = e.try_cloned::<&Padding>().unwrap_or_default();
    let intr = e.try_cloned::<&Intrinsic>().unwrap_or_default();
    let scroll = e.try_cloned::<&ScrollState>().unwrap_or_default();
    let fixed = e.try_cloned::<&FixedSize>().unwrap_or_default();
    let safe = e.try_cloned::<&SafeAreaInset>().unwrap_or_default();
    let safe_pad = e.try_cloned::<&SafeAreaPadding>().unwrap_or_default();
    let role = if e.has(Background::id()) {
        Role::Background
    } else if e.has(Overlay::id()) {
        Role::Overlay
    } else {
        Role::Flow
    };
    let grid = e.try_cloned::<&Grid>().map(|g| {
        // Its columns are the GridColumn children (not Nodes), in the order they were declared.
        let mut cols: Vec<(u64, GridColumn)> = Vec::new();
        e.each_child(|c| {
            if let Some(col) = c.try_cloned::<&GridColumn>() {
                cols.push((*c.id(), col));
            }
        });
        cols.sort_by_key(|(id, _)| *id);
        GridSpec { hgap: g.hgap, vgap: g.vgap, align: g.align, valign: g.valign, cols: cols.into_iter().map(|(_, c)| (c.align, c.w)).collect() }
    });
    Item {
        id: *e.id(),
        size: [size.w, size.h],
        flex: e.try_cloned::<&Flex>().unwrap_or_default(),
        pad: [pad.l, pad.r, pad.t, pad.b],
        aspect: e.try_cloned::<&Aspect>().map_or(0.0, |a| a.ratio),
        intrinsic: [intr.w, intr.h],
        baseline: [intr.base, intr.last],
        scroll: [scroll.x, scroll.y],
        scrolls: e.has(ScrollState::id()),
        centre: e.try_cloned::<&Centre>().map(|c| c.at),
        align_self: e.try_cloned::<&AlignSelf>().map(|a| a.align),
        guide: e.try_cloned::<&AlignmentGuide>().map_or([0.0; 2], |g| [g.dx, g.dy]),
        // `ShrinkFirst` (ui.Lead) is a priority below the default 0, unless the node says its own.
        priority: e.try_cloned::<&LayoutPriority>().map_or(if e.has(ShrinkFirst::id()) { -1.0 } else { 0.0 }, |p| p.value),
        fixed: [fixed.h, fixed.v],
        role,
        grid,
        grid_row: e.has(GridRow::id()),
        adaptive: e.try_cloned::<&GridColumns>().map(|g| [g.min, g.max]),
        lazy: e.try_cloned::<&Lazy>().map(|l| LazySpec { count: l.count, row: l.row, overscan: l.overscan }),
        lazy_index: e.try_cloned::<&LazyIndex>().map(|l| l.i),
        fits: e.has(ViewThatFits::id()),
        clip: e.has(Clip::id()),
        safe: [safe.l, safe.r, safe.t, safe.b],
        safe_pad: [safe_pad.l, safe_pad.r, safe_pad.t, safe_pad.b],
        ..Item::default()
    }
}

/// Gather the Node subtree under `e` into `items`, returning its index. Child order is flecs's child order, which is
/// layout order when the container carries `OrderedChildren`. A `Collapse` node (placed, so a root of its own) is a
/// stand-in here: a fixed box its laid size, `k` of it along the main axis; `ghosts` gets its index and that size.
fn gather<'a>(e: EntityView<'a>, items: &mut Vec<Item>, ghosts: &mut Vec<(usize, Laid)>) -> usize {
    let at = items.len();
    items.push(read(e));
    let main = usize::from(items[at].flex.axis != FlexAxis::Row);
    for c in harness_native_ui_node_api::children(e) {
        // A placed node is a root of its own, not a part of this flow; a collapsing one keeps a share of its spot.
        if c.has(Placed::id()) {
            let (Some(k), Some(l)) = (c.try_cloned::<&Collapse>(), c.try_cloned::<&Laid>()) else { continue };
            if c.has(Background::id()) || c.has(Overlay::id()) {
                continue;
            }
            let mut size = [Sizing::fixed(l.w), Sizing::fixed(l.h)];
            size[main] = Sizing::fixed(collapsed([l.w, l.h][main], k.k));
            let i = items.len();
            ghosts.push((i, l));
            items[at].children.push(i);
            items.push(Item { id: *c.id(), size, ..Item::default() });
            continue;
        }
        let i = gather(c, items, ghosts);
        items[at].children.push(i);
    }
    at
}

/// How long a collapsing node stands in its parent's flow: `k` (clamped to 0..1, a spring overshoots) of `len`.
fn collapsed(len: f32, k: f32) -> f32 {
    len * k.clamp(0.0, 1.0)
}

/// Fold `words` into the running hash `h`.
fn mix(h: u64, words: impl IntoIterator<Item = u64>) -> u64 {
    words.into_iter().fold(h, |h, v| (h ^ v).wrapping_mul(0x0100_0000_01b3))
}

/// What a root's places are made from: every gathered node's inputs and the tree's shape, the room, where the root was given
/// and the anchor it opens against. The same inputs give the same places.
fn inputs(items: &[Item], room: [f32; 2], given: Option<Laid>, anchor: Option<(Laid, Side)>) -> u64 {
    let rect = |l: Laid| [l.x, l.y, l.w, l.h].map(|v| u64::from(v.to_bits()));
    let mut h = mix(0xcbf2_9ce4_8422_2325, room.map(|v| u64::from(v.to_bits())));
    h = mix(h, given.map(rect).into_iter().flatten());
    if let Some((a, side)) = anchor {
        h = mix(h, rect(a).into_iter().chain([side.edge as u64, u64::from(side.dx.to_bits()), u64::from(side.dy.to_bits())]));
    }
    for it in items {
        let sizing = |s: Sizing| [s.kind as u64, u64::from(s.value.to_bits()), u64::from(s.min.to_bits()), u64::from(s.max.to_bits())];
        let f = it.flex;
        h = mix(h, [it.id, f.axis as u64, u64::from(f.gap.to_bits()), f.align as u64, f.justify as u64, u64::from(f.wrap)]);
        h = mix(h, sizing(it.size[0]).into_iter().chain(sizing(it.size[1])));
        h = mix(h, it.pad.iter().chain(&it.intrinsic).chain(&it.scroll).chain([&it.aspect]).map(|v| u64::from(v.to_bits())));
        h = mix(h, [it.centre.map_or(u64::MAX, |c| u64::from(c.to_bits())), it.children.len() as u64]);
        h = mix(h, it.children.iter().map(|&c| c as u64));
        h = mix(h, extras(it));
    }
    h
}

/// The SwiftUI-vocabulary inputs of one item (alignment, priority, grid, lazy, safe area), as words for [`inputs`].
fn extras(it: &Item) -> Vec<u64> {
    let f = |v: f32| u64::from(v.to_bits());
    let mut v = vec![
        it.align_self.map_or(u64::MAX, |a| a as u64),
        f(it.guide[0]),
        f(it.guide[1]),
        f(it.priority),
        u64::from(it.fixed[0]) | u64::from(it.fixed[1]) << 1 | u64::from(it.grid_row) << 2 | u64::from(it.fits) << 3 | u64::from(it.clip) << 4 | u64::from(it.scrolls) << 5,
        it.role as u64,
        f(it.baseline[0]),
        f(it.baseline[1]),
        it.lazy_index.map_or(u64::MAX, u64::from),
    ];
    v.extend(it.safe.iter().map(|&s| f(s)));
    v.extend(it.safe_pad.iter().map(|&b| u64::from(b)));
    v.extend(it.adaptive.into_iter().flatten().map(f));
    v.extend(it.lazy.into_iter().flat_map(|l| [u64::from(l.count), f(l.row), u64::from(l.overscan)]));
    if let Some(g) = &it.grid {
        v.extend([f(g.hgap), f(g.vgap), g.align as u64, g.valign as u64]);
        v.extend(g.cols.iter().flat_map(|(a, s)| [*a as u64, s.kind as u64, f(s.value), f(s.min), f(s.max)]));
    }
    v
}

/// Create the layout system in the module scope; deleting that scope removes it before unloading.
pub fn install(world: WorldRef<'_>) {
    // Each root's inputs when it was last solved: a root whose inputs are the same keeps the places it has. Layout runs more
    // than once a frame (the host lays its views out again once it has synced them), and most roots are still.
    let mut solved: HashMap<u64, u64> = HashMap::new();
    // In its own phase, after everything that sizes nodes. It writes every node's Laid (and LazyRange; its Hidden tags go
    // in the same merge) through the stage; `writes` makes flecs merge them before the systems after it in the same pass
    // (hits, matched geometry, the draw pass), so none of them reads the last pass's places. Without it (and with flecs
    // ordering a phase by recycled entity ids) a zoom drew a frame of the old layout and the selection pill chased it.
    world.system_named::<&Node>("UiLayout").without(Hosted::id()).without(Offscreen::id()).phase(Phase::Layout).writes(Laid::id()).writes(LazyRange::id()).each_iter(move |it, row, _| {
        let e = it.entity(row);
        let placed = e.has(Placed::id());
        if !placed && e.parent().is_some_and(|p| p.has(Node::id())) {
            return;
        }
        let w = it.world();
        let room = w.try_get::<&Viewport>(|v| [v.w, v.h]).unwrap_or([0.0; 2]);
        let mut items = Vec::new();
        let mut ghosts = Vec::new();
        let root = gather(e, &mut items, &mut ghosts);
        let anchor = e.target(AnchoredTo::id(), 0).and_then(|a| a.try_cloned::<&Laid>());
        // A placed node with nothing to open against keeps the place and size it was given: its children go inside it.
        let given = e.try_cloned::<&Laid>().filter(|_| placed && anchor.is_none());
        if let Some(l) = given {
            items[root].size = [Sizing::fixed(l.w), Sizing::fixed(l.h)];
        }
        let side = anchor.map(|a| (a, e.try_cloned::<&Side>().unwrap_or_default()));
        let key = inputs(&items, room, given, side);
        if solved.get(&*e.id()) == Some(&key) {
            return;
        }
        if solved.len() > 4096 {
            solved.clear();
        }
        solved.insert(*e.id(), key);
        solve(&mut items, root, room);
        if let Some(l) = given {
            shift(&mut items, root, l.x, l.y);
        }
        if let Some((anchor, side)) = side {
            let size = [items[root].out.w, items[root].out.h];
            let at = anchored(anchor, size, side.edge, [side.dx, side.dy], room);
            shift(&mut items, root, at[0], at[1]);
        }
        // A stand-in moves its node with the flow; the node keeps its own size (its own root lays its children out in it).
        for &(i, l) in &ghosts {
            let o = items[i].out;
            items[i].out = Laid { x: o.x, y: o.y, w: l.w, h: l.h };
        }
        for item in &items {
            let ent = w.entity_from_id(item.id);
            if ent.try_cloned::<&Laid>() != Some(item.out) {
                ent.set(item.out);
            }
            if item.hidden != ent.has(Hidden::id()) {
                if item.hidden {
                    ent.add(Hidden::id());
                } else {
                    ent.remove(Hidden::id());
                }
            }
            if let Some((first, last)) = item.range {
                let r = LazyRange { first, last };
                if ent.try_cloned::<&LazyRange>() != Some(r) {
                    ent.set(r);
                }
            }
        }
    });
}

harness_native_api::module!(
    "layout",
    install,
    [harness_native_api::API_LAYOUTS, harness_native_ui_node_api::API_LAYOUTS, harness_native_ui_layout_api::API_LAYOUTS, harness_native_ui_look_api::API_LAYOUTS]
);

#[cfg(test)]
mod tests {
    use super::collapsed;

    /// A spring that overshoots past 0 or 1 still stands between none and all of the node.
    #[test]
    fn collapsed_clamps_the_share() {
        assert_eq!(collapsed(28.0, 0.5), 14.0);
        assert_eq!(collapsed(28.0, -0.1), 0.0);
        assert_eq!(collapsed(28.0, 1.2), 28.0);
    }
}
