---
status: record
summary: rustc's own test suites through blaze.
date: 2026-10-20
---
# rustc's own test suites through blaze

Branch `blaze-rustc-suites`, off `blaze-collate`; round two merges `blaze-collate` again, last at
`3d47ed59bf`. Everything below was measured on one Linux x86-64 VM with stable
`rustc 1.99.0` and the rust-lang/rust checkout at the `1.99.0` tag. Each number is from a
single run unless it says otherwise. "Cranelift" is `cargo build -p blaze-engine --release`; "LLVM"
is the same with `--features llvm` (`LLVM_SYS_201_PREFIX=/usr/lib/llvm-20`), run with
`BLAZE_BACKEND=llvm BLAZE_OPT=2 BLAZE_EMIT=fast`. Both binaries are built from the head of
this branch.

The numbers in the Summary and in "Round two" are from the last run of round two. The
sections after "Round two" describe round one and are kept as written then, except where
they say otherwise.

## Summary

| suite | backend | tests run | matching | differing | left out | functions from blaze | left to rustc |
|---|---|---|---|---|---|---|---|
| `tests/ui` run-pass | Cranelift | 2787 | 2773 | 14 | 995 | 5726 of 5750 | 24 |
| `tests/ui` run-pass | LLVM | 2785 | 2770 | 15 | 997 | 5723 of 5745 | 22 |
| `coretests` | Cranelift | 1197 | 1197 | 0 | see Part 2 | 1494 of 1499 | 5 |
| `coretests` | LLVM | 1197 | 1197 | 0 | | 1494 of 1499 | 5 |
| `alloctests` | Cranelift | 703 | 703 | 0 | | 966 of 966 | 0 |
| `alloctests` | LLVM | 703 | 703 | 0 | | 966 of 966 | 0 |
| std's tests (28 binaries) | Cranelift | 252 | 252 | 0 | | 380 of 381 | 1 |
| std's tests (28 binaries) | LLVM | 252 | 252 | 0 | | 380 of 381 | 1 |

No blaze crash, hang or link error in any of the eight runs.

Round one ended at 2690 `tests/ui` tests run, 4 differing, 57 functions left to rustc, and 11
left in the libraries. Round two runs about 96 more tests and leaves 24 + 6 functions to rustc (22 + 6 on LLVM).
The two backends do not run the same number of tests because the tests that are left out
for printing something else in every run, or for racing, vary from run to run: 11 in the
Cranelift run and 13 in the LLVM run here, and the four runs on this head and the one before
it had 2785, 2786, 2787 and 2785 tests. The differing tests were the same in each.
The differing count went from 4 to 14 (15 on LLVM) because tests that were left out are now
run, not because anything that matched stopped matching: 9 of the new ones are a family of
tests marked `known-bug` upstream, one needs the optimiser, and the LLVM one comes from the
gate's own `BLAZE_OPT=2`. They are listed under "Round two: differing".

Where round one started, with `blaze-collate` as found: Cranelift 2650 tests run, 16
differing, 1 link error; LLVM 2649 run, 17 differing, 1 link error; `coretests` hung.

## Round two

### 1. Functions left to rustc

Round one left 57 functions in `tests/ui` and 11 in the libraries. 30 are left now (24 + 5 +
1 on Cranelift), and that includes functions of the tests that are newly run. Each fix below
has a reduced case in `harness/breakers/` unless it says otherwise. The first column is the
round-one count for that reason where the round-one table had one; I did not re-derive it
function by function.

| was left | cause | fix | breaker |
|---|---|---|---|
| 13 | `async fn` with `ref` pattern parameters: the closure analysis counted locals the coroutine owns | skip locals owned by the closure | `ref_async_parameter` |
| 7 | pattern on a union | lower a union pattern to its one field | `union_pattern` |
| 4 | upcast to a supertrait the vtable names with other lifetimes, a binder or a projection | compare traits with lifetimes erased and projections normalised | `upcast_through_lifetimes` |
| 3 | range and path patterns over `<T>::CONST`, `Self` as a unit struct pattern | take the constant inference resolved; a unit struct pattern always matches | `path_patterns` |
| 2 | `%` on floats in a constant | implemented | `float_remainder_constant` |
| not counted apart | `[x; N]` argument coerced to a slice of trait objects | resolve the expected type before reading its element type | `repeat_coerced_argument` |
| 2 + 1 (libraries) | `type_name` of function items, constructors, `dyn` with projections, `for<'a>` references, variadic pointers | spelled as rustc spells them | `spelled_type_names` |
| 1 + 2 (libraries) | `PinMacroHelper<[PhantomPinned]>`, and an empty array compared with a slice: the element type was never inferred | well-formedness goals for the types of an expectation | `expectation_not_well_formed`, `empty_array_by_impl` |
| not counted apart | `panic!` in a macro of an older edition | the rustc rule: walk up from where the call is written, through sysroot macros, to the edition of the code that wrote it | `panic_of_old_edition` |
| 1 (part) | `m! {} | 1` as a match arm: a braced macro call ended the arm | a braced macro call is an expression in a match arm | `braced_macro_arm`, parser test `match_arm_braced_macro_operand` |
| 1 | a static of an `extern` block: blaze looked for it among rustc's symbols | import it under its link name | `foreign_static` |
| 1 | a trait method whose own `where` clause the type does not meet was compiled for the vtable | leave the slot empty, as rustc does | `uncallable_vtable_slot` |
| not counted apart | proc-macro crates and `proc_macro` loaded as if their impls were in scope | a proc-macro crate gives its macros and nothing else | covered by the `proc-macro/*` tests only |

