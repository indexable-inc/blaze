import Blaze.Mir.Outcome

/-!
What the proofs about `Outcome.bind` chains rest on (the semantics in `Step.lean` is such
chains): congruence of `bind`, `bind` commuting with a map of its result, and a predicate
that holds of the value an outcome has (`Outcome.All`).

Each lemma is a case split on the outcome and nothing else. They are applied as terms, so
what Lean checks at each use is a definitional unfolding of the function at hand to its
`bind`, never a rewrite.
-/

namespace Blaze.Mir

theorem Outcome.map_ok {α β : Type} (t : α → β) (a : α) :
    Outcome.map t (Outcome.ok a) = Outcome.ok (t a) := rfl

/-- Two binds of one outcome agree when their continuations agree on its value. -/
theorem Outcome.bind_congr_same {α β : Type} {x : Outcome α} {f g : α → Outcome β}
    (h : ∀ (a : α), x = Outcome.ok a → f a = g a) : x.bind f = x.bind g := by
  cases x with
  | ok a => exact h a rfl
  | panic e => rfl
  | undefined why => rfl
  | diverge => rfl

theorem Outcome.bind_congr {α β : Type} {x x' : Outcome α} {f g : α → Outcome β}
    (hx : x' = x) (h : ∀ (a : α), x = Outcome.ok a → f a = g a) : x'.bind f = x.bind g :=
  (congrArg (fun y => Outcome.bind y f) hx).trans (Outcome.bind_congr_same h)

/-- A bind commutes with a map of its result when its continuation does. -/
theorem Outcome.bind_commutes_same {α β γ : Type} {t : β → γ} {x : Outcome α}
    {f : α → Outcome γ} {g : α → Outcome β}
    (h : ∀ (a : α), x = Outcome.ok a → f a = Outcome.map t (g a)) :
    x.bind f = Outcome.map t (x.bind g) := by
  cases x with
  | ok a => exact h a rfl
  | panic e => rfl
  | undefined why => rfl
  | diverge => rfl

theorem Outcome.bind_commutes {α β γ : Type} {t : β → γ} {x x' : Outcome α}
    {f : α → Outcome γ} {g : α → Outcome β} (hx : x' = x)
    (h : ∀ (a : α), x = Outcome.ok a → f a = Outcome.map t (g a)) :
    x'.bind f = Outcome.map t (x.bind g) :=
  (congrArg (fun y => Outcome.bind y f) hx).trans (Outcome.bind_commutes_same h)

/-- The same when the bound outcome is itself a map of the other's. -/
theorem Outcome.bind_commutes_map_same {α α' β γ : Type} {s : α → α'} {t : β → γ}
    {x : Outcome α} {f : α' → Outcome γ} {g : α → Outcome β}
    (h : ∀ (a : α), x = Outcome.ok a → f (s a) = Outcome.map t (g a)) :
    (Outcome.map s x).bind f = Outcome.map t (x.bind g) := by
  cases x with
  | ok a => exact h a rfl
  | panic e => rfl
  | undefined why => rfl
  | diverge => rfl

theorem Outcome.bind_commutes_map {α α' β γ : Type} {s : α → α'} {t : β → γ}
    {x : Outcome α} {x' : Outcome α'} {f : α' → Outcome γ} {g : α → Outcome β}
    (hx : x' = Outcome.map s x)
    (h : ∀ (a : α), x = Outcome.ok a → f (s a) = Outcome.map t (g a)) :
    x'.bind f = Outcome.map t (x.bind g) :=
  (congrArg (fun y => Outcome.bind y f) hx).trans (Outcome.bind_commutes_map_same h)

/-- A bind of a mapped outcome is the bind of the outcome, when the continuations agree
through the map. -/
theorem Outcome.bind_map_congr_same {α α' β : Type} {s : α → α'} {x : Outcome α}
    {f : α' → Outcome β} {g : α → Outcome β}
    (h : ∀ (a : α), x = Outcome.ok a → f (s a) = g a) :
    (Outcome.map s x).bind f = x.bind g := by
  cases x with
  | ok a => exact h a rfl
  | panic e => rfl
  | undefined why => rfl
  | diverge => rfl

theorem Outcome.bind_map_congr {α α' β : Type} {s : α → α'} {x : Outcome α}
    {x' : Outcome α'} {f : α' → Outcome β} {g : α → Outcome β} (hx : x' = Outcome.map s x)
    (h : ∀ (a : α), x = Outcome.ok a → f (s a) = g a) : x'.bind f = x.bind g :=
  (congrArg (fun y => Outcome.bind y f) hx).trans (Outcome.bind_map_congr_same h)

/-! Equations between different constructors, as eliminations. -/

theorem none_eq_some_elim {α : Type} {x : α} {C : Prop} (h : (none : Option α) = some x) : C := by
  cases h

theorem false_eq_true_elim {C : Prop} (h : false = true) : C := by
  cases h

theorem true_eq_false_elim {C : Prop} (h : true = false) : C := by
  cases h

theorem and_true_left {a b : Bool} (h : (a && b) = true) : a = true := by
  cases a with
  | true => rfl
  | false => exact h

theorem and_true_right {a b : Bool} (h : (a && b) = true) : b = true := by
  cases a with
  | true => exact h
  | false => exact false_eq_true_elim h

/-- `P` holds of the value `x` has, when it has one. -/
def Outcome.All {α : Type} (P : α → Prop) (x : Outcome α) : Prop :=
  ∀ (a : α), x = Outcome.ok a → P a

theorem Outcome.all_ok {α : Type} {P : α → Prop} {a : α} (h : P a) :
    Outcome.All P (Outcome.ok a) := by
  intro b hb
  cases hb
  exact h

theorem Outcome.all_panic {α : Type} (P : α → Prop) (e : Error) :
    Outcome.All P (Outcome.panic e : Outcome α) := by
  intro b hb
  cases hb

theorem Outcome.all_undefined {α : Type} (P : α → Prop) (why : Undefined) :
    Outcome.All P (Outcome.undefined why : Outcome α) := by
  intro b hb
  cases hb

theorem Outcome.all_diverge {α : Type} (P : α → Prop) :
    Outcome.All P (Outcome.diverge : Outcome α) := by
  intro b hb
  cases hb

theorem Outcome.all_true {α : Type} (x : Outcome α) : Outcome.All (fun _ => True) x :=
  fun _ _ => True.intro

theorem Outcome.All.imp {α : Type} {P Q : α → Prop} {x : Outcome α}
    (hq : Outcome.All Q x) (h : ∀ (a : α), Q a → P a) : Outcome.All P x :=
  fun a ha => h a (hq a ha)

/-- `P` holds of what a bind gives when `Q` holds of what is bound and the continuation
takes `Q` to `P`. -/
theorem Outcome.all_bind {α β : Type} {Q : α → Prop} {P : β → Prop} {x : Outcome α}
    {f : α → Outcome β} (hx : Outcome.All Q x) (hf : ∀ (a : α), Q a → Outcome.All P (f a)) :
    Outcome.All P (x.bind f) := by
  intro b hb
  have ⟨a, ha, hfa⟩ := Outcome.bind_eq_ok x f b hb
  exact hf a (hx a ha) b hfa

end Blaze.Mir
