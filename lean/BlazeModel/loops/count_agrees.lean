import Blaze
import Blaze.Mir.Lemmas
import BlazeModel.loops.count
import BlazeMir.loops.count

/-!
`loops::count`: the functional model (`BlazeModel.loops.count`) agrees with the semantics of the
function's MIR (`BlazeMir.loops.count`), for every argument: objective V7
(docs/v2/plan/design/formal-verification.md). The proof is the lemmas of `Blaze.Mir.Lemmas`
applied in the body's block order, one theorem per block; it is the shape the translator is
to emit with each model. The embedded body is a prediction of the encoder's output until its
golden test has run: when it changes, the block theorems change with it.
-/

open Blaze Blaze.Mir

namespace BlazeAgrees.loops.count

set_option maxRecDepth 8192

abbrev P : Program := loops.count.mir

/-- The one frame: the block to run, `n` (local 1), and what locals 0, 2 and 3 hold. -/
def at_ (block : BlockId) (s0 : Slot) (n : U32) (s2 s3 : Slot) : State :=
  { base := [],
    stack :=
      [({ id := 1, fn := 0, block := block,
          locals := [s0, Slot.init (Value.int ScalarTy.U32 n), s2, s3],
          dest := { frame := 0, slot := 0, path := [] }, target := none } : Frame)],
    next := 2 }

/-- The loop's head with the counter at `i`. -/
def head (n i : U32) (s3 : Slot) : State :=
  at_ 1 Slot.uninit n (Slot.init (Value.int ScalarTy.U32 i)) s3

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

/-! The blocks. -/

/-- Block 0: `i = 0`. -/
theorem step0 (n : U32) :
    step P (at_ 0 Slot.uninit n Slot.uninit Slot.uninit) =
      Outcome.ok (Next.state (head n (0 : U32) Slot.uninit)) := rfl

def cmpStmt : Statement :=
  Statement.assign (Place.mk 3 [])
    (Rvalue.binary BinOp.lt (Operand.copy (Place.mk 2 [])) (Operand.copy (Place.mk 1 [])))

def switchTerm : Terminator :=
  Terminator.switchInt (Operand.move (Place.mk 3 [])) [(0, 2)] 3

/-- Block 1's statement: `i < n` is in local 3. -/
theorem compared (n i : U32) (s3 : Slot) :
    execStmt P.adts (head n i s3) cmpStmt =
      Outcome.ok (head n i (Slot.init (Value.bool (Scalar.lt i n)))) :=
  execStmt_assign P.adts (head n i s3) (Place.mk 3 [])
    (Rvalue.binary BinOp.lt (Operand.copy (Place.mk 2 [])) (Operand.copy (Place.mk 1 [])))
    (Value.bool (Scalar.lt i n))
    (head n i s3) { frame := 1, slot := 3, path := [] }
    (head n i (Slot.init (Value.bool (Scalar.lt i n))))
    (evalTwo_ok (head n i s3) (Operand.copy (Place.mk 2 [])) (Operand.copy (Place.mk 1 []))
      (binaryOp false BinOp.lt)
      (Value.int ScalarTy.U32 i) (head n i s3)
      (Value.int ScalarTy.U32 n) (head n i s3)
      (Value.bool (Scalar.lt i n))
      rfl rfl
      ((binaryOp_same BinOp.lt i n (by decide) (by decide)).trans rfl))
    rfl rfl

theorem switch_true (n i : U32) :
    execBlockFrom P.adts P.sig (head n i (Slot.init (Value.bool true))) [] switchTerm =
      Outcome.ok (Next.state
        (at_ 3 Slot.uninit n (Slot.init (Value.int ScalarTy.U32 i)) Slot.moved)) := rfl

theorem switch_false (n i : U32) :
    execBlockFrom P.adts P.sig (head n i (Slot.init (Value.bool false))) [] switchTerm =
      Outcome.ok (Next.state
        (at_ 2 Slot.uninit n (Slot.init (Value.int ScalarTy.U32 i)) Slot.moved)) := rfl

/-- Block 1 when `i < n`: on to the body. -/
theorem step1_true (n i : U32) (s3 : Slot) (hlt : Scalar.lt i n = true) :
    step P (head n i s3) =
      Outcome.ok (Next.state
        (at_ 3 Slot.uninit n (Slot.init (Value.int ScalarTy.U32 i)) Slot.moved)) := by
  have hs :
      execBlockFrom P.adts P.sig (head n i s3) [cmpStmt] switchTerm =
        execBlockFrom P.adts P.sig (head n i (Slot.init (Value.bool (Scalar.lt i n)))) []
          switchTerm :=
    execBlockFrom_step P.adts P.sig _ cmpStmt [] switchTerm _ (compared n i s3)
  rw [hlt] at hs
  show execBlockFrom P.adts P.sig (head n i s3) [cmpStmt] switchTerm = _
  exact hs.trans (switch_true n i)

