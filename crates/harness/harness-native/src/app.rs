//! The app: one flecs world holding the model (harness-native-model: devices, objects, their relations) and the view (den's
//! UI primitives as entity trees, kept in step with the model by `sync_view`), the typesetter that sizes its words, the
//! terminal of the selected object, and the keyboard and pointer. The window (window.rs) or the offscreen shot (shot.rs)
//! asks it for a frame and hands it events.
//!
//! The frame loop is den's (den-native ui/native.rs): write `Viewport`, `PointerState` and `Paint`, typeset, `progress`,
//! typeset again while sizes move, then turn the painted nodes into Metal instances (`emit`).
use crate::draw::Draw;
use crate::link::Link;
use crate::pack::Frame;
use crate::pane::TermNode;
use crate::term::{self, Button, Grid, KeyAction, MouseAct, Sel};
use crate::text::TypeLook;
use harness_native_api::spring::{Spring, Tune};
use harness_native_api::{APP, Clock, HoverAmt, moving, settle};
use harness_native_flecs::prelude::*;
use harness_native_model as model;
use harness_native_render::paint::{Painter, panel_from};
use harness_native_ui_draw_api::{Paint, PaintKind};
use harness_native_ui_input_api::{Action, Activated, Hovered, Pointer, PointerState, Pressed};
use harness_native_ui_layout_api::{Size, SizeKind, Sizing, Viewport};
use harness_native_ui_look_api::{Fill, Rgba, Shape};
use harness_native_ui_node_api::{Laid, Level};
use harness_native_ui_view::{self as view, View, for_each};
use harness_vt::MouseTracking;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

mod archive;
mod chat;
mod fold;
mod keys;
mod panes;
mod pointer;
mod side_drag;
mod sidebar;
mod stats;
pub use stats::TICK as STATS_TICK;
mod ghostty;
mod script;
pub use keys::{MenuLine, Outcome};
pub use sidebar::Wheel;

pub type Wake = Arc<dyn Fn() + Send + Sync>;

const TOKENS: &str = include_str!("../scene/00-tokens.flecs");
/// The harness scene: 10-harness.flecs includes the widget files (scene/widgets/), each a managed script of its own. They
/// are compiled in and served to flecs as files (harness_native_ecs::embed_files) under these names.
const SCENE: &[(&str, &str)] = &[
    ("scene/10-harness.flecs", include_str!("../scene/10-harness.flecs")),
    ("scene/widgets/frame.flecs", include_str!("../scene/widgets/frame.flecs")),
    ("scene/widgets/parts.flecs", include_str!("../scene/widgets/parts.flecs")),
    ("scene/widgets/row.flecs", include_str!("../scene/widgets/row.flecs")),
    ("scene/widgets/rows.flecs", include_str!("../scene/widgets/rows.flecs")),
    ("scene/widgets/panes.flecs", include_str!("../scene/widgets/panes.flecs")),
    ("scene/widgets/archive.flecs", include_str!("../scene/widgets/archive.flecs")),
    ("scene/widgets/chat.flecs", include_str!("../scene/widgets/chat.flecs")),
    ("scene/widgets/genui.flecs", include_str!("../scene/widgets/genui.flecs")),
    ("scene/widgets/palette.flecs", include_str!("../scene/widgets/palette.flecs")),
    ("scene/widgets/picker.flecs", include_str!("../scene/widgets/picker.flecs")),
    ("scene/widgets/theme.flecs", include_str!("../scene/widgets/theme.flecs")),
    ("scene/widgets/stats.flecs", include_str!("../scene/widgets/stats.flecs")),
];

/// The terminal's font size at zoom 1 (points) when the Ghostty config sets none (src/app/ghostty.rs).
const TERM_SIZE: f32 = 13.0;
const ZOOMS: [f32; 9] = [0.67, 0.75, 0.85, 0.92, 1.0, 1.1, 1.25, 1.4, 1.6];

pub use crate::term::Mods as KeyMods;

/// What a palette entry does.
#[derive(Clone, Debug)]
enum Do {
    /// Run an action (a new object's act.Spawn), as its keys would.
    Act(u64),
    Select(u64),
    /// An archived object: opened with the way back to the Archive.
    Archived(u64),
    Kill,
    Sidebar,
    Zoom(i32),
}

/// What the overlay over the window is: the command palette (cmd+K) or the new-object picker (cmd+N). One overlay at a
/// time; both are a card on the scrim (scene/widgets/palette.flecs), share its keys' actions and take the pointer and the
/// keyboard from the panes while open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Commands,
    New,
}

/// An act.Spawn action: the kind it makes and whose mark shows it (`claude-tui` shows Claude's), its name in the picker
/// (`title`, empty: not in the picker) and in the palette (`menu`), the keys that run it from anywhere (`hint`) and the
/// key that picks it in the picker (`cap`).
struct Pick {
    act: u64,
    kind: String,
    mark: String,
    title: String,
    menu: String,
    hint: String,
    cap: String,
}

struct Palette {
    mode: Mode,
    query: String,
    at: usize,
    /// The query history was last searched for, and when (the app's clock) the query last changed: the daemon is asked
    /// once typing has paused for [`PALETTE_DEBOUNCE`].
    searched: String,
    changed: f32,
}

impl Palette {
    fn new() -> Self {
        Self {
            mode: Mode::Commands,
            query: String::new(),
            at: 0,
            searched: String::new(),
            changed: 0.0,
        }
    }
}

/// How long typing in the palette pauses before older history is searched (seconds).
const PALETTE_DEBOUNCE: f32 = 0.2;
/// How far up from what the pointer is on a node's hover reaches: a chat's drawn view is hovered from a control deep in it.
const HOVER_DEPTH: usize = 16;

/// The view's fixed nodes, by entity id.
struct Ids {
    tree: u64,
    device: u64,
    /// `hn.LinkNote` over the Archive row: the link's refusal, when it is refused.
    link_note: u64,
    /// `hn.Main`: the row of pane nodes.
    main: u64,
    /// The Archive's row pinned to the sidebar's bottom (pressed, the Archive pane opens).
    archive_row: u64,
    /// The overlay on screen and what it is (made again when the mode changes).
    palette: Option<(u64, Mode)>,
    /// The title-bar strip's root (`hn.TitleBar`): only the back chevron, no title.
    bar: u64,
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
    /// Fonts, glyph atlas, layouts and pictures, and the paint pass shared with harness-video (harness-native-render).
    paint: std::cell::RefCell<Painter>,
    terms: model::Terms,
    /// The parsers of runs and stages being read, by entity id.
    run_terms: model::Terms,
    /// The chat sessions' transcripts, by session id, and each chat pane's own state (src/app/chat.rs).
    chats: model::Chats,
    chat: std::cell::RefCell<HashMap<u64, chat::ChatState>>,
    /// The cells' controls in use (chat/genui/control.rs): a field typed in, a slider dragged, a select open.
    genui: std::cell::RefCell<chat::GenLive>,
    /// The runs and stages whose output the link follows (each shown by a pane), by entity: their run and stage.
    watching: HashMap<u64, (u64, Option<u32>)>,
    /// The sidebar's width (points) as last dragged, kept in native.json; None: the scene's.
    side_w: Option<f32>,
    /// Dragging the sidebar's edge, the window's rubber band past its widest, and their pulses (src/app/side_drag.rs).
    drag: side_drag::SideDrag,
    /// A run (and stage) to select once the runs show it.
    pending_run: Option<(u64, Option<u32>)>,
    link: Link,
    grid: Grid,
    zoom: usize,
    ids: Ids,
    pointer: PointerState,
    /// The modifier keys as the last pointer event or modifier change left them (Shift over a terminal that reports the
    /// mouse selects instead, so the pointer turns back to the I-beam).
    mods: KeyMods,
    /// The system pointer's shape as last resolved, and why (src/app/pointer.rs; window.rs sets NSCursor when it changes).
    shape: (Pointer, &'static str),
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
    /// The keyboard's bindings and actions (scene/20-keys.flecs and the user's keys.flecs), the user's file as watched,
    /// and how many times they have been loaded.
    keymap: crate::keys::Keymap,
    key_file: keys::KeyFile,
    keys_gen: u64,
    /// The sidebar's fold (cmd+S): open or not, and its spring (0 folded away to 1 open); the traffic lights fade with it.
    side_open: bool,
    side_x: harness_native_api::spring::Spring,
    /// The traffic lights shown over the folded sidebar while the pointer rests in their corner (Dia's): 0 to 1.
    corner_x: harness_native_api::spring::Spring,
    /// F3's debug overlay (src/app/stats.rs) and the last poke's reason.
    stats: stats::Stats,
    /// The user's Ghostty config, live: the terminal's themes, font size and padding (src/app/ghostty.rs).
    look: ghostty::Look,
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
        (!s.error.is_null())
            .then(|| unsafe { std::ffi::CStr::from_ptr(s.error) }.to_string_lossy().into_owned())
    })
    .flatten()
}

