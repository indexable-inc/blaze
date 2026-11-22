/-!
Capability monotonicity and bounded-channel invariants for `blaze-std` (`bstd`, ledger row `S1`):
- `bstd::fs::Dir<P, Scope>` (`Scope := Subtree | Flat | Leaf`): narrowing a directory capability
  never grants access to paths previously disallowed, and `Flat` rejects nested paths.
- `bstd::sync::Chan`: bounded FIFO channel push/pop preserve the fixed capacity bound and
  deliver messages in strict FIFO order.
-/

namespace BlazeStd.Capability

/-! ### Directory capability scopes (`bstd::fs::Dir<P, Scope>`) -/

/-- Access permission parameter `P` of `bstd::fs::Dir<P, Scope>`. -/
inductive Perm where
  | ReadOnly
  | ReadWrite
  deriving DecidableEq, Repr

/-- Path traversal scope parameter of `bstd::fs::Dir<P, Scope>`:
    - `Subtree`: any non-empty relative path beneath the directory root,
    - `Flat`: direct child entries only (no nested subdirectories),
    - `Leaf`: narrowed to a leaf handle permitting no further relative path lookups. -/
inductive Scope where
  | Subtree
  | Flat
  | Leaf
  deriving DecidableEq, Repr

/-- A relative path beneath a `Dir` capability, represented by its path segments. -/
structure RelPath where
  segments : List String
  deriving DecidableEq, Repr

/-- Capability handle modeling `bstd::fs::Dir<P, Scope>`. -/
structure Dir where
  perm : Perm
  scope : Scope
  deriving DecidableEq, Repr

/-- Whether a relative path enters a nested subdirectory (2 or more path segments). -/
def nested (p : RelPath) : Bool :=
  decide (1 < p.segments.length)

/-- Path access predicate: whether a capability of scope `s` permits accessing relative path `p`. -/
def allows : Scope → RelPath → Bool
  | .Subtree, p => decide (1 ≤ p.segments.length)
  | .Flat, p => decide (p.segments.length = 1)
  | .Leaf, _ => false

/-- Scope narrowing relation: `Narrowed s₂ s₁` holds when every path allowed under `s₂`
    is also allowed under `s₁` (narrowing never widens authority). -/
def Narrowed (s₂ s₁ : Scope) : Prop :=
  ∀ p : RelPath, allows s₂ p = true → allows s₁ p = true

/-- Narrowing `Subtree` to `Flat` is monotone: every path allowed by `Flat` is allowed by `Subtree`. -/
theorem dir_narrow_subtree_to_flat_monotone (p : RelPath)
    (h : allows Scope.Flat p = true) :
    allows Scope.Subtree p = true := by
  simp only [allows, decide_eq_true_eq] at h ⊢
  omega

/-- A `Flat`-scoped directory rejects every nested path. -/
theorem dir_flat_rejects_nested (p : RelPath)
    (h : nested p = true) :
    allows Scope.Flat p = false := by
  simp only [nested, allows, decide_eq_true_eq, decide_eq_false_iff_not] at h ⊢
  omega

/-- Capability narrowing composes transitively: narrowing `s₁` to `s₂` and `s₂` to `s₃`
    yields a valid narrowing from `s₁` to `s₃`. -/
theorem dir_narrow_transitive (s₁ s₂ s₃ : Scope)
    (h12 : Narrowed s₂ s₁) (h23 : Narrowed s₃ s₂) :
    Narrowed s₃ s₁ :=
  fun p hp => h12 p (h23 p hp)

/-! ### Bounded FIFO channel (`bstd::sync::Chan`) -/

/-- Sans-IO model of a bounded FIFO channel (`bstd::sync::Chan`) with fixed capacity `cap`. -/
structure BoundedChan (α : Type) where
  cap : Nat
  buf : List α

/-- An empty bounded channel with capacity `cap`. -/
def BoundedChan.empty {α : Type} (cap : Nat) : BoundedChan α :=
  { cap := cap, buf := [] }

/-- Capacity invariant: the number of queued elements never exceeds `cap`. -/
def BoundedChan.withinBound {α : Type} (c : BoundedChan α) : Prop :=
  c.buf.length ≤ c.cap

/-- Non-blocking enqueue (`try_send`): appends `v` at the back if strictly below capacity. -/
def BoundedChan.push {α : Type} (c : BoundedChan α) (v : α) : Option (BoundedChan α) :=
  if c.buf.length < c.cap then
    some { cap := c.cap, buf := c.buf ++ [v] }
  else
    none

/-- Non-blocking dequeue (`try_recv`): removes and returns the oldest element from the front. -/
def BoundedChan.pop {α : Type} (c : BoundedChan α) : Option (α × BoundedChan α) :=
  match c.buf with
  | [] => none
  | v :: rest => some (v, { cap := c.cap, buf := rest })

/-- A successful push preserves the bounded channel's capacity invariant. -/
theorem chan_push_preserves_bound {α : Type} (c c' : BoundedChan α) (v : α)
    (hpush : c.push v = some c') :
    c'.withinBound := by
  unfold BoundedChan.push at hpush
  by_cases hlt : c.buf.length < c.cap
  · simp only [hlt, ↓reduceIte] at hpush
    cases hpush
    unfold BoundedChan.withinBound
    simp only [List.length_append, List.length_singleton]
    omega
  · simp only [hlt, ↓reduceIte] at hpush
    cases hpush

/-- A successful pop preserves the bounded channel's capacity invariant. -/
theorem chan_pop_preserves_bound {α : Type} (c c' : BoundedChan α) (v : α)
    (hbound : c.withinBound) (hpop : c.pop = some (v, c')) :
    c'.withinBound := by
  unfold BoundedChan.pop at hpop
  cases hbuf : c.buf with
  | nil =>
    rw [hbuf] at hpop
    cases hpop
  | cons x rest =>
    rw [hbuf] at hpop
    cases hpop
    unfold BoundedChan.withinBound at hbound ⊢
    rw [hbuf] at hbound
    simp only [List.length_cons] at hbound
    dsimp only
    exact Nat.le_of_succ_le hbound

/-- Strict FIFO delivery: pushing `a` then `b` into an empty channel of capacity ≥ 2 pops `a`
    first, then `b`, returning the channel to empty. -/
theorem chan_fifo_order {α : Type} (cap : Nat) (hcap : 2 ≤ cap) (a b : α) :
    ∃ c1 c2 c3,
      (BoundedChan.empty cap).push a = some c1 ∧
      c1.push b = some c2 ∧
      c2.pop = some (a, c3) ∧
      c3.pop = some (b, BoundedChan.empty cap) := by
  refine ⟨{ cap := cap, buf := [a] }, { cap := cap, buf := [a, b] }, { cap := cap, buf := [b] },
    ?_, ?_, rfl, rfl⟩
  · unfold BoundedChan.empty BoundedChan.push
    have h0 : 0 < cap := by omega
    simp only [List.length_nil, List.nil_append, h0, ↓reduceIte]
  · unfold BoundedChan.push
    have h1 : 1 < cap := by omega
    simp only [List.length_singleton, List.cons_append, List.nil_append, h1, ↓reduceIte]

end BlazeStd.Capability
