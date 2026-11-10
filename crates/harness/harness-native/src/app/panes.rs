//! The panes (src/pane.rs): each pane node under `hn.Main` draws its own `(Shows, object)` with the prefab its Kind picks,
//! the focused one following the app's `(Selected, object)`; and every terminal's sync, drawing and input, which read
//! and write the `hn.Terminal` node's components rather than any one terminal's fields on the App.
use super::*;
use harness_native_ui_motion_api::CopyFlash;
use crate::pane::{
    MarkedText, PaneFocus, PaneNode, TermCursor, TermFocus, TermGrid, TermInput, TermMouse,
    TermNode, TermScroll, TermSelection, Unit,
};
use harness_native_flecs::ComponentId;
use harness_native_ui_layout_api::{ScrollExtent, ScrollState, ScrollerGrip, ScrollerPart, scroll_quietly};
use harness_native_ui_text_api::{Kip, KipStand};
use model::PaneKind;

/// The prefab that draws a pane of `kind` (None: nothing shown).
fn pane_prefab(kind: Option<PaneKind>) -> &'static str {
    match kind {
        Some(PaneKind::Run) => "hn.RunPane",
        Some(PaneKind::Archive) => "hn.ArchivePane",
        Some(PaneKind::Chat) => "hn.ChatPane",
        Some(_) => "hn.TermPane",
        None => "hn.EmptyPane",
    }
}

/// Kip on `node` as scene prefab `look` has him (its `Kip`, and its `KipStand` when it has one) in `mood` (None: the
/// look's), faded by `fade` (0 shown, 1 gone); or, with no look, taken off it: a node without a `Kip` draws nothing, and
/// off screen his clock stops (harness-native-render kips.rs).
pub(super) fn show_kip(node: View<'_>, look: Option<(&str, Option<&str>)>, fade: f32) {
    let e = node.entity();
    let Some((look, mood)) = look else {
        if e.has(Kip::id()) {
            _ = e.remove(Kip::id());
        }
        if e.has(KipStand::id()) {
            _ = e.remove(KipStand::id());
        }
        return;
    };
    let p = view::prefab(e.world(), look).unwrap_or_else(|| panic!("the scene has no {look} (Kip's look in the chrome)"));
    let mut k = p.try_cloned::<&Kip>().unwrap_or_else(|| panic!("{look} has no Kip"));
    if let Some(m) = mood {
        k.mood = m.into();
    }
    if e.try_cloned::<&Kip>().as_ref() != Some(&k) {
        e.set(k);
    }
    match p.try_cloned::<&KipStand>() {
        Some(s) if e.try_cloned::<&KipStand>() != Some(s) => _ = e.set(s),
        None if e.has(KipStand::id()) => _ = e.remove(KipStand::id()),
        _ => {}
    }
    let pose = harness_native_ui_motion_api::Pose { fade: fade.clamp(0.0, 1.0), ..Default::default() };
    if e.try_cloned::<&harness_native_ui_motion_api::Pose>().unwrap_or_default() != pose {
        e.set(pose);
    }
}

impl App {
    // ---- components ----

    fn get<T: ComponentId>(&self, e: u64) -> T {
        self.world.entity_from_id(e).try_cloned::<&T>().unwrap_or_default()
    }

    /// Set `v` on `e` when it differs.
    fn put<T: ComponentId + PartialEq>(&self, e: u64, v: T) {
        let x = self.world.entity_from_id(e);
        if x.is_alive() && x.try_cloned::<&T>().as_ref() != Some(&v) {
            x.set(v);
        }
    }

    // ---- panes ----

    /// The pane nodes, in order.
    pub(super) fn panes(&self) -> Vec<u64> {
        let mut out = Vec::new();
        self.world.entity_from_id(self.ids.main).each_child(|c| {
            if c.has(PaneNode::id()) {
                out.push(*c.id());
            }
        });
        out
    }

    pub(super) fn focused_pane(&self) -> Option<u64> {
        let panes = self.panes();
        panes
            .iter()
            .copied()
            .find(|&p| self.world.entity_from_id(p).has(PaneFocus::id()))
            .or_else(|| panes.first().copied())
    }

    /// The pane's `hn.Terminal`, if its prefab has one.
    fn terminal_of(&self, pane: u64) -> Option<u64> {
        View::of(self.world.entity_from_id(pane))
            .part("term")
            .filter(|t| t.entity().has(TermNode::id()))
            .map(View::id)
    }

    fn terminals(&self) -> Vec<u64> {
        self.panes().into_iter().filter_map(|p| self.terminal_of(p)).collect()
    }

    /// The terminal keys go to: the focused pane's.
    pub(super) fn focused_term(&self) -> Option<u64> {
        self.focused_pane().and_then(|p| self.terminal_of(p))
    }

    /// The object terminal `t`'s pane shows.
    fn shown_by(&self, t: u64) -> Option<u64> {
        let pane = self.world.entity_from_id(t).parent()?;
        model::shows(self.world.world_ref(), *pane.id()).map(|e| *e.id())
    }

