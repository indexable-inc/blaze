-- Hand-written, not generated: the proof of the `derives` fixture's statement.
import Blaze
import BlazeModel.derives.clone_is_equal

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
