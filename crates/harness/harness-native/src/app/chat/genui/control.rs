//! A cell's controls (scene/widgets/genui.flecs `hn.Gen<Button|Toggle|...>`): drawn from the tree's props and armed with
//! what a press sends. A plain press (a button, a toggle, an option, a stepper's sign) carries its whole event in its
//! Action: `ui:` and the JSON `[cell, handler, value]`, sent as it is ([`Link::ui_event`], `Request::UiEvent`); the kernel
//! draws the tree again and it comes back on the same card. A control that keeps state of its own while it is used (a
//! field being typed in, a slider being dragged, a select's open list) carries `ui:` and a JSON object, its [`Spec`]: the
//! app keeps that state in [`Live`] (one field, one slide and one open list at a time: there is one keyboard and one
//! pointer), shows it in place of the tree's value while it lasts, and sends at a bounded rate. A control with no handler,
//! or disabled, is drawn faded (`hn.GenInert`) and takes no press. Props it cannot draw are a refusal ([`check`]), drawn
//! in the failed tone naming the element and the prop.
use super::*;
use crate::link::Link;
use harness_native_ui_input_api::{Focused, Pressable};
use harness_native_ui_layout_api::{LayoutPriority, ScrollState, scroll_quietly};
use harness_native_ui_look_api::Edge;
use harness_native_ui_motion_api::Pose;
use std::collections::BTreeMap;

/// The most options a Select, Choice or Radio shows.
const MAX_OPTIONS: usize = 200;
/// A field sends what is typed at most this often (seconds) while it is typed, and once more as it loses the keys.
const TYPE_EVERY: f32 = 0.150;
/// A slider sends its value at most this often while it is dragged, and once more as it is let go.
const SLIDE_EVERY: f32 = 0.050;
/// cod-check, cod-chevron-down.
const CHECK: &str = "\u{EAB2}";
const CHEVRON: &str = "\u{EAB4}";

/// The controls that keep state while they are used: with tier 3's Split (its rule dragged as a slider's knob is),
/// Toast (a press hides it here as well as sending onDismiss) and a Video's scrub rail (dragged as a slider's; it seeks
/// the player here and sends nothing of its own: genui/surface.rs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Input,
    Area,
    Number,
    Slider,
    Select,
    Split,
    Toast,
    Scrub,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Input => "input",
            Kind::Area => "area",
            Kind::Number => "number",
            Kind::Slider => "slider",
            Kind::Select => "select",
            Kind::Split => "split",
            Kind::Toast => "toast",
            Kind::Scrub => "scrub",
        }
    }

    fn of(s: &str) -> Option<Self> {
        Some(match s {
            "input" => Kind::Input,
            "area" => Kind::Area,
            "number" => Kind::Number,
            "slider" => Kind::Slider,
            "select" => Kind::Select,
            "split" => Kind::Split,
            "toast" => Kind::Toast,
            "scrub" => Kind::Scrub,
            _ => return None,
        })
    }

    /// Dragged: a slider's knob, a split's rule, a video's scrub rail.
    fn drags(self) -> bool {
        matches!(self, Kind::Slider | Kind::Split | Kind::Scrub)
    }

    fn typed(self) -> bool {
        matches!(self, Kind::Input | Kind::Area | Kind::Number)
    }
}

/// What a stateful control's Action says: its cell, its entity (the control's own, the one [`Live`] is keyed by), its
/// kind, its handlers and a number's bounds and step.
#[derive(Clone, Debug, PartialEq)]
struct Spec {
    cell: String,
    node: u64,
    k: Kind,
    change: Option<String>,
    submit: Option<String>,
    min: f64,
    max: f64,
    step: f64,
}

impl Spec {
    fn action(&self) -> String {
        let n = |x: f64| if x.is_finite() { serde_json::json!(x) } else { Value::Null };
        let v = serde_json::json!({
            "cell": self.cell, "node": self.node, "k": self.k.name(), "change": self.change, "submit": self.submit,
            "min": n(self.min), "max": n(self.max), "step": n(self.step),
        });
        format!("ui:{v}")
    }

    fn parse(v: &Value) -> Option<Self> {
        let text = |k: &str| v[k].as_str().map(str::to_owned);
        Some(Spec {
            cell: text("cell")?,
            node: v["node"].as_u64()?,
            k: Kind::of(v["k"].as_str()?)?,
            change: text("change"),
            submit: text("submit"),
            min: v["min"].as_f64().unwrap_or(f64::NEG_INFINITY),
            max: v["max"].as_f64().unwrap_or(f64::INFINITY),
            step: v["step"].as_f64().unwrap_or(0.0),
        })
    }

    /// The spec in entity `e`'s Action, when it is a stateful control's.
    fn of(e: EntityView<'_>) -> Option<Self> {
        let a = e.try_cloned::<&Action>()?;
        let json = a.name.as_str().strip_prefix("ui:")?;
        if !json.starts_with('{') {
            return None;
        }
        Self::parse(&serde_json::from_str(json).ok()?)
    }
}

/// The Action of a press that sends `value` to `handler` of `cell`.
pub(super) fn send(cell: &str, handler: &str, value: Value) -> String {
    format!("ui:{}", serde_json::json!([cell, handler, value]))
}

/// The Action of a press that sends each of `events` (handler, value) to `cell`, in order: one event as [`send`] has it,
/// more as a list of them (an Accordion's close-then-open, a Menu's pick-then-close).
pub(super) fn sends(cell: &str, events: &[(String, Value)]) -> String {
    match events {
        [(h, v)] => send(cell, h, v.clone()),
        _ => format!("ui:{}", Value::Array(events.iter().map(|(h, v)| serde_json::json!([cell, h, v])).collect())),
    }
}

/// The Action of a press that does more than send `events` (handler, value) to `cell`: opens `open` first (an `open:`
/// target, src/open.rs; Option shows it in the Finder), and shuts the context menu `shut` here.
pub(super) fn then(cell: &str, events: &[(String, Value)], open: Option<&str>, shut: Option<u64>) -> String {
    let send: Vec<Value> = events.iter().map(|(h, v)| serde_json::json!([cell, h, v])).collect();
    let mut o = serde_json::Map::new();
    o.insert("send".to_owned(), Value::Array(send));
    if let Some(t) = open {
        o.insert("open".to_owned(), Value::String(t.to_owned()));
    }
    if let Some(n) = shut {
        o.insert("shut".to_owned(), Value::from(n));
    }
    format!("ui:{}", Value::Object(o))
}

/// The controls in use: the field that has the keys (its own words, authoritative until it lets them go), the slider
/// being dragged, the select whose list is open (its entity). With the clock and whether carets blink, for drawing.
#[derive(Default)]
pub struct Live {
    field: Option<Field>,
    slide: Option<Slide>,
    open: Option<u64>,
    now: f32,
    blink: bool,
    /// What floats open over the transcript (a popover, a menu, a dialog, a sheet), by its element's entity: what closes
    /// it. Filled as the trees are drawn; a press off a popover or a menu, or Escape, sends its close.
    pub(super) floats: BTreeMap<u64, Float>,
    /// Each toast's words, since when it shows (the clock), and whether it went (its time ran out, or it was pressed).
    pub(super) toasts: BTreeMap<u64, Toast>,
    /// Events a drawing decided to send (a toast whose time ran out): (cell, handler, value), sent by the next step.
    pub(super) pending: Vec<(String, String, Value)>,
    /// Something drawn moves on the clock (a spinner, a skeleton, a toast's time): keep drawing.
    pub(super) anim: bool,
    /// Each split dragged here, by its element's entity: where it was let go, over the ratio the tree said then. It holds
    /// until the tree says another ratio (its onChange's echo, or the cell's own change).
    splits: BTreeMap<u64, Kept>,
    /// The cells' Shaders and Videos, and the host's Media they are drawn by (genui/surface.rs).
    pub(super) surf: super::surface::Surfaces,
    /// The new elements' (lane E): the colour pickers picked here and the system colour panel's (genui/color.rs), the
    /// drop zones drawn and the one Finder files are carried over (genui/dropzone.rs).
    pub(super) colors: super::color::Colors,
    pub(super) drops: super::dropzone::Drops,
    /// Each popover that opens on hover, by its element's entity: whether it is open (the pointer on its trigger or its
    /// plate), so its onToggle hears each change once.
    pub(super) hovering: BTreeMap<u64, bool>,
    /// Each context menu open, by its element's entity: where the right press was, from its region's corner.
    pub(super) context: BTreeMap<u64, [f32; 2]>,
    /// When each Loading and Skeleton first showed (the clock): they stay unseen a moment, then fade in.
    pub(super) born: BTreeMap<u64, f32>,
    /// Each context menu's region as last drawn, by its entity: what a right press in it opens.
    pub(super) regions: BTreeMap<u64, Region>,
}

