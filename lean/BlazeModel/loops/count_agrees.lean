import Blaze
import Blaze.Mir.Walk
import BlazeModel.loops.count
import BlazeMir.loops.count

/-!
`loops::count`: the functional model (`BlazeModel.loops.count`) agrees with the semantics of the
function's MIR (`BlazeMir.loops.count`), for every argument: objective V7
(docs/v2/plan/design/formal-verification.md). `mir_walk` (`Blaze.Mir.Walk`) runs a turn of the
loop (two blocks) and the way out (three), with the comparison and the increment passed in;
`run_loop` does the rest. No state between two blocks is written down: a change of the
body's temporaries or storage markers changes only `Temps` and the step counts.
-/

open Blaze Blaze.Mir

namespace BlazeAgrees.loops.count

set_option maxRecDepth 8192

abbrev P : Program := loops.count.mir

/-- What locals 3 to 9 (the temporaries) hold. No turn of the loop reads one it did not
write first. -/
structure Temps where
  (s3 s4 s5 s6 s7 s8 s9 : Slot)

/-- No temporary holds anything. -/
def Temps.none : Temps :=
  ⟨Slot.uninit, Slot.uninit, Slot.uninit, Slot.uninit, Slot.uninit, Slot.uninit, Slot.uninit⟩

/-- The one frame: the block to run, local 0, `n` (local 1), the counter's slot (local 2)
and the temporaries. -/
def frame (block : BlockId) (s0 : Slot) (n : U32) (s2 : Slot) (t : Temps) : State :=
  { base := [],
    stack :=
      [({ id := 1, fn := 0, block := block,
          locals :=
            [s0, Slot.init (Value.int ScalarTy.U32 n), s2, t.s3, t.s4, t.s5, t.s6, t.s7, t.s8,
             t.s9],
          dest := { frame := 0, slot := 0, path := [] }, target := none } : Frame)],
    next := 2 }

/-- The frame with only locals 0, 2 and 3 given. -/
def at_ (block : BlockId) (s0 : Slot) (n : U32) (s2 s3 : Slot) : State :=
  frame block s0 n s2 ⟨s3, Slot.uninit, Slot.uninit, Slot.uninit, Slot.uninit, Slot.uninit,
    Slot.uninit⟩

/-- The loop's head with the counter at `i`. -/
def head (n i : U32) (t : Temps) : State :=
  frame 1 Slot.uninit n (Slot.init (Value.int ScalarTy.U32 i)) t

/-- The state a call starts in. -/
theorem start_entry (n : U32) :
    P.start 0 [] [Value.int ScalarTy.U32 n] =
      Outcome.ok (at_ 0 Slot.uninit n Slot.uninit Slot.uninit) := rfl

theorem one_val : (1 : U32).val = 1 :=
  Scalar.ofNat_val .U32 1 (by simp only [ScalarTy.min, ScalarTy.max] <;> omega)

theorem zero_val : (0 : U32).val = 0 :=
  Scalar.ofNat_val .U32 0 (by simp only [ScalarTy.min, ScalarTy.max] <;> omega)

/-- While `i < n`, `i + 1` does not overflow. -/
theorem in_range (n i : U32) (hlt : Scalar.lt i n = true) :
    ScalarTy.min ScalarTy.U32 ≤ i.val + (1 : U32).val ∧
      i.val + (1 : U32).val ≤ ScalarTy.max ScalarTy.U32 := by
  have hlt' : i.val < n.val := (Scalar.lt_iff _ _).mp hlt
  have hn : n.val ≤ 4294967295 := n.hmax
  have h0 : (0 : Int) ≤ i.val := i.hmin
  rw [one_val]
  simp only [ScalarTy.min, ScalarTy.max]
  omega

/-! The blocks: `mir_walk` runs a turn of the loop, and the way out. -/

/-- Block 0: `i = 0`. -/
theorem step0 (n : U32) :
    step P (at_ 0 Slot.uninit n Slot.uninit Slot.uninit) =
      Outcome.ok (Next.state (head n (0 : U32) Temps.none)) := rfl

/-! The loop: `run_loop` with the invariant `i + k = n` and the variant `k`. -/

/-- `k` turns are left when the counter is `k` short of `n`. -/
def good (n : U32) (k : Nat) (x : U32 × Temps) : Prop := x.1.val + (k : Int) = n.val

/-- The loop's head when `i >= n`: three blocks to return `i`, which is `n`. -/
theorem loop_exit (n : U32) (x : U32 × Temps) (extra : Nat) (hg : good n 0 x) :
    run P (3 + extra) (head n x.1 x.2) = Outcome.ok (Value.int ScalarTy.U32 n) := by
  have hg' : x.1.val + ((0 : Nat) : Int) = n.val := hg
  have hlt : Scalar.lt x.1 n = false := by
    unfold Scalar.lt
    exact decide_eq_false (by omega)
  have hin : x.1 = n := Scalar.ext (by omega)
  rw [show 3 + extra = extra + 1 + 1 + 1 by omega]
  mir_walk [P, loops.count.mir, head, frame, hlt]
  rw [hin]

