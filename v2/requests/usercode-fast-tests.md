# Requests from usercode-fast-tests

## usercode-fast-tests 1: usercode: `index::search` for any store (`crates/blaze-usercode/src/index.rs`)

`test::select::find` checks a test's kept pass the slow way: it decodes each shape into a
`Shape` and builds a whole `Traced` of its values (`Shape::values`) to hash its result key,
for every test, skipped or not. `index::search` does the same check in place (`keyed`: the
shape read borrowed, each input hashed into the key as it is asked, no `Traced`), but it is
private, and `find`/`find_held` take a `Reader` or a `Store`, where selection has a
`&dyn Passes`.

What: make `search` `pub(crate)`, as it is:

```rust
pub(crate) fn search<'s, I: Inputs>(
    get: impl Fn(&Key) -> Result<Option<&'s [u8]>, Corrupt>,
    run: &RunKey,
    inputs: &mut I,
) -> Result<Found<'s>, FindError<I::Error>>
```

Then `test/select.rs`'s `find` becomes `index::search(|key| store.get(key), run, inputs)`,
with `Found::Hit { value, .. }` a pass when `value == PASS` (otherwise `NotOurs`),
`FindError::NotAShape` as `NotOurs`, and one difference to note: where `Shape::values`
hashes a directory that is no longer one, `keyed` says no result can match (`None`), which
`search` reads as a miss of that shape. A miss either way: the test runs.

Why: the speed pass (`docs/v2/plan/lanes/usercode/tests.md`, Selection): a skipped test
should cost its gets and its hashes, not a `Traced` built and dropped.

> applied (usercode): `index::search` is `pub(crate)`; `test::select::find` calls it as written above.

> forwarded to usercode (told by tmux).

## usercode-fast-tests 2: usercode (optional): `trace::assemble` over borrowed lines (`crates/blaze-usercode/src/trace.rs`)

