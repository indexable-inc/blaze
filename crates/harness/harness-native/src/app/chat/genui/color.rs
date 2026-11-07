//! A ColorPicker, after macOS's accent picker (the one element harness draws unlike den, which offers tones alone): its
//! label over a row of round swatches, the picked one ringed, and a last well, a wheel of hues, that opens the system
//! colour panel (NSColorPanel, window.rs) for any colour. The cell gets a colour: onChange is sent `#rrggbb`.
//!
//! `options` are `#rrggbb` words or tone names (a tone shows the theme's ink for it, and sends that ink as it shows on the
//! ground under it); with none it offers the tones. `selected` (bindable) is a `#rrggbb` or a tone; a colour that is no
//! swatch's shows as a dot in the well, ringed. A pick shows at once: it holds over what the tree says until the tree says
//! another (its onChange's echo), as a split's drag does (control.rs `Kept`). The ring is one node per picker, matched by
//! id, so a new pick's ring slides from the last one on the matched spring.
//!
//! A swatch's press carries `ui:` and `{"k":"color", cell, node, change, value}` (the well's `"well":true` as well), taken
//! by [`App::genui_color`]. The well asks for the panel: the window opens it after the frame, on the colour shown, and
//! sends each colour the user picks in it to [`App::genui_color_panel`], which sends onChange at most every
//! [`PANEL_EVERY`] seconds and once more for the last.
use super::*;
use control::{arm, handler, inert, labelled};
use harness_native_ui_look_api::{Rgba, Scheme};
use harness_native_ui_motion_api::MatchedGeometry;
use harness_native_ui_text_api::{Vector, VectorFit};
use std::collections::BTreeMap;

/// The tones offered when a picker says none: den's swatch tones, less `strong` (the ink here, so it would be a second
/// swatch of the same colour).
pub(super) const TONES: [&str; 6] = ["ink", "muted", "accent", "success", "warning", "error"];
/// The tones an option may name (props.rs's, which `strong` is one of).
pub(super) const NAMED: [&str; 7] = ["ink", "muted", "strong", "error", "accent", "success", "warning"];
/// The most options a picker takes.
pub(super) const MAX_SWATCHES: usize = 24;
/// The panel sends a colour at most this often (seconds) while it is dragged about, and once more for the last.
const PANEL_EVERY: f32 = 0.050;
/// The wheel's wedges (hn.GenColorWheel's children).
const WEDGES: usize = 12;

/// The pickers in use: each one's pick held over the tree's `selected`, what the tree said last, and the one the system
/// colour panel works for.
#[derive(Default)]
pub(super) struct Colors {
    kept: BTreeMap<u64, Kept>,
    said: BTreeMap<u64, Value>,
    panel: Option<Panel>,
    /// A press on a well asked for the panel, on this colour (sRGB, 0 to 1): the window takes it after the frame.
    ask: Option<[f64; 3]>,
}

/// A pick (`at`, `#rrggbb`) made while the tree said `from`.
struct Kept {
    from: Value,
    at: String,
}

/// What the system colour panel picks for: a picker's cell, element and onChange, and what it last sent and when.
struct Panel {
    cell: String,
    node: u64,
    change: Option<String>,
    latest: String,
    sent: Option<String>,
    last: f32,
}

/// One swatch: what it sends, what it draws.
struct Swatch {
    hex: String,
    fill: Fill,
}

/// `s` as a colour when it is `#rrggbb` (either case): sRGB, 0 to 1.
pub(super) fn parse_hex(s: &str) -> Option<[f32; 3]> {
    let h = s.strip_prefix('#').filter(|h| h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()))?;
    let at = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok().map(|v| f32::from(v) / 255.0);
    Some([at(0)?, at(2)?, at(4)?])
}

/// Colour `c` (sRGB, 0 to 1) as `#rrggbb`.
fn hex(c: [f32; 3]) -> String {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", b(c[0]), b(c[1]), b(c[2]))
}

/// Whether `v`'s words are drawn dark (the nearest `Scheme` above it says; the chat's pane and the gallery set one).
fn dark_at(v: View<'_>) -> bool {
    let mut e = Some(v.entity());
    while let Some(x) = e {
        if let Some(s) = x.try_cloned::<&Scheme>() {
            return s.dark;
        }
        e = x.parent();
    }
    true
}

/// Fill `f` as it shows in `dark` (or light) over the chat's ground (the theme's `ground`), opaque.
fn shown(world: WorldRef<'_>, f: Fill, dark: bool) -> [f32; 3] {
    let pick = |f: Fill| if dark { f.dark } else { f.light };
    let c = pick(f);
    let g = pick(crate::look::tone(world, "ground"));
    let mix = |a: f32, b: f32| a * c.a + b * (1.0 - c.a);
    [mix(c.r, g.r), mix(c.g, g.g), mix(c.b, g.b)]
}

