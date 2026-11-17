//! How far a download has come, as a sans-IO core: a [`Meter`] is told the bytes that arrive
//! ([`Meter::got`]) and the time ([`Meter::advance_clock`], from the driver's clock, as a
//! [`Duration`] since the driver's own origin), and says when a line is due
//! ([`Meter::take`]), what it says ([`Line`]: bytes done of the total, the rate over the last
//! few seconds, the time left), and when no byte has come for its stall bound
//! ([`Said::Stalled`]), which the driver turns into an error naming the address. It owns no
//! clock, terminal or socket: the driver prints, and asks again at [`Meter::next_deadline`].
//!
//! One meter is shared by every way blaze shows a download: `blaze version-fetch`'s line on a
//! terminal (redrawn in place) or off one (a line every few seconds), the daemon's downloads
//! (`crate::downloads`, which the client's status line and `blaze status` read), and the
//! build's stall bound, for which a byte arriving is progress.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{fmt, time::Duration};

/// How long a download may go without a byte before it is stalled, everywhere blaze downloads.
pub const STALL: Duration = Duration::from_secs(30);

/// How often a line is said on a terminal, where it is redrawn in place.
pub const TERMINAL_EVERY: Duration = Duration::from_millis(250);

/// How often a line is said off a terminal (a CI log, a pipe), each on its own line.
pub const LOG_EVERY: Duration = Duration::from_secs(5);

/// How many samples the rate is taken over, one at most every [`SAMPLE_EVERY`]: the rate is of
/// the last few seconds, so a download that slows says so soon.
const SAMPLES: usize = 10;
const SAMPLE_EVERY: Duration = Duration::from_millis(500);

/// After how long without a byte a line says so.
const QUIET_SAID: Duration = Duration::from_secs(3);

/// The progress of one download, on an injected clock.
#[derive(Clone, Debug)]
pub struct Meter {
    total: Option<u64>,
    done: u64,
    now: Duration,
    start: Duration,
    last_byte: Duration,
    every: Duration,
    stall: Duration,
    next_line: Duration,
    stalled: bool,
    /// `(when, done then)`, oldest first, at most [`SAMPLES`].
    samples: [(Duration, u64); SAMPLES],
    sampled: usize,
}

/// What a [`Meter`] says when asked ([`Meter::take`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Said {
    /// A line is due: print it.
    Line(Line),
    /// No byte came for `idle`, at least the stall bound: the download is stopped as stalled,
    /// with `done` of `total` bytes come. Said once.
    Stalled { idle: Duration, done: u64, total: Option<u64> },
}

/// One line of progress: `12.3 of 40.0 MB (31%), 2.1 MB/s, 13 s left`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Line {
    pub done: u64,
    pub total: Option<u64>,
    /// Bytes a second over the last few seconds; `None` before there are two samples.
    pub rate: Option<f64>,
    /// How long since the last byte.
    pub idle: Duration,
    /// How long since the download began.
    pub elapsed: Duration,
}

impl Meter {
    /// A download starting at `now`, `total` bytes long when known, a line due every `every`
    /// ([`TERMINAL_EVERY`], [`LOG_EVERY`]) and stalled after `stall` without a byte ([`STALL`]).
    pub fn new(now: Duration, total: Option<u64>, every: Duration, stall: Duration) -> Meter {
        Meter {
            total,
            done: 0,
            now,
            start: now,
            last_byte: now,
            every,
            stall,
            next_line: now + every,
            stalled: false,
            samples: [(now, 0); SAMPLES],
            sampled: 1,
        }
    }

    /// The whole length, once a response says it.
    pub fn set_total(&mut self, total: Option<u64>) {
        self.total = total;
    }

    /// `bytes` were there before this download began (a partial file it resumes): counted as
    /// done, not in the rate.
    pub fn resumed(&mut self, bytes: u64) {
        self.done = self.done.saturating_add(bytes);
        for (_, had) in &mut self.samples[..self.sampled] {
            *had = had.saturating_add(bytes);
        }
    }

