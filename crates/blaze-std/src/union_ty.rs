//! Anonymous Union Types (`A | B | C`) with small local tags, by-value widening, and
//! type-safe narrowing (`bstd::union_ty`).
//!
//! Provides a `#![no_std]`, zero-`unsafe`, zero-proc-macro library surface mirroring Blaze's
//! native union-type algebra (`blaze_ty::union_ty`):
//!
//! - **Inline small-local-tag representation**: [`Union<H, T>`] / [`Never`] uses nested enum
//!   storage (zero heap allocation, `Never` tail elided by `rustc` discriminant optimization).
//! - **Exact fixed auto-traits & sealed extensibility**: [`Union<H, T>`] automatically implements
//!   `Debug`, `Display`, `core::error::Error`, `Clone`, `Copy`, `PartialEq`, `Eq`, `Hash` (plus
//!   auto traits `Send`, `Sync`, `Unpin`) whenever every member implements them. All core union
//!   traits are sealed via `private::Sealed`, preventing external crates from writing arbitrary
//!   union trait impls.
//! - **Compile-time uniqueness**: backed by [`crate::type_set::UniqueSet`] and
//!   [`crate::type_set::TypeNode`] so duplicate members (including lifetime-erased duplicates
//!   like `&'a str` and `&'b str`) fail compile-time `const` assertions.
//! - **By-value widening only**: [`Union::widen`] re-tags any subset union into any superset or
//!   reordered union (`Union![A, B]` -> `Union![C, A, B]` or `Union![B, A]`) by value with zero
//!   heap allocation; reference widening (`&Union![A, B]` -> `&Union![A, B, C]`) is intentionally
//!   not provided.
//! - **Type-safe narrowing**: [`Union::narrow`] extracts one member `M` and returns the narrowed
//!   remainder union `Result<M, Remainder>` for ergonomic error propagation
//!   (`match err.narrow::<IoErr, _>() { Ok(io) => handle(io), Err(rest) => return Err(rest.widen()) }`).
//! - **Declarative macros**: [`crate::Union!`], [`crate::union_val!`], and [`crate::define_union!`].

use core::any::type_name;
use core::fmt;

pub use crate::type_set::{
    Cons, Here, Nil, There, TypeNode, TypeSetMeta, UniqueSet, assert_names_unique,
    node_all_distinct, node_contains,
};

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

mod private {
    pub trait Sealed {}
    impl Sealed for super::Never {}
    impl<H, T> Sealed for super::Union<H, T> {}
}

/// Uninhabited tail of a [`Union`] sum-type chain (`!`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Never {}

impl Never {
    /// Converts an uninhabited [`Never`] value into any type (proves exhaustive narrowing).
    #[inline]
    pub const fn absurd<R>(self) -> R {
        match self {}
    }
}

impl fmt::Display for Never {
    #[inline]
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {}
    }
}

impl core::error::Error for Never {}

/// Inline anonymous union node `H | T` (`Either`-style small-local-tag representation, zero heap
/// allocation).
///
/// Automatically implements `Debug`, `Display`, `core::error::Error`, `Clone`, `Copy`,
/// `PartialEq`, `Eq`, `Hash`, `Send`, `Sync`, and `Unpin` whenever every member implements them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Union<H, T> {
    /// Value belongs to the head member type `H`.
    Head(H),
    /// Value belongs to one of the remaining member types in `T`.
    Tail(T),
}

impl<H: fmt::Display, T: fmt::Display> fmt::Display for Union<H, T> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Union::Head(h) => fmt::Display::fmt(h, f),
            Union::Tail(t) => fmt::Display::fmt(t, f),
        }
    }
}

impl<H: core::error::Error, T: core::error::Error> core::error::Error for Union<H, T> {
    #[inline]
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Union::Head(h) => h.source(),
            Union::Tail(t) => t.source(),
        }
    }
}

impl TypeSetMeta for Never {
    const LEN: usize = 0;
    const IS_UNIQUE: bool = true;

    #[inline]
    fn contains_name(_name: &str) -> bool {
        false
    }
}

impl<H, T: TypeSetMeta> TypeSetMeta for Union<H, T> {
    const LEN: usize = 1 + T::LEN;
    const IS_UNIQUE: bool = T::IS_UNIQUE;

