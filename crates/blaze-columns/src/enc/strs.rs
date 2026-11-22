//! A string column (bytes; UTF-8 is the caller's to keep): its strings end to end with an
//! integer column of where each ends ("plain"), FSST codes with the same offsets over the
//! codes ([`super::fsst`]), or a dictionary of whole strings: the distinct strings in order of
//! first use as a plain or FSST column, and each row's code as an integer column (names repeat
//! in a compiler's records; BtrBlocks cascades a dictionary's strings into FSST the same way).
//!
//! # Layout
//!
//! Each starts with its kind (1 byte) and its count `n` (`u32`):
//!
//! - plain (0): the offsets column's bytes (`u32`); the offsets (`n + 1` ends, from 0); the
//!   bytes (as many as the last offset).
//! - FSST (1): the table's bytes (`u32`), the offsets column's bytes (`u32`); the table; the
//!   offsets over the codes; the codes.
//! - dictionary (2): the strings' column's bytes (`u32`); the distinct strings as a plain or
//!   FSST column (never a dictionary: nothing recurses); the codes column.
//!
//! The offsets are an [`IntView`] chosen with a constant `get` (a sorted column is a frame
//! against a line), so a string is two loads of offsets and its bytes.
//!
//! # Choosing
//!
//! Plain and the dictionary are sized exactly; FSST is estimated from a sample (every k-th
//! string up to [`fsst::SAMPLE`] bytes): the table is built from it and the sample encoded,
//! and the column's codes taken to shrink by the same ratio. The smallest estimate wins, a
//! tie going to the earlier of [`StrScheme::ALL`]; then only the winner is encoded.

use blaze_kit::Dict;
use blaze_own::{AVec, Alloc, Vec};

use super::block::GetCost;
use super::fsst::{self, Decoder, Table};
use super::ints::{DictUse, IntPolicy, encode_ints, ints_size};
use super::{EncError, IntView, len32, patch32, put, read8, read32};

const KIND_PLAIN: u8 = 0;
const KIND_FSST: u8 = 1;
const KIND_DICT: u8 = 2;

/// How a string column is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrScheme {
    Plain,
    Fsst,
    /// A dictionary whose distinct strings are plain.
    DictPlain,
    /// A dictionary whose distinct strings are FSST codes.
    DictFsst,
}

impl StrScheme {
    /// In the order a tie goes by.
    pub const ALL: [StrScheme; 4] = [StrScheme::Plain, StrScheme::Fsst, StrScheme::DictPlain, StrScheme::DictFsst];

    pub const fn name(self) -> &'static str {
        match self {
            StrScheme::Plain => "plain",
            StrScheme::Fsst => "fsst",
            StrScheme::DictPlain => "dict+plain",
            StrScheme::DictFsst => "dict+fsst",
        }
    }
}

/// How a string column is chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrPolicy {
    /// The dearest `get` a dictionary's codes may cost.
    pub max_get: GetCost,
    /// This scheme, else the chooser's.
    pub only: Option<StrScheme>,
}

impl StrPolicy {
    pub const RECORD: StrPolicy = StrPolicy { max_get: GetCost::Log, only: None };
}

/// What the chooser wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrChoice {
    pub scheme: StrScheme,
    pub bytes: usize,
    /// Symbols of the FSST table, 0 with none.
    pub symbols: usize,
}

/// Each string's end, from a 0 before the first.
fn ends<'a>(strs: &[&[u8]], alloc: Alloc<'a>) -> Result<Vec<u64, Alloc<'a>>, EncError> {
    let mut ends = Vec::try_with_capacity_in(strs.len() + 1, alloc)?;
    let mut at = 0u64;
    ends.push(0);
    for s in strs {
        at += u64::try_from(s.len()).unwrap_or(u64::MAX);
        ends.push(at);
    }
    Ok(ends)
}

/// Bytes the plain column of `strs` takes.
fn plain_size(strs: &[&[u8]], alloc: Alloc<'_>) -> Result<usize, EncError> {
    let ends = ends(strs, alloc)?;
    Ok(9 + ints_size(&ends, IntPolicy::CONSTANT, alloc)? + strs.iter().map(|s| s.len()).sum::<usize>())
}

