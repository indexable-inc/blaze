//! A physically based fire, for the Blaze logo catching light.
//!
//! Ported from procedural-fire's `research/audio-ref/fire-audio-dsp.mjs`
//! (MIT, Leon Lin, github.com/Leonxlnx/procedural-fire), trimmed to the
//! flame itself: no crickets, log shifts, distance or outdoor reverb.
//!
//! - Roar: pink noise through a one-pole knee, so its spectrum falls as
//!   f^-3 (Chadwick & James), its level log-normally modulated by flame
//!   puffing at 1.5/sqrt(D) Hz (Cetegen & Ahmed) plus 7 and 14 Hz flutter.
//! - Breath and lapping: the mid-band whoosh of the flame, and Farnell's
//!   30 Hz band driven into tanh for its flapping harmonics.
//! - Hiss: outgassing, high-passed noise in z^4 surges.
//! - Crackle: power-law energies, each a short fracture sequence, arriving as
//!   a Hawkes process so one crackle sets off more.
//! - Pops: resin pockets bursting, a rupture click, a cavity ring, a crack,
//!   a thump for big ones, a micro-crackle cluster and a steam hiss tail.
//!
//! The level follows an intensity curve; a flare and a gust make the catch.

use std::f64::consts::{PI, TAU};

/// Control-rate interval, samples.
const CR: usize = 32;

fn db(x: f64) -> f64 {
    10f64.powf(x / 20.0)
}

/// mulberry32, seeded per layer so layers never share a stream.
struct Rng {
    s: u32,
    spare: Option<f64>,
}

impl Rng {
    fn new(seed: u32, salt: &str) -> Self {
        let mut h = seed ^ 0x9e37_79b9;
        for c in salt.bytes() {
            h = (h ^ c as u32).wrapping_mul(0x85eb_ca6b);
            h ^= h >> 13;
        }
        h = (h ^ (h >> 16)).wrapping_mul(0xc2b2_ae35);
        h ^= h >> 16;
        Self { s: h, spare: None }
    }

    /// Uniform in [0, 1).
    fn next(&mut self) -> f64 {
        self.s = self.s.wrapping_add(0x6d2b_79f5);
        let mut t = self.s;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        (t ^ (t >> 14)) as f64 / 4_294_967_296.0
    }

    /// Uniform in [-1, 1).
    fn bi(&mut self) -> f64 {
        self.next() * 2.0 - 1.0
    }

    fn range(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.next()
    }

    fn log_range(&mut self, a: f64, b: f64) -> f64 {
        a * (b / a).powf(self.next())
    }

    fn gauss(&mut self) -> f64 {
        if let Some(g) = self.spare.take() {
            return g;
        }
        let u = self.next().max(1e-12);
        let (v, r) = (self.next(), (-2.0 * u.ln()).sqrt());
        self.spare = Some(r * (TAU * v).sin());
        r * (TAU * v).cos()
    }

    /// Truncated power law, pdf ~ x^-b on [lo, hi].
    fn pareto(&mut self, lo: f64, hi: f64, b: f64) -> f64 {
        let k = b - 1.0;
        let r = (lo / hi).powf(k);
        lo * (1.0 - self.next() * (1.0 - r)).powf(-1.0 / k)
    }
}

