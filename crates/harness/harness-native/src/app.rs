//! The app: one flecs world holding the model (harness-native-model: devices, objects, their relations) and the view (den's
//! UI primitives as entity trees, kept in step with the model by `sync_view`), the typesetter that sizes its words, the
//! terminal of the selected object, and the keyboard and pointer. The window (window.rs) or the offscreen shot (shot.rs)
//! asks it for a frame and hands it events.
//!
//! The frame loop is den's (den-native ui/native.rs): write `Viewport`, `PointerState` and `Paint`, typeset, `progress`,
//! typeset again while sizes move, then turn the painted nodes into Metal instances (`emit`).
use crate::draw::{ATLAS_SIZE, Draw};
use crate::link::Link;
use crate::pack::{Frame, GlyphInst, PanelInst};
use crate::term::{self, Grid, Mods, Sel};
use crate::text::{Face, Fonts, GlyphCache, Layout, SUBPX, Style, TypeLook};
use harness_native_api::spring::Tune;
use harness_native_api::{APP, Clock, HoverAmt, moving, settle};
use harness_native_flecs::prelude::*;
use harness_native_model::{self as model, Msg};
use harness_native_ui_draw_api::{Paint, PaintKind};
use harness_native_ui_input_api::{Action, Activated, Hovered, PointerState, Pressed};
use harness_native_ui_layout::typeset;
use harness_native_ui_layout_api::{Intrinsic, Size, SizeKind, Sizing, Viewport};
use harness_native_ui_look_api::{Edge, Fill, HoverLook, Lift, PressLook, Rgba, Shape, TextStyle};
use harness_native_ui_node_api::{Laid, Level};
use harness_native_ui_text_api::Text;
use harness_native_ui_view::{self as view, View, for_each};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

pub type Wake = Arc<dyn Fn() + Send + Sync>;

const TOKENS: &str = include_str!("../scene/00-tokens.flecs");
const HARNESS: &str = include_str!("../scene/10-harness.flecs");

/// The terminal's font size at zoom 1 (points), and its padding inside the main card.
const TERM_SIZE: f32 = 13.0;
const TERM_PAD: (f32, f32) = (14.0, 10.0);
const ZOOMS: [f32; 9] = [0.67, 0.75, 0.85, 0.92, 1.0, 1.1, 1.25, 1.4, 1.6];

/// The window's ground under the sidebar.
const GROUND: [f32; 4] = [0.043, 0.043, 0.047, 1.0];

#[derive(Clone, Copy, Debug, Default)]
pub struct KeyMods {
    pub cmd: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

/// What a palette entry does.
#[derive(Clone, Debug)]
enum Do {
    New(&'static str),
    Select(u64),
    Kill,
    Zoom(i32),
}

struct Palette {
    query: String,
    at: usize,
}

/// The view's fixed nodes, by entity id.
struct Ids {
    tree: u64,
    device: u64,
    main: u64,
    term: u64,
    palette: Option<u64>,
}

pub struct App {
    pub world: World,
    fonts: Fonts,
    cache: GlyphCache,
    layouts: HashMap<(String, u32, u32, bool, bool, u32), Rc<Layout>>,
    terms: model::Terms,
    /// The parsers of runs and stages being read, by entity id.
    run_terms: model::Terms,
    /// The run or stage entity whose output the link follows.
    watching: Option<u64>,
    /// A run (and stage) to select once the runs show it.
    pending_run: Option<(u64, Option<u32>)>,
    link: Link,
    grid: Grid,
    zoom: usize,
    ids: Ids,
    pointer: PointerState,
    time: f32,
    /// Frames to draw before the window may stop: anything that changed asks for a few, so layout settles.
    pending: u32,
    palette: Option<Palette>,
    /// The terminal's rectangle (layout points), from the last frame.
    term_rect: Option<[f32; 4]>,
    sel: Option<Sel>,
    selecting: bool,
    wheel: f32,
    /// Session ids attached since launch: an ended one is attached again only when it is still running.
    tried: HashMap<u64, f32>,
    /// The session a spawn made, to select once the list shows it.
    spawned: Arc<Mutex<Option<u64>>>,
    pub focused: bool,
    pub title: String,
}

/// The fill a tone prefab of the scene holds (`hn.ToneRunning`).
fn tone(w: WorldRef<'_>, name: &str) -> Fill {
    view::prefab(w, name).and_then(|p| p.try_cloned::<&Fill>()).unwrap_or_default()
}

fn rgba(c: Rgba) -> [f32; 4] {
    [c.r, c.g, c.b, c.a]
}

fn script_error(e: EntityView<'_>) -> Option<String> {
    e.try_get::<&flecs::Script>(|s| {
        // SAFETY: `error` is null or a NUL-terminated string owned by the EcsScript component, alive for this call.
        (!s.error.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(s.error) }.to_string_lossy().into_owned())
    })
    .flatten()
}

fn kind_glyph(kind: &str) -> &'static str {
    match kind {
        "claude" => "\u{2733}",
        "codex" => "\u{25C6}",
        "cursor" => "\u{25B2}",
        "shell" => "\u{276F}",
        "stage" => "\u{25C7}",
        _ => "\u{2022}",
    }
}

fn clip_text(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_owned() } else { format!("{}\u{2026}", s.chars().take(n - 1).collect::<String>()) }
}

/// `~` for the home directory at the front of a path.
fn tilde(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && p.starts_with(&h) => format!("~{}", &p[h.len()..]),
        _ => p.to_owned(),
    }
}

fn set_of(s: &TextStyle) -> Style {
    let size = if s.size > 0.0 { s.size } else { 14.0 };
    Style { size, bold: s.bold != 0, line: if s.line > 0.0 { s.line } else { (size * 1.3).round() } }
}

