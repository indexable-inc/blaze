//! The window: raw AppKit (NSApplication, one NSWindow, one flipped layer-hosting NSView over a CAMetalLayer), after
//! den-native's window.rs. Frames come from a `CAMetalDisplayLink`, paused whenever the app is idle; the daemon's threads
//! wake it through the main queue.
use crate::app::{App, Chrome, KeyMods, MenuLine, Outcome};
use crate::gpu::{self, Dest, Renderer};
use crate::single::{self, Ask};
use crate::term::{self, Button, KeyAction};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSAutoresizingMaskOptions,
    NSBackingStoreType, NSColor, NSColorPanel, NSColorSpace, NSCursor, NSDragOperation, NSDraggingInfo, NSEvent, NSEventModifierFlags, NSMenu, NSMenuItem, NSPasteboard,
    NSPasteboardTypeFileURL, NSPasteboardTypeString, NSResponder, NSTextInputClient, NSTrackingArea, NSTrackingAreaOptions,
    NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState,
    NSVisualEffectView, NSWindow, NSWindowButton, NSWindowStyleMask, NSWindowTitleVisibility,
};
use harness_native_render::hdr::Headroom;
use harness_native_ui_input_api::Pointer;
use objc2_core_foundation::CGSize;
use objc2_core_graphics::{CGColorSpace, kCGColorSpaceExtendedSRGB};
use objc2_foundation::{
    NSArray, NSAttributedString, NSAttributedStringKey, NSNotFound, NSNotification, NSObject,
    NSObjectProtocol, NSPoint, NSRange, NSRangePointer, NSRect, NSRunLoop, NSRunLoopCommonModes,
    NSSize, NSString, NSUInteger,
};
use objc2_metal::MTLCreateSystemDefaultDevice;
use objc2_quartz_core::{
    CAFrameRateRange, CAMetalDisplayLink, CAMetalDisplayLinkDelegate, CAMetalDisplayLinkUpdate,
    CAMetalLayer, kCAGravityTopLeft,
};
use std::cell::{OnceCell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const INITIAL: (f64, f64) = (1180.0, 760.0);
/// Frame rates: while anything moves, and while only a breath moves.
const LIVE: CAFrameRateRange = CAFrameRateRange { minimum: 60.0, maximum: 120.0, preferred: 120.0 };
const CALM: CAFrameRateRange = CAFrameRateRange { minimum: 20.0, maximum: 30.0, preferred: 30.0 };
/// From the zoom button's right edge to the title's first ink, as Ghostty's title bar sets it (measured on Tahoe).
const TITLE_GAP: f64 = 16.0;

struct State {
    app: App,
    renderer: Renderer,
    layer: Retained<CAMetalLayer>,
    link: Retained<CAMetalDisplayLink>,
    window: Retained<NSWindow>,
    last: Option<f64>,
    /// The view's size in points and its backing scale at the last frame drawn (what a step out of sight lays out for).
    points: (f32, f32),
    backing: f32,
    title: String,
    /// The traffic lights' opacity as last set (they fade with the sidebar's fold).
    lights: f32,
    tip: String,
    /// Drawing at the calm rate: only a breath moves.
    calm: bool,
    /// The keys' load the menu bar was made from (made again when the user's keys.flecs changes).
    menu_gen: u64,
    /// The system pointer's shape as this app last set it (None: AppKit or another window has it since).
    cursor: Option<Pointer>,
    /// The pointer is over the view in the key window (between mouseEntered/mouseMoved and mouseExited or resigning key): only
    /// then does the app set the system pointer, so it never takes it from the window's resize edges or another window.
    inside: bool,
    /// Where the window rests while the sidebar's over-drag pulls it aside (`App::window_shift`).
    rest: Option<NSPoint>,
    /// The windows the open menus, popovers and hover cards float in (overlay.rs).
    overlays: overlay::Overlays,
    /// Apple's maps over the frame's Map cards (maps.rs).
    maps: maps::Maps,
    /// The window is tucked away: its turn's item floating at the screen's corner (mini.rs).
    mini: Option<mini::Mini>,
}

/// Tuck the window away for chat `sid`: it is ordered out and the chat's item floats until a press on it (or the Dock
/// icon) brings the window back ([`untuck`]).
fn tuck(st: &mut State, sid: u64) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    if let Some(old) = st.mini.take() {
        old.close();
    }
    let block = block2::RcBlock::new(move |_: std::ptr::NonNull<objc2_foundation::NSTimer>| {
        let now = objc2_quartz_core::CACurrentMediaTime();
        with(|st| {
            let Some(m) = st.mini.as_mut() else { return };
            let (face, running) = st.app.tucked(m.sid);
            m.step(now, face, running);
        });
    });
    // SAFETY: the block runs on the main thread (a main-run-loop timer) and touches only main-thread state.
    let timer = unsafe { objc2_foundation::NSTimer::timerWithTimeInterval_repeats_block(1.0 / 12.0, true, &block) };
    // SAFETY: the main run loop, common modes.
    unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    st.mini = Some(mini::Mini::show(mtm, &st.window, sid, timer, objc2_quartz_core::CACurrentMediaTime()));
    st.window.orderOut(None);
}

/// Bring a tucked-away window back, on the chat it was tucked for. Nothing when it is not tucked.
fn untuck() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let back = with(|st| {
        let Some(m) = st.mini.take() else { return false };
        let sid = m.sid;
        m.close();
        st.app.select_session(sid);
        st.app.poke();
        st.link.setPaused(false);
        st.window.makeKeyAndOrderFront(None);
        true
    });
    if back == Some(true) {
        // The person pressed the item (or the Dock icon): the app comes to the front with its window.
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    }
}

mod maps;
mod mini;
mod overlay;

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

/// The paths of the files a drag carries (den-native window.rs).
fn dragged_paths(pb: &NSPasteboard) -> Vec<String> {
    let items = pb.pasteboardItems().map(|items| items.to_vec()).unwrap_or_default();
    // SAFETY: NSPasteboardTypeFileURL is a valid static pasteboard type.
    let file = unsafe { NSPasteboardTypeFileURL };
    items
        .iter()
        .filter_map(|it| it.stringForType(file).and_then(|u| objc2_foundation::NSURL::URLWithString(&u)))
        .filter(|u| u.isFileURL())
        .filter_map(|u| u.path())
        .map(|s| s.to_string())
        .collect()
}

fn not_found() -> NSRange {
    NSRange::new(NSNotFound as NSUInteger, 0)
}

/// Run `f` on the window's state, unless it is in use (an event during a frame).
fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn wake() {
    if UNSEEN.load(Ordering::SeqCst) {
        if ASLEEP.swap(false, Ordering::SeqCst) {
            step_unseen_soon();
        }
        return;
    }
    ASLEEP.store(false, Ordering::SeqCst);
    with(|st| st.link.setPaused(false));
}

/// The window is out of sight (covered by others, miniaturized, on another Space, the screen locked or asleep): the
/// display link stops calling, so no frame would take the daemon's messages, and they would pile up in the mailbox for
/// as long as it stays hidden (terminal output, chat events). Out of sight the link is paused and each wake steps the
/// app without drawing ([`step_unseen`]) instead, so the world, terminals and transcripts stay current and nothing
/// waits longer than [`UNSEEN_STEP`].
static UNSEEN: AtomicBool = AtomicBool::new(false);

/// How long a wake out of sight waits before its step, so a burst of messages is one step.
const UNSEEN_STEP: std::time::Duration = std::time::Duration::from_millis(100);

fn step_unseen_soon() {
    let when = dispatch2::DispatchTime::try_from(UNSEEN_STEP).expect("a short delay");
    let _ = DispatchQueue::main().after(when, step_unseen);
}

