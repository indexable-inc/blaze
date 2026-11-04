//! blaze's soundtrack, synthesised up front in stereo from the same timeline constants the mark uses ([`crate::mark`]), so
//! sound and picture cannot drift apart.
//!
//! Copied from indexable-inc/anim's `src/audio.rs` (the Blaze parts: the letters landing on a climbing A major, the fire
//! catching on the I chord, and the exit burning the word away) and `src/fire_sound.rs` ([`fire`], the physically modelled
//! fire). Only the player is gone: [`soundtrack`] renders the whole thing offline, deterministic from its seed.

mod fire;

use std::f32::consts::{PI, SQRT_2, TAU};

use crate::mark as blaze;

/// Risset's bell: partial ratios, loudness and how long each rings.
const BELL: [(f32, f32, f32); 9] = [
    (0.56, 1.0, 1.0),
    (0.92, 0.67, 0.9),
    (1.19, 1.0, 0.65),
    (1.71, 1.8, 0.55),
    (2.0, 2.67, 0.33),
    (2.74, 1.67, 0.35),
    (3.0, 1.46, 0.25),
    (3.76, 1.33, 0.2),
    (4.07, 1.33, 0.15),
];

/// Level of the fire against the rest of the soundtrack.
const FIRE_GAIN: f32 = 0.9;

/// Seconds into the exit when the letters are burnt through, and when the last mote is gone (anim's `reveal::BURNT` and
/// `reveal::END`): the pad and the glints the exit ends on hang off them.
const BURNT: f32 = 3.5;
const END: f32 = 6.2;

/// xorshift64, as anim's.
pub struct Rng(pub u64);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, a: f32, b: f32) -> f32 {
        a + (b - a) * self.next()
    }

    fn fork(&mut self) -> Self {
        Self((self.0 ^ 0x9e37_79b9_7f4a_7c15).wrapping_mul(0xbf58_476d_1ce4_e5b9) | 1)
    }
}

struct Track {
    sr: f32,
    dry: Vec<[f32; 2]>,
    /// Mono feed into the reverb.
    send: Vec<f32>,
    rng: Rng,
}

impl Track {
    fn at(&self, t: f32) -> usize {
        (t.max(0.0) * self.sr) as usize
    }

    /// Adds `f(seconds since start)` for `dur` seconds from `t0`. It pans
    /// from `pan.0` to `pan.1` (-1 left, 1 right) and sends `send` of itself
    /// to the reverb.
    fn add(
        &mut self,
        t0: f32,
        dur: f32,
        pan: (f32, f32),
        send: f32,
        mut f: impl FnMut(f32, &mut Rng) -> f32,
    ) {
        let (a, n) = (self.at(t0), self.at(dur));
        let (dry, wet, rng) = (&mut self.dry, &mut self.send, &mut self.rng);
        for i in 0..n {
            let Some(d) = dry.get_mut(a + i) else { break };
            let t = i as f32 / self.sr;
            let v = f(t, rng);
            // Equal power, so a sound keeps its loudness as it moves.
            let p = pan.0 + (pan.1 - pan.0) * (i as f32 / n as f32);
            let angle = (p.clamp(-1.0, 1.0) + 1.0) * PI / 4.0;
            d[0] += v * angle.cos() * SQRT_2;
            d[1] += v * angle.sin() * SQRT_2;
            wet[a + i] += v * send;
        }
    }

    /// A short noise tick with a ringing tone, for small clicks.
    fn click(&mut self, t0: f32, amp: f32, freq: f32, pan: f32) {
        let mut prev = 0.0;
        self.add(t0, 0.06, (pan, pan), 0.2, |t, rng| {
            let n = rng.range(-1.0, 1.0);
            let hp = n - prev;
            prev = n;
            amp * ((-t * 600.0).exp() * hp * 0.6 + (-t * 70.0).exp() * (TAU * freq * t).sin() * 0.5)
        });
    }

    /// Filtered noise whose brightness follows `cutoff(progress)`.
    fn whoosh(
        &mut self,
        t0: f32,
        dur: f32,
        amp: f32,
        pan: (f32, f32),
        cutoff: impl Fn(f32) -> f32,
    ) {
        let sr = self.sr;
        let mut low = 0.0f32;
        self.add(t0, dur, pan, 0.25, |t, rng| {
            let p = t / dur;
            let k = 1.0 - (-TAU * cutoff(p) / sr).exp();
            low += k * (rng.range(-1.0, 1.0) - low);
            amp * (PI * p).sin().powi(2) * low * 3.0
        });
    }

