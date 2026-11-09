//! The app's music: a small band that plays itself and follows a mood, a game's adaptive score. It is synthesised
//! and composed live, sample by sample, in the audio callback ([`crate::audio`]); nothing is a recording or a loop.
//!
//! - **Layers.** A pad holding the bar's chord, a bass under it, an arpeggio through it, a plucked lead playing a motif
//!   it repeats and varies, far bells, and drums (a kick, a low drum, a hat). Each has a level; a mood is mostly which
//!   layers are up.
//! - **Moods** (the scene's, `hn.sound.mood`: scene/widgets/sound.flecs): each a tempo, a scale by day and one by night,
//!   a chord progression (a root a bar, as steps of the scale) and its layers' levels. `calm` is a pad, a slow lead and
//!   bells; `flow` adds bass and a gentle arpeggio; `drive` quickens and brings the drums in; `battle` is the minor
//!   scale, fast, everything up.
//! - **Transitions.** A new mood never cuts: the layers' levels and the tempo glide to it over a bar or two, and its
//!   scale and chords take over on the next bar line, where every part changes together. Going to no mood at all fades
//!   every layer out and the band rests.
//!
//! No IO and no clock but the samples asked for: [`Band::render`] is the whole of it, so a test plays it offline.
use std::f32::consts::TAU;

/// Samples a second the band is rendered at.
pub const SR: f32 = 48_000.0;
/// Steps (sixteenths) a bar.
const STEPS: usize = 16;
/// The key's root: A2.
const ROOT: f32 = 110.0;
/// The most voices sounding at once (the oldest gives way).
const VOICES: usize = 40;

/// A mood: its tempo (quarter notes a minute), its scale (semitones over the key) by day and by night, its chords (a
/// root a bar, as steps of the scale) and how loud each layer plays (0: silent).
#[derive(Clone, Debug, PartialEq)]
pub struct Mood {
    pub tempo: f32,
    pub scale: Vec<i32>,
    pub night: Vec<i32>,
    pub chords: Vec<i32>,
    pub pad: f32,
    pub bass: f32,
    pub arp: f32,
    pub lead: f32,
    pub bell: f32,
    pub drums: f32,
}

impl Mood {
    fn levels(&self) -> [f32; 6] {
        [self.pad, self.bass, self.arp, self.lead, self.bell, self.drums]
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Pad,
    Bass,
    Pluck,
    Bell,
    Kick,
    Drum,
    Hat,
}

#[derive(Clone, Copy)]
struct Voice {
    kind: Kind,
    freq: f32,
    amp: f32,
    pan: f32,
    /// Samples played, and how many it holds before its release.
    t: u32,
    hold: u32,
    phase: f32,
}

impl Voice {
    /// The voice's next sample and whether it has ended.
    fn next(&mut self, noise: f32) -> (f32, bool) {
        let t = self.t as f32 / SR;
        self.t += 1;
        let w = TAU * self.freq * t;
        match self.kind {
            Kind::Pad => {
                // A slow swell and a slow release: three soft partials, the upper two a hair out of tune so it moves.
                let hold = self.hold as f32 / SR;
                let env = (t / 1.2).min(1.0) * if t > hold { (-(t - hold) * 1.4).exp() } else { 1.0 };
                let s = w.sin() + 0.42 * (w * 2.004).sin() + 0.18 * (w * 2.997).sin();
                (0.33 * env * s * (1.0 + 0.08 * (TAU * 0.21 * t + self.pan).sin()), t > hold + 4.0)
            }
            Kind::Bass => {
                let env = (t * 120.0).min(1.0) * (-t * 2.6).exp();
                (env * (w.sin() + 0.28 * (2.0 * w).sin()), t > 2.0)
            }
            Kind::Pluck => {
                // A plucked string: harmonics that die the faster the higher they are.
                let mut s = 0.0;
                for h in 1..=6 {
                    let h = h as f32;
                    s += (w * h * (1.0 + 0.0006 * h * h)).sin() / h * (-t * (1.6 + 1.3 * h)).exp();
                }
                ((t * 400.0).min(1.0) * 0.7 * s, t > 3.0)
            }
            Kind::Bell => {
                let s = (w + 1.6 * (-t * 2.2).exp() * (w * 3.5).sin()).sin();
                ((t * 300.0).min(1.0) * (-t * 1.1).exp() * 0.6 * s, t > 5.0)
            }
            Kind::Kick => {
                self.phase += TAU * (46.0 + 90.0 * (-t * 32.0).exp()) / SR;
                ((-t * 9.0).exp() * self.phase.sin(), t > 0.6)
            }
            Kind::Drum => {
                self.phase += TAU * (self.freq + 50.0 * (-t * 22.0).exp()) / SR;
                ((-t * 6.5).exp() * self.phase.sin() + 0.25 * (-t * 70.0).exp() * noise, t > 0.9)
            }
            Kind::Hat => {
                // Noise with its lows taken out (the difference of two samples), very short.
                let s = noise - self.phase;
                self.phase = noise;
                (0.5 * (-t * 55.0).exp() * s, t > 0.12)
            }
        }
    }
}

/// A room: four combs and two all-passes a side (Schroeder's), the right side's a little longer.
struct Room {
    combs: [[Vec<f32>; 4]; 2],
    damp: [[f32; 4]; 2],
    passes: [[Vec<f32>; 2]; 2],
    at: usize,
}

impl Room {
    fn new() -> Self {
        let line = |n: usize| vec![0.0; n]; // alloc: the room's delay lines, made once
        let side = |d: usize| [line(1687 + d), line(1601 + d), line(2053 + d), line(2251 + d)];
        let pass = |d: usize| [line(347 + d), line(113 + d)];
        Self { combs: [side(0), side(37)], damp: [[0.0; 4]; 2], passes: [pass(0), pass(11)], at: 0 }
    }

