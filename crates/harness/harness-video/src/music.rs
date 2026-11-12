//! Cutting to music. A film is edited in musical time: [`analyse`] finds a track's beat grid (tempo, beats, bars,
//! sections, impacts), `beats.json` beside the scene keeps it, and [`constants`] turns it into `m_*` constants a scene
//! places its cuts by (`$m_b15_1`: bar 15, beat 1; `$m_drop`), each snapped to a frame. Another track, or another cut of
//! the track, re-times the film.
//!
//! The track is a file the owner supplies with the rights to use it (`music.wav` beside the scene; never a ripped
//! recording). Until there is one, [`temp_score`] synthesises a stand-in with an unmistakable pulse, a build and a
//! drop, so the edit has something real to cut against; a render made with it is labelled temp music.
//!
//! Everything here is plain arithmetic on the samples in order: the same track in gives the same grid out.
use serde::{Deserialize, Serialize};

use crate::loud::{RATE, fft};

/// A track's grid, as `beats.json` holds it. Times are seconds from the track's start.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Grid {
    /// What was analysed, in words (a file's name, or "temp score").
    pub source: String,
    pub bpm: f64,
    /// Every beat.
    pub beats: Vec<f64>,
    /// The index into `beats` of each bar's first beat (four beats a bar).
    pub bars: Vec<usize>,
    pub sections: Vec<Section>,
    /// The strongest onsets, strongest first.
    pub impacts: Vec<Impact>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Section {
    /// `intro`, `build`, `drop`, `break` or `outro`.
    pub label: String,
    /// The bar it starts at, from 1.
    pub bar: usize,
    pub at: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Impact {
    pub at: f64,
    /// 0 to 1, of the strongest.
    pub strength: f64,
}

/// Samples between analysis frames, and a frame's window.
const HOP: usize = 512;
const WINDOW: usize = 1024;

/// The onset envelopes of `mix`: for each frame, the spectral flux (how much louder each band got since the frame
/// before, summed) over the whole spectrum and over the low band alone (under 150 Hz: the kick).
fn flux(mix: &[[f32; 2]]) -> (Vec<f64>, Vec<f64>) {
    let frames = mix.len().saturating_sub(WINDOW) / HOP + 1;
    let low_bins = (150.0 * WINDOW as f64 / f64::from(RATE)).ceil() as usize;
    let mut before = vec![0.0f64; WINDOW / 2];
    let (mut all, mut low) = (Vec::with_capacity(frames), Vec::with_capacity(frames));
    for f in 0..frames {
        let mut re = [0.0f64; WINDOW];
        let mut im = [0.0f64; WINDOW];
        for (i, r) in re.iter_mut().enumerate() {
            let s = mix.get(f * HOP + i).map_or(0.0, |s| f64::from(s[0] + s[1]) * 0.5);
            *r = s * (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / WINDOW as f64).cos());
        }
        fft(&mut re, &mut im);
        let (mut sum, mut sum_low) = (0.0, 0.0);
        for (bin, was) in before.iter_mut().enumerate() {
            // Log-compressed magnitude, so a quiet passage's onsets count too.
            let now = (1.0 + 100.0 * (re[bin] * re[bin] + im[bin] * im[bin]).sqrt()).ln();
            let rise = (now - *was).max(0.0);
            sum += rise;
            if bin <= low_bins {
                sum_low += rise;
            }
            *was = now;
        }
        all.push(sum);
        low.push(sum_low);
    }
    (all, low)
}

