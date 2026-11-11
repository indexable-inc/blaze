import Blaze
import Blaze.Mir.Lemmas
import BlazeModel.blaze_graph.ledger.fnv1a_from
import BlazeMir.blaze_graph.ledger.fnv1a_from
import BlazeGraph.Ledger

/-!
`blaze_graph::ledger::fnv1a_from`: the functional model the proofs are about agrees with the
semantics of the function's MIR (docs/v2/plan/design/formal-verification.md, "The chain for
`fnv1a_from`"; objective V7).

The MIR is `blaze_graph.ledger.fnv1a_from.mir`, the body the encoder writes
(lean/BlazeMir/blaze_graph/ledger/fnv1a_from.lean, held to the encoder by a golden test). The
proof walks it block by block with the lemmas of `Blaze.Mir.Lemmas`: one theorem per block
(`step0` .. `step5`), each a `step` from a state of known shape, then an induction over the
bytes left (`loop_runs`). The machine takes one step for the entry block, four per byte and
three to leave: `4 * len + 4` in all.

- `fnv1a_from_runs`: with that fuel or more, the run returns FNV-1a's fold. It never panics
  and has no undefined behaviour.
- `fnv1a_from_any_fuel`: with any fuel, the run is out of fuel or returns the fold.
- `fnv1a_from_model_agrees`: the run is the model's result (`BlazeModel`), as an outcome.

When the encoder's output for this function changes, the block theorems change with it; each
fails on its own.
-/

open Blaze Blaze.Mir

namespace BlazeGraph.LedgerMir

set_option maxRecDepth 8192

abbrev P : Program := blaze_graph.ledger.fnv1a_from.mir

/-- The bytes as the slice reference's referent holds them. -/
def bytesValue (bytes : Slice U8) : List Value :=
  bytes.val.map (fun b => Value.int ScalarTy.U8 b)

/-- A state of the one frame: the block to run, `hash` (local 1), the reference to the bytes
(local 2, into frame 0), and what locals 0 and 3 to 7 hold. -/
def at_ (block : BlockId) (hash : U64) (bytes : Slice U8) (s0 s3 s4 s5 s6 s7 : Slot) : State :=
  { base := [Slot.init (Value.array (bytesValue bytes))],
    stack :=
      [({ id := 1, fn := 0, block := block,
          locals :=
            [s0, Slot.init (Value.int ScalarTy.U64 hash),
             Slot.init (Value.ref { frame := 0, slot := 0, path := [] }), s3, s4, s5, s6, s7],
          dest := { frame := 0, slot := 0, path := [] }, target := none } : Frame)],
    next := 2 }

/-- The counter `at` (local 3) holding `i`. -/
def idx (i : Usize) : Slot := Slot.init (Value.int ScalarTy.Usize i)

/-- A state in the loop. -/
def loopAt (block : BlockId) (hash : U64) (bytes : Slice U8) (i : Usize)
    (s4 s5 s6 s7 : Slot) : State :=
  at_ block hash bytes Slot.uninit (idx i) s4 s5 s6 s7

/-- The state a call starts in. -/
def entry (hash : U64) (bytes : Slice U8) : State :=
  at_ 0 hash bytes Slot.uninit Slot.uninit Slot.uninit Slot.uninit Slot.uninit Slot.uninit

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

/-- Block 0: `at = 0`. -/
theorem step0 (hash : U64) (bytes : Slice U8) :
    step P (entry hash bytes) =
      Outcome.ok (Next.state
        (loopAt 1 hash bytes (0 : Usize) Slot.uninit Slot.uninit Slot.uninit Slot.uninit)) :=
  rfl

/-- Block 1: `bytes.len()`. -/
theorem step1 (hash : U64) (bytes : Slice U8) (i : Usize) (s4 s5 s6 s7 : Slot) :
    step P (loopAt 1 hash bytes i s4 s5 s6 s7) =
      Outcome.ok (Next.state
        (loopAt 2 hash bytes i (Slot.init (Value.int ScalarTy.Usize (Slice.len bytes)))
          s5 s6 s7)) := by
  rw [← len_value bytes]
  rfl

def cmpStmt : Statement :=
  Statement.assign (Place.mk 5 [])
    (Rvalue.binary BinOp.lt (Operand.copy (Place.mk 3 [])) (Operand.move (Place.mk 4 [])))