    fn next(&mut self, x: f32) -> [f32; 2] {
        self.at = self.at.wrapping_add(1);
        let mut out = [0.0; 2];
        for (side, o) in out.iter_mut().enumerate() {
            let mut s = 0.0;
            for (k, line) in self.combs[side].iter_mut().enumerate() {
                let i = self.at % line.len();
                let y = line[i];
                self.damp[side][k] = y * 0.72 + self.damp[side][k] * 0.28;
                line[i] = x + self.damp[side][k] * 0.86;
                s += y;
            }
            for line in &mut self.passes[side] {
                let i = self.at % line.len();
                let y = line[i];
                line[i] = s + y * 0.5;
                s = y - s * 0.5;
            }
            *o = s * 0.2;
        }
        out
    }
}

/// The band: its voices, its room, where it is in the bar, and the mood it plays and the one it is moving to.
pub struct Band {
    voices: Vec<Voice>,
    room: Room,
    rng: u64,
    /// The mood being played (its scale and chords change on a bar line; None: nothing yet) and the next.
    mood: Option<Mood>,
    next: Option<Mood>,
    /// The layers' levels now (pad, bass, arp, lead, bell, drums) and the tempo now.
    levels: [f32; 6],
    tempo: f32,
    night: bool,
    step: usize,
    bar: usize,
    /// Samples until the next step.
    wait: f32,
    /// The lead's motif: two bars of steps, each a step of the scale or a rest.
    motif: [Option<i8>; 2 * STEPS],
}

impl Default for Band {
    fn default() -> Self {
        Self::new(0x2545_f491_4f6c_dd1d)
    }
}

impl Band {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            voices: Vec::with_capacity(VOICES), // alloc: the pool, made once
            room: Room::new(),
            rng: seed | 1,
            mood: None,
            next: None,
            levels: [0.0; 6],
            tempo: 72.0,
            night: false,
            step: 0,
            bar: 0,
            wait: 0.0,
            motif: [None; 2 * STEPS],
        }
    }

    /// Play toward `mood` (None: fade out and rest), by `night` its night scale.
    pub fn set(&mut self, mood: Option<&Mood>, night: bool) {
        self.night = night;
        if self.next.as_ref() != mood {
            self.next = mood.cloned();
        }
    }

    /// Whether anything sounds or is about to: false once it has faded out and rung out.
    #[must_use]
    pub fn playing(&self) -> bool {
        self.next.is_some() || !self.voices.is_empty() || self.levels.iter().any(|l| *l > 0.003)
    }

    fn rand(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 40) as f32 / (1u64 << 24) as f32
    }

    fn play(&mut self, kind: Kind, freq: f32, amp: f32, pan: f32, hold: f32) {
        if amp < 0.002 {
            return;
        }
        if self.voices.len() >= VOICES {
            self.voices.remove(0);
        }
        self.voices.push(Voice { kind, freq, amp, pan, t: 0, hold: (hold * SR) as u32, phase: 0.0 });
    }

    /// The pitch of step `degree` of `scale` (past its end: the octaves above; below 0: those below), `octave` up.
    fn pitch(scale: &[i32], degree: i32, octave: i32) -> f32 {
        let n = scale.len().max(1) as i32;
        let semis = scale.get(degree.rem_euclid(n) as usize).copied().unwrap_or(0) + 12 * (degree.div_euclid(n) + octave);
        ROOT * 2f32.powf(semis as f32 / 12.0)
    }

    /// A new motif for the lead: mostly rests, notes on the stronger steps, each a step or two from the last.
    fn compose(&mut self) {
        let mut at = 2i32;
        for i in 0..self.motif.len() {
            let strong = i % 4 == 0;
            let chance = if strong { 0.62 } else if i % 2 == 0 { 0.3 } else { 0.08 };
            self.motif[i] = if self.rand() < chance {
                at = (at + [-2, -1, -1, 1, 1, 2, 3][(self.rand() * 7.0) as usize % 7]).clamp(-2, 9);
                Some(at as i8)
            } else {
                None
            };
        }
        // It ends at home.
        self.motif[2 * STEPS - 4] = Some(0);
    }

    /// One step of the bar: what each layer plays on it.
    fn tick(&mut self) {
        if self.step == 0 {
            // The bar line: the next mood's scale and chords take over here, every part at once.
            if self.next != self.mood && (self.next.is_some() || self.levels.iter().all(|l| *l < 0.01)) {
                let fresh = self.mood.is_none();
                self.mood.clone_from(&self.next);
                if fresh {
                    self.bar = 0;
                }
            }
            if self.bar % 8 == 0 {
                self.compose();
            } else if self.bar % 2 == 0 {
                // The motif again, one note changed.
                let i = (self.rand() * self.motif.len() as f32) as usize % self.motif.len();
                if let Some(d) = self.motif[i] {
                    self.motif[i] = Some((i32::from(d) + if self.rand() < 0.5 { 1 } else { -1 }).clamp(-2, 9) as i8);
                }
            }
        }
        // Toward the mood asked for: levels and tempo glide (about a bar), never jump.
        let want = self.next.as_ref().map_or([0.0; 6], Mood::levels);
        for (l, w) in self.levels.iter_mut().zip(want) {
            *l += (w - *l) * 0.09;
        }
        if let Some(n) = &self.next {
            self.tempo += (n.tempo - self.tempo) * 0.06;
        }
        let Some(mood) = self.mood.clone() else { return };
        let scale = if self.night && !mood.night.is_empty() { &mood.night } else { &mood.scale };
        let [pad, bass, arp, lead, bell, drums] = self.levels;
        let (step, bar) = (self.step, self.bar);
        let root = mood.chords.get(bar % mood.chords.len().max(1)).copied().unwrap_or(0);
        let beat = 60.0 / self.tempo.max(30.0);
        // The chord: the root and the notes two and four steps of the scale over it.
        let chord = [root, root + 2, root + 4];
        if step == 0 {
            for (k, d) in chord.iter().enumerate() {
                self.play(Kind::Pad, Self::pitch(scale, *d, 1), 0.11 * pad, [-0.5, 0.0, 0.5][k], beat * 4.0);
            }
        }
        // The bass: the root on the bar, again on its middle once it is up, and pushed notes when it drives.
        let on_bass = step == 0 || (step == 8 && bass > 0.3) || (matches!(step, 6 | 14) && bass > 0.65);
        if on_bass {
            self.play(Kind::Bass, Self::pitch(scale, root, 0), 0.3 * bass, 0.0, 0.0);
        }
        // The arpeggio: up through the chord over two octaves, in eighths, in sixteenths when it is high.
        if arp > 0.02 && (step % 2 == 0 || arp > 0.65) {
            let n = if arp > 0.65 { step } else { step / 2 };
            let d = chord[n % 3] + (scale.len() as i32) * ((n / 3) % 2) as i32;
            let accent = if step % 4 == 0 { 1.0 } else { 0.62 };
            self.play(Kind::Pluck, Self::pitch(scale, d, 2), 0.13 * arp * accent, if n % 2 == 0 { -0.35 } else { 0.35 }, 0.0);
        }
        // The lead: its motif, over the chord's root.
        if lead > 0.02
            && let Some(d) = self.motif[(bar % 2) * STEPS + step]
        {
            let pan = self.rand() * 0.4 - 0.2;
            self.play(Kind::Pluck, Self::pitch(scale, root + i32::from(d), 2), 0.2 * lead, pan, 0.0);
        }
        // Bells: now and then, high, on a beat.
        if bell > 0.02 && step % 4 == 0 && self.rand() < 0.22 {
            let d = chord[(self.rand() * 3.0) as usize % 3];
            let pan = self.rand() * 1.2 - 0.6;
            self.play(Kind::Bell, Self::pitch(scale, d, 3), 0.1 * bell, pan, 0.0);
        }
        // Drums: the kick on the bar and its middle, the low drum on the beats between, a hat on the off eighths; fuller
        // as the level rises.
        if drums > 0.02 {
            if step == 0 || step == 8 || (drums > 0.7 && matches!(step, 3 | 11)) {
                self.play(Kind::Kick, 0.0, 0.5 * drums, 0.0, 0.0);
            }
            if matches!(step, 4 | 12) || (drums > 0.7 && step == 14) {
                self.play(Kind::Drum, 96.0, 0.33 * drums, 0.2, 0.0);
            }
            if step % 4 == 2 || (drums > 0.5 && step % 2 == 1) {
                self.play(Kind::Hat, 0.0, 0.09 * drums, -0.3, 0.0);
            }
        }
    }

    /// Add the band's next `out.len()` frames to `out`.
    pub fn render(&mut self, out: &mut [[f32; 2]]) {
        if !self.playing() {
            return;
        }
        for frame in out {
            if self.wait <= 0.0 {
                self.tick();
                self.wait += 60.0 / self.tempo.max(30.0) / 4.0 * SR;
                self.step += 1;
                if self.step == STEPS {
                    self.step = 0;
                    self.bar += 1;
                }
            }
            self.wait -= 1.0;
            let noise = self.rand() * 2.0 - 1.0;
            let (mut dry, mut send) = ([0.0f32; 2], 0.0f32);
            self.voices.retain_mut(|v| {
                let (s, ended) = v.next(noise);
                let s = s * v.amp;
                dry[0] += s * (0.5 - 0.4 * v.pan);
                dry[1] += s * (0.5 + 0.4 * v.pan);
                // The drums stay dry; everything else sits in the room.
                send += s * if matches!(v.kind, Kind::Kick | Kind::Hat) { 0.03 } else { 0.3 };
                !ended
            });
            let wet = self.room.next(send);
            frame[0] += dry[0] + wet[0];
            frame[1] += dry[1] + wet[1];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mood(tempo: f32, levels: [f32; 6]) -> Mood {
        let [pad, bass, arp, lead, bell, drums] = levels;
        Mood { tempo, scale: vec![0, 2, 4, 7, 9], night: vec![0, 2, 3, 7, 8], chords: vec![0, 3, 2, 4], pad, bass, arp, lead, bell, drums }
    }

    fn seconds(b: &mut Band, s: f32) -> (f32, f32) {
        let mut out = vec![[0.0f32; 2]; (s * SR) as usize];
        b.render(&mut out);
        let peak = out.iter().flatten().fold(0.0f32, |p, x| p.max(x.abs()));
        let rms = (out.iter().flatten().map(|x| x * x).sum::<f32>() / (out.len() * 2) as f32).sqrt();
        (peak, rms)
    }

    /// The band plays a mood under full scale, a fuller mood is louder, a change of mood glides (no step in level
    /// between one second and the next), and with no mood it fades out, rings out and rests.
    #[test]
    fn the_band_follows_its_mood_and_rests() {
        let (calm, battle) = (mood(66.0, [0.5, 0.0, 0.0, 0.35, 0.3, 0.0]), mood(132.0, [0.3, 0.8, 0.9, 0.6, 0.0, 0.9]));
        let mut b = Band::new(7);
        assert!(!b.playing());
        b.set(Some(&calm), false);
        let (peak, quiet) = seconds(&mut b, 12.0);
        assert!(peak > 0.01 && peak < 0.9, "calm peaks at {peak}");
        b.set(Some(&battle), false);
        let (_, first) = seconds(&mut b, 1.0);
        assert!(first < quiet * 6.0, "the change jumped: {quiet} to {first} within a second");
        let (peak, loud) = seconds(&mut b, 12.0);
        assert!(peak < 0.98 && loud > quiet * 1.5, "battle peaks at {peak}, rms {loud} over calm's {quiet}");
        b.set(None, false);
        seconds(&mut b, 30.0);
        assert!(!b.playing(), "it never came to rest");
        let (peak, _) = seconds(&mut b, 1.0);
        assert!(peak == 0.0, "at rest it still sounds: {peak}");
    }
}
