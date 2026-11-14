import Blaze.Slice
import Blaze.Std

/-!
The methods of Rust's integers past the operators (`library/core/src/num`), over `Scalar`'s
value: bit counts, rotations and byte orders, signs, powers, checked and overflowing forms,
`Ord`'s `max`, `min` and `clamp`, and `TryFrom` between integers.

Each total method is a function to a `Scalar` (or an `Option`, a pair, a byte array), built
through `wrap`, `clamp` or `checked` so its bounds hold by construction. A method that panics in
Rust gives a `Result` and fails exactly where it panics: `abs`, `pow` and `next_power_of_two`
with overflow checks on (each has a `wrapping_` model for a crate without them), division by
zero, `clamp` with its least above its most.

Each has a lemma saying what it gives: its value, or exactly when it succeeds and with what.
A model whose behaviour is not plain from its definition says Rust's in its doc comment and has
two `example`s with values from Rust's documentation, checked by `decide` or `rfl`.

Bits are counted on the two's complement (`toBits`), least significant first. The byte order
called native (`to_ne_bytes`, `from_ne_bytes`) is little-endian: every target blaze translates
for is.
-/

namespace Blaze

namespace ScalarTy

/-- The unsigned type of the same width: what `unsigned_abs` and a signed `abs_diff` give. -/
def unsigned : ScalarTy → ScalarTy
  | .U8 | .I8 => .U8
  | .U16 | .I16 => .U16
  | .U32 | .I32 => .U32
  | .U64 | .I64 => .U64
  | .U128 | .I128 => .U128
  | .Usize | .Isize => .Usize

/-- The width in bytes. -/
def bytes (ty : ScalarTy) : Nat := ty.bits / 8

theorem bits_le (ty : ScalarTy) : ty.bits ≤ 128 := by
  cases ty <;> decide

/-- The facts of the ranges the sign methods' bounds rest on. -/
theorem facts (ty : ScalarTy) :
    ty.min ≤ 0 ∧ 0 < ty.max ∧ ty.max - ty.min ≤ ty.unsigned.max ∧ ty.unsigned.min = 0 := by
  cases ty <;> decide

end ScalarTy

namespace Scalar

variable {ty : ScalarTy}

instance : DecidableEq (Scalar ty) := fun a b =>
  if h : a.val = b.val then isTrue (ext h) else isFalse (fun e => h (congrArg Scalar.val e))

/-- `checked` succeeds exactly with the value in range. -/
@[simp] theorem checked_eq_ok (x : Int) (b : Scalar ty) :
    checked ty x = Result.ok b ↔ b.val = x := by
  unfold checked
  split
  · exact ⟨fun e => by cases e; rfl, fun e => congrArg Result.ok (ext e.symm)⟩
  · rename_i h
    exact ⟨(fun e => nomatch e),
      fun e => (h ⟨by have := b.hmin; omega, by have := b.hmax; omega⟩).elim⟩

@[simp] theorem clamp_val (x : Int) :
    (clamp ty x).val = if x < ty.min then ty.min else if ty.max < x then ty.max else x := by
  unfold clamp
  by_cases h1 : x < ty.min <;> by_cases h2 : ty.max < x <;> simp [h1, h2]

/-! Bits. -/

/-- Bit `i` of `a`'s two's complement, the least significant first. -/
def bit (a : Scalar ty) (i : Nat) : Bool := a.toBits.testBit i

/-- The number whose bit `i` is `f i`, for each `i` below `n`. -/
def ofBits (n : Nat) (f : Nat → Bool) : Nat :=
  ((_root_.List.range n).map (fun i => if f i then 2 ^ i else 0)).sum

/-- A count of at most 128, as the `u32` Rust's bit counts give. -/
def ofCount (n : Nat) (h : n ≤ 128) : U32 :=
  ⟨n, by show (0 : Int) ≤ (n : Int); omega, by show (n : Int) ≤ 4294967295; omega⟩

theorem range_length_le (ty : ScalarTy) : (_root_.List.range ty.bits).length ≤ 128 := by
  rw [_root_.List.length_range]
  exact ty.bits_le

theorem countP_le (p : Nat → Bool) : (_root_.List.range ty.bits).countP p ≤ 128 :=
  Nat.le_trans _root_.List.countP_le_length (range_length_le ty)

theorem findIdx_le (p : Nat → Bool) (l : _root_.List Nat) (h : l.length ≤ 128) :
    l.findIdx p ≤ 128 :=
  Nat.le_trans _root_.List.findIdx_le_length h

/-- `count_ones`: the bits set.

Rust: `0b01001100u8.count_ones() == 3`. -/
def count_ones (a : Scalar ty) : U32 :=
  ofCount ((_root_.List.range ty.bits).countP (bit a)) (countP_le _)

example : (count_ones (0b01001100 : U8)).val = 3 := by decide
example : (count_ones (255 : U8)).val = 8 := by decide

/-- `count_zeros`: the bits clear.

Rust: `u8::MAX.count_zeros() == 0`, `0u32.count_zeros() == 32`. -/
def count_zeros (a : Scalar ty) : U32 :=
  ofCount ((_root_.List.range ty.bits).countP (fun i => !bit a i)) (countP_le _)

example : (count_zeros (255 : U8)).val = 0 := by decide
example : (count_zeros (0 : U32)).val = 32 := by decide