/// The swatches `node` offers, and the colour words `said` name (a tone, else a `#rrggbb`), in `dark`.
fn swatches(world: WorldRef<'_>, node: &Value, dark: bool) -> Vec<Swatch> {
    let given: Vec<&str> = prop(node, "options").as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let given = if given.is_empty() { TONES.to_vec() } else { given };
    let mut out: Vec<Swatch> = Vec::new();
    // One swatch a colour: `strong` is the ink here, so the kernel's seven tones show six.
    for s in given.into_iter().take(MAX_SWATCHES).filter_map(|s| swatch(world, s, dark)) {
        if !out.iter().any(|o| o.hex == s.hex) {
            out.push(s);
        }
    }
    out
}

fn swatch(world: WorldRef<'_>, s: &str, dark: bool) -> Option<Swatch> {
    if let Some(c) = parse_hex(s) {
        let c = Rgba { r: c[0], g: c[1], b: c[2], a: 1.0 };
        return Some(Swatch { hex: hex([c.r, c.g, c.b]), fill: Fill { dark: c, light: c } });
    }
    let fill = media::fill_of(world, &format!("hn.GenTone.{s}"))?;
    Some(Swatch { hex: hex(shown(world, fill, dark)), fill })
}

/// Draw ColorPicker `node` on `v` (an `hn.GenColorPicker`), and arm each swatch.
pub(super) fn element(v: View<'_>, node: &Value, b: &mut Budget<'_, '_>) {
    let world = v.entity().world();
    let id = v.id();
    let dark = dark_at(v);
    let all = swatches(world, node, dark);
    let said = prop(node, "selected").clone();
    let tree_pick = said.as_str().and_then(|s| swatch(world, s, dark)).map(|s| s.hex);
    // A pick made here holds until the tree says another.
    let mut pick = tree_pick;
    if let Some(live) = b.live.as_deref_mut() {
        let c = &mut live.colors;
        // Forget the pickers that are gone, now and then.
        if c.said.len() > 64 {
            c.said.retain(|e, _| world.entity_from_id(*e).is_alive());
            c.kept.retain(|e, _| world.entity_from_id(*e).is_alive());
        }
        c.said.insert(id, said.clone());
        match c.kept.get(&id) {
            Some(k) if k.from == said => pick = Some(k.at.clone()),
            Some(_) => _ = c.kept.remove(&id),
            None => {}
        }
        // The panel's last colour, sent once the rate allows.
        let now = b.now;
        if let Some(p) = c.panel.as_mut().filter(|p| p.node == id)
            && p.sent.as_deref() != Some(p.latest.as_str())
        {
            if now - p.last >= PANEL_EVERY {
                if let Some(h) = &p.change {
                    live.pending.push((p.cell.clone(), h.clone(), Value::String(p.latest.clone())));
                }
                p.sent = Some(p.latest.clone());
                p.last = now;
            } else {
                live.anim = true;
            }
        }
    }
    let at = pick.as_ref().and_then(|h| all.iter().position(|s| &s.hex == h));
    let custom = pick.as_ref().filter(|_| at.is_none()).cloned();
    let change = handler(node, "onChange");
    inert(v, change.is_none());
    let Some(row) = labelled(v, &words(node, "label"), "hn.GenColorRow", None) else { return };
    let ring = MatchedGeometry { id: format!("hn.gen.color.{id}").as_str().into(), jump: 0.0 };
    let action = |value: &str, well: bool| {
        let mut a = serde_json::json!({"k": "color", "cell": b.cell, "node": id, "change": change, "value": value});
        if well {
            a["well"] = Value::Bool(true);
        }
        format!("ui:{a}")
    };
    let mut keyed: Vec<(String, &str, Option<&Swatch>)> = all.iter().enumerate().map(|(i, s)| (format!("s{i}"), "hn.GenSwatch", Some(s))).collect();
    keyed.push(("well".to_owned(), "hn.GenSwatch", None));
    let well_at = custom.clone().or_else(|| pick.clone()).or_else(|| all.first().map(|s| s.hex.clone())).unwrap_or_else(|| "#808080".to_owned());
    let wheel = view::prefab(world, "hn.GenColorWheel");
    view::for_each_of(row, keyed, |sw, s, _| {
        let ringed = match s {
            Some(s) => pick.as_deref() == Some(s.hex.as_str()),
            None => custom.is_some(),
        };
        arm(sw, change.as_ref().map(|_| action(s.map_or(well_at.as_str(), |s| s.hex.as_str()), s.is_none())).as_deref());
        let mut parts: Vec<(&str, &str)> = match s {
            Some(_) => vec![("dot", "hn.GenSwatchDot")],
            None if custom.is_some() => vec![("wheel", "hn.GenSwatchWheel"), ("custom", "hn.GenSwatchCustom")],
            None => vec![("wheel", "hn.GenSwatchWheel")],
        };
        if ringed {
            parts.push(("ring", "hn.GenSwatchRing"));
        }
        view::for_each_of(sw, parts.iter().map(|(k, p)| (*k, *p, *k)), |part, k, _| match k {
            "dot" => media::filled(part, s.map(|s| s.fill)),
            "custom" => {
                let c = custom.as_deref().and_then(parse_hex).map(|c| Rgba { r: c[0], g: c[1], b: c[2], a: 1.0 });
                media::filled(part, c.map(|c| Fill { dark: c, light: c }));
            }
            "wheel" => wedges(part, wheel),
            "ring" => {
                if part.entity().try_cloned::<&MatchedGeometry>().as_ref() != Some(&ring) {
                    part.set(ring.clone());
                }
            }
            _ => {}
        });
    });
}

