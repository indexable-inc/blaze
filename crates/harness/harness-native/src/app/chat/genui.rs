//! Generative UI in the chat: what a harness REPL cell draws when its value is Svelte 5 markup (the kernel's mcp/src/ui/,
//! copied from den). The kernel sends the tree on the cell's `tool_detail` as `ui`: `{type, props, children}` (or a list of
//! them), sent whole again each time its `$state` changes. Each element becomes an entity under the card's `hn.ChatUi` row,
//! made from the scene's `hn.Gen<type>` (scene/widgets/genui.flecs), its props set as component values, its nesting
//! ChildOf. A tree sent again is laid onto the same entities by place and type ([`view::for_each_of`]: the same key and
//! prefab keeps its entity), so hover and the springs on what stayed keep running. A type not in [`ELEMENTS`] draws its
//! name in the failed tone, never a guess. The caps (elements, depth, table rows and columns) cut a runaway tree and say so.
//! Copied from den's den-native/src/genui.rs (its `slot`/`prune` is `for_each_of` here). Tier 3 (layout, folding and
//! tabs, what floats over the transcript, status, pictures, chips and files) is in genui/layout.rs, genui/float.rs and
//! genui/media.rs, its props checked by genui/props.rs.
use super::*;
use harness_native_ui_layout_api::{Flex, Padding};
use harness_native_ui_node_api::Laid;

mod browser;
mod canvas;
mod cellref;
mod chart;
mod color;
mod dropzone;
mod files;
mod common;
mod control;
mod float;
mod layout;
mod media;
mod props;
mod surface;

pub use common::{GenFeel, GenOn, Prims};
pub use control::Live;
pub(super) use browser::shown_card;

/// The most elements one cell draws, how deep they nest, and the most rows and columns a table shows.
const MAX_ELEMENTS: usize = 600;
const MAX_DEPTH: usize = 8;
const MAX_ROWS: usize = 200;
const MAX_COLS: usize = 12;

/// Each element the app draws and its prefab: the kernel's `ELEMENT_NAMES` (mcp/src/ui/element.ts).
const ELEMENTS: [(&str, &str); 53] = [
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
    ("Button", "hn.GenButton"),
    ("Toggle", "hn.GenToggle"),
    ("Checkbox", "hn.GenCheckbox"),
    ("Slider", "hn.GenSlider"),
    ("NumberField", "hn.GenNumberField"),
    ("Input", "hn.GenInput"),
    ("TextArea", "hn.GenTextArea"),
    ("Select", "hn.GenSelect"),
    ("Choice", "hn.GenChoice"),
    ("Radio", "hn.GenRadio"),
    ("Grid", "hn.GenGrid"),
    ("Stack", "hn.GenStack"),
    ("Layer", "hn.GenLayer"),
    ("Wrap", "hn.GenWrap"),
    ("Scroll", "hn.GenScroll"),
    ("Split", "hn.GenSplit"),
    ("Tabs", "hn.GenTabs"),
    ("Disclosure", "hn.GenDisclosure"),
    ("Accordion", "hn.GenAccordion"),
    ("Popover", "hn.GenPopover"),
    ("Menu", "hn.GenMenu"),
    ("Dialog", "hn.GenDialog"),
    ("Sheet", "hn.GenSheet"),
    ("Toast", "hn.GenToast"),
    ("Progress", "hn.GenProgress"),
    ("Loading", "hn.GenLoading"),
    ("Skeleton", "hn.GenSkeleton"),
    ("Icon", "hn.GenIcon"),
    ("Image", "hn.GenImage"),
    ("Chip", "hn.GenChip"),
    ("Tile", "hn.GenTile"),
    ("FileRef", "hn.GenFileRef"),
    ("Kip", "hn.GenKip"),
    ("Shader", "hn.GenShader"),
    ("Video", "hn.GenVideo"),
    ("File", "hn.GenFile"),
    ("CellRef", "hn.GenCellRef"),
    ("ColorPicker", "hn.GenColorPicker"),
    ("DropZone", "hn.GenDropZone"),
    ("Browser", "hn.GenBrowser"),
];
/// The elements that take input (genui/control.rs).
const CONTROLS: [&str; 10] = ["Button", "Toggle", "Checkbox", "Slider", "NumberField", "Input", "TextArea", "Select", "Choice", "Radio"];
const UNKNOWN: &str = "hn.GenUnknown";

/// What is left of the caps as a tree is built, and whether it was cut; with the cell the tree is (what its controls'
/// events name) and the controls in use.
struct Budget<'a, 'm> {
    left: usize,
    cut: bool,
    /// How tables measure their words (see [`view::table::table`]).
    measure: &'a mut Measure<'m>,
    cell: &'a str,
    live: Option<&'a mut Live>,
    /// What the element being built sits in, and what its children will ([`children`] hands `next` down as `within`).
    within: Within,
    next: Within,
    /// The pane the transcript scrolls in, as laid out last frame: what a dialog's scrim covers and a toast floats over.
    pane: Laid,
    /// The clock (seconds), for what moves on it (a spinner, a skeleton's breath, a toast's time).
    now: f32,
    /// The tree is drawn as it is made (its agent is at work): a Kip new to it plays its moods, where one drawn from
    /// history stands in its last.
    playing: bool,
    /// What the cell's ids name as it is filled (a control's live value, a Video's or Shader's entity, an Image's file),
    /// and the Shaders drawn: each is resolved against the ids once the whole cell is filled ([`surface::resolve`]), so
    /// where an id sits in the tree does not matter.
    ids: std::collections::BTreeMap<String, surface::Ref>,
    shaders: Vec<u64>,
    /// This chat's cells, by tool call (its transcript's `details`), for a CellRef; None outside a transcript.
    cells: Option<&'a std::collections::HashMap<String, Detail>>,
    /// The tree is a cell's view shown in another's CellRef: a CellRef in it shows only a note.
    nested: bool,
}

/// What an element sits in, when that changes how it is drawn or what a press on it sends.
#[derive(Clone, Debug, Default, PartialEq)]
enum Within {
    #[default]
    Flow,
    /// A Stack: the one place a Layer may be.
    Stack,
    /// An Accordion, with the onToggle of the Disclosure in it that is open (opening another closes it first).
    Accordion(Option<String>),
    /// A Popover's or Menu's trigger: a press on it (on a Button in it as well) sends these events too.
    Trigger(Vec<(String, Value)>),
}

/// Fill a `hn.ChatUi` row with the tree `ui` that cell `cell` (`<repl>.<task>`) drew. `live` is the app's controls in use
/// (a field being typed in shows its own words); None draws the tree's values. `playing`: the agent is at work, so what is
/// new plays (a Kip's moods).
#[cfg(test)]
pub(super) fn fill(
    row: View<'_>,
    ui: &Value,
    cell: &str,
    live: Option<&std::cell::RefCell<Live>>,
    playing: bool,
    measure: &mut Measure<'_>,
) {
    fill_in(row, ui, cell, None, live, playing, measure);
}

/// The drawing of the kind of file at `path`, as a FileRef draws it (for a step's file chip, steps.rs).
pub(super) fn file_icon(path: &str) -> &'static str {
    media::files::drawing(media::kind_of(path))
}

/// [`fill`], with the chat's cells by tool call (`cells`: its transcript's `details`), which a CellRef shows.
pub(super) fn fill_in(
    row: View<'_>,
    ui: &Value,
    cell: &str,
    cells: Option<&std::collections::HashMap<String, Detail>>,
    live: Option<&std::cell::RefCell<Live>>,
    playing: bool,
    measure: &mut Measure<'_>,
) {
    let mut guard = live.map(std::cell::RefCell::borrow_mut);
    let world = row.entity().world();
    let now = world.try_get::<&harness_native_api::Clock>(|c| c.time).unwrap_or(0.0);
    let pane = float::pane_of(row);
    let mut b = Budget {
        left: MAX_ELEMENTS,
        cut: false,
        measure,
        cell,
        live: guard.as_deref_mut(),
        within: Within::Flow,
        next: Within::Flow,
        pane,
        now,
        playing,
        ids: std::collections::BTreeMap::new(),
        shaders: Vec::new(),
        cells,
        nested: false,
    };
    if let Some(body) = row.part("body") {
        let nodes: Vec<&Value> = match ui {
            Value::Array(a) => a.iter().collect(),
            v => vec![v],
        };
        children(body, &nodes, 0, &mut b);
    }
    surface::resolve(&mut b);
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

/// Whether tree `ui` has a Dialog or a Sheet open (within the caps a drawing keeps to): its row must stay while it does.
pub(super) fn holds_modal(ui: &Value) -> bool {
    fn walk(node: &Value, depth: usize, left: &mut usize) -> bool {
        match node {
            Value::Array(a) => a.iter().any(|n| walk(n, depth, left)),
            Value::Object(_) if *left > 0 && depth < MAX_DEPTH => {
                *left -= 1;
                let modal = matches!(node["type"].as_str(), Some("Dialog" | "Sheet")) && prop(node, "open") == &Value::Bool(true);
                modal || walk(&node["children"], depth + 1, left)
            }
            _ => false,
        }
    }
    let mut left = MAX_ELEMENTS;
    walk(ui, 0, &mut left)
}

fn prefab_of(kind: &str) -> &'static str {
    ELEMENTS.iter().find(|(k, _)| *k == kind).map_or(UNKNOWN, |(_, p)| p)
}

