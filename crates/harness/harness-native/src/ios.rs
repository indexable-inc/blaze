//! The phone's shell: UIKit in place of AppKit (src/window.rs is the Mac's). The app's core, its scenes and its
//! renderer are the same; UIKit only hands out what the system owns: a view whose layer is a `CAMetalLayer`, the
//! display's clock (`CADisplayLink`), touches and the keyboard.
//!
//! Touch: a tap is a click where the finger lifts (pointer down and up there); a finger that moves scrolls what is
//! under it, as a trackpad's precise wheel does (began, changed, ended), with no glide after it lifts yet. Typing: the
//! view is the first responder and a `UIKeyInput`, so the system keyboard's text goes to the app as committed text,
//! Return and Delete as their keys. The keyboard covers the view's foot: the view is as tall as what the keyboard
//! leaves, so the composer stands on it.
//!
//! Overlays that float in windows of their own on the Mac (menus, popovers, hover cards) are drawn over the frame in
//! the one view, as an offscreen shot lays them.
use crate::app::{App, Chrome, KeyMods, Wheel};
use crate::gpu::{self, Dest, Pass, Renderer};
use crate::term::{self, Button, KeyAction};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{ClassType, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColorSpace, kCGColorSpaceExtendedSRGB};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes, NSSet, NSString};
use objc2_metal::MTLCreateSystemDefaultDevice;
use objc2_quartz_core::{CADisplayLink, CAMetalLayer};
use objc2_ui_kit::{
    UIApplication, UIApplicationDelegate, UIEvent, UIKeyInput, UIResponder, UIScene, UISceneConnectionOptions, UISceneDelegate,
    UISceneSession, UITextInputTraits, UITouch, UIView, UIViewController, UIWindow, UIWindowScene, UIWindowSceneDelegate,
};
use std::cell::RefCell;
use std::sync::Arc;

/// How far a finger moves (points) before its touch is a scroll, not a tap.
const SLOP: f64 = 8.0;
/// NSEvent's scroll phases, which the app's wheel speaks: began, changed, ended.
const BEGAN: usize = 1;
const CHANGED: usize = 4;
const ENDED: usize = 8;
/// The Mac's virtual key codes for the two keys the system keyboard sends as keys: Return and Delete.
const KEY_RETURN: u16 = 36;
const KEY_DELETE: u16 = 51;

