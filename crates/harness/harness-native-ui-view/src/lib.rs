//! Views as entity trees (docs/ui-model.md): what a feature module writes to put something on screen, the way SwiftUI builds a
//! view body or HTML nests elements. A [`View`] is an entity made from a primitive prefab (`ui.Column`, `ui.Words`,
//! `ui.Button`, ...) or a composite of them written in a scene (`prefab SessionRow : ui.Row { title : ui.Words {} ... }`);
//! its children are views too, in order. Instancing a composite instantiates its named parts with it, so a feature sets only
//! what differs ([`View::part`]). Layout, drawing and hit testing are the generic passes': nothing here computes a rect.
//!
//! Lists are [`for_each`]: one row per key, made the first time the key is seen, deleted when it goes (or sent out on its
//! `Transition`, [`leave`]), kept in order. A long list is [`lazy_for_each`]: rows only for the keys in view.
//!
//! A plain library linked into each module that uses it: an edit swaps those modules in, as an edit of their own code does.
use std::collections::HashSet;

pub mod field;
pub mod level;
pub mod menu;
pub mod search;
pub mod table;
pub use table::table;

use harness_native_flecs::prelude::*;
use harness_native_ui_input_api::Action;
use harness_native_ui_layout_api::{AnchoredTo, Background, Collapse, Lazy, LazyIndex, LazyRange, Overlay, ScrollerPart};
use harness_native_ui_motion_api::{Leaving, Transition};
use harness_native_ui_node_api::{Clip, Node, Placed};
use harness_native_ui_text_api::{Drawing, Icon, Text};

/// A node of a view tree, by entity.
#[derive(Clone, Copy)]
pub struct View<'a>(EntityView<'a>);

impl<'a> View<'a> {
    /// A new root view made from `prefab`.
    pub fn root(world: WorldRef<'a>, prefab: &str) -> Self {
        Self(make(world, None, prefab))
    }

    /// An entity as a view: the parent a feature builds under, or a node it made before.
    #[must_use]
    pub fn of(e: EntityView<'a>) -> Self {
        Self(e)
    }

