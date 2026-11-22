//! The host's state, owned by CEF's UI thread (the main thread): every view, every socket connection, the app's port,
//! the DevTools id table. Other threads reach it only through [`post`] (a command on a crossbeam channel plus one CEF
//! task that drains it), so nothing here is shared.
//!
//! The one re-entrancy rule: CEF calls back synchronously (creating a browser asks for its view rect and screen info,
//! closing one may run `OnBeforeClose`, a DevTools message may be answered before the send returns), so no CEF call is
//! ever made inside [`with`]. Each step takes what it needs out of the host (a `Browser`, a `BrowserHost`, a message to
//! send), lets go, and then calls CEF.
use crate::cdp_ids::{Asker, CdpIds, HostAsk, Incoming};
use crate::ring::{Copy as Blit, Frames, Gpu, Ring, SLOTS};
use crate::slots::After;
use crate::ui::Origin;
use crate::{handlers, kernel, manage};
use cef::*;
use crossbeam_channel::{Receiver, Sender};
use harness_mach::{SendError, SendRight, Source};
use harness_proto::browser::{AppDown, KernelDown, KernelUp, ViewInfo};
use harness_proto::cbor;
use objc2_metal::MTLTexture;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// A view's size when the opener names none, in points.
pub const DEFAULT_SIZE: [u32; 2] = [1280, 800];
/// The scale a view renders at until the app says where it draws it (`AppUp::Resize`): CSS pixels, as headless
/// Chromium screenshots them.
pub const DEFAULT_SCALE: f32 = 1.0;
/// Frames per second of a view nobody watches (agents still screenshot it over CDP) and of one the app draws.
pub const FPS_IDLE: i32 = 10;
pub const FPS_WATCHED: i32 = 60;
/// How long views get to close on quit before the loop stops anyway.
pub const QUIT_GRACE_MS: i64 = 5_000;

// ---------------------------------------------------------------- configuration

/// What the host was started with.
pub struct Config {
    /// `HARNESS_HOME`, exactly as the daemon passed it (the Mach service name is derived from these bytes).
    pub home: PathBuf,
    /// `$HARNESS_HOME/browser`.
    pub dir: PathBuf,
    /// `HARNESS_DAEMON_PID`.
    pub daemon_pid: i32,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Set the configuration (main, before `cef_initialize`).
///
/// # Errors
/// It was set already.
pub fn configure(cfg: Config) -> Result<(), String> {
    CONFIG.set(cfg).map_err(|_| "the host's configuration was set twice".to_owned())
}

/// The configuration.
///
/// # Panics
/// Before [`configure`] (main sets it before CEF can call anything that asks).
pub fn config() -> &'static Config {
    CONFIG.get().expect("main configures the host before cef_initialize")
}

// ---------------------------------------------------------------- the command queue

/// What other threads (and CEF callbacks that must not act in place) ask of the UI thread.
pub enum Cmd {
    /// A socket client said hello: its frames go to `tx`.
    Connected { conn: u64, tx: Sender<Vec<u8>> },
    Kernel { conn: u64, msg: KernelUp },
    /// A socket client is gone.
    Dropped { conn: u64, why: String },
    /// A page asked for a new window (window.open, target=_blank): open it as a view of its own.
    OpenPopup { opener: u64, url: String },
    /// The app's port died.
    AppGone,
    /// A location request's bound passed (location.rs): answer it if nothing has.
    LocationWaited { conn: u64, req: u64 },
    Quit { why: String },
}

static QUEUE: OnceLock<(Sender<Cmd>, Receiver<Cmd>)> = OnceLock::new();
/// The message loop was told to stop: nothing more is posted.
static STOPPING: AtomicBool = AtomicBool::new(false);

fn queue() -> &'static (Sender<Cmd>, Receiver<Cmd>) {
    QUEUE.get_or_init(crossbeam_channel::unbounded)
}

/// Ask the UI thread to run `cmd`. False once the host is stopping (the caller's thread should end).
pub fn post(cmd: Cmd) -> bool {
    if STOPPING.load(Ordering::Acquire) {
        return false;
    }
    if queue().0.send(cmd).is_err() {
        return false;
    }
    // One drain task per command, so every command has the task that runs it.
    let mut task = handlers::drain_task();
    post_task(ThreadId::UI, Some(&mut task)) == 1
}

/// Run one queued command (the drain task, on the UI thread).
pub fn drain() {
    if let Ok(cmd) = queue().1.try_recv() {
        handle(cmd);
    }
}

/// Stop the message loop (once).
pub fn stop_loop(why: &str) {
    if !STOPPING.swap(true, Ordering::AcqRel) {
        eprintln!("harness-browser: stopping: {why}");
        quit_message_loop();
    }
}

// ---------------------------------------------------------------- the state

struct Conn {
    tx: Sender<Vec<u8>>,
}

/// Who waits for a view's `Opened`.
#[derive(Clone, Copy)]
enum Waiter {
    Kernel { conn: u64, req: u64 },
    App { req: u64 },
}

/// One view (named Page so it never reads as `cef::View`).
pub struct Page {
    browser: Option<Browser>,
    /// Keeps the DevTools observer registered; dropped with the page.
    _observer: Option<Registration>,
    info: ViewInfo,
    /// The connection that opened it: closing that connection closes the view, unless `keep`.
    opener: Option<u64>,
    keep: bool,
    /// The connections its CDP events go to.
    attached: BTreeSet<u64>,
    waiters: Vec<Waiter>,
    watched: bool,
    frames: Frames,
    /// Why it is closing, once something closed it.
    closing: Option<String>,
    /// A UI view's lock and first url (README.md, "UI views"); None for every other view.
    ui: Option<UiPage>,
}

/// What a UI view keeps: the url it was opened at (loaded once its binding is in, named in its errors) and that url's
/// origin, which it never leaves.
struct UiPage {
    url: String,
    origin: Origin,
    /// The host has loaded `url` (its binding is in): until then nothing may navigate the view.
    loaded: bool,
}

/// The binding a UI view's documents get: `window.harnessPost(string)`.
pub const BINDING: &str = "harnessPost";