/-- Block 1 when `i >= n`: out of the loop. -/
theorem step1_false (n i : U32) (s3 : Slot) (hlt : Scalar.lt i n = false) :
    step P (head n i s3) =
      Outcome.ok (Next.state
        (at_ 2 Slot.uninit n (Slot.init (Value.int ScalarTy.U32 i)) Slot.moved)) := by
  have hs :
      execBlockFrom P.adts P.sig (head n i s3) [cmpStmt] switchTerm =
        execBlockFrom P.adts P.sig (head n i (Slot.init (Value.bool (Scalar.lt i n)))) []
          switchTerm :=
    execBlockFrom_step P.adts P.sig _ cmpStmt [] switchTerm _ (compared n i s3)
  rw [hlt] at hs
  show execBlockFrom P.adts P.sig (head n i s3) [cmpStmt] switchTerm = _
  exact hs.trans (switch_false n i)

/-- Block 2: the function returns `i`. -/
theorem step2 (n i : U32) (s3 : Slot) :
    step P (at_ 2 Slot.uninit n (Slot.init (Value.int ScalarTy.U32 i)) s3) =
      Outcome.ok (Next.done (Value.int ScalarTy.U32 i)) := rfl

def addStmt : Statement :=
  Statement.assign (Place.mk 2 [])
    (Rvalue.checked BinOp.add (Operand.copy (Place.mk 2 []))
      (Operand.const (Const.int ScalarTy.U32 1)))

/-- Block 3's statement, `i + 1` in range: the counter is one more. -/
theorem add (n i : U32) (s3 : Slot)
    (h : ScalarTy.min ScalarTy.U32 ≤ i.val + (1 : U32).val ∧
      i.val + (1 : U32).val ≤ ScalarTy.max ScalarTy.U32) :
    execStmt P.adts (at_ 3 Slot.uninit n (Slot.init (Value.int ScalarTy.U32 i)) s3) addStmt =
      Outcome.ok
        (at_ 3 Slot.uninit n
          (Slot.init (Value.int ScalarTy.U32 ⟨i.val + (1 : U32).val, h.1, h.2⟩)) s3) :=
  execStmt_assign P.adts (at_ 3 Slot.uninit n (Slot.init (Value.int ScalarTy.U32 i)) s3)
    (Place.mk 2 [])
    (Rvalue.checked BinOp.add (Operand.copy (Place.mk 2 []))
      (Operand.const (Const.int ScalarTy.U32 1)))
    (Value.int ScalarTy.U32 ⟨i.val + (1 : U32).val, h.1, h.2⟩)
    (at_ 3 Slot.uninit n (Slot.init (Value.int ScalarTy.U32 i)) s3)
    { frame := 1, slot := 2, path := [] }
    (at_ 3 Slot.uninit n
      (Slot.init (Value.int ScalarTy.U32 ⟨i.val + (1 : U32).val, h.1, h.2⟩)) s3)
    (evalTwo_ok (at_ 3 Slot.uninit n (Slot.init (Value.int ScalarTy.U32 i)) s3)
      (Operand.copy (Place.mk 2 [])) (Operand.const (Const.int ScalarTy.U32 1))
      (binaryOp true BinOp.add)
      (Value.int ScalarTy.U32 i) (at_ 3 Slot.uninit n (Slot.init (Value.int ScalarTy.U32 i)) s3)
      (Value.int ScalarTy.U32 (1 : U32))
      (at_ 3 Slot.uninit n (Slot.init (Value.int ScalarTy.U32 i)) s3)
      (Value.int ScalarTy.U32 ⟨i.val + (1 : U32).val, h.1, h.2⟩)
      rfl rfl (binaryOp_checked_add i (1 : U32) h))
    rfl rfl

theorem back (n i : U32) (s3 : Slot) :
    execBlockFrom P.adts P.sig
        (at_ 3 Slot.uninit n (Slot.init (Value.int ScalarTy.U32 i)) s3) []
        (Terminator.goto 1) =
      Outcome.ok (Next.state (head n i s3)) := rfl

/-- Block 3, `i + 1` in range: back to the loop's head. -/
theorem step3 (n i : U32) (s3 : Slot)
    (h : ScalarTy.min ScalarTy.U32 ≤ i.val + (1 : U32).val ∧
      i.val + (1 : U32).val ≤ ScalarTy.max ScalarTy.U32) :
    step P (at_ 3 Slot.uninit n (Slot.init (Value.int ScalarTy.U32 i)) s3) =
      Outcome.ok (Next.state (head n ⟨i.val + (1 : U32).val, h.1, h.2⟩ s3)) := by
  show execBlockFrom P.adts P.sig
    (at_ 3 Slot.uninit n (Slot.init (Value.int ScalarTy.U32 i)) s3) [addStmt]
    (Terminator.goto 1) = _
  exact (execBlockFrom_step P.adts P.sig _ addStmt [] (Terminator.goto 1) _
    (add n i s3 h)).trans (back n ⟨i.val + (1 : U32).val, h.1, h.2⟩ s3)

/-! The loop: `run_loop` with the invariant `i + k = n` and the variant `k`. -/

