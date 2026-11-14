import Blaze.Ordering

/-!
`core::time::Duration`: whole seconds (`u64`) and the nanoseconds past them (`u32`, below a
second), the invariant carried in the structure as `Slice` carries its length bound.

Each operation is a transcription of the pinned nightly's `library/core/src/time.rs`, whose line
it quotes, over the fields' values. A method that panics in Rust (`new` when the carry
overflows, `+` and `-` through `expect`) gives a `Blaze.Result` and fails with `Error.panic`
exactly where it panics; `expect`'s message is not kept.

`toNanos`, the whole duration in nanoseconds (`secs * 10^9 + nanos`), is what the lemmas
speak of: each operation's effect on it, and that it tells durations apart (`ext_toNanos`), so a
proof about durations is a proof about integers.

Not modelled: the methods over floats (`as_secs_f64`, `mul_f64`, ..), `checked_mul`,
`checked_div` and the operators `*` and `/`, `abs_diff`, `from_weeks` and the like, `Sum`, `Debug`.
A call of one is refused by name.
-/

namespace Blaze.core.time

/-- `core::time::Duration` (time.rs:79: `pub struct Duration { secs: u64, nanos: Nanoseconds }`;
`Nanoseconds` is a `u32` below `NANOS_PER_SEC`, time.rs:27: `1_000_000_000`). -/
structure Duration where
  secs : U64
  nanos : U32
  hnanos : nanos.val < 1000000000

namespace Duration

private theorem u64_bounds (x : U64) : 0 ≤ x.val ∧ x.val ≤ 18446744073709551615 :=
  ⟨x.hmin, x.hmax⟩

private theorem u32_bounds (x : U32) : 0 ≤ x.val ∧ x.val ≤ 4294967295 :=
  ⟨x.hmin, x.hmax⟩

/-- Two durations are equal when their fields are. -/
theorem ext : ∀ {a b : Duration}, a.secs = b.secs → a.nanos = b.nanos → a = b
  | ⟨_, _, _⟩, ⟨_, _, _⟩, rfl, rfl => rfl

instance : DecidableEq Duration := fun a b =>
  if h : a.secs = b.secs ∧ a.nanos = b.nanos then isTrue (ext h.1 h.2)
  else isFalse (fun e => h ⟨congrArg secs e, congrArg nanos e⟩)

/-- The duration of `secs` seconds and `nanos` nanoseconds, each in range. -/
def ofParts (secs nanos : Int) (hs : 0 ≤ secs ∧ secs ≤ 18446744073709551615)
    (hn : 0 ≤ nanos ∧ nanos < 1000000000) : Duration :=
  ⟨⟨secs, hs.1, hs.2⟩, ⟨nanos, hn.1, by have := hn.2; show nanos ≤ 4294967295; omega⟩, hn.2⟩

/-- A constant, as const evaluation gives it: its fields, `nanos` below a second. -/
def ofLit (secs : U64) (nanos : U32) (h : nanos.val < 1000000000 := by decide) : Duration :=
  ⟨secs, nanos, h⟩

/-- The whole duration in nanoseconds. -/
def toNanos (d : Duration) : Int := d.secs.val * 1000000000 + d.nanos.val

/-- The longest duration, `Duration::MAX`, in nanoseconds. -/
def maxNanos : Int := 18446744073709551615999999999

theorem toNanos_bounds (d : Duration) : 0 ≤ d.toNanos ∧ d.toNanos ≤ maxNanos := by
  have := u64_bounds d.secs
  have := u32_bounds d.nanos
  have := d.hnanos
  unfold toNanos maxNanos
  omega

/-- A duration is its nanoseconds: two with the same `toNanos` are equal. -/
theorem ext_toNanos {a b : Duration} (h : a.toNanos = b.toNanos) : a = b := by
  have := a.hnanos
  have := b.hnanos
  have := u32_bounds a.nanos
  have := u32_bounds b.nanos
  unfold toNanos at h
  exact ext (Scalar.ext (by omega)) (Scalar.ext (by omega))

theorem eq_iff_toNanos (a b : Duration) : a = b ↔ a.toNanos = b.toNanos :=
  ⟨fun h => h ▸ rfl, ext_toNanos⟩

