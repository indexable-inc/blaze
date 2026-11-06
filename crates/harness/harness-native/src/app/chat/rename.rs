//! Renaming a session in place (cmd+R, `act.Rename` in scene/20-keys.flecs; the palette's "Rename chat"). The name in
//! its sidebar row (`hn.ObjRow`'s `name`, an `hn.tree.Head`) turns into a one-line field holding what it is called now,
//! all of it selected, its pill the field's ground (the row's `edit` prop). No dialog: the chat input's edit keys
//! (ctx.Chat) and typed, marked and pasted text act on the field while it lasts. Return saves, Escape cancels, and a press
//! anywhere outside the row, the selection moving on or the palette opening saves. Saved, the name goes to the daemon
//! (`rename`), which keeps it with the session (its index row's `label`, so a restart keeps it) and lists it back as the
//! session's `label`, which the model shows as its `Name` everywhere it shows; saved empty, the session has its automatic
//! name again.
use super::*;
use harness_native_ui_input_api::Focused;

/// The rename in progress: the row's object (its entity id), its session, the words being typed, what it was called when
/// the rename began, and when its caret last moved (its blink starts solid from then).
pub struct Rename {
    pub obj: u64,
    sid: u64,
    draft: Draft,
    was: String,
    since: f32,
}

impl Rename {
    /// An input method's marked text on the field.
    pub(in crate::app) fn marked(&self) -> Option<String> {
        self.draft.marked.clone()
    }
}

/// Where `m` takes the caret along one line: up and a line's start are its start, down and a line's end its end.
fn along(d: &Draft, m: Move) -> usize {
    d.target(m).unwrap_or(match m {
        Move::Up | Move::LineStart => 0,
        _ => d.text.len(),
    })
}

impl App {
    /// Whether a name is being renamed: it has the keys (keys.rs `active_contexts` gives it ctx.Chat's edits).
    pub(in crate::app) fn renaming(&self) -> bool {
        self.rename.is_some()
    }

    /// cmd+R: rename the selected session (not a run, not the Archive). The sidebar opens, if folded, to show the field;
    /// the palette, a model menu and a cell's field let go of the keys.
    pub(in crate::app) fn rename_start(&mut self) {
        if self.renaming() {
            return;
        }
        let Some(e) = self.selected() else { return };
        if self.run_of(e).is_some() {
            return;
        }
        let o = self.world.entity_from_id(e);
        let Some(sid) = o.try_cloned::<&model::Object>().map(|ob| ob.id) else { return };
        let was = o.try_cloned::<&model::Name>().map(|n| n.s.as_str().to_owned()).unwrap_or_default();
        self.palette = None;
        self.chat_pick_close();
        self.world.lookup(APP).remove((Focused::id(), flecs::Wildcard::ID));
        self.tree_focus = false;
        self.side_open = true;
        let draft = Draft { caret: was.len(), anchor: 0, text: was.clone(), ..Draft::default() };
        self.rename = Some(Rename { obj: e, sid, draft, was, since: self.time });
        self.poke();
    }

    /// End the rename: `save` sends the name to the daemon when it changed (trimmed; empty: the automatic name again),
    /// shown at once; else the name stays what it was. The field's words go back to rest.
    pub(in crate::app) fn rename_end(&mut self, save: bool) {
        let Some(r) = self.rename.take() else { return };
        let title = r.draft.text.trim();
        if save && title != r.was {
            self.link.rename(r.sid, title);
            // The daemon lists the same name back; only it knows the automatic one an empty title goes back to.
            let o = self.world.entity_from_id(r.obj);
            if !title.is_empty() && o.is_alive() {
                o.set(model::Name { s: title.into() });
            }
        }
        if let Some(n) = self.rename_node(r.obj) {
            let n = self.world.entity_from_id(n);
            let te = n.try_cloned::<&TextEdit>().unwrap_or_default();
            let rest = TextEdit { caret: 0, anchor: 0, mark_lo: 0, mark_hi: 0, focused: false, ..te };
            if rest != te {
                n.set(rest);
            }
        }
        self.poke();
    }

    /// The rename's field: its row's `name` node in the tree, while the row is there.
    pub(in crate::app) fn rename_node(&self, obj: u64) -> Option<u64> {
        let tree = View::of(self.world.entity_from_id(self.ids.tree));
        tree.part(&format!("o{obj}"))?.part("name").map(|n| n.id())
    }

