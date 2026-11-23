//! What a span costs: ns per span (open, enter, exit, close) at 1 and 8 threads, for blaze_trace
//! with nothing recording, blaze_trace recording into its rings, and the third-party `tracing`
//! with no subscriber, with a subscriber that keeps nothing and with a subscriber writing the
//! same two entries into the same rings.
//!
//! `cargo run --release -p blaze-trace --example cost`. `tracing` is a dev-dependency of this
//! crate only for this comparison; it goes once the numbers are in docs/profiling.md.
//!
//! The rings are blaze_trace's: per thread, 32-byte slots (stamp, site and depth, tick, span),
//! overwrite-oldest with an exact lost count, the tick read from the CPU counter in user space
//! (`cntvct_el0` on arm64, `rdtsc` on x86-64). A span is two entries: one at enter, one at
//! exit. The floor line times that alone: two counter reads and two slot writes.

#![feature(allocator_api)]

use std::{
    hint::black_box,
    sync::{Arc, Barrier, atomic::AtomicU64},
    time::Instant,
};

use blaze_trace::record::{self, Rings};

const SPANS: u64 = 2_000_000;

/// One edge of a span written straight into the thread's ring: the floor.
#[inline(always)]
fn edge(site: u64, began: bool) {
    black_box(record::emit([0x80 | u64::from(began) | site << 32, record::counter(), site]));
}

/// `tracing` with a subscriber that is interested in everything and keeps nothing.
struct TheirsNothing;

impl tracing::Subscriber for TheirsNothing {
    fn register_callsite(&self, _: &'static tracing::Metadata<'static>) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::always()
    }

    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, _: &tracing::Event<'_>) {}

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// `tracing` into the same ring as [`OursRing`].
struct TheirsRing;

impl tracing::Subscriber for TheirsRing {
    fn register_callsite(&self, _: &'static tracing::Metadata<'static>) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::always()
    }

    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(core::ptr::from_ref(span.metadata()).addr() as u64)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, _: &tracing::Event<'_>) {}

    fn enter(&self, span: &tracing::span::Id) {
        edge(span.into_u64(), true);
    }

    fn exit(&self, span: &tracing::span::Id) {
        edge(span.into_u64(), false);
    }

    fn try_close(&self, _: tracing::span::Id) -> bool {
        true
    }
}

#[derive(Clone, Copy)]
enum Case {
    OursOff,
    OursRing,
    TheirsNone,
    TheirsNothing,
    TheirsRing,
}

impl Case {
    fn name(self) -> &'static str {
        match self {
            Case::OursOff => "blaze_trace, no sink",
            Case::OursRing => "blaze_trace, ring",
            Case::TheirsNone => "tracing, no subscriber",
            Case::TheirsNothing => "tracing, subscriber keeping nothing",
            Case::TheirsRing => "tracing, ring",
        }
    }
}

/// Each case's loop has its own callsite, so no case's cached interest is another's.
fn ours_loop() {
    for i in 0..SPANS {
        let _span = blaze_trace::info_span!("cost").entered();
        black_box(i);
    }
}

fn ours_ring_loop() {
    for i in 0..SPANS {
        let _span = blaze_trace::info_span!("cost_ring").entered();
        black_box(i);
    }
}

fn theirs_loop() {
    for i in 0..SPANS {
        let _span = tracing::info_span!("cost").entered();
        black_box(i);
    }
}

fn theirs_ring_loop() {
    for i in 0..SPANS {
        let _span = tracing::info_span!("cost_ring").entered();
        black_box(i);
    }
}

fn theirs_nothing_loop() {
    for i in 0..SPANS {
        let _span = tracing::info_span!("cost_nothing").entered();
        black_box(i);
    }
}

/// ns per span on this thread.
fn one(case: Case) -> f64 {
    let start = Instant::now();
    match case {
        Case::OursOff => ours_loop(),
        Case::OursRing => ours_ring_loop(),
        Case::TheirsNone => theirs_loop(),
        Case::TheirsNothing => tracing::subscriber::with_default(TheirsNothing, theirs_nothing_loop),
        Case::TheirsRing => tracing::subscriber::with_default(TheirsRing, theirs_ring_loop),
    }
    let ns = start.elapsed().as_nanos() as f64 / SPANS as f64;
    ns
}