/// RBJ biquad, transposed direct form II.
#[derive(Clone, Default)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    fn set(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self { b: [b0 / a0, b1 / a0, b2 / a0], a: [a1 / a0, a2 / a0], z: [0.0; 2] }
    }

    fn wc(f: f64, q: f64, fs: f64) -> (f64, f64) {
        let w = TAU * f / fs;
        (w.cos(), w.sin() / (2.0 * q))
    }

    fn hp(f: f64, q: f64, fs: f64) -> Self {
        let (c, al) = Self::wc(f, q, fs);
        Self::set((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0, 1.0 + al, -2.0 * c, 1.0 - al)
    }

    /// Band-pass, 0 dB at the peak.
    fn bp(f: f64, q: f64, fs: f64) -> Self {
        let (c, al) = Self::wc(f, q, fs);
        Self::set(al, 0.0, -al, 1.0 + al, -2.0 * c, 1.0 - al)
    }

    fn peak(f: f64, q: f64, gain_db: f64, fs: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let (c, al) = Self::wc(f, q, fs);
        Self::set(1.0 + al * a, -2.0 * c, 1.0 - al * a, 1.0 + al / a, -2.0 * c, 1.0 - al / a)
    }

    fn high_shelf(f: f64, s: f64, gain_db: f64, fs: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w = TAU * f / fs;
        let c = w.cos();
        let al = w.sin() / 2.0 * ((a + 1.0 / a) * (1.0 / s - 1.0) + 2.0).sqrt();
        let sa = 2.0 * a.sqrt() * al;
        Self::set(
            a * ((a + 1.0) + (a - 1.0) * c + sa),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
            a * ((a + 1.0) + (a - 1.0) * c - sa),
            (a + 1.0) - (a - 1.0) * c + sa,
            2.0 * ((a - 1.0) - (a + 1.0) * c),
            (a + 1.0) - (a - 1.0) * c - sa,
        )
    }

    fn tick(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// Simper/Cytomic trapezoidal state-variable filter: stable under fast
/// cutoff changes. `k * bp` is the unity-peak band-pass.
#[derive(Clone, Default)]
struct Svf {
    ic: [f64; 2],
    k: f64,
    a: [f64; 3],
    lp: f64,
    bp: f64,
    hp: f64,
}

impl Svf {
    fn new(f: f64, q: f64, fs: f64) -> Self {
        let mut s = Self::default();
        s.set(f, q, fs);
        s
    }

    fn set(&mut self, f: f64, q: f64, fs: f64) {
        let g = (PI * f.min(fs * 0.45) / fs).tan();
        self.k = 1.0 / q;
        self.a[0] = 1.0 / (1.0 + g * (g + self.k));
        self.a[1] = g * self.a[0];
        self.a[2] = g * self.a[1];
    }

    fn tick(&mut self, v0: f64) {
        let v3 = v0 - self.ic[1];
        let v1 = self.a[0] * self.ic[0] + self.a[1] * v3;
        let v2 = self.ic[1] + self.a[1] * self.ic[0] + self.a[2] * v3;
        self.ic = [2.0 * v1 - self.ic[0], 2.0 * v2 - self.ic[1]];
        (self.lp, self.bp, self.hp) = (v2, v1, v0 - self.k * v1 - v2);
    }
}

#[derive(Clone, Default)]
struct OnePole {
    a: f64,
    y: f64,
}

impl OnePole {
    fn new(fc: f64, fs: f64) -> Self {
        Self { a: 1.0 - (-TAU * fc / fs).exp(), y: 0.0 }
    }

    fn lp(&mut self, x: f64) -> f64 {
        self.y += self.a * (x - self.y);
        self.y
    }
}

/// Std of a one-pole low-pass fed unit-variance white noise.
fn one_pole_noise_std(a: f64) -> f64 {
    (a / (2.0 - a)).sqrt()
}

/// Paul Kellet's refined pink filter.
#[derive(Clone, Default)]
struct Pink([f64; 7]);

impl Pink {
    fn tick(&mut self, w: f64) -> f64 {
        let b = &mut self.0;
        b[0] = 0.99886 * b[0] + w * 0.0555179;
        b[1] = 0.99332 * b[1] + w * 0.0750759;
        b[2] = 0.969 * b[2] + w * 0.153852;
        b[3] = 0.8665 * b[3] + w * 0.3104856;
        b[4] = 0.55 * b[4] + w * 0.5329522;
        b[5] = -0.7616 * b[5] - w * 0.016898;
        let p = b[0] + b[1] + b[2] + b[3] + b[4] + b[5] + b[6] + w * 0.5362;
        b[6] = w * 0.115926;
        p * 0.11
    }
}

/// Narrow-band unit-variance Gaussian at control rate: quasi-periodic puffing.
struct NbMod {
    svf: Svf,
    norm: f64,
}

impl NbMod {
    fn new(f: f64, q: f64, fcr: f64) -> Self {
        Self { svf: Svf::new(f, q, fcr), norm: 1.0 / ((PI / 2.0) * (f / q) / (fcr / 2.0)).sqrt() }
    }

    fn tick(&mut self, rng: &mut Rng) -> f64 {
        self.svf.tick(rng.gauss());
        self.svf.k * self.svf.bp * self.norm
    }
}

/// Slow unit-variance Gaussian at control rate.
struct SlowMod {
    op: OnePole,
    norm: f64,
}

impl SlowMod {
    fn new(fc: f64, fcr: f64) -> Self {
        let op = OnePole::new(fc, fcr);
        let norm = 1.0 / one_pole_noise_std(op.a);
        Self { op, norm }
    }

    fn tick(&mut self, rng: &mut Rng) -> f64 {
        self.op.lp(rng.gauss()) * self.norm
    }
}

/// Equal-power pan, `p` in [-1, 1].
fn pan_gains(p: f64) -> [f64; 2] {
    let th = (p.clamp(-1.0, 1.0) + 1.0) * PI / 4.0;
    [th.cos(), th.sin()]
}

/// Where a voice goes: its stereo gains and reverb send.
#[derive(Clone, Copy, Default)]
struct Out {
    g: [f64; 2],
    send: f64,
}

/// A crackle: 1 to 5 noise impulses in a quick fracture sequence,
/// exponentially decaying, part band-passed and part raw.
#[derive(Clone, Default)]
struct Crackle {
    active: bool,
    out: Out,
    svf: Svf,
    sub_t: [u32; 6],
    sub_a: [f64; 6],
    n_sub: usize,
    si: usize,
    t: u32,
    env: f64,
    amp: f64,
    d: f64,
    raw: f64,
    lvl: f64,
}

impl Crackle {
    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        rng: &mut Rng,
        fs: f64,
        amp: f64,
        tau_ms: f64,
        fc: f64,
        q: f64,
        raw: f64,
        out: Out,
        n_sub: usize,
        gap_ms: f64,
        lvl: f64,
    ) {
        (self.active, self.t, self.env, self.amp, self.lvl, self.raw, self.out) =
            (true, 0, 0.0, amp, lvl, raw, out);
        self.d = (-1000.0 / (tau_ms * fs)).exp();
        self.svf = Svf::new(fc, q, fs);
        (self.n_sub, self.si) = (n_sub, 0);
        let mut tt = 0.0f64;
        for i in 0..n_sub {
            self.sub_t[i] = tt.round() as u32;
            self.sub_a[i] = amp * if i == 0 { 1.0 } else { rng.range(0.25, 0.9) };
            tt += rng.range(0.3, 1.0) * gap_ms * fs / 1000.0 + 1.0;
        }
    }

    fn tick(&mut self, rng: &mut Rng) -> f64 {
        if self.si < self.n_sub && self.t == self.sub_t[self.si] {
            self.env = self.env.max(self.sub_a[self.si]);
            self.si += 1;
        }
        let x = rng.bi() * self.env;
        self.svf.tick(x);
        // 2.2 makes up for the energy the band-pass takes.
        let y = (1.0 - self.raw) * self.svf.k * self.svf.bp * 2.2 + self.raw * x;
        self.env *= self.d;
        self.t += 1;
        if self.si >= self.n_sub && self.env < self.amp * 3e-4 {
            self.active = false;
        }
        y * self.lvl
    }
}

/// Up to three damped sinusoids: cavity rings, thumps.
#[derive(Clone, Default)]
struct Modal {
    active: bool,
    out: Out,
    n: usize,
    c: [[f64; 2]; 3],
    y: [[f64; 2]; 3],
    t: u32,
    len: u32,
}

impl Modal {
    fn start(&mut self, fs: f64, modes: &[(f64, f64, f64)], out: Out) {
        (self.active, self.n, self.t, self.out) = (true, modes.len(), 0, out);
        let mut max_tau = 0.0f64;
        for (i, &(f, tau, amp)) in modes.iter().enumerate() {
            let th = TAU * f.min(fs * 0.45) / fs;
            let r = (-1.0 / (tau * fs)).exp();
            self.c[i] = [2.0 * r * th.cos(), -r * r];
            // Starting at phase 0 keeps it click-free.
            self.y[i] = [amp * r * th.sin(), 0.0];
            max_tau = max_tau.max(tau);
        }
        self.len = (max_tau * 9.2 * fs).ceil() as u32;
    }

    fn tick(&mut self) -> f64 {
        let mut out = 0.0;
        for i in 0..self.n {
            let [y1, y2] = self.y[i];
            out += y2;
            self.y[i] = [self.c[i][0] * y1 + self.c[i][1] * y2, y1];
        }
        self.t += 1;
        if self.t > self.len {
            self.active = false;
        }
        out
    }
}

/// A filtered noise burst with an attack and exponential decay, optionally
/// sputtering.
#[derive(Clone, Default)]
struct Noise {
    active: bool,
    out: Out,
    svf: Svf,
    jit: OnePole,
    t: u32,
    att: u32,
    len: u32,
    amp: f64,
    env: f64,
    d: f64,
    jd: f64,
    jn: f64,
}

impl Noise {
    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        fs: f64,
        amp: f64,
        f: f64,
        q: f64,
        attack_ms: f64,
        tau_ms: f64,
        jitter: f64,
        out: Out,
    ) {
        (self.active, self.t, self.amp, self.out, self.env) = (true, 0, amp, out, 0.0);
        self.svf = Svf::new(f, q, fs);
        self.att = ((attack_ms * fs / 1000.0).round() as u32).max(1);
        self.d = (-1000.0 / (tau_ms * fs)).exp();
        self.len = self.att + (tau_ms * 9.2 * fs / 1000.0).ceil() as u32;
        self.jit = OnePole::new(40.0, fs);
        self.jd = jitter;
        self.jn = if jitter > 0.0 { 1.0 / one_pole_noise_std(self.jit.a) } else { 0.0 };
    }

    fn tick(&mut self, rng: &mut Rng) -> f64 {
        let e = if self.t < self.att {
            self.t as f64 / self.att as f64 * self.amp
        } else {
            self.env = if self.t == self.att { self.amp } else { self.env * self.d };
            self.env
        };
        self.svf.tick(rng.bi());
        let mut y = self.svf.k * self.svf.bp;
        if self.jd > 0.0 {
            y *= (1.0 + self.jd * self.jit.lp(rng.gauss()) * self.jn).max(0.0);
        }
        self.t += 1;
        if self.t > self.len {
            self.active = false;
        }
        y * e * 1.7
    }
}

