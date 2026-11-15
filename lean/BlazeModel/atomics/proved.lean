-- Hand-written, not generated: the proofs of the `atomics` fixture's theorems.
import BlazeModel.atomics.two_adds_make_two
import BlazeModel.atomics.adds_until_done
import BlazeModel.atomics.pokes_count
import BlazeModel.atomics.weak_message_is_passed

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

/-! Message passing in the weak world (`threads::two_weak`, decision D46): the writer stores 42
to `data` (`Relaxed`), then 1 to `flag` (`Release`); the reader loads `flag` (`Acquire`) and,
where it reads 1, loads `data` (`Relaxed`). The statement says the reader never sees anything
but 42, on every schedule, with every stale read the orderings allow. What is proved here is
that statement on fixed schedules, by evaluation: each gives the data load, or the flag load, a
pick that asks for a stale message. The theorem for every schedule waits for the weak world's
induction over a schedule, `Blaze.ThreadsWeakAuto`. -/

namespace atomics.weak_message_is_passed.runs

/-- The writer (thread 1) goes, with the world's pick. -/
def w (pick : Nat := 0) : Threads.Step := ⟨false, Scalar.lit .U8 pick⟩
/-- The reader (thread 2) goes, with the world's pick. -/
def r (pick : Nat := 0) : Threads.Step := ⟨true, Scalar.lit .U8 pick⟩

-- Both stores, then the reader reads the flag's 1 and asks for the data's stale 0 (pick 1):
-- the acquire brought the 42 into its view, so the 0 is behind it and it reads 42.
example : (atomics.weak_message_is_passed ⟨[w, w, r, r 1], by decide⟩).valueWeak = .ok true := by
  rfl
-- The same, asking for a message further back than there is.
example : (atomics.weak_message_is_passed ⟨[w, w, r, r 3], by decide⟩).valueWeak = .ok true := by
  rfl
-- Both stores, then the reader asks for the flag's stale 0 (pick 1): it reads 0 and returns 42
-- without loading the data.
example : (atomics.weak_message_is_passed ⟨[w, w, r 1, r 1], by decide⟩).valueWeak = .ok true := by
  rfl
-- The reader reads the flag between the writer's stores: 0, so 42 without loading the data.
example : (atomics.weak_message_is_passed ⟨[w, r, w, r 1], by decide⟩).valueWeak = .ok true := by
  rfl

end atomics.weak_message_is_passed.runs
