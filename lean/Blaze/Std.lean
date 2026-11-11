import Blaze.Result

/-!
The model of core's types that are not Lean's own. Each is named by its Rust path under
`Blaze`. `core::option::Option` is Lean's `Option`.
-/

namespace Blaze.core.result

/-- `core::result::Result`. -/
inductive Result (T E : Type) : Type where
  | Ok (f0 : T) : Result T E
  | Err (f0 : E) : Result T E

end Blaze.core.result