/// One step of the app with nothing drawn, out of sight: it takes the daemon's messages as a frame would. Then asleep
/// (the next message wakes it), or another step when messages came during this one.
fn step_unseen() {
    if !UNSEEN.load(Ordering::SeqCst) {
        // Back in sight since this was asked for: frames take the messages.
        wake();
        return;
    }
    let seen = POSTED.load(Ordering::SeqCst);
    let stepped = with(|st| {
        st.link.setPaused(true);
        let now = objc2_quartz_core::CACurrentMediaTime();
        let dt = st.last.map_or(1.0 / 120.0, |l| (now - l).clamp(0.0, 0.1)) as f32;
        st.last = Some(now);
        let renderer = &st.renderer;
        let _ = st.app.frame(st.points, st.backing, dt, &mut |x, y, w, h, b| renderer.upload_atlas(x, y, w, h, b));
    })
    .is_some();
    ASLEEP.store(true, Ordering::SeqCst);
    // A message posted during the step, or a step that could not run (the state was in use): step again.
    if (!stepped || POSTED.load(Ordering::SeqCst) != seen) && ASLEEP.swap(false, Ordering::SeqCst) {
        step_unseen_soon();
    }
}


/// The display link is paused (the app went idle): the next daemon message must wake it. While it runs, each frame
/// drains the daemon's messages itself, so the daemon's threads leave the main thread alone.
static ASLEEP: AtomicBool = AtomicBool::new(false);
/// Daemon messages posted so far (a frame that would pause checks none came since it began draining).
static POSTED: AtomicU64 = AtomicU64::new(0);

/// A wake the daemon's threads call after each message: it reaches the main thread only when the display link sleeps,
/// once per sleep. Messages that come while frames run are drained by the next frame (at most one apply a vsync).
fn wake_from_any_thread() -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(move || {
        POSTED.fetch_add(1, Ordering::SeqCst);
        if !ASLEEP.swap(false, Ordering::SeqCst) {
            return;
        }
        if UNSEEN.load(Ordering::SeqCst) {
            step_unseen_soon();
            return;
        }
        // Only unpause: the frame drains the message and draws on if it changed anything (`daemon`), else sleeps again.
        DispatchQueue::main().exec_async(move || {
            with(|st| {
                framelog::wake(
                    st.link.isPaused(),
                    st.window
                        .occlusionState()
                        .contains(objc2_app_kit::NSWindowOcclusionState::Visible),
                );
                st.link.setPaused(false);
            });
        });
    })
}

/// Each frame counts on; a Kip's nap (below) is still due only when no frame came between.
static FRAMES_DRAWN: AtomicU64 = AtomicU64::new(0);

/// The app went idle with the Kips in view at rest: wake in `secs` for the next one's blink or fidget, moving their clocks
/// on by the time slept (den-native's `mascot_rest`), unless a frame was drawn meanwhile (their clocks ran with it).
fn arm_kip_nap(secs: f64) {
    let at = FRAMES_DRAWN.load(Ordering::SeqCst);
    let Ok(when) = dispatch2::DispatchTime::try_from(std::time::Duration::from_secs_f64(secs.clamp(0.05, 60.0))) else {
        return;
    };
    let _ = DispatchQueue::main().after(when, move || {
        if FRAMES_DRAWN.load(Ordering::SeqCst) != at {
            return;
        }
        with(|st| {
            st.app.kip_woke(secs);
            st.link.setPaused(false);
        });
    });
}

/// The app went idle with a finished row lingering in the tree: wake in `secs`, when its linger is out and it folds
/// away, unless a frame was drawn meanwhile (that frame armed the next wake itself, if one is still due).
fn arm_linger(secs: f64) {
    let at = FRAMES_DRAWN.load(Ordering::SeqCst);
    let Ok(when) = dispatch2::DispatchTime::try_from(std::time::Duration::from_secs_f64(secs.clamp(0.02, 60.0))) else {
        return;
    };
    let _ = DispatchQueue::main().after(when, move || {
        if FRAMES_DRAWN.load(Ordering::SeqCst) != at {
            return;
        }
        with(|st| {
            st.app.linger_woke();
            st.link.setPaused(false);
        });
    });
}

/// The headroom of the screen `view`'s window is on (SDR when it is on none). Public NSScreen API: CoreDisplay's private
/// `CoreDisplay_Display_Get{Current,Potential,Reference}Headroom` were measured to return the same numbers (macOS 27,
/// built-in XDR: 1.0, 16.0, 0.0), so they would add nothing (rule macos-private-apis).
fn screen_headroom(view: &NSView) -> Headroom {
    let Some(screen) = view.window().and_then(|w| w.screen()) else { return Headroom::SDR };
    Headroom {
        current: screen.maximumExtendedDynamicRangeColorComponentValue() as f32,
        potential: screen.maximumPotentialExtendedDynamicRangeColorComponentValue() as f32,
        reference: screen.maximumReferenceExtendedDynamicRangeColorComponentValue() as f32,
    }
}

/// How often an idle app on an HDR-capable screen looks at the headroom again: AppKit posts nothing when it moves (with
/// brightness, or as other HDR content comes and goes; no public notification exists and CoreDisplay's private
/// `CDNotificationRegisterCallback` is undocumented), and a drawn frame reads it anyway.
const HEADROOM_WATCH: std::time::Duration = std::time::Duration::from_millis(500);

/// The app went idle on a screen with room past white: look at the headroom once in [`HEADROOM_WATCH`] and draw again
/// only if it moved (the frame re-arms this when it idles again), unless a frame was drawn meanwhile.
fn arm_headroom_watch() {
    let at = FRAMES_DRAWN.load(Ordering::SeqCst);
    let when = dispatch2::DispatchTime::try_from(HEADROOM_WATCH).expect("a short delay");
    let _ = DispatchQueue::main().after(when, move || {
        if FRAMES_DRAWN.load(Ordering::SeqCst) != at {
            return;
        }
        let moved = with(|st| {
            let h = st.window.contentView().map_or(Headroom::SDR, |v| screen_headroom(&v));
            h != st.app.headroom
        });
        match moved {
            Some(true) => {
                with(|st| {
                    st.app.poke();
                    st.link.setPaused(false);
                });
            }
            // Unchanged: keep watching while idle.
            Some(false) => arm_headroom_watch(),
            None => {}
        }
    });
}

