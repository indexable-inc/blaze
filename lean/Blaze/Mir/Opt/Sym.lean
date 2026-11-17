import Blaze.Mir.Step
import Blaze.Mir.Opt.Bind

/-!
Symbolic execution, first piece: expressions over a block's initial locals, and rvalues as
expressions.

An `Expr` is what a local holds as a function of what the locals held when the block began:
a local's initial value, a constant, or an operator on expressions. `Expr.eval st0 e` is its
value in the state `st0` the block began in (it can panic or be undefined, as the operator
can). A symbolic store `σ` says, for the locals assigned so far, the expression each holds
(`look`); `Reads st0 σ st` says the state `st` agrees with it: a read of local `x` in `st`
is the value of `look σ x` in `st0`.

`evalRvalue_sym`: under `Reads`, an rvalue of the subset evaluates to the value of its
expression and leaves the state as it was.

The subset (`symRvalue` is `none` outside it): `use`, `binary`, `checked`, `withOverflow`,
`unary` and `cast`, on operands that are a constant or a `copy` of a whole local. No `move`
(it changes the state), no projection (a field, an index, a dereference), no `ref`, no
aggregate.
-/

namespace Blaze.Mir

/-- What a local holds, as a function of the locals when the block began. -/
inductive Expr where
  | var (x : Local)
  | const (c : Const)
  | binary (checked : Bool) (op : BinOp) (a b : Expr)
  | overflow (op : BinOp) (a b : Expr)
  | unary (op : UnOp) (a : Expr)
  | cast (a : Expr) (ty : ScalarTy)
  deriving DecidableEq, Repr

/-- The value of the expression in the state the block began in. -/
def Expr.eval (st0 : State) : Expr → Outcome Value
  | Expr.var x => st0.read { frame := st0.top, slot := x, path := [] }
  | Expr.const c => Outcome.ok (constValue c)
  | Expr.binary checked op a b =>
    (Expr.eval st0 a).bind (fun va => (Expr.eval st0 b).bind (fun vb => binaryOp checked op va vb))
  | Expr.overflow op a b =>
    (Expr.eval st0 a).bind (fun va => (Expr.eval st0 b).bind (fun vb => overflowingOp op va vb))
  | Expr.unary op a => (Expr.eval st0 a).bind (fun va => unaryOp op va)
  | Expr.cast a ty => (Expr.eval st0 a).bind (fun va => castTo ty va)

/-- The expression a local holds: what the store says, or its initial value. -/
def look : List (Local × Expr) → Local → Expr
  | [], x => Expr.var x
  | entry :: rest, x => if entry.1 = x then entry.2 else look rest x

theorem look_cons (entry : Local × Expr) (rest : List (Local × Expr)) (x : Local) :
    look (entry :: rest) x = if entry.1 = x then entry.2 else look rest x := rfl

/-- The state agrees with the symbolic store: it runs the frame `st0` ran, and a read of a
local is the value of the local's expression in `st0`. -/
def Reads (st0 : State) (σ : List (Local × Expr)) (st : State) : Prop :=
  st.top = st0.top ∧
    ∀ (x : Local), st.read { frame := st0.top, slot := x, path := [] } = (look σ x).eval st0

/-- The state a block begins in agrees with the empty store. -/
theorem Reads.start (st0 : State) : Reads st0 [] st0 :=
  And.intro rfl (fun _ => rfl)

/-! Operands and rvalues, as expressions. -/

def symPlace (σ : List (Local × Expr)) : Place → Option Expr
  | { base := x, proj := [] } => some (look σ x)
  | { base := _, proj := _ :: _ } => none

def symOperand (σ : List (Local × Expr)) : Operand → Option Expr
  | Operand.copy p => symPlace σ p
  | Operand.move _ => none
  | Operand.const c => some (Expr.const c)

def opt1 (f : Expr → Expr) : Option Expr → Option Expr
  | some a => some (f a)
  | none => none

def opt2 (f : Expr → Expr → Expr) : Option Expr → Option Expr → Option Expr
  | some a, some b => some (f a b)
  | some _, none => none
  | none, _ => none

theorem opt1_some (f : Expr → Expr) :
    ∀ (oa : Option Expr) (e : Expr), opt1 f oa = some e → ∃ a, oa = some a ∧ f a = e
  | some a, _, h => ⟨a, rfl, Option.some.inj h⟩
  | none, _, h => none_eq_some_elim h

theorem opt2_some (f : Expr → Expr → Expr) :
    ∀ (oa ob : Option Expr) (e : Expr), opt2 f oa ob = some e →
      ∃ a b, oa = some a ∧ ob = some b ∧ f a b = e
  | some a, some b, _, h => ⟨a, b, rfl, rfl, Option.some.inj h⟩
  | some _, none, _, h => none_eq_some_elim h
  | none, _, _, h => none_eq_some_elim h

def symRvalue (σ : List (Local × Expr)) : Rvalue → Option Expr
  | Rvalue.use o => symOperand σ o
  | Rvalue.binary op a b => opt2 (Expr.binary false op) (symOperand σ a) (symOperand σ b)
  | Rvalue.checked op a b => opt2 (Expr.binary true op) (symOperand σ a) (symOperand σ b)
  | Rvalue.withOverflow op a b => opt2 (Expr.overflow op) (symOperand σ a) (symOperand σ b)
  | Rvalue.unary op o => opt1 (Expr.unary op) (symOperand σ o)
  | Rvalue.cast o ty => opt1 (fun a => Expr.cast a ty) (symOperand σ o)
  | Rvalue.aggregate _ _ => none
  | Rvalue.replicate _ _ => none
  | Rvalue.discriminant _ _ => none
  | Rvalue.ref _ _ => none
  | Rvalue.len _ => none

