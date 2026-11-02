//! Compression by tier (docs/v2/plan/design/data.md, "Compression follows the columns"). A
//! column is `u32`s; the store picks its tier by how recently it was read, and every tier gives
//! back exactly the values it was given.
//!
//! - **hot** ([`HotColumn`]): plain fixed-width values at the narrowest width their largest
//!   needs (`u8`, `u16` or `u32`, recorded in the column), mapped and read in place: a load.
//! - **warm** ([`WarmColumn`], [`Packed`]): frame of reference plus bit-packing in SIMD-BP128's
//!   layout (Lemire and Boytsov, "Decoding billions of integers per second through
//!   vectorization"): each value its offset from the column's minimum in as many bits as the
//!   range needs; values in blocks of 128, value `4k + l` of a block in lane `l`, word `w` of
//!   every lane side by side, so one 128-bit load holds word `w` of all four lanes and
//!   [`WarmRef::unpack`] decodes a block with the same shift and mask in every lane (4-wide
//!   SSE2 or NEON, the width a constant per loop). A tail of fewer than 128 values is packed
//!   in order. Element `i` is still a shift and a mask, read in place. The same bits on every
//!   machine: the layout, not the instruction set, fixes them.
//! - **cold** ([`encode_cold`]): a codec per column, the smallest of plain packing, runs (for
//!   tag columns), deltas (for sorted columns: CSR offsets, sorted relocations, keys) and a
//!   dictionary (for few distinct values), each size computed exactly in one pass so only the
//!   winner is encoded, then zstd with a dictionary per kind of column ([`ColdDictionary`],
//!   digested once) on a compression context reused per thread; decoded when read in.

use std::{cell::RefCell, io::Cursor, mem::offset_of};

use blaze_ids::{ContentId, radix};
use zstd::zstd_safe::{self, CCtx, CDict, DCtx, DDict};

use crate::{FormatError, Slice, Writer, leb, record};

/// How wide a hot column's values are.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Width {
    U8,
    U16,
    U32,
}

impl Width {
    /// The narrowest width that holds every one of `values`.
    pub fn of(values: &[u32]) -> Width {
        match values.iter().fold(0, |most, &it| most | it) {
            0..=0xff => Width::U8,
            0x100..=0xffff => Width::U16,
            _ => Width::U32,
        }
    }

    pub fn bytes(self) -> usize {
        match self {
            Width::U8 => 1,
            Width::U16 => 2,
            Width::U32 => 4,
        }
    }
}

record! {
    /// A hot column in a buffer: one of its slices holds the values, at the width
    /// [`Width::of`] gave them; the others are empty.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct HotColumn: 1 {
        u8s: Slice<u8>,
        u16s: Slice<u16>,
        u32s: Slice<u32>,
    }
}

/// A hot column's values written to a buffer, to be named by the record that holds the column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlacedHot {
    width: Width,
    at: u64,
    len: usize,
}

impl Writer {
    /// Writes `values` at their narrowest width, straight into the buffer.
    pub fn push_hot(&mut self, values: &[u32]) -> PlacedHot {
        let width = Width::of(values);
        let at = self.pad(width.bytes());
        self.bytes.reserve(values.len() * width.bytes());
        match width {
            Width::U8 => self.bytes.extend(values.iter().map(|&it| it as u8)),
            Width::U16 => self.bytes.extend(values.iter().flat_map(|&it| (it as u16).to_le_bytes())),
            Width::U32 => self.bytes.extend(values.iter().flat_map(|&it| it.to_le_bytes())),
        }
        PlacedHot { width, at, len: values.len() }
    }

    /// Writes `values` bit-packed.
    pub fn push_warm(&mut self, values: &[u32]) -> PlacedWarm {
        let packed = Packed::pack(values);
        let at = self.push_slice(&packed.words);
        PlacedWarm { min: packed.min, bits: packed.bits, len: packed.len, at, words: packed.words.len() }
    }
}

impl HotColumn {
    /// The column a field at `field_at` holds, of the values `placed` wrote before it.
    pub fn new(field_at: u64, placed: PlacedHot) -> HotColumn {
        let field = |offset: usize| field_at + offset as u64;
        let mut column = HotColumn { u8s: Slice::EMPTY, u16s: Slice::EMPTY, u32s: Slice::EMPTY };
        match placed.width {
            Width::U8 => {
                column.u8s = Slice::between(field(offset_of!(HotColumn, u8s)), placed.at, placed.len)
            }
            Width::U16 => {
                column.u16s =
                    Slice::between(field(offset_of!(HotColumn, u16s)), placed.at, placed.len)
            }
            Width::U32 => {
                column.u32s =
                    Slice::between(field(offset_of!(HotColumn, u32s)), placed.at, placed.len)
            }
        }
        column
    }

