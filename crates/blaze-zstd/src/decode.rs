//! RFC 8878 Zstandard frame, block, literals, and sequence decoder powered by zero-pre-zeroing
//! [`blaze_own::ByteCursor`], flat pre-resolved [`SeqTables`], and `HUF_4X2` Huffman decoding.

use core::alloc::Allocator;

use blaze_own::byte_cursor::RevBitSlack8;
use blaze_own::{ByteCursor, Int, RevBitCursor, SeqOp, Vec};

use crate::dict::MAX_DICT_BYTES;
use crate::fse::{
    FseDecTable, LL_DEFAULT_LOG, LL_DEFAULT_NORM, LL_MAX_CODE, LL_MAX_LOG, ML_DEFAULT_LOG,
    ML_DEFAULT_NORM, ML_MAX_CODE, ML_MAX_LOG, OF_DEFAULT_LOG, OF_DEFAULT_NORM, OF_MAX_CODE,
    OF_MAX_LOG, SEQ_ML_OFFSET, SEQ_OF_OFFSET, SeqKind, SeqTables,
};
use crate::huffman::HuffTable;
use crate::xxh64::xxh64;
use crate::{
    MAX_BLOCK_SIZE, SKIPPABLE_MAGIC_MAX, SKIPPABLE_MAGIC_MIN, ZSTD_MAGIC, ZstdError, ZstdLimits,
};

const MAX_PREALLOC_HINT: usize = 16 << 20;

/// Grow `lit_buf` (reused across blocks and calls) via a single `memset` into spare capacity.
#[inline]
fn grow_lit_buf<A: Allocator>(buf: &mut Vec<u8, A>, target: usize) -> Result<(), ZstdError> {
    if buf.len() >= target {
        return Ok(());
    }
    let rem = target - buf.len();
    let mut cur = buf.byte_cursor();
    cur.try_fill_bytes(0, rem)?;
    cur.finish();
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    #[inline]
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    #[inline]
    const fn is_empty(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    #[inline]
    fn rest(&self) -> &'a [u8] {
        self.bytes.get(self.pos..).unwrap_or(&[])
    }

    #[inline]
    fn read_u8(&mut self) -> Result<u8, ZstdError> {
        let Some(&b) = self.bytes.get(self.pos) else {
            return Err(ZstdError::Truncated);
        };
        self.pos += 1;
        Ok(b)
    }

    #[inline]
    fn read_slice(&mut self, len: usize) -> Result<&'a [u8], ZstdError> {
        let Some(end) = self.pos.checked_add(len) else {
            return Err(ZstdError::Truncated);
        };
        let Some(slice) = self.bytes.get(self.pos..end) else {
            return Err(ZstdError::Truncated);
        };
        self.pos = end;
        Ok(slice)
    }

    #[inline]
    fn skip(&mut self, len: usize) -> Result<(), ZstdError> {
        let Some(end) = self.pos.checked_add(len) else {
            return Err(ZstdError::Truncated);
        };
        if end > self.bytes.len() {
            return Err(ZstdError::Truncated);
        }
        self.pos = end;
        Ok(())
    }

    #[inline]
    fn read_u16_le(&mut self) -> Result<u16, ZstdError> {
        let bytes = self.read_slice(2)?;
        let Some(&arr) = bytes.first_chunk::<2>() else {
            return Err(ZstdError::Truncated);
        };
        Ok(u16::from_le_bytes(arr))
    }

    #[inline]
    fn read_u32_le(&mut self) -> Result<u32, ZstdError> {
        let bytes = self.read_slice(4)?;
        let Some(&arr) = bytes.first_chunk::<4>() else {
            return Err(ZstdError::Truncated);
        };
        Ok(u32::from_le_bytes(arr))
    }

    #[inline]
    fn read_u64_le(&mut self) -> Result<u64, ZstdError> {
        let bytes = self.read_slice(8)?;
        let Some(&arr) = bytes.first_chunk::<8>() else {
            return Err(ZstdError::Truncated);
        };
        Ok(u64::from_le_bytes(arr))
    }
}

struct FrameHeader {
    window_bytes: u64,
    checksum_flag: bool,
    content_size: Option<u64>,
}

fn parse_frame_header(cur: &mut Cursor<'_>) -> Result<FrameHeader, ZstdError> {
    let fhd = cur.read_u8()?;
    let fcs_flag = (fhd >> 6) & 0x03;
    let single_segment = (fhd & 0x20) != 0;
    let reserved_bit = (fhd & 0x08) != 0;
    let checksum_flag = (fhd & 0x04) != 0;
    let dict_id_flag = fhd & 0x03;

    if reserved_bit {
        return Err(ZstdError::ReservedBitSet);
    }

    let mut window_bytes: u64 = 0;
    if !single_segment {
        let wd = cur.read_u8()?;
        let exponent = u32::from(wd >> 3);
        let mantissa = u64::from(wd & 0x07);
        let window_base = 1u64 << (10u32 + exponent);
        window_bytes = window_base + (window_base >> 3) * mantissa;
    }

    let dict_id: u32 = match dict_id_flag {
        0 => 0,
        1 => u32::from(cur.read_u8()?),
        2 => u32::from(cur.read_u16_le()?),
        _ => cur.read_u32_le()?,
    };
    if dict_id != 0 {
        return Err(ZstdError::UnsupportedDictionary { dict_id });
    }

    let content_size = match fcs_flag {
        0 => {
            if single_segment {
                Some(u64::from(cur.read_u8()?))
            } else {
                None
            }
        }
        1 => Some(u64::from(cur.read_u16_le()?) + 256u64),
        2 => Some(u64::from(cur.read_u32_le()?)),
        _ => Some(cur.read_u64_le()?),
    };

    if single_segment {
        window_bytes = content_size.unwrap_or(0);
    }

    Ok(FrameHeader { window_bytes, checksum_flag, content_size })
}

pub(crate) fn peek_frame_content_size(input: &[u8]) -> Result<Option<u64>, ZstdError> {
    if input.is_empty() {
        return Err(ZstdError::Truncated);
    }
    let mut cur = Cursor::new(input);
    for _ in 0..ZstdLimits::USUAL.most_frames {
        let magic = cur.read_u32_le()?;
        if magic == ZSTD_MAGIC {
            let hdr = parse_frame_header(&mut cur)?;
            return Ok(hdr.content_size);
        }
        if (SKIPPABLE_MAGIC_MIN..=SKIPPABLE_MAGIC_MAX).contains(&magic) {
            let skip_u32 = cur.read_u32_le()?;
            cur.skip(skip_u32.widen_to::<usize>())?;
            if cur.is_empty() {
                return Ok(Some(0));
            }
            continue;
        }
        return Err(ZstdError::BadMagic);
    }
    Err(ZstdError::TooManyFrames { limit: ZstdLimits::USUAL.most_frames })
}

struct FrameState {
    huff: HuffTable,
    scratch_fse: FseDecTable,
    seqs: SeqTables,
    has_huff: bool,
    has_ll: bool,
    has_of: bool,
    has_ml: bool,
    ll_predef: bool,
    of_predef: bool,
    ml_predef: bool,
    rep: [usize; 3],
}

