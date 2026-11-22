//! The `LangItems` record: the lang items one crate defines. The lookup across the crates a
//! crate loads (`blaze_impls::lang_item`) is blaze-impls'.
//!
//! A lang item is named by the text of its `#[lang = ".."]` ([`LangName`], 1 to
//! [`MAX_LANG_NAME`] bytes), not by an enum of this crate's: the table of lang items is
//! hir-def's (`lang_item::LangItemEnum`), and this record files whatever names a crate's
//! attributes give, so it neither duplicates that table nor moves when it grows.
//!
//! The layout (`LangRoot`, version 2): the crate's key, its entries (`LangEntry`: where the
//! name is in the names table, its length, and the item's key), sorted by name, so a lookup is
//! one binary search, and the names' bytes back to back. Version 1 held each name in a fixed
//! 32-byte field, which refused `core`'s `panic_misaligned_pointer_dereference` (36 bytes).
//!
//! Across the graph, rustc loads every crate's lang items and refuses a name defined twice
//! (E0152). `blaze_impls::lang_item` reads, in index order, the crates `from` loads that may
//! define lang items ([`CrateFlags::LANG_ITEMS`](crate::crate_graph::CrateFlags::LANG_ITEMS),
//! from each crate's roots: the language's crates and those with `#![feature(lang_items)]`),
//! typically `core`, `alloc` and `std`; every read is noted, and a second definition is a
//! refusal naming both.

use core::fmt;
use core::mem::offset_of;

use blaze_format::{FormatError, Part, Record, SchemaHash, Slice, Writer, WriterOpts, schema_of};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};

use crate::column::RecordBytes;
use crate::crate_graph::slice_field;

/// The longest lang item name, in bytes: a bound on a table entry (rule 18), far past every
/// name rustc's table has (the longest, 36 bytes, is `panic_misaligned_pointer_dereference`).
pub const MAX_LANG_NAME: usize = 256;

/// The text of a `#[lang = ".."]`: 1 to [`MAX_LANG_NAME`] bytes with no NUL, borrowed from
/// the attribute or from the record it is read from. Names order by their bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LangName<'t>(&'t str);

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
                write!(f, "a lang item name of {len} bytes, past the {MAX_LANG_NAME} a record holds")
            }
            LangNameError::Nul => f.write_str("a lang item name with a NUL byte"),
        }
    }
}

impl core::error::Error for LangNameError {}

impl<'t> LangName<'t> {
    /// The name `text` is.
    pub const fn new(text: &'t str) -> Result<LangName<'t>, LangNameError> {
        let bytes = text.as_bytes();
        if bytes.is_empty() {
            return Err(LangNameError::Empty);
        }
        if bytes.len() > MAX_LANG_NAME {
            return Err(LangNameError::TooLong { len: bytes.len() });
        }
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at] == 0 {
                return Err(LangNameError::Nul);
            }
            at += 1;
        }
        Ok(LangName(text))
    }

    /// The name in `bytes`, as a record holds it, refused when no name writes them.
    fn from_bytes(bytes: &'t [u8]) -> Option<LangName<'t>> {
        LangName::new(core::str::from_utf8(bytes).ok()?).ok()
    }

    pub fn as_str(&self) -> &'t str {
        self.0
    }
}

impl fmt::Debug for LangName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for LangName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a crate's lang items are not a record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LangItemsError<'t> {
    /// The crate defines `name` twice (E0152): its two items, by key order.
    Duplicate { krate: CrateItemKey, name: LangName<'t>, first: ItemKey, second: ItemKey },
    /// An item not of the crate whose record is built.
    ForeignItem { krate: CrateItemKey, name: LangName<'t>, item: ItemKey },
    /// The names' bytes together are past what a record's `u32` offsets reach.
    TooLarge { bytes: usize },
}

impl fmt::Display for LangItemsError<'_> {
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
            LangItemsError::TooLarge { bytes } => {
                write!(f, "lang item names of {bytes} bytes together, past a record's 4 GiB")
            }
        }
    }
}

impl core::error::Error for LangItemsError<'_> {}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    struct LangEntry: 2 {
        item: ContentId,
        name_at: u32,
        name_len: u32,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy)]
    struct LangRoot: 2 {
        krate: ContentId,
        entries: Slice<LangEntry>,
        names: Slice<u8>,
    }
}

/// The schema of a `LangItems` buffer.
pub const LANG_SCHEMA: SchemaHash =
    schema_of(&[Part::Str("LangItems"), Part::U64(2), Part::Schema(LangRoot::SCHEMA)]);