    fn thud(&mut self, t0: f32, size: f32) {
        let sr = self.sr;
        let mut phase = 0.0f32;
        self.add(t0, 0.4, (0.0, 0.0), 0.15, |t, _| {
            phase += TAU * (42.0 + 30.0 * (-t * 18.0).exp()) / sr;
            0.5 * size * (-t * 11.0).exp() * phase.sin()
        });
        self.click(t0, 0.12 * size, 1800.0, 0.0);
    }

    /// A warm mallet note for a letter landing: a soft felt attack, a
    /// marimba-like body whose overtones die first, and a detuned twin on
    /// the other side for width, ringing long enough to join the chord.
    fn mallet(&mut self, t0: f32, freq: f32, amp: f32, ring: f32, pan: f32) {
        let sr = self.sr;
        let detune = 1.0 + self.rng.range(0.001, 0.0025);
        for (f, p, a) in [(freq, pan, 1.0), (freq * detune, -0.6 * pan, 0.45)] {
            self.add(t0, ring, (p, p), 0.45, move |t, _| {
                let attack = (t / 0.004).min(1.0);
                let body = (-t * 3.0 / ring).exp() * (TAU * f * t).sin();
                let second = 0.22 * (-t * 9.0 / ring).exp() * (TAU * 2.0 * f * t).sin();
                let wood = 0.12 * (-t * 40.0).exp() * (TAU * 3.93 * f * t).sin();
                amp * a * attack * (body + second + wood)
            });
        }
        // The felt: a few ms of soft, low-passed noise.
        let k = 1.0 - (-TAU * (freq * 3.0).min(5000.0) / sr).exp();
        let mut low = 0.0f32;
        self.add(t0, 0.03, (pan, pan), 0.2, move |t, rng| {
            low += k * (rng.range(-1.0, 1.0) - low);
            amp * 0.5 * (-t * 160.0).exp() * low
        });
    }

    /// A round sine bass, swelling in a little rather than plucked.
    fn bass(&mut self, t0: f32, freq: f32, amp: f32, dur: f32) {
        self.add(t0, dur, (0.0, 0.0), 0.15, move |t, _| {
            let env = (t / 0.02).min(1.0) * (-t * 2.2 / dur).exp() * (1.0 - (t / dur).powi(4));
            amp * env * ((TAU * freq * t).sin() + 0.15 * (TAU * 2.0 * freq * t).sin())
        });
    }

    fn bell(&mut self, t0: f32, freq: f32, amp: f32, dur: f32, pan: f32) {
        for &(r, a, d) in &BELL {
            let (f, ring) = (freq * r, dur * d);
            self.add(t0, ring, (pan, pan), 0.5, move |t, _| {
                amp * a / 14.0 * (-4.6 * t / ring).exp() * (TAU * f * t).sin()
            });
        }
    }

    /// Fire catching across the word in `catch` seconds and burning for
    /// `dur`: air drawn in just before, a whoomph that travels from the
    /// first letter to the last over a sub thump, then the fire itself.
    fn ignite(&mut self, t0: f32, catch: f32, dur: f32) {
        let sr = self.sr;
        let lp = |cutoff: f32| 1.0 - (-TAU * cutoff / sr).exp();
        // The intake: rising, brightening noise, cut right at the catch.
        let mut low = 0.0f32;
        self.add(t0 - 0.22, 0.22, (0.0, -0.3), 0.15, move |t, rng| {
            let p = t / 0.22;
            low += lp(400.0 + 5000.0 * p * p) * (rng.range(-1.0, 1.0) - low);
            0.35 * p.powi(3) * low
        });
        // Sub thump, its pitch falling.
        let mut phase = 0.0f32;
        self.add(t0, 1.2, (0.0, 0.0), 0.05, move |t, _| {
            phase += TAU * (38.0 + 70.0 * (-t * 14.0).exp()) / sr;
            0.5 * (t / 0.008).min(1.0) * (-t * 4.0).exp() * phase.sin()
        });
        // The whoomph: low noise that opens up fast, then closes slowly,
        // panning along the word as it catches.
        let mut low = 0.0f32;
        self.add(t0, catch + 0.9, (-0.6, 0.6), 0.4, move |t, rng| {
            let open = (t / 0.12).min(1.0);
            let cutoff = 120.0 + 1600.0 * open * (-(t - 0.12).max(0.0) * 2.5).exp() + 250.0;
            low += lp(cutoff) * (rng.range(-1.0, 1.0) - low);
            0.8 * (t / 0.03).min(1.0) * (-t * 2.0).exp() * low
        });
        self.fire(t0, catch, dur);
    }