/-- `k` turns are left when the counter is `k` short of `n`. -/
def good (n : U32) (k : Nat) (x : U32 × Slot) : Prop := x.1.val + (k : Int) = n.val

theorem loop_exit (n : U32) (x : U32 × Slot) (extra : Nat) (hg : good n 0 x) :
    run P (2 + extra) (head n x.1 x.2) = Outcome.ok (Value.int ScalarTy.U32 n) := by
  have hg' : x.1.val + ((0 : Nat) : Int) = n.val := hg
  have hlt : Scalar.lt x.1 n = false := by
    unfold Scalar.lt
    exact decide_eq_false (by omega)
  have hin : x.1 = n := Scalar.ext (by omega)
  have e : 2 + extra = extra + 1 + 1 := by omega
  rw [e, run_step P (extra + 1) _ _ (step1_false n x.1 x.2 hlt)]
  have hdone := run_done P extra _ _ (step2 n x.1 Slot.moved)
  rw [hin] at hdone
  rw [hin]
  exact hdone

theorem loop_iter (n : U32) (k : Nat) (x : U32 × Slot) (hg : good n (k + 1) x) :
    ∃ x' : U32 × Slot, good n k x' ∧
      ∀ (fuel : Nat), run P (2 + fuel) (head n x.1 x.2) = run P fuel (head n x'.1 x'.2) := by
  have hg' : x.1.val + ((k + 1 : Nat) : Int) = n.val := hg
  have hlt : Scalar.lt x.1 n = true := (Scalar.lt_iff _ _).mpr (by omega)
  have hin := in_range n x.1 hlt
  have h1 := one_val
  have hgood : good n k (⟨x.1.val + (1 : U32).val, hin.1, hin.2⟩, Slot.moved) := by
    show x.1.val + (1 : U32).val + (k : Int) = n.val
    omega
  have hrun : ∀ (fuel : Nat), run P (2 + fuel) (head n x.1 x.2) =
      run P fuel (head n ⟨x.1.val + (1 : U32).val, hin.1, hin.2⟩ Slot.moved) := by
    intro fuel
    have e : 2 + fuel = fuel + 1 + 1 := by omega
    rw [e, run_step P (fuel + 1) _ _ (step1_true n x.1 x.2 hlt),
      run_step P fuel _ _ (step3 n x.1 Slot.moved hin)]
  exact ⟨(⟨x.1.val + (1 : U32).val, hin.1, hin.2⟩, Slot.moved), hgood, hrun⟩

/-- The MIR of `count`, run with `2 * n + 3` steps of fuel or more, returns `n`. -/
theorem count_runs (n : U32) (extra : Nat) :
    run P (loopFuel 2 2 n.val.toNat + extra + 1) (at_ 0 Slot.uninit n Slot.uninit Slot.uninit) =
      Outcome.ok (Value.int ScalarTy.U32 n) := by
  have h0 : (0 : Int) ≤ n.val := n.hmin
  have hz := zero_val
  have hg : good n n.val.toNat ((0 : U32), Slot.uninit) := by
    show (0 : U32).val + (n.val.toNat : Int) = n.val
    omega
  rw [run_step P (loopFuel 2 2 n.val.toNat + extra) _ _ (step0 n)]
  exact run_loop P (good n) (fun x => head n x.1 x.2) (Value.int ScalarTy.U32 n) 2 2
    (loop_exit n) (loop_iter n) n.val.toNat ((0 : U32), Slot.uninit) extra hg

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
    rw [if_neg hlt, hin]
  | succ k ih =>
    intro i hk
    have hlt : Scalar.lt i n = true := (Scalar.lt_iff _ _).mpr (by omega)
    have hin := in_range n i hlt
    have h1 := one_val
    rw [loops.count.loop_0]
    rw [if_pos hlt]
    simp only [Scalar.add_ok i (1 : U32) hin, Result.bind_ok]
    exact ih ⟨i.val + (1 : U32).val, hin.1, hin.2⟩ (by
      show i.val + (1 : U32).val + (k : Int) = n.val
      omega)

/-- The model of `count` (with `n + 1` turns of fuel) and the semantics of its MIR (with
`2 * n + 3` steps or more) give the same outcome, for every `n`. -/
theorem agrees (n : U32) (extra : Nat) :
    run P (loopFuel 2 2 n.val.toNat + extra + 1) (at_ 0 Slot.uninit n Slot.uninit Slot.uninit) =
      Outcome.map (Value.int ScalarTy.U32)
        (Outcome.ofResult (loops.count (n.val.toNat + 1) n)) := by
  have h0 : (0 : Int) ≤ n.val := n.hmin
  have hz := zero_val
  have hm : loops.count (n.val.toNat + 1) n = Result.ok n :=
    model_loop n n.val.toNat (0 : U32) (by omega)
  exact (count_runs n extra).trans
    (congrArg (fun r => Outcome.map (Value.int ScalarTy.U32) (Outcome.ofResult r)) hm).symm

end BlazeAgrees.loops.count
