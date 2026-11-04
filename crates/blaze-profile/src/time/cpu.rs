//! A span's self CPU from the thread's CPU clock read at a few boundaries, never per span: the
//! sans-IO arithmetic the flight recorder ([`crate::flight`]) writes with and its reader reads
//! with, and the error it makes.
//!
//! # What is read when
//!
//! The recorder reads the thread's CPU time (`CLOCK_THREAD_CPUTIME_ID`, a system call of about
//! 100 ns) only at a *boundary*: a fiber switch (`switches.cpu_ns` holds the reading), a pool
//! worker going idle or waking, a wait span opening and closing, the thread entering a root
//! span after [`GAP`] in no span, a root span closing once the stretch is [`ROOT_AGE`] old, any
//! span closing once it is [`AGE`] old, the thread starting and ending, and a reader's read on
//! its own thread. Between two boundaries of a thread is a *stretch*: `W` ticks of the
//! recorder's counter ([`crate::clock`]) and `c` nanoseconds of CPU.
//!
//! Each span counts, in ticks, the time it is the innermost span its thread is inside (the
//! counter is read at each enter and exit, a few cycles, no system call). At the stretch's end
//! those ticks `L` become `c * L / W` nanoseconds ([`Rate::cpu_of`]); a span's self CPU is the
//! sum over the stretches it was innermost in. Ticks a span closed with before its stretch
//! ended are written in its row ([`share_word`]) and turned into nanoseconds by the reader at
//! the thread's next boundary row, so neither the extra row nor the two system calls a span
//! cost before (2026-11-04) are made.
//!
//! What it counts is what the per-span reads counted for every span that is entered while it
//! works (every span kind, `.entered()` and `in_scope` spans, held spans): the CPU while the
//! span is the innermost one its thread is inside, its job's alone, less the spans under it.
//! A span made and never entered counts 0, where the per-span reads gave it its job's CPU
//! while it was open less its children's: what `tracing` itself calls a span's busy time is
//! its time entered, and the held spans of a state machine already counted that way.
//!
//! # The error
//!
//! In a stretch whose off-CPU time is `O = W - c` (preempted, faulting, or blocked with no wait
//! span saying so), a span innermost for `L` of it had true CPU `L - o` for some `o` in
//! `[0, min(L, O)]`, and is given `c * L / W = L - L * O / W`, also `L` less something in
//! `[0, min(L, O)]`. So for every span
//!
//! ```text
//! |derived - true| <= sum over the stretches s it was innermost in of min(L_s, O_s)
//!                     + 1 tick for each enter and exit it was innermost across
//!                     + 1 ns for each stretch it was innermost in (rounding down)
//!                     + dur / 2^23 (its row's share of its last stretch, [`SHARE_ONE`])
//! ```
//!
//! A tick is 41.7 ns on Apple's 24 MHz counter, about 0.3 ns on an x86 TSC and 1 ns on
//! Neoverse. A stretch fully on CPU (`O = 0`) gives each span its CPU to the roundings. Waits,
//! switches and the gaps between root spans are boundaries, so the off-CPU time left inside a
//! stretch is preemption, page faults and blocking under no wait span, which rule
//! every-wait-says-what already names as a hole. Two cases fall outside the stretch a span's
//! ticks were counted in, each bounded by those ticks: a span left on one thread and closed on
//! another before the first thread's next boundary (its ticks take the closing thread's rate),
//! and the slice between a parking job's switch and its spans leaving the thread, under
//! [`SLIVER`], which takes the rate of the stretch the switch ended.

use std::time::Duration;

/// A thread's CPU stretch is cut at a span's close once it is this old: at most 4,000 reads a
/// second a thread (1.2 ns a span at 360,000 spans a second), and a row waits at most this long
/// for the rate that resolves it.
pub const AGE: Duration = Duration::from_micros(250);