    /// The fire itself, physically modelled (see `fire_sound`): it grows as
    /// the word catches, flares and gusts at the catch, then tapers to a low
    /// burn so the chord rings over it, and dies down at the end.
    fn fire(&mut self, t0: f32, catch: f32, dur: f32) {
        const GAIN: f32 = 0.6 * FIRE_GAIN;
        let (catch, dur) = (catch as f64, dur as f64);
        let seed = (self.rng.next() * u32::MAX as f32) as u32;
        let taper = move |t: f64| 0.3 + 0.7 * (-(t - catch).max(0.0) / 0.8).exp();
        let intensity = move |t: f64| {
            (0.15 + 0.85 * (t / catch).min(1.0)) * taper(t) * ((dur - t) / 1.5).clamp(0.0, 1.0)
        };
        let mut fire = fire::Fire::new(self.sr as f64, seed, catch, intensity);
        fire.flare(0.6);
        fire.gust(0.0, 0.5, 1.2);
        fire.pop(catch * 0.8);
        let a = self.at(t0);
        let (dry, wet) = fire.render(self.at(dur as f32));
        for (i, (d, w)) in dry.iter().zip(&wet).enumerate() {
            let Some(o) = self.dry.get_mut(a + i) else { break };
            o[0] += GAIN * d[0];
            o[1] += GAIN * d[1];
            self.send[a + i] += GAIN * w;
        }
    }

    /// The peak as Blaze lights: A major in full from a low A up, blooming
    /// in over a tenth of a second and ringing for seconds, with the high A
    /// struck on top, the note the letters were climbing to.
    fn resolve(&mut self, t0: f32) {
        self.bass(t0, 55.0, 0.13, 4.5);
        let voices = [
            (110.0, 0.045, 0.0),
            (164.81, 0.03, -0.3),
            (220.0, 0.035, 0.3),
            (277.18, 0.03, -0.45),
            (329.63, 0.028, 0.45),
            (440.0, 0.025, -0.2),
            (554.37, 0.016, 0.2),
        ];
        for (f, a, pan) in voices {
            let detune = self.rng.range(-0.5, 0.5);
            let bloom = self.rng.range(0.08, 0.16);
            self.add(t0, 4.5, (pan, pan), 0.45, move |t, _| {
                let env = (t / bloom).min(1.0).powi(2) * (-t * 0.75).exp();
                a * env
                    * ((TAU * f * t).sin()
                        + 0.5 * (TAU * (f + detune) * t).sin()
                        + 0.12 * (TAU * 2.0 * f * t).sin())
            });
        }
        self.mallet(t0, 880.0, 0.09, 3.5, 0.0);
        self.bell(t0 + 0.04, 1760.0, 0.03, 2.5, 0.1);
    }

