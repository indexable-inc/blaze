---
status: plan
summary: Lint `blaze::vecreport` - report what LLVM's optimized code still does in a loop (a bounds, overflow or panic check that survived, a loop left scalar and why) at the source line, read from the IR after every pass and the loop vectorizer's remarks, hot loops only under a profile; prior art, the design, the guarantee it rests on. Built, not wired: the IR walk in vendor/llvm-sys's C++ wrapper and the lint's id; nothing in Rust calls the walk.
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
- In blaze: the lint ids `blaze::checkfree` (a marked function holds no check) and
  `blaze::profile_hot` (function-level heat from the lint profile) are declared in
  blaze-lint-defs.

What is new here: one report of both kinds of finding, read from the IR LLVM is about to emit,
placed by blaze's own statement lines, filtered by the profile's loop counts, and kept with the
function's code in the store, so a cached function reports as a compiled one does.

## Design

**What is read.** After the optimizing tier's LLVM pipeline (D97), before any sanitizer, the
wrapper (`vendor/llvm-sys/wrappers/passes.cpp`, `LLVMBlazeReport*`) walks the function:

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

**Where.** Each instruction's line is the statement index plus one, in a file named
after the function. Inlined code is placed at its outermost `inlinedAt`, the function's own
statement. Report mode forces these lines into the IR on every format (Mach-O too) and marks
the function; variables stay off unless debug info is asked for. When the
object should not carry the lines (no debug info, or Mach-O, whose line table blaze doesn't
read back), they are stripped after the walk and before the object is written.

**Not built: the Rust side.** Reading the wrapper's rows, mapping each line to the function's
statement and span and each `blaze.f.N` callee to its instance (a line or callee nothing places
is an error naming the function, never a dropped row), keeping the sorted rows with the
function's code in the store (so a function the store had reports with no compile), putting the
report flag in the function's key only when on, and printing: rows of workspace crates
resolved to `file:line:col`, classified by callee (bounds, overflow, division, `unwrap`, panic),
filtered by heat, sorted, each printed once as a warning.

## The guarantee it rests on

LLVM's rule that debug information and remarks don't change code: a remark is made after the
decision it describes and only an observer receives it, and debug intrinsics and locations are
ignored by every transform's cost model (LLVM's `-g`-invariance testing, `llvm/utils/check-
debugify`). The test owed is that the object is the same bytes with and without the report on a vectorized
loop. Even so the flag is in the key, so a
report build and a plain one never share an entry by assumption.

## Profile gating

Without a profile every finding is reported. With one, a finding is reported only if its
loop's header count is hot by the profile summary; a loop no count reaches (unrolled away,
or never counted) is not. Function-level heat (the lint profile of `blaze::profile_hot`) is a
coarser filter the profile's loop counts make unnecessary here.

## Left for later

- A diagnostic with a real span (an editor's squiggle) rather than a `file:line:col` prefix.
- `#![warn(blaze::vecreport)]` in the source: the flag is in the build's keys, which are fixed
  before the source is read.
- `blaze::checkfree`'s optimized-code check can read these rows instead of walking again.