/-- `trailing_zeros`: the clear bits below the lowest set one; all of them for zero.

Rust: `0b0101000u8.trailing_zeros() == 3`, `0u8.trailing_zeros() == 8`. -/
def trailing_zeros (a : Scalar ty) : U32 :=
  ofCount ((_root_.List.range ty.bits).findIdx (bit a)) (findIdx_le _ _ (range_length_le ty))

example : (trailing_zeros (0b0101000 : U8)).val = 3 := by decide
example : (trailing_zeros (0 : U8)).val = 8 := by decide

/-- `leading_zeros`: the clear bits above the highest set one; all of them for zero.

Rust: `(u8::MAX >> 2).leading_zeros() == 2`, `0u16.leading_zeros() == 16`. -/
def leading_zeros (a : Scalar ty) : U32 :=
  ofCount ((_root_.List.range ty.bits).reverse.findIdx (bit a))
    (findIdx_le _ _ (by rw [_root_.List.length_reverse]; exact range_length_le ty))

example : (leading_zeros (63 : U8)).val = 2 := by decide
example : (leading_zeros (0 : U16)).val = 16 := by decide

@[simp] theorem count_ones_val (a : Scalar ty) :
    (count_ones a).val = ((_root_.List.range ty.bits).countP (bit a) : Int) := rfl

@[simp] theorem count_zeros_val (a : Scalar ty) :
    (count_zeros a).val = ((_root_.List.range ty.bits).countP (fun i => !bit a i) : Int) := rfl

@[simp] theorem trailing_zeros_val (a : Scalar ty) :
    (trailing_zeros a).val = ((_root_.List.range ty.bits).findIdx (bit a) : Int) := rfl

@[simp] theorem leading_zeros_val (a : Scalar ty) :
    (leading_zeros a).val = ((_root_.List.range ty.bits).reverse.findIdx (bit a) : Int) := rfl

/-- `is_power_of_two` (unsigned): exactly one bit is set.

Rust: `16u8.is_power_of_two()`, `!10u8.is_power_of_two()`. -/
def is_power_of_two (a : Scalar ty) : Bool :=
  decide ((_root_.List.range ty.bits).countP (bit a) = 1)

example : is_power_of_two (16 : U8) = true := by decide
example : is_power_of_two (10 : U8) = false := by decide

@[simp] theorem is_power_of_two_iff (a : Scalar ty) :
    is_power_of_two a = true ↔ (_root_.List.range ty.bits).countP (bit a) = 1 := by
  simp [is_power_of_two]

/-- The least power of two at or above `a` (unsigned), as an integer: `1` for zero, and
`2 ^ bits` past the highest power of two of the type. -/
def nextPow (a : Scalar ty) : Int :=
  2 ^ (_root_.List.range (ty.bits + 1)).findIdx (fun k => decide (a.val ≤ 2 ^ k))

/-- `next_power_of_two` with overflow checks on: it panics past the type's highest power of
two.

Rust: `2u8.next_power_of_two() == 2`, `3u8.next_power_of_two() == 4`, `0u8.next_power_of_two()
== 1`. -/
def next_power_of_two (a : Scalar ty) : Result (Scalar ty) := checked ty (nextPow a)

/-- `next_power_of_two` without overflow checks: zero past the type's highest power of two. -/
def wrapping_next_power_of_two (a : Scalar ty) : Scalar ty := wrap ty (nextPow a)

example : next_power_of_two (3 : U8) = Result.ok 4 := rfl
example : next_power_of_two (0 : U8) = Result.ok 1 := rfl
example : next_power_of_two (200 : U8) = Result.fail Error.arithmetic := rfl
example : wrapping_next_power_of_two (200 : U8) = 0 := by decide

@[simp] theorem next_power_of_two_eq_ok (a b : Scalar ty) :
    next_power_of_two a = Result.ok b ↔ b.val = nextPow a :=
  checked_eq_ok _ _

@[simp] theorem wrapping_next_power_of_two_val (a : Scalar ty) :
    (wrapping_next_power_of_two a).val = (wrap ty (nextPow a)).val := rfl

/-- `reverse_bits`: bit `i` becomes bit `bits - 1 - i`.

Rust: `0x12u8.reverse_bits() == 0x48`, `1u8.reverse_bits() == 0x80`. -/
def reverse_bits (a : Scalar ty) : Scalar ty :=
  wrap ty (ofBits ty.bits (fun i => bit a (ty.bits - 1 - i)) : Int)

example : reverse_bits (0x12 : U8) = 0x48 := by decide
example : reverse_bits (1 : U8) = 0x80 := by decide

/-- `rotate_left`: bit `i` moves to bit `(i + n) % bits`.

Rust: `0x82u8.rotate_left(2) == 0xa`. -/
def rotate_left (a : Scalar ty) (n : U32) : Scalar ty :=
  wrap ty (ofBits ty.bits
    (fun i => bit a ((i + ty.bits - n.val.toNat % ty.bits) % ty.bits)) : Int)

/-- `rotate_right`: bit `(i + n) % bits` moves to bit `i`.

Rust: `0xau8.rotate_right(2) == 0x82`. -/
def rotate_right (a : Scalar ty) (n : U32) : Scalar ty :=
  wrap ty (ofBits ty.bits (fun i => bit a ((i + n.val.toNat % ty.bits) % ty.bits)) : Int)