    /// The column in place: three bounds checks, nothing read per value.
    pub fn follow<'a>(&'a self, buffer: &'a [u8]) -> Result<HotRef<'a>, FormatError> {
        let (u8s, u16s, u32s) =
            (self.u8s.follow(buffer)?, self.u16s.follow(buffer)?, self.u32s.follow(buffer)?);
        match (u8s.is_empty(), u16s.is_empty(), u32s.is_empty()) {
            (_, true, true) => Ok(HotRef::U8(u8s)),
            (true, false, true) => Ok(HotRef::U16(u16s)),
            (true, true, false) => Ok(HotRef::U32(u32s)),
            _ => Err(FormatError::Invalid {
                at: (self as *const HotColumn as usize).wrapping_sub(buffer.as_ptr() as usize)
                    as u64,
                what: "a hot column of more than one width",
            }),
        }
    }
}

/// A hot column read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HotRef<'a> {
    U8(&'a [u8]),
    U16(&'a [u16]),
    U32(&'a [u32]),
}

impl HotRef<'_> {
    pub fn width(&self) -> Width {
        match self {
            HotRef::U8(_) => Width::U8,
            HotRef::U16(_) => Width::U16,
            HotRef::U32(_) => Width::U32,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            HotRef::U8(it) => it.len(),
            HotRef::U16(it) => it.len(),
            HotRef::U32(it) => it.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, at: usize) -> u32 {
        match self {
            HotRef::U8(it) => it[at] as u32,
            HotRef::U16(it) => it[at] as u32,
            HotRef::U32(it) => it[at],
        }
    }

    /// Elements `start..start + out.len()` into `out`: one widening loop per width.
    pub fn unpack(&self, start: usize, out: &mut [u32]) {
        let end = start + out.len();
        match self {
            HotRef::U8(it) => out.iter_mut().zip(&it[start..end]).for_each(|(o, &v)| *o = v as u32),
            HotRef::U16(it) => out.iter_mut().zip(&it[start..end]).for_each(|(o, &v)| *o = v as u32),
            HotRef::U32(it) => out.copy_from_slice(&it[start..end]),
        }
    }

    pub fn to_vec(&self) -> Vec<u32> {
        // alloc: an owned copy, asked for by name
        let mut out = vec![0; self.len()];
        self.unpack(0, &mut out);
        out
    }
}

/// The lanes of a warm block: 4 `u32`s, one 128-bit vector.
pub const LANES: usize = 4;
/// The values of a warm block: 32 per lane.
pub const BLOCK_VALUES: usize = 32 * LANES;

/// Values as offsets from their minimum, `bits` bits each: full blocks of [`BLOCK_VALUES`] in
/// SIMD-BP128's layout (`bits * LANES` words each), then the tail in order, with one more word
/// than it fills so a read of two words never leaves it. `min` is at most `u32::MAX` less the
/// largest `bits`-bit offset (0 for 32 bits), so `min` plus any `bits`-bit offset is a `u32`. Costs `bits / 8` bytes a value plus
/// at most 8 bytes; [`WarmRef::get`] is one or two loads.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Packed {
    pub min: u32,
    pub bits: u32,
    pub len: usize,
    pub words: Vec<u32>,
}

fn tail_words(len: usize, bits: u32) -> usize {
    match len {
        0 => 0,
        _ => (len * bits as usize).div_ceil(32) + 1,
    }
}

fn words_for(len: usize, bits: u32) -> usize {
    match bits {
        0 => 0,
        _ => len / BLOCK_VALUES * bits as usize * LANES + tail_words(len % BLOCK_VALUES, bits),
    }
}

#[inline(always)]
fn mask(bits: u32) -> u32 {
    match bits {
        32.. => u32::MAX,
        _ => (1 << bits) - 1,
    }
}

/// Whether a column of `bits`-bit offsets from `min` holds only `u32`s.
fn frame_fits(min: u32, bits: u32) -> bool {
    bits <= 32 && min as u64 + mask(bits) as u64 <= u32::MAX as u64
}

/// `bits` bits from `shift` in `words[word]`, continued in `words[word + stride]`.
#[inline(always)]
fn read_bits(words: &[u32], word: usize, stride: usize, shift: u32, bits: u32) -> u32 {
    let low = words[word] >> shift;
    let high = match shift + bits > 32 {
        true => words[word + stride] << (32 - shift),
        false => 0,
    };
    (low | high) & mask(bits)
}

#[inline(always)]
fn write_bits(words: &mut [u32], word: usize, stride: usize, shift: u32, bits: u32, value: u32) {
    words[word] |= value << shift;
    if shift + bits > 32 {
        words[word + stride] |= value >> (32 - shift);
    }
}

/// Where value `at` of a column of `len` is: its word, the stride to the next of its lane, and
/// its shift.
#[inline(always)]
fn place(len: usize, bits: u32, at: usize) -> (usize, usize, u32) {
    let full = len / BLOCK_VALUES;
    let block_words = bits as usize * LANES;
    match at < full * BLOCK_VALUES {
        true => {
            let (block, j) = (at / BLOCK_VALUES, at % BLOCK_VALUES);
            let bit = j / LANES * bits as usize;
            (block * block_words + bit / 32 * LANES + j % LANES, LANES, (bit % 32) as u32)
        }
        false => {
            let bit = (at - full * BLOCK_VALUES) * bits as usize;
            (full * block_words + bit / 32, 1, (bit % 32) as u32)
        }
    }
}

impl Packed {
    pub fn pack(values: &[u32]) -> Packed {
        let (min, max) = values
            .iter()
            .fold((u32::MAX, 0), |(min, max), &it| (min.min(it), max.max(it)));
        let len = values.len();
        let (min, bits) = frame(min, max, len);
        // alloc: once a column packed
        let mut words = vec![0u32; words_for(len, bits)];
        if bits > 0 {
            let full = len / BLOCK_VALUES * BLOCK_VALUES;
            for (block, chunk) in values[..full].chunks_exact(BLOCK_VALUES).enumerate() {
                pack_block(&mut words[block * bits as usize * LANES..], bits, min, chunk);
            }
            let tail = full / BLOCK_VALUES * bits as usize * LANES;
            for (i, &value) in values[full..].iter().enumerate() {
                let bit = i * bits as usize;
                write_bits(&mut words, tail + bit / 32, 1, (bit % 32) as u32, bits, value - min);
            }
        }
        Packed { min, bits, len, words }
    }

    pub fn as_ref(&self) -> WarmRef<'_> {
        WarmRef { min: self.min, bits: self.bits, len: self.len, words: &self.words }
    }

    pub fn unpack(&self) -> Vec<u32> {
        // alloc: an owned copy, asked for by name
        self.as_ref().to_vec()
    }
}