/// A context menu's region: its cell, the menu's element (the entity [`Live`]'s context is keyed by) and its onToggle.
#[derive(Clone)]
pub(super) struct Region {
    pub(super) cell: String,
    pub(super) menu: u64,
    pub(super) toggle: Option<String>,
}

/// The value of the slider (or split, or scrub rail) of entity `node` while it is dragged: where the pointer has it, not
/// where the tree last said. What an `@id` uniform reads at frame rate (genui/surface.rs).
pub(super) fn slide_value(live: &Live, node: u64) -> Option<f64> {
    live.slide.as_ref().filter(|s| s.spec.node == node).map(|s| s.value)
}

/// The video whose scrub rail is being dragged, and where to (seconds).
pub(super) fn scrubbing(live: &Live) -> Option<(u64, f64)> {
    live.slide.as_ref().filter(|s| s.spec.k == Kind::Scrub).map(|s| (s.spec.node, s.value))
}

/// The Action of video `node`'s scrub rail in cell `cell`: dragged as a slider over 0 to `duration` seconds, to a
/// millisecond; it seeks the player and sends nothing itself (its timeupdate does: genui/surface.rs).
pub(super) fn scrub_action(cell: &str, node: u64, duration: f64) -> String {
    Spec { cell: cell.to_owned(), node, k: Kind::Scrub, change: None, submit: None, min: 0.0, max: duration.max(0.001), step: 0.001 }.action()
}

/// A split's ratio as dragged (`at`) while the tree still says `from`.
struct Kept {
    from: f64,
    at: f64,
}

/// An open float: its cell, what closes it (the events to send), and whether it is modal (a dialog or a sheet: only
/// Escape and its scrim close it, a press elsewhere cannot reach anything else).
pub(super) struct Float {
    pub(super) cell: String,
    pub(super) close: Vec<(String, Value)>,
    pub(super) modal: bool,
}

pub(super) struct Toast {
    pub(super) text: String,
    pub(super) since: f32,
    pub(super) gone: bool,
}

struct Field {
    spec: Spec,
    draft: Draft,
    /// What was last sent to its onChange (the tree's value when it took the keys), and when.
    sent: String,
    last: f32,
    /// When the caret last moved (it shows solid again).
    since: f32,
    /// After a submit, the next value the tree gives that is none of the values this field sent (`echoes`) replaces the
    /// words: the kernel cleared or rewrote the field. Typing again ends it.
    adopt: bool,
    echoes: Vec<String>,
    /// A press on it is held: the unit it selects by and the span it selected, which a drag extends from and keeps whole
    /// ([`Draft::drag_to`]). Held, it keeps the keys until the input pass has given it focus.
    drag: Option<(Unit, (usize, usize))>,
}

struct Slide {
    spec: Spec,
    value: f64,
    sent: Option<f64>,
    last: f32,
}

/// One event to send.
struct Out {
    cell: String,
    handler: String,
    value: Value,
}

impl Out {
    fn new(spec: &Spec, handler: Option<&String>, value: Value) -> Option<Self> {
        Some(Out { cell: spec.cell.clone(), handler: handler?.clone(), value })
    }
}

fn flush(link: &Link, out: Vec<Out>) {
    for o in out {
        link.ui_event(&o.cell, &o.handler, o.value);
    }
}

// ---- Props ----

pub(super) fn describe(v: &Value) -> &'static str {
    match v {
        Value::Null => "missing",
        Value::Bool(_) => "a bool",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// Why control `node` is not drawn: a prop of a kind it cannot be (a handler that is not a handler's id, a toggle's value
/// that is not a bool, a slider's bounds the wrong way round, options that are not a list), named with the element.
pub(super) fn check(node: &Value) -> Option<String> {
    let kind = node["type"].as_str().unwrap_or("");
    let p = &node["props"];
    let bad = |name: &str, want: &str| Some(format!("<{kind}> {name} is {}, not {want}", describe(&p[name])));
    for h in ["onClick", "onChange", "onSubmit"] {
        if !matches!(p[h], Value::Null | Value::String(_)) {
            return bad(h, "a handler");
        }
    }
    if !matches!(p["disabled"], Value::Null | Value::Bool(_)) {
        return bad("disabled", "true or false");
    }
    for name in ["label", "placeholder"] {
        if matches!(p[name], Value::Array(_) | Value::Object(_)) {
            return bad(name, "words");
        }
    }
    let number = |name: &str| match &p[name] {
        Value::Null => Ok(None),
        Value::Number(n) => Ok(n.as_f64()),
        _ => Err(bad(name, "a number")),
    };
    match kind {
        "Button" => match &p["tone"] {
            Value::Null => None,
            Value::String(t) if matches!(t.as_str(), "" | "accent" | "error" | "success" | "warning" | "muted") => None,
            _ => bad("tone", "accent, error, success, warning or muted"),
        },
        "Toggle" | "Checkbox" => {
            let key = if kind == "Toggle" { "value" } else { "checked" };
            (!matches!(p[key], Value::Null | Value::Bool(_))).then(|| bad(key, "true or false")).flatten()
        }
        "Slider" | "NumberField" => {
            let mut got = [None; 4];
            for (i, name) in ["value", "min", "max", "step"].into_iter().enumerate() {
                match number(name) {
                    Ok(v) => got[i] = v,
                    Err(e) => return e,
                }
            }
            let [_, min, max, step] = got;
            let (min, max) = if kind == "Slider" { (min.or(Some(0.0)), max.or(Some(100.0))) } else { (min, max) };
            if let (Some(lo), Some(hi)) = (min, max)
                && !(lo < hi)
            {
                return Some(format!("<{kind}> min {lo} is not below max {hi}"));
            }
            step.is_some_and(|s| s <= 0.0).then(|| bad("step", "a number above 0")).flatten()
        }
        "Input" | "TextArea" => {
            if !matches!(p["value"], Value::Null | Value::String(_) | Value::Number(_)) {
                return bad("value", "words");
            }
            match number("lines") {
                Err(e) => e,
                Ok(Some(n)) if n < 1.0 => bad("lines", "a count of 1 or more"),
                Ok(_) => None,
            }
        }
        "Select" | "Choice" | "Radio" => options(node).err(),
        _ => None,
    }
}

/// The prefab control `node` is drawn from when its props pick one of its type's looks: a button's tone, a toggle on, a
/// box checked, radio dots along a row.
pub(super) fn prefab(kind: &str, node: &Value) -> Option<&'static str> {
    let p = &node["props"];
    Some(match kind {
        "Button" => match p["tone"].as_str() {
            Some("accent") => "hn.GenButtonAccent",
            Some("error") => "hn.GenButtonError",
            Some("success") => "hn.GenButtonSuccess",
            Some("warning") => "hn.GenButtonWarning",
            Some("muted") => "hn.GenButtonMuted",
            _ => return None,
        },
        "Toggle" if p["value"] == true => "hn.GenToggleOn",
        "Checkbox" if p["checked"] == true => "hn.GenCheckboxOn",
        "Radio" if p["row"] == true => "hn.GenRadioRow",
        _ => return None,
    })
}

/// A Select's, Choice's or Radio's options as (label, value), at most [`MAX_OPTIONS`]: `{label, value}` records (a
/// record without a label shows its value) or bare values, each its own label.
fn options(node: &Value) -> Result<Vec<(String, Value)>, String> {
    let kind = node["type"].as_str().unwrap_or("");
    let given = prop(node, "options");
    let Some(all) = given.as_array() else {
        return Err(format!("<{kind}> options is {}, not a list: options={{[{{label: 'A', value: 'a'}}]}}", describe(given)));
    };
    all.iter()
        .take(MAX_OPTIONS)
        .enumerate()
        .map(|(i, o)| match o {
            Value::Object(m) => {
                let value = m.get("value").or_else(|| m.get("label")).filter(|v| !v.is_null());
                let Some(value) = value else {
                    return Err(format!("<{kind}> options[{i}] has no value: {{label: 'A', value: 'a'}}"));
                };
                let label = match m.get("label") {
                    None | Some(Value::Null) => shown(value),
                    Some(l) => shown(l),
                };
                Ok((label, value.clone()))
            }
            Value::Null | Value::Array(_) => Err(format!("<{kind}> options[{i}] is {}, not an option", describe(o))),
            v => Ok((shown(v), v.clone())),
        })
        .collect()
}