pub struct Host {
    gpu: Gpu,
    conns: BTreeMap<u64, Conn>,
    pages: BTreeMap<u64, Page>,
    next_view: u64,
    cdp: CdpIds,
    app: Option<SendRight>,
    /// The Mach service's receive source (dropping it cancels it).
    _source: Option<Source>,
    quitting: Option<String>,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

/// Run `f` on the host. None before start or after teardown, or when called from inside another step (a CEF call
/// made while the host was borrowed, which breaks the rule in the module doc): that is reported, naming `what`.
pub fn with<R>(what: &str, f: impl FnOnce(&mut Host) -> R) -> Option<R> {
    HOST.with(|h| {
        let Ok(mut g) = h.try_borrow_mut() else {
            eprintln!("harness-browser: {what} ran inside another host step (a CEF call made while the host was borrowed); it was not handled");
            return None;
        };
        g.as_mut().map(f)
    })
}

// ---------------------------------------------------------------- sending

/// Send `msg` to connection `conn`, from a step that is not the host's own (location.rs: Core Location's callbacks).
pub fn answer(conn: u64, msg: &KernelDown) {
    with("an answer to a connection", |h| h.to_conn(conn, msg));
}

fn cdp_error(id: &Value, message: &str) -> String {
    serde_json::json!({ "id": id, "error": { "code": -32000, "message": message } }).to_string()
}

/// Send to the app. A dead port clears it (and posts [`Cmd::AppGone`] for the watches it had).
fn send_app(app: &mut Option<SendRight>, msg: &AppDown, ports: &[&SendRight]) -> Result<(), SendError> {
    let Some(port) = app.as_ref() else { return Err(SendError::Other("no app is linked".into())) };
    let r = port.send(&cbor::encode(msg), ports);
    if r == Err(SendError::Dead) {
        *app = None;
        post(Cmd::AppGone);
    }
    r
}

impl Host {
    fn to_conn(&self, conn: u64, msg: &KernelDown) {
        if let Some(c) = self.conns.get(&conn) {
            // A connection whose writer ended is dropping; its Dropped command follows.
            let _ = c.tx.send(kernel::frame(msg));
        }
    }

    fn to_all(&self, msg: &KernelDown) {
        let f = kernel::frame(msg);
        for c in self.conns.values() {
            let _ = c.tx.send(f.clone());
        }
    }

    /// Send a message (no ports) to the app if one is linked; a failure other than "none linked" is reported.
    fn to_app(&mut self, msg: &AppDown) {
        if self.app.is_none() {
            return;
        }
        if let Err(e) = send_app(&mut self.app, msg, &[]) {
            eprintln!("harness-browser: sending {} to the app: {e}", app_kind(msg));
        }
    }

    fn app_error(&mut self, req: Option<u64>, error: String) {
        eprintln!("harness-browser: app request refused: {error}");
        self.to_app(&AppDown::Error { req, error });
    }

    fn views(&self) -> Vec<ViewInfo> {
        self.pages.values().map(|p| p.info.clone()).collect() // alloc: one list per change, sent at once
    }

    /// The views a socket client sees: all but the UI views, which are the app's alone.
    fn kernel_views(&self) -> Vec<ViewInfo> {
        self.pages.values().filter(|p| p.ui.is_none()).map(|p| p.info.clone()).collect() // alloc: one list per request
    }

    /// Why connection `conn` may not `act` on view `view`: it is a UI view (None: it may, or there is no such view).
    fn kernel_refused(&self, conn: u64, act: &str, view: u64) -> Option<String> {
        let ui = self.pages.get(&view)?.ui.as_ref()?;
        Some(format!("connection {conn} cannot {act} view {view} ({}): it is a UI view, the app's own page", ui.url))
    }

    fn views_changed(&mut self) {
        let views = self.views();
        self.to_app(&AppDown::Views { views });
    }

    fn answer(&mut self, w: Waiter, r: Result<ViewInfo, String>) {
        match (w, r) {
            (Waiter::Kernel { conn, req }, Ok(view)) => self.to_conn(conn, &KernelDown::Opened { req, view }),
            (Waiter::Kernel { conn, req }, Err(error)) => self.to_conn(conn, &KernelDown::Error { req: Some(req), error }),
            (Waiter::App { req }, Ok(v)) => self.to_app(&AppDown::Opened { req, view: v.view }),
            (Waiter::App { req }, Err(error)) => self.app_error(Some(req), error),
        }
    }

    /// The page's browser host, cloned out to call outside the borrow.
    fn browser_host(&self, view: u64) -> Result<BrowserHost, String> {
        let p = self.pages.get(&view).ok_or_else(|| format!("there is no view {view}"))?;
        let b = p.browser.as_ref().ok_or_else(|| format!("view {view} has no browser yet"))?;
        b.host().ok_or_else(|| format!("view {view}'s browser has no host"))
    }

    fn browser(&self, view: u64) -> Result<Browser, String> {
        let p = self.pages.get(&view).ok_or_else(|| format!("there is no view {view}"))?;
        p.browser.clone().ok_or_else(|| format!("view {view} has no browser yet"))
    }
}

fn app_kind(msg: &AppDown) -> &'static str {
    match msg {
        AppDown::Hello { .. } => "hello",
        AppDown::Views { .. } => "views",
        AppDown::Ring { .. } => "a ring",
        AppDown::Frame { .. } => "a frame",
        AppDown::Cursor { .. } => "a cursor",
        AppDown::ImeRect { .. } => "an IME rect",
        AppDown::Opened { .. } => "opened",
        AppDown::Closed { .. } => "closed",
        AppDown::Extensions { .. } => "extensions",
        AppDown::Message { .. } => "a UI view's message",
        AppDown::Link { .. } => "a UI view's link",
        AppDown::Error { .. } => "an error",
    }
}

/// The browser host of `view`, or the error naming it (taken out of the host; call CEF on it afterwards).
pub fn host_of(view: u64) -> Result<BrowserHost, String> {
    with("looking up a view", |h| h.browser_host(view)).unwrap_or_else(|| Err(format!("view {view}: the host is not running")))
}

/// The browser of `view` (as [`host_of`]).
pub fn browser_of(view: u64) -> Result<Browser, String> {
    with("looking up a view", |h| h.browser(view)).unwrap_or_else(|| Err(format!("view {view}: the host is not running")))
}

// ---------------------------------------------------------------- start and teardown

/// Start serving (CEF's `OnContextInitialized`): the socket, the Mach service, the daemon watch, then the ready line.
/// Any failure prints `harness-browser failed: <why>` on stdout (the daemon reads it) and exits 1.
pub fn start() {
    if let Err(e) = try_start() {
        println!("harness-browser failed: {e}");
        eprintln!("harness-browser failed: {e}");
        std::process::exit(1);
    }
    use std::io::Write;
    println!("harness-browser ready pid={}", std::process::id());
    let _ = std::io::stdout().flush();
}

