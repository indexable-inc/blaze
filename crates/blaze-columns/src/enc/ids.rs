//! A column of fixed-width ids (`[u8; W]`; a 32-byte `ContentId` is `W = 32`): each distinct
//! id once, in a table in order of first use, and each row a bit-packed code into it, a
//! column of `For`/`Linear` blocks ([`IntPolicy::CONSTANT`]). An id is a uniformly random
//! string of bytes, so no scheme shrinks one; what shrinks is repetition, the same item
//! named by many rows of a record. A `get` is the codes column's `get` (an entry load and a
//! value load) and one load of the id from the table.
//!
//! # Layout
//!
//! Kind 3 (1 byte), `W` (1 byte), the distinct ids (`u32`), the table (`W` bytes an id), the
//! codes column. When every row is distinct the table is the rows and the codes are `0..n`,
//! one `Linear` block each with a width of 0: 24 bytes a 1,024 rows, so a dictionary never
//! loses more than 0.1% to plain rows of 32 bytes and the chooser has nothing to choose.

use blaze_int::Int;
use blaze_kit::Dict;
use blaze_own::{AVec, Alloc};

use super::ints::{IntPolicy, IntView, encode_ints};
use super::plant::{self, Plant};
use super::{EncError, len32, put, read8, read32};

const KIND_IDS: u8 = 3;

/// Appends `rows` as a column of ids; scratch in `alloc`. The bytes written.
pub fn encode_ids<const W: usize>(rows: &[[u8; W]], alloc: Alloc<'_>, out: &mut AVec<'_, u8>) -> Result<usize, EncError> {
    let start = out.len();
    let width = u8::try_from(W).map_err(|_| EncError::TooLong { what: "an id's width", len: W })?;
    let mut dict = Dict::<u32, [u8; W], Alloc<'_>>::new_in(alloc);
    for &row in rows {
        dict.try_push(row).map_err(|refused| EncError::Alloc(refused.why))?;
    }
    put(out, &[KIND_IDS, width])?;
    put(out, &len32(dict.distinct(), "an id column's table")?.to_le_bytes())?;
    out.try_reserve(W * dict.distinct())?;
    for id in dict.values() {
        put(out, id)?;
    }
    let mut codes = blaze_own::Vec::try_with_capacity_in(rows.len(), alloc)?;
    for (row, &code) in dict.codes().iter().enumerate() {
        let code = u64::from(code);
        codes.push(if plant::on(Plant::DictCodePlusOne) && row > 0 { code + 1 } else { code });
    }
    encode_ints(&codes, IntPolicy::CONSTANT, alloc, out)?;
    Ok(out.len() - start)
}

/// A column of ids, checked once by [`IdView::open`]: reads through it check nothing again.
#[derive(Clone, Copy, Debug)]
pub struct IdView<'a, const W: usize> {
    table: &'a [[u8; W]],
    codes: IntView<'a>,
}

impl<'a, const W: usize> IdView<'a, W> {
    /// The column that is all of `bytes`: its width must be `W`, and every code must name an
    /// id of the table (checked here, once, by decoding the codes).
    pub fn open(bytes: &'a [u8]) -> Result<IdView<'a, W>, EncError> {
        if read8(bytes, 0, "a column's kind")? != KIND_IDS {
            return Err(EncError::Corrupt { what: "not a column of ids", at: 0 });
        }
        if usize::from(read8(bytes, 1, "an id's width")?) != W {
            return Err(EncError::Corrupt { what: "ids of another width", at: 1 });
        }
        let distinct = blaze_int::idx(read32(bytes, 2, "an id column's table")?);
        let table_end = 6 + W * distinct;
        let Some(table) = bytes.get(6..table_end) else {
            return Err(EncError::Corrupt { what: "a table past the column's end", at: 6 });
        };
        let (table, _) = table.as_chunks::<W>();
        let codes = IntView::open(&bytes[table_end..]).map_err(|e| match e {
            EncError::Corrupt { what, at } => EncError::Corrupt { what, at: at + table_end },
            other => other,
        })?;
        // Verify once: no code past the table, so `get` indexes without a check that can fail.
        let mut block = [0u64; super::bits::BLOCK];
        for b in 0..codes.blocks() {
            let n = codes.decode_block(b, &mut block);
            if block[..n].iter().any(|&code| code >= distinct.widen_to::<u64>()) {
                return Err(EncError::Corrupt { what: "a code past the id table", at: table_end });
            }
        }
        Ok(IdView { table, codes })
    }

    pub fn len(&self) -> usize {
        self.codes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    /// The distinct ids, in order of first use.
    pub fn table(&self) -> &'a [[u8; W]] {
        self.table
    }

    /// Row `i`'s id, `i` below `len`.
    #[inline]
    pub fn get(&self, i: usize) -> &'a [u8; W] {
        // `open` saw every code below the table's length.
        let code = usize::try_from(self.codes.get(i)).unwrap_or(usize::MAX);
        &self.table[code]
    }
}