fn pack_block(words: &mut [u32], bits: u32, min: u32, values: &[u32]) {
    for k in 0..BLOCK_VALUES / LANES {
        let bit = k * bits as usize;
        let (word, shift) = (bit / 32 * LANES, (bit % 32) as u32);
        for lane in 0..LANES {
            write_bits(words, word + lane, LANES, shift, bits, values[k * LANES + lane] - min);
        }
    }
}

/// One full block, `BITS` a constant so every shift and mask is too: the lane loop is the same
/// four operations on four adjacent words, which the compiler emits as one vector operation.
#[inline(always)]
fn unpack_block<const BITS: u32>(block: &[u32], min: u32, out: &mut [u32]) {
    let block = &block[..BITS as usize * LANES];
    let out = &mut out[..BLOCK_VALUES];
    for k in 0..BLOCK_VALUES / LANES {
        let bit = k * BITS as usize;
        let (word, shift) = (bit / 32 * LANES, (bit % 32) as u32);
        for lane in 0..LANES {
            out[k * LANES + lane] = min + read_bits(block, word + lane, LANES, shift, BITS);
        }
    }
}

macro_rules! unpack_by_width {
    ($bits:expr, $block:expr, $min:expr, $out:expr; $($n:literal)*) => {
        match $bits {
            $($n => unpack_block::<$n>($block, $min, $out),)*
            other => unreachable!("a warm column of {other} bits"),
        }
    };
}

record! {
    /// A warm column in a buffer: [`Packed`]'s fields, its words in the buffer.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct WarmColumn: 2 {
        min: u32,
        bits: u32,
        len: u32,
        words: Slice<u32>,
    }
}

