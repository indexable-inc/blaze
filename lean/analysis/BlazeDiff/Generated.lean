/-
Generated directly from `blaze_algebra::rules` (`STANDARD_REWRITE_RULES` & `DERIV_RULES`).
Every rewrite and derivative rule is emitted as a Lean 4 `Prop` over `ℝ` with one field
per rule in `AllSound`, so an unproven rule cannot pass the Lean gate.
-/
import Mathlib.Analysis.SpecialFunctions.Trigonometric.Deriv
import Mathlib.Analysis.SpecialFunctions.Log.Deriv
import Mathlib.Analysis.SpecialFunctions.Pow.Deriv

namespace BlazeAlgebra.Verified

/-- `neg_neg`: `-(-x) = x` (IEEE-754 bit-exact). -/
def rule_neg_neg : Prop :=
  ∀ (x0 : ℝ), (-(-x0)) = x0

/-- `mul_one`: `x * 1 = x` (IEEE-754 exact for non-NaN). -/
def rule_mul_one : Prop :=
  ∀ (x0 : ℝ), (x0 * (1 : ℝ)) = x0

/-- `div_one`: `x / 1 = x` (IEEE-754 exact for non-NaN). -/
def rule_div_one : Prop :=
  ∀ (x0 : ℝ), (x0 / (1 : ℝ)) = x0

/-- `add_zero`: `x + 0 = x` over the reals. -/
def rule_add_zero : Prop :=
  ∀ (x0 : ℝ), (x0 + (0 : ℝ)) = x0

/-- `mul_zero`: `x * 0 = 0` over the reals. -/
def rule_mul_zero : Prop :=
  ∀ (x0 : ℝ), (x0 * (0 : ℝ)) = (0 : ℝ)

/-- `sub_self`: `x - x = 0` over the reals. -/
def rule_sub_self : Prop :=
  ∀ (x0 : ℝ), (x0 - x0) = (0 : ℝ)

/-- `add_neg_cancel`: `x + (-x) = 0` over the reals. -/
def rule_add_neg_cancel : Prop :=
  ∀ (x0 : ℝ), (x0 + (-x0)) = (0 : ℝ)

/-- `div_self`: `x / x = 1` for `x ≠ 0`. -/
def rule_div_self : Prop :=
  ∀ (x0 : ℝ), x0 ≠ 0 → (x0 / x0) = (1 : ℝ)

/-- `exp_mul`: `exp(a) * exp(b) = exp(a + b)`. -/
def rule_exp_mul : Prop :=
  ∀ (x0 x1 : ℝ), ((Real.exp x0) * (Real.exp x1)) = (Real.exp (x0 + x1))

/-- `ln_add`: `ln(a) + ln(b) = ln(a * b)` for `a, b > 0`. -/
def rule_ln_add : Prop :=
  ∀ (x0 x1 : ℝ), 0 < x0 ∧ 0 < x1 → ((Real.log x0) + (Real.log x1)) = (Real.log (x0 * x1))

/-- `ln_exp`: `ln(exp(x)) = x` over the reals. -/
def rule_ln_exp : Prop :=
  ∀ (x0 : ℝ), (Real.log (Real.exp x0)) = x0

/-- `exp_ln`: `exp(ln(x)) = x` for `x > 0`. -/
def rule_exp_ln : Prop :=
  ∀ (x0 : ℝ), 0 < x0 → (Real.exp (Real.log x0)) = x0

/-- `fma_add`: `a * b + c = fma(a, b, c)` over the reals. -/
def rule_fma_add : Prop :=
  ∀ (x0 x1 x2 : ℝ), ((x0 * x1) + x2) = (x0 * x1 + x2)

/-- `pythagorean_sin_cos`: `sin(x)^2 + cos(x)^2 = 1`. -/
def rule_pythagorean_sin_cos : Prop :=
  ∀ (x0 : ℝ), (((Real.sin x0) ^ (2 : ℕ)) + ((Real.cos x0) ^ (2 : ℕ))) = (1 : ℝ)

/-- Derivative soundness for `neg` (arity 1). -/
def deriv_neg : Prop :=
  ∀ (f : ℝ → ℝ) (x df : ℝ), HasDerivAt f df x → HasDerivAt (fun t => -f t) (-df) x

