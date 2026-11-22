//! An item's chain identifies its fine-grained dependency cell. Its body digest belongs to the
//! record that owns its text. File rows join the current FileItems entry by outline index.
//! Expansion rows refer to a digest stored after this section's chain rows.

use blaze_file_items::Outline;
use blaze_id::{ContentId, ItemKey};
use blaze_surface::{Item, RecordBytes};

use super::{ItemsRecord, ItemsRecordError, NONE, T_ITEMS, id_at, le32, pad, put32, u32_of};
use crate::resolve::{At, CrateNames};

pub(super) const F_GRAIN_AT: usize = 37;
const GRAIN_ROW: usize = 36;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ItemPieces {
    pub chain: ContentId,
    pub body: ContentId,
}

/// `item`'s chain in `outline` of `text`: for it and each entry it is written in, up to the
/// text's root, the kind's byte, the name as written (length, bytes; empty for none), the
/// disambiguator and, for an entry with no name (an impl, a `use`, a macro call), its head, so
/// two unnamed siblings never share a chain. A walk up the parents, no recursion.
///
/// The chain's one home is `blaze_file_items::item_chain` (drive.md's piece 5): this is a call
/// to it, the bytes and the domain as they were.
pub fn item_chain(outline: &Outline<'_>, item: &Item, text: &str) -> ContentId {
    blaze_file_items::item_chain(outline, item, text)
}

/// `item`'s two digests.
pub fn pieces_of(outline: &Outline<'_>, item: &Item, text: &str) -> ItemPieces {
    ItemPieces { chain: item_chain(outline, item, text), body: item.hash }
}

/// Appends the grain section for `items` (the item rows' keys, in their order) to `out`, a
/// record whose header is written, and sets its offset in the header.
pub(super) fn append(out: &mut RecordBytes<'_>, names: &CrateNames<'_>, items: &[ItemKey]) -> Result<(), ItemsRecordError> {
    pad(out);
    let at = u32_of(out.len(), "the item grain section")?;
    let mut expansion_hashes = RecordBytes::new_in(*out.allocator());
    for key in items {
        let (it, place) = names.tables.texts.get(key).ok_or(ItemsRecordError::NoOutline { item: *key })?;
        let (text, outline) = names.outline_at(*place).ok_or(ItemsRecordError::NoOutline { item: *key })?;
        let item = outline.items().get(it.item.0 as usize).ok_or(ItemsRecordError::NoOutline { item: *key })?;
        out.extend_from_slice(&item_chain(outline, item, text).0);
        let offset = match place {
            At::Expansion(_) => {
                let offset = u32_of(expansion_hashes.len(), "expansion item hashes")?;
                expansion_hashes.extend_from_slice(&item.hash.0);
                offset
            }
            At::File(_) => NONE,
        };
        put32(out, offset);
    }
    out.extend_from_slice(&expansion_hashes);
    let field = 8 + 4 * F_GRAIN_AT;
    out[field..field + 4].copy_from_slice(&at.to_le_bytes());
    Ok(())
}

impl<'a> ItemsRecord<'a> {
    /// Item row `i`'s pieces, as the solve saw them.
    pub fn pieces_at(&self, i: usize) -> Result<ItemPieces, ItemsRecordError> {
        let short = ItemsRecordError::Short { what: "an item grain row", at: i };
        if i >= self.counts[T_ITEMS] {
            return Err(short);
        }
        let base = self.field(F_GRAIN_AT) as usize;
        let Some(r) = i.checked_mul(GRAIN_ROW).and_then(|o| base.checked_add(o)).filter(|r| r.checked_add(GRAIN_ROW).is_some_and(|end| end <= self.bytes.len())) else {
            return Err(short);
        };
        let row = self.row(T_ITEMS, i);
        let body = match self.at_of(le32(self.bytes, row + 36))? {
            Some(At::File(file)) => self.file_entry(file, blaze_surface::ItemIndex(le32(self.bytes, row + 48)))?.1.hash,
            Some(At::Expansion(_)) => {
                let offset = le32(self.bytes, r + 32) as usize;
                let hashes = self.counts[T_ITEMS].checked_mul(GRAIN_ROW).and_then(|n| base.checked_add(n)).ok_or(short)?;
                let pos = hashes.checked_add(offset).filter(|n| n.checked_add(32).is_some_and(|end| end <= self.bytes.len()))
                    .ok_or(ItemsRecordError::Short { what: "an expansion item hash", at: offset })?;
                id_at(self.bytes, pos)
            }
            None => return Err(ItemsRecordError::NoPlace { key: super::key_at(self.bytes, row) }),
        };
        Ok(ItemPieces { chain: id_at(self.bytes, r), body })
    }

    /// The item `key`'s pieces; `Ok(None)` when the crate has no item with text under that key.
    pub fn pieces(&self, key: ItemKey) -> Result<Option<ItemPieces>, ItemsRecordError> {
        match self.find(T_ITEMS, key) {
            Some(i) => self.pieces_at(i).map(Some),
            None => Ok(None),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ItemNow<'a> {
    Same(super::ItemRow<'a>),
    Fresh { row: super::ItemRow<'a>, text: &'a str, body_changed: bool },
}

impl<'a, L: super::LoadItems<'a>> super::RecordItems<'a, L> {
    /// Compare two snapshots of the same surface. Only the FileItems records carry the
    /// before and after body digests. Trivia can move a span without changing either digest.
    pub fn item_now_since(&self, key: ItemKey, before: &ItemsRecord<'_>) -> Result<Option<ItemNow<'a>>, ItemsRecordError> {
        let Some((krate, record)) = self.of(key) else { return Ok(None) };
        let Some(row) = record.item(key)? else { return Ok(None) };
        let at = row.text.ok_or(ItemsRecordError::NoPlace { key })?.at;
        let text = self.whole(krate, at)?;
        let old = before.identity(key)?.ok_or(ItemsRecordError::NoPlace { key })?;
        let (Some(At::File(old_file)), At::File(file)) = (old.at, at) else {
            if let (Some(At::Expansion(was)), At::Expansion(call)) = (old.at, at) && was == call {
                let saw = before.expansion(call).ok_or(ItemsRecordError::NoPlace { key })?.id;
                let found = record.expansion(call).ok_or(ItemsRecordError::NoPlace { key })?.id;
                if saw == found { return Ok(Some(ItemNow::Same(row))); }
                return Err(ItemsRecordError::Stale { krate: record.key(), file: None, call: Some(call), saw, found });
            }
            return Err(ItemsRecordError::NoPlace { key });
        };
        let (was, previous) = before.file_entry(old_file, old.outline)?;
        let (now, current) = record.file_entry(file, row.outline)?;
        if was.surface != now.surface {
            return Err(ItemsRecordError::Stale { krate: record.key(), file: Some(file), call: None, saw: was.surface, found: now.surface });
        }
        if was.view.file() == now.view.file() {
            return Ok(Some(ItemNow::Same(row)));
        }
        Ok(Some(ItemNow::Fresh { row, text, body_changed: previous.hash != current.hash }))
    }
}