/-- An operand of the subset is the value of its expression, and leaves the state. -/
theorem evalOperand_sym {st0 : State} {σ : List (Local × Expr)} {st : State}
    (h : Reads st0 σ st) :
    ∀ (o : Operand) (e : Expr), symOperand σ o = some e →
      evalOperand st o = Outcome.map (fun v => (v, st)) (e.eval st0)
  | Operand.copy { base := x, proj := [] }, e, hs => by
    have he : look σ x = e := Option.some.inj hs
    have place : evalOperand st (Operand.copy { base := x, proj := [] }) =
        (st.read { frame := st.top, slot := x, path := [] }).bind (fun v => Outcome.ok (v, st)) :=
      rfl
    have top : st.read { frame := st.top, slot := x, path := [] } =
        st.read { frame := st0.top, slot := x, path := [] } :=
      congrTo (fun t => st.read { frame := t, slot := x, path := [] }) (And.left h)
    have val : st.read { frame := st0.top, slot := x, path := [] } = e.eval st0 :=
      (And.right h x).trans (congrTo (fun y => Expr.eval st0 y) he)
    exact place.trans
      (congrTo (fun r => Outcome.bind r (fun v => Outcome.ok (v, st))) (top.trans val))
  | Operand.copy { base := _, proj := _ :: _ }, _, hs => none_eq_some_elim hs
  | Operand.move _, _, hs => none_eq_some_elim hs
  | Operand.const c, e, hs => by
    have he : Expr.const c = e := Option.some.inj hs
    rw [← he]
    rfl

/-- Two operands, then an operator: the operator on the two expressions' values. -/
theorem evalTwo_sym {st0 : State} {σ : List (Local × Expr)} {st : State} (h : Reads st0 σ st)
    (a b : Operand) (ea eb : Expr) (ha : symOperand σ a = some ea)
    (hb : symOperand σ b = some eb) (f : Value → Value → Outcome Value) :
    evalTwo st a b f =
      Outcome.map (fun v => (v, st))
        ((ea.eval st0).bind (fun va => (eb.eval st0).bind (fun vb => f va vb))) :=
  Outcome.bind_commutes_map (evalOperand_sym h a ea ha) (fun _ _ =>
    Outcome.bind_commutes_map (evalOperand_sym h b eb hb) (fun _ _ => rfl))

theorem evalOne_sym {st0 : State} {σ : List (Local × Expr)} {st : State} (h : Reads st0 σ st)
    (o : Operand) (eo : Expr) (ho : symOperand σ o = some eo) (f : Value → Outcome Value) :
    evalOne st o f =
      Outcome.map (fun v => (v, st)) ((eo.eval st0).bind (fun va => f va)) :=
  Outcome.bind_commutes_map (evalOperand_sym h o eo ho) (fun _ _ => rfl)

/-- An rvalue of the subset is the value of its expression, and leaves the state. -/
theorem evalRvalue_sym {st0 : State} {σ : List (Local × Expr)} {st : State}
    (adts : List AdtDecl) (h : Reads st0 σ st) :
    ∀ (rv : Rvalue) (e : Expr), symRvalue σ rv = some e →
      evalRvalue adts st rv = Outcome.map (fun v => (v, st)) (e.eval st0)
  | Rvalue.use o, e, hs => evalOperand_sym h o e hs
  | Rvalue.binary op a b, e, hs => by
    have ⟨ea, eb, ha, hb, he⟩ := opt2_some (Expr.binary false op) (symOperand σ a)
      (symOperand σ b) e hs
    rw [← he]
    exact evalTwo_sym h a b ea eb ha hb (binaryOp false op)
  | Rvalue.checked op a b, e, hs => by
    have ⟨ea, eb, ha, hb, he⟩ := opt2_some (Expr.binary true op) (symOperand σ a)
      (symOperand σ b) e hs
    rw [← he]
    exact evalTwo_sym h a b ea eb ha hb (binaryOp true op)
  | Rvalue.withOverflow op a b, e, hs => by
    have ⟨ea, eb, ha, hb, he⟩ := opt2_some (Expr.overflow op) (symOperand σ a)
      (symOperand σ b) e hs
    rw [← he]
    exact evalTwo_sym h a b ea eb ha hb (overflowingOp op)
  | Rvalue.unary op o, e, hs => by
    have ⟨eo, ho, he⟩ := opt1_some (Expr.unary op) (symOperand σ o) e hs
    rw [← he]
    exact evalOne_sym h o eo ho (unaryOp op)
  | Rvalue.cast o ty, e, hs => by
    have ⟨eo, ho, he⟩ := opt1_some (fun a => Expr.cast a ty) (symOperand σ o) e hs
    rw [← he]
    exact evalOne_sym h o eo ho (castTo ty)
  | Rvalue.aggregate _ _, _, hs => none_eq_some_elim hs
  | Rvalue.replicate _ _, _, hs => none_eq_some_elim hs
  | Rvalue.discriminant _ _, _, hs => none_eq_some_elim hs
  | Rvalue.ref _ _, _, hs => none_eq_some_elim hs
  | Rvalue.len _, _, hs => none_eq_some_elim hs

end Blaze.Mir