example : rotate_left (0x82 : U8) 2 = 0xa := by decide
example : rotate_left (0x82 : U8) 10 = 0xa := by decide
example : rotate_right (0xa : U8) 2 = 0x82 := by decide
example : rotate_right (0xa : U8) 10 = 0x82 := by decide

@[simp] theorem reverse_bits_val (a : Scalar ty) :
    (reverse_bits a).val = (wrap ty (ofBits ty.bits (fun i => bit a (ty.bits - 1 - i)))).val :=
  rfl

@[simp] theorem rotate_left_val (a : Scalar ty) (n : U32) :
    (rotate_left a n).val = (wrap ty (ofBits ty.bits
      (fun i => bit a ((i + ty.bits - n.val.toNat % ty.bits) % ty.bits)))).val := rfl

@[simp] theorem rotate_right_val (a : Scalar ty) (n : U32) :
    (rotate_right a n).val =
      (wrap ty (ofBits ty.bits (fun i => bit a ((i + n.val.toNat % ty.bits) % ty.bits)))).val :=
  rfl

/-! Bytes. -/

/-- Byte `i` of `a`'s two's complement, the least significant first. -/
def byte (a : Scalar ty) (i : Nat) : U8 := wrap .U8 ((a.toBits / 256 ^ i : Nat) : Int)

/-- `to_le_bytes`: the bytes, the least significant first. -/
def to_le_bytes (a : Scalar ty) : Arr U8 ty.bytes :=
  ⟨(_root_.List.range ty.bytes).map (byte a), by simp⟩

/-- `to_be_bytes`: the bytes, the most significant first.

Rust: `0x12345678u32.to_be_bytes() == [0x12, 0x34, 0x56, 0x78]`. -/
def to_be_bytes (a : Scalar ty) : Arr U8 ty.bytes :=
  ⟨(to_le_bytes a).val.reverse, by rw [_root_.List.length_reverse]; exact (to_le_bytes a).hlen⟩

/-- `to_ne_bytes`: little-endian, the order of every target blaze translates for. -/
def to_ne_bytes (a : Scalar ty) : Arr U8 ty.bytes := to_le_bytes a

example : (to_be_bytes (0x12345678 : U32)).val = [0x12, 0x34, 0x56, 0x78] := by decide
example : (to_le_bytes (0x12345678 : U32)).val = [0x78, 0x56, 0x34, 0x12] := by decide

/-- The number little-endian bytes spell. -/
def ofLE : _root_.List U8 → Nat
  | [] => 0
  | b :: rest => b.toBits + 256 * ofLE rest

/-- `from_le_bytes`: the integer of type `tgt` whose bytes, the least significant first, are
`b`. -/
def from_le_bytes (tgt : ScalarTy) (b : Arr U8 tgt.bytes) : Scalar tgt :=
  wrap tgt (ofLE b.val : Int)

/-- `from_be_bytes`: the integer of type `tgt` whose bytes, the most significant first, are
`b`.

Rust: `u32::from_be_bytes([0x12, 0x34, 0x56, 0x78]) == 0x12345678`. -/
def from_be_bytes (tgt : ScalarTy) (b : Arr U8 tgt.bytes) : Scalar tgt :=
  wrap tgt (ofLE b.val.reverse : Int)

/-- `from_ne_bytes`: little-endian, as `to_ne_bytes`. -/
def from_ne_bytes (tgt : ScalarTy) (b : Arr U8 tgt.bytes) : Scalar tgt := from_le_bytes tgt b

example : from_be_bytes .U32 ⟨[0x12, 0x34, 0x56, 0x78], rfl⟩ = 0x12345678 := by decide
example : from_le_bytes .U32 ⟨[0x78, 0x56, 0x34, 0x12], rfl⟩ = 0x12345678 := by decide

/-- `swap_bytes`: the bytes in the other order.

Rust: `0x1234u16.swap_bytes() == 0x3412`. -/
def swap_bytes (a : Scalar ty) : Scalar ty := from_be_bytes ty (to_le_bytes a)

example : swap_bytes (0x1234 : U16) = 0x3412 := by decide
example : swap_bytes (0x12345678 : U32) = 0x78563412 := by decide

@[simp] theorem byte_val (a : Scalar ty) (i : Nat) :
    (byte a i).val = ((a.toBits / 256 ^ i : Nat) : Int) % 256 := by
  simp only [byte, wrap, ScalarTy.min, ScalarTy.modulus, Int.sub_zero, Int.add_zero]

@[simp] theorem to_le_bytes_val (a : Scalar ty) :
    (to_le_bytes a).val = (_root_.List.range ty.bytes).map (byte a) := rfl

@[simp] theorem to_be_bytes_val (a : Scalar ty) :
    (to_be_bytes a).val = ((_root_.List.range ty.bytes).map (byte a)).reverse := rfl

@[simp] theorem to_ne_bytes_val (a : Scalar ty) :
    (to_ne_bytes a).val = (_root_.List.range ty.bytes).map (byte a) := rfl

@[simp] theorem from_le_bytes_val (tgt : ScalarTy) (b : Arr U8 tgt.bytes) :
    (from_le_bytes tgt b).val = (wrap tgt (ofLE b.val : Int)).val := rfl

