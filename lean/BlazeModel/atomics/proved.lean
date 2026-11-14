-- Hand-written, not generated: the proofs of the `atomics` fixture's theorems.
import BlazeModel.atomics.two_adds_make_two
import BlazeModel.atomics.adds_until_done
import BlazeModel.atomics.pokes_count

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

/-- A compare-exchange loop that returns the value it saw from inside its `Ok` arm (`Ok(old) =>
return old`): from 5, adding 2 returns 5 and leaves 7. Run alone the first exchange succeeds,
so two rounds of fuel are enough. -/
theorem atomics.adds_until_done.spec.proof : atomics.adds_until_done.spec.statement :=
  ⟨2, by rfl⟩

/-- A trait's provided bodies (`prepare`, `poke`) at the implementor `Real`, `poke` calling
`Real`'s own `kind`: two pokes of a zeroed word return 1 from the second and leave 2. -/
theorem atomics.pokes_count.spec.proof : atomics.pokes_count.spec.statement := by
  rfl
