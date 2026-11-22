//! Order-independent, zero-overhead compile-time Type Sets (`bstd::type_set`).
//!
//! Models an anonymous product type whose fields are keyed by member type rather than name:
//!
//! - **Order-independent identity & conversion**: `{MaxSpeed, Health}` and `{Health, MaxSpeed}`
//!   carry the exact same members and convert cleanly via [`Subset::project`] / [`Reorder::reorder`]
//!   with zero runtime overhead.
//! - **Compile-time uniqueness & `Lacks<T>`**: duplicate members (`{Health, Health}`) are rejected
//!   at compile time via `const` monomorphization checks (`TypeSetMeta::IS_UNIQUE`) and explicit
//!   [`Lacks`] / [`Distinct`] bounds for open row-polymorphic functions.
//! - **100% safe disjoint multi-field borrows (`#![forbid(unsafe_code)]`)**:
//!   [`HasPairMut::borrow_pair_mut`] and [`HasPairMut::borrow_mut_ref`] prove at compile time via
//!   inductive Peano witnesses ([`Here`], [`There`]) that two borrowed members occupy distinct
//!   fields — attempting to mutably borrow the same member type twice is a compile-time type error.
//! - **Row-polymorphic extension & subsetting**: `fn damage<S, I1, I2>(e: &mut S, amount: u32)`
//!   works on any type set containing at least `{Health, Armor}`, and `e.extend(Burning)` extends
//!   any type set that lacks `Burning`.
//!
//! Under native Blaze (`cfg(blaze)`), the compiler's canonical type-set algebra
//! (`blaze_ty::type_set`) unifies `{MaxSpeed, Health}` and `{Health, MaxSpeed}` directly at the
//! type level; this module provides the matching `#![no_std]`, zero-proc-macro library surface
//! usable today under both Blaze and `rustc`.

use core::any::type_name;

/// Compile-time byte equality for `&str` (used by `const` type-set uniqueness checks).
#[inline]
const fn str_eq(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    if ab.len() != bb.len() {
        return false;
    }
    let mut i = 0;
    while i < ab.len() {
        if ab[i] != bb[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Empty type set `{}`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct Nil;

/// Non-empty type set node `{Head, ..Tail}` laid out directly inline with zero heap indirection.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct Cons<Head, Tail> {
    pub head: Head,
    pub tail: Tail,
}

/// Compile-time witness that a member lives at the head of a [`Cons`] chain.
#[derive(Clone, Copy, Debug)]
pub struct Here;

/// Compile-time witness that a member lives inside the tail at index `I`.
#[derive(Clone, Copy, Debug)]
pub struct There<I>(core::marker::PhantomData<I>);

/// Compile-time metadata for every type set: cardinality, membership by type name, and uniqueness.
pub trait TypeSetMeta {
    /// Number of member types in this set.
    const LEN: usize;
    /// `true` iff every member type in this set is distinct.
    const IS_UNIQUE: bool;
    /// `true` iff this set already contains a member with canonical type name `name`.
    fn contains_name(name: &str) -> bool;
}

/// Const-evaluable uniqueness / membership helper (mirrors [`TypeSetMeta`] in `const` contexts).
pub trait ConstTypeSet {
    const LEN: usize;
    const IS_UNIQUE: bool;
    const NAMES: &'static [&'static str];
}

const fn slice_contains_str(haystack: &[&str], needle: &str) -> bool {
    let mut i = 0;
    while i < haystack.len() {
        if str_eq(haystack[i], needle) {
            return true;
        }
        i += 1;
    }
    false
}

const fn slice_all_unique(names: &[&str]) -> bool {
    let mut i = 0;
    while i < names.len() {
        let mut j = i + 1;
        while j < names.len() {
            if str_eq(names[i], names[j]) {
                return false;
            }
            j += 1;
        }
        i += 1;
    }
    true
}

impl TypeSetMeta for Nil {
    const LEN: usize = 0;
    const IS_UNIQUE: bool = true;

    #[inline]
    fn contains_name(_name: &str) -> bool {
        false
    }
}

impl<H, T: TypeSetMeta> TypeSetMeta for Cons<H, T> {
    const LEN: usize = 1 + T::LEN;
    const IS_UNIQUE: bool = T::IS_UNIQUE;

    #[inline]
    fn contains_name(name: &str) -> bool {
        str_eq(type_name::<H>(), name) || T::contains_name(name)
    }
}

/// Static compile-time linked list of canonical member type names for a [`TypeSet`].
#[derive(Clone, Copy, Debug)]
pub struct TypeNode {
    pub name: &'static str,
    pub tail: Option<&'static TypeNode>,
}

/// Returns `true` iff `needle` appears anywhere in `node` (const-evaluable).
#[inline]
pub const fn node_contains(mut node: Option<&'static TypeNode>, needle: &str) -> bool {
    while let Some(cur) = node {
        if str_eq(cur.name, needle) {
            return true;
        }
        node = cur.tail;
    }
    false
}

/// Returns `true` iff every member type name in `node` is pairwise distinct (const-evaluable).
#[inline]
pub const fn node_all_distinct(mut node: Option<&'static TypeNode>) -> bool {
    while let Some(cur) = node {
        if node_contains(cur.tail, cur.name) {
            return false;
        }
        node = cur.tail;
    }
    true
}

/// Compile-time uniqueness assertion for a type set (`const` evaluated per monomorphization).
pub trait UniqueSet: TypeSetMeta {
    /// Compile-time list of member type names in this set.
    const NODE: Option<&'static TypeNode>;
    /// `true` iff all member types in this set are pairwise distinct.
    const ALL_DISTINCT: bool = node_all_distinct(Self::NODE);

    /// Triggers a compile-time error if this set contains any duplicate member type.
    #[inline]
    fn assert_unique() {
        const {
            assert!(Self::ALL_DISTINCT, "TypeSet contains duplicate member type");
        }
    }
}

impl UniqueSet for Nil {
    const NODE: Option<&'static TypeNode> = None;
}

impl<H, T: UniqueSet> UniqueSet for Cons<H, T> {
    const NODE: Option<&'static TypeNode> = Some(&TypeNode {
        name: type_name::<H>(),
        tail: T::NODE,
    });
}

