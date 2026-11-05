---
status: record
summary: What the daemon's memory is, by salsa query and allocation site, and what salsa's lru can evict.
date: 2026-10-21
---
# What the daemon's memory is, and what salsa's `lru` can do about it

2026-10-21, for [#39](https://github.com/indexable-inc/blaze/issues/39) (Glancer item 5,
[docs/glancer.md](../glancer.md#item-5-memo-eviction-and-per-phase-memory)). Measured on
Andrew's Mac (Apple silicon, macOS, shared with other builds), blaze at `8b25a045da`,
Cranelift, the `bench.py` projects. Each claim is **read** (measured, or read in a source),
**reported** (someone says so) or **unverified** (our inference). The salsa source read is in
[docs/upstream/daemon-memory/](../upstream/daemon-memory/SOURCE.md).

## Short version

- What `bench.py` calls the daemon is the daemon's whole process tree. After the first build,
  a **spare worker** is started for the next project, and it holds a loaded standard library:
  **940 MB** of ripgrep's 2,834 MB after an edit (the project's worker holds 1,909 MB).
  The worker itself does not grow at the first edit (**read**).
- Of the worker, the largest holder is `described` (what a debugger is told of a type): on
  nushell **2.44 GB of values and 0.29 GB of salsa's records**, about 30% of the live heap.
  Each of its 77,114 memos is a type with every type it is made of, copied whole, names
  included (**read**).
- salsa 0.28.5's `lru` cannot be put on `infer` or the MIR queries: they have cycle handling,
  and salsa takes an evicted memo for one a panic left and throws (**read**, it killed
  nushell's worker in a cold build).
- An `lru` evicts only when a revision begins, so it lowers nothing during a build, and what it
  frees stays with the allocator: `lru = 4096` on `described` lowered ripgrep's live heap from
  1,622 MB to 1,416 MB and its footprint not at all (**read**).

## How it was measured

- `BLAZE_MEMORY=<dir>` reports after load, resolve, build and warm-up, and now after each
  request a worker answers (`request-N`). On macOS the resident figure is now the physical
  footprint (`proc_pid_rusage`), which was 0 before (**read**).
- A `--features heap-profile` build allocates with jemalloc and, under
  `_RJEM_MALLOC_CONF=prof:true`, writes a heap profile per report. jeprof names nothing right on
  macOS: the profile has no load address. The daemon now writes it beside the profile
  (`<step>.load`), and `harness/heapfold.py` names the stacks with `atos` and `rustfilt`.
  `harness/heap.py` gives each allocation to the innermost salsa ingredient on its stack, as a
  query's value or salsa's record of the memo (**read**).
- Sampled bytes are scaled as jeprof scales them, `size / (1 - e^(-avg/rate))`; the sampled
  total is within 15% of jemalloc's live count (ripgrep 1,444 against 1,622 MB) (**read**).

## What holds it

Live heap by phase, jemalloc's count (**read**):

| | load | resolve | build | ready |
|---|---|---|---|---|
| ripgrep | 826 MB | 861 MB | 1,785 MB | 1,622 MB |
| nushell | 1,880 MB | 2,206 MB | 10,249 MB | 10,030 MB |

nushell's cold build compiled 6,399 functions and 152,692 instances in 14.3 s. Its heap once
ready, by holder (9.1 GB sampled: values 6.2 GB, salsa's records 1.5 GB, blaze's own
1.0 GB, interned 0.4 GB) (**read**):

| holder | value | record | memos |
|---|---|---|---|
| `described` (debuginfo types) | 2,440 MB | 289 MB | 77,114 |
| `runtime_mir_body` | 550 MB | | 22,285 |
| `compiled_instance` | 439 MB | 209 MB | 158,496 |
| `parse_macro_expansion` | 415 MB | | |
| `parse` | 400 MB | | |
| `mir_body` | 371 MB | | 22,615 |
| vfs and loading | 277-293 MB | | |
| splice (blaze) | 249 MB | | |
| `infer` | 236 MB | 53 MB | 22,559 |
| symbol table (blaze) | 229 MB | | |
| `Body::relative` | 186 MB | | 135,996 |
| `inlinable` | 121 MB | 140 MB | 118,693 |
| `crate_local_def_map` | 99 MB | | |
| `body_syntax` | 91 MB | 66 MB | 135,996 |
| `with_source_map` | 85 MB | | |
| `compiled_root` | 67 MB | | 9,585 |

ripgrep once ready (1,444 MB sampled): `parse` 186 MB, `described` 180 MB and 20 MB of
records, `parse_macro_expansion` 91 MB, loading 88 MB, `Body::relative` 67 MB,
`runtime_mir_body` 55 MB, `mir_body` 46 MB, `compiled_instance` 44 MB and 14 MB of records,
`infer` 44 MB and 14 MB of records; salsa's records 254 MB in all (**read**).

`described`, split by what allocated its bytes on nushell: 1,331 MB copying a type's set into
its `Arc<[DebugType]>`, 893 MB formatting names, 227 MB vectors of fields, 285 MB salsa's
completion (**read**). A set is about 32 KB: a type and the types it is made of, two pointers
deep, each with its name as a `String`. The sets of `Vec<A>` and `Vec<B>` each hold their own
copy of everything they share. It became a query in `ae53631abb` (2026-10-20) so that an edit
does not describe the types of the functions it compiles again, part of nushell's main edit
going from 0.40 s to 0.30 s (**read**, that commit's message).

parse trees stay although `parse` has `lru = 128`: `body_syntax` (135,996 memos on nushell)
holds the green subtree of every body, which keeps the trees' nodes alive. `Body::of` keeps an
`Arc<Body>` for every body, so `Body::relative`'s `lru = 4096` frees little (**unverified**:
read from the code and the counts, not measured by evicting).

The spare: after the first request the daemon starts `blaze-engine --spare` with the 21 crates of the
standard library loaded ("spare: 21 crates in 5.4s"), for the next project to start from. On
ripgrep after an edit: worker 1,909 MB, spare 940 MB (`footprint` of each process) (**read**).
That is the step from "once ready" to "after the no-op" in `bench.py`: ripgrep 2,309 to
2,826 MB, fd 1,722 to 2,566 MB, nushell 11,330 to 11,970 MB (**unverified** for fd and
nushell, whose processes were not measured one by one).

## salsa's `lru`, read and tried

- `#[salsa::tracked(lru = N)]` evicts in `reset_for_new_revision`, which runs when a revision
  begins; nothing is evicted during the revision a build runs in (**read**,
  `function.rs:505`).
- Eviction sets the memo's value to `None` and keeps its revisions and dependencies; only a
  `Derived` memo is evicted (**read**, `function/memo.rs:87`). An evicted memo whose inputs
  changed cannot be backdated: there is no old value to compare, so what depends on it
  re-executes too (**read**).
- A query with cycle handling (`cycle_result` or `cycle_fn`) re-executes through
  `execute_maybe_iterate`. When the old memo was verified in the current revision and has no
  value, `previous_iteration` takes it for a cycle head that panicked earlier in the revision
  and throws `Cancelled::PropagatedPanic` (**read**, `function/execute.rs:364`). An evicted
  memo that an edit has deep-verified and something then asks the value of is exactly that.
  The memo a panic leaves has a `fixpoint_initial` origin and an evicted one a `Derived`
  origin, so salsa could tell them apart (**unverified**: not tried). The same code is in
  salsa's latest release (**read**, docs.rs).
- Tried: `lru` on `infer` (2,048), `mir_body` and `runtime_mir_body` (2,048 each) and their
  closure queries (1,024 each). ripgrep built and edited; nushell's worker exited with status
  101 and no message in its cold build, after "symbols: 230220". Under lldb the throw was
  `Cancelled::throw` from `MemoHeader::previous_iteration`, inside `infer`'s `execute`,
  reached from `compiled_root`'s `maybe_changed_after` as `root_rows` was fetched on a
  `lower_until` thread (**read**). `Cancelled` is resumed with `resume_unwind`, which skips the
  panic hook, which is why nothing was said (**read**). Every MIR query and `inlinable` has
  cycle handling too.
- Tried: `lru = 4096` on `described` alone (no cycle handling). ripgrep with jemalloc: ready
  1,416 MB live (1,622 on main), 1,403 and 1,394 MB after two body edits; footprint 1,982 MB
  ready. With the release allocator (mimalloc), the worker's footprint after an edit was
  1,907 MB against main's 1,925 MB (**read**, one run each). `bench.py --runs 3`: ripgrep
  ready 2,179 MB (2,309 on main), fd 1,818 (1,722), nushell 10,876 (11,330); after edits
  nushell 11.9-12.3 GB as on main (**read**; the machine was busy, cold builds took about
  twice main's time in that run). The edits it slows are the ones that write the whole object,
  which describe every variable's type again (nushell's generic, struct and new function edits
  were full links in that run) (**unverified** as a cost: those edits were not interleaved
  with main's).

## What would cut it

1. **Share what `described` keeps** (nushell about 2.4 GB, ripgrep about 180 MB): a memo per
   type of the type alone, naming what it is made of by type rather than copying it, names
   interned. The writer and the store's `stored_types` read sets today; both change. Keeps
   edits as fast, since it is still a query.
2. **The spare worker** (about 0.6-0.9 GB of each project's figure): start it when a second
   project asks, or let it go when a project is served. A second project's cold start pays
   loading the standard library again (5.4 s on this Mac).
3. **Eviction salsa can do**: patch `previous_iteration` to throw only for a
   `fixpoint_initial` memo (a vendored salsa, as `crates/rustc_abi` is patched in), then
   `lru` on `infer` and the MIR queries. Bounded by what those hold (nushell about
   1.2 GB of values) and by the allocator returning it. An edit that reaches an evicted body
   re-executes what depends on it rather than backdating.
4. **What the allocator keeps**: jemalloc's footprint was 450 MB over its live count on
   ripgrep once ready, and mimalloc's eviction above returned nothing. Whatever is evicted is
   worth measuring as footprint, not only as live bytes.

## 2026-10-22: the spare and eviction

For [#56](https://github.com/indexable-inc/blaze/issues/56) and
[#57](https://github.com/indexable-inc/blaze/issues/57). Measured on an AWS c8a.48xlarge (AMD
EPYC 9R45, Linux 6.18), each run pinned to 16 of its cores, blaze at `494f0ee948` and branches
of it, Cranelift. Before and after ran on the same host; `spare.py` interleaves its variants
round by round, `bench.py` ran main's and the branch's three runs of a project one after the
other.

### The spare

- Since #11 a new project's spare loads its workspace beside cargo: the client sends
  `Request::Ahead` before cargo starts, and `Daemon::ahead` hands it the spare (**read**,
  `cargo/daemon.rs`). Only the spare held in reserve afterwards, which `reap` started again once
  a workspace was loaded, waits for a build nobody has asked for.
- That one held 1.3 GB on Linux: an idle daemon with one tiny project 2,635 MB, 1,275 without
  it (**read**, `spare.py`, medians of 3).
- Starting a spare when a build of a new workspace begins (`Daemon::new_spare` in `ahead`), and
  none after, leaves a second project's cold start as it was: ripgrep 4.74 to 4.86 s, fd 24.21
  to 24.17 s, nushell 30.48 to 30.70 s. A warm one (cargo had built it, the daemon started
  again) pays the library's load: 1.29 to 2.62 s, 2.19 to 3.48 s, 8.62 to 9.51 s (**read**).
- Two ways to win that back were measured and dropped (**read**, 3 rounds on ripgrep): a spare
  told its build at once that loads the library with the workspace, as a plain worker does
  (warm 2.90 s), and one that resolves the library on every core instead of a quarter (2.66 s).
  The time is not the resolving.
- No spare at all (`BLAZE_SPARE=0`) holds the same and loses the overlap with cargo: cold 7.14
  s on ripgrep, 27.46 s on fd, 38.43 s on nushell; warm 2.14, 2.25 and 6.83 s (**read**).
- Loading the library into the spare without resolving it holds 1,631 MB idle (350 MB more than
  none) and gains nothing: warm 2.55 s on ripgrep, cold as with one (**read**).
- Letting the reserve spare go once a project is served and idle was not built: it holds what no
  spare holds once the time is up, and what the old default held until then (**unverified**).
- `bench.py`, main against the branch, 3 runs: ripgrep's daemon after its edits 3,623 to 3,656
  MB on main, 2,307 to 2,336 on the branch; fd 3,420 to 3,466, 2,039 to 2,080; nushell 12,695
  to 12,976, 11,362 to 11,623. Once ready the same (1,835 and 1,838 MB on ripgrep), since the
  reserve was started after it. Every edit time within 5 ms on ripgrep and fd, within 50 ms on
  nushell (signature 0.284 and 0.333 s, the rest within 22 ms) (**read**). Nothing the change
  touches runs during an edit, so the 50 ms is the runs' spread (**unverified**).

### salsa's `lru` with the patch

- salsa 0.28.5 vendored with two changes (`docs/upstream/daemon-memory/`): `previous_iteration`
  takes a memo without a value that is not its own cycle head for an earlier revision's, and
  `can_evict_value` refuses a provisional memo. The memo a panic leaves cannot be told by its
  origin, contrary to the guess above: `fixpoint_initial` makes a `Derived` origin with no
  edges. It is provisional and its own cycle head, which an evicted memo is not (**read**).
- With `lru` on `infer`, `mir_body`, `runtime_mir_body` (2,048 each), their closure queries
  (1,024) and `inlinable` (8,192), nushell's cold build, edits and warm start finish and every
  program behaves as cargo's (**read**, `bench.py`, 1 run). Values are evicted at the
  `synthetic_write` after the build: the reports count memos without values from `warm-up` on.
- nushell, 1 run each: the daemon after edits 12.4 to 12.5 GB with `lru`, 12.9 GB on main;
  edits body 0.73 s (0.41), signature 2.43 s (0.42), the others within 0.1 s; a second run,
  with the forced collect below, 0.78 and 2.32 s (**read**). An
  edit that re-executes an evicted memo cannot backdate it, so what depends on it runs again,
  down to the compiled code (**unverified** as the whole cause).
- `lru` on `infer` and `runtime_mir_body` alone, so that `mir_body` stops the chain: 12.56 GB
  after edits, body 0.52 s, signature 0.64 s (**read**, 1 run).
- A forced `mi_collect(true)` after the warm-up and after each request, which reclaims the
  segments of threads that ended: 1 to 8 ms on ripgrep, 20 to 50 ms on nushell's 11 GB worker;
  ripgrep's worker 2,150 MB against 2,185 on main after the edits; nushell's daemon 12.85 GB
  after edits against 12.9 (**read**, 1 run each). With `lru` as well, the collect after
  nushell's cold build took its worker from 14.06 to 12.03 GB, and the next ones 20 to 200 MB
  each, ending near 10.9 GB (**read**, the worker's log, 1 run).
- Nothing of it is merged. What is left to evict is bounded by those queries' values (about
  1.2 GB on nushell, above) and costs edits; `described` (#55) is twice that and keeps edits.

## 2026-10-23: the own base, and the allocator

For [#39](https://github.com/indexable-inc/blaze/issues/39). Measured on an AWS c7i.16xlarge
(dev-aws-7, Intel Xeon, Linux, transparent huge pages on `madvise`), each run on 16 cores of
its own, blaze at `d6291bd7cb` (main) and the `mem-39` branch, Cranelift. Per process with
`harness/memsplit.py`, per allocation with `heap.py` on jemalloc profiles, the whole tree with
`bench.py`.

### Why the daemon doubles after ready

- It is not the first edit. One second after the daemon is ready with nothing asked, it builds
  its own base: every function of the workspace compiled by blaze, so that later edits patch
  its code (`build_own_base`). `bench.py`'s first edit comes after cargo's reference build, by
  which time that is over, and the no-op before it already shows it: nushell 6.2 GB ready, 10.8
  GB after; helix 4.0 and 16.2 GB (12.1 once the first edit is over); bun 12.3 and 24.9 GB
  (**read**, `bench.py`, 1 run each on main).
- What it keeps is the memos of compiling everything, `described` first (**read**, jemalloc's
  sampled live heap, the worker of a store-warm daemon once ready against the measured one
  after its own base):

| | nushell ready | nushell own base | helix ready | helix own base |
|---|---|---|---|---|
| live heap | 4.1 GB | 8.0 GB | 3.2 GB | 10.0 GB |
| `described`, value and record | 0.33 GB | 2.61 GB | 0.17 GB | 5.64 GB |
| `compiled_instance`, value and record | | 0.54 GB | | 0.47 GB |
| `runtime_mir_body` | 0.27 GB | 0.45 GB | 0.08 GB | 0.26 GB |
| `mir_body` | 0.18 GB | 0.32 GB | 0.09 GB | 0.20 GB |
| `inlinable`, value and record | | 0.27 GB | | 0.27 GB |
| `infer`, value and record | 0.18 GB | 0.28 GB | 0.10 GB | 0.20 GB |

  On helix `described` is 82% of what the own base adds; on nushell 59%. That is #55.
- The spare is not in it any more: once the own base is built the tree is the project's
  worker (the process started as the spare, so its command line says `--spare`), the
  proc-macro server (85 MB) and the daemon itself (under 50 MB) (**read**, `memsplit.py`).
  #56 took the reserve spare out; nothing of the figure is another project's.
- bun's add-a-function edit (33.8 GB at most, 27.0 GB before it, on main) writes the whole
  object again although it compiles 2 functions: 7.3 s writing the object, 8.4 s of threads
  describing variables for the debugger, 5.3 s linking (**read**, the edit's steps in
  `bench.py`'s result). The 6.8 GB above what the daemon holds is that write; the signature
  edit before it is the same (36.0 GB at most) (**unverified**: not profiled, inferred from
  the steps and the peak).

### The allocator

- blaze allocated with mimalloc 3.3.2 (`libmimalloc-sys` 0.1.49 builds its v3 unless told
  `v2`). It asks for transparent huge pages (`allow_thp` is 1 by default) and purges freed
  pages after a second (**read**, `options.c`). On a host with THP on `madvise`, nushell's
  worker held 2.5-2.9 GB more than with jemalloc after the same build and edits
  (**read**):

| nushell, `memsplit.py`, after its edits | worker |
|---|---|
| mimalloc (main), two runs | 11.98, 12.37 GB |
| mimalloc, `MIMALLOC_PURGE_DELAY=0` | 11.58 GB |
| mimalloc, `MIMALLOC_ALLOW_THP=0` | 10.77 GB |
| mimalloc, both | 11.00 GB |
| jemalloc 5.3 (tikv-jemallocator 0.5.4), defaults, two runs | 9.46, 9.60 GB |
| jemalloc, `background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:0` | 9.50 GB |
| jemalloc, `narenas:16` | 10.76 GB |
| jemalloc, `dirty_decay_ms:0,muzzy_decay_ms:0` | 8.98 GB |

- The live heap is the same either way (about 8 GB after the own base); what differs is what
  the allocator keeps of what the own base's build freed: the worker's high-water mark is
  13.2-13.4 GB with jemalloc, 14.6-16.5 GB with mimalloc (**read**).
- jemalloc on Unix, mimalloc kept for Windows (jemalloc is not built there), `bench.py`, two
  rounds, main and the branch at once on two sets of 16 cores and swapped for the second round
  (**read**; bun one run each, after one another on the same cores):

| | ready | after the own base | after its edits | most at an edit |
|---|---|---|---|---|
| ripgrep | 1.72 → 1.31 GB | 2.05 → 1.68 GB | 2.03 → 1.57 GB | 2.08 → 1.70 GB |
| nushell | 6.16 → 5.21 GB | 10.77 → 10.06 GB | 10.80 → 9.52 GB | 10.86 → 10.08 GB |
| helix | 4.13 → 3.71 GB | 15.95 → 14.23 GB | 12.13 → 11.07 GB | 16.07 → 14.23 GB |
| bun | 12.29 → 10.72 GB | 24.86 → 23.75 GB | 27.00 → 24.28 GB | 36.05 → 32.90 GB |
| bevy | 8.84 → 8.68 GB | 31.61 → 31.62 GB | 43.46 → 41.15 GB | 84.31 → 76.12 GB |

  bevy one run each, after one another. Its body and leaf edits fall back to cargo on both
  builds after 512-577 s (a writable static without a rustc symbol; `UnresolvedMethod("collect")`
  in `prepare_mesh_view_bind_groups`), and those are its peaks; add-a-function is a full link,
  438 → 452 s. The branch's run stopped after add-a-function when the host's `/tmp` filled up.

  Edits within the runs' spread: nushell's six edits 0.25-0.39 s on main and 0.26-0.39 s on
  the branch (medians of the two rounds, each edit's within 35 ms of main's, either way);
  ripgrep's within 20 ms; helix's within 25 ms but signature's second round (0.39 s against
  0.26-0.27); bun's, one run each, body 1.53 → 0.63 s, leaf 0.52 → 0.76 s, generic 0.80 → 0.68
  s, the two full links 37.4 → 32.8 s and 26.4 → 26.4 s. The cold build to ready within the spread (nushell 39.5-48.0 s on main, 41.5-42.1
  s on the branch; ripgrep 6.4-6.7 s and 6.4-7.1 s).
- `dirty_decay_ms:0` would give back another 0.6 GB on nushell; each page a build frees is then
  returned at once and faulted in again at the next build. Not taken: the edits it was measured
  with were within the spread, but one run is not enough to say (**unverified**).

## 2026-10-23: sharing `described` (#55)

For [#55](https://github.com/indexable-inc/blaze/issues/55). Same host as above (dev-aws-7,
124 GiB), blaze main at `04bdd5b01e` (with jemalloc) and the `share-types-4` branch merged
onto it, Cranelift, each pair at once on two sets of 16 cores, one run each, with
`harness/memsplit.py` (the edits body and add-a-function; bun's signature and
add-a-function).

### What changed

- A set of debug types (`TypeSet`, what `described` returns for a variable's type) was a list
  of `DebugType`s, each with its own `String`s: the name of every type, field and generic
  parameter, copied into every set the type is in. What a type is told as apart from the
  places of its parts in the set (`Told`) is now interned once per distinct description, and a
  set holds, for each of its types, a reference to that and the places of its parts: 8 bytes
  for the reference and 4 for where its parts start, plus 4 for each part. That is linear in the
  distinct descriptions and in the sum of the sets' lengths, not in what the sets spell out
  (**read**, `crates/hir-ty-codegen/debuginfo/set.rs`). The store keeps a set as the list it
  was, so stored entries are read back the same.
- Before the own base gives up for memory, the worker asks jemalloc to purge the pages of what
  it freed (`arena.<all>.purge`) and gives up only if it is still over. How much of bun's
  figures below this is was not measured apart (**unverified**).

### The DWARF is the same

- The objects of a parallel build differ between two runs of the same blaze: which unit a
  type's entry is shared in follows the order functions are compiled in (`debuginfo.py`'s
  rung2, stress and corpus objects, main against main: 17 of 99 with debug info differ in
  their DIEs) (**read**).
- On one core (`taskset -c 40`, which `available_parallelism` follows) the order is fixed.
  `debuginfo.py --only stress`, `rung2` and `corpus` on main and on the branch: all 941
  objects and executables with debug info have the same `llvm-dwarfdump --debug-info
  --debug-line`, once the checkout's path and the temporary directories' names are set aside;
  the `.debug_info` and `.debug_line` bytes are the same, `.debug_str` one byte shorter for the
  shorter checkout path (**read**). `debuginfo.py` in full passes on both, perf unwinding by
  `.eh_frame` included (341 and 338 frames of blaze's functions in about 400 samples, none
  without a line) (**read**).

### Memory, main → branch

| | ready | after the own base (most) | after its edits | most at an edit |
|---|---|---|---|---|
| nushell | 5.48 → 5.41 GB | 11.17 (13.78) → 8.48 (11.50) GB | 9.74 → 7.46 GB | 13.96 → 11.73 GB |
| helix | 4.19 → 4.00 GB | 11.74 (16.17) → 7.09 (11.54) GB | 11.30 → 6.39 GB | 14.54 → 9.89 GB |
| bun | 10.46 → 8.87 GB | 30.72 (31.86) given up → 18.89 (24.03) GB | 24.87 → 15.94 GB | 33.62 → 23.23 GB |

(**read**.) The edits' times are the same but for bun's signature edit: 32.8 s on main, where
the base was given up at this host's bound (31.7 GB, a quarter of its memory, the same as the
128 GiB Mac's 32 GB) and the edit wrote the whole object and linked it, against 6.3 s on the
branch, where the base was built and the edit patched it. nushell's edits 0.28-0.35 s on both
after the first (1.95 and 2.20 s right after the own base); helix's 0.11-0.21 s (1.48 and
2.24 s). The own base took 22 s on nushell on both, 70 → 61 s on helix, 28 → 38 s on bun,
where main's stopped at the bound.

### zed

Before main's jemalloc (main `ee227bdb4d` against the branch then, mimalloc): ready at 16.2
and 14.7 GB; the own base reached the bound (31.7 GB) in 27 s on main and 94 s on the branch,
about 0.58 and 0.18 GB/s; with `BLAZE_OWN_BASE_MEMORY=off` under a 64 GB cgroup the
branch's worker was killed at 66.7 GB (**read**). So what `described` copied was most of
zed's growth, as the 2026-10-22 profile said, and something else still grows past 64 GB; #29
stays open.

## 2026-10-23: each type once (#29)

For [#29](https://github.com/indexable-inc/blaze/issues/29). Same host (dev-aws-7, 124 GiB),
blaze's `described-nodes` branch, Cranelift, 16 cores, `harness/memsplit.py` with
`BLAZE_OWN_BASE_MEMORY=off` under a 64 GB cgroup, profiles with `heap.py`.

### What changed

- A `TypeSet` was still made per variable's type: the types reachable from it, two pointers
  deep, each with the places of its parts in that list, so a type in many sets was in each
  (#55 interned the descriptions, not the lists). Now a type is described once, by
  `told_type(env, ty, pointers)`, a salsa query of the type alone that names its parts by
  type; the types that are made of each other are put together into one group by their
  strongly connected components (Tarjan's), and every set that reaches a group shares it by
  `Arc`. What sets hold is linear in the distinct types, not in the sum of the sets' lengths
  (**read**, `crates/hir-ty-codegen/debuginfo/set.rs`). A type as deep as the writer goes
  (`DebugType::DEEPEST`) is not told of, as the writer did not tell of it before.
- `debuginfo.py` passes and matches main's run row for row; what differs is counts from the
  newer main its run was built on (8,558 against 8,407 functions, debugger sections 1.9%
  larger) (**read**).
- On zed, what `described` held went from 31 GB of a 47.8 GB sample to 2.4 GB for
  `told_type` (**read**, two profiles of the own base at about the same point).

### Memory, main → branch

main at `c93e530e47` against the branch merged onto it, one after the other on the same 16
cores, one run each, `memsplit.py` with its default edits (bun's signature and add-a-function),
bun in `nix develop .#bench-bun`:

| | ready | after the own base (worker's most) | after its edits | most at an edit |
|---|---|---|---|---|
| nushell | 5.22 → 5.31 GB | 8.97 (11.46) → 7.87 (11.11) GB | 7.63 → 7.36 GB | 11.70 → 7.87 GB |
| helix | 3.72 → 3.96 GB | 7.02 (11.39) → 6.14 (10.62) GB | 6.55 → 5.87 GB | 9.80 → 6.14 GB |
| bun | 9.09 → 8.93 GB | 18.77 (24.18) → 15.85 (22.06) GB | 15.97 → 14.47 GB | 22.85 → 15.78 GB |

(**read**.) The first edit after the own base is faster: nushell's body 2.06 → 0.37 s, helix's
1.71 → 0.19 s, bun's signature 7.23 → 1.51 s (that main's first edit makes sets the base
already made, which the branch finds in the groups it keeps, is **unverified**); the edits
after it are within 0.1
s of each other (nushell 0.28-0.36 s, helix 0.12-0.26 s, bun 0.65-1.02 s). The own base took
19.7 → 21.5 s on nushell, 59.7 → 58.9 s on helix, 38.4 → 39.1 s on bun. Ready is 0.1-0.2 GB
more on nushell and helix, and the cold builds the same (37.2 s on both, 35.0 and 35.5 s);
why ready holds more was not looked at (**unverified**).

### zed

- The own base finishes: 1,893,135 functions in 5 rounds, 662 s, a peak of 62.6 GB for the
  daemon's tree (the worker's high-water mark 59.6 GB), 51.5 GB once done; ready before it
  at 14.4 GB (**read**). 497 s of the 662 are rounds thrown away: a static used by a function
  left to rustc (twice), 369 instances that wait on the layout of an `async` coroutine, 9
  writable statics without a rustc symbol (**read**, the worker's log).
- Its link failed first: without `--gc-sections`, which the daemon takes out so a patch can
  call what the executable's own code no longer reaches, lld reads webrtc's
  `SharedXDisplay::IgnoreXServerGrabs`, which calls `XTestQueryExtension` and
  `XTestGrabControl`; rustc's line never links `libXtst`, and its `--gc-sections` drops that
  code before lld looks for the names. The link now writes rustc's own line next to its own and
  runs it when the linker names what nothing defines and none of it is in the daemon's
  objects: 27.5 s failed, then 9.9 s (**read**).
- `memsplit.py` took the base as done at its `stats:` line and made its first edit while
  the base was being linked, which interrupted it; it waits for `edit-to-binary:` now.
- After the base, 44.4 GB sampled live (**read**): salsa's records 10.5 GB (`told_type` 5.39M
  memos, `layout_of_ty` 4.26M, `row_place` 2.85M, `layout_of_adt` 2.21M,
  `compiled_instance` 1.91M, `inlinable` 1.37M), MIR about 8.9 GB (`runtime_mir_body` 2.2,
  `monomorphized_runtime_mir_body_for_coroutine` 2.1, `mir_body` 1.9), `compiled_instance`'s
  code 3.9 GB, inference 2.8 GB, blaze's symbol table 1.9 GB, `inlinable` 1.5 GB,
  `told_type` 1.4 GB, layouts 1.6 GB.
- Under the default bound, a quarter of memory (31.7 GB here), the base is still given up,
  and zed's edits patch rustc's executable (#159). All of it is held in the one revision the
  base is built in, where salsa evicts nothing (above): the base would have to be built in
  pieces, revisions apart, for `lru` to bound it
  ([#188](https://github.com/indexable-inc/blaze/issues/188)) (**unverified** as a fix).
- `memsplit.py`'s body edit of zed is in `crates/zed/src/main.rs`, whose `main` blaze leaves
  to rustc (it awaits through `async` blocks whose coroutine layout it can't make), so it
  is no patch: the build in full is refused, cargo builds it, and in the same cgroup as the
  59 GB worker cargo's build was killed (**read**).

## 2026-10-26: reth, and evicting the base's MIR and code (#261)

For [#261](https://github.com/indexable-inc/blaze/issues/261). Measured on dev-aws-14 (AWS,
Linux, 370 GB), blaze main at `598936fc68` and the `mem-261` branch, Cranelift, reth at
`482366d170` (`--bin reth --no-default-features --features jemalloc,otlp,otlp-logs,js-tracer,
keccak-cache-global,asm-keccak,gmp,min-trace-logs,portable`; the default features want LLVM
23), each run on 6 to 24 cores of its own, a driver that does a cold `blaze build`, waits
for the own base's end in the worker's log, then edits: a body, a leaf body, a new function and
a signature with its caller, each made and taken back, with the worker's `VmRSS`, `VmHWM` and
jemalloc's live count after each.

### No growth per edit

- reth on main, 50 body edits after the own base: the worker 16.20-16.26 GB and 13.03 GB live
  from the first to the fiftieth (**read**). The growth the issue saw on Tempo, 8.9 to 23.3 GB
  over two edits after a pause, is the own base (above, 2026-10-23): it begins a second after the
  daemon is idle, every edit interrupts it, and the pause let it finish (**unverified** on
  Tempo: its cold build did not finish here, below).
- reth's worker once ready 6.4 GB (5.0-5.9 GB live); after the own base 17.4 GB, its most
  19.9-20.2 GB, the issue's "19.6 GB at load" (**read**).

### What the own base keeps

jemalloc's sampled live heap after the 50th edit, 12.9 GB (**read**, `heap.py`): values 6.5
GB, salsa's records 3.0 GB, blaze's own 2.2 GB, interned 1.0 GB. By holder: the MIR queries
(`runtime_mir_body`, `mir_body`, their closure, coroutine and monomorphized copies) about
2.3 GB, `compiled_instance` 0.95 GB of code and 0.61 GB of records, `infer` 0.86 GB and 0.12
GB, `inlinable` 0.32 GB and 0.39 GB, blaze's symbols of what rustc linked 1.4 GB (the symbol
table 0.53 GB, `symbols::Linked` 0.58 GB, `symbols::Named` 0.28 GB), splice 0.38 GB, vfs 0.35
GB, `Body::relative` 0.30 GB, layouts 0.26 GB. What the own base adds to ready is about 7.6
GB of it. The worker's resident figure is 3.2 GB over the live count: jemalloc returns freed
pages only as it allocates, and an idle worker does not (**read**: `dirty_decay_ms:0` held 15.3
GB against 16.2, `background_thread:true` 15.9).

### What changed

- salsa's evicted-memo patch (`docs/upstream/daemon-memory/`) is applied to `vendor/salsa`.
- The MIR queries, `compiled_instance` and `inlinable` have an `lru`, which the worker sets
  (`BLAZE_LRU`, [cargo.md](../cargo.md)): 2,048 lowered and runtime bodies each, 16,384
  instances' code, 8,192 bodies to inline. Values past that are let go when the build is over
  (`let_go`), and jemalloc is asked to purge (`memory::give_back`).
- Nothing is let go while a program's own base is still to come (wanted, not built and not
  given up): an edit interrupts the base, and the base built again after it takes what it had
  computed; let go, it would compute it all again and, with edits more often than it takes,
  never be done. Only a worker that serves sets the `lru`s, whose bookkeeping (a lock per
  fetch) is a cost of every fetch of those queries; it is the likely cause of the own base's 70
  → 73-74 s below (**unverified**: not profiled).
- `compiled_root`, the code of each non-generic function under its own symbol, has no `lru`:
  a patch reads it for every function of the program it links (**unverified** as a cost).
- `vendor/salsa/tests/lru_cycle.rs` keeps the patch's case: a memo evicted, verified again by
  what depends on it, then asked for. It fails without the patch (`Cancelled::PropagatedPanic`)
  and passes with it (**read**). `vendor/salsa` is no member of the workspace, so CI does not
  run it: copy the directory out, add `[workspace]` to its `Cargo.toml`, and run `cargo test
  --test lru_cycle --features inventory` there.
- `infer` was tried with an `lru` too (a free tracked function in place of the method): the
  first edit after the own base took 11-12 s against 3.4 s, and the worker held the same, so
  it is not in.

### Memory and edits, main → branch

Two runs of each at once, 6 cores each (**read**):

| | ready | after the own base (most) | after 24 edits | live after 24 edits |
|---|---|---|---|---|
| main | 6.41, 6.43 GB | 17.36 (20.08), 17.33 (19.89) GB | 16.14, 16.14 GB | 12.84, 12.87 GB |
| branch | 6.64, 6.50 GB | 17.89 (19.82), 17.80 (19.78) GB | 14.57, 14.61 GB | 10.59, 10.60 GB |

- Edits after the first within the runs' spread: 2.1-2.7 s on main, 2.1-2.6 s on the branch.
  The first after the own base is slower, as a value let go is computed again: the body edit
  3.81 and 3.47 s on main, 5.55 and 6.99 s on the branch; the new function 3.8 → 4.0-4.7 s.
  The own base took 70 s on main and 73-74 s on the branch.
- Lower bounds (512 bodies, 2,048 instances and bodies to inline): 14.1-14.4 GB after the
  edits, 9.9-10.1 GB live, the first body edit 6.9 s (**read**, one run each).
- The most the worker holds is the same: the own base is built in one revision, in which salsa
  lets nothing go. Bounding that is building it in pieces
  ([#188](https://github.com/indexable-inc/blaze/issues/188)).

### Tempo

At `60c3923`, blaze builds with its own toolchain (`nightly-2026-10-25`) whatever the
project's `rust-toolchain` says. rustc on `tempo_consensus` in cargo's first build held 29 GB
after 5 minutes and 39 GB after 12; one such build was killed by the kernel at 78 GB with
three other builds on the host (**read**). The issue's cargo build, on 1.99.0, took 74 s.
