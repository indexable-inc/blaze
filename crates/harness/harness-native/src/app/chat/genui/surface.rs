//! A cell's live pictures: `Shader` (a fragment shader the kernel compiled to MSL, rendered off screen by the host's Media)
//! and `Video` (an AVPlayer whose frames Media pulls). Each element's `pic` part is a `Surface` keyed by its entity; the
//! app keeps each element's state here ([`Surfaces`], in [`Live`]) and, after the frame is painted, asks Media for what
//! it should show ([`App::step_media`]: a Spec each into `Draw::media`, visible when its Surface was painted).
//!
//! Box: the `width` prop, else the column's; the `height` prop, else width over `aspect`; with neither height nor aspect
//! a Shader is $gen_shader_h tall and a Video takes its own shape once Media knows it (16:9 until then).
//!
//! Ids: while a cell is filled, each element with an `id` records what it names ([`record`], [`Ref`]): a Slider's or
//! NumberField's live number (a dragged slider's as dragged), a Toggle's or Checkbox's 1 or 0, a Video's or Shader's
//! entity, an Image's file. Once the whole cell is filled each Shader is resolved against them ([`resolve`]): a uniform
//! `from` an id reads that value (a Video's: its playhead in seconds), a texture ref binds that element's frames (a
//! Shader's own id: its previous frame) or that Image's file, and a texture that is a video file gets a hidden player of
//! its own. An id the cell does not have, or one of the wrong kind, is said in words over the shader, in the failed tone.
//!
//! What a frame asks of Media follows `harness_native_render::media`'s contract: a shader's data is the ShaderFrame
//! floats `[w_px, h_px, px_per_point, time, pointer_x, pointer_y, hovered, down, slots...]` (4 floats a slot, wire order,
//! at least one slot), the pointer in device pixels from the picture's top left and (-1, -1, 0, 0) when it is not over it.
//! A video's `play`, `muted` and seeks are the app's: the wire's `paused` and `currentTime` are applied only when they
//! change (and a `currentTime` only when it is more than [`SEEK_SLACK`] from the playhead, so a timeupdate's echo never
//! seeks); the person's presses flip them locally (local actions `media:play:<entity>`, `media:mute:<entity>`,
//! `media:resume:<entity>`), and events go back through the `ui:` path: onPlay and onPause on a press, onEnded once each
//! time it ends, onTimeUpdate every [`TU_EVERY`] while it plays and once after a seek.
use super::*;
use control::arm;
use harness_native_api::{APP, HoverAmt};
use harness_native_render::draw::Draw;
use harness_native_render::media::{Input, Media, ShaderSpec, Spec as MediaSpec, Status, VideoSpec, VideoState};
use harness_native_ui_input_api::Hovered;
use harness_native_ui_layout_api::{Aspect, SizeKind};
use harness_native_ui_node_api::Laid;
use harness_native_ui_motion_api::Pose;
use harness_native_ui_text_api::{Picture, Surface};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

/// The most uniforms and textures a Shader has (the kernel's caps).
const MAX_UNIFORMS: usize = 16;
const MAX_TEXTURES: usize = 8;
/// A video's onTimeUpdate is sent at most this often (seconds) while it plays or is scrubbed.
const TU_EVERY: f32 = 0.25;
/// A wire `currentTime` this close to the playhead (seconds) is the player's own time echoed back, not a seek.
const SEEK_SLACK: f64 = 0.3;
/// The most device pixels a shader's picture is on a side (Media's cap).
const MAX_PX: f32 = 2048.0;
/// A video's shape until Media knows its own.
const VIDEO_SHAPE: f32 = 16.0 / 9.0;
/// The most failed shaders remembered (by MSL key) to draw as refusals.
const MAX_FAILED: usize = 256;
/// The bit a hidden player's key carries (with its shader's entity and texture slot): flecs never sets it in an entity
/// id, so it is never an element's.
const PLAYER_BIT: u64 = 0x4000_0000_0000_0000;
/// cod-play, cod-debug-pause, cod-mute, cod-unmute.
const PLAY: &str = "\u{EB2C}";
const PAUSE: &str = "\u{EAD1}";
const MUTED: &str = "\u{EB24}";
const SOUND: &str = "\u{EB75}";

/// What an id in a cell names.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Ref {
    /// A Slider's or NumberField's number, a Toggle's or Checkbox's 1 or 0.
    Value(f64),
    Video(u64),
    Shader(u64),
    /// An Image's file (an absolute path).
    Image(String),
}

impl Ref {
    fn said(&self) -> &'static str {
        match self {
            Ref::Value(_) => "a control's value",
            Ref::Video(_) => "a Video",
            Ref::Shader(_) => "a Shader",
            Ref::Image(_) => "an Image",
        }
    }
}

/// One uniform as the wire has it: its lanes' values, or the id whose live value it reads.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Uniform {
    name: String,
    value: [f32; 4],
    from: Option<String>,
}

/// One texture slot as the wire has it.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Tex {
    /// A PNG or JPEG at this absolute path.
    Image(String),
    /// A video file's absolute path or an http(s) URL: a hidden player of its own.
    Video(String),
    /// An id in the cell (a Video's or a Shader's).
    Ref(String),
}