Two things found on the way that were worse than a refusal:

- The well-formedness goals above made blaze crash on
  `regions/regions-relate-bound-regions-on-closures-to-inference-variables.rs` (index out of
  bounds in the solver) during this round. Types under a `for<..>` they name are now passed
  over. Breaker `well_formed_under_binder`. The crash never reached a pushed commit.
- With `-O`, a small `main` that blaze refused left the program with a C `main` calling a
  function that was not there, and the link failed. blaze now drops its start shim when the
  `main` behind it is left to rustc. I verified this by hand and have no breaker for it,
  because the corpus is not built with `-O`.

`harness/breakers.py` and `harness/keys.py` counted a function as "not skipped" when its
reason contained the words "no rustc symbol", which is also part of two real refusals. They
now look for "no rustc symbol for function", as the other harnesses do. `breakers.py` had
been hiding one skipped function that way.

What is still left to rustc, with why. Every one is refused with the reason in the second
column, and the test still matches because rustc's copy of the function runs.

| functions | reason blaze gives | tests | why it is left |
|---|---|---|---|
| 6 | names or bodies missing because "a macro of the crate did not expand: proc-macro panicked" | `proc-macro/attr-args.rs`, `attr-cfg.rs`, `attr-on-trait.rs`, `derive-same-struct.rs`, `expand-with-a-macro.rs`, `load-two.rs` | These macros `assert_eq!` on `TokenStream::to_string()` of their input. blaze runs proc macros through the toolchain's proc-macro server, whose token streams do not print with rustc's spacing, so the macro panics. blaze should refuse here: it cannot give the macro the input rustc gives it. Breaker `macro_reads_its_spelling` (left). The reason names the first macro of the crate that failed, which may not be the one the function uses. |
| 4 | calling convention `win64` | `asm/x86_64/multiple-clobber-abi.rs`, `c-variadic/same-program-multiple-abis-x86_64.rs` (2), `extern/extern-calling-convention-test.rs` | `extern "win64"` on Linux. The Windows x64 argument code exists in blaze but is only selected for Windows targets; choosing an ABI per function is the work on `blaze-plat-abi2`. |
| 2 | `asm!` with a `sym` | `abi/abi-sysv64-register-usage.rs`, `asm/x86_64/multiple-clobber-abi.rs` | blaze assembles `asm!` text itself. The `blaze-plat-abi2` work that came in with `blaze-collate` puts one `sym` operand into it, which is what took `c-variadic/naked.rs` off this list; these two have more than one. Not done here. |
| 1 | an `asm!` constant that is not a literal | `asm/x86_64/const.rs` | `const constfn(5)` as an operand. blaze lowers a `const { }` expression as ordinary code in the body, so there is no value when the text is assembled. Not done. |
| 2 | incomplete expression | `rust-2021/reserved-prefixes-via-macro.rs`, `rust-2024/reserved-guarded-strings-via-macro.rs` | A proc macro makes tokens that would be a reserved prefix if written in the source. blaze turns the stream back into text and parses it again, where they are one token. Not done. |
| 1 | body has errors | `parser/macro/statement-boundaries.rs` | An `$e:expr` fragment that is an `if` or block, followed by something that could continue it. rustc keeps the fragment as one expression; blaze pastes its tokens and reads a statement boundary. Breaker `fragment_ends_statement` (left). |
| 3 | instantiation fails with a type or constant error | `const-generics/min_const_generics/macro.rs`, `impl-trait/closure-in-impl-trait.rs`, `closure-in-impl-trait-arg.rs` | A closure or a `macro_rules!` definition inside an array length or const argument in a signature. The analysis gives such an anonymous constant no body of its own. Not done, no breaker. |
| 1 | method `len` does not resolve | `coherence/pr-review-132289-2.rs` | The test pins which impl a `dyn Trait<P, Assoc = A>` projection picks when a blanket impl in another crate also applies; the analysis leaves the type unknown. Not done, no breaker. |
| 1 | `type_id` of a type with lifetimes in a constant | `consts/const-typeid-of-rpass.rs` | blaze does not compute rustc's `TypeId` hashes at compile time, so a constant that holds a `TypeId` is refused whatever the type; it only answers constants that compare two. blaze should refuse until it can compute the hash. |
| 1 | writable static `CACHE` without a rustc symbol | `rfcs/rfc-2396-target_feature-11/return-fn-ptr.rs` | `is_x86_feature_detected!` reads a private static of `std` with interior mutability. rustc's `std` exports no symbol for it, and a second copy would not be the one `std` writes. blaze should refuse. |
| 2 (Cranelift only) | `ImplLimitExceeded` | `codegen/huge-stacks.rs`, both revisions | A stack frame of 4 GB and more, past what Cranelift takes. LLVM compiles them. |
| 5 (`coretests`) | the caller's location in a constant | `panic::location::*` | blaze does not know the file name rustc records there. Breaker `caller_in_constant` (left). |
| 1 (std) | `type_name` of a function item with type arguments | `type-name-unsized.rs` | Not done. |

