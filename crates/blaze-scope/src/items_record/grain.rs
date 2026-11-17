//! The items record at item grain (drive.md, "End state: the whole pipeline on the command",
//! piece 4): each item row has a grain row beside it, so an edit to one item's text voids that
//! item's row alone, never its whole file's.
//!
//! A grain row (64 bytes, one per item row, in the item rows' key order) holds two digests of
//! the item as the solve's outline saw it ([`ItemPieces`]):
//!
//! - its CHAIN ([`item_chain`]): where it is by names from its text's root (each enclosing
//!   entry's kind, name as written and disambiguator, up to the item), so no edit elsewhere in
//!   the file moves it (rule 71);
//! - its BODY: the whole item's digest (`hash`): a proof, never a fact (`body_changed`).
//!
//! It holds NO head (D114 rulings, "Signatures"; a record holds nothing its read list does not
//! cover): the record's `CrateScope` reads its files' `Surface` (blaze-file-items
//! names_digest.rs), which leaves out every type, generic and bound, and every fact of a row is
//! one the `Surface` covers (key, kind, module, outline entry, name, flags, impl row: the
//! impl's header is in its names digest). So a row read against a changed text whose chain is
//! found is fresh by construction; a type edit moves the item's `Signature` (blaze-cli), never
//! its row. A grain row with a head (until 2026-11-16) went stale on a type edit while its
//! scope stood valid by its reads, and nothing remade it.
//!
//! The section starts at the offset in the header's last free field ([`F_GRAIN_AT`]), after
//! the strings, 16-aligned. Its rows hold no position: a row's place in today's text is found
//! by its chain in today's outline.
//!
//! A reader asks [`ItemsRecord::item_now`]. Its text unchanged (the whole file's digest equal,
//! the one hash [`ItemsRecord::whole`] already does): every row is answered as written, no
//! per-item work. Its file changed: the item is found by its chain in today's outline, and the
//! row's facts hold and are answered at today's place ([`ItemNow::Fresh`], saying whether its
//! body changed); its chain gone, that item alone is [`ItemNow::Stale`]. An expansion's text
//! that changed stays refused whole ([`ItemsRecordError::Stale`]): an expansion is remade as
//! one record.
//!
//! One home: the chain is `blaze_file_items::item_chain` (drive.md's piece 5); [`item_chain`]
//! here calls it. blaze-cli work.rs `item_chain`
//! (the same walk without an unnamed entry's head, cut to 64 bits, for findings) moves to the
//! same function in the driver's lane.

use blaze_file_items::Outline;
use blaze_id::{ContentId, ItemKey};
use blaze_own::Alloc;
use blaze_surface::{Edition, Item, ItemIndex, RecordBytes};

use super::{ItemRow, ItemsRecord, ItemsRecordError, LoadItems, RecordItems, T_ITEMS, TextSpan, body_holds_items, id_at, pad, u32_of};
use crate::resolve::{At, CrateNames};

/// The header field (u32, at byte `8 + 4 * 37` = 156, the last before the keys at 160) that
/// holds the grain section's offset.
pub(super) const F_GRAIN_AT: usize = 37;
/// A grain row: chain 32, body 32.
const GRAIN_ROW: usize = 64;

/// An item's two digests, as a grain row holds them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ItemPieces {
    /// Its place by names ([`item_chain`]).
    pub chain: ContentId,
    /// The whole item, body included: a proof, never a fact.
    pub body: ContentId,
}

/// An entry of a text's outline as it is now: what a row whose text changed is checked against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ItemToday<'a> {
    /// The whole text it is in, as it is now.
    pub text: &'a str,
    /// Its place among the outline's entries.
    pub index: ItemIndex,
    pub item: Item,
}

