//! Explicit zero-cost newtypes, affine spaces (`ByteOffset` vs `ByteLen` + `ByteSpan`),
//! Kotlin-style selective delegation (`newtype!`), and **`TypeSet!`-powered unit/invariant sets**.
//!
//! # Why `TypeSet!` + Newtypes + Units fit together
//!
//! 1. **Order-independent typed bundles (`TypeSet![ByteOffset, ByteLen, Alignment]`)**:
//!    Because every quantity is its own distinct newtype (`ByteOffset` ≠ `ByteLen`), any
//!    `bstd::TypeSet!` of newtypes is unique by construction and order-independent. Functions
//!    write row-polymorphic bounds (`S: Has<ByteOffset> + Has<ByteLen>`) instead of positional
//!    `(u64, u64)` tuples — swapping arguments or adding extra fields is impossible to miswire.
//! 2. **Zero-overhead ZST invariant/tag sets (`Tagged<T, Tags>`)**:
//!    Attach an order-independent `bstd::TypeSet!` of zero-sized property/unit tags (e.g.
//!    `TypeSet![Aligned, Confined]`) to any value `T` with `size_of::<Tagged<T, Tags>>() == size_of::<T>()`.
//!    Callers can project/reorder tags (`narrow`), extend with new proven tags (`tag`), and require
//!    only the tags they care about (`Tags: Has<Aligned>`).
//! 3. **Extensible unit-exponent sets (`Dim<Unit, const EXP: i8>`)**:
//!    A plain set deduplicates `{Metre, Metre}`, whereas dimensional analysis needs signed exponents
//!    (`m^2`, `s^-1`). Storing `Dim<Unit, const EXP: i8>` inside a `TypeSet!` (e.g.
//!    `TypeSet![Dim<Metre, 1>, Dim<Second, -1>]`) gives an **open, extensible, order-independent**
//!    dimension signature where each base unit appears at most once and any crate can introduce
//!    new base units (`Byte`, `Packet`, `Token`, `Pixel`, `Metre`, `Second`) with zero central table.

use core::marker::PhantomData;

pub use blaze_kit::newtype::{
    Alignment, ByteLen, ByteOffset, ByteSpan, Capacity, ColNo, Count, LineNo, Port, Stride,
    TextLen, TextOffset, TextSpan,
};
pub use blaze_kit::newtype;

use crate::type_set::{Cons, Has, Subset, UniqueSet};

/// Extensible base-unit exponent marker for `TypeSet!`-based dimensional analysis, e.g.
/// `TypeSet![Dim<Metre, 1>, Dim<Second, -1>]` (zero-sized, order-independent, unique per unit).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct Dim<Unit, const EXP: i8>(PhantomData<Unit>);

impl<Unit, const EXP: i8> Dim<Unit, EXP> {
    pub const UNIT: Self = Self(PhantomData);
    pub const EXPONENT: i8 = EXP;
}

/// Zero-overhead value `T` carrying an order-independent compile-time `TypeSet!` of ZST
/// invariants, unit dimensions, or domain tags (`size_of::<Tagged<T, Tags>>() == size_of::<T>()`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Tagged<T, Tags: UniqueSet> {
    value: T,
    _tags: PhantomData<Tags>,
}

impl<T, Tags: UniqueSet> Tagged<T, Tags> {
    /// Wrap `value` with compile-time uniqueness check on `Tags`.
    #[inline(always)]
    pub fn new(value: T) -> Self {
        <Tags as UniqueSet>::assert_unique();
        Self { value, _tags: PhantomData }
    }

    #[inline(always)]
    pub const fn get(&self) -> &T {
        &self.value
    }

    #[inline(always)]
    pub fn into_inner(self) -> T {
        self.value
    }

    /// Reorder or drop tags into any subset/permutation `Target` (order-independent!).
    ///
    /// Example: `Tagged<ByteOffset, TypeSet![Aligned, Confined]>` narrows cleanly into
    /// `Tagged<ByteOffset, TypeSet![Confined, Aligned]>` or `Tagged<ByteOffset, TypeSet![Aligned]>`.
    #[inline(always)]
    pub fn narrow<Target, Indices>(self) -> Tagged<T, Target>
    where
        Target: UniqueSet,
        Tags: Subset<Target, Indices>,
    {
        <Target as UniqueSet>::assert_unique();
        Tagged { value: self.value, _tags: PhantomData }
    }

