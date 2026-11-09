//! The embedded browser in the app (crates/harness/harness-browser's README, "App ⇄ host: Mach" and "In the app"): the
//! Mach link to the daemon's browser host, every `ui.Browser` node drawn as its view's live surface, the browser pane on
//! the right (`hn.WebPane`, its state on `hn.web`), and the person's pointer, keys and input method forwarded to the view
//! that has them.
//!
//! The link. On first use (a node that shows a view, or the pane opened) the app asks the daemon for `browser` once, on a
//! thread of its own (the request blocks until the host is up); the answer names the host's Mach service. The app looks
//! it up, makes a receive port drained on the main queue (harness-mach's `Source`), and sends `AppUp::Hello` with a send
//! right to it. What arrives is queued and handled at the next frame: `Views` (url, title, loading, history), `Ring` (the
//! view's IOSurfaces, into Media), `Frame` (its newest slot, into Media, which latches it once the GPU is done with the
//! one shown and reports `AppUp::Latched` back), `Cursor` (the node's pointer shape), `ImeRect` (the candidate window),
//! `Opened`, `Closed`, `Extensions`, `Error`. Anything refused (no daemon, no host, a lost link, a bad ring) is shown on
//! the plate of every node it leaves without a picture, naming the harness browser and what the daemon or host said;
//! nothing stands in for it. Opening the pane again after a lost link asks the daemon again.
//!
//! On screen. A view is watched (`AppUp::Watch`) while a node showing it is drawn, unwatched when none is. The pane's view
//! is told its box (`AppUp::Resize`, its laid points at the frame's device scale) whenever that changes; a chat card
//! never resizes the page (an agent drives it at its own size): it shows the ring scaled to the card, cut to its shape.
//!
//! Input. The pane's page (or a popup) takes the pointer: moves, presses, releases, leaving and the wheel, in view points
//! with CEF's modifier bits; a press on it gives it `BrowserFocus`, and a press anywhere else takes it away. While a view
//! has it the keys go there (`AppUp::Key` with the Mac key code and the characters) after the bindings in ctx.Browser
//! (cmd+L, cmd+[, cmd+], cmd+R: scene/20-keys.flecs), and the input method's work goes through the window's
//! NSTextInputClient path as `AppUp::Ime` (setMarkedText: Set, insertText: Commit, unmarkText: Finish). A card's press
//! opens its view in the pane; a `web:open:<url>` action (a check's row on a pull request's hover card), an `open:` web
//! link and a chat's link or chip (a pull request, an issue, a page) open that page in the pane as a view of its own
//! ([`App::open_target`]), and so does a file a chat or a cell names, as a `file://` page; Option-pressed, either opens
//! outside (the system's browser, the file's own app).
use super::*;
use harness_mach::iosurface::Surface as IoSurface;
use harness_mach::{Msg, RecvPort, SendRight, Source};
use harness_native_render::media::{BrowserSpec, Media, Spec as MediaSpec, browser as mb};
use harness_native_ui_browser_api::{BrowserFocus, BrowserView};
use harness_native_ui_input_api::Cursor;
use harness_native_ui_text_api::Surface;
use harness_proto::browser::{self as hb, AppDown, AppUp, Extension, Ime, KeyKind, Nav, PointerKind, ViewInfo, mods};
use harness_proto::cbor;
use harness_proto::msg::{Reply, Request};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, TryRecvError};

mod drag;
mod fileview;

/// The `file://` URL of absolute path `path`: each byte outside the unreserved set and `/` percent-encoded.
fn file_url(path: &str) -> String {
    let mut out = String::from("file://");
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The pane's state, on `hn.web` (scene/widgets/web.flecs): the view it shows (0 none yet), whether it is open (asked
/// for: it shows only once it has a page, [`App::web_shown`]), its width in points as last dragged (0: hn.WebPane's).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct WebState {
    pub view: u64,
    pub open: bool,
    pub width: f32,
}

pub(super) fn register(world: &World) {
    world.component::<WebState>().writable();
}

const WEB: &str = "hn::web";
/// The page the pane opens on when the host has no view to show it.
const BLANK: &str = "about:blank";
/// The Chrome Web Store, opened in the host's manage window from the extensions menu.
const STORE: &str = "https://chromewebstore.google.com";
/// Mac virtual key codes the edit commands and the address field use.
const KEY_A: u16 = 0;
const KEY_X: u16 = 7;
const KEY_C: u16 = 8;
const KEY_V: u16 = 9;
const KEY_RETURN: u16 = 36;
const KEY_BACKSPACE: u16 = 51;
const KEY_ESCAPE: u16 = 53;
/// The toolbar's glyphs (codicons, like look.rs's): chevron-left, chevron-right, refresh, extensions, close.
const BACK_ICON: &str = "\u{EAB5}";
const FORWARD_ICON: &str = "\u{EAB6}";
const RELOAD_ICON: &str = "\u{EB37}";
const EXT_ICON: &str = "\u{EAE6}";
const CLOSE_ICON: &str = "\u{EA76}";

/// What the main-queue handler hands the frame: messages with their port rights, or why the link broke.
#[derive(Default)]
struct Inbox {
    downs: Vec<(AppDown, Vec<SendRight>)>, // alloc: the messages between two frames
    broke: Option<String>,
}

/// The link to the host.
enum Conn {
    /// Not asked yet (nothing has wanted a view).
    Idle,
    /// The daemon is being asked on its thread: the host's Mach service, or the daemon's refusal.
    Asking(Receiver<Result<String, String>>),
    /// Hello sent; `_source` drains our port until it drops.
    Live { _source: Source },
    /// Why there is no link, naming the harness browser and what refused.
    Failed(String),
}

/// What an `AppUp::Open` or `Action` was for, by its request id.
enum Opening {
    /// A node's `src`.
    Src(String),
    /// The pane, which had no view.
    Pane,
    /// An extension's action popup.
    Popup,
}