/// A palette entry's glyph: a kind's mark (`look::kind_mark`), or a codicon (`look::*_ICON`) in the muted ink.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PalGlyph {
    Kind(String),
    Char(&'static str),
}

fn clip_text(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        format!("{}\u{2026}", s.chars().take(n - 1).collect::<String>())
    }
}

/// `~` for the home directory at the front of a path.
fn tilde(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && p.starts_with(&h) => format!("~{}", &p[h.len()..]),
        _ => p.to_owned(),
    }
}

/// The app's world before anything runs in it: every component registered, the tokens, the primitives and the harness
/// scene (with its included widget files) loaded. Err names the first script that failed and flecs's error. No window,
/// Metal or daemon: the `scenes_load` test builds it headless.
fn scene_world() -> Result<World, String> {
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
    stats::register(&world);
    crate::keys::register(&world);
    view::register(&world);
    world.component::<HoverLift>().writable();
    world.component::<Bump>().writable();
    script::install(&world);
    let _ = world.entity_named(APP);
    world.set(Clock::default());
    world.set(PointerState::default());
    world.set(Paint::default());
    let mut scripts: Vec<(&str, &str)> = vec![("00-tokens", TOKENS)];
    scripts.extend(harness_native_ui_prims::SCENES.iter().copied());
    for (name, src) in scripts {
        let e = world.script_named(&format!("scene::{name}")).build_from_code(src);
        if let Some(err) = script_error(*e) {
            return Err(format!("scene script {name}: {err}"));
        }
    }
    harness_native_ecs::embed_files(SCENE);
    let e = world.script_named("scene::10-harness").build_from_file(SCENE[0].0);
    if let Some(err) = script_error(*e) {
        return Err(format!("scene script {}: {err}", SCENE[0].0));
    }
    // Each `include` is a managed script of its own, so its error is on its own entity, not the includer's.
    let mut bad = None;
    world.new_query::<&flecs::Script>().each_entity(|e, _| {
        if bad.is_none() {
            bad = script_error(e).map(|err| format!("scene script {}: {err}", e.name()));
        }
    });
    match bad {
        Some(err) => Err(err),
        None => Ok(world),
    }
}

