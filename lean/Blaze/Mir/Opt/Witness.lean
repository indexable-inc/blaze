import Blaze.Mir.Opt.SimplifyCfg
import Blaze.Mir.Opt.RemoveOverwritten
import Blaze.Mir.Opt.SimplifyConstCondition
import Blaze.Mir.Opt.RemoveStorageMarkers
import Blaze.Mir.Opt.ConstFold
import Blaze.Mir.Opt.RemoveDead

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

/-! A rewrite proved for calls whose caller hands over pointers into frame 0 only. -/

/-- The pointer is into frame 0: what the entry function's references point to. -/
def ZeroPtr (p : Pointer) : Prop := p.frame = 0

/-- Every pointer in the base slots and the arguments of a call is into frame 0. -/
def FrameZero (base : List Slot) (args : List Value) : Prop :=
  SlotsOk ZeroPtr base ∧ ∀ (v : Value), v ∈ args → Value.Ok ZeroPtr v

/-- The pass keeps every call that meets `C`, ends, and not in undefined behaviour. -/
def PreservesWhen (C : List Slot → List Value → Prop) (f : Program → Program) : Prop :=
  ∀ (prog : Program) (n : Nat) (id : FnId) (base : List Slot) (args : List Value)
    (r : Outcome Value), C base args →
    prog.call n id base args = r → r ≠ Outcome.diverge →
      (∀ (why : Undefined), r ≠ Outcome.undefined why) → (f prog).call n id base args = r

theorem Preserves.when {f : Program → Program} (h : Preserves f)
    (C : List Slot → List Value → Prop) : PreservesWhen C f :=
  fun prog n id base args r _ hr hd hu => h prog n id base args r hr hd hu

/-- `removeOverwritten` with the references the body takes as its borrowed locals, where the
program takes references only to those (`checkBorrows`); the program otherwise. -/
def removeOverwrittenChecked (prog : Program) : Program :=
  if checkBorrows refsOf prog = true then removeOverwritten refsOf prog else prog

theorem removeOverwrittenChecked_preserves : PreservesWhen FrameZero removeOverwrittenChecked := by
  intro prog n id base args r hz h _ hu
  show (if checkBorrows refsOf prog = true then removeOverwritten refsOf prog
    else prog).call n id base args = r
  by_cases hc : checkBorrows refsOf prog = true
  · rw [if_pos hc]
    have zero : ∀ (q : Pointer), ZeroPtr q → PtrOk (BorrowedIn refsOf prog) [] q :=
      fun _ hq => Or.inl hq
    have hbase : SlotsOk (PtrOk (BorrowedIn refsOf prog) []) base :=
      slotsOk_mono zero (And.left hz)
    have hargs : ∀ (v : Value), v ∈ args → Value.Ok (PtrOk (BorrowedIn refsOf prog) []) v :=
      fun v hv => Value.Ok.mono zero v (And.right hz v hv)
    have hr : (∃ why, prog.call n id base args = Outcome.undefined why) ∨
        (removeOverwritten refsOf prog).call n id base args = prog.call n id base args :=
      removeOverwritten_call refsOf prog (checkBorrows_sound refsOf prog hc) n id base args
        hbase hargs
    cases hr with
    | inl undef =>
      have ⟨why, e⟩ := undef
      exact absurd (h.symm.trans e) (hu why)
    | inr same => exact same.trans h
  · rw [if_neg hc]
    exact h

/-- `removeDead` with the certificate given and the references the body takes as its borrowed
locals, where the certificate is consistent (`checkLiveness`) and the program takes
references only to those (`checkBorrows`); the program otherwise. -/
def removeDeadStoresChecked (cert : List (List (List (List Local)))) (prog : Program) : Program :=
  if (checkBorrows refsOf prog && checkLiveness prog cert) = true then
    removeDead cert refsOf prog
  else prog