    #[inline]
    fn contains_name(name: &str) -> bool {
        str_eq(type_name::<H>(), name) || T::contains_name(name)
    }
}

impl UniqueSet for Never {
    const NODE: Option<&'static TypeNode> = None;
}

impl<H, T: UniqueSet> UniqueSet for Union<H, T> {
    const NODE: Option<&'static TypeNode> = Some(&TypeNode {
        name: type_name::<H>(),
        tail: T::NODE,
    });
}

/// Sealed trait for injecting and inspecting a member `T` at compile-time witness `I`.
pub trait Inject<T, I>: private::Sealed + Sized {
    /// Wraps `value` into this union at position `I`.
    fn inject(value: T) -> Self;
    /// Returns `Some(&T)` iff this union currently holds member `T`.
    fn as_member(&self) -> Option<&T>;
    /// Returns `Some(&mut T)` iff this union currently holds member `T`.
    fn as_member_mut(&mut self) -> Option<&mut T>;
}

impl<T, Tail> Inject<T, Here> for Union<T, Tail> {
    #[inline]
    fn inject(value: T) -> Self {
        Union::Head(value)
    }

    #[inline]
    fn as_member(&self) -> Option<&T> {
        match self {
            Union::Head(v) => Some(v),
            Union::Tail(_) => None,
        }
    }

    #[inline]
    fn as_member_mut(&mut self) -> Option<&mut T> {
        match self {
            Union::Head(v) => Some(v),
            Union::Tail(_) => None,
        }
    }
}

impl<T, Head, Tail, I> Inject<T, There<I>> for Union<Head, Tail>
where
    Tail: Inject<T, I>,
{
    #[inline]
    fn inject(value: T) -> Self {
        Union::Tail(Tail::inject(value))
    }

    #[inline]
    fn as_member(&self) -> Option<&T> {
        match self {
            Union::Head(_) => None,
            Union::Tail(t) => t.as_member(),
        }
    }

    #[inline]
    fn as_member_mut(&mut self) -> Option<&mut T> {
        match self {
            Union::Head(_) => None,
            Union::Tail(t) => t.as_member_mut(),
        }
    }
}

/// Sealed trait for type-safe narrowing: extracts member `M` at witness `I`, returning either
/// `Ok(M)` or `Err(Self::Remainder)` with `M` removed from the union type.
pub trait Narrow<M, I>: private::Sealed + Sized {
    type Remainder;
    fn narrow(self) -> Result<M, Self::Remainder>;
}

impl<M, Tail> Narrow<M, Here> for Union<M, Tail> {
    type Remainder = Tail;

    #[inline]
    fn narrow(self) -> Result<M, Tail> {
        match self {
            Union::Head(m) => Ok(m),
            Union::Tail(rest) => Err(rest),
        }
    }
}

impl<M, Head, Tail, I> Narrow<M, There<I>> for Union<Head, Tail>
where
    Tail: Narrow<M, I>,
{
    type Remainder = Union<Head, Tail::Remainder>;

    #[inline]
    fn narrow(self) -> Result<M, Self::Remainder> {
        match self {
            Union::Head(h) => Err(Union::Head(h)),
            Union::Tail(t) => match t.narrow() {
                Ok(m) => Ok(m),
                Err(rem) => Err(Union::Tail(rem)),
            },
        }
    }
}

/// Sealed trait for order-independent, by-value widening (`Union![A, B]` -> `Union![C, A, B]`
/// or `Union![B, A]`) with zero heap allocation and no reference widening.
pub trait Widen<Target, Idxs>: private::Sealed + Sized {
    fn widen(self) -> Target;
}

impl<Target: private::Sealed> Widen<Target, Nil> for Never {
    #[inline]
    fn widen(self) -> Target {
        match self {}
    }
}

impl<H, T, Target, HeadIdx, TailIdxs> Widen<Target, Cons<HeadIdx, TailIdxs>> for Union<H, T>
where
    Target: Inject<H, HeadIdx>,
    T: Widen<Target, TailIdxs>,
{
    #[inline]
    fn widen(self) -> Target {
        match self {
            Union::Head(h) => Target::inject(h),
            Union::Tail(t) => t.widen(),
        }
    }
}