So the list is not down to "nightly-only or should be refused". By my reading 12 of the 30
are refusals blaze should keep making (proc-macro spelling 6, `TypeId` constant 1, `CACHE` 1,
Win64 until `blaze-plat-abi2` 4), 2 are a Cranelift limit, and 16 are gaps I did not close
(`asm!` 3, reserved prefixes 2, fragment boundary 1, anonymous constants 3, the coherence
case 1, caller location 5, `type_name` 1). None is silent.

### 2. Release-mode comparisons

Round one left out 60 tests for flags: 47 set an optimisation level, 5 turn checks off, 8
ask for LTO. The harness now passes `-O`, `-C opt-level`, `-C debug-assertions`,
`-C overflow-checks`, `-C panic=abort` and `-C target-feature`/`target-cpu` to cargo for the
workspace the test is built in, and blaze reads them from the build as it reads a profile. Counted over the tests
that ran in an earlier Cranelift run of this round (2787 tests run; the set has not changed
since but for the tests that race):

| flag in `compile-flags` | tests run | matching | differing |
|---|---|---|---|
| `-O` or `-C opt-level` (7 of them `opt-level=3`) | 46 | 36 | 10 |
| `-C debug-assertions` or `-C overflow-checks`, on or off | 31 | 31 | 0 |
| `-C panic=abort` or `=unwind` | 4 | 4 | 0 |

What "release mode" means here: rustc's side is optimised as the test asks. On blaze's side
the checks and the panic strategy follow the flags; Cranelift does not optimise, and LLVM
runs at `BLAZE_OPT=2` whatever the test says. The 11 LTO tests are still left out: with LTO
rustc inlines the test into the program and nothing of blaze's would run.

Two bugs came out of this and are fixed: a closure with
`#[rustc_inherit_overflow_checks]` in a library (`iter().sum()` overflow) did not take the
setting of the crate that calls it (`overflow_in_library_closure`), and constant evaluation
panicked on overflow in a build with checks off.

### 3. Harness limits

Left out went from 1114 of 3804 to 996 of 3782 (995 and 997 in the last two runs). The total is 22 lower because the harness
now takes a test's revisions and `ignore-`/`only-` directives per revision; I did not check
that this accounts for all 22. By limit:

| round one | now | what changed |
|---|---|---|
| 73 without a top-level `fn main()` | 0 | `main` is found when a macro writes it or it is spelled with other spacing |
| 47 + 5 optimisation and check flags | 0 | passed through, see above |
| 12 auxiliary crates with flags of their own | 8 | an auxiliary crate's edition, `opt-level`, `overflow-checks` and `debug-assertions` go into its package; `-Z`, `--crate-type`, `--cfg`, `-Cprefer-dynamic` stay out |
| 8 `#![no_std]` | 0 | built as written |
| 7 `rustc-env` | 0 | set for the build; `--env-set` (2) is still out |
| 7 + 4 files not there | 1 | files beside the test are linked in |
| 6 `--test` | 23 | not lifted: the program is libtest's. The count is higher because this reason is now given before others a test also has; I did not trace where each of the 17 was counted in round one |
| 18 + 9 do not build as a package | 19 + 11 | not lifted |

`run-flags`, `exec-env` and `edition` were already honoured; in the last run 4 tests with
`exec-env`, 147 with an edition directive and 257 with auxiliary crates ran. 11 to 14 tests
are left out in each run because rustc's own program prints something else every time; the
exact tests vary between runs.

`check-run-results`: the harness now holds what blaze's program prints against the test's
`.run.stdout` and `.run.stderr` files (a missing file meaning nothing is printed), after
taking out source paths with their line and column and the thread id, which a build in a
package changes. In the last run, on each backend, all 28 tests with the directive match rustc's program, and
26 print what the files say. The other two are the two
revisions of `compiletest-self-test/normalize-with-revision.rs`, which tests compiletest's
own `normalize-stderr` rules; the harness does not apply those. A test that does not print
what its files say is listed by the harness but is not counted as differing.

### 4. `opt-level=3` directives and `tests/run-make`

The `opt-level` directives are part of item 2. `tests/run-make` was not attempted: every
test there is an `rmake.rs` program that drives rustc itself through the `run_make_support`
library, so there is no plain program to hand to the harness without writing a second
driver. `tests/codegen-units` and `tests/mir-opt` have no run-pass tests; they compare
compiler output.

### 5. `VaList` on AArch64 and Windows x64

- AArch64: `decays_in_c` is now asked first for each argument of a C-ABI call, and a type it
  says yes to is classified `Aapcs64::Memory` (passed by address) before `aapcs64_class` runs.
- Windows x64: `win64_pass_mode` returns `PassMode::Indirect` for an argument (not a return
  value) that `decays_in_c` says yes to, before it looks at size.
- `decays_in_c` is true only for a struct marked `#[rustc_pass_indirectly_in_non_rustic_abis]`,
  through transparent wrappers. In core 1.99 the AArch64 `VaListInner` and the Windows
  `VaList` do not have the attribute (only x86-64 SysV, PowerPC, s390x, Xtensa and Hexagon
  do), so no argument should change class on those two targets; a mistake would show in
  `harness/breakers` case `list_by_address` and in `tests/ui/abi/variadic-ffi.rs`, which
  pass a `VaList` to C `vsnprintf`. Neither target was run here.

### 6. Deslop pass