/// The grid of `mix` (stereo at [`RATE`]), named `source`. Err for a track too short or too quiet to have one.
pub fn analyse(mix: &[[f32; 2]], source: &str) -> Result<Grid, String> {
    let (onset, low) = flux(mix);
    let seconds_a_frame = HOP as f64 / f64::from(RATE);
    if onset.len() < (8.0 / seconds_a_frame) as usize {
        return Err(format!("{source}: under 8 seconds: too short to find a tempo in"));
    }
    let mean = onset.iter().sum::<f64>() / onset.len() as f64;
    if !(mean > 0.0) {
        return Err(format!("{source}: silent: nothing to find a tempo in"));
    }
    let env: Vec<f64> = onset.iter().map(|v| (v - mean).max(0.0)).collect();
    // Tempo: the lag at which the envelope best matches itself, between 70 and 180 beats a minute, leaning a little
    // toward 110 so a track is not read at half or twice its speed.
    let lag_of = |bpm: f64| 60.0 / bpm / seconds_a_frame;
    let (lo, hi) = (lag_of(180.0).floor() as usize, lag_of(70.0).ceil() as usize);
    let mut best = (0.0, lo);
    for lag in lo..=hi {
        let sum: f64 = env.iter().zip(&env[lag..]).map(|(a, b)| a * b).sum();
        let bpm = 60.0 / (lag as f64 * seconds_a_frame);
        let lean = (-0.5 * ((bpm / 110.0).log2() / 0.9).powi(2)).exp();
        if sum * lean > best.0 {
            best = (sum * lean, lag);
        }
    }
    let period = best.1 as f64;
    // Beats: dynamic programming (Ellis's tracker). A beat's score is its onset strength plus the best score a period
    // before it, less a penalty for the gap being off the period; the best chain is read back from the end.
    let n = env.len();
    let mut score = env.clone();
    let mut back = vec![usize::MAX; n];
    let peak = env.iter().copied().fold(0.0, f64::max);
    let tight = 100.0 * peak / 8.0;
    for i in 0..n {
        let (from, to) = ((i as f64 - 2.0 * period).max(0.0) as usize, (i as f64 - period / 2.0).max(0.0) as usize);
        let mut top = (f64::NEG_INFINITY, usize::MAX);
        for j in from..to {
            let gap = ((i - j) as f64 / period).ln();
            let s = score[j] - tight * gap * gap;
            if s > top.0 {
                top = (s, j);
            }
        }
        if top.1 != usize::MAX {
            score[i] = env[i] + top.0;
            back[i] = top.1;
        }
    }
    let tail = n - (period as usize).min(n);
    let mut at = (tail..n).max_by(|a, b| score[*a].total_cmp(&score[*b])).unwrap_or(n - 1);
    let mut frames = vec![at];
    while back[at] != usize::MAX {
        at = back[at];
        frames.push(at);
    }
    frames.reverse();
    // The grid is even: one tempo, fitted to the tracked beats by least squares, so a beat the tracker put a frame
    // early or late does not move a cut. Extended back to the track's start and on to its end.
    let k = frames.len() as f64;
    let (sx, sy) = ((k - 1.0) * k / 2.0, frames.iter().map(|f| *f as f64).sum::<f64>());
    let sxx = (k - 1.0) * k * (2.0 * k - 1.0) / 6.0;
    let sxy: f64 = frames.iter().enumerate().map(|(i, f)| i as f64 * *f as f64).sum();
    let step = (k * sxy - sx * sy) / (k * sxx - sx * sx);
    let first = (sy - step * sx) / k;
    let (step, mut first) = (step * seconds_a_frame, first * seconds_a_frame);
    // The window's middle is where its energy is.
    first += WINDOW as f64 / 2.0 / f64::from(RATE) - seconds_a_frame;
    while first - step >= -0.02 {
        first -= step;
    }
    let length = mix.len() as f64 / f64::from(RATE);
    let beats: Vec<f64> = (0..).map(|i| (first + step * f64::from(i)).max(0.0)).take_while(|t| *t < length - 0.05).collect();
    // Downbeats: of the four ways to count, the one whose "one" has the most low-band onset (where the kick lands).
    let low_at = |t: f64| {
        let f = (t / seconds_a_frame) as usize;
        (f.saturating_sub(2)..(f + 3).min(low.len())).map(|i| low[i]).fold(0.0, f64::max)
    };
    let phase = (0..4)
        .max_by(|a, b| {
            let sum = |p: usize| beats.iter().skip(p).step_by(4).map(|t| low_at(*t)).sum::<f64>();
            sum(*a).total_cmp(&sum(*b))
        })
        .unwrap_or(0);
    let bars: Vec<usize> = (phase..beats.len()).step_by(4).collect();
    // Sections: each bar's energy against the loudest bar's. The drop is the bar that rises most over the one before;
    // before it a quiet bar is intro and the rest build; after it a quiet bar is a break, or the outro if nothing
    // loud follows.
    let energy: Vec<f64> = bars
        .iter()
        .map(|b| {
            let (from, to) = ((beats[*b] * f64::from(RATE)) as usize, ((beats[*b] + 4.0 * step) * f64::from(RATE)) as usize);
            let part = &mix[from.min(mix.len())..to.min(mix.len())];
            (part.iter().map(|s| f64::from(s[0] * s[0] + s[1] * s[1])).sum::<f64>() / part.len().max(1) as f64).sqrt()
        })
        .collect();
    let loudest = energy.iter().copied().fold(0.0, f64::max).max(1e-9);
    let drop = (1..energy.len()).max_by(|a, b| (energy[*a] - energy[*a - 1]).total_cmp(&(energy[*b] - energy[*b - 1]))).unwrap_or(0);
    let label = |bar: usize| {
        let quiet = energy[bar] < 0.4 * loudest;
        if bar < drop {
            if quiet && energy[..=bar].iter().all(|e| *e < 0.4 * loudest) { "intro" } else { "build" }
        } else if !quiet {
            "drop"
        } else if energy[bar..].iter().all(|e| *e < 0.4 * loudest) {
            "outro"
        } else {
            "break"
        }
    };
    let mut sections: Vec<Section> = Vec::new();
    for bar in 0..bars.len() {
        let l = label(bar);
        if sections.last().is_none_or(|s| s.label != l) {
            sections.push(Section { label: l.to_owned(), bar: bar + 1, at: beats[bars[bar]] });
        }
    }
    // Impacts: the strongest onsets, at least a second apart.
    let mut order: Vec<usize> = (0..env.len()).collect();
    order.sort_by(|a, b| env[*b].total_cmp(&env[*a]).then(a.cmp(b)));
    let mut impacts: Vec<Impact> = Vec::new();
    for f in order {
        let at = f as f64 * seconds_a_frame;
        if impacts.len() < 8 && impacts.iter().all(|i| (i.at - at).abs() >= 1.0) {
            impacts.push(Impact { at, strength: env[f] / peak.max(1e-9) });
        }
    }
    Ok(Grid { source: source.to_owned(), bpm: 60.0 / step, beats, bars, sections, impacts })
}

