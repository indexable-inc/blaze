import Blaze.Mir.Opt.Local

/-!
`RemoveStorageMarkers` (crates/hir-ty-mir/src/mir/optimize/locals.rs,
`remove_storage_markers`; rustc's pass of the name): every `StorageLive` and `StorageDead`
goes. The Rust pass does exactly this, so `after = removeStorageMarkers before` is checked
for every body it runs on (`harness/v2/mir_semantics.py validate`).

The theorem is about this semantics, in which a storage marker does nothing: it says the
pass changes no run, and it says nothing of what a marker means to a backend (the extent of a
stack slot), which the first subset does not model.
-/

namespace Blaze.Mir.Opt

open Blaze.Mir

def isStorageMarker : Statement → Bool
  | Statement.storageLive _ => true
  | Statement.storageDead _ => true
  | _ => false

def withoutMarkers (stmts : List Statement) : List Statement :=
  stmts.filter (fun s => !isStorageMarker s)

def removeMarkersBlock (blk : BasicBlock) : BasicBlock :=
  { stmts := withoutMarkers blk.stmts, terminator := blk.terminator }

/-- The pass. -/
def removeStorageMarkers (prog : Program) : Program := prog.mapBlocks removeMarkersBlock

theorem execBlockFrom_withoutMarkers (adts : List AdtDecl) (sig : FnId → Option FnSig)
    (t : Terminator) :
    ∀ (stmts : List Statement) (st : State),
      execBlockFrom adts sig st (withoutMarkers stmts) t = execBlockFrom adts sig st stmts t := by
  intro stmts
  induction stmts with
  | nil =>
    intro st
    rfl
  | cons s rest ih =>
    intro st
    have kept : (fun st1 => execBlockFrom adts sig st1 (withoutMarkers rest) t) =
        (fun st1 => execBlockFrom adts sig st1 rest t) := funext ih
    cases s with
    | assign p rv =>
      show execBlockFrom adts sig st (Statement.assign p rv :: withoutMarkers rest) t =
        execBlockFrom adts sig st (Statement.assign p rv :: rest) t
      rw [execBlockFrom_cons, execBlockFrom_cons, kept]
    | storageLive l =>
      show execBlockFrom adts sig st (withoutMarkers rest) t =
        execBlockFrom adts sig st (Statement.storageLive l :: rest) t
      exact ih st
    | storageDead l =>
      show execBlockFrom adts sig st (withoutMarkers rest) t =
        execBlockFrom adts sig st (Statement.storageDead l :: rest) t
      exact ih st
    | nop =>
      show execBlockFrom adts sig st (Statement.nop :: withoutMarkers rest) t =
        execBlockFrom adts sig st (Statement.nop :: rest) t
      rw [execBlockFrom_cons, execBlockFrom_cons, kept]

theorem removeMarkersBlock_preserving : BlockPreserving removeMarkersBlock := by
  unfold BlockPreserving
  intro adts sig st blk
  exact execBlockFrom_withoutMarkers adts sig blk.terminator blk.stmts st

/-- `RemoveStorageMarkers` is correct: for every program, fuel and state, the program
without its markers runs as the program does. -/
theorem removeStorageMarkers_correct (prog : Program) (fuel : Nat) (st : State) :
    run (removeStorageMarkers prog) fuel st = run prog fuel st :=
  run_mapBlocks removeMarkersBlock removeMarkersBlock_preserving prog fuel st

/-- The same as a refinement. -/
theorem removeStorageMarkers_refines (prog : Program) (fuel : Nat) (st : State) :
    (run (removeStorageMarkers prog) fuel st).Refines (run prog fuel st) :=
  Outcome.Refines.of_eq (removeStorageMarkers_correct prog fuel st)

end Blaze.Mir.Opt