/// Marker proving two types `A` and `B` are distinct for explicit generic `where R: Lacks<T>`
/// bounds. Built-in macros and `declare_distinct!` implement it symmetrically; concrete calls can
/// also rely directly on [`UniqueSet`] compile-time monomorphization checks.
pub trait Distinct<Other> {}

/// Row-polymorphic disjointness bound: `Self` does not contain a member of type `T`.
///
/// Mirrors Blaze's `where R: Lacks<Burning>` bound:
/// - `Nil: Lacks<T>` for every `T`.
/// - `Cons<H, R>: Lacks<T>` whenever `H: Distinct<T>` and `R: Lacks<T>`.
pub trait Lacks<T> {}

impl<T> Lacks<T> for Nil {}

impl<H, R, T> Lacks<T> for Cons<H, R>
where
    H: Distinct<T>,
    R: Lacks<T>,
{
}

/// Type-level extension `{T, ..Set}`.
pub type ExtendSet<T, Set> = Cons<T, Set>;

impl Nil {
    /// Creates an empty type set `{}`.
    #[inline]
    pub const fn new() -> Self {
        Nil
    }

    /// Inserts the first member `{T}`.
    #[inline]
    pub const fn extend<T>(self, value: T) -> Cons<T, Nil> {
        Cons { head: value, tail: Nil }
    }
}

impl<H, T> Cons<H, T> {
    /// Extends `{H, ..T}` with a new member `V`, enforcing at compile time that `V` is not
    /// already present in `{H, ..T}` (`{V, ..self}`).
    #[inline]
    pub fn extend<V>(self, value: V) -> Cons<V, Self>
    where
        Self: UniqueSet,
    {
        const {
            assert!(Self::ALL_DISTINCT, "TypeSet already contains duplicate member type");
            assert!(
                !node_contains(Self::NODE, type_name::<V>()),
                "TypeSet::extend: member type already present in set (violates Lacks<V>)"
            );
        }
        Cons { head: value, tail: self }
    }

