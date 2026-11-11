import Blaze.Mir.Opt.SlotExec
import Blaze.Mir.Opt.SwitchSame
import Blaze.Mir.Opt.Local

/-!
A store that is overwritten in its own block, before anything mentions its local, is removed:
the part of `DeadStoreElimination` (crates/hir-ty-mir/src/mir/optimize/dse.rs) that needs no
liveness across blocks.

`removeOverwritten`: in a block, `l = rv1; ..; l = rv2` becomes `nop; ..; l = rv2` when
- `l` is a whole local its function never borrows (so no pointer is into its slot),
- `rv1` is a copy of a place with no index, or a constant (it cannot panic and moves nothing),
- no statement between mentions `l` as a place's base or as an index, and `rv2` does not.
One store per block per run; more are the pass again.

`removeOverwritten_refines`: the rewritten program refines the program (the same run, unless
the program's is undefined: the removed store's evaluation can be) from every state the
borrow invariant holds of (`Borrows`, `Opt/BorrowStep.lean`), for a program whose `ref`s are
of borrowed locals (`checkBorrows`, which the kernel evaluates). A step runs a whole block,
so the two programs' states are equal at every step, and the proof is `run_refines_inv`
with the block lemma `killFirst_refines`.

What the Rust pass does beyond this, none of it specified here: liveness over the whole
control-flow graph, unwinding included (a store is dead when no path reads its local before
writing it, not only when its own block overwrites it); every rvalue but a
pointer-to-address cast is removable there, where here only one that cannot panic or move
is; a `Deinit` is removable; a copy into a call of a local dead after it becomes a move;
and the storage markers of a local left unused go. The global form needs the two states
related across steps: equal but, per frame, on the locals dead at the frame's resume point.
`Opt/Ptr.lean`, `Opt/Borrow.lean`, `Opt/BorrowStep.lean`, `Opt/SlotAlgebra.lean` and
`Opt/SlotExec.lean` are what it would be built on.
-/

namespace Blaze.Mir