/// A warm column's words written to a buffer, to be named by the record that holds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlacedWarm {
    min: u32,
    bits: u32,
    len: usize,
    at: u64,
    words: usize,
}

impl WarmColumn {
    /// The column a field at `field_at` holds, of the words `placed` wrote before it.
    pub fn new(field_at: u64, placed: PlacedWarm) -> WarmColumn {
        let len = u32::try_from(placed.len).expect("a column of 2^32 values or more");
        let words = field_at + offset_of!(WarmColumn, words) as u64;
        WarmColumn {
            min: placed.min,
            bits: placed.bits,
            len,
            words: Slice::between(words, placed.at, placed.words),
        }
    }

    /// The column in place, its frame and word count checked: then every read stays in it.
    pub fn follow<'a>(&'a self, buffer: &'a [u8]) -> Result<WarmRef<'a>, FormatError> {
        let words = self.words.follow(buffer)?;
        let at = (self as *const WarmColumn as usize).wrapping_sub(buffer.as_ptr() as usize) as u64;
        let fits = frame_fits(self.min, self.bits)
            && words.len() == words_for(self.len as usize, self.bits);
        match fits {
            true => Ok(WarmRef { min: self.min, bits: self.bits, len: self.len as usize, words }),
            false => Err(FormatError::Invalid { at, what: "a warm column whose words don't fit it" }),
        }
    }
}

/// A warm column read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WarmRef<'a> {
    pub min: u32,
    pub bits: u32,
    pub len: usize,
    pub words: &'a [u32],
}

impl WarmRef<'_> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, at: usize) -> u32 {
        assert!(at < self.len, "element {at} of a column of {}", self.len);
        if self.bits == 0 {
            return self.min;
        }
        let (word, stride, shift) = place(self.len, self.bits, at);
        self.min + read_bits(self.words, word, stride, shift, self.bits)
    }

    /// Elements `start..start + out.len()` into `out`: whole blocks decoded straight into
    /// `out`, a block cut by either end through one on the stack, the tail value by value.
    pub fn unpack(&self, start: usize, out: &mut [u32]) {
        let end = start + out.len();
        assert!(end <= self.len, "past the end of a column of {}", self.len);
        if self.bits == 0 {
            out.fill(self.min);
            return;
        }
        let blocks_end = self.len / BLOCK_VALUES * BLOCK_VALUES;
        let block_words = self.bits as usize * LANES;
        let mut scratch = [0u32; BLOCK_VALUES];
        let mut at = start;
        while at < end.min(blocks_end) {
            let block = at / BLOCK_VALUES;
            let (first, last) = (block * BLOCK_VALUES, (block + 1) * BLOCK_VALUES);
            let words = &self.words[block * block_words..];
            match at == first && last <= end {
                true => {
                    unpack_by_width!(self.bits, words, self.min, &mut out[at - start..];
                        1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32);
                    at = last;
                }
                false => {
                    unpack_by_width!(self.bits, words, self.min, &mut scratch;
                        1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32);
                    let upto = last.min(end);
                    out[at - start..upto - start].copy_from_slice(&scratch[at - first..upto - first]);
                    at = upto;
                }
            }
        }
        for at in at.max(blocks_end)..end {
            let (word, stride, shift) = place(self.len, self.bits, at);
            out[at - start] = self.min + read_bits(self.words, word, stride, shift, self.bits);
        }
    }

    pub fn to_vec(&self) -> Vec<u32> {
        // alloc: an owned copy, asked for by name
        let mut out = vec![0; self.len];
        self.unpack(0, &mut out);
        out
    }
}

/// The codec of a cold column, its first byte before zstd.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Codec {
    /// Every value packed, as warm.
    Plain = 0,
    /// Runs of equal values: their values and their lengths, each packed.
    Runs = 1,
    /// A non-decreasing column: its first value and the differences, packed.
    Deltas = 2,
    /// Its distinct values, as deltas, and each value's index among them, packed.
    Dictionary = 3,
}

impl Codec {
    const ALL: [Codec; 4] = [Codec::Plain, Codec::Runs, Codec::Deltas, Codec::Dictionary];
}

/// A zstd dictionary for one kind of column, named by its content ID, which every cold column
/// made with it records: decoding with another dictionary is refused, not decoded wrong. Its
/// compression and decompression tables are built once, here, not per column.
pub struct ColdDictionary {
    pub id: ContentId,
    pub bytes: Vec<u8>,
    compress: CDict<'static>,
    decompress: DDict<'static>,
}