def switchTerm : Terminator :=
  Terminator.switchInt (Operand.move (Place.mk 5 [])) [(0, 3)] 4

/-- Block 2's statement: `at < len` is in local 5, and the length is moved out of. -/
theorem compare_len (hash : U64) (bytes : Slice U8) (i : Usize) (s5 s6 s7 : Slot) :
    execStmt P.adts
        (loopAt 2 hash bytes i (Slot.init (Value.int ScalarTy.Usize (Slice.len bytes))) s5 s6 s7)
        cmpStmt =
      Outcome.ok
        (loopAt 2 hash bytes i Slot.moved
          (Slot.init (Value.bool (Scalar.lt i (Slice.len bytes)))) s6 s7) :=
  execStmt_assign P.adts
    (loopAt 2 hash bytes i (Slot.init (Value.int ScalarTy.Usize (Slice.len bytes))) s5 s6 s7)
    (Place.mk 5 [])
    (Rvalue.binary BinOp.lt (Operand.copy (Place.mk 3 [])) (Operand.move (Place.mk 4 [])))
    (Value.bool (Scalar.lt i (Slice.len bytes)))
    (loopAt 2 hash bytes i Slot.moved s5 s6 s7) { frame := 1, slot := 5, path := [] }
    (loopAt 2 hash bytes i Slot.moved
      (Slot.init (Value.bool (Scalar.lt i (Slice.len bytes)))) s6 s7)
    (evalTwo_ok
      (loopAt 2 hash bytes i (Slot.init (Value.int ScalarTy.Usize (Slice.len bytes))) s5 s6 s7)
      (Operand.copy (Place.mk 3 [])) (Operand.move (Place.mk 4 [])) (binaryOp false BinOp.lt)
      (Value.int ScalarTy.Usize i)
      (loopAt 2 hash bytes i (Slot.init (Value.int ScalarTy.Usize (Slice.len bytes))) s5 s6 s7)
      (Value.int ScalarTy.Usize (Slice.len bytes)) (loopAt 2 hash bytes i Slot.moved s5 s6 s7)
      (Value.bool (Scalar.lt i (Slice.len bytes)))
      rfl rfl
      ((binaryOp_same BinOp.lt i (Slice.len bytes) (by decide) (by decide)).trans rfl))
    rfl rfl

/-- Block 2's switch on `true`: on to the body. -/
theorem switch_true (hash : U64) (bytes : Slice U8) (i : Usize) (s6 s7 : Slot) :
    execBlockFrom P.adts P.sig
        (loopAt 2 hash bytes i Slot.moved (Slot.init (Value.bool true)) s6 s7) [] switchTerm =
      Outcome.ok (Next.state (loopAt 4 hash bytes i Slot.moved Slot.moved s6 s7)) :=
  rfl

/-- Block 2's switch on `false`: out of the loop. -/
theorem switch_false (hash : U64) (bytes : Slice U8) (i : Usize) (s6 s7 : Slot) :
    execBlockFrom P.adts P.sig
        (loopAt 2 hash bytes i Slot.moved (Slot.init (Value.bool false)) s6 s7) [] switchTerm =
      Outcome.ok (Next.state (loopAt 3 hash bytes i Slot.moved Slot.moved s6 s7)) :=
  rfl

/-- Block 2 when `at < len`: on to the body. -/
theorem step2_true (hash : U64) (bytes : Slice U8) (i : Usize) (s5 s6 s7 : Slot)
    (hlt : Scalar.lt i (Slice.len bytes) = true) :
    step P (loopAt 2 hash bytes i (Slot.init (Value.int ScalarTy.Usize (Slice.len bytes)))
        s5 s6 s7) =
      Outcome.ok (Next.state (loopAt 4 hash bytes i Slot.moved Slot.moved s6 s7)) := by
  have hs :
      execBlockFrom P.adts P.sig
          (loopAt 2 hash bytes i (Slot.init (Value.int ScalarTy.Usize (Slice.len bytes)))
            s5 s6 s7) [cmpStmt] switchTerm =
        execBlockFrom P.adts P.sig
          (loopAt 2 hash bytes i Slot.moved
            (Slot.init (Value.bool (Scalar.lt i (Slice.len bytes)))) s6 s7) [] switchTerm :=
    execBlockFrom_step P.adts P.sig _ cmpStmt [] switchTerm _ (compare_len hash bytes i s5 s6 s7)
  rw [hlt] at hs
  show execBlockFrom P.adts P.sig
      (loopAt 2 hash bytes i (Slot.init (Value.int ScalarTy.Usize (Slice.len bytes))) s5 s6 s7)
      [cmpStmt] switchTerm = _
  exact hs.trans (switch_true hash bytes i s6 s7)

