-- Hand-written, not generated: the proof of the `atomics` fixture's theorem.
import BlazeModel.atomics.two_adds_make_two

/-!
The first theorem over shared memory proved (crates/hir-ty-lean/src/tests.rs, `ATOMICS`;
docs/v2/plan/design/proofs-for-sans-io.md, section 8). A new `AtomicU32` given two
`fetch_add(1)`s loads 2: the model, a `Threads.Prog` of four operations on the memory, run
alone from an empty memory (`Threads.Prog.value`), gives `true`. It is closed, so the kernel
decides it by evaluation.
-/

open Blaze

theorem atomics.two_adds_make_two.spec.proof : atomics.two_adds_make_two.spec.statement := by
  rfl