impl FrameState {
    const fn new() -> Self {
        Self {
            huff: HuffTable::new_empty(),
            scratch_fse: FseDecTable::new_empty(),
            seqs: SeqTables::new_empty(),
            has_huff: false,
            has_ll: false,
            has_of: false,
            has_ml: false,
            ll_predef: false,
            of_predef: false,
            ml_predef: false,
            rep: [1, 4, 8],
        }
    }

    fn reset_frame(&mut self) {
        self.has_huff = false;
        self.has_ll = false;
        self.has_of = false;
        self.has_ml = false;
        self.rep = [1, 4, 8];
    }
}

/// Reusable RFC 8878 Zstandard decoder context.
///
/// Reuses decoded Huffman/FSE table buffers and the literals scratch buffer across calls,
/// eliminating per-frame table/scratch allocations.
pub struct ZstdDecoder<A: Allocator + Copy> {
    state: FrameState,
    lit_buf: Vec<u8, A>,
    dict_work: Vec<u8, A>,
    alloc: A,
}

impl<A: Allocator + Copy> ZstdDecoder<A> {
    /// Create a new reusable Zstandard decoder backed by `alloc`.
    #[inline]
    pub fn new_in(alloc: A) -> Self {
        Self {
            state: FrameState::new(),
            lit_buf: Vec::new_in(alloc),
            dict_work: Vec::new_in(alloc),
            alloc,
        }
    }

    /// Decompress `input` into a freshly allocated `Vec<u8, A>` in this decoder's allocator.
    pub fn decompress(&mut self, input: &[u8], limits: ZstdLimits) -> Result<Vec<u8, A>, ZstdError> {
        let mut out = Vec::new_in(self.alloc);
        self.decompress_into_inner(input, limits, &mut out, false)?;
        Ok(out)
    }

    /// Decompress `input` using raw-content dictionary `dict` into a freshly allocated `Vec<u8, A>`.
    pub fn decompress_with_dict(
        &mut self,
        input: &[u8],
        dict: &[u8],
        limits: ZstdLimits,
    ) -> Result<Vec<u8, A>, ZstdError> {
        let mut out = Vec::new_in(self.alloc);
        self.decompress_with_dict_into(input, dict, limits, &mut out)?;
        Ok(out)
    }

    /// Decompress `input`, appending decoded bytes into `out` without pre-zeroing `out`'s spare
    /// capacity. Callers reusing `out` across frames pay zero allocations after warmup.
    pub fn decompress_into(
        &mut self,
        input: &[u8],
        limits: ZstdLimits,
        out: &mut Vec<u8, A>,
    ) -> Result<(), ZstdError> {
        self.decompress_into_inner(input, limits, out, false)
    }

    /// Decompress `input` compressed with raw-content dictionary `dict`, appending decoded bytes
    /// into `out` (never copying `dict` into `out`). Reuses `dict_work` across frames with the
    /// same dictionary without re-copying `dict`.
    pub fn decompress_with_dict_into<O: Allocator>(
        &mut self,
        input: &[u8],
        dict: &[u8],
        limits: ZstdLimits,
        out: &mut Vec<u8, O>,
    ) -> Result<(), ZstdError> {
        if dict.is_empty() {
            return decompress_into_core(&mut self.state, &mut self.lit_buf, input, limits, out, false, false);
        }
        let dict_slice = if dict.len() > MAX_DICT_BYTES {
            dict.get(dict.len() - MAX_DICT_BYTES..).unwrap_or(&[])
        } else {
            dict
        };
        let dict_len = dict_slice.len();
        if self.dict_work.as_slice().get(..dict_len) != Some(dict_slice) {
            self.dict_work.clear();
            self.dict_work.try_extend_from_slice(dict_slice)?;
        } else {
            self.dict_work.truncate(dict_len);
        }
        decompress_into_core(
            &mut self.state,
            &mut self.lit_buf,
            input,
            limits,
            &mut self.dict_work,
            false,
            true,
        )?;
        let decoded = self.dict_work.as_slice().get(dict_len..).ok_or(ZstdError::CorruptSequences)?;
        out.try_extend_from_slice(decoded)?;
        Ok(())
    }

    pub(crate) fn decompress_into_inner<O: Allocator>(
        &mut self,
        input: &[u8],
        limits: ZstdLimits,
        out: &mut Vec<u8, O>,
        skip_checksum: bool,
    ) -> Result<(), ZstdError> {
        decompress_into_core(&mut self.state, &mut self.lit_buf, input, limits, out, skip_checksum, false)
    }
}

fn decompress_into_core<A: Allocator, O: Allocator>(
    state: &mut FrameState,
    lit_buf: &mut Vec<u8, A>,
    input: &[u8],
    limits: ZstdLimits,
    out: &mut Vec<u8, O>,
    skip_checksum: bool,
    has_dict_prefix: bool,
) -> Result<(), ZstdError> {
    if input.is_empty() {
        return Err(ZstdError::Truncated);
    }

    let start_out_len = out.len();
    let mut cur = Cursor::new(input);
    let mut frames: usize = 0;

    while !cur.is_empty() {
        frames = frames
            .checked_add(1)
            .ok_or(ZstdError::TooManyFrames { limit: limits.most_frames })?;
        if frames > limits.most_frames {
            return Err(ZstdError::TooManyFrames { limit: limits.most_frames });
        }

        let magic = cur.read_u32_le()?;
        if (SKIPPABLE_MAGIC_MIN..=SKIPPABLE_MAGIC_MAX).contains(&magic) {
            let skip_u32 = cur.read_u32_le()?;
            cur.skip(skip_u32.widen_to::<usize>())?;
            continue;
        }
        if magic != ZSTD_MAGIC {
            return Err(ZstdError::BadMagic);
        }

        let hdr = parse_frame_header(&mut cur)?;
        let produced_so_far = out.len().saturating_sub(start_out_len);
        if let Some(cs) = hdr.content_size {
            let rem_cap = limits.most_output_bytes.saturating_sub(produced_so_far);
            if cs > rem_cap.widen_to::<u64>() {
                return Err(ZstdError::OutputTooLarge { limit: limits.most_output_bytes });
            }
        }
        if hdr.window_bytes > limits.most_window_bytes.widen_to::<u64>() {
            return Err(ZstdError::WindowTooLarge {
                window_bytes: hdr.window_bytes,
                limit: limits.most_window_bytes,
            });
        }

        let frame_start = out.len();
        let history_start = if has_dict_prefix && frames == 1 { 0 } else { frame_start };
        let total_output_cap = start_out_len.saturating_add(limits.most_output_bytes);
        state.reset_frame();

        {
            let mut writer = out.byte_cursor();
            // Reserve the entire frame once without zeroing (plus 64B slack for NEON wildcopy).
            if let Some(cs) = hdr.content_size {
                let safe_cap = input.len().saturating_mul(16).max(65_536).min(MAX_PREALLOC_HINT);
                let hint = cs.saturate::<usize>().min(safe_cap);
                writer.try_reserve(hint.saturating_add(64))?;
            }

            loop {
                let bh = cur.read_slice(3)?;
                let &[b0, b1, b2] = bh.first_chunk::<3>().ok_or(ZstdError::Truncated)?;
                let hdr_u32 = u32::from_le_bytes([b0, b1, b2, 0]);
                let last_block = (hdr_u32 & 1) != 0;
                let block_type = (hdr_u32 >> 1) & 0x03;
                let block_size = (hdr_u32 >> 3).widen_to::<usize>();

                match block_type {
                    0 => {
                        if block_size > MAX_BLOCK_SIZE {
                            return Err(ZstdError::BlockTooLarge { size: block_size });
                        }
                        check_output_budget(
                            writer.len(),
                            block_size,
                            total_output_cap,
                            limits.most_output_bytes,
                        )?;
                        let raw = cur.read_slice(block_size)?;
                        writer.try_write_slice(raw)?;
                    }
                    1 => {
                        if block_size > MAX_BLOCK_SIZE {
                            return Err(ZstdError::BlockTooLarge { size: block_size });
                        }
                        check_output_budget(
                            writer.len(),
                            block_size,
                            total_output_cap,
                            limits.most_output_bytes,
                        )?;
                        let val = cur.read_u8()?;
                        writer.try_fill_bytes(val, block_size)?;
                    }
                    2 => {
                        if block_size > MAX_BLOCK_SIZE {
                            return Err(ZstdError::BlockTooLarge { size: block_size });
                        }
                        let block_bytes = cur.read_slice(block_size)?;
                        decode_compressed_block(
                            block_bytes,
                            state,
                            lit_buf,
                            &mut writer,
                            history_start,
                            hdr.window_bytes,
                            total_output_cap,
                            limits.most_output_bytes,
                        )?;
                    }
                    _ => return Err(ZstdError::BadBlockType),
                }

                if last_block {
                    break;
                }
            }
            writer.finish();
        }

        if hdr.checksum_flag {
            let expected = cur.read_u32_le()?;
            let frame_slice = out.get(frame_start..).ok_or(ZstdError::CorruptSequences)?;
            let actual = xxh64(frame_slice, 0).truncate_to::<u32>();
            if !skip_checksum && actual != expected {
                return Err(ZstdError::ChecksumMismatch { expected, actual });
            }
        }

        if let Some(expected) = hdr.content_size {
            let actual = out.len().saturating_sub(frame_start);
            if actual.widen_to::<u64>() != expected {
                return Err(ZstdError::ContentSizeMismatch { expected, actual });
            }
        }
    }

    Ok(())
}

