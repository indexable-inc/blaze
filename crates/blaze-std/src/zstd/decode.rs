//! RFC 8878 Zstandard frame, block, literals, and sequence decoder powered by zero-pre-zeroing
//! [`blaze_own::ByteCursor`], pre-resolved [`SeqDecTable`], and `HUF_4X2` Huffman decoding.

use core::alloc::Allocator;

use blaze_own::{ByteCursor, RevBitCursor, SeqOp, Vec};

use crate::digest::xxh64;
use crate::zstd::fse::{
    FseDecTable, LL_DEFAULT_LOG, LL_DEFAULT_NORM, LL_MAX_CODE, LL_MAX_LOG, ML_DEFAULT_LOG,
    ML_DEFAULT_NORM, ML_MAX_CODE, ML_MAX_LOG, OF_DEFAULT_LOG, OF_DEFAULT_NORM, OF_MAX_CODE,
    OF_MAX_LOG, SeqDecTable, SeqKind,
};
use crate::zstd::huffman::HuffTable;
use crate::zstd::{
    MAX_BLOCK_SIZE, SKIPPABLE_MAGIC_MAX, SKIPPABLE_MAGIC_MIN, ZSTD_MAGIC, ZstdError, ZstdLimits,
};

const MAX_PREALLOC_HINT: usize = 16 << 20;
const ZERO_PAGE: [u8; 4096] = [0u8; 4096];

/// Grow `lit_buf` (reused across blocks and calls) via 4 KiB bulk `memcpy` chunks.
#[inline]
fn grow_lit_buf<A: Allocator>(buf: &mut Vec<u8, A>, target: usize) -> Result<(), ZstdError> {
    if buf.len() >= target {
        return Ok(());
    }
    let mut rem = target - buf.len();
    buf.try_reserve(rem)?;
    while rem >= ZERO_PAGE.len() {
        buf.try_extend_from_slice(&ZERO_PAGE)?;
        rem -= ZERO_PAGE.len();
    }
    if rem > 0
        && let Some(tail) = ZERO_PAGE.get(..rem)
    {
        buf.try_extend_from_slice(tail)?;
    }
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
        let &b = self.bytes.get(self.pos).ok_or(ZstdError::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    #[inline]
    fn read_slice(&mut self, len: usize) -> Result<&'a [u8], ZstdError> {
        let end = self.pos.checked_add(len).ok_or(ZstdError::Truncated)?;
        let slice = self.bytes.get(self.pos..end).ok_or(ZstdError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    #[inline]
    fn skip(&mut self, len: usize) -> Result<(), ZstdError> {
        let end = self.pos.checked_add(len).ok_or(ZstdError::Truncated)?;
        if end > self.bytes.len() {
            return Err(ZstdError::Truncated);
        }
        self.pos = end;
        Ok(())
    }

    #[inline]
    fn read_u16_le(&mut self) -> Result<u16, ZstdError> {
        let bytes = self.read_slice(2)?;
        let [b0, b1] = *bytes else {
            return Err(ZstdError::Truncated);
        };
        Ok(u16::from_le_bytes([b0, b1]))
    }

    #[inline]
    fn read_u32_le(&mut self) -> Result<u32, ZstdError> {
        let bytes = self.read_slice(4)?;
        let [b0, b1, b2, b3] = *bytes else {
            return Err(ZstdError::Truncated);
        };
        Ok(u32::from_le_bytes([b0, b1, b2, b3]))
    }

    #[inline]
    fn read_u64_le(&mut self) -> Result<u64, ZstdError> {
        let bytes = self.read_slice(8)?;
        let [b0, b1, b2, b3, b4, b5, b6, b7] = *bytes else {
            return Err(ZstdError::Truncated);
        };
        Ok(u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7]))
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
            let skip_len = usize::try_from(skip_u32).map_err(|_| ZstdError::Truncated)?;
            cur.skip(skip_len)?;
            if cur.is_empty() {
                return Ok(Some(0));
            }
            continue;
        }
        return Err(ZstdError::BadMagic);
    }
    Err(ZstdError::TooManyFrames { limit: ZstdLimits::USUAL.most_frames })
}

