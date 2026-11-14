import Blaze.Slice

/-!
`alloc::vec::Vec<T>` in the global allocator, and the operations on it the models call. A `Vec`
is a slice's value: its elements, at most `usize::MAX` of them. Its capacity and its allocator
are not observable in the subset blaze translates, so neither is kept, and a `Vec` deref'd to a
slice is itself. A `Vec` in an allocator other than `Global` is not translated: a bounded
allocator can refuse a push.

Trusted: every allocation succeeds. A `Vec` that grows asks the allocator for memory, and Rust
panics or aborts when it cannot have it (the allocator is out of memory, or the capacity asked
for passes `isize::MAX` bytes); the models assume it always can, as every verifier of Rust
does. This assumption is part of the trusted base of every proof about a model that uses a
`Vec`.

What panics in Rust fails here, and only that: `push`, `insert` and `extend_from_slice` past
`usize::MAX` elements and `insert`, `remove` and `swap_remove` out of bounds (`Error.panic`:
their own checks panic), and `swap` out of bounds (`Error.bounds`, as a slice index).
-/

namespace Blaze

/-- `Vec<T>` in the global allocator: a slice's value. -/
abbrev Vec (α : Type) := Slice α

namespace Slice

variable {α : Type}

/-- `s.is_empty()`: whether it has no elements. -/
@[simp] def is_empty (s : Slice α) : Bool := s.val.isEmpty

/-- `s.get(i)`: the element at `i`, `None` out of bounds. -/
@[simp] def get (s : Slice α) (i : Usize) : Option α := s.val[i.val.toNat]?

/-- `s.first()`: the first element, `None` when there is none. -/
@[simp] def first (s : Slice α) : Option α := s.val.head?

/-- `s.last()`: the last element, `None` when there is none. -/
@[simp] def last (s : Slice α) : Option α := s.val.getLast?

