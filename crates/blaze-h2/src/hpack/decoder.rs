//! The HPACK decoder (RFC 7541 sections 3, 4.2, 6): one field block in, its fields out, the
//! dynamic table updated as each representation says. One loop over the block; nothing recurses.
//!
//! Bounded (RFC 9113 section 10.5.1, "SETTINGS_MAX_HEADER_LIST_SIZE"): the caller passes the
//! most bytes the decoded list may hold, by section 4.1's accounting (name, value, 32 per
//! field). Past it the decoder stops copying fields out and says so ([`Decoded::over`]), but goes
//! on decoding to the block's end: the dynamic table must take every insertion the encoder made
//! or the two tables part ways (RFC 7541 section 2.2: "the decoder ... must process"). An indexed
//! field past the cap costs one lookup and no copy, so a block that names one large entry many
//! times (the header list bomb) costs time in its own length and memory in the cap alone.

#![blaze::sans_io]

use blaze_own::{AVec, Alloc};

use super::{
    Halt, HpackFault, get_int, huffman,
    table::{STATIC, STATIC_LEN, Side, Table, entry_size},
};
use crate::head::Fields;

/// What one block decoded to, beside the fields.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Decoded {
    /// The decoded list's size by section 4.1's accounting, all of it (past the cap too).
    pub list_size: u64,
    /// Whether the list passed the cap: then only the fields before it were copied out.
    pub over: bool,
}

/// One connection's decoder.
pub struct Decoder<'a> {
    table: Table<'a>,
    /// The SETTINGS_HEADER_TABLE_SIZE in force: the most a size update may set.
    allowed: u32,
    /// A size update is owed at the next block's start: the allowed maximum went below the
    /// table's.
    update_owed: bool,
    name: AVec<'a, u8>,
    value: AVec<'a, u8>,
}

impl<'a> Decoder<'a> {
    /// A decoder whose table may be `allowed` bytes (the peer's encoder starts at it: RFC 9113
    /// section 6.5.2, SETTINGS_HEADER_TABLE_SIZE's initial value is 4096).
    pub fn new(allowed: u32, alloc: Alloc<'a>) -> Decoder<'a> {
        Decoder {
            table: Table::new(Side::Decoder, allowed, alloc),
            allowed,
            update_owed: false,
            name: AVec::new_in(alloc),
            value: AVec::new_in(alloc),
        }
    }

    /// The dynamic table.
    pub fn table(&self) -> &Table<'a> {
        &self.table
    }

    /// Our SETTINGS_HEADER_TABLE_SIZE took effect (the peer acknowledged it): a size update to
    /// at most `allowed` is owed first when it is below the table's maximum now.
    pub fn set_allowed(&mut self, allowed: u32) {
        if allowed < self.table.max() {
            self.update_owed = true;
        }
        self.allowed = allowed;
    }

    /// Decodes `block` into `out` (appended), copying fields while the list is at most
    /// `most_list` bytes.
    pub fn decode(&mut self, block: &[u8], out: &mut Fields<'_>, most_list: u32) -> Result<Decoded, Halt> {
        let mut at = 0;
        let mut fields_seen = false;
        let mut updates = 0u8;
        let mut decoded = Decoded { list_size: 0, over: false };
        while at < block.len() {
            let first = block[at];
            if first & 0x20 != 0 && first & 0xc0 == 0 {
                // 001xxxxx: a dynamic table size update (section 6.3).
                if fields_seen {
                    return Err(HpackFault::SizeUpdateLate.into());
                }
                updates += 1;
                if updates > 2 {
                    return Err(HpackFault::SizeUpdatesOverTwo.into());
                }
                let size = get_int(block, &mut at, 5)?;
                if size > self.allowed {
                    return Err(HpackFault::SizeOverMax { size, most: self.allowed }.into());
                }
                self.table.set_max(size);
                self.update_owed = false;
                continue;
            }
            if self.update_owed {
                return Err(HpackFault::SizeUpdateMissing.into());
            }
            fields_seen = true;
            if first & 0x80 != 0 {
                // 1xxxxxxx: an indexed field (section 6.1).
                let index = get_int(block, &mut at, 7)?;
                let (name, value) = lookup(&self.table, index)?;
                emit(out, name, value, false, most_list, &mut decoded)?;
            } else if first & 0x40 != 0 {
                // 01xxxxxx: a literal with incremental indexing (section 6.2.1).
                self.literal(block, &mut at, 6)?;
                emit(out, &self.name, &self.value, false, most_list, &mut decoded)?;
                self.table.insert(&self.name, &self.value)?;
            } else {
                // 0001xxxx never indexed (section 6.2.3), 0000xxxx without indexing (6.2.2).
                let never = first & 0x10 != 0;
                self.literal(block, &mut at, 4)?;
                emit(out, &self.name, &self.value, never, most_list, &mut decoded)?;
            }
        }
        if self.update_owed {
            return Err(HpackFault::SizeUpdateMissing.into());
        }
        Ok(decoded)
    }

    /// Reads a literal's name (indexed or a string) into `self.name` and its value into
    /// `self.value`. A name taken from the dynamic table is copied: the insertion that follows
    /// may evict its entry (section 4.4's caution).
    fn literal(&mut self, block: &[u8], at: &mut usize, prefix: u8) -> Result<(), Halt> {
        let index = get_int(block, at, prefix)?;
        self.name.clear();
        if index == 0 {
            read_string(block, at, &mut self.name)?;
        } else {
            let (name, _) = lookup(&self.table, index)?;
            self.name.try_extend_from_slice(name)?;
        }
        self.value.clear();
        read_string(block, at, &mut self.value)
    }
}

/// The entry at `index` of the one address space (section 2.3.3).
fn lookup<'t>(table: &'t Table<'_>, index: u32) -> Result<(&'t [u8], &'t [u8]), HpackFault> {
    if index == 0 {
        return Err(HpackFault::IndexZero);
    }
    if index <= STATIC_LEN {
        let (name, value) = STATIC[usize::try_from(index - 1).unwrap_or(0)];
        return Ok((name, value));
    }
    table.get(index - STATIC_LEN).ok_or(HpackFault::IndexOutOfRange { index, entries: STATIC_LEN + table.len() })
}

/// A string literal (section 5.2): `H` and a 7-bit-prefix length, then the octets.
fn read_string(block: &[u8], at: &mut usize, into: &mut AVec<'_, u8>) -> Result<(), Halt> {
    let Some(&first) = block.get(*at) else {
        return Err(HpackFault::Truncated.into());
    };
    let huffman = first & 0x80 != 0;
    let len = usize::try_from(get_int(block, at, 7)?).map_err(|_| HpackFault::IntegerOverflow)?;
    let end = at.checked_add(len).ok_or(HpackFault::Truncated)?;
    let Some(bytes) = block.get(*at..end) else {
        return Err(HpackFault::Truncated.into());
    };
    *at = end;
    if huffman {
        huffman::decode(bytes, into)
    } else {
        into.try_extend_from_slice(bytes)?;
        Ok(())
    }
}

/// Counts a field and copies it out while the list is within the cap.
fn emit(
    out: &mut Fields<'_>,
    name: &[u8],
    value: &[u8],
    never: bool,
    most_list: u32,
    decoded: &mut Decoded,
) -> Result<(), Halt> {
    decoded.list_size = decoded.list_size.saturating_add(entry_size(name, value));
    if decoded.list_size > u64::from(most_list) {
        decoded.over = true;
    }
    if !decoded.over {
        out.push(name, value, never)?;
    }
    Ok(())
}