impl Clone for ColdDictionary {
    fn clone(&self) -> ColdDictionary {
        // clone: this is `Clone` itself
        ColdDictionary::new(self.bytes.clone())
    }
}

impl PartialEq for ColdDictionary {
    fn eq(&self, other: &ColdDictionary) -> bool {
        self.id == other.id
    }
}

impl Eq for ColdDictionary {}

impl std::fmt::Debug for ColdDictionary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ColdDictionary").field("id", &self.id).field("len", &self.bytes.len()).finish()
    }
}

const DICTIONARY_DOMAIN: &str = "blaze v2 zstd dictionary 1";
const COLD_VERSION: u8 = 2;
const COLD_HEADER: usize = 1 + 32 + 8;
/// zstd's level for cold columns: written once in the background, read on every page-in.
const LEVEL: i32 = 9;

impl ColdDictionary {
    pub fn new(bytes: Vec<u8>) -> ColdDictionary {
        ColdDictionary {
            id: ContentId::of(DICTIONARY_DOMAIN, &bytes),
            compress: CDict::create(&bytes, LEVEL),
            decompress: DDict::create(&bytes),
            bytes,
        }
    }

    /// A dictionary of at most `max` bytes trained on `samples`, cold columns of one kind
    /// before zstd ([`raw_cold`]).
    pub fn train(samples: &[Vec<u8>], max: usize) -> Result<ColdDictionary, FormatError> {
        zstd::dict::from_samples(samples, max)
            .map(ColdDictionary::new)
            .map_err(|_| FormatError::Invalid { at: 0, what: "zstd could not train a dictionary" })
    }
}

thread_local! {
    /// One compression and one decompression context per thread: a context is hundreds of KiB
    /// at level 9, so it is made once, not per column.
    static CONTEXTS: RefCell<(CCtx<'static>, DCtx<'static>)> =
        RefCell::new((CCtx::create(), DCtx::create()));
}

/// The frame [`Packed::pack`] gives a column of `len` values from `min` to `max`.
fn frame(min: u32, max: u32, len: usize) -> (u32, u32) {
    let (min, max) = match len {
        0 => (0, 0),
        _ => (min, max),
    };
    let bits = u32::BITS - (max - min).leading_zeros();
    (min.min(u32::MAX - mask(bits)), bits)
}

/// The bytes [`put_packed`] writes for `len` values from `min` to `max`.
fn packed_len(min: u32, max: u32, len: usize) -> usize {
    let (min, bits) = frame(min, max, len);
    leb::len_u64(min as u64) + 1 + leb::len_u64(len as u64) + 4 * words_for(len, bits)
}

fn put_packed(out: &mut Vec<u8>, values: &[u32]) {
    let packed = Packed::pack(values);
    leb::write_u64(out, packed.min as u64);
    out.push(packed.bits as u8);
    leb::write_u64(out, packed.len as u64);
    out.reserve(packed.words.len() * 4);
    out.extend(packed.words.iter().flat_map(|word| word.to_le_bytes()));
}

const CORRUPT: FormatError = FormatError::Invalid { at: 0, what: "a cold column that does not decode" };

/// The most values a column holds, as a [`Slice`]'s length does: a cold column claiming more
/// is refused before anything is allocated for it. A run or a constant column takes a few bytes
/// whatever its length, so its bytes can't bound it.
const MOST_VALUES: u64 = u32::MAX as u64;

struct Bytes<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Bytes<'_> {
    fn byte(&mut self) -> Result<u8, FormatError> {
        let byte = *self.bytes.get(self.at).ok_or(CORRUPT)?;
        self.at += 1;
        Ok(byte)
    }

    fn varint(&mut self) -> Result<u64, FormatError> {
        leb::read_u64(self.bytes, &mut self.at).map_err(|_| CORRUPT)
    }

    fn u32(&mut self) -> Result<u32, FormatError> {
        u32::try_from(self.varint()?).map_err(|_| CORRUPT)
    }

    /// A packed column of at most `most` values, its words checked present before anything
    /// is allocated for them.
    fn packed(&mut self, most: usize) -> Result<Vec<u32>, FormatError> {
        let min = self.u32()?;
        let bits = self.byte()? as u32;
        let len = usize::try_from(self.varint()?).map_err(|_| CORRUPT)?;
        if len > most || !frame_fits(min, bits) {
            return Err(CORRUPT);
        }
        let count = words_for(len, bits);
        let end = count
            .checked_mul(4)
            .and_then(|need| self.at.checked_add(need))
            .filter(|&end| end <= self.bytes.len())
            .ok_or(CORRUPT)?;
        let words: Vec<u32> = self.bytes[self.at..end]
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
            // alloc: once a column decoded from its tier
            .collect();
        self.at = end;
        // alloc: as the words
        Ok(WarmRef { min, bits, len, words: &words }.to_vec())
    }
}

