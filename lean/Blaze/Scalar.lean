import Blaze.Result

/-!
Rust's integers: each type is an `Int` in its range (`Scalar ty`), as in Aeneas's first Lean
library. An `Int` with its bounds keeps `omega` usable on every arithmetic fact, signed and
unsigned alike; bit-vectors would need `bv_decide`, which trusts native evaluation.

`usize` and `isize` are 64 bits wide: v0 translates for 64-bit targets.

Checked operations (`add`, `sub`, `mul`, `div`, `rem`, `shl`, `shr`, `neg`) give a `Result`
and fail where Rust panics; the `wrapping_*` and `saturating_*` ones, the bit operations, the
casts and the comparisons are total.
-/

namespace Blaze

inductive ScalarTy where
  | U8 | U16 | U32 | U64 | U128 | Usize
  | I8 | I16 | I32 | I64 | I128 | Isize
  deriving Repr, DecidableEq

namespace ScalarTy

def min : ScalarTy → Int
  | .U8 => 0
  | .U16 => 0
  | .U32 => 0
  | .U64 => 0
  | .U128 => 0
  | .Usize => 0
  | .I8 => -128
  | .I16 => -32768
  | .I32 => -2147483648
  | .I64 => -9223372036854775808
  | .I128 => -170141183460469231731687303715884105728
  | .Isize => -9223372036854775808

def max : ScalarTy → Int
  | .U8 => 255
  | .U16 => 65535
  | .U32 => 4294967295
  | .U64 => 18446744073709551615
  | .U128 => 340282366920938463463374607431768211455
  | .Usize => 18446744073709551615
  | .I8 => 127
  | .I16 => 32767
  | .I32 => 2147483647
  | .I64 => 9223372036854775807
  | .I128 => 170141183460469231731687303715884105727
  | .Isize => 9223372036854775807

/-- `2 ^ bits`. -/
def modulus : ScalarTy → Int
  | .U8 => 256
  | .U16 => 65536
  | .U32 => 4294967296
  | .U64 => 18446744073709551616
  | .U128 => 340282366920938463463374607431768211456
  | .Usize => 18446744073709551616
  | .I8 => 256
  | .I16 => 65536
  | .I32 => 4294967296
  | .I64 => 18446744073709551616
  | .I128 => 340282366920938463463374607431768211456
  | .Isize => 18446744073709551616

def bits : ScalarTy → Nat
  | .U8 => 8
  | .U16 => 16
  | .U32 => 32
  | .U64 => 64
  | .U128 => 128
  | .Usize => 64
  | .I8 => 8
  | .I16 => 16
  | .I32 => 32
  | .I64 => 64
  | .I128 => 128
  | .Isize => 64

theorem min_le_max (ty : ScalarTy) : ty.min ≤ ty.max := by
  cases ty <;> simp only [ScalarTy.min, ScalarTy.max] <;> omega

/-- Any integer, reduced modulo `2 ^ bits` into the type's range, is in it. -/
theorem wrap_bounds (ty : ScalarTy) (x : Int) :
    ty.min ≤ (x - ty.min) % ty.modulus + ty.min ∧
      (x - ty.min) % ty.modulus + ty.min ≤ ty.max := by
  cases ty <;> simp only [ScalarTy.min, ScalarTy.max, ScalarTy.modulus] <;> omega

/-- An integer in the type's range is what it reduces to. -/
theorem wrap_id (ty : ScalarTy) (x : Int) (h : ty.min ≤ x ∧ x ≤ ty.max) :
    (x - ty.min) % ty.modulus + ty.min = x := by
  cases ty <;> simp only [ScalarTy.min, ScalarTy.max, ScalarTy.modulus] at h ⊢ <;> omega

end ScalarTy

/-- An integer of the Rust type `ty`. -/
structure Scalar (ty : ScalarTy) where
  val : Int
  hmin : ty.min ≤ val
  hmax : val ≤ ty.max

abbrev U8 := Scalar .U8
abbrev U16 := Scalar .U16
abbrev U32 := Scalar .U32
abbrev U64 := Scalar .U64
abbrev U128 := Scalar .U128
abbrev Usize := Scalar .Usize
abbrev I8 := Scalar .I8
abbrev I16 := Scalar .I16
abbrev I32 := Scalar .I32
abbrev I64 := Scalar .I64
abbrev I128 := Scalar .I128
abbrev Isize := Scalar .Isize

namespace Scalar

theorem ext {ty : ScalarTy} : ∀ {a b : Scalar ty}, a.val = b.val → a = b
  | ⟨_, _, _⟩, ⟨_, _, _⟩, rfl => rfl