Done over the round-two diff against the merge of `blaze-collate`, with the checklist of the
`deslop-rust-skill`: no `unwrap` or `expect` added on an input path, no comment that narrates
an edit, no comment trailing its code, new items private except `inherits_overflow_checks` (a method in `hir-def` and a field of
the MIR body), which code generation reads. `cargo clippy` over the changed crates reported
three lints on lines of mine (a `clone` of a `Copy` value and two needless borrows), which
are fixed. Nothing was restructured.

### Round two: differing

The four from round one are unchanged (see "Still differing"). New, because the tests are
now run:

| test | backend | cause |
|---|---|---|
| `codegen/equal-pointers-unequal/{as-cast,exposed-provenance,strict-provenance}/{inline1,inline2,zero}.rs` (9) | both | Marked `known-bug` for rust-lang/rust#107975, built with `-O`. Each asserts that two pointers with equal addresses compare unequal, which is the LLVM miscompilation the issue is about. blaze's program compares them equal. No breaker: there is nothing to reduce to but the bug. |
| `codegen/issue-28950.rs` | both | Built with `-O`. A thread with a 16 KB stack makes a large temporary that the test expects the optimiser to remove. rustc `-O` removes it; blaze's code keeps it and the thread overflows its stack (signal 11 on Cranelift, abort on LLVM). Open, no breaker: the corpus is not built with `-O`. |
| `panics/panic-abort-backtrace-without-debuginfo.rs` | LLVM | The test asks for `opt-level=0` so that two functions stay in the backtrace. The gate's `BLAZE_OPT=2` inlines them. With `BLAZE_OPT=0` the test matches (measured once). |

The harness prints the `known-bug` issue next to a differing test and counts them: 10 of the
14 (the nine above and `metavar_cross_edition_recursive_macros.rs`).

`array-slice-vec/nested-vec-3.rs` differed for a while in this round and matches at this
head. It builds a `vec!` of `vec!`s and panics in the middle, and checks the order of the
drops. `blaze-collate` at `8f21942505` dropped the finished inner vectors before the
elements of the one under construction, where rustc drops the innermost operands first; it
matched at `ce52276cf5`. The `blaze-fuzz` branch had fixed the same thing from a fuzzer
finding (`5fba09c841`, breaker `unwind_nested_operands`), so that commit is cherry-picked
here unchanged apart from its context lines. `blaze-collate` has it too as of `3d47ed59bf`.

Two things broke when `blaze-collate` `14ee89181b` was merged, and both are fixed here:

- Four tests differed on Cranelift (`io-checks/write-macro-error.rs`,
  `iterators/iter-filter-count-debug-check.rs`, `iterators/iter-sum-overflow-debug.rs`,
  `iterators/iter-sum-overflow-overflow-checks.rs`; the LLVM run was not made at that
  point). Each panics in code of the standard library that is compiled into the test, and
  prints where. rustc names the file where the sources are installed
  (`~/.rustup/toolchains/../lib/rustlib/src/rust/library/..`); blaze printed
  `/rustc/<commit>/library/..`. The collated store names library files by where they were
  built so that an entry means the same on every machine, and the same name had become
  what the program says. Now the program is given the installed path, and the path is
  rewritten to the built one when an entry is stored and back when it is loaded, as is
  already done for files under the cargo home. Breaker `library_panic_location`, and a unit
  test in `codegen/cached.rs` for the round trip.
- `coretests` did not finish compiling: after 12 minutes blaze was still in
  `Evaluator::patch_addresses`. `tests/slice.rs` has `const EMPTY_MAX: &[()] = &[(); usize::MAX]`
  and takes a reference to it. When a constant's memory is copied, blaze goes over every
  element behind a slice to carry addresses over, and did so for elements of no size too.
  A value without bytes is now skipped. Breaker `unit_slice_constant`; it hangs on the
  binary from before the fix (stopped after 60 seconds) and takes seconds after.

One mistake of mine to report: a probe script ran in my home directory after a failed `cd`
and executed `rm -rf blaze; mkdir blaze` there. I found no sign a `~/blaze` existed before,
and removed what the script made. The repository checkout is elsewhere and was not touched.

### Round two: gate

This gate was run on the merge of `blaze-collate` `14ee89181b` (`617524cf63`, pushed as
`f8b880e65a`), not on the head of the branch. On the head, which merges `3d47ed59bf`, both
binaries build, the four suites were run on both backends (the Summary numbers), and the
Cranelift leg of the gate passed its first nine scripts (`difftest.py` through
`worktrees.py`) before the run was stopped; the rest of that leg, the LLVM leg and
`cargo test` were not run on it.

The harness list of `.github/workflows/blaze.yaml` as of `blaze-collate` `14ee89181b`, run
from `harness/` with `BLAZE_BIN` at each binary, plus `blaze_cli.py --small`. Every script
exited 0 on both backends. `buildmodel.py` runs on the Cranelift leg and `backends.py` on
the LLVM leg, as in the workflow. `remote.py` was not run: there is no MinIO on this machine.

