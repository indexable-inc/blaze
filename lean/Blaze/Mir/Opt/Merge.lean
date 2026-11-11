import Blaze.Mir.Lemmas
import Blaze.Mir.Opt.Local
import Blaze.Mir.Opt.MemMap
import Blaze.Mir.Opt.Renumber

/-!
Merging a block into its predecessor: `merge_successor` of
crates/hir-ty-mir/src/mir/optimize/cfg.rs. A block that ends in `goto t` gets `t`'s
statements after its own and `t`'s terminator.

That is right whatever jumps to `t`, so the pass takes which targets to merge as a parameter
(`pick`) and is proved for every choice; `mergeSuccessors` picks as the Rust pass does, a
target with one predecessor. Blocks stay where they are (a merged-away block is
`removeDeadBlocks`' to remove), so a state of the program is a state of the merged program,
and the proof is a stuttering simulation through the identity with bound 2
(`mergeGotos_target`, `mergeGotos_source`).

The one fact it needs of the semantics: a block does the same from a state whose running
frame is at another block (`execBlockFrom_top`), since the merged block runs `t`'s statements
while its frame still says the predecessor. That holds unless the block ends in a call, which
would leave the frame's stale block in the state under the callee. So a target that ends in a
call is not merged: there the specification covers less than the Rust pass, as it does in
merging one target per run (a chain is the pass again).
-/

namespace Blaze.Mir

/-- The stack with the running frame at block `c`. -/
def withTopBlock (c : BlockId) : List Frame → List Frame
  | [] => []
  | fr :: rest => fr.withBlock c :: rest

theorem withTopBlock_memMap (c : BlockId) : MemMap (withTopBlock c) where
  locals := by
    intro l id
    cases l with
    | nil => rfl
    | cons fr rest =>
      by_cases h : fr.id = id
      · have h' : (fr.withBlock c).id = id := h
        have left : findFrame (withTopBlock c (fr :: rest)) id = some (fr.withBlock c) :=
          (findFrame_cons (fr.withBlock c) rest id).trans (if_pos h')
        have right : findFrame (fr :: rest) id = some fr :=
          (findFrame_cons fr rest id).trans (if_pos h)
        rw [left, right]
        try rfl
      · have h' : ¬ (fr.withBlock c).id = id := h
        have left : findFrame (withTopBlock c (fr :: rest)) id = findFrame rest id :=
          (findFrame_cons (fr.withBlock c) rest id).trans (if_neg h')
        have right : findFrame (fr :: rest) id = findFrame rest id :=
          (findFrame_cons fr rest id).trans (if_neg h)
        rw [left, right]
  set := by
    intro l id slots
    cases l with
    | nil => rfl
    | cons fr rest =>
      by_cases h : fr.id = id
      · have h' : (fr.withBlock c).id = id := h
        have left : setFrame (withTopBlock c (fr :: rest)) id slots =
            (fr.withBlock c).withLocals slots :: rest :=
          (setFrame_cons (fr.withBlock c) rest id slots).trans (if_pos h')
        have right : setFrame (fr :: rest) id slots = fr.withLocals slots :: rest :=
          (setFrame_cons fr rest id slots).trans (if_pos h)
        exact left.trans (congrTo (withTopBlock c) right).symm
      · have h' : ¬ (fr.withBlock c).id = id := h
        have left : setFrame (withTopBlock c (fr :: rest)) id slots =
            fr.withBlock c :: setFrame rest id slots :=
          (setFrame_cons (fr.withBlock c) rest id slots).trans (if_neg h')
        have right : setFrame (fr :: rest) id slots = fr :: setFrame rest id slots :=
          (setFrame_cons fr rest id slots).trans (if_neg h)
        exact left.trans (congrTo (withTopBlock c) right).symm
  top := by
    intro l
    cases l with
    | nil => rfl
    | cons fr rest => rfl

/-! A terminator that is no call does the same whatever block the running frame is at. -/

def isCall : Terminator → Bool
  | Terminator.goto _ => false
  | Terminator.switchInt _ _ _ => false
  | Terminator.call _ _ _ _ => true
  | Terminator.ret => false
  | Terminator.assert _ _ _ _ => false
  | Terminator.drop _ _ => false
  | Terminator.unreachable => false

theorem State.jump_top (c : BlockId) (st : State) (x : BlockId) :
    (st.mapStack (withTopBlock c)).jump x = st.jump x := by
  cases st with
  | mk base stack next =>
    cases stack with
    | nil => rfl
    | cons fr rest => rfl

theorem switchOn_top (c : BlockId) (st : State) (arms : List (Int × BlockId))
    (otherwise : BlockId) :
    ∀ (key : Option Int),
      switchOn (st.mapStack (withTopBlock c)) arms otherwise key = switchOn st arms otherwise key
  | some key => State.jump_top c st (switchTarget key arms otherwise)
  | none => rfl

theorem assertOn_top (c : BlockId) (st : State) (expected : Bool) (kind : Error)
    (target : BlockId) :
    ∀ (v : Value),
      assertOn (st.mapStack (withTopBlock c)) expected kind target v =
        assertOn st expected kind target v
  | Value.bool b => by
    by_cases h : b = expected
    · have left : assertOn (st.mapStack (withTopBlock c)) expected kind target (Value.bool b) =
          (st.mapStack (withTopBlock c)).jump target :=
        (assertOn_bool (st.mapStack (withTopBlock c)) expected kind target b).trans (if_pos h)
      have right : assertOn st expected kind target (Value.bool b) = st.jump target :=
        (assertOn_bool st expected kind target b).trans (if_pos h)
      exact left.trans ((State.jump_top c st target).trans right.symm)
    · have left : assertOn (st.mapStack (withTopBlock c)) expected kind target (Value.bool b) =
          Outcome.panic kind :=
        (assertOn_bool (st.mapStack (withTopBlock c)) expected kind target b).trans (if_neg h)
      have right : assertOn st expected kind target (Value.bool b) = Outcome.panic kind :=
        (assertOn_bool st expected kind target b).trans (if_neg h)
      exact left.trans right.symm
  | Value.unit => rfl
  | Value.int _ _ => rfl
  | Value.adt _ _ => rfl
  | Value.array _ => rfl
  | Value.ref _ => rfl
  | Value.dead => rfl

theorem State.ret_top (c : BlockId) (st : State) :
    (st.mapStack (withTopBlock c)).ret = st.ret := by
  cases st with
  | mk base stack next =>
    cases stack with
    | nil => rfl
    | cons fr rest =>
      cases fr with
      | mk fid ffn fblock locals dest target => rfl

theorem execTerm_top (sig : FnId → Option FnSig) (c : BlockId) (st : State) :
    ∀ (term : Terminator), isCall term = false →
      execTerm sig (st.mapStack (withTopBlock c)) term = execTerm sig st term
  | Terminator.goto x, _ => State.jump_top c st x
  | Terminator.switchInt discr arms otherwise, _ =>
    Outcome.bind_map_congr (evalOperand_mapStack (withTopBlock_memMap c) st discr) (fun r _ =>
      switchOn_top c r.2 arms otherwise (switchKey r.1))
  | Terminator.call _ _ _ _, h => true_eq_false_elim h
  | Terminator.ret, _ => State.ret_top c st
  | Terminator.assert cond expected kind target, _ =>
    Outcome.bind_map_congr (evalOperand_mapStack (withTopBlock_memMap c) st cond) (fun r _ =>
      assertOn_top c r.2 expected kind target r.1)
  | Terminator.drop _ x, _ => State.jump_top c st x
  | Terminator.unreachable, _ => rfl

/-- A block that does not end in a call does the same whatever block the running frame is
at. -/
theorem execBlockFrom_top (adts : List AdtDecl) (sig : FnId → Option FnSig) (c : BlockId)
    (term : Terminator) (h : isCall term = false) :
    ∀ (stmts : List Statement) (st : State),
      execBlockFrom adts sig (st.mapStack (withTopBlock c)) stmts term =
        execBlockFrom adts sig st stmts term
  | [], st => execTerm_top sig c st term h
  | s :: rest, st =>
    Outcome.bind_map_congr (execStmt_mapStack adts (withTopBlock_memMap c) st s) (fun st1 _ =>
      execBlockFrom_top adts sig c term h rest st1)

/-- Statements, then more statements and a terminator: what the first statements and a
`goto` do when they fail, or else the rest from the state the `goto` jumps from. -/
theorem execBlockFrom_append (adts : List AdtDecl) (sig : FnId → Option FnSig) (t : BlockId)
    (s2 : List Statement) (term2 : Terminator) :
    ∀ (s1 : List Statement) (st : State),
      execBlockFrom adts sig st (s1 ++ s2) term2 =
          execBlockFrom adts sig st s1 (Terminator.goto t) ∨
        ∃ st1, st1.ctl = st.ctl ∧
          execBlockFrom adts sig st s1 (Terminator.goto t) = st1.jump t ∧
          execBlockFrom adts sig st (s1 ++ s2) term2 = execBlockFrom adts sig st1 s2 term2
  | [], st => Or.inr ⟨st, rfl, rfl, rfl⟩
  | s :: rest, st => by
    cases hs : execStmt adts st s with
    | ok st' =>
      have a : execBlockFrom adts sig st ((s :: rest) ++ s2) term2 =
          execBlockFrom adts sig st' (rest ++ s2) term2 :=
        execBlockFrom_step adts sig st s (rest ++ s2) term2 st' hs
      have b : execBlockFrom adts sig st (s :: rest) (Terminator.goto t) =
          execBlockFrom adts sig st' rest (Terminator.goto t) :=
        execBlockFrom_step adts sig st s rest (Terminator.goto t) st' hs
      have hctl : st'.ctl = st.ctl := execStmt_ctl adts st s st' hs
      cases execBlockFrom_append adts sig t s2 term2 rest st' with
      | inl same => exact Or.inl (a.trans (same.trans b.symm))
      | inr two =>
        have ⟨st1, h1, h2, h3⟩ := two
        exact Or.inr ⟨st1, h1.trans hctl, b.trans h2, a.trans h3⟩
    | panic e =>
      have a : execBlockFrom adts sig st ((s :: rest) ++ s2) term2 = Outcome.panic e :=
        congrTo (fun x => Outcome.bind x (fun st1 => execBlockFrom adts sig st1 (rest ++ s2) term2))
          hs
      have b : execBlockFrom adts sig st (s :: rest) (Terminator.goto t) = Outcome.panic e :=
        congrTo
          (fun x => Outcome.bind x (fun st1 => execBlockFrom adts sig st1 rest (Terminator.goto t)))
          hs
      exact Or.inl (a.trans b.symm)
    | undefined why =>
      have a : execBlockFrom adts sig st ((s :: rest) ++ s2) term2 = Outcome.undefined why :=
        congrTo (fun x => Outcome.bind x (fun st1 => execBlockFrom adts sig st1 (rest ++ s2) term2))
          hs
      have b : execBlockFrom adts sig st (s :: rest) (Terminator.goto t) =
          Outcome.undefined why :=
        congrTo
          (fun x => Outcome.bind x (fun st1 => execBlockFrom adts sig st1 rest (Terminator.goto t)))
          hs
      exact Or.inl (a.trans b.symm)
    | diverge =>
      have a : execBlockFrom adts sig st ((s :: rest) ++ s2) term2 = Outcome.diverge :=
        congrTo (fun x => Outcome.bind x (fun st1 => execBlockFrom adts sig st1 (rest ++ s2) term2))
          hs
      have b : execBlockFrom adts sig st (s :: rest) (Terminator.goto t) = Outcome.diverge :=
        congrTo
          (fun x => Outcome.bind x (fun st1 => execBlockFrom adts sig st1 rest (Terminator.goto t)))
          hs
      exact Or.inl (a.trans b.symm)

/-- A jump to `t`, then the block `t`, is the block `t` from where the jump was made, when
`t` does not end in a call. -/
theorem jump_inline (prog : Program) (t : BlockId) (tb : BasicBlock)
    (hc : isCall tb.terminator = false) :
    ∀ (st1 : State) (c : Ctl) (cs : List Ctl), st1.ctl = c :: cs →
      prog.block st1.topFn t = some tb →
      ∃ s1, st1.jump t = Outcome.ok (Next.state s1) ∧
        step prog s1 = execBlockFrom prog.adts prog.sig st1 tb.stmts tb.terminator := by
  intro st1 c cs hne hb
  cases st1 with
  | mk base stack next =>
    cases stack with
    | nil =>
      have h' : ([] : List Ctl) = c :: cs := hne
      cases h'
    | cons fr rest =>
      have hb' : prog.block fr.fn t = some tb := hb
      have second :
          step prog { base := base, stack := fr.withBlock t :: rest, next := next } =
            execBlockFrom prog.adts prog.sig { base := base, stack := fr :: rest, next := next }
              tb.stmts tb.terminator := by
        show execBlock? prog.adts prog.sig
            { base := base, stack := fr.withBlock t :: rest, next := next }
            (prog.block fr.fn t) =
          execBlockFrom prog.adts prog.sig { base := base, stack := fr :: rest, next := next }
            tb.stmts tb.terminator
        rw [hb']
        exact execBlockFrom_top prog.adts prog.sig t tb.terminator hc tb.stmts
          { base := base, stack := fr :: rest, next := next }
      exact ⟨{ base := base, stack := fr.withBlock t :: rest, next := next }, rfl, second⟩

end Blaze.Mir

namespace Blaze.Mir.Opt

open Blaze.Mir

/-- Where the terminator goes, when it is a `goto`. -/
def gotoTarget : Terminator → Option BlockId
  | Terminator.goto t => some t
  | Terminator.switchInt _ _ _ => none
  | Terminator.call _ _ _ _ => none
  | Terminator.ret => none
  | Terminator.assert _ _ _ _ => none
  | Terminator.drop _ _ => none
  | Terminator.unreachable => none

theorem gotoTarget_some :
    ∀ (term : Terminator) (t : BlockId), gotoTarget term = some t → term = Terminator.goto t
  | Terminator.goto _, _, h => congrTo Terminator.goto (Option.some.inj h)
  | Terminator.switchInt _ _ _, _, h => none_eq_some_elim h
  | Terminator.call _ _ _ _, _, h => none_eq_some_elim h
  | Terminator.ret, _, h => none_eq_some_elim h
  | Terminator.assert _ _ _ _, _, h => none_eq_some_elim h
  | Terminator.drop _ _, _, h => none_eq_some_elim h
  | Terminator.unreachable, _, h => none_eq_some_elim h

/-- `blk` with `tb` after it, unless `tb` ends in a call. -/
def inlineInto (blk tb : BasicBlock) : BasicBlock :=
  if isCall tb.terminator = false then
    { stmts := blk.stmts ++ tb.stmts, terminator := tb.terminator }
  else blk

def inlineOpt (blk : BasicBlock) : Option BasicBlock → BasicBlock
  | some tb => inlineInto blk tb
  | none => blk

def mergeOpt (pick : BlockId → Bool) (blocks : List BasicBlock) (blk : BasicBlock) :
    Option BlockId → BasicBlock
  | some t => if pick t = true then inlineOpt blk (nth? blocks t) else blk
  | none => blk

/-- The block with the block its `goto` targets merged in, when `pick` chooses that target. -/
def mergeAt (pick : BlockId → Bool) (blocks : List BasicBlock) (blk : BasicBlock) : BasicBlock :=
  mergeOpt pick blocks blk (gotoTarget blk.terminator)

def mergeBody (pick : Body → BlockId → Bool) (body : Body) : Body :=
  body.mapBlocks (mergeAt (pick body) body.blocks)

/-- The pass, for a choice of the targets to merge. -/
def mergeGotos (pick : Body → BlockId → Bool) (prog : Program) : Program :=
  { adts := prog.adts, fns := prog.fns.map (mergeBody pick) }

/-- How many terminators jump to `t`. -/
def countPreds (blocks : List BasicBlock) (t : BlockId) : Nat :=
  blocks.foldl
    (fun n blk => n + ((successors blk.terminator).filter (fun x => decide (x = t))).length) 0

/-- A target other than the entry block that one terminator jumps to: what the Rust pass
merges. -/
def singlePred (body : Body) (t : BlockId) : Bool :=
  decide (t ≠ 0) && decide (countPreds body.blocks t = 1)

/-- The pass as `SimplifyCfg` picks: a block with one predecessor is merged into it. -/
def mergeSuccessors (prog : Program) : Program := mergeGotos singlePred prog

/-! One block. -/

/-- `x` in place of `blk` is one step of the program where `blk` is one, or two. -/
def Merged (prog : Program) (st : State) (blk x : BasicBlock) : Prop :=
  execBlock prog.adts prog.sig st x = execBlock prog.adts prog.sig st blk ∨
    ∃ s1, execBlock prog.adts prog.sig st blk = Outcome.ok (Next.state s1) ∧
      execBlock prog.adts prog.sig st x = step prog s1

theorem merged_self (prog : Program) (st : State) (blk : BasicBlock) :
    Merged prog st blk blk := Or.inl rfl

theorem merged_ite (prog : Program) (st : State) (blk : BasicBlock) (c : Prop) [Decidable c]
    (a b : BasicBlock) (ha : c → Merged prog st blk a) (hb : Merged prog st blk b) :
    Merged prog st blk (if c then a else b) := by
  by_cases h : c
  · rw [if_pos h]
    exact ha h
  · rw [if_neg h]
    exact hb

theorem inlineOpt_merged (prog : Program) (st : State) (blk : BasicBlock) :
    ∀ (o : Option BasicBlock),
      (∀ (tb : BasicBlock), o = some tb → Merged prog st blk (inlineInto blk tb)) →
        Merged prog st blk (inlineOpt blk o)
  | some tb, h => h tb rfl
  | none, _ => merged_self prog st blk

theorem mergeOpt_merged (prog : Program) (st : State) (pick : BlockId → Bool)
    (blocks : List BasicBlock) (blk : BasicBlock) :
    ∀ (o : Option BlockId),
      (∀ (t : BlockId), o = some t → Merged prog st blk (inlineOpt blk (nth? blocks t))) →
        Merged prog st blk (mergeOpt pick blocks blk o)
  | some t, h =>
    merged_ite prog st blk (pick t = true) (inlineOpt blk (nth? blocks t)) blk
      (fun _ => h t rfl) (merged_self prog st blk)
  | none, _ => merged_self prog st blk

/-- The merged block: the block's statements, a jump to `t`, then the block `t`. -/
theorem merged_inline (prog : Program) (st : State) (fn : FnId) (hfn : st.topFn = fn)
    (c : Ctl) (cs : List Ctl) (hst : st.ctl = c :: cs) (body : Body)
    (hbody : nth? prog.fns fn = some body) (blk : BasicBlock) (t : BlockId)
    (hterm : blk.terminator = Terminator.goto t) (tb : BasicBlock)
    (htb : nth? body.blocks t = some tb) (hc : isCall tb.terminator = false) :
    Merged prog st blk { stmts := blk.stmts ++ tb.stmts, terminator := tb.terminator } := by
  have src : execBlock prog.adts prog.sig st blk =
      execBlockFrom prog.adts prog.sig st blk.stmts (Terminator.goto t) :=
    congrTo (execBlockFrom prog.adts prog.sig st blk.stmts) hterm
  cases execBlockFrom_append prog.adts prog.sig t tb.stmts tb.terminator blk.stmts st with
  | inl same => exact Or.inl (same.trans src.symm)
  | inr two =>
    have ⟨st1, hctl, hj, hm⟩ := two
    have hblock : prog.block st1.topFn t = some tb := by
      show blockOf t (nth? prog.fns st1.topFn) = some tb
      rw [State.topFn_of_ctl hctl, hfn, hbody]
      exact htb
    have ⟨s1, hj1, hstep⟩ := jump_inline prog t tb hc st1 c cs (hctl.trans hst) hblock
    exact Or.inr ⟨s1, src.trans (hj.trans hj1), hm.trans hstep.symm⟩

theorem inlineInto_merged (prog : Program) (st : State) (blk tb : BasicBlock)
    (h : isCall tb.terminator = false →
      Merged prog st blk { stmts := blk.stmts ++ tb.stmts, terminator := tb.terminator }) :
    Merged prog st blk (inlineInto blk tb) :=
  merged_ite prog st blk (isCall tb.terminator = false)
    { stmts := blk.stmts ++ tb.stmts, terminator := tb.terminator } blk h
    (merged_self prog st blk)

theorem mergeAt_merged (prog : Program) (pick : Body → BlockId → Bool) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (c : Ctl) (cs : List Ctl) (hst : st.ctl = c :: cs) (body : Body)
    (hbody : nth? prog.fns fn = some body) (blk : BasicBlock) :
    Merged prog st blk (mergeAt (pick body) body.blocks blk) :=
  mergeOpt_merged prog st (pick body) body.blocks blk (gotoTarget blk.terminator) (fun t ht =>
    inlineOpt_merged prog st blk (nth? body.blocks t) (fun tb htb =>
      inlineInto_merged prog st blk tb (fun hc =>
        merged_inline prog st fn hfn c cs hst body hbody blk t
          (gotoTarget_some blk.terminator t ht) tb htb hc)))

/-! The program. -/

theorem sig_merge (pick : Body → BlockId → Bool) (prog : Program) :
    (mergeGotos pick prog).sig = prog.sig := by
  funext id
  show (nth? (prog.fns.map (mergeBody pick)) id).map _ = (nth? prog.fns id).map _
  rw [nth?_map]
  cases nth? prog.fns id with
  | none => rfl
  | some body => rfl

def mergedBlockOf (pick : Body → BlockId → Bool) (b : BlockId) : Option Body → Option BasicBlock
  | some body => (nth? body.blocks b).map (mergeAt (pick body) body.blocks)
  | none => none

theorem block_merge (pick : Body → BlockId → Bool) (prog : Program) (fn : FnId) (b : BlockId) :
    (mergeGotos pick prog).block fn b = mergedBlockOf pick b (nth? prog.fns fn) := by
  show blockOf b (nth? (prog.fns.map (mergeBody pick)) fn) =
    mergedBlockOf pick b (nth? prog.fns fn)
  rw [nth?_map]
  cases nth? prog.fns fn with
  | none => rfl
  | some body => exact nth?_map (mergeAt (pick body) body.blocks) body.blocks b

theorem merge_block (prog : Program) (pick : Body → BlockId → Bool) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (c : Ctl) (cs : List Ctl) (hst : st.ctl = c :: cs) (b : BlockId) :
    ∀ (body? : Option Body), nth? prog.fns fn = body? →
      execBlock? prog.adts prog.sig st (mergedBlockOf pick b body?) =
          execBlock? prog.adts prog.sig st (blockOf b body?) ∨
        ∃ s1, execBlock? prog.adts prog.sig st (blockOf b body?) = Outcome.ok (Next.state s1) ∧
          execBlock? prog.adts prog.sig st (mergedBlockOf pick b body?) = step prog s1
  | none, _ => Or.inl rfl
  | some body, hbody => by
    show execBlock? prog.adts prog.sig st
          ((nth? body.blocks b).map (mergeAt (pick body) body.blocks)) =
        execBlock? prog.adts prog.sig st (nth? body.blocks b) ∨
      ∃ s1, execBlock? prog.adts prog.sig st (nth? body.blocks b) =
          Outcome.ok (Next.state s1) ∧
        execBlock? prog.adts prog.sig st
          ((nth? body.blocks b).map (mergeAt (pick body) body.blocks)) = step prog s1
    cases nth? body.blocks b with
    | none => exact Or.inl rfl
    | some blk => exact mergeAt_merged prog pick st fn hfn c cs hst body hbody blk

/-- One step of the merged program is one step of the program, or two. -/
theorem merge_step (pick : Body → BlockId → Bool) (prog : Program) :
    ∀ (st : State),
      step (mergeGotos pick prog) st = step prog st ∨
        ∃ s1, step prog st = Outcome.ok (Next.state s1) ∧
          step (mergeGotos pick prog) st = step prog s1
  | { base := _, stack := [], next := _ } => Or.inl rfl
  | { base := base, stack := fr :: rest, next := next } => by
    have merged :
        step (mergeGotos pick prog) { base := base, stack := fr :: rest, next := next } =
          execBlock? prog.adts prog.sig { base := base, stack := fr :: rest, next := next }
            (mergedBlockOf pick fr.block (nth? prog.fns fr.fn)) := by
      show execBlock? prog.adts (mergeGotos pick prog).sig
          { base := base, stack := fr :: rest, next := next }
          ((mergeGotos pick prog).block fr.fn fr.block) =
        execBlock? prog.adts prog.sig { base := base, stack := fr :: rest, next := next }
          (mergedBlockOf pick fr.block (nth? prog.fns fr.fn))
      rw [block_merge, sig_merge]
    rw [merged]
    exact merge_block prog pick { base := base, stack := fr :: rest, next := next } fr.fn rfl
      (ctlOf fr) (rest.map ctlOf) rfl fr.block (nth? prog.fns fr.fn) rfl

/-- Merging is a stuttering simulation through the identity, with bound 2. -/
theorem merge_stutter (pick : Body → BlockId → Bool) (prog : Program) :
    Stutter prog (mergeGotos pick prog) (fun st => st) (fun _ => True) 2 where
  commutes := by
    intro st _
    cases merge_step pick prog st with
    | inl same =>
      exact ⟨0, Nat.zero_lt_succ 1,
        (same.trans (steps_one prog st).symm).trans (mapNext_id (steps prog (0 + 1) st)).symm,
        fun _ _ => True.intro⟩
    | inr two =>
      have ⟨s1, h1, h2⟩ := two
      exact ⟨1, Nat.lt_succ_self 1,
        (h2.trans (steps_two prog st s1 h1).symm).trans (mapNext_id (steps prog (1 + 1) st)).symm,
        fun _ _ => True.intro⟩

/-- A run of the merged program that ends in `n` steps is the program's run of `2 * n`. -/
theorem mergeGotos_target (pick : Body → BlockId → Bool) (prog : Program) (n : Nat) (st : State)
    (hne : run (mergeGotos pick prog) n st ≠ Outcome.diverge) :
    run prog (2 * n) st = run (mergeGotos pick prog) n st :=
  (merge_stutter pick prog).run_target n st True.intro hne

/-- A run of the program that ends in `n` steps is the merged program's run of `n`. -/
theorem mergeGotos_source (pick : Body → BlockId → Bool) (prog : Program) (n : Nat) (st : State)
    (hne : run prog n st ≠ Outcome.diverge) :
    run (mergeGotos pick prog) n st = run prog n st :=
  (merge_stutter pick prog).run_source n st True.intro hne

theorem start_merge (pick : Body → BlockId → Bool) (prog : Program) (id : FnId)
    (base : List Slot) (args : List Value) :
    (mergeGotos pick prog).start id base args = prog.start id base args := by
  unfold Program.start
  rw [sig_merge]

/-- A call that ends in `n` steps gives the same outcome in the merged program. -/
theorem mergeGotos_call_source (pick : Body → BlockId → Bool) (prog : Program) (n : Nat)
    (id : FnId) (base : List Slot) (args : List Value)
    (hne : prog.call n id base args ≠ Outcome.diverge) :
    (mergeGotos pick prog).call n id base args = prog.call n id base args :=
  Outcome.bind_congr (start_merge pick prog id base args) (fun st hst =>
    mergeGotos_source pick prog n st (fun hd =>
      hne ((congrTo (fun x => Outcome.bind x (run prog n)) hst).trans hd)))

end Blaze.Mir.Opt
