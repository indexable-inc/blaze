import Blaze
import Blaze.Mir.Walk
import BlazeModel.blaze_graph.ledger.fnv1a_from
import BlazeMir.blaze_graph.ledger.fnv1a_from
import BlazeGraph.Ledger

/-!
`blaze_graph::ledger::fnv1a_from`: the functional model the proofs are about agrees with the
semantics of the function's MIR (docs/v2/plan/design/formal-verification.md, "The chain for
`fnv1a_from`"; objective V7).

The MIR is `blaze_graph.ledger.fnv1a_from.mir`, the body the encoder writes
(lean/BlazeMir/blaze_graph/ledger/fnv1a_from.lean, held to the encoder by a golden test). The
proof runs it with `mir_walk` (`Blaze.Mir.Walk`) a turn of the loop at a time, from the loop's
head (block 1) back to it, with the four facts about the data the walk cannot decide passed
in: the slice's length, the comparison, the byte read and the counter's increment. Then an
induction over the bytes left (`loop_runs`). The machine takes one step for the entry block,
four per byte and four to leave: `4 * len + 5` in all.

- `fnv1a_from_runs`: with that fuel or more, the run returns FNV-1a's fold. It never panics
  and has no undefined behaviour.
- `fnv1a_from_any_fuel`: with any fuel, the run is out of fuel or returns the fold.
- `fnv1a_from_model_agrees`: the run is the model's result (`BlazeModel`), as an outcome.

No state between two blocks is written down: when the encoder's output changes its
temporaries or storage markers, only `Temps` and the step counts change.
-/

open Blaze Blaze.Mir

namespace BlazeGraph.LedgerMir

set_option maxRecDepth 8192

abbrev P : Program := blaze_graph.ledger.fnv1a_from.mir

/-- The bytes as the slice reference's referent holds them. -/
def bytesValue (bytes : Slice U8) : List Value :=
  bytes.val.map (fun b => Value.int ScalarTy.U8 b)

/-- What the temporaries (locals 4 to 19) hold at the loop's head: the first time nothing,
after a turn what the turn left. No turn reads one it did not write first. -/
structure Temps where
  (s4 s5 s6 s7 s8 s9 s10 s11 s12 s13 s14 s15 s16 s17 s18 s19 : Slot)

/-- A state of the one frame: the block to run, local 0, `hash` (local 1), the reference to
the bytes (local 2, into frame 0), the counter's slot (local 3) and the temporaries. -/
def at_ (block : BlockId) (s0 : Slot) (hash : U64) (bytes : Slice U8) (s3 : Slot) (t : Temps) :
    State :=
  { base := [Slot.init (Value.array (bytesValue bytes))],
    stack :=
      [({ id := 1, fn := 0, block := block,
          locals :=
            [s0, Slot.init (Value.int ScalarTy.U64 hash),
             Slot.init (Value.ref { frame := 0, slot := 0, path := [] }), s3,
             t.s4, t.s5, t.s6, t.s7, t.s8, t.s9, t.s10, t.s11, t.s12, t.s13, t.s14, t.s15,
             t.s16, t.s17, t.s18, t.s19],
          dest := { frame := 0, slot := 0, path := [] }, target := none } : Frame)],
    next := 2 }

/-- No temporary holds anything. -/
def Temps.none : Temps :=
  ⟨Slot.uninit, Slot.uninit, Slot.uninit, Slot.uninit, Slot.uninit, Slot.uninit, Slot.uninit,
   Slot.uninit, Slot.uninit, Slot.uninit, Slot.uninit, Slot.uninit, Slot.uninit, Slot.uninit,
   Slot.uninit, Slot.uninit⟩

/-- The loop's head with the counter `at` (local 3) holding `i`. -/
def head (hash : U64) (bytes : Slice U8) (i : Usize) (t : Temps) : State :=
  at_ 1 Slot.uninit hash bytes (Slot.init (Value.int ScalarTy.Usize i)) t

/-- The state a call starts in. -/
def entry (hash : U64) (bytes : Slice U8) : State :=
  at_ 0 Slot.uninit hash bytes Slot.uninit Temps.none

/-- `entry` is where `Program.start` puts a call with the seed and a reference to the bytes. -/
theorem start_entry (hash : U64) (bytes : Slice U8) :
    P.start 0 [Slot.init (Value.array (bytesValue bytes))]
        [Value.int ScalarTy.U64 hash, Value.ref { frame := 0, slot := 0, path := [] }] =
      Outcome.ok (entry hash bytes) := rfl