/-- Block 2 when `at >= len`: out of the loop. -/
theorem step2_false (hash : U64) (bytes : Slice U8) (i : Usize) (s5 s6 s7 : Slot)
    (hlt : Scalar.lt i (Slice.len bytes) = false) :
    step P (loopAt 2 hash bytes i (Slot.init (Value.int ScalarTy.Usize (Slice.len bytes)))
        s5 s6 s7) =
      Outcome.ok (Next.state (loopAt 3 hash bytes i Slot.moved Slot.moved s6 s7)) := by
  have hs :
      execBlockFrom P.adts P.sig
          (loopAt 2 hash bytes i (Slot.init (Value.int ScalarTy.Usize (Slice.len bytes)))
            s5 s6 s7) [cmpStmt] switchTerm =
        execBlockFrom P.adts P.sig
          (loopAt 2 hash bytes i Slot.moved
            (Slot.init (Value.bool (Scalar.lt i (Slice.len bytes)))) s6 s7) [] switchTerm :=
    execBlockFrom_step P.adts P.sig _ cmpStmt [] switchTerm _ (compare_len hash bytes i s5 s6 s7)
  rw [hlt] at hs
  show execBlockFrom P.adts P.sig
      (loopAt 2 hash bytes i (Slot.init (Value.int ScalarTy.Usize (Slice.len bytes))) s5 s6 s7)
      [cmpStmt] switchTerm = _
  exact hs.trans (switch_false hash bytes i s6 s7)

/-- Block 3: the function returns `hash`. -/
theorem step3 (hash : U64) (bytes : Slice U8) (i : Usize) (s4 s5 s6 s7 : Slot) :
    step P (loopAt 3 hash bytes i s4 s5 s6 s7) =
      Outcome.ok (Next.done (Value.int ScalarTy.U64 hash)) :=
  rfl

def readPlace : Place := Place.mk 2 [ProjElem.deref, ProjElem.index 3]

def readStmt : Statement :=
  Statement.assign (Place.mk 6 []) (Rvalue.use (Operand.copy readPlace))

def castStmt : Statement :=
  Statement.assign (Place.mk 7 []) (Rvalue.cast (Operand.move (Place.mk 6 [])) ScalarTy.U64)

def xorStmt : Statement :=
  Statement.assign (Place.mk 1 [])
    (Rvalue.binary BinOp.bitXor (Operand.copy (Place.mk 1 [])) (Operand.move (Place.mk 7 [])))

def mulTerm : Terminator :=
  Terminator.call (Callee.intrinsic Intrinsic.wrappingMul)
    [Operand.copy (Place.mk 1 []), Operand.const (Const.int ScalarTy.U64 1099511628211)]
    (Place.mk 1 []) (some 5)

