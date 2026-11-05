//! A CellRef: another cell of this chat, shown on a card (den's link.rs `cell_card` and `embedded`). The kernel resolves
//! the cell's `task` to its tool call (`call`); the card's head is the cell's name (its `title`, else "cell <task>") and a
//! badge while it runs or once it failed, and under it the view that cell draws, live (its events go to that cell, not
//! this one), else what it printed as code, [`CELL_LINES`] lines of it. Only this chat's cells are found (the cells are
//! its transcript's `details`, by call). A cell shown inside a cell shows only what that one made itself: a CellRef in an
//! embedded view is a note, so a cell that shows itself never recurses.
use super::*;

/// The most lines of a cell's printed output a card shows (den's CELL_LINES).
const CELL_LINES: usize = 30;

/// What a card shows under its head.
enum Body<'d> {
    /// The cell's view: its roots, and the cell their events name.
    View(Vec<&'d Value>, &'d str),
    /// Its printed output, as code.
    Printed(String),
    /// A quiet line saying why there is nothing more.
    Note(&'static str),
}

/// Draw CellRef `node` on `v` (an `hn.GenCellRef`): its `head` and `body` parts, both filled by [`children`].
pub(super) fn element(v: View<'_>, node: &Value, depth: usize, b: &mut Budget<'_, '_>) {
    let task = match prop(node, "task") {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    let title = match words(node, "title") {
        t if !t.is_empty() => t,
        _ if !task.is_empty() => format!("cell {task}"),
        _ => "cell".to_owned(),
    };
    let cells = b.cells;
    let call = prop(node, "call").as_str().filter(|c| !c.is_empty());
    let found = call.and_then(|c| cells.and_then(|all| all.get(c)));
    let body = match (call, found) {
        _ if b.nested => Body::Note("a cell shown inside a cell shows only what that one made itself"),
        (None, _) => Body::Note("this cell is not one of this session\u{2019}s"),
        (Some(_), None) => Body::Note("this cell is not in the transcript yet"),
        (Some(_), Some(d)) => body_of(d),
    };
    let badge = match found.map(|d| d.status.as_str()) {
        Some("running") => Some(("running", "accent")),
        Some("error") => Some(("failed", "error")),
        _ => None,
    };
    let mut head = vec![serde_json::json!({"type": "Text", "props": {"text": title, "tone": "muted"}, "children": []})];
    if let Some((text, tone)) = badge {
        head.push(serde_json::json!({"type": "Badge", "props": {"text": text, "tone": tone}, "children": []}));
    }
    view::for_each_of(v, [("head", "hn.GenCellRefHead"), ("body", "hn.GenCellRefBody")].map(|(k, p)| (k, p, k)), |part, k, _| {
        if k == "head" {
            children(part, &head.iter().collect::<Vec<_>>(), depth + 1, b);
            return;
        }
        match &body {
            Body::View(roots, cell) => {
                // The cell's own view, its controls sending to that cell, inside this card's caps.
                let mut inner = Budget {
                    left: b.left,
                    cut: false,
                    measure: &mut *b.measure,
                    cell: *cell,
                    cells: b.cells,
                    nested: true,
                    live: b.live.as_deref_mut(),
                    within: Within::Flow,
                    next: Within::Flow,
                    pane: b.pane,
                    now: b.now,
                    playing: b.playing,
                    // Its ids are its own cell's: an '@id' in it names what that cell drew, resolved here.
                    ids: std::collections::BTreeMap::new(),
                    shaders: Vec::new(),
                };
                children(part, roots, depth + 1, &mut inner);
                super::surface::resolve(&mut inner);
                let (left, cut) = (inner.left, inner.cut);
                b.left = left;
                b.cut |= cut;
            }
            Body::Printed(text) => {
                let code = serde_json::json!({"type": "Code", "props": {"text": text}, "children": []});
                children(part, &[&code], depth + 1, b);
            }
            Body::Note(says) => {
                let note = serde_json::json!({"type": "Text", "props": {"text": says, "tone": "muted"}, "children": []});
                children(part, &[&note], depth + 1, b);
            }
        }
    });
}

/// What cell `d` shows on a card: its view while it draws one, else what it printed (its logs, else its value's words),
/// else a note that it is still running or made nothing.
fn body_of(d: &Detail) -> Body<'_> {
    let roots: Vec<&Value> = match &*d.ui {
        Value::Array(a) => a.iter().filter(|r| r.is_object()).collect(),
        r @ Value::Object(_) => vec![r],
        _ => Vec::new(),
    };
    if !roots.is_empty() {
        return Body::View(roots, &d.cell);
    }
    let v = &d.value;
    let printed = if d.logs.trim().is_empty() {
        match v["t"].as_str().unwrap_or("") {
            "text" => v["v"].as_str().unwrap_or("").to_owned(),
            "scalar" => super::super::cell_text(&v["v"]),
            "error" => v["stack"].as_str().filter(|s| !s.is_empty()).or_else(|| v["message"].as_str()).unwrap_or("").to_owned(),
            _ => String::new(),
        }
    } else {
        d.logs.clone()
    };
    if printed.trim().is_empty() {
        return Body::Note(if d.status == "running" { "running\u{2026}" } else { "no output" });
    }
    Body::Printed(super::super::head_lines(&printed, CELL_LINES))
}
