//! The window: raw AppKit (NSApplication, one NSWindow, one flipped layer-hosting NSView over a CAMetalLayer), after
//! den-native's window.rs. Frames come from a `CAMetalDisplayLink`, paused whenever the app is idle; the daemon's threads
//! wake it through the main queue.
use crate::app::{App, KeyMods};
use crate::gpu::{self, Dest, Renderer};
use crate::single::{self, Ask};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSAutoresizingMaskOptions, NSBackingStoreType, NSColor, NSEvent, NSEventModifierFlags, NSMenu, NSMenuItem,
    NSPasteboard, NSPasteboardTypeString, NSResponder, NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow, NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_core_foundation::CGSize;
use objc2_core_graphics::{CGColorSpace, kCGColorSpaceExtendedSRGB};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString};
use objc2_metal::MTLCreateSystemDefaultDevice;
use objc2_quartz_core::{CAFrameRateRange, CAMetalDisplayLink, CAMetalDisplayLinkDelegate, CAMetalDisplayLinkUpdate, CAMetalLayer, kCAGravityTopLeft};
use std::cell::{OnceCell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const INITIAL: (f64, f64) = (1180.0, 760.0);

struct State {
    app: App,
    renderer: Renderer,
    layer: Retained<CAMetalLayer>,
    link: Retained<CAMetalDisplayLink>,
    window: Retained<NSWindow>,
    last: Option<f64>,
    title: String,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static DELEGATE: OnceCell<Retained<AppDelegate>> = const { OnceCell::new() };
    /// The session `HARNESS_NATIVE_SELECT` asked for, for the window to show once it is made.
    static SELECT: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Run `f` on the window's state, unless it is in use (an event during a frame).
fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn wake() {
    with(|st| st.link.setPaused(false));
}

/// A wake the daemon's threads may call: it reaches the main thread once per burst.
fn wake_from_any_thread() -> Arc<dyn Fn() + Send + Sync> {
    let pending = Arc::new(AtomicBool::new(false));
    Arc::new(move || {
        if pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let p = pending.clone();
        DispatchQueue::main().exec_async(move || {
            p.store(false, Ordering::Release);
            with(|st| {
                st.app.poke();
                st.link.setPaused(false);
            });
        });
    })
}

fn mods_of(flags: NSEventModifierFlags) -> KeyMods {
    KeyMods {
        cmd: flags.contains(NSEventModifierFlags::Command),
        shift: flags.contains(NSEventModifierFlags::Shift),
        ctrl: flags.contains(NSEventModifierFlags::Control),
        alt: flags.contains(NSEventModifierFlags::Option),
    }
}

define_class!(
    // SAFETY: NSView has no subclassing requirements beyond calling super's designated initialiser; no Drop.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HarnessView"]
    struct HarnessView;

    unsafe impl NSObjectProtocol for HarnessView {}

    impl HarnessView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            // SAFETY: forwarding the same selector to the superclass with its own argument.
            let _: () = unsafe { msg_send![super(self), setFrameSize: size] };
            self.sync_size();
        }

        #[unsafe(method(viewDidChangeBackingProperties))]
        fn did_change_backing(&self) {
            self.sync_size();
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let chars = event.characters().map(|s| s.to_string()).unwrap_or_default();
            let bare = event.charactersIgnoringModifiers().map(|s| s.to_string()).unwrap_or_default();
            let m = mods_of(event.modifierFlags());
            with(|st| st.app.key(event.keyCode(), &chars, &bare, m));
            wake();
        }

        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            let m = mods_of(event.modifierFlags());
            let key = event.charactersIgnoringModifiers().map(|s| s.to_string()).unwrap_or_default();
            let took = m.cmd && !m.ctrl && !matches!(key.as_str(), "c" | "v" | "q" | "h" | "m") && with(|st| st.app.shortcut(&key, event.keyCode(), m)).unwrap_or(false);
            if took {
                wake();
                true
            } else {
                // SAFETY: forwarding the same selector to the superclass with its own argument.
                let handled: objc2::runtime::Bool = unsafe { msg_send![super(self), performKeyEquivalent: event] };
                handled.as_bool()
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = self.local(event);
            with(|st| st.app.pointer_down(p.x as f32, p.y as f32));
            wake();
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let p = self.local(event);
            with(|st| st.app.pointer_move(p.x as f32, p.y as f32));
            wake();
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            with(|st| st.app.pointer_up());
            wake();
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let p = self.local(event);
            with(|st| st.app.pointer_move(p.x as f32, p.y as f32));
            wake();
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            with(|st| st.app.pointer_move(-1.0e4, -1.0e4));
            wake();
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let precise = event.hasPreciseScrollingDeltas();
            let dy = event.scrollingDeltaY() as f32;
            with(|st| {
                let z = st.app.zoom();
                st.app.wheel(if precise { dy / z } else { dy }, precise);
            });
            wake();
        }

        #[unsafe(method(copy:))]
        fn copy(&self, _sender: Option<&AnyObject>) {
            if let Some(text) = with(|st| st.app.copy()).flatten() {
                let pb = NSPasteboard::generalPasteboard();
                pb.clearContents();
                // SAFETY: NSPasteboardTypeString is a valid static pasteboard type.
                pb.setString_forType(&NSString::from_str(&text), unsafe { NSPasteboardTypeString });
            }
        }

        #[unsafe(method(paste:))]
        fn paste(&self, _sender: Option<&AnyObject>) {
            // SAFETY: NSPasteboardTypeString is a valid static pasteboard type.
            let text = NSPasteboard::generalPasteboard().stringForType(unsafe { NSPasteboardTypeString }).map(|s| s.to_string());
            if let Some(text) = text {
                with(|st| st.app.paste(&text));
                wake();
            }
        }

        /// A right click (or control-click) on a tree row: what can be done to its object.
        #[unsafe(method_id(menuForEvent:))]
        fn menu_for_event(&self, event: &NSEvent) -> Option<Retained<NSMenu>> {
            self.context_menu(event)
        }

        #[unsafe(method(ctxKill:))]
        fn ctx_kill(&self, sender: &NSMenuItem) {
            let e = sender.tag() as u64;
            with(|st| st.app.kill_object(e));
            wake();
        }

        #[unsafe(method(windowDidBecomeKey:))]
        fn did_become_key(&self, _note: &NSNotification) {
            with(|st| {
                st.app.focused = true;
                st.app.poke();
            });
            wake();
        }

        #[unsafe(method(windowDidResignKey:))]
        fn did_resign_key(&self, _note: &NSNotification) {
            with(|st| {
                st.app.focused = false;
                st.app.poke();
            });
            wake();
        }
    }

    unsafe impl CAMetalDisplayLinkDelegate for HarnessView {
        #[unsafe(method(metalDisplayLink:needsUpdate:))]
        fn needs_update(&self, _link: &CAMetalDisplayLink, update: &CAMetalDisplayLinkUpdate) {
            self.frame(update);
        }
    }
);