/// Contiguous container of the three pre-resolved sequence FSE tables so `decode_exec_leaf`
/// indexes all three tables from a single base pointer register (freeing 2 GPRs).
struct SeqTables {
    ll: SeqDecTable,
    of: SeqDecTable,
    ml: SeqDecTable,
}

impl SeqTables {
    const fn new() -> Self {
        Self {
            ll: SeqDecTable::new_empty(),
            of: SeqDecTable::new_empty(),
            ml: SeqDecTable::new_empty(),
        }
    }
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
            seqs: SeqTables::new(),
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
    alloc: A,
}

impl<A: Allocator + Copy> ZstdDecoder<A> {
    /// Create a new reusable Zstandard decoder backed by `alloc`.
    #[inline]
    pub fn new_in(alloc: A) -> Self {
        Self { state: FrameState::new(), lit_buf: Vec::new_in(alloc), alloc }
    }

    /// Decompress `input` into a freshly allocated `Vec<u8, A>` in this decoder's allocator.
    pub fn decompress(&mut self, input: &[u8], limits: ZstdLimits) -> Result<Vec<u8, A>, ZstdError> {
        let mut out = Vec::new_in(self.alloc);
        self.decompress_into_inner(input, limits, &mut out, false)?;
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

    pub(crate) fn decompress_into_inner(
        &mut self,
        input: &[u8],
        limits: ZstdLimits,
        out: &mut Vec<u8, A>,
        skip_checksum: bool,
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
                let skip_len = usize::try_from(skip_u32).map_err(|_| ZstdError::Truncated)?;
                cur.skip(skip_len)?;
                continue;
            }
            if magic != ZSTD_MAGIC {
                return Err(ZstdError::BadMagic);
            }

            let hdr = parse_frame_header(&mut cur)?;
            let produced_so_far = out.len().saturating_sub(start_out_len);
            if let Some(cs) = hdr.content_size {
                let rem_cap = limits.most_output_bytes.saturating_sub(produced_so_far);
                let rem_cap_u64 = u64::try_from(rem_cap).unwrap_or(u64::MAX);
                if cs > rem_cap_u64 {
                    return Err(ZstdError::OutputTooLarge { limit: limits.most_output_bytes });
                }
            }
            let win_limit_u64 = u64::try_from(limits.most_window_bytes).unwrap_or(u64::MAX);
            if hdr.window_bytes > win_limit_u64 {
                return Err(ZstdError::WindowTooLarge {
                    window_bytes: hdr.window_bytes,
                    limit: limits.most_window_bytes,
                });
            }

            let frame_start = out.len();
            let total_output_cap = start_out_len.saturating_add(limits.most_output_bytes);
            self.state.reset_frame();

            {
                let mut writer = out.byte_cursor();
                // Reserve the entire frame once without zeroing (plus 64B slack for NEON wildcopy).
                if let Some(cs) = hdr.content_size {
                    let safe_cap = input.len().saturating_mul(16).max(65_536).min(MAX_PREALLOC_HINT);
                    let hint = usize::try_from(cs).unwrap_or(safe_cap).min(safe_cap);
                    writer.try_reserve(hint.saturating_add(64))?;
                }

                loop {
                    let bh = cur.read_slice(3)?;
                    let [b0, b1, b2] = *bh else {
                        return Err(ZstdError::Truncated);
                    };
                    let hdr_u32 = u32::from_le_bytes([b0, b1, b2, 0]);
                    let last_block = (hdr_u32 & 1) != 0;
                    let block_type = (hdr_u32 >> 1) & 0x03;
                    let block_size =
                        usize::try_from(hdr_u32 >> 3).map_err(|_| ZstdError::Truncated)?;

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
                                &mut self.state,
                                &mut self.lit_buf,
                                &mut writer,
                                frame_start,
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
                let hash = xxh64(frame_slice, 0);
                let [c0, c1, c2, c3, ..] = hash.to_le_bytes();
                let actual = u32::from_le_bytes([c0, c1, c2, c3]);
                if !skip_checksum && actual != expected {
                    return Err(ZstdError::ChecksumMismatch { expected, actual });
                }
            }

            if let Some(expected) = hdr.content_size {
                let actual = out.len().saturating_sub(frame_start);
                if u64::try_from(actual).ok() != Some(expected) {
                    return Err(ZstdError::ContentSizeMismatch { expected, actual });
                }
            }
        }

        Ok(())
    }
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
                    let regen = usize::try_from((v >> 4) & 0x3FF)
                        .map_err(|_| ZstdError::CorruptLiterals)?;
                    let comp = usize::try_from((v >> 14) & 0x3FF)
                        .map_err(|_| ZstdError::CorruptLiterals)?;
                    let streams: u8 = if size_format == 0 { 1 } else { 4 };
                    (streams, regen, comp)
                }
                2 => {
                    let b1 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let b2 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let b3 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let v = u32::from_le_bytes([b0, b1, b2, b3]);
                    let regen = usize::try_from((v >> 4) & 0x3FFF)
                        .map_err(|_| ZstdError::CorruptLiterals)?;
                    let comp = usize::try_from((v >> 18) & 0x3FFF)
                        .map_err(|_| ZstdError::CorruptLiterals)?;
                    (4u8, regen, comp)
                }
                _ => {
                    let b1 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let b2 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let b3 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let b4 = cur.read_u8().map_err(|_| ZstdError::CorruptLiterals)?;
                    let v = u64::from_le_bytes([b0, b1, b2, b3, b4, 0, 0, 0]);
                    let regen = usize::try_from((v >> 4) & 0x3_FFFF)
                        .map_err(|_| ZstdError::CorruptLiterals)?;
                    let comp = usize::try_from((v >> 22) & 0x3_FFFF)
                        .map_err(|_| ZstdError::CorruptLiterals)?;
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
    table: &mut SeqDecTable,
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
                table.compile_from(scratch, kind)?;
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
            table.compile_from(scratch, kind)?;
            *is_predef = false;
            *has_table = true;
            Ok(())
        }
        2 => {
            let used = scratch.decode_header_and_build(cur.rest(), max_sym, max_log)?;
            cur.skip(used).map_err(|_| ZstdError::CorruptFse)?;
            table.compile_from(scratch, kind)?;
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
        let off = usize::try_from(offset_value - 3).map_err(|_| ZstdError::CorruptSequences)?;
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
                let off = r0
                    .checked_sub(1)
                    .ok_or(ZstdError::BadOffset { offset: 0, available })?;
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
    let next = current_len.checked_add(add).ok_or(ZstdError::OutputTooLarge { limit })?;
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
fn extract_top(win: u64, n: u32) -> u32 {
    // `(!n) & 63 == 63 - (n & 63)`, exact for all `n in 0..=32` (returns `0` when `n == 0`)
    // and compiles to a single `mvn` + `lsr` reusing the already-extracted `n` register.
    let v = (win >> 1).wrapping_shr(!n);
    let [b0, b1, b2, b3, ..] = v.to_le_bytes();
    u32::from_le_bytes([b0, b1, b2, b3])
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
    Pending(SeqOp),
    Wide,
    LitOverflow,
    OutOverflow { next_out: usize },
    BadOffset { offset: usize, available: usize },
}

/// Zero-call fused sequence decoder + 128-bit NEON wildcopy executor.
///
/// Contains ZERO function calls (`bl`) so LLVM keeps all ~23 hot registers (FSE tables, states,
/// bitstream cursor, repeat offsets, literal pointer, and output `ByteCursor`) live in AArch64's
/// 29 GPRs with zero stack spills, while letting Apple Silicon's out-of-order engine overlap
/// sequence `i`'s NEON wildcopies with sequence `i + 1`'s FSE table loads and bit shifts.
///
/// Whenever a rare sequence (`lit_len > 32`, `offset < 16`, final 32B literal slack, or a
/// `> 56`-bit sequence) occurs, the leaf returns `LeafStop::Pending` / `LeafStop::Wide` so the
/// outer driver executes that single sequence and immediately re-enters the leaf loop.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn decode_exec_leaf<const SINGLE_RELOAD: bool, A: Allocator>(
    tables: &SeqTables,
    literals: &[u8],
    writer: &mut ByteCursor<'_, A>,
    st: &mut FusedState<'_>,
    last_idx: usize,
    frame_start: usize,
    win_usize: usize,
    block_out_limit: usize,
) -> LeafStop {
    let mut br = st.br;
    let mut ll_state = st.ll_state;
    let mut of_state = st.of_state;
    let mut ml_state = st.ml_state;
    let [mut r0, mut r1, mut r2] = st.rep;
    let mut lit_pos = st.lit_pos;
    let mut seq_idx = st.seq_idx;
    let lit_total = literals.len();
    let lit_safe_end = lit_total.saturating_sub(32);
    let out_safe_limit = block_out_limit.min(writer.cap().saturating_sub(48));
    let min_avail: usize = if SINGLE_RELOAD { 8 } else { 16 };
    let mut stop = LeafStop::Done;

    while seq_idx < last_idx && br.avail_bytes() >= min_avail {
        let (ll_base, ll_eb, ll_nb, ll_bl) = tables.ll.entry_fast(ll_state).unpack();
        let (of_base, of_eb, of_nb, of_bl) = tables.of.entry_fast(of_state).unpack();
        let (ml_base, ml_eb, ml_nb, ml_bl) = tables.ml.entry_fast(ml_state).unpack();

        if !br.reload_if_slack8() {
            break;
        }
        let extras_bits = of_eb + ml_eb + ll_eb;
        let total = extras_bits + ll_nb + ml_nb + of_nb;
        if !SINGLE_RELOAD && total > 56 {
            stop = LeafStop::Wide;
            break;
        }

        let mut we = br.bits_window();
        let mut ws = we.wrapping_shl(extras_bits);
        br.advance_fast(total);

        let ll_add = usize::try_from(extract_top(ws, ll_nb)).unwrap_or(0);
        ws = ws.wrapping_shl(ll_nb);
        let ml_add = usize::try_from(extract_top(ws, ml_nb)).unwrap_or(0);
        ws = ws.wrapping_shl(ml_nb);
        let of_add = usize::try_from(extract_top(ws, of_nb)).unwrap_or(0);

        let of_extra = extract_top(we, of_eb);
        we = we.wrapping_shl(of_eb);
        let ml_extra = extract_top(we, ml_eb);
        we = we.wrapping_shl(ml_eb);
        let ll_extra = extract_top(we, ll_eb);

        ll_state = ll_bl + ll_add;
        ml_state = ml_bl + ml_add;
        of_state = of_bl + of_add;

        let offset_value = of_base.wrapping_add(of_extra);
        let match_len_u32 = ml_base.wrapping_add(ml_extra);
        let lit_len_u32 = ll_base.wrapping_add(ll_extra);

        // Branchless 2-level `csel` tree in 32-bit registers for repeat offsets.
        let rep_code = offset_value.wrapping_sub(1) + u32::from(lit_len_u32 == 0);
        let r0_sub = r0.wrapping_sub(1);
        let r01 = if rep_code == 0 { r0 } else { r1 };
        let r23 = if rep_code == 2 { r2 } else { r0_sub };
        let rep_off = if rep_code <= 1 { r01 } else { r23 };
        let new_off = offset_value.wrapping_sub(3);
        let actual_offset = if offset_value > 3 { new_off } else { rep_off };
        r2 = if rep_code <= 1 { r2 } else { r1 };
        r1 = if rep_code == 0 { r1 } else { r0 };
        r0 = actual_offset;

        let lit_len = usize::try_from(lit_len_u32).unwrap_or(usize::MAX);
        let match_len = usize::try_from(match_len_u32).unwrap_or(usize::MAX);
        let off_usize = usize::try_from(actual_offset).unwrap_or(usize::MAX);

        let lit_end = lit_pos + lit_len;
        let after_lit = writer.len() + lit_len;
        let next_out = after_lit + match_len;
        let available = after_lit - frame_start;
        let max_off = available.min(win_usize);
        if lit_pos > lit_safe_end
            || lit_len > 32
            || next_out > out_safe_limit
            || off_usize.wrapping_sub(1) >= max_off
        {
            if lit_end > lit_total {
                stop = LeafStop::LitOverflow;
                break;
            }
            if next_out > block_out_limit {
                stop = LeafStop::OutOverflow { next_out };
                break;
            }
            if off_usize.wrapping_sub(1) >= max_off {
                stop = LeafStop::BadOffset { offset: off_usize, available };
                break;
            }
            stop = LeafStop::Pending(SeqOp {
                lit_len: lit_len_u32,
                match_len: match_len_u32,
                offset: actual_offset,
            });
            break;
        }

        if !writer.try_exec_seq_leaf(literals, lit_pos, lit_len, off_usize, match_len) {
            stop = LeafStop::Pending(SeqOp {
                lit_len: lit_len_u32,
                match_len: match_len_u32,
                offset: actual_offset,
            });
            break;
        }
        lit_pos = lit_end;
        seq_idx += 1;
    }

    st.br = br;
    st.ll_state = ll_state;
    st.of_state = of_state;
    st.ml_state = ml_state;
    st.rep = [r0, r1, r2];
    st.lit_pos = lit_pos;
    st.seq_idx = seq_idx;
    stop
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
    let (ll_base, ll_eb, ll_nb, ll_bl) = tables.ll.entry_fast(st.ll_state).unpack();
    let (of_base, of_eb, of_nb, of_bl) = tables.of.entry_fast(st.of_state).unpack();
    let (ml_base, ml_eb, ml_nb, ml_bl) = tables.ml.entry_fast(st.ml_state).unpack();

    let of_extra = st.br.read_bits_branchless(of_eb);
    let ml_extra = st.br.read_bits_branchless(ml_eb);
    let _ = st.br.reload_if_slack8();
    let ll_extra = st.br.read_bits_branchless(ll_eb);
    let ll_add = usize::try_from(st.br.read_bits_branchless(ll_nb)).unwrap_or(0);
    let ml_add = usize::try_from(st.br.read_bits_branchless(ml_nb)).unwrap_or(0);
    let of_add = usize::try_from(st.br.read_bits_branchless(of_nb)).unwrap_or(0);

    st.ll_state = ll_bl + ll_add;
    st.ml_state = ml_bl + ml_add;
    st.of_state = of_bl + of_add;

    let offset_value = of_base.wrapping_add(of_extra);
    let match_len = usize::try_from(ml_base.wrapping_add(ml_extra))
        .map_err(|_| ZstdError::CorruptSequences)?;
    let lit_len = usize::try_from(ll_base.wrapping_add(ll_extra))
        .map_err(|_| ZstdError::CorruptSequences)?;

    let [r0, r1, r2] = st.rep;
    let mut rep_usize = [
        usize::try_from(r0).unwrap_or(usize::MAX),
        usize::try_from(r1).unwrap_or(usize::MAX),
        usize::try_from(r2).unwrap_or(usize::MAX),
    ];
    let lit_end = st.lit_pos.checked_add(lit_len).ok_or(ZstdError::CorruptSequences)?;
    if lit_end > literals.len() {
        return Err(ZstdError::CorruptSequences);
    }
    let after_lit = writer.len().checked_add(lit_len).ok_or(ZstdError::OutputTooLarge { limit })?;
    let next_out = after_lit.checked_add(match_len).ok_or(ZstdError::OutputTooLarge { limit })?;
    if next_out > block_out_limit {
        let regen = next_out - block_start_out;
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
    st.rep = [
        u32::try_from(nr0).unwrap_or(u32::MAX),
        u32::try_from(nr1).unwrap_or(u32::MAX),
        u32::try_from(nr2).unwrap_or(u32::MAX),
    ];
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
        rep: [
            u32::try_from(r0_in).unwrap_or(u32::MAX),
            u32::try_from(r1_in).unwrap_or(u32::MAX),
            u32::try_from(r2_in).unwrap_or(u32::MAX),
        ],
        lit_pos: 0,
        seq_idx: 0,
    };
    let max_seq_bits = u32::from(tables.ll.max_extra_bits())
        + u32::from(tables.of.max_extra_bits())
        + u32::from(tables.ml.max_extra_bits())
        + u32::from(tables.ll.accuracy_log())
        + u32::from(tables.of.accuracy_log())
        + u32::from(tables.ml.accuracy_log());
    let single_reload = max_seq_bits <= 56;

    loop {
        let stop = if single_reload {
            decode_exec_leaf::<true, A>(
                tables,
                literals,
                writer,
                &mut st,
                last_idx,
                frame_start,
                win_usize,
                block_out_limit,
            )
        } else {
            decode_exec_leaf::<false, A>(
                tables,
                literals,
                writer,
                &mut st,
                last_idx,
                frame_start,
                win_usize,
                block_out_limit,
            )
        };
        match stop {
            LeafStop::Done => break,
            LeafStop::Pending(seq) => {
                let lit_len =
                    usize::try_from(seq.lit_len).map_err(|_| ZstdError::CorruptSequences)?;
                let match_len =
                    usize::try_from(seq.match_len).map_err(|_| ZstdError::CorruptSequences)?;
                let offset =
                    usize::try_from(seq.offset).map_err(|_| ZstdError::CorruptSequences)?;
                if !writer.exec_seq_within_cap(literals, st.lit_pos, lit_len, offset, match_len) {
                    return Err(ZstdError::OutputTooLarge { limit });
                }
                st.lit_pos += lit_len;
                st.seq_idx += 1;
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
            LeafStop::LitOverflow => return Err(ZstdError::CorruptSequences),
            LeafStop::OutOverflow { next_out } => {
                let regen = next_out - block_start_out;
                if regen > MAX_BLOCK_SIZE {
                    return Err(ZstdError::BlockTooLarge { size: regen });
                }
                return Err(ZstdError::OutputTooLarge { limit });
            }
            LeafStop::BadOffset { offset, available } => {
                return Err(ZstdError::BadOffset { offset, available });
            }
        }
    }

    let [r0_out, r1_out, r2_out] = st.rep;
    Ok(FastSeqOut {
        br: st.br,
        ll_state: st.ll_state,
        of_state: st.of_state,
        ml_state: st.ml_state,
        rep: [
            usize::try_from(r0_out).unwrap_or(usize::MAX),
            usize::try_from(r1_out).unwrap_or(usize::MAX),
            usize::try_from(r2_out).unwrap_or(usize::MAX),
        ],
        lit_pos: st.lit_pos,
        seq_idx: st.seq_idx,
    })
}

#[inline(never)]
fn decode_compressed_block<A: Allocator>(
    block_bytes: &[u8],
    state: &mut FrameState,
    lit_buf: &mut Vec<u8, A>,
    writer: &mut ByteCursor<'_, A>,
    frame_start: usize,
    window_bytes: u64,
    total_output_cap: usize,
    limit: usize,
) -> Result<(), ZstdError> {
    let mut cur = Cursor::new(block_bytes);
    let lit_ref = decode_literals(&mut cur, state, lit_buf)?;
    let literals: &[u8] = match lit_ref {
        LiteralsRef::Borrowed(slice) => slice,
        LiteralsRef::Buffered(n) => lit_buf.as_slice().get(..n).ok_or(ZstdError::CorruptLiterals)?,
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
        &mut state.seqs.ll,
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
        &mut state.seqs.of,
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
        &mut state.seqs.ml,
        SeqKind::Ml,
        &mut state.has_ml,
        &mut state.ml_predef,
        &ML_DEFAULT_NORM,
        ML_DEFAULT_LOG,
        ML_MAX_CODE,
        ML_MAX_LOG,
    )?;

    let mut br = RevBitCursor::new(cur.rest()).ok_or(ZstdError::CorruptSequences)?;
    let ll_log = u32::from(state.seqs.ll.accuracy_log());
    let of_log = u32::from(state.seqs.of.accuracy_log());
    let ml_log = u32::from(state.seqs.ml.accuracy_log());

    let ll_state = usize::try_from(br.read_bits(ll_log).ok_or(ZstdError::CorruptSequences)?)
        .map_err(|_| ZstdError::CorruptSequences)?;
    let of_state = usize::try_from(br.read_bits(of_log).ok_or(ZstdError::CorruptSequences)?)
        .map_err(|_| ZstdError::CorruptSequences)?;
    let ml_state = usize::try_from(br.read_bits(ml_log).ok_or(ZstdError::CorruptSequences)?)
        .map_err(|_| ZstdError::CorruptSequences)?;

    if ll_state >= (1usize << ll_log)
        || of_state >= (1usize << of_log)
        || ml_state >= (1usize << ml_log)
    {
        return Err(ZstdError::CorruptSequences);
    }

    // Reserve room for up to one full block (+ 64B wildcopy slack) once, with ZERO zeroing.
    let block_start_out = writer.len();
    let block_out_limit = block_start_out.saturating_add(MAX_BLOCK_SIZE).min(total_output_cap);
    let block_reserve = block_out_limit.saturating_sub(block_start_out).saturating_add(64);
    writer.try_reserve(block_reserve)?;

    let win_usize = usize::try_from(window_bytes).unwrap_or(usize::MAX);
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

    // Exact tail loop for the final ~10 sequences (`br.avail_bytes() < 16` or `seq_idx == last_idx`).
    let mut block_regen = writer.len().saturating_sub(block_start_out);
    state.rep = rep;
    while seq_idx < num_seq {
        let (ll_base, ll_eb, ll_nb, ll_bl) = state.seqs.ll.entry_fast(ll_state).unpack();
        let (of_base, of_eb, of_nb, of_bl) = state.seqs.of.entry_fast(of_state).unpack();
        let (ml_base, ml_eb, ml_nb, ml_bl) = state.seqs.ml.entry_fast(ml_state).unpack();

        let of_extra = br.read_bits(of_eb).ok_or(ZstdError::CorruptSequences)?;
        let ml_extra = br.read_bits(ml_eb).ok_or(ZstdError::CorruptSequences)?;
        let ll_extra = br.read_bits(ll_eb).ok_or(ZstdError::CorruptSequences)?;
        if seq_idx < last_idx {
            let ll_add = usize::try_from(br.read_bits(ll_nb).ok_or(ZstdError::CorruptSequences)?)
                .map_err(|_| ZstdError::CorruptSequences)?;
            let ml_add = usize::try_from(br.read_bits(ml_nb).ok_or(ZstdError::CorruptSequences)?)
                .map_err(|_| ZstdError::CorruptSequences)?;
            let of_add = usize::try_from(br.read_bits(of_nb).ok_or(ZstdError::CorruptSequences)?)
                .map_err(|_| ZstdError::CorruptSequences)?;
            ll_state = ll_bl + ll_add;
            ml_state = ml_bl + ml_add;
            of_state = of_bl + of_add;
        }

        let offset_value = of_base.wrapping_add(of_extra);
        let match_len = usize::try_from(ml_base.wrapping_add(ml_extra))
            .map_err(|_| ZstdError::CorruptSequences)?;
        let lit_len = usize::try_from(ll_base.wrapping_add(ll_extra))
            .map_err(|_| ZstdError::CorruptSequences)?;

        let lit_end = lit_pos.checked_add(lit_len).ok_or(ZstdError::CorruptSequences)?;
        if lit_end > lit_total {
            return Err(ZstdError::CorruptSequences);
        }
        let seq_out = lit_len.checked_add(match_len).ok_or(ZstdError::CorruptSequences)?;
        block_regen = block_regen.checked_add(seq_out).ok_or(ZstdError::CorruptSequences)?;
        if block_regen > MAX_BLOCK_SIZE {
            return Err(ZstdError::BlockTooLarge { size: block_regen });
        }
        check_output_budget(writer.len(), seq_out, total_output_cap, limit)?;

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

    let trailing = literals.get(lit_pos..).ok_or(ZstdError::CorruptSequences)?;
    if !trailing.is_empty() {
        block_regen = block_regen
            .checked_add(trailing.len())
            .ok_or(ZstdError::CorruptSequences)?;
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