@[simp] theorem ofParts_toNanos (secs nanos : Int) hs hn :
    (ofParts secs nanos hs hn).toNanos = secs * 1000000000 + nanos := rfl

@[simp] theorem ofLit_toNanos (secs : U64) (nanos : U32) h :
    (ofLit secs nanos h).toNanos = secs.val * 1000000000 + nanos.val := rfl

/-! Constants. -/

/-- `Duration::ZERO` (time.rs:231): `Duration::from_nanos(0)`. -/
def ZERO : Duration := ofParts 0 0 (by decide) (by decide)

/-- `Duration::MAX` (time.rs:250): `Duration::new(u64::MAX, NANOS_PER_SEC - 1)`. -/
def MAX : Duration := ofParts 18446744073709551615 999999999 (by decide) (by decide)

@[simp] theorem ZERO_toNanos : ZERO.toNanos = 0 := rfl
@[simp] theorem MAX_toNanos : MAX.toNanos = maxNanos := rfl

/-! Making a duration. -/

/-- `Duration::new` (time.rs:274): with `nanos < NANOS_PER_SEC` the two fields; else
`secs.checked_add((nanos / NANOS_PER_SEC) as u64).expect(..)` and `nanos % NANOS_PER_SEC`,
panicking when the carry overflows. -/
def new (secs : U64) (nanos : U32) : Result Duration :=
  if h : nanos.val < 1000000000 then .ok ⟨secs, nanos, h⟩
  else if hc : secs.val + nanos.val / 1000000000 ≤ 18446744073709551615 then
    .ok (ofParts (secs.val + nanos.val / 1000000000) (nanos.val % 1000000000)
      (by have := u64_bounds secs; have := u32_bounds nanos; omega)
      (by omega))
  else .fail .panic

/-- `Duration::from_secs` (time.rs:304): `Duration { secs, nanos: Nanoseconds::ZERO }`. -/
def from_secs (secs : U64) : Duration := ⟨secs, ⟨0, by decide, by decide⟩, by decide⟩

/-- `Duration::from_millis` (time.rs:324): `millis / MILLIS_PER_SEC` seconds and
`(millis % MILLIS_PER_SEC) * NANOS_PER_MILLI` nanoseconds. -/
def from_millis (millis : U64) : Duration :=
  ofParts (millis.val / 1000) (millis.val % 1000 * 1000000)
    (by have := u64_bounds millis; omega) (by have := u64_bounds millis; omega)

/-- `Duration::from_micros` (time.rs:350): `micros / MICROS_PER_SEC` seconds and
`(micros % MICROS_PER_SEC) * NANOS_PER_MICRO` nanoseconds. -/
def from_micros (micros : U64) : Duration :=
  ofParts (micros.val / 1000000) (micros.val % 1000000 * 1000)
    (by have := u64_bounds micros; omega) (by have := u64_bounds micros; omega)

/-- `Duration::from_nanos` (time.rs:381): `nanos / NANOS_PER_SEC` seconds and
`nanos % NANOS_PER_SEC` nanoseconds. -/
def from_nanos (nanos : U64) : Duration :=
  ofParts (nanos.val / 1000000000) (nanos.val % 1000000000)
    (by have := u64_bounds nanos; omega) (by have := u64_bounds nanos; omega)

/-! Reading a duration. -/

/-- `is_zero` (time.rs:566): `self.secs == 0 && self.nanos.as_inner() == 0`. -/
def is_zero (d : Duration) : Bool := decide (d.secs.val = 0) && decide (d.nanos.val = 0)

/-- `as_secs` (time.rs:594): `self.secs`. -/
def as_secs (d : Duration) : U64 := d.secs

/-- `subsec_millis` (time.rs:617): `self.nanos.as_inner() / NANOS_PER_MILLI`. -/
def subsec_millis (d : Duration) : U32 :=
  ⟨d.nanos.val / 1000000, by have := u32_bounds d.nanos; show 0 ≤ _; omega,
    by have := u32_bounds d.nanos; show _ ≤ 4294967295; omega⟩

