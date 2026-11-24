//! Macro expansions as their own records, keyed by content (runtime.md, plan row B3; "The end
//! state" 4: "expansions as their own records by content").
//!
//! A crate's `CrateScope` record held every text its macro calls made, with their origins, in
//! one section: 48 MB of the core library's 83 MB. Resolving names does not read that text once
//! the expansion is outlined and its items declared; only a body a macro wrote, or a read of
//! such an item's text, does, and for a dependency whose bodies are never checked almost none
//! is. So the text leaves the crate's record:
//!
//! - An EXPANSION RECORD ([`ExpansionRecord`]) holds one expansion's text and the COLUMN of its
//!   origins (each rendered token's place in the text and where it came from, input tokens by
//!   index: blaze-expand origin_column.rs), under the digest of its own bytes ([`record_key`]).
//!   Both are functions of the expansion alone (the macro's definition, the call's input tokens,
//!   their marks and edition), never of where the call stands (rules 21, 25, 71), so two calls of
//!   equal input (`impl_int!` for one type in two places, `debug_assert!` with equal tokens)
//!   make one record. The key is the record's content, not its inputs: the record is never
//!   looked up before the expansion is made (the names task expands, then files it), and the
//!   cell a reader of the text records is `NameCell::Expansion`, whose digest covers the items
//!   row's text id and the directory row's record key (names_cells.rs `expansion_digest`): the
//!   text and the column the record holds, so a reader is entered when either moves.
//! - The crate's DIRECTORY ([`Directory`]), the section the texts were in, holds a row a call:
//!   its key, its record's key, the text's length, the column's length and the CALL PART of its
//!   origins (the calling text, the entry, where the input starts, the macro's path: blaze-scope
//!   expansion_origins.rs, everything before the column). The items the expansion declared are
//!   the items record's rows already; the directory adds none.
//!
//! Form of a record ([`RECORD_FORM`] 1), little-endian: the form u32, the text's length u32, the
//! column's length u32, a zero u32, the text, zeros to a multiple of 4, the column. Positions in
//! the column are relative to the text (rule 71).
//!
//! Form of the directory ([`DIRECTORY_FORM`] 4, after the texts section's 3: blaze-cli
//! names/expansion_texts.rs), little-endian, 16-aligned as a section: the count u32, the form
//! u32, the count of distinct records u32, a zero u32; then `count` rows of [`DIRECTORY_ROW`]
//! bytes sorted by call key (call key 32, record key 32, text length u32, column length u32, the
//! call part's start u32 from the blob's start and its length u32: 0 when the text keeps no
//! origins); then the blob, the call parts in row order. A function of the crate's expansions
//! only. Sans-IO: the caller puts the records and reads them back.
#![blaze::sans_io]

use blaze_id::{ContentId, ItemKey};
use blaze_ids::{Domain, Hasher};
use blaze_own::{AVec, Alloc};

use crate::expansion_origins::{OriginsError, OriginsView, split};
use crate::resolve::CrateNames;

/// The domain an expansion record's key is the digest of its bytes in. Its number moves with
/// [`RECORD_FORM`].
pub const RECORD_DOMAIN: &str = "blaze-scope expansion record 2";
static RECORDS: Domain = Domain::new(RECORD_DOMAIN);
static DEDUP: Domain = Domain::new("blaze-scope expansion dedup 2");

/// An expansion record's form (form 2 adds optional RFC 8878 zstd compression of the payload).
pub const RECORD_FORM: u32 = 2;
/// An expansion record's head: form, text length, column length, flags (`0` raw, `1` zstd, `2` delta-columnar + zstd).
pub const RECORD_HEAD: usize = 16;
const FLAG_RAW: u32 = 0;
const FLAG_ZSTD: u32 = 1;
const FLAG_ZSTD_COL: u32 = 2;
/// Payloads shorter than this stay raw (below zstd's frame header break-even).
const COMPRESS_MIN: usize = 128;

const COL_HEAD: usize = 8;
const COL_MARK: usize = blaze_expand::origin_column::MARK;
const COL_ROW: usize = blaze_expand::origin_column::ROW;

fn push_varint(mut v: u32, out: &mut AVec<'_, u8>) {
    while v >= 0x80 {
        out.push(u8::try_from((v & 0x7F) | 0x80).unwrap_or(0x80));
        v >>= 7;
    }
    out.push(u8::try_from(v).unwrap_or(0));
}

fn read_varint(bytes: &[u8], at: &mut usize) -> Option<u32> {
    let mut val = 0u32;
    let mut shift = 0u32;
    loop {
        let b = *bytes.get(*at)?;
        *at += 1;
        val |= u32::from(b & 0x7F).checked_shl(shift)?;
        if b & 0x80 == 0 {
            return Some(val);
        }
        shift += 7;
        if shift >= 35 {
            return None;
        }
    }
}

fn zigzag_enc(v: u32) -> u32 {
    let s = v.cast_signed();
    ((s << 1) ^ (s >> 31)).cast_unsigned()
}

fn zigzag_dec(v: u32) -> u32 {
    (v >> 1) ^ (0u32.wrapping_sub(v & 1))
}

