//! An integer column: its values (`u64`; a narrower column widens) in blocks of [`BLOCK`],
//! each block in the scheme the chooser picked for it, behind a directory of where each block
//! starts; or a dictionary: its distinct values in order of first use as one such column, and
//! each row's code into them as another (Abadi, Madden and Ferreira, SIGMOD 2006, for
//! compression a column store reads without decompressing; BtrBlocks, Kuschewski et al.,
//! SIGMOD 2023, for choosing per block and cascading the codes into a second scheme).
//!
//! # Layout
//!
//! A column of blocks: kind 0 (1 byte), `len` (`u32`), the directory (an [`ENTRY`] of 24
//! bytes a block: its scheme, parameters and body's start, [`block`]'s table), the bodies end
//! to end, [`PAD`] zero bytes. A
//! dictionary: kind 1, `len`, the bytes of its values column (`u32`), the values column, the
//! codes column (each a column of blocks). A dictionary's columns are never dictionaries, so
//! nothing recurses.
//!
//! # Choosing
//!
//! A block's scheme is chosen by its exact size: every scheme within the policy's `get` bound
//! sizes the block in one pass of arithmetic, no bytes written, and the smallest wins, a tie
//! going to the cheaper `get` ([`Scheme::ALL`]'s order). The dictionary is chosen the same way
//! against the column of blocks: built (hashing each value once, blaze-kit's [`Dict`]), sized,
//! kept only when strictly smaller. For blocks of 1,024 an exact plan is a handful of passes
//! over 8 KiB, cheaper than sampling would be; sampling is where a trial is dear (FSST's
//! table, in [`super::strs`]).

use blaze_kit::Dict;
use blaze_own::{AVec, Alloc, Vec};

use super::bits::{BLOCK, PAD};
use super::block::{self, ENTRY, Entry, GetCost, Plan, Scheme};
use super::plant::{self, Plant};
use super::{EncError, len32, patch32, put, read8, read32};

const KIND_BLOCKS: u8 = 0;
const KIND_DICT: u8 = 1;

/// Whether a column is written as a dictionary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DictUse {
    Never,
    /// When it is strictly smaller than the column of blocks.
    Try,
    Always,
}

/// How an integer column is chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntPolicy {
    /// The dearest `get` a block may cost.
    pub max_get: GetCost,
    pub dict: DictUse,
    /// Every block in this scheme (the cost example's rows), else the chooser's pick.
    pub only: Option<Scheme>,
}

impl IntPolicy {
    /// What a record's column takes: a `get` of at most a binary search over a block's runs.
    pub const RECORD: IntPolicy = IntPolicy { max_get: GetCost::Log, dict: DictUse::Try, only: None };
    /// One load a `get`, no dictionary: a string column's offsets.
    pub const CONSTANT: IntPolicy = IntPolicy { max_get: GetCost::Constant, dict: DictUse::Never, only: None };
}

/// What the chooser wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntChoice {
    pub dict: bool,
    /// Blocks in each scheme of [`Scheme::ALL`] (of the codes column, for a dictionary).
    pub blocks: [usize; 5],
    pub bytes: usize,
}

/// The plan of one block under `policy`.
fn choose(values: &[u64], policy: IntPolicy) -> Plan {
    if let Some(only) = policy.only {
        return Plan::of(only, values);
    }
    let n = values.len();
    let mut best = Plan::of(Scheme::For, values);
    let mut best_size = best.size(n);
    for scheme in Scheme::ALL {
        if scheme == Scheme::For || scheme.cost() > policy.max_get {
            continue;
        }
        let plan = Plan::of(scheme, values);
        let size = plan.size(n);
        if size < best_size {
            (best, best_size) = (plan, size);
        }
    }
    best
}

/// Bytes the column of blocks of `values` takes under `policy` (each plan's size holds its
/// directory entry).
fn blocks_size(values: &[u64], policy: IntPolicy) -> usize {
    let blocks: usize = values.chunks(BLOCK).map(|chunk| choose(chunk, policy).size(chunk.len())).sum();
    5 + blocks + PAD
}