/// `krate`'s `LangItems` record of `items`, in any order, in `alloc`: equal bytes for every
/// order.
pub fn build_lang_items<'a, 't>(
    krate: CrateItemKey,
    items: &[(LangName<'t>, ItemKey)],
    alloc: Alloc<'a>,
) -> Result<RecordBytes<'a>, LangItemsError<'t>> {
    if let Some(&(name, item)) = items.iter().find(|(_, item)| !item.is_of(krate)) {
        return Err(LangItemsError::ForeignItem { krate, name, item });
    }
    let mut entries: AVec<'a, (LangName<'t>, ItemKey)> = AVec::with_capacity_in(items.len(), alloc);
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
    let total: usize = entries.iter().map(|(name, _)| name.0.len()).sum();
    if u32::try_from(total).is_err() {
        return Err(LangItemsError::TooLarge { bytes: total });
    }
    let mut rows: AVec<'a, LangEntry> = AVec::with_capacity_in(entries.len(), alloc);
    let mut names: AVec<'a, u8> = AVec::with_capacity_in(total, alloc);
    for &(name, item) in entries.iter() {
        let at = u32::try_from(names.len()).map_err(|_| LangItemsError::TooLarge { bytes: total })?;
        let len = u32::try_from(name.0.len()).map_err(|_| LangItemsError::TooLarge { bytes: total })?;
        names.extend_from_slice(name.0.as_bytes());
        rows.push(LangEntry { item: item.0, name_at: at, name_len: len });
    }
    let mut out = Writer::new(WriterOpts { schema: LANG_SCHEMA, alloc: alloc, capacity: None });
    let entries_at = out.push_slice(&rows);
    let names_at = out.push_slice(&names);
    let at = out.next::<LangRoot>();
    let root = LangRoot {
        krate: krate.0,
        entries: slice_field(at, offset_of!(LangRoot, entries), entries_at, rows.len()),
        names: slice_field(at, offset_of!(LangRoot, names), names_at, names.len()),
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
    names: &'a [u8],
}

impl<'a> LangItemsView<'a> {
    /// The record in `buffer`, its schema, layout and every name checked: once, so a lookup
    /// reads names without checking them again.
    pub fn read(buffer: &'a [u8]) -> Result<LangItemsView<'a>, FormatError> {
        let root = blaze_format::open_as::<LangRoot>(buffer, LANG_SCHEMA)?;
        let at = blaze_format::header(buffer)?.root;
        let entries = root.entries.follow(buffer)?;
        let names = root.names.follow(buffer)?;
        let mut last: Option<LangName<'a>> = None;
        for entry in entries {
            let name = name_bytes(names, entry)
                .and_then(LangName::from_bytes)
                .ok_or(FormatError::Invalid { at, what: "a lang item name no name writes" })?;
            if last.is_some_and(|last| last >= name) {
                return Err(FormatError::Invalid { at, what: "lang items not sorted strictly by name" });
            }
            last = Some(name);
        }
        Ok(LangItemsView { krate: CrateItemKey(root.krate), entries, names })
    }

    pub fn krate(&self) -> CrateItemKey {
        self.krate
    }

    /// The item the crate defines as `name`.
    pub fn get(&self, name: LangName<'_>) -> Option<ItemKey> {
        let at = self
            .entries
            .binary_search_by(|entry| name_bytes(self.names, entry).cmp(&Some(name.as_str().as_bytes())))
            .ok()?;
        Some(ItemKey(self.entries[at].item))
    }

    /// Every lang item, by name.
    pub fn iter(&self) -> impl Iterator<Item = (LangName<'a>, ItemKey)> + use<'a> {
        let view = *self;
        view.entries.iter().map(move |entry| (view.name_of(entry), ItemKey(entry.item)))
    }

    /// The name of `entry`, which `read` checked.
    fn name_of(&self, entry: &LangEntry) -> LangName<'a> {
        let names: &'a [u8] = self.names;
        match name_bytes(names, entry).map(core::str::from_utf8) {
            Some(Ok(text)) => LangName(text),
            _ => unreachable!("`LangItemsView::read` checked every entry's name"),
        }
    }
}

/// The bytes `entry` names in `names`, if they are in it.
fn name_bytes<'a>(names: &'a [u8], entry: &LangEntry) -> Option<&'a [u8]> {
    let start = entry.name_at as usize;
    names.get(start..start.checked_add(entry.name_len as usize)?)
}
