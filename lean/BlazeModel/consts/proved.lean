-- Hand-written, not generated: the proof of the `consts` fixture's statement.
import Blaze
import BlazeModel.consts.constants_are_their_values

/-!
Constants read by name (crates/hir-ty-lean/src/tests.rs, `CONSTS`) are their values by const
evaluation at the instance: a trait's constant at an implementor (its own, or the trait's
default computed from the implementor's), by path, through `Self::` and through a generic
function's instance; an inherent one; and a `const { .. }` block.
-/

open Blaze

theorem consts.constants_are_their_values.spec.proof :
    consts.constants_are_their_values.spec.statement := by
  unfold consts.constants_are_their_values.spec.statement
  decide