    /// `bytes` more came, at the clock's last reading.
    pub fn got(&mut self, bytes: u64) {
        if bytes > 0 {
            self.done = self.done.saturating_add(bytes);
            self.last_byte = self.now;
            self.stalled = false;
        }
    }

    /// The clock reads `now` (never earlier than before: an earlier reading is taken as the
    /// last).
    pub fn advance_clock(&mut self, now: Duration) {
        if now <= self.now {
            return;
        }
        self.now = now;
        let (last, _) = self.samples[self.sampled - 1];
        if now.saturating_sub(last) >= SAMPLE_EVERY {
            if self.sampled == SAMPLES {
                self.samples.rotate_left(1);
                self.sampled -= 1;
            }
            self.samples[self.sampled] = (now, self.done);
            self.sampled += 1;
        }
    }

    /// When to ask [`Meter::take`] again: the next line, or the stall, whichever is first.
    pub fn next_deadline(&self) -> Duration {
        self.next_line.min(self.last_byte + self.stall)
    }

    /// What is due at the clock's reading: the stall once its bound is reached, else a line
    /// every `every`, else nothing.
    pub fn take(&mut self) -> Option<Said> {
        let idle = self.now.saturating_sub(self.last_byte);
        if idle >= self.stall && !self.stalled {
            self.stalled = true;
            return Some(Said::Stalled { idle, done: self.done, total: self.total });
        }
        if self.now < self.next_line {
            return None;
        }
        // The next is a whole `every` on from this one, never a burst to catch up.
        self.next_line = self.now + self.every;
        Some(Said::Line(self.line()))
    }

    /// The line as of now, whether or not one is due: the last one a driver says when it ends.
    pub fn line(&self) -> Line {
        Line {
            done: self.done,
            total: self.total,
            rate: self.rate(),
            idle: self.now.saturating_sub(self.last_byte),
            elapsed: self.now.saturating_sub(self.start),
        }
    }

    /// Bytes so far.
    pub fn done(&self) -> u64 {
        self.done
    }

    /// The rate over the samples kept: from the oldest to now.
    fn rate(&self) -> Option<f64> {
        let (then, had) = self.samples[0];
        let span = self.now.saturating_sub(then);
        (span >= SAMPLE_EVERY).then(|| (self.done - had) as f64 / span.as_secs_f64())
    }
}

impl Line {
    /// Seconds left at this rate, when the total and a rate above nothing are known.
    pub fn left(&self) -> Option<Duration> {
        let (total, rate) = (self.total?, self.rate?);
        (rate > 0.0).then(|| {
            Duration::from_secs_f64((total.saturating_sub(self.done) as f64 / rate).min(1e7))
        })
    }

    /// The overall rate since the download began, for the line said at its end.
    pub fn average(&self) -> Option<f64> {
        (self.elapsed > Duration::ZERO).then(|| self.done as f64 / self.elapsed.as_secs_f64())
    }
}

/// `12.3 of 40.0 MB (31%), 2.1 MB/s, 13 s left`, or with no total `12.3 MB, 2.1 MB/s`, and `no
/// data for 8 s` when nothing has come for a while.
impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.total {
            Some(total) if total > 0 => {
                let percent = self.done.min(total) * 100 / total;
                // Both counts in the total's unit, the unit said once.
                let (per, unit) = Bytes(total).unit();
                match per {
                    1 => write!(f, "{} of {total} {unit} ({percent}%)", self.done)?,
                    _ => {
                        let (done, total) = (self.done as f64 / per as f64, total as f64 / per as f64);
                        write!(f, "{done:.1} of {total:.1} {unit} ({percent}%)")?;
                    }
                }
            }
            _ => write!(f, "{}", Bytes(self.done))?,
        }
        if self.idle >= QUIET_SAID {
            return write!(f, ", no data for {}", Secs(self.idle));
        }
        if let Some(rate) = self.rate {
            write!(f, ", {}/s", Bytes(rate as u64))?;
        }
        if let Some(left) = self.left() {
            write!(f, ", {} left", Secs(left))?;
        }
        Ok(())
    }
}