theorem len_value (bytes : Slice U8) :
    Scalar.lit ScalarTy.Usize ((bytesValue bytes).length : Int) = Slice.len bytes := by
  have hlen : (bytesValue bytes).length = bytes.val.length := by simp [bytesValue]
  have hmax := bytes.hlen
  have hb : ScalarTy.min ScalarTy.Usize ≤ (bytes.val.length : Int) ∧
      (bytes.val.length : Int) ≤ ScalarTy.max ScalarTy.Usize := by
    simp only [ScalarTy.min, ScalarTy.max]
    omega
  apply Scalar.ext
  rw [hlen, Slice.len_val]
  exact Scalar.lit_val ScalarTy.Usize _ hb

/-! The blocks. -/

/-- Block 0, `at = 0`: on to the loop's head. -/
theorem step0 (hash : U64) (bytes : Slice U8) :
    step P (entry hash bytes) =
      Outcome.ok (Next.state (head hash bytes (0 : Usize) Temps.none)) :=
  rfl

/-- The loop's head when `at >= len`: four blocks to return `hash`. -/
theorem leave (hash : U64) (bytes : Slice U8) (i : Usize) (t : Temps) (fuel : Nat)
    (hlt : Scalar.lt i (Slice.len bytes) = false) :
    run P (fuel + 4) (head hash bytes i t) = Outcome.ok (Value.int ScalarTy.U64 hash) := by
  mir_walk [P, blaze_graph.ledger.fnv1a_from.mir, head, at_, len_value, hlt]

/-- A turn of the loop when `at < len`: one byte of FNV-1a is in `hash`, the counter is one
more, and four blocks later the run is at the loop's head again. -/
theorem turn (hash : U64) (bytes : Slice U8) (i : Usize) (t : Temps)
    (h : i.val.toNat < bytes.val.length)
    (hlt : Scalar.lt i (Slice.len bytes) = true)
    (hadd : ScalarTy.min ScalarTy.Usize ≤ i.val + (1 : Usize).val ∧
      i.val + (1 : Usize).val ≤ ScalarTy.max ScalarTy.Usize) :
    ∃ t' : Temps, ∀ fuel : Nat,
      run P (fuel + 4) (head hash bytes i t) =
        run P fuel (head (BlazeGraph.Ledger.step hash (bytes.val.get ⟨i.val.toNat, h⟩)) bytes
          ⟨i.val + (1 : Usize).val, hadd.1, hadd.2⟩ t') := by
  have hn : nth? (bytesValue bytes) i.val.toNat =
      some (Value.int ScalarTy.U8 (bytes.val.get ⟨i.val.toNat, h⟩)) :=
    nth?_map_get (fun b => Value.int ScalarTy.U8 b) bytes.val i.val.toNat h
  have ha : Scalar.add i (Scalar.lit ScalarTy.Usize 1) =
      Result.ok ⟨i.val + (1 : Usize).val, hadd.1, hadd.2⟩ :=
    Scalar.add_ok i (1 : Usize) hadd
  exact ⟨⟨_, _, _, _, _, _, _, _, _, _, _, _, _, _, _, _⟩, fun fuel => by
    mir_walk [P, blaze_graph.ledger.fnv1a_from.mir, head, at_, len_value, hlt, hn, ha]
    rfl⟩

/-! The loop and the function. -/

/-- From the loop's head with `k` bytes left, `4 * k + 4` steps return FNV-1a's fold over
them. No step panics or is undefined. -/
theorem loop_runs (bytes : Slice U8) :
    ∀ (k : Nat) (hash : U64) (i : Usize) (t : Temps) (extra : Nat),
      i.val.toNat + k = bytes.val.length →
      run P (4 * k + 4 + extra) (head hash bytes i t) =
        Outcome.ok (Value.int ScalarTy.U64
          (BlazeGraph.Ledger.fold hash (bytes.val.drop i.val.toNat))) := by
  intro k
  induction k with
  | zero =>
    intro hash i t extra hk
    have hlen : (Slice.len bytes).val = (bytes.val.length : Int) := Slice.len_val bytes
    have h0 : (0 : Int) ≤ i.val := i.hmin
    have hlt : Scalar.lt i (Slice.len bytes) = false := by
      unfold Scalar.lt
      exact decide_eq_false (by omega)
    have e : 4 * 0 + 4 + extra = extra + 4 := by omega
    rw [Blaze.List.drop_of_length_le bytes.val i.val.toNat (by omega),
      BlazeGraph.Ledger.fold_nil, e]
    exact leave hash bytes i t extra hlt
  | succ k ih =>
    intro hash i t extra hk
    have hlen : (Slice.len bytes).val = (bytes.val.length : Int) := Slice.len_val bytes
    have h0 : (0 : Int) ≤ i.val := i.hmin
    have hlt : Scalar.lt i (Slice.len bytes) = true := (Scalar.lt_iff _ _).mpr (by omega)
    have ⟨hidx, hadd, hnext⟩ := BlazeGraph.Ledger.in_bounds bytes i hlt
    have ⟨t', hturn⟩ := turn hash bytes i t hidx hlt hadd
    have e : 4 * (k + 1) + 4 + extra = (4 * k + 4 + extra) + 4 := by omega
    rw [Blaze.List.drop_eq_get_cons bytes.val i.val.toNat hidx, BlazeGraph.Ledger.fold_cons,
      ← hnext, e, hturn]
    exact ih (BlazeGraph.Ledger.step hash (bytes.val.get ⟨i.val.toNat, hidx⟩))
      ⟨i.val + (1 : Usize).val, hadd.1, hadd.2⟩ t' extra (by
        show (i.val + (1 : Usize).val).toNat + k = bytes.val.length
        rw [hnext]
        omega)

