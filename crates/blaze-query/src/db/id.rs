//! The dense key space's IDs: one space per [`Storage`](super::Storage), in pages of
//! [`PAGE_LEN`] slots, each page owned by one ingredient.

use std::{fmt, num::NonZeroU32};

use blaze_format::FormatError;

/// Slots of a page: an ID's page is its bits above these.
pub(crate) const PAGE_BITS: u32 = 10;
pub(crate) const PAGE_LEN: usize = 1 << PAGE_BITS;
/// Pages of one storage: IDs are below `MAX_PAGES << PAGE_BITS` (2^30).
pub(crate) const MAX_PAGES: usize = 1 << 20;

/// The ID of an interned value, an input, or a function's key: 4 bytes, never 0, so
/// `Option<Id>` is 4 bytes too. Nothing is ever freed, so an ID has no generation.
///
/// Never above [`Id::MAX_U32`] either, so `page() < MAX_PAGES`: the hit paths index an
/// ingredient's page directory (readable for `MAX_PAGES` entries) by it with no bounds load
/// (`function::fetch`, `verify::function_header`), and [`Id::new`] (with [`Id::from_u32`] and
/// [`Id::decode`] over it) is the one way to make an `Id`, which holds it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Id(NonZeroU32);

impl Id {
    /// The largest ID a storage gives; IDs above it are free for other spaces to number into
    /// the same `u32` (span's syntax contexts).
    pub const MAX_U32: u32 = ((MAX_PAGES << PAGE_BITS) - 1) as u32;

    /// The ID numbered `bits`, if it is one: `None` for 0, which names nothing, and above
    /// [`Id::MAX_U32`], past every page a storage has. What a reader of bits it did not make
    /// (a persisted record, a restored image, a question from outside) goes through, so a
    /// corrupt word becomes its own typed error rather than a panic.
    #[inline]
    pub const fn new(bits: u32) -> Option<Id> {
        match NonZeroU32::new(bits) {
            Some(id) if bits <= Id::MAX_U32 => Some(Id(id)),
            _ => None,
        }
    }

    /// The ID a persisted word `word` holds, where `at` is the byte of the buffer it is in
    /// and `what` names the field: [`FormatError::Zero`] for 0, and for any word past
    /// [`Id::MAX_U32`] (a corrupt or foreign word) [`FormatError::Bound`] naming the word and
    /// the bound.
    #[inline]
    pub fn decode(word: u64, at: u64, what: &'static str) -> Result<Id, FormatError> {
        match u32::try_from(word).ok().and_then(Id::new) {
            Some(id) => Ok(id),
            None if word == 0 => Err(FormatError::Zero { at, what }),
            None => {
                Err(FormatError::Bound { at, what, found: word, bound: u64::from(Id::MAX_U32) })
            }
        }
    }

    /// The ID numbered `bits`, which this process made (read back from its own memory, or a
    /// test's constant). Bits from anywhere else go through [`Id::new`] or [`Id::decode`].
    ///
    /// # Panics
    ///
    /// On 0, which names nothing, and above [`Id::MAX_U32`], past every page a storage has
    /// (another space's number, or a key forged or decoded unchecked): the page directories
    /// are indexed by an ID's page unchecked, so no such `Id` may exist.
    #[inline]
    #[track_caller]
    pub fn from_u32(bits: u32) -> Id {
        match Id::new(bits) {
            Some(id) => id,
            None => not_an_id(bits),
        }
    }

    #[inline]
    pub const fn as_u32(self) -> u32 {
        self.0.get()
    }

    /// Its page: below `MAX_PAGES` by construction. The mask changes nothing for an `Id`
    /// (`as_u32() <= MAX_U32`), but it shows the bound to the compiler, so a column of
    /// `MAX_PAGES` places indexed by it ([`super::dense::Whole`]) checks nothing at run time.
    #[inline]
    pub(crate) const fn page(self) -> usize {
        debug_assert!(
            self.0.get() <= Id::MAX_U32,
            "an `Id` above `Id::MAX_U32`, which `Id::new` refuses"
        );
        (self.0.get() >> PAGE_BITS) as usize & (MAX_PAGES - 1)
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

#[cold]
#[inline(never)]
#[track_caller]
fn not_an_id(bits: u32) -> ! {
    match bits {
        0 => panic!("an `Id` of 0, which names nothing"),
        _ => panic!(
            "an `Id` of {bits}, above `Id::MAX_U32` ({}): past every ID page a query storage has \
             (another space's number, or a key forged or decoded without checking its bound)",
            Id::MAX_U32,
        ),
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
