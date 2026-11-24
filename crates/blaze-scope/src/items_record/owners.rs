//! The member owners section of the items record: "which trait or impl owns this member" as
//! ONE search. The member rows are kept by owner (an owner row names its run of member rows,
//! in declaration order), so before this section a reader that held only a member's key read
//! every trait's and impl's member list of the member's crate to find its owner (the front
//! end's `member_owner`: a crate scan on the first ask about a crate). The section is that
//! question's index, written from the same rows.
//!
//! The section starts at the offset in header field [`F_OWNED_AT`] and has the count of rows in
//! header field [`F_OWNED_COUNT`], after everything the record held before it (no existing
//! field or row changes: [`append`] writes after the last byte, 16-aligned). One 8-byte row
//! per NAMED member (a member with no name, a macro call in a trait or impl body, has no row:
//! nothing asks its owner), sorted by (member key, owner row, member row): its owner's row in
//! the owner table u32; its own row in the member table u32. The member's key and name are read
//! from the member row, the owner's key from the owner row: nothing is stored twice.
//!
//! One member key under two owners is kept as two rows; [`ItemsRecord::member_owner`] answers
//! the LAST of them (the owner with the greatest key: the owner rows are sorted by key),
//! which is what a reader filling a map from every owner's list in key order answered.

use blaze_id::ItemKey;
use blaze_surface::RecordBytes;

use super::{ItemsRecord, ItemsRecordError, LoadItems, RecordItems, T_MEMBERS, T_OWNERS, key_at, le32, pad, put32, u32_of};

/// The header field holding how many rows the section has.
pub(crate) const F_OWNED_COUNT: usize = 35;
/// The header field holding the section's offset.
pub(crate) const F_OWNED_AT: usize = 36;
/// A row: owner row u32, member row u32 (8 bytes; sorted by the member row's key in `T_MEMBERS`
/// so the 32-byte member key is never duplicated — 5× smaller than the 40-byte form).
pub(crate) const OWNED_ROW: usize = 8;

/// Appends the section for `owned` (each named member's key, its owner's row in the owner
/// table and its own row in the member table, in any order: sorted here) to `out`, a record
/// whose header is written, and sets its count and offset in the header.
pub(super) fn append(out: &mut RecordBytes<'_>, owned: &mut [(ItemKey, u32, u32)]) -> Result<(), ItemsRecordError> {
    owned.sort_unstable();
    pad(out);
    let at = u32_of(out.len(), "the member owners section")?;
    let count = u32_of(owned.len(), "the member owners section's rows")?;
    for (_, owner, row) in owned.iter() {
        put32(out, *owner);
        put32(out, *row);
    }
    let field = 8 + 4 * F_OWNED_COUNT;
    out[field..field + 4].copy_from_slice(&count.to_le_bytes());
    let field = 8 + 4 * F_OWNED_AT;
    out[field..field + 4].copy_from_slice(&at.to_le_bytes());
    Ok(())
}

impl<'a> ItemsRecord<'a> {
    /// How many rows the member owners section has: one per named member of the crate's
    /// traits and impls.
    pub fn owned_members(&self) -> usize {
        self.field(F_OWNED_COUNT) as usize
    }

    /// The member key of row `i` of the section (rows sorted by member key; one key under two
    /// owners is two rows): what a reader's `MemberOf` cells are written from (names_cells.rs).
    /// `Ok(None)` past the section's end.
    pub fn owned_key_at(&self, i: usize) -> Result<Option<ItemKey>, ItemsRecordError> {
        let (base, count) = (self.field(F_OWNED_AT) as usize, self.owned_members());
        if count.checked_mul(OWNED_ROW).and_then(|len| base.checked_add(len)).is_none_or(|end| end > self.bytes.len()) {
            return Err(ItemsRecordError::Short { what: "the member owners section", at: base });
        }
        if i >= count {
            return Ok(None);
        }
        let row = usize::try_from(le32(self.bytes, base + i * OWNED_ROW + 4)).unwrap_or(usize::MAX);
        if row >= self.counts.get(T_MEMBERS).copied().unwrap_or(0) {
            return Err(ItemsRecordError::Short { what: "a member owner's member row", at: i });
        }
        Ok(Some(self.member_key(row)))
    }

    /// The trait or impl that owns the named member `member`, and the member's name: one
    /// binary search of the section, then one owner row and one member row read in place (no
    /// member list is decoded, no text is opened). `Ok(None)`: no named member of the crate
    /// has that key.
    pub fn member_owner(&self, member: ItemKey) -> Result<Option<(ItemKey, &'a str)>, ItemsRecordError> {
        if member.0.0[..8] != self.key.prefix() {
            return Ok(None);
        }
        let (base, count) = (self.field(F_OWNED_AT) as usize, self.owned_members());
        if count.checked_mul(OWNED_ROW).and_then(|len| base.checked_add(len)).is_none_or(|end| end > self.bytes.len()) {
            return Err(ItemsRecordError::Short { what: "the member owners section", at: base });
        }
        let tail = &member.0.0[8..];
        // The first row whose key is greater than `member`: the row before it is the last
        // row of `member`'s run, if it has one.
        let (mut lo, mut hi) = (0usize, count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let row = usize::try_from(le32(self.bytes, base + mid * OWNED_ROW + 4)).unwrap_or(usize::MAX);
            if row >= self.counts.get(T_MEMBERS).copied().unwrap_or(0) {
                return Err(ItemsRecordError::Short { what: "a member owner's member row", at: mid });
            }
            if self.member_key_tail(row) <= tail {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let Some(last) = lo.checked_sub(1) else { return Ok(None) };
        let r = base + last * OWNED_ROW;
        let owner = usize::try_from(le32(self.bytes, r)).unwrap_or(usize::MAX);
        let row = usize::try_from(le32(self.bytes, r + 4)).unwrap_or(usize::MAX);
        if owner >= self.counts.get(T_OWNERS).copied().unwrap_or(0) || row >= self.counts.get(T_MEMBERS).copied().unwrap_or(0) {
            return Err(ItemsRecordError::Short { what: "a member owner's rows", at: last });
        }
        if self.member_key_tail(row) != tail {
            return Ok(None);
        }
        let name = self.member_name(row)?;
        Ok(Some((key_at(self.bytes, self.row(T_OWNERS, owner)), name)))
    }
}

impl<'a, L: LoadItems<'a>> RecordItems<'a, L> {
    /// The owner and name of the named member `member`: [`ItemsRecord::member_owner`] of its
    /// crate (found by the key's first eight bytes; opened on first ask). `Ok(None)`: a key of
    /// no crate given, a crate with no record kept, or no named member under that key.
    pub fn member_owner(&self, member: ItemKey) -> Result<Option<(ItemKey, &'a str)>, ItemsRecordError> {
        match self.of(member) {
            Some((_, r)) => r.member_owner(member),
            None => Ok(None),
        }
    }
}
