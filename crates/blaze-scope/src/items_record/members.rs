//! A trait's or an impl's members read in place (D114 rule 1): the owner's row is found by one
//! binary search of the owners table, and its members are a run of the members table, each row
//! read from the bytes when asked. Nothing is decoded ahead or kept; [`Members`] is a `Copy` of
//! a reference and two numbers.
//!
//! NEVER COMPILED (lane d114a-inplace, 2026-11-16): inert until `pub mod members;` is added to
//! items_record.rs by the wiring, which also deletes the old `ItemsRecord::members`.

use blaze_id::ItemKey;
use blaze_surface::{ItemIndex, ItemKind};

use super::{
    BODY_HOLDS_ITEMS, CTOR_OR_WRITTEN_BY, ItemsRecord, ItemsRecordError, MemberRow, NAMED, T_MEMBERS, T_OWNERS, TextSpan, key_at, le16,
    le32,
};
use crate::resolve::At;

/// An owner's members, in declaration order, as rows of its crate's items record.
#[derive(Clone, Copy)]
pub struct Members<'r, 'a> {
    record: &'r ItemsRecord<'a>,
    first: usize,
    count: usize,
}

impl<'r, 'a> Members<'r, 'a> {
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Member `i`, read from its row; `Ok(None)` past the last.
    pub fn get(&self, i: usize) -> Result<Option<MemberRow<'a>>, ItemsRecordError> {
        if i >= self.count {
            return Ok(None);
        }
        self.record.member_row(self.first + i).map(Some)
    }

    /// Every member, each read from its row as the iterator reaches it.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Result<MemberRow<'a>, ItemsRecordError>> + use<'r, 'a> {
        let all = *self;
        (0..all.count).map(move |i| all.record.member_row(all.first + i))
    }
}

impl<'a> ItemsRecord<'a> {
    /// A trait's or an impl's members in declaration order (question 13), read in place;
    /// `Ok(None)` when `owner` is no trait or impl the crate holds members of.
    pub fn members(&self, owner: ItemKey) -> Result<Option<Members<'_, 'a>>, ItemsRecordError> {
        let Some(i) = self.find(T_OWNERS, owner) else { return Ok(None) };
        let r = self.row(T_OWNERS, i);
        let (first, count) = (le32(self.bytes, r + 32) as usize, le32(self.bytes, r + 36) as usize);
        if first.checked_add(count).is_none_or(|end| end > self.counts[T_MEMBERS]) {
            return Err(ItemsRecordError::Short { what: "an owner's member rows", at: first });
        }
        Ok(Some(Members { record: self, first, count }))
    }

    /// Member row `k` of the members table, below its count.
    fn member_row(&self, k: usize) -> Result<MemberRow<'a>, ItemsRecordError> {
        let b: &'a [u8] = self.bytes;
        let m = self.row(T_MEMBERS, k);
        let flags = b[m + 55];
        let outline = ItemIndex(le32(b, m + 44));
        let at = self.at_of(le32(b, m + 32))?;
        let text = self.item_span(at, outline, le32(b, m + 36), le32(b, m + 40))?;
        let written_by = match text {
            Some(TextSpan { at: At::Expansion(call), .. }) if flags & CTOR_OR_WRITTEN_BY != 0 => Some(call),
            _ => None,
        };
        Ok(MemberRow {
            key: key_at(b, m),
            name: if flags & NAMED != 0 { Some(self.str_at("a member's name", le32(b, m + 48), u32::from(le16(b, m + 52)))?) } else { None },
            outline,
            kind: ItemKind::from_byte(b[m + 54]).ok_or(ItemsRecordError::Byte { what: "a member's kind", byte: b[m + 54] })?,
            body_holds_items: flags & BODY_HOLDS_ITEMS != 0,
            text,
            written_by,
            lifetimes: super::lifetimes_of(le16(b, m + 56)),
        })
    }
}
