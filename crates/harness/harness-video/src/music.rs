//! Cutting to music. A film is edited in musical time: [`analyse`] finds a track's beat grid (tempo, beats, bars,
//! sections, impacts), `beats.json` beside the scene keeps it, and [`constants`] turns it into `m_*` constants a scene
//! places its cuts by (`$m_b15_1`: bar 15, beat 1; `$m_drop`), each snapped to a frame. Another track, or another cut of
//! the track, re-times the film.
//!
//! The track is a file the owner supplies with the rights to use it (`music.wav` beside the scene; never a ripped
//! recording). Until there is one, [`compose`] writes and renders a score of our own (one motif, introduced, built,
//! dropped and answered), so the edit has something real to cut against; a render made with it is labelled temp music.
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
        // Counted from the second bar's first beat, a bar back: the track's very first beat is held at 0 when the grid
        // would put it a few milliseconds before the file begins.
        let first = self.bars.get(1).and_then(|b| self.beats.get(*b)).map_or(0.0, |t| t - 4.0 * step);
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

/// A note of the score as written: which bar (from 1) and beat (from 1, fractions allowed), how many beats long,
/// and its pitch as a MIDI number (69 is A4, 440 Hz, equal temperament).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Written {
    pub bar: usize,
    pub beat: f64,
    pub beats: f64,
    pub midi: i32,
}

/// The score as written and as rendered: its stems (stereo at [`RATE`], each already through its own reverb, so they
/// sum to the mix), and the lead's and the bass's notes, for the piano roll and for checking the audio against the page.
pub struct Score {
    pub bpm: f64,
    pub bars: usize,
    pub lead: Vec<[f32; 2]>,
    pub harmony: Vec<[f32; 2]>,
    pub bass: Vec<[f32; 2]>,
    pub drums: Vec<[f32; 2]>,
    pub effects: Vec<[f32; 2]>,
    pub lead_notes: Vec<Written>,
    pub bass_notes: Vec<Written>,
}

/// The score's tempo and length: 22 bars of four at 88 are exactly a minute.
pub const BPM: f64 = 88.0;
pub const BARS: usize = 22;
/// The bar the drop lands on, and the bar of the final statement.
pub const DROP: usize = 15;
pub const FINAL: usize = 21;

/// The motif, in A minor: E4, up a fourth to A4, a step back to G4, and down to E4 where it began (a question). As
/// (beat, beats, MIDI).
pub const MOTIF: [(f64, f64, i32); 4] = [(1.0, 1.0, 64), (2.0, 1.5, 69), (3.5, 0.5, 67), (4.0, 1.0, 64)];
/// The final statement, over A major: the same rise, E4 to A4, but it does not step back: on to B4 and up to C#5, the
/// major third (the answer), each note twice as long (the last held).
pub const ANSWER: [(f64, f64, i32); 4] = [(1.0, 2.0, 64), (3.0, 2.0, 69), (5.0, 2.0, 71), (7.0, 2.0, 73)];
/// The counter-line that answers the motif on the bars between: a fall of three notes into the next chord.
const COUNTER_F: [(f64, f64, i32); 3] = [(1.0, 1.0, 72), (2.0, 1.0, 71), (3.0, 2.0, 69)];
const COUNTER_G: [(f64, f64, i32); 3] = [(1.0, 1.0, 74), (2.0, 1.0, 71), (3.0, 2.0, 67)];

/// The chord of each bar, by name, with its pad voicing (voice-led: common tones held) and its bass note, as MIDI.
/// i, VI, III, VII in A minor (Am, F, C, G), four times round with the fourth G left bare before the drop, then Am
/// and F, and A major under the final statement.
pub fn chord(bar: usize) -> (&'static str, [i32; 3], i32) {
    const AM: (&str, [i32; 3], i32) = ("Am", [57, 60, 64], 45);
    const F: (&str, [i32; 3], i32) = ("F", [57, 60, 65], 41);
    const C: (&str, [i32; 3], i32) = ("C", [55, 60, 64], 48);
    const G: (&str, [i32; 3], i32) = ("G", [55, 59, 62], 43);
    match bar {
        1 | 2 => AM,
        3..=18 => [AM, F, C, G][(bar - 3) % 4],
        19 => AM,
        20 => F,
        _ => ("A", [57, 61, 64], 45),
    }
}

