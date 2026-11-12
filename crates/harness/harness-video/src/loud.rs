//! The mix bus: how loud a soundtrack is, and bringing it to a target. [`measure`] is ITU-R BS.1770-4's integrated
//! loudness (K-weighting, 400 ms blocks every 100 ms, the absolute gate at -70 LUFS and the relative one 10 LU under the
//! ungated mean) and its true peak (the largest sample of the signal four times oversampled). [`master`] scales a mix to
//! a loudness and holds its peaks under a ceiling with a look-ahead limiter. All of it is plain arithmetic on the
//! samples in order: the same mix in gives the same bytes out.
//!
//! 48 kHz stereo only: the K-weighting's coefficients are the standard's own, which it gives for 48 kHz.

/// The rate everything here is for.
pub const RATE: u32 = 48_000;

/// What [`measure`] found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Loudness {
    /// Integrated loudness, LUFS (negative infinity for silence).
    pub lufs: f64,
    /// True peak, dBTP.
    pub true_peak: f64,
    /// The largest sample, dBFS.
    pub sample_peak: f64,
}

/// A biquad, direct form 1, `a0` 1.
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    x: [f64; 2],
    y: [f64; 2],
}

impl Biquad {
    fn new(b: [f64; 3], a: [f64; 2]) -> Self {
        Self { b, a, x: [0.0; 2], y: [0.0; 2] }
    }

    fn step(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1] - self.a[0] * self.y[0] - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

/// One channel through BS.1770's K-weighting at 48 kHz: the head's shelf, then the RLB high-pass.
fn k_weighted(channel: impl Iterator<Item = f32>) -> Vec<f64> {
    let mut shelf = Biquad::new([1.53512485958697, -2.69169618940638, 1.19839281085285], [-1.69065929318241, 0.73248077421585]);
    let mut high = Biquad::new([1.0, -2.0, 1.0], [-1.99004745483398, 0.99007225036621]);
    channel.map(|s| high.step(shelf.step(f64::from(s)))).collect()
}

/// Taps each side of the true-peak interpolator.
const HALF: isize = 12;

/// The largest magnitude of `channel` oversampled four times (a Hann-windowed sinc), linear.
fn true_peak(channel: &[f32]) -> f64 {
    let mut taps = [[0.0f64; (2 * HALF) as usize]; 3];
    for (k, phase) in taps.iter_mut().enumerate() {
        let frac = (k + 1) as f64 / 4.0;
        for (j, tap) in phase.iter_mut().enumerate() {
            // The sample at `i + frac` from the samples `i - HALF + 1 ..= i + HALF`.
            let d = (j as isize - HALF + 1) as f64 - frac;
            let sinc = if d == 0.0 { 1.0 } else { (std::f64::consts::PI * d).sin() / (std::f64::consts::PI * d) };
            let window = 0.5 + 0.5 * (std::f64::consts::PI * d / (HALF as f64 + 0.5)).cos();
            *tap = sinc * window;
        }
    }
    let at = |i: isize| if i < 0 || i as usize >= channel.len() { 0.0 } else { f64::from(channel[i as usize]) };
    let mut peak = 0.0f64;
    for i in 0..channel.len() as isize {
        peak = peak.max(at(i).abs());
        for phase in &taps {
            let v: f64 = phase.iter().enumerate().map(|(j, t)| t * at(i + j as isize - HALF + 1)).sum();
            peak = peak.max(v.abs());
        }
    }
    peak
}

fn db(linear: f64) -> f64 {
    20.0 * linear.log10()
}

/// The loudness and peaks of `mix` (stereo at [`RATE`]).
pub fn measure(mix: &[[f32; 2]]) -> Loudness {
    let k = [k_weighted(mix.iter().map(|s| s[0])), k_weighted(mix.iter().map(|s| s[1]))];
    let (block, step) = (RATE as usize * 4 / 10, RATE as usize / 10);
    // Each block's energy: the sum over both channels of the mean square.
    let mut energies = Vec::new();
    let mut from = 0;
    while from + block <= mix.len() {
        let e: f64 = k.iter().map(|c| c[from..from + block].iter().map(|v| v * v).sum::<f64>() / block as f64).sum();
        energies.push(e);
        from += step;
    }
    let loud = |e: f64| -0.691 + 10.0 * e.log10();
    let mean = |es: &[f64]| es.iter().sum::<f64>() / es.len() as f64;
    let over: Vec<f64> = energies.iter().copied().filter(|e| loud(*e) > -70.0).collect();
    let lufs = if over.is_empty() {
        f64::NEG_INFINITY
    } else {
        let gate = loud(mean(&over)) - 10.0;
        let kept: Vec<f64> = over.iter().copied().filter(|e| loud(*e) > gate).collect();
        if kept.is_empty() { f64::NEG_INFINITY } else { loud(mean(&kept)) }
    };
    let left: Vec<f32> = mix.iter().map(|s| s[0]).collect();
    let right: Vec<f32> = mix.iter().map(|s| s[1]).collect();
    let sample = mix.iter().flat_map(|s| [s[0].abs(), s[1].abs()]).fold(0.0f32, f32::max);
    Loudness { lufs, true_peak: db(true_peak(&left).max(true_peak(&right))), sample_peak: db(f64::from(sample)) }
}

/// How far ahead the limiter sees: 5 ms.
const LOOK: usize = RATE as usize / 200;

/// Hold every sample of `mix` at or under `ceiling` (linear): each sample's gain is the least any sample within
/// [`LOOK`] ahead of it needs, averaged over the [`LOOK`] before it, so the gain is already down when a peak arrives and
/// moves smoothly. The average of gains each at most what this sample needs is at most what it needs.
fn limit(mix: &mut [[f32; 2]], ceiling: f32) {
    let need: Vec<f32> = mix.iter().map(|s| (ceiling / s[0].abs().max(s[1].abs()).max(1e-9)).min(1.0)).collect();
    let n = need.len();
    // least[i] = min need[i..=i+LOOK], by a monotonic queue of indices.
    let mut least = vec![1.0f32; n];
    let mut queue = std::collections::VecDeque::new();
    let mut next = 0;
    for (i, slot) in least.iter_mut().enumerate() {
        while next < n && next <= i + LOOK {
            while queue.back().is_some_and(|&j: &usize| need[j] >= need[next]) {
                queue.pop_back();
            }
            queue.push_back(next);
            next += 1;
        }
        while queue.front().is_some_and(|&j| j < i) {
            queue.pop_front();
        }
        *slot = queue.front().map_or(1.0, |&j| need[j]);
    }
    let mut sum = 0.0f64;
    for i in 0..n {
        sum += f64::from(least[i]);
        if i > LOOK {
            sum -= f64::from(least[i - LOOK - 1]);
        }
        let g = (sum / (i.min(LOOK) + 1) as f64) as f32;
        mix[i] = mix[i].map(|s| s * g);
    }
}

/// Bring `mix` to `lufs` integrated with no true peak over `ceiling` dBTP, and say what it then measures. The limiter
/// works on samples, 0.4 dB under the ceiling, so what lies between samples stays under it; if a true peak still passes
/// the ceiling the whole mix comes down by the difference. Err for a silent mix (nothing to bring to a loudness).
pub fn master(mix: &mut [[f32; 2]], lufs: f64, ceiling: f64) -> Result<Loudness, String> {
    let before = measure(mix);
    if !before.lufs.is_finite() {
        return Err("the mix is silent: it has no loudness to bring to a target".into());
    }
    let hold = 10f64.powf((ceiling - 0.4) / 20.0) as f32;
    // Twice: the limiter takes a little loudness off, which the second pass gives back.
    for _ in 0..2 {
        let now = measure(mix);
        let gain = 10f64.powf((lufs - now.lufs) / 20.0) as f32;
        for s in mix.iter_mut() {
            *s = s.map(|v| v * gain);
        }
        limit(mix, hold);
    }
    let after = measure(mix);
    if after.true_peak > ceiling {
        let down = 10f64.powf((ceiling - after.true_peak) / 20.0) as f32;
        for s in mix.iter_mut() {
            *s = s.map(|v| v * down);
        }
        return Ok(measure(mix));
    }
    Ok(after)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f64, amp: f64, seconds: f64) -> Vec<[f32; 2]> {
        (0..(seconds * f64::from(RATE)) as usize)
            .map(|i| {
                let v = (amp * (std::f64::consts::TAU * hz * i as f64 / f64::from(RATE)).sin()) as f32;
                [v, v]
            })
            .collect()
    }

