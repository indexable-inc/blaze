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

mod canvas;
mod chart;

/// The most elements one cell draws, how deep they nest, and the most rows and columns a table shows.
const MAX_ELEMENTS: usize = 600;
const MAX_DEPTH: usize = 8;
const MAX_ROWS: usize = 200;
const MAX_COLS: usize = 12;

/// Each element the app draws and its prefab: the kernel's `ELEMENT_NAMES` (mcp/src/ui/element.ts).
const ELEMENTS: [(&str, &str); 13] = [
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
    ("Chart", "hn.GenChart"),
    ("Canvas", "hn.GenCanvas"),
];
const UNKNOWN: &str = "hn.GenUnknown";

/// What is left of the caps as a tree is built, and whether it was cut.
struct Budget<'a, 'm> {
    left: usize,
    cut: bool,
    /// How tables measure their words (see [`view::table::table`]).
    measure: &'a mut Measure<'m>,
}

/// Fill a `hn.ChatUi` row with the tree `ui`.
pub(super) fn fill(row: View<'_>, ui: &Value, measure: &mut Measure<'_>) {
    let mut b = Budget { left: MAX_ELEMENTS, cut: false, measure };
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

/// The prefab `node` is drawn from: its type's, a pie's own, or [`UNKNOWN`] when it is refused ([`refusal`]).
fn prefab_for(world: WorldRef<'_>, node: &Value) -> &'static str {
    let kind = node["type"].as_str().unwrap_or("");
    if refusal(world, node).is_some() {
        return UNKNOWN;
    }
    match (kind, node["props"]["kind"].as_str()) {
        ("Chart", Some("pie" | "donut")) => "hn.GenChartPie",
        _ => prefab_of(kind),
    }
}

/// Why `node` is not drawn as its type, named for the agent and drawn in the failed tone in its place: an element the app
/// does not know, or props it cannot draw (a Table's rows that are not a list, a Chart's data, a Canvas's shapes).
fn refusal(world: WorldRef<'_>, node: &Value) -> Option<String> {
    let kind = node["type"].as_str().unwrap_or("");
    match kind {
        "Chart" => chart::check(world, node),
        "Canvas" => canvas::check(world, node),
        "Table" => match prop(node, "rows") {
            Value::Null | Value::Array(_) => None,
            v => Some(format!(
                "<Table> rows is {}, not a list of records or lists: rows={{[{{name: 'a', n: 3}}]}}",
                match v {
                    Value::Object(_) => "an object",
                    Value::String(_) => "a string",
                    Value::Number(_) => "a number",
                    _ => "a bool",
                }
            )),
        },
        _ if ELEMENTS.iter().any(|(k, _)| *k == kind) => None,
        _ => {
            let said: String = kind.chars().take(32).collect();
            let names: Vec<&str> = ELEMENTS.iter().map(|(k, _)| *k).collect();
            Some(format!("unknown element \u{201C}{said}\u{201D}: the elements are {}", names.join(", ")))
        }
    }
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
fn children(parent: View<'_>, nodes: &[&Value], depth: usize, b: &mut Budget<'_, '_>) {
    let mut kept = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        if b.left == 0 || depth >= MAX_DEPTH {
            b.cut = true;
            break;
        }
        b.left -= 1;
        kept.push((format!("g{i}"), prefab_for(parent.entity().world(), node), *node));
    }
    view::for_each_of(parent, kept, |v, node, _| element(v, node, depth, b));
}

/// Set one element's props on its entity, and build its children.
fn element(v: View<'_>, node: &Value, depth: usize, b: &mut Budget<'_, '_>) {
    if let Some(why) = refusal(v.entity().world(), node) {
        v.text(&why);
        return;
    }
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
            super::blocks(&words(node, "text"), false, &mut rows);
            view::for_each_of(v, rows.iter().enumerate().map(|(i, r)| (format!("m{i}"), r.prefab(), r)), |row, r, _| {
                super::fill_row(row, r, &RowCtx::NOWHERE, &mut *b.measure);
            });
        }
        "Code" => {
            let code = Row::Code { lang: words(node, "lang"), text: words(node, "text") };
            super::fill_row(v, &code, &RowCtx::NOWHERE, &mut *b.measure);
        }
        "Table" => table(v, node, b),
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
        "Chart" => {
            if let Err(e) = chart::chart(v, node, b) {
                // Checked before its prefab was picked ([`refusal`]): only a cap left can end it here.
                b.cut = true;
                eprintln!("harness-native: genui: {e}");
            }
        }
        "Canvas" => {
            if let Err(e) = canvas::canvas(v, node, b) {
                b.cut = true;
                eprintln!("harness-native: genui: {e}");
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
        _ => {}
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
/// a `ui.Table` with a header, cells typed by their values, a foot saying how many rows when not all show.
fn table(v: View<'_>, node: &Value, b: &mut Budget<'_, '_>) {
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
    let data = view::table::Table {
        cols: cols.iter().map(|(title, _)| view::table::Column { name: title.clone(), align: view::table::Align::Auto }).collect(),
        rows: (0..shown).map(|r| (0..cols.len()).map(|c| super::value_cell(cell(r, c), "")).collect()).collect(),
        head: true,
    };
    if let Some(grid) = v.part("card").and_then(|c| c.part("grid")) {
        let opts = view::table::Opts { look: "hn.ChatTable", shown, action: None, open: None };
        view::table(grid, &data, opts, &mut *b.measure);
    }
    if let Some(f) = v.part("foot") {
        let total = rows.len();
        super::foot(f, &if shown < total { format!("{shown} of {total} rows") } else { String::new() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_native_api::Clock;
    use harness_native_ui_layout_api::Viewport;

    /// Property `a-table-given-rows-shows-them`: a cell's `<Table rows=...>` lays out a body row for each of its rows, each
    /// with height, under its header; never a header alone. Headless (the scene world and layout; no window or daemon).
    #[test]
    fn a_table_given_rows_shows_them() {
        let world = crate::app::scene_world().unwrap_or_else(|e| panic!("{e}"));
        let w = world.world_ref();
        harness_native_ui_layout::install(w);
        harness_native_ui_motion::install(w);
        world.set(Clock::default());
        world.set(Viewport { w: 900.0, h: 700.0 });
        let row = View::root(w, "hn.ChatUi");
        let ui = serde_json::json!({"type": "Column", "props": {}, "children": [
            {"type": "Table", "props": {
                "rows": [{"": "a", "part": "system", "tokens": 1200, "share": 0.4},
                         {"": "b", "part": "tools", "tokens": 900, "share": 0.3},
                         {"": "c", "part": "chat", "tokens": 900, "share": 0.3}],
                "columns": ["", "part", "tokens", "share"]}, "children": []}]});
        let mut measure = |s: &str, st: &TextStyle, _md: bool| s.chars().count() as f32 * st.size.max(1.0) * 0.55;
        for _ in 0..6 {
            fill(row, &ui, &mut measure);
            world.get::<&mut Clock>(|c| c.time += 1.0 / 60.0);
            world.progress_time(1.0 / 60.0);
        }
        let body = row.part("body").and_then(|b| b.part("g0")).and_then(|c| c.part("g0"));
        let table = body.and_then(|t| t.part("card")).and_then(|c| c.part("grid")).expect("the table's grid");
        let inner = table.part("scroll").and_then(|s| s.part("inner")).expect("scroll.inner");
        let rows_of = |v: View<'_>| {
            let mut n = Vec::new();
            v.entity().each_child(|c| {
                if c.has(harness_native_ui_node_api::Node::id()) {
                    n.push((c.name(), c.try_cloned::<&Laid>()));
                }
            });
            n
        };
        let shown = rows_of(inner.part("body").expect("body"));
        let laid = |v: Option<View<'_>>| v.and_then(|v| v.entity().try_cloned::<&Laid>());
        assert_eq!(
            shown.iter().filter(|(_, l)| l.is_some_and(|l| l.h > 0.0)).count(),
            3,
            "body rows {shown:?}; table {:?}, inner {:?}, card {:?}",
            laid(Some(table)),
            laid(Some(inner)),
            laid(body)
        );
    }

    /// Property `charts-and-drawings-draw`: a pie, a line, bars and a canvas lay out as their parts (each slice and line a
    /// `Vector` with path data the painter reads, each bar a share of its plot's height, a canvas's shapes over its box);
    /// a chart or canvas whose props are wrong draws one line in the failed tone naming what is wrong, never half of it.
    #[test]
    fn charts_and_drawings_draw() {
        use harness_native_ui_text_api::Vector;
        let world = crate::app::scene_world().unwrap_or_else(|e| panic!("{e}"));
        let w = world.world_ref();
        harness_native_ui_layout::install(w);
        harness_native_ui_motion::install(w);
        world.set(Clock::default());
        world.set(Viewport { w: 900.0, h: 1400.0 });
        let row = View::root(w, "hn.ChatUi");
        let ui = serde_json::json!([
            {"type": "Chart", "props": {"kind": "pie", "data": [{"label": "a", "value": 3}, {"label": "other", "value": 1}, {"label": "b", "value": 2}]}, "children": []},
            {"type": "Chart", "props": {"kind": "line", "data": [1, 4, 2, 8], "labels": ["q1", "q2", "q3", "q4"]}, "children": []},
            {"type": "Chart", "props": {"data": [2, 0, 5]}, "children": []},
            {"type": "Canvas", "props": {"view": [0, 0, 100, 50]}, "children": [
                {"type": "Group", "props": {"transform": [1, 0, 0, 1, 10, 0], "opacity": 0.5}, "children": [
                    {"type": "Rect", "props": {"x": 0, "y": 0, "width": 20, "height": 10, "radius": 3, "fill": "accent"}, "children": []}]},
                {"type": "Path", "props": {"d": "M0 0 L10 10 A5 5 0 0 1 20 20 Z", "stroke": "series2", "fill": "none"}, "children": []},
                {"type": "Text", "props": {"text": "hi", "x": 50, "y": 40, "anchor": "middle"}, "children": []}]},
            {"type": "Chart", "props": {"kind": "bar", "data": [1, -2]}, "children": []},
            {"type": "Canvas", "props": {}, "children": [{"type": "Row", "props": {}, "children": []}]}
        ]);
        let mut measure = |s: &str, st: &TextStyle, _md: bool| s.chars().count() as f32 * st.size.max(1.0) * 0.55;
        for _ in 0..6 {
            fill(row, &ui, &mut measure);
            world.get::<&mut Clock>(|c| c.time += 1.0 / 60.0);
            world.progress_time(1.0 / 60.0);
        }
        let body = row.part("body").expect("body");
        let el = |i: usize| body.part(&format!("g{i}")).unwrap_or_else(|| panic!("element {i}"));
        let laid = |v: View<'_>| v.entity().try_cloned::<&Laid>().expect("laid out");
        let vectors = |v: View<'_>| {
            let mut n = 0;
            v.entity().each_child(|c| {
                if let Some(vec) = c.try_cloned::<&Vector>() {
                    assert!(
                        vec.d.as_str().is_empty() || harness_native_render::vector::segments(vec.d.as_str()).is_ok(),
                        "{:?}",
                        vec.d.as_str()
                    );
                    n += 1;
                }
            });
            n
        };
        let pie = el(0).part("disc").expect("a pie's disc");
        assert_eq!(vectors(pie), 3, "a slice each");
        assert_eq!(el(0).part("legend").map(|l| { let mut n = 0; l.entity().each_child(|_| n += 1); n }), Some(3));
        let area = |i: usize| el(i).part("plot").and_then(|p| p.part("body")).and_then(|b| b.part("area")).expect("a plot's area");
        assert_eq!(vectors(area(1)), 2, "gridlines and the line");
        let mut bars = Vec::new();
        area(2).entity().each_child(|c| {
            if c.try_cloned::<&Vector>().is_none() {
                bars.push(c.try_cloned::<&Laid>().map_or(-1.0, |l| l.h));
            }
        });
        assert_eq!(bars.len(), 3);
        assert!(bars[2] > bars[0] && bars[0] > 0.0 && bars[1] == 0.0, "bars by value: {bars:?}");
        let canvas = el(3);
        assert_eq!(vectors(canvas), 3);
        let c = laid(canvas);
        assert!(c.w >= 100.0 && (c.w / c.h - 2.0).abs() < 0.05, "a canvas keeps its view box's shape: {c:?}");
        for i in [4, 5] {
            let t = el(i).entity().try_cloned::<&Text>().map(|t| t.s.as_str().to_owned()).unwrap_or_default();
            assert!(!t.is_empty() && el(i).entity().try_cloned::<&Laid>().is_some(), "element {i} is refused in words: {t:?}");
        }
    }
}

/// A drawn tree's words as plain text, for a copy that takes the card whole: each element's own words (its text, title,
/// label, value, note; a table's rows a line each, cells a tab apart) in reading order, one element a line.
pub(super) fn plain(ui: &Value) -> String {
    fn walk(node: &Value, out: &mut Vec<String>) {
        match node {
            Value::Array(a) => a.iter().for_each(|n| walk(n, out)),
            Value::String(s) if !s.trim().is_empty() => out.push(s.clone()),
            Value::Object(_) => {
                for name in ["title", "label", "text", "value", "note"] {
                    match prop(node, name) {
                        Value::String(s) if !s.trim().is_empty() => out.push(s.clone()),
                        Value::Number(n) => out.push(n.to_string()),
                        _ => {}
                    }
                }
                if node["type"].as_str() == Some("Table") {
                    for r in prop(node, "rows").as_array().into_iter().flatten() {
                        let cells: Vec<String> = match r {
                            Value::Object(o) => o.values().map(super::cell_text).collect(),
                            Value::Array(a) => a.iter().map(super::cell_text).collect(),
                            x => vec![super::cell_text(x)],
                        };
                        out.push(cells.join("\t"));
                    }
                }
                walk(&node["children"], out);
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(ui, &mut out);
    out.join("\n")
}
