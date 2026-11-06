//! Tier 3's props as the kernel sends them (the scratchpad's tier3-wire.md, mcp/src/ui/element.ts): each element's own
//! names, and what each must be. The kernel checks them first and fills in every default; what reaches the app wrong (a
//! prop it does not know, a value of the wrong kind, a child where none goes) is drawn as a refusal naming the element and
//! the prop, never guessed around.
use super::*;

/// The tier-3 elements and the props each takes on the wire.
const PROPS: [(&str, &[&str]); 29] = [
    ("Grid", &["columns", "fit", "gap", "pad"]),
    ("Stack", &["align"]),
    ("Layer", &["anchor", "x", "y", "z", "onClick"]),
    ("Wrap", &["gap"]),
    ("Scroll", &["height", "horizontal"]),
    ("Split", &["ratio", "vertical", "onChange"]),
    ("Tabs", &["tabs", "selected", "onChange"]),
    ("Disclosure", &["title", "open", "onToggle"]),
    ("Accordion", &[]),
    ("Popover", &["placement", "align", "trigger", "open", "onToggle"]),
    ("Menu", &["items", "open", "context", "onSelect", "onToggle"]),
    ("Dialog", &["title", "open", "dismissable", "onClose"]),
    ("Sheet", &["title", "open", "dismissable", "onClose"]),
    ("Toast", &["text", "tone", "duration", "onDismiss"]),
    ("Progress", &["value", "max", "label"]),
    ("Loading", &["label"]),
    ("Skeleton", &["lines"]),
    ("Icon", &["name", "kind"]),
    ("Image", &["src", "width", "height", "fit", "alt", "id"]),
    ("Chip", &["label", "selected", "value", "onClick"]),
    ("Tile", &["gap", "pad", "value", "onClick"]),
    ("FileRef", &["path", "line", "kind", "name", "onClick"]),
    ("Kip", &["mood", "size"]),
    ("Shader", &["msl", "key", "uniforms", "textures", "width", "height", "aspect", "animate", "pointer", "id"]),
    (
        "Video",
        &[
            "src", "poster", "autoplay", "loop", "muted", "controls", "width", "height", "aspect", "fit", "id", "paused", "currentTime",
            "onTimeUpdate", "onEnded", "onPlay", "onPause",
        ],
    ),
    // The new elements (lane E: genui/files.rs, cellref.rs, color.rs, dropzone.rs).
    ("File", &["name", "kind", "size", "modified", "value", "onClick"]),
    ("CellRef", &["task", "call", "title"]),
    ("ColorPicker", &["options", "selected", "label", "disabled", "onChange"]),
    ("DropZone", &["label", "onDrop"]),
];

pub(super) const TIER3: [&str; 29] = {
    let mut out = [""; 29];
    let mut i = 0;
    while i < 29 {
        out[i] = PROPS[i].0;
        i += 1;
    }
    out
};

/// A Layer's anchors, and where a Stack puts its children that have none (its `align`).
pub(super) const ANCHORS: [&str; 9] = ["top-left", "top", "top-right", "left", "center", "right", "bottom-left", "bottom", "bottom-right"];
/// The most a Scroll is tall (points), the most tabs and menu items, the longest title or label, the most skeleton lines.
pub(super) const MAX_SCROLL_H: f64 = 600.0;
const MAX_TABS: usize = 20;
const MAX_ITEMS: usize = 50;
const MAX_TITLE: usize = 200;
const MAX_LINES: f64 = 20.0;

/// A Toast's tones (the scene's hn.GenTone).
const TONES: [&str; 7] = ["ink", "muted", "strong", "error", "accent", "success", "warning"];

/// The elements that hold nothing.
const LEAVES: [&str; 14] = ["Toast", "Progress", "Loading", "Skeleton", "Icon", "Image", "Chip", "FileRef", "Kip", "Shader", "Video", "File", "CellRef", "ColorPicker"];

