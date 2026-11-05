---
status: plan
summary: Lint `blaze::vecreport` - report what LLVM's optimized code still does in a loop (a bounds, overflow or panic check that survived, a loop left scalar and why) at the source line, read from the IR after every pass and the loop vectorizer's remarks, hot loops only under a profile; prior art, the design, the guarantee it rests on, and what the stencil tier can say.
date: 2026-11-02
---
# vecreport: what the optimized code still does

`blaze::bounds_check_in_loop` guesses from MIR whether an index's check runs every iteration.
This lint reads what codegen kept: a release (LLVM) build made for it tells, for each function
of the workspace, which loops still branch to a panic path once every pass has run, and which
loops the vectorizer left scalar with its reason. The output is a warning at the source line, in
a deterministic order, never an IR dump. With a profile, only hot loops are told.

## Prior art

- **Go's `-gcflags=-d=ssa/check_bce`** prints `file:line:col: Found IsInBounds` for every
  bounds check that survived the compiler's BCE pass, and `go/test/checkbce.go` pins them as
  `// ERROR` comments. Closest in spirit: the compiler reports its own surviving checks at the
  source line. No loop or heat filter, no vectorizer. <https://go.dev/test/checkbce.go>,
  <https://www.go101.org/optimizations/5-bce.html>.
- **LLVM's optimization remarks**: `-Rpass-missed=loop-vectorize` and
  `-Rpass-analysis=loop-vectorize` in clang, `-C remark=loop-vectorize` in rustc, print
  "loop not vectorized" with the reason. rustc's `-Zremark-dir` (rust-lang/rust#113040, Kobzol)
  writes them as YAML. They say nothing of checks kept, print every crate's (std's too) and
  print once per codegen unit with rustc's own lines. Hotness needs
  `-fdiagnostics-show-hotness`, which runs BFI for every remark.
- **opt-viewer** (llvm/tools/opt-viewer) and **optview2** (OfekShilon/optview2, 153 stars):
  HTML of the YAML remarks over the source. **cargo-remark** (Kobzol/cargo-remark, 209 stars):
  the same for a cargo build, filtered to the workspace. Viewers of remarks, a step after this
  lint's input; none reads surviving checks.
- **Compiler Explorer**'s opt-remarks pane: remarks next to the source, one function at a time.
- **gatlinnewhouse/bounds_analyzer**: searches a compiled binary for calls of
  `panic_bounds_check`, by address, not by loop or line.
