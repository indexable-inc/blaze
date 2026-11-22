//! Typed 32-bit ids: [`Id`], the one trait every structure here stores an index through, and
//! [`id!`], which declares an id type over a [`NonMax`] so `Option<TheId>` is 4 bytes.
//!
//! An id of one table cannot index another: each [`Table`](crate::Table) is indexed only by its
//! own id type, so `Vec<T>[usize]`'s "any integer indexes anything" is a compile error here
//! (`refusals/wrong_table_index.txt`). `u32` is an `Id` too, for a structure whose caller has no
//! id type of its own; a table of `u32` gives that check up, on purpose.

use core::{cmp::Ordering, fmt, hash, num::NonZeroU32};

/// A typed 32-bit id: a dense index, `0..u32::MAX` (the top value is not an id, so an
/// `Option` of an [`id!`] type costs nothing).
pub trait Id: Copy + Eq {
    /// The id of index `raw`. `raw` is below `u32::MAX`; `u32::MAX` is a bug and panics at the
    /// caller, naming the type.
    fn from_u32(raw: u32) -> Self;

    /// The index this id names.
    fn to_u32(self) -> u32;

    /// The index as a `usize`, for a slice.
    #[inline(always)]
    fn index(self) -> usize {
        blaze_int::idx(self.to_u32())
    }
}

impl Id for u32 {
    #[inline(always)]
    fn from_u32(raw: u32) -> u32 {
        raw
    }

    #[inline(always)]
    fn to_u32(self) -> u32 {
        self
    }
}

/// A `u32` that is never `u32::MAX`, stored as its complement in a `NonZeroU32`, so
/// `Option<NonMax>` is 4 bytes. Ordered, compared and hashed by its value.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct NonMax(NonZeroU32);

impl NonMax {
    /// The largest value: `u32::MAX - 1`.
    pub const MAX: u32 = u32::MAX - 1;

    /// `raw`, or `None` when it is `u32::MAX`.
    #[inline(always)]
    pub const fn new(raw: u32) -> Option<NonMax> {
        match NonZeroU32::new(!raw) {
            Some(inverted) => Some(NonMax(inverted)),
            None => None,
        }
    }

    /// `raw`, which must be below `u32::MAX`; `u32::MAX` panics at the caller naming `what`.
    #[inline(always)]
    #[track_caller]
    pub fn new_or_stop(raw: u32, what: &'static str) -> NonMax {
        match NonMax::new(raw) {
            Some(it) => it,
            None => panic!("blaze_kit: u32::MAX is not an id of `{what}`; ids are 0..u32::MAX"),
        }
    }

    #[inline(always)]
    pub const fn get(self) -> u32 {
        !self.0.get()
    }
}

impl PartialOrd for NonMax {
    #[inline]
    fn partial_cmp(&self, other: &NonMax) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NonMax {
    #[inline]
    fn cmp(&self, other: &NonMax) -> Ordering {
        self.get().cmp(&other.get())
    }
}

impl hash::Hash for NonMax {
    #[inline]
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        state.write_u32(self.get());
    }
}

impl fmt::Debug for NonMax {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

/// Declares an id type: a `#[repr(transparent)]` newtype over a [`NonMax`], `Copy`, ordered
/// and hashed by its index, an [`Id`], printed as `Name#index`.
///
/// ```ignore
/// blaze_kit::id! {
///     /// A local of one body.
///     pub struct Local;
/// }
/// ```
#[macro_export]
macro_rules! id {
    ($(#[$meta:meta])* $vis:vis struct $name:ident;) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(transparent)]
        $vis struct $name($crate::NonMax);

        impl $crate::Id for $name {
            #[inline(always)]
            #[track_caller]
            fn from_u32(raw: u32) -> $name {
                $name($crate::NonMax::new_or_stop(raw, ::core::stringify!($name)))
            }

            #[inline(always)]
            fn to_u32(self) -> u32 {
                self.0.get()
            }
        }

        impl ::core::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::write!(f, "{}#{}", ::core::stringify!($name), self.0.get())
            }
        }
    };
}