/// A root span closing once the stretch is this old cuts it, so a thread that blocks after its
/// work with no span (between requests, on a channel) leaves no off-CPU time in the stretch of
/// its last spans; under it, the error is bounded by those spans' ticks.
pub const ROOT_AGE: Duration = Duration::from_micros(20);

/// A thread entering a root span after this long in none cuts the stretch, so the time it spent
/// outside every span (blocked or not) is a stretch of its own.
pub const GAP: Duration = Duration::from_micros(10);

/// A parking job's spans leave their thread this soon after the switch's boundary: the ticks
/// counted in between take that boundary's rate instead of a read of their own.
pub const SLIVER: Duration = Duration::from_micros(2);

/// The thread's CPU clock and the recorder's counter, read together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    pub ticks: u64,
    pub cpu_ns: u64,
}

/// A stretch's CPU over its wall: what turns the ticks a span was innermost in it into CPU
/// nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rate {
    ticks: u64,
    cpu_ns: u64,
}

impl Rate {
    /// The rate of the stretch from `from` to `to`: `None` when the counter did not move or the
    /// CPU clock went back (two threads' readings).
    pub fn between(from: Reading, to: Reading) -> Option<Rate> {
        let ticks = to.ticks.checked_sub(from.ticks).filter(|it| *it > 0)?;
        let cpu_ns = to.cpu_ns.checked_sub(from.cpu_ns)?;
        Some(Rate { ticks, cpu_ns })
    }

    /// The stretch's ticks and CPU nanoseconds: [`Rate::between`] a zero reading and these
    /// gives it back.
    pub fn parts(self) -> (u64, u64) {
        (self.ticks, self.cpu_ns)
    }

    /// The CPU nanoseconds of `ticks` ticks of the stretch, rounded down.
    pub fn cpu_of(self, ticks: u64) -> u64 {
        let ns = u128::from(ticks) * u128::from(self.cpu_ns) / u128::from(self.ticks);
        u64::try_from(ns).unwrap_or(u64::MAX)
    }
}

/// Bits of a span row's share word ([`share_word`]).
pub const SHARE_BITS: u32 = 24;
/// The share word of a span whose CPU is counted and has no ticks waiting for a rate.
pub const SHARE_COUNTED: u64 = 1;
const SHARE_FIRST: u64 = 2;
/// The unit of a share: a span's ticks waiting for a rate, in parts of its duration.
pub const SHARE_ONE: u64 = (1 << SHARE_BITS) - 1 - SHARE_FIRST;

/// The word a span's row holds for `pending` ticks it was innermost in the stretch it closed
/// in, of its `dur` ticks: [`SHARE_COUNTED`] for none, else the ticks in [`SHARE_ONE`]ths of
/// its duration, so 24 bits hold any span's to within `dur / 2^24`. 0 says its CPU is not
/// counted, and is never this.
pub fn share_word(pending: u64, dur: u64) -> u64 {
    if pending == 0 {
        return SHARE_COUNTED;
    }
    // A span is innermost only while it is open; a counter read out of order is clamped.
    let dur = dur.max(pending);
    let share =
        (u128::from(pending) * u128::from(SHARE_ONE) + u128::from(dur / 2)) / u128::from(dur);
    SHARE_FIRST + share as u64
}

/// What a span row's share word says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Share {
    /// Its CPU is not counted: the binary reads no CPU clock, or a read failed.
    Uncounted,
    /// Counted, with `pending` ticks waiting for the rate of its thread's next boundary.
    Counted { pending: u64 },
}

/// The share `word` of a span of `dur` ticks ([`share_word`]).
pub fn share_of(word: u64, dur: u64) -> Share {
    match word {
        0 => Share::Uncounted,
        SHARE_COUNTED => Share::Counted { pending: 0 },
        _ => {
            let parts = (word - SHARE_FIRST).min(SHARE_ONE);
            let pending = (u128::from(parts) * u128::from(dur) + u128::from(SHARE_ONE / 2))
                / u128::from(SHARE_ONE);
            Share::Counted { pending: pending as u64 }
        }
    }
}

