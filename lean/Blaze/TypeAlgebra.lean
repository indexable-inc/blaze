/-!
Algebraic foundations of Blaze's canonical type sets and structural error/value unions
(`docs/v2/plan/design/error-sets.md`, ledger row `E3`).

Erased type keys are modeled as natural numbers (`TypeKey := Nat`). A `TypeSet` keeps its keys
in strictly ascending order (sorted and deduplicated), guaranteeing that:
- insertion order does not affect canonical representation (`typeset_canonical_order_independent`),
- duplicate or lifetime-erased colliding keys collapse idempotently (`typeset_insert_idempotent`),
- distinct keys occupy distinct slot indices (`typeset_disjoint_borrow_sound`),
- union widening `U ⊆ V` preserves membership and re-tag lookup (`union_widen_preserves_member`),
- union narrowing `U \ {m}` partitions every member of `U` completely (`union_narrow_complete`).
-/

namespace Blaze.TypeAlgebra

/-- Erased canonical type key. -/
abbrev TypeKey := Nat

/-- Insert a key into a sorted, deduplicated list of keys, preserving strict ascending order
    and dropping duplicates. -/
def insertKey (k : TypeKey) : List TypeKey → List TypeKey
  | [] => [k]
  | x :: xs =>
    if k < x then k :: x :: xs
    else if k = x then x :: xs
    else x :: insertKey k xs

/-- Remove all occurrences of key `m` from a list of keys. -/
def removeKey (m : TypeKey) : List TypeKey → List TypeKey
  | [] => []
  | x :: xs =>
    if x = m then removeKey m xs
    else x :: removeKey m xs

/-- 0-based canonical slot index of key `k` in a list of keys, or `none` if absent. -/
def indexOfKey (k : TypeKey) : List TypeKey → Option Nat
  | [] => none
  | x :: xs =>
    if k = x then some 0
    else (indexOfKey k xs).map (· + 1)

/-- Every key in `xs` is strictly greater than `x`. -/
def AllGt (x : TypeKey) : List TypeKey → Prop
  | [] => True
  | y :: ys => x < y ∧ AllGt x ys

/-- Strictly ascending (sorted and deduplicated) invariant on key lists. -/
def StrictlySorted : List TypeKey → Prop
  | [] => True
  | x :: xs => AllGt x xs ∧ StrictlySorted xs

private theorem allGt_mono {x y : TypeKey} (hxy : x < y) :
    ∀ {xs : List TypeKey}, AllGt y xs → AllGt x xs
  | [], _ => trivial
  | _ :: _, ⟨hyz, hrest⟩ => ⟨by omega, allGt_mono hxy hrest⟩

private theorem allGt_insertKey {x k : TypeKey} (hxk : x < k) :
    ∀ {xs : List TypeKey}, AllGt x xs → AllGt x (insertKey k xs)
  | [], _ => ⟨hxk, trivial⟩
  | y :: ys, ⟨hxy, hys⟩ => by
    simp only [insertKey]
    by_cases hky : k < y
    · rw [if_pos hky]
      exact ⟨hxk, hxy, hys⟩
    · rw [if_neg hky]
      by_cases hkeq : k = y
      · rw [if_pos hkeq]
        exact ⟨hxy, hys⟩
      · rw [if_neg hkeq]
        exact ⟨hxy, allGt_insertKey hxk hys⟩

private theorem strictlySorted_insertKey (k : TypeKey) :
    ∀ {xs : List TypeKey}, StrictlySorted xs → StrictlySorted (insertKey k xs)
  | [], _ => ⟨trivial, trivial⟩
  | y :: ys, ⟨hy, hys⟩ => by
    simp only [insertKey]
    by_cases hky : k < y
    · rw [if_pos hky]
      exact ⟨⟨hky, allGt_mono hky hy⟩, hy, hys⟩
    · rw [if_neg hky]
      by_cases hkeq : k = y
      · rw [if_pos hkeq]
        exact ⟨hy, hys⟩
      · rw [if_neg hkeq]
        have hyk : y < k := by omega
        exact ⟨allGt_insertKey hyk hy, strictlySorted_insertKey k hys⟩