theorem removeDeadStoresChecked_preserves (cert : List (List (List (List Local)))) :
    PreservesWhen FrameZero (removeDeadStoresChecked cert) := by
  intro prog n id base args r hz h _ hu
  show (if (checkBorrows refsOf prog && checkLiveness prog cert) = true then
      removeDead cert refsOf prog
    else prog).call n id base args = r
  by_cases hc : (checkBorrows refsOf prog && checkLiveness prog cert) = true
  · rw [if_pos hc]
    have zero : ∀ (q : Pointer), ZeroPtr q → PtrOk (BorrowedIn refsOf prog) [] q :=
      fun _ hq => Or.inl hq
    have hbase : SlotsOk (PtrOk (BorrowedIn refsOf prog) []) base :=
      slotsOk_mono zero (And.left hz)
    have hargs : ∀ (v : Value), v ∈ args → Value.Ok (PtrOk (BorrowedIn refsOf prog) []) v :=
      fun v hv => Value.Ok.mono zero v (And.right hz v hv)
    have hr : (∃ why, prog.call n id base args = Outcome.undefined why) ∨
        (removeDead cert refsOf prog).call n id base args = prog.call n id base args :=
      removeDead_call cert refsOf prog (and_true_right hc) (and_true_left hc) n id base args
        hbase hargs
    cases hr with
    | inl undef =>
      have ⟨why, e⟩ := undef
      exact absurd (h.symm.trans e) (hu why)
    | inr same => exact same.trans h
  · rw [if_neg hc]
    exact h

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
  /-- A store its own block overwrites, removed: proved for calls whose caller hands over
  pointers into frame 0 only. -/
  | removeOverwritten
  /-- Every store a liveness certificate says is dead, removed (global dead-store
  elimination): the certificate is per function, per block, per point, the locals live
  there. Proved for the same calls. -/
  | removeDead (cert : List (List (List (List Local))))

/-- The rewrite is proved for every call. -/
def Rewrite.plain : Rewrite → Bool
  | Rewrite.removeOverwritten => false
  | Rewrite.removeDead _ => false
  | Rewrite.removeNops => true
  | Rewrite.removeStorageMarkers => true
  | Rewrite.constFold => true
  | Rewrite.simplifyConstCondition => true
  | Rewrite.switchToGoto => true
  | Rewrite.threadGotos => true
  | Rewrite.threadAll => true
  | Rewrite.mergeSuccessors => true
  | Rewrite.removeDeadBlocks => true
  | Rewrite.permute _ => true

def allPlain : List Rewrite → Bool
  | [] => true
  | r :: rest => r.plain && allPlain rest

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
  | Rewrite.removeOverwritten => Opt.removeOverwrittenChecked
  | Rewrite.removeDead cert => Opt.removeDeadStoresChecked cert

/-- Each plain rewrite keeps every call that ends, and not in undefined behaviour. -/
theorem Rewrite.preserves : ∀ (r : Rewrite), r.plain = true → Preserves r.apply
  | Rewrite.removeNops, _ => removeNops_preserves
  | Rewrite.removeStorageMarkers, _ =>
    mapBlocks_preserves removeMarkersBlock removeMarkersBlock_preserving
  | Rewrite.constFold, _ => mapBlocks_preserves foldBlock foldBlock_preserving
  | Rewrite.simplifyConstCondition, _ => mapBlocks_preserves simplifyBlock simplifyBlock_preserving
  | Rewrite.switchToGoto, _ => switchToGoto_preserves
  | Rewrite.threadGotos, _ => threadGotos_preserves
  | Rewrite.threadAll, _ => threadAll_preserves
  | Rewrite.mergeSuccessors, _ => mergeSuccessors_preserves
  | Rewrite.removeDeadBlocks, _ => removeDeadChecked_preserves
  | Rewrite.permute orders, _ => permuteChecked_preserves orders
  | Rewrite.removeOverwritten, h => false_eq_true_elim h
  | Rewrite.removeDead _, h => false_eq_true_elim h

