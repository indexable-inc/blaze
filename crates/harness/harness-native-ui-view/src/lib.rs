//! Views as entity trees (docs/ui-model.md): what a feature module writes to put something on screen, the way SwiftUI builds a
//! view body or HTML nests elements. A [`View`] is an entity made from a primitive prefab (`ui.Column`, `ui.Words`,
//! `ui.Button`, ...) or a composite of them written in a scene (`prefab SessionRow : ui.Row { title : ui.Words {} ... }`);
//! its children are views too, in order. Instancing a composite instantiates its named parts with it, so a feature sets only
//! what differs ([`View::part`]). Layout, drawing and hit testing are the generic passes': nothing here computes a rect.
//!
//! Lists are [`for_each`]: one row per key, made the first time the key is seen, deleted when it goes, kept in order.
//!
//! A plain library linked into each module that uses it: an edit swaps those modules in, as an edit of their own code does.
use std::collections::HashSet;

use harness_native_flecs::prelude::*;
use harness_native_ui_input_api::Action;
use harness_native_ui_layout_api::AnchoredTo;
use harness_native_ui_node_api::Node;
use harness_native_ui_text_api::{Icon, Text};

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

    /// Its glyph from den's icon set (a `ui.Symbol`); written only when it changes.
    pub fn icon(self, glyph: &str) -> Self {
        if self.0.try_cloned::<&Icon>().is_none_or(|i| i.glyph.as_str() != glyph) {
            self.0.set(Icon { glyph: glyph.into() });
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
    if let Some(pf) = self::prefab(world, prefab) {
        e.is_a(*pf.id());
    }
    e.add(Node::id())
}

/// Keep `parent`'s rows in step with `rows`, SwiftUI's `ForEach`: one child per key, made from `prefab` and named by the key
/// the first time the key is seen, deleted once it is gone, and kept in `rows`' order. `update` runs for every row with
/// whether it was just made, to set what differs (its words, its state relations). The parent holds only rows (a `ui.List`
/// or a `ui.Column` of them): a child that is not a row is deleted. A key seen twice keeps its first row.
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
        let base = self::prefab(world, prefab).map(|p| *p.id());
        // The prefab it was made from, not one that prefab is made from: a lit toggle (`ui.CodeChangesOn`, made from
        // `ui.CodeChanges`) is made again when the unlit one is asked for.
        let old = parent.part(key).filter(|row| base.is_none_or(|b| row.0.owns((flecs::IsA::ID, b))));
        if old.is_none()
            && let Some(stale) = parent.part(key)
        {
            stale.0.destruct();
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
        if !kept.contains(&*c.id()) {
            c.destruct();
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