#[cfg(test)]
mod tests {
    use hegel::{TestCase, generators as gs};

    use super::*;

    #[hegel::test(test_cases = 300)]
    fn a_share_reads_back_within_a_part_of_its_spans_duration(tc: TestCase) {
        let dur = tc.draw(gs::integers::<u64>().min_value(1).max_value(1 << 50));
        let pending = tc.draw(gs::integers::<u64>().max_value(dur));
        let word = share_word(pending, dur);
        assert!(
            word != 0 && word < 1 << SHARE_BITS,
            "{word} is a counted share of {SHARE_BITS} bits"
        );
        let Share::Counted { pending: read } = share_of(word, dur) else {
            panic!("{word} reads as counted")
        };
        assert!(
            read.abs_diff(pending) <= dur / SHARE_ONE + 1,
            "{pending} of {dur} read back as {read}"
        );
    }

    #[hegel::test(test_cases = 300)]
    fn a_stretchs_rate_gives_its_whole_cpu_to_its_whole_wall(tc: TestCase) {
        let from = Reading {
            ticks: tc.draw(gs::integers::<u64>().max_value(1 << 60)),
            cpu_ns: tc.draw(gs::integers::<u64>().max_value(1 << 60)),
        };
        let wall = tc.draw(gs::integers::<u64>().min_value(1).max_value(1 << 40));
        let cpu = tc.draw(gs::integers::<u64>().max_value(1 << 40));
        let rate =
            Rate::between(from, Reading { ticks: from.ticks + wall, cpu_ns: from.cpu_ns + cpu })
                .expect("a stretch");
        assert_eq!(rate.cpu_of(wall), cpu);
        let part = tc.draw(gs::integers::<u64>().max_value(wall));
        assert!(rate.cpu_of(part) <= cpu, "a part of the stretch has at most its CPU");
        assert_eq!(Rate::between(from, from), None, "a stretch of no ticks has no rate");
    }

    /// A thread's synthetic timeline, in units of [`UNIT`] ticks (a tick a nanosecond): the CPU
    /// it had in each, spans entered and left at unit edges, and boundaries at unit edges.
    struct Timeline {
        cpu: Vec<u64>,
        /// At each unit's start: 0 nothing, 1 enter a span, 2 leave the innermost.
        step: Vec<u8>,
        /// Whether a boundary reads the clock at each unit's start.
        boundary: Vec<bool>,
    }

    const UNIT: u64 = 1_000;

    fn timeline(tc: &TestCase, on_cpu: bool) -> Timeline {
        let units = tc.draw(gs::integers::<usize>().min_value(1).max_value(160));
        let mut it = Timeline { cpu: Vec::new(), step: Vec::new(), boundary: Vec::new() };
        for _ in 0..units {
            it.cpu.push(if on_cpu { UNIT } else { tc.draw(gs::integers::<u64>().max_value(UNIT)) });
            it.step.push(tc.draw(gs::integers::<u8>().max_value(2)));
            it.boundary.push(tc.draw(gs::integers::<u8>().max_value(5)) == 0);
        }
        it
    }

    /// A span of the timeline: what the per-span reads gave it (its CPU while innermost), what
    /// the stretches give it, and the bound on their difference.
    #[derive(Debug, Default, Clone)]
    struct Measured {
        start: u64,
        end: u64,
        exact: u64,
        known: u64,
        /// Ticks innermost in the stretch now, and in each stretch so far (for the bound).
        pending: u64,
        per_stretch: Vec<(usize, u64)>,
        /// Closed with `pending` ticks the row's share holds, resolved at the next boundary.
        closed_pending: Option<u64>,
        derived: Option<u64>,
    }

