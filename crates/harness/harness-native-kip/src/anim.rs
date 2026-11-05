//! The animation: a state machine of moods with crossfades, locomotion with root motion, and procedural layers over
//! them (breath, blinks, look-at, springs). Every number comes from the scene (scene/45-mascot.flecs, [`Tuning`]).
//!
//! - **Moods** are procedural clips of the rig ([`clip`]): idle, thinking, working, happy, confused, sleeping, wave. The
//!   host sets a base mood ([`Mascot::set_mood`]) and plays one-shots over it ([`Mascot::play`]: happy, wave), which end
//!   by themselves. Idle with nothing happening for `sleep_after` seconds dozes off; any [`Mascot::poke`] wakes it.
//!   A change of mood crossfades from the pose it had when the change came (so a change in mid-fade never pops) over
//!   the new mood's `blend`.
//! - **Locomotion** walks or hops along legs given by the host ([`Mascot::walk_to`], [`Mascot::hop_to`],
//!   [`Mascot::follow`]). A walk is root motion: a foot in stance moves back exactly as fast as the root moves on, so
//!   feet never slide; the gait's weight is the speed's share of the most, a one-dimensional blend from standing to the
//!   walk. A hop anticipates (a crouch), stretches up, arcs, squashes on landing and overshoots back (follow-through).
//! - **Layers**: breathing, blinks on a seeded schedule, look-at toward a point (the pointer, the active field) on
//!   critically damped springs, and underdamped springs that let the antennae and arms lag the body.
//! - **Time** is the host's frame clock, integrated in fixed steps of [`STEP`], so a pose at a time depends only on what
//!   was asked and when, never on the frame rate.
//! - **Power**: idle breathes only for `awake` seconds after something happened, then rests still with a breath or a
//!   glance now and then (`fidget_every`); [`Mascot::animating`] says when frames are wanted and [`Mascot::rest_for`]
//!   how long until the next blink or fidget.
//! - **Reduce Motion** ([`Mascot::reduce_motion`]): the rest pose, still, with blinks only; a walk or hop arrives at
//!   once.
use super::rig::{Pose, Role};
use std::collections::VecDeque;
use std::f32::consts::{PI, TAU};

/// The fixed step the animation is integrated in, seconds.
pub const STEP: f64 = 1.0 / 240.0;

/// What the character is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mood {
    Idle,
    Thinking,
    Working,
    Happy,
    Confused,
    Sleeping,
    Wave,
}

impl Mood {
    pub const ALL: [Self; 7] = [Self::Idle, Self::Thinking, Self::Working, Self::Happy, Self::Confused, Self::Sleeping, Self::Wave];

    /// Its name in the scene (`mascot.<name>`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Thinking => "thinking",
            Self::Working => "working",
            Self::Happy => "happy",
            Self::Confused => "confused",
            Self::Sleeping => "sleeping",
            Self::Wave => "wave",
        }
    }

    /// It plays once over the base mood and ends by itself.
    #[must_use]
    pub fn one_shot(self) -> bool {
        matches!(self, Self::Happy | Self::Wave)
    }
}

/// A mood's clip as the scene tunes it: the crossfade into it (seconds), how fast and how much it moves, and how long
/// a one-shot lasts (seconds).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Clip {
    pub blend: f32,
    pub speed: f32,
    pub amount: f32,
    pub length: f32,
}

/// The motion's numbers, from the scene's `MascotMotion` (angles in radians here).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Motion {
    pub breath_hz: f32,
    pub breath: f32,
    pub blink_min: f32,
    pub blink_max: f32,
    pub blink_len: f32,
    pub look_omega: f32,
    pub look_yaw: f32,
    pub look_pitch: f32,
    pub turn_omega: f32,
    pub antenna_hz: f32,
    pub antenna_damping: f32,
    pub antenna_gain: f32,
    pub arm_hz: f32,
    pub arm_damping: f32,
    pub arm_gain: f32,
    pub stride: f32,
    pub cadence: f32,
    pub accel: f32,
    pub hop_height: f32,
    pub hop_time: f32,
    pub anticipation: f32,
    pub sleep_after: f32,
    pub awake: f32,
    pub fidget_every: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tuning {
    pub motion: Motion,
    /// One per [`Mood::ALL`], in its order.
    pub clips: [Clip; 7],
}

impl Tuning {
    #[must_use]
    pub fn clip(&self, m: Mood) -> Clip {
        self.clips[m as usize]
    }
}

/// How a leg of a route is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gait {
    Walk,
    Hop,
}

/// One leg of a route: to a point (host points, the feet), walking or hopping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Leg {
    pub to: [f32; 2],
    pub gait: Gait,
}