@[simp] theorem from_be_bytes_val (tgt : ScalarTy) (b : Arr U8 tgt.bytes) :
    (from_be_bytes tgt b).val = (wrap tgt (ofLE b.val.reverse : Int)).val := rfl

@[simp] theorem from_ne_bytes_val (tgt : ScalarTy) (b : Arr U8 tgt.bytes) :
    (from_ne_bytes tgt b).val = (wrap tgt (ofLE b.val : Int)).val := rfl

@[simp] theorem swap_bytes_val (a : Scalar ty) :
    (swap_bytes a).val = (wrap ty (ofLE (to_le_bytes a).val.reverse : Int)).val := rfl

/-! Signs and distances. -/

/-- `abs` (signed) with overflow checks on: `|a|`, which panics at `MIN`.

Rust: `(-10i8).abs() == 10`; `i8::MIN.abs()` panics. -/
def abs (a : Scalar ty) : Result (Scalar ty) := checked ty (if a.val < 0 then -a.val else a.val)

/-- `wrapping_abs` (signed), and `abs` without overflow checks: `|a|`, `MIN` at `MIN`.

Rust: `(-100i8).wrapping_abs() == 100`, `i8::MIN.wrapping_abs() == i8::MIN`. -/
def wrapping_abs (a : Scalar ty) : Scalar ty := wrap ty (if a.val < 0 then -a.val else a.val)

example : abs (lit .I8 (-10)) = Result.ok 10 := rfl
example : abs (lit .I8 (-128)) = Result.fail Error.arithmetic := rfl
example : (wrapping_abs (lit .I8 (-100))).val = 100 := by decide
example : (wrapping_abs (lit .I8 (-128))).val = -128 := by decide

/-- `unsigned_abs` (signed): `|a|` in the unsigned type of the same width, so `MIN` fits.

Rust: `(-100i8).unsigned_abs() == 100u8`, `i8::MIN.unsigned_abs() == 128u8`. -/
def unsigned_abs (a : Scalar ty) : Scalar ty.unsigned :=
  wrap ty.unsigned (if a.val < 0 then -a.val else a.val)

example : (unsigned_abs (lit .I8 (-100))).val = 100 := by decide
example : (unsigned_abs (lit .I8 (-128))).val = 128 := by decide

/-- `abs_diff`: `|a - b|`, in the unsigned type of the same width.

Rust: `100u8.abs_diff(80) == 20u8`, `(-100i8).abs_diff(80) == 180u8`. -/
def abs_diff (a b : Scalar ty) : Scalar ty.unsigned :=
  wrap ty.unsigned (if a.val < b.val then b.val - a.val else a.val - b.val)

example : (abs_diff (100 : U8) 80).val = 20 := by decide
example : (abs_diff (lit .I8 (-100)) 80).val = 180 := by decide

/-- `signum` (signed): `-1`, `0` or `1` by the sign.

Rust: `10i8.signum() == 1`, `(-10i8).signum() == -1`. -/
def signum (a : Scalar ty) : Scalar ty :=
  wrap ty (if a.val < 0 then -1 else if a.val = 0 then 0 else 1)

example : (signum (10 : I8)).val = 1 := by decide
example : (signum (lit .I8 (-10))).val = -1 := by decide

@[simp] theorem abs_eq_ok (a b : Scalar ty) :
    abs a = Result.ok b ↔ b.val = if a.val < 0 then -a.val else a.val :=
  checked_eq_ok _ _

@[simp] theorem wrapping_abs_val (a : Scalar ty) :
    (wrapping_abs a).val = (wrap ty (if a.val < 0 then -a.val else a.val)).val := rfl

@[simp] theorem unsigned_abs_val (a : Scalar ty) :
    (unsigned_abs a).val = if a.val < 0 then -a.val else a.val := by
  have := a.hmin
  have := a.hmax
  have := ty.facts
  unfold unsigned_abs
  apply wrap_val
  split <;> omega

@[simp] theorem abs_diff_val (a b : Scalar ty) :
    (abs_diff a b).val = if a.val < b.val then b.val - a.val else a.val - b.val := by
  have := a.hmin
  have := a.hmax
  have := b.hmin
  have := b.hmax
  have := ty.facts
  unfold abs_diff
  apply wrap_val
  split <;> omega

/-- `signum`'s value, on a signed type. -/
theorem signum_val (a : Scalar ty) (h : ty.min < 0) :
    (signum a).val = if a.val < 0 then -1 else if a.val = 0 then 0 else 1 := by
  have := ty.facts
  unfold signum
  apply wrap_val
  split
  · omega
  · split <;> omega

/-! Powers, saturation, division. -/

/-- `pow` with overflow checks on: `a ^ e`, which panics when it is out of range. Rust's
square-and-multiply squares only while bits of `e` are left, so it overflows exactly when the
power does.

Rust: `2u8.pow(7) == 128`; `2u8.pow(8)` panics. -/
def pow (a : Scalar ty) (e : U32) : Result (Scalar ty) := checked ty (a.val ^ e.val.toNat)

/-- `wrapping_pow`, and `pow` without overflow checks: `a ^ e` modulo `2 ^ bits`.

