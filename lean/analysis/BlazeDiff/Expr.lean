import Mathlib
import Blaze.Scalar
import BlazeDiff.Ops

/-!
The expressions of `crates/blaze-diff/src/expr.rs` and what they mean over the reals.

`Expr` is a `Graph`'s node as a tree: `Op` (generated from `rules.json`, as the Rust `OPS` is)
with its arguments, a constant's value, a variable's index, a `powi`'s exponent. Like a Rust
`Node` it is not sorted by its type: `sem` gives every term a real and a proposition, and the one
that is not the term's sort is junk (0, `False`); a term the Rust `Graph::try_add` refuses has a
meaning here that nothing uses.

The meaning is over `ℝ`, with Mathlib's total functions (`Real.log`, `Real.sqrt`, `/`, `rpow`,
`zpow`). Rust evaluates in `f32` or `f64`: nothing here is about rounding, NaN, infinities or
signed zeros. A `powi` exponent is Rust's `i32`, `Blaze.I32` of `lean/Blaze/Scalar.lean`.
-/

noncomputable section

namespace BlazeDiff

/-- `if p then a else b` for any proposition, decided classically: one instance everywhere, so
no two `ite`s differ by how they decide. -/
def pick (p : Prop) (a b : ℝ) : ℝ :=
  @ite ℝ p (Classical.propDecidable p) a b

-- `ite_eq_left : c → ite c t e = t`, `ite_eq_right : ¬c → ite c t e = e` (the instance is
-- implicit).
theorem pick_pos {p : Prop} (h : p) (a b : ℝ) : pick p a b = a := by
  unfold pick
  exact ite_eq_left h

theorem pick_neg {p : Prop} (h : ¬p) (a b : ℝ) : pick p a b = b := by
  unfold pick
  exact ite_eq_right h

/-- A float operation of one argument (`eval::apply`); 0 for any other operation. -/
def evalUn : Op → ℝ → ℝ
  | .neg, a => -a
  | .sqrt, a => Real.sqrt a
  | .exp, a => Real.exp a
  | .ln, a => Real.log a
  | .sin, a => Real.sin a
  | .cos, a => Real.cos a
  | .tan, a => Real.tan a
  | .tanh, a => Real.tanh a
  | .abs, a => |a|
  | _, _ => 0

/-- A float operation of two arguments; 0 for any other operation. `powf` is `Real.rpow`. -/
def evalBin : Op → ℝ → ℝ → ℝ
  | .add, a, b => a + b
  | .sub, a, b => a - b
  | .mul, a, b => a * b
  | .div, a, b => a / b
  | .powf, a, b => a ^ b
  | .min, a, b => min a b
  | .max, a, b => max a b
  | _, _, _ => 0

/-- A float operation of three arguments, given the first as a condition and as a float:
`mul_add(a, b, c)` is `a * b + c`, `select(p, b, c)` is `b` when `p` holds and `c` otherwise. -/
def evalTer : Op → Prop → ℝ → ℝ → ℝ → ℝ
  | .mulAdd, _, a, b, c => a * b + c
  | .select, p, _, b, c => pick p b c
  | _, _, _, _, _ => 0

/-- A condition of one argument: `not`. -/
def holdsUn : Op → Prop → Prop
  | .not, p => ¬p
  | _, _ => False

/-- A condition of two arguments, given both as floats and as conditions. -/
def holdsBin : Op → ℝ → ℝ → Prop → Prop → Prop
  | .lt, a, b, _, _ => a < b
  | .le, a, b, _, _ => a ≤ b
  | .gt, a, b, _, _ => b < a
  | .ge, a, b, _, _ => b ≤ a
  | .eq, a, b, _, _ => a = b
  | .ne, a, b, _, _ => a ≠ b
  | .and, _, _, p, q => p ∧ q
  | .or, _, _, p, q => p ∨ q
  | _, _, _, _, _ => False

/-- A term: a node of a `Graph` with its arguments as terms. -/
inductive Expr where
  /-- A constant, by its value. -/
  | const (r : ℝ)
  /-- The function's parameter `i`. -/
  | var (i : ℕ)
  | bool (b : Bool)
  /-- `a.powi(n)`. -/
  | powi (n : Blaze.I32) (a : Expr)
  | un (op : Op) (a : Expr)
  | bin (op : Op) (a b : Expr)
  /-- `mul_add(a, b, c)`, or `select(a, b, c)` with `a` the condition. -/
  | ter (op : Op) (a b c : Expr)

/-- A term's value as a float and as a condition at the point `env`. One function, so a
`select` reads its condition and a comparison reads its floats without mutual recursion. -/
def sem (env : ℕ → ℝ) : Expr → ℝ × Prop
  | .const r => (r, False)
  | .var i => (env i, False)
  | .bool b => (0, b = true)
  | .powi n a => ((sem env a).1 ^ n.val, False)
  | .un op a => (evalUn op (sem env a).1, holdsUn op (sem env a).2)
  | .bin op a b =>
    (evalBin op (sem env a).1 (sem env b).1,
      holdsBin op (sem env a).1 (sem env b).1 (sem env a).2 (sem env b).2)
  | .ter op a b c =>
    (evalTer op (sem env a).2 (sem env a).1 (sem env b).1 (sem env c).1, False)

/-- The value of a float term at `env`. -/
def Expr.eval (e : Expr) (env : ℕ → ℝ) : ℝ :=
  (sem env e).1

/-- Whether a condition holds at `env`. -/
def Expr.holds (e : Expr) (env : ℕ → ℝ) : Prop :=
  (sem env e).2

theorem eval_const (env : ℕ → ℝ) (r : ℝ) : (Expr.const r).eval env = r := rfl

theorem eval_var (env : ℕ → ℝ) (i : ℕ) : (Expr.var i).eval env = env i := rfl

theorem eval_powi (env : ℕ → ℝ) (n : Blaze.I32) (a : Expr) :
    (Expr.powi n a).eval env = a.eval env ^ n.val := rfl

theorem eval_un (env : ℕ → ℝ) (op : Op) (a : Expr) :
    (Expr.un op a).eval env = evalUn op (a.eval env) := rfl

theorem eval_bin (env : ℕ → ℝ) (op : Op) (a b : Expr) :
    (Expr.bin op a b).eval env = evalBin op (a.eval env) (b.eval env) := rfl

theorem eval_ter (env : ℕ → ℝ) (op : Op) (a b c : Expr) :
    (Expr.ter op a b c).eval env =
      evalTer op (a.holds env) (a.eval env) (b.eval env) (c.eval env) := rfl

theorem holds_bin (env : ℕ → ℝ) (op : Op) (a b : Expr) :
    (Expr.bin op a b).holds env =
      holdsBin op (a.eval env) (b.eval env) (a.holds env) (b.holds env) := rfl

/-- Moving coordinate `x` to where it is changes nothing.
`Function.update_eq_self : Function.update f a (f a) = f`. -/
theorem eval_update_self (env : ℕ → ℝ) (x : ℕ) (e : Expr) :
    e.eval (Function.update env x (env x)) = e.eval env := by
  rw [Function.update_eq_self]

theorem holds_update_self (env : ℕ → ℝ) (x : ℕ) (e : Expr) :
    e.holds (Function.update env x (env x)) = e.holds env := by
  rw [Function.update_eq_self]

end BlazeDiff
