//! Folding the sidebar's tree: a folded node (every node starts so) shows only its running children and the selected
//! one, so a run that finishes folds away on its own; each row's disclosure chevron (left of its icon, Finder's) expands
//! it to all of them or folds it back, as does a double click on a session's row, Left and Right while the tree has the
//! keyboard, and Option on any of them for every sibling at once. Rows come and go on one spring each (`RowFold`, the
//! `tree_fold` tune of the sidebar's one spring family): a row that comes grows in from no height, pushing the rows under
//! it down, and its words fade in and settle into place once there is room for them; one that goes lets its words go
//! first and then closes. Its selection pill is not part of that: it is the tree's one matched pill and glides to the
//! row as it opens. Under Reduce Motion rows come and go at once. What is expanded is kept in native.json (`folds`, by
//! stable key: `false` for expanded, as it was saved before).
use std::collections::{HashMap, HashSet};

use super::*;
use crate::look::FoldIn;
use harness_native_api::reduced;
use harness_native_ui_layout_api::Background;
use harness_native_ui_motion_api::Pose;

/// A tree row as the view lists it: its key, its prefab and the model's row.
pub type Listed = (String, &'static str, model::Row);

/// The tree's rows across frames, so a row that left the model's list stays (folding away) until its spring is out.
#[derive(Default)]
pub struct TreeFold {
    /// What was listed last frame (leaving rows included), in order.
    prev: Vec<Listed>,
    /// Rows folding away, by key.
    leaving: HashMap<String, Listed>,
    /// Rows whose fold-away finished: dropped from the next list.
    done: HashSet<String>,
    /// Rows that came this frame (after the first list): they unfold in.
    entering: HashSet<String>,
}

/// A tree row folding in or away: its height at rest (`h`), and its spring from 0 (folded, no height) to 1 (open)
/// toward `target`. While it moves the row is `h` times the spring tall, and its parts (all but its selection pill) are
/// posed by [`content_share`]; open again, it goes back to its own height and the component goes.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct RowFold {
    pub h: f32,
    pub spring: Spring,
    pub target: f32,
}

