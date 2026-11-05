---
status: record
summary: blaze's MIR against rustc's optimized MIR: match rates, passes ported and not, production gaps ranked.
date: 2026-10-20
---
# blaze's MIR against rustc's, and what production apps still need

2026-10-20. Claims are marked **read** (in source we read, or measured here, with the command),
**reported** (someone says so, unchecked) or **unverified** (our inference). rustc is 1.99.0
(`b940084d7`), the toolchain blaze pins; its pass sources are in
[docs/upstream/rustc-mir-transform/](../upstream/rustc-mir-transform/SOURCE.md).

## Which MIR to compare

- rustc generates code from its *optimized* MIR, which `-Zunpretty=mir` prints (read, in its
  output: no fake reads or false edges, which only optimized MIR lacks). At `-Copt-level=0` that is
  `-Zmir-opt-level=1`; above it, level 2 (read, `session-excerpt.rs`).
- At level 1 storage markers are gone, because no LLVM lifetime markers are emitted at
  `-Copt-level=0`; at level 2 they stay (read, `remove_storage_markers.rs`,
  `emit_lifetime_markers`).
- A debug build has rustc's UB checks: alignment and null checks before a dereference of a raw
  pointer, enum checks on transmutes (read, `check_alignment.rs`, `check_null.rs`,
  `check_enums.rs`; on when `debug_assertions`). blaze has none (read, no such assert kinds in
  `hir-ty/src/mir`).
- So blaze before this work (its runtime body, `BLAZE_MIR_OPT` unset) is compared with
  rustc's level 1, blaze's level 1 with rustc's level 1, and blaze's level 2 with rustc's
  `-Copt-level=2 -Zinline-mir=no` with debug assertions and overflow checks kept on, since
  blaze compiles the dev profile. UB checks are left out on rustc's side unless asked
  (`--ub-checks`), or they would mask everything else in a function that has one.

## The harness