    #[must_use]
    pub fn entity(self) -> EntityView<'a> {
        self.0
    }

    #[must_use]
    pub fn id(self) -> u64 {
        *self.0.id()
    }

    /// A new child made from `prefab`, after the children already there.
    pub fn child(self, prefab: &str) -> View<'a> {
        Self(make(self.0.world(), Some(self.0), prefab))
    }

    /// Build children in `f`, then go on with this view:
    /// `View::root(w, "ui.Column").children(|c| { c.child("ui.Words").text("Hello"); c.child("ui.Divider"); })`.
    pub fn children(self, f: impl FnOnce(Self)) -> Self {
        f(self);
        self
    }

    /// The child named `name`: a part of the composite this view was made from, instantiated with it (`row.part("title")`),
    /// or a row of a [`for_each`].
    #[must_use]
    pub fn part(self, name: &str) -> Option<View<'a>> {
        self.0.lookup_child(name).map(Self)
    }

    /// Name this view among its siblings, for [`part`](Self::part).
    pub fn named(self, name: &str) -> Self {
        self.0.set_name(name);
        self
    }

    /// Its words (a `ui.Words` or anything with `Text`); written only when they change.
    pub fn text(self, s: &str) -> Self {
        if self.0.try_cloned::<&Text>().is_none_or(|t| t.s.as_str() != s) {
            self.0.set(Text { s: s.into() });
        }
        self
    }

    /// Its glyph from den's icon set (a `ui.Symbol`), any `Drawing` it showed before gone; written only when it changes.
    pub fn icon(self, glyph: &str) -> Self {
        if self.0.has(Drawing::id()) {
            self.0.remove(Drawing::id());
        }
        if self.0.try_cloned::<&Icon>().is_none_or(|i| i.glyph.as_str() != glyph) {
            self.0.set(Icon { glyph: glyph.into() });
        }
        self
    }

    /// Its multi-colour drawing by name (a file kind's icon, harness-native-ui-inline's files.rs), in an icon's place:
    /// any `Icon` glyph it showed before gone, so a `ui.Symbol` part can show either. Written only when it changes.
    pub fn drawing(self, name: &str) -> Self {
        if self.0.has(Icon::id()) {
            self.0.remove(Icon::id());
        }
        if self.0.try_cloned::<&Drawing>().is_none_or(|d| d.name.as_str() != name) {
            self.0.set(Drawing { name: name.into() });
        }
        self
    }

    /// What activating it does: the name feature systems match on beside `Activated`.
    pub fn action(self, name: &str) -> Self {
        if self.0.try_cloned::<&Action>().is_none_or(|a| a.name.as_str() != name) {
            self.0.set(Action { name: name.into() });
        }
        self
    }

    /// Open against `node` (an overlay: a menu, a tip, a card), on the side its `Side` says.
    pub fn anchored(self, node: u64) -> Self {
        if self.0.target(AnchoredTo::id(), 0).is_none_or(|t| *t.id() != node) {
            self.0.add((AnchoredTo::id(), node));
        }
        self
    }

    pub fn set<T: ComponentId>(self, v: T) -> Self {
        self.0.set(v);
        self
    }

    pub fn add(self, id: impl IntoId) -> Self {
        self.0.add(id);
        self
    }

    /// Set the props of `template` (`hn.ObjRow`: the script template this row is an instance of) by name, SwiftUI's view
    /// inputs: the first call instantiates the template on this node, a later one re-runs only what reads a prop that
    /// changed. Nothing is written when the props are the ones written last. A prop the template lacks is a scene bug:
    /// it panics in a debug build and is logged once otherwise.
    pub fn props(self, template: &str, props: &[(&str, Prop<'_>)]) -> Self {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        template.hash(&mut h);
        props.hash(&mut h);
        let h = h.finish();
        if self.0.try_cloned::<&PropsHash>().is_some_and(|p| p.h == h) {
            return self;
        }
        let world = self.0.world();
        let Some(t) = self::prefab(world, template) else {
            debug_assert!(false, "the scene has no template {template}");
            return self;
        };
        if let Err(e) = self.0.set_props(t.id(), props) {
            debug_assert!(false, "{template}: {e}");
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| eprintln!("harness-native: {template}: {e}"));
        }
        self.0.set(PropsHash { h });
        self
    }
}

/// Register the view layer's own component.
pub fn register(world: &World) {
    world.component::<PropsHash>();
}

/// The prefab the scene calls `name`: a script's path (`ui.SessionRow`, `.` between its parts) as the world's (`ui::SessionRow`).
#[must_use]
pub fn prefab<'a>(world: WorldRef<'a>, name: &str) -> Option<EntityView<'a>> {
    world.try_lookup(&name.replace('.', "::"))
}

/// A node under `parent` (a root without one) made from `prefab`: ordered before the prefab's parts are instantiated under
/// it, so they keep the order the scene declares them in. A prefab the scene does not have makes a bare node.
fn make<'a>(world: WorldRef<'a>, parent: Option<EntityView<'a>>, prefab: &str) -> EntityView<'a> {
    let e = world.entity();
    if let Some(p) = parent {
        e.child_of(*p.id());
    }
    e.add(flecs::OrderedChildren::ID);
    // A template (`template hn.ObjRow { prop ... }`) is not inherited from: the first `View::props` instantiates it.
    if let Some(pf) = self::prefab(world, prefab).filter(|p| !is_template(*p)) {
        e.is_a(*pf.id());
    }
    e.add(Node::id())
}

/// Whether `e` is a script template (a component whose body a script runs for each instance), not a prefab.
#[must_use]
pub fn is_template(e: EntityView<'_>) -> bool {
    !e.has(flecs::Prefab::ID) && e.try_get::<&flecs::Script>(|_| ()).is_some()
}