/// A free voice, else the next one round-robin.
fn take<T>(pool: &mut [T], rr: &mut usize, active: impl Fn(&T) -> bool) -> usize {
    if let Some(i) = pool.iter().position(|v| !active(v)) {
        return i;
    }
    let i = *rr;
    *rr = (*rr + 1) % pool.len();
    i
}

/// Layer levels: the original's mix (dB) times its measured calibration.
struct Mix {
    ticks: f64,
    crackles: f64,
    roar: f64,
    breath: f64,
    lapping: f64,
    hiss: f64,
    pops: f64,
    whoosh: f64,
}

impl Mix {
    fn new() -> Self {
        Self {
            ticks: db(-9.0) * 3.677,
            crackles: db(-4.0) * 15.53,
            roar: db(-7.0) * 0.1106,
            breath: db(-8.0) * 0.4097,
            lapping: db(-15.0) * 0.1996,
            hiss: db(-16.0) * 0.3187,
            pops: db(2.0) * 1.304,
            whoosh: db(-9.0) * 0.2219,
        }
    }
}

const SEND_TICKS: f64 = 0.12;
const SEND_CRACKLES: f64 = 0.2;
const SEND_ROAR: f64 = 0.08;
const SEND_HISS: f64 = 0.1;
const SEND_POPS: f64 = 0.4;
const SEND_WHOOSH: f64 = 0.15;
/// Pan range of the fire.
const WIDTH: f64 = 0.55;

