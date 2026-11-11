import Blaze.Mir.Run

/-!
Simulations: the one way a pass that changes the shape of states is proved
(docs/v2/plan/design/formal-verification.md, "The first proved transformations").

`Sim src tgt f Inv`: `f` takes a state of the source program to the state of the target
program that stands for it; on every state the invariant holds of, one step of the target
from `f st` is `f` of one step of the source from `st` (a lockstep forward simulation, as
CompCert's "star-free" diagram), and the invariant is kept. `Sim.run`: then the target's run
from `f st` is the source's run from `st`, with the same fuel.

A pass whose states are the source's (`Program.mapBlocks`) is the case `f = id`
(`Opt/Local.lean` proves that case directly). A pass that renumbers blocks or locals gives an
`f` that renames them; a pass that removes code nothing reaches gives an `Inv` that says
where control can be. A pass that changes how many steps a run takes (threading a `goto`)
needs the stuttering form, which is not here.
-/

namespace Blaze.Mir

def Next.map (f : State → State) : Next → Next
  | Next.state st => Next.state (f st)
  | Next.done v => Next.done v

def mapNext (f : State → State) : Outcome Next → Outcome Next
  | Outcome.ok next => Outcome.ok (next.map f)
  | Outcome.panic e => Outcome.panic e
  | Outcome.undefined why => Outcome.undefined why
  | Outcome.diverge => Outcome.diverge

/-- A lockstep simulation of `src` by `tgt` through `f`, on the states `Inv` holds of. -/
structure Sim (src tgt : Program) (f : State → State) (Inv : State → Prop) : Prop where
  /-- A step of the target from `f st` is `f` of the step of the source from `st`. -/
  commutes : ∀ (st : State), Inv st → Blaze.Mir.step tgt (f st) = mapNext f (Blaze.Mir.step src st)
  /-- The invariant holds of the next state. -/
  preserved : ∀ (st st' : State), Inv st →
    Blaze.Mir.step src st = Outcome.ok (Next.state st') → Inv st'

/-- A simulation gives equal runs: for every fuel and every state the invariant holds of. -/
theorem Sim.run {src tgt : Program} {f : State → State} {Inv : State → Prop}
    (h : Sim src tgt f Inv) :
    ∀ (fuel : Nat) (st : State), Inv st →
      Blaze.Mir.run tgt fuel (f st) = Blaze.Mir.run src fuel st := by
  intro fuel
  induction fuel with
  | zero =>
    intro st _
    rfl
  | succ n ih =>
    intro st hinv
    rw [run_succ, run_succ, h.commutes st hinv]
    cases hs : Blaze.Mir.step src st with
    | ok next =>
      cases next with
      | state st' => exact ih st' (h.preserved st st' hinv hs)
      | done v => rfl
    | panic e => rfl
    | undefined why => rfl
    | diverge => rfl

/-- The same as a refinement. -/
theorem Sim.refines {src tgt : Program} {f : State → State} {Inv : State → Prop}
    (h : Sim src tgt f Inv) (fuel : Nat) (st : State) (hinv : Inv st) :
    (Blaze.Mir.run tgt fuel (f st)).Refines (Blaze.Mir.run src fuel st) :=
  Outcome.Refines.of_eq (h.run fuel st hinv)

/-- Programs that step alike simulate each other through the identity. -/
theorem Sim.of_step_eq (src tgt : Program)
    (h : ∀ (st : State), Blaze.Mir.step tgt st = Blaze.Mir.step src st) :
    Sim src tgt (fun st => st) (fun _ => True) where
  commutes := by
    intro st _
    rw [h st]
    cases Blaze.Mir.step src st with
    | ok next =>
      cases next with
      | state st' => rfl
      | done v => rfl
    | panic e => rfl
    | undefined why => rfl
    | diverge => rfl
  preserved := fun _ _ _ _ => True.intro

end Blaze.Mir