/// What plays in each bar, in words: the form.
pub fn section(bar: usize) -> &'static str {
    match bar {
        1 | 2 => "intro: the motif alone, one soft voice; the pad comes in under its last note",
        3..=6 => "statement: motif on Am and C, pad and a whole-note bass",
        7..=10 => "second statement: a kick and a hat join; the counter-line answers on F and G",
        11..=13 => "build: kick on every beat, hats in sixteenths, the motif an octave up, a riser through bar 13",
        14 => "near-silence: the pad's tail and a swell; the last beat bare",
        15..=18 => "drop: the motif in octaves, counter-line, full drums, bass in eighths",
        19 | 20 => "after: the motif once more, the drums light",
        21 => "final statement: the motif changed (E4 A4 B4 C#5, twice as slow) over A major, one hit",
        _ => "tail: the chord and the reverb",
    }
}

fn hz(midi: i32) -> f32 {
    440.0 * 2f32.powf((midi - 69) as f32 / 12.0)
}

/// A Schroeder reverb (four combs, two all-passes a side, the right side's delays a little longer), mixed in at `wet`.
fn reverb(stem: &mut [[f32; 2]], wet: f32) {
    for (ch, spread) in [(0usize, 0usize), (1, 23)] {
        let dry: Vec<f32> = stem.iter().map(|s| s[ch]).collect();
        let mut sum = vec![0.0f32; dry.len()];
        for (len, fb) in [(1557, 0.82), (1617, 0.81), (1491, 0.83), (1422, 0.80)] {
            let mut buf = vec![0.0f32; len + spread];
            let mut at = 0;
            for (i, d) in dry.iter().enumerate() {
                let out = buf[at];
                buf[at] = d + out * fb;
                at = (at + 1) % buf.len();
                sum[i] += out * 0.25;
            }
        }
        for len in [225, 556] {
            let mut buf = vec![0.0f32; len + spread];
            let mut at = 0;
            for v in &mut sum {
                let delayed = buf[at];
                let x = *v + delayed * 0.5;
                buf[at] = x;
                at = (at + 1) % buf.len();
                *v = delayed - x * 0.5;
            }
        }
        for (s, w) in stem.iter_mut().zip(&sum) {
            s[ch] += w * wet;
        }
    }
}