fn try_start() -> Result<(), String> {
    let cfg = config();
    let gpu = Gpu::new()?;
    let listener = kernel::bind(&cfg.dir)?;
    HOST.with(|h| {
        *h.borrow_mut() = Some(Host {
            gpu,
            conns: BTreeMap::new(),
            pages: BTreeMap::new(),
            next_view: 1,
            cdp: CdpIds::new(),
            app: None,
            _source: None,
            quitting: None,
        });
    });
    kernel::serve(listener)?;
    let service = harness_proto::browser::service_name(&cfg.home);
    let port = harness_mach::RecvPort::check_in(&service).map_err(|e| format!("checking in the Mach service {service}: {e}"))?;
    let source = Source::on_main_queue(port, Box::new(crate::app_link::on_msg)).map_err(|e| format!("draining the Mach service {service}: {e}"))?; // alloc: the source's handler, for its life
    with("start", |h| h._source = Some(source)).ok_or("the host was borrowed during start")?;
    crate::parent::watch(cfg.daemon_pid)
}

/// After the message loop: drop every view and the Mach source, and remove the socket.
pub fn teardown() {
    let host = HOST.with(|h| h.borrow_mut().take());
    if let Some(h) = &host
        && !h.pages.is_empty()
    {
        eprintln!("harness-browser: {} view(s) still open at teardown: {:?}", h.pages.len(), h.pages.keys().collect::<Vec<_>>());
    }
    drop(host);
    let sock = config().dir.join(harness_proto::browser::SOCK);
    if let Err(e) = std::fs::remove_file(&sock)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("harness-browser: removing {}: {e}", sock.display());
    }
}

// ---------------------------------------------------------------- commands

fn handle(cmd: Cmd) {
    match cmd {
        Cmd::Connected { conn, tx } => {
            with("a connection's hello", |h| {
                h.conns.insert(conn, Conn { tx });
            });
        }
        Cmd::Kernel { conn, msg } => kernel_msg(conn, msg),
        Cmd::Dropped { conn, why } => dropped(conn, &why),
        Cmd::OpenPopup { opener, url } => open_popup(opener, url),
        Cmd::AppGone => app_gone(),
        Cmd::LocationWaited { conn, req } => crate::location::waited(conn, req),
        Cmd::Quit { why } => quit(why),
    }
}

/// What opens a view.
pub struct OpenSpec {
    pub url: String,
    pub owner: Option<String>,
    pub size: [u32; 2],
    pub opener: Option<u64>,
    pub keep: bool,
    pub attached: BTreeSet<u64>,
    pub waiter: Option<WaiterSpec>,
    /// A UI view (the app's `AppUp::Open { ui: true }`).
    pub ui: bool,
}

/// Who an opened view answers.
pub enum WaiterSpec {
    Kernel { conn: u64, req: u64 },
    App { req: u64 },
}

/// Open a view: the page goes in the table first (CEF asks for its rect while it creates the browser), then the
/// browser, its DevTools observer, and the `Target.getTargetInfo` whose answer gives the waiter its `Opened`.
///
/// A UI view's browser is made at its url, and its DevTools session is sent `Runtime.enable` and
/// `Runtime.addBinding { name: "harnessPost" }` at once; the binding's answer loads its url again ([`devtools`]). So
/// the binding is in that document before any of its script runs. The first load may have no binding: the page draws
/// nothing without it (crates/harness/web), and the reload replaces it.
///
/// # Errors
/// CEF refused the browser or the observer, naming the view and the url; a UI view at a url that is not `file://`,
/// `http://` or `https://`. The waiter is not answered (the caller answers the error).
pub fn open(spec: OpenSpec) -> Result<u64, String> {
    if spec.size[0] == 0 || spec.size[1] == 0 {
        return Err(format!("a view {}x{} points at {} is empty", spec.size[0], spec.size[1], spec.url));
    }
    let ui = if spec.ui { Some(UiPage { url: spec.url.clone(), origin: Origin::of(&spec.url)?, loaded: false }) } else { None };
    let url = spec.url.clone();
    let view = with("opening a view", |h| {
        if let Some(why) = &h.quitting {
            return Err(format!("the host is quitting ({why}): no view opens for {url}"));
        }
        let view = h.next_view;
        h.next_view += 1;
        let waiters = match spec.waiter {
            Some(WaiterSpec::Kernel { conn, req }) => vec![Waiter::Kernel { conn, req }], // alloc: per view
            Some(WaiterSpec::App { req }) => vec![Waiter::App { req }],                   // alloc: per view
            None => Vec::new(),
        };
        h.pages.insert(
            view,
            Page {
                browser: None,
                _observer: None,
                info: ViewInfo {
                    view,
                    url: spec.url.clone(),
                    owner: spec.owner.clone(),
                    size: spec.size,
                    scale: DEFAULT_SCALE,
                    loading: true,
                    ui: ui.is_some(),
                    ..Default::default()
                },
                opener: spec.opener,
                keep: spec.keep,
                attached: spec.attached,
                waiters,
                watched: false,
                frames: Frames::default(),
                closing: None,
                ui,
            },
        );
        Ok(view)
    })
    .unwrap_or_else(|| Err(format!("opening {url}: the host is not running")))?;
    // A UI view starts at its page too: a browser made with no url has no renderer to answer the Runtime domain, and
    // one made on about:blank turns the load of a file:// page into a new window. Its binding comes with the reload
    // once the binding is in (the function's doc); the page draws nothing until it has the binding.
    let first = url.as_str();

    let fail = |e: String| {
        with("a view that failed to open", |h| h.pages.remove(&view));
        Err(e)
    };
    let info = WindowInfo { windowless_rendering_enabled: 1, shared_texture_enabled: 1, runtime_style: RuntimeStyle::ALLOY, ..Default::default() };
    let settings = BrowserSettings { windowless_frame_rate: FPS_IDLE, ..Default::default() };
    let mut client = handlers::view_client(view);
    let Some(browser) = browser_host_create_browser_sync(Some(&info), Some(&mut client), Some(&CefString::from(first)), Some(&settings), None, None) else {
        return fail(format!("CEF refused to create view {view}'s browser at {url} (cef.log says why)"));
    };
    let Some(bhost) = browser.host() else {
        return fail(format!("view {view}'s new browser at {url} has no host"));
    };
    let mut observer = handlers::devtools_observer(view);
    let Some(reg) = bhost.add_dev_tools_message_observer(Some(&mut observer)) else {
        bhost.close_browser(1);
        return fail(format!("CEF refused a DevTools observer on view {view} ({url})"));
    };
    // The host's own requests, in the order the session runs them: a UI view's Runtime.enable (bindingCalled is a
    // Runtime event) and its binding first, then the target id every view's opener waits for.
    let asks = with("a new view's own DevTools requests", |h| {
        let page = h.pages.get_mut(&view)?;
        page.browser = Some(browser.clone());
        page._observer = Some(reg);
        let mut asks = Vec::with_capacity(3); // alloc: per view, at most three
        if spec.ui {
            asks.push(("Runtime.enable", h.cdp.host_request(view, HostAsk::RuntimeEnable, "Runtime.enable", serde_json::json!({}))));
            asks.push(("Runtime.addBinding", h.cdp.host_request(view, HostAsk::AddBinding, "Runtime.addBinding", serde_json::json!({ "name": BINDING }))));
        }
        asks.push(("Target.getTargetInfo", h.cdp.host_request(view, HostAsk::TargetInfo, "Target.getTargetInfo", serde_json::json!({}))));
        Some(asks)
    })
    .flatten();
    // Closed while it was being made (its OnBeforeClose already took the page).
    let Some(asks) = asks else { return Err(format!("view {view} ({url}) closed while it was being opened")) };
    for (method, ask) in asks {
        let ask = match ask {
            Ok(ask) => ask,
            Err(e) => {
                close(view, e.clone());
                return Err(e);
            }
        };
        if bhost.send_dev_tools_message(Some(ask.as_bytes())) != 1 {
            close(view, format!("CEF refused the host's {method} for view {view} ({url})"));
            return Err(format!("CEF refused to send {method} to view {view} ({url})"));
        }
    }
    with("a new view", |h| h.views_changed());
    Ok(view)
}