| command | Cranelift | LLVM |
|---|---|---|
| `difftest.py corpus --bin runner --crate cases --layouts` | MATCH, 822 of 822 functions, 0 of 893 layouts differ | the same |
| `programs.py` | 22 of 22 | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts | 99 of 99, 0 restarts |
| `breakers.py` | 80 of 80, 8 open, 0 functions skipped | the same |
| `unwinds.py`, `splices.py`, `asmedits.py`, `includes.py`, `worktrees.py` | 8 of 8, 71 of 71, 10 of 10, 9 of 9, 62 of 62 | the same |
| `keys.py` | 46 of 46 | 46 of 46 |
| `fuzz/fuzz.py smoke` | 505 functions, 0 differences, 0 crashes, 0 left to rustc | the same |
| `backends.py corpus runner cases --bin runner --levels 2f` | not part of this leg | same output as cargo's |
| `debuginfo.py` | all right | all right |
| `buildmodel.py` | 194 of 194 | not part of this leg |
| `storekills.py --kills 100`, `storecorrupts.py --rounds 4`, `storefull.py`, `storeshared.py`, `storeguards.py`, `selects.py` | pass | pass |
| `blaze_cli.py --small` | pass | pass |

The 8 that `breakers.py` calls open are five cases that still differ (`assert_message`,
`unreachable_arm_move`, `raw_write_then_match`, `future_unwind_flags`,
`fragment_edition_through_macro`) and three that blaze refuses with a reason
(`caller_in_constant`, `fragment_ends_statement`, `macro_reads_its_spelling`).

`cargo fmt --all -- --check` is clean. `cargo test --release -p hir-ty -p hir-def -p parser
-p syntax -p blaze-engine`: 1013 passed, 0 failed. One expectation was changed for that:
`test_assert_expand_2015` in `hir-def` carried a FIXME saying its expected expansion was the
wrong one (`panic_2021!` in a 2015 crate); with the panic-edition rule of this round the
expansion is `$crate::panic!`, and the test now expects that.

### Where this stands

Done: the merge of `blaze-collate` `3d47ed59bf`, the suite runs on it under both backends,
the two regressions the `14ee89181b` merge brought (`library_panic_location`,
`unit_slice_constant`), and items 2, 3, 5 and 6 of round two.

Half-done: the gate on the head, as said above. Item 1: 24 + 6 functions are still left to
rustc (22 + 6 on LLVM), each with a stated reason, but the list is not down to "nightly-only
or should be refused"; the table in item 1 says which are not done. `blaze-collate-next` is
not merged.

Not done: item 4 (`tests/run-make`), `remote.py`, and any target but Linux x86-64.

Next, in this order: finish the gate on the head under both backends and `cargo test`;
`asm!` with more than one `sym` and with a constant operand (3 functions); the proc-macro
token spacing behind `macro_reads_its_spelling` and the reserved-prefix cases (8);
`extern "win64"` on Linux once an ABI can be chosen per function (4); `codegen/issue-28950.rs`,
which needs the large temporary removed without an optimiser.

## Part 1: `tests/ui` run-pass

    git clone --filter=blob:none --sparse https://github.com/rust-lang/rust -b 1.99.0
    git sparse-checkout set tests/ui tests/codegen-units library
    BLAZE_BIN=<blaze> python3 harness/uitest.py <rust> --work <dir> --fresh --json ui.json

`harness/uitest.py` was there; it ran tests one at a time and left out anything with an
auxiliary crate. It now accounts for every file marked `//@ run-pass` (3760 files, 3804 with
revisions counted separately): a test is either run, once per revision, or left out with a
reason that is counted. A test that runs becomes a cargo package whose library is the test
and whose binary calls its `main`. `aux-build`, `aux-crate` and `proc-macro` files become
packages it depends on; files named by `mod`, `#[path]` and `include!` are copied beside it;
`exec-env`, `run-flags` and `edition` are honoured. rustc builds the package, blaze rebuilds
the library's functions and they are linked in front of rustc's, and the two programs must
exit with the same status and print the same on stdout and stderr.

Tests are built a workspace at a time, several workspaces at once (`--jobs`). Each
workspace's result is kept under `<work>/results`, keyed by its tests, the blaze binary and
the backend, so a stopped run picks up where it was. A whole run takes about three minutes
on this machine.

Three things the harness does so that a difference means something:

- It relinks rustc's own objects with the link line it keeps and runs that as well. One test
  (`attributes/used_with_archive.rs`) differs there already, because the kept link line lacks
  rustc's `symbols.o`; it is left out with that reason.
- It runs rustc's program twice. If the two runs differ, the test is left out as "prints
  something else in every run" (addresses, hash order, thread interleaving). This is 8 to 10
  tests depending on the run.
- When blaze's program differs, it is run six more times, and rustc's program is run over
  and over for 20 seconds to see whether it ever does what blaze's did. If it does, the test
  races and is left out with the reason "races"; if blaze's program differs once and never
  again, it is counted as "differing in one run only" and fails the run. This came from
  `threads-sendsync/yield1.rs` and `panics/panic-handler-flail-wildly.rs`, which each differed
  in one sweep and not in the others. For `yield1.rs` I saw rustc's own binary print `2` before `1` twice in
  3000 runs, but that count was taken through a pipe that closed early and on a loaded
  machine, so take it as "it happens", not as a rate. In the final runs neither test differed.

### What was left out (round one)

The current counts are in "Round two", item 3, and at the end of the harness's output. From the final Cranelift run of round one (`ui-cl8b`); the LLVM run differs only in the count of tests
that print something else in every run.