/// A Kip's mood words (the kernel's `KIP_MOODS`; harness-native-kip act.rs plays them), what joins them, the most it plays in
/// turn, and how tall it stands (points).
const KIP_MOODS: [&str; 8] = ["wave", "happy", "celebrate", "thinking", "working", "oops", "sleepy", "idle"];
const KIP_JOINS: [&str; 2] = ["then", "and"];
const MAX_KIP_STEPS: usize = 6;
pub(super) const KIP_SIZES: std::ops::RangeInclusive<f64> = 20.0..=160.0;

/// The prefab a tier-3 element's props pick, when not its type's own.
pub(super) fn prefab(kind: &str, node: &Value) -> Option<&'static str> {
    let p = &node["props"];
    Some(match kind {
        "Grid" if !p["fit"].is_null() => "hn.GenGridFit",
        "Scroll" if p["horizontal"] == true => "hn.GenScrollAcross",
        "Split" if p["vertical"] == true => "hn.GenSplitDown",
        "Chip" if p["selected"] == true => "hn.GenChipOn",
        "Tile" if p["onClick"].is_string() => "hn.GenTileOn",
        "File" if p["onClick"].is_string() => "hn.GenFileOn",
        _ => return None,
    })
}

/// Why tier-3 element `node` (sitting in `within`) is not drawn, or None.
pub(super) fn check(node: &Value, within: &Within) -> Option<String> {
    let kind = node["type"].as_str().unwrap_or("");
    let p = &node["props"];
    let kids: &[Value] = node["children"].as_array().map_or(&[], Vec::as_slice);
    let names = PROPS.iter().find(|(k, _)| *k == kind).map_or(&[][..], |(_, n)| *n);
    if let Value::Object(m) = p {
        if let Some(k) = m.keys().find(|k| !names.contains(&k.as_str()) && !super::common::COMMON.contains(&k.as_str())) {
            let said: String = k.chars().take(32).collect();
            let has = if names.is_empty() { "it takes none".to_owned() } else { format!("its props are {}", names.join(", ")) };
            return Some(format!("<{kind}> has no prop \u{201C}{said}\u{201D}: {has}"));
        }
    } else if !p.is_null() {
        return Some(format!("<{kind}> props is {}, not a record", control::describe(p)));
    }
    let bad = |name: &str, want: &str| Some(format!("<{kind}> {name} is {}, not {want}", control::describe(&p[name])));
    for h in names.iter().filter(|n| n.starts_with("on")) {
        if !matches!(p[*h], Value::Null | Value::String(_)) {
            return bad(h, "a handler");
        }
    }
    let number = |name: &str| -> Result<Option<f64>, Option<String>> {
        match &p[name] {
            Value::Null => Ok(None),
            Value::Number(n) => Ok(n.as_f64()),
            _ => Err(bad(name, "a number")),
        }
    };
    let flag = |name: &str| (!matches!(p[name], Value::Null | Value::Bool(_))).then(|| bad(name, "true or false")).flatten();
    let text = |name: &str, required: bool| match &p[name] {
        Value::Null if required => bad(name, "words (it is required)"),
        Value::Null => None,
        Value::String(s) if s.chars().count() > MAX_TITLE => Some(format!("<{kind}> {name} is {} characters, more than {MAX_TITLE}", s.chars().count())),
        Value::String(_) => None,
        _ => bad(name, "words"),
    };
    let at_least = |name: &str, lo: f64, open: bool| match number(name) {
        Err(e) => e,
        Ok(Some(v)) if v < lo || (open && v == lo) => bad(name, &format!("a number {} {lo}", if open { "above" } else { "of at least" })),
        Ok(_) => None,
    };
    // A file kind's name (an Icon's or a FileRef's `kind`): one of media.rs's kinds.
    let file_kind = |k: &str| {
        (!media::KIND_NAMES.contains(&k)).then(|| {
            let said: String = k.chars().take(40).collect();
            format!("<{kind}> kind \u{201C}{said}\u{201D} is not a file kind: {}", media::KIND_NAMES.join(", "))
        })
    };
    if LEAVES.contains(&kind) && !kids.is_empty() {
        return Some(format!("<{kind}> holds nothing, but is given {} children", kids.len()));
    }
    match kind {
        "Grid" => match (number("columns"), number("fit")) {
            (Err(e), _) | (_, Err(e)) => e,
            (Ok(Some(_)), Ok(Some(_))) => Some(format!("<{kind}> is given both columns and fit: one or the other")),
            (Ok(Some(c)), _) if !(1.0..=12.0).contains(&c) || c.fract() != 0.0 => bad("columns", "a whole number from 1 to 12"),
            (Ok(None), Ok(None)) => Some(format!("<{kind}> is given neither columns nor fit")),
            (_, Ok(Some(f))) if f <= 0.0 => bad("fit", "points above 0"),
            _ => at_least("gap", 0.0, false).or_else(|| at_least("pad", 0.0, false)),
        },
        "Stack" => match &p["align"] {
            Value::Null => None,
            Value::String(a) if ANCHORS.contains(&a.as_str()) => None,
            _ => bad("align", &ANCHORS.join(", ")),
        },
        "Layer" => {
            if *within != Within::Stack {
                return Some(format!("<{kind}> is not in a Stack: a Layer goes only directly in a <Stack>"));
            }
            match &p["anchor"] {
                Value::Null => {}
                Value::String(a) if ANCHORS.contains(&a.as_str()) => {}
                _ => return bad("anchor", &ANCHORS.join(", ")),
            }
            ["x", "y", "z"].into_iter().find_map(|n| number(n).err().flatten())
        }
        "Wrap" => at_least("gap", 0.0, false),
        "Scroll" => at_least("height", 0.0, true).or_else(|| flag("horizontal")),
        "Split" => {
            if kids.len() != 2 {
                return Some(format!("<{kind}> holds exactly 2 panes, not {}", kids.len()));
            }
            match number("ratio") {
                Err(e) => e,
                Ok(Some(r)) if !(0.05..=0.95).contains(&r) => bad("ratio", "a number from 0.05 to 0.95"),
                Ok(_) => flag("vertical"),
            }
        }
        "Tabs" => tabs(node).err(),
        "Disclosure" => text("title", true).or_else(|| flag("open")),
        "Accordion" => kids.iter().enumerate().find_map(|(i, k)| {
            let t = k["type"].as_str().unwrap_or("");
            (t != "Disclosure").then(|| format!("<{kind}> holds only Disclosures, but child {i} is a {}", if t.is_empty() { "value" } else { t }))
        }),
        "Popover" | "Menu" => {
            // A context menu's children are the region a right press on opens it in; any other's first is its trigger.
            let context = kind == "Menu" && p["context"] == true;
            let least = if kind == "Menu" && !context { kids.len() != 1 } else { kids.is_empty() };
            if least {
                let want = match kind {
                    "Menu" if context => "the region a right press opens it in",
                    "Menu" => "exactly 1 child (its trigger)",
                    _ => "its trigger first",
                };
                return Some(format!("<{kind}> holds {want}, not {} children", kids.len()));
            }
            if kind == "Popover" {
                match &p["placement"] {
                    Value::Null => {}
                    Value::String(s) if matches!(s.as_str(), "top" | "bottom" | "left" | "right") => {}
                    _ => return bad("placement", "top, bottom, left or right"),
                }
                match &p["align"] {
                    Value::Null => {}
                    Value::String(s) if matches!(s.as_str(), "start" | "center") => {}
                    _ => return bad("align", "start or center"),
                }
                match &p["trigger"] {
                    Value::Null => {}
                    Value::String(s) if matches!(s.as_str(), "click" | "hover") => {}
                    _ => return bad("trigger", "click or hover"),
                }
            }
            flag("open").or_else(|| flag("context")).or_else(|| if kind == "Menu" { items(node).err() } else { None })
        }
        "Dialog" | "Sheet" => text("title", false).or_else(|| flag("open")).or_else(|| flag("dismissable")),
        "Toast" => text("text", true).or_else(|| at_least("duration", 0.0, false)).or_else(|| match &p["tone"] {
            Value::Null => None,
            Value::String(t) if TONES.contains(&t.as_str()) => None,
            _ => bad("tone", &TONES.join(", ")),
        }),
        "Progress" => match (number("value"), number("max")) {
            (Err(e), _) | (_, Err(e)) => e,
            (_, Ok(Some(m))) if m <= 0.0 => bad("max", "a number above 0"),
            (Ok(Some(v)), _) if v < 0.0 => bad("value", "a number of at least 0"),
            _ => text("label", false),
        },
        "Loading" => text("label", false),
        "Kip" => {
            let mood = match &p["mood"] {
                Value::String(m) => m.to_lowercase(),
                _ => return bad("mood", "mood words (it is required)"),
            };
            let words: Vec<&str> = mood.split(|c: char| !c.is_ascii_lowercase()).filter(|w| !w.is_empty() && !KIP_JOINS.contains(w)).collect();
            if let Some(w) = words.iter().find(|w| !KIP_MOODS.contains(w)) {
                let said: String = w.chars().take(32).collect();
                return Some(format!("<Kip> mood \u{201C}{said}\u{201D} is not a mood: {}", KIP_MOODS.join(", ")));
            }
            if words.is_empty() || words.len() > MAX_KIP_STEPS {
                return bad("mood", &format!("1 to {MAX_KIP_STEPS} mood words"));
            }
            match number("size") {
                Err(e) => e,
                Ok(Some(s)) if !KIP_SIZES.contains(&s) => bad("size", "points from 20 to 160"),
                Ok(_) => None,
            }
        }
        "Skeleton" => match number("lines") {
            Err(e) => e,
            Ok(Some(n)) if !(1.0..=MAX_LINES).contains(&n) || n.fract() != 0.0 => bad("lines", "a whole number from 1 to 20"),
            Ok(_) => None,
        },
        "Icon" => match (&p["name"], &p["kind"]) {
            (_, Value::String(k)) => file_kind(k),
            (_, Value::Null) => match &p["name"] {
                Value::String(n) if media::glyph(n).is_some() => None,
                Value::String(n) => {
                    let said: String = n.chars().take(40).collect();
                    Some(format!("<Icon> name \u{201C}{said}\u{201D} is not an icon: a file kind ({}) or a Nerd Font symbol (cod-…)", media::KIND_NAMES.join(", ")))
                }
                Value::Null => Some("<Icon> has neither name nor kind: one says what it draws".to_owned()),
                _ => bad("name", "an icon's name"),
            },
            _ => bad("kind", "a file kind"),
        },
        "Image" => {
            let src = match &p["src"] {
                Value::String(s) => s,
                _ => return bad("src", "a file's absolute path (it is required)"),
            };
            if !src.starts_with('/') {
                return Some(format!("<Image> src \u{201C}{src}\u{201D} is not an absolute path"));
            }
            if let Err(e) = media::photo_size(src) {
                return Some(format!("<Image> src: {e}"));
            }
            match &p["fit"] {
                Value::Null => {}
                Value::String(f) if matches!(f.as_str(), "contain" | "cover") => {}
                _ => return bad("fit", "contain or cover"),
            }
            at_least("width", 0.0, true).or_else(|| at_least("height", 0.0, true)).or_else(|| text("alt", false)).or_else(|| text("id", false))
        }
        // The kernel compiled the source and resolved the files; what is checked here is the wire's shape
        // (surface::wire), a box that can be drawn, and its flags.
        "Shader" => surface::wire(node)
            .err()
            .or_else(|| at_least("width", 0.0, true))
            .or_else(|| at_least("height", 0.0, true))
            .or_else(|| at_least("aspect", 0.0, true))
            .or_else(|| flag("animate"))
            .or_else(|| flag("pointer"))
            .or_else(|| text("id", false)),
        "Video" => {
            let src = match &p["src"] {
                Value::String(s) if !s.is_empty() => s,
                _ => return bad("src", "a video file's absolute path or an http(s) URL (it is required)"),
            };
            if !(src.starts_with('/') || src.starts_with("http://") || src.starts_with("https://")) {
                return Some(format!("<Video> src \u{201C}{src}\u{201D} is not an absolute path or an http(s) URL"));
            }
            match &p["poster"] {
                Value::Null => {}
                Value::String(s) if !s.starts_with('/') => return Some(format!("<Video> poster \u{201C}{s}\u{201D} is not an absolute path")),
                Value::String(s) => {
                    if let Err(e) = media::photo_size(s) {
                        return Some(format!("<Video> poster: {e}"));
                    }
                }
                _ => return bad("poster", "an image file's absolute path"),
            }
            match &p["fit"] {
                Value::Null => {}
                Value::String(f) if matches!(f.as_str(), "contain" | "cover") => {}
                _ => return bad("fit", "contain or cover"),
            }
            ["autoplay", "loop", "muted", "controls", "paused"]
                .into_iter()
                .find_map(|f| flag(f))
                .or_else(|| at_least("width", 0.0, true))
                .or_else(|| at_least("height", 0.0, true))
                .or_else(|| at_least("aspect", 0.0, true))
                .or_else(|| at_least("currentTime", 0.0, false))
                .or_else(|| text("id", false))
        }
        "Chip" => text("label", true).or_else(|| flag("selected")),
        "Tile" => at_least("gap", 0.0, false).or_else(|| at_least("pad", 0.0, false)),
        "File" => text("name", true).or_else(|| text("modified", false)).or_else(|| at_least("size", 0.0, false)).or_else(|| match &p["kind"] {
            Value::Null => None,
            Value::String(k) if media::kind_named(k).is_some() => None,
            _ => bad("kind", &format!("a file kind ({})", media::KIND_NAMES.join(", "))),
        }),
        "CellRef" => match (&p["task"], &p["call"]) {
            (Value::String(_) | Value::Number(_), Value::Null | Value::String(_)) => text("title", false),
            (Value::String(_) | Value::Number(_), _) => bad("call", "the cell's tool call id"),
            _ => bad("task", "a cell's task, 't3' or 3 (it is required)"),
        },
        "ColorPicker" => {
            let colour = |name: &str, s: &str| {
                (color::parse_hex(s).is_none() && !color::NAMED.contains(&s)).then(|| {
                    let said: String = s.chars().take(32).collect();
                    format!("<ColorPicker> {name} \u{201C}{said}\u{201D} is not a colour: #rrggbb or a tone ({})", color::NAMED.join(", "))
                })
            };
            let options = match &p["options"] {
                Value::Null => None,
                Value::Array(a) if a.len() > color::MAX_SWATCHES => Some(format!("<ColorPicker> has {} options: at most {}", a.len(), color::MAX_SWATCHES)),
                Value::Array(a) => a.iter().enumerate().find_map(|(i, o)| match o {
                    Value::String(s) => colour(&format!("options[{i}]"), s),
                    _ => Some(format!("<ColorPicker> options[{i}] is {}, not #rrggbb or a tone", control::describe(o))),
                }),
                _ => bad("options", "a list of #rrggbb words or tones"),
            };
            options
                .or_else(|| match &p["selected"] {
                    Value::Null => None,
                    Value::String(s) => colour("selected", s),
                    _ => bad("selected", "#rrggbb or a tone"),
                })
                .or_else(|| text("label", false))
                .or_else(|| flag("disabled"))
        }
        "DropZone" => text("label", false),
        "FileRef" => {
            let path = match &p["path"] {
                Value::String(s) => s,
                _ => return bad("path", "a file's absolute path (it is required)"),
            };
            if !path.starts_with('/') {
                return Some(format!("<FileRef> path \u{201C}{path}\u{201D} is not an absolute path"));
            }
            match number("line") {
                Err(e) => return e,
                Ok(Some(l)) if l < 1.0 || l.fract() != 0.0 => return bad("line", "a whole number of at least 1"),
                Ok(_) => {}
            }
            match &p["kind"] {
                Value::Null => text("name", false),
                Value::String(k) => file_kind(k).or_else(|| text("name", false)),
                _ => bad("kind", "a file kind"),
            }
        }
        _ => None,
    }
}

