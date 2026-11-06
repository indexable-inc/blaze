//! The Linux sampler: `perf_event_open`, no signals. For each thread that holds a ring, the
//! sampler's thread opens one event on it (blaze's one perf reader, [`ix_prof::perf`]), its
//! task clock with switch records, and reads its mapped ring every [`DRAIN_EVERY`]:
//!
//! - on the CPU: `hz` samples a CPU second, user call chains the kernel walks by frame pointer;
//! - off it: a `PERF_RECORD_SWITCH` at every switch out and in. A switch out that blocked,
//!   inside a span, becomes off-CPU samples at `hz` over the time until it was switched back in
//!   (each thread's fraction of a period carried to its next block, so short blocks are
//!   counted unbiased), paid up to each read while it stays blocked. A preemption leaves the
//!   thread runnable: no off-CPU sample, and the task clock counts it again once it runs.
//!
//! Off-CPU samples have no stack on Linux (depth 0): the kernel takes context-switch samples in
//! the scheduler, in kernel mode, and drops every one of an event that excludes the kernel,
//! while an event that counts the kernel needs `kernel.perf_event_paranoid` 1 or less, or
//! `CAP_PERFMON`. The sampler needs neither (Fedora's default and the ix VMs' are 2), so it
//! reads only the switch records, which carry the time and whether it was a preemption. What
//! a block is blamed on is its span: the raw-block gate (`raw_blocks`, sql/examples.sql) asks
//! only whether a `wait` span is above it. macOS stops the thread and so has the stack it
//! blocked in (`macos.rs`).
//!
//! Every sample is stamped on `CLOCK_MONOTONIC` (`use_clockid`), turned into the flight
//! recorder's ticks ([`crate::clock::Scale::ticks_at`]), and told the span, job and fiber its
//! thread was inside at that moment by its ring's context log
//! ([`crate::flight::Ring::context_at`]), which the thread writes at every change of its
//! innermost span, job or fiber.
//!
//! Why perf works now: it was dropped on 2026-11-04 because its samples arrive after the fact
//! and a ring could only say what its thread was inside when the sampler read it, so every
//! sample read span 0; the context log answers for the moment of the sample instead. The
//! realtime-signal sampler that replaced it then interrupted every thread that records spans
//! with SIGRTMIN+6 about every 20 ms, and a kernel wait that restarted its relative timeout on
//! each interruption never ended (the `blaze serve` hang of 2026-11-05,
//! docs/v2/plan/design/deadline-waits.md): nothing signals a thread now.
//!
//! There is one path: where the kernel refuses the event (`kernel.perf_event_paranoid` above 2
//! without `CAP_PERFMON`, or a seccomp filter), [`check`] says so, with its errno, and the
//! process takes no samples.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use blaze_core::wait::{Clock as _, Deadline, Monotonic};
use ix_prof::perf::{Counter, Event, Record};
use blaze_core::lock::Lock;

use super::{DEPTH, Sample, Unsampled, keep};
use crate::{clock::Scale, flight::Ring};

pub(super) const HOW: &str = "perf_event_open on each thread that records spans (its task clock on the CPU with user stacks by frame pointer, its switch records off it, blocked samples with no stack), each sample's span, job and fiber from its ring's context log";

/// How often the sampler reads the events' rings: well inside a ring's 64 KiB at the deepest
/// stacks, and inside the time a busy thread takes to write its context log over.
const DRAIN_EVERY: Duration = Duration::from_millis(20);
/// How often the samples taken are kept.
const KEEP_EVERY: Duration = Duration::from_millis(100);
/// Pages of records an event's ring holds after its header page: 64 KiB.
const DATA_PAGES: usize = 16;

/// Samples older than their ring's context log (or whose entries were written over as they
/// were read), so read as span 0; records the kernel lost to a full ring; threads whose events
/// could not be opened (a file limit): what `taken` says beside the sampler's state.
static OLDER: AtomicU64 = AtomicU64::new(0);
static LOST: AtomicU64 = AtomicU64::new(0);
static UNOPENED: AtomicU64 = AtomicU64::new(0);

/// What a ring holds for the sampler (`Ring::sampled`), under the lock its thread takes to
/// enter and leave: which thread holds it, and the events the sampler opened on that thread.
/// The thread only says who it is; the sampler opens, reads and closes the events.
#[derive(Default)]
pub(crate) struct Thread {
    tid: u64,
    live: bool,
    events: Option<Events>,
    /// A thread whose events the kernel would not open, not asked again.
    refused: u64,
}

impl Thread {
    /// On the thread itself, as it takes a ring.
    pub(crate) fn enter(&mut self, tid: u64) {
        self.tid = tid;
        self.live = true;
    }