impl App {
    pub fn new(wake: Wake) -> Result<Self, String> {
        let world = scene_world()?;
        let app = world.entity_named(APP);
        let w = world.world_ref();
        // The rule's delays for a finished run with no session and a past top-level session, from the scene's tokens
        // (seconds).
        world.set(model::ArchiveAfter {
            ms: (crate::look::width(w, "archive_after").max(0.0) * 1000.0) as u64,
            past_ms: (crate::look::width(w, "archive_past_after").max(0.0) as f64 * 1000.0) as u64,
        });
        harness_native_ui_layout::install(w);
        harness_native_ui_input::install(w);
        harness_native_ui_motion::install(w);
        crate::look::install(w);
        harness_native_ui_draw::install(w);
        // den-native-hover: each hoverable's hover spring toward whether the pointer is on it.
        let app_id = *app.id();
        // Each kicked Bump rings back to rest on its HoverLift's spring.
        world.system_named::<(&mut Bump, &HoverLift)>("Bump").each_iter(|it, _, (b, m)| {
            if settle(
                &mut b.spring,
                0.0,
                Tune { omega: m.omega.max(1.0), zeta: m.zeta.max(0.05) },
                it.delta_time(),
            ) {
                moving(&it.world());
            }
        });
        world.system_named::<&mut HoverAmt>("Hover").each_iter(move |it, row, h| {
            let (e, w, dt) = (it.entity(row), it.world(), it.delta_time());
            // On it, or on a part of it that takes the pointer itself (a row's disclosure chevron, a control deep in a
            // chat's drawn view, whose hover shows the view's code button).
            let on = w.entity_from_id(app_id).target(Hovered::id(), 0).is_some_and(|t| {
                std::iter::successors(Some(t), |n| n.parent()).take(HOVER_DEPTH).any(|n| n.id() == e.id())
            });
            if settle(&mut h.spring, f32::from(u8::from(on)), Tune { omega: 34.0, zeta: 1.0 }, dt) {
                moving(&w);
            }
        });
        let rx = model::Mailbox::default();
        let tx = rx.clone();
        let host = std::process::Command::new("scutil")
            .args(["--get", "ComputerName"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "This Mac".into());
        let (terms, run_terms, chats) =
            model::install(w, &host, &crate::link::sock().display().to_string(), rx);
        let key_file = keys::KeyFile::watch(wake.clone());
        let look = ghostty::Look::new(wake.clone());
        let link = Link::start(tx, wake);
        // The view's fixed frame: the window row, the sidebar (device row over the tree), the main card.
        let root = View::root(w, "hn.Window");
        let side = root.child("hn.SideClip").child("hn.Sidebar");
        let device = side.child("hn.DeviceRow");
        let side_w = crate::prefs::get("sidebar_width")
            .and_then(|v| v.as_f64())
            .map(|v| {
                let t = crate::look::side_drag(w);
                (v as f32).clamp(t.min, t.max)
            });
        if let Some(w) = side_w {
            set_side_width(side.entity(), w);
        }
        let tree = side.child("hn.Tree");
        let foot = side.child("hn.SideFoot");
        let link_note = foot.child("hn.LinkNote");
        let archive_row = foot
            .child("hn.ArchiveRow")
            .props("hn.ArchiveRow", &[("selected", Prop::Bool(false))]);
        archive_row.set(HoverAmt::default());
        // The panes: sync_panes makes the first (and swaps each one's prefab by what it shows).
        root.child("hn.Seam");
        let main = root.child("hn.Main");
        let bar = View::root(w, "hn.TitleBar");
        let ids = Ids {
            tree: tree.id(),
            device: device.id(),
            link_note: link_note.id(),
            main: main.id(),
            archive_row: archive_row.id(),
            palette: None,
            bar: bar.id(),
        };
        let mut paint = Painter::new(TypeLook { code_scale: 1.0, ..TypeLook::default() });
        let grid = Grid::new(&mut paint.fonts, TERM_SIZE, 2.0);
        let mut app = Self {
            world,
            paint: std::cell::RefCell::new(paint),
            terms,
            run_terms,
            chats,
            chat: std::cell::RefCell::default(),
            genui: std::cell::RefCell::default(),
            watching: HashMap::new(),
            pending_run: None,
            side_w,
            drag: side_drag::SideDrag::default(),
            link,
            grid,
            zoom: 4,
            ids,
            pointer: PointerState::default(),
            mods: KeyMods::default(),
            shape: (Pointer::Arrow, "none"),
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
                .or_else(|| {
                    crate::prefs::ids("archived")
                        .or_else(|| crate::prefs::ids("trashed"))
                        .map(|ids| ids.into_iter().map(|i| format!("s{i}")).collect())
                })
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
            keymap: crate::keys::Keymap::default(),
            key_file,
            keys_gen: 0,
            stats: stats::Stats::default(),
            look,
        };
        app.apply_ghostty();
        app.load_keys()?;
        // F3's overlay as it was left (native.json `stats`), or HARNESS_NATIVE_STATS=1|0 over that.
        let stats = match std::env::var("HARNESS_NATIVE_STATS").as_deref() {
            Ok("1") => true,
            Ok("0") => false,
            _ => crate::prefs::get("stats").and_then(|v| v.as_bool()).unwrap_or(false),
        };
        app.set_stats(stats);
        Ok(app)
    }

    /// The tooltip of the node under the pointer (or the nearest ancestor that has one); empty for none.
    pub fn tooltip(&self) -> String {
        let w = self.world.world_ref();
        let Some(t) = w.entity_named(APP).target(Hovered::id(), 0) else { return String::new() };
        std::iter::successors(Some(t), |n| n.parent())
            .take(6)
            .find_map(|n| n.try_cloned::<&harness_native_ui_input_api::Tooltip>())
            .map(|t| t.text.as_str().to_owned())
            .unwrap_or_default()
    }

    pub fn zoom(&self) -> f32 {
        ZOOMS[self.zoom]
    }

    /// Something changed: draw a few frames (the line that asked is the reason F3's overlay shows).
    #[track_caller]
    pub fn poke(&mut self) {
        self.poke_as(stats::Why::At(std::panic::Location::caller()));
    }

    /// [`poke`](Self::poke) with a reason tag (`daemon`, `key`, `layout`) for F3's overlay.
    pub fn poke_why(&mut self, tag: &'static str) {
        self.poke_as(stats::Why::Tag(tag));
    }

    fn poke_as(&mut self, why: stats::Why) {
        self.pending = self.pending.max(3);
        self.stats.note(why);
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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
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
            (Some(h), Some(t)) if status != "pending" && status != "skipped" => {
                format!("{h} \u{00B7} {t}")
            }
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
        let up = w
            .lookup(model::LOCAL)
            .target(model::Link::id(), 0)
            .is_some_and(|t| *t.id() == *w.component_id::<model::Up>());
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
            let first = rows
                .iter()
                .find(|(e, _)| self.status_of(*e) == "running")
                .or(rows.first())
                .map(|(e, _)| *e);
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
        let host = dev
            .try_cloned::<&model::Device>()
            .map(|d| d.host.as_str().to_owned())
            .unwrap_or_default();
        let link = dev.target(model::Link::id(), 0).map_or(0, |t| *t.id());
        let (link_tone, link_word) = if link == *w.component_id::<model::Up>() {
            (tone(w, "running"), format!("{}", rows.len()))
        } else if link == *w.component_id::<model::Down>() {
            (tone(w, "failed"), "offline".to_owned())
        } else if link == *w.component_id::<model::Refused>() {
            (tone(w, "failed"), "refused".to_owned())
        } else {
            (tone(w, "lost"), "connecting".to_owned())
        };
        if let Some(n) = device.part("name") {
            n.text(&host);
        }
        if let Some(n) = device.part("count") {
            n.text(&link_word);
        }
        // The refusal in words (which daemon, `harness stop`), in place of a spinner that never ends.
        let why = dev.try_cloned::<&model::LinkError>().map(|e| e.text.as_str().to_owned()).unwrap_or_default();
        let note = View::of(w.entity_from_id(self.ids.link_note)).text(&why);
        let h = if why.is_empty() { Sizing::fixed(0.0) } else { Sizing::FIT };
        if note.entity().try_cloned::<&Size>().is_none_or(|s| s.h != h) {
            note.set(Size { w: Sizing::GROW, h });
        }
        if let Some(d) = device.part("dot") {
            // A light only while the daemon is not up.
            set_if(d, if link == *w.component_id::<model::Up>() { clear } else { link_tone });
        }
        let tree = View::of(w.entity_from_id(self.ids.tree));
        let hint: HashMap<u64, usize> =
            rows.iter().enumerate().map(|(i, &(e, _))| (e, i)).collect();
        let info: Vec<(String, &str, model::Row)> = model::rows(w)
            .into_iter()
            .map(|r| match r {
                model::Row::Obj { e, .. } if self.run_of(e).is_some() => {
                    (format!("o{e}"), "hn.RunRow", r)
                }
                model::Row::Obj { e, .. } => (format!("o{e}"), "hn.ObjRow", r),
                model::Row::More { parent, .. } => (format!("more{parent}"), "hn.MoreRow", r),
            })
            .collect();
        // Rows that left the list stay while they fold away.
        let info = self.fold.merge(info, w);
        let me = &*self;
        // Each row is a template instance (scene/widgets/rows.flecs): the row's facts as props, written only when one
        // changed (View::props), and flecs re-runs only the parts that read it.
        view::for_each_of(tree, info, |row, r, new| {
            if new {
                row.set(HoverAmt::default());
            }
            let (e, depth) = match r {
                model::Row::Obj { e, depth } => (e, depth),
                model::Row::More { parent, hidden, depth, unfolded } => {
                    row.props(
                        "hn.MoreRow",
                        &[
                            ("obj", Prop::Uint(parent)),
                            ("depth", Prop::Int(depth as i64)),
                            ("hidden", Prop::Int(hidden as i64)),
                            ("unfolded", Prop::Bool(unfolded)),
                        ],
                    );
                    return;
                }
            };
            let o = w.entity_from_id(e);
            let status = me.status_of(e);
            let failed = status == "killed"
                || o.try_cloned::<&model::ExitCode>().is_some_and(|c| c.code != 0);
            // One line: the part is a ui.Line, cut with an ellipsis to the room its row leaves it when drawn.
            let name = o
                .try_cloned::<&model::Name>()
                .map(|n| n.s.as_str().split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap_or_default();
            let common = [
                ("obj", Prop::Uint(e)),
                ("name", Prop::Str(&name)),
                ("selected", Prop::Bool(sel == Some(e))),
                ("depth", Prop::Int(depth as i64)),
                ("kids", Prop::Bool(fold::has_kids(o))),
                ("open", Prop::Bool(!o.has(model::Collapsed::id()))),
            ];
            if me.run_of(e).is_some() {
                // A run: a small dot tinted by how it went, breathing while it runs; how long it took (and how it failed)
                // shown on hover, a failure's always.
                let state = match status {
                    "running" => "running",
                    "lost" => "lost",
                    _ if failed => "failed",
                    _ => "faint",
                };
                let meta = me.run_meta(e);
                let keep = failed || matches!(status, "lost" | "pending" | "skipped");
                row.props(
                    "hn.RunRow",
                    &[
                        &common[..],
                        &[
                            ("state", Prop::Str(state)),
                            ("meta", Prop::Str(&meta)),
                            ("keep", Prop::Bool(keep)),
                        ],
                    ]
                    .concat(),
                );
            } else {
                // A session: its kind's mark, its shortcut on hover, a failure's exit code as a small red badge.
                let hint = hint
                    .get(&e)
                    .filter(|&&i| i < 9)
                    .map(|i| me.hint(&format!("Select{}", i + 1)))
                    .unwrap_or_default();
                let code = o.try_cloned::<&model::ExitCode>().map(|c| c.code).filter(|&c| c != 0);
                let badge = match (status, code) {
                    ("killed", _) => "\u{2715}".to_owned(),
                    (_, Some(c)) => c.to_string(),
                    _ => String::new(),
                };
                row.props(
                    "hn.ObjRow",
                    &[
                        &common[..],
                        &[
                            ("kind", Prop::Str(model::kind_name(o))),
                            ("badge", Prop::Str(&badge)),
                            ("hint", Prop::Str(&hint)),
                        ],
                    ]
                    .concat(),
                );
            }
        });
        self.fold.animate(
            self.world.entity_from_id(self.ids.tree),
            crate::look::tune(self.world.world_ref(), "tree_fold"),
        );
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
        let (title, _subtitle) = match sel.map(|e| w.entity_from_id(e)) {
            Some(o) => {
                let e = *o.id();
                let kind = model::kind_name(o);
                let name = o
                    .try_cloned::<&model::Name>()
                    .map(|n| n.s.as_str().to_owned())
                    .unwrap_or_default();
                let t = o
                    .try_cloned::<&model::Title>()
                    .map(|n| n.s.as_str().to_owned())
                    .unwrap_or_default();
                let cwd =
                    o.try_cloned::<&model::Cwd>().map(|n| tilde(n.s.as_str())).unwrap_or_default();
                let code = o.try_cloned::<&model::ExitCode>().map(|c| c.code);
                let join = |v: Vec<String>| {
                    v.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("  \u{00B7}  ")
                };
                match (self.run_of(e), model::pane_kind(o)) {
                    // A run: its id and command (one line), then where and how it stands.
                    (Some((n, stage)), _) => {
                        let id = stage.map_or(format!("r{n}"), |k| format!("r{n}.{k}"));
                        let cmd = name.split_whitespace().collect::<Vec<_>>().join(" ");
                        (format!("{id}  {cmd}"), join(vec![self.run_cwd(e), self.run_state(e)]))
                    }
                    (None, model::PaneKind::Archive) => (name, self.archive_subtitle()),
                    _ => {
                        // The program-set title (OSC 0/2) never names anything (agents' titles are spinner frames and
                        // "Claude Code"): only a shell's shows, in place of its directory.
                        let place =
                            if kind == "shell" && !t.is_empty() && t != name { t } else { cwd };
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
        self.title = if sel.is_some() {
            format!("{} \u{2014} harness", clip_text(&title, 60))
        } else {
            "harness".to_owned()
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
        self.world
            .new_query::<&model::Object>()
            .each_entity(|e, o| objs.push((*e.id(), format!("s{}", o.id))));
        self.world
            .new_query::<&model::Run>()
            .each_entity(|e, r| objs.push((*e.id(), format!("r{}", r.n))));
        for (e, key) in &objs {
            let e = w.entity_from_id(*e);
            for (on, tag) in [
                (self.closed.contains(key), *w.component_id::<model::Closed>()),
                (self.pinned.contains(key), *w.component_id::<model::Pinned>()),
            ] {
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

    /// The Archive's row at the sidebar's bottom (hn.ArchiveRow): lit while the Archive, or an object opened from it, is
    /// selected.
    fn sync_archive(&mut self) {
        let w = self.world.world_ref();
        let lit = self.selected().is_some_and(|s| {
            s == model::archive_view(w) || w.entity_from_id(s).has(model::Archived::id())
        });
        View::of(w.entity_from_id(self.ids.archive_row))
            .props("hn.ArchiveRow", &[("selected", Prop::Bool(lit))]);
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
            if let (Some(m), Some(mut b)) = (e.try_cloned::<&HoverLift>(), e.try_cloned::<&Bump>())
            {
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
                        n.strip_prefix("select:")
                            .or_else(|| n.strip_prefix("arch:open:"))
                            .and_then(|s| s.parse::<u64>().ok())
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
    fn entries(&self) -> Vec<(String, PalGlyph, String, String, Do)> {
        if self.picking() {
            return self
                .picks()
                .into_iter()
                .map(|p| {
                    (format!("a{}", p.act), PalGlyph::Kind(p.mark), p.title, p.hint, Do::Act(p.act))
                })
                .collect();
        }
        let w = self.world.world_ref();
        // A new object of each kind, run as its act.Spawn (as its keys and the picker run it); each hint is the binding
        // that runs the same action (scene/20-keys.flecs, or the user's keys.flecs).
        let mut out: Vec<(String, PalGlyph, String, String, Do)> = self
            .spawns()
            .into_iter()
            .map(|p| {
                (format!("new-{}", p.kind), PalGlyph::Kind(p.mark), p.menu, p.hint, Do::Act(p.act))
            })
            .collect();
        for (i, e) in self.objects().into_iter().enumerate() {
            let o = w.entity_from_id(e);
            let name =
                o.try_cloned::<&model::Name>().map(|n| n.s.as_str().to_owned()).unwrap_or_default();
            let hint = if i < 9 { self.hint(&format!("Select{}", i + 1)) } else { String::new() };
            out.push((
                format!("obj{e}"),
                PalGlyph::Kind(model::kind_name(o).into()),
                name,
                hint,
                Do::Select(e),
            ));
        }
        // The Archive, then what is in it (newest first; a run by its id and command).
        let view = model::archive_view(w);
        out.push((
            "archive".into(),
            PalGlyph::Char(crate::look::ARCHIVE_ICON),
            "Open Archive".into(),
            String::new(),
            Do::Select(view),
        ));
        for e in model::archived(w).into_iter().rev() {
            let o = w.entity_from_id(e);
            let name = o
                .try_cloned::<&model::Name>()
                .map(|n| n.s.as_str().split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap_or_default();
            let name = match self.run_of(e) {
                Some((n, _)) => format!("r{n}  {name}"),
                None => name,
            };
            out.push((
                format!("arc{e}"),
                PalGlyph::Kind(model::kind_name(o).into()),
                name,
                "archived".into(),
                Do::Archived(e),
            ));
        }
        // While searching, every run the world holds that is not listed yet (in the tree, or found in older history by
        // the daemon's search), newest first.
        let q = self.palette.as_ref().map(|p| p.query.trim().to_lowercase()).unwrap_or_default();
        if !q.is_empty() {
            let mut runs: Vec<(u64, u64)> = Vec::new();
            self.world.new_query::<&model::Run>().each_entity(|e, r| {
                if !e.has(model::Archived::id()) {
                    runs.push((r.n, *e.id()));
                }
            });
            runs.sort_unstable_by(|a, b| b.cmp(a));
            for (n, e) in runs {
                let o = w.entity_from_id(e);
                let cmd = o
                    .try_cloned::<&model::Name>()
                    .map(|n| n.s.as_str().split_whitespace().collect::<Vec<_>>().join(" "))
                    .unwrap_or_default();
                out.push((
                    format!("run{e}"),
                    PalGlyph::Kind("proc".into()),
                    format!("r{n}  {cmd}"),
                    "run".into(),
                    Do::Select(e),
                ));
            }
        }
        out.push((
            "kill".into(),
            PalGlyph::Char(crate::look::KILL_ICON),
            "Kill current object".into(),
            self.hint("Archive"),
            Do::Kill,
        ));
        out.push((
            "sidebar".into(),
            PalGlyph::Char(crate::look::SIDEBAR_ICON),
            "Toggle sidebar".into(),
            self.hint("ToggleSidebar"),
            Do::Sidebar,
        ));
        out.push((
            "zoom-in".into(),
            PalGlyph::Char(crate::look::ZOOM_IN_ICON),
            "Zoom in".into(),
            self.hint("ZoomIn"),
            Do::Zoom(1),
        ));
        out.push((
            "zoom-out".into(),
            PalGlyph::Char(crate::look::ZOOM_OUT_ICON),
            "Zoom out".into(),
            self.hint("ZoomOut"),
            Do::Zoom(-1),
        ));
        let q = self.palette.as_ref().map(|p| p.query.to_lowercase()).unwrap_or_default();
        out.retain(|(_, _, name, _, _)| fuzzy(&name.to_lowercase(), &q));
        out
    }

    fn sync_palette(&mut self) {
        // Older history than the world holds: once typing pauses, the daemon searches every run; what it finds joins the
        // world (as a page would) and so the palette's entries.
        let (time, mut wait) = (self.time, false);
        if let Some(p) = self.palette.as_mut() {
            let q = p.query.trim().to_owned();
            if q != p.searched {
                if q.chars().count() >= 2 && time - p.changed >= PALETTE_DEBOUNCE {
                    self.link.runs_search(&q);
                    p.searched = q;
                } else {
                    wait = true;
                }
            }
        }
        if wait {
            self.poke();
        }
        let w = self.world.world_ref();
        let want = self.palette.as_ref().map(|p| (p.mode, p.at, p.query.clone()));
        // The overlay on screen is no longer wanted (closed, or the other mode opened): it goes out on its springs.
        if let Some((id, mode)) = self.ids.palette
            && want.as_ref().is_none_or(|(m, ..)| *m != mode)
        {
            leave_overlay(View::of(w.entity_from_id(id)));
            self.ids.palette = None;
        }
        let Some((mode, at, query)) = want else { return };
        let root = match self.ids.palette {
            Some((id, _)) => View::of(w.entity_from_id(id)),
            None => {
                let v = View::root(
                    w,
                    match mode {
                        Mode::Commands => "hn.Palette",
                        Mode::New => "hn.Picker",
                    },
                );
                self.ids.palette = Some((v.id(), mode));
                v
            }
        };
        let card = root.part("card");
        if mode == Mode::New {
            self.sync_picker(card, at);
            return;
        }
        let shown =
            if query.is_empty() { "Search objects and commands\u{2026}".to_owned() } else { query };
        if let Some(t) = card.and_then(|c| c.part("query")).and_then(|q| q.part("text")) {
            t.text(&shown);
        }
        let entries = self.entries();
        let at = at.min(entries.len().saturating_sub(1));
        let (clear, chosen) = (tone(w, "clear"), tone(w, "selected"));
        if let Some(items) = card.and_then(|c| c.part("items")) {
            let rows = entries
                .into_iter()
                .enumerate()
                .take(12)
                .map(|(i, (k, g, n, h, _))| (k, (i, g, n, h)));
            for_each(items, "hn.PalRow", rows, |row, (i, g, n, h), new| {
                if new {
                    row.set(HoverAmt::default());
                }
                row.action(&format!("pal:{i}"));
                set_if(row, if i == at { chosen } else { clear });
                if let Some(p) = row.part("glyph") {
                    match g {
                        PalGlyph::Kind(k) => crate::look::kind_mark(w, p.entity(), &k),
                        PalGlyph::Char(c) => {
                            crate::look::show_glyph(p.entity(), c);
                            set_if(p, tone(w, "muted"));
                        }
                    }
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

    /// The picker's rows (scene/widgets/picker.flecs `hn.PickRow`), one for each act.Spawn, the highlighted one (`at`)
    /// holding the pill.
    fn sync_picker(&self, card: Option<View<'_>>, at: usize) {
        let Some(items) = card.and_then(|c| c.part("items")) else { return };
        let picks = self.picks();
        let at = at.min(picks.len().saturating_sub(1));
        let rows = picks.iter().enumerate().map(|(i, p)| (format!("a{}", p.act), (i, p)));
        for_each(items, "hn.PickRow", rows, |row, (i, p), new| {
            if new {
                row.set(HoverAmt::default());
            }
            row.props(
                "hn.PickRow",
                &[
                    ("obj", Prop::Uint(i as u64)),
                    ("name", Prop::Str(&p.title)),
                    ("selected", Prop::Bool(i == at)),
                    ("kind", Prop::Str(&p.mark)),
                    ("hint", Prop::Str(&p.hint)),
                    ("cap", Prop::Str(&p.cap)),
                ],
            );
        });
    }

    /// The picker's rows: the act.Spawn actions with a title, in their order.
    fn picks(&self) -> Vec<Pick> {
        self.spawns().into_iter().filter(|p| !p.title.is_empty()).collect()
    }

    /// Every act.Spawn action (scene/20-keys.flecs and the user's keys.flecs), in their order.
    fn spawns(&self) -> Vec<Pick> {
        let w = self.world.world_ref();
        let ctx = w.try_lookup(crate::keys::PICKER).map(|e| *e.id());
        self.keymap
            .acts
            .iter()
            .filter_map(|a| {
                let k = w.entity_from_id(a.id).try_cloned::<&crate::keys::SpawnKind>()?;
                let kind = k.kind.as_str().to_owned();
                let title = k.title.as_str().to_owned();
                let mark = kind.split('-').next().unwrap_or_default().to_owned();
                let menu = match &a.menu {
                    Some((_, t, _)) => t.clone(),
                    None if !title.is_empty() => format!("New {title}"),
                    None => format!("New {}", capitalised(&kind)),
                };
                let hint =
                    self.keymap.primary(a.id).map(crate::keys::Chord::hint).unwrap_or_default();
                let cap = ctx
                    .and_then(|c| self.keymap.in_context(a.id, c))
                    .map(crate::keys::Chord::hint)
                    .unwrap_or_default();
                Some(Pick { act: a.id, kind, mark, title, menu, hint, cap })
            })
            .collect()
    }

    /// Whether the overlay open is the new-object picker.
    fn picking(&self) -> bool {
        self.palette.as_ref().is_some_and(|p| p.mode == Mode::New)
    }

    /// Open the new-object picker (cmd+N), the kind picked last highlighted (native.json `new_kind`), its pill landing
    /// there rather than sliding from where it was last drawn.
    pub fn open_picker(&mut self) {
        let last = crate::prefs::get("new_kind").and_then(|v| v.as_str().map(str::to_owned));
        let at = last.and_then(|k| self.picks().iter().position(|p| p.kind == k)).unwrap_or(0);
        if let Some(m) = self.world.world_ref().try_lookup("ui_matched::hn_picker") {
            m.destruct();
        }
        self.palette = Some(Palette { mode: Mode::New, at, ..Palette::new() });
        self.poke();
    }

    /// An object of `kind` is being made by an act.Spawn: from the picker, the choice is kept for its next opening (not
    /// from an offscreen shot); any overlay open closes.
    fn spawning(&mut self, kind: &str) {
        if self.picking() && self.persist {
            crate::prefs::set("new_kind", serde_json::json!(kind));
        }
        self.palette = None;
    }

    /// Move the overlay's highlight by `by` rows: the palette's stops at its ends, the picker's few rows go round.
    fn palette_step(&mut self, by: i64) {
        let n = self.entries().len() as i64;
        let Some(p) = self.palette.as_mut() else { return };
        if n == 0 {
            return;
        }
        let at = p.at as i64 + by;
        p.at = match p.mode {
            Mode::New => at.rem_euclid(n),
            Mode::Commands => at.clamp(0, n - 1),
        } as usize;
    }

    /// An entry chosen (Return, or a click): it runs, and the overlay closes. A picker row runs its act.Spawn with the
    /// picker still open, so the action sees where it came from (`spawning`).
    fn choose(&mut self, d: Do) {
        if !matches!(d, Do::Act(_)) {
            self.palette = None;
        }
        self.run(d);
        self.palette = None;
    }

    // ---- the frame ----

    /// One frame at `points` (the window's size in points) on a display of `backing` pixels a point.
    pub fn frame(
        &mut self,
        points: (f32, f32),
        backing: f32,
        dt: f32,
        upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8]),
    ) -> Draw {
        self.time += dt;
        self.pending = self.pending.saturating_sub(1);
        self.stats_begin();
        self.reload_keys_if_changed();
        self.reload_ghostty_if_changed();
        let z = self.zoom();
        let view = (points.0 / z, points.1 / z);
        let scale = backing * z;
        if self.grid.scale != scale {
            // Cells are whole device pixels: new metrics at a new scale (the pane's grid may change size with them).
            self.grid = Grid::new(&mut self.paint.get_mut().fonts, self.look.size, scale);
        }
        self.step_sidebar(dt);
        self.step_side_drag(dt);
        self.lay_chrome(z);
        self.step_fold();
        self.step_tree_scroll(dt);
        self.step_chat_scroll();
        let w = self.world.world_ref();
        if w.try_get::<&Viewport>(|v| (v.w, v.h)) != Some(view) {
            self.world.set(Viewport { w: view.0, h: view.1 });
        }
        self.world.set(self.pointer);
        self.world.get::<&mut Clock>(|c| {
            c.time += dt;
            c.moving = false;
        });
        let before = self.world.try_get::<&model::Inbox>(|i| i.applied);
        self.sync_view();
        self.typeset();
        self.progress(dt);
        if self.world.try_get::<&model::Inbox>(|i| i.applied) != before {
            // The daemon said something: the view follows in this frame.
            self.sync_view();
            self.poke_why("daemon");
        }
        for _ in 0..3 {
            if self.typeset() == 0 {
                break;
            }
            self.progress(0.0);
            self.poke_why("layout");
        }
        self.forward_clicks();
        self.actions();
        harness_native_ui_input::end_frame(self.world.world_ref());
        self.pointer.went_down = false;
        self.pointer.went_up = false;
        if self.world.try_get::<&Clock>(|c| c.moving).unwrap_or(false) {
            self.poke_why("animation");
        }
        self.sync_terms();
        self.resolve_pointer();
        let mut d = Draw {
            frame: Frame {
                view: [view.0, view.1],
                scale,
                time: self.time,
                dark: f32::from(u8::from(self.dark)),
                live: scale,
                textfx: [1.0, 0.0],
                tune: [0.82, 1.12, 1.0, 0.0],
                slug: [0.0, 1.0, 1.0, 0.0],
            },
            // Clear: the window's vibrancy shows through everywhere the scene paints nothing.
            clear: [0.0; 4],
            ..Draw::default()
        };
        self.emit(&mut d, scale, upload);
        if self.paint.get_mut().end_frame() {
            self.poke();
        }
        self.stats_end(&d);
        d
    }

    /// One `progress` of the world, its draw pass under a new `Paint` number (the contract: advanced before each
    /// `progress`). A frame runs several while text settles, and only what the last one painted is drawn: a node a later
    /// pass hid, faded out or clipped away does not keep the number of an earlier one.
    fn progress(&mut self, dt: f32) {
        let next = self.world.try_get::<&Paint>(|p| p.frame).unwrap_or(0).wrapping_add(1);
        self.world.set(Paint { frame: next });
        self.world.progress_time(dt);
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
        bar.set(Padding { l: (width + self.look.pad[0]).max(past_lights), ..bpad });
    }

    /// Step the sidebar's fold spring toward open or folded (cmd+S).
    fn step_sidebar(&mut self, dt: f32) {
        // Folded, the lights come back while the pointer is over their corner: as wide as they reach, as tall as the strip.
        let z = self.zoom();
        let (px, py) = (self.pointer.x, self.pointer.y);
        let corner = !self.side_open
            && self.chrome.strip > 0.0
            && px >= 0.0
            && py >= 0.0
            && px < self.chrome.title_x / z
            && py < self.chrome.strip / z;
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

    /// Size every `Text` node with the typesetter; how many changed.
    fn typeset(&mut self) -> usize {
        self.paint.get_mut().typeset(self.world.world_ref())
    }

    /// What was activated this frame: a row selects its object, a palette entry runs.
    /// Tell the scripts about this frame's clicks (each `Activated` node, src/app/script.rs): a widget's
    /// `await on.click(this)` wakes, and what it asks for (`hn.act`) runs with the actions.
    fn forward_clicks(&mut self) {
        let mut hit = Vec::new();
        self.world.new_query::<&Activated>().each_entity(|e, _| hit.push(*e.id()));
        for e in hit {
            script::click(&self.world, e, self.press.1.alt);
        }
    }

    fn actions(&mut self) {
        let alt = self.press.1.alt;
        let mut acts = Vec::new();
        self.world.new_query::<&Action>().each_entity(|e, a| {
            if e.has(Activated::id()) && !a.name.as_str().is_empty() {
                acts.push((a.name.as_str().to_owned(), alt));
            }
        });
        acts.extend(script::drain());
        for (a, alt) in acts {
            self.poke();
            if let Some(e) = a.strip_prefix("select:").and_then(|s| s.parse().ok()) {
                // A double click on a row with rows under it folds or unfolds it too.
                let again = self.press.0 >= 2 && self.selected() == Some(e);
                self.select(Some(e));
                self.tree_focus = true;
                if again && fold::has_kids(self.world.entity_from_id(e)) {
                    self.toggle_fold_of(e, alt);
                }
            } else if let Some(e) = a
                .strip_prefix("toggle:")
                .or_else(|| a.strip_prefix("collapse:"))
                .and_then(|s| s.parse().ok())
            {
                self.tree_focus = true;
                self.toggle_fold_of(e, alt);
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
            } else if let Some(b) = a.strip_prefix("chat:open:") {
                self.chat_toggle(b);
            } else if let Some(rest) = a.strip_prefix("chat:row:") {
                self.chat_row_toggle(rest);
            } else if let Some(ev) = a.strip_prefix("ui:") {
                self.genui_action(ev);
            } else if let Some(target) = a.strip_prefix("open:") {
                // A link in a view (the browser), or a file (a cell's FileRef) in the app its kind opens in, at its line.
                crate::open::open(target);
            } else if a == "chat:focus" {
                self.tree_focus = false;
            } else if a == "pal:close" {
                self.palette = None;
            } else if let Some(i) = a.strip_prefix("pal:").and_then(|s| s.parse::<usize>().ok()) {
                if let Some((.., d)) = self.entries().into_iter().nth(i) {
                    self.choose(d);
                }
            }
        }
    }

    fn run(&mut self, d: Do) {
        match d {
            Do::Act(id) => {
                self.run_action(id, None);
            }
            Do::Select(e) => self.select(Some(e)),
            Do::Archived(e) => self.open_from_archive(e),
            Do::Kill => self.kill(),
            Do::Sidebar => self.toggle_sidebar(),
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
            let laid =
                if p.w > 0.0 || p.h > 0.0 { Laid { x: p.x, y: p.y, w: p.w, h: p.h } } else { laid };
            // A breathing dot dims and brightens; a hover-only part fades with its row's hover.
            p.alpha *= crate::look::alpha(w, e, lo);
            let is_term = e.has(TermNode::id());
            let scroller_part = e.has(harness_native_ui_layout_api::ScrollerPart::id());
            // Nothing is drawn that cannot be seen, a terminal's thousands of cells least of all.
            if !seen(&p) {
                continue;
            }
            let rect = [laid.x, laid.y, laid.w, laid.h];
            // F3's sparkline: bars, not a plate.
            if e.has(stats::StatsSpark::id()) {
                self.emit_spark(d, e, rect, p.alpha);
                continue;
            }
            // A HoverLift node: how far its row's hover and its bump have moved it (read before the drawing borrows self).
            let motion = e.try_cloned::<&HoverLift>().map(|m| {
                let hover = e
                    .parent()
                    .and_then(|r| r.try_cloned::<&HoverAmt>())
                    .map_or(0.0, |h| h.spring.x.clamp(0.0, 1.0));
                let bump = e.try_cloned::<&Bump>().map_or(0.0, |b| b.spring.x);
                (
                    -(m.lift * hover + m.kick_lift * bump),
                    (1.0 + (m.scale - 1.0) * hover + m.kick_scale * bump).max(0.1),
                )
            });
            let at = match self.paint.get_mut().node(d, e, &p, laid, p.dark(dark), held == Some(id), sc, upload) {
                Ok(at) => at,
                Err(err) => {
                    eprintln!("harness-native: paint: {err}");
                    continue;
                }
            };
            let (np, ng) = (at.panels, at.glyphs);
            let turn = (p.kind == PaintKind::Icon).then(|| crate::look::turn(e)).flatten();
            if is_term && p.kind == PaintKind::Plate {
                let cells = (d.panels.len(), d.glyphs.len(), d.sprites.len());
                self.emit_term(id, d, sc, upload);
                // Its cells at the node's opacity, as every other node's paint is (a fade, a Pose): never popped in whole.
                if p.alpha < 1.0 {
                    fade_cells(d, cells, p.alpha);
                }
            }
            // A disclosure chevron turns about its middle.
            if let Some(turn) = turn {
                for g in &mut d.glyphs[ng..] {
                    g.clip[2] = turn;
                }
            }
            // A HoverLift node: its glyphs risen and scaled about its centre by its row's hover and its bump.
            if let Some((dy, k)) = motion {
                let (cx, cy) = (laid.x + laid.w * 0.5, laid.y + laid.h * 0.5);
                for g in &mut d.glyphs[ng..] {
                    g.rect = [
                        cx + (g.rect[0] - cx) * k,
                        cy + (g.rect[1] - cy) * k + dy,
                        g.rect[2] * k,
                        g.rect[3] * k,
                    ];
                }
            }
            // Cut to its band: soft at an edge with more beyond it (its clip's ClipFade: the tree's, a chat's; not a scroller).
            if p.clipped && !scroller_part && p.fade != [0.0; 2] {
                for panel in &mut d.panels[np..] {
                    panel.clip[2..].copy_from_slice(&p.fade);
                }
                for glyph in &mut d.glyphs[ng..] {
                    glyph.fade[..2].copy_from_slice(&p.fade);
                }
            }
            let _ = Level::Base;
        }
    }

    /// Text typed into the open palette (a key no binding takes): its query.
    fn palette_type(&mut self, chars: &str) {
        let time = self.time;
        let Some(p) = self.palette.as_mut().filter(|p| p.mode == Mode::Commands) else { return };
        let typed: String = chars
            .chars()
            .filter(|c| !c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(c))
            .collect();
        if !typed.is_empty() {
            p.query.push_str(&typed);
            p.at = 0;
            p.changed = time;
        }
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

    /// One line of what a shot sequence measures (src/shot.rs `HARNESS_NATIVE_SHOT_SEQ`): the fold spring, the focused
    /// terminal's grid origin and columns, the selection pill's drawn y and the tree's scroll.
    pub fn probe(&self) -> String {
        let w = self.world.world_ref();
        let grid = self
            .focused_term()
            .and_then(|t| w.entity_from_id(t).try_cloned::<&crate::pane::TermGrid>());
        let pill = w
            .try_lookup("ui_matched::hn_selection")
            .and_then(|h| h.try_cloned::<&harness_native_ui_motion_api::Matched>());
        let drawn = pill.and_then(|m| {
            w.entity_from_id(m.owner).try_cloned::<&harness_native_ui_motion_api::Drawn>()
        });
        format!(
            "side={:.4} open={} grid_x={:.2} cols={} pill_y={:.2} pill_v={:.1} scroll={:.2}",
            self.side_x.x,
            self.side_open,
            grid.map_or(-1.0, |g| g.x),
            grid.map_or(0, |g| g.cols),
            drawn.map_or(-1.0, |d| d.y),
            pill.map_or(0.0, |m| m.y.v),
            w.entity_from_id(self.ids.tree)
                .try_cloned::<&harness_native_ui_layout_api::ScrollState>()
                .map_or(0.0, |s| s.y)
        )
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
        let at = self
            .selected()
            .and_then(|s| objs.iter().position(|&e| e == s))
            .map_or(if by > 0 { -1 } else { n }, |i| i as i64);
        self.select(Some(objs[(at + by).rem_euclid(n) as usize]));
    }

    /// Command-= / Command-- / Command-0: the UI's scale steps (layout, drawing and input all in view points, window
    /// points over the zoom). The pointer has not moved on the screen, so it stays where it is in window points: in view
    /// points it scales with the zoom (else hover and hits would land on what was under it before the zoom). Matched
    /// geometry lands on its rows (motion sees the room change), so nothing slides.
    pub fn step_zoom(&mut self, by: i32) {
        let was = self.zoom();
        self.zoom = match by {
            0 => 4,
            b if b > 0 => (self.zoom + 1).min(ZOOMS.len() - 1),
            _ => self.zoom.saturating_sub(1),
        };
        let k = was / self.zoom();
        self.pointer.x *= k;
        self.pointer.y *= k;
        self.poke();
    }

    fn spawn(&mut self, kind: &str) {
        let cwd = self
            .selected()
            .and_then(|e| self.world.entity_from_id(e).try_cloned::<&model::Cwd>())
            .map(|c| c.s.as_str().to_owned())
            .filter(|c| !c.is_empty())
            .or_else(|| std::env::var("HOME").ok());
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
            let up = self
                .world
                .entity_from_id(e)
                .parent()
                .filter(|p| p.has(model::Object::id()))
                .map(|p| *p.id());
            self.archive_object(e);
            self.select(up);
            return;
        }
        let Some(sid) = self.session_of(e) else { return };
        // The next object takes its place.
        let objs = self.objects();
        let next = objs
            .iter()
            .position(|&o| o == e)
            .and_then(|i| objs.get(i + 1).or_else(|| i.checked_sub(1).and_then(|j| objs.get(j))))
            .copied();
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
        self.palette = Some(Palette::new());
        self.poke();
    }

    /// Type `q` into the open palette (the shot's `HARNESS_NATIVE_SHOT_PALETTE=<query>`).
    pub fn palette_query(&mut self, q: &str) {
        let time = self.time;
        if let Some(p) = self.palette.as_mut() {
            p.query = q.to_owned();
            p.changed = time;
        }
        self.poke();
    }
}

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

/// Whether what the draw pass painted can be seen: opaque past the cut and not clipped to nothing. Only that is drawn
/// (`App::emit`), and only a terminal painted so this frame is laid out, followed and hit (src/app/panes.rs).
pub(super) fn seen(p: &harness_native_ui_draw_api::Painted) -> bool {
    p.alpha > 0.002 && !(p.clipped && (p.clip_w <= 0.0 || p.clip_h <= 0.0))
}

/// The panels, glyphs and sprites drawn from `from` on (a terminal's cells) at `alpha` of their opacity.
fn fade_cells(d: &mut Draw, from: (usize, usize, usize), alpha: f32) {
    for panel in &mut d.panels[from.0..] {
        panel.fx2[1] *= alpha;
    }
    for glyph in &mut d.glyphs[from.1..] {
        glyph.col_d[3] *= alpha;
        glyph.col_l[3] *= alpha;
    }
    for sprite in &mut d.sprites[from.2..] {
        sprite.fg[3] *= alpha;
        sprite.bg[3] *= alpha;
    }
}

/// An overlay sent away: its scrim fades and its card shrinks back on their Transitions (scene/widgets/palette.flecs),
/// taking no pointer while they go, and motion deletes them at rest.
fn leave_overlay(root: View<'_>) {
    if let Some(card) = root.part("card") {
        card.add(harness_native_ui_motion_api::Leaving::id());
    }
    view::leave(root);
}

/// `kind` with its first letter upper case (a picker row's title when its act.Spawn names none).
fn capitalised(kind: &str) -> String {
    let mut c = kind.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
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
}

fn set_if(v: View<'_>, fill: Fill) {
    if v.entity().try_cloned::<&Fill>() != Some(fill) {
        v.set(fill);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tree as the daemon's `list` and `runs` tell it, at change `n` (the title and the newest run move each time).
    fn tree(n: usize) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
        let sessions = (1..=3)
            .map(|id| serde_json::json!({"id": id, "kind": "shell", "status": "running", "title": format!("t{n}"), "busy": n % 2 == 0}))
            .collect();
        let runs = (0..100)
            .map(|r| serde_json::json!({"id": format!("r{}", n + r), "status": "done", "cmd": "echo hi", "session": "s1", "stages": []}))
            .collect();
        (sessions, runs)
    }

    /// Property `stalled-inbox-holds-one-tree`: while no frame takes the mail (a window out of sight, so its display link
    /// stops calling), the daemon announcing a change 20 times a second for ten minutes leaves at most one `list` and
    /// one `runs` answer waiting, not one per change (each a copy of every session and the last 100 runs: what grew the
    /// app to 176 GB); and the frame that comes after applies the newest of each, so the world's entities are the same
    /// as after a frame per change.
    #[test]
    fn stalled_inbox_holds_one_tree() {
        let world = scene_world().unwrap_or_else(|e| panic!("{e}"));
        let mail = model::Mailbox::default();
        let _ = model::install(world.world_ref(), "test", "/nonexistent", mail.clone());
        mail.send(model::Msg::Link(model::LinkState::Up));
        let changes = 20 * 600;
        for n in 0..changes {
            let (sessions, runs) = tree(n);
            mail.send(model::Msg::List(sessions));
            mail.send(model::Msg::Runs(runs));
            mail.send(model::Msg::Data { id: 1, bytes: b"x".to_vec() });
            assert!(mail.len() <= 3 + n + 1, "change {n}: {} messages wait", mail.len());
        }
        // Only the streamed bytes queue (one per change here): the tree is one answer of each, not `changes` of each.
        assert_eq!(mail.len(), 1 + 2 + changes, "the link state, one list, one runs and the bytes");
        world.progress_time(1.0 / 60.0);
        assert!(mail.is_empty());
        let stalled = world.world_ref().alive_count();
        let mut titles = Vec::new();
        world.new_query::<&model::Title>().each_entity(|_, t| titles.push(t.s.as_str().to_owned()));
        let title = titles.first().cloned();
        assert_eq!(title.as_deref(), Some(format!("t{}", changes - 1).as_str()), "the newest list was applied");

        // The same changes, a frame after each: the world ends with as many entities.
        let world2 = scene_world().unwrap_or_else(|e| panic!("{e}"));
        let mail2 = model::Mailbox::default();
        let _ = model::install(world2.world_ref(), "test", "/nonexistent", mail2.clone());
        mail2.send(model::Msg::Link(model::LinkState::Up));
        for n in 0..changes {
            let (sessions, runs) = tree(n);
            mail2.send(model::Msg::List(sessions));
            mail2.send(model::Msg::Runs(runs));
            world2.progress_time(1.0 / 60.0);
        }
        assert_eq!(world2.world_ref().alive_count(), stalled, "a stalled inbox ends with the same world");
    }

    /// Zoom is a uniform scale with nothing sliding: the picker's highlight (a MatchedGeometry pill on a row of a card
    /// centred in the window, so the zoom moves the row in view points) is mid-slide to a new row when cmd+= lands; in that
    /// same frame the pill sits exactly on its row, and the card is the zoom times as big in window points. It fails when
    /// matched geometry runs before layout in a pass (flecs orders a phase by recycled entity ids: it read the last pass's
    /// places) or slides across a change of room. Headless (the scene world, layout and motion; no window, Metal or daemon).
    #[test]
    fn zoom_lands_the_pill_on_its_row() {
        use harness_native_ui_motion_api::Drawn;
        let world = scene_world().unwrap_or_else(|e| panic!("{e}"));
        let w = world.world_ref();
        harness_native_ui_layout::install(w);
        harness_native_ui_motion::install(w);
        world.set(Clock::default());
        let (win_w, win_h) = (1280.0_f32, 800.0_f32);
        let frame = |z: f32| {
            let room = Viewport { w: win_w / z, h: win_h / z };
            if w.try_get::<&Viewport>(|v| *v) != Some(room) {
                world.set(room);
            }
            world.get::<&mut Clock>(|c| c.time += 1.0 / 60.0);
            world.progress_time(1.0 / 60.0);
        };
        let picker = View::root(w, "hn.Picker");
        let items = picker.part("card").and_then(|c| c.part("items")).expect("hn.Picker's card.items");
        let rows: Vec<View<'_>> = (0..3).map(|_| items.child("hn.PickRowLook")).collect();
        rows[0].child("hn.PickPill");
        for _ in 0..30 {
            frame(1.0);
        }
        // Down one row: the old pill goes, the new row's pill slides from where the old one was drawn.
        rows[0].entity().each_child(|c| c.destruct());
        let pill = rows[1].child("hn.PickPill");
        frame(1.0);
        frame(1.0);
        let drawn = |p: View<'_>| p.entity().try_cloned::<&Drawn>().expect("the pill is drawn");
        let laid = |v: View<'_>| v.entity().try_cloned::<&Laid>().expect("laid out");
        assert!((drawn(pill).y - laid(rows[1]).y).abs() > 1.0, "mid-slide before the zoom");
        let card = picker.part("card").expect("card");
        let at1 = laid(card);
        // cmd+= to 1.25: the room shrinks, the card recentres, the pill lands on its row in this frame.
        let z = 1.25;
        frame(z);
        let (d, r) = (drawn(pill), laid(rows[1]));
        assert_eq!((d.x, d.y, d.w, d.h), (r.x, r.y, r.w, r.h), "the pill slid across the zoom");
        let at = laid(card);
        assert_eq!((at.w, at.h), (at1.w, at1.h), "the card keeps its size in view points (z times it in window points)");
        assert!(((at.x + at.w * 0.5) * z - win_w * 0.5).abs() < 1e-2, "centred in the window at the zoom");
        // The next frame, in the same room, moving the selection slides again.
        rows[1].entity().each_child(|c| c.destruct());
        let pill = rows[2].child("hn.PickPill");
        frame(z);
        frame(z);
        assert!((drawn(pill).y - laid(rows[2]).y).abs() > 1.0, "a selection still slides");
    }

    /// Every scene script loads, headless (no window, Metal or daemon): the tokens, the primitives, the harness scene and
    /// its widget files, the keys, the primitives gallery and the SwiftUI-vocabulary examples. A prefab named like a
    /// component it sets, or an entity named by a script keyword, fails here with flecs's error instead of at launch.
    /// `cargo test -p harness-native scenes_load`.
    #[test]
    fn scenes_load() {
        let world = scene_world().unwrap_or_else(|e| panic!("{e}"));
        for root in crate::keys::ROOTS {
            world.entity_named(root).add(flecs::OrderedChildren::ID);
        }
        let mut extra = vec![("20-keys", keys::KEYS), harness_native_ui_prims::GALLERY];
        extra.extend(harness_native_ui_prims::EXAMPLES.iter().copied());
        for (name, src) in extra {
            let e = world.script_named(&format!("scene::{name}")).build_from_code(src);
            if let Some(err) = script_error(*e) {
                panic!("scene script {name}: {err}");
            }
        }
        let mut bad = Vec::new();
        world.new_query::<&flecs::Script>().each_entity(|e, _| {
            if let Some(err) = script_error(e) {
                bad.push(format!("scene script {}: {err}", e.name()));
            }
        });
        assert!(bad.is_empty(), "{}", bad.join("\n"));
        // A scroll carries ui.Scroller's parts (from its second base) and its settings; the terminal's too.
        for prefab in ["hn.Tree", "hn.Terminal", "ui.TextArea"] {
            let v = View::root(world.world_ref(), prefab);
            for part in ["track_y", "knob_y", "track_x", "knob_x"] {
                let p = v.part(part).unwrap_or_else(|| panic!("{prefab} has no scroller part {part}"));
                assert!(p.entity().has(harness_native_ui_layout_api::ScrollerPart::id()), "{prefab}.{part} is no ScrollerPart");
            }
            let c = v.entity().try_cloned::<&harness_native_ui_layout_api::Scroller>();
            assert!(c.is_some_and(|c| c.thin > 0.0 && c.wide > c.thin && c.min > 0.0), "{prefab} has no Scroller: {c:?}");
        }
        // Prefabs the code names (a missing one would draw as a bare node, silently).
        let missing: Vec<&str> = [
            "ui.Table", "ui.TableHeadRow", "ui.TableRow", "ui.TableRecord", "ui.ResultList", "ui.Result",
            "ui.TableCell", "ui.TableCellMd", "ui.TableCellCode", "ui.TableCellNum", "ui.TableCellDim",
            "ui.TableCellLink", "ui.TableCellMark", "ui.TableHead", "ui.TableField", "ui.TableFieldMd",
            "hn.ChatTableCell", "hn.ChatTableCellMd", "hn.ChatTableCellCode", "hn.ChatTableCellNum",
            "hn.ChatTableCellDim", "hn.ChatTableCellLink", "hn.ChatTableCellMark", "hn.ChatTableHead",
            "hn.ChatTableField", "hn.ChatTableFieldMd", "hn.ChatTable", "hn.ChatResults", "hn.ChatResult",
            "hn.ChatCard", "hn.ChatCardFailed", "hn.ChatStep", "hn.ChatStepFailed", "hn.ChatGrid", "hn.ChatOutput",
            "hn.ChatOutputError", "hn.ChatCodeToggle", "ex.TableDemo",
            "hn.ChatUi", "hn.GenTone.ink",
            "hn.GenTone.error", "hn.GenColumn", "hn.GenRow", "hn.GenText", "hn.GenMarkdown", "hn.GenCode", "hn.GenTable",
            "hn.GenBadge", "hn.GenStat", "hn.GenCard", "hn.GenDivider", "hn.GenSpacer", "hn.GenUnknown",
            "hn.GenCanvas", "hn.GenShape", "hn.GenShapeBack", "hn.GenChart", "hn.GenChartPlot", "hn.GenChartTick",
            "hn.GenChartBar", "hn.GenChartGroup", "hn.GenChartStack", "hn.GenChartHit", "hn.GenChartLabel",
            "hn.GenChartLegend", "hn.GenChartKey", "hn.GenChartPie", "hn.GenChartCenter", "hn.GenTone.faint",
            "hn.GenTone.series1", "hn.GenTone.series8",
        ]
        .into_iter()
        .filter(|p| harness_native_ui_view::prefab(world.world_ref(), p).is_none())
        .collect();
        assert!(missing.is_empty(), "prefabs the code names but no scene has: {missing:?}");
    }
}