/-- `s.swap(i, j)`: the elements at `i` and `j` exchanged; out of bounds it panics. -/
def swap (s : Slice α) (i j : Usize) : Result (Slice α) :=
  if h : i.val.toNat < s.val.length ∧ j.val.toNat < s.val.length then
    Result.ok ⟨(s.val.set i.val.toNat (s.val[j.val.toNat]'h.2)).set j.val.toNat
      (s.val[i.val.toNat]'h.1), by simpa using s.hlen⟩
  else Result.fail Error.bounds

/-- In bounds, `swap` gives the elements exchanged. -/
@[simp] theorem swap_ok (s : Slice α) (i j : Usize)
    (h : i.val.toNat < s.val.length ∧ j.val.toNat < s.val.length) :
    swap s i j = Result.ok ⟨(s.val.set i.val.toNat (s.val[j.val.toNat]'h.2)).set j.val.toNat
      (s.val[i.val.toNat]'h.1), by simpa using s.hlen⟩ := by
  unfold swap
  rw [dite_eq_left h]

/-- Out of bounds, `swap` panics. -/
@[simp] theorem swap_fail (s : Slice α) (i j : Usize)
    (h : ¬ (i.val.toNat < s.val.length ∧ j.val.toNat < s.val.length)) :
    swap s i j = Result.fail Error.bounds := by
  unfold swap
  rw [dite_eq_right h]

end Slice

namespace Vec

variable {α : Type}

/-- `Vec::new()`, `Vec::with_capacity(n)`, `vec![]`: no elements. -/
def new : Vec α := ⟨[], Nat.zero_le _⟩

/-- A new `Vec` has no elements. -/
@[simp, grind =] theorem new_val : (new : Vec α).val = [] := rfl

/-- `vec![x; n]`: `n` copies of `x`. -/
def from_elem (x : α) (n : Usize) : Vec α :=
  ⟨List.replicate n.val.toNat x, by
    have h := n.hmax
    simp only [List.length_replicate, ScalarTy.max] at *
    omega⟩

/-- `vec![x; n]` is `n` copies of `x`. -/
@[simp, grind =] theorem from_elem_val (x : α) (n : Usize) :
    (from_elem x n).val = List.replicate n.val.toNat x := rfl

/-- `v.push(x)`: `x` after the elements; it panics when there are `usize::MAX` already. -/
def push (v : Vec α) (x : α) : Result (Vec α) :=
  if h : v.val.length < 18446744073709551615 then
    Result.ok ⟨v.val ++ [x], by simp only [List.length_append, List.length_singleton]; omega⟩
  else Result.fail Error.panic

/-- Below `usize::MAX` elements, `push` puts `x` after them. -/
@[simp] theorem push_ok (v : Vec α) (x : α) (h : v.val.length < 18446744073709551615) :
    push v x = Result.ok ⟨v.val ++ [x],
      by simp only [List.length_append, List.length_singleton]; omega⟩ := by
  unfold push
  rw [dite_eq_left h]

/-- At `usize::MAX` elements, `push` panics. -/
@[simp] theorem push_fail (v : Vec α) (x : α) (h : ¬ v.val.length < 18446744073709551615) :
    push v x = Result.fail Error.panic := by
  unfold push
  rw [dite_eq_right h]

/-- `v.pop()`: the last element (`None` when there is none), and the elements before it. -/
def pop (v : Vec α) : Option α × Vec α :=
  (v.val.getLast?, ⟨v.val.dropLast, by have h := v.hlen; simp only [List.length_dropLast]; omega⟩)

/-- What `pop` gives is the last element. -/
@[simp, grind =] theorem pop_fst (v : Vec α) : (pop v).1 = v.val.getLast? := rfl

/-- What `pop` leaves is the elements before the last. -/
@[simp, grind =] theorem pop_snd_val (v : Vec α) : (pop v).2.val = v.val.dropLast := rfl

/-- `v.clear()`: no elements. -/
def clear (_v : Vec α) : Vec α := new

/-- What `clear` leaves has no elements. -/
@[simp, grind =] theorem clear_val (v : Vec α) : (clear v).val = [] := rfl

/-- `v.truncate(n)`: the first `n` elements, all of them when there are fewer. -/
def truncate (v : Vec α) (n : Usize) : Vec α :=
  ⟨v.val.take n.val.toNat, by have h := v.hlen; simp only [List.length_take]; omega⟩

/-- What `truncate` leaves is the first `n` elements. -/
@[simp, grind =] theorem truncate_val (v : Vec α) (n : Usize) :
    (truncate v n).val = v.val.take n.val.toNat := rfl

/-- `v.insert(i, x)`: `x` at `i`, the elements from `i` on after it; it panics when `i` is past
the length or there are `usize::MAX` elements already. -/
def insert (v : Vec α) (i : Usize) (x : α) : Result (Vec α) :=
  if h : i.val.toNat ≤ v.val.length ∧ v.val.length < 18446744073709551615 then
    Result.ok ⟨v.val.insertIdx i.val.toNat x, by
      rw [List.length_insertIdx_of_le_length h.1]; omega⟩
  else Result.fail Error.panic

/-- At or before the length, below `usize::MAX` elements, `insert` puts `x` at `i`. -/
@[simp] theorem insert_ok (v : Vec α) (i : Usize) (x : α)
    (h : i.val.toNat ≤ v.val.length ∧ v.val.length < 18446744073709551615) :
    insert v i x = Result.ok ⟨v.val.insertIdx i.val.toNat x, by
      rw [List.length_insertIdx_of_le_length h.1]; omega⟩ := by
  unfold insert
  rw [dite_eq_left h]

/-- Past the length, or at `usize::MAX` elements, `insert` panics. -/
@[simp] theorem insert_fail (v : Vec α) (i : Usize) (x : α)
    (h : ¬ (i.val.toNat ≤ v.val.length ∧ v.val.length < 18446744073709551615)) :
    insert v i x = Result.fail Error.panic := by
  unfold insert
  rw [dite_eq_right h]

/-- `v.remove(i)`: the element at `i`, and the others in order; it panics out of bounds. -/
def remove (v : Vec α) (i : Usize) : Result (α × Vec α) :=
  if h : i.val.toNat < v.val.length then
    Result.ok (v.val[i.val.toNat], ⟨v.val.eraseIdx i.val.toNat, by
      have h' := v.hlen; rw [List.length_eraseIdx_of_lt h]; omega⟩)
  else Result.fail Error.panic

/-- In bounds, `remove` takes the element at `i` out. -/
@[simp] theorem remove_ok (v : Vec α) (i : Usize) (h : i.val.toNat < v.val.length) :
    remove v i = Result.ok (v.val[i.val.toNat], ⟨v.val.eraseIdx i.val.toNat, by
      have h' := v.hlen; rw [List.length_eraseIdx_of_lt h]; omega⟩) := by
  unfold remove
  rw [dite_eq_left h]

/-- Out of bounds, `remove` panics. -/
@[simp] theorem remove_fail (v : Vec α) (i : Usize) (h : ¬ i.val.toNat < v.val.length) :
    remove v i = Result.fail Error.panic := by
  unfold remove
  rw [dite_eq_right h]

/-- `v.swap_remove(i)`: the element at `i`, the last element put in its place; it panics out
of bounds. -/
def swap_remove (v : Vec α) (i : Usize) : Result (α × Vec α) :=
  if h : i.val.toNat < v.val.length then
    Result.ok (v.val[i.val.toNat], ⟨(v.val.set i.val.toNat
      (v.val.getLast (List.ne_nil_of_length_pos (by omega)))).dropLast, by
        have h' := v.hlen; simp only [List.length_dropLast, List.length_set]; omega⟩)
  else Result.fail Error.panic

/-- In bounds, `swap_remove` takes the element at `i` out and puts the last in its place. -/
@[simp] theorem swap_remove_ok (v : Vec α) (i : Usize) (h : i.val.toNat < v.val.length) :
    swap_remove v i = Result.ok (v.val[i.val.toNat], ⟨(v.val.set i.val.toNat
      (v.val.getLast (List.ne_nil_of_length_pos (by omega)))).dropLast, by
        have h' := v.hlen; simp only [List.length_dropLast, List.length_set]; omega⟩) := by
  unfold swap_remove
  rw [dite_eq_left h]

/-- Out of bounds, `swap_remove` panics. -/
@[simp] theorem swap_remove_fail (v : Vec α) (i : Usize) (h : ¬ i.val.toNat < v.val.length) :
    swap_remove v i = Result.fail Error.panic := by
  unfold swap_remove
  rw [dite_eq_right h]

/-- `v.extend_from_slice(s)`: `s`'s elements after `v`'s (for elements whose clone is a copy);
it panics past `usize::MAX` elements. -/
def extend_from_slice (v : Vec α) (s : Slice α) : Result (Vec α) :=
  if h : v.val.length + s.val.length ≤ 18446744073709551615 then
    Result.ok ⟨v.val ++ s.val, by rw [List.length_append]; exact h⟩
  else Result.fail Error.panic

/-- Up to `usize::MAX` elements together, `extend_from_slice` appends `s`. -/
@[simp] theorem extend_from_slice_ok (v : Vec α) (s : Slice α)
    (h : v.val.length + s.val.length ≤ 18446744073709551615) :
    extend_from_slice v s = Result.ok ⟨v.val ++ s.val, by rw [List.length_append]; exact h⟩ := by
  unfold extend_from_slice
  rw [dite_eq_left h]

/-- Past `usize::MAX` elements together, `extend_from_slice` panics. -/
@[simp] theorem extend_from_slice_fail (v : Vec α) (s : Slice α)
    (h : ¬ v.val.length + s.val.length ≤ 18446744073709551615) :
    extend_from_slice v s = Result.fail Error.panic := by
  unfold extend_from_slice
  rw [dite_eq_right h]

end Vec

end Blaze
