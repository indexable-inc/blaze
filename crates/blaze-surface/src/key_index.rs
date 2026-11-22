//! [`KeyIndex`]: a page of a crate's index of its items' keys, one per module, so finding an
//! item from its key ([`crate::Items::place`], blaze-id's `resolve`) is one probe.

use core::mem::offset_of;

use blaze_format::{FormatError, Slice, Writer, WriterOpts};
use blaze_id::{ContentId, ItemKey};
use blaze_own::{AVec, Alloc};

use crate::column::{Column, RecordBytes, check_increasing, finish_with, key_of_word, key_word};
use crate::error::BuildError;
use crate::kind::{Place, PlaceTag};

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct KeyRow: 1 {
        pub(crate) key: ContentId,
        pub(crate) parent: ContentId,
        pub(crate) origin: ContentId,
        pub(crate) index: u32,
        pub(crate) place: PlaceTag,
        pub(crate) pad: [u8; 3],
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct KeyIndexRoot: 1 {
        pub(crate) module: ContentId,
        pub(crate) rows: Slice<KeyRow>,
    }
}

/// Where an item of a key index page is, and the item it is in. The parent is here because a
/// place cannot give it back: two modules whose files have equal bytes share one `FileItems`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct KeyEntry {
    pub key: ItemKey,
    pub place: Place,
    /// The module, impl, trait, enum or struct the item is in; `None` for a crate's root module.
    pub parent: Option<ItemKey>,
}

/// The writer of a module's [`KeyIndex`] page: the keys of its items (and of the items inside
/// them: variants, fields, associated items). Sorted by key at the end; two places for one key
/// are refused.
pub struct KeyIndex<'a> {
    module: ItemKey,
    rows: AVec<'a, KeyRow>,
    alloc: Alloc<'a>,
}

impl<'a> KeyIndex<'a> {
    pub fn new_in(module: ItemKey, alloc: Alloc<'a>) -> KeyIndex<'a> {
        KeyIndex { module, rows: AVec::new_in(alloc), alloc }
    }

    pub fn entry(&mut self, entry: KeyEntry) {
        let (place, origin, index) = entry.place.to_record();
        self.rows.push(KeyRow {
            key: entry.key.0,
            parent: key_word(entry.parent),
            origin,
            index,
            place,
            pad: [0; 3],
        });
    }

    pub fn finish(mut self) -> Result<RecordBytes<'a>, BuildError> {
        self.rows.sort_unstable_by_key(|row| (row.key, row.parent, row.origin, row.index, row.place));
        self.rows.dedup();
        if let Some(pair) = self.rows.windows(2).find(|pair| pair[0].key == pair[1].key) {
            return Err(BuildError::Twice { table: "KeyIndex rows", key: pair[1].key });
        }
        crate::column::count(self.rows.len(), "KeyIndex rows")?;
        let mut out = Writer::new(WriterOpts { schema: <KeyIndexRoot as blaze_format::Record>::SCHEMA, alloc: self.alloc, capacity: None });
        let rows = Column::push(&mut out, &self.rows);
        Ok(finish_with(out, |at| KeyIndexRoot {
            module: self.module.0,
            rows: rows.slice(at, offset_of!(KeyIndexRoot, rows)),
        }))
    }
}

/// A [`KeyIndex`] page read in place.
#[derive(Clone, Copy, Debug)]
pub struct KeyIndexView<'a> {
    module: ItemKey,
    rows: &'a [KeyRow],
}

impl<'a> KeyIndexView<'a> {
    /// The page in `buffer`, checked once: its layout and that its keys strictly increase.
    pub fn read(buffer: &'a [u8]) -> Result<KeyIndexView<'a>, FormatError> {
        let root = blaze_format::open::<KeyIndexRoot>(buffer)?;
        let rows = root.rows.follow(buffer)?;
        check_increasing(rows, buffer, "keys out of order or repeated", |row| row.key)?;
        Ok(KeyIndexView { module: ItemKey(root.module), rows })
    }

    pub fn module(&self) -> ItemKey {
        self.module
    }

    fn decode(row: &KeyRow) -> KeyEntry {
        KeyEntry {
            key: ItemKey(row.key),
            place: Place::from_record(row.place, row.origin, row.index),
            parent: key_of_word(row.parent),
        }
    }

    /// The entry of `key`, by one binary search.
    pub fn get(&self, key: ItemKey) -> Option<KeyEntry> {
        let at = self.rows.binary_search_by_key(&key.0, |row| row.key).ok()?;
        Some(KeyIndexView::decode(&self.rows[at]))
    }

    /// Every entry, by key.
    pub fn entries(&self) -> impl Iterator<Item = KeyEntry> + use<'a> {
        self.rows.iter().map(KeyIndexView::decode)
    }
}
