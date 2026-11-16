//! The `LangItems` record: the lang items one crate defines. The lookup across the crates a
//! crate loads (`blaze_impls::lang_item`) is blaze-impls'.
//!
//! A lang item is named by the text of its `#[lang = ".."]` ([`LangName`], 32 bytes, zero
//! padded), not by an enum of this crate's: the table of lang items is hir-def's
//! (`lang_item::LangItemEnum`), and this record files whatever names a crate's attributes
//! give, so it neither duplicates that table nor moves when it grows.
//!
//! The layout (`LangRoot`, version 1): the crate's key, then its entries (`LangEntry`: the
//! name's 32 bytes and the item's key), sorted by name, so a lookup is one binary search.
//!
//! Across the graph, rustc loads every crate's lang items and refuses a name defined twice
//! (E0152). `blaze_impls::lang_item` reads, in index order, the crates `from` loads that may
//! define lang items ([`CrateFlags::LANG_ITEMS`](crate::crate_graph::CrateFlags::LANG_ITEMS),
//! from each crate's roots: the language's crates and those with `#![feature(lang_items)]`),
//! typically `core`, `alloc` and `std`; every read is noted, and a second definition is a
//! refusal naming both.

use core::alloc::Allocator;
use core::fmt;
use core::mem::offset_of;

use blaze_format::{FormatError, Part, Record, SchemaHash, Slice, Writer, schema_of};
use blaze_id::{ContentId, CrateItemKey, ItemKey, Portable};
use blaze_own::Vec;

use crate::column::RecordBytes;
use crate::crate_graph::slice_field;

/// The longest lang item name, in bytes.
const NAME_LEN: usize = 32;

/// The text of a `#[lang = ".."]`: 1 to 32 bytes with no NUL, zero padded to 32.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LangName([u8; NAME_LEN]);

// SAFETY: the bytes of a name.
unsafe impl Portable for LangName {}

/// Why text is not a [`LangName`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LangNameError {
    Empty,
    TooLong { len: usize },
    Nul,
}

impl fmt::Display for LangNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LangNameError::Empty => f.write_str("an empty lang item name"),
            LangNameError::TooLong { len } => {
                write!(f, "a lang item name of {len} bytes, past the {NAME_LEN} a record holds")
            }
            LangNameError::Nul => f.write_str("a lang item name with a NUL byte"),
        }
    }
}

impl core::error::Error for LangNameError {}

impl LangName {
    /// The name `text` is.
    pub const fn new(text: &str) -> Result<LangName, LangNameError> {
        let bytes = text.as_bytes();
        if bytes.is_empty() {
            return Err(LangNameError::Empty);
        }
        if bytes.len() > NAME_LEN {
            return Err(LangNameError::TooLong { len: bytes.len() });
        }
        let mut out = [0u8; NAME_LEN];
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at] == 0 {
                return Err(LangNameError::Nul);
            }
            out[at] = bytes[at];
            at += 1;
        }
        Ok(LangName(out))
    }

    /// The name in `bytes`, as a record holds it, refused when no name writes them.
    fn from_bytes(bytes: [u8; NAME_LEN]) -> Option<LangName> {
        let len = bytes.iter().position(|&it| it == 0).unwrap_or(NAME_LEN);
        let ok = len > 0
            && bytes[len..].iter().all(|&it| it == 0)
            && core::str::from_utf8(&bytes[..len]).is_ok();
        ok.then_some(LangName(bytes))
    }

    pub fn as_str(&self) -> &str {
        let len = self.0.iter().position(|&it| it == 0).unwrap_or(NAME_LEN);
        match core::str::from_utf8(&self.0[..len]) {
            Ok(text) => text,
            Err(_) => unreachable!("a LangName is made of a `&str` or of bytes checked as one"),
        }
    }
}

impl fmt::Debug for LangName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for LangName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a crate's lang items are not a record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LangItemsError {
    /// The crate defines `name` twice (E0152): its two items, by key order.
    Duplicate { krate: CrateItemKey, name: LangName, first: ItemKey, second: ItemKey },
    /// An item not of the crate whose record is built.
    ForeignItem { krate: CrateItemKey, name: LangName, item: ItemKey },
}

