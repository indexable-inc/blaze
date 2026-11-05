//! Generative UI in the chat: what a harness REPL cell draws when its value is Svelte 5 markup (the kernel's mcp/src/ui/,
//! copied from den). The kernel sends the tree on the cell's `tool_detail` as `ui`: `{type, props, children}` (or a list of
//! them), sent whole again each time its `$state` changes. Each element becomes an entity under the card's `hn.ChatUi` row,
//! made from the scene's `hn.Gen<type>` (scene/widgets/genui.flecs), its props set as component values, its nesting
//! ChildOf. A tree sent again is laid onto the same entities by place and type ([`view::for_each_of`]: the same key and
//! prefab keeps its entity), so hover and the springs on what stayed keep running. A type not in [`ELEMENTS`] draws its
//! name in the failed tone, never a guess. The caps (elements, depth, table rows and columns) cut a runaway tree and say so.
//! Copied from den's den-native/src/genui.rs (its `slot`/`prune` is `for_each_of` here) and cut to the v1 elements.
use super::*;
use harness_native_ui_layout_api::{Flex, Padding};

/// The most elements one cell draws, how deep they nest, and the most rows and columns a table shows.
const MAX_ELEMENTS: usize = 600;
const MAX_DEPTH: usize = 8;
const MAX_ROWS: usize = 200;
const MAX_COLS: usize = 12;

/// Each element the app draws and its prefab: the kernel's `ELEMENT_NAMES` (mcp/src/ui/element.ts).
const ELEMENTS: [(&str, &str); 11] = [
    ("Column", "hn.GenColumn"),
    ("Row", "hn.GenRow"),
    ("Text", "hn.GenText"),
    ("Markdown", "hn.GenMarkdown"),
    ("Code", "hn.GenCode"),
    ("Table", "hn.GenTable"),
    ("Badge", "hn.GenBadge"),
    ("Stat", "hn.GenStat"),
    ("Card", "hn.GenCard"),
    ("Divider", "hn.GenDivider"),
    ("Spacer", "hn.GenSpacer"),
];
const UNKNOWN: &str = "hn.GenUnknown";

/// What is left of the caps as a tree is built, and whether it was cut.
struct Budget {
    left: usize,
    cut: bool,
}

/// Fill a `hn.ChatUi` row with the tree `ui`.
pub(super) fn fill(row: View<'_>, ui: &Value) {
    let mut b = Budget { left: MAX_ELEMENTS, cut: false };
    if let Some(body) = row.part("body") {
        let nodes: Vec<&Value> = match ui {
            Value::Array(a) => a.iter().collect(),
            v => vec![v],
        };
        children(body, &nodes, 0, &mut b);
    }
    if let Some(n) = row.part("note") {
        let said = if b.cut { format!("Cut short: a cell draws at most {MAX_ELEMENTS} elements, {MAX_DEPTH} deep") } else { String::new() };
        n.text(&said);
        // Nothing to say takes no room.
        let h = if said.is_empty() { Sizing::fixed(0.0) } else { Sizing::FIT };
        let want = Size { w: Sizing::GROW, h };
        if n.entity().try_cloned::<&Size>() != Some(want) {
            n.set(want);
        }
    }
}

fn prefab_of(kind: &str) -> &'static str {
    ELEMENTS.iter().find(|(k, _)| *k == kind).map_or(UNKNOWN, |(_, p)| p)
}

fn prop<'v>(node: &'v Value, name: &str) -> &'v Value {
    &node["props"][name]
}

