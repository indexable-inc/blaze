//! `bitflags!`: a set of named bits over an unsigned integer.
//!
//! Two forms, those of the crates.io `bitflags` 2 this replaces:
//!
//! ```ignore
//! bitflags! {
//!     #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
//!     pub struct Flags: u8 {
//!         const A = 1 << 0;
//!         const B = 1 << 1;
//!         const AB = Flags::A.bits() | Flags::B.bits();
//!     }
//! }
//!
//! #[derive(Clone, Copy, PartialEq, Eq)]
//! pub struct Mine(u16);
//! bitflags! {
//!     impl Mine: u16 {
//!         const X = 1;
//!     }
//! }
//! ```
//!
//! The first declares the struct, over a hidden [`__private::Raw`] whose `Debug` prints the set
//! as bitflags does (`Flags(A | B)`, `Flags(A | 0x10)`, `Flags(0x0)`); the second adds the flags
//! to the caller's own one-field struct, which writes its own `Debug` (usually with
//! [`parser::to_writer`]). Both give each flag as an associated `const`, the set operations as
//! `const fn`s (`empty`, `all`, `bits`, `from_bits`, `from_bits_truncate`, `from_bits_retain`,
//! `contains`, `intersects`, `union`, `intersection`, `difference`, `symmetric_difference`,
//! `complement`, `insert`, `remove`, `toggle`, `set`, `is_empty`, `is_all`), `from_name`,
//! `iter`, `iter_names`, the operators `| & ^ - !` and their assignments, `Extend`,
//! `FromIterator` and `IntoIterator`, and [`Flags`].
//!
//! `all` is every declared flag OR-ed together; `complement` and `!` keep only those bits;
//! `from_bits` refuses a bit no flag declares, `from_bits_truncate` drops it, and
//! `from_bits_retain` keeps it.

#![no_std]

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

use core::{
    fmt,
    ops::{BitAnd, BitOr, Not},
};

/// The integers a set of flags can be over.
pub trait Bits:
    Copy + Eq + BitAnd<Output = Self> + BitOr<Output = Self> + Not<Output = Self> + fmt::LowerHex
{
    const EMPTY: Self;
}

macro_rules! bits {
    ($($t:ty),*) => {
        $(impl Bits for $t {
            const EMPTY: $t = 0;
        })*
    };
}

bits!(u8, u16, u32, u64, u128, usize);

/// A type `bitflags!` made: its integer and its named flags, in declaration order.
pub trait Flags: Sized + 'static {
    type Bits: Bits;
    const FLAGS: &'static [(&'static str, Self)];
    fn bits(&self) -> Self::Bits;
    fn from_bits_retain(bits: Self::Bits) -> Self;
}

/// The iterators of a set.
pub mod iter {
    use crate::{Bits, Flags};

    /// The named flags a set holds, in declaration order, each once it adds a bit not yet
    /// yielded: a flag that is all of two earlier ones is yielded only when it adds a bit.
    pub struct IterNames<F: Flags> {
        index: usize,
        source: F::Bits,
        remaining: F::Bits,
    }

    impl<F: Flags> IterNames<F> {
        pub fn new(flags: &F) -> IterNames<F> {
            let bits = flags.bits();
            IterNames { index: 0, source: bits, remaining: bits }
        }

        /// The bits no flag yielded so far covers.
        pub fn remaining(&self) -> F {
            F::from_bits_retain(self.remaining)
        }
    }

    impl<F: Flags> Iterator for IterNames<F> {
        type Item = (&'static str, F);

        fn next(&mut self) -> Option<(&'static str, F)> {
            while let Some((name, flag)) = F::FLAGS.get(self.index) {
                if self.remaining == <F::Bits as Bits>::EMPTY {
                    return None;
                }
                self.index += 1;
                if name.is_empty() {
                    continue;
                }
                let bits = flag.bits();
                if self.source & bits == bits && self.remaining & bits != <F::Bits as Bits>::EMPTY {
                    self.remaining = self.remaining & !bits;
                    return Some((*name, F::from_bits_retain(bits)));
                }
            }
            None
        }
    }

