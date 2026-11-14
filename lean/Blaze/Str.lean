import Blaze.ScalarOps

/-!
`str`: a string slice, as its UTF-8 bytes. A `&str` is the value it points to, as a `&[T]` is.

The UTF-8 invariant is not carried: a `Str` is any bytes, so a proof cannot use that a string
is valid UTF-8, and nothing that reads characters (`chars`, `char_indices`, slicing by byte
range, `parse`, ..) is modelled. Every string a translated program has is a literal or comes
from one through the methods below, so each is valid UTF-8 in fact; the model only does not
say so. A call of any other method of `str` is refused by name.

Each definition quotes the line of the pinned nightly's `library/core/src/str` it models.
-/

namespace Blaze

/-- `str`: its bytes. -/
structure Str where
  bytes : Slice U8
  deriving DecidableEq

namespace Str

/-- A string literal: its bytes, fewer than `usize::MAX`. -/
def ofBytes (l : List U8) (h : l.length ≤ 18446744073709551615 := by decide) : Str := ⟨⟨l, h⟩⟩

/-- `str::len` (str/mod.rs:155): `self.as_bytes().len()`, the length in bytes. -/
def len (s : Str) : Usize := Slice.len s.bytes

/-- `str::is_empty` (str/mod.rs:175): `self.len() == 0`. -/
def is_empty (s : Str) : Bool := decide ((len s).val = 0)

/-- `str::as_bytes` (str/mod.rs:513): the bytes. -/
def as_bytes (s : Str) : Slice U8 := s.bytes

-- Rust's documentation: `"foo".len() == 3`, `"ƒoo".len() == 4` (`ƒ` is two bytes),
-- `"".is_empty()`, `!"not empty".is_empty()`, `"bors".as_bytes() == b"bors"`.
example : (ofBytes [102, 111, 111]).len = 3 := by decide
example : (ofBytes [198, 146, 111, 111]).len = 4 := by decide
example : (ofBytes []).is_empty = true := by decide
example : (ofBytes [110, 111, 116]).is_empty = false := by decide
example : (ofBytes [98, 111, 114, 115]).as_bytes.val = [98, 111, 114, 115] := rfl
example : ofBytes [97] ≠ ofBytes [98] := by decide

@[simp] theorem len_val (s : Str) : (len s).val = (s.bytes.val.length : Int) := rfl

@[simp] theorem is_empty_iff (s : Str) : is_empty s = true ↔ s.bytes.val = [] := by
  unfold is_empty
  rw [decide_eq_true_eq, len_val]
  constructor
  · intro h
    exact List.eq_nil_of_length_eq_zero (by omega)
  · intro h
    rw [h]
    rfl

@[simp] theorem as_bytes_val (s : Str) : (as_bytes s).val = s.bytes.val := rfl

end Str

end Blaze