| count | reason |
|---|---|
| 703 | needs a nightly feature (`#![feature]`) |
| 76 | compile flag `-Z` (nightly only) |
| 73 | no `fn main()` written at the top level (it comes from a macro or is not there) |
| 48 | an auxiliary crate needs a nightly feature |
| 47 | compile flag sets an optimisation level |
| 18 | rustc does not build it as a package |
| 12 | an auxiliary crate has compile flags of its own |
| 10 | `needs-sanitizer` |
| 9 | fails as rustc builds it in a package |
| 8 | prints something else in every run |
| 8 | `#![no_std]` |
| 8 | compile flag asks for LTO |
| 7 | names a module file that is not there |
| 7 | directive `rustc-env` |
| 6 | compile flag `--extern` |
| 6 | compile flag `--test` |
| 5 | `#![no_main]` |
| 5 | compile flag turns debug assertions or overflow checks off |
| 4 | names a file that is not there |
| 29 | not for this target (`only-windows` 7, `only-aarch64` 5, `only-32bit` 5, `only-x86` 3, `only-msvc` 2, `ignore-test` 2, five others once each) |
| 22 | other compile flags and directives, one to three tests each (`-Cllvm-args`, `-Cno-prepopulate-passes`, `-C panic=abort`, `pp-exact`, `aux-bin`, `--crate-name`, target features, `-Ccontrol-flow-guard`, `unset-rustc-env`, `--env-set`, `-Clink-args`, `--remap-path-scope`, `-lstatic`, `unused-revision-names`) |
| 1 | two auxiliary crates of one name |
| 1 | `#![crate_type]` |
| 1 | differs when relinked from rustc's objects alone |

The full list with every count is at the end of the harness's output. Some of these are the
harness's limits and not blaze's: the 73 without a top-level `main`, the 18 + 9 that do not
survive being turned into a package, and the 12 auxiliary crates with their own flags could
be run with more work on the harness. The optimisation, LTO and assertion flags (60 tests)
are left out because the harness compares against a debug build; nothing was measured about
them.

### Functions left to rustc (round one)

The current list is in "Round two", item 1. Round one: 57 functions, the same on both backends. The harness prints them under "functions left to
rustc by reason".

| functions | reason blaze gives | tests |
|---|---|---|
| 13 | a local that cannot be reached in an `async fn` taking `ref` pattern parameters (9), and the functions that share an `async` block with those (4) | `async-await/drop-order/drop-order-for-async-fn-parameters-by-ref-binding.rs` |
| 7 | pattern matching on a union | `union/*` (4 tests), `structs-enums/align-struct.rs` |
| 4 | upcast of a `dyn` type with bound regions | `traits/trait-upcasting/*` (3), `regions/regions-early-bound-trait-param.rs` |
| 4 | calling convention `win64` on Linux | 3 tests |
| 3 | inline assembly with a `sym` operand | 3 tests |
| 1 | an `asm!` constant that is not a literal | `asm/x86_64/const.rs` |
| 3 | a body with type errors rustc does not have | `binding/pat-ranges.rs`, `coercion/trait-object-arrays-11205.rs`, `parser/macro/statement-boundaries.rs` |
| 2 | incomplete pattern (range patterns over associated constants) | 2 tests |
| 2 | incomplete expression (reserved prefixes through a macro) | 2 tests |
| 2 | `%` on floats in a constant | `consts/const_let_eq_float.rs`, `numbers-arithmetic/apfloat-modulo-wrong.rs` |
| 1 | `type_id` of a type with lifetimes in a constant | `consts/const-typeid-of-rpass.rs` |
| 7 | a name or method that does not resolve (four of them behind a proc macro) | 7 tests |
| 3 | a constant or type that fails when the function is instantiated | `const-generics/min_const_generics/macro.rs`, `impl-trait/closure-in-impl-trait*.rs` |
| 1 | a `Self` constructor in pattern position | `rfcs/rfc-2302-self-struct-ctor` |
| 2 | a static with no symbol in rustc's object (extern static, writable `CACHE`) | 2 tests |
| 1 | call to `partial_cmp` with no body and no rustc symbol | `where-clauses/obligation-error-propagation.rs` |
| 1 | missing function definition under `#[cfg]` from an attribute macro | `proc-macro/attr-cfg.rs` |

Every one of these tests still matches: the function is refused with the reason above and
rustc's copy runs. I did not reduce these to breaker cases or work on them; the differing
tests and the library suites took the time. They are the next worklist, largest first.

## Part 2: the library suites

    python3 harness/libsuite.py <rust> coretests  --work <dir> -- --json core.json
    python3 harness/libsuite.py <rust> alloctests --work <dir> -- --json alloc.json
    python3 harness/libsuite.py <rust> stdtests   --work <dir> -- --json std.json

`coretests`, `alloctests` and `library/std/tests` are written against the unstable interface
of the library: each begins with a list of `#![feature]`s that a stable compiler turns down.
`harness/libsuite.py` (new) cuts a suite down to what stable builds. It removes the feature
lines and then, until `cargo check` is quiet, deletes every item an error points into: the
test function when the error is in one, otherwise the `use`, helper or macro, whose users go
in the next round. Nothing is rewritten, so a test either runs as written or is not there.
What is deleted is counted by error code in `<work>/<suite>/pruned.json`. The package that is
left goes to `harness/cratetest.py`, which builds each test binary with rustc and again with
blaze's objects in front, runs both and compares every test by name.