fn shown(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// Whether `a` is `b` (an option's value against `selected`): equal JSON, numbers by value (1 and 1.0 are one).
pub(super) fn same(a: &Value, b: &Value) -> bool {
    a == b || matches!((a.as_f64(), b.as_f64()), (Some(x), Some(y)) if x == y)
}

/// The handler `name` names while the control takes input: set, and the control not disabled.
pub(super) fn handler(node: &Value, name: &str) -> Option<String> {
    if prop(node, "disabled") == &Value::Bool(true) {
        return None;
    }
    prop(node, name).as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

pub(super) fn number(node: &Value, name: &str) -> Option<f64> {
    prop(node, name).as_f64().filter(|v| v.is_finite())
}

/// Decimal places that show `x` (up to 6).
fn places(x: f64) -> usize {
    (0..=6).find(|&d| {
        let s = x * 10f64.powi(d as i32);
        (s - s.round()).abs() < 1e-7 * s.abs().max(1.0)
    })
    .unwrap_or(6)
}

/// `x` on the grid of `step` from `min` (from 0 when there is no least), kept within the bounds, rounded to the step's
/// places so 0.1 + 0.2 shows as 0.3.
fn snap(x: f64, min: f64, max: f64, step: f64) -> f64 {
    let base = if min.is_finite() { min } else { 0.0 };
    let v = if step > 0.0 { base + ((x - base) / step).round() * step } else { x };
    let v = v.clamp(min, max);
    let p = 10f64.powi(places(step).max(places(base)) as i32);
    (v * p).round() / p
}

/// `x` bounded and rounded to `step`'s places (a stepper's sign moves the value by a step from where it is, off the grid
/// or not).
fn bounded(x: f64, min: f64, max: f64, step: f64) -> f64 {
    let p = 10f64.powi(places(step).max(places(x).min(6)) as i32);
    ((x.clamp(min, max)) * p).round() / p
}

/// A number as the kernel reads it back: an integer when it is one.
fn num_value(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.0e15 { serde_json::json!(v as i64) } else { serde_json::json!(v) }
}

/// A number as shown: with its step's places, or its own when it has more.
fn fmt_num(v: f64, step: f64) -> String {
    format!("{v:.*}", places(step).max(places(v)))
}

/// A slider's step when it names none: two places below its range's (100 wide steps by 1, 1 wide by 0.01).
fn slider_step(min: f64, max: f64) -> f64 {
    10f64.powi(((max - min).log10().floor() as i32) - 2)
}

// ---- Drawing ----

/// Make `v` pressable with `action`, or not (None): a control that takes no input is never hovered or pressed.
pub(super) fn arm(v: View<'_>, action: Option<&str>) {
    let e = v.entity();
    match action {
        Some(a) => {
            if !e.has(Pressable::id()) {
                e.add(Pressable::id());
            }
            if !e.has(HoverAmt::id()) {
                e.set(HoverAmt::default());
            }
            v.action(a);
        }
        None => {
            if e.has(Pressable::id()) {
                e.remove(Pressable::id());
            }
            if e.has(Action::id()) {
                e.remove(Action::id());
            }
            if e.has(HoverAmt::id()) {
                e.remove(HoverAmt::id());
            }
        }
    }
}

/// Fade `v` (and all in it) as `hn.GenInert` does while it takes no input.
pub(super) fn inert(v: View<'_>, off: bool) {
    let e = v.entity();
    if off {
        let pose = view::prefab(e.world(), "hn.GenInert").and_then(|p| p.try_cloned::<&Pose>()).unwrap_or_default();
        if e.try_cloned::<&Pose>() != Some(pose) {
            e.set(pose);
        }
    } else if e.has(Pose::id()) {
        e.remove(Pose::id());
    }
}

/// A row's gap as its prefab has it, or none when its label is empty (so no label takes no room).
pub(super) fn gap(v: View<'_>, prefab: &str, label: &str) {
    let base = view::prefab(v.entity().world(), prefab).and_then(|p| p.try_cloned::<&Flex>()).unwrap_or_default();
    let want = if label.is_empty() { Flex { gap: 0.0, ..base } } else { base };
    if v.entity().try_cloned::<&Flex>() != Some(want) {
        v.set(want);
    }
}

/// A labelled control's parts in order (genui.flecs `hn.GenLabelled`): its label when it has one, its `body` made from
/// `body`, and `menu` when given. The body.
pub(super) fn labelled<'a>(v: View<'a>, label: &str, body: &str, menu: Option<&str>) -> Option<View<'a>> {
    let mut parts = Vec::with_capacity(3);
    if !label.is_empty() {
        parts.push(("label", "hn.GenLabel"));
    }
    parts.push(("body", body));
    if let Some(m) = menu {
        parts.push(("menu", m));
    }
    view::for_each_of(v, parts.iter().map(|(k, p)| (*k, *p, *k)), |part, k, _| {
        if k == "label" {
            part.text(label);
        }
    });
    v.part("body")
}

/// The Edge `prefab`'s part `part` has.
fn edge_of(world: WorldRef<'_>, prefab: &str, part: &str) -> Option<Edge> {
    view::prefab(world, prefab)?.lookup_child(part)?.try_cloned::<&Edge>()
}

/// Draw control `node` (of `kind`, one of the controls in genui's ELEMENTS) on `v`, and arm it.
/// A Button that is a Popover's or Menu's trigger (`within`) sends the trigger's toggle after its own click, and is not
/// faded for having no onClick of its own.
pub(super) fn control(v: View<'_>, node: &Value, kind: &str, within: &Within, b: &mut Budget<'_, '_>) {
    let cell = b.cell;
    let label = words(node, "label");
    match kind {
        "Button" => {
            if let Some(l) = v.part("label") {
                l.text(&label);
            }
            // Its onClick hears its `value` (null when it has none).
            let h = handler(node, "onClick");
            let mut events: Vec<(String, Value)> = h.iter().map(|h| (h.clone(), prop(node, "value").clone())).collect();
            if let Within::Trigger(t) = within
                && prop(node, "disabled") != &Value::Bool(true)
            {
                events.extend(t.iter().cloned());
            }
            arm(v, (!events.is_empty()).then(|| sends(cell, &events)).as_deref());
            inert(v, events.is_empty());
        }
        "Toggle" | "Checkbox" => {
            let key = if kind == "Toggle" { "value" } else { "checked" };
            let on = prop(node, key) == &Value::Bool(true);
            if let Some(l) = v.part("label") {
                l.text(&label);
            }
            gap(v, if kind == "Toggle" { "hn.GenToggle" } else { "hn.GenCheckbox" }, &label);
            // Beside others in a row, the longer label gives up room first (wrapping), as SwiftUI gives the least
            // flexible view its size first: a short label never truncates for a long neighbour's sake.
            let rank = LayoutPriority { value: -(label.chars().count() as f32) };
            if v.entity().try_cloned::<&LayoutPriority>() != Some(rank) {
                v.set(rank);
            }
            if let Some(t) = v.part("box").and_then(|b| b.part("tick")) {
                t.icon(CHECK);
            }
            let h = handler(node, "onChange");
            arm(v, h.as_deref().map(|h| send(cell, h, Value::Bool(!on))).as_deref());
            inert(v, h.is_none());
        }
        "Radio" | "Choice" => {
            let opts = options(node).unwrap_or_default();
            let sel = prop(node, "selected");
            let h = handler(node, "onChange");
            inert(v, h.is_none());
            let (off, on) = if kind == "Radio" { ("hn.GenRadioItem", "hn.GenRadioItemOn") } else { ("hn.GenChoiceItem", "hn.GenChoiceItemOn") };
            let rows = opts.iter().enumerate().map(|(i, o)| (format!("o{i}"), if same(sel, &o.1) { on } else { off }, o));
            view::for_each_of(v, rows, |item, (text, value), _| {
                if let Some(l) = item.part("label") {
                    l.text(text);
                }
                arm(item, h.as_deref().map(|h| send(cell, h, value.clone())).as_deref());
            });
        }
        "Slider" => slider(v, node, &label, b),
        "NumberField" => {
            let (min, max) = (number(node, "min").unwrap_or(f64::NEG_INFINITY), number(node, "max").unwrap_or(f64::INFINITY));
            let step = number(node, "step").filter(|s| *s > 0.0).unwrap_or(1.0);
            let value = number(node, "value");
            let h = handler(node, "onChange");
            inert(v, h.is_none());
            let Some(body) = labelled(v, &label, "hn.GenNumberBody", None) else { return };
            let at = value.unwrap_or(if min.is_finite() { min.max(0.0f64.min(max)) } else { 0.0f64.min(max) });
            for (part, sign, to) in [("minus", "\u{2212}", bounded(at - step, min, max, step)), ("plus", "+", bounded(at + step, min, max, step))] {
                let Some(s) = body.part(part) else { continue };
                if let Some(t) = s.part("sign") {
                    t.text(sign);
                }
                // At a bound its sign has nowhere to go: faded, and it takes no press.
                let moves = value.is_none_or(|v| to != v);
                arm(s, h.as_deref().filter(|_| moves).map(|h| send(cell, h, num_value(to))).as_deref());
                inert(s, h.is_some() && !moves);
            }
            let spec = Spec { cell: cell.to_owned(), node: v.id(), k: Kind::Number, change: h, submit: None, min, max, step };
            let shown = value.map(|x| fmt_num(x, step)).unwrap_or_default();
            if let Some(bx) = body.part("box") {
                field(bx, body, &spec, shown, &words(node, "placeholder"), "hn.GenNumberBody", b);
            }
        }
        "Input" | "TextArea" => {
            let area = kind == "TextArea";
            let (h, s) = (handler(node, "onChange"), handler(node, "onSubmit"));
            inert(v, h.is_none() && s.is_none());
            let body = if area { "hn.GenTextAreaBox" } else { "hn.GenInputBox" };
            let Some(bx) = labelled(v, &label, body, None) else { return };
            if area {
                // Room for `lines` lines (3 when it says none) before it grows with its words.
                let lines = number(node, "lines").unwrap_or(3.0).min(40.0) as f32;
                let base = view::prefab(v.entity().world(), body).and_then(|p| p.try_cloned::<&Size>()).unwrap_or_default();
                let pad = view::prefab(v.entity().world(), body).and_then(|p| p.try_cloned::<&Padding>()).unwrap_or_default();
                let line = bx.part("text").and_then(|t| t.entity().try_cloned::<&TextStyle>()).map_or(19.0, |s| s.line);
                let want = Size { h: Sizing { min: lines * line + pad.t + pad.b + 2.0, ..base.h }, ..base };
                if bx.entity().try_cloned::<&Size>() != Some(want) {
                    bx.set(want);
                }
            }
            let k = if area { Kind::Area } else { Kind::Input };
            let spec = Spec { cell: cell.to_owned(), node: v.id(), k, change: h, submit: s, min: 0.0, max: 0.0, step: 0.0 };
            let value = match prop(node, "value") {
                Value::Null => String::new(),
                v => shown(v),
            };
            field(bx, bx, &spec, value, &words(node, "placeholder"), body, b);
        }
        "Select" => select(v, node, &label, b),
        _ => {}
    }
}

