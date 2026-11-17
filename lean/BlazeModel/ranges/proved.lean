-- Hand-written, not generated: the proofs of the `ranges` fixture's statements.
import Blaze
import BlazeModel.ranges.contains_at_the_ends
import BlazeModel.ranges.a_subslice_is_its_part

/-!
`Range` and `RangeInclusive` as values, and a slice cut by a range, proved through their models
(crates/hir-ty-lean/src/tests.rs, `RANGES`; `lean/Blaze/Range.lean`). A `start..end` holds its
start exactly when it is not empty and never holds its end; a `start..=end` holds both ends
exactly when `start <= end`. A subslice `&v[a..b]` in bounds has `b - a` elements, the first of
which is `v[a]`, and taking it does not panic. A model that made `Range`'s end inclusive, or cut
from the wrong place, fails these.
-/

open Blaze

theorem ranges.contains_at_the_ends.spec.proof : ranges.contains_at_the_ends.spec.statement := by
  intro a b
  simp only [ranges.contains_at_the_ends, ranges.in_span, ranges.in_closed, ranges.span,
    ranges.closed, ranges.span_start, ranges.closed_end, core.ops.Range.new,
    core.ops.RangeInclusive.new, core.ops.Range.contains, core.ops.RangeInclusive.contains,
    Scalar.lt, Scalar.le, Scalar.eq]
  by_cases h1 : a.val < b.val <;> by_cases h2 : a.val ≤ b.val <;> simp [h1, h2] <;> omega

theorem ranges.a_subslice_is_its_part.spec.proof : ranges.a_subslice_is_its_part.spec.statement := by
  intro v a b
  have ha0 : 0 ≤ a.val := a.hmin
  have h0 : (0 : Usize).val = 0 := Scalar.ofNat_val .Usize 0 (by simp [ScalarTy.min, ScalarTy.max])
  unfold ranges.a_subslice_is_its_part
  dsimp only
  by_cases hin : a.val < b.val ∧ b.val ≤ (v.val.length : Int)
  · obtain ⟨hab, hbv⟩ := hin
    have hsub : ScalarTy.Usize.min ≤ b.val - a.val ∧ b.val - a.val ≤ ScalarTy.Usize.max := by
      have := b.hmax
      simp only [ScalarTy.min, ScalarTy.max] at this ⊢; omega
    have hok : a.val ≤ b.val ∧ b.val.toNat ≤ v.val.length := ⟨by omega, by omega⟩
    have hlen := Slice.cut_length v a.val.toNat b.val.toNat hok.2
    have hi0 : (0 : Usize).val.toNat < (Slice.cut v a.val.toNat b.val.toNat).val.length := by
      rw [hlen, h0]; omega
    have hia : a.val.toNat < v.val.length := by omega
    have helem := Slice.cut_getElem? v a.val.toNat b.val.toNat 0 (by omega)
    rw [List.getElem?_eq_getElem (by omega), List.getElem?_eq_getElem (by omega)] at helem
    have hget : (Slice.cut v a.val.toNat b.val.toNat).val.get ⟨(0 : Usize).val.toNat, hi0⟩ =
        v.val.get ⟨a.val.toNat, hia⟩ := by
      simp only [List.get_eq_getElem, h0]
      simpa using helem
    have hlenv : (Slice.len (Slice.cut v a.val.toNat b.val.toNat)).val = b.val - a.val := by
      rw [Slice.len_val, hlen]; omega
    have hc : (if Scalar.lt a b = true then Scalar.le b (Slice.len v) else Scalar.lt a b) = true := by
      simp [Scalar.lt, Scalar.le, Slice.len_val, hab, hbv]
    have hp : ranges.part v a b = Result.ok (Slice.cut v a.val.toNat b.val.toNat) := by
      simp only [ranges.part, Slice.index_range, core.ops.Range.new]
      exact Slice.subslice_ok v a b hok
    have heq : Scalar.eq (Slice.len (Slice.cut v a.val.toNat b.val.toNat))
        ⟨b.val - a.val, hsub.1, hsub.2⟩ = true := by
      simp [Scalar.eq, hlenv]
    rw [hc, ite_eq_left rfl]
    show (ranges.part v a b >>= _) = _
    rw [hp, Result.bind_ok]
    show (Scalar.sub b a >>= _) = _
    rw [Scalar.sub_ok b a hsub, Result.bind_ok, heq, ite_eq_left rfl]
    show (Slice.index _ 0 >>= _) = _
    rw [Slice.index_ok _ _ hi0, Result.bind_ok]
    show (Slice.index v a >>= _) = _
    rw [Slice.index_ok _ _ hia, Result.bind_ok, hget]
    simp [Scalar.eq]
  · have hc : (if Scalar.lt a b = true then Scalar.le b (Slice.len v) else Scalar.lt a b) = false := by
      by_cases h1 : a.val < b.val
      · have h2 : ¬ b.val ≤ (v.val.length : Int) := fun h2 => hin ⟨h1, h2⟩
        simp [Scalar.lt, Scalar.le, Slice.len_val, h1, h2]
      · simp [Scalar.lt, h1]
    rw [hc]
    rfl
