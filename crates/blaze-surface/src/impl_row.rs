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

    // What the WRITER READ of the header, past what the header says (lane BK, 2026-11-17: the
    // facts the front end's impl questions answer with, so the index is their one home and no
    // reader files an impl again). Each is a fact of the header's text and the names it was
    // resolved by, so equal inputs write equal rows.

    /// A trait impl whose trait is written with no generic arguments (`impl PartialEq for A`):
    /// every parameter of the trait is at its default, so where those defaults are `Self` the
    /// orphan rules let only the self type be the impl's crate's local type.
    pub const BARE: ImplFlags = ImplFlags(1 << 6);
    /// The self type's shape was not read (an alias not followed, a qualified path, a macro
    /// call): filed as [`SelfShape::BLANKET`] is, a candidate at every shape, though no
    /// parameter of the impl.
    pub const SELF_UNREAD: ImplFlags = ImplFlags(1 << 7);
    /// The self type is written as a path that names no type (not found, a prefix only, a
    /// module, a trait): filed as [`SelfShape::BLANKET`] is, but by the coherence rules a
    /// candidate only where the goal's self type can be its crate's local type (the impl is
    /// refused when its header is lowered, and that refusal is a gap of those goals alone).
    pub const SELF_NO_TYPE: ImplFlags = ImplFlags(1 << 8);
    /// A trait impl whose trait path the writer did not resolve: its row has no trait, and
    /// it is filed by its self shape under [`crate::BucketKey::UnreadTrait`], a candidate of
    /// every trait question at that shape. Its why is [`ImplFlags::why`].
    pub const TRAIT_UNREAD: ImplFlags = ImplFlags(1 << 9);
    /// A header not read far enough to tell a trait impl from an inherent one: filed under
    /// [`crate::BucketKey::Blind`], a candidate of every question. Its why is
    /// [`ImplFlags::why`].
    pub const BLIND: ImplFlags = ImplFlags(1 << 10);

    /// Where an unread row's why is: one byte, the writer's class (blaze-scope's `NotFiled`).
    const WHY_SHIFT: u32 = 16;
    const WHY_MASK: u32 = 0xff << ImplFlags::WHY_SHIFT;

    const ALL: u32 = ((1 << 11) - 1) | ImplFlags::WHY_MASK;

    /// The flags a header itself says (`!`, `unsafe`, `default`, `const`, a reservation, a
    /// derive): none of what the writer read.
    pub const HEADER: ImplFlags = ImplFlags((1 << 6) - 1);

    /// `self` with the why of an unread row ([`ImplFlags::TRAIT_UNREAD`],
    /// [`ImplFlags::BLIND`]).
    #[must_use]
    pub const fn with_why(self, why: u8) -> ImplFlags {
        ImplFlags((self.0 & !ImplFlags::WHY_MASK) | ((why as u32) << ImplFlags::WHY_SHIFT))
    }

    /// The why of an unread row; 0 on any other.
    pub const fn why(self) -> u8 {
        ((self.0 & ImplFlags::WHY_MASK) >> ImplFlags::WHY_SHIFT) as u8
    }

    /// Only the flags of `mask`.
    #[must_use]
    pub const fn intersect(self, mask: ImplFlags) -> ImplFlags {
        ImplFlags(self.0 & mask.0)
    }

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

    /// Whether it is an impl for any type (`impl<T> Trait for T`), or one whose self type was
    /// not read to a shape ([`ImplFlags::SELF_UNREAD`], [`ImplFlags::SELF_NO_TYPE`]).
    pub fn is_blanket(&self) -> bool {
        self.shape.is_blanket()
    }

    /// Whether its self type is one of its own parameters: [`SelfShape::BLANKET`] read as
    /// such, not for a self type that was not read to a shape.
    pub fn is_param_self(&self) -> bool {
        self.shape.is_blanket() && !self.flags.contains(ImplFlags::SELF_UNREAD) && !self.flags.contains(ImplFlags::SELF_NO_TYPE)
    }
}