    /// BS.1770's own reference: a 997 Hz sine at -23 dBFS in both channels of a stereo pair reads -23 LUFS... per
    /// channel it is -3.01 - 23 + the two channels' sum (+3.01) - 0.691 + the K-weighting's gain at 997 Hz (+0.691).
    #[test]
    fn a_reference_tone_reads_its_level() {
        let l = measure(&sine(997.0, 10f64.powf(-23.0 / 20.0), 5.0));
        assert!((l.lufs - -23.0).abs() < 0.1, "{l:?}");
        assert!((l.sample_peak - -23.0).abs() < 0.05 && (l.true_peak - -23.0).abs() < 0.05, "{l:?}");
        assert_eq!(measure(&vec![[0.0; 2]; RATE as usize]).lufs, f64::NEG_INFINITY);
    }

    /// A true peak lies between samples: a sine at a quarter of the rate, sampled 45 degrees off its crests, has samples
    /// 3 dB under its peak, and the meter finds the peak.
    #[test]
    fn true_peak_sees_between_samples() {
        let mix: Vec<[f32; 2]> = (0..RATE as usize)
            .map(|i| {
                let v = (std::f64::consts::TAU * (i as f64 / 4.0 + 0.125)).sin() as f32 * 0.5;
                [v, v]
            })
            .collect();
        let l = measure(&mix);
        assert!((l.sample_peak - db(0.5 * 0.5f64.sqrt())).abs() < 0.01, "{l:?}");
        assert!((l.true_peak - db(0.5)).abs() < 0.2, "{l:?}");
    }

    /// The master lands on its targets, and the limiter holds a burst far over the ceiling. The control: without the
    /// master the same mix is over both.
    #[test]
    fn master_lands_on_loudness_under_the_ceiling() {
        let mut mix = sine(220.0, 0.05, 6.0);
        for (i, s) in mix.iter_mut().enumerate().skip(RATE as usize).take(4800) {
            let burst = ((i as f32) * 0.9).sin() * 0.9;
            *s = [s[0] + burst, s[1] - burst];
        }
        let raw = measure(&mix);
        assert!(raw.lufs < -20.0 || raw.true_peak > -1.0, "the control is already on target: {raw:?}");
        let out = master(&mut mix, -16.0, -1.0).unwrap_or_else(|e| panic!("{e}"));
        assert!((out.lufs - -16.0).abs() < 0.5, "{out:?}");
        assert!(out.true_peak <= -1.0 + 1e-6, "{out:?}");
        assert_eq!(out, measure(&mix));
        assert!(master(&mut vec![[0.0; 2]; RATE as usize], -16.0, -1.0).is_err());
    }
}