/-- Derivative soundness for `add` (arity 2). -/
def deriv_add : Prop :=
  ∀ (f g : ℝ → ℝ) (x df dg : ℝ), HasDerivAt f df x → HasDerivAt g dg x → HasDerivAt (fun t => f t + g t) (df + dg) x

/-- Derivative soundness for `sub` (arity 2). -/
def deriv_sub : Prop :=
  ∀ (f g : ℝ → ℝ) (x df dg : ℝ), HasDerivAt f df x → HasDerivAt g dg x → HasDerivAt (fun t => f t - g t) (df - dg) x

/-- Derivative soundness for `mul` (arity 2). -/
def deriv_mul : Prop :=
  ∀ (f g : ℝ → ℝ) (x df dg : ℝ), HasDerivAt f df x → HasDerivAt g dg x → HasDerivAt (fun t => f t * g t) (df * g x + f x * dg) x

/-- Derivative soundness for `div` (arity 2). -/
def deriv_div : Prop :=
  ∀ (f g : ℝ → ℝ) (x df dg : ℝ), HasDerivAt f df x → HasDerivAt g dg x → g x ≠ 0 → HasDerivAt (fun t => f t / g t) ((df * g x - f x * dg) / (g x) ^ 2) x

/-- Derivative soundness for `sin` (arity 1). -/
def deriv_sin : Prop :=
  ∀ (f : ℝ → ℝ) (x df : ℝ), HasDerivAt f df x → HasDerivAt (fun t => Real.sin (f t)) (Real.cos (f x) * df) x

/-- Derivative soundness for `cos` (arity 1). -/
def deriv_cos : Prop :=
  ∀ (f : ℝ → ℝ) (x df : ℝ), HasDerivAt f df x → HasDerivAt (fun t => Real.cos (f t)) (-Real.sin (f x) * df) x

/-- Derivative soundness for `exp` (arity 1). -/
def deriv_exp : Prop :=
  ∀ (f : ℝ → ℝ) (x df : ℝ), HasDerivAt f df x → HasDerivAt (fun t => Real.exp (f t)) (Real.exp (f x) * df) x

/-- Derivative soundness for `ln` (arity 1). -/
def deriv_ln : Prop :=
  ∀ (f : ℝ → ℝ) (x df : ℝ), HasDerivAt f df x → f x ≠ 0 → HasDerivAt (fun t => Real.log (f t)) (df / f x) x

/-- Derivative soundness for `sqrt` (arity 1). -/
def deriv_sqrt : Prop :=
  ∀ (f : ℝ → ℝ) (x df : ℝ), HasDerivAt f df x → f x ≠ 0 → HasDerivAt (fun t => Real.sqrt (f t)) (df / (2 * Real.sqrt (f x))) x

/-- Derivative soundness for `fma` (arity 3). -/
def deriv_fma : Prop :=
  ∀ (f g h : ℝ → ℝ) (x df dg dh : ℝ), HasDerivAt f df x → HasDerivAt g dg x → HasDerivAt h dh x → HasDerivAt (fun t => f t * g t + h t) (df * g x + f x * dg + dh) x

/-- Proof witness bundle: one field per rewrite and derivative rule. -/
structure AllSound : Prop where
  rule_neg_neg : rule_neg_neg
  rule_mul_one : rule_mul_one
  rule_div_one : rule_div_one
  rule_add_zero : rule_add_zero
  rule_mul_zero : rule_mul_zero
  rule_sub_self : rule_sub_self
  rule_add_neg_cancel : rule_add_neg_cancel
  rule_div_self : rule_div_self
  rule_exp_mul : rule_exp_mul
  rule_ln_add : rule_ln_add
  rule_ln_exp : rule_ln_exp
  rule_exp_ln : rule_exp_ln
  rule_fma_add : rule_fma_add
  rule_pythagorean_sin_cos : rule_pythagorean_sin_cos
  deriv_neg : deriv_neg
  deriv_add : deriv_add
  deriv_sub : deriv_sub
  deriv_mul : deriv_mul
  deriv_div : deriv_div
  deriv_sin : deriv_sin
  deriv_cos : deriv_cos
  deriv_exp : deriv_exp
  deriv_ln : deriv_ln
  deriv_sqrt : deriv_sqrt
  deriv_fma : deriv_fma

end BlazeAlgebra.Verified