/// A time snapped to a sixtieth of a second: a frame of every preset.
fn frame(t: f64) -> f64 {
    (t * 60.0).round() / 60.0
}

impl Grid {
    /// The `m_*` constants, as flecs script: `m_bpm`; `m_beat` (seconds a beat); `m_bars`; `m_b<bar>_<beat>` for every
    /// beat of every bar (from 1), and `m_b<bar>_<beat>h` for the half beat after it; `m_<label>` where each kind of
    /// section first starts, and `m_<label>_bar`; `m_impact_<k>` (from 1, strongest first). Every time is snapped to
    /// a sixtieth of a second.
    pub fn constants(&self) -> String {
        let mut out = String::from("// Generated by harness-video (src/music.rs) from the scene's beats.json.\n");
        let step = 60.0 / self.bpm;
        out.push_str(&format!("export const m_bpm: f32 = {}\nexport const m_beat: f32 = {step}\nexport const m_bars: f32 = {}\n", self.bpm, self.bars.len()));
        for (bar, first) in self.bars.iter().enumerate() {
            for beat in 0..4 {
                if let Some(t) = self.beats.get(first + beat) {
                    out.push_str(&format!("export const m_b{}_{}: f32 = {}\n", bar + 1, beat + 1, frame(*t)));
                    out.push_str(&format!("export const m_b{}_{}h: f32 = {}\n", bar + 1, beat + 1, frame(*t + step / 2.0)));
                }
            }
        }
        let mut seen: Vec<&str> = Vec::new();
        for s in &self.sections {
            if !seen.contains(&s.label.as_str()) {
                seen.push(&s.label);
                out.push_str(&format!("export const m_{}: f32 = {}\nexport const m_{}_bar: f32 = {}\n", s.label, frame(s.at), s.label, s.bar));
            }
        }
        for (k, i) in self.impacts.iter().enumerate() {
            out.push_str(&format!("export const m_impact_{}: f32 = {}\n", k + 1, frame(i.at)));
        }
        out
    }

    /// Where `t` is in the music: the nearest beat or half beat, as `bar 9 beat 1` (`beat 2.5` for a half), and how
    /// far `t` is from it, seconds.
    pub fn position(&self, t: f64) -> (String, f64) {
        let step = 60.0 / self.bpm;
        let first = self.bars.first().and_then(|b| self.beats.get(*b)).copied().unwrap_or(0.0);
        let halves = ((t - first) / (step / 2.0)).round();
        let off = t - (first + halves * step / 2.0);
        let (bar, half) = (halves.div_euclid(8.0) as i64 + 1, halves.rem_euclid(8.0));
        let beat = 1.0 + half / 2.0;
        let section = self.sections.iter().rev().find(|s| s.at <= t + 0.01).map_or(String::new(), |s| format!(" ({})", s.label));
        (format!("bar {bar} beat {beat}{section}"), off)
    }

