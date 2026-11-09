//! The windows windowed overlays float in (src/app/floats.rs): one borderless, non-activating `NSPanel` per open overlay,
//! a child of the main window (`addChildWindow:ordered:`), so it moves, changes space and enters full screen with it,
//! and reaches past its edge as NSMenu's and NSPopover's windows do. Each has its own `CAMetalLayer`, set up as the main
//! one is (extended sRGB, EDR, premultiplied and clear where nothing is drawn), drawn in the main frame's command buffer
//! (`Renderer::render_passes`) from the overlay's own draw; the window server shapes its shadow from what it draws.
//!
//! Keys: a panel never becomes key (a borderless NSPanel can't), so the main window keeps the keyboard and its view's
//! keyDown goes on to the app, which gives an open search menu's filter its typing, as a menu tracking keys does; the
//! input method's candidate window still follows the main view's caret.
//!
//! Pointer: a panel's view hands each event on to the main view (`HarnessView`), whose `local` maps a point in any of
//! the app's windows into its own, so hit-testing, hover, presses and the wheel are the main window's, in the same
//! points layout placed the overlay in.
//!
//! Glass: a frosted overlay (its node has `Frost`) has the window server's vibrancy (`NSVisualEffectView`, the popover
//! material) behind its view, cut to the node's corners, so what is behind the window shows through its translucent
//! plate blurred: the app's own glass, the way NSMenu's and NSPopover's windows are. Any other overlay hides it. The glass
//! follows the plate through its transition each frame (its drawn rectangle, scaled corners and alpha), so a menu that
//! closes fades its frost with its words instead of leaving a blank frosted shape for the frames it takes to go.
//!
//! Showing: a panel's layer presents with its transaction, and a panel is ordered in only after its first picture is
//! drawn, so a window is never seen with nothing or with the overlay it showed last (a reused panel's stale picture).
use super::HarnessView;
use crate::app::Float;
use crate::gpu::{self, Dest};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBackingStoreType, NSColor, NSEvent, NSPanel, NSResponder, NSTrackingArea,
    NSTrackingAreaOptions, NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState,
    NSVisualEffectView, NSWindow, NSWindowAnimationBehavior, NSWindowCollectionBehavior, NSWindowOrderingMode,
    NSWindowStyleMask,
};
use objc2_core_foundation::CGSize;
use objc2_core_graphics::{CGColorSpace, kCGColorSpaceExtendedSRGB};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use objc2_metal::MTLDevice;
use objc2_quartz_core::{CAMetalLayer, kCAGravityTopLeft};

/// Hidden panels kept for the next overlay to open (a menu opened and closed again and again makes none).
const SPARE: usize = 2;

pub struct OverlayIvars {
    /// The main window's view: every pointer event goes on to it.
    main: Retained<HarnessView>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements beyond calling super's designated initialiser; no Drop.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HarnessOverlayView"]
    #[ivars = OverlayIvars]
    pub struct OverlayView;

    unsafe impl NSObjectProtocol for OverlayView {}

    // SAFETY (each forward): the main view is a live HarnessView (it lives as long as the app), and each selector is one it
    // implements, taking one NSEvent and returning nothing.
    impl OverlayView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        /// A press on a menu while the app is in the background acts at once, as NSMenu's does.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            unsafe { msg_send![&*self.ivars().main, mouseDown: event] }
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            unsafe { msg_send![&*self.ivars().main, rightMouseDown: event] }
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            unsafe { msg_send![&*self.ivars().main, otherMouseDown: event] }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            unsafe { msg_send![&*self.ivars().main, mouseUp: event] }
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            unsafe { msg_send![&*self.ivars().main, rightMouseUp: event] }
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            unsafe { msg_send![&*self.ivars().main, otherMouseUp: event] }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            unsafe { msg_send![&*self.ivars().main, mouseDragged: event] }
        }

        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            unsafe { msg_send![&*self.ivars().main, rightMouseDragged: event] }
        }

        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            unsafe { msg_send![&*self.ivars().main, otherMouseDragged: event] }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            unsafe { msg_send![&*self.ivars().main, mouseMoved: event] }
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, event: &NSEvent) {
            unsafe { msg_send![&*self.ivars().main, mouseEntered: event] }
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, event: &NSEvent) {
            unsafe { msg_send![&*self.ivars().main, mouseExited: event] }
        }

        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, event: &NSEvent) {
            unsafe { msg_send![&*self.ivars().main, cursorUpdate: event] }
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            unsafe { msg_send![&*self.ivars().main, scrollWheel: event] }
        }
    }
);