/// Whether `row` was made from `base` (a prefab it inherits from directly, or a template it is an instance of).
fn made_from(row: EntityView<'_>, base: Option<EntityView<'_>>) -> bool {
    base.is_none_or(|b| if is_template(b) { row.has(*b.id()) } else { row.owns((flecs::IsA::ID, *b.id())) })
}

/// The hash of the props a [`View::props`] last wrote, so a frame that changes none writes nothing.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct PropsHash {
    pub h: u64,
}

/// Keep `parent`'s rows in step with `rows`, SwiftUI's `ForEach`: one child per key, made from `prefab` and named by the key
/// the first time the key is seen, deleted once it is gone, and kept in `rows`' order. `update` runs for every row with
/// whether it was just made, to set what differs (its words, its state relations). The parent holds only rows (a `ui.List`
/// or a `ui.Column` of them): a child that is not a row is deleted, except a scroll's own scroller parts (`ScrollerPart`:
/// a `ui.Scroll` may hold its rows directly). A key seen twice keeps its first row.
///
/// Inside a system the rows it makes arrive at the end of the system; reordering waits for the call in which every row
/// already exists (rows that are only appended keep their order without it).
pub fn for_each<'a, K: AsRef<str>, T>(parent: View<'a>, prefab: &str, rows: impl IntoIterator<Item = (K, T)>, update: impl FnMut(View<'a>, T, bool)) {
    for_each_of(parent, rows.into_iter().map(|(k, t)| (k, prefab, t)), update);
}

/// [`for_each`] for a list whose rows are not all alike (a tree of folders and files, a note where a list is empty): each
/// row says its prefab. A key whose prefab changed is made again from the new one.
pub fn for_each_of<'a, 'p, K: AsRef<str>, T>(parent: View<'a>, rows: impl IntoIterator<Item = (K, &'p str, T)>, mut update: impl FnMut(View<'a>, T, bool)) {
    let world = parent.0.world();
    let mut seen = HashSet::new();
    let mut order = Vec::new();
    for (key, prefab, item) in rows {
        let key = key.as_ref();
        if !seen.insert(key.to_owned()) {
            continue;
        }
        let base = self::prefab(world, prefab);
        // The prefab it was made from, not one that prefab is made from: a lit toggle (`ui.CodeChangesOn`, made from
        // `ui.CodeChanges`) is made again when the unlit one is asked for.
        let old = parent.part(key).filter(|row| made_from(row.0, base));
        if old.is_none()
            && let Some(stale) = parent.part(key)
        {
            leave(stale);
        }
        let (row, new) = match old {
            Some(row) => (row, false),
            None => (parent.child(prefab).named(key), true),
        };
        order.push(row.id());
        update(row, item, new);
    }
    let kept: HashSet<u64> = order.iter().copied().collect();
    parent.0.each_child(|c| {
        if !kept.contains(&*c.id()) && !c.has(Leaving::id()) && !c.has(ScrollerPart::id()) {
            leave(View(c));
        }
    });
    // Read again: outside a system the stale rows are gone now and the new ones are children; inside one neither has
    // happened yet, the child set does not match and the order is set by a later call.
    let mut now = Vec::new();
    parent.0.each_child(|c| now.push(*c.id()));
    let placed: Vec<u64> = now.iter().copied().filter(|id| kept.contains(id)).collect();
    if placed != order {
        let rest = now.iter().copied().filter(|id| !kept.contains(id));
        let full: Vec<u64> = order.iter().copied().chain(rest).collect();
        parent.0.set_child_order(&full);
    }
}