/// F3's overlay is shown and the app idle: wake once in its tick (about 4 Hz), no sooner.
fn arm_stats_tick() {
    let when = dispatch2::DispatchTime::try_from(crate::app::STATS_TICK).expect("a short delay");
    let _ = DispatchQueue::main().after(when, || {
        with(|st| {
            st.app.stats_wake();
            st.link.setPaused(false);
        });
    });
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

        /// Light or dark: the scene's fills pick their half, the sidebar's vibrancy follows on its own.
        #[unsafe(method(viewDidChangeEffectiveAppearance))]
        fn did_change_appearance(&self) {
            let dark = is_dark(self);
            with(|st| {
                st.app.dark = dark;
                st.app.poke();
                st.link.setPaused(false);
            });
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
            // A Command key: the bindings (scene/20-keys.flecs) first, then AppKit's menu.
            let out = if m.cmd {
                let unshifted = event.charactersByApplyingModifiers(NSEventModifierFlags::empty()).and_then(|s| s.to_string().chars().next());
                with(|st| st.app.shortcut(event.keyCode(), unshifted, m)).unwrap_or(Outcome::None)
            } else {
                Outcome::None
            };
            match out {
                Outcome::Done => {
                    wake();
                    true
                }
                Outcome::Selector(sel) => {
                    send_selector(self.mtm(), &sel);
                    wake();
                    true
                }
                Outcome::None => {
                    // SAFETY: forwarding the same selector to the superclass with its own argument.
                    let handled: objc2::runtime::Bool = unsafe { msg_send![super(self), performKeyEquivalent: event] };
                    handled.as_bool()
                }
            }
        }

        /// A menu bar item made from an action (its tag the action's entity).
        #[unsafe(method(runAction:))]
        fn run_action(&self, sender: &NSMenuItem) {
            let id = sender.tag() as u64;
            if let Some(Outcome::Selector(sel)) = with(|st| st.app.run_action(id, None)) {
                send_selector(self.mtm(), &sel);
            }
            wake();
        }

        // Files carried in from Finder (den-native window.rs): a generated DropZone under them lights, and takes them when
        // they are let go (genui/dropzone.rs).
        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(&self, info: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            self.files_over(info)
        }

        #[unsafe(method(draggingUpdated:))]
        fn dragging_updated(&self, info: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            self.files_over(info)
        }

        #[unsafe(method(draggingExited:))]
        fn dragging_exited(&self, _info: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
            with(|st| st.app.genui_files_over(None));
            wake();
        }

        #[unsafe(method(performDragOperation:))]
        fn perform_drag_operation(&self, info: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            let p = self.convertPoint_fromView(info.draggingLocation(), None);
            let paths = dragged_paths(&info.draggingPasteboard());
            let took = with(|st| st.app.genui_files_dropped(p.x as f32, p.y as f32, &paths)).unwrap_or(false);
            wake();
            took
        }

        /// The system colour panel's colour changed (a ColorPicker's well opened it, genui/color.rs): its colour in sRGB
        /// goes to the picker.
        #[unsafe(method(genuiColorChanged:))]
        fn genui_color_changed(&self, _sender: Option<&AnyObject>) {
            let panel = NSColorPanel::sharedColorPanel(self.mtm());
            let Some(c) = panel.color().colorUsingColorSpace(&NSColorSpace::sRGBColorSpace()) else {
                eprintln!("harness-native: the colour panel's colour has no sRGB form");
                return;
            };
            let rgb = [c.redComponent(), c.greenComponent(), c.blueComponent()];
            with(|st| st.app.genui_color_panel(rgb));
            wake();
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
            with(|st| st.inside = true);
            self.pointer_move(event);
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, event: &NSEvent) {
            with(|st| st.inside = true);
            self.pointer_move(event);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            with(|st| st.inside = false);
            // A drag that leaves the view goes on (mouseDragged keeps coming): the pointer is where those put it.
            if NSEvent::pressedMouseButtons() != 0 {
                return;
            }
            // From the view into an overlay's window or back (overlay.rs): still over the app, the hover holds (a hover
            // card opened from the view stays open while the pointer goes onto it); the next move there places it.
            if with(|st| st.overlays.holds(&st.window, NSEvent::mouseLocation())).unwrap_or(false) {
                with(|st| st.inside = true);
                return;
            }
            with(|st| st.app.pointer_move(-1.0e4, -1.0e4, KeyMods::default()));
            wake();
        }

        /// AppKit asks for the pointer's shape (the pointer came into the view, the window became key, a live resize
        /// ended): set the one last resolved now, and move the app's pointer here so the next frame resolves it afresh.
        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, event: &NSEvent) {
            with(|st| {
                st.inside = true;
                st.cursor = None;
            });
            self.pointer_move(event);
            with(|st| apply_cursor(st));
        }

        /// Shift (or another modifier) went down or up with no pointer event: Shift over a terminal whose program reports
        /// the mouse selects instead, so the pointer's shape may change.
        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            let m = mods_of(event.modifierFlags());
            with(|st| st.app.set_mods(m));
            wake();
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            // AppKit sends wheel events to the window under the pointer even when it is not key, but the tracking area
            // (ActiveInKeyWindow) sends no moves then, so the app's pointer is stale and the wheel would go to the wrong
            // place or nowhere. Take the pointer from the wheel event itself first.
            with(|st| st.inside = true);
            self.pointer_move(event);
            let ev = crate::app::Wheel {
                dx: event.scrollingDeltaX() as f32,
                dy: event.scrollingDeltaY() as f32,
                precise: event.hasPreciseScrollingDeltas(),
                phase: event.phase().0 as usize,
                momentum: event.momentumPhase().0 as usize,
                inverted: event.isDirectionInvertedFromDevice(),
            };
            let m = mods_of(event.modifierFlags());
            // A swipe over the chat's effort pulses at each stop it steps (chat/pick.rs): play them with the event.
            let pulses = with(|st| {
                st.app.scroll_wheel(ev, m);
                st.app.take_haptics()
            });
            play(&pulses.unwrap_or_default());
            wake();
        }

        #[unsafe(method(copy:))]
        fn copy(&self, _sender: Option<&AnyObject>) {
            if let Some(text) = with(|st| st.app.copy()).flatten() {
                pasteboard_put(&text);
            }
            wake();
        }

        #[unsafe(method(cut:))]
        fn cut(&self, _sender: Option<&AnyObject>) {
            if let Some(text) = with(|st| st.app.cut()).flatten() {
                pasteboard_put(&text);
            }
            wake();
        }

        /// Files on the pasteboard (a Finder copy, which carries their names as a string too) paste as their paths (a
        /// chat attaches the pictures among them), else its string pastes, else its picture (a screenshot, a browser's
        /// Copy Image) is saved and attached to a chat's message, or its path pasted elsewhere.
        #[unsafe(method(paste:))]
        fn paste(&self, _sender: Option<&AnyObject>) {
            let pb = NSPasteboard::generalPasteboard();
            let paths = pasteboard_paths(&pb);
            if paths.is_empty() {
                // SAFETY: NSPasteboardTypeString is a valid static pasteboard type.
                let text = pb.stringForType(unsafe { NSPasteboardTypeString }).map(|s| s.to_string());
                if let Some(text) = text {
                    with(|st| st.app.paste(&text));
                } else {
                    let pngs = pasteboard_pngs(&pb);
                    if !pngs.is_empty() {
                        with(|st| st.app.paste_pictures(&pngs));
                    }
                }
            } else {
                with(|st| st.app.paste_paths(&paths));
            }
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

        /// The system's display options changed (Accessibility, Display): Reduce Motion may have gone on or off.
        #[unsafe(method(motionChanged:))]
        fn motion_changed(&self, _note: &NSNotification) {
            let on = reduce_motion();
            with(|st| {
                st.app.set_reduce_motion(on);
                st.link.setPaused(false);
            });
        }

        /// The window moved to another screen, or a screen changed: the next frame reads its headroom (`screen_headroom`).
        #[unsafe(method(screenChanged:))]
        fn screen_changed(&self, _note: &NSNotification) {
            let headroom = screen_headroom(self);
            with(|st| {
                if headroom != st.app.headroom {
                    st.app.headroom = headroom;
                    st.app.poke();
                    st.link.setPaused(false);
                }
            });
        }

        /// The window came into sight or went out of it ([`UNSEEN`]).
        #[unsafe(method(windowOcclusionChanged:))]
        fn occlusion_changed(&self, _note: &NSNotification) {
            let Some(visible) = with(|st| {
                st.window.occlusionState().contains(objc2_app_kit::NSWindowOcclusionState::Visible)
            }) else {
                return;
            };
            if visible == !UNSEEN.load(Ordering::SeqCst) {
                return;
            }
            if visible {
                UNSEEN.store(false, Ordering::SeqCst);
                with(|st| st.app.poke());
                wake();
            } else {
                UNSEEN.store(true, Ordering::SeqCst);
                ASLEEP.store(false, Ordering::SeqCst);
                step_unseen();
            }
        }

        #[unsafe(method(windowDidResignKey:))]
        fn did_resign_key(&self, _note: &NSNotification) {
            with(|st| {
                // The tracking area sleeps until the window is key again (its cursorUpdate: then gives the pointer back).
                st.inside = false;
                st.cursor = None;
                st.app.focused = false;
                // Another app or window took the keyboard: open menus and popovers close, as NSMenu and NSPopover do.
                st.app.close_floats();
                st.app.poke();
            });
            wake();
        }

        /// The window moved (to another screen, or across one): the room the overlays are kept in moves with it.
        #[unsafe(method(windowMoved:))]
        fn window_moved(&self, _note: &NSNotification) {
            let changed = with(|st| {
                let Some(room) = overlay::screen_room(&st.window, self) else { return false };
                st.app.set_screen(room);
                true
            });
            if changed == Some(true) {
                wake();
            }
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

    /// Files carried over the view: a copy when a generated DropZone is under them, else nothing.
    fn files_over(&self, info: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
        let p = self.convertPoint_fromView(info.draggingLocation(), None);
        let takes = with(|st| st.app.genui_files_over(Some((p.x as f32, p.y as f32)))).unwrap_or(false);
        wake();
        if takes { NSDragOperation::Copy } else { NSDragOperation::None }
    }

    /// Open the system colour panel on `rgb` (sRGB, 0 to 1) for a ColorPicker's well, its picks sent to
    /// `genuiColorChanged:`. Only on the user's press (the app asks after a frame that ran one), never in a shot.
    fn open_color_panel(&self, rgb: [f64; 3]) {
        let panel = NSColorPanel::sharedColorPanel(self.mtm());
        panel.setShowsAlpha(false);
        // Its colour first, so setting it is not sent back as a pick.
        panel.setColor(&NSColor::colorWithSRGBRed_green_blue_alpha(rgb[0], rgb[1], rgb[2], 1.0));
        let target: &AnyObject = self;
        // SAFETY: the view lives as long as the window (the app's), and genuiColorChanged: is its method above.
        unsafe {
            panel.setTarget(Some(target));
            panel.setAction(Some(sel!(genuiColorChanged:)));
        }
        panel.setContinuous(true);
        panel.orderFront(None);
    }

    /// Where `event` is in the view's points: from the main window, or from an overlay's window (overlay.rs hands its
    /// events on here), whose points go through the screen's.
    fn local(&self, event: &NSEvent) -> NSPoint {
        overlay::local(self, event)
    }

    /// Hand key `event` to the app: `text` what the input method typed for it, else the event's own characters
    /// (with Option as Meta, the key's character without Option's symbol).
    fn send_key(&self, event: &NSEvent, text: Option<&str>, action: KeyAction) {
        let m = mods_of(event.modifierFlags());
        let text = text.map(str::to_owned).unwrap_or_else(|| {
            let typed =
                if m.alt { event.charactersIgnoringModifiers() } else { event.characters() };
            typed.map(|s| s.to_string()).unwrap_or_default()
        });
        let unshifted = event
            .charactersByApplyingModifiers(NSEventModifierFlags::empty())
            .and_then(|s| s.to_string().chars().next());
        let k = term::KeyIn {
            code: event.keyCode(),
            text: &text,
            unshifted,
            mods: m,
            action,
            composing: false,
        };
        // A Command key that reached keyDown (no key equivalent took it) can still be bound to a selector (`copy:`).
        if let Some(crate::app::Outcome::Selector(sel)) = with(|st| st.app.key(&k)) {
            send_selector(self.mtm(), &sel);
        }
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
        let pulses = with(|st| {
            st.app.pointer_move(p.x as f32, p.y as f32, m);
            st.app.take_haptics()
        });
        play(&pulses.unwrap_or_default());
        wake();
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
        let entered = objc2_quartz_core::CACurrentMediaTime();
        self.frame_into(
            Dest::Drawable(update.drawable()),
            entered,
            update.targetTimestamp(),
            update.targetPresentationTimestamp(),
        );
    }

    /// One frame drawn into `dest`, for the display time `t` (presented at `present`); `entered` when the clock called.
    fn frame_into(&self, dest: Dest, entered: f64, t: f64, present: f64) {
        // Before this frame drains the daemon's messages: any posted after it may not have been drained.
        let seen = POSTED.load(Ordering::SeqCst);
        FRAMES_DRAWN.fetch_add(1, Ordering::SeqCst);
        let points = self.bounds().size;
        let backing = self.convertSizeToBacking(points).width / points.width.max(1.0);
        // The keys were loaded again (the user's keys.flecs changed): the menu bar follows.
        let menu = with(|st| {
            (st.app.keys_gen() != st.menu_gen).then(|| {
                st.menu_gen = st.app.keys_gen();
                st.app.menu()
            })
        })
        .flatten();
        if let Some(m) = menu {
            NSApplication::sharedApplication(self.mtm()).setMainMenu(Some(&main_menu(
                self.mtm(),
                &m,
                Some(self),
            )));
        }
        let headroom = screen_headroom(self);
        with(|st| {
            // The headroom moves with brightness and the screen: a change redraws (HDR pictures and the fire follow it).
            if headroom != st.app.headroom {
                st.app.headroom = headroom;
                st.app.poke();
            }
            let dt = st.last.map_or(1.0 / 120.0, |l| (t - l).clamp(0.0, 0.1)) as f32;
            st.last = Some(t);
            (st.points, st.backing) = ((points.width as f32, points.height as f32), backing as f32);
            // The screen the windowed overlays are kept on, before layout places them.
            if let Some(room) = overlay::screen_room(&st.window, self) {
                st.app.set_screen(room);
            }
            let renderer = &st.renderer;
            let draw = st.app.frame(
                (points.width as f32, points.height as f32),
                backing as f32,
                dt,
                &mut |x, y, w, h, b| renderer.upload_atlas(x, y, w, h, b),
            );
            // The live line's tuck button: the window goes away until its turn is done (mini.rs).
            if let Some(sid) = st.app.take_tuck() {
                tuck(st, sid);
            }
            let built = objc2_quartz_core::CACurrentMediaTime();
            // The open overlays into their own windows, drawn in the same command buffer as the window.
            let floats = st.app.take_floats();
            let dests = st.overlays.prepare(&st.window, &floats, st.app.zoom(), backing as f32);
            let drawn: Vec<bool> = dests.iter().map(Option::is_some).collect(); // alloc: a flag per open overlay
            let mut passes = Vec::with_capacity(1 + floats.len()); // alloc: the window's pass and one per open overlay
            passes.push(gpu::Pass { draw: &draw, dest, over: false });
            for (f, d) in floats.iter().zip(dests) {
                if let Some(dest) = d {
                    passes.push(gpu::Pass { draw: &f.draw, dest, over: false });
                }
            }
            st.renderer.render_passes(&passes);
            drop(passes);
            let tip = st.app.tooltip();
            st.overlays.shown(&st.window, &floats, &drawn, &tip);
            // The Map cards the frame painted, each under its live map (maps.rs).
            let cards = st.app.take_maps();
            st.maps.sync(&st.window, &cards, st.app.zoom());
            framelog::note(
                entered,
                t,
                present,
                built,
                objc2_quartz_core::CACurrentMediaTime(),
                f32::from_bits(st.renderer.gpu_clock().load(Ordering::Relaxed)),
            );
            if st.app.title != st.title {
                st.title.clone_from(&st.app.title);
                st.window.setTitle(&NSString::from_str(&st.title));
            }
            apply_cursor(st);
            shift_window(st);
            if tip != st.tip {
                st.tip = tip;
                self.setToolTip(
                    (!st.tip.is_empty()).then(|| NSString::from_str(&st.tip)).as_deref(),
                );
            }
            let lights = st.app.lights();
            if (lights - st.lights).abs() > 1e-3 || (lights == 0.0) != (st.lights == 0.0) {
                st.lights = lights;
                set_lights(&st.window, lights);
            }
            match (st.app.idle(), st.app.breathing()) {
                // Only a breath moves: keep drawing, at a calm rate.
                (true, true) if !st.calm => {
                    st.calm = true;
                    st.link.setPreferredFrameRateRange(CALM);
                }
                (true, true) => {}
                (true, false) => {
                    // Asleep first, then a last look: a message posted since this frame began either shows here (draw
                    // once more) or finds ASLEEP set and wakes the link itself.
                    ASLEEP.store(true, Ordering::SeqCst);
                    if POSTED.load(Ordering::SeqCst) != seen {
                        ASLEEP.store(false, Ordering::SeqCst);
                        return;
                    }
                    st.link.setPaused(true);
                    st.last = None;
                    if st.app.stats_arm() {
                        arm_stats_tick();
                    }
                    if let Some(secs) = st.app.kip_nap() {
                        arm_kip_nap(secs);
                    }
                    if let Some(secs) = st.app.linger_due() {
                        arm_linger(secs);
                    }
                    // A pause in typing: the chats' drafts are kept for the next launch.
                    st.app.flush_drafts();
                    if st.app.headroom.potential > 1.0 {
                        arm_headroom_watch();
                    }
                }
                (false, _) if st.calm => {
                    st.calm = false;
                    st.link.setPreferredFrameRateRange(LIVE);
                }
                (false, _) => {}
            }
        });
        // A press on a ColorPicker's well this frame asked for the system colour panel: opened here, outside the state's
        // borrow, since AppKit may call back into the view.
        if let Some(rgb) = with(|st| st.app.take_color_panel()).flatten() {
            self.open_color_panel(rgb);
        }
    }
}

/// `HARNESS_NATIVE_BENCH_HZ=<hz>`: frames from a main-run-loop timer at `hz` into an offscreen texture instead of the
/// display link, while the link is unpaused (as the link would call). For measuring frame pacing where no display link
/// calls (a locked screen, an occluded window): the timer is a run-loop source like the link, so whatever keeps the main
/// thread busy delays it the same way.
fn bench_clock(view: &Retained<HarnessView>, hz: f64) {
    let view = view.clone();
    let tex: RefCell<Option<gpu::Tex>> = RefCell::new(None);
    let last = std::cell::Cell::new(0.0f64);
    let block = block2::RcBlock::new(move |_: std::ptr::NonNull<objc2_foundation::NSTimer>| {
        let Some((paused, texture)) = with(|st| {
            let size = st.layer.drawableSize();
            let (w, h) = (size.width.max(1.0) as usize, size.height.max(1.0) as usize);
            let mut tex = tex.borrow_mut();
            if tex.as_ref().is_none_or(|t| {
                objc2_metal::MTLTexture::width(&**t) != w
                    || objc2_metal::MTLTexture::height(&**t) != h
            }) {
                *tex = Some(st.renderer.offscreen(w, h));
            }
            // At the rate the link is asked for (the calm rate while only a dot breathes), as the link would call.
            let want = f64::from(st.link.preferredFrameRateRange().preferred).clamp(1.0, hz);
            (
                st.link.isPaused()
                    || objc2_quartz_core::CACurrentMediaTime() - last.get() < 1.0 / want - 0.5 / hz,
                tex.clone().unwrap(),
            )
        }) else {
            return;
        };
        if paused {
            return;
        }
        let now = objc2_quartz_core::CACurrentMediaTime();
        last.set(now);
        view.frame_into(Dest::Texture(texture), now, now + 1.0 / hz, now + 2.0 / hz);
    });
    // SAFETY: the block runs on the main thread (a main-run-loop timer) and touches only main-thread state.
    let timer = unsafe {
        objc2_foundation::NSTimer::timerWithTimeInterval_repeats_block(1.0 / hz, true, &block)
    };
    // SAFETY: the main run loop, common modes.
    unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
}

/// `HARNESS_NATIVE_FRAMELOG=<path>`: one line a display-link callback (media-time seconds: entered, target, target
/// presentation, app frame built, render returned; then the last GPU time in ms), for measuring the frame clock against real timestamps.
mod framelog {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};
    static LOG: OnceLock<Option<Mutex<std::io::BufWriter<std::fs::File>>>> = OnceLock::new();
    static WAKES: Mutex<(f64, u32)> = Mutex::new((0.0, 0));
    /// A daemon wake reached the main thread: once a second, how many came, whether the link was paused, and whether the
    /// window is visible (an occluded window's display link stops calling).
    pub fn wake(paused: bool, visible: bool) {
        if std::env::var_os("HARNESS_NATIVE_FRAMELOG").is_none() {
            return;
        }
        let now = objc2_quartz_core::CACurrentMediaTime();
        let mut w = WAKES.lock().unwrap();
        w.1 += 1;
        if now - w.0 >= 1.0 {
            eprintln!("framelog: {now:.3} wakes={} paused={paused} visible={visible}", w.1);
            *w = (now, 0);
        }
    }
    pub fn note(entered: f64, target: f64, present: f64, built: f64, rendered: f64, gpu_ms: f32) {
        let log = LOG.get_or_init(|| {
            std::env::var_os("HARNESS_NATIVE_FRAMELOG")
                .and_then(|p| std::fs::File::create(p).ok())
                .map(|f| Mutex::new(std::io::BufWriter::new(f)))
        });
        if let Some(l) = log {
            let mut w = l.lock().unwrap();
            let _ = writeln!(
                w,
                "{entered:.6} {target:.6} {present:.6} {built:.6} {rendered:.6} {gpu_ms:.3}"
            );
            let _ = w.flush();
        }
    }
}

