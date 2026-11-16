//! [`ImplRow`]: what the impl index holds of one impl.

use core::fmt;

use blaze_id::{ContentId, ItemKey, Portable};

use crate::shape::SelfShape;

/// What an impl's header says besides its trait and self type.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct ImplFlags(u32);

// SAFETY: a `u32` of flags.
unsafe impl Portable for ImplFlags {}

/// Bits [`ImplFlags::from_bits`] does not know.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnknownFlags {
    pub bits: u32,
}

impl fmt::Display for UnknownFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "impl flags {:#x} hold bits no impl row has (known: {:#x})", self.bits, ImplFlags::ALL)
    }
}

impl core::error::Error for UnknownFlags {}

impl ImplFlags {
    pub const NONE: ImplFlags = ImplFlags(0);
    /// `impl !Trait for T`.
    pub const NEGATIVE: ImplFlags = ImplFlags(1);
    /// `unsafe impl`.
    pub const UNSAFE: ImplFlags = ImplFlags(1 << 1);
    /// `default impl`, or an impl with a `default` item: specializable.
    pub const DEFAULT: ImplFlags = ImplFlags(1 << 2);
    /// `impl const Trait for T`.
    pub const CONST: ImplFlags = ImplFlags(1 << 3);
    /// `#[rustc_reservation_impl]`: coherence sees it, the trait solver skips it
    /// (rust-lang/rust#64631).
    pub const RESERVATION: ImplFlags = ImplFlags(1 << 4);
    /// A built-in derive's impl (`#[derive(Clone)]` on a type), which has no impl item of
    /// text: its key is the derive's.
    pub const BUILTIN_DERIVE: ImplFlags = ImplFlags(1 << 5);

    const ALL: u32 = (1 << 6) - 1;

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn from_bits(bits: u32) -> Result<ImplFlags, UnknownFlags> {
        match bits & !ImplFlags::ALL {
            0 => Ok(ImplFlags(bits)),
            _ => Err(UnknownFlags { bits }),
        }
    }

    #[must_use]
    pub const fn union(self, other: ImplFlags) -> ImplFlags {
        ImplFlags(self.0 | other.0)
    }

    pub const fn contains(self, other: ImplFlags) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// One impl as the index files it: which impl, of which trait (none for an inherent impl),
/// for which self shape, with its flags and the content hash of its `ImplHeader` record.
///
/// An impl inside a body (in a block, a `const _`, a function) is a row like any other: an
/// impl is its crate's wherever it is written. Whether it is is its key's
/// ([`ImplRow::in_body`]), which is not stored twice.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ImplRow {
    pub impl_: ItemKey,
    pub trait_: Option<ItemKey>,
    pub shape: SelfShape,
    pub flags: ImplFlags,
    /// The content hash of the impl's `ImplHeader` record: its generics, trait ref and self
    /// type in full, which a candidate's reader reads next.
    pub header: ContentId,
}

// SAFETY: keys, a shape, flags and a content hash, each `Portable`.
unsafe impl Portable for ImplRow {}

impl ImplRow {
    /// Whether the impl is written inside a body ([`ItemKey::is_inner`]).
    pub fn in_body(&self) -> bool {
        self.impl_.is_inner()
    }

    /// Whether it is an impl for any type (`impl<T> Trait for T`).
    pub fn is_blanket(&self) -> bool {
        self.shape.is_blanket()
    }
}