/// Remove a view: at once, or, when it has a `Transition`, by sending it out on it (SwiftUI's removal transition). A leaving
/// view keeps the place it was laid out at (`Placed`), loses its name (so its key can come back as a new row while it goes)
/// and is deleted by motion once its spring reaches 0. One in its parent's flow folds away as it goes (`Collapse`, clipped
/// to what it still holds), so its neighbours slide into its space as in a SwiftUI list instead of jumping at the end.
/// One placed already (a menu, a popover, a hover card: a root of its own, laid against its anchor) was never in that
/// flow and goes from its own place: with its anchor gone, layout keeps the `Laid` it last had.
pub fn leave(v: View<'_>) {
    let e = v.0;
    if !e.has(Transition::id()) {
        e.destruct();
        return;
    }
    if !e.has(Leaving::id()) {
        let id = *e.id();
        // Whether it is in its parent's flow is read before it is placed below. Layout's `gather` keeps every placed child
        // out of the flow, so a collapsing stand-in for one placed before would lay it at the flow's start (its slot's
        // corner) for the rest of its exit: a closing hover card's window jumped to the pane's top-left.
        let in_flow = !e.has(Placed::id())
            && e.parent().is_some_and(|p| p.has(Node::id()))
            && !e.has(Background::id())
            && !e.has(Overlay::id());
        e.set_name(&format!("leaving_{id}"));
        e.add(Placed::id());
        e.add(Leaving::id());
        if in_flow {
            e.set(Collapse { k: 1.0 });
            e.add(Clip::id());
        }
    }
}

/// Whether the pointer is crossing from `anchor` to the `plate` that floats by it: in the band between their facing edges,
/// across both their widths (or heights, for a plate at a side). A hover card open on its anchor stays open while the
/// pointer travels the gap onto it, as a submenu's does; nothing is hovered there, and the pointer is on neither. False
/// when they touch or overlap (no gap to cross), or either has no place yet.
pub fn bridged(anchor: EntityView<'_>, plate: EntityView<'_>) -> bool {
    use harness_native_ui_input_api::PointerState;
    use harness_native_ui_node_api::Laid;
    let (Some(a), Some(p)) = (anchor.try_cloned::<&Laid>(), plate.try_cloned::<&Laid>()) else { return false };
    let Some(at) = anchor.world().try_get::<&PointerState>(|s| [s.x, s.y]) else { return false };
    let span = |a0: f32, a1: f32, p0: f32, p1: f32, v: f32| v >= a0.min(p0) && v <= a1.max(p1);
    let gap = |a0: f32, a1: f32, p0: f32, p1: f32, v: f32| {
        if p1 <= a0 {
            v >= p1 && v <= a0
        } else if p0 >= a1 {
            v >= a1 && v <= p0
        } else {
            false
        }
    };
    let (ax1, ay1, px1, py1) = (a.x + a.w, a.y + a.h, p.x + p.w, p.y + p.h);
    let vertical = gap(a.y, ay1, p.y, py1, at[1]) && span(a.x, ax1, p.x, px1, at[0]);
    let sideways = gap(a.x, ax1, p.x, px1, at[0]) && span(a.y, ay1, p.y, py1, at[1]);
    vertical || sideways
}

/// Keep a `Lazy` container's rows to the ones in view (SwiftUI's `LazyVStack { ForEach }`): `count` rows in all, row `i`
/// named `key(i)`. Rows whose index is in the `LazyRange` layout wrote are kept by key; a row that scrolled out is
/// recycled for one that scrolled in (renamed, re-indexed, `update` told it is fresh) and the rest are deleted. `update`
/// runs for every row in view with its index and whether it was just made or rebound. Before layout has said a range, the
/// first `overscan` + 16 rows are made. Returns how many rows exist.
pub fn lazy_for_each<'a, K: AsRef<str>>(parent: View<'a>, prefab: &str, count: u32, key: impl Fn(u32) -> K, update: impl FnMut(View<'a>, u32, bool)) -> usize {
    lazy_for_each_of(parent, count, key, |_| prefab, update)
}

