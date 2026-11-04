//! The per-surface engine: AppKit's scroll animator plus the rules that tie it
//! to a GPUI scroll container (geometry, commands, frame demand, the clock).
//! Pure `f64` code with no GPUI types, so its tests run anywhere; the GPUI
//! adapter is `scroll_physics.rs`.
use super::animator::{
    AxisModePolicy, Geometry, MomentumCalculator, Phase, Rect, ScrollAnimator, State, Vec2,
    WheelEvent,
};
use super::momentum::CatesCalculator;

/// One configuration of a scroll container, in AppKit's flipped clip
/// coordinates (points).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceConfig {
    pub viewport: Vec2,
    /// Laid-out content extent (may be smaller than the viewport).
    pub content: Vec2,
    /// Axes that scroll. A non-scrolling axis never moves, even when content
    /// overflows it (`overflow_y_scroll` with a wide child).
    pub axes: [bool; 2],
    pub predominant_axis: bool,
    /// The bounded origin GPUI holds.
    pub origin: Vec2,
    pub command_revision: usize,
    /// `convertSizeFromBacking:(1,1)`: 0.5 on a 2x display.
    pub points_per_pixel: f64,
    /// The display's vsync spacing in seconds; 0 when unknown.
    pub frame_interval: f64,
}

/// A raw scroll event, as `-[NSScrollView scrollWheel:]` receives it.
#[derive(Clone, Copy, Debug)]
pub struct Input {
    /// `scrollingDeltaX/Y`: points when precise, lines otherwise.
    pub delta: Vec2,
    pub precise: bool,
    pub direct: Phase,
    pub momentum: Phase,
    /// Event timestamp, engine seconds.
    pub timestamp: f64,
}

/// AppKit drops its scroll display link to this rate for the slow tail of a
/// coast (`ScrollAnimator::preferred_frame_rate`); the engine then advances
/// only on every other 120 Hz vsync. The margin absorbs vsync jitter.
const REDUCED_RATE_MARGIN: f64 = 0.002;

fn geometry(config: &SurfaceConfig) -> Geometry {
    // A non-scrolling axis gets content == viewport, so the animator has no
    // range on it; the removed NSScrollView driver sized its document the same
    // way (`contentSize = horizontal ? contentWidth : width`).
    let extent = |axis: usize, viewport: f64, content: f64| {
        if config.axes[axis] {
            content.max(viewport)
        } else {
            viewport
        }
    };
    Geometry {
        scrollable_frame: Rect {
            origin: Vec2::ZERO,
            size: Vec2::new(
                extent(0, config.viewport.x, config.content.x),
                extent(1, config.viewport.y, config.content.y),
            ),
        },
        clip_size: config.viewport,
        points_per_pixel: config.points_per_pixel,
        flipped: true,
        allows_horizontal_stretching: config.axes[0],
        allows_vertical_stretching: config.axes[1],
        line_scroll: Vec2::new(10., 10.),
        axis_mode: if config.predominant_axis {
            AxisModePolicy::Predominant
        } else {
            AxisModePolicy::Free
        },
        dynamic_dampening_threshold: 0.,
    }
}

fn same_geometry(a: &Geometry, b: &Geometry) -> bool {
    a.scrollable_frame == b.scrollable_frame
        && a.clip_size == b.clip_size
        && a.points_per_pixel == b.points_per_pixel
        && a.allows_horizontal_stretching == b.allows_horizontal_stretching
        && a.allows_vertical_stretching == b.allows_vertical_stretching
        && a.axis_mode == b.axis_mode
}

fn new_animator(geometry: Geometry, origin: Vec2) -> ScrollAnimator {
    ScrollAnimator::new(
        geometry,
        origin,
        Box::new(|init| Box::new(CatesCalculator::new(init)) as Box<dyn MomentumCalculator>),
    )
}

/// `GPUI_SCROLL_PHYSICS_TRACE=1` prints every event and tick to stderr
/// (`SCROLL_ENGINE ...` lines) for the parity harness's diagnostics.
fn tracing() -> bool {
    static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TRACE.get_or_init(|| std::env::var_os("GPUI_SCROLL_PHYSICS_TRACE").is_some())
}

/// Deliberate breakage for the parity gate's rejection control: past an edge
/// each frame's move is multiplied by 4, a far softer rubber band than
/// AppKit's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mutation {
    None,
    SoftRubberBand,
}

