import BlazeDiff.Rewrites

/-!
Every `fast_math` rewrite of the simplifier is an identity over the reals, under its
`requires`: the proofs of the statements `Rewrites.lean` generates from `rules.json`.

Each proof restates its goal over reals with `show` (the term's meaning unfolds to it by
definition) and closes it with `ring` or one named lemma. `rewrites_sound` gives
`Rewrite.AllSound` its one value: a rewrite added to `rules.json` adds a field, and this file
does not build until its proof is here.

These are claims about `ℝ`. In `f32` and `f64` a `fast_math` rewrite can change a result by
rounding, and at a NaN, an infinity or a signed zero: that is why it runs only when asked. The
exact rewrites claim equal bits instead; nothing is stated for them here, and the tests of
`crates/blaze-diff` hold them.
-/

noncomputable section

namespace BlazeDiff

theorem add_zero_sound : Rewrite.add_zero := by
  intro env a
  show a.eval env + 0 = a.eval env
  exact add_zero _

theorem mul_zero_sound : Rewrite.mul_zero := by
  intro env a
  show a.eval env * 0 = 0
  exact mul_zero _

theorem zero_div_sound : Rewrite.zero_div := by
  intro env a _
  show 0 / a.eval env = 0
  exact zero_div _

theorem sub_self_sound : Rewrite.sub_self := by
  intro env a
  show a.eval env - a.eval env = 0
  exact sub_self _

-- `div_self : a ≠ 0 → a / a = 1`
theorem div_self_sound : Rewrite.div_self := by
  intro env a h
  show a.eval env / a.eval env = 1
  exact div_self h

-- `mul_div_assoc : a * b / c = a * (b / c)`
theorem mul_div_cancel_sound : Rewrite.mul_div_cancel := by
  intro env a b h
  show a.eval env * b.eval env / b.eval env = a.eval env
  rw [mul_div_assoc, div_self h, mul_one]

-- `div_mul_eq_mul_div : a / b * c = a * c / b`
theorem div_mul_cancel_sound : Rewrite.div_mul_cancel := by
  intro env a b h
  show a.eval env / b.eval env * b.eval env = a.eval env
  rw [div_mul_eq_mul_div, mul_div_assoc, div_self h, mul_one]

theorem assoc_add_sound : Rewrite.assoc_add := by
  intro env a b c
  show a.eval env + b.eval env + c.eval env = a.eval env + (b.eval env + c.eval env)
  ring

theorem assoc_add_back_sound : Rewrite.assoc_add_back := by
  intro env a b c
  show a.eval env + (b.eval env + c.eval env) = a.eval env + b.eval env + c.eval env
  ring

theorem assoc_mul_sound : Rewrite.assoc_mul := by
  intro env a b c
  show a.eval env * b.eval env * c.eval env = a.eval env * (b.eval env * c.eval env)
  ring

theorem assoc_mul_back_sound : Rewrite.assoc_mul_back := by
  intro env a b c
  show a.eval env * (b.eval env * c.eval env) = a.eval env * b.eval env * c.eval env
  ring

theorem factor_sound : Rewrite.factor := by
  intro env a b c
  show a.eval env * c.eval env + b.eval env * c.eval env = (a.eval env + b.eval env) * c.eval env
  ring

theorem factor_sub_sound : Rewrite.factor_sub := by
  intro env a b c
  show a.eval env * c.eval env - b.eval env * c.eval env = (a.eval env - b.eval env) * c.eval env
  ring

-- `add_div : (a + b) / c = a / c + b / c`
theorem common_divisor_sound : Rewrite.common_divisor := by
  intro env a b c _
  show a.eval env / c.eval env + b.eval env / c.eval env = (a.eval env + b.eval env) / c.eval env
  exact (add_div _ _ _).symm

-- `sub_div : (a - b) / c = a / c - b / c`
theorem common_divisor_sub_sound : Rewrite.common_divisor_sub := by
  intro env a b c _
  show a.eval env / c.eval env - b.eval env / c.eval env = (a.eval env - b.eval env) / c.eval env
  exact (sub_div _ _ _).symm

theorem mul_of_quotient_sound : Rewrite.mul_of_quotient := by
  intro env a b c _
  show a.eval env * (b.eval env / c.eval env) = a.eval env * b.eval env / c.eval env
  exact (mul_div_assoc _ _ _).symm

-- `div_div : a / b / c = a / (b * c)`
theorem quotient_of_quotient_sound : Rewrite.quotient_of_quotient := by
  intro env a b c _ _
  show a.eval env / b.eval env / c.eval env = a.eval env / (b.eval env * c.eval env)
  exact div_div _ _ _

theorem sub_of_quotient_sound : Rewrite.sub_of_quotient := by
  intro env a b c h
  show a.eval env - b.eval env / c.eval env = (a.eval env * c.eval env - b.eval env) / c.eval env
  rw [sub_div, mul_div_assoc, div_self h, mul_one]