/// FSST's table for `strs` from its sample, and the bytes its column is estimated to take.
fn fsst_estimate(strs: &[&[u8]], alloc: Alloc<'_>) -> Result<(Table, usize), EncError> {
    let total: usize = strs.iter().map(|s| s.len()).sum();
    let stride = (total / fsst::SAMPLE).max(1);
    let mut sample: Vec<&[u8], Alloc<'_>> = Vec::new_in(alloc);
    let mut raw = 0usize;
    for s in strs.iter().step_by(stride) {
        if raw >= fsst::SAMPLE {
            break;
        }
        sample.try_reserve(1)?;
        sample.push(s);
        raw += s.len();
    }
    let table = Table::build(&sample, alloc)?;
    let mut codes = Vec::new_in(alloc);
    for s in &sample {
        table.encode(s, &mut codes)?;
    }
    let ends = ends(strs, alloc)?;
    let coded = if raw == 0 { 0 } else { total * codes.len() / raw };
    let size = 13 + table.size() + ints_size(&ends, IntPolicy::CONSTANT, alloc)? + coded;
    Ok((table, size))
}

fn write_plain(strs: &[&[u8]], alloc: Alloc<'_>, out: &mut AVec<'_, u8>) -> Result<(), EncError> {
    put(out, &[KIND_PLAIN])?;
    put(out, &len32(strs.len(), "a string column")?.to_le_bytes())?;
    let size_at = out.len();
    put(out, &[0; 4])?;
    let ends = ends(strs, alloc)?;
    let offsets = encode_ints(&ends, IntPolicy::CONSTANT, alloc, out)?;
    patch32(out, size_at, len32(offsets.bytes, "a string column's offsets")?);
    for s in strs {
        put(out, s)?;
    }
    Ok(())
}

fn write_fsst(strs: &[&[u8]], table: &Table, alloc: Alloc<'_>, out: &mut AVec<'_, u8>) -> Result<(), EncError> {
    let mut codes = Vec::new_in(alloc);
    let mut ends = Vec::try_with_capacity_in(strs.len() + 1, alloc)?;
    ends.push(0u64);
    for s in strs {
        table.encode(s, &mut codes)?;
        ends.push(u64::try_from(codes.len()).unwrap_or(u64::MAX));
    }
    put(out, &[KIND_FSST])?;
    put(out, &len32(strs.len(), "a string column")?.to_le_bytes())?;
    put(out, &len32(table.size(), "a symbol table")?.to_le_bytes())?;
    let size_at = out.len();
    put(out, &[0; 4])?;
    table.write(out)?;
    let offsets = encode_ints(&ends, IntPolicy::CONSTANT, alloc, out)?;
    patch32(out, size_at, len32(offsets.bytes, "a string column's offsets")?);
    put(out, &codes)
}