/// Losslessly transforms an `OriginColumn` (`[head | marks | 20 B rows]`) into `[head | marks |
/// 5 transposed delta+zigzag+varint streams]` (`d_start`, `len`, `tag`, `d_value`, `d_mark`) so
/// monotonic token positions and constant mark runs crush 10–15× under zstd. Returns `false` if
/// `column` is empty or does not have standard `OriginColumn` geometry (falls back to `FLAG_ZSTD`).
fn try_encode_col_delta(column: &[u8], out: &mut AVec<'_, u8>) -> bool {
    if column.len() < COL_HEAD {
        return false;
    }
    let row_count = usize::try_from(u32_at(column, 0)).unwrap_or(usize::MAX);
    let mark_count = usize::try_from(u32_at(column, 4)).unwrap_or(usize::MAX);
    if row_count == 0 {
        return false;
    }
    let Some(marks_end) = mark_count.checked_mul(COL_MARK).and_then(|n| n.checked_add(COL_HEAD)) else {
        return false;
    };
    let Some(want) = row_count.checked_mul(COL_ROW).and_then(|n| n.checked_add(marks_end)) else {
        return false;
    };
    if want != column.len() {
        return false;
    }
    let Some(rows) = column.get(marks_end..) else {
        return false;
    };
    // Verify all tags fit in u8 before committing to the transform.
    for chunk in rows.chunks_exact(COL_ROW) {
        if u32_at(chunk, 8) > u32::from(u8::MAX) {
            return false;
        }
    }
    out.extend_from_slice(column.get(..marks_end).unwrap_or(&[]));
    // Stream 1: d_start = start - prev_end (zigzag varint).
    let mut prev_end = 0u32;
    for chunk in rows.chunks_exact(COL_ROW) {
        let start = u32_at(chunk, 0);
        let end = u32_at(chunk, 4);
        push_varint(zigzag_enc(start.wrapping_sub(prev_end)), out);
        prev_end = end;
    }
    // Stream 2: len = end - start (zigzag varint).
    for chunk in rows.chunks_exact(COL_ROW) {
        let start = u32_at(chunk, 0);
        let end = u32_at(chunk, 4);
        push_varint(zigzag_enc(end.wrapping_sub(start)), out);
    }
    // Stream 3: tag (1 byte per token).
    for chunk in rows.chunks_exact(COL_ROW) {
        out.push(u8::try_from(u32_at(chunk, 8)).unwrap_or(0));
    }
    // Stream 4: d_value = value - prev_value (zigzag varint).
    let mut prev_val = 0u32;
    for chunk in rows.chunks_exact(COL_ROW) {
        let val = u32_at(chunk, 12);
        push_varint(zigzag_enc(val.wrapping_sub(prev_val)), out);
        prev_val = val;
    }
    // Stream 5: d_mark = mark - prev_mark (zigzag varint).
    let mut prev_mark = blaze_expand::origin_column::NO_MARK;
    for chunk in rows.chunks_exact(COL_ROW) {
        let mark = u32_at(chunk, 16);
        push_varint(zigzag_enc(mark.wrapping_sub(prev_mark)), out);
        prev_mark = mark;
    }
    true
}

/// Exact inverse of [`try_encode_col_delta`]: reconstructs canonical `OriginColumn` bytes into `dst`.
fn decode_col_delta(src: &[u8], want_col_len: usize, dst: &mut [u8]) -> Result<(), ExpansionRecordsError> {
    let short = || ExpansionRecordsError::Short { what: "an expansion record", len: src.len(), want: want_col_len };
    if src.len() < COL_HEAD || dst.len() != want_col_len {
        return Err(short());
    }
    let row_count = usize::try_from(u32_at(src, 0)).unwrap_or(usize::MAX);
    let mark_count = usize::try_from(u32_at(src, 4)).unwrap_or(usize::MAX);
    let marks_end = mark_count.checked_mul(COL_MARK).and_then(|n| n.checked_add(COL_HEAD)).ok_or_else(short)?;
    let expected = row_count.checked_mul(COL_ROW).and_then(|n| n.checked_add(marks_end)).ok_or_else(short)?;
    if expected != want_col_len || src.len() < marks_end {
        return Err(short());
    }
    if let (Some(head_src), Some(head_dst)) = (src.get(..marks_end), dst.get_mut(..marks_end)) {
        head_dst.copy_from_slice(head_src);
    } else {
        return Err(short());
    }
    let mut at = marks_end;
    let Some(rows_dst) = dst.get_mut(marks_end..) else {
        return Err(short());
    };
    // Stream 1: d_start (store raw delta temporarily in word 0, finish after reading len in stream 2).
    for chunk in rows_dst.chunks_exact_mut(COL_ROW) {
        let d_start = zigzag_dec(read_varint(src, &mut at).ok_or_else(short)?);
        if let Some(slot) = chunk.get_mut(0..4) {
            slot.copy_from_slice(&d_start.to_le_bytes());
        }
    }
    // Stream 2: len -> reconstruct absolute start & end.
    let mut prev_end = 0u32;
    for chunk in rows_dst.chunks_exact_mut(COL_ROW) {
        let d_start = u32_at(chunk, 0);
        let len = zigzag_dec(read_varint(src, &mut at).ok_or_else(short)?);
        let start = prev_end.wrapping_add(d_start);
        let end = start.wrapping_add(len);
        if let Some(slot) = chunk.get_mut(0..4) {
            slot.copy_from_slice(&start.to_le_bytes());
        }
        if let Some(slot) = chunk.get_mut(4..8) {
            slot.copy_from_slice(&end.to_le_bytes());
        }
        prev_end = end;
    }
    // Stream 3: tag.
    for chunk in rows_dst.chunks_exact_mut(COL_ROW) {
        let tag = u32::from(*src.get(at).ok_or_else(short)?);
        at += 1;
        if let Some(slot) = chunk.get_mut(8..12) {
            slot.copy_from_slice(&tag.to_le_bytes());
        }
    }
    // Stream 4: d_value.
    let mut prev_val = 0u32;
    for chunk in rows_dst.chunks_exact_mut(COL_ROW) {
        let dv = zigzag_dec(read_varint(src, &mut at).ok_or_else(short)?);
        let val = prev_val.wrapping_add(dv);
        if let Some(slot) = chunk.get_mut(12..16) {
            slot.copy_from_slice(&val.to_le_bytes());
        }
        prev_val = val;
    }
    // Stream 5: d_mark.
    let mut prev_mark = blaze_expand::origin_column::NO_MARK;
    for chunk in rows_dst.chunks_exact_mut(COL_ROW) {
        let dm = zigzag_dec(read_varint(src, &mut at).ok_or_else(short)?);
        let mark = prev_mark.wrapping_add(dm);
        if let Some(slot) = chunk.get_mut(16..20) {
            slot.copy_from_slice(&mark.to_le_bytes());
        }
        prev_mark = mark;
    }
    if at != src.len() {
        return Err(short());
    }
    Ok(())
}

/// The directory's form: 5 deduplicates `record_keys` into a table right after the header and
/// stores a `u32` index per row (`DIRECTORY_ROW` 80 → 52 B), refusing older forms cleanly (rule 26).
pub const DIRECTORY_FORM: u32 = 5;
const DIRECTORY_HEAD: usize = 16;
/// One directory row: call key (32), record key index (4), text length (4), column length (4),
/// call part's start (4) and length (4).
pub const DIRECTORY_ROW: usize = 32 + 4 + 4 * 4;

