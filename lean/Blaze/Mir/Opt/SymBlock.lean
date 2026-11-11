import Blaze.Mir.Opt.Sym
import Blaze.Mir.Opt.RemoveOverwritten

/-!
Symbolic execution, second piece: a block's statements as a trace.

`symStmts σ stmts` runs the statements on the symbolic store and gives their trace: the
locals assigned, in order, each with its expression over the block's initial locals (`none`
when a statement is outside the subset: a store to a part of a local, or an rvalue
`symRvalue` does not take). `den st0 st trace` is what a trace means: each expression is
evaluated in the initial state `st0` and written to its local.

`execStmts_sym`: executing the statements is the meaning of their trace. So two statement
lists with one trace do the same from every state (`execStmts_same`), whatever they look
like: that is what a pass that rewrites operands inside a block (copy and constant
propagation of uses) keeps.
-/

namespace Blaze.Mir

/-! Values that are not the hole of a move. -/

def Value.Live (v : Value) : Prop := v.checkLive = Outcome.ok v

theorem Value.live_int (ty : ScalarTy) (x : Scalar ty) : (Value.int ty x).Live := rfl

theorem Value.live_bool (b : Bool) : (Value.bool b).Live := rfl

theorem Value.live_adt (variant : Nat) (fields : List Value) :
    (Value.adt variant fields).Live := rfl

theorem constValue_live : ∀ (c : Const), (constValue c).Live
  | Const.unit => rfl
  | Const.bool _ => rfl
  | Const.int _ _ => rfl

theorem intResult_live {ty : ScalarTy} (r : Result (Scalar ty)) :
    Outcome.All Value.Live (intResult r) :=
  Outcome.all_bind (Outcome.all_true _) (fun v _ => Outcome.all_ok (Value.live_int ty v))

theorem sameBinary_live (op : BinOp) {ty : ScalarTy} (a b : Scalar ty) :
    Outcome.All Value.Live (sameBinary op a b) := by
  cases op <;>
    first
    | exact Outcome.all_ok (Value.live_int _ _)
    | exact Outcome.all_ok (Value.live_bool _)
    | exact intResult_live _

theorem sameChecked_live (op : BinOp) {ty : ScalarTy} (a b : Scalar ty) :
    Outcome.All Value.Live (sameChecked op a b) := by
  cases op <;>
    first
    | exact intResult_live _
    | exact Outcome.all_ok (Value.live_int _ _)
    | exact Outcome.all_ok (Value.live_bool _)

theorem sameOr_live {ty : ScalarTy} (g : Scalar ty → Outcome Value)
    (hg : ∀ (y : Scalar ty), Outcome.All Value.Live (g y)) :
    ∀ (o : Option (Scalar ty)), Outcome.All Value.Live (sameOr o g)
  | some b => hg b
  | none => Outcome.all_undefined _ _

theorem withSame_live {ty ty' : ScalarTy} (a : Scalar ty) (b : Scalar ty')
    (f : Scalar ty → Scalar ty → Outcome Value)
    (hf : ∀ (x y : Scalar ty), Outcome.All Value.Live (f x y)) :
    Outcome.All Value.Live (withSame a b f) :=
  sameOr_live (f a) (hf a) (asTy ty b)