/// Appends `strs` as a column under `policy`; scratch in `alloc`.
pub fn encode_strs(
    strs: &[&[u8]],
    policy: StrPolicy,
    alloc: Alloc<'_>,
    out: &mut AVec<'_, u8>,
) -> Result<StrChoice, EncError> {
    let start = out.len();
    let codes_policy = IntPolicy { max_get: policy.max_get, dict: DictUse::Never, only: None };
    let wants_dict = !matches!(policy.only, Some(StrScheme::Plain | StrScheme::Fsst));
    let mut dict = Dict::<u32, &[u8], Alloc<'_>>::new_in(alloc);
    if wants_dict {
        for &s in strs {
            dict.try_push(s).map_err(|refused| EncError::Alloc(refused.why))?;
        }
    }
    // The tables the estimates built, kept for the write.
    let (mut all_table, mut dict_table) = (None, None);
    let scheme = match policy.only {
        Some(only) => only,
        None => {
            let mut codes = Vec::try_with_capacity_in(strs.len(), alloc)?;
            for &c in dict.codes() {
                codes.push(u64::from(c));
            }
            let codes_size = 9 + ints_size(&codes, codes_policy, alloc)?;
            let (table, fsst_size) = fsst_estimate(strs, alloc)?;
            all_table = Some(table);
            let (table, dict_fsst_size) = fsst_estimate(dict.values(), alloc)?;
            dict_table = Some(table);
            let sizes = [
                plain_size(strs, alloc)?,
                fsst_size,
                codes_size + plain_size(dict.values(), alloc)?,
                codes_size + dict_fsst_size,
            ];
            let mut best = 0;
            for k in 1..sizes.len() {
                if sizes[k] < sizes[best] {
                    best = k;
                }
            }
            StrScheme::ALL[best]
        }
    };
    let mut symbols = 0;
    match scheme {
        StrScheme::Plain => write_plain(strs, alloc, out)?,
        StrScheme::Fsst => {
            let table = match all_table {
                Some(table) => table,
                None => fsst_estimate(strs, alloc)?.0,
            };
            symbols = table.len();
            write_fsst(strs, &table, alloc, out)?;
        }
        StrScheme::DictPlain | StrScheme::DictFsst => {
            put(out, &[KIND_DICT])?;
            put(out, &len32(strs.len(), "a string column")?.to_le_bytes())?;
            let size_at = out.len();
            put(out, &[0; 4])?;
            let inner_at = out.len();
            if scheme == StrScheme::DictFsst {
                let table = match dict_table {
                    Some(table) => table,
                    None => fsst_estimate(dict.values(), alloc)?.0,
                };
                symbols = table.len();
                write_fsst(dict.values(), &table, alloc, out)?;
            } else {
                write_plain(dict.values(), alloc, out)?;
            }
            let size = len32(out.len() - inner_at, "a dictionary's strings")?;
            patch32(out, size_at, size);
            let mut codes = Vec::try_with_capacity_in(strs.len(), alloc)?;
            for &c in dict.codes() {
                codes.push(u64::from(c));
            }
            encode_ints(&codes, codes_policy, alloc, out)?;
        }
    }
    Ok(StrChoice { scheme, bytes: out.len() - start, symbols })
}

/// A plain or FSST column, checked by [`Flat::open`].
#[derive(Clone, Debug)]
pub struct Flat<'a> {
    offsets: IntView<'a>,
    /// The strings' bytes, or their codes.
    bytes: &'a [u8],
    fsst: Option<Decoder>,
}

impl<'a> Flat<'a> {
    /// The plain or FSST column at the start of `bytes`, and the bytes it takes.
    fn open(bytes: &'a [u8], at: usize) -> Result<(Flat<'a>, usize), EncError> {
        let corrupt = |what, off: usize| EncError::Corrupt { what, at: at + off };
        let kind = read8(bytes, 0, "a string column's kind")?;
        let n = blaze_int::idx(read32(bytes, 1, "a string column's count")?);
        let (fsst, offsets_size_at, offsets_at) = match kind {
            KIND_PLAIN => (None, 5, 9),
            KIND_FSST => {
                let table_size = blaze_int::idx(read32(bytes, 5, "a symbol table's size")?);
                let (decoder, size) = Decoder::open(&bytes[13.min(bytes.len())..], at + 13)?;
                if size != table_size {
                    return Err(corrupt("a symbol table of another size", 5));
                }
                (Some(decoder), 9, 13 + size)
            }
            _ => return Err(corrupt("not a plain or FSST string column", 0)),
        };
        let offsets_size = blaze_int::idx(read32(bytes, offsets_size_at, "an offsets column's size")?);
        let (offsets, size) = IntView::open_at(&bytes[offsets_at.min(bytes.len())..], at + offsets_at)?;
        if size != offsets_size || offsets.len() != n + 1 {
            return Err(corrupt("offsets of another size or count", offsets_at));
        }
        let data_at = offsets_at + size;
        let data_len = usize::try_from(offsets.get(n)).unwrap_or(usize::MAX);
        let data = bytes.get(data_at..data_at.saturating_add(data_len)).ok_or(corrupt("strings past the column's end", data_at))?;
        Ok((Flat { offsets, bytes: data, fsst }, data_at + data_len))
    }