/// The score, composed and rendered: one motif, introduced bare, restated over a progression that wants to resolve,
/// built, dropped at full strength, and answered at the end by its changed form (module consts say the notes;
/// [`chord`] and [`section`] the harmony and the form). Deterministic: the only noise is a fixed xorshift.
pub fn compose() -> Score {
    let sr = f64::from(RATE);
    let beat = 60.0 / BPM;
    let n = (BARS as f64 * 4.0 * beat * sr).round() as usize;
    let empty = || vec![[0.0f32; 2]; n];
    let (mut lead, mut harmony, mut bass, mut drums, mut effects) = (empty(), empty(), empty(), empty(), empty());
    let (mut lead_notes, mut bass_notes) = (Vec::new(), Vec::new());
    let tau = std::f32::consts::TAU;
    let at = |bar: usize, b: f64| ((bar - 1) as f64 * 4.0 + b - 1.0) * beat;
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut noise = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    };
    // A touch of the hand: each note a little louder or softer, by its place.
    let touch = |bar: usize, b: f64| 0.92 + 0.16 * (((bar * 37 + (b * 8.0) as usize * 11) % 17) as f32 / 16.0);
    let add = |stem: &mut Vec<[f32; 2]>, t: f64, dur: f64, pan: f32, f: &mut dyn FnMut(f32) -> f32| {
        let from = (t * sr).round() as usize;
        let (l, r) = ((0.5 * (1.0 - pan)).sqrt(), (0.5 * (1.0 + pan)).sqrt());
        for i in 0..(dur * sr) as usize {
            let Some(o) = stem.get_mut(from + i) else { break };
            let v = f(i as f32 / RATE as f32);
            o[0] += v * l;
            o[1] += v * r;
        }
    };
    // The lead: a bell-like pluck (FM, the modulator an octave up and dying faster than the note), with one echo a
    // dotted eighth later.
    let mut pluck = |lead: &mut Vec<[f32; 2]>, notes: &mut Vec<Written>, bar: usize, b: f64, beats: f64, midi: i32, amp: f32, written: bool| {
        let f = hz(midi);
        let amp = amp * touch(bar, b);
        let ring = (beats * beat) as f32 + 0.9;
        for (delay, level, pan) in [(0.0, 1.0, -0.1), (0.75 * beat, 0.28, 0.35)] {
            add(lead, at(bar, b) + delay, f64::from(ring), pan, &mut |t| {
                amp * level * (1.0 - (-t * 400.0).exp()) * (-t * 2.6).exp() * (tau * f * t + 1.4 * (-t * 7.0).exp() * (tau * 2.0 * f * t).sin()).sin()
            });
        }
        if written {
            notes.push(Written { bar, beat: b, beats, midi });
        }
    };
    for bar in 1..=BARS {
        let (_, voicing, root) = chord(bar);
        let level: f32 = match bar {
            1 | 2 => 0.5,
            3..=6 => 0.6,
            7..=10 => 0.72,
            11..=13 => 0.85,
            14 => 0.0,
            15..=18 => 1.0,
            19 | 20 => 0.7,
            _ => 0.85,
        };
        // Lead.
        match bar {
            1 => MOTIF.iter().for_each(|(b, len, m)| pluck(&mut lead, &mut lead_notes, bar, *b, *len, *m, 0.20, true)),
            3 | 5 | 7 | 9 | 19 => MOTIF.iter().for_each(|(b, len, m)| pluck(&mut lead, &mut lead_notes, bar, *b, *len, *m, 0.26 * level, true)),
            8 => COUNTER_F.iter().for_each(|(b, len, m)| pluck(&mut lead, &mut lead_notes, bar, *b, *len, *m, 0.2 * level, true)),
            10 => COUNTER_G.iter().for_each(|(b, len, m)| pluck(&mut lead, &mut lead_notes, bar, *b, *len, *m, 0.2 * level, true)),
            11 | 13 => MOTIF.iter().for_each(|(b, len, m)| pluck(&mut lead, &mut lead_notes, bar, *b, *len, *m + 12, 0.24 * level, true)),
            12 => COUNTER_F.iter().for_each(|(b, len, m)| pluck(&mut lead, &mut lead_notes, bar, *b, *len, *m, 0.2 * level, true)),
            15 | 17 => MOTIF.iter().for_each(|(b, len, m)| {
                pluck(&mut lead, &mut lead_notes, bar, *b, *len, *m, 0.30, true);
                pluck(&mut lead, &mut lead_notes, bar, *b, *len, *m + 12, 0.22, false);
            }),
            16 => COUNTER_F.iter().for_each(|(b, len, m)| pluck(&mut lead, &mut lead_notes, bar, *b, *len, *m, 0.26, true)),
            18 => COUNTER_G.iter().for_each(|(b, len, m)| pluck(&mut lead, &mut lead_notes, bar, *b, *len, *m, 0.26, true)),
            FINAL => ANSWER.iter().for_each(|(b, len, m)| pluck(&mut lead, &mut lead_notes, bar, *b, *len, *m, 0.32, true)),
            _ => {}
        }
        // Harmony: three voices, each two detuned saws through a low-pass, slow in and slow out. From the intro's
        // second bar; held through the last two bars as one chord.
        if bar >= 2 && bar != BARS {
            let bars_long = if bar == FINAL { 2.0 } else { 1.0 };
            let dur = bars_long * 4.0 * beat + 0.5;
            let amp = 0.055 * if bar == 14 { 0.35 } else { level.max(0.5) };
            for (v, m) in voicing.iter().enumerate() {
                let f = hz(*m);
                let (mut lp, mut lp2) = (0.0f32, 0.0f32);
                let cut = 0.035 + 0.05 * level;
                add(&mut harmony, at(bar, 1.0), dur, [-0.5, 0.0, 0.5][v], &mut |t| {
                    let saw = |f: f32| 2.0 * (t * f - (t * f + 0.5).floor());
                    lp += cut * (saw(f) + saw(f * 1.004) - lp);
                    lp2 += cut * (lp - lp2);
                    amp * (t / 0.5).min(1.0) * ((dur as f32 - t) / 0.6).clamp(0.0, 1.0) * lp2
                });
            }
        }
        // Bass: whole notes at first, eighths in the build and the drop; a sine with a little third harmonic, pressed.
        if bar >= 3 && bar != 14 && bar != BARS {
            let eighths = (11..=13).contains(&bar) || (15..=18).contains(&bar);
            let steps: Vec<(f64, f64)> = if bar == FINAL { vec![(1.0, 8.0)] } else if eighths { (0..8).map(|i| (1.0 + f64::from(i) * 0.5, 0.5)).collect() } else { vec![(1.0, 4.0)] };
            for (b, len) in steps {
                let f = hz(root + 12);
                let amp = 0.30 * level * touch(bar, b);
                let dur = len * beat * if eighths { 0.9 } else { 1.0 };
                add(&mut bass, at(bar, b), dur, 0.0, &mut |t| {
                    let env = (1.0 - (-t * 150.0).exp()) * (-t * if eighths { 4.0 } else { 0.5 }).exp() * ((dur as f32 - t) / 0.03).clamp(0.0, 1.0);
                    (1.6 * amp * env * ((tau * f * t).sin() + 0.2 * (tau * 3.0 * f * t).sin())).tanh() / 1.6
                });
                bass_notes.push(Written { bar, beat: b, beats: len, midi: root + 12 });
            }
        }
        // Drums: a kick and a hat from the second statement; every beat and sixteenths in the build and the drop; a
        // soft backbeat in the drop.
        let drummed = (7..=13).contains(&bar) || (15..=20).contains(&bar);
        if drummed {
            let busy = (11..=13).contains(&bar) || (15..=18).contains(&bar);
            let kicks: &[f64] = if busy { &[1.0, 2.0, 3.0, 4.0] } else { &[1.0, 3.0] };
            for b in kicks {
                let amp = 0.55 * level * touch(bar, *b);
                add(&mut drums, at(bar, *b), 0.4, 0.0, &mut |t| amp * (-t * 10.0).exp() * (tau * (50.0 * t + 12.0 * (1.0 - (-t * 30.0).exp()))).sin());
            }
            let hats = if busy { 16 } else { 8 };
            for h in 0..hats {
                let b = 1.0 + 4.0 * f64::from(h) / f64::from(hats);
                let amp = 0.07 * level * touch(bar, b) * if h % (hats / 4) == 0 { 1.0 } else { 0.55 };
                let mut prev = 0.0f32;
                add(&mut drums, at(bar, b), 0.05, if h % 2 == 0 { -0.3 } else { 0.3 }, &mut |t| {
                    let v = noise();
                    let hp = v - prev;
                    prev = v;
                    amp * (-t * 80.0).exp() * hp
                });
            }
            if (15..=18).contains(&bar) {
                for b in [2.0, 4.0] {
                    let mut lp = 0.0f32;
                    add(&mut drums, at(bar, b), 0.2, 0.1, &mut |t| {
                        lp += 0.45 * (noise() - lp);
                        0.3 * (-t * 24.0).exp() * lp
                    });
                }
            }
        }
        // Effects, few: a riser through bar 13, a swell through bar 14's first three beats, a low drop on the drop,
        // one hit on the final statement.
        if bar == 13 || bar == 14 {
            let len = (if bar == 13 { 4.0 } else { 3.0 } * beat) as f32;
            let top = if bar == 13 { 0.22 } else { 0.3 };
            let mut lp = 0.0f32;
            add(&mut effects, at(bar, 1.0), f64::from(len), 0.0, &mut |t| {
                let p = t / len;
                lp += (0.02 + 0.5 * p * p) * (noise() - lp);
                top * p * p * lp * ((len - t) / 0.02).clamp(0.0, 1.0)
            });
        }
        if bar == DROP || bar == FINAL {
            add(&mut effects, at(bar, 1.0), 2.2, 0.0, &mut |t| 0.5 * (-t * 2.5).exp() * (tau * (41.0 * t + 14.0 * (1.0 - (-t * 12.0).exp()))).sin());
        }
    }
    reverb(&mut lead, 0.35);
    reverb(&mut harmony, 0.3);
    reverb(&mut drums, 0.08);
    reverb(&mut effects, 0.25);
    Score { bpm: BPM, bars: BARS, lead, harmony, bass, drums, effects, lead_notes, bass_notes }
}