fn slider(v: View<'_>, node: &Value, label: &str, b: &mut Budget<'_, '_>) {
    let (min, max) = (number(node, "min").unwrap_or(0.0), number(node, "max").unwrap_or(100.0));
    let step = number(node, "step").filter(|s| *s > 0.0).unwrap_or_else(|| slider_step(min, max));
    let h = handler(node, "onChange");
    let spec = Spec { cell: b.cell.to_owned(), node: v.id(), k: Kind::Slider, change: h, submit: None, min, max, step };
    let mut value = number(node, "value").unwrap_or(min).clamp(min, max);
    // Dragged: where the pointer has it, not where the tree last said.
    if let Some(s) = b.live.as_deref_mut().and_then(|l| l.slide.as_mut()).filter(|s| s.spec.node == v.id()) {
        s.spec = spec.clone();
        value = s.value;
    }
    inert(v, spec.change.is_none());
    let Some(body) = labelled(v, label, "hn.GenSliderBody", None) else { return };
    let frac = ((value - min) / (max - min)).clamp(0.0, 1.0) as f32;
    if let Some(rail) = body.part("rail") {
        // A Grow of 0 counts as 1: the empty side keeps a sliver of weight instead.
        for (part, w) in [("fill", frac), ("rest", 1.0 - frac)] {
            let Some(p) = rail.part(part) else { continue };
            let mut s = p.entity().try_cloned::<&Size>().unwrap_or_default();
            let w = w.max(1e-4);
            if (s.w.value - w).abs() > 1e-5 {
                s.w.value = w;
                p.set(s);
            }
        }
        arm(rail, spec.change.is_some().then(|| spec.action()).as_deref());
    }
    if let Some(t) = body.part("value") {
        t.text(&fmt_num(value, step));
    }
}

/// A Split (genui.flecs `hn.GenSplit`, `hn.GenSplitDown` when vertical): its two panes `a` and `b` shared by the ratio
/// (their Grow weights). Its rule always drags: the ratio is the pointer's while it is dragged, then where it was let go
/// ([`Kept`], the split's own state) until the tree says another; an onChange only hears of it.
pub(super) fn split(v: View<'_>, node: &Value, kids: &[&Value], depth: usize, b: &mut Budget<'_, '_>) {
    let h = handler(node, "onChange");
    let spec = Spec { cell: b.cell.to_owned(), node: v.id(), k: Kind::Split, change: h, submit: None, min: 0.05, max: 0.95, step: 0.001 };
    let said = number(node, "ratio").unwrap_or(0.5).clamp(0.05, 0.95);
    let mut ratio = said;
    if let Some(live) = b.live.as_deref_mut() {
        if let Some(s) = live.slide.as_mut().filter(|s| s.spec.node == v.id()) {
            s.spec = spec.clone();
            ratio = s.value;
            live.splits.insert(v.id(), Kept { from: said, at: ratio });
        } else if let Some(k) = live.splits.get(&v.id()) {
            if (k.from - said).abs() < 1e-9 {
                ratio = k.at;
            } else {
                live.splits.remove(&v.id());
            }
        }
    }
    let down = v.entity().try_cloned::<&Flex>().is_some_and(|f| f.axis == harness_native_ui_layout_api::FlexAxis::Column);
    for (part, share) in [("a", ratio as f32), ("b", 1.0 - ratio as f32)] {
        let Some(p) = v.part(part) else { continue };
        let mut s = p.entity().try_cloned::<&Size>().unwrap_or_default();
        let axis = if down { &mut s.h } else { &mut s.w };
        if (axis.value - share).abs() > 1e-5 {
            axis.value = share;
            p.set(s);
        }
    }
    if let Some(rule) = v.part("rule") {
        arm(rule, Some(&spec.action()));
    }
    for (part, kid) in [("a", kids.first()), ("b", kids.get(1))] {
        if let (Some(p), Some(kid)) = (v.part(part), kid) {
            children(p, &[*kid], depth + 1, b);
        }
    }
}

/// The Action of a toast's plate: a press hides it and sends its onDismiss (when it has one).
pub(super) fn toast_action(cell: &str, node: u64, dismiss: Option<String>) -> String {
    Spec { cell: cell.to_owned(), node, k: Kind::Toast, change: dismiss, submit: None, min: 0.0, max: 0.0, step: 0.0 }.action()
}

fn select(v: View<'_>, node: &Value, label: &str, b: &mut Budget<'_, '_>) {
    let cell = b.cell;
    let opts = options(node).unwrap_or_default();
    let sel = prop(node, "selected");
    let h = handler(node, "onChange");
    inert(v, h.is_none());
    let open = h.is_some() && b.live.as_deref().is_some_and(|l| l.open == Some(v.id()));
    let Some(button) = labelled(v, label, "hn.GenSelectButton", open.then_some("hn.GenSelectMenu")) else { return };
    if let Some(t) = button.part("text") {
        match opts.iter().find(|(_, val)| same(sel, val)) {
            Some((text, _)) => toned(t, text, ""),
            None => {
                let hint = words(node, "placeholder");
                toned(t, if hint.is_empty() { "Choose\u{2026}" } else { &hint }, "muted");
            }
        }
    }
    if let Some(c) = button.part("chevron") {
        c.icon(CHEVRON);
    }
    let spec = Spec { cell: cell.to_owned(), node: v.id(), k: Kind::Select, change: h.clone(), submit: None, min: 0.0, max: 0.0, step: 0.0 };
    arm(button, h.is_some().then(|| spec.action()).as_deref());
    if let Some(menu) = v.part("menu") {
        let rows = opts.iter().enumerate().map(|(i, o)| {
            (format!("o{i}"), if same(sel, &o.1) { "hn.GenSelectOptionOn" } else { "hn.GenSelectOption" }, o)
        });
        view::for_each_of(menu, rows, |o, (text, value), _| {
            if let Some(m) = o.part("mark") {
                m.icon(CHECK);
            }
            if let Some(l) = o.part("label") {
                l.text(text);
            }
            arm(o, h.as_deref().map(|h| send(cell, h, value.clone())).as_deref());
        });
    }
}