    /// On the thread itself, as it gives its ring back (its thread-locals ending): the
    /// sampler reads what its events still hold and closes them on its next read.
    pub(crate) fn leave(&mut self) {
        self.live = false;
    }
}

/// The event on one thread, and what its switches left owed.
struct Events {
    tid: u64,
    clock: Event,
    switched: Switches,
}

/// The one counter: the task clock, with switch records.
const fn counter(hz: u32) -> Counter {
    Counter::TaskClock { hz, switches: true }
}

impl Events {
    fn open(tid: u64, hz: u32) -> Result<Events, std::io::Error> {
        let clock = Event::open(tid as libc::pid_t, counter(hz), DEPTH as u16, DATA_PAGES)?;
        Ok(Events { tid, clock, switched: Switches::default() })
    }
}

/// A stretch the thread spent blocked inside a span, owed as off-CPU samples until it is
/// switched back in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Owed {
    /// Samples are owed from here on, in `CLOCK_MONOTONIC` nanoseconds: the switch out, then
    /// the end of what was paid.
    from_ns: u64,
    span: u64,
    job: u64,
    fiber: u32,
}

/// One thread's switches, a core with no IO: what blocked stretch is owed, and the fraction of
/// a period carried past its last sample.
#[derive(Default)]
struct Switches {
    owed: Option<Owed>,
    carry_ns: u64,
}

/// The samples `ns` more nanoseconds owe at one each `period_ns`, with `carry_ns` left over
/// from before: how many, and the nanoseconds left over after. A block shorter than a period
/// pays nothing now and carries its time, so many short blocks pay what one long one would.
fn weigh(ns: u64, period_ns: u64, carry_ns: u64) -> (u64, u64) {
    let total = carry_ns.saturating_add(ns);
    (total / period_ns.max(1), total % period_ns.max(1))
}

impl Switches {
    /// Pays what is owed up to `to_ns`: a sample each period, each at its moment in the
    /// stretch, by `each` (its time, and the span, job and fiber it blocked in).
    fn pay(&mut self, to_ns: u64, period_ns: u64, each: &mut dyn FnMut(u64, &Owed)) {
        let Some(owed) = self.owed.as_mut() else { return };
        let Some(ns) = to_ns.checked_sub(owed.from_ns) else { return };
        let period = period_ns.max(1);
        let (count, carry) = weigh(ns, period, self.carry_ns);
        // The first sample falls where the carried fraction makes a whole period.
        let first = owed.from_ns + (period - self.carry_ns.min(period));
        for k in 0..count {
            each(first + k * period, owed);
        }
        self.carry_ns = carry;
        owed.from_ns = to_ns;
    }
}

/// `CLOCK_MONOTONIC` nanoseconds, the samples' times, turned into instants (std's `Instant`
/// counts the same clock on Linux) and into the flight recorder's ticks.
struct Clock {
    now: Instant,
    now_ns: u64,
    scale: Scale,
}

impl Clock {
    fn now() -> Clock {
        let scale = Scale::now();
        Clock { now: Instant::now(), now_ns: crate::clock::mono_ns() as u64, scale }
    }

    fn instant(&self, ns: u64) -> Instant {
        self.now
            .checked_sub(Duration::from_nanos(self.now_ns.saturating_sub(ns)))
            .unwrap_or(self.now)
    }

    fn ticks(&self, ns: u64) -> u64 {
        self.scale.ticks_at(ns as i64)
    }
}

/// The span, job and fiber `ring`'s thread was inside at `ns`, kept; all 0, counted, when the
/// context log no longer reaches back that far.
fn context(ring: &Ring, clock: &Clock, ns: u64) -> (u64, u64, u32) {
    ring.context_at(clock.ticks(ns)).unwrap_or_else(|| {
        OLDER.fetch_add(1, Ordering::Relaxed);
        (0, 0, 0)
    })
}