/-- `subsec_micros` (time.rs:640): `self.nanos.as_inner() / NANOS_PER_MICRO`. -/
def subsec_micros (d : Duration) : U32 :=
  ⟨d.nanos.val / 1000, by have := u32_bounds d.nanos; show 0 ≤ _; omega,
    by have := u32_bounds d.nanos; show _ ≤ 4294967295; omega⟩

/-- `subsec_nanos` (time.rs:663): `self.nanos.as_inner()`. -/
def subsec_nanos (d : Duration) : U32 := d.nanos

/-- `as_millis` (time.rs:681): `self.secs as u128 * MILLIS_PER_SEC as u128 +
(self.nanos.as_inner() / NANOS_PER_MILLI) as u128`. -/
def as_millis (d : Duration) : U128 :=
  ⟨d.secs.val * 1000 + d.nanos.val / 1000000,
    by have := u64_bounds d.secs; have := u32_bounds d.nanos; show 0 ≤ _; omega,
    by have := u64_bounds d.secs; have := u32_bounds d.nanos
       show _ ≤ 340282366920938463463374607431768211455; omega⟩

/-- `as_micros` (time.rs:700): `self.secs as u128 * MICROS_PER_SEC as u128 +
(self.nanos.as_inner() / NANOS_PER_MICRO) as u128`. -/
def as_micros (d : Duration) : U128 :=
  ⟨d.secs.val * 1000000 + d.nanos.val / 1000,
    by have := u64_bounds d.secs; have := u32_bounds d.nanos; show 0 ≤ _; omega,
    by have := u64_bounds d.secs; have := u32_bounds d.nanos
       show _ ≤ 340282366920938463463374607431768211455; omega⟩

/-- `as_nanos` (time.rs:719): `self.secs as u128 * NANOS_PER_SEC as u128 +
self.nanos.as_inner() as u128`. -/
def as_nanos (d : Duration) : U128 :=
  ⟨d.secs.val * 1000000000 + d.nanos.val,
    by have := u64_bounds d.secs; have := u32_bounds d.nanos; show 0 ≤ _; omega,
    by have := u64_bounds d.secs; have := u32_bounds d.nanos
       show _ ≤ 340282366920938463463374607431768211455; omega⟩

/-! Arithmetic. -/

/-- `checked_add` (time.rs:758): `self.secs.checked_add(rhs.secs)`, then the nanoseconds'
sum, less a second and with one more second (`secs.checked_add(1)`) when it is a second or
more; `None` where either addition overflows. -/
def checked_add (a b : Duration) : Option Duration :=
  if hs : a.secs.val + b.secs.val ≤ 18446744073709551615 then
    if hn : 1000000000 ≤ a.nanos.val + b.nanos.val then
      if hs1 : a.secs.val + b.secs.val + 1 ≤ 18446744073709551615 then
        some (ofParts (a.secs.val + b.secs.val + 1) (a.nanos.val + b.nanos.val - 1000000000)
          (by have := u64_bounds a.secs; have := u64_bounds b.secs; omega)
          (by have := a.hnanos; have := b.hnanos; omega))
      else none
    else
      some (ofParts (a.secs.val + b.secs.val) (a.nanos.val + b.nanos.val)
        (by have := u64_bounds a.secs; have := u64_bounds b.secs; omega)
        (by have := u32_bounds a.nanos; have := u32_bounds b.nanos; omega))
  else none

/-- `checked_sub` (time.rs:814): `self.secs.checked_sub(rhs.secs)`, then the nanoseconds'
difference, borrowing a second (`secs.checked_sub(1)`) when `self`'s are fewer; `None` where
either subtraction overflows. -/
def checked_sub (a b : Duration) : Option Duration :=
  if hs : b.secs.val ≤ a.secs.val then
    if hn : b.nanos.val ≤ a.nanos.val then
      some (ofParts (a.secs.val - b.secs.val) (a.nanos.val - b.nanos.val)
        (by have := u64_bounds a.secs; have := u64_bounds b.secs; omega)
        (by have := a.hnanos; have := u32_bounds b.nanos; omega))
    else if hs1 : 1 ≤ a.secs.val - b.secs.val then
      some (ofParts (a.secs.val - b.secs.val - 1) (a.nanos.val + 1000000000 - b.nanos.val)
        (by have := u64_bounds a.secs; have := u64_bounds b.secs; omega)
        (by have := b.hnanos; have := u32_bounds a.nanos; omega))
    else none
  else none

