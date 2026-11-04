//! The window: raw AppKit (NSApplication, one NSWindow, one flipped layer-hosting NSView over a CAMetalLayer), after
//! den-native's window.rs. Frames come from a `CAMetalDisplayLink`, paused whenever the app is idle; the daemon's threads
//! wake it through the main queue.
use crate::app::{App, Chrome, KeyMods};
use crate::term::{self, Button, KeyAction};
use crate::gpu::{self, Dest, Renderer};
use crate::single::{self, Ask};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSAutoresizingMaskOptions, NSBackingStoreType, NSColor, NSEvent, NSEventModifierFlags, NSMenu, NSMenuItem,
    NSPasteboard, NSPasteboardTypeString, NSResponder, NSTextInputClient, NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow, NSWindowButton, NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_core_foundation::CGSize;
use objc2_core_graphics::{CGColorSpace, kCGColorSpaceExtendedSRGB};
use objc2_foundation::{
    NSArray, NSAttributedString, NSAttributedStringKey, NSNotFound, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRange, NSRangePointer, NSRect, NSRunLoop,
    NSRunLoopCommonModes, NSSize, NSString, NSUInteger,
};
use objc2_metal::MTLCreateSystemDefaultDevice;
use objc2_quartz_core::{CAFrameRateRange, CAMetalDisplayLink, CAMetalDisplayLinkDelegate, CAMetalDisplayLinkUpdate, CAMetalLayer, kCAGravityTopLeft};
use std::cell::{OnceCell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const INITIAL: (f64, f64) = (1180.0, 760.0);
/// From the zoom button's right edge to the title's first ink, as Ghostty's title bar sets it (measured on Tahoe).
const TITLE_GAP: f64 = 16.0;

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
    /// The key event the input method is interpreting, and whether text was being composed when it came: the
    /// NSTextInputClient calls during `interpretKeyEvents` take it when they consume it.
    static IME_KEY: RefCell<Option<(Retained<NSEvent>, bool)>> = const { RefCell::new(None) };
    /// The session `HARNESS_NATIVE_SELECT` asked for, for the window to show once it is made.
    static SELECT: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

fn composing() -> bool {
    with(|st| st.app.marked().is_some()).unwrap_or(false)
}

/// The string an NSTextInputClient call carries (an NSString or an NSAttributedString).
fn text_of(obj: &AnyObject) -> String {
    if let Some(a) = obj.downcast_ref::<NSAttributedString>() {
        a.string().to_string()
    } else if let Some(s) = obj.downcast_ref::<NSString>() {
        s.to_string()
    } else {
        String::new()
    }
}

fn not_found() -> NSRange {
    NSRange::new(NSNotFound as NSUInteger, 0)
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
            let m = mods_of(event.modifierFlags());
            let was = composing();
            // Control, Option (as Meta), Command and the function keys go straight to the encoder; typing goes
            // through the input method (dead keys, IMEs), which calls back into NSTextInputClient below.
            if !was && (m.ctrl || m.alt || m.cmd || term::is_function_key(event.keyCode())) {
                self.send_key(event, None, if event.isARepeat() { KeyAction::Repeat } else { KeyAction::Press });
            } else {
                IME_KEY.with(|k| *k.borrow_mut() = Some((event.retain(), was)));
                self.interpretKeyEvents(&NSArray::from_slice(&[event]));
                // No text and no preedit came of it (return, tab, backspace, escape): the key itself, unless the
                // input method was composing and so owned it.
                if let Some((ev, was)) = IME_KEY.with(|k| k.borrow_mut().take())
                    && !was
                    && !composing()
                {
                    self.send_key(&ev, None, if ev.isARepeat() { KeyAction::Repeat } else { KeyAction::Press });
                }
            }
            wake();
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            if !composing() {
                self.send_key(event, None, KeyAction::Release);
            }
        }

        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            let m = mods_of(event.modifierFlags());
            let key = event.charactersIgnoringModifiers().map(|s| s.to_string()).unwrap_or_default();
            // Control-Command-Space: the emoji picker, which types through insertText.
            let picker = m.cmd && m.ctrl && key == " ";
            if picker {
                NSApplication::sharedApplication(self.mtm()).orderFrontCharacterPalette(None);
            }
            let took = picker || (m.cmd && !m.ctrl && !matches!(key.as_str(), "c" | "v" | "q" | "h" | "m") && with(|st| st.app.shortcut(&key, event.keyCode(), m)).unwrap_or(false));
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
            self.pointer_down(event, Button::Left);
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            self.pointer_down(event, Button::Right);
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            self.pointer_down(event, Button::Middle);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.pointer_up(event, Button::Left);
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            self.pointer_up(event, Button::Right);
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            self.pointer_up(event, Button::Middle);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.pointer_move(event);
        }

        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            self.pointer_move(event);
        }

        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            self.pointer_move(event);
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.pointer_move(event);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            with(|st| st.app.pointer_move(-1.0e4, -1.0e4, KeyMods::default()));
            wake();
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let precise = event.hasPreciseScrollingDeltas();
            let dy = event.scrollingDeltaY() as f32;
            let m = mods_of(event.modifierFlags());
            with(|st| {
                let z = st.app.zoom();
                st.app.wheel(if precise { dy / z } else { dy }, precise, m);
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

        #[unsafe(method(ctxUnarchive:))]
        fn ctx_unarchive(&self, sender: &NSMenuItem) {
            let e = sender.tag() as u64;
            with(|st| st.app.unarchive(e));
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

        /// The window resized or went in or out of fullscreen: AppKit has laid the title bar out again.
        #[unsafe(method(windowChromeChanged:))]
        fn chrome_changed(&self, _note: &NSNotification) {
            self.sync_chrome();
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

    // The input method's client: preedit (marked) text, committed text (IMEs, dead keys, the emoji picker, dictation)
    // and where the candidate window goes. The terminal has no text storage to offer, so ranges are the preedit's.
    unsafe impl NSTextInputClient for HarnessView {
        #[unsafe(method(insertText:replacementRange:))]
        fn insert_text(&self, string: &AnyObject, _replacement: NSRange) {
            let text = text_of(string);
            match IME_KEY.with(|k| k.borrow_mut().take()) {
                // A plain keystroke that typed its own text: encode it as that key (the kitty protocol may want it).
                Some((ev, false)) if !composing() => self.send_key(&ev, Some(&text), if ev.isARepeat() { KeyAction::Repeat } else { KeyAction::Press }),
                _ => {
                    with(|st| st.app.commit_text(&text));
                }
            }
            wake();
        }

        #[unsafe(method(doCommandBySelector:))]
        fn do_command(&self, _selector: objc2::runtime::Sel) {
            // Left for keyDown: the key event itself is encoded once interpretKeyEvents returns.
        }

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        fn set_marked_text(&self, string: &AnyObject, _selected: NSRange, _replacement: NSRange) {
            IME_KEY.with(|k| k.borrow_mut().take());
            let text = text_of(string);
            with(|st| st.app.set_marked(Some(text)));
            wake();
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            with(|st| st.app.set_marked(None));
            wake();
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> NSRange {
            not_found()
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> NSRange {
            with(|st| st.app.marked().map(|m| NSRange::new(0, m.encode_utf16().count()))).flatten().unwrap_or_else(not_found)
        }

        #[unsafe(method(hasMarkedText))]
        fn has_marked_text(&self) -> bool {
            composing()
        }

        #[unsafe(method_id(attributedSubstringForProposedRange:actualRange:))]
        fn attributed_substring(&self, _range: NSRange, _actual: NSRangePointer) -> Option<Retained<NSAttributedString>> {
            None
        }

        #[unsafe(method_id(validAttributesForMarkedText))]
        fn valid_attributes(&self) -> Retained<NSArray<NSAttributedStringKey>> {
            NSArray::new()
        }

        #[unsafe(method(firstRectForCharacterRange:actualRange:))]
        fn first_rect(&self, _range: NSRange, _actual: NSRangePointer) -> NSRect {
            let [x, y, w, h] = with(|st| st.app.caret_rect()).flatten().unwrap_or_default();
            let local = NSRect::new(NSPoint::new(f64::from(x), f64::from(y)), NSSize::new(f64::from(w), f64::from(h)));
            let in_window = self.convertRect_toView(local, None);
            self.window().map_or(NSRect::ZERO, |w| w.convertRectToScreen(in_window))
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index(&self, _point: NSPoint) -> NSUInteger {
            NSNotFound as NSUInteger
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

    /// Hand key `event` to the app: `text` what the input method typed for it, else the event's own characters
    /// (with Option as Meta, the key's character without Option's symbol).
    fn send_key(&self, event: &NSEvent, text: Option<&str>, action: KeyAction) {
        let m = mods_of(event.modifierFlags());
        let text = text.map(str::to_owned).unwrap_or_else(|| {
            let typed = if m.alt { event.charactersIgnoringModifiers() } else { event.characters() };
            typed.map(|s| s.to_string()).unwrap_or_default()
        });
        let unshifted = event.charactersByApplyingModifiers(NSEventModifierFlags::empty()).and_then(|s| s.to_string().chars().next());
        let k = term::KeyIn { code: event.keyCode(), text: &text, unshifted, mods: m, action, composing: false };
        with(|st| st.app.key(&k));
        wake();
    }

    fn pointer_down(&self, event: &NSEvent, b: Button) {
        let p = self.local(event);
        let m = mods_of(event.modifierFlags());
        with(|st| st.app.pointer_down(p.x as f32, p.y as f32, b, event.clickCount(), m));
        wake();
    }

    fn pointer_up(&self, event: &NSEvent, b: Button) {
        let p = self.local(event);
        let m = mods_of(event.modifierFlags());
        with(|st| st.app.pointer_up(p.x as f32, p.y as f32, b, m));
        wake();
    }

    fn pointer_move(&self, event: &NSEvent) {
        let p = self.local(event);
        let m = mods_of(event.modifierFlags());
        with(|st| st.app.pointer_move(p.x as f32, p.y as f32, m));
        wake();
    }

    fn context_menu(&self, event: &NSEvent) -> Option<Retained<NSMenu>> {
        let p = self.local(event);
        let (x, y) = (p.x as f32, p.y as f32);
        let mtm = self.mtm();
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let add = |title: &str, action: objc2::runtime::Sel, tag: u64| {
            let it = item(mtm, title, action, "");
            // SAFETY: the view outlives the menu it pops up; the item only holds it weakly.
            unsafe { it.setTarget(Some(self)) };
            it.setTag(tag as isize);
            menu.addItem(&it);
        };
        let e = with(|st| st.app.object_at(x, y)).flatten()?;
        if with(|st| st.app.is_archived(e)).unwrap_or(false) {
            add("Unarchive", sel!(ctxUnarchive:), e);
        } else {
            add("Kill", sel!(ctxKill:), e);
        }
        Some(menu)
    }

    /// Tell the app where the title bar is (Ghostty's layout: the traffic lights centred in a strip as tall as AppKit's
    /// title bar, the title past them on the same line), re-centring the lights when AppKit has put them elsewhere.
    fn sync_chrome(&self) {
        with(|st| {
            let c = chrome(&st.window);
            st.app.set_chrome(c);
            st.link.setPaused(false);
        });
    }

    fn sync_size(&self) {
        self.sync_chrome();
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

/// The title-bar strip of `window` (none in fullscreen, where AppKit hides it), its traffic lights moved to its centre line.
fn chrome(window: &NSWindow) -> Chrome {
    if window.styleMask().contains(NSWindowStyleMask::FullScreen) {
        return Chrome::default();
    }
    let height = window.frame().size.height;
    let strip = (height - window.contentLayoutRect().size.height).max(0.0);
    let mut right: f64 = 0.0;
    for kind in [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton] {
        let Some(b) = window.standardWindowButton(kind) else { continue };
        // In window coordinates: origin at the bottom left, `height` tall.
        let r = b.convertRect_toView(b.bounds(), None);
        let centre = height - (r.origin.y + r.size.height / 2.0);
        let d = strip / 2.0 - centre;
        if d.abs() > 0.25 {
            let mut o = b.frame().origin;
            // SAFETY: a button in the window's title bar has a superview while the window lives (we hold it).
            let flipped = unsafe { b.superview() }.is_some_and(|v| v.isFlipped());
            o.y += if flipped { d } else { -d };
            b.setFrameOrigin(o);
        }
        right = right.max(r.origin.x + r.size.width);
    }
    if std::env::var_os("HARNESS_NATIVE_CHROME_LOG").is_some() {
        eprintln!("harness-native: chrome strip={strip} lights_right={right}");
    }
    Chrome { strip: strip as f32, title_x: if right > 0.0 { (right + TITLE_GAP) as f32 } else { 0.0 } }
}

/// `HARNESS_NATIVE_CHROME_TEST`: resize this app's own window (no focus taken), logging the title bar after it, so a
/// capture after about 3 s shows the title bar as AppKit lays it out again. Fullscreen is not exercised here: it takes
/// a Space (and the screen); `windowChromeChanged:` handles its exit the same way.
fn chrome_test(_window: &NSWindow) {
    fn later(secs: f64, f: fn(&NSWindow)) {
        let when = dispatch2::DispatchTime::try_from(std::time::Duration::from_secs_f64(secs)).expect("a short delay");
        let _ = DispatchQueue::main().after(when, move || {
            let w = with(|st| st.window.clone());
            if let Some(w) = w {
                f(&w);
                eprintln!("harness-native: chrome test step at {secs}s: {:?}", chrome(&w));
            }
        });
    }
    later(2.0, |w| {
        let mut f = w.frame();
        f.size = NSSize::new(980.0, 640.0);
        w.setFrame_display(f, true);
    });
}

/// `HARNESS_NATIVE_NO_ACTIVATE`: show the window behind whatever has focus, without activating the app or taking the
/// keyboard (for captures while someone is using the Mac).
fn no_activate() -> bool {
    std::env::var_os("HARNESS_NATIVE_NO_ACTIVATE").is_some()
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
        // AppKit lays the title bar (and the traffic lights) out again after these.
        for name in [objc2_app_kit::NSWindowDidResizeNotification, objc2_app_kit::NSWindowDidEnterFullScreenNotification, objc2_app_kit::NSWindowDidExitFullScreenNotification] {
            center.addObserver_selector_name_object(&view, sel!(windowChromeChanged:), Some(name), Some(&window));
        }
    }
    window.center();
    window.makeFirstResponder(Some(&view));
    if no_activate() {
        window.orderBack(None);
    } else {
        window.makeKeyAndOrderFront(None);
    }
    view.sync_size();
    if std::env::var_os("HARNESS_NATIVE_CHROME_TEST").is_some() {
        chrome_test(&window);
    }
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
            if !no_activate() {
                NSApplication::sharedApplication(mtm).activate();
            }
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
    app.setActivationPolicy(if no_activate() { NSApplicationActivationPolicy::Accessory } else { NSApplicationActivationPolicy::Regular });
    app.setMainMenu(Some(&main_menu(mtm)));
    let delegate = AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    DELEGATE.with(|d| {
        let _ = d.set(delegate.clone());
    });
    app.run();
}