enum Event {
    MicroCrackle { pan: f64, size: f64 },
    Pop,
}

struct Tick {
    hp: Biquad,
    bp: Biquad,
    env: f64,
    d: f64,
    route: f64,
}

struct RoarCh {
    pink: Pink,
    knee: OnePole,
    hp: Biquad,
    body: Biquad,
    breath: Biquad,
    ms: f64,
}

struct BreathCh {
    pink: Pink,
    svf: Svf,
    hp: Biquad,
}

struct Gust {
    t0: f64,
    s: f64,
    att: f64,
    rel: f64,
}

pub struct Fire<I: Fn(f64) -> f64> {
    fs: f64,
    /// Target intensity at a time: 0 out, 1 a healthy fire.
    intensity: I,
    /// Seconds for the fire to spread from the left of the stereo field to
    /// the right; crackle only comes from where it has caught.
    catch: f64,
    n: usize,
    queue: Vec<(usize, Event)>,
    mix: Mix,
    r_tick: Rng,
    r_crk: Rng,
    r_pop: Rng,
    r_roar: Rng,
    r_hiss: Rng,
    r_mod: Rng,
    r_whoosh: Rng,
    r_voice: Rng,
    i: f64,
    ie: f64,
    a_smooth: f64,
    flare: f64,
    k_flare: f64,
    gusts: Vec<Gust>,
    gust: f64,
    gust_lag: f64,
    k_gust_lag: f64,
    hawkes: f64,
    k_hawkes: f64,
    tick_burst: f64,
    k_tick_burst: f64,
    m_puff: NbMod,
    m_flut: NbMod,
    m_flap: NbMod,
    m_slow: SlowMod,
    m_hiss: SlowMod,
    m_dens: SlowMod,
    roar_amp: f64,
    breath_amp: f64,
    hiss_amp: f64,
    whoosh_amp: f64,
    whoosh_pan: f64,
    p_tick: f64,
    p_crk: f64,
    p_pop: f64,
    last_knee: f64,
    ticks: [Tick; 2],
    roar: [RoarCh; 2],
    breath: [BreathCh; 2],
    lap_bp: Biquad,
    lap_hp: [Biquad; 2],
    lap_norm: f64,
    hiss: [(Biquad, Biquad); 2],
    whoosh_svf: [Svf; 2],
    whoosh_pink: [Pink; 2],
    gr_buf: [[f64; 256]; 2],
    gr_idx: usize,
    gr_lp: [OnePole; 2],
    gr_d: [usize; 2],
    crackles: Vec<Crackle>,
    modals: Vec<Modal>,
    noises: Vec<Noise>,
    rr: [usize; 3],
    /// Every pop so far: when, in seconds, and how big, 0.25 to 1.2.
    pub pops: Vec<(f64, f64)>,
}