/// Bridge between a concrete [`crate::define_union!`] enum and its structural [`Union`] type,
/// allowing `widen_into` to target either a [`Union`] or another `define_union!` enum.
pub trait UnionRepr: Sized {
    type Structural: UniqueSet;
    fn into_union(self) -> Self::Structural;
    fn from_union(u: Self::Structural) -> Self;
}

impl UnionRepr for Never {
    type Structural = Never;

    #[inline]
    fn into_union(self) -> Self::Structural {
        self
    }

    #[inline]
    fn from_union(u: Self::Structural) -> Self {
        u
    }
}

impl<H, T> UnionRepr for Union<H, T>
where
    Self: UniqueSet,
{
    type Structural = Self;

    #[inline]
    fn into_union(self) -> Self::Structural {
        self
    }

    #[inline]
    fn from_union(u: Self::Structural) -> Self {
        u
    }
}

impl<T> Union<T, Never> {
    /// Extracts the sole remaining member of a 1-member union infallibly.
    #[inline]
    pub fn into_inner(self) -> T {
        match self {
            Union::Head(v) => v,
            Union::Tail(never) => never.absurd(),
        }
    }
}

impl<H, T> Union<H, T> {
    /// Constructs this union from any member `M`, enforcing compile-time member uniqueness.
    #[inline]
    pub fn from_member<M, I>(value: M) -> Self
    where
        Self: Inject<M, I> + UniqueSet,
    {
        <Self as UniqueSet>::assert_unique();
        <Self as Inject<M, I>>::inject(value)
    }

    /// Returns `true` iff this union currently holds a value of member type `M`.
    #[inline]
    pub fn is<M, I>(&self) -> bool
    where
        Self: Inject<M, I> + UniqueSet,
    {
        <Self as UniqueSet>::assert_unique();
        <Self as Inject<M, I>>::as_member(self).is_some()
    }

    /// Borrows the active value if it has member type `M`.
    #[inline]
    pub fn get<M, I>(&self) -> Option<&M>
    where
        Self: Inject<M, I> + UniqueSet,
    {
        <Self as UniqueSet>::assert_unique();
        <Self as Inject<M, I>>::as_member(self)
    }

    /// Mutably borrows the active value if it has member type `M`.
    #[inline]
    pub fn get_mut<M, I>(&mut self) -> Option<&mut M>
    where
        Self: Inject<M, I> + UniqueSet,
    {
        <Self as UniqueSet>::assert_unique();
        <Self as Inject<M, I>>::as_member_mut(self)
    }

    /// Type-safe narrowing: extracts member `M` if active, or returns the narrowed remainder
    /// union with `M` removed (`Result<M, Remainder>`).
    #[inline]
    pub fn narrow<M, I>(self) -> Result<M, <Self as Narrow<M, I>>::Remainder>
    where
        Self: Narrow<M, I> + UniqueSet,
    {
        <Self as UniqueSet>::assert_unique();
        <Self as Narrow<M, I>>::narrow(self)
    }

    /// By-value widening and order-independent re-tagging into any superset or reordered union
    /// `Target` (zero heap allocation; no reference widening).
    #[inline]
    pub fn widen<Target, Idxs>(self) -> Target
    where
        Self: Widen<Target, Idxs> + UniqueSet,
        Target: UniqueSet,
    {
        <Self as UniqueSet>::assert_unique();
        <Target as UniqueSet>::assert_unique();
        <Self as Widen<Target, Idxs>>::widen(self)
    }

    /// By-value widening into any [`UnionRepr`] target (either a [`Union`] or a concrete
    /// [`crate::define_union!`] enum).
    #[inline]
    pub fn widen_into<Target, Idxs>(self) -> Target
    where
        Target: UnionRepr,
        Self: Widen<Target::Structural, Idxs> + UniqueSet,
    {
        <Self as UniqueSet>::assert_unique();
        <Target::Structural as UniqueSet>::assert_unique();
        Target::from_union(<Self as Widen<Target::Structural, Idxs>>::widen(self))
    }
}

/// Type-level syntax for an anonymous union: `Union![A, B, C]`.
#[macro_export]
macro_rules! Union {
    () => {
        $crate::union_ty::Never
    };
    ($head:ty $(, $tail:ty)* $(,)?) => {
        $crate::union_ty::Union<$head, $crate::Union![$($tail),*]>
    };
}