Rust: `3u8.wrapping_pow(5) == 243`, `3u8.wrapping_pow(6) == 217`. -/
def wrapping_pow (a : Scalar ty) (e : U32) : Scalar ty := wrap ty (a.val ^ e.val.toNat)

/-- `saturating_pow`: `a ^ e` brought to the nearest end of the range.

Rust: `4u8.saturating_pow(4) == u8::MAX`, `(-3i8).saturating_pow(5) == i8::MIN`. -/
def saturating_pow (a : Scalar ty) (e : U32) : Scalar ty := clamp ty (a.val ^ e.val.toNat)

/-- `saturating_mul`: `a * b` brought to the nearest end of the range. -/
def saturating_mul (a b : Scalar ty) : Scalar ty := clamp ty (a.val * b.val)

example : pow (2 : U8) 7 = Result.ok 128 := rfl
example : pow (2 : U8) 8 = Result.fail Error.arithmetic := rfl
example : wrapping_pow (3 : U8) 5 = 243 := by decide
example : wrapping_pow (3 : U8) 6 = 217 := by decide
example : saturating_pow (4 : U8) 4 = 255 := by decide
example : (saturating_pow (lit .I8 (-3)) 5).val = -128 := by decide

@[simp] theorem pow_eq_ok (a b : Scalar ty) (e : U32) :
    pow a e = Result.ok b ↔ b.val = a.val ^ e.val.toNat :=
  checked_eq_ok _ _

@[simp] theorem wrapping_pow_val (a : Scalar ty) (e : U32) :
    (wrapping_pow a e).val = (wrap ty (a.val ^ e.val.toNat)).val := rfl

@[simp] theorem saturating_pow_val (a : Scalar ty) (e : U32) :
    (saturating_pow a e).val = (clamp ty (a.val ^ e.val.toNat)).val := rfl

@[simp] theorem saturating_mul_val (a b : Scalar ty) :
    (saturating_mul a b).val = (clamp ty (a.val * b.val)).val := rfl

/-- `div_ceil` (unsigned): `a / b` rounded up; a zero divisor panics.

Rust: `7u8.div_ceil(4) == 2`, `8u8.div_ceil(4) == 2`. -/
def div_ceil (a b : Scalar ty) : Result (Scalar ty) :=
  if b.val = 0 then Result.fail Error.arithmetic
  else Result.ok (wrap ty ((a.val + b.val - 1) / b.val))

example : div_ceil (7 : U8) 4 = Result.ok 2 := rfl
example : div_ceil (8 : U8) 4 = Result.ok 2 := rfl

/-- `wrapping_div`: `/` toward zero, with `MIN / -1` wrapping to `MIN`; a zero divisor
panics.

Rust: `100u8.wrapping_div(10) == 10`, `(-128i8).wrapping_div(-1) == -128`. -/
def wrapping_div (a b : Scalar ty) : Result (Scalar ty) :=
  if b.val = 0 then Result.fail Error.arithmetic else Result.ok (wrap ty (Int.tdiv a.val b.val))

/-- `wrapping_rem`: `%` with the sign of the dividend, `MIN % -1` giving `0`; a zero divisor
panics.

Rust: `100u8.wrapping_rem(10) == 0`, `(-128i8).wrapping_rem(-1) == 0`. -/
def wrapping_rem (a b : Scalar ty) : Result (Scalar ty) :=
  if b.val = 0 then Result.fail Error.arithmetic else Result.ok (wrap ty (Int.tmod a.val b.val))

example : wrapping_div (100 : U8) 10 = Result.ok 10 := rfl
example : wrapping_div (lit .I8 (-128)) (lit .I8 (-1)) = Result.ok (lit .I8 (-128)) := rfl
example : wrapping_rem (100 : U8) 10 = Result.ok 0 := rfl
example : wrapping_rem (lit .I8 (-128)) (lit .I8 (-1)) = Result.ok 0 := rfl

/-- `is_multiple_of`: `a % b == 0`, and for a zero `b`, `a == 0`.

Rust: `6u8.is_multiple_of(2)`, `!5u8.is_multiple_of(2)`, `0u8.is_multiple_of(0)`,
`!6u8.is_multiple_of(0)`. -/
def is_multiple_of (a b : Scalar ty) : Bool :=
  if b.val = 0 then decide (a.val = 0) else decide (a.val % b.val = 0)

example : is_multiple_of (6 : U8) 2 = true ∧ is_multiple_of (5 : U8) 2 = false := by decide
example : is_multiple_of (0 : U8) 0 = true ∧ is_multiple_of (6 : U8) 0 = false := by decide

@[simp] theorem div_ceil_eq_ok (a b c : Scalar ty) :
    div_ceil a b = Result.ok c ↔ b.val ≠ 0 ∧ c = wrap ty ((a.val + b.val - 1) / b.val) := by
  unfold div_ceil
  split
  · rename_i h
    exact ⟨(fun e => nomatch e), fun e => absurd h e.1⟩
  · rename_i h
    exact ⟨fun e => ⟨h, by cases e; rfl⟩, fun e => by rw [e.2]⟩

