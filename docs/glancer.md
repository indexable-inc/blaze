---
status: plan
summary: A front end more like Rust Glancer: six ideas, where time and memory go, and the ordered steps.
date: 2026-10-21
---
# A front end more like Glancer

2026-10-21. What Rust Glancer and matklad say, with sources, is in
[research/glancer.md](research/glancer.md). This file is the plan: what blaze already has
toward each of six ideas taken from them, where the time and memory actually go, and an
ordered list of steps, each of which merges on its own. Valid MIR is never given up: every
step either keeps the analysis rustc's semantics need or skips it only where a frozen copy
of the same result exists.

## Short version

- The largest cost on the workspaces that hurt was not analysis. helix's 631 s to ready
  and 100.7 GB were names: a store key spells out every generic argument, a closure that
  wraps closures holds each of their types twice, and helix-parsec's combinators made one
  name of 665 MB. Hashing a type once its spelling passes 1 KB took helix to **109 s and
  18.4 GB**, with the same 158,768 functions compiled (step 1, below; issue #29).
- What remains on a cold start is mostly blaze's own: on helix, a second round of 29 s
  because one static started the build over; on both, about 76 s of thread time in
  translation. Glancer's ideas act first on loading and name resolution, which is 12 s of
  nushell's 50 s and 7 s of helix's 109 s on a cold start, and on inference and MIR only
  once something is kept from an earlier start.
- Where the ideas do pay is the **warm** start. A store that has every function still costs
  nushell its whole analysis, because the instances to look up are found by analysing: in
  the cold-start lane's run, 165,752 functions found, and still 36 s of inference, 33 s of
  MIR, 21 s of monomorphization and 88 s of translation (thread time). Items 1 and 2 are
  what make a warm start a lookup.
- Order: measure memory per query first (step 0), bounded names (done), proc-macro
  expansion cache (item 3), memo eviction (item 5's quick part), frozen dependency artifacts
  (item 1) in stages each guarded by a differential check, then the workspace's own state
  (item 2, with the cold-start-2 lane). The flat syntax tree (item 4) has its own lane
  (#42); stripping the LSP parts (item 6) waits for numbers that say it matters. Who has
  what is under "Working with the other lanes".

## What was measured

All on dev-aws-7 (c7i.16xlarge, 123 GB), the daemon pinned to 16 cores with `taskset`,
Cranelift, an empty store, `harness/coldprof.py`, blaze at `5f26408566` (main on
2026-10-21) and the same with step 1. "ready" is wall time from the start of `cargo build`;
"cargo" is cargo's own build, which the daemon overlaps. Steps are summed thread time.

| | cargo | ready | peak RSS | compile step | rounds | store | stitching (resolve) |
|---|---:|---:|---:|---:|---:|---:|---:|
| helix, main | 34.0 s | 631.3 s | 100.7 GB | 590.3 s | 2 (first 290.7 s) | 14 GB | 131.5 s |
| helix, step 1 | 34.0 s | **109.1 s** | **18.4 GB** | 69.7 s | 2 (first 29.2 s) | 1.6 GB | 11.4 s |
| nushell, main | 36.2 s | 51.4 s | 16.3 GB | 10.9 s | 1 | 1.1 GB | 3.0 s |
| nushell, step 1 | 36.0 s | 49.9 s | 15.8 GB | 9.7 s | 1 | 866 MB | 1.9 s |

Thread time per step, helix with step 1: infer 12.8 s, MIR 16.1 s, monomorphize 13.6 s,
translate 76.1 s, describe 6.2 s, store write 1.3 s (on main: store read 28.1 s, store write
35.8 s). nushell with step 1: infer 32.0 s, MIR 27.9 s, monomorphize 15.4 s, translate
76.1 s.

From the committed benchmarks ([`docs/benchmarks/linux-x86_64.json`](https://github.com/indexable-inc/blaze/blob/b3e953eb13e8e4476a4673ab1c7dfbdff61880c6/docs/benchmarks/linux-x86_64.json), `2b2f2dcd5b`, same
machine type): ripgrep ready 12.5 s at 2.6 GB, load 0.9 s, resolve 0.5 s; bevy ready
115.8 s at 8.5 GB, load 14.4 s, resolve 12.6 s; nushell load 7.2 s, resolve 5.0 s; helix load
7.1 s, resolve 5.6 s. zed did not fit in 111 GB (#29).

**After cold-start-2 (2026-10-21, dev-aws-11, 187 GB, 16 cores, same harness).** cargo
is answered from what rustc linked, and the program edits patch (the own base) is built in
the background once nothing has asked for a second:

| | `blaze build` returned | ready | RSS at ready | own base done | own base peak |
|---|---:|---:|---:|---:|---:|
| helix | 27.3 s | 29.4 s | 4.7 GB | 108.4 s | 19.1 GB (13.6 GB after) |
| zed v1.24.4 | 160.3 s | 214 s | 14.6 GB | given up at 48.1 GB | 49.8 GB, flat after |

- helix's own base is step 3's restart round now: its first round (39.1 s) is thrown away on
  `__RUST_STD_INTERNAL_VAL`, in the background. It does not loop, and memory does not grow
  without bound; the 55 GB seen before was a binary from before step 1.
- zed's own base restarted once a second (2131 interruptions in 40 minutes, flat at
  16.2 GB): `livekit_api`, an optional dependency a dev-dependency's feature turns on, includes
  a file under an `OUT_DIR` no build script wrote, and an include that could not be read was
  counted as one still to load before every turn. Fixed on `include-loop`
  (`harness/rustcbase.py`'s `unbuilt-member`).
- With the loop gone, zed's own base grows about 1.2 GB/s and passed 150 GB in two minutes. A
  heap profile at 73.6 GB live: `described` (debuginfo types) 51.2 GB, 69.6%; `infer`, MIR
  and `compiled_instance` each under 1 GB. That is #55's, and item 5's `described` sharing.
- The safety valve until then: a worker gives an own base up once it holds a quarter of the
  machine's memory (`BLAZE_OWN_BASE_MEMORY`), and edits patch rustc's executable. It is not
  #39's result dropping: nothing is freed (salsa keeps the memos, #57), the build just stops.

How helix was found: during its compile step the worker ran on one core at 40 to 100 GB.
A `perf` sample of it was 14% `memmove`, 13% SHA-256, 11% `core::fmt::write`, 7% hex
formatting of bytes, 6% hashing of `Arc<str>`, and the rest in `StableNames::{ty, args,
item}` and `cached::Entries::{get, usable}`. The longest printable strings in the store
were stable names: 665,474,913 bytes for one `helix-parsec` closure, and the largest packs
(3.0 GB, 1.3 GB, 665 MB) whole multiples of it.

## What blaze already has, item by item

1. **Frozen dependency artifacts.** Nothing of analysis is kept across runs. The store
   (`crates/blaze-store`, [cache.md](cache.md)) keeps compiled code only, keyed
   by stable names; `content.rs` names a crate of the checkout by its source; `remote.rs`
   shares packs through a directory or S3; `deps.rs` pushes and pulls cargo's dependency
   units. Those are the shareable, content-addressed half of item 1, for code. rustc
   compiles every dependency; blaze analyses dependencies only as far as the instances it
   compiles (generic or inlinable functions homed there, `home.rs`), but rust-analyzer loads
   and resolves every crate first.
2. **The workspace's own state.** Nothing persisted. While cargo builds, a spare worker
   loads and resolves the sysroot and the workspace and compiles the functions of each crate
   cargo is done with on a quarter of the cores (#11). The cold-start-2 branch made that
   lighter and built no persistence: what is left of the command's cost, 5 to 7% over
   cargo's on just, ripgrep and nushell, is the spare resolving the sysroot and the
   workspace beside cargo, which is this item's to remove (and item 1's for the sysroot),
   through the one id serialization both need, not a mechanism of its own.
3. **Proc macros and build scripts.** Build scripts: `seed_own_target` copies cargo's target
   directory and commits `20708cd2da` and `c8f58ac7d9` stop re-running scripts cargo already
   ran, so this half is mostly done. Proc macros: one proc-macro server process
   (`proc_macro_processes: 1`), and no cache of expansions.
4. **Syntax trees.** syntax-tree everywhere. The parse query keeps at most 128 trees, macro
   expansions 512 and 1,024, bodies 512 and 4,096 (salsa `lru` attributes in the subtree),
   but blaze's `body_syntax` held each anchor's green node, which kept whole files' trees
   past that lru until #42's step (a)
   ([flat-syntax.md](flat-syntax.md#step-a-no-tree-outlives-its-query)). Since #42's steps
   (b) and (c) the compiler's parses make flat trees (arrays) behind syntax-tree's cursor.
5. **Memory discipline.** No eviction of inference or MIR: `infer`, `mir_body` and
   blaze's monomorphized MIR have no `lru`, and blaze drops nothing. Measuring is there:
   with `BLAZE_MEMORY=<dir>`, `crates/blaze-cargo/src/memory.rs` reports resident memory after
   load, resolve, build, warm-up and each request answered (on macOS the physical footprint)
   and writes a table of every salsa ingredient (count,
   bytes of fields and of salsa's records; heap bytes only where an ingredient declares
   `heap_size`, which no rust-analyzer query does), and jemalloc's live bytes. A build with
   the `heap-profile` feature, with `_RJEM_MALLOC_CONF=prof:true`, also
   writes a heap profile for `jeprof` (on macOS for `harness/heapfold.py`, with `heap.py` to
   give it to queries). rust-analyzer's own per-query report
   (`per_query_memory_usage` in `ide-db/src/apply_change.rs`) is commented out since its
   salsa migration.
6. **LSP-shaped parts.** blaze has its own watcher (`inputs.rs` for the manifests and
   cargo config, `tree.rs` for sources, "watching N files"); line tables serve only panic
   locations and debug info. The client already sends a sync request per build.

## The items

### Step 1: bounded names (done)

Not one of the six, but it is what #29 was. `StableNames` memoizes the spelling of each
interned type and of each crate, and a type whose spelling passes 1,024 bytes is written as
`$` and the SHA-256 of the spelling, with a `~` kept when the spelling had one, so that
`of_checkout` still sees types of the checkout. A name's size now grows with the number of
distinct types, not with the tree they unfold to. Names under 1 KB are unchanged, so the
entries of a store stay valid. `harness/deepnames.py` builds closures nested 20 deep: on
main the store is 156 MB, with step 1 it is 0.2 MB, and a second daemon finds the 114 entries
again. In `blaze_cli.py helix`, the cold `blaze run` takes 640.8 s on main and
46.9 s with step 1 (dev-aws-7; both runs exit 1 where cargo's exits 0, a separate issue).

### Item 3: proc-macro expansion cache

**Design.** A wrapper around each `ProcMacroExpander` the workspace loads, installed by
blaze after `load_workspace` and before the crate graph is set. The key is a SHA-256 of
the macro dylib's content hash, the macro's name and kind, the input token trees (and the
attribute's) with spans replaced by their index in a table of the distinct spans of the
input plus def, call and mixed site, the environment the expander is given, its current
directory, and blaze's binary hash. The value is the output token tree with the same span
indices. An expansion is not stored when it errored, when its output has a span not in the
table, or when the expander asked the server anything during it (`SourceText`,
`LocalFilePath`, `FilePath`, `ByteRange`, `LineColumn`, `SpanSource`): those read outside
the tokens. A per-macro opt-out (`BLAZE_PROC_MACRO_CACHE=off` or a list of names) covers
macros that read files or the environment by themselves, which the server cannot see.

**Where.** The wrapper and its disk format in a new crate under `crates/` (it is machinery:
hashing, files, no Rust semantics), stored in the same store directory with its own key
domain. Seeing the sub-requests needs a hook in load-cargo's `Expander`
(`crates/load-cargo/src/lib.rs`, around line 703): a new module file
in load-cargo declared with one line, which counts the callback's requests per expansion.

**Payoff, measured (2026-10-21, dev-aws-7, 16 cores, `blaze build` cold then warm on
one store).** Expansion is a small part of a load. nushell's cold start expands 1677 macros
in 3.20 s (summed over threads; 1520 derives take 2.95 s) of a 14.3 s load; helix's 1476 in
3.40 s. On the warm start 1583 (nushell) and 1455 (helix) come from the store in about 0.6 s,
plus 0.4 to 0.5 s to key them, and the expansions left are function-like macros, which are
not kept by default. Resolve goes from 5.0 to 3.9 s on nushell and from 5.5 to 4.1 s on helix;
time to ready from 33.6 to 32.3 s and from 139.4 to 138.2 s. ripgrep expands no proc macro.
Peak memory, the daemon's memory once ready and the store's size do not change (store +0.01 GB).
Since the daemon is ready once it has read what rustc linked (cold-start, #11), the same
expansions are a larger share of a warm start (main 494f0ee948, against a build that also had
#37's first form, which only acts on the background base; dev-aws-7 with nothing else running, 16 cores, medians of four interleaved runs each): helix's resolve goes from 6.0 to
4.3 s and its time to ready from 9.6 to 8.0 s, nushell's from 5.8 to 4.5 s and from 10.1 to
8.8 s. Cold starts do not change (helix 35.5 and 35.2 s, nushell 36.8 and 36.7 s): the
daemon is ready 1.7 to 3.7 s after cargo returns either way, and runs on a busy host that
looked slower differed in cargo's own build time.

Build scripts need no cache of their own: a warm start does not run them. The daemon reads
their results from the messages of cargo's own build, which cargo's fingerprints keep fresh
(helix: the command returns in 0.5 s and the workspace loads in 0.6 s).

**Risks.** A macro that is impure without asking the server (reads a file through
`std::fs`, `env!` at expansion time) gives a stale expansion: the opt-out, and a check mode
that expands anyway and compares (like `BLAZE_CACHE_CHECK` for the store), are the
guards. Span renumbering must give back the same spans, or diagnostics and hygiene drift:
the check mode compares spans too.

### Item 5: memo eviction and per-phase memory

**Design.** First the quick part: once a body's machine code is in the object, its
`infer` and `mir_body` values and blaze's monomorphized MIR are not needed until an edit
reaches it. salsa's LRU drops a value and keeps the memo's revisions and dependencies
(`evict_value_from_memo_for`, salsa 0.28.5 `function/memo.rs`), so an evicted memo is still
validated by an edit and recomputed only when asked for again. Then the Glancer
discipline where it fits: blaze already runs a phase across all crates (load, resolve,
compile ahead, compile), and the `BLAZE_MEMORY` report after each phase says which phase
leaves what behind; compaction and a phase arena come after those
numbers, and only for what they show.

**Where.** `lru = N` on the three queries is an attribute in upstream files; blaze's
monomorphized MIR is blaze's own query.

**Payoff, measured (2026-10-21, dev-aws-7, 16 cores, `harness/heap.py` on jemalloc
profiles).** Inference and MIR hold little of the peak; debug information holds most of it.
At helix's high-water mark (an interval dump, 15.4 GB live of a 17.9 GB peak) `mir_body` holds
0.5 GB, `compiled_instance` and `compiled_root` 1.0 GB, `inlinable` 0.15 GB, while the
`described` query holds 5.8 GB, most of it copies of the type tables each memo makes
(`Arc<[DebugType]>` and `Vec<Field>` cloned), and the DWARF writer's names (`Dwarf::named`)
3.0 GB: together 62% of the peak. Once the build is over helix holds 6.4 GB live (`described`
2.1 GB, item trees 1.3 GB, blaze's own structures 1.2 GB) and nushell 5.6 GB (`described`
1.0 GB, blaze's own 1.1 GB, `mir_body` 0.5 GB, item trees 0.4 GB). With `BLAZE_DEBUGINFO=0`,
helix's warm start peaks at 8.6 GB instead of 17.0 GB and holds 5.9 GB once ready instead of
11.9 GB; nushell's 10.5 and 7.2 GB instead of 14.0 and 10.3 GB; the stores are 0.25 and
0.31 GB instead of 1.70 and 0.91 GB. Line tables alone (`lines`) leave what is held once
ready where it was. So the first target is debug information: one copy of a type's
description shared by the memos that reach it, and the writer's names interned, before an
LRU on infer and MIR. About 5 GB of what the daemon holds once ready is the allocator's and
not live (helix 11.7 GB resident against 6.4 GB live, nushell 10.2 against 5.6 GB).

**Risks.** An edit that reaches many bodies recomputes their MIR: the edit harnesses
(`editbench.py`, `movedlines.py`, `storefirst.py`) and edit-to-binary times are the check.

**Measured (2026-10-21, #39).** The `lru` route does not work as designed, and nothing of it
is merged ([research/daemon-memory.md](research/daemon-memory.md)):
- salsa 0.28.5 takes an evicted memo of a query with cycle handling, which `infer` and every
  MIR query have, for one a panic left, and throws. With `lru` on them nushell's worker died
  in its cold build.
- An `lru` evicts only when a revision begins, so a build's peak keeps everything.
- `lru = 4096` on `described`, which has no cycle handling, lowered ripgrep's live heap once
  ready from 1,622 to 1,416 MB, and its footprint not at all: what it frees stays with the
  allocator.

What holds the memory: on nushell's 10.0 GB live heap once ready, `described` (debuginfo
types, each memo a whole copy of a type and everything it is made of) is 2.7 GB,
`compiled_instance` 0.65 GB, `runtime_mir_body` 0.55 GB, the parse trees 0.8 GB, `mir_body`
0.37 GB, `infer` 0.29 GB, salsa's records 1.5 GB in all. And about 0.6-0.9 GB of each
project's figure in `bench.py` after the first request is not the project's: it is the spare
worker the daemon starts for the next project (940 MB on ripgrep). Sharing what `described`
keeps, the spare, and a patched salsa that can evict a query with cycle handling are the
next steps, each its own issue.

**Measured (2026-10-22, #56, #57; Linux x86-64, 16 cores).** The spare is gone once a
workspace is loaded: one is started for a build as it begins, and `BLAZE_SPARE=ready` keeps
the old one. An idle daemon holds 1.28 GB instead of 2.64; the daemon after `bench.py`'s edits
2.31 GB instead of 3.63 on ripgrep and 11.36 instead of 12.70 on nushell, with the same edit
times. A second project's cold start is unchanged
and its warm start pays the library's load, 1.3 s on ripgrep and 0.9 s on nushell
([cargo.md](cargo.md#what-a-build-does)). A vendored salsa patched to tell an evicted memo from one
a panic left lets `lru` on `infer`, the MIR queries and `inlinable` through nushell's cold
build and edits, but it is not merged: it took nushell's daemon after edits from 12.9 to
12.45 GB and its signature edit from 0.42 to 2.43 s, since what depends on an evicted memo
cannot be backdated; on `infer` and `runtime_mir_body` alone, 0.35 GB and 0.42 to 0.64 s. A
forced `mi_collect` after each request returned 30 to 40 MB of ripgrep's worker. The rest is
the values the daemon keeps, `described` first (#55)
([research/daemon-memory.md](research/daemon-memory.md#2026-10-22-the-spare-and-eviction)).

**Measured (2026-10-23, #39; Linux x86-64, 16 cores).** What doubles the daemon after ready
is its own base, not the first edit: once idle it compiles every function of the workspace,
and keeps the memos. Of what that adds to the live heap, `described` is 82% on helix (5.6 of
6.8 GB) and 59% on nushell (2.3 of 3.9 GB); `compiled_instance`, the MIR queries and
`inlinable` the most of the rest. The tree holds nothing of another project's any more: the
worker, the proc-macro server and the daemon. bun's add-a-function edit peaks at 33.8 GB
because it writes the whole object, debug information included, for 2 functions compiled.
What the allocator kept was the other lever: mimalloc 3 asks for transparent huge pages,
and nushell's worker held 2.5-2.9 GB more with it than with jemalloc after the same build and
edits. blaze now allocates with jemalloc on Unix (mimalloc on Windows), edits within the
runs' spread (`bench.py`, two interleaved rounds, main → branch):

| | ready | after its edits | most at an edit |
|---|---|---|---|
| ripgrep | 1.72 → 1.31 GB | 2.03 → 1.57 GB | 2.08 → 1.70 GB |
| nushell | 6.16 → 5.21 GB | 10.80 → 9.52 GB | 10.86 → 10.08 GB |
| helix | 4.13 → 3.71 GB | 12.13 → 11.07 GB | 16.07 → 14.23 GB |
| bun (1 run) | 12.29 → 10.72 GB | 27.00 → 24.28 GB | 36.05 → 32.90 GB |
| bevy (1 run) | 8.84 → 8.68 GB | 43.46 → 41.15 GB | 84.31 → 76.12 GB |

bevy's most is at the two edits that fall back to cargo after about 9 minutes each (a
writable static without a rustc symbol, and `UnresolvedMethod("collect")`), on both builds.

([research/daemon-memory.md](research/daemon-memory.md#2026-10-23-the-own-base-and-the-allocator)).

**Measured (2026-10-23, #55; Linux x86-64, 16 cores, 124 GiB).** A set of debug types kept
every type it is made of as its own strings, so every set with a `String` in it had its own
`String`, `Vec` and `RawVec`. What a type is told as, apart from where its parts are in the
set, is now interned once, and a set holds a reference to it for each of its types and a
number for each part: 12 bytes a type and 4 a part, against the whole description. The
DWARF is the same bytes: 941 objects and executables of `debuginfo.py`'s stress, rung2 and corpus programs, built on one core so
that the order is the same, have the same `--debug-info` and `--debug-line` once paths are
set aside, and `debuginfo.py` passes, perf's unwinding included. And before the own base
gives up for memory, it has jemalloc give back what it holds freed. `memsplit.py`, after
main's jemalloc (main → branch, one run each at once on two sets of 16 cores):

| | after the own base | after its edits | most at an edit |
|---|---|---|---|
| nushell | 11.17 → 8.48 GB | 9.74 → 7.46 GB | 13.96 → 11.73 GB |
| helix | 11.74 → 7.09 GB | 11.30 → 6.39 GB | 14.54 → 9.89 GB |
| bun | 30.72 GB given up → 18.89 GB | 24.87 → 15.94 GB | 33.62 → 23.23 GB |

bun's base was given up on main at this host's bound (31.7 GB, a quarter of 124 GiB, as on
the 128 GiB Mac), and its signature edit wrote the whole object and linked it, 32.8 s; it
builds now, peaks at 24.0 GB, and the signature edit is a patch, 6.3 s. Edits are otherwise
the same (nushell's 0.28-0.35 s, helix's 0.11-0.21 s, both builds). zed's own base still
did not fit: it reaches the bound in 94 s rather than 27 s, and with no bound passed 64 GB
([research/daemon-memory.md](research/daemon-memory.md#2026-10-23-sharing-described-55)).
With each type described once (#29) it finishes with no bound of its own in 662 s, at a peak
of 62.6 GB, and is still given up at the default bound, a quarter of memory: it is all held in
the one revision it is built in (#188,
[research/daemon-memory.md](research/daemon-memory.md#2026-10-23-each-type-once-29)).

### Item 1: frozen dependency artifacts

**Design.** One artifact per dependency crate, content-addressed by its source hash, cfg,
features, toolchain and blaze's binary hash, holding what other crates ask of it:
signatures, module exports, impls, layouts, and MIR only for generic, `#[inline]` and
`const fn` items (the set rustc encodes, `should_encode_mir` in rustc 1.99.0). Its own
format, mmapped, not `.rmeta`. A dependency whose source is edited (a path dependency, a
`[patch]`) thaws to salsa. Artifacts live in the store directory and travel through the
remote like packs.

The obstacle is that ids are salsa-interned per process and every lookup in rust-analyzer
bottoms out in `DefMap`, `ItemTree` and the parse. So item 1 is staged:

- **1a.** Tiers (frozen, thawed, live) for each crate, the content keys (a Merkle hash over
  a crate's source and its dependencies' keys), an empty container, and a differential
  checker that, for a frozen crate, computes the live salsa value next to the frozen one and
  fails on any difference. Nothing is served frozen yet.
- **1b.** Frozen query families one at a time, in the order of measured cost (step 0
  says which): a hook at the top of each query, in a new module file of `hir-def` or
  `hir-ty` declared with one line, returns the decoded value for a frozen crate. Decoding
  interns into the running database, so ids stay process-local. Each family merges after
  the gate passes with the checker on.
- **1c.** Values read without being memoized (straight from the mmap), then zero-copy
  views for MIR and impl indices.

**Payoff.** On a cold start with no artifacts, none. On every later start, on any checkout
that shares dependencies: the load and resolve of dependencies (bevy 14.4 s + 12.6 s,
nushell 7.2 s + 5.0 s, helix 7.1 s + 5.6 s, less the workspace's own share), and the
inference and MIR of dependency instances. Memory: salsa memos of dependencies become pages
of a mapped file, which the kernel can drop.

Measured with step 0 (2026-10-21, dev-aws-7, 16 cores): a warm start loads the workspace in
0.6 s (helix) and 1.2 s (nushell) and resolves in 5.5 s and 5.0 s, of 146 s and 34 s to
ready. The rest goes to compiling with every function found in the store (nushell 14 s) and to
compiling the checkout again so the first edit can be patched (nushell 9 s, helix 47 s), which
is item 2's. Of what is live once a build is over, the dependency front end (item trees,
def maps, impl headers, salsa inputs) is about 1.8 GB on helix and 1.1 GB on nushell, against
2.1 GB and 1.0 GB of debug-info descriptions (item 5). So 1b's first family is the item
tree, the largest single front-end query on both (helix 1.3 GB, nushell 0.4 GB), and the time
1a and 1b can save on a warm start is about 5 s. Since cold-start (#11) a warm start is ready
in 8.0 s on helix and 8.8 s on nushell with item 3, of which resolving is 4.3 s and 4.5 s:
what is left of the warm start is mostly this item's.

**Built (2026-10-23).** Three families are frozen for each crate whose source is the
toolchain's, a registry's or a git checkout's and whose dependencies' are too: the `DefMap`
(with the `LocalDefMap`), the `TraitImpls` and `InherentImpls`, and the lang items, each its own
store entry under the crate's Merkle key (`crates/blaze-engine/src/frozen.rs`; the encodings in
`hir-def`'s `nameres/frozen.rs` and `hir-ty`'s `method_resolution/frozen.rs`). The bytes hold
no salsa ids: a file is its crate's index and its path from the crate's root, a module of a
block is its owner's body and the block's `AstId`, an interned id is its loc. A file outside the
crate's directory, which a build script may have written, is named with a hash of its text, and
bytes whose file changed are not read. So is the file each `include!` reads: a second checkout
of bevy read `crunchy`'s def map before the daemon had read the `OUT_DIR/lib.rs` it includes,
and looking up its `macro_rules!` in the empty expansion panicked. `BLAZE_FROZEN=check` resolves and finds as well and fails
the build on any difference, `verify` writes what was read and fails the same way; on ripgrep's
153 parts and hello's 78 both find none. The entries go through `blaze_store::Store` and so
travel through the remote with the packs. On dev-aws-8, pinned to one socket's 96 cores, hello's
build with a warm remote resolves in 0.33 s of 2.2 s and takes 1.22 s of 3.02 s, 2.3 GB of 3.4 GB;
ripgrep's resolves in 0.38 s of 2.4 s and takes 4.25 s, against cargo's cold 5.5 s.

What resolving still did for a build is now the build's: the impl headers' types (about 1 s of
ripgrep's compiling, most of it expanding the macros that write `core`'s impls) and the
expansions behind signatures and bodies. Lowering every impl header after names are read costs
the same second on the resolve side, so the next families are the impl headers and the macro
expansions of frozen crates. The spare daemon (`Held::load`) does not read frozen values yet: the
store's identity is told the toolchain once a process, before the spare knows the build's.

**Risks.** It is the largest change here and touches the most upstream behaviour. The
differential checker, kept as a mode, is the guard: a frozen value that differs from the
live one is a bug, never a fallback. salsa's own `persistence` feature is the alternative
for the time win (no memory win), at the cost of serde on hundreds of rust-analyzer types.

### Item 2: the workspace's own state

**Design.** The cheap form first, which is blaze's and not salsa's: a manifest per build,
keyed by the content of the checkout's crates, listing the stable names of every instance
the build compiled and the roots it started from. A warm start whose crates have the same
content looks the names up in the store and links, with no analysis; a mismatch falls back
to the full build. Then salsa's state proper (memo values and dependency graph) for the
crates of the checkout, so that a start after an edit revalidates instead of recomputing.

**Where.** The manifest beside the store, in `crates/blaze-store`. Salsa state with
cold-start-2.

**Payoff.** The warm start: nushell found all 165,752 functions in the store and still
spent 36 s of inference, 33 s of MIR, 21 s of monomorphization and 88 s of translation
(thread time) to find which (the cold-start lane, dev-aws-13, 16 vCPU, a warm store against
an empty one). The manifest skips all of it when nothing changed.

**Risks.** A manifest that matches content but not configuration (features, target,
cfg) would link the wrong program: it is keyed by everything the store key has, plus the
crate graph.

**Built (2026-10-27, `crates/blaze-engine/src/manifest.rs`).** Not a list of names to look up but
the files themselves: a full build of a program keeps what it wrote (the object, its patch
object; the standard library's archive and the executable are made again by the link) in the
store, each file by its content, under a key of
- every crate loaded: name, edition, kind, attributes, cfg, environment, origin and root, the
  files of its package (or of its build script's `OUT_DIR`) with their content, and the keys
  of its dependencies;
- the build asked for (`Args`: crates, profile, features, target, debug info, the outputs and
  the checkout's paths), the target, the standard library unit it links (by its bytes) and
  the variables that change what is compiled (`BLAZE_*` but the log's, the store's and the
  remote's; `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, `CARGO_PROFILE_*`, `CARGO_BUILD_*`,
  `CARGO_TARGET_*`). The store adds the toolchain and blaze's own build.

What only compiling finds out (variables `env!` and `option_env!` read of the process, files
`include_bytes!` and `include_str!` read) is kept with the files and checked before they are
used. A manifest is kept only when the key computed after the build is the one before it (a
build that loaded more files than it started with keeps none). A start whose key matches
writes the files and links, with nothing analysed or compiled. The manifest and its files are
ordinary store entries, so `blaze-engine cache push` and `pull` carry them.

`BLAZE_MANIFEST=check` builds in full anyway and fails the build when the program is not
the manifest's; `off` neither reads nor writes one. Two builds of one checkout are not alike
byte for byte: they lay functions out in the order threads finish and number what they name
themselves (`blaze::<name>::<n>`) in that order, and the debug info follows. So `check`
compares an object by what it defines: each loaded section's kind, size, bytes and
relocations (a numbered or local target by the bytes of its section), the unwind table by
what it covers, and the global symbols. On ripgrep it passes for a cold build against two
fresh machines' builds from the store, and a fresh machine with `RUSTFLAGS=--cfg=…` or
`BLAZE_DEBUGINFO=0` misses and builds in full.

Measured (`harness/freshwarm.py ripgrep`, arm64 ix VM, 32 vCPUs, 16 GB; a fresh machine is
an empty home and store at the seed's paths, `blaze-engine cache pull` from the remote the seed
pushed to, then `blaze build`):

| fresh machine | pull | build, main | build, manifest | compiled with the manifest |
| --- | --- | --- | --- | --- |
| nothing changed | 1.0-2.4 s | 8.7 s | 6.9-8.9 s | none (files written in 0.35 s) |
| one body edit | 1.3-2.7 s | 8.1 s | 9.7 s | as main; a manifest is kept |
| one signature edit | 1.2-1.5 s | 11.1 s | 9.5 s | as main; a manifest is kept |

What is left of an unchanged build on a fresh machine is the load: 5.2 s of its 6.9 s, of
which 4.9 s is the build scripts and proc macros (`cargo check --compile-time-deps` and
the units it loads; the `load-us:` lines under `BLAZE_STATS`), the nocargo lane's driver.
An edit low in the graph still misses the store for every crate that depends on it, since an
edit renames those crates (`content.rs`): that is item 2's second half and #344's code keys
on `blaze-next`, not this manifest. A daemon that starts from a manifest answers with its files,
then, while nobody asks, makes the build a start without one would (the checkout's code from
the store), which holds the patch base: its first edit is a patch, as `storetracked.py`'s
restart on the same checkout and store checks.

### Item 4: a flat syntax tree for files nobody edits

Its own lane (#42); the measurements and the design are in
[flat-syntax.md](flat-syntax.md). On main after the first request, syntax was 12.7% of
nushell's live heap (1,164 MB), 22% of ripgrep's (315 MB) and 8% of helix's (852 MB), most of
it green trees of whole files that `body_syntax` kept past the parse lru. Step (a), merged,
keeps an anchor's text and range instead and parses it again when asked: nushell's syntax
falls to 676 MB and helix's to 436 MB, resident memory right after the first request is
1.1 GB and 2.1 GB less, and ready time does not move. In cold time, building green trees and
walking red ones are 16% of ripgrep's samples to the first request and 7% of nushell's. Steps
(b) and (c), merged, put a flat tree behind syntax-tree's cursor so that no lowering code changes:
building trees falls from 12.1% of ripgrep's cold-start CPU to 1.9% and from 5.4% to 1.1% on
nushell, trees as a whole take a quarter to a third less CPU, and ready time and resident
memory do not move. What is left of syntax is lexing and parsing to events (3.6% on nushell),
token trees (4.0%) and walking (5.2%), and in memory macro expansions' trees and span maps.
Item 1 would remove most dependency parsing anyway; each says here which share it took.

### Item 6: no LSP-shaped guessing

Later, and for correctness more than speed. The client already sends a request per build;
it can send the paths and content hashes cargo's build used (from cargo's dep-info), and the
daemon then compares hashes instead of trusting its watcher's timing. The watcher stays as
the trigger for builds nobody asked for. Line tables stay: panics and debug info need them.

## Steps, in order

Each is a branch off main, merged after the gate (`cargo test --release -p blaze-engine`, corpus
`--mixed 8`, mixfutures `--mixed 16 --layouts`, `growth.py`, `movedlines.py`,
`storefirst.py`, `blaze_cli.py`, `borrowck_ui.py`, hir-ide failing only its known 18)
and cold-start time and RSS on helix and nushell before and after.

1. Bounded names. Done with this file.
2. Step 0, measurement: helix and nushell under a `heap-profile` build with `BLAZE_MEMORY`
   and `prof:true`, read with `jeprof` by the function that allocated, to say what share of
   the peak inference, MIR, monomorphized MIR, item trees and syntax hold; and proc-macro
   expansions counted and timed (the one new piece of code here).
3. The restart round: helix's first round of 29 s is thrown away because one instance needs
   a writable static (`__RUST_STD_INTERNAL_VAL`, std's thread-local) without a rustc symbol.
   Compiling the instance saves it, which is no-rustc's step E (#85); keeping the
   rejection so a later daemon forwards its callers to rustc's copies (#37's first form) is
   not merged. Done: the instance is `Context::with`'s closure, and its static is declared by
   `thread_local!` in the body, which rust-analyzer expands twice (to items and to
   statements, of the same text). blaze counted the static's place in the expansion sema
   gave, which was not the one the static is in, so it never found rustc's symbol. The count
   now goes into the expansions the static is in, and helix's own base builds in one round.
4. Proc-macro expansion cache (item 3), with its check mode.
5. Memo eviction (item 5), sized by step 0.
6. Item 1a, then 1b family by family, then 1c.
7. The warm-start manifest (item 2), then salsa state with cold-start-2.
8. zed measured again (its ready path fits since cold-start-2; its own base finishes under
   64 GB with #29 and is given up at the default bound, #188);
   item 6 if the numbers say so. Item 4 has its own lane (#42).

## Working with the other lanes

Who has what, as of 2026-10-22. Each lane keeps its line here current when it merges or
stops.

| Work | Issue | Lane |
|---|---|---|
| The restart round (step 3) | #37 | glancer, done as part of no-rustc step E (#85): the instance compiles, nothing is kept or forwarded to rustc |
| Proc-macro expansion cache (item 3) | #38 | glancer, merged ([#110](https://github.com/indexable-inc/blaze/pull/110)) |
| `described` copies whole type closures (item 5) | #55 | glancer (share-types-4): types interned once, helix 11.3 → 6.4 GB after edits, bun's base fits |
| The spare worker's memory (item 5) | #56 | spare, merged (8e01def1cf): 1.3 GB less after edits; a warm second project's cost is #99 |
| salsa eviction of queries with cycle handling (item 5) | #57 | spare, closed unmerged: lru saves 0.35-0.5 GB on nushell but slows the signature edit 0.42 to 0.64-2.43 s |
| What the daemon keeps after ready, and the allocator (item 5) | #39 | glancer: jemalloc on Unix, 1.3 GB less on nushell after edits |
| Frozen dependency artifacts (item 1) | #40 | glancer, after #38 |
| The workspace's own state (item 2) | #41 | glancer, after #40's 1a |
| A flat syntax tree (item 4) | #42 | flat-syntax: done, steps (a), (b) and (c) merged |
| blaze never runs rustc | #81 to #93 | no-rustc; steps A to M of `docs/no-rustc.md`, A to C ready to assign |

- **no-rustc.** Andrew's rule is that blaze must never run rustc. Since cold-start (#11)
  the time to ready is what rustc linked, so the warm start is cheap today because rustc
  built the program. Without rustc the daemon's own build is the ready path again, and the
  cold start is the store and the remote serving what blaze built, the standard library
  included (no-rustc's steps F, G and K, which the cold-start lane owns). That makes items
  1 and 2 (#40, #41) the cold and warm start's path: a frozen dependency and an unchanged
  checkout are linked from what blaze built and kept, without analysis. Item 3's keys
  name the macro's library by its content, so they hold when blaze builds that library
  (step J). #37 no longer keeps rejections to forward their callers to rustc's copies: the
  restart round is gone because the instance compiles (step E).
  `docs/no-rustc.md` says in which order rustc goes (its step K, #91, is the cold start). Items 1 and 2 are designed not to need rustc's
  output, and a step here that adds a new dependency on it says so in this table.
- **flat-syntax (#42)** and item 1 both change what dependency files cost. A frozen crate
  (1b) no longer parses for its item tree, and a flat tree makes the parses that remain
  cheaper. Both measure with step 0's tooling (`harness/heap.py`, `BLAZE_MEMORY`), and
  each says in its item here which share of load and resolve it took, so the other doesn't
  count it again.
- **share-types (#55)** and **spare (#56, #57)** both report the ready heap and the edit
  peak with `harness/memory.py`, and write item 5's numbers here when they merge.
- **cold-start-2** owns persisting the analysed workspace. Item 2's manifest is about
  compiled names and sits next to the store; salsa state is theirs. Both need a serialization
  layer for ids that survive a process: whoever lands first puts it where the other can use
  it, and says so in this file.
- **bun** (the bun-e lane, 2026-10-22) is measured here and not worked on there: its cold
  start and the daemon's memory are this lane's. `bench.py bun` on Linux x86-64 (16 cores of an
  AWS c8a.24xlarge, Cranelift, memory by `harness/memory.py`,
  [docs/benchmarks/bun-linux-x86-64.json](benchmarks/bun-linux-x86-64.json)): cold to ready in
  70.9 s + 2.4 s against cargo's 73.1 s, the cold build at most 15.5 GB at once against
  cargo's 4.5 GB. The daemon holds 12.5 GB once ready and 25.7 GB after the first edit, and the
  edit that adds a function peaks at 33.5 GB against cargo's 3.3 GB. On macOS arm64
  (2026-10-21, `docs/research/bun.md`) it was 291 s to ready, 28 GB, near 40 GB at an edit.
  2026-10-23, blaze `948d54e375` (the bun-perf lane), same machine: an added function is now
  a patch, so its 33.5 GB peak is gone; every edit peaks at 27.3 GB at most. The daemon holds
  12.0 GB once ready and 27.2 GB from the first build on, then stays flat to 0.1 GB over 33
  builds: the growth is the background build of the program for edits to patch (every
  function of bun's C-ABI main is an entry, about 68k functions), not the edits. Cold, the
  build peaks at 15.4 GB against cargo's 4.5 GB, as before.
  On macOS arm64 (Apple M5 Max, 128 GiB, same commit, read, the daemon's log of `bench.py bun
  --keep`) the background build of the program for edits gives up: "it holds 32826 MB, more
  than the 32768 MB it may (`BLAZE_OWN_BASE_MEMORY`)", a quarter of 128 GiB; Linux's quarter
  of 185 GiB is 46 GB, which bun's base fits. Edits then go on patching rustc's executable,
  which a signature edit cannot patch ("an edit that is more than the bodies of functions"):
  its first sample writes the whole object and links bun in full, twice (280 s, then 279 s
  again with `-headerpad` for the splices to fit), 574 s; the two after it splice into that
  link in under 2 s. bun's base needs about 1 GB less, or the bound more room, for macOS's
  signature edit to be a patch as Linux's is.
  2026-10-23 (#55): on a Linux host whose quarter is the Mac's (31.7 GB of 124 GiB), main's
  bun base is given up the same way and its signature edit is a full link (32.8 s, 33.6 GB);
  with the debug types shared it builds, at most 24.0 GB and 18.9 GB after, and the signature
  edit is a 6.3 s patch (item 5's measurements above; the Mac not run again).
- **architecture split**: the store is the crate `crates/blaze-store`, and the code
  generator moves to a crate of its own. New store-side code (item 3's cache, item 1's
  container, item 2's manifest) goes in or beside `blaze-store`, behind the store's
  interface, not into `hir-ty-codegen`'s module tree.

## Not checked

- bevy and ripgrep with step 1; zed's own base built in pieces under the default bound
  (#188).
- Whether `instance_path` and `home::Mentions`, which also walk types without a memo,
  grow the same way on other workspaces; on helix they were under 3% of the sample.