/-- Block 4's first statement, `bytes[at]` in bounds: the byte is in local 6. -/
theorem read_byte (hash : U64) (bytes : Slice U8) (i : Usize) (s4 s5 s6 s7 : Slot)
    (h : i.val.toNat < bytes.val.length) :
    execStmt P.adts (loopAt 4 hash bytes i s4 s5 s6 s7) readStmt =
      Outcome.ok (loopAt 4 hash bytes i s4 s5
        (Slot.init (Value.int ScalarTy.U8 (bytes.val.get ⟨i.val.toNat, h⟩))) s7) := by
  have hn : nth? (bytesValue bytes) i.val.toNat =
      some (Value.int ScalarTy.U8 (bytes.val.get ⟨i.val.toNat, h⟩)) :=
    nth?_map_get (fun b => Value.int ScalarTy.U8 b) bytes.val i.val.toNat h
  have hp : evalPlace (loopAt 4 hash bytes i s4 s5 s6 s7) readPlace =
      Outcome.ok { frame := 0, slot := 0, path := [PathElem.index i.val.toNat] } := by
    show (inBounds { frame := 0, slot := 0, path := [] } i.val.toNat
        (nth? (bytesValue bytes) i.val.toNat)).bind
      (fun q => evalProj (loopAt 4 hash bytes i s4 s5 s6 s7) q []) = _
    rw [hn]
    rfl
  have hv : (loopAt 4 hash bytes i s4 s5 s6 s7).read
        { frame := 0, slot := 0, path := [PathElem.index i.val.toNat] } =
      Outcome.ok (Value.int ScalarTy.U8 (bytes.val.get ⟨i.val.toNat, h⟩)) := by
    show ((found Undefined.dangling (nth? (bytesValue bytes) i.val.toNat)).bind
      (fun inner => Value.project inner [])).bind Value.checkLive = _
    rw [hn]
    rfl
  exact execStmt_assign P.adts (loopAt 4 hash bytes i s4 s5 s6 s7) (Place.mk 6 [])
    (Rvalue.use (Operand.copy readPlace))
    (Value.int ScalarTy.U8 (bytes.val.get ⟨i.val.toNat, h⟩))
    (loopAt 4 hash bytes i s4 s5 s6 s7) { frame := 1, slot := 6, path := [] }
    (loopAt 4 hash bytes i s4 s5
      (Slot.init (Value.int ScalarTy.U8 (bytes.val.get ⟨i.val.toNat, h⟩))) s7)
    (evalOperand_copy (loopAt 4 hash bytes i s4 s5 s6 s7) readPlace
      { frame := 0, slot := 0, path := [PathElem.index i.val.toNat] }
      (Value.int ScalarTy.U8 (bytes.val.get ⟨i.val.toNat, h⟩)) hp hv)
    rfl rfl

/-- Block 4's second statement: the byte as a `u64` is in local 7. -/
theorem cast_byte (hash : U64) (bytes : Slice U8) (i : Usize) (s4 s5 s7 : Slot) (b : U8) :
    execStmt P.adts
        (loopAt 4 hash bytes i s4 s5 (Slot.init (Value.int ScalarTy.U8 b)) s7) castStmt =
      Outcome.ok
        (loopAt 4 hash bytes i s4 s5 Slot.moved
          (Slot.init (Value.int ScalarTy.U64 (Scalar.cast ScalarTy.U64 b)))) :=
  execStmt_assign P.adts
    (loopAt 4 hash bytes i s4 s5 (Slot.init (Value.int ScalarTy.U8 b)) s7)
    (Place.mk 7 []) (Rvalue.cast (Operand.move (Place.mk 6 [])) ScalarTy.U64)
    (Value.int ScalarTy.U64 (Scalar.cast ScalarTy.U64 b))
    (loopAt 4 hash bytes i s4 s5 Slot.moved s7) { frame := 1, slot := 7, path := [] }
    (loopAt 4 hash bytes i s4 s5 Slot.moved
      (Slot.init (Value.int ScalarTy.U64 (Scalar.cast ScalarTy.U64 b))))
    (evalOne_ok
      (loopAt 4 hash bytes i s4 s5 (Slot.init (Value.int ScalarTy.U8 b)) s7)
      (Operand.move (Place.mk 6 [])) (castTo ScalarTy.U64)
      (Value.int ScalarTy.U8 b) (loopAt 4 hash bytes i s4 s5 Slot.moved s7)
      (Value.int ScalarTy.U64 (Scalar.cast ScalarTy.U64 b))
      rfl rfl)
    rfl rfl