impl HarnessView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: initWithFrame: is NSView's designated initialiser.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn local(&self, event: &NSEvent) -> NSPoint {
        self.convertPoint_fromView(event.locationInWindow(), None)
    }

    fn context_menu(&self, event: &NSEvent) -> Option<Retained<NSMenu>> {
        let p = self.local(event);
        let e = with(|st| st.app.object_at(p.x as f32, p.y as f32)).flatten()?;
        let mtm = self.mtm();
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let kill = item(mtm, "Kill", sel!(ctxKill:), "");
        // SAFETY: the view outlives the menu it pops up; the item only holds it weakly.
        unsafe { kill.setTarget(Some(self)) };
        kill.setTag(e as isize);
        menu.addItem(&kill);
        Some(menu)
    }

    fn sync_size(&self) {
        let points = self.bounds().size;
        let pixels = self.convertSizeToBacking(points);
        if points.width <= 0.0 || points.height <= 0.0 {
            return;
        }
        with(|st| {
            st.layer.setContentsScale(pixels.width / points.width);
            st.layer.setDrawableSize(CGSize::new(pixels.width.round(), pixels.height.round()));
            st.app.poke();
            st.link.setPaused(false);
        });
    }

    fn frame(&self, update: &CAMetalDisplayLinkUpdate) {
        let points = self.bounds().size;
        let backing = self.convertSizeToBacking(points).width / points.width.max(1.0);
        let drawable = update.drawable();
        let t = update.targetTimestamp();
        with(|st| {
            let dt = st.last.map_or(1.0 / 120.0, |l| (t - l).clamp(0.0, 0.1)) as f32;
            st.last = Some(t);
            let renderer = &st.renderer;
            let draw = st.app.frame((points.width as f32, points.height as f32), backing as f32, dt, &mut |x, y, w, h, b| renderer.upload_atlas(x, y, w, h, b));
            st.renderer.render(&draw, Dest::Drawable(drawable));
            if st.app.title != st.title {
                st.title.clone_from(&st.app.title);
                st.window.setTitle(&NSString::from_str(&st.title));
            }
            if st.app.idle() {
                st.link.setPaused(true);
                st.last = None;
            }
        });
    }
}