- In blaze: `blaze::checkfree` (docs/lints.md#checkfree; lane `v2-checkfree`) holds a marked
  function to no check by a MIR scan (`hir_ty_mir::mir::checks`), with the LLVM tier's own
  check planned; `blaze::profile_hot` and the lint profile (`clippy.toml`'s `profile`,
  `blaze-lints/src/profile.rs`) give function-level heat from a profile.

What is new here: one report of both kinds of finding, read from the IR LLVM is about to emit,
placed by blaze's own statement lines, filtered by the profile's loop counts, and kept with the
function's code in the store, so a cached function reports as a compiled one does.

## Design

**What is read.** In `llvm::release::compile`, after the post-link pipeline (and the rebase
and `elim-avail-extern` runs), before the sanitizers, the wrapper
(`vendor/llvm-sys/wrappers/passes.cpp`) walks the function:

- *Kept checks*: `LoopInfo` over the function; for each block of a loop, each successor
  outside the loop whose path (following single successors, fuel 16) ends in `unreachable` (or
  an `invoke` whose normal destination is unreachable) is a kept check, placed at the first
  call on that path. Blaze lowers a panic as `call @blaze.f.N(..)`, `llvm.trap`, `unreachable`,
  so the call names the panic function. Rows are deduplicated by exit block and loop.
- *Vectorizer remarks*: a `DiagnosticHandler` wrapping the passes' own catches
  `DiagnosticInfoIROptimization` of pass `loop-vectorize`, Missed and Analysis (with
  `FPCommute` and `Aliasing`). Analyses come before the Missed remark they explain; they are
  tied to its loop.
- *Heat*: with a profile, `BranchProbabilityInfo`, `BlockFrequencyInfo` and
  `ProfileSummaryInfo` are computed once after the passes; a loop's count is its header's,
  hot by `isHotCount`. Remarks get their loop's count by its line. Computing BFI in the walk,
  not by asking remarks for hotness, keeps the passes' analysis cache as it is.

**Where.** Each instruction's line is the statement index plus one (`LineRow`), in a file named
after the function. Inlined code is placed at its outermost `inlinedAt`, the function's own
statement. Report mode forces these lines into the IR on every format (Mach-O too) and marks
the function (`Defined::marked`); variables stay off unless debug info is asked for. When the
object should not carry the lines (no debug info, or Mach-O, whose line table blaze doesn't
read back), they are stripped after the walk and before the object is written.

**Kept with the code.** `v2::release::emit` maps each row: line to statement to the function's
`SpanRef`, a `blaze.f.N` callee to its request's `Target` (`Function`/`Direct` to the instance,
`Extern`/`Weak` to the symbol). A line past the statements or a callee nothing places is an
error naming the function, never a dropped row. Interleaving remarks are dropped. Rows are
sorted and kept in the function's side table as a trailing postcard section with its own
version (`v2::report`); a side table without one is byte-identical, and a function with rows
and no table gets an empty table first. The store keeps it under `keys::side`, so a function
the store had reports with no compile.

**Keys.** `Spec::report` is hashed into `Settings` only when on, as a suffix, so every build
without it keeps its keys; the report's code may in principle differ (lines in the IR), and its
side table does. `Policy::report` sets it for LLVM units only.

**Printing.** blaze-cargo turns it on from the unit's rustflags (`-W`/`-D`/`-F`/`--force-warn`
`blaze::vecreport` or `blaze::pedantic_perf`, the last flag winning). At the link phase,
`Generated::report` reads the side table of each function written in a workspace crate
(`Built::editable`; not `Owner`, which says what a dependency image holds) from
the build's puts or the store, and `HirFrontend::vecreport` resolves each `SpanRef` through
the build's span tables to `file:line:col`, classifies the callee by its debug name (bounds,
overflow, division, `unwrap`, panic), drops rows not hot under a profile, sorts by position and
message and prints each once as a warning.

## The guarantee it rests on

LLVM's rule that debug information and remarks don't change code: a remark is made after the
decision it describes and only an observer receives it, and debug intrinsics and locations are
ignored by every transform's cost model (LLVM's `-g`-invariance testing, `llvm/utils/check-
debugify`). `asking_for_the_report_changes_no_code` holds LLVM to it on a vectorized loop:
the object is the same bytes with and without the report. Even so the flag is in the key, so a
report build and a plain one never share an entry by assumption.

## The stencil tier

A stencil keeps every check its MIR has (no pass proves a range) and vectorizes no loop. The
only true report there is the MIR's own check list, which `blaze::checkfree`'s scan
(`hir_ty_mir::mir::checks`) already makes; `vecreport` reports nothing for stencil or Cranelift
code rather than repeat it. Dev builds are told so in docs/lints.md#vecreport.

## Profile gating

Without a profile every finding is reported. With one, a finding is reported only if its
loop's header count is hot by the profile summary; a loop no count reaches (unrolled away,
or never counted) is not. Function-level heat (the lint profile of `blaze::profile_hot`) is a
coarser filter the profile's loop counts make unnecessary here.

## Left for later

- A diagnostic with a real span (an editor's squiggle) rather than a `file:line:col` prefix.
- `#![warn(blaze::vecreport)]` in the source: the flag is in the build's keys, which are fixed
  before the source is read.
- The checkfree lane's LLVM-tier check can read these rows instead of walking again.
