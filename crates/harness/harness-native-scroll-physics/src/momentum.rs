//! Pure-Rust port of AppKit's private `_NSScrollingMomentumCatesCalculator`,
//! the momentum (inertial scroll) model a default `NSScrollView` uses after a
//! trackpad gesture lifts with velocity (macOS 27.0, AppKit on hydra, reverse
//! engineered 2026-09-22).
//!
//! Provenance: every method and C helper was disassembled with lldb against the
//! loaded AppKit (listings in /Volumes/Projects/tmp/os-scroll-physics/probe/dis2.txt
//! and /Volumes/Projects/tmp/os-scroll-physics/agentA/h1.txt, h3.txt, h5.txt).
//! Addresses below are the macOS 27.0 shared-cache addresses of this boot. The
//! C mirror of this exact code in
//! /Volumes/Projects/tmp/os-scroll-physics/agentA/cates_probe.m matched the real
//! class bit for bit (2,725 comparisons, 0 mismatches); this file is a line by
//! line transcription of that mirror, keeping AppKit's operation order, its
//! fused multiply-adds (`fmadd` -> `mul_add`) and its libm calls (`pow`, `exp`,
//! `hypot` from libsystem_m, which Rust's `powf`/`exp`/`hypot` also call on
//! macOS).
//!
//! Units: points and seconds. Coordinates are those of a flipped document view
//! (y grows downward), which is the only orientation the class supports: both
//! of its `_NSStretchOfClipBoundsForDocumentFrame` calls hardcode `flipped = 1`.
//! The class has no points-per-pixel input: `NSScrollingAnimator` passes
//! `pointsPerPixel` as the `tolerance:` argument of the 6-argument initializer,
//! and that initializer discards it and stores tolerance (1, 1)
//! (`-[_NSScrollingMomentumCalculator initWithInitialOrigin:velocity:...:tolerance:]`
//! at 0x1870cb8fc; confirmed by reading `_tolerance` after init, cates_probe.m
//! "INFO" line). Rounding, when enabled, rounds to whole points.

// ---------------------------------------------------------------------------
// Constants. AppKit reads the first five through `_NSGetCGFloatAppConfig`
// (user defaults, key in the comment); the values are the compiled fallbacks.

/// `NSScrollingBehaviorMomentumConstant`, used whenever `_decelerationRate <= 0`
/// (the default path). Fallback literal at 0x187a64270 (loaded from
/// `calculateDestination` +1156, `_catesCalculateCachedProperties` +364,
/// `_catesPositionAfterDuration` +776).
pub const MOMENTUM_CONSTANT: f64 = 1000.0;
/// `NSScrollViewRubberbandStiffness`, `NSScrollViewRubberbandStiffness()` at
/// 0x18749bccc, literal `fmov d0, #20.0`.
pub const RUBBERBAND_STIFFNESS: f64 = 20.0;
/// `NSScrollViewRubberbandAmplitude`, literal at 0x187a65400
/// (`_NSElasticDeltaForTimeDelta2` +204).
pub const RUBBERBAND_AMPLITUDE: f64 = 0.31;
/// `NSScrollViewRubberbandPeriod`, literal at 0x187a64410
/// (`_NSElasticDeltaForTimeDelta2` +240).
pub const RUBBERBAND_PERIOD: f64 = 1.6;
/// `NSScrollViewScrollVelocityMax`, literal at 0x187a67798
/// (`NSScrollViewScrollVelocityMax()` +52).
pub const SCROLL_VELOCITY_MAX: f64 = 20000.0;