impl Score {
    /// The stems summed, softly limited: the track.
    pub fn mix(&self) -> Vec<[f32; 2]> {
        (0..self.lead.len())
            .map(|i| {
                let s = |k: usize| self.lead[i][k] + self.harmony[i][k] + self.bass[i][k] + self.drums[i][k] + self.effects[i][k];
                [(s(0) * 1.4).tanh() / 1.4, (s(1) * 1.4).tanh() / 1.4]
            })
            .collect()
    }

    /// The pitch heard in the lead stem for each written note, as a MIDI number with its fraction: the strongest
    /// period (autocorrelation, 180 to 1300 Hz) of the 40 ms that start 25 ms into the note.
    pub fn heard(&self) -> Vec<(Written, f64)> {
        let beat = 60.0 / self.bpm;
        self.lead_notes
            .iter()
            .map(|w| {
                let from = ((((w.bar - 1) as f64 * 4.0 + w.beat - 1.0) * beat + 0.025) * f64::from(RATE)) as usize;
                let win: Vec<f64> = self.lead[from.min(self.lead.len())..(from + 1920).min(self.lead.len())].iter().map(|s| f64::from(s[0] + s[1])).collect();
                let (lo, hi) = ((f64::from(RATE) / 1300.0) as usize, (f64::from(RATE) / 180.0) as usize);
                let score = |lag: usize| win.iter().zip(&win[lag.min(win.len())..]).map(|(a, b)| a * b).sum::<f64>() / (win.len() - lag.min(win.len())).max(1) as f64;
                let top = (lo..=hi).map(score).fold(f64::NEG_INFINITY, f64::max);
                // The first lag that is nearly the best: the period itself, not a multiple of it.
                let lag = (lo..=hi).find(|l| score(*l) > 0.9 * top && score(*l) >= score(*l + 1) && score(*l) >= score(*l - 1)).unwrap_or(lo);
                let (a, b, c) = (score(lag - 1), score(lag), score(lag + 1));
                let fine = lag as f64 + 0.5 * (a - c) / (a - 2.0 * b + c).min(-1e-12);
                (*w, 69.0 + 12.0 * (f64::from(RATE) / fine / 440.0).log2())
            })
            .collect()
    }