impl App {
    pub fn new(wake: Wake) -> Result<Self, String> {
        harness_native_ecs::init_os_api();
        harness_native_flecs::set_message_source(harness_native_ecs::last_error);
        // SAFETY: the OS API is set, and this binary links flecs (harness-native-ecs).
        let world = unsafe { World::init_host() };
        harness_native_ui_prims::register(&world);
        harness_native_api::register(&world);
        model::register(&world);
        let app = world.entity_named(APP);
        world.set(Clock::default());
        world.set(PointerState::default());
        world.set(Paint::default());
        let mut scripts: Vec<(&str, &str)> = vec![("00-tokens", TOKENS)];
        scripts.extend(harness_native_ui_prims::SCENES.iter().copied());
        scripts.push(("10-harness", HARNESS));
        for (name, src) in scripts {
            let e = world.script_named(&format!("scene::{name}")).build_from_code(src);
            if let Some(err) = script_error(*e) {
                return Err(format!("scene script {name}: {err}"));
            }
        }
        let w = world.world_ref();
        harness_native_ui_layout::install(w);
        harness_native_ui_input::install(w);
        harness_native_ui_motion::install(w);
        harness_native_ui_draw::install(w);
        // den-native-hover: each hoverable's hover spring toward whether the pointer is on it.
        let app_id = *app.id();
        world.system_named::<&mut HoverAmt>("Hover").each_iter(move |it, row, h| {
            let (e, w, dt) = (it.entity(row), it.world(), it.delta_time());
            let on = w.entity_from_id(app_id).target(Hovered::id(), 0).is_some_and(|t| t.id() == e.id());
            if settle(&mut h.spring, f32::from(u8::from(on)), Tune { omega: 34.0, zeta: 1.0 }, dt) {
                moving(&w);
            }
        });
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        let host = std::process::Command::new("scutil").args(["--get", "ComputerName"]).output().ok().and_then(|o| String::from_utf8(o.stdout).ok()).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).unwrap_or_else(|| "This Mac".into());
        let (terms, run_terms) = model::install(w, &host, &crate::link::sock().display().to_string(), rx);
        let link = Link::start(tx, wake);
        // The view's fixed frame: the window row, the sidebar (device row over the tree), the main card.
        let root = View::root(w, "hn.Window");
        let side = root.child("hn.Sidebar");
        let device = side.child("hn.DeviceRow");
        let tree = side.child("hn.Tree");
        let main = root.child("hn.Main");
        let term = main.part("term").map_or(0, View::id);
        let ids = Ids { tree: tree.id(), device: device.id(), main: main.id(), term, palette: None };
        let mut fonts = Fonts::new(TypeLook { code_scale: 1.0, ..TypeLook::default() });
        let grid = Grid::new(&mut fonts, TERM_SIZE, 2.0);
        Ok(Self {
            world,
            fonts,
            cache: GlyphCache::new(ATLAS_SIZE),
            layouts: HashMap::new(),
            terms,
            run_terms,
            watching: None,
            pending_run: None,
            link,
            grid,
            zoom: 4,
            ids,
            pointer: PointerState::default(),
            time: 0.0,
            pending: 4,
            palette: None,
            term_rect: None,
            sel: None,
            selecting: false,
            wheel: 0.0,
            tried: HashMap::new(),
            spawned: Arc::default(),
            focused: true,
            title: "harness".into(),
        })
    }

    pub fn zoom(&self) -> f32 {
        ZOOMS[self.zoom]
    }

    /// Something changed: draw a few frames.
    pub fn poke(&mut self) {
        self.pending = self.pending.max(3);
    }

    /// Whether the window may stop drawing until the next event.
    pub fn idle(&self) -> bool {
        self.pending == 0
    }

    // ---- model helpers ----

    fn objects(&self) -> Vec<u64> {
        model::tree(self.world.world_ref()).into_iter().map(|(e, _)| e).collect()
    }

    fn selected(&self) -> Option<u64> {
        model::selected(self.world.world_ref()).map(|e| *e.id())
    }

    fn session_of(&self, e: u64) -> Option<u64> {
        self.world.entity_from_id(e).try_cloned::<&model::Object>().map(|o| o.id)
    }

    fn entity_of(&self, sid: u64) -> Option<u64> {
        let mut found = None;
        self.world.new_query::<&model::Object>().each_entity(|e, o| {
            if o.id == sid {
                found = Some(*e.id());
            }
        });
        found
    }

    fn status_of(&self, e: u64) -> &'static str {
        let w = self.world.world_ref();
        let t = w.entity_from_id(e).target(model::Status::id(), 0).map_or(0, |t| *t.id());
        if t == *w.component_id::<model::Exited>() {
            "exited"
        } else if t == *w.component_id::<model::Lost>() {
            "lost"
        } else if t == *w.component_id::<model::Killed>() {
            "killed"
        } else if t == *w.component_id::<model::Pending>() {
            "pending"
        } else if t == *w.component_id::<model::Skipped>() {
            "skipped"
        } else {
            "running"
        }
    }

    /// A run (or stage): its number and stage index.
    fn run_of(&self, e: u64) -> Option<(u64, Option<u32>)> {
        let o = self.world.entity_from_id(e);
        if let Some(r) = o.try_cloned::<&model::Run>() {
            return Some((r.n, None));
        }
        let k = o.try_cloned::<&model::StageNo>()?.k;
        Some((o.parent()?.try_cloned::<&model::Run>()?.n, Some(k)))
    }

    /// A run's or stage's right-hand words: how it ended and how long it ran (or has been running).
    /// How long a run or stage ran, or has been running.
    fn run_took(&self, e: u64) -> Option<String> {
        let o = self.world.entity_from_id(e);
        let start = o.try_cloned::<&model::Created>().map(|c| c.ms).filter(|&m| m > 0)?;
        let end = o.try_cloned::<&model::Ended>().map(|c| c.ms);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        Some(fmt_dur(end.unwrap_or(now).saturating_sub(start)))
    }

    fn run_meta(&self, e: u64) -> String {
        let code = self.world.entity_from_id(e).try_cloned::<&model::ExitCode>().map(|c| c.code);
        let took = self.run_took(e);
        let status = self.status_of(e);
        let head = match (status, code) {
            ("pending" | "skipped" | "lost" | "killed", _) => Some(status.to_owned()),
            (_, Some(c)) if c != 0 => Some(format!("exit {c}")),
            _ => None,
        };
        match (head, took) {
            (Some(h), Some(t)) if status != "pending" && status != "skipped" => format!("{h} \u{00B7} {t}"),
            (Some(h), _) => h,
            (None, Some(t)) => t,
            (None, None) => String::new(),
        }
    }

    fn dot_tone(&self, e: u64) -> Fill {
        let w = self.world.world_ref();
        let code = w.entity_from_id(e).try_cloned::<&model::ExitCode>().map(|c| c.code);
        match self.status_of(e) {
            "running" => tone(w, "hn.ToneRunning"),
            "lost" => tone(w, "hn.ToneLost"),
            "killed" => tone(w, "hn.ToneFailed"),
            _ if code.is_some_and(|c| c != 0) => tone(w, "hn.ToneFailed"),
            _ => tone(w, "hn.ToneExited"),
        }
    }

    fn select(&mut self, e: Option<u64>) {
        if e != self.selected() {
            self.sel = None;
            model::select(self.world.world_ref(), e);
        }
        self.poke();
    }

    // ---- the view ----

    /// Keep the view's nodes in step with the model: the device row, one row per object, the main card's header, the
    /// palette.
    fn sync_view(&mut self) {
        let w = self.world.world_ref();
        let rows = model::tree(w);
        // Nothing selected (or it went): the first running object, else the first.
        if self.selected().is_none() && !rows.is_empty() {
            let first = rows.iter().find(|(e, _)| self.status_of(*e) == "running").or(rows.first()).map(|(e, _)| *e);
            model::select(w, first);
        }
        let pending = *self.spawned.lock().unwrap();
        if let Some(sid) = pending {
            if let Some(e) = self.entity_of(sid) {
                model::select(w, Some(e));
                *self.spawned.lock().unwrap() = None;
            }
        }
        if let Some((n, k)) = self.pending_run {
            let mut found = None;
            self.world.new_query::<&model::Run>().each_entity(|e, r| {
                if r.n == n {
                    found = Some(*e.id());
                }
            });
            let found = match (found, k) {
                (Some(run), Some(k)) => {
                    let mut stage = None;
                    w.entity_from_id(run).each_child(|c| {
                        if c.try_cloned::<&model::StageNo>().is_some_and(|s| s.k == k) {
                            stage = Some(*c.id());
                        }
                    });
                    stage
                }
                (run, _) => run,
            };
            if found.is_some() {
                model::select(w, found);
                self.pending_run = None;
            }
        }
        let sel = self.selected();
        let (clear, chosen) = (tone(w, "hn.ToneClear"), tone(w, "hn.ToneSelected"));
        // The device row.
        let dev = w.lookup(model::LOCAL);
        let device = View::of(w.entity_from_id(self.ids.device));
        let host = dev.try_cloned::<&model::Device>().map(|d| d.host.as_str().to_owned()).unwrap_or_default();
        let link = dev.target(model::Link::id(), 0).map_or(0, |t| *t.id());
        let (link_tone, link_word) = if link == *w.component_id::<model::Up>() {
            (tone(w, "hn.ToneRunning"), format!("{}", rows.len()))
        } else if link == *w.component_id::<model::Down>() {
            (tone(w, "hn.ToneFailed"), "offline".to_owned())
        } else {
            (tone(w, "hn.ToneLost"), "connecting".to_owned())
        };
        if let Some(n) = device.part("name") {
            n.text(&clip_text(&host, 26));
        }
        if let Some(n) = device.part("count") {
            n.text(&link_word);
        }
        if let Some(d) = device.part("dot") {
            set_if(d, link_tone);
        }
        // One row per session, run and stage, with a fold row over each session's older runs.
        let tree = View::of(w.entity_from_id(self.ids.tree));
        let hint: HashMap<u64, usize> = rows.iter().enumerate().map(|(i, &(e, _))| (e, i)).collect();
        let info: Vec<(String, &str, model::Row)> = model::rows(w)
            .into_iter()
            .map(|r| match r {
                model::Row::Obj { e, .. } if self.run_of(e).is_some() => (format!("o{e}"), "hn.RunRow", r),
                model::Row::Obj { e, .. } => (format!("o{e}"), "hn.ObjRow", r),
                model::Row::More { parent, .. } => (format!("more{parent}"), "hn.MoreRow", r),
            })
            .collect();
        let me = &*self;
        let indent = |row: View<'_>, depth: usize| {
            if let Some(p) = row.part("indent") {
                let s = Size { w: Sizing { kind: SizeKind::Fixed, value: depth as f32 * 14.0, min: 0.0, max: 0.0 }, h: Sizing { kind: SizeKind::Fixed, value: 1.0, min: 0.0, max: 0.0 } };
                if p.entity().try_cloned::<&Size>() != Some(s) {
                    p.set(s);
                }
            }
        };
        view::for_each_of(tree, info, |row, r, new| {
            if new {
                row.set(HoverAmt::default());
            }
            let (e, depth) = match r {
                model::Row::Obj { e, depth } => (e, depth),
                model::Row::More { parent, hidden, depth, unfolded } => {
                    row.action(&format!("fold:{parent}"));
                    indent(row, depth);
                    if let Some(p) = row.part("glyph") {
                        p.text(if unfolded { "\u{25B4}" } else { "\u{25BE}" });
                    }
                    if let Some(p) = row.part("name") {
                        p.text(&if unfolded { "fewer runs".to_owned() } else { format!("{hidden} more run{}", if hidden == 1 { "" } else { "s" }) });
                    }
                    return;
                }
            };
            let o = w.entity_from_id(e);
            row.action(&format!("select:{e}"));
            set_if(row, if sel == Some(e) { chosen } else { clear });
            indent(row, depth);
            let kind = model::kind_name(o);
            let run = me.run_of(e);
            if let Some(p) = row.part("glyph") {
                p.text(match run {
                    Some((_, Some(_))) => "\u{2502}",
                    Some(_) => "$",
                    None => kind_glyph(kind),
                });
            }
            let code = o.try_cloned::<&model::ExitCode>().map(|c| c.code);
            let meta = match (run, me.status_of(e), code) {
                (Some(_), ..) => me.run_meta(e),
                (None, "lost", _) => "lost".to_owned(),
                (None, "exited", Some(c)) if c != 0 => format!("exit {c}"),
                (None, "exited", _) => "done".to_owned(),
                _ => hint.get(&e).filter(|&&i| i < 9).map(|i| format!("\u{2318}{}", i + 1)).unwrap_or_default(),
            };
            let name = o.try_cloned::<&model::Name>().map(|n| n.s.as_str().to_owned()).unwrap_or_default();
            if let Some(p) = row.part("name") {
                // A run's command on one line; the code face is wider, so it gets fewer characters, and fewer again
                // beside long right-hand words (`killed · 1m04s`).
                let (name, room) = match run {
                    Some(_) => (name.split_whitespace().collect::<Vec<_>>().join(" "), 19usize.saturating_sub(meta.chars().count().saturating_sub(6) * 2 / 3)),
                    None => (name, 26usize),
                };
                p.text(&clip_text(&name, room.saturating_sub(depth * 2).max(8)));
            }
            if let Some(p) = row.part("meta") {
                p.text(&meta);
            }
            if let Some(p) = row.part("dot") {
                set_if(p, me.dot_tone(e));
            }
        });
        // The main card's header.
        let main = View::of(w.entity_from_id(self.ids.main));
        let head = main.part("head");
        let part = |n: &str| head.and_then(|h| h.part(n));
        let (glyph, title, sub, state, dot, window) = match sel.map(|e| w.entity_from_id(e)) {
            Some(o) => {
                let kind = model::kind_name(o);
                let name = o.try_cloned::<&model::Name>().map(|n| n.s.as_str().to_owned()).unwrap_or_default();
                let t = o.try_cloned::<&model::Title>().map(|n| n.s.as_str().to_owned()).unwrap_or_default();
                let cwd = o.try_cloned::<&model::Cwd>().map(|n| tilde(n.s.as_str())).unwrap_or_default();
                let sub = if t.is_empty() || t == name { cwd } else { t };
                let code = o.try_cloned::<&model::ExitCode>().map(|c| c.code);
                let e = *o.id();
                let state = match (self.run_of(e), self.status_of(e), code) {
                    (Some(_), s, c) => {
                        let s = if s == "exited" { format!("exited {}", c.unwrap_or(0)) } else { s.to_owned() };
                        match self.run_took(e) {
                            Some(t) if s != "pending" && s != "skipped" => format!("{s} \u{00B7} {t}"),
                            _ => s,
                        }
                    }
                    (None, "exited", Some(c)) => format!("exited {c}"),
                    (None, s, _) => s.to_owned(),
                };
                let (glyph, title, sub) = match self.run_of(e) {
                    // A run: its id and command (one line), its directory under it.
                    Some((n, stage)) => {
                        let id = stage.map_or(format!("r{n}"), |k| format!("r{n}.{k}"));
                        let cmd = name.split_whitespace().collect::<Vec<_>>().join(" ");
                        let cwd = o.try_cloned::<&model::Cwd>().or_else(|| o.parent().and_then(|p| p.try_cloned::<&model::Cwd>())).map(|c| tilde(c.s.as_str())).unwrap_or_default();
                        ("$", clip_text(&format!("{id}  {cmd}"), 70), cwd)
                    }
                    None => (kind_glyph(kind), name.clone(), sub),
                };
                let window = format!("{} \u{2014} harness", clip_text(&title, 60));
                (glyph, title, clip_text(&sub, 70), state, self.dot_tone(e), window)
            }
            None => ("", "No objects".to_owned(), "\u{2318}T new claude   \u{2318}\u{21E7}T new codex   \u{2318}K palette".to_owned(), String::new(), tone(w, "hn.ToneExited"), "harness".to_owned()),
        };
        if let Some(p) = part("glyph") {
            p.text(glyph);
        }
        if let Some(p) = part("title") {
            p.text(&title);
        }
        if let Some(p) = part("sub") {
            p.text(&sub);
        }
        if let Some(p) = part("state") {
            p.text(&state);
        }
        if let Some(p) = part("dot") {
            set_if(p, dot);
        }
        self.title = window;
        self.sync_palette();
    }

    /// The palette's entries for its query: new objects, then each object, then a few commands.
    fn entries(&self) -> Vec<(String, &'static str, String, String, Do)> {
        let w = self.world.world_ref();
        let mut out: Vec<(String, &'static str, String, String, Do)> = vec![
            ("new-claude".into(), kind_glyph("claude"), "New claude".into(), "\u{2318}T".into(), Do::New("claude")),
            ("new-codex".into(), kind_glyph("codex"), "New codex".into(), "\u{2318}\u{21E7}T".into(), Do::New("codex")),
            ("new-shell".into(), kind_glyph("shell"), "New shell".into(), String::new(), Do::New("shell")),
        ];
        for (i, e) in self.objects().into_iter().enumerate() {
            let o = w.entity_from_id(e);
            let name = o.try_cloned::<&model::Name>().map(|n| n.s.as_str().to_owned()).unwrap_or_default();
            let hint = if i < 9 { format!("\u{2318}{}", i + 1) } else { String::new() };
            out.push((format!("obj{e}"), kind_glyph(model::kind_name(o)), name, hint, Do::Select(e)));
        }
        out.push(("kill".into(), "\u{2715}", "Kill current object".into(), "\u{2318}W".into(), Do::Kill));
        out.push(("zoom-in".into(), "+", "Zoom in".into(), "\u{2318}=".into(), Do::Zoom(1)));
        out.push(("zoom-out".into(), "\u{2212}", "Zoom out".into(), "\u{2318}\u{2212}".into(), Do::Zoom(-1)));
        let q = self.palette.as_ref().map(|p| p.query.to_lowercase()).unwrap_or_default();
        out.retain(|(_, _, name, _, _)| fuzzy(&name.to_lowercase(), &q));
        out
    }

    fn sync_palette(&mut self) {
        let w = self.world.world_ref();
        match (&self.palette, self.ids.palette) {
            (None, Some(id)) => {
                w.entity_from_id(id).destruct();
                self.ids.palette = None;
            }
            (Some(p), _) => {
                let root = match self.ids.palette {
                    Some(id) => View::of(w.entity_from_id(id)),
                    None => {
                        let v = View::root(w, "hn.Palette");
                        self.ids.palette = Some(v.id());
                        v
                    }
                };
                let card = root.part("card");
                let shown = if p.query.is_empty() { "Search objects and commands\u{2026}".to_owned() } else { p.query.clone() };
                if let Some(t) = card.and_then(|c| c.part("query")).and_then(|q| q.part("text")) {
                    t.text(&shown);
                }
                let entries = self.entries();
                let at = p.at.min(entries.len().saturating_sub(1));
                let (clear, chosen) = (tone(w, "hn.ToneClear"), tone(w, "hn.ToneSelected"));
                if let Some(items) = card.and_then(|c| c.part("items")) {
                    let rows = entries.into_iter().enumerate().take(12).map(|(i, (k, g, n, h, _))| (k, (i, g, n, h)));
                    for_each(items, "hn.PalRow", rows, |row, (i, g, n, h), new| {
                        if new {
                            row.set(HoverAmt::default());
                        }
                        row.action(&format!("pal:{i}"));
                        set_if(row, if i == at { chosen } else { clear });
                        if let Some(p) = row.part("glyph") {
                            p.text(g);
                        }
                        if let Some(p) = row.part("name") {
                            p.text(&n);
                        }
                        if let Some(p) = row.part("hint") {
                            p.text(&h);
                        }
                    });
                }
            }
            _ => {}
        }
    }

    // ---- the frame ----

    /// One frame at `points` (the window's size in points) on a display of `backing` pixels a point.
    pub fn frame(&mut self, points: (f32, f32), backing: f32, dt: f32, upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8])) -> Draw {
        self.time += dt;
        self.pending = self.pending.saturating_sub(1);
        let z = self.zoom();
        let view = (points.0 / z, points.1 / z);
        let scale = backing * z;
        if self.grid.scale != scale {
            // Cells are whole device pixels: new metrics at a new scale (the pane's grid may change size with them).
            self.grid = Grid::new(&mut self.fonts, TERM_SIZE, scale);
        }
        let w = self.world.world_ref();
        if w.try_get::<&Viewport>(|v| (v.w, v.h)) != Some(view) {
            self.world.set(Viewport { w: view.0, h: view.1 });
        }
        self.world.set(self.pointer);
        let next = w.try_get::<&Paint>(|p| p.frame).unwrap_or(0).wrapping_add(1);
        self.world.set(Paint { frame: next });
        self.world.get::<&mut Clock>(|c| {
            c.time += dt;
            c.moving = false;
        });
        let before = self.world.try_get::<&model::Inbox>(|i| i.applied);
        self.sync_view();
        self.typeset();
        self.world.progress_time(dt);
        if self.world.try_get::<&model::Inbox>(|i| i.applied) != before {
            // The daemon said something: the view follows in this frame.
            self.sync_view();
            self.poke();
        }
        for _ in 0..3 {
            if self.typeset() == 0 {
                break;
            }
            self.world.progress_time(0.0);
            self.poke();
        }
        self.actions();
        harness_native_ui_input::end_frame(self.world.world_ref());
        self.pointer.went_down = false;
        self.pointer.went_up = false;
        if self.world.try_get::<&Clock>(|c| c.moving).unwrap_or(false) {
            self.poke();
        }
        self.sync_term();
        let mut d = Draw {
            frame: Frame { view: [view.0, view.1], scale, time: self.time, dark: 1.0, live: scale, textfx: [1.0, 0.0], tune: [0.82, 1.12, 1.0, 0.0], slug: [0.0; 4] },
            clear: GROUND,
            ..Draw::default()
        };
        self.emit(&mut d, scale, upload);
        if self.cache.overflowed() {
            self.cache.clear();
            self.poke();
        }
        if self.layouts.len() > 4096 {
            self.layouts.clear();
        }
        d
    }

    /// Size every `Text` node with the typesetter; how many changed.
    fn typeset(&mut self) -> usize {
        let wanted = typeset::wanted(self.world.world_ref());
        let sizes: Vec<Intrinsic> = wanted
            .iter()
            .map(|t| {
                let one = self.layout(&t.text, &t.style, 1.0e5);
                let w = one.lines.iter().map(|l| l.width).fold(0.0, f32::max).ceil();
                let at = t.at.unwrap_or(w);
                let lay = if at + 0.5 >= w { one } else { self.layout(&t.text, &t.style, at) };
                let st = set_of(&t.style);
                let h = lay.lines.last().map_or(st.line, |l| l.top + l.height).max(st.line);
                Intrinsic { w, h, at }
            })
            .collect();
        typeset::apply(self.world.world_ref(), &wanted, &sizes)
    }

    fn layout(&mut self, text: &str, style: &TextStyle, width: f32) -> Rc<Layout> {
        let st = set_of(style);
        let code = style.role == 9;
        let key = (text.to_owned(), st.size.to_bits(), st.line.to_bits(), st.bold, code, width.to_bits());
        if let Some(l) = self.layouts.get(&key) {
            return l.clone();
        }
        let lay = Rc::new(if code { Layout::in_face(&mut self.fonts, text, Face::Code, st, width) } else { Layout::new(&mut self.fonts, text, st, width) });
        self.layouts.insert(key, lay.clone());
        lay
    }

    /// What was activated this frame: a row selects its object, a palette entry runs.
    fn actions(&mut self) {
        let mut acts = Vec::new();
        self.world.new_query::<&Action>().each_entity(|e, a| {
            if e.has(Activated::id()) {
                acts.push(a.name.as_str().to_owned());
            }
        });
        for a in acts {
            self.poke();
            if let Some(e) = a.strip_prefix("select:").and_then(|s| s.parse().ok()) {
                self.select(Some(e));
            } else if let Some(p) = a.strip_prefix("fold:").and_then(|s| s.parse().ok()) {
                model::toggle_fold(self.world.world_ref(), p);
            } else if a == "pal:close" {
                self.palette = None;
            } else if let Some(i) = a.strip_prefix("pal:").and_then(|s| s.parse::<usize>().ok()) {
                if let Some((.., d)) = self.entries().into_iter().nth(i) {
                    self.palette = None;
                    self.run(d);
                }
            }
        }
    }

    fn run(&mut self, d: Do) {
        match d {
            Do::New(kind) => self.spawn(kind),
            Do::Select(e) => self.select(Some(e)),
            Do::Kill => self.kill(),
            Do::Zoom(by) => self.step_zoom(by),
        }
        self.poke();
    }

    // ---- the terminal ----

    /// Attach the selected object's terminal, and keep its grid the size of the pane.
    fn sync_term(&mut self) {
        let Some(e) = self.selected() else { return };
        if self.watching.is_some_and(|w| w != e) {
            // Another object took the pane: stop following the run it showed.
            self.link.unwatch_run();
            if let Some(w) = self.watching.take() {
                self.run_terms.borrow_mut().remove(&w);
            }
        }
        let laid = self.world.entity_from_id(self.ids.term).try_cloned::<&Laid>();
        let Some(l) = laid.filter(|l| l.w > 0.0 && l.h > 0.0) else { return };
        let rect = [l.x + TERM_PAD.0, l.y + TERM_PAD.1, l.w - 2.0 * TERM_PAD.0, l.h - 2.0 * TERM_PAD.1];
        self.term_rect = Some(rect);
        let (cols, rows) = self.grid.fit(rect[2], rect[3]);
        let o = self.world.entity_from_id(e);
        if let Some((n, stage)) = self.run_of(e) {
            // A run (or stage): its output, read-only, from the start into a fresh parser the pane's size.
            if self.watching != Some(e) {
                self.watching = Some(e);
                self.run_terms.borrow_mut().remove(&e);
                o.set(model::Term { cols, rows, scroll: 0, seq: 0 });
                self.link.watch_run(n, stage);
                return;
            }
            if let Some(mut t) = o.try_cloned::<&model::Term>().filter(|t| (t.cols, t.rows) != (cols, rows)) {
                t.cols = cols;
                t.rows = rows;
                o.set(t);
                if let Some(p) = self.run_terms.borrow_mut().get_mut(&e) {
                    p.resize(cols, rows);
                }
            }
            return;
        }
        let Some(sid) = self.session_of(e) else { return };
        let running = self.status_of(e) == "running";
        // Attach when it is not: a running object again a second after its last try, an ended one once (its last screen).
        let last = self.tried.get(&sid).copied();
        let due = if running { last.is_none_or(|t| self.time - t > 1.0) } else { last.is_none() };
        if !self.link.attached(sid) && due {
            self.tried.insert(sid, self.time);
            // A fresh parser for the snapshot the attach starts with.
            self.terms.borrow_mut().remove(&sid);
            o.set(model::Term { cols, rows, scroll: 0, seq: 0 });
            self.link.attach(sid, cols, rows);
            return;
        }
        let Some(mut t) = o.try_cloned::<&model::Term>() else { return };
        if (t.cols, t.rows) != (cols, rows) {
            t.cols = cols;
            t.rows = rows;
            o.set(t);
            if let Some(p) = self.terms.borrow_mut().get_mut(&sid) {
                p.resize(cols, rows);
            }
            self.link.resize(sid, cols, rows);
        }
    }

    /// The selected object's parser map and key in it: a session's by its id, a run's or stage's by its entity.
    fn parser_key(&self) -> Option<(&model::Terms, u64)> {
        let e = self.selected()?;
        match self.session_of(e) {
            Some(sid) => Some((&self.terms, sid)),
            None => self.run_of(e).map(|_| (&self.run_terms, e)),
        }
    }

    fn with_parser<R>(&self, f: impl FnOnce(&mut harness_vt::Term, u64) -> R) -> Option<R> {
        let (terms, key) = self.parser_key()?;
        let mut map = terms.borrow_mut();
        map.get_mut(&key).map(|p| f(p, key))
    }

    fn scroll(&self) -> usize {
        self.selected().and_then(|e| self.world.entity_from_id(e).try_cloned::<&model::Term>()).map_or(0, |t| t.scroll as usize)
    }

    fn set_scroll(&mut self, s: usize) {
        if let Some(e) = self.selected() {
            let o = self.world.entity_from_id(e);
            if let Some(mut t) = o.try_cloned::<&model::Term>() {
                if t.scroll as usize != s {
                    t.scroll = s as u32;
                    o.set(t);
                }
            }
        }
        self.poke();
    }

    // ---- drawing ----

    fn emit(&mut self, d: &mut Draw, sc: f32, upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8])) {
        let held = self.world.lookup(APP).target(Pressed::id(), 0).map(|t| *t.id());
        let painted = harness_native_ui_draw::painted(self.world.world_ref());
        for (id, p) in painted {
            let w = self.world.world_ref();
            let e = w.entity_from_id(id);
            let Some(laid) = e.try_cloned::<&Laid>() else { continue };
            if p.clipped && (p.clip_w <= 0.0 || p.clip_h <= 0.0) {
                continue;
            }
            let rect = [laid.x, laid.y, laid.w, laid.h];
            let (np, ng) = (d.panels.len(), d.glyphs.len());
            match p.kind {
                PaintKind::Plate => {
                    let fill = e.try_cloned::<&Fill>().unwrap_or_default();
                    let edge = e.try_cloned::<&Edge>().unwrap_or_default();
                    let mut shape = e.try_cloned::<&Shape>().unwrap_or_default();
                    if shape.n <= 0.0 {
                        shape.n = 4.0;
                    }
                    let mut panel = panel_from(rgba(fill.dark), rgba(edge.dark), shape, rect, p.alpha);
                    if let Some(l) = e.try_cloned::<&Lift>() {
                        panel.fx = [l.blur, l.alpha, 0.0, 0.0];
                        panel.fx2[0] = l.dy;
                    }
                    if fill.dark.a > 0.0 || edge.dark.a > 0.0 || panel.fx[1] > 0.0 {
                        d.panel(panel);
                    }
                    let wash = |c: Rgba, r: [f32; 4], radius: f32, a: f32| panel_from(rgba(c), [0.0; 4], Shape { radius, n: shape.n, border: 0.0, feather: 0.0 }, r, p.alpha * a);
                    let hover = e.try_cloned::<&HoverLook>().zip(e.try_cloned::<&HoverAmt>().map(|h| h.spring.x.clamp(0.0, 1.0)));
                    if let Some((l, h)) = hover.filter(|(_, h)| *h > 0.004) {
                        let r = [rect[0] - l.pad, rect[1] - l.pad, rect[2] + 2.0 * l.pad, rect[3] + 2.0 * l.pad];
                        d.panel(wash(l.dark, r, if l.radius < 0.0 { shape.radius + l.pad } else { l.radius }, h));
                    }
                    if let Some(l) = e.try_cloned::<&PressLook>().filter(|_| held == Some(id)) {
                        d.panel(wash(l.dark, rect, shape.radius, 1.0));
                    }
                    if id == self.ids.term {
                        self.emit_term(d, sc, upload);
                    }
                }
                PaintKind::Glyphs => {
                    let Some(text) = e.try_cloned::<&Text>() else { continue };
                    let style = e.try_cloned::<&TextStyle>().unwrap_or_default();
                    let ink = e.try_cloned::<&Fill>().map_or([0.9, 0.9, 0.92, 1.0], |f| rgba(f.dark));
                    let lay = self.layout(text.s.as_str(), &style, laid.w.max(1.0) + 0.5);
                    emit_layout(d, &self.fonts, &mut self.cache, &lay, (laid.x, laid.y), ink, p.alpha, sc, upload);
                }
                PaintKind::Icon | PaintKind::Picture | PaintKind::Ribbon => {}
            }
            if p.clipped {
                let band = [p.clip_y, p.clip_y + p.clip_h];
                for panel in &mut d.panels[np..] {
                    panel.clip[..2].copy_from_slice(&band);
                }
                for glyph in &mut d.glyphs[ng..] {
                    glyph.clip[..2].copy_from_slice(&band);
                }
            }
            let _ = Level::Base;
        }
    }

    fn emit_term(&mut self, d: &mut Draw, sc: f32, upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8])) {
        let Some(rect) = self.term_rect else { return };
        let Some((terms, key)) = self.parser_key() else { return };
        let terms = terms.clone();
        let scroll = self.scroll();
        let mut map = terms.borrow_mut();
        let Some(parser) = map.get_mut(&key) else { return };
        parser.set_scrollback(scroll);
        let scroll = parser.scrollback();
        let snap = term::snapshot(parser, &term::DARK);
        parser.set_scrollback(0);
        drop(map);
        let paint = term::Paint { origin: (rect[0], rect[1]), scale: sc, focused: self.focused && self.palette.is_none(), sel: self.sel, scroll, theme: &term::DARK, time: self.time };
        term::draw(d, &mut self.grid, &mut self.fonts, &mut self.cache, &snap, &paint, upload);
    }

    // ---- input ----

    /// The pointer moved to `(x, y)` (window points).
    pub fn pointer_move(&mut self, x: f32, y: f32) {
        let z = self.zoom();
        self.pointer.x = x / z;
        self.pointer.y = y / z;
        if self.selecting {
            let scroll = self.scroll() as i64;
            if let (Some(s), Some(c)) = (self.sel.as_mut(), cell_at(self.term_rect, &self.grid, self.pointer.x, self.pointer.y, 0)) {
                s.b = (c.0 - scroll, c.1);
            }
        }
        self.poke();
    }

    pub fn pointer_down(&mut self, x: f32, y: f32) {
        self.pointer_move(x, y);
        self.pointer.down = true;
        self.pointer.went_down = true;
        if self.palette.is_none()
            && let Some(c) = cell_at(self.term_rect, &self.grid, self.pointer.x, self.pointer.y, 0)
        {
            let scroll = self.scroll() as i64;
            let at = (c.0 - scroll, c.1);
            self.sel = Some(Sel { a: at, b: at });
            self.selecting = true;
        }
    }

    pub fn pointer_up(&mut self) {
        self.pointer.down = false;
        self.pointer.went_up = true;
        if self.selecting {
            self.selecting = false;
            if self.sel.is_some_and(|s| s.a == s.b) {
                self.sel = None;
            }
        }
        self.poke();
    }

    /// A scroll of `dy` points (positive: back into the history).
    pub fn wheel(&mut self, dy: f32, precise: bool) {
        let lines = if precise { dy / self.grid.ch } else { dy };
        self.wheel += lines;
        let n = self.wheel.trunc();
        if n == 0.0 {
            return;
        }
        self.wheel -= n;
        let alt = self.with_parser(|p, _| (p.alternate_screen(), p.application_cursor())).unwrap_or((false, false));
        if alt.0 {
            // A full-screen program: the wheel is its arrow keys, as Terminal's alternate scroll does.
            let key = if n > 0.0 { 126 } else { 125 };
            let bytes = term::encode_key(key, "", "", Mods::default(), alt.1).repeat(n.abs() as usize);
            self.send(&bytes);
            return;
        }
        let max = self.with_parser(|p, _| p.history());
        let next = (self.scroll() as f32 + n).clamp(0.0, max.unwrap_or(0) as f32) as usize;
        self.set_scroll(next);
    }

    fn send(&mut self, bytes: &[u8]) {
        if let Some(sid) = self.selected().and_then(|e| self.session_of(e)) {
            self.link.input(sid, bytes);
        }
    }

    /// A key typed: into the palette when it is open, else the terminal.
    pub fn key(&mut self, code: u16, chars: &str, bare: &str, m: KeyMods) {
        self.poke();
        if self.palette.is_some() {
            return self.palette_key(code, chars);
        }
        if chars.chars().next().is_some_and(|c| ('\u{F700}'..='\u{F8FF}').contains(&c)) && !matches!(code, 96..=126) {
            return;
        }
        let app_cursor = self.with_parser(|p, _| p.application_cursor()).unwrap_or(false);
        let bytes = term::encode_key(code, chars, bare, Mods { shift: m.shift, ctrl: m.ctrl, alt: m.alt }, app_cursor);
        if bytes.is_empty() {
            return;
        }
        self.sel = None;
        self.set_scroll(0);
        self.send(&bytes);
    }

    fn palette_key(&mut self, code: u16, chars: &str) {
        let n = self.entries().len();
        let Some(p) = self.palette.as_mut() else { return };
        match code {
            53 => self.palette = None,
            125 => p.at = (p.at + 1).min(n.saturating_sub(1)),
            126 => p.at = p.at.saturating_sub(1),
            36 | 76 => {
                let at = p.at;
                if let Some((.., d)) = self.entries().into_iter().nth(at) {
                    self.palette = None;
                    self.run(d);
                }
            }
            51 => {
                p.query.pop();
                p.at = 0;
            }
            _ => {
                let typed: String = chars.chars().filter(|c| !c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(c)).collect();
                if !typed.is_empty() {
                    p.query.push_str(&typed);
                    p.at = 0;
                }
            }
        }
    }

    /// A Command shortcut: whether it was one of the app's.
    pub fn shortcut(&mut self, key: &str, code: u16, m: KeyMods) -> bool {
        self.poke();
        let k = key.to_lowercase();
        match (k.as_str(), m.shift) {
            ("]", _) | ("}", _) => self.step_object(1),
            ("[", _) | ("{", _) => self.step_object(-1),
            ("t", false) => self.spawn("claude"),
            ("t", true) => self.spawn("codex"),
            ("w", false) => self.kill(),
            ("s", false) => self.toggle_sidebar(),
            ("k", false) => {
                self.palette = if self.palette.is_some() { None } else { Some(Palette { query: String::new(), at: 0 }) };
            }
            ("=", _) | ("+", _) => self.step_zoom(1),
            ("-", _) | ("_", _) => self.step_zoom(-1),
            ("0", false) => self.step_zoom(0),
            (d, false) if d.len() == 1 && ('1'..='9').contains(&d.chars().next().unwrap_or('0')) => {
                let i = d.parse::<usize>().unwrap_or(1) - 1;
                if let Some(&e) = self.objects().get(i) {
                    self.select(Some(e));
                }
            }
            _ => {
                let _ = code;
                return false;
            }
        }
        true
    }

    /// Command-S: fold the sidebar away (zero wide, clipped, so its rows neither draw nor take clicks; the main card
    /// takes the window) or bring it back at the scene's size.
    fn toggle_sidebar(&mut self) {
        use harness_native_ui_layout_api::Padding;
        use harness_native_ui_node_api::Clip;
        let w = self.world.world_ref();
        let Some(side) = w.entity_from_id(self.ids.device).parent() else { return };
        let Some(proto) = view::prefab(w, "hn.Sidebar") else { return };
        let (Some(mut size), Some(full), Some(pad)) = (side.try_cloned::<&Size>(), proto.try_cloned::<&Size>(), proto.try_cloned::<&Padding>()) else { return };
        let show = size.w.value == 0.0;
        size.w = if show { full.w } else { Sizing { kind: SizeKind::Fixed, value: 0.0, min: 0.0, max: 0.0 } };
        side.set(size);
        side.set(if show { pad } else { Padding::default() });
        if show {
            side.remove(Clip::id());
        } else {
            side.add(Clip::id());
        }
        // The window keeps no left margin beside the sidebar; without it the main card gets the same margin as its other sides.
        if let (Some(root), Some(win)) = (side.parent(), view::prefab(w, "hn.Window").and_then(|p| p.try_cloned::<&Padding>())) {
            root.set(Padding { l: if show { win.l } else { win.r }, ..win });
        }
    }

    fn step_object(&mut self, by: i64) {
        let objs = self.objects();
        if objs.is_empty() {
            return;
        }
        let at = self.selected().and_then(|s| objs.iter().position(|&e| e == s)).map_or(0, |i| i as i64);
        let n = objs.len() as i64;
        self.select(Some(objs[((at + by) % n + n) as usize % objs.len()]));
    }

    pub fn step_zoom(&mut self, by: i32) {
        self.zoom = match by {
            0 => 4,
            b if b > 0 => (self.zoom + 1).min(ZOOMS.len() - 1),
            _ => self.zoom.saturating_sub(1),
        };
        self.poke();
    }

    fn spawn(&mut self, kind: &'static str) {
        let cwd = self.selected().and_then(|e| self.world.entity_from_id(e).try_cloned::<&model::Cwd>()).map(|c| c.s.as_str().to_owned()).filter(|c| !c.is_empty()).or_else(|| std::env::var("HOME").ok());
        let (cols, rows) = self.term_rect.map_or((120, 40), |r| self.grid.fit(r[2], r[3]));
        let slot = self.spawned.clone();
        self.link.spawn(kind, cwd, cols, rows, move |id| *slot.lock().unwrap() = Some(id));
    }

    fn kill(&mut self) {
        let Some(e) = self.selected() else { return };
        let Some(sid) = self.session_of(e) else { return };
        // The next object takes its place.
        let objs = self.objects();
        let next = objs.iter().position(|&o| o == e).and_then(|i| objs.get(i + 1).or_else(|| i.checked_sub(1).and_then(|j| objs.get(j)))).copied();
        self.link.kill(sid);
        self.select(next);
    }

    /// The text of the terminal's selection, for the pasteboard.
    pub fn copy(&self) -> Option<String> {
        let sel = self.sel?;
        let scroll = self.scroll() as i64;
        self.with_parser(|p, _| {
            let rows = i64::from(p.size().1);
            let ((r0, c0), (r1, c1)) = sel.ordered();
            let (r0, r1) = ((r0 + scroll).clamp(0, rows - 1) as u16, (r1 + scroll).clamp(0, rows - 1) as u16);
            p.set_scrollback(scroll as usize);
            let text = p.contents_between(r0, c0, r1, c1.saturating_add(1));
            p.set_scrollback(0);
            text.lines().map(str::trim_end).collect::<Vec<_>>().join("\n")
        })
        .filter(|s| !s.is_empty())
    }

    pub fn paste(&mut self, text: &str) {
        if self.palette.is_some() {
            if let Some(p) = self.palette.as_mut() {
                p.query.push_str(text.lines().next().unwrap_or(""));
            }
            self.poke();
            return;
        }
        let bracketed = self.with_parser(|p, _| p.bracketed_paste()).unwrap_or(false);
        self.set_scroll(0);
        self.send(&term::paste(text, bracketed));
    }

    /// Select the object of daemon session `sid` once the list has it (the shot's `HARNESS_NATIVE_SHOT_SELECT`).
    pub fn select_session(&mut self, sid: u64) {
        *self.spawned.lock().unwrap() = Some(sid);
    }

    /// Select run `r{n}` (or its stage `k`) once the runs show it (the shot's `HARNESS_NATIVE_SHOT_SELECT=r12[.0]`).
    pub fn select_run(&mut self, n: u64, k: Option<u32>) {
        self.pending_run = Some((n, k));
    }

    /// Open the palette (the shot's `HARNESS_NATIVE_SHOT_PALETTE`).
    pub fn open_palette(&mut self) {
        self.palette = Some(Palette { query: String::new(), at: 0 });
        self.poke();
    }
}