/-- `saturating_add` (time.rs:791): `checked_add`, or `Duration::MAX` on `None`. -/
def saturating_add (a b : Duration) : Duration :=
  match checked_add a b with
  | some d => d
  | none => MAX

/-- `saturating_sub` (time.rs:847): `checked_sub`, or `Duration::ZERO` on `None`. -/
def saturating_sub (a b : Duration) : Duration :=
  match checked_sub a b with
  | some d => d
  | none => ZERO

/-- `a + b`, `Add::add` (time.rs:1345): `self.checked_add(rhs).expect("adding durations should
not exceed u64::MAX")`. -/
def add (a b : Duration) : Result Duration :=
  match checked_add a b with
  | some d => .ok d
  | none => .fail .panic

/-- `a - b`, `Sub::sub` (time.rs:1365): `self.checked_sub(rhs).expect("subtracting durations
should not be negative")`. -/
def sub (a b : Duration) : Result Duration :=
  match checked_sub a b with
  | some d => .ok d
  | none => .fail .panic

/-- `a += b`, `AddAssign::add_assign` (time.rs:1355): `*self = *self + rhs`. -/
def add_assign (a b : Duration) : Result Duration := add a b

/-- `a -= b`, `SubAssign::sub_assign` (time.rs:1375): `*self = *self - rhs`. -/
def sub_assign (a b : Duration) : Result Duration := sub a b

/-! Order: derived (time.rs:79, `#[derive(.., PartialEq, Eq, PartialOrd, Ord, ..)]`), so the
fields compared in order, `secs` then `nanos`. -/

/-- `Ord::cmp`, derived: the seconds' order, then the nanoseconds'. -/
def cmp (a b : Duration) : core.cmp.Ordering := (Scalar.cmp a.secs b.secs).then (Scalar.cmp a.nanos b.nanos)

/-- `PartialOrd::partial_cmp`, derived: `Some` of the fields' order. -/
def partial_cmp (a b : Duration) : Option core.cmp.Ordering := some (cmp a b)

/-- `a < b`, `PartialOrd::lt` (cmp.rs:1502: `self.partial_cmp(other).is_some_and(Ordering::is_lt)`). -/
def lt (a b : Duration) : Bool := (cmp a b).is_lt
/-- `a <= b` (cmp.rs:1520: `is_some_and(Ordering::is_le)`). -/
def le (a b : Duration) : Bool := (cmp a b).is_le
/-- `a > b` (cmp.rs:1538: `is_some_and(Ordering::is_gt)`). -/
def gt (a b : Duration) : Bool := (cmp a b).is_gt
/-- `a >= b` (cmp.rs:1556: `is_some_and(Ordering::is_ge)`). -/
def ge (a b : Duration) : Bool := (cmp a b).is_ge

/-- `Ord::max` (cmp.rs:1101: `if other < self { self } else { other }`). -/
def max (a b : Duration) : Duration := if lt b a then a else b

/-- `Ord::min` (cmp.rs:1140: `if other < self { other } else { self }`). -/
def min (a b : Duration) : Duration := if lt b a then b else a

/-! Examples, from Rust's documentation of each. -/