    /// A picture of the track with its grid on it, for a person to check the grid by: the waveform, a faint line at
    /// every beat, a bright one at every bar, the accent at every section's start. `(w, h)` RGBA8.
    pub fn picture(&self, mix: &[[f32; 2]], (w, h): (usize, usize)) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        let length = mix.len() as f64 / f64::from(RATE);
        let per = (mix.len() / w).max(1);
        for x in 0..w {
            let chunk = &mix[(x * per).min(mix.len())..((x + 1) * per).min(mix.len())];
            let peak = chunk.iter().flat_map(|s| [s[0].abs(), s[1].abs()]).fold(0.0f32, f32::max).min(1.0);
            let half = (peak * (h / 2) as f32) as usize;
            for y in 0..h {
                let c: [u8; 3] = if y.abs_diff(h / 2) <= half { [150, 158, 175] } else { [12, 13, 16] };
                px[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
            }
        }
        let mut line = |t: f64, c: [u8; 3], from: usize| {
            let x = ((t / length) * w as f64) as usize;
            if x < w {
                for y in from..h {
                    px[(y * w + x) * 4..(y * w + x) * 4 + 3].copy_from_slice(&c);
                }
            }
        };
        for t in &self.beats {
            line(*t, [70, 74, 84], h * 3 / 4);
        }
        for b in &self.bars {
            line(self.beats[*b], [235, 238, 245], h / 2);
        }
        for s in &self.sections {
            line(s.at, [255, 149, 50], 0);
        }
        px
    }
}

/// A WAV file's samples as stereo float at [`RATE`]: PCM of 16, 24 or 32 bits or 32-bit float, one or two channels;
/// another rate is resampled (linear). Err names what the file is when it is none of those.
pub fn read_wav(bytes: &[u8], name: &str) -> Result<Vec<[f32; 2]>, String> {
    let bad = |why: &str| format!("{name}: {why} (a WAV of 16, 24 or 32-bit PCM or 32-bit float, mono or stereo, is read; convert anything else to that)");
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(bad("not a RIFF WAVE file"));
    }
    let (mut at, mut format, mut data) = (12usize, None, None);
    while at + 8 <= bytes.len() {
        let size = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]]) as usize;
        let body = bytes.get(at + 8..(at + 8 + size).min(bytes.len())).ok_or_else(|| bad("a chunk runs past the end"))?;
        match &bytes[at..at + 4] {
            b"fmt " if body.len() >= 16 => {
                let tag = u16::from_le_bytes([body[0], body[1]]);
                // An extensible format says its real kind in the sub-format's first two bytes.
                let tag = if tag == 0xfffe && body.len() >= 26 { u16::from_le_bytes([body[24], body[25]]) } else { tag };
                format = Some((
                    tag,
                    u16::from_le_bytes([body[2], body[3]]) as usize,
                    u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
                    u16::from_le_bytes([body[14], body[15]]) as usize,
                ));
            }
            b"data" => data = Some(body),
            _ => {}
        }
        at += 8 + size + size % 2;
    }
    let (tag, channels, rate, bits) = format.ok_or_else(|| bad("no fmt chunk"))?;
    let data = data.ok_or_else(|| bad("no data chunk"))?;
    if !(1..=2).contains(&channels) || rate == 0 {
        return Err(bad(&format!("{channels} channels at {rate} Hz")));
    }
    let sample = |b: &[u8]| -> Option<f32> {
        Some(match (tag, bits) {
            (1, 16) => f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0,
            (1, 24) => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
            (1, 32) => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
            (3, 32) => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            _ => return None,
        })
    };
    let width = bits / 8;
    if sample(&[0; 4]).is_none() {
        return Err(bad(&format!("format tag {tag} at {bits} bits")));
    }
    let frames: Vec<[f32; 2]> = data
        .chunks_exact(width * channels)
        .map(|f| {
            let l = sample(&f[..width]).unwrap_or(0.0);
            let r = if channels == 2 { sample(&f[width..]).unwrap_or(0.0) } else { l };
            [l, r]
        })
        .collect();
    if rate == RATE {
        return Ok(frames);
    }
    let k = f64::from(rate) / f64::from(RATE);
    let n = (frames.len() as f64 / k) as usize;
    Ok((0..n)
        .map(|i| {
            let x = i as f64 * k;
            let (a, f) = (x as usize, (x - x.floor()) as f32);
            let (p, q) = (frames[a], frames[(a + 1).min(frames.len() - 1)]);
            [p[0] + (q[0] - p[0]) * f, p[1] + (q[1] - p[1]) * f]
        })
        .collect())
}