/// The prefab `node` is drawn from: its type's, a pie's own, or [`UNKNOWN`] when it is refused ([`refusal`]).
fn prefab_for(world: WorldRef<'_>, node: &Value, within: &Within, live: Option<&Live>) -> &'static str {
    let kind = node["type"].as_str().unwrap_or("");
    if refusal(world, node, within, live).is_some() {
        return UNKNOWN;
    }
    if let Some(p) = control::prefab(kind, node).or_else(|| props::prefab(kind, node)) {
        return p;
    }
    match (kind, node["props"]["kind"].as_str()) {
        ("Chart", Some("pie" | "donut")) => "hn.GenChartPie",
        _ => prefab_of(kind),
    }
}

/// Why `node` is not drawn as its type, named for the agent and drawn in the failed tone in its place: an element the app
/// does not know, or props it cannot draw (a Table's rows that are not a list, a Chart's data, a Canvas's shapes), or a
/// Shader the GPU could not draw (`live`'s media said so: [`surface::failed`]).
fn refusal(world: WorldRef<'_>, node: &Value, within: &Within, live: Option<&Live>) -> Option<String> {
    own_refusal(world, node, within, live).or_else(|| common::check(world, node))
}

/// The half of [`refusal`] that is the element's own: its type and its own props.
fn own_refusal(world: WorldRef<'_>, node: &Value, within: &Within, live: Option<&Live>) -> Option<String> {
    let kind = node["type"].as_str().unwrap_or("");
    match kind {
        k if props::TIER3.contains(&k) => props::check(node, within).or_else(|| surface::failed(node, live)),
        "Chart" => chart::check(world, node),
        "Canvas" => canvas::check(world, node),
        "Browser" => browser::check(node),
        k if CONTROLS.contains(&k) => control::check(node),
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
/// Keep `parent`'s children in step with `nodes`: child `i` keyed `g<i>` and made from its type's prefab. They sit in
/// what `b.next` says (taken: it holds for these children only).
fn children(parent: View<'_>, nodes: &[&Value], depth: usize, b: &mut Budget<'_, '_>) {
    let within = std::mem::take(&mut b.next);
    let mut kept = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        if b.left == 0 || depth >= MAX_DEPTH {
            b.cut = true;
            break;
        }
        b.left -= 1;
        kept.push((format!("g{i}"), prefab_for(parent.entity().world(), node, &within, b.live.as_deref()), *node));
    }
    view::for_each_of(parent, kept, |v, node, _| {
        b.within = within.clone();
        element(v, node, depth, b);
    });
}

/// Set one element's props on its entity, and build its children.
fn element(v: View<'_>, node: &Value, depth: usize, b: &mut Budget<'_, '_>) {
    let within = std::mem::take(&mut b.within);
    if let Some(why) = refusal(v.entity().world(), node, &within, b.live.as_deref()) {
        v.text(&why);
        return;
    }
    let kind = node["type"].as_str().unwrap_or("");
    let kids: Vec<&Value> = node["children"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    match kind {
        "Grid" | "Stack" | "Layer" | "Wrap" | "Scroll" | "Split" | "Tabs" | "Disclosure" | "Accordion" => {
            layout::element(v, node, kind, &kids, depth, &within, b);
        }
        "Popover" | "Menu" | "Dialog" | "Sheet" | "Toast" => float::element(v, node, kind, &kids, depth, b),
        "Progress" | "Loading" | "Skeleton" | "Icon" | "Image" | "Chip" | "Tile" | "FileRef" | "Kip" => {
            media::element(v, node, kind, &kids, depth, b);
            surface::record(v, node, kind, b);
        }
        "Shader" | "Video" => surface::element(v, node, kind, b),
        "File" => files::element(v, node, b),
        "CellRef" => cellref::element(v, node, depth, b),
        "ColorPicker" => color::element(v, node, b),
        "DropZone" => dropzone::element(v, node, &kids, depth, b),
        "Browser" => browser::element(v, node),
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
            let tone = words(node, "tone");
            if let Some(t) = v.part("text") {
                toned(t, &words(node, "text"), &tone);
            }
            // Its plate in its tone, faint (den's toned badges); ink, muted and strong keep the plain one.
            let world = v.entity().world();
            let plate = match tone.as_str() {
                "accent" | "success" | "warning" | "error" => media::fill_of(world, &format!("hn.GenBadgeTone.{tone}")),
                _ => media::fill_of(world, "hn.GenBadge"),
            };
            media::filled(v, plate);
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
        k if CONTROLS.contains(&k) => {
            control::control(v, node, k, &within, b);
            surface::record(v, node, k, b);
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
    common::apply(v, node, kind, b);
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
    // A cell `{text, open}`: its words, and a press that sends `open` to onOpen.
    let on_open = control::handler(node, "onOpen");
    let made = |x: &Value| match x {
        Value::Object(o) if o.contains_key("text") => {
            let c = super::value_cell(&o["text"], "");
            match (&on_open, o.get("open")) {
                (Some(h), Some(open)) => c.with_press(control::send(b.cell, h, open.clone())),
                _ => c,
            }
        }
        x => super::value_cell(x, ""),
    };
    let built: Vec<Vec<view::table::Cell>> = (0..rows.len()).map(|r| (0..cols.len()).map(|c| made(cell(r, c))).collect()).collect();
    // `query` keeps the rows with a cell that has it (as the cell shows), each match marked; rows keep their own index.
    let query = words(node, "query");
    let query = query.trim();
    let kept: Vec<usize> =
        (0..rows.len()).filter(|&r| query.is_empty() || built[r].iter().any(|c| view::table::find(&c.shown(), query).is_some())).collect();
    let total = kept.len();
    let shown = total.min(MAX_ROWS);
    // onSelect is sent the row's index in `rows`.
    let on_select = control::handler(node, "onSelect");
    let presses: Option<Vec<String>> = on_select.as_deref().map(|h| kept.iter().take(shown).map(|&r| control::send(b.cell, h, Value::from(r))).collect());
    let mut built = built;
    let data = view::table::Table {
        cols: cols.iter().map(|(title, _)| view::table::Column { name: title.clone(), align: view::table::Align::Auto }).collect(),
        rows: kept.iter().take(shown).map(|&r| std::mem::take(&mut built[r])).collect(),
        head: true,
    };
    if let Some(grid) = v.part("card").and_then(|c| c.part("grid")) {
        let opts = view::table::Opts { look: "hn.ChatTable", shown, presses: presses.as_deref(), find: query, ..view::table::Opts::default() };
        view::table(grid, &data, opts, &mut *b.measure);
    }
    if let Some(f) = v.part("foot") {
        let foot = if total == 0 && !query.is_empty() {
            "No rows match".to_owned()
        } else if shown < rows.len() {
            format!("{shown} of {} rows", rows.len())
        } else {
            String::new()
        };
        super::foot(f, &foot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_native_api::Clock;
    use harness_native_ui_layout_api::Viewport;

    /// Property `new-elements-draw-and-send`: a File row sends its value, else its name; a ColorPicker's swatches send
    /// their #rrggbb, its well asks for the panel, its ring is on the selected swatch and moves at once to a pick (before
    /// the tree echoes it); a DropZone lights only while files are over it; a CellRef shows its cell's live view (that
    /// cell's events, not this one's) or its printed output cut to 30 lines, and a cell shown inside a cell is a note.
    #[test]
    fn new_elements_draw_and_send() {
        use harness_native_ui_input_api::Action;
        let world = crate::app::scene_world().unwrap_or_else(|e| panic!("{e}"));
        let w = world.world_ref();
        harness_native_ui_layout::install(w);
        harness_native_ui_input::install(w);
        harness_native_ui_motion::install(w);
        world.set(Clock::default());
        world.set(Viewport { w: 900.0, h: 2000.0 });
        let ui = serde_json::json!([
            {"type": "File", "props": {"name": "src/", "modified": "today", "onClick": "h1"}, "children": []},
            {"type": "File", "props": {"name": "package.json", "size": 812, "modified": "2 hours ago", "value": "pkg", "onClick": "h2"}, "children": []},
            {"type": "File", "props": {"name": "README", "kind": "markdown", "size": 14336}, "children": []},
            {"type": "ColorPicker", "props": {"options": ["#0A84FF", "#bf5af2", "accent"], "selected": "#0a84ff", "label": "Accent", "onChange": "h3"}, "children": []},
            {"type": "DropZone", "props": {"label": "Drop here", "onDrop": "h4"}, "children": []},
            {"type": "CellRef", "props": {"task": "t1", "call": "c1", "title": "First"}, "children": []},
            {"type": "CellRef", "props": {"task": "t2", "call": "c2"}, "children": []}
        ]);
        let printed: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
        let mut cells = std::collections::HashMap::new();
        cells.insert("c1".to_owned(), Detail {
            cell: "other.1".to_owned(),
            status: "running".to_owned(),
            ui: std::sync::Arc::new(serde_json::json!({"type": "Column", "props": {}, "children": [
                {"type": "File", "props": {"name": "inner.txt", "onClick": "h9"}, "children": []},
                {"type": "CellRef", "props": {"task": "t1", "call": "c1"}, "children": []}]})),
            ..Default::default()
        });
        cells.insert("c2".to_owned(), Detail { cell: "other.2".to_owned(), status: "error".to_owned(), logs: printed.join("\n"), ..Default::default() });
        let live = std::cell::RefCell::new(Live::default());
        let row = View::root(w, "hn.ChatUi");
        let mut measure = |s: &str, st: &TextStyle, _md: bool| s.chars().count() as f32 * st.size.max(1.0) * 0.55;
        let mut frame = || {
            fill_in(row, &ui, "test.0", Some(&cells), Some(&live), true, &mut measure);
            harness_native_ui_layout::typeset::typeset(w, &mut harness_native_ui_layout::typeset::Estimate);
            world.get::<&mut Clock>(|c| c.time += 1.0 / 60.0);
            world.progress_time(1.0 / 60.0);
        };
        for _ in 0..4 {
            frame();
        }
        let body = row.part("body").expect("body");
        let el = |i: usize| body.part(&format!("g{i}")).unwrap_or_else(|| panic!("element {i}"));
        fn part<'w>(v: View<'w>, path: &str) -> View<'w> {
            path.split('.').try_fold(v, |v, p| v.part(p)).unwrap_or_else(|| panic!("no part {path}"))
        }
        let is = |v: View<'_>, prefab: &str| {
            let p = view::prefab(w, prefab).unwrap_or_else(|| panic!("no prefab {prefab}"));
            v.entity().owns((flecs::IsA::ID, *p.id()))
        };
        let action = |v: View<'_>| v.entity().try_cloned::<&Action>().map(|a| a.name.as_str().to_owned());
        let words_of = |v: View<'_>| v.entity().try_cloned::<&Text>().map(|t| t.s.as_str().to_owned()).unwrap_or_default();
        // Every action and every word under `v`, depth first.
        fn under(e: EntityView<'_>, acts: &mut Vec<String>, words: &mut Vec<String>) {
            if let Some(a) = e.try_cloned::<&harness_native_ui_input_api::Action>() {
                acts.push(a.name.as_str().to_owned());
            }
            if let Some(t) = e.try_cloned::<&Text>() {
                words.push(t.s.as_str().to_owned());
            }
            e.each_child(|c| under(c, acts, words));
        }
        let all = |v: View<'_>| {
            let (mut a, mut t) = (Vec::new(), Vec::new());
            under(v.entity(), &mut a, &mut t);
            (a, t)
        };

        // File: a folder's name loses its slash and sends the name; a value is sent over the name; no onClick, no press.
        assert!(is(el(0), "hn.GenFileOn") && is(el(1), "hn.GenFileOn") && is(el(2), "hn.GenFile") && !is(el(2), "hn.GenFileOn"));
        assert_eq!(action(el(0)).as_deref(), Some(r#"ui:["test.0","h1","src/"]"#));
        assert_eq!(words_of(part(el(0), "name")), "src");
        assert_eq!(action(el(1)).as_deref(), Some(r#"ui:["test.0","h2","pkg"]"#));
        assert_eq!(words_of(part(el(1), "meta")), "812 B \u{00B7} 2 hours ago");
        assert_eq!(words_of(part(el(2), "meta")), "14.3 KB");
        assert_eq!(action(el(2)), None, "a File with no onClick takes no press");

        // ColorPicker: each swatch sends its #rrggbb; the well asks for the panel; the ring is on the selected one.
        let picker = el(3);
        let sw = |i: &str| part(picker, &format!("body.{i}"));
        for (i, hex) in [("s0", "#0a84ff"), ("s1", "#bf5af2")] {
            let a = action(sw(i)).unwrap_or_default();
            assert!(a.starts_with("ui:{") && a.contains(r#""k":"color""#) && a.contains(r#""change":"h3""#) && a.contains(&format!(r#""value":"{hex}""#)), "{i} sends {hex}: {a}");
            assert!(!a.contains("well"), "a swatch is no well: {a}");
        }
        let accent = action(sw("s2")).unwrap_or_default();
        assert!(accent.contains(r##""value":"#"##), "a tone sends a #rrggbb too: {accent}");
        let well = action(sw("well")).unwrap_or_default();
        assert!(well.contains(r#""well":true"#) && well.contains(r#""k":"color""#), "the well asks for the panel: {well}");
        let ringed = || ["s0", "s1", "s2", "well"].into_iter().filter(|i| sw(i).part("ring").is_some()).collect::<Vec<_>>();
        assert_eq!(ringed(), ["s0"], "the selected swatch is ringed");
        // A pick (what App::genui_color does with the s1 press) moves the ring at once, while the tree still says s0.
        live.borrow_mut().colors.keep(picker.id(), "#bf5af2");
        frame();
        assert_eq!(ringed(), ["s1"], "a pick shows before the tree echoes it");

        // DropZone: lit only while files are over it.
        let zone = el(4);
        assert!(is(zone, "hn.GenDropZone") && zone.part("hot").is_none(), "a zone at rest is not lit");
        assert_eq!(words_of(part(zone, "label")), "Drop here");
        live.borrow_mut().drops.over = Some(zone.id());
        frame();
        assert!(zone.part("hot").is_some(), "a zone with files over it lights");
        live.borrow_mut().drops.over = None;

        // CellRef: the live view sends to its own cell; a cell inside a cell is a note; printed output is cut to 30 lines.
        let (acts, words) = all(el(5));
        assert!(words.iter().any(|t| t == "First") && words.iter().any(|t| t == "running"), "head and badge: {words:?}");
        assert!(acts.iter().any(|a| a == r#"ui:["other.1","h9","inner.txt"]"#), "the embedded view sends to its cell: {acts:?}");
        assert!(words.iter().any(|t| t.contains("shows only what that one made itself")), "the nested CellRef is a note: {words:?}");
        let (_, words) = all(el(6));
        assert!(words.iter().any(|t| t == "cell t2") && words.iter().any(|t| t == "failed"), "{words:?}");
        let code = words.iter().find(|t| t.contains("line 0")).unwrap_or_else(|| panic!("printed output: {words:?}"));
        assert!(code.contains("line 29") && !code.contains("line 30\n") && code.ends_with("10 more lines"), "30 lines, then the cut: {code}");
    }

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
            fill(row, &ui, "test.0", None, false, &mut measure);
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
            fill(row, &ui, "test.0", None, false, &mut measure);
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

    /// Property `controls-draw-and-press-sends`: a form of every control draws each as its prefab with its parts laid out,
    /// arms each with the event it sends (`ui:` and `[cell, handler, value]`: a Toggle the negation of its value, a stepper
    /// its value one step on, an option its value), leaves one with no handler inert, refuses a malformed one in words,
    /// and a real press and release on a Button through the input pass activates it with `["test.0","h1",null]`.
    #[test]
    fn controls_draw_and_press_sends() {
        use harness_native_ui_input_api::{Action, Activated, PointerState};
        use harness_native_ui_motion_api::Pose;
        let world = crate::app::scene_world().unwrap_or_else(|e| panic!("{e}"));
        let w = world.world_ref();
        harness_native_ui_layout::install(w);
        harness_native_ui_input::install(w);
        harness_native_ui_motion::install(w);
        world.set(Clock::default());
        world.set(Viewport { w: 900.0, h: 1400.0 });
        let row = View::root(w, "hn.ChatUi");
        let sizes = serde_json::json!([{"label": "Small", "value": "s"}, {"label": "Medium", "value": "m"}, {"label": "Large", "value": "l"}]);
        let ui = serde_json::json!([
            {"type": "Button", "props": {"label": "Deploy", "onClick": "h1", "tone": "accent"}, "children": []},
            {"type": "Toggle", "props": {"label": "Dry run", "value": false, "onChange": "h2"}, "children": []},
            {"type": "Checkbox", "props": {"label": "Notify", "checked": true, "onChange": "h3"}, "children": []},
            {"type": "Slider", "props": {"label": "Traffic", "min": 0, "max": 100, "step": 5, "value": 40, "onChange": "h4"}, "children": []},
            {"type": "NumberField", "props": {"label": "Replicas", "min": 1, "max": 9, "value": 3, "onChange": "h5"}, "children": []},
            {"type": "Input", "props": {"label": "Name", "value": "api", "onChange": "h6", "onSubmit": "h7"}, "children": []},
            {"type": "TextArea", "props": {"label": "Notes", "placeholder": "Why", "onChange": "h8"}, "children": []},
            {"type": "Select", "props": {"label": "Size", "options": sizes, "selected": "m", "onChange": "h9"}, "children": []},
            {"type": "Choice", "props": {"options": sizes, "selected": "m", "onChange": "h10"}, "children": []},
            {"type": "Radio", "props": {"options": [1, 2], "selected": 2.0, "onChange": "h11"}, "children": []},
            {"type": "Button", "props": {"label": "Off", "onClick": "h12", "disabled": true}, "children": []},
            {"type": "Toggle", "props": {"label": "Bad", "value": "yes", "onChange": "h13"}, "children": []}
        ]);
        let mut measure = |s: &str, st: &TextStyle, _md: bool| s.chars().count() as f32 * st.size.max(1.0) * 0.55;
        let mut frame = |pointer: Option<PointerState>| {
            if let Some(p) = pointer {
                world.set(p);
            }
            fill(row, &ui, "test.0", None, false, &mut measure);
            world.get::<&mut Clock>(|c| c.time += 1.0 / 60.0);
            world.progress_time(1.0 / 60.0);
        };
        for _ in 0..6 {
            frame(None);
        }
        let body = row.part("body").expect("body");
        let el = |i: usize| body.part(&format!("g{i}")).unwrap_or_else(|| panic!("element {i}"));
        let is = |v: View<'_>, prefab: &str| {
            let p = view::prefab(w, prefab).unwrap_or_else(|| panic!("no prefab {prefab}"));
            v.entity().owns((flecs::IsA::ID, *p.id()))
        };
        let laid = |v: View<'_>, what: &str| {
            let l = v.entity().try_cloned::<&Laid>().unwrap_or_else(|| panic!("{what} is not laid out"));
            assert!(l.w > 0.0 && l.h > 0.0, "{what} has no room: {l:?}");
            l
        };
        let action = |v: View<'_>| v.entity().try_cloned::<&Action>().map(|a| a.name.as_str().to_owned());
        fn part<'w>(v: View<'w>, path: &str) -> View<'w> {
            path.split('.').try_fold(v, |v, p| v.part(p)).unwrap_or_else(|| panic!("no part {path}"))
        }
        let sends = |v: View<'_>, handler: &str, value: &str| {
            assert_eq!(action(v), Some(format!(r#"ui:["test.0","{handler}",{value}]"#)), "{handler}");
        };
        for (i, prefab) in [
            (0, "hn.GenButtonAccent"),
            (1, "hn.GenToggle"),
            (2, "hn.GenCheckboxOn"),
            (3, "hn.GenSlider"),
            (4, "hn.GenNumberField"),
            (5, "hn.GenInput"),
            (6, "hn.GenTextArea"),
            (7, "hn.GenSelect"),
            (8, "hn.GenChoice"),
            (9, "hn.GenRadio"),
            (10, "hn.GenButton"),
        ] {
            assert!(is(el(i), prefab), "element {i} is a {prefab}");
            laid(el(i), prefab);
        }
        sends(el(0), "h1", "null");
        sends(el(1), "h2", "true");
        sends(el(2), "h3", "false");
        let rail = part(el(3), "body.rail");
        assert!(action(rail).is_some_and(|a| a.starts_with("ui:{")), "the slider's rail is armed: {:?}", action(rail));
        laid(part(el(3), "body.rail.knob"), "the knob");
        assert_eq!(part(el(3), "body.value").entity().try_cloned::<&Text>().map(|t| t.s.as_str().to_owned()).as_deref(), Some("40"));
        sends(part(el(4), "body.minus"), "h5", "2");
        sends(part(el(4), "body.plus"), "h5", "4");
        for (i, what) in [(5, "the input's box"), (6, "the text area's box")] {
            let bx = part(el(i), "body");
            laid(bx, what);
            assert!(action(bx).is_some_and(|a| a.starts_with("ui:{")), "{what} is armed");
        }
        let button = part(el(7), "body");
        assert!(action(button).is_some_and(|a| a.starts_with("ui:{")), "the select is armed");
        assert_eq!(part(button, "text").entity().try_cloned::<&Text>().map(|t| t.s.as_str().to_owned()).as_deref(), Some("Medium"));
        for (n, (value, on)) in [("\"s\"", false), ("\"m\"", true), ("\"l\"", false)].into_iter().enumerate() {
            let o = part(el(8), &format!("o{n}"));
            assert!(is(o, if on { "hn.GenChoiceItemOn" } else { "hn.GenChoiceItem" }), "choice o{n} selected: {on}");
            sends(o, "h10", value);
        }
        assert!(is(part(el(9), "o1"), "hn.GenRadioItemOn"), "2.0 selects the option 2");
        sends(part(el(9), "o0"), "h11", "1");
        assert_eq!(action(el(10)), None, "a disabled button takes no press");
        assert!(el(10).entity().try_cloned::<&Pose>().is_some_and(|p| p.fade > 0.0), "and is faded");
        let t = el(11).entity().try_cloned::<&Text>().map(|t| t.s.as_str().to_owned()).unwrap_or_default();
        assert!(t.contains("<Toggle>") && t.contains("value"), "a malformed toggle is refused naming it and its prop: {t:?}");

        // A press and release on the Button, through the input pass.
        let b = laid(el(0), "the button");
        let (x, y) = (b.x + b.w / 2.0, b.y + b.h / 2.0);
        frame(Some(PointerState { x, y, down: false, went_down: false, went_up: false }));
        frame(Some(PointerState { x, y, down: true, went_down: true, went_up: false }));
        frame(Some(PointerState { x, y, down: false, went_down: false, went_up: true }));
        let mut activated = Vec::new();
        world.new_query::<&Activated>().each_entity(|e, _| activated.push(e.try_cloned::<&Action>().map(|a| a.name.as_str().to_owned())));
        assert_eq!(activated, [Some(r#"ui:["test.0","h1",null]"#.to_owned())], "the press activates the button and only it");
    }

    /// Property `choice-wraps-never-cuts`: a Choice whose options fit stays one row; one whose options do not wraps them
    /// onto more rows inside the room it has, each short option at its own full width (never cut for a long neighbour's
    /// sake), and an option wider than the whole row wraps its own words onto more lines within the row.
    #[test]
    fn choice_wraps_never_cuts() {
        use harness_native_ui_layout_api::Intrinsic;
        use harness_native_ui_text_api::{OneLine, Truncate};
        let world = crate::app::scene_world().unwrap_or_else(|e| panic!("{e}"));
        let w = world.world_ref();
        harness_native_ui_layout::install(w);
        harness_native_ui_input::install(w);
        harness_native_ui_motion::install(w);
        world.set(Clock::default());
        world.set(Viewport { w: 360.0, h: 1400.0 });
        let long = "Mix: comet while it works, ember while it rests, and the wordmark on the splash screen until the first frame lands";
        let ui = serde_json::json!([
            {"type": "Choice", "props": {"options": ["A", "B", "C"], "selected": "B", "onChange": "h1"}, "children": []},
            {"type": "Choice", "props": {"options": ["Comet", "Pixel art", "Ember", long], "selected": "Comet", "onChange": "h2"}, "children": []}
        ]);
        let row = View::root(w, "hn.ChatUi");
        let mut measure = |s: &str, st: &TextStyle, _md: bool| s.chars().count() as f32 * st.size.max(1.0) * 0.55;
        for _ in 0..8 {
            fill(row, &ui, "test.0", None, false, &mut measure);
            harness_native_ui_layout::typeset::typeset(w, &mut harness_native_ui_layout::typeset::Estimate);
            world.get::<&mut Clock>(|c| c.time += 1.0 / 60.0);
            world.progress_time(1.0 / 60.0);
        }
        let body = row.part("body").expect("body");
        let el = |i: usize| body.part(&format!("g{i}")).unwrap_or_else(|| panic!("element {i}"));
        let laid = |v: View<'_>, what: &str| {
            let l = v.entity().try_cloned::<&Laid>().unwrap_or_else(|| panic!("{what} is not laid out"));
            assert!(l.w > 0.0 && l.h > 0.0, "{what} has no room: {l:?}");
            l
        };
        fn option(c: View<'_>, n: usize) -> View<'_> {
            c.part(&format!("o{n}")).unwrap_or_else(|| panic!("option o{n}"))
        }
        fn label(o: View<'_>) -> View<'_> {
            o.part("label").expect("an option's label")
        }
        let natural = |o: View<'_>| label(o).entity().try_cloned::<&Intrinsic>().expect("a measured label");
        for c in [el(0), el(1)] {
            for n in 0..3 {
                let l = label(option(c, n));
                assert!(!l.entity().has(OneLine::id()) && !l.entity().has(Truncate::id()), "an option's words are never cut to one line");
            }
        }

        // Three short options fit: one row, side by side, each at its own width.
        let few = laid(el(0), "the short choice");
        let tops: Vec<f32> = (0..3).map(|n| laid(option(el(0), n), "a short option").y).collect();
        assert!(tops.iter().all(|y| (y - tops[0]).abs() < 0.5), "options that fit share one row: {tops:?}");
        assert!(few.h < 40.0, "a choice that fits is one control tall: {few:?}");

        // Four that do not fit: they wrap, none past the plate, the short ones whole and the long one wrapped in the row.
        let plate = laid(el(1), "the wrapping choice");
        assert!(plate.w <= 360.0 + 0.5, "the choice keeps to the room it has: {plate:?}");
        let opts: Vec<Laid> = (0..4).map(|n| laid(option(el(1), n), "an option")).collect();
        for (n, o) in opts.iter().enumerate() {
            assert!(o.x >= plate.x - 0.5 && o.x + o.w <= plate.x + plate.w + 0.5, "option o{n} stays inside the plate: {o:?} in {plate:?}");
            assert!(o.y >= plate.y - 0.5 && o.y + o.h <= plate.y + plate.h + 0.5, "the plate is as tall as its rows: o{n} {o:?} in {plate:?}");
        }
        for n in 0..3 {
            let (o, words) = (option(el(1), n), natural(option(el(1), n)));
            let l = laid(label(o), "a short option's words");
            assert!(l.w + 0.5 >= words.w, "short option o{n} keeps its full width: {l:?} for {words:?}");
        }
        let (big, words) = (opts[3], natural(option(el(1), 3)));
        let big_words = laid(label(option(el(1), 3)), "the long option's words");
        assert!(words.w > plate.w, "the long option is wider than the row: {words:?} vs {plate:?}");
        assert!(big.y > opts[0].y + 0.5, "the long option wraps onto a row of its own: {big:?} after {:?}", opts[0]);
        assert!(big_words.w < words.w && big_words.h > 16.0 * 1.5, "the long option wraps its words: {big_words:?} for {words:?}");
        assert!(big.h > opts[0].h, "and is as tall as its lines: {big:?} vs {:?}", opts[0]);
    }

    /// Property `tier-three-draws-and-sends`: every tier-3 element draws from a tree (laid out, with room), each press
    /// is armed with the events it sends (a tab its value, a disclosure's head `!open`, an accordion's closed head the open
    /// one's close then its own open, a popover's trigger and its button `!open`, a menu row its pick then the close, a
    /// dialog's scrim onClose(false) and a sheet's that may not be dismissed nothing, a chip its value or `!selected`, a
    /// tile its value, a file chip `open:` its path and line, a split's rule its drag, with an onChange or without), what
    /// floats is placed over the pane (a popover's plate under its trigger, a dialog's plate on its scrim) and noted to
    /// close, misplaced or wrong props are refused in words, a toast's time runs out into onDismiss(null), and real
    /// presses through the input pass activate a dialog's scrim, a tab and an accordion's head with those events.
    #[test]
    fn tier_three_draws_and_sends() {
        use harness_native_ui_input_api::{Action, Activated, PointerState};
        use harness_native_ui_motion_api::Pose;
        use harness_native_ui_text_api::Icon;
        let world = crate::app::scene_world().unwrap_or_else(|e| panic!("{e}"));
        let w = world.world_ref();
        harness_native_ui_layout::install(w);
        harness_native_ui_input::install(w);
        harness_native_ui_motion::install(w);
        world.set(Clock::default());
        world.set(Viewport { w: 900.0, h: 2600.0 });
        let icon = format!("{}/../../../docs/brand/blaze-icon-dark-180.png", env!("CARGO_MANIFEST_DIR"));
        let t = |s: &str| serde_json::json!({"type": "Text", "props": {"text": s}, "children": []});
        let lines: Vec<Value> = (0..12).map(|i| t(&format!("line {i}"))).collect();
        let tree = |dialog: bool, sheet: bool, floats: bool| {
            serde_json::json!([
                {"type": "Grid", "props": {"columns": 2, "gap": 12, "pad": 0}, "children": [t("a"), t("b"), t("c")]},
                {"type": "Stack", "props": {}, "children": [
                    {"type": "Image", "props": {"src": icon, "width": 96, "height": 96, "fit": "cover"}, "children": []},
                    {"type": "Layer", "props": {"anchor": "top-right", "x": -4, "y": 4, "z": 1}, "children": [
                        {"type": "Badge", "props": {"text": "new"}, "children": []}]}]},
                {"type": "Wrap", "props": {"gap": 8}, "children": [
                    {"type": "Chip", "props": {"label": "Files", "selected": false, "onClick": "h2"}, "children": []},
                    {"type": "Chip", "props": {"label": "Tests", "selected": true, "value": "tests", "onClick": "h3"}, "children": []}]},
                {"type": "Scroll", "props": {"height": 100, "horizontal": false}, "children": lines},
                {"type": "Split", "props": {"ratio": 0.4, "vertical": false, "onChange": "h6"}, "children": [t("left"), t("right")]},
                {"type": "Tabs", "props": {"tabs": ["a", {"label": "Bee", "value": "b"}], "selected": "b", "onChange": "h1"}, "children": [t("body")]},
                {"type": "Accordion", "props": {}, "children": [
                    {"type": "Disclosure", "props": {"title": "Details", "open": true, "onToggle": "h7"}, "children": [t("details")]},
                    {"type": "Disclosure", "props": {"title": "History", "open": false, "onToggle": "h8"}, "children": [t("history")]}]},
                {"type": "Popover", "props": {"placement": "bottom", "open": floats, "onToggle": "h9"}, "children": [
                    {"type": "Button", "props": {"label": "Info"}, "children": []}, t("inside")]},
                {"type": "Menu", "props": {"items": ["Rename", "-", {"label": "Delete", "value": "rm", "danger": true, "keys": "\u{2318}D"},
                    {"label": "Off", "value": "off", "disabled": true}], "open": floats, "onSelect": "h11", "onToggle": "h10"}, "children": [
                    {"type": "Button", "props": {"label": "Actions"}, "children": []}]},
                {"type": "Dialog", "props": {"title": "Delete the build?", "open": dialog, "dismissable": true, "onClose": "h15"}, "children": [t("sure?")]},
                {"type": "Sheet", "props": {"title": "Settings", "open": sheet, "dismissable": false, "onClose": "h17"}, "children": [t("settings")]},
                {"type": "Toast", "props": {"text": "Saved", "tone": "success", "duration": 0, "onDismiss": "h18"}, "children": []},
                {"type": "Progress", "props": {"value": 3, "max": 5, "label": "Build"}, "children": []},
                {"type": "Loading", "props": {"label": "Building"}, "children": []},
                {"type": "Skeleton", "props": {"lines": 2}, "children": []},
                {"type": "Icon", "props": {"name": "rust"}, "children": []},
                {"type": "Tile", "props": {"gap": 8, "pad": 12, "value": "icon", "onClick": "h5"}, "children": [t("tile")]},
                {"type": "FileRef", "props": {"path": "/x/src/tree.ts", "line": 60}, "children": []},
                {"type": "Layer", "props": {"anchor": "top-left", "x": 0, "y": 0, "z": 0}, "children": []},
                {"type": "Icon", "props": {"name": "no-such-icon-xyz"}, "children": []},
                {"type": "Grid", "props": {"columns": 2, "gap": 12, "pad": 0, "foo": 1}, "children": []},
                {"type": "Split", "props": {"ratio": 0.5, "vertical": true}, "children": [t("up"), t("down")]},
                {"type": "Grid", "props": {"fit": 120, "gap": 12, "pad": 0}, "children": [t("x"), t("y")]},
                {"type": "Scroll", "props": {"height": 80, "horizontal": true}, "children": [t("across")]},
                {"type": "Toast", "props": {"text": "Going", "tone": "ink", "duration": 500, "onDismiss": "h19"}, "children": []},
                {"type": "Button", "props": {"label": "Go", "value": {"n": 2}, "onClick": "h20"}, "children": []},
                {"type": "Table", "props": {"rows": [{"name": "alpha", "n": 1}, {"name": "beta", "n": 2}, {"name": {"text": "Gamma", "open": "g"}, "n": 3}],
                    "query": "ga", "onSelect": "h21", "onOpen": "h22"}, "children": []},
                {"type": "Table", "props": {"rows": [{"name": "alpha"}], "query": "zzz"}, "children": []},
                {"type": "FileRef", "props": {"path": "/x/a.rs", "line": 3, "name": "entry", "onClick": "h23"}, "children": []}
            ])
        };
        let live = std::cell::RefCell::new(Live::default());
        let row = View::root(w, "hn.ChatUi");
        let mut measure = |s: &str, st: &TextStyle, _md: bool| s.chars().count() as f32 * st.size.max(1.0) * 0.55;
        let mut frame = |ui: &Value, pointer: Option<PointerState>| {
            if let Some(p) = pointer {
                world.set(p);
            }
            fill(row, ui, "test.0", Some(&live), true, &mut measure);
            harness_native_ui_layout::typeset::typeset(w, &mut harness_native_ui_layout::typeset::Estimate);
            world.get::<&mut Clock>(|c| c.time += 1.0 / 60.0);
            world.progress_time(1.0 / 60.0);
        };
        let ui = tree(true, true, true);
        for _ in 0..6 {
            frame(&ui, None);
        }
        let body = row.part("body").expect("body");
        let el = |i: usize| body.part(&format!("g{i}")).unwrap_or_else(|| panic!("element {i}"));
        fn part<'w>(v: View<'w>, path: &str) -> View<'w> {
            path.split('.').try_fold(v, |v, p| v.part(p)).unwrap_or_else(|| panic!("no part {path}"))
        }
        let is = |v: View<'_>, prefab: &str| {
            let p = view::prefab(w, prefab).unwrap_or_else(|| panic!("no prefab {prefab}"));
            v.entity().owns((flecs::IsA::ID, *p.id()))
        };
        let laid = |v: View<'_>, what: &str| {
            let l = v.entity().try_cloned::<&Laid>().unwrap_or_else(|| panic!("{what} is not laid out"));
            assert!(l.w > 0.0 && l.h > 0.0, "{what} has no room: {l:?}");
            l
        };
        let action = |v: View<'_>| v.entity().try_cloned::<&Action>().map(|a| a.name.as_str().to_owned());
        let words_of = |v: View<'_>| v.entity().try_cloned::<&Text>().map(|t| t.s.as_str().to_owned()).unwrap_or_default();
        let one = |h: &str, v: &str| format!(r#"ui:["test.0","{h}",{v}]"#);

        // Each draws, as its prefab.
        for (i, prefab) in [
            (0, "hn.GenGrid"),
            (1, "hn.GenStack"),
            (2, "hn.GenWrap"),
            (3, "hn.GenScroll"),
            (4, "hn.GenSplit"),
            (5, "hn.GenTabs"),
            (6, "hn.GenAccordion"),
            (7, "hn.GenPopover"),
            (8, "hn.GenMenu"),
            (9, "hn.GenDialog"),
            (10, "hn.GenSheet"),
            (11, "hn.GenToast"),
            (12, "hn.GenProgress"),
            (13, "hn.GenLoading"),
            (14, "hn.GenSkeleton"),
            (15, "hn.GenIcon"),
            (16, "hn.GenTileOn"),
            (17, "hn.GenFileRef"),
            (21, "hn.GenSplitDown"),
            (22, "hn.GenGridFit"),
            (23, "hn.GenScrollAcross"),
        ] {
            assert!(is(el(i), prefab), "element {i} is a {prefab}");
            laid(el(i), prefab);
        }
        let grid = |r: usize, c: usize| part(el(0), &format!("r{r}.c{c}"));
        let (a, c) = (laid(grid(0, 0), "a grid cell"), laid(grid(1, 0), "the next row's cell"));
        assert!(c.y >= a.y + a.h + 11.0 && (laid(grid(0, 1), "b").x - a.x - a.w - 12.0).abs() < 0.5, "two columns 12 apart");
        let stack = laid(el(1), "the stack");
        let pic = laid(part(el(1), "g0.pic"), "the image");
        assert!((pic.w - 96.0).abs() < 0.5 && (pic.h - 96.0).abs() < 0.5, "a 96 by 96 image: {pic:?}");
        assert_eq!(el(1).part("g0").and_then(|i| i.part("pic")).and_then(|p| p.entity().try_cloned::<&harness_native_ui_text_api::Picture>()).map(|p| p.handle.as_str().to_owned()), Some(icon.clone()));
        let badge = laid(part(el(1), "g1.g0"), "the layer's badge");
        assert!((stack.w - 96.0).abs() < 0.5 && (stack.h - 96.0).abs() < 0.5, "a stack is as big as its largest child, here the image over a smaller layer: {stack:?}");
        assert!((badge.x + badge.w - (stack.x + stack.w - 4.0)).abs() < 0.5 && (badge.y - stack.y - 4.0).abs() < 0.5, "the badge at the top right, 4 in: {badge:?} in {stack:?}");
        assert_eq!(action(part(el(2), "g0")), Some(one("h2", "true")), "a chip with no value sends !selected");
        assert!(is(part(el(2), "g1"), "hn.GenChipOn"));
        assert_eq!(action(part(el(2), "g1")), Some(one("h3", "\"tests\"")));
        assert!((laid(el(3), "the scroll").h - 100.0).abs() < 0.5, "a scroll is its height tall");
        let (sa, sb) = (laid(part(el(4), "a"), "the split's a"), laid(part(el(4), "b"), "the split's b"));
        assert!((sa.w / (sa.w + sb.w) - 0.4).abs() < 0.01, "panes by the ratio: {sa:?} {sb:?}");
        let rule = action(part(el(4), "rule")).unwrap_or_default();
        assert!(rule.starts_with("ui:{") && rule.contains(r#""k":"split""#) && rule.contains(r#""change":"h6""#), "the rule drags: {rule}");
        let quiet = action(part(el(21), "rule")).unwrap_or_default();
        assert!(quiet.contains(r#""k":"split""#) && quiet.contains(r#""change":null"#), "a split with no onChange drags all the same: {quiet}");
        assert_eq!(action(part(el(5), "bar.tabs.t0")), Some(one("h1", "\"a\"")));
        assert!(is(part(el(5), "bar.tabs.t1"), "hn.GenTabOn") && words_of(part(el(5), "bar.tabs.t1.head.label")) == "Bee");
        laid(part(el(5), "body.g0"), "the tab's body");
        assert_eq!(action(part(el(6), "g0.head")), Some(one("h7", "false")));
        laid(part(el(6), "g0.body.g0"), "the open disclosure's body");
        assert!(el(6).part("g1").and_then(|d| d.part("body")).is_none(), "a shut disclosure shows no body");
        let both = r#"ui:[["test.0","h7",false],["test.0","h8",true]]"#;
        assert_eq!(action(part(el(6), "g1.head")).as_deref(), Some(both), "opening one closes the open one first");
        let trig = laid(part(el(7), "trigger"), "the popover's trigger");
        assert_eq!(action(part(el(7), "trigger")), Some(one("h9", "false")));
        assert_eq!(action(part(el(7), "trigger.g0")), Some(one("h9", "false")), "its button toggles it");
        let fl = laid(part(el(7), "float"), "the popover's plate");
        assert!(fl.y >= trig.y + trig.h + 5.5, "the plate opens under its trigger: {fl:?} {trig:?}");
        laid(part(el(7), "float.g0"), "the popover's content");
        let pick = r#"ui:[["test.0","h11","Rename"],["test.0","h10",false]]"#;
        assert_eq!(action(part(el(8), "float.i0")).as_deref(), Some(pick));
        assert!(is(part(el(8), "float.i1"), "hn.GenMenuSep") && is(part(el(8), "float.i2"), "hn.GenMenuDanger"));
        assert_eq!(words_of(part(el(8), "float.i2.keys")), "\u{2318}D");
        assert_eq!(action(part(el(8), "float.i3")), None, "a disabled item takes no press");
        assert!(part(el(8), "float.i3").entity().try_cloned::<&Pose>().is_some_and(|p| p.fade > 0.0), "and is faded");
        let pane = laid(el(9), "the dialog");
        assert!((pane.w - 900.0).abs() < 0.5 && (pane.h - 2600.0).abs() < 0.5, "a dialog covers the pane: {pane:?}");
        assert_eq!(action(part(el(9), "scrim")), Some(one("h15", "false")));
        let plate = laid(part(el(9), "scrim.plate"), "the dialog's plate");
        assert!(plate.w <= 380.5 && (plate.x + plate.w / 2.0 - 450.0).abs() < 1.0, "a dialog's plate is centred: {plate:?}");
        assert_eq!(words_of(part(el(9), "scrim.plate.title")), "Delete the build?");
        assert_eq!(action(part(el(10), "scrim")), None, "a sheet that may not be dismissed closes on no press");
        let sheet = laid(part(el(10), "scrim.plate"), "the sheet's plate");
        assert!((sheet.y + sheet.h - 2600.0).abs() < 0.5, "a sheet sits on the pane's foot: {sheet:?}");
        let toast = action(part(el(11), "plate")).unwrap_or_default();
        assert!(toast.contains(r#""k":"toast""#) && toast.contains(r#""change":"h18""#), "a toast's press dismisses it: {toast}");
        let fill = laid(part(el(12), "body.fill"), "the progress");
        assert!((fill.w / laid(part(el(12), "body"), "the track").w - 0.6).abs() < 0.01, "3 of 5");
        assert!(part(el(13), "spinner.arc").entity().try_cloned::<&harness_native_ui_text_api::Vector>().is_some_and(|v| v.ma != 0.0 || v.mb != 0.0), "the arc turns");
        assert!(is(part(el(14), "b1"), "hn.GenBoneShort") && laid(part(el(14), "b1"), "a bone").w < laid(part(el(14), "b0"), "a bone").w);
        let rust = harness_native_ui_inline::files::kind_named("rust").map(harness_native_ui_inline::files::drawing);
        assert_eq!(el(15).entity().try_cloned::<&harness_native_ui_text_api::Drawing>().map(|d| d.name.as_str().to_owned()).as_deref(), rust, "rust is a file kind, drawn");
        assert_eq!(action(el(16)), Some(one("h5", "\"icon\"")));
        assert_eq!(action(el(17)).as_deref(), Some("open:60:/x/src/tree.ts"), "a file chip opens its file at its line");
        assert_eq!((words_of(part(el(17), "name")), words_of(part(el(17), "line"))), ("tree.ts".to_owned(), ":60".to_owned()));
        // A Button sends its value; a Table's query keeps the rows that have it, each marked, and its rows and cells send.
        assert_eq!(action(el(25)), Some(one("h20", r#"{"n":2}"#)), "a button sends its value");
        let row0 = part(el(26), "card.grid.scroll.inner.body.r0");
        assert_eq!(action(row0), Some(one("h21", "2")), "onSelect hears the row's index in rows");
        assert!(el(26).part("card").and_then(|c| part(c, "grid.scroll.inner.body").part("r1")).is_none(), "the rows without the query go");
        assert_eq!(action(part(row0, "c1")), Some(one("h22", "\"g\"")), "a cell's open goes to onOpen");
        assert!(is(part(row0, "c1"), "hn.ChatTableCellLink"));
        let found = part(row0, "c1.text.match").entity().try_cloned::<&harness_native_ui_text_api::TextRange>();
        assert_eq!(found, Some(harness_native_ui_text_api::TextRange { from: 0, to: 2 }), "the match is marked, case aside");
        assert_eq!(words_of(part(el(27), "foot")), "No rows match");
        assert_eq!(
            action(el(28)).as_deref(),
            Some(r#"ui:{"open":"3:/x/a.rs","send":[["test.0","h23","/x/a.rs"]]}"#),
            "a file chip with onClick opens it, then sends its path"
        );
        assert_eq!(words_of(part(el(28), "name")), "entry");
        for (i, says) in [(18, "<Layer> is not in a Stack"), (19, "<Icon> name"), (20, "<Grid> has no prop \u{201C}foo\u{201D}")] {
            let t = words_of(el(i));
            assert!(t.contains(says), "element {i} is refused in words: {t:?}");
        }
        assert_eq!(live.borrow().floats.len(), 4, "the popover, the menu, the dialog and the sheet are noted to close");

        // A toast's time runs out: it goes, and says so to its onDismiss.
        world.get::<&mut Clock>(|c| c.time += 0.5);
        frame(&ui, None);
        assert!(el(24).part("plate").is_none(), "a toast whose time ran out goes");
        assert!(el(11).part("plate").is_some(), "one that stays stays");
        assert_eq!(live.borrow().pending, [("test.0".to_owned(), "h19".to_owned(), Value::Null)]);

        // Real presses: the dialog's scrim (off its plate), then with nothing floating a tab and the accordion's shut head.
        let press = |frame: &mut dyn FnMut(&Value, Option<PointerState>), ui: &Value, at: Laid| -> Vec<Option<String>> {
            let (x, y) = (at.x + at.w / 2.0, at.y + at.h / 2.0);
            let mut before = Vec::new();
            world.new_query::<&Activated>().each_entity(|e, _| before.push(e.id()));
            frame(ui, Some(PointerState { x, y, down: false, went_down: false, went_up: false }));
            frame(ui, Some(PointerState { x, y, down: true, went_down: true, went_up: false }));
            frame(ui, Some(PointerState { x, y, down: false, went_down: false, went_up: true }));
            let mut got = Vec::new();
            world.new_query::<&Activated>().each_entity(|e, _| {
                if !before.contains(&e.id()) {
                    got.push(e.try_cloned::<&Action>().map(|a| a.name.as_str().to_owned()));
                }
            });
            got
        };
        let only_dialog = tree(true, false, false);
        for _ in 0..4 {
            frame(&only_dialog, None);
        }
        let corner = Laid { x: pane.x + 4.0, y: pane.y + 4.0, w: 4.0, h: 4.0 };
        assert_eq!(press(&mut frame, &only_dialog, corner), [Some(one("h15", "false"))], "a press on the scrim closes the dialog");
        let calm = tree(false, false, false);
        for _ in 0..4 {
            frame(&calm, None);
        }
        let tab = laid(part(el(5), "bar.tabs.t0"), "a tab");
        assert_eq!(press(&mut frame, &calm, tab), [Some(one("h1", "\"a\""))], "a press on a tab sends its value");
        let head = laid(part(el(6), "g1.head"), "the shut head");
        assert_eq!(press(&mut frame, &calm, head), [Some(both.to_owned())], "a press on a shut head in an accordion sends both");
    }

    /// Property `shaders-and-videos-draw-and-resolve`: with no Media (a test, or before the window has a renderer) a cell's
    /// Shader draws quietly as building and its Video with its controls, each box sized from its props (a shader's aspect,
    /// a video's 16:9 until its own shape is known), each picture a Surface keyed by its element with a press that plays;
    /// once the whole cell is filled a Shader's `@id` uniforms read the live values and its texture refs bind the elements
    /// (an earlier one, a later one, itself) or an Image's file, wherever they sit in the tree; an id the cell does not
    /// have is said over the shader, and a wire that is not the kernel's is refused in words.
    #[test]
    fn shaders_and_videos_draw_and_resolve() {
        use harness_native_render::media::Input;
        use harness_native_ui_input_api::Action;
        use harness_native_ui_text_api::{Icon, Surface};
        let world = crate::app::scene_world().unwrap_or_else(|e| panic!("{e}"));
        let w = world.world_ref();
        harness_native_ui_layout::install(w);
        harness_native_ui_input::install(w);
        harness_native_ui_motion::install(w);
        world.set(Clock::default());
        world.set(Viewport { w: 900.0, h: 2600.0 });
        let icon = format!("{}/../../../docs/brand/blaze-icon-dark-180.png", env!("CARGO_MANIFEST_DIR"));
        let ui = serde_json::json!([
            {"type": "Shader", "props": {"msl": "// msl", "key": "00000000000000ab", "uniforms": [
                {"name": "gain", "lanes": 1, "value": [0, 0, 0, 0], "from": "gain"},
                {"name": "tint", "lanes": 4, "value": [1, 0.5, 0.25, 1]}], "textures": [
                {"name": "clip", "kind": "ref", "ref": "clip"},
                {"name": "pic", "kind": "image", "src": icon},
                {"name": "self", "kind": "ref", "ref": "fx"},
                {"name": "photo", "kind": "ref", "ref": "logo"}],
                "aspect": 2, "animate": true, "pointer": false, "id": "fx"}, "children": []},
            {"type": "Video", "props": {"id": "clip", "src": "/nowhere/surf.mp4", "autoplay": true, "loop": true, "muted": true,
                "controls": true, "fit": "contain", "onPlay": "h2", "onPause": "h3"}, "children": []},
            {"type": "Slider", "props": {"id": "gain", "label": "Glow", "min": 0, "max": 4, "step": 0.01, "value": 1.5, "onChange": "h1"}, "children": []},
            {"type": "Image", "props": {"id": "logo", "src": icon, "width": 32, "height": 32}, "children": []},
            {"type": "Shader", "props": {"msl": "// msl", "key": "00000000000000ac", "uniforms": [
                {"name": "g", "lanes": 1, "value": [0, 0, 0, 0], "from": "nope"}], "textures": [], "animate": false, "pointer": false}, "children": []},
            {"type": "Shader", "props": {"msl": "// msl", "key": "zz", "uniforms": [], "textures": [], "animate": false, "pointer": false}, "children": []},
            {"type": "Video", "props": {"autoplay": false, "loop": false, "muted": true, "controls": true, "fit": "contain"}, "children": []}
        ]);
        let live = std::cell::RefCell::new(Live::default());
        let row = View::root(w, "hn.ChatUi");
        let mut measure = |s: &str, st: &TextStyle, _md: bool| s.chars().count() as f32 * st.size.max(1.0) * 0.55;
        for _ in 0..6 {
            fill(row, &ui, "test.0", Some(&live), false, &mut measure);
            harness_native_ui_layout::typeset::typeset(w, &mut harness_native_ui_layout::typeset::Estimate);
            world.get::<&mut Clock>(|c| c.time += 1.0 / 60.0);
            world.progress_time(1.0 / 60.0);
        }
        let body = row.part("body").expect("body");
        let el = |i: usize| body.part(&format!("g{i}")).unwrap_or_else(|| panic!("element {i}"));
        fn part<'w>(v: View<'w>, path: &str) -> View<'w> {
            path.split('.').try_fold(v, |v, p| v.part(p)).unwrap_or_else(|| panic!("no part {path}"))
        }
        let is = |v: View<'_>, prefab: &str| {
            let p = view::prefab(w, prefab).unwrap_or_else(|| panic!("no prefab {prefab}"));
            v.entity().owns((flecs::IsA::ID, *p.id()))
        };
        let laid = |v: View<'_>, what: &str| {
            let l = v.entity().try_cloned::<&Laid>().unwrap_or_else(|| panic!("{what} is not laid out"));
            assert!(l.w > 0.0 && l.h > 0.0, "{what} has no room: {l:?}");
            l
        };
        let words_of = |v: View<'_>| v.entity().try_cloned::<&Text>().map(|t| t.s.as_str().to_owned()).unwrap_or_default();
        let (fx, clip) = (el(0), el(1));
        assert!(is(fx, "hn.GenShader") && is(clip, "hn.GenVideo"));
        assert_eq!(part(fx, "pic").entity().try_cloned::<&Surface>(), Some(Surface { key: fx.id(), cover: 0 }));
        assert_eq!(part(clip, "pic").entity().try_cloned::<&Surface>(), Some(Surface { key: clip.id(), cover: 0 }));
        let s = laid(fx, "the shader");
        assert!(s.w > 400.0 && (s.h - s.w / 2.0).abs() < 1.0, "a shader of aspect 2 as wide as the column: {s:?}");
        let v = laid(clip, "the video");
        assert!(v.w > 400.0 && (v.h - v.w * 9.0 / 16.0).abs() < 1.0, "a video 16:9 until its shape is known: {v:?}");
        assert_eq!(words_of(part(fx, "status.text")), "Building shader", "with no Media a shader is building");
        let play = format!("media:play:{}", clip.id());
        assert_eq!(part(clip, "pic").entity().try_cloned::<&Action>().map(|a| a.name.as_str().to_owned()), Some(play.clone()));
        assert_eq!(part(clip, "bar.plate.play").entity().try_cloned::<&Action>().map(|a| a.name.as_str().to_owned()), Some(play));
        let glyph = part(clip, "bar.plate.play.icon").entity().try_cloned::<&Icon>().map(|i| i.glyph.as_str().to_owned());
        assert_eq!(glyph.as_deref(), Some("\u{EAD1}"), "an autoplaying video shows pause");
        assert_eq!(words_of(part(clip, "bar.plate.elapsed")), "0:00");
        let (slots, inputs, problem) = surface::resolved(&live.borrow(), fx.id()).expect("the shader's state");
        assert_eq!(slots, [[1.5, 0.0, 0.0, 0.0], [1.0, 0.5, 0.25, 1.0]], "@gain reads the slider below it");
        assert_eq!(inputs, [Input::Surface(clip.id()), Input::Photo(icon.clone()), Input::Surface(fx.id()), Input::Photo(icon.clone())]);
        assert_eq!(problem, None);
        let lost = words_of(part(el(4), "status.text"));
        assert!(lost.contains("@nope") && lost.contains("no element"), "an unknown id is said over the shader: {lost:?}");
        for (i, says) in [(5, "<Shader> key"), (6, "<Video> src")] {
            let t = words_of(el(i));
            assert!(t.contains(says), "element {i} is refused in words: {t:?}");
        }
    }

    /// Property `layers-size-their-stack`: a Stack whose children are all Layers is as big as its largest Layer's content
    /// (here a 280 by 30 Shader), not 0 by 0, and a Text in another Layer is proposed the stack's width, so it sits on one
    /// line at its anchor (the left, centred down) instead of wrapping a character a line.
    #[test]
    fn layers_size_their_stack() {
        use harness_native_ui_layout_api::Intrinsic;
        let world = crate::app::scene_world().unwrap_or_else(|e| panic!("{e}"));
        let w = world.world_ref();
        harness_native_ui_layout::install(w);
        harness_native_ui_input::install(w);
        harness_native_ui_motion::install(w);
        world.set(Clock::default());
        world.set(Viewport { w: 900.0, h: 2600.0 });
        let ui = serde_json::json!([
            {"type": "Stack", "props": {}, "children": [
                {"type": "Layer", "props": {}, "children": [
                    {"type": "Shader", "props": {"msl": "// msl", "key": "00000000000000ad", "uniforms": [], "textures": [],
                        "width": 280, "height": 30, "animate": false, "pointer": false}, "children": []}]},
                {"type": "Layer", "props": {"anchor": "left"}, "children": [
                    {"type": "Text", "props": {"text": "fix model picker"}, "children": []}]}]},
            {"type": "Text", "props": {"text": "after the stack"}, "children": []}
        ]);
        let live = std::cell::RefCell::new(Live::default());
        let row = View::root(w, "hn.ChatUi");
        let mut measure = |s: &str, st: &TextStyle, _md: bool| s.chars().count() as f32 * st.size.max(1.0) * 0.55;
        for _ in 0..6 {
            fill(row, &ui, "test.0", Some(&live), false, &mut measure);
            harness_native_ui_layout::typeset::typeset(w, &mut harness_native_ui_layout::typeset::Estimate);
            world.get::<&mut Clock>(|c| c.time += 1.0 / 60.0);
            world.progress_time(1.0 / 60.0);
        }
        let body = row.part("body").expect("body");
        let el = |i: usize| body.part(&format!("g{i}")).unwrap_or_else(|| panic!("element {i}"));
        fn part<'w>(v: View<'w>, path: &str) -> View<'w> {
            path.split('.').try_fold(v, |v, p| v.part(p)).unwrap_or_else(|| panic!("no part {path}"))
        }
        let laid = |v: View<'_>, what: &str| {
            let l = v.entity().try_cloned::<&Laid>().unwrap_or_else(|| panic!("{what} is not laid out"));
            assert!(l.w > 0.0 && l.h > 0.0, "{what} has no room: {l:?}");
            l
        };
        let stack = laid(el(0), "the stack");
        let fx = laid(part(el(0), "g0.g0"), "the shader");
        assert!((fx.w - 280.0).abs() < 0.5 && (fx.h - 30.0).abs() < 0.5, "a 280 by 30 shader: {fx:?}");
        assert!((stack.w - 280.0).abs() < 0.5 && (stack.h - 30.0).abs() < 0.5, "a stack of layers is its largest layer's size: {stack:?}");
        let words = part(el(0), "g1.g0");
        let t = laid(words, "the layer's text");
        let unwrapped = words.entity().try_cloned::<&Intrinsic>().map_or(0.0, |i| i.w);
        let line = words.entity().try_cloned::<&TextStyle>().map_or(0.0, |s| s.line);
        assert!(unwrapped > 0.0 && unwrapped < 280.0, "the words are measured, narrower than the shader: {unwrapped}");
        assert!(t.w + 0.5 >= unwrapped && t.h < line * 1.5, "the text is on one line: {t:?}, {unwrapped} wide unwrapped, lines {line} tall");
        assert!((t.x - stack.x).abs() < 0.5 && t.y >= stack.y - 0.5 && t.y + t.h <= stack.y + stack.h + line, "the text at the stack's left: {t:?} in {stack:?}");
        let after = laid(el(1), "the text after the stack");
        assert!(after.y >= stack.y + stack.h - 0.5, "what follows the stack starts below it: {after:?} after {stack:?}");
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