/// Why expansion records or a directory could not be written or read (rule: blaze-scope
/// expansion_records.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExpansionRecordsError {
    /// More than a u32 can count or place (`what`: the calls, a text, a column, the blob).
    TooBig { what: &'static str, bytes: usize },
    /// A call's origins do not cut into a call part and a column: a writer's bug.
    Origins { call: ItemKey, error: OriginsError },
    /// Two expansions of one record key with other bytes (rule 21: the fix is the key).
    TwoValues { key: ContentId, first: ItemKey, second: ItemKey },
    /// Bytes shorter than their head or rows say.
    Short { what: &'static str, len: usize, want: usize },
    /// Another form than this reader's: the record is rebuilt (rule 26), never migrated.
    OldForm { what: &'static str, found: u32 },
    /// A row's call part runs past the blob.
    Range { call: ItemKey, start: u32, len: u32 },
    /// A record's text is not UTF-8 (a writer's bug or a damaged record).
    Utf8,
    /// A record whose text or column is not as long as the directory's row says: the record
    /// served for `call` is not the one its row names.
    NotItsRecord { call: ItemKey, key: ContentId },
    /// No record for the call's row: what a reader names instead of expanding again (rule 3).
    Missing { call: ItemKey, key: ContentId },
    /// An unknown payload flag in the expansion record head.
    BadFlags { found: u32 },
    /// Zstandard compression or decompression failed on an expansion record payload.
    Zstd(blaze_zstd::ZstdError),
}

impl core::fmt::Display for ExpansionRecordsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        const RULE: &str = "(rule: blaze-scope expansion_records.rs)";
        match self {
            ExpansionRecordsError::TooBig { what, bytes } => write!(f, "{what} of {bytes} do not fit a u32 {RULE}"),
            ExpansionRecordsError::Origins { call, error } => write!(f, "the origins of macro call {call:?} do not cut into a call part and a column: {error} {RULE}"),
            ExpansionRecordsError::TwoValues { key, first, second } => {
                write!(f, "macro calls {first:?} and {second:?} made other expansions under one record key {key:?} {RULE}")
            }
            ExpansionRecordsError::Short { what, len, want } => write!(f, "{what} of {len} bytes is shorter than the {want} its head says {RULE}"),
            ExpansionRecordsError::OldForm { what, found } => write!(f, "{what} of form {found}: the crate's record is rebuilt (rule 26) {RULE}"),
            ExpansionRecordsError::Range { call, start, len } => write!(f, "the call part of macro call {call:?} at {start}+{len} runs past its directory {RULE}"),
            ExpansionRecordsError::Utf8 => write!(f, "an expansion record's text is not UTF-8 {RULE}"),
            ExpansionRecordsError::NotItsRecord { call, key } => write!(f, "the expansion record {key:?} served for macro call {call:?} is not the one its directory row names {RULE}"),
            ExpansionRecordsError::Missing { call, key } => write!(f, "the store holds no expansion record {key:?} for macro call {call:?} {RULE}"),
            ExpansionRecordsError::BadFlags { found } => write!(f, "an expansion record has unknown flags {found} {RULE}"),
            ExpansionRecordsError::Zstd(error) => write!(f, "an expansion record's zstd codec failed: {error} {RULE}"),
        }
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

fn fits(n: usize, what: &'static str) -> Result<u32, ExpansionRecordsError> {
    u32::try_from(n).map_err(|_| ExpansionRecordsError::TooBig { what, bytes: n })
}

/// The head of the record of `text` with `column` and `flags`.
fn head_with_flags(text: &str, column: &[u8], flags: u32) -> Result<[u8; RECORD_HEAD], ExpansionRecordsError> {
    let mut head = [0u8; RECORD_HEAD];
    head[..4].copy_from_slice(&RECORD_FORM.to_le_bytes());
    head[4..8].copy_from_slice(&fits(text.len(), "an expansion's text")?.to_le_bytes());
    head[8..12].copy_from_slice(&fits(column.len(), "an expansion's column")?.to_le_bytes());
    head[12..16].copy_from_slice(&flags.to_le_bytes());
    Ok(head)
}

const ZEROS: [u8; 3] = [0; 3];

/// The canonical uncompressed dedup key of `(text, column)` in [`DEDUP`], hashed from its parts,
/// never joined; equal to `ContentId::of_domain(&DEDUP, raw_bytes)`.
pub fn key_of(text: &str, column: &[u8]) -> Result<ContentId, ExpansionRecordsError> {
    let head = head_with_flags(text, column, FLAG_RAW)?;
    let mut h = Hasher::of(&DEDUP);
    h.update(&head).update(text.as_bytes()).update(&ZEROS[..pad4(text.len()) - text.len()]).update(column);
    Ok(h.finish())
}

/// The key of the record `bytes`: the digest of its exact stored bytes.
pub fn record_key(bytes: &[u8]) -> ContentId {
    ContentId::of_domain(&RECORDS, bytes)
}

/// One expansion record, read where it lies (raw) or decompressed into the reader's frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExpansionRecord<'s> {
    pub text: &'s str,
    /// The origin column (blaze-expand `OriginsRef::open`'s form); empty when the text keeps no
    /// origins.
    pub column: &'s [u8],
}

impl<'s> ExpansionRecord<'s> {
    /// Writes the raw (uncompressed, `FLAG_RAW`) record of `text` with `column` to `out`.
    pub fn write_raw(text: &str, column: &[u8], out: &mut AVec<'_, u8>) -> Result<(), ExpansionRecordsError> {
        let head = head_with_flags(text, column, FLAG_RAW)?;
        out.reserve(RECORD_HEAD + pad4(text.len()) + column.len());
        out.extend_from_slice(&head);
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(&ZEROS[..pad4(text.len()) - text.len()]);
        out.extend_from_slice(column);
        Ok(())
    }

    /// Writes the record of `text` with `column` to `out`, compressing `[text | pad4 | column]`
    /// with `encoder` when it saves bytes (deterministic function of `text` and `column`, rule 25).
    fn write_with_encoder<'a>(
        text: &str,
        column: &[u8],
        encoder: &mut blaze_zstd::ZstdEncoder<Alloc<'a>>,
        raw_buf: &mut AVec<'a, u8>,
        comp_buf: &mut AVec<'a, u8>,
        out: &mut AVec<'a, u8>,
    ) -> Result<(), ExpansionRecordsError> {
        let pad = pad4(text.len()) - text.len();
        let raw_len = text.len() + pad + column.len();
        if raw_len >= COMPRESS_MIN {
            raw_buf.clear();
            raw_buf.reserve(raw_len);
            raw_buf.extend_from_slice(text.as_bytes());
            if let Some(zeros) = ZEROS.get(..pad) {
                raw_buf.extend_from_slice(zeros);
            }
            let col_transformed = try_encode_col_delta(column, raw_buf);
            if !col_transformed {
                raw_buf.extend_from_slice(column);
            }
            comp_buf.clear();
            encoder
                .compress_into(raw_buf.as_slice(), blaze_zstd::CompressOpts::FAST, comp_buf)
                .map_err(ExpansionRecordsError::Zstd)?;
            if comp_buf.len() < raw_len {
                let flag = if col_transformed { FLAG_ZSTD_COL } else { FLAG_ZSTD };
                let head = head_with_flags(text, column, flag)?;
                out.reserve(RECORD_HEAD + comp_buf.len());
                out.extend_from_slice(&head);
                out.extend_from_slice(comp_buf.as_slice());
                return Ok(());
            }
            return Self::write_raw(text, column, out);
        }
        Self::write_raw(text, column, out)
    }

    /// The record of `text` with `column`, appended to `out` (zstd-compressed when smaller).
    pub fn write<'a>(text: &str, column: &[u8], out: &mut AVec<'a, u8>) -> Result<(), ExpansionRecordsError> {
        let alloc = *out.allocator();
        let mut encoder = blaze_zstd::ZstdEncoder::try_new_in(alloc).map_err(ExpansionRecordsError::Zstd)?;
        let mut raw_buf = AVec::new_in(alloc);
        let mut comp_buf = AVec::new_in(alloc);
        Self::write_with_encoder(text, column, &mut encoder, &mut raw_buf, &mut comp_buf, out)
    }

    /// Opens an expansion record from `bytes`: zero-copy when stored raw (`FLAG_RAW`), or
    /// decompressed into `bump` when zstd-compressed (`FLAG_ZSTD`).
    pub fn open_in(bytes: &'s [u8], bump: blaze_own::Bump<'s>) -> Result<ExpansionRecord<'s>, ExpansionRecordsError> {
        if bytes.len() < RECORD_HEAD {
            return Err(ExpansionRecordsError::Short { what: "an expansion record", len: bytes.len(), want: RECORD_HEAD });
        }
        let found = u32_at(bytes, 0);
        if found != RECORD_FORM {
            return Err(ExpansionRecordsError::OldForm { what: "an expansion record", found });
        }
        let text_len = usize::try_from(u32_at(bytes, 4)).unwrap_or(usize::MAX);
        let column_len = usize::try_from(u32_at(bytes, 8)).unwrap_or(usize::MAX);
        let flags = u32_at(bytes, 12);
        let payload_col_at = pad4(text_len);
        let want_payload = payload_col_at.saturating_add(column_len);
        let payload: &'s [u8] = match flags {
            FLAG_RAW => {
                let want = RECORD_HEAD.saturating_add(want_payload);
                if bytes.len() != want {
                    return Err(ExpansionRecordsError::Short { what: "an expansion record", len: bytes.len(), want });
                }
                bytes.get(RECORD_HEAD..).unwrap_or(&[])
            }
            FLAG_ZSTD => {
                let compressed = bytes.get(RECORD_HEAD..).unwrap_or(&[]);
                let limits = blaze_zstd::ZstdLimits {
                    most_output_bytes: want_payload,
                    most_window_bytes: 8 << 20,
                    most_frames: 4,
                };
                let decoded = blaze_zstd::decompress_in(compressed, limits, bump).map_err(ExpansionRecordsError::Zstd)?;
                if decoded.len() != want_payload {
                    return Err(ExpansionRecordsError::Short { what: "an expansion record", len: decoded.len(), want: want_payload });
                }
                decoded.into_bump_slice()
            }
            FLAG_ZSTD_COL => {
                let compressed = bytes.get(RECORD_HEAD..).unwrap_or(&[]);
                // Delta-varint stream is at most ~1.25x raw column bytes even on pathological inputs.
                let limits = blaze_zstd::ZstdLimits {
                    most_output_bytes: want_payload.saturating_add(column_len / 2).saturating_add(64),
                    most_window_bytes: 8 << 20,
                    most_frames: 4,
                };
                let decoded = blaze_zstd::decompress_in(compressed, limits, bump).map_err(ExpansionRecordsError::Zstd)?;
                let mut rebuilt = blaze_own::Vec::with_capacity_in(want_payload, bump);
                let text_part = decoded.get(..payload_col_at).ok_or(ExpansionRecordsError::Short {
                    what: "an expansion record",
                    len: decoded.len(),
                    want: payload_col_at,
                })?;
                rebuilt.extend_from_slice(text_part);
                rebuilt.resize(want_payload, 0);
                let rebuilt_len = rebuilt.len();
                let col_src = decoded.get(payload_col_at..).unwrap_or(&[]);
                let col_dst = rebuilt.get_mut(payload_col_at..).ok_or(ExpansionRecordsError::Short {
                    what: "an expansion record",
                    len: rebuilt_len,
                    want: want_payload,
                })?;
                decode_col_delta(col_src, column_len, col_dst)?;
                rebuilt.into_bump_slice()
            }
            other => return Err(ExpansionRecordsError::BadFlags { found: other }),
        };
        let text_bytes = payload.get(..text_len).ok_or(ExpansionRecordsError::Short { what: "an expansion record", len: payload.len(), want: text_len })?;
        let text = core::str::from_utf8(text_bytes).map_err(|_| ExpansionRecordsError::Utf8)?;
        let column = payload.get(payload_col_at..).unwrap_or(&[]);
        Ok(ExpansionRecord { text, column })
    }
}