    /// Keep the pane nodes in step with the model: at least one, the focused one showing the selection, each made from
    /// the prefab its object's Kind picks (made again from another when the Kind changes), its terminal's components
    /// there, and the run and group panes' lines and rows filled in.
    pub(super) fn sync_panes(&self) {
        let w = self.world.world_ref();
        let main = View::of(w.entity_from_id(self.ids.main));
        if self.panes().is_empty() {
            main.child("hn.TermPane").add(PaneNode::id()).add(PaneFocus::id());
        }
        let Some(focus) = self.focused_pane() else { return };
        let fe = w.entity_from_id(focus);
        if !fe.has(PaneFocus::id()) {
            fe.add(PaneFocus::id());
        }
        model::show(w, focus, self.selected());
        for p in self.panes() {
            let pe = w.entity_from_id(p);
            let shown = model::shows(w, p).map(|e| *e.id());
            let want = pane_prefab(shown.map(|e| model::pane_kind(w.entity_from_id(e))));
            let Some(proto) = view::prefab(w, want) else { continue };
            let pane = if pe.owns((flecs::IsA::ID, *proto.id())) {
                p
            } else {
                // Its object's Kind wants another prefab: a new node in its place, showing the same, keeping its focus.
                let n = main.child(want).add(PaneNode::id());
                if pe.has(PaneFocus::id()) {
                    n.add(PaneFocus::id());
                }
                // Where its back affordance went comes along.
                if let Some(b) = pe.target(crate::pane::Back::id(), 0) {
                    n.add((crate::pane::Back::id(), *b.id()));
                }
                model::show(w, n.id(), shown);
                let mut order = Vec::new();
                main.entity().each_child(|c| order.push(*c.id()));
                order.retain(|&c| c != n.id());
                if let Some(at) = order.iter().position(|&c| c == p) {
                    order[at] = n.id();
                }
                pe.destruct();
                main.entity().set_child_order(&order);
                n.id()
            };
            let focused = w.entity_from_id(pane).has(PaneFocus::id());
            if let Some(t) = self.terminal_of(pane) {
                let te = w.entity_from_id(t);
                if !te.has(TermSelection::id()) {
                    te.set(TermGrid::default())
                        .set(TermCursor::default())
                        .set(TermInput::default())
                        .set(TermSelection::default())
                        .set(TermScroll::default())
                        .set(TermMouse::default());
                }
                if te.has(TermFocus::id()) != focused {
                    if focused {
                        te.add(TermFocus::id());
                    } else {
                        te.remove(TermFocus::id());
                        te.remove(MarkedText::id());
                    }
                }
            }
            match shown.map(|e| (e, model::pane_kind(w.entity_from_id(e)))) {
                Some((e, PaneKind::Run)) => self.fill_run_pane(pane, e),
                Some((_, PaneKind::Archive)) => self.fill_archive_pane(pane),
                Some((e, PaneKind::Chat)) => self.fill_chat_pane(pane, e),
                None => self.fill_empty_pane(pane),
                _ => {}
            }
        }
    }

    /// The main card with nothing selected (no chats yet): Kip waving once the daemon's list is in, over the key that
    /// starts a chat. Before the list he is not there, so a launch does not flash him.
    fn fill_empty_pane(&self, pane: u64) {
        let w = self.world.world_ref();
        let up = w
            .lookup(model::LOCAL)
            .target(model::Link::id(), 0)
            .is_some_and(|t| *t.id() == *w.component_id::<model::Up>());
        let v = View::of(w.entity_from_id(pane));
        if let Some(k) = v.part("kip") {
            show_kip(k, up.then_some(("hn.KipEmpty", None)), 0.0);
        }
        if let Some(n) = v.part("note") {
            let key = self.hint("Picker");
            let said = match (up, key.is_empty()) {
                (false, _) => String::new(),
                (true, false) => format!("Start a chat with {key}"),
                (true, true) => "No chat open".to_owned(),
            };
            n.text(&said);
        }
    }

    /// A run's (or stage's) id: `r12`, `r12.1`.
    fn run_id(&self, e: u64) -> String {
        match self.run_of(e) {
            Some((n, Some(k))) => format!("r{n}.{k}"),
            Some((n, None)) => format!("r{n}"),
            None => String::new(),
        }
    }

    /// How a run or stage stands, for its summary and the title bar: `running · 4.2s`, `exited 0 · 340ms`, `skipped`.
    pub(super) fn run_state(&self, e: u64) -> String {
        let code = self.world.entity_from_id(e).try_cloned::<&model::ExitCode>().map(|c| c.code);
        let s = match self.status_of(e) {
            "exited" => format!("exited {}", code.unwrap_or(0)),
            s => s.to_owned(),
        };
        match self.run_took(e) {
            Some(t) if s != "pending" && s != "skipped" => format!("{s} \u{00B7} {t}"),
            _ => s,
        }
    }

    /// A run's directory (a stage's is its run's).
    pub(super) fn run_cwd(&self, e: u64) -> String {
        let o = self.world.entity_from_id(e);
        o.try_cloned::<&model::Cwd>()
            .or_else(|| o.parent().and_then(|p| p.try_cloned::<&model::Cwd>()))
            .map(|c| tilde(c.s.as_str()))
            .unwrap_or_default()
    }

    /// The run pane's stages (its run's, `e`'s lit); the title bar carries its id, command, state and directory.
    fn fill_run_pane(&self, pane: u64, e: u64) {
        let w = self.world.world_ref();
        let o = w.entity_from_id(e);
        let Some(info) = View::of(w.entity_from_id(pane)).part("info") else { return };
        let run = if o.has(model::Run::id()) { Some(o) } else { o.parent() };
        let mut stages: Vec<(u32, u64)> = Vec::new();
        if let Some(r) = run {
            r.each_child(|c| {
                if let Some(s) = c.try_cloned::<&model::StageNo>() {
                    stages.push((s.k, *c.id()));
                }
            });
        }
        stages.sort_unstable();
        if let Some(list) = info.part("stages") {
            for_each(
                list,
                "hn.PaneRow",
                stages.into_iter().map(|(_, c)| (format!("s{c}"), c)),
                |row, c, new| {
                    if new {
                        row.set(HoverAmt::default());
                    }
                    self.fill_pane_row(row, c, c == e);
                },
            );
        }
    }

    /// A pane row (hn.PaneRow, scene/widgets/panes.flecs): its run's id, command (one line), how it stands and how it
    /// ended, and whether it is the one shown.
    fn fill_pane_row(&self, row: View<'_>, e: u64, shown: bool) {
        let w = self.world.world_ref();
        let o = w.entity_from_id(e);
        let name =
            o.try_cloned::<&model::Name>().map(|n| n.s.as_str().to_owned()).unwrap_or_default();
        let name = clip_text(&name.split_whitespace().collect::<Vec<_>>().join(" "), 110);
        let code = o.try_cloned::<&model::ExitCode>().map(|c| c.code);
        let state = match self.status_of(e) {
            "running" => "running",
            "lost" => "lost",
            "killed" => "failed",
            _ if code.is_some_and(|c| c != 0) => "failed",
            _ => "exited",
        };
        let (id, meta) = (self.run_id(e), self.run_meta(e));
        row.props(
            "hn.PaneRow",
            &[
                ("obj", Prop::Uint(e)),
                ("name", Prop::Str(&name)),
                ("selected", Prop::Bool(shown)),
                ("id", Prop::Str(&id)),
                ("state", Prop::Str(state)),
                ("meta", Prop::Str(&meta)),
            ],
        );
    }

    /// Focus pane `p`: the selection follows what it shows.
    fn focus_pane(&mut self, p: u64) {
        if self.focused_pane() == Some(p) && self.world.entity_from_id(p).has(PaneFocus::id()) {
            return;
        }
        for q in self.panes() {
            let qe = self.world.entity_from_id(q);
            if q != p && qe.has(PaneFocus::id()) {
                qe.remove(PaneFocus::id());
            }
        }
        self.world.entity_from_id(p).add(PaneFocus::id());
        if let Some(e) = model::shows(self.world.world_ref(), p).map(|e| *e.id()) {
            self.select(Some(e));
        }
        self.poke();
    }

