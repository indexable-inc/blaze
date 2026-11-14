-- Hand-written, not generated: the proofs of the `derives` fixture's statements.
import Blaze
import BlazeModel.derives.clone_is_equal
import BlazeModel.derives.waited_eq_is_the_values

/-!
Derived `Clone` and `PartialEq` proved through their models (crates/hir-ty-lean/src/tests.rs,
`DERIVES`): a derived `clone` is the value itself, and a derived `==` is equality decided by the
`DecidableEq` instance each type's `impl` module derives, so `x.clone() == x` for a struct
holding a `Vec` and an `Option` of the crate's own types, and for an enum; `!=` is its negation.
-/

open Blaze

theorem derives.clone_is_equal.spec.proof : derives.clone_is_equal.spec.statement := by
  intro p s
  simp [derives.clone_is_equal, core.cmp.PartialEq.ne]

/-- The same at instances of a generic enum: `Waited<()>`, `Waited<u32>` and `Waited<Point>`
each have their own type and `DecidableEq`, so `==` and `!=` are equality of the values and
`clone` the value itself, whatever `x` is. -/
theorem derives.waited_eq_is_the_values.spec.proof :
    derives.waited_eq_is_the_values.spec.statement := by
  intro x
  simp [derives.waited_eq_is_the_values, derives.is_ready, derives.waited_differ,
    derives.copy_waited, derives.ready_or, core.cmp.PartialEq.ne, core.cmp.PartialEq.eq]
  rfl
