import Blaze.Mir.Lemmas
import Blaze.Mir.Opt.Local
import Blaze.Mir.Opt.Rename

/-!
A `switchInt` whose arms all go to one block is a `goto` to it: `simplify_branch` of
crates/hir-ty-mir/src/mir/optimize/cfg.rs.

This is a refinement and not an equality. The `goto` no longer evaluates the discriminant,
and in this semantics that evaluation can be undefined (a local that is uninitialized or
moved out of, a value that is no integer): there the program has undefined behaviour and the
rewritten program goes on. So `switchToGoto_refines` says `Outcome.Refines`: the rewritten
program's run is the program's, unless the program's is undefined.

The specification covers less than the Rust pass: the discriminant must be a `copy` of a
place with no index. A `move` changes the state, and an index can panic, which a `goto` would
drop; the Rust pass rewrites those too, where rustc's MIR has no such effects.

What is general here, for every pass that is equal to its source but where the source is
undefined (dead-store elimination is one): `run_refines` (a step-wise refinement is a
refinement of runs), `run_mapBlocks_refines`, and `Outcome.Calm` (an outcome that is a value
or undefined, never a panic), proved of reads and of places without an index.
-/

namespace Blaze.Mir

/-! Refinement, step by step. -/

theorem Outcome.Refines.bind {α β : Type} {x : Outcome α} {f g : α → Outcome β}
    (h : ∀ (a : α), x = Outcome.ok a → (f a).Refines (g a)) :
    (x.bind f).Refines (x.bind g) := by
  cases x with
  | ok a => exact h a rfl
  | panic e => exact Outcome.Refines.of_eq rfl
  | undefined why => exact Outcome.Refines.of_eq rfl
  | diverge => exact Outcome.Refines.of_eq rfl

/-- When every step of the target refines the step of the source, every run does. -/
theorem run_refines (src tgt : Program)
    (h : ∀ (st : State), (step tgt st).Refines (step src st)) :
    ∀ (fuel : Nat) (st : State), (run tgt fuel st).Refines (run src fuel st) := by
  intro fuel
  induction fuel with
  | zero =>
    intro st
    exact Outcome.Refines.of_eq rfl
  | succ n ih =>
    intro st
    have hs : (∃ why, step src st = Outcome.undefined why) ∨ step tgt st = step src st := h st
    rw [run_succ, run_succ]
    cases hs with
    | inl hu =>
      have ⟨why, e⟩ := hu
      rw [e]
      exact Or.inl ⟨why, rfl⟩
    | inr e =>
      rw [e]
      cases step src st with
      | ok next =>
        cases next with
        | state st' => exact ih st'
        | done v => exact Outcome.Refines.of_eq rfl
      | panic e => exact Outcome.Refines.of_eq rfl
      | undefined why => exact Outcome.Refines.of_eq rfl
      | diverge => exact Outcome.Refines.of_eq rfl

/-- A rewrite of blocks under which each block refines what it was. -/
def BlockRefining (f : BasicBlock → BasicBlock) : Prop :=
  ∀ (adts : List AdtDecl) (sig : FnId → Option FnSig) (st : State) (blk : BasicBlock),
    (execBlock adts sig st (f blk)).Refines (execBlock adts sig st blk)

theorem stepFrame_mapBlocks_refines (f : BasicBlock → BasicBlock) (hf : BlockRefining f)
    (prog : Program) (st : State) (fr : Frame) :
    (stepFrame (prog.mapBlocks f) st fr).Refines (stepFrame prog st fr) := by
  unfold stepFrame
  rw [Program.block_mapBlocks, Program.sig_mapBlocks]
  cases prog.block fr.fn fr.block with
  | none => exact Outcome.Refines.of_eq rfl
  | some blk => exact hf prog.adts prog.sig st blk

theorem step_mapBlocks_refines (f : BasicBlock → BasicBlock) (hf : BlockRefining f)
    (prog : Program) (st : State) : (step (prog.mapBlocks f) st).Refines (step prog st) := by
  unfold step
  cases st.stack with
  | nil => exact Outcome.Refines.of_eq rfl
  | cons fr rest => exact stepFrame_mapBlocks_refines f hf prog st fr

/-- A block-refining rewrite refines every run, with the same fuel. -/
theorem run_mapBlocks_refines (f : BasicBlock → BasicBlock) (hf : BlockRefining f)
    (prog : Program) (fuel : Nat) (st : State) :
    (run (prog.mapBlocks f) fuel st).Refines (run prog fuel st) :=
  run_refines prog (prog.mapBlocks f) (step_mapBlocks_refines f hf prog) fuel st