/// The mean ns per span over `threads` threads running `case` at once.
fn run(case: Case, threads: usize) -> f64 {
    let gate = Arc::new(Barrier::new(threads));
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                gate.wait();
                one(case)
            })
        })
        .collect();
    let all: Vec<f64> = handles.into_iter().map(|it| it.join().expect("a cost thread panicked")).collect();
    all.iter().sum::<f64>() / all.len() as f64
}

/// ns per iteration of `body`, run `SPANS` times on this thread.
#[inline(always)]
fn per(mut body: impl FnMut(u64)) -> f64 {
    let start = Instant::now();
    for i in 0..SPANS {
        body(black_box(i));
    }
    start.elapsed().as_nanos() as f64 / SPANS as f64
}

/// The disabled path's intended cost: a static word, loaded relaxed, and a branch.
static FLAG: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

static LADDER_META: blaze_trace::Metadata<'static> = blaze_trace::Metadata::new(
    "ladder_site",
    "cost",
    blaze_trace::Level::INFO,
    Some(file!()),
    Some(line!()),
    Some(module_path!()),
    blaze_trace::field::FieldSet::new(&[]),
    blaze_trace::Kind::SPAN,
);
static LADDER_SITE: blaze_trace::Site = blaze_trace::Site::new(&LADDER_META);

/// The ladder: each line one step more than the last, on one thread, with its difference from
/// the line before. Its first part runs before the rings are installed (nothing records).
fn ladder_off(lines: &mut Vec<(&'static str, f64)>) {
    use core::sync::atomic::Ordering::Relaxed;
    lines.push(("(a) empty loop", per(|i| _ = black_box(i))));
    lines.push((
        "(b) static AtomicU32 load + branch",
        per(|i| {
            if black_box(&FLAG).load(Relaxed) & 3 != 0 {
                black_box(i);
            }
        }),
    ));
    lines.push(("(b2) Site::how alone, nothing records", per(|_| _ = black_box(LADDER_SITE.how().is_some()))));
    lines.push((
        "(c0) Span::none() made and dropped",
        per(|i| {
            let span = black_box(blaze_trace::Span::none());
            drop(span);
            black_box(i);
        }),
    ));
    lines.push((
        "(c1) the disabled macro, not entered",
        per(|i| {
            let _span = blaze_trace::info_span!("ladder_off_open");
            black_box(i);
        }),
    ));
    lines.push((
        "(c) the disabled macro, entered",
        per(|i| {
            let _span = blaze_trace::info_span!("ladder_off").entered();
            black_box(i);
        }),
    ));
    lines.push(("(d) one counter read", per(|_| _ = black_box(record::counter()))));
    lines.push((
        "(e) two counter reads",
        per(|_| {
            black_box(record::counter());
            black_box(record::counter());
        }),
    ));
    lines.push(("(f) the thread-state pointer (thread-local)", per(|_| _ = black_box(record::thread_state()))));
}

/// A ring of 32-byte slots of the example's own, to take the protocol's stores apart.
static SLOTS: [AtomicU64; 4 * 4096] = [const { AtomicU64::new(0) }; 4 * 4096];
static HEAD: AtomicU64 = AtomicU64::new(0);

#[inline(always)]
fn slot(i: u64) -> &'static [AtomicU64] {
    let at = (i as usize & 4095) * 4;
    &black_box(&SLOTS)[at..at + 4]
}

