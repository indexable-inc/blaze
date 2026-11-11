import Blaze.Mir.Opt.RemoveNops
import Blaze.Mir.Opt.SwitchSame
import Blaze.Mir.Opt.Merge
import Blaze.Mir.Opt.ThreadCalls

/-!
`SimplifyCfg`, composed from the proved passes in the order of the loop in
crates/hir-ty-mir/src/mir/optimize/cfg.rs: `nop`s removed; then rounds of threading every
target, a `switchInt` whose arms agree made a `goto`, dead blocks removed, and a block with
one predecessor merged into it; then dead blocks removed.

What is proved of the composition is what each part gives, composed (`Preserves`): a call
that ends, and not in undefined behaviour, gives the same outcome with the same fuel
(`simplifyCfg_call`). The parts differ in what they are (an equality, a refinement, two
stuttering simulations), and that is the statement all of them have.

It is not yet the Rust pass, block for block. The Rust loop changes blocks in place, one at
a time, with predecessor counts kept as it goes, where each part here rewrites every block
from the program it was given; it merges a target that ends in a call, rewrites a
`switchInt` on a `move`, and merges the empty `unreachable` blocks. So `after = spec before`
is to be tried per body (`mir_semantics.py validate`), and where it fails the body says
which of these it met.
-/

namespace Blaze.Mir.Opt

open Blaze.Mir

/-- The pass keeps every call that ends, and not in undefined behaviour, with its fuel. -/
def Preserves (f : Program → Program) : Prop :=
  ∀ (prog : Program) (n : Nat) (id : FnId) (base : List Slot) (args : List Value)
    (r : Outcome Value),
    prog.call n id base args = r → r ≠ Outcome.diverge →
      (∀ (why : Undefined), r ≠ Outcome.undefined why) → (f prog).call n id base args = r

theorem Preserves.comp {f g : Program → Program} (hf : Preserves f) (hg : Preserves g) :
    Preserves (fun prog => g (f prog)) :=
  fun prog n id base args r h hd hu =>
    hg (f prog) n id base args r (hf prog n id base args r h hd hu) hd hu

/-- `f`, `k` times. -/
def rounds (f : Program → Program) : Nat → Program → Program
  | 0, prog => prog
  | k + 1, prog => f (rounds f k prog)

theorem Preserves.rounds {f : Program → Program} (hf : Preserves f) :
    ∀ (k : Nat), Preserves (Blaze.Mir.Opt.rounds f k)
  | 0 => fun _ _ _ _ _ _ h _ _ => h
  | k + 1 => fun prog n id base args r h hd hu =>
    hf (Blaze.Mir.Opt.rounds f k prog) n id base args r
      (Preserves.rounds hf k prog n id base args r h hd hu) hd hu

/-! Each part. -/

theorem start_mapBlocks (f : BasicBlock → BasicBlock) (prog : Program) (id : FnId)
    (base : List Slot) (args : List Value) :
    (prog.mapBlocks f).start id base args = prog.start id base args := by
  unfold Program.start
  rw [Program.sig_mapBlocks]

theorem call_mapBlocks (f : BasicBlock → BasicBlock) (hf : BlockPreserving f) (prog : Program)
    (n : Nat) (id : FnId) (base : List Slot) (args : List Value) :
    (prog.mapBlocks f).call n id base args = prog.call n id base args :=
  Outcome.bind_congr (start_mapBlocks f prog id base args) (fun st _ =>
    run_mapBlocks f hf prog n st)

theorem removeNops_preserves : Preserves removeNops :=
  fun prog n id base args _ h _ _ =>
    (call_mapBlocks removeNopsBlock removeNopsBlock_preserving prog n id base args).trans h

theorem threadAll_preserves : Preserves threadAll :=
  fun prog n id base args _ h hd _ =>
    (threadAll_call_source prog n id base args (fun e => hd (h.symm.trans e))).trans h

theorem mergeSuccessors_preserves : Preserves mergeSuccessors :=
  fun prog n id base args _ h hd _ =>
    (mergeGotos_call_source singlePred prog n id base args
      (fun e => hd (h.symm.trans e))).trans h

theorem switchToGoto_call_refines (prog : Program) (n : Nat) (id : FnId) (base : List Slot)
    (args : List Value) :
    ((switchToGoto prog).call n id base args).Refines (prog.call n id base args) := by
  have es : (prog.mapBlocks switchToGotoBlock).start id base args = prog.start id base args :=
    start_mapBlocks switchToGotoBlock prog id base args
  show Outcome.Refines
    (((prog.mapBlocks switchToGotoBlock).start id base args).bind (run (switchToGoto prog) n))
    ((prog.start id base args).bind (run prog n))
  rw [es]
  exact Outcome.Refines.bind (fun st _ => switchToGoto_refines prog n st)

theorem switchToGoto_preserves : Preserves switchToGoto := by
  intro prog n id base args r h _ hu
  have hr : (∃ why, prog.call n id base args = Outcome.undefined why) ∨
      (switchToGoto prog).call n id base args = prog.call n id base args :=
    switchToGoto_call_refines prog n id base args
  cases hr with
  | inl undef =>
    have ⟨why, e⟩ := undef
    exact absurd (h.symm.trans e) (hu why)
  | inr same => exact same.trans h

/-- Dead-block removal where its check passes, the program where it does not. -/
def removeDeadChecked (prog : Program) : Program :=
  if checkDead prog = true then removeDeadBlocks prog else prog

theorem removeDeadChecked_preserves : Preserves removeDeadChecked := by
  intro prog n id base args r h _ _
  show (if checkDead prog = true then removeDeadBlocks prog else prog).call n id base args = r
  by_cases hc : checkDead prog = true
  · rw [if_pos hc]
    exact (removeDeadBlocks_call prog hc n id base args).trans h
  · rw [if_neg hc]
    exact h

/-! The composition. -/

/-- One round of the loop. -/
def cfgRound (prog : Program) : Program :=
  mergeSuccessors (removeDeadChecked (switchToGoto (threadAll prog)))

theorem cfgRound_preserves : Preserves cfgRound :=
  Preserves.comp
    (Preserves.comp (Preserves.comp threadAll_preserves switchToGoto_preserves)
      removeDeadChecked_preserves)
    mergeSuccessors_preserves

/-- `SimplifyCfg` with `k` rounds of its loop. -/
def simplifyCfg (k : Nat) (prog : Program) : Program :=
  removeDeadChecked (rounds cfgRound k (removeNops prog))

theorem simplifyCfg_preserves (k : Nat) : Preserves (simplifyCfg k) :=
  Preserves.comp (Preserves.comp removeNops_preserves (Preserves.rounds cfgRound_preserves k))
    removeDeadChecked_preserves

/-- `SimplifyCfg` keeps every call that ends, and not in undefined behaviour: the same
outcome with the same fuel. -/
theorem simplifyCfg_call (k : Nat) (prog : Program) (n : Nat) (id : FnId) (base : List Slot)
    (args : List Value) (r : Outcome Value) (h : prog.call n id base args = r)
    (hd : r ≠ Outcome.diverge) (hu : ∀ (why : Undefined), r ≠ Outcome.undefined why) :
    (simplifyCfg k prog).call n id base args = r :=
  simplifyCfg_preserves k prog n id base args r h hd hu

end Blaze.Mir.Opt
