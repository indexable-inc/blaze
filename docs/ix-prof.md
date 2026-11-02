---
status: current
summary: Profiling any Rust program, not only blaze, like Go's pprof. Covers the ix-prof crate (CPU samples of every thread, tracing spans with their own CPU time and allocations, named waits, allocations by stack), `IX_PROF=<dir>`, the loopback `/debug/pprof` endpoint and live stream, and `blaze pprof` with top, peek, tree, list, flame, diff, export and --http.
---
# Profiling any Rust program: ix-prof and `blaze pprof`

`crates/ix-prof` is a profiler any Rust program can link. Nothing in it is specific to blaze.
`blaze pprof` reads what it writes, as well as pprof files from Go, pprof-rs or parca. The
file format and the live stream are specified in [ix-prof-format.md](ix-prof-format.md).
[profiling.md](profiling.md) covers blaze's own profiler of its builds.

Status as of 2026-11-02: the code is written and type-checks. It has not yet been run on a
real program.

## Turn it on

```toml
[dependencies]
ix-prof = { path = "…/crates/ix-prof", features = ["http"] }   # `http`: the endpoint
```

```rust
// Optional: counts allocations and gives the heap profile.
#[global_allocator]
static ALLOC: ix_prof::alloc::Counting<std::alloc::System> =
    ix_prof::alloc::Counting::new(std::alloc::System);

fn main() {
    let _prof = ix_prof::start_from_env().expect("ix-prof");
    // Optional: spans, and the waits you mark with a field `wait = "<kind>"`.
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry().with(ix_prof::layer()).init();
    // …
}
```

Then run it with:

```sh
IX_PROF=/tmp/prof ./my-program        # writes /tmp/prof/my-program-<pid>.ixprof at exit
```

With no `IX_PROF` and no `IX_PROF_HTTP` set, nothing starts. Each span callback and each
allocation then costs one relaxed atomic load.

| variable | meaning | default |
|---|---|---|
| `IX_PROF=<dir>` | turns profiling on; the profile is written to this directory when the guard drops | off |
| `IX_PROF_HZ` | CPU samples per second per thread; `0` turns the sampler off | 99 |
| `IX_PROF_ALLOC=<bytes>\|off` | mean bytes between sampled allocations (Go's `MemProfileRate`) | 524288 |
| `IX_PROF_HTTP=127.0.0.1:<port>` | serves `/debug/pprof` (needs the `http` feature); loopback only | off |
| `IX_PROF_RETAIN=<bytes>` | how many bytes of the live stream are kept for resuming | 64 MiB |

What gets recorded:

- **CPU samples of every thread.** No thread has to register.
  - Linux: `perf_event_open`, using each thread's task clock. If the kernel refuses (for
    example `kernel.perf_event_paranoid` > 2), `start` returns an error that says so.
  - arm64 macOS: each thread is stopped briefly and its frame pointers are walked.
  - Blocked threads are sampled one tick in ten, marked off-CPU and weighted for the ticks
    that were skipped.
- **The span tree.** Each span records its wall time, its own CPU time (minus its
  children's) and the allocations made inside it.
- **Waits.** A span with fields `wait`, `on` and `owner` is a wait: what it waits for and who
  will wake it.
- **Allocations**, sampled by stack, with live and total bytes. Heap counters are recorded
  every 250 ms.
- **Symbols and line numbers** come from the binary's DWARF (or its dSYM on macOS) when the
  profile is written, never while sampling.

## Read it: `blaze pprof`

```sh
blaze pprof prof.ixprof                      # top 20 functions by CPU
blaze pprof prof.ixprof top -cum -n 40
blaze pprof --kind spans prof.ixprof top     # span self time; --kind heap | waits
blaze pprof prof.ixprof peek 'parse'         # callers and callees of matching functions
blaze pprof prof.ixprof tree --min 0.01
blaze pprof prof.ixprof list 'my_fn' --source .
blaze pprof prof.ixprof flame                # a flame graph in the terminal
blaze pprof prof.ixprof flame --folded > out.folded   # for inferno / flamegraph.pl
blaze pprof new.ixprof diff --base old.ixprof top
blaze pprof prof.ixprof export --pprof prof.pb.gz     # for go tool pprof, speedscope, Perfetto
blaze pprof prof.ixprof export --chrome trace.json
blaze pprof prof.ixprof --http               # opens https://ix.dev/explore?profile=…
blaze pprof http://127.0.0.1:6060/debug/pprof/heap top
```

`--type NAME` picks the sample type to weigh by. For heap profiles these are `inuse_space`,
`alloc_space` and the matching object counts.

## Live

With `IX_PROF_HTTP=127.0.0.1:6060`, the program serves Go's layout:

- `/debug/pprof/profile?seconds=30` (CPU)
- `/heap` and `/allocs`
- `/spans` and `/waits`
- `/threads`: each thread's open spans and what it waits on, right now
- `/ixprof?last=N`: a window snapshot
- `/live`: the stream the explorer draws, which can be resumed from a cursor

`go tool pprof http://127.0.0.1:6060/debug/pprof/profile?seconds=10` works unchanged.

The server binds loopback only, and answers 403 unless the `Host` header names loopback.