enum LiteralsRef<'a> {
    Borrowed(&'a [u8]),
    Buffered(usize),
}

fn decode_literals<'a, A: Allocator>(
    cur: &mut Cursor<'a>,
    state: &mut FrameState,
    lit_buf: &mut Vec<u8, A>,
) -> Result<LiteralsRef<'a>, ZstdError> {
    let b0 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
    let lit_type = b0 & 0x03;
    let size_format = (b0 >> 2) & 0x03;

    match lit_type {
        0 | 1 => {
            let regen_size: usize = match size_format {
                0 | 2 => usize::from(b0 >> 3),
                1 => {
                    let b1 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    (usize::from(b0) >> 4) | (usize::from(b1) << 4)
                }
                _ => {
                    let b1 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let b2 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    (usize::from(b0) >> 4) | (usize::from(b1) << 4) | (usize::from(b2) << 12)
                }
            };
            if regen_size > MAX_BLOCK_SIZE {
                return Err(ZstdError::CorruptLiterals);
            }
            if lit_type == 0 {
                let raw = cur.read_slice(regen_size).map_err(|_| ZstdError::CorruptLiterals)?;
                Ok(LiteralsRef::Borrowed(raw))
            } else {
                let val = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                grow_lit_buf(lit_buf, regen_size)?;
                if let Some(dst) = lit_buf.as_mut_slice().get_mut(..regen_size) {
                    dst.fill(val);
                }
                Ok(LiteralsRef::Buffered(regen_size))
            }
        }
        _ => {
            let (num_streams, regen_size, comp_size) = match size_format {
                0 | 1 => {
                    let b1 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let b2 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let v = u32::from_le_bytes([b0, b1, b2, 0]);
                    let regen = ((v >> 4) & 0x3FF).widen_to::<usize>();
                    let comp = ((v >> 14) & 0x3FF).widen_to::<usize>();
                    let streams: u8 = if size_format == 0 { 1 } else { 4 };
                    (streams, regen, comp)
                }
                2 => {
                    let b1 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let b2 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let b3 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let v = u32::from_le_bytes([b0, b1, b2, b3]);
                    let regen = ((v >> 4) & 0x3FFF).widen_to::<usize>();
                    let comp = ((v >> 18) & 0x3FFF).widen_to::<usize>();
                    (4u8, regen, comp)
                }
                _ => {
                    let b1 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let b2 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let b3 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let b4 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let v = u64::from_le_bytes([b0, b1, b2, b3, b4, 0, 0, 0]);
                    let regen = ((v >> 4) & 0x3_FFFF).truncate_to::<usize>();
                    let comp = ((v >> 22) & 0x3_FFFF).truncate_to::<usize>();
                    (4u8, regen, comp)
                }
            };

            if regen_size > MAX_BLOCK_SIZE {
                return Err(ZstdError::CorruptLiterals);
            }
            let comp_data = cur.read_slice(comp_size).map_err(|_| ZstdError::CorruptLiterals)?;

            let stream_data = if lit_type == 2 {
                let tree_bytes = state.huff.decode_description(comp_data)?;
                state.has_huff = true;
                comp_data.get(tree_bytes..).ok_or(ZstdError::CorruptHuffman)?
            } else {
                if !state.has_huff {
                    return Err(ZstdError::CorruptHuffman);
                }
                comp_data
            };

            grow_lit_buf(lit_buf, regen_size)?;
            let dst = lit_buf.as_mut_slice().get_mut(..regen_size).ok_or(ZstdError::CorruptHuffman)?;
            if num_streams == 1 {
                state.huff.decode_1_stream_into_slice(stream_data, dst)?;
            } else {
                state.huff.decode_4_streams_into_slice(stream_data, dst)?;
            }
            Ok(LiteralsRef::Buffered(regen_size))
        }
    }
}

fn load_seq_table(
    cur: &mut Cursor<'_>,
    mode: u8,
    scratch: &mut FseDecTable,
    tables: &mut SeqTables,
    kind: SeqKind,
    has_table: &mut bool,
    is_predef: &mut bool,
    default_norm: &[i16],
    default_log: u8,
    max_sym: usize,
    max_log: u8,
) -> Result<(), ZstdError> {
    match mode {
        0 => {
            if !*is_predef {
                scratch.build_from_norm(default_norm, default_log)?;
                tables.compile_from(scratch, kind)?;
                *is_predef = true;
            }
            *has_table = true;
            Ok(())
        }
        1 => {
            let sym = cur.read_u8().map_err(|_| ZstdError::CorruptFse)?;
            if usize::from(sym) > max_sym {
                return Err(ZstdError::CorruptFse);
            }
            scratch.set_rle(sym);
            tables.compile_from(scratch, kind)?;
            *is_predef = false;
            *has_table = true;
            Ok(())
        }
        2 => {
            let used = scratch.decode_header_and_build(cur.rest(), max_sym, max_log)?;
            cur.skip(used).map_err(|_| ZstdError::CorruptFse)?;
            tables.compile_from(scratch, kind)?;
            *is_predef = false;
            *has_table = true;
            Ok(())
        }
        _ => {
            if !*has_table {
                return Err(ZstdError::CorruptFse);
            }
            Ok(())
        }
    }
}

