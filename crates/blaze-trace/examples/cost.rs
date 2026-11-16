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
    alloc::Global,
    hint::black_box,
    sync::{Arc, Barrier},
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

fn main() {
    println!("{:<40} {:>10} {:>10}", "case", "1 thread", "8 threads");
    // Nothing records until the rings are installed, once a process: the nothing-records case
    // runs first.
    for case in [Case::OursOff, Case::TheirsNone, Case::TheirsNothing] {
        println!("{:<40} {:>10.2} {:>10.2}", case.name(), run(case, 1), run(case, 8));
    }
    let rings: &'static Rings = Box::leak(Box::new(Rings::new_in(64, 1 << 12, Global).expect("the rings")));
    blaze_trace::install_rings(blaze_trace::RingConfig { rings, wants: |_| true, clock: None }).expect("rings installed once");
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
}
