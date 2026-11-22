//! The attribute value table of the items record: each `link_name` / `export_name` value
//! written as a macro call (ring 0.17.14's `#[link_name = concat!(prefix!(), stringify!(f))]`),
//! as the crate's solve expanded it at its item ([`crate::attr_values`], `CrateNames::values`).
//! What outlives the solve is a record (rule 51): a command that reads the crate over its records
//! (a dependency, a later run) asks the symbol of a foreign fn here, by the item's key, and never
//! expands the value again (rule 77). The record is a section of the crate's `CrateScope`
//! record, so its key (the crate's: its package's name and version, its source, its features)
//! and its read list (the macros' sources the expansion asked) cover every input of a row
//! (rule 21).
//!
//! The section starts at the offset in header field [`F_VALUES_AT`] and has the count of rows in
//! header field [`F_VALUES_COUNT`], after everything the record held before it ([`append`]
//! writes after the last byte, 16-aligned). A crate with NO such value writes no section: both
//! fields stay 0 and the record's bytes are those of a record without the section.
//!
//! One 48-byte row per (item, attribute), sorted by (item key, attribute byte): the item's key
//! 32; the attribute u8 (its index in [`ATTRS`]); the state u8 ([`VALUE`]: the string,
//! [`REFUSED`]: the refusal's words); 2 zero bytes; the text's offset in the section's text u32;
//! its length u32; 4 zero bytes. Then a 16-byte sub-header (the text's length u32, 12 zero
//! bytes), then the text: every row's string, in row order.
//!
//! A refusal is kept as its words (`ValueRefused`'s `Display`: the item, the attribute, the
//! macro it stopped at, why, the last form reached): a reader over the record refuses by the
//! same words the solve gave (attr_values `RowRefusal::Recorded`).

use core::fmt::Write as _;

use blaze_id::ItemKey;
use blaze_own::{AString, AVec, Alloc};
use blaze_surface::RecordBytes;

use super::{ItemsRecord, ItemsRecordError, key_at, le32, pad, put32, u32_of};
use crate::attr_values::{ATTRS, ValueRow};

/// The header field holding how many rows the section has (0: no section).
pub(crate) const F_VALUES_COUNT: usize = 27;
/// The header field holding the section's offset (0: no section).
pub(crate) const F_VALUES_AT: usize = 28;
/// A row: item key 32, attribute u8, state u8, 2 zero bytes, text offset u32, length u32, 4 zero bytes.
pub(crate) const VALUE_ROW: usize = 48;
/// The sub-header between the rows and the text: the text's length u32, 12 zero bytes.
const TEXT_HEAD: usize = 16;
/// A row's state: its text is the value's string.
const VALUE: u8 = 0;
/// A row's state: its text is the refusal's words.
const REFUSED: u8 = 1;

/// The byte of attribute `attr` ([`ATTRS`]'s index).
fn attr_byte(attr: &str) -> Option<u8> {
    ATTRS.iter().position(|a| *a == attr).and_then(|i| u8::try_from(i).ok())
}

/// Appends the section for `rows` (the solve's, `CrateNames::values`) to `out`, a record whose
/// header is written, and sets its count and offset in the header. No rows: nothing is written.
pub(super) fn append(out: &mut RecordBytes<'_>, rows: &[ValueRow<'_>], alloc: Alloc<'_>) -> Result<(), ItemsRecordError> {
    if rows.is_empty() {
        return Ok(());
    }
    // (item, attribute byte, the row's index), sorted: the section's order, never the solve's.
    let mut order: AVec<'_, (ItemKey, u8, usize)> = AVec::with_capacity_in(rows.len(), alloc);
    for (i, row) in rows.iter().enumerate() {
        let attr = attr_byte(row.attr).ok_or(ItemsRecordError::Byte { what: "a value row's attribute (link_name or export_name)", byte: 0xff })?;
        order.push((row.item, attr, i));
    }
    order.sort_unstable();
    if order.windows(2).any(|pair| (pair[0].0, pair[0].1) == (pair[1].0, pair[1].1)) {
        return Err(ItemsRecordError::Short { what: "the value table (two rows of one item and attribute)", at: 0 });
    }
    let mut text = AString::new_in(alloc);
    let mut spans: AVec<'_, (u8, u32, u32)> = AVec::with_capacity_in(order.len(), alloc);
    for &(_, _, i) in order.iter() {
        let start = text.len();
        let state = match rows[i].value {
            Ok(value) => {
                text.push_str(value);
                VALUE
            }
            Err(refused) => {
                write!(text, "{refused}").map_err(|_| ItemsRecordError::TooBig { what: "a value row's refusal" })?;
                REFUSED
            }
        };
        spans.push((state, u32_of(start, "the value table's text")?, u32_of(text.len() - start, "a value row's text")?));
    }
    pad(out);
    let at = u32_of(out.len(), "the value table")?;
    let count = u32_of(order.len(), "the value table's rows")?;
    for (&(item, attr, _), &(state, start, len)) in order.iter().zip(spans.iter()) {
        out.extend_from_slice(&item.0.0);
        out.extend_from_slice(&[attr, state, 0, 0]);
        put32(out, start);
        put32(out, len);
        put32(out, 0);
    }
    put32(out, u32_of(text.len(), "the value table's text")?);
    out.extend_from_slice(&[0u8; TEXT_HEAD - 4]);
    out.extend_from_slice(text.as_bytes());
    let field = 8 + 4 * F_VALUES_COUNT;
    out[field..field + 4].copy_from_slice(&count.to_le_bytes());
    let field = 8 + 4 * F_VALUES_AT;
    out[field..field + 4].copy_from_slice(&at.to_le_bytes());
    Ok(())
}