/// What every codec's size depends on, from one pass over a column (and one radix sort, for
/// the dictionary).
struct Shape {
    min: u32,
    max: u32,
    /// Runs, and the shortest and longest.
    runs: usize,
    shortest: u32,
    longest: u32,
    /// Non-decreasing, and the smallest and largest step, when it is.
    sorted: bool,
    least_step: u32,
    most_step: u32,
    /// The distinct values, ascending, and the smallest and largest gap between them.
    distinct: Vec<u32>,
    least_gap: u32,
    most_gap: u32,
}

fn steps(values: &[u32]) -> (u32, u32) {
    values
        .windows(2)
        .fold((u32::MAX, 0), |(least, most), pair| {
            let step = pair[1].wrapping_sub(pair[0]);
            (least.min(step), most.max(step))
        })
}

impl Shape {
    fn of(values: &[u32]) -> Shape {
        let (mut min, mut max) = (u32::MAX, 0);
        let (mut runs, mut shortest, mut longest, mut run) = (0usize, u32::MAX, 0u32, 0u32);
        let mut sorted = true;
        for (i, &value) in values.iter().enumerate() {
            (min, max) = (min.min(value), max.max(value));
            match i > 0 && values[i - 1] == value {
                true => run += 1,
                false => {
                    if run > 0 {
                        (shortest, longest) = (shortest.min(run), longest.max(run));
                    }
                    runs += 1;
                    run = 1;
                }
            }
            sorted &= i == 0 || values[i - 1] <= value;
        }
        if run > 0 {
            (shortest, longest) = (shortest.min(run), longest.max(run));
        }
        let (least_step, most_step) = match sorted {
            true => steps(values),
            false => (0, 0),
        };
        // alloc: once a column encoded
        let mut distinct = values.to_vec();
        radix::sort(&mut distinct);
        distinct.dedup();
        let (least_gap, most_gap) = steps(&distinct);
        Shape {
            min,
            max,
            runs,
            shortest,
            longest,
            sorted,
            least_step,
            most_step,
            distinct,
            least_gap,
            most_gap,
        }
    }

    /// The bytes [`encode_raw`] writes for a column of `len` in `codec`, `None` when the codec
    /// can't hold it.
    fn size(&self, len: usize, codec: Codec) -> Option<usize> {
        let head = 1 + leb::len_u64(len as u64);
        let body = match codec {
            Codec::Plain => packed_len(self.min, self.max, len),
            Codec::Runs => {
                packed_len(self.min, self.max, self.runs)
                    + packed_len(self.shortest, self.longest, self.runs)
            }
            Codec::Deltas if !self.sorted => return None,
            Codec::Deltas => {
                let first = if len == 0 { 0 } else { self.min };
                leb::len_u64(first as u64)
                    + packed_len(self.least_step, self.most_step, len.saturating_sub(1))
            }
            Codec::Dictionary => {
                let count = self.distinct.len();
                leb::len_u64(count as u64)
                    + leb::len_u64(self.distinct.first().copied().unwrap_or(0) as u64)
                    + packed_len(self.least_gap, self.most_gap, count.saturating_sub(1))
                    + packed_len(0, count.saturating_sub(1) as u32, len)
            }
        };
        Some(head + body)
    }
}

