import BlazeDiff.Correct

/-!
What is promised at a point without a derivative, where `diff_correct` says nothing.

With `Choice::Error` (the default) the Rust differentiator refuses such a point when the
variable reaches it. With `Choice::Subgradient(c)` the rule's tangent there is what `diff.rs`
documents for `Choice`, and each is proved here of the generated template:

- `abs` at 0: `c` times the argument's derivative, and any `c` in `[-1, 1]` is a subgradient
  of `|·|` at 0.
- `min` and `max` at a tie: `c` of the first operand's derivative and `1 - c` of the second's,
  and for `c` in `[0, 1]` that weighting is a supergradient of `min`, a subgradient of `max`.
- `sqrt` at 0: `c` times the argument's derivative. `sqrt` has no subgradient at 0 (its slope
  is unbounded): the value is a convention, and nothing more is claimed.
- `select`: the taken arm's derivative, whatever `c` is.
-/

noncomputable section

namespace BlazeDiff

/-- `abs` at 0: the tangent is the argument's derivative times the choice. -/
theorem abs_tangent_at_kink (c a1 a2 t0 t1 t2 o : ℝ) (n : Blaze.I32) (p : Prop) :
    P.evalR
      { a0 := 0, a1 := a1, a2 := a2, t0 := t0, t1 := t1, t2 := t2, out := o, choice := c,
        exponent := n, cond := p }
      (Rules.tangent .abs) = t0 * c := by
  show t0 * pick ((0 : ℝ) < 0) 1 (pick ((0 : ℝ) < 0) (-1) c) = t0 * c
  rw [pick_neg (lt_irrefl (0 : ℝ)), pick_neg (lt_irrefl (0 : ℝ))]

-- `abs_of_nonneg : 0 ≤ a → |a| = a`, `abs_of_nonpos : a ≤ 0 → |a| = -a`
/-- A choice in `[-1, 1]` is a subgradient of `|·|` at 0: the line through 0 with that slope
is nowhere above it. -/
theorem abs_choice_is_subgradient (c : ℝ) (h1 : -1 ≤ c) (h2 : c ≤ 1) (y : ℝ) :
    |(0 : ℝ)| + c * (y - 0) ≤ |y| := by
  rw [abs_zero, zero_add, sub_zero]
  rcases le_total 0 y with hy | hy
  · rw [abs_of_nonneg hy]
    nlinarith [mul_nonneg (sub_nonneg.mpr h2) hy]
  · rw [abs_of_nonpos hy]
    nlinarith [mul_nonneg (by linarith : (0 : ℝ) ≤ c + 1) (neg_nonneg.mpr hy)]

/-- `min` and `max` at a tie: the tangent weighs the operands' derivatives by the choice. -/
theorem min_tangent_at_tie (c a a2 t0 t1 t2 o : ℝ) (n : Blaze.I32) (p : Prop) :
    P.evalR
      { a0 := a, a1 := a, a2 := a2, t0 := t0, t1 := t1, t2 := t2, out := o, choice := c,
        exponent := n, cond := p }
      (Rules.tangent .min) = c * t0 + (1 - c) * t1 := by
  show pick (a < a) t0 (pick (a < a) t1 (c * t0 + (1 - c) * t1)) = c * t0 + (1 - c) * t1
  rw [pick_neg (lt_irrefl a), pick_neg (lt_irrefl a)]

theorem max_tangent_at_tie (c a a2 t0 t1 t2 o : ℝ) (n : Blaze.I32) (p : Prop) :
    P.evalR
      { a0 := a, a1 := a, a2 := a2, t0 := t0, t1 := t1, t2 := t2, out := o, choice := c,
        exponent := n, cond := p }
      (Rules.tangent .max) = c * t0 + (1 - c) * t1 := by
  show pick (a < a) t0 (pick (a < a) t1 (c * t0 + (1 - c) * t1)) = c * t0 + (1 - c) * t1
  rw [pick_neg (lt_irrefl a), pick_neg (lt_irrefl a)]

-- `le_max_left a b : a ≤ max a b`, `le_max_right a b : b ≤ max a b`
/-- A choice in `[0, 1]` is a subgradient of `max` at a tie `(a, a)`: the plane through the
tie with slopes `(c, 1 - c)` is nowhere above it. -/
theorem max_choice_is_subgradient (c : ℝ) (h0 : 0 ≤ c) (h1 : c ≤ 1) (a u v : ℝ) :
    max a a + (c * (u - a) + (1 - c) * (v - a)) ≤ max u v := by
  rw [max_self]
  nlinarith [mul_nonneg h0 (sub_nonneg.mpr (le_max_left u v)),
    mul_nonneg (sub_nonneg.mpr h1) (sub_nonneg.mpr (le_max_right u v))]

-- `min_le_left a b : min a b ≤ a`, `min_le_right a b : min a b ≤ b`
/-- A choice in `[0, 1]` is a supergradient of `min` at a tie: the plane is nowhere below. -/
theorem min_choice_is_supergradient (c : ℝ) (h0 : 0 ≤ c) (h1 : c ≤ 1) (a u v : ℝ) :
    min u v ≤ min a a + (c * (u - a) + (1 - c) * (v - a)) := by
  rw [min_self]
  nlinarith [mul_nonneg h0 (sub_nonneg.mpr (min_le_left u v)),
    mul_nonneg (sub_nonneg.mpr h1) (sub_nonneg.mpr (min_le_right u v))]

/-- `sqrt` at 0: the tangent is the choice times the argument's derivative. -/
theorem sqrt_tangent_at_kink (c a1 a2 t0 t1 t2 o : ℝ) (n : Blaze.I32) (p : Prop) :
    P.evalR
      { a0 := 0, a1 := a1, a2 := a2, t0 := t0, t1 := t1, t2 := t2, out := o, choice := c,
        exponent := n, cond := p }
      (Rules.tangent .sqrt) = c * t0 := by
  show pick ((0 : ℝ) = 0) (c * t0) (t0 / (2 * o)) = c * t0
  rw [pick_pos rfl]

/-- `select`: the tangent is the taken arm's derivative, whatever the choice. -/
theorem select_tangent (c a0 a1 a2 t0 t1 t2 o : ℝ) (n : Blaze.I32) (p : Prop) :
    P.evalR
      { a0 := a0, a1 := a1, a2 := a2, t0 := t0, t1 := t1, t2 := t2, out := o, choice := c,
        exponent := n, cond := p }
      (Rules.tangent .select) = pick p t1 t2 :=
  rfl

end BlazeDiff
