---
status: current
---

# The `blaze run` ladder

A walking skeleton: `blaze run <dir>` runs a program end to end on the new layer, one rung at a
time (docs/v2/plan/design/bottom.md section 16). Each rung is a project directory here with its
source and an `expected` file; a rung is done when
`bash /Volumes/Projects/blaze-int-1112-notes/ladderrun.sh <rung dir> run .` prints what `expected`
says.

`expected`: line 1 is `status <n>`, the process's exit status; line 2 is `stdout`; every byte
after line 2 is the program's standard output, exactly.

| rung | what | status |
|---|---|---|
| 1-empty | `fn main() {}`: exit 0 | runs (2026-11-15) |
| 2-exit | `#![no_core]`, `extern "C" fn main() -> i32`: arithmetic, `let`, `if`, `while`, a call: exit 42 | runs (2026-11-15) |
| 3-hello | `#![no_core]`, `extern "C" { fn write(..) }`, a byte string cast to a pointer: prints `hello` | runs (2026-11-15) |
| 4-modules | `mod util;` in a second file, a struct with fields, a `match` on an integer | runs (2026-11-15) |

Next, not this ladder's yet: the real `println!` hello world. It needs `core`, `alloc` and `std`
lowered, checked and built through the new layer: blaze-lower for their bodies and signatures,
blaze-ty's real types and blaze-solve's trait solving (formatting is `fmt::Arguments` over
`dyn Write`, generics, closures, traits with associated types), monomorphization (an instance's
LIR per substitution), floats and aggregates in the backend, statics and thread-locals in
placement, unwinding, and `lang_start`.

## The route (decided 2026-11-15, 22:00)

- **(a) Source to a typed body.** crates/blaze-lower was being written and did not compile, so
  `blaze-ladder::lower` lowers the rungs' subset straight from the parser's events (the way
  blaze-file-items' walk reads them) into blaze-typeck's body form (`BodyView`), and
  `blaze-ladder::world` answers the checker's `World` over the lowered program. Both go when
  blaze-lower and the item layer's `World` answer: one home, blaze-lower's. The checker is
  blaze-typeck's own `check`, over its toy type context `blaze_typeck::toy::ToyCtx`, made public
  under feature `toy` for this until blaze-ty implements `TypeCtx`.
- **(b) Typed body to LIR.** crates/blaze-build: `BodyView` + `Checked` + the owner's `FnSig` to
  one LIR function, explicit stacks, every local in a stack slot, verified by blaze-lir's
  verifier; tested against blaze-lir's evaluator.
- **(c) LIR to machine code.** crates/blaze-emit, the baseline tier: one pass, no optimization,
  every value in a 16-byte frame cell, scratch registers, AAPCS64 (Apple's variant) for calls in
  and out, calls of program functions through the launcher's slot words, outside symbols through
  a literal word. Un-parking blaze-stencil was not taken: it is built on blaze-codegen's
  `CompiledFunction` and `InstanceKey`, and blaze-codegen on blaze-api, blaze-query (deleted)
  and blaze-items (docs/v2/plan/parked.md, and the manifests), so bringing it back starts with
  a repair of crates unrelated to LIR. blaze-emit does not replace the stencil backend: it is the first thing
  that runs, the tier the stencils stand in for once they read LIR (backend-next.md).
- **(d) Placing and running.** blaze-launch-rt's `Driver`: one slot a function with a body, all
  bound in one edit, called through the slot's word; outside symbols (`write`) by `dlsym` of
  `RTLD_DEFAULT` in `blaze-ladder::place` (blaze-launch-rt's own `dlsym` is private to its
  `Loaded` records). `main` runs on the `blaze` process's own thread.
- **(e) The command.** crates/blaze-cli's `run` performs the request machine's
  `Effect::StartProgram` by handing the package's `src/**.rs` (read through bstd) to
  `blaze_ladder::run::run`, steps `Event::Exited`, and exits with `main`'s status.

## Where the skeleton shortcuts the design (a list that shrinks)

1. The pipeline is a straight line in one command function (`blaze_ladder::run::run`), not tasks
   the request machine asks for, with no records, keys or `Need`.
2. Its own lowering and `World` (`blaze-ladder::lower`, `world`) instead of blaze-lower and the
   item layer's records; item keys are hashes of module path, kind and name.
3. The toy type context (`ToyCtx`) instead of blaze-ty and blaze-solve.
4. `main` runs inside the `blaze` process, on its thread, not in a child launched by
   blaze-run's `Parent`; a crash of the program is a crash of blaze.
5. Every function is compiled and bound before `main` runs: no lazy stubs, no tiers.
6. Outside symbols resolve by a direct `dlsym` in blaze-ladder, not by blaze-launch-rt's `Loaded`
   records; code is placed from an in-memory table, not from code records in the store.
7. `fn main() {}` runs without `std`'s `lang_start`: no runtime set-up, no `std` at all.
8. Struct layout is declaration order with natural alignment (blaze-build), not the layout
   query's.
9. Unsafety is not checked: calling an `unsafe fn` outside `unsafe` is not refused.
10. Rust-ABI functions use the C convention over their lowered signature (blaze-lir's own rule),
    with only integer and pointer parameters.
11. Rungs 2 and 4 use integer operators under `#![no_core]` without declaring the operator lang
    items (`add`, `partial_ord`, ...); blaze-typeck types them by its built-in rule when the
    trait is absent (rules/op.rs). Whether nightly takes these programs as written is not
    checked here (nothing may run rustc); if it does not, the rungs gain mini-core's operator
    traits and impls once the lowering takes traits and impls.

## Log

One line per rung run: date, rung, command, output, exit status.

- 2026-11-15, 1-empty: `ladderrun.sh crates/blaze-cli/tests/ladder run 1-empty` printed `blaze: 2 runs, 3 store reads (0 found), certificate missed`, no stdout; `ladderrun: exit status 0`. Again as `run ./1-empty/`: `0 runs, 1 store reads (1 found), certificate stood`; exit status 0.
- 2026-11-15, 2-exit: `ladderrun.sh crates/blaze-cli/tests/ladder run 2-exit`: `blaze: 2 runs, 3 store reads (0 found), certificate missed; 59.565 ms in main`, no stdout; `ladderrun: exit status 42`.
- 2026-11-15, 3-hello: `... run 3-hello`: stdout `hello`; `45.141 ms in main`; `ladderrun: exit status 0`.
- 2026-11-15, 4-modules: `... run 4-modules`: `blaze: 3 runs, 4 store reads (0 found)`, no stdout; `ladderrun: exit status 42`.