/-- The MIR of `fnv1a_from`, run with `4 * len + 5` steps of fuel or more, returns FNV-1a's
fold of the seed over the bytes. -/
theorem fnv1a_from_runs (hash : U64) (bytes : Slice U8) (extra : Nat) :
    run P (4 * bytes.val.length + 5 + extra) (entry hash bytes) =
      Outcome.ok (Value.int ScalarTy.U64 (BlazeGraph.Ledger.fold hash bytes.val)) := by
  have e : 4 * bytes.val.length + 5 + extra = (4 * bytes.val.length + 4 + extra) + 1 := by
    omega
  rw [e, run_step P (4 * bytes.val.length + 4 + extra) _ _ (step0 hash bytes)]
  have h := loop_runs bytes bytes.val.length hash (0 : Usize) Temps.none extra
    (by have := BlazeGraph.Ledger.zero_val; omega)
  rw [BlazeGraph.Ledger.zero_val] at h
  exact h

/-- With any fuel, the MIR of `fnv1a_from` is out of fuel or returns the fold: it never
panics and never has undefined behaviour. -/
theorem fnv1a_from_any_fuel (fuel : Nat) (hash : U64) (bytes : Slice U8) :
    run P fuel (entry hash bytes) = Outcome.diverge ∨
      run P fuel (entry hash bytes) =
        Outcome.ok (Value.int ScalarTy.U64 (BlazeGraph.Ledger.fold hash bytes.val)) := by
  by_cases hd : run P fuel (entry hash bytes) = Outcome.diverge
  · exact Or.inl hd
  · have enough := fnv1a_from_runs hash bytes 0
    have ends : run P (4 * bytes.val.length + 5 + 0) (entry hash bytes) ≠ Outcome.diverge := by
      rw [enough]
      intro h
      cases h
    have same := run_agree P (entry hash bytes) fuel (4 * bytes.val.length + 5 + 0) hd ends
    rw [enough] at same
    exact Or.inr same

/-- The chain's link (objective V7): the functional model of `fnv1a_from`
(`blaze_graph.ledger.fnv1a_from`, with fuel for every byte) and the semantics of its MIR
(with `4 * len + 5` steps or more) give the same outcome, for every seed and every slice. -/
theorem fnv1a_from_model_agrees (hash : U64) (bytes : Slice U8) (extra : Nat) :
    run P (4 * bytes.val.length + 5 + extra) (entry hash bytes) =
      Outcome.map (Value.int ScalarTy.U64)
        (Outcome.ofResult
          (blaze_graph.ledger.fnv1a_from (bytes.val.length + 1) hash bytes)) := by
  rw [fnv1a_from_runs hash bytes extra,
    BlazeGraph.Ledger.fnv1a_from_returns (bytes.val.length + 1) hash bytes (by omega)]
  rfl

/-- `4 * len + 5` is the least fuel that is enough: with `4 * len + 4`, the run of no bytes is
out of fuel. -/
example : run P 4 (entry 0 ⟨[], by decide⟩) = Outcome.diverge := rfl

end BlazeGraph.LedgerMir