/// Set the system pointer to the app's resolved shape when it differs from the one last set, while the pointer is over
/// the view in the key window or a drag that began there goes on (AppKit keeps a drag's pointer, even outside the view).
fn apply_cursor(st: &mut State) {
    let want = st.app.shape();
    if st.cursor == Some(want) || !(st.inside || NSEvent::pressedMouseButtons() != 0) {
        return;
    }
    st.cursor = Some(want);
    ns_cursor(want).set();
}

/// The NSCursor macOS shows for `p`: columnResizeCursor and its one-way forms for a divider (NSSplitView's, macOS 15),
/// not the deprecated resizeLeftRightCursor.
fn ns_cursor(p: Pointer) -> Retained<NSCursor> {
    use objc2_app_kit::{NSHorizontalDirections, NSVerticalDirections};
    match p {
        Pointer::Arrow => NSCursor::arrowCursor(),
        Pointer::Text => NSCursor::IBeamCursor(),
        Pointer::VerticalText => NSCursor::IBeamCursorForVerticalLayout(),
        Pointer::Hand => NSCursor::pointingHandCursor(),
        Pointer::OpenHand => NSCursor::openHandCursor(),
        Pointer::Grabbing => NSCursor::closedHandCursor(),
        Pointer::ResizeLeftRight => NSCursor::columnResizeCursorInDirections(NSHorizontalDirections::All),
        Pointer::ResizeLeft => NSCursor::columnResizeCursorInDirections(NSHorizontalDirections::Left),
        Pointer::ResizeRight => NSCursor::columnResizeCursorInDirections(NSHorizontalDirections::Right),
        Pointer::ResizeUpDown => NSCursor::rowResizeCursorInDirections(NSVerticalDirections::All),
        Pointer::NotAllowed => NSCursor::operationNotAllowedCursor(),
    }
}