/-- Block 4's third statement: `hash ^= c`. -/
theorem xor_byte (hash : U64) (bytes : Slice U8) (i : Usize) (s4 s5 : Slot) (c : U64) :
    execStmt P.adts
        (loopAt 4 hash bytes i s4 s5 Slot.moved (Slot.init (Value.int ScalarTy.U64 c)))
        xorStmt =
      Outcome.ok (loopAt 4 (Scalar.xor hash c) bytes i s4 s5 Slot.moved Slot.moved) :=
  execStmt_assign P.adts
    (loopAt 4 hash bytes i s4 s5 Slot.moved (Slot.init (Value.int ScalarTy.U64 c)))
    (Place.mk 1 [])
    (Rvalue.binary BinOp.bitXor (Operand.copy (Place.mk 1 [])) (Operand.move (Place.mk 7 [])))
    (Value.int ScalarTy.U64 (Scalar.xor hash c))
    (loopAt 4 hash bytes i s4 s5 Slot.moved Slot.moved) { frame := 1, slot := 1, path := [] }
    (loopAt 4 (Scalar.xor hash c) bytes i s4 s5 Slot.moved Slot.moved)
    (evalTwo_ok
      (loopAt 4 hash bytes i s4 s5 Slot.moved (Slot.init (Value.int ScalarTy.U64 c)))
      (Operand.copy (Place.mk 1 [])) (Operand.move (Place.mk 7 [])) (binaryOp false BinOp.bitXor)
      (Value.int ScalarTy.U64 hash)
      (loopAt 4 hash bytes i s4 s5 Slot.moved (Slot.init (Value.int ScalarTy.U64 c)))
      (Value.int ScalarTy.U64 c) (loopAt 4 hash bytes i s4 s5 Slot.moved Slot.moved)
      (Value.int ScalarTy.U64 (Scalar.xor hash c))
      rfl rfl
      ((binaryOp_same BinOp.bitXor hash c (by decide) (by decide)).trans rfl))
    rfl rfl

/-- Block 4's terminator: `hash = hash.wrapping_mul(prime)`, then block 5. -/
theorem mul_hash (hash : U64) (bytes : Slice U8) (i : Usize) (s4 s5 s6 s7 : Slot) :
    execBlockFrom P.adts P.sig (loopAt 4 hash bytes i s4 s5 s6 s7) [] mulTerm =
      Outcome.ok (Next.state
        (loopAt 5 (Scalar.wrapping_mul hash (1099511628211 : U64)) bytes i s4 s5 s6 s7)) :=
  rfl

/-- Block 4, `bytes[at]` in bounds: one byte of FNV-1a is in `hash`. -/
theorem step4 (hash : U64) (bytes : Slice U8) (i : Usize) (s4 s5 s6 s7 : Slot)
    (h : i.val.toNat < bytes.val.length) :
    step P (loopAt 4 hash bytes i s4 s5 s6 s7) =
      Outcome.ok (Next.state
        (loopAt 5 (BlazeGraph.Ledger.step hash (bytes.val.get ⟨i.val.toNat, h⟩)) bytes i
          s4 s5 Slot.moved Slot.moved)) := by
  have h0 := read_byte hash bytes i s4 s5 s6 s7 h
  have h1 := cast_byte hash bytes i s4 s5 s7 (bytes.val.get ⟨i.val.toNat, h⟩)
  have h2 := xor_byte hash bytes i s4 s5
    (Scalar.cast ScalarTy.U64 (bytes.val.get ⟨i.val.toNat, h⟩))
  have h3 := mul_hash
    (Scalar.xor hash (Scalar.cast ScalarTy.U64 (bytes.val.get ⟨i.val.toNat, h⟩)))
    bytes i s4 s5 Slot.moved Slot.moved
  show execBlockFrom P.adts P.sig (loopAt 4 hash bytes i s4 s5 s6 s7)
    [readStmt, castStmt, xorStmt] mulTerm = _
  exact (execBlockFrom_step P.adts P.sig _ readStmt [castStmt, xorStmt] mulTerm _ h0).trans
    ((execBlockFrom_step P.adts P.sig _ castStmt [xorStmt] mulTerm _ h1).trans
      ((execBlockFrom_step P.adts P.sig _ xorStmt [] mulTerm _ h2).trans h3))

def addStmt : Statement :=
  Statement.assign (Place.mk 3 [])
    (Rvalue.checked BinOp.add (Operand.copy (Place.mk 3 []))
      (Operand.const (Const.int ScalarTy.Usize 1)))