@[simp] theorem wrapping_div_eq_ok (a b c : Scalar ty) :
    wrapping_div a b = Result.ok c ↔ b.val ≠ 0 ∧ c = wrap ty (Int.tdiv a.val b.val) := by
  unfold wrapping_div
  split
  · rename_i h
    exact ⟨(fun e => nomatch e), fun e => absurd h e.1⟩
  · rename_i h
    exact ⟨fun e => ⟨h, by cases e; rfl⟩, fun e => by rw [e.2]⟩

@[simp] theorem wrapping_rem_eq_ok (a b c : Scalar ty) :
    wrapping_rem a b = Result.ok c ↔ b.val ≠ 0 ∧ c = wrap ty (Int.tmod a.val b.val) := by
  unfold wrapping_rem
  split
  · rename_i h
    exact ⟨(fun e => nomatch e), fun e => absurd h e.1⟩
  · rename_i h
    exact ⟨fun e => ⟨h, by cases e; rfl⟩, fun e => by rw [e.2]⟩

@[simp] theorem is_multiple_of_iff (a b : Scalar ty) :
    is_multiple_of a b = true ↔ if b.val = 0 then a.val = 0 else a.val % b.val = 0 := by
  unfold is_multiple_of
  split <;> simp

/-! Checked forms: `None` where the operator would panic or overflow. -/

/-- `x` as a `ty`, `none` when it is out of range. -/
def opt (ty : ScalarTy) (x : Int) : Option (Scalar ty) :=
  if h : ty.min ≤ x ∧ x ≤ ty.max then some ⟨x, h.1, h.2⟩ else none

@[simp] theorem opt_eq_some (x : Int) (b : Scalar ty) : opt ty x = some b ↔ b.val = x := by
  unfold opt
  split
  · exact ⟨fun e => by cases e; rfl, fun e => congrArg some (ext e.symm)⟩
  · rename_i h
    exact ⟨(fun e => nomatch e),
      fun e => (h ⟨by have := b.hmin; omega, by have := b.hmax; omega⟩).elim⟩

@[simp] theorem opt_eq_none (x : Int) : opt ty x = none ↔ ¬ (ty.min ≤ x ∧ x ≤ ty.max) := by
  unfold opt
  split
  · rename_i h
    exact ⟨(fun e => nomatch e), fun e => absurd h e⟩
  · rename_i h
    exact ⟨fun _ => h, fun _ => rfl⟩

def checked_add (a b : Scalar ty) : Option (Scalar ty) := opt ty (a.val + b.val)
def checked_sub (a b : Scalar ty) : Option (Scalar ty) := opt ty (a.val - b.val)
def checked_mul (a b : Scalar ty) : Option (Scalar ty) := opt ty (a.val * b.val)
def checked_neg (a : Scalar ty) : Option (Scalar ty) := opt ty (-a.val)
def checked_pow (a : Scalar ty) (e : U32) : Option (Scalar ty) := opt ty (a.val ^ e.val.toNat)

/-- `checked_div`: `/` toward zero; `None` for a zero divisor and for `MIN / -1`.

Rust: `128u8.checked_div(2) == Some(64)`, `1u8.checked_div(0) == None`. -/
def checked_div (a b : Scalar ty) : Option (Scalar ty) :=
  if b.val = 0 then none else opt ty (Int.tdiv a.val b.val)

/-- `checked_rem`: `%` with the sign of the dividend; `None` for a zero divisor and for
`MIN % -1`, whose quotient overflows.

Rust: `5i32.checked_rem(2) == Some(1)`, `i32::MIN.checked_rem(-1) == None`. -/
def checked_rem (a b : Scalar ty) : Option (Scalar ty) :=
  if b.val = 0 ∨ (a.val = ty.min ∧ b.val = -1) then none else opt ty (Int.tmod a.val b.val)

/-- `checked_shl`: `<<`, `None` for a shift of `bits` or more; the bits shifted out are lost.

Rust: `0x1u32.checked_shl(4) == Some(0x10)`, `0x10u32.checked_shl(129) == None`. -/
def checked_shl (a : Scalar ty) (b : U32) : Option (Scalar ty) :=
  if b.val < (ty.bits : Int) then some (wrap ty (a.val * 2 ^ b.val.toNat)) else none

/-- `checked_shr`: `>>`, `None` for a shift of `bits` or more; arithmetic on a signed type.

Rust: `0x10u32.checked_shr(4) == Some(0x1)`, `0x10u32.checked_shr(33) == None`. -/
def checked_shr (a : Scalar ty) (b : U32) : Option (Scalar ty) :=
  if b.val < (ty.bits : Int) then some (wrap ty (a.val / 2 ^ b.val.toNat)) else none

example : checked_div (128 : U8) 2 = some 64 ∧ checked_div (1 : U8) 0 = none := by decide
example : checked_rem (lit .I32 5) 2 = some 1 := by decide
example : checked_rem (lit .I32 (-2147483648)) (lit .I32 (-1)) = none := by decide
example : checked_shl (1 : U32) 4 = some 0x10 ∧ checked_shl (0x10 : U32) 129 = none := by decide
example : checked_shr (0x10 : U32) 4 = some 1 ∧ checked_shr (0x10 : U32) 33 = none := by decide

@[simp] theorem checked_add_eq_some (a b c : Scalar ty) :
    checked_add a b = some c ↔ c.val = a.val + b.val := opt_eq_some _ _

@[simp] theorem checked_sub_eq_some (a b c : Scalar ty) :
    checked_sub a b = some c ↔ c.val = a.val - b.val := opt_eq_some _ _

