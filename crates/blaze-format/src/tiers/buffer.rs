//! Whole buffers in the warm and cold tiers, codec by column (docs/v2/plan/design/data.md,
//! "Compression follows the columns"): the store sees a value as bytes, so the column codecs
//! are applied here, where the buffer's schema says where its columns are.
//!
//! A buffer's word tables ([`WordTable`]: the items of every slice of word-aligned records
//! reachable from its root, found by [`Record::columns`]) are cut out of it, and each word of
//! a row is encoded as a column of its own, a struct split into its fields as Parquet and
//! BtrBlocks split one; the rest of the buffer is kept as it is. Both forms give back the exact
//! bytes, so every reference in the buffer, counted from where it sits, is the same after.
//!
//! The form: a version, the tier, the buffer's length, the rest's length, the rest, then for
//! each table in order the bytes of the rest before it, its rows, its stride, and for each of
//! its `stride` columns the encoding's length and the encoding. Warm: each column packed by
//! frame of reference in SIMD-BP128 blocks ([`super::Packed`]), decoded block by block, no
//! zstd. Cold: each column in its smallest codec ([`super::raw_cold`]), then zstd over the
//! whole form with the schema's dictionary.

use super::{Bytes, CORRUPT, ColdDictionary, MOST_RAW, compress, decode_raw, decompress, put_packed, raw_cold};
use crate::{ALIGN, Aligned, FormatError, Record, SchemaHash, WordTable, bytes_of, leb, open_as};

const FORM_VERSION: u8 = 2;
const WARM: u8 = 0;
const COLD: u8 = 1;

/// A table of fewer rows is kept as one column of all its words: a column's frame costs a few
/// bytes, more than splitting so few rows saves.
const SPLIT_ROWS: usize = 16;

/// Where the word tables of a buffer of schema `schema`, whose root is an `R`, are, in order
/// and none overlapping: two that overlap are one table of one word a row over both. The
/// buffer is checked whole first, as [`open_as`] checks it.
pub fn columns_of<R: Record>(buffer: &[u8], schema: SchemaHash) -> Result<Vec<WordTable>, FormatError> {
    let copy;
    let buffer = match (buffer.as_ptr() as usize).is_multiple_of(ALIGN) {
        true => buffer,
        false => {
            copy = Aligned::copy_from(buffer);
            &*copy
        }
    };
    let root = open_as::<R>(buffer, schema)?;
    // alloc: the columns of a buffer moved to a tier, once a buffer
    let mut found = Vec::new();
    R::columns(bytes_of(root), buffer, &mut found);
    Ok(merged(found))
}

fn end(table: &WordTable) -> usize {
    table.at + 4 * table.rows * table.stride
}

fn merged(mut tables: Vec<WordTable>) -> Vec<WordTable> {
    tables.sort_unstable();
    let mut out: Vec<WordTable> = Vec::with_capacity(tables.len());
    for table in tables {
        match out.last_mut() {
            Some(last) if *last == table => {}
            Some(last) if table.at < end(last) => {
                let words = (end(&table).max(end(last)) - last.at) / 4;
                *last = WordTable { at: last.at, rows: words, stride: 1 };
            }
            _ => out.push(table),
        }
    }
    out
}

/// The rows and stride `table` is encoded with.
fn shape(table: &WordTable) -> (usize, usize) {
    match table.rows < SPLIT_ROWS {
        true => (table.rows * table.stride, 1),
        false => (table.rows, table.stride),
    }
}

fn encode(buffer: &[u8], tables: &[WordTable], tier: u8) -> Vec<u8> {
    let table_bytes: usize = tables.iter().map(|table| end(table) - table.at).sum();
    let mut out = Vec::with_capacity(buffer.len() + 32 + 12 * tables.len());
    out.extend([FORM_VERSION, tier]);
    leb::write_u64(&mut out, buffer.len() as u64);
    leb::write_u64(&mut out, (buffer.len() - table_bytes) as u64);
    let mut last = 0;
    for table in tables {
        out.extend_from_slice(&buffer[last..table.at]);
        last = end(table);
    }
    out.extend_from_slice(&buffer[last..]);
    // alloc: buffers reused over the buffer's tables, once a buffer encoded
    let (mut last, mut words, mut values, mut column) = (0, Vec::new(), Vec::new(), Vec::new());
    for table in tables {
        let (rows, stride) = shape(table);
        words.clear();
        words.extend(
            buffer[table.at..end(table)]
                .chunks_exact(4)
                .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]])),
        );
        leb::write_u64(&mut out, (table.at - last) as u64);
        leb::write_u64(&mut out, rows as u64);
        leb::write_u64(&mut out, stride as u64);
        for field in 0..stride {
            values.clear();
            values.extend(words[field..].iter().step_by(stride));
            let cold;
            let encoded: &[u8] = match tier {
                WARM => {
                    column.clear();
                    put_packed(&mut column, &values);
                    &column
                }
                _ => {
                    cold = raw_cold(&values);
                    &cold
                }
            };
            leb::write_u64(&mut out, encoded.len() as u64);
            out.extend_from_slice(encoded);
        }
        last = end(table);
    }
    out
}