/// A Tabs's tabs as (label, value): `{label, value}` records, or words that are both.
pub(super) fn tabs(node: &Value) -> Result<Vec<(String, Value)>, String> {
    let given = prop(node, "tabs");
    let Some(all) = given.as_array() else {
        return Err(format!("<Tabs> tabs is {}, not a list: tabs={{[{{label: 'Files', value: 'files'}}]}}", control::describe(given)));
    };
    if all.is_empty() || all.len() > MAX_TABS {
        return Err(format!("<Tabs> has {} tabs: 1 to {MAX_TABS}", all.len()));
    }
    all.iter()
        .enumerate()
        .map(|(i, t)| match t {
            Value::String(s) => Ok((s.clone(), t.clone())),
            Value::Object(m) => match (m.get("label"), m.get("value")) {
                (Some(Value::String(l)), Some(v)) if !v.is_null() => Ok((l.clone(), v.clone())),
                _ => Err(format!("<Tabs> tabs[{i}] is not {{label, value}}")),
            },
            _ => Err(format!("<Tabs> tabs[{i}] is {}, not a tab", control::describe(t))),
        })
        .collect()
}

/// One of a Menu's rows.
pub(super) enum Item {
    Separator,
    Pick { label: String, value: Value, danger: bool, disabled: bool, keys: String },
}