private theorem allGt_removeKey {x m : TypeKey} :
    ∀ {xs : List TypeKey}, AllGt x xs → AllGt x (removeKey m xs)
  | [], _ => trivial
  | y :: ys, ⟨hxy, hys⟩ => by
    simp only [removeKey]
    by_cases hym : y = m
    · rw [if_pos hym]
      exact allGt_removeKey hys
    · rw [if_neg hym]
      exact ⟨hxy, allGt_removeKey hys⟩

private theorem strictlySorted_removeKey (m : TypeKey) :
    ∀ {xs : List TypeKey}, StrictlySorted xs → StrictlySorted (removeKey m xs)
  | [], _ => trivial
  | y :: ys, ⟨hy, hys⟩ => by
    simp only [removeKey]
    by_cases hym : y = m
    · rw [if_pos hym]
      exact strictlySorted_removeKey m hys
    · rw [if_neg hym]
      exact ⟨allGt_removeKey hy, strictlySorted_removeKey m hys⟩

private theorem insertKey_comm (a b : TypeKey) :
    ∀ (xs : List TypeKey), insertKey a (insertKey b xs) = insertKey b (insertKey a xs)
  | [] => by
    by_cases hlt : a < b
    · have h1 : ¬(b < a) := by omega
      have h2 : ¬(b = a) := by omega
      simp [insertKey, hlt, h1, h2]
    · by_cases heq : a = b
      · subst heq; rfl
      · have hgt : b < a := by omega
        have h1 : ¬(a < b) := by omega
        simp [insertKey, hgt, h1, heq]
  | x :: rest => by
    have ih := insertKey_comm a b rest
    by_cases ha1 : a < x
    · by_cases hb1 : b < x
      · by_cases hab1 : a < b
        · have hba : ¬(b < a) := by omega
          have hne : ¬(b = a) := by omega
          simp [insertKey, ha1, hb1, hab1, hba, hne]
        · by_cases hab2 : a = b
          · subst hab2; rfl
          · have hba : b < a := by omega
            simp [insertKey, ha1, hb1, hab1, hab2, hba]
      · by_cases hb2 : b = x
        · subst hb2
          have h1 : ¬(b < b) := by omega
          have h2 : ¬(b < a) := by omega
          have h3 : ¬(b = a) := by omega
          simp [insertKey, ha1, h1, h2, h3]
        · have h1 : ¬(b < a) := by omega
          have h2 : ¬(b = a) := by omega
          simp [insertKey, ha1, hb1, hb2, h1, h2]
    · by_cases ha2 : a = x
      · subst ha2
        by_cases hb1 : b < a
        · have h1 : ¬(a < a) := by omega
          have h2 : ¬(a < b) := by omega
          have h3 : ¬(a = b) := by omega
          simp [insertKey, hb1, h1, h2, h3]
        · by_cases hb2 : b = a
          · subst hb2; rfl
          · have h1 : ¬(a < a) := by omega
            simp [insertKey, hb1, hb2, h1]
      · by_cases hb1 : b < x
        · have h1 : ¬(a < b) := by omega
          have h2 : ¬(a = b) := fun h => ha1 (h ▸ hb1)
          simp [insertKey, ha1, ha2, hb1, h1, h2]
        · by_cases hb2 : b = x
          · subst hb2
            have h1 : ¬(b < b) := Nat.lt_irrefl b
            simp [insertKey, ha1, ha2, h1]
          · simp [insertKey, ha1, ha2, hb1, hb2, ih]

private theorem insertKey_idem (a : TypeKey) :
    ∀ (xs : List TypeKey), insertKey a (insertKey a xs) = insertKey a xs
  | [] => by
    simp [insertKey]
  | x :: rest => by
    have ih := insertKey_idem a rest
    by_cases ha1 : a < x
    · have h1 : ¬(a < a) := Nat.lt_irrefl a
      simp [insertKey, ha1, h1]
    · by_cases ha2 : a = x
      · subst ha2
        have h1 : ¬(a < a) := Nat.lt_irrefl a
        simp [insertKey, h1]
      · simp [insertKey, ha1, ha2, ih]

