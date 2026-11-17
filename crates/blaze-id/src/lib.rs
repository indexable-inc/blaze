//! An item's identity (docs/v2/plan/design/identity.md).
//!
//! An item is named two ways. An interned id (`FunctionId`, `ImplId`, ...) holds a `Gid`: an
//! arena, its generation and the slot interning happened to reach the item in. It compares in
//! one instruction, and it means nothing outside the snapshot that made it. An [`ItemKey`] is
//! the hash of what the item is in, its kind, its name and where it is written: the same in
//! every process, and kept by any edit that leaves those alone.
//!
//! This crate holds what keeps the two apart:
//!
//! - [`Portable`], the bound a value meets when it may be written to a record or key a table
//!   that outlives a snapshot. Keys meet it; nothing holding a `Gid` does.
//! - [`ItemKey`] and [`CrateItemKey`], the keys of an item and of a crate.
//! - [`KeyOf`] and [`Resolve`], the two ways between an id and its key, which a database
//!   implements, and [`key_of`] and [`resolve`], the one pair of functions every record's
//!   encoder and decoder goes through. [`NoItem`] is a key that names no item of a crate.
//! - [`Carried`], a table that outlives a snapshot: it can only be keyed by a `Portable` key.
//!
//! It is `no_std` on `core` alone, without `alloc`: its one table is blaze-hash's, in the
//! allocator its maker names. It is not on bstd (docs/v2/plan/design/std.md) yet:
//! bstd uses syntax only blaze accepts, and while cargo with the pinned nightly builds and
//! tests blaze in the dev loop (AGENTS.md, `blaze-builds-blaze`; decision D25) a crate that
//! must be verified now cannot depend on it. It moves to bstd when that rule returns to
//! blaze-only.

#![no_std]
#![feature(allocator_api)]

mod carried;

use core::fmt;

pub use blaze_ids::{ContentId, Fingerprint};

pub use crate::carried::Carried;

/// A value that means the same in every process and after any edit elsewhere in the
/// workspace, so it may be written to a record or key a table that outlives a snapshot
/// (docs/v2/plan/design/identity.md, point 3).
///
/// Keys and the plain values a key is made of are `Portable`: content hashes, integers of a
/// fixed width, text, and arrays, slices, tuples and options of `Portable` values. An interned
/// id is not, nor is anything holding one: its `Gid` is the order interning reached it in on
/// whichever thread got there, and it names a dead arena after the next snapshot. Such a
/// value is written to a record, or keys a long-lived table, as its key ([`key_of`]) and read
/// back with [`resolve`].
///
/// `usize` and `isize` are not `Portable`: their width is the target's, and a record written
/// on one host is read on another.
///
/// A refused id, with a stand-in for blaze-memo's `Gid` (this crate does not depend on
/// blaze-memo):
///
/// ```compile_fail,E0277
/// #![feature(allocator_api)]
/// use std::alloc::Global;
///
/// /// What blaze-memo's `Gid` is: an arena, its generation and a slot.
/// #[derive(Clone, Copy, PartialEq, Eq, Hash)]
/// struct Gid { arena: u32, generation: u32, slot: u32 }
///
/// /// An interned id, as `#[blaze_memo::interned]` makes one: a `Gid` inside.
/// #[derive(Clone, Copy, PartialEq, Eq, Hash)]
/// struct FunctionId(Gid);
///
/// let traces: blaze_id::Carried<FunctionId, u32, Global> = blaze_id::Carried::new_in(Global);
/// ```
///
/// The control: the same table keyed by the item's key compiles.
///
/// ```
/// #![feature(allocator_api)]
/// use std::alloc::Global;
///
/// use blaze_id::{ContentId, ItemKey};
///
/// let mut traces: blaze_id::Carried<ItemKey, u32, Global> = blaze_id::Carried::new_in(Global);
/// traces.insert(ItemKey(ContentId([7; 32])), 1);
/// assert_eq!(traces.get(&ItemKey(ContentId([7; 32]))), Some(&1));
/// ```
///
/// # Safety
///
/// An implementor's every value means the same thing in every process, on every host blaze
/// runs on, and in every snapshot: it holds no interned id (no `Gid`), no address, no index
/// whose meaning is an order reached at run time, and nothing of the machine (a path, a time,
/// a host name). A table keyed by one may outlive `Memo::next`, and a record holding one may be
/// read by another process: an impl that breaks this hands out ids of dead arenas or reads one
/// item's work as another's. It is `unsafe` as blaze_query's `Value`, a contract of the same
/// kind, is.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not `blaze_id::Portable`: it may not be written to a record or key a table that outlives a snapshot",
    note = "an interned id is local to its snapshot: write its `blaze_id::ItemKey` (`blaze_id::key_of`) and read it back with `blaze_id::resolve`"
)]
pub unsafe trait Portable {}

macro_rules! portable {
    ($($ty:ty),* $(,)?) => {
        $(
            // SAFETY: a value of this type is its bits, the same on every host and in every
            // snapshot; it names nothing interned.
            unsafe impl Portable for $ty {}
        )*
    };
}

portable! {
    (), bool, char, u8, u16, u32, u64, u128, i8, i16, i32, i64, i128,
    core::num::NonZeroU8, core::num::NonZeroU16, core::num::NonZeroU32, core::num::NonZeroU64,
    core::num::NonZeroU128, core::num::NonZeroI8, core::num::NonZeroI16, core::num::NonZeroI32,
    core::num::NonZeroI64, core::num::NonZeroI128,
    str, ContentId, Fingerprint, ItemKey, CrateItemKey, NoItem,
}