struct State {
    app: App,
    renderer: Renderer,
    layer: Retained<CAMetalLayer>,
    window: Retained<UIWindow>,
    view: Retained<HarnessView>,
    last: Option<f64>,
    /// The touch in hand: where it began and was last (view points), and whether it has become a scroll.
    touch: Option<(CGPoint, CGPoint, bool)>,
    /// How much of the window's foot the keyboard covers, points.
    keyboard: f64,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

define_class!(
    // SAFETY: UIView has no subclassing requirements beyond calling super's designated initialiser; no Drop.
    #[unsafe(super(UIView, UIResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HarnessView"]
    struct HarnessView;

    unsafe impl NSObjectProtocol for HarnessView {}

    impl HarnessView {
        /// The view's layer is the Metal layer the renderer presents into.
        #[unsafe(method(layerClass))]
        fn layer_class() -> &'static AnyClass {
            CAMetalLayer::class()
        }

        /// It takes the keyboard.
        #[unsafe(method(canBecomeFirstResponder))]
        fn can_become_first_responder(&self) -> bool {
            true
        }

        /// The display's clock: one frame.
        #[unsafe(method(step:))]
        fn step(&self, _link: &CADisplayLink) {
            self.draw_frame();
        }

        #[unsafe(method(touchesBegan:withEvent:))]
        fn touches_began(&self, touches: &NSSet<UITouch>, _event: Option<&UIEvent>) {
            let Some(p) = self.point(touches) else { return };
            with(|st| {
                st.touch = Some((p, p, false));
                st.app.pointer_move(p.x as f32, p.y as f32, KeyMods::default());
            });
            // A touch anywhere brings the keyboard's owner back.
            let _ = self.becomeFirstResponder();
        }

        #[unsafe(method(touchesMoved:withEvent:))]
        fn touches_moved(&self, touches: &NSSet<UITouch>, _event: Option<&UIEvent>) {
            let Some(p) = self.point(touches) else { return };
            with(|st| {
                let Some((from, last, scrolling)) = st.touch else { return };
                let far = (p.x - from.x).hypot(p.y - from.y) > SLOP;
                if !scrolling && !far {
                    return;
                }
                let phase = if scrolling { CHANGED } else { BEGAN };
                st.touch = Some((from, p, true));
                // The content follows the finger: natural scrolling's deltas.
                let ev = Wheel { dx: (p.x - last.x) as f32, dy: (p.y - last.y) as f32, precise: true, phase, momentum: 0, inverted: true };
                st.app.scroll_wheel(ev, KeyMods::default());
            });
        }

        #[unsafe(method(touchesEnded:withEvent:))]
        fn touches_ended(&self, touches: &NSSet<UITouch>, _event: Option<&UIEvent>) {
            let at = self.point(touches);
            with(|st| {
                let Some((from, last, scrolling)) = st.touch.take() else { return };
                let p = at.unwrap_or(last);
                let m = KeyMods::default();
                if scrolling {
                    st.app.scroll_wheel(Wheel { dx: 0.0, dy: 0.0, precise: true, phase: ENDED, momentum: 0, inverted: true }, m);
                } else {
                    // A tap: a click where it began.
                    st.app.pointer_down(from.x as f32, from.y as f32, Button::Left, 1, m);
                    st.app.pointer_up(p.x as f32, p.y as f32, Button::Left, m);
                }
            });
        }

        #[unsafe(method(touchesCancelled:withEvent:))]
        fn touches_cancelled(&self, _touches: &NSSet<UITouch>, _event: Option<&UIEvent>) {
            with(|st| {
                if let Some((_, _, true)) = st.touch.take() {
                    st.app.scroll_wheel(Wheel { dx: 0.0, dy: 0.0, precise: true, phase: ENDED, momentum: 0, inverted: true }, KeyMods::default());
                }
            });
        }

        /// The keyboard came, went or changed size: the view ends where it begins.
        #[unsafe(method(keyboardChanged:))]
        fn keyboard_changed(&self, note: &NSNotification) {
            // SAFETY: the notification's user info holds the keyboard's end frame as an NSValue of a CGRect, in screen points.
            let end: Option<CGRect> = unsafe {
                let info: Option<Retained<AnyObject>> = msg_send![note, userInfo];
                let key = NSString::from_str("UIKeyboardFrameEndUserInfoKey");
                let value: Option<Retained<AnyObject>> = info.and_then(|i| msg_send![&*i, objectForKey: &*key]);
                value.map(|v| msg_send![&*v, CGRectValue])
            };
            let Some(end) = end else { return };
            with(|st| {
                let all = st.window.bounds();
                st.keyboard = (all.size.height - end.origin.y).clamp(0.0, all.size.height);
                fit(st);
                st.app.poke();
            });
        }
    }

    unsafe impl UITextInputTraits for HarnessView {}

    unsafe impl UIKeyInput for HarnessView {
        #[unsafe(method(hasText))]
        fn has_text(&self) -> bool {
            true
        }

        #[unsafe(method(insertText:))]
        fn insert_text(&self, text: &NSString) {
            let text = text.to_string();
            with(|st| {
                if text == "\n" {
                    st.app.key(&key(KEY_RETURN, "\r"));
                } else {
                    st.app.commit_text(&text);
                }
            });
        }

        #[unsafe(method(deleteBackward))]
        fn delete_backward(&self) {
            with(|st| st.app.key(&key(KEY_DELETE, "")));
        }
    }
);