impl<'a> ItemsRecord<'a> {
    /// The section's rows' offset and count, and its text's offset and length, checked to lie in
    /// the record. `None`: the crate wrote no section (no value written as a call).
    fn value_table(&self) -> Result<Option<(usize, usize, usize, usize)>, ItemsRecordError> {
        let (base, count) = (self.field(F_VALUES_AT) as usize, self.field(F_VALUES_COUNT) as usize);
        if count == 0 {
            return Ok(None);
        }
        let short = || ItemsRecordError::Short { what: "the value table", at: base };
        let head = count.checked_mul(VALUE_ROW).and_then(|len| base.checked_add(len)).filter(|end| end + TEXT_HEAD <= self.bytes.len()).ok_or_else(short)?;
        let (text_at, text_len) = (head + TEXT_HEAD, le32(self.bytes, head) as usize);
        if text_at.checked_add(text_len).is_none_or(|end| end > self.bytes.len()) {
            return Err(short());
        }
        Ok(Some((base, count, text_at, text_len)))
    }

    /// The value of `attr` (`link_name` or `export_name`) on `item`, written as a macro call and
    /// expanded at the item by the crate's solve: `Ok(value)` its string, `Err(words)` the
    /// refusal's words. One binary search of the section; no text of the crate is read.
    /// `Ok(None)`: no row (the value is no call, or the item has no such attribute).
    pub fn attr_value(&self, item: ItemKey, attr: &str) -> Result<Option<Result<&'a str, &'a str>>, ItemsRecordError> {
        let Some(attr) = attr_byte(attr) else { return Ok(None) };
        let Some((base, count, text_at, text_len)) = self.value_table()? else { return Ok(None) };
        let row_key = |i: usize| {
            let r = base + i * VALUE_ROW;
            (&self.bytes[r..r + 32], self.bytes[r + 32])
        };
        let want = (&item.0.0[..], attr);
        // The first row not below (item, attr).
        let (mut lo, mut hi) = (0usize, count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if row_key(mid) < want {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo >= count || row_key(lo) != want {
            return Ok(None);
        }
        let r = base + lo * VALUE_ROW;
        debug_assert_eq!(key_at(self.bytes, r), item);
        let (state, start, len) = (self.bytes[r + 33], le32(self.bytes, r + 36) as usize, le32(self.bytes, r + 40) as usize);
        if start.checked_add(len).is_none_or(|end| end > text_len) {
            return Err(ItemsRecordError::Short { what: "a value row's text", at: lo });
        }
        let bytes = &self.bytes[text_at + start..text_at + start + len];
        let text = core::str::from_utf8(bytes).map_err(|_| ItemsRecordError::Utf8 { what: "a value row's text", at: lo })?;
        match state {
            VALUE => Ok(Some(Ok(text))),
            REFUSED => Ok(Some(Err(text))),
            byte => Err(ItemsRecordError::Byte { what: "a value row's state", byte }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No rows write no byte: after `append` of none, a record is byte for byte what it was,
    /// both header fields 0. TWIN: one row writes the section and sets both fields.
    #[test]
    fn no_rows_write_no_byte() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut out: RecordBytes<'_> = RecordBytes::new_in(alloc);
        out.resize(8 + 4 * (F_VALUES_AT + 1), 0);
        out[..4].copy_from_slice(b"head");
        let before: std::vec::Vec<u8> = out.iter().copied().collect();
        append(&mut out, &[], alloc).unwrap_or_else(|e| panic!("append of no rows: {e:?}"));
        assert_eq!(out.as_slice(), before.as_slice(), "no rows, no byte");

        let row = ValueRow { item: ItemKey(blaze_id::ContentId([1; 32])), attr: "link_name", value: Ok("sym"), reads: &[] };
        append(&mut out, &[row], alloc).unwrap_or_else(|e| panic!("append of one row: {e:?}"));
        assert!(out.len() > before.len(), "twin: one row writes the section");
        assert_eq!(le32(&out, 8 + 4 * F_VALUES_COUNT), 1);
        assert_ne!(le32(&out, 8 + 4 * F_VALUES_AT), 0);
    }
}
