//! Port of AppKit's trackpad / wheel scroll state machine for a default `NSScrollView`
//! (macOS 27.0, AppKit on this machine, reverse-engineered 2026-09-22).
//!
//! AppKit reference chain, one scroll view:
//!   `-[NSScrollingBehaviorSingleThreadedVBL scrollView:scrollWheelWithEvent:]`
//!   -> `-[NSScrollingAnimator scrollWithEvent:]` -> `scrollWithDelta:timestamp:phase:momentumPhase:`
//!   -> `_NSScrollingPredominantAxisFilter` (axis lock + `_NS2DVelocityFilter` = 2 x `_NS1DVelocityFilter`)
//!   -> `-[NSScrollingAnimator _updateWithPhase:delta:velocity:]` (queues "unused delta")
//!   -> `-[NSScrollingAnimator _updateStateWithEvent:]` (6-state machine).
//! Every display-link tick: `-[NSScrollingBehaviorSingleThreadedVBL _advanceTimeWithDisplayLink:]`
//!   -> `-[NSScrollingAnimator consumeUnusedDeltaForTimestamp:handler:]`
//!   -> `-[_NSScrollingRubberbandFilter addDelta:velocity:]` while tracking, or the momentum
//!      calculator while animating -> handler block: `backingAlignedRect:options:0xf` then
//!      `_scrollTo:` with constraining disabled.
//!
//! Momentum (coasting after lift and the rubber-band snap-back) is generated entirely by
//! AppKit's own `_NSScrollingMomentumCatesCalculator`; it is injected here through
//! [`MomentumCalculator`]. OS momentum-phase events never move the view: only a momentum
//! Ended/Cancelled event matters, and it stops the coast unless the view is stretched.
//!
//! Provenance notation: `Class method +N` is the byte offset in the arm64e disassembly of that
//! method in the shared cache (AppKit, macOS 27.0 build on hydra); "measured" means the
//! oracle fixture `/Volumes/Projects/tmp/os-scroll-physics/agentB/oracle2`.
//!
//! Scope and known gaps are listed in the report; the most important:
//! - the content affine transform is assumed identity (AppKit concatenates a flip for flipped
//!   clip views, `-[NSScrollingBehaviorSingleThreadedVBL affineTransformOfContent]`, which
//!   yields identity for a plain scroll view);
//! - cursor-exit cancellation (`isCursorWithinContent`) and the delegate momentum target hook
//!   (`scrollViewBeganMomentum:withVelocity:targetContentOffset:`) are not modelled;
//! - AppKit's internal ~0.3 ms skew between `trackingBeganDate` and the display-link reference
//!   timestamp is not modelled (both use the `now` passed to the Began/first event).

#![allow(clippy::float_cmp)]

// ---------------------------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------------------------

/// Weight of the newest instantaneous velocity in the exponential filter:
/// `v = v*(1-a) + a*(d/dt)`. `-[_NS1DVelocityFilter init] +80..+96` stores 0x3fe999999999999a.
pub const VELOCITY_FILTER_ALPHA: f64 = 0.8;
/// Samples further apart than this reset the velocity filter.
/// `-[_NS1DVelocityFilter addDelta:withTimestamp:] +44` and `_isTimestampOutOfDate: +4` (0.2).
pub const VELOCITY_STALE_INTERVAL: f64 = 0.2;
/// Samples closer than this are dropped entirely (timestamp not advanced).
/// `-[_NS1DVelocityFilter addDelta:withTimestamp:] +124` (0.002).
pub const VELOCITY_MIN_INTERVAL: f64 = 0.002;
/// Hyperbolic stretch coefficient the VBL behaviour installs at the start of every gesture.
/// `-[NSScrollingBehaviorSingleThreadedVBL scrollView:scrollWheelWithEvent:] +304..+312` (0.075).
pub const VBL_HYPERBOLIC_COEFFICIENT: f64 = 0.075;
/// `_NSScrollingRubberbandFilter` default (`init +152`, q (0.0, 0.55)); overridden by the VBL value.
pub const FILTER_DEFAULT_HYPERBOLIC_COEFFICIENT: f64 = 0.55;
/// `NSScrollViewRubberbandDirectionLockStretchRatio()` default (`+48`, fmov 1.0; app-config key).
pub const DIRECTION_LOCK_STRETCH_RATIO: f64 = 1.0;
/// `NSScrollViewRubberbandMinimumRequiredDeltaBeforeStretch()` default (`+48`, fmov 10.0).
pub const MIN_DELTA_BEFORE_STRETCH: f64 = 10.0;
/// Cap of the inverse stretch factor when the stretch reaches the clip size.
/// `-[_NSScrollingRubberbandFilter addDelta:velocity:] +568` (998.9999999999991).
pub const INVERSE_STRETCH_CAP: f64 = 998.999_999_999_999_1;
/// Linear stretch scale used only when clip size or coefficient is not positive
/// (`addDelta:velocity: +1124`, 20.0; same constant as `_NSReboundDeltaForElasticDelta`).
pub const DEGENERATE_REBOUND_SCALE: f64 = 20.0;
/// `_NSExpandedDocFrame` / `_NSStretchOfClipBoundsForDocumentFrame +52` epsilon (0.0001).
pub const EXPANDED_DOC_EPSILON: f64 = 0.0001;
/// Discrete (line) scroll tracking ends this long after the last discrete event.
/// `-[NSScrollingAnimator _startTrackingDiscreteScroll] +52` (0.1).
pub const DISCRETE_SCROLL_TIMEOUT: f64 = 0.1;
/// Momentum ends early when within this distance of the destination (both axes) ...
/// `consumeUnusedDeltaForTimestamp:handler: +1596` (0.05).
pub const MOMENTUM_STOP_DISTANCE: f64 = 0.05;
/// ... and slower than this (`consumeUnusedDeltaForTimestamp:handler: +1624`, 0.5).
pub const MOMENTUM_STOP_SPEED: f64 = 0.5;
/// Above this momentum speed an OS momentum End only stops the coast if the last filtered
/// velocity is at least [`MIN_ACCELERATED_STOPPABLE_VELOCITY`].
/// `scrollWithDelta:timestamp:phase:momentumPhase: +276` (41000.0, app-config key).
pub const ACCELERATED_MOMENTUM_SPEED: f64 = 41000.0;
/// `NSScrollingBehaviorMinAcceleratedStoppableVelocity()` default (`+52`, 70.0).
pub const MIN_ACCELERATED_STOPPABLE_VELOCITY: f64 = 70.0;
/// `NSScrollingVFDVelocityThreshold()` default (`+48`, 320.0): the first momentum frame slower
/// than this sends `scrollingAnimator:velocityHasSlowed:YES`, which drops the display link to
/// [`VFD_REDUCED_FPS`] (measured: frame spacing 8.3 ms -> 16.7 ms on the next frame).
pub const VFD_VELOCITY_THRESHOLD: f64 = 320.0;
/// `NSScrollingVFDReducedFPS()` default (`+48`, 60.0), applied with
/// `setPreferredFrameRateRange:CAFrameRateRangeMake(60, 60, 60)` in
/// `-[NSScrollingBehaviorSingleThreadedVBL scrollingAnimator:velocityHasSlowed:]`. The `NO`
/// notification does nothing there, so the rate stays reduced until the display link is
/// replaced at the next tracking start.
pub const VFD_REDUCED_FPS: f64 = 60.0;
/// `-[NSView backingAlignedRect:options:]` snaps a coordinate to the next pixel only when it is
/// closer than this to it (in pixels); measured with `agentB/align*.m`: 0.99609375 -> 0,
/// 0.9961 -> 1, -0.0039 -> 0, -0.00390625 -> -1.
pub const BACKING_ALIGN_TOLERANCE: f64 = 1.0 / 256.0;
/// `NSScrollView.verticalLineScroll` / `horizontalLineScroll` default (measured: 10).
pub const DEFAULT_LINE_SCROLL: f64 = 10.0;
/// `-[_NSScrollingMomentumCalculator tolerance]` as the animator reads it back: the 6-argument
/// initializer discards the `pointsPerPixel` it is given and stores (1, 1)
/// (agentA, `initWithInitialOrigin:velocity:...:tolerance:` at 0x1870cb8fc; oracle `M` lines
/// print `tol=1` on the 2x display too). Used by the "offset moved under the animation" test
/// in `consumeUnusedDeltaForTimestamp: +620..+700`.
pub const MOMENTUM_CALCULATOR_TOLERANCE: Vec2 = Vec2 { x: 1.0, y: 1.0 };

