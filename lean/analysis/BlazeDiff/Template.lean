import BlazeDiff.Expr

/-!
A derivative rule's parts, as `crates/blaze-diff/src/diff.rs` has them: the template a tangent
is written in (`P`), where a rule holds (`Domain`) and where its operation has no derivative
(`KinkAt`). `Rules.lean`, generated from `rules.json`, fills them in per operation.

A template is instantiated two ways: `P.inst` builds the term (what the Rust `instantiate`
builds, without its elision of structural zeros and ones, which changes no real value), and
`P.evalR` is its value given the values of the node's parts. `eval_inst` says they agree, so a
rule is proved once, about reals.
-/

noncomputable section

namespace BlazeDiff

/-- An argument slot of a node. -/
inductive Slot where
  | s0 | s1 | s2

/-- A comparison a template may select by (`diff::C`, but its `Cond`). -/
inductive Cmp where
  | lt | gt | eq

/-- The operation of a comparison. -/
def Cmp.op : Cmp → Op
  | .lt => .lt
  | .gt => .gt
  | .eq => .eq

def Cmp.Holds : Cmp → ℝ → ℝ → Prop
  | .lt, x, y => x < y
  | .gt, x, y => y < x
  | .eq, x, y => x = y

/-- When a kink is on the way of the variable (`diff::KinkAt`). -/
inductive KinkAt where
  | value | condition
  deriving DecidableEq

/-- Where a rule holds (`diff::Domain`). -/
inductive Domain where
  | everywhere | argPositive | argNonZero | divisorNonZero | argsDiffer | awayFromPoles | powiBase

/-- A domain at a node with exponent `n` (a `powi`'s) and arguments `a` and `b`. -/
def Domain.Holds : Domain → Blaze.I32 → ℝ → ℝ → Prop
  | .everywhere, _, _, _ => True
  | .argPositive, _, a, _ => 0 < a
  | .argNonZero, _, a, _ => a ≠ 0
  | .divisorNonZero, _, _, b => b ≠ 0
  | .argsDiffer, _, a, b => a ≠ b
  | .awayFromPoles, _, a, _ => Real.cos a ≠ 0
  | .powiBase, n, a, _ => 1 ≤ n.val ∨ a ≠ 0

/-- A template over one node (`diff::P`). `sel c l r x y` is `Select(C::c(l, r), x, y)` and
`selCond x y` is `Select(C::Cond, x, y)`. -/
inductive P where
  | a (i : Slot)
  | t (i : Slot)
  | out
  | lit (r : ℝ)
  | choice
  | exponent
  | powiDown
  | add (x y : P)
  | sub (x y : P)
  | mul (x y : P)
  | div (x y : P)
  | neg (x : P)
  | call (op : Op) (x : P)
  | powf (x y : P)
  | sel (c : Cmp) (l r x y : P)
  | selCond (x y : P)

/-- What a slot or payload holds in a node that has none. -/
def nil : Expr := Expr.const 0

/-- The exponent of a node that is not a `powi`. -/
def noExp : Blaze.I32 := Blaze.Scalar.lit .I32 0

/-- A node's parts as terms: its arguments, their derivatives, itself, the value chosen at
its kink and its exponent. -/
structure Cx where
  a0 : Expr
  a1 : Expr
  a2 : Expr
  t0 : Expr
  t1 : Expr
  t2 : Expr
  out : Expr
  choice : ℝ
  exponent : Blaze.I32

def Cx.arg (cx : Cx) : Slot → Expr
  | .s0 => cx.a0
  | .s1 => cx.a1
  | .s2 => cx.a2

def Cx.tan (cx : Cx) : Slot → Expr
  | .s0 => cx.t0
  | .s1 => cx.t1
  | .s2 => cx.t2

/-- A `powi` node's argument to the exponent less one. `i32::MIN` has no `i32` below it:
there it is the node over its argument, as the Rust `instantiate` builds it. -/
def Cx.powiDown (cx : Cx) : Expr :=
  if cx.exponent.val = Blaze.ScalarTy.min .I32 then Expr.bin .div cx.out cx.a0
  else Expr.powi (Blaze.Scalar.wrap .I32 (cx.exponent.val - 1)) cx.a0

/-- The term a template is at a node. -/
def P.inst (cx : Cx) : P → Expr
  | .a i => cx.arg i
  | .t i => cx.tan i
  | .out => cx.out
  | .lit r => Expr.const r
  | .choice => Expr.const cx.choice
  | .exponent => Expr.const (cx.exponent.val : ℝ)
  | .powiDown => cx.powiDown
  | .add x y => Expr.bin .add (P.inst cx x) (P.inst cx y)
  | .sub x y => Expr.bin .sub (P.inst cx x) (P.inst cx y)
  | .mul x y => Expr.bin .mul (P.inst cx x) (P.inst cx y)
  | .div x y => Expr.bin .div (P.inst cx x) (P.inst cx y)
  | .neg x => Expr.un .neg (P.inst cx x)
  | .call op x => Expr.un op (P.inst cx x)
  | .powf x y => Expr.bin .powf (P.inst cx x) (P.inst cx y)
  | .sel c l r x y =>
    Expr.ter .select (Expr.bin c.op (P.inst cx l) (P.inst cx r)) (P.inst cx x) (P.inst cx y)
  | .selCond x y => Expr.ter .select cx.a0 (P.inst cx x) (P.inst cx y)

/-- A node's parts as values; `cond` is its first argument as a condition (a `select`'s). -/
structure RCx where
  a0 : ℝ
  a1 : ℝ
  a2 : ℝ
  t0 : ℝ
  t1 : ℝ
  t2 : ℝ
  out : ℝ
  choice : ℝ
  exponent : Blaze.I32
  cond : Prop

