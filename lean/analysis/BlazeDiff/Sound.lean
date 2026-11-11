import BlazeDiff.Diff

/-!
Each derivative rule is sound: given the arguments' derivatives, the rule's tangent is the
operation's derivative, on the rule's domain. One lemma per operation, each about real
functions alone and each one Mathlib lemma away, so one that fails says which rule (or which
Mathlib name) is wrong and hides no other.

Each statement is written in the unfolded form of the generated template (`Rules.tangent op`
evaluated by `P.evalR`); `Correct.lean` checks, by definitional unfolding, that it is that
template. A rule changed in `rules.json` therefore stops `Correct.lean` from building until
its lemma here is changed and proved again.

The Mathlib statements each proof expects are written beside it: a name that moved is one line
to fix.
-/

noncomputable section

namespace BlazeDiff

variable {f g k : ℝ → ℝ} {f' g' k' x : ℝ}

/-- Two differentiable functions apart at a point are apart near it.
`ContinuousAt.eventually_lt : ContinuousAt f x → ContinuousAt g x → f x < g x →
∀ᶠ y in 𝓝 x, f y < g y`; `HasDerivAt.continuousAt : HasDerivAt f f' x → ContinuousAt f x`. -/
theorem eventually_lt_of_hasDerivAt (hf : HasDerivAt f f' x) (hg : HasDerivAt g g' x)
    (h : f x < g x) : ∀ᶠ y in nhds x, f y < g y :=
  hf.continuousAt.eventually_lt hg.continuousAt h

-- `HasDerivAt.add : HasDerivAt f f' x → HasDerivAt g g' x →
--   HasDerivAt (fun x => f x + g x) (f' + g') x`
theorem rule_sound_add (hf : HasDerivAt f f' x) (hg : HasDerivAt g g' x) :
    HasDerivAt (fun t => f t + g t) (f' + g') x :=
  hf.add hg

-- `HasDerivAt.sub : ... HasDerivAt (fun x => f x - g x) (f' - g') x`
theorem rule_sound_sub (hf : HasDerivAt f f' x) (hg : HasDerivAt g g' x) :
    HasDerivAt (fun t => f t - g t) (f' - g') x :=
  hf.sub hg

-- `HasDerivAt.mul : ... HasDerivAt (fun y => f y * g y) (f' * g x + f x * g') x`
theorem rule_sound_mul (hf : HasDerivAt f f' x) (hg : HasDerivAt g g' x) :
    HasDerivAt (fun t => f t * g t) (f' * g x + f x * g') x :=
  hf.mul hg

-- `HasDerivAt.div : HasDerivAt c c' x → HasDerivAt d d' x → d x ≠ 0 →
--   HasDerivAt (fun y => c y / d y) ((c' * d x - c x * d') / d x ^ 2) x`
-- `HasDerivAt.congr_deriv : HasDerivAt f f' x → f' = g' → HasDerivAt f g' x`
theorem rule_sound_div (hf : HasDerivAt f f' x) (hg : HasDerivAt g g' x) (hd : g x ≠ 0) :
    HasDerivAt (fun t => f t / g t) ((f' - f x / g x * g') / g x) x := by
  have h := hf.div hg hd
  -- `div_mul_eq_mul_div : a / b * c = a * c / b`, `mul_div_assoc : a * b / c = a * (b / c)`
  have hb : f x / g x * g x = f x := by
    rw [div_mul_eq_mul_div, mul_div_assoc, div_self hd, mul_one]
  -- `div_eq_div_iff : b ≠ 0 → d ≠ 0 → (a / b = c / d ↔ a * d = c * b)`
  have e : (f' * g x - f x * g') / g x ^ 2 = (f' - f x / g x * g') / g x := by
    rw [div_eq_div_iff (pow_ne_zero 2 hd) hd]
    linear_combination (g' * g x) * hb
  exact h.congr_deriv e

-- `HasDerivAt.neg : HasDerivAt f f' x → HasDerivAt (fun x => -f x) (-f') x`
theorem rule_sound_neg (hf : HasDerivAt f f' x) : HasDerivAt (fun t => -f t) (-f') x :=
  hf.neg

-- `hasDerivAt_zpow (m : ℤ) (x : 𝕜) (h : x ≠ 0 ∨ 0 ≤ m) :
--   HasDerivAt (fun x => x ^ m) (↑m * x ^ (m - 1)) x`
-- `HasDerivAt.comp (x) : HasDerivAt h₂ h₂' (h x) → HasDerivAt h h' x →
--   HasDerivAt (h₂ ∘ h) (h₂' * h') x`
-- `zpow_sub_one₀ : a ≠ 0 → ∀ n : ℤ, a ^ (n - 1) = a ^ n * a⁻¹`
-- `Blaze.Scalar.wrap_val ty x : ty.min ≤ x ∧ x ≤ ty.max → (wrap ty x).val = x`
/-- `powi`, for every `i32` exponent: one below `i32::MIN` is no `i32`, and there the rule's
`powi_down` is the node over its argument. -/
theorem rule_sound_powi (n : Blaze.I32) (hf : HasDerivAt f f' x) (hd : 1 ≤ n.val ∨ f x ≠ 0) :
    HasDerivAt (fun t => f t ^ n.val)
      (f' * ((n.val : ℝ) *
        (if n.val = Blaze.ScalarTy.min .I32 then f x ^ n.val / f x
          else f x ^ (Blaze.Scalar.wrap .I32 (n.val - 1)).val))) x := by
  have hz : f x ≠ 0 ∨ 0 ≤ n.val := hd.elim (fun h1 => Or.inr (by omega)) Or.inl
  have h := (hasDerivAt_zpow n.val (f x) hz).comp x hf
  have hmin : (-2147483648 : Int) ≤ n.val := n.hmin
  have hmax : n.val ≤ (2147483647 : Int) := n.hmax
  have key : (if n.val = Blaze.ScalarTy.min .I32 then f x ^ n.val / f x
      else f x ^ (Blaze.Scalar.wrap .I32 (n.val - 1)).val) = f x ^ (n.val - 1) := by
    by_cases hn : n.val = Blaze.ScalarTy.min .I32
    · rw [if_pos hn]
      have hn' : n.val = (-2147483648 : Int) := hn
      have hx : f x ≠ 0 := by
        rcases hd with h1 | h1
        · exact absurd h1 (by omega)
        · exact h1
      rw [zpow_sub_one₀ hx, div_eq_mul_inv]
    · rw [if_neg hn]
      have hn' : n.val ≠ (-2147483648 : Int) := hn
      have hw : (Blaze.Scalar.wrap .I32 (n.val - 1)).val = n.val - 1 :=
        Blaze.Scalar.wrap_val .I32 (n.val - 1)
          ⟨by show (-2147483648 : Int) ≤ n.val - 1; omega,
            by show n.val - 1 ≤ (2147483647 : Int); omega⟩
      rw [hw]
  rw [key]
  exact h.congr_deriv (by ring)

-- `HasDerivAt.rpow : HasDerivAt f f' x → HasDerivAt g g' x → 0 < f x →
--   HasDerivAt (fun x => f x ^ g x)
--     (f' * g x * f x ^ (g x - 1) + g' * f x ^ g x * Real.log (f x)) x`
theorem rule_sound_powf (hf : HasDerivAt f f' x) (hg : HasDerivAt g g' x) (hd : 0 < f x) :
    HasDerivAt (fun t => f t ^ g t)
      (f' * (g x * f x ^ (g x - 1)) + g' * (f x ^ g x * Real.log (f x))) x := by
  have h := hf.rpow hg hd
  exact h.congr_deriv (by ring)

-- `HasDerivAt.sqrt : HasDerivAt f f' x → f x ≠ 0 →
--   HasDerivAt (fun y => √(f y)) (f' / (2 * √(f x))) x`
/-- `sqrt` above 0: the rule's `select` on `a0 == 0` takes its second arm. -/
theorem rule_sound_sqrt (c : ℝ) (hf : HasDerivAt f f' x) (hd : 0 < f x) :
    HasDerivAt (fun t => Real.sqrt (f t))
      (pick (f x = 0) (c * f') (f' / (2 * Real.sqrt (f x)))) x := by
  rw [pick_neg hd.ne']
  exact hf.sqrt hd.ne'

-- `HasDerivAt.exp : HasDerivAt f f' x → HasDerivAt (fun x => Real.exp (f x)) (Real.exp (f x) * f') x`
theorem rule_sound_exp (hf : HasDerivAt f f' x) :
    HasDerivAt (fun t => Real.exp (f t)) (f' * Real.exp (f x)) x :=
  hf.exp.congr_deriv (mul_comm _ _)

-- `HasDerivAt.log : HasDerivAt f f' x → f x ≠ 0 → HasDerivAt (fun y => Real.log (f y)) (f' / f x) x`
theorem rule_sound_ln (hf : HasDerivAt f f' x) (hd : 0 < f x) :
    HasDerivAt (fun t => Real.log (f t)) (f' / f x) x :=
  hf.log hd.ne'

-- `HasDerivAt.sin : HasDerivAt f f' x → HasDerivAt (fun x => Real.sin (f x)) (Real.cos (f x) * f') x`
theorem rule_sound_sin (hf : HasDerivAt f f' x) :
    HasDerivAt (fun t => Real.sin (f t)) (f' * Real.cos (f x)) x :=
  hf.sin.congr_deriv (mul_comm _ _)

-- `HasDerivAt.cos : HasDerivAt f f' x → HasDerivAt (fun x => Real.cos (f x)) (-Real.sin (f x) * f') x`
theorem rule_sound_cos (hf : HasDerivAt f f' x) :
    HasDerivAt (fun t => Real.cos (f t)) (-(f' * Real.sin (f x))) x :=
  hf.cos.congr_deriv (by ring)

-- `Real.tan_eq_sin_div_cos (x) : Real.tan x = Real.sin x / Real.cos x`
-- `Real.sin_sq_add_cos_sq (x) : Real.sin x ^ 2 + Real.cos x ^ 2 = 1`
theorem one_add_tan_mul_tan {v : ℝ} (hv : Real.cos v ≠ 0) :
    1 + Real.tan v * Real.tan v = 1 / Real.cos v ^ 2 := by
  have hs := Real.sin_sq_add_cos_sq v
  rw [Real.tan_eq_sin_div_cos]
  field_simp
  nlinarith [hs]

-- `Real.hasDerivAt_tan : Real.cos x ≠ 0 → HasDerivAt Real.tan (1 / Real.cos x ^ 2) x`
theorem rule_sound_tan (hf : HasDerivAt f f' x) (hd : Real.cos (f x) ≠ 0) :
    HasDerivAt (fun t => Real.tan (f t)) (f' * (1 + Real.tan (f x) * Real.tan (f x))) x := by
  have h := (Real.hasDerivAt_tan hd).comp x hf
  exact h.congr_deriv (by rw [one_add_tan_mul_tan hd]; ring)

-- `Real.hasDerivAt_sinh (x) : HasDerivAt Real.sinh (Real.cosh x) x`
-- `Real.hasDerivAt_cosh (x) : HasDerivAt Real.cosh (Real.sinh x) x`
-- `Real.cosh_pos (x) : 0 < Real.cosh x`
-- `Real.tanh_eq_sinh_div_cosh (x) : Real.tanh x = Real.sinh x / Real.cosh x`
/-- `tanh`'s derivative, from `sinh / cosh`: no Mathlib name for it is relied on. -/
theorem hasDerivAt_tanh (v : ℝ) :
    HasDerivAt Real.tanh (1 - Real.tanh v * Real.tanh v) v := by
  have hc : Real.cosh v ≠ 0 := (Real.cosh_pos v).ne'
  have hc2 : Real.cosh v ^ 2 ≠ 0 := pow_ne_zero 2 hc
  have h := (Real.hasDerivAt_sinh v).div (Real.hasDerivAt_cosh v) hc
  have hfun : Real.tanh = fun y => Real.sinh y / Real.cosh y :=
    funext Real.tanh_eq_sinh_div_cosh
  have e : (Real.cosh v * Real.cosh v - Real.sinh v * Real.sinh v) / Real.cosh v ^ 2 =
      1 - Real.tanh v * Real.tanh v := by
    -- `div_mul_div_comm : a / b * (c / d) = a * c / (b * d)`, `sq : a ^ 2 = a * a`,
    -- `sub_div : (a - b) / c = a / c - b / c`
    rw [Real.tanh_eq_sinh_div_cosh, div_mul_div_comm, ← sq (Real.cosh v), sub_div, div_self hc2]
  have h2 : HasDerivAt Real.tanh
      ((Real.cosh v * Real.cosh v - Real.sinh v * Real.sinh v) / Real.cosh v ^ 2) v := by
    rw [hfun]
    exact h
  exact h2.congr_deriv e

theorem rule_sound_tanh (hf : HasDerivAt f f' x) :
    HasDerivAt (fun t => Real.tanh (f t)) (f' * (1 - Real.tanh (f x) * Real.tanh (f x))) x := by
  have h := (hasDerivAt_tanh (f x)).comp x hf
  exact h.congr_deriv (mul_comm _ _)

-- `HasDerivAt.congr_of_eventuallyEq : HasDerivAt f f' x → f₁ =ᶠ[𝓝 x] f → HasDerivAt f₁ f' x`
-- `hasDerivAt_const (x) (c) : HasDerivAt (fun _ => c) 0 x`
-- `abs_of_pos : 0 < a → |a| = a`, `abs_of_neg : a < 0 → |a| = -a`
/-- `abs` away from 0: the rule's nested `select` is the argument's sign. -/
theorem rule_sound_abs (c : ℝ) (hf : HasDerivAt f f' x) (hd : f x ≠ 0) :
    HasDerivAt (fun t => |f t|) (f' * pick (0 < f x) 1 (pick (f x < 0) (-1) c)) x := by
  rcases lt_or_gt_of_ne hd with h | h
  · have e : f' * pick (0 < f x) 1 (pick (f x < 0) (-1) c) = -f' := by
      rw [pick_neg (not_lt.mpr h.le), pick_pos h]
      ring
    rw [e]
    refine hf.neg.congr_of_eventuallyEq ?_
    filter_upwards [eventually_lt_of_hasDerivAt hf (hasDerivAt_const x (0 : ℝ)) h] with t ht
    exact abs_of_neg ht
  · have h0 : 0 < f x := h
    have e : f' * pick (0 < f x) 1 (pick (f x < 0) (-1) c) = f' := by
      rw [pick_pos h0]
      ring
    rw [e]
    refine hf.congr_of_eventuallyEq ?_
    filter_upwards [eventually_lt_of_hasDerivAt (hasDerivAt_const x (0 : ℝ)) hf h0] with t ht
    exact abs_of_pos ht

-- `min_eq_left : a ≤ b → min a b = a`, `min_eq_right : b ≤ a → min a b = b`
/-- `min` away from a tie: the smaller operand's derivative. -/
theorem rule_sound_min (c : ℝ) (hf : HasDerivAt f f' x) (hg : HasDerivAt g g' x)
    (hd : f x ≠ g x) :
    HasDerivAt (fun t => min (f t) (g t))
      (pick (f x < g x) f' (pick (g x < f x) g' (c * f' + (1 - c) * g'))) x := by
  rcases lt_or_gt_of_ne hd with h | h
  · rw [pick_pos h]
    refine hf.congr_of_eventuallyEq ?_
    filter_upwards [eventually_lt_of_hasDerivAt hf hg h] with t ht
    exact min_eq_left ht.le
  · have h0 : g x < f x := h
    rw [pick_neg (not_lt.mpr h0.le), pick_pos h0]
    refine hg.congr_of_eventuallyEq ?_
    filter_upwards [eventually_lt_of_hasDerivAt hg hf h0] with t ht
    exact min_eq_right ht.le

-- `max_eq_left : b ≤ a → max a b = a`, `max_eq_right : a ≤ b → max a b = b`
/-- `max` away from a tie: the larger operand's derivative. -/
theorem rule_sound_max (c : ℝ) (hf : HasDerivAt f f' x) (hg : HasDerivAt g g' x)
    (hd : f x ≠ g x) :
    HasDerivAt (fun t => max (f t) (g t))
      (pick (g x < f x) f' (pick (f x < g x) g' (c * f' + (1 - c) * g'))) x := by
  rcases lt_or_gt_of_ne hd with h | h
  · rw [pick_neg (not_lt.mpr h.le), pick_pos h]
    refine hg.congr_of_eventuallyEq ?_
    filter_upwards [eventually_lt_of_hasDerivAt hf hg h] with t ht
    exact max_eq_right ht.le
  · have h0 : g x < f x := h
    rw [pick_pos h0]
    refine hf.congr_of_eventuallyEq ?_
    filter_upwards [eventually_lt_of_hasDerivAt hg hf h0] with t ht
    exact max_eq_left ht.le

theorem rule_sound_mulAdd (hf : HasDerivAt f f' x) (hg : HasDerivAt g g' x)
    (hk : HasDerivAt k k' x) :
    HasDerivAt (fun t => f t * g t + k t) (f' * g x + f x * g' + k') x :=
  (hf.mul hg).add hk

/-- `select` where its condition is steady (`q` is the condition at the point): the taken
arm's derivative, and only the taken arm need be differentiable. -/
theorem rule_sound_select {p : ℝ → Prop} {q : Prop} (hp : ∀ᶠ t in nhds x, (p t ↔ q))
    (hf : q → HasDerivAt f f' x) (hg : ¬q → HasDerivAt g g' x) :
    HasDerivAt (fun t => pick (p t) (f t) (g t)) (pick q f' g') x := by
  by_cases hq : q
  · rw [pick_pos hq]
    refine (hf hq).congr_of_eventuallyEq ?_
    filter_upwards [hp] with t ht
    exact pick_pos (ht.mpr hq) _ _
  · rw [pick_neg hq]
    refine (hg hq).congr_of_eventuallyEq ?_
    filter_upwards [hp] with t ht
    exact pick_neg (fun hpt => hq (ht.mp hpt)) _ _

end BlazeDiff
