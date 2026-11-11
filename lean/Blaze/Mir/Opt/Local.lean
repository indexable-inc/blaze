import Blaze.Mir.Run

/-!
Passes that rewrite each block on its own (docs/v2/plan/design/formal-verification.md, "The
first proved transformation"). A pass is a function on programs, written here; it is the
specification of the Rust pass of the same name in crates/hir-ty-mir/src/mir/optimize.

`run_mapBlocks`: when a rewrite of a block leaves what the block does from every state as it
was, the rewritten program runs as the program does, with the same fuel. The proof is a
forward simulation in CompCert's sense with the identity as its relation; each pass proves
its own block lemma.
-/

namespace Blaze.Mir

def Body.mapBlocks (f : BasicBlock → BasicBlock) (body : Body) : Body :=
  { name := body.name, args := body.args, locals := body.locals, blocks := body.blocks.map f }

def Program.mapBlocks (f : BasicBlock → BasicBlock) (prog : Program) : Program :=
  { adts := prog.adts, fns := prog.fns.map (Body.mapBlocks f) }

theorem Program.sig_mapBlocks (f : BasicBlock → BasicBlock) (prog : Program) :
    (prog.mapBlocks f).sig = prog.sig := by
  funext i
  show (nth? (prog.fns.map (Body.mapBlocks f)) i).map _ = (nth? prog.fns i).map _
  rw [nth?_map]
  cases nth? prog.fns i with
  | none => rfl
  | some body => rfl

theorem Program.block_mapBlocks (f : BasicBlock → BasicBlock) (prog : Program) (fn : FnId)
    (block : BlockId) : (prog.mapBlocks f).block fn block = (prog.block fn block).map f := by
  show blockOf block (nth? (prog.fns.map (Body.mapBlocks f)) fn) =
    (blockOf block (nth? prog.fns fn)).map f
  rw [nth?_map]
  cases nth? prog.fns fn with
  | none => rfl
  | some body => exact nth?_map f body.blocks block

/-- A rewrite that leaves each block's meaning as it was. -/
def BlockPreserving (f : BasicBlock → BasicBlock) : Prop :=
  ∀ (adts : List AdtDecl) (sig : FnId → Option FnSig) (st : State) (blk : BasicBlock),
    execBlock adts sig st (f blk) = execBlock adts sig st blk

theorem stepFrame_mapBlocks (f : BasicBlock → BasicBlock) (hf : BlockPreserving f)
    (prog : Program) (st : State) (fr : Frame) :
    stepFrame (prog.mapBlocks f) st fr = stepFrame prog st fr := by
  unfold stepFrame
  rw [Program.block_mapBlocks, Program.sig_mapBlocks]
  cases prog.block fr.fn fr.block with
  | none => rfl
  | some blk => exact hf prog.adts prog.sig st blk

theorem step_mapBlocks (f : BasicBlock → BasicBlock) (hf : BlockPreserving f)
    (prog : Program) (st : State) : step (prog.mapBlocks f) st = step prog st := by
  unfold step
  cases st.stack with
  | nil => rfl
  | cons fr rest => exact stepFrame_mapBlocks f hf prog st fr

/-- A block-preserving rewrite preserves every run: for every fuel and state, the rewritten
program's outcome is the program's. -/
theorem run_mapBlocks (f : BasicBlock → BasicBlock) (hf : BlockPreserving f)
    (prog : Program) :
    ∀ (fuel : Nat) (st : State), run (prog.mapBlocks f) fuel st = run prog fuel st := by
  intro fuel
  induction fuel with
  | zero =>
    intro st
    rfl
  | succ n ih =>
    intro st
    have rest : run (prog.mapBlocks f) n = run prog n := funext ih
    rw [run_succ, run_succ, step_mapBlocks f hf prog st, rest]

/-- The same as a refinement: every behaviour of the rewritten program is a behaviour of the
program. -/
theorem refines_mapBlocks (f : BasicBlock → BasicBlock) (hf : BlockPreserving f)
    (prog : Program) (fuel : Nat) (st : State) :
    (run (prog.mapBlocks f) fuel st).Refines (run prog fuel st) :=
  Outcome.Refines.of_eq (run_mapBlocks f hf prog fuel st)

/-- A rewrite of terminators alone preserves a block when it preserves each terminator. -/
theorem execBlockFrom_terminator (g : Terminator → Terminator)
    (hg : ∀ (sig : FnId → Option FnSig) (st : State) (t : Terminator),
      execTerm sig st (g t) = execTerm sig st t)
    (adts : List AdtDecl) (sig : FnId → Option FnSig) (t : Terminator) :
    ∀ (stmts : List Statement) (st : State),
      execBlockFrom adts sig st stmts (g t) = execBlockFrom adts sig st stmts t := by
  intro stmts
  induction stmts with
  | nil =>
    intro st
    exact hg sig st t
  | cons s rest ih =>
    intro st
    have same : (fun st1 => execBlockFrom adts sig st1 rest (g t)) =
        (fun st1 => execBlockFrom adts sig st1 rest t) := funext ih
    rw [execBlockFrom_cons, execBlockFrom_cons, same]

end Blaze.Mir
