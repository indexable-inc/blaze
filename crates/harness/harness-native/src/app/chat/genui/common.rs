//! The props every element takes (den's universal props): its size and padding over its prefab's, a pose it glides to
//! (`x`, `y`, `scale`, `opacity`), what is over what (`z`), a glide to where it moved (`flip`), how it comes in and goes out
//! (`enter`, `exit`), its pose and inks under the pointer (`hover`), and the pointer, key and focus handlers. [`apply`] runs
//! once at the end of genui.rs `element()` and says all of it with the general prims (Size, Padding, Z, motion's
//! `Posed`/`Posing`/`Flip`/`HoverPose`, look's `HoverInk`); the handlers' events are sent by the `impl App` below, which the
//! chat's pointer, wheel, key and frame paths call. After den's den-native/src/genui.rs `Traits` and ui/genui_prims.rs
//! (`rest_of`, `from_of`, `track_pose`, `tune_of`, `delta_of`, the press, drag, wheel and key prims, `point_value`).
use super::*;
use harness_native_api::HoverAmt;
use harness_native_ui_input_api::{Hoverable, Tooltip};
use harness_native_ui_layout_api::{FlexAxis, Padding, SizeKind};
use harness_native_ui_look_api::HoverInk;
use harness_native_ui_motion_api::{Flip, HoverPose, PoseAxis, Posed, Posing};
use harness_native_ui_node_api::Z;

/// The props every element takes, besides its own (props.rs `check` lets them through on every element).
pub(super) const COMMON: [&str; 29] = [
    "tip", "width", "height", "minWidth", "maxWidth", "minHeight", "maxHeight", "grow", "padX", "padY", "x", "y", "scale", "rotate", "opacity",
    "z", "flip", "enter", "exit", "hover", "onPointerdown", "onPointermove", "onPointerup", "onDrag", "onWheel", "onHover",
    "onKeydown", "onFocus", "onBlur",
];

/// The pointer, key and focus handlers, wire name and the key [`GenOn`] keeps it under.
const HANDLERS: [(&str, &str); 9] = [
    ("onPointerdown", "down"),
    ("onPointermove", "moved"),
    ("onPointerup", "up"),
    ("onDrag", "drag"),
    ("onWheel", "wheel"),
    ("onHover", "hover"),
    ("onKeydown", "key"),
    ("onFocus", "focus"),
    ("onBlur", "blur"),
];

/// The size props: an element that reads `width`/`height` as its own keeps that meaning ([`OWN_SIZE`]).
const SIZE_PROPS: [&str; 7] = ["width", "height", "minWidth", "maxWidth", "minHeight", "maxHeight", "grow"];
const OWN_SIZE: [&str; 4] = ["Image", "Chart", "Scroll", "Canvas"];

/// The transitions `enter` and `exit` name.
const KINDS: [&str; 4] = ["fly", "fade", "scale", "slide"];

/// How the universal props feel (`hn.GenUniversal`, the "genui universal props" tokens): an enter or exit lasting `ms` springs
/// on omega `settle_rate * 1000 / ms` at damping `settle_zeta`; a press is a drag past `drag_slop` points; a Canvas shape's
/// or Group's pose springs on `shape_omega`, `shape_zeta`.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct GenFeel {
    pub settle_rate: f32,
    pub settle_zeta: f32,
    pub drag_slop: f32,
    pub shape_omega: f32,
    pub shape_zeta: f32,
}

/// The scene's [`GenFeel`]. The scene always has one (scenes_load checks it loads).
pub(super) fn feel(world: WorldRef<'_>) -> GenFeel {
    view::prefab(world, "hn.GenUniversal")
        .and_then(|p| p.try_cloned::<&GenFeel>())
        .unwrap_or_else(|| panic!("the scene has no hn.GenUniversal with a GenFeel (widgets/genui.flecs)"))
}

/// An element's pointer, key and focus handlers and the cell they go to, as JSON (`{"cell": .., "down": "h3", ..}`).
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct GenOn {
    pub spec: ScriptString,
}

/// What the common pass last did to an element: the Size and Padding its own code gave it (`*_base`) and what this pass
/// wrote over them (`*_wrote`), so a prop taken away gives the element its own back; whether it wrote its Z; whether its
/// exit was playing.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
struct Common {
    sized: bool,
    size_base: Size,
    size_wrote: Size,
    padded: bool,
    pad_base: Padding,
    pad_wrote: Padding,
    z: bool,
    exiting: bool,
    tip: bool,
}