/// The cell under `(x, y)` (layout points) in the terminal at `rect`: its row on the screen and column.
fn cell_at(rect: Option<[f32; 4]>, grid: &Grid, x: f32, y: f32, _: i64) -> Option<(i64, u16)> {
    let r = rect?;
    if x < r[0] - 4.0 || y < r[1] - 4.0 || x > r[0] + r[2] + 4.0 || y > r[1] + r[3] + 4.0 {
        return None;
    }
    let col = ((x - r[0]) / grid.cw).floor().max(0.0) as u16;
    let row = ((y - r[1]) / grid.ch).floor().max(0.0) as i64;
    Some((row, col))
}

/// A duration for a run's row: `340ms`, `4.2s`, `38s`, `3m04s`, `2h05m`.
fn fmt_dur(ms: u64) -> String {
    let s = ms / 1000;
    match ms {
        0..1000 => format!("{ms}ms"),
        1000..10_000 => format!("{:.1}s", ms as f64 / 1000.0),
        10_000..60_000 => format!("{s}s"),
        60_000..3_600_000 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s / 60) % 60),
    }
}

/// Whether every character of `q` comes in `s`, in order.
fn fuzzy(s: &str, q: &str) -> bool {
    let mut it = s.chars();
    q.chars().all(|c| c == ' ' || it.any(|x| x == c))
}

