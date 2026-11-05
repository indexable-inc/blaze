---
status: record
summary: Where nushell's cold start, edits and heap go in the front end (#236), and the lean path ranked.
date: 2026-10-24
---
# A lean frontend for compiling: where the time and memory go

2026-10-24, for [#236](https://github.com/indexable-inc/blaze/issues/236) (a strict, lean
frontend: lossless trees and error tolerance off the codegen path), beside [#40](https://github.com/indexable-inc/blaze/issues/40)
(frozen dependencies), [#41](https://github.com/indexable-inc/blaze/issues/41) (the warm
manifest), [#188](https://github.com/indexable-inc/blaze/issues/188),
[#216](https://github.com/indexable-inc/blaze/issues/216) and
[#11](https://github.com/indexable-inc/blaze/issues/11). The question: rust-analyzer was built
for an editor (a lossless tree per file, inference that tolerates any error, salsa recording
every read so any edit can be answered incrementally); which of that does a compiler pay for,
and how much? Measured on dev-aws-8 (AMD EPYC 9R14, 192 vCPU, 369 GB, Linux), blaze at
`c48ca3f800` and, for the listing change, `07217080d0`; release builds, Cranelift, nushell 0.108.0
(`bench.py`'s pin), the worker pinned to 16 cores (`taskset -c 120-135`) and builds on others.
Each claim is **read** (measured, or read in a source), **reported** (someone says so) or
**unverified** (our inference).

## Short version

- The two things the critique names first cost little on the codegen path. Error tolerance:
  frames that only collect diagnostics are under 0.1% of the cold start's CPU, and the
  diagnostics kept are 18 MB (0.3%) of the heap; the `*_with_diagnostics` queries (7.7% on the
  stack) spend it computing the values the compiler needs. Lossless trees: after the flat trees
  of [flat-syntax.md](../flat-syntax.md), building trees is 3.5% and walking them 1.6% of the
  cold start; trivia handling is about 0.4% (**read**).
- What the cold start (52 s to ready, 41 s of it loading the workspace) spends is lexing and
  parsing (11.5% innermost, 13.4% on the stack), macro expansion (30.8% on the stack, token
  trees 4.3% innermost) and code generation (28.6% on the stack). A parser without trivia
  would still lex and parse; what a compiler saves is in not doing it again, which is #40
  and #41's (**read** for the shares, **unverified** for the rest).
- On edits, the cost is salsa's bookkeeping: revalidating memos is on the stack in 38% of a
  body edit's samples and 47% of a signature edit's. A signature edit in nu-protocol walks
  30.6M edges, 27.6M of them to memos already verified in that revision, for 1.7M deep
  verifications and 4.7k re-executions; `compiled_instance` is 12.2M of the edges (**read**).
- Durability does not help: dependencies are already verified shallowly (17 deep
  verifications at MEDIUM per body edit). Giving workspace files MEDIUM made the first
  signature edit 3.7 s, as salsa cannot backdate a memo whose durability falls (**read**).
- Salsa's records are the largest holder of the heap after the base build: 1.3 GB, 22.6% of
  5.8 GB sampled, about 64M edges, a third of them on memos of dependencies (**read**).
- Landed: the crates a build asks for are listed on every core, not one after another: a
  signature edit's `roots` stage 53 to 30 ms and its edit-to-binary 410 to 373 ms (−9%)
  ([below](#slice-1-crates-listed-on-every-core)).
- Landed after it, slices 2 to 4 and two fixes the edit's critical path showed: on nushell a
  body edit 315 to 258 ms (−18%) and a signature edit 403 to 277 ms (−31%), the cold start and
  the peaks as they were; on ripgrep 2 to 3% ([below](#slices-2-to-4-and-where-a-body-edits-time-goes)).

## How it was measured

- `harness/leanprof.py PROJECT --work W` runs a project's first hour as phases and writes when
  each began and ended (monotonic clock): `cold` (`blaze build` from nothing until
  ready), `base` (the own base the daemon builds once idle), each edit of `--edits` made and
  undone `--rounds` times, then `warm` (the daemon stopped and started on the same store) and
  `warm base`. `--no-warm` stops after the edits (**read**).
- `perf record -k CLOCK_MONOTONIC -g -F 199` of the whole run, with frame pointers, cut by
  `harness/cpufold.py --phases W/phases.json`: per phase, the share of samples with a kind of
  work on the stack, the kind of the innermost frame, and for samples revalidating memos, the
  query whose memo was verified, the query that asked and the blaze function that asked
  (**read**).
- A build with counters in salsa 0.28.5 (not committed: a patched copy of salsa and a dump at
  each request) gave per revision the deep verifications by durability and query, edges walked
  and executions. The heap: a `heap-profile` build under `_RJEM_MALLOC_CONF=prof:true` and
  `BLAZE_MEMORY`, read with `heap.py` (**read**).
- `BLAZE_MEMORY` makes edits slower (to 1.5 s), so timings come from runs without it and
  memory from runs with it (**read**).

## The cold start and the base build

nushell, one run: ready in 52.1 s (`load workspace: 41.4s`; the reference `cargo build` beside
it 46.3 s), resident 5.2 GB; the own base then took 21.2 s, peak 11.2 GB, 7.5 GB after. A warm
start took 37.2 s and recompiled everything, none reused: that is #41's and #216's (**read**).

CPU on the stack (a sample counts under every kind on its stack, so columns add up to more than
100%) and by the innermost frame of a kind (adds up to 100%) (**read**):

| kind | cold, on stack | base, on stack |
|---|---|---|
| parse a file | 13.4% | 1.3% |
| parse a macro expansion | 2.9% | 1.0% |
| item trees | 7.0% | 0.5% |
| bodies and signatures lowered | 6.2% | 2.9% |
| macro expansion | 30.8% | 8.5% |
| name resolution | 8.2% | 0.5% |
| inference | 10.8% | 14.0% |
| MIR building | 9.2% | 23.6% |
| MIR passes and monomorphization | 5.6% | 22.2% |
| code generation | 28.6% | 88.3% |
| the store | 6.0% | 0.8% |
| `*_with_diagnostics` and diagnostics | 7.7% | 2.4% |
| salsa revalidating | 5.1% | 7.8% |
| salsa interning | 3.3% | 3.3% |

| innermost frame, cold | share |
|---|---|
| other (allocator, kernel, std) | 24.2% |
| inference and traits | 12.7% |
| lexed and parsed (events) | 11.5% |
| salsa: memos fetched and stored | 10.9% |
| code generation | 10.3% |
| macro expansion | 4.7% |
| name resolution | 4.7% |
| token trees (syntax to and from) | 4.3% |
| salsa: memos revalidated | 4.1% |
| flat trees built | 3.2% |
| trees walked | 1.6% |
| ast id maps | 1.5% |

Within the parser's samples, `LexedStr::new` is 10%, `event::process` 8%, the flat tree builder
6.5% and putting trivia between tokens (`intersperse_trivia`) 3.6% (**read**). So a parser that
kept no trivia would save at most about 0.4% of the cold start, and a tree format leaner than
the flat one at most its 3.2% plus 1.6% (**unverified**: an upper bound, not a measurement).

Of the cold start's revalidation (5.1%), 2.3% is `Body::of` verifying name-resolution memos
(`ModuleScope::name`, `ModuleScope::legacy_macro`) during lowering (**read**).

## Edits

Body edits (a line in `main` of nushell's `src/main.rs`) took 0.33 to 0.39 s and signature
edits (`coerce_bool` in nu-protocol, called across the workspace) 0.79 s the first time, then
0.44 to 0.47 s (**read**). Where a body edit's 300 ms goes, from the daemon's `timeline-us`
(**read**):

| stage | body edit | signature edit |
|---|---|---|
| notice (the watcher) | 35 ms | 47 to 49 ms |
| apply (outlines lowered) | 21 ms | 31 ms |
| roots (the crates listed again) | 6 ms | 50 to 60 ms |
| check (stale functions found and compiled) | 155 to 200 ms | 220 to 257 ms |
| build-rest (waiting for the diagnostics) | 35 to 76 ms | 36 ms |
| refuse and link | 28 ms | 32 ms |

- A body edit's `check` is real work: `main` is inferred, lowered to MIR and compiled again.
  The diagnostics pass (`broken_in`, 205 to 250 ms on one thread for the one edited file) runs
  beside the build and is on the critical path by 35 to 76 ms; half of it is the borrow check of
  `main`, the rest waiting on queries the build is computing (**read**).
- Salsa per body edit: 264k deep verifications at LOW walking 1.44M edges, 3.6k executed,
  mostly `row_place` (1,806) and `source_place` (804) for the lines that moved, then
  `macro_arg` (281) and `parse_macro_expansion` (157); 17 at MEDIUM (**read**).
- A signature edit asks the patch base about 6,269 roots and 106,256 instances, because every
  instance whose home crate depends on nu-protocol might be stale. That is 1.7M deep
  verifications walking 30.6M edges (27.6M to memos already verified this revision) and 4.7k
  executions; `compiled_instance` alone is 106k verifications and 12.2M edges. Interned
  arguments of tracked functions are verified 3.67M times
  (`TraitImpls::in_crate_and_deps_for_trait_and_self_ty` 1.25M, `layout_of_ty` 0.67M,
  `blanket_in_crate_and_deps` 0.47M) (**read**).
- The 19 crates touched by the signature edit were listed again one after another, 0.2 to 7 ms
  each: that is most of the `roots` stage (**read**).

## Memory

The worker's heap after the base build, jemalloc's sampled bytes given to the holder by
`heap.py` (5,789 MB sampled) (**read**):

| holder | MB | share |
|---|---|---|
| salsa's records (edges, revisions) | 1,307 | 22.6% |
| blaze's own (vfs, symbol table, splice) | 988 | 17.1% |
| inference and traits | 639 | 11.0% |
| codegen and monomorphization | 617 | 10.7% |
| macro expansion (trees, arguments, span maps) | 443 | 7.7% |
| MIR | 435 | 7.5% |
| interned values | 411 | 7.1% |
| source maps | 248 | 4.3% |
| bodies and signatures | 242 | 4.2% |
| syntax held (parse, ast id maps, body syntax) | 169 | 2.9% |
| name resolution | 138 | 2.4% |
| layout | 127 | 2.2% |
| diagnostics kept | 18 | 0.3% |

`heap.py --syntax` counts all syntax, expansions included, at 587 MB (10.1%): expansion trees and
span maps 324 MB, ast id maps of bodies 106 MB; after the edits 4.8%. The parse trees the
build report shows (754 MB) go at the next revision, when the parse lru evicts (**read**).
Salsa's own count of its metadata is 898 MB (**read**).

## Experiments

- **Workspace files at MEDIUM durability, except the ones being edited** (a temporary branch
  setting their text again after load). Body edits: no change. The first signature edit:
  3.7 s, against 0.79 s, as every memo that read a file now at LOW could not backdate. Rejected
  (**read**).
- **Interned arguments that are never collected** (salsa-macro-rules' implicit interner of
  tracked functions with more than one argument given `REVISIONS = usize::MAX`, so no read
  edge is recorded and none verified): salsa's metadata 898 to 859 MB, a signature edit's
  revalidation about 245 to 220 ms and its edit-to-binary median 401 to 374 ms (7%); body edits
  unchanged. It needs a fork of salsa-macro-rules, and the arguments are never freed in a long
  session. An argument that holds an interned value salsa does collect could outlive it and
  name whatever takes its slot; that every argument's values are themselves never collected
  was not checked (**read** for the numbers, **unverified** for the rest).
- **Dependencies' inputs at `NEVER_CHANGE`** (not run): salsa drops edges to them, about 250 MB of
  the records, but setting one again panics, so every input the dependencies' memos read would
  have to be one, and a change to the crate graph would need a new worker (**read** in salsa's
  source, **unverified** for the size).

## What a lean path is, ranked by what was measured

1. **List the crates a build asks for on every core.** Landed
   ([below](#slice-1-crates-listed-on-every-core)).
2. **Ask the patch base about fewer instances on a wide edit.** The largest cost found:
   `compiled_instance`'s 12.2M edges and the interned arguments under it, about half of a
   signature edit's `check`. Either a firewall per instance (a query of what the instance's
   code depends on that backdates, so verification stops there) or asking only about instances
   that mention a changed item. The stale check is codegen's, so this goes in `crates/`
   (**unverified**: no prototype).
3. **Take the diagnostics pass off a body edit's critical path** (35 to 76 ms of about 300):
   split one file's diagnostics across cores by definition, and leave out the definitions the
   edit did not change, whose diagnostics are memoized. `blaze check` keeps the
   tolerant path whole: what it reports must stay rustc's (#222) (**unverified**).
4. **Interned arguments that are never collected**: 7% of a signature edit and 39 MB, from a
   one-line change in a pinned fork of salsa-macro-rules, once it is shown that no argument
   holds a value salsa collects (**read** for the numbers, as above).
5. **Dependencies never revalidated** (`NEVER_CHANGE` inputs, or frozen parts): memory, not
   time, as their memos are already verified shallowly. This is #40; nothing separate here.
6. **Syntax**: a parser that keeps no trivia (about 0.4% of the cold start) or drops trees once
   lowered (the parse lru and [flat-syntax.md](../flat-syntax.md)'s step (a) already do most of
   it: 2.9% of the heap). An item-only parse for item trees would save more of the parse but
   needs ast id maps and `def_syntax` that do not need the whole file (**unverified**).
7. **A strict mode without recovery or diagnostics**: under 0.1% of the CPU and 0.3% of the heap
   on a project without errors. Not worth a second path (**read**).

The cold start's biggest shares, macro expansion and parsing, are not the editor's design but
work done again that rustc keeps in `.rmeta`; not doing it again is #40 (frozen dependencies),
#41 (the warm manifest) and the expansion cache of [proc-macro-cache.md](proc-macro-cache.md).

## Slice 1: crates listed on every core

[#239](https://github.com/indexable-inc/blaze/pull/239). `build` listed each crate it builds (its functions, what it exports) one after another, though
`Listed::of_all` lists crates one to a core and was used only for linked crates. It now lists
them all first with `of_all`, which skips crates already listed. What is listed is the same.

nushell, main at `07217080d0` against the change on it, two interleaved runs each of
`leanprof.py nushell --no-warm --env BLAZE_STATS=1` on cores 16 to 31, medians of the
ten edits after the first of each kind (**read**):

| | main | listed on every core |
|---|---|---|
| signature edit, `roots` stage | 53.1 ms | 29.9 ms |
| signature edit, edit-to-binary | 410 ms | 373 ms (−9%) |
| signature edit, the harness's wall time | 0.461 s | 0.426 s (−7.5%) |
| body edit, the harness's wall time | 0.346 s | 0.346 s |
| own base | 21.3 s, 21.3 s | 21.3 s, 20.8 s |
| peak during the own base | 11.3 GB, 11.1 GB | 14.1 GB, 11.2 GB |
| peak during signature edits | 7.13 GB, 7.10 GB | 6.93 GB, 7.12 GB |

The cold start does not move (51.2 and 53.2 s, against 52.1 and 52.3 s): its 289 listings are
made before the first request, not by `build`. The own base's 17 listings took about 600 ms one after
another and 155 ms together, which its 18 s of compiling hides. The one peak of 14.1 GB was not
there again; the listings end before compiling begins, where the peak is (**read** for the
numbers, **unverified** that it was noise). The signature edit's 19 listings sum to 27 ms one
after another, but each takes 8 to 18 ms beside the others, verifying the same memos of
nu-protocol: what is left of the stage is that and the exported functions (**read**).

## Slices 2 to 4, and where a body edit's time goes

2026-10-25. As for slice 1, but on cores 120 to 135 with builds on 136 to 191,
`leanprof.py nushell --no-warm --env BLAZE_STATS=1 --rounds 6`, two interleaved runs per build
and medians of the 25 edits of each kind after the first. Each branch is measured against main at
its own base: `81648d0a9c` (#239) for the patch marks, salsa and borrowck branches and the crates
lookup, `08924cadc8` (#241, which brings #235) for the impls branch and for all of them together.
Profiles are `cpufold.py`'s, which now counts each sample for its period: at `-F 199` a thread
that just woke is sampled with a short period, and counting samples gave an edit's short bursts
on many cores more than their share (**read**).

**Where a nushell body edit's 300 ms went** on main, from the stage timeline and the stacks of
each thread over one edit (**read**): noticing it, 35 ms, most of which is the cargo client
starting and asking; applying it, 23 ms; naming the build's crates, about 40 ms, a walk of the
crate graph and of each root's path for every name; the first type check verifying
`TraitImpls::for_crate` of every crate it reaches, one after another through
`for_crate_and_deps` (an impl may be written in a body, so each crate's memo is verified again:
about 275M cycles an edit), 37 to 75 ms while the other threads wait for its claim; inferring
`main` (nushell's `main.rs` is one function of 507 lines and two small ones), about 70 ms; and
on the diagnostics thread borrow checking `main`, about 100 ms, which outlasted code generation
by about 46 ms and which `build` waits for; then the link, 14 ms.

| nushell | main | branch |
|---|---|---|
| **patch marks** (#243): signature edit | 394 / 397 ms | 338 / 326 ms |
| its `check` stage | 245 / 254 ms | 194 / 185 ms |
| **salsa vendored, interned values verified without a lock** (#244): signature edit | 394 / 397 ms | 382 / 386 ms |
| **crates named through a query** (#252): body edit | 304 / 312 ms | 279 / 270 ms |
| signature edit | 395 / 390 ms | 340 / 355 ms |
| **impls verified on every core** (#245): body edit | 317 / 313 ms | 302 / 300 ms |
| signature edit | 399 / 406 ms | 379 / 381 ms |
| **borrowck's drops liveness** (#246): body edit | 308 / 306 ms | 295 / 294 ms |
| its `build-rest` stage | 74 / 74 ms | 62 / 64 ms |
| **interned arguments kept**, six functions against salsa vendored: signature edit | 384 / 382 ms | 367 / 377 ms |
| the four that landed (#253), alone against main before it: body edit | 272 / 263 ms | 248 / 246 ms |
| signature edit | 320 / 365 ms | 294 / 280 ms |
| its `check` stage | 216 / 246 ms | 186 / 185 ms |
| **all of them**: body edit | 317 / 313 ms | 257 / 259 ms (−18%) |
| signature edit | 399 / 406 ms | 268 / 285 ms (−31%) |
| the harness's wall time, body / signature | 0.36 / 0.45 s | 0.31 / 0.33 s |
| cold start | 61.5 / 61.4 s | 62.7 / 61.7 s |
| peak during the own base | 11.0 / 11.1 GB | 11.0 / 11.0 GB |
| peak during body / signature edits | 7.5-7.6 / 6.9-7.0 GB | 7.5-7.6 / 7.0 GB |

All **read**. No branch moves the cold start or the peaks beyond the runs' spread, but for one:
the own base peaked at 13.9 to 14.1 GB in both runs of #252 alone and in one of the
impls branch, as it did once for slice 1, and never with all the branches (**read**; what it is
was not found, **unverified** that it is not a change's).

On ripgrep 15.1.0 (all the branches against `08924cadc8`, the same runs) edits were already short
and move little: a body edit 47.5 / 47.1 to 46.5 / 45.4 ms, a signature edit 89.6 / 90.8 to
86.0 / 87.9 ms; the cold start stays 7.4 s and the own base's peak 2.2 GB (**read**). Noticing
an edit, 38 ms, is about 45% of its wall time there.

**Slice 2, the patch base asked about fewer instances.** With debug info on, the patch built
against the base asked a `Mark` of every row it placed, each an interned call of `row_place` and
a walk of its edges. `root_mark` and `instance_mark` are now tracked queries that return the mark
by value, so they are the firewall per instance the issue proposed: a signature edit verifies one
memo per instance and backdates it when the lines did not move (#243). The other half of a
signature edit's verification was salsa taking each interned value's shard lock to verify it;
an interned value that is immortal, or was already verified in this revision, is now answered
from its generation without the lock (#244, with salsa, salsa-macros and salsa-macro-rules
vendored as published). Asking only about instances that mention a changed item was not built.
The 27.6M edges to memos already verified are each a lookup and a revision compare; with the
marks most of those of `row_place` are gone, and the rest were not measured again (**unverified**).

**The crates of a build named through queries.** `crates_named` and `crates_called` walked the
crate graph and the path of every crate's root for every name a build gave, several times a
build. The graph's names and roots are one query (`named_crates`), and a crate's dependencies
another (`deps_of`), both asked again only when the graph changes (#252).

**Slice 3, diagnostics off a body edit's critical path.** The diagnostics are not what the edit
waits for: borrow checking the edited function is, on the diagnostics thread, and within it
`maybe_initialized_at_drops`, which for every local walked all the move paths below it at every
block. A local is initialized in part where one of its paths is, so the set is now the locals of
the initialized paths (#246). `borrowck_ui.py --mode both` gives the same counts before and after
(polonius 1540 of 1581, NLL 1536 of 1579 in scope). The impls the type check verifies one crate
after another are now asked for on every core beside the stale check (#245). Splitting one file's
diagnostics by definition was not built: on nushell the edited file is one large function, whose
borrow check is the long pole, so it would gain little there (**unverified**). Left in borrowck:
`regions::build` (84M cycles an edit) and `liveness::region_liveness` (79M), and checking a
body's closures in parallel (**read** for the cycles).

**Slice 4, implicit interned arguments kept.** A tracked function of more than one argument
interns its arguments as a tuple, which salsa keeps for 3 revisions after it was last asked for and
then may collect and reuse. Each call of it records the tuple's read, an edge, and its LRU. A tuple
can be kept for good only if nothing in it is collected (a value it holds, freed and its slot
reused, would make the tuple name another value). What salsa collects in blaze (**read**, by
search of every `#[salsa::interned]` and `#[salsa::tracked]` struct): the tracked structs
`ModuleIdLt`, `BlockIdLt`, `Modules`, `ModuleScope` and `DefMapPair` (hir-def), and the interned
`InternedFileId` (ide-db) and `InternedSourceRootId` (base-db). Every other interned type,
the ids of `impl_intern_key!` (functions, impls, traits, closures and the rest), the solver's def
ids and syntax contexts among them, is declared `revisions = usize::MAX`; types, generic arguments
and clauses are `intern` crate values, reference counted and never collected (the crate's
`gc` is only called by the proc-macro server's copy); `Crate` is a salsa input. So no blanket
change: `ModuleScope::name` and many others take a `ModuleId` or a tracked struct. salsa-macros
now takes `revisions` on a tracked function, for its interned arguments, and four hot ones of
blaze's own whose arguments hold none of the collected types keep theirs for good: `row_place`,
`source_place`, `compiled_instance` and `instance_serial` (#253). Two more of the fork's, whose
arguments qualify too, were measured with them: `layout_of_ty_query` and
`TraitImpls::in_crate_and_deps_for_trait_and_self_ty`. They stay as they were, as CI builds the
fork alone against crates.io's salsa, which takes no `revisions` there. The table's first slice 4
row is of all six against vendored salsa; the second is of the four that landed, main at #253's
merge (`d4f6027e82`) against its first parent (`82fb05c1eb`), measured after it merged. One run
before was out of line with the other (365 against 320 ms), so the size is uncertain; the
direction held in all four pairs of runs, and the cold starts of both builds were about 4 s longer
than earlier that day (**read**; the host busier, **unverified**). salsa's own
tests pass with it (126 test binaries), and its compile-fail test of a single salsa struct's
function given `revisions` now says why; four other compile-fail expectations differ from rustc
1.99's output in rustc's own wording, which this does not touch (**read**). The −39 MB of salsa metadata measured earlier on a fork was not
measured again here (**unverified** for these functions).

**The first edit after the own base** took 12.3 s at the client while the daemon answered it in
335 ms: in its log the next request came 12 s after that reply, and the second edit's wall was
0.31 s (**read**). On ripgrep the first edit's wall is 0.16 s. The time is the client's, or
work that is not a request; it was not looked into (**unverified** where it goes).

## What is left, superseded

The rest of #236 is not going to be built on salsa: push-based invalidation, replacing salsa, is
being built on the `blaze-next` branch, and with it what an edit verifies changes. Left as found,
for that work to measure against: the first edit after the own base (12 s at the client on
nushell, above), `regions::build` and `liveness::region_liveness` in borrowck (84M and 79M
cycles an edit), the own base's occasional 14 GB peak, noticing an edit (35 to 48 ms, mostly the
cargo client starting, **unverified**), the per-definition split of diagnostics, the two fork
functions of slice 4, and slices 5 to 7 of the issue.

## Not checked

- helix was not measured; slices 2 to 4 were also measured on ripgrep, all together only.
- `debuginfo.py` did not run on the union of the branches (the dev shell has no gdb); the patch
  marks' lines rest on `splices.py`, the gate and CI's fast tier.
- The "all of them" rows include the two fork functions #253 left out.
- The counters' build was a different binary (patched salsa), so its counts and the timings
  come from different runs of the same commit.