fn count(bytes: &mut Bytes<'_>) -> Result<usize, FormatError> {
    usize::try_from(bytes.varint()?).map_err(|_| CORRUPT)
}

fn decode(form: &[u8], tier: u8) -> Result<Vec<u8>, FormatError> {
    let mut bytes = Bytes { bytes: form, at: 0 };
    if bytes.byte()? != FORM_VERSION || bytes.byte()? != tier {
        return Err(FormatError::Header("buffer columns of another version or tier"));
    }
    let len = bytes.varint()?;
    let rest_len = bytes.varint()?;
    if len > MOST_RAW || rest_len > len {
        return Err(CORRUPT);
    }
    let rest_end = bytes.at.checked_add(rest_len as usize).filter(|&end| end <= form.len()).ok_or(CORRUPT)?;
    let rest = &form[bytes.at..rest_end];
    bytes.at = rest_end;
    let len = len as usize;
    let mut out = Vec::with_capacity(len.min(form.len().saturating_mul(64)));
    // alloc: once a buffer decoded from its tier
    let (mut taken, mut fields) = (0usize, Vec::new());
    while bytes.at < form.len() {
        let gap = count(&mut bytes)?;
        let rows = count(&mut bytes)?;
        let stride = count(&mut bytes)?;
        let before = taken.checked_add(gap).filter(|&before| before <= rest.len()).ok_or(CORRUPT)?;
        let words = rows.checked_mul(stride).ok_or(CORRUPT)?;
        if rows == 0 || stride == 0 || words > (len - out.len().min(len)) / 4 {
            return Err(CORRUPT);
        }
        fields.clear();
        for _ in 0..stride {
            let encoded = count(&mut bytes)?;
            let end = bytes.at.checked_add(encoded).filter(|&end| end <= form.len()).ok_or(CORRUPT)?;
            let column = &form[bytes.at..end];
            bytes.at = end;
            let values = match tier {
                WARM => {
                    let mut packed = Bytes { bytes: column, at: 0 };
                    let values = packed.packed(rows)?;
                    if packed.at != column.len() {
                        return Err(CORRUPT);
                    }
                    values
                }
                _ => decode_raw(column)?,
            };
            if values.len() != rows {
                return Err(CORRUPT);
            }
            fields.push(values);
        }
        out.extend_from_slice(&rest[taken..before]);
        taken = before;
        out.reserve(4 * words);
        match &fields[..] {
            [values] => out.extend(values.iter().flat_map(|value| value.to_le_bytes())),
            _ => (0..rows).for_each(|row| out.extend(fields.iter().flat_map(|field| field[row].to_le_bytes()))),
        }
    }
    out.extend_from_slice(&rest[taken..]);
    match out.len() == len {
        true => Ok(out),
        false => Err(CORRUPT),
    }
}

/// The warm and cold forms of the buffers of one schema: what `blaze_cas::Store::encode_columns`
/// is given for that schema (`blaze-cas` depends on this crate). Made by [`BufferColumns::of`]
/// with the schema's root record type, so the columns are found from the schema, never guessed
/// from the bytes.
///
/// The warm form is not read in place: a reader of a warm buffer decodes it with
/// [`BufferColumns::hot_from_warm`], which unpacks 128 values a block.
#[derive(Clone, Debug)]
pub struct BufferColumns {
    schema: SchemaHash,
    find: fn(&[u8], SchemaHash) -> Result<Vec<WordTable>, FormatError>,
    dictionary: Option<ColdDictionary>,
}

impl BufferColumns {
    /// The forms of buffers of `schema` whose root is an `R`: a [`crate::Persist`] type's
    /// schema and the root record its `write` finishes with.
    pub fn of<R: Record>(schema: SchemaHash) -> BufferColumns {
        BufferColumns { schema, find: columns_of::<R>, dictionary: None }
    }

    /// Cold forms compressed with `dictionary`, trained on the schema's buffers
    /// ([`ColdDictionary::train`]).
    pub fn with_dictionary(self, dictionary: ColdDictionary) -> BufferColumns {
        BufferColumns { dictionary: Some(dictionary), ..self }
    }

    pub fn schema(&self) -> SchemaHash {
        self.schema
    }

    pub fn warm(&self, hot: &[u8]) -> Result<Vec<u8>, FormatError> {
        Ok(encode(hot, &(self.find)(hot, self.schema)?, WARM))
    }

    pub fn hot_from_warm(&self, warm: &[u8]) -> Result<Vec<u8>, FormatError> {
        decode(warm, WARM)
    }

    pub fn cold(&self, hot: &[u8]) -> Result<Vec<u8>, FormatError> {
        let form = encode(hot, &(self.find)(hot, self.schema)?, COLD);
        Ok(compress(&form, self.dictionary.as_ref()))
    }

    pub fn hot_from_cold(&self, cold: &[u8]) -> Result<Vec<u8>, FormatError> {
        decode(&decompress(cold, self.dictionary.as_ref())?, COLD)
    }
}
