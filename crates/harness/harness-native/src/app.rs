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
use harness_native_api::spring::Spring;
use harness_native_api::{APP, Clock, HoverAmt, HoverSteered, Lit, moving, settle};
use harness_native_flecs::prelude::*;
use harness_native_model as model;
use harness_native_render::paint::{Painter, panel_from};
use harness_native_ui_draw_api::{Paint, PaintKind};
use harness_native_ui_input_api::{Action, Activated, Hovered, Pointer, PointerState, Pressed};
use harness_native_ui_layout_api::{Size, SizeKind, Sizing, Viewport};
use harness_native_ui_look_api::{Fill, Lift, Rgba, Shape};
use harness_native_ui_node_api::{Laid, Level};
use harness_native_ui_view::{self as view, View, for_each};
use harness_vt::MouseTracking;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

mod archive;
mod browser;
mod chat;
mod floats;
mod fold;
mod keys;
pub use floats::Float;
mod panes;
mod pointer;
mod row_menu;
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
    ("scene/widgets/tree.flecs", include_str!("../scene/widgets/tree.flecs")),
    ("scene/widgets/rows.flecs", include_str!("../scene/widgets/rows.flecs")),
    ("scene/widgets/panes.flecs", include_str!("../scene/widgets/panes.flecs")),
    ("scene/widgets/archive.flecs", include_str!("../scene/widgets/archive.flecs")),
    ("scene/widgets/chat_status.flecs", include_str!("../scene/widgets/chat_status.flecs")),
    ("scene/widgets/chat.flecs", include_str!("../scene/widgets/chat.flecs")),
    ("scene/widgets/context_plate.flecs", include_str!("../scene/widgets/context_plate.flecs")),
    ("scene/widgets/context.flecs", include_str!("../scene/widgets/context.flecs")),
    ("scene/widgets/peek.flecs", include_str!("../scene/widgets/peek.flecs")),
    ("scene/widgets/genui.flecs", include_str!("../scene/widgets/genui.flecs")),
    ("scene/widgets/steps.flecs", include_str!("../scene/widgets/steps.flecs")),
    ("scene/widgets/web.flecs", include_str!("../scene/widgets/web.flecs")),
    ("scene/widgets/palette.flecs", include_str!("../scene/widgets/palette.flecs")),
    ("scene/widgets/picker.flecs", include_str!("../scene/widgets/picker.flecs")),
    ("scene/widgets/theme.flecs", include_str!("../scene/widgets/theme.flecs")),
    ("scene/widgets/stats.flecs", include_str!("../scene/widgets/stats.flecs")),
    ("scene/widgets/sound.flecs", include_str!("../scene/widgets/sound.flecs")),
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
    /// Rename the selected session in its row (cmd+R).
    Rename,
    Sidebar,
    Zoom(i32),
}

/// What cmd+N starts a chat of an agent kind on before the person ever chose for it (kind, model id, effort): Fable 5.2 at
/// medium for Claude. Any other kind starts as its agent does by default.
const FIRST_CHOICE: &[(&str, &str, &str)] = &[("claude", "claude-fable-5-2", "medium")];

/// A model and effort remembered for an agent kind (cmd+N's).
#[derive(Clone, Debug, Default)]
struct Choice {
    model: Option<String>,
    effort: Option<String>,
}

/// A session to select once the list has it: a spawn's, with the model and effort it was asked to start on (seeded as
/// its chat's choice when it is selected, so its picker never shows a placeholder), or a deep link's (neither).
#[derive(Clone, Debug)]
struct Spawned {
    sid: u64,
    model: Option<String>,
    effort: Option<String>,
}

/// The agent kinds cmd+N may start (Claude and Codex in the chat pane, Cursor in its terminal).
const CHAT_KINDS: [&str; 3] = ["claude", "codex", "cursor"];

/// What the overlay over the window is: the command palette (cmd+K) or the new-object picker (File > New…). One overlay at a
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
    /// The sidebar's foot: the focused chat's status bar, and the plan meter's slot.
    side_chat: u64,
    /// The work bar over the window's top edge (hn.WorkBar).
    work_bar: u64,
    side_plan: u64,
    /// The overlay on screen and what it is (made again when the mode changes).
    palette: Option<(u64, Mode)>,
    /// The title-bar strip's root (`hn.TitleBar`): only the back chevron, no title.
    bar: u64,
    /// The point a row's menu opens at (`hn.RowMenuAt`, placed where the press was), holding the menu while it is open
    /// (src/app/row_menu.rs).
    row_menu: u64,
}

/// The window's title-bar strip as AppKit lays it out, in points: how tall it is (0 in fullscreen, where it is hidden)
/// and where the title starts (past the traffic lights).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Chrome {
    pub strip: f32,
    pub title_x: f32,
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
    /// A session's name being renamed in its sidebar row (cmd+R, chat/rename.rs).
    rename: Option<chat::Rename>,
    /// The live inline components' keys and the link's answers last written to the world (chat/peek.rs).
    peek_live: std::cell::RefCell<chat::PeekLive>,
    /// The universal handlers' pointer, key and focus state (genui/common.rs).
    gen_prims: std::cell::RefCell<chat::GenPrims>,
    /// The runs and stages whose output the link follows (each shown by a pane), by entity: their run and stage.
    watching: HashMap<u64, (u64, Option<u32>)>,
    /// The sidebar's width (points) as last dragged, kept in native.json; None: the scene's.
    side_w: Option<f32>,
    /// Dragging the sidebar's edge, the window's rubber band past its widest, and their pulses (src/app/side_drag.rs).
    drag: side_drag::SideDrag,
    /// The chat a sideways swipe over its picker began on (chat/pick.rs): the rest of the swipe is its, wherever the
    /// pointer goes.
    swiping: Option<u64>,
    /// A run (and stage) to select once the runs show it.
    pending_run: Option<(u64, Option<u32>)>,
    /// A shot's recorded chats (HARNESS_NATIVE_SHOT_FIXTURE, [`App::shot_fixture`]): each chat's event frames, fed in
    /// place of an attach.
    shot_feed: HashMap<u64, Vec<u8>>,
    /// Chats attached for a look only (a sub-agent's chip over its parent's composer: its card says its step or last
    /// words), by session id: attached once each (chat.rs `watch_chat`); and those asked for and not attached yet.
    watched: std::cell::RefCell<(std::collections::HashSet<u64>, Vec<u64>)>,
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
    /// The session a spawn made (with the model and effort it was asked to start on), to select once the list shows it.
    spawned: Arc<Mutex<Option<Spawned>>>,
    /// Why cmd+N could not start on the remembered model, shown on the new chat's models menu once it is selected.
    refuse_spawned: Option<String>,
    /// What cmd+N starts on: the agent kind last used, and by kind the model and effort last chosen (native.json
    /// `chat_kind` and `chat_choice` when the app persists; an offscreen shot keeps them here alone).
    last_kind: Option<String>,
    choices: std::collections::BTreeMap<String, Choice>,
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
    /// An offscreen shot's `HARNESS_NATIVE_SHOT_WORKING` (src/shot.rs): every session row and every run row shows at
    /// work; unset, rows show what is really at work.
    pub shot_working: bool,
    /// An offscreen shot's `HARNESS_NATIVE_SHOT_PLAN` (src/shot.rs, chat/plan.rs): every chat's status bar shows this plan;
    /// empty, each shows what its agent kind last reported.
    pub shot_plan: Vec<harness_proto::event::PlanWindow>,
    pub focused: bool,
    pub title: String,
    /// The title-bar strip the window reports, and the (strip, zoom) last written into the view.
    chrome: Chrome,
    chrome_laid: Option<(Chrome, f32, f32, f32)>,
    /// The dark appearance (the window's effective NSAppearance; the scene's fills pick their dark or light half).
    pub dark: bool,
    /// The window's screen's EDR headroom (window.rs reads it every frame and when the screen changes; the shot sets it
    /// from HARNESS_NATIVE_SHOT_HEADROOM): carried into the frame (`Frame::headroom`) for HDR pictures and the mark's fire.
    pub headroom: harness_native_render::hdr::Headroom,
    /// The sidebar tree's scroll (src/app/sidebar.rs).
    tree: sidebar::TreeScroll,
    /// The Archive list's scroll (app/archive.rs).
    arch_scroll: archive::ListScroll,
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
    /// The screen's room for windowed overlays, window points from the view's corner, as the host last said (src/app/floats.rs).
    screen: Option<[f32; 4]>,
    /// The windowed overlays the last frame drew, until the host takes them (src/app/floats.rs).
    floats: Vec<floats::Float>,
    /// The embedded browser: its link to the host, the pane, the views' input (app/browser.rs).
    browser: browser::Browser,
    /// A sidebar row's menu while it is open (a right press or a control-click on the row; src/app/row_menu.rs).
    row_menu: Option<row_menu::RowMenu>,
}

