import Blaze.Mir.Opt.Rename

/-!
Where control can be: the invariant a pass that removes blocks is proved under.

`L fn b` says block `b` of function `fn` is live (kept by the pass). `Reaches L st`, the
reachability invariant: the running frame's block is live, and so is every block a frame's
caller goes on at when the frame returns. It holds of every state a run reaches when it holds
of the first (`step_reaches`), provided `L` is closed: block 0 of every function is live,
and every target of a live block's terminator is live (`Closed`).

`Renumbers.sim_live`: under that invariant a renumbering that keeps only the live blocks is a
lockstep simulation.
-/

namespace Blaze.Mir

/-- A return target, when there is one, is live. -/
def OptLive (P : BlockId → Prop) : Option BlockId → Prop
  | none => True
  | some t => P t

/-- Each frame's return target is live in its caller, the frame under it. -/
def CtlLive (L : FnId → BlockId → Prop) : List Ctl → Prop
  | [] => True
  | c :: rest => OptLive (L (headFn rest)) c.2.2 ∧ CtlLive L rest

/-- The reachability invariant: control is at a live block, and returns to live blocks. -/
def Reaches (L : FnId → BlockId → Prop) (st : State) : Prop :=
  TopLive L st.ctl ∧ CtlLive L st.ctl

/-- What a step gives is a state the invariant holds of, or the run's answer. -/
def NextReaches (L : FnId → BlockId → Prop) : Next → Prop
  | Next.state st => Reaches L st
  | Next.done _ => True

theorem Reaches.of_ctl {L : FnId → BlockId → Prop} {st st' : State} (e : st'.ctl = st.ctl)
    (h : Reaches L st) : Reaches L st' := by
  show TopLive L st'.ctl ∧ CtlLive L st'.ctl
  rw [e]
  exact h

theorem CtlLive.of_ctl {L : FnId → BlockId → Prop} {st st' : State} (e : st'.ctl = st.ctl)
    (h : CtlLive L st.ctl) : CtlLive L st'.ctl := by
  rw [e]
  exact h

/-! Terminators. -/

def ArmsLive (P : BlockId → Prop) : List (Int × BlockId) → Prop
  | [] => True
  | arm :: rest => P arm.2 ∧ ArmsLive P rest

/-- Every target of the terminator is live. -/
def TermLive (P : BlockId → Prop) : Terminator → Prop
  | Terminator.goto t => P t
  | Terminator.switchInt _ arms otherwise => ArmsLive P arms ∧ P otherwise
  | Terminator.call _ _ _ target => OptLive P target
  | Terminator.ret => True
  | Terminator.assert _ _ _ t => P t
  | Terminator.drop _ t => P t
  | Terminator.unreachable => True

theorem switchTarget_live (P : BlockId → Prop) (key : Int) (otherwise : BlockId)
    (ho : P otherwise) :
    ∀ (arms : List (Int × BlockId)), ArmsLive P arms → P (switchTarget key arms otherwise) := by
  intro arms
  induction arms with
  | nil =>
    intro _
    exact ho
  | cons arm rest ih =>
    intro ha
    have here : P arm.2 := And.left ha
    have there : P (switchTarget key rest otherwise) := ih (And.right ha)
    by_cases h : key = arm.1
    · rw [switchTarget_cons, ite_eq_left h]
      exact here
    · rw [switchTarget_cons, ite_eq_right h]
      exact there

theorem State.jump_reaches (L : FnId → BlockId → Prop) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (t : BlockId) (hc : CtlLive L st.ctl) (ht : L fn t) :
    Outcome.All (NextReaches L) (st.jump t) := by
  subst hfn
  cases st with
  | mk base stack next =>
    cases stack with
    | nil => exact Outcome.all_undefined _ _
    | cons fr rest => exact Outcome.all_ok (And.intro ht hc)

theorem State.jump?_reaches (L : FnId → BlockId → Prop) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (hc : CtlLive L st.ctl) :
    ∀ (target : Option BlockId), OptLive (L fn) target →
      Outcome.All (NextReaches L) (st.jump? target)
  | some t, ht => State.jump_reaches L st fn hfn t hc ht
  | none, _ => Outcome.all_undefined _ _

theorem switchOn_reaches (L : FnId → BlockId → Prop) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (hc : CtlLive L st.ctl) (arms : List (Int × BlockId))
    (otherwise : BlockId) (ha : ArmsLive (L fn) arms) (ho : L fn otherwise) :
    ∀ (key : Option Int), Outcome.All (NextReaches L) (switchOn st arms otherwise key)
  | some key =>
    State.jump_reaches L st fn hfn (switchTarget key arms otherwise) hc
      (switchTarget_live (L fn) key otherwise ho arms ha)
  | none => Outcome.all_undefined _ _