/// `values` in `codec`, before zstd; `None` when the codec can't hold them (deltas of a
/// column that decreases). `distinct` is the column's distinct values, ascending.
fn encode_with(values: &[u32], codec: Codec, distinct: &[u32]) -> Option<Vec<u8>> {
    assert!(values.len() as u64 <= MOST_VALUES, "a column of {} values", values.len());
    // alloc: once a column encoded
    let mut out = vec![codec as u8];
    leb::write_u64(&mut out, values.len() as u64);
    match codec {
        Codec::Plain => put_packed(&mut out, values),
        Codec::Runs => {
            // alloc: once a column encoded
            let (mut runs, mut lengths) = (Vec::new(), Vec::new());
            for &value in values {
                match runs.last() {
                    Some(&last) if last == value => *lengths.last_mut().unwrap() += 1,
                    _ => {
                        runs.push(value);
                        lengths.push(1u32);
                    }
                }
            }
            put_packed(&mut out, &runs);
            put_packed(&mut out, &lengths);
        }
        Codec::Deltas => {
            if values.windows(2).any(|pair| pair[1] < pair[0]) {
                return None;
            }
            // alloc: once a column encoded
            let deltas: Vec<u32> = values.windows(2).map(|pair| pair[1] - pair[0]).collect();
            leb::write_u64(&mut out, values.first().copied().unwrap_or(0) as u64);
            put_packed(&mut out, &deltas);
        }
        Codec::Dictionary => {
            // alloc: once a column encoded
            let gaps: Vec<u32> = distinct.windows(2).map(|pair| pair[1] - pair[0]).collect();
            let indices: Vec<u32> = values
                .iter()
                .map(|value| distinct.binary_search(value).expect("a value among the distinct") as u32)
                // alloc: once a column encoded
                .collect();
            leb::write_u64(&mut out, distinct.len() as u64);
            leb::write_u64(&mut out, distinct.first().copied().unwrap_or(0) as u64);
            put_packed(&mut out, &gaps);
            put_packed(&mut out, &indices);
        }
    }
    Some(out)
}

#[cfg(test)]
fn encode_raw(values: &[u32], codec: Codec) -> Option<Vec<u8>> {
    let mut distinct = values.to_vec();
    radix::sort(&mut distinct);
    distinct.dedup();
    encode_with(values, codec, &distinct)
}

fn decode_raw(raw: &[u8]) -> Result<Vec<u32>, FormatError> {
    let mut bytes = Bytes { bytes: raw, at: 0 };
    let codec = bytes.byte()?;
    let len = bytes.varint()?;
    if len > MOST_VALUES {
        return Err(CORRUPT);
    }
    let len = len as usize;
    let values = match Codec::ALL.get(codec as usize).copied().ok_or(CORRUPT)? {
        Codec::Plain => bytes.packed(len)?,
        Codec::Runs => {
            let runs = bytes.packed(len)?;
            let lengths = bytes.packed(len)?;
            if runs.len() != lengths.len()
                || lengths.iter().map(|&it| it as u64).sum::<u64>() != len as u64
            {
                return Err(CORRUPT);
            }
            let mut values = Vec::with_capacity(len);
            for (&value, &length) in runs.iter().zip(&lengths) {
                values.extend(std::iter::repeat_n(value, length as usize));
            }
            values
        }
        Codec::Deltas => {
            let first = bytes.u32()?;
            let deltas = bytes.packed(len.saturating_sub(1))?;
            let mut values = Vec::with_capacity(deltas.len() + 1);
            if len > 0 {
                values.push(first);
            }
            let mut last = first;
            for delta in deltas {
                last = last.checked_add(delta).ok_or(CORRUPT)?;
                values.push(last);
            }
            values
        }
        Codec::Dictionary => {
            let count = usize::try_from(bytes.varint()?).map_err(|_| CORRUPT)?;
            if count > len {
                return Err(CORRUPT);
            }
            let first = bytes.u32()?;
            let gaps = bytes.packed(count.saturating_sub(1))?;
            let mut distinct = Vec::with_capacity(count);
            if count > 0 {
                distinct.push(first);
            }
            let mut last = first;
            for gap in gaps {
                last = last.checked_add(gap).ok_or(CORRUPT)?;
                distinct.push(last);
            }
            if distinct.len() != count {
                return Err(CORRUPT);
            }
            let indices = bytes.packed(len)?;
            // alloc: once a column decoded from its tier, at its length
            let mut values = Vec::with_capacity(indices.len());
            for &index in &indices {
                values.push(distinct.get(index as usize).copied().ok_or(CORRUPT)?);
            }
            values
        }
    };
    match values.len() == len && bytes.at == raw.len() {
        true => Ok(values),
        false => Err(CORRUPT),
    }
}

/// `values` in the smallest codec before zstd (the first of [`Codec`]'s order among equals):
/// what a [`ColdDictionary`] is trained on. Every codec's size is computed exactly from one
/// [`Shape`]; only the smallest is encoded.
pub fn raw_cold(values: &[u32]) -> Vec<u8> {
    let shape = Shape::of(values);
    let codec = Codec::ALL
        .iter()
        .copied()
        .filter_map(|codec| shape.size(values.len(), codec).map(|size| (size, codec)))
        .min_by_key(|&(size, _)| size)
        .expect("plain packing holds any column");
    let raw = encode_with(values, codec.1, &shape.distinct).expect("a codec that holds the column");
    debug_assert_eq!(raw.len(), codec.0, "{:?}'s size", codec.1);
    raw
}