/// A Shader's wire props (the kernel's `{msl, key, uniforms, textures, animate, pointer}`).
pub(super) struct Wire<'v> {
    msl: &'v str,
    key: u64,
    uniforms: Vec<Uniform>,
    textures: Vec<(String, Tex)>,
    animate: bool,
    pointer: bool,
}

/// A Shader's wire props, or why they are not what the kernel sends (props.rs draws it as a refusal).
pub(super) fn wire(node: &Value) -> Result<Wire<'_>, String> {
    let p = &node["props"];
    let msl = match &p["msl"] {
        Value::String(s) if !s.is_empty() => s.as_str(),
        v => return Err(format!("<Shader> msl is {}, not the kernel's compiled MSL", control::describe(v))),
    };
    let key = match &p["key"] {
        Value::String(k) if k.len() == 16 => u64::from_str_radix(k, 16).map_err(|_| format!("<Shader> key \u{201C}{k}\u{201D} is not 16 hex digits"))?,
        Value::String(k) => {
            let said: String = k.chars().take(32).collect();
            return Err(format!("<Shader> key \u{201C}{said}\u{201D} is not 16 hex digits"));
        }
        v => return Err(format!("<Shader> key is {}, not 16 hex digits", control::describe(v))),
    };
    let flag = |name: &str| match &p[name] {
        Value::Bool(b) => Ok(*b),
        Value::Null => Ok(false),
        v => Err(format!("<Shader> {name} is {}, not true or false", control::describe(v))),
    };
    let (animate, pointer) = (flag("animate")?, flag("pointer")?);
    let empty = Vec::new();
    let list = |name: &str| match &p[name] {
        Value::Null => Ok(&empty),
        Value::Array(a) => Ok(a),
        v => Err(format!("<Shader> {name} is {}, not a list", control::describe(v))),
    };
    let (us, ts) = (list("uniforms")?, list("textures")?);
    if us.len() > MAX_UNIFORMS {
        return Err(format!("<Shader> has {} uniforms: at most {MAX_UNIFORMS}", us.len()));
    }
    if ts.len() > MAX_TEXTURES {
        return Err(format!("<Shader> has {} textures: at most {MAX_TEXTURES}", ts.len()));
    }
    let name_of = |what: &str, i: usize, u: &Value| match &u["name"] {
        Value::String(n) if !n.is_empty() => Ok(n.clone()),
        v => Err(format!("<Shader> {what}[{i}].name is {}, not a name", control::describe(v))),
    };
    let mut uniforms = Vec::with_capacity(us.len());
    for (i, u) in us.iter().enumerate() {
        let name = name_of("uniforms", i, u)?;
        let lanes = match u["lanes"].as_u64() {
            Some(n @ 1..=4) => n as usize,
            _ => return Err(format!("<Shader> uniform \u{201C}{name}\u{201D} lanes is {}, not 1 to 4", control::describe(&u["lanes"]))),
        };
        let mut value = [0.0f32; 4];
        match &u["value"] {
            Value::Array(a) if a.len() == 4 && a.iter().all(|x| x.as_f64().is_some_and(f64::is_finite)) => {
                for (lane, x) in value.iter_mut().zip(a).take(lanes) {
                    *lane = x.as_f64().unwrap_or(0.0) as f32;
                }
            }
            v => return Err(format!("<Shader> uniform \u{201C}{name}\u{201D} value is {}, not 4 numbers", control::describe(v))),
        }
        let from = match &u["from"] {
            Value::Null => None,
            Value::String(id) if !id.is_empty() => Some(id.clone()),
            v => return Err(format!("<Shader> uniform \u{201C}{name}\u{201D} from is {}, not an id", control::describe(v))),
        };
        uniforms.push(Uniform { name, value, from });
    }
    let mut textures = Vec::with_capacity(ts.len());
    for (i, t) in ts.iter().enumerate() {
        let name = name_of("textures", i, t)?;
        let text = |k: &str| t[k].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        let tex = match (t["kind"].as_str(), text("src"), text("ref")) {
            (Some("image"), Some(src), _) if src.starts_with('/') => Tex::Image(src),
            (Some("video"), Some(src), _) if src.starts_with('/') || src.starts_with("http://") || src.starts_with("https://") => Tex::Video(src),
            (Some("ref"), _, Some(id)) => Tex::Ref(id),
            (Some(k @ ("image" | "video")), src, _) => {
                return Err(format!(
                    "<Shader> texture \u{201C}{name}\u{201D} is a {k} whose src is {}, not an absolute path{}",
                    src.map_or_else(|| "missing".to_owned(), |s| format!("\u{201C}{s}\u{201D}")),
                    if k == "video" { " or an http(s) URL" } else { "" }
                ));
            }
            (Some("ref"), ..) => return Err(format!("<Shader> texture \u{201C}{name}\u{201D} is a ref with no id")),
            (k, ..) => return Err(format!("<Shader> texture \u{201C}{name}\u{201D} kind is {}, not image, video or ref", k.map_or("missing", |_| "unknown"))),
        };
        textures.push((name, tex));
    }
    Ok(Wire { msl, key, uniforms, textures, animate, pointer })
}