/-- Block 5's statement, `at + 1` in range: the counter is one more. -/
theorem add_one (hash : U64) (bytes : Slice U8) (i : Usize) (s4 s5 s6 s7 : Slot)
    (h : ScalarTy.min ScalarTy.Usize ≤ i.val + (1 : Usize).val ∧
      i.val + (1 : Usize).val ≤ ScalarTy.max ScalarTy.Usize) :
    execStmt P.adts (loopAt 5 hash bytes i s4 s5 s6 s7) addStmt =
      Outcome.ok (loopAt 5 hash bytes ⟨i.val + (1 : Usize).val, h.1, h.2⟩ s4 s5 s6 s7) :=
  execStmt_assign P.adts (loopAt 5 hash bytes i s4 s5 s6 s7) (Place.mk 3 [])
    (Rvalue.checked BinOp.add (Operand.copy (Place.mk 3 []))
      (Operand.const (Const.int ScalarTy.Usize 1)))
    (Value.int ScalarTy.Usize ⟨i.val + (1 : Usize).val, h.1, h.2⟩)
    (loopAt 5 hash bytes i s4 s5 s6 s7) { frame := 1, slot := 3, path := [] }
    (loopAt 5 hash bytes ⟨i.val + (1 : Usize).val, h.1, h.2⟩ s4 s5 s6 s7)
    (evalTwo_ok (loopAt 5 hash bytes i s4 s5 s6 s7) (Operand.copy (Place.mk 3 []))
      (Operand.const (Const.int ScalarTy.Usize 1)) (binaryOp true BinOp.add)
      (Value.int ScalarTy.Usize i) (loopAt 5 hash bytes i s4 s5 s6 s7)
      (Value.int ScalarTy.Usize (1 : Usize)) (loopAt 5 hash bytes i s4 s5 s6 s7)
      (Value.int ScalarTy.Usize ⟨i.val + (1 : Usize).val, h.1, h.2⟩)
      rfl rfl (binaryOp_checked_add i (1 : Usize) h))
    rfl rfl

/-- Block 5, `at + 1` in range: back to the loop's head. -/
theorem step5 (hash : U64) (bytes : Slice U8) (i : Usize) (s4 s5 s6 s7 : Slot)
    (h : ScalarTy.min ScalarTy.Usize ≤ i.val + (1 : Usize).val ∧
      i.val + (1 : Usize).val ≤ ScalarTy.max ScalarTy.Usize) :
    step P (loopAt 5 hash bytes i s4 s5 s6 s7) =
      Outcome.ok (Next.state
        (loopAt 1 hash bytes ⟨i.val + (1 : Usize).val, h.1, h.2⟩ s4 s5 s6 s7)) := by
  show (execStmt P.adts (loopAt 5 hash bytes i s4 s5 s6 s7) addStmt).bind
    (fun st1 => execBlockFrom P.adts P.sig st1 [] (Terminator.goto 1)) = _
  rw [add_one hash bytes i s4 s5 s6 s7 h]
  rfl

/-! The loop and the function. -/

