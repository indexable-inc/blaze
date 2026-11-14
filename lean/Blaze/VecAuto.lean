import Blaze.Auto

/-!
The specifications `blaze_auto` uses for `Blaze.Vec`'s operations that can fail. They are
stated in `Result`'s weakest preconditions, which `Blaze.Auto` defines and which imports all of
`Blaze`, `Blaze.Vec` with it: so they are in a module of their own after it.
-/

open Std.WP Lean.Order

namespace Blaze

/-- `v.push(x)` succeeds with `x` after `v`'s elements when there are fewer than `usize::MAX`. -/
@[spec] theorem Vec.push_spec {α : Type} (v : Vec α) (x : α) {epost : Result.EPost} :
    Triple (Vec.push v x) (v.val.length < 18446744073709551615)
      (fun r => r.val = v.val ++ [x]) epost := by
  constructor
  intro h
  rw [Vec.push_ok v x h]
  exact rfl

end Blaze