| suite | `#[test]` functions in the source | build on stable | deleted, by the error that deleted the item |
|---|---|---|---|
| `coretests` | 1443 | 937 | E0658 (unstable) 622, syntax 72, unresolved after earlier deletions 92, others 3 |
| `alloctests` | 951 | 643 | E0658 252, unresolved 190, others 14 |
| std (`library/std/tests`) | 421 | 259 | E0658 199, unresolved 27, E0554 1 |

The deletion counts are items, not tests, so they do not sum to the difference. "Tests run"
in the summary is what libtest reports, which is more than the `#[test]` count for `coretests`
because macros there write many tests each.

Six `coretests` tests are left out because rustc's own unoptimised binary does not finish
them in 60 seconds (`slice::split_off_*_max_range_*`).

std's unit tests (the `tests.rs` files under `library/std/src`) are not run. They are
compiled as part of `std` itself, which cannot be built on stable. The same holds for the
unit tests inside `core` and `alloc`.

`cratetest.py` used to take a package's test targets from the top of `tests/`; it now takes
their names and paths from `cargo metadata`, which is how std went from 15 binaries to 28.

Functions left to rustc in round one, the same on both backends (now 5, 0 and 1; see "Round two", item 1):

| suite | functions | reason | reduced case |
|---|---|---|---|
| `coretests` | 5 | `Location::caller()` in a constant: blaze does not know what rustc names the file there, so it refuses | `caller_in_constant` |
| `coretests` | 2 | `type_name` of a function item and of a `dyn` type with a binder | none |
| `coretests` | 1 | layout of `PinMacroHelper<[PhantomPinned]>` (an unsized coercion inside `pin!`) | none |
| `alloctests` | 2 | an empty array compared with a slice or `VecDeque`: inference leaves the element type unknown, so there is no layout | `empty_array_by_impl` |
| std | 1 | `type_name` of a `dyn` type with a binder | none |

## What was fixed

Each row was found by a differing or crashing test, reduced to a case under
`harness/breakers/` and fixed in blaze. `python3 harness/breakers.py` runs them all.

| cause | found by | reduced case |
|---|---|---|
| A `[x; 0]` repeat did not drop `x`, and a repeated constant was treated as moved | `drop/repeat-drop.rs` | `repeat_moved` |
| Temporaries of a `let ... else` were dropped after the `else` block ran | `let-else/let-else-drop-order.rs` | `let_else_drops` |
| Before 2024, the temporaries of an `if let` scrutinee live to the end of the whole `if`; nested ones leaked | `drop/drop-order-comparisons.rs` (2021) | `if_let_outlives`, `harness/breakers/edition2021` |
| An array moved out of by a guard and given back was dropped in the order of a partly moved one | `drop/if-let-guards.rs` (2021, 2024) | `guard_let_drops` |
| Unwinding dropped named locals in the wrong order | same sweep | `tail_tuple_drops`, `repeat_moved` |
| A closure capturing a `#[non_exhaustive]` enum of its own crate captured too little | `closures/2229_closure_analysis/match/partial-move-drop-order.rs` | `non_exhaustive_capture` |
| A tuple-variant constructor cast to a pointer was taken for a discriminant cast | `mir/mir_refs_correct.rs` | `constructor_as_pointer` |
| `for<'a>` on a function pointer type was bound after its parameters, so two types had one `TypeId` | `unboxed-closures/type-id-higher-rank.rs` | `nested_binder_type_id` |
| A proc macro was handed `-1` as one literal token | `proc-macro/negative-token.rs` | `negative_literal_token`, `harness/breakers/literals` |
| An `extern` block function of a dependency was called by its Rust name, not its `link_name` | 2 link errors | `foreign_of_dependency`, `harness/breakers/foreign` |
| The methods of a `#[global_allocator]` were not entry points, so a thread-local used inside one was rustc's copy | `threads-sendsync/tls-in-global-alloc.rs` | `allocator_static` (see below) |
| Constant evaluation walked every element of `[(); usize::MAX]` | `coretests` did not compile | `repeat_moved` |
| A repeat of a zero-sized value looped once per element at run time | `coretests` `slice::test_array_windows_count` hung | `repeat_moved` |
| A reference to an empty slice at a made-up address lost the address in a constant | `coretests` | `numbered_empty_slice` |
| A constant's allocation lost its alignment, so `#[repr(align(64))]` data could land unaligned | `coretests` | `aligned_constant_field` |
| Two uses of one constant string had two addresses | `alloctests` `str::const_str_ptr` | `shared_constant_address` |
| A trait alias got an implied `Self: Alias` clause, so `<*const T as Default>::default` did not resolve through `Thin` | `coretests`, 2 functions | `null_by_default` |
| `-4` matched as `$x:literal` and followed by `.pow(2)` came out as `-(4.pow(2))` | std `num` tests, a wrong result | `negative_literal_operand`, unit test `negative_literal_stays_one_operand` |
| `&(3 + N)` with a const parameter was a stack temporary, not promoted data | `const-generics/promotion.rs` (LLVM showed it) | `promoted_const_parameter` |
| A `VaList` passed to a C function was copied by value | `abi/variadic-ffi.rs` crashed (LLVM showed it) | `list_by_address` |

`allocator_static` does not reproduce the allocator bug in the `cases` crate, because that
crate has no `main` and so every function in it is already an entry point. The fix is
covered by the UI test only.