/// Why Shader `node` is drawn as a refusal: Media failed its MSL (it did not build, or ran too long on the GPU).
pub(super) fn failed(node: &Value, live: Option<&Live>) -> Option<String> {
    if node["type"].as_str() != Some("Shader") {
        return None;
    }
    let key = u64::from_str_radix(node["props"]["key"].as_str()?, 16).ok()?;
    let why = live?.surf.failed.get(&key)?;
    Some(format!("This shader could not be drawn: {why}"))
}

/// The cells' Shaders and Videos by element entity, what Media failed, and Media itself (the host's: [`App::set_media`];
/// None in tests and before the window has a renderer, when shaders show as building and videos as not yet loaded).
#[derive(Default)]
pub(in crate::app) struct Surfaces {
    pub(super) media: Option<Rc<RefCell<Media>>>,
    els: BTreeMap<u64, El>,
    /// Failed shaders' reasons by MSL key: the element is drawn as a refusal from then on (keyed by the code, not the
    /// element, so the refusal it is remade as is not asked of Media again).
    failed: BTreeMap<u64, String>,
    /// The pointer (view points) as the last frame saw it, to tell when it moved over a shader that reads it.
    pointer_at: (f32, f32),
}

enum El {
    Shader(ShaderEl),
    Video(VideoEl),
}

struct ShaderEl {
    msl: Rc<str>,
    msl_key: u64,
    uniforms: Vec<Uniform>,
    textures: Vec<(String, Tex)>,
    animate: bool,
    pointer: bool,
    /// Seconds it has animated while on screen.
    time: f32,
    /// Resolved against the cell's ids after each fill ([`resolve`]): each uniform's four lanes, each texture's input, the
    /// hidden players its video files play in, and what it could not resolve.
    slots: Vec<[f32; 4]>,
    inputs: Vec<Input>,
    players: Vec<VideoSpec>,
    problem: Option<String>,
    /// The person pressed Run on it while it was paused: sent with the next spec.
    resume: bool,
    /// The pointer was over it last frame.
    over: bool,
}

struct VideoEl {
    cell: String,
    src: String,
    looped: bool,
    /// What the app has it doing: paused or not (the wire's `paused`, else not `autoplay`; then the person's presses),
    /// muted or not (the wire's, then the mute press).
    paused: bool,
    muted: bool,
    /// The wire's `paused`, `currentTime` and `muted` as last seen: a change is applied, the same value again is not.
    wire_paused: Option<bool>,
    wire_time: Option<f64>,
    wire_muted: bool,
    on_time: Option<String>,
    on_ended: Option<String>,
    on_play: Option<String>,
    on_pause: Option<String>,
    /// A seek to ask Media for with the next spec, and the time to say in onTimeUpdate once it is asked.
    seek: Option<f64>,
    told: Option<f64>,
    /// When the last onTimeUpdate was sent (the app's clock).
    last_tu: f32,
    /// Media said it had ended last frame (onEnded goes once, on the change).
    was_ended: bool,
    /// It has played (the poster goes).
    played: bool,
    /// Where its scrub rail was last dragged to, while it is.
    scrub: Option<f64>,
}

impl Surfaces {
    /// Video `node`'s scrub rail was let go at `at` seconds: it seeks there.
    pub(super) fn scrubbed(&mut self, node: u64, at: f64) {
        if let Some(El::Video(v)) = self.els.get_mut(&node) {
            v.seek = Some(at);
            v.scrub = Some(at);
        }
    }

    /// Shader `e`'s state, made or brought up to date with its wire props.
    fn shader(&mut self, e: u64, w: &Wire<'_>) -> &mut ShaderEl {
        let fresh = || {
            El::Shader(ShaderEl {
                msl: Rc::from(w.msl),
                msl_key: w.key,
                uniforms: Vec::new(),
                textures: Vec::new(),
                animate: w.animate,
                pointer: w.pointer,
                time: 0.0,
                slots: Vec::new(),
                inputs: Vec::new(),
                players: Vec::new(),
                problem: None,
                resume: false,
                over: false,
            })
        };
        let el = self.els.entry(e).or_insert_with(fresh);
        if !matches!(el, El::Shader(_)) {
            // An entity is made from one prefab: a key that was a video's is a new element.
            *el = fresh();
        }
        let El::Shader(s) = el else { unreachable!("made a shader above") };
        if s.msl_key != w.key {
            s.msl = Rc::from(w.msl);
            s.msl_key = w.key;
        }
        if s.uniforms != w.uniforms {
            s.uniforms.clone_from(&w.uniforms);
        }
        if s.textures != w.textures {
            s.textures.clone_from(&w.textures);
        }
        s.animate = w.animate;
        s.pointer = w.pointer;
        s
    }