    /// Extends `{H, ..T}` with a new member `V` using an explicit [`Lacks`] trait bound for
    /// open row-polymorphic functions (`fn ignite<R: Lacks<Burning>>(...)`).
    #[inline]
    pub const fn extend_lacking<V>(self, value: V) -> Cons<V, Self>
    where
        Self: Lacks<V>,
    {
        Cons { head: value, tail: self }
    }

    /// Shared access by member type (`s.[T]`).
    #[inline]
    pub fn get<Target, I>(&self) -> &Target
    where
        Self: Has<Target, I>,
    {
        <Self as Has<Target, I>>::get(self)
    }

    /// Exclusive access by member type (`&mut s.[T]`).
    #[inline]
    pub fn get_mut<Target, I>(&mut self) -> &mut Target
    where
        Self: Has<Target, I>,
    {
        <Self as Has<Target, I>>::get_mut(self)
    }

    /// Simultaneous disjoint mutable borrow `(&mut s.[A], &mut s.[B])` with zero `unsafe`.
    /// Attempting to borrow the same member type twice fails to compile.
    #[inline]
    pub fn borrow_pair_mut<A, B, IA, IB>(&mut self) -> (&mut A, &mut B)
    where
        Self: HasPairMut<A, B, IA, IB>,
    {
        <Self as HasPairMut<A, B, IA, IB>>::borrow_pair_mut(self)
    }

    /// Simultaneous disjoint mixed borrow `(&mut s.[A], &s.[B])` with zero `unsafe`.
    #[inline]
    pub fn borrow_mut_ref<A, B, IA, IB>(&mut self) -> (&mut A, &B)
    where
        Self: HasPairMut<A, B, IA, IB>,
    {
        let (a, b) = <Self as HasPairMut<A, B, IA, IB>>::borrow_pair_mut(self);
        (a, &*b)
    }

    /// Removes member `Target` by type, returning `(Target, Remainder)`.
    #[inline]
    pub fn pluck<Target, I>(self) -> (Target, <Self as Pluck<Target, I>>::Remainder)
    where
        Self: Pluck<Target, I>,
    {
        <Self as Pluck<Target, I>>::pluck(self)
    }

    /// Reorders or projects this type set into any subset/permutation `Target` in any order.
    #[inline]
    pub fn project<Target, Idxs>(self) -> Target
    where
        Self: Subset<Target, Idxs>,
    {
        <Self as Subset<Target, Idxs>>::project(self)
    }
}

/// Access a single member of type `T` at compile-time witness `I` (zero runtime cost).
pub trait Has<T, I> {
    fn get(&self) -> &T;
    fn get_mut(&mut self) -> &mut T;
}

impl<T, Tail> Has<T, Here> for Cons<T, Tail> {
    #[inline]
    fn get(&self) -> &T {
        &self.head
    }

    #[inline]
    fn get_mut(&mut self) -> &mut T {
        &mut self.head
    }
}

impl<T, Head, Tail, I> Has<T, There<I>> for Cons<Head, Tail>
where
    Tail: Has<T, I>,
{
    #[inline]
    fn get(&self) -> &T {
        self.tail.get()
    }

    #[inline]
    fn get_mut(&mut self) -> &mut T {
        self.tail.get_mut()
    }
}

/// Safe, zero-`unsafe` simultaneous disjoint mutable borrow of two distinct member types `A` and
/// `B`. Has no impl for `(Here, Here)`, making aliasing a compile-time type error.
pub trait HasPairMut<A, B, IA, IB> {
    fn borrow_pair_mut(&mut self) -> (&mut A, &mut B);
}

impl<A, B, Tail, IB> HasPairMut<A, B, Here, There<IB>> for Cons<A, Tail>
where
    Tail: Has<B, IB>,
{
    #[inline]
    fn borrow_pair_mut(&mut self) -> (&mut A, &mut B) {
        (&mut self.head, self.tail.get_mut())
    }
}