@[simp] theorem checked_mul_eq_some (a b c : Scalar ty) :
    checked_mul a b = some c ↔ c.val = a.val * b.val := opt_eq_some _ _

@[simp] theorem checked_neg_eq_some (a c : Scalar ty) :
    checked_neg a = some c ↔ c.val = -a.val := opt_eq_some _ _

@[simp] theorem checked_pow_eq_some (a c : Scalar ty) (e : U32) :
    checked_pow a e = some c ↔ c.val = a.val ^ e.val.toNat := opt_eq_some _ _

@[simp] theorem checked_div_eq_some (a b c : Scalar ty) :
    checked_div a b = some c ↔ b.val ≠ 0 ∧ c.val = Int.tdiv a.val b.val := by
  unfold checked_div
  split
  · rename_i h
    exact ⟨(fun e => nomatch e), fun e => absurd h e.1⟩
  · rename_i h
    rw [opt_eq_some]
    exact ⟨fun e => ⟨h, e⟩, fun e => e.2⟩

@[simp] theorem checked_rem_eq_some (a b c : Scalar ty) :
    checked_rem a b = some c ↔
      ¬ (b.val = 0 ∨ (a.val = ty.min ∧ b.val = -1)) ∧ c.val = Int.tmod a.val b.val := by
  unfold checked_rem
  split
  · rename_i h
    exact ⟨(fun e => nomatch e), fun e => absurd h e.1⟩
  · rename_i h
    rw [opt_eq_some]
    exact ⟨fun e => ⟨h, e⟩, fun e => e.2⟩

@[simp] theorem checked_shl_eq_some (a c : Scalar ty) (b : U32) :
    checked_shl a b = some c ↔
      b.val < (ty.bits : Int) ∧ c = wrap ty (a.val * 2 ^ b.val.toNat) := by
  unfold checked_shl
  split
  · rename_i h
    exact ⟨fun e => ⟨h, by cases e; rfl⟩, fun e => by rw [e.2]⟩
  · rename_i h
    exact ⟨(fun e => nomatch e), fun e => absurd e.1 h⟩

@[simp] theorem checked_shr_eq_some (a c : Scalar ty) (b : U32) :
    checked_shr a b = some c ↔
      b.val < (ty.bits : Int) ∧ c = wrap ty (a.val / 2 ^ b.val.toNat) := by
  unfold checked_shr
  split
  · rename_i h
    exact ⟨fun e => ⟨h, by cases e; rfl⟩, fun e => by rw [e.2]⟩
  · rename_i h
    exact ⟨(fun e => nomatch e), fun e => absurd e.1 h⟩

/-! Overflowing forms: the wrapped value, and whether it wrapped. -/

def overflowing_add (a b : Scalar ty) : Scalar ty × Bool :=
  (wrap ty (a.val + b.val), decide ¬ (ty.min ≤ a.val + b.val ∧ a.val + b.val ≤ ty.max))
def overflowing_sub (a b : Scalar ty) : Scalar ty × Bool :=
  (wrap ty (a.val - b.val), decide ¬ (ty.min ≤ a.val - b.val ∧ a.val - b.val ≤ ty.max))
def overflowing_mul (a b : Scalar ty) : Scalar ty × Bool :=
  (wrap ty (a.val * b.val), decide ¬ (ty.min ≤ a.val * b.val ∧ a.val * b.val ≤ ty.max))

example : overflowing_add (255 : U8) 1 = (0, true) ∧ overflowing_add (5 : U8) 2 = (7, false) := by
  decide

@[simp] theorem overflowing_add_val (a b : Scalar ty) :
    (overflowing_add a b).1.val = (wrap ty (a.val + b.val)).val ∧
      ((overflowing_add a b).2 = true ↔ ¬ (ty.min ≤ a.val + b.val ∧ a.val + b.val ≤ ty.max)) :=
  ⟨rfl, decide_eq_true_iff⟩

@[simp] theorem overflowing_sub_val (a b : Scalar ty) :
    (overflowing_sub a b).1.val = (wrap ty (a.val - b.val)).val ∧
      ((overflowing_sub a b).2 = true ↔ ¬ (ty.min ≤ a.val - b.val ∧ a.val - b.val ≤ ty.max)) :=
  ⟨rfl, decide_eq_true_iff⟩

@[simp] theorem overflowing_mul_val (a b : Scalar ty) :
    (overflowing_mul a b).1.val = (wrap ty (a.val * b.val)).val ∧
      ((overflowing_mul a b).2 = true ↔ ¬ (ty.min ≤ a.val * b.val ∧ a.val * b.val ≤ ty.max)) :=
  ⟨rfl, decide_eq_true_iff⟩

/-! Order: `Ord::max`, `Ord::min`, `Ord::clamp` and `cmp::max`, `cmp::min`. -/

/-- `max`: the greater, `b` when they are equal. -/
def max (a b : Scalar ty) : Scalar ty := if b.val < a.val then a else b

/-- `min`: the lesser, `a` when they are equal. -/
def min (a b : Scalar ty) : Scalar ty := if b.val < a.val then b else a

/-- `clamp`: `a` brought into `[lo, hi]`; it panics when `lo > hi`.