    /// The score on paper: key, tempo, the motif and its answer by name, and each bar's chord and what plays.
    pub fn page(&self) -> String {
        let name = |m: i32| format!("{}{}", ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"][m.rem_euclid(12) as usize], m / 12 - 1);
        let line = |notes: &[(f64, f64, i32)]| notes.iter().map(|(b, len, m)| format!("{} (beat {b}, {len} beats)", name(*m))).collect::<Vec<_>>().join(", ");
        let mut out = format!(
            "Key: A minor, ending in A major. Tempo: {} beats a minute, 4/4, {} bars = {:.1} s. Equal temperament, A4 = 440 Hz.\n\
             The motif: {}.\nThe answer (the final statement, bar {FINAL}): {}: the same rise, then on up to the major third instead of falling back, twice as slow.\n\
             Counter-line: over F {}; over G {}.\nProgression: i VI III VII (Am F C G), a chord a bar.\n\nbar  chord  what plays\n",
            self.bpm,
            self.bars,
            self.bars as f64 * 240.0 / self.bpm,
            line(&MOTIF),
            line(&ANSWER),
            line(&COUNTER_F),
            line(&COUNTER_G)
        );
        for bar in 1..=self.bars {
            let (chord, voicing, root) = chord(bar);
            let lead: Vec<String> = self.lead_notes.iter().filter(|w| w.bar == bar).map(|w| name(w.midi)).collect();
            out.push_str(&format!(
                "{bar:>3}  {chord:<5}  pad {} | bass {} | lead {} | {}\n",
                voicing.map(name).join(" "),
                name(root + 12),
                if lead.is_empty() { "-".to_owned() } else { lead.join(" ") },
                section(bar)
            ));
        }
        out
    }

    /// The piano roll: the lead's notes (accent) and the bass's (grey) by pitch, against the bar lines. `(w, h)` RGBA8.
    pub fn roll(&self, (w, h): (usize, usize)) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        for p in px.chunks_exact_mut(4) {
            p.copy_from_slice(&[12, 13, 16, 255]);
        }
        let total = self.bars as f64 * 4.0;
        let (low, high) = (36, 88);
        let mut rect = |x0: f64, x1: f64, y: usize, c: [u8; 3]| {
            for x in (x0 * w as f64) as usize..((x1 * w as f64) as usize).min(w) {
                for yy in y..(y + 4).min(h) {
                    px[(yy * w + x) * 4..(yy * w + x) * 4 + 3].copy_from_slice(&c);
                }
            }
        };
        for bar in 0..self.bars {
            let x = bar as f64 * 4.0 / total;
            for y in (0..h).step_by(if bar + 1 == DROP || bar + 1 == FINAL { 1 } else { 3 }) {
                rect(x, x + 1.0 / w as f64, y, [70, 74, 84]);
            }
        }
        for (notes, c) in [(&self.bass_notes, [150u8, 158, 175]), (&self.lead_notes, [255, 149, 50])] {
            for n in notes.iter() {
                let x0 = ((n.bar - 1) as f64 * 4.0 + n.beat - 1.0) / total;
                let y = h - 6 - ((n.midi - low) as usize * (h - 12)) / (high - low) as usize;
                rect(x0, x0 + n.beats * 0.92 / total, y, c);
            }
        }
        px
    }
}