/// The codec [`encode_cold`] picks for `values`.
pub fn cold_codec(values: &[u32]) -> Codec {
    Codec::ALL[raw_cold(values)[0] as usize]
}

/// `values` as a cold column: the version, the dictionary's content ID (zeros for none), the
/// length before zstd, then zstd's frame of the smallest codec's bytes.
pub fn encode_cold(values: &[u32], dictionary: Option<&ColdDictionary>) -> Vec<u8> {
    compress(&raw_cold(values), dictionary)
}

pub fn decode_cold(bytes: &[u8], dictionary: Option<&ColdDictionary>) -> Result<Vec<u32>, FormatError> {
    decode_raw(&decompress(bytes, dictionary)?)
}

/// Bytes (text, a column of another type already laid out) as a cold column: zstd alone.
pub fn encode_cold_bytes(bytes: &[u8], dictionary: Option<&ColdDictionary>) -> Vec<u8> {
    compress(bytes, dictionary)
}

pub fn decode_cold_bytes(
    bytes: &[u8],
    dictionary: Option<&ColdDictionary>,
) -> Result<Vec<u8>, FormatError> {
    decompress(bytes, dictionary)
}

/// The header, then zstd's frame written straight after it into the one allocation.
fn compress(raw: &[u8], dictionary: Option<&ColdDictionary>) -> Vec<u8> {
    let mut out = Vec::with_capacity(COLD_HEADER + zstd_safe::compress_bound(raw.len()));
    out.push(COLD_VERSION);
    out.extend_from_slice(&dictionary.map_or([0; 32], |it| it.id.0));
    out.extend_from_slice(&(raw.len() as u64).to_le_bytes());
    let mut frame = Cursor::new(out);
    frame.set_position(COLD_HEADER as u64);
    let written = CONTEXTS.with_borrow_mut(|(compressor, _)| match dictionary {
        Some(dictionary) => compressor.compress_using_cdict(&mut frame, raw, &dictionary.compress),
        None => compressor.compress(&mut frame, raw, LEVEL),
    });
    if let Err(code) = written {
        panic!("zstd failed to compress {} bytes: {}", raw.len(), zstd_safe::get_error_name(code));
    }
    frame.into_inner()
}

/// The most a cold column may say it holds before zstd: no column is near it, and a corrupt
/// length is refused before anything that size is allocated.
const MOST_RAW: u64 = 1 << 32;

fn decompress(bytes: &[u8], dictionary: Option<&ColdDictionary>) -> Result<Vec<u8>, FormatError> {
    if bytes.len() < COLD_HEADER {
        return Err(FormatError::Header("a cold column shorter than its header"));
    }
    if bytes[0] != COLD_VERSION {
        return Err(FormatError::Header("a cold column of another version"));
    }
    let named = &bytes[1..33];
    match dictionary {
        Some(dictionary) if named != dictionary.id.0 => {
            return Err(FormatError::Header("a cold column made with another dictionary"));
        }
        None if named != [0; 32] => {
            return Err(FormatError::Header("a cold column made with a dictionary, read without"));
        }
        _ => {}
    }
    let mut len = [0; 8];
    len.copy_from_slice(&bytes[33..41]);
    let len = u64::from_le_bytes(len);
    if len > MOST_RAW {
        return Err(CORRUPT);
    }
    let frame = &bytes[COLD_HEADER..];
    let mut raw = Vec::with_capacity(len as usize);
    let read = CONTEXTS.with_borrow_mut(|(_, decompressor)| match dictionary {
        Some(dictionary) => decompressor.decompress_using_ddict(&mut raw, frame, &dictionary.decompress),
        None => decompressor.decompress(&mut raw, frame),
    });
    match read {
        Ok(read) if read as u64 == len && raw.len() as u64 == len => Ok(raw),
        _ => Err(CORRUPT),
    }
}

const _: () = assert!(size_of::<HotColumn>() == 24 && size_of::<WarmColumn>() == 20);

mod buffer;
pub use buffer::{BufferColumns, columns_of};

#[cfg(test)]
mod tests;
