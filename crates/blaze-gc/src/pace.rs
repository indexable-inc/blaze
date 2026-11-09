//! The pacer (docs/gc.md, "Pacing"): how hard the collector works, from how full each volume is.
//! Like Go's GC pacer, it never switches collection on or off at one threshold. Pressure `p`
//! rises smoothly from 0 at the comfortable level of free space to 1 at the floor, and the work
//! (how often a slice runs, how many steps and bytes it may take) scales with it. Pure: every
//! function takes what it needs (the volume, the clock) and returns a number. Nothing here
//! reads a clock or the file system.
//!
//! Pressure is in permille (0 to 1000), an integer, so the same inputs always give the same
//! slices (rule 25).

use blaze_serde::{Deserialize, Serialize};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
const SECOND: u64 = 1_000_000_000;

/// Full pressure, in permille.
pub const FULL: u32 = 1000;

/// A volume as measured: `statvfs`'s `f_bavail` (free to an unprivileged user) and size on
/// unix, `GetDiskFreeSpaceExW` on Windows; `id` tells two areas on one volume apart from two
/// volumes (`st_dev`; an APFS container's name on macOS, whose volumes share free space; the
/// volume's serial number on Windows: [`crate::space::volume`]).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Volume {
    pub id: u64,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

/// Where pressure starts and where it is full, and blaze's share of a volume. The defaults:
/// comfortable at max(15%, 30 GiB) free, the floor at max(10%, 20 GiB), blaze's own bytes at
/// most half the volume. Overridable in `<root>/gc.json` ([`crate::settings`]).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Levels {
    pub comfortable_percent: u8,
    pub comfortable_bytes: u64,
    pub floor_percent: u8,
    pub floor_bytes: u64,
    /// blaze's own bytes on a volume (every object the collector lists there) past this share
    /// of it are pressure as well.
    pub share_percent: u8,
    /// Collection starts once pressure reaches this, and stops once free space is this far
    /// (permille of the band between floor and comfortable) above the comfortable level: a band
    /// either side, so free space hovering at a level does not switch it on and off.
    pub hysteresis_permille: u32,
}

impl Default for Levels {
    fn default() -> Levels {
        Levels {
            comfortable_percent: 15,
            comfortable_bytes: 30 * GIB,
            floor_percent: 10,
            floor_bytes: 20 * GIB,
            share_percent: 50,
            hysteresis_permille: 50,
        }
    }
}

impl Levels {
    /// Free bytes at which pressure is 0, never above the volume.
    pub fn comfortable(&self, volume: &Volume) -> u64 {
        percent(volume.total_bytes, self.comfortable_percent)
            .max(self.comfortable_bytes)
            .min(volume.total_bytes)
    }

    /// Free bytes at which pressure is full, below the comfortable level by at least a byte.
    pub fn floor(&self, volume: &Volume) -> u64 {
        let comfortable = self.comfortable(volume);
        percent(volume.total_bytes, self.floor_percent)
            .max(self.floor_bytes)
            .min(comfortable.saturating_sub(1))
    }

    /// The bytes blaze may hold on `volume`.
    pub fn share(&self, volume: &Volume) -> u64 {
        percent(volume.total_bytes, self.share_percent)
    }

    /// Pressure from free space, signed: 0 at comfortable, [`FULL`] at the floor, negative
    /// above comfortable (down to -[`FULL`]), so the hysteresis band can be told.
    pub fn free_pressure(&self, volume: &Volume) -> i64 {
        let comfortable = self.comfortable(volume);
        let band = comfortable.saturating_sub(self.floor(volume)).max(1);
        let below = i128::from(comfortable) - i128::from(volume.free_bytes);
        let p = below * i128::from(FULL) / i128::from(band);
        p.clamp(-i128::from(FULL), i128::from(FULL)) as i64
    }

    /// blaze's bytes at which its share's pressure starts: 80% of its share.
    pub fn share_start(&self, volume: &Volume) -> u64 {
        self.share(volume) / 5 * 4
    }

    /// Pressure from blaze's share: 0 at 80% of its share ([`Levels::share_start`]), [`FULL`]
    /// at the share.
    pub fn share_pressure(&self, volume: &Volume, blaze_bytes: u64) -> i64 {
        let share = self.share(volume);
        let start = self.share_start(volume);
        if blaze_bytes <= start {
            return 0;
        }
        let p =
            u128::from(blaze_bytes - start) * u128::from(FULL) / u128::from((share - start).max(1));
        p.min(u128::from(FULL)) as i64
    }

    /// Bytes to free on `volume` for free space to be back past the band above comfortable,
    /// and blaze back at the start of its share's pressure ([`Levels::share_start`]): where the
    /// pressure curve starts, so pressure from the share always comes with bytes to free, and
    /// the hysteresis turns off once they are freed (review N10; before, blaze between 80% and
    /// 100% of its share kept pressure on with nothing to free).
    pub fn to_free(&self, volume: &Volume, blaze_bytes: u64) -> u64 {
        let comfortable = self.comfortable(volume);
        let band = comfortable.saturating_sub(self.floor(volume));
        let target = comfortable
            .saturating_add(band * u64::from(self.hysteresis_permille) / u64::from(FULL));
        let free = target.saturating_sub(volume.free_bytes);
        free.max(blaze_bytes.saturating_sub(self.share_start(volume)))
    }
}

