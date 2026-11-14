import Blaze.Result

/-!
A sans-IO core is a step function: from a state, asked something, it ends in a new state.
Its author proves three facts about a well-formedness predicate `wf` (it holds of a new state;
from a `wf` state an allowed ask succeeds and keeps `wf`; `wf` implies the property), and this
file turns them into: the property holds after every step of every allowed run, and no step of
such a run fails (docs/v2/plan/design/proofs-for-sans-io.md, "The idea").
-/

namespace Blaze.Machine

variable {σ ι β : Type}

/-- A step as a relation: `step s i s'` says that from `s`, asked `i`, the machine may end in
`s'`. Every shape of model (with fuel or without) gives one. -/
abbrev Step (σ ι : Type) := σ → ι → σ → Prop

/-- The step of a model without fuel: `s'` is reached when the function gives `ok s'`. -/
def ofFn (step : σ → ι → Result σ) : Step σ ι := fun s i s' => step s i = Result.ok s'

/-- The step of a model whose loops take fuel first: `s'` is reached when some fuel gives
`ok s'`. -/
def ofFuel (step : Nat → σ → ι → Result σ) : Step σ ι :=
  fun s i s' => ∃ fuel, step fuel s i = Result.ok s'

/-- The new state of a `&mut self` method, which gives back its return value and the new
state, dropping the return value. -/
def ofMethod (f : σ → ι → Result (β × σ)) : σ → ι → Result σ :=
  fun s i => f s i >>= fun r => Result.ok r.2

/-- `Run step s asks s'`: answering `asks` in order from `s`, the machine may end in `s'`. -/
inductive Run (step : Step σ ι) : σ → List ι → σ → Prop where
  /-- No ask leaves the state as it is. -/
  | nil {s : σ} : Run step s [] s
  /-- One step, then the rest of the asks from where it ended. -/
  | cons {s t u : σ} {i : ι} {is : List ι} : step s i t → Run step t is u → Run step s (i :: is) u

/-- A run of no asks ends where it started. -/
@[simp] theorem run_nil_iff {step : Step σ ι} {s u : σ} : Run step s [] u ↔ u = s :=
  ⟨fun h => by cases h; rfl, fun h => h ▸ Run.nil⟩

/-- A run of `i :: is` is one step on `i` followed by a run of `is`. -/
@[simp] theorem run_cons_iff {step : Step σ ι} {s u : σ} {i : ι} {is : List ι} :
    Run step s (i :: is) u ↔ ∃ t, step s i t ∧ Run step t is u :=
  ⟨fun h => by cases h with | cons ht hr => exact ⟨_, ht, hr⟩,
   fun ⟨_, ht, hr⟩ => Run.cons ht hr⟩

/-- `Allowed step allowed s asks`: every ask of `asks` is allowed in every state the run may
have reached when it is asked. -/
def Allowed (step : Step σ ι) (allowed : σ → ι → Prop) : σ → List ι → Prop
  | _, [] => True
  | s, i :: is => allowed s i ∧ ∀ t, step s i t → Allowed step allowed t is

/-- When every ask is allowed in every state, every list of asks is allowed. -/
theorem Allowed.of_forall {step : Step σ ι} {allowed : σ → ι → Prop}
    (h : ∀ s i, allowed s i) : ∀ (asks : List ι) (s : σ), Allowed step allowed s asks
  | [], _ => trivial
  | _ :: is, s => ⟨h s _, fun t _ => Allowed.of_forall h is t⟩

/-- The asks of an allowed list's prefix are allowed. -/
theorem Allowed.of_append {step : Step σ ι} {allowed : σ → ι → Prop} :
    ∀ {pre : List ι} {rest : List ι} {s : σ},
      Allowed step allowed s (pre ++ rest) → Allowed step allowed s pre
  | [], _, _, _ => trivial
  | _ :: _, _, _, h => ⟨h.1, fun t ht => Allowed.of_append (h.2 t ht)⟩