pub(super) struct Browser {
    conn: Conn,
    inbox: Rc<RefCell<Inbox>>,
    /// The host's send right, shared with Media's latch sink.
    host: Rc<RefCell<Option<SendRight>>>,
    /// Errors the latch sink met (it has no App to put them on).
    latch_errors: Rc<RefCell<Vec<String>>>, // alloc: rarely more than one, read each frame
    latch_set: bool,
    pub(in crate::app) media: Option<Rc<RefCell<Media>>>,
    wake: Wake,
    /// The host's views as its last `Views` said, and whether one has come.
    views: HashMap<u64, ViewInfo>,
    listed: bool,
    opening: HashMap<u64, Opening>,
    next_req: u64,
    /// Each `src` opened, its view; each refused, why.
    opened: HashMap<String, u64>,
    refused: HashMap<String, String>,
    /// Views the host closed, and why; a view's ring or frame it could not take.
    closed: HashMap<u64, String>,
    errors: HashMap<u64, String>,
    /// The host's last error that named no request.
    note: Option<String>,
    watched: BTreeSet<u64>,
    /// The pane's view's box as last sent: points and the device scale's bits.
    sized: HashMap<u64, (u32, u32, u32)>,
    cursors: HashMap<u64, Pointer>,
    ime_rect: HashMap<u64, [f32; 4]>,
    /// The node with BrowserFocus, and the view a press holds (its moves and release go there wherever the pointer is).
    focus: Option<u64>,
    grab: Option<(u64, u64)>,
    /// The view the pointer was last over (it hears Leave when the pointer goes).
    over: Option<(u64, u64)>,
    marked: Option<String>,
    /// The pane's fold: 0 shut, 1 open.
    x: Spring,
    clip: u64,
    pane: u64,
    /// The address field's words while it is edited.
    url: Option<String>,
    menu: bool,
    ext_req: Option<u64>,
    extensions: Option<Vec<Extension>>,
    popup: Option<u64>,
    /// A page to open in the pane as a view of its own once the link is up (a check's log from a chat card, `web:open:`).
    go: Option<String>,
    /// The pane's left edge while it is dragged (drag.rs).
    drag: Option<drag::Grab>,
}

impl Browser {
    pub(super) fn new(wake: Wake, clip: u64, pane: u64) -> Self {
        Self {
            conn: Conn::Idle,
            inbox: Rc::default(),
            host: Rc::default(),
            latch_errors: Rc::default(),
            latch_set: false,
            media: None,
            wake,
            views: HashMap::new(),
            listed: false,
            opening: HashMap::new(),
            next_req: 1,
            opened: HashMap::new(),
            refused: HashMap::new(),
            closed: HashMap::new(),
            errors: HashMap::new(),
            note: None,
            watched: BTreeSet::new(),
            sized: HashMap::new(),
            cursors: HashMap::new(),
            ime_rect: HashMap::new(),
            focus: None,
            grab: None,
            over: None,
            marked: None,
            x: Spring::at(0.0),
            clip,
            pane,
            url: None,
            menu: false,
            ext_req: None,
            extensions: None,
            popup: None,
            go: None,
            drag: None,
        }
    }

    fn req(&mut self) -> u64 {
        let r = self.next_req;
        self.next_req += 1;
        r
    }

    /// Send `m` to the host; a refused send breaks the link, naming what was sent and why.
    fn up(&mut self, m: &AppUp) {
        let sent = match self.host.borrow().as_ref() {
            Some(h) => h.send(&cbor::encode(m), &[]),
            None => return,
        };
        if let Err(e) = sent {
            self.fail(format!("the harness browser's link broke sending {}: {e}", what(m)));
        }
    }

    fn fail(&mut self, why: String) {
        eprintln!("harness-native: browser: {why}");
        self.conn = Conn::Failed(why);
        *self.host.borrow_mut() = None;
        self.watched.clear();
        self.sized.clear();
    }

    fn live(&self) -> bool {
        matches!(self.conn, Conn::Live { .. })
    }

    /// Ask the daemon for the host, once, on a thread of its own.
    fn ask(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        let wake = self.wake.clone();
        let spawned = std::thread::Builder::new().name("harness-browser-ask".into()).spawn(move || {
            let r = match crate::link::request(Request::Browser) {
                Ok(Ok(Reply::Browser { service, .. })) => Ok(service),
                Ok(Ok(other)) => Err(format!("the daemon answered `browser` with {other:?}")),
                Ok(Err(e)) => Err(format!("the daemon refused `browser`: {e}")),
                Err(e) => Err(format!("no daemon to ask for it ({}): {e}", crate::link::sock().display())),
            };
            let _ = tx.send(r);
            wake();
        });
        self.conn = match spawned {
            Ok(_) => Conn::Asking(rx),
            Err(e) => Conn::Failed(format!("Not connected to the harness browser: no thread to ask the daemon on: {e}")),
        };
    }

    /// Look up the host's service, make our port and say hello.
    fn connect(&mut self, service: &str) -> Result<(), String> {
        let host = SendRight::look_up(service).map_err(|e| format!("looking up its Mach service {service}: {e}"))?;
        let port = RecvPort::new().map_err(|e| format!("making the app's receive port: {e}"))?;
        let mine = port.make_send().map_err(|e| format!("making a send right to the app's port: {e}"))?;
        let inbox = self.inbox.clone();
        let wake = self.wake.clone();
        let source = Source::on_main_queue(
            port,
            Box::new(move |m: Result<Msg, harness_mach::Error>| {
                let mut i = inbox.borrow_mut();
                match m {
                    Ok(Msg { body, ports }) => match cbor::decode::<AppDown>(&body) {
                        Ok(d) => i.downs.push((d, ports)),
                        Err(e) => i.broke = Some(format!("the host sent a message the app cannot read: {e}")),
                    },
                    Err(e) => i.broke = Some(format!("receiving from the host: {e}")),
                }
                drop(i);
                wake();
            }),
        )
        .map_err(|e| format!("draining the app's port on the main queue: {e}"))?;
        host.send(&cbor::encode(&AppUp::Hello { schema: harness_proto::SCHEMA }), &[&mine])
            .map_err(|e| format!("sending hello to {service}: {e}"))?;
        *self.host.borrow_mut() = Some(host);
        self.conn = Conn::Live { _source: source };
        Ok(())
    }
}

/// An AppUp's name, for an error.
fn what(m: &AppUp) -> &'static str {
    match m {
        AppUp::Hello { .. } => "hello",
        AppUp::Watch { .. } => "watch",
        AppUp::Resize { .. } => "resize",
        AppUp::Pointer { .. } => "a pointer event",
        AppUp::Key { .. } => "a key",
        AppUp::Ime { .. } => "the input method's text",
        AppUp::Focus { .. } => "focus",
        AppUp::Latched { .. } => "latched",
        AppUp::Nav { .. } => "a navigation",
        AppUp::Open { .. } => "open",
        AppUp::Close { .. } => "close",
        AppUp::Manage { .. } => "manage",
        AppUp::Extensions { .. } => "extensions",
        AppUp::Action { .. } => "an extension's action",
    }
}

/// A shape name the host sends (`AppDown::Cursor`) as the pointer the app sets.
fn shape(name: &str) -> Option<Pointer> {
    Some(match name {
        "Arrow" => Pointer::Arrow,
        "Text" => Pointer::Text,
        "VerticalText" => Pointer::VerticalText,
        "Hand" => Pointer::Hand,
        "OpenHand" => Pointer::OpenHand,
        "Grabbing" => Pointer::Grabbing,
        "ResizeLeftRight" => Pointer::ResizeLeftRight,
        "ResizeLeft" => Pointer::ResizeLeft,
        "ResizeRight" => Pointer::ResizeRight,
        "ResizeUpDown" => Pointer::ResizeUpDown,
        "NotAllowed" => Pointer::NotAllowed,
        _ => return None,
    })
}