/// [`lazy_for_each`] for a list whose rows are not all alike (day headings among items, a note, a "load more" foot):
/// row `i` is made from `prefab(i)`, and a row that scrolled out is recycled only for one made from the same prefab.
/// Every row is still the `Lazy` container's `row` points long.
pub fn lazy_for_each_of<'a, 'p, K: AsRef<str>>(
    parent: View<'a>, count: u32, key: impl Fn(u32) -> K, prefab: impl Fn(u32) -> &'p str, mut update: impl FnMut(View<'a>, u32, bool),
) -> usize {
    let e = parent.0;
    let world = e.world();
    // The prefab a row was made from (not one that prefab is made from), as `for_each_of` tells them apart.
    let base = |name: &str| self::prefab(world, name);
    let mut lazy = e.try_cloned::<&Lazy>().unwrap_or_default();
    if lazy.count != count {
        lazy.count = count;
        e.set(lazy);
    }
    let range = e.try_cloned::<&LazyRange>().map_or(0..count.min(lazy.overscan + 16), |r| r.first.min(count)..r.last.min(count));
    let want: Vec<(u32, String)> = range.map(|i| (i, key(i).as_ref().to_owned())).collect();
    let wanted: HashSet<&str> = want.iter().map(|(_, k)| k.as_str()).collect();
    // Rows no longer in view, free to be bound again.
    let mut spare: Vec<EntityView<'a>> = Vec::new();
    e.each_child(|c| {
        if c.has(Node::id()) && !c.has(Leaving::id()) && !c.has(ScrollerPart::id()) && !wanted.contains(c.name().as_str()) {
            spare.push(c);
        }
    });
    let mut live = 0;
    for (i, k) in &want {
        let pf = prefab(*i);
        let b = base(pf);
        let kept = parent.part(k).filter(|row| made_from(row.0, b));
        if kept.is_none()
            && let Some(stale) = parent.part(k)
        {
            stale.0.destruct();
        }
        let (row, fresh) = match kept {
            Some(row) => (row, false),
            None => match spare.iter().position(|&c| made_from(c, b)) {
                Some(at) => (View(spare.swap_remove(at)).named(k), true),
                None => (parent.child(pf).named(k), true),
            },
        };
        if row.0.try_cloned::<&LazyIndex>().is_none_or(|l| l.i != *i) {
            row.0.set(LazyIndex { i: *i });
        }
        update(row, *i, fresh);
        live += 1;
    }
    for c in spare {
        c.destruct();
    }
    live
}

/// The heights a [`lazy_stack`] measured its rows at, by key, and the width it last measured them at. A new width keeps
/// them: each is its row's best guess until the row is laid out at that width (the rows in view re-wrap every frame of a
/// fold; a row out of view settles when it comes into view, the scroll held still by [`Stacked::shift`]). Forgetting them
/// would put every row out of view back at the estimate, and the transcript above the view would jump.
#[derive(Clone, Debug, Default)]
pub struct Heights {
    width: f32,
    h: std::collections::HashMap<String, f32>,
}

/// What [`lazy_anchor`] found after rows were laid out at a new width: how far the scroll moves to hold the row at the top
/// of the view where it was (`shift`), and how much taller all the rows are (`grew`, negative when shorter).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Anchored {
    pub shift: f32,
    pub grew: f32,
}

/// How far the rows of a stack moved the place `top` points down its content, from rows `(old, new)` heights in order,
/// the first starting `first` down: a row wholly above `top` moves it by all it grew, the row `top` is in by as much of
/// its growth as lies above `top` (the same share of it stays above the view, so the line read at the top stays there
/// while its words re-wrap), a row below not at all. Also how much they grew in all.
#[must_use]
pub fn anchor_shift(first: f32, rows: impl IntoIterator<Item = (f32, f32)>, top: f32) -> Anchored {
    let (mut at, mut out) = (first, Anchored::default());
    for (old, new) in rows {
        let grew = new - old;
        out.grew += grew;
        if at + old <= top {
            out.shift += grew;
        } else if at < top && old > 0.0 {
            out.shift += grew * (top - at) / old;
        }
        at += old;
    }
    out
}

/// What a [`lazy_stack`] call found: how tall all its rows are (measured or estimated), and how far the rows above the
/// view grew since the last call (the scroll moves by it to keep what is in view still).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stacked {
    pub content: f32,
    pub shift: f32,
}