/// One record a crate's calls name, to put under its key.
#[derive(Debug)]
pub struct Record<'a> {
    pub key: ContentId,
    pub bytes: AVec<'a, u8>,
}

/// A crate's expansions split: its directory (the section of its record) and each distinct
/// record its calls name, sorted by key, each once.
#[derive(Debug)]
pub struct Split<'a> {
    pub directory: AVec<'a, u8>,
    pub records: AVec<'a, Record<'a>>,
}

/// One call that made a text, as the writer takes it.
struct Call<'n> {
    call: ItemKey,
    text: &'n str,
    call_part: &'n [u8],
    column: &'n [u8],
    key: ContentId,
}

/// Deduplicated expansion payloads ready for parallel or sequential compression: `unique()` is
/// a pure `Sync` slice of `(text, column)` pairs so callers may fan out zstd+BLAKE3 compression
/// across worker threads without touching `CrateNames` or any non-`Sync` allocator.
pub struct Prepared<'n, 'a> {
    calls: AVec<'a, Call<'n>>,
    unique: AVec<'a, (&'n str, &'n [u8])>,
    call_to_unique: AVec<'a, u32>,
}

impl<'n, 'a> Prepared<'n, 'a> {
    /// Distinct `(text, column)` payloads to compress once each, in canonical dedup-key order.
    pub fn unique(&self) -> &[(&'n str, &'n [u8])] {
        &self.unique
    }