/// CEF's modifier bits for `m`, with the left button's while it is held.
fn bits(m: KeyMods, held: bool) -> u32 {
    let mut b = 0;
    for (on, bit) in [(m.shift, mods::SHIFT), (m.ctrl, mods::CONTROL), (m.alt, mods::ALT), (m.cmd, mods::COMMAND)] {
        if on {
            b |= bit;
        }
    }
    if held {
        b |= mods::LEFT_BUTTON;
    }
    b
}

/// A URL's host and path, for a card's second line.
fn short(url: &str) -> &str {
    let s = url.split_once("://").map_or(url, |(_, rest)| rest);
    s.strip_prefix("www.").unwrap_or(s).trim_end_matches('/')
}

/// What the address field's words go to: a URL as typed (a scheme, or a dotted host), else a search.
fn address(typed: &str) -> String {
    let t = typed.trim();
    if t.contains("://") || t.starts_with("about:") {
        t.to_owned()
    } else if !t.contains(' ') && t.contains('.') {
        format!("https://{t}")
    } else {
        let q: String = t
            .bytes()
            .map(|b| match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                b' ' => "+".into(),
                _ => format!("%{b:02X}"),
            })
            .collect();
        format!("https://www.google.com/search?q={q}")
    }
}

fn set_if<T: ComponentId + PartialEq + Clone>(e: EntityView<'_>, v: T) {
    if e.try_cloned::<&T>().as_ref() != Some(&v) {
        e.set(v);
    }
}

impl App {
    /// Shown into from `set_media`: Media holds the views' rings.
    pub(in crate::app) fn browser_media(&mut self, media: Rc<RefCell<Media>>) {
        self.browser.media = Some(media);
        self.browser.latch_set = false;
    }

    fn web(&self) -> WebState {
        self.world.world_ref().try_lookup(WEB).and_then(|e| e.try_cloned::<&WebState>()).unwrap_or_default()
    }

    fn set_web(&mut self, s: WebState) {
        if let Some(e) = self.world.world_ref().try_lookup(WEB) {
            set_if(e, s);
        }
        self.poke();
    }

    /// The pane's page node.
    fn web_page(&self) -> Option<u64> {
        View::of(self.world.entity_from_id(self.browser.pane)).part("page").map(|p| p.id())
    }

    /// cmd+shift+B: open the pane (on the view it last showed, else the newest, else a new blank one) or shut it.
    pub(super) fn toggle_web(&mut self) {
        let mut s = self.web();
        s.open = !s.open;
        if s.open {
            self.browser.note = None;
            if matches!(self.browser.conn, Conn::Failed(_)) {
                self.browser.conn = Conn::Idle;
            }
            if s.view == 0 || self.browser.closed.contains_key(&s.view) {
                s.view = self.browser.views.keys().copied().filter(|v| !self.browser.closed.contains_key(v)).max().unwrap_or(0);
            }
            self.set_web(s);
            self.web_focus_page();
        } else {
            self.web_shut(s);
        }
    }

    /// Shut the pane at `s` (its `open` cleared): the keyboard, the address and the extensions menu leave with it.
    fn web_shut(&mut self, s: WebState) {
        self.set_web(WebState { open: false, ..s });
        self.web_blur();
        self.browser.menu = false;
        self.browser.url = None;
    }

    /// Whether the pane shows: open, with its page's picture in (or a reason the harness browser gave for there being
    /// none: a refused link or open, a view it closed or whose frames it could not take). It never stands open on nothing:
    /// asked for before the page is made and has drawn, it opens on the page's first frame.
    pub(super) fn web_shown(&self) -> bool {
        let s = self.web();
        let b = &self.browser;
        if !s.open {
            return false;
        }
        if matches!(b.conn, Conn::Failed(_)) || b.note.is_some() {
            return true;
        }
        s.view != 0
            && !b.closed.contains_key(&s.view)
            && (b.errors.contains_key(&s.view) || b.media.as_ref().is_some_and(|m| m.borrow().browser_showing(s.view)))
    }

    /// Open `url` in the pane, as a view of its own (the pane shows it once the host made it; what the pane showed stays
    /// a view): the pane opens, the link is asked for if it is not up, and the next frame sends the open.
    fn web_open(&mut self, url: &str) {
        self.browser.note = None;
        if matches!(self.browser.conn, Conn::Failed(_)) {
            self.browser.conn = Conn::Idle;
        }
        self.browser.go = Some(url.to_owned());
        let s = self.web();
        self.set_web(WebState { open: true, ..s });
        self.web_focus_page();
    }

    /// Open what a press on a link names (an `open:` action, a chat's link or inline component: `<url>`, `<path>` or
    /// `<line>:<path>`) in the pane on the right, as a view of its own: a web page, or a file or folder as a `file://`
    /// page (a text file as fileview.rs's page of it: markdown rendered, its line marked). With Option held (`alt`) it opens outside instead, as crate::open says: a page in the system's browser, a
    /// file in the app its kind opens in, at its line.
    pub(crate) fn open_target(&mut self, target: &str, alt: bool) {
        if alt {
            return crate::open::open(target, false);
        }
        let (line, path) = match target.split_once(':') {
            Some((n, rest)) if rest.starts_with('/') && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => (n.parse::<u32>().ok(), rest),
            _ => (None, target),
        };
        if target.starts_with("https://") || target.starts_with("http://") || target.starts_with("file://") {
            self.web_open(target);
        } else if path.starts_with('/') {
            // A text file as a page of its own (fileview.rs): markdown rendered, the line it was opened at marked. Anything
            // else (a folder, a picture, a page) as the file itself.
            let shown = fileview::page(path, line);
            self.web_open(&file_url(shown.as_deref().and_then(|p| p.to_str()).unwrap_or(path)));
        } else {
            eprintln!("harness-native: open {target:?}: not a web link or an absolute path");
            return;
        }
        self.poke();
    }

    /// Open the pane on `view`, its page focused.
    fn web_show(&mut self, view: u64) {
        let s = self.web();
        self.set_web(WebState { view, open: true, ..s });
        self.web_focus_page();
    }

    fn web_focus_page(&mut self) {
        if let Some(p) = self.web_page() {
            self.browser_focus(Some(p));
        }
    }