/// The whole window rubber-bands after the sidebar is dragged past its widest (den's, after os's `ElasticWindow`):
/// moved right of where it rested by the app's shift, not draggable meanwhile, and put back exactly there once it
/// settles. Never in full screen. The app is told the shift applied, so it reads the pointer where the window rests.
fn shift_window(st: &mut State) {
    let window = &st.window;
    let full = window.styleMask().contains(NSWindowStyleMask::FullScreen);
    let zoom = f64::from(st.app.zoom());
    let dx = if full { 0.0 } else { (f64::from(st.app.window_shift()) * zoom).round() };
    match st.rest {
        None if dx == 0.0 => {}
        None => {
            let at = window.frame().origin;
            st.rest = Some(at);
            window.setMovable(false);
            window.setFrameOrigin(NSPoint::new(at.x + dx, at.y));
        }
        Some(at) if dx == 0.0 => {
            window.setFrameOrigin(at);
            window.setMovable(true);
            st.rest = None;
        }
        Some(at) => window.setFrameOrigin(NSPoint::new(at.x + dx, at.y)),
    }
    st.app.set_window_moved((dx / zoom) as f32);
}

/// Play trackpad pulses through AppKit's NSHapticFeedbackManager (`defaultPerformer`, `performFeedbackPattern:
/// performanceTime:` at `.now`): the one API that reaches the Force Touch trackpad's actuator, so the sidebar's latch and
/// the chat effort's detents (chat/pick.rs: `.levelChange`, Apple's pattern for stepping between discrete levels) are
/// felt under the finger as they happen. On a trackpad without the actuator, or a mouse, it plays nothing.
fn play(pulses: &[crate::haptic::Haptic]) {
    use objc2_app_kit::{
        NSHapticFeedbackManager, NSHapticFeedbackPattern, NSHapticFeedbackPerformanceTime,
        NSHapticFeedbackPerformer,
    };
    for p in pulses {
        let pattern = match p {
            crate::haptic::Haptic::Alignment => NSHapticFeedbackPattern::Alignment,
            crate::haptic::Haptic::LevelChange => NSHapticFeedbackPattern::LevelChange,
        };
        NSHapticFeedbackManager::defaultPerformer()
            .performFeedbackPattern_performanceTime(pattern, NSHapticFeedbackPerformanceTime::Now);
    }
}

