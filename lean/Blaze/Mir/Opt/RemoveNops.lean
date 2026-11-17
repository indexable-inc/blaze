import Blaze.Mir.Opt.Local

/-!
Removing `nop`s: what `DeadStoreElimination` and `SimplifyCfg` do after they turn a statement
into a `Nop` (crates/hir-ty-mir/src/mir/optimize/dse.rs, `remove_unused_storage_markers`;
cfg.rs). No Rust pass does only this, so no pair is checked against it; it is one of the
rewrites the validator's normal form applies (`Opt/Validate.lean`).
-/

namespace Blaze.Mir.Opt

open Blaze.Mir

def isNop : Statement → Bool
  | Statement.nop => true
  | _ => false

def withoutNops (stmts : List Statement) : List Statement :=
  stmts.filter (fun s => !isNop s)

def removeNopsBlock (blk : BasicBlock) : BasicBlock :=
  { stmts := withoutNops blk.stmts, terminator := blk.terminator }

/-- The pass. -/
def removeNops (prog : Program) : Program := prog.mapBlocks removeNopsBlock

theorem execBlockFrom_withoutNops (adts : List AdtDecl) (sig : FnId → Option FnSig)
    (t : Terminator) :
    ∀ (stmts : List Statement) (st : State),
      execBlockFrom adts sig st (withoutNops stmts) t = execBlockFrom adts sig st stmts t := by
  intro stmts
  induction stmts with
  | nil =>
    intro st
    rfl
  | cons s rest ih =>
    intro st
    have kept : (fun st1 => execBlockFrom adts sig st1 (withoutNops rest) t) =
        (fun st1 => execBlockFrom adts sig st1 rest t) := funext ih
    cases s with
    | assign p rv =>
      show execBlockFrom adts sig st (Statement.assign p rv :: withoutNops rest) t =
        execBlockFrom adts sig st (Statement.assign p rv :: rest) t
      rw [execBlockFrom_cons, execBlockFrom_cons, kept]
    | storageLive l =>
      show execBlockFrom adts sig st (Statement.storageLive l :: withoutNops rest) t =
        execBlockFrom adts sig st (Statement.storageLive l :: rest) t
      rw [execBlockFrom_cons, execBlockFrom_cons, kept]
    | storageDead l =>
      show execBlockFrom adts sig st (Statement.storageDead l :: withoutNops rest) t =
        execBlockFrom adts sig st (Statement.storageDead l :: rest) t
      rw [execBlockFrom_cons, execBlockFrom_cons, kept]
    | nop =>
      show execBlockFrom adts sig st (withoutNops rest) t =
        execBlockFrom adts sig st (Statement.nop :: rest) t
      exact ih st

theorem removeNopsBlock_preserving : BlockPreserving removeNopsBlock := by
  unfold BlockPreserving
  intro adts sig st blk
  exact execBlockFrom_withoutNops adts sig blk.terminator blk.stmts st

/-- Removing `nop`s is correct: for every program, fuel and state, the program without them
runs as the program does. -/
theorem removeNops_correct (prog : Program) (fuel : Nat) (st : State) :
    run (removeNops prog) fuel st = run prog fuel st :=
  run_mapBlocks removeNopsBlock removeNopsBlock_preserving prog fuel st

end Blaze.Mir.Opt
