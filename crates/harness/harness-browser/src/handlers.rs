//! CEF's interfaces, as cef-rs's `wrap_*` types. Each per-view handler carries only its view's id and hands everything
//! to host.rs, which owns the state; none of them keeps a CEF object.
use crate::host;
use cef::*;
use std::ffi::c_int;

#[repr(C)]
#[derive(Clone, Copy)]
struct CgRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CgRect;
}

/// The main display's size in points, at the origin. None when Core Graphics names no display (no screen attached).
fn main_display() -> Option<Rect> {
    // SAFETY: two Core Graphics calls with no preconditions; CGRect is four doubles (origin, then size).
    let r = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    (r.w >= 1.0 && r.h >= 1.0).then(|| Rect { x: 0, y: 0, width: r.w as i32, height: r.h as i32 })
}

// ---------------------------------------------------------------- the app and the browser process

wrap_app! {
    pub struct HostApp {
        process: BrowserProcessHandler,
    }

    impl App {
        fn on_before_command_line_processing(&self, process_type: Option<&CefString>, command_line: Option<&mut CommandLine>) {
            let is_browser = process_type.is_none_or(|t| t.to_string().is_empty());
            let Some(cl) = command_line else { return };
            // Never the login Keychain, in any process (the network service encrypts cookies too): Chromium's cookie
            // key would be a "Harness Browser Safe Storage" item, and every rebuilt (re-signed) host makes macOS ask the
            // person for their Keychain password, while cookie access, and so every navigation, waits on the answer.
            // Chromium on macOS has no file-backed key (`--password-store` is Linux's), so the mock keychain: a fixed
            // key, the profile protected by `$HARNESS_HOME/browser/` being 0700.
            cl.append_switch(Some(&CefString::from("use-mock-keychain")));
            // The rest is the browser process's (no --type); helpers get Chromium's own switches from it.
            if !is_browser {
                return;
            }
            // No notifications, ever: no Web Notification API for pages, and no macOS notification bridge (the
            // SystemNotifications and NativeNotifications features route them to UNUserNotificationCenter, whose
            // first use makes macOS ask the person for permission, once for the host and once for its Alerts helper).
            cl.append_switch(Some(&CefString::from("disable-notifications")));
            // UseKeychainKeyProvider: os_crypt_async's Keychain provider, kept off with the mock keychain above.
            let mut off: Vec<&str> = vec!["SystemNotifications", "NativeNotifications", "UseKeychainKeyProvider"]; // alloc: once per process start
            let dirs = match crate::extensions::unpacked_dirs(&host::config().dir) {
                Ok(d) => d,
                Err(e) => {
                    println!("harness-browser failed: {e}");
                    eprintln!("harness-browser failed: {e}");
                    std::process::exit(1);
                }
            };
            if !dirs.is_empty() {
                let mut list = String::new();
                for d in &dirs {
                    let Some(s) = d.to_str() else {
                        println!("harness-browser failed: the unpacked extension directory {} is not UTF-8", d.display());
                        std::process::exit(1);
                    };
                    if s.contains(',') {
                        println!("harness-browser failed: the unpacked extension directory {s} has a comma, which --load-extension cannot carry");
                        std::process::exit(1);
                    }
                    if !list.is_empty() {
                        list.push(',');
                    }
                    list.push_str(s);
                }
                cl.append_switch_with_value(Some(&CefString::from("load-extension")), Some(&CefString::from(list.as_str())));
                // Chromium 137+ ignores --load-extension unless this feature is off.
                off.push("DisableLoadExtensionCommandLineSwitch");
            }
            // Chromium reads one --disable-features: keep what is there (CEF's or the caller's) and add ours to it.
            let name = CefString::from("disable-features");
            let had = CefString::from(&cl.switch_value(Some(&name))).to_string();
            let mut value = had.clone();
            for f in off {
                if !value.split(',').any(|x| x == f) {
                    if !value.is_empty() {
                        value.push(',');
                    }
                    value.push_str(f);
                }
            }
            cl.append_switch_with_value(Some(&name), Some(&CefString::from(value.as_str())));
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(self.process.clone())
        }
    }
}

wrap_browser_process_handler! {
    pub struct HostProcess;

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            host::start();
        }
    }
}

/// The browser process's app.
pub fn app() -> App {
    HostApp::new(HostProcess::new())
}

// ---------------------------------------------------------------- tasks

wrap_task! {
    pub struct Drain;

    impl Task {
        fn execute(&self) {
            host::drain();
        }
    }
}

wrap_task! {
    pub struct ForceQuit;

    impl Task {
        fn execute(&self) {
            host::force_quit();
        }
    }
}

wrap_task! {
    pub struct Repaint {
        view: u64,
    }

    impl Task {
        fn execute(&self) {
            host::repaint(self.view);
        }
    }
}