/// A field's box `bx` (its words in `text`, the placeholder in `hint`) and the node whose hairline shows it has the keys
/// (`edged`, the box or the plate it sits in, made from `prefab`): the tree's `value`, or while it has the keys its own
/// words, caret and selection.
fn field(bx: View<'_>, edged: View<'_>, spec: &Spec, value: String, placeholder: &str, prefab: &str, b: &mut Budget<'_, '_>) {
    let mut shown = value.clone();
    let mut edit = None;
    if let Some(l) = b.live.as_deref_mut() {
        let blink = l.blink;
        if let Some(f) = l.field.as_mut().filter(|f| f.spec.node == spec.node) {
            f.spec = spec.clone();
            if f.adopt && value != f.draft.text && !f.echoes.contains(&value) {
                let n = value.len();
                f.draft = Draft { text: value.clone(), caret: n, anchor: n, ..Draft::default() };
                f.sent = value;
                f.adopt = false;
                f.echoes.clear();
                f.since = l.now;
            }
            let (s, units) = f.draft.shown();
            shown = s;
            edit = Some((units, f.since, blink));
        }
    }
    if let Some(t) = bx.part("text") {
        t.text(&shown);
        let te = t.entity().try_cloned::<&TextEdit>().unwrap_or_default();
        let next = match edit {
            Some(([caret, anchor, mark_lo, mark_hi], since, blink)) => TextEdit { caret, anchor, mark_lo, mark_hi, focused: true, blink, since, ..te },
            None => TextEdit { caret: 0, anchor: 0, mark_lo: 0, mark_hi: 0, focused: false, ..te },
        };
        if next != te {
            t.set(next);
        }
    }
    if let Some(h) = bx.part("hint") {
        h.text(if shown.is_empty() { placeholder } else { "" });
    }
    // One line (an Input, a number's box): while it has the keys its box scrolls (a ScrollState, so layout gives its
    // words their whole length instead of cutting them with an ellipsis) and they slide sideways to keep the caret in
    // view, moved as little as they can from where they were (the box as laid out last frame). As it lets the keys go
    // the box stops scrolling: its words show from their start again, cut to fit. A TextArea grows with its words.
    if spec.k != Kind::Area {
        let e = bx.entity();
        match (edit, bx.part("text")) {
            (Some(([caret, ..], ..)), Some(t)) => {
                let from = e.try_cloned::<&ScrollState>().unwrap_or_default();
                let room = e.try_cloned::<&Laid>().map_or(0.0, |l| {
                    let pad = e.try_cloned::<&Padding>().unwrap_or_default();
                    l.w - pad.l - pad.r
                });
                let style = t.entity().try_cloned::<&TextStyle>().unwrap_or_default();
                let at = (*b.measure)(&shown[..byte_of(&shown, caret as usize)], &style, false);
                let len = (*b.measure)(&shown, &style, false);
                let x = if room > 0.0 { keep_in_view(from.x, at, room, len) } else { 0.0 };
                scroll_quietly(e, ScrollState { x, y: 0.0 });
            }
            _ if e.has(ScrollState::id()) => _ = e.remove(ScrollState::id()),
            _ => {}
        }
    }
    let world = bx.entity().world();
    let rest = view::prefab(world, prefab).and_then(|p| p.try_cloned::<&Edge>());
    let want = if edit.is_some() { edge_of(world, "hn.GenCheckboxOn", "box") } else { rest };
    if let Some(e) = want
        && edged.entity().try_cloned::<&Edge>() != Some(e)
    {
        edged.set(e);
    }
    let takes = spec.change.is_some() || spec.submit.is_some();
    arm(bx, takes.then(|| spec.action()).as_deref());
}

// ---- Using them ----

/// A field's words node: `box.text` of its body (a number's box sits in its body between the signs).
fn text_node<'w>(w: WorldRef<'w>, spec: &Spec) -> Option<View<'w>> {
    let e = w.entity_from_id(spec.node);
    if !e.is_alive() {
        return None;
    }
    let body = View::of(e).part("body")?;
    match spec.k {
        Kind::Number => body.part("box")?.part("text"),
        _ => body.part("text"),
    }
}

impl App {
    /// Whether a cell's field has the keys (keys and typed text go to it, scene/20-keys.flecs ctx.Chat's edits).
    pub(in crate::app) fn genui_editing(&self) -> bool {
        self.genui.borrow().field.is_some()
    }

    /// A press on a control (app.rs `actions`, `ui:` and the rest): send an event as it is, or use a stateful control.
    pub(in crate::app) fn genui_action(&mut self, payload: &str) {
        let parsed: Result<Value, _> = serde_json::from_str(payload);
        match parsed {
            Ok(Value::Array(a)) if a.len() == 3 && a[0].is_string() => {
                let (Some(cell), Some(handler)) = (a[0].as_str(), a[1].as_str()) else {
                    eprintln!("harness-native: genui: a press's event names no cell and handler: {payload}");
                    return;
                };
                self.link.ui_event(cell, handler, a[2].clone());
                self.genui.borrow_mut().open = None;
            }
            // Events sent in order (an Accordion's close-then-open, a Menu's pick-then-close): each [cell, handler,
            // value], checked all before any is sent.
            Ok(Value::Array(all)) if !all.is_empty() && all.iter().all(|e| e.is_array()) => {
                let mut out = Vec::with_capacity(all.len());
                for e in &all {
                    match (e[0].as_str(), e[1].as_str(), e.as_array().map(Vec::len)) {
                        (Some(cell), Some(handler), Some(3)) => out.push(Out { cell: cell.to_owned(), handler: handler.to_owned(), value: e[2].clone() }),
                        _ => {
                            eprintln!("harness-native: genui: a press's events hold one that is not [cell, handler, value]: {payload}");
                            return;
                        }
                    }
                }
                flush(&self.link, out);
                self.genui.borrow_mut().open = None;
            }
            Ok(v @ Value::Object(_)) if v["k"] == "color" => self.genui_color(&v),
            // A press that does more than send (a FileRef's: open the file, then tell its onClick; a context menu's row: pick,
            // then shut the menu here): `{open?, shut?, send}` (genui/media.rs, genui/float.rs `then`).
            Ok(Value::Object(o)) if o.contains_key("send") => {
                let mut out = Vec::new();
                for e in o["send"].as_array().into_iter().flatten() {
                    match (e[0].as_str(), e[1].as_str(), e.as_array().map(Vec::len)) {
                        (Some(cell), Some(handler), Some(3)) => out.push(Out { cell: cell.to_owned(), handler: handler.to_owned(), value: e[2].clone() }),
                        _ => {
                            eprintln!("harness-native: genui: a press's events hold one that is not [cell, handler, value]: {payload}");
                            return;
                        }
                    }
                }
                if let Some(target) = o.get("open").and_then(Value::as_str) {
                    crate::open::open(target, self.press.1.alt);
                }
                if let Some(el) = o.get("shut").and_then(Value::as_u64) {
                    self.genui.borrow_mut().context.remove(&el);
                }
                flush(&self.link, out);
                self.genui.borrow_mut().open = None;
            }
            Ok(v @ Value::Object(_)) => {
                let Some(spec) = Spec::parse(&v) else {
                    eprintln!("harness-native: genui: a control's action is not one: {payload}");
                    return;
                };
                match spec.k {
                    Kind::Select => {
                        let mut live = self.genui.borrow_mut();
                        live.open = if live.open == Some(spec.node) { None } else { Some(spec.node) };
                    }
                    Kind::Toast => {
                        if let Some(t) = self.genui.borrow_mut().toasts.get_mut(&spec.node) {
                            t.gone = true;
                        }
                        flush(&self.link, Out::new(&spec, spec.change.as_ref(), Value::Null).into_iter().collect());
                    }
                    Kind::Slider | Kind::Split | Kind::Scrub => {
                        let at = self.drag_at(&spec);
                        let mut live = self.genui.borrow_mut();
                        let sent = live.slide.take().filter(|s| s.spec.node == spec.node).and_then(|s| s.sent);
                        // A split keeps where it was let go.
                        if let (Some(v), Some(k)) = (at, live.splits.get_mut(&spec.node)) {
                            k.at = v;
                        }
                        // A video seeks to where it was let go.
                        if spec.k == Kind::Scrub
                            && let Some(v) = at
                        {
                            live.surf.scrubbed(spec.node, v);
                        }
                        drop(live);
                        if let Some(v) = at.filter(|v| sent != Some(*v)) {
                            flush(&self.link, Out::new(&spec, spec.change.as_ref(), num_value(v)).into_iter().collect());
                        }
                    }
                    Kind::Input | Kind::Area | Kind::Number => self.field_press(spec),
                }
            }
            _ => eprintln!("harness-native: genui: a press's action is not an event: {payload}"),
        }
        self.poke();
    }