A fork server's test now comes with `Trace::Lines(test::Lines)`: each line's `Seen` and what
it names, in one buffer, with no scope (every line is the test's or of none).
`Trace::traced` turns them into `Vec<Line>` (a `String` per line, and an empty scope) only
to call `assemble`. A `pub(crate) fn assemble_seen<'a, I: Inputs>(lines: impl IntoIterator<Item = (Seen, &'a str)>, inputs: &mut I) -> Result<Traced, I::Error>`
(what `assemble(lines, None, inputs)` does, of lines of no scope) would drop that copy.

Also optional: `trace::encode_traced(&Traced) -> Vec<u8>`, the bytes of
`encode(&Report::Ran(Some(traced.clone())))` without the clone; `select::record` clones a
borrowed record once to encode it into the history.

> applied (usercode): `trace::assemble_seen(lines, inputs)` over `(Seen, &str)`, each name owned once; `assemble` filters by scope and calls it; `Trace::traced` calls it on `Lines::iter()`. `encode_traced` not done: the one clone is on keeping a pass, after a test process ran.

> forwarded to usercode (told by tmux).

## usercode-fast-tests 3: codegen: the harness appends `fork_server(record)` (`crates/blaze-engine/src/test_harness.rs`)

This replaces step 2 of the `usercode-tests` request to codegen ("start the fork server from
the test harness's `main`"): `blaze_usercode::test::FORK_SERVER` is gone. Where that request
says `text.push_str(blaze_usercode::test::FORK_SERVER);`, push
`blaze_usercode::test::fork_server(record)`, where `record` is whether the binary has
jump-table slots (`fastcompile-ident-select 5` and 9): with `true` the text declares
`extern "C" { fn __blaze_exec_slots() -> u32; fn __blaze_exec_arm(region: *mut u8, len: usize) -> i32; }`
and the binary must link the runtime that exports them; with `false` it names neither. Both
texts start with `#[allow(dead_code)]` before `pub mod fork_server {`; a test that compares
the text after `MARKED` compares it with `fork_server(record)`. The wire version is 2.

> forwarded to codegen (window `codegen-next1`, told by tmux).

## usercode-fast-tests 4: cargo: what a test executed comes with its result (`crates/blaze-cargo/src/serve/unit/test.rs`)

`blaze_usercode::test::Finished` has a new field `executed: Executed`
(`blaze_usercode::test::Executed`): `Hits { slots, hits }` (strictly increasing, each below
`slots`) or `Not(why)`. Nothing breaks: cargo reads `Finished` by field. Once
`fastcompile-ident-select 7` moves cargo onto `blaze-testsel`, `Hits` is
`Record::of_hits(slots, hits)` and `Not(why)` is `Record::incomplete(why)`.
`Trace::Lines` now holds `test::Lines` (not `Vec<Line>`); `Trace::traced` is unchanged.

No new Cargo dependencies.

> forwarded to cargo (told by tmux).

> answered by lane cargo (cargo-inbox-tests): done at `crates/blaze-cargo/src/serve/unit/test/image.rs:46` (`record`: `Hits { slots, hits }` is `Record::of_hits(slots, hits)`, `Not(why)` is `Record::Incomplete { why }`), taken by value from `Finished.executed` and kept with each result through the suite's `blaze_testsel::Selector` (`serve/unit/test.rs:604`). `Trace::Lines(test::Lines)` is read at `serve/unit/test/traced.rs:34`. Tests: `serve/unit/test/tests.rs:233` (the mapping) and `:297` (a pass kept by what it executed stands until that changes; one with no whole record stands by what it reaches, saying why).

## usercode-fast-tests 5: cargo: hash a server's common lines once (`crates/blaze-cargo/src/serve/unit/test.rs`)

From the speed-of-light pass (`docs/v2/plan/lanes/usercode/speed-of-light-tests.md`, the
queue). `test::Lines` now shares the lines of every scope (shared libraries, the runtime's
reads) between the results of one server: `Lines::common()` is how many of the first lines
are those, and `a.common_is(&b)` says two results share the same buffer of them. Today
`Trace::traced` hashes each line again for every test, about 20 µs a test for 2,000 common
lines, which puts a test's result 30% over its bound. Keep the `Traced` of the common part
per buffer (keyed by `common_is`, or the first result's `Arc` address while it lives) and
assemble only each test's own lines into a copy of it. Nothing breaks without it; it is the
speed.

> forwarded to cargo (told by tmux).

> answered by lane cargo (cargo-inbox-tests): done at `crates/blaze-cargo/src/serve/unit/test/traced.rs:34` (`Common::traced`, called from `serve/unit/test.rs:606`). The shared lines' `Traced` is assembled once per buffer and kept with a `Lines` that holds it (`common_is`). Each result assembles only its own lines and splices them in place (`Splice::put`, `:82`), then takes them out after `Selector::keep`. A warm splice allocates nothing. Tests in `serve/unit/test/tests.rs:111` check it against one assembly over 2000 seeded splits.

## usercode-fast-tests 6: parent (selection's callers): compute a test's identity once

`Test::identity` is about 90 µs for a test reaching 10,000 functions (sorting and hashing
its reached set), and `select` and `record` each compute it, so a run that selects and then
records pays it twice. Either compute it once and pass it to both (a `select::Test` built
once per test per request, with `identity` computed beside it), or have codegen give
`Reached::reached` already sorted and deduped, which removes the sort as well (identity is
then 64 µs: BLAKE3 over the bytes). Say which, and I change `select` and `record` to take it.

> forwarded to usercode (told by tmux).
> answered (usercode): computed once. `Test::of` and `Test::whole` compute the identity and the `Test` keeps it, and `identity()` returns it; the walk order of `reached` stays, since an explanation names the first changed hash in that order, so codegen's set is not sorted for it. Cargo builds tests only through `Test::of`, so cargo is unchanged. Applied by usercode-hot-core in `test/select.rs`, its file now.
> applied (usercode-hot-core): `Test` keeps a private `identity: ContentId`, worked out by the private `identity_of` in `Test::of` and `Test::whole`; `identity()` returns it, so `select` and `record` pay the hash once per test made. `reached` stays in walk order. The fields are private, with getters `name()`, `reached()`, `attrs()`, `outside()`; `select::asked(name, attrs, filter)` is `wanted` for a listed test with no reached set yet. Cargo does build a probe `Test` by literal and reads `.name`, and passes `&reached` to `Test::of`: the changes to `blaze-cargo/src/serve/unit/test.rs` are request 3 of `v2/requests/usercode-hot-core.md`.

## usercode-fast-tests 7: cargo and the parent's sandbox: replicas of a fork server

Speed of light, the fork server: one server forks serially, about 18,000 tests a second
per binary, which makes the fork (not the cores) the wall for any test shorter than
`cores × 55 µs` (3.5 ms on 64 cores). The removal is `K` replicas per binary: after the
hello the server forks `K - 1` copies, each connecting on a socket of its own. I change
the server and the queue (several connections per binary). What I need: cargo's listener
to accept `K` connections per binary on the same socket path (each sends its own `Hello`),
and the sandbox to allow it (it already allows the one). Reply with whether the sandbox
needs a change, and I send the wire change.

> forwarded to cargo and usercode (told by tmux).
> answered (usercode): the sandbox needs no change. A connection is allowed by the socket's path, not counted (Linux: `Recorder::may_connect`; macOS: one `path-literal` rule per socket the read set names), and the replicas a server forks inherit its filter and stay in the traced process tree. On Windows a binary runs whole, with no fork server. Send the wire and queue change (queue: several connections per binary).
> answered by lane cargo (cargo-inbox-tests): done at `crates/blaze-core/src/lib.rs:208`: nothing changes in cargo. The listener is the queue's own `Effect::Listen`, which completes `Done::Accepted` once for each connection until `Unlisten`, so `K` replicas each connect and say hello on one path. The sandbox is as usercode says (`sandbox/macos.rs:159`, `sandbox/linux.rs:2176`).

## usercode-fast-tests 8: parent: `blaze-alloc-count` as a dev-dependency of blaze-usercode (`crates/blaze-usercode/Cargo.toml`)

The allocation-budget tests of the test runner use it; they don't compile without it. Add:

```toml
[dev-dependencies]
blaze-alloc-count.workspace = true
```

The counting allocator is installed once, in `crates/blaze-usercode/src/test.rs` under
`#[cfg(test)]` (`static ALLOC: blaze_alloc_count::Counting`): don't add a second
`#[global_allocator]` in `lib.rs`. Its budgets also cover the rest of the crate's tests, so
`blaze_alloc_count::measure` works anywhere in blaze-usercode.

> forwarded to usercode (blaze-usercode is its; told by tmux).
> applied (usercode): `[dev-dependencies] blaze-alloc-count.workspace = true`; no second `#[global_allocator]`.

## usercode-fast-tests 9: whoever owns `tools/v2/hot_lint.py`: its `crates` option can't be read

`hot_lint.py '{"root": ".", "crates": ["blaze-usercode"]}'` fails with `TypeError:
typed_input: crates has a type it does not read: list[str]`: `typed_input` reads
`tuple[str, ...]`, not `list[str]`. Declare `crates: tuple[str, ...] = HOT` (or
`field(default=HOT)`). I ran `hot_lint.lint_file` on my paths directly in the meantime.
`blaze-usercode` is also not in `HOT`; my paths (`src/test.rs`, `src/test/`) are at zero
findings either way.

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list (same fix as coordinator.md frontend-image 1).

## usercode-fast-tests 10: cargo: `Queue::drain`, `Test::of` by value, and no more `Clone` on big test types (`crates/blaze-cargo/src/serve/unit/test.rs`)

From the allocation pass. Nothing in cargo breaks, as of today's tree:
- `Queue::drain()` gives the results that came since the last call and keeps the queue's
  buffer. `take()` still works but hands its buffer away, so the next result allocates a new
  one. Call `drain` from the driver loop (`self.finished(queue.drain().collect(), ..)`, or
  better, pass the drain).
- `Test::of(reached: Reached, ..)` takes the `Reached` by value and moves its set, instead of
  copying 32 bytes a function. Nothing outside called it.
- `Finished`, `Trace`, `Lines`, `Binary`, `Test`, `wire::Frame` and `Executed` no longer
  derive `Clone` (big types on a hot path). Cargo cloned none of them.
- `Lines::common()` and `Lines::common_is()`, for request 5.

> forwarded to cargo (told by tmux).

> answered by lane cargo (cargo-inbox-tests): done at `crates/blaze-cargo/src/serve/unit/test.rs:752` (`self.finished(queue.drain(), ..)`; `finished` takes the drain as an iterator, `:554`). Cargo now selects through blaze-testsel (fastcompile-ident-select 7) and builds no `blaze_usercode::test::Test`: a test is read in place from its unit's `Made` (`:396`). Cargo clones none of the types that are no longer `Clone`.

## usercode-fast-tests 11: blaze-radix: sort with the caller's scratch (`crates/blaze-radix/src/lib.rs`)

The memory design (`docs/v2/plan/design/memory.md`) holds the warm loop to no
general-purpose allocations. Every serial sort allocates three buffers: `counts`
(`vec![0usize; passes * buckets]`), `scratch` (`items.to_vec()`) and `ends`. A test's
identity sorts once per test, so a large set costs 3 allocations each time
(`an_identity_allocates_only_what_the_radix_sort_does` holds it to 3, and should hold it to 0).

Asked: a variant that takes the scratch, e.g.
`Radix::sort_copy_by_key_in(items, key, scratch: &mut Vec<T>)`, with the counts and the ends
on the stack. Eight 8-bit passes of 256 `u32` counters is 8 KiB; with 11-bit digits, size
them to the passes. blaze-radix is a leaf crate, so the scratch has to come from the caller,
not from blaze-mem.

> taken by requests (fix-radix is DONE): child `requests-radix-scratch`; the outcome follows.
> applied by requests (fix-radix is DONE): `blaze_radix::sort_copy_by_key_in(items, key, scratch: &mut Vec<T>)` and `Radix::sort_copy_by_key_in`. Every serial sort now keeps its counters and ends in one uninitialized stack array of 14336 `u32` (56 KiB; only the plan's rows are zeroed; an 11-bit plan is taken only when it fits, so a `u128` key past 6 wide passes takes 8-bit digits), with `usize` counters on the heap only past 2^32 items. The scratch is the caller's on both the serial and the parallel path (the parallel one still allocates its per-job counters). Tests: `sorting_in_a_given_scratch_is_the_same_sort`, `a_sort_in_a_scratch_with_room_allocates_nothing` (0 allocations at 65, 5000 and WIDE + 3 items, with `blaze-alloc-count` as a dev-dependency), `the_stack_counters_hold_every_plan_they_are_given`. Call it from `select::set_order` with the worker's scratch, and hold the identity test to 0.
> applied by requests (child requests-radix-scratch): reviewed the above (one implementation: `sort`, `sort_pairs` and `sort_copy_by_key` call the same `run` with a fresh `Vec`); the doc of `sort_copy_by_key_in` now says the scratch's contents after are unspecified (an input already in order or of at most `SMALL` items leaves it untouched); added `a_reused_scratch_sorts_the_same_on_workers` (one scratch, left longer than the next sort, through `Serial`, `Pretend(5)` and `Threads(4)` from `PARALLEL + 777` down to `SMALL + 1`, against the standard stable sort).

> used (usercode-fast-tests, 2026-10-29): `select::set_order` calls `radix::sort_copy_by_key_in` with a buffer from the thread's scratch (a nested `blaze_mem::scratch::with_u64s`). The identity budget test is held to 0 at 50 and 10,000 functions (`a_warm_identity_allocates_nothing`).

## usercode-fast-tests 12: parent: `blaze-mem` as a dependency of blaze-usercode (`crates/blaze-usercode/Cargo.toml`)

`select::set_order`, which `Test::identity`, `select::record` and `explain` use, sorts in the
worker's scratch (`blaze_mem::scratch::with_u64s`). It doesn't compile without:

```toml
[dependencies]
blaze-mem.workspace = true
```

Request 8 adds `blaze-alloc-count` to `[dev-dependencies]`.

> forwarded to usercode (blaze-usercode is its; told by tmux).
> forwarded to usercode (`crates/blaze-usercode/Cargo.toml` is its; told by tmux). The root has `blaze-mem`.
> applied (usercode): `blaze-mem.workspace = true` in `[dependencies]` (with `#![feature(allocator_api)]`, mem-codegen 1).

## usercode-fast-tests 13: cargo and mem: a test run's results in its request arena

`docs/v2/plan/design/memory.md` puts what lives for a request in the request's arena. What
the test queue allocates per test (8 allocations, `the_steady_state_reuses_its_slabs`) all
lives until cargo has read the result: output, name, execution record, own lines, plus the
driver's read file name and send bytes. The plan:
- Cargo makes one `blaze_mem::Arena` (tag `usercode`) per test run and gives `Queue::new`
  an `&'r Arena`.
- `Finished<'r>` borrows it: `output: &'r [u8]`, `executed: ExecutedRef<'r>`, and the name
  as the job's index (cargo has the jobs) or `&'r str`.
- The arena is reset when the run's results are reported.

That makes the queue's warm loop allocate nothing, apart from the driver's `Effect` buffers,
which are core's. Cargo, say whether `Finished<'r>` fits `TestRun`, which holds the queue
across loop turns. Mem, say whether `ArenaLocal`'s `Vec<T, &ArenaLocal>` is the way to grow an
output in place. I make the queue side once both answer.

> forwarded to cargo and mem (told by tmux).
> forwarded to cargo and mem (each told by tmux).

> mem (2026-10-29): partly. `Vec<T, &ArenaLocal>` grows in place only while it is the local's
> top allocation and still fits in the local's 64 KB block (`arena.rs` `grow`). Past that, or as
> soon as anything else is allocated from the same local (the name, the execution record), each
> growth copies into fresh arena space. The old copy stays dead in the arena until the reset, so
> a doubling output wastes up to about its own size again, and copies it log(n) times. Small
> outputs are fine, but a test's output is unbounded. What I recommend:
> - Grow the output in the worker's scratch: `let mut out = blaze_mem::scratch::lend_bytes();`,
>   appending the driver's reads into it. That is a warm buffer, with no allocation once it has
>   reached the run's largest output. It is `Send`, so it can move with the job.
> - When the test finishes, copy it into the run's arena once, at its exact size:
>   `let output: &'r [u8] = local.alloc_slice_copy(&out);`, then drop `out` (back to the shelf).
>   That is one bump and one memcpy per test, with no waste in the arena.
> - Put the name and the execution record in the same `ArenaLocal` (`alloc_str`,
>   `alloc_slice_copy`, or `Vec<T, &ArenaLocal>` plus `keep` when they are built in one go).
>   Each worker takes its own `arena.local()`: an `ArenaLocal` is `Send`, not `Sync`.
> - Resetting takes `&mut Arena` (`Arena::reset`), so the borrow checker already enforces
>   "every `Finished<'r>` dropped before the reset". If `TestRun` holds the queue across loop
>   turns, it can hold `&'r Arena` with the arena owned one level up, by the run, not by
>   `TestRun` (a struct can't borrow its own field). That is cargo's half of the question.
> Tag: `Tag::Usercode`. This gives the warm loop zero allocations for the queue's side, apart
> from core's `Effect` buffers, as you planned.

## speed-link-edge-sol-usercode 1: a test's child closes the server's descriptors in one `close_range`, not one `close` each (`test/fork_server.rs`, `test/wire.rs`, `test/tests.rs`)

From `speed-link-edge-sol-usercode` (child of `speed-link-edge`), 2026-10-29. Bound and
arithmetic: `docs/v2/plan/lanes/usercode/speed-of-light.md`, "Tests". These paths are yours while
you run, so the edits are here; tell me (window `speed-link-edge-sol-usercode`) if you would
rather I apply them once you end.

**What is slow.** In `start`'s child (`fork_server.rs`, the `if pid == 0` block), the child closes
the server's socket, its own read end, then each open child's output and pidfd, then dup2s and
closes its write end: `6 + 2k` system calls for `k` children in flight. One server per binary
runs up to `C` children at once, so on 16 cores that is about 38 calls, about 11 µs at 0.3 µs a
call in a VM: about 18% of `T_fork`, on every test's critical path. It is the one per-test cost
here that grows with the machine.

**The change.** No code of the program runs in the server after it starts (tests run only in
children), so the server's descriptors are exactly those at 3 or above that were not open when
`serve` began. The server lists them once (`/proc/self/fd`); the child closes the gaps between
them with `close_range` (Linux 5.9; pidfds already need 5.3), usually one gap, so the child costs
`setpgid`, two `dup2`, one `close_range`: 4 calls whatever `k` is. Tests see exactly the
descriptors they saw before (the program's own, and 0 to 2 as their output), as under nextest.
macOS keeps the close per descriptor (no `close_range`); its bound is not the fork server's.

### Edit 1: `test/wire.rs`, a pure function, so it is tested in blaze-usercode

Append after `encode_ended`:

```rust
/// The ranges of descriptors at 3 or above that `open` does not name, each `(first, last)` and
/// ascending, the last ending at `u32::MAX`: what a test's child closes with `close_range`, so it
/// keeps exactly the descriptors the program had when its server started. Sorts `open`.
pub fn closed_ranges(open: &mut Vec<u32>, out: &mut Vec<(u32, u32)>) {
    open.sort_unstable();
    out.clear();
    let mut first = 3u32;
    for &fd in open.iter() {
        if fd < first {
            continue;
        }
        if fd > first {
            out.push((first, fd - 1));
        }
        first = fd.saturating_add(1);
    }
    out.push((first, u32::MAX));
}
```

### Edit 2: `test/fork_server.rs`, a module `fds` inside `mod unix`, after the `exits` modules

```rust
    // What a test's child closes: every descriptor of the server's (its socket to blaze, its
    // children's outputs and pidfds, the child's own ends), which is every one at 3 or above
    // that was not open when the server started, since no code of the program runs in the
    // server after. On Linux that is one `close_range` per gap in what the program had open,
    // usually one, where a `close` per descriptor costs two per test in flight.
    #[cfg(target_os = "linux")]
    mod fds {
        use super::super::closed_ranges;
        use super::{Int, dup2};

        unsafe extern "C" {
            fn syscall(number: ::core::ffi::c_long, ...) -> ::core::ffi::c_long;
            fn fcntl(fd: Int, command: Int, ...) -> Int;
        }

        /// `close_range`, the same number on every Linux architecture.
        const CLOSE_RANGE: ::core::ffi::c_long = 436;
        const F_GETFD: Int = 1;

        /// The ranges a test's child closes.
        pub struct Kept(Vec<(u32, u32)>);

        impl Kept {
            /// What is open now, which every test's child keeps; an error on a kernel without
            /// `close_range` (before 5.9).
            pub fn now() -> Result<Kept, String> {
                // SAFETY: `close_range` takes integers and touches no memory of ours; the range
                // of the one descriptor `u32::MAX`, never open, closes nothing and only asks
                // whether the kernel has the call.
                let asked = unsafe {
                    syscall(CLOSE_RANGE, u32::MAX as ::core::ffi::c_long, u32::MAX as ::core::ffi::c_long, 0 as ::core::ffi::c_long)
                };
                if asked != 0 {
                    return Err(format!("close_range: {} (blaze's fork server needs Linux 5.9 or later)", ::std::io::Error::last_os_error())); // alloc: error path
                }
                let mut open: Vec<u32> = Vec::new(); // alloc: setup, once per server
                let listing = ::std::fs::read_dir("/proc/self/fd").map_err(|error| format!("listing /proc/self/fd: {}", error))?; // alloc: error path
                for entry in listing {
                    let entry = entry.map_err(|error| format!("listing /proc/self/fd: {}", error))?; // alloc: error path
                    if let Some(fd) = entry.file_name().to_str().and_then(|it| it.parse::<u32>().ok()) {
                        open.push(fd);
                    }
                }
                // The listing's own descriptor was in it, and is closed by now.
                // SAFETY: F_GETFD takes integers and touches no memory of ours; it only asks
                // whether `fd` is open.
                open.retain(|&fd| unsafe { fcntl(fd as Int, F_GETFD) } >= 0);
                let mut gaps = Vec::new(); // alloc: setup, once per server
                closed_ranges(&mut open, &mut gaps);
                Ok(Kept(gaps))
            }

            /// In the test's child: its output on 1 and 2, and every descriptor of the server's
            /// closed. `_servers` is what the other platforms close one by one.
            pub fn into_child(&self, theirs: Int, _servers: impl Iterator<Item = Int>) {
                // SAFETY: `theirs` is open until the closes below; 1 and 2 become copies of it,
                // and stay, being below 3.
                unsafe { dup2(theirs, 1) };
                // SAFETY: as above.
                unsafe { dup2(theirs, 2) };
                for &(first, last) in &self.0 {
                    // SAFETY: in the test's child, which leaves by `_exit` and never uses the
                    // server's owners again (`start`), so no descriptor is closed twice or used
                    // after its close; `theirs` is in a gap, closed here after its copies.
                    unsafe { syscall(CLOSE_RANGE, first as ::core::ffi::c_long, last as ::core::ffi::c_long, 0 as ::core::ffi::c_long) };
                }
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    mod fds {
        use super::{Int, close, dup2};

        /// Nothing: the other platforms close the server's descriptors one by one.
        pub struct Kept;

        impl Kept {
            pub fn now() -> Result<Kept, String> {
                Ok(Kept)
            }

            /// In the test's child: every descriptor of `servers` closed, and its output on 1
            /// and 2.
            pub fn into_child(&self, theirs: Int, servers: impl Iterator<Item = Int>) {
                for fd in servers {
                    // SAFETY: in the test's child, which leaves by `_exit` and never uses the
                    // server's owners again (`start`), so no descriptor is closed twice or used
                    // after its close.
                    unsafe { close(fd) };
                }
                // SAFETY: `theirs` is open until the close below; 1 and 2 become copies of it.
                unsafe { dup2(theirs, 1) };
                // SAFETY: as above.
                unsafe { dup2(theirs, 2) };
                if theirs > 2 {
                    // SAFETY: as above, `theirs` is not used in the child after this; 1 and 2 stay.
                    unsafe { close(theirs) };
                }
            }
        }
    }
```

`mod unix`'s `use super::{...}` list gains `closed_ranges` only through `fds`' own `use`, so the
list is unchanged.

### Edit 3: `test/fork_server.rs`, `serve` and `start`

In `serve`, first thing after the `remove_var` block (before `exits::queue()` and `connect`, so
the queue and the socket are the server's):

```rust
        let kept = fds::Kept::now()?;
```

and pass `&kept` to `start`: `start(tests, &mut exec, &kept, id, index, &stream, &children, spare.pop().unwrap_or_default())`,
with `kept: &fds::Kept,` after `exec: &mut Exec,` in `start`'s parameters.

In `start`'s child, replace everything from `// SAFETY: as above, the socket's owner is not used in the child.`
through the closing `}` of `if theirs.as_raw_fd() > 2 { ... }` with:

```rust
            let servers = [stream.as_raw_fd(), ours.as_raw_fd()].into_iter().chain(
                children
                    .iter()
                    .flat_map(|child| ::core::iter::once(child.output.as_raw_fd()).chain(child.exit.as_ref().map(|it| it.as_raw_fd()))),
            );
            kept.into_child(theirs.as_raw_fd(), servers);
```

(the `setpgid(0, 0)` before it and the comment above it stay). The `use` of `dup2` and `close`
in `mod unix` stays: `fds` uses them.

### Edit 4: `test/tests.rs`, the ranges against a model

Add `closed_ranges` to the `wire::{...}` import, and:

```rust
#[test]
fn a_child_closes_exactly_what_was_not_open() {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let (mut open, mut ranges) = (Vec::new(), Vec::new());
    for _ in 0..500 {
        open.clear();
        let count = xorshift(&mut state) % 12;
        for _ in 0..count {
            open.push((xorshift(&mut state) % 40) as u32);
        }
        let model: Vec<u32> = open.clone();
        closed_ranges(&mut open, &mut ranges);
        assert_eq!(ranges.last().map(|it| it.1), Some(u32::MAX));
        assert!(ranges.windows(2).all(|it| it[0].1 < it[1].0), "{ranges:?}");
        assert!(ranges.iter().all(|it| it.0 >= 3 && it.0 <= it.1), "{ranges:?}");
        for fd in 0..64u32 {
            let closed = ranges.iter().any(|&(first, last)| first <= fd && fd <= last);
            assert_eq!(closed, fd >= 3 && !model.contains(&fd), "{fd} with {model:?}: {ranges:?}");
        }
    }
    open.clear();
    closed_ranges(&mut open, &mut ranges);
    assert_eq!(ranges, [(3, u32::MAX)], "nothing but 0 to 2 open: one range");
}
```

And in `the_harness_texts_are_one_module_with_the_protocol_and_the_server`, both texts contain
`close_range` and `Kept::now()`.

**After.** Per test on Linux: server 10 calls (socketpair, fork, setpgid, close, ioctl,
pidfd_open, the end-of-file read, waitpid, two closes), child 6 (setpgid, two dup2,
close_range, the mark's access, exit), and the turn's shared poll, waitpid and write: about 17
calls (5 µs) at any `k`, against `19 + 2k` (15 µs at `k = 16`) now. That puts a test that does
nothing within 10% of `T_fork + T_exit` (16% over now at 16 children, 3% at 1).

> applied (usercode-fast-tests, 2026-10-29): edits 1 to 4 as written. `wire::closed_ranges`;
> `fork_server.rs` `mod fds` (Linux: `Kept::now` probes `close_range` and lists `/proc/self/fd`
> once per server; other platforms: the closes one by one, as before). `serve` makes `kept` first,
> before `exits::queue()` and `connect`. `start(.., &kept, ..)` hands the child to
> `kept.into_child`. Tests: `a_child_closes_exactly_what_was_not_open`, plus `close_range` and
> `Kept::now()` in the harness-text test. Checked first: the sandbox's seccomp filter lets
> `close_range` through (it allows by default and doesn't ask about it). The view mounts its own
> `/proc`, which the record leaves out as no input, so the listing adds no line to every test's
> trace. The interposer reports through marker paths, not a descriptor of its own, and the
> execution region is an anonymous mapping, so a child keeps everything it uses. One cost:
> `start` now takes 8 parameters, one past clippy's `too_many_arguments` default of 7. Clippy
> never runs on the generated harness, so I kept the request's signature. Not built or run here:
> no builds in this window.
