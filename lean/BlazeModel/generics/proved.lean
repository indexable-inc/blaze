-- Hand-written, not generated: the proofs of the `generics` fixture's statements.
import Blaze.Auto
import BlazeModel.generics.largest_is_at_least_both
import BlazeModel.generics.swap_twice_is_identity
import BlazeModel.generics.instances_agree

/-!
The first proofs over instances of generic functions, methods and impls
(crates/hir-ty-lean/src/tests.rs, `GENERICS`; docs/v2/plan/design/proofs-for-sans-io.md,
section 3, "Generics are translated by instance"). Each instance is its own function, the
generic body with its type arguments substituted: `largest` at `u32` is at least both its
arguments, swapping a `Pair<u32>` twice gives it back, and `pick` at `u32` and `u64`, `larger`
at `u32`, `largest` at the crate's `Ver` (whose `cmp` is the crate's `Ord`), the generic
`Shape` impl at `Pair<u32>`, `From<Small> for Big` and `Option<Pair<u32>>` each give what
Rust's semantics says.
-/

open Blaze

/-- `largest` at `u32` is one of its arguments, and not smaller than the other. -/
theorem generics.largest.u32.val (a b : U32) :
    a.val ≤ (generics.largest.u32 a b).val ∧ b.val ≤ (generics.largest.u32 a b).val := by
  unfold generics.largest.u32
  by_cases h : b.val < a.val
  · rw [ite_eq_left ((core.cmp.Ordering.is_gt_iff _).2 ((Scalar.cmp_eq_Greater a b).2 h))]
    omega
  · rw [ite_eq_right (fun hg => h ((Scalar.cmp_eq_Greater a b).1 ((core.cmp.Ordering.is_gt_iff _).1 hg)))]
    omega

theorem generics.largest_is_at_least_both.spec.proof :
    generics.largest_is_at_least_both.spec.statement := by
  intro a b
  have h := generics.largest.u32.val a b
  unfold generics.largest_is_at_least_both
  simp [h.1, h.2]

theorem generics.swap_twice_is_identity.spec.proof :
    generics.swap_twice_is_identity.spec.statement := by
  intro a b
  simp [generics.swap_twice_is_identity, generics.Pair.u32.swap, generics.Pair.u32.new]

theorem generics.instances_agree.spec.proof : generics.instances_agree.spec.statement := by
  intro a b s
  -- `s as u32` keeps the value: every `u8` is a `u32`.
  have hcast : (Scalar.cast ScalarTy.U32 s).val = s.val :=
    Scalar.cast_val _ _ (by
      have := s.hmin
      have := s.hmax
      simp only [ScalarTy.min, ScalarTy.max] at *
      omega)
  have ha := a.hmin
  simp only [ScalarTy.min] at ha
  -- `largest` at `Ver` compares the majors with the crate's `Ord`.
  have hver : (generics.largest.Ver ({ major := a } : generics.Ver)
      ({ major := (0 : U32) } : generics.Ver)).major.val = a.val := by
    unfold generics.largest.Ver generics.Ver.Ord.cmp
    by_cases h : (0 : U32).val < a.val
    · rw [ite_eq_left ((core.cmp.Ordering.is_gt_iff _).2 ((Scalar.cmp_eq_Greater _ _).2 h))]
    · rw [ite_eq_right (fun hg => h ((Scalar.cmp_eq_Greater _ _).1 ((core.cmp.Ordering.is_gt_iff _).1 hg)))]
      show (0 : U32).val = a.val
      have : (0 : U32).val = 0 := rfl
      omega
  -- The generic `Shape` impl at `Pair<u32>`: a pair of equal halves is ordered.
  have hshape : generics.Pair.u32.Shape.ordered (generics.Pair.u32.new a a) = true := by
    unfold generics.Pair.u32.Shape.ordered generics.Pair.u32.new
    rw [core.cmp.Ordering.is_le_iff, (Scalar.cmp_eq_Equal a a).2 rfl]
    decide
  have h1 : Scalar.eq (generics.pick.u32 a (0 : U32) true) a = true := by
    simp [generics.pick.u32]
  have h2 : Scalar.eq (generics.pick.u64 b (0 : U64) false) (0 : U64) = true := by
    simp [generics.pick.u64]
  have h3 : Scalar.eq (generics.larger.u32 a a) a = true := by
    unfold generics.larger.u32
    rw [ite_eq_right (by rw [Scalar.gt_iff]; omega)]
    simp
  have h4 : Scalar.eq (generics.largest.Ver ({ major := a } : generics.Ver)
      ({ major := (0 : U32) } : generics.Ver)).major a = true := by
    rw [Scalar.eq_true_eq]
    exact hver
  have h6 : Scalar.eq (generics.widen ({ v := s } : generics.Small)).v (Scalar.cast .U32 s) = true := by
    simp [generics.widen, generics.Big.From.Small.«from»]
  have h7 : Scalar.eq (generics.first_or (Option.some (generics.Pair.u32.new a (Scalar.cast .U32 b))
      : Option generics.Pair.u32) (0 : U32)) a = true := by
    simp [generics.first_or, generics.Pair.u32.first, generics.Pair.u32.new]
  have h8 : Scalar.eq (generics.first_or (Option.none : Option generics.Pair.u32) a) a = true := by
    simp [generics.first_or]
  unfold generics.instances_agree
  simp [h1, h2, h3, h4, hshape, h6, h7, h8]
