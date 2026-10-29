---
status: record
summary: What an edit inside one function redoes.
date: 2026-10-20
---
# What an edit inside one function redoes

Branch `fn-incremental`, from main at 46aff2550e, 2026-10-20. Measured on a loaded Mac (load
10 to 12 from other builds), so wall times are noisy; query counts are exact, and the processor
time the daemon spends on an edit is the steadier measure of work.

`harness/fnincr.py` starts the daemon in watch mode with salsa's execution counts on
(`RUSTD_TRACE`, `BLAZE_TRACE`), edits, and prints how often each query ran for the edit. Its
file is `harness/genbig.py`'s: 500 functions of 27 lines (a loop, a match, a closure, a
`panic!`) in one 13,500-line file, and the same functions in 50 files of 10 as the control.
`--workspace` takes a real file; here ripgrep 15.1.0's `crates/core/flags/defs.rs` (7,775 lines,
872 functions).

## Why it was the whole file

For an edit inside `f250` of the 500-function file, before:

| Query | Runs | What it is |
|---|---|---|
| `Body::with_source_map` | 507 | lowering every body of the file |
| `Body::of` | 1 to 500 | 1 when nothing moved, 250 when the bodies below moved |
| `InferenceResult::for_body` | 1 | |
| `mir_body_query` | 336 to 500 | |
| `mir_body_for_closure_query` | 672 to 1000 | |
| `MacroCallId::parse_macro_expansion` | 1310 | every macro of the file expanded again |
| `source_location` | 33,500 | the place of every panic of every function of the file |
| `row_location` | 31,020 to 41,002 | the line of every instruction, for the debugger |
| functions compiled | 1 (3 with a line added) | |

The edges, in the order an edit goes through them:

1. **Lowering reads the whole file.** `Body::with_source_map` takes the function's syntax
   through `ItemLoc::source`, so it depends on `parse(file)`, `ast_id_map(file)` and the file's
   span map. Every edit of a file changes all three (the `AstIdMap` holds pointers with their
   ranges), so every body of the file is lowered again. Upstream accepts this: `Body::of`
   copies the body out and backdates, so inference does not run again (read: PR #17670 says
   the query "is being invalidated all the time anyways (we have an extra query on top of it
   for the body incrementality that is not source dependent)"). That held here: inference ran
   for the edited function only.
2. **Evicted memos do not backdate.** `Body::with_source_map` is kept for 512 bodies,
   `parse_macro_expansion` for 512, `HirFileId::ast_id_map` for 1024; the program has 29,052
   bodies. salsa backdates a query only by comparing the new value with the old one
   (`backdate_if_appropriate` in salsa 0.28.5 needs `old_memo.value()`), so a query that runs
   again after its value was evicted counts as changed, and everything that read it runs again.
   The file's 1,310 macro calls were expanded again on every edit for this reason.
3. **blaze read positions where it needed none.** `mir_body` checked for failed expansions
   (`check_expansions`) through `Body::with_source_map`, and type names of closures
   (`closures_around`) ordered them through it. Neither result depends on where the body is,
   but both took the source map as an input, so MIR was lowered again for every function of the
   file: 336 to 500, plus their closures.
4. **Places were found from the whole file.** `source_location` (a panic's
   `core::panic::Location`) and `row_location` (debugger rows) are queries per place. They read
   the absolute source map, the file's parse and its line index, so all 64,000 of them ran again
   on every edit, and after a line added above they also came out different, which is right.

What was not the cause: compiled code never depended on positions. `Location`s and rows are
looked up when the object is stitched (`fragment.rs`, DEBUGINFO.md), so moving a function
compiled nothing again before this branch as well, and the body edit compiled the one function.

## The fix

Each step is a commit, measured on its own:

1. `failed_expansion` and `closures_around_query` are queries of their own, with results that
   do not change when the body moves: MIR is lowered again for the edited function only.
2. rust-analyzer lowers a body from its own syntax. `body_syntax(def)` is the anchor the body is
   written in, as a tree of its own: its green node, the part of the `AstIdMap` and of the span
   map inside it, all moved to begin at 0. The anchor is the one spans are already relative to
   (`RealSpanMap`): the function itself for a free function or a method, the `impl` or module
   for an item of one with attributes, the outer function for a function in a body. It runs on
   every edit of the file (cheap: one walk of the anchor) and stays equal unless the anchor's
   own text changed. `Body::relative` lowers from it; `Body::of` is its body;
   `Body::with_source_map` moves its source map to where the anchor is in the file, so a body
   is lowered once and the IDE's map and blaze's agree on every id (a `BlockId` is a tracked
   struct of the query that lowers it, so two lowerings would not). `RA_CHECK_RELATIVE_BODIES`
   compares every moved map with the body lowered in its file.
3. Places are found from the body alone (`places.rs`): a row and column in the anchor, from
   `Body::relative`, the anchor's text, and spans left unresolved
   (`map_node_range_up_rooted_span`); where the anchor begins is a query per body
   (`anchor_location`) added when a place is asked for. A place outside the anchor (a macro
   of the crate defined elsewhere) is found as before. `BLAZE_CHECK_PLACES` compares every
   place with the old way.

`harness/movedlines.py` checks the result end to end: one function panics in seven ways
(`panic!`, `unwrap`, an index, a `#[track_caller]` function, `assert_eq!`, a closure, an
overflow), lines are added and taken out above it and in the function above, each step is
compared with what `cargo build` prints, and moving the function compiles nothing.

## Before and after

Medians of 5 edits, each followed by its undo. CPU is the daemon's processor time for the edit,
all threads.