    /// Before the rows are filled: the selection moved off the renamed object (a press on another row, cmd+1-9), the
    /// object went, or the palette opened: the rename saves.
    pub(in crate::app) fn rename_settle(&mut self) {
        let Some(obj) = self.rename.as_ref().map(|r| r.obj) else { return };
        if self.selected() != Some(obj) || !self.world.entity_from_id(obj).is_alive() || self.palette.is_some() {
            self.rename_end(true);
        }
    }

    /// A press went down: anywhere outside the renamed row (the node under the pointer not in it), the rename saves.
    pub(in crate::app) fn rename_press(&mut self) {
        let Some(obj) = self.rename.as_ref().map(|r| r.obj) else { return };
        let tree = View::of(self.world.entity_from_id(self.ids.tree));
        let row = tree.part(&format!("o{obj}")).map(|r| r.id());
        let hit = self.world.lookup(APP).target(Hovered::id(), 0);
        let inside = hit.is_some_and(|h| {
            std::iter::successors(Some(h), |n| n.parent()).take(32).any(|n| Some(*n.id()) == row)
        });
        if !inside {
            self.rename_end(true);
        }
    }

    /// An `act.Edit` (ctx.Chat's) on the field: the input's moves, selections and erases along its one line; Return
    /// (`send`, `submit`) saves, Escape (`interrupt`) cancels; a new line and the model picker's keys do nothing here.
    pub(in crate::app) fn rename_edit(&mut self, op: &str) {
        let (select, name) = match op.strip_prefix("select_") {
            Some(n) => (true, n),
            None => (false, op),
        };
        match op {
            "send" | "submit" => return self.rename_end(true),
            "interrupt" => return self.rename_end(false),
            _ => {}
        }
        let now = self.time;
        let Some(r) = self.rename.as_mut() else { return };
        let d = &mut r.draft;
        if let Some(m) = Move::named(name) {
            let to = along(d, m);
            d.go(m, to, select);
        } else if let Some(m) = Move::erasing(op) {
            let to = along(d, m);
            d.erase(to);
        } else {
            return;
        }
        r.since = now;
        self.poke();
    }

    /// Run `f` on the field's words ([`App::with_draft`] while renaming: typed, marked, pasted and cut text): a name is one
    /// line, so a pasted line break or tab is a space.
    pub(in crate::app) fn rename_draft(&mut self, f: impl FnOnce(&mut Draft)) -> bool {
        let now = self.time;
        let Some(r) = self.rename.as_mut() else { return false };
        f(&mut r.draft);
        if r.draft.text.contains(['\n', '\t']) {
            // Same length, so the caret stays put.
            r.draft.text = r.draft.text.replace(['\n', '\t'], " ");
        }
        r.since = now;
        self.poke();
        true
    }

    /// What Command-C takes while renaming: the field's selection (None: nothing selected), pulsed on its words.
    pub(in crate::app) fn rename_copy(&self) -> Option<Option<(String, Copied)>> {
        let r = self.rename.as_ref()?;
        let node = self.rename_node(r.obj)?;
        let picked = r.draft.selected();
        Some((!picked.is_empty()).then(|| (picked.to_owned(), Copied::Field(node))))
    }

    /// The words row `e` shows while its name is being renamed (the marked text in) and its `TextEdit` units: caret,
    /// anchor, marked range (UTF-16). None for any other row.
    pub(in crate::app) fn rename_shown(&self, e: u64) -> Option<(String, [u32; 4])> {
        self.rename.as_ref().filter(|r| r.obj == e).map(|r| r.draft.shown())
    }

    /// Put the field's caret and selection (`units`, from [`Self::rename_shown`]) on the row's name node: focused while
    /// the window is key, blinking as the terminal's cursor does.
    pub(in crate::app) fn rename_caret(&self, row: View<'_>, [caret, anchor, lo, hi]: [u32; 4]) {
        let (Some(n), Some(r)) = (row.part("name"), self.rename.as_ref()) else { return };
        let blink = self.look.themes[usize::from(self.dark)].blink;
        let te = n.entity().try_cloned::<&TextEdit>().unwrap_or_default();
        let next = TextEdit { caret, anchor, mark_lo: lo, mark_hi: hi, focused: self.focused, blink, since: r.since, ..te };
        if next != te {
            n.set(next);
        }
    }
}