/-- A rewrite of terminators alone refines a block when it refines each terminator. -/
theorem execBlockFrom_refines_term (g : Terminator → Terminator)
    (hg : ∀ (sig : FnId → Option FnSig) (st : State) (t : Terminator),
      (execTerm sig st (g t)).Refines (execTerm sig st t))
    (adts : List AdtDecl) (sig : FnId → Option FnSig) (t : Terminator) :
    ∀ (stmts : List Statement) (st : State),
      (execBlockFrom adts sig st stmts (g t)).Refines (execBlockFrom adts sig st stmts t)
  | [], st => hg sig st t
  | _ :: rest, _ =>
    Outcome.Refines.bind (fun st1 _ => execBlockFrom_refines_term g hg adts sig t rest st1)

/-! Calm outcomes: a value or undefined behaviour, never a panic and never out of fuel. -/

def Outcome.Calm {α : Type} (x : Outcome α) : Prop :=
  (∃ a, x = Outcome.ok a) ∨ ∃ why, x = Outcome.undefined why

theorem Outcome.calm_ok {α : Type} (a : α) : (Outcome.ok a).Calm := Or.inl ⟨a, rfl⟩

theorem Outcome.calm_undefined {α : Type} (why : Undefined) :
    (Outcome.undefined why : Outcome α).Calm := Or.inr ⟨why, rfl⟩

theorem Outcome.Calm.bind {α β : Type} {x : Outcome α} {f : α → Outcome β} (hx : x.Calm)
    (hf : ∀ (a : α), (f a).Calm) : (x.bind f).Calm := by
  cases x with
  | ok a => exact hf a
  | panic e => exact Or.elim hx (fun ⟨_, h⟩ => nomatch h) (fun ⟨_, h⟩ => nomatch h)
  | undefined why => exact Outcome.calm_undefined why
  | diverge => exact Or.elim hx (fun ⟨_, h⟩ => nomatch h) (fun ⟨_, h⟩ => nomatch h)

theorem Outcome.calm_ite {α : Type} (c : Prop) [Decidable c] (a : α) (why : Undefined) :
    (if c then Outcome.ok a else Outcome.undefined why : Outcome α).Calm := by
  by_cases h : c
  · rw [if_pos h]
    exact Outcome.calm_ok a
  · rw [if_neg h]
    exact Outcome.calm_undefined why

theorem slotOf_calm : ∀ (o : Option Slot), (slotOf o).Calm
  | none => Outcome.calm_undefined _
  | some slot => Outcome.calm_ok slot

theorem slotIn_calm (slot : Nat) : ∀ (o : Option (List Slot)), (slotIn slot o).Calm
  | none => Outcome.calm_undefined _
  | some slots => slotOf_calm (nth? slots slot)

theorem State.slot_calm (st : State) (p : Pointer) : (st.slot p).Calm :=
  slotIn_calm p.slot (st.slots? p.frame)

theorem Slot.load_calm : ∀ (slot : Slot), slot.load.Calm
  | Slot.init v => Outcome.calm_ok v
  | Slot.uninit => Outcome.calm_undefined _
  | Slot.moved => Outcome.calm_undefined _

theorem found_calm (missing : Undefined) : ∀ (o : Option Value), (found missing o).Calm
  | some v => Outcome.calm_ok v
  | none => Outcome.calm_undefined _

theorem Value.child_calm (v : Value) (e : PathElem) : (v.child e).Calm := by
  cases v <;> cases e <;>
    first
    | exact Outcome.calm_undefined _
    | exact found_calm _ _
    | exact Outcome.calm_ite _ _ _

theorem Value.project_calm : ∀ (path : List PathElem) (v : Value), (v.project path).Calm
  | [], v => Outcome.calm_ok v
  | e :: rest, v =>
    Outcome.Calm.bind (Value.child_calm v e) (fun inner => Value.project_calm rest inner)

theorem Value.checkLive_calm (v : Value) : v.checkLive.Calm := by
  cases v <;> first | exact Outcome.calm_ok _ | exact Outcome.calm_undefined _

/-- A read is a value or undefined behaviour. -/
theorem State.read_calm (st : State) (p : Pointer) : (st.read p).Calm :=
  Outcome.Calm.bind (State.slot_calm st p) (fun slot =>
    Outcome.Calm.bind (Slot.load_calm slot) (fun v =>
      Outcome.Calm.bind (Value.project_calm p.path v) (fun v' => Value.checkLive_calm v')))

theorem asRef_calm (v : Value) : (asRef v).Calm := by
  cases v <;> first | exact Outcome.calm_ok _ | exact Outcome.calm_undefined _

