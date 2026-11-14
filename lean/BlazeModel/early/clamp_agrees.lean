import Blaze
import Blaze.Mir.Walk
import BlazeModel.early.clamp
import BlazeMir.early.clamp

/-!
`early::clamp`: the functional model (`BlazeModel.early.clamp`) agrees with the semantics of the
function's MIR (`BlazeMir.early.clamp`), for every argument: objective V7
(docs/v2/plan/design/formal-verification.md). A case on `x > limit`, then `mir_walk`
(`Blaze.Mir.Walk`) runs the body down the branch taken: three blocks when `x > limit` (the
early return goes through the return block), two otherwise. No state between two blocks is
written down, so a change of the body's temporaries or storage markers changes only `at_`.
-/

open Blaze Blaze.Mir

namespace BlazeAgrees.early.clamp

set_option maxRecDepth 8192

abbrev P : Program := early.clamp.mir

/-- The one frame: the block to run, `x` and `limit` (locals 1 and 2), and what locals 0
and 3 hold; the other temporaries (locals 4 to 8) hold nothing. -/
def at_ (block : BlockId) (s0 s3 : Slot) (x limit : U32) : State :=
  { base := [],
    stack :=
      [({ id := 1, fn := 0, block := block,
          locals :=
            [s0, Slot.init (Value.int ScalarTy.U32 x),
             Slot.init (Value.int ScalarTy.U32 limit), s3, Slot.uninit, Slot.uninit,
             Slot.uninit, Slot.uninit, Slot.uninit],
          dest := { frame := 0, slot := 0, path := [] }, target := none } : Frame)],
    next := 2 }

/-- The state a call starts in. -/
theorem start_entry (x limit : U32) :
    P.start 0 [] [Value.int ScalarTy.U32 x, Value.int ScalarTy.U32 limit] =
      Outcome.ok (at_ 0 Slot.uninit Slot.uninit x limit) := rfl

/-- The model of `clamp` is what the semantics of its MIR returns, for every `x` and
`limit` and three steps of fuel or more. -/
theorem agrees (x limit : U32) (extra : Nat) :
    run P (extra + 1 + 1 + 1) (at_ 0 Slot.uninit Slot.uninit x limit) =
      Outcome.ok (Value.int ScalarTy.U32 (early.clamp x limit)) := by
  unfold early.clamp
  cases hgt : Scalar.gt x limit <;> mir_walk [P, early.clamp.mir, at_, hgt]

/-- Three steps is the least fuel that is enough: with two, the early return is out of
fuel. -/
example : run P 2 (at_ 0 Slot.uninit Slot.uninit (1 : U32) (0 : U32)) = Outcome.diverge := rfl

end BlazeAgrees.early.clamp