/// An `enter` or `exit`, every field filled (the kernel fills them; these are Svelte's defaults for a tree that did not).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Trans {
    kind: &'static str,
    x: f32,
    y: f32,
    start: f32,
    opacity: f32,
    axis: PoseAxis,
    ms: f32,
    delay: f32,
}

fn num(v: &Value) -> Option<f32> {
    v.as_f64().filter(|x| x.is_finite()).map(|x| x as f32)
}

fn trans(v: &Value) -> Option<Trans> {
    let kind = KINDS.iter().find(|k| v["kind"].as_str() == Some(**k))?;
    let n = |k: &str, d: f32| num(&v[k]).unwrap_or(d);
    Some(Trans {
        kind,
        x: n("x", 0.0),
        y: n("y", 0.0),
        start: n("start", 0.0),
        opacity: n("opacity", 0.0).clamp(0.0, 1.0),
        axis: if v["axis"].as_str() == Some("x") { PoseAxis::X } else { PoseAxis::Y },
        ms: n("ms", 400.0).max(0.0),
        delay: n("delay", 0.0).max(0.0),
    })
}

/// Where transition `t` has an element that rests at `rest` at its far end (den's `from_of`): flown in from (x, y), grown
/// from `start`, unfolded from nothing along its axis, and faded from `opacity` (but a slide, which only unfolds).
fn from_of(t: Trans, rest: Posed) -> Posed {
    match t.kind {
        "fly" => Posed { x: rest.x + t.x, y: rest.y + t.y, opacity: rest.opacity * t.opacity, ..rest },
        "scale" => Posed { scale: rest.scale * t.start, opacity: rest.opacity * t.opacity, ..rest },
        "slide" => Posed { share: 0.0, axis: t.axis, ..rest },
        _ => Posed { opacity: rest.opacity * t.opacity, ..rest },
    }
}

/// The ink tone `name` names (`hn.GenTone.<name>`), for a hover's fill, stroke and tone.
pub(super) fn tone(world: WorldRef<'_>, name: &str) -> Option<Fill> {
    view::prefab(world, &format!("hn.GenTone.{name}")).and_then(|p| p.try_cloned::<&Fill>())
}

/// Why `node`'s common props cannot be drawn, named for the agent: one of the wrong type, a transition of no kind, a hover
/// ink that is no tone.
pub(super) fn check(world: WorldRef<'_>, node: &Value) -> Option<String> {
    let kind = node["type"].as_str().unwrap_or("");
    let p = &node["props"];
    let bad = |name: &str, want: &str| Some(format!("<{kind}> {name} is {}, not {want}", control::describe(&p[name])));
    let own = OWN_SIZE.contains(&kind);
    for k in ["width", "height", "minWidth", "maxWidth", "minHeight", "maxHeight", "padX", "padY", "x", "y", "scale", "rotate", "opacity", "z"] {
        if own && (k == "width" || k == "height") {
            continue;
        }
        if !matches!(p[k], Value::Null | Value::Number(_)) {
            return bad(k, "a number");
        }
    }
    for k in ["grow", "flip"] {
        if !matches!(p[k], Value::Null | Value::Bool(_)) {
            return bad(k, "true or false");
        }
    }
    if !matches!(p["tip"], Value::Null | Value::String(_)) {
        return bad("tip", "words");
    }
    for (h, _) in HANDLERS {
        if !matches!(p[h], Value::Null | Value::String(_)) {
            return bad(h, "a handler");
        }
    }
    for k in ["enter", "exit"] {
        match &p[k] {
            Value::Null => {}
            Value::Object(_) if trans(&p[k]).is_some() => {}
            Value::Object(_) => return bad(k, "a transition: its kind is fly, fade, scale or slide"),
            _ => return bad(k, "a transition {kind, x, y, start, opacity, axis, ms, delay}"),
        }
    }
    match &p["hover"] {
        Value::Null => {}
        Value::Object(h) => {
            for (k, v) in h {
                let ok = match k.as_str() {
                    "x" | "y" | "scale" | "rotate" | "opacity" => matches!(v, Value::Number(_)),
                    "fill" | "stroke" | "tone" => v.as_str().is_some_and(|t| tone(world, t).is_some()),
                    _ => return Some(format!("<{kind}> hover has no \u{201C}{}\u{201D}: it takes x, y, scale, rotate, opacity, fill, stroke, tone", k.chars().take(32).collect::<String>())),
                };
                if !ok {
                    return Some(format!("<{kind}> hover {k} is {}, not {}", control::describe(v), if matches!(k.as_str(), "fill" | "stroke" | "tone") { "a tone (ink muted strong error accent success warning, faint, series1 to series8)" } else { "a number" }));
                }
            }
        }
        _ => return bad("hover", "a record {x, y, scale, rotate, opacity, fill, stroke, tone}"),
    }
    None
}