example : (from_secs 5).toNanos = 5000000000 := by decide
example : (from_millis 2569).secs = 2 ∧ (from_millis 2569).nanos = 569000000 := by decide
example : (from_micros 1000002).secs = 1 ∧ (from_micros 1000002).nanos = 2000 := by decide
example : (from_nanos 1000000123).secs = 1 ∧ (from_nanos 1000000123).nanos = 123 := by decide
example : new 5 30 = .ok (ofLit 5 30) := rfl
example : new 18446744073709551615 1000000000 = .fail .panic := rfl
example : new 1 2000000001 = .ok (ofLit 3 1) := rfl
example : ZERO.is_zero = true := by decide
example : (from_nanos 1).is_zero = false := by decide
example : (ofLit 5 730023852).as_secs = 5 := by decide
example : (ofLit 5 730023852).as_millis = 5730 := by decide
example : (ofLit 5 730023852).as_micros = 5730023 := by decide
example : (ofLit 5 730023852).as_nanos = 5730023852 := by decide
example : (from_millis 5432).subsec_millis = 432 := by decide
example : (from_micros 1234567).subsec_micros = 234567 := by decide
example : (from_millis 5010).subsec_nanos = 10000000 := by decide
example : checked_add (ofLit 0 0) (ofLit 0 1) = some (ofLit 0 1) := by decide
example : checked_add (ofLit 1 0) (ofLit 18446744073709551615 0) = none := by decide
example : checked_add (ofLit 0 999999999) (ofLit 0 1) = some (ofLit 1 0) := by decide
example : checked_sub (ofLit 0 1) (ofLit 0 0) = some (ofLit 0 1) := by decide
example : checked_sub ZERO (ofLit 0 1) = none := by decide
example : checked_sub (ofLit 1 0) (ofLit 0 1) = some (ofLit 0 999999999) := by decide
example : saturating_add (ofLit 0 0) (ofLit 0 1) = ofLit 0 1 := by decide
example : saturating_add (ofLit 1 0) MAX = MAX := by decide
example : saturating_sub (ofLit 0 1) (ofLit 0 0) = ofLit 0 1 := by decide
example : saturating_sub ZERO (ofLit 0 1) = ZERO := by decide
example : add (from_secs 1) (from_millis 500) = .ok (ofLit 1 500000000) := rfl
example : add MAX (from_nanos 1) = .fail .panic := rfl
example : sub (from_secs 1) (from_millis 500) = .ok (ofLit 0 500000000) := rfl
example : sub ZERO (from_nanos 1) = .fail .panic := rfl
example : cmp (from_secs 1) (from_millis 1500) = .Less := by decide
example : lt (ofLit 1 5) (ofLit 1 6) = true := by decide
example : ge (ofLit 2 0) (ofLit 1 999999999) = true := by decide
example : max (from_secs 1) (from_millis 900) = from_secs 1 := by decide
example : min (from_secs 1) (from_millis 900) = from_millis 900 := by decide

/-! What each operation does to `toNanos`. -/

@[simp] theorem from_secs_toNanos (s : U64) : (from_secs s).toNanos = s.val * 1000000000 := by
  show s.val * 1000000000 + 0 = s.val * 1000000000
  omega

@[simp] theorem from_millis_toNanos (m : U64) : (from_millis m).toNanos = m.val * 1000000 := by
  show m.val / 1000 * 1000000000 + m.val % 1000 * 1000000 = m.val * 1000000
  omega

@[simp] theorem from_micros_toNanos (m : U64) : (from_micros m).toNanos = m.val * 1000 := by
  show m.val / 1000000 * 1000000000 + m.val % 1000000 * 1000 = m.val * 1000
  omega

@[simp] theorem from_nanos_toNanos (n : U64) : (from_nanos n).toNanos = n.val := by
  show n.val / 1000000000 * 1000000000 + n.val % 1000000000 = n.val
  omega

@[simp] theorem as_nanos_val (d : Duration) : d.as_nanos.val = d.toNanos := rfl

@[simp] theorem as_micros_val (d : Duration) : d.as_micros.val = d.toNanos / 1000 := by
  have := d.hnanos
  have := u32_bounds d.nanos
  show d.secs.val * 1000000 + d.nanos.val / 1000 = (d.secs.val * 1000000000 + d.nanos.val) / 1000
  omega

@[simp] theorem as_millis_val (d : Duration) : d.as_millis.val = d.toNanos / 1000000 := by
  have := d.hnanos
  have := u32_bounds d.nanos
  show d.secs.val * 1000 + d.nanos.val / 1000000 =
    (d.secs.val * 1000000000 + d.nanos.val) / 1000000
  omega

@[simp] theorem as_secs_val (d : Duration) : d.as_secs.val = d.toNanos / 1000000000 := by
  have := d.hnanos
  have := u32_bounds d.nanos
  show d.secs.val = (d.secs.val * 1000000000 + d.nanos.val) / 1000000000
  omega