    /// Where the pointer puts dragged control `spec`'s value: a slider's ([`Self::slider_at`]) or a split's ratio (the
    /// rule's middle along the split, less the rule: 0.05 to 0.95, to a thousandth).
    fn drag_at(&self, spec: &Spec) -> Option<f64> {
        match spec.k {
            Kind::Split => {}
            Kind::Scrub => return self.scrub_at(spec),
            _ => return self.slider_at(spec),
        }
        let e = self.world.world_ref().entity_from_id(spec.node);
        if !e.is_alive() {
            return None;
        }
        let v = View::of(e);
        let l = e.try_cloned::<&Laid>()?;
        let down = e.try_cloned::<&Flex>().is_some_and(|f| f.axis == harness_native_ui_layout_api::FlexAxis::Column);
        let rule = v.part("rule").and_then(|r| r.entity().try_cloned::<&Laid>())?;
        let (at, start, len, hit) = if down { (self.pointer.y, l.y, l.h, rule.h) } else { (self.pointer.x, l.x, l.w, rule.w) };
        Some(split_ratio(at, start, len, hit))
    }

    /// Where the pointer puts slider `spec`'s value: along its rail (the knob's middle within it), snapped and bounded.
    fn slider_at(&self, spec: &Spec) -> Option<f64> {
        let w = self.world.world_ref();
        let e = w.entity_from_id(spec.node);
        if !e.is_alive() {
            return None;
        }
        self.rail_at(View::of(e).part("body")?.part("rail")?, spec)
    }

    /// Where the pointer puts video `spec`'s playhead: along its bar's rail, as a slider's.
    fn scrub_at(&self, spec: &Spec) -> Option<f64> {
        let w = self.world.world_ref();
        let e = w.entity_from_id(spec.node);
        if !e.is_alive() {
            return None;
        }
        self.rail_at(View::of(e).part("bar")?.part("plate")?.part("rail")?, spec)
    }

    /// Where the pointer puts `spec`'s value along `rail` (the knob's middle within it), snapped and bounded.
    fn rail_at(&self, rail: View<'_>, spec: &Spec) -> Option<f64> {
        let l = rail.entity().try_cloned::<&Laid>()?;
        let knob = rail.part("knob").and_then(|k| k.entity().try_cloned::<&Laid>()).map_or(16.0, |k| k.w);
        let frac = f64::from(((self.pointer.x - l.x - knob * 0.5) / (l.w - knob).max(1.0)).clamp(0.0, 1.0));
        Some(snap(spec.min + frac * (spec.max - spec.min), spec.min, spec.max, spec.step))
    }

    /// Field `spec` was pressed and let go over itself: it has the keys. It took them at the press, the caret where the
    /// press was ([`Self::genui_pointer_down`]), so a drag's selection stands.
    fn field_press(&self, spec: Spec) {
        self.genui_begin(&spec);
    }

    /// A left press (before the chat's: [`App::chat_pointer_down`]) on a cell's field, the hovered node or one around
    /// it: the field takes the keys now (the input pass focuses it after) and the caret goes where the press was, a
    /// double press selecting its word and a triple its line, held so a drag extends it ([`Self::genui_pointer_drag`]).
    /// Whether the press was on a field.
    pub(in crate::app) fn genui_pointer_down(&mut self, clicks: isize) -> bool {
        let w = self.world.world_ref();
        let Some(h) = w.entity_named(APP).target(Hovered::id(), 0) else { return false };
        let spec = std::iter::successors(Some(h), |n| n.parent()).take(4).find_map(Spec::of);
        let Some(spec) = spec.filter(|s| s.k.typed()) else { return false };
        self.genui_begin(&spec);
        let unit = Unit::of_clicks(clicks);
        let at = self.field_index(&spec);
        let now = self.time;
        if let Some(f) = self.genui.borrow_mut().field.as_mut().filter(|f| f.spec.node == spec.node)
            && f.draft.marked.is_none()
            && let Some(i) = at
        {
            f.drag = Some((unit, f.draft.press(i, unit)));
            f.since = now;
        }
        self.poke();
        true
    }

    /// The pointer moved with a press on a cell's field held: its selection follows, the pressed span kept whole
    /// ([`Draft::drag_to`]). Whether a field's press is held.
    pub(in crate::app) fn genui_pointer_drag(&mut self) -> bool {
        let held = self.genui.borrow().field.as_ref().and_then(|f| Some((f.spec.clone(), f.drag?)));
        let Some((spec, (unit, span))) = held else { return false };
        let at = self.field_index(&spec);
        let now = self.time;
        if let Some(f) = self.genui.borrow_mut().field.as_mut()
            && let Some(i) = at
        {
            f.draft.drag_to(i, unit, span);
            f.since = now;
        }
        self.poke();
        true
    }

    /// The press let go: a field's drag ends. Whether one was held.
    pub(in crate::app) fn genui_pointer_up(&mut self) -> bool {
        let held = self.genui.borrow_mut().field.as_mut().is_some_and(|f| f.drag.take().is_some());
        if held {
            self.poke();
        }
        held
    }

    /// The byte of field `spec`'s words nearest the pointer, on its words as laid out (shifted as its box scrolls them
    /// sideways). None while it does not have the keys or is not laid out.
    fn field_index(&self, spec: &Spec) -> Option<usize> {
        let t = text_node(self.world.world_ref(), spec)?;
        let (lay, l) = words_layout(&mut self.paint.borrow_mut(), t.entity())?;
        let u = lay.index_at(self.pointer.x - l.x, self.pointer.y - l.y);
        let live = self.genui.borrow();
        let f = live.field.as_ref().filter(|f| f.spec.node == spec.node)?;
        Some(byte_of(&f.draft.text, u))
    }

    /// Where the field that has the keys shows its caret (window points, unzoomed: [`words_caret`]), for the input
    /// method's candidate window; Some(None) while its words are not laid out. None when no field has the keys.
    pub(in crate::app) fn genui_caret(&self) -> Option<Option<[f32; 4]>> {
        let spec = self.genui.borrow().field.as_ref()?.spec.clone();
        let t = text_node(self.world.world_ref(), &spec);
        Some(t.and_then(|t| words_caret(&mut self.paint.borrow_mut(), t.entity())))
    }

    /// Field `spec` takes the keys (unless it has them): its words are the tree's as shown. The field that had them lets
    /// them go first, sending its words.
    fn genui_begin(&self, spec: &Spec) {
        let w = self.world.world_ref();
        let mut live = self.genui.borrow_mut();
        if live.field.as_ref().is_some_and(|f| f.spec.node == spec.node) {
            return;
        }
        let out: Vec<Out> = live.field.take().and_then(|f| commit(&f)).into_iter().collect();
        let text = text_node(w, spec).and_then(|t| t.entity().try_cloned::<&Text>()).map(|t| t.s.as_str().to_owned()).unwrap_or_default();
        let n = text.len();
        live.field = Some(Field {
            spec: spec.clone(),
            draft: Draft { text: text.clone(), caret: n, anchor: n, ..Draft::default() },
            sent: text,
            last: f32::NEG_INFINITY,
            since: self.time,
            adopt: false,
            echoes: Vec::new(),
            drag: None,
        });
        drop(live);
        flush(&self.link, out);
    }