impl<I: Fn(f64) -> f64> Fire<I> {
    pub fn new(fs: f64, seed: u32, catch: f64, intensity: I) -> Self {
        let fcr = fs / CR as f64;
        let r_mod = Rng::new(seed, "mod");
        // Puffing frequency of a pool fire of this diameter, metres.
        let diameter = 0.7f64;
        let roar_ch = || RoarCh {
            pink: Pink::default(),
            knee: OnePole::new(250.0, fs),
            hp: Biquad::hp(45.0, 0.7, fs),
            body: Biquad::peak(120.0, 0.8, 4.0, fs),
            breath: Biquad::peak(420.0, 1.0, 2.0, fs),
            ms: 1e-4,
        };
        let tick_ch = || Tick {
            hp: Biquad::hp(2500.0, 0.7, fs),
            bp: Biquad::bp(5500.0, 0.9, fs),
            env: 0.0,
            d: 0.0,
            route: 0.0,
        };
        let breath_ch = || BreathCh {
            pink: Pink::default(),
            svf: Svf::new(750.0, 0.55, fs),
            hp: Biquad::hp(220.0, 0.7, fs),
        };
        let hiss_ch = || (Biquad::hp(1200.0, 0.6, fs), Biquad::high_shelf(9000.0, 0.8, -4.0, fs));
        Self {
            fs,
            intensity,
            catch,
            n: 0,
            queue: Vec::new(),
            mix: Mix::new(),
            r_tick: Rng::new(seed, "tick"),
            r_crk: Rng::new(seed, "crackle"),
            r_pop: Rng::new(seed, "pop"),
            r_roar: Rng::new(seed, "roar"),
            r_hiss: Rng::new(seed, "hiss"),
            r_whoosh: Rng::new(seed, "whoosh"),
            r_voice: Rng::new(seed, "voicenoise"),
            r_mod,
            i: 0.0,
            ie: 0.0,
            a_smooth: 1.0 - (-1.0 / (0.15 * fcr)).exp(),
            flare: 0.0,
            k_flare: (-1.0 / (2.0 * fcr)).exp(),
            gusts: Vec::new(),
            gust: 0.0,
            gust_lag: 0.0,
            k_gust_lag: 1.0 - (-1.0 / (0.4 * fcr)).exp(),
            hawkes: 0.0,
            k_hawkes: (-(CR as f64) / (0.07 * fs)).exp(),
            tick_burst: 0.0,
            k_tick_burst: (-1.0 / (0.25 * fcr)).exp(),
            m_puff: NbMod::new(1.5 / diameter.sqrt(), 2.5, fcr),
            m_flut: NbMod::new(7.0, 1.2, fcr),
            m_flap: NbMod::new(14.0, 2.0, fcr),
            m_slow: SlowMod::new(0.12, fcr),
            m_hiss: SlowMod::new(1.0, fcr),
            m_dens: SlowMod::new(0.6, fcr),
            roar_amp: 0.0,
            breath_amp: 0.0,
            hiss_amp: 0.0,
            whoosh_amp: 0.0,
            whoosh_pan: 0.0,
            p_tick: 0.0,
            p_crk: 0.0,
            p_pop: 0.0,
            last_knee: -1.0,
            ticks: [tick_ch(), tick_ch()],
            roar: [roar_ch(), roar_ch()],
            breath: [breath_ch(), breath_ch()],
            lap_bp: Biquad::bp(30.0, 5.0, fs),
            lap_hp: [Biquad::hp(25.0, 0.7, fs), Biquad::hp(25.0, 0.7, fs)],
            lap_norm: 1.0 / ((PI / 2.0) * (30.0 / 5.0) / (fs / 2.0)).sqrt(),
            hiss: [hiss_ch(), hiss_ch()],
            whoosh_svf: [Svf::default(), Svf::default()],
            whoosh_pink: [Pink::default(), Pink::default()],
            gr_buf: [[0.0; 256]; 2],
            gr_idx: 0,
            gr_lp: [OnePole::new(2500.0, fs), OnePole::new(2500.0, fs)],
            gr_d: [(0.00085 * fs).round() as usize, (0.00105 * fs).round() as usize],
            crackles: vec![Crackle::default(); 48],
            modals: vec![Modal::default(); 40],
            noises: vec![Noise::default(); 16],
            rr: [0; 3],
            pops: Vec::new(),
        }
    }

    /// The fire flaring up by `amount`, decaying over a couple of seconds.
    pub fn flare(&mut self, amount: f64) {
        self.flare += amount;
    }

    /// A gust of air at `t` seconds: strength, then total length.
    pub fn gust(&mut self, t: f64, s: f64, dur: f64) {
        self.gusts.push(Gust { t0: t, s, att: dur * 0.35, rel: dur * 0.4 });
    }

    /// A resin pocket bursting at `t` seconds.
    pub fn pop(&mut self, t: f64) {
        self.schedule(t, Event::Pop);
    }

    fn schedule(&mut self, t: f64, ev: Event) {
        let n = self.n.max((t * self.fs).round() as usize);
        let i = self.queue.partition_point(|(m, _)| *m <= n);
        self.queue.insert(i, (n, ev));
    }

    fn t(&self) -> f64 {
        self.n as f64 / self.fs
    }

    /// Pan for something new in the fire: only where it has caught so far.
    fn lit_pan(&self, rng_v: f64) -> f64 {
        let lit = (self.t() / self.catch).min(1.0);
        -WIDTH + 2.0 * WIDTH * lit * rng_v
    }