Rust: `(-3i32).clamp(-2, 1) == -2`, `2i32.clamp(-2, 1) == 1`. -/
def ord_clamp (a lo hi : Scalar ty) : Result (Scalar ty) :=
  if hi.val < lo.val then Result.fail Error.panic
  else Result.ok (if a.val < lo.val then lo else if hi.val < a.val then hi else a)

example : ord_clamp (lit .I32 (-3)) (lit .I32 (-2)) 1 = Result.ok (lit .I32 (-2)) := rfl
example : ord_clamp (lit .I32 2) (lit .I32 (-2)) 1 = Result.ok 1 := rfl
example : ord_clamp (5 : U8) 7 3 = Result.fail Error.panic := rfl

@[simp] theorem max_val (a b : Scalar ty) :
    (Scalar.max a b).val = if b.val < a.val then a.val else b.val := by
  unfold Scalar.max
  split <;> rfl

@[simp] theorem min_val (a b : Scalar ty) :
    (Scalar.min a b).val = if b.val < a.val then b.val else a.val := by
  unfold Scalar.min
  split <;> rfl

theorem le_max_left (a b : Scalar ty) : a.val ≤ (Scalar.max a b).val := by
  rw [max_val]; split <;> omega

theorem le_max_right (a b : Scalar ty) : b.val ≤ (Scalar.max a b).val := by
  rw [max_val]; split <;> omega

theorem max_le (a b c : Scalar ty) (ha : a.val ≤ c.val) (hb : b.val ≤ c.val) :
    (Scalar.max a b).val ≤ c.val := by
  rw [max_val]; split <;> omega

theorem min_le_left (a b : Scalar ty) : (Scalar.min a b).val ≤ a.val := by
  rw [min_val]; split <;> omega

theorem min_le_right (a b : Scalar ty) : (Scalar.min a b).val ≤ b.val := by
  rw [min_val]; split <;> omega

theorem le_min (a b c : Scalar ty) (ha : c.val ≤ a.val) (hb : c.val ≤ b.val) :
    c.val ≤ (Scalar.min a b).val := by
  rw [min_val]; split <;> omega

@[simp] theorem ord_clamp_eq_ok (a lo hi c : Scalar ty) :
    ord_clamp a lo hi = Result.ok c ↔
      lo.val ≤ hi.val ∧ c = (if a.val < lo.val then lo else if hi.val < a.val then hi else a) := by
  unfold ord_clamp
  split
  · rename_i h
    exact ⟨(fun e => nomatch e), fun e => absurd e.1 (by omega)⟩
  · rename_i h
    exact ⟨fun e => ⟨by omega, by cases e; rfl⟩, fun e => by rw [e.2]⟩

@[simp] theorem ord_clamp_eq_fail (a lo hi : Scalar ty) (e : Error) :
    ord_clamp a lo hi = Result.fail e ↔ hi.val < lo.val ∧ e = Error.panic := by
  unfold ord_clamp
  split
  · rename_i h
    exact ⟨fun x => ⟨h, by cases x; rfl⟩, fun x => by rw [x.2]⟩
  · rename_i h
    exact ⟨(fun x => nomatch x), fun x => absurd x.1 h⟩

theorem ord_clamp_bounds (a lo hi c : Scalar ty) (h : ord_clamp a lo hi = Result.ok c) :
    lo.val ≤ c.val ∧ c.val ≤ hi.val := by
  obtain ⟨hle, rfl⟩ := (ord_clamp_eq_ok a lo hi c).1 h
  split
  · omega
  · split <;> omega

/-! `TryFrom` between integers. -/

/-- `tgt::try_from(a)`: `Ok` with the same value when it is in `tgt`'s range, else `Err`.

Rust: `u8::try_from(200u32) == Ok(200)`, `u8::try_from(300u32).is_err()`. -/
def try_from (tgt : ScalarTy) (a : Scalar ty) :
    core.result.Result (Scalar tgt) core.num.TryFromIntError :=
  if h : tgt.min ≤ a.val ∧ a.val ≤ tgt.max then .Ok ⟨a.val, h.1, h.2⟩ else .Err ⟨⟩

example : try_from .U8 (200 : U32) = .Ok 200 := rfl
example : try_from .U8 (300 : U32) = .Err ⟨⟩ := rfl

@[simp] theorem try_from_eq_Ok (tgt : ScalarTy) (a : Scalar ty) (b : Scalar tgt) :
    try_from tgt a = .Ok b ↔ b.val = a.val := by
  unfold try_from
  split
  · exact ⟨fun e => by cases e; rfl, fun e => congrArg _ (ext e.symm)⟩
  · rename_i h
    exact ⟨(fun e => nomatch e),
      fun e => (h ⟨by have := b.hmin; omega, by have := b.hmax; omega⟩).elim⟩

@[simp] theorem try_from_eq_Err (tgt : ScalarTy) (a : Scalar ty) (e : core.num.TryFromIntError) :
    try_from tgt a = .Err e ↔ ¬ (tgt.min ≤ a.val ∧ a.val ≤ tgt.max) := by
  unfold try_from
  split
  · rename_i h
    exact ⟨(fun x => nomatch x), fun x => absurd h x⟩
  · rename_i h
    exact ⟨fun _ => h, fun _ => by cases e; rfl⟩

end Scalar

end Blaze