    /// The fire eating the word and dying with it: a flare, a roar that
    /// fades out over `dur`, embers ticking as the letters crumble, the
    /// theme falling home to A as the last go out, and a soft pad and a
    /// few glints under the motes that linger after.
    fn burn_out(&mut self, dur: f32) {
        let pan = self.rng.range(-0.2, 0.2);
        self.whoosh(0.0, 0.7, 0.05, (pan, -pan), |p| 500.0 + 3500.0 * (PI * p).sin());
        let seed = (self.rng.next() * u32::MAX as f32) as u32;
        let d = dur as f64;
        let intensity = move |t: f64| (1.0 - t / d).clamp(0.0, 1.0).powf(1.5);
        let mut fire = fire::Fire::new(self.sr as f64, seed, 0.01, intensity);
        fire.flare(1.6);
        fire.gust(0.3, 0.8, 1.0);
        fire.pop(0.6);
        let (dry, wet) = fire.render(self.at(dur));
        for (i, (d, w)) in dry.iter().zip(&wet).enumerate() {
            let Some(o) = self.dry.get_mut(i) else { break };
            o[0] += FIRE_GAIN * d[0];
            o[1] += FIRE_GAIN * d[1];
            self.send[i] += FIRE_GAIN * w;
        }
        let mut t = 0.7;
        while t < 2.4 {
            let (amp, freq, pan) = (
                self.rng.range(0.04, 0.1) * (2.6 - t) / 2.0,
                self.rng.range(2500.0, 7000.0),
                self.rng.range(-0.6, 0.6),
            );
            self.click(t, amp, freq, pan);
            t += self.rng.range(0.03, 0.15) * (0.6 + t / 2.0);
        }
        // The theme coming home as the last letters go: E, A, then a low A.
        self.mallet(1.45, 1318.51, 0.04, 1.2, 0.15);
        self.mallet(1.6, 880.0, 0.05, 1.6, -0.15);
        self.mallet(1.8, 440.0, 0.06, 3.0, 0.0);
        self.bass(1.8, 110.0, 0.05, 2.5);
        // Waking from it: a soft, open A pad (with the B for air) breathes in
        // under the last motes and thins out with them, and a few of them
        // glint as they go, higher and fainter each time.
        let (from, to) = (BURNT - 0.9, END - 0.05);
        let len = to - from;
        for (f, a, pan) in [
            (220.0, 0.022, -0.3),
            (329.63, 0.018, 0.3),
            (493.88, 0.012, -0.15),
            (554.37, 0.013, 0.2),
            (880.0, 0.006, 0.0),
        ] {
            let detune = self.rng.range(-0.3, 0.3);
            self.add(from, len, (pan, pan), 0.7, move |t, _| {
                let p = t / len;
                let env = (t / 1.2).min(1.0).powi(2) * (0.5 + 0.5 * (PI * p).cos());
                a * env * ((TAU * f * t).sin() + 0.6 * (TAU * (f + detune) * t).sin())
            });
        }
        for (k, f) in [1760.0, 2217.46, 1975.53, 2637.02, 2959.96].into_iter().enumerate() {
            let at = BURNT - 0.2 + 0.45 * k as f32 + self.rng.range(-0.1, 0.1);
            let pan = self.rng.range(-0.5, 0.5);
            self.bell(at, f, 0.018 * (1.0 - 0.15 * k as f32), 1.4, pan);
        }
    }
}

/// Freeverb: eight damped combs in parallel and four allpasses in series per
/// side, the right side slightly detuned from the left for width.
fn reverb(input: &[f32], sr: f32) -> Vec<[f32; 2]> {
    const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
    const ALLPASSES: [usize; 4] = [556, 441, 341, 225];
    const ROOM: f32 = 0.86;
    const DAMP: f32 = 0.3;
    let len = |n: usize| ((n as f32 * sr / 44100.0) as usize).max(1);
    let mut out = vec![[0.0f32; 2]; input.len()];
    for (ch, spread) in [0, 23].into_iter().enumerate() {
        let mut combs: Vec<(Vec<f32>, usize, f32)> =
            COMBS.iter().map(|&n| (vec![0.0; len(n + spread)], 0, 0.0)).collect();
        let mut passes: Vec<(Vec<f32>, usize)> =
            ALLPASSES.iter().map(|&n| (vec![0.0; len(n + spread)], 0)).collect();
        for (i, &x) in input.iter().enumerate() {
            let x = x * 0.015;
            let mut y = 0.0;
            for (buf, pos, store) in &mut combs {
                let o = buf[*pos];
                *store = o * (1.0 - DAMP) + *store * DAMP;
                buf[*pos] = x + *store * ROOM;
                *pos = (*pos + 1) % buf.len();
                y += o;
            }
            for (buf, pos) in &mut passes {
                let b = buf[*pos];
                buf[*pos] = y + b * 0.5;
                y = b - y;
                *pos = (*pos + 1) % buf.len();
            }
            out[i][ch] = y * 3.0;
        }
    }
    out
}

impl Track {
    /// The dry track plus its reverb, saturated and normalised, and the
    /// gain that normalised it.
    fn mix(self) -> (Vec<[f32; 2]>, f32) {
        let mut out = self.saturated();
        let peak = out.iter().flatten().fold(0.0f32, |m, s| m.max(s.abs())).max(1e-6);
        let gain = 0.85 / peak;
        out.iter_mut().flatten().for_each(|s| *s *= gain);
        (out, gain)
    }