    fn control(&mut self) {
        let t = self.t();
        self.i += self.a_smooth * ((self.intensity)(t) - self.i);
        let mut g = 0.0;
        for gu in &self.gusts {
            let x = t - gu.t0;
            if x < 0.0 {
                continue;
            }
            g += if x < gu.att {
                gu.s * 0.5 * (1.0 - (PI * x / gu.att).cos())
            } else {
                gu.s * (-(x - gu.att) / gu.rel).exp()
            };
        }
        self.gust = g.min(1.5);
        self.gust_lag += self.k_gust_lag * (self.gust - self.gust_lag);
        self.flare *= self.k_flare;
        self.hawkes *= self.k_hawkes;
        self.tick_burst *= self.k_tick_burst;

        let ie = (self.i * (1.0 + self.flare)).max(0.0);
        self.ie = ie;
        let rm = &mut self.r_mod;
        let (up, uf, ufl, us) = (
            self.m_puff.tick(rm),
            self.m_flut.tick(rm),
            self.m_flap.tick(rm),
            self.m_slow.tick(rm),
        );
        let (zh, ud) = (self.m_hiss.tick(rm), self.m_dens.tick(rm));
        let flap = 0.12 + 0.35 * self.gust;
        let sd = (0.32f64.powi(2) + 0.16f64.powi(2) + flap * flap + 0.22f64.powi(2)).sqrt();
        let mut ln_amp = 0.32 * up + 0.16 * uf + flap * ufl + 0.22 * us;
        // Soft-cap the swells at about 2.2 sigma.
        ln_amp = 2.2 * sd * (ln_amp / (2.2 * sd)).tanh() - 0.5 * sd * sd;
        self.roar_amp = ie.powf(1.5) * ln_amp.exp() * (1.0 + 1.4 * self.gust);
        let bf = 0.2 + 0.3 * self.gust;
        let ln_b = 0.25 * up + 0.4 * uf + bf * ufl - 0.5 * (0.0625 + 0.16 + bf * bf);
        self.breath_amp = ie.powf(1.3) * ln_b.exp() * (1.0 + self.gust);
        // Brighter when stronger or windier.
        let knee = (150.0 + 220.0 * ie.min(1.5) + 260.0 * self.gust + 40.0 * up).max(60.0);
        if (knee - self.last_knee).abs() > 2.0 {
            self.last_knee = knee;
            let a = OnePole::new(knee, self.fs).a;
            self.roar[0].knee.a = a;
            self.roar[1].knee.a = a;
        }
        // Farnell's hiss surges, z^4, over a steady floor.
        self.hiss_amp =
            ie * (0.4 + 0.6 * (zh * zh * zh * zh).min(16.0) / 3.0) * (1.0 + 0.8 * self.gust);
        let dens = (0.35 * ud - 0.06).exp() * (1.0 + 0.25 * up);
        self.p_tick = ((260.0 * ie.powf(1.2) * dens * (1.0 + 0.8 * self.gust_lag)
            + self.tick_burst)
            / self.fs)
            .min(0.2);
        self.p_crk = ((16.0 * ie.powf(1.3) * (1.0 + 0.2 * up) * (1.0 + 1.5 * self.gust_lag)
            + self.hawkes)
            / self.fs)
            .min(0.2);
        self.p_pop = 0.12 * ie.powf(1.5) * (1.0 + self.gust_lag) / self.fs;
        self.whoosh_amp = self.gust.min(1.5).powf(1.5);
        if self.whoosh_amp > 1e-4 {
            let wf = 280.0 + 750.0 * self.gust.min(1.0);
            self.whoosh_svf[0].set(wf, 0.9, self.fs);
            self.whoosh_svf[1].set(wf * 1.12, 0.9, self.fs);
        }
        self.whoosh_pan = (self.gust * 0.6 - 0.3).clamp(-1.0, 1.0);
    }

    fn crackle(&mut self, scale: f64, around: Option<f64>) {
        let r = &mut self.r_crk;
        // Energy follows a power law, as acoustic emission from breaking wood does.
        let e = r.pareto(1.0, 100.0, 1.6) * scale;
        let sz = (e.log10() / 2.0).min(1.0);
        let amp = 0.02 * e.powf(0.4);
        let tau = 2.0 * e.powf(0.15) * r.log_range(0.5, 2.5);
        let fc = r.log_range(1800.0, 11000.0) / (1.0 + 0.6 * sz);
        let (q, raw, pv) = (r.range(0.8, 2.5), r.range(0.15, 0.45), r.next());
        let pan = match around {
            Some(p) => p + r.range(-1.0, 1.0) * 0.08,
            None => self.lit_pan(pv),
        };
        let r = &mut self.r_crk;
        let mut n_sub = 1;
        while n_sub < 5 && r.next() < 0.45 {
            n_sub += 1;
        }
        let i = take(&mut self.crackles, &mut self.rr[0], |v| v.active);
        let out = Out { g: pan_gains(pan), send: SEND_CRACKLES };
        self.crackles[i].start(
            &mut self.r_voice,
            self.fs,
            amp,
            tau,
            fc,
            q,
            raw,
            out,
            n_sub,
            1.6 + 2.0 * sz,
            self.mix.crackles,
        );
        // Bigger crackles set off more.
        self.hawkes += (0.45 * (0.6 + 0.8 * sz)).min(0.85) / 0.07;
    }

