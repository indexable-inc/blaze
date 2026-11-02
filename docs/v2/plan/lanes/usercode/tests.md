---
status: plan
summary: How blaze v2 runs tests (a fork server per binary, its wire, what each test executed, one queue over every binary, which tests run at all), and how each part compares with the fastest known way.
date: 2026-10-29
---
# Tests

Part of [usercode](../usercode.md). Code: `crates/blaze-usercode/src/test.rs` and
`crates/blaze-usercode/src/test/`. Requests: `v2/requests/usercode-fast-tests.md`.

A request's tests go through one sans-IO queue (`test/queue.rs`) over every test binary.
Each binary starts once, in its sandbox or outside it ([below](#in-the-sandbox-or-outside)), as
a fork server (`test/fork_server.rs`, text the
harness compiles in); the server forks a child per test, and answers how it ended, what it
wrote and what it executed (`test/wire.rs`). Before any of that, selection (`test/select.rs`)
skips a test whose pass is kept for the code it reaches and the files it read. Nothing here is
measured yet: the comparisons are of technique, and the benchmark to settle them is under
[What is not done](#what-is-not-done).

## A process per test, from a fork server

What we do: the binary's `main` connects to blaze's socket and says hello with its tests. For
each run blaze asks, it forks a child that marks its scope in the sandbox's record and runs
the one test as libtest would; a pidfd (Linux) or kqueue (macOS) wakes the server when a child
ends, so it never polls. Everything a test pays for once per binary (exec, dynamic linking,
the runtime's start, libtest's test list, the sandbox) is paid once per server; a test costs a
`fork`, its run and its exit.

The fastest known way:

- **AFL++'s fork server** is the same shape: the target starts once, stops before `main`'s
  work, and forks a child per input on a pipe's word. Ours is it, with tests for inputs.
- **AFL++'s deferred fork server** stops later, after expensive start-up the inputs don't
  change. A test binary's start-up past `main` is libtest's list, which we already skip
  (the server has it); a crate whose tests share a `static` built lazily rebuilds it in every
  child. Matching this needs a hook a test crate can call ("ready"), a blaze extension.
- **AFL++'s persistent mode** runs many inputs in one process with no fork, resetting state
  by hand, for 10-20x more inputs a second. Tests can't be reset by hand: a test's statics,
  threads, file descriptors and panics are its own, and libtest's process isolation is part
  of what running a test means here (#1443). We don't do this and shouldn't, except as an
  opt-in for crates that say their tests are pure.
- **Snapshot fuzzing** (AFL++'s snapshot LKM, Nyx) restores a process's dirty pages instead of
  forking: cheaper than `fork` for a big address space, since `fork` copies page tables. A
  test binary's address space is small next to a fuzzer's VM targets; `fork` is within a
  small factor, and needs no kernel module.
- **nextest** runs a process per test too, but each one is a fresh `exec` of the binary
  (`--exact name`): exec, dynamic link, runtime start and libtest's list for every test. We
  beat it by what a fork saves over an exec, which for a large test binary is most of a
  short test's time. nextest's advantage is that every test starts from a clean `main`; a
  fork starts from the server's state, which is `main` before any test ran, so the two see
  the same state but for `static`s that the hello's list touched (none: the list is static
  data).

One server forks one test at a time, so after its hello it is asked for `K - 1` replicas
(`Frame::Replicate`, wire version 3): copies forked before any test runs, in halving rounds,
each connecting anew on the same socket with its own hello. The queue hands each test, in the
one order, to the first replica with room, and `K` comes from the cores, the fork the server
measured and the binary's median test (speed-of-light.md, "Tests"). A replica that dies loses
only the tests it ran, with which replica and why; the server is killed only when none is
left. The first server's own end (the process blaze waits for) still ends them all.

Where we fall short: a dead replica's tests come back lost, not rerun. A forked child of a
multithreaded server would be unsound; the server has one thread, and the queue never asks it
for more.

## The wire (`test/wire.rs`)

What we do: frames of a `u32` length and a body, read in place. A frame is decoded as a
borrowed `View` (every length checked against the bytes, every string checked UTF-8, nothing
copied); the server encodes each end straight from the buffers its child's output and its hits
were read into (`encode_ended`), into one buffer reused for every frame. `Frames` keeps one
buffer for the stream, skips over what it read and moves the rest only when a push would grow
it, and says how many bytes the next frame needs, so a long frame comes in one read.

The fastest known way: this is the shape of any length-prefixed binary protocol done well
(Cap'n Proto's in-place reads without its schema; AFL++'s fork server sends 4-byte words).
Ours costs one pass over each byte and no allocation on the server's side. blaze's side still
owns what it receives: the driver hands each read as a `Vec` and the queue keeps each test's
output, so an end costs the copy into its result's `output`.

Where we fall short: blaze's side copies a test's output once more than it must (driver
buffer, then the result's `Vec`); a driver that lent its read buffer would save it. That is
the driver's API (`blaze_core::Done::Read(Vec<u8>)`), not this module's.

## What each test executed

What we do: a server built with jump-table slots asks the runtime how many slots it has once,
and before each fork hands the child a region shared with it: a 16-byte header (`blzexec1`, the
slot count, the slots armed) and a byte per slot. The child's runtime arms it, then each slot's
first call sets its byte. Once the child has exited, the server reads the region by the rules
of `blaze_testsel::record::Record::of_region` and sends the slots set. The read is four words
a step: a group of zero words is skipped in one compare (most of a binary never runs in one
test), and
in a word that is not zero the set bytes are found by `((w & 0x7f..) + 0x7f..) | w) & 0x80..`
and a count of trailing zeros per hit, the last word masked to the slots it holds. A test of
a binary without slots says so ("built without jump-table slots"); one that did not exit, or
whose runtime did not arm every slot, says why its record is not whole.

The fastest known way:

- **AFL's coverage map** is the same idea: a shared region the child writes (an edge counter
  per byte) and the parent reads after each run, word at a time, skipping zero words. Ours
  records first calls, not edges, which is what selection needs and is one byte write per
  function per test.
- **SanitizerCoverage's `trace-pc-guard`** is the compiler-side equivalent: a guard per edge,
  whose callback can clear it after the first hit. Ours is the jump table's slot (`fastcompile-lazy`), so a binary
  pays nothing it doesn't already pay to be lazily linked.
- **AVX2 or NEON** would read 32 or 16 bytes a step; `u64` SWAR is within 2-4x of that for a
  region that is mostly zero, portable, and safe code (the module is compiled into every test
  crate, in every edition).

A region read is kept for the next test: the server clears the bytes the read found (all of
them after a record that was not whole) and writes the header again, so a steady stream of
tests makes no `mmap` or `munmap` and allocates no page; the server maps only as many
regions as tests run at once. A process a test left running may still set a byte in a region
another test now has; the runtime only sets bytes, so that test's record may hold a slot it
did not call and never lacks one it did: selection reruns more, never less.

Where we fall short: the bound and the gaps are in `speed-of-light-tests.md`.

## One queue over every binary (`test/queue.rs`)

What we do: every test of every binary is a `u32` unit; the order is the longest expected
first (a stable radix sort on the complement of each unit's expected nanoseconds), and what is
pending and what is ready are bitsets over ranks, so the next test is the first set bit. An
attempt is a slot in a slab (its id is the slot and a generation, so an end of an old id names
nothing), a tag is a slot in another, and each server's record lines are interned: what a
line names is a `u32` in the server's table, and a line is that id and its kind in one `u32`,
so a line seen again (libc, every test) costs one hash and one compare. Every deadline (a
test's timeout, the grace after its kill, a binary run whole) is a timer in one hierarchical
timing wheel (11 levels of 64 slots, 1 ms ticks); the driver holds one timer, at the wheel's
next expiry. A test started and ended allocates nothing past its result once the slabs are
warm.

The fastest known way:

- **Critical path first** is the classic longest-processing-time rule (Graham's LPT), which
  Bazel and buck2 use for actions with a duration history, and which bounds the makespan
  within 4/3 of optimal on identical machines. We have it with no estimate for a new test
  (it goes last, at zero): a better prior (the binary's mean) would start unknown long tests
  sooner.
- **Bazel's test sharding** splits one test binary over N processes by index modulo N. Ours
  is finer: one queue of single tests over every binary, so no shard waits on its slowest
  member. Bazel's remote execution spreads over machines; ours is one machine's cores.
- **Timing wheels** (Varghese and Lauck) give O(1) insert and cancel, which is what the
  kernel and every fast event loop use; a heap is O(log n). We match it.
- **nextest** schedules by test order and a thread pool, with no duration history; we beat
  it on a suite with a few long tests by starting them first.

Where we fall short: a server's tests can only start once it said hello, so the first test of
a binary waits for its exec and sandbox; a pool of warm servers (#1443's warm pool) would
hide that for the next request. Retries keep their place in the order but always go to the
same server.

## In the sandbox or outside

Tests run outside any sandbox, as cargo runs them, unless the workspace opts into the
`sandboxed-tests` experiment or the package says `tests = true` under
`[package.metadata.blaze.sandbox]` (docs/cargo.md, "Sandboxed tests"). `test::Binary::sandboxed`
says which, and cargo sets it (`v2/requests/usercode-tests-off.md`). It is `false` by default,
and `false` for a package with `tests = false` and for every binary under `BLAZE_SANDBOX=off`.

- **Outside**, the binary is spawned with `Sandbox::Off`: no record is made or read, and no
  `confine::LIVE` is set. Its fork server and replicas run each test in a process of its own
  as inside. A child's scope mark is an `access` of a path that isn't there, which nothing
  sees. Each result's trace is `Trace::Unknown(OUTSIDE)`, so selection keeps no pass, and
  cargo also marks the test `outside` so it runs every time. A binary run whole as libtest's
  threads is the same.
- **`unsandboxed-tests`** in an opted-in package makes a second `Binary` of the same program,
  outside, whose jobs are the tests named. The sandboxed one gets the rest. This is libtest's
  `--exact` split: each binary's jobs are exact names, a process each or, run whole, `NAME...
  --exact`. Each binary has its own socket and record (`t{b}`, `r{b}`) by its place in the
  queue.
- **Outside after inside.** A binary outside starts only once every sandboxed test in the queue
  has its result. An outside test writes the machine for real, and a sandboxed test's reads are
  hashed as its result is drained, so running the two at once could key a pass to files the
  test never saw. In a workspace that doesn't opt in, every binary is outside and nothing
  waits. In one that does, the outside tests are the few a package named.
- **No fallback.** Nothing goes outside except by this field. A sandboxed binary whose sandbox
  can't be made loses its tests with why, as before.

A test outside costs the queue less than one inside: no record read and no lines, only the
reason string of its trace (inside the steady-state budget of 8 allocations a test,
`the_steady_state_reuses_its_slabs`, run both ways).

## Which tests run at all (`test/select.rs`)

What we do: a test runs only when it is new, reaches code it did not reach when it last
passed, read something at run time that is not as it was, has no pass kept, or reaches the
outside world. A pass is keyed by content (the test's name, the code hashes it reaches, its
attributes, its suite), then by what it read at run time, as a build script's result is; a
test's history is a chain of entries whose end is found by doubling and halving, each entry
read in place so what it read is decoded only to explain a rerun.

The fastest known way:

- **Meta's predictive test selection** (Machalica et al., 2019) and **Sapienz/TestPilot**
  learn which tests a change is likely to break and run those, running a fraction of the
  tests at the price of a small share of failures missed. Ours is exact: a test is skipped only when nothing it reached or read
  changed, so it never misses a failure. The cost is that a change to a function every test
  reaches reruns every test; theirs would run a sample.
- **Google's TAP** and Bazel select by the build graph: a test reruns when any dependency of
  its target changed. Ours is finer by the call graph within a crate (a test reaches some of
  its crate's functions) and by run-time reads (a fixture file it opened).
- **Ekstazi** (Gligoric et al., 2015) selects Java tests by the class files each one loaded
  and the files it read, the closest prior work. Ours does the same with function-level code
  hashes, and with an execution record (above) as the next step: keyed by what a test
  executed, not what it could reach.

Where we fall short: a kept pass is checked by building the whole record of the test's reads
as they are now and hashing it (`Shape::values`), where the store's own index check hashes
each input into the key as it reads it (`index::search`, requested as `pub(crate)`:
`usercode-fast-tests 1`). And a pass is keyed by the code hashes a test can reach, not the ones
it executed; `fastcompile-ident-select 7` moves selection onto the execution record.

## Memory ([memory.md](../../design/memory.md))

Each structure by how long it lives, and its allocator. **Now** is what the code does today;
**Then** is where it goes on `blaze-mem` (tag `usercode`).

| what | lives | now | then |
|---|---|---|---|
| the queue's columns (unit to binary and job, order, ranks, tries) | a test run (request) | `Vec`s sized once in `Queue::new`, never grown | the run's request arena |
| attempts, tags, the wheel's timers | come and go per test | slabs with free lists, generational ids (attempt id = generation << 32, slot), grown to what runs at once | `blaze_mem::Slab`, whose `Handle::to_bits` is the same id |
| a server's interned names, its lines of every scope | a server (request) | `Names` (one text buffer, one index) and an `Arc<Run>` shared by its results | `RegionVec`s, which never move |
| a result's output, name, execution record, own lines | a test run (request), read by cargo | heap `Vec`s and `String`s owned by `Finished` | the run's request arena, `Finished<'r>` borrowing it |
| a test's identity and reached set: sort keys | a call | the worker's scratch (`blaze_mem::scratch::with_u64s`), hashed from a 16 KiB stack block | (done) |
| the radix sort's counts and scratch | a call | three heap buffers per sort, inside `blaze-radix` | a caller's scratch |
| the fork server (in the test binary) | the test binary's process | std only: it runs in the user's binary, which links no blaze crate | (stays) |

The warm loop's allocations today are the result's (8 per test, `the_steady_state_reuses_its_slabs`) and the radix sort's (3 per identity of a large set,
`an_identity_allocates_only_what_the_radix_sort_does`). Both go to zero with the request
arena and the radix scratch (`usercode-fast-tests` 11 and 13).

## What is not done

- A benchmark: a suite of many short tests in one binary (fork server against nextest's exec
  per test), a suite with a few long tests (the order against test order), and a rerun with
  one function changed (selection against all). Each in an ix VM, cold and warm.
- A duration prior for new tests; warm servers across requests.
- `index::search` in selection (`usercode-fast-tests 1`), and a borrowed `trace::assemble` for
  the lines a fork server's test read (`usercode-fast-tests 2`).