/// Whether `view` is drawn in the dark appearance.
/// Whether the system's Reduce Motion is on (NSWorkspace's `accessibilityDisplayShouldReduceMotion`).
fn reduce_motion() -> bool {
    objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
}

fn is_dark(view: &NSView) -> bool {
    // SAFETY: the appearance names are valid static strings.
    let (aqua, dark) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
    view.effectiveAppearance()
        .bestMatchFromAppearancesWithNames(&NSArray::from_slice(&[aqua, dark]))
        .is_some_and(|n| &*n == dark)
}

/// Show the traffic lights at `alpha` (they live on the sidebar: Dia's), hidden outright once they are gone.
fn set_lights(window: &NSWindow, alpha: f32) {
    for kind in
        [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton]
    {
        if let Some(b) = window.standardWindowButton(kind) {
            b.setAlphaValue(f64::from(alpha));
            b.setHidden(alpha <= 0.0);
        }
    }
}

/// `HARNESS_NATIVE_APPEARANCE=light|dark`: this app's appearance (NSApp.appearance), not the system's; for captures.
fn force_appearance(app: &NSApplication) {
    let name = match std::env::var("HARNESS_NATIVE_APPEARANCE").as_deref() {
        // SAFETY: the appearance names are valid static strings.
        Ok("light") => unsafe { NSAppearanceNameAqua },
        Ok("dark") => unsafe { NSAppearanceNameDarkAqua },
        _ => return,
    };
    app.setAppearance(NSAppearance::appearanceNamed(name).as_deref());
}