    /// Video `e`'s state, made from `node`'s props the first time (paused as the wire says, else unless it autoplays; a
    /// `currentTime` asked as a seek) and after that following the wire's changes only. `at` is the playhead Media has.
    fn video(&mut self, e: u64, cell: &str, node: &Value, at: Option<f64>) -> &mut VideoEl {
        let p = &node["props"];
        let wire_paused = p["paused"].as_bool();
        let wire_time = p["currentTime"].as_f64().filter(|t| t.is_finite() && *t >= 0.0);
        let wire_muted = p["muted"].as_bool().unwrap_or(true);
        let made = !matches!(self.els.get(&e), Some(El::Video(_)));
        if made {
            let seek = wire_time.filter(|t| *t > SEEK_SLACK);
            self.els.insert(
                e,
                El::Video(VideoEl {
                    cell: cell.to_owned(),
                    src: String::new(),
                    looped: false,
                    paused: wire_paused.unwrap_or(p["autoplay"] != true),
                    muted: wire_muted,
                    wire_paused,
                    wire_time,
                    wire_muted,
                    on_time: None,
                    on_ended: None,
                    on_play: None,
                    on_pause: None,
                    seek,
                    told: seek,
                    last_tu: f32::NEG_INFINITY,
                    was_ended: false,
                    played: false,
                    scrub: None,
                }),
            );
        }
        let Some(El::Video(v)) = self.els.get_mut(&e) else { unreachable!("made a video above") };
        if !made {
            if v.wire_paused != wire_paused {
                v.wire_paused = wire_paused;
                if let Some(paused) = wire_paused {
                    v.paused = paused;
                }
            }
            if v.wire_time != wire_time {
                v.wire_time = wire_time;
                if let Some(t) = wire_time
                    && (t - at.unwrap_or(0.0)).abs() > SEEK_SLACK
                {
                    v.seek = Some(t);
                    v.told = Some(t);
                }
            }
            if v.wire_muted != wire_muted {
                v.wire_muted = wire_muted;
                v.muted = wire_muted;
            }
        }
        let text = |k: &str| p[k].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        cell.clone_into(&mut v.cell);
        if let Some(src) = text("src")
            && src != v.src
        {
            v.src = src;
        }
        v.looped = p["loop"] == true;
        v.on_time = text("onTimeUpdate");
        v.on_ended = text("onEnded");
        v.on_play = text("onPlay");
        v.on_pause = text("onPause");
        v
    }
}

/// What `node`'s `id` names, recorded for the Shaders in its cell: a Slider's or NumberField's number (the live one while
/// it is dragged), a Toggle's or Checkbox's 1 or 0, an Image's file. Videos and Shaders record theirs in [`element`].
pub(super) fn record(v: View<'_>, node: &Value, kind: &str, b: &mut Budget<'_, '_>) {
    let Some(id) = prop(node, "id").as_str().filter(|s| !s.is_empty()) else { return };
    let on = |name: &str| if prop(node, name) == &Value::Bool(true) { 1.0 } else { 0.0 };
    let r = match kind {
        "Slider" | "NumberField" => {
            let dragged = b.live.as_deref().and_then(|l| control::slide_value(l, v.id()));
            let least = if kind == "Slider" { control::number(node, "min").unwrap_or(0.0) } else { 0.0 };
            Ref::Value(dragged.or_else(|| control::number(node, "value")).unwrap_or(least))
        }
        "Toggle" => Ref::Value(on("value")),
        "Checkbox" => Ref::Value(on("checked")),
        "Image" => Ref::Image(words(node, "src")),
        _ => return,
    };
    b.ids.insert(id.to_owned(), r);
}

/// Draw Shader or Video `node` on `v` (its entity the key of its Surface and of its Media spec).
pub(super) fn element(v: View<'_>, node: &Value, kind: &str, b: &mut Budget<'_, '_>) {
    if let Some(id) = prop(node, "id").as_str().filter(|s| !s.is_empty()) {
        let r = if kind == "Shader" { Ref::Shader(v.id()) } else { Ref::Video(v.id()) };
        b.ids.insert(id.to_owned(), r);
    }
    if kind == "Shader" {
        shader(v, node, b);
    } else {
        video(v, node, b);
    }
}

/// `node`'s `aspect` (width over height), when said.
fn aspect(node: &Value) -> Option<f32> {
    prop(node, "aspect").as_f64().filter(|a| a.is_finite() && *a > 0.0).map(|a| a as f32)
}

fn sized(v: View<'_>, want: Size) {
    if v.entity().try_cloned::<&Size>() != Some(want) {
        v.set(want);
    }
}

fn shaped(v: View<'_>, ratio: f32) {
    let want = Aspect { ratio };
    if v.entity().try_cloned::<&Aspect>() != Some(want) {
        v.set(want);
    }
}

fn surfaced(v: View<'_>, key: u64, cover: bool) {
    let want = Surface { key, cover: i32::from(cover) };
    if v.entity().try_cloned::<&Surface>() != Some(want) {
        v.set(want);
    }
}