theorem intBinary_live (checked : Bool) (op : BinOp) {ty ty' : ScalarTy}
    (a : Scalar ty) (b : Scalar ty') : Outcome.All Value.Live (intBinary checked op a b) := by
  cases checked <;> cases op <;>
    first
    | exact Outcome.all_ok (Value.live_int _ _)
    | exact intResult_live _
    | exact withSame_live a b _ (fun x y => sameBinary_live _ x y)
    | exact withSame_live a b _ (fun x y => sameChecked_live _ x y)

theorem boolBinary_live (op : BinOp) (a b : Bool) :
    Outcome.All Value.Live (boolBinary op a b) := by
  cases op <;>
    first
    | exact Outcome.all_ok (Value.live_bool _)
    | exact Outcome.all_undefined _ _

theorem binaryOp_live (checked : Bool) (op : BinOp) (a b : Value) :
    Outcome.All Value.Live (binaryOp checked op a b) := by
  cases a <;> cases b <;>
    first
    | exact intBinary_live _ _ _ _
    | exact boolBinary_live _ _ _
    | exact Outcome.all_undefined _ _

theorem sameOverflowing_live (op : BinOp) {ty : ScalarTy} (a b : Scalar ty) :
    Outcome.All Value.Live (sameOverflowing op a b) := by
  cases op <;>
    first
    | exact Outcome.all_ok (Value.live_adt _ _)
    | exact Outcome.all_undefined _ _

theorem overflowingOp_live (op : BinOp) (a b : Value) :
    Outcome.All Value.Live (overflowingOp op a b) := by
  cases a <;> cases b <;>
    first
    | exact withSame_live _ _ _ (fun x y => sameOverflowing_live _ x y)
    | exact Outcome.all_undefined _ _

theorem unaryOp_live (op : UnOp) (v : Value) :
    Outcome.All Value.Live (unaryOp op v) := by
  cases v <;> cases op <;>
    first
    | exact Outcome.all_ok (Value.live_bool _)
    | exact Outcome.all_ok (Value.live_int _ _)
    | exact intResult_live _
    | exact Outcome.all_undefined _ _

theorem castTo_live (target : ScalarTy) (v : Value) :
    Outcome.All Value.Live (castTo target v) := by
  cases v <;>
    first
    | exact Outcome.all_ok (Value.live_int _ _)
    | exact Outcome.all_undefined _ _

/-- A read gives a value that is not the hole of a move. -/
theorem State.read_live (st : State) (p : Pointer) : Outcome.All Value.Live (st.read p) := by
  intro v h
  have h' : (st.slot p).bind (fun slot =>
      slot.load.bind (fun x => (x.project p.path).bind Value.checkLive)) = Outcome.ok v := h
  have ⟨slot, _, r1⟩ := Outcome.bind_eq_ok _ _ _ h'
  have r1' : slot.load.bind (fun x => (x.project p.path).bind Value.checkLive) =
      Outcome.ok v := r1
  have ⟨x, _, r2⟩ := Outcome.bind_eq_ok _ _ _ r1'
  have r2' : (x.project p.path).bind Value.checkLive = Outcome.ok v := r2
  have ⟨y, _, r3⟩ := Outcome.bind_eq_ok _ _ _ r2'
  have r3' : y.checkLive = Outcome.ok v := r3
  have e : y = v := Value.checkLive_ok y v r3'
  rw [e] at r3'
  exact r3'

/-- An expression's value is not the hole of a move. -/
theorem Expr.eval_live (st0 : State) : ∀ (e : Expr), Outcome.All Value.Live (e.eval st0)
  | Expr.var x => State.read_live st0 { frame := st0.top, slot := x, path := [] }
  | Expr.const c => Outcome.all_ok (constValue_live c)
  | Expr.binary checked op _ _ =>
    Outcome.all_bind (Outcome.all_true _) (fun va _ =>
      Outcome.all_bind (Outcome.all_true _) (fun vb _ => binaryOp_live checked op va vb))
  | Expr.overflow op _ _ =>
    Outcome.all_bind (Outcome.all_true _) (fun va _ =>
      Outcome.all_bind (Outcome.all_true _) (fun vb _ => overflowingOp_live op va vb))
  | Expr.unary op _ => Outcome.all_bind (Outcome.all_true _) (fun va _ => unaryOp_live op va)
  | Expr.cast _ ty => Outcome.all_bind (Outcome.all_true _) (fun va _ => castTo_live ty va)

/-- A whole local written, then read: the value written. -/
theorem State.read_write_whole (st st' : State) (f l : Nat) (v : Value) (hv : v.Live)
    (h : st.write { frame := f, slot := l, path := [] } v = Outcome.ok st') :
    st'.read { frame := f, slot := l, path := [] } = Outcome.ok v := by
  have h' : (st.slot { frame := f, slot := l, path := [] }).bind (fun slot =>
      (Slot.store slot [] v).bind (fun slot' =>
        Outcome.ok (st.setSlot { frame := f, slot := l, path := [] } slot'))) =
          Outcome.ok st' := h
  have ⟨slot0, h1, r1⟩ := Outcome.bind_eq_ok _ _ _ h'
  have e : st.setSlot { frame := f, slot := l, path := [] } (Slot.init v) = st' :=
    Outcome.ok.inj r1
  have h1' : slotIn l (st.slots? f) = Outcome.ok slot0 := h1
  cases hs : st.slots? f with
  | none =>
    rw [hs] at h1'
    exact undefined_ne_ok h1'
  | some slots =>
    rw [hs] at h1'
    have h2 : slotOf (nth? slots l) = Outcome.ok slot0 := h1'
    have put : st.setSlot { frame := f, slot := l, path := [] } (Slot.init v) =
        st.setSlots f (setNth slots l (Slot.init v)) :=
      State.setSlot_some st { frame := f, slot := l, path := [] } (Slot.init v) slots hs
    have hs' : (st.setSlots f (setNth slots l (Slot.init v))).slots? f =
        some (setNth slots l (Slot.init v)) :=
      State.slots?_setSlots_some st f slots (setNth slots l (Slot.init v)) hs
    have slot' : st'.slot { frame := f, slot := l, path := [] } =
        slotOf ((nth? slots l).map (fun _ => Slot.init v)) := by
      rw [← e, put]
      show slotIn l ((st.setSlots f (setNth slots l (Slot.init v))).slots? f) = _
      rw [hs']
      exact congrTo slotOf (nth?_setNth_same slots l (Slot.init v))
    cases hn : nth? slots l with
    | none =>
      rw [hn] at h2
      exact undefined_ne_ok h2
    | some old =>
      rw [hn] at slot'
      have slot'' : st'.slot { frame := f, slot := l, path := [] } = Outcome.ok (Slot.init v) :=
        slot'
      have read : st'.read { frame := f, slot := l, path := [] } = v.checkLive :=
        congrTo
          (fun r => Outcome.bind r (fun (s : Slot) =>
            s.load.bind (fun x => (Value.project x []).bind Value.checkLive)))
          slot''
      exact read.trans hv

/-! Statements, concretely and symbolically. -/

/-- The statements in order. -/
def execStmts (adts : List AdtDecl) : State → List Statement → Outcome State
  | st, [] => Outcome.ok st
  | st, s :: rest => (execStmt adts st s).bind (fun st1 => execStmts adts st1 rest)

theorem execBlockFrom_stmts (adts : List AdtDecl) (sig : FnId → Option FnSig)
    (term : Terminator) :
    ∀ (stmts : List Statement) (st : State),
      execBlockFrom adts sig st stmts term =
        (execStmts adts st stmts).bind (fun st' => execTerm sig st' term)
  | [], _ => rfl
  | s :: rest, st => by
    show (execStmt adts st s).bind (fun st1 => execBlockFrom adts sig st1 rest term) =
      ((execStmt adts st s).bind (fun st1 => execStmts adts st1 rest)).bind
        (fun st' => execTerm sig st' term)
    cases execStmt adts st s with
    | ok st1 => exact execBlockFrom_stmts adts sig term rest st1
    | panic e => rfl
    | undefined why => rfl
    | diverge => rfl

/-- What a statement is, symbolically: outside the subset (`none`), nothing (`some none`),
or a whole local assigned an expression. -/
def assigned (l : Local) : Option Expr → Option (Option (Local × Expr))
  | some e => some (some (l, e))
  | none => none

def symStep (σ : List (Local × Expr)) : Statement → Option (Option (Local × Expr))
  | Statement.assign { base := l, proj := [] } rv => assigned l (symRvalue σ rv)
  | Statement.assign { base := _, proj := _ :: _ } _ => none
  | Statement.storageLive _ => some none
  | Statement.storageDead _ => some none
  | Statement.nop => some none

def consTrace (entry : Local × Expr) :
    Option (List (Local × Expr)) → Option (List (Local × Expr))
  | some t => some (entry :: t)
  | none => none

def symNext :
    Option (Option (Local × Expr)) → Option (List (Local × Expr)) →
      ((Local × Expr) → Option (List (Local × Expr))) → Option (List (Local × Expr))
  | none, _, _ => none
  | some none, same, _ => same
  | some (some entry), _, pushed => consTrace entry (pushed entry)

/-- The trace of the statements: the locals assigned, in order, with their expressions. -/
def symStmts : List (Local × Expr) → List Statement → Option (List (Local × Expr))
  | _, [] => some []
  | σ, s :: rest =>
    symNext (symStep σ s) (symStmts σ rest) (fun entry => symStmts (entry :: σ) rest)

theorem symStmts_cons (σ : List (Local × Expr)) (s : Statement) (rest : List Statement) :
    symStmts σ (s :: rest) =
      symNext (symStep σ s) (symStmts σ rest) (fun entry => symStmts (entry :: σ) rest) := rfl

/-- What a trace means: each expression evaluated in the initial state, and written. -/
def den (st0 : State) : State → List (Local × Expr) → Outcome State
  | st, [] => Outcome.ok st
  | st, entry :: rest =>
    (entry.2.eval st0).bind (fun v =>
      (st.write { frame := st0.top, slot := entry.1, path := [] } v).bind (fun st' =>
        den st0 st' rest))

theorem assigned_some (l : Local) :
    ∀ (o : Option Expr) (entry : Local × Expr), assigned l o = some (some entry) →
      ∃ e, o = some e ∧ (l, e) = entry
  | some e, _, h => ⟨e, rfl, Option.some.inj (Option.some.inj h)⟩
  | none, _, h => none_eq_some_elim h

theorem assigned_not_none (l : Local) {C : Prop} :
    ∀ (o : Option Expr), assigned l o = some none → C
  | some _, h => none_eq_some_elim (Option.some.inj h).symm
  | none, h => none_eq_some_elim h

theorem consTrace_some (entry : Local × Expr) :
    ∀ (o : Option (List (Local × Expr))) (tr : List (Local × Expr)),
      consTrace entry o = some tr → ∃ t, o = some t ∧ entry :: t = tr
  | some t, _, h => ⟨t, rfl, Option.some.inj h⟩
  | none, _, h => none_eq_some_elim h

/-- A statement that is nothing symbolically does nothing. -/
theorem execStmt_skip (adts : List AdtDecl) (σ : List (Local × Expr)) (st : State) :
    ∀ (s : Statement), symStep σ s = some none → execStmt adts st s = Outcome.ok st
  | Statement.assign { base := l, proj := [] } rv, h => assigned_not_none l (symRvalue σ rv) h
  | Statement.assign { base := _, proj := _ :: _ } _, h => none_eq_some_elim h
  | Statement.storageLive _, _ => rfl
  | Statement.storageDead _, _ => rfl
  | Statement.nop, _ => rfl

/-- A statement that assigns symbolically evaluates its expression and writes it. -/
theorem execStmt_assigned {st0 : State} {σ : List (Local × Expr)} {st : State}
    (adts : List AdtDecl) (hr : Reads st0 σ st) :
    ∀ (s : Statement) (entry : Local × Expr), symStep σ s = some (some entry) →
      execStmt adts st s =
        (entry.2.eval st0).bind (fun v =>
          st.write { frame := st0.top, slot := entry.1, path := [] } v)
  | Statement.assign { base := l, proj := [] } rv, entry, h => by
    have ⟨e, he, hentry⟩ := assigned_some l (symRvalue σ rv) entry h
    rw [← hentry]
    have value : evalRvalue adts st rv = Outcome.map (fun v => (v, st)) (e.eval st0) :=
      evalRvalue_sym adts hr rv e he
    have whole : execStmt adts st (Statement.assign (Place.mk l []) rv) =
        (evalRvalue adts st rv).bind (fun r =>
          r.2.write { frame := r.2.top, slot := l, path := [] } r.1) :=
      execStmt_whole adts st l rv
    have mapped : (evalRvalue adts st rv).bind (fun r =>
          r.2.write { frame := r.2.top, slot := l, path := [] } r.1) =
        (e.eval st0).bind (fun v => st.write { frame := st.top, slot := l, path := [] } v) :=
      Outcome.bind_map_congr value (fun _ _ => rfl)
    have top : (e.eval st0).bind (fun v => st.write { frame := st.top, slot := l, path := [] } v) =
        (e.eval st0).bind (fun v => st.write { frame := st0.top, slot := l, path := [] } v) :=
      congrTo
        (fun t => (e.eval st0).bind (fun v => st.write { frame := t, slot := l, path := [] } v))
        (And.left hr)
    exact whole.trans (mapped.trans top)
  | Statement.assign { base := _, proj := _ :: _ } _, _, h => none_eq_some_elim h
  | Statement.storageLive _, _, h => none_eq_some_elim (Option.some.inj h)
  | Statement.storageDead _, _, h => none_eq_some_elim (Option.some.inj h)
  | Statement.nop, _, h => none_eq_some_elim (Option.some.inj h)

/-- After a whole local is written the value of an expression, the state agrees with the
store that says so. -/
theorem Reads.write {st0 : State} {σ : List (Local × Expr)} {st st' : State}
    (hr : Reads st0 σ st) (l : Local) (e : Expr) (v : Value) (he : e.eval st0 = Outcome.ok v)
    (hw : st.write { frame := st0.top, slot := l, path := [] } v = Outcome.ok st') :
    Reads st0 ((l, e) :: σ) st' := by
  have top : st'.top = st0.top :=
    (State.top_of_ids st st'
      (State.write_ids st { frame := st0.top, slot := l, path := [] } v st' hw)).trans
      (And.left hr)
  refine And.intro top ?_
  intro x
  by_cases hx : l = x
  · have found : look ((l, e) :: σ) x = e := (look_cons (l, e) σ x).trans (if_pos hx)
    have live : v.Live := Expr.eval_live st0 e v he
    have read : st'.read { frame := st0.top, slot := l, path := [] } = Outcome.ok v :=
      State.read_write_whole st st' st0.top l v live hw
    rw [found, ← hx]
    exact read.trans he.symm
  · have found : look ((l, e) :: σ) x = look σ x := (look_cons (l, e) σ x).trans (if_neg hx)
    have other : st'.read { frame := st0.top, slot := x, path := [] } =
        st.read { frame := st0.top, slot := x, path := [] } :=
      State.read_write_other st st' { frame := st0.top, slot := l, path := [] }
        { frame := st0.top, slot := x, path := [] } v hw (Or.inr hx)
    rw [found]
    exact other.trans (And.right hr x)

/-- Executing the statements is the meaning of their trace. -/
theorem execStmts_sym (adts : List AdtDecl) (st0 : State) :
    ∀ (stmts : List Statement) (σ : List (Local × Expr)) (st : State)
      (tr : List (Local × Expr)), Reads st0 σ st → symStmts σ stmts = some tr →
        execStmts adts st stmts = den st0 st tr
  | [], _, _, tr, _, h => by
    have e : ([] : List (Local × Expr)) = tr := Option.some.inj h
    rw [← e]
    rfl
  | s :: rest, σ, st, tr, hr, h => by
    rw [symStmts_cons] at h
    cases hstep : symStep σ s with
    | none =>
      rw [hstep] at h
      exact none_eq_some_elim h
    | some step =>
      cases step with
      | none =>
        rw [hstep] at h
        have h' : symStmts σ rest = some tr := h
        have skip : execStmt adts st s = Outcome.ok st := execStmt_skip adts σ st s hstep
        have first : execStmts adts st (s :: rest) = execStmts adts st rest :=
          congrTo (fun r => Outcome.bind r (fun st1 => execStmts adts st1 rest)) skip
        exact first.trans (execStmts_sym adts st0 rest σ st tr hr h')
      | some entry =>
        rw [hstep] at h
        have h' : consTrace entry (symStmts (entry :: σ) rest) = some tr := h
        have ⟨t, ht, htr⟩ := consTrace_some entry (symStmts (entry :: σ) rest) tr h'
        have stmt : execStmt adts st s =
            (entry.2.eval st0).bind (fun v =>
              st.write { frame := st0.top, slot := entry.1, path := [] } v) :=
          execStmt_assigned adts hr s entry hstep
        have first : execStmts adts st (s :: rest) =
            ((entry.2.eval st0).bind (fun v =>
              st.write { frame := st0.top, slot := entry.1, path := [] } v)).bind
              (fun st1 => execStmts adts st1 rest) :=
          congrTo (fun r => Outcome.bind r (fun st1 => execStmts adts st1 rest)) stmt
        rw [first, ← htr]
        show ((entry.2.eval st0).bind (fun v =>
              st.write { frame := st0.top, slot := entry.1, path := [] } v)).bind
              (fun st1 => execStmts adts st1 rest) =
          (entry.2.eval st0).bind (fun v =>
            (st.write { frame := st0.top, slot := entry.1, path := [] } v).bind (fun st' =>
              den st0 st' t))
        cases he : entry.2.eval st0 with
        | ok v =>
          exact Outcome.bind_congr_same (fun st' hw =>
            execStmts_sym adts st0 rest (entry :: σ) st' t
              (Reads.write hr entry.1 entry.2 v he hw) ht)
        | panic e => rfl
        | undefined why => rfl
        | diverge => rfl

/-- Statement lists with one trace do the same from every state. -/
theorem execStmts_same (adts : List AdtDecl) (s1 s2 : List Statement)
    (tr : List (Local × Expr)) (h1 : symStmts [] s1 = some tr) (h2 : symStmts [] s2 = some tr)
    (st : State) : execStmts adts st s1 = execStmts adts st s2 :=
  (execStmts_sym adts st s1 [] st tr (Reads.start st) h1).trans
    (execStmts_sym adts st s2 [] st tr (Reads.start st) h2).symm

end Blaze.Mir