/-- `x` reduced modulo `2 ^ bits` into `ty`'s range: what an `as` cast and wrapping
arithmetic give. -/
def wrap (ty : ScalarTy) (x : Int) : Scalar ty :=
  ⟨(x - ty.min) % ty.modulus + ty.min, (ty.wrap_bounds x).1, (ty.wrap_bounds x).2⟩

theorem wrap_val (ty : ScalarTy) (x : Int) (h : ty.min ≤ x ∧ x ≤ ty.max) :
    (wrap ty x).val = x :=
  ty.wrap_id x h

/-- The constant `x`, which a translated program has only in range. -/
def lit (ty : ScalarTy) (x : Int) : Scalar ty :=
  if h : ty.min ≤ x ∧ x ≤ ty.max then ⟨x, h.1, h.2⟩ else wrap ty x

theorem lit_val (ty : ScalarTy) (x : Int) (h : ty.min ≤ x ∧ x ≤ ty.max) :
    (lit ty x).val = x := by
  unfold lit
  rw [dif_pos h]

/-- An integer written back from its own value is itself. -/
theorem lit_self {ty : ScalarTy} (x : Scalar ty) : lit ty x.val = x :=
  ext (lit_val ty x.val ⟨x.hmin, x.hmax⟩)

instance {ty : ScalarTy} {n : Nat} : OfNat (Scalar ty) n := ⟨lit ty (n : Int)⟩

/-- A numeral in range is its value: `(1 : Usize).val = 1`. -/
theorem ofNat_val (ty : ScalarTy) (n : Nat)
    (h : ty.min ≤ (n : Int) ∧ (n : Int) ≤ ty.max) :
    (OfNat.ofNat n : Scalar ty).val = (n : Int) :=
  lit_val ty (n : Int) h

/-- `x` as a `ty`, failing when it is out of range. -/
def checked (ty : ScalarTy) (x : Int) : Result (Scalar ty) :=
  if h : ty.min ≤ x ∧ x ≤ ty.max then Result.ok ⟨x, h.1, h.2⟩ else Result.fail Error.arithmetic

theorem checked_ok (ty : ScalarTy) (x : Int) (h : ty.min ≤ x ∧ x ≤ ty.max) :
    checked ty x = Result.ok ⟨x, h.1, h.2⟩ := by
  unfold checked
  rw [dif_pos h]

theorem checked_fail (ty : ScalarTy) (x : Int) (h : ¬ (ty.min ≤ x ∧ x ≤ ty.max)) :
    checked ty x = Result.fail Error.arithmetic := by
  unfold checked
  rw [dif_neg h]

/-- `x` brought to the nearest end of `ty`'s range. -/
def clamp (ty : ScalarTy) (x : Int) : Scalar ty :=
  if h1 : x < ty.min then ⟨ty.min, Int.le_refl _, ty.min_le_max⟩
  else if h2 : ty.max < x then ⟨ty.max, ty.min_le_max, Int.le_refl _⟩
  else ⟨x, by omega, by omega⟩

variable {ty : ScalarTy}

/-! Checked arithmetic: Rust's operators under `overflow-checks`. -/

def add (a b : Scalar ty) : Result (Scalar ty) := checked ty (a.val + b.val)
def sub (a b : Scalar ty) : Result (Scalar ty) := checked ty (a.val - b.val)
def mul (a b : Scalar ty) : Result (Scalar ty) := checked ty (a.val * b.val)
def neg (a : Scalar ty) : Result (Scalar ty) := checked ty (-a.val)

/-- `/`: toward zero; a zero divisor and `MIN / -1` panic. -/
def div (a b : Scalar ty) : Result (Scalar ty) :=
  if b.val = 0 then Result.fail Error.arithmetic else checked ty (Int.tdiv a.val b.val)

/-- `%`: the sign of the dividend; a zero divisor and `MIN % -1` panic. -/
def rem (a b : Scalar ty) : Result (Scalar ty) :=
  if b.val = 0 ∨ (a.val = ty.min ∧ b.val = -1) then Result.fail Error.arithmetic
  else checked ty (Int.tmod a.val b.val)

/-- `<<`: a shift of `bits` or more panics; the bits shifted out are lost. -/
def shl {ty' : ScalarTy} (a : Scalar ty) (b : Scalar ty') : Result (Scalar ty) :=
  if 0 ≤ b.val ∧ b.val < (ty.bits : Int) then Result.ok (wrap ty (a.val * 2 ^ b.val.toNat))
  else Result.fail Error.arithmetic

/-- `>>`: a shift of `bits` or more panics; arithmetic on a signed type. -/
def shr {ty' : ScalarTy} (a : Scalar ty) (b : Scalar ty') : Result (Scalar ty) :=
  if 0 ≤ b.val ∧ b.val < (ty.bits : Int) then Result.ok (wrap ty (a.val / 2 ^ b.val.toNat))
  else Result.fail Error.arithmetic

