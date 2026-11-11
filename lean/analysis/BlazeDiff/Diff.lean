import BlazeDiff.Rules

/-!
The differentiator of `crates/blaze-diff/src/diff.rs`, and the points it is claimed at.

`diff c x e` is `d(e)/d(variable x)` with `c` the value chosen at a kink
(`Choice::Subgradient(c)`): at each node, the generated rule's tangent (`Rules.tangent`)
instantiated at the node's arguments, their derivatives and the node. Nothing here says what
any operation's derivative is: `Rules.lean` does, from `rules.json`.

How it relates to the Rust `Differentiator`, which this is not a copy of:
- Rust walks a hash-consed DAG once and keeps each node's derivative; this recurses over the
  term the DAG unfolds to. A shared node is one value either way.
- Rust never builds a structural zero or one (`0 * e`, `0 + e`, `1 * e`, `0 / e`, `e / 1`, a
  `select` of equal arms): the same real value.
- Under `Choice::Error` Rust refuses a kink the variable reaches; `DiffAt` is the claim for
  points off every kink, where `c` is not read.
These three are the link the crate's tests hold (`tests/dual.rs`), not theorems.
-/

noncomputable section

namespace BlazeDiff

/-- `d(e)/d(variable x)`, choosing `c` at a kink. -/
def diff (c : ℝ) (x : ℕ) : Expr → Expr
  | .const _ => Expr.const 0
  | .var i => if i = x then Expr.const 1 else Expr.const 0
  | .bool _ => Expr.const 0
  | .powi n a =>
    P.inst
      { a0 := a, a1 := nil, a2 := nil, t0 := diff c x a, t1 := nil, t2 := nil,
        out := Expr.powi n a, choice := c, exponent := n }
      (Rules.tangent .powi)
  | .un op a =>
    P.inst
      { a0 := a, a1 := nil, a2 := nil, t0 := diff c x a, t1 := nil, t2 := nil,
        out := Expr.un op a, choice := c, exponent := noExp }
      (Rules.tangent op)
  | .bin op a b =>
    P.inst
      { a0 := a, a1 := b, a2 := nil, t0 := diff c x a, t1 := diff c x b, t2 := nil,
        out := Expr.bin op a b, choice := c, exponent := noExp }
      (Rules.tangent op)
  | .ter op a b d =>
    P.inst
      { a0 := a, a1 := b, a2 := d, t0 := diff c x a, t1 := diff c x b, t2 := diff c x d,
        out := Expr.ter op a b d, choice := c, exponent := noExp }
      (Rules.tangent op)

/-- A condition does not change when coordinate `x` moves a little: the point is off the
boundary of a `select`'s condition. -/
def Steady (env : ℕ → ℝ) (x : ℕ) (cond : Expr) : Prop :=
  ∀ᶠ t in nhds (env x), (cond.holds (Function.update env x t) ↔ cond.holds env)

/-- Where the rules claim `e` differentiable by variable `x`: every node is a ruled float
operation inside its rule's domain (`Rules.domain`), and a `select` (whose rule's kink is at
its condition) has a steady condition and its taken arm differentiable. Stronger than the Rust
differentiator asks in one way: it asks the domain at every node, the Rust one only at nodes
the variable reaches (so `powf` by a constant exponent needs a positive base here). -/
def DiffAt (env : ℕ → ℝ) (x : ℕ) : Expr → Prop
  | .const _ => True
  | .var _ => True
  | .bool _ => False
  | .powi n a => DiffAt env x a ∧ (Rules.domain .powi).Holds n (a.eval env) 0
  | .un op a =>
    Rules.unary op = true ∧ DiffAt env x a ∧ (Rules.domain op).Holds noExp (a.eval env) 0
  | .bin op a b =>
    Rules.binary op = true ∧ DiffAt env x a ∧ DiffAt env x b ∧
      (Rules.domain op).Holds noExp (a.eval env) (b.eval env)
  | .ter op a b d =>
    (op = Op.mulAdd ∧ DiffAt env x a ∧ DiffAt env x b ∧ DiffAt env x d) ∨
      (op = Op.select ∧ (Rules.kink .select = some .condition → Steady env x a) ∧
        (a.holds env → DiffAt env x b) ∧ (¬a.holds env → DiffAt env x d))

/-- The derivative term's value is the rule's template at the node's values. -/
theorem eval_diff_powi (c : ℝ) (env : ℕ → ℝ) (x : ℕ) (n : Blaze.I32) (a : Expr) :
    (diff c x (Expr.powi n a)).eval env =
      P.evalR
        { a0 := a.eval env, a1 := 0, a2 := 0, t0 := (diff c x a).eval env, t1 := 0, t2 := 0,
          out := a.eval env ^ n.val, choice := c, exponent := n, cond := a.holds env }
        (Rules.tangent .powi) :=
  eval_inst env _ _

theorem eval_diff_un (c : ℝ) (env : ℕ → ℝ) (x : ℕ) (op : Op) (a : Expr) :
    (diff c x (Expr.un op a)).eval env =
      P.evalR
        { a0 := a.eval env, a1 := 0, a2 := 0, t0 := (diff c x a).eval env, t1 := 0, t2 := 0,
          out := evalUn op (a.eval env), choice := c, exponent := noExp, cond := a.holds env }
        (Rules.tangent op) :=
  eval_inst env _ _

theorem eval_diff_bin (c : ℝ) (env : ℕ → ℝ) (x : ℕ) (op : Op) (a b : Expr) :
    (diff c x (Expr.bin op a b)).eval env =
      P.evalR
        { a0 := a.eval env, a1 := b.eval env, a2 := 0, t0 := (diff c x a).eval env,
          t1 := (diff c x b).eval env, t2 := 0, out := evalBin op (a.eval env) (b.eval env),
          choice := c, exponent := noExp, cond := a.holds env }
        (Rules.tangent op) :=
  eval_inst env _ _

theorem eval_diff_ter (c : ℝ) (env : ℕ → ℝ) (x : ℕ) (op : Op) (a b d : Expr) :
    (diff c x (Expr.ter op a b d)).eval env =
      P.evalR
        { a0 := a.eval env, a1 := b.eval env, a2 := d.eval env, t0 := (diff c x a).eval env,
          t1 := (diff c x b).eval env, t2 := (diff c x d).eval env,
          out := evalTer op (a.holds env) (a.eval env) (b.eval env) (d.eval env),
          choice := c, exponent := noExp, cond := a.holds env }
        (Rules.tangent op) :=
  eval_inst env _ _

end BlazeDiff