pub struct Engine {
    geometry: Geometry,
    command_revision: usize,
    animator: ScrollAnimator,
    /// An event arrived since the last frame: its delta or its effect on the
    /// velocity is consumed at the next frame.
    event_pending: bool,
    /// Last time the animator advanced (for the 60 Hz tail).
    last_advance: f64,
    /// Vsync spacing (0 = unknown) and the grid time of the last tick.
    /// AppKit's animator ticks on every vsync from its own display link,
    /// whether or not the app draws; how events batch into ticks changes the
    /// result (the first tick past the bottom edge moves 1:1, the velocity is
    /// per tick). GPUI draws when it can, so the engine replays the vsync ticks
    /// it missed, in time order with the events, before each event and frame.
    period: f64,
    last_tick: f64,
    /// The origin GPUI was last given ([`Self::report`]). GPUI writes it back
    /// as the container's offset, so a configured offset that differs from it
    /// was moved by someone else. The animator's live origin is the wrong
    /// reference: the vsync ticks replayed before an event move it during
    /// event handling, between GPUI frames, which would read as a foreign move
    /// and cancel the gesture (it cancelled every coast when it was used).
    reported: Vec2,
    mutation: Mutation,
}

impl Engine {
    pub fn new(config: SurfaceConfig, mutation: Mutation) -> Self {
        let geometry = geometry(&config);
        Self {
            geometry,
            command_revision: config.command_revision,
            animator: new_animator(geometry, config.origin),
            event_pending: false,
            last_advance: f64::NEG_INFINITY,
            period: config.frame_interval,
            last_tick: f64::NEG_INFINITY,
            reported: config.origin,
            mutation,
        }
    }

    fn maximum(&self) -> Vec2 {
        let frame = self.geometry.scrollable_frame.size;
        let clip = self.geometry.clip_size;
        Vec2::new((frame.x - clip.x).max(0.), (frame.y - clip.y).max(0.))
    }

    fn bounded(&self, origin: Vec2) -> Vec2 {
        let maximum = self.maximum();
        Vec2::new(origin.x.clamp(0., maximum.x), origin.y.clamp(0., maximum.y))
    }

    /// Applies layout and commands.
    ///
    /// - A new command revision, or a bounded origin that no longer matches
    ///   GPUI's (it moved the offset directly), puts the engine at rest there:
    ///   an explicit jump beats a coasting gesture.
    /// - An origin outside the range while nothing can bring it back (idle,
    ///   fingers down but not moving, or wheel-notch tracking, which never
    ///   stretches) means the content shrank under it: it is pulled back into
    ///   range at once, as `NSClipView` constrains its bounds. A drag, coast
    ///   or snap-back in flight handles the change itself (the animator
    ///   rebuilds its momentum from the new offset).
    pub fn configure(&mut self, config: SurfaceConfig) {
        let geometry = geometry(&config);
        if !same_geometry(&geometry, &self.geometry) {
            self.geometry = geometry;
            self.animator.set_geometry(geometry);
        }
        self.period = config.frame_interval;
        if matches!(
            self.animator.state(),
            State::Idle | State::MayBegin | State::PhysicalDiscrete
        ) {
            let origin = self.animator.origin();
            let bounded = self.bounded(origin);
            if bounded != origin {
                self.animator.set_origin(bounded);
            }
        }
        let requested = self.bounded(config.origin);
        let bounded = self.bounded(self.reported);
        let drift =
            (bounded.x - requested.x).abs() > 0.01 || (bounded.y - requested.y).abs() > 0.01;
        if config.command_revision != self.command_revision || drift {
            self.command_revision = config.command_revision;
            self.animator = new_animator(self.geometry, requested);
            self.event_pending = false;
            self.reported = requested;
        }
    }