/// Close `view` (its `OnBeforeClose` takes it out of the table and tells everyone, with `why`).
pub fn close(view: u64, why: String) {
    let b = with("closing a view", |h| {
        let page = h.pages.get_mut(&view)?;
        page.closing.get_or_insert(why);
        page.browser.clone()
    })
    .flatten();
    if let Some(host) = b.and_then(|b| b.host()) {
        // Forced: an off-screen view has nowhere to show a beforeunload prompt.
        host.close_browser(1);
    }
}

fn kernel_msg(conn: u64, msg: KernelUp) {
    match msg {
        KernelUp::Hello { .. } => with("a second hello", |h| {
            h.to_conn(conn, &KernelDown::Error { req: None, error: format!("connection {conn} sent hello twice") });
        })
        .unwrap_or(()),
        KernelUp::Open { req, url, owner, size, keep } => {
            let spec = OpenSpec {
                url,
                owner,
                size: size.unwrap_or(DEFAULT_SIZE),
                opener: Some(conn),
                keep,
                attached: BTreeSet::from([conn]),
                waiter: Some(WaiterSpec::Kernel { conn, req }),
                ui: false,
            };
            if let Err(error) = open(spec) {
                with("an open refused", |h| h.to_conn(conn, &KernelDown::Error { req: Some(req), error }));
            }
        }
        KernelUp::Attach { req, view } => {
            with("attach", |h| {
                if let Some(error) = h.kernel_refused(conn, "attach to", view) {
                    return h.to_conn(conn, &KernelDown::Error { req: Some(req), error });
                }
                let Some(p) = h.pages.get_mut(&view) else {
                    return h.to_conn(conn, &KernelDown::Error { req: Some(req), error: format!("connection {conn} cannot attach to view {view}: there is no such view") });
                };
                p.attached.insert(conn);
                if p.info.target.is_empty() {
                    p.waiters.push(Waiter::Kernel { conn, req });
                } else {
                    let info = p.info.clone();
                    h.to_conn(conn, &KernelDown::Opened { req, view: info });
                }
            });
        }
        KernelUp::Cdp { view, message } => cdp(conn, view, &message),
        KernelUp::Close { view } => {
            let refused = with("close", |h| {
                let error = match h.kernel_refused(conn, "close", view) {
                    Some(e) => e,
                    None if h.pages.contains_key(&view) => return false,
                    None => format!("connection {conn} closed view {view}, which does not exist"),
                };
                h.to_conn(conn, &KernelDown::Error { req: None, error });
                true
            })
            .unwrap_or(true);
            if !refused {
                close(view, format!("connection {conn} closed it"));
            }
        }
        KernelUp::Views { req } => {
            with("views", |h| {
                let views = h.kernel_views();
                h.to_conn(conn, &KernelDown::Views { req, views });
            });
        }
        KernelUp::Extensions { req } => {
            let r = crate::extensions::list(&config().dir);
            with("extensions", |h| match r {
                Ok(extensions) => h.to_conn(conn, &KernelDown::Extensions { req, extensions }),
                Err(error) => h.to_conn(conn, &KernelDown::Error { req: Some(req), error }),
            });
        }
        KernelUp::Location { req } => crate::location::request(conn, req),
        KernelUp::Places { req, query, latitude, longitude, radius_m, limit } => crate::places::request(conn, req, &query, latitude, longitude, radius_m, limit),
        KernelUp::Manage { req, url } => {
            let r = manage::open(url.as_deref());
            with("manage", |h| match r {
                Ok(()) => h.to_conn(conn, &KernelDown::Done { req }),
                Err(error) => h.to_conn(conn, &KernelDown::Error { req: Some(req), error }),
            });
        }
    }
}

fn cdp(conn: u64, view: u64, message: &str) {
    let refuse = |h: &mut Host, error: String| {
        // CDP-shaped when the client's id can be read, so its client fails that one call.
        match serde_json::from_str::<Value>(message).ok().and_then(|v| v.get("id").cloned()) {
            Some(id) => h.to_conn(conn, &KernelDown::Cdp { view, message: cdp_error(&id, &error) }),
            None => h.to_conn(conn, &KernelDown::Error { req: None, error }),
        }
    };
    let out = with("a CDP message", |h| {
        let attached = match (h.pages.get(&view), h.kernel_refused(conn, "send CDP messages to", view)) {
            (None, _) => Err(format!("connection {conn} sent a CDP message to view {view}, which does not exist")),
            (Some(_), Some(e)) => Err(e),
            (Some(p), None) if !p.attached.contains(&conn) => Err(format!("connection {conn} is not attached to view {view} (send attach first)")),
            (Some(_), None) => Ok(()),
        };
        let r = attached.and_then(|()| h.browser_host(view)).and_then(|bh| h.cdp.outgoing(view, conn, message).map(|(id, text)| (bh, id, text)));
        match r {
            Ok(x) => Some(x),
            Err(e) => {
                refuse(h, e);
                None
            }
        }
    })
    .flatten();
    let Some((bh, id, text)) = out else { return };
    if bh.send_dev_tools_message(Some(text.as_bytes())) != 1 {
        with("a CDP message CEF refused", |h| {
            if let Some(Asker::Conn { id: client, .. }) = h.cdp.cancel(id) {
                h.to_conn(conn, &KernelDown::Cdp { view, message: cdp_error(&client, &format!("view {view}'s browser refused the DevTools message")) });
            }
        });
    }
}