    fn modal(&mut self, modes: &[(f64, f64, f64)], pan: f64, send: f64) {
        let i = take(&mut self.modals, &mut self.rr[1], |v| v.active);
        self.modals[i].start(self.fs, modes, Out { g: pan_gains(pan), send });
    }

    #[allow(clippy::too_many_arguments)]
    fn noise(
        &mut self,
        amp: f64,
        f: f64,
        q: f64,
        attack_ms: f64,
        tau_ms: f64,
        jitter: f64,
        pan: f64,
        send: f64,
    ) {
        let i = take(&mut self.noises, &mut self.rr[2], |v| v.active);
        self.noises[i].start(
            self.fs,
            amp,
            f,
            q,
            attack_ms,
            tau_ms,
            jitter,
            Out { g: pan_gains(pan), send },
        );
    }

    fn fire_pop(&mut self) {
        let r = &mut self.r_pop;
        let s = r.pareto(0.25, 1.2, 2.2);
        let pv = r.next();
        self.pops.push((self.t(), s));
        let pan = self.lit_pan(pv);
        let (l, send, fs) = (self.mix.pops, SEND_POPS, self.fs);
        // Rupture click: a very short pressure pulse.
        let i = take(&mut self.crackles, &mut self.rr[0], |v| v.active);
        self.crackles[i].start(
            &mut self.r_voice,
            fs,
            0.5 * l * s.powf(0.8),
            0.5,
            7000.0,
            0.7,
            0.85,
            Out { g: pan_gains(pan), send },
            1,
            0.0,
            1.0,
        );
        // The pocket rings like a Helmholtz cavity: bigger is lower.
        let r = &mut self.r_pop;
        let f1 = r.log_range(1200.0, 4500.0) / s.powf(0.4);
        let (f2, f3) = (f1 * r.range(1.4, 2.3), f1 * r.range(2.6, 3.8));
        let tau = r.range(0.004, 0.012) * s.sqrt();
        let a = 0.11 * l * s;
        self.modal(&[(f1, tau, a), (f2, tau * 0.6, a * 0.5), (f3, tau * 0.4, a * 0.25)], pan, send);
        // The crack itself.
        let r = &mut self.r_pop;
        let (cf, ctau) = (r.range(2500.0, 5000.0) / s.sqrt(), r.range(2.0, 6.0) * s.sqrt());
        self.noise(0.16 * l * s, cf, 0.7, 0.1, ctau, 0.0, pan, send);
        // A thump from the gas pulse of big ones.
        if s > 0.5 {
            let f = self.r_pop.range(90.0, 200.0);
            self.modal(&[(f, 0.02 * s, 0.32 * l * s)], pan * 0.5, send * 0.5);
        }
        if s > 0.7 {
            self.tick_burst += 700.0 * s;
        }
        // The fracture runs on as a cluster of small crackles.
        let r = &mut self.r_pop;
        let n_micro = (3.0 + 9.0 * s * r.next()).round() as usize;
        let mut tt = 0.0;
        let now = self.t();
        for _ in 0..n_micro {
            tt += self.r_pop.range(0.002, 0.012);
            self.schedule(now + tt, Event::MicroCrackle { pan, size: 0.35 * s });
        }
        // Steam jetting out.
        if self.r_pop.next() < 0.3 + 0.3 * s {
            let r = &mut self.r_pop;
            let (f, tau) = (r.range(3500.0, 7000.0), r.range(60.0, 250.0) * s);
            self.noise(0.03 * l * s, f, 0.9, 8.0, tau, 0.6, pan, send);
        }
        self.hawkes += 35.0 * s;
    }

    /// Renders `n` stereo frames, and the mono feed for a reverb.
    pub fn render(&mut self, n: usize) -> (Vec<[f32; 2]>, Vec<f32>) {
        let (mut dry, mut wet) = (Vec::with_capacity(n), Vec::with_capacity(n));
        for _ in 0..n {
            if self.n % CR == 0 {
                self.control();
            }
            while self.queue.first().is_some_and(|(m, _)| *m <= self.n) {
                match self.queue.remove(0).1 {
                    Event::MicroCrackle { pan, size } => self.crackle(size, Some(pan)),
                    Event::Pop => self.fire_pop(),
                }
            }
            // Bernoulli per sample: Poisson with a time-varying rate.
            if self.r_crk.next() < self.p_crk {
                self.crackle(1.0, None);
            }
            if self.r_pop.next() < self.p_pop {
                self.fire_pop();
            }
            let (l, r, sl, sr) = self.sample();
            dry.push([l as f32, r as f32]);
            wet.push((0.5 * (sl + sr)) as f32);
            self.n += 1;
        }
        (dry, wet)
    }