    /// Give BrowserFocus to `node` (None: to nothing), telling the views that gain and lose the keyboard.
    fn browser_focus(&mut self, node: Option<u64>) {
        if self.browser.focus == node {
            return;
        }
        let old = self.browser.focus.take();
        let old_view = old.and_then(|n| self.node_view(self.world.entity_from_id(n)));
        let new_view = node.and_then(|n| self.node_view(self.world.entity_from_id(n)));
        if let Some(old) = old {
            let e = self.world.entity_from_id(old);
            if e.is_alive() && e.has(BrowserFocus::id()) {
                e.remove(BrowserFocus::id());
            }
        }
        if let Some(v) = old_view {
            self.browser.up(&AppUp::Focus { view: v, on: false });
        }
        if let Some(n) = node {
            self.world.entity_from_id(n).add(BrowserFocus::id());
            self.browser.focus = Some(n);
            if let Some(v) = new_view {
                self.browser.up(&AppUp::Focus { view: v, on: true });
            }
            // The keyboard leaves the tree and the panes.
            self.tree_focus = false;
        }
        self.browser.marked = None;
        self.poke();
    }

    fn web_blur(&mut self) {
        self.browser_focus(None);
    }

    /// Whether a browser view has the keyboard (ctx.Browser).
    pub(super) fn browser_focused(&self) -> bool {
        self.browser.focus.is_some() || self.browser.url.is_some()
    }

    /// cmd+L: edit the pane's address, starting from the page's.
    pub(super) fn web_focus_url(&mut self) {
        let s = self.web();
        if !s.open {
            self.set_web(WebState { open: true, ..s });
        }
        let now = self.browser.views.get(&s.view).map(|v| v.url.clone()).unwrap_or_default();
        self.browser.url = Some(if now == BLANK { String::new() } else { now });
        self.poke();
    }

    /// Back, forward, reload: the focused view's (the pane's when nothing else has the keys).
    pub(super) fn web_nav(&mut self, go: Nav) {
        let Some(view) = self.browser.focus.and_then(|n| self.node_view(self.world.entity_from_id(n))).or_else(|| {
            let v = self.web().view;
            (v != 0).then_some(v)
        }) else {
            return;
        };
        self.browser.up(&AppUp::Nav { view, go });
        self.poke();
    }

    /// The view a node (a ui.Browser) shows now: its own, or the one its `src` opened.
    fn node_view(&self, e: EntityView<'_>) -> Option<u64> {
        let bv = e.try_cloned::<&BrowserView>()?;
        if bv.view != 0 {
            return Some(bv.view);
        }
        self.browser.opened.get(bv.src.as_str()).copied()
    }

    /// A `web:` action (scene/widgets/web.flecs, a card's press).
    pub(super) fn web_action(&mut self, a: &str) {
        if let Some(v) = a.strip_prefix("show:").and_then(|v| v.parse().ok()) {
            self.web_show(v);
            return;
        }
        if let Some(url) = a.strip_prefix("open:") {
            self.web_open(url);
            self.poke();
            return;
        }
        if let Some(id) = a.strip_prefix("ext:") {
            let view = self.web().view;
            let req = self.browser.req();
            self.browser.opening.insert(req, Opening::Popup);
            self.browser.up(&AppUp::Action { req, view, extension: id.to_owned() });
            self.browser.menu = false;
            return;
        }
        match a {
            "back" => self.web_nav(Nav::Back),
            "forward" => self.web_nav(Nav::Forward),
            "reload" => self.web_nav(Nav::Reload),
            "url" => self.web_focus_url(),
            "close" => self.toggle_web(),
            "extensions" => {
                self.browser.menu = !self.browser.menu;
                if self.browser.menu {
                    let req = self.browser.req();
                    self.browser.ext_req = Some(req);
                    self.browser.extensions = None;
                    self.browser.up(&AppUp::Extensions { req });
                }
            }
            "manage" => {
                self.browser.menu = false;
                self.browser.up(&AppUp::Manage { url: None });
            }
            "store" => {
                self.browser.menu = false;
                self.browser.up(&AppUp::Manage { url: Some(STORE.into()) });
            }
            _ => eprintln!("harness-native: browser: no action web:{a}"),
        }
        self.poke();
    }

    // ---- each frame -----------------------------------------------------------------------------------------------