def RCx.arg (cx : RCx) : Slot → ℝ
  | .s0 => cx.a0
  | .s1 => cx.a1
  | .s2 => cx.a2

def RCx.tan (cx : RCx) : Slot → ℝ
  | .s0 => cx.t0
  | .s1 => cx.t1
  | .s2 => cx.t2

def RCx.powiDown (cx : RCx) : ℝ :=
  if cx.exponent.val = Blaze.ScalarTy.min .I32 then cx.out / cx.a0
  else cx.a0 ^ (Blaze.Scalar.wrap .I32 (cx.exponent.val - 1)).val

/-- The value a template has at a node. -/
def P.evalR (cx : RCx) : P → ℝ
  | .a i => cx.arg i
  | .t i => cx.tan i
  | .out => cx.out
  | .lit r => r
  | .choice => cx.choice
  | .exponent => (cx.exponent.val : ℝ)
  | .powiDown => cx.powiDown
  | .add x y => P.evalR cx x + P.evalR cx y
  | .sub x y => P.evalR cx x - P.evalR cx y
  | .mul x y => P.evalR cx x * P.evalR cx y
  | .div x y => P.evalR cx x / P.evalR cx y
  | .neg x => -P.evalR cx x
  | .call op x => evalUn op (P.evalR cx x)
  | .powf x y => P.evalR cx x ^ P.evalR cx y
  | .sel c l r x y => pick (c.Holds (P.evalR cx l) (P.evalR cx r)) (P.evalR cx x) (P.evalR cx y)
  | .selCond x y => pick cx.cond (P.evalR cx x) (P.evalR cx y)

/-- A node's parts at the point `env`. -/
def Cx.eval (cx : Cx) (env : ℕ → ℝ) : RCx where
  a0 := cx.a0.eval env
  a1 := cx.a1.eval env
  a2 := cx.a2.eval env
  t0 := cx.t0.eval env
  t1 := cx.t1.eval env
  t2 := cx.t2.eval env
  out := cx.out.eval env
  choice := cx.choice
  exponent := cx.exponent
  cond := cx.a0.holds env

theorem eval_powiDown (env : ℕ → ℝ) (cx : Cx) :
    cx.powiDown.eval env = (cx.eval env).powiDown := by
  by_cases h : cx.exponent.val = Blaze.ScalarTy.min .I32
  · have e1 : cx.powiDown = Expr.bin .div cx.out cx.a0 := if_pos h
    have e2 : (cx.eval env).powiDown = (cx.eval env).out / (cx.eval env).a0 := if_pos h
    rw [e1, e2] <;> rfl
  · have e1 : cx.powiDown = Expr.powi (Blaze.Scalar.wrap .I32 (cx.exponent.val - 1)) cx.a0 :=
      if_neg h
    have e2 : (cx.eval env).powiDown =
        (cx.eval env).a0 ^ (Blaze.Scalar.wrap .I32 ((cx.eval env).exponent.val - 1)).val :=
      if_neg h
    rw [e1, e2] <;> rfl

/-- The term a template builds means the value the template has: every case is the two
definitions unfolding to the same thing, by the induction hypotheses. -/
theorem eval_inst (env : ℕ → ℝ) (cx : Cx) (p : P) :
    (P.inst cx p).eval env = P.evalR (cx.eval env) p := by
  induction p with
  | a i => cases i <;> rfl
  | t i => cases i <;> rfl
  | out => rfl
  | lit r => rfl
  | choice => rfl
  | exponent => rfl
  | powiDown => exact eval_powiDown env cx
  | add x y ihx ihy =>
    show (P.inst cx x).eval env + (P.inst cx y).eval env = _
    rw [ihx, ihy] <;> rfl
  | sub x y ihx ihy =>
    show (P.inst cx x).eval env - (P.inst cx y).eval env = _
    rw [ihx, ihy] <;> rfl
  | mul x y ihx ihy =>
    show (P.inst cx x).eval env * (P.inst cx y).eval env = _
    rw [ihx, ihy] <;> rfl
  | div x y ihx ihy =>
    show (P.inst cx x).eval env / (P.inst cx y).eval env = _
    rw [ihx, ihy] <;> rfl
  | neg x ihx =>
    show -(P.inst cx x).eval env = _
    rw [ihx] <;> rfl
  | call op x ihx =>
    show evalUn op ((P.inst cx x).eval env) = _
    rw [ihx] <;> rfl
  | powf x y ihx ihy =>
    show (P.inst cx x).eval env ^ (P.inst cx y).eval env = _
    rw [ihx, ihy] <;> rfl
  | sel c l r x y ihl ihr ihx ihy =>
    show pick (holdsBin c.op ((P.inst cx l).eval env) ((P.inst cx r).eval env)
        ((P.inst cx l).holds env) ((P.inst cx r).holds env))
      ((P.inst cx x).eval env) ((P.inst cx y).eval env) = _
    rw [ihl, ihr, ihx, ihy]
    cases c <;> rfl
  | selCond x y ihx ihy =>
    show pick (cx.a0.holds env) ((P.inst cx x).eval env) ((P.inst cx y).eval env) = _
    rw [ihx, ihy] <;> rfl

end BlazeDiff
