import Blaze.Mir.Opt.SimplifyCfg
import Blaze.Mir.Opt.SimplifyConstCondition
import Blaze.Mir.Opt.RemoveStorageMarkers
import Blaze.Mir.Opt.ConstFold

/-!
A translation validator that takes a witness (objective V8's second half; Pnueli, Siegel and
Singerman's translation validation, with the witness as in Necula's and Rinard's).

The witness is a script: the proved rewrites, in order, that take the program before a pass
to the program after it (`Rewrite`: each is one of the passes of this directory, with what it
takes as data, a block order for a permutation). `validateWith a b w` replays the script on
`a` and accepts when it gives `b`, a comparison the kernel decides. `validateWith_sound`: an
accepted pair gives every call that ends, and not in undefined behaviour, the same outcome
with the same fuel.

So the Rust pass is not trusted and need not be the composition block for block of a fixed
specification: whatever it did, a script that reproduces its output is evidence, and the
script can be found by the pass (which rewrites it applied, in which order) or by search.
What it accepts grows with every rewrite added to `Rewrite` with its `Preserves` proof.

What this is not: the validator by symbolic execution, which compares each block of the two
programs through a block map by running both on symbolic locals to a normal form (stores of
expressions, panics in order, a terminator with a symbolic condition). That form needs no
script and accepts rewrites nobody proved one by one (copy propagation, local renumbering,
reordering of independent statements); it needs an expression language with its evaluation
proved to agree with `evalRvalue`, and `Renumbers` with "the target block is symbolically
equal" in place of "is the renamed block". The inliner needs a relation between one frame
and two beyond that, and loop transforms a witness that is not per block.
-/

namespace Blaze.Mir.Opt

open Blaze.Mir

def ordersOr : Option (List BlockId) → List BlockId
  | some order => order
  | none => []

/-- A block permutation where its check passes, the program where it does not. -/
def permuteChecked (orders : List (List BlockId)) (prog : Program) : Program :=
  if checkPermutation (fun fn => ordersOr (nth? orders fn)) prog = true then
    renumber (fun fn => ordersOr (nth? orders fn)) prog
  else prog

theorem permuteChecked_preserves (orders : List (List BlockId)) :
    Preserves (permuteChecked orders) := by
  intro prog n id base args r h _ _
  show (if checkPermutation (fun fn => ordersOr (nth? orders fn)) prog = true then
      renumber (fun fn => ordersOr (nth? orders fn)) prog
    else prog).call n id base args = r
  by_cases hc : checkPermutation (fun fn => ordersOr (nth? orders fn)) prog = true
  · rw [if_pos hc]
    exact (permuteBlocks_call (fun fn => ordersOr (nth? orders fn)) prog hc n id base args).trans h
  · rw [if_neg hc]
    exact h

theorem mapBlocks_preserves (f : BasicBlock → BasicBlock) (hf : BlockPreserving f) :
    Preserves (fun prog => prog.mapBlocks f) :=
  fun prog n id base args _ h _ _ => (call_mapBlocks f hf prog n id base args).trans h

theorem threadGotos_preserves : Preserves threadGotos :=
  fun prog n id base args _ h hd _ =>
    (threadGotos_call_source prog n id base args (fun e => hd (h.symm.trans e))).trans h

/-- One proved rewrite, with what it takes. -/
inductive Rewrite where
  | removeNops
  | removeStorageMarkers
  | constFold
  | simplifyConstCondition
  | switchToGoto
  | threadGotos
  | threadAll
  | mergeSuccessors
  | removeDeadBlocks
  /-- A permutation of each function's blocks: per function, the old numbers in their new
  order. -/
  | permute (orders : List (List BlockId))

def Rewrite.apply : Rewrite → Program → Program
  | Rewrite.removeNops => Opt.removeNops
  | Rewrite.removeStorageMarkers => Opt.removeStorageMarkers
  | Rewrite.constFold => Opt.constFold
  | Rewrite.simplifyConstCondition => Opt.simplifyConstCondition
  | Rewrite.switchToGoto => Opt.switchToGoto
  | Rewrite.threadGotos => Opt.threadGotos
  | Rewrite.threadAll => Opt.threadAll
  | Rewrite.mergeSuccessors => Opt.mergeSuccessors
  | Rewrite.removeDeadBlocks => Opt.removeDeadChecked
  | Rewrite.permute orders => Opt.permuteChecked orders

/-- Each rewrite keeps every call that ends, and not in undefined behaviour. -/
theorem Rewrite.preserves : ∀ (r : Rewrite), Preserves r.apply
  | Rewrite.removeNops => removeNops_preserves
  | Rewrite.removeStorageMarkers => mapBlocks_preserves removeMarkersBlock removeMarkersBlock_preserving
  | Rewrite.constFold => mapBlocks_preserves foldBlock foldBlock_preserving
  | Rewrite.simplifyConstCondition => mapBlocks_preserves simplifyBlock simplifyBlock_preserving
  | Rewrite.switchToGoto => switchToGoto_preserves
  | Rewrite.threadGotos => threadGotos_preserves
  | Rewrite.threadAll => threadAll_preserves
  | Rewrite.mergeSuccessors => mergeSuccessors_preserves
  | Rewrite.removeDeadBlocks => removeDeadChecked_preserves
  | Rewrite.permute orders => permuteChecked_preserves orders

/-- The script, replayed. -/
def applyAll : List Rewrite → Program → Program
  | [], prog => prog
  | r :: rest, prog => applyAll rest (r.apply prog)

theorem applyAll_preserves : ∀ (w : List Rewrite), Preserves (applyAll w)
  | [] => fun _ _ _ _ _ _ h _ _ => h
  | r :: rest => fun prog n id base args out h hd hu =>
    applyAll_preserves rest (r.apply prog) n id base args out
      (Rewrite.preserves r prog n id base args out h hd hu) hd hu

/-- Whether the script `w` takes `a` to `b`. -/
def validateWith (a b : Program) (w : List Rewrite) : Bool := decide (applyAll w a = b)

/-- The validator is sound: when a script of proved rewrites takes `a` to `b`, every call of
`a` that ends, and not in undefined behaviour, gives the same outcome in `b` with the same
fuel. -/
theorem validateWith_sound (a b : Program) (w : List Rewrite) (h : validateWith a b w = true)
    (n : Nat) (id : FnId) (base : List Slot) (args : List Value) (r : Outcome Value)
    (hr : a.call n id base args = r) (hd : r ≠ Outcome.diverge)
    (hu : ∀ (why : Undefined), r ≠ Outcome.undefined why) : b.call n id base args = r := by
  have e : applyAll w a = b := of_decide_eq_true h
  rw [← e]
  exact applyAll_preserves w a n id base args r hr hd hu

end Blaze.Mir.Opt
