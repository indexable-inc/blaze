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
IX_PROF=target/prof ./my-program      # writes target/prof/my-program-<pid>-<session>.ixprof at exit
```

The directory must be yours alone. ix-prof makes it, mode 0700, if it is missing, and refuses
(at start, naming the directory and the rule) one that is a symbolic link, belongs to another
user, or has any group or other mode bits; it never changes the mode of a directory it did not
make. The profile and the endpoint's token go there, and whoever owns or can write a directory
can replace the files in it: in a shared `/tmp/prof` that another user made first, they could
swap the endpoint's address for their own port and collect the token.

With no `IX_PROF` and no `IX_PROF_HTTP` set, nothing starts. Each span callback and each
allocation then costs one relaxed atomic load.

| variable | meaning | default |
|---|---|---|
| `IX_PROF=<dir>` | turns profiling on; the profile is written to this directory when the guard drops | off |
| `IX_PROF_HZ` | CPU samples per second per thread; `0` turns the sampler off | 99 |
| `IX_PROF_ALLOC=<bytes>\|off` | mean bytes between sampled allocations (Go's `MemProfileRate`) | 524288 |
| `IX_PROF_HTTP=127.0.0.1:<port>` | serves `/debug/pprof` (needs the `http` feature, Unix); loopback only, every request needs the token | off |
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
blaze pprof prof.ixprof --http               # opens https://ix.dev/explore#profile=…
blaze pprof --dir target/prof heap top       # a running program's endpoint (its IX_PROF dir)
blaze pprof --token-file token http://127.0.0.1:6060/debug/pprof/heap top   # token: a 0600 file of yours
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

Every request needs the token: 128 random bits made at start. With `IX_PROF=<dir>`, ix-prof
writes the address and the token to `<dir>/http.endpoint` (one 0600 file, the address on the
first line and the token on the second, renamed into place whole) and prints only where it is
(`ix-prof: live at http://…/debug/pprof/live (its token is in <dir>/http.endpoint; …)`): a
program's stderr often lands where other users can read it, such as a CI log or a shared
`2>run.log`. Without `IX_PROF` there is no file to put the token in, so the stderr line carries
it (`…/live?token=…`), and so does whatever stderr goes to. The file is removed when the
profiler stops.

Send the token as a header, read from that file, never as an argument:

```sh
blaze pprof --dir target/prof 'profile?seconds=10' top     # reads http.endpoint
curl -sH @<(printf 'Authorization: Bearer %s\n' "$(sed -n 2p target/prof/http.endpoint)") \
  "http://$(sed -n 1p target/prof/http.endpoint)/debug/pprof/threads"   # printf is a builtin: not in argv
blaze pprof --dir target/prof 'profile?seconds=10' export --pprof cpu.pb.gz && go tool pprof cpu.pb.gz
```

`blaze pprof --dir` and `--token-file` read the token only from your own regular file, mode
0600, at most 64 bytes, in a directory that is yours and closed to group and other (the same
rule as ix-prof's), and `--dir` only from a loopback address. The token is sent only to the
source's own host and port: a `diff --base` at another endpoint is fetched without it.

A token on a command line (`blaze pprof 'http://…?token=…'`, `go tool pprof '…&token=…'`,
`curl -H "Authorization: Bearer $(cat …)"`) is readable by every local user for as long as
that command runs (`ps`, `/proc/<pid>/cmdline`), and the token stays good for the profiled
program's whole life; `blaze pprof` warns when it is given one. `token=` in the query is for
the browser (the ix.dev explorer), which cannot send the header.

The server binds loopback only, and answers 403 unless the `Host` header names loopback and
the request carries the token (`Authorization: Bearer <token>`, or `token=<token>` in the
query). Loopback alone is not enough: every local account can connect to a loopback port.
What runs before the token check is bounded: a request over 16 KiB, or not whole within 5 s of
its connection being accepted (one deadline for the whole request, so a client that trickles
a byte at a time is cut off all the same), is refused; at most 32 connections wait for their
token check at once, and one more evicts the oldest of them rather than being turned away, so
connections another user holds open cannot lock you out (a connection that passed the check is
never evicted); an `accept` that fails (out of file descriptors) pauses accepting for up to
1 s at a time instead of spinning. The profile file is mode 0600, and a directory ix-prof makes for it is 0700. The
endpoint is HTTP and not a Unix socket because the ix.dev explorer, a browser page, reads it.
The server stops, and lets its port go, when the guard drops.

`blaze pprof FILE --http` serves the file the same way: a new token per run, required on every
request. The explorer link carries the URL and its token in the fragment (`#profile=`), which
the browser never sends to ix.dev, and is opened through a 0600 page in a new 0700 temporary
directory, so the token is on no command line. Fetched answers are capped at 256 MiB and a
gzipped pprof at 256 MiB unpacked.