pub fn repaint_task(view: u64) -> Task {
    Repaint::new(view)
}

pub fn drain_task() -> Task {
    Drain::new()
}

pub fn force_quit_task() -> Task {
    ForceQuit::new()
}

// ---------------------------------------------------------------- one view's client

wrap_client! {
    pub struct ViewClient {
        render: RenderHandler,
        display: DisplayHandler,
        load: LoadHandler,
        life: LifeSpanHandler,
        request: RequestHandler,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render.clone())
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display.clone())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load.clone())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life.clone())
        }

        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.request.clone())
        }
    }
}

/// The client of view `view`.
pub fn view_client(view: u64) -> Client {
    ViewClient::new(ViewRender::new(view), ViewDisplay::new(view), ViewLoad::new(view), ViewLife::new(view), ViewRequest::new(view))
}

wrap_render_handler! {
    pub struct ViewRender {
        view: u64,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            match host::view_rect(self.view) {
                Some(r) => *rect = r,
                None => {
                    // Only when the page is gone (closing) or the host was borrowed (reported by `with`).
                    eprintln!("harness-browser: CEF asked for view {}'s rect, which the host does not have", self.view);
                    *rect = Rect { x: 0, y: 0, width: 1, height: 1 };
                }
            }
        }

        fn screen_point(&self, _browser: Option<&mut Browser>, view_x: c_int, view_y: c_int, screen_x: Option<&mut c_int>, screen_y: Option<&mut c_int>) -> c_int {
            // An off-screen view has no place on a screen: view points are screen points (menus and tooltips the
            // page draws itself stay in the view).
            let (Some(sx), Some(sy)) = (screen_x, screen_y) else { return 0 };
            *sx = view_x;
            *sy = view_y;
            1
        }

        fn screen_info(&self, _browser: Option<&mut Browser>, screen_info: Option<&mut ScreenInfo>) -> c_int {
            let Some(si) = screen_info else { return 0 };
            let Some((scale, rect)) = host::screen(self.view) else { return 0 };
            si.device_scale_factor = scale;
            si.depth = 24;
            si.depth_per_component = 8;
            si.is_monochrome = 0;
            // The screen is the Mac's main display, not the view: a page sees a window on a screen (`screen.width`
            // larger than its `innerWidth`), as in any browser; a screen exactly the size of the viewport is what an
            // embedded or headless browser looks like. The view's own rect when the display cannot be read.
            let rect = main_display().unwrap_or(rect);
            si.rect = rect.clone();
            si.available_rect = rect;
            1
        }

        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: c_int) {
            host::popup_show(self.view, show != 0);
        }

        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            if let Some(r) = rect {
                host::popup_size(self.view, r);
            }
        }

        fn on_accelerated_paint(&self, _browser: Option<&mut Browser>, type_: PaintElementType, _dirty_rects: Option<&[Rect]>, info: Option<&AcceleratedPaintInfo>) {
            let Some(info) = info else { return };
            host::paint(self.view, type_ == PaintElementType::POPUP, info.shared_texture_io_surface, info.format);
        }

        fn on_ime_composition_range_changed(&self, _browser: Option<&mut Browser>, _selected_range: Option<&Range>, character_bounds: Option<&[Rect]>) {
            if let Some(b) = character_bounds {
                host::ime_rect(self.view, b);
            }
        }
    }
}

wrap_display_handler! {
    pub struct ViewDisplay {
        view: u64,
    }

    impl DisplayHandler {
        fn on_address_change(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, url: Option<&CefString>) {
            if frame.is_some_and(|f| f.is_main() == 0) {
                return;
            }
            let url = url.map(ToString::to_string).unwrap_or_default();
            host::info_changed(self.view, |i| i.url = url);
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            let title = title.map(ToString::to_string).unwrap_or_default();
            host::info_changed(self.view, |i| i.title = title);
        }

        fn on_cursor_change(&self, _browser: Option<&mut Browser>, _cursor: *mut u8, type_: CursorType, _custom_cursor_info: Option<&CursorInfo>) -> c_int {
            host::cursor(self.view, cursor_name(type_));
            1
        }
    }
}