fn dropped(conn: u64, why: &str) {
    let close_these = with("a dropped connection", |h| {
        h.conns.remove(&conn);
        h.cdp.forget_conn(conn);
        let mut theirs = Vec::new(); // alloc: per dropped connection
        for (v, p) in &mut h.pages {
            p.attached.remove(&conn);
            p.waiters.retain(|w| !matches!(w, Waiter::Kernel { conn: c, .. } if *c == conn));
            if p.opener == Some(conn) && !p.keep {
                theirs.push(*v);
            }
        }
        theirs
    })
    .unwrap_or_default();
    for v in close_these {
        close(v, format!("connection {conn}, which opened it, dropped ({why})"));
    }
}

fn open_popup(opener: u64, url: String) {
    let spec = with("a page's popup", |h| {
        let p = h.pages.get(&opener)?;
        Some(OpenSpec {
            url: url.clone(),
            owner: Some(format!("popup:{opener}")),
            size: p.info.size,
            opener: p.opener,
            keep: p.keep,
            attached: p.attached.clone(),
            waiter: None,
            // A UI view's popups never open (popup_requested sends them to the app as links).
            ui: false,
        })
    })
    .flatten();
    let Some(spec) = spec else {
        eprintln!("harness-browser: view {opener} opened {url} as a popup, but view {opener} is gone");
        return;
    };
    if let Err(e) = open(spec) {
        eprintln!("harness-browser: view {opener}'s popup {url}: {e}");
        with("a popup refused", |h| h.app_error(None, format!("view {opener}'s popup {url}: {e}")));
    }
}

fn app_gone() {
    let hosts = with("the app's port died", |h| {
        eprintln!("harness-browser: the app's Mach port died; its watches are dropped");
        h.app = None;
        let mut hs = Vec::new(); // alloc: once per app exit
        for (v, p) in &mut h.pages {
            if p.watched {
                p.watched = false;
                p.frames.ring = None;
                hs.push(*v);
            }
        }
        hs.into_iter().filter_map(|v| h.browser_host(v).ok()).collect::<Vec<_>>()
    })
    .unwrap_or_default();
    for bh in hosts {
        bh.set_windowless_frame_rate(FPS_IDLE);
    }
}

/// Quit: close every view, and stop the loop once they are gone or [`QUIT_GRACE_MS`] passed.
pub fn quit(why: String) {
    let browsers = with("quit", |h| {
        if h.quitting.is_some() {
            return None;
        }
        h.quitting = Some(why.clone());
        Some(h.pages.iter_mut().filter_map(|(_, p)| {
            p.closing.get_or_insert_with(|| format!("the host is stopping: {why}"));
            p.browser.clone()
        }).collect::<Vec<_>>()) // alloc: once
    })
    .flatten();
    let Some(browsers) = browsers else { return };
    manage::close();
    if browsers.is_empty() {
        return stop_loop(&why);
    }
    for b in browsers {
        if let Some(h) = b.host() {
            h.close_browser(1);
        }
    }
    let mut t = handlers::force_quit_task();
    if post_delayed_task(ThreadId::UI, Some(&mut t), QUIT_GRACE_MS) != 1 {
        stop_loop(&format!("{why} (the grace timer could not be posted)"));
    }
}

/// The grace period ran out.
pub fn force_quit() {
    let open = with("force quit", |h| h.pages.keys().copied().collect::<Vec<_>>()).unwrap_or_default();
    stop_loop(&format!("views {open:?} did not close within {QUIT_GRACE_MS} ms of quitting"));
}

// ---------------------------------------------------------------- CEF's callbacks, per view

/// A DevTools message from view `view`'s browser (the observer, on the UI thread). The bytes are only valid during
/// the call: everything is decided and sent before it returns.
pub fn devtools(view: u64, message: &[u8]) {
    /// What the message leaves to do once the host has let go.
    enum Then {
        Close(String),
        /// A UI view's binding is in: load its page.
        Load(Browser, String),
    }
    let then = with("a DevTools message", |h| {
        let r = h.cdp.incoming(view, message);
        match r {
            Err(e) => {
                eprintln!("harness-browser: {e}");
                None
            }
            Ok(Incoming::Orphan { .. }) => None,
            Ok(Incoming::Event) => {
                // A UI view has no connection to hear its events (none can attach); its one event of use is a call of
                // its binding, which goes to the app.
                if h.pages.get(&view)?.ui.is_some() {
                    match binding_call(message) {
                        Ok(Some(msg)) => h.to_app(&AppDown::Message { view, msg }),
                        Ok(None) => {}
                        Err(e) => {
                            let url = h.pages.get(&view).and_then(|p| p.ui.as_ref()).map(|u| u.url.clone()).unwrap_or_default();
                            h.app_error(None, format!("UI view {view} ({url}): {e}"));
                        }
                    }
                    return None;
                }
                let Some(p) = h.pages.get(&view) else { return None };
                let text = String::from_utf8_lossy(message).into_owned();
                for c in &p.attached {
                    h.to_conn(*c, &KernelDown::Cdp { view, message: text.clone() });
                }
                None
            }
            Ok(Incoming::Answer { asker: Asker::Conn { conn, .. }, message, .. }) => {
                h.to_conn(conn, &KernelDown::Cdp { view, message });
                None
            }
            Ok(Incoming::Answer { asker: Asker::Host(ask @ (HostAsk::RuntimeEnable | HostAsk::AddBinding)), value, .. }) => {
                let p = h.pages.get(&view)?;
                let url = p.ui.as_ref().map_or_else(|| p.info.url.clone(), |u| u.url.clone());
                let method = if ask == HostAsk::AddBinding { "Runtime.addBinding" } else { "Runtime.enable" };
                if let Some(err) = value.get("error") {
                    // Without its binding a UI view cannot talk to the app: it closes, saying why.
                    return Some(Then::Close(format!("UI view {view} ({url}): the host's {method} failed: {err}")));
                }
                if ask != HostAsk::AddBinding {
                    return None;
                }
                let b = p.browser.clone()?;
                if let Some(u) = h.pages.get_mut(&view).and_then(|p| p.ui.as_mut()) {
                    u.loaded = true;
                }
                Some(Then::Load(b, url))
            }
            Ok(Incoming::Answer { asker: Asker::Host(HostAsk::TargetInfo), value, .. }) => {
                let target = value.get("result").and_then(|r| r.get("targetInfo")).and_then(|t| t.get("targetId")).and_then(Value::as_str).map(str::to_owned);
                let Some(p) = h.pages.get_mut(&view) else { return None };
                let waiters = std::mem::take(&mut p.waiters);
                match target {
                    Some(t) => {
                        p.info.target = t;
                        let info = p.info.clone();
                        for w in waiters {
                            h.answer(w, Ok(info.clone()));
                        }
                        h.views_changed();
                        None
                    }
                    None => {
                        let why = format!("view {view}'s Target.getTargetInfo gave no targetId: {}", Value::Object(value));
                        for w in waiters {
                            h.answer(w, Err(why.clone()));
                        }
                        Some(Then::Close(why))
                    }
                }
            }
        }
    })
    .flatten();
    match then {
        None => {}
        Some(Then::Close(why)) => close(view, why),
        Some(Then::Load(b, url)) => match b.main_frame() {
            Some(f) => f.load_url(Some(&CefString::from(url.as_str()))),
            None => close(view, format!("UI view {view} has no main frame to load {url} in")),
        },
    }
}

