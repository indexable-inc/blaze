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
where control can be (`Opt/Rename.lean`, `Opt/Reach.lean`).

A pass that changes how many steps a run takes (threading a `goto`) is proved with the
stuttering form, `Stutter src tgt f Inv K`: one step of the target from `f st` is `f` of
`k + 1` steps of the source from `st`, for some `k < K`. The runs then agree up to fuel, and
the fuel is stated: a target run of `n` steps that ends is the source's run of `K * n` steps
(`Stutter.run_target`), and a source run of `n` steps that ends is the target's run of `n`
steps (`Stutter.run_source`). So neither diverges unless the other does.
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

/-! Stuttering: the target takes one step where the source takes one or more. -/

/-- What `k` more steps make of one step's outcome. -/
def thenSteps (rest : State → Outcome Next) : Outcome Next → Outcome Next
  | Outcome.ok (Next.state st) => rest st
  | Outcome.ok (Next.done v) => Outcome.ok (Next.done v)
  | Outcome.panic e => Outcome.panic e
  | Outcome.undefined why => Outcome.undefined why
  | Outcome.diverge => Outcome.diverge

/-- `k` steps: the state after them, or what ended the run before. -/
def steps (prog : Program) : Nat → State → Outcome Next
  | 0, st => Outcome.ok (Next.state st)
  | k + 1, st => thenSteps (fun st' => steps prog k st') (Blaze.Mir.step prog st)

theorem steps_succ (prog : Program) (k : Nat) (st : State) :
    steps prog (k + 1) st = thenSteps (steps prog k) (Blaze.Mir.step prog st) := rfl

/-- One step is `step`. -/
theorem steps_one (prog : Program) (st : State) :
    steps prog (0 + 1) st = Blaze.Mir.step prog st := by
  rw [steps_succ]
  cases Blaze.Mir.step prog st with
  | ok next =>
    cases next with
    | state st' => rfl
    | done v => rfl
  | panic e => rfl
  | undefined why => rfl
  | diverge => rfl

/-- Two steps are the step after the first. -/
theorem steps_two (prog : Program) (st s1 : State)
    (h : Blaze.Mir.step prog st = Outcome.ok (Next.state s1)) :
    steps prog (1 + 1) st = Blaze.Mir.step prog s1 := by
  rw [steps_succ, h]
  exact steps_one prog s1

/-- A run of `k + n` steps is `k` steps, then a run of `n`. -/
theorem run_steps (prog : Program) :
    ∀ (k n : Nat) (st : State),
      Blaze.Mir.run prog (k + n) st = afterStep (Blaze.Mir.run prog n) (steps prog k st) := by
  intro k
  induction k with
  | zero =>
    intro n st
    have e : 0 + n = n := Nat.zero_add n
    rw [e]
    rfl
  | succ k ih =>
    intro n st
    have e : k + 1 + n = (k + n) + 1 := by omega
    rw [e, run_succ, steps_succ]
    cases hs : Blaze.Mir.step prog st with
    | ok next =>
      cases next with
      | state st' => exact ih n st'
      | done v => rfl
    | panic e => rfl
    | undefined why => rfl
    | diverge => rfl

theorem mapNext_id : ∀ (o : Outcome Next), mapNext (fun st => st) o = o
  | Outcome.ok (Next.state _) => rfl
  | Outcome.ok (Next.done _) => rfl
  | Outcome.panic _ => rfl
  | Outcome.undefined _ => rfl
  | Outcome.diverge => rfl

/-- A stuttering simulation of `src` by `tgt` through `f`, on the states `Inv` holds of: one
step of the target from `f st` is `f` of `k + 1` steps of the source from `st`, for a `k`
below the bound `K`, and the invariant holds of the state those steps reach. -/
structure Stutter (src tgt : Program) (f : State → State) (Inv : State → Prop) (K : Nat) :
    Prop where
  commutes : ∀ (st : State), Inv st →
    ∃ (k : Nat), k < K ∧
      Blaze.Mir.step tgt (f st) = mapNext f (steps src (k + 1) st) ∧
      ∀ (st' : State), steps src (k + 1) st = Outcome.ok (Next.state st') → Inv st'

/-- A target run of `n` steps that ends is the source's run of `K * n` steps. -/
theorem Stutter.run_target {src tgt : Program} {f : State → State} {Inv : State → Prop}
    {K : Nat} (h : Stutter src tgt f Inv K) :
    ∀ (n : Nat) (st : State), Inv st → Blaze.Mir.run tgt n (f st) ≠ Outcome.diverge →
      Blaze.Mir.run src (K * n) st = Blaze.Mir.run tgt n (f st) := by
  intro n
  induction n with
  | zero =>
    intro st _ hne
    exact absurd (run_zero tgt (f st)) hne
  | succ n ih =>
    intro st hinv hne
    have ⟨k, hk, hstep, hpres⟩ := h.commutes st hinv
    have e0 : K * (n + 1) = K * n + K := Nat.mul_succ K n
    have e : K * (n + 1) = (k + 1) + (K * n + (K - (k + 1))) := by
      rw [e0]
      omega
    rw [e, run_steps src (k + 1) (K * n + (K - (k + 1))) st, run_succ tgt n (f st), hstep]
    rw [run_succ tgt n (f st), hstep] at hne
    revert hne
    cases hs : steps src (k + 1) st with
    | ok next =>
      cases next with
      | state st' =>
        intro hne
        have hne' : Blaze.Mir.run tgt n (f st') ≠ Outcome.diverge := hne
        have base : Blaze.Mir.run src (K * n) st' = Blaze.Mir.run tgt n (f st') :=
          ih st' (hpres st' hs) hne'
        exact run_mono src (K * n) (K - (k + 1)) st' (Blaze.Mir.run tgt n (f st')) base hne'
      | done v =>
        intro _
        rfl
    | panic e =>
      intro _
      rfl
    | undefined why =>
      intro _
      rfl
    | diverge =>
      intro _
      rfl