/// Set `fill` on a node when it differs.
fn set_if(v: View<'_>, fill: Fill) {
    if v.entity().try_cloned::<&Fill>() != Some(fill) {
        v.set(fill);
    }
}

/// den's `panel_from`: a plate of `fill` with `edge` and `shape` at `alpha`.
fn panel_from(fill: [f32; 4], edge: [f32; 4], shape: Shape, rect: [f32; 4], alpha: f32) -> PanelInst {
    PanelInst {
        rect,
        fill_d: fill,
        fill_l: fill,
        edge_d: edge,
        edge_l: edge,
        shape: [shape.radius, shape.n, shape.border, shape.feather],
        fx: [0.0; 4],
        fx2: [0.0, alpha, 0.0, 0.0],
        lens: [0.0; 4],
        fx3: [0.0; 4],
        clip: [0.0; 4],
    }
}

/// A layout's glyphs at `(ox, oy)` in `ink`, snapped to the device pixel grid with den's subpixel steps.
#[allow(clippy::too_many_arguments)]
fn emit_layout(d: &mut Draw, fonts: &Fonts, cache: &mut GlyphCache, lay: &Layout, (ox, oy): (f32, f32), ink: [f32; 4], alpha: f32, sc: f32, upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8])) {
    let asz = cache.size as f32;
    let mut col = ink;
    col[3] *= alpha;
    for line in &lay.lines {
        for g in &line.glyphs {
            let base_px = ((oy + line.baseline - g.y) * sc).round();
            let xp = (ox + g.x) * sc;
            let ix = xp.floor();
            let sub = (((xp - ix) * SUBPX as f32).floor() as u32).min(SUBPX - 1);
            let Some(e) = cache.get(fonts, g.font, g.id, sc, sub, upload) else { continue };
            if e.w == 0 {
                continue;
            }
            d.glyph(GlyphInst {
                rect: [(ix + e.left as f32) / sc, (base_px - e.top as f32) / sc, e.w as f32 / sc, e.h as f32 / sc],
                uv: [e.x as f32 / asz, e.y as f32 / asz, (e.x + e.w) as f32 / asz, (e.y + e.h) as f32 / asz],
                col_d: col,
                col_l: col,
                anim: [-1.0e9, 30.0, 0.0, 1.0],
                kind: [f32::from(u8::from(e.colour)), 0.0, 0.0, 0.0],
                ..Default::default()
            });
        }
    }
}