/// Say `node`'s common props on its element `v` (of `kind`), over what its own code set.
pub(super) fn apply(v: View<'_>, node: &Value, kind: &str, b: &mut Budget<'_, '_>) {
    let e = v.entity();
    let world = e.world();
    let p = &node["props"];
    let mut rec = e.try_cloned::<&Common>().unwrap_or_default();
    let before = rec;

    // Size: the given axes, over what the element's own code (or its prefab) says. Grow is along its parent's flow.
    let sized = !OWN_SIZE.contains(&kind) && !(kind == "Spacer" && !p["size"].is_null()) && SIZE_PROPS.iter().any(|k| !p[*k].is_null());
    let cur = e.try_cloned::<&Size>();
    if sized {
        let base = match cur {
            Some(c) if rec.sized && c == rec.size_wrote => rec.size_base,
            c => c.unwrap_or_default(),
        };
        let mut want = base;
        let row = e.parent().and_then(|p| p.try_cloned::<&Flex>()).is_some_and(|f| f.axis == FlexAxis::Row);
        if p["grow"].as_bool() == Some(true) {
            let s = if row { &mut want.w } else { &mut want.h };
            *s = Sizing { kind: SizeKind::Grow, value: 1.0, ..*s };
        }
        for (k, s) in [("width", &mut want.w), ("height", &mut want.h)] {
            if let Some(n) = num(&p[k]) {
                *s = Sizing { kind: SizeKind::Fixed, value: n.max(0.0), ..*s };
            }
        }
        for (k, s) in [("minWidth", &mut want.w), ("minHeight", &mut want.h)] {
            if let Some(n) = num(&p[k]) {
                s.min = n.max(0.0);
            }
        }
        for (k, s) in [("maxWidth", &mut want.w), ("maxHeight", &mut want.h)] {
            if let Some(n) = num(&p[k]) {
                s.max = n.max(0.0);
            }
        }
        (rec.sized, rec.size_base, rec.size_wrote) = (true, base, want);
        if cur != Some(want) {
            e.set(want);
        }
    } else if rec.sized {
        if cur == Some(rec.size_wrote) {
            e.set(rec.size_base);
        }
        rec.sized = false;
    }

    // Padding: padX on the left and right, padY on the top and bottom.
    let padded = !p["padX"].is_null() || !p["padY"].is_null();
    let cur = e.try_cloned::<&Padding>();
    if padded {
        let base = match cur {
            Some(c) if rec.padded && c == rec.pad_wrote => rec.pad_base,
            c => c.unwrap_or_default(),
        };
        let mut want = base;
        if let Some(n) = num(&p["padX"]) {
            (want.l, want.r) = (n.max(0.0), n.max(0.0));
        }
        if let Some(n) = num(&p["padY"]) {
            (want.t, want.b) = (n.max(0.0), n.max(0.0));
        }
        (rec.padded, rec.pad_base, rec.pad_wrote) = (true, base, want);
        if cur != Some(want) {
            e.set(want);
        }
    } else if rec.padded {
        if cur == Some(rec.pad_wrote) {
            e.set(rec.pad_base);
        }
        rec.padded = false;
    }

    // What is over what among its siblings.
    match num(&p["z"]) {
        Some(z) => {
            if e.try_cloned::<&Z>() != Some(Z { z }) {
                e.set(Z { z });
            }
            rec.z = true;
        }
        None if rec.z => {
            e.remove(Z::id());
            rec.z = false;
        }
        None => {}
    }

    // Its pose, on springs (den's track_pose): it rests at its x, y, scale and opacity (a Layer's x and y are its offset,
    // laid out by layout.rs); an exit playing sends it to the exit's far end; it springs on the transition's ms, else the
    // pose spring. Seen for the first time, it starts at its enter's far end, held there for the enter's delay.
    let layer = kind == "Layer";
    let at = |k: &str| if layer { None } else { num(&p[k]) };
    let rest = Posed {
        x: at("x").unwrap_or(0.0),
        y: at("y").unwrap_or(0.0),
        scale: num(&p["scale"]).map_or(1.0, |s| s.clamp(0.0, 8.0)),
        opacity: num(&p["opacity"]).map_or(1.0, |o| o.clamp(0.0, 1.0)),
        ..Posed::REST
    };
    let (enter, exit) = (trans(&p["enter"]), trans(&p["exit"]));
    let hover = &p["hover"];
    let flip = p["flip"].as_bool() == Some(true);
    let mut hp = HoverPose::default();
    for (k, bit) in [("x", HoverPose::X), ("y", HoverPose::Y), ("scale", HoverPose::SCALE), ("opacity", HoverPose::OPACITY)] {
        if let Some(n) = num(&hover[k]) {
            hp.set |= bit;
            match k {
                "x" => hp.x = n,
                "y" => hp.y = n,
                "scale" => hp.scale = n.clamp(0.0, 8.0),
                _ => hp.opacity = n.clamp(0.0, 1.0),
            }
        }
    }
    if rest != Posed::REST || enter.is_some() || exit.is_some() || hp.set != 0 || flip {
        let to = exit.map_or(rest, |x| from_of(x, rest));
        let feel = feel(world);
        let (omega, zeta) = exit.or(enter).filter(|t| t.ms > 0.0).map_or((0.0, 0.0), |t| (feel.settle_rate * 1000.0 / t.ms, feel.settle_zeta));
        let axis = exit.or(enter).map_or(PoseAxis::Y, |t| t.axis);
        let target = Posed { omega, zeta, axis, ..to };
        if e.try_cloned::<&Posed>() != Some(target) {
            e.set(target);
        }
        match e.try_cloned::<&Posing>() {
            None => {
                let start = match enter {
                    Some(t) if b.playing && exit.is_none() => from_of(t, rest),
                    _ => to,
                };
                let hold = b.now + enter.map_or(0.0, |t| t.delay / 1000.0);
                e.set(Posing::at(start, hold));
            }
            // An exit's delay holds its start, as an enter's does.
            Some(mut s) if !rec.exiting && exit.is_some_and(|t| t.delay > 0.0) => {
                s.hold = b.now + exit.map_or(0.0, |t| t.delay / 1000.0);
                e.set(s);
            }
            Some(_) => {}
        }
    } else if e.has(Posed::id()) {
        e.remove(Posed::id());
        e.remove(Posing::id());
    }
    rec.exiting = exit.is_some();
    match (flip, e.has(Flip::id())) {
        (true, false) => drop(e.set(Flip::default())),
        (false, true) => drop(e.remove(Flip::id())),
        _ => {}
    }
    if hp.set != 0 {
        if e.try_cloned::<&HoverPose>() != Some(hp) {
            e.set(hp);
        }
    } else if e.has(HoverPose::id()) {
        e.remove(HoverPose::id());
    }

    hover_ink(e, hover);
    handlers(e, p, b.cell);
    // Words shown while the pointer rests on it (a Canvas says its own, canvas.rs).
    let tip = p["tip"].as_str().filter(|t| !t.is_empty() && kind != "Canvas");
    match tip {
        Some(t) => {
            let want = Tooltip { text: t.into() };
            if e.try_cloned::<&Tooltip>().as_ref() != Some(&want) {
                e.set(want);
            }
            if !e.has(Hoverable::id()) {
                e.add(Hoverable::id());
            }
            rec.tip = true;
        }
        None if rec.tip => {
            e.remove(Tooltip::id());
            rec.tip = false;
        }
        None => {}
    }
    if rec != before {
        e.set(rec);
    }
}