    /// Encodes the directory section (form 5) given `stored_keys` (one [`record_key`] per entry
    /// in [`Prepared::unique`], in the same order). Returns `(directory_bytes, unique_to_sorted_order)`
    /// where `unique_to_sorted_order` is the permutation of `0..unique.len()` sorted by `stored_keys`.
    pub fn finish_directory(self, stored_keys: &[ContentId], alloc: Alloc<'a>) -> Result<(AVec<'a, u8>, AVec<'a, usize>), ExpansionRecordsError> {
        let count = fits(self.calls.len(), "a crate's macro calls")?;
        let n_rec = stored_keys.len();
        let rec_count = fits(n_rec, "a crate's expansion records")?;
        // Deterministic record-key order (sorted by stored ContentId, rule 25).
        let mut order: AVec<'a, usize> = AVec::from_iter_in(0..n_rec, alloc);
        order.sort_unstable_by_key(|&u| stored_keys.get(u).copied().unwrap_or_default());
        let mut rank: AVec<'_, u32> = AVec::new_in(alloc);
        rank.resize(n_rec, 0);
        for (sorted_idx, &u) in order.iter().enumerate() {
            if let Some(slot) = rank.get_mut(u) {
                *slot = u32::try_from(sorted_idx).unwrap_or(u32::MAX);
            }
        }
        let blob: usize = self.calls.iter().map(|c| c.call_part.len()).sum();
        fits(blob, "a crate's call parts")?;
        let keys_bytes = n_rec.checked_mul(32).ok_or(ExpansionRecordsError::TooBig { what: "a crate's expansion record keys", bytes: usize::MAX })?;
        let mut directory = AVec::with_capacity_in(DIRECTORY_HEAD + keys_bytes + self.calls.len() * DIRECTORY_ROW + blob, alloc);
        directory.extend_from_slice(&count.to_le_bytes());
        directory.extend_from_slice(&DIRECTORY_FORM.to_le_bytes());
        directory.extend_from_slice(&rec_count.to_le_bytes());
        directory.resize(DIRECTORY_HEAD, 0);
        for &u in order.iter() {
            if let Some(k) = stored_keys.get(u) {
                directory.extend_from_slice(&k.0);
            }
        }
        let mut at = 0u32;
        for (c, &u) in self.calls.iter().zip(self.call_to_unique.iter()) {
            directory.extend_from_slice(&c.call.0.0);
            let r = usize::try_from(u).ok().and_then(|idx| rank.get(idx).copied()).unwrap_or(0);
            directory.extend_from_slice(&r.to_le_bytes());
            let text_len = u32::try_from(c.text.len()).unwrap_or(0);
            let col_len = u32::try_from(c.column.len()).unwrap_or(0);
            let part_len = u32::try_from(c.call_part.len()).unwrap_or(0);
            for word in [text_len, col_len, at, part_len] {
                directory.extend_from_slice(&word.to_le_bytes());
            }
            at = at.saturating_add(part_len);
        }
        for c in self.calls.iter() {
            directory.extend_from_slice(c.call_part);
        }
        Ok((directory, order))
    }
}

/// Compresses one `(text, column)` pair into `out` using a caller-reused `ZstdEncoder` and
/// returns its [`record_key`]. Safe to run concurrently across chunks of [`Prepared::unique`].
pub fn compress_one<'a>(
    text: &str,
    column: &[u8],
    encoder: &mut blaze_zstd::ZstdEncoder<Alloc<'a>>,
    raw_buf: &mut AVec<'a, u8>,
    comp_buf: &mut AVec<'a, u8>,
    out: &mut AVec<'a, u8>,
) -> Result<ContentId, ExpansionRecordsError> {
    out.clear();
    ExpansionRecord::write_with_encoder(text, column, encoder, raw_buf, comp_buf, out)?;
    Ok(record_key(out))
}

/// Compresses a slice of unique `(text, column)` entries using one reused `ZstdEncoder` in
/// `scratch`, copying each compressed record directly into an exact-sized `out_alloc` buffer.
pub fn compress_chunk<A: core::alloc::Allocator + Copy>(
    chunk: &[(&str, &[u8])],
    scratch: Alloc<'_>,
    out_alloc: A,
) -> Result<blaze_own::Vec<(ContentId, blaze_own::Vec<u8, A>), A>, ExpansionRecordsError> {
    let mut encoder = blaze_zstd::ZstdEncoder::try_new_in(scratch).map_err(ExpansionRecordsError::Zstd)?;
    let mut raw_buf: AVec<'_, u8> = AVec::new_in(scratch);
    let mut comp_buf: AVec<'_, u8> = AVec::new_in(scratch);
    let mut tmp_out: AVec<'_, u8> = AVec::new_in(scratch);
    let mut out = blaze_own::Vec::with_capacity_in(chunk.len(), out_alloc);
    for &(text, column) in chunk {
        let key = compress_one(text, column, &mut encoder, &mut raw_buf, &mut comp_buf, &mut tmp_out)?;
        let mut owned = blaze_own::Vec::with_capacity_in(tmp_out.len(), out_alloc);
        owned.extend_from_slice(&tmp_out);
        out.push((key, owned));
    }
    Ok(out)
}