    /// The dry track plus its reverb, gently saturated.
    fn saturated(self) -> Vec<[f32; 2]> {
        let wet = reverb(&self.send, self.sr);
        self.dry
            .iter()
            .zip(&wet)
            .map(|(d, w)| [d[0] + w[0], d[1] + w[1]].map(|s| (s * 1.4).tanh() / 1.4))
            .collect()
    }
}

/// The mark's soundtrack, `seconds` long at `sr` Hz: the letters start falling at `start` seconds and, with `exit`, burn away
/// from then. Stereo frames, peaking under 0.75 (AAC's overshoot stays below full scale), the last fifth of a second faded out so the video ends clean.
#[must_use]
pub fn soundtrack(
    sr: u32,
    seconds: f32,
    start: f32,
    exit: Option<f32>,
    seed: u64,
) -> Vec<[f32; 2]> {
    let mut rng = Rng(seed | 1);
    let len = (seconds * sr as f32) as usize;
    let mut tr =
        Track { sr: sr as f32, dry: vec![[0.0; 2]; len], send: vec![0.0; len], rng: rng.fork() };
    let off = start;
    // The letters fall in left to right, climbing A B C# E to the leading tone G# over an E pedal, each a little louder:
    // a V that hangs unresolved until the fire catches on the I, A major, with a high A ringing over it as the peak.
    for (i, &start) in blaze::DROP_START.iter().enumerate() {
        let pan = -0.5 + 0.25 * i as f32;
        let land = off + start + blaze::DROP_DUR + tr.rng.range(-0.004, 0.004);
        let last = i == 4;
        tr.whoosh(off + start, blaze::DROP_DUR, 0.022, (pan, pan), |p| 300.0 + 1800.0 * p);
        tr.thud(land, if last { 0.5 } else { 0.25 });
        let amp = (0.05 + 0.008 * i as f32) * tr.rng.range(0.92, 1.08);
        tr.mallet(
            land,
            [440.0, 493.88, 554.37, 659.25, 830.61][i],
            amp,
            if last { 1.2 } else { 1.8 },
            pan,
        );
    }
    let first = off + blaze::DROP_START[0] + blaze::DROP_DUR;
    let lit = off + blaze::IGNITE_AT;
    tr.bass(first, 82.41, 0.07, lit - first + 0.08);
    // The fire burns on until the exit takes over (or the end).
    let burns = exit.map_or(seconds, |e| e + 0.6) - lit;
    tr.ignite(lit, blaze::IGNITE, burns.max(1.6));
    tr.resolve(lit);
    let (mut out, gain) = tr.mix();
    if let Some(at) = exit {
        // The exit as anim plays it: its own track through the same room, at the soundtrack's scale.
        let n = (END * sr as f32) as usize;
        let mut ex =
            Track { sr: sr as f32, dry: vec![[0.0; 2]; n], send: vec![0.0; n], rng: rng.fork() };
        ex.burn_out(3.2);
        let wet = reverb(&ex.send, sr as f32);
        let from = (at * sr as f32) as usize;
        for (i, (d, w)) in ex.dry.iter().zip(&wet).enumerate() {
            let Some(o) = out.get_mut(from + i) else { break };
            o[0] += gain * (d[0] + 0.5 * w[0]);
            o[1] += gain * (d[1] + 0.5 * w[1]);
        }
    }
    let tail = (0.2 * sr as f32) as usize;
    let n = out.len();
    for (i, o) in out.iter_mut().enumerate() {
        let left = n - i;
        let fade = if left < tail { left as f32 / tail as f32 } else { 1.0 };
        *o = o.map(|s| 0.9 * (s * 1.2).tanh() / 1.2 * fade);
    }
    out
}

/// `frames` as 16-bit stereo PCM WAV bytes.
#[must_use]
pub fn wav(sr: u32, frames: &[[f32; 2]]) -> Vec<u8> {
    let data = (frames.len() * 4) as u32;
    let mut b = Vec::with_capacity(44 + data as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    for v in [16u32, 1 | (2 << 16), sr, sr * 4, 4 | (16 << 16)] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data.to_le_bytes());
    for s in frames.iter().flatten() {
        b.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    b
}
