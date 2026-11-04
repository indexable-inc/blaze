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
use crate::term::{self, Button, Grid, KeyAction, MouseAct, Sel};
use crate::pane::TermNode;
use harness_vt::MouseTracking;
use crate::text::{Face, Fonts, GlyphCache, Layout, SUBPX, Style, TypeLook};
use harness_native_api::spring::{Spring, Tune};
use harness_native_api::{APP, Clock, HoverAmt, moving, settle};
use harness_native_flecs::prelude::*;
use harness_native_model::{self as model, Msg};
use harness_native_ui_draw_api::{Paint, PaintKind};
use harness_native_ui_input_api::{Action, Activated, Hovered, PointerState, Pressed};
use harness_native_ui_layout::typeset;
use harness_native_ui_layout_api::{Intrinsic, Size, SizeKind, Sizing, Viewport};
use harness_native_ui_look_api::{Edge, Fill, HoverLook, Lift, PressLook, Rgba, Shape, TextStyle};
use harness_native_ui_node_api::{Laid, Level};
use harness_native_ui_text_api::{Icon, OneLine, Text, Truncate, TruncateMode};
use harness_native_ui_view::{self as view, View, for_each};
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

mod panes;
mod fold;
mod sidebar;
mod archive;
pub use sidebar::Wheel;

pub type Wake = Arc<dyn Fn() + Send + Sync>;

const TOKENS: &str = include_str!("../scene/00-tokens.flecs");
const HARNESS: &str = include_str!("../scene/10-harness.flecs");

/// The terminal's font size at zoom 1 (points), and its padding inside the main card.
const TERM_SIZE: f32 = 13.0;
/// The terminal's padding inside its pane (Ghostty's window-padding), in points; the cell remainder is added on top.
const TERM_PAD: (f32, f32) = (18.0, 16.0);
const ZOOMS: [f32; 9] = [0.67, 0.75, 0.85, 0.92, 1.0, 1.1, 1.25, 1.4, 1.6];


pub use crate::term::Mods as KeyMods;

/// What a palette entry does.
#[derive(Clone, Debug)]
enum Do {
    New(&'static str),
    Select(u64),
    /// An archived object: opened with the way back to the Archive.
    Archived(u64),
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
    /// `hn.Main`: the row of pane nodes.
    main: u64,
    /// The Archive's row pinned to the sidebar's bottom (pressed, the Archive pane opens).
    archive_row: u64,
    palette: Option<u64>,
    /// The title-bar strip's root (`hn.TitleBar`), its label and the muted words after it.
    bar: u64,
    bar_title: u64,
    bar_sub: u64,
}

/// The window's title-bar strip as AppKit lays it out, in points: how tall it is (0 in fullscreen, where it is hidden)
/// and where the title starts (past the traffic lights).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Chrome {
    pub strip: f32,
    pub title_x: f32,
}

/// A node's motion, set in the scene (hn.ArchiveRow's icon and count): on its row's hover (the row's `HoverAmt`
/// spring) it rises `lift` points and grows to `scale`; kicked (an object archived) its [`Bump`] spring, pushed at
/// `omega` and ringing out at `omega` / `zeta`, adds up to `kick_lift` points of rise and `kick_scale` of size.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct HoverLift {
    pub lift: f32,
    pub scale: f32,
    pub kick_lift: f32,
    pub kick_scale: f32,
    pub omega: f32,
    pub zeta: f32,
}

/// The spring a [`HoverLift`] node bounces on: at rest at 0, kicked by a push of velocity.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Bump {
    pub spring: Spring,
}

pub struct App {
    pub world: World,
    fonts: Fonts,
    cache: GlyphCache,
    layouts: HashMap<(String, u32, u32, bool, u8, u32), Rc<Layout>>,
    terms: model::Terms,
    /// The parsers of runs and stages being read, by entity id.
    run_terms: model::Terms,
    /// The runs and stages whose output the link follows (each shown by a pane), by entity: their run and stage.
    watching: HashMap<u64, (u64, Option<u32>)>,
    /// The sidebar's width (points) as last dragged, kept in native.json; None: the scene's.
    side_w: Option<f32>,
    /// Dragging the sidebar's edge.
    resizing: bool,
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
    /// The keyboard encoder (set from the focused terminal's `TermInput` before each key).
    keys: term::Keys,
    /// Keys whose press went to the program, so it hears their releases (kitty's event types).
    pressed: Vec<u16>,
    /// Session ids attached since launch: an ended one is attached again only when it is still running.
    tried: HashMap<u64, f32>,
    /// The session a spawn made, to select once the list shows it.
    spawned: Arc<Mutex<Option<u64>>>,
    /// The session selected when the app last ran (~/.harness/native.json), selected once the daemon's first list is in.
    restore: Option<Option<u64>>,
    /// What was last written to native.json.
    saved: Option<Option<u64>>,
    /// Objects cmd+W'd (native.json `closed`, by key: `s14`, `r12`); the world's `Closed` tags follow them.
    closed: BTreeSet<String>,
    /// Objects kept in the tree whatever the rule says (native.json `pinned`); the world's `Pinned` tags follow them.
    pinned: BTreeSet<String>,
    /// Open the Archive once the daemon's first list is in (the shot's `HARNESS_NATIVE_SHOT_SELECT=archive`).
    pending_archive: bool,
    /// The ids the daemon no longer lists were dropped (once, on its first list).
    pruned: bool,
    /// Remember the selected object in native.json (not for the offscreen shot).
    pub persist: bool,
    pub focused: bool,
    pub title: String,
    /// The title-bar strip the window reports, and the (strip, zoom) last written into the view.
    chrome: Chrome,
    chrome_laid: Option<(Chrome, f32, f32)>,
    /// The dark appearance (the window's effective NSAppearance; the scene's fills pick their dark or light half).
    pub dark: bool,
    /// The sidebar tree's scroll (src/app/sidebar.rs).
    tree: sidebar::TreeScroll,
    /// The sidebar tree's rows across frames, for folding rows in and away (src/app/fold.rs).
    fold: fold::TreeFold,
    /// The tree has the keyboard (a tree row was pressed): arrows move in it until a pane is pressed or another key typed.
    tree_focus: bool,
    /// The last press's click count and modifiers (a double click on a row folds it, Option folds its siblings too).
    press: (isize, KeyMods),
    /// The sidebar's fold (cmd+S): open or not, and its spring (0 folded away to 1 open); the traffic lights fade with it.
    side_open: bool,
    side_x: harness_native_api::spring::Spring,
    /// The traffic lights shown over the folded sidebar while the pointer rests in their corner (Dia's): 0 to 1.
    corner_x: harness_native_api::spring::Spring,
}

