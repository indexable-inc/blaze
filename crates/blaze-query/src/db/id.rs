//! The dense key space's IDs: one space per [`Storage`](super::Storage), in pages of
//! [`PAGE_LEN`] slots, each page owned by one ingredient.

use std::{fmt, num::NonZeroU32};

/// Slots of a page: an ID's page is its bits above these.
pub(crate) const PAGE_BITS: u32 = 10;
pub(crate) const PAGE_LEN: usize = 1 << PAGE_BITS;
/// Pages of one storage: IDs are below `MAX_PAGES << PAGE_BITS` (2^30).
pub(crate) const MAX_PAGES: usize = 1 << 20;

/// The ID of an interned value, an input, or a function's key: 4 bytes, never 0, so
/// `Option<Id>` is 4 bytes too. Nothing is ever freed, so an ID has no generation.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Id(NonZeroU32);

impl Id {
    /// The largest ID a storage gives; IDs above it are free for other spaces to number into
    /// the same `u32` (span's syntax contexts).
    pub const MAX_U32: u32 = ((MAX_PAGES << PAGE_BITS) - 1) as u32;

    /// The ID numbered `bits`.
    ///
    /// # Panics
    ///
    /// On 0, which names nothing.
    #[inline]
    #[track_caller]
    pub const fn from_u32(bits: u32) -> Id {
        match NonZeroU32::new(bits) {
            Some(bits) => Id(bits),
            None => panic!("an `Id` of 0"),
        }
    }

    #[inline]
    pub const fn as_u32(self) -> u32 {
        self.0.get()
    }

    #[inline]
    pub(crate) const fn page(self) -> usize {
        (self.0.get() >> PAGE_BITS) as usize
    }

    #[inline]
    pub(crate) const fn slot(self) -> usize {
        self.0.get() as usize & (PAGE_LEN - 1)
    }

    #[inline]
    pub(crate) fn at(page: usize, slot: usize) -> Id {
        debug_assert!(page < MAX_PAGES && slot < PAGE_LEN && (page | slot) != 0);
        Id::from_u32(((page << PAGE_BITS) | slot) as u32)
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id({})", self.0)
    }
}

/// A type that is an [`Id`] (an interned or input struct, a function's key, an enum of them).
pub trait AsId: Sized {
    fn as_id(&self) -> Id;
}

/// A type made of an [`Id`] alone, whatever ingredient's: interned and input structs.
pub trait FromId: AsId + Sized {
    fn from_id(id: Id) -> Self;
}

impl AsId for Id {
    #[inline]
    fn as_id(&self) -> Id {
        *self
    }
}

impl FromId for Id {
    #[inline]
    fn from_id(id: Id) -> Self {
        id
    }
}

/// A key type an [`Id`] of a storage can be read back as: the type itself when the ID's
/// page belongs to one of its ingredients, `None` else. What an enum of ID types
/// (`#[derive(Supertype)]`) is built on, and what a function's key is decoded with.
pub trait FromIdIn: AsId + Sized {
    fn from_id_in(storage: &super::Storage, id: Id) -> Option<Self>;
}
