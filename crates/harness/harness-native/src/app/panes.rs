//! The panes (src/pane.rs): each pane node under `hn.Main` draws its own `(Shows, object)` with the prefab its Kind picks,
//! the focused one following the app's `(Selected, object)`; and every terminal's sync, drawing and input, which read
//! and write the `hn.Terminal` node's components rather than any one terminal's fields on the App.
use super::*;
use crate::pane::{MarkedText, PaneNode, PaneFocus, TermCursor, TermFocus, TermGrid, TermInput, TermMouse, TermScroll, TermSelection, TermNode, Unit};
use harness_native_flecs::ComponentId;
use model::PaneKind;

/// The prefab that draws a pane of `kind` (None: nothing shown).
fn pane_prefab(kind: Option<PaneKind>) -> &'static str {
    match kind {
        Some(PaneKind::Run) => "hn.RunPane",
        Some(PaneKind::Archive) => "hn.ArchivePane",
        _ => "hn.TermPane",
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
        panes.iter().copied().find(|&p| self.world.entity_from_id(p).has(PaneFocus::id())).or_else(|| panes.first().copied())
    }

    /// The pane's `hn.Terminal`, if its prefab has one.
    fn terminal_of(&self, pane: u64) -> Option<u64> {
        View::of(self.world.entity_from_id(pane)).part("term").filter(|t| t.entity().has(TermNode::id())).map(View::id)
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
                    te.set(TermGrid::default()).set(TermCursor::default()).set(TermInput::default()).set(TermSelection::default()).set(TermScroll::default()).set(TermMouse::default());
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
                _ => {}
            }
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
        o.try_cloned::<&model::Cwd>().or_else(|| o.parent().and_then(|p| p.try_cloned::<&model::Cwd>())).map(|c| tilde(c.s.as_str())).unwrap_or_default()
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
        let (clear, chosen) = (tone(w, "clear"), tone(w, "selected"));
        if let Some(list) = info.part("stages") {
            for_each(list, "hn.PaneRow", stages.into_iter().map(|(_, c)| (format!("s{c}"), c)), |row, c, new| {
                if new {
                    row.set(HoverAmt::default());
                }
                row.action(&format!("select:{c}"));
                set_if(row, if c == e { chosen } else { clear });
                self.fill_pane_row(row, c);
            });
        }
    }

    /// A pane row's dot, id, command (one line) and how it ended.
    fn fill_pane_row(&self, row: View<'_>, e: u64) {
        let o = self.world.entity_from_id(e);
        let name = o.try_cloned::<&model::Name>().map(|n| n.s.as_str().to_owned()).unwrap_or_default();
        if let Some(p) = row.part("dot") {
            set_if(p, self.dot_tone(e));
        }
        if let Some(p) = row.part("id") {
            p.text(&self.run_id(e));
        }
        if let Some(p) = row.part("name") {
            p.text(&clip_text(&name.split_whitespace().collect::<Vec<_>>().join(" "), 110));
        }
        if let Some(p) = row.part("meta") {
            p.text(&self.run_meta(e));
        }
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
        self.panes().into_iter().find(|&p| self.world.entity_from_id(p).try_cloned::<&Laid>().is_some_and(|l| l.contains(x, y)))
    }

    /// The terminal under the pointer (a few points around its grid count).
    fn term_at(&self) -> Option<u64> {
        let (x, y) = (self.pointer.x, self.pointer.y);
        self.terminals().into_iter().find(|&t| self.get::<TermGrid>(t).holds(x, y, 4.0))
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

    /// Every terminal pane: its grid the size of its node, and its object's output followed (a session attached, a run
    /// or stage watched); runs no pane shows any more stop being followed.
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
            let laid = w.entity_from_id(t).try_cloned::<&Laid>();
            let Some(l) = laid.filter(|l| l.w > 0.0 && l.h > 0.0) else { continue };
            // Fit the grid inside the padding, pinned to its top-left inset (snapped to a whole device pixel so every cell
            // starts on one); what is left of a cell goes to the right and bottom. Balancing it on both sides would
            // re-centre the text by half a cell each time the column count changes (a jolt at the end of a cmd+S fold).
            let (cols, rows) = self.grid.fit(l.w - 2.0 * TERM_PAD.0, l.h - 2.0 * TERM_PAD.1);
            let (gw, gh) = (f32::from(cols) * self.grid.cw, f32::from(rows) * self.grid.ch);
            let snap = |v: f32| (v * self.grid.scale).round() / self.grid.scale;
            self.put(t, TermGrid { x: snap(l.x + TERM_PAD.0), y: snap(l.y + TERM_PAD.1), w: gw, h: gh, cols, rows });
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
                if let Some(mut tm) = o.try_cloned::<&model::Term>().filter(|tm| (tm.cols, tm.rows) != (cols, rows)) {
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
            let due = if running { last.is_none_or(|t| self.time - t > 1.0) } else { last.is_none() };
            if !self.link.attached(sid) && due {
                self.tried.insert(sid, self.time);
                // A fresh parser for the snapshot the attach starts with.
                self.terms.borrow_mut().remove(&sid);
                o.set(model::Term { cols, rows, scroll: 0, seq: 0 });
                self.link.attach(sid, cols, rows);
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
        let gone: Vec<(u64, (u64, Option<u32>))> = self.watching.iter().filter(|(e, _)| !shown_runs.contains_key(e)).map(|(&e, &r)| (e, r)).collect();
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
        self.shown_by(t).and_then(|e| self.world.entity_from_id(e).try_cloned::<&model::Term>()).map_or(0, |tm| tm.scroll as usize)
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
    pub(super) fn emit_term(&mut self, t: u64, d: &mut Draw, sc: f32, upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8])) {
        let g = self.get::<TermGrid>(t);
        if g.w <= 0.0 {
            return;
        }
        let Some((terms, key)) = self.parser_of(t) else { return };
        let mut st = self.get::<TermScroll>(t);
        // A fraction of a row back: the viewport one row further back, lifted by what is left of that row.
        let want = self.scroll(t) + usize::from(st.frac > 0.0);
        let mut map = terms.borrow_mut();
        let Some(parser) = map.get_mut(&key) else { return };
        parser.set_scrollback(want);
        let shown = parser.scrollback();
        let snap = term::snapshot(parser, &term::DARK);
        parser.set_scrollback(0);
        drop(map);
        let lift = if st.frac > 0.0 && shown == want { (1.0 - st.frac) * self.grid.ch } else { 0.0 };
        st.top = g.y - lift;
        st.shift = shown as i64;
        self.put(t, st);
        self.put(t, TermInput::pack(&snap.input));
        self.put(t, TermCursor { row: snap.input.cursor.0, col: snap.input.cursor.1 });
        let te = self.world.entity_from_id(t);
        let focus = te.has(TermFocus::id());
        let marked = te.try_cloned::<&MarkedText>().map(|m| m.s.as_str().to_owned()).filter(|s| focus && !s.is_empty());
        let sel = self.get::<TermSelection>(t).get();
        let (np, ng) = (d.panels.len(), d.glyphs.len());
        let paint = term::Paint {
            origin: (g.x, st.top),
            scale: sc,
            focused: focus && self.focused && self.palette.is_none(),
            sel,
            scroll: shown,
            theme: &term::DARK,
            time: self.time,
            marked: marked.as_deref(),
        };
        term::draw(d, &mut self.grid, &mut self.fonts, &mut self.cache, &snap, &paint, upload);
        if lift > 0.0 {
            let band = [g.y, g.y + g.h];
            for p in &mut d.panels[np..] {
                p.clip[..2].copy_from_slice(&band);
            }
            for gl in &mut d.glyphs[ng..] {
                gl.clip[..2].copy_from_slice(&band);
            }
        }
        if self.grid.animating {
            // The cursor fades on the app clock: keep drawing.
            self.poke();
        }
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

    /// The terminal holding a mouse report or a drag, if any.
    fn term_with(&self, f: impl Fn(&Self, u64) -> bool) -> Option<u64> {
        self.terminals().into_iter().find(|&t| f(self, t))
    }

    /// The pointer moved to `(x, y)` (window points).
    pub fn pointer_move(&mut self, x: f32, y: f32, m: KeyMods) {
        self.set_pointer(x, y);
        if self.resizing {
            if let Some(side) = self.sidebar() {
                let left = side.try_cloned::<&Laid>().map_or(0.0, |l| l.x);
                let v = (self.pointer.x - left).clamp(SIDE_MIN, SIDE_MAX).round();
                set_side_width(side, v);
                self.side_w = Some(v);
            }
            self.poke();
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
                self.set_sel(t, Some(if span.0 < a0 { Sel { a: a1, b: span.0 } } else { Sel { a: a0, b: span.1 } }));
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
        self.poke();
        self.press = (clicks, m);
        // On the sidebar's right edge: drag it wider or narrower.
        if b == Button::Left && self.palette.is_none() && self.side_open && let Some(l) = self.sidebar().and_then(|s| s.try_cloned::<&Laid>()).filter(|l| l.w > 0.0) {
            if (self.pointer.x - (l.x + l.w)).abs() <= SIDE_GRAB && self.pointer.y >= l.y && self.pointer.y <= l.y + l.h {
                self.resizing = true;
                return;
            }
        }
        if b == Button::Left {
            self.pointer.down = true;
            self.pointer.went_down = true;
        }
        if self.palette.is_some() {
            return;
        }
        // A press in another pane focuses it (the selection follows what it shows), and takes the keyboard from the tree.
        if let Some(p) = self.pane_at() {
            self.tree_focus = false;
            self.focus_pane(p);
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
        let unit = match clicks {
            ..=1 => Unit::Cell,
            2 => Unit::Word,
            _ => Unit::Line,
        };
        let anchor = self.unit_span(t, at, unit);
        self.put(t, TermSelection { sel: Some(anchor), selecting: true, unit, anchor });
    }

    pub fn pointer_up(&mut self, x: f32, y: f32, b: Button, m: KeyMods) {
        self.set_pointer(x, y);
        if self.resizing {
            self.resizing = false;
            if let Some(v) = self.side_w {
                crate::prefs::set("sidebar_width", serde_json::json!(v));
            }
            self.poke();
            return;
        }
        if b == Button::Left {
            self.pointer.down = false;
            self.pointer.went_up = true;
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
        let mut step = |code: u16, n: i64, keys: &mut term::Keys| out.extend(keys.press(code, &modes).repeat(n.unsigned_abs() as usize));
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
        let Some(t) = self.focused_term() else { return };
        let input = self.input_of(t);
        let (cols, rows) = input.size;
        if rows == 0 {
            return;
        }
        let all = ((-(input.history as i64), 0), (i64::from(rows) - 1, cols.saturating_sub(1)));
        self.put(t, TermSelection { sel: Some(all), selecting: false, unit: Unit::Cell, anchor: all });
    }

    /// A scroll of `dy` (points when `precise`, else lines; positive: back into the history) on the terminal under the
    /// pointer (else the focused one). The primary screen scrolls its history by the pixel; a program with a mouse mode
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
                term::mouse(if n > 0.0 { Button::WheelUp } else { Button::WheelDown }, MouseAct::Press, col, row, m, &modes).repeat(count)
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

    /// A key: into the palette when it is open, else natural text editing or the program's own encoding, for the focused
    /// terminal.
    pub fn key(&mut self, k: &term::KeyIn<'_>) {
        self.poke();
        if self.palette.is_some() {
            if k.action != KeyAction::Release {
                self.palette_key(k.code, k.text);
            }
            return;
        }
        if self.archive_key(k) {
            return;
        }
        if self.tree_key(k) {
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
        if let Some(bytes) = term::natural_edit(k.code, k.unshifted, k.mods) {
            self.typed();
            self.send_to(t, bytes);
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
        self.set_marked(None);
        if let Some(p) = self.palette.as_mut() {
            p.query.push_str(text);
            p.at = 0;
            return;
        }
        if self.archive_text(text) {
            return;
        }
        self.typed();
        self.send(text.as_bytes());
    }

    /// An input method's uncommitted text on the focused terminal (None: none).
    pub fn set_marked(&mut self, text: Option<String>) {
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
        let t = self.focused_term()?;
        self.world.entity_from_id(t).try_cloned::<&MarkedText>().map(|m| m.s.as_str().to_owned()).filter(|s| !s.is_empty())
    }

    /// Where typed text goes (window points): the cell after the focused terminal's cursor and any marked text, for the
    /// input method's candidate window.
    pub fn caret_rect(&self) -> Option<[f32; 4]> {
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

    /// The text of the focused terminal's selection, for the pasteboard (None: nothing selected, so Command-C does nothing).
    pub fn copy(&self) -> Option<String> {
        let t = self.focused_term()?;
        let sel = self.get::<TermSelection>(t).get()?;
        self.with_parser(t, |p| term::selection_text(p, sel.ordered())).filter(|s| !s.is_empty())
    }

    pub fn paste(&mut self, text: &str) {
        if self.palette.is_some() {
            if let Some(p) = self.palette.as_mut() {
                p.query.push_str(text.lines().next().unwrap_or(""));
            }
            self.poke();
            return;
        }
        if self.archive_text(text) {
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