The last two rows showed only under LLVM, but both were wrong on both backends: Cranelift
happened to leave the stack bytes alone.

### rustc sources behind the analysis changes

`AI_POLICY.md` asks for the rustc source behind changes to analysis crates. Paths are in
rust-lang/rust at `1.99.0`.

- Trait aliases have no implied `Self: Trait` clause (`crates/hir-ty/src/lower.rs`).
  `compiler/rustc_hir_analysis/src/collect/clauses_of.rs:48` adds the clause only
  `if tcx.is_trait(def_id)`, and `compiler/rustc_middle/src/ty/util.rs:635` defines `is_trait`
  as `DefKind::Trait`, which a trait alias is not.
- A negative literal is one `literal` fragment and stays one operand
  (`crates/mbe/src/expander/matcher.rs`, `transcriber.rs`).
  `compiler/rustc_parse/src/parser/nonterminal.rs:166` parses `:literal` with
  `parse_literal_maybe_minus`, and `compiler/rustc_expand/src/mbe/transcribe.rs:541` puts it
  back inside an invisible `MetaVarKind::Literal` group.
- `VaList` goes by address in C calling conventions (`crates/hir-def/src/attrs.rs`,
  `crates/hir-ty/src/mir/codegen.rs`). `core` marks the type
  `#[rustc_pass_indirectly_in_non_rustic_abis]`;
  `compiler/rustc_abi/src/layout/ty.rs:244-264` describes the flag and follows transparent
  wrappers, and `compiler/rustc_target/src/callconv/x86_64.rs`, `aarch64.rs` and
  `x86_win64.rs` make such an argument indirect. blaze now does the same for SysV x86-64,
  AAPCS64 and Windows x64. Only the first was run here.
- A const parameter in a promotable expression (`crates/hir-ty/src/mir/lower.rs`). This
  follows what `tests/ui/const-generics/promotion.rs` requires; I did not trace it to a line
  in `rustc_const_eval`'s promotion pass.

The drop-order changes follow what the named UI tests print, not a reading of rustc's drop
elaboration.

## Still differing

Four tests from round one, on both backends; the ones round two added are under "Round two:
differing". Each of the four is marked open in `harness/breakers.py`.

| test | reduced case | cause |
|---|---|---|
| `async-await/async-fn-size-moved-locals.rs`, `async-fn-size-uninit-locals.rs` | `future_unwind_flags` | The tests assert the size of a future. rustc shares the unwind cleanup of a local between two calls and keeps a drop flag for it, which becomes a byte of the future. blaze drops by where the call is and keeps no flag, so its future is smaller. The program is otherwise right; matching it means reproducing rustc's coroutine layout decisions. |
| `drop/issue-90752-raw-ptr-shenanigans.rs` | `raw_write_then_match` | A local known to be `None` is written through a raw pointer and then partly moved out of. rustc's drop elaboration does not see the write and leaks the rest; the test pins that. blaze drops it. |
| `macros/metavar_cross_edition_recursive_macros.rs` | `fragment_edition_through_macro` | The test is marked `known-bug` for rust-lang/rust#130484. rustc reads an `expr` fragment by the edition of the macro that wrote the name; blaze reads it by the edition where it was written, which is what the test says rustc will do when the bug is fixed. |

In all three blaze is not doing what rustc 1.99 does. In the last two, what rustc does is
something the Rust project calls a bug or an accident; I left blaze as it is and say so
here instead of copying it. The first is a real gap if anything depends on the exact size
of a future.

## Regression gate

Run from `harness/` with `BLAZE_BIN` at each binary, once per backend. All passed.

| command | Cranelift | LLVM |
|---|---|---|
| `difftest.py corpus --bin runner --crate cases --layouts` | MATCH, 0 of 893 layouts differ | MATCH, 0 of 893 layouts differ |
| `programs.py` | 22 of 22 | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts | 99 of 99, 0 restarts |
| `breakers.py` | 43 of 43, 5 open | 43 of 43, 5 open |
| `unwinds.py` | 8 of 8 | 8 of 8 |
| `splices.py` | 71 of 71 | 71 of 71 |
| `asmedits.py` | 10 of 10 | 10 of 10 |
| `includes.py` | 9 of 9 | 9 of 9 |
| `worktrees.py` | 58 of 58 | 58 of 58 |

`cargo fmt --all -- --check` is clean. `cargo test --release -p hir-ty -p hir-def -p parser
-p syntax -p blaze-engine`: 913 passed, 0 failed.

## What is open

- The four differing tests above.
- The 57 + 11 functions left to rustc. None is wrong, all are refused with a reason, and
  only the three library reasons with a case have been reduced.
- 1114 `tests/ui` run-pass tests are not run. About 830 need nightly. Roughly 110 are the
  harness's own limits (no top-level `main`, does not build as a package, auxiliary crates
  with flags), and 60 need a release-mode comparison the harness does not do.
- The unit tests inside `core`, `alloc` and `std` are not run, because those crates cannot be
  built on stable.
- Only Linux x86-64 was run. The `VaList` change touches the AArch64 and Windows argument
  code, which was not exercised.
- Tests that race are left out, not judged. The 20-second check can miss a race that rustc's
  program shows less often than that, in which case the test is reported as differing, which
  is the safe side.