/// CEF's cursor as a name of harness-native-ui-input-api's `Pointer`. Every CEF cursor is listed; those the app has no
/// shape for are the arrow, said so here rather than by a catch-all.
fn cursor_name(t: CursorType) -> &'static str {
    const TABLE: &[(CursorType, &str)] = &[
        (CursorType::POINTER, "Arrow"),
        (CursorType::IBEAM, "Text"),
        (CursorType::VERTICALTEXT, "VerticalText"),
        (CursorType::HAND, "Hand"),
        (CursorType::EASTWESTRESIZE, "ResizeLeftRight"),
        (CursorType::COLUMNRESIZE, "ResizeLeftRight"),
        (CursorType::EASTRESIZE, "ResizeRight"),
        (CursorType::WESTRESIZE, "ResizeLeft"),
        (CursorType::NORTHSOUTHRESIZE, "ResizeUpDown"),
        (CursorType::ROWRESIZE, "ResizeUpDown"),
        (CursorType::NORTHRESIZE, "ResizeUpDown"),
        (CursorType::SOUTHRESIZE, "ResizeUpDown"),
        (CursorType::NOTALLOWED, "NotAllowed"),
        (CursorType::NODROP, "NotAllowed"),
        (CursorType::GRAB, "OpenHand"),
        (CursorType::GRABBING, "Grabbing"),
        // No shape in the app: the arrow.
        (CursorType::CROSS, "Arrow"),
        (CursorType::WAIT, "Arrow"),
        (CursorType::PROGRESS, "Arrow"),
        (CursorType::HELP, "Arrow"),
        (CursorType::MOVE, "Arrow"),
        (CursorType::CUSTOM, "Arrow"),
        (CursorType::NONE, "Arrow"),
    ];
    match TABLE.iter().find(|(c, _)| *c == t).map(|(_, n)| *n) {
        Some(n) => n,
        None => {
            eprintln!("harness-browser: CEF cursor {t:?} has no entry in handlers::cursor_name; it shows as the arrow");
            "Arrow"
        }
    }
}

wrap_load_handler! {
    pub struct ViewLoad {
        view: u64,
    }

    impl LoadHandler {
        fn on_loading_state_change(&self, _browser: Option<&mut Browser>, is_loading: c_int, can_go_back: c_int, can_go_forward: c_int) {
            host::info_changed(self.view, |i| {
                i.loading = is_loading != 0;
                i.can_back = can_go_back != 0;
                i.can_forward = can_go_forward != 0;
            });
        }
    }
}

wrap_life_span_handler! {
    pub struct ViewLife {
        view: u64,
    }

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut c_int>
        ) -> c_int {
            // No native window: the page's new window opens as a view of its own (owner popup:<view>).
            let url = target_url.map(ToString::to_string).unwrap_or_default();
            host::popup_requested(self.view, url);
            1
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            host::closed(self.view);
        }
    }
}

wrap_request_handler! {
    pub struct ViewRequest {
        view: u64,
    }

    impl RequestHandler {
        fn on_render_process_terminated(&self, _browser: Option<&mut Browser>, status: TerminationStatus, error_code: c_int, error_string: Option<&CefString>) {
            let words = error_string.map(ToString::to_string).unwrap_or_default();
            host::renderer_gone(self.view, format!("its renderer ended ({status:?}, code {error_code}{}{words})", if words.is_empty() { "" } else { ": " }));
        }
    }
}

wrap_dev_tools_message_observer! {
    pub struct ViewDevTools {
        view: u64,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_message(&self, _browser: Option<&mut Browser>, message: Option<&[u8]>) -> c_int {
            if let Some(m) = message {
                host::devtools(self.view, m);
            }
            1
        }
    }
}

pub fn devtools_observer(view: u64) -> DevToolsMessageObserver {
    ViewDevTools::new(view)
}

// ---------------------------------------------------------------- the manage window (manage.rs)

wrap_client! {
    pub struct PlainClient;

    impl Client {}
}

pub fn plain_client() -> Client {
    PlainClient::new()
}

wrap_browser_view_delegate! {
    pub struct ManageViewDelegate {}

    impl ViewDelegate {}

    impl BrowserViewDelegate {
        fn browser_runtime_style(&self) -> RuntimeStyle {
            RuntimeStyle::CHROME
        }
    }
}

pub fn manage_view_delegate() -> BrowserViewDelegate {
    ManageViewDelegate::new()
}

wrap_window_delegate! {
    pub struct ManageWindow {
        view: BrowserView,
    }

    impl ViewDelegate {}

    impl PanelDelegate {}

    impl WindowDelegate {
        fn on_window_created(&self, window: Option<&mut Window>) {
            let Some(w) = window else { return };
            let mut v = View::from(&self.view);
            w.add_child_view(Some(&mut v));
            w.set_title(Some(&CefString::from("Harness Browser")));
            w.center_window(Some(&Size { width: 1200, height: 820 }));
            w.show();
        }

        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
            crate::manage::destroyed();
        }

        fn can_close(&self, _window: Option<&mut Window>) -> c_int {
            // Let the browser run its unload handlers first; the window closes when it is done.
            match self.view.browser().and_then(|b| b.host()) {
                Some(h) => h.try_close_browser(),
                None => 1,
            }
        }

        fn window_runtime_style(&self) -> RuntimeStyle {
            RuntimeStyle::CHROME
        }
    }
}

pub fn manage_window(view: BrowserView) -> WindowDelegate {
    ManageWindow::new(view)
}
