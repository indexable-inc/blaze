//! Folding the sidebar's tree: each row's disclosure chevron (left of its icon, Finder's) folds or unfolds what is under
//! it, as does a double click on a session's row, a click anywhere on a group's, Left and Right while the tree has the
//! keyboard, and Option on any of them for every sibling at once. Rows come and go on a spring (their height and
//! opacity; the rows under them slide), and what is folded is kept in native.json (`folds`, by stable key).
use std::collections::{HashMap, HashSet};

use super::*;
use harness_native_ui_input_api::Pressable;
use harness_native_ui_motion_api::{Motion, Reveal, Shown, Target};
use harness_native_ui_node_api::Clip;

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

/// A row's height at rest, kept while it folds in or away (its `Size.h` follows its `Shown` spring).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct RowRest {
    pub h: f32,
}

/// An object whose saved fold (native.json `folds`) was applied.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct FoldRead;

impl TreeFold {
    /// The rows to show: the model's `now`, with the rows that left it still in place (after the row they came after)
    /// until they have folded away. Rows of objects that are gone leave at once.
    pub fn merge(&mut self, now: Vec<Listed>, w: WorldRef<'_>) -> Vec<Listed> {
        let keys: HashSet<&str> = now.iter().map(|r| r.0.as_str()).collect();
        let first = self.prev.is_empty();
        self.entering = if first { HashSet::new() } else { let old: HashSet<&str> = self.prev.iter().map(|r| r.0.as_str()).collect(); keys.iter().filter(|k| !old.contains(*k)).map(|k| (*k).to_owned()).collect() };
        let alive = |r: &model::Row| match *r {
            model::Row::Obj { e, .. } => w.entity_from_id(e).is_alive() && !w.entity_from_id(e).has(model::Archived::id()),
            model::Row::More { parent, .. } => w.entity_from_id(parent).is_alive(),
        };
        for r in &self.prev {
            if !keys.contains(r.0.as_str()) && !self.done.contains(&r.0) && alive(&r.2) {
                self.leaving.entry(r.0.clone()).or_insert_with(|| r.clone());
            }
        }
        self.leaving.retain(|k, r| !keys.contains(k.as_str()) && !self.done.contains(k) && alive(&r.2));
        self.done.clear();
        let mut out = now;
        // Each leaving row goes back after the row it followed last frame.
        for (i, r) in self.prev.iter().enumerate() {
            if let Some(l) = self.leaving.get(&r.0) {
                let after = self.prev[..i].iter().rev().find_map(|p| out.iter().position(|o| o.0 == p.0));
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
    /// back while leaving unfolds again from where it was.
    pub fn animate(&self, tree: EntityView<'_>, tune: Tune) {
        for c in harness_native_ui_node_api::children(tree) {
            let name = c.name();
            let leaving = self.leaving.contains_key(name.as_str());
            let entering = self.entering.contains(name.as_str());
            if !leaving && !entering && !c.has(RowRest::id()) {
                continue;
            }
            if !c.has(RowRest::id()) {
                let h = c.try_cloned::<&Size>().map_or(28.0, |s| s.h.value);
                c.set(RowRest { h });
                c.set(Motion { omega: tune.omega, zeta: tune.zeta });
                c.set(Reveal { alpha0: 0.0, dx: 0.0, dy: 0.0, scale0: 1.0 });
                c.set(Shown { spring: Spring::at(if entering { 0.0 } else { 1.0 }), started: true });
                c.add(Clip::id());
            }
            let want = if leaving { 0.0 } else { 1.0 };
            if c.try_cloned::<&Target>().is_none_or(|t| t.0 != want) {
                c.set(Target(want));
            }
        }
    }
}

impl App {
    /// Each folding row's height follows its spring; one that has unfolded settles back to its own height, one that
    /// has folded away is dropped from the next list.
    pub(super) fn step_fold(&mut self) {
        let tree = self.world.entity_from_id(self.ids.tree);
        let mut done = Vec::new();
        let mut moving = false;
        for c in harness_native_ui_node_api::children(tree) {
            let Some(rest) = c.try_cloned::<&RowRest>() else { continue };
            let x = c.try_cloned::<&Shown>().map_or(1.0, |s| s.spring.x);
            let target = c.try_cloned::<&Target>().map_or(1.0, |t| t.0);
            let settled = c.try_cloned::<&Shown>().is_none_or(|s| s.spring.settled(target));
            let h = rest.h * x.clamp(0.0, 1.0);
            if let Some(mut s) = c.try_cloned::<&Size>()
                && (s.h.value - h).abs() > 0.01
            {
                s.h = Sizing::fixed(if settled && target > 0.5 { rest.h } else { h });
                c.set(s);
            }
            if settled && target > 0.5 {
                c.remove(RowRest::id());
                c.remove(Motion::id());
                c.remove(Shown::id());
                c.remove(Target::id());
                c.remove(Reveal::id());
                c.remove(Clip::id());
                if let Some(mut s) = c.try_cloned::<&Size>() {
                    s.h = Sizing::fixed(rest.h);
                    c.set(s);
                }
            } else if settled {
                done.push(c.name());
            } else {
                moving = true;
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
        w.query::<()>().with((model::Kind::id(), flecs::Wildcard::ID)).without(FoldRead::id()).build().each_entity(|e, ()| fresh.push(*e.id()));
        if fresh.is_empty() {
            return;
        }
        let saved = crate::prefs::get("folds").and_then(|v| v.as_object().cloned()).unwrap_or_default();
        for id in fresh {
            let e = w.entity_from_id(id);
            e.add(FoldRead::id());
            if let Some(folded) = fold_key(e).and_then(|k| saved.get(&k)).and_then(serde_json::Value::as_bool) {
                if folded {
                    e.add(model::Collapsed::id());
                } else {
                    e.remove(model::Collapsed::id());
                }
            }
        }
    }

    /// Fold or unfold `e` (with `all`, every sibling that folds too, to the state `e` goes to), and keep it.
    pub(super) fn toggle_fold_of(&mut self, e: u64, all: bool) {
        let w = self.world.world_ref();
        let o = w.entity_from_id(e);
        let fold = !o.has(model::Collapsed::id());
        let mut targets = vec![e];
        if all && let Some(p) = o.parent() {
            p.each_child(|c| {
                if *c.id() != e && has_kids(c) {
                    targets.push(*c.id());
                }
            });
        }
        let mut saved = crate::prefs::get("folds").and_then(|v| v.as_object().cloned()).unwrap_or_default();
        for t in targets {
            let c = w.entity_from_id(t);
            if fold {
                c.add(model::Collapsed::id());
            } else {
                c.remove(model::Collapsed::id());
            }
            if let Some(k) = fold_key(c) {
                saved.insert(k, serde_json::Value::Bool(fold));
            }
        }
        crate::prefs::set("folds", serde_json::Value::Object(saved));
        self.poke();
    }

    /// A key while the tree has the keyboard (since a tree row was pressed): Up and Down step the selection, Left folds
    /// the selected row or goes to its parent, Right unfolds it (Finder's, Xcode's); Return or Escape hand the keyboard
    /// back to the terminal, and any other key does too, then goes to it. True when the tree took the key.
    pub(super) fn tree_key(&mut self, k: &term::KeyIn<'_>) -> bool {
        if !self.tree_focus || k.action == KeyAction::Release {
            return false;
        }
        if k.mods.cmd || k.mods.ctrl {
            return false;
        }
        let sel = self.selected();
        let w = self.world.world_ref();
        match k.code {
            KEY_UP => self.step_object(-1),
            KEY_DOWN => self.step_object(1),
            KEY_LEFT => {
                let Some(s) = sel.map(|s| w.entity_from_id(s)) else { return true };
                if has_kids(s) && !s.has(model::Collapsed::id()) {
                    self.toggle_fold_of(*s.id(), k.mods.alt);
                } else if let Some(p) = s.parent().filter(|p| p.target(model::Kind::id(), 0).is_some()) {
                    self.select(Some(*p.id()));
                }
            }
            KEY_RIGHT => {
                let Some(s) = sel.map(|s| w.entity_from_id(s)) else { return true };
                if has_kids(s) && s.has(model::Collapsed::id()) {
                    self.toggle_fold_of(*s.id(), k.mods.alt);
                }
            }
            KEY_RETURN | KEY_ESCAPE => self.tree_focus = false,
            _ => {
                self.tree_focus = false;
                return false;
            }
        }
        self.poke();
        true
    }
}

const KEY_RETURN: u16 = 36;
const KEY_ESCAPE: u16 = 53;
const KEY_LEFT: u16 = 123;
const KEY_RIGHT: u16 = 124;
const KEY_DOWN: u16 = 125;
const KEY_UP: u16 = 126;

/// Whether `e` has rows under it in the tree (objects, not archived): what a disclosure folds.
pub fn has_kids(e: EntityView<'_>) -> bool {
    let mut any = false;
    e.each_child(|c| any |= c.target(model::Kind::id(), 0).is_some() && !c.has(model::Archived::id()));
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

/// Set a row's disclosure: a chevron that folds `e` (turned open or not) when it has rows under it, shown while the row
/// is hovered or while it is folded; nothing, and no hit target (the row takes the press), when it has none.
pub fn disclosure(row: View<'_>, e: Option<EntityView<'_>>) {
    // In the sidebar it hangs in the row's `hang` layer (left of the icon); in the Archive it is in the row's flow.
    let Some(p) = row.part("disclose").or_else(|| row.part("hang").and_then(|h| h.part("disclose"))) else { return };
    let kids = e.filter(|e| has_kids(*e));
    let pe = p.entity();
    match kids {
        Some(o) => {
            let open = !o.has(model::Collapsed::id());
            if !pe.has(Pressable::id()) {
                pe.add(Pressable::id());
            }
            p.action(&format!("toggle:{}", *o.id()));
            if let Some(i) = p.part("icon") {
                i.icon(crate::look::CHEVRON);
                crate::look::disclose(i.entity(), open);
                crate::look::hover_show(i.entity(), row.id(), !open);
            }
        }
        None => {
            if pe.has(Pressable::id()) {
                pe.remove(Pressable::id());
                pe.remove(harness_native_ui_input_api::Hoverable::id());
                pe.remove(harness_native_ui_input_api::Focusable::id());
            }
            if let Some(i) = p.part("icon") {
                i.icon("");
            }
        }
    }
}
