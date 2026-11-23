//! RFC 8878 Zstandard frame, block, literals, and sequence decoder.

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::digest::xxh64;
use crate::zstd::fse::{
    FseDecTable, LL_DEFAULT_LOG, LL_DEFAULT_NORM, LL_MAX_CODE, LL_MAX_LOG, ML_DEFAULT_LOG,
    ML_DEFAULT_NORM, ML_MAX_CODE, ML_MAX_LOG, OF_DEFAULT_LOG, OF_DEFAULT_NORM, OF_MAX_CODE,
    OF_MAX_LOG, RevBitReader, SeqDecTable, SeqKind,
};
use crate::zstd::huffman::HuffTable;
use crate::zstd::{
    MAX_BLOCK_SIZE, SKIPPABLE_MAGIC_MAX, SKIPPABLE_MAGIC_MIN, ZSTD_MAGIC, ZstdError, ZstdLimits,
};

const MAX_PREALLOC_HINT: usize = 16 << 20;
const ZERO_PAGE: [u8; 4096] = [0u8; 4096];

/// Extend `out` by `extra` zero bytes using 4 KiB bulk `memcpy` chunks instead of scalar byte push.
#[inline]
fn grow_zeroed<A: Allocator>(out: &mut Vec<u8, A>, extra: usize) -> Result<(), ZstdError> {
    if extra == 0 {
        return Ok(());
    }
    out.try_reserve(extra)?;
    let mut rem = extra;
    while rem >= ZERO_PAGE.len() {
        out.try_extend_from_slice(&ZERO_PAGE)?;
        rem -= ZERO_PAGE.len();
    }
    if rem > 0
        && let Some(tail) = ZERO_PAGE.get(..rem)
    {
        out.try_extend_from_slice(tail)?;
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

struct FrameState {
    huff: HuffTable,
    scratch_fse: FseDecTable,
    ll_table: SeqDecTable,
    of_table: SeqDecTable,
    ml_table: SeqDecTable,
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
            ll_table: SeqDecTable::new_empty(),
            of_table: SeqDecTable::new_empty(),
            ml_table: SeqDecTable::new_empty(),
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
            // Raw_Literals_Block (0) or RLE_Literals_Block (1)
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
                if lit_buf.len() < regen_size {
                    let add = regen_size - lit_buf.len();
                    grow_zeroed(lit_buf, add)?;
                }
                if let Some(dst) = lit_buf.as_mut_slice().get_mut(..regen_size) {
                    dst.fill(val);
                }
                Ok(LiteralsRef::Buffered(regen_size))
            }
        }
        _ => {
            // Compressed_Literals_Block (2) or Treeless_Literals_Block (3)
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

            // Reuse `lit_buf` across blocks without zeroing already-allocated bytes.
            if lit_buf.len() < regen_size {
                let add = regen_size - lit_buf.len();
                grow_zeroed(lit_buf, add)?;
            }
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

#[inline(always)]
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
    most_output_bytes: usize,
) -> Result<(), ZstdError> {
    let next = current_len.checked_add(add).ok_or(ZstdError::OutputTooLarge {
        limit: most_output_bytes,
    })?;
    if next > most_output_bytes {
        return Err(ZstdError::OutputTooLarge { limit: most_output_bytes });
    }
    Ok(())
}

#[inline(always)]
fn copy_lits_fast(literals: &[u8], lit_pos: usize, lit_len: usize, buf: &mut [u8], out_pos: usize) {
    if lit_len <= 16
        && let Some(s16) = literals.get(lit_pos..).and_then(|s| s.first_chunk::<16>())
        && let Some(d16) = buf.get_mut(out_pos..).and_then(|s| s.first_chunk_mut::<16>())
    {
        *d16 = *s16;
        return;
    }
    if lit_len <= 32
        && let Some(s32) = literals.get(lit_pos..).and_then(|s| s.first_chunk::<32>())
        && let Some(d32) = buf.get_mut(out_pos..).and_then(|s| s.first_chunk_mut::<32>())
    {
        *d32 = *s32;
        return;
    }
    copy_lits_exact(literals, lit_pos, lit_len, buf, out_pos);
}

#[cold]
#[inline(never)]
fn copy_lits_exact(literals: &[u8], lit_pos: usize, lit_len: usize, buf: &mut [u8], out_pos: usize) {
    if let (Some(src), Some(dst)) = (
        literals.get(lit_pos..lit_pos + lit_len),
        buf.get_mut(out_pos..out_pos + lit_len),
    ) {
        dst.copy_from_slice(src);
    }
}

/// Fast inline 128-bit/64-bit wild copy for common Zstandard matches (`<= 32` bytes),
/// deferring long or overlapping matches to a `#[cold]` helper so the sequence loop keeps
/// all state in registers.
#[inline(always)]
fn copy_match_in_slice(buf: &mut [u8], out_pos: usize, offset: usize, match_len: usize) {
    let src = out_pos - offset;

    if offset >= 16
        && match_len <= 16
        && let Some(s16) = buf.get(src..).and_then(|s| s.first_chunk::<16>())
    {
        let v0 = *s16;
        if let Some(d16) = buf.get_mut(out_pos..).and_then(|s| s.first_chunk_mut::<16>()) {
            *d16 = v0;
            return;
        }
    }

    if offset >= 16 && match_len <= 32 && out_pos.saturating_add(32) <= buf.len() {
        if let Some(s16) = buf.get(src..).and_then(|s| s.first_chunk::<16>()) {
            let v0 = *s16;
            if let Some(d16) = buf.get_mut(out_pos..).and_then(|s| s.first_chunk_mut::<16>()) {
                *d16 = v0;
            }
        }
        if let Some(s16) = buf.get(src + 16..).and_then(|s| s.first_chunk::<16>()) {
            let v1 = *s16;
            if let Some(d16) = buf.get_mut(out_pos + 16..).and_then(|s| s.first_chunk_mut::<16>()) {
                *d16 = v1;
            }
        }
        return;
    }

    if offset >= 8 && match_len <= 16 && out_pos.saturating_add(16) <= buf.len() {
        if let Some(s8) = buf.get(src..).and_then(|s| s.first_chunk::<8>()) {
            let v0 = *s8;
            if let Some(d8) = buf.get_mut(out_pos..).and_then(|s| s.first_chunk_mut::<8>()) {
                *d8 = v0;
            }
        }
        if match_len > 8
            && let Some(s8) = buf.get(src + 8..).and_then(|s| s.first_chunk::<8>())
        {
            let v1 = *s8;
            if let Some(d8) = buf.get_mut(out_pos + 8..).and_then(|s| s.first_chunk_mut::<8>()) {
                *d8 = v1;
            }
        }
        return;
    }

    if offset == 1
        && match_len <= 16
        && let Some(&byte) = buf.get(src)
        && let Some(d16) = buf.get_mut(out_pos..).and_then(|s| s.first_chunk_mut::<16>())
    {
        *d16 = [byte; 16];
        return;
    }

    copy_match_slow(buf, out_pos, src, offset, match_len);
}

#[cold]
#[inline(never)]
fn copy_match_slow(buf: &mut [u8], out_pos: usize, src: usize, offset: usize, match_len: usize) {
    if offset == 1 {
        let byte = buf.get(src).copied().unwrap_or(0);
        if let Some(dst) = buf.get_mut(out_pos..out_pos + match_len) {
            dst.fill(byte);
        }
        return;
    }
    if offset >= match_len {
        if let Some((left, right)) = buf.split_at_mut_checked(out_pos)
            && let (Some(src_s), Some(dst_s)) =
                (left.get(src..src + match_len), right.get_mut(..match_len))
        {
            dst_s.copy_from_slice(src_s);
        }
        return;
    }
    // Overlapping repeat (`1 < offset < match_len`): seed `offset` bytes then double.
    if let Some((left, right)) = buf.split_at_mut_checked(out_pos)
        && let (Some(src_s), Some(dst_s)) = (left.get(src..out_pos), right.get_mut(..offset))
    {
        dst_s.copy_from_slice(src_s);
    }
    let mut copied = offset;
    while copied < match_len {
        let chunk = copied.min(match_len - copied);
        if let Some((head, tail)) = buf.split_at_mut_checked(out_pos + copied)
            && let (Some(src_s), Some(dst_s)) =
                (head.get(out_pos..out_pos + chunk), tail.get_mut(..chunk))
        {
            dst_s.copy_from_slice(src_s);
        }
        copied += chunk;
    }
}

struct SeqLoopCtx {
    seq_idx: usize,
    ll_state: usize,
    of_state: usize,
    ml_state: usize,
    lit_pos: usize,
    block_regen: usize,
}

enum SeqLoopOutcome {
    Done,
    NeedRoom,
}

/// Tight register-resident sequence execution loop operating directly on `&mut [u8]`.
fn run_sequences_slice(
    buf: &mut [u8],
    out_pos: &mut usize,
    literals: &[u8],
    br: &mut RevBitReader<'_>,
    ll_table: &SeqDecTable,
    of_table: &SeqDecTable,
    ml_table: &SeqDecTable,
    rep: &mut [usize; 3],
    ctx: &mut SeqLoopCtx,
    num_seq: usize,
    frame_start: usize,
    win_usize: usize,
    most_output_bytes: usize,
) -> Result<SeqLoopOutcome, ZstdError> {
    let buf_len = buf.len();
    let lit_total = literals.len();
    let mut cur_out = *out_pos;
    let mut lit_pos = ctx.lit_pos;
    let mut block_regen = ctx.block_regen;
    let mut ll_state = ctx.ll_state;
    let mut of_state = ctx.of_state;
    let mut ml_state = ctx.ml_state;
    let mut seq_idx = ctx.seq_idx;
    let last_idx = num_seq - 1;

    while seq_idx < num_seq {
        let ll_e = ll_table.entry_fast(ll_state);
        let of_e = of_table.entry_fast(of_state);
        let ml_e = ml_table.entry_fast(ml_state);

        // Save pre-step state in case a corrupt frame exceeds preallocated `buf_len`.
        let saved_ll = ll_state;
        let saved_of = of_state;
        let saved_ml = ml_state;

        let (of_extra, ml_extra, ll_extra);
        if seq_idx < last_idx && br.byte_pos() > 0 {
            br.reload();
            if br.byte_pos() > 0 {
                // Fast branchless path: `bits_consumed <= 7` (57+ bits guaranteed live).
                of_extra = br.read_bits_branchless(u32::from(of_e.extra_bits));
                ml_extra = br.read_bits_branchless(u32::from(ml_e.extra_bits));
                if br.bits_consumed() > 48 {
                    br.reload();
                    ll_extra = br
                        .read_bits(u32::from(ll_e.extra_bits))
                        .ok_or(ZstdError::CorruptSequences)?;
                } else {
                    ll_extra = br.read_bits_branchless(u32::from(ll_e.extra_bits));
                }

                if br.bits_consumed() > 38 {
                    br.reload();
                }
                if br.bits_consumed() <= 38 {
                    let ll_add = usize::try_from(br.read_bits_branchless(u32::from(ll_e.num_bits)))
                        .map_err(|_| ZstdError::CorruptSequences)?;
                    let ml_add = usize::try_from(br.read_bits_branchless(u32::from(ml_e.num_bits)))
                        .map_err(|_| ZstdError::CorruptSequences)?;
                    let of_add = usize::try_from(br.read_bits_branchless(u32::from(of_e.num_bits)))
                        .map_err(|_| ZstdError::CorruptSequences)?;
                    ll_state = usize::from(ll_e.baseline) + ll_add;
                    ml_state = usize::from(ml_e.baseline) + ml_add;
                    of_state = usize::from(of_e.baseline) + of_add;
                } else {
                    let ll_add = usize::try_from(
                        br.read_bits(u32::from(ll_e.num_bits))
                            .ok_or(ZstdError::CorruptSequences)?,
                    )
                    .map_err(|_| ZstdError::CorruptSequences)?;
                    let ml_add = usize::try_from(
                        br.read_bits(u32::from(ml_e.num_bits))
                            .ok_or(ZstdError::CorruptSequences)?,
                    )
                    .map_err(|_| ZstdError::CorruptSequences)?;
                    let of_add = usize::try_from(
                        br.read_bits(u32::from(of_e.num_bits))
                            .ok_or(ZstdError::CorruptSequences)?,
                    )
                    .map_err(|_| ZstdError::CorruptSequences)?;
                    ll_state = usize::from(ll_e.baseline) + ll_add;
                    ml_state = usize::from(ml_e.baseline) + ml_add;
                    of_state = usize::from(of_e.baseline) + of_add;
                }
            } else {
                // Just crossed `byte_pos == 0`: exact checked reads for final ~10 sequences.
                of_extra = br
                    .read_bits(u32::from(of_e.extra_bits))
                    .ok_or(ZstdError::CorruptSequences)?;
                ml_extra = br
                    .read_bits(u32::from(ml_e.extra_bits))
                    .ok_or(ZstdError::CorruptSequences)?;
                ll_extra = br
                    .read_bits(u32::from(ll_e.extra_bits))
                    .ok_or(ZstdError::CorruptSequences)?;
                let ll_add = usize::try_from(
                    br.read_bits(u32::from(ll_e.num_bits))
                        .ok_or(ZstdError::CorruptSequences)?,
                )
                .map_err(|_| ZstdError::CorruptSequences)?;
                let ml_add = usize::try_from(
                    br.read_bits(u32::from(ml_e.num_bits))
                        .ok_or(ZstdError::CorruptSequences)?,
                )
                .map_err(|_| ZstdError::CorruptSequences)?;
                let of_add = usize::try_from(
                    br.read_bits(u32::from(of_e.num_bits))
                        .ok_or(ZstdError::CorruptSequences)?,
                )
                .map_err(|_| ZstdError::CorruptSequences)?;
                ll_state = usize::from(ll_e.baseline) + ll_add;
                ml_state = usize::from(ml_e.baseline) + ml_add;
                of_state = usize::from(of_e.baseline) + of_add;
            }
        } else {
            of_extra = br
                .read_bits(u32::from(of_e.extra_bits))
                .ok_or(ZstdError::CorruptSequences)?;
            ml_extra = br
                .read_bits(u32::from(ml_e.extra_bits))
                .ok_or(ZstdError::CorruptSequences)?;
            ll_extra = br
                .read_bits(u32::from(ll_e.extra_bits))
                .ok_or(ZstdError::CorruptSequences)?;
            if seq_idx < last_idx {
                let ll_add = usize::try_from(
                    br.read_bits(u32::from(ll_e.num_bits))
                        .ok_or(ZstdError::CorruptSequences)?,
                )
                .map_err(|_| ZstdError::CorruptSequences)?;
                let ml_add = usize::try_from(
                    br.read_bits(u32::from(ml_e.num_bits))
                        .ok_or(ZstdError::CorruptSequences)?,
                )
                .map_err(|_| ZstdError::CorruptSequences)?;
                let of_add = usize::try_from(
                    br.read_bits(u32::from(of_e.num_bits))
                        .ok_or(ZstdError::CorruptSequences)?,
                )
                .map_err(|_| ZstdError::CorruptSequences)?;
                ll_state = usize::from(ll_e.baseline) + ll_add;
                ml_state = usize::from(ml_e.baseline) + ml_add;
                of_state = usize::from(of_e.baseline) + of_add;
            }
        }

        let offset_value = of_e.base.wrapping_add(of_extra);
        let match_len = usize::try_from(ml_e.base.wrapping_add(ml_extra))
            .map_err(|_| ZstdError::CorruptSequences)?;
        let lit_len = usize::try_from(ll_e.base.wrapping_add(ll_extra))
            .map_err(|_| ZstdError::CorruptSequences)?;

        let lit_end = lit_pos.checked_add(lit_len).ok_or(ZstdError::CorruptSequences)?;
        if lit_end > lit_total {
            return Err(ZstdError::CorruptSequences);
        }

        let seq_out = lit_len.checked_add(match_len).ok_or(ZstdError::CorruptSequences)?;
        let next_regen = block_regen.checked_add(seq_out).ok_or(ZstdError::CorruptSequences)?;
        if next_regen > MAX_BLOCK_SIZE {
            return Err(ZstdError::BlockTooLarge { size: next_regen });
        }
        check_output_budget(cur_out, seq_out, most_output_bytes)?;

        let needed = cur_out + seq_out;
        if needed > buf_len {
            // Cold path: grow buffer and execute this single sequence in the caller.
            *out_pos = cur_out;
            ctx.seq_idx = seq_idx;
            ctx.ll_state = saved_ll;
            ctx.of_state = saved_of;
            ctx.ml_state = saved_ml;
            ctx.lit_pos = lit_pos;
            ctx.block_regen = block_regen;
            // Execute the already-decoded sequence right after caller grows `out`.
            let _ = (offset_value, match_len, lit_len);
            return Ok(SeqLoopOutcome::NeedRoom);
        }

        block_regen = next_regen;
        if lit_len > 0 {
            copy_lits_fast(literals, lit_pos, lit_len, buf, cur_out);
            cur_out += lit_len;
        }
        lit_pos = lit_end;

        let available = cur_out.saturating_sub(frame_start);
        let actual_offset = resolve_offset(rep, offset_value, lit_len, available)?;
        let max_off = available.min(win_usize);
        if actual_offset == 0 || actual_offset > max_off {
            return Err(ZstdError::BadOffset { offset: actual_offset, available });
        }

        copy_match_in_slice(buf, cur_out, actual_offset, match_len);
        cur_out += match_len;
        seq_idx += 1;
    }

    *out_pos = cur_out;
    ctx.seq_idx = seq_idx;
    ctx.ll_state = ll_state;
    ctx.of_state = of_state;
    ctx.ml_state = ml_state;
    ctx.lit_pos = lit_pos;
    ctx.block_regen = block_regen;
    Ok(SeqLoopOutcome::Done)
}

fn decode_compressed_block<A: Allocator>(
    block_bytes: &[u8],
    state: &mut FrameState,
    lit_buf: &mut Vec<u8, A>,
    out: &mut Vec<u8, A>,
    out_pos: &mut usize,
    frame_start: usize,
    window_bytes: u64,
    most_output_bytes: usize,
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
        check_output_budget(*out_pos, literals.len(), most_output_bytes)?;
        let needed = *out_pos + literals.len();
        if needed > out.len() {
            let add = needed - out.len();
            grow_zeroed(out, add)?;
        }
        if let Some(dst) = out.as_mut_slice().get_mut(*out_pos..needed) {
            dst.copy_from_slice(literals);
        }
        *out_pos = needed;
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
        &mut state.ll_table,
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
        &mut state.of_table,
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
        &mut state.ml_table,
        SeqKind::Ml,
        &mut state.has_ml,
        &mut state.ml_predef,
        &ML_DEFAULT_NORM,
        ML_DEFAULT_LOG,
        ML_MAX_CODE,
        ML_MAX_LOG,
    )?;

    let mut br = RevBitReader::new(cur.rest()).ok_or(ZstdError::CorruptSequences)?;
    let ll_log = u32::from(state.ll_table.accuracy_log());
    let of_log = u32::from(state.of_table.accuracy_log());
    let ml_log = u32::from(state.ml_table.accuracy_log());

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

    // Ensure enough room for a full 128 KiB block (with 32 bytes slack for wild copies) so
    // `run_sequences_slice` completes in a single zero-allocation pass.
    let block_need = *out_pos + MAX_BLOCK_SIZE.min(most_output_bytes.saturating_sub(*out_pos));
    if out.len() < block_need {
        let add = block_need - out.len();
        grow_zeroed(out, add)?;
    }

    let win_usize = usize::try_from(window_bytes).unwrap_or(usize::MAX);
    let mut ctx = SeqLoopCtx {
        seq_idx: 0,
        ll_state,
        of_state,
        ml_state,
        lit_pos: 0,
        block_regen: 0,
    };

    // Since `out.len() >= *out_pos + min(MAX_BLOCK_SIZE, most_output_bytes - *out_pos)`,
    // any valid block finishes in a single call without ever returning `NeedRoom`.
    match run_sequences_slice(
        out.as_mut_slice(),
        out_pos,
        literals,
        &mut br,
        &state.ll_table,
        &state.of_table,
        &state.ml_table,
        &mut state.rep,
        &mut ctx,
        num_seq,
        frame_start,
        win_usize,
        most_output_bytes,
    )? {
        SeqLoopOutcome::Done => {}
        SeqLoopOutcome::NeedRoom => return Err(ZstdError::OutputTooLarge { limit: most_output_bytes }),
    }

    if !br.is_empty() {
        return Err(ZstdError::CorruptSequences);
    }

    // Copy trailing literals after the final sequence.
    let trailing = literals.get(ctx.lit_pos..).ok_or(ZstdError::CorruptSequences)?;
    if !trailing.is_empty() {
        let next_regen = ctx
            .block_regen
            .checked_add(trailing.len())
            .ok_or(ZstdError::CorruptSequences)?;
        if next_regen > MAX_BLOCK_SIZE {
            return Err(ZstdError::BlockTooLarge { size: next_regen });
        }
        check_output_budget(*out_pos, trailing.len(), most_output_bytes)?;
        let needed = *out_pos + trailing.len();
        if needed > out.len() {
            let add = needed - out.len();
            grow_zeroed(out, add)?;
        }
        if let Some(dst) = out.as_mut_slice().get_mut(*out_pos..needed) {
            dst.copy_from_slice(trailing);
        } else {
            return Err(ZstdError::CorruptSequences);
        }
        *out_pos = needed;
    }

    Ok(())
}