/// Keep a `LazyStack` container's rows to the ones in view, rows of any height (a chat's transcript): `count` rows, row
/// `i` named `key(i)` and made from `prefab(i)`; `top` is how far its content is scrolled and `room` how tall its view is.
/// Rows are measured from their last layout into `heights`; rows never laid out count the `LazyStack`'s estimate. The rows
/// above and below the ones that exist are two spacers (`_top`, `_foot`) as tall as they are, so the stack is as long as
/// all its rows. The stack's `Flex.gap` must be 0 (rows carry their own padding). A row out of view that `keep` holds (one
/// with a modal open over the pane) stays an entity, parked out of the flow (`Placed`, laid below the view, so the clip
/// around the stack hides it) and still updated, so what it floats over the pane stays up; in view again, it rejoins the
/// flow as the same entity.
#[allow(clippy::too_many_arguments)]
pub fn lazy_stack<'a, 'p, K: AsRef<str>>(
    parent: View<'a>, heights: &mut Heights, top: f32, room: f32, count: usize, key: impl Fn(usize) -> K,
    prefab: impl Fn(usize) -> &'p str, keep: impl Fn(usize) -> bool, mut update: impl FnMut(View<'a>, usize, bool),
) -> Stacked {
    use harness_native_ui_layout_api::{LazyStack, Size, SizeKind, Sizing};
    use harness_native_ui_node_api::Laid;
    let e = parent.0;
    let spec = e.try_cloned::<&LazyStack>().unwrap_or(LazyStack { estimate: 40.0, overscan: 400.0 });
    heights.width = e.try_cloned::<&Laid>().map_or(0.0, |l| l.w);
    // Measure the rows that exist; what changed above the view moves the scroll.
    let keys: Vec<String> = (0..count).map(|i| key(i).as_ref().to_owned()).collect();
    let tops = |heights: &Heights| {
        let mut at = 0.0;
        let mut v = Vec::with_capacity(count + 1);
        for k in &keys {
            v.push(at);
            at += heights.h.get(k).copied().unwrap_or(spec.estimate);
        }
        v.push(at);
        v
    };
    let before = tops(heights);
    let mut shift = 0.0;
    e.each_child(|c| {
        let name = c.name().to_string();
        if name.starts_with('_') || !c.has(Node::id()) || c.has(Placed::id()) {
            return;
        }
        let Some(l) = c.try_cloned::<&Laid>().filter(|l| l.h > 0.0) else { return };
        let Some(i) = keys.iter().position(|k| *k == name) else { return };
        let old = heights.h.get(&name).copied().unwrap_or(spec.estimate);
        if (old - l.h).abs() > 0.25 {
            if before[i] + old <= top {
                shift += l.h - old;
            }
            heights.h.insert(name, l.h);
        }
    });
    let tops = tops(heights);
    let total = tops[count];
    let (lo, hi) = (top + shift - spec.overscan, top + shift + room + spec.overscan);
    let first = (0..count).find(|&i| tops[i + 1] > lo).unwrap_or(count);
    let last = (first..count).find(|&i| tops[i] >= hi).unwrap_or(count);
    let gap = |h: f32| Size {
        w: Sizing { kind: SizeKind::Grow, value: 1.0, min: 0.0, max: 0.0 },
        h: Sizing { kind: SizeKind::Fixed, value: h, min: 0.0, max: 0.0 },
    };
    enum Slot {
        Gap(f32),
        Row(usize),
        Parked(usize),
    }
    // Parked rows lie past the stack's end by more than the view is tall: below the view whatever the scroll.
    let at = e.try_cloned::<&Laid>().unwrap_or_default();
    let parked_y = at.y + at.h + room + spec.overscan;
    let parked = (0..first).chain(last..count).filter(|&i| keep(i));
    let rows = std::iter::once(("_top".to_owned(), "ui.Node", Slot::Gap(tops[first])))
        .chain((first..last).map(|i| (keys[i].clone(), prefab(i), Slot::Row(i))))
        .chain(std::iter::once(("_foot".to_owned(), "ui.Node", Slot::Gap(total - tops[last]))))
        .chain(parked.map(|i| (keys[i].clone(), prefab(i), Slot::Parked(i))));
    for_each_of(parent, rows, |row, slot, new| match slot {
        Slot::Gap(h) => {
            if row.0.try_cloned::<&Size>().is_none_or(|s| (s.h.value - h).abs() > 0.01) {
                row.0.set(gap(h));
            }
        }
        Slot::Row(i) => {
            if row.0.has(Placed::id()) {
                row.0.remove(Placed::id());
            }
            update(row, i, new);
        }
        Slot::Parked(i) => {
            if !row.0.has(Placed::id()) {
                row.0.add(Placed::id());
            }
            let h = heights.h.get(&keys[i]).copied().unwrap_or(spec.estimate);
            let want = Laid { x: at.x, y: parked_y, w: at.w, h };
            if row.0.try_cloned::<&Laid>() != Some(want) {
                row.0.set(want);
            }
            update(row, i, new);
        }
    });
    Stacked { content: total, shift }
}