    /// The pane under the pointer.
    fn pane_at(&self) -> Option<u64> {
        let (x, y) = (self.pointer.x, self.pointer.y);
        self.panes().into_iter().find(|&p| {
            self.world.entity_from_id(p).try_cloned::<&Laid>().is_some_and(|l| l.contains(x, y))
        })
    }

    /// The terminal under the pointer (a few points around its grid count), of those that can be seen.
    fn term_at(&self) -> Option<u64> {
        let (x, y) = (self.pointer.x, self.pointer.y);
        self.terminals().into_iter().find(|&t| self.term_seen(t) && self.get::<TermGrid>(t).holds(x, y, 4.0))
    }

    /// Whether terminal `t` was painted this frame where it can be seen (`super::seen`): not Hidden or under a Hidden
    /// ancestor, not faded out, not clipped away. One that cannot be seen is neither laid out, attached, resized, drawn
    /// nor hit.
    fn term_seen(&self, t: u64) -> bool {
        let w = self.world.world_ref();
        let Some(frame) = w.try_get::<&harness_native_ui_draw_api::Paint>(|p| p.frame) else { return false };
        w.entity_from_id(t)
            .try_cloned::<&harness_native_ui_draw_api::Painted>()
            .is_some_and(|p| p.frame == frame && super::seen(&p))
    }

    /// A pane's object changed under the focused terminal: its selection, smooth scroll and mouse report start over.
    pub(super) fn reset_focused_term(&self) {
        if let Some(t) = self.focused_term() {
            self.put(t, TermSelection::default());
            self.put(t, TermScroll::default());
            self.put(t, TermMouse::default());
        }
    }

    // ---- the terminals ----

    /// Every terminal pane that can be seen this frame: its grid the size of its node, and its object's output followed (a
    /// session attached, a run or stage watched); runs no pane shows any more stop being followed.
    pub(super) fn sync_terms(&mut self) {
        let mut shown_runs: HashMap<u64, (u64, Option<u32>)> = HashMap::new();
        for p in self.panes() {
            let w = self.world.world_ref();
            let Some(e) = model::shows(w, p).map(|e| *e.id()) else { continue };
            let Some(t) = self.terminal_of(p) else { continue };
            let run = self.run_of(e);
            if let Some(r) = run {
                shown_runs.insert(e, r);
            }
            // Out of sight it keeps what it follows, but its grid is not laid out and nothing is attached or resized.
            if !self.term_seen(t) {
                continue;
            }
            let laid = w.entity_from_id(t).try_cloned::<&Laid>();
            let Some(l) = laid.filter(|l| l.w > 0.0 && l.h > 0.0) else { continue };
            // Fit the grid inside the padding, pinned to its top-left inset (snapped to a whole device pixel so every cell
            // starts on one); what is left of a cell goes to the right and bottom. Balancing it on both sides would
            // re-centre the text by half a cell each time the column count changes (a jolt at the end of a cmd+S fold).
            let [pl, pr, pt, pb] = self.look.pad;
            let (cols, rows) = self.grid.fit(l.w - pl - pr, l.h - pt - pb);
            let (gw, gh) = (f32::from(cols) * self.grid.cw, f32::from(rows) * self.grid.ch);
            let snap = |v: f32| (v * self.grid.scale).round() / self.grid.scale;
            self.put(
                t,
                TermGrid { x: snap(l.x + pl), y: snap(l.y + pt), w: gw, h: gh, cols, rows },
            );
            let o = w.entity_from_id(e);
            if let Some((n, stage)) = run {
                // A run (or stage): its output, read-only, from the start into a fresh parser the pane's size.
                if !self.watching.contains_key(&e) {
                    self.watching.insert(e, (n, stage));
                    self.run_terms.borrow_mut().remove(&e);
                    o.set(model::Term { cols, rows, scroll: 0, seq: 0 });
                    self.link.watch_run(n, stage);
                    continue;
                }
                if let Some(mut tm) =
                    o.try_cloned::<&model::Term>().filter(|tm| (tm.cols, tm.rows) != (cols, rows))
                {
                    tm.cols = cols;
                    tm.rows = rows;
                    o.set(tm);
                    if let Some(p) = self.run_terms.borrow_mut().get_mut(&e) {
                        p.resize(cols, rows);
                    }
                }
                continue;
            }
            let Some(sid) = self.session_of(e) else { continue };
            let running = self.status_of(e) == "running";
            // Attach when it is not: a running object again a second after its last try, an ended one once (its last screen).
            let last = self.tried.get(&sid).copied();
            let due =
                if running { last.is_none_or(|t| self.time - t > 1.0) } else { last.is_none() };
            if !self.link.attached(sid) && due {
                self.tried.insert(sid, self.time);
                // A fresh parser for the snapshot the attach starts with.
                self.terms.borrow_mut().remove(&sid);
                o.set(model::Term { cols, rows, scroll: 0, seq: 0 });
                self.link.attach(sid, cols, rows, false);
                continue;
            }
            let Some(mut tm) = o.try_cloned::<&model::Term>() else { continue };
            if (tm.cols, tm.rows) != (cols, rows) {
                tm.cols = cols;
                tm.rows = rows;
                o.set(tm);
                if let Some(p) = self.terms.borrow_mut().get_mut(&sid) {
                    p.resize(cols, rows);
                }
                self.link.resize(sid, cols, rows);
            }
        }
        let gone: Vec<(u64, (u64, Option<u32>))> = self
            .watching
            .iter()
            .filter(|(e, _)| !shown_runs.contains_key(e))
            .map(|(&e, &r)| (e, r))
            .collect();
        for (e, (n, stage)) in gone {
            self.watching.remove(&e);
            self.link.unwatch_run(n, stage);
            self.run_terms.borrow_mut().remove(&e);
        }
    }

    /// TermNode `t`'s parser map and key in it: a session's by its id, a run's or stage's by its entity.
    fn parser_of(&self, t: u64) -> Option<(model::Terms, u64)> {
        let e = self.shown_by(t)?;
        match self.session_of(e) {
            Some(sid) => Some((self.terms.clone(), sid)),
            None => self.run_of(e).map(|_| (self.run_terms.clone(), e)),
        }
    }