    /// The named flags of [`IterNames`], then the bits no flag names as one value, if any.
    pub struct Iter<F: Flags> {
        names: IterNames<F>,
        done: bool,
    }

    impl<F: Flags> Iter<F> {
        pub fn new(flags: &F) -> Iter<F> {
            Iter { names: IterNames::new(flags), done: false }
        }
    }

    impl<F: Flags> Iterator for Iter<F> {
        type Item = F;

        fn next(&mut self) -> Option<F> {
            if let Some((_, flag)) = self.names.next() {
                return Some(flag);
            }
            if self.done {
                return None;
            }
            self.done = true;
            let rest = self.names.remaining;
            (rest != <F::Bits as Bits>::EMPTY).then(|| F::from_bits_retain(rest))
        }
    }
}

/// The text form of a set.
pub mod parser {
    use core::fmt;

    use crate::{Bits, Flags, iter::IterNames};

    /// `A | B`, then any bits no flag names as `0x..`; nothing for the empty set.
    pub fn to_writer<F: Flags>(flags: &F, mut writer: impl fmt::Write) -> fmt::Result {
        let mut first = true;
        let mut names = IterNames::new(flags);
        for (name, _) in &mut names {
            if !first {
                writer.write_str(" | ")?;
            }
            first = false;
            writer.write_str(name)?;
        }
        let rest = names.remaining().bits();
        if rest != <F::Bits as Bits>::EMPTY {
            if !first {
                writer.write_str(" | ")?;
            }
            write!(writer, "{rest:#x}")?;
        }
        Ok(())
    }
}

/// What the macro's expansion names; not for callers.
#[doc(hidden)]
pub mod __private {
    use core::{fmt, marker::PhantomData};

    use crate::{Bits, Flags, parser};

    /// The bits of a struct `bitflags!` declares. `F` is that struct, held only to print the
    /// set by its flags' names; `fn() -> F` keeps `Raw` free of `F`'s size and auto traits.
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
    pub struct Raw<B, F> {
        pub bits: B,
        flags: PhantomData<fn() -> F>,
    }

    impl<B, F> Raw<B, F> {
        #[inline]
        pub const fn new(bits: B) -> Raw<B, F> {
            Raw { bits, flags: PhantomData }
        }
    }

    impl<B: Bits, F: Flags<Bits = B>> fmt::Debug for Raw<B, F> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            if self.bits == B::EMPTY {
                write!(f, "{:#x}", self.bits)
            } else {
                parser::to_writer(&F::from_bits_retain(self.bits), f)
            }
        }
    }
}

