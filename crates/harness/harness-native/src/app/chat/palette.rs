//! Palette search fields share the chat's Draft editor: one source of text, selection and IME state.
use super::*;

impl App {
    pub(in crate::app) fn palette_editing(&self) -> bool {
        self.palette.as_ref().is_some_and(|p| p.mode != Mode::New)
    }

    pub(in crate::app) fn palette_field(&self) -> Option<View<'_>> {
        let (id, mode) = self.ids.palette?;
        (mode != Mode::New).then_some(())?;
        View::of(self.world.entity_from_id(id)).part("card")?.part("query")?.part("field")
    }

    pub(in crate::app) fn palette_text(&self) -> Option<View<'_>> {
        self.palette_field()?.part("text")
    }

    /// A single-line field keeps the editor's byte-boundary and selection rules. Only text changes restart search.
    pub(in crate::app) fn palette_draft(&mut self, f: impl FnOnce(&mut Draft)) -> bool {
        let Some(p) = self.palette.as_mut().filter(|p| p.mode != Mode::New) else { return false };
        let before = p.query.text.clone(); // alloc: short search query, compared only on an input event
        f(&mut p.query);
        if p.query.text.contains(['\n', '\r', '\t']) { p.query.text = p.query.text.replace(['\n', '\r', '\t'], " "); }
        if before != p.query.text {
            p.at = 0;
            p.changed = self.time;
        }
        self.poke();
        true
    }

    pub(in crate::app) fn palette_edit(&mut self, op: &str) {
        let (select, name) = op.strip_prefix("select_").map_or((false, op), |n| (true, n));
        self.palette_draft(|d| {
            if let Some(m) = Move::named(name) {
                let to = d.target(m).unwrap_or(match m { Move::Up | Move::LineStart => 0, _ => d.text.len() });
                d.go(m, to, select);
            } else if let Some(m) = Move::erasing(op) {
                let to = d.target(m).unwrap_or(0);
                d.erase(to);
            }
        });
    }

    pub(in crate::app) fn palette_copy(&self) -> Option<Option<(String, Copied)>> {
        let p = self.palette.as_ref().filter(|p| p.mode != Mode::New)?;
        let text = p.query.selected();
        Some(self.palette_text().filter(|_| !text.is_empty()).map(|node| (text.to_owned(), Copied::Field(node.id()))))
    }

    pub(in crate::app) fn sync_palette_field(&self, card: View<'_>, placeholder: &str) {
        let Some(p) = self.palette.as_ref().filter(|p| p.mode != Mode::New) else { return };
        let Some(field) = card.part("query").and_then(|q| q.part("field")) else { return };
        let Some(text) = field.part("text") else { return };
        let (shown, [caret, anchor, lo, hi]) = p.query.shown();
        text.text(&shown);
        if let Some(hint) = field.part("hint_at").and_then(|h| h.part("hint")) {
            hint.text(if shown.is_empty() { placeholder } else { "" });
        }
        self.sync_draft_caret(text, [caret, anchor, lo, hi], self.focused);
        // A long query stays on one line. Follow the caret horizontally without a distracting visible scroller.
        if let (Some(area), Some(caret)) = (field.entity().try_cloned::<&Laid>(), words_caret(&mut self.paint.borrow_mut(), text.entity())) {
            let shown = field.entity().try_cloned::<&ScrollState>().unwrap_or_default();
            let x = if caret[0] < area.x { shown.x + caret[0] - area.x }
                else if caret[0] + caret[2] > area.x + area.w { shown.x + caret[0] + caret[2] - area.x - area.w }
                else { shown.x };
            if (x.max(0.0) - shown.x).abs() > 0.01 { scroll_quietly(field.entity(), ScrollState { x: x.max(0.0), y: 0.0 }); }
        }
    }

    fn palette_index(&self, x: f32, y: f32) -> Option<usize> {
        let text = self.palette_text()?;
        let (layout, laid) = words_layout(&mut self.paint.borrow_mut(), text.entity())?;
        let at = layout.index_at(x - laid.x, y - laid.y);
        self.palette.as_ref().map(|p| byte_of(&p.query.text, at))
    }

    pub(in crate::app) fn palette_pointer_down(&mut self, clicks: isize) {
        let (x, y) = (self.pointer.x, self.pointer.y);
        let inside = self.palette_field().and_then(|f| f.entity().try_cloned::<&Laid>()).is_some_and(|l| l.contains(x, y));
        if !inside { return; }
        let Some(at) = self.palette_index(x, y) else { return };
        let unit = Unit::of_clicks(clicks);
        let Some(p) = self.palette.as_mut() else { return };
        if p.query.marked.is_some() { return; }
        if self.mods.shift && clicks == 1 { p.query.caret = at; }
        else { let span = p.query.press(at, unit); p.query_drag = Some((unit, span)); }
        self.poke();
    }

    pub(in crate::app) fn palette_pointer_drag(&mut self) {
        let Some((unit, span)) = self.palette.as_ref().and_then(|p| p.query_drag) else { return };
        let Some(at) = self.palette_index(self.pointer.x, self.pointer.y) else { return };
        if let Some(p) = self.palette.as_mut() { p.query.drag_to(at, unit, span); }
        self.poke();
    }
}
