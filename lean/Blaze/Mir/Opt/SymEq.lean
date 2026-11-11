import Blaze.Mir.Opt.SymBlock
import Blaze.Mir.Opt.Witness

/-!
Symbolic execution, third piece: terminators, blocks, programs, and the validator.

A terminator is symbolic where its condition is (`symTerm`: a `switchInt` or an `assert` on
an operand of the subset is on the operand's expression; anything else is itself). A block
is its statements' trace and its symbolic terminator (`symBlock`); `execBlock_sym`: a block
does what its trace and its terminator mean. So two blocks that are equal, or have one
symbolic form, do the same from every state (`blockEq_sound`).

`symEq a b`: the two programs have the same types and, function by function and block by
block, blocks that are equal or have one symbolic form. `symEq_sound`: then they run alike,
for every fuel and state. `validateSym a b w` replays a script of proved rewrites on `a`
(`Opt/Witness.lean`: a block permutation, the CFG rewrites) and compares the result with `b`
by `symEq`: the script is the block map, symbolic equality the rest.

What it accepts beyond the script validator: a pass that rewrites operands inside a block
and keeps every store (copy and constant propagation of uses). It does not accept a store or
a local removed: there the states differ across steps (ledger V11).
-/

namespace Blaze.Mir

/-! Terminators. -/

inductive SymTerm where
  | switch (e : Expr) (arms : List (Int × BlockId)) (otherwise : BlockId)
  | check (e : Expr) (expected : Bool) (kind : Error) (target : BlockId)
  | other (t : Terminator)
  deriving DecidableEq

def switchOr (d : Operand) (arms : List (Int × BlockId)) (otherwise : BlockId) :
    Option Expr → SymTerm
  | some e => SymTerm.switch e arms otherwise
  | none => SymTerm.other (Terminator.switchInt d arms otherwise)

def checkOr (c : Operand) (expected : Bool) (kind : Error) (target : BlockId) :
    Option Expr → SymTerm
  | some e => SymTerm.check e expected kind target
  | none => SymTerm.other (Terminator.assert c expected kind target)

def symTerm (σ : List (Local × Expr)) : Terminator → SymTerm
  | Terminator.switchInt d arms otherwise => switchOr d arms otherwise (symOperand σ d)
  | Terminator.assert c expected kind target => checkOr c expected kind target (symOperand σ c)
  | Terminator.goto t => SymTerm.other (Terminator.goto t)
  | Terminator.call callee args dest target =>
    SymTerm.other (Terminator.call callee args dest target)
  | Terminator.ret => SymTerm.other Terminator.ret
  | Terminator.drop p t => SymTerm.other (Terminator.drop p t)
  | Terminator.unreachable => SymTerm.other Terminator.unreachable

/-- What a symbolic terminator means, in the state `st` the statements left, with the
condition evaluated in the initial state `st0`. -/
def denTerm (sig : FnId → Option FnSig) (st0 st : State) : SymTerm → Outcome Next
  | SymTerm.switch e arms otherwise =>
    (e.eval st0).bind (fun v => switchOn st arms otherwise (switchKey v))
  | SymTerm.check e expected kind target =>
    (e.eval st0).bind (fun v => assertOn st expected kind target v)
  | SymTerm.other t => execTerm sig st t

theorem execTerm_switch {st0 : State} {σ : List (Local × Expr)} {st : State}
    (sig : FnId → Option FnSig) (hr : Reads st0 σ st) (d : Operand)
    (arms : List (Int × BlockId)) (otherwise : BlockId) :
    ∀ (oe : Option Expr), symOperand σ d = oe →
      execTerm sig st (Terminator.switchInt d arms otherwise) =
        denTerm sig st0 st (switchOr d arms otherwise oe)
  | none, _ => rfl
  | some e, h => Outcome.bind_map_congr (evalOperand_sym hr d e h) (fun _ _ => rfl)

theorem execTerm_check {st0 : State} {σ : List (Local × Expr)} {st : State}
    (sig : FnId → Option FnSig) (hr : Reads st0 σ st) (c : Operand) (expected : Bool)
    (kind : Error) (target : BlockId) :
    ∀ (oe : Option Expr), symOperand σ c = oe →
      execTerm sig st (Terminator.assert c expected kind target) =
        denTerm sig st0 st (checkOr c expected kind target oe)
  | none, _ => rfl
  | some e, h => Outcome.bind_map_congr (evalOperand_sym hr c e h) (fun _ _ => rfl)

/-- A terminator does what its symbolic form means. -/
theorem execTerm_sym {st0 : State} {σ : List (Local × Expr)} {st : State}
    (sig : FnId → Option FnSig) (hr : Reads st0 σ st) :
    ∀ (t : Terminator), execTerm sig st t = denTerm sig st0 st (symTerm σ t)
  | Terminator.switchInt d arms otherwise =>
    execTerm_switch sig hr d arms otherwise (symOperand σ d) rfl
  | Terminator.assert c expected kind target =>
    execTerm_check sig hr c expected kind target (symOperand σ c) rfl
  | Terminator.goto _ => rfl
  | Terminator.call _ _ _ _ => rfl
  | Terminator.ret => rfl
  | Terminator.drop _ _ => rfl
  | Terminator.unreachable => rfl

/-! Blocks. -/

/-- The store after a trace: each entry put in front, in order. -/
def pushAll : List (Local × Expr) → List (Local × Expr) → List (Local × Expr)
  | σ, [] => σ
  | σ, entry :: rest => pushAll (entry :: σ) rest

/-- The state a trace leaves agrees with the store after the trace. -/
theorem den_reads (st0 : State) :
    ∀ (tr σ : List (Local × Expr)) (st st' : State), Reads st0 σ st →
      den st0 st tr = Outcome.ok st' → Reads st0 (pushAll σ tr) st'
  | [], _, _, _, hr, h => by
    have e := Outcome.ok.inj h
    rw [← e]
    exact hr
  | entry :: rest, σ, st, st', hr, h => by
    have h' : (entry.2.eval st0).bind (fun v =>
        (st.write { frame := st0.top, slot := entry.1, path := [] } v).bind (fun s =>
          den st0 s rest)) = Outcome.ok st' := h
    have ⟨v, he, r1⟩ := Outcome.bind_eq_ok _ _ _ h'
    have r1' : (st.write { frame := st0.top, slot := entry.1, path := [] } v).bind (fun s =>
        den st0 s rest) = Outcome.ok st' := r1
    have ⟨s1, hw, r2⟩ := Outcome.bind_eq_ok _ _ _ r1'
    exact den_reads st0 rest (entry :: σ) s1 st' (Reads.write hr entry.1 entry.2 v he hw) r2