/// Hold a [`lazy_stack`]'s view still once its rows were laid out at a new width (a fold moving a transcript's column),
/// in the frame they were: measure the rows that exist into `heights` and say how far the scroll at `top` moves to keep
/// the row at the top of the view where it was ([`anchor_shift`]). The host calls it after layout settles and before the
/// frame is drawn, so the view never shows a frame of rows above it grown without the scroll following; [`lazy_stack`]
/// finds them measured on the next frame and moves nothing again. None while the stack's width is the one it measured at.
pub fn lazy_anchor(parent: View<'_>, heights: &mut Heights, top: f32) -> Option<Anchored> {
    use harness_native_ui_layout_api::{LazyStack, Size};
    use harness_native_ui_node_api::Laid;
    let e = parent.0;
    let width = e.try_cloned::<&Laid>().map_or(0.0, |l| l.w);
    if (width - heights.width).abs() <= 0.5 {
        return None;
    }
    heights.width = width;
    let estimate = e.try_cloned::<&LazyStack>().map_or(40.0, |s| s.estimate);
    // The rows above the ones that exist are the `_top` spacer, as tall as their heights (which nothing changed).
    let mut first = 0.0;
    let mut rows = Vec::new(); // alloc: the rows in view, once a frame while a fold moves
    // In layout's order (the stack's flow), so each row's place down the content is the sum of the ones before it.
    for c in harness_native_ui_node_api::children(e) {
        let name = c.name().to_string();
        if name == "_top" {
            first = c.try_cloned::<&Size>().map_or(0.0, |s| s.h.value);
        }
        if name.starts_with('_') || c.has(Placed::id()) {
            continue;
        }
        let old = heights.h.get(&name).copied().unwrap_or(estimate);
        let new = c.try_cloned::<&Laid>().map(|l| l.h).filter(|&h| h > 0.0 && (h - old).abs() > 0.25);
        if let Some(h) = new {
            heights.h.insert(name, h);
        }
        rows.push((old, new.unwrap_or(old)));
    }
    Some(anchor_shift(first, rows, top))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three rows from 100 down, 50 tall each, re-wrapped to 80, 70 and 90, with the view's top 25 into the second: the
    /// first, wholly above, moves the top by all it grew (30); the second by the half of its growth above the top (10);
    /// the third, below, not at all. All three grew 90.
    #[test]
    fn the_row_at_the_top_keeps_the_share_of_it_above_the_view() {
        let a = anchor_shift(100.0, [(50.0, 80.0), (50.0, 70.0), (50.0, 90.0)], 175.0);
        assert_eq!(a, Anchored { shift: 40.0, grew: 90.0 });
        // A top above every row (the start of the transcript) moves nothing; one at a row's top keeps that row's top.
        assert_eq!(anchor_shift(100.0, [(50.0, 80.0)], 50.0).shift, 0.0);
        assert_eq!(anchor_shift(100.0, [(50.0, 80.0), (50.0, 20.0)], 150.0).shift, 30.0);
    }
}