    /// The recorder's arithmetic over `line`: ticks innermost counted per stretch, resolved at
    /// each boundary with the stretch's [`Rate`]; a span closed mid-stretch through its row's
    /// share word, resolved at the thread's next boundary as the reader does.
    fn run(line: &Timeline) -> (Vec<Measured>, Vec<(u64, u64)>) {
        let mut spans: Vec<Measured> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        // Each stretch's wall and CPU.
        let mut stretches: Vec<(u64, u64)> = Vec::new();
        let mut last = Reading { ticks: 0, cpu_ns: 0 };
        let mut cpu_ns = 0;
        let resolve = |spans: &mut Vec<Measured>, stack: &[usize], rate: Rate, stretch: usize| {
            for (i, span) in spans.iter_mut().enumerate() {
                if let Some(pending) = span.closed_pending.take() {
                    span.derived = Some(span.known + rate.cpu_of(pending));
                } else if stack.contains(&i) && span.pending > 0 {
                    span.known += rate.cpu_of(span.pending);
                }
                if span.pending > 0 {
                    span.per_stretch.push((stretch, span.pending));
                    span.pending = 0;
                }
            }
        };
        let units = line.cpu.len();
        for unit in 0..=units {
            let now = unit as u64 * UNIT;
            let closing = unit == units;
            if closing {
                while let Some(top) = stack.pop() {
                    close(&mut spans[top], now);
                }
            } else {
                match line.step[unit] {
                    1 => {
                        spans.push(Measured { start: now, ..Measured::default() });
                        stack.push(spans.len() - 1);
                    }
                    2 => {
                        if let Some(top) = stack.pop() {
                            close(&mut spans[top], now);
                        }
                    }
                    _ => {}
                }
            }
            if closing || line.boundary[unit] {
                let reading = Reading { ticks: now, cpu_ns };
                if let Some(rate) = Rate::between(last, reading) {
                    stretches.push((reading.ticks - last.ticks, reading.cpu_ns - last.cpu_ns));
                    resolve(&mut spans, &stack, rate, stretches.len() - 1);
                }
                last = reading;
            }
            if closing {
                break;
            }
            // The unit runs: its CPU is the innermost span's, and the counter is the clock.
            cpu_ns += line.cpu[unit];
            if let Some(&top) = stack.last() {
                spans[top].exact += line.cpu[unit];
                spans[top].pending += UNIT;
            }
        }
        (spans, stretches)
    }

    fn close(span: &mut Measured, now: u64) {
        span.end = now;
        let dur = now - span.start;
        span.closed_pending = Some(match share_of(share_word(span.pending, dur), dur) {
            Share::Counted { pending } => pending,
            Share::Uncounted => unreachable!("a share word is never the uncounted one"),
        });
    }

    fn check(line: &Timeline) {
        let (spans, stretches) = run(line);
        for span in &spans {
            let derived = span.derived.expect("the last boundary resolves every span");
            let off: u64 = span
                .per_stretch
                .iter()
                .map(|&(s, ticks)| ticks.min(stretches[s].0 - stretches[s].1))
                .sum();
            let rounding = span.per_stretch.len() as u64 + (span.end - span.start) / SHARE_ONE + 1;
            assert!(
                derived.abs_diff(span.exact) <= off + rounding,
                "derived {derived}, per-span reads {}: off by more than {off} off-CPU and {rounding} rounding: {span:?}",
                span.exact
            );
        }
    }

    /// What the stretches give each span is what reading the clock at its every enter and exit
    /// gave it, to within its stretches' off-CPU time (min of its ticks and theirs) and the
    /// roundings, however the boundaries fall.
    #[hegel::test(test_cases = 300)]
    fn self_cpu_from_stretches_is_the_per_span_reads_within_the_bound(tc: TestCase) {
        check(&timeline(&tc, false));
    }

    /// On a thread that never leaves the CPU the bound is the roundings alone.
    #[hegel::test(test_cases = 200)]
    fn a_thread_always_on_cpu_gets_each_spans_cpu_to_the_roundings(tc: TestCase) {
        check(&timeline(&tc, true));
    }
}