fn shader(v: View<'_>, node: &Value, b: &mut Budget<'_, '_>) {
    let e = v.id();
    let world = v.entity().world();
    // Checked before its prefab was picked (props.rs): only a refusal draws a wire that is not one.
    let Ok(w) = wire(node) else { return };
    let tall = view::prefab(world, "hn.GenShader").and_then(|p| p.try_cloned::<&Size>()).map_or(0.0, |s| s.h.value);
    let a = aspect(node);
    let fixed = Sizing::fixed;
    let size = match (points(node, "width"), points(node, "height")) {
        (Some(wd), Some(h)) => Size { w: fixed(wd), h: fixed(h) },
        (Some(wd), None) => Size { w: fixed(wd), h: fixed(a.map_or(tall, |a| wd / a)) },
        (None, Some(h)) => Size { w: Sizing::GROW, h: fixed(h) },
        (None, None) if a.is_some() => Size { w: Sizing::GROW, h: Sizing::FIT },
        (None, None) => Size { w: Sizing::GROW, h: fixed(tall) },
    };
    sized(v, size);
    let status = b.live.as_deref().and_then(|l| l.surf.media.as_ref()).and_then(|m| m.borrow().status(e));
    let mut problem = None;
    if let Some(live) = b.live.as_deref_mut() {
        problem.clone_from(&live.surf.shader(e, &w).problem);
        if let Some(Status::Failed(why)) = &status {
            // Drawn as a refusal from the next frame on (genui.rs `refusal`).
            if live.surf.failed.len() >= MAX_FAILED {
                live.surf.failed.clear();
            }
            live.surf.failed.insert(w.key, why.clone());
            live.anim = true;
        }
    }
    b.shaders.push(e);
    let said: Option<(&str, String, &str)> = match (problem, status) {
        (Some(p), _) => Some(("hn.GenShaderStatus", p, "error")),
        (None, Some(Status::Ready)) => None,
        (None, Some(Status::Paused(why))) => Some(("hn.GenShaderPaused", why, "muted")),
        (None, Some(Status::Failed(why))) => Some(("hn.GenShaderStatus", format!("This shader could not be drawn: {why}"), "error")),
        (None, None | Some(Status::Building)) => Some(("hn.GenShaderStatus", "Building shader".to_owned(), "muted")),
    };
    let mut parts = vec![("pic", "hn.GenShaderPic")];
    if let Some((prefab, ..)) = &said {
        parts.push(("status", *prefab));
    }
    view::for_each_of(v, parts.iter().map(|(k, p)| (*k, *p, *k)), |part, k, _| match k {
        "pic" => {
            sized(part, size);
            shaped(part, a.unwrap_or(1.0));
            surfaced(part, e, false);
        }
        _ => {
            let Some((prefab, text, tone)) = &said else { return };
            let paused = *prefab == "hn.GenShaderPaused";
            let plate = if paused { part.part("plate") } else { Some(part) };
            if let Some(t) = plate.and_then(|p| p.part("text")) {
                toned(t, text, tone);
            }
            if paused && let Some(p) = part.part("plate") {
                arm(p, Some(&format!("media:resume:{e}")));
            }
        }
    });
}

