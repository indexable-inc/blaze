import Blaze
import Blaze.Mir.Walk
import BlazeModel.arith.add_one
import BlazeMir.arith.add_one

/-!
`arith::add_one`: the functional model (`BlazeModel.arith.add_one`) agrees with the semantics of
the function's MIR (`BlazeMir.arith.add_one`), for every argument: objective V7
(docs/v2/plan/design/formal-verification.md). `mir_walk` (`Blaze.Mir.Walk`) runs the one block
up to the checked addition, whose outcome it cannot decide for an `x` it does not know; the
model is that same addition, so a case on its result closes both sides at once. No state
between statements is written down, so a change of the body's temporaries or storage
markers changes only `at_`.
-/

open Blaze Blaze.Mir

namespace BlazeAgrees.arith.add_one

abbrev P : Program := arith.add_one.mir

/-- The one frame: what local 0 holds, `x` (local 1), and the temporary (local 2). -/
def at_ (s0 : Slot) (x : U32) : State :=
  { base := [],
    stack :=
      [({ id := 1, fn := 0, block := 0,
          locals := [s0, Slot.init (Value.int ScalarTy.U32 x), Slot.uninit],
          dest := { frame := 0, slot := 0, path := [] }, target := none } : Frame)],
    next := 2 }

/-- The state a call starts in. -/
theorem start_entry (x : U32) :
    P.start 0 [] [Value.int ScalarTy.U32 x] = Outcome.ok (at_ Slot.uninit x) := rfl

/-- The model of `add_one` and the semantics of its MIR give the same outcome, a value or
the overflow panic, for every `x` and one step of fuel or more. -/
theorem agrees (x : U32) (extra : Nat) :
    run P (extra + 1) (at_ Slot.uninit x) =
      Outcome.map (Value.int ScalarTy.U32) (Outcome.ofResult (arith.add_one x)) := by
  have e : arith.add_one x = Scalar.add x (Scalar.lit ScalarTy.U32 1) := rfl
  rw [e]
  mir_walk [P, arith.add_one.mir, at_]
  cases Scalar.add x (Scalar.lit ScalarTy.U32 1) <;> rfl

end BlazeAgrees.arith.add_one