theorem assertOn_reaches (L : FnId → BlockId → Prop) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (hc : CtlLive L st.ctl) (expected : Bool) (kind : Error)
    (target : BlockId) (ht : L fn target) :
    ∀ (v : Value), Outcome.All (NextReaches L) (assertOn st expected kind target v)
  | Value.bool b => by
    by_cases h : b = expected
    · rw [assertOn_bool, ite_eq_left h]
      exact State.jump_reaches L st fn hfn target hc ht
    · rw [assertOn_bool, ite_eq_right h]
      exact Outcome.all_panic _ _
  | Value.unit => Outcome.all_undefined _ _
  | Value.int _ _ => Outcome.all_undefined _ _
  | Value.adt _ _ => Outcome.all_undefined _ _
  | Value.array _ => Outcome.all_undefined _ _
  | Value.ref _ => Outcome.all_undefined _ _
  | Value.dead => Outcome.all_undefined _ _

/-- A call enters at block 0, which is live, and will return to a live block. -/
theorem State.enter_reaches (L : FnId → BlockId → Prop) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (hc : CtlLive L st.ctl) (id : FnId) (sig : FnSig)
    (args : List Value) (dest : Pointer) (target : Option BlockId) (h0 : L id 0)
    (ht : OptLive (L fn) target) :
    Outcome.All (NextReaches L) (st.enter id sig args dest target) := by
  subst hfn
  by_cases hl : args.length = sig.args
  · rw [State.enter_eq, ite_eq_left hl]
    exact Outcome.all_ok (And.intro h0 (And.intro ht hc))
  · rw [State.enter_eq, ite_eq_right hl]
    exact Outcome.all_undefined _ _

theorem State.ret_reaches (L : FnId → BlockId → Prop) (st : State) (h : Reaches L st) :
    Outcome.All (NextReaches L) st.ret := by
  cases st with
  | mk base stack next =>
    cases stack with
    | nil => exact Outcome.all_undefined _ _
    | cons fr rest =>
      cases fr with
      | mk fid ffn fblock locals dest target =>
        cases locals with
        | nil => exact Outcome.all_undefined _ _
        | cons slot locals =>
          cases rest with
          | nil =>
            exact Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok True.intro)
          | cons caller rest =>
            have below : OptLive (L caller.fn) target ∧
                CtlLive L (State.ctl { base := base, stack := caller :: rest, next := next }) :=
              And.right h
            exact Outcome.all_bind (Outcome.all_true _) (fun v _ =>
              Outcome.all_bind
                (State.write_ctl { base := base, stack := caller :: rest, next := next } dest v)
                (fun st1 h1 =>
                  State.jump?_reaches L st1 caller.fn (State.topFn_of_ctl h1)
                    (CtlLive.of_ctl h1 (And.right below)) target (And.left below)))

theorem callOn_reaches (L : FnId → BlockId → Prop) (sig : FnId → Option FnSig)
    (h0 : ∀ (id : FnId) (s : FnSig), sig id = some s → L id 0) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (hc : CtlLive L st.ctl) (args : List Value) (dest : Pointer)
    (target : Option BlockId) (ht : OptLive (L fn) target) :
    ∀ (callee : Callee), Outcome.All (NextReaches L) (callOn sig st callee args dest target)
  | Callee.fn id => by
    simp only [callOn]
    cases hs : sig id with
    | none => exact Outcome.all_undefined _ _
    | some s => exact State.enter_reaches L st fn hfn hc id s args dest target (h0 id s hs) ht
  | Callee.intrinsic _ =>
    Outcome.all_bind (Outcome.all_true _) (fun v _ =>
      Outcome.all_bind (State.write_ctl st dest v) (fun st1 h1 =>
        State.jump?_reaches L st1 fn ((State.topFn_of_ctl h1).trans hfn) (CtlLive.of_ctl h1 hc)
          target ht))