#[cold]
#[inline(never)]
fn resolve_offset(
    rep: &mut [usize; 3],
    offset_value: u32,
    lit_len: usize,
    available: usize,
) -> Result<usize, ZstdError> {
    let [r0, r1, r2] = *rep;
    if offset_value > 3 {
        let off = (offset_value - 3).widen_to::<usize>();
        *rep = [off, r0, r1];
        return Ok(off);
    }

    if lit_len > 0 {
        match offset_value {
            1 => Ok(r0),
            2 => {
                *rep = [r1, r0, r2];
                Ok(r1)
            }
            3 => {
                *rep = [r2, r0, r1];
                Ok(r2)
            }
            _ => Err(ZstdError::CorruptSequences),
        }
    } else {
        match offset_value {
            1 => {
                *rep = [r1, r0, r2];
                Ok(r1)
            }
            2 => {
                *rep = [r2, r0, r1];
                Ok(r2)
            }
            3 => {
                let Some(off) = r0.checked_sub(1) else {
                    return Err(ZstdError::BadOffset { offset: 0, available });
                };
                if off == 0 {
                    return Err(ZstdError::BadOffset { offset: 0, available });
                }
                *rep = [off, r0, r1];
                Ok(off)
            }
            _ => Err(ZstdError::CorruptSequences),
        }
    }
}

#[inline(always)]
fn check_output_budget(
    current_len: usize,
    add: usize,
    total_output_cap: usize,
    limit: usize,
) -> Result<(), ZstdError> {
    let Some(next) = current_len.checked_add(add) else {
        return Err(ZstdError::OutputTooLarge { limit });
    };
    if next > total_output_cap {
        return Err(ZstdError::OutputTooLarge { limit });
    }
    Ok(())
}

struct FastSeqOut<'a> {
    br: RevBitCursor<'a>,
    ll_state: usize,
    of_state: usize,
    ml_state: usize,
    rep: [usize; 3],
    lit_pos: usize,
    seq_idx: usize,
}

#[inline(always)]
fn extract_top_u64(w: u64, nbits: u64) -> u64 {
    // `63 ^ nbits == 63 - nbits` for all `nbits in 0..=63` (returns `0` when `nbits == 0`),
    // compiling to a single immediate `eor x, x, #63` + `lsr` with zero `ubfx #0, #32`.
    (w >> 1) >> (63u64 ^ nbits)
}

struct FusedState<'a> {
    br: RevBitCursor<'a>,
    ll_state: usize,
    of_state: usize,
    ml_state: usize,
    rep: [u32; 3],
    lit_pos: usize,
    seq_idx: usize,
}

enum LeafStop {
    Done,
    Bail(SeqOp),
    Wide,
}

/// Zero-spill, zero-call, single-reload fused sequence decoder + unconditional 32-byte NEON
/// wildcopy executor for the 99.9% hot path (`tables.max_seq_bits() <= 56`).
///
/// - Uses 3-word [`RevBitSlack8`] (no cached `container` live across the copy body, no aggregate
///   struct copy) and pointer-based [`blaze_own::SeqExecCtx`] specialized on `WINDOW_FULL`,
///   keeping 16 loop-carried GPRs across the loop.
/// - Handles all valid offsets (`offset >= 1`, including `1 <= offset < 8`) inline inside
///   [`blaze_own::SeqExecCtx::exec_leaf_fast`] with zero calls, keeping this function a true
///   call-free leaf with all 26 AArch64 GPRs available (instead of 10 callee-saved GPRs) and
///   zero stack spills/reloads.
/// - Computes the total sequence bit width upfront via a packed SWAR sum of the top 16 bits of
///   `e_ll`, `e_of`, and `e_ml`, updating `bf.bits_consumed` immediately and forking `bits` into
///   two independent parallel shift chains (`win_fse = bits << total_extra_bits` for FSE next
///   states first, then `bits` for extra bits, each with shift depth `<= 2`) so the loop-carried
///   FSE state updates overlap with extra-bit extraction while keeping peak live GPRs within leaf
///   registers with zero spills.
/// - Updates `[r0, r1, r2]` before execution using adjacent same-condition selects (reusing NZCV
///   flags with zero `cset`) and retiring old `r0` early so `actual_offset` co-colors directly
///   into `r0`'s GPR with zero loop-backedge `mov`.
/// - Returns `None` when `rem == 0` or `< 8` unread bitstream bytes remain, or `Some(seq)` when a
///   sequence requires out-of-line validation/execution (committing `writer`, `st.lit_pos`, and
///   `st.seq_idx` BEFORE `seq`, and `st.br`/FSE states/`st.rep` AFTER decoding `seq`).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn decode_exec_leaf_single<const WINDOW_FULL: bool, A: Allocator>(
    tables: &SeqTables,
    literals: &[u8],
    writer: &mut ByteCursor<'_, A>,
    st: &mut FusedState<'_>,
    last_idx: usize,
    frame_start: usize,
    win_usize: usize,
    block_out_limit: usize,
) -> Option<SeqOp> {
    let mut rem = last_idx.saturating_sub(st.seq_idx);
    if rem == 0 || !st.br.has_slack8() {
        return None;
    }
    let mut bf: RevBitSlack8<'_> = st.br.open_slack8();
    let mut ll_state = st.ll_state;
    let mut of_state = st.of_state;
    let mut ml_state = st.ml_state;
    let [mut r0, mut r1, mut r2] = st.rep;
    let mut ctx = writer.seq_exec_ctx::<WINDOW_FULL>(
        literals,
        st.lit_pos,
        block_out_limit,
        frame_start,
        win_usize,
    );

    let bail: Option<SeqOp> = loop {
        let Some(bits) = bf.reload_window_slack8() else {
            break None;
        };
        let e_ll = tables.entry_flat(ll_state);
        let e_of = tables.entry_flat(of_state);
        let e_ml = tables.entry_flat(ml_state);

        // Sum all 6 bit widths via packed SWAR on the top 16 bits (`extra_bits | (num_bits << 8)`):
        // low byte sums `<= 16 + 31 + 16 = 63 < 256` (no carry into high byte), high byte sums
        // `<= 9 + 8 + 9 = 26`. Here `total <= 56`, so `total_extra_bits <= 56 < 64` always.
        let b_sum = (e_ll.0 >> 48) + (e_of.0 >> 48) + (e_ml.0 >> 48);
        let total_extra_bits = b_sum & 0xFF;
        bf.advance(total_extra_bits + (b_sum >> 8));

        // Parallel 2-chain bit extraction (RFC 8878 §3.1.1.4): fork `bits` into independent FSE
        // next-state (`win_fse`) and extra-bit (`bits`) chains, each with shift depth `<= 2`.
        // Chain B runs first so the loop-carried `ll_state`/`ml_state`/`of_state` updates start
        // immediately after `bits` loads (`ll_nb + ml_nb <= 18 < 64`).
        let win_fse = bits << total_extra_bits;
        let ll_nb = e_ll.num_bits_u64();
        ll_state =
            e_ll.baseline().wrapping_add(extract_top_u64(win_fse, ll_nb).truncate_to::<usize>());
        let ml_nb = e_ml.num_bits_u64();
        let ll_ml_nb = ll_nb + ml_nb;
        let win_fse_ml = win_fse << ll_nb;
        let win_fse_of = win_fse << ll_ml_nb;
        ml_state = e_ml
            .baseline()
            .wrapping_add(extract_top_u64(win_fse_ml, ml_nb).truncate_to::<usize>());
        let of_nb = e_of.num_bits_u64();
        of_state = e_of
            .baseline()
            .wrapping_add(extract_top_u64(win_fse_of, of_nb).truncate_to::<usize>());

        // Chain A: extra bits (`of_eb`, `ml_eb`, `ll_eb`), retiring `e_of`, `e_ml`, `e_ll` as each
        // field is decoded (`of_eb + ml_eb <= total_extra_bits <= 56 < 64`).
        let of_eb = e_of.extra_bits_u64();
        let offset_value =
            e_of.base().wrapping_add(extract_top_u64(bits, of_eb).truncate_to::<u32>());
        let ml_eb = e_ml.extra_bits_u64();
        let of_ml_eb = of_eb + ml_eb;
        let win_ml = bits << of_eb;
        let win_ll = bits << of_ml_eb;
        let match_len_u32 =
            e_ml.base().wrapping_add(extract_top_u64(win_ml, ml_eb).truncate_to::<u32>());
        let ll_eb = e_ll.extra_bits_u64();
        let lit_len_u32 =
            e_ll.base().wrapping_add(extract_top_u64(win_ll, ll_eb).truncate_to::<u32>());

        // Branchless repeat-offset update with adjacent same-condition selects (reusing NZCV flags
        // with zero `cset`) and early death of old `r0` so `actual_offset` co-colors into `r0`'s
        // GPR with zero loop-backedge `mov`.
        let rep_code = offset_value.wrapping_sub(1).wrapping_add(u32::from(lit_len_u32 == 0));
        let r0_m1 = r0.wrapping_sub(1);
        let is_rep0 = rep_code == 0;
        let rep_lo = if is_rep0 { r0 } else { r1 };
        let next_r1 = if is_rep0 { r1 } else { r0 };
        let rep_hi = if rep_code == 2 { r2 } else { r0_m1 };
        let is_lt2 = rep_code < 2;
        let rep_off = if is_lt2 { rep_lo } else { rep_hi };
        let next_r2 = if is_lt2 { r2 } else { r1 };
        let new_off = offset_value.wrapping_sub(3);
        let actual_offset = if offset_value > 3 { new_off } else { rep_off };
        r2 = next_r2;
        r1 = next_r1;
        r0 = actual_offset;

        // `exec_leaf_fast` is a true zero-call leaf handling all `offset >= 1` inline.
        if !ctx.exec_leaf_fast(lit_len_u32, actual_offset, match_len_u32) {
            break Some(SeqOp {
                lit_len: lit_len_u32,
                match_len: match_len_u32,
                offset: actual_offset,
            });
        }
        rem -= 1;
        if rem == 0 {
            break None;
        }
    };

    st.lit_pos = ctx.lit_pos();
    ctx.commit();
    bf.commit_to(&mut st.br);
    st.ll_state = ll_state;
    st.of_state = of_state;
    st.ml_state = ml_state;
    st.rep = [r0, r1, r2];
    st.seq_idx = last_idx.saturating_sub(rem);
    bail
}