/// A count of bytes in SI units with one decimal past a kilobyte: `950 B`, `12.3 MB`.
#[derive(Clone, Copy, Debug)]
pub struct Bytes(pub u64);

impl Bytes {
    /// The unit this count is said in and how many bytes one of it is: `(1, "B")` under a
    /// kilobyte, then the largest of kB to TB the count does not round up past 999.9 of.
    fn unit(self) -> (u64, &'static str) {
        const UNITS: [&str; 4] = ["kB", "MB", "GB", "TB"];
        if self.0 < 1000 {
            return (1, "B");
        }
        let (mut per, mut unit) = (1000u64, 0);
        while self.0 as f64 / per as f64 >= 999.95 && unit + 1 < UNITS.len() {
            per *= 1000;
            unit += 1;
        }
        (per, UNITS[unit])
    }
}

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.unit() {
            (1, unit) => write!(f, "{} {unit}", self.0),
            (per, unit) => write!(f, "{:.1} {unit}", self.0 as f64 / per as f64),
        }
    }
}

/// A duration to the second, in minutes past two of them: `13 s`, `4 min 05 s`, `2 h 03 min`.
#[derive(Clone, Copy, Debug)]
pub struct Secs(pub Duration);

impl fmt::Display for Secs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs();
        match secs {
            0..120 => write!(f, "{secs} s"),
            120..7200 => write!(f, "{} min {:02} s", secs / 60, secs % 60),
            _ => write!(f, "{} h {:02} min", secs / 3600, secs / 60 % 60),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    /// A transcript on a clock the test moves: lines every `every`, a rate of the bytes over the
    /// last seconds, the time left from it, the quiet said after three seconds, and the stall
    /// said once at its bound and not before.
    #[test]
    fn a_meter_says_lines_on_time_and_the_stall_once() {
        let mut meter = Meter::new(Duration::ZERO, Some(40_000_000), LOG_EVERY, STALL);
        let mut said = Vec::new();
        for second in 1..=10u64 {
            meter.got(2_000_000);
            meter.advance_clock(S * second as u32);
            if let Some(it) = meter.take() {
                said.push((second, it));
            }
        }
        assert_eq!(said.len(), 2, "{said:?}");
        let Said::Line(line) = said[1].1 else { panic!("{said:?}") };
        assert_eq!(line.done, 20_000_000);
        assert_eq!(line.to_string(), "20.0 of 40.0 MB (50%), 2.0 MB/s, 10 s left");
        // The last byte came at second 9: the quiet is said, then the stall at 9 + 30 s.
        meter.advance_clock(S * 15);
        let quiet = meter.take();
        assert!(
            matches!(quiet, Some(Said::Line(l)) if l.to_string().ends_with("no data for 6 s")),
            "{quiet:?}"
        );
        assert_eq!(meter.next_deadline(), S * 20);
        meter.advance_clock(S * 38);
        assert!(matches!(meter.take(), Some(Said::Line(_))));
        meter.advance_clock(S * 39);
        assert_eq!(
            meter.take(),
            Some(Said::Stalled { idle: S * 30, done: 20_000_000, total: Some(40_000_000) })
        );
        meter.advance_clock(S * 40);
        assert_eq!(meter.take(), None, "the stall is said once");
        // A byte is progress: the stall's clock starts again from it.
        meter.got(1);
        assert_eq!(meter.next_deadline(), S * 43);
        assert_eq!(meter.line().idle, Duration::ZERO);
    }

    #[test]
    fn sizes_and_times_read_as_people_say_them() {
        let said = |n| Bytes(n).to_string();
        assert_eq!([said(950), said(1_000), said(12_345_678), said(999_960_000)], [
            "950 B", "1.0 kB", "12.3 MB", "1.0 GB"
        ]);
        let secs = |n| Secs(Duration::from_secs(n)).to_string();
        assert_eq!([secs(13), secs(245), secs(7_380)], ["13 s", "4 min 05 s", "2 h 03 min"]);
    }
}