/// `m:ss` (`h:mm:ss` from an hour), whole seconds.
fn clock(secs: f64) -> String {
    let s = if secs.is_finite() { secs.max(0.0) as u64 } else { 0 };
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

fn video(v: View<'_>, node: &Value, b: &mut Budget<'_, '_>) {
    let e = v.id();
    let cell = b.cell;
    let media = b.live.as_deref().and_then(|l| l.surf.media.clone());
    let st: VideoState = media.as_ref().and_then(|m| m.borrow().video(e)).unwrap_or_default();
    let natural = st.size.filter(|(w, h)| *w > 0 && *h > 0).map_or(VIDEO_SHAPE, |(w, h)| w as f32 / h as f32);
    let cover = prop(node, "fit").as_str() == Some("cover");
    let a = aspect(node);
    let shape = a.unwrap_or(natural);
    let fixed = Sizing::fixed;
    let (wd, ht) = (points(node, "width"), points(node, "height"));
    let size = match (wd, ht) {
        (Some(w), Some(h)) => Size { w: fixed(w), h: fixed(h) },
        (Some(w), None) => Size { w: fixed(w), h: fixed(w / shape) },
        (None, Some(h)) => Size { w: Sizing::GROW, h: fixed(h) },
        (None, None) => Size { w: Sizing::GROW, h: Sizing::FIT },
    };
    // The picture: covering the box, or contained in it in its own shape (a box of another shape said by `aspect` alone,
    // with no size, is laid out in that shape and the picture stretched to it).
    let (pic, pic_shape) = match (cover, size.w.kind, size.h.kind) {
        (true, ..) => (size, shape),
        (false, SizeKind::Fixed, SizeKind::Fixed) => {
            let (bw, bh) = (size.w.value, size.h.value);
            let (w, h) = if bw / bh.max(1e-3) > natural { (bh * natural, bh) } else { (bw, bw / natural) };
            (Size { w: fixed(w), h: fixed(h) }, natural)
        }
        (false, _, SizeKind::Fixed) => (Size { w: Sizing::FIT, h: size.h }, natural),
        (false, ..) => (size, shape),
    };
    sized(v, size);
    // Its controls show on hover (its own hover spring, stepped by the Hover system) and while paused.
    if !v.entity().has(HoverAmt::id()) {
        v.entity().set(HoverAmt::default());
    }
    let hover = v.entity().try_cloned::<&HoverAmt>().map_or(0.0, |h| h.spring.x.clamp(0.0, 1.0));
    let (paused, muted, played, scrub) = match b.live.as_deref_mut() {
        Some(live) => {
            let scrub = control::slide_value(live, e);
            let el = live.surf.video(e, cell, node, Some(st.time));
            (el.paused, el.muted, el.played || st.playing, scrub)
        }
        None => (prop(node, "paused").as_bool().unwrap_or(prop(node, "autoplay") != &Value::Bool(true)), prop(node, "muted") != &Value::Bool(false), false, None),
    };
    let poster = words(node, "poster");
    let controls = prop(node, "controls") != &Value::Bool(false);
    let mut parts = vec![("pic", "hn.GenVideoPic")];
    if !poster.is_empty() && !played && st.error.is_none() {
        parts.push(("poster", "hn.GenVideoPoster"));
    }
    if st.error.is_some() {
        parts.push(("status", "hn.GenShaderStatus"));
    } else if controls {
        parts.push(("bar", "hn.GenVideoBar"));
    }
    let play = format!("media:play:{e}");
    view::for_each_of(v, parts.iter().map(|(k, p)| (*k, *p, *k)), |part, k, _| match k {
        "pic" => {
            sized(part, pic);
            shaped(part, pic_shape);
            surfaced(part, e, cover);
            arm(part, Some(&play));
        }
        "poster" => {
            sized(part, pic);
            shaped(part, pic_shape);
            if part.entity().try_cloned::<&Picture>().is_none_or(|p| p.handle.as_str() != poster) {
                part.set(Picture { handle: poster.as_str().into() });
            }
        }
        "status" => {
            if let (Some(t), Some(why)) = (part.part("text"), &st.error) {
                toned(t, &format!("This video could not be played: {why}"), "error");
            }
        }
        _ => {
            let shown = if paused || scrub.is_some() { 1.0 } else { hover };
            let pose = Pose { fade: 1.0 - shown, ..Pose::default() };
            if part.entity().try_cloned::<&Pose>() != Some(pose) {
                part.set(pose);
            }
            let Some(plate) = part.part("plate") else { return };
            if let Some(p) = plate.part("play") {
                if let Some(i) = p.part("icon") {
                    i.icon(if paused { PLAY } else { PAUSE });
                }
                arm(p, Some(&play));
            }
            if let Some(m) = plate.part("mute") {
                if let Some(i) = m.part("icon") {
                    i.icon(if muted { MUTED } else { SOUND });
                }
                arm(m, Some(&format!("media:mute:{e}")));
            }
            let at = scrub.unwrap_or(st.time);
            let frac = if st.duration > 0.0 { (at / st.duration).clamp(0.0, 1.0) as f32 } else { 0.0 };
            if let Some(rail) = plate.part("rail") {
                // A Grow of 0 counts as 1: the empty side keeps a sliver of weight instead.
                for (side, w) in [("fill", frac), ("rest", 1.0 - frac)] {
                    let Some(p) = rail.part(side) else { continue };
                    let mut s = p.entity().try_cloned::<&Size>().unwrap_or_default();
                    let w = w.max(1e-4);
                    if (s.w.value - w).abs() > 1e-5 {
                        s.w.value = w;
                        p.set(s);
                    }
                }
                // Scrubbed once its length is known.
                let scrubs = (st.duration > 0.0).then(|| control::scrub_action(cell, e, st.duration));
                arm(rail, scrubs.as_deref());
            }
            if let Some(t) = plate.part("elapsed") {
                let said = if st.duration > 0.0 { format!("{} / {}", clock(at), clock(st.duration)) } else { clock(at) };
                t.text(&said);
            }
        }
    });
}

/// The key of the hidden player that plays shader `shader`'s texture slot `slot` (a video file it samples).
fn player_key(shader: u64, slot: usize) -> u64 {
    PLAYER_BIT | ((slot as u64 & 0xF) << 48) | (shader & 0xFFFF_FFFF_FFFF)
}

/// Resolve the cell's Shaders against its ids, once the whole cell is filled: each uniform's lanes (an `@id`'s live
/// value), each texture's input (an `@id`'s frames or file, a video file's hidden player), and what does not resolve,
/// in words.
pub(super) fn resolve(b: &mut Budget<'_, '_>) {
    let Some(live) = b.live.as_deref_mut() else { return };
    let media = live.surf.media.clone();
    let m = media.as_ref().map(|m| m.borrow());
    let mut changed = false;
    for &e in &b.shaders {
        let Some(El::Shader(s)) = live.surf.els.get_mut(&e) else { continue };
        let mut problem: Option<String> = None;
        let mut slots = Vec::with_capacity(s.uniforms.len());
        for u in &s.uniforms {
            let Some(id) = &u.from else {
                slots.push(u.value);
                continue;
            };
            let x = match b.ids.get(id) {
                Some(Ref::Value(x)) => Some(*x as f32),
                Some(Ref::Video(k)) => Some(m.as_ref().and_then(|m| m.video(*k)).map_or(0.0, |st| st.time as f32)),
                Some(r) => {
                    problem.get_or_insert_with(|| {
                        format!(
                            "uniform \u{201C}{}\u{201D} reads @{id}, {}: a uniform reads a Slider, NumberField, Toggle, Checkbox or Video",
                            u.name,
                            r.said()
                        )
                    });
                    None
                }
                None => {
                    problem.get_or_insert_with(|| format!("uniform \u{201C}{}\u{201D} reads @{id}, but no element in this cell has id \u{201C}{id}\u{201D}", u.name));
                    None
                }
            };
            slots.push(x.map_or(u.value, |x| [x, 0.0, 0.0, 0.0]));
        }
        let mut inputs = Vec::with_capacity(s.textures.len());
        let mut players = Vec::new();
        for (slot, (name, t)) in s.textures.iter().enumerate() {
            let input = match t {
                Tex::Image(src) => Input::Photo(src.clone()),
                Tex::Video(src) => {
                    let key = player_key(e, slot);
                    players.push(VideoSpec { key, src: src.clone(), looped: true, muted: true, play: true, seek: None, visible: false });
                    Input::Surface(key)
                }
                Tex::Ref(id) => match b.ids.get(id) {
                    Some(Ref::Video(k) | Ref::Shader(k)) => Input::Surface(*k),
                    Some(Ref::Image(src)) => Input::Photo(src.clone()),
                    Some(r @ Ref::Value(_)) => {
                        problem.get_or_insert_with(|| {
                            format!("texture \u{201C}{name}\u{201D} reads @{id}, {}: a texture reads a Video, Image or Shader", r.said())
                        });
                        Input::None
                    }
                    None => {
                        problem.get_or_insert_with(|| format!("texture \u{201C}{name}\u{201D} reads @{id}, but no element in this cell has id \u{201C}{id}\u{201D}"));
                        Input::None
                    }
                },
            };
            inputs.push(input);
        }
        s.slots = slots;
        s.inputs = inputs;
        s.players = players;
        if s.problem != problem {
            // Said over the shader from the next frame.
            s.problem = problem;
            changed = true;
        }
    }
    if changed {
        live.anim = true;
    }
}

#[cfg(test)]
pub(super) fn resolved(live: &Live, e: u64) -> Option<(Vec<[f32; 4]>, Vec<Input>, Option<String>)> {
    match live.surf.els.get(&e)? {
        El::Shader(s) => Some((s.slots.clone(), s.inputs.clone(), s.problem.clone())),
        El::Video(_) => None,
    }
}

impl App {
    /// The host's Media (the renderer's), which the cells' Shaders and Videos are drawn by.
    pub fn set_media(&mut self, media: Rc<RefCell<Media>>) {
        self.genui.get_mut().surf.media = Some(media.clone());
        self.browser_media(media);
    }

    /// A press on a Shader or a Video (app.rs `actions`, `media:<what>:<entity>`): `play` plays or pauses a video
    /// (sending its onPlay or onPause; a play after it ended starts it again from the beginning), `mute` mutes or unmutes
    /// it, `resume` runs a paused shader again.
    pub(in crate::app) fn media_action(&mut self, a: &str) {
        let Some((what, e)) = a.split_once(':').and_then(|(w, e)| Some((w, e.parse::<u64>().ok()?))) else {
            eprintln!("harness-native: genui: media action {a:?} is not <play|mute|resume>:<entity>");
            return;
        };
        let live = self.genui.get_mut();
        let ended = live.surf.media.as_ref().and_then(|m| m.borrow().video(e)).is_some_and(|s| s.ended);
        let mut out = Vec::new();
        match (what, live.surf.els.get_mut(&e)) {
            ("play", Some(El::Video(v))) => {
                v.paused = !v.paused;
                if !v.paused {
                    v.played = true;
                    if ended {
                        v.seek = Some(0.0);
                        v.told = Some(0.0);
                    }
                }
                let h = if v.paused { &v.on_pause } else { &v.on_play };
                if let Some(h) = h {
                    out.push((v.cell.clone(), h.clone(), Value::Null));
                }
            }
            ("mute", Some(El::Video(v))) => v.muted = !v.muted,
            ("resume", Some(El::Shader(s))) => s.resume = true,
            _ => {
                eprintln!("harness-native: genui: media action {a:?}: entity {e} is no {} in the cells drawn", if what == "resume" { "Shader" } else { "Video" });
                return;
            }
        }
        for (cell, h, value) in out {
            self.link.ui_event(&cell, &h, value);
        }
        self.poke_why("video");
    }

    /// After the frame is painted: ask Media for each Shader and Video still drawn (a spec each into `d.media`, visible
    /// when its Surface was painted, `d.surface_keys`); step shader time (`dt`, only while visible and animating) and
    /// the pointer over each (`scale`: device pixels a point); send the videos' events; keep drawing only while
    /// something moves (a visible shader that animates, or reads the pointer as it moves over it; a visible video that
    /// plays; Media with a render or a frame in flight).
    pub(in crate::app) fn step_media(&mut self, d: &mut Draw, dt: f32, scale: f32) {
        let w = self.world.world_ref();
        let pointer = self.pointer;
        let now = self.time;
        // The pointer's target and the two nodes around it: over a shader is over its picture or its status.
        let hovered: Vec<u64> = w
            .entity_named(APP)
            .target(Hovered::id(), 0)
            .map(|t| std::iter::successors(Some(t), |n| n.parent()).take(3).map(|n| *n.id()).collect())
            .unwrap_or_default();
        let live = self.genui.get_mut();
        let Some(media) = live.surf.media.clone() else { return };
        let scrubbing = control::scrubbing(live);
        let surf = &mut live.surf;
        // Gone from the trees: no spec, so Media lets them go after its FORGET_SECS.
        surf.els.retain(|e, _| w.entity_from_id(*e).is_alive());
        let moved = surf.pointer_at != (pointer.x, pointer.y);
        surf.pointer_at = (pointer.x, pointer.y);
        let m = media.borrow();
        let (mut shading, mut playing) = (false, false);
        let mut out: Vec<(String, String, Value)> = Vec::new();
        for (&e, el) in &mut surf.els {
            let visible = d.surface_keys.contains(&e);
            match el {
                El::Shader(s) => {
                    let pic = View::of(w.entity_from_id(e)).part("pic").and_then(|p| Some((p.id(), p.entity().try_cloned::<&Laid>()?)));
                    let Some((pic_id, l)) = pic.filter(|(_, l)| l.w > 0.0 && l.h > 0.0) else { continue };
                    if visible && s.animate {
                        s.time += dt;
                        shading = true;
                    }
                    // Device pixels, each side at most MAX_PX (a bigger box renders at fewer pixels a point).
                    let sc = scale.min(MAX_PX / l.w).min(MAX_PX / l.h).max(1e-3);
                    let px = [(l.w * sc).round().clamp(1.0, MAX_PX) as u32, (l.h * sc).round().clamp(1.0, MAX_PX) as u32];
                    let inside = pointer.x >= l.x && pointer.x < l.x + l.w && pointer.y >= l.y && pointer.y < l.y + l.h;
                    let over = inside && hovered.iter().any(|h| *h == pic_id || *h == e);
                    if s.pointer && visible && (over != s.over || (over && moved)) {
                        shading = true;
                    }
                    s.over = over;
                    let at = if over {
                        [(pointer.x - l.x) * sc, (pointer.y - l.y) * sc, 1.0, f32::from(u8::from(pointer.down))]
                    } else {
                        [-1.0, -1.0, 0.0, 0.0]
                    };
                    let mut data = Vec::with_capacity(8 + 4 * s.slots.len().max(1));
                    data.extend_from_slice(&[px[0] as f32, px[1] as f32, sc, s.time]);
                    data.extend_from_slice(&at);
                    for slot in &s.slots {
                        data.extend_from_slice(slot);
                    }
                    if s.slots.is_empty() {
                        data.extend_from_slice(&[0.0; 4]);
                    }
                    d.media.push(MediaSpec::Shader(ShaderSpec {
                        key: e,
                        msl: s.msl.clone(),
                        msl_key: s.msl_key,
                        pixels: px,
                        data,
                        textures: s.inputs.clone(),
                        visible,
                        resume: std::mem::take(&mut s.resume),
                    }));
                    for p in &s.players {
                        d.media.push(MediaSpec::Video(VideoSpec { visible, ..p.clone() }));
                    }
                }
                El::Video(v) => {
                    let st = m.video(e).unwrap_or_default();
                    let tell = |v: &VideoEl, t: f64, out: &mut Vec<(String, String, Value)>| {
                        if let Some(h) = &v.on_time {
                            out.push((v.cell.clone(), h.clone(), serde_json::json!(t)));
                        }
                    };
                    // Scrubbed: it seeks as the rail is dragged (telling at most every TU_EVERY), and says where it
                    // stopped once let go.
                    match scrubbing.filter(|(n, _)| *n == e) {
                        Some((_, x)) => {
                            if v.scrub != Some(x) {
                                v.seek = Some(x);
                                v.scrub = Some(x);
                                if now - v.last_tu >= TU_EVERY {
                                    tell(v, x, &mut out);
                                    v.last_tu = now;
                                }
                            }
                        }
                        None => {
                            if let Some(x) = v.scrub.take() {
                                v.told = Some(x);
                            }
                        }
                    }
                    // It came to its end (not looping): it stays paused there, and says so once.
                    if st.ended && !v.was_ended && !v.looped {
                        v.paused = true;
                        if let Some(h) = &v.on_ended {
                            out.push((v.cell.clone(), h.clone(), Value::Null));
                        }
                    }
                    v.was_ended = st.ended;
                    v.played |= st.playing;
                    if let Some(t) = v.told.take() {
                        tell(v, t, &mut out);
                        v.last_tu = now;
                    } else if st.playing && v.scrub.is_none() && now - v.last_tu >= TU_EVERY {
                        tell(v, st.time, &mut out);
                        v.last_tu = now;
                    }
                    d.media.push(MediaSpec::Video(VideoSpec {
                        key: e,
                        src: v.src.clone(),
                        looped: v.looped,
                        muted: v.muted,
                        play: !v.paused,
                        seek: v.seek.take(),
                        visible,
                    }));
                    playing |= visible && !v.paused;
                }
            }
        }
        let busy = m.moving();
        drop(m);
        for (cell, h, value) in out {
            self.link.ui_event(&cell, &h, value);
        }
        if shading {
            self.poke_why("shader");
        }
        if playing {
            self.poke_why("video");
        }
        if busy {
            self.poke_why("media");
        }
    }
}