/// Collects and deduplicates `names`' expansions on canonical uncompressed content before
/// compression so callers can compress [`Prepared::unique`] sequentially or in parallel.
pub fn prepare<'n, 'a>(names: &'n CrateNames<'n>, alloc: Alloc<'a>) -> Result<Prepared<'n, 'a>, ExpansionRecordsError> {
    let mut calls: AVec<'a, Call<'n>> = AVec::new_in(alloc);
    for (call, expansion) in names.expansions.iter() {
        let Some(made) = expansion.made else { continue };
        let (call_part, column) = match made.origins {
            Ok(origins) => split(origins).map_err(|error| ExpansionRecordsError::Origins { call: *call, error })?,
            Err(_) => (&[][..], &[][..]),
        };
        calls.push(Call { call: *call, text: made.text, call_part, column, key: ContentId::default() });
    }
    fill_keys(&mut calls, alloc)?;
    // Sorted by call key: the map's order is the machine's (rule 25).
    calls.sort_unstable_by_key(|c| c.call);
    let _ = fits(calls.len(), "a crate's macro calls")?;

    let mut by_key: AVec<'_, (ContentId, usize)> = AVec::with_capacity_in(calls.len(), alloc);
    by_key.extend(calls.iter().enumerate().map(|(i, c)| (c.key, i)));
    by_key.sort_unstable();

    let mut unique: AVec<'a, (&'n str, &'n [u8])> = AVec::new_in(alloc);
    let mut call_to_unique: AVec<'a, u32> = AVec::new_in(alloc);
    call_to_unique.resize(calls.len(), 0);
    let mut last: Option<(ContentId, usize, u32)> = None;
    for &(dedup_key, i) in by_key.iter() {
        let Some(ci) = calls.get(i) else { continue };
        if let Some((_, first, uid)) = last.filter(|(held, _, _)| *held == dedup_key) {
            if let Some(a) = calls.get(first)
                && (a.text != ci.text || a.column != ci.column)
            {
                return Err(ExpansionRecordsError::TwoValues { key: dedup_key, first: a.call, second: ci.call });
            }
            if let Some(slot) = call_to_unique.get_mut(i) {
                *slot = uid;
            }
            continue;
        }
        let uid = fits(unique.len(), "a crate's expansion records")?;
        unique.push((ci.text, ci.column));
        if let Some(slot) = call_to_unique.get_mut(i) {
            *slot = uid;
        }
        last = Some((dedup_key, i, uid));
    }
    Ok(Prepared { calls, unique, call_to_unique })
}

/// The directory and the records of `names`' expansions, in `alloc`: every call whose expansion
/// made a text (the items record's expansion rows are the same set, in the same order). A text
/// whose origins are `Err` (a call's statements read as items, a block's module text, a column
/// that could not be made) keeps none: its row's call part and its record's column are empty,
/// and its reader answers "no origins kept", typed, as before.
pub fn write<'a>(names: &CrateNames<'_>, alloc: Alloc<'a>) -> Result<Split<'a>, ExpansionRecordsError> {
    let prepared = prepare(names, alloc)?;
    let n_rec = prepared.unique.len();
    let mut stored_keys: AVec<'_, ContentId> = AVec::with_capacity_in(n_rec, alloc);
    let mut raw_records: AVec<'_, AVec<'a, u8>> = AVec::with_capacity_in(n_rec, alloc);
    let mut encoder = blaze_zstd::ZstdEncoder::try_new_in(alloc).map_err(ExpansionRecordsError::Zstd)?;
    let mut raw_buf: AVec<'_, u8> = AVec::new_in(alloc);
    let mut comp_buf: AVec<'_, u8> = AVec::new_in(alloc);
    for &(text, column) in prepared.unique.iter() {
        let mut bytes = AVec::new_in(alloc);
        let key = compress_one(text, column, &mut encoder, &mut raw_buf, &mut comp_buf, &mut bytes)?;
        stored_keys.push(key);
        raw_records.push(bytes);
    }
    let (directory, order) = prepared.finish_directory(&stored_keys, alloc)?;
    let mut records: AVec<'a, Record<'a>> = AVec::with_capacity_in(n_rec, alloc);
    for &u in order.iter() {
        if let (Some(slot), Some(&key)) = (raw_records.get_mut(u), stored_keys.get(u)) {
            let bytes = core::mem::replace(slot, AVec::new_in(alloc));
            records.push(Record { key, bytes });
        }
    }
    Ok(Split { directory, records })
}

/// The records [`fill_keys`] hashes side by side at once: blaze-blake3's batch.
const KEY_BATCH: usize = 64;

/// The longest record whose key is hashed beside others' (a BLAKE3 chunk, one SIMD lane); a
/// longer one is hashed alone either way, by [`key_of`] from its parts.
const SHORT_RECORD: usize = blaze_ids::b3::CHUNK_LEN;

/// Each call's key, the [`key_of`] its text and column. A record of at most [`SHORT_RECORD`]
/// bytes is written into one reused buffer as [`ExpansionRecord::write`] writes it, and
/// [`KEY_BATCH`] of them are hashed side by side ([`ContentId::of_each_domain`]); a longer one
/// goes through [`key_of`], streamed from its parts with no copy. Both digest the record's bytes
/// in [`RECORDS`] (`key_of`'s parts are those bytes in order, and BLAKE3 hashes a stream by its
/// bytes, not by its updates), so each key is the one `key_of` gives
/// (`batched_keys_are_key_of`).
fn fill_keys(calls: &mut [Call<'_>], alloc: Alloc<'_>) -> Result<(), ExpansionRecordsError> {
    let mut scratch: AVec<'_, u8> = AVec::new_in(alloc);
    let mut members = [0usize; KEY_BATCH];
    // Where each waiting record ends in `scratch`; the first starts at 0.
    let mut ends = [0usize; KEY_BATCH + 1];
    let mut count = 0;
    for at in 0..calls.len() {
        let (text, column) = (calls[at].text, calls[at].column);
        if RECORD_HEAD + pad4(text.len()) + column.len() > SHORT_RECORD {
            calls[at].key = key_of(text, column)?;
            continue;
        }
        ExpansionRecord::write_raw(text, column, &mut scratch)?;
        members[count] = at;
        count += 1;
        ends[count] = scratch.len();
        if count == KEY_BATCH {
            hash_keys(calls, &members[..count], &ends[..=count], &scratch);
            scratch.clear();
            count = 0;
        }
    }
    hash_keys(calls, &members[..count], &ends[..=count], &scratch);
    Ok(())
}

/// The keys of the records `scratch[ends[i]..ends[i + 1]]`, side by side, into the calls
/// `members[i]`.
fn hash_keys(calls: &mut [Call<'_>], members: &[usize], ends: &[usize], scratch: &[u8]) {
    let count = members.len();
    let mut inputs: [&[u8]; KEY_BATCH] = [&[]; KEY_BATCH];
    for (at, input) in inputs[..count].iter_mut().enumerate() {
        *input = &scratch[ends[at]..ends[at + 1]];
    }
    let mut keys = [ContentId::default(); KEY_BATCH];
    ContentId::of_each_domain(&DEDUP, &inputs[..count], &mut keys[..count])
        .expect("as many key slots as records: both are `members.len()`");
    for (&member, key) in members.iter().zip(keys) {
        calls[member].key = key;
    }
}

/// One call's row of a directory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry<'s> {
    pub call: ItemKey,
    /// The key of the record that holds its text and column.
    pub record: ContentId,
    pub text_len: u32,
    pub column_len: u32,
    /// Its origins' call part ([`split`]); empty when the text keeps no origins.
    pub call_part: &'s [u8],
}

impl<'s> Entry<'s> {
    /// The text of `record`, the record served for this row: refused when it is not the one
    /// the row names by its lengths ([`ExpansionRecordsError::NotItsRecord`]; the items record
    /// checks the text's digest at its first read).
    pub fn text<'r>(&self, record: &ExpansionRecord<'r>) -> Result<&'r str, ExpansionRecordsError> {
        self.check(record)?;
        Ok(record.text)
    }

    /// The origins of this call's text, read where they lie (the call part in the directory,
    /// the column in the record): `None` when the text keeps none.
    pub fn origins<'r>(&self, record: &ExpansionRecord<'r>) -> Result<Option<OriginsView<'r>>, ExpansionRecordsError>
    where
        's: 'r,
    {
        self.check(record)?;
        if self.call_part.is_empty() {
            return Ok(None);
        }
        OriginsView::open_parts(self.call_part, record.column).map(Some).map_err(|error| ExpansionRecordsError::Origins { call: self.call, error })
    }

    fn check(&self, record: &ExpansionRecord<'_>) -> Result<(), ExpansionRecordsError> {
        if record.text.len() != self.text_len as usize || record.column.len() != self.column_len as usize {
            return Err(ExpansionRecordsError::NotItsRecord { call: self.call, key: self.record });
        }
        Ok(())
    }
}