500 functions in one file:

| Edit | `Body::of` | MIR (fn + closures) | expansions | places + rows | CPU before | CPU after | edit to binary before | after |
|---|---|---|---|---|---|---|---|---|
| literal in f0 (top) | 1 / 1 | 1029 / 3 | 1310 / 1304 | 64,520 / 287 | 1370 ms | 840 ms | 308 ms | 330 ms |
| line added in f0 | 500 / 1 | 1500 / 3 | 1310 / 1304 | 74,504 / 366 | 1540 ms | 870 ms | 334 ms | 326 ms |
| literal in f250 (middle) | 1 / 1 | 1113 / 3 | 1310 / 1304 | 64,520 / 299 | 1410 ms | 850 ms | 313 ms | 348 ms |
| line added in f250 | 250 / 1 | 1500 / 3 | 1310 / 1304 | 69,504 / 247 | 1490 ms | 790 ms | 313 ms | 251 ms |
| literal in f499 (bottom) | 1 / 1 | 1254 / 3 | 1310 / 1304 | 64,520 / 223 | 1390 ms | 770 ms | 319 ms | 240 ms |
| line added in f499 | 1 / 1 | 1257 / 3 | 1313 / 1307 | 64,524 / 216 | 1400 ms | 780 ms | 327 ms | 226 ms |
| blank line at the top | 500 / 0 | 1500 / 0 | 1310 / 1304 | 74,500 / 82 | 1250 ms | 800 ms | 604 ms | 245 ms |

The same functions in 50 files (the control): 690 to 730 ms of CPU before, 640 to 660 after.
So a 500-function file cost 600 to 800 ms more CPU per edit than small files did, and now costs
130 to 220 ms more. Bodies lowered, inferred, lowered to MIR and compiled are the same 1 (3 with
a line added: the function, its closure, and the edit's own `let`) in both layouts.

ripgrep, `crates/core/flags/defs.rs`:

| Edit | `Body::of` | MIR | places + rows | CPU before | CPU after | edit to binary before | after |
|---|---|---|---|---|---|---|---|
| line added at line 612 (top) | 733 / 1 | 807 / 1 | 6,103 / 503 | 1230 ms | 1200 ms | 305 ms | 287 ms |
| literal at line 3741 (middle) | 469 / 1 | 538 / 1 | 5,654 / 116 | 1300 ms | 1180 ms | 414 ms | 271 ms |
| line added at line 3741 | 498 / 1 | 566 / 1 | 5,709 / 365 | 1310 ms | 1160 ms | 436 ms | 416 ms |
| line added at line 5728 | 487 / 1 | 553 / 1 | 5,689 / 171 | 1330 ms | 1240 ms | 426 ms | 340 ms |

ripgrep's flag functions are short (most return a string), so redoing all of them was cheap and
the edit's time is the build of the whole program: stitching ripgrep and its dependencies,
which no file layout changes. That is the case for the rest of this table too: of the 200 to
300 ms of an edit, `resolve`, `define`, `object` and `describe` (debug rows for every function
of the object) are most, and they are per program, not per file. A file of long functions
(nushell's are) gains more, as the synthetic file does. nushell itself is not measured here: no
checkout on this machine, and the real-project job pins it.

Tests: `cargo test --release -p hir-ide` fails the 18 tests it failed before
(`/tmp/blaze-verify/base.txt`), after two query-log snapshots were updated: in
`typing_inside_a_function_should_not_invalidate_types_in_another` the untouched function now
runs `body_syntax` and no longer `Body::with_source_map` and `Body::of`. `difftest.py corpus
--mixed 8`, `difftest.py mixfutures --mixed 16 --layouts` and `growth.py` pass as before; the
corpus also passes with `BLAZE_CHECK_PLACES=1 RA_CHECK_RELATIVE_BODIES=1` with no difference.

## What is left

- **The whole file is parsed again.** salsa's `parse` parses from scratch; rust-analyzer's
  incremental reparser (`syntax::parsing::reparsing`, a token or a block) is used only by
  completion and typing (read). Parsing the 13,500-line file, its `AstIdMap` and span map is
  most of the 25 ms of "apply" that remains against 1 ms for a small file. Feeding the edit to
  `Parse::reparse` would make it proportional to the edit.
- **`body_syntax`, `anchor_location` and `Body::with_source_map` still run once per body of the
  file** (500 here): each cheap (a walk of the anchor and an equality test, a line lookup, a
  move of a map) but proportional to the file.
- **The check of the edited files** (`broken_in`, rust-analyzer's semantic diagnostics of
  every edited file, run beside the build) asks for every body's source map and descends into
  every macro call: the 1,304 expansions in the table are its, evicted and expanded again. It
  is not on the path to the binary here (60 to 75 ms beside a 250 ms build), but it is whole-file
  work. After a cold start its first edit of a file also lowers that file's bodies again when
  `Body::relative` (kept for 4,096 bodies) let them go (871 on ripgrep's first edit).
- **Anchors bigger than a function.** A method of an `impl` with attributes, an item of a module
  with attributes and a function in a body are lowered with their anchor's text, so an edit
  elsewhere in that anchor lowers them again.
- **Upstream.** The body lowering and the moved source map are in the fork
  (indexable-inc/rust-analyzer, branch `blaze`). Upstream made `AstId`s stable by hashing an
  item's kind, name and parent (read: PR #19837, 2025), which is what lets `body_syntax`'s
  part of the `AstIdMap` stay equal; it has nothing like lowering a body from its own tree.