/// A mix's loudness over time, as a curve: each column the level (RMS, dB) of a half second, -60 dB at the bottom to
/// 0 at the top, with the bar lines of a `bpm` grid. `(w, h)` RGBA8. The build should climb and the drop stand tallest.
pub fn loudness_curve(mix: &[[f32; 2]], bpm: f64, (w, h): (usize, usize)) -> Vec<u8> {
    let mut px = vec![0u8; w * h * 4];
    for p in px.chunks_exact_mut(4) {
        p.copy_from_slice(&[12, 13, 16, 255]);
    }
    let length = mix.len() as f64 / f64::from(RATE);
    let bar = 240.0 / bpm;
    for x in 0..w {
        let t = x as f64 / w as f64 * length;
        let (from, to) = (((t - 0.25).max(0.0) * f64::from(RATE)) as usize, (((t + 0.25) * f64::from(RATE)) as usize).min(mix.len()));
        let rms = (mix[from..to].iter().map(|s| f64::from(s[0] * s[0] + s[1] * s[1]) * 0.5).sum::<f64>() / (to - from).max(1) as f64).sqrt();
        let level = ((20.0 * rms.max(1e-6).log10() + 60.0) / 60.0).clamp(0.0, 1.0);
        let top = h - 1 - (level * (h - 1) as f64) as usize;
        let on_bar = (t / bar).fract() < length / w as f64 / bar;
        for y in 0..h {
            let c: [u8; 3] = if y >= top { [255, 149, 50] } else if on_bar { [70, 74, 84] } else { [12, 13, 16] };
            px[(y * w + x) * 4..(y * w + x) * 4 + 3].copy_from_slice(&c);
        }
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refinement: the analysis finds the grid the score was written on. Its tempo to within half a beat a minute,
    /// every beat to within a frame and a half of where the score put it, the bars counted from the score's own first
    /// beat, and the drop at the bar the score dropped on. The control: a track with no pulse (noise at one level) has
    /// no drop where the score's is.
    #[test]
    fn the_grid_of_the_score_is_found() {
        let score = compose().mix();
        assert_eq!(score.len(), 60 * RATE as usize, "22 bars at 88 are a minute");
        let grid = analyse(&score, "temp score").unwrap_or_else(|e| panic!("{e}"));
        assert!((grid.bpm - BPM).abs() < 0.5, "tempo {}", grid.bpm);
        let step = 60.0 / BPM;
        for (i, t) in grid.beats.iter().enumerate() {
            let nearest = (t / step).round() * step;
            assert!((t - nearest).abs() < 0.025, "beat {i} at {t}, {} off the score's", t - nearest);
        }
        assert_eq!(grid.bars.first(), Some(&0), "bars are counted from the first beat");
        assert_eq!(grid.bars.len(), BARS);
        let dropped = grid.sections.iter().find(|s| s.label == "drop").unwrap_or_else(|| panic!("no drop in {:?}", grid.sections));
        assert_eq!(dropped.bar, DROP, "{:?}", grid.sections);
        assert!(grid.constants().contains("export const m_b15_1: f32 = "));
        assert_eq!(grid, analyse(&compose().mix(), "temp score").unwrap_or_else(|e| panic!("{e}")));
        let mut state = 9u64;
        let flat: Vec<[f32; 2]> = (0..20 * RATE as usize)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let v = ((state >> 40) as f32 / (1u64 << 23) as f32 - 1.0) * 0.2;
                [v, v]
            })
            .collect();
        let none = analyse(&flat, "noise").unwrap_or_else(|e| panic!("{e}"));
        assert!(none.sections.iter().all(|s| s.label != "drop" || s.bar != DROP), "{:?}", none.sections);
    }

    /// The refinement: the audio is the page. Every written note of the lead is heard in the lead stem as that note (within
    /// half a semitone: the echo of the note before can pull the reading tens of cents); the first statement (bar 1) is the motif, E4 A4 G4 E4; the final statement (bar 21) is the
    /// answer, the same first two notes and then B4 and C#5 where the motif had G4 and E4, each note twice as long.
    #[test]
    fn the_motif_is_heard_as_written_and_its_answer_differs_as_designed() {
        let score = compose();
        let heard = score.heard();
        assert!(heard.len() > 40, "{} notes", heard.len());
        for (w, midi) in &heard {
            assert!((midi - f64::from(w.midi)).abs() < 0.5, "bar {} beat {}: written {}, heard {midi:.2}", w.bar, w.beat, w.midi);
        }
        let of = |bar: usize| -> Vec<(i32, f64)> { heard.iter().filter(|(w, _)| w.bar == bar).map(|(w, m)| (m.round() as i32, w.beats)).collect() };
        assert_eq!(of(1).iter().map(|n| n.0).collect::<Vec<_>>(), [64, 69, 67, 64], "the motif, heard");
        assert_eq!(of(FINAL).iter().map(|n| n.0).collect::<Vec<_>>(), [64, 69, 71, 73], "the answer, heard");
        assert!(of(FINAL).iter().all(|n| n.1 == 2.0) && of(1)[0].1 == 1.0, "the answer is twice as slow");
        assert!(score.page().contains("E4 (beat 1, 1 beats), A4 (beat 2, 1.5 beats), G4 (beat 3.5, 0.5 beats), E4 (beat 4, 1 beats)"));
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