/-- The main theorem: if every allowed step from a `wf` state can be taken (progress) and every
step it may take keeps `wf` (preservation), an allowed run from a `wf` state reaches some state
and every state it reaches is `wf`. -/
theorem run_wf {step : Step σ ι} {allowed : σ → ι → Prop} {wf : σ → Prop}
    (progress : ∀ s i, wf s → allowed s i → ∃ s', step s i s')
    (preserve : ∀ s i s', wf s → allowed s i → step s i s' → wf s') :
    ∀ (asks : List ι) (s : σ), wf s → Allowed step allowed s asks →
      (∃ s', Run step s asks s') ∧ ∀ s', Run step s asks s' → wf s'
  | [], s, hs, _ => ⟨⟨s, Run.nil⟩, fun _ h => (run_nil_iff.1 h) ▸ hs⟩
  | i :: is, s, hs, ⟨ha, hrest⟩ => by
    obtain ⟨t, ht⟩ := progress s i hs ha
    obtain ⟨⟨u, hu⟩, _⟩ := run_wf progress preserve is t (preserve s i t hs ha ht) (hrest t ht)
    refine ⟨⟨u, Run.cons ht hu⟩, fun s' h => ?_⟩
    obtain ⟨t', ht', hr⟩ := run_cons_iff.1 h
    exact (run_wf progress preserve is t' (preserve s i t' hs ha ht') (hrest t' ht')).2 s' hr

/-- The corollary a user states: from every initial state, for every allowed list of asks and
every prefix of it, the run reaches some state and every state it reaches satisfies `P`. -/
theorem always {step : Step σ ι} {allowed : σ → ι → Prop} {init wf P : σ → Prop}
    (start : ∀ s, init s → wf s)
    (progress : ∀ s i, wf s → allowed s i → ∃ s', step s i s')
    (preserve : ∀ s i s', wf s → allowed s i → step s i s' → wf s')
    (prop : ∀ s, wf s → P s) :
    ∀ (s : σ) (asks pre : List ι), init s → Allowed step allowed s asks → pre <+: asks →
      (∃ s', Run step s pre s') ∧ ∀ s', Run step s pre s' → P s' := by
  intro s asks pre hi ha ⟨rest, hpre⟩
  subst hpre
  obtain ⟨hr, hw⟩ := run_wf progress preserve pre s (start s hi) (Allowed.of_append ha)
  exact ⟨hr, fun s' h => prop s' (hw s' h)⟩

/-- A model without fuel, run over a list of asks: each step's result feeds the next, and the
first failure or divergence is the run's. -/
def run (step : σ → ι → Result σ) : σ → List ι → Result σ
  | s, [] => Result.ok s
  | s, i :: is => step s i >>= fun t => run step t is

/-- For a function, the relational run reaches exactly the state its `run` gives. -/
theorem run_iff {step : σ → ι → Result σ} :
    ∀ {asks : List ι} {s s' : σ}, Run (ofFn step) s asks s' ↔ run step s asks = Result.ok s'
  | [], s, s' => by
    simp only [run_nil_iff, run]
    exact ⟨fun h => h ▸ rfl, fun h => by cases h; rfl⟩
  | i :: is, s, s' => by
    simp only [run_cons_iff, run, ofFn]
    cases h : step s i with
    | ok t => simpa using run_iff
    | fail e => simp [Result.bind_fail]
    | div => simp [Result.bind_div]

/-- The two obligations for a function in one: from a `wf` state an allowed ask gives `ok` of
a `wf` state; this is progress and preservation of `ofFn step`. -/
theorem ofFn_obligations {step : σ → ι → Result σ} {allowed : σ → ι → Prop} {wf : σ → Prop}
    (ok : ∀ s i, wf s → allowed s i → ∃ s', step s i = Result.ok s' ∧ wf s') :
    (∀ s i, wf s → allowed s i → ∃ s', ofFn step s i s') ∧
      (∀ s i s', wf s → allowed s i → ofFn step s i s' → wf s') :=
  ⟨fun s i hs ha => (ok s i hs ha).imp fun _ h => h.1,
   fun s i s' hs ha h => by
    obtain ⟨t, ht, hw⟩ := ok s i hs ha
    exact (Result.ok.inj (ht.symm.trans h)) ▸ hw⟩

/-- The main theorem for a function without fuel, as an equation: an allowed run from a `wf`
state gives `ok` of a `wf` state. -/
theorem run_ok {step : σ → ι → Result σ} {allowed : σ → ι → Prop} {wf : σ → Prop}
    (ok : ∀ s i, wf s → allowed s i → ∃ s', step s i = Result.ok s' ∧ wf s') :
    ∀ (asks : List ι) (s : σ), wf s → Allowed (ofFn step) allowed s asks →
      ∃ s', run step s asks = Result.ok s' ∧ wf s' := by
  intro asks s hs ha
  obtain ⟨progress, preserve⟩ := ofFn_obligations ok
  obtain ⟨⟨s', hr⟩, hw⟩ := run_wf progress preserve asks s hs ha
  exact ⟨s', run_iff.1 hr, hw s' hr⟩

/-- The corollary for a function without fuel: from every initial state, every prefix of an
allowed list of asks runs to `ok` of a state satisfying `P`. -/
theorem always_fn {step : σ → ι → Result σ} {allowed : σ → ι → Prop} {init wf P : σ → Prop}
    (start : ∀ s, init s → wf s)
    (ok : ∀ s i, wf s → allowed s i → ∃ s', step s i = Result.ok s' ∧ wf s')
    (prop : ∀ s, wf s → P s) :
    ∀ (s : σ) (asks pre : List ι), init s → Allowed (ofFn step) allowed s asks → pre <+: asks →
      ∃ s', run step s pre = Result.ok s' ∧ P s' := by
  intro s asks pre hi ha ⟨rest, hpre⟩
  subst hpre
  obtain ⟨s', hr, hw⟩ := run_ok ok pre s (start s hi) (Allowed.of_append ha)
  exact ⟨s', hr, prop s' hw⟩

end Blaze.Machine