fn make_window(mtm: MainThreadMarker) -> Result<(), String> {
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(INITIAL.0, INITIAL.1));
    let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable | NSWindowStyleMask::Resizable | NSWindowStyleMask::FullSizeContentView;
    // SAFETY: a fresh window with a valid rect and style.
    let window = unsafe { NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), rect, style, NSBackingStoreType::Buffered, false) };
    // SAFETY: the state keeps its own reference; AppKit must not release it on close as well.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("harness"));
    window.setTitlebarAppearsTransparent(true);
    window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
    window.setMinSize(NSSize::new(560.0, 320.0));
    window.setBackgroundColor(Some(&NSColor::blackColor()));
    let view = HarnessView::new(mtm, rect);
    view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);

    let renderer = Renderer::new(MTLCreateSystemDefaultDevice().ok_or("no Metal device")?);
    let layer = CAMetalLayer::new();
    layer.setDevice(Some(&**renderer.device()));
    layer.setPixelFormat(gpu::PIXEL_FORMAT);
    layer.setMaximumDrawableCount(3);
    layer.setOpaque(true);
    // SAFETY: kCGColorSpaceExtendedSRGB is a valid static colour space name.
    layer.setColorspace(CGColorSpace::with_name(Some(unsafe { kCGColorSpaceExtendedSRGB })).as_deref());
    // SAFETY: kCAGravityTopLeft is a valid static gravity name.
    layer.setContentsGravity(unsafe { kCAGravityTopLeft });
    view.setLayer(Some(&layer));
    view.setWantsLayer(true);
    let options = NSTrackingAreaOptions::MouseEnteredAndExited | NSTrackingAreaOptions::MouseMoved | NSTrackingAreaOptions::ActiveInKeyWindow | NSTrackingAreaOptions::InVisibleRect;
    // SAFETY: the view owns the area and outlives it; no user info.
    let area = unsafe { NSTrackingArea::initWithRect_options_owner_userInfo(NSTrackingArea::alloc(), NSRect::ZERO, options, Some(&view), None) };
    view.addTrackingArea(&area);

    let link = CAMetalDisplayLink::initWithMetalLayer(CAMetalDisplayLink::alloc(), &layer);
    link.setDelegate(Some(ProtocolObject::from_ref(&*view)));
    link.setPreferredFrameRateRange(CAFrameRateRange { minimum: 60.0, maximum: 120.0, preferred: 120.0 });
    link.setPreferredFrameLatency(1.0);
    // SAFETY: the main run loop and the common modes (frames keep coming during live resize and tracking).
    unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };

    let mut app = App::new(wake_from_any_thread())?;
    if let Some(sid) = SELECT.with(std::cell::Cell::get) {
        app.select_session(sid);
    }
    STATE.with(|s| *s.borrow_mut() = Some(State { app, renderer, layer, link, window: window.clone(), last: None, title: String::new() }));
    window.setContentView(Some(&view));
    // The view hears the window's key changes (focus for the cursor).
    let center = objc2_foundation::NSNotificationCenter::defaultCenter();
    // SAFETY: the view outlives the window's notifications (both live until the app quits); the selectors take one
    // NSNotification.
    unsafe {
        center.addObserver_selector_name_object(&view, sel!(windowDidBecomeKey:), Some(objc2_app_kit::NSWindowDidBecomeKeyNotification), Some(&window));
        center.addObserver_selector_name_object(&view, sel!(windowDidResignKey:), Some(objc2_app_kit::NSWindowDidResignKeyNotification), Some(&window));
    }
    window.center();
    window.makeFirstResponder(Some(&view));
    window.makeKeyAndOrderFront(None);
    view.sync_size();
    Ok(())
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HarnessAppDelegate"]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _note: &NSNotification) {
            let mtm = self.mtm();
            if let Err(e) = make_window(mtm) {
                eprintln!("harness-native: {e}");
                std::process::exit(1);
            }
            NSApplication::sharedApplication(mtm).activate();
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _note: &NSNotification) {
            single::release();
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn terminate_after_last_window(&self, _app: &NSApplication) -> bool {
            true
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: init is NSObject's designated initialiser.
        unsafe { msg_send![super(this), init] }
    }
}

