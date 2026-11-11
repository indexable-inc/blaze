import Blaze.Mir.Step

/-!
A run: steps until the entry function returns, a panic, undefined behaviour, or the fuel is
gone (`Outcome.diverge`). Recursive on the fuel, so total and evaluated by the kernel.

Proved here: a run that ends does so with any more fuel and the same answer (`run_mono`), so
two runs of one state that both end agree (`run_agree`).
-/

namespace Blaze.Mir

/-- What a run makes of one step's outcome; `rest` runs what is left. -/
def afterStep (rest : State → Outcome Value) : Outcome Next → Outcome Value
  | Outcome.ok (Next.state st) => rest st
  | Outcome.ok (Next.done v) => Outcome.ok v
  | Outcome.panic e => Outcome.panic e
  | Outcome.undefined why => Outcome.undefined why
  | Outcome.diverge => Outcome.diverge

def run (prog : Program) : Nat → State → Outcome Value
  | 0, _ => Outcome.diverge
  | fuel + 1, st => afterStep (fun st' => run prog fuel st') (step prog st)

theorem run_zero (prog : Program) (st : State) : run prog 0 st = Outcome.diverge := rfl

theorem run_succ (prog : Program) (fuel : Nat) (st : State) :
    run prog (fuel + 1) st = afterStep (run prog fuel) (step prog st) := rfl

/-- A step to a next state costs one unit of fuel. -/
theorem run_step (prog : Program) (fuel : Nat) (st st' : State)
    (h : step prog st = Outcome.ok (Next.state st')) :
    run prog (fuel + 1) st = run prog fuel st' := by
  rw [run_succ, h]
  rfl

/-- A step that returns from the entry function ends the run. -/
theorem run_done (prog : Program) (fuel : Nat) (st : State) (v : Value)
    (h : step prog st = Outcome.ok (Next.done v)) :
    run prog (fuel + 1) st = Outcome.ok v := by
  rw [run_succ, h]
  rfl

/-- Fuel monotonicity: a run that ends, ends the same with any more fuel. -/
theorem run_mono (prog : Program) :
    ∀ (n k : Nat) (st : State) (r : Outcome Value),
      run prog n st = r → r ≠ Outcome.diverge → run prog (n + k) st = r := by
  intro n
  induction n with
  | zero =>
    intro k st r h hr
    rw [run_zero] at h
    exact absurd h.symm hr
  | succ n ih =>
    intro k st r h hr
    have hk : n + 1 + k = (n + k) + 1 := by omega
    rw [hk, run_succ]
    rw [run_succ] at h
    cases hs : step prog st with
    | ok next =>
      rw [hs] at h
      cases next with
      | state st' => exact ih k st' r h hr
      | done v => exact h
    | panic e =>
      rw [hs] at h
      exact h
    | undefined why =>
      rw [hs] at h
      exact h
    | diverge =>
      rw [hs] at h
      exact h

/-- Two runs of one state that both end give the same answer, whatever their fuel. -/
theorem run_agree (prog : Program) (st : State) (n m : Nat)
    (hn : run prog n st ≠ Outcome.diverge) (hm : run prog m st ≠ Outcome.diverge) :
    run prog n st = run prog m st := by
  cases Nat.le_total n m with
  | inl h =>
    have e : n + (m - n) = m := by omega
    have grown := run_mono prog n (m - n) st (run prog n st) rfl hn
    rw [e] at grown
    exact grown.symm
  | inr h =>
    have e : m + (n - m) = n := by omega
    have grown := run_mono prog m (n - m) st (run prog m st) rfl hm
    rw [e] at grown
    exact grown

/-- The state a call of `id` starts in: `base` is what its reference arguments point to
(frame 0), `args` its arguments. -/
def Program.start (prog : Program) (id : FnId) (base : List Slot) (args : List Value) :
    Outcome State :=
  match prog.sig id with
  | none => Outcome.undefined Undefined.illFormed
  | some sig =>
    (State.enter { base := base, stack := [] } id sig args
        { frame := 0, slot := 0, path := [] } none).bind (fun next =>
      match next with
      | Next.state st => Outcome.ok st
      | Next.done _ => Outcome.undefined Undefined.illFormed)

/-- A call of function `id`, run with `fuel`. -/
def Program.call (prog : Program) (fuel : Nat) (id : FnId) (base : List Slot)
    (args : List Value) : Outcome Value :=
  (prog.start id base args).bind (run prog fuel)

end Blaze.Mir