/// Injects a value into an anonymous union type with compile-time uniqueness enforcement:
/// - `union_val!(expr)` (infers the union type from context)
/// - `union_val!(expr => Union![A, B, C])` (specifies the union type explicitly)
#[macro_export]
macro_rules! union_val {
    ($val:expr) => {
        $crate::union_ty::Union::from_member($val)
    };
    ($val:expr => $target:ty) => {
        <$target>::from_member($val)
    };
}

/// Internal helper for [`crate::define_union!`]: unwraps a structural [`Union`] chain into a
/// concrete enum variant.
#[doc(hidden)]
#[macro_export]
macro_rules! __blaze_union_from_structural {
    ($name:ident, $u:expr,) => {
        match $u {}
    };
    ($name:ident, $u:expr, $head_var:ident $(, $tail_var:ident)*) => {
        match $u {
            $crate::union_ty::Union::Head(v) => $name::$head_var(v),
            $crate::union_ty::Union::Tail(rest) => {
                $crate::__blaze_union_from_structural!($name, rest, $($tail_var),*)
            }
        }
    };
}

/// Internal helper for [`crate::define_union!`]: generates typed `narrow_*` methods returning
/// `Result<Member, Union![Remaining...]>` for each variant.
#[doc(hidden)]
#[macro_export]
macro_rules! __blaze_union_narrow_methods {
    (
        $vis:vis,
        $name:ident,
        prev: [$($prev_var:ident($prev_ty:ty)),*],
        rest: []
    ) => {};
    (
        $vis:vis,
        $name:ident,
        prev: [$($prev_var:ident($prev_ty:ty)),*],
        rest: [$cur_var:ident($cur_ty:ty) => $cur_fn:ident $(, $next_var:ident($next_ty:ty) => $next_fn:ident)*]
    ) => {
        #[allow(dead_code)]
        #[inline]
        $vis fn $cur_fn(
            self,
        ) -> core::result::Result<$cur_ty, $crate::Union![$($prev_ty,)* $($next_ty),*]> {
            match self {
                Self::$cur_var(v) => core::result::Result::Ok(v),
                $(
                    Self::$prev_var(v) => core::result::Result::Err(
                        $crate::union_ty::Union::from_member(v),
                    ),
                )*
                $(
                    Self::$next_var(v) => core::result::Result::Err(
                        $crate::union_ty::Union::from_member(v),
                    ),
                )*
            }
        }

        $crate::__blaze_union_narrow_methods!(
            $vis,
            $name,
            prev: [$($prev_var($prev_ty),)* $cur_var($cur_ty)],
            rest: [$($next_var($next_ty) => $next_fn),*]
        );
    };
}

