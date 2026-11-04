//! The one layout pass, a hot-swappable module. A single system runs on every root `Node` (a node whose parent is not a
//! Node), gathers its subtree into the pure solver ([`solve`]), and writes `Laid` back where it changed; a root whose inputs
//! are what they were when it was last solved is left as it is. It keeps nothing else: text sizes come from the `Intrinsic`
//! component the host's typesetter writes ([`typeset`]), the room from the `Viewport` singleton. Overlays (`(AnchoredTo, node)`) are laid out as roots and placed against the anchor's `Laid` of the last
//! frame (one frame of lag while the anchor moves).
use std::collections::HashMap;

use harness_native_flecs::prelude::*;
use harness_native_ui_layout_api::{AnchoredTo, Aspect, Centre, Flex, Intrinsic, Padding, ScrollState, Side, Size, Sizing, Viewport};
use harness_native_ui_node_api::{Offscreen, Hosted, Laid, Node, Placed};

pub mod solve;
pub mod typeset;
pub use solve::{Item, anchored, shift, solve};
pub use typeset::{Estimate, Measure, typeset};

/// Read one node's layout components into an [`Item`] (inherited values included).
fn read(e: EntityView<'_>) -> Item {
    let size = e.try_cloned::<&Size>().unwrap_or_default();
    let pad = e.try_cloned::<&Padding>().unwrap_or_default();
    let intr = e.try_cloned::<&Intrinsic>().unwrap_or_default();
    let scroll = e.try_cloned::<&ScrollState>().unwrap_or_default();
    Item {
        id: *e.id(),
        size: [size.w, size.h],
        flex: e.try_cloned::<&Flex>().unwrap_or_default(),
        pad: [pad.l, pad.r, pad.t, pad.b],
        aspect: e.try_cloned::<&Aspect>().map_or(0.0, |a| a.ratio),
        intrinsic: [intr.w, intr.h],
        scroll: [scroll.x, scroll.y],
        centre: e.try_cloned::<&Centre>().map(|c| c.at),
        ..Item::default()
    }
}

/// Gather the Node subtree under `e` into `items`, returning its index. Child order is flecs's child order, which is
/// layout order when the container carries `OrderedChildren`.
fn gather<'a>(e: EntityView<'a>, items: &mut Vec<Item>) -> usize {
    let at = items.len();
    items.push(read(e));
    for c in harness_native_ui_node_api::children(e) {
        // A placed node is a root of its own, not a part of this flow.
        if c.has(Placed::id()) {
            continue;
        }
        let i = gather(c, items);
        items[at].children.push(i);
    }
    at
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
    }
    h
}

/// Create the layout system in the module scope; deleting that scope removes it before unloading.
pub fn install(world: WorldRef<'_>) {
    // Each root's inputs when it was last solved: a root whose inputs are the same keeps the places it has. Layout runs more
    // than once a frame (the host lays its views out again once it has synced them), and most roots are still.
    let mut solved: HashMap<u64, u64> = HashMap::new();
    world.system_named::<&Node>("UiLayout").without(Hosted::id()).without(Offscreen::id()).each_iter(move |it, row, _| {
        let e = it.entity(row);
        let placed = e.has(Placed::id());
        if !placed && e.parent().is_some_and(|p| p.has(Node::id())) {
            return;
        }
        let w = it.world();
        let room = w.try_get::<&Viewport>(|v| [v.w, v.h]).unwrap_or([0.0; 2]);
        let mut items = Vec::new();
        let root = gather(e, &mut items);
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
        for item in &items {
            let ent = w.entity_from_id(item.id);
            if ent.try_cloned::<&Laid>() != Some(item.out) {
                ent.set(item.out);
            }
        }
    });
}

harness_native_api::module!(
    "layout",
    install,
    [harness_native_api::API_LAYOUTS, harness_native_ui_node_api::API_LAYOUTS, harness_native_ui_layout_api::API_LAYOUTS, harness_native_ui_look_api::API_LAYOUTS]
);