/-- A source run of `m` steps that ends is the target's run of `m` steps: the target never
needs more fuel than the source. -/
theorem Stutter.run_source_le {src tgt : Program} {f : State → State} {Inv : State → Prop}
    {K : Nat} (h : Stutter src tgt f Inv K) :
    ∀ (n m : Nat), m ≤ n → ∀ (st : State), Inv st →
      Blaze.Mir.run src m st ≠ Outcome.diverge →
        Blaze.Mir.run tgt m (f st) = Blaze.Mir.run src m st := by
  intro n
  induction n with
  | zero =>
    intro m hm st _ hne
    have e : m = 0 := by omega
    rw [e] at hne
    exact absurd (run_zero src st) hne
  | succ n ih =>
    intro m hm st hinv hne
    cases m with
    | zero => exact absurd (run_zero src st) hne
    | succ m' =>
      have ⟨k, hk, hstep, hpres⟩ := h.commutes st hinv
      rw [run_succ tgt m' (f st), hstep]
      by_cases hlt : k + 1 ≤ m' + 1
      · -- The source has the fuel for its `k + 1` steps.
        have e : k + 1 + (m' - k) = m' + 1 := by omega
        have hsrc : Blaze.Mir.run src (m' + 1) st =
            afterStep (Blaze.Mir.run src (m' - k)) (steps src (k + 1) st) := by
          have r := run_steps src (k + 1) (m' - k) st
          rw [e] at r
          exact r
        rw [hsrc]
        rw [hsrc] at hne
        revert hne
        cases hs : steps src (k + 1) st with
        | ok next =>
          cases next with
          | state st' =>
            intro hne
            have hne' : Blaze.Mir.run src (m' - k) st' ≠ Outcome.diverge := hne
            have base : Blaze.Mir.run tgt (m' - k) (f st') = Blaze.Mir.run src (m' - k) st' :=
              ih (m' - k) (by omega) st' (hpres st' hs) hne'
            have grown : Blaze.Mir.run tgt (m' - k + k) (f st') =
                Blaze.Mir.run src (m' - k) st' :=
              run_mono tgt (m' - k) k (f st') (Blaze.Mir.run src (m' - k) st') base hne'
            have e2 : m' - k + k = m' := by omega
            rw [e2] at grown
            exact grown
          | done v =>
            intro _
            rfl
        | panic e =>
          intro _
          rfl
        | undefined why =>
          intro _
          rfl
        | diverge =>
          intro _
          rfl
      · -- The source ended before its `k + 1` steps: those steps are how it ended.
        have up : Blaze.Mir.run src (m' + 1 + (k - m')) st = Blaze.Mir.run src (m' + 1) st :=
          run_mono src (m' + 1) (k - m') st (Blaze.Mir.run src (m' + 1) st) rfl hne
        have e3 : k + 1 + 0 = m' + 1 + (k - m') := by omega
        have r0 : Blaze.Mir.run src (m' + 1) st =
            afterStep (Blaze.Mir.run src 0) (steps src (k + 1) st) := by
          have r := run_steps src (k + 1) 0 st
          rw [e3, up] at r
          exact r
        rw [r0]
        rw [r0] at hne
        revert hne
        cases hs : steps src (k + 1) st with
        | ok next =>
          cases next with
          | state st' =>
            intro hne
            exact absurd (run_zero src st') hne
          | done v =>
            intro _
            rfl
        | panic e =>
          intro _
          rfl
        | undefined why =>
          intro _
          rfl
        | diverge =>
          intro _
          rfl

/-- A source run of `n` steps that ends is the target's run of `n` steps. -/
theorem Stutter.run_source {src tgt : Program} {f : State → State} {Inv : State → Prop}
    {K : Nat} (h : Stutter src tgt f Inv K) (n : Nat) (st : State) (hinv : Inv st)
    (hne : Blaze.Mir.run src n st ≠ Outcome.diverge) :
    Blaze.Mir.run tgt n (f st) = Blaze.Mir.run src n st :=
  h.run_source_le n n (Nat.le_refl n) st hinv hne

/-- The same as a refinement up to fuel: with `n` steps the target is out of fuel, or it
ends as the source does with `K * n`. -/
theorem Stutter.refines {src tgt : Program} {f : State → State} {Inv : State → Prop}
    {K : Nat} (h : Stutter src tgt f Inv K) (n : Nat) (st : State) (hinv : Inv st) :
    Blaze.Mir.run tgt n (f st) = Outcome.diverge ∨
      (Blaze.Mir.run tgt n (f st)).Refines (Blaze.Mir.run src (K * n) st) := by
  by_cases hd : Blaze.Mir.run tgt n (f st) = Outcome.diverge
  · exact Or.inl hd
  · exact Or.inr (Outcome.Refines.of_eq (h.run_target n st hinv hd).symm)

end Blaze.Mir
