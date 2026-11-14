-- Hand-written, not generated: the proofs of the `traits` fixture's statements.
import Blaze.Auto
import BlazeModel.traits.shapes_agree
import BlazeModel.traits.provided_agree

/-!
The first proof over methods of trait impls (crates/hir-ty-lean/src/tests.rs, `TRAITS`;
docs/v2/plan/design/proofs-for-sans-io.md, section 3 step b). Each call reaches the impl's
method that static dispatch picks, a function named `<type>.<trait>.<method>`: a square and a
rectangle of the same side have the same size and corners, a default clock is at zero, a
square added to itself is the square of twice the side, and none of it panics.
-/

open Blaze Std.WP Lean.Order

theorem traits.shapes_agree.spec.proof : traits.shapes_agree.spec.statement := by
  intro side
  -- `side as u64` keeps the value: every `u32` is a `u64`.
  have hside : (Scalar.cast ScalarTy.U64 side).val = side.val :=
    Scalar.cast_val _ _ (by
      have := side.hmin
      have := side.hmax
      simp only [ScalarTy.min, ScalarTy.max] at *
      omega)
  blaze_auto [traits.shapes_agree, traits.by_add, traits.Square.Add.add, traits.by_method,
    traits.Square.Shape.size, traits.by_trait_path, traits.Tile.Shape.size, traits.by_type_path,
    traits.Square.Shape.corners, traits.by_qualified, traits.Tile.Shape.corners,
    traits.by_default, traits.Clock.Default.default, traits.by_eq, traits.Square.PartialEq.eq]

/-- A trait's provided methods called on its implementors are their bodies at each: `twice`
of a square and of a rectangle as wide is four times the side, `wide` is the default's
`width > 10` for a square and `Tile`'s own `true`, and a provided method returning `()` leaves
the value alone. -/
theorem traits.provided_agree.spec.proof : traits.provided_agree.spec.statement := by
  intro side
  have hside : (Scalar.cast ScalarTy.U64 side).val = side.val :=
    Scalar.cast_val _ _ (by
      have := side.hmin
      have := side.hmax
      simp only [ScalarTy.min, ScalarTy.max] at *
      omega)
  blaze_auto [traits.provided_agree, traits.by_provided, traits.Square.Measure.twice,
    traits.Tile.Measure.twice, traits.Square.Measure.width, traits.Tile.Measure.width,
    traits.by_provided_pure, traits.Square.Measure.wide, traits.Tile.Measure.wide,
    traits.by_provided_unit]