/// The payload of a DevTools event if it is a call of a UI view's binding (`Runtime.bindingCalled` named
/// [`BINDING`]); None for any other event.
///
/// # Errors
/// A `Runtime.bindingCalled` for the binding with no string payload.
fn binding_call(message: &[u8]) -> Result<Option<String>, String> {
    #[derive(serde::Deserialize)]
    struct Event {
        #[serde(default)]
        method: String,
        #[serde(default)]
        params: Value,
    }
    let e: Event = serde_json::from_slice(message).map_err(|e| format!("a DevTools event that is not JSON: {e}"))?;
    if e.method != "Runtime.bindingCalled" || e.params.get("name").and_then(Value::as_str) != Some(BINDING) {
        return Ok(None);
    }
    match e.params.get("payload").and_then(Value::as_str) {
        Some(p) => Ok(Some(p.to_owned())),
        None => Err(format!("its {BINDING} call carried no string payload: {}", e.params)),
    }
}

/// `OnBeforeClose`: the view is gone. Everyone hears why; its pending requests and waiters are answered.
pub fn closed(view: u64) {
    let after = with("a view closing", |h| {
        let page = h.pages.remove(&view)?;
        let why = page.closing.clone().unwrap_or_else(|| "the page closed itself".to_owned());
        for a in h.cdp.forget_view(view) {
            if let Asker::Conn { conn, id } = a {
                h.to_conn(conn, &KernelDown::Cdp { view, message: cdp_error(&id, &format!("view {view} closed before answering: {why}")) });
            }
        }
        for w in page.waiters.iter().copied() {
            h.answer(w, Err(format!("view {view} closed before it opened: {why}")));
        }
        // Socket clients never saw a UI view.
        if page.ui.is_none() {
            h.to_all(&KernelDown::Closed { view, why: why.clone() });
        }
        h.to_app(&AppDown::Closed { view, why });
        h.views_changed();
        let quit = h.quitting.clone().filter(|_| h.pages.is_empty());
        Some((page, quit))
    })
    .flatten();
    if let Some((page, quit)) = after {
        // The browser and the observer's registration are released here, outside the borrow.
        drop(page);
        if let Some(why) = quit {
            stop_loop(&why);
        }
    }
}

/// The renderer of `view` ended: the view is closed, saying so.
pub fn renderer_gone(view: u64, why: String) {
    eprintln!("harness-browser: view {view}: {why}");
    close(view, why);
}

/// A page asked for a new window: it opens as a view of its own (posted, so it is made outside CEF's callback). A UI
/// view's never opens: it goes to the app as a link ([`ui_link`]).
pub fn popup_requested(opener: u64, url: String) {
    if is_ui(opener) {
        return ui_link(opener, url);
    }
    if !post(Cmd::OpenPopup { opener, url: url.clone() }) {
        eprintln!("harness-browser: view {opener}'s popup {url} is not opened: the host is stopping");
    }
}

/// The view's rect in points (CEF wants it non-empty).
pub fn view_rect(view: u64) -> Option<Rect> {
    with("view_rect", |h| h.pages.get(&view).map(|p| Rect { x: 0, y: 0, width: p.info.size[0].max(1) as i32, height: p.info.size[1].max(1) as i32 })).flatten()
}

/// The view's scale and rect.
pub fn screen(view: u64) -> Option<(f32, Rect)> {
    with("screen_info", |h| {
        h.pages.get(&view).map(|p| (p.info.scale, Rect { x: 0, y: 0, width: p.info.size[0].max(1) as i32, height: p.info.size[1].max(1) as i32 }))
    })
    .flatten()
}

/// Url, title or loading changed.
pub fn info_changed(view: u64, f: impl FnOnce(&mut ViewInfo)) {
    with("a view's url, title or loading state", |h| {
        if let Some(p) = h.pages.get_mut(&view) {
            f(&mut p.info);
            h.views_changed();
        }
    });
}

pub fn cursor(view: u64, shape: &str) {
    with("a cursor change", |h| {
        if h.pages.get(&view).is_some_and(|p| p.watched) {
            h.to_app(&AppDown::Cursor { view, shape: shape.to_owned() });
        }
    });
}

pub fn ime_rect(view: u64, bounds: &[Rect]) {
    let Some(first) = bounds.first() else { return };
    let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.x + first.width, first.y + first.height);
    for r in bounds {
        x0 = x0.min(r.x);
        y0 = y0.min(r.y);
        x1 = x1.max(r.x + r.width);
        y1 = y1.max(r.y + r.height);
    }
    with("an IME range change", |h| {
        if h.pages.get(&view).is_some_and(|p| p.watched) {
            h.to_app(&AppDown::ImeRect { view, x: x0 as f32, y: y0 as f32, w: (x1 - x0) as f32, h: (y1 - y0) as f32 });
        }
    });
}

/// `OnPopupShow`: a hidden popup's pixels are dropped and the view repainted without it.
pub fn popup_show(view: u64, show: bool) {
    let repaint = with("a popup shown or hidden", |h| {
        let p = h.pages.get_mut(&view)?;
        p.frames.popup.shown = show;
        if show {
            return None;
        }
        p.frames.popup.tex = None;
        h.browser_host(view).ok()
    })
    .flatten();
    if let Some(bh) = repaint {
        bh.invalidate(PaintElementType::VIEW);
    }
}

/// `OnPopupSize`: the popup's rect in view points, kept in pixels.
pub fn popup_size(view: u64, r: &Rect) {
    with("a popup's size", |h| {
        if let Some(p) = h.pages.get_mut(&view) {
            let s = p.info.scale;
            let px = |v: i32| (v as f32 * s).round() as i32;
            p.frames.popup.rect = (px(r.x), px(r.y), px(r.width).max(0) as u32, px(r.height).max(0) as u32);
        }
    });
}

