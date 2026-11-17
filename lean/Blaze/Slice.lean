import Blaze.Scalar

/-!
Slices and arrays: a list with what Rust knows of its length. A slice has at most
`usize::MAX` elements, so its length is a `Usize`; an index out of bounds fails.
-/

namespace Blaze

namespace List

theorem drop_of_length_le {α : Type} : ∀ (l : List α) (n : Nat), l.length ≤ n → l.drop n = []
  | [], 0, _ => rfl
  | [], _ + 1, _ => rfl
  | _ :: _, 0, h => absurd h (Nat.not_succ_le_zero _)
  | _ :: l, n + 1, h => drop_of_length_le l n (Nat.le_of_succ_le_succ h)

/-- What is left from `n` on starts with the element at `n`. -/
theorem drop_eq_get_cons {α : Type} :
    ∀ (l : List α) (n : Nat) (h : n < l.length), l.drop n = l.get ⟨n, h⟩ :: l.drop (n + 1)
  | [], _, h => absurd h (Nat.not_lt_zero _)
  | _ :: _, 0, _ => rfl
  | _ :: l, n + 1, h => drop_eq_get_cons l n (Nat.lt_of_succ_lt_succ h)

end List

/-- `[T]`, behind any reference. -/
structure Slice (α : Type) where
  val : _root_.List α
  hlen : val.length ≤ 18446744073709551615

namespace Slice

variable {α : Type}

/-- Two slices are equal when their elements are. -/
theorem ext : ∀ {a b : Slice α}, a.val = b.val → a = b
  | ⟨_, _⟩, ⟨_, _⟩, rfl => rfl

/-- `==` of slices, `Vec`s and what holds them: element by element. -/
instance [DecidableEq α] : DecidableEq (Slice α) := fun a b =>
  if h : a.val = b.val then isTrue (ext h) else isFalse (fun e => h (congrArg Slice.val e))

/-- A list short enough to be a slice. -/
def ofList? (l : _root_.List α) : Option (Slice α) :=
  if h : l.length ≤ 18446744073709551615 then some ⟨l, h⟩ else none

def len (s : Slice α) : Usize :=
  ⟨(s.val.length : Int),
    by simp only [ScalarTy.min] <;> omega,
    by have h := s.hlen; simp only [ScalarTy.max] <;> omega⟩

theorem len_val (s : Slice α) : (len s).val = (s.val.length : Int) := rfl

/-- `s[i]`. -/
def index (s : Slice α) (i : Usize) : Result α :=
  if h : i.val.toNat < s.val.length then Result.ok (s.val.get ⟨i.val.toNat, h⟩)
  else Result.fail Error.bounds

theorem index_ok (s : Slice α) (i : Usize) (h : i.val.toNat < s.val.length) :
    index s i = Result.ok (s.val.get ⟨i.val.toNat, h⟩) := by
  unfold index
  rw [dite_eq_left h]

/-- `s[i] = v`. -/
def update (s : Slice α) (i : Usize) (v : α) : Result (Slice α) :=
  if i.val.toNat < s.val.length then
    Result.ok ⟨s.val.set i.val.toNat v, by simpa using s.hlen⟩
  else Result.fail Error.bounds

/-- The elements of `s` from `start` up to `stop`: those at `start`, `start + 1`, .., `stop - 1`
that it has. -/
def cut (s : Slice α) (start stop : Nat) : Slice α :=
  ⟨(s.val.take stop).drop start, by
    have h := s.hlen
    simp only [List.length_drop, List.length_take]
    omega⟩

/-- Up to its length, a cut has `stop - start` elements. -/
theorem cut_length (s : Slice α) (start stop : Nat) (h : stop ≤ s.val.length) :
    (cut s start stop).val.length = stop - start := by
  simp only [cut, List.length_drop, List.length_take]
  omega

/-- The element at `i` of a cut is the element at `start + i` of the slice, below `stop`. -/
theorem cut_getElem? (s : Slice α) (start stop i : Nat) (h : start + i < stop) :
    (cut s start stop).val[i]? = s.val[start + i]? := by
  simp only [cut, List.getElem?_drop, List.getElem?_take]
  simp [h]

/-- `&s[start..end]` (library/core/src/slice/index.rs:405-413, `SliceIndex<[T]> for
ops::Range<usize>`'s `index`: `if let Some(new_len) = usize::checked_sub(self.end, self.start) &&
self.end <= slice.len()` gives the elements from `start` up to `end`, else
`slice_index_fail(self.start, self.end, slice.len())` panics): an index out of bounds unless
`start <= end <= len`. -/
def subslice (s : Slice α) (start stop : Usize) : Result (Slice α) :=
  if start.val ≤ stop.val ∧ stop.val.toNat ≤ s.val.length then
    Result.ok (cut s start.val.toNat stop.val.toNat)
  else Result.fail Error.bounds

/-- In bounds, a subslice is the cut. -/
@[simp] theorem subslice_ok (s : Slice α) (start stop : Usize)
    (h : start.val ≤ stop.val ∧ stop.val.toNat ≤ s.val.length) :
    subslice s start stop = Result.ok (cut s start.val.toNat stop.val.toNat) := by
  unfold subslice
  rw [ite_eq_left h]

/-- Out of bounds, a subslice panics. -/
@[simp] theorem subslice_fail (s : Slice α) (start stop : Usize)
    (h : ¬ (start.val ≤ stop.val ∧ stop.val.toNat ≤ s.val.length)) :
    subslice s start stop = Result.fail Error.bounds := by
  unfold subslice
  rw [ite_eq_right h]

/-- `s.get(start..end)` (index.rs:335-345, `SliceIndex<[T]> for ops::Range<usize>`'s `get`, by the
same test as `index`): `Some` of the subslice in bounds, else `None`. -/
def get_subslice (s : Slice α) (start stop : Usize) : Option (Slice α) :=
  if start.val ≤ stop.val ∧ stop.val.toNat ≤ s.val.length then
    some (cut s start.val.toNat stop.val.toNat)
  else none

end Slice

/-- `[T; n]`. -/
structure Arr (α : Type) (n : Nat) where
  val : _root_.List α
  hlen : val.length = n

namespace Arr

variable {α : Type} {n : Nat}

/-- Two arrays are equal when their elements are. -/
theorem ext : ∀ {a b : Arr α n}, a.val = b.val → a = b
  | ⟨_, _⟩, ⟨_, _⟩, rfl => rfl

/-- `==` of arrays: element by element. -/
instance [DecidableEq α] : DecidableEq (Arr α n) := fun a b =>
  if h : a.val = b.val then isTrue (ext h) else isFalse (fun e => h (congrArg Arr.val e))

def ofList (l : _root_.List α) : Arr α l.length := ⟨l, rfl⟩

def replicate (n : Nat) (v : α) : Arr α n := ⟨_root_.List.replicate n v, by simp⟩

def index (a : Arr α n) (i : Usize) : Result α :=
  if h : i.val.toNat < a.val.length then Result.ok (a.val.get ⟨i.val.toNat, h⟩)
  else Result.fail Error.bounds

def update (a : Arr α n) (i : Usize) (v : α) : Result (Arr α n) :=
  if i.val.toNat < a.val.length then
    Result.ok ⟨a.val.set i.val.toNat v, by simpa using a.hlen⟩
  else Result.fail Error.bounds

end Arr

end Blaze
