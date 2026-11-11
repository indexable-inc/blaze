import Blaze
import BlazeModel.blaze_graph.ledger.fnv1a_from

/-!
Proofs about `blaze_graph::ledger` (crates/blaze-graph/src/ledger.rs), over the models blaze
generates from its MIR (docs/v2/plan/design/formal-verification.md).

`fnv1a_from`, the checksum of the ledger's file: its loop never panics (no index is out of
bounds, the counter never overflows) and what it returns is FNV-1a's fold over the bytes,
`fold`; with more fuel than bytes it returns. The generated statement of its `ensures`
(`blaze_graph.ledger.fnv1a_from.spec.statement`) follows.
-/

open Blaze

namespace BlazeGraph.Ledger

/-- One byte of FNV-1a: xor it in, multiply by the 64-bit prime. -/
def step (hash : U64) (byte : U8) : U64 :=
  Scalar.wrapping_mul (Scalar.xor hash (Scalar.cast .U64 byte)) (1099511628211 : U64)

/-- FNV-1a from `hash` over `bytes`. -/
def fold (hash : U64) (bytes : List U8) : U64 := bytes.foldl step hash

theorem fold_nil (hash : U64) : fold hash [] = hash := rfl

theorem fold_cons (hash : U64) (b : U8) (bs : List U8) :
    fold hash (b :: bs) = fold (step hash b) bs := rfl

theorem one_val : (1 : Usize).val = 1 :=
  Scalar.ofNat_val .Usize 1 (by simp only [ScalarTy.min, ScalarTy.max] <;> omega)

theorem zero_val : (0 : Usize).val = 0 :=
  Scalar.ofNat_val .Usize 0 (by simp only [ScalarTy.min, ScalarTy.max] <;> omega)

/-- What a turn of the loop knows when `i` is in bounds: the index reads, the counter does
not overflow, and the next counter is one more. -/
theorem in_bounds (bytes : Slice U8) (i : Usize) (hlt : Scalar.lt i (Slice.len bytes) = true) :
    i.val.toNat < bytes.val.length ∧
      (ScalarTy.min .Usize ≤ i.val + (1 : Usize).val ∧
        i.val + (1 : Usize).val ≤ ScalarTy.max .Usize) ∧
      (i.val + (1 : Usize).val).toNat = i.val.toNat + 1 := by
  have hlt' : i.val < (Slice.len bytes).val := (Scalar.lt_iff _ _).mp hlt
  have hlen : (Slice.len bytes).val = (bytes.val.length : Int) := Slice.len_val bytes
  have h0 : (0 : Int) ≤ i.val := i.hmin
  have hmax := bytes.hlen
  rw [one_val]
  simp only [ScalarTy.min, ScalarTy.max]
  omega

/-- The loop, from any state with `i` at most the length: out of fuel, or FNV-1a's fold over
the bytes from `i` on. It never fails. -/
theorem loop_eq (bytes : Slice U8) :
    ∀ (fuel : Nat) (hash : U64) (i : Usize), i.val.toNat ≤ bytes.val.length →
      blaze_graph.ledger.fnv1a_from.loop_0 fuel hash bytes i = Result.div ∨
        blaze_graph.ledger.fnv1a_from.loop_0 fuel hash bytes i =
          Result.ok (fold hash (bytes.val.drop i.val.toNat)) := by
  intro fuel
  induction fuel with
  | zero =>
    intro hash i _
    rw [blaze_graph.ledger.fnv1a_from.loop_0]
    exact Or.inl rfl
  | succ n ih =>
    intro hash i hi
    rw [blaze_graph.ledger.fnv1a_from.loop_0]
    by_cases hlt : Scalar.lt i (Slice.len bytes) = true
    · have ⟨hidx, hadd, hnext⟩ := in_bounds bytes i hlt
      rw [if_pos hlt]
      simp only [Slice.index_ok bytes i hidx, Scalar.add_ok i (1 : Usize) hadd, Result.bind_ok]
      have hstep := ih (step hash (bytes.val.get ⟨i.val.toNat, hidx⟩))
        ⟨i.val + (1 : Usize).val, hadd.1, hadd.2⟩ (by
          show (i.val + (1 : Usize).val).toNat ≤ bytes.val.length
          omega)
      rw [Blaze.List.drop_eq_get_cons bytes.val i.val.toNat hidx, fold_cons, ← hnext]
      exact hstep
    · rw [if_neg hlt]
      have hge : ¬ i.val < (Slice.len bytes).val := fun h => hlt ((Scalar.lt_iff _ _).mpr h)
      have hlen : (Slice.len bytes).val = (bytes.val.length : Int) := Slice.len_val bytes
      have h0 : (0 : Int) ≤ i.val := i.hmin
      have hdrop : bytes.val.drop i.val.toNat = [] :=
        Blaze.List.drop_of_length_le bytes.val i.val.toNat (by omega)
      rw [hdrop, fold_nil]
      exact Or.inr rfl

