import Blaze.Mir.Opt.Local
import Blaze.Mir.Opt.Rename

/-!
Goto threading: a jump to an empty block that only jumps on goes where that block goes.

`threadGotos` rewrites the targets of `goto`, `switchInt`, `assert` and `drop`: a target `t`
whose block has no statement and ends in `goto c` becomes `c`. One hop: a chain is threaded
by running the pass again. Blocks stay where they are (the ones nothing jumps to any more are
`removeDeadBlocks`' to remove), so a state of the program is a state of the threaded program.

The threaded program takes fewer steps, so the proof is a stuttering simulation
(`Opt/Sim.lean`, `Stutter`) with the identity as its map and 2 as its bound: one step of the
threaded program is one step of the program, or two when the first lands on a threaded
block. The fuel is stated: `threadGotos_target`, `threadGotos_source`, and for calls
`threadGotos_call_target` and `threadGotos_call_source`.

Against `collapse_goto_chain` of crates/hir-ty-mir/src/mir/optimize/cfg.rs, this covers
less: the Rust pass follows a whole chain at once, threads the return target of a call too
(here a frame's `target` is left as it is, so no state is renamed), and is one part of
`SimplifyCfg`, which also merges a block into its only predecessor. So no pair of bodies is
checked against this specification.
-/

namespace Blaze.Mir.Opt

open Blaze.Mir

/-- Where the block goes, when it has no statement and ends in a `goto`. -/
def emptyGoto (blk : BasicBlock) : Option BlockId :=
  match blk.stmts, blk.terminator with
  | [], Terminator.goto c => some c
  | _, _ => none

def emptyGoto? : Option BasicBlock → Option BlockId
  | some blk => emptyGoto blk
  | none => none

def orSelf (t : BlockId) : Option BlockId → BlockId
  | some c => c
  | none => t

/-- Where a jump to `t` ends up after one hop. -/
def threadTarget (blocks : List BasicBlock) (t : BlockId) : BlockId :=
  orSelf t (emptyGoto? (nth? blocks t))

/-- The terminator with its jump targets sent through `τ`. A call's return target is kept. -/
def threadTerm (τ : BlockId → BlockId) : Terminator → Terminator
  | Terminator.goto t => Terminator.goto (τ t)
  | Terminator.switchInt discr arms otherwise =>
    Terminator.switchInt discr (renameArms τ arms) (τ otherwise)
  | Terminator.call callee args dest target => Terminator.call callee args dest target
  | Terminator.ret => Terminator.ret
  | Terminator.assert cond expected kind target => Terminator.assert cond expected kind (τ target)
  | Terminator.drop p target => Terminator.drop p (τ target)
  | Terminator.unreachable => Terminator.unreachable

def threadBlock (τ : BlockId → BlockId) (blk : BasicBlock) : BasicBlock :=
  { stmts := blk.stmts, terminator := threadTerm τ blk.terminator }

def threadBody (body : Body) : Body :=
  body.mapBlocks (threadBlock (threadTarget body.blocks))

/-- The pass. -/
def threadGotos (prog : Program) : Program :=
  { adts := prog.adts, fns := prog.fns.map threadBody }

/-! The threaded program's signatures and blocks. -/

theorem sig_thread (prog : Program) : (threadGotos prog).sig = prog.sig := by
  funext id
  show (nth? (prog.fns.map threadBody) id).map _ = (nth? prog.fns id).map _
  rw [nth?_map]
  cases nth? prog.fns id with
  | none => rfl
  | some body => rfl

/-- Block `b` of the threaded body. -/
def threadedBlockOf (b : BlockId) : Option Body → Option BasicBlock
  | some body => (nth? body.blocks b).map (threadBlock (threadTarget body.blocks))
  | none => none

theorem block_thread (prog : Program) (fn : FnId) (b : BlockId) :
    (threadGotos prog).block fn b = threadedBlockOf b (nth? prog.fns fn) := by
  show blockOf b (nth? (prog.fns.map threadBody) fn) = threadedBlockOf b (nth? prog.fns fn)
  rw [nth?_map]
  cases nth? prog.fns fn with
  | none => rfl
  | some body => exact nth?_map (threadBlock (threadTarget body.blocks)) body.blocks b

/-- An empty block that ends in `goto c` is a jump to `c`. -/
theorem execBlock_emptyGoto (adts : List AdtDecl) (sig : FnId → Option FnSig) (st : State)
    (c : BlockId) :
    ∀ (blk : BasicBlock), emptyGoto blk = some c → execBlock adts sig st blk = st.jump c := by
  intro blk h
  cases blk with
  | mk stmts term =>
    cases stmts with
    | cons s rest => cases term <;> exact none_eq_some_elim h
    | nil =>
      cases term with
      | goto t =>
        have e : t = c := Option.some.inj h
        exact congrTo (State.jump st) e
      | switchInt discr arms otherwise => exact none_eq_some_elim h
      | call callee args dest target => exact none_eq_some_elim h
      | ret => exact none_eq_some_elim h
      | assert cond expected kind target => exact none_eq_some_elim h
      | drop p target => exact none_eq_some_elim h
      | unreachable => exact none_eq_some_elim h

theorem emptyGoto?_some :
    ∀ (o : Option BasicBlock) (c : BlockId), emptyGoto? o = some c →
      ∃ blk, o = some blk ∧ emptyGoto blk = some c
  | some blk, _, h => ⟨blk, rfl, h⟩
  | none, _, h => none_eq_some_elim h

/-! A threaded terminator does what the terminator does, or jumps one hop further. -/

/-- `tgt` is `src`, or `src` is a jump of the function `fn` to `t` and `tgt` the jump of the
same state to `τ t`. -/
def Redirects (τ : BlockId → BlockId) (fn : FnId) (src tgt : Outcome Next) : Prop :=
  tgt = src ∨ ∃ (s : State) (t : BlockId), s.topFn = fn ∧ src = s.jump t ∧ tgt = s.jump (τ t)

theorem Redirects.cast {τ : BlockId → BlockId} {fn fn' : FnId} {a b : Outcome Next}
    (e : fn' = fn) (h : Redirects τ fn' a b) : Redirects τ fn a b := by
  subst e
  exact h

theorem Redirects.bind {α : Type} {τ : BlockId → BlockId} {fn : FnId} {x : Outcome α}
    {f g : α → Outcome Next}
    (h : ∀ (a : α), x = Outcome.ok a → Redirects τ fn (f a) (g a)) :
    Redirects τ fn (x.bind f) (x.bind g) := by
  cases x with
  | ok a => exact h a rfl
  | panic e => exact Or.inl rfl
  | undefined why => exact Or.inl rfl
  | diverge => exact Or.inl rfl

theorem switchOn_thread (τ : BlockId → BlockId) (s : State) (arms : List (Int × BlockId))
    (otherwise : BlockId) :
    ∀ (key : Option Int),
      Redirects τ s.topFn (switchOn s arms otherwise key)
        (switchOn s (renameArms τ arms) (τ otherwise) key)
  | some key =>
    Or.inr ⟨s, switchTarget key arms otherwise, rfl, rfl,
      congrTo (State.jump s) (switchTarget_rename τ key otherwise arms)⟩
  | none => Or.inl rfl

theorem assertOn_thread (τ : BlockId → BlockId) (s : State) (expected : Bool) (kind : Error)
    (target : BlockId) :
    ∀ (v : Value),
      Redirects τ s.topFn (assertOn s expected kind target v)
        (assertOn s expected kind (τ target) v)
  | Value.bool b => by
    by_cases h : b = expected
    · have left : assertOn s expected kind target (Value.bool b) = s.jump target :=
        (assertOn_bool s expected kind target b).trans (if_pos h)
      have right : assertOn s expected kind (τ target) (Value.bool b) = s.jump (τ target) :=
        (assertOn_bool s expected kind (τ target) b).trans (if_pos h)
      exact Or.inr ⟨s, target, rfl, left, right⟩
    · have left : assertOn s expected kind target (Value.bool b) = Outcome.panic kind :=
        (assertOn_bool s expected kind target b).trans (if_neg h)
      have right : assertOn s expected kind (τ target) (Value.bool b) = Outcome.panic kind :=
        (assertOn_bool s expected kind (τ target) b).trans (if_neg h)
      exact Or.inl (right.trans left.symm)
  | Value.unit => Or.inl rfl
  | Value.int _ _ => Or.inl rfl
  | Value.adt _ _ => Or.inl rfl
  | Value.array _ => Or.inl rfl
  | Value.ref _ => Or.inl rfl
  | Value.dead => Or.inl rfl

theorem execTerm_thread (sig : FnId → Option FnSig) (τ : BlockId → BlockId) (st : State) :
    ∀ (term : Terminator),
      Redirects τ st.topFn (execTerm sig st term) (execTerm sig st (threadTerm τ term))
  | Terminator.goto t => Or.inr ⟨st, t, rfl, rfl, rfl⟩
  | Terminator.switchInt discr arms otherwise =>
    Redirects.bind (fun r hr =>
      Redirects.cast (State.topFn_of_ctl (evalOperand_ctl st discr r hr))
        (switchOn_thread τ r.2 arms otherwise (switchKey r.1)))
  | Terminator.call _ _ _ _ => Or.inl rfl
  | Terminator.ret => Or.inl rfl
  | Terminator.assert cond expected kind target =>
    Redirects.bind (fun r hr =>
      Redirects.cast (State.topFn_of_ctl (evalOperand_ctl st cond r hr))
        (assertOn_thread τ r.2 expected kind target r.1))
  | Terminator.drop _ t => Or.inr ⟨st, t, rfl, rfl, rfl⟩
  | Terminator.unreachable => Or.inl rfl

theorem execBlockFrom_thread (adts : List AdtDecl) (sig : FnId → Option FnSig)
    (τ : BlockId → BlockId) (term : Terminator) :
    ∀ (stmts : List Statement) (st : State),
      Redirects τ st.topFn (execBlockFrom adts sig st stmts term)
        (execBlockFrom adts sig st stmts (threadTerm τ term))
  | [], st => execTerm_thread sig τ st term
  | s :: rest, st =>
    Redirects.bind (fun st1 h1 =>
      Redirects.cast (State.topFn_of_ctl (execStmt_ctl adts st s st1 h1))
        (execBlockFrom_thread adts sig τ term rest st1))

/-! The second step: a jump to an empty block that ends in `goto c`, then that block. -/

/-- A jump to `t`, then the block `t` when it is an empty `goto c`, is a jump to `c`. -/
theorem jump_then (prog : Program) (c t : BlockId) (blk : BasicBlock)
    (hg : emptyGoto blk = some c) :
    ∀ (s : State), prog.block s.topFn t = some blk →
      s.jump t = s.jump c ∨
        ∃ s1, s.jump t = Outcome.ok (Next.state s1) ∧ step prog s1 = s.jump c
  | { base := _, stack := [], next := _ }, _ => Or.inl rfl
  | { base := base, stack := fr :: rest, next := next }, hb => by
    have hb' : prog.block fr.fn t = some blk := hb
    have second :
        step prog { base := base, stack := fr.withBlock t :: rest, next := next } =
          State.jump { base := base, stack := fr :: rest, next := next } c := by
      show execBlock? prog.adts prog.sig
          { base := base, stack := fr.withBlock t :: rest, next := next }
          (prog.block fr.fn t) =
        State.jump { base := base, stack := fr :: rest, next := next } c
      rw [hb']
      exact execBlock_emptyGoto prog.adts prog.sig
        { base := base, stack := fr.withBlock t :: rest, next := next } c blk hg
    exact Or.inr ⟨{ base := base, stack := fr.withBlock t :: rest, next := next }, rfl, second⟩

/-- A block of the body, threaded, from a state of that body's function: what the block
does, or what the block and then the block it lands on do. -/
theorem thread_exec (prog : Program) (st : State) (fn : FnId) (hfn : st.topFn = fn)
    (body : Body) (hbody : nth? prog.fns fn = some body) (blk : BasicBlock) :
    execBlock prog.adts prog.sig st (threadBlock (threadTarget body.blocks) blk) =
        execBlock prog.adts prog.sig st blk ∨
      ∃ s1, execBlock prog.adts prog.sig st blk = Outcome.ok (Next.state s1) ∧
        execBlock prog.adts prog.sig st (threadBlock (threadTarget body.blocks) blk) =
          step prog s1 := by
  have r : execBlock prog.adts prog.sig st (threadBlock (threadTarget body.blocks) blk) =
        execBlock prog.adts prog.sig st blk ∨
      ∃ (s : State) (t : BlockId), s.topFn = st.topFn ∧
        execBlock prog.adts prog.sig st blk = s.jump t ∧
        execBlock prog.adts prog.sig st (threadBlock (threadTarget body.blocks) blk) =
          s.jump (threadTarget body.blocks t) :=
    execBlockFrom_thread prog.adts prog.sig (threadTarget body.blocks) blk.terminator blk.stmts
      st
  cases r with
  | inl same => exact Or.inl same
  | inr moved =>
    have ⟨s, t, hs, hsrc, htgt⟩ := moved
    cases hg : emptyGoto? (nth? body.blocks t) with
    | none =>
      have e : threadTarget body.blocks t = t := congrTo (orSelf t) hg
      exact Or.inl (htgt.trans ((congrTo (State.jump s) e).trans hsrc.symm))
    | some c =>
      have e : threadTarget body.blocks t = c := congrTo (orSelf t) hg
      have ⟨blk2, hb2, hg2⟩ := emptyGoto?_some (nth? body.blocks t) c hg
      have hblock : prog.block s.topFn t = some blk2 := by
        show blockOf t (nth? prog.fns s.topFn) = some blk2
        rw [hs, hfn, hbody]
        exact hb2
      cases jump_then prog c t blk2 hg2 s hblock with
      | inl same =>
        exact Or.inl
          (htgt.trans ((congrTo (State.jump s) e).trans (same.symm.trans hsrc.symm)))
      | inr two =>
        have ⟨s1, hj, hstep⟩ := two
        exact Or.inr
          ⟨s1, hsrc.trans hj, htgt.trans ((congrTo (State.jump s) e).trans hstep.symm)⟩

theorem thread_block (prog : Program) (st : State) (fn : FnId) (hfn : st.topFn = fn)
    (b : BlockId) :
    ∀ (body? : Option Body), nth? prog.fns fn = body? →
      execBlock? prog.adts prog.sig st (threadedBlockOf b body?) =
          execBlock? prog.adts prog.sig st (blockOf b body?) ∨
        ∃ s1, execBlock? prog.adts prog.sig st (blockOf b body?) = Outcome.ok (Next.state s1) ∧
          execBlock? prog.adts prog.sig st (threadedBlockOf b body?) = step prog s1
  | none, _ => Or.inl rfl
  | some body, hbody => by
    show execBlock? prog.adts prog.sig st
          ((nth? body.blocks b).map (threadBlock (threadTarget body.blocks))) =
        execBlock? prog.adts prog.sig st (nth? body.blocks b) ∨
      ∃ s1, execBlock? prog.adts prog.sig st (nth? body.blocks b) =
          Outcome.ok (Next.state s1) ∧
        execBlock? prog.adts prog.sig st
          ((nth? body.blocks b).map (threadBlock (threadTarget body.blocks))) = step prog s1
    cases nth? body.blocks b with
    | none => exact Or.inl rfl
    | some blk => exact thread_exec prog st fn hfn body hbody blk

/-- One step of the threaded program is one step of the program, or two. -/
theorem thread_step (prog : Program) :
    ∀ (st : State),
      step (threadGotos prog) st = step prog st ∨
        ∃ s1, step prog st = Outcome.ok (Next.state s1) ∧
          step (threadGotos prog) st = step prog s1
  | { base := _, stack := [], next := _ } => Or.inl rfl
  | { base := base, stack := fr :: rest, next := next } => by
    have threaded :
        step (threadGotos prog) { base := base, stack := fr :: rest, next := next } =
          execBlock? prog.adts prog.sig { base := base, stack := fr :: rest, next := next }
            (threadedBlockOf fr.block (nth? prog.fns fr.fn)) := by
      show execBlock? prog.adts (threadGotos prog).sig
          { base := base, stack := fr :: rest, next := next }
          ((threadGotos prog).block fr.fn fr.block) =
        execBlock? prog.adts prog.sig { base := base, stack := fr :: rest, next := next }
          (threadedBlockOf fr.block (nth? prog.fns fr.fn))
      rw [block_thread, sig_thread]
    rw [threaded]
    exact thread_block prog { base := base, stack := fr :: rest, next := next } fr.fn rfl
      fr.block (nth? prog.fns fr.fn) rfl

/-- Threading is a stuttering simulation through the identity, with bound 2. -/
theorem thread_stutter (prog : Program) :
    Stutter prog (threadGotos prog) (fun st => st) (fun _ => True) 2 where
  commutes := by
    intro st _
    cases thread_step prog st with
    | inl same =>
      exact ⟨0, Nat.zero_lt_succ 1,
        (same.trans (steps_one prog st).symm).trans (mapNext_id (steps prog (0 + 1) st)).symm,
        fun _ _ => True.intro⟩
    | inr two =>
      have ⟨s1, h1, h2⟩ := two
      exact ⟨1, Nat.lt_succ_self 1,
        (h2.trans (steps_two prog st s1 h1).symm).trans (mapNext_id (steps prog (1 + 1) st)).symm,
        fun _ _ => True.intro⟩

/-- A run of the threaded program that ends in `n` steps is the program's run of `2 * n`. -/
theorem threadGotos_target (prog : Program) (n : Nat) (st : State)
    (hne : run (threadGotos prog) n st ≠ Outcome.diverge) :
    run prog (2 * n) st = run (threadGotos prog) n st :=
  (thread_stutter prog).run_target n st True.intro hne

/-- A run of the program that ends in `n` steps is the threaded program's run of `n`: the
threaded program never takes more steps. -/
theorem threadGotos_source (prog : Program) (n : Nat) (st : State)
    (hne : run prog n st ≠ Outcome.diverge) :
    run (threadGotos prog) n st = run prog n st :=
  (thread_stutter prog).run_source n st True.intro hne

/-- The same as a refinement up to fuel. -/
theorem threadGotos_refines (prog : Program) (n : Nat) (st : State) :
    run (threadGotos prog) n st = Outcome.diverge ∨
      (run (threadGotos prog) n st).Refines (run prog (2 * n) st) :=
  (thread_stutter prog).refines n st True.intro

/-! Calls. -/

theorem start_thread (prog : Program) (id : FnId) (base : List Slot) (args : List Value) :
    (threadGotos prog).start id base args = prog.start id base args := by
  unfold Program.start
  rw [sig_thread]

/-- A call of the program that ends in `n` steps ends the same in the threaded program. -/
theorem threadGotos_call_source (prog : Program) (n : Nat) (id : FnId) (base : List Slot)
    (args : List Value) (hne : prog.call n id base args ≠ Outcome.diverge) :
    (threadGotos prog).call n id base args = prog.call n id base args :=
  Outcome.bind_congr (start_thread prog id base args) (fun st hst =>
    threadGotos_source prog n st (fun hd =>
      hne ((congrTo (fun x => Outcome.bind x (run prog n)) hst).trans hd)))

/-- A call of the threaded program that ends in `n` steps ends the same in the program with
`2 * n`. -/
theorem threadGotos_call_target (prog : Program) (n : Nat) (id : FnId) (base : List Slot)
    (args : List Value) (hne : (threadGotos prog).call n id base args ≠ Outcome.diverge) :
    prog.call (2 * n) id base args = (threadGotos prog).call n id base args := by
  have h : (threadGotos prog).call n id base args = prog.call (2 * n) id base args :=
    Outcome.bind_congr (start_thread prog id base args) (fun st hst =>
      (threadGotos_target prog n st (fun hd =>
        hne ((congrTo (fun x => Outcome.bind x (run (threadGotos prog) n))
          ((start_thread prog id base args).trans hst)).trans hd))).symm)
  exact h.symm

/-! Whole chains: the pass again, as often as the longest chain is long. Each round at most
doubles the steps the program takes for one of the threaded program's. -/

/-- `threadGotos`, `k` times: a chain of up to `k` empty blocks is threaded through. -/
def threadGotosN : Nat → Program → Program
  | 0, prog => prog
  | k + 1, prog => threadGotos (threadGotosN k prog)

/-- The fuel bound of `k` rounds: `2 ^ k`. -/
def twoPow : Nat → Nat
  | 0 => 1
  | k + 1 => 2 * twoPow k

/-- A run of the program that ends in `n` steps is the run of `n` steps after any number of
rounds. -/
theorem threadGotosN_source (prog : Program) :
    ∀ (k n : Nat) (st : State), run prog n st ≠ Outcome.diverge →
      run (threadGotosN k prog) n st = run prog n st
  | 0, _, _, _ => rfl
  | k + 1, n, st, hne => by
    have ih : run (threadGotosN k prog) n st = run prog n st :=
      threadGotosN_source prog k n st hne
    have hne' : run (threadGotosN k prog) n st ≠ Outcome.diverge :=
      fun hd => hne (ih.symm.trans hd)
    exact (threadGotos_source (threadGotosN k prog) n st hne').trans ih

/-- A run after `k` rounds that ends in `n` steps is the program's run of `2 ^ k * n`. -/
theorem threadGotosN_target (prog : Program) :
    ∀ (k n : Nat) (st : State), run (threadGotosN k prog) n st ≠ Outcome.diverge →
      run prog (twoPow k * n) st = run (threadGotosN k prog) n st
  | 0, n, st, _ => congrTo (fun j => run prog j st) (Nat.one_mul n)
  | k + 1, n, st, hne => by
    have t1 : run (threadGotosN k prog) (2 * n) st = run (threadGotos (threadGotosN k prog)) n st :=
      threadGotos_target (threadGotosN k prog) n st hne
    have hne2 : run (threadGotosN k prog) (2 * n) st ≠ Outcome.diverge :=
      fun hd => hne (t1.symm.trans hd)
    have ih : run prog (twoPow k * (2 * n)) st = run (threadGotosN k prog) (2 * n) st :=
      threadGotosN_target prog k (2 * n) st hne2
    have e : 2 * twoPow k * n = twoPow k * (2 * n) := by
      rw [Nat.mul_comm 2 (twoPow k), Nat.mul_assoc]
    have fuel : run prog (twoPow (k + 1) * n) st = run prog (twoPow k * (2 * n)) st :=
      congrTo (fun j => run prog j st) e
    exact fuel.trans (ih.trans t1)

end Blaze.Mir.Opt