    /// Step the controls a frame (before the view is filled): a field takes or lets go of the keys as focus moves (letting
    /// go sends its words), sends what is typed at most every [`TYPE_EVERY`]; a pressed slider follows the pointer and
    /// sends at most every [`SLIDE_EVERY`], and once more when let go; a press anywhere else closes an open select.
    pub(in crate::app) fn step_genui(&mut self) {
        let w = self.world.world_ref();
        let app = w.entity_named(APP);
        let focused = app.target(Focused::id(), 0).and_then(Spec::of).filter(|s| s.k.typed());
        let pressed = app.target(Pressed::id(), 0).and_then(Spec::of).filter(|s| s.k.drags());
        let hovered = app.target(Hovered::id(), 0).map(|e| *e.id());
        let (now, down, went_down) = (self.time, self.pointer.down, self.pointer.went_down);
        let at = pressed.as_ref().filter(|_| down).and_then(|s| self.drag_at(s));
        let mut out = Vec::new();
        let mut busy = false;
        {
            let mut live = self.genui.borrow_mut();
            live.now = now;
            live.blink = self.focused && self.look.themes[usize::from(self.dark)].blink;
            // Focus went elsewhere: the field lets go of the keys and sends its words. Not while a press is new (this
            // frame's input pass moves focus to what it pressed, after this step): a field pressed took the keys at the
            // press (`genui_pointer_down`), before focus reaches it.
            let settled = !went_down;
            if settled && let Some(f) = live.field.take_if(|f| focused.as_ref().is_none_or(|s| s.node != f.spec.node)) {
                out.extend(commit(&f));
            }
            if settled
                && live.field.is_none()
                && let Some(s) = &focused
            {
                drop(live);
                self.genui_begin(s);
                live = self.genui.borrow_mut();
            }
            if let Some(f) = live.field.as_mut().filter(|f| f.spec.k != Kind::Number) {
                if f.draft.text != f.sent && now - f.last >= TYPE_EVERY {
                    out.extend(Out::new(&f.spec, f.spec.change.as_ref(), Value::String(f.draft.text.clone())));
                    f.sent = f.draft.text.clone();
                    f.echoes.push(f.sent.clone());
                    f.last = now;
                }
                busy |= f.draft.text != f.sent;
            }
            // The slider being dragged follows the pointer; let go (off its rail: a release on it is its action), it sends
            // where it stopped.
            match (pressed, at) {
                (Some(spec), Some(v)) => {
                    let s = live.slide.get_or_insert_with(|| Slide { spec: spec.clone(), value: v, sent: None, last: f32::NEG_INFINITY });
                    s.value = v;
                    if s.sent != Some(v) && now - s.last >= SLIDE_EVERY {
                        out.extend(Out::new(&s.spec, s.spec.change.as_ref(), num_value(v)));
                        s.sent = Some(v);
                        s.last = now;
                    }
                    busy = true;
                }
                _ if !down => {
                    if let Some(s) = live.slide.take().filter(|s| s.sent != Some(s.value)) {
                        out.extend(Out::new(&s.spec, s.spec.change.as_ref(), num_value(s.value)));
                    }
                }
                _ => {}
            }
            // A press off an open select (and off its list) closes it.
            if went_down
                && let Some(sel) = live.open
            {
                let inside = hovered.is_some_and(|h| {
                    std::iter::successors(Some(w.entity_from_id(h)), |n| n.parent()).take(8).any(|n| *n.id() == sel)
                });
                if !inside {
                    live.open = None;
                }
            }
            // What floats: gone from the trees are forgotten; a press off a popover or a menu (off its trigger and its
            // plate) closes it.
            prune(w, &mut live);
            if went_down {
                let inside = |el: u64| {
                    hovered.is_some_and(|h| std::iter::successors(Some(w.entity_from_id(h)), |n| n.parent()).take(64).any(|n| *n.id() == el))
                };
                let off: Vec<u64> = live.floats.iter().filter(|(el, f)| !f.modal && !inside(**el)).map(|(el, _)| *el).collect();
                for el in off {
                    live.context.remove(&el);
                    if let Some(f) = live.floats.remove(&el) {
                        out.extend(f.close.into_iter().map(|(handler, value)| Out { cell: f.cell.clone(), handler, value }));
                    }
                }
            }
            out.extend(live.pending.drain(..).map(|(cell, handler, value)| Out { cell, handler, value }));
            busy |= std::mem::take(&mut live.anim);
            busy |= live.field.is_some() && live.blink;
        }
        flush(&self.link, out);
        if busy {
            self.poke_why("genui control");
        }
    }

    /// A right press at the pointer: the context menu whose region it is in (the innermost) opens there, telling its
    /// onToggle. Whether one did.
    pub(in crate::app) fn genui_context(&mut self) -> bool {
        let w = self.world.world_ref();
        let Some(h) = w.entity_named(APP).target(Hovered::id(), 0) else { return false };
        let (x, y) = (self.pointer.x, self.pointer.y);
        let out = {
            let mut live = self.genui.borrow_mut();
            prune(w, &mut live);
            let found = std::iter::successors(Some(h), |n| n.parent()).take(64).find_map(|n| live.regions.get(&*n.id()).map(|r| (n, r.clone())));
            let Some((region, r)) = found else { return false };
            let at = region.try_cloned::<&Laid>().unwrap_or_default();
            live.context.insert(r.menu, [x - at.x, y - at.y]);
            r.toggle.map(|handler| Out { cell: r.cell, handler, value: Value::Bool(true) })
        };
        flush(&self.link, out.into_iter().collect());
        self.poke();
        true
    }

    /// Escape over a cell's floats: the open popovers and menus close (each sends its onToggle(false)); with none open,
    /// the dialogs and sheets that may be dismissed do (onClose(false)). Whether any closed.
    pub(in crate::app) fn genui_escape(&mut self) -> bool {
        self.genui_shut(|live| {
            let light = light_floats(live);
            if light.is_empty() { live.floats.iter().filter(|(_, f)| !f.close.is_empty()).map(|(el, _)| *el).collect() } else { light }
        })
    }

    /// The window lost the keyboard (floats.rs `close_floats`): a cell's open popovers and menus close, as NSPopover and
    /// NSMenu do (each sends its onToggle(false)); its dialogs and sheets stay. Whether any closed.
    pub(in crate::app) fn genui_blur(&mut self) -> bool {
        self.genui_shut(light_floats)
    }

    /// Close the floats `pick` names (by their element), telling each cell through its close handlers. Whether any closed.
    fn genui_shut(&mut self, pick: impl FnOnce(&Live) -> Vec<u64>) -> bool {
        let w = self.world.world_ref();
        let out: Vec<Out> = {
            let mut live = self.genui.borrow_mut();
            prune(w, &mut live);
            let shut = pick(&live);
            // A context menu closes here whether or not it has an onToggle to tell.
            let mut any = false;
            for el in &shut {
                any |= live.context.remove(el).is_some();
            }
            let out: Vec<Out> = shut
                .into_iter()
                .filter_map(|el| live.floats.remove(&el))
                .flat_map(|f| f.close.into_iter().map(move |(handler, value)| Out { cell: f.cell.clone(), handler, value }))
                .collect();
            if out.is_empty() && !any {
                return false;
            }
            out
        };
        flush(&self.link, out);
        self.poke();
        true
    }

    /// The wheel over a cell's Scroll (genui.flecs `hn.GenScroll`, the hovered node or one around it): it moves while it
    /// can (down or across, as the Scroll goes) and keeps the wheel; at its end the wheel goes on to the transcript.
    pub(in crate::app) fn genui_wheel(&mut self, dx: f32, dy: f32) -> bool {
        use harness_native_ui_layout_api::{ScrollExtent, ScrollState};
        let w = self.world.world_ref();
        let Some(h) = w.entity_named(APP).target(Hovered::id(), 0) else { return false };
        let (down, across) = (view::prefab(w, "hn.GenScroll"), view::prefab(w, "hn.GenScrollAcross"));
        let is = |e: EntityView<'_>, p: Option<EntityView<'_>>| p.is_some_and(|p| e.owns((flecs::IsA::ID, *p.id())));
        for e in std::iter::successors(Some(h), |n| n.parent()).take(64) {
            let sideways = is(e, across);
            if !sideways && !is(e, down) {
                continue;
            }
            let (Some(l), Some(x), st) = (e.try_cloned::<&Laid>(), e.try_cloned::<&ScrollExtent>(), e.try_cloned::<&ScrollState>().unwrap_or_default()) else {
                return false;
            };
            let next = if sideways {
                let by = if dx.abs() > dy.abs() { dx } else { dy };
                ScrollState { x: (st.x - by).clamp(0.0, (x.w - l.w).max(0.0)), ..st }
            } else {
                ScrollState { y: (st.y - dy).clamp(0.0, (x.h - l.h).max(0.0)), ..st }
            };
            if next == st {
                return false;
            }
            e.set(next);
            self.poke();
            return true;
        }
        false
    }

