-- Hand-written, not generated: the proofs of the `derives` fixture's statements.
import Blaze
import BlazeModel.derives.clone_is_equal
import BlazeModel.derives.waited_eq_is_the_values
import BlazeModel.derives.tagged_eq_is_its_id
import BlazeModel.derives.point_and_tag_eq

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

/-- `PhantomData<T>` is a field with one value: two `Tagged<Point>`s are equal exactly when their
ids are, whatever `PhantomData` each holds (here one taken out of another value), and a clone
and a copy of one are equal. -/
theorem derives.tagged_eq_is_its_id.spec.proof : derives.tagged_eq_is_its_id.spec.statement := by
  intro a b
  have h : a = b ↔ a.val = b.val := ⟨fun h => h ▸ rfl, Scalar.ext⟩
  simp [derives.tagged_eq_is_its_id, derives.same_tagged, derives.tag, derives.clone_tagged,
    derives.copy_tagged, derives.marker_of, core.cmp.PartialEq.eq, Scalar.eq, h]

/-- `derives.Point` and `derives.Tagged.Point`, two types alike in their last name, compared in
one model: each one's `DecidableEq` is its own (in its type's namespace), and a value of each is
equal to itself. -/
theorem derives.point_and_tag_eq.spec.proof : derives.point_and_tag_eq.spec.statement := by
  intro x y a
  simp [derives.point_and_tag_eq, derives.same_point_and_tag, derives.tag, core.cmp.PartialEq.eq]
