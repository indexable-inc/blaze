import Blaze.Mir.Opt.SimplifyConstCondition
import Blaze.Mir.Opt.RemoveStorageMarkers
import Blaze.Mir.Opt.RemoveNops
import Blaze.Mir.Opt.ConstFold

/-!
A translation validator proved sound once (objective V8's second half; Pnueli, Siegel and
Singerman's translation validation, with Alive2's lesson that the checker is the small
thing to trust, here proved).

`validate a b` accepts a program `b` as a compilation of `a` when the two have the same
normal form: the program after every rewrite proved to preserve runs (`normalize`: storage
markers and `nop`s removed, constants folded, constant conditions made `goto`s). It needs
no witness. `validate_sound`: an accepted pair runs alike, for every fuel and state. The
check is a computation Lean's kernel does, per pair, with no axiom.

What it accepts is what the normal form identifies, so it grows with every proved rewrite
added to `normalize`. It does not yet accept a pass that changes locals or blocks
(`SimplifyLocals`, `SimplifyCfg`, `CopyProp`, `DeadStoreElimination`): that needs the
symbolic-execution form, in which each block of the two bodies is run on symbolic locals to
a store of expressions and the stores compared under a block mapping, proved sound through
`Opt/Sim.lean`. The inliner needs more: a relation between one frame and two.
-/

namespace Blaze.Mir.Opt

open Blaze.Mir

/-- The normal form: every proved rewrite, applied once. -/
def normalize (prog : Program) : Program :=
  simplifyConstCondition (constFold (removeNops (removeStorageMarkers prog)))

/-- A program runs as its normal form does. -/
theorem run_normalize (prog : Program) (fuel : Nat) (st : State) :
    run (normalize prog) fuel st = run prog fuel st :=
  (simplifyConstCondition_correct (constFold (removeNops (removeStorageMarkers prog))) fuel
      st).trans
    ((constFold_correct (removeNops (removeStorageMarkers prog)) fuel st).trans
      ((removeNops_correct (removeStorageMarkers prog) fuel st).trans
        (removeStorageMarkers_correct prog fuel st)))

/-- Whether `b` is accepted as a compilation of `a`: their normal forms are equal. -/
def validate (a b : Program) : Bool := decide (normalize a = normalize b)

/-- The validator is sound: an accepted pair runs alike, for every fuel and every state. -/
theorem validate_sound (a b : Program) (h : validate a b = true) (fuel : Nat) (st : State) :
    run b fuel st = run a fuel st :=
  have e : normalize a = normalize b := of_decide_eq_true h
  (run_normalize b fuel st).symm.trans
    ((congrArg (fun p => run p fuel st) e).symm.trans (run_normalize a fuel st))

/-- The same as a refinement: every behaviour of the accepted program is one of the
source's. -/
theorem validate_refines (a b : Program) (h : validate a b = true) (fuel : Nat) (st : State) :
    (run b fuel st).Refines (run a fuel st) :=
  Outcome.Refines.of_eq (validate_sound a b h fuel st)

end Blaze.Mir.Opt