/// The Metal view's frame in its window: under the status bar and the island, between the screen's rounded sides, and
/// down to the keyboard while it is up, else to the home indicator's strip.
fn fit(st: &mut State) {
    let all = st.window.bounds();
    let safe = st.window.safeAreaInsets();
    let foot = st.keyboard.max(safe.bottom);
    let want = CGRect::new(
        CGPoint::new(all.origin.x + safe.left, all.origin.y + safe.top),
        CGSize::new((all.size.width - safe.left - safe.right).max(1.0), (all.size.height - safe.top - foot).max(1.0)),
    );
    if st.view.frame() != want {
        st.view.setFrame(want);
    }
}

/// A key the system keyboard sends as a key, pressed with no modifiers.
fn key(code: u16, text: &str) -> term::KeyIn<'_> {
    term::KeyIn { code, text, unshifted: None, mods: KeyMods::default(), action: KeyAction::Press, composing: false }
}

impl HarnessView {
    /// Where the first of `touches` is, in the view's points.
    fn point(&self, touches: &NSSet<UITouch>) -> Option<CGPoint> {
        touches.anyObject().map(|t| t.locationInView(Some(self)))
    }

    /// One frame: the app steps and draws at the view's size, the renderer presents it.
    fn draw_frame(&self) {
        let bounds = self.bounds();
        let scale = self.contentScaleFactor();
        let now = objc2_quartz_core::CACurrentMediaTime();
        with(|st| {
            // The safe area moves with the device's turn: the view follows it.
            fit(st);
            let (w, h) = (bounds.size.width as f32, bounds.size.height as f32);
            if w < 1.0 || h < 1.0 {
                return;
            }
            st.layer.setDrawableSize(CGSize::new(bounds.size.width * scale, bounds.size.height * scale));
            let dt = st.last.map_or(1.0 / 120.0, |l| (now - l).clamp(0.0, 0.1)) as f32;
            st.last = Some(now);
            // No title strip: the view begins under the status bar (`fit`).
            st.app.set_chrome(Chrome { strip: 0.0, title_x: 0.0 });
            st.app.set_screen([0.0, 0.0, w, h]);
            let renderer = &st.renderer;
            let draw = st.app.frame((w, h), scale as f32, dt, &mut |x, y, w, h, b| renderer.upload_atlas(x, y, w, h, b));
            // What floats in its own window on the Mac is drawn over the frame here, where layout placed it.
            let floats: Vec<crate::draw::Draw> = st
                .app
                .take_floats()
                .into_iter()
                .map(|mut f| {
                    f.draw.frame.view = draw.frame.view;
                    f.draw.frame.place = [0.0; 4];
                    f.draw
                })
                .collect(); // alloc: the frame's open overlays, none or one or two
            let Some(drawable) = st.layer.nextDrawable() else { return };
            let mut passes = Vec::with_capacity(1 + floats.len()); // alloc: the view's pass and one per open overlay
            passes.push(Pass { draw: &draw, dest: Dest::Drawable(drawable.clone()), over: false });
            passes.extend(floats.iter().map(|f| Pass { draw: f, dest: Dest::Drawable(drawable.clone()), over: true }));
            st.renderer.render_passes(&passes);
        });
    }
}

define_class!(
    // SAFETY: UIResponder has no subclassing requirements; no Drop.
    #[unsafe(super(UIResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HarnessSceneDelegate"]
    struct SceneDelegate;

    unsafe impl NSObjectProtocol for SceneDelegate {}

    unsafe impl UISceneDelegate for SceneDelegate {
        /// The app's one scene connected: its window, the view, the app, and the display's clock.
        #[unsafe(method(scene:willConnectToSession:options:))]
        fn will_connect(&self, scene: &UIScene, _session: &UISceneSession, _options: &UISceneConnectionOptions) {
            let mtm = self.mtm();
            // SAFETY: the app's scenes are window scenes (Info.plist's manifest names no other role).
            let scene: &UIWindowScene = unsafe { &*std::ptr::from_ref(scene).cast() };
            if let Err(e) = start(mtm, scene) {
                eprintln!("harness-native: {e}");
                std::process::exit(1);
            }
        }
    }

    unsafe impl UIWindowSceneDelegate for SceneDelegate {}
);