@[simp] theorem is_zero_iff (d : Duration) : d.is_zero = true ↔ d.toNanos = 0 := by
  have := u64_bounds d.secs
  have := u32_bounds d.nanos
  unfold is_zero toNanos
  rw [Bool.and_eq_true, decide_eq_true_eq, decide_eq_true_eq]
  constructor
  · intro h
    omega
  · intro h
    omega

/-- `checked_add` gives the sum exactly when it is at most `Duration::MAX`. -/
theorem checked_add_eq_some (a b c : Duration) :
    checked_add a b = some c ↔
      a.toNanos + b.toNanos ≤ maxNanos ∧ c.toNanos = a.toNanos + b.toNanos := by
  have := a.hnanos
  have := b.hnanos
  have := u32_bounds a.nanos
  have := u32_bounds b.nanos
  unfold checked_add
  split
  · split
    · split
      · rw [Option.some.injEq, eq_iff_toNanos, ofParts_toNanos]
        unfold toNanos maxNanos
        constructor
        · intro h
          omega
        · intro h
          omega
      · refine ⟨fun e => (nomatch e), fun h => ?_⟩
        unfold toNanos maxNanos at h
        omega
    · rw [Option.some.injEq, eq_iff_toNanos, ofParts_toNanos]
      unfold toNanos maxNanos
      constructor
      · intro h
        omega
      · intro h
        omega
  · refine ⟨fun e => (nomatch e), fun h => ?_⟩
    unfold toNanos maxNanos at h
    omega

/-- `checked_add` is `None` exactly when the sum is more than `Duration::MAX`. -/
theorem checked_add_eq_none (a b : Duration) :
    checked_add a b = none ↔ maxNanos < a.toNanos + b.toNanos := by
  have := a.hnanos
  have := b.hnanos
  have := u32_bounds a.nanos
  have := u32_bounds b.nanos
  unfold checked_add
  split
  · split
    · split
      · refine ⟨fun e => (nomatch e), fun h => ?_⟩
        unfold toNanos maxNanos at h
        omega
      · refine ⟨fun _ => ?_, fun _ => rfl⟩
        unfold toNanos maxNanos
        omega
    · refine ⟨fun e => (nomatch e), fun h => ?_⟩
      unfold toNanos maxNanos at h
      omega
  · refine ⟨fun _ => ?_, fun _ => rfl⟩
    unfold toNanos maxNanos
    omega

/-- `checked_sub` gives the difference exactly when it is not negative. -/
theorem checked_sub_eq_some (a b c : Duration) :
    checked_sub a b = some c ↔ b.toNanos ≤ a.toNanos ∧ c.toNanos = a.toNanos - b.toNanos := by
  have := a.hnanos
  have := b.hnanos
  have := u32_bounds a.nanos
  have := u32_bounds b.nanos
  unfold checked_sub
  split
  · split
    · rw [Option.some.injEq, eq_iff_toNanos, ofParts_toNanos]
      unfold toNanos
      constructor
      · intro h
        omega
      · intro h
        omega
    · split
      · rw [Option.some.injEq, eq_iff_toNanos, ofParts_toNanos]
        unfold toNanos
        constructor
        · intro h
          omega
        · intro h
          omega
      · refine ⟨fun e => (nomatch e), fun h => ?_⟩
        unfold toNanos at h
        omega
  · refine ⟨fun e => (nomatch e), fun h => ?_⟩
    unfold toNanos at h
    omega

/-- `checked_sub` is `None` exactly when `b` is longer than `a`. -/
theorem checked_sub_eq_none (a b : Duration) :
    checked_sub a b = none ↔ a.toNanos < b.toNanos := by
  have := a.hnanos
  have := b.hnanos
  have := u32_bounds a.nanos
  have := u32_bounds b.nanos
  unfold checked_sub
  split
  · split
    · refine ⟨fun e => (nomatch e), fun h => ?_⟩
      unfold toNanos at h
      omega
    · split
      · refine ⟨fun e => (nomatch e), fun h => ?_⟩
        unfold toNanos at h
        omega
      · refine ⟨fun _ => ?_, fun _ => rfl⟩
        unfold toNanos
        omega
  · refine ⟨fun _ => ?_, fun _ => rfl⟩
    unfold toNanos
    omega