fn words(node: &Value, name: &str) -> String {
    match prop(node, name) {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

fn points(node: &Value, name: &str) -> Option<f32> {
    prop(node, name).as_f64().filter(|v| v.is_finite() && *v >= 0.0).map(|v| v as f32)
}

/// Keep `parent`'s children in step with `nodes`: child `i` keyed `g<i>` and made from its type's prefab.
fn children(parent: View<'_>, nodes: &[&Value], depth: usize, b: &mut Budget) {
    let mut kept = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        if b.left == 0 || depth >= MAX_DEPTH {
            b.cut = true;
            break;
        }
        b.left -= 1;
        kept.push((format!("g{i}"), prefab_of(node["type"].as_str().unwrap_or("")), *node));
    }
    view::for_each_of(parent, kept, |v, node, _| element(v, node, depth, b));
}

/// Set one element's props on its entity, and build its children.
fn element(v: View<'_>, node: &Value, depth: usize, b: &mut Budget) {
    let kind = node["type"].as_str().unwrap_or("");
    let kids: Vec<&Value> = node["children"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    match kind {
        "Column" | "Row" => {
            spacing(v, prefab_of(kind), node);
            children(v, &kids, depth + 1, b);
        }
        "Card" => {
            spacing(v, "hn.GenCard", node);
            // A title is its first child: words, bold.
            let title = words(node, "title");
            let head = serde_json::json!({"type": "Text", "props": {"text": title, "bold": true}, "children": []});
            let all: Vec<&Value> = if title.is_empty() { kids } else { std::iter::once(&head).chain(kids).collect() };
            children(v, &all, depth + 1, b);
        }
        "Text" => {
            let tone = words(node, "tone");
            let bold = prop(node, "bold").as_bool().unwrap_or(false) || tone == "strong";
            styled(v, "hn.GenText", bold);
            toned(v, &words(node, "text"), &tone);
        }
        "Markdown" => {
            let mut rows = Vec::new();
            super::blocks(&words(node, "text"), &mut rows);
            view::for_each_of(v, rows.iter().enumerate().map(|(i, r)| (format!("m{i}"), r.prefab(), r)), |row, r, _| super::fill_row(row, r));
        }
        "Code" => super::fill_row(v, &Row::Code { lang: words(node, "lang"), text: words(node, "text") }),
        "Table" => table(v, node),
        "Badge" => {
            if let Some(t) = v.part("text") {
                toned(t, &words(node, "text"), &words(node, "tone"));
            }
        }
        "Stat" => {
            for (part, name) in [("label", "label"), ("note", "note")] {
                if let Some(p) = v.part(part) {
                    p.text(&words(node, name));
                }
            }
            if let Some(p) = v.part("value") {
                toned(p, &words(node, "value"), &words(node, "tone"));
            }
        }
        "Divider" => {}
        "Spacer" => {
            let want = match points(node, "size") {
                Some(s) => Size { w: Sizing::fixed(s), h: Sizing::fixed(s) },
                None => Size { w: Sizing::GROW, h: Sizing::GROW },
            };
            if v.entity().try_cloned::<&Size>() != Some(want) {
                v.set(want);
            }
        }
        _ => {
            let said: String = kind.chars().take(32).collect();
            let names: Vec<&str> = ELEMENTS.iter().map(|(k, _)| *k).collect();
            v.text(&format!("unknown element \u{201C}{said}\u{201D}: the elements are {}", names.join(", ")));
        }
    }
}

/// A container's `gap` and `pad` (points), else its prefab's.
fn spacing(v: View<'_>, prefab: &str, node: &Value) {
    let world = v.entity().world();
    let base = view::prefab(world, prefab);
    let mut flex = base.and_then(|p| p.try_cloned::<&Flex>()).unwrap_or_default();
    if let Some(g) = points(node, "gap") {
        flex.gap = g;
    }
    if v.entity().try_cloned::<&Flex>() != Some(flex) {
        v.set(flex);
    }
    let pad = match points(node, "pad") {
        Some(p) => Padding { l: p, r: p, t: p, b: p },
        None => base.and_then(|p| p.try_cloned::<&Padding>()).unwrap_or_default(),
    };
    if v.entity().try_cloned::<&Padding>() != Some(pad) {
        v.set(pad);
    }
}

/// Words set as `prefab` sets them, bold when said.
fn styled(v: View<'_>, prefab: &str, bold: bool) {
    let base = view::prefab(v.entity().world(), prefab).and_then(|p| p.try_cloned::<&TextStyle>()).unwrap_or_default();
    let want = TextStyle { bold: i32::from(bold), ..base };
    if v.entity().try_cloned::<&TextStyle>() != Some(want) {
        v.set(want);
    }
}

/// Words `s` in the ink of `tone` (`hn.GenTone.<tone>`, the ink when none). A tone the scene has not (the kernel lets
/// none through) shows as an error in place of the words.
fn toned(v: View<'_>, s: &str, tone: &str) {
    let name = if tone.is_empty() { "ink" } else { tone };
    let world = v.entity().world();
    let (fill, s) = match view::prefab(world, &format!("hn.GenTone.{name}")).and_then(|p| p.try_cloned::<&Fill>()) {
        Some(f) => (Some(f), s.to_owned()),
        None => (
            view::prefab(world, "hn.GenTone.error").and_then(|p| p.try_cloned::<&Fill>()),
            format!("tone \u{201C}{name}\u{201D} is not a tone"),
        ),
    };
    if let Some(f) = fill
        && v.entity().try_cloned::<&Fill>() != Some(f)
    {
        v.set(f);
    }
    v.text(&s);
}

/// A `Table`: `rows` of records (columns their keys; the kernel sends their order as `columns`) or of lists, `columns` naming them (a title,
/// or `{title, key}`: the record field it shows). Drawn as the transcript's grids are (super::fill_grid), without an index:
/// a header, numbers at the end, nulls dim, a cut cell whole on hover, a foot saying how many rows.
fn table(v: View<'_>, node: &Value) {
    let rows: &[Value] = prop(node, "rows").as_array().map_or(&[], Vec::as_slice);
    // (title, record key) of each column.
    let mut cols: Vec<(String, String)> = prop(node, "columns")
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| match c {
            Value::String(s) => (s.clone(), s.clone()),
            c => {
                let title = c["title"].as_str().unwrap_or("").to_owned();
                let key = c["key"].as_str().map_or_else(|| title.clone(), str::to_owned);
                (title, key)
            }
        })
        .collect();
    if cols.is_empty() {
        for r in rows {
            match r {
                Value::Object(o) => {
                    for k in o.keys() {
                        if !cols.iter().any(|(_, key)| key == k) {
                            cols.push((k.clone(), k.clone()));
                        }
                    }
                }
                Value::Array(a) => {
                    while cols.len() < a.len() {
                        let n = cols.len().to_string();
                        cols.push((n.clone(), n));
                    }
                }
                _ => {}
            }
        }
    }
    cols.truncate(MAX_COLS);
    let cell = |r: usize, c: usize| -> &Value {
        match &rows[r] {
            Value::Object(o) => o.get(&cols[c].1).unwrap_or(&Value::Null),
            Value::Array(a) => a.get(c).unwrap_or(&Value::Null),
            v if c == 0 => v,
            _ => &Value::Null,
        }
    };
    let shown = rows.len().min(MAX_ROWS);
    let texts: Vec<Vec<String>> = (0..shown).map(|r| (0..cols.len()).map(|c| super::cell_text(cell(r, c))).collect()).collect();
    let columns: Vec<view::TableColumn<'_>> = cols
        .iter()
        .enumerate()
        .map(|(c, (title, _))| view::TableColumn {
            name: title,
            chars: texts.iter().map(|r| r[c].chars().count()).max().unwrap_or(0).max(title.chars().count()),
            end: (0..shown).all(|r| cell(r, c).is_number() || cell(r, c).is_null()) && (0..shown).any(|r| cell(r, c).is_number()),
        })
        .collect();
    if let Some(grid) = v.part("card").and_then(|c| c.part("grid")) {
        let prefab = |r: u32, c: usize| if cell(r as usize, c).is_null() { "hn.ChatCellDim" } else { "hn.ChatCell" };
        view::table(grid, &columns, shown as u32, Some("hn.ChatHead"), prefab, |e, r, c| {
            let s = &texts[r as usize][c];
            if let Some(t) = e.part("text") {
                t.text(s);
            }
            let cut = s.chars().count() > columns[c].chars.min(view::TABLE_MAX_CHARS);
            let tip = e.entity().try_cloned::<&Tooltip>();
            if cut && tip.as_ref().is_none_or(|t| t.text.as_str() != s) {
                e.set(Tooltip { text: s.as_str().into() });
            } else if !cut && tip.is_some() {
                e.entity().remove(Tooltip::id());
            }
        });
    }
    if let Some(f) = v.part("foot") {
        let total = rows.len();
        let s = if total == 1 { "" } else { "s" };
        f.text(&if shown < total { format!("{shown} of {total} rows") } else { format!("{total} row{s}") });
    }
}
