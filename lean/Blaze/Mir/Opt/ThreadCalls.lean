import Blaze.Mir.Opt.MemMap
import Blaze.Mir.Opt.Thread

/-!
Goto threading through every target, a call's return target too: `collapse_goto_chain` of
crates/hir-ty-mir/src/mir/optimize/cfg.rs, one hop per run.

`threadAll` sends every target of every terminator past an empty block that only jumps on.
A frame's `target` (where its caller goes on) is then another block in the threaded program,
so a state is mapped: `retargetStack` sends each frame's `target` through its caller's map
and leaves every `block`. It is a `MemMap` (`Opt/MemMap.lean`), so statements commute with
it; a terminator from the mapped state gives the mapped next state, or the mapped jump one
hop further (`RedirectsT`), and that hop is the program's second step (`Thread.lean`'s
`jump_then`). So: a stuttering simulation through the map with bound 2
(`threadAll_target`, `threadAll_source`), and the same outcome for every call
(`threadAll_call_source`).
-/

namespace Blaze.Mir.Opt

open Blaze.Mir

/-- A frame with its return target sent through its caller's map. -/
def retargetFrame (τ : FnId → BlockId → BlockId) (caller : FnId) (fr : Frame) : Frame :=
  { id := fr.id, fn := fr.fn, block := fr.block, locals := fr.locals, dest := fr.dest,
    target := mapTarget (τ caller) fr.target }

def retargetStack (τ : FnId → BlockId → BlockId) : List Frame → List Frame
  | [] => []
  | fr :: rest => retargetFrame τ (headFn (rest.map ctlOf)) fr :: retargetStack τ rest