/// `OnAcceleratedPaint`: copy CEF's surface into the ring (or the popup texture) and tell the app.
///
/// `surface` is CEF's pool's IOSurface, valid only during this call: every blit reading it completes before this
/// returns (ring.rs's module doc).
pub fn paint(view: u64, popup: bool, surface: *mut c_void, format: ColorType) {
    let after = with("a paint", |h| {
        let Host { pages, gpu, app, .. } = h;
        let Some(p) = pages.get_mut(&view) else { return After::Nothing };
        if !p.watched || app.is_none() {
            return After::Nothing;
        }
        let r = if format != ColorType::BGRA_8888 {
            Err(format!("view {view} painted in {format:?}; the ring takes only BGRA_8888"))
        } else if surface.is_null() {
            Err(format!("view {view}'s paint carried no IOSurface"))
        } else {
            // SAFETY: CEF's live IOSurface for this call.
            let (w, hh) = unsafe { crate::ring::surface_size(surface) };
            if popup { paint_popup(gpu, app, view, &mut p.frames, surface, w, hh) } else { paint_view(gpu, app, view, &mut p.frames, surface, w, hh) }
        };
        match r {
            Ok(after) => {
                p.frames.reported = false;
                return after;
            }
            Err(e) if !p.frames.reported => {
                p.frames.reported = true;
                eprintln!("harness-browser: {e}");
                if app.is_some() {
                    let _ = send_app(app, &AppDown::Error { req: None, error: e }, &[]);
                }
            }
            Err(_) => {}
        }
        After::Nothing
    });
    if let Some(after) = after {
        follow(view, after);
    }
}

/// How long a view waits to paint again after the app's queue was full.
const FULL_RETRY_MS: i64 = 16;

/// Send slot `slot`'s new picture, the `f.seq`th, and record it in the ring's core: what to do next.
fn send_frame(app: &mut Option<SendRight>, view: u64, f: &mut Frames, slot: u8) -> Result<After, String> {
    let seq = f.seq;
    let ring = f.ring.as_mut().expect("a frame is sent only with a ring");
    let generation = ring.state.generation();
    match send_app(app, &AppDown::Frame { view, generation, slot, seq }, &[]) {
        Ok(()) => Ok(ring.state.sent(slot, seq, true)),
        // The app is not draining: this picture is skipped, and the view paints again shortly.
        Err(SendError::Full) => Ok(ring.state.sent(slot, seq, false)),
        Err(e) => Err(format!("sending view {view}'s frame {seq}: {e}")),
    }
}

fn paint_view(gpu: &Gpu, app: &mut Option<SendRight>, view: u64, f: &mut Frames, surface: *mut c_void, w: u32, h: u32) -> Result<After, String> {
    if f.ring.as_ref().is_none_or(|r| (r.w, r.h) != (w, h)) {
        f.ring = None;
        let generation = f.next_generation;
        f.next_generation = f.next_generation.wrapping_add(1);
        let ring = Ring::new(gpu, generation, w, h).map_err(|e| format!("view {view}'s ring {w}x{h}: {e}"))?;
        let ports = ring.slots.iter().map(|s| s.surface.to_port()).collect::<Result<Vec<_>, _>>().map_err(|e| format!("view {view}'s ring surfaces as ports: {e}"))?; // alloc: once per ring
        let refs: Vec<&SendRight> = ports.iter().collect(); // alloc: once per ring
        send_app(app, &AppDown::Ring { view, generation, w, h, slots: SLOTS as u8 }, &refs).map_err(|e| format!("sending view {view}'s ring {generation} ({w}x{h}): {e}"))?;
        f.ring = Some(ring);
    }
    // No slot the app cannot sample: this paint is dropped, and the latch that frees one asks for it again.
    let Some(slot) = f.ring.as_mut().expect("made above").state.claim() else { return Ok(After::Nothing) };
    // SAFETY: CEF's surface, live for this call; `src` is dropped at the end of this function, after the blit completed.
    let src = unsafe { gpu.wrap(surface, w, h) }.map_err(|e| format!("view {view}'s paint: {e}"))?;
    let ring = f.ring.as_ref().expect("made above");
    let dst = &ring.slots[slot as usize].tex;
    let mut copies = Vec::with_capacity(2); // alloc: per frame, two entries
    copies.push(Blit { src: &src, from: (0, 0), size: (w, h), dst, to: (0, 0) });
    copies.extend(f.popup_copy(dst));
    gpu.blit(&copies).map_err(|e| format!("view {view}: {e}"))?;
    drop(copies);
    f.seq += 1;
    send_frame(app, view, f, slot)
}

fn paint_popup(gpu: &Gpu, app: &mut Option<SendRight>, view: u64, f: &mut Frames, surface: *mut c_void, w: u32, h: u32) -> Result<After, String> {
    if f.popup.tex.as_ref().is_none_or(|t| (t.width() as u32, t.height() as u32) != (w, h)) {
        f.popup.tex = Some(gpu.private(w, h).map_err(|e| format!("view {view}'s popup: {e}"))?);
    }
    {
        // SAFETY: CEF's surface, live for this call; `src` is dropped at the end of this block, after the blit completed.
        let src = unsafe { gpu.wrap(surface, w, h) }.map_err(|e| format!("view {view}'s popup paint: {e}"))?;
        let dst = f.popup.tex.as_ref().expect("made above");
        gpu.blit(&[Blit { src: &src, from: (0, 0), size: (w, h), dst, to: (0, 0) }]).map_err(|e| format!("view {view}'s popup: {e}"))?;
    }
    // Put it over the newest picture, into a writable slot (never the newest: slots.rs); with no picture yet, the
    // next view paint carries it.
    let Some(ring) = f.ring.as_mut() else { return Ok(After::Nothing) };
    let Some(newest) = ring.state.newest() else { return Ok(After::Nothing) };
    let Some(slot) = ring.state.claim() else { return Ok(After::Nothing) };
    let ring = f.ring.as_ref().expect("checked above");
    let dst = &ring.slots[slot as usize].tex;
    let mut copies = Vec::with_capacity(2); // alloc: per popup frame, two entries
    copies.push(Blit { src: &ring.slots[newest as usize].tex, from: (0, 0), size: (ring.w, ring.h), dst, to: (0, 0) });
    copies.extend(f.popup_copy(dst));
    gpu.blit(&copies).map_err(|e| format!("view {view}: {e}"))?;
    drop(copies);
    f.seq += 1;
    send_frame(app, view, f, slot)
}

/// Do what the ring's core asked for view `view` (outside the host's borrow).
fn follow(view: u64, after: After) {
    match after {
        After::Nothing => {}
        After::Repaint => repaint(view),
        After::RepaintLater => {
            let mut t = handlers::repaint_task(view);
            if post_delayed_task(ThreadId::UI, Some(&mut t), FULL_RETRY_MS) != 1 {
                eprintln!("harness-browser: view {view}'s repaint after a full app queue could not be posted (the host is stopping)");
            }
        }
    }
}