    /// Run `f` on the words of the field that has the keys; whether one has. A one-line field keeps to one line.
    pub(in crate::app) fn genui_draft(&mut self, f: impl FnOnce(&mut Draft)) -> bool {
        let now = self.time;
        {
            let mut live = self.genui.borrow_mut();
            let Some(field) = live.field.as_mut() else { return false };
            f(&mut field.draft);
            if field.spec.k != Kind::Area && field.draft.text.contains('\n') {
                // Same length, so the caret stays put.
                field.draft.text = field.draft.text.replace('\n', " ");
            }
            field.since = now;
            field.adopt = false;
        }
        self.poke();
        true
    }

    /// An `act.Edit` (scene/20-keys.flecs) on the field that has the keys: the chat input's moves and erases, and Return
    /// (`send`: a new line in a TextArea, else submit), Cmd+Return (`submit`), Escape (`interrupt`: let go of the keys).
    pub(in crate::app) fn genui_edit(&mut self, op: &str) {
        let Some(k) = self.genui.borrow().field.as_ref().map(|f| f.spec.k) else { return };
        let (select, name) = match op.strip_prefix("select_") {
            Some(n) => (true, n),
            None => (false, op),
        };
        if let Some(m) = Move::named(name) {
            if let Some(to) = self.field_target(m) {
                self.genui_draft(|d| d.go(m, to, select));
            }
            return;
        }
        if let Some(m) = Move::erasing(op) {
            if let Some(to) = self.field_target(m) {
                self.genui_draft(|d| d.erase(to));
            }
            return;
        }
        match op {
            "newline" if k == Kind::Area => _ = self.genui_draft(|d| d.insert("\n")),
            "send" if k == Kind::Area => _ = self.genui_draft(|d| d.insert("\n")),
            "newline" => {}
            "send" | "submit" => self.genui_submit(),
            "interrupt" => {
                // Let go of the keys; the next step sends the words.
                self.world.lookup(APP).remove((Focused::id(), flecs::Wildcard::ID));
            }
            _ => eprintln!("harness-native: keys: act.Edit has no edit {op:?} for a cell's field"),
        }
        self.poke();
    }

    /// Submit the field that has the keys: a number is sent to onChange; words go to onChange (when they changed) and
    /// then onSubmit. It keeps the keys, and takes the next value the kernel gives (a cleared field after a send).
    fn genui_submit(&mut self) {
        let now = self.time;
        let out = {
            let mut live = self.genui.borrow_mut();
            let Some(f) = live.field.as_mut() else { return };
            let mut out: Vec<Out> = commit(f).into_iter().collect();
            if f.spec.k == Kind::Number {
                // The number as sent (bounded), or the tree's again when it was not one.
                let text = match parse_num(&f.draft.text, &f.spec) {
                    Some(v) => fmt_num(v, f.spec.step),
                    None => f.sent.clone(),
                };
                let n = text.len();
                f.draft = Draft { text: text.clone(), caret: n, anchor: n, ..Draft::default() };
                f.sent = text;
            } else {
                f.echoes.push(f.draft.text.clone());
                f.sent = f.draft.text.clone();
                out.extend(Out::new(&f.spec, f.spec.submit.as_ref(), Value::String(f.draft.text.clone())));
            }
            f.last = now;
            f.since = now;
            f.adopt = true;
            out
        };
        flush(&self.link, out);
        self.poke();
    }

    /// Where `m` takes the field's caret: along its words, or along its lines as laid out.
    fn field_target(&mut self, m: Move) -> Option<usize> {
        let spec = {
            let live = self.genui.borrow();
            let f = live.field.as_ref()?;
            if let Some(to) = f.draft.target(m) {
                return Some(to);
            }
            f.spec.clone()
        };
        let w = self.world.world_ref();
        let t = text_node(w, &spec)?;
        let (lay, _) = words_layout(&mut self.paint.borrow_mut(), t.entity())?;
        let mut live = self.genui.borrow_mut();
        let f = live.field.as_mut()?;
        along_lines(&mut f.draft, &lay, m)
    }

    /// What the field that has the keys has selected (Command-C), and its words node.
    pub(in crate::app) fn genui_copy(&self) -> Option<(String, u64)> {
        let live = self.genui.borrow();
        let f = live.field.as_ref()?;
        let t = text_node(self.world.world_ref(), &f.spec)?;
        Some((f.draft.selected().to_owned(), t.id()))
    }

    /// The field's input-method text, when a field has the keys.
    pub(in crate::app) fn genui_marked(&self) -> Option<Option<String>> {
        self.genui.borrow().field.as_ref().map(|f| f.draft.marked.clone())
    }
}

/// A split's ratio with the pointer `at` along it (from `start`, `len` long, its rule `hit` wide): the rule's middle under
/// the pointer, as a share of the room less the rule, kept within 0.05 and 0.95 and rounded to a thousandth.
fn split_ratio(at: f32, start: f32, len: f32, hit: f32) -> f64 {
    let frac = f64::from((at - start - hit * 0.5) / (len - hit).max(1.0));
    (frac.clamp(0.05, 0.95) * 1000.0).round() / 1000.0
}

/// The open floats that close lightly (popovers and menus, not modal dialogs or sheets), by their element.
fn light_floats(live: &Live) -> Vec<u64> {
    live.floats.iter().filter(|(_, f)| !f.modal).map(|(el, _)| *el).collect() // alloc: the few open floats, once per close
}

/// Forget the floats and toasts whose elements are gone from the trees (or closed: no plate or scrim now).
fn prune(w: WorldRef<'_>, live: &mut Live) {
    live.floats.retain(|el, _| {
        let e = w.entity_from_id(*el);
        e.is_alive() && (View::of(e).part("float").is_some() || View::of(e).part("scrim").is_some())
    });
    live.toasts.retain(|el, _| w.entity_from_id(*el).is_alive());
    live.splits.retain(|el, _| w.entity_from_id(*el).is_alive());
    live.hovering.retain(|el, _| w.entity_from_id(*el).is_alive());
    live.context.retain(|el, _| w.entity_from_id(*el).is_alive());
    live.born.retain(|el, _| w.entity_from_id(*el).is_alive());
    live.regions.retain(|el, _| w.entity_from_id(*el).is_alive());
}

/// A number typed into `spec`'s field, bounded; None when it is not one.
fn parse_num(s: &str, spec: &Spec) -> Option<f64> {
    let v: f64 = s.trim().replace('\u{2212}', "-").parse().ok().filter(|v: &f64| v.is_finite())?;
    Some(bounded(v, spec.min, spec.max, spec.step))
}

/// What a field sends as it lets go of the keys (or submits): its words to onChange when they changed since last sent; a
/// number, typed and bounded, when it is one.
fn commit(f: &Field) -> Option<Out> {
    if f.spec.k == Kind::Number {
        let v = parse_num(&f.draft.text, &f.spec)?;
        let was = parse_num(&f.sent, &f.spec);
        return (was != Some(v)).then(|| Out::new(&f.spec, f.spec.change.as_ref(), num_value(v))).flatten();
    }
    (f.draft.text != f.sent).then(|| Out::new(&f.spec, f.spec.change.as_ref(), Value::String(f.draft.text.clone()))).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_snap_and_show() {
        assert_eq!(snap(0.30000000000000004, 0.0, 1.0, 0.1), 0.3);
        assert_eq!(snap(47.6, 0.0, 100.0, 5.0), 50.0);
        assert_eq!(snap(120.0, 0.0, 100.0, 1.0), 100.0);
        assert_eq!(fmt_num(0.5, 0.1), "0.5");
        assert_eq!(fmt_num(3.0, 1.0), "3");
        assert_eq!(num_value(3.0), serde_json::json!(3));
        assert_eq!(slider_step(0.0, 1.0), 0.01);
        assert!(same(&serde_json::json!(1), &serde_json::json!(1.0)));
    }

    /// A split's rule dragged to the middle of a 413-wide split (a 13-wide rule) is half; past its ends it stops at 0.05
    /// and 0.95; a ratio is to a thousandth.
    #[test]
    fn a_dragged_split_rule_gives_its_ratio() {
        assert_eq!(split_ratio(206.5, 0.0, 413.0, 13.0), 0.5);
        assert_eq!(split_ratio(-50.0, 0.0, 413.0, 13.0), 0.05);
        assert_eq!(split_ratio(900.0, 0.0, 413.0, 13.0), 0.95);
        assert_eq!(split_ratio(100.0 + 6.5 + 160.0, 100.0, 413.0, 13.0), 0.4);
        assert_eq!(split_ratio(123.4567, 0.0, 413.0, 13.0), 0.292);
    }
}