/// The well's wheel: a wedge of each of `wheel`'s hues, from twelve o'clock round, each a little past its turn so no
/// seam shows between them.
fn wedges(v: View<'_>, wheel: Option<EntityView<'_>>) {
    let turn = std::f32::consts::TAU / WEDGES as f32;
    view::for_each_of(v, (0..WEDGES).map(|i| (format!("w{i}"), "hn.GenShape", i)), |w, i, _| {
        let a0 = -std::f32::consts::FRAC_PI_2 + turn * i as f32;
        let a1 = a0 + turn * 1.08;
        let d = format!("M0 0L{} {}A1 1 0 0 1 {} {}Z", a0.cos(), a0.sin(), a1.cos(), a1.sin());
        let want = Vector { d: d.as_str().into(), view_x: -1.0, view_y: -1.0, view_w: 2.0, view_h: 2.0, fit: VectorFit::Meet, ..Vector::default() };
        if w.entity().try_cloned::<&Vector>().as_ref() != Some(&want) {
            w.set(want);
        }
        media::filled(w, wheel.and_then(|p| p.lookup_child(&format!("w{i}"))).and_then(|c| c.try_cloned::<&Fill>()));
    });
}

impl Colors {
    /// Picker `node` shows `at` (#rrggbb) from now on, over what its tree says, until the tree says another.
    pub(super) fn keep(&mut self, node: u64, at: &str) {
        let from = self.said.get(&node).cloned().unwrap_or(Value::Null);
        self.kept.insert(node, Kept { from, at: at.to_owned() });
    }
}

impl App {
    /// A press on a ColorPicker's swatch or well (control.rs `genui_action`, `ui:` and `{"k":"color", ...}`): a swatch's
    /// colour shows at once and goes to onChange; the well asks for the system colour panel, for this picker.
    pub(super) fn genui_color(&mut self, v: &Value) {
        let (Some(cell), Some(node), Some(value)) = (v["cell"].as_str(), v["node"].as_u64(), v["value"].as_str().and_then(parse_hex)) else {
            eprintln!("harness-native: genui: a ColorPicker's press names no cell, element and #rrggbb value: {v}");
            return;
        };
        let change = v["change"].as_str().map(str::to_owned);
        let at = hex(value);
        let mut live = self.genui.borrow_mut();
        if v["well"] == true {
            live.colors.panel = Some(Panel { cell: cell.to_owned(), node, change, latest: at.clone(), sent: Some(at), last: f32::NEG_INFINITY });
            live.colors.ask = Some(value.map(f64::from));
            return;
        }
        live.colors.keep(node, &at);
        drop(live);
        if let Some(h) = change {
            self.link.ui_event(cell, &h, Value::String(at));
        }
    }

    /// The colour the system colour panel opens on (sRGB, 0 to 1), when a press on a well asked for it since the last
    /// call: window.rs opens the panel with it after the frame. Never in a shot or a test, which have no window.
    pub(crate) fn take_color_panel(&mut self) -> Option<[f64; 3]> {
        self.genui.borrow_mut().colors.ask.take()
    }

    /// The system colour panel's colour is now `rgb` (sRGB, 0 to 1; the user is picking in it): it shows on its picker at
    /// once, and goes to the picker's onChange at most every [`PANEL_EVERY`] seconds (the last one once the rate allows,
    /// sent as the picker is drawn). A panel whose picker is gone picks for nothing.
    pub(crate) fn genui_color_panel(&mut self, rgb: [f64; 3]) {
        let w = self.world.world_ref();
        let now = w.try_get::<&harness_native_api::Clock>(|c| c.time).unwrap_or(0.0);
        let at = hex(rgb.map(|c| c as f32));
        let mut out = None;
        {
            let mut live = self.genui.borrow_mut();
            let c = &mut live.colors;
            let Some(p) = c.panel.as_mut() else { return };
            if !w.entity_from_id(p.node).is_alive() {
                c.panel = None;
                return;
            }
            p.latest = at.clone();
            let node = p.node;
            c.keep(node, &at);
            let Some(p) = c.panel.as_mut() else { return };
            if p.sent.as_deref() != Some(at.as_str()) && now - p.last >= PANEL_EVERY {
                out = p.change.clone().map(|h| (p.cell.clone(), h));
                p.sent = Some(at.clone());
                p.last = now;
            }
        }
        if let Some((cell, h)) = out {
            self.link.ui_event(&cell, &h, Value::String(at));
        }
        self.poke();
    }
}