/-- `saturating_add` is the sum, or `Duration::MAX` past it. -/
@[simp] theorem saturating_add_toNanos (a b : Duration) :
    (saturating_add a b).toNanos =
      if a.toNanos + b.toNanos ≤ maxNanos then a.toNanos + b.toNanos else maxNanos := by
  unfold saturating_add
  split
  · rename_i d hd
    have := (checked_add_eq_some a b d).1 hd
    rw [ite_eq_left this.1]
    exact this.2
  · rename_i hd
    have := (checked_add_eq_none a b).1 hd
    rw [ite_eq_right (show ¬ (a.toNanos + b.toNanos ≤ maxNanos) by omega)]
    rfl

/-- `saturating_sub` is the difference, or `Duration::ZERO` below it. -/
@[simp] theorem saturating_sub_toNanos (a b : Duration) :
    (saturating_sub a b).toNanos =
      if b.toNanos ≤ a.toNanos then a.toNanos - b.toNanos else 0 := by
  unfold saturating_sub
  split
  · rename_i d hd
    have := (checked_sub_eq_some a b d).1 hd
    rw [ite_eq_left this.1]
    exact this.2
  · rename_i hd
    have := (checked_sub_eq_none a b).1 hd
    rw [ite_eq_right (show ¬ (b.toNanos ≤ a.toNanos) by omega)]
    rfl

/-- `a + b` gives the sum exactly when it is at most `Duration::MAX`, and panics otherwise. -/
theorem add_eq_ok (a b c : Duration) :
    add a b = .ok c ↔ a.toNanos + b.toNanos ≤ maxNanos ∧ c.toNanos = a.toNanos + b.toNanos := by
  rw [← checked_add_eq_some]
  unfold add
  split
  · rename_i d hd
    try rw [hd]
    exact ⟨fun e => by cases e; rfl, fun e => by cases e; rfl⟩
  · rename_i hd
    try rw [hd]
    exact ⟨fun e => (nomatch e), fun e => (nomatch e)⟩

theorem add_eq_fail (a b : Duration) :
    add a b = .fail .panic ↔ maxNanos < a.toNanos + b.toNanos := by
  rw [← checked_add_eq_none]
  unfold add
  split
  · rename_i d hd
    try rw [hd]
    exact ⟨fun e => (nomatch e), fun e => (nomatch e)⟩
  · rename_i hd
    try rw [hd]
    exact ⟨fun _ => rfl, fun _ => rfl⟩

/-- `a - b` gives the difference exactly when it is not negative, and panics otherwise. -/
theorem sub_eq_ok (a b c : Duration) :
    sub a b = .ok c ↔ b.toNanos ≤ a.toNanos ∧ c.toNanos = a.toNanos - b.toNanos := by
  rw [← checked_sub_eq_some]
  unfold sub
  split
  · rename_i d hd
    try rw [hd]
    exact ⟨fun e => by cases e; rfl, fun e => by cases e; rfl⟩
  · rename_i hd
    try rw [hd]
    exact ⟨fun e => (nomatch e), fun e => (nomatch e)⟩

theorem sub_eq_fail (a b : Duration) :
    sub a b = .fail .panic ↔ a.toNanos < b.toNanos := by
  rw [← checked_sub_eq_none]
  unfold sub
  split
  · rename_i d hd
    try rw [hd]
    exact ⟨fun e => (nomatch e), fun e => (nomatch e)⟩
  · rename_i hd
    try rw [hd]
    exact ⟨fun _ => rfl, fun _ => rfl⟩

/-- The derived order is the order of `toNanos`. -/
theorem cmp_eq_Less (a b : Duration) : cmp a b = .Less ↔ a.toNanos < b.toNanos := by
  have := a.hnanos
  have := b.hnanos
  have := u32_bounds a.nanos
  have := u32_bounds b.nanos
  unfold cmp toNanos
  by_cases h1 : a.secs.val < b.secs.val
  · rw [(Scalar.cmp_eq_Less _ _).2 h1]
    exact ⟨fun _ => by omega, fun _ => rfl⟩
  · by_cases h2 : a.secs.val = b.secs.val
    · rw [(Scalar.cmp_eq_Equal _ _).2 h2]
      show Scalar.cmp a.nanos b.nanos = .Less ↔ _
      rw [Scalar.cmp_eq_Less]
      constructor
      · intro h
        omega
      · intro h
        omega
    · rw [(Scalar.cmp_eq_Greater a.secs b.secs).2 (by omega)]
      show core.cmp.Ordering.Greater = .Less ↔ _
      exact ⟨fun e => (nomatch e), fun _ => by omega⟩