    /// Attach a newly proven ZST tag `NewTag` (refused at compile time if `Tags` already holds it).
    #[inline(always)]
    pub fn with_tag<NewTag>(self) -> Tagged<T, Cons<NewTag, Tags>>
    where
        Cons<NewTag, Tags>: UniqueSet,
    {
        <Cons<NewTag, Tags> as UniqueSet>::assert_unique();
        Tagged { value: self.value, _tags: PhantomData }
    }

    /// Compile-time proof helper: requires `Tags` to contain `RequiredTag` at any position.
    #[inline(always)]
    pub const fn require<RequiredTag, Idx>(&self) -> &T
    where
        Tags: Has<RequiredTag, Idx>,
    {
        &self.value
    }
}

impl<Tags: UniqueSet> Tagged<f64, Tags> {
    /// Add two dimensionally-compatible quantities (same `Tags` in any order via `Subset`).
    #[inline(always)]
    pub fn add_dim<OtherTags: UniqueSet, Indices>(self, rhs: Tagged<f64, OtherTags>) -> Self
    where
        OtherTags: Subset<Tags, Indices>,
    {
        let rhs_aligned: Self = rhs.narrow();
        Tagged::new(self.value + rhs_aligned.value)
    }

    /// Subtract two dimensionally-compatible quantities (same `Tags` in any order).
    #[inline(always)]
    pub fn sub_dim<OtherTags: UniqueSet, Indices>(self, rhs: Tagged<f64, OtherTags>) -> Self
    where
        OtherTags: Subset<Tags, Indices>,
    {
        let rhs_aligned: Self = rhs.narrow();
        Tagged::new(self.value - rhs_aligned.value)
    }

    /// Scale a dimensionally-tagged quantity by a dimensionless scalar.
    #[inline(always)]
    pub const fn scale(self, factor: f64) -> Self {
        Tagged { value: self.value * factor, _tags: PhantomData }
    }

    /// Multiply by an orthogonal unit `Dim<U, E>` (compile-time checked disjoint via `UniqueSet`).
    #[inline(always)]
    pub fn mul_unit<U, const E: i8>(
        self,
        rhs: Tagged<f64, Cons<Dim<U, E>, crate::type_set::Nil>>,
    ) -> Tagged<f64, Cons<Dim<U, E>, Tags>>
    where
        Cons<Dim<U, E>, crate::type_set::Nil>: UniqueSet,
        Cons<Dim<U, E>, Tags>: UniqueSet,
    {
        Tagged::new(self.value * rhs.value)
    }

    /// Divide by an orthogonal unit `Dim<U, 1>`, producing `Dim<U, -1>` in the result `TypeSet!`
    /// (e.g. `Distance / Time -> Velocity`).
    #[inline(always)]
    pub fn div_unit<U>(
        self,
        rhs: Tagged<f64, Cons<Dim<U, 1>, crate::type_set::Nil>>,
    ) -> Tagged<f64, Cons<Dim<U, -1>, Tags>>
    where
        Cons<Dim<U, 1>, crate::type_set::Nil>: UniqueSet,
        Cons<Dim<U, -1>, Tags>: UniqueSet,
    {
        Tagged::new(self.value / rhs.value)
    }
}

impl<T: core::fmt::Debug, Tags: UniqueSet> core::fmt::Debug for Tagged<T, Tags> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(&self.value, f)
    }
}