/-- A projection with no index: only an index panics. -/
def plainProj : List ProjElem → Bool
  | [] => true
  | ProjElem.deref :: rest => plainProj rest
  | ProjElem.field _ :: rest => plainProj rest
  | ProjElem.downcast _ :: rest => plainProj rest
  | ProjElem.index _ :: _ => false
  | ProjElem.constIndex _ :: _ => false

theorem evalProj_calm (st : State) :
    ∀ (proj : List ProjElem) (p : Pointer), plainProj proj = true → (evalProj st p proj).Calm
  | [], p, _ => Outcome.calm_ok p
  | ProjElem.deref :: rest, p, h =>
    Outcome.Calm.bind (Outcome.Calm.bind (State.read_calm st p) asRef_calm)
      (fun q => evalProj_calm st rest q h)
  | ProjElem.field _ :: rest, _, h =>
    Outcome.Calm.bind (Outcome.calm_ok _) (fun q => evalProj_calm st rest q h)
  | ProjElem.downcast _ :: rest, _, h =>
    Outcome.Calm.bind (Outcome.calm_ok _) (fun q => evalProj_calm st rest q h)
  | ProjElem.index _ :: _, _, h => false_eq_true_elim h
  | ProjElem.constIndex _ :: _, _, h => false_eq_true_elim h

theorem evalPlace_calm (st : State) (place : Place) (h : plainProj place.proj = true) :
    (evalPlace st place).Calm :=
  evalProj_calm st place.proj { frame := st.top, slot := place.base, path := [] } h

/-- A copy of a place with no index is its value, with the state as it was, or undefined. -/
theorem evalCopy_calm (st : State) (p : Place) (h : plainProj p.proj = true) :
    (∃ v, evalOperand st (Operand.copy p) = Outcome.ok (v, st)) ∨
      ∃ why, evalOperand st (Operand.copy p) = Outcome.undefined why := by
  have hp : (∃ ptr, evalPlace st p = Outcome.ok ptr) ∨
      ∃ why, evalPlace st p = Outcome.undefined why := evalPlace_calm st p h
  cases hp with
  | inr hu =>
    have ⟨why, e⟩ := hu
    exact Or.inr ⟨why,
      congrTo
        (fun x => Outcome.bind x (fun ptr => (st.read ptr).bind (fun v => Outcome.ok (v, st))))
        e⟩
  | inl ho =>
    have ⟨ptr, e⟩ := ho
    have hr : (∃ v, st.read ptr = Outcome.ok v) ∨ ∃ why, st.read ptr = Outcome.undefined why :=
      State.read_calm st ptr
    cases hr with
    | inl hv =>
      have ⟨v, ev⟩ := hv
      exact Or.inl ⟨v, evalOperand_copy st p ptr v e ev⟩
    | inr hu =>
      have ⟨why, ev⟩ := hu
      have first : evalOperand st (Operand.copy p) =
          (st.read ptr).bind (fun v => Outcome.ok (v, st)) :=
        congrTo
          (fun x => Outcome.bind x (fun ptr => (st.read ptr).bind (fun v => Outcome.ok (v, st))))
          e
      have second : (st.read ptr).bind (fun v => (Outcome.ok (v, st) : Outcome (Value × State))) =
          Outcome.undefined why :=
        congrTo (fun x => Outcome.bind x (fun v => (Outcome.ok (v, st) : Outcome (Value × State))))
          ev
      exact Or.inr ⟨why, first.trans second⟩

end Blaze.Mir

namespace Blaze.Mir.Opt

open Blaze.Mir

/-- Every arm goes to `o`. -/
def allTo (o : BlockId) : List (Int × BlockId) → Bool
  | [] => true
  | arm :: rest => decide (arm.2 = o) && allTo o rest

theorem switchTarget_allTo (o : BlockId) (key : Int) :
    ∀ (arms : List (Int × BlockId)), allTo o arms = true → switchTarget key arms o = o := by
  intro arms
  induction arms with
  | nil =>
    intro _
    rfl
  | cons arm rest ih =>
    intro h
    have here : arm.2 = o := of_decide_eq_true (and_true_left h)
    by_cases hk : key = arm.1
    · exact ((switchTarget_cons key arm rest o).trans (if_pos hk)).trans here
    · exact ((switchTarget_cons key arm rest o).trans (if_neg hk)).trans (ih (and_true_right h))