theorem cmp_eq_Equal (a b : Duration) : cmp a b = .Equal ↔ a.toNanos = b.toNanos := by
  have := a.hnanos
  have := b.hnanos
  have := u32_bounds a.nanos
  have := u32_bounds b.nanos
  unfold cmp toNanos
  by_cases h1 : a.secs.val < b.secs.val
  · rw [(Scalar.cmp_eq_Less _ _).2 h1]
    show core.cmp.Ordering.Less = .Equal ↔ _
    exact ⟨fun e => (nomatch e), fun _ => by omega⟩
  · by_cases h2 : a.secs.val = b.secs.val
    · rw [(Scalar.cmp_eq_Equal _ _).2 h2]
      show Scalar.cmp a.nanos b.nanos = .Equal ↔ _
      rw [Scalar.cmp_eq_Equal]
      constructor
      · intro h
        omega
      · intro h
        omega
    · rw [(Scalar.cmp_eq_Greater a.secs b.secs).2 (by omega)]
      show core.cmp.Ordering.Greater = .Equal ↔ _
      exact ⟨fun e => (nomatch e), fun _ => by omega⟩

theorem cmp_eq_Greater (a b : Duration) : cmp a b = .Greater ↔ b.toNanos < a.toNanos := by
  cases h : cmp a b
  · have := (cmp_eq_Less a b).1 h
    exact ⟨fun e => (nomatch e), fun _ => by omega⟩
  · have := (cmp_eq_Equal a b).1 h
    exact ⟨fun e => (nomatch e), fun _ => by omega⟩
  · refine ⟨fun _ => ?_, fun _ => rfl⟩
    have h1 : ¬ a.toNanos < b.toNanos := fun x => nomatch h.symm.trans ((cmp_eq_Less a b).2 x)
    have h2 : ¬ a.toNanos = b.toNanos := fun x => nomatch h.symm.trans ((cmp_eq_Equal a b).2 x)
    omega

@[simp] theorem lt_iff (a b : Duration) : lt a b = true ↔ a.toNanos < b.toNanos := by
  unfold lt
  rw [core.cmp.Ordering.is_lt_iff, cmp_eq_Less]

@[simp] theorem le_iff (a b : Duration) : le a b = true ↔ a.toNanos ≤ b.toNanos := by
  unfold le
  rw [core.cmp.Ordering.is_le_iff, ne_eq, cmp_eq_Greater]
  exact Int.not_lt

@[simp] theorem gt_iff (a b : Duration) : gt a b = true ↔ b.toNanos < a.toNanos := by
  unfold gt
  rw [core.cmp.Ordering.is_gt_iff, cmp_eq_Greater]

@[simp] theorem ge_iff (a b : Duration) : ge a b = true ↔ b.toNanos ≤ a.toNanos := by
  unfold ge
  rw [core.cmp.Ordering.is_ge_iff, ne_eq, cmp_eq_Less]
  exact Int.not_lt

@[simp] theorem lt_eq_false (a b : Duration) : lt a b = false ↔ b.toNanos ≤ a.toNanos := by
  rw [← Bool.not_eq_true, lt_iff]
  exact Int.not_lt

@[simp] theorem le_eq_false (a b : Duration) : le a b = false ↔ b.toNanos < a.toNanos := by
  rw [← Bool.not_eq_true, le_iff]
  exact Int.not_le

@[simp] theorem gt_eq_false (a b : Duration) : gt a b = false ↔ a.toNanos ≤ b.toNanos := by
  rw [← Bool.not_eq_true, gt_iff]
  exact Int.not_lt

@[simp] theorem ge_eq_false (a b : Duration) : ge a b = false ↔ a.toNanos < b.toNanos := by
  rw [← Bool.not_eq_true, ge_iff]
  exact Int.not_le

end Duration

end Blaze.core.time