/-- When every step of the target refines the step of the source on the states an invariant
holds of, and the source's steps keep the invariant, every run from such a state refines. -/
theorem run_refines_inv (src tgt : Program) (Inv : State → Prop)
    (hstep : ∀ (st : State), Inv st → (step tgt st).Refines (step src st))
    (hpres : ∀ (st st' : State), Inv st → step src st = Outcome.ok (Next.state st') → Inv st') :
    ∀ (fuel : Nat) (st : State), Inv st → (run tgt fuel st).Refines (run src fuel st) := by
  intro fuel
  induction fuel with
  | zero =>
    intro st _
    exact Outcome.Refines.of_eq rfl
  | succ n ih =>
    intro st hinv
    have hs : (∃ why, step src st = Outcome.undefined why) ∨ step tgt st = step src st :=
      hstep st hinv
    rw [run_succ, run_succ]
    cases hs with
    | inl hu =>
      have ⟨why, e⟩ := hu
      rw [e]
      exact Or.inl ⟨why, rfl⟩
    | inr e =>
      rw [e]
      cases hsrc : step src st with
      | ok next =>
        cases next with
        | state st' => exact ih st' (hpres st st' hinv hsrc)
        | done v => exact Outcome.Refines.of_eq rfl
      | panic e => exact Outcome.Refines.of_eq rfl
      | undefined why => exact Outcome.Refines.of_eq rfl
      | diverge => exact Outcome.Refines.of_eq rfl

/-- Two outcomes of a step are related: next states `R` relates, or the same answer, panic,
undefined behaviour or lack of fuel. -/
inductive StepRel (R : State → State → Prop) : Outcome Next → Outcome Next → Prop where
  | state (s t : State) (h : R s t) :
    StepRel R (Outcome.ok (Next.state s)) (Outcome.ok (Next.state t))
  | done (v : Value) : StepRel R (Outcome.ok (Next.done v)) (Outcome.ok (Next.done v))
  | panic (e : Error) : StepRel R (Outcome.panic e) (Outcome.panic e)
  | undefined (why : Undefined) : StepRel R (Outcome.undefined why) (Outcome.undefined why)
  | diverge : StepRel R Outcome.diverge Outcome.diverge

/-- The relational form of `run_refines_inv`: the two programs run from states `R` relates,
not from one state. When from related states the source's step is undefined or the two steps
are related, every run of the target from a state refines the source's from a related one. -/
theorem run_refines_rel (src tgt : Program) (R : State → State → Prop)
    (hstep : ∀ (s t : State), R s t →
      (∃ why, step src s = Outcome.undefined why) ∨ StepRel R (step src s) (step tgt t)) :
    ∀ (fuel : Nat) (s t : State), R s t → (run tgt fuel t).Refines (run src fuel s) := by
  intro fuel
  induction fuel with
  | zero =>
    intro s t _
    exact Outcome.Refines.of_eq rfl
  | succ n ih =>
    intro s t hr
    have after : ∀ (a b : Outcome Next), StepRel R a b →
        (afterStep (run tgt n) b).Refines (afterStep (run src n) a) := by
      intro a b h
      cases h with
      | state s' t' h' => exact ih s' t' h'
      | done v => exact Outcome.Refines.of_eq rfl
      | panic e => exact Outcome.Refines.of_eq rfl
      | undefined why => exact Outcome.Refines.of_eq rfl
      | diverge => exact Outcome.Refines.of_eq rfl
    rw [run_succ, run_succ]
    cases hstep s t hr with
    | inl hu =>
      have ⟨why, e⟩ := hu
      rw [e]
      exact Or.inl ⟨why, rfl⟩
    | inr rel => exact after (step src s) (step tgt t) rel

/-! An rvalue that cannot panic and moves nothing. -/

def operandCalm : Operand → Bool
  | Operand.copy p => plainProj p.proj
  | Operand.move _ => false
  | Operand.const _ => true

def rvalueCalm : Rvalue → Bool
  | Rvalue.use o => operandCalm o
  | Rvalue.binary _ _ _ => false
  | Rvalue.checked _ _ _ => false
  | Rvalue.withOverflow _ _ _ => false
  | Rvalue.unary _ _ => false
  | Rvalue.cast _ _ => false
  | Rvalue.aggregate _ _ => false
  | Rvalue.replicate _ _ => false
  | Rvalue.discriminant _ _ => false
  | Rvalue.ref _ _ => false
  | Rvalue.len _ => false

/-- A calm rvalue is its value, with the state as it was, or undefined. -/
theorem evalRvalue_calm (adts : List AdtDecl) (st : State) :
    ∀ (rv : Rvalue), rvalueCalm rv = true →
      (∃ v, evalRvalue adts st rv = Outcome.ok (v, st)) ∨
        ∃ why, evalRvalue adts st rv = Outcome.undefined why
  | Rvalue.use (Operand.copy p), h => evalCopy_calm st p h
  | Rvalue.use (Operand.move _), h => false_eq_true_elim h
  | Rvalue.use (Operand.const c), _ => Or.inl ⟨constValue c, rfl⟩
  | Rvalue.binary _ _ _, h => false_eq_true_elim h
  | Rvalue.checked _ _ _, h => false_eq_true_elim h
  | Rvalue.withOverflow _ _ _, h => false_eq_true_elim h
  | Rvalue.unary _ _, h => false_eq_true_elim h
  | Rvalue.cast _ _, h => false_eq_true_elim h
  | Rvalue.aggregate _ _, h => false_eq_true_elim h
  | Rvalue.replicate _ _, h => false_eq_true_elim h
  | Rvalue.discriminant _ _, h => false_eq_true_elim h
  | Rvalue.ref _ _, h => false_eq_true_elim h
  | Rvalue.len _, h => false_eq_true_elim h

/-! A store to a whole local. -/

/-- A write of a whole slot replaces the slot, or the slot is not there. -/
theorem State.write_whole (st : State) (f l : Nat) (v : Value) :
    st.write { frame := f, slot := l, path := [] } v = Outcome.ok (st.put f l (Slot.init v)) ∨
      ∃ why, st.write { frame := f, slot := l, path := [] } v = Outcome.undefined why := by
  have hc : (∃ a, st.slot { frame := f, slot := l, path := [] } = Outcome.ok a) ∨
      ∃ why, st.slot { frame := f, slot := l, path := [] } = Outcome.undefined why :=
    State.slot_calm st { frame := f, slot := l, path := [] }
  cases hc with
  | inl ha =>
    have ⟨a, e⟩ := ha
    exact Or.inl
      (congrTo
        (fun x => Outcome.bind x (fun slot =>
          (Slot.store slot [] v).bind (fun slot' =>
            Outcome.ok (st.setSlot { frame := f, slot := l, path := [] } slot'))))
        e)
  | inr hu =>
    have ⟨why, e⟩ := hu
    exact Or.inr ⟨why,
      congrTo
        (fun x => Outcome.bind x (fun slot =>
          (Slot.store slot [] v).bind (fun slot' =>
            Outcome.ok (st.setSlot { frame := f, slot := l, path := [] } slot'))))
        e⟩

theorem execStmt_whole (adts : List AdtDecl) (st : State) (l : Local) (rv : Rvalue) :
    execStmt adts st (Statement.assign (Place.mk l []) rv) =
      (evalRvalue adts st rv).bind (fun r =>
        r.2.write { frame := r.2.top, slot := l, path := [] } r.1) := rfl

/-- A store to the whole hidden local forgets what its slot held. -/
theorem execStmt_overwrite {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    (adts : List AdtDecl) {st : State} (h : Hides B g f l st) (sl : Slot) (fn : FnId)
    (hfn : st.topFn = fn) (rv : Rvalue) (hB : RvalueFine B fn rv)
    (hfine : rvalueFine l rv = true) :
    execStmt adts (st.put f l sl) (Statement.assign (Place.mk l []) rv) =
      execStmt adts st (Statement.assign (Place.mk l []) rv) := by
  rw [execStmt_whole, execStmt_whole]
  exact Outcome.bind_map_congr (evalRvalue_put adts h sl rv hfine) (fun r hr => by
    have hr2 : Hides B g f l r.2 :=
      h.next (And.right (evalRvalue_okAt adts st h.okAt fn hfn rv hB r hr))
        (evalRvalue_ids adts st rv r hr)
    have e1 : (r.2.put f l sl).top = f := (State.put_top r.2 f l sl).trans hr2.top
    have e2 : r.2.top = f := hr2.top
    show (r.2.put f l sl).write { frame := (r.2.put f l sl).top, slot := l, path := [] } r.1 =
      r.2.write { frame := r.2.top, slot := l, path := [] } r.1
    rw [e1, e2]
    exact State.write_setSlot_same r.2 f l sl r.1)

/-! The statements after the store: an overwrite of `l` before any mention of it. -/

/-- A store to the whole local `l` of an rvalue that does not mention `l`. -/
def isOverwrite (l : Local) : Statement → Bool
  | Statement.assign p rv => decide (p.base = l) && (decide (p.proj = []) && rvalueFine l rv)
  | Statement.storageLive _ => false
  | Statement.storageDead _ => false
  | Statement.nop => false

/-- The statements overwrite `l` before any of them mentions it. -/
def overwrites (l : Local) : List Statement → Bool
  | [] => false
  | s :: rest => isOverwrite l s || (stmtFine l s && overwrites l rest)

theorem isOverwrite_exec {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    (adts : List AdtDecl) {st : State} (h : Hides B g f l st) (sl : Slot) (fn : FnId)
    (hfn : st.topFn = fn) :
    ∀ (s : Statement), StmtFine B fn s → isOverwrite l s = true →
      execStmt adts (st.put f l sl) s = execStmt adts st s
  | Statement.assign { base := base, proj := proj } rv, hB, ho => by
    have hb : base = l := of_decide_eq_true (and_true_left ho)
    have hp : proj = [] := of_decide_eq_true (and_true_left (and_true_right ho))
    have hrv : rvalueFine l rv = true := and_true_right (and_true_right ho)
    subst hb
    subst hp
    exact execStmt_overwrite adts h sl fn hfn rv hB hrv
  | Statement.storageLive _, _, ho => false_eq_true_elim ho
  | Statement.storageDead _, _, ho => false_eq_true_elim ho
  | Statement.nop, _, ho => false_eq_true_elim ho

/-- Statements that overwrite the hidden local before mentioning it do the same whatever its
slot holds. -/
theorem execBlockFrom_put {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    (adts : List AdtDecl) (sig : FnId → Option FnSig) (term : Terminator) (fn : FnId)
    (sl : Slot) :
    ∀ (stmts : List Statement) (st : State), Hides B g f l st → st.topFn = fn →
      (∀ (s : Statement), s ∈ stmts → StmtFine B fn s) → overwrites l stmts = true →
        execBlockFrom adts sig (st.put f l sl) stmts term = execBlockFrom adts sig st stmts term
  | [], _, _, _, _, hov => false_eq_true_elim hov
  | s :: rest, st, hh, hfn, hB, hov => by
    have hsB : StmtFine B fn s := hB s (List.mem_cons.mpr (Or.inl rfl))
    cases ho : isOverwrite l s with
    | true =>
      have e : execStmt adts (st.put f l sl) s = execStmt adts st s :=
        isOverwrite_exec adts hh sl fn hfn s hsB ho
      exact Outcome.bind_congr e (fun _ _ => rfl)
    | false =>
      have hov' : (isOverwrite l s || (stmtFine l s && overwrites l rest)) = true := hov
      rw [ho] at hov'
      have hrest : (stmtFine l s && overwrites l rest) = true := hov'
      exact Outcome.bind_map_congr
        (execStmt_put adts hh sl fn hfn s hsB (and_true_left hrest))
        (fun st1 h1 =>
          execBlockFrom_put adts sig term fn sl rest st1
            (hh.next (execStmt_okAt adts st hh.okAt fn hfn s hsB st1 h1)
              (execStmt_ids adts st s st1 h1))
            ((State.topFn_of_ctl (execStmt_ctl adts st s st1 h1)).trans hfn)
            (fun s' m => hB s' (List.mem_cons.mpr (Or.inr m))) (and_true_right hrest))

/-- The store removed: the block without it refines the block with it. -/
theorem kill_refines {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    (adts : List AdtDecl) (sig : FnId → Option FnSig) (term : Terminator) (fn : FnId)
    {st : State} (hh : Hides B g f l st) (hfn : st.topFn = fn) (rv : Rvalue)
    (hcalm : rvalueCalm rv = true) (rest : List Statement)
    (hB : ∀ (s : Statement), s ∈ rest → StmtFine B fn s) (hov : overwrites l rest = true) :
    (execBlockFrom adts sig st (Statement.nop :: rest) term).Refines
      (execBlockFrom adts sig st (Statement.assign (Place.mk l []) rv :: rest) term) := by
  have tgt : execBlockFrom adts sig st (Statement.nop :: rest) term =
      execBlockFrom adts sig st rest term := rfl
  have block : ∀ (o : Outcome State),
      execStmt adts st (Statement.assign (Place.mk l []) rv) = o →
        execBlockFrom adts sig st (Statement.assign (Place.mk l []) rv :: rest) term =
          o.bind (fun st1 => execBlockFrom adts sig st1 rest term) :=
    fun o e => congrTo (fun x => Outcome.bind x (fun st1 => execBlockFrom adts sig st1 rest term)) e
  cases evalRvalue_calm adts st rv hcalm with
  | inr hu =>
    have ⟨why, e⟩ := hu
    have stmt : execStmt adts st (Statement.assign (Place.mk l []) rv) =
        Outcome.undefined why :=
      congrTo
        (fun x => Outcome.bind x (fun (r : Value × State) =>
          r.2.write { frame := r.2.top, slot := l, path := [] } r.1))
        e
    exact Or.inl ⟨why, block (Outcome.undefined why) stmt⟩
  | inl hv =>
    have ⟨v, e⟩ := hv
    have stmt0 : execStmt adts st (Statement.assign (Place.mk l []) rv) =
        st.write { frame := st.top, slot := l, path := [] } v :=
      congrTo
        (fun x => Outcome.bind x (fun (r : Value × State) =>
          r.2.write { frame := r.2.top, slot := l, path := [] } r.1))
        e
    have stmt1 : execStmt adts st (Statement.assign (Place.mk l []) rv) =
        st.write { frame := f, slot := l, path := [] } v :=
      stmt0.trans (congrTo (fun t => st.write { frame := t, slot := l, path := [] } v) hh.top)
    cases State.write_whole st f l v with
    | inl written =>
      have src : execBlockFrom adts sig st (Statement.assign (Place.mk l []) rv :: rest) term =
          execBlockFrom adts sig (st.put f l (Slot.init v)) rest term :=
        block (Outcome.ok (st.put f l (Slot.init v))) (stmt1.trans written)
      have same : execBlockFrom adts sig (st.put f l (Slot.init v)) rest term =
          execBlockFrom adts sig st rest term :=
        execBlockFrom_put adts sig term fn (Slot.init v) rest st hh hfn hB hov
      exact Or.inr (tgt.trans (src.trans same).symm)
    | inr hu =>
      have ⟨why, e2⟩ := hu
      exact Or.inl ⟨why, block (Outcome.undefined why) (stmt1.trans e2)⟩

end Blaze.Mir

namespace Blaze.Mir.Opt

open Blaze.Mir

theorem not_true_false {a : Bool} (h : (!a) = true) : a = false := by
  cases a with
  | true => exact false_eq_true_elim h
  | false => rfl

/-- The store `p = rv` before `rest` can go. `bw` says which locals the function borrows. -/
def killAssign (bw : Local → Bool) (p : Place) (rv : Rvalue) (rest : List Statement) : Bool :=
  decide (p.proj = []) && (!bw p.base && (rvalueCalm rv && overwrites p.base rest))

def killable (bw : Local → Bool) : Statement → List Statement → Bool
  | Statement.assign p rv, rest => killAssign bw p rv rest
  | Statement.storageLive _, _ => false
  | Statement.storageDead _, _ => false
  | Statement.nop, _ => false

/-- The statements with the first store that can go made a `nop`. -/
def killFirst (bw : Local → Bool) : List Statement → List Statement
  | [] => []
  | s :: rest => if killable bw s rest = true then Statement.nop :: rest else s :: killFirst bw rest

theorem killFirst_cons (bw : Local → Bool) (s : Statement) (rest : List Statement) :
    killFirst bw (s :: rest) =
      if killable bw s rest = true then Statement.nop :: rest else s :: killFirst bw rest := rfl

theorem killable_refines {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f : Nat}
    (adts : List AdtDecl) (sig : FnId → Option FnSig) (term : Terminator) (fn : FnId)
    (bw : Local → Bool) (hbw : ∀ (l : Local), bw l = false → ¬ B fn l)
    (hf : fnOfIds g f = some fn) (hpos : ¬ f = 0) {st : State} (hs : OkAt B g st)
    (htop : st.top = f) (hfn : st.topFn = fn) (rest : List Statement)
    (hB : ∀ (s : Statement), s ∈ rest → StmtFine B fn s) :
    ∀ (s : Statement), killable bw s rest = true →
      (execBlockFrom adts sig st (Statement.nop :: rest) term).Refines
        (execBlockFrom adts sig st (s :: rest) term)
  | Statement.assign { base := base, proj := proj } rv, hk => by
    have hk' : (decide (proj = []) && (!bw base && (rvalueCalm rv && overwrites base rest))) =
        true := hk
    have hp : proj = [] := of_decide_eq_true (and_true_left hk')
    have hnb : bw base = false := not_true_false (and_true_left (and_true_right hk'))
    have hcalm : rvalueCalm rv = true := and_true_left (and_true_right (and_true_right hk'))
    have hov : overwrites base rest = true :=
      and_true_right (and_true_right (and_true_right hk'))
    subst hp
    have hides : Hides B g f base st :=
      { okAt := hs, top := htop,
        off := ptrOk_off B g f base fn hf (hbw base hnb) hpos }
    exact kill_refines adts sig term fn hides hfn rv hcalm rest hB hov
  | Statement.storageLive _, hk => false_eq_true_elim hk
  | Statement.storageDead _, hk => false_eq_true_elim hk
  | Statement.nop, hk => false_eq_true_elim hk

/-- A block with its first overwritten store removed refines the block. -/
theorem killFirst_refines {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f : Nat}
    (adts : List AdtDecl) (sig : FnId → Option FnSig) (term : Terminator) (fn : FnId)
    (bw : Local → Bool) (hbw : ∀ (l : Local), bw l = false → ¬ B fn l)
    (hf : fnOfIds g f = some fn) (hpos : ¬ f = 0) :
    ∀ (stmts : List Statement) (st : State), OkAt B g st → st.top = f → st.topFn = fn →
      (∀ (s : Statement), s ∈ stmts → StmtFine B fn s) →
        (execBlockFrom adts sig st (killFirst bw stmts) term).Refines
          (execBlockFrom adts sig st stmts term)
  | [], _, _, _, _, _ => Outcome.Refines.of_eq rfl
  | s :: rest, st, hs, htop, hfn, hB => by
    rw [killFirst_cons]
    by_cases hk : killable bw s rest = true
    · rw [if_pos hk]
      exact killable_refines adts sig term fn bw hbw hf hpos hs htop hfn rest
        (fun s' m => hB s' (List.mem_cons.mpr (Or.inr m))) s hk
    · rw [if_neg hk]
      have hsB : StmtFine B fn s := hB s (List.mem_cons.mpr (Or.inl rfl))
      exact Outcome.Refines.bind (fun st1 h1 =>
        killFirst_refines adts sig term fn bw hbw hf hpos rest st1
          (execStmt_okAt adts st hs fn hfn s hsB st1 h1)
          ((State.top_of_ids st st1 (execStmt_ids adts st s st1 h1)).trans htop)
          ((State.topFn_of_ctl (execStmt_ctl adts st s st1 h1)).trans hfn)
          (fun s' m => hB s' (List.mem_cons.mpr (Or.inr m))))

/-! The pass. -/

def removeOverwrittenBlock (bw : Local → Bool) (blk : BasicBlock) : BasicBlock :=
  { stmts := killFirst bw blk.stmts, terminator := blk.terminator }

def removeOverwrittenBody (borrowed : Body → Local → Bool) (body : Body) : Body :=
  body.mapBlocks (removeOverwrittenBlock (borrowed body))

/-- The pass. `borrowed body l` says whether the body ever takes a reference to its local
`l`. -/
def removeOverwritten (borrowed : Body → Local → Bool) (prog : Program) : Program :=
  { adts := prog.adts, fns := prog.fns.map (removeOverwrittenBody borrowed) }

def borrowedOf (borrowed : Body → Local → Bool) : Option Body → Local → Bool
  | some body => borrowed body
  | none => fun _ => false

/-- The locals each function of the program may have a pointer to. -/
def BorrowedIn (borrowed : Body → Local → Bool) (prog : Program) : FnId → Local → Prop :=
  fun fn l => borrowedOf borrowed (nth? prog.fns fn) l = true

theorem sig_removeOverwritten (borrowed : Body → Local → Bool) (prog : Program) :
    (removeOverwritten borrowed prog).sig = prog.sig := by
  funext id
  show (nth? (prog.fns.map (removeOverwrittenBody borrowed)) id).map _ =
    (nth? prog.fns id).map _
  rw [nth?_map]
  cases nth? prog.fns id with
  | none => rfl
  | some body => rfl

theorem blockOf_removeOverwritten (borrowed : Body → Local → Bool) (b : BlockId) :
    ∀ (o : Option Body),
      blockOf b (o.map (removeOverwrittenBody borrowed)) =
        (blockOf b o).map (removeOverwrittenBlock (borrowedOf borrowed o))
  | none => rfl
  | some body => nth?_map (removeOverwrittenBlock (borrowed body)) body.blocks b

theorem block_removeOverwritten (borrowed : Body → Local → Bool) (prog : Program) (fn : FnId)
    (b : BlockId) :
    (removeOverwritten borrowed prog).block fn b =
      (prog.block fn b).map
        (removeOverwrittenBlock (borrowedOf borrowed (nth? prog.fns fn))) := by
  show blockOf b (nth? (prog.fns.map (removeOverwrittenBody borrowed)) fn) =
    (blockOf b (nth? prog.fns fn)).map
      (removeOverwrittenBlock (borrowedOf borrowed (nth? prog.fns fn)))
  rw [nth?_map]
  exact blockOf_removeOverwritten borrowed b (nth? prog.fns fn)

/-- One step of the rewritten program refines the step of the program, on a state the borrow
invariant holds of. -/
theorem removeOverwritten_step (borrowed : Body → Local → Bool) (prog : Program)
    (hfine : ProgFine (BorrowedIn borrowed prog) prog) :
    ∀ (st : State), (∃ g, Borrows (BorrowedIn borrowed prog) g st) →
      (step (removeOverwritten borrowed prog) st).Refines (step prog st)
  | { base := _, stack := [], next := _ }, _ => Outcome.Refines.of_eq rfl
  | { base := base, stack := fr :: rest, next := next }, hinv => by
    have ⟨g, hb⟩ := hinv
    have rewritten :
        step (removeOverwritten borrowed prog) { base := base, stack := fr :: rest, next := next } =
          execBlock? prog.adts prog.sig { base := base, stack := fr :: rest, next := next }
            ((prog.block fr.fn fr.block).map
              (removeOverwrittenBlock (borrowedOf borrowed (nth? prog.fns fr.fn)))) := by
      show execBlock? prog.adts (removeOverwritten borrowed prog).sig
          { base := base, stack := fr :: rest, next := next }
          ((removeOverwritten borrowed prog).block fr.fn fr.block) = _
      rw [block_removeOverwritten, sig_removeOverwritten]
    have src : step prog { base := base, stack := fr :: rest, next := next } =
        execBlock? prog.adts prog.sig { base := base, stack := fr :: rest, next := next }
          (prog.block fr.fn fr.block) := rfl
    rw [rewritten, src]
    cases hblk : prog.block fr.fn fr.block with
    | none => exact Outcome.Refines.of_eq rfl
    | some blk =>
      have hcov : fnOfIds g fr.id = some fr.fn :=
        And.left (And.left hb) (fr.id, fr.fn) (List.mem_cons.mpr (Or.inl rfl))
      have pos : 0 < fr.id := KeysBelow.pos (And.right hb) fr.id fr.fn hcov
      have hpos : ¬ fr.id = 0 := by omega
      exact killFirst_refines prog.adts prog.sig blk.terminator fr.fn
        (borrowedOf borrowed (nth? prog.fns fr.fn))
        (fun l hl hB => true_eq_false_elim (Eq.trans (Eq.symm hB) hl)) hcov hpos blk.stmts
        { base := base, stack := fr :: rest, next := next } (And.left hb) rfl rfl
        (hfine fr.fn fr.block blk hblk)

/-- Removing a store overwritten in its own block refines the program: for every fuel and
every state the borrow invariant holds of, the rewritten program's run is the program's,
unless the program's is undefined. -/
theorem removeOverwritten_refines (borrowed : Body → Local → Bool) (prog : Program)
    (hfine : ProgFine (BorrowedIn borrowed prog) prog) (fuel : Nat) (st : State)
    (hinv : ∃ g, Borrows (BorrowedIn borrowed prog) g st) :
    (run (removeOverwritten borrowed prog) fuel st).Refines (run prog fuel st) :=
  run_refines_inv prog (removeOverwritten borrowed prog)
    (fun s => ∃ g, Borrows (BorrowedIn borrowed prog) g s)
    (removeOverwritten_step borrowed prog hfine)
    (fun s s' hs hstep => by
      have ⟨g, hb⟩ := hs
      exact step_borrows hfine s hb (Next.state s') hstep)
    fuel st hinv

/-! Calls. A call's start state holds the borrow invariant when what the caller hands over
has pointers into frame 0 only. -/

theorem start_removeOverwritten (borrowed : Body → Local → Bool) (prog : Program) (id : FnId)
    (base : List Slot) (args : List Value) :
    (removeOverwritten borrowed prog).start id base args = prog.start id base args := by
  unfold Program.start
  rw [sig_removeOverwritten]

theorem start_borrows (B : FnId → Local → Prop) (prog : Program) (id : FnId) (base : List Slot)
    (args : List Value) (hbase : SlotsOk (PtrOk B []) base)
    (hargs : ∀ (v : Value), v ∈ args → Value.Ok (PtrOk B []) v) :
    Outcome.All (fun (st : State) => ∃ g, Borrows B g st) (prog.start id base args) := by
  have h0 : Borrows B [] { base := base, stack := [], next := 1 } :=
    And.intro
      (And.intro (fun _ he => mem_nil_elim he)
        (And.intro hbase (fun _ m => mem_nil_elim m)))
      (And.intro (Nat.succ_pos 0) (fun _ he => mem_nil_elim he))
  unfold Program.start
  cases hs : prog.sig id with
  | none => exact Outcome.all_undefined _ _
  | some s =>
    exact Outcome.all_bind
      (State.enter_borrows h0 id s args { frame := 0, slot := 0, path := [] } none hargs)
      (fun next hn => by
        cases next with
        | state st => exact Outcome.all_ok hn
        | done v => exact Outcome.all_undefined _ _)

/-- Removing a store overwritten in its own block refines every call whose base slots and
arguments have pointers into frame 0 only: the same outcome, unless the program's is
undefined. -/
theorem removeOverwritten_call (borrowed : Body → Local → Bool) (prog : Program)
    (hfine : ProgFine (BorrowedIn borrowed prog) prog) (fuel : Nat) (id : FnId)
    (base : List Slot) (args : List Value)
    (hbase : SlotsOk (PtrOk (BorrowedIn borrowed prog) []) base)
    (hargs : ∀ (v : Value), v ∈ args → Value.Ok (PtrOk (BorrowedIn borrowed prog) []) v) :
    ((removeOverwritten borrowed prog).call fuel id base args).Refines
      (prog.call fuel id base args) := by
  have es : (removeOverwritten borrowed prog).start id base args = prog.start id base args :=
    start_removeOverwritten borrowed prog id base args
  show Outcome.Refines
    (((removeOverwritten borrowed prog).start id base args).bind
      (run (removeOverwritten borrowed prog) fuel))
    ((prog.start id base args).bind (run prog fuel))
  rw [es]
  exact Outcome.Refines.bind (fun st hst =>
    removeOverwritten_refines borrowed prog hfine fuel st
      (start_borrows (BorrowedIn borrowed prog) prog id base args hbase hargs st hst))

/-! The condition on the program, as a check the kernel evaluates. -/

theorem or_true_cases {a b : Bool} (h : (a || b) = true) : a = true ∨ b = true := by
  cases a with
  | true => exact Or.inl rfl
  | false => exact Or.inr h

/-- A `ref` is of a borrowed local or through a dereference. -/
def refFine (bw : Local → Bool) : Rvalue → Bool
  | Rvalue.ref _ p => bw p.base || hasDeref p.proj
  | Rvalue.use _ => true
  | Rvalue.binary _ _ _ => true
  | Rvalue.checked _ _ _ => true
  | Rvalue.withOverflow _ _ _ => true
  | Rvalue.unary _ _ => true
  | Rvalue.cast _ _ => true
  | Rvalue.aggregate _ _ => true
  | Rvalue.replicate _ _ => true
  | Rvalue.discriminant _ _ => true
  | Rvalue.len _ => true

def stmtFineB (bw : Local → Bool) : Statement → Bool
  | Statement.assign _ rv => refFine bw rv
  | Statement.storageLive _ => true
  | Statement.storageDead _ => true
  | Statement.nop => true

def stmtsFineB (bw : Local → Bool) : List Statement → Bool
  | [] => true
  | s :: rest => stmtFineB bw s && stmtsFineB bw rest

def blocksFineB (bw : Local → Bool) : List BasicBlock → Bool
  | [] => true
  | blk :: rest => stmtsFineB bw blk.stmts && blocksFineB bw rest

def fnsFineB (borrowed : Body → Local → Bool) : List Body → Bool
  | [] => true
  | body :: rest => blocksFineB (borrowed body) body.blocks && fnsFineB borrowed rest

/-- Every `ref` of the program is of a local `borrowed` lists, or through a dereference. -/
def checkBorrows (borrowed : Body → Local → Bool) (prog : Program) : Bool :=
  fnsFineB borrowed prog.fns

theorem refFine_sound (bw : Local → Bool) (B : FnId → Local → Prop) (fn : FnId)
    (hB : ∀ (l : Local), bw l = true → B fn l) :
    ∀ (rv : Rvalue), refFine bw rv = true → RvalueFine B fn rv
  | Rvalue.ref _ p, h =>
    (or_true_cases h).elim (fun a => Or.inl (hB p.base a)) (fun d => Or.inr d)
  | Rvalue.use _, _ => True.intro
  | Rvalue.binary _ _ _, _ => True.intro
  | Rvalue.checked _ _ _, _ => True.intro
  | Rvalue.withOverflow _ _ _, _ => True.intro
  | Rvalue.unary _ _, _ => True.intro
  | Rvalue.cast _ _, _ => True.intro
  | Rvalue.aggregate _ _, _ => True.intro
  | Rvalue.replicate _ _, _ => True.intro
  | Rvalue.discriminant _ _, _ => True.intro
  | Rvalue.len _, _ => True.intro

theorem stmtFineB_sound (bw : Local → Bool) (B : FnId → Local → Prop) (fn : FnId)
    (hB : ∀ (l : Local), bw l = true → B fn l) :
    ∀ (s : Statement), stmtFineB bw s = true → StmtFine B fn s
  | Statement.assign _ rv, h => refFine_sound bw B fn hB rv h
  | Statement.storageLive _, _ => True.intro
  | Statement.storageDead _, _ => True.intro
  | Statement.nop, _ => True.intro

theorem stmtsFineB_mem (bw : Local → Bool) :
    ∀ (stmts : List Statement), stmtsFineB bw stmts = true →
      ∀ (s : Statement), s ∈ stmts → stmtFineB bw s = true
  | [], _, _, m => mem_nil_elim m
  | _ :: rest, h, s, m =>
    (List.mem_cons.mp m).elim
      (fun e => by
        rw [e]
        exact and_true_left h)
      (stmtsFineB_mem bw rest (and_true_right h) s)

theorem blocksFineB_nth (bw : Local → Bool) :
    ∀ (blocks : List BasicBlock) (b : Nat) (blk : BasicBlock), blocksFineB bw blocks = true →
      nth? blocks b = some blk → stmtsFineB bw blk.stmts = true
  | [], _, _, _, h => none_eq_some_elim h
  | x :: _, 0, blk, hc, h => by
    have e : x = blk := Option.some.inj h
    rw [← e]
    exact and_true_left hc
  | _ :: rest, b + 1, blk, hc, h => blocksFineB_nth bw rest b blk (and_true_right hc) h

theorem fnsFineB_nth (borrowed : Body → Local → Bool) :
    ∀ (fns : List Body) (fn : Nat) (body : Body), fnsFineB borrowed fns = true →
      nth? fns fn = some body → blocksFineB (borrowed body) body.blocks = true
  | [], _, _, _, h => none_eq_some_elim h
  | x :: _, 0, body, hc, h => by
    have e : x = body := Option.some.inj h
    rw [← e]
    exact and_true_left hc
  | _ :: rest, fn + 1, body, hc, h => fnsFineB_nth borrowed rest fn body (and_true_right hc) h

/-- The check is sound. -/
theorem checkBorrows_sound (borrowed : Body → Local → Bool) (prog : Program)
    (h : checkBorrows borrowed prog = true) : ProgFine (BorrowedIn borrowed prog) prog := by
  intro fn b blk hblk s hs
  cases hbody : nth? prog.fns fn with
  | none =>
    have e : prog.block fn b = none := congrTo (blockOf b) hbody
    exact none_eq_some_elim (e.symm.trans hblk)
  | some body =>
    have e : prog.block fn b = nth? body.blocks b := congrTo (blockOf b) hbody
    have hn : nth? body.blocks b = some blk := e.symm.trans hblk
    have hb : stmtsFineB (borrowed body) blk.stmts = true :=
      blocksFineB_nth (borrowed body) body.blocks b blk
        (fnsFineB_nth borrowed prog.fns fn body h hbody) hn
    have hsB : stmtFineB (borrowed body) s = true :=
      stmtsFineB_mem (borrowed body) blk.stmts hb s hs
    have hB : ∀ (l : Local), borrowed body l = true → BorrowedIn borrowed prog fn l := by
      intro l hl
      show borrowedOf borrowed (nth? prog.fns fn) l = true
      rw [hbody]
      exact hl
    exact stmtFineB_sound (borrowed body) (BorrowedIn borrowed prog) fn hB s hsB

/-- The locals a body takes a reference to: the base of each `ref`'s place. -/
def refBase (l : Local) : Statement → Bool
  | Statement.assign _ (Rvalue.ref _ p) => decide (p.base = l)
  | _ => false

def refsOf (body : Body) (l : Local) : Bool :=
  body.blocks.any (fun blk => blk.stmts.any (refBase l))

end Blaze.Mir.Opt
