//! What the always-on flight recorder, `blaze_profile::flight`, costs a span: an info span of the
//! target `blaze` opened, entered, left and closed on one thread, with the thread's CPU counted
//! (`flight::count_cpu`, as every blaze process does) and no `BLAZE_PROFILE`
//! (docs/v2/plan.md):
//!
//! - `record/root`: a span under no other, so its close may end a CPU stretch
//!   (`time::cpu::ROOT_AGE`): one CPU read in about every 20 µs of such spans;
//! - `record/nested`: a span under one held open the whole benchmark, which reads the CPU
//!   clock only when a stretch passes `time::cpu::AGE`.
//!
//! Budget: 30 ns a span for each. The recorder reads the thread's CPU clock (a syscall, about
//! 100 ns) only at fiber switches, thread parks and the boundaries `time::cpu` names, never once
//! a span; a span that costs past the budget is a read back on the span path. The ring wraps
//! with nothing draining it, which costs the writer nothing more.
//!
//! Every benchmark does the same work on every run, so that `blaze_bench` can pair the binary of
//! a change with the binary of its base.

use blaze_bench::{IntoBenchmarks, benchmark_fn};

/// Installs the recorder and counts the thread's CPU, once a process.
fn recording() {
    blaze_profile::install();
    // Err: counted already, by an earlier benchmark of this process.
    _ = blaze_profile::flight::count_cpu(blaze_profile::thread_cpu);
}

fn benchmarks() -> impl IntoBenchmarks {
    [
        benchmark_fn("record/root", |b| {
            recording();
            b.iter(|| {
                let _span = tracing::info_span!(target: "blaze", "record root").entered();
            })
        }),
        benchmark_fn("record/nested", |b| {
            recording();
            let outer = tracing::info_span!(target: "blaze", "record outer").entered();
            b.iter(move || {
                let _held = &outer;
                let _span = tracing::info_span!(target: "blaze", "record nested").entered();
            })
        }),
    ]
}

blaze_bench::main!(benchmarks());
