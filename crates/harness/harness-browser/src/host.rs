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
}

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
}

/// Who an opened view answers.
pub enum WaiterSpec {
    Kernel { conn: u64, req: u64 },
    App { req: u64 },
}

/// Open a view: the page goes in the table first (CEF asks for its rect while it creates the browser), then the
/// browser, its DevTools observer, and the `Target.getTargetInfo` whose answer gives the waiter its `Opened`.
///
/// # Errors
/// CEF refused the browser or the observer, naming the view and the url. The waiter is not answered (the caller
/// answers the error).
pub fn open(spec: OpenSpec) -> Result<u64, String> {
    if spec.size[0] == 0 || spec.size[1] == 0 {
        return Err(format!("a view {}x{} points at {} is empty", spec.size[0], spec.size[1], spec.url));
    }
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
                info: ViewInfo { view, url: spec.url.clone(), owner: spec.owner.clone(), size: spec.size, scale: DEFAULT_SCALE, loading: true, ..Default::default() },
                opener: spec.opener,
                keep: spec.keep,
                attached: spec.attached,
                waiters,
                watched: false,
                frames: Frames::default(),
                closing: None,
            },
        );
        Ok(view)
    })
    .unwrap_or_else(|| Err(format!("opening {url}: the host is not running")))?;

    let fail = |e: String| {
        with("a view that failed to open", |h| h.pages.remove(&view));
        Err(e)
    };
    let info = WindowInfo { windowless_rendering_enabled: 1, shared_texture_enabled: 1, runtime_style: RuntimeStyle::ALLOY, ..Default::default() };
    let settings = BrowserSettings { windowless_frame_rate: FPS_IDLE, ..Default::default() };
    let mut client = handlers::view_client(view);
    let Some(browser) = browser_host_create_browser_sync(Some(&info), Some(&mut client), Some(&CefString::from(url.as_str())), Some(&settings), None, None) else {
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
    let ask = with("a new view's target request", |h| {
        let page = h.pages.get_mut(&view)?;
        page.browser = Some(browser.clone());
        page._observer = Some(reg);
        Some(h.cdp.host_request(view, HostAsk::TargetInfo, "Target.getTargetInfo", serde_json::json!({})))
    })
    .flatten();
    let ask = match ask {
        Some(Ok(ask)) => ask,
        Some(Err(e)) => {
            close(view, e.clone());
            return Err(e);
        }
        // Closed while it was being made (its OnBeforeClose already took the page).
        None => return Err(format!("view {view} ({url}) closed while it was being opened")),
    };
    if bhost.send_dev_tools_message(Some(ask.as_bytes())) != 1 {
        close(view, format!("CEF refused the Target.getTargetInfo that names view {view}'s page target"));
        return Err(format!("CEF refused to send Target.getTargetInfo to view {view} ({url})"));
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
            };
            if let Err(error) = open(spec) {
                with("an open refused", |h| h.to_conn(conn, &KernelDown::Error { req: Some(req), error }));
            }
        }
        KernelUp::Attach { req, view } => {
            with("attach", |h| {
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
            let known = with("close", |h| h.pages.contains_key(&view)).unwrap_or(false);
            if known {
                close(view, format!("connection {conn} closed it"));
            } else {
                with("close", |h| h.to_conn(conn, &KernelDown::Error { req: None, error: format!("connection {conn} closed view {view}, which does not exist") }));
            }
        }
        KernelUp::Views { req } => {
            with("views", |h| {
                let views = h.views();
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
        let attached = match h.pages.get(&view) {
            None => Err(format!("connection {conn} sent a CDP message to view {view}, which does not exist")),
            Some(p) if !p.attached.contains(&conn) => Err(format!("connection {conn} is not attached to view {view} (send attach first)")),
            Some(_) => Ok(()),
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
    let close_why = with("a DevTools message", |h| {
        let r = h.cdp.incoming(view, message);
        match r {
            Err(e) => {
                eprintln!("harness-browser: {e}");
                None
            }
            Ok(Incoming::Orphan { .. }) => None,
            Ok(Incoming::Event) => {
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
                        Some(why)
                    }
                }
            }
        }
    })
    .flatten();
    if let Some(why) = close_why {
        close(view, why);
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
        h.to_all(&KernelDown::Closed { view, why: why.clone() });
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

/// A page asked for a new window: it opens as a view of its own (posted, so it is made outside CEF's callback).
pub fn popup_requested(opener: u64, url: String) {
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