`harness/mirparity.py` (read, it's ours): rustc's MIR through `cargo rustc -- -Zunpretty=mir`,
blaze's through the new `blaze-engine mir <dir> --crate <name> --level N`, which prints the bodies
blaze's code generator compiles in rustc's format. Functions are paired by path and parameter
count, normalized (block and local numbering, path spelling, non-scalar constants) and compared
at four strictnesses: `shape` (blocks and terminator kinds), `counts` (statements per block),
`identical` (the match rate) and `typed` (also local types).

## Match rates on `harness/corpus` crate `cases` (read, measured 2026-10-20)

1510 functions in rustc's MIR, 1115 in blaze's, 1070 paired; 931 were paired before impls
written by derives were named at the derive's line, as rustc names them.

| | shape | counts | identical | typed |
| --- | --- | --- | --- | --- |
| blaze, no optimizations, against rustc level 1 | 38.4% | 4.4% | 4.0% | 4.0% |
| blaze level 1 against rustc level 1 | 55.7% | 39.8% | 21.5% | 20.7% |
| blaze level 2 against rustc level 2 | 52.7% | 26.0% | 14.5% | 14.4% |

Most of the level 1 gain is the passes; moving out of temporaries in lowering, as rustc does,
took identical from 15.3% to 21.5% (read, the same runs before and after).

```sh
cd harness && BLAZE_BIN=<release blaze> python3 mirparity.py corpus --crate cases --level 1
python3 mirparity.py corpus --crate cases --level 2 --rustc-level 2
```

## What the passes cost and buy (read, measured 2026-10-20, macOS arm64, Cranelift)

The machine was shared, at a load average near 30 on 18 cores, so only differences well past
the run-to-run spread are reported; cargo's debug build, run alongside, moved up to 8% between
runs.

- **Code size.** `harness/corpus/kernels`: 147,352 bytes of code with no passes, 127,424
  at level 1 (-13.5%), 123,456 at level 2 (-16%), and 823 instances compiled instead of 867
  (a primitive's `clone` becomes a copy). `growth.py`'s 160-arm `serialize`: 380,856 bytes,
  366,744 at level 1, 303,168 at level 2 (-20%).
- **Run time**, best of 9 (`backends.py corpus kernels kernels --levels , --kernels --runs 9`):
  collatz 490 ms to 421 at level 1 (-14%), fnv 284 to 222 (-22%); sieve, qsort, matmul,
  nbody, mandelbrot and words within the spread (-4% to +4%).
- **Compile time.** Optimizing every body of corpus `cases` (1115) takes 24 to 71 ms at
  level 1 and 33 to 191 ms at level 2, against 0.6 to 4 s to build them (`blaze-engine mir --time`,
  the spread is the load). The kernels' cold compile: 103 ms with no passes, 104 at level 1,
  122 at level 2. An edit re-optimizes only the bodies it changed, as the passes are salsa
  queries; `movedlines.py`'s six edits took 10 to 27 ms with no passes, 10 to 18 at level 1 and 9 to 20 at level 2. `editbench.py` is
  Linux-only (it reads `/proc`).
- **The daemon's edit loop**, `bench.py ripgrep --backend cranelift --runs 3`, twice at each
  level in alternating order: a body edit took 0.22 and 0.25 s with no passes, 0.18 and 0.19 s
  at level 2; signature, leaf and generic edits 0.06 to 0.15 s at either level, every one
  spliced in by the daemon. No cost shows past the spread.
- **Default.** Off: level 1 costs almost nothing per edit and is the safest to turn on next,
  once it has run on more than the corpus and the interpreter's tests.

## What still differs, most frequent first (read, the harness's lists at level 1)

1. **Matches are chains of tests.** rustc lowers a `match` on an enum to one `switchInt` with a
   target per variant; blaze's lowering tests one variant at a time, re-reading the
   discriminant (`switchInt(move _) -> [0: bb, 1: bb, otherwise: bb]` in 82 functions on
   rustc's side). Fix: lower matches through a decision tree as `rustc_mir_build` does, or a
   pass that merges a chain of switches on one discriminant (unverified which is less work).
2. **Operands of user variables and places.** `_ = copy _` (190 functions) and
   `switchInt(copy _) -> [0: bb, otherwise: bb]` and `[1: bb, otherwise: bb]` (202 and 181): a place expression is read into a temporary first, where
   rustc reads the place directly (`switchInt(copy _1)` for `if b`). Lowering, not passes.
3. **Unwinding out of drops.** rustc's `drop(_) -> [return: bb, unwind terminate(cleanup)]`
   and `unwind continue` against blaze's cleanup blocks (about 100 functions): rustc's
   `AbortUnwindingCalls` and cleanup simplification.
4. **Reborrows** `_ = &(*_)` (420 functions before optimization, out of the top differences
   at level 1): `InstSimplify` removes `&*` of a reference (read); rustc's
   `ReferencePropagation` (level 2) would remove more (unverified).
5. **Pointer casts of fat pointers** `copy _.0 as *const` (61): how blaze lowers trait upcasts.
6. **Derive bodies** rustc's `#[derive(PartialEq)]` and `Debug` expand differently from
   rust-analyzer's builtin derive expansion; most of the 440 unpaired rustc functions are
   derive helpers (`assert_fields_are_eq`) and trait items rust-analyzer does not lower.

## Passes: ported and not

The table in [SOURCE.md](../upstream/rustc-mir-transform/SOURCE.md) has every pass of 1.99's
pipeline and whether blaze ports it; `passes_not_ported(level)` in
`hir-ty/src/mir/optimize.rs` lists the rest at each level. Ported: everything rustc runs at
level 1 except `RemoveZsts`, `UnreachableEnumBranching` and `ForceInline`; at level 2 also
`UnreachablePropagation` and `DeadStoreElimination`. Not ported at level 2: `Inline`, `GVN`,
`ReferencePropagation`, `ScalarReplacementOfAggregates`, `SsaRangePropagation`,
`MatchBranchSimplification`, `JumpThreading`, `EarlyOtherwiseBranch`,
`SimplifyComparisonIntegral` and `DestinationPropagation`.

## Production gaps

Ranked by how likely each is to hurt a real app, then by cost. "Fixed" is in this branch.

0. **A program blaze builds alone does nothing on macOS** (unknown cost, found here). When
   blaze compiles every crate and puts its own C `main` in the object (`define_start` in
   `hir-ty-codegen`), the linked `_main` on macOS arm64 is at the address of the allocator shim's
   `__rust_alloc`, a branch to `__rdl_alloc`, so the program returns at once with status 0
   and prints nothing (read: `nm` and `otool -tV` on `runbench.py`'s kernels build; a raw
   `write(2, ..)` as the first line of `main` never shows). Unmodified `main` too, Cranelift and
   LLVM alike; `blaze build`, `difftest.py` and `backends.py` keep rustc's `main` and are
   not affected; `runbench.py` builds this way. Linux not checked.
   It is why the README's runtime table has no macOS numbers.
   **Fixed on `whole-program-main`** (2026-10-20, read): the stand-in rustc builds for the
   program defines a weak, code-free `_main`, and blaze's object was weakened as a patch is,
   so ld64 was free to keep the stand-in's; blaze's `_main` now stays strong
   (`weaken::ours`). The same builds also ignored a `#[global_allocator]` on macOS and on
   x86-64 Linux (read, `wholeprogram.py` against unmodified main on both): stand-ins declare
   none, so rustc's shim forwarded to `__rdl_alloc`. Linux's `main` was right, as ELF objects
   are not weakened.
1. **Debug-build UB checks** (medium). A program that dereferences a misaligned or null raw
   pointer, or transmutes an invalid enum value, panics under rustc's debug build and carries on
   under blaze (read, as above). Port `CheckAlignment`, `CheckNull` and `CheckEnums`, with
   the three panics in codegen and the interpreter.
2. **Cargo's `build-dir` and its new layout** (medium). blaze reads `<profile>/deps`,
   `build` and `.fingerprint` and doesn't read `build-dir` (read, `deps.rs`,
   `cargo/state.rs`). With `build.build-dir` set, or once layout v2 is cargo's default
   (reported to be coming, rust-compiler-2026.md), it would not find dependencies (unverified).
3. **No borrow checker** (large). Programs rustc rejects run (read, `docs/support.md`). The
   advice to keep `cargo check` running stays; Polonius changes what it says, not this.
4. **Unwinding out of `nounwind` code** (medium). A panic that unwinds out of a drop during
   unwinding, or out of an `extern "C"` function, aborts under rustc
   (`AbortUnwindingCalls`, `UnwindAction::Terminate`); blaze has it in some places only (read,
   `docs/mir-passes.md`, "Not done yet", and the harness's `unwind terminate` counts).
5. **Code quality at level 2** (large). No MIR inlining, `GVN`, `ScalarReplacementOfAggregates`
   or `DestinationPropagation`, and matches as chains of tests: Cranelift does none of these
   itself (unverified). The LLVM tier is on LLVM 20 where rustc is on 23 (read, `docs/support.md`).
6. **`-Zembed-metadata=no`** (small). blaze reads `.rmeta` files and skips `lib.rmeta` in
   rlibs, so it should work (read), but nothing tests it.
7. **Fixed here:** a `match` arm guard's value was marked dead (`StorageDead`) before the switch
   that reads it; harmless while storage markers are stripped, a wrong branch once dead-store
   elimination honours them (read, the new `levels_agree_on_matches_and_options` test caught
   it). Parameters no longer get storage markers, which rustc's never have. The printer writes
   `debug` lines in declaration order, as rustc does.