/// The fill of the theme's role `name` (`running`: `hn.Theme.running`).
fn tone(w: WorldRef<'_>, name: &str) -> Fill {
    crate::look::tone(w, name)
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
        crate::pane::register(&world);
        crate::look::register(&world);
        archive::register(&world);
        world.component::<HoverLift>().writable();
        world.component::<Bump>();
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
        // The rule's delay for a finished run with no session, from the scene's token (seconds).
        world.set(model::ArchiveAfter { ms: (crate::look::width(w, "archive_after").max(0.0) * 1000.0) as u64 });
        harness_native_ui_layout::install(w);
        harness_native_ui_input::install(w);
        harness_native_ui_motion::install(w);
        crate::look::install(w);
        harness_native_ui_draw::install(w);
        // den-native-hover: each hoverable's hover spring toward whether the pointer is on it.
        let app_id = *app.id();
        // Each kicked Bump rings back to rest on its HoverLift's spring.
        world.system_named::<(&mut Bump, &HoverLift)>("Bump").each_iter(|it, _, (b, m)| {
            if settle(&mut b.spring, 0.0, Tune { omega: m.omega.max(1.0), zeta: m.zeta.max(0.05) }, it.delta_time()) {
                moving(&it.world());
            }
        });
        world.system_named::<&mut HoverAmt>("Hover").each_iter(move |it, row, h| {
            let (e, w, dt) = (it.entity(row), it.world(), it.delta_time());
            // On it, or on a part of it that takes the pointer itself (a row's disclosure chevron).
            let on = w.entity_from_id(app_id).target(Hovered::id(), 0).is_some_and(|t| std::iter::successors(Some(t), |n| n.parent()).take(4).any(|n| n.id() == e.id()));
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
        let side = root.child("hn.SideClip").child("hn.Sidebar");
        let device = side.child("hn.DeviceRow");
        let side_w = crate::prefs::get("sidebar_width").and_then(|v| v.as_f64()).map(|w| (w as f32).clamp(SIDE_MIN, SIDE_MAX));
        if let Some(w) = side_w {
            set_side_width(side.entity(), w);
        }
        let tree = side.child("hn.Tree");
        let archive_row = side.child("hn.ArchiveRow");
        archive_row.set(HoverAmt::default());
        for name in ["icon", "count"] {
            if let Some(p) = archive_row.part(name) {
                p.set(Bump::default());
            }
        }
        // The panes: sync_panes makes the first (and swaps each one's prefab by what it shows).
        let main = root.child("hn.Main");
        let bar = View::root(w, "hn.TitleBar");
        let bar_title = bar.part("title").map_or(0, View::id);
        let bar_sub = bar.part("sub").map_or(0, View::id);
        let ids = Ids { tree: tree.id(), device: device.id(), main: main.id(), archive_row: archive_row.id(), palette: None, bar: bar.id(), bar_title, bar_sub };
        let mut fonts = Fonts::new(TypeLook { code_scale: 1.0, ..TypeLook::default() });
        let grid = Grid::new(&mut fonts, TERM_SIZE, 2.0);
        Ok(Self {
            world,
            fonts,
            cache: GlyphCache::new(ATLAS_SIZE),
            layouts: HashMap::new(),
            terms,
            run_terms,
            watching: HashMap::new(),
            pending_run: None,
            side_w,
            resizing: false,
            link,
            grid,
            zoom: 4,
            ids,
            pointer: PointerState::default(),
            time: 0.0,
            pending: 4,
            palette: None,
            keys: term::Keys::new(),
            pressed: Vec::new(),
            tried: HashMap::new(),
            spawned: Arc::default(),
            restore: Some(crate::prefs::get("selected").and_then(|v| v.as_u64())),
            saved: None,
            // The older `archived` (or `trashed`) session ids, read once (the next save writes `closed` and drops them).
            closed: crate::prefs::keys("closed")
                .or_else(|| crate::prefs::ids("archived").or_else(|| crate::prefs::ids("trashed")).map(|ids| ids.into_iter().map(|i| format!("s{i}")).collect()))
                .unwrap_or_default(),
            pinned: crate::prefs::keys("pinned").unwrap_or_default(),
            pending_archive: false,
            pruned: false,
            persist: true,
            focused: true,
            title: "harness".into(),
            chrome: Chrome::default(),
            chrome_laid: None,
            dark: true,
            tree: sidebar::TreeScroll::default(),
            fold: fold::TreeFold::default(),
            tree_focus: false,
            press: (0, KeyMods::default()),
            side_open: true,
            side_x: harness_native_api::spring::Spring::at(1.0),
            corner_x: harness_native_api::spring::Spring::at(0.0),
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
            "running" => tone(w, "running"),
            "lost" => tone(w, "lost"),
            "killed" => tone(w, "failed"),
            _ if code.is_some_and(|c| c != 0) => tone(w, "failed"),
            _ => tone(w, "exited"),
        }
    }

    fn select(&mut self, e: Option<u64>) {
        if e != self.selected() {
            self.reset_focused_term();
            self.pressed.clear();
            model::select(self.world.world_ref(), e);
            // The focused pane follows the selection (and forgets where its back went).
            if let Some(p) = self.focused_pane() {
                model::show(self.world.world_ref(), p, e);
                let pe = self.world.entity_from_id(p);
                if pe.target(crate::pane::Back::id(), 0).is_some() {
                    pe.remove((crate::pane::Back::id(), flecs::Wildcard::ID));
                }
            }
        }
        self.poke();
    }

    // ---- the view ----

    /// Keep the view's nodes in step with the model: the device row, one row per object, the main card's header, the
    /// palette.
    fn sync_view(&mut self) {
        // Folds kept in native.json land on objects before their rows are read.
        self.read_folds();
        let w = self.world.world_ref();
        let up = w.lookup(model::LOCAL).target(model::Link::id(), 0).is_some_and(|t| *t.id() == *w.component_id::<model::Up>());
        self.sync_marks(up);
        let w = self.world.world_ref();
        let rows = model::tree(w);
        // Once the daemon's first list is in: the object selected when the app last ran, if it is still there.
        if up && let Some(last) = self.restore.take() {
            if let Some(e) = last.and_then(|sid| self.entity_of(sid)) {
                model::select(w, Some(e));
            }
        }
        // Nothing selected (or it went): the first running object, else the first.
        if self.restore.is_none() && self.selected().is_none() && !rows.is_empty() {
            let first = rows.iter().find(|(e, _)| self.status_of(*e) == "running").or(rows.first()).map(|(e, _)| *e);
            model::select(w, first);
        }
        // A spawn's or a deep link's session, once the list has it (and after the restore, which it overrides).
        let pending = (*self.spawned.lock().unwrap()).filter(|_| self.restore.is_none());
        if let Some(sid) = pending {
            if let Some(e) = self.entity_of(sid) {
                model::select(w, Some(e));
                *self.spawned.lock().unwrap() = None;
            }
        }
        if self.pending_archive && self.restore.is_none() {
            self.pending_archive = false;
            model::select(w, Some(model::archive_view(w)));
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
        let clear = tone(w, "clear");
        // The device row.
        let dev = w.lookup(model::LOCAL);
        let device = View::of(w.entity_from_id(self.ids.device));
        let host = dev.try_cloned::<&model::Device>().map(|d| d.host.as_str().to_owned()).unwrap_or_default();
        let link = dev.target(model::Link::id(), 0).map_or(0, |t| *t.id());
        let (link_tone, link_word) = if link == *w.component_id::<model::Up>() {
            (tone(w, "running"), format!("{}", rows.len()))
        } else if link == *w.component_id::<model::Down>() {
            (tone(w, "failed"), "offline".to_owned())
        } else {
            (tone(w, "lost"), "connecting".to_owned())
        };
        if let Some(n) = device.part("name") {
            n.text(&host);
        }
        if let Some(n) = device.part("count") {
            n.text(&link_word);
        }
        if let Some(d) = device.part("dot") {
            // A light only while the daemon is not up.
            set_if(d, if link == *w.component_id::<model::Up>() { clear } else { link_tone });
        }
        let looks = RowLooks::of(w, clear);
        let step = crate::look::width(w, "indent");
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
        // Rows that left the list stay while they fold away.
        let info = self.fold.merge(info, w);
        let me = &*self;
        // Names to clip to their row's room once the loop is done (the typesetter needs `self` mutably).
        let indent = |row: View<'_>, depth: usize| {
            if let Some(p) = row.part("indent") {
                let s = Size { w: Sizing { kind: SizeKind::Fixed, value: depth as f32 * step, min: 0.0, max: 0.0 }, h: Sizing { kind: SizeKind::Fixed, value: 1.0, min: 0.0, max: 0.0 } };
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
                    fold::disclosure(row, None);
                    if let Some(p) = row.part("name") {
                        p.text(&if unfolded { "Show fewer".to_owned() } else { format!("Show {hidden} more") });
                    }
                    return;
                }
            };
            let o = w.entity_from_id(e);
            row.action(&format!("select:{e}"));
            fold::disclosure(row, Some(o));
            selection(row, sel == Some(e), clear);
            indent(row, depth);
            let run = me.run_of(e);
            let status = me.status_of(e);
            let failed = status == "killed" || o.try_cloned::<&model::ExitCode>().is_some_and(|c| c.code != 0);
            if run.is_none() {
                looks.session(me, row, e, hint.get(&e).filter(|&&i| i < 9).map(|i| format!("\u{2318}{}", i + 1)).unwrap_or_default());
            } else if let Some(p) = row.part("glyph") {
                // A run: a small dot tinted by how it went, breathing while it runs.
                p.icon(crate::look::RUN_ICON);
                set_if(p, match status {
                    "running" => looks.accent,
                    "lost" => looks.lost,
                    _ if failed => looks.failed,
                    _ => looks.faint,
                });
                crate::look::breathe(p.entity(), status == "running");
            }
            let name = o.try_cloned::<&model::Name>().map(|n| n.s.as_str().to_owned()).unwrap_or_default();
            if let Some(p) = row.part("name") {
                // One line: the part is a ui.Line, cut with an ellipsis to the room its row leaves it when drawn.
                p.text(&name.split_whitespace().collect::<Vec<_>>().join(" "));
            }
            if run.is_some()
                && let Some(p) = row.part("meta")
            {
                // How long it took (and how it failed), shown on hover; a failure's always.
                p.text(&me.run_meta(e));
                crate::look::hover_show(p.entity(), row.id(), failed || matches!(status, "lost" | "pending" | "skipped"));
            }
        });
        self.fold.animate(self.world.entity_from_id(self.ids.tree), crate::look::tune(self.world.world_ref(), "tree_fold"));
        self.sync_panes();
        self.sync_title();
        self.sync_archive();
        self.sync_palette();
        self.save_selected();
    }

    /// The title bar and the window title from the selected object (what the focused pane shows): its name, then its
    /// directory and state as muted words after it; both dimmed while the window is not key.
    fn sync_title(&mut self) {
        let w = self.world.world_ref();
        let sel = self.selected();
        let (title, sub) = match sel.map(|e| w.entity_from_id(e)) {
            Some(o) => {
                let e = *o.id();
                let kind = model::kind_name(o);
                let name = o.try_cloned::<&model::Name>().map(|n| n.s.as_str().to_owned()).unwrap_or_default();
                let t = o.try_cloned::<&model::Title>().map(|n| n.s.as_str().to_owned()).unwrap_or_default();
                let cwd = o.try_cloned::<&model::Cwd>().map(|n| tilde(n.s.as_str())).unwrap_or_default();
                let code = o.try_cloned::<&model::ExitCode>().map(|c| c.code);
                let join = |v: Vec<String>| v.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("  \u{00B7}  ");
                match (self.run_of(e), model::pane_kind(o)) {
                    // A run: its id and command (one line), then where and how it stands.
                    (Some((n, stage)), _) => {
                        let id = stage.map_or(format!("r{n}"), |k| format!("r{n}.{k}"));
                        let cmd = name.split_whitespace().collect::<Vec<_>>().join(" ");
                        (format!("{id}  {cmd}"), join(vec![self.run_cwd(e), self.run_state(e)]))
                    }
                    (None, model::PaneKind::Archive) => {
                        let n = model::archived(w).len();
                        (name, format!("{n} archived"))
                    }
                    _ => {
                        // The program-set title (OSC 0/2) never names anything (agents' titles are spinner frames and
                        // "Claude Code"): only a shell's shows, in place of its directory.
                        let place = if kind == "shell" && !t.is_empty() && t != name { t } else { cwd };
                        let state = match (self.status_of(e), code) {
                            ("exited", Some(c)) => format!("exited {c}"),
                            (s, _) => s.to_owned(),
                        };
                        (name, join(vec![place, state]))
                    }
                }
            }
            None => ("harness".to_owned(), String::new()),
        };
        self.title = if sel.is_some() { format!("{} \u{2014} harness", clip_text(&title, 60)) } else { "harness".to_owned() };
        // The prefab's ink while the window is key, at half its alpha while it is not.
        let focused = self.focused;
        let ink = |part: &str| {
            let mut f = view::prefab(w, "hn.TitleBar").and_then(|p| p.lookup_child(part)).and_then(|c| c.try_cloned::<&Fill>()).unwrap_or_default();
            if !focused {
                f.dark.a *= 0.5;
                f.light.a *= 0.5;
            }
            f
        };
        // The back chevron, while the focused pane has somewhere to go back to.
        if let Some(b) = View::of(w.entity_from_id(self.ids.bar)).part("back") {
            let on = self.back_target().is_some();
            b.icon(if on { archive::BACK_ICON } else { "" });
            let box_w = if on { crate::look::width(w, "indent") } else { 0.0 };
            if let Some(mut sz) = b.entity().try_cloned::<&Size>()
                && sz.w.value != box_w
            {
                sz.w = Sizing::fixed(box_w);
                b.set(sz);
            }
        }
        for (id, text, part) in [(self.ids.bar_title, clip_text(&title, 80), "title"), (self.ids.bar_sub, clip_text(&sub, 90), "sub")] {
            if id != 0 {
                let v = View::of(w.entity_from_id(id)).text(&text);
                set_if(v, ink(part));
            }
        }
    }

    /// Write the selected object's session id to native.json when it changed.
    fn save_selected(&mut self) {
        if !self.persist || self.restore.is_some() {
            return;
        }
        let now = self.selected().and_then(|e| self.session_of(e));
        if self.saved != Some(now) {
            crate::prefs::set("selected", serde_json::json!(now));
            self.saved = Some(now);
        }
    }

    /// An object's key across launches: its session (`s14`) or its run (`r12`); None for a stage or anything else.
    fn key_of(&self, e: u64) -> Option<String> {
        let o = self.world.entity_from_id(e);
        if let Some(ob) = o.try_cloned::<&model::Object>() {
            return Some(format!("s{}", ob.id));
        }
        o.try_cloned::<&model::Run>().map(|r| format!("r{}", r.n))
    }

    /// Keep the objects' `Closed` and `Pinned` tags in step with the kept keys (the model's `ArchiveRule` reads them);
    /// on the daemon's first list, drop the keys of objects it no longer has.
    fn sync_marks(&mut self, up: bool) {
        let w = self.world.world_ref();
        let mut objs: Vec<(u64, String)> = Vec::new();
        self.world.new_query::<&model::Object>().each_entity(|e, o| objs.push((*e.id(), format!("s{}", o.id))));
        self.world.new_query::<&model::Run>().each_entity(|e, r| objs.push((*e.id(), format!("r{}", r.n))));
        for (e, key) in &objs {
            let e = w.entity_from_id(*e);
            for (on, tag) in [(self.closed.contains(key), *w.component_id::<model::Closed>()), (self.pinned.contains(key), *w.component_id::<model::Pinned>())] {
                if e.has(tag) != on {
                    if on {
                        e.add(tag);
                    } else {
                        e.remove(tag);
                    }
                }
            }
        }
        // The link reports Up only after the list and the runs it got are in the world.
        if up && !self.pruned {
            self.pruned = true;
            let known: BTreeSet<String> = objs.into_iter().map(|(_, k)| k).collect();
            let n = self.closed.len() + self.pinned.len();
            self.closed.retain(|k| known.contains(k));
            self.pinned.retain(|k| known.contains(k));
            if n != self.closed.len() + self.pinned.len() {
                self.save_marks();
            }
        }
    }

    /// Write the closed and pinned keys to native.json (dropping the older Archive's and Trash's keys).
    fn save_marks(&self) {
        if self.persist {
            crate::prefs::set("closed", serde_json::json!(self.closed));
            crate::prefs::set("pinned", serde_json::json!(self.pinned));
            crate::prefs::remove(&["archived", "trashed", "hidden"]);
        }
    }

    /// The Archive's row at the sidebar's bottom: its icon, how many objects it holds and a chevron on hover; lit while
    /// the Archive, or an object opened from it, is selected.
    fn sync_archive(&mut self) {
        let w = self.world.world_ref();
        let n = model::archived(w).len();
        let sel = self.selected();
        let row = View::of(w.entity_from_id(self.ids.archive_row));
        if let Some(p) = row.part("icon") {
            p.icon(crate::look::ARCHIVE_ICON);
        }
        if let Some(p) = row.part("name") {
            p.text("Archive");
        }
        if let Some(p) = row.part("count") {
            p.text(&if n == 0 { String::new() } else { n.to_string() });
        }
        if let Some(p) = row.part("chevron") {
            p.icon(crate::look::CHEVRON);
            crate::look::hover_show(p.entity(), row.id(), false);
        }
        let (clear, chosen) = (tone(w, "clear"), tone(w, "selected"));
        let lit = sel.is_some_and(|s| s == model::archive_view(w) || w.entity_from_id(s).has(model::Archived::id()));
        set_if(row, if lit { chosen } else { clear });
    }

    /// Close object `e` (cmd+W, a Kill): it goes to the Archive whatever the rule says (and loses its pin); the archive
    /// row's icon bounces and its count pulses.
    fn archive_object(&mut self, e: u64) {
        let Some(key) = self.key_of(e) else { return };
        let pinned = self.pinned.remove(&key);
        if self.closed.insert(key) || pinned {
            let o = self.world.entity_from_id(e);
            o.add(model::Closed::id());
            o.remove(model::Pinned::id());
            self.save_marks();
            self.kick_archive();
        }
    }

    /// Kick the archive row's springs (its parts with `HoverLift` and `Bump`): each rings out from a push of its omega.
    fn kick_archive(&mut self) {
        let row = View::of(self.world.entity_from_id(self.ids.archive_row));
        for name in ["icon", "count"] {
            let Some(p) = row.part(name) else { continue };
            let e = p.entity();
            if let (Some(m), Some(mut b)) = (e.try_cloned::<&HoverLift>(), e.try_cloned::<&Bump>()) {
                b.spring.v = m.omega;
                e.set(b);
            }
        }
        self.poke();
    }

    /// Take object `e` back out of the Archive into the tree (right-click, Unarchive): no longer closed, and kept there
    /// so the rule does not take it straight back.
    pub fn unarchive(&mut self, e: u64) {
        let Some(key) = self.key_of(e) else { return };
        self.closed.remove(&key);
        self.pinned.insert(key);
        let o = self.world.entity_from_id(e);
        o.remove(model::Closed::id());
        o.add(model::Pinned::id());
        self.save_marks();
        self.poke();
    }

    /// Keep object `e` in the tree whatever the rule says (right-click, Keep in Tree), or stop.
    pub fn toggle_pin(&mut self, e: u64) {
        let Some(key) = self.key_of(e) else { return };
        let o = self.world.entity_from_id(e);
        if self.pinned.remove(&key) {
            o.remove(model::Pinned::id());
        } else {
            self.pinned.insert(key);
            o.add(model::Pinned::id());
        }
        self.save_marks();
        self.poke();
    }

    /// Whether `e` is kept in the tree ("Keep in Tree").
    pub fn is_pinned(&self, e: u64) -> bool {
        self.world.entity_from_id(e).has(model::Pinned::id())
    }

    /// Whether `e` can be closed, pinned or unarchived: a session or a run.
    pub fn is_object(&self, e: u64) -> bool {
        self.key_of(e).is_some()
    }

    /// Whether `e` is in the Archive.
    pub fn is_archived(&self, e: u64) -> bool {
        self.world.entity_from_id(e).has(model::Archived::id())
    }

    /// The tree's (or the Archive's) row under `(x, y)` (window points): its object.
    pub fn object_at(&self, x: f32, y: f32) -> Option<u64> {
        let (x, y) = (x / self.zoom(), y / self.zoom());
        let w = self.world.world_ref();
        let mut hit = None;
        let lists = [self.archive_list().unwrap_or(0), self.ids.tree];
        for list in lists.into_iter().filter(|&l| l != 0) {
            let l = w.entity_from_id(list);
            if !l.try_cloned::<&Laid>().is_some_and(|r| r.contains(x, y)) {
                continue;
            }
            l.each_child(|c| {
                if hit.is_none() && c.try_cloned::<&Laid>().is_some_and(|l| l.contains(x, y)) {
                    hit = c.try_cloned::<&Action>().and_then(|a| {
                        let n = a.name.as_str();
                        n.strip_prefix("select:").or_else(|| n.strip_prefix("arch:open:")).and_then(|s| s.parse::<u64>().ok())
                    });
                }
            });
        }
        hit.filter(|&e| w.entity_from_id(e).is_alive())
    }

    /// The context menu's Kill on object `e` (no asking): as cmd+W, it goes to the Archive.
    pub fn kill_object(&mut self, e: u64) {
        if !self.world.entity_from_id(e).is_alive() {
            return;
        }
        if self.selected() == Some(e) {
            self.kill();
        } else if let Some(sid) = self.session_of(e) {
            self.link.kill(sid);
            self.archive_object(e);
        } else if let Some((n, None)) = self.run_of(e) {
            if self.status_of(e) == "running" {
                self.link.kill_run(n);
            }
            self.archive_object(e);
        }
        self.poke();
    }

    /// The palette's entries for its query: new objects, then each object, then a few commands.
    fn entries(&self) -> Vec<(String, &'static str, String, String, Do)> {
        let w = self.world.world_ref();
        let mut out: Vec<(String, &'static str, String, String, Do)> = vec![
            ("new-claude".into(), kind_glyph("claude"), "New claude".into(), "\u{2318}T".into(), Do::New("claude")),
            ("new-codex".into(), kind_glyph("codex"), "New codex".into(), "\u{2318}\u{21E7}T".into(), Do::New("codex")),
            ("new-cursor".into(), kind_glyph("cursor"), "New cursor".into(), String::new(), Do::New("cursor")),
            ("new-sh".into(), kind_glyph("shell"), "New sh".into(), String::new(), Do::New("shell")),
        ];
        for (i, e) in self.objects().into_iter().enumerate() {
            let o = w.entity_from_id(e);
            let name = o.try_cloned::<&model::Name>().map(|n| n.s.as_str().to_owned()).unwrap_or_default();
            let hint = if i < 9 { format!("\u{2318}{}", i + 1) } else { String::new() };
            out.push((format!("obj{e}"), kind_glyph(model::kind_name(o)), name, hint, Do::Select(e)));
        }
        // The Archive, then what is in it (newest first; a run by its id and command).
        let view = model::archive_view(w);
        out.push(("archive".into(), "\u{2630}", "Open Archive".into(), String::new(), Do::Select(view)));
        for e in model::archived(w).into_iter().rev() {
            let o = w.entity_from_id(e);
            let name = o.try_cloned::<&model::Name>().map(|n| n.s.as_str().split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default();
            let name = match self.run_of(e) {
                Some((n, _)) => format!("r{n}  {name}"),
                None => name,
            };
            out.push((format!("arc{e}"), kind_glyph(model::kind_name(o)), name, "archived".into(), Do::Archived(e)));
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
                let (clear, chosen) = (tone(w, "clear"), tone(w, "selected"));
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
        self.step_sidebar(dt);
        self.lay_chrome(z);
        self.step_fold();
        self.step_tree_scroll(dt);
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
        self.sync_terms();
        let mut d = Draw {
            frame: Frame { view: [view.0, view.1], scale, time: self.time, dark: f32::from(u8::from(self.dark)), live: scale, textfx: [1.0, 0.0], tune: [0.82, 1.12, 1.0, 0.0], slug: [0.0; 4] },
            // Clear: the window's vibrancy shows through everywhere the scene paints nothing.
            clear: [0.0; 4],
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

    /// The window's title-bar strip (from AppKit, in points; see window.rs): the view leaves it clear and sets the title in it.
    pub fn set_chrome(&mut self, c: Chrome) {
        if c != self.chrome {
            self.chrome = c;
            self.poke();
        }
    }

    /// Lay the window's frame out at zoom `z` (Ghostty's and Finder's): the sidebar full height, its rows starting under
    /// the title-bar strip (the traffic lights sit on its vibrancy), as wide as its fold's spring leaves it; the main
    /// card from the window's top, its terminal padded down by the strip; the title bar the strip's height, its title at
    /// the card's text inset.
    fn lay_chrome(&mut self, z: f32) {
        use harness_native_ui_layout_api::{Padding, SafeAreaInset, ScrollState};
        let w = self.world.world_ref();
        let Some(proto) = view::prefab(w, "hn.Sidebar") else { return };
        let Some(full) = proto.try_cloned::<&Size>() else { return };
        let x = self.side_x.x.clamp(0.0, 1.0);
        let open = self.side_w.unwrap_or(full.w.value);
        let width = open * x;
        if self.chrome_laid == Some((self.chrome, z, width)) {
            return;
        }
        self.chrome_laid = Some((self.chrome, z, width));
        let top = self.chrome.strip / z;
        // The sidebar keeps its open width (its rows never relayout mid-fold); its clip (hn.SideClip) takes the
        // spring's width and slides it left by the part folded away.
        if let Some(side) = w.entity_from_id(self.ids.device).parent() {
            let mut size = side.try_cloned::<&Size>().unwrap_or(full);
            size.w = Sizing::fixed(open);
            side.set(size);
            if let Some(clip) = side.parent() {
                let mut size = clip.try_cloned::<&Size>().unwrap_or(full);
                size.w = Sizing::fixed(width);
                clip.set(size);
                clip.set(ScrollState { x: open - width, y: 0.0 });
            }
        }
        // The strip is the window's safe-area inset: the sidebar and the main card pad themselves out of it
        // (SafeAreaPadding in the scene).
        if let Some(window) = w.entity_from_id(self.ids.main).parent() {
            window.set(SafeAreaInset { l: 0.0, r: 0.0, t: top, b: 0.0 });
        }
        let bar = w.entity_from_id(self.ids.bar);
        if let Some(mut size) = bar.try_cloned::<&Size>() {
            size.h = Sizing { kind: SizeKind::Fixed, value: top, min: 0.0, max: 0.0 };
            bar.set(size);
        }
        let bpad = bar.try_cloned::<&Padding>().unwrap_or_default();
        let past_lights = self.chrome.title_x / z * self.corner_x.x.clamp(0.0, 1.0);
        bar.set(Padding { l: (width + TERM_PAD.0).max(past_lights), ..bpad });
    }

    /// Step the sidebar's fold spring toward open or folded (cmd+S).
    fn step_sidebar(&mut self, dt: f32) {
        // Folded, the lights come back while the pointer is over their corner: as wide as they reach, as tall as the strip.
        let z = self.zoom();
        let (px, py) = (self.pointer.x, self.pointer.y);
        let corner = !self.side_open && self.chrome.strip > 0.0 && px >= 0.0 && py >= 0.0 && px < self.chrome.title_x / z && py < self.chrome.strip / z;
        let target = f32::from(u8::from(corner));
        if !self.corner_x.settled(target) {
            let tune = crate::look::tune(self.world.world_ref(), "lights");
            if !harness_native_api::settle(&mut self.corner_x, target, tune, dt) {
                self.corner_x = harness_native_api::spring::Spring::at(target);
            }
            self.chrome_laid = None;
            self.poke();
        }
        let target = f32::from(u8::from(self.side_open));
        if self.side_x.settled(target) {
            return;
        }
        let tune = crate::look::tune(self.world.world_ref(), "sidebar");
        if !harness_native_api::settle(&mut self.side_x, target, tune, dt) {
            self.side_x = harness_native_api::spring::Spring::at(target);
        }
        self.poke();
    }

    /// How shown the traffic lights are (0 to 1): they live on the sidebar and fade with its fold.
    pub fn lights(&self) -> f32 {
        self.side_x.x.max(self.corner_x.x).clamp(0.0, 1.0)
    }

    /// Whether a running dot breathes (the window keeps drawing, at a calm rate, while the app is otherwise idle).
    pub fn breathing(&self) -> bool {
        crate::look::breathing(self.world.world_ref())
    }

    /// `text` cut (with an ellipsis) to the longest front that the typesetter sets within `room` points.
    fn fit_text(&mut self, text: &str, style: &TextStyle, room: f32) -> String {
        let width = |me: &mut Self, t: &str| me.layout(t, style, 1.0e5).lines.iter().map(|l| l.width).fold(0.0, f32::max);
        if width(self, text) <= room {
            return text.to_owned();
        }
        let chars: Vec<char> = text.chars().collect();
        let (mut lo, mut hi) = (0usize, chars.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let t: String = chars[..mid].iter().collect::<String>() + "\u{2026}";
            if width(self, &t) <= room {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        chars[..lo].iter().collect::<String>().trim_end().to_owned() + "\u{2026}"
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
                let lay = if t.one_line || at + 0.5 >= w { one } else { self.layout(&t.text, &t.style, at) };
                let st = set_of(&t.style);
                let h = lay.lines.last().map_or(st.line, |l| l.top + l.height).max(st.line);
                let [base, last] = [lay.lines.first(), lay.lines.last()].map(|l| l.map_or(0.0, |l| l.baseline));
                Intrinsic { w, h, at, base, last }
            })
            .collect();
        typeset::apply(self.world.world_ref(), &wanted, &sizes)
    }

    fn layout(&mut self, text: &str, style: &TextStyle, width: f32) -> Rc<Layout> {
        let st = set_of(style);
        let code = style.role == 9;
        let medium = !code && !st.bold && style.weight == 500;
        let key = (text.to_owned(), st.size.to_bits(), st.line.to_bits(), st.bold, u8::from(code) | u8::from(medium) << 1, width.to_bits());
        if let Some(l) = self.layouts.get(&key) {
            return l.clone();
        }
        let lay = Rc::new(if code {
            Layout::in_face(&mut self.fonts, text, Face::Code, st, width)
        } else if medium {
            Layout::in_face(&mut self.fonts, text, Face::Medium, st, width)
        } else {
            Layout::new(&mut self.fonts, text, st, width)
        });
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
                // A double click on a row with rows under it folds or unfolds it too.
                let again = self.press.0 >= 2 && self.selected() == Some(e);
                self.select(Some(e));
                self.tree_focus = true;
                if again && fold::has_kids(self.world.entity_from_id(e)) {
                    self.toggle_fold_of(e, self.press.1.alt);
                }
            } else if let Some(e) = a.strip_prefix("toggle:").or_else(|| a.strip_prefix("collapse:")).and_then(|s| s.parse().ok()) {
                self.tree_focus = true;
                self.toggle_fold_of(e, self.press.1.alt);
            } else if let Some(p) = a.strip_prefix("fold:").and_then(|s| s.parse().ok()) {
                model::toggle_fold(self.world.world_ref(), p);
            } else if a == "archive:open" {
                self.open_archive();
            } else if a == "nav:back" {
                self.go_back();
            } else if let Some(e) = a.strip_prefix("arch:open:").and_then(|s| s.parse().ok()) {
                self.open_from_archive(e);
            } else if let Some(e) = a.strip_prefix("arch:fold:").and_then(|s| s.parse().ok()) {
                self.toggle_archive_open(e);
            } else if let Some(i) = a.strip_prefix("arch:chip:").and_then(|s| s.parse().ok()) {
                self.set_archive_chip(i);
            } else if a == "arch:more" {
                self.request_older();
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
            Do::Archived(e) => self.open_from_archive(e),
            Do::Kill => self.kill(),
            Do::Zoom(by) => self.step_zoom(by),
        }
        self.poke();
    }

    // ---- drawing ----

    fn emit(&mut self, d: &mut Draw, sc: f32, upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8])) {
        let held = self.world.lookup(APP).target(Pressed::id(), 0).map(|t| *t.id());
        let painted = harness_native_ui_draw::painted(self.world.world_ref());
        let dark = self.dark;
        let lo = crate::look::floor(self.world.world_ref(), "breath");
        for (id, mut p) in painted {
            let w = self.world.world_ref();
            let e = w.entity_from_id(id);
            let Some(laid) = e.try_cloned::<&Laid>() else { continue };
            // Where the draw pass says it goes (moved by a transition, mid-slide on a MatchedGeometry), before its scale.
            let laid = if p.w > 0.0 || p.h > 0.0 { Laid { x: p.x, y: p.y, w: p.w, h: p.h } } else { laid };
            // A breathing dot dims and brightens; a hover-only part fades with its row's hover.
            p.alpha *= crate::look::alpha(w, e, lo);
            let is_term = e.has(TermNode::id());
            if p.alpha <= 0.002 && !is_term {
                continue;
            }
            if p.clipped && (p.clip_w <= 0.0 || p.clip_h <= 0.0) {
                continue;
            }
            let rect = [laid.x, laid.y, laid.w, laid.h];
            let (np, ng) = (d.panels.len(), d.glyphs.len());
            // A HoverLift node: how far its row's hover and its bump have moved it (read before the drawing borrows self).
            let motion = e.try_cloned::<&HoverLift>().map(|m| {
                let hover = e.parent().and_then(|r| r.try_cloned::<&HoverAmt>()).map_or(0.0, |h| h.spring.x.clamp(0.0, 1.0));
                let bump = e.try_cloned::<&Bump>().map_or(0.0, |b| b.spring.x);
                (-(m.lift * hover + m.kick_lift * bump), (1.0 + (m.scale - 1.0) * hover + m.kick_scale * bump).max(0.1))
            });
            match p.kind {
                PaintKind::Plate => {
                    let fill = e.try_cloned::<&Fill>().unwrap_or_default();
                    let edge = e.try_cloned::<&Edge>().unwrap_or_default();
                    let mut shape = e.try_cloned::<&Shape>().unwrap_or_default();
                    if shape.n <= 0.0 {
                        shape.n = 4.0;
                    }
                    let (fc, ec) = if dark { (fill.dark, edge.dark) } else { (fill.light, edge.light) };
                    let mut panel = panel_from(rgba(fc), rgba(ec), shape, rect, p.alpha);
                    if let Some(l) = e.try_cloned::<&Lift>() {
                        panel.fx = [l.blur, l.alpha, 0.0, 0.0];
                        panel.fx2[0] = l.dy;
                    }
                    if fc.a > 0.0 || ec.a > 0.0 || panel.fx[1] > 0.0 {
                        d.panel(panel);
                    }
                    let wash = |c: Rgba, r: [f32; 4], radius: f32, a: f32| panel_from(rgba(c), [0.0; 4], Shape { radius, n: shape.n, border: 0.0, feather: 0.0 }, r, p.alpha * a);
                    let hover = e.try_cloned::<&HoverLook>().zip(e.try_cloned::<&HoverAmt>().map(|h| h.spring.x.clamp(0.0, 1.0)));
                    if let Some((l, h)) = hover.filter(|(_, h)| *h > 0.004) {
                        let r = [rect[0] - l.pad, rect[1] - l.pad, rect[2] + 2.0 * l.pad, rect[3] + 2.0 * l.pad];
                        d.panel(wash(if dark { l.dark } else { l.light }, r, if l.radius < 0.0 { shape.radius + l.pad } else { l.radius }, h));
                    }
                    if let Some(l) = e.try_cloned::<&PressLook>().filter(|_| held == Some(id)) {
                        d.panel(wash(if dark { l.dark } else { l.light }, rect, shape.radius, 1.0));
                    }
                    if is_term {
                        self.emit_term(id, d, sc, upload);
                    }
                }
                PaintKind::Glyphs => {
                    let Some(text) = e.try_cloned::<&Text>() else { continue };
                    let style = e.try_cloned::<&TextStyle>().unwrap_or_default();
                    let ink = e.try_cloned::<&Fill>().map_or([0.9, 0.9, 0.92, 1.0], |f| rgba(if dark { f.dark } else { f.light }));
                    let truncate = e.try_cloned::<&Truncate>().map(|t| t.mode).or_else(|| e.has(OneLine::id()).then_some(TruncateMode::Tail));
                    let lay = if let Some(mode) = truncate {
                        // One line, never wrapped: cut with an ellipsis (at its end, middle or start) to the width layout gave it.
                        let room = laid.w.max(1.0) + 0.5;
                        let cut = typeset::elide(text.s.as_str(), mode, room, |t| self.layout(t, &style, 1.0e5).lines.iter().map(|l| l.width).fold(0.0, f32::max));
                        self.layout(&cut, &style, 1.0e5)
                    } else {
                        self.layout(text.s.as_str(), &style, laid.w.max(1.0) + 0.5)
                    };
                    emit_layout(d, &self.fonts, &mut self.cache, &lay, (laid.x, laid.y), ink, p.alpha, sc, upload);
                }
                PaintKind::Icon => {
                    // A ui.Symbol: its glyph (a Nerd Font icon) set by the typesetter, centred in its rect.
                    let Some(icon) = e.try_cloned::<&Icon>() else { continue };
                    let turn = crate::look::turn(e);
                    let ink = e.try_cloned::<&Fill>().map_or([0.9, 0.9, 0.92, 1.0], |f| rgba(if dark { f.dark } else { f.light }));
                    // 14 in a 16 box, as SF Symbols sit in a sidebar row.
                    let style = TextStyle { size: laid.h.min(laid.w + 2.0) * 0.875, line: laid.h, ..TextStyle::default() };
                    let lay = self.layout(icon.glyph.as_str(), &style, 1.0e5);
                    let gw = lay.lines.iter().map(|l| l.width).fold(0.0, f32::max);
                    emit_layout(d, &self.fonts, &mut self.cache, &lay, (laid.x + (laid.w - gw) * 0.5, laid.y), ink, p.alpha, sc, upload);
                    // A disclosure chevron turns about its middle.
                    if let Some(turn) = turn {
                        for g in &mut d.glyphs[ng..] {
                            g.clip[2] = turn;
                        }
                    }
                }
                PaintKind::Picture | PaintKind::Ribbon => {}
            }
            // A HoverLift node: its glyphs risen and scaled about its centre by its row's hover and its bump.
            if let Some((dy, k)) = motion {
                let (cx, cy) = (laid.x + laid.w * 0.5, laid.y + laid.h * 0.5);
                for g in &mut d.glyphs[ng..] {
                    g.rect = [cx + (g.rect[0] - cx) * k, cy + (g.rect[1] - cy) * k + dy, g.rect[2] * k, g.rect[3] * k];
                }
            }
            // A transition's scale: what this node drew, scaled about its corner.
            if (p.scale - 1.0).abs() > 1e-4 && p.scale > 0.0 {
                let (ox, oy, k) = (laid.x, laid.y, p.scale);
                for panel in &mut d.panels[np..] {
                    panel.rect = [ox + (panel.rect[0] - ox) * k, oy + (panel.rect[1] - oy) * k, panel.rect[2] * k, panel.rect[3] * k];
                }
                for g in &mut d.glyphs[ng..] {
                    g.rect = [ox + (g.rect[0] - ox) * k, oy + (g.rect[1] - oy) * k, g.rect[2] * k, g.rect[3] * k];
                }
            }
            if p.clipped {
                let band = [p.clip_y, p.clip_y + p.clip_h];
                for panel in &mut d.panels[np..] {
                    panel.clip[..2].copy_from_slice(&band);
                }
                for glyph in &mut d.glyphs[ng..] {
                    glyph.clip[..2].copy_from_slice(&band);
                }
                // Cut to the tree's band: soft at an edge with more of the tree beyond it.
                if let Some(fade) = self.tree_fade(&p) {
                    for panel in &mut d.panels[np..] {
                        panel.clip[2..].copy_from_slice(&fade);
                    }
                    for glyph in &mut d.glyphs[ng..] {
                        glyph.fade[..2].copy_from_slice(&fade);
                    }
                }
            }
            let _ = Level::Base;
        }
        self.emit_tree_knob(d);
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
        if self.palette.is_none()
            && let Some(bytes) = term::natural_edit(code, key.chars().next(), m)
        {
            self.typed();
            self.send(bytes);
            return true;
        }
        let k = key.to_lowercase();
        match (k.as_str(), m.shift) {
            ("a", false) => self.select_all(),
            ("]", _) | ("}", _) => self.step_object(1),
            ("[", _) | ("{", _) => {
                // Back to the Archive after opening something from it; else the previous object.
                if !self.go_back() {
                    self.step_object(-1);
                }
            }
            ("t" | "n", false) => self.spawn("claude"),
            ("t" | "n", true) => self.spawn("codex"),
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

    /// The sidebar node (the device row's parent).
    fn sidebar(&self) -> Option<EntityView<'_>> {
        self.world.entity_from_id(self.ids.device).parent()
    }

    /// Command-S: fold the sidebar away (it narrows to nothing on a spring, clipped, so its rows neither draw nor take
    /// clicks; the main card takes the window and the traffic lights fade out) or bring it back.
    fn toggle_sidebar(&mut self) {
        self.side_open = !self.side_open;
        self.poke();
    }

    /// Start with the sidebar folded away (`HARNESS_NATIVE_SIDEBAR=0`, for captures).
    pub fn fold_sidebar_now(&mut self) {
        self.side_open = false;
        self.side_x = harness_native_api::spring::Spring::at(0.0);
        self.poke();
    }

    fn step_object(&mut self, by: i64) {
        let objs = self.objects();
        if objs.is_empty() {
            return;
        }
        let n = objs.len() as i64;
        // From an archived (or no) object: the first or the last of the tree.
        let at = self.selected().and_then(|s| objs.iter().position(|&e| e == s)).map_or(if by > 0 { -1 } else { n }, |i| i as i64);
        self.select(Some(objs[(at + by).rem_euclid(n) as usize]));
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
        let (cols, rows) = self.focused_cells().unwrap_or((120, 40));
        let slot = self.spawned.clone();
        self.link.spawn(kind, cwd, cols, rows, move |id| *slot.lock().unwrap() = Some(id));
    }

    fn kill(&mut self) {
        let Some(e) = self.selected() else { return };
        if let Some((n, None)) = self.run_of(e) {
            // A run: killed while it runs, and closed; its session (if any) takes its place.
            if self.status_of(e) == "running" {
                self.link.kill_run(n);
            }
            let up = self.world.entity_from_id(e).parent().filter(|p| p.has(model::Object::id())).map(|p| *p.id());
            self.archive_object(e);
            self.select(up);
            return;
        }
        let Some(sid) = self.session_of(e) else { return };
        // The next object takes its place.
        let objs = self.objects();
        let next = objs.iter().position(|&o| o == e).and_then(|i| objs.get(i + 1).or_else(|| i.checked_sub(1).and_then(|j| objs.get(j)))).copied();
        self.link.kill(sid);
        self.archive_object(e);
        self.select(next);
    }

    /// Select the object of daemon session `sid` once the list has it (the shot's `HARNESS_NATIVE_SHOT_SELECT`).
    pub fn select_session(&mut self, sid: u64) {
        *self.spawned.lock().unwrap() = Some(sid);
    }

    /// Select run `r{n}` (or its stage `k`) once the runs show it (the shot's `HARNESS_NATIVE_SHOT_SELECT=r12[.0]`).
    pub fn select_run(&mut self, n: u64, k: Option<u32>) {
        self.pending_run = Some((n, k));
    }

    /// Open the Archive once the daemon's first list is in (the shot's `HARNESS_NATIVE_SHOT_SELECT=archive`).
    pub fn select_archive_later(&mut self) {
        self.pending_archive = true;
    }

    /// Open the palette (the shot's `HARNESS_NATIVE_SHOT_PALETTE`).
    pub fn open_palette(&mut self) {
        self.palette = Some(Palette { query: String::new(), at: 0 });
        self.poke();
    }
}

/// The sidebar's width range when dragged, and how near its edge a press grabs it (points).
const SIDE_MIN: f32 = 180.0;
const SIDE_MAX: f32 = 720.0;
const SIDE_GRAB: f32 = 5.0;

/// Set the sidebar node's (open) width; its fold's clip follows in `lay_chrome`.
fn set_side_width(side: EntityView<'_>, v: f32) {
    let Some(mut size) = side.try_cloned::<&Size>() else { return };
    size.w = Sizing { kind: SizeKind::Fixed, value: v, min: 0.0, max: 0.0 };
    side.set(size);
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
/// The tones a sidebar row is tinted with, read from the scene once a sync.
struct RowLooks {
    clear: Fill,
    accent: Fill,
    failed: Fill,
    failed_wash: Fill,
    faint: Fill,
    lost: Fill,
    badge_on: harness_native_ui_layout_api::Padding,
}

impl RowLooks {
    fn of(w: WorldRef<'_>, clear: Fill) -> Self {
        Self {
            clear,
            accent: tone(w, "accent"),
            failed: tone(w, "failed"),
            failed_wash: tone(w, "failed_wash"),
            faint: tone(w, "faint"),
            lost: tone(w, "lost"),
            badge_on: crate::look::padding(w, "badge_on"),
        }
    }

    /// A session's row: its kind's icon in its colour, its shortcut on hover (`hint`), a failure's exit code as a small
    /// red badge, and a breathing accent dot while it runs (amber when lost). A clean exit shows nothing.
    fn session(&self, me: &App, row: View<'_>, e: u64, hint: String) {
        let w = me.world.world_ref();
        let o = w.entity_from_id(e);
        let kind = model::kind_name(o);
        let status = me.status_of(e);
        let code = o.try_cloned::<&model::ExitCode>().map(|c| c.code).filter(|&c| c != 0);
        if let Some(p) = row.part("glyph") {
            p.icon(crate::look::kind_icon(kind));
            set_if(p, crate::look::tone(w, crate::look::kind_tone(kind)));
        }
        if let Some(p) = row.part("meta") {
            p.text(&hint);
            crate::look::hover_show(p.entity(), row.id(), false);
        }
        if let Some(b) = row.part("badge") {
            let shown = match (status, code) {
                ("killed", _) => Some("\u{2715}".to_owned()),
                (_, Some(c)) => Some(c.to_string()),
                _ => None,
            };
            if let Some(t) = b.part("code") {
                t.text(shown.as_deref().unwrap_or(""));
            }
            set_if(b, if shown.is_some() { self.failed_wash } else { self.clear });
            let pad = if shown.is_some() { self.badge_on } else { harness_native_ui_layout_api::Padding::default() };
            if b.entity().try_cloned::<&harness_native_ui_layout_api::Padding>() != Some(pad) {
                b.set(pad);
            }
        }
        if let Some(d) = row.part("dot") {
            set_if(d, match status {
                "running" => self.accent,
                "lost" => self.lost,
                _ => self.clear,
            });
            crate::look::breathe(d.entity(), status == "running");
        }
        let _ = self.failed;
    }
}

/// A tree row's selection: a `hn.Selection` pill under it while selected (MatchedGeometry slides it from the row lit
/// before), the row's own fill clear.
fn selection(row: View<'_>, on: bool, clear: Fill) {
    set_if(row, clear);
    match (row.part("pill"), on) {
        (None, true) => {
            row.child("hn.Selection").named("pill");
        }
        (Some(p), false) => p.entity().destruct(),
        _ => {}
    }
}

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