/// Reads one thread's event into `batch`, and pays what its switches owe up to `to_ns`
/// (`None`: its thread is gone, nothing more is owed).
fn drain(
    events: &mut Events,
    ring: &Ring,
    clock: &Clock,
    period_ns: u64,
    to_ns: Option<u64>,
    batch: &mut Vec<Sample>,
) {
    let tid = events.tid;
    // A sample on the CPU has its stack; one off it has none (depth 0: the header says why).
    let mut sample = |at_ns: u64, on_cpu: bool, (span, job, fiber): (u64, u64, u32), pcs: &[u64]| {
        let depth = pcs.len().min(DEPTH);
        if on_cpu && depth == 0 {
            return;
        }
        let mut it = Sample {
            at: clock.instant(at_ns),
            tid,
            depth: depth as u8,
            pcs: [0; DEPTH],
            span,
            build: crate::flight::build_of(span),
            on_cpu,
            job,
            fiber,
        };
        it.pcs[..depth].copy_from_slice(&pcs[..depth]);
        batch.push(it);
    };
    let switched = &mut events.switched;
    events.clock.drain(&mut |record| match record {
        Record::Sample { time, chain, .. } => {
            let mut pcs = [0u64; DEPTH];
            let depth = chain.pcs(&mut pcs);
            sample(time, true, context(ring, clock, time), &pcs[..depth]);
        }
        Record::Switch { time, out: true, preempt, .. } => {
            // A preemption leaves the thread runnable, not blocked: nothing is owed. A block
            // outside every span is not sampled, as on macOS: no span to blame its wait on.
            switched.owed = None;
            if !preempt {
                let (span, job, fiber) = context(ring, clock, time);
                switched.owed = (span != 0).then_some(Owed { from_ns: time, span, job, fiber });
            }
        }
        Record::Switch { time, out: false, .. } => {
            switched.pay(time, period_ns, &mut |at, owed| {
                sample(at, false, (owed.span, owed.job, owed.fiber), &[]);
            });
            switched.owed = None;
        }
        Record::Lost { lost } => _ = LOST.fetch_add(lost, Ordering::Relaxed),
    });
    match to_ns {
        // Still blocked as the ring was read: paid up to now, the rest owed.
        Some(to) => switched.pay(to, period_ns, &mut |at, owed| {
            sample(at, false, (owed.span, owed.job, owed.fiber), &[]);
        }),
        None => switched.owed = None,
    }
}

/// Whether the kernel opens the event on this thread, with what it says when it does not:
/// the one way Linux samples, no other is tried.
pub(super) fn check() -> Result<(), Unsampled> {
    // SAFETY: `gettid` has no precondition.
    let tid = unsafe { libc::gettid() } as u64;
    Events::open(tid, super::HZ).map(drop).map_err(|why| {
        let paranoid =
            std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid").unwrap_or_default();
        // alloc: an error's message
        Unsampled(format!(
            "perf_event_open of this thread's task clock with switch records refused: {why} (errno {}; kernel.perf_event_paranoid = {}; it needs {}; a seccomp filter refusing perf_event_open says EPERM or ENOSYS)",
            why.raw_os_error().unwrap_or(0),
            paranoid.trim(),
            counter(super::HZ).needs()
        ))
    })
}

/// The sampler's loop, for the process's life: every [`DRAIN_EVERY`], open the events of each
/// thread that took a ring since, read every ring's events, and close those of threads that
/// left.
pub(super) fn run(hz: u32) {
    let period_ns = 1_000_000_000 / u64::from(hz.max(1));
    // alloc: the sampler's thread, at the edge, once a process
    let mut rings = Vec::new();
    // alloc: as above
    let mut batch: Vec<Sample> = Vec::with_capacity(4096);
    let mut kept = Instant::now();
    let ticks = Monotonic::new();
    let mut next = ticks.now();
    loop {
        next += DRAIN_EVERY;
        // Read before the rings: a switch in before it is in them, so nothing is paid twice.
        let clock = Clock::now();
        crate::flight::rings(&mut rings);
        for ring in &rings {
            // Held while the events are read, so the ring's thread cannot leave (and another
            // take the ring) in between; the thread takes it only to enter and leave.
            let mut guard = ring.sampled.lock();
            let thread = &mut *guard;
            if let Some(mut events) = thread.events.take() {
                let gone = !thread.live || events.tid != thread.tid;
                let to = (!gone).then_some(clock.now_ns);
                drain(&mut events, ring, &clock, period_ns, to, &mut batch);
                if !gone {
                    thread.events = Some(events);
                }
            }
            if thread.live && thread.events.is_none() && thread.refused != thread.tid {
                match Events::open(thread.tid, hz) {
                    Ok(events) => thread.events = Some(events),
                    Err(_) => {
                        // A file limit, or a thread that ended as it was opened: counted,
                        // never asked again for this thread.
                        UNOPENED.fetch_add(1, Ordering::Relaxed);
                        thread.refused = thread.tid;
                    }
                }
            }
        }
        if kept.elapsed() >= KEEP_EVERY || batch.len() >= batch.capacity() / 2 {
            keep(&batch);
            batch.clear();
            kept = Instant::now();
        }
        // An absolute deadline per tick: a slow read shortens the next wait, never moves the
        // schedule. A tick that fell behind starts the next at once, with no burst to catch up.
        next = next.max(ticks.now());
        if let Err(why) = blaze_core::wait::sleep_until(
            "the sampler's next read of its threads' perf rings",
            Deadline::at(next),
            &ticks,
        ) {
            // No other timer to fall back on: the sampler stops, and every dump says why.
            keep(&batch);
            // alloc: once a process, as the sampler stops
            *STOPPED.lock() = Some(why.to_string());
            return;
        }
    }
}