theorem findFrame_retargetStack (τ : FnId → BlockId → BlockId) (id : Nat) :
    ∀ (l : List Frame),
      (findFrame (retargetStack τ l) id).map (fun fr => fr.locals) =
        (findFrame l id).map (fun fr => fr.locals) := by
  intro l
  induction l with
  | nil => rfl
  | cons fr rest ih =>
    by_cases h : fr.id = id
    · have h' : (retargetFrame τ (headFn (rest.map ctlOf)) fr).id = id := h
      have left : findFrame (retargetStack τ (fr :: rest)) id =
          some (retargetFrame τ (headFn (rest.map ctlOf)) fr) :=
        (findFrame_cons (retargetFrame τ (headFn (rest.map ctlOf)) fr) (retargetStack τ rest) id).trans
          (if_pos h')
      have right : findFrame (fr :: rest) id = some fr :=
        (findFrame_cons fr rest id).trans (if_pos h)
      rw [left, right]
      try rfl
    · have h' : ¬ (retargetFrame τ (headFn (rest.map ctlOf)) fr).id = id := h
      have left : findFrame (retargetStack τ (fr :: rest)) id =
          findFrame (retargetStack τ rest) id :=
        (findFrame_cons (retargetFrame τ (headFn (rest.map ctlOf)) fr) (retargetStack τ rest) id).trans
          (if_neg h')
      have right : findFrame (fr :: rest) id = findFrame rest id :=
        (findFrame_cons fr rest id).trans (if_neg h)
      rw [left, right]
      exact ih

theorem setFrame_retargetStack (τ : FnId → BlockId → BlockId) (id : Nat) (slots : List Slot) :
    ∀ (l : List Frame),
      setFrame (retargetStack τ l) id slots = retargetStack τ (setFrame l id slots) := by
  intro l
  induction l with
  | nil => rfl
  | cons fr rest ih =>
    by_cases h : fr.id = id
    · have h' : (retargetFrame τ (headFn (rest.map ctlOf)) fr).id = id := h
      have left : setFrame (retargetStack τ (fr :: rest)) id slots =
          (retargetFrame τ (headFn (rest.map ctlOf)) fr).withLocals slots :: retargetStack τ rest :=
        (setFrame_cons (retargetFrame τ (headFn (rest.map ctlOf)) fr) (retargetStack τ rest) id
          slots).trans (if_pos h')
      have right : setFrame (fr :: rest) id slots = fr.withLocals slots :: rest :=
        (setFrame_cons fr rest id slots).trans (if_pos h)
      exact left.trans (congrTo (retargetStack τ) right).symm
    · have h' : ¬ (retargetFrame τ (headFn (rest.map ctlOf)) fr).id = id := h
      have left : setFrame (retargetStack τ (fr :: rest)) id slots =
          retargetFrame τ (headFn (rest.map ctlOf)) fr :: setFrame (retargetStack τ rest) id slots :=
        (setFrame_cons (retargetFrame τ (headFn (rest.map ctlOf)) fr) (retargetStack τ rest) id
          slots).trans (if_neg h')
      have right : setFrame (fr :: rest) id slots = fr :: setFrame rest id slots :=
        (setFrame_cons fr rest id slots).trans (if_neg h)
      have hd : headFn ((setFrame rest id slots).map ctlOf) = headFn (rest.map ctlOf) :=
        congrTo headFn (setFrame_ctl id slots rest)
      have last : retargetFrame τ (headFn (rest.map ctlOf)) fr :: retargetStack τ (setFrame rest id slots) =
          retargetStack τ (fr :: setFrame rest id slots) :=
        congrTo (fun c => retargetFrame τ c fr :: retargetStack τ (setFrame rest id slots)) hd.symm
      exact left.trans
        ((congrTo (fun t => retargetFrame τ (headFn (rest.map ctlOf)) fr :: t) ih).trans
          (last.trans (congrTo (retargetStack τ) right).symm))

theorem retargetStack_memMap (τ : FnId → BlockId → BlockId) : MemMap (retargetStack τ) where
  locals := fun l id => findFrame_retargetStack τ id l
  set := fun l id slots => setFrame_retargetStack τ id slots l
  top := by
    intro l
    cases l with
    | nil => rfl
    | cons fr rest => rfl

/-- An intrinsic reads memory and nothing else of the state. -/
theorem intrinsicEval_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State)
    (i : Intrinsic) (args : List Value) : i.eval (st.mapStack M) args = i.eval st args := by
  unfold Intrinsic.eval
  rw [State.read_mapStack_fn hM st]

/-! A terminator with every target threaded, from the mapped state. -/

/-- `tgt` is the mapped `src`, or `src` is a jump to `t` and `tgt` the mapped jump of the same
state to where `t` is threaded to. -/
def RedirectsT (τ : FnId → BlockId → BlockId) (src tgt : Outcome Next) : Prop :=
  tgt = Outcome.map (Next.map (State.mapStack (retargetStack τ))) src ∨
    ∃ (s : State) (t : BlockId), src = s.jump t ∧
      tgt = Outcome.map (Next.map (State.mapStack (retargetStack τ))) (s.jump (τ s.topFn t))

theorem RedirectsT.cast {τ : FnId → BlockId → BlockId} {a a' b b' : Outcome Next}
    (ea : a' = a) (eb : b' = b) (h : RedirectsT τ a b) : RedirectsT τ a' b' := by
  subst ea
  subst eb
  exact h

theorem RedirectsT.bind_same {α : Type} {τ : FnId → BlockId → BlockId} {x : Outcome α}
    {f g : α → Outcome Next}
    (h : ∀ (a : α), x = Outcome.ok a → RedirectsT τ (f a) (g a)) :
    RedirectsT τ (x.bind f) (x.bind g) := by
  cases x with
  | ok a => exact h a rfl
  | panic e => exact Or.inl rfl
  | undefined why => exact Or.inl rfl
  | diverge => exact Or.inl rfl

theorem RedirectsT.bind {α : Type} {τ : FnId → BlockId → BlockId} {x x' : Outcome α}
    {f g : α → Outcome Next} (hx : x' = x)
    (h : ∀ (a : α), x = Outcome.ok a → RedirectsT τ (f a) (g a)) :
    RedirectsT τ (x.bind f) (x'.bind g) :=
  RedirectsT.cast rfl (congrTo (fun y => Outcome.bind y g) hx) (RedirectsT.bind_same h)

theorem RedirectsT.bind_map {α α' : Type} {τ : FnId → BlockId → BlockId} {s : α → α'}
    {x : Outcome α} {x' : Outcome α'} {f : α → Outcome Next} {g : α' → Outcome Next}
    (hx : x' = Outcome.map s x)
    (h : ∀ (a : α), x = Outcome.ok a → RedirectsT τ (f a) (g (s a))) :
    RedirectsT τ (x.bind f) (x'.bind g) := by
  subst hx
  cases x with
  | ok a => exact h a rfl
  | panic e => exact Or.inl rfl
  | undefined why => exact Or.inl rfl
  | diverge => exact Or.inl rfl

/-- The map leaves blocks, so a jump from the mapped state is the mapped jump. -/
theorem jump_retarget (τ : FnId → BlockId → BlockId) (st : State) (x : BlockId) :
    (st.mapStack (retargetStack τ)).jump x =
      Outcome.map (Next.map (State.mapStack (retargetStack τ))) (st.jump x) := by
  cases st with
  | mk base stack next =>
    cases stack with
    | nil => rfl
    | cons fr rest => rfl

theorem jump_rt (τ : FnId → BlockId → BlockId) (st : State) (fn : FnId) (hfn : st.topFn = fn)
    (t : BlockId) :
    RedirectsT τ (st.jump t) ((st.mapStack (retargetStack τ)).jump (τ fn t)) := by
  subst hfn
  exact Or.inr ⟨st, t, rfl, jump_retarget τ st (τ st.topFn t)⟩

theorem jump?_rt (τ : FnId → BlockId → BlockId) (st : State) (fn : FnId) (hfn : st.topFn = fn) :
    ∀ (target : Option BlockId),
      RedirectsT τ (st.jump? target)
        ((st.mapStack (retargetStack τ)).jump? (mapTarget (τ fn) target))
  | some t => jump_rt τ st fn hfn t
  | none => Or.inl rfl

theorem switchOn_rt (τ : FnId → BlockId → BlockId) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (arms : List (Int × BlockId)) (otherwise : BlockId) :
    ∀ (key : Option Int),
      RedirectsT τ (switchOn st arms otherwise key)
        (switchOn (st.mapStack (retargetStack τ)) (renameArms (τ fn) arms) (τ fn otherwise) key)
  | some key =>
    RedirectsT.cast rfl
      (congrTo (State.jump (st.mapStack (retargetStack τ)))
        (switchTarget_rename (τ fn) key otherwise arms))
      (jump_rt τ st fn hfn (switchTarget key arms otherwise))
  | none => Or.inl rfl

theorem assertOn_rt (τ : FnId → BlockId → BlockId) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (expected : Bool) (kind : Error) (target : BlockId) :
    ∀ (v : Value),
      RedirectsT τ (assertOn st expected kind target v)
        (assertOn (st.mapStack (retargetStack τ)) expected kind (τ fn target) v)
  | Value.bool b => by
    by_cases h : b = expected
    · have left : assertOn st expected kind target (Value.bool b) = st.jump target :=
        (assertOn_bool st expected kind target b).trans (if_pos h)
      have right : assertOn (st.mapStack (retargetStack τ)) expected kind (τ fn target)
            (Value.bool b) =
          (st.mapStack (retargetStack τ)).jump (τ fn target) :=
        (assertOn_bool (st.mapStack (retargetStack τ)) expected kind (τ fn target) b).trans
          (if_pos h)
      exact RedirectsT.cast left right (jump_rt τ st fn hfn target)
    · have left : assertOn st expected kind target (Value.bool b) = Outcome.panic kind :=
        (assertOn_bool st expected kind target b).trans (if_neg h)
      have right : assertOn (st.mapStack (retargetStack τ)) expected kind (τ fn target)
            (Value.bool b) =
          Outcome.panic kind :=
        (assertOn_bool (st.mapStack (retargetStack τ)) expected kind (τ fn target) b).trans
          (if_neg h)
      exact RedirectsT.cast left right (Or.inl rfl)
  | Value.unit => Or.inl rfl
  | Value.int _ _ => Or.inl rfl
  | Value.adt _ _ => Or.inl rfl
  | Value.array _ => Or.inl rfl
  | Value.ref _ => Or.inl rfl
  | Value.dead => Or.inl rfl

/-- A call enters the mapped state: the new frame's return target is the threaded one. -/
theorem enter_retarget (τ : FnId → BlockId → BlockId) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (id : FnId) (sig : FnSig) (args : List Value) (dest : Pointer)
    (target : Option BlockId) :
    (st.mapStack (retargetStack τ)).enter id sig args dest (mapTarget (τ fn) target) =
      Outcome.map (Next.map (State.mapStack (retargetStack τ)))
        (st.enter id sig args dest target) := by
  subst hfn
  by_cases hl : args.length = sig.args
  · have left : (st.mapStack (retargetStack τ)).enter id sig args dest
          (mapTarget (τ st.topFn) target) =
        Outcome.ok (Next.state (enterState (st.mapStack (retargetStack τ)) id sig args dest
          (mapTarget (τ st.topFn) target))) :=
      (State.enter_eq (st.mapStack (retargetStack τ)) id sig args dest
        (mapTarget (τ st.topFn) target)).trans (if_pos hl)
    have right : st.enter id sig args dest target =
        Outcome.ok (Next.state (enterState st id sig args dest target)) :=
      (State.enter_eq st id sig args dest target).trans (if_pos hl)
    exact left.trans
      (congrTo (Outcome.map (Next.map (State.mapStack (retargetStack τ)))) right).symm
  · have left : (st.mapStack (retargetStack τ)).enter id sig args dest
          (mapTarget (τ st.topFn) target) =
        Outcome.undefined Undefined.illFormed :=
      (State.enter_eq (st.mapStack (retargetStack τ)) id sig args dest
        (mapTarget (τ st.topFn) target)).trans (if_neg hl)
    have right : st.enter id sig args dest target = Outcome.undefined Undefined.illFormed :=
      (State.enter_eq st id sig args dest target).trans (if_neg hl)
    exact left.trans
      (congrTo (Outcome.map (Next.map (State.mapStack (retargetStack τ)))) right).symm

theorem ret_rt (τ : FnId → BlockId → BlockId) (st : State) :
    RedirectsT τ st.ret (st.mapStack (retargetStack τ)).ret := by
  cases st with
  | mk base stack next =>
    cases stack with
    | nil => exact Or.inl rfl
    | cons fr rest =>
      cases fr with
      | mk fid ffn fblock locals dest target =>
        cases locals with
        | nil => exact Or.inl rfl
        | cons slot locals =>
          cases rest with
          | nil => exact RedirectsT.bind rfl (fun _ _ => Or.inl rfl)
          | cons caller rest =>
            exact RedirectsT.bind rfl (fun v _ =>
              RedirectsT.bind_map
                (State.write_mapStack (retargetStack_memMap τ)
                  { base := base, stack := caller :: rest, next := next } dest v)
                (fun st1 h1 =>
                  jump?_rt τ st1 caller.fn
                    (State.topFn_of_ctl
                      (State.write_ctl { base := base, stack := caller :: rest, next := next }
                        dest v st1 h1))
                    target))

theorem callOn_rt (τ : FnId → BlockId → BlockId) (sig : FnId → Option FnSig) (st : State)
    (fn : FnId) (hfn : st.topFn = fn) (args : List Value) (dest : Pointer)
    (target : Option BlockId) :
    ∀ (callee : Callee),
      RedirectsT τ (callOn sig st callee args dest target)
        (callOn sig (st.mapStack (retargetStack τ)) callee args dest (mapTarget (τ fn) target))
  | Callee.fn id => by
    simp only [callOn]
    cases hs : sig id with
    | none => exact Or.inl rfl
    | some s => exact Or.inl (enter_retarget τ st fn hfn id s args dest target)
  | Callee.intrinsic i =>
    RedirectsT.bind (intrinsicEval_mapStack (retargetStack_memMap τ) st i args) (fun v _ =>
      RedirectsT.bind_map (State.write_mapStack (retargetStack_memMap τ) st dest v)
        (fun st1 h1 =>
          jump?_rt τ st1 fn ((State.topFn_of_ctl (State.write_ctl st dest v st1 h1)).trans hfn)
            target))

theorem execTerm_rt (τ : FnId → BlockId → BlockId) (sig : FnId → Option FnSig) (st : State)
    (fn : FnId) (hfn : st.topFn = fn) :
    ∀ (term : Terminator),
      RedirectsT τ (execTerm sig st term)
        (execTerm sig (st.mapStack (retargetStack τ)) (term.rename (τ fn)))
  | Terminator.goto t => jump_rt τ st fn hfn t
  | Terminator.switchInt discr arms otherwise =>
    RedirectsT.bind_map (evalOperand_mapStack (retargetStack_memMap τ) st discr) (fun r hr =>
      switchOn_rt τ r.2 fn ((State.topFn_of_ctl (evalOperand_ctl st discr r hr)).trans hfn) arms
        otherwise (switchKey r.1))
  | Terminator.call callee args dest target =>
    RedirectsT.bind_map (evalOperands_mapStack (retargetStack_memMap τ) args st) (fun r hr =>
      RedirectsT.bind (evalPlace_mapStack (retargetStack_memMap τ) r.2 dest) (fun ptr _ =>
        callOn_rt τ sig r.2 fn ((State.topFn_of_ctl (evalOperands_ctl args st r hr)).trans hfn)
          r.1 ptr target callee))
  | Terminator.ret => ret_rt τ st
  | Terminator.assert cond expected kind target =>
    RedirectsT.bind_map (evalOperand_mapStack (retargetStack_memMap τ) st cond) (fun r hr =>
      assertOn_rt τ r.2 fn ((State.topFn_of_ctl (evalOperand_ctl st cond r hr)).trans hfn)
        expected kind target r.1)
  | Terminator.drop _ t => jump_rt τ st fn hfn t
  | Terminator.unreachable => Or.inl rfl

theorem execBlockFrom_rt (τ : FnId → BlockId → BlockId) (adts : List AdtDecl)
    (sig : FnId → Option FnSig) (fn : FnId) (term : Terminator) :
    ∀ (stmts : List Statement) (st : State), st.topFn = fn →
      RedirectsT τ (execBlockFrom adts sig st stmts term)
        (execBlockFrom adts sig (st.mapStack (retargetStack τ)) stmts (term.rename (τ fn)))
  | [], st, hfn => execTerm_rt τ sig st fn hfn term
  | s :: rest, st, hfn =>
    RedirectsT.bind_map (execStmt_mapStack adts (retargetStack_memMap τ) st s) (fun st1 h1 =>
      execBlockFrom_rt τ adts sig fn term rest st1
        ((State.topFn_of_ctl (execStmt_ctl adts st s st1 h1)).trans hfn))

/-! The pass. -/

/-- Where a jump to a block of this body ends up after one hop. -/
def targetIn : Option Body → BlockId → BlockId
  | some body => threadTarget body.blocks
  | none => fun t => t

/-- Each function's map. -/
def threadMap (prog : Program) : FnId → BlockId → BlockId :=
  fun fn => targetIn (nth? prog.fns fn)

def threadAllBody (body : Body) : Body :=
  body.mapBlocks (BasicBlock.rename (threadTarget body.blocks))

/-- The pass: every target of every terminator, a call's return target too, is threaded. -/
def threadAll (prog : Program) : Program :=
  { adts := prog.adts, fns := prog.fns.map threadAllBody }

theorem targetIn_spec :
    ∀ (o : Option Body) (t : BlockId),
      targetIn o t = t ∨
        ∃ blk, blockOf t o = some blk ∧ emptyGoto blk = some (targetIn o t)
  | none, _ => Or.inl rfl
  | some body, t => by
    cases hg : emptyGoto? (nth? body.blocks t) with
    | none =>
      have e : targetIn (some body) t = t := congrTo (orSelf t) hg
      exact Or.inl e
    | some c =>
      have e : targetIn (some body) t = c := congrTo (orSelf t) hg
      have ⟨blk, hb, hg2⟩ := emptyGoto?_some (nth? body.blocks t) c hg
      have hb' : blockOf t (some body) = some blk := hb
      exact Or.inr ⟨blk, hb', hg2.trans (congrTo some e.symm)⟩

theorem sig_threadAll (prog : Program) : (threadAll prog).sig = prog.sig := by
  funext id
  show (nth? (prog.fns.map threadAllBody) id).map _ = (nth? prog.fns id).map _
  rw [nth?_map]
  cases nth? prog.fns id with
  | none => rfl
  | some body => rfl

theorem blockOf_threadAll (b : BlockId) :
    ∀ (o : Option Body),
      blockOf b (o.map threadAllBody) = (blockOf b o).map (BasicBlock.rename (targetIn o))
  | none => rfl
  | some body => nth?_map (BasicBlock.rename (threadTarget body.blocks)) body.blocks b

theorem block_threadAll (prog : Program) (fn : FnId) (b : BlockId) :
    (threadAll prog).block fn b =
      (prog.block fn b).map (BasicBlock.rename (threadMap prog fn)) := by
  show blockOf b (nth? (prog.fns.map threadAllBody) fn) =
    (blockOf b (nth? prog.fns fn)).map (BasicBlock.rename (targetIn (nth? prog.fns fn)))
  rw [nth?_map]
  exact blockOf_threadAll b (nth? prog.fns fn)

/-- One step of the threaded program from the mapped state is the mapped step of the program,
or the mapped second step. -/
theorem threadAll_step (prog : Program) :
    ∀ (st : State),
      step (threadAll prog) (st.mapStack (retargetStack (threadMap prog))) =
          Outcome.map (Next.map (State.mapStack (retargetStack (threadMap prog))))
            (step prog st) ∨
        ∃ s1, step prog st = Outcome.ok (Next.state s1) ∧
          step (threadAll prog) (st.mapStack (retargetStack (threadMap prog))) =
            Outcome.map (Next.map (State.mapStack (retargetStack (threadMap prog))))
              (step prog s1)
  | { base := _, stack := [], next := _ } => Or.inl rfl
  | { base := base, stack := fr :: rest, next := next } => by
    have threaded :
        step (threadAll prog)
            (State.mapStack (retargetStack (threadMap prog))
              { base := base, stack := fr :: rest, next := next }) =
          execBlock? prog.adts prog.sig
            (State.mapStack (retargetStack (threadMap prog))
              { base := base, stack := fr :: rest, next := next })
            ((prog.block fr.fn fr.block).map (BasicBlock.rename (threadMap prog fr.fn))) := by
      show execBlock? prog.adts (threadAll prog).sig
          (State.mapStack (retargetStack (threadMap prog))
            { base := base, stack := fr :: rest, next := next })
          ((threadAll prog).block fr.fn fr.block) = _
      rw [block_threadAll, sig_threadAll]
    have src : step prog { base := base, stack := fr :: rest, next := next } =
        execBlock? prog.adts prog.sig { base := base, stack := fr :: rest, next := next }
          (prog.block fr.fn fr.block) := rfl
    rw [threaded, src]
    cases prog.block fr.fn fr.block with
    | none => exact Or.inl rfl
    | some blk =>
      have r :
          execBlock prog.adts prog.sig
              (State.mapStack (retargetStack (threadMap prog))
                { base := base, stack := fr :: rest, next := next })
              (blk.rename (threadMap prog fr.fn)) =
            Outcome.map (Next.map (State.mapStack (retargetStack (threadMap prog))))
              (execBlock prog.adts prog.sig { base := base, stack := fr :: rest, next := next }
                blk) ∨
          ∃ (s : State) (t : BlockId),
            execBlock prog.adts prog.sig { base := base, stack := fr :: rest, next := next } blk =
              s.jump t ∧
            execBlock prog.adts prog.sig
                (State.mapStack (retargetStack (threadMap prog))
                  { base := base, stack := fr :: rest, next := next })
                (blk.rename (threadMap prog fr.fn)) =
              Outcome.map (Next.map (State.mapStack (retargetStack (threadMap prog))))
                (s.jump (threadMap prog s.topFn t)) :=
        execBlockFrom_rt (threadMap prog) prog.adts prog.sig fr.fn blk.terminator blk.stmts
          { base := base, stack := fr :: rest, next := next } rfl
      cases r with
      | inl same => exact Or.inl same
      | inr moved =>
        have ⟨s, t, hsrc, htgt⟩ := moved
        cases targetIn_spec (nth? prog.fns s.topFn) t with
        | inl e =>
          have e' : threadMap prog s.topFn t = t := e
          exact Or.inl (htgt.trans
            (congrTo
              (fun x => Outcome.map (Next.map (State.mapStack (retargetStack (threadMap prog)))) x)
              ((congrTo (State.jump s) e').trans hsrc.symm)))
        | inr hop =>
          have ⟨blk2, hb, hg⟩ := hop
          have hb' : prog.block s.topFn t = some blk2 := hb
          have hg' : emptyGoto blk2 = some (threadMap prog s.topFn t) := hg
          cases jump_then prog (threadMap prog s.topFn t) t blk2 hg' s hb' with
          | inl same =>
            exact Or.inl (htgt.trans
              (congrTo
                (fun x =>
                  Outcome.map (Next.map (State.mapStack (retargetStack (threadMap prog)))) x)
                (same.symm.trans hsrc.symm)))
          | inr two =>
            have ⟨s1, hj, hstep⟩ := two
            exact Or.inr ⟨s1, hsrc.trans hj,
              htgt.trans
                (congrTo
                  (fun x =>
                    Outcome.map (Next.map (State.mapStack (retargetStack (threadMap prog)))) x)
                  hstep.symm)⟩

/-- Threading every target is a stuttering simulation through the map on return targets,
with bound 2. -/
theorem threadAll_stutter (prog : Program) :
    Stutter prog (threadAll prog) (State.mapStack (retargetStack (threadMap prog)))
      (fun _ => True) 2 where
  commutes := by
    intro st _
    cases threadAll_step prog st with
    | inl same =>
      exact ⟨0, Nat.zero_lt_succ 1,
        (same.trans
          (congrTo
            (fun x => Outcome.map (Next.map (State.mapStack (retargetStack (threadMap prog)))) x)
            (steps_one prog st).symm)).trans
          (mapNext_eq (State.mapStack (retargetStack (threadMap prog)))
            (steps prog (0 + 1) st)).symm,
        fun _ _ => True.intro⟩
    | inr two =>
      have ⟨s1, h1, h2⟩ := two
      exact ⟨1, Nat.lt_succ_self 1,
        (h2.trans
          (congrTo
            (fun x => Outcome.map (Next.map (State.mapStack (retargetStack (threadMap prog)))) x)
            (steps_two prog st s1 h1).symm)).trans
          (mapNext_eq (State.mapStack (retargetStack (threadMap prog)))
            (steps prog (1 + 1) st)).symm,
        fun _ _ => True.intro⟩

/-- A run of the threaded program that ends in `n` steps is the program's run of `2 * n`. -/
theorem threadAll_target (prog : Program) (n : Nat) (st : State)
    (hne : run (threadAll prog) n (st.mapStack (retargetStack (threadMap prog))) ≠
      Outcome.diverge) :
    run prog (2 * n) st =
      run (threadAll prog) n (st.mapStack (retargetStack (threadMap prog))) :=
  (threadAll_stutter prog).run_target n st True.intro hne

/-- A run of the program that ends in `n` steps is the threaded program's run of `n`. -/
theorem threadAll_source (prog : Program) (n : Nat) (st : State)
    (hne : run prog n st ≠ Outcome.diverge) :
    run (threadAll prog) n (st.mapStack (retargetStack (threadMap prog))) = run prog n st :=
  (threadAll_stutter prog).run_source n st True.intro hne

/-- A call of the threaded program starts in the mapped start state. -/
theorem start_threadAll (prog : Program) (id : FnId) (base : List Slot) (args : List Value) :
    (threadAll prog).start id base args =
      Outcome.map (State.mapStack (retargetStack (threadMap prog))) (prog.start id base args) := by
  unfold Program.start
  rw [sig_threadAll]
  cases hs : prog.sig id with
  | none => rfl
  | some s =>
    exact Outcome.bind_commutes_map
      (enter_retarget (threadMap prog) { base := base, stack := [], next := 1 } 0 rfl id s args
        { frame := 0, slot := 0, path := [] } none)
      (fun next _ => by
        cases next with
        | state st => rfl
        | done v => rfl)

/-- A call that ends in `n` steps gives the same outcome in the threaded program. -/
theorem threadAll_call_source (prog : Program) (n : Nat) (id : FnId) (base : List Slot)
    (args : List Value) (hne : prog.call n id base args ≠ Outcome.diverge) :
    (threadAll prog).call n id base args = prog.call n id base args :=
  Outcome.bind_map_congr (start_threadAll prog id base args) (fun st hst =>
    threadAll_source prog n st (fun hd =>
      hne ((congrTo (fun x => Outcome.bind x (run prog n)) hst).trans hd)))

end Blaze.Mir.Opt