/// Element (or Canvas shape) `e`'s inks under the pointer, from its `hover`: its fill (or, on words, its tone) and its
/// outline, toward those tones; it takes the pointer and gets a hover spring of its own.
pub(super) fn hover_ink(e: EntityView<'_>, hover: &Value) {
    let world = e.world();
    let ink_of = |k: &str| hover[k].as_str().and_then(|t| tone(world, t));
    let mut ink = HoverInk::default();
    if let Some(f) = ink_of("fill").or_else(|| ink_of("tone")) {
        (ink.fill, ink.set) = (f, ink.set | HoverInk::FILL);
    }
    if let Some(f) = ink_of("stroke") {
        (ink.stroke, ink.set) = (f, ink.set | HoverInk::STROKE);
    }
    if ink.set != 0 {
        if e.try_cloned::<&HoverInk>() != Some(ink) {
            e.set(ink);
        }
    } else if e.has(HoverInk::id()) {
        e.remove(HoverInk::id());
    }
    if !hover.is_null() {
        if !e.has(Hoverable::id()) {
            e.add(Hoverable::id());
        }
        if !e.has(HoverAmt::id()) {
            e.set(HoverAmt::default());
        }
    }
}

/// Element (or Canvas shape) `e`'s pointer, key and focus handlers from props `p`, for the paths in the `impl App` below,
/// their events going to `cell`. One with any is a hit target.
pub(super) fn handlers(e: EntityView<'_>, p: &Value, cell: &str) {
    let mut on = serde_json::Map::new();
    for (wire, key) in HANDLERS {
        if let Some(h) = p[wire].as_str() {
            on.insert(key.to_owned(), Value::String(h.to_owned())); // alloc: the handler ids, one small map per element
        }
    }
    if on.is_empty() {
        if e.has(GenOn::id()) {
            e.remove(GenOn::id());
        }
        return;
    }
    on.insert("cell".to_owned(), Value::String(cell.to_owned())); // alloc: the cell the events go to
    let want = GenOn { spec: Value::Object(on).to_string().as_str().into() };
    if e.try_cloned::<&GenOn>().as_ref() != Some(&want) {
        e.set(want);
    }
    if !e.has(Hoverable::id()) {
        e.add(Hoverable::id());
    }
}