    fn sample(&mut self) -> (f64, f64, f64, f64) {
        let m = &self.mix;
        let (mut out, mut send) = ([0.0f64; 2], [0.0f64; 2]);
        // Ticks: independent per side, through two fixed colours.
        for (c, tk) in self.ticks.iter_mut().enumerate() {
            let rt = &mut self.r_tick;
            if rt.next() < self.p_tick {
                let a = db(-17.0 + 3.5 * rt.gauss());
                if a > tk.env {
                    tk.env = a;
                    tk.d = (-1.0 / (rt.range(0.08, 0.3) * 1e-3 * self.fs)).exp();
                    tk.route = rt.next();
                }
            }
            let (mut x0, mut x1) = (0.0, 0.0);
            if tk.env > 1e-5 {
                let x = tk.env * rt.bi();
                tk.env *= tk.d;
                if tk.route < 0.55 { x0 = x } else { x1 = x }
            }
            let y = (tk.hp.tick(x0) + tk.bp.tick(x1) * 1.5) * m.ticks;
            out[c] += y;
            send[c] += y * SEND_TICKS;
        }
        // Roar: correlated stereo, soft-saturated at three times its running RMS.
        let rr = &mut self.r_roar;
        let (mid, side) = (rr.bi(), rr.bi() * 0.45);
        for (c, ch) in self.roar.iter_mut().enumerate() {
            let x = ch.pink.tick(if c == 0 { mid + side } else { mid - side });
            let x = ch.breath.tick(ch.body.tick(ch.hp.tick(ch.knee.lp(x))));
            ch.ms += 2e-5 * (x * x - ch.ms);
            let cl = 3.0 * (ch.ms + 1e-12).sqrt();
            let y = cl * (x / cl).tanh() * self.roar_amp * m.roar * 6.0;
            out[c] += y;
            send[c] += y * SEND_ROAR;
        }
        // Lapping: a 30 Hz band driven into tanh, for the flapping harmonics.
        let lb = self.lap_bp.tick(rr.bi() * 1.732_050_8) * self.lap_norm;
        let l0 = self.lap_hp[0].tick((1.6 * lb).tanh());
        let ly = self.lap_hp[1].tick(l0) * self.ie.powf(1.2) * (1.0 + 1.2 * self.gust) * m.lapping;
        out[0] += ly;
        out[1] += ly;
        // Breath: the flame's mid-band whoosh.
        for (c, ch) in self.breath.iter_mut().enumerate() {
            ch.svf.tick(ch.pink.tick(rr.bi() * 1.732_050_8));
            let y = ch.hp.tick(ch.svf.k * ch.svf.bp) * self.breath_amp * m.breath;
            out[c] += y;
            send[c] += y * SEND_ROAR;
        }
        // Hiss: gas escaping.
        for (c, (hp, hs)) in self.hiss.iter_mut().enumerate() {
            let y = hs.tick(hp.tick(self.r_hiss.bi())) * self.hiss_amp * m.hiss * 0.35;
            out[c] += y;
            send[c] += y * SEND_HISS;
        }
        // Gust band.
        if self.whoosh_amp > 1e-4 {
            let wa = self.whoosh_amp * m.whoosh;
            let g = pan_gains(self.whoosh_pan);
            let mut y = [0.0; 2];
            for c in 0..2 {
                let w = self.r_whoosh.bi() * 1.732_050_8;
                let s = &mut self.whoosh_svf[c];
                s.tick(self.whoosh_pink[c].tick(w));
                y[c] = s.bp * s.k * wa * 2.5;
            }
            out[0] += y[0] * g[0] * 1.2 + y[1] * 0.3;
            out[1] += y[1] * g[1] * 1.2 + y[0] * 0.3;
            send[0] += y[0] * SEND_WHOOSH;
            send[1] += y[1] * SEND_WHOOSH;
        }
        let mut voice = |y: f64, o: Out| {
            for c in 0..2 {
                out[c] += y * o.g[c];
                send[c] += y * o.g[c] * o.send;
            }
        };
        for v in self.crackles.iter_mut().filter(|v| v.active) {
            voice(v.tick(&mut self.r_voice), v.out);
        }
        for v in self.modals.iter_mut().filter(|v| v.active) {
            voice(v.tick(), v.out);
        }
        for v in self.noises.iter_mut().filter(|v| v.active) {
            voice(v.tick(&mut self.r_voice), v.out);
        }
        // A ground reflection, a millisecond late and dulled.
        let gi = self.gr_idx;
        for c in 0..2 {
            self.gr_buf[c][gi & 255] = out[c];
            out[c] += 0.25 * self.gr_lp[c].lp(self.gr_buf[c][gi.wrapping_sub(self.gr_d[c]) & 255]);
        }
        self.gr_idx = gi + 1;
        (out[0], out[1], send[0], send[1])
    }
}