/// Exponent of the displacement law, literal at 0x187a677e8 (0x3ff5555555555555,
/// `calculateDestination` +248, `_catesCalculateToReachDestination` +208).
const DISPLACEMENT_EXPONENT: f64 = 4.0 / 3.0;
/// Width/height slack of `_NSExpandedDocFrame` and
/// `_NSStretchOfClipBoundsForDocumentFrame`, literal at 0x187a64548.
const DOC_EPSILON: f64 = 0.0001;
/// Bisection stop width of the settle search, literal at 0x187a64778
/// (`_catesCalculateCachedProperties` +184).
const SETTLE_BISECTION_WIDTH: f64 = 0.01;
/// Upper bound of the settle search after rubber-banding starts
/// (`fmov d0/d14, #1.5`, `_catesCalculateCachedProperties` +172).
const SETTLE_SEARCH_MAX: f64 = 1.5;
/// Elastic offset under which the bounce counts as settled (`fmov d12, #0.5`).
const SETTLE_OFFSET: f64 = 0.5;
/// Finite-difference step of the rubber-band speed, literal at 0x187a64920
/// (`velocityAfterDuration:` +228).
const VELOCITY_DT: f64 = 0.001;
/// Durations above this on the default path re-target the destination to the
/// 2 s position (`calculateDestination` +1004, `fmov d1, #2.0`).
const RETARGET_DURATION: f64 = 2.0;
/// `velocityVectorAfterDuration:` zero threshold, 0x3e80000000000000 = 2^-23.
const VECTOR_EPSILON: f64 = 1.0 / 8388608.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}
impl Size {
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}
impl Rect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            origin: Point::new(x, y),
            size: Size::new(width, height),
        }
    }
    const ZERO: Rect = Rect::new(0.0, 0.0, 0.0, 0.0);
}

/// `_rubberBandingAxis` (q at ivar offset 256): 0 none, 1 horizontal, 2 vertical.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RubberBandAxis {
    None,
    Horizontal,
    Vertical,
}

/// Mirrors the ivars of `_NSScrollingMomentumCalculator` (offsets 16..152) and
/// `_NSScrollingMomentumCatesCalculator` (offsets 160..256).
#[derive(Clone, Debug)]
pub struct CatesMomentum {
    initial_origin: Point,
    initial_velocity: Point,
    destination_origin: Point,
    document_frame: Rect,
    constrained_clipping_origin: Point,
    clipping_size: Size,
    tolerance: Size,
    deceleration_rate: f64,
    should_round: bool,
    duration_until_stop: f64,
    duration_until_stop_ignoring_rubberbanding: f64,
    abs_displacement_ignoring_rubberbanding: Size,
    linear_displacement_ignoring_rubberbanding: f64,
    destination_ignoring_rubberbanding: Point,
    /// Negative (-1) when the trajectory never reaches an edge.
    duration_until_rubberband: f64,
    initial_rubberbanding_origin: Point,
    initial_rubberbanding_velocity: Point,
    rubber_banding_axis: RubberBandAxis,
}

/// AppKit's inline point rounding (e.g. `calculateDestination` +332..+380):
/// snap to the nearest integer when within 1/8, then round half toward zero.
fn round_point_value(x: f64) -> f64 {
    let nearest = x.round(); // frinta: ties away from zero
    let x = if (x - nearest).abs() < 0.125 {
        nearest
    } else {
        x
    };
    if x > 0.0 {
        (x + -0.5).ceil()
    } else {
        (x + 0.5).floor()
    }
}

fn copysign_one(v: f64) -> f64 {
    1.0f64.copysign(v)
}

/// `_NSElasticDeltaForTimeDelta2(pos, vel, t, stiffness)` at 0x187235134:
/// `exp(-(t * stiffness) / period) * (pos + (t * -clamp(vel)) * amplitude)`,
/// the inner sum an `fmadd`.
fn elastic_delta(position: f64, velocity: f64, t: f64, stiffness: f64) -> f64 {
    let decay = (-(t * stiffness) / RUBBERBAND_PERIOD).exp();
    let capped = if velocity < SCROLL_VELOCITY_MAX {
        velocity
    } else {
        SCROLL_VELOCITY_MAX
    };
    let negated = if capped < -SCROLL_VELOCITY_MAX {
        SCROLL_VELOCITY_MAX
    } else {
        -capped
    };
    decay * (t * negated).mul_add(RUBBERBAND_AMPLITUDE, position)
}