/// An item row against the text as it is now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ItemNow<'a> {
    /// Its text is the one the record saw: the row as written.
    Same(ItemRow<'a>),
    /// Its text changed and one entry of today's text has its chain: the row's facts hold (the
    /// record's `Surface` reads cover every one; a type edit is fresh), at today's place in
    /// `text` (the row's text span, impl header, outline entry and whether its body holds items
    /// are today's). `body_changed`: its whole digest differs from the record's.
    Fresh { row: ItemRow<'a>, text: &'a str, body_changed: bool },
    /// No single entry of today's text has its chain `chain` (renamed, removed, or two entries
    /// share it): this item's row is stale, and no fact of it is answered.
    Stale { key: ItemKey, file: u32, chain: ContentId },
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

/// The one entry of `outline` (of `text`) whose chain is `chain`; `None` when there is none or
/// more than one.
pub fn today<'a>(outline: &Outline<'_>, text: &'a str, chain: ContentId) -> Option<ItemToday<'a>> {
    let mut found = None;
    for (i, item) in outline.items().iter().enumerate() {
        if item_chain(outline, item, text) == chain {
            if found.is_some() {
                return None;
            }
            found = Some(ItemToday { text, index: ItemIndex(u32::try_from(i).ok()?), item: *item });
        }
    }
    found
}

/// [`today`] over `text` outlined now in `alloc`: what [`LoadItems::item`] does for a loader
/// that holds no outline of the file.
pub fn outline_today<'a>(text: &'a str, chain: ContentId, edition: Edition, alloc: Alloc<'a>) -> Option<ItemToday<'a>> {
    let outline = blaze_file_items::outline(text, edition, alloc);
    today(&outline, text, chain)
}

/// Appends the grain section for `items` (the item rows' keys, in their order) to `out`, a
/// record whose header is written, and sets its offset in the header.
pub(super) fn append(out: &mut RecordBytes<'_>, names: &CrateNames<'_>, items: &[ItemKey]) -> Result<(), ItemsRecordError> {
    pad(out);
    let at = u32_of(out.len(), "the item grain section")?;
    for key in items.iter() {
        let (it, place) = names.tables.texts.get(key).ok_or(ItemsRecordError::NoOutline { item: *key })?;
        let (text, outline) = names.outline_at(*place).ok_or(ItemsRecordError::NoOutline { item: *key })?;
        let item = outline.items().get(it.item.0 as usize).ok_or(ItemsRecordError::NoOutline { item: *key })?;
        let p = pieces_of(outline, item, text);
        out.extend_from_slice(&p.chain.0);
        out.extend_from_slice(&p.body.0);
    }
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
        Ok(ItemPieces { chain: id_at(self.bytes, r), body: id_at(self.bytes, r + 32) })
    }

    /// The item `key`'s pieces; `Ok(None)` when the crate has no item with text under that key.
    pub fn pieces(&self, key: ItemKey) -> Result<Option<ItemPieces>, ItemsRecordError> {
        match self.find(T_ITEMS, key) {
            Some(i) => self.pieces_at(i).map(Some),
            None => Ok(None),
        }
    }

    /// The item `key`'s row against its text as it is now (see the module docs): `read` gives
    /// a text now (as [`ItemsRecord::whole`] asks it), `today` the entry of the file at a path
    /// whose chain is the one given, from the file as it is now. `Ok(None)`: no item with text
    /// under that key.
    pub fn item_now(
        &self,
        key: ItemKey,
        read: &dyn Fn(super::TextOf<'_>) -> Option<&'a str>,
        today: &dyn Fn(crate::package_path::PackagePath<'a>, ContentId) -> Option<ItemToday<'a>>,
    ) -> Result<Option<ItemNow<'a>>, ItemsRecordError> {
        let Some(i) = self.find(T_ITEMS, key) else { return Ok(None) };
        let row = self.item_at(i)?.ok_or(ItemsRecordError::Short { what: "an item row", at: i })?;
        let span = row.text.ok_or(ItemsRecordError::NoPlace { key })?;
        let index = match self.whole(span.at, read) {
            Ok(_) => return Ok(Some(ItemNow::Same(row))),
            Err(ItemsRecordError::Stale { file: Some(index), .. }) => index,
            Err(e) => return Err(e),
        };
        let file = self.file(index)?.ok_or(ItemsRecordError::Short { what: "a file row", at: index as usize })?;
        let was = self.pieces_at(i)?;
        let Some(now) = today(file.path, was.chain) else {
            return Ok(Some(ItemNow::Stale { key, file: index, chain: was.chain }));
        };
        // No head is compared: every fact of the row is covered by the record's `Surface`
        // reads (module docs); a type edit is fresh, and moves the item's `Signature`.
        let range = ItemsRecordError::Range { krate: self.key, start: span.start, len: span.len };
        let start = now.item.range.start;
        let len = now.item.range.end.checked_sub(start).ok_or(range)?;
        // The impl header is in its names digest, hence in the `Surface` the record stands on:
        // unchanged, at the same offset from the item's start.
        let impl_header = match row.impl_header {
            Some(h) => {
                let at = h.start.checked_sub(span.start).and_then(|rel| rel.checked_add(start));
                Some(TextSpan { at: At::File(index), start: at.ok_or(ItemsRecordError::Range { krate: self.key, start: h.start, len: h.len })?, len: h.len })
            }
            None => None,
        };
        let row = ItemRow {
            text: Some(TextSpan { at: At::File(index), start, len }),
            outline: now.index,
            body_holds_items: body_holds_items(now.item.detail),
            impl_header,
            ..row
        };
        Ok(Some(ItemNow::Fresh { row, text: now.text, body_changed: now.item.hash != was.body }))
    }
}

impl<'a, L: LoadItems<'a>> RecordItems<'a, L> {
    /// [`ItemsRecord::item_now`] of `key`'s crate, the texts and today's entries from the
    /// loader. `Ok(None)`: no crate given holds the key, or it holds no item with text under it.
    pub fn item_now(&self, key: ItemKey) -> Result<Option<ItemNow<'a>>, ItemsRecordError> {
        let Some((krate, record)) = self.of(key) else { return Ok(None) };
        let edition = record.edition();
        record.item_now(key, &|which| self.read(krate, which), &|path, chain| self.load.item(krate, path, chain, edition, self.alloc))
    }

    /// The text of file `index` of crate `krate` as the loader has it now, for a reader that
    /// reads every item in it at item grain ([`RecordItems::item_now`]): the text the record
    /// saw, proved by its digest ([`ItemsRecord::whole`]); or, when the file changed since, the
    /// file as it is now, whose items are each proved or refused by `item_now` when read: never
    /// read through the record's old offsets. An expansion is not asked here (an expansion's
    /// text that changed stays refused whole).
    pub fn file_now(&self, krate: usize, index: u32) -> Result<&'a str, ItemsRecordError> {
        let record = self.record(krate).ok_or(ItemsRecordError::Short { what: "a crate with an items record", at: krate })?;
        match record.whole(At::File(index), &|which| self.read(krate, which)) {
            Ok(text) => Ok(text),
            Err(ItemsRecordError::Stale { file: Some(_), call: None, .. }) => {
                let row = record.file(index)?.ok_or(ItemsRecordError::Short { what: "a file row", at: index as usize })?;
                self.load.file(krate, row.path).ok_or(ItemsRecordError::Gone { krate: record.key, file: Some(index), call: None })
            }
            Err(e) => Err(e),
        }
    }
}