/-- A turn of the loop when `i < n`: two blocks back to the head with the counter one more. -/
theorem loop_iter (n : U32) (k : Nat) (x : U32 × Temps) (hg : good n (k + 1) x) :
    ∃ x' : U32 × Temps, good n k x' ∧
      ∀ (fuel : Nat), run P (2 + fuel) (head n x.1 x.2) = run P fuel (head n x'.1 x'.2) := by
  have hg' : x.1.val + ((k + 1 : Nat) : Int) = n.val := hg
  have hlt : Scalar.lt x.1 n = true := (Scalar.lt_iff _ _).mpr (by omega)
  have hin := in_range n x.1 hlt
  have h1 := one_val
  have hgood : ∀ t : Temps, good n k (⟨x.1.val + (1 : U32).val, hin.1, hin.2⟩, t) := fun _ => by
    show x.1.val + (1 : U32).val + (k : Int) = n.val
    omega
  have ha : Scalar.add x.1 (Scalar.lit ScalarTy.U32 1) =
      Result.ok ⟨x.1.val + (1 : U32).val, hin.1, hin.2⟩ :=
    Scalar.add_ok x.1 (1 : U32) hin
  exact ⟨(⟨x.1.val + (1 : U32).val, hin.1, hin.2⟩, ⟨_, _, _, _, _, _, _⟩), hgood _, fun fuel => by
    rw [show 2 + fuel = fuel + 1 + 1 by omega]
    mir_walk [P, loops.count.mir, head, frame, hlt, ha]
    rfl⟩

/-- The MIR of `count`, run with `2 * n + 4` steps of fuel or more, returns `n`. -/
theorem count_runs (n : U32) (extra : Nat) :
    run P (loopFuel 2 3 n.val.toNat + extra + 1) (at_ 0 Slot.uninit n Slot.uninit Slot.uninit) =
      Outcome.ok (Value.int ScalarTy.U32 n) := by
  have h0 : (0 : Int) ≤ n.val := n.hmin
  have hz := zero_val
  have hg : good n n.val.toNat ((0 : U32), Temps.none) := by
    show (0 : U32).val + (n.val.toNat : Int) = n.val
    omega
  rw [run_step P (loopFuel 2 3 n.val.toNat + extra) _ _ (step0 n)]
  exact run_loop P (good n) (fun x => head n x.1 x.2) (Value.int ScalarTy.U32 n) 2 3
    (loop_exit n) (loop_iter n) n.val.toNat ((0 : U32), Temps.none) extra hg

/-! The model. -/

/-- The model's loop, with a turn of fuel for each turn left and one more, returns `n`. -/
theorem model_loop (n : U32) :
    ∀ (k : Nat) (i : U32), i.val + (k : Int) = n.val →
      loops.count.loop_0 (k + 1) n i = Result.ok n := by
  intro k
  induction k with
  | zero =>
    intro i hk
    have hlt : ¬ Scalar.lt i n = true := fun h => by
      have h' : i.val < n.val := (Scalar.lt_iff _ _).mp h
      omega
    have hin : i = n := Scalar.ext (by omega)
    rw [loops.count.loop_0]
    rw [ite_eq_right hlt, hin]
  | succ k ih =>
    intro i hk
    have hlt : Scalar.lt i n = true := (Scalar.lt_iff _ _).mpr (by omega)
    have hin := in_range n i hlt
    have h1 := one_val
    rw [loops.count.loop_0]
    rw [ite_eq_left hlt]
    simp only [Scalar.add_ok i (1 : U32) hin, Result.bind_ok]
    exact ih ⟨i.val + (1 : U32).val, hin.1, hin.2⟩ (by
      show i.val + (1 : U32).val + (k : Int) = n.val
      omega)

/-- The model of `count` (with `n + 1` turns of fuel) and the semantics of its MIR (with
`2 * n + 4` steps or more) give the same outcome, for every `n`. -/
theorem agrees (n : U32) (extra : Nat) :
    run P (loopFuel 2 3 n.val.toNat + extra + 1) (at_ 0 Slot.uninit n Slot.uninit Slot.uninit) =
      Outcome.map (Value.int ScalarTy.U32)
        (Outcome.ofResult (loops.count (n.val.toNat + 1) n)) := by
  have h0 : (0 : Int) ≤ n.val := n.hmin
  have hz := zero_val
  have hm : loops.count (n.val.toNat + 1) n = Result.ok n :=
    model_loop n n.val.toNat (0 : U32) (by omega)
  exact (count_runs n extra).trans
    (congrArg (fun r => Outcome.map (Value.int ScalarTy.U32) (Outcome.ofResult r)) hm).symm

/-- `2 * n + 4` is the least fuel that is enough: with `2 * n + 3`, the run of `n = 0` is out
of fuel. -/
example : run P (loopFuel 2 2 0 + 0 + 1) (at_ 0 Slot.uninit (0 : U32) Slot.uninit Slot.uninit) =
    Outcome.diverge := rfl

end BlazeAgrees.loops.count