/// The temp score: a stand-in track to cut against until the owner supplies one. `bars` bars of four at `bpm`, in
/// trailer shape: a cold open of single low hits (bars 1 and 2), a groove that thickens (to the bar before
/// `drop_bar`, whose last beat is silent), the drop (everything, a crash on its downbeat) for `drop_bars` bars, a
/// lighter groove, a break (the bar before the last two: silence, then one hit on its third beat), and the last
/// downbeat held, with one more hit a bar later (the button). Its notes are its own: a syncopated kick, a backbeat,
/// a bass on A, and a five-note figure. `seed` fixes its noise.
pub fn temp_score(bpm: f64, bars: usize, drop_bar: usize, drop_bars: usize, seed: u64) -> Vec<[f32; 2]> {
    let sr = f64::from(RATE);
    let beat = 60.0 / bpm;
    let n = (bars as f64 * 4.0 * beat * sr).round() as usize;
    let mut out = vec![[0.0f32; 2]; n];
    let mut state = seed | 1;
    let mut noise = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    };
    // One sound: `f(seconds since its start)` for `dur` seconds from `at`, at `pan` (-1 to 1).
    let mut add = |out: &mut Vec<[f32; 2]>, at: f64, dur: f64, pan: f32, f: &mut dyn FnMut(f32) -> f32| {
        let from = (at * sr).round() as usize;
        let (l, r) = ((0.5 * (1.0 - pan)).sqrt(), (0.5 * (1.0 + pan)).sqrt());
        for i in 0..(dur * sr) as usize {
            let Some(o) = out.get_mut(from + i) else { break };
            let v = f(i as f32 / RATE as f32);
            o[0] += v * l;
            o[1] += v * r;
        }
    };
    let tau = std::f32::consts::TAU;
    let kick = |amp: f32| move |t: f32| amp * (-t * 9.0).exp() * (tau * (48.0 * t + 14.0 * (1.0 - (-t * 30.0).exp()))).sin();
    // A low hit for a big cut: a kick under a slow, rough fifth.
    let braam = |amp: f32| move |t: f32| {
        let saw = |f: f32| 2.0 * (t * f - (t * f + 0.5).floor());
        amp * ((-t * 2.2).exp() * 0.5 * (saw(55.0) + saw(55.4) + 0.6 * saw(82.4)) * (1.0 - (-t * 40.0).exp()) + (-t * 7.0).exp() * (tau * (45.0 * t + 12.0 * (1.0 - (-t * 25.0).exp()))).sin())
    };
    let last_groove = bars.saturating_sub(3);
    for bar in 0..bars {
        let at = |b: f64| (bar as f64 * 4.0 + b) * beat;
        let n1 = bar + 1;
        let (in_drop, lighter) = (n1 >= drop_bar && n1 < drop_bar + drop_bars, n1 >= drop_bar + drop_bars && n1 <= last_groove);
        if n1 <= 2 {
            // The cold open: a hit a beat in bar 1, one on the downbeat of bar 2 that rings.
            for b in 0..if n1 == 1 { 4 } else { 1 } {
                add(&mut out, at(f64::from(b)), 1.6, 0.0, &mut braam(0.5));
            }
        } else if n1 < drop_bar || in_drop || lighter {
            let level = if in_drop { 1.0 } else if lighter { 0.7 } else { 0.45 + 0.4 * (n1 - 3) as f32 / (drop_bar - 3).max(1) as f32 };
            let silent_tail = n1 + 1 == drop_bar;
            // Kick: one, the and of two, three (and four in the drop). Backbeat on two and four.
            for b in [0.0, 1.5, 2.0].into_iter().chain(in_drop.then_some(3.5)) {
                if !(silent_tail && b >= 3.0) {
                    add(&mut out, at(b), 0.5, 0.0, &mut kick(0.8 * level));
                }
            }
            for b in [1.0, 3.0] {
                if !(silent_tail && b >= 3.0) {
                    let mut lp = 0.0f32;
                    add(&mut out, at(b), 0.22, 0.1, &mut |t| {
                        lp += 0.5 * (noise() - lp);
                        0.45 * level * (-t * 22.0).exp() * (lp + 0.5 * (tau * 190.0 * t).sin() * (-t * 30.0).exp())
                    });
                }
            }
            // Hats: eighths; sixteenths from the fourth bar of the groove and in the drop. In the two bars before the
            // drop a roll that doubles.
            let hats = if in_drop || n1 >= 7 { 16 } else { 8 };
            for h in 0..hats {
                let b = 4.0 * f64::from(h) / f64::from(hats);
                if silent_tail && b >= 3.0 {
                    continue;
                }
                let mut prev = 0.0f32;
                let amp = 0.10 * level * if h % (hats / 4) == 0 { 1.0 } else { 0.6 };
                add(&mut out, at(b), 0.05, if h % 2 == 0 { -0.3 } else { 0.3 }, &mut |t| {
                    let v = noise();
                    let hp = v - prev;
                    prev = v;
                    amp * (-t * 90.0).exp() * hp
                });
            }
            if n1 + 2 >= drop_bar && n1 < drop_bar {
                let hits = if silent_tail { 24 } else { 8 };
                for h in 0..hits {
                    let b = (if silent_tail { 3.0 } else { 4.0 }) * f64::from(h) / f64::from(hits);
                    let mut lp = 0.0f32;
                    let amp = 0.12 + 0.25 * h as f32 / hits as f32;
                    add(&mut out, at(b), 0.12, 0.0, &mut |t| {
                        lp += 0.6 * (noise() - lp);
                        amp * (-t * 35.0).exp() * lp
                    });
                }
            }
            // Bass on A, an eighth-note line that leans on the off-beats.
            for (i, semis) in [0, 0, 7, 0, 10, 0, 7, 12].into_iter().enumerate() {
                let b = i as f64 * 0.5;
                if silent_tail && b >= 3.0 {
                    continue;
                }
                let f = 55.0 * 2f32.powf(semis as f32 / 12.0);
                let amp = 0.28 * level * if i % 2 == 1 { 1.0 } else { 0.75 };
                add(&mut out, at(b), beat * 0.45, 0.0, &mut |t| amp * (1.0 - (-t * 200.0).exp()) * (-t * 5.0).exp() * ((tau * f * t).sin() + 0.3 * (tau * 2.0 * f * t).sin()));
            }
            // The figure: five notes of A minor pentatonic over two beats, from the groove's fifth bar; an octave up
            // and doubled in the drop.
            if n1 >= 7 || in_drop {
                for rep in 0..2 {
                    for (i, semis) in [12, 15, 19, 17, 24].into_iter().enumerate() {
                        let b = f64::from(rep) * 2.0 + [0.0, 0.75, 1.0, 1.5, 1.75][i];
                        if silent_tail && b >= 3.0 {
                            continue;
                        }
                        let f = 220.0 * 2f32.powf(semis as f32 / 12.0) * if in_drop { 2.0 } else { 1.0 };
                        let amp = 0.11 * level;
                        add(&mut out, at(b), 0.35, if i % 2 == 0 { -0.4 } else { 0.4 }, &mut |t| {
                            amp * (-t * 9.0).exp() * ((tau * f * t).sin() + 0.4 * (tau * 2.0 * f * t).sin() + 0.15 * (tau * 3.0 * f * t).sin())
                        });
                    }
                }
            }
            // A riser through the bar before the drop; a crash and a low hit on the drop's downbeat and on each
            // bar of the lighter groove.
            if silent_tail {
                let mut lp = 0.0f32;
                let len = (3.0 * beat) as f32;
                add(&mut out, at(0.0), 3.0 * beat, 0.0, &mut |t| {
                    let p = t / len;
                    lp += (0.02 + 0.6 * p * p) * (noise() - lp);
                    0.5 * p * p * lp + 0.12 * p * p * (tau * (220.0 + 660.0 * p * p) * t).sin()
                });
            }
            if n1 == drop_bar || lighter {
                let amp = if n1 == drop_bar { 1.0 } else { 0.5 };
                add(&mut out, at(0.0), 2.0, 0.0, &mut braam(0.6 * amp));
                let mut prev = 0.0f32;
                add(&mut out, at(0.0), 1.8, 0.0, &mut |t| {
                    let v = noise();
                    let hp = v - 0.7 * prev;
                    prev = v;
                    0.22 * amp * (-t * 2.4).exp() * hp
                });
            }
        } else if n1 + 2 == bars {
            // The break: silence, then one hit on three.
            add(&mut out, at(2.0), 1.4, 0.0, &mut braam(0.6));
        } else {
            // The last two bars: the final downbeat, held as a chord; a bar later, the button.
            add(&mut out, at(0.0), 2.2, 0.0, &mut braam(if n1 + 1 == bars { 0.8 } else { 0.7 }));
            if n1 + 1 == bars {
                let hold = (8.0 * beat) as f32;
                for (f, pan) in [(110.0, 0.0), (164.81, -0.4), (220.0, 0.4), (277.18, -0.2), (329.63, 0.2)] {
                    add(&mut out, at(0.0), 8.0 * beat, pan, &mut |t| 0.07 * (t / 0.05).min(1.0) * (1.0 - t / hold).max(0.0).powi(2) * ((tau * f * t).sin() + 0.5 * (tau * (f + 0.4) * t).sin()));
                }
            }
        }
    }
    // A soft clip, so the hits do not pass full scale.
    for s in &mut out {
        *s = s.map(|v| (v * 0.9).tanh());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refinement: the analysis finds the grid the temp score was written on. Its tempo to within half a beat a
    /// minute, every beat to within a frame and a half of where the score put it, the bars counted from the score's
    /// own first beat, and the drop at the bar the score dropped on. The control: a track with no pulse (noise at one
    /// level) has no drop where the score's is.
    #[test]
    fn the_grid_of_the_temp_score_is_found() {
        let (bpm, bars, drop) = (104.0, 26, 15);
        let score = temp_score(bpm, bars, drop, 5, 7);
        assert_eq!(score.len(), 60 * RATE as usize, "26 bars at 104 are a minute");
        let grid = analyse(&score, "temp score").unwrap_or_else(|e| panic!("{e}"));
        assert!((grid.bpm - bpm).abs() < 0.5, "tempo {}", grid.bpm);
        let step = 60.0 / bpm;
        for (i, t) in grid.beats.iter().enumerate() {
            let nearest = (t / step).round() * step;
            assert!((t - nearest).abs() < 0.025, "beat {i} at {t}, {} off the score's", t - nearest);
        }
        assert_eq!(grid.bars.first(), Some(&0), "bars are counted from the first beat");
        assert_eq!(grid.bars.len(), bars);
        let dropped = grid.sections.iter().find(|s| s.label == "drop").unwrap_or_else(|| panic!("no drop in {:?}", grid.sections));
        assert_eq!(dropped.bar, drop, "{:?}", grid.sections);
        assert!(grid.constants().contains("export const m_b15_1: f32 = "));
        let (said, off) = grid.position(14.0 * 4.0 * step);
        assert!(said.starts_with("bar 15 beat 1") && off.abs() < 0.02, "{said} {off}");
        // The same track twice is the same grid, to the bit.
        assert_eq!(grid, analyse(&temp_score(bpm, bars, drop, 5, 7), "temp score").unwrap_or_else(|e| panic!("{e}")));
        let mut state = 9u64;
        let flat: Vec<[f32; 2]> = (0..20 * RATE as usize)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let v = ((state >> 40) as f32 / (1u64 << 23) as f32 - 1.0) * 0.2;
                [v, v]
            })
            .collect();
        let none = analyse(&flat, "noise").unwrap_or_else(|e| panic!("{e}"));
        assert!(none.sections.iter().all(|s| s.label != "drop" || s.bar != drop), "{:?}", none.sections);
    }

    /// A WAV written and read back is the samples it was (16-bit, to its step), and a file that is not one is refused
    /// by name.
    #[test]
    fn a_wav_reads_back() {
        let mix: Vec<[f32; 2]> = (0..4800).map(|i| [(i as f32 * 0.01).sin() * 0.5, (i as f32 * 0.013).cos() * 0.25]).collect();
        let back = read_wav(&harness_native_blaze::sound::wav(RATE, &mix), "t.wav").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(back.len(), mix.len());
        assert!(mix.iter().zip(&back).all(|(a, b)| (a[0] - b[0]).abs() < 2e-4 && (a[1] - b[1]).abs() < 2e-4));
        assert!(read_wav(b"ID3\x04 not a wave file at all", "song.mp3").is_err_and(|e| e.starts_with("song.mp3: not a RIFF WAVE")));
    }
}