// SAFETY: every element is `Portable`.
unsafe impl<T: Portable, const N: usize> Portable for [T; N] {}
// SAFETY: every element is `Portable`.
unsafe impl<T: Portable> Portable for [T] {}
// SAFETY: what it borrows is `Portable`, and the borrow cannot outlive what it points at.
unsafe impl<T: Portable + ?Sized> Portable for &T {}
// SAFETY: nothing, or a `Portable` value.
unsafe impl<T: Portable> Portable for Option<T> {}

macro_rules! portable_tuples {
    ($(($($name:ident),+)),* $(,)?) => {
        $(
            // SAFETY: every field is `Portable`.
            unsafe impl<$($name: Portable),+> Portable for ($($name,)+) {}
        )*
    };
}

portable_tuples! {
    (A),
    (A, B),
    (A, B, C),
    (A, B, C, D),
    (A, B, C, D, E),
    (A, B, C, D, E, F),
    (A, B, C, D, E, F, G),
    (A, B, C, D, E, F, G, H),
}

/// Which item: the hash of what it is in (its module, impl, trait, enum or body), its kind,
/// its name and where it is written. An edit of the item's signature or body keeps it; a
/// rename or a move gives a new one. blaze-items computes it (`blaze_items::item_key`).
///
/// It has rustc's `DefPathHash` shape: bytes 0..8 are the first 8 of its crate's key
/// ([`ItemKey::crate_prefix`]), the top bit of byte 8 says whether it is of something inside a
/// body ([`ItemKey::is_inner`]), and the other 247 bits are the hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(transparent)]
pub struct ItemKey(pub ContentId);

impl ItemKey {
    /// 32-byte words viewed as keys in place, with no copy: what a record's key column is read as.
    pub fn slice_of(words: &[[u8; 32]]) -> &[ItemKey] {
        // SAFETY: `ItemKey` is `#[repr(transparent)]` over `ContentId`, itself
        // `#[repr(transparent)]` over `[u8; 32]`: the same size (32) and alignment (1), and every
        // bit pattern is valid. So `words` is `words.len()` keys at the same address, borrowed for
        // the same lifetime.
        unsafe { core::slice::from_raw_parts(words.as_ptr().cast::<ItemKey>(), words.len()) }
    }
    /// The bit of byte 8 that says a key is of something inside a body: an item of a block,
    /// a block's module, a closure, an anonymous constant, an opaque type.
    pub const INNER: u8 = 0x80;

    /// The first 8 bytes of the key of the crate the item is in.
    pub fn crate_prefix(&self) -> [u8; 8] {
        let [a, b, c, d, e, f, g, h, ..] = self.0.0;
        [a, b, c, d, e, f, g, h]
    }

    /// Whether the key is of something inside a body ([`ItemKey::INNER`]).
    pub fn is_inner(&self) -> bool {
        self.0.0[8] & ItemKey::INNER != 0
    }

    /// Whether the key is of an item of `krate`, by its first 8 bytes. Two crates of one
    /// build never share them: blaze-items stops the build with a `CrateKeyClash` first.
    pub fn is_of(&self, krate: CrateItemKey) -> bool {
        self.crate_prefix() == krate.prefix()
    }
}

impl fmt::Display for ItemKey {
    /// The first 12 hex digits, as git shows a short id.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::LowerHex for ItemKey {
    /// All 64 hex digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

/// Which crate: the hash of its content (its name, version, origin, what it is built for and
/// as, its edition, crate attributes, dependencies by name and key, and its cfg), never its
/// session's `Crate` id. base-db computes it (`base_db::crate_item_key`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CrateItemKey(pub ContentId);

impl CrateItemKey {
    /// The key's first 8 bytes: what every [`ItemKey`] of the crate begins with.
    pub fn prefix(&self) -> [u8; 8] {
        let [a, b, c, d, e, f, g, h, ..] = self.0.0;
        [a, b, c, d, e, f, g, h]
    }
}

impl fmt::Display for CrateItemKey {
    /// The first 12 hex digits, as git shows a short id.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::LowerHex for CrateItemKey {
    /// All 64 hex digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

/// A key that names no item of a crate: the item was renamed, moved or removed since the key
/// was written, or the key is of another crate.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NoItem {
    pub krate: CrateItemKey,
    pub key: ItemKey,
}

impl fmt::Display for NoItem {
    /// Both keys in full, so either can be searched for in a profile or a record.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the item key {:x} names no item of the crate {:x}", self.key, self.krate)
    }
}

impl core::error::Error for NoItem {}

/// The way from an id to its key, which a database implements for each id type it interns.
///
/// One id is asked at a time, and an implementation does not hash one key per call: it hashes
/// the keys of a module's, an impl's or a trait's items in one batch, keeps them for the
/// snapshot, and answers each call from them (blaze-items' `item_key` does).
pub trait KeyOf<Id> {
    /// The key of `id`, an id of this database's current snapshot.
    fn key_of(&self, id: Id) -> ItemKey;
}

/// The way from a key back to the id of the item it names, which a database implements for
/// each id type it interns: a lookup in the crate's index of its items' keys.
pub trait Resolve<Id> {
    /// The id, in this database's current snapshot, of the item of `krate` that `key` names,
    /// or [`NoItem`] when none does (a key of another crate included).
    fn resolve(&self, krate: CrateItemKey, key: ItemKey) -> Result<Id, NoItem>;
}

/// The key of `id` in `db`: what a record's encoder writes for an id.
pub fn key_of<Id, Db: KeyOf<Id> + ?Sized>(db: &Db, id: Id) -> ItemKey {
    db.key_of(id)
}

/// The id in `db` of the item of `krate` that `key` names: what a record's decoder reads an
/// id as.
pub fn resolve<Id, Db: Resolve<Id> + ?Sized>(
    db: &Db,
    krate: CrateItemKey,
    key: ItemKey,
) -> Result<Id, NoItem> {
    db.resolve(krate, key)
}