def withTerm (t : Terminator) :
    Option (List (Local × Expr)) → Option (List (Local × Expr) × SymTerm)
  | some tr => some (tr, symTerm (pushAll [] tr) t)
  | none => none

/-- A block, symbolically: its statements' trace and its terminator on the store after them.
`none` when a statement is outside the subset. -/
def symBlock (blk : BasicBlock) : Option (List (Local × Expr) × SymTerm) :=
  withTerm blk.terminator (symStmts [] blk.stmts)

theorem withTerm_some (t : Terminator) :
    ∀ (o : Option (List (Local × Expr))) (x : List (Local × Expr) × SymTerm),
      withTerm t o = some x → o = some x.1 ∧ symTerm (pushAll [] x.1) t = x.2
  | some tr, x, h => by
    have e : (tr, symTerm (pushAll [] tr) t) = x := Option.some.inj h
    rw [← e]
    exact And.intro rfl rfl
  | none, _, h => none_eq_some_elim h

/-- A block does what its trace and its symbolic terminator mean. -/
theorem execBlock_sym (adts : List AdtDecl) (sig : FnId → Option FnSig) (st : State)
    (blk : BasicBlock) (x : List (Local × Expr) × SymTerm) (h : symBlock blk = some x) :
    execBlock adts sig st blk = (den st st x.1).bind (fun st' => denTerm sig st st' x.2) := by
  have ⟨htr, hterm⟩ := withTerm_some blk.terminator (symStmts [] blk.stmts) x h
  have stmts : execStmts adts st blk.stmts = den st st x.1 :=
    execStmts_sym adts st blk.stmts [] st x.1 (Reads.start st) htr
  have whole : execBlock adts sig st blk =
      (execStmts adts st blk.stmts).bind (fun st' => execTerm sig st' blk.terminator) :=
    execBlockFrom_stmts adts sig blk.terminator blk.stmts st
  exact whole.trans (Outcome.bind_congr stmts (fun st' hd =>
    (execTerm_sym sig (den_reads st x.1 [] st st' (Reads.start st) hd) blk.terminator).trans
      (congrTo (denTerm sig st st') hterm)))

def sameSome {α : Type} [DecidableEq α] : Option α → Option α → Bool
  | some x, some y => decide (x = y)
  | some _, none => false
  | none, _ => false

theorem sameSome_true {α : Type} [DecidableEq α] :
    ∀ (a b : Option α), sameSome a b = true → ∃ x, a = some x ∧ b = some x
  | some x, some y, h => ⟨x, rfl, congrTo some (of_decide_eq_true h).symm⟩
  | some _, none, h => false_eq_true_elim h
  | none, _, h => false_eq_true_elim h

/-- The blocks are equal, or have one symbolic form. -/
def blockEq (a b : BasicBlock) : Bool := decide (a = b) || sameSome (symBlock a) (symBlock b)

theorem blockEq_sound (adts : List AdtDecl) (sig : FnId → Option FnSig) (st : State)
    (a b : BasicBlock) (h : blockEq a b = true) :
    execBlock adts sig st b = execBlock adts sig st a := by
  cases or_true_cases h with
  | inl same =>
    have e : a = b := of_decide_eq_true same
    rw [e]
  | inr sym =>
    have ⟨x, ha, hb⟩ := sameSome_true (symBlock a) (symBlock b) sym
    exact (execBlock_sym adts sig st b x hb).trans (execBlock_sym adts sig st a x ha).symm

/-! Programs. -/

/-- The lists have one length and `p` holds of each pair. -/
def allPairs {α : Type} (p : α → α → Bool) : List α → List α → Bool
  | [], [] => true
  | x :: xs, y :: ys => p x y && allPairs p xs ys
  | [], _ :: _ => false
  | _ :: _, [] => false

def OptRel {α : Type} (p : α → α → Bool) : Option α → Option α → Prop
  | none, none => True
  | some x, some y => p x y = true
  | none, some _ => False
  | some _, none => False

theorem allPairs_nth {α : Type} (p : α → α → Bool) :
    ∀ (xs ys : List α) (i : Nat), allPairs p xs ys = true → OptRel p (nth? xs i) (nth? ys i)
  | [], [], _, _ => True.intro
  | [], _ :: _, _, h => false_eq_true_elim h
  | _ :: _, [], _, h => false_eq_true_elim h
  | _ :: _, _ :: _, 0, h => and_true_left h
  | _ :: xs, _ :: ys, i + 1, h => allPairs_nth p xs ys i (and_true_right h)

def bodyEq (a b : Body) : Bool :=
  decide (a.args = b.args) &&
    (decide (a.locals.length = b.locals.length) && allPairs blockEq a.blocks b.blocks)

/-- The programs have the same types and, function by function and block by block, blocks
that are equal or have one symbolic form. -/
def symEq (a b : Program) : Bool := decide (a.adts = b.adts) && allPairs bodyEq a.fns b.fns

theorem sig_of_rel (a b : Program) (id : FnId) (h : OptRel bodyEq (nth? a.fns id) (nth? b.fns id)) :
    b.sig id = a.sig id := by
  show (nth? b.fns id).map (fun body => ({ args := body.args, locals := body.locals.length } : FnSig)) =
    (nth? a.fns id).map (fun body => ({ args := body.args, locals := body.locals.length } : FnSig))
  cases ha : nth? a.fns id with
  | none =>
    cases hb : nth? b.fns id with
    | none => rfl
    | some y =>
      rw [ha, hb] at h
      exact False.elim h
  | some x =>
    cases hb : nth? b.fns id with
    | none =>
      rw [ha, hb] at h
      exact False.elim h
    | some y =>
      rw [ha, hb] at h
      have hxy : bodyEq x y = true := h
      have hargs : x.args = y.args := of_decide_eq_true (and_true_left hxy)
      have hlen : x.locals.length = y.locals.length :=
        of_decide_eq_true (and_true_left (and_true_right hxy))
      show some ({ args := y.args, locals := y.locals.length } : FnSig) =
        some ({ args := x.args, locals := x.locals.length } : FnSig)
      rw [hargs, hlen]

theorem block_of_rel (adts : List AdtDecl) (sig : FnId → Option FnSig) (st : State)
    (b : BlockId) :
    ∀ (oa ob : Option Body), OptRel bodyEq oa ob →
      execBlock? adts sig st (blockOf b ob) = execBlock? adts sig st (blockOf b oa)
  | none, none, _ => rfl
  | none, some _, h => False.elim h
  | some _, none, h => False.elim h
  | some x, some y, h => by
    have hxy : bodyEq x y = true := h
    have rel : OptRel blockEq (nth? x.blocks b) (nth? y.blocks b) :=
      allPairs_nth blockEq x.blocks y.blocks b (and_true_right (and_true_right hxy))
    show execBlock? adts sig st (nth? y.blocks b) = execBlock? adts sig st (nth? x.blocks b)
    cases hx : nth? x.blocks b with
    | none =>
      cases hy : nth? y.blocks b with
      | none => rfl
      | some by' =>
        rw [hx, hy] at rel
        exact False.elim rel
    | some bx =>
      cases hy : nth? y.blocks b with
      | none =>
        rw [hx, hy] at rel
        exact False.elim rel
      | some by' =>
        rw [hx, hy] at rel
        exact blockEq_sound adts sig st bx by' rel

/-- Symbolically equal programs step alike. -/
theorem step_symEq (a b : Program) (h : symEq a b = true) :
    ∀ (st : State), step b st = step a st
  | { base := _, stack := [], next := _ } => rfl
  | { base := base, stack := fr :: rest, next := next } => by
    have hadts : a.adts = b.adts := of_decide_eq_true (and_true_left h)
    have rel : ∀ (id : FnId), OptRel bodyEq (nth? a.fns id) (nth? b.fns id) :=
      fun id => allPairs_nth bodyEq a.fns b.fns id (and_true_right h)
    have hsig : b.sig = a.sig := funext (fun id => sig_of_rel a b id (rel id))
    show execBlock? b.adts b.sig { base := base, stack := fr :: rest, next := next }
        (blockOf fr.block (nth? b.fns fr.fn)) =
      execBlock? a.adts a.sig { base := base, stack := fr :: rest, next := next }
        (blockOf fr.block (nth? a.fns fr.fn))
    rw [← hadts, hsig]
    exact block_of_rel a.adts a.sig { base := base, stack := fr :: rest, next := next } fr.block
      (nth? a.fns fr.fn) (nth? b.fns fr.fn) (rel fr.fn)

/-- Symbolically equal programs run alike, for every fuel and every state. -/
theorem symEq_sound (a b : Program) (h : symEq a b = true) (fuel : Nat) (st : State) :
    run b fuel st = run a fuel st :=
  (Sim.of_step_eq a b (step_symEq a b h)).run fuel st True.intro

end Blaze.Mir

namespace Blaze.Mir.Opt

open Blaze.Mir

/-- Symbolically equal programs give every call the same outcome. -/
theorem symEq_call (a b : Program) (h : symEq a b = true) (n : Nat) (id : FnId)
    (base : List Slot) (args : List Value) : b.call n id base args = a.call n id base args := by
  have rel : ∀ (i : FnId), OptRel bodyEq (nth? a.fns i) (nth? b.fns i) :=
    fun i => allPairs_nth bodyEq a.fns b.fns i (and_true_right h)
  have hsig : b.sig = a.sig := funext (fun i => sig_of_rel a b i (rel i))
  have start : b.start id base args = a.start id base args := by
    unfold Program.start
    rw [hsig]
  exact Outcome.bind_congr start (fun st _ => symEq_sound a b h n st)

/-- Whether a script of proved rewrites takes `a` to a program symbolically equal to `b`. -/
def validateSym (a b : Program) (w : List Rewrite) : Bool := symEq (applyAll w a) b

/-- The validator is sound: every call of `a` whose caller hands over pointers into frame 0
only, that ends, and not in undefined behaviour, gives the same outcome in `b` with the same
fuel. -/
theorem validateSym_sound (a b : Program) (w : List Rewrite) (h : validateSym a b w = true)
    (n : Nat) (id : FnId) (base : List Slot) (args : List Value) (hz : FrameZero base args)
    (r : Outcome Value) (hr : a.call n id base args = r) (hd : r ≠ Outcome.diverge)
    (hu : ∀ (why : Undefined), r ≠ Outcome.undefined why) : b.call n id base args = r :=
  (symEq_call (applyAll w a) b h n id base args).trans
    (applyAll_preservesWhen w a n id base args r hz hr hd hu)

end Blaze.Mir.Opt