/// The title-bar strip of `window` (none in fullscreen, where AppKit hides it), its traffic lights moved to its centre line.
fn chrome(window: &NSWindow) -> Chrome {
    if window.styleMask().contains(NSWindowStyleMask::FullScreen) {
        return Chrome::default();
    }
    let height = window.frame().size.height;
    let strip = (height - window.contentLayoutRect().size.height).max(0.0);
    let mut right: f64 = 0.0;
    for kind in
        [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton]
    {
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
    Chrome {
        strip: strip as f32,
        title_x: if right > 0.0 { (right + TITLE_GAP) as f32 } else { 0.0 },
    }
}

/// `HARNESS_NATIVE_CHROME_TEST`: resize this app's own window (no focus taken), logging the title bar after it, so a
/// capture after about 3 s shows the title bar as AppKit lays it out again. Fullscreen is not exercised here: it takes
/// a Space (and the screen); `windowChromeChanged:` handles its exit the same way.
fn chrome_test(_window: &NSWindow) {
    fn later(secs: f64, f: fn(&NSWindow)) {
        let when = dispatch2::DispatchTime::try_from(std::time::Duration::from_secs_f64(secs))
            .expect("a short delay");
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
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable
        | NSWindowStyleMask::FullSizeContentView;
    // SAFETY: a fresh window with a valid rect and style.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the state keeps its own reference; AppKit must not release it on close as well.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("harness"));
    window.setTitlebarAppearsTransparent(true);
    window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
    window.setMinSize(NSSize::new(560.0, 320.0));
    // Clear: the sidebar's vibrancy (the effect view under the Metal view) shows wherever the scene paints nothing.
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    let view = HarnessView::new(mtm, rect);
    view.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );

    let renderer = Renderer::new(MTLCreateSystemDefaultDevice().ok_or("no Metal device")?);
    let layer = CAMetalLayer::new();
    layer.setDevice(Some(&**renderer.device()));
    layer.setPixelFormat(gpu::PIXEL_FORMAT);
    layer.setMaximumDrawableCount(3);
    // Not opaque: the drawable's alpha (premultiplied, as every pipeline blends) lets the vibrancy through.
    layer.setOpaque(false);
    // SAFETY: kCGColorSpaceExtendedSRGB is a valid static colour space name.
    layer.setColorspace(
        CGColorSpace::with_name(Some(unsafe { kCGColorSpaceExtendedSRGB })).as_deref(),
    );
    // EDR: values past 1.0 in the extended sRGB target show brighter than SDR white, up to the screen's headroom
    // (`screen_headroom`); SDR content (everything at or under 1.0) is unchanged. `preferredDynamicRange` replaces the
    // deprecated `wantsExtendedDynamicRangeContent` (macOS 26).
    // SAFETY: CADynamicRangeHigh is a valid static dynamic range name.
    layer.setPreferredDynamicRange(unsafe { objc2_quartz_core::CADynamicRangeHigh });
    // SAFETY: kCAGravityTopLeft is a valid static gravity name.
    layer.setContentsGravity(unsafe { kCAGravityTopLeft });
    view.setLayer(Some(&layer));
    view.setWantsLayer(true);
    // CursorUpdate: AppKit sends `cursorUpdate:` when the pointer comes into the view or the window becomes key, so the
    // pointer's shape is set again after AppKit has shown its own (a resize edge, the title bar, another window).
    let options = NSTrackingAreaOptions::MouseEnteredAndExited
        | NSTrackingAreaOptions::MouseMoved
        | NSTrackingAreaOptions::CursorUpdate
        | NSTrackingAreaOptions::ActiveInKeyWindow
        | NSTrackingAreaOptions::InVisibleRect;
    // SAFETY: the view owns the area and outlives it; no user info.
    let area = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            NSTrackingArea::alloc(),
            NSRect::ZERO,
            options,
            Some(&view),
            None,
        )
    };
    view.addTrackingArea(&area);
    // Finder files dragged over the window, for a generated DropZone (genui/dropzone.rs).
    // SAFETY: NSPasteboardTypeFileURL is a valid static pasteboard type.
    view.registerForDraggedTypes(&NSArray::from_slice(&[unsafe { NSPasteboardTypeFileURL }]));

    let link = CAMetalDisplayLink::initWithMetalLayer(CAMetalDisplayLink::alloc(), &layer);
    link.setDelegate(Some(ProtocolObject::from_ref(&*view)));
    link.setPreferredFrameRateRange(LIVE);
    link.setPreferredFrameLatency(1.0);
    let bench = std::env::var("HARNESS_NATIVE_BENCH_HZ")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|hz| *hz > 0.0);
    if let Some(hz) = bench {
        bench_clock(&view, hz);
    } else {
        // SAFETY: the main run loop and the common modes (frames keep coming during live resize and tracking).
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };
    }

    let mut app = App::new(wake_from_any_thread())?;
    app.set_gpu_clock(renderer.gpu_clock());
    app.set_media(renderer.media());
    // Tooltips come quickly (AppKit waits about a second and a half): what a step's item or a control says under the
    // pointer is part of reading the window, not a hint for the lost.
    objc2_foundation::NSUserDefaults::standardUserDefaults().setInteger_forKey(250, objc2_foundation::ns_string!("NSInitialToolTipDelay"));
    // The world served to agents, and the edits saved from it replayed (src/world_api.rs): the window's only.
    app.serve_world();
    // The app's sound (src/audio.rs): the window's only. A scene or a device that refuses it leaves the app silent.
    if let Err(e) = app.start_sound() {
        eprintln!("harness-native: sound: {e}");
    }
    if std::env::var("HARNESS_NATIVE_SIDEBAR").as_deref() == Ok("0") {
        app.fold_sidebar_now();
    }
    if let Some(sid) = SELECT.with(std::cell::Cell::get) {
        app.select_session(sid);
    }
    // The menu bar, from the keys' actions and bindings (rebuilt in `frame` when they are loaded again).
    NSApplication::sharedApplication(mtm).setMainMenu(Some(&main_menu(
        mtm,
        &app.menu(),
        Some(&*view),
    )));
    let menu_gen = app.keys_gen();
    let overlays = overlay::Overlays::new(view.clone(), renderer.device().clone());
    let maps = maps::Maps::new(view.clone());
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            app,
            renderer,
            layer,
            link,
            window: window.clone(),
            last: None,
            points: (1.0, 1.0),
            backing: 1.0,
            title: String::new(),
            lights: 1.0,
            tip: String::new(),
            calm: false,
            menu_gen,
            cursor: None,
            inside: false,
            rest: None,
            overlays,
            maps,
            mini: None,
        })
    });
    // The sidebar's material behind the whole window (Finder's, Zed's): the main card covers it with an opaque plate,
    // and the sidebar's ground (`tone_sidebar`, scene/00-tokens.flecs) is a tint over it, so what is behind the window
    // shows blurred through the sidebar only. It follows the window's appearance, as the scene's fills do (`is_dark`).
    let effect = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), rect);
    // HARNESS_NATIVE_MATERIAL=sidebar|under|hud: the vibrancy material under the sidebar's tint plate.
    effect.setMaterial(match std::env::var("HARNESS_NATIVE_MATERIAL").as_deref() {
        Ok("sidebar") => NSVisualEffectMaterial::Sidebar,
        Ok("hud") => NSVisualEffectMaterial::HUDWindow,
        _ => NSVisualEffectMaterial::UnderWindowBackground,
    });
    effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    // HARNESS_NATIVE_VIBRANCY=active: always the active material (captures of a window that never takes the keyboard).
    effect.setState(if std::env::var("HARNESS_NATIVE_VIBRANCY").as_deref() == Ok("active") {
        NSVisualEffectState::Active
    } else {
        NSVisualEffectState::FollowsWindowActiveState
    });
    effect.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    effect.addSubview(&view);
    window.setContentView(Some(&effect));
    with(|st| {
        st.app.dark = is_dark(&view);
        st.app.set_reduce_motion(reduce_motion());
    });
    // Reduce Motion going on or off is posted on the workspace's own notification centre.
    // SAFETY: the view outlives the workspace's notifications (it lives until the app quits); the selector takes one
    // NSNotification, and the name is a valid static string.
    unsafe {
        objc2_app_kit::NSWorkspace::sharedWorkspace().notificationCenter().addObserver_selector_name_object(
            &view,
            sel!(motionChanged:),
            Some(objc2_app_kit::NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification),
            None,
        );
    }
    // The view hears the window's key changes (focus for the cursor).
    let center = objc2_foundation::NSNotificationCenter::defaultCenter();
    // SAFETY: the view outlives the window's notifications (both live until the app quits); the selectors take one
    // NSNotification.
    unsafe {
        center.addObserver_selector_name_object(
            &view,
            sel!(windowDidBecomeKey:),
            Some(objc2_app_kit::NSWindowDidBecomeKeyNotification),
            Some(&window),
        );
        center.addObserver_selector_name_object(
            &view,
            sel!(windowDidResignKey:),
            Some(objc2_app_kit::NSWindowDidResignKeyNotification),
            Some(&window),
        );
        center.addObserver_selector_name_object(
            &view,
            sel!(windowOcclusionChanged:),
            Some(objc2_app_kit::NSWindowDidChangeOcclusionStateNotification),
            Some(&window),
        );
        // The headroom is the screen's: moving to another screen, or a screen's mode changing, redraws at its headroom.
        center.addObserver_selector_name_object(
            &view,
            sel!(screenChanged:),
            Some(objc2_app_kit::NSWindowDidChangeScreenNotification),
            Some(&window),
        );
        center.addObserver_selector_name_object(
            &view,
            sel!(screenChanged:),
            Some(objc2_app_kit::NSApplicationDidChangeScreenParametersNotification),
            None,
        );
        center.addObserver_selector_name_object(
            &view,
            sel!(windowMoved:),
            Some(objc2_app_kit::NSWindowDidMoveNotification),
            Some(&window),
        );
        // AppKit lays the title bar (and the traffic lights) out again after these.
        for name in [
            objc2_app_kit::NSWindowDidResizeNotification,
            objc2_app_kit::NSWindowDidEnterFullScreenNotification,
            objc2_app_kit::NSWindowDidExitFullScreenNotification,
        ] {
            center.addObserver_selector_name_object(
                &view,
                sel!(windowChromeChanged:),
                Some(name),
                Some(&window),
            );
        }
    }
    window.center();
    // `HARNESS_NATIVE_FRAME=x,y,w,h` (points, from the main screen's top left): where the window goes, for measuring the
    // frame clock in a corner no other window covers (an occluded window's display link stops calling).
    if let Some([x, y, w, h]) = std::env::var("HARNESS_NATIVE_FRAME")
        .ok()
        .and_then(|v| {
            v.split(',').map(|p| p.trim().parse::<f64>().ok()).collect::<Option<Vec<_>>>()
        })
        .and_then(|v| <[f64; 4]>::try_from(v).ok())
    {
        let top = objc2_app_kit::NSScreen::mainScreen(mtm).map_or(0.0, |s| s.frame().size.height);
        window
            .setFrame_display(NSRect::new(NSPoint::new(x, top - y - h), NSSize::new(w, h)), false);
    }
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
            // What was typed since the last pause is kept too.
            with(|st| st.app.flush_drafts());
            single::release();
        }

        /// The Dock icon pressed: a tucked-away window comes back.
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn handle_reopen(&self, _app: &NSApplication, _visible: bool) -> bool {
            untuck();
            true
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

fn item(
    mtm: MainThreadMarker,
    title: &str,
    action: objc2::runtime::Sel,
    key: &str,
) -> Retained<NSMenuItem> {
    // SAFETY: a fresh item with a valid selector; the target is the responder chain.
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
        )
    }
}

fn submenu(
    mtm: MainThreadMarker,
    title: &str,
    items: &[Retained<NSMenuItem>],
) -> Retained<NSMenuItem> {
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
    for i in items {
        menu.addItem(i);
    }
    let holder = NSMenuItem::new(mtm);
    holder.setTitle(&NSString::from_str(title));
    holder.setSubmenu(Some(&menu));
    holder
}