/// Ask view `view` to paint again (a dropped paint, or one the app's full queue refused).
pub fn repaint(view: u64) {
    if let Some(bh) = with("a repaint", |h| h.pages.get(&view).filter(|p| p.watched).and_then(|_| h.browser_host(view).ok())).flatten() {
        bh.invalidate(PaintElementType::VIEW);
    }
}

// ---------------------------------------------------------------- the app's view state (app_link.rs)

/// The app said hello with its port: it replaces any earlier one, and every watch starts over.
pub fn app_hello(port: SendRight) -> Vec<BrowserHost> {
    with("the app's hello", |h| {
        h.app = Some(port);
        let mut idle = Vec::new(); // alloc: once per app hello
        for (v, p) in &mut h.pages {
            if p.watched {
                p.watched = false;
                p.frames.ring = None;
                idle.push(*v);
            }
        }
        let hosts: Vec<BrowserHost> = idle.into_iter().filter_map(|v| h.browser_host(v).ok()).collect(); // alloc: once per app hello
        h.to_app(&AppDown::Hello { schema: harness_proto::SCHEMA, pid: std::process::id() });
        h.views_changed();
        hosts
    })
    .unwrap_or_default()
}

/// Watch on or off: a new ring at the next paint.
pub fn watch(view: u64, on: bool) -> Result<BrowserHost, String> {
    with("a watch", |h| {
        let p = h.pages.get_mut(&view).ok_or_else(|| format!("the app watched view {view}, which does not exist"))?;
        p.watched = on;
        p.frames.ring = None;
        p.frames.reported = false;
        h.browser_host(view)
    })
    .unwrap_or_else(|| Err(format!("watching view {view}: the host is not running")))
}

/// Resize: the new size in points and scale. Answers the host and whether the scale changed.
pub fn resize(view: u64, w: u32, hh: u32, scale: f32) -> Result<(BrowserHost, bool), String> {
    if w == 0 || hh == 0 || !(scale > 0.0) {
        return Err(format!("the app resized view {view} to {w}x{hh} points at scale {scale}: empty"));
    }
    with("a resize", |h| {
        let p = h.pages.get_mut(&view).ok_or_else(|| format!("the app resized view {view}, which does not exist"))?;
        let scaled = p.info.scale != scale;
        p.info.size = [w, hh];
        p.info.scale = scale;
        let bh = h.browser_host(view)?;
        h.views_changed();
        Ok((bh, scaled))
    })
    .unwrap_or_else(|| Err(format!("resizing view {view}: the host is not running")))
}

/// The app latched a slot.
pub fn latched(view: u64, generation: u32, slot: u8) -> Result<(), String> {
    let after = with("a latch", |h| {
        let p = h.pages.get_mut(&view).ok_or_else(|| format!("the app latched a slot of view {view}, which does not exist"))?;
        // With no ring (unwatched, or a new one not yet painted) the latch is about surfaces already dropped here.
        let Some(r) = p.frames.ring.as_mut() else { return Ok(After::Nothing) };
        r.state.latched(generation, slot).map_err(|e| format!("view {view}: {e}"))
    })
    .unwrap_or_else(|| Err(format!("latching view {view}: the host is not running")))?;
    follow(view, after);
    Ok(())
}

/// Send an error (or anything else) to the app from outside the borrow.
pub fn tell_app(msg: AppDown) {
    with("a message to the app", |h| {
        if let AppDown::Error { req, error } = msg {
            h.app_error(req, error);
        } else {
            h.to_app(&msg);
        }
    });
}

/// Whether a view exists.
pub fn exists(view: u64) -> bool {
    with("a view lookup", |h| h.pages.contains_key(&view)).unwrap_or(false)
}

// ---------------------------------------------------------------- UI views (README.md, "UI views")

/// Whether `view` is a UI view.
pub fn is_ui(view: u64) -> bool {
    with("a UI view lookup", |h| h.pages.get(&view).is_some_and(|p| p.ui.is_some())).unwrap_or(false)
}

/// UI view `view` tried to go to `url`, which it may not: the app hears it as a link to open where it opens links.
pub fn ui_link(view: u64, url: String) {
    eprintln!("harness-browser: UI view {view} stays on its page; {url} goes to the app as a link");
    with("a UI view's link", |h| h.to_app(&AppDown::Link { view, url }));
}

/// A top-level navigation of `view` to `url` (`OnBeforeBrowse` of its main frame): whether it is cancelled. Only a
/// UI view's is: before the host loaded its page (no navigation is the page's then; it is cancelled and logged), or
/// when `url` is outside the origin it opened at (that url goes to the app as a link).
pub fn navigation_refused(view: u64, url: &str) -> bool {
    /// Why a UI view's navigation is cancelled.
    enum No {
        Early,
        Outside,
    }
    let no = with("a navigation", |h| {
        let u = h.pages.get(&view)?.ui.as_ref()?;
        if !u.loaded {
            (!u.origin.admits(url)).then_some(No::Early)
        } else if !u.origin.admits(url) {
            Some(No::Outside)
        } else {
            None
        }
    })
    .flatten();
    match no {
        None => false,
        Some(No::Early) => {
            eprintln!("harness-browser: UI view {view} was navigated to {url} before the host loaded its page; cancelled");
            true
        }
        Some(No::Outside) => {
            ui_link(view, url.to_owned());
            true
        }
    }
}

/// `AppUp::Post`: the main frame of UI view `view`, and the script that hands `msg` to its page's `harnessRecv`.
///
/// # Errors
/// No such view, a view that is not a UI view, text that is not JSON, or no main frame, naming the view (and its url).
pub fn ui_post(view: u64, msg: &str) -> Result<(Frame, String), String> {
    let b = with("a post to a UI view", |h| {
        let p = h.pages.get(&view).ok_or_else(|| format!("the app posted to view {view}, which does not exist"))?;
        let ui = p.ui.as_ref().ok_or_else(|| format!("the app posted to view {view} ({}), which is not a UI view", p.info.url))?;
        if let Err(e) = serde_json::from_str::<serde::de::IgnoredAny>(msg) {
            return Err(format!("the app posted text that is not JSON to UI view {view} ({}): {e}", ui.url));
        }
        let b = p.browser.clone().ok_or_else(|| format!("UI view {view} ({}) has no browser yet", ui.url))?;
        Ok((b, ui.url.clone()))
    })
    .unwrap_or_else(|| Err(format!("posting to view {view}: the host is not running")))?;
    let (b, url) = b;
    let f = b.main_frame().ok_or_else(|| format!("UI view {view} ({url}) has no main frame to post to"))?;
    // JSON text is a JavaScript expression; a page with no harnessRecv yet (still loading) gets nothing, and says
    // `ready` once it has one.
    Ok((f, format!("window.harnessRecv && window.harnessRecv({msg});")))
}