/// Row-polymorphic buffer slice using ANY `TypeSet!` containing both `ByteOffset` and `ByteLen`
/// (in any order, alongside any other typed metadata).
#[inline]
pub fn slice_from<'a, S, OffIdx, LenIdx>(set: &S, buf: &'a [u8]) -> Option<&'a [u8]>
where
    S: Has<ByteOffset, OffIdx> + Has<ByteLen, LenIdx>,
{
    let offset = *Has::<ByteOffset, OffIdx>::get(set);
    let len = *Has::<ByteLen, LenIdx>::get(set);
    offset.span(len).slice(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TypeSet;

    #[derive(Clone, Copy, Debug)]
    struct AlignedTag;
    #[derive(Clone, Copy, Debug)]
    struct ConfinedTag;
    #[derive(Clone, Copy, Debug)]
    struct Metre;
    #[derive(Clone, Copy, Debug)]
    struct Second;

    #[test]
    fn typeset_of_newtypes_is_order_independent_and_zero_overhead() {
        // Order A: {ByteOffset, ByteLen, Alignment}
        let a = crate::type_set::Nil
            .extend(Alignment::new(8).expect("8 is pow2"))
            .extend(ByteLen::new(5))
            .extend(ByteOffset::new(3));
        // Order B: {ByteLen, ByteOffset}
        let b = crate::type_set::Nil
            .extend(ByteOffset::new(3))
            .extend(ByteLen::new(5));

        let buf = b"012hello89";
        assert_eq!(slice_from(&a, buf), Some(&b"hello"[..]));
        assert_eq!(slice_from(&b, buf), Some(&b"hello"[..]));
    }

    #[test]
    fn tagged_typeset_is_zero_sized_and_supports_order_independent_narrowing() {
        type AlignedConfined = TypeSet![AlignedTag, ConfinedTag];
        type ConfinedAligned = TypeSet![ConfinedTag, AlignedTag];
        type OnlyAligned = TypeSet![AlignedTag];

        assert_eq!(
            core::mem::size_of::<Tagged<ByteOffset, AlignedConfined>>(),
            core::mem::size_of::<ByteOffset>()
        );

        let off: Tagged<ByteOffset, OnlyAligned> = Tagged::new(ByteOffset::new(64));
        let both: Tagged<ByteOffset, ConfinedAligned> = off.with_tag::<ConfinedTag>();
        let reordered: Tagged<ByteOffset, AlignedConfined> = both.narrow();
        let narrowed: Tagged<ByteOffset, OnlyAligned> = reordered.narrow();
        assert_eq!(*narrowed.require::<AlignedTag, _>(), ByteOffset::new(64));
    }

    #[test]
    fn extensible_dim_typeset_is_order_independent_and_zero_sized() {
        type Velocity = TypeSet![Dim<Metre, 1>, Dim<Second, -1>];
        type VelocitySwapped = TypeSet![Dim<Second, -1>, Dim<Metre, 1>];

        let speed: Tagged<f64, Velocity> = Tagged::new(9.81);
        assert_eq!(core::mem::size_of_val(&speed), core::mem::size_of::<f64>());
        let swapped: Tagged<f64, VelocitySwapped> = speed.narrow();
        assert_eq!(*swapped.require::<Dim<Metre, 1>, _>(), 9.81);
        assert_eq!(*swapped.require::<Dim<Second, -1>, _>(), 9.81);

        // Dimensional arithmetic: Distance / Time -> Velocity (order-independent `add_dim`).
        type Distance = TypeSet![Dim<Metre, 1>];
        type Time = TypeSet![Dim<Second, 1>];
        let d: Tagged<f64, Distance> = Tagged::new(100.0);
        let t: Tagged<f64, Time> = Tagged::new(10.0);
        let v: Tagged<f64, Velocity> = d.div_unit(t).narrow();
        let total_v: Tagged<f64, Velocity> = v.add_dim(swapped).scale(2.0);
        assert!((*total_v.require::<Dim<Metre, 1>, _>() - 39.62).abs() < 1e-12);
    }

    #[test]
    fn algebra_and_dim_typeset_compose_at_comptime_and_runtime() {
        use crate::algebra::{ConstExpr, ConstGraph};

        // Kinematics at compile time: x(t) = v0 * t + 0.5 * a * t^2
        // => v(t) = dx/dt = v0 + a * t  [m * s^-1]
        const VEL_EXPR: (ConstGraph<32>, ConstExpr) = {
            let mut g = ConstGraph::new();
            let t = g.var(0);
            let v0 = g.var(1);
            let a = g.var(2);
            let t2 = g.mul(t, t);
            let half = g.lit(0.5);
            let term1 = g.mul(v0, t);
            let half_a = g.mul(half, a);
            let term2 = g.mul(half_a, t2);
            let pos = g.add(term1, term2);
            let vel = g.diff(pos, 0);
            (g, vel)
        };
        const V_AT_2S: f64 = VEL_EXPR.0.eval_const(VEL_EXPR.1, &[2.0, 3.0, 9.8]);

        type Velocity = TypeSet![Dim<Metre, 1>, Dim<Second, -1>];
        let v: Tagged<f64, Velocity> = Tagged::new(V_AT_2S);
        assert!((*v.require::<Dim<Metre, 1>, _>() - 22.6).abs() < 1e-12);
    }
}