fn percent(of: u64, percent: u8) -> u64 {
    (u128::from(of) * u128::from(percent) / 100) as u64
}

/// How a slice's size and rate scale with pressure: each quantity goes linearly from its value
/// at the start of pressure to its value at full.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pace {
    /// Between slices: from `slow_gap_ns` near no pressure to `fast_gap_ns` at full.
    pub slow_gap_ns: u64,
    pub fast_gap_ns: u64,
    /// Renames and unlinks a slice may ask for.
    pub few_steps: u32,
    pub many_steps: u32,
    /// Bytes a slice may free, from the bandwidth: `slow_bytes_per_s` near no pressure to
    /// `fast_bytes_per_s` at full, times the gap to the next slice.
    pub slow_bytes_per_s: u64,
    pub fast_bytes_per_s: u64,
    /// Build dirs and stores a slice's scan walks: a cursor goes over them across slices.
    pub few_dirs: u32,
    pub many_dirs: u32,
    /// A slice's deadline on the injected clock.
    pub slice_ns: u64,
    /// Below this pressure a slice runs only while the daemon is idle.
    pub busy_from: u32,
}

impl Default for Pace {
    fn default() -> Pace {
        Pace {
            slow_gap_ns: 60 * SECOND,
            fast_gap_ns: SECOND,
            few_steps: 4,
            many_steps: 256,
            slow_bytes_per_s: 16 * MIB,
            fast_bytes_per_s: 1024 * MIB,
            few_dirs: 4,
            many_dirs: 64,
            slice_ns: 5 * SECOND,
            busy_from: 500,
        }
    }
}

/// One slice's bounds.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slice {
    /// The pressure it was paced for, permille.
    pub pressure: u32,
    pub steps: u32,
    pub bytes: u64,
    pub dirs: u32,
    pub slice_ns: u64,
    /// When the next may start after this one ends.
    pub gap_ns: u64,
    /// Whether it may run while builds run (it still yields to them: background jobs).
    pub busy: bool,
    /// A build would fail for want of space: the caps are lifted.
    pub urgent: bool,
}

/// `slow` at pressure 0 to `fast` at [`FULL`], linearly.
fn lerp(slow: u64, fast: u64, pressure: u32) -> u64 {
    let p = u128::from(pressure.min(FULL));
    let (slow, fast) = (u128::from(slow), u128::from(fast));
    let at = if fast >= slow {
        slow + (fast - slow) * p / u128::from(FULL)
    } else {
        slow - (slow - fast) * p / u128::from(FULL)
    };
    at as u64
}

impl Pace {
    /// The slice for `pressure` (permille); `urgent` lifts every cap but the slice's deadline.
    pub fn slice(&self, pressure: u32, urgent: bool) -> Slice {
        let pressure = pressure.min(FULL);
        let gap_ns = lerp(self.slow_gap_ns, self.fast_gap_ns, pressure);
        let per_s = lerp(self.slow_bytes_per_s, self.fast_bytes_per_s, pressure);
        // A slice starts at most once a gap, so a gap's worth of the bandwidth bounds it: the
        // bytes freed a second stay at `per_s` however long each slice takes.
        let bytes = (u128::from(per_s) * u128::from(gap_ns) / u128::from(SECOND)) as u64;
        Slice {
            pressure,
            steps: if urgent {
                u32::MAX
            } else {
                lerp(u64::from(self.few_steps), u64::from(self.many_steps), pressure) as u32
            },
            bytes: if urgent { u64::MAX } else { bytes },
            dirs: if urgent {
                u32::MAX
            } else {
                lerp(u64::from(self.few_dirs), u64::from(self.many_dirs), pressure) as u32
            },
            slice_ns: self.slice_ns,
            gap_ns,
            busy: urgent || pressure >= self.busy_from,
            urgent,
        }
    }
}

/// Whether pressure collection is on, with its band: on once the pressure reaches the band's
/// start, off once free space is past the band above comfortable. In between it keeps what it
/// was, so a level crossed back and forth switches it at most once.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Hysteresis {
    pub on: bool,
}

impl Hysteresis {
    /// The pressure to pace for, given the signed pressure now: 0 while off; at least the
    /// band's start while on, so the last of the work is done gently, not dropped.
    pub fn step(&mut self, signed: i64, band: u32) -> u32 {
        let band = i64::from(band);
        if !self.on && signed >= band {
            self.on = true;
        } else if self.on && signed <= -band {
            self.on = false;
        }
        match self.on {
            true => signed.max(band) as u32,
            false => 0,
        }
    }
}