theorem execTerm_reaches (L : FnId → BlockId → Prop) (sig : FnId → Option FnSig)
    (h0 : ∀ (id : FnId) (s : FnSig), sig id = some s → L id 0) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (h : Reaches L st) :
    ∀ (term : Terminator), TermLive (L fn) term →
      Outcome.All (NextReaches L) (execTerm sig st term)
  | Terminator.goto t, ht => State.jump_reaches L st fn hfn t (And.right h) ht
  | Terminator.switchInt discr arms otherwise, ht =>
    Outcome.all_bind (evalOperand_ctl st discr) (fun r hr =>
      switchOn_reaches L r.2 fn ((State.topFn_of_ctl hr).trans hfn)
        (CtlLive.of_ctl hr (And.right h)) arms otherwise (And.left ht) (And.right ht)
        (switchKey r.1))
  | Terminator.call callee args _ target, ht =>
    Outcome.all_bind (evalOperands_ctl args st) (fun r hr =>
      Outcome.all_bind (Outcome.all_true _) (fun ptr _ =>
        callOn_reaches L sig h0 r.2 fn ((State.topFn_of_ctl hr).trans hfn)
          (CtlLive.of_ctl hr (And.right h)) r.1 ptr target ht callee))
  | Terminator.ret, _ => State.ret_reaches L st h
  | Terminator.assert cond expected kind target, ht =>
    Outcome.all_bind (evalOperand_ctl st cond) (fun r hr =>
      assertOn_reaches L r.2 fn ((State.topFn_of_ctl hr).trans hfn)
        (CtlLive.of_ctl hr (And.right h)) expected kind target ht r.1)
  | Terminator.drop _ t, ht => State.jump_reaches L st fn hfn t (And.right h) ht
  | Terminator.unreachable, _ => Outcome.all_undefined _ _

theorem execBlockFrom_reaches (L : FnId → BlockId → Prop) (adts : List AdtDecl)
    (sig : FnId → Option FnSig)
    (h0 : ∀ (id : FnId) (s : FnSig), sig id = some s → L id 0) (fn : FnId)
    (term : Terminator) (ht : TermLive (L fn) term) :
    ∀ (stmts : List Statement) (st : State), st.topFn = fn → Reaches L st →
      Outcome.All (NextReaches L) (execBlockFrom adts sig st stmts term)
  | [], st, hfn, h => execTerm_reaches L sig h0 st fn hfn h term ht
  | s :: rest, st, hfn, h =>
    Outcome.all_bind (execStmt_ctl adts st s) (fun st1 h1 =>
      execBlockFrom_reaches L adts sig h0 fn term ht rest st1
        ((State.topFn_of_ctl h1).trans hfn) (Reaches.of_ctl h1 h))

/-- `L` is closed in `prog`: block 0 of every function is live, and every target of a live
block's terminator is live. -/
structure Closed (prog : Program) (L : FnId → BlockId → Prop) : Prop where
  entry : ∀ (id : FnId) (s : FnSig), prog.sig id = some s → L id 0
  targets : ∀ (fn : FnId) (b : BlockId) (blk : BasicBlock), L fn b →
    prog.block fn b = some blk → TermLive (L fn) blk.terminator

/-- The invariant is kept by a step. -/
theorem step_reaches {prog : Program} {L : FnId → BlockId → Prop} (hc : Closed prog L) :
    ∀ (st : State), Reaches L st → Outcome.All (NextReaches L) (step prog st) := by
  intro st h
  cases st with
  | mk base stack next =>
    cases stack with
    | nil => exact Outcome.all_undefined _ _
    | cons fr rest =>
      have htop : L fr.fn fr.block := And.left h
      show Outcome.All (NextReaches L)
        (execBlock? prog.adts prog.sig { base := base, stack := fr :: rest, next := next }
          (prog.block fr.fn fr.block))
      cases hb : prog.block fr.fn fr.block with
      | none => exact Outcome.all_undefined _ _
      | some blk =>
        exact execBlockFrom_reaches L prog.adts prog.sig hc.entry fr.fn blk.terminator
          (hc.targets fr.fn fr.block blk htop hb) blk.stmts
          { base := base, stack := fr :: rest, next := next } rfl h

/-- A renumbering that keeps the live blocks of a closed set is a lockstep simulation on the
states the reachability invariant holds of. -/
theorem Renumbers.sim_live {src tgt : Program} {ρ : FnId → BlockId → BlockId}
    {L : FnId → BlockId → Prop} (h : Renumbers src tgt ρ L) (hc : Closed src L) :
    Sim src tgt (State.rename ρ) (Reaches L) where
  commutes := fun st hr =>
    (step_rename h st (And.left hr)).trans (mapNext_eq (State.rename ρ) (step src st)).symm
  preserved := fun st st' hr hs => step_reaches hc st hr (Next.state st') hs

end Blaze.Mir