/-- Every rewrite keeps every such call whose caller hands over pointers into frame 0 only. -/
theorem Rewrite.preservesWhen (r : Rewrite) : PreservesWhen FrameZero r.apply := by
  cases hp : r.plain with
  | true => exact Preserves.when (Rewrite.preserves r hp) FrameZero
  | false =>
    cases r with
    | removeOverwritten => exact removeOverwrittenChecked_preserves
    | removeDead cert => exact removeDeadStoresChecked_preserves cert
    | removeNops => exact false_eq_true_elim hp.symm
    | removeStorageMarkers => exact false_eq_true_elim hp.symm
    | constFold => exact false_eq_true_elim hp.symm
    | simplifyConstCondition => exact false_eq_true_elim hp.symm
    | switchToGoto => exact false_eq_true_elim hp.symm
    | threadGotos => exact false_eq_true_elim hp.symm
    | threadAll => exact false_eq_true_elim hp.symm
    | mergeSuccessors => exact false_eq_true_elim hp.symm
    | removeDeadBlocks => exact false_eq_true_elim hp.symm
    | permute orders => exact false_eq_true_elim hp.symm

/-- The script, replayed. -/
def applyAll : List Rewrite → Program → Program
  | [], prog => prog
  | r :: rest, prog => applyAll rest (r.apply prog)

theorem applyAll_preserves :
    ∀ (w : List Rewrite), allPlain w = true → Preserves (applyAll w)
  | [], _ => fun _ _ _ _ _ _ h _ _ => h
  | r :: rest, hp => fun prog n id base args out h hd hu =>
    applyAll_preserves rest (and_true_right hp) (r.apply prog) n id base args out
      (Rewrite.preserves r (and_true_left hp) prog n id base args out h hd hu) hd hu

theorem applyAll_preservesWhen : ∀ (w : List Rewrite), PreservesWhen FrameZero (applyAll w)
  | [] => fun _ _ _ _ _ _ _ h _ _ => h
  | r :: rest => fun prog n id base args out hz h hd hu =>
    applyAll_preservesWhen rest (r.apply prog) n id base args out hz
      (Rewrite.preservesWhen r prog n id base args out hz h hd hu) hd hu

/-- Whether the script `w` takes `a` to `b`. -/
def validateWith (a b : Program) (w : List Rewrite) : Bool := decide (applyAll w a = b)

/-- The validator is sound: when a script of rewrites proved for every call takes `a` to
`b`, every call of `a` that ends, and not in undefined behaviour, gives the same outcome in
`b` with the same fuel. -/
theorem validateWith_sound (a b : Program) (w : List Rewrite) (h : validateWith a b w = true)
    (hp : allPlain w = true) (n : Nat) (id : FnId) (base : List Slot) (args : List Value)
    (r : Outcome Value) (hr : a.call n id base args = r) (hd : r ≠ Outcome.diverge)
    (hu : ∀ (why : Undefined), r ≠ Outcome.undefined why) : b.call n id base args = r := by
  have e : applyAll w a = b := of_decide_eq_true h
  rw [← e]
  exact applyAll_preserves w hp a n id base args r hr hd hu

/-- The same for any script, `removeOverwritten` among its rewrites, for the calls whose
caller hands over pointers into frame 0 only. -/
theorem validateWith_sound_frameZero (a b : Program) (w : List Rewrite)
    (h : validateWith a b w = true) (n : Nat) (id : FnId) (base : List Slot)
    (args : List Value) (hz : FrameZero base args) (r : Outcome Value)
    (hr : a.call n id base args = r) (hd : r ≠ Outcome.diverge)
    (hu : ∀ (why : Undefined), r ≠ Outcome.undefined why) : b.call n id base args = r := by
  have e : applyAll w a = b := of_decide_eq_true h
  rw [← e]
  exact applyAll_preservesWhen w a n id base args r hz hr hd hu

end Blaze.Mir.Opt
