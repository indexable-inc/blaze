//! The HPACK encoder (RFC 7541 sections 4.2, 6, 7.1). Deterministic: the same fields after the
//! same history give the same bytes on any machine.
//!
//! The policy, field by field, in this order:
//! 1. a field marked sensitive is a literal never indexed (section 6.2.3, and section 7.1.3:
//!    "an intermediary MUST NOT re-encode a value that uses the never-indexed literal
//!    representation with another representation that would index it"), its name indexed when
//!    a table has it;
//! 2. a field the static table holds whole is indexed by it, then one the dynamic table holds
//!    whole (the newest such entry);
//! 3. else a literal with incremental indexing, its name indexed by the static table first, then
//!    by the dynamic table; but a field too large for the table is a literal without indexing,
//!    since inserting it would only empty the table (section 4.4).
//!
//! This is the policy RFC 7541 Appendix C's examples follow, which the tests check byte for byte.
//!
//! The table's maximum is the smaller of our own cap and the peer's SETTINGS_HEADER_TABLE_SIZE
//! (RFC 9113 section 6.5.2). When it changes, the next block starts with the size updates
//! section 4.2 asks for: "the smallest maximum table size that occurs in that interval MUST be
//! signaled in a dynamic table size update. The final maximum size is always signaled".

#![blaze::sans_io]

use blaze_own::{AVec, Alloc, AllocFailure};

use super::{
    huffman, put_int,
    table::{STATIC_LEN, Side, Table, entry_size, find_static},
};
use crate::head::Field;

/// How string literals are written (section 5.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Strings {
    /// Always the raw octets.
    Raw,
    /// Always Huffman-coded.
    Huffman,
    /// Huffman-coded when that is not longer than the raw octets.
    Shorter,
}

/// One connection's encoder.
pub struct Encoder<'a> {
    table: Table<'a>,
    /// Our own cap on the table, whatever the peer allows.
    own_max: u32,
    /// The smallest maximum since the last block, when it changed.
    pending_min: Option<u32>,
    /// The maximum to signal at the next block's start, when it changed.
    pending_final: Option<u32>,
    strings: Strings,
}

impl<'a> Encoder<'a> {
    /// An encoder whose table is at most `own_max` bytes, starting at the RFC's default
    /// SETTINGS_HEADER_TABLE_SIZE (4096) until the peer's SETTINGS say otherwise.
    pub fn new(own_max: u32, strings: Strings, alloc: Alloc<'a>) -> Encoder<'a> {
        let start = own_max.min(4096);
        let mut encoder = Encoder {
            table: Table::new(Side::Encoder, 4096, alloc),
            own_max,
            pending_min: None,
            pending_final: None,
            strings,
        };
        if start != 4096 {
            encoder.peer_allows(4096);
        }
        encoder
    }

    /// An encoder whose table both ends already agree is `max` bytes, with no size update owed:
    /// the starting point of RFC 7541 Appendix C.5 and C.6 ("using a header table size of 256").
    pub fn agreed(max: u32, strings: Strings, alloc: Alloc<'a>) -> Encoder<'a> {
        Encoder { table: Table::new(Side::Encoder, max, alloc), own_max: max, pending_min: None, pending_final: None, strings }
    }

    /// The dynamic table.
    pub fn table(&self) -> &Table<'a> {
        &self.table
    }

    /// The peer's SETTINGS_HEADER_TABLE_SIZE is `peer` now.
    pub fn peer_allows(&mut self, peer: u32) {
        let new = self.own_max.min(peer);
        if new == self.table.max() && self.pending_final.is_none() {
            return;
        }
        self.pending_min = Some(self.pending_min.map_or(new, |min| min.min(new)));
        self.pending_final = Some(new);
        self.table.set_max(new);
    }

    /// Appends the block of `fields` to `out`.
    pub fn encode<'f>(
        &mut self,
        fields: impl Iterator<Item = Field<'f>>,
        out: &mut AVec<'_, u8>,
    ) -> Result<(), AllocFailure> {
        if let Some(last) = self.pending_final.take() {
            if let Some(min) = self.pending_min.take()
                && min < last
            {
                put_int(out, 0x20, 5, min)?;
            }
            put_int(out, 0x20, 5, last)?;
        }
        for field in fields {
            self.field(field, out)?;
        }
        Ok(())
    }

    fn field(&mut self, field: Field<'_>, out: &mut AVec<'_, u8>) -> Result<(), AllocFailure> {
        let (static_full, static_name) = find_static(field.name, field.value);
        let (dynamic_full, dynamic_name) = self.find_dynamic(field.name, field.value);
        let name_index = static_name.or(dynamic_name.map(|i| i + STATIC_LEN));
        if field.sensitive {
            return self.literal(0x10, 4, name_index, field, out);
        }
        if let Some(index) = static_full.or(dynamic_full.map(|i| i + STATIC_LEN)) {
            return put_int(out, 0x80, 7, index);
        }
        if entry_size(field.name, field.value) > u64::from(self.table.max()) {
            return self.literal(0x00, 4, name_index, field, out);
        }
        self.literal(0x40, 6, name_index, field, out)?;
        self.table.insert(field.name, field.value)
    }

    /// A literal of the kind `flags` with an `n`-bit name index prefix.
    fn literal(
        &self,
        flags: u8,
        n: u8,
        name_index: Option<u32>,
        field: Field<'_>,
        out: &mut AVec<'_, u8>,
    ) -> Result<(), AllocFailure> {
        match name_index {
            Some(index) => put_int(out, flags, n, index)?,
            None => {
                put_int(out, flags, n, 0)?;
                self.string(field.name, out)?;
            }
        }
        self.string(field.value, out)
    }

    fn string(&self, bytes: &[u8], out: &mut AVec<'_, u8>) -> Result<(), AllocFailure> {
        let coded = huffman::encoded_len(bytes);
        let use_huffman = match self.strings {
            Strings::Raw => false,
            Strings::Huffman => true,
            Strings::Shorter => coded <= bytes.len(),
        };
        if use_huffman {
            put_int(out, 0x80, 7, u32::try_from(coded).unwrap_or(u32::MAX))?;
            huffman::encode(bytes, out)
        } else {
            put_int(out, 0x00, 7, u32::try_from(bytes.len()).unwrap_or(u32::MAX))?;
            out.try_extend_from_slice(bytes)
        }
    }

    /// The newest dynamic entry holding the field whole, and the newest holding its name, as
    /// dynamic indices (1 is the newest).
    fn find_dynamic(&self, name: &[u8], value: &[u8]) -> (Option<u32>, Option<u32>) {
        let mut name_at = None;
        for i in 1..=self.table.len() {
            let Some((n, v)) = self.table.get(i) else { break };
            if n == name {
                if v == value {
                    return (Some(i), name_at.or(Some(i)));
                }
                name_at = name_at.or(Some(i));
            }
        }
        (None, name_at)
    }
}