private theorem indexOfKey_injective (a b : TypeKey) (hne : a ≠ b) :
    ∀ (xs : List TypeKey) (i j : Nat),
      indexOfKey a xs = some i → indexOfKey b xs = some j → i ≠ j
  | [], _, _, ha, _ => by cases ha
  | x :: rest, i, j, ha, hb => by
    simp only [indexOfKey] at ha hb
    by_cases hax : a = x
    · have hbx : ¬(b = x) := fun h => hne (hax.trans h.symm)
      simp only [hax, ↓reduceIte] at ha
      simp only [hbx, ↓reduceIte] at hb
      cases ha
      cases hbr : indexOfKey b rest with
      | none =>
        rw [hbr] at hb
        cases hb
      | some j' =>
        rw [hbr] at hb
        dsimp only [Option.map] at hb
        cases hb
        intro h
        cases h
    · simp only [hax, ↓reduceIte] at ha
      by_cases hbx : b = x
      · simp only [hbx, ↓reduceIte] at hb
        cases hb
        cases har : indexOfKey a rest with
        | none =>
          rw [har] at ha
          cases ha
        | some i' =>
          rw [har] at ha
          dsimp only [Option.map] at ha
          cases ha
          intro h
          cases h
      · simp only [hbx, ↓reduceIte] at hb
        cases har : indexOfKey a rest with
        | none =>
          rw [har] at ha
          cases ha
        | some i' =>
          rw [har] at ha
          dsimp only [Option.map] at ha
          cases ha
          cases hbr : indexOfKey b rest with
          | none =>
            rw [hbr] at hb
            cases hb
          | some j' =>
            rw [hbr] at hb
            dsimp only [Option.map] at hb
            cases hb
            have hne' := indexOfKey_injective a b hne rest i' j' har hbr
            intro h
            exact hne' (Nat.succ.inj h)

private theorem mem_iff_indexOfKey_isSome (k : TypeKey) :
    ∀ (xs : List TypeKey), k ∈ xs ↔ ∃ i, indexOfKey k xs = some i
  | [] => by
    constructor
    · intro h; cases h
    · intro ⟨_, h⟩; cases h
  | x :: rest => by
    have ih := mem_iff_indexOfKey_isSome k rest
    constructor
    · intro hmem
      by_cases hkx : k = x
      · exact ⟨0, by simp only [indexOfKey]; rw [if_pos hkx]⟩
      · cases hmem with
        | head => exact (hkx rfl).elim
        | tail _ hrest =>
          obtain ⟨i, hi⟩ := ih.mp hrest
          exact ⟨i + 1, by simp only [indexOfKey]; rw [if_neg hkx, hi]; rfl⟩
    · intro ⟨i, hi⟩
      by_cases hkx : k = x
      · subst hkx
        exact List.Mem.head rest
      · simp only [indexOfKey] at hi
        rw [if_neg hkx] at hi
        cases hrest : indexOfKey k rest with
        | none =>
          rw [hrest] at hi
          cases hi
        | some j =>
          exact List.Mem.tail x (ih.mpr ⟨j, hrest⟩)

private theorem mem_or_removeKey (k m : TypeKey) :
    ∀ (xs : List TypeKey), k ∈ xs → k = m ∨ k ∈ removeKey m xs
  | [], h => by cases h
  | x :: rest, h => by
    cases h with
    | head =>
      by_cases hkm : k = m
      · exact Or.inl hkm
      · right
        simp only [removeKey]
        rw [if_neg hkm]
        exact List.Mem.head _
    | tail _ hrest =>
      cases mem_or_removeKey k m rest hrest with
      | inl hkm => exact Or.inl hkm
      | inr hrem =>
        right
        simp only [removeKey]
        by_cases hxm : x = m
        · rw [if_pos hxm]
          exact hrem
        · rw [if_neg hxm]
          exact List.Mem.tail x hrem

private theorem not_mem_removeKey (m : TypeKey) :
    ∀ (xs : List TypeKey), m ∉ removeKey m xs
  | [], h => by cases h
  | x :: rest, h => by
    simp only [removeKey] at h
    by_cases hxm : x = m
    · rw [if_pos hxm] at h
      exact not_mem_removeKey m rest h
    · rw [if_neg hxm] at h
      cases h with
      | head => exact hxm rfl
      | tail _ hrest => exact not_mem_removeKey m rest hrest

/-! ### Canonical `TypeSet` and `UnionTy` -/