/// A crate's directory read in place, its head checked once against its length.
#[derive(Clone, Copy, Debug)]
pub struct Directory<'s> {
    record_keys: &'s [u8],
    rows: &'s [u8],
    blob: &'s [u8],
    records: u32,
}

impl<'s> Directory<'s> {
    /// The directory `bytes`; an empty section (a crate that made no text) has no rows.
    pub fn open(bytes: &'s [u8]) -> Result<Directory<'s>, ExpansionRecordsError> {
        if bytes.is_empty() {
            return Ok(Directory { record_keys: &[], rows: &[], blob: &[], records: 0 });
        }
        if bytes.len() < DIRECTORY_HEAD {
            return Err(ExpansionRecordsError::Short { what: "an expansions directory", len: bytes.len(), want: DIRECTORY_HEAD });
        }
        let found = u32_at(bytes, 4);
        if found != DIRECTORY_FORM {
            return Err(ExpansionRecordsError::OldForm { what: "an expansions directory", found });
        }
        let count = u32_at(bytes, 0) as usize;
        let records = u32_at(bytes, 8);
        let keys_end = usize::try_from(records)
            .ok()
            .and_then(|r| r.checked_mul(32))
            .and_then(|k| k.checked_add(DIRECTORY_HEAD))
            .ok_or(ExpansionRecordsError::Short { what: "an expansions directory", len: bytes.len(), want: usize::MAX })?;
        let want = count
            .checked_mul(DIRECTORY_ROW)
            .and_then(|rows| rows.checked_add(keys_end))
            .ok_or(ExpansionRecordsError::Short { what: "an expansions directory", len: bytes.len(), want: usize::MAX })?;
        let (Some(record_keys), Some(rows), Some(blob)) = (bytes.get(DIRECTORY_HEAD..keys_end), bytes.get(keys_end..want), bytes.get(want..)) else {
            return Err(ExpansionRecordsError::Short { what: "an expansions directory", len: bytes.len(), want });
        };
        Ok(Directory {
            record_keys,
            rows,
            blob,
            records,
        })
    }

    /// How many calls made a text.
    pub fn len(&self) -> usize {
        self.rows.len() / DIRECTORY_ROW
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// How many distinct records the calls name.
    pub fn records(&self) -> u32 {
        self.records
    }

    fn call_at(&self, i: usize) -> ItemKey {
        let mut key = [0u8; 32];
        key.copy_from_slice(&self.rows[i * DIRECTORY_ROW..i * DIRECTORY_ROW + 32]);
        ItemKey(ContentId(key))
    }

    /// Row `i`, in call-key order.
    pub fn entry_at(&self, i: usize) -> Result<Entry<'s>, ExpansionRecordsError> {
        let r = i * DIRECTORY_ROW;
        let call = self.call_at(i);
        let rec_idx = u32_at(self.rows, r + 32);
        if rec_idx >= self.records {
            return Err(ExpansionRecordsError::Range { call, start: rec_idx, len: 1 });
        }
        let rk = usize::try_from(rec_idx).unwrap_or(usize::MAX / 32) * 32;
        let Some(rec_slice) = self.record_keys.get(rk..rk + 32) else {
            return Err(ExpansionRecordsError::Range { call, start: rec_idx, len: 1 });
        };
        let mut record = [0u8; 32];
        record.copy_from_slice(rec_slice);
        let (text_len, column_len) = (u32_at(self.rows, r + 36), u32_at(self.rows, r + 40));
        let (start, len) = (u32_at(self.rows, r + 44), u32_at(self.rows, r + 48));
        let range = (start as usize).checked_add(len as usize).filter(|end| *end <= self.blob.len()).map(|end| start as usize..end);
        let call_part = range.map(|range| &self.blob[range]).ok_or(ExpansionRecordsError::Range { call, start, len })?;
        Ok(Entry { call, record: ContentId(record), text_len, column_len, call_part })
    }

    /// The row of `call`, by binary search over the sorted keys; `None` when the call made no
    /// text.
    pub fn entry(&self, call: ItemKey) -> Result<Option<Entry<'s>>, ExpansionRecordsError> {
        match self.position(call) {
            Some(at) => self.entry_at(at).map(Some),
            None => Ok(None),
        }
    }