/-- From the loop's head with `k` bytes left, `4 * k + 3` steps return FNV-1a's fold over
them. No step panics or is undefined. -/
theorem loop_runs (bytes : Slice U8) :
    ∀ (k : Nat) (hash : U64) (i : Usize) (s4 s5 s6 s7 : Slot) (extra : Nat),
      i.val.toNat + k = bytes.val.length →
      run P (4 * k + 3 + extra) (loopAt 1 hash bytes i s4 s5 s6 s7) =
        Outcome.ok (Value.int ScalarTy.U64
          (BlazeGraph.Ledger.fold hash (bytes.val.drop i.val.toNat))) := by
  intro k
  induction k with
  | zero =>
    intro hash i s4 s5 s6 s7 extra hk
    have hlen : (Slice.len bytes).val = (bytes.val.length : Int) := Slice.len_val bytes
    have h0 : (0 : Int) ≤ i.val := i.hmin
    have hlt : Scalar.lt i (Slice.len bytes) = false := by
      unfold Scalar.lt
      exact decide_eq_false (by omega)
    have e : 4 * 0 + 3 + extra = extra + 1 + 1 + 1 := by omega
    rw [Blaze.List.drop_of_length_le bytes.val i.val.toNat (by omega),
      BlazeGraph.Ledger.fold_nil, e]
    rw [run_step P (extra + 1 + 1) _ _ (step1 hash bytes i s4 s5 s6 s7)]
    rw [run_step P (extra + 1) _ _ (step2_false hash bytes i s5 s6 s7 hlt)]
    exact run_done P extra _ _ (step3 hash bytes i Slot.moved Slot.moved s6 s7)
  | succ k ih =>
    intro hash i s4 s5 s6 s7 extra hk
    have hlen : (Slice.len bytes).val = (bytes.val.length : Int) := Slice.len_val bytes
    have h0 : (0 : Int) ≤ i.val := i.hmin
    have hlt : Scalar.lt i (Slice.len bytes) = true := (Scalar.lt_iff _ _).mpr (by omega)
    have ⟨hidx, hadd, hnext⟩ := BlazeGraph.Ledger.in_bounds bytes i hlt
    have e : 4 * (k + 1) + 3 + extra = (4 * k + 3 + extra) + 1 + 1 + 1 + 1 := by omega
    rw [Blaze.List.drop_eq_get_cons bytes.val i.val.toNat hidx, BlazeGraph.Ledger.fold_cons,
      ← hnext, e]
    rw [run_step P (4 * k + 3 + extra + 1 + 1 + 1) _ _ (step1 hash bytes i s4 s5 s6 s7)]
    rw [run_step P (4 * k + 3 + extra + 1 + 1) _ _ (step2_true hash bytes i s5 s6 s7 hlt)]
    rw [run_step P (4 * k + 3 + extra + 1) _ _
      (step4 hash bytes i Slot.moved Slot.moved s6 s7 hidx)]
    rw [run_step P (4 * k + 3 + extra) _ _
      (step5 (BlazeGraph.Ledger.step hash (bytes.val.get ⟨i.val.toNat, hidx⟩)) bytes i
        Slot.moved Slot.moved Slot.moved Slot.moved hadd)]
    exact ih (BlazeGraph.Ledger.step hash (bytes.val.get ⟨i.val.toNat, hidx⟩))
      ⟨i.val + (1 : Usize).val, hadd.1, hadd.2⟩ Slot.moved Slot.moved Slot.moved Slot.moved
      extra (by
        show (i.val + (1 : Usize).val).toNat + k = bytes.val.length
        rw [hnext]
        omega)

/-- The MIR of `fnv1a_from`, run with `4 * len + 4` steps of fuel or more, returns FNV-1a's
fold of the seed over the bytes. -/
theorem fnv1a_from_runs (hash : U64) (bytes : Slice U8) (extra : Nat) :
    run P (4 * bytes.val.length + 4 + extra) (entry hash bytes) =
      Outcome.ok (Value.int ScalarTy.U64 (BlazeGraph.Ledger.fold hash bytes.val)) := by
  have e : 4 * bytes.val.length + 4 + extra = (4 * bytes.val.length + 3 + extra) + 1 := by
    omega
  rw [e, run_step P (4 * bytes.val.length + 3 + extra) _ _ (step0 hash bytes)]
  have h := loop_runs bytes bytes.val.length hash (0 : Usize) Slot.uninit Slot.uninit
    Slot.uninit Slot.uninit extra (by have := BlazeGraph.Ledger.zero_val; omega)
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
    have ends : run P (4 * bytes.val.length + 4 + 0) (entry hash bytes) ≠ Outcome.diverge := by
      rw [enough]
      intro h
      cases h
    have same := run_agree P (entry hash bytes) fuel (4 * bytes.val.length + 4 + 0) hd ends
    rw [enough] at same
    exact Or.inr same

/-- The chain's link (objective V7): the functional model of `fnv1a_from`
(`blaze_graph.ledger.fnv1a_from`, with fuel for every byte) and the semantics of its MIR
(with `4 * len + 4` steps or more) give the same outcome, for every seed and every slice. -/
theorem fnv1a_from_model_agrees (hash : U64) (bytes : Slice U8) (extra : Nat) :
    run P (4 * bytes.val.length + 4 + extra) (entry hash bytes) =
      Outcome.map (Value.int ScalarTy.U64)
        (Outcome.ofResult
          (blaze_graph.ledger.fnv1a_from (bytes.val.length + 1) hash bytes)) := by
  rw [fnv1a_from_runs hash bytes extra,
    BlazeGraph.Ledger.fnv1a_from_returns (bytes.val.length + 1) hash bytes (by omega)]
  rfl

end BlazeGraph.LedgerMir