/// An element's handlers: which of [`HANDLERS`] it has and the cell its events go to.
struct On {
    cell: String,
    by: serde_json::Map<String, Value>,
}

impl On {
    fn of(e: EntityView<'_>) -> Option<Self> {
        let g = e.try_cloned::<&GenOn>()?;
        let Ok(Value::Object(mut by)) = serde_json::from_str::<Value>(g.spec.as_str()) else { return None };
        let cell = by.remove("cell")?.as_str()?.to_owned();
        Some(Self { cell, by })
    }

    fn has(&self, key: &str) -> bool {
        self.by.contains_key(key)
    }

    fn pointer(&self) -> bool {
        ["down", "up", "drag"].iter().any(|k| self.has(k))
    }

    fn keys(&self) -> bool {
        ["key", "focus", "blur"].iter().any(|k| self.has(k))
    }
}

/// A press held on an element with handlers: where it went down and where it is, its element's rectangle then, whether it
/// has moved past the drag slop, and where the drag last sent (a move not sent yet waits for the frame).
#[derive(Clone, Copy, Debug)]
struct Press {
    el: u64,
    rect: Laid,
    start: (f32, f32),
    last: (f32, f32),
    sent: (f32, f32),
    moved: bool,
    pending: bool,
}

/// The pointer, key and focus state of the universal handlers: the element with `onHover` under the pointer, the press
/// held, the element that has the keys, and where the pointer was when `onPointermove` last sent.
#[derive(Debug, Default)]
pub struct Prims {
    over: Option<u64>,
    press: Option<Press>,
    keys: Option<u64>,
    moved_at: Option<(f32, f32)>,
}

/// A point over rectangle `r` as den sends it: points from its top left, and shares of its size.
fn point_value(r: Laid, x: f32, y: f32) -> Value {
    let (px, py) = (x - r.x, y - r.y);
    serde_json::json!({"x": round(px), "y": round(py), "u": round(if r.w > 0.0 { px / r.w } else { 0.0 }), "v": round(if r.h > 0.0 { py / r.h } else { 0.0 })})
}

/// A point or share on the wire: to a hundredth, so an f32 sends no float noise.
fn round(v: f32) -> f64 {
    (f64::from(v) * 100.0).round() / 100.0
}