/// Appends the column of blocks of `values`; the blocks in each scheme.
fn write_blocks(values: &[u64], policy: IntPolicy, out: &mut AVec<'_, u8>) -> Result<[usize; 5], EncError> {
    let len = len32(values.len(), "an integer column")?;
    let blocks = values.len().div_ceil(BLOCK);
    put(out, &[KIND_BLOCKS])?;
    put(out, &len.to_le_bytes())?;
    let dir = out.len();
    out.try_reserve(ENTRY * blocks)?;
    out.resize(dir + ENTRY * blocks, 0);
    let bodies = out.len();
    let mut counts = [0usize; 5];
    for (b, chunk) in values.chunks(BLOCK).enumerate() {
        let plan = choose(chunk, policy);
        let start = len32(out.len() - bodies, "an integer column's bytes")?;
        plan.write_body(chunk, out)?;
        plan.write_entry(start, &mut out[dir + ENTRY * b..dir + ENTRY * (b + 1)]);
        counts[Scheme::ALL.iter().position(|&s| s == plan.scheme()).unwrap_or(0)] += 1;
    }
    put(out, &[0; PAD])?;
    Ok(counts)
}

/// The dictionary of `values`: its distinct values in first-use order, and each row's code
/// widened to the `u64` a column holds.
fn dictionary<'a>(values: &[u64], alloc: Alloc<'a>) -> Result<(Vec<u64, Alloc<'a>>, Vec<u64, Alloc<'a>>), EncError> {
    let mut dict = Dict::<u32, u64, Alloc<'a>>::new_in(alloc);
    for &v in values {
        dict.try_push(v).map_err(|refused| EncError::Alloc(refused.why))?;
    }
    let mut distinct = Vec::try_with_capacity_in(dict.distinct(), alloc)?;
    distinct.extend_from_slice(dict.values());
    let mut codes = Vec::try_with_capacity_in(values.len(), alloc)?;
    for (row, &code) in dict.codes().iter().enumerate() {
        let code = u64::from(code);
        codes.push(if plant::on(Plant::DictCodePlusOne) && row > 0 { code + 1 } else { code });
    }
    Ok((distinct, codes))
}

/// Appends `values` as a column under `policy`; scratch in `alloc`. Refused with `out` as it
/// was up to its length (bytes past it may have been written and are the caller's to drop).
pub fn encode_ints(
    values: &[u64],
    policy: IntPolicy,
    alloc: Alloc<'_>,
    out: &mut AVec<'_, u8>,
) -> Result<IntChoice, EncError> {
    let start = out.len();
    let dict = match policy.dict {
        DictUse::Never => None,
        DictUse::Try | DictUse::Always => {
            let (distinct, codes) = dictionary(values, alloc)?;
            let size = 9 + blocks_size(&distinct, policy) + blocks_size(&codes, policy);
            let keep = policy.dict == DictUse::Always || size < blocks_size(values, policy);
            keep.then_some((distinct, codes))
        }
    };
    let (is_dict, blocks) = match dict {
        None => (false, write_blocks(values, policy, out)?),
        Some((distinct, codes)) => {
            put(out, &[KIND_DICT])?;
            put(out, &len32(values.len(), "an integer column")?.to_le_bytes())?;
            let size_at = out.len();
            put(out, &[0; 4])?;
            let values_at = out.len();
            write_blocks(&distinct, policy, out)?;
            let size = len32(out.len() - values_at, "a dictionary's values")?;
            patch32(out, size_at, size);
            (true, write_blocks(&codes, policy, out)?)
        }
    };
    Ok(IntChoice { dict: is_dict, blocks, bytes: out.len() - start })
}

