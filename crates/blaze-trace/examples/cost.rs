//! What a span costs: ns per span (open, enter, exit, close) at 1 and 8 threads, for blaze_trace
//! with no sink, blaze_trace recording into a per-thread ring, and the third-party `tracing`
//! with no subscriber, with a subscriber that keeps nothing and with the same ring.
//!
//! `cargo run --release -p blaze-trace --example cost`. `tracing` is a dev-dependency of this
//! crate only for this comparison; it goes once the numbers are in docs/profiling.md.
//!
//! The ring is the same for both: per thread, 4096 entries of 32 bytes (site, depth, tick,
//! edge), overwrite-oldest with an exact lost count, the tick read from the CPU counter in user
//! space (`cntvct_el0` on arm64, `rdtsc` on x86-64). A span is two entries: one at enter, one
//! at exit. The floor is that: two counter reads and two 32-byte stores.

use std::{
    cell::Cell,
    hint::black_box,
    sync::{Arc, Barrier},
    time::Instant,
};

const SPANS: u64 = 2_000_000;
const ENTRIES: usize = 4096;

/// The CPU's counter, read in user space.
#[inline(always)]
fn ticks() -> u64 {
    let t: u64;
    #[cfg(target_arch = "aarch64")]
    // SAFETY: `cntvct_el0` is readable at EL0 on every arm64 host blaze runs on (macOS and
    // Linux both enable it); the read has no side effect and writes only `t`.
    unsafe {
        core::arch::asm!("mrs {t}, cntvct_el0", t = out(reg) t, options(nomem, nostack));
    }
    #[cfg(target_arch = "x86_64")]
    {
        let (lo, hi): (u32, u32);
        // SAFETY: `rdtsc` is unprivileged unless CR4.TSD is set, which no host blaze runs on
        // sets; it writes only eax and edx.
        unsafe {
            core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
        }
        t = u64::from(hi) << 32 | u64::from(lo);
    }
    t
}

thread_local! {
    static RING: [Cell<[u64; 4]>; ENTRIES] = const { [const { Cell::new([0; 4]) }; ENTRIES] };
    static HEAD: Cell<u64> = const { Cell::new(0) };
    static DEPTH: Cell<u64> = const { Cell::new(0) };
}

/// One edge of a span into this thread's ring: 32 bytes, nothing formatted.
#[inline(always)]
fn edge(site: u64, began: bool) {
    let depth = DEPTH.get();
    let depth = if began { DEPTH.set(depth + 1); depth } else { DEPTH.set(depth - 1); depth - 1 };
    let head = HEAD.get();
    HEAD.set(head + 1);
    let at = head as usize & (ENTRIES - 1);
    RING.with(|ring| ring[at].set([site, depth, ticks(), u64::from(began)]));
}

/// Entries this thread's ring overwrote: the exact lost count.
fn lost() -> u64 {
    HEAD.get().saturating_sub(ENTRIES as u64)
}

/// blaze_trace's ring sink. A span's id is its site's address, so enter and exit find the
/// site without a table; the depth tells nested spans of one site apart.
struct OursRing;

impl blaze_trace::Sink for OursRing {
    fn enabled(&self, _: &'static blaze_trace::Metadata<'static>) -> bool {
        true
    }

    fn new_span(&self, span: &blaze_trace::Attributes<'_>) -> blaze_trace::Id {
        blaze_trace::Id::from_u64(core::ptr::from_ref(span.metadata()).addr() as u64)
    }

    fn record(&self, _: &blaze_trace::Id, _: &blaze_trace::Record<'_>) {}

    fn event(&self, _: &blaze_trace::Event<'_>) {}

    fn enter(&self, span: &blaze_trace::Id) {
        edge(span.into_u64(), true);
    }

    fn exit(&self, span: &blaze_trace::Id) {
        edge(span.into_u64(), false);
    }

    fn try_close(&self, _: blaze_trace::Id) -> bool {
        true
    }
}

static OURS_RING: OursRing = OursRing;

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
        Case::OursRing => blaze_trace::with_thread_sink(&OURS_RING, ours_loop),
        Case::TheirsNone => theirs_loop(),
        Case::TheirsNothing => tracing::subscriber::with_default(TheirsNothing, theirs_nothing_loop),
        Case::TheirsRing => tracing::subscriber::with_default(TheirsRing, theirs_ring_loop),
    }
    let ns = start.elapsed().as_nanos() as f64 / SPANS as f64;
    black_box(lost());
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
    // No sink is installed until `OursRing`, and the group flag is never lowered: the
    // no-sink case runs first.
    let cases = [Case::OursOff, Case::TheirsNone, Case::TheirsNothing, Case::TheirsRing, Case::OursRing];
    let floor = {
        let start = Instant::now();
        for i in 0..SPANS {
            edge(i, true);
            edge(i, false);
        }
        start.elapsed().as_nanos() as f64 / SPANS as f64
    };
    println!("{:<40} {:>10} {:>10}", "case", "1 thread", "8 threads");
    for case in cases {
        println!("{:<40} {:>10.2} {:>10.2}", case.name(), run(case, 1), run(case, 8));
    }
    println!("{:<40} {:>10.2}", "floor: two ticks, two 32-byte stores", floor);
}