/// The DOM name of key `code` (a macOS virtual key code) for `onKeydown`, when it types no character of its own.
fn key_name(code: u16) -> Option<&'static str> {
    Some(match code {
        36 | 76 => "Enter",
        53 => "Escape",
        48 => "Tab",
        51 => "Backspace",
        117 => "Delete",
        126 => "ArrowUp",
        125 => "ArrowDown",
        123 => "ArrowLeft",
        124 => "ArrowRight",
        116 => "PageUp",
        121 => "PageDown",
        115 => "Home",
        119 => "End",
        _ => return None,
    })
}

impl App {
    /// The nearest element at or around the hovered node (within 64 levels) whose handlers `want` takes.
    fn gen_on_under(&self, want: impl Fn(&On) -> bool) -> Option<(u64, On)> {
        let w = self.world.world_ref();
        let h = w.entity_named(APP).target(Hovered::id(), 0)?;
        std::iter::successors(Some(h), |n| n.parent()).take(64).find_map(|n| On::of(n).filter(|o| want(o)).map(|o| (*n.id(), o)))
    }

    /// Send element `el`'s handler `key` (one of [`HANDLERS`]'s) with `value`, when it has one and is still there.
    fn gen_emit(&self, el: u64, key: &str, value: Value) {
        let w = self.world.world_ref();
        let Some(e) = live(w, el) else { return };
        if let Some(on) = On::of(e)
            && let Some(h) = on.by.get(key).and_then(Value::as_str)
        {
            self.link.ui_event(&on.cell, h, value);
        }
    }

    fn gen_rect(&self, el: u64) -> Laid {
        live(self.world.world_ref(), el).and_then(|e| e.try_cloned::<&Laid>()).unwrap_or_default()
    }

    /// Element `el` takes the keys (None: none does): the one that had them hears onBlur, `el` onFocus.
    fn gen_focus(&self, el: Option<u64>) {
        let was = std::mem::replace(&mut self.gen_prims.borrow_mut().keys, el);
        if was == el {
            return;
        }
        if let Some(w) = was {
            self.gen_emit(w, "blur", Value::Null);
        }
        if let Some(e) = el {
            self.gen_emit(e, "focus", Value::Null);
        }
    }

    /// A left press: an element with key handlers under it takes the keys (from whichever had them; a press on none lets
    /// them go), and one with pointer handlers hears onPointerdown and holds the press for its drag and release. Whether
    /// an element took the press (the transcript then starts no selection under it).
    pub(in crate::app) fn genui_common_down(&mut self) -> bool {
        let (x, y) = (self.pointer.x, self.pointer.y);
        let keys = self.gen_on_under(On::keys);
        self.gen_focus(keys.as_ref().map(|(el, _)| *el));
        let Some((el, on)) = self.gen_on_under(On::pointer) else { return keys.is_some() };
        let rect = self.gen_rect(el);
        if on.has("down") {
            self.gen_emit(el, "down", point_value(rect, x, y));
        }
        self.gen_prims.borrow_mut().press = Some(Press { el, rect, start: (x, y), last: (x, y), sent: (x, y), moved: false, pending: false });
        self.poke();
        true
    }

    /// The pointer moved with a press held on an element: past the slop it is a drag, its move sent on the next frame
    /// (coalesced). Whether such a press is held.
    pub(in crate::app) fn genui_common_drag(&mut self) -> bool {
        let (x, y) = (self.pointer.x, self.pointer.y);
        let slop = feel(self.world.world_ref()).drag_slop;
        let mut prims = self.gen_prims.borrow_mut();
        let Some(p) = prims.press.as_mut() else { return false };
        p.last = (x, y);
        if !p.moved && ((x - p.start.0).powi(2) + (y - p.start.1).powi(2)).sqrt() > slop {
            p.moved = true;
        }
        p.pending |= p.moved;
        drop(prims);
        self.poke();
        true
    }

    /// The press let go: a drag sends what it has not yet, then its end; then onPointerup. Whether a press was held.
    pub(in crate::app) fn genui_common_up(&mut self) -> bool {
        self.gen_drag_flush();
        let Some(p) = self.gen_prims.borrow_mut().press.take() else { return false };
        let (x, y) = p.last;
        if p.moved {
            let (rx, ry) = (x - p.rect.x, y - p.rect.y);
            let end = serde_json::json!({"dx": 0.0, "dy": 0.0, "x": round(rx), "y": round(ry), "tx": round(x - p.start.0), "ty": round(y - p.start.1), "phase": "end"});
            self.gen_emit(p.el, "drag", end);
        }
        self.gen_emit(p.el, "up", point_value(p.rect, x, y));
        self.poke();
        true
    }