/// A Menu's items: `"-"` a separator, words both label and value, or `{label, value, danger?, disabled?, keys?}` and
/// `{separator: true}`.
pub(super) fn items(node: &Value) -> Result<Vec<Item>, String> {
    let given = prop(node, "items");
    let Some(all) = given.as_array() else {
        return Err(format!("<Menu> items is {}, not a list: items={{['Rename', '-', {{label: 'Delete', value: 'rm', danger: true}}]}}", control::describe(given)));
    };
    if all.is_empty() || all.len() > MAX_ITEMS {
        return Err(format!("<Menu> has {} items: 1 to {MAX_ITEMS}", all.len()));
    }
    all.iter()
        .enumerate()
        .map(|(i, it)| match it {
            Value::String(s) if s == "-" => Ok(Item::Separator),
            Value::String(s) => Ok(Item::Pick { label: s.clone(), value: it.clone(), danger: false, disabled: false, keys: String::new() }),
            Value::Object(m) if m.get("separator") == Some(&Value::Bool(true)) => Ok(Item::Separator),
            Value::Object(m) => {
                let label = match m.get("label") {
                    Some(Value::String(l)) => l.clone(),
                    _ => return Err(format!("<Menu> items[{i}] has no label: {{label: 'Rename', value: 'rename'}}")),
                };
                let value = m.get("value").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| Value::String(label.clone()));
                let on = |k: &str| m.get(k) == Some(&Value::Bool(true));
                let keys = match m.get("keys") {
                    None | Some(Value::Null) => String::new(),
                    Some(Value::String(k)) => k.clone(),
                    Some(v) => return Err(format!("<Menu> items[{i}].keys is {}, not words", control::describe(v))),
                };
                Ok(Item::Pick { label, value, danger: on("danger"), disabled: on("disabled"), keys })
            }
            _ => Err(format!("<Menu> items[{i}] is {}, not an item", control::describe(it))),
        })
        .collect()
}