/// Defines a concrete small-local-tag union `enum` with:
/// - Compile-time member uniqueness check (`assert_names_unique`)
/// - Exact fixed trait set: `Debug`, `Display`, `core::error::Error`, `Clone`, `Copy`,
///   `PartialEq`, `Eq`, `Hash` (plus `Send`, `Sync`, `Unpin` automatically)
/// - `From<Member>` for each unique member type
/// - `into_union`, `from_union`, `narrow::<M, _>`, optional per-variant `narrow_*` methods, and
///   by-value `widen_into` (targeting either a `Union![...]` or another `define_union!` enum).
///
/// Prefix with `value` instead of omitting it when members do not implement `core::error::Error`.
///
/// # Examples
/// ```ignore
/// define_union! {
///     pub enum AppErr {
///         Io(IoErr) => narrow_io,
///         Parse(ParseErr) => narrow_parse,
///         Timeout(TimeoutErr) => narrow_timeout,
///     }
/// }
/// ```
#[macro_export]
macro_rules! define_union {
    // Error union with explicit `narrow_*` method names.
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($variant:ident($ty:ty) => $narrow_fn:ident),+ $(,)?
        }
    ) => {
        $crate::define_union! {
            @core
            $(#[$meta])*
            $vis enum $name {
                $($variant($ty)),+
            }
        }

        impl core::error::Error for $name {
            #[inline]
            fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
                match self {
                    $(Self::$variant(v) => core::error::Error::source(v)),+
                }
            }
        }

        impl $name {
            $crate::__blaze_union_narrow_methods!(
                $vis,
                $name,
                prev: [],
                rest: [$($variant($ty) => $narrow_fn),+]
            );
        }
    };

    // Error union without explicit `narrow_*` method names (still provides generic `narrow`).
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($variant:ident($ty:ty)),+ $(,)?
        }
    ) => {
        $crate::define_union! {
            @core
            $(#[$meta])*
            $vis enum $name {
                $($variant($ty)),+
            }
        }

        impl core::error::Error for $name {
            #[inline]
            fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
                match self {
                    $(Self::$variant(v) => core::error::Error::source(v)),+
                }
            }
        }
    };

    // Value union (without `core::error::Error`) with explicit `narrow_*` method names.
    (
        $(#[$meta:meta])*
        value $vis:vis enum $name:ident {
            $($variant:ident($ty:ty) => $narrow_fn:ident),+ $(,)?
        }
    ) => {
        $crate::define_union! {
            @core
            $(#[$meta])*
            $vis enum $name {
                $($variant($ty)),+
            }
        }

        impl $name {
            $crate::__blaze_union_narrow_methods!(
                $vis,
                $name,
                prev: [],
                rest: [$($variant($ty) => $narrow_fn),+]
            );
        }
    };

    // Value union (without `core::error::Error`) without explicit `narrow_*` method names.
    (
        $(#[$meta:meta])*
        value $vis:vis enum $name:ident {
            $($variant:ident($ty:ty)),+ $(,)?
        }
    ) => {
        $crate::define_union! {
            @core
            $(#[$meta])*
            $vis enum $name {
                $($variant($ty)),+
            }
        }
    };

    // Shared core expansion.
    (
        @core
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($variant:ident($ty:ty)),+
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        $vis enum $name {
            $($variant($ty)),+
        }

        const _: () = {
            assert!(
                $crate::union_ty::assert_names_unique(&[
                    $(core::any::type_name::<$ty>()),+
                ]),
                "define_union!: duplicate member type in union definition"
            );
        };

        impl core::fmt::Display for $name {
            #[inline]
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                match self {
                    $(Self::$variant(v) => core::fmt::Display::fmt(v, f)),+
                }
            }
        }

        $(
            impl core::convert::From<$ty> for $name {
                #[inline]
                fn from(value: $ty) -> Self {
                    Self::$variant(value)
                }
            }
        )+

        impl $crate::union_ty::UnionRepr for $name {
            type Structural = $crate::Union![$($ty),+];

            #[inline]
            fn into_union(self) -> Self::Structural {
                match self {
                    $(Self::$variant(v) => $crate::union_ty::Union::from_member(v)),+
                }
            }

            #[inline]
            fn from_union(u: Self::Structural) -> Self {
                $crate::__blaze_union_from_structural!($name, u, $($variant),+)
            }
        }

        #[allow(dead_code)]
        impl $name {
            /// Converts this concrete union enum into its structural `Union![...]` form.
            #[inline]
            $vis fn into_union(self) -> $crate::Union![$($ty),+] {
                <Self as $crate::union_ty::UnionRepr>::into_union(self)
            }

            /// Reconstructs this concrete union enum from its structural `Union![...]` form.
            #[inline]
            $vis fn from_union(u: $crate::Union![$($ty),+]) -> Self {
                <Self as $crate::union_ty::UnionRepr>::from_union(u)
            }

            /// Generic type-safe narrowing by member type `M`.
            #[inline]
            $vis fn narrow<M, I>(
                self,
            ) -> core::result::Result<
                M,
                <$crate::Union![$($ty),+] as $crate::union_ty::Narrow<M, I>>::Remainder,
            >
            where
                $crate::Union![$($ty),+]: $crate::union_ty::Narrow<M, I> + $crate::union_ty::UniqueSet,
            {
                self.into_union().narrow()
            }

            /// By-value widening into any superset or reordered union (either a `Union![...]`
            /// or another `define_union!` enum).
            #[inline]
            $vis fn widen_into<Target, Idxs>(self) -> Target
            where
                Target: $crate::union_ty::UnionRepr,
                $crate::Union![$($ty),+]:
                    $crate::union_ty::Widen<Target::Structural, Idxs> + $crate::union_ty::UniqueSet,
            {
                self.into_union().widen_into()
            }
        }
    };
}
