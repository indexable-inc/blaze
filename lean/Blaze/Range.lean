import Blaze.Slice

/-!
`core::ops::Range<Idx>` and `core::ops::RangeInclusive<Idx>` as values: built, their fields read,
cloned and compared, `contains` and `is_empty` of a range of integers, `start()` and `end()` of an
inclusive one, and a slice indexed by a `Range<usize>`. Each definition is a transcription of the
pinned nightly's `library/core/src/ops/range.rs` (or `slice/index.rs`), whose line it quotes.

Not modelled: iterating a range (`for i in a..b`, `Iterator::next`), `len` (`ExactSizeIterator`),
`RangeFrom`, `RangeTo` and the other kinds of range, a range of a type that is not an integer (it
compares by its `PartialOrd`), and slicing by an inclusive range. A use of one is refused by name.
-/

namespace Blaze.core.ops

/-- `core::ops::Range<Idx>` (range.rs:86: `pub struct Range<Idx> { pub start: Idx, pub end: Idx }`),
`start..end`. Its `Clone` and `PartialEq` are derived (range.rs:84,
`#[derive_const(Clone, Default, PartialEq)]`): the value, and both fields equal, which is Lean's
`=` of the structure. -/
structure Range (Idx : Type) : Type where
  start : Idx
  «end» : Idx
  deriving DecidableEq

/-- `core::ops::RangeInclusive<Idx>` (range.rs:362: `pub(crate) start: Idx, pub(crate) end: Idx,
pub(crate) exhausted: bool`), `start..=end`. `exhausted` is `false` when it is made and set only by
iterating it, which is not modelled. Its `Clone` and `PartialEq` are derived (range.rs:359-360,
`#[derive(Clone, Hash)] #[derive_const(Eq, PartialEq)]`): all three fields, Lean's `=`. -/
structure RangeInclusive (Idx : Type) : Type where
  start : Idx
  «end» : Idx
  exhausted : Bool
  deriving DecidableEq

namespace Range

variable {ty : ScalarTy}

/-- `start..end`: MIR's aggregate of the fields in order (range.rs:86). -/
@[simp] def new {Idx : Type} (start stop : Idx) : Range Idx := ⟨start, stop⟩

/-- `Range::contains` (range.rs:132: `<Self as RangeBounds<Idx>>::contains(self, item)`), whose
`RangeBounds::contains` (range.rs:891-898: `(match self.start_bound() { Included(start) => start
<= item, .. }) && (match self.end_bound() { .. Excluded(end) => item < end, .. })`) has, for a
`Range`, `start_bound` `Included(&self.start)` and `end_bound` `Excluded(&self.end)`
(range.rs:1125, 1128): `start <= item && item < end`, of integers. -/
@[simp] def contains (r : Range (Scalar ty)) (item : Scalar ty) : Bool :=
  decide (r.start.val ≤ item.val) && decide (item.val < r.«end».val)

/-- `Range::is_empty` (range.rs:160: `!(self.start < self.end)`). -/
@[simp] def is_empty (r : Range (Scalar ty)) : Bool := !decide (r.start.val < r.«end».val)

end Range

namespace RangeInclusive

variable {ty : ScalarTy}

/-- `RangeInclusive::new` and `start..=end` (range.rs:400-401: `pub const fn new(start: Idx, end:
Idx) -> Self { Self { start, end, exhausted: false } }`). `start()` and `end()` (range.rs:425-426,
450-451: `&self.start`, `&self.end`) are the fields' projections, `RangeInclusive.start` and
`RangeInclusive.«end»`: a shared reference is its value in the models. -/
@[simp] def new {Idx : Type} (start stop : Idx) : RangeInclusive Idx := ⟨start, stop, false⟩

/-- `RangeInclusive::contains` (range.rs:544: `<Self as RangeBounds<Idx>>::contains(self, item)`),
`RangeBounds::contains` (range.rs:891-898) with `start_bound` `Included(&self.start)` and
`end_bound` (range.rs:1146-1155) `if self.exhausted { Excluded(&self.start) } else
{ Included(&self.end) }`: `start <= item && item <= end`, or `start <= item && item < start`
(nothing) once exhausted, of integers. -/
@[simp] def contains (r : RangeInclusive (Scalar ty)) (item : Scalar ty) : Bool :=
  decide (r.start.val ≤ item.val) &&
    (if r.exhausted then decide (item.val < r.start.val) else decide (item.val ≤ r.«end».val))

/-- `RangeInclusive::is_empty` (range.rs:581: `self.exhausted || !(self.start <= self.end)`). -/
@[simp] def is_empty (r : RangeInclusive (Scalar ty)) : Bool :=
  r.exhausted || !decide (r.start.val ≤ r.«end».val)

end RangeInclusive

end Blaze.core.ops

namespace Blaze.Slice

variable {α : Type}

/-- `&s[r]` of a slice or a `Vec` by a `Range<usize>` (slice/index.rs:405-413): `Slice.subslice`
of its bounds. -/
@[simp] def index_range (s : Slice α) (r : core.ops.Range Usize) : Result (Slice α) :=
  subslice s r.start r.«end»

/-- `s.get(r)` by a `Range<usize>` (slice/index.rs:335-345): `Slice.get_subslice` of its bounds. -/
@[simp] def get_range (s : Slice α) (r : core.ops.Range Usize) : Option (Slice α) :=
  get_subslice s r.start r.«end»

end Blaze.Slice