impl OverlayView {
    fn new(mtm: MainThreadMarker, main: Retained<HarnessView>, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(OverlayIvars { main });
        // SAFETY: initWithFrame: is NSView's designated initialiser.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

/// One overlay window: the panel, its view and layer, the overlay it shows (`id`, 0 while spare) and whether it is on
/// screen, and the frame it was last given (screen points) and the drawable size.
struct Panel {
    id: u64,
    panel: Retained<NSPanel>,
    /// The vibrancy behind the view (shown for a frosted overlay), and the corner radius it is cut to (window points).
    glass: Retained<NSVisualEffectView>,
    /// The glass as last set: its frame in the content view, its corner radius and its alpha (window points); None while
    /// hidden.
    corner: Option<(NSRect, f64, f64)>,
    view: Retained<OverlayView>,
    layer: Retained<CAMetalLayer>,
    shown: bool,
    frame: NSRect,
    pixels: CGSize,
    tip: String,
}

/// The overlay windows, by the overlay each shows.
pub struct Overlays {
    main: Retained<HarnessView>,
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    panels: Vec<Panel>,
}

impl Overlays {
    pub fn new(main: Retained<HarnessView>, device: Retained<ProtocolObject<dyn MTLDevice>>) -> Self {
        Self { main, device, panels: Vec::new() } // alloc: the few overlay windows, made once and kept
    }

    /// This frame's overlays into their windows: each placed over its rect (main view points at zoom `zoom`, backing
    /// scale `backing`), its drawable sized to it and taken. The drawables, by float, for the frame's passes (None: a
    /// float whose window has no drawable now, with an error naming it; it is not shown this frame).
    pub fn prepare(&mut self, window: &NSWindow, floats: &[Float], zoom: f32, backing: f32) -> Vec<Option<Dest>> {
        let mtm = self.main.mtm();
        // Panels whose overlay closed are spare for the ones that opened.
        for p in &mut self.panels {
            if p.id != 0 && !floats.iter().any(|f| f.id == p.id) {
                p.id = 0;
            }
        }
        let mut out = Vec::with_capacity(floats.len()); // alloc: one drawable per open overlay, a frame
        for f in floats {
            let at = match self.panels.iter().position(|p| p.id == f.id) {
                Some(i) => i,
                None => match self.panels.iter().position(|p| p.id == 0 && !p.shown).or_else(|| self.panels.iter().position(|p| p.id == 0)) {
                    Some(i) => {
                        self.panels[i].id = f.id;
                        i
                    }
                    None => {
                        let p = self.make(mtm, f.id);
                        self.panels.push(p);
                        self.panels.len() - 1
                    }
                },
            };
            let z = f64::from(zoom);
            // The rect in the main view's points (window points: view points at the zoom), then on the screen.
            let local = NSRect::new(
                NSPoint::new(f64::from(f.rect[0]) * z, f64::from(f.rect[1]) * z),
                NSSize::new(f64::from(f.rect[2]) * z, f64::from(f.rect[3]) * z),
            );
            let frame = window.convertRectToScreen(self.main.convertRect_toView(local, None));
            let p = &mut self.panels[at];
            // The glass where the plate is drawn this frame and as faded as it is (a plain container, so up from the bottom).
            let corner = f.glass.then(|| {
                let [x, y, w, h] = f.plate.map(|v| f64::from(v) * z);
                (NSRect::new(NSPoint::new(x, frame.size.height - y - h), NSSize::new(w, h)), f64::from(f.radius) * z, f64::from(f.alpha))
            });
            if p.corner != corner {
                p.glass.setHidden(corner.is_none());
                if let Some((at, r, a)) = corner {
                    if p.corner.is_none_or(|c| c.0 != at) {
                        p.glass.setFrame(at);
                    }
                    if let Some(l) = p.glass.layer() {
                        l.setCornerRadius(r);
                    }
                    p.glass.setAlphaValue(a);
                }
                p.corner = corner;
            }
            if p.frame != frame {
                p.frame = frame;
                p.panel.setFrame_display(frame, false);
            }
            let pixels = CGSize::new(
                (frame.size.width * f64::from(backing)).round().max(1.0),
                (frame.size.height * f64::from(backing)).round().max(1.0),
            );
            if p.pixels != pixels {
                p.pixels = pixels;
                p.layer.setContentsScale(f64::from(backing));
                p.layer.setDrawableSize(pixels);
            }
            match p.layer.nextDrawable() {
                Some(d) => out.push(Some(Dest::Drawable(d))),
                None => {
                    eprintln!("harness-native: overlay {}: its window's layer gave no drawable: not shown this frame", f.id);
                    out.push(None);
                }
            }
        }
        out
    }

    /// After the frame's passes were committed: each drawn overlay's window ordered in (the first time) and its shadow
    /// shaped again from what it now shows, each tooltip set on its view; the windows of overlays that closed ordered out,
    /// and spare ones past [`SPARE`] let go.
    pub fn shown(&mut self, window: &NSWindow, floats: &[Float], drawn: &[bool], tip: &str) {
        for (f, &drawn) in floats.iter().zip(drawn) {
            let Some(p) = self.panels.iter_mut().find(|p| p.id == f.id) else { continue };
            if !drawn {
                continue;
            }
            if !p.shown {
                p.shown = true;
                // SAFETY: both are live windows of this app; the panel is not already a child of another window.
                unsafe { window.addChildWindow_ordered(&p.panel, NSWindowOrderingMode::Above) };
            }
            p.panel.invalidateShadow();
            if p.tip != tip {
                p.tip = tip.to_owned(); // alloc: the tooltip's words, when they change
                p.view.setToolTip((!tip.is_empty()).then(|| NSString::from_str(tip)).as_deref());
            }
        }
        for p in &mut self.panels {
            if p.id == 0 && p.shown {
                p.shown = false;
                window.removeChildWindow(&p.panel);
                p.panel.orderOut(None);
            }
        }
        let mut spare = 0;
        self.panels.retain(|p| {
            if p.id != 0 {
                return true;
            }
            spare += 1;
            spare <= SPARE
        });
    }

    /// Whether the topmost window at `screen` (screen points, the pointer) is the main window or an overlay's: the
    /// pointer that left one of them for another is still over the app, so its hover holds.
    pub fn holds(&self, window: &NSWindow, screen: NSPoint) -> bool {
        let top = NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(screen, 0, self.main.mtm());
        top == window.windowNumber() || self.panels.iter().any(|p| p.shown && p.panel.windowNumber() == top)
    }

    fn make(&self, mtm: MainThreadMarker, id: u64) -> Panel {
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            rect,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: the pool keeps its own reference; AppKit must not release it on close as well.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        // The window server's shadow, shaped from the drawn plate's alpha (invalidated each drawn frame), as a menu's.
        panel.setHasShadow(true);
        // A panel never takes the keyboard from the composer (it could not: borderless), nor hides with the app (its
        // parent decides), nor animates in (the overlay's own transition does).
        panel.setBecomesKeyOnlyIfNeeded(true);
        panel.setHidesOnDeactivate(false);
        panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::Transient
                | NSWindowCollectionBehavior::IgnoresCycle
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        panel.setAcceptsMouseMovedEvents(true);
        let view = OverlayView::new(mtm, self.main.clone(), rect);
        view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&*self.device));
        layer.setPixelFormat(gpu::PIXEL_FORMAT);
        layer.setMaximumDrawableCount(3);
        layer.setOpaque(false);
        // SAFETY: kCGColorSpaceExtendedSRGB is a valid static colour space name.
        layer.setColorspace(CGColorSpace::with_name(Some(unsafe { kCGColorSpaceExtendedSRGB })).as_deref());
        // EDR as the main window's layer (window.rs): an HDR picture or glowing words in a popover reach the headroom.
        // SAFETY: CADynamicRangeHigh is a valid static dynamic range name.
        layer.setPreferredDynamicRange(unsafe { objc2_quartz_core::CADynamicRangeHigh });
        // SAFETY: kCAGravityTopLeft is a valid static gravity name.
        layer.setContentsGravity(unsafe { kCAGravityTopLeft });
        // Its picture lands with its window's frame (and its ordering in): never a frame of the old place or overlay.
        layer.setPresentsWithTransaction(true);
        view.setLayer(Some(&layer));
        view.setWantsLayer(true);
        // ActiveInActiveApp, not the main view's ActiveInKeyWindow: the panel is never key, the main window is.
        let options = NSTrackingAreaOptions::MouseEnteredAndExited
            | NSTrackingAreaOptions::MouseMoved
            | NSTrackingAreaOptions::CursorUpdate
            | NSTrackingAreaOptions::ActiveInActiveApp
            | NSTrackingAreaOptions::InVisibleRect;
        // SAFETY: the view owns the area and outlives it; no user info.
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(NSTrackingArea::alloc(), NSRect::ZERO, options, Some(&view), None)
        };
        view.addTrackingArea(&area);
        // The glass under the view, filling the panel as the view does; hidden until a frosted overlay takes the panel.
        let glass = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), rect);
        glass.setMaterial(NSVisualEffectMaterial::Popover);
        glass.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        glass.setState(NSVisualEffectState::Active);
        glass.setWantsLayer(true);
        if let Some(l) = glass.layer() {
            l.setMasksToBounds(true);
            // SAFETY: kCACornerCurveContinuous is a valid static corner curve name (the plates' superellipse corners).
            l.setCornerCurve(unsafe { objc2_quartz_core::kCACornerCurveContinuous });
        }
        glass.setHidden(true);
        // Placed each frame where the plate is drawn (a transition grows and shrinks it inside the window), not stretched.
        // Siblings in a plain container, the glass behind: hiding the glass never hides what the view draws.
        let content = NSView::initWithFrame(NSView::alloc(mtm), rect);
        content.addSubview(&glass);
        content.addSubview(&view);
        panel.setContentView(Some(&content));
        Panel {
            id,
            panel,
            glass,
            corner: None,
            view,
            layer,
            shown: false,
            frame: NSRect::ZERO,
            pixels: CGSize::new(0.0, 0.0),
            tip: String::new(),
        }
    }
}

