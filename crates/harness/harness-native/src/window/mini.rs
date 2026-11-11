//! The window tucked away (the live step line's `tuck` button, scene/widgets/steps.flecs): the main window is ordered
//! out and the turn's pixel-art item floats alone at the screen's top right corner, in a small panel over every
//! window and Space, still moving as the step changes (harness-native-render's block.rs, the picture the transcript
//! draws). When the turn ends the item becomes a cup of tea (or a cross, on an error) and hops; a press on it, or the
//! Dock icon, brings the window back on that chat.
//!
//! The panel is AppKit's alone: a layer-backed view whose layer holds the item's picture as an image, texels
//! unfiltered, on a solid plate with one hairline (the app's rule for what floats). No Metal: the display link is
//! paused while the window is out of sight, and a timer here steps the picture a dozen times a second.
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSEvent, NSPanel, NSResponder, NSView, NSWindow, NSWindowAnimationBehavior,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_graphics::{CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer};
use objc2_quartz_core::{CALayer, CATransaction, kCAFilterNearest};

use harness_native_render::block;

/// Points a texel of the item is drawn at, the plate's padding round it, and its gap from the screen's corner.
const TEXEL: f64 = 3.0;
const PAD: f64 = 7.0;
const MARGIN: f64 = 14.0;
/// Steps of the picture a second.
const HZ: f64 = 12.0;
/// How far the item hops (points up) at each step after its turn ends well, and how far it shakes (points across)
/// after it ends on an error; then it rests.
const HOP: [f64; 14] = [0.0, 4.0, 7.0, 8.0, 7.0, 4.0, 0.0, 0.0, 3.0, 5.0, 3.0, 0.0, 2.0, 0.0];
const SHAKE: [f64; 8] = [-3.0, 3.0, -3.0, 3.0, -2.0, 2.0, -1.0, 0.0];

define_class!(
    // SAFETY: NSView has no subclassing requirements beyond calling super's designated initialiser; no Drop.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HarnessMiniView"]
    pub struct MiniView;

    unsafe impl NSObjectProtocol for MiniView {}

    impl MiniView {
        /// A press while another app is in front acts at once.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            super::untuck();
        }
    }
);