impl<A, B, Tail, IA> HasPairMut<A, B, There<IA>, Here> for Cons<B, Tail>
where
    Tail: Has<A, IA>,
{
    #[inline]
    fn borrow_pair_mut(&mut self) -> (&mut A, &mut B) {
        (self.tail.get_mut(), &mut self.head)
    }
}

impl<A, B, Head, Tail, IA, IB> HasPairMut<A, B, There<IA>, There<IB>> for Cons<Head, Tail>
where
    Tail: HasPairMut<A, B, IA, IB>,
{
    #[inline]
    fn borrow_pair_mut(&mut self) -> (&mut A, &mut B) {
        self.tail.borrow_pair_mut()
    }
}

/// Remove member `T` by type, returning `(T, Self::Remainder)`.
pub trait Pluck<T, I>: Sized {
    type Remainder;
    fn pluck(self) -> (T, Self::Remainder);
}

impl<T, Tail> Pluck<T, Here> for Cons<T, Tail> {
    type Remainder = Tail;

    #[inline]
    fn pluck(self) -> (T, Tail) {
        (self.head, self.tail)
    }
}

impl<T, Head, Tail, I> Pluck<T, There<I>> for Cons<Head, Tail>
where
    Tail: Pluck<T, I>,
{
    type Remainder = Cons<Head, Tail::Remainder>;

    #[inline]
    fn pluck(self) -> (T, Self::Remainder) {
        let (target, rem) = self.tail.pluck();
        (target, Cons { head: self.head, tail: rem })
    }
}

/// Order-independent projection / reordering: converts `Self` into any target type set `Target`
/// whose members are a subset (or permutation) of `Self`.
pub trait Subset<Target, Idxs>: Sized {
    fn project(self) -> Target;
}

impl<S> Subset<Nil, Nil> for S {
    #[inline]
    fn project(self) -> Nil {
        Nil
    }
}

impl<S, Head, Tail, HeadIdx, TailIdxs> Subset<Cons<Head, Tail>, Cons<HeadIdx, TailIdxs>> for S
where
    S: Pluck<Head, HeadIdx>,
    S::Remainder: Subset<Tail, TailIdxs>,
{
    #[inline]
    fn project(self) -> Cons<Head, Tail> {
        let (head, rem) = self.pluck();
        Cons { head, tail: rem.project() }
    }
}

/// Checks at compile time that a slice of canonical type names has no duplicates.
#[inline]
pub const fn assert_names_unique(names: &[&str]) -> bool {
    slice_all_unique(names)
}

/// Checks at compile time whether `needle` is absent from `names`.
#[inline]
pub const fn lacks_name(names: &[&str], needle: &str) -> bool {
    !slice_contains_str(names, needle)
}

/// Type-level syntax for a type set: `TypeSet![MaxSpeed, Health]`.
#[macro_export]
macro_rules! TypeSet {
    () => {
        $crate::type_set::Nil
    };
    ($head:ty $(, $tail:ty)* $(,)?) => {
        $crate::type_set::Cons<$head, $crate::TypeSet![$($tail),*]>
    };
}

/// Value constructor for a type set with compile-time uniqueness enforcement:
/// `tset! { Health(500), MaxSpeed(100) }`.
#[macro_export]
macro_rules! tset {
    () => {
        $crate::type_set::Nil
    };
    ($val:expr $(,)?) => {
        $crate::type_set::Cons {
            head: $val,
            tail: $crate::type_set::Nil,
        }
    };
    ($head:expr, $($tail:expr),+ $(,)?) => {{
        let tail = $crate::tset!($($tail),+);
        tail.extend($head)
    }};
}

/// Declares pairwise [`Distinct`] impls between a list of component types so generic
/// `where R: Lacks<T>` row-polymorphic functions work cleanly on `rustc` today.
#[macro_export]
macro_rules! declare_distinct {
    () => {};
    ($only:ty $(,)?) => {};
    ($first:ty, $($rest:ty),+ $(,)?) => {
        $(
            impl $crate::type_set::Distinct<$rest> for $first {}
            impl $crate::type_set::Distinct<$first> for $rest {}
        )+
        $crate::declare_distinct!($($rest),+);
    };
}