/// Bytes [`encode_ints`] would write, planned without writing them.
pub fn ints_size(values: &[u64], policy: IntPolicy, alloc: Alloc<'_>) -> Result<usize, EncError> {
    let plain = blocks_size(values, policy);
    match policy.dict {
        DictUse::Never => Ok(plain),
        DictUse::Try | DictUse::Always => {
            let (distinct, codes) = dictionary(values, alloc)?;
            let dict = 9 + blocks_size(&distinct, policy) + blocks_size(&codes, policy);
            Ok(if policy.dict == DictUse::Always || dict < plain { dict } else { plain })
        }
    }
}

/// A column of blocks, checked by [`Blocks::open`].
#[derive(Clone, Copy, Debug)]
pub struct Blocks<'a> {
    /// The directory: an [`ENTRY`] a block.
    dir: &'a [u8],
    /// The blocks' bodies end to end, then the padding.
    bodies: &'a [u8],
    len: usize,
}

impl<'a> Blocks<'a> {
    /// The column of blocks at the start of `bytes`, every block's entry checked; the bytes
    /// it takes. `at` is its place, for errors.
    fn open(bytes: &'a [u8], at: usize) -> Result<(Blocks<'a>, usize), EncError> {
        let corrupt = |what, off: usize| EncError::Corrupt { what, at: at + off };
        if read8(bytes, 0, "a column's kind")? != KIND_BLOCKS {
            return Err(corrupt("not a column of blocks", 0));
        }
        let len = blaze_int::idx(read32(bytes, 1, "a column's length")?);
        let blocks = len.div_ceil(BLOCK);
        let data = 5 + ENTRY * blocks;
        let (Some(dir), Some(rest)) = (bytes.get(5..data), bytes.get(data..)) else {
            return Err(corrupt("a directory past the column's end", 5));
        };
        let mut end = 0usize;
        for (b, raw) in dir.chunks_exact(ENTRY).enumerate() {
            let place = 5 + ENTRY * b;
            let e = match raw.first_chunk::<ENTRY>() {
                Some(raw) if raw[ENTRY - 1] == 0 => Entry::read(raw),
                _ => return Err(corrupt("an entry whose last byte is not zero", place)),
            };
            if e.at != end {
                return Err(corrupt("a block that does not start where the one before ends", place));
            }
            let n = (len - b * BLOCK).min(BLOCK);
            end = e.at + block::check(&e, rest, n, at + place)?;
        }
        let size = data + end + PAD;
        match bytes.get(data + end..size) {
            Some(pad) if pad.iter().all(|&byte| byte == 0) => Ok((Blocks { dir, bodies: &bytes[data..size], len }, size)),
            _ => Err(corrupt("a column without its padding", data + end)),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Block `b`'s entry, `b` below the blocks.
    #[inline(always)]
    fn entry(&self, b: usize) -> Entry {
        match self.dir.get(b * ENTRY..).and_then(<[u8]>::first_chunk::<ENTRY>) {
            Some(raw) => Entry::read(raw),
            None => out_of_range(b * BLOCK, self.len),
        }
    }

    /// Value `i`, below `len`: block `i / BLOCK`'s entry, then, for `For` and `Linear`, one
    /// load of the packed value.
    #[inline]
    pub fn get(&self, i: usize) -> u64 {
        if i >= self.len {
            out_of_range(i, self.len);
        }
        block::get(&self.entry(i / BLOCK), self.bodies, i % BLOCK)
    }

    /// The number of values in block `b`.
    pub fn block_len(&self, b: usize) -> usize {
        self.len.saturating_sub(b * BLOCK).min(BLOCK)
    }

    /// Fills `out` with block `b`'s values; how many. `out` holds at least [`BLOCK`].
    pub fn decode_block(&self, b: usize, out: &mut [u64], vector: bool) -> usize {
        let n = self.block_len(b);
        if n > 0 {
            block::decode(&self.entry(b), self.bodies, &mut out[..n], vector);
        }
        n
    }
}

#[cold]
#[inline(never)]
#[track_caller]
fn out_of_range(i: usize, len: usize) -> ! {
    panic!("blaze_columns::enc: value {i} of a column of {len}")
}

/// An integer column, checked once by [`IntView::open`]: reads through it check nothing again.
#[derive(Clone, Copy, Debug)]
pub enum IntView<'a> {
    Blocks(Blocks<'a>),
    Dict { values: Blocks<'a>, codes: Blocks<'a> },
}

impl<'a> IntView<'a> {
    /// The column at the start of `bytes`, and the bytes it takes; `at` is its place in a
    /// larger column, for errors.
    pub fn open_at(bytes: &'a [u8], at: usize) -> Result<(IntView<'a>, usize), EncError> {
        match read8(bytes, 0, "a column's kind")? {
            KIND_BLOCKS => {
                let (blocks, size) = Blocks::open(bytes, at)?;
                Ok((IntView::Blocks(blocks), size))
            }
            KIND_DICT => {
                let len = blaze_int::idx(read32(bytes, 1, "a column's length")?);
                let values_size = blaze_int::idx(read32(bytes, 5, "a dictionary's values size")?);
                let (values, size) = Blocks::open(&bytes[9.min(bytes.len())..], at + 9)?;
                if size != values_size {
                    return Err(EncError::Corrupt { what: "a dictionary's values of another size", at: at + 5 });
                }
                let (codes, codes_size) = Blocks::open(&bytes[9 + size..], at + 9 + size)?;
                if codes.len != len || (values.len == 0 && len > 0) {
                    return Err(EncError::Corrupt { what: "a dictionary's codes of another length", at: at + 1 });
                }
                Ok((IntView::Dict { values, codes }, 9 + size + codes_size))
            }
            _ => Err(EncError::Corrupt { what: "an unknown column kind", at }),
        }
    }

    /// The column that is all of `bytes`.
    pub fn open(bytes: &'a [u8]) -> Result<IntView<'a>, EncError> {
        let (view, size) = IntView::open_at(bytes, 0)?;
        if size != bytes.len() {
            return Err(EncError::Corrupt { what: "bytes after the column", at: size });
        }
        Ok(view)
    }

    pub fn len(&self) -> usize {
        match self {
            IntView::Blocks(blocks) => blocks.len,
            IntView::Dict { codes, .. } => codes.len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Value `i`, below `len`.
    #[inline]
    pub fn get(&self, i: usize) -> u64 {
        match self {
            IntView::Blocks(blocks) => blocks.get(i),
            IntView::Dict { values, codes } => values.get(code_index(codes.get(i), values.len)),
        }
    }

    pub fn blocks(&self) -> usize {
        self.len().div_ceil(BLOCK)
    }

    /// Fills `out` (at least [`BLOCK`] long) with block `b`'s values, unpacked by vector;
    /// how many.
    pub fn decode_block(&self, b: usize, out: &mut [u64]) -> usize {
        self.decode_block_as(b, out, true)
    }

    /// [`IntView::decode_block`] by the scalar unpack: its twin.
    pub fn decode_block_scalar(&self, b: usize, out: &mut [u64]) -> usize {
        self.decode_block_as(b, out, false)
    }

    fn decode_block_as(&self, b: usize, out: &mut [u64], vector: bool) -> usize {
        match self {
            IntView::Blocks(blocks) => blocks.decode_block(b, out, vector),
            IntView::Dict { values, codes } => {
                let n = codes.decode_block(b, out, vector);
                for v in &mut out[..n] {
                    *v = values.get(code_index(*v, values.len));
                }
                n
            }
        }
    }
}

/// A code as an index into `len` values, clamped: a code past them is a corrupt column the
/// record's digest refuses, never a read past the values.
#[inline(always)]
fn code_index(code: u64, len: usize) -> usize {
    usize::try_from(code).unwrap_or(usize::MAX).min(len.saturating_sub(1))
}