/// The room windowed overlays are kept in: the visible frame of the screen `window` is on (all of it in full screen,
/// where no menu bar or Dock is kept out), in the main view's points from its top-left corner. None: the window is on no
/// screen (offscreen, between screens), and the last room holds.
pub fn screen_room(window: &NSWindow, view: &NSView) -> Option<[f32; 4]> {
    let screen = window.screen()?;
    let full = window.styleMask().contains(NSWindowStyleMask::FullScreen);
    let vis = if full { screen.frame() } else { screen.visibleFrame() };
    // The view is flipped: the rect comes back from its top-left corner.
    let r = view.convertRect_fromView(window.convertRectFromScreen(vis), None);
    Some([r.origin.x as f32, r.origin.y as f32, r.size.width as f32, r.size.height as f32])
}

/// A point of `event` (in whichever of the app's windows it came to) in `view`'s own points.
pub fn local(view: &NSView, event: &NSEvent) -> NSPoint {
    let at = event.locationInWindow();
    match (event.window(view.mtm()), view.window()) {
        (Some(from), Some(to)) if !std::ptr::eq(&*from, &*to) => {
            view.convertPoint_fromView(to.convertPointFromScreen(from.convertPointToScreen(at)), None)
        }
        _ => view.convertPoint_fromView(at, None),
    }
}