fn item(mtm: MainThreadMarker, title: &str, action: objc2::runtime::Sel, key: &str) -> Retained<NSMenuItem> {
    // SAFETY: a fresh item with a valid selector; the target is the responder chain.
    unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str(title), Some(action), &NSString::from_str(key)) }
}

fn submenu(mtm: MainThreadMarker, title: &str, items: &[Retained<NSMenuItem>]) -> Retained<NSMenuItem> {
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
    for i in items {
        menu.addItem(i);
    }
    let holder = NSMenuItem::new(mtm);
    holder.setTitle(&NSString::from_str(title));
    holder.setSubmenu(Some(&menu));
    holder
}

fn main_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let bar = NSMenu::new(mtm);
    bar.addItem(&submenu(mtm, "harness", &[item(mtm, "Hide harness", sel!(hide:), "h"), item(mtm, "Quit harness", sel!(terminate:), "q")]));
    bar.addItem(&submenu(mtm, "Edit", &[item(mtm, "Copy", sel!(copy:), "c"), item(mtm, "Paste", sel!(paste:), "v")]));
    bar.addItem(&submenu(mtm, "Window", &[item(mtm, "Minimize", sel!(performMiniaturize:), "m")]));
    bar
}

/// Another launch asked for the window (and maybe a session): bring it forward on the main thread.
fn asked(ask: Ask) {
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else { return };
        with(|st| {
            if let Ask::Select(sid) = ask {
                st.app.select_session(sid);
            }
            st.app.poke();
            st.link.setPaused(false);
            if st.window.isMiniaturized() {
                st.window.deminiaturize(None);
            }
            st.window.makeKeyAndOrderFront(None);
        });
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    });
}

/// The app, holding the single-instance `listener` (when it has one) and showing session `select` first.
pub fn run(listener: Option<std::os::unix::net::UnixListener>, select: Option<u64>) {
    let mtm = MainThreadMarker::new().expect("harness-native runs on the main thread");
    SELECT.with(|s| s.set(select));
    if let Some(l) = listener {
        single::serve(l, asked);
    }
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.setMainMenu(Some(&main_menu(mtm)));
    let delegate = AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    DELEGATE.with(|d| {
        let _ = d.set(delegate.clone());
    });
    app.run();
}