/// Separate zero-spill, zero-call wide-bitstream leaf decoder for blocks whose FSE tables allow
/// `> 56` bits in a single sequence (`!single_reload`, common on `zstd -3` streams with wide
/// offsets or literal lengths). Kept separate from [`decode_exec_leaf_single`] so the hot
/// single-reload loop has zero `avail >= 16` / `total > 56` branches, while using the same
/// 3-word [`RevBitSlack8`] (`reload_window_slack16`), packed-SWAR bit-width sum, parallel 2-chain
/// bit extraction, and adjacent-condition repeat-offset update with zero stack spills.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn decode_exec_leaf_wide<const WINDOW_FULL: bool, A: Allocator>(
    tables: &SeqTables,
    literals: &[u8],
    writer: &mut ByteCursor<'_, A>,
    st: &mut FusedState<'_>,
    last_idx: usize,
    frame_start: usize,
    win_usize: usize,
    block_out_limit: usize,
) -> LeafStop {
    let mut rem = last_idx.saturating_sub(st.seq_idx);
    if rem == 0 || !st.br.has_slack16() {
        return LeafStop::Done;
    }
    let mut bf: RevBitSlack8<'_> = st.br.open_slack8();
    let mut ll_state = st.ll_state;
    let mut of_state = st.of_state;
    let mut ml_state = st.ml_state;
    let [mut r0, mut r1, mut r2] = st.rep;
    let mut ctx = writer.seq_exec_ctx::<WINDOW_FULL>(
        literals,
        st.lit_pos,
        block_out_limit,
        frame_start,
        win_usize,
    );

    let stop = loop {
        let Some(bits) = bf.reload_window_slack16() else {
            break LeafStop::Done;
        };
        let e_ll = tables.entry_flat(ll_state);
        let e_of = tables.entry_flat(of_state);
        let e_ml = tables.entry_flat(ml_state);

        // Sum all 6 bit widths via packed SWAR on the top 16 bits (`extra_bits | (num_bits << 8)`):
        // low byte sums `<= 16 + 31 + 16 = 63 < 256` (no carry into high byte), high byte sums
        // `<= 9 + 8 + 9 = 26`. Guard `total <= 56` guarantees `total_extra_bits <= 56 < 64`.
        let b_sum = (e_ll.0 >> 48) + (e_of.0 >> 48) + (e_ml.0 >> 48);
        let total_extra_bits = b_sum & 0xFF;
        let total = total_extra_bits + (b_sum >> 8);
        if total > 56 {
            break LeafStop::Wide;
        }
        bf.advance(total);

        // Parallel 2-chain bit extraction (RFC 8878 §3.1.1.4): fork `bits` into independent FSE
        // next-state (`win_fse`) and extra-bit (`bits`) chains, each with shift depth `<= 2`.
        // Chain B runs first so the loop-carried `ll_state`/`ml_state`/`of_state` updates start
        // immediately after `bits` loads (`ll_nb + ml_nb <= 18 < 64`).
        let win_fse = bits << total_extra_bits;
        let ll_nb = e_ll.num_bits_u64();
        ll_state =
            e_ll.baseline().wrapping_add(extract_top_u64(win_fse, ll_nb).truncate_to::<usize>());
        let ml_nb = e_ml.num_bits_u64();
        let ll_ml_nb = ll_nb + ml_nb;
        let win_fse_ml = win_fse << ll_nb;
        let win_fse_of = win_fse << ll_ml_nb;
        ml_state = e_ml
            .baseline()
            .wrapping_add(extract_top_u64(win_fse_ml, ml_nb).truncate_to::<usize>());
        let of_nb = e_of.num_bits_u64();
        of_state = e_of
            .baseline()
            .wrapping_add(extract_top_u64(win_fse_of, of_nb).truncate_to::<usize>());

        // Chain A: extra bits (`of_eb`, `ml_eb`, `ll_eb`), retiring `e_of`, `e_ml`, `e_ll` as each
        // field is decoded (`of_eb + ml_eb <= total_extra_bits <= 56 < 64`).
        let of_eb = e_of.extra_bits_u64();
        let offset_value =
            e_of.base().wrapping_add(extract_top_u64(bits, of_eb).truncate_to::<u32>());
        let ml_eb = e_ml.extra_bits_u64();
        let of_ml_eb = of_eb + ml_eb;
        let win_ml = bits << of_eb;
        let win_ll = bits << of_ml_eb;
        let match_len_u32 =
            e_ml.base().wrapping_add(extract_top_u64(win_ml, ml_eb).truncate_to::<u32>());
        let ll_eb = e_ll.extra_bits_u64();
        let lit_len_u32 =
            e_ll.base().wrapping_add(extract_top_u64(win_ll, ll_eb).truncate_to::<u32>());

        // Branchless repeat-offset update with adjacent same-condition selects (reusing NZCV flags
        // with zero `cset`) and early death of old `r0` so `actual_offset` co-colors into `r0`'s
        // GPR with zero loop-backedge `mov`.
        let rep_code = offset_value.wrapping_sub(1).wrapping_add(u32::from(lit_len_u32 == 0));
        let r0_m1 = r0.wrapping_sub(1);
        let is_rep0 = rep_code == 0;
        let rep_lo = if is_rep0 { r0 } else { r1 };
        let next_r1 = if is_rep0 { r1 } else { r0 };
        let rep_hi = if rep_code == 2 { r2 } else { r0_m1 };
        let is_lt2 = rep_code < 2;
        let rep_off = if is_lt2 { rep_lo } else { rep_hi };
        let next_r2 = if is_lt2 { r2 } else { r1 };
        let new_off = offset_value.wrapping_sub(3);
        let actual_offset = if offset_value > 3 { new_off } else { rep_off };
        r2 = next_r2;
        r1 = next_r1;
        r0 = actual_offset;

        if !ctx.exec_leaf_fast(lit_len_u32, actual_offset, match_len_u32) {
            break LeafStop::Bail(SeqOp {
                lit_len: lit_len_u32,
                match_len: match_len_u32,
                offset: actual_offset,
            });
        }
        rem -= 1;
        if rem == 0 {
            break LeafStop::Done;
        }
    };

    st.lit_pos = ctx.lit_pos();
    ctx.commit();
    bf.commit_to(&mut st.br);
    st.ll_state = ll_state;
    st.of_state = of_state;
    st.ml_state = ml_state;
    st.rep = [r0, r1, r2];
    st.seq_idx = last_idx.saturating_sub(rem);
    stop
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn exec_one_bail_seq<A: Allocator>(
    literals: &[u8],
    writer: &mut ByteCursor<'_, A>,
    st: &mut FusedState<'_>,
    seq: SeqOp,
    frame_start: usize,
    win_usize: usize,
    block_start_out: usize,
    block_out_limit: usize,
    limit: usize,
) -> Result<(), ZstdError> {
    let lit_len = seq.lit_len.widen_to::<usize>();
    let match_len = seq.match_len.widen_to::<usize>();
    let offset = seq.offset.widen_to::<usize>();

    let Some(lit_end) = st.lit_pos.checked_add(lit_len) else {
        return Err(ZstdError::CorruptSequences);
    };
    if lit_end > literals.len() {
        return Err(ZstdError::CorruptSequences);
    }
    let Some(after_lit) = writer.len().checked_add(lit_len) else {
        return Err(ZstdError::OutputTooLarge { limit });
    };
    let Some(next_out) = after_lit.checked_add(match_len) else {
        return Err(ZstdError::OutputTooLarge { limit });
    };
    if next_out > block_out_limit {
        let regen = next_out.saturating_sub(block_start_out);
        if regen > MAX_BLOCK_SIZE {
            return Err(ZstdError::BlockTooLarge { size: regen });
        }
        return Err(ZstdError::OutputTooLarge { limit });
    }
    let available = after_lit.saturating_sub(frame_start);
    let max_off = available.min(win_usize);
    if offset == 0 || offset > max_off {
        return Err(ZstdError::BadOffset { offset, available });
    }
    if !writer.exec_seq_within_cap(literals, st.lit_pos, lit_len, offset, match_len) {
        return Err(ZstdError::OutputTooLarge { limit });
    }
    st.lit_pos = lit_end;
    st.seq_idx += 1;
    Ok(())
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn decode_one_wide_seq<A: Allocator>(
    tables: &SeqTables,
    literals: &[u8],
    writer: &mut ByteCursor<'_, A>,
    st: &mut FusedState<'_>,
    frame_start: usize,
    win_usize: usize,
    block_start_out: usize,
    block_out_limit: usize,
    limit: usize,
) -> Result<(), ZstdError> {
    let (ll_base, ll_eb, ll_nb, ll_bl) = tables.entry_flat(st.ll_state).unpack();
    let (of_base, of_eb, of_nb, of_bl) = tables.entry_flat(st.of_state).unpack();
    let (ml_base, ml_eb, ml_nb, ml_bl) = tables.entry_flat(st.ml_state).unpack();

    let of_extra = st.br.read_bits_branchless(of_eb);
    let ml_extra = st.br.read_bits_branchless(ml_eb);
    let _ = st.br.reload_if_slack8();
    let ll_extra = st.br.read_bits_branchless(ll_eb);
    let ll_add = st.br.read_bits_branchless(ll_nb).widen_to::<usize>();
    let ml_add = st.br.read_bits_branchless(ml_nb).widen_to::<usize>();
    let of_add = st.br.read_bits_branchless(of_nb).widen_to::<usize>();

    st.ll_state = ll_bl + ll_add;
    st.ml_state = ml_bl + ml_add;
    st.of_state = of_bl + of_add;

    let offset_value = of_base.wrapping_add(of_extra);
    let match_len = ml_base.wrapping_add(ml_extra).widen_to::<usize>();
    let lit_len = ll_base.wrapping_add(ll_extra).widen_to::<usize>();

    let [r0, r1, r2] = st.rep;
    let mut rep_usize = [r0.widen_to::<usize>(), r1.widen_to::<usize>(), r2.widen_to::<usize>()];
    let Some(lit_end) = st.lit_pos.checked_add(lit_len) else {
        return Err(ZstdError::CorruptSequences);
    };
    if lit_end > literals.len() {
        return Err(ZstdError::CorruptSequences);
    }
    let Some(after_lit) = writer.len().checked_add(lit_len) else {
        return Err(ZstdError::OutputTooLarge { limit });
    };
    let Some(next_out) = after_lit.checked_add(match_len) else {
        return Err(ZstdError::OutputTooLarge { limit });
    };
    if next_out > block_out_limit {
        let regen = next_out.saturating_sub(block_start_out);
        if regen > MAX_BLOCK_SIZE {
            return Err(ZstdError::BlockTooLarge { size: regen });
        }
        return Err(ZstdError::OutputTooLarge { limit });
    }
    let available = after_lit.saturating_sub(frame_start);
    let actual_offset = resolve_offset(&mut rep_usize, offset_value, lit_len, available)?;
    let max_off = available.min(win_usize);
    if actual_offset == 0 || actual_offset > max_off {
        return Err(ZstdError::BadOffset { offset: actual_offset, available });
    }
    let [nr0, nr1, nr2] = rep_usize;
    st.rep = [nr0.saturate::<u32>(), nr1.saturate::<u32>(), nr2.saturate::<u32>()];
    if !writer.exec_seq_within_cap(literals, st.lit_pos, lit_len, actual_offset, match_len) {
        return Err(ZstdError::OutputTooLarge { limit });
    }
    st.lit_pos = lit_end;
    st.seq_idx += 1;
    Ok(())
}

/// Fused zero-call-leaf sequence decoder driver for the hot 99.9% of Zstandard sequences.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn decode_sequences_fast<'a, A: Allocator>(
    tables: &SeqTables,
    literals: &[u8],
    writer: &mut ByteCursor<'_, A>,
    br_in: &RevBitCursor<'a>,
    ll_state: usize,
    of_state: usize,
    ml_state: usize,
    rep_in: [usize; 3],
    last_idx: usize,
    frame_start: usize,
    win_usize: usize,
    block_start_out: usize,
    block_out_limit: usize,
    limit: usize,
) -> Result<FastSeqOut<'a>, ZstdError> {
    let [r0_in, r1_in, r2_in] = rep_in;
    let mut st = FusedState {
        br: *br_in,
        ll_state,
        of_state,
        ml_state,
        rep: [r0_in.saturate::<u32>(), r1_in.saturate::<u32>(), r2_in.saturate::<u32>()],
        lit_pos: 0,
        seq_idx: 0,
    };
    let single_reload = tables.max_seq_bits() <= 56;
    let win_full_at = frame_start.saturating_add(win_usize);

    loop {
        if st.seq_idx >= last_idx
            || literals.len().saturating_sub(st.lit_pos) < 32
            || writer.spare() < 48
        {
            break;
        }
        let window_full = writer.len().saturating_sub(frame_start) >= win_usize;
        let leaf_out_limit = if window_full {
            block_out_limit
        } else {
            block_out_limit.min(win_full_at)
        };

        let stop = if single_reload {
            let bail = if window_full {
                decode_exec_leaf_single::<true, A>(
                    tables,
                    literals,
                    writer,
                    &mut st,
                    last_idx,
                    frame_start,
                    win_usize,
                    leaf_out_limit,
                )
            } else {
                decode_exec_leaf_single::<false, A>(
                    tables,
                    literals,
                    writer,
                    &mut st,
                    last_idx,
                    frame_start,
                    win_usize,
                    leaf_out_limit,
                )
            };
            match bail {
                None => LeafStop::Done,
                Some(seq) => LeafStop::Bail(seq),
            }
        } else if window_full {
            decode_exec_leaf_wide::<true, A>(
                tables,
                literals,
                writer,
                &mut st,
                last_idx,
                frame_start,
                win_usize,
                leaf_out_limit,
            )
        } else {
            decode_exec_leaf_wide::<false, A>(
                tables,
                literals,
                writer,
                &mut st,
                last_idx,
                frame_start,
                win_usize,
                leaf_out_limit,
            )
        };

        match stop {
            LeafStop::Done => break,
            LeafStop::Bail(seq) => {
                exec_one_bail_seq(
                    literals,
                    writer,
                    &mut st,
                    seq,
                    frame_start,
                    win_usize,
                    block_start_out,
                    block_out_limit,
                    limit,
                )?;
            }
            LeafStop::Wide => {
                decode_one_wide_seq(
                    tables,
                    literals,
                    writer,
                    &mut st,
                    frame_start,
                    win_usize,
                    block_start_out,
                    block_out_limit,
                    limit,
                )?;
            }
        }
    }

    let [r0_out, r1_out, r2_out] = st.rep;
    Ok(FastSeqOut {
        br: st.br,
        ll_state: st.ll_state,
        of_state: st.of_state,
        ml_state: st.ml_state,
        rep: [r0_out.widen_to::<usize>(), r1_out.widen_to::<usize>(), r2_out.widen_to::<usize>()],
        lit_pos: st.lit_pos,
        seq_idx: st.seq_idx,
    })
}