    fn with_parser<R>(&self, t: u64, f: impl FnOnce(&mut harness_vt::Term) -> R) -> Option<R> {
        let (terms, key) = self.parser_of(t)?;
        let mut map = terms.borrow_mut();
        map.get_mut(&key).map(f)
    }

    /// Whole rows terminal `t` is scrolled back (on its object's `Term`).
    fn scroll(&self, t: u64) -> usize {
        self.shown_by(t)
            .and_then(|e| self.world.entity_from_id(e).try_cloned::<&model::Term>())
            .map_or(0, |tm| tm.scroll as usize)
    }

    fn set_scroll(&mut self, t: u64, s: usize) {
        if let Some(e) = self.shown_by(t) {
            let o = self.world.entity_from_id(e);
            if let Some(mut tm) = o.try_cloned::<&model::Term>() {
                if tm.scroll as usize != s {
                    tm.scroll = s as u32;
                    o.set(tm);
                }
            }
        }
        self.poke();
    }

    /// Draw terminal `t`: snapshot its parser (the one reader) at its scroll, and write back what input reads.
    pub(super) fn emit_term(
        &mut self,
        t: u64,
        d: &mut Draw,
        sc: f32,
        upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8]),
    ) {
        let g = self.get::<TermGrid>(t);
        if g.w <= 0.0 {
            return;
        }
        let Some((terms, key)) = self.parser_of(t) else { return };
        let mut st = self.get::<TermScroll>(t);
        let gripped = self.term_scroller_grip(t, st);
        // Whole rows only: the grid snaps to its cells, and a wheel's fraction of a row waits in `frac`.
        let want = self.scroll(t);
        let mut map = terms.borrow_mut();
        let Some(parser) = map.get_mut(&key) else { return };
        parser.set_scrollback(want);
        let shown = parser.scrollback();
        let bar = parser.scrollbar().filter(|_| !parser.alternate_screen());
        let history = if self.stats_shown() { parser.history() } else { 0 };
        let snap = term::snapshot(parser, &self.look.themes[usize::from(self.dark)]);
        parser.set_scrollback(0);
        drop(map);
        self.stats_history(t, history);
        let moved = shown as i64 != st.shift;
        st.top = g.y;
        st.shift = shown as i64;
        (st.total, st.len) = bar.map_or((0, 0), |b| (b.total, b.len));
        st.row = self.feed_term_scroller(t, bar, moved, gripped);
        self.put(t, st);
        self.put(t, TermInput::pack(&snap.input));
        self.put(t, TermCursor { row: snap.input.cursor.0, col: snap.input.cursor.1 });
        let te = self.world.entity_from_id(t);
        let focus = te.has(TermFocus::id());
        let marked = te
            .try_cloned::<&MarkedText>()
            .map(|m| m.s.as_str().to_owned())
            .filter(|s| focus && !s.is_empty());
        let sel = self.get::<TermSelection>(t).get();
        let select = harness_native_render::select::Look::of(self.world.world_ref());
        let paint = term::Paint {
            origin: (g.x, st.top),
            scale: sc,
            focused: focus && self.focused && self.palette.is_none(),
            sel,
            scroll: shown,
            theme: &self.look.themes[usize::from(self.dark)],
            time: self.time,
            marked: marked.as_deref(),
            select: &select,
            select_ink: select.colour(self.term_ground_dark()),
            ground_dark: self.term_ground_dark(),
            flash: te.try_cloned::<&CopyFlash>(),
        };
        let paint_ = self.paint.get_mut();
        term::draw(
            d,
            &mut self.grid,
            &mut paint_.fonts,
            &mut paint_.cache,
            &snap,
            &paint,
            upload,
        );
        if self.grid.animating {
            // The cursor fades on the app clock: keep drawing.
            self.poke_why("cursor blink");
        }
    }

    /// A drag or a page on terminal `t`'s overlay scroller (ui.Scroller, which wrote its `ScrollState`): the rows back it
    /// points at become the terminal's scroll. Whether the scroller holds the scroll this frame (then its `ScrollState` is
    /// its own, not fed).
    fn term_scroller_grip(&mut self, t: u64, st: TermScroll) -> bool {
        let te = self.world.entity_from_id(t);
        if !te.try_cloned::<&ScrollerGrip>().is_some_and(|g| g.drag != 0 || g.page != 0) {
            return false;
        }
        let Some(s) = te.try_cloned::<&ScrollState>().filter(|_| st.row > 0.0 && st.total > st.len) else { return true };
        let top = st.total - st.len;
        let offset = ((s.y / st.row).round().max(0.0) as u64).min(top);
        let back = usize::try_from(top - offset).unwrap_or(0);
        if back != self.scroll(t) {
            self.set_scroll(t, back);
        }
        true
    }

    /// Feed terminal `t`'s overlay scroller from where its viewport sits in its history (`bar`, rows; None on the
    /// alternate screen, which has no history): its extent and its scroll in points, a row being the node's height over
    /// the rows shown. A scroll through the history (`moved`) shows the scroller; output arriving on the live screen moves
    /// it quietly. While the scroller holds the scroll (`gripped`) its `ScrollState` is left to it. Returns the points a
    /// row is.
    fn feed_term_scroller(&self, t: u64, bar: Option<harness_vt::Scrollbar>, moved: bool, gripped: bool) -> f32 {
        let te = self.world.entity_from_id(t);
        let Some(l) = te.try_cloned::<&Laid>().filter(|l| l.h > 0.0) else { return 0.0 };
        let (h, y, row) = match bar.filter(|b| b.len > 0 && b.total > b.len) {
            Some(b) => {
                let row = l.h / b.len as f32;
                (b.total as f32 * row, b.offset as f32 * row, row)
            }
            None => (l.h, 0.0, 0.0),
        };
        let x = ScrollExtent { w: l.w, h };
        if te.try_cloned::<&ScrollExtent>() != Some(x) {
            te.set(x);
        }
        let s = ScrollState { x: 0.0, y };
        if !gripped && te.try_cloned::<&ScrollState>() != Some(s) {
            if moved {
                te.set(s);
            } else {
                scroll_quietly(te, s);
            }
        }
        // Drawn in the terminal ground's appearance, as a chat's transcript is.
        let scheme = harness_native_ui_look_api::Scheme { dark: self.term_ground_dark() };
        if te.try_cloned::<&harness_native_ui_look_api::Scheme>() != Some(scheme) {
            te.set(scheme);
        }
        row
    }

    // ---- input ----

    fn input_of(&self, t: u64) -> term::Input {
        self.get::<TermInput>(t).unpack(self.get::<TermCursor>(t))
    }

    fn set_sel(&self, t: u64, s: Option<Sel>) {
        let mut ts = self.get::<TermSelection>(t);
        ts.put(s);
        self.put(t, ts);
    }

    fn set_pointer(&mut self, x: f32, y: f32) {
        let z = self.zoom();
        self.pointer.x = x / z;
        self.pointer.y = y / z;
    }

    /// The viewport cell of terminal `t` under the pointer (row, column), clamped to the grid when `clamp`, else only
    /// within it.
    fn pointer_cell(&self, t: u64, clamp: bool) -> Option<(i64, u16)> {
        let g = self.get::<TermGrid>(t);
        let (x, y) = (self.pointer.x, self.pointer.y);
        if g.w <= 0.0 || (!clamp && !g.holds(x, y, 4.0)) {
            return None;
        }
        let (cols, rows) = self.input_of(t).size;
        let top = self.get::<TermScroll>(t).top;
        let col = ((x - g.x) / self.grid.cw).floor().clamp(0.0, f32::from(cols.max(1) - 1)) as u16;
        let row = ((y - top) / self.grid.ch).floor().clamp(0.0, f32::from(rows.max(1) - 1)) as i64;
        Some((row, col))
    }

    /// The pointer's cell as a mouse report to `t` addresses it (viewport column and row).
    fn report_cell(&self, t: u64) -> Option<(u16, u16)> {
        self.pointer_cell(t, true).map(|(r, c)| (c, r as u16))
    }

    /// Whether the pointer goes to `t`'s program: it turned a mouse mode on, and Shift is not held to select instead.
    fn reporting(&self, t: u64, m: KeyMods) -> bool {
        self.input_of(t).modes.mouse != MouseTracking::None && !m.shift && self.palette.is_none()
    }

    fn report_mouse(&mut self, t: u64, b: Button, act: MouseAct, m: KeyMods) {
        let Some((col, row)) = self.report_cell(t) else { return };
        let mut tm = self.get::<TermMouse>(t);
        if act == MouseAct::Motion && tm.reported == Some((col, row)) {
            return;
        }
        tm.reported = Some((col, row));
        self.put(t, tm);
        let bytes = term::mouse(b, act, col, row, m, &self.input_of(t).modes);
        if !bytes.is_empty() {
            self.send_to(t, &bytes);
        }
    }

    /// The pointer's shape a terminal's drag holds (src/app/pointer.rs): a selection, the I-beam; a press reported to its
    /// program, the arrow.
    pub(super) fn term_drag_pointer(&self) -> Option<(Pointer, &'static str)> {
        if self.term_with(|a, t| a.get::<TermSelection>(t).selecting).is_some() {
            Some((Pointer::Text, "drag: terminal selection"))
        } else if self.term_with(|a, t| a.get::<TermMouse>(t).report.is_some()).is_some() {
            Some((Pointer::Arrow, "drag: reported to the program"))
        } else {
            None
        }
    }

    /// The pointer's shape over a terminal's grid (src/app/pointer.rs), where `pointer_down` takes a press: the I-beam, or
    /// the arrow while a press there goes to the program (it turned a mouse mode on and Shift is not held).
    pub(super) fn term_zone_pointer(&self) -> Option<(Pointer, &'static str)> {
        let t = self.term_at()?;
        self.pointer_cell(t, false)?;
        Some(if self.reporting(t, self.mods) {
            (Pointer::Arrow, "terminal, mouse reported to the program")
        } else {
            (Pointer::Text, "terminal")
        })
    }

    /// The terminal holding a mouse report or a drag, if any.
    fn term_with(&self, f: impl Fn(&Self, u64) -> bool) -> Option<u64> {
        self.terminals().into_iter().find(|&t| f(self, t))
    }

    /// The pointer moved to `(x, y)` (window points).
    pub fn pointer_move(&mut self, x: f32, y: f32, m: KeyMods) {
        self.set_pointer(x, y);
        self.mods = m;
        if self.drag.active() {
            self.side_drag_move();
            return;
        }
        if self.web_dragging() {
            self.web_drag_move();
            return;
        }
        // Over a live browser page, or holding a press on one: the page's (app/browser.rs).
        if self.browser_pointer_move(m) {
            self.poke();
            return;
        }
        if self.chat_pointer_drag() {
            return;
        }
        if let Some(t) = self.term_with(|a, t| a.get::<TermMouse>(t).report.is_some()) {
            if let Some(b) = self.get::<TermMouse>(t).button() {
                self.report_mouse(t, b, MouseAct::Motion, m);
            }
        } else if let Some(t) = self.term_with(|a, t| a.get::<TermSelection>(t).selecting) {
            if let Some((vr, col)) = self.pointer_cell(t, true) {
                let ts = self.get::<TermSelection>(t);
                let at = (vr - self.get::<TermScroll>(t).shift, col);
                let span = self.unit_span(t, at, ts.unit);
                let (a0, a1) = ts.anchor;
                self.set_sel(
                    t,
                    Some(if span.0 < a0 {
                        Sel { a: a1, b: span.0 }
                    } else {
                        Sel { a: a0, b: span.1 }
                    }),
                );
            }
        } else if let Some(t) = self.term_at()
            && self.input_of(t).modes.mouse == MouseTracking::Any
            && self.reporting(t, m)
            && self.pointer_cell(t, false).is_some()
        {
            self.report_mouse(t, Button::None, MouseAct::Motion, m);
        }
        self.poke();
    }

    /// A button went down at `(x, y)`: the `clicks`th in a row.
    pub fn pointer_down(&mut self, x: f32, y: f32, b: Button, clicks: isize, m: KeyMods) {
        self.set_pointer(x, y);
        self.mods = m;
        self.poke();
        self.press = (clicks, m);
        // On the sidebar's right edge (or the window's left edge, folded): drag it wider, narrower, shut or open.
        if b == Button::Left && self.on_side_edge() {
            self.side_drag_start();
            return;
        }
        // On the browser pane's left edge: drag it wider or narrower (app/browser/drag.rs).
        if b == Button::Left && self.on_web_edge() {
            self.web_drag_start();
            return;
        }
        // A right press or a control-click on a sidebar row opens its menu at the pointer, the selection left where it is;
        // a press off an open one closes it and goes no further (row_menu.rs).
        if self.row_menu_down(x, y, b, m) {
            return;
        }
        if b == Button::Left {
            self.pointer.down = true;
            self.pointer.went_down = true;
        }
        // A press on the open row menu is its items' (the UI layer's press), not a pane's or a terminal's under it.
        if self.on_row_menu() {
            return;
        }
        // A press off the row whose name is being renamed saves the rename (chat/rename.rs), and does what it does.
        self.rename_press();
        if self.palette.is_some() {
            return;
        }
        // A press on a live browser page focuses it and goes there; one elsewhere takes the keyboard from it.
        if self.browser_pointer_down(b, clicks, m) {
            return;
        }
        // A press on a scroll's overlay scroller is the scroller's (ui.Scroller: the UI layer drags or pages it), not the
        // pane's, its transcript's or its terminal's.
        if self.world.lookup(APP).target(Hovered::id(), 0).is_some_and(|h| h.has(ScrollerPart::id())) {
            return;
        }
        // A press in another pane focuses it (the selection follows what it shows), and takes the keyboard from the tree.
        if let Some(p) = self.pane_at() {
            self.tree_focus = false;
            self.focus_pane(p);
        }
        if b == Button::Left && self.chat_pointer_down(clicks) {
            return;
        }
        // A right press in a cell's context menu's region opens it at the pointer.
        if b == Button::Right && self.genui_context() {
            return;
        }
        let Some(t) = self.term_at() else { return };
        let Some((vr, col)) = self.pointer_cell(t, false) else { return };
        if self.reporting(t, m) {
            self.set_sel(t, None);
            self.put(t, TermMouse { report: Some(TermMouse::code(b)), reported: None });
            self.report_mouse(t, b, MouseAct::Press, m);
            return;
        }
        if b != Button::Left {
            return;
        }
        if m.alt && !m.cmd {
            self.click_move(t, vr, col);
            return;
        }
        let at = (vr - self.get::<TermScroll>(t).shift, col);
        let unit = Unit::of_clicks(clicks);
        let anchor = self.unit_span(t, at, unit);
        self.put(t, TermSelection { sel: Some(anchor), selecting: true, unit, anchor });
    }

    pub fn pointer_up(&mut self, x: f32, y: f32, b: Button, m: KeyMods) {
        self.set_pointer(x, y);
        self.mods = m;
        if self.drag.active() {
            self.side_drag_end();
            return;
        }
        if self.web_dragging() {
            self.web_drag_end();
            return;
        }
        if b == Button::Left {
            self.pointer.down = false;
            self.pointer.went_up = true;
        }
        if self.browser_pointer_up(b, m) {
            self.poke();
            return;
        }
        if b == Button::Left && self.chat_pointer_up() {
            return;
        }
        let code = TermMouse::code(b);
        if let Some(t) = self.term_with(|a, t| a.get::<TermMouse>(t).report == Some(code)) {
            self.put(t, TermMouse { report: None, ..self.get::<TermMouse>(t) });
            self.report_mouse(t, b, MouseAct::Release, m);
        } else if b == Button::Left
            && let Some(t) = self.term_with(|a, t| a.get::<TermSelection>(t).selecting)
        {
            let mut ts = self.get::<TermSelection>(t);
            ts.selecting = false;
            if ts.unit == Unit::Cell && ts.sel.is_some_and(|(a, b)| a == b) {
                ts.sel = None;
            }
            self.put(t, ts);
        }
        self.poke();
    }

    /// Option-click: move `t`'s cursor to viewport cell `(row, col)` with arrow keys (along the line in a shell, which
    /// wraps a long prompt; up/down then across in a full-screen program).
    fn click_move(&mut self, t: u64, row: i64, col: u16) {
        if self.get::<TermScroll>(t).shift != 0 {
            return;
        }
        let input = self.input_of(t);
        let modes = input.modes;
        let (crow, ccol) = (i64::from(input.cursor.0), i64::from(input.cursor.1));
        let cols = i64::from(input.size.0);
        let col = i64::from(col);
        let mut out = Vec::new();
        let mut step = |code: u16, n: i64, keys: &mut term::Keys| {
            out.extend(keys.press(code, &modes).repeat(n.unsigned_abs() as usize))
        };
        if modes.alt_screen {
            step(if row > crow { term::DOWN } else { term::UP }, row - crow, &mut self.keys);
            step(if col > ccol { term::RIGHT } else { term::LEFT }, col - ccol, &mut self.keys);
        } else {
            let d = (row - crow) * cols + (col - ccol);
            step(if d > 0 { term::RIGHT } else { term::LEFT }, d, &mut self.keys);
        }
        self.set_sel(t, None);
        self.send_to(t, &out);
    }

    /// Screen row `s` of `t` (negative: history) as the terminal holds it.
    fn row_at(&self, t: u64, s: i64) -> Option<harness_vt::Row> {
        self.with_parser(t, |p| {
            p.set_scrollback(usize::try_from(-s).unwrap_or(0));
            let n = p.scrollback() as i64;
            let (rows, _) = p.rows();
            p.set_scrollback(0);
            usize::try_from(s + n).ok().and_then(|i| rows.into_iter().nth(i))
        })
        .flatten()
    }

    /// The span a press at screen cell `at` of `t` selects by `unit`: the cell, its word, or its line (soft wraps followed).
    fn unit_span(&self, t: u64, at: (i64, u16), unit: Unit) -> ((i64, u16), (i64, u16)) {
        match unit {
            Unit::Cell => (at, at),
            Unit::Word => self
                .row_at(t, at.0)
                .map(|row| {
                    let cells: Vec<&str> = row.cells.iter().map(|c| c.text.as_str()).collect();
                    let (a, b) = term::word_span(&cells, usize::from(at.1));
                    ((at.0, a as u16), (at.0, b as u16))
                })
                .unwrap_or((at, at)),
            Unit::Line => {
                let (mut r0, mut r1) = (at.0, at.0);
                for _ in 0..256 {
                    if !self.row_at(t, r0 - 1).is_some_and(|r| r.wrapped) {
                        break;
                    }
                    r0 -= 1;
                }
                for _ in 0..256 {
                    if !self.row_at(t, r1).is_some_and(|r| r.wrapped) {
                        break;
                    }
                    r1 += 1;
                }
                ((r0, 0), (r1, self.input_of(t).size.0.saturating_sub(1)))
            }
        }
    }

    /// Command-A: everything in the focused terminal, history and screen.
    pub(super) fn select_all(&mut self) {
        if self.palette.is_none() && self.browser_select_all() {
            return;
        }
        if self.chat_select_all() {
            return;
        }
        let Some(t) = self.focused_term() else { return };
        let input = self.input_of(t);
        let (cols, rows) = input.size;
        if rows == 0 {
            return;
        }
        let all = ((-(input.history as i64), 0), (i64::from(rows) - 1, cols.saturating_sub(1)));
        self.put(
            t,
            TermSelection { sel: Some(all), selecting: false, unit: Unit::Cell, anchor: all },
        );
    }

    /// A scroll of `dy` (points when `precise`, else lines; positive: back into the history) on the terminal under the
    /// pointer (else the focused one). The primary screen scrolls its history a whole row at a time (a trackpad's points
    /// add up in `frac` until they make a row); a program with a mouse mode
    /// gets wheel reports, a full-screen one arrows.
    pub fn wheel(&mut self, dy: f32, precise: bool, m: KeyMods) {
        let Some(t) = self.term_at().or_else(|| self.focused_term()) else { return };
        let lines = if precise { dy / self.grid.ch } else { dy };
        let input = self.input_of(t);
        let modes = input.modes;
        let report = self.reporting(t, m);
        let mut st = self.get::<TermScroll>(t);
        if report || modes.alt_screen {
            st.wheel += lines;
            let n = st.wheel.trunc();
            if n == 0.0 {
                self.put(t, st);
                return;
            }
            st.wheel -= n;
            self.put(t, st);
            let count = n.abs() as usize;
            let bytes = if report {
                let (col, row) = self.report_cell(t).unwrap_or((0, 0));
                term::mouse(
                    if n > 0.0 { Button::WheelUp } else { Button::WheelDown },
                    MouseAct::Press,
                    col,
                    row,
                    m,
                    &modes,
                )
                .repeat(count)
            } else {
                self.keys.press(if n > 0.0 { term::UP } else { term::DOWN }, &modes).repeat(count)
            };
            self.send_to(t, &bytes);
            return;
        }
        let max = input.history as f32;
        let s = (self.scroll(t) as f32 + st.frac + lines).clamp(0.0, max);
        let s = if precise { s } else { s.round() };
        let whole = s.floor();
        st.frac = s - whole;
        self.put(t, st);
        self.set_scroll(t, whole as usize);
    }

    /// Typing into the focused terminal: its selection goes and it scrolls to the bottom.
    pub(super) fn typed(&mut self) {
        let Some(t) = self.focused_term() else { return };
        self.set_sel(t, None);
        let mut st = self.get::<TermScroll>(t);
        st.frac = 0.0;
        self.put(t, st);
        self.set_scroll(t, 0);
    }

    /// Bytes into the program of the focused terminal.
    pub(super) fn send(&mut self, bytes: &[u8]) {
        if let Some(t) = self.focused_term() {
            self.send_to(t, bytes);
        }
    }

    /// Bytes into the program terminal `t` shows (a run's output is read-only).
    fn send_to(&mut self, t: u64, bytes: &[u8]) {
        if let Some(sid) = self.shown_by(t).and_then(|e| self.session_of(e)) {
            self.link.input(sid, bytes);
        }
    }

    /// A key: the binding it matches in the active contexts runs (scene/20-keys.flecs: the palette's, the Archive's, the
    /// tree's, natural text editing in the terminal's); a key none takes goes to the palette's or the Archive's search, or
    /// (handing the keyboard back from the tree) to the focused terminal's program in its own encoding.
    /// A key from the window's keyDown: its binding's action when one takes it (a selector, `copy:`, is the window's to
    /// send up the responder chain), else typing for whatever has the keyboard.
    pub fn key(&mut self, k: &term::KeyIn<'_>) -> super::Outcome {
        self.poke_why("key");
        // An open row menu has the keys while it is open, as NSMenu has (row_menu.rs).
        if self.row_menu_key(k) {
            return super::Outcome::None;
        }
        // An element of a cell's UI that has the keys hears them first (genui/common.rs).
        if self.genui_common_key(k) {
            return super::Outcome::None;
        }
        if k.action != KeyAction::Release
            && let Some(c) = crate::keys::Chord::of(k.code, k.unshifted, k.mods)
        {
            let out = self.dispatch(&c);
            if out != super::Outcome::None {
                return out;
            }
        }
        self.key_unbound(k);
        super::Outcome::None
    }

    /// A key no binding took: the palette's query, the Archive's search, the chat's input, else the focused terminal.
    fn key_unbound(&mut self, k: &term::KeyIn<'_>) {
        // A browser view with the keyboard (or the pane's address field) takes every key ctx.Browser left.
        if self.palette.is_none() && self.browser_key(k) {
            return;
        }
        if k.action != KeyAction::Release {
            if self.palette.is_some() {
                self.palette_type(k.text);
                return;
            }
            if self.archive_focused() {
                if !k.mods.cmd && !k.mods.ctrl {
                    self.archive_search(super::archive::SearchEdit::Type(k.text));
                }
                return;
            }
            if self.tree_focus && !k.mods.cmd && !k.mods.ctrl {
                self.tree_focus = false;
            }
            if self.renaming() || self.chat_focused() || self.genui_editing() {
                if !k.mods.cmd && !k.mods.ctrl {
                    self.chat_type(k.text);
                }
                return;
            }
        } else if self.palette.is_some() || self.renaming() || self.chat_focused() || self.genui_editing() {
            return;
        }
        let Some(t) = self.focused_term() else { return };
        let modes = self.input_of(t).modes;
        if k.action == KeyAction::Release {
            // Only keys whose press the program heard; the encoder sends nothing unless kitty's event types are on.
            if let Some(i) = self.pressed.iter().position(|&c| c == k.code) {
                self.pressed.swap_remove(i);
                let bytes = self.keys.encode(k, &modes);
                self.send_to(t, &bytes);
            }
            return;
        }
        let bytes = self.keys.encode(k, &modes);
        if bytes.is_empty() {
            return;
        }
        if !self.pressed.contains(&k.code) {
            self.pressed.push(k.code);
        }
        self.typed();
        self.send_to(t, &bytes);
    }

    /// Text an input method, the emoji picker or dictation committed: sent as typed to the focused terminal.
    pub fn commit_text(&mut self, text: &str) {
        // Typing while a row menu is open goes nowhere, as under an open NSMenu.
        if self.row_menu_open() {
            return;
        }
        if self.palette.is_none() && self.browser_commit(text) {
            return;
        }
        self.set_marked(None);
        if let Some(p) = self.palette.as_mut() {
            if p.mode == super::Mode::Commands {
                p.query.push_str(text);
                p.at = 0;
            }
            return;
        }
        if self.archive_text(text) || self.chat_type(text) {
            return;
        }
        self.typed();
        self.send(text.as_bytes());
    }

    /// An input method's uncommitted text on the focused terminal (None: none).
    pub fn set_marked(&mut self, text: Option<String>) {
        if self.palette.is_none() && self.browser_set_marked(text.as_deref()) {
            return;
        }
        if self.chat_set_marked(text.as_deref()) {
            return;
        }
        if let Some(t) = self.focused_term() {
            let te = self.world.entity_from_id(t);
            match text.filter(|s| !s.is_empty()) {
                Some(s) => self.put(t, MarkedText { s: s.as_str().into() }),
                None if te.has(MarkedText::id()) => {
                    te.remove(MarkedText::id());
                }
                None => {}
            }
        }
        self.poke();
    }

    pub fn marked(&self) -> Option<String> {
        if let Some(m) = self.browser_marked().filter(|_| self.palette.is_none()) {
            return m;
        }
        if let Some(m) = self.chat_marked() {
            return m;
        }
        let t = self.focused_term()?;
        self.world
            .entity_from_id(t)
            .try_cloned::<&MarkedText>()
            .map(|m| m.s.as_str().to_owned())
            .filter(|s| !s.is_empty())
    }

    /// Where typed text goes (window points): the cell after the focused terminal's cursor and any marked text, for the
    /// input method's candidate window.
    pub fn caret_rect(&mut self) -> Option<[f32; 4]> {
        if let Some(r) = self.browser_caret_rect().filter(|_| self.palette.is_none()) {
            return r;
        }
        if let Some(r) = self.chat_caret_rect() {
            return r;
        }
        let t = self.focused_term()?;
        let g = self.get::<TermGrid>(t);
        if g.w <= 0.0 {
            return None;
        }
        let c = self.get::<TermCursor>(t);
        let st = self.get::<TermScroll>(t);
        let z = self.zoom();
        let col = f32::from(c.col) + self.marked().as_deref().map_or(0, term::columns) as f32;
        let x = g.x + col * self.grid.cw;
        let y = st.top + (i64::from(c.row) + st.shift) as f32 * self.grid.ch;
        Some([x * z, y * z, self.grid.cw * z, self.grid.ch * z])
    }

    /// What Command-C puts on the pasteboard: the focused chat's selection, else the focused terminal's (None: nothing
    /// selected, so it does nothing). What it took pulses (`CopyFlash`, harness-native-render select.rs).
    pub fn copy(&mut self) -> Option<String> {
        // A browser view with the keyboard copies onto the pasteboard itself.
        if self.palette.is_none() && self.browser_copy() {
            return None;
        }
        if let Some(c) = self.chat_copy() {
            let (text, what) = c?;
            self.chat_flash(&what);
            self.poke();
            return Some(text);
        }
        let t = self.focused_term()?;
        let sel = self.get::<TermSelection>(t).get()?;
        let text = self.with_parser(t, |p| term::selection_text(p, sel.ordered())).filter(|s| !s.is_empty())?;
        crate::audio::cue("copy");
        let pulse = harness_native_render::select::Look::of(self.world.world_ref()).kick();
        self.put(t, pulse);
        self.poke();
        Some(text)
    }

    /// What Command-X takes out for the pasteboard: the focused chat input's selection (None: nothing editable is
    /// selected; a terminal's or a transcript's selection is read-only, so it stays).
    pub fn cut(&mut self) -> Option<String> {
        self.poke();
        if self.palette.is_none() && self.browser_cut() {
            return None;
        }
        self.chat_cut()
    }

    /// Pictures pasted (Command-V of a screenshot, a browser's Copy Image, several copied at once), as PNG bytes: each
    /// saved under `$HARNESS_HOME/pastes`, then all attached to the focused chat's message, else their paths pasted
    /// ([`Self::paste_paths`]).
    pub fn paste_pictures(&mut self, pngs: &[Vec<u8>]) {
        let dir = crate::link::home().join("pastes");
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let mut paths = Vec::new(); // alloc: a paste's few pictures
        for (i, png) in pngs.iter().enumerate() {
            let path = dir.join(format!("{ms}-{i}.png"));
            if let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, png)) {
                eprintln!("harness-native: paste: saving the picture to {}: {e}", path.display());
                continue;
            }
            paths.push(path.to_string_lossy().into_owned());
        }
        if !paths.is_empty() && !self.chat_attach(&paths) {
            self.paste_paths(&paths);
        }
    }

    /// Files pasted (a Finder copy: their paths): pictures into a chat's message ([`Self::chat_attach`]); else into a
    /// terminal as words its shell reads back as those paths (quoted where they need it), anywhere else as the paths, a
    /// space apart.
    pub fn paste_paths(&mut self, paths: &[String]) {
        if self.chat_attach(paths) {
            return;
        }
        let terminal = self.palette.is_none() && !self.archive_focused() && !self.chat_focused() && !self.renaming();
        let words: Vec<String> = paths.iter().map(|p| if terminal { term::shell_quote(p) } else { p.clone() }).collect();
        self.paste(&words.join(" "));
    }

    /// Text pasted (Command-V) into whatever has the keyboard: the palette's query (its first line), the Archive's search,
    /// the focused chat's input, else the focused terminal (bracketed when its program asked).
    pub fn paste(&mut self, text: &str) {
        if self.palette.is_none() && self.browser_paste(text) {
            return;
        }
        if self.palette.is_some() {
            if let Some(p) = self.palette.as_mut().filter(|p| p.mode == super::Mode::Commands) {
                p.query.push_str(text.lines().next().unwrap_or(""));
            }
            self.poke();
            return;
        }
        if self.archive_text(text) || self.chat_type(text) {
            return;
        }
        let Some(t) = self.focused_term() else { return };
        let bracketed = self.input_of(t).modes.bracketed_paste;
        self.typed();
        self.send(&term::paste(text, bracketed));
    }

    /// The focused terminal's grid in cells (a new session starts that size).
    pub(super) fn focused_cells(&self) -> Option<(u16, u16)> {
        let g = self.get::<TermGrid>(self.focused_term()?);
        (g.cols > 0 && g.rows > 0).then_some((g.cols, g.rows))
    }
}