/-- Canonical sorted, deduplicated set of erased type keys. -/
structure TypeSet where
  keys : List TypeKey
  sorted : StrictlySorted keys

theorem TypeSet.ext {s t : TypeSet} (h : s.keys = t.keys) : s = t := by
  cases s; cases t; cases h; rfl

/-- The empty type set. -/
def TypeSet.empty : TypeSet :=
  ⟨[], trivial⟩

/-- Insert a type key into a canonical `TypeSet`. -/
def TypeSet.insert (s : TypeSet) (k : TypeKey) : TypeSet :=
  ⟨insertKey k s.keys, strictlySorted_insertKey k s.sorted⟩

/-- Membership predicate on a canonical `TypeSet`. -/
def TypeSet.mem (s : TypeSet) (k : TypeKey) : Prop :=
  k ∈ s.keys

/-- Canonical 0-based slot index assigned to key `k` in `s`. -/
def TypeSet.slot (s : TypeSet) (k : TypeKey) : Option Nat :=
  indexOfKey k s.keys

/-- Structural union type over a canonical `TypeSet` of member keys. -/
structure UnionTy where
  members : TypeSet

/-- Subset widening relation `u ⊆ v`: every member of `u` is a member of `v`. -/
def UnionTy.Sub (u v : UnionTy) : Prop :=
  ∀ k, u.members.mem k → v.members.mem k

/-- Canonical discriminant tag for member `k` in union `u`. -/
def UnionTy.tagOf (u : UnionTy) (k : TypeKey) : Option Nat :=
  u.members.slot k

/-- Narrow union `u` by removing a matched member `m` (`U \ {m}`). -/
def UnionTy.narrow (u : UnionTy) (m : TypeKey) : UnionTy :=
  ⟨⟨removeKey m u.members.keys, strictlySorted_removeKey m u.members.sorted⟩⟩

/-! ### Main theorems -/

/-- Insertion order independence: inserting `a` then `b` yields the exact same canonical
    `TypeSet` as inserting `b` then `a`. -/
theorem typeset_canonical_order_independent (s : TypeSet) (a b : TypeKey) :
    (s.insert a).insert b = (s.insert b).insert a :=
  TypeSet.ext (insertKey_comm b a s.keys)

/-- Insertion idempotence: inserting the same erased type key twice (e.g., after lifetime
    erasure collapses two instantiations) produces the same `TypeSet`. -/
theorem typeset_insert_idempotent (s : TypeSet) (a : TypeKey) :
    (s.insert a).insert a = s.insert a :=
  TypeSet.ext (insertKey_idem a s.keys)

/-- Disjoint borrow soundness: distinct type keys in a canonical `TypeSet` always occupy
    distinct slot indices. -/
theorem typeset_disjoint_borrow_sound (s : TypeSet) (a b : TypeKey) (i j : Nat)
    (hne : a ≠ b) (ha : s.slot a = some i) (hb : s.slot b = some j) :
    i ≠ j :=
  indexOfKey_injective a b hne s.keys i j ha hb

/-- Widening soundness: widening `U ⊆ V` preserves membership and guarantees a valid
    re-tag discriminant lookup in `V`. -/
theorem union_widen_preserves_member (u v : UnionTy) (hsub : u.Sub v) (k : TypeKey) (tag : Nat)
    (hu : u.tagOf k = some tag) :
    v.members.mem k ∧ ∃ newTag, v.tagOf k = some newTag := by
  have hmemU : u.members.mem k := (mem_iff_indexOfKey_isSome k u.members.keys).mpr ⟨tag, hu⟩
  have hmemV : v.members.mem k := hsub k hmemU
  exact ⟨hmemV, (mem_iff_indexOfKey_isSome k v.members.keys).mp hmemV⟩

/-- Narrowing completeness: every member `k` of `U` is either the matched member `m` or
    remains in `U \ {m}`, and `m` itself is absent from `U \ {m}`. -/
theorem union_narrow_complete (u : UnionTy) (m k : TypeKey) (hk : u.members.mem k) :
    (k = m ∨ (u.narrow m).members.mem k) ∧ ¬ (u.narrow m).members.mem m :=
  ⟨mem_or_removeKey k m u.members.keys hk, not_mem_removeKey m u.members.keys⟩

end Blaze.TypeAlgebra