    /// Before the world's progress: the link (asked, answered, its messages handled), the pane's fold, and every
    /// ui.Browser node's picture key, pointer shape and plate (and a card's title, address and hairline).
    pub(super) fn browser_frame_start(&mut self, dt: f32) {
        let mut nodes: Vec<(u64, BrowserView)> = Vec::new(); // alloc: this frame's browser nodes
        self.world.new_query::<&BrowserView>().each_entity(|e, bv| nodes.push((*e.id(), bv.clone())));
        let wanted = self.web().open || !nodes.is_empty();
        if wanted && matches!(self.browser.conn, Conn::Idle) {
            self.browser.ask();
        }
        let answer = match &self.browser.conn {
            Conn::Asking(rx) => Some(rx.try_recv()),
            _ => None,
        };
        match answer {
            Some(Ok(Ok(service))) => {
                if let Err(e) = self.browser.connect(&service) {
                    self.browser.fail(format!("Not connected to the harness browser: {e}"));
                }
            }
            Some(Ok(Err(e))) => self.browser.fail(format!("Not connected to the harness browser: {e}")),
            Some(Err(TryRecvError::Disconnected)) => {
                self.browser.fail("Not connected to the harness browser: the thread asking the daemon ended".into())
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
        self.browser_latch_sink();
        self.browser_drain();
        self.browser_fold(dt);
        // The pane shows its view.
        let s = self.web();
        if let Some(p) = self.web_page() {
            set_if(self.world.entity_from_id(p), BrowserView { view: s.view, src: "".into() });
        }
        // A page asked for (`web:open:`) opens as a view of its own, the pane's once it is made; an open pane with no view
        // gets a blank one. A refused open stays refused (its plate says why) until the pane is opened again.
        if s.open
            && self.browser.live()
            && self.browser.note.is_none()
            && !self.browser.opening.values().any(|o| matches!(o, Opening::Pane))
        {
            let url = match self.browser.go.take() {
                Some(url) => Some(url),
                None if s.view == 0 => Some(BLANK.to_owned()),
                None => None,
            };
            if let Some(url) = url {
                let req = self.browser.req();
                self.browser.opening.insert(req, Opening::Pane);
                self.browser.up(&AppUp::Open { req, url });
            }
        }
        for (e, bv) in nodes {
            self.browser_node(e, &bv);
        }
        self.browser_pane();
    }

    /// Media's latch sink: sends `Latched` on the host's right (set once both exist).
    fn browser_latch_sink(&mut self) {
        if self.browser.latch_set {
            return;
        }
        let Some(media) = self.browser.media.clone() else { return };
        let (host, errors) = (self.browser.host.clone(), self.browser.latch_errors.clone());
        media.borrow_mut().set_browser_latch(Box::new(move |view, generation, slot| {
            if let Some(h) = host.borrow().as_ref()
                && let Err(e) = h.send(&cbor::encode(&AppUp::Latched { view, generation, slot }), &[])
            {
                errors.borrow_mut().push(format!("the harness browser's link broke sending latched for view {view}: {e}"));
            }
        }));
        self.browser.latch_set = true;
    }

    /// Handle what the host sent since the last frame.
    fn browser_drain(&mut self) {
        let (downs, broke) = {
            let mut i = self.browser.inbox.borrow_mut();
            (std::mem::take(&mut i.downs), i.broke.take())
        };
        let latch: Vec<String> = std::mem::take(&mut *self.browser.latch_errors.borrow_mut());
        if let Some(why) = broke.or_else(|| latch.into_iter().next()) {
            self.browser.fail(why);
        }
        if downs.is_empty() {
            return;
        }
        self.poke_why("browser");
        let media = self.browser.media.clone();
        for (d, ports) in downs {
            self.browser_down(d, ports, media.as_ref());
        }
    }

    fn browser_down(&mut self, d: AppDown, ports: Vec<SendRight>, media: Option<&Rc<RefCell<Media>>>) {
        let b = &mut self.browser;
        match d {
            AppDown::Hello { .. } => {}
            AppDown::Views { views } => {
                b.views = views.into_iter().map(|v| (v.view, v)).collect();
                b.listed = true;
            }
            AppDown::Ring { view, generation, w, h, slots } => {
                let Some(media) = media else { return };
                if ports.len() != usize::from(slots) {
                    b.errors.insert(
                        view,
                        format!("the harness browser sent view {view}'s ring generation {generation} with {} ports for {slots} slots", ports.len()),
                    );
                    return;
                }
                let surfaces: Result<Vec<IoSurface>, String> = ports
                    .iter()
                    .enumerate()
                    .map(|(i, p)| IoSurface::from_port(p).map_err(|e| format!("view {view}'s ring slot {i}: {e}")))
                    .collect();
                match surfaces.and_then(|s| media.borrow_mut().browser_ring(view, generation, w, h, s)) {
                    Ok(()) => {
                        b.errors.remove(&view);
                    }
                    Err(e) => {
                        b.errors.insert(view, format!("the harness browser's frames: {e}"));
                    }
                }
            }
            AppDown::Frame { view, generation, slot, seq } => {
                if let Some(media) = media
                    && let Err(e) = media.borrow_mut().browser_frame(view, generation, slot, seq)
                {
                    b.errors.insert(view, format!("the harness browser's frames: {e}"));
                }
            }
            AppDown::Cursor { view, shape: name } => match shape(&name) {
                Some(p) => {
                    b.cursors.insert(view, p);
                }
                None => eprintln!("harness-native: browser: view {view}'s pointer shape {name:?} is not one the app knows"),
            },
            AppDown::ImeRect { view, x, y, w, h } => {
                b.ime_rect.insert(view, [x, y, w, h]);
            }
            AppDown::Opened { req, view } => match b.opening.remove(&req) {
                Some(Opening::Src(src)) => {
                    b.opened.insert(src, view);
                }
                Some(Opening::Pane) => {
                    let s = self.web();
                    self.set_web(WebState { view, ..s });
                }
                Some(Opening::Popup) => b.popup = Some(view),
                None => {}
            },
            AppDown::Closed { view, why } => {
                b.closed.insert(view, why);
                b.watched.remove(&view);
                b.sized.remove(&view);
                if b.popup == Some(view) {
                    b.popup = None;
                }
                if let Some(m) = media {
                    m.borrow_mut().browser_forget(view);
                }
                // The pane never stands empty: it moves to the newest page left, or shuts when none is.
                let s = self.web();
                if s.view == view {
                    let next = self.browser.views.keys().copied().filter(|v| !self.browser.closed.contains_key(v)).max();
                    match next {
                        Some(next) => self.set_web(WebState { view: next, ..s }),
                        None => self.web_shut(WebState { view: 0, ..s }),
                    }
                }
            }
            AppDown::Extensions { req, extensions } => {
                if b.ext_req == Some(req) {
                    b.extensions = Some(extensions);
                }
            }
            AppDown::Error { req, error } => {
                eprintln!("harness-native: browser: the host refused: {error}");
                match req.and_then(|r| b.opening.remove(&r)) {
                    Some(Opening::Src(src)) => {
                        b.refused.insert(src, error);
                    }
                    _ => b.note = Some(error),
                }
            }
        }
    }

    /// Spring the pane's clip toward its width (shown) or 0 (shut, or first opened with no page yet), on the sidebar's
    /// spring; the pane keeps its full width.
    fn browser_fold(&mut self, dt: f32) {
        let mut moved = false;
        let full = self.web_width();
        // Out already and still open: it stays out while a page opened in it loads (the new view has no frame yet, so
        // `web_shown` is false for a moment), never folding away and coming out again.
        let stays = self.web().open && self.browser.x.x > 0.0;
        let target = f32::from(u8::from(self.web_shown() || stays));
        let w = self.world.world_ref();
        if !self.browser.x.settled(target) {
            let tune = crate::look::tune(w, "sidebar");
            if !settle(&mut self.browser.x, target, tune, dt) {
                self.browser.x = Spring::at(target);
            }
            moved = true;
        }
        let pane = w.entity_from_id(self.browser.pane);
        if let Some(mut z) = pane.try_cloned::<&Size>() {
            z.w = Sizing::fixed(full);
            set_if(pane, z);
        }
        let clip = w.entity_from_id(self.browser.clip);
        if let Some(mut z) = clip.try_cloned::<&Size>() {
            // The seam's hairline rides with it.
            z.w = Sizing::fixed(((full + 1.0) * self.browser.x.x.clamp(0.0, 1.0)).round());
            set_if(clip, z);
        }
        if moved {
            self.poke();
        }
    }

    /// Why a node shows no picture (empty: it has one).
    fn plate_words(&self, view: Option<u64>, src: &str) -> String {
        let b = &self.browser;
        if let Conn::Failed(why) = &b.conn {
            return why.clone();
        }
        let Some(view) = view else {
            if let Some(why) = b.refused.get(src) {
                return format!("The harness browser could not open {src}: {why}");
            }
            if let Some(why) = b.note.as_ref().filter(|_| src.is_empty()) {
                return format!("The harness browser refused: {why}");
            }
            // The pane has no page only while it folds away (it shuts when its last page goes): no words over it.
            return if src.is_empty() { String::new() } else { format!("Opening {}", short(src)) };
        };
        if let Some(why) = b.closed.get(&view) {
            return format!("The harness browser closed this page: {why}");
        }
        if let Some(why) = b.errors.get(&view) {
            return why.clone();
        }
        if !b.live() {
            return "Connecting to the harness browser".into();
        }
        if b.listed && !b.views.contains_key(&view) {
            return format!("The harness browser has no view {view}");
        }
        if b.media.as_ref().is_some_and(|m| m.borrow().browser_showing(view)) { String::new() } else { "Loading".into() }
    }

    /// Fill one ui.Browser node, and the card around it when it is one.
    fn browser_node(&mut self, node: u64, bv: &BrowserView) {
        let e = self.world.entity_from_id(node);
        let src = bv.src.as_str();
        let mut view = self.node_view(e);
        if view.is_none()
            && !src.is_empty()
            && self.browser.live()
            && !self.browser.refused.contains_key(src)
            && !self.browser.opening.values().any(|o| matches!(o, Opening::Src(s) if s == src))
        {
            let req = self.browser.req();
            self.browser.opening.insert(req, Opening::Src(src.to_owned()));
            self.browser.up(&AppUp::Open { req, url: src.to_owned() });
        }
        if view.is_some_and(|v| v == 0) {
            view = None;
        }
        let v = View::of(e);
        let pane = Some(*e.id()) == self.web_page();
        if let Some(pic) = v.part("pic") {
            set_if(pic.entity(), Surface { key: view.map_or(0, mb::key), cover: i32::from(!pane) });
            let shape = view.and_then(|v| self.browser.cursors.get(&v).copied()).unwrap_or(Pointer::Arrow);
            set_if(pic.entity(), Cursor { shape: if pane || e.has(BrowserFocus::id()) { shape } else { Pointer::Arrow } });
        }
        let words = self.plate_words(view, src);
        if let Some(t) = v.part("plate").and_then(|p| p.part("text")) {
            t.text(&words);
        }
        let info = view.and_then(|v| self.browser.views.get(&v));
        if pane {
            return;
        }
        // A card (hn.GenBrowser): its shape follows the page's, its press opens the pane on it.
        if let Some(i) = info.filter(|i| i.size[0] > 0 && i.size[1] > 0) {
            set_if(e, harness_native_ui_layout_api::Aspect { ratio: i.size[0] as f32 / i.size[1] as f32 });
        }
        let Some(card) = e.parent().map(View::of).filter(|c| c.part("meta").is_some()) else { return };
        if let Some(v) = view {
            card.action(&format!("web:show:{v}"));
        }
        let (title, url, loading) = info.map_or((String::new(), src.to_owned(), view.is_none() && !src.is_empty()), |i| {
            (if i.title.is_empty() { short(&i.url).to_owned() } else { i.title.clone() }, i.url.clone(), i.loading)
        });
        if let Some(m) = card.part("meta") {
            if let Some(t) = m.part("title") {
                t.text(&title);
            }
            if let Some(u) = m.part("url") {
                u.text(short(&url));
            }
        }
        if let Some(l) = card.part("load") {
            hairline(l.entity(), loading);
        }
    }

    /// The pane's toolbar, hairline, extensions menu and popup.
    fn browser_pane(&mut self) {
        let w = self.world.world_ref();
        let pane = View::of(w.entity_from_id(self.browser.pane));
        let s = self.web();
        let info = self.browser.views.get(&s.view).cloned();
        if let Some(bar) = pane.part("bar") {
            // The toolbar is out of the way: the page fills the pane, and the bar (back, forward, reload, the address)
            // comes only while the address is edited (cmd+L) or the extensions menu is open.
            let want = if self.browser.url.is_some() || self.browser.menu {
                view::prefab(self.world.world_ref(), "hn.WebPane").and_then(|p| View::of(p).part("bar")).and_then(|b| b.entity().try_cloned::<&Size>())
            } else {
                bar.entity().try_cloned::<&Size>().map(|s| Size { h: Sizing::fixed(0.0), ..s })
            };
            if let Some(want) = want
                && bar.entity().try_cloned::<&Size>() != Some(want)
            {
                bar.set(want);
            }
            for (part, glyph) in
                [("back", BACK_ICON), ("forward", FORWARD_ICON), ("reload", RELOAD_ICON), ("ext", EXT_ICON), ("close", CLOSE_ICON)]
            {
                if let Some(i) = bar.part(part).and_then(|b| b.part("icon")) {
                    i.icon(glyph);
                }
            }
            if let Some(t) = bar.part("url").and_then(|u| u.part("text")) {
                let shown = match &self.browser.url {
                    Some(draft) => format!("{draft}\u{258F}"),
                    None => info.as_ref().map(|i| i.url.clone()).filter(|u| u != BLANK).unwrap_or_default(),
                };
                t.text(&shown);
            }
        }
        if let Some(l) = pane.part("page").and_then(|p| p.part("load")) {
            hairline(l.entity(), info.as_ref().is_some_and(|i| i.loading));
        }
        let Some(holder) = pane.part("menu") else { return };
        let menu = self.browser.menu && s.open;
        let popup = self.browser.popup.filter(|_| s.open);
        let floats = menu.then_some(("plate", "hn.WebMenu", 0u64)).into_iter().chain(popup.map(|p| ("popup", "hn.WebPopup", p)));
        let ext = pane.part("bar").and_then(|b| b.part("ext")).map(|b| b.id());
        let bar = pane.part("bar").map(|b| b.id());
        let rows = self.web_menu_rows();
        let hovered: Vec<u64> = w
            .entity_named(APP)
            .target(Hovered::id(), 0)
            .map(|t| std::iter::successors(Some(t), |n| n.parent()).take(4).map(|n| *n.id()).collect())
            .unwrap_or_default();
        view::for_each_of(holder, floats, |f, view, _| {
            if view == 0 {
                if let Some(x) = ext {
                    f.anchored(x);
                }
                let keyed = rows.iter().enumerate().map(|(i, r)| (format!("r{i}"), r.0, r));
                view::for_each_of(f, keyed, |row, (_, label, about, action), _| {
                    if let Some(l) = row.part("label") {
                        l.text(label);
                    }
                    if let Some(a) = row.part("about") {
                        a.text(about);
                    }
                    if !action.is_empty() {
                        row.action(action);
                        view::menu::light(row.entity(), hovered.contains(&row.id()));
                    }
                });
            } else {
                if let Some(b) = bar {
                    f.anchored(b);
                }
                if let Some(p) = f.part("page") {
                    set_if(p.entity(), BrowserView { view, src: "".into() });
                }
            }
        });
    }

    /// The extensions menu's rows: (prefab, label, about, action).
    fn web_menu_rows(&self) -> Vec<(&'static str, String, String, String)> {
        let mut rows = Vec::new();
        match &self.browser.extensions {
            None => rows.push(("ui.MenuNote", "Reading the extensions".to_owned(), String::new(), String::new())),
            Some(list) if list.iter().all(|e| !e.enabled) => {
                rows.push(("ui.MenuNote", "No extensions installed".to_owned(), String::new(), String::new()));
            }
            Some(list) => {
                for x in list.iter().filter(|e| e.enabled) {
                    let (about, action) = match &x.popup {
                        Some(_) => (x.version.clone(), format!("web:ext:{}", x.id)),
                        None => ("No popup".to_owned(), String::new()),
                    };
                    rows.push((if action.is_empty() { "ui.MenuNote" } else { "ui.MenuItem" }, x.name.clone(), about, action));
                }
            }
        }
        rows.push(("ui.MenuSep", String::new(), String::new(), String::new()));
        rows.push(("ui.MenuItem", "Manage extensions\u{2026}".into(), String::new(), "web:manage".into()));
        rows.push(("ui.MenuItem", "Chrome Web Store\u{2026}".into(), String::new(), "web:store".into()));
        rows
    }

    /// After the paint: which views are on screen (their rings kept, watched; the rest unwatched) and the pane's box.
    pub(super) fn browser_frame_end(&mut self, d: &mut Draw, scale: f32) {
        if !self.browser.live() {
            return;
        }
        let w = self.world.world_ref();
        let pane_page = self.web_page();
        let open = self.web().open;
        let mut shown: BTreeSet<u64> = BTreeSet::new();
        let mut pane_box: Option<(u64, Laid)> = None;
        let mut nodes = Vec::new();
        w.new_query::<&BrowserView>().each_entity(|e, _| nodes.push(*e.id()));
        for n in nodes {
            let e = w.entity_from_id(n);
            let Some(view) = self.node_view(e).filter(|v| *v != 0) else { continue };
            let pane = Some(n) == pane_page;
            if d.surface_keys.contains(&mb::key(view)) || (pane && open) {
                shown.insert(view);
                if pane && let Some(l) = View::of(e).part("pic").and_then(|p| p.entity().try_cloned::<&Laid>()) {
                    pane_box = Some((view, l));
                }
            }
        }
        for &view in &shown {
            d.media.push(MediaSpec::Browser(BrowserSpec { view }));
        }
        let gone: Vec<u64> = self.browser.watched.difference(&shown).copied().collect();
        let new: Vec<u64> = shown.difference(&self.browser.watched).copied().collect();
        for view in gone {
            self.browser.watched.remove(&view);
            self.browser.up(&AppUp::Watch { view, on: false });
        }
        for view in new {
            self.browser.watched.insert(view);
            self.browser.up(&AppUp::Watch { view, on: true });
        }
        if let Some((view, l)) = pane_box.filter(|(_, l)| l.w >= 1.0 && l.h >= 1.0) {
            let at = (l.w.round() as u32, l.h.round() as u32, scale.to_bits());
            if self.browser.sized.get(&view) != Some(&at) {
                self.browser.sized.insert(view, at);
                self.browser.up(&AppUp::Resize { view, w: at.0, h: at.1, scale });
            }
        }
    }

    // ---- input ----------------------------------------------------------------------------------------------------

    /// The live browser node under the pointer (the pane's page, a popup's, or a node with BrowserFocus): the node, its
    /// view and its picture's box.
    fn browser_under(&self) -> Option<(u64, u64, Laid)> {
        let w = self.world.world_ref();
        let hit = w.entity_named(APP).target(Hovered::id(), 0)?;
        let node = std::iter::successors(Some(hit), |n| n.parent()).take(4).find(|n| n.has(BrowserView::id()))?;
        let id = *node.id();
        let live = Some(id) == self.web_page() || node.has(BrowserFocus::id()) || self.browser.popup.is_some_and(|p| self.node_view(node) == Some(p));
        if !live {
            return None;
        }
        let view = self.node_view(node).filter(|v| *v != 0)?;
        let l = View::of(node).part("pic")?.entity().try_cloned::<&Laid>()?;
        Some((id, view, l))
    }

    /// The box of the view `view`'s live node (for a grab, wherever the pointer is now).
    fn browser_box(&self, node: u64) -> Option<Laid> {
        View::of(self.world.entity_from_id(node)).part("pic")?.entity().try_cloned::<&Laid>()
    }

    fn browser_pointer(&mut self, view: u64, l: Laid, kind: PointerKind, button: u8, clicks: u8, m: KeyMods) {
        let (x, y) = (self.pointer.x - l.x, self.pointer.y - l.y);
        let held = self.browser.grab.is_some() && !matches!(kind, PointerKind::Up);
        let ev = hb::Pointer { kind, x, y, button, clicks, dx: 0.0, dy: 0.0, mods: bits(m, held) };
        self.browser.up(&AppUp::Pointer { view, ev });
    }

    /// The pointer moved: to the view a press holds, else the live view under it (the one it left hears Leave). True
    /// when a view took it.
    pub(super) fn browser_pointer_move(&mut self, m: KeyMods) -> bool {
        if let Some((node, view)) = self.browser.grab {
            if let Some(l) = self.browser_box(node) {
                self.browser_pointer(view, l, PointerKind::Move, 0, 0, m);
            }
            return true;
        }
        let under = self.browser_under();
        let now = under.map(|(n, v, _)| (n, v));
        if let Some((n, v)) = self.browser.over.filter(|o| Some(*o) != now) {
            if let Some(l) = self.browser_box(n) {
                self.browser_pointer(v, l, PointerKind::Leave, 0, 0, m);
            }
        }
        self.browser.over = now;
        let Some((_, view, l)) = under else { return false };
        self.browser_pointer(view, l, PointerKind::Move, 0, 0, m);
        true
    }

    /// A press: on a live view it focuses it and goes there (true); anywhere else the views lose the keyboard, the
    /// address field its edit, and the extensions menu closes unless the press is on it.
    pub(super) fn browser_pointer_down(&mut self, b: Button, clicks: isize, m: KeyMods) -> bool {
        let button = match b {
            Button::Left => 0,
            Button::Middle => 1,
            Button::Right => 2,
            _ => return false,
        };
        if let Some((node, view, l)) = self.browser_under() {
            self.browser.url = None;
            self.browser.menu = false;
            self.browser_focus(Some(node));
            self.browser.grab = Some((node, view));
            self.browser_pointer(view, l, PointerKind::Down, button, clicks.clamp(1, 255) as u8, m);
            return true;
        }
        let w = self.world.world_ref();
        let on = |part: &str| {
            let pane = View::of(w.entity_from_id(self.browser.pane));
            let Some(target) = pane.part(part).map(|p| p.id()) else { return false };
            w.entity_named(APP)
                .target(Hovered::id(), 0)
                .is_some_and(|h| std::iter::successors(Some(h), |n| n.parent()).take(12).any(|n| *n.id() == target))
        };
        let (on_menu, on_bar) = (on("menu"), on("bar"));
        if !on_menu && !on_bar {
            self.browser.menu = false;
            self.browser.url = None;
            if self.browser.focus.is_some() {
                self.web_blur();
            }
        }
        false
    }

    /// A release: to the view the press went to (true).
    pub(super) fn browser_pointer_up(&mut self, b: Button, m: KeyMods) -> bool {
        let Some((node, view)) = self.browser.grab.take() else { return false };
        let button = match b {
            Button::Middle => 1,
            Button::Right => 2,
            _ => 0,
        };
        if let Some(l) = self.browser_box(node) {
            let clicks = self.press.0.clamp(1, 255) as u8;
            self.browser_pointer(view, l, PointerKind::Up, button, clicks, m);
        }
        true
    }

    /// A scroll over a live view: to the page, in points, positive down and right as the page scrolls (true).
    pub(super) fn browser_wheel(&mut self, ev: &Wheel, m: KeyMods) -> bool {
        let Some((_, view, l)) = self.browser_under() else { return false };
        let z = self.zoom();
        // AppKit's deltas are positive when the content moves down (toward the top of the page); lines are 40 points.
        let k = if ev.precise { 1.0 / z } else { 40.0 };
        let (x, y) = (self.pointer.x - l.x, self.pointer.y - l.y);
        let ev = hb::Pointer { kind: PointerKind::Wheel, x, y, button: 0, clicks: 0, dx: -ev.dx * k, dy: -ev.dy * k, mods: bits(m, false) };
        self.browser.up(&AppUp::Pointer { view, ev });
        true
    }

    /// The view with the keyboard, when its node is alive.
    fn focused_view(&self) -> Option<u64> {
        let n = self.browser.focus?;
        let e = self.world.entity_from_id(n);
        if !e.is_alive() {
            return None;
        }
        self.node_view(e).filter(|v| *v != 0)
    }

    /// A key no binding took: into the address field while it is edited, else to the focused view (true).
    pub(super) fn browser_key(&mut self, k: &term::KeyIn<'_>) -> bool {
        if let Some(draft) = self.browser.url.as_mut() {
            if matches!(k.action, KeyAction::Release) || k.mods.cmd || k.mods.ctrl {
                return true;
            }
            match k.code {
                KEY_RETURN => {
                    let url = address(draft);
                    self.browser.url = None;
                    if !url.is_empty() {
                        let view = self.web().view;
                        if view != 0 {
                            self.browser.up(&AppUp::Nav { view, go: Nav::Go { url } });
                        }
                    }
                    self.web_focus_page();
                }
                KEY_ESCAPE => {
                    self.browser.url = None;
                    self.web_focus_page();
                }
                KEY_BACKSPACE => {
                    draft.pop();
                }
                _ => draft.push_str(k.text),
            }
            self.poke();
            return true;
        }
        let Some(view) = self.focused_view() else { return false };
        if k.composing {
            return true;
        }
        let mods = bits(k.mods, false) | if matches!(k.action, KeyAction::Repeat) { mods::IS_REPEAT } else { 0 };
        let unmod = k.unshifted.map(String::from).unwrap_or_default();
        let release = matches!(k.action, KeyAction::Release);
        let kind = if release { KeyKind::Up } else { KeyKind::Down };
        self.browser.up(&AppUp::Key { view, ev: hb::Key { kind, code: k.code, chars: k.text.to_owned(), unmod: unmod.clone(), mods } });
        // What the key typed, as CEF's CHAR events (not for Command or Control chords: those are commands).
        if !release && !k.text.is_empty() && !k.mods.cmd && !k.mods.ctrl {
            self.browser.up(&AppUp::Key { view, ev: hb::Key { kind: KeyKind::Char, code: k.code, chars: k.text.to_owned(), unmod, mods } });
        }
        true
    }

    /// An edit command (Command-A, -C, -X, -V from the menu or its key) for the focused view: sent as its keys, so the page
    /// does it with its own pasteboard (true).
    pub(super) fn browser_edit(&mut self, code: u16) -> bool {
        let Some(view) = self.focused_view() else { return false };
        let c = match code {
            KEY_A => "a",
            KEY_X => "x",
            KEY_C => "c",
            KEY_V => "v",
            _ => "",
        };
        let mods = mods::COMMAND;
        for kind in [KeyKind::Down, KeyKind::Up] {
            self.browser.up(&AppUp::Key { view, ev: hb::Key { kind, code, chars: c.into(), unmod: c.into(), mods } });
        }
        true
    }

    pub(super) fn browser_copy(&mut self) -> bool {
        self.browser_edit(KEY_C)
    }

    pub(super) fn browser_cut(&mut self) -> bool {
        self.browser_edit(KEY_X)
    }

    pub(super) fn browser_select_all(&mut self) -> bool {
        self.browser_edit(KEY_A)
    }

    /// Text pasted: into the address field while it is edited (its first line), else the focused page pastes its own.
    pub(super) fn browser_paste(&mut self, text: &str) -> bool {
        if let Some(draft) = self.browser.url.as_mut() {
            draft.push_str(text.lines().next().unwrap_or(""));
            self.poke();
            return true;
        }
        self.browser_edit(KEY_V)
    }

    /// Text an input method, the emoji picker or dictation committed (insertText:): the focused view's (true).
    pub(super) fn browser_commit(&mut self, text: &str) -> bool {
        if let Some(draft) = self.browser.url.as_mut() {
            draft.push_str(text);
            self.poke();
            return true;
        }
        let Some(view) = self.focused_view() else { return false };
        self.browser.marked = None;
        self.browser.up(&AppUp::Ime { view, ev: Ime::Commit { text: text.to_owned(), replace: None } });
        self.poke();
        true
    }

    /// The input method's marked text (setMarkedText:, unmarkText): the focused view's composition (true).
    pub(super) fn browser_set_marked(&mut self, text: Option<&str>) -> bool {
        if self.browser.url.is_some() {
            return true;
        }
        let Some(view) = self.focused_view() else { return false };
        let ev = match text {
            Some(s) if !s.is_empty() => {
                let n = s.encode_utf16().count() as u32;
                self.browser.marked = Some(s.to_owned());
                Ime::Set { text: s.to_owned(), sel: [n, n], replace: None }
            }
            Some(_) => {
                self.browser.marked = None;
                Ime::Cancel
            }
            None => {
                if self.browser.marked.take().is_none() {
                    return true;
                }
                Ime::Finish
            }
        };
        self.browser.up(&AppUp::Ime { view, ev });
        self.poke();
        true
    }

    /// The focused view's marked text (Some: a view has the keyboard).
    pub(super) fn browser_marked(&self) -> Option<Option<String>> {
        self.focused_view()?;
        Some(self.browser.marked.clone())
    }

    /// Where the input method's candidate window goes (window points): the focused view's composition, as its host said.
    pub(super) fn browser_caret_rect(&self) -> Option<Option<[f32; 4]>> {
        let view = self.focused_view()?;
        let l = self.browser_box(self.browser.focus?)?;
        let z = self.zoom();
        Some(self.browser.ime_rect.get(&view).map(|r| [(l.x + r[0]) * z, (l.y + r[1]) * z, r[2] * z, r[3] * z]))
    }
}

/// A loading hairline: across its row while `on`, else none.
fn hairline(e: EntityView<'_>, on: bool) {
    if let Some(mut z) = e.try_cloned::<&Size>() {
        z.w = if on { Sizing::GROW } else { Sizing::fixed(0.0) };
        set_if(e, z);
    }
}