    /// The row index of `call`, by binary search over the sorted keys: what a reader that keeps
    /// one served record per row indexes by (blaze-cli names/expansion_texts.rs); `None` when
    /// the call made no text.
    pub fn position(&self, call: ItemKey) -> Option<usize> {
        let (mut low, mut high) = (0usize, self.len());
        while low < high {
            let mid = low + (high - low) / 2;
            match self.call_at(mid).cmp(&call) {
                core::cmp::Ordering::Less => low = mid + 1,
                core::cmp::Ordering::Greater => high = mid,
                core::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::{string::String, vec::Vec};

    use blaze_sim::{Config, Tape};

    use super::*;

    /// A length near where a record stops being short, or any up to past a chunk.
    fn length(tape: &Tape, label: &'static str) -> usize {
        match tape.coin(label) {
            true => tape.index(1100, label),
            false => SHORT_RECORD - RECORD_HEAD - 8 + tape.index(16, label),
        }
    }

    /// Up to 200 drawn texts (ASCII) and columns.
    fn drawn(tape: &Tape) -> Vec<(String, Vec<u8>)> {
        let count = tape.index(if cfg!(miri) { 20 } else { 201 }, "calls");
        (0..count)
            .map(|_| {
                let mut text = std::vec![0u8; length(tape, "text")];
                tape.fill(&mut text, "text bytes");
                let text: Vec<u8> = text.iter().map(|b| b'a' + b % 26).collect();
                let mut column = std::vec![0u8; length(tape, "column") % 600];
                tape.fill(&mut column, "column bytes");
                (String::from_utf8(text).expect("ASCII"), column)
            })
            .collect()
    }

    /// Every key [`fill_keys`] sets, side by side or alone, is [`key_of`] of the call's text and
    /// column and [`record_key`] of the record [`ExpansionRecord::write`] makes of them.
    #[test]
    fn batched_keys_are_key_of() {
        blaze_sim::check(Config::cases(if cfg!(miri) { 4 } else { 128 }), |tape| {
            let alloc = blaze_own::testing::heap().alloc();
            let datas = drawn(tape);
            let mut calls: Vec<Call<'_>> = datas
                .iter()
                .enumerate()
                .map(|(at, (text, column))| {
                    let mut call = [0u8; 32];
                    call[..8].copy_from_slice(&u64::try_from(at).expect("a count fits u64").to_le_bytes());
                    Call { call: ItemKey(ContentId(call)), text, call_part: &[], column, key: ContentId::default() }
                })
                .collect();
            fill_keys(&mut calls, alloc).expect("drawn records fit a u32");
            for (at, call) in calls.iter().enumerate() {
                let alone = key_of(call.text, call.column).expect("drawn records fit a u32");
                let mut raw = AVec::new_in(alloc);
                ExpansionRecord::write_raw(call.text, call.column, &mut raw).expect("drawn records fit a u32");
                assert_eq!(call.key, alone, "call {at}: {} B of text, {} B of column", call.text.len(), call.column.len());
                assert_eq!(call.key, ContentId::of_domain(&DEDUP, &raw), "call {at}: the raw record's dedup digest");
                let mut encoded = AVec::new_in(alloc);
                ExpansionRecord::write(call.text, call.column, &mut encoded).expect("drawn records fit a u32");
                let reopened = ExpansionRecord::open_in(&encoded, blaze_own::testing::bump()).expect("encoded record round-trips");
                assert_eq!(reopened.text, call.text, "call {at}: round-trip text");
                assert_eq!(reopened.column, call.column, "call {at}: round-trip column");
            }
        });
    }

    /// Records whose bytes end a byte either side of 1, 2, 3, 4, 8 and 16 chunks (and on them),
    /// in a drawn order among short ones, so the batch, the streamed [`key_of`] (four updates
    /// over a lazily merged stack) and the one-shot [`record_key`] each meet every tree shape
    /// up to 16 chunks: all three keys equal.
    #[test]
    fn keys_across_chunk_edges_are_key_of() {
        let mut shapes: Vec<(usize, usize)> = Vec::new();
        for chunks in [1usize, 2, 3, 4, 8, 16] {
            for delta in -5i64..=5 {
                for column in [0usize, 1, 3, 70, 600] {
                    let target = usize::try_from(i64::try_from(chunks * 1024).expect("small") + delta).expect("positive");
                    let room = target - RECORD_HEAD - column;
                    // A text of a multiple of four bytes, the column taking the rest, so the
                    // record is `target` bytes exactly.
                    let text = room / 4 * 4;
                    shapes.push((text, column + room - text));
                }
            }
        }
        for short in [0usize, 5, 64, 400] {
            shapes.push((short, short % 9));
        }
        blaze_sim::check(Config::cases(if cfg!(miri) { 2 } else { 16 }), |tape| {
            let alloc = blaze_own::testing::heap().alloc();
            let mut order = shapes.clone();
            if cfg!(miri) {
                order.retain(|&(text, column)| RECORD_HEAD + text + column <= 3 * 1024);
            }
            tape.shuffle(&mut order, "order");
            let datas: Vec<(String, Vec<u8>)> = order
                .iter()
                .map(|&(text, column)| {
                    let text: String = (0..text).map(|at| char::from(b'a' + u8::try_from(at % 26).expect("small"))).collect();
                    let column: Vec<u8> = (0..column).map(|at| u8::try_from(at % 251).expect("small")).collect();
                    (text, column)
                })
                .collect();
            let mut calls: Vec<Call<'_>> = datas
                .iter()
                .enumerate()
                .map(|(at, (text, column))| {
                    let mut call = [0u8; 32];
                    call[..8].copy_from_slice(&u64::try_from(at).expect("a count fits u64").to_le_bytes());
                    Call { call: ItemKey(ContentId(call)), text, call_part: &[], column, key: ContentId::default() }
                })
                .collect();
            fill_keys(&mut calls, alloc).expect("records fit a u32");
            for call in &calls {
                let alone = key_of(call.text, call.column).expect("records fit a u32");
                let mut bytes = AVec::new_in(alloc);
                ExpansionRecord::write_raw(call.text, call.column, &mut bytes).expect("records fit a u32");
                let what = std::format!("{} B record, {} B of text, {} B of column", bytes.len(), call.text.len(), call.column.len());
                assert_eq!(call.key, alone, "{what}: batched against key_of");
                assert_eq!(alone, ContentId::of_domain(&DEDUP, &bytes), "{what}: key_of against the raw record's one-shot digest");
            }
        });
    }
}