#[inline(never)]
fn decode_compressed_block<A: Allocator, O: Allocator>(
    block_bytes: &[u8],
    state: &mut FrameState,
    lit_buf: &mut Vec<u8, A>,
    writer: &mut ByteCursor<'_, O>,
    frame_start: usize,
    window_bytes: u64,
    total_output_cap: usize,
    limit: usize,
) -> Result<(), ZstdError> {
    let mut cur = Cursor::new(block_bytes);
    let lit_ref = decode_literals(&mut cur, state, lit_buf)?;
    let literals: &[u8] = match lit_ref {
        LiteralsRef::Borrowed(slice) => slice,
        LiteralsRef::Buffered(n) => {
            let Some(slice) = lit_buf.as_slice().get(..n) else {
                return Err(ZstdError::CorruptLiterals);
            };
            slice
        }
    };

    // Sequences section header (RFC 8878 §3.1.1.3.2)
    let b0 = cur.read_u8().map_err(|_| ZstdError::CorruptSequences)?;
    let num_seq: usize = if b0 == 0 {
        0
    } else if b0 < 128 {
        usize::from(b0)
    } else if b0 < 255 {
        let b1 = cur.read_u8().map_err(|_| ZstdError::CorruptSequences)?;
        ((usize::from(b0) - 128) << 8) + usize::from(b1)
    } else {
        let lo = cur.read_u8().map_err(|_| ZstdError::CorruptSequences)?;
        let hi = cur.read_u8().map_err(|_| ZstdError::CorruptSequences)?;
        usize::from(u16::from_le_bytes([lo, hi])) + 0x7F00
    };

    if num_seq == 0 {
        if !cur.is_empty() {
            return Err(ZstdError::CorruptSequences);
        }
        check_output_budget(writer.len(), literals.len(), total_output_cap, limit)?;
        writer.try_write_slice(literals)?;
        return Ok(());
    }

    let modes = cur.read_u8().map_err(|_| ZstdError::CorruptSequences)?;
    if (modes & 0x03) != 0 {
        return Err(ZstdError::ReservedBitSet);
    }
    let ll_mode = (modes >> 6) & 0x03;
    let of_mode = (modes >> 4) & 0x03;
    let ml_mode = (modes >> 2) & 0x03;

    load_seq_table(
        &mut cur,
        ll_mode,
        &mut state.scratch_fse,
        &mut state.seqs,
        SeqKind::Ll,
        &mut state.has_ll,
        &mut state.ll_predef,
        &LL_DEFAULT_NORM,
        LL_DEFAULT_LOG,
        LL_MAX_CODE,
        LL_MAX_LOG,
    )?;
    load_seq_table(
        &mut cur,
        of_mode,
        &mut state.scratch_fse,
        &mut state.seqs,
        SeqKind::Of,
        &mut state.has_of,
        &mut state.of_predef,
        &OF_DEFAULT_NORM,
        OF_DEFAULT_LOG,
        OF_MAX_CODE,
        OF_MAX_LOG,
    )?;
    load_seq_table(
        &mut cur,
        ml_mode,
        &mut state.scratch_fse,
        &mut state.seqs,
        SeqKind::Ml,
        &mut state.has_ml,
        &mut state.ml_predef,
        &ML_DEFAULT_NORM,
        ML_DEFAULT_LOG,
        ML_MAX_CODE,
        ML_MAX_LOG,
    )?;

    let Some(mut br) = RevBitCursor::new(cur.rest()) else {
        return Err(ZstdError::CorruptSequences);
    };
    let ll_log = u32::from(state.seqs.ll_log());
    let of_log = u32::from(state.seqs.of_log());
    let ml_log = u32::from(state.seqs.ml_log());

    let Some(ll_bits) = br.read_bits(ll_log) else {
        return Err(ZstdError::CorruptSequences);
    };
    let Some(of_bits) = br.read_bits(of_log) else {
        return Err(ZstdError::CorruptSequences);
    };
    let Some(ml_bits) = br.read_bits(ml_log) else {
        return Err(ZstdError::CorruptSequences);
    };
    let ll_raw = ll_bits.widen_to::<usize>();
    let of_raw = of_bits.widen_to::<usize>();
    let ml_raw = ml_bits.widen_to::<usize>();

    if ll_raw >= (1usize << ll_log)
        || of_raw >= (1usize << of_log)
        || ml_raw >= (1usize << ml_log)
    {
        return Err(ZstdError::CorruptSequences);
    }
    let ll_state = ll_raw;
    let of_state = SEQ_OF_OFFSET + of_raw;
    let ml_state = SEQ_ML_OFFSET + ml_raw;

    // Reserve room for up to one full block (+ 64B wildcopy slack) once, with ZERO zeroing.
    let block_start_out = writer.len();
    let block_out_limit = block_start_out.saturating_add(MAX_BLOCK_SIZE).min(total_output_cap);
    let block_reserve = block_out_limit.saturating_sub(block_start_out).saturating_add(64);
    writer.try_reserve(block_reserve)?;

    let win_usize = window_bytes.saturate::<usize>();
    let lit_total = literals.len();
    let last_idx = num_seq - 1;

    let FastSeqOut {
        mut br,
        mut ll_state,
        mut of_state,
        mut ml_state,
        rep,
        mut lit_pos,
        mut seq_idx,
    } = decode_sequences_fast(
        &state.seqs,
        literals,
        writer,
        &br,
        ll_state,
        of_state,
        ml_state,
        state.rep,
        last_idx,
        frame_start,
        win_usize,
        block_start_out,
        block_out_limit,
        limit,
    )?;

    // Exact tail loop for the final sequences (`br.avail_bytes() < 8`/`< 16` or `seq_idx == last_idx`).
    let mut block_regen = writer.len().saturating_sub(block_start_out);
    state.rep = rep;
    while seq_idx < num_seq {
        let (ll_base, ll_eb, ll_nb, ll_bl) = state.seqs.entry_flat(ll_state).unpack();
        let (of_base, of_eb, of_nb, of_bl) = state.seqs.entry_flat(of_state).unpack();
        let (ml_base, ml_eb, ml_nb, ml_bl) = state.seqs.entry_flat(ml_state).unpack();

        let Some(of_extra) = br.read_bits(of_eb) else {
            return Err(ZstdError::CorruptSequences);
        };
        let Some(ml_extra) = br.read_bits(ml_eb) else {
            return Err(ZstdError::CorruptSequences);
        };
        let Some(ll_extra) = br.read_bits(ll_eb) else {
            return Err(ZstdError::CorruptSequences);
        };
        if seq_idx < last_idx {
            let Some(ll_add) = br.read_bits(ll_nb) else {
                return Err(ZstdError::CorruptSequences);
            };
            let Some(ml_add) = br.read_bits(ml_nb) else {
                return Err(ZstdError::CorruptSequences);
            };
            let Some(of_add) = br.read_bits(of_nb) else {
                return Err(ZstdError::CorruptSequences);
            };
            ll_state = ll_bl + ll_add.widen_to::<usize>();
            ml_state = ml_bl + ml_add.widen_to::<usize>();
            of_state = of_bl + of_add.widen_to::<usize>();
        }

        let offset_value = of_base.wrapping_add(of_extra);
        let match_len = ml_base.wrapping_add(ml_extra).widen_to::<usize>();
        let lit_len = ll_base.wrapping_add(ll_extra).widen_to::<usize>();

        let Some(lit_end) = lit_pos.checked_add(lit_len) else {
            return Err(ZstdError::CorruptSequences);
        };
        if lit_end > lit_total {
            return Err(ZstdError::CorruptSequences);
        }
        let Some(seq_out) = lit_len.checked_add(match_len) else {
            return Err(ZstdError::CorruptSequences);
        };
        let Some(next_regen) = block_regen.checked_add(seq_out) else {
            return Err(ZstdError::CorruptSequences);
        };
        block_regen = next_regen;
        if block_regen > MAX_BLOCK_SIZE {
            return Err(ZstdError::BlockTooLarge { size: block_regen });
        }
        let Some(next_out) = writer.len().checked_add(seq_out) else {
            return Err(ZstdError::OutputTooLarge { limit });
        };
        if next_out > total_output_cap {
            return Err(ZstdError::OutputTooLarge { limit });
        }

        let available = (writer.len() + lit_len).saturating_sub(frame_start);
        let actual_offset = resolve_offset(&mut state.rep, offset_value, lit_len, available)?;
        let max_off = available.min(win_usize);
        if actual_offset == 0 || actual_offset > max_off {
            return Err(ZstdError::BadOffset { offset: actual_offset, available });
        }

        if !writer.exec_seq_within_cap(literals, lit_pos, lit_len, actual_offset, match_len) {
            return Err(ZstdError::OutputTooLarge { limit });
        }
        lit_pos = lit_end;
        seq_idx += 1;
    }

    if !br.is_empty() {
        return Err(ZstdError::CorruptSequences);
    }

    let Some(trailing) = literals.get(lit_pos..) else {
        return Err(ZstdError::CorruptSequences);
    };
    if !trailing.is_empty() {
        let Some(next_regen) = block_regen.checked_add(trailing.len()) else {
            return Err(ZstdError::CorruptSequences);
        };
        block_regen = next_regen;
        if block_regen > MAX_BLOCK_SIZE {
            return Err(ZstdError::BlockTooLarge { size: block_regen });
        }
        check_output_budget(writer.len(), trailing.len(), total_output_cap, limit)?;
        writer.try_write_slice(trailing)?;
    }

    Ok(())
}

pub(crate) fn decompress_in_inner<A: Allocator + Copy>(
    input: &[u8],
    limits: ZstdLimits,
    alloc: A,
    skip_checksum: bool,
) -> Result<Vec<u8, A>, ZstdError> {
    let mut dec = ZstdDecoder::new_in(alloc);
    let mut out = Vec::new_in(alloc);
    dec.decompress_into_inner(input, limits, &mut out, skip_checksum)?;
    Ok(out)
}