/// The ladder's second part, once the rings are installed.
fn ladder_on(lines: &mut Vec<(&'static str, f64)>) {
    use core::sync::atomic::Ordering::{Relaxed, Release};
    lines.push((
        "(g1) 4 relaxed stores into a slot",
        per(|i| {
            let s = slot(i);
            for (w, it) in s.iter().enumerate() {
                it.store(i + w as u64, Relaxed);
            }
        }),
    ));
    lines.push((
        "(g2) 3 relaxed + the stamp Release",
        per(|i| {
            let s = slot(i);
            for (w, it) in s[1..].iter().enumerate() {
                it.store(i + w as u64, Relaxed);
            }
            s[0].store(i << 1, Release);
        }),
    ));
    lines.push((
        "(g3) + busy stamp first, dmb ishst",
        per(|i| {
            let s = slot(i);
            s[0].store(i << 1 | 1, Relaxed);
            #[cfg(target_arch = "aarch64")]
            // SAFETY: a barrier only.
            unsafe {
                core::arch::asm!("dmb ishst", options(nostack, preserves_flags));
            }
            for (w, it) in s[1..].iter().enumerate() {
                it.store(i + w as u64, Relaxed);
            }
            s[0].store(i << 1, Release);
        }),
    ));
    lines.push((
        "(g4) + the head Release (the protocol)",
        per(|i| {
            let s = slot(i);
            s[0].store(i << 1 | 1, Relaxed);
            #[cfg(target_arch = "aarch64")]
            // SAFETY: a barrier only.
            unsafe {
                core::arch::asm!("dmb ishst", options(nostack, preserves_flags));
            }
            for (w, it) in s[1..].iter().enumerate() {
                it.store(i + w as u64, Relaxed);
            }
            s[0].store(i << 1, Release);
            HEAD.store(i + 1, Release);
        }),
    ));
    lines.push((
        "(g5) the protocol, no barrier",
        per(|i| {
            let s = slot(i);
            s[0].store(i << 1 | 1, Relaxed);
            for (w, it) in s[1..].iter().enumerate() {
                it.store(i + w as u64, Relaxed);
            }
            s[0].store(i << 1, Release);
            HEAD.store(i + 1, Release);
        }),
    ));
    lines.push(("(g) one slot write, barrier, no counter", per(|i| _ = black_box(record::emit([0x80, i, i])))));
    lines.push((
        "(h) two slot writes",
        per(|i| {
            black_box(record::emit([0x80, i, i]));
            black_box(record::emit([0x81, i, i]));
        }),
    ));
    lines.push((
        "(h2) two slot writes, two counter reads",
        per(|i| {
            black_box(record::emit([0x80, record::counter(), i]));
            black_box(record::emit([0x81, record::counter(), i]));
        }),
    ));
    lines.push((
        "(i) the full span, rings",
        per(|i| {
            let _span = blaze_trace::info_span!("ladder_ring").entered();
            black_box(i);
        }),
    ));
}

fn main() {
    let mut ladder = Vec::new();
    ladder_off(&mut ladder);
    println!("{:<40} {:>10} {:>10}", "case", "1 thread", "8 threads");
    // The loop alone, to subtract from every line.
    let empty = {
        let start = Instant::now();
        for i in 0..SPANS {
            black_box(i);
        }
        start.elapsed().as_nanos() as f64 / SPANS as f64
    };
    println!("{:<40} {:>10.2}", "empty loop", empty);
    // Nothing records until the rings are installed, once a process: the nothing-records case
    // runs first.
    for case in [Case::OursOff, Case::TheirsNone, Case::TheirsNothing] {
        println!("{:<40} {:>10.2} {:>10.2}", case.name(), run(case, 1), run(case, 8));
    }
    // The example is its own entry: its heap is the test capability (blaze-own's `testing`).
    let heap = blaze_own::testing::heap().shared();
    let rings: &'static Rings =
        blaze_own::Box::leak_static(blaze_own::Box::new_in(Rings::new_in(64, 1 << 12, heap).expect("the rings"), heap));
    blaze_trace::install_rings(blaze_trace::RingConfig {
        rings,
        wants: |_| true,
        clock: record::Clock::Counter,
        limits: blaze_trace::TraceLimits::SHIPPED,
    })
    .expect("rings installed once");
    ladder_on(&mut ladder);
    for case in [Case::OursRing, Case::TheirsRing] {
        println!("{:<40} {:>10.2} {:>10.2}", case.name(), run(case, 1), run(case, 8));
    }
    let floor = {
        let start = Instant::now();
        for i in 0..SPANS {
            edge(i & 0xffff, true);
            edge(i & 0xffff, false);
        }
        start.elapsed().as_nanos() as f64 / SPANS as f64
    };
    println!("{:<40} {:>10.2}", "floor: two ticks, two slot writes", floor);
    println!("counter: {:?} Hz; entries not recorded: {}", record::counter_hz(), record::unrecorded());
    println!();
    println!("{:<44} {:>8} {:>8}", "ladder (1 thread)", "ns", "step");
    let mut before = 0.0;
    for (name, ns) in &ladder {
        println!("{:<44} {:>8.2} {:>+8.2}", name, ns, ns - before);
        before = *ns;
    }
}