    /// Feeds one raw event; `now` is when it is handled (AppKit stamps
    /// tracking and momentum start with it). Its effect lands at the next
    /// [`Self::advance`].
    pub fn wheel(&mut self, input: Input, now: f64) {
        // AppKit's display link delivers the tick for vsync T one refresh
        // early (at T - period, `targetTimestamp` = T), so by `now` every tick
        // for a vsync up to `now + period` has consumed what was queued.
        self.catch_up(now + self.period);
        let event = WheelEvent {
            delta: input.delta,
            precise: input.precise,
            phase: input.direct,
            momentum_phase: input.momentum,
            timestamp: input.timestamp,
        };
        self.animator.on_wheel(&event, now);
        self.event_pending = true;
        if tracing() {
            let velocity = self.animator.last_filtered_velocity();
            eprintln!(
                "SCROLL_ENGINE event now={now:.6} ts={:.6} dy={} precise={} direct={:?} momentum={:?} state={:?} origin={:.3} velocity={:.3}",
                input.timestamp,
                input.delta.y,
                input.precise,
                input.direct,
                input.momentum,
                self.animator.state(),
                self.animator.origin().y,
                velocity.y
            );
        }
    }

    /// One display frame at vsync time `t`. Monotonic: a `t` at or before the
    /// last tick (a draw outside the display link) changes nothing. Returns
    /// the clip origin.
    pub fn advance(&mut self, t: f64) -> Vec2 {
        // The real frame's timestamp replaces the grid tick nearest to it, so
        // the replayed grid re-anchors on the display link every frame
        // instead of drifting from it.
        self.catch_up(t - self.period / 2.);
        if t > self.last_tick + 1e-6 {
            self.tick(t);
        }
        self.animator.origin()
    }

    /// AppKit's display link rate for this surface: `Some(60)` in the slow
    /// tail of a coast (the window caps its display link to it), `None` at
    /// the display's rate.
    pub fn preferred_frame_rate(&self) -> Option<f64> {
        self.animator.preferred_frame_rate()
    }

    /// Runs the vsync ticks AppKit's display link would have delivered after
    /// the last one and no later than `until`. Needs a known grid; a gap of
    /// more than two seconds (a hidden window) is crossed in one tick.
    fn catch_up(&mut self, until: f64) {
        const MAX_GAP: f64 = 2.;
        if self.period <= 0. || !self.last_tick.is_finite() || until - self.last_tick > MAX_GAP {
            return;
        }
        while self.last_tick + self.period <= until + 1e-6 {
            // In the 60 Hz tail the display link, not this grid, decides which
            // vsyncs tick (Core Animation aligns the reduced rate to the
            // display's own 60 Hz cadence, as it does for AppKit's link), so
            // only real frames advance. Stop without moving `last_tick`: an
            // event arriving before a late frame must not use up that frame's
            // vsync (`advance` only ticks after `last_tick`).
            if self.animator.preferred_frame_rate().is_some() {
                return;
            }
            self.tick(self.last_tick + self.period);
        }
    }

    /// One tick of AppKit's scroll display link at vsync time `t`.
    fn tick(&mut self, t: f64) {
        self.last_tick = t;
        self.animator.on_timer(t);
        if let Some(rate) = self.animator.preferred_frame_rate()
            && t - self.last_advance < 1. / rate - REDUCED_RATE_MARGIN
        {
            return;
        }
        self.last_advance = t;
        let before = self.animator.origin();
        let after = self.animator.on_frame(t);
        self.event_pending = false;
        if tracing() && after != before {
            eprintln!(
                "SCROLL_ENGINE tick t={t:.6} origin={:.3} state={:?} velocity={:.3}",
                after.y,
                self.animator.state(),
                self.animator.last_filtered_velocity().y
            );
        }
        if self.mutation == Mutation::SoftRubberBand && after != before {
            let maximum = self.maximum();
            if after.y < 0. || after.y > maximum.y {
                let softened = before.y + (after.y - before.y) * 4.;
                self.animator.set_origin(Vec2::new(after.x, softened));
            }
        }
    }

    /// Motion is pending: a queued event, a coast or a snap-back. At rest, and
    /// while fingers rest on the trackpad without moving, this is false, so a
    /// still window requests no frames. (AppKit's own display link keeps
    /// ticking while tracking, but a tick without a new event or a coast
    /// changes nothing.)
    pub fn needs_frame(&self) -> bool {
        self.event_pending || self.animator.state() == State::Animating
    }

    pub fn origin(&self) -> Vec2 {
        self.animator.origin()
    }

    /// The origin to show now; recorded as what GPUI was given.
    pub fn report(&mut self) -> Vec2 {
        self.reported = self.animator.origin();
        self.reported
    }
}
