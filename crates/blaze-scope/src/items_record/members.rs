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
    BODY_HOLDS_ITEMS, CTOR_OR_WRITTEN_BY, ItemsRecord, ItemsRecordError, MemberRow, NAMED, T_MEMBERS, T_OWNERS, TextSpan, le32,
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
        let cols = self.member_cols;
        let meta = cols.meta.get(k);
        let name_len = u32::try_from(meta & 0xffff).unwrap_or(0);
        let kind_byte = u8::try_from((meta >> 16) & 0xff).unwrap_or(0);
        let flags = u8::try_from((meta >> 24) & 0xff).unwrap_or(0);
        let lifetimes = u16::try_from((meta >> 32) & 0xffff).unwrap_or(0);
        let outline = ItemIndex(cols.outline.get_u32(k));
        let at = self.at_of(cols.code.get_u32(k))?;
        let text = self.item_span(at, outline, cols.start.get_u32(k), cols.len.get_u32(k))?;
        let written_by = match text {
            Some(TextSpan { at: At::Expansion(call), .. }) if flags & CTOR_OR_WRITTEN_BY != 0 => Some(call),
            _ => None,
        };
        Ok(MemberRow {
            key: self.member_key(k),
            name: if flags & NAMED != 0 { Some(self.str_at("a member's name", cols.name_at.get_u32(k), name_len)?) } else { None },
            outline,
            kind: ItemKind::from_byte(kind_byte).ok_or(ItemsRecordError::Byte { what: "a member's kind", byte: kind_byte })?,
            body_holds_items: flags & BODY_HOLDS_ITEMS != 0,
            text,
            written_by,
            lifetimes: super::lifetimes_of(lifetimes),
        })
    }
}