    pub fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    /// Where string `i`'s bytes or codes are, clamped inside them.
    #[inline]
    fn range(&self, i: usize) -> (usize, usize) {
        let clamp = |v: u64| usize::try_from(v).unwrap_or(usize::MAX).min(self.bytes.len());
        let end = clamp(self.offsets.get(i + 1));
        (clamp(self.offsets.get(i)).min(end), end)
    }

    /// The buffer string `i` may need.
    pub fn need(&self, i: usize) -> usize {
        match self.fsst {
            None => 0,
            Some(_) => {
                let (s, e) = self.range(i);
                8 * (e - s)
            }
        }
    }

    #[inline]
    fn get<'b>(&'b self, i: usize, buf: &'b mut [u8]) -> Result<&'b [u8], EncError> {
        let (s, e) = self.range(i);
        match &self.fsst {
            None => Ok(&self.bytes[s..e]),
            Some(decoder) => {
                let n = decoder.decode(&self.bytes[s..e], buf, i)?;
                Ok(&buf[..n])
            }
        }
    }
}

/// A string column, checked once by [`StrView::open`]: reads through it check nothing again.
#[derive(Clone, Debug)]
pub enum StrView<'a> {
    Flat(Flat<'a>),
    Dict { strings: Flat<'a>, codes: IntView<'a> },
}

impl<'a> StrView<'a> {
    /// The column that is all of `bytes`.
    pub fn open(bytes: &'a [u8]) -> Result<StrView<'a>, EncError> {
        let (view, size) = match read8(bytes, 0, "a string column's kind")? {
            KIND_DICT => {
                let n = blaze_int::idx(read32(bytes, 1, "a string column's count")?);
                let inner_size = blaze_int::idx(read32(bytes, 5, "a dictionary's strings' size")?);
                let (strings, size) = Flat::open(&bytes[9.min(bytes.len())..], 9)?;
                if size != inner_size {
                    return Err(EncError::Corrupt { what: "a dictionary's strings of another size", at: 5 });
                }
                let (codes, codes_size) = IntView::open_at(&bytes[9 + size..], 9 + size)?;
                if codes.len() != n || (strings.len() == 0 && n > 0) {
                    return Err(EncError::Corrupt { what: "a dictionary's codes of another count", at: 1 });
                }
                (StrView::Dict { strings, codes }, 9 + size + codes_size)
            }
            _ => {
                let (flat, size) = Flat::open(bytes, 0)?;
                (StrView::Flat(flat), size)
            }
        };
        if size != bytes.len() {
            return Err(EncError::Corrupt { what: "bytes after the column", at: size });
        }
        Ok(view)
    }

    pub fn len(&self) -> usize {
        match self {
            StrView::Flat(flat) => flat.len(),
            StrView::Dict { codes, .. } => codes.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Which string of a flat column row `i` is.
    #[inline]
    fn row(&self, i: usize) -> (&Flat<'a>, usize) {
        match self {
            StrView::Flat(flat) => (flat, i),
            StrView::Dict { strings, codes } => {
                let code = usize::try_from(codes.get(i)).unwrap_or(usize::MAX).min(strings.len().saturating_sub(1));
                (strings, code)
            }
        }
    }

    /// The buffer [`StrView::get`] of row `i` may need: 0 for plain bytes, which are borrowed.
    pub fn need(&self, i: usize) -> usize {
        let (flat, row) = self.row(i);
        flat.need(row)
    }

    /// String `i` (below `len`): borrowed from the column when it is plain, else decoded into
    /// `buf` (at least [`StrView::need`] long).
    #[inline]
    pub fn get<'b>(&'b self, i: usize, buf: &'b mut [u8]) -> Result<&'b [u8], EncError> {
        let (flat, row) = self.row(i);
        flat.get(row, buf)
    }
}