/-- The `switchInt` on a copy of `p`, or the `goto` it is when every arm agrees. -/
def switchOrGoto (p : Place) (arms : List (Int × BlockId)) (otherwise : BlockId) : Terminator :=
  if (plainProj p.proj && allTo otherwise arms) = true then Terminator.goto otherwise
  else Terminator.switchInt (Operand.copy p) arms otherwise

def switchToGotoTerm : Terminator → Terminator
  | Terminator.switchInt (Operand.copy p) arms otherwise => switchOrGoto p arms otherwise
  | t => t

def switchToGotoBlock (blk : BasicBlock) : BasicBlock :=
  { stmts := blk.stmts, terminator := switchToGotoTerm blk.terminator }

/-- The pass. -/
def switchToGoto (prog : Program) : Program := prog.mapBlocks switchToGotoBlock

theorem execTerm_switchOrGoto (sig : FnId → Option FnSig) (st : State) (p : Place)
    (arms : List (Int × BlockId)) (otherwise : BlockId) :
    (execTerm sig st (switchOrGoto p arms otherwise)).Refines
      (execTerm sig st (Terminator.switchInt (Operand.copy p) arms otherwise)) := by
  unfold switchOrGoto
  by_cases hc : (plainProj p.proj && allTo otherwise arms) = true
  · rw [if_pos hc]
    have hev : (∃ v, evalOperand st (Operand.copy p) = Outcome.ok (v, st)) ∨
        ∃ why, evalOperand st (Operand.copy p) = Outcome.undefined why :=
      evalCopy_calm st p (and_true_left hc)
    cases hev with
    | inr hu =>
      have ⟨why, e⟩ := hu
      have src : execTerm sig st (Terminator.switchInt (Operand.copy p) arms otherwise) =
          Outcome.undefined why :=
        congrTo (fun x => Outcome.bind x (fun r => switchOn r.2 arms otherwise (switchKey r.1))) e
      exact Or.inl ⟨why, src⟩
    | inl hv =>
      have ⟨v, e⟩ := hv
      have src : execTerm sig st (Terminator.switchInt (Operand.copy p) arms otherwise) =
          switchOn st arms otherwise (switchKey v) :=
        congrTo (fun x => Outcome.bind x (fun r => switchOn r.2 arms otherwise (switchKey r.1))) e
      cases hk : switchKey v with
      | none =>
        have ill : switchOn st arms otherwise (switchKey v) =
            Outcome.undefined Undefined.illTyped :=
          congrTo (switchOn st arms otherwise) hk
        exact Or.inl ⟨Undefined.illTyped, src.trans ill⟩
      | some key =>
        have jump : switchOn st arms otherwise (switchKey v) = st.jump otherwise :=
          (congrTo (switchOn st arms otherwise) hk).trans
            (congrTo (State.jump st) (switchTarget_allTo otherwise key arms (and_true_right hc)))
        have tgt : execTerm sig st (Terminator.goto otherwise) = st.jump otherwise := rfl
        exact Or.inr (tgt.trans (jump.symm.trans src.symm))
  · rw [if_neg hc]
    exact Outcome.Refines.of_eq rfl

theorem execTerm_switchToGotoTerm (sig : FnId → Option FnSig) (st : State) (t : Terminator) :
    (execTerm sig st (switchToGotoTerm t)).Refines (execTerm sig st t) := by
  cases t with
  | goto target => exact Outcome.Refines.of_eq rfl
  | switchInt discr arms otherwise =>
    cases discr with
    | copy p => exact execTerm_switchOrGoto sig st p arms otherwise
    | move p => exact Outcome.Refines.of_eq rfl
    | const c => exact Outcome.Refines.of_eq rfl
  | call callee args dest target => exact Outcome.Refines.of_eq rfl
  | ret => exact Outcome.Refines.of_eq rfl
  | assert cond expected kind target => exact Outcome.Refines.of_eq rfl
  | drop p target => exact Outcome.Refines.of_eq rfl
  | unreachable => exact Outcome.Refines.of_eq rfl

theorem switchToGotoBlock_refining : BlockRefining switchToGotoBlock := by
  intro adts sig st blk
  exact execBlockFrom_refines_term switchToGotoTerm execTerm_switchToGotoTerm adts sig
    blk.terminator blk.stmts st

/-- A `switchInt` whose arms all agree made a `goto` refines the program: for every fuel and
state, the rewritten program's run is the program's, unless the program's is undefined. -/
theorem switchToGoto_refines (prog : Program) (fuel : Nat) (st : State) :
    (run (switchToGoto prog) fuel st).Refines (run prog fuel st) :=
  run_mapBlocks_refines switchToGotoBlock switchToGotoBlock_refining prog fuel st

end Blaze.Mir.Opt