/// Foundation `NSIntersectionRect` at 0x18382de24.
fn intersection(a: Rect, b: Rect) -> Rect {
    if !(a.size.width > 0.0)
        || !(b.size.width > 0.0)
        || !(a.size.height > 0.0)
        || !(b.size.height > 0.0)
    {
        return Rect::ZERO;
    }
    let x0 = if a.origin.x > b.origin.x {
        a.origin.x
    } else {
        b.origin.x
    };
    let a_max_x = a.origin.x + a.size.width;
    let b_max_x = b.origin.x + b.size.width;
    let x1 = if a_max_x < b_max_x { a_max_x } else { b_max_x };
    if !(x1 > x0) {
        return Rect::ZERO;
    }
    let y0 = if a.origin.y > b.origin.y {
        a.origin.y
    } else {
        b.origin.y
    };
    let a_max_y = a.origin.y + a.size.height;
    let b_max_y = b.origin.y + b.size.height;
    let y1 = if a_max_y < b_max_y { a_max_y } else { b_max_y };
    if !(y1 > y0) {
        return Rect::ZERO;
    }
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

/// `_NSExpandedDocFrame` at 0x18749bf08: an axis whose document is not longer
/// than the clip (plus 1e-4) is replaced by the clip's own span.
fn expanded_document_frame(document: Rect, clip_origin: Point, clip_size: Size) -> Rect {
    let (x, width) = if document.size.width > clip_size.width + DOC_EPSILON {
        (document.origin.x, document.size.width)
    } else {
        (clip_origin.x, clip_size.width)
    };
    let (y, height) = if document.size.height > clip_size.height + DOC_EPSILON {
        (document.origin.y, document.size.height)
    } else {
        (clip_origin.y, clip_size.height)
    };
    Rect::new(x, y, width, height)
}

/// `_NSStretchOfClipBoundsForDocumentFrame(flipped = 1, ...)` at 0x186751b60:
/// how far a clip rect at `origin` sticks out of the (expanded) document,
/// negative past the top/left, positive past the bottom/right, (0, 0) inside.
fn stretch_of_clip(origin: Point, clip_size: Size, document: Rect, clip_origin: Point) -> Point {
    let e = expanded_document_frame(document, clip_origin, clip_size);
    let clip = Rect::new(origin.x, origin.y, clip_size.width, clip_size.height);
    let overlap = intersection(clip, e);
    let (rx, ry, rw, rh) = if overlap == Rect::ZERO {
        let column = intersection(
            Rect::new(origin.x, e.origin.y, clip_size.width, e.size.height),
            e,
        );
        let row = intersection(
            Rect::new(e.origin.x, origin.y, e.size.width, clip_size.height),
            e,
        );
        (
            column.origin.x,
            row.origin.y,
            column.size.width,
            row.size.height,
        )
    } else {
        (
            overlap.origin.x,
            overlap.origin.y,
            overlap.size.width,
            overlap.size.height,
        )
    };
    if clip == Rect::new(rx, ry, rw, rh) {
        return Point::new(0.0, 0.0);
    }
    let (w, x) = if rw == 0.0 {
        (e.size.width, e.origin.x)
    } else {
        (rw, rx)
    };
    let sx = if origin.x < x {
        origin.x - x
    } else {
        (origin.x + clip_size.width) - (w + x)
    };
    let (h, y) = if rh == 0.0 {
        (e.size.height, e.origin.y)
    } else {
        (rh, ry)
    };
    let sy = if origin.y < y {
        y - origin.y
    } else {
        (h + y) - (origin.y + clip_size.height)
    };
    // flipped = 1 negates the vertical term (+412..+420).
    Point::new(sx, -sy)
}

impl CatesMomentum {
    /// The path `NSScrollingAnimator _momentumCalulatorWithOffset:velocity:`
    /// takes: `initWithInitialOrigin:velocity:documentFrame:constrainedClippingOrigin:clippingSize:tolerance:`
    /// (0x1870cb8fc), which forwards tolerance (1, 1), deceleration rate 0 (use
    /// `MOMENTUM_CONSTANT`) and no rounding to the designated initializer. The
    /// animator passes `scrollableFrame` as the document frame and its origin as
    /// the constrained clipping origin.
    pub fn new(
        initial_origin: Point,
        velocity: Point,
        document_frame: Rect,
        constrained_clipping_origin: Point,
        clipping_size: Size,
    ) -> Self {
        Self::with_options(
            initial_origin,
            velocity,
            document_frame,
            constrained_clipping_origin,
            clipping_size,
            Size::new(1.0, 1.0),
            0.0,
            false,
        )
    }

    /// `initWithInitialOrigin:initialVelocity:documentFrame:constrainedClippingOrigin:clippingSize:tolerance:decelerationRate:shouldRoundCalculations:`
    /// (0x1870cb844): stores the ivars, then `[self calculateDestination]`.
    /// `deceleration_rate <= 0` selects `MOMENTUM_CONSTANT`.
    #[allow(clippy::too_many_arguments)] // mirrors the AppKit designated initializer
    pub fn with_options(
        initial_origin: Point,
        velocity: Point,
        document_frame: Rect,
        constrained_clipping_origin: Point,
        clipping_size: Size,
        tolerance: Size,
        deceleration_rate: f64,
        should_round: bool,
    ) -> Self {
        let mut m = Self {
            initial_origin,
            initial_velocity: velocity,
            destination_origin: Point::default(),
            document_frame,
            constrained_clipping_origin,
            clipping_size,
            tolerance,
            deceleration_rate,
            should_round,
            duration_until_stop: 0.0,
            duration_until_stop_ignoring_rubberbanding: 0.0,
            abs_displacement_ignoring_rubberbanding: Size::default(),
            linear_displacement_ignoring_rubberbanding: 0.0,
            destination_ignoring_rubberbanding: Point::default(),
            duration_until_rubberband: 0.0,
            initial_rubberbanding_origin: Point::default(),
            initial_rubberbanding_velocity: Point::default(),
            rubber_banding_axis: RubberBandAxis::None,
        };
        m.calculate_destination();
        m
    }

    fn k(&self) -> f64 {
        if self.deceleration_rate > 0.0 {
            self.deceleration_rate
        } else {
            MOMENTUM_CONSTANT
        }
    }

    fn round_if(&self, x: f64) -> f64 {
        if self.should_round {
            round_point_value(x)
        } else {
            x
        }
    }

    /// Where the scroll comes to rest (`destinationOrigin`).
    pub fn destination(&self) -> (f64, f64) {
        (self.destination_origin.x, self.destination_origin.y)
    }

    /// `durationUntilStop` (ivar at 160).
    /// `initialVelocity`, points per second.
    pub fn initial_velocity(&self) -> Point {
        self.initial_velocity
    }

    pub fn duration_until_stop(&self) -> f64 {
        self.duration_until_stop
    }

    /// Time at which the scroll reaches the document edge, if it does.
    pub fn duration_until_rubber_band(&self) -> Option<f64> {
        (self.duration_until_rubberband >= 0.0).then_some(self.duration_until_rubberband)
    }

    pub fn rubber_banding_axis(&self) -> RubberBandAxis {
        self.rubber_banding_axis
    }

    /// `-isRubberBandingAfterDuration:` at 0x1870cd140.
    pub fn is_rubber_banding_after(&self, t: f64) -> bool {
        self.duration_until_rubberband >= 0.0 && !(t < self.duration_until_rubberband)
    }

    /// `-positionAfterDuration:` -> `_catesPositionAfterDuration` at 0x1870cc654.
    pub fn position_after(&self, t: f64) -> (f64, f64) {
        let p = self.position_point(t);
        (p.x, p.y)
    }

    fn position_point(&self, t: f64) -> Point {
        let mut r = self.destination_origin;
        let rubber = self.is_rubber_banding_after(t);
        if rubber {
            let since = t - self.duration_until_rubberband;
            match self.rubber_banding_axis {
                RubberBandAxis::Vertical => {
                    let e = elastic_delta(
                        -self.initial_rubberbanding_origin.y,
                        self.initial_rubberbanding_velocity.y,
                        since,
                        RUBBERBAND_STIFFNESS,
                    );
                    r.y = self.destination_origin.y - self.round_if(e);
                }
                RubberBandAxis::Horizontal => {
                    let e = elastic_delta(
                        -self.initial_rubberbanding_origin.x,
                        self.initial_rubberbanding_velocity.x,
                        since,
                        RUBBERBAND_STIFFNESS,
                    );
                    r.x = self.destination_origin.x - self.round_if(e);
                }
                RubberBandAxis::None => {}
            }
        }
        if !(self.linear_displacement_ignoring_rubberbanding > 0.0) {
            return Point::new(
                self.round_if(self.destination_origin.x),
                self.round_if(self.destination_origin.y),
            );
        }
        let k = self.k();
        let lin = self.linear_displacement_ignoring_rubberbanding;
        // fmaxnm(T - t, 0): a NaN remainder becomes 0, as f64::max does.
        let remaining = (self.duration_until_stop_ignoring_rubberbanding - t).max(0.0);
        let fraction = (lin - k * remaining.powf(4.0)) / lin;
        if !(rubber && self.rubber_banding_axis == RubberBandAxis::Horizontal) {
            let d = self.round_if(
                fraction * (self.destination_ignoring_rubberbanding.x - self.initial_origin.x),
            );
            r.x = self.initial_origin.x + d;
            if rubber && self.rubber_banding_axis == RubberBandAxis::Vertical {
                return r;
            }
        }
        let d = self.round_if(
            fraction * (self.destination_ignoring_rubberbanding.y - self.initial_origin.y),
        );
        r.y = self.initial_origin.y + d;
        r
    }

    /// `-velocityAfterDuration:` at 0x1870ccf5c: scalar speed in points/s.
    /// Free phase: `4k (T - t)^3` (not clamped: negative past `T`). Rubber-band
    /// phase: a 1 ms forward difference of the elastic law.
    pub fn velocity_after(&self, t: f64) -> f64 {
        if self.duration_until_rubberband >= 0.0 && t >= self.duration_until_rubberband {
            let since = t - self.duration_until_rubberband;
            let o = self.initial_rubberbanding_origin;
            let v = self.initial_rubberbanding_velocity;
            let ax = elastic_delta(o.x, v.x, since, RUBBERBAND_STIFFNESS);
            let ay = elastic_delta(o.y, v.y, since, RUBBERBAND_STIFFNESS);
            let later = since + VELOCITY_DT;
            let bx = elastic_delta(o.x, v.x, later, RUBBERBAND_STIFFNESS);
            let by = elastic_delta(o.y, v.y, later, RUBBERBAND_STIFFNESS);
            return (bx - ax).abs().hypot((by - ay).abs()) / VELOCITY_DT;
        }
        (self.k() * 4.0) * (self.duration_until_stop_ignoring_rubberbanding - t).powf(3.0)
    }

    /// Base-class `-velocityVectorAfterDuration:` at 0x1870cbf7c: the scalar
    /// speed along the (possibly re-targeted) initial velocity direction.
    pub fn velocity_vector_after(&self, t: f64) -> (f64, f64) {
        let speed = self.velocity_after(t);
        let v = self.initial_velocity;
        let len = v.x.mul_add(v.x, v.y * v.y).sqrt();
        if len.abs() > VECTOR_EPSILON && speed.abs() > VECTOR_EPSILON {
            (speed * v.x / len, speed * v.y / len)
        } else {
            (0.0, 0.0)
        }
    }

    /// `_catesCalculateCachedProperties` at 0x1870cc4d0.
    fn calculate_cached_properties(&mut self) {
        let k = self.k();
        let o = self.initial_origin;
        let d = self.destination_ignoring_rubberbanding;
        self.abs_displacement_ignoring_rubberbanding =
            Size::new((o.x - d.x).abs(), (o.y - d.y).abs());
        let abs = self.abs_displacement_ignoring_rubberbanding;
        self.linear_displacement_ignoring_rubberbanding = abs.width.hypot(abs.height);
        self.duration_until_stop_ignoring_rubberbanding =
            (self.linear_displacement_ignoring_rubberbanding / k).powf(0.25);
        if !(self.duration_until_rubberband >= 0.0) {
            self.duration_until_stop = self.duration_until_stop_ignoring_rubberbanding;
            return;
        }
        // Bisection for the first time both elastic offsets fall under 0.5 pt.
        let (ro, rv) = (
            self.initial_rubberbanding_origin,
            self.initial_rubberbanding_velocity,
        );
        let mut lo = 0.0;
        let mut hi = SETTLE_SEARCH_MAX;
        let mut span = SETTLE_SEARCH_MAX;
        loop {
            let mid = span * 0.5 + lo;
            let ex = elastic_delta(ro.x, rv.x, mid, RUBBERBAND_STIFFNESS).abs();
            let ey = elastic_delta(ro.y, rv.y, mid, RUBBERBAND_STIFFNESS).abs();
            if ex < SETTLE_OFFSET && ey < SETTLE_OFFSET {
                hi = mid;
            } else {
                lo = mid;
            }
            span = hi - lo;
            if !(span > SETTLE_BISECTION_WIDTH) {
                break;
            }
        }
        self.duration_until_stop = lo + self.duration_until_rubberband;
    }

    /// `-[_NSScrollingMomentumCatesCalculator calculateDestination]` at 0x1870cc034.
    fn calculate_destination(&mut self) {
        let k = self.k();
        let k4 = k * 4.0;
        self.duration_until_rubberband = -1.0;
        let v = self.initial_velocity;
        let (sx, sy) = (copysign_one(v.x), copysign_one(v.y));
        let dx = sx.mul_add(
            k * (v.x.abs() / k4).powf(DISPLACEMENT_EXPONENT),
            self.initial_origin.x,
        );
        self.destination_ignoring_rubberbanding.x = self.round_if(dx);
        let dy = sy.mul_add(
            k * (v.y.abs() / k4).powf(DISPLACEMENT_EXPONENT),
            self.initial_origin.y,
        );
        self.destination_ignoring_rubberbanding.y = self.round_if(dy);
        self.calculate_cached_properties();

        let expanded = expanded_document_frame(
            self.document_frame,
            self.constrained_clipping_origin,
            self.clipping_size,
        );
        let s = stretch_of_clip(
            self.destination_ignoring_rubberbanding,
            self.clipping_size,
            expanded,
            self.constrained_clipping_origin,
        );
        self.rubber_banding_axis = RubberBandAxis::None;
        if s.x.abs() < self.tolerance.width && s.y.abs() < self.tolerance.height {
            self.destination_origin = self.destination_ignoring_rubberbanding;
            self.duration_until_rubberband = -1.0;
        } else {
            // The dominant axis (larger free displacement, ties vertical) may
            // rubber-band; the other axis is clamped to the document edge.
            let abs = self.abs_displacement_ignoring_rubberbanding;
            let (axis, offset, dominant_stretch) = if abs.height >= abs.width {
                self.destination_ignoring_rubberbanding.x -= s.x;
                (RubberBandAxis::Vertical, Point::new(0.0, s.y), s.y)
            } else {
                self.destination_ignoring_rubberbanding.y -= s.y;
                (RubberBandAxis::Horizontal, Point::new(s.x, 0.0), s.x)
            };
            let d = self.destination_ignoring_rubberbanding;
            self.destination_origin = Point::new(d.x - offset.x, d.y - offset.y);
            if dominant_stretch == 0.0 || dominant_stretch.is_nan() {
                // Only the non-dominant axis overshot: clamp it, no bounce.
                self.calculate_cached_properties();
                self.destination_origin = self.destination_ignoring_rubberbanding;
                self.duration_until_rubberband = -1.0;
            } else {
                self.rubber_banding_axis = axis;
                self.calculate_cached_properties();
                let s0 = stretch_of_clip(
                    self.initial_origin,
                    self.clipping_size,
                    expanded,
                    self.constrained_clipping_origin,
                );
                if s0.x == 0.0 && s0.y == 0.0 {
                    // Starts inside: the free curve reaches the edge at t_rb.
                    let lin = self.linear_displacement_ignoring_rubberbanding;
                    let o = self.initial_origin;
                    let dest = self.destination_origin;
                    let dist = (dest.x - o.x).abs().hypot((dest.y - o.y).abs());
                    let total = self.duration_until_stop_ignoring_rubberbanding;
                    let t_rb = total - ((lin - dist) / k).powf(0.25);
                    self.duration_until_rubberband = t_rb;
                    let speed = k4 * (total - t_rb).powf(3.0);
                    let abs = self.abs_displacement_ignoring_rubberbanding;
                    self.initial_rubberbanding_velocity = Point::new(
                        sx * (abs.width / lin * speed),
                        sy * (abs.height / lin * speed),
                    );
                } else {
                    // Already stretched when momentum begins: bounce from here.
                    self.initial_rubberbanding_origin = s0;
                    self.duration_until_rubberband = 0.0;
                    self.initial_rubberbanding_velocity = self.initial_velocity;
                    self.destination_ignoring_rubberbanding = self.destination_origin;
                }
                self.calculate_cached_properties();
            }
        }
        if !(self.deceleration_rate > 0.0)
            && self.duration_until_stop_ignoring_rubberbanding > RETARGET_DURATION
            && self.duration_until_rubberband < 0.0
        {
            self.destination_origin = self.position_point(RETARGET_DURATION);
            self.calculate_to_reach_destination();
        }
    }

    /// `_catesCalculateToReachDestination` at 0x1870cc974: fit velocity or
    /// deceleration so the curve ends exactly at `destination_origin`.
    fn calculate_to_reach_destination(&mut self) {
        let k = self.k();
        let k4 = k * 4.0;
        self.duration_until_rubberband = -1.0;
        self.destination_ignoring_rubberbanding = self.destination_origin;
        let v = self.initial_velocity;
        let (sx, sy) = (copysign_one(v.x), copysign_one(v.y));
        let mut dx0 = sx * (k * (v.x.abs() / k4).powf(DISPLACEMENT_EXPONENT));
        let mut dy0 = sy * (k * (v.y.abs() / k4).powf(DISPLACEMENT_EXPONENT));
        let mut ddx = self.destination_origin.x - self.initial_origin.x;
        let mut ddy = self.destination_origin.y - self.initial_origin.y;
        if self.should_round {
            dx0 = round_point_value(dx0);
            dy0 = round_point_value(dy0);
            ddx = round_point_value(ddx);
            ddy = round_point_value(ddy);
        }
        let reach = dx0.mul_add(dx0, dy0 * dy0).sqrt();
        let needed = ddx.mul_add(ddx, ddy * ddy).sqrt();
        let total_cubed = self.duration_until_stop_ignoring_rubberbanding.powf(3.0);
        if !(needed > 0.5) {
            // "Already at destination, just stop" (the cached pass below
            // recomputes both durations from the remaining displacement).
            self.duration_until_stop = 0.0;
            self.duration_until_stop_ignoring_rubberbanding = 0.0;
            self.calculate_cached_properties();
            return;
        }
        let speed = k4 * total_cubed;
        let changed_direction = (ddx != 0.0 && ddx.is_sign_negative() != dx0.is_sign_negative())
            || (ddy != 0.0 && ddy.is_sign_negative() != dy0.is_sign_negative());
        if changed_direction || needed > reach {
            self.initial_velocity = Point::new(ddx / needed * speed, ddy / needed * speed);
        } else {
            // "we'd go too far on the current velocity, so adjust the deceleration rate"
            let s0 = v.x.mul_add(v.x, v.y * v.y).sqrt();
            let rate = (s0 / (needed.powf(0.75) * 4.0)).powf(4.0);
            if rate.is_nan() {
                // "INFINITE BRAKES"
                self.duration_until_stop = 0.0;
                self.duration_until_stop_ignoring_rubberbanding = 0.0;
                self.calculate_cached_properties();
                return;
            }
            self.deceleration_rate = rate;
            let total = (needed / rate).powf(0.25);
            self.duration_until_stop = total;
            self.duration_until_stop_ignoring_rubberbanding = total;
            self.initial_velocity = Point::new(ddx / needed * s0, ddy / needed * s0);
        }
        self.calculate_cached_properties();
    }
}

use super::animator::{MomentumCalculator, MomentumInit, Vec2};

/// The Cates port behind the animator's calculator interface. The animator
/// passes `scrollableFrame` as the document frame and its origin as the
/// constrained clipping origin; the tolerance it passes is discarded by
/// AppKit's 6-argument initializer, as here.
pub struct CatesCalculator(CatesMomentum);

impl CatesCalculator {
    pub fn new(init: &MomentumInit) -> Self {
        let point = |v: Vec2| Point::new(v.x, v.y);
        let frame = init.document_frame;
        Self(CatesMomentum::new(
            point(init.initial_origin),
            point(init.initial_velocity),
            Rect::new(frame.origin.x, frame.origin.y, frame.size.x, frame.size.y),
            point(init.constrained_clipping_origin),
            Size::new(init.clipping_size.x, init.clipping_size.y),
        ))
    }
}

impl MomentumCalculator for CatesCalculator {
    fn position_after_duration(&self, t: f64) -> Vec2 {
        let (x, y) = self.0.position_after(t);
        Vec2::new(x, y)
    }
    fn velocity_after_duration(&self, t: f64) -> f64 {
        self.0.velocity_after(t)
    }
    fn duration_until_stop(&self) -> f64 {
        self.0.duration_until_stop()
    }
    fn destination_origin(&self) -> Vec2 {
        let (x, y) = self.0.destination();
        Vec2::new(x, y)
    }
    fn initial_velocity(&self) -> Vec2 {
        let v = self.0.initial_velocity();
        Vec2::new(v.x, v.y)
    }
    fn is_rubber_banding_after_duration(&self, t: f64) -> bool {
        self.0.is_rubber_banding_after(t)
    }
}