pub(crate) fn decompress_in_inner<A: Allocator + Copy>(
    input: &[u8],
    limits: ZstdLimits,
    alloc: A,
    skip_checksum: bool,
) -> Result<Vec<u8, A>, ZstdError> {
    if input.is_empty() {
        return Err(ZstdError::Truncated);
    }

    let mut cur = Cursor::new(input);
    let mut out = Vec::new_in(alloc);
    let mut lit_buf = Vec::new_in(alloc);
    let mut state = FrameState::new();
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
        if let Some(cs) = hdr.content_size {
            let rem_cap = limits.most_output_bytes.saturating_sub(out.len());
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
        let mut out_pos = frame_start;

        // Pre-stage the entire frame once using bulk NEON zeroing (plus 32B slack for wild copies).
        if let Some(cs) = hdr.content_size {
            let safe_cap = input.len().saturating_mul(16).max(65_536).min(MAX_PREALLOC_HINT);
            let hint = usize::try_from(cs).unwrap_or(safe_cap).min(safe_cap);
            grow_zeroed(&mut out, hint.saturating_add(32))?;
        }

        state.reset_frame();

        loop {
            let bh = cur.read_slice(3)?;
            let [b0, b1, b2] = *bh else {
                return Err(ZstdError::Truncated);
            };
            let hdr_u32 = u32::from_le_bytes([b0, b1, b2, 0]);
            let last_block = (hdr_u32 & 1) != 0;
            let block_type = (hdr_u32 >> 1) & 0x03;
            let block_size = usize::try_from(hdr_u32 >> 3).map_err(|_| ZstdError::Truncated)?;

            match block_type {
                0 => {
                    // Raw_Block
                    if block_size > MAX_BLOCK_SIZE {
                        return Err(ZstdError::BlockTooLarge { size: block_size });
                    }
                    check_output_budget(out_pos, block_size, limits.most_output_bytes)?;
                    let raw = cur.read_slice(block_size)?;
                    let needed = out_pos + block_size;
                    if needed > out.len() {
                        let add = needed - out.len();
                        grow_zeroed(&mut out, add)?;
                    }
                    if let Some(dst) = out.as_mut_slice().get_mut(out_pos..needed) {
                        dst.copy_from_slice(raw);
                    }
                    out_pos = needed;
                }
                1 => {
                    // RLE_Block
                    if block_size > MAX_BLOCK_SIZE {
                        return Err(ZstdError::BlockTooLarge { size: block_size });
                    }
                    check_output_budget(out_pos, block_size, limits.most_output_bytes)?;
                    let val = cur.read_u8()?;
                    let needed = out_pos + block_size;
                    if needed > out.len() {
                        let add = needed - out.len();
                        grow_zeroed(&mut out, add)?;
                    }
                    if let Some(dst) = out.as_mut_slice().get_mut(out_pos..needed) {
                        dst.fill(val);
                    }
                    out_pos = needed;
                }
                2 => {
                    // Compressed_Block
                    if block_size > MAX_BLOCK_SIZE {
                        return Err(ZstdError::BlockTooLarge { size: block_size });
                    }
                    let block_bytes = cur.read_slice(block_size)?;
                    decode_compressed_block(
                        block_bytes,
                        &mut state,
                        &mut lit_buf,
                        &mut out,
                        &mut out_pos,
                        frame_start,
                        hdr.window_bytes,
                        limits.most_output_bytes,
                    )?;
                }
                _ => return Err(ZstdError::BadBlockType),
            }

            if last_block {
                break;
            }
        }

        out.truncate(out_pos);

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

    Ok(out)
}
