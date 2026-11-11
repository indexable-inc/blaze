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
  rw [dif_pos h]

/-- `s[i] = v`. -/
def update (s : Slice α) (i : Usize) (v : α) : Result (Slice α) :=
  if i.val.toNat < s.val.length then
    Result.ok ⟨s.val.set i.val.toNat v, by simpa using s.hlen⟩
  else Result.fail Error.bounds

end Slice

/-- `[T; n]`. -/
structure Arr (α : Type) (n : Nat) where
  val : _root_.List α
  hlen : val.length = n

namespace Arr

variable {α : Type} {n : Nat}

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