/// How much of a folding row's content shows when the row has `x` of its height: none until it has `lead` of it, then
/// eased in (smoothstep) as the rest opens, so words never squeeze into a sliver or fade against the height; going away
/// the same curve runs back, the words gone before the row closes.
fn content_share(x: f32, lead: f32) -> f32 {
    let t = ((x - lead) / (1.0 - lead).max(1e-3)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Pose a folding row as its spring has it: its height, and its parts faded and lifted by what of them shows.
fn shape(row: EntityView<'_>, f: RowFold, look: FoldIn) {
    let x = f.spring.x.clamp(0.0, 1.0);
    if let Some(mut s) = row.try_cloned::<&Size>() {
        let h = f.h * x;
        if (s.h.value - h).abs() > 0.01 {
            s.h = Sizing::fixed(h);
            row.set(s);
        }
    }
    let a = content_share(x, look.lead);
    let pose = Pose { dx: 0.0, dy: -look.rise * (1.0 - a), shrink: 0.0, fade: 1.0 - a, cut: 0.0 };
    for part in harness_native_ui_node_api::children(row) {
        if !part.has(Background::id()) && part.try_cloned::<&Pose>() != Some(pose) {
            part.set(pose);
        }
    }
}

/// A row that has opened: its own height and its parts at rest.
fn rest(row: EntityView<'_>, f: RowFold) {
    row.remove(RowFold::id());
    if let Some(mut s) = row.try_cloned::<&Size>() {
        s.h = Sizing::fixed(f.h);
        row.set(s);
    }
    for part in harness_native_ui_node_api::children(row) {
        if part.has(Pose::id()) {
            part.remove(Pose::id());
        }
    }
}

/// An object whose saved fold (native.json `folds`) was applied.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct FoldRead;

impl TreeFold {
    /// The rows to show: the model's `now`, with the rows that left it still in place (after the row they came after)
    /// until they have folded away. Rows of objects that are gone leave at once, and under Reduce Motion every row comes
    /// and goes at once.
    pub fn merge(&mut self, now: Vec<Listed>, w: WorldRef<'_>) -> Vec<Listed> {
        let keys: HashSet<&str> = now.iter().map(|r| r.0.as_str()).collect();
        let still = reduced(w);
        if still {
            self.leaving.clear();
        }
        self.entering = if self.prev.is_empty() || still {
            HashSet::new()
        } else {
            let old: HashSet<&str> = self.prev.iter().map(|r| r.0.as_str()).collect();
            keys.iter().filter(|k| !old.contains(*k)).map(|k| (*k).to_owned()).collect()
        };
        let alive = |r: &model::Row| {
            w.entity_from_id(r.e).is_alive() && !w.entity_from_id(r.e).has(model::Archived::id())
        };
        for r in &self.prev {
            if !still && !keys.contains(r.0.as_str()) && !self.done.contains(&r.0) && alive(&r.2) {
                self.leaving.entry(r.0.clone()).or_insert_with(|| r.clone());
            }
        }
        self.leaving
            .retain(|k, r| !keys.contains(k.as_str()) && !self.done.contains(k) && alive(&r.2));
        self.done.clear();
        let mut out = now;
        // Each leaving row goes back after the row it followed last frame.
        for (i, r) in self.prev.iter().enumerate() {
            if let Some(l) = self.leaving.get(&r.0) {
                let after =
                    self.prev[..i].iter().rev().find_map(|p| out.iter().position(|o| o.0 == p.0));
                let at = after.map_or(0, |a| a + 1);
                if !out.iter().any(|o| o.0 == l.0) {
                    out.insert(at, l.clone());
                }
            }
        }
        self.prev.clone_from(&out);
        out
    }

    /// Start each row's spring: one that came unfolds in from nothing, one that is leaving folds away, one that came
    /// back while leaving unfolds again from where it was. A row that starts folding is shaped at once, before this
    /// frame's layout: a new row is laid out at no height from its first frame, so the rows under it never drop a whole
    /// row and come back.
    pub fn animate(&self, tree: EntityView<'_>, look: FoldIn) {
        for c in harness_native_ui_node_api::children(tree) {
            let name = c.name();
            let leaving = self.leaving.contains_key(name.as_str());
            let entering = self.entering.contains(name.as_str());
            let had = c.try_cloned::<&RowFold>();
            if !leaving && !entering && had.is_none() {
                continue;
            }
            let target = if leaving { 0.0 } else { 1.0 };
            let f = match had {
                Some(f) if f.target == target => continue,
                Some(f) => RowFold { target, ..f },
                None => RowFold {
                    h: c.try_cloned::<&Size>().map_or(28.0, |s| s.h.value),
                    spring: Spring::at(if entering { 0.0 } else { 1.0 }),
                    target,
                },
            };
            shape(c, f, look);
            c.set(f);
        }
    }
}

impl App {
    /// Step each folding row's spring and shape it; one that has unfolded goes back to its own height, one that has
    /// folded away is dropped from the next list. Under Reduce Motion each lands where it is going.
    pub(super) fn step_fold(&mut self, dt: f32) {
        let w = self.world.world_ref();
        let tree = w.entity_from_id(self.ids.tree);
        let (tune, look, still) = (crate::look::tune(w, "tree_fold"), crate::look::fold_in(w), reduced(w));
        let mut done = Vec::new();
        let mut moving = false;
        for c in harness_native_ui_node_api::children(tree) {
            let Some(mut f) = c.try_cloned::<&RowFold>() else { continue };
            if !still && settle(&mut f.spring, f.target, tune, dt) {
                shape(c, f, look);
                c.set(f);
                moving = true;
            } else if f.target > 0.5 {
                rest(c, f);
            } else {
                f.spring = Spring::at(0.0);
                shape(c, f, look);
                c.set(f);
                done.push(c.name());
            }
        }
        if !done.is_empty() || moving {
            self.fold.done.extend(done);
            self.poke();
        }
    }

    /// Apply the folds saved in native.json to objects that have not had theirs yet.
    pub(super) fn read_folds(&mut self) {
        let w = self.world.world_ref();
        let mut fresh = Vec::new();
        w.query::<()>()
            .with((model::Kind::id(), flecs::Wildcard::ID))
            .without(FoldRead::id())
            .build()
            .each_entity(|e, ()| fresh.push(*e.id()));
        if fresh.is_empty() {
            return;
        }
        let saved =
            crate::prefs::get("folds").and_then(|v| v.as_object().cloned()).unwrap_or_default();
        for id in fresh {
            let e = w.entity_from_id(id);
            e.add(FoldRead::id());
            if let Some(folded) =
                fold_key(e).and_then(|k| saved.get(&k)).and_then(serde_json::Value::as_bool)
            {
                model::set_expanded(w, id, !folded);
            }
        }
    }

    /// Fold or unfold `e` (with `all`, every sibling that folds too, to the state `e` goes to), and keep it.
    pub(super) fn toggle_fold_of(&mut self, e: u64, all: bool) {
        let w = self.world.world_ref();
        let o = w.entity_from_id(e);
        let fold = o.has(model::Expanded::id());
        let mut targets = vec![e];
        if all && let Some(p) = o.parent() {
            p.each_child(|c| {
                if *c.id() != e && has_kids(c) {
                    targets.push(*c.id());
                }
            });
        }
        let mut saved =
            crate::prefs::get("folds").and_then(|v| v.as_object().cloned()).unwrap_or_default();
        for t in targets {
            let c = w.entity_from_id(t);
            model::set_expanded(w, t, !fold);
            if let Some(k) = fold_key(c) {
                saved.insert(k, serde_json::Value::Bool(fold));
            }
        }
        crate::prefs::set("folds", serde_json::Value::Object(saved));
        self.poke();
    }

    /// Left in the tree (scene/20-keys.flecs, ctx.Tree): fold the selected row back to its running children, or go to its
    /// parent when it is folded or has no rows under it (Finder's, Xcode's); with `all`, its siblings fold too.
    pub(super) fn tree_fold(&mut self, all: bool) {
        let w = self.world.world_ref();
        let Some(s) = self.selected().map(|s| w.entity_from_id(s)) else { return };
        if has_kids(s) && s.has(model::Expanded::id()) {
            self.toggle_fold_of(*s.id(), all);
        } else if let Some(p) = s.parent().filter(|p| p.target(model::Kind::id(), 0).is_some()) {
            self.select(Some(*p.id()));
        }
    }

    /// Right in the tree: expand the selected row to all its children.
    pub(super) fn tree_unfold(&mut self, all: bool) {
        let w = self.world.world_ref();
        let Some(s) = self.selected().map(|s| w.entity_from_id(s)) else { return };
        if has_kids(s) && !s.has(model::Expanded::id()) {
            self.toggle_fold_of(*s.id(), all);
        }
    }
}

/// Whether `e` has rows under it in the tree (objects, not archived): what a disclosure folds.
pub fn has_kids(e: EntityView<'_>) -> bool {
    let mut any = false;
    e.each_child(|c| {
        any |= c.target(model::Kind::id(), 0).is_some() && !c.has(model::Archived::id())
    });
    any
}

/// The key an object's fold is kept by across launches: its daemon session (`s14`) or its run (`r12`).
fn fold_key(e: EntityView<'_>) -> Option<String> {
    if let Some(o) = e.try_cloned::<&model::Object>() {
        return Some(format!("s{}", o.id));
    }
    if e.has(model::StageNo::id()) {
        return None;
    }
    e.try_cloned::<&model::Run>().map(|r| format!("r{}", r.n))
}