/// The fill of the theme's role `name` (`success`: `hn.Theme.success`).
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
    browser::register(&world);
    stats::register(&world);
    crate::keys::register(&world);
    view::register(&world);
    world.component::<chat::GenFeel>().writable();
    world.component::<crate::audio::Mood>().writable();
    world.component::<crate::audio::Instrument>().writable();
    world.component::<crate::audio::Part>().writable();
    world.component::<crate::audio::RainOn>().writable();
    world.component::<chat::GenOn>();
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
    if let Some(err) = bad {
        return Err(err);
    }
    // The inline components' labels and marks (`<PR repo=.. n={..}/>` shows `#n`), for the typesetter, which reads no
    // world.
    harness_native_render::inline::install(world.world_ref());
    Ok(world)
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
            done_ms: (crate::look::width(w, "archive_done_after").max(0.0) as f64 * 1000.0) as u64,
        });
        // How long a finished child keeps its row (and its done mark) under a folded node (seconds).
        world.set(model::Linger { ms: (crate::look::width(w, "tree_linger_for").max(0.0) * 1000.0) as u64 });
        harness_native_ui_layout::install(w);
        harness_native_ui_input::install(w);
        harness_native_ui_motion::install(w);
        crate::look::install(w);
        harness_native_ui_draw::install(w);
        // den-native-hover: each hoverable's hover spring toward whether the pointer is on it.
        let app_id = *app.id();
        world.system_named::<&mut HoverAmt>("Hover").each_iter(move |it, row, h| {
            let (e, w) = (it.entity(row), it.world());
            // On it, or on a part of it that takes the pointer itself (a row's disclosure chevron, a control deep in a
            // chat's drawn view, whose hover shows the view's code button).
            // A menu's item lights where its owner keeps the highlight (`Lit`), which the pointer and the keys move.
            let on = if e.has(HoverSteered::id()) {
                e.has(Lit::id())
            } else {
                w.entity_from_id(app_id).target(Hovered::id(), 0).is_some_and(|t| {
                    std::iter::successors(Some(t), |n| n.parent()).take(HOVER_DEPTH).any(|n| n.id() == e.id())
                })
            };
            // Hover lands at once, never eased: a wash fading out on one row while it fades in on the next shows two
            // rows lit, and in a menu two highlights with one line about only one of them.
            let to = f32::from(u8::from(on));
            if h.spring.x != to || h.spring.v != 0.0 {
                h.spring = Spring::at(to);
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
        let browser_wake = wake.clone();
        let link = Link::start(tx, wake);
        // The view's fixed frame: the window row, the sidebar (device row over the tree), the main card.
        let root = View::root(w, "hn.Window");
        // The fold's clip holds the sidebar and, at its right edge, the hairline between it and the main card: folded to
        // nothing, the clip leaves neither, and the main card meets the window's edge.
        let side_clip = root.child("hn.SideClip");
        let side = side_clip.child("hn.Sidebar");
        let device = side.child("hn.DeviceRow");
        // Its count shows on its hover (scene/widgets/parts.flecs).
        device.set(HoverAmt::default());
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
        let status = foot.child("hn.SideStatus");
        let archive_row = status
            .child("hn.ArchiveRow")
            .props("hn.ArchiveRow", &[("selected", Prop::Bool(false))]);
        archive_row.set(HoverAmt::default());
        let side_chat = status.child("hn.SideChat");
        let side_plan = status.child("hn.SidePlan");
        side_clip.child("hn.Seam");
        // The panes: sync_panes makes the first (and swaps each one's prefab by what it shows).
        let main = root.child("hn.Main");
        // The browser pane on the right (scene/widgets/web.flecs): its clip folds, the pane inside keeps its width.
        let web_clip = root.child("hn.WebClip");
        let work_bar = root.child("hn.WorkBar").id();
        let web_pane = web_clip.child("hn.WebPane").id();
        let web_clip = web_clip.id();
        let bar = View::root(w, "hn.TitleBar");
        // Out of the window row's flow (Placed): layout keeps the place row_menu.rs gives it.
        let row_menu = root.child("hn.RowMenuAt");
        row_menu.set(Laid::default());
        let ids = Ids {
            tree: tree.id(),
            device: device.id(),
            link_note: link_note.id(),
            main: main.id(),
            archive_row: archive_row.id(),
            side_chat: side_chat.id(),
            work_bar,
            side_plan: side_plan.id(),
            palette: None,
            bar: bar.id(),
            row_menu: row_menu.id(),
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
            rename: None,
            peek_live: std::cell::RefCell::default(),
            gen_prims: std::cell::RefCell::default(),
            watching: HashMap::new(),
            pending_run: None,
            shot_feed: HashMap::new(),
            watched: std::cell::RefCell::default(),
            side_w,
            drag: side_drag::SideDrag::default(),
            swiping: None,
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
            refuse_spawned: None,
            last_kind: None,
            choices: std::collections::BTreeMap::new(),
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
            shot_working: false,
            shot_plan: Vec::new(), // alloc: empty but in a shot
            focused: true,
            title: "harness".into(),
            chrome: Chrome::default(),
            chrome_laid: None,
            dark: true,
            headroom: harness_native_render::hdr::Headroom::SDR,
            tree: sidebar::TreeScroll::default(),
            arch_scroll: archive::ListScroll::default(),
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
            screen: None,
            floats: Vec::new(), // alloc: empty until an overlay's window shows
            browser: browser::Browser::new(browser_wake, web_clip, web_pane),
            row_menu: None,
        };
        app.apply_ghostty();
        app.load_keys()?;
        // The browser pane's width as it was last dragged (native.json `web_width`).
        app.web_restore_width();
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

    /// The system's Reduce Motion (src/window.rs reads it from AppKit): kept as the world's `ReduceMotion`, which the
    /// sidebar's springs and matched slides read to land at once.
    pub fn set_reduce_motion(&mut self, on: bool) {
        if self.world.try_get::<&harness_native_api::ReduceMotion>(|r| r.on) != Some(on) {
            self.world.set(harness_native_api::ReduceMotion { on });
            self.poke_why("reduce motion");
        }
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

    /// A shot's recorded world (`HARNESS_NATIVE_SHOT_FIXTURE=<json>`, src/shot.rs): `{list, runs, chats: {"<id>": [event]}}`,
    /// the daemon's `list` and `runs` answers and each chat's events as its log holds them (harness-proto's JSON form),
    /// posted to the inbox as the link would; a chat's events are fed in place of its attach. A time field (`ts`,
    /// `started_ms`, `created_ms`, `ended_ms`, `start_ms`, `end_ms`) given as a negative number is that many ms before now,
    /// so a shot shows a turn that has run for minutes.
    pub fn shot_fixture(&mut self, path: &str) -> Result<(), String> {
        fn rel(v: &mut serde_json::Value, now: i64) {
            match v {
                serde_json::Value::Object(m) => {
                    for (k, x) in m.iter_mut() {
                        match x.as_i64() {
                            Some(n) if n < 0 && ["ts", "started_ms", "created_ms", "ended_ms", "start_ms", "end_ms"].contains(&k.as_str()) => *x = (now + n).into(),
                            _ => rel(x, now),
                        }
                    }
                }
                serde_json::Value::Array(a) => a.iter_mut().for_each(|x| rel(x, now)),
                _ => {}
            }
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("HARNESS_NATIVE_SHOT_FIXTURE {path}: {e}"))?;
        let mut f: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("HARNESS_NATIVE_SHOT_FIXTURE {path}: {e}"))?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        rel(&mut f, now);
        // No daemon comes up to restore a selection after: the shot's own (HARNESS_NATIVE_SHOT_SELECT) is the one.
        self.restore = None;
        let mail = self.link.mailbox();
        mail.send(model::Msg::List(f["list"].as_array().cloned().unwrap_or_default()));
        mail.send(model::Msg::Runs(f["runs"].as_array().cloned().unwrap_or_default()));
        for (id, evs) in f["chats"].as_object().into_iter().flatten() {
            let id: u64 = id.parse().map_err(|_| format!("HARNESS_NATIVE_SHOT_FIXTURE {path}: chat id {id} is not a number"))?;
            let mut bytes = Vec::new();
            for e in evs.as_array().into_iter().flatten() {
                let ev = harness_proto::event::Event::from_json(e).ok_or_else(|| format!("HARNESS_NATIVE_SHOT_FIXTURE {path}: not an event: {e}"))?;
                bytes.extend(ev.frame());
            }
            self.shot_feed.insert(id, bytes);
        }
        Ok(())
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
            "running" => tone(w, "success"),
            "lost" => tone(w, "warning"),
            "killed" => tone(w, "error"),
            _ if code.is_some_and(|c| c != 0) => tone(w, "error"),
            _ => tone(w, "ink_faint"),
        }
    }

    fn select(&mut self, e: Option<u64>) {
        if e != self.selected() {
            if e.is_some() {
                crate::audio::cue("select");
            }
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
        // A rename whose row is no longer the selection (or under the palette) is saved before the rows are filled.
        self.rename_settle();
        // An open row menu's rows for its object as it is now (one whose object went closes); its row is marked below.
        self.sync_row_menu();
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
        let mut refused = None;
        let pending = self.spawned.lock().unwrap().clone().filter(|_| self.restore.is_none());
        if let Some(p) = pending {
            if let Some(e) = self.entity_of(p.sid) {
                // Its chat shows what it was started on from the frame it is selected in, not what its agent says once
                // it is up (chat.rs `chat_started_on`).
                self.chat_started_on(p.sid, p.model.as_deref(), p.effort.as_deref());
                model::select(w, Some(e));
                *self.spawned.lock().unwrap() = None;
                if let Some(why) = self.refuse_spawned.take() {
                    refused = Some((p.sid, why));
                }
            }
        }
        if let Some((sid, why)) = refused {
            self.chat_pick_refuse(sid, why);
        }
        let w = self.world.world_ref();
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
        // The device row.
        let dev = w.lookup(model::LOCAL);
        let device = View::of(w.entity_from_id(self.ids.device));
        let host = dev
            .try_cloned::<&model::Device>()
            .map(|d| d.host.as_str().to_owned())
            .unwrap_or_default();
        let link = dev.target(model::Link::id(), 0).map_or(0, |t| *t.id());
        let link_word = if link == *w.component_id::<model::Up>() {
            format!("{}", rows.len())
        } else if link == *w.component_id::<model::Down>() {
            "offline".to_owned()
        } else if link == *w.component_id::<model::Refused>() {
            "refused".to_owned()
        } else {
            "connecting".to_owned()
        };
        if let Some(n) = device.part("name") {
            n.text(&host);
        }
        if let Some(n) = device.part("count") {
            // How many objects, on the header's hover; the link's trouble (offline, refused, connecting) always.
            crate::look::hover_show(n.text(&link_word).entity(), 0, link != *w.component_id::<model::Up>());
        }
        // The refusal in words (which daemon, `harness stop`), in place of a spinner that never ends.
        let why = dev.try_cloned::<&model::LinkError>().map(|e| e.text.as_str().to_owned()).unwrap_or_default();
        let note = View::of(w.entity_from_id(self.ids.link_note)).text(&why);
        let h = if why.is_empty() { Sizing::fixed(0.0) } else { Sizing::FIT };
        if note.entity().try_cloned::<&Size>().is_none_or(|s| s.h != h) {
            note.set(Size { w: Sizing::GROW, h });
        }
        let tree = View::of(w.entity_from_id(self.ids.tree));
        let hint: HashMap<u64, usize> =
            rows.iter().enumerate().map(|(i, &(e, _))| (e, i)).collect();
        let info: Vec<(String, &str, model::Row)> = model::rows(w)
            .into_iter()
            .map(|r| {
                let prefab = if self.run_of(r.e).is_some() { "hn.RunRow" } else { "hn.ObjRow" };
                (format!("o{}", r.e), prefab, r)
            })
            .collect();
        // Rows that left the list stay while they fold away.
        let info = self.fold.merge(info, w);
        let menu_for = self.row_menu_for();
        let me = &*self;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        // Each row is a template instance (scene/widgets/rows.flecs): the row's facts as props, written only when one
        // changed (View::props), and flecs re-runs only the parts that read it. A session's row leads with its kind's mark
        // (`kind`); no status dots: how a row stands is in its title's ink (`look`), its mark's ink while it works
        // and a few muted words trailing it (`meta`). Folded rows count what works under them (`under`).
        view::for_each_of(tree, info, |row, r, new| {
            if new {
                row.set(HoverAmt::default());
            }
            let model::Row { e, depth } = r;
            let o = w.entity_from_id(e);
            let status = me.status_of(e);
            let code = o.try_cloned::<&model::ExitCode>().map(|c| c.code).filter(|&c| c != 0);
            // Ended by an interrupt or a terminate (128 + SIGINT, 128 + SIGTERM): stopped on purpose, not a failure.
            let stopped = matches!(code, Some(130 | 143));
            let failed = status == "killed" || (code.is_some() && !stopped);
            let selected = sel == Some(e);
            // A sub-agent up but idle sleeps: its title faded like what ended (no mark: only a failure earns one), and it
            // sorts after the ones at work (the model's rows).
            let sleeps = me.run_of(e).is_none() && depth > 0 && status == "running" && !o.has(model::Busy::id());
            let look = if selected {
                "on"
            } else if status == "running" && !sleeps {
                "rest"
            } else {
                "done"
            };
            let common = [
                ("obj", Prop::Uint(e)),
                ("selected", Prop::Bool(selected)),
                ("depth", Prop::Int(depth as i64)),
                ("kids", Prop::Bool(fold::has_kids(o))),
                ("open", Prop::Bool(!model::folded(o))),
                ("focus", Prop::Bool(selected && me.tree_focus && me.focused)),
                ("look", Prop::Str(look)),
                // Its menu is open (row_menu.rs): ringed, the selection left where it is.
                ("menu", Prop::Bool(menu_for == Some(e))),
            ];
            if me.run_of(e).is_some() {
                // A run: its command, cut to what says what it does; how long it took (and how it failed) on hover, a failure's always.
                let name = o.try_cloned::<&model::Name>().map(|n| short_cmd(n.s.as_str())).unwrap_or_default();
                let meta = me.run_meta(e);
                let keep = failed || matches!(status, "lost" | "pending" | "skipped");
                row.props(
                    "hn.RunRow",
                    &[
                        &common[..],
                        &[
                            ("name", Prop::Str(&name)),
                            ("busy", Prop::Bool(status == "running" || me.shot_working)),
                            ("alarm", Prop::Bool(failed)),
                            ("meta", Prop::Str(&meta)),
                            ("keep", Prop::Bool(keep)),
                            ("mark", Prop::Str(done_mark(status, code.filter(|_| !stopped), depth.max(1)))),
                        ],
                    ]
                    .concat(),
                );
            } else {
                // A session: its title; trailing how it ended when that went wrong, else how long ago it started or ended, and its shortcut in
                // that place on hover.
                let name = o
                    .try_cloned::<&model::Name>()
                    .map(|n| n.s.as_str().split_whitespace().collect::<Vec<_>>().join(" "))
                    .unwrap_or_default();
                let hint = hint
                    .get(&e)
                    .filter(|&&i| i < 9)
                    .map(|i| me.hint(&format!("Select{}", i + 1)))
                    .unwrap_or_default();
                // At work: its program wrote output lately (the daemon's busy), or its chat's agent is mid-turn.
                let busy = status == "running"
                    && (o.has(model::Busy::id())
                        || o.try_cloned::<&model::Object>()
                            .is_some_and(|ob| me.chats.borrow().get(&ob.id).is_some_and(|t| t.busy)));
                // HARNESS_NATIVE_SHOT_WORKING: a shot shows every session at work.
                let busy = busy || me.shot_working;
                // What it is doing while it works (the command or file trailing it, its words first in the tooltip), or
                // how it went wrong; else nothing, the title has the row (its name mostly is its folder already). Where
                // it works and when it started or ended are the row's tooltip.
                let doing = if busy { o.try_cloned::<&model::Object>().and_then(|ob| me.chats.borrow().get(&ob.id).and_then(doing_of)) } else { None };
                let when = ago(now.saturating_sub(model::when(o)));
                let folder = o.try_cloned::<&model::Cwd>().map(|c| folder_of(c.s.as_str())).unwrap_or_default();
                // What it is (its icon on the row) and its number, which its name never carries: the tooltip's last words.
                let kind = model::kind_name(o);
                let number = o.try_cloned::<&model::Object>().map(|ob| format!(" \u{b7} {} s{}", capitalised(kind), ob.id)).unwrap_or_default();
                // Folded (as every row starts), what is at work under it is counted here rather than opening it.
                let under = match model::hidden_active(w, e) {
                    0 => String::new(),
                    n => n.to_string(),
                };
                let (meta, why) = match (status, code) {
                    ("killed", _) => ("killed".to_owned(), format!("{folder} \u{b7} killed {when} ago")),
                    (_, Some(c)) if stopped => ("stopped".to_owned(), format!("{folder} \u{b7} stopped (exit {c}) {when} ago")),
                    (_, Some(c)) => (format!("exit {c}"), format!("{folder} \u{b7} exited with code {c} {when} ago")),
                    ("lost", _) => ("lost".to_owned(), format!("{folder} \u{b7} lost (the daemon restarted under it)")),
                    ("running", _) => match &doing {
                        Some(d) => (d.detail.clone(), format!("{} \u{b7} {folder} \u{b7} started {when} ago", d.tip())),
                        None => (String::new(), format!("{folder} \u{b7} started {when} ago")),
                    },
                    _ => (String::new(), format!("{folder} \u{b7} ended {when} ago")),
                };
                let why = why + &number;
                let mark = done_mark(status, code.filter(|_| !stopped), depth);
                // Its name being renamed (chat/rename.rs): the field's words in its place, alone
                // on the row (no trailing words or shortcut).
                let edit = me.rename_shown(e);
                let (name, meta, hint) = match &edit {
                    Some((shown, _)) => (shown.clone(), String::new(), String::new()),
                    None => (name, meta, hint),
                };
                row.props(
                    "hn.ObjRow",
                    &[
                        &common[..],
                        &[
                            ("name", Prop::Str(&name)),
                            ("under", Prop::Str(&under)),
                            ("meta", Prop::Str(&meta)),
                            ("why", Prop::Str(&why)),
                            ("hint", Prop::Str(&hint)),
                            ("edit", Prop::Bool(edit.is_some())),
                            ("mark", Prop::Str(mark)),
                        ],
                    ]
                    .concat(),
                );
                // Before its title, a mark only when there is something to say, so a quiet tree is plain words: a dot in
                // the strong ink while its agent works, the same dot in the error ink when it failed (the row's one
                // colour: its trailing words stay faint), nothing else. The dot is the chat's own running glyph
                // (scene/widgets/chat_status.flecs), still here. Which agent it is ends the tooltip. A shell keeps its
                // terminal glyph, since it opens a terminal and not a chat.
                if let Some(i) = row.part("kind_icon") {
                    let kind = model::kind_name(o);
                    let agent = matches!(kind, "claude" | "codex" | "cursor");
                    if agent {
                        let dot = view::prefab(w, "hn.chat_status.running")
                            .and_then(|p| p.try_cloned::<&harness_native_ui_text_api::Icon>())
                            .unwrap_or_else(|| panic!("the scene's hn.chat_status.running has no Icon (scene/widgets/chat_status.flecs)"));
                        crate::look::show_glyph(i.entity(), if busy || failed { dot.glyph.as_str() } else { "" });
                    } else {
                        crate::look::kind_mark(w, i.entity(), kind);
                    }
                    let f = tone(w, if failed && agent { "error" } else if busy { "ink_strong" } else { "ink_faint" });
                    if i.entity().try_cloned::<&Fill>() != Some(f) {
                        i.entity().set(f);
                    }
                }
                if let Some((_, units)) = edit {
                    me.rename_caret(row, units);
                }
            }
        });
        self.fold.animate(
            self.world.entity_from_id(self.ids.tree),
            crate::look::fold_in(self.world.world_ref()),
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
            crate::audio::cue("archive");
            let o = self.world.entity_from_id(e);
            o.add(model::Closed::id());
            o.remove(model::Pinned::id());
            self.save_marks();
        }
    }

    /// Take object `e` back out of the Archive into the tree (its row's menu, Unarchive): no longer closed, and kept there
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

    /// Keep object `e` in the tree whatever the rule says (its row's menu, Keep in Tree), or stop.
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

    /// Close object `e` from its row's menu (no asking), as cmd+W closes the selected one: its session (or a run that
    /// runs) is killed and it goes to the Archive. The selected one goes through cmd+W itself, so what takes its place is
    /// selected; another leaves the selection where it is.
    pub fn close_object(&mut self, e: u64) {
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

    /// Kill object `e` from its row's menu (no asking) while it runs: its session's process, or the run, is stopped and
    /// its row stays where it is (the rule archives it as it would any that ended). Close is the one that also puts it
    /// away.
    pub fn kill_object(&mut self, e: u64) {
        if !self.world.entity_from_id(e).is_alive() || self.status_of(e) != "running" {
            return;
        }
        if let Some((n, None)) = self.run_of(e) {
            self.link.kill_run(n);
        } else if let Some(sid) = self.session_of(e) {
            self.link.kill(sid);
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
        // A session (not a run, not the Archive) can be renamed in its row.
        let renamable = self
            .selected()
            .filter(|&e| self.run_of(e).is_none())
            .map(|e| w.entity_from_id(e))
            .filter(|o| o.has(model::Object::id()));
        if let Some(o) = renamable {
            let what = if o.has(model::ChatMode::id()) { "Rename chat" } else { "Rename session" };
            out.push(("rename".into(), PalGlyph::Char(crate::look::RENAME_ICON), what.into(), self.hint("Rename"), Do::Rename));
        }
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
        let (clear, chosen) = (tone(w, "clear"), tone(w, "selection"));
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
                            set_if(p, tone(w, "ink_muted"));
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

    /// Open the new-object picker (File > New…), the kind picked last highlighted (native.json `new_kind`), its pill landing
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

    /// An object of `kind` is being made by an act.Spawn or cmd+N: from the picker, the choice is kept for its next
    /// opening; a chat agent's kind, from anywhere, is what cmd+N starts next ([`Self::chat_kind`]) (neither from an
    /// offscreen shot); any overlay open closes.
    fn spawning(&mut self, kind: &str) {
        if self.picking() && self.persist {
            crate::prefs::set("new_kind", serde_json::json!(kind));
        }
        if CHAT_KINDS.contains(&kind) {
            self.last_kind = Some(kind.to_owned());
            if self.persist {
                crate::prefs::set("chat_kind", serde_json::json!(kind));
            }
        }
        self.palette = None;
    }

    /// The person chose `model` at `effort` for an agent of `kind` (in any chat, or by starting one): what cmd+N starts
    /// on next. An effort without a model keeps the model remembered.
    pub(in crate::app) fn remember_choice(&mut self, kind: &str, model: Option<&str>, effort: Option<&str>) {
        let mut c = self.choice_of(kind);
        if let Some(m) = model {
            c = Choice { model: Some(m.to_owned()), effort: None };
        }
        c.effort = effort.map(str::to_owned).or(c.effort);
        self.last_kind = Some(kind.to_owned());
        if self.persist {
            crate::prefs::set("chat_kind", serde_json::json!(kind));
            let mut all = crate::prefs::get("chat_choice").and_then(|v| v.as_object().cloned()).unwrap_or_default();
            all.insert(kind.to_owned(), serde_json::json!({"model": c.model, "effort": c.effort}));
            crate::prefs::set("chat_choice", serde_json::Value::Object(all));
        }
        self.choices.insert(kind.to_owned(), c);
    }

    /// The model and effort last chosen for `kind` (here, else native.json `chat_choice`); for a kind never chosen for,
    /// its [`FIRST_CHOICE`] when it has one.
    fn choice_of(&self, kind: &str) -> Choice {
        self.choices.get(kind).cloned().unwrap_or_else(|| {
            let v = crate::prefs::get("chat_choice").and_then(|v| v.get(kind).cloned());
            let s = |k: &str| v.as_ref().and_then(|v| v.get(k)).and_then(|x| x.as_str()).map(str::to_owned);
            match (v.is_some(), FIRST_CHOICE.iter().find(|(k, ..)| *k == kind)) {
                (false, Some(&(_, model, effort))) => Choice { model: Some(model.to_owned()), effort: Some(effort.to_owned()) },
                _ => Choice { model: s("model"), effort: s("effort") },
            }
        })
    }

    /// The sidebar's foot (hn.SideStatus): how much of the plan is used (the focused chat's agent's, else the agent a
    /// new chat is of), and, with no chat in front, the chat's status bar emptied (a chat in front fills it, chat.rs).
    fn sync_side_status(&mut self) {
        let w = self.world.world_ref();
        let kind = match self.focused_chat() {
            Some((_, e, _)) => model::kind_name(w.entity_from_id(e)).to_owned(),
            None => {
                let bar = View::of(w.entity_from_id(self.ids.side_chat));
                for slot in ["pick", "meter", "menu", "pop"] {
                    if let Some(s) = bar.part(slot) {
                        view::for_each_of(s, std::iter::empty::<(&str, &str, ())>(), |_, (), _| {});
                    }
                }
                self.chat_kind()
            }
        };
        let windows = self.chat_plan(&kind);
        chat::plan::fill_plan(View::of(w.entity_from_id(self.ids.side_plan)), &windows, &kind, tone(w, "ink_muted"), model::now_ms() / 1000, false);
    }

    /// Start the app's sound from the scene's `hn.sound` (scene/widgets/sound.flecs, src/audio.rs): the window's, never
    /// an offscreen shot's. native.json `sound: false` keeps it silent, `music: false` leaves the music out, `ambient: false` the rain.
    ///
    /// # Errors
    /// The scene's sound is malformed (named), or the system refused the output.
    pub fn start_sound(&self) -> Result<(), String> {
        let on = |k: &str| crate::prefs::get(k).and_then(|v| v.as_bool()).unwrap_or(true);
        if !on("sound") {
            return Ok(());
        }
        let (level, music, ambient, rain, cues, moods) = scene_sound(self.world.world_ref())?;
        crate::audio::start(level, if on("music") { music } else { 0.0 }, if on("ambient") { ambient } else { 0.0 }, &rain, cues, moods)
    }

    /// How long a turn takes, by the turns already finished in every chat: their middle length (ms), once there are at
    /// least three. An estimate and no more: the work bar says "about".
    fn turn_estimate(&self) -> Option<u64> {
        let chats = self.chats.borrow();
        let mut took: Vec<u64> = chats // alloc: the finished turns' lengths, a frame while one runs
            .values()
            .flat_map(|t| t.items.iter())
            .filter_map(|i| match i {
                model::chat::Item::TurnEnd { duration_ms: Some(ms), .. } if *ms > 0 => Some(*ms),
                _ => None,
            })
            .collect();
        if took.len() < 3 {
            return None;
        }
        took.sort_unstable();
        Some(took[took.len() / 2])
    }

    /// The work bar (hn.WorkBar): while the chat in front works, a band along the window's top edge, placed by the
    /// track's padding. With an estimate ([`Self::turn_estimate`]) it fills from the left as the turn nears it and
    /// `eta` says about how long is left; with none, or past it, it sweeps. At rest it has no width. Drawn every frame
    /// the display gives while it moves; under Reduce Motion a sweep is the whole hairline, still.
    fn step_work_bar(&mut self) {
        use harness_native_ui_layout_api::Padding;
        let w = self.world.world_ref();
        let bar = View::of(w.entity_from_id(self.ids.work_bar));
        let Some(track) = bar.part("track") else { return };
        let whole = track.entity().try_cloned::<&Laid>().map_or(0.0, |l| l.w);
        let since = self.focused_chat().and_then(|(_, _, sid)| self.chats.borrow().get(&sid).filter(|t| t.busy).map(|t| t.turn_since));
        let busy = since.is_some() && whole >= 1.0;
        let still = harness_native_api::reduced(w);
        // How far through the estimate the turn is, while it has not outrun it; and how long is left.
        let run = since.flatten().map(|from| model::now_ms().saturating_sub(from));
        let toward = run.zip(self.turn_estimate()).filter(|(run, est)| run < est);
        let mut left = String::new();
        let pad = if !busy {
            Padding { l: whole, r: 0.0, t: 0.0, b: 0.0 }
        } else if let Some((run, est)) = toward {
            let mins = (est - run).div_ceil(60_000);
            left = if est - run < 45_000 { "under a minute".to_owned() } else { format!("about {mins} min") };
            Padding { l: 0.0, r: (whole * (1.0 - run as f32 / est as f32)).max(0.0), t: 0.0, b: 0.0 }
        } else if still {
            Padding::default()
        } else {
            let look = crate::look::role(w, "work_bar").try_cloned::<&Size>().unwrap_or_default();
            let (share, period) = (look.w.value.clamp(0.02, 1.0), look.h.value.max(0.1));
            let time = w.try_get::<&Clock>(|c| c.time).unwrap_or(0.0);
            let phase = (time / period).rem_euclid(1.0);
            let eased = phase * phase * (3.0 - 2.0 * phase);
            let len = whole * share;
            let x0 = -len + (whole + len) * eased;
            let (a, z) = (x0.max(0.0), (x0 + len).min(whole));
            Padding { l: a, r: (whole - z).max(0.0), t: 0.0, b: 0.0 }
        };
        if track.entity().try_cloned::<&Padding>() != Some(pad) {
            track.set(pad);
        }
        if let Some(eta) = bar.part("foot").and_then(|f| f.part("eta")) {
            eta.text(&left);
        }
        // It moves: every frame the display gives (a sweep at a calm rate steps visibly).
        if busy && !(still && toward.is_none()) {
            self.poke_why("work bar");
        }
    }

    /// The agent kind cmd+N starts a chat of: the one last used (here, else native.json `chat_kind`), else the picker's
    /// last pick when that is an agent (`new_kind`), else Claude.
    fn chat_kind(&self) -> String {
        let pref = |k: &str| crate::prefs::get(k).and_then(|v| v.as_str().map(str::to_owned));
        self.last_kind
            .clone()
            .into_iter()
            .chain(pref("chat_kind"))
            .chain(pref("new_kind"))
            .find(|k| CHAT_KINDS.contains(&k.as_str()))
            .unwrap_or_else(|| "claude".to_owned())
    }

    /// cmd+N: a chat of the last agent kind on its last model and effort ([`Self::remember_choice`]). A remembered model
    /// (or effort) its kind no longer lists is not started on: the chat starts as its agent does by default, its models'
    /// menu open with an error naming what is gone.
    /// Whether the chat `sid` has had a message sent in it: a chat with none is still the new chat Cmd-N would make.
    pub(in crate::app) fn sent(&self, sid: u64) -> bool {
        self.chats.borrow().get(&sid).is_some_and(|t| t.items.iter().any(|i| matches!(i, model::chat::Item::User { .. })))
    }

    pub(in crate::app) fn new_chat(&mut self) {
        // Cmd-N on a chat nothing has been sent in yet does nothing: that chat already is the new one.
        if let Some(sid) = self.selected().and_then(|e| self.session_of(e))
            && self.chats.borrow().contains_key(&sid)
            && !self.sent(sid)
        {
            self.poke();
            return;
        }
        crate::audio::cue("fresh");
        let kind = self.chat_kind();
        let c = self.choice_of(&kind);
        let kept = self.link.agent_models();
        let listed = kept.get(&kind);
        let entry = c.model.as_deref().and_then(|m| listed.map(|l| l.iter().find(|x| x.is(m))));
        let title = self.agent_titles().remove(&kind).unwrap_or_else(|| kind.clone());
        let why = match (&c.model, entry, &c.effort) {
            (Some(m), Some(None), _) => Some(format!("{m}, the model last used, is no longer offered by {title}. Pick one.")),
            (Some(m), Some(Some(x)), Some(e)) if !x.efforts.contains(e) => {
                Some(format!("{m} no longer takes effort {e}, the effort last used. Pick a model."))
            }
            _ => None,
        };
        self.spawning(&kind);
        match why {
            Some(why) => {
                self.refuse_spawned = Some(why);
                self.spawn_on(&kind, None, None, self.selected_cwd());
            }
            None => self.spawn_on(&kind, c.model, c.effort, self.selected_cwd()),
        }
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
        self.browser_frame_start(dt);
        self.step_fold(dt);
        self.step_tree_scroll(dt);
        self.step_work_bar();
        // The sound follows the chat in front (src/audio.rs; silent unless the window started it).
        {
            let front = self.focused_chat().is_some_and(|(_, _, sid)| self.chats.borrow().get(&sid).is_some_and(|t| t.busy));
            let agents = self.chats.borrow().values().filter(|t| t.busy).count();
            // Whether the chat in front's turn ended on an error: its ending rings low, not home.
            let failed = self.focused_chat().is_some_and(|(_, _, sid)| {
                self.chats.borrow().get(&sid).is_some_and(|t| t.items.iter().rev().find(|i| !matches!(i, model::chat::Item::TurnEnd { .. })).is_some_and(|i| matches!(i, model::chat::Item::Error { .. })))
            });
            crate::audio::scene(agents, front, failed, chat::plan::night(model::now_ms() / 1000));
        }
        self.sync_side_status();
        self.step_archive_scroll();
        self.step_chat_scroll();
        let w = self.world.world_ref();
        if w.try_get::<&Viewport>(|v| (v.w, v.h)) != Some(view) {
            self.world.set(Viewport { w: view.0, h: view.1 });
        }
        self.sync_screen(z);
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
        // Words laid out at a new width (a fold moving a transcript's column) hold each chat's view still in this frame,
        // not the next (chat.rs anchor_chats): laid out once more where a scroll moved.
        if self.anchor_chats() {
            self.progress(0.0);
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
                textfx: [self.headroom.frame(), 0.0],
                tune: [0.82, 1.12, 1.0, 0.0],
                slug: [0.0, 1.0, 1.0, 0.0],
                place: [0.0; 4],
            },
            // Clear: the window's vibrancy shows through everywhere the scene paints nothing.
            clear: [0.0; 4],
            ..Draw::default()
        };
        self.emit(&mut d, scale, upload);
        // The cells' Shaders and Videos: what Media should render and play, now that the paint says which are visible.
        self.step_media(&mut d, dt, scale);
        // The browser views on screen: kept, watched, and the pane's sized (app/browser.rs).
        self.browser_frame_end(&mut d, scale);
        if self.paint.get_mut().end_frame() {
            self.poke();
        }
        // Words streaming in fade in glyph by glyph (GlyphsArrive): draw until the last has.
        if self.paint.get_mut().arriving {
            self.poke_why("glyphs arriving");
        }
        // A caret that jumped sweeps into its place (harness-native-render caret.rs): draw until it has.
        if self.paint.get_mut().gliding {
            self.poke_why("caret glide");
        }
        // A Kip in view that hops, waves or crossfades wants every frame (kips.rs `Wake`); one that only breathes is
        // drawn at the calm rate ([`Self::breathing`]), one at rest wakes the window for his next blink ([`Self::kip_nap`]).
        if self.paint.get_mut().kip_wake == harness_native_render::kips::Wake::Lively {
            self.poke_why("kip");
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
        // The hairline at the sidebar's right edge (hn.Seam) folds with it, inside its clip.
        let Some(seam) = view::prefab(w, "hn.Seam").and_then(|p| p.try_cloned::<&Size>()) else { return };
        let seam = seam.w.value;
        let x = self.side_x.x.clamp(0.0, 1.0);
        let open = self.side_w.unwrap_or(full.w.value);
        let shown = open + seam;
        let width = shown * x;
        // What the card's LaysAtRest panes (a terminal's grid) lay out against while the fold moves: the open width, where
        // the card is narrowest, so a terminal resizes once per cmd+S, not on every frame of the spring: opening, on its
        // first frame; folding, once the spring rests closed. Everything else follows the spring, words included: they
        // re-wrap live at the width they have each frame.
        let rest = if self.side_x.settled(0.0) { 0.0 } else { shown };
        if self.chrome_laid == Some((self.chrome, z, width, rest)) {
            return;
        }
        self.chrome_laid = Some((self.chrome, z, width, rest));
        let top = self.chrome.strip / z;
        // The sidebar keeps its open width (its rows never relayout mid-fold); its clip (hn.SideClip) takes the
        // spring's share of it and its seam, and slides both left by the part folded away: at rest folded the clip is
        // 0 wide, so nothing of the sidebar or its hairline paints.
        if let Some(side) = w.entity_from_id(self.ids.device).parent() {
            let mut size = side.try_cloned::<&Size>().unwrap_or(full);
            size.w = Sizing::fixed(open);
            side.set(size);
            if let Some(clip) = side.parent() {
                let mut size = clip.try_cloned::<&Size>().unwrap_or(full);
                size.w = Sizing::fixed(width);
                clip.set(size);
                clip.set(ScrollState { x: shown - width, y: 0.0 });
                clip.set(harness_native_ui_layout_api::RestSize { len: rest });
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

    /// Whether something breathes (a chat status), a sheen moved through words drawn last frame (a working row's title),
    /// or a Kip in view breathes, blinks or springs (the window keeps drawing, at a calm rate, while the app is otherwise
    /// idle). A sheen out of view, or still under Reduce Motion, keeps nothing drawing.
    pub fn breathing(&self) -> bool {
        let paint = self.paint.borrow();
        crate::look::breathing(self.world.world_ref())
            || paint.shimmering
            || paint.blocks_live
            || paint.lantern_live
            || matches!(paint.kip_wake, harness_native_render::kips::Wake::Calm(_))
    }

    /// How long until a finished row's linger runs out (seconds; the window, idle, wakes then and the row folds away).
    pub fn linger_due(&self) -> Option<f64> {
        model::linger_due(self.world.world_ref()).map(|ms| ms as f64 / 1000.0 + 0.02)
    }

    /// A linger ran out while the window slept: a frame lists the tree again.
    pub fn linger_woke(&mut self) {
        self.poke_why("linger");
    }

    /// The Kips in view all rest: how long until one of them blinks or fidgets (the window, idle, wakes then).
    pub fn kip_nap(&self) -> Option<f64> {
        match self.paint.borrow().kip_wake {
            harness_native_render::kips::Wake::Rest(s) if s.is_finite() => Some(s.max(0.05)),
            _ => None,
        }
    }

    /// The window slept `secs` while the Kips in view rested: their clocks move on and a frame draws their blink.
    pub fn kip_woke(&mut self, secs: f64) {
        self.paint.get_mut().rest_kips(secs);
        self.poke_why("kip");
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
            } else if let Some(rest) = a.strip_prefix("chat:turn:") {
                self.chat_turn_toggle(rest);
            } else if let Some(rest) = a.strip_prefix("row:") {
                // An item of a sidebar row's menu (row_menu.rs).
                self.row_menu_action(rest);
            } else if let Some(rest) = a.strip_prefix("chat:pick:") {
                self.chat_pick_action(rest);
            } else if let Some(rest) = a.strip_prefix("chat:set:") {
                self.chat_pick_set(rest);
            } else if let Some(rest) = a.strip_prefix("chat:spawn:") {
                self.chat_pick_spawn(rest);
            } else if let Some(rest) = a.strip_prefix("chat:older:") {
                self.chat_pick_older(rest);
            } else if let Some(rest) = a.strip_prefix("chat:mention:") {
                self.chat_mention_action(rest);
            } else if let Some(rest) = a.strip_prefix("chat:ctx:") {
                self.chat_ctx_toggle(rest);
            } else if let Some(rest) = a.strip_prefix("chat:unattach:") {
                self.chat_unattach(rest);
            } else if let Some(rest) = a.strip_prefix("chat:aside_close:") {
                self.chat_aside_dismiss(rest);
            } else if let Some(rest) = a.strip_prefix("chat:row:") {
                self.chat_row_toggle(rest);
            } else if let Some(ev) = a.strip_prefix("ui:") {
                self.genui_action(ev);
            } else if let Some(w) = a.strip_prefix("web:") {
                // The browser pane's toolbar and menu, a chat card's press (app/browser.rs).
                self.web_action(w);
            } else if let Some(m) = a.strip_prefix("media:") {
                // A press on a cell's Shader or Video (chat/genui/surface.rs): play, mute, resume.
                self.media_action(m);
            } else if let Some(target) = a.strip_prefix("open:") {
                // A link in a view (in the browser pane), or a file (a cell's FileRef) in the app its kind opens in, at its line.
                self.open_target(target, alt);
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
            Do::Rename => self.rename_start(),
            Do::Sidebar => self.toggle_sidebar(),
            Do::Zoom(by) => self.step_zoom(by),
        }
        self.poke();
    }

    // ---- drawing ----

    fn emit(&mut self, main: &mut Draw, sc: f32, upload: &mut dyn FnMut(u32, u32, u32, u32, &[u8])) {
        let held = self.world.lookup(APP).target(Pressed::id(), 0).map(|t| *t.id());
        let painted = harness_native_ui_draw::painted(self.world.world_ref());
        let dark = self.dark;
        // The windowed overlays' draws (src/app/floats.rs), by their node, in the order they first paint.
        let mut floats: Vec<(u64, Draw)> = Vec::new(); // alloc: none, or an overlay window's draw or two a frame
        for (id, mut p) in painted {
            let w = self.world.world_ref();
            let e = w.entity_from_id(id);
            let Some(laid) = e.try_cloned::<&Laid>() else { continue };
            // Where the draw pass says it goes (moved by a transition, mid-slide on a MatchedGeometry), before its scale.
            let laid =
                if p.w > 0.0 || p.h > 0.0 { Laid { x: p.x, y: p.y, w: p.w, h: p.h } } else { laid };
            // A breath dims and brightens; a hover-only part fades with its row's hover.
            p.alpha *= crate::look::alpha(w, e);
            let is_term = e.has(TermNode::id());
            let scroller_part = e.has(harness_native_ui_layout_api::ScrollerPart::id());
            // Nothing is drawn that cannot be seen, a terminal's thousands of cells least of all.
            if !seen(&p) {
                continue;
            }
            // A terminal's cells and a Kip's picture are drawn on the main window's pixel grid and passes only.
            if p.window != 0 && (is_term || e.has(harness_native_ui_text_api::Kip::id())) {
                eprintln!(
                    "harness-native: paint: {} ({id}) is a terminal or a Kip inside windowed overlay {}: an overlay's window draws neither, so it is left out",
                    e.name(),
                    p.window
                );
                continue;
            }
            // Into the main window's draw, or that of the windowed overlay it is in (made by its first node that shows:
            // an overlay faded out has no window).
            let d = floats::target(&mut *main, &mut floats, p.window);
            let rect = [laid.x, laid.y, laid.w, laid.h];
            // F3's sparkline: bars, not a plate.
            if e.has(stats::StatsSpark::id()) {
                self.emit_spark(d, e, rect, p.alpha);
                continue;
            }
            let at = match self.paint.get_mut().node(d, e, &p, laid, p.dark(dark), held == Some(id), sc, upload) {
                Ok(at) => at,
                Err(err) => {
                    eprintln!("harness-native: paint: {err}");
                    continue;
                }
            };
            let (np, ng) = (at.panels, at.glyphs);
            // A windowed overlay's plate casts the window server's shadow (overlay.rs, `hasShadow`), shaped from what its
            // window draws, as a menu's does; its own drawn one would be cut at the window's edge, so it is left off.
            if p.window != 0 && e.has(Lift::id()) {
                if let Some(plate) = d.panels.get_mut(np) {
                    plate.fx[0] = 0.0;
                    plate.fx[1] = 0.0;
                    plate.fx2[0] = 0.0;
                }
            }
            let turn =matches!(p.kind, PaintKind::Icon | PaintKind::Mark).then(|| crate::look::turn(e)).flatten();
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
        self.floats = floats::finish(self.world.world_ref(), floats, sc);
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

    /// Command-[ / Command-]: the previous or next row the sidebar shows ([`model::rows`]); it stops at the ends, never wrapping round. Rows folded
    /// away (under a collapsed node, or a run that has finished and left) are not stepped to, as they cannot be seen.
    fn step_object(&mut self, by: i64) {
        let objs: Vec<u64> = model::rows(self.world.world_ref()).into_iter().map(|r| r.e).collect();
        if objs.is_empty() {
            return;
        }
        let n = objs.len() as i64;
        // From an archived (or no) object: the first or the last of the tree.
        let at = self
            .selected()
            .and_then(|s| objs.iter().position(|&e| e == s))
            .map_or(if by > 0 { -1 } else { n }, |i| i as i64);
        // Stops at the first and last rows rather than wrapping round.
        let to = (at + by).clamp(0, n - 1);
        if to != at {
            self.select(Some(objs[to as usize]));
        }
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
        self.spawn_on(kind, None, None, self.selected_cwd());
    }

    /// The selected object's directory.
    fn selected_cwd(&self) -> Option<String> {
        self.selected()
            .and_then(|e| self.world.entity_from_id(e).try_cloned::<&model::Cwd>())
            .map(|c| c.s.as_str().to_owned())
            .filter(|c| !c.is_empty())
    }

    /// A new object of `kind` in `cwd` (else home), its agent started on `model` at `effort` when given; selected once
    /// the list has it.
    fn spawn_on(&mut self, kind: &str, model: Option<String>, effort: Option<String>, cwd: Option<String>) {
        let cwd = cwd.or_else(|| std::env::var("HOME").ok());
        let (cols, rows) = self.focused_cells().unwrap_or((120, 40));
        let slot = self.spawned.clone();
        let asked = (model.clone(), effort.clone());
        self.link.spawn(kind, cwd, model, effort, cols, rows, move |sid| {
            *slot.lock().unwrap() = Some(Spawned { sid, model: asked.0, effort: asked.1 });
        });
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
        *self.spawned.lock().unwrap() = Some(Spawned { sid, model: None, effort: None });
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
        crate::audio::cue("open");
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

/// How long ago, for a session's row, as short as it reads: `now`, `4m`, `3h`, `2d`, `5w`.
/// The mark a finished row shows in its gutter while it lingers (hn.Hang's `mark`): "fail" when it was killed, lost
/// or exited non-zero; "" otherwise. A row that ended well gets no mark: its title fades (`look` "done") as an agent's
/// does, and a check on every finished step was only noise. A top-level session (depth 0) never gets one.
fn done_mark(status: &str, code: Option<i32>, depth: usize) -> &'static str {
    match status {
        _ if depth == 0 => "",
        "killed" | "lost" => "fail",
        "running" | "pending" => "",
        _ if code.is_some() => "fail",
        _ => "",
    }
}

/// What a working agent is doing now, from its transcript's last item: the words its tooltip leads with, and the
/// detail its row trails (the command, the file, the pattern, the page): only what names a thing. A state alone
/// (thinking, writing) trails nothing, since the row's dot already says it is at work.
struct Doing {
    what: &'static str,
    detail: String,
}

impl Doing {
    /// Its tooltip's words: what it does and on what ("Running a command: cargo test"), or only what when the detail says
    /// no more ("Thinking").
    fn tip(&self) -> String {
        if self.detail.is_empty() || self.detail.eq_ignore_ascii_case(self.what) {
            self.what.to_owned()
        } else {
            format!("{}: {}", self.what, self.detail)
        }
    }
}

/// [`Doing`] of transcript `t` while its agent works (None at rest or before it says anything).
fn doing_of(t: &model::chat::Transcript) -> Option<Doing> {
    use model::chat::Item;
    if !t.busy {
        return None;
    }
    let base = |p: &str| p.rsplit('/').next().unwrap_or(p).to_owned();
    // A message still queued is not what it is on.
    let last = (0..t.items.len()).rev().find(|&k| !t.is_queued(k)).map(|k| &t.items[k])?;
    Some(match last {
        Item::Thinking { done: false, .. } => Doing { what: "Thinking", detail: String::new() },
        // A harness REPL cell: what its running step does ("blaze check", "Reading chat.rs"), never the tool's name.
        Item::Tool { block, result: None, .. } if t.details.contains_key(block) => {
            match t.details[block].effects.iter().rev().find(|e| e.status == "running") {
                Some(e) if e.kind == "exec" => Doing { what: "Running a command", detail: chat::step_words(e) },
                Some(e) => Doing { what: "Working", detail: chat::step_words(e) },
                None => Doing { what: "Running code", detail: String::new() },
            }
        }
        Item::Tool { name, input, result: None, .. } => {
            let s = |k: &str| input[k].as_str().unwrap_or("").to_owned();
            let n = name.to_ascii_lowercase();
            if ["bash", "shell", "command", "exec"].iter().any(|k| n.contains(k)) {
                Doing { what: "Running a command", detail: short_cmd(&s("command")) }
            } else if ["edit", "write", "patch", "filechange"].iter().any(|k| n.contains(k)) {
                Doing { what: "Editing", detail: base(&s("file_path")) }
            } else if ["read", "grep", "glob", "search", "ls"].iter().any(|k| n.contains(k)) && !n.contains("web") {
                let what = s("file_path");
                Doing { what: "Reading", detail: if what.is_empty() { s("pattern") } else { base(&what) } }
            } else if n.contains("web") || n.contains("fetch") {
                let url = s("url");
                let host = url.split("://").nth(1).and_then(|r| r.split('/').next()).map_or_else(|| s("query"), str::to_owned);
                Doing { what: "On the web", detail: host }
            } else if n.contains("task") || n.contains("agent") {
                Doing { what: "Running a sub-agent", detail: s("description") }
            } else {
                Doing { what: "Using a tool", detail: name.rsplit("__").next().unwrap_or(name).to_owned() }
            }
        }
        Item::Text { done: false, .. } => Doing { what: "Writing", detail: String::new() },
        _ => Doing { what: "Thinking", detail: String::new() },
    })
}

/// The last part of directory `cwd` (the home directory as `~`): where a session works, as its row trails it.
fn folder_of(cwd: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let cwd = cwd.trim_end_matches('/');
    if !home.is_empty() && cwd == home.trim_end_matches('/') {
        return "~".to_owned();
    }
    cwd.rsplit('/').next().unwrap_or(cwd).to_owned()
}

fn ago(ms: u64) -> String {
    let m = ms / 60_000;
    match m {
        0 => "now".to_owned(),
        1..60 => format!("{m}m"),
        60..1440 => format!("{}h", m / 60),
        1440..10_080 => format!("{}d", m / 1440),
        _ => format!("{}w", m / 10_080),
    }
}

/// A run's command as its tree row shows it: its first line, without the `cd <dir> &&` and `VAR=value` before what it
/// runs, the program by its name rather than its path (`/usr/bin/gh repo list` reads `gh repo list`), whitespace folded.
/// The row cuts what is still too long with an ellipsis.
fn short_cmd(cmd: &str) -> String {
    let line = cmd.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let mut rest = line;
    // Leading `cd <dir> &&` (or `;`) steps: where it runs is the pane's to say.
    while let Some(after) = rest.strip_prefix("cd ") {
        match after.find("&&").map(|i| (i, 2)).or_else(|| after.find(';').map(|i| (i, 1))) {
            Some((i, n)) => rest = after[i + n..].trim_start(),
            None => break,
        }
    }
    let mut words = rest.split_whitespace().peekable();
    // Leading environment assignments.
    while words.peek().is_some_and(|w| {
        w.split_once('=').is_some_and(|(k, _)| !k.is_empty() && k.chars().all(|c| c == '_' || c.is_ascii_alphanumeric()))
    }) {
        words.next();
    }
    let mut out: Vec<&str> = Vec::new();
    if let Some(prog) = words.next() {
        out.push(if prog.starts_with('/') || prog.starts_with("./") { prog.rsplit('/').next().unwrap_or(prog) } else { prog });
    }
    out.extend(words);
    if out.is_empty() { line.split_whitespace().collect::<Vec<_>>().join(" ") } else { out.join(" ") }
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
            failed: tone(w, "error"),
            failed_wash: tone(w, "error_wash"),
            faint: tone(w, "ink_faint"),
            lost: tone(w, "warning"),
            badge_on: crate::look::padding(w, "badge_on"),
        }
    }
}

fn set_if(v: View<'_>, fill: Fill) {
    if v.entity().try_cloned::<&Fill>() != Some(fill) {
        v.set(fill);
    }
}

/// The scene's sound (`hn.sound`, scene/widgets/sound.flecs): its master level, its cues and its themes.
///
/// # Errors
/// A part of it is missing or malformed, named.
/// The scene's sound as [`scene_sound`] reads it: the master, the music's and the rain's levels, the places the rain
/// falls, the cues and the moods.
type SceneSound = (f32, f32, f32, Vec<crate::music::RainSource>, Vec<crate::audio::Cue>, HashMap<String, crate::music::Mood>);

fn scene_sound(w: WorldRef<'_>) -> Result<SceneSound, String> {
    use harness_native_blaze::sound::{Note, Voice as V};
    use harness_native_ui_timeline_api::{Gain, Lasts, Onset, Pan, Pitch, Sound, Voice};
    let at = |path: &str| w.try_lookup(path).ok_or_else(|| format!("the scene has no {path} (scene/widgets/sound.flecs)"));
    let voice = |v: Voice| match v {
        Voice::Bell => V::Bell,
        Voice::Mallet => V::Mallet,
        Voice::Bass => V::Bass,
        Voice::Click => V::Click,
        Voice::Thud => V::Thud,
        Voice::Whoosh => V::Whoosh,
        Voice::Fire => V::Fire,
    };
    let gain = |e: EntityView<'_>| e.try_cloned::<&Gain>().map_or(0.0, |g| g.amp);
    let lasts = |e: EntityView<'_>| e.try_cloned::<&Lasts>().map_or(0.0, |l| l.dur);
    let key = at("hn::sound::key")?.try_cloned::<&Pitch>().map(|p| p.hz).filter(|hz| *hz > 0.0).ok_or("hn.sound.key has no Pitch")?;
    let mut cues = Vec::new(); // alloc: the scene's sounds, read once
    at("hn::sound::cue")?.each_child(|c| {
        let mut notes = Vec::new();
        c.each_child(|n| {
            if let (Some(s), Some(o)) = (n.try_cloned::<&Sound>(), n.try_cloned::<&Onset>()) {
                let Pan { pos, sweep } = n.try_cloned::<&Pan>().unwrap_or_default();
                let freq = n.try_cloned::<&Pitch>().map_or(0.0, |p| p.hz);
                notes.push(Note { voice: voice(s.voice), at: o.at.max(0.0), dur: lasts(n), freq, amp: gain(n), pan: pos.clamp(-1.0, 1.0), sweep });
            }
        });
        notes.sort_by(|a, b| a.at.total_cmp(&b.at));
        cues.push(crate::audio::Cue(c.name(), gain(c), notes));
    });
    let mut moods = HashMap::new(); // alloc: the scene's moods, read once
    let mut bad = None;
    at("hn::sound::mood")?.each_child(|m| {
        let Some(mood) = m.try_cloned::<&crate::audio::Mood>() else { return };
        // Its parts: each child that is a part (a `hn.sound.part` prefab's instance, its instrument inherited with it).
        let mut parts = Vec::new();
        m.each_child(|c| {
            let (Some(part), Some(inst)) = (c.try_cloned::<&crate::audio::Part>(), c.try_cloned::<&crate::audio::Instrument>()) else { return };
            match inst.read(&c.path()).and_then(|i| part.read(c.name(), i, gain(c), &c.path())) {
                Ok(p) => parts.push(p),
                Err(e) => bad = Some(e),
            }
        });
        parts.sort_by(|a, b| a.name.cmp(&b.name));
        match mood.read(parts, &m.path()) {
            Ok(read) => _ = moods.insert(m.name(), read),
            Err(e) => bad = Some(e),
        }
    });
    if let Some(e) = bad {
        return Err(e);
    }
    cues.sort_by(|a, b| a.0.cmp(&b.0));
    let mut rain = Vec::new(); // alloc: the places the rain falls, read once
    at("hn::sound::ambient")?.each_child(|c| {
        if let Some(r) = c.try_cloned::<&crate::audio::RainOn>() {
            rain.push((c.name(), crate::music::RainSource { azimuth: r.azimuth, elevation: r.elevation, distance: r.distance, gain: gain(c), hiss: r.hiss, rate: r.rate, low: r.low, high: r.high, ring: r.ring, bend: r.bend }));
        }
    });
    rain.sort_by(|a, b| a.0.cmp(&b.0));
    let rain = rain.into_iter().map(|(_, r)| r).collect();
    Ok((gain(at("hn::sound")?), gain(at("hn::sound::mood")?), gain(at("hn::sound::ambient")?), rain, cues, moods))
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

    /// Zoom is a uniform scale with nothing sliding: a MatchedGeometry pill (the test's own: the picker's highlight
    /// lands at once, so the test matches it itself) on a row of a card centred in the window, so the zoom moves the row
    /// in view points, is mid-slide to a new row when cmd+= lands; in that
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
        fn pill_on<'a>(row: View<'a>) -> View<'a> {
            let pill = row.child("hn.PickPill");
            pill.set(harness_native_ui_motion_api::MatchedGeometry { id: "test.pill".into(), jump: 0.0 });
            pill
        }
        pill_on(rows[0]);
        for _ in 0..30 {
            frame(1.0);
        }
        // Down one row: the old pill goes, the new row's pill slides from where the old one was drawn.
        rows[0].entity().each_child(|c| c.destruct());
        let pill = pill_on(rows[1]);
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
        let pill = pill_on(rows[2]);
        frame(z);
        frame(z);
        assert!((drawn(pill).y - laid(rows[2]).y).abs() > 1.0, "a selection still slides");
    }

    /// The scene's sound reads whole: every step item has a cue, `ready` and `done` ring, and every mood of the ladder is there.
    #[test]
    fn the_scenes_sound_reads() {
        let world = scene_world().unwrap_or_else(|e| panic!("{e}"));
        let (level, music, ambient, rain, cues, moods) = scene_sound(world.world_ref()).unwrap_or_else(|e| panic!("{e}"));
        assert!(level > 0.0 && music > 0.0 && ambient > 0.0 && rain.len() >= 3 && rain.iter().all(|r| r.gain > 0.0 && r.high > r.low));
        for item in ["exec", "read", "edit", "search", "net", "build", "spawn", "wait", "sql", "think"] {
            assert!(cues.iter().any(|c| c.0 == item && c.1 > 0.0 && !c.2.is_empty()), "no cue for {item}");
        }
        assert!(["ready", "done"].iter().all(|n| cues.iter().any(|c| c.0 == *n && !c.2.is_empty())));
        for mood in ["rest", "calm", "flow", "drive", "battle"] {
            assert!(moods.get(mood).is_some_and(|m| m.tempo > 0.0 && !m.scale.is_empty() && !m.chords.is_empty() && m.parts.len() >= 2 && m.parts.iter().all(|p| p.level > 0.0 && !p.instrument.partials.is_empty() || p.instrument.noise > 0.0)), "mood {mood} is missing or has no parts");
        }
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
        for prefab in ["hn.Tree", "hn.Terminal", "ui.TextArea", "ui.field.GrowingTextArea"] {
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
            "hn.ChatTableField", "hn.ChatTableFieldMd", "hn.ChatTable", "hn.ChatGrid", "hn.ChatOutput",
            "hn.StepLine", "hn.StepLineFailed", "hn.StepHead", "hn.StepHeadFailed", "hn.StepPage", "hn.StepFile",
            "hn.StepResults", "hn.StepResultsQuery", "hn.StepResult", "hn.StepText", "hn.StepCode", "hn.StepValue",
            "hn.StepFail", "hn.StepNote", "hn.StepGrid",
            "hn.StepKind.exec", "hn.StepKind.read", "hn.StepKind.edit", "hn.StepKind.write", "hn.StepKind.delete", "hn.StepKind.search", "hn.StepKind.list",
            "hn.StepKind.build", "hn.StepKind.net", "hn.StepKind.sql", "hn.StepKind.spawn", "hn.StepKind.wait", "hn.StepKind.cell", "hn.StepKind.tool", "ex.TableDemo",
            "hn.chat_status.thinking", "hn.chat_status.thought", "hn.ChatPick", "hn.ChatControlInk.rest", "hn.ChatControlInk.hot",
            "hn.ChatControlInk.off", "hn.ChatEffort", "hn.ChatEffortBar", "ui.caret",
            "hn.ChatContext", "hn.ChatPlan", "hn.ChatWorking", "hn.ChatAside", "hn.ChatAsideWorking", "hn.ChatNote", "hn.ChatError",
            "hn.ChatItem", "hn.ChatItemNumber", "hn.ChatItemTask",
            "hn.ChatPeek", "hn.ChatPeekAnchor", "hn.PeekSite", "hn.PeekSiteGlyph", "hn.PeekTitle", "hn.PeekState", "hn.PeekMeta", "hn.PeekBody", "hn.PeekCode",
            "hn.PeekImage", "hn.PeekPage", "hn.PeekCheckFailed", "hn.PeekCheckRunning", "hn.PeekCheckQueued", "hn.PeekChecksFolded",
            "hn.chat_status.running", "hn.chat_status.no_result", "hn.tree.Head", "hn.tree.Cmd", "hn.tree.Trail",
            "hn.ChatUi", "hn.GenTone.ink",
            "hn.GenTone.error", "hn.GenColumn", "hn.GenRow", "hn.GenText", "hn.GenMarkdown", "hn.GenCode", "hn.GenTable",
            "hn.GenBadge", "hn.GenStat", "hn.GenCard", "hn.GenDivider", "hn.GenSpacer", "hn.GenUnknown",
            "hn.GenCanvas", "hn.GenShape", "hn.GenShapeBack", "hn.GenChart", "hn.GenChartPlot", "hn.GenChartTick",
            "hn.GenChartBar", "hn.GenChartGroup", "hn.GenChartStack", "hn.GenChartHit", "hn.GenChartLabel",
            "hn.GenChartLegend", "hn.GenChartKey", "hn.GenChartPie", "hn.GenChartCenter", "hn.GenTone.faint",
            "hn.GenTone.series1", "hn.GenTone.series8", "ui.Browser", "hn.GenBrowser", "hn.WebClip", "hn.WebPane",
            "hn.WebMenu", "hn.WebPopup", "ui.MenuItem", "ui.MenuNote", "ui.MenuSep", "hn.ChatMention", "hn.ChatMentionItem", "hn.ChatMentionHead", "hn.ChatMentionCode", "hn.ChatMentionImage", "hn.PeekList", "hn.PeekEntry", "hn.PeekMeta",
        ]
        .into_iter()
        .filter(|p| harness_native_ui_view::prefab(world.world_ref(), p).is_none())
        .collect();
        assert!(missing.is_empty(), "prefabs the code names but no scene has: {missing:?}");
        // The scene's modules are modules, and the inline vocabulary has what the painter and a press read
        // (harness_native_render::inline).
        let w = world.world_ref();
        let module = w.try_lookup("flecs::core::Module").expect("flecs has no flecs.core.Module");
        for m in ["ui::inline", "hn::chat_status", "hn::tree"] {
            assert!(w.try_lookup(m).is_some_and(|e| e.has(module)), "{m} is not a module");
        }
        for e in [
            "look", "rest", "meta", "unknown", "hover", "pressed", "underline", "live", "marks::success", "marks::failure",
            "marks::pending", "marks::success::card", "marks::failure::card", "marks::pending::card", "Path", "FileRef",
            "Symbol", "Link", "Link::unknown", "PR::open", "PR::draft", "PR::merged", "PR::closed", "PR::unknown",
            "Issue::open", "Issue::closed", "Issue::not_planned", "Issue::unknown",
        ] {
            let path = format!("{}::{e}", harness_native_render::inline::MODULE);
            assert!(w.try_lookup(&path).is_some(), "the scene has no {path}");
        }
        // The theme's colour roles (scene/00-tokens.flecs's `tone_<role>`, scene/widgets/theme.flecs's children): each has
        // a Fill, so the roles the code names (look::tone panics on a missing one) are all there.
        for role in [
            "ground", "sidebar", "card", "raised", "well", "term_ground", "line", "line_strong", "ink_strong", "ink",
            "ink_muted", "ink_faint", "hover", "wash", "selection", "selection_focus", "press", "scrim", "clear",
            "text_selection", "copy_sheen", "sheen", "accent", "accent_wash", "on_accent", "link", "code_ink", "success", "warning", "error",
            "success_wash", "warning_wash", "error_wash", "kind_claude", "kind_codex", "kind_cursor", "kind_shell", "aside",
            "aside_line",
        ] {
            let _ = crate::look::tone(w, role);
        }
    }

    /// The colour of a brace `{...}` at the start of `s` when it is a colour literal: four plain numbers (`{0.1, 0.1,
    /// 0.1, 1}`) or four named `r`, `g`, `b`, `a` (`{r: 1, g: 1, b: 1, a: 1}`). A Padding's `{l, r, t, b}` is not one.
    fn colour_literal(s: &str) -> bool {
        let Some(body) = s.strip_prefix('{').and_then(|b| b.split_once('}')).map(|(b, _)| b) else { return false };
        let parts: Vec<&str> = body.split(',').map(str::trim).collect();
        if parts.len() != 4 {
            return false;
        }
        let num = |p: &str| p.parse::<f32>().is_ok();
        parts.iter().all(|p| num(p))
            || parts.iter().zip(["r", "g", "b", "a"]).all(|(p, k)| {
                p.split_once(':').is_some_and(|(name, v)| name.trim() == k && num(v.trim()))
            })
    }

    /// One theme: no colour is chosen anywhere but the theme. Outside the tokens files (harness-native's
    /// scene/00-tokens.flecs, which holds the theme's roles and the palettes; harness-video's tokens.flecs, its mirror)
    /// and Kip's artwork (kip.flecs: a character's own paint, not a surface), no scene script writes a colour literal, and
    /// no Rust in this crate builds an `Rgba` from numbers: each names a role (`Fill: $tone_card`, `look::tone(w, "card")`).
    /// `cargo test -p harness-native no_raw_colours`.
    #[test]
    fn no_raw_colours() {
        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let roots = [
            here.join("scene"),
            here.join("src"),
            here.join("../harness-native-ui-prims/scene"),
            here.join("../harness-video/scene"),
        ];
        let allowed = [
            "harness-native/scene/00-tokens.flecs",
            "harness-video/scene/tokens.flecs",
            "harness-native-ui-prims/scene/kip.flecs",
        ];
        let mut files = Vec::new();
        let mut dirs: Vec<std::path::PathBuf> = roots.to_vec();
        while let Some(d) = dirs.pop() {
            let entries = std::fs::read_dir(&d).unwrap_or_else(|e| panic!("{}: {e}", d.display()));
            for entry in entries {
                let p = entry.unwrap_or_else(|e| panic!("{}: {e}", d.display())).path();
                if p.is_dir() {
                    dirs.push(p);
                } else if p.extension().is_some_and(|x| x == "flecs" || x == "rs") {
                    files.push(p);
                }
            }
        }
        files.sort();
        assert!(files.iter().any(|f| f.ends_with("scene/widgets/frame.flecs")), "found no scene scripts under {roots:?}");
        let mut bad = Vec::new();
        for f in files.iter().filter(|f| !allowed.iter().any(|a| f.ends_with(a))) {
            let src = std::fs::read_to_string(f).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
            let rust = f.extension().is_some_and(|x| x == "rs");
            for (i, line) in src.lines().enumerate() {
                // A Rust file's tests (at its end) may build what they check.
                if rust && line.trim_start().starts_with("#[cfg(test)]") {
                    break;
                }
                let code = line.split("//").next().unwrap_or("");
                let hit = if rust {
                    code.match_indices("Rgba {").any(|(at, _)| colour_literal(&code[at + "Rgba ".len()..]))
                } else {
                    code.match_indices('{').any(|(at, _)| colour_literal(&code[at..]))
                };
                if hit {
                    bad.push(format!("{}:{}: {}", f.display(), i + 1, line.trim()));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "a colour outside the theme (name a role: a scene's `$tone_<role>`, Rust's `look::tone(w, \"<role>\")`; add a \
             role to scene/00-tokens.flecs and hn.Theme if none fits):\n{}",
            bad.join("\n")
        );
    }

    /// The raw-colour check sees both literal shapes and lets a Padding by.
    #[test]
    fn colour_literals() {
        assert!(colour_literal("{0.063, 0.063, 0.067, 1}"));
        assert!(colour_literal("{r: 1, g: 0.5, b: 0, a: 1}}"));
        assert!(!colour_literal("{l: 0, r: 0, t: 0, b: 0}"));
        assert!(!colour_literal("{dark: $tone_line.dark, light: $tone_line.light}"));
        assert!(!colour_literal("{w: 1, h: 2}"));
    }

    /// A closing hover card keeps its place: the chat's `peek` slot is a node, so the card (a ui.Menu, placed and
    /// anchored) is its child, and leaving it must not fold it into the slot's flow (`Collapse`, `Clip`), where layout lays
    /// a stand-in at the flow's start and writes that corner into its `Laid`: its window jumped to the pane's top-left as
    /// it faded. A row in a flow still folds away as it goes (the control).
    #[test]
    fn leaving_overlay_stays_out_of_the_flow() {
        use harness_native_ui_layout_api::Collapse;
        use harness_native_ui_motion_api::{Leaving, Transition};
        use harness_native_ui_node_api::{Clip, Placed};
        let world = scene_world().unwrap_or_else(|e| panic!("{e}"));
        let slot = View::root(world.world_ref(), "ui.Node");
        // The app's own overlays have no Transition (they are gone at once); the test gives the card one, as a placed
        // overlay that fades out would have.
        let card = slot.child("hn.ChatPeek").set(Transition::default());
        assert!(card.entity().has(Placed::id()), "hn.ChatPeek is no placed overlay");
        view::leave(card);
        let e = card.entity();
        assert!(e.has(Leaving::id()), "the card is not leaving");
        assert!(!e.has(Collapse::id()) && !e.has(Clip::id()), "a leaving overlay was folded into its slot's flow");
        let row = slot.child("ui.Row").set(Transition::default());
        view::leave(row);
        assert!(row.entity().has(Collapse::id()), "a leaving row in a flow no longer folds away");
    }
}