// ---------------------------------------------------------------------------------------------
// Geometry and event types
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec2 {
    pub x: f64,
    pub y: f64,
}

impl Vec2 {
    pub const ZERO: Vec2 = Vec2 { x: 0.0, y: 0.0 };
    pub const fn new(x: f64, y: f64) -> Self {
        Vec2 { x, y }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub origin: Vec2,
    pub size: Vec2,
}

/// `NSEventPhase` bit values (not the CoreGraphics phase numbers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    None = 0,
    Began = 1,
    Stationary = 2,
    Changed = 4,
    Ended = 8,
    Cancelled = 16,
    MayBegin = 32,
}

impl Phase {
    /// CoreGraphics `kCGScrollWheelEventScrollPhase` value -> NSEventPhase.
    pub fn from_cg_scroll_phase(v: i64) -> Phase {
        match v {
            1 => Phase::Began,
            2 => Phase::Changed,
            4 => Phase::Ended,
            8 => Phase::Cancelled,
            128 => Phase::MayBegin,
            _ => Phase::None,
        }
    }
    /// CoreGraphics `kCGScrollWheelEventMomentumPhase` value -> NSEventPhase.
    pub fn from_cg_momentum_phase(v: i64) -> Phase {
        match v {
            1 => Phase::Began,
            2 => Phase::Changed,
            3 => Phase::Ended,
            _ => Phase::None,
        }
    }
}

/// One `NSEventTypeScrollWheel` event as the scroll view sees it.
#[derive(Clone, Copy, Debug)]
pub struct WheelEvent {
    /// `scrollingDeltaX/Y` (points for precise events, lines for line wheels).
    pub delta: Vec2,
    /// `hasPreciseScrollingDeltas`.
    pub precise: bool,
    pub phase: Phase,
    pub momentum_phase: Phase,
    /// `NSEvent.timestamp`, seconds. Drives velocity estimation only.
    pub timestamp: f64,
}

/// `_NSScrollingPredominantAxisFilter.predominantAxisMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxisFilterMode {
    /// 0: pass both axes.
    Both,
    /// 1: keep x, zero y.
    HorizontalOnly,
    /// 2: keep y, zero x.
    VerticalOnly,
    /// 3 (treated as 4, `filterInputDelta:timestamp:outputDelta:velocity: +68..+76`): per
    /// event, keep the larger axis and zero the other; ties keep y.
    Dominant,
}

/// How the behaviour chooses the axis mode when a gesture starts
/// (`-[NSScrollingBehaviorSingleThreadedVBL _scrollView:scrollingWillBeginWithDelta:]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxisModePolicy {
    /// `_scrollingModeForAxis:` is 0 for both axes: `usesPredominantAxisScrolling ? 3 : 0`.
    /// A default `NSScrollView` reports mode 3 (measured, `axis=3` in every trace).
    Predominant,
    /// `_scrollingModeForAxis:` is 0 for both axes and `usesPredominantAxisScrolling == NO`.
    Free,
    /// Some axis has a non-zero scrolling mode: lock to the first event's larger axis
    /// (`|dy| >= |dx| ? VerticalOnly : HorizontalOnly`, +164..+188).
    LockToFirstEvent,
}

#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    /// `-[NSScrollingBehaviorSingleThreadedVBL scrollableFrame]`: origin = the constrained
    /// minimum clip origin, size = max(clip size, effective document size).
    pub scrollable_frame: Rect,
    /// Clip view bounds size.
    pub clip_size: Vec2,
    /// `convertSizeFromBacking:(1,1)`: 0.5 on a 2x display.
    pub points_per_pixel: f64,
    /// Clip view `isFlipped` (y grows downwards). The whole port is verified for `true`.
    pub flipped: bool,
    /// `_allowsHorizontalStretching` / `_allowsVerticalStretching` of the scroll view
    /// (elasticity).
    pub allows_horizontal_stretching: bool,
    pub allows_vertical_stretching: bool,
    /// `horizontalLineScroll` / `verticalLineScroll` of the scroll view.
    pub line_scroll: Vec2,
    pub axis_mode: AxisModePolicy,
    /// `-[NSScrollingBehaviorSingleThreadedVBL refreshControlDynamicDampeningThreshold]`:
    /// 0 unless the scroll view has a refresh control.
    pub dynamic_dampening_threshold: f64,
}