/// An underdamped spring of one angle.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Osc {
    x: f32,
    v: f32,
}

impl Osc {
    fn step(&mut self, target: f32, input: f32, hz: f32, damping: f32, h: f32) {
        let w = TAU * hz;
        self.v += (w * w * (target - self.x) - 2.0 * damping * w * self.v + input) * h;
        self.x = (self.x + self.v * h).clamp(-1.2, 1.2);
    }

    fn calm(&self, target: f32) -> bool {
        (self.x - target).abs() < 1e-3 && self.v.abs() < 1e-2
    }
}

/// A hop under way: its start (the crouch), its legs' ends, its arc's height in cells.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Hop {
    at: f64,
    from: [f32; 2],
    to: [f32; 2],
    air: f32,
    height: f32,
}

fn smooth(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// splitmix64 of `k` under `seed`, as 0 to 1.
fn chance(seed: u64, k: u64) -> f32 {
    let mut z = seed ^ k.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
}

/// The pose of mood `m` at `tau` seconds into it (`breath` the breathing's swing, -1 to 1).
#[must_use]
pub fn clip(m: Mood, tau: f32, breath: f32, k: &Tuning) -> Pose {
    let c = k.clip(m);
    let (a, t) = (c.amount, tau * c.speed);
    let mut p = Pose::default();
    let b = breath * k.motion.breath;
    {
        let body = p.joint_mut(Role::Body);
        body.s = [1.0 - 0.5 * b, 1.0 + b, 1.0 - 0.5 * b];
    }
    p.joint_mut(Role::ArmL).r[2] = -0.6 * b;
    p.joint_mut(Role::ArmR).r[2] = 0.6 * b;
    match m {
        Mood::Idle => {}
        Mood::Thinking => {
            let body = p.joint_mut(Role::Body);
            body.r[2] = 0.07 * a * (t * 0.9).sin();
            body.r[0] = -0.05 * a;
            // A hand to its chin, the near one.
            let arm = p.joint_mut(Role::ArmR);
            arm.r[0] = -1.15 * a;
            arm.r[2] = 0.45 * a + 0.06 * a * (t * 5.0).sin();
            p.glow = 0.55 + 0.45 * (t * 3.2).sin();
        }
        Mood::Working => {
            let tap = |phase: f32| (t * 13.0 + phase).sin().max(0.0);
            let body = p.joint_mut(Role::Body);
            body.r[0] = 0.14 * a;
            body.t[1] = -0.12 * a * (t * 13.0).sin().abs();
            p.joint_mut(Role::ArmL).r[0] = -0.95 * a + 0.35 * a * tap(0.0);
            p.joint_mut(Role::ArmR).r[0] = -0.95 * a + 0.35 * a * tap(PI);
            p.glow = 0.3;
        }
        Mood::Happy => {
            // A crouch, a little jump in place, a squash on landing that overshoots back.
            let (crouch, air) = (0.14, 0.38);
            let height = 2.2 * a;
            if t < crouch {
                p.squash = 1.0 - 0.18 * smooth(t / crouch);
            } else if t < crouch + air {
                let s = (t - crouch) / air;
                p.lift = 4.0 * height * s * (1.0 - s);
                p.squash = 1.0 + 0.14 * (1.0 - 2.0 * s).powi(2) * if s < 0.5 { 1.0 } else { 0.5 };
            } else {
                let u = t - crouch - air;
                p.squash = 1.0 - 0.2 * (-9.0 * u).exp() * (15.0 * u).cos();
            }
            let up = smooth(t / 0.2) * (1.0 - smooth((t - (c.length - 0.35)) / 0.35));
            p.joint_mut(Role::ArmL).r[2] = -2.3 * up * a;
            p.joint_mut(Role::ArmR).r[2] = 2.3 * up * a;
            p.joint_mut(Role::Mouth).s = [1.0 + 0.6 * up, 1.0 + 0.4 * up, 1.0];
            for e in [Role::EyeL, Role::EyeR] {
                p.joint_mut(e).s[1] = 1.0 - 0.4 * up;
            }
            p.joint_mut(Role::Body).r[2] = 0.08 * up * (t * 9.0).sin();
        }
        Mood::Confused => {
            p.yaw = 0.28 * a * (-3.0 * t).exp() * (t * 17.0).sin();
            let body = p.joint_mut(Role::Body);
            body.r[2] = 0.16 * a + 0.03 * (t * 1.3).sin();
            body.r[0] = -0.05 * a;
            p.joint_mut(Role::AntennaL).r[2] = 0.5 * a;
            p.joint_mut(Role::AntennaR).r[2] = -0.15 * a;
            p.joint_mut(Role::EyeL).s[1] = 1.0 - 0.3 * a;
            p.joint_mut(Role::EyeR).s[1] = 1.0 + 0.12 * a;
            let mouth = p.joint_mut(Role::Mouth);
            mouth.t[0] = 0.5 * a;
            mouth.s[0] = 1.0 - 0.3 * a;
        }
        Mood::Sleeping => {
            let body = p.joint_mut(Role::Body);
            body.s = [1.0 + 0.03 * a - 0.5 * b, 0.93 + b, 1.0 + 0.03 * a - 0.5 * b];
            body.r[0] = 0.12 * a;
            for e in [Role::EyeL, Role::EyeR] {
                p.joint_mut(e).s[1] = 0.12;
            }
            p.joint_mut(Role::AntennaL).r = [0.3 * a, 0.0, 0.55 * a];
            p.joint_mut(Role::AntennaR).r = [0.3 * a, 0.0, -0.55 * a];
            p.joint_mut(Role::ArmL).r[2] = -0.12 * a - 0.6 * b;
            p.joint_mut(Role::ArmR).r[2] = 0.12 * a + 0.6 * b;
            p.joint_mut(Role::Mouth).s = [0.6, 1.0, 1.0];
        }
        Mood::Wave => {
            let up = smooth(t / 0.28) * (1.0 - smooth((t - (c.length - 0.3)) / 0.3));
            // The near arm (the view turns its right side to you), out from the shoulder and a little forward so it
            // clears the head.
            let arm = p.joint_mut(Role::ArmR);
            arm.r[2] = up * a * (2.05 + 0.42 * (t * 12.0).sin());
            arm.t = [0.6 * up * a, 0.4 * up * a, 0.8 * up * a];
            p.joint_mut(Role::Body).r[2] = -0.07 * up * a;
            p.joint_mut(Role::Mouth).s = [1.0 + 0.45 * up, 1.0 + 0.2 * up, 1.0];
        }
    }
    p
}

/// The walk's pose at `phase` (cycles): `stride` cells each way, feet lifted `lift` cells. Each foot is in stance for
/// half the cycle, moving back from +stride to -stride at an even pace, the pace the root moves on, so it is planted.
#[must_use]
pub fn walk(phase: f32, stride: f32, lift: f32) -> Pose {
    let mut p = Pose::default();
    let foot = |f: f32| -> ([f32; 3], f32) {
        let f = f.rem_euclid(1.0);
        if f < 0.5 {
            ([0.0, 0.0, stride * (1.0 - 4.0 * f)], 0.0)
        } else {
            let u = (f - 0.5) * 2.0;
            ([0.0, lift * (PI * u).sin(), -stride + 2.0 * stride * smooth(u)], -0.35 * (PI * u).sin())
        }
    };
    let (l, lr) = foot(phase);
    let (r, rr) = foot(phase + 0.5);
    p.joint_mut(Role::FootL).t = l;
    p.joint_mut(Role::FootL).r[0] = lr;
    p.joint_mut(Role::FootR).t = r;
    p.joint_mut(Role::FootR).r[0] = rr;
    let body = p.joint_mut(Role::Body);
    body.t[1] = 0.35 * (0.5 - 0.5 * (TAU * 2.0 * phase).cos());
    body.r[2] = 0.06 * (TAU * phase).sin();
    body.r[0] = 0.06;
    p.joint_mut(Role::ArmL).r[0] = 0.55 * (TAU * phase).sin();
    p.joint_mut(Role::ArmR).r[0] = -0.55 * (TAU * phase).sin();
    p
}

/// Blend the walk over `p` by `w`: its feet, its arms' swing and its body's bob and sway, the rest of the mood kept.
fn over(p: &mut Pose, walk: &Pose, w: f32) {
    for r in [Role::FootL, Role::FootR] {
        *p.joint_mut(r) = p.joint(r).lerp(walk.joint(r), w);
    }
    for r in [Role::ArmL, Role::ArmR] {
        p.joint_mut(r).r[0] += walk.joint(r).r[0] * w;
    }
    let (wb, body) = (*walk.joint(Role::Body), p.joint_mut(Role::Body));
    body.t[1] += wb.t[1] * w;
    body.r[0] += wb.r[0] * w;
    body.r[2] += wb.r[2] * w;
}

/// One character's animation: its moods, its route, its layers and its clock.
#[derive(Clone, Debug)]
pub struct Mascot {
    pub tuning: Tuning,
    /// The system's Reduce Motion: a still pose with blinks only.
    pub reduce_motion: bool,
    /// Its height at rest, cells (its rig's).
    cells: f32,
    /// Points a cell is across, from the size it is drawn at.
    cell: f32,
    t: f64,
    acc: f64,
    seed: u64,
    base: Mood,
    asleep: bool,
    shot: Option<(Mood, f64)>,
    /// The mood shown and since when.
    shown: (Mood, f64),
    /// A crossfade into it: the pose it starts from, when, and how long it takes.
    fade: Option<(Pose, f64, f32)>,
    /// The mood's pose at the last step, before locomotion and the layers: where a crossfade starts.
    raw: Pose,
    out: Pose,
    poked: f64,
    breath_t0: f64,
    breath_until: f64,
    fidget: f64,
    fidget_k: u64,
    blink_at: f64,
    blink_double: bool,
    blink_next: f64,
    blink_k: u64,
    pos: [f32; 2],
    route: VecDeque<Leg>,
    hop: Option<Hop>,
    landed: Option<f64>,
    speed: f32,
    phase: f32,
    facing: f32,
    facing_v: f32,
    target: Option<[f32; 2]>,
    glance: Option<([f32; 2], f64)>,
    look: [f32; 2],
    look_v: [f32; 2],
    /// The antennae (left, right) about X and Z, and the arms' outward swing (left, right).
    antenna: [[Osc; 2]; 2],
    arm: [Osc; 2],
    /// The body's place (cells, y up) and speed at the last step, for its acceleration.
    anchor: Option<([f32; 2], [f32; 2])>,
}

impl Mascot {
    /// A character `cells` tall at rest, standing at `at` (host points, its feet), awake and idle. `seed` keeps its
    /// blinks and glances from falling in step with another's.
    #[must_use]
    pub fn new(tuning: Tuning, cells: f32, at: [f32; 2], seed: u64) -> Self {
        let mut m = Self {
            tuning,
            reduce_motion: false,
            cells: cells.max(1.0),
            cell: 1.0,
            t: 0.0,
            acc: 0.0,
            seed,
            base: Mood::Idle,
            asleep: false,
            shot: None,
            shown: (Mood::Idle, 0.0),
            fade: None,
            raw: Pose::default(),
            out: Pose::default(),
            poked: 0.0,
            breath_t0: 0.0,
            breath_until: 0.0,
            fidget: 0.0,
            fidget_k: 0,
            blink_at: -1e9,
            blink_double: false,
            blink_next: 0.0,
            blink_k: 0,
            pos: at,
            route: VecDeque::new(),
            hop: None,
            landed: None,
            speed: 0.0,
            phase: 0.0,
            facing: 0.0,
            facing_v: 0.0,
            target: None,
            glance: None,
            look: [0.0; 2],
            look_v: [0.0; 2],
            antenna: [[Osc::default(); 2]; 2],
            arm: [Osc::default(); 2],
            anchor: None,
        };
        m.blink_next = m.blink_gap();
        m.fidget = m.fidget_gap();
        m.raw = clip(Mood::Idle, 0.0, 0.0, &m.tuning);
        m.out = m.raw;
        m
    }

    /// The size it is drawn at, points tall: what a cell is in points, for its root motion.
    pub fn set_height(&mut self, points: f32) {
        self.cell = (points / self.cells).max(0.01);
    }

    /// Points a cell is across.
    #[must_use]
    pub fn cell(&self) -> f32 {
        self.cell
    }

    /// Its clock, seconds.
    #[must_use]
    pub fn time(&self) -> f64 {
        self.t
    }

    /// The pose to draw.
    #[must_use]
    pub fn pose(&self) -> Pose {
        self.out
    }

    /// Where its feet are, host points (not lifted by a hop: that is in [`Pose::lift`]).
    #[must_use]
    pub fn at(&self) -> [f32; 2] {
        self.pos
    }

    /// The mood it shows (fading into, if a crossfade is under way): a one-shot over the base, or the base, or asleep
    /// once idle long enough.
    #[must_use]
    pub fn mood(&self) -> Mood {
        self.shot.map_or(if self.asleep && self.base == Mood::Idle { Mood::Sleeping } else { self.base }, |s| s.0)
    }

    /// The base mood the host set.
    #[must_use]
    pub fn base(&self) -> Mood {
        self.base
    }

    /// Stand at `at` at once, its route dropped.
    pub fn place(&mut self, at: [f32; 2]) {
        self.pos = at;
        self.route.clear();
        self.hop = None;
        self.speed = 0.0;
        self.anchor = None;
    }

    /// Walk to `at` along a straight line.
    pub fn walk_to(&mut self, at: [f32; 2]) {
        self.follow([Leg { to: at, gait: Gait::Walk }]);
    }

    /// Hop to `at` in one arc.
    pub fn hop_to(&mut self, at: [f32; 2]) {
        self.follow([Leg { to: at, gait: Gait::Hop }]);
    }

    /// Go along `legs` in order, after what is left of its route.
    pub fn follow(&mut self, legs: impl IntoIterator<Item = Leg>) {
        self.route.extend(legs);
        self.poke();
    }

    /// Stop where it is (a hop under way lands first; a walk slows to a stop over what its pace needs).
    pub fn stop(&mut self) {
        self.route.clear();
    }

    /// Where its route ends (where it stands, with none).
    #[must_use]
    pub fn goal(&self) -> [f32; 2] {
        self.route.back().map_or(self.pos, |l| l.to)
    }

    /// Nothing left to walk or hop, and standing still.
    #[must_use]
    pub fn arrived(&self) -> bool {
        self.route.is_empty() && self.hop.is_none() && self.speed < 1e-3
    }

    /// Look toward `p` (host points), or about by itself with None.
    pub fn look_at(&mut self, p: Option<[f32; 2]>) {
        if p != self.target {
            self.target = p;
            if p.is_some() {
                self.poke();
            }
        }
    }

    /// The base mood (a one-shot plays instead); a change crossfades and wakes it.
    pub fn set_mood(&mut self, m: Mood) {
        if m.one_shot() {
            self.play(m);
        } else if m != self.base {
            self.base = m;
            self.poke();
        }
    }

    /// Play a one-shot (happy, wave) over the base mood; it ends by itself.
    pub fn play(&mut self, m: Mood) {
        self.shot = Some((m, self.t));
        self.poke();
    }

    /// Something happened near it: awake, out of a doze, and breathing again.
    pub fn poke(&mut self) {
        if self.t >= self.breath_until {
            self.breath_t0 = self.t;
        }
        self.poked = self.t;
        self.asleep = false;
    }

    /// Its clock jumps `secs` on while it was still and drew nothing; a blink or fidget due in that time comes next.
    pub fn resume(&mut self, secs: f64) {
        if secs > 0.0 && !self.animating() {
            self.t += secs;
            self.blink_next = self.blink_next.max(self.t);
            self.fidget = self.fidget.max(self.t);
        }
    }

    /// Frames are wanted: something moves now (not only a blink or fidget to come, which [`Self::rest_for`] gives).
    #[must_use]
    pub fn animating(&self) -> bool {
        self.moves().is_some()
    }

    /// What moves now, the first of them: a blink, its breath, a walk or hop, a crossfade, a glance, its springs, its look
    /// or its turn. None while it rests.
    #[must_use]
    pub fn moves(&self) -> Option<&'static str> {
        if self.blink_closure() > 0.0 || self.t < self.blink_at + self.blink_span() {
            return Some("blink");
        }
        if self.reduce_motion {
            return None;
        }
        let springs = self.antenna.iter().flatten().chain(&self.arm).any(|o| !o.calm(0.0));
        let look = self.look_v.iter().any(|v| v.abs() > 1e-3) || (self.look[0] - self.look_goal()[0]).abs() > 1e-3 || (self.look[1] - self.look_goal()[1]).abs() > 1e-3;
        [
            ("breath", self.breathing()),
            ("travel", self.moving()),
            ("crossfade", self.fade.is_some()),
            ("glance", self.glance.is_some()),
            ("springs", springs),
            ("look", look),
            ("turn", self.facing_v.abs() > 1e-3),
        ]
        .into_iter()
        .find_map(|(name, on)| on.then_some(name))
    }

    /// It moves enough to want every frame the display gives: travelling, landing, a one-shot or a crossfade. Breath,
    /// blinks, glances and springs are slow enough for the caret's blink pace.
    #[must_use]
    pub fn lively(&self) -> bool {
        !self.reduce_motion && (self.moving() || self.shot.is_some() || self.fade.is_some())
    }

    /// Seconds until a blink or fidget is due, while not [`Self::animating`].
    #[must_use]
    pub fn rest_for(&self) -> f64 {
        let blink = self.blink_next - self.t;
        if self.reduce_motion { blink.max(0.0) } else { blink.min(self.fidget - self.t).max(0.0) }
    }

    /// Run the clock `dt` seconds on, in fixed steps of [`STEP`].
    pub fn step(&mut self, dt: f32) {
        self.acc += f64::from(dt.max(0.0)).min(1.0);
        while self.acc >= STEP - 1e-9 {
            self.acc -= STEP;
            self.t += STEP;
            self.substep(STEP as f32);
        }
    }

    fn blink_gap(&self) -> f64 {
        let m = &self.tuning.motion;
        self.t + f64::from(m.blink_min + (m.blink_max - m.blink_min).max(0.0) * chance(self.seed, self.blink_k * 2))
    }

    fn fidget_gap(&self) -> f64 {
        self.t + f64::from(self.tuning.motion.fidget_every.max(0.5) * (0.7 + 0.6 * chance(self.seed ^ 0xf1d6, self.fidget_k)))
    }

    fn blink_span(&self) -> f64 {
        let len = f64::from(self.tuning.motion.blink_len.max(0.02));
        if self.blink_double { len * 2.6 } else { len }
    }

    /// The eyes' closure now, 0 open to 1 shut.
    fn blink_closure(&self) -> f32 {
        let len = f64::from(self.tuning.motion.blink_len.max(0.02));
        let one = |start: f64| {
            let u = ((self.t - start) / len) as f32;
            if !(0.0..1.0).contains(&u) {
                0.0
            } else if u < 0.35 {
                smooth(u / 0.35)
            } else {
                1.0 - smooth((u - 0.35) / 0.65)
            }
        };
        let second = if self.blink_double { one(self.blink_at + len * 1.6) } else { 0.0 };
        one(self.blink_at).max(second)
    }

    /// The breath's period, seconds: slower asleep.
    fn breath_period(&self) -> f64 {
        let p = 1.0 / f64::from(self.tuning.motion.breath_hz.max(0.05));
        if self.shown.0 == Mood::Sleeping { p * 1.6 } else { p }
    }

    /// Breathes on and on now (rather than now and then): busy, just moved or poked, or travelling.
    fn awake(&self) -> bool {
        let since = self.t - self.shown.1;
        match self.shown.0 {
            Mood::Thinking | Mood::Working | Mood::Happy | Mood::Wave => true,
            Mood::Confused => since < f64::from(self.tuning.clip(Mood::Confused).length),
            Mood::Idle | Mood::Sleeping => self.t - self.poked < f64::from(self.tuning.motion.awake) || !self.arrived(),
        }
    }

    fn breathing(&self) -> bool {
        self.awake() || self.t < self.breath_until
    }

    fn moving(&self) -> bool {
        !self.arrived() || self.landed.is_some_and(|l| self.t - l < 0.6)
    }

    /// Where the look is headed, as its share of the most it turns (yaw, pitch; -1 to 1).
    fn look_goal(&self) -> [f32; 2] {
        if self.reduce_motion {
            return [0.0; 2];
        }
        match (self.shown.0, self.target) {
            (Mood::Sleeping, _) => [0.0, 0.6],
            (_, Some(p)) => {
                let eye = [self.pos[0], self.pos[1] - 0.7 * self.cells * self.cell];
                let d = 3.0 * self.cells * self.cell;
                let m = &self.tuning.motion;
                let yaw = (p[0] - eye[0]).atan2(d) / m.look_yaw.max(1e-3);
                let pitch = (p[1] - eye[1]).atan2(d) / m.look_pitch.max(1e-3);
                [yaw.clamp(-1.0, 1.0), pitch.clamp(-1.0, 1.0)]
            }
            (Mood::Thinking, None) => [-0.4, -0.7],
            (Mood::Working, None) => [0.0, 0.8],
            _ => self.glance.map_or([0.0; 2], |(g, _)| g),
        }
    }

    fn substep(&mut self, h: f32) {
        let m = self.tuning.motion;
        // The state machine: a doze, a one-shot's end, and a crossfade whenever the mood shown changes.
        if self.base == Mood::Idle && !self.asleep && self.arrived() && self.t - self.poked >= f64::from(m.sleep_after) {
            self.asleep = true;
        }
        if let Some((s, at)) = self.shot
            && self.t - at >= f64::from(self.tuning.clip(s).length)
        {
            self.shot = None;
        }
        let want = self.mood();
        let replayed = self.shot.is_some_and(|s| s.1 != self.shown.1);
        if want != self.shown.0 || replayed {
            self.fade = Some((self.raw, self.t, self.tuning.clip(want).blend.max(0.0)));
            self.shown = (want, self.shot.map_or(self.t, |s| s.1));
        }

        // Breath: on and on while awake, finishing its last breath after; at rest a breath (and maybe a glance) now
        // and then.
        let period = self.breath_period();
        if self.awake() {
            let cycles = ((self.t - self.breath_t0) / period).floor() + 1.0;
            self.breath_until = self.breath_t0 + cycles * period;
        }
        if self.t >= self.fidget {
            self.fidget_k += 1;
            if !self.awake() && self.t >= self.breath_until {
                self.breath_t0 = self.t;
                self.breath_until = self.t + period;
            }
            if self.shown.0 == Mood::Idle && self.target.is_none() && chance(self.seed ^ 0x61a7, self.fidget_k) < 0.65 {
                let k = self.fidget_k;
                let g = [chance(self.seed, k * 7 + 3) * 1.6 - 0.8, chance(self.seed, k * 7 + 5) * 0.8 - 0.5];
                self.glance = Some((g, self.t));
            }
            self.fidget = self.fidget_gap();
        }
        if self.glance.is_some_and(|(_, at)| self.t - at > 1.4) {
            self.glance = None;
        }
        let breath = if self.t < self.breath_until { (TAU * ((self.t - self.breath_t0) / period) as f32).sin() } else { 0.0 };

        if self.t >= self.blink_next {
            self.blink_at = self.blink_next;
            self.blink_k += 1;
            self.blink_double = chance(self.seed, self.blink_k * 2 + 1) < 0.2;
            self.blink_next = self.blink_gap() + self.blink_span();
        }

        // The mood's pose, crossfaded from where the last change left it.
        let tau = (self.t - self.shown.1) as f32;
        let mut raw = clip(self.shown.0, tau, breath, &self.tuning);
        if let Some((from, at, dur)) = self.fade {
            let w = if dur <= 0.0 { 1.0 } else { smooth(((self.t - at) as f32) / dur) };
            raw = from.lerp(&raw, w);
            if w >= 1.0 {
                self.fade = None;
            }
        }
        self.raw = raw;

        if self.reduce_motion {
            self.travel_at_once();
            let mut p = Pose::default();
            self.blink_into(&mut p);
            self.out = p;
            self.look = [0.0; 2];
            self.look_v = [0.0; 2];
            self.facing = 0.0;
            self.facing_v = 0.0;
            self.antenna = [[Osc::default(); 2]; 2];
            self.arm = [Osc::default(); 2];
            self.anchor = None;
            return;
        }

        let mut p = raw;
        let face_goal = self.travel(h, &mut p);
        let w = m.turn_omega;
        self.facing_v += (w * w * (face_goal - self.facing) - 2.0 * w * self.facing_v) * h;
        self.facing += self.facing_v * h;

        // Look: the body turns part of the way, the eyes the rest.
        let goal = self.look_goal();
        let w = m.look_omega;
        for i in 0..2 {
            self.look_v[i] += (w * w * (goal[i] - self.look[i]) - 2.0 * w * self.look_v[i]) * h;
            self.look[i] += self.look_v[i] * h;
        }
        p.yaw += self.facing + 0.6 * self.look[0] * m.look_yaw;
        p.joint_mut(Role::Body).r[0] += 0.5 * self.look[1] * m.look_pitch;
        for e in [Role::EyeL, Role::EyeR] {
            let j = p.joint_mut(e);
            j.t[0] += 0.4 * self.look[0];
            j.t[1] -= 0.35 * self.look[1];
        }

        // Springs: the antennae and arms lag what the body does, in its own frame.
        let body = p.joint(Role::Body).t;
        let anchor = [self.pos[0] / self.cell, -self.pos[1] / self.cell + p.lift + body[1]];
        let accel = match self.anchor {
            Some((last, v0)) => {
                let v = [(anchor[0] - last[0]) / h, (anchor[1] - last[1]) / h];
                self.anchor = Some((anchor, v));
                [((v[0] - v0[0]) / h).clamp(-400.0, 400.0), ((v[1] - v0[1]) / h).clamp(-400.0, 400.0)]
            }
            None => {
                self.anchor = Some((anchor, [0.0; 2]));
                [0.0; 2]
            }
        };
        let (s, c) = p.yaw.sin_cos();
        let (side, ahead, up) = (c * accel[0], s * accel[0], accel[1]);
        let g = m.antenna_gain;
        let ins = [[-g * ahead, g * (side + 0.5 * up)], [-g * ahead, g * (side - 0.5 * up)]];
        for (osc, input) in self.antenna.iter_mut().zip(ins) {
            for (o, i) in osc.iter_mut().zip(input) {
                o.step(0.0, i, m.antenna_hz, m.antenna_damping, h);
            }
        }
        for o in &mut self.arm {
            o.step(0.0, -m.arm_gain * up, m.arm_hz, m.arm_damping, h);
        }
        for (r, osc) in [Role::AntennaL, Role::AntennaR].into_iter().zip(&self.antenna) {
            let j = p.joint_mut(r);
            j.r[0] += osc[0].x;
            j.r[2] += osc[1].x;
        }
        p.joint_mut(Role::ArmL).r[2] -= self.arm[0].x;
        p.joint_mut(Role::ArmR).r[2] += self.arm[1].x;

        self.blink_into(&mut p);
        self.out = p;
    }

    fn blink_into(&self, p: &mut Pose) {
        let shut = self.blink_closure();
        for e in [Role::EyeL, Role::EyeR] {
            p.joint_mut(e).s[1] *= 1.0 - 0.9 * shut;
        }
    }

    /// Under Reduce Motion a route ends where it ends, at once.
    fn travel_at_once(&mut self) {
        if let Some(last) = self.route.back() {
            self.pos = last.to;
        } else if let Some(hop) = self.hop {
            self.pos = hop.to;
        }
        self.route.clear();
        self.hop = None;
        self.speed = 0.0;
        self.landed = None;
    }

    /// Move along the route one step, posing the walk or hop into `p`; the turn the body wants for it.
    fn travel(&mut self, h: f32, p: &mut Pose) -> f32 {
        let m = self.tuning.motion;
        let cell = self.cell;
        let most = 4.0 * m.stride * m.cadence * cell;
        let mut face = 0.0;
        if self.hop.is_none()
            && let Some(leg) = self.route.front().copied()
        {
            let d = [leg.to[0] - self.pos[0], leg.to[1] - self.pos[1]];
            let dist = d[0].hypot(d[1]);
            match leg.gait {
                Gait::Hop => {
                    let climb = ((self.pos[1] - leg.to[1]) / cell).max(0.0);
                    self.hop = Some(Hop { at: self.t, from: self.pos, to: leg.to, air: m.hop_time.max(0.1), height: m.hop_height * self.cells + climb });
                    self.speed = 0.0;
                }
                Gait::Walk => {
                    let a = m.accel * cell;
                    let want = most.min((2.0 * a * dist).sqrt());
                    self.speed += (want - self.speed).clamp(-a * h, a * h);
                    let go = (self.speed * h).min(dist);
                    if dist <= go + 1e-4 {
                        self.pos = leg.to;
                        self.route.pop_front();
                        if !self.route.front().is_some_and(|l| l.gait == Gait::Walk) {
                            self.speed = 0.0;
                        }
                    } else {
                        self.pos = [self.pos[0] + d[0] / dist * go, self.pos[1] + d[1] / dist * go];
                        face = d[0] / dist * 1.15;
                    }
                    self.phase = (self.phase + go / (4.0 * m.stride * cell).max(1e-3)).rem_euclid(1.0);
                }
            }
        } else if self.hop.is_none() && self.speed > 0.0 {
            self.speed = 0.0;
        }
        let w = (self.speed / most.max(1e-3)).clamp(0.0, 1.0);
        if w > 0.0 {
            over(p, &walk(self.phase, m.stride, 0.9), w);
        }

        if let Some(hop) = self.hop {
            let tau = (self.t - hop.at) as f32;
            let crouch = m.anticipation.max(0.0);
            let d = [hop.to[0] - hop.from[0], hop.to[1] - hop.from[1]];
            let dist = d[0].hypot(d[1]);
            if dist > 1e-3 {
                face = d[0] / dist * 0.9;
            }
            if tau < crouch {
                let u = smooth(tau / crouch.max(1e-3));
                p.squash *= 1.0 - 0.2 * u;
                p.joint_mut(Role::Body).r[0] += 0.12 * u;
                p.joint_mut(Role::ArmL).r[0] += 0.5 * u;
                p.joint_mut(Role::ArmR).r[0] += 0.5 * u;
            } else if tau < crouch + hop.air {
                let s = (tau - crouch) / hop.air;
                self.pos = [hop.from[0] + d[0] * s, hop.from[1] + d[1] * s];
                p.lift += 4.0 * hop.height * s * (1.0 - s);
                p.squash *= 1.0 + 0.16 * (1.0 - 2.0 * s).powi(2) * if s < 0.5 { 1.0 } else { 0.5 };
                let tuck = (PI * s).sin();
                for f in [Role::FootL, Role::FootR] {
                    p.joint_mut(f).t[1] += 0.6 * tuck;
                }
            } else {
                self.pos = hop.to;
                self.hop = None;
                self.landed = Some(self.t);
                self.route.pop_front();
            }
        }
        if let Some(at) = self.landed {
            let u = (self.t - at) as f32;
            if u < 0.6 {
                p.squash *= 1.0 - 0.22 * (-9.0 * u).exp() * (15.0 * u).cos();
            } else {
                self.landed = None;
            }
        }
        if !self.route.is_empty() || self.hop.is_some() { face } else { 0.0 }
    }
}