-- `mul_one_div : a * (1 / b) = a / b`
theorem mul_reciprocal_sound : Rewrite.mul_reciprocal := by
  intro env a b _
  show a.eval env * (1 / b.eval env) = a.eval env / b.eval env
  exact mul_one_div _ _

-- `Real.sin_sq_add_cos_sq (x) : Real.sin x ^ 2 + Real.cos x ^ 2 = 1`
theorem pythagoras_sound : Rewrite.pythagoras := by
  intro env a
  show Real.sin (a.eval env) * Real.sin (a.eval env) +
    Real.cos (a.eval env) * Real.cos (a.eval env) = 1
  nlinarith [Real.sin_sq_add_cos_sq (a.eval env)]

-- `Real.log_exp (x) : Real.log (Real.exp x) = x`
theorem ln_exp_sound : Rewrite.ln_exp := by
  intro env a
  show Real.log (Real.exp (a.eval env)) = a.eval env
  exact Real.log_exp _

-- `Real.exp_add (x y) : Real.exp (x + y) = Real.exp x * Real.exp y`
theorem exp_sum_sound : Rewrite.exp_sum := by
  intro env a b
  show Real.exp (a.eval env) * Real.exp (b.eval env) = Real.exp (a.eval env + b.eval env)
  exact (Real.exp_add _ _).symm

-- `Blaze.Scalar.lit_val ty x : ty.min ≤ x ∧ x ≤ ty.max → (lit ty x).val = x`
-- `zpow_two : a ^ (2 : ℤ) = a * a`, `zpow_one : a ^ (1 : ℤ) = a`, `zpow_zero : a ^ (0 : ℤ) = 1`
theorem powi_two_sound : Rewrite.powi_two := by
  intro env a
  show a.eval env ^ (Blaze.Scalar.lit .I32 2).val = a.eval env * a.eval env
  rw [Blaze.Scalar.lit_val .I32 2 (by decide)]
  exact zpow_two _

theorem powi_one_sound : Rewrite.powi_one := by
  intro env a
  show a.eval env ^ (Blaze.Scalar.lit .I32 1).val = a.eval env
  rw [Blaze.Scalar.lit_val .I32 1 (by decide)]
  exact zpow_one _

theorem powi_zero_sound : Rewrite.powi_zero := by
  intro env a
  show a.eval env ^ (Blaze.Scalar.lit .I32 0).val = 1
  rw [Blaze.Scalar.lit_val .I32 0 (by decide)]
  exact zpow_zero _

/-- `a * b + c` is `mul_add(a, b, c)` over the reals by definition: the difference is one
rounding, which the reals do not have. -/
theorem fma_sound : Rewrite.fma := by
  intro env a b c
  rfl

-- `neg_sub : -(a - b) = b - a`
theorem neg_of_difference_sound : Rewrite.neg_of_difference := by
  intro env a b
  show -(a.eval env - b.eval env) = b.eval env - a.eval env
  exact neg_sub _ _

-- `Real.mul_self_sqrt : 0 ≤ a → √a * √a = a`
theorem sqrt_squared_sound : Rewrite.sqrt_squared := by
  intro env a h
  show Real.sqrt (a.eval env) * Real.sqrt (a.eval env) = a.eval env
  exact Real.mul_self_sqrt h

/-- Every `fast_math` rewrite preserves a term's meaning over the reals. -/
theorem rewrites_sound : Rewrite.AllSound where
  add_zero := add_zero_sound
  mul_zero := mul_zero_sound
  zero_div := zero_div_sound
  sub_self := sub_self_sound
  div_self := div_self_sound
  mul_div_cancel := mul_div_cancel_sound
  div_mul_cancel := div_mul_cancel_sound
  assoc_add := assoc_add_sound
  assoc_add_back := assoc_add_back_sound
  assoc_mul := assoc_mul_sound
  assoc_mul_back := assoc_mul_back_sound
  factor := factor_sound
  factor_sub := factor_sub_sound
  common_divisor := common_divisor_sound
  common_divisor_sub := common_divisor_sub_sound
  mul_of_quotient := mul_of_quotient_sound
  quotient_of_quotient := quotient_of_quotient_sound
  sub_of_quotient := sub_of_quotient_sound
  mul_reciprocal := mul_reciprocal_sound
  pythagoras := pythagoras_sound
  ln_exp := ln_exp_sound
  exp_sum := exp_sum_sound
  powi_two := powi_two_sound
  powi_one := powi_one_sound
  powi_zero := powi_zero_sound
  fma := fma_sound
  neg_of_difference := neg_of_difference_sound
  sqrt_squared := sqrt_squared_sound

end BlazeDiff