impl Geometry {
    /// A flipped vertical list: document `content` tall inside a `clip` tall viewport.
    pub fn vertical(width: f64, clip: f64, content: f64, points_per_pixel: f64) -> Geometry {
        Geometry {
            scrollable_frame: Rect {
                origin: Vec2::ZERO,
                size: Vec2::new(width, content.max(clip)),
            },
            clip_size: Vec2::new(width, clip),
            points_per_pixel,
            flipped: true,
            allows_horizontal_stretching: false,
            allows_vertical_stretching: true,
            line_scroll: Vec2::new(DEFAULT_LINE_SCROLL, DEFAULT_LINE_SCROLL),
            axis_mode: AxisModePolicy::Predominant,
            dynamic_dampening_threshold: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Momentum calculator interface (implemented by the Cates port)
// ---------------------------------------------------------------------------------------------

/// Arguments of `-[_NSScrollingMomentumCalculator initWithInitialOrigin:velocity:documentFrame:
/// constrainedClippingOrigin:clippingSize:tolerance:]` as `-[NSScrollingAnimator
/// _momentumCalulatorWithOffset:velocity:] +156..+204` passes them.
#[derive(Clone, Copy, Debug)]
pub struct MomentumInit {
    /// Current clip origin.
    pub initial_origin: Vec2,
    /// Last filtered (origin-space) velocity, points per second.
    pub initial_velocity: Vec2,
    /// `scrollableFrame`.
    pub document_frame: Rect,
    /// `scrollableFrame.origin`.
    pub constrained_clipping_origin: Vec2,
    pub clipping_size: Vec2,
    /// `(pointsPerPixel, pointsPerPixel)` as passed; the calculator ignores it (see
    /// [`MOMENTUM_CALCULATOR_TOLERANCE`]).
    pub tolerance: Vec2,
}

/// The subset of `_NSScrollingMomentumCalculator` that `NSScrollingAnimator` uses.
pub trait MomentumCalculator {
    /// `positionAfterDuration:` — absolute clip origin `t` seconds after momentum start.
    fn position_after_duration(&self, t: f64) -> Vec2;
    /// `velocityAfterDuration:` — scalar speed (points/s) along the initial velocity direction.
    fn velocity_after_duration(&self, t: f64) -> f64;
    /// `durationUntilStop`.
    fn duration_until_stop(&self) -> f64;
    /// `destinationOrigin`.
    fn destination_origin(&self) -> Vec2;
    /// `initialVelocity`.
    fn initial_velocity(&self) -> Vec2;
    /// `isRubberBandingAfterDuration:`; not consulted by `NSScrollingAnimator` itself.
    fn is_rubber_banding_after_duration(&self, t: f64) -> bool;
}

pub type MomentumFactory = Box<dyn FnMut(&MomentumInit) -> Box<dyn MomentumCalculator>>;

// ---------------------------------------------------------------------------------------------
// Velocity filter (_NS1DVelocityFilter)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct VelocityFilter1D {
    /// `_maxAllowedVelocity`; 0 = unclamped (default, never changed on this path).
    max_allowed: f64,
    last_timestamp: f64,
    /// NaN = no estimate.
    velocity: f64,
}

impl VelocityFilter1D {
    fn new() -> Self {
        // init: _maxAllowedVelocity = 0, _lastTimestamp = DBL_MAX, velocity = NaN.
        VelocityFilter1D {
            max_allowed: 0.0,
            last_timestamp: f64::MAX,
            velocity: f64::NAN,
        }
    }
    fn reset(&mut self) {
        self.last_timestamp = f64::MAX;
        self.velocity = f64::NAN;
    }
    /// `-[_NS1DVelocityFilter addDelta:withTimestamp:]`.
    fn add(&mut self, d: f64, t: f64) {
        let dt = t - self.last_timestamp;
        if dt > VELOCITY_STALE_INTERVAL {
            self.reset();
        } else if dt > 0.0 {
            // A delta of opposite sign to the estimate (NaN counts as positive) resets it.
            if d != 0.0 && (d.is_sign_negative() != self.velocity.is_sign_negative()) {
                self.reset();
            }
            if dt < VELOCITY_MIN_INTERVAL {
                return; // dropped without advancing the timestamp (+132..+176)
            }
            let inst = d / dt;
            self.velocity = if self.velocity.is_nan() {
                inst
            } else {
                self.velocity * (1.0 - VELOCITY_FILTER_ALPHA) + VELOCITY_FILTER_ALPHA * inst
            };
        }
        self.last_timestamp = t;
    }
    /// `-[_NS1DVelocityFilter filteredVelocity]`.
    fn filtered(&self) -> f64 {
        let v = if self.velocity.is_nan() {
            0.0
        } else {
            self.velocity
        };
        if self.max_allowed == 0.0 {
            v
        } else if v.is_sign_negative() {
            v.max(-self.max_allowed)
        } else {
            v.min(self.max_allowed)
        }
    }
    fn reset_if_out_of_date(&mut self, t: f64) {
        if t - self.last_timestamp > VELOCITY_STALE_INTERVAL {
            self.reset();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Rubber band filter (_NSScrollingRubberbandFilter), exact transcription
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct RubberbandFilter {
    constrained_origin: Vec2,
    clip: Vec2,
    doc: Rect,
    ppp: f64,
    dampening: f64,
    coeff: f64,
    flipped: bool,
    allow_x: bool,
    allow_y: bool,
    stretched: bool,
    position: Vec2,
    filtered_delta: Vec2,
    filtered_velocity: Vec2,
    /// `_overflow`: carried, rebound-space remainder; survives `setScrollPosition:`,
    /// cleared by `reset`.
    overflow: Vec2,
}

/// `_NSStretchOfClipBoundsForDocumentFrame`: how far the clip rect at `p` lies outside the
/// (expanded) document, per axis. Negative = before the start, positive = past the end
/// (y sign inverted for unflipped views).
fn stretch_of_clip_bounds(
    p: Vec2,
    clip: Vec2,
    doc: Rect,
    constrained: Vec2,
    flipped: bool,
) -> Vec2 {
    let (ex, ew) = if doc.size.x > clip.x + EXPANDED_DOC_EPSILON {
        (doc.origin.x, doc.size.x)
    } else {
        (constrained.x, clip.x)
    };
    let (ey, eh) = if doc.size.y > clip.y + EXPANDED_DOC_EPSILON {
        (doc.origin.y, doc.size.y)
    } else {
        (constrained.y, clip.y)
    };
    // NSIntersectionRect: empty (zero rect) unless both spans overlap with positive length.
    fn span(a0: f64, a1: f64, b0: f64, b1: f64) -> Option<(f64, f64)> {
        let lo = a0.max(b0);
        let hi = a1.min(b1);
        if hi <= lo { None } else { Some((lo, hi - lo)) }
    }
    let both = match (
        span(p.x, p.x + clip.x, ex, ex + ew),
        span(p.y, p.y + clip.y, ey, ey + eh),
    ) {
        (Some(x), Some(y)) => Some((x, y)),
        _ => None,
    };
    let ((ix, iw), (iy, ih)) = match both {
        Some(v) => v,
        None => {
            // Zero intersection: intersect each axis against the full other axis (+168..+256).
            let x = span(p.x, p.x + clip.x, ex, ex + ew).unwrap_or((0.0, 0.0));
            let y = span(p.y, p.y + clip.y, ey, ey + eh).unwrap_or((0.0, 0.0));
            (x, y)
        }
    };
    if p.x == ix && p.y == iy && clip.x == iw && clip.y == ih {
        return Vec2::ZERO;
    }
    let (w, x) = if iw == 0.0 { (ew, ex) } else { (iw, ix) };
    let sx = if p.x < x {
        p.x - x
    } else {
        (p.x + clip.x) - (w + x)
    };
    let (h, y) = if ih == 0.0 { (eh, ey) } else { (ih, iy) };
    let s = if p.y < y {
        y - p.y
    } else {
        (h + y) - (p.y + clip.y)
    };
    Vec2::new(sx, if flipped { -s } else { s })
}

/// Rebound-space offset whose hyperbolic stretch is `s` (+536..+596).
fn inverse_stretch(s: f64, dim: f64, coeff: f64) -> f64 {
    let r = s.abs() / dim;
    let mut f = if r < 1.0 {
        1.0 / (1.0 - r) - 1.0
    } else {
        INVERSE_STRETCH_CAP
    };
    if s < 0.0 {
        f = -f;
    }
    dim * f / coeff
}

/// Hyperbolic stretch `dim * (1 - 1/(coeff*|u|/dim + 1))`, signed like `u` (+1152..+1192).
fn forward_stretch(u: f64, dim: f64, coeff: f64) -> f64 {
    let mut t = 1.0 - 1.0 / (coeff * u.abs() / dim + 1.0);
    if u < 0.0 {
        t = -t;
    }
    dim * t
}

impl RubberbandFilter {
    fn new() -> Self {
        RubberbandFilter {
            constrained_origin: Vec2::ZERO,
            clip: Vec2::ZERO,
            doc: Rect::default(),
            ppp: 1.0,
            dampening: 0.0,
            coeff: FILTER_DEFAULT_HYPERBOLIC_COEFFICIENT,
            flipped: false,
            allow_x: true,
            allow_y: true,
            stretched: false,
            position: Vec2::ZERO,
            filtered_delta: Vec2::ZERO,
            filtered_velocity: Vec2::ZERO,
            overflow: Vec2::ZERO,
        }
    }
    /// `-reset`: zeroes filtered delta, velocity and overflow (0xb0..0xdf).
    fn reset(&mut self) {
        self.filtered_delta = Vec2::ZERO;
        self.filtered_velocity = Vec2::ZERO;
        self.overflow = Vec2::ZERO;
    }
    /// `-setScrollPosition:`: also zeroes filtered delta and velocity, not the overflow.
    fn set_scroll_position(&mut self, p: Vec2) {
        self.position = p;
        self.filtered_delta = Vec2::ZERO;
        self.filtered_velocity = Vec2::ZERO;
    }

    /// `-[_NSScrollingRubberbandFilter addDelta:velocity:]`, transcribed branch for branch
    /// (validated bit-exactly against the real class on 73,952 random steps, `agentB/rbcheck.m`).
    /// `delta` is the scroll delta (positive y moves a flipped origin up); outputs are origin
    /// changes.
    fn add_delta(&mut self, delta: Vec2, velocity: Vec2) {
        let (dx_in, dy_in) = (delta.x, delta.y);
        let mut ex = dx_in + self.overflow.x; // identity content transform
        let mut ey = dy_in + self.overflow.y;
        self.overflow = Vec2::ZERO;
        let sgn = if self.flipped { -1.0 } else { 1.0 };
        let (ppp, cw, ch, coeff, thr) = (
            self.ppp,
            self.clip.x,
            self.clip.y,
            self.coeff,
            self.dampening,
        );
        let s = stretch_of_clip_bounds(
            self.position,
            self.clip,
            self.doc,
            self.constrained_origin,
            self.flipped,
        );
        let (sx, sy) = (s.x, s.y);
        let (asx, asy) = (sx.abs(), sy.abs());
        let mut stretched_x = asx >= ppp; // w22
        let s2 = stretch_of_clip_bounds(
            Vec2::new(self.position.x - ex, sgn * ey + self.position.y),
            self.clip,
            self.doc,
            self.constrained_origin,
            self.flipped,
        );
        let (s2x, s2y) = (s2.x, s2.y);
        // Axes without elasticity clamp at the edge.
        let clamp_y = !self.allow_y && s2y != 0.0;
        if clamp_y {
            ey += s2y;
        }
        let clamp_x = !self.allow_x && s2x != 0.0;
        if clamp_x {
            ex += s2x;
        }
        let mut stretched_y = asy >= ppp; // w20
        let mut will_y = s2y != 0.0 && !clamp_y; // w23
        let mut will_x = s2x != 0.0 && !clamp_x; // w24

        enum Next {
            L720,
            L864,
            X,
            Y,
        }
        let next: Next;
        if asy >= ppp {
            if asx < ppp && will_x {
                // Stretched vertically and x would start stretching: direction lock.
                let dropped = if ey != 0.0 && (ex / ey).abs() < DIRECTION_LOCK_STRETCH_RATIO {
                    true
                } else if ex.abs() >= MIN_DELTA_BEFORE_STRETCH {
                    will_x = true;
                    false
                } else {
                    self.overflow.x += ex;
                    true
                };
                if dropped {
                    stretched_x = false;
                    ex = 0.0;
                    next = Next::Y;
                } else {
                    next = Next::L720;
                }
            } else {
                next = Next::L720;
            }
        } else if asx >= ppp {
            if !will_y {
                next = Next::X;
            } else if ex != 0.0 && (ey / ex).abs() < DIRECTION_LOCK_STRETCH_RATIO {
                ey = 0.0;
                next = Next::X;
            } else if ey.abs() >= MIN_DELTA_BEFORE_STRETCH {
                will_y = true;
                next = Next::L864;
            } else {
                self.overflow.y += ey;
                ey = 0.0;
                next = Next::X;
            }
        } else if !will_x {
            next = Next::L864;
        } else if ey.abs() < ex.abs() || ex.abs() >= MIN_DELTA_BEFORE_STRETCH {
            will_x = true;
            next = Next::L720;
        } else {
            // y dominates and x has not moved far enough to start stretching: bank x.
            let nx = ex + self.overflow.x;
            will_x = false;
            ex = 0.0;
            self.overflow.x = nx;
            next = Next::L864;
        }

        let (run_x, run_y) = match next {
            Next::X => (true, false),
            Next::Y => (false, true),
            Next::L720 | Next::L864 => {
                if let Next::L720 = next {
                    // Entering an x stretch from in range: stop at the edge first, carry the rest.
                    if !(asx >= ppp) && will_x {
                        let t = s2x + ex;
                        if t.abs() >= ppp {
                            let o = ex - t;
                            will_x = false;
                            ex = t;
                            self.overflow.x = o;
                        } else {
                            will_x = true;
                        }
                    }
                }
                // L864: same for y.
                let mut decided: Option<(bool, bool)> = None;
                if !(asy >= ppp) && will_y {
                    let t = s2y + ey;
                    if t.abs() >= ppp {
                        let w8 = asx >= ppp;
                        self.overflow.y = ey - t;
                        let w9 = if w8 || !will_x { w8 } else { ex > 0.0 };
                        ey = t;
                        decided = Some((w9, stretched_y));
                    } else {
                        will_y = true;
                    }
                }
                match decided {
                    Some(v) => v,
                    None => {
                        // L896. Note the asymmetric `> 0` tests (AppKit behaviour, verified).
                        let w8 = asx >= ppp;
                        let w9 = if w8 || !will_x { w8 } else { ex > 0.0 };
                        let y = if !(asy >= ppp) && will_y {
                            ey > 0.0
                        } else {
                            stretched_y
                        };
                        (w9, y)
                    }
                }
            }
        };
        if run_x {
            let u = if cw > 0.0 && coeff > 0.0 {
                inverse_stretch(sx, cw, coeff)
            } else {
                DEGENERATE_REBOUND_SCALE * sx
            };
            let up = u - ex;
            let mut sp = if cw > 0.0 {
                forward_stretch(up, cw, coeff)
            } else {
                up / DEGENERATE_REBOUND_SCALE
            };
            // Stretch is kept on whole points, rounded towards the direction of travel.
            sp = if ex > 0.0 { sp.ceil() } else { sp.floor() };
            let upp = if cw > 0.0 && coeff > 0.0 {
                inverse_stretch(sp, cw, coeff)
            } else {
                DEGENERATE_REBOUND_SCALE * sp
            };
            ex = sx - sp;
            self.overflow.x = upp - up;
            stretched_x = if asx >= ppp { true } else { ex.abs() > ppp };
        }
        if run_y {
            if thr > 0.0 && sy < 0.0 {
                // Dynamic dampening (refresh control): 1:1 up to `thr`, hyperbolic beyond.
                let mut o = 0.0;
                if !(asy < thr) {
                    let e = asy - thr;
                    if e > 0.0 {
                        let degenerate = ch.min(coeff) <= 0.0;
                        let u = if degenerate {
                            -DEGENERATE_REBOUND_SCALE * e
                        } else {
                            -inverse_stretch(e, ch, coeff)
                        };
                        let up = u - ey;
                        let mut sp = if ch > 0.0 {
                            forward_stretch(up, ch, coeff)
                        } else {
                            up / DEGENERATE_REBOUND_SCALE
                        };
                        sp = if ey > 0.0 { sp.ceil() } else { sp.floor() };
                        ey = thr + (sy + sp.abs());
                        o = if degenerate {
                            sp * DEGENERATE_REBOUND_SCALE - up
                        } else {
                            inverse_stretch(sp, ch, coeff) - up
                        };
                    }
                }
                self.overflow.y = o;
            } else {
                let degenerate = ch.min(coeff) <= 0.0;
                let u = if degenerate {
                    sy * DEGENERATE_REBOUND_SCALE
                } else {
                    inverse_stretch(sy, ch, coeff)
                };
                let up = u - ey;
                let mut sp = if ch > 0.0 {
                    forward_stretch(up, ch, coeff)
                } else {
                    up / DEGENERATE_REBOUND_SCALE
                };
                sp = if ey > 0.0 { sp.ceil() } else { sp.floor() };
                ey = sy - sp;
                self.overflow.y = if degenerate {
                    sp * DEGENERATE_REBOUND_SCALE - up
                } else {
                    inverse_stretch(sp, ch, coeff) - up
                };
            }
            stretched_y = if asy >= ppp { true } else { ey.abs() > ppp };
        }
        self.stretched = stretched_x || stretched_y;
        // Velocity passes through (sign-converted to origin space) unless the axis is pinned.
        let mut vel_y = velocity.y;
        if ey == 0.0 && dy_in != 0.0 {
            vel_y = 0.0;
        }
        let vel_x = if ex == 0.0 && dx_in != 0.0 {
            -0.0
        } else {
            -velocity.x
        };
        let dy_out = if self.flipped { -ey } else { ey };
        let vel_y = if self.flipped { -vel_y } else { vel_y };
        self.position.x -= ex;
        self.position.y += dy_out;
        self.filtered_delta.x -= ex;
        self.filtered_delta.y += dy_out;
        self.filtered_velocity = Vec2::new(vel_x, vel_y);
    }

    /// Geometry refresh done before every `addDelta:` in `consumeUnusedDeltaForTimestamp:`.
    fn refresh(&mut self, g: &Geometry, origin: Vec2) {
        self.doc = g.scrollable_frame;
        self.constrained_origin = g.scrollable_frame.origin;
        self.set_scroll_position(origin);
        self.dampening = g.dynamic_dampening_threshold;
    }
}

// ---------------------------------------------------------------------------------------------
// Animator state machine
// ---------------------------------------------------------------------------------------------

/// `animatorStateString.stateNames`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle = 0,
    MayBegin = 1,
    PhysicalGesture = 2,
    PhysicalDiscrete = 3,
    Animating = 4,
    Paused = 5,
}

/// `animatorEventString.eventNames`. `MouseExited` comes from the cursor-exit check this port
/// does not model; it is kept so the transition table matches AppKit's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
enum Event {
    GestureMayBegin = 0,
    GestureChanged = 1,
    GestureEnded = 2,
    Cancel = 3,
    Discrete = 4,
    MouseExited = 5,
    AnimationComplete = 6,
}

/// Display-link state owned by the VBL behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayLink {
    /// No link: frames are ignored.
    Off,
    Running,
    /// `scrollingAnimatorPauseConsumption:` -> `setPaused:YES`.
    Paused,
}

pub struct ScrollAnimator {
    geometry: Geometry,
    origin: Vec2,
    state: State,
    axis_mode: AxisFilterMode,
    vel_x: VelocityFilter1D,
    vel_y: VelocityFilter1D,
    rubberband: RubberbandFilter,
    momentum_factory: MomentumFactory,
    momentum: Option<Box<dyn MomentumCalculator>>,
    unused_delta: Vec2,
    last_event_velocity: Vec2,
    last_filtered_velocity: Vec2,
    last_known_content_frame: Rect,
    last_known_scroll_offset: Vec2,
    /// Seconds since tracking began (NSDate clock in AppKit).
    momentum_start_timestamp: f64,
    last_known_momentum_timestamp: f64,
    required_min_velocity_for_momentum: f64,
    /// `_trackingBeganDate`.
    tracking_began_at: f64,
    /// Behaviour `_refTimestamp` (display-link clock base).
    ref_timestamp: f64,
    /// Behaviour `_isInTracking`.
    in_tracking: bool,
    display_link: DisplayLink,
    /// `_hasNotifiedOfSlowVelocity` (0xbd).
    has_notified_of_slow_velocity: bool,
    /// The display link runs at [`VFD_REDUCED_FPS`].
    link_rate_reduced: bool,
    discrete_deadline: Option<f64>,
}

impl ScrollAnimator {
    pub fn new(geometry: Geometry, origin: Vec2, momentum_factory: MomentumFactory) -> Self {
        let mut rubberband = RubberbandFilter::new();
        rubberband.flipped = geometry.flipped;
        ScrollAnimator {
            geometry,
            origin,
            state: State::Idle,
            axis_mode: AxisFilterMode::Dominant,
            vel_x: VelocityFilter1D::new(),
            vel_y: VelocityFilter1D::new(),
            rubberband,
            momentum_factory,
            momentum: None,
            unused_delta: Vec2::ZERO,
            last_event_velocity: Vec2::ZERO,
            last_filtered_velocity: Vec2::ZERO,
            last_known_content_frame: Rect::default(),
            last_known_scroll_offset: Vec2::ZERO,
            momentum_start_timestamp: 0.0,
            last_known_momentum_timestamp: 0.0,
            required_min_velocity_for_momentum: 0.0,
            tracking_began_at: 0.0,
            ref_timestamp: 0.0,
            in_tracking: false,
            display_link: DisplayLink::Off,
            has_notified_of_slow_velocity: false,
            link_rate_reduced: false,
            discrete_deadline: None,
        }
    }

    pub fn origin(&self) -> Vec2 {
        self.origin
    }
    pub fn state(&self) -> State {
        self.state
    }
    pub fn display_link(&self) -> DisplayLink {
        self.display_link
    }
    /// Frames are wanted only while the display link runs.
    pub fn wants_frames(&self) -> bool {
        self.display_link == DisplayLink::Running
    }
    /// `Some(60.0)` once the momentum has slowed below [`VFD_VELOCITY_THRESHOLD`]: AppKit's
    /// display link then ticks at 60 Hz until tracking ends. `None` = native refresh rate.
    pub fn preferred_frame_rate(&self) -> Option<f64> {
        (self.display_link != DisplayLink::Off && self.link_rate_reduced).then_some(VFD_REDUCED_FPS)
    }
    /// Origin-space velocity handed to the last frame (`_lastFilteredVelocity`).
    pub fn last_filtered_velocity(&self) -> Vec2 {
        self.last_filtered_velocity
    }
    /// Next pending timer (discrete-scroll timeout); the host calls [`Self::on_timer`] then.
    pub fn next_timer_deadline(&self) -> Option<f64> {
        self.discrete_deadline
    }
    /// Programmatic scroll (`scrollToPoint:`). While animating, the next frame rebuilds the
    /// momentum calculator from here (`consumeUnusedDeltaForTimestamp: +660..+700`).
    pub fn set_origin(&mut self, origin: Vec2) {
        self.origin = origin;
    }
    pub fn set_geometry(&mut self, geometry: Geometry) {
        self.geometry = geometry;
        self.rubberband.flipped = geometry.flipped;
    }

    /// Fires due timers (the 0.1 s discrete-scroll timeout).
    pub fn on_timer(&mut self, now: f64) {
        if let Some(deadline) = self.discrete_deadline {
            if now >= deadline {
                self.discrete_deadline = None;
                // _discreteScrollTimeout: -> _updateWithPhase:Ended delta:0 velocity:0
                self.update_with_phase(Phase::Ended, Vec2::ZERO, Vec2::ZERO, now);
            }
        }
    }

    /// `-[NSScrollingBehaviorSingleThreadedVBL scrollView:scrollWheelWithEvent:]` followed by
    /// `-[NSScrollingAnimator scrollWithEvent:]`. `now` is the dispatch time in the display-link
    /// clock (`CACurrentMediaTime`): it stamps tracking start and momentum start.
    pub fn on_wheel(&mut self, ev: &WheelEvent, now: f64) {
        self.on_timer(now);
        if !self.in_tracking {
            // _scrollView:scrollingWillBeginWithDelta: + calculator class, min velocity 0,
            // coefficient 0.075 (+200..+312).
            let d = ev.delta;
            self.axis_mode = match self.geometry.axis_mode {
                AxisModePolicy::Predominant => AxisFilterMode::Dominant,
                AxisModePolicy::Free => AxisFilterMode::Both,
                AxisModePolicy::LockToFirstEvent => {
                    if d.y.abs() >= d.x.abs() {
                        AxisFilterMode::VerticalOnly
                    } else {
                        AxisFilterMode::HorizontalOnly
                    }
                }
            };
            self.rubberband.allow_x = self.geometry.allows_horizontal_stretching;
            self.rubberband.allow_y = self.geometry.allows_vertical_stretching;
            self.required_min_velocity_for_momentum = 0.0;
            self.rubberband.coeff = VBL_HYPERBOLIC_COEFFICIENT;
        }
        // scrollWithEvent: (+88..+312)
        let (delta, phase, momentum_phase) = if ev.precise {
            (ev.delta, ev.phase, ev.momentum_phase)
        } else if ev.phase == Phase::Ended {
            (Vec2::ZERO, Phase::Ended, Phase::None)
        } else {
            (
                Vec2::new(
                    ev.delta.x * self.geometry.line_scroll.x,
                    ev.delta.y * self.geometry.line_scroll.y,
                ),
                Phase::None,
                ev.momentum_phase,
            )
        };
        self.scroll_with_delta(delta, ev.timestamp, phase, momentum_phase, now);
    }

    /// `-[NSScrollingAnimator scrollWithDelta:timestamp:phase:momentumPhase:]`.
    fn scroll_with_delta(
        &mut self,
        delta: Vec2,
        timestamp: f64,
        phase: Phase,
        momentum_phase: Phase,
        now: f64,
    ) {
        if momentum_phase != Phase::None {
            // OS momentum events never move the view. Ended/Cancelled may stop AppKit's coast.
            if matches!(momentum_phase, Phase::Ended | Phase::Cancelled) {
                if let Some(calc) = &self.momentum {
                    let iv = calc.initial_velocity();
                    let speed = (iv.x * iv.x + iv.y * iv.y).sqrt().abs();
                    let threshold = if speed > ACCELERATED_MOMENTUM_SPEED {
                        MIN_ACCELERATED_STOPPABLE_VELOCITY
                    } else {
                        0.0
                    };
                    if self.last_filtered_velocity.y.abs() >= threshold
                        || self.last_filtered_velocity.x.abs() >= threshold
                    {
                        self.update_state(Event::Cancel, now);
                    }
                }
            }
            return;
        }
        let (out_delta, velocity) = self.filter_input(delta, timestamp, phase);
        self.update_with_phase(phase, out_delta, velocity, now);
    }

    /// `-[_NSScrollingPredominantAxisFilter filterInputDelta:timestamp:phase:momentumPhase:
    /// outputDelta:velocity:]` (momentum phase is always None here).
    fn filter_input(&mut self, delta: Vec2, timestamp: f64, phase: Phase) -> (Vec2, Vec2) {
        match phase {
            Phase::Began | Phase::Changed | Phase::None => {
                let d = match self.axis_mode {
                    AxisFilterMode::Both => delta,
                    AxisFilterMode::HorizontalOnly => Vec2::new(delta.x, 0.0),
                    AxisFilterMode::VerticalOnly => Vec2::new(0.0, delta.y),
                    AxisFilterMode::Dominant => {
                        if delta.y.abs() < delta.x.abs() {
                            Vec2::new(delta.x, 0.0)
                        } else {
                            Vec2::new(0.0, delta.y)
                        }
                    }
                };
                self.vel_x.add(d.x, timestamp);
                self.vel_y.add(d.y, timestamp);
                (d, Vec2::new(self.vel_x.filtered(), self.vel_y.filtered()))
            }
            Phase::Ended => {
                self.vel_x.reset_if_out_of_date(timestamp);
                self.vel_y.reset_if_out_of_date(timestamp);
                (
                    Vec2::ZERO,
                    Vec2::new(self.vel_x.filtered(), self.vel_y.filtered()),
                )
            }
            Phase::Stationary | Phase::Cancelled | Phase::MayBegin => {
                self.vel_x.reset();
                self.vel_y.reset();
                (Vec2::ZERO, Vec2::ZERO)
            }
        }
    }

    /// `-[NSScrollingAnimator _updateWithPhase:delta:velocity:]`.
    fn update_with_phase(&mut self, phase: Phase, delta: Vec2, velocity: Vec2, now: f64) {
        self.unused_delta.x += delta.x;
        self.unused_delta.y += delta.y;
        self.last_event_velocity = velocity;
        let event = match phase {
            Phase::MayBegin => {
                self.last_filtered_velocity = Vec2::ZERO;
                Event::GestureMayBegin
            }
            Phase::Began => {
                self.last_filtered_velocity = Vec2::ZERO;
                Event::GestureChanged
            }
            Phase::Changed => Event::GestureChanged,
            Phase::Ended => Event::GestureEnded,
            Phase::Cancelled => Event::Cancel,
            Phase::None | Phase::Stationary => Event::Discrete,
        };
        self.update_state(event, now);
    }

    /// `-[NSScrollingAnimator _updateStateWithEvent:]`. Combinations AppKit asserts on are
    /// ignored here.
    fn update_state(&mut self, event: Event, now: f64) {
        use Event::*;
        match (self.state, event) {
            (State::Idle, GestureChanged) | (State::MayBegin, GestureChanged) => {
                self.start_tracking_gesture(now)
            }
            (State::Idle, Discrete) | (State::MayBegin, Discrete) => {
                self.start_tracking_discrete(now)
            }
            (State::Idle, GestureMayBegin) => self.state = State::MayBegin, // _trackMayBegin
            (State::Idle, _) => {}
            (State::MayBegin, GestureEnded | Cancel | MouseExited) => self.reset(), // _cancelTrackingGestureScroll
            (State::MayBegin, _) => {}

            (State::PhysicalGesture, GestureEnded | MouseExited) => {
                // Flush what is still queued (AppKit passes the NSDate-based time; unused in
                // this state), then start momentum from the last filtered velocity. The
                // flush feeds the velocity but does not move the view: with a delta queued
                // at release, AppKit stayed at 400 and started its calculator there with
                // the flushed velocity (3541.253 pt/s), ending 30 pt short of a port that
                // applied the delta (parity run10, 2026-09-22, research/scroll-physics.md).
                let origin = self.origin;
                self.consume(now - self.tracking_began_at);
                self.origin = origin;
                self.rubberband.reset();
                if event == GestureEnded && self.required_min_velocity_for_momentum > 0.0 {
                    let v = self.last_filtered_velocity;
                    let m = self.required_min_velocity_for_momentum;
                    if v.x * v.x + v.y * v.y < m * m {
                        self.last_filtered_velocity = Vec2::ZERO;
                    }
                }
                self.start_momentum_animation(now);
            }
            (State::PhysicalGesture, Cancel) => {
                self.rubberband.reset();
                self.start_momentum_animation(now);
            }
            (State::PhysicalGesture, _) => {}

            (State::PhysicalDiscrete, GestureChanged) => {
                self.discrete_deadline = None; // _cancelTrackingDiscreteScroll
                self.state = State::PhysicalGesture;
            }
            (State::PhysicalDiscrete, GestureEnded) => {
                // _endTrackingDiscreteScroll without a momentum target: back to idle.
                self.state = State::Idle;
                self.tracking_ended();
            }
            (State::PhysicalDiscrete, Discrete) => {
                self.discrete_deadline = Some(now + DISCRETE_SCROLL_TIMEOUT)
            }
            (State::PhysicalDiscrete, _) => {}

            (State::Animating, GestureMayBegin) => {
                self.momentum = None; // _stopAnimating
                if self.display_link == DisplayLink::Running {
                    self.display_link = DisplayLink::Paused;
                }
                self.state = State::Paused;
            }
            (State::Animating, GestureChanged) => {
                self.momentum = None;
                self.has_notified_of_slow_velocity = false; // velocityHasSlowed:NO (no-op in VBL)
                self.state = State::PhysicalGesture;
            }
            (State::Animating, Cancel) => {
                if self.stop_animating_if_allowed() {
                    self.end_tracking_gesture();
                }
            }
            (State::Animating, AnimationComplete) => self.end_tracking_gesture(),
            (State::Animating, _) => {}

            (State::Paused, GestureEnded | Cancel | MouseExited) => {
                self.has_notified_of_slow_velocity = false;
                self.continue_consumption();
                self.last_event_velocity = Vec2::ZERO;
                self.start_momentum_animation(now);
            }
            (State::Paused, GestureChanged) => {
                self.has_notified_of_slow_velocity = false;
                self.continue_consumption();
                self.state = State::PhysicalGesture;
            }
            (State::Paused, _) => {}
        }
    }

    fn continue_consumption(&mut self) {
        if self.display_link == DisplayLink::Paused {
            self.display_link = DisplayLink::Running;
        }
    }

    /// `_reset`: note it keeps `lastEventVelocity` and `lastFilteredVelocity`.
    fn reset(&mut self) {
        self.unused_delta = Vec2::ZERO;
        self.momentum = None;
        self.last_known_content_frame = Rect::default();
        self.last_known_scroll_offset = Vec2::ZERO;
        self.last_known_momentum_timestamp = 0.0;
        self.has_notified_of_slow_velocity = false; // strh wzr, [x19, #0xbc]
        self.state = State::Idle;
    }

    fn start_tracking_gesture(&mut self, now: f64) {
        self.state = State::PhysicalGesture;
        self.common_start_tracking(now);
    }

    fn start_tracking_discrete(&mut self, now: f64) {
        self.state = State::PhysicalDiscrete;
        self.discrete_deadline = Some(now + DISCRETE_SCROLL_TIMEOUT);
        self.common_start_tracking(now);
    }

    /// `_commonStartTrackingScroll` + `updateFilters` + `scrollingAnimatorTrackingBegan:`.
    fn common_start_tracking(&mut self, now: f64) {
        self.tracking_began_at = now;
        self.rubberband.reset();
        self.rubberband.doc = self.geometry.scrollable_frame;
        self.rubberband.constrained_origin = self.geometry.scrollable_frame.origin;
        self.rubberband.clip = self.geometry.clip_size;
        self.rubberband.ppp = self.geometry.points_per_pixel;
        self.rubberband.flipped = self.geometry.flipped;
        self.rubberband.dampening = self.geometry.dynamic_dampening_threshold;
        // Behaviour: new display link, reference timestamp, _isInTracking = YES.
        self.ref_timestamp = now;
        self.display_link = DisplayLink::Running;
        self.link_rate_reduced = false;
        self.in_tracking = true;
    }

    /// `scrollingAnimatorTrackingEnded:`: display link invalidated, `_isInTracking = NO`.
    fn tracking_ended(&mut self) {
        self.display_link = DisplayLink::Off;
        self.in_tracking = false;
        self.discrete_deadline = None;
    }

    /// `_endTrackingGestureScroll`.
    fn end_tracking_gesture(&mut self) {
        self.reset();
        self.tracking_ended();
    }

    /// `_momentumCalulatorWithOffset:velocity:` (the VBL momentum-target hook returns the
    /// destination unchanged for a scroll view without a delegate override).
    fn make_momentum(&mut self, origin: Vec2, velocity: Vec2) -> Box<dyn MomentumCalculator> {
        let g = &self.geometry;
        let ppp = g.points_per_pixel;
        let init = MomentumInit {
            initial_origin: origin,
            initial_velocity: velocity,
            document_frame: g.scrollable_frame,
            constrained_clipping_origin: g.scrollable_frame.origin,
            clipping_size: g.clip_size,
            tolerance: Vec2::new(ppp, ppp),
        };
        (self.momentum_factory)(&init)
    }

    /// `_startMomentumAnimation` + `_cacheMomentumAnimationIvars`.
    fn start_momentum_animation(&mut self, now: f64) {
        let calc = self.make_momentum(self.origin, self.last_filtered_velocity);
        self.last_known_content_frame = self.geometry.scrollable_frame;
        self.last_known_scroll_offset = self.origin;
        self.momentum_start_timestamp = now - self.tracking_began_at;
        self.last_known_momentum_timestamp = self.momentum_start_timestamp;
        let duration = calc.duration_until_stop();
        self.momentum = Some(calc);
        self.state = State::Animating;
        if duration <= 0.0 {
            self.update_state(Event::AnimationComplete, now);
        }
    }

    /// `_stopAnimatingIfAllowed`: an OS momentum End does not stop a stretched view.
    fn stop_animating_if_allowed(&mut self) -> bool {
        let g = self.geometry;
        self.rubberband.refresh(&g, self.origin);
        self.rubberband.add_delta(Vec2::ZERO, Vec2::ZERO);
        if self.rubberband.stretched {
            return false;
        }
        self.momentum = None;
        true
    }

    /// One display-link tick. `target_timestamp` is `CADisplayLink.targetTimestamp` (the
    /// presentation time of the frame being prepared), in the same clock as `now`.
    /// Returns the new clip origin.
    pub fn on_frame(&mut self, target_timestamp: f64) -> Vec2 {
        if self.display_link != DisplayLink::Running {
            return self.origin;
        }
        let t = target_timestamp - self.ref_timestamp;
        self.consume(t);
        self.origin
    }

    /// `-[NSScrollingAnimator consumeUnusedDeltaForTimestamp:handler:]` plus the handler block
    /// of `_advanceTimeWithDisplayLink:`.
    fn consume(&mut self, t: f64) {
        let d = self.unused_delta;
        let v = self.last_event_velocity;
        self.unused_delta = Vec2::ZERO;
        let mut finished = false;
        let mut slowed = false;
        let (delta, velocity) = match self.state {
            State::Paused => (Vec2::ZERO, Vec2::ZERO),
            State::PhysicalGesture | State::PhysicalDiscrete => {
                if d == Vec2::ZERO {
                    // Keep the previous filtered velocity per axis unless the event velocity
                    // on that axis is exactly zero (+456..+492, +1652).
                    let vx = if v.x == 0.0 {
                        0.0
                    } else {
                        self.last_filtered_velocity.x
                    };
                    let vy = if v.y == 0.0 {
                        0.0
                    } else {
                        self.last_filtered_velocity.y
                    };
                    (Vec2::ZERO, Vec2::new(vx, vy))
                } else {
                    let g = self.geometry;
                    self.rubberband.refresh(&g, self.origin);
                    if self.state == State::PhysicalDiscrete {
                        // Line scrolling never stretches (+972..+1064).
                        let (ax, ay) = (self.rubberband.allow_x, self.rubberband.allow_y);
                        self.rubberband.allow_x = false;
                        self.rubberband.allow_y = false;
                        self.rubberband.add_delta(d, v);
                        self.rubberband.allow_x = ax;
                        self.rubberband.allow_y = ay;
                    } else {
                        self.rubberband.add_delta(d, v);
                    }
                    (
                        self.rubberband.filtered_delta,
                        self.rubberband.filtered_velocity,
                    )
                }
            }
            State::Animating => {
                let (delta, velocity, done, speed) = self.consume_momentum(t);
                finished = done;
                slowed = speed < VFD_VELOCITY_THRESHOLD;
                (delta, velocity)
            }
            // AppKit asserts here and hands the raw values to the handler.
            State::Idle | State::MayBegin => (d, v),
        };
        self.last_filtered_velocity = velocity;
        self.apply(delta);
        if finished {
            self.update_state(Event::AnimationComplete, t + self.ref_timestamp);
        } else if slowed && !self.has_notified_of_slow_velocity {
            // _delegateVelocityHasSlowed:YES -> 60 Hz display link (+1336..+1356).
            self.has_notified_of_slow_velocity = true;
            self.link_rate_reduced = true;
        }
    }

    /// Animating branch of `consumeUnusedDeltaForTimestamp:handler:` (+548..+1760).
    fn consume_momentum(&mut self, t: f64) -> (Vec2, Vec2, bool, f64) {
        let offset = self.origin;
        let frame = self.geometry.scrollable_frame;
        let tol = MOMENTUM_CALCULATOR_TOLERANCE;
        let unchanged = self.last_known_content_frame == frame
            && !((self.last_known_scroll_offset.x - offset.x).abs() > tol.x)
            && !((self.last_known_scroll_offset.y - offset.y).abs() > tol.y);
        if !unchanged {
            // Content or offset changed under the animation: restart the calculator from the
            // current offset with the speed it had at the last frame (+704..+1508).
            let calc = self
                .momentum
                .as_ref()
                .expect("animating without calculator");
            let iv = calc.initial_velocity();
            let n = iv.x * iv.x + iv.y * iv.y;
            let dir = if n > 0.0 {
                Vec2::new(iv.x / n.sqrt(), iv.y / n.sqrt())
            } else {
                Vec2::ZERO
            };
            let speed = calc.velocity_after_duration(
                self.last_known_momentum_timestamp - self.momentum_start_timestamp,
            );
            let new_calc = self.make_momentum(offset, Vec2::new(dir.x * speed, dir.y * speed));
            self.momentum = Some(new_calc);
            self.last_known_scroll_offset = offset;
            self.last_known_content_frame = frame;
            self.momentum_start_timestamp = self.last_known_momentum_timestamp;
        }
        let dt = t - self.momentum_start_timestamp;
        let calc = self
            .momentum
            .as_ref()
            .expect("animating without calculator");
        let pos = calc.position_after_duration(dt);
        let speed = calc.velocity_after_duration(dt);
        let dest = calc.destination_origin();
        let duration = calc.duration_until_stop();
        let done = dt >= duration
            || ((dest.x - offset.x).abs() < MOMENTUM_STOP_DISTANCE
                && (dest.y - offset.y).abs() < MOMENTUM_STOP_DISTANCE
                && speed < MOMENTUM_STOP_SPEED);
        let target = if done { dest } else { pos };
        self.last_known_scroll_offset = target;
        let iv = calc.initial_velocity();
        let n = iv.x * iv.x + iv.y * iv.y;
        let dir = if n > 0.0 {
            Vec2::new(iv.x / n.sqrt(), iv.y / n.sqrt())
        } else {
            Vec2::ZERO
        };
        self.last_known_momentum_timestamp = t;
        (
            Vec2::new(target.x - offset.x, target.y - offset.y),
            Vec2::new(speed * dir.x, speed * dir.y),
            done,
            speed,
        )
    }

    /// Handler block of `_advanceTimeWithDisplayLink:`: skip a zero delta, otherwise move the
    /// clip origin to `backingAlignedRect:{origin + delta, 0x0} options:NSAlignAllEdgesInward`
    /// without constraining (so stretched origins survive).
    fn apply(&mut self, delta: Vec2) {
        if delta == Vec2::ZERO {
            return;
        }
        let ppp = self.geometry.points_per_pixel;
        let target = Vec2::new(self.origin.x + delta.x, self.origin.y + delta.y);
        // Alignment happens on the backing pixel grid, which moves with the current bounds
        // origin (measured with a non-aligned bounds origin of 320.3).
        let px = (target.x - self.origin.x) / ppp;
        let py = (target.y - self.origin.y) / ppp;
        let ax = align_ceil(px);
        let ay = if self.geometry.flipped {
            align_floor(py)
        } else {
            align_ceil(py)
        };
        self.origin = Vec2::new(self.origin.x + ax * ppp, self.origin.y + ay * ppp);
    }
}

/// Inward alignment of a minimum edge in a flipped view: floor, unless within 1/256 px of the
/// next pixel.
fn align_floor(v: f64) -> f64 {
    let c = v.ceil();
    if c - v < BACKING_ALIGN_TOLERANCE {
        c
    } else {
        v.floor()
    }
}

/// Inward alignment of the x edge: ceil, unless within 1/256 px of the previous pixel.
fn align_ceil(v: f64) -> f64 {
    let f = v.floor();
    if v - f < BACKING_ALIGN_TOLERANCE {
        f
    } else {
        v.ceil()
    }
}

// ---------------------------------------------------------------------------------------------
// Tests: recorded AppKit oracle traces
// ---------------------------------------------------------------------------------------------