/// Declares a set of flags: see the crate's documentation.
#[macro_export]
macro_rules! bitflags {
    (
        $(#[$outer:meta])*
        $vis:vis struct $Name:ident: $T:ty {
            $(
                $(#[$inner:meta])*
                const $Flag:ident = $value:expr;
            )*
        }
        $($rest:tt)*
    ) => {
        $(#[$outer])*
        $vis struct $Name($crate::__private::Raw<$T, $Name>);

        impl $Name {
            /// The set's bits.
            #[inline]
            pub const fn bits(&self) -> $T {
                self.0.bits
            }

            /// The set of exactly `bits`, named by a flag or not.
            #[inline]
            pub const fn from_bits_retain(bits: $T) -> Self {
                Self($crate::__private::Raw::new(bits))
            }
        }

        $crate::__flags! { $Name: $T { $($(#[$inner])* const $Flag = $value;)* } }

        $crate::bitflags! { $($rest)* }
    };
    (
        $(#[$outer:meta])*
        impl $Name:ident: $T:ty {
            $(
                $(#[$inner:meta])*
                const $Flag:ident = $value:expr;
            )*
        }
        $($rest:tt)*
    ) => {
        $(#[$outer])*
        impl $Name {
            /// The set's bits.
            #[inline]
            pub const fn bits(&self) -> $T {
                self.0
            }

            /// The set of exactly `bits`, named by a flag or not.
            #[inline]
            pub const fn from_bits_retain(bits: $T) -> Self {
                Self(bits)
            }
        }

        $crate::__flags! { $Name: $T { $($(#[$inner])* const $Flag = $value;)* } }

        $crate::bitflags! { $($rest)* }
    };
    () => {};
}

/// The flags, the set operations and the trait impls of both forms of [`bitflags!`], over the
/// type's `bits` and `from_bits_retain`.
#[doc(hidden)]
#[macro_export]
macro_rules! __flags {
    ($Name:ident: $T:ty { $($(#[$inner:meta])* const $Flag:ident = $value:expr;)* }) => {
        impl $Name {
            $(
                $(#[$inner])*
                pub const $Flag: Self = Self::from_bits_retain($value);
            )*

            #[inline]
            pub const fn empty() -> Self {
                Self::from_bits_retain(0)
            }

            /// Every declared flag.
            #[inline]
            pub const fn all() -> Self {
                Self::from_bits_retain(0 $(| Self::bits(&Self::$Flag))*)
            }

            /// `bits`, or `None` when it has a bit no flag declares.
            #[inline]
            pub const fn from_bits(bits: $T) -> ::core::option::Option<Self> {
                if bits & !Self::bits(&Self::all()) == 0 {
                    ::core::option::Option::Some(Self::from_bits_retain(bits))
                } else {
                    ::core::option::Option::None
                }
            }

            /// `bits` without the bits no flag declares.
            #[inline]
            pub const fn from_bits_truncate(bits: $T) -> Self {
                Self::from_bits_retain(bits & Self::bits(&Self::all()))
            }

            /// The flag declared as `name`.
            pub fn from_name(name: &str) -> ::core::option::Option<Self> {
                $(
                    if name == ::core::stringify!($Flag) {
                        return ::core::option::Option::Some(Self::$Flag);
                    }
                )*
                ::core::option::Option::None
            }

            #[inline]
            pub const fn is_empty(&self) -> bool {
                Self::bits(self) == 0
            }

            /// Whether every declared flag is in the set.
            #[inline]
            pub const fn is_all(&self) -> bool {
                let all = Self::bits(&Self::all());
                all & Self::bits(self) == all
            }

            #[inline]
            pub const fn intersects(&self, other: Self) -> bool {
                Self::bits(self) & Self::bits(&other) != 0
            }

            #[inline]
            pub const fn contains(&self, other: Self) -> bool {
                Self::bits(self) & Self::bits(&other) == Self::bits(&other)
            }

            #[inline]
            pub const fn insert(&mut self, other: Self) {
                *self = Self::from_bits_retain(Self::bits(self) | Self::bits(&other));
            }

            #[inline]
            pub const fn remove(&mut self, other: Self) {
                *self = Self::from_bits_retain(Self::bits(self) & !Self::bits(&other));
            }

            #[inline]
            pub const fn toggle(&mut self, other: Self) {
                *self = Self::from_bits_retain(Self::bits(self) ^ Self::bits(&other));
            }

            /// Inserts `other` when `value`, else removes it.
            #[inline]
            pub const fn set(&mut self, other: Self, value: bool) {
                if value {
                    Self::insert(self, other);
                } else {
                    Self::remove(self, other);
                }
            }

            #[inline]
            #[must_use]
            pub const fn intersection(self, other: Self) -> Self {
                Self::from_bits_retain(Self::bits(&self) & Self::bits(&other))
            }

            #[inline]
            #[must_use]
            pub const fn union(self, other: Self) -> Self {
                Self::from_bits_retain(Self::bits(&self) | Self::bits(&other))
            }

            #[inline]
            #[must_use]
            pub const fn difference(self, other: Self) -> Self {
                Self::from_bits_retain(Self::bits(&self) & !Self::bits(&other))
            }

            #[inline]
            #[must_use]
            pub const fn symmetric_difference(self, other: Self) -> Self {
                Self::from_bits_retain(Self::bits(&self) ^ Self::bits(&other))
            }

            /// The declared flags not in the set.
            #[inline]
            #[must_use]
            pub const fn complement(self) -> Self {
                Self::from_bits_truncate(!Self::bits(&self))
            }

            /// The named flags, then the bits no flag names as one value.
            #[inline]
            pub fn iter(&self) -> $crate::iter::Iter<Self> {
                $crate::iter::Iter::new(self)
            }

            /// The named flags with their names.
            #[inline]
            pub fn iter_names(&self) -> $crate::iter::IterNames<Self> {
                $crate::iter::IterNames::new(self)
            }
        }

        impl $crate::Flags for $Name {
            type Bits = $T;
            const FLAGS: &'static [(&'static str, Self)] = &[$((::core::stringify!($Flag), Self::$Flag),)*];

            #[inline]
            fn bits(&self) -> $T {
                $Name::bits(self)
            }

            #[inline]
            fn from_bits_retain(bits: $T) -> Self {
                $Name::from_bits_retain(bits)
            }
        }

        impl ::core::ops::BitOr for $Name {
            type Output = Self;
            #[inline]
            fn bitor(self, other: Self) -> Self {
                Self::union(self, other)
            }
        }

        impl ::core::ops::BitOrAssign for $Name {
            #[inline]
            fn bitor_assign(&mut self, other: Self) {
                Self::insert(self, other);
            }
        }

        impl ::core::ops::BitAnd for $Name {
            type Output = Self;
            #[inline]
            fn bitand(self, other: Self) -> Self {
                Self::intersection(self, other)
            }
        }

        impl ::core::ops::BitAndAssign for $Name {
            #[inline]
            fn bitand_assign(&mut self, other: Self) {
                *self = Self::intersection(*self, other);
            }
        }

        impl ::core::ops::BitXor for $Name {
            type Output = Self;
            #[inline]
            fn bitxor(self, other: Self) -> Self {
                Self::symmetric_difference(self, other)
            }
        }

        impl ::core::ops::BitXorAssign for $Name {
            #[inline]
            fn bitxor_assign(&mut self, other: Self) {
                Self::toggle(self, other);
            }
        }

        impl ::core::ops::Sub for $Name {
            type Output = Self;
            #[inline]
            fn sub(self, other: Self) -> Self {
                Self::difference(self, other)
            }
        }

        impl ::core::ops::SubAssign for $Name {
            #[inline]
            fn sub_assign(&mut self, other: Self) {
                Self::remove(self, other);
            }
        }

        impl ::core::ops::Not for $Name {
            type Output = Self;
            #[inline]
            fn not(self) -> Self {
                Self::complement(self)
            }
        }

        impl ::core::iter::Extend<$Name> for $Name {
            fn extend<I: ::core::iter::IntoIterator<Item = Self>>(&mut self, flags: I) {
                for flag in flags {
                    Self::insert(self, flag);
                }
            }
        }

        impl ::core::iter::FromIterator<$Name> for $Name {
            fn from_iter<I: ::core::iter::IntoIterator<Item = Self>>(flags: I) -> Self {
                let mut set = Self::empty();
                ::core::iter::Extend::extend(&mut set, flags);
                set
            }
        }

        impl ::core::iter::IntoIterator for $Name {
            type Item = Self;
            type IntoIter = $crate::iter::Iter<Self>;
            fn into_iter(self) -> $crate::iter::Iter<Self> {
                $crate::iter::Iter::new(&self)
            }
        }
    };
}