theorem add_ok (a b : Scalar ty) (h : ty.min ≤ a.val + b.val ∧ a.val + b.val ≤ ty.max) :
    add a b = Result.ok ⟨a.val + b.val, h.1, h.2⟩ :=
  checked_ok ty (a.val + b.val) h

theorem sub_ok (a b : Scalar ty) (h : ty.min ≤ a.val - b.val ∧ a.val - b.val ≤ ty.max) :
    sub a b = Result.ok ⟨a.val - b.val, h.1, h.2⟩ :=
  checked_ok ty (a.val - b.val) h

theorem mul_ok (a b : Scalar ty) (h : ty.min ≤ a.val * b.val ∧ a.val * b.val ≤ ty.max) :
    mul a b = Result.ok ⟨a.val * b.val, h.1, h.2⟩ :=
  checked_ok ty (a.val * b.val) h

/-! Total arithmetic: `wrapping_*`, `saturating_*`, and the operators without
`overflow-checks`. -/

def wrapping_add (a b : Scalar ty) : Scalar ty := wrap ty (a.val + b.val)
def wrapping_sub (a b : Scalar ty) : Scalar ty := wrap ty (a.val - b.val)
def wrapping_mul (a b : Scalar ty) : Scalar ty := wrap ty (a.val * b.val)
def wrapping_neg (a : Scalar ty) : Scalar ty := wrap ty (-a.val)
def saturating_add (a b : Scalar ty) : Scalar ty := clamp ty (a.val + b.val)
def saturating_sub (a b : Scalar ty) : Scalar ty := clamp ty (a.val - b.val)

/-- `<<` with the shift taken modulo `bits`. -/
def wrapping_shl {ty' : ScalarTy} (a : Scalar ty) (b : Scalar ty') : Scalar ty :=
  wrap ty (a.val * 2 ^ (b.val % (ty.bits : Int)).toNat)

/-- `>>` with the shift taken modulo `bits`. -/
def wrapping_shr {ty' : ScalarTy} (a : Scalar ty) (b : Scalar ty') : Scalar ty :=
  wrap ty (a.val / 2 ^ (b.val % (ty.bits : Int)).toNat)

/-! Bits: each operand as the natural number its two's complement bits spell. -/

def toBits (a : Scalar ty) : Nat := (a.val % ty.modulus).toNat

def xor (a b : Scalar ty) : Scalar ty := wrap ty ((a.toBits ^^^ b.toBits : Nat) : Int)
def and (a b : Scalar ty) : Scalar ty := wrap ty ((a.toBits &&& b.toBits : Nat) : Int)
def or (a b : Scalar ty) : Scalar ty := wrap ty ((a.toBits ||| b.toBits : Nat) : Int)
def not (a : Scalar ty) : Scalar ty := wrap ty (-a.val - 1)

/-! Casts. -/

/-- `a as tgt`. -/
def cast (tgt : ScalarTy) (a : Scalar ty) : Scalar tgt := wrap tgt a.val

/-- `b as tgt`. -/
def ofBool (tgt : ScalarTy) (b : Bool) : Scalar tgt :=
  match b with
  | true => lit tgt 1
  | false => lit tgt 0

theorem cast_val (tgt : ScalarTy) (a : Scalar ty) (h : tgt.min ≤ a.val ∧ a.val ≤ tgt.max) :
    (cast tgt a).val = a.val :=
  wrap_val tgt a.val h

/-! Comparisons, as the `bool` Rust computes. -/

def eq (a b : Scalar ty) : Bool := decide (a.val = b.val)
def ne (a b : Scalar ty) : Bool := decide (a.val ≠ b.val)
def lt (a b : Scalar ty) : Bool := decide (a.val < b.val)
def le (a b : Scalar ty) : Bool := decide (a.val ≤ b.val)
def gt (a b : Scalar ty) : Bool := decide (b.val < a.val)
def ge (a b : Scalar ty) : Bool := decide (b.val ≤ a.val)

theorem eq_iff (a b : Scalar ty) : eq a b = true ↔ a.val = b.val := by simp [eq]
theorem ne_iff (a b : Scalar ty) : ne a b = true ↔ a.val ≠ b.val := by simp [ne]
theorem lt_iff (a b : Scalar ty) : lt a b = true ↔ a.val < b.val := by simp [lt]
theorem le_iff (a b : Scalar ty) : le a b = true ↔ a.val ≤ b.val := by simp [le]
theorem gt_iff (a b : Scalar ty) : gt a b = true ↔ b.val < a.val := by simp [gt]
theorem ge_iff (a b : Scalar ty) : ge a b = true ↔ b.val ≤ a.val := by simp [ge]

end Scalar

end Blaze
