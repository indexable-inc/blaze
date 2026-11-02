---
status: plan
summary: lane usercode's speed of light: the bound each component's work and the hardware set, the arithmetic, and how the design reaches 90% of it.
date: 2026-10-30
---
# Lane usercode: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each component of lane usercode has a section here,
written by the agent that owns it. Unless a section says otherwise, figures are for one core of
the target machines (EPYC Turin, Graviton 5, [hardware.md](../../design/hardware.md)):
- about 3.5 GHz, so figures in ns use 3.5 GHz;
- streaming reads from L1 or L2 at about 32 bytes a cycle, which is also memchr's rate;
- UTF-8 validation of mostly ASCII text at about 16 bytes a cycle (std's word-at-a-time path);
- about 2 instructions per cycle for branchy code and about 4 for straight-line integer code;
- a small allocation from a thread cache in about 20 cycles;
- a system call that resolves a path (stat, openat, mkdir), with the path cached, in about
  0.4 µs, and one on an open descriptor (write, fstat, futimens, close) in about 0.15 µs.

None of this has been measured yet: nothing compiles until every lane is done. `proof`'s budgets
measure each bound once it does, and a component more than 10% above its bound is a bug.

## The machine (speed-sol2-usercode)

The second round's sections use these figures, those of
[the link lane's speed of light](../link/speed-of-light.md), not yet measured:

| what | figure |
|---|---|
| clock | 3.5 GHz |
| L1 read, streaming | 64 B a cycle |
| L2 read, streaming | 32 B a cycle (about 110 GB/s) |
| one core from DRAM, streaming | about 25 GB/s |
| page-cache copy (`read`, `write`) | about 10 GB/s a core |
| system call on an open descriptor | 0.15 µs |
| system call that resolves a path | 0.4 µs |
| IPC | about 2 instructions a cycle for branchy code, about 4 for straight-line integer code |
| BLAKE3 with AVX-512 | about 6 GB/s a core |

Process figures, also estimates: `fork` of a small single-threaded process (a launcher of about
10 MB resident) about 30 µs; `execve` of a dynamically linked program to its `main` 100 to
300 µs, of blaze's own binary about 1 ms (its page faults); a new network namespace about
0.1 ms, its teardown deferred to the kernel's workqueue; a mount namespace's copy about 1 µs a
mount; a tmpfs or bind mount 5 to 10 µs, each under the kernel's global `namespace_sem`, so
mounts of parallel runs serialize.

## Where each component is (speed-sol2-usercode)

| component | crates | section |
|---|---|---|
| a build script's output, environment, elision, rerun checks, begin and finish | `blaze-build-script` | "Build scripts" below |
| a run's `OUT_DIR` read back and written again | `blaze-usercode` `run.rs` | "A run's `OUT_DIR`" below |
| a sandboxed run's setup on Linux and macOS | `blaze-cargo/src/sandbox/` | "A sandboxed run's fixed cost" below |
| tracing, the Windows sandbox | `blaze-cargo/src/sandbox/` | "The sandbox: tracing", "The Windows sandbox" below |
| the test queue and fork server | `blaze-usercode` `test/` | "Tests" below, [speed-of-light-tests.md](speed-of-light-tests.md) |
| finding a kept result, outside paths, assembling what was read | `blaze-usercode` `index.rs`, `inputs.rs`, `trace.rs` | the sections of those names below |
| a proc-macro call, the token wire, the server, native expanders | `blaze-expand`, `proc-macro-api`, `proc-macro-srv` | [speed-of-light-macros.md](speed-of-light-macros.md) (speed-sol2-usercode-macros) |
| wasm proc macros and the wasi runtime (deleted 2026-10-30) | `blaze-macro-wasm`, `blaze-wasi` | [speed-of-light-wasm.md](speed-of-light-wasm.md) (superseded) |

## Build scripts: `crates/blaze-build-script/` (usercode-fast-scripts)

What each part does and the state of the art it is compared with:
[scripts.md](scripts.md). Workloads, from scripts as published: a typical script prints about
20 lines (1 KB); a heavy one (cc, bindgen) about 500 lines (40 KB), of which about 400 are
directives kept. A unit's environment is about 100 variables cargo sets (40 fixed, 20
features, 30 cfgs, 10 from dependencies) and about 60 inherited, so about 150 pairs and 8 KB.
A root `build.rs` is 0.3 to 5 KB.

### What a script prints (`output::read`, `parse`, `watch::watched_in`)

Work: split \(n\) bytes into \(L\) lines, validate the \(n_d\) bytes of directive lines as
UTF-8, and dispatch each directive.
- **Bytes.** Line breaks at \(n/32\) cycles, validation at \(n_d/16\).
- **Instructions.** Per line, the minimum is about 8 to find the first byte that is not
  space and test `cargo:`, about 10 for the key (its length, then its bytes), about 5 to find
  `=`, and about 10 to hand the directive on: about 35, at IPC 2 about **18 cycles a line**.
- **Allocations.** `parse` hands back an `Output` of owned strings: one allocation of 20
  cycles per kept value \(D\).
- **The bound** for `read` is \(n/32 + n_d/16 + 18L\) cycles. For the heavy script: 1,250 +
  2,500 + 9,000 = **12,750 cycles, 3.6 µs**. `parse` adds \(20D\) = 8,000: **20,750 cycles,
  5.9 µs**. A typical script is about 0.15 µs.
- **Where the design is.** At the bound: one pass, each line looked at once, no copy but a
  kept value, and no search of values unless `OUT_DIR` moved (then one `memmem::Finder` built
  per output).
- **What stands in the way** of `parse` reaching `read`'s bound is the 400 allocations, which
  the `Output` type's owned fields make part of the work. A caller that needs only part of
  the output reads it with its own `Sink` and copies nothing else (`watched_in` does). An
  `Output` held as one buffer with spans would remove them; that changes every caller of
  `Output`'s fields in blaze-cargo and blaze-usercode, so it is asked for in
  `v2/requests/usercode-fast-scripts.md` (request 7), not done here.

### The environment (`env`)

Work: make about 100 names and values, order them, and merge them with about 60 inherited.
- **Allocations.** `Env::vars` holds owned pairs: for about 150 pairs that is up to 300
  allocations, about **6,000 cycles**, and the copy of 8 KB, about 250.
- **Values.** Each derived value is made at its length without `format!` (`envify`,
  `prefixed`, `decimal`): about 20 cycles plus a cycle per 16 bytes, for 100 values about
  **2,500 cycles**.
- **Order.** What cargo sets comes in about five groups that could each be sorted already;
  their merge is \(100 \log_2 5 \approx 230\) comparisons. A comparison of two names by a
  `u128` of their first 16 bytes is about 4 cycles (a tie goes to the bytes), so about **900
  cycles**.
- **Inherited.** The inherited environment is the same for every script of a build. Passed
  sorted, checking that is 60 comparisons of names (about 10 cycles each), **600 cycles**, and
  the merge of about 160 entries about **800 cycles**.
- **The bound** is about 6,250 + 2,500 + 900 + 600 + 800 = **11,050 cycles, 3.2 µs** a unit.
- **Where the design is.** The same allocations, values, check and merge, but what cargo sets
  is sorted whole: \(100 \log_2 100 \approx 660\) comparisons, about 2,600 cycles in place of
  900. That is about **12,750 cycles, 15% over the bound**. Before this pass, the sort key was
  the first 8 bytes, which tie on most names (`CARGO_CF`, `CARGO_FE`, `CARGO_PK`) and cost
  about 10 cycles a comparison (6,600 cycles, 37% over); and the inherited environment was
  sorted with the rest (another \(60 \log_2 60 \approx 360\) comparisons, 3,600 cycles). Both
  are removed: the key is 16 bytes, and an inherited environment passed sorted is only checked.
- **What stands in the way.**
  1. The whole sort of what cargo sets, about 1,700 cycles. To remove it, each group is made
     in order (the fixed names from a table sorted at compile time, features in the
     manifest's sorted order, cfgs sorted once per target) and the groups are merged. This
     is the next change here, once `proof` confirms the 15%, since the estimate is close to
     its own error.
  2. Callers that pass the inherited environment unsorted pay the 3,600 cycles back. Request
     6 asks blaze-usercode and blaze-cargo to sort it once per build.
  3. The 6,000 cycles of allocations, more than half the bound, are the owned `Env::vars`.
     An `Env` held as one buffer with spans would remove them. Callers move the pairs into a
     run's key and a `Command`, so this is asked for with request 7.

### Scripts read rather than run (`elide`, `elide::lex`)

- **Refused at the start** (most scripts, which start with `use`, `#!` or an attribute). The
  work is to find the root file among the script's files (a `BTreeMap` lookup of 2 to 4 path
  comparisons, about 100 cycles) and look at its first bytes (about 10). **The bound is about
  110 cycles, 30 ns.** The design is at it: `may_open_main` refuses on the first byte that is
  not white space when it is neither `/` nor `f`. Before this, the whole file was validated,
  searched three times (`\r\n`, a lone `\r`, text direction code points) and its first token
  lexed: for 3 KB about \(3000/16 + 3 \cdot 3000/32 \approx 470\) cycles more, 5 times the
  bound.
- **Read to the end** (a script elision may accept), \(n\) bytes and about \(T = n/6\)
  tokens:
  - **Bound.** One classification of every byte at about 0.2 cycles a byte (simdjson's stage
    1: 64 bytes a step into masks) and about 10 instructions per token (5 cycles) give
    \(0.2n + 5n/6 \approx\) **\(1.0n\) cycles**. For a 300-byte accepted script that is about
    300 cycles.
  - **Where the design is.** Validation \(n/16\), three SIMD searches \(3n/32\), a table load
    per byte outside literals (about \(0.6n\)), and about 30 instructions per token (15 cycles,
    \(2.5n\)): about **\(3.3n\) cycles, 3.3 times the bound**. For 300 bytes, about 1,000
    cycles (0.3 µs).
  - **What stands in the way** is the per-token instruction count and the per-byte loop. The
    fix is a stage 1 over `std::simd` (safe code, `portable_simd` on the pinned nightly): 64
    bytes a step into masks of identifier bytes, spaces, quotes, `\r` and `0xE2` (the text
    direction check folded in), with token boundaries from the masks' bits. The lexer then
    does about 10 instructions a token. This is owned here and comes first after `proof`
    measures, since every accepted script is small and read once per edit of its source.
- **Printing** what an accepted script prints copies at most \(16n\) bytes (the bound against
  scripts that print the square of their size), at 32 bytes a cycle.

### What reruns a script (`watch`)

Work per printed path: join it to the package's directory (about 70 bytes), keep it once, and
copy it out if it is new.
- **Bound.** Copying 70 bytes is about 2 cycles and the allocation 20. Keeping each once must
  use `Path` equality, as cargo's does (`a//b` and `a/./b` are `a/b`), and a hash consistent
  with it reads each byte at a component boundary check, about 1 cycle a byte, so 70. **About
  92 cycles a path.**
- **Where the design is.** About 100 cycles: one reused join buffer, one copy per kept path, a
  scan up to 16 paths and std's `Path` hash past that, within 10% of the bound.

### Rerun checks and directory times (`apply::Rerun::stale`, `newest`)

This is I/O bound, and the bound is the number of system calls.
- **Bound.** One stat per watched file. For a watched directory tree of \(D\) directories,
  \(E\) entries and \(K\) links: \(D\) times (openat, getdents64 per about 200 entries, close),
  plus one fstatat per entry, plus one stat per link's target. For `src` with 50 directories
  and 500 files that is \(50 \cdot 3 + 500 = 650\) calls: about 150 on descriptors and 500
  relative to one, **about 120 µs**.
- **Where the design is.** At the bound. Each entry's time is read relative to its directory
  (`DirEntry::metadata`), a path is made only for a link or a directory, and a directory is
  visited once by device and inode from the metadata already read. Before this pass, a
  directory was checked by `canonicalize` (glibc's `realpath`: about one call per component,
  so about 8 per directory, 400 here), and each entry was stat'd by its full path: about 1.6
  times the bound.
- **Beyond the bound of these calls**, batching the stats through io_uring (`IORING_OP_STATX`)
  would cut the per-call cost. That is the daemon's effects (lane core), not this crate's
  std calls; request 2 moves runs' files to effects, and `stale` can follow it.
- **Allocations.** A check that finds nothing stale allocates nothing for files and
  variables: the path is built in the thread's path buffer, and the lookups
  return borrowed values (`a_fresh_run_is_checked_without_allocating` holds it to zero). A
  watched directory still allocates what std's `read_dir` does, its handle and a copy of its
  path, per directory; reading directories without allocating needs `getdents64` into a
  frame buffer, which is the daemon's effects (lane core), not this crate's safe std calls.

### Starting and finishing a run (`run::begin`, `Ran::finish`, `run::judge`)

- **`begin`**, the directories existing (every run but the first): `mkdir` of `OUT_DIR`,
  which fails as existing, and std's check that it is a directory (2 calls); in cargo's
  second layout the same for the run's directory (2); then openat, write, fstat and close of
  `invoked.timestamp` (4). **The bound is 6 calls (8 in the second layout), about 1.6 µs.**
  The design is at it. Before this pass, the first layout made the run's directory again
  (it holds `OUT_DIR`, so it already existed: 2 more calls) and stat'd the timestamp by path
  after closing it.
- **Writing cargo's files**: the printed file is openat, write, futimens and close (4), then
  `stderr` and `root-output` are 3 each. **The bound is 10 calls, about 2.3 µs.** The design
  is at it. Before, the printed file was written, closed and opened again to set its time
  (12 calls).
- **`judge`** is `logged`'s one search for `\ncargo:` (\(n/32\)) plus `parse`'s bound above.
- **The run itself** is bound by spawning the script, its own work and waiting for it:
  posix_spawn and exec of a dynamically linked binary take about 100 to 300 µs. Everything
  this crate does around a run is about 14 calls, under 5 µs, so a run's critical path is the
  script. The wait on `ETXTBSY` (a script another thread's fork still holds open) only
  happens when that fork races the write of the script; spawning from one place in the
  daemon (lane core) removes it.

### Who receives what (`apply::to_link`, `Compile`)

- **`to_link`** walks the unit graph once: the bound is one node made per edge and one copy
  per script run received. The design is at it. It made each dependency's node twice before,
  and followed dependencies that receive nothing.
- **`Compile::args`** borrows each argument but a path read lossily and a `link-arg=`,
  which it makes at their exact length.
- **`to_link_in`** lends the memo's entry: a unit met again allocates nothing
  (`a_unit_met_again_is_free`).

### Memory by lifetime (docs/v2/plan/design/memory.md)

What this crate allocates, by how long it lives, and the allocator each belongs on:

| what | lives | allocator |
|---|---|---|
| `Output`, `Env`, `Compile`, `Watched`: owned results | a kept run, a unit's build | the revision arena (request 7: one buffer with `u32` spans) |
| elision's printed text | the caller's buffer, kept as the run's output | the caller's (`elide_into`), or the result's own (`elide`) |
| elision's argument list, placeholder flags, normalized copy | one read of a root file | the task's frame (`frame::task(Tag::Usercode, ..)`), strings lent from the thread's shelf |
| `Rerun::stale`'s path | one check | the thread's path buffer (`scratch::with_path`) |
| messages, errors | when one is shown | the general-purpose allocator: the edge |

Elision and the rerun check use `blaze-mem`; the owned results wait on request 7, since
moving them onto an arena changes their shape for the callers. The paths
that run in the warm loop already make no general-purpose allocation. Those are reading what
was printed, lexing, a refused root file, the check that a kept run is fresh, a memo hit,
environment lookups, the check of a run for errors and a warm elision, and the allocation-budget tests hold
each to zero.

### What `proof` measures

Each figure above, with its bound times 1.1 as the budget:
- cycles per byte and per line of `read` and `parse` on the corpus of `harness/scripts.py`;
- nanoseconds per unit of `env`, with the inherited environment sorted and not;
- nanoseconds per refused root file, and cycles per byte of a root file read to the end;
- system calls (strace's count) of `begin`, `Ran::finish` and `newest` over a fixed tree.

## Tests: the queue and fork server, `crates/blaze-usercode/src/test.rs`, `test/`

Written by speed-link-edge-sol-usercode. The fork server, the execution record, the wire, the
queue and a suite's critical path, each with its arithmetic, are in
[speed-of-light-tests.md](speed-of-light-tests.md) (usercode-fast-tests). This section takes the
two components the bar names, counts system calls per test against cargo-nextest, and says what
is left. Tests run in ix VMs, so the costs are speed-of-light-tests.md's: a cheap system call
about 0.3 µs, `fork` of a 10 to 30 MB server about 50 µs (`T_fork`), the child's exit and reap
about 10 µs (`T_exit`).

**Workload.** One test binary of 600 unit tests, its server about 20 MB resident, on 16 cores,
so up to `k = 15` other children of the same server in flight while one starts. Each test does
nothing (the worst case for overhead) or runs about 100 µs to 1 ms (the common case).

### A test from the queue to its result

- **The bound, with a fork server:** one `fork` (its page tables, about 20 MB / 4 KiB × 8 bytes
  = 40 KB copied, and later a 4 KiB copy for each page the test writes), the test's own time, the
  exit, and one frame each way (shared by every test that ends in the same turn). The system
  calls that can't be merged are 17 a test:
  - the server: `socketpair`, `fork`, `setpgid` (both sides set it, so a kill sent at once finds
    the group), `close` of the child's end, `ioctl` to make its end non-blocking, `pidfd_open`,
    the read that sees end of file, `waitpid`, and two `close`s;
  - the child: `setpgid`, two `dup2`, closing the server's descriptors, the scope mark's
    `access`, the exit;
  - the turn's `poll`, the `waitpid` that finds no more and one `write`, shared.

  That is \(T_{fork} + T_{exit} + 17 \times 0.3\) µs = 50 + 10 + 5 = **65 µs + T**. The test's
  thread (libtest runs every test on a thread named after it, so nextest pays it too) is part of
  `T`.
- **Without a fork server** (a package that opts out, and Windows, where there is no fork):
  the binary runs whole as libtest's threads, one process per binary. The bound is one
  `posix_spawn` or `CreateProcessW` per binary. It costs two today, since the sandbox's launcher
  starts the binary; per binary that is about 1 ms on Linux, spread over its tests. On Windows
  the second process is in the next section.
- **Where the code was:** `19 + 2k` system calls a test. The child closes the server's socket,
  its own read end, each of the `k` other children's output and pidfd one by one, then its own
  write end. At `k = 15` that is 49 calls, 15 µs, so an empty test cost 75 µs: **16% over** the
  bound (3% at `k = 1`). It is the one per-test cost here that grows with the machine. Copies:
  the page tables and the pages written, as the bound; a test's output crosses five copies,
  which is noise under 64 KiB (speed-of-light-tests.md, "The wire").
- **What changes:** the child closes the server's descriptors with one `close_range` per gap in
  the descriptors the program had when its server started (no program code runs in the server
  after, so everything else is the server's). That is usually one gap, so 4 calls in the child
  whatever `k` is. The exact edits, with a test of the ranges against a model, are in
  `v2/requests/usercode-fast-tests.md` ("speed-link-edge-sol-usercode 1"), since the code is
  usercode-fast-tests' while it runs. Tests still see exactly the descriptors they saw before.
  macOS has no `close_range` and keeps the loop.
- **Where it is with that:** 17 calls, 5 µs: an empty test at 65 µs, **at the bound**. Not
  taken: `clone3` with `CLONE_PIDFD` would fold `pidfd_open` into the fork (one call, 0.5%), but
  a raw `clone3` skips the C library's fork handlers. Rebuilding `poll`'s list each turn costs
  about 20 ns a child, under 1%.

Against **cargo-nextest**, which runs one process per test (`posix_spawn` of the binary with
`--exact NAME`), counted for a dynamically linked Rust test binary on Linux. These counts are
estimated from the code, not measured:

| per test | blaze's fork server | cargo-nextest |
|---|---|---|
| system calls, parent | 10, plus the shared turn | about 15: two `pipe2`, the spawn, closes, `epoll_ctl`s, reads to end of file, the wait |
| system calls, child | 6 | about 70: `execve`, the dynamic loader opening and mapping libc and libgcc_s, std's start (signal handlers, the alternate stack), libtest's arguments, the exit |
| exec and linking | none: the binary started once | every test: the image mapped, its relocations applied |
| pages copied | page tables (40 KB), then 4 KiB for each page the test writes | each relocated page of `.data.rel.ro` and the GOT, and every page the start touches, made fresh |
| time before the test's code | about 65 µs | about 0.5 to 2 ms |

So nextest pays about 5 times the system calls and 8 to 30 times the time per test. The fork
server is the zygote pattern (Android's zygote, AFL++'s fork server): one start per binary,
then a fork per test.

### The queue

- **The bound:** choosing a test and scheduling it should cost far less than a fork. That means
  O(1) or O(log n) with no lock: the next test in the longest-first order whose server can take
  it, an O(1) deadline insert, a slab slot for the attempt, and a 30-byte frame. That is about 300
  instructions, about 0.1 µs at 3.5 GHz and an IPC of 3, plus what it must hand on (the
  result's output, record and name).
- **Where the code is:** at the bound. `schedule` pops `ready.first()`, a bitset over the
  longest-first order with a cursor that only moves forward until a lower bit is set again, so a
  pick is amortized O(1): one word load per 64 tests and one `tzcnt`. Deadlines are a timing
  wheel, O(1) to insert and cancel. Attempts and tags are slabs with generational ids. The queue
  is sans-IO, driven from one thread, with no lock and nothing shared. About 0.5 µs a test with
  its 8 allocations, **under 1% of the fork**.
- **What is left:** the 8 allocations, each owned by the result past the queue, go to 0 with a
  run's results in its request arena (`v2/requests/usercode-fast-tests.md`, 13), which needs
  cargo. The fork wall of one server is removed by replicas (next section).

### Replicas of a fork server

- **The wall:** one server forks one test at a time, `F + s` each: its fork `F` (about 50 µs)
  and its other calls `s` (about 5 µs, the server's 10 calls above). So a binary starts at most
  `1 / (F + s)`, about 18,000 tests a second. A core holds a test for about `F + T + E` (the
  fork, the test, its exit and reap `E`, about 10 µs), so `C` cores ask for `C / (F + T + E)`
  tests a second. The fork is the wall whenever `T < C × (F + s) - F - E`: on 16 cores any test
  under 820 µs, on 64 cores any test under 3.5 ms, the common case for unit tests.
- **The removal:** `K` replicas per binary. `K` servers start `K / (F + s)` tests a second,
  enough once `K ≥ C × (F + s) / (F + T + E)`. The queue takes
  \(K = \lceil C \times F / (F + T) \rceil\), at least 1, at most `C` and the binary's test
  count, with `F` the server's own measure and `T` the median of the binary's expected test
  times (zero for a test never run, so a new binary gets `C`). Against the exact need the two
  terms it leaves out cancel to within 10% either way: at `T = 0` it asks `F / F = 1` against
  `(F + s) / (F + E)` = 0.92, and as `T` grows past `F` it asks `F / (F + T)` against at most
  `(F + s) / (F + T)`, at most `s / F` = 10% under before rounding up. A fork measured as 0 gives
  1. Examples: 16 cores, `F` = 50 µs, `T` = 100 µs gives 6; `T` = 1 ms gives 1; 64 cores and
  `T` = 0 gives 64.
- **Measuring `F`:** the server forks a child that leaves at once, three times before its hello,
  and sends the least (`Frame::Hello`'s `fork`). That costs `3 × (F + E)`, about 180 µs once per
  binary, and measures the fork at the server's real size (its page tables), on the machine it
  runs on. A fixed `F` would be wrong by the size of the binary.
- **Making the replicas:** blaze's first frame to the first server is `Frame::Replicate { count:
  K - 1 }`. Each process forks a copy and gives it half of what is left, so `K - 1` copies come
  in \(\lceil \log_2 K \rceil\) rounds: 6 forks deep (300 µs) for `K = 64`, where one after another
  would be 63 forks (3.2 ms), more than a binary's start. Each copy is a fork of a server that
  ran no test, so it has the statics, the slot count and an empty pool of regions; it gets a
  kqueue of its own on macOS (a fork inherits none), connects anew on the same socket and says
  its own hello. Memory: page tables (40 KB for a 20 MB server) and the pages it writes.
- **Handing tests out:** each live replica takes at most `room = ceil(C / live)` tests at once,
  so the live replicas together always have room for every free core and a ready test never
  waits for one. The replicas with room are a bitset (`blaze_ids::bits`); a pick is its first
  set bit (one word for `K ≤ 64`) and an end sets the bit again: O(1), no allocation, so the
  queue's 0.5 µs and 8 allocations a test are unchanged (`the_steady_state_reuses_its_slabs`
  runs at one server and at four replicas). Tests still go in the one longest-first order.
- **A replica that dies:** its connection ends, so the tests it ran are lost with which replica
  (its pid) and why. The rest carry on, with `room` recomputed for the replicas left, and only
  when none is left is the server killed. The first server is the process blaze started and
  waits for, so its own end still ends every replica's tests, as one server's end did; it runs
  no test itself, so that takes a fault in the server. A late hello takes its share from the
  next free core.
- **Where it is:** a binary starts up to `K / (F + s)` tests a second. At `K = C` and `T = 0`
  that is `C` times one server, the cores' own bound for empty tests.

**State of the art:** Android's zygote and AFL++'s fork server for the per-test start; nextest
for process-per-test isolation, which this keeps at about a tenth of its cost; timing wheels
(Varghese and Lauck) for deadlines.

**What `proof` measures** (budget = bound × 1.1):
- system calls per test (`strace -f -c` over a binary of 600 empty tests) at 1 and at 15
  children in flight: at most 19;
- wall time per empty test from `Run` sent to `Ended` read: at most \(1.1 \times (T_{fork} +
  T_{exit} + 5\ \mu s)\), with `T_fork` and `T_exit` measured on the same VM;
- the queue's nanoseconds per test with a fake driver: at most 0.55 µs.

## Finding a kept result: `index::find`, `trace::Keyer`

Workload: a build script's hit with 50 outside paths of about 60 bytes (headers), 10 view files,
15 variables and a shape chain of one step. The key stream is about 50 × (1 + 8 + 60 + 1 + 32) +
25 × 50 ≈ 6 KB. The shape is about 4 KB, read in place.

The work, and its bound:
- **Reading the shape:** 4 KB at 32 bytes a cycle is 125 cycles, plus two dependent store
  lookups (the shape slot, then the result) of about 100 ns each when they miss the cache.
  That is about 0.25 µs.
- **Hashing the key:** BLAKE3 over 6 KB. Given 8 KiB at a time it hashes its 1 KiB chunks side
  by side: about 0.5 cycles a byte with AVX2 and 0.3 with AVX-512, so 3,000 cycles, 0.9 µs.
  Fed one tiny update per tag, length and name (about 300 a key), it compresses one 64-byte
  block at a time at about 2 cycles a byte, plus about 25 cycles a call. That is 12,000 +
  7,500 cycles, 5.6 µs, 6× the bound.
- **Checking 50 outside paths:** each is a path id, one probe and three loads (see the next
  section), about 15 ns warm and 50 ns cold. That is 0.75 to 2.5 µs.
- **Checking 10 view files:** query edges the database already holds, about 50 ns each, so
  0.5 µs.

Speed of light: about 2.4 µs warm and 4 µs cold.

The design:
- **`Keyer` gathers entries in an 8 KiB stack buffer** and gives BLAKE3 the whole buffer
  (applied 2026-10-29, with a test against one update per piece), so the hash is at its SIMD
  rate.
- **Nothing is allocated, and the shape is not copied:** see notes.md, Speed.
- **In the way: a `stat` per outside path** until the daemon drives `blaze_vfs::Outside`
  (v2/requests/requests-outside.md, 2, cargo's). That is 50 × 0.4 µs = 20 µs, 5 to 8× the
  bound. When the daemon drives it, a check is three loads.

## Checking an outside path: `inputs::Files::file`

Bound per check, with nothing changed: find the path's record and confirm its directory heard
nothing. That is one hash of about 60 bytes, one probe of a lock-free table (one cache line,
two when cold) and three atomic loads (`Cover::unchanged_since`).
- **The hash:** FxHash twice over 60 bytes is 2 × 8 multiplies, about 30 cycles, plus 60 bytes
  compared against the stored path, about 5 cycles.
- **The rest:** about 15 cycles warm, or 100 ns for a probe that misses the cache.

Speed of light: about 15 ns warm.

The design:
- **The path id is two seeded FxHash passes**, mixed by splitmix64's finalizer, with the path
  stored and compared on each find. A collision is looked at on the disk.
  - This was BLAKE3, which costs about 250 cycles for 60 bytes, and that alone was 3 to 4× the
    bound (changed 2026-10-29).
  - It is a speed hash, not a content one, because the table never leaves the process.
- **The answer is under a sequence lock:** two loads of the sequence and five words between
  them.
- **A file that changed** is hashed once per process, with BLAKE3 over 64 KiB reads, so the
  bound is the device: a 100 KB header at 3 GB/s is 33 µs, and BLAKE3 at about 6 GB/s (AVX2)
  hides under it.

## Assembling what a run read: `trace::assemble_seen`

Bound: each distinct name owned once, plus one check per input; the checks are bounded above.
A line is one insert into an `FxHashSet<&str>`, about 30 cycles. A test of a fork server shares
the server's lines (the queue keeps them once, `Server::common`). A proc macro's lines are
hashed where they lie in the record's buffer, with no copy (`warm::Tracer`). The design is at
the bound.

## The sandbox: tracing, `crates/blaze-cargo/src/sandbox/`

The bound for a run's tracing is that the traced run takes at most 10% more time than the
untraced run. Every read must be seen, or the key would miss an input, so the irreducible
part is one record of each distinct read.
- **macOS:** an interposed call costs its own syscall, plus about 50 ns of the library's
  (a lock-free dedup table, a working directory cached per thread). A new line costs one `write`
  of about 0.2 µs.
  - A C build script opening 500 headers and `stat`ing 3,000 paths then pays about
    3,500 × 50 ns + 600 × 0.2 µs ≈ 0.3 ms, against a run of about 300 ms: 0.1%.
- **Linux:** a traced call is a seccomp user notification. That is two context switches, with
  `SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP` putting the supervisor on the waiting thread's CPU, plus
  `recv`, `process_vm_readv` and `send`: about 2.5 µs.
  - The same script pays 3,500 × 2.5 µs ≈ 9 ms: 3%, within the bar.
  - A configure-style script that `stat`s 100,000 paths in a 1 s run pays 0.25 s: 25%, past it.
  - **In the way:** the notification round trip.
  - **What would remove it:** the macOS design on Linux. An interposed library records
    in-process, and the BPF filter lets calls through without a notification when
    `seccomp_data.instruction_pointer` is inside the library's syscall stub. That brings a call
    to about 50 ns.
  - **Not done:** a program can jump into that stub with its own arguments, and read a file
    untraced. The filesystem view and Landlock still confine it, so it can't escape, but its
    result could be keyed without that read. The stub check has to be proven unforgeable first,
    or the cost accepted for programs that do this.
- **Starting a run:** `blaze sandbox --v2` is exec'd, and it starts the program, so a run costs
  two execs. The launcher's is of blaze's own binary, about 1 ms of page faults. The bound is one
  exec, the program's. The daemon can't fork the program itself, since it is multi-threaded and
  the child would allocate.
  - **What would remove it:** a warm, single-threaded launcher per daemon that forks per run
    (buck2's forkserver, Android's zygote). That is one fork of a small process plus the
    program's exec.
  - It needs a spawn effect of the driver's (cargo's) that reaches the launcher. It is the
    next step for this lane and cargo.
  - (speed-sol2-usercode) Two execs is not all of it: each Linux run also forks a probe of
    user namespaces (about 0.1 ms) and makes every mount anew. The whole count, the bound and
    what removes each part are in "A sandboxed run's fixed cost on Linux and macOS" below.

## The Windows sandbox: `crates/blaze-cargo/src/sandbox/windows.rs`

Written by speed-link-edge-sol-usercode. Nothing here was built or run (no Windows build runs in
this pass), and the Windows figures are estimates, not measurements:
- a process creation of a small program is about 1 to 3 ms, and more in an AppContainer
  (its lowbox token) or with the antivirus scanning the image;
- an NT call on an open handle is about 0.5 to 1 µs;
- an open that resolves a path on NTFS is about 5 to 10 µs;
- `SetNamedSecurityInfoW` on an empty directory is about 30 to 100 µs, plus a rewrite of every
  file below it when the entry it changes is inheritable.

**Workload.** A build script's run: `spec.read` of 20 paths, plus the program's directory and the
working directory, so R = 22 paths checked, all granted by an earlier run (read grants are kept).
One writable directory, `OUT_DIR`, already there, which the script fills with about 5 files.
The run's temporary directory. No network.

**The bound.** One `CreateProcessW` of the program, in its AppContainer and its job from the
start, plus:
- the job (`CreateJobObjectW` and two `SetInformationJobObject`);
- the wait, the exit code and the closes;
- `OUT_DIR`'s write grant and its revoke. Each is a DACL read and write, since a run's write
  capability is a SID of its own (item 3 below), with the revoke rewriting the run's files below
  it.

What the bound does not need:
- the temporary directory's grant can be part of the `CreateDirectoryW` that makes it, and it
  needs no revoke, since the directory is removed;
- the R read checks need no system call at all where a long-lived process remembers them, until
  a watch says a DACL changed;
- handles, SIDs and the profile can be made once.

So the bound is one process creation and about 15 system calls around it, set by the program's
process creation.

**Where the code was** (after speed-link-edge-usercode's pass, which took the read checks out of
the lock):
- About 283 system calls a run:
  - 22 × 8 for the reads, each a stat by path and then a DACL read by path, so two path opens
    each;
  - 2 × 10 to grant `OUT_DIR` and the temporary directory, and 2 × (10 + its files) to revoke
    them;
  - the session lock opened and closed twice (8);
  - a suspended start, `AssignProcessToJobObject` and `ResumeThread`;
  - `exists` and `create_dir_all`'s stats on directories it had just made or found;
  - `TerminateJobObject` on a job already empty.
- ACL calls: 26 DACL reads and 4 DACL writes, against the bound's 2 and 2 plus the reads.
- Two process creations: the daemon starts `blaze sandbox --v2`, which starts the program.

**What changed** (`windows.rs`, 2026-10-29):
- **The job at creation:** `PROC_THREAD_ATTRIBUTE_JOB_LIST` puts the process in the job as it
  is made (Windows 10 and later). Gone: `CREATE_SUSPENDED`, `AssignProcessToJobObject`,
  `ResumeThread`, and the moment the process existed outside the job.
- **One handle per read path:** `covered` opens the path once (`READ_CONTROL |
  FILE_READ_ATTRIBUTES`, following links as `metadata` did), reads whether it is a directory
  (`GetFileInformationByHandleEx`) and its DACL (`GetSecurityInfo`) on that handle, then
  closes it. That is about 5 calls and one path resolution, in place of about 8 and two. A path
  that is not there is skipped as before. An open that fails for another reason (access denied)
  is now an error naming the path, where `metadata` used to skip it and the DACL read then
  failed on it anyway.
- **Directories made with their grant:** the run's temporary directory, and a writable
  directory that is missing, are made by `CreateDirectoryW` with a descriptor whose DACL gives the
  run's SID write, inheritably. It is not protected, so the system merges in what the directory
  inherits ("DACL for a New Object" in Windows' documentation). That takes no DACL read or write,
  no lock and nothing to propagate. The temporary directory is not revoked, since it is removed
  whole. A writable directory that was already there is checked with one `GetFileAttributesW`
  (a link still goes through `is_dir`) and granted under the lock as before.
- **The lock only when needed:** taken for the grants only when a read is missing or a writable
  directory was already there, and at the end only when there are write grants to take back.
  Its handle is opened once per process and kept.
- **Fewer stats:** `Container::open`'s folder check is one `GetFileAttributesW`. The temporary
  directory is made first and cleared only if it was there. `end` asks the job before ending
  it, so the usual run (nothing left running) costs one call.
- Tests: the handle check against the old stat-and-DACL-by-name check, over a directory, a file,
  a missing path, `System32` and `win.ini`, with three SID and mask sets; a directory made with
  its grant is covered for the run's SID inheritably, a file written in it inherits the grant,
  the user who made it can still list it and remove it, and `make_dir_all` finds, makes nested
  directories and refuses a file; `directory` against `Path::is_dir`; an empty job ends at once.
  The existing run test (writes only where granted, the exit code, no temporary directory left)
  covers the job list.

**Where it is now:**
- About 176 system calls a run: 22 × 5 for the reads, 10 to grant `OUT_DIR`, 25 to revoke it, 6
  around the process, the job's 3 and the rest.
- ACL calls: 24 DACL reads and 2 writes. That is the minimum for a design that starts a launcher
  per run, which can remember nothing between runs.
- What separates it from the bound is the second process. The launcher is blaze's own large
  binary, about as costly to start as the program, so a run's fixed cost is about **2 to 3 times
  the bound**.

**The warm launcher** (usercode-win-launcher, 2026-10-29; written, not built or run, and the
driver's side waits on lane core, below):
- **One launcher per daemon:** `blaze sandbox --v2-serve` (`sandbox/windows/serve.rs`), started
  by the driver outside the daemon, in no job of the driver's (Windows won't nest the run's job,
  which has UI limits, under one the launcher is in), and outside any sandbox. It reads one frame
  per run on its stdin: a fixed header (version, the driver's run number, and the handles of its
  stdin, stdout, stderr and job, which the driver duplicated into the launcher) and the run's
  `Spec`, as `confine::Launcher::hand` writes it and `confine::read_hand` reads it. It runs
  `windows::run` on a thread of the run's own and writes back a 12-byte `Answer`, the run number
  and the exit code.
- **The driver keeps the job:** it makes each run's job (killed on close, a crash ending a
  process, the memory limit) and its pipes as for a spawn it starts. So kill
  (`TerminateJobObject`), peak memory and the output stay the driver's, and no kill frame is
  needed. The launcher checks that the job it was handed kills on close (else it refuses the
  run), adds the UI limits and starts the program into it with `PROC_THREAD_ATTRIBUTE_JOB_LIST`.
- **Kept once per launcher:** the container SID and the two well-known SIDs (`Kept`, copied
  once), the session lock's handle, and the read checks (`sandbox/windows/reads.rs`). A path found
  covered is kept in a lock-free table until a `ReadDirectoryChangesW` watch on its volume's root
  (subtree, `FILE_NOTIFY_CHANGE_SECURITY | FILE_NAME | DIR_NAME`) reports that path changed. A
  removal or rename forgets everything under it. An overflow forgets the whole volume, and a watch
  that ends leaves its volume checked every run, which is reported on stderr. Only paths whose
  final path is the one asked are kept, so a link or junction is checked every run. A check
  begun before a change is reported is not kept (each check takes a token before it reads the
  DACL and publishes only if no change came in between).
- **A launcher fault ends it:** a bad frame or a failed answer stops it, with its runs' jobs
  ended and their grants taken back for up to 4 s. A panic aborts it at once, and the driver's
  handles of the jobs end its runs. A write grant left behind names a run's own SID, which
  nothing holds again. The driver fails every run in flight naming the launcher, and starts a
  new one at the next run. A run never falls back to a launcher
  per run or to running outside the sandbox.
- **Per run, steady state, estimated:**
  - In the launcher, about 70 system calls in place of 176:
    - the 22 read checks cost none (a hash lookup each), in place of 110;
    - `OUT_DIR`'s grant 10 and its revoke 25, as the bound;
    - about 6 around `CreateProcessW`, plus 3 on the job (one query, the UI limits, the
      emptiness check at the end);
    - about 5 for the temporary directory (made with its grant, removed);
    - about 6 for the frame's read, the thread and the answer;
    - about 10 for the report and live files;
    - 4 closes of the handed handles.
  - In the driver, about 15: the job and its limits (3), the pipes as now, 4
    `DuplicateHandle`s, the frame's write, the answer's read, and the job's peak.
  - ACL calls: 2 DACL reads and 2 writes, `OUT_DIR`'s, which is the bound.
  - One process creation, the program's.
- **What separates it from the bound** is about 30 cheap calls (the temporary directory, the
  report and live files, the frame and answer, the duplicates), about 50 to 150 µs against a
  process creation of 1 to 3 ms. So a run's fixed cost is about **1.03 to 1.1 times the bound**,
  in place of 2 to 3.
  - A watch overflow (many changes on a volume at once, a checkout) makes the next run check
    its 22 paths again, the 110 calls of before, once.
  - A volume that can't be watched (some network shares) is checked every run.

**The three known obstacles** (`v2/requests/usercode-sandbox.md` section 6):
- **Item 3, write grants rewritten every run:** partly removed.
  - The temporary directory's grant and revoke are gone, and so is the grant of a writable
    directory the launcher makes.
  - What is left is `OUT_DIR`'s grant, and its revoke rewriting the run's files. A capability
    derived from the directory would keep the grant between runs. The sandbox's owner declined
    that, since anyone can compute such a SID, so a sandboxed program that can name a capability
    could write into every directory whose grant stays. I agree, and it stays declined.
  - A revoke that does not propagate (`SetFileSecurityW`, which leaves the files' inherited
    copies naming a SID nothing holds again) would make the revoke O(1). It is not applied: it
    changes how the DACL's inheritance flags are stored, and nothing can be tested on Windows
    in this pass.
- **Item 4, `CreateAppContainerProfile` every run:** removed. The lead applied it: the SID is
  derived when the profile's folder is there. The folder check is now one `GetFileAttributesW`.
- **Item 5, two process creations a run:** removed on the sandbox's side, by the warm launcher
  above. The driver's side waits on lane core (`v2/requests/usercode-win-launcher.md`, 1 and 2),
  and the daemon on Windows still needs to run the IOCP driver (3 there, cargo's). The lead
  declined running the launcher inside the daemon, because a launcher fault would end the daemon,
  the job objects and console handler would be the daemon's, and `confine` would stop being one
  `Command`.
  - The removal is a warm launcher: one long-lived `blaze sandbox` process per daemon, outside
    it, that holds the jobs and the console handler, makes the SIDs and handles once, remembers
    read grants until a `ReadDirectoryChangesW` watch with `FILE_NOTIFY_CHANGE_SECURITY` says a
    DACL changed, and starts each program with one `CreateProcessW`. It is the same warm
    launcher the section above asks for on Linux and macOS.
  - It needs `Confine` to hand a spec to a running process rather than return a `Command`, which
    is the driver's API and the lead's. That is asked in `v2/requests/usercode.md`
    ("speed-link-edge-sol-usercode: one process creation per sandboxed run on Windows").
  - With it, a run is within 10% of the bound.

**State of the art:** Chromium's sandbox, where a long-lived broker makes each target once, in
its job and AppContainer, with its policy and SIDs made once. BuildXL's engine also starts each
tool once, in a job. Both are the warm launcher's design. Per run, the code is now at the floor
of a launcher-per-run design; the broker design is what reaches the bound.

**What `proof` measures** (budget = bound × 1.1):
- system calls per run (an ETW kernel trace) of the workload above: at most 194 with a launcher
  per run, and with the warm launcher at most 77 in the launcher and 17 in the driver, with no
  DACL read for a read path checked by an earlier run;
- wall time from the spawn to the program's exit for an empty program, against a bare
  `CreateProcessW` of the same program in the same AppContainer and job: the ratio, about 2 to 3
  with a launcher per run, and at most 1.1 with the warm launcher.

## A proc-macro expansion: `blaze-expand`, `proc-macro-api/src/warm.rs`

Work for a hit, for a call with 2 KB of input tokens:
- the input encoded (2 KB written) and hashed;
- the key's parts hashed;
- one shape step;
- the expansion decoded in place into a token tree.

Bound: BLAKE3 at its SIMD rate over 2 KB plus the parts, about 1,500 cycles, plus two store
lookups (0.2 µs), plus the output tree built (about 4 KB of columns written, about 150 cycles).
About 0.7 µs.

The design:
- **The variables are hashed as they are lent** (`Given::env_keyed`), so a hit allocates one
  `Vec` of borrowed pairs.
- **In the way: the input is encoded and then hashed.** frontend-tokens hands the columns to
  the wire (`usercode-wire`, in progress), and hashing those columns in place removes the
  encoding pass.
- **A miss to a warm server:** a socket round trip (2 × 5 µs), the macro's own work, and the
  trace record read with one `pread` (applied 2026-10-29).
- **Change 20** moves this to fastcompile's evaluator. The keys, the store and the trace are the
  same (v2/requests/usercode-bridge.md).

## A run's `OUT_DIR`: `run::collect`, `run::restore` (speed-sol2-usercode)

**Workload.** A build script's `OUT_DIR` as it ends: typically 1 to 5 files of a few KB
(generated Rust, a `cfg` probe's output), and for a heavy one (bindgen, a C library's objects)
about 50 files and 5 directories, 5 MB. `collect` reads it back as a run ends, to keep it.
`restore` writes it again when a kept run is reused, which is every build that finds one.

**The work, and its bound.** Per file: its bytes once, and the calls that can't be merged.
- **Reading one file:** open (resolving its path once), fstat for its mode and length, a read
  of that length, the read that sees the end, close. That is 1 × 0.4 + 4 × 0.15 = **1.0 µs**,
  plus its bytes at 10 GB/s. A directory costs its `getdents64`, and its entries' kinds come
  from the listing (`d_type`), so no call per entry. A link costs a `realpath` to check that it
  stays inside, and a stat of what it leads to.
- **Writing one file:** open with create, write, close, and `fchmod` when executable:
  0.4 + 2 × 0.15 = **0.7 µs**, plus its bytes. Each directory is one `mkdir`.
- For the heavy workload: reading 50 × 1.0 + 5 × 0.5 ≈ 53 µs plus 5 MB at 10 GB/s, 500 µs:
  **about 0.55 ms**. Writing about 50 × 0.7 + 5 × 0.4 = 37 µs plus 500 µs: **about 0.54 ms**.

**Where the code was.**
- `collect` stat'd every entry twice by its full path, once without following links and once
  following, and then `std::fs::read` opened it by path again: 7 calls a file, 3 of them
  resolving a path, about **1.8 µs**, 1.8 times the bound's calls.
- `restore` called `create_dir_all` on each file's parent. The parent is already there for
  every file but the first in a directory, so that is `mkdir` failing with `EEXIST` and a stat:
  2 more path calls a file. `std::fs::write` and then `set_permissions` by path for an
  executable gave 5 to 6 calls a file, 3 or 4 of them resolving a path, about **1.5 µs**, 2.1
  times the bound's calls.
- For the heavy workload, both stayed within 10% only because 5 MB of copying hides the calls.
  For the typical one, where the calls are the work, they were about 2 times the bound.

**What changed** (2026-10-30, `crates/blaze-usercode/src/run.rs`):
- `collect` takes an entry's kind from the listing, follows a link only to check and stat what it
  leads to, and opens each file once, reading its mode and length on the descriptor: 5 calls,
  one path resolved.
- `restore` keeps the directories it has made (a set lent from the value's paths). A
  directory the value lists, or one made for an earlier file, is not made again. A file is
  `File::create`, `write_all` and, when executable, `fchmod` on its descriptor.
- The bytes, the order, the modes and the errors are what they were.
- Test: `a_restore_makes_each_directory_once_and_collect_reads_it_back`. It covers a parent the
  value lists after a file in it, an unlisted parent, an executable file's mode, a link to a
  directory inside, which is kept as the directory and not walked, and a second restore over
  what is there.

**Where it is now:** at the bound's calls for both, so within 10% for either workload.

**What is left.**
- A reused run decodes its value (`Value::decode`), which copies every file's bytes out of the
  store before `restore` writes them. That is one more pass over the bytes at about 20 GB/s,
  against the write's 10 GB/s: about 50% on the bytes of a heavy `OUT_DIR`, 0.25 ms of 5 MB.
- The only caller of `Begun::Reused` (`blaze-cargo/src/serve/unit/script.rs`) reads its code,
  signal, stdout and stderr, never its files. So a value restored from the store's bytes in
  place, with `Begun::Reused` carrying no file bytes, removes the copy. That changes what
  `Begun::Reused` holds, which is an interface cargo's code builds against, so it is asked of
  cargo in the queue rather than done here.
- Beyond that, `copy_file_range` or a reflink from the store's file would make a restore
  copy no bytes in the page cache at all. That needs the store to keep `OUT_DIR` files as files
  of their own, which is lane store's design.

**What `proof` measures:** system calls (strace's count) of `collect` and `restore` over the
two workloads: at most 5 a file read, 4 a file written, 1 a directory. Also the wall time of
`restore` of the heavy workload against `cp -r` of the same tree.

## A sandboxed run's fixed cost on Linux and macOS (speed-sol2-usercode)

What a build script or probe pays to run in the sandbox, beyond its own work and what tracing
costs per call (that is "The sandbox: tracing", above). Counted from `blaze-usercode/src/confine.rs`
and `blaze-cargo/src/sandbox/{v2,linux,macos}.rs`.

**Workload.** A traced build script: R = 20 readable paths, W = 1 writable (`OUT_DIR`), on a
machine with M = 40 mounts (a NixOS or container host has a few hundred), 16 to 64 scripts
started in parallel at the start of a cold build.

**The bound.** The program's own exec, plus the isolation itself: one fork from a warm process,
the namespaces (user, mount copied from the host, network, IPC, PID), the binds that differ per
run (W), the run's `/proc`, and a seccomp filter. Every other step is the same for every run and
can be made once per launcher. That is the exec (100 to 300 µs) plus about 30 µs of fork, 0.1 ms
of network namespace, M × 1 µs for the mount copy, W × 10 µs of binds and 20 µs of `/proc`:
**the exec plus about 0.2 ms.**

**Where the code is, on Linux** (each run, from the driver's spawn to the program's `main`):
- the driver writes the spec (`Launcher::confine`): openat, write, close;
- **the launcher's exec**, `blaze sandbox --v2 SPEC`, blaze's own binary: about 1 ms;
- it reads and parses the spec, opens and writes the live record, fstats its three standard
  descriptors (`given`): about 12 calls;
- **a probe of user namespaces** (`linux::namespaces`), on every run: a fork, `unshare` of a
  user namespace, three opens, writes and closes of its maps, the exit and the wait: about
  **0.1 ms**, to decide between namespaces and the confined sandbox. The answer is the same for
  every run of a launcher;
- the interposer's file read whole and compared (`interposer`), and the spec cloned with its
  directory: about 10 µs;
- a fork of the sandbox's first process. It unshares user, mount, IPC and network namespaces,
  writes the maps (9 calls), makes the mounts private, opens R + W descriptors, makes every mount
  read-only (`mount_setattr`, O(M)), mounts 2 tmpfs for staging and up to 5 empty ones (`EMPTIED`,
  each after an lstat), lists `/run` and remakes its links, binds R + W directories, each after a
  `create_dir_all` (2 calls), brings up the loopback (3), unshares a PID namespace and forks;
- the PID namespace's first process mounts `/proc` and forks the program's process, which
  installs the filter, sends its descriptor and execs.

That is the exec plus about 1.4 ms: 1 ms of the launcher's exec, 0.1 ms of probe, three forks
(90 µs), about 30 mounts (R + W + 7 tmpfs, about 0.25 ms) and about 100 cheap calls. With an exec
of 0.2 ms, that is **1.6 ms against 0.4 ms, about 4 times the bound**. Of the 1.2 ms over:
- the launcher's exec is 1 ms;
- the probe is 0.1 ms;
- the R binds that are the same every run are about 0.2 ms, and they serialize across parallel
  runs on `namespace_sem`;
- the second fork (the PID namespace's first process apart from the sandbox's first) is 30 µs.

**Where the code is, on macOS:** the launcher's exec (about 1 ms); the interposer library read
whole and compared (`library`); the profile's text made and its parameters (the writable
directories, the live record, the sockets, the services), a config file written; one fork, in
which `sandbox_init_with_parameters` compiles the profile and applies it; the exec, with the
interposer loaded by `DYLD_INSERT_LIBRARIES`. The profile's compile is in the bound while its
parameters differ per run: Seatbelt binds parameters when it compiles. So what is over the bound
is again the launcher's exec, about 1 ms.

**What stands in the way, and what removes it.**
1. **The launcher's exec, 1 ms a run on both.** A warm launcher, one per daemon, that forks a run
   from itself: Windows has one (`--v2-serve`, "The Windows sandbox" above). On Unix, the
   driver's side (a Unix socket, the run's descriptors passed with `SCM_RIGHTS`, `Handed`'s
   Unix meaning already in `blaze-core`) is queue 00551, to cargo, open. The `uring` and
   `kqueue` drivers do not ask `Confine::warm` yet. Until they do, `Launcher::warm` returns
   `None` off Windows, and the launcher's side waits for that protocol.
2. **The probe, 0.1 ms a run.** It moves into the warm launcher, asked once as it starts. In a
   launcher per run there is nothing to keep it in. The driver can't ask it either: it is
   multi-threaded and the probe forks. Deciding by trying the real `unshare` in the sandbox's
   first process would need the supervisor's recorder, filter and directories to change after
   the fork, so it is not done.
3. **The binds that are the same every run.** With a warm launcher, a template mount namespace
   made once (every mount read-only, the empty directories, the R binds of the build's
   toolchain and registry) would leave a run to copy it (M µs), bind its own W and mount its
   `/proc`. bubblewrap and nsjail make every mount per run; this is the zygote's design applied
   to the mounts.
4. **The second fork.** `clone3` with `CLONE_NEWPID` makes the sandbox's first process the
   PID namespace's first too, 30 µs. The C library's fork handlers don't run for a raw
   `clone3`, which is safe in the launcher's own single-threaded process.

With 1 and 2 a run is the exec plus about 0.4 ms, 1.5 times the bound. With 3 and 4 it is the
exec plus about 0.2 ms, within 10%. Items 2 to 4 are this lane's own once 1 lands.

**State of the art:** buck2's forkserver and Android's zygote for the warm launcher; bubblewrap,
nsjail and Bazel's `linux-sandbox` make every namespace and mount per run, which is where the
code is today; Chromium's broker on Windows.

**What `proof` measures:** the wall time from `Effect::Spawn` to the program's first instruction
(a program that is `_exit(0)` in `_start`), against an `execve` of the same program outside any
sandbox. The budget is the difference at most 1.1 × 0.2 ms with the warm launcher, and today's
count meanwhile. Also the calls of the launcher per run (strace), and mounts per run.

## What is left in `blaze-build-script` (speed-sol2-usercode)

Two gaps the "Build scripts" section above records are still open after this round. Both are in
free code of this lane, and are in the queue for it:
- **Elision's lexer, 3.3 times its bound** (about 3.3n cycles a byte against 1.0n). A
  `std::simd` stage 1 and a lexer at about 10 instructions a token, with a differential test
  against `elide/lex/tests/reference.rs`, as that section says. Not done this round: it is a
  rewrite of `elide/lex.rs` that must be proven against the reference, and this round had no
  room for it. In absolute terms it is about 0.2 µs over, per edit of a 300-byte root file.
- **The environment's sort, about 15% over.** It needs the fixed names as slots indexed by a
  `const` sorted table at each `cmd.set` call (about 40), so that only the variable part
  (`[env]`, features, cfgs, `DEP_*`) is sorted before one merge. That brings it to about 8%
  over. It saves about 1,700 cycles a unit, behind the 6,000 cycles of allocations that queue
  00209 (`Output` and `Env` as one buffer with spans) holds.

## What `proof` measures, for the sections above

- Cycles of `index::find` on a hit, with the workload above: warm and cold, and with `Files`
  vouched and not.
- Cycles of `Files::file` on a vouched path.
- The traced-to-untraced time of the three script kinds above, on Linux and macOS.
- The time to start a sandboxed run, from the `Effect::Spawn` to the program's first
  instruction.