/// The tucked-away window's item, floating.
pub struct Mini {
    panel: Retained<NSPanel>,
    plate: Retained<CALayer>,
    art: Retained<CALayer>,
    timer: Retained<NSTimer>,
    /// The chat it stands for.
    pub sid: u64,
    /// Where it rests (screen points, AppKit's: from the bottom left).
    home: NSPoint,
    /// When it was shown and when its turn ended (media time), and the step last drawn with its item.
    since: f64,
    ended: Option<f64>,
    drawn: Option<(u64, &'static str)>,
}

impl Mini {
    /// The item for chat `sid`, shown at the top right of `window`'s screen; `timer` steps it.
    pub fn show(mtm: MainThreadMarker, window: &NSWindow, sid: u64, timer: Retained<NSTimer>, now: f64) -> Self {
        let side = block::SIDE as f64 * TEXEL + PAD * 2.0;
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(side, side));
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            rect,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: this struct keeps its own reference; AppKit must not release it on close as well.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(true);
        panel.setHidesOnDeactivate(false);
        panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
        // Over every window (NSFloatingWindowLevel) on every Space, and out of the window cycle.
        panel.setLevel(3);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        let view: Retained<MiniView> = unsafe { msg_send![MiniView::alloc(mtm), initWithFrame: rect] };
        view.setWantsLayer(true);
        view.setToolTip(Some(&NSString::from_str("Bring the window back")));
        // The plate: the app's ground, solid, one hairline, the radius of what floats.
        let plate = CALayer::new();
        plate.setFrame(rect);
        plate.setCornerRadius(8.0);
        plate.setBorderWidth(1.0);
        plate.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(0.039, 0.043, 0.051, 0.96).CGColor()));
        let art = CALayer::new();
        art.setFrame(NSRect::new(NSPoint::new(PAD, PAD), NSSize::new(side - PAD * 2.0, side - PAD * 2.0)));
        // SAFETY: kCAFilterNearest is a valid static filter name: texels are shown as squares, never smoothed.
        art.setMagnificationFilter(unsafe { kCAFilterNearest });
        plate.addSublayer(&art);
        if let Some(root) = view.layer() {
            root.addSublayer(&plate);
        }
        panel.setContentView(Some(&view));
        let room = window.screen().map_or(rect, |s| s.visibleFrame());
        let home = NSPoint::new(room.origin.x + room.size.width - side - MARGIN, room.origin.y + room.size.height - side - MARGIN);
        panel.setFrameOrigin(home);
        let mut mini = Self { panel, plate, art, timer, sid, home, since: now, ended: None, drawn: None };
        mini.step(now, "think", true);
        mini.panel.orderFrontRegardless();
        mini
    }

    /// The picture at `now`: item `face`, moving; once its turn no longer `running`, hopping (or, for `fail`, shaking)
    /// from where it stood, then at rest, its hairline in the ending's colour.
    pub fn step(&mut self, now: f64, face: &'static str, running: bool) {
        match (running, self.ended) {
            (false, None) => self.ended = Some(now),
            // Another turn began while it was away.
            (true, Some(_)) => self.ended = None,
            _ => {}
        }
        let at = ((now - self.since) * HZ) as u64;
        if self.drawn == Some((at, face)) {
            return;
        }
        self.drawn = Some((at, face));
        let k = self.ended.map(|e| ((now - e) * HZ) as usize);
        let (dx, dy) = match (k, face) {
            (Some(k), "fail") => (SHAKE.get(k).copied().unwrap_or(0.0), 0.0),
            (Some(k), _) => (0.0, HOP.get(k).copied().unwrap_or(0.0)),
            (None, _) => (0.0, 0.0),
        };
        self.panel.setFrameOrigin(NSPoint::new(self.home.x + dx, self.home.y + dy));
        let edge = match (self.ended.is_some(), face) {
            (true, "fail") => (0.94, 0.43, 0.43, 0.9),
            (true, _) => (0.59, 0.75, 0.43, 0.9),
            _ => (1.0, 1.0, 1.0, 0.14),
        };
        // No implicit animation: a layer's change lands at once, as everything in the app does.
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.plate.setBorderColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(edge.0, edge.1, edge.2, edge.3).CGColor()));
        self.draw(&block::picture(face, (now - self.since) as f32));
        CATransaction::commit();
    }

    /// `texels` (the item's side squared, straight RGBA) as the art layer's image.
    fn draw(&self, texels: &[u8]) {
        let n = block::SIDE;
        // CoreGraphics takes premultiplied alpha.
        let mut buf: Vec<u8> = texels // alloc: one small picture a step
            .chunks_exact(4)
            .flat_map(|p| {
                let a = u16::from(p[3]);
                [(u16::from(p[0]) * a / 255) as u8, (u16::from(p[1]) * a / 255) as u8, (u16::from(p[2]) * a / 255) as u8, p[3]]
            })
            .collect();
        let Some(space) = CGColorSpace::new_device_rgb() else { return };
        // SAFETY: the buffer is n*n*4 bytes, 4 a pixel, RGBA premultiplied (kCGImageAlphaPremultipliedLast = 1); the
        // image is made (a copy) before the buffer is dropped.
        let image = unsafe {
            let Some(ctx) = CGBitmapContextCreate(buf.as_mut_ptr().cast(), n, n, 8, n * 4, Some(&space), 1) else { return };
            CGBitmapContextCreateImage(Some(&ctx))
        };
        let Some(image) = image else { return };
        // SAFETY: a CGImage is an object a layer takes as its contents.
        unsafe {
            let obj: &AnyObject = &*(objc2_core_foundation::CFRetained::as_ptr(&image).as_ptr().cast::<AnyObject>());
            self.art.setContents(Some(obj));
        }
    }

    /// Gone: its timer stopped, its panel closed.
    pub fn close(self) {
        self.timer.invalidate();
        self.panel.orderOut(None);
        self.panel.close();
    }
}