    /// The drag's move since it last sent, if any: `{dx, dy, x, y, tx, ty, phase: 'move'}`.
    fn gen_drag_flush(&self) {
        let mut prims = self.gen_prims.borrow_mut();
        let Some(p) = prims.press.as_mut().filter(|p| p.pending) else { return };
        p.pending = false;
        let ((x, y), (sx, sy)) = (p.last, p.sent);
        p.sent = p.last;
        let p = *p;
        drop(prims);
        let v = serde_json::json!({
            "dx": round(x - sx), "dy": round(y - sy), "x": round(x - p.rect.x), "y": round(y - p.rect.y),
            "tx": round(x - p.start.0), "ty": round(y - p.start.1), "phase": "move",
        });
        self.gen_emit(p.el, "drag", v);
    }

    /// The wheel over an element with onWheel (at or around the hovered node): it hears `{dx, dy}` and keeps the wheel.
    pub(in crate::app) fn genui_common_wheel(&mut self, dx: f32, dy: f32) -> bool {
        let Some((el, _)) = self.gen_on_under(|o| o.has("wheel")) else { return false };
        self.gen_emit(el, "wheel", serde_json::json!({"dx": round(dx), "dy": round(dy)}));
        true
    }

    /// A key while an element has the keys: it hears `{key}` (the DOM name; a key that types sends each character it
    /// types), and Escape also lets the keys go. Command chords stay the app's. Whether the element took it.
    pub(in crate::app) fn genui_common_key(&mut self, k: &crate::term::KeyIn<'_>) -> bool {
        let Some(el) = self.gen_prims.borrow().keys else { return false };
        if k.mods.cmd || k.composing {
            return false;
        }
        if live(self.world.world_ref(), el).is_none_or(|e| On::of(e).is_none_or(|o| !o.keys())) {
            self.gen_prims.borrow_mut().keys = None;
            return false;
        }
        if k.action == crate::term::KeyAction::Release {
            return true;
        }
        match key_name(k.code) {
            Some(name) => {
                self.gen_emit(el, "key", serde_json::json!({"key": name}));
                if name == "Escape" {
                    self.gen_focus(None);
                }
            }
            None => {
                for c in k.text.chars().filter(|c| !c.is_control()) {
                    self.gen_emit(el, "key", serde_json::json!({"key": c.to_string()}));
                }
            }
        }
        self.poke();
        true
    }

    /// Once a frame: the element with onHover under the pointer hears it come (true) and go (false); with no press held,
    /// the one with onPointermove hears where the pointer is when it moved; a drag's move goes out (coalesced to one a
    /// frame); the keys let go of an element gone from its tree.
    pub(in crate::app) fn step_genui_common(&mut self) {
        let w = self.world.world_ref();
        let alive = |el: u64| live(w, el).is_some_and(|e| e.has(GenOn::id()));
        let keys = self.gen_prims.borrow().keys;
        if keys.is_some_and(|k| !alive(k)) {
            self.gen_prims.borrow_mut().keys = None;
        }
        let over = self.gen_on_under(|o| o.has("hover")).map(|(el, _)| el);
        let was = std::mem::replace(&mut self.gen_prims.borrow_mut().over, over);
        if over != was {
            if let Some(el) = was.filter(|el| alive(*el)) {
                self.gen_emit(el, "hover", Value::Bool(false));
            }
            if let Some(el) = over {
                self.gen_emit(el, "hover", Value::Bool(true));
            }
        }
        let at = (self.pointer.x, self.pointer.y);
        if !self.pointer.down
            && let Some((el, _)) = self.gen_on_under(|o| o.has("moved"))
            && self.gen_prims.borrow().moved_at != Some(at)
        {
            self.gen_prims.borrow_mut().moved_at = Some(at);
            self.gen_emit(el, "moved", point_value(self.gen_rect(el), at.0, at.1));
        }
        self.gen_drag_flush();
    }
}

/// Entity `el`, while it lives.
fn live(w: WorldRef<'_>, el: u64) -> Option<EntityView<'_>> {
    Some(w.entity_from_id(el)).filter(|e| e.is_alive())
}