impl fmt::Display for LangItemsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LangItemsError::Duplicate { krate, name, first, second } => write!(
                f,
                "the crate {krate:x} defines the lang item `{name}` twice, as {first:x} and {second:x} (E0152)"
            ),
            LangItemsError::ForeignItem { krate, name, item } => write!(
                f,
                "the lang item `{name}` of the crate {krate:x} names {item:x}, an item of another crate"
            ),
        }
    }
}

impl core::error::Error for LangItemsError {}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    struct LangEntry: 1 {
        name: [u8; 32],
        item: ContentId,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy)]
    struct LangRoot: 1 {
        krate: ContentId,
        entries: Slice<LangEntry>,
    }
}

/// The schema of a `LangItems` buffer.
pub const LANG_SCHEMA: SchemaHash =
    schema_of(&[Part::Str("LangItems"), Part::U64(1), Part::Schema(LangRoot::SCHEMA)]);

/// `krate`'s `LangItems` record of `items`, in any order, in `alloc`: equal bytes for every
/// order.
pub fn build_lang_items<A: Allocator + Clone>(
    krate: CrateItemKey,
    items: &[(LangName, ItemKey)],
    alloc: A,
) -> Result<RecordBytes<A>, LangItemsError> {
    if let Some(&(name, item)) = items.iter().find(|(_, item)| !item.is_of(krate)) {
        return Err(LangItemsError::ForeignItem { krate, name, item });
    }
    let mut entries: Vec<(LangName, ItemKey), A> = Vec::with_capacity_in(items.len(), alloc.clone());
    entries.extend_from_slice(items);
    entries.sort_unstable();
    if let Some(pair) = entries.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(LangItemsError::Duplicate {
            krate,
            name: pair[0].0,
            first: pair[0].1,
            second: pair[1].1,
        });
    }
    let mut rows: Vec<LangEntry, A> = Vec::with_capacity_in(entries.len(), alloc.clone());
    rows.extend(entries.iter().map(|&(name, item)| LangEntry { name: name.0, item: item.0 }));
    let mut out = Writer::new_in(LANG_SCHEMA, alloc);
    let entries_at = out.push_slice(&rows);
    let at = out.next::<LangRoot>();
    let root = LangRoot {
        krate: krate.0,
        entries: slice_field(at, offset_of!(LangRoot, entries), entries_at, rows.len()),
    };
    let written = out.push(&root);
    debug_assert_eq!(written, at);
    Ok(out.finish(at))
}

/// A crate's `LangItems` read in place.
#[derive(Clone, Copy, Debug)]
pub struct LangItemsView<'a> {
    krate: CrateItemKey,
    entries: &'a [LangEntry],
}

impl<'a> LangItemsView<'a> {
    /// The record in `buffer`, its schema, layout and every name checked.
    pub fn read(buffer: &'a [u8]) -> Result<LangItemsView<'a>, FormatError> {
        let root = blaze_format::open_as::<LangRoot>(buffer, LANG_SCHEMA)?;
        let at = blaze_format::header(buffer)?.root;
        let entries = root.entries.follow(buffer)?;
        if entries.iter().any(|entry| LangName::from_bytes(entry.name).is_none()) {
            return Err(FormatError::Invalid { at, what: "a lang item name no name writes" });
        }
        if entries.windows(2).any(|pair| pair[0].name >= pair[1].name) {
            return Err(FormatError::Invalid { at, what: "lang items not sorted strictly by name" });
        }
        Ok(LangItemsView { krate: CrateItemKey(root.krate), entries })
    }

    pub fn krate(&self) -> CrateItemKey {
        self.krate
    }

    /// The item the crate defines as `name`.
    pub fn get(&self, name: LangName) -> Option<ItemKey> {
        let at = self.entries.binary_search_by(|entry| entry.name.cmp(&name.0)).ok()?;
        Some(ItemKey(self.entries[at].item))
    }

    /// Every lang item, by name.
    pub fn iter(&self) -> impl Iterator<Item = (LangName, ItemKey)> + use<'a> {
        let entries: &'a [LangEntry] = self.entries;
        entries.iter().map(|entry| (LangName(entry.name), ItemKey(entry.item)))
    }
}