/// The menu bar from the keys' menus (`App::menu`): each action an item, its key equivalent its binding's; an item with
/// a selector sends it up the responder chain, the others run their action through `target`'s `runAction:`.
fn main_menu(
    mtm: MainThreadMarker,
    bars: &[(String, Vec<MenuLine>)],
    target: Option<&HarnessView>,
) -> Retained<NSMenu> {
    let bar = NSMenu::new(mtm);
    for (title, lines) in bars {
        let mut items = Vec::new();
        for l in lines {
            if l.sep && !items.is_empty() {
                items.push(NSMenuItem::separatorItem(mtm));
            }
            let sel = l.selector.as_deref().map_or(sel!(runAction:), sel_of);
            let (key, mask) =
                l.key.as_ref().map_or((String::new(), NSEventModifierFlags::empty()), |(k, m)| {
                    let mut f = NSEventModifierFlags::empty();
                    for (on, flag) in [
                        (m.cmd, NSEventModifierFlags::Command),
                        (m.shift, NSEventModifierFlags::Shift),
                        (m.alt, NSEventModifierFlags::Option),
                        (m.ctrl, NSEventModifierFlags::Control),
                    ] {
                        if on {
                            f |= flag;
                        }
                    }
                    (k.clone(), f)
                });
            let it = item(mtm, &l.title, sel, &key);
            it.setKeyEquivalentModifierMask(mask);
            if l.selector.is_none() {
                it.setTag(l.action as isize);
                // SAFETY: the view outlives the menu bar (both live until the app quits); the item holds it weakly.
                if let Some(t) = target {
                    unsafe { it.setTarget(Some(t)) };
                }
            }
            items.push(it);
        }
        bar.addItem(&submenu(mtm, title, &items));
    }
    bar
}

/// Put `text` on the general pasteboard as its one string.
fn pasteboard_put(text: &str) {
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    // SAFETY: NSPasteboardTypeString is a valid static pasteboard type.
    pb.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString });
}

/// The pictures on pasteboard `pb` as PNG bytes, one for each item that holds one (a copy of several pictures is
/// several items): its PNG as it is, else its TIFF (a screenshot's, Safari's Copy Image) converted. Empty when none holds
/// a picture (or only TIFFs AppKit cannot read).
fn pasteboard_pngs(pb: &NSPasteboard) -> Vec<Vec<u8>> {
    let Some(items) = pb.pasteboardItems() else { return Vec::new() };
    let mut out = Vec::new(); // alloc: a paste's few pictures
    for item in &items {
        // SAFETY: NSPasteboardTypePNG is a valid static pasteboard type.
        if let Some(d) = item.dataForType(unsafe { objc2_app_kit::NSPasteboardTypePNG }) {
            out.push(d.to_vec());
            continue;
        }
        // SAFETY: NSPasteboardTypeTIFF is a valid static pasteboard type.
        let Some(tiff) = item.dataForType(unsafe { objc2_app_kit::NSPasteboardTypeTIFF }) else { continue };
        let Some(rep) = objc2_app_kit::NSBitmapImageRep::imageRepWithData(&tiff) else { continue };
        // SAFETY: an empty properties dictionary has the type the method takes.
        let png = unsafe { rep.representationUsingType_properties(objc2_app_kit::NSBitmapImageFileType::PNG, &objc2_foundation::NSDictionary::new()) };
        if let Some(png) = png {
            out.push(png.to_vec());
        }
    }
    out
}

/// The paths of the files on pasteboard `pb` (each item's file URL; Finder's are file reference URLs, resolved to paths).
fn pasteboard_paths(pb: &NSPasteboard) -> Vec<String> {
    let Some(items) = pb.pasteboardItems() else { return Vec::new() };
    let mut out = Vec::new(); // alloc: a paste's few paths
    for item in &items {
        // SAFETY: NSPasteboardTypeFileURL is a valid static pasteboard type.
        let Some(url) = item.stringForType(unsafe { objc2_app_kit::NSPasteboardTypeFileURL }) else { continue };
        let path = objc2_foundation::NSURL::URLWithString(&url).and_then(|u| u.filePathURL()).and_then(|u| u.path());
        if let Some(p) = path {
            out.push(p.to_string());
        }
    }
    out
}

/// Send AppKit action `sel` (`copy:`) up the responder chain from the key window's first responder to the app.
fn send_selector(mtm: MainThreadMarker, sel: &str) {
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: a registered selector; a nil target walks the responder chain, which only calls methods taking a sender.
    unsafe { app.sendAction_to_from(sel_of(sel), None, None) };
}

/// The selector named `name` (`copy:`).
fn sel_of(name: &str) -> objc2::runtime::Sel {
    objc2::runtime::Sel::register(&std::ffi::CString::new(name).unwrap_or_default())
}

/// `HARNESS_NATIVE_MENU_DUMP`: the menu bar as AppKit holds it, one line an item (title, key equivalent and its
/// modifiers as the menu shows them, action), read back from the NSMenu built for `bars`.
pub fn dump_menu(bars: &[(String, Vec<MenuLine>)]) -> String {
    let Some(mtm) = MainThreadMarker::new() else { return String::new() };
    let menu = main_menu(mtm, bars, None);
    let mut out = String::new();
    for i in 0..menu.numberOfItems() {
        let Some(holder) = menu.itemAtIndex(i) else { continue };
        out += &format!("{}\n", holder.title());
        let Some(sub) = holder.submenu() else { continue };
        for j in 0..sub.numberOfItems() {
            let Some(it) = sub.itemAtIndex(j) else { continue };
            if it.isSeparatorItem() {
                out += "  ----\n";
                continue;
            }
            let m = it.keyEquivalentModifierMask();
            let mut keys = String::new();
            for (flag, g) in [
                (NSEventModifierFlags::Control, '\u{2303}'),
                (NSEventModifierFlags::Option, '\u{2325}'),
                (NSEventModifierFlags::Shift, '\u{21E7}'),
                (NSEventModifierFlags::Command, '\u{2318}'),
            ] {
                if m.contains(flag) {
                    keys.push(g);
                }
            }
            let k = it.keyEquivalent().to_string();
            keys += &match k.as_str() {
                "" => String::new(),
                " " => "Space".into(),
                "-" => "\u{2212}".into(),
                k => k.to_uppercase(),
            };
            let action =
                it.action().map(|s| s.name().to_string_lossy().into_owned()).unwrap_or_default();
            out += &format!(
                "  {:<18} {:<8} {}{}\n",
                it.title(),
                keys,
                action,
                if action == "runAction:" { format!(" (tag {})", it.tag()) } else { String::new() }
            );
        }
    }
    out
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
    app.setActivationPolicy(if no_activate() {
        NSApplicationActivationPolicy::Accessory
    } else {
        NSApplicationActivationPolicy::Regular
    });
    force_appearance(&app);
    let delegate = AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    DELEGATE.with(|d| {
        let _ = d.set(delegate.clone());
    });
    app.run();
}

#[cfg(test)]
mod tests {
    /// The haptics `play` sends exist in this AppKit: the class, its shared performer and the method it calls.
    #[test]
    fn haptic_feedback_api_exists() {
        use objc2::runtime::{AnyClass, Sel};
        let class = AnyClass::get(c"NSHapticFeedbackManager").expect("AppKit has no NSHapticFeedbackManager");
        assert!(class.class_method(Sel::register(c"defaultPerformer")).is_some(), "no +[NSHapticFeedbackManager defaultPerformer]");
        let performer = objc2_app_kit::NSHapticFeedbackManager::defaultPerformer();
        let performer: &objc2::runtime::AnyObject = performer.as_ref();
        let sel = Sel::register(c"performFeedbackPattern:performanceTime:");
        assert!(performer.class().responds_to(sel), "the default performer does not take performFeedbackPattern:performanceTime:");
    }
}