define_class!(
    // SAFETY: UIResponder has no subclassing requirements; no Drop.
    #[unsafe(super(UIResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HarnessAppDelegate"]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl UIApplicationDelegate for AppDelegate {}
);

/// The window, the view and the app for `scene`.
fn start(mtm: MainThreadMarker, scene: &UIWindowScene) -> Result<(), String> {
    let window = UIWindow::initWithWindowScene(UIWindow::alloc(mtm), scene);
    let bounds = window.bounds();
    // SAFETY: UIView's designated initialiser, on a fresh allocation of its subclass.
    let view: Retained<HarnessView> = unsafe { msg_send![HarnessView::alloc(mtm), initWithFrame: bounds] };
    let renderer = Renderer::new(MTLCreateSystemDefaultDevice().ok_or("no Metal device")?);
    // SAFETY: the view's layer is a CAMetalLayer (`layerClass`).
    let layer: Retained<CAMetalLayer> = unsafe { Retained::cast_unchecked(view.layer()) };
    layer.setDevice(Some(&**renderer.device()));
    layer.setPixelFormat(gpu::PIXEL_FORMAT);
    layer.setMaximumDrawableCount(3);
    // SAFETY: kCGColorSpaceExtendedSRGB is a valid static colour space name.
    layer.setColorspace(CGColorSpace::with_name(Some(unsafe { kCGColorSpaceExtendedSRGB })).as_deref());
    let controller = UIViewController::new(mtm);
    // A plain root view, in the app's ground, holds the Metal view, so the Metal view can keep to the safe area and end
    // above the keyboard.
    if let Some(root) = controller.view() {
        root.setBackgroundColor(Some(&objc2_ui_kit::UIColor::colorWithRed_green_blue_alpha(0.039, 0.043, 0.051, 1.0)));
        root.addSubview(&view);
    }
    window.setRootViewController(Some(&controller));
    window.makeKeyAndVisible();

    let wake: crate::app::Wake = Arc::new(|| {
        dispatch2::DispatchQueue::main().exec_async(|| {
            with(|st| st.app.poke());
        });
    });
    let mut app = App::new(wake)?;
    app.set_gpu_clock(renderer.gpu_clock());
    app.set_media(renderer.media());
    // A phone has no room for the tree beside the chat: it starts folded.
    app.fold_sidebar_now();
    // No vibrancy under the Metal view here: the sidebar's ground is drawn opaque.
    app.opaque_sidebar();
    if let Err(e) = app.start_sound() {
        eprintln!("harness-native: sound: {e}");
    }
    // SAFETY: the view lives as long as the app; `step:` takes the link and returns nothing.
    let link = unsafe { CADisplayLink::displayLinkWithTarget_selector(&view, sel!(step:)) };
    // SAFETY: the main run loop, common modes.
    unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };
    // SAFETY: the view observes for as long as the app runs; the selector takes the notification.
    unsafe {
        let name = NSString::from_str("UIKeyboardWillChangeFrameNotification");
        NSNotificationCenter::defaultCenter().addObserver_selector_name_object(&view, sel!(keyboardChanged:), Some(&name), None);
    }
    let _ = view.becomeFirstResponder();
    STATE.with(|s| *s.borrow_mut() = Some(State { app, renderer, layer, window, view, last: None, touch: None, keyboard: 0.0 }));
    Ok(())
}

/// Run the app: UIKit's main loop, which makes the scene's delegate (Info.plist names it) and with it the window.
pub fn run() -> ! {
    let mtm = MainThreadMarker::new().expect("harness-native runs on the main thread");
    // The classes UIKit makes by name exist before it asks for them.
    let _ = (SceneDelegate::class(), HarnessView::class());
    let delegate = NSString::from_str(AppDelegate::class().name().to_str().unwrap_or("HarnessAppDelegate"));
    UIApplication::main(None, Some(&delegate), mtm)
}