/// Why the sampler's loop ended, if it did: its tick's wait failed. Said in every dump after.
static STOPPED: Lock<Option<String>> = Lock::unrecorded("profile.sampler.stopped", None);

/// What the sampler counted that a dump should say: samples older than their context log,
/// records lost, threads not sampled. Empty when all are 0.
pub(super) fn said() -> String {
    let (older, lost, unopened) = (
        OLDER.load(Ordering::Relaxed),
        LOST.load(Ordering::Relaxed),
        UNOPENED.load(Ordering::Relaxed),
    );
    let stopped = STOPPED.lock().as_ref().map_or_else(String::new, |why| {
        // alloc: a dump, on demand
        format!("; stopped: {why}")
    });
    if older == 0 && lost == 0 && unopened == 0 {
        return stopped;
    }
    // alloc: a dump, on demand
    format!(
        "; {older} samples older than their thread's context log (span 0), {lost} perf records lost to a full ring, {unopened} threads whose perf event did not open (not sampled){stopped}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block pays a sample a period, the fraction left carried to the next: blocks shorter
    /// than a period add up to what one long block of their sum would pay.
    #[test]
    fn off_cpu_samples_weigh_blocks_by_their_length_carrying_the_fraction() {
        assert_eq!(weigh(0, 10, 0), (0, 0));
        assert_eq!(weigh(25, 10, 0), (2, 5));
        assert_eq!(weigh(5, 10, 5), (1, 0), "the carried half and this half make one");
        let (mut paid, mut carry) = (0, 0);
        for _ in 0..1000 {
            let (n, c) = weigh(3, 10, carry);
            paid += n;
            carry = c;
        }
        assert_eq!((paid, carry), weigh(3000, 10, 0), "a thousand 3 ns blocks pay as 3 us does");

        // Paid in pieces up to now and then at the switch in: the samples sit a period apart
        // across the stretch, as many as one payment would make.
        let mut switches = Switches {
            owed: Some(Owed { from_ns: 100, span: 9, job: 1, fiber: 2 }),
            carry_ns: 4,
        };
        // alloc: a test's samples
        let mut at = Vec::new();
        switches.pay(120, 10, &mut |t, owed| at.push((t, owed.span, owed.fiber)));
        switches.pay(137, 10, &mut |t, owed| at.push((t, owed.span, owed.fiber)));
        assert_eq!(at, [(106, 9, 2), (116, 9, 2), (126, 9, 2), (136, 9, 2)]);
        assert_eq!(switches.carry_ns, 1);
    }

    /// A sample taken while a thread spins inside a named span names that span: the property
    /// perf alone lacked (every sample read span 0), from the context log.
    #[test]
    fn a_sample_inside_a_span_names_it() {
        crate::install();
        if super::super::start(super::super::HZ * 20).is_err() {
            // The kernel refused perf (the error says why, and so do dumps): nothing to check.
            return;
        }
        #[inline(never)]
        fn spins_in_a_span(until: Instant) -> u64 {
            let mut n = 0u64;
            while Instant::now() < until {
                n = std::hint::black_box(n.wrapping_add(1));
            }
            n
        }
        let since = Instant::now();
        let (span, tid) = std::thread::Builder::new()
            // alloc: a test's thread name
            .name("span-spinner".to_owned())
            .spawn(|| {
                let entered = tracing::info_span!(target: "blaze", "spinning in a span").entered();
                let ring = crate::flight::ring().expect("a thread that records spans has a ring");
                let span = ring.innermost();
                spins_in_a_span(Instant::now() + Duration::from_millis(1500));
                drop(entered);
                (span, super::super::os_tid())
            })
            .unwrap()
            .join()
            .unwrap();
        assert_ne!(span, 0, "the span was recorded");
        let mut named = false;
        blaze_graph::spin::SpinWait::within(
            "the sampler to keep a sample of the spinner inside its span",
            Duration::from_secs(10),
            || {
                // alloc: a test's copy of the samples
                let mut kept = Vec::new();
                let mut next = 0;
                super::super::process().since(&mut next, &mut kept);
                named = kept.iter().any(|it| it.tid == tid && it.at >= since && it.span == span);
                named
            },
        );
        assert!(named, "no sample of the spinner named the span {span:#x} it spun in");
    }
}