/-- With more fuel than bytes left, the loop does not run out. -/
theorem loop_returns (bytes : Slice U8) :
    ∀ (fuel : Nat) (hash : U64) (i : Usize), i.val.toNat ≤ bytes.val.length →
      bytes.val.length - i.val.toNat < fuel →
      blaze_graph.ledger.fnv1a_from.loop_0 fuel hash bytes i ≠ Result.div := by
  intro fuel
  induction fuel with
  | zero =>
    intro hash i _ hfuel
    omega
  | succ n ih =>
    intro hash i hi hfuel
    rw [blaze_graph.ledger.fnv1a_from.loop_0]
    by_cases hlt : Scalar.lt i (Slice.len bytes) = true
    · have ⟨hidx, hadd, hnext⟩ := in_bounds bytes i hlt
      rw [if_pos hlt]
      simp only [Slice.index_ok bytes i hidx, Scalar.add_ok i (1 : Usize) hadd, Result.bind_ok]
      exact ih (step hash (bytes.val.get ⟨i.val.toNat, hidx⟩))
        ⟨i.val + (1 : Usize).val, hadd.1, hadd.2⟩
        (by
          show (i.val + (1 : Usize).val).toNat ≤ bytes.val.length
          omega)
        (by
          show bytes.val.length - (i.val + (1 : Usize).val).toNat < n
          omega)
    · rw [if_neg hlt]
      intro h
      cases h

/-- `fnv1a_from` never fails: out of fuel, or FNV-1a's fold over all the bytes. -/
theorem fnv1a_from_eq (fuel : Nat) (hash : U64) (bytes : Slice U8) :
    blaze_graph.ledger.fnv1a_from fuel hash bytes = Result.div ∨
      blaze_graph.ledger.fnv1a_from fuel hash bytes = Result.ok (fold hash bytes.val) := by
  have h := loop_eq bytes fuel hash (0 : Usize) (by have := zero_val; omega)
  rw [zero_val] at h
  exact h

/-- With more fuel than bytes, `fnv1a_from` returns FNV-1a's fold. -/
theorem fnv1a_from_returns (fuel : Nat) (hash : U64) (bytes : Slice U8)
    (hfuel : bytes.val.length < fuel) :
    blaze_graph.ledger.fnv1a_from fuel hash bytes = Result.ok (fold hash bytes.val) := by
  have hne : blaze_graph.ledger.fnv1a_from fuel hash bytes ≠ Result.div :=
    loop_returns bytes fuel hash (0 : Usize)
      (by have := zero_val; omega)
      (by have := zero_val; omega)
  cases fnv1a_from_eq fuel hash bytes with
  | inl h => exact absurd h hne
  | inr h => exact h

end BlazeGraph.Ledger

/-- The generated statement: `fnv1a_from` does not panic, and of no bytes it returns its
seed. -/
theorem blaze_graph.ledger.fnv1a_from.spec.proof :
    blaze_graph.ledger.fnv1a_from.spec.statement := by
  unfold blaze_graph.ledger.fnv1a_from.spec.statement
  intro fuel hash bytes
  cases BlazeGraph.Ledger.fnv1a_from_eq fuel hash bytes with
  | inl h => simp only [h]
  | inr h =>
    simp only [h]
    cases hb : bytes.val with
    | nil => exact Or.inr rfl
    | cons b bs =>
      have hl : bytes.val.length = bs.length + 1 := congrArg List.length hb
      have hlen : (Slice.len bytes).val = (bytes.val.length : Int) := Slice.len_val bytes
      exact Or.inl (by omega)
