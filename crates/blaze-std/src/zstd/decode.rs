//! RFC 8878 Zstandard frame, block, literals, and sequence decoder.

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::digest::xxh64;
use crate::zstd::fse::{
    FseDecTable, LL_DEFAULT_LOG, LL_DEFAULT_NORM, LL_MAX_CODE, LL_MAX_LOG, ML_DEFAULT_LOG,
    ML_DEFAULT_NORM, ML_MAX_CODE, ML_MAX_LOG, OF_DEFAULT_LOG, OF_DEFAULT_NORM, OF_MAX_CODE,
    OF_MAX_LOG, RevBitReader, ll_base_and_bits, ml_base_and_bits,
};
use crate::zstd::huffman::HuffTable;
use crate::zstd::{
    MAX_BLOCK_SIZE, SKIPPABLE_MAGIC_MAX, SKIPPABLE_MAGIC_MIN, ZSTD_MAGIC, ZstdError, ZstdLimits,
};

const MAX_PREALLOC_HINT: usize = 1 << 20;

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
    ll_table: FseDecTable,
    of_table: FseDecTable,
    ml_table: FseDecTable,
    has_huff: bool,
    has_ll: bool,
    has_of: bool,
    has_ml: bool,
    rep: [usize; 3],
}

impl FrameState {
    const fn new() -> Self {
        Self {
            huff: HuffTable::new_empty(),
            ll_table: FseDecTable::new_empty(),
            of_table: FseDecTable::new_empty(),
            ml_table: FseDecTable::new_empty(),
            has_huff: false,
            has_ll: false,
            has_of: false,
            has_ml: false,
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
    Buffered,
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
                lit_buf.clear();
                lit_buf.try_reserve(regen_size)?;
                for _ in 0..regen_size {
                    lit_buf.try_push(val).map_err(|r| r.why)?;
                }
                Ok(LiteralsRef::Buffered)
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

            lit_buf.clear();
            if num_streams == 1 {
                state.huff.decode_1_stream(stream_data, regen_size, lit_buf)?;
            } else {
                state.huff.decode_4_streams(stream_data, regen_size, lit_buf)?;
            }
            Ok(LiteralsRef::Buffered)
        }
    }
}

fn load_seq_table(
    cur: &mut Cursor<'_>,
    mode: u8,
    table: &mut FseDecTable,
    has_table: &mut bool,
    default_norm: &[i16],
    default_log: u8,
    max_sym: usize,
    max_log: u8,
) -> Result<(), ZstdError> {
    match mode {
        0 => {
            // Predefined_Mode
            table.build_from_norm(default_norm, default_log)?;
            *has_table = true;
            Ok(())
        }
        1 => {
            // RLE_Mode
            let sym = cur.read_u8().map_err(|_| ZstdError::CorruptFse)?;
            if usize::from(sym) > max_sym {
                return Err(ZstdError::CorruptFse);
            }
            table.set_rle(sym);
            *has_table = true;
            Ok(())
        }
        2 => {
            // FSE_Compressed_Mode
            let used = table.decode_header_and_build(cur.rest(), max_sym, max_log)?;
            cur.skip(used).map_err(|_| ZstdError::CorruptFse)?;
            *has_table = true;
            Ok(())
        }
        _ => {
            // Repeat_Mode (3)
            if !*has_table {
                return Err(ZstdError::CorruptFse);
            }
            Ok(())
        }
    }
}

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

fn decode_compressed_block<A: Allocator>(
    block_bytes: &[u8],
    state: &mut FrameState,
    lit_buf: &mut Vec<u8, A>,
    out: &mut Vec<u8, A>,
    frame_start: usize,
    window_bytes: u64,
    most_output_bytes: usize,
) -> Result<(), ZstdError> {
    let mut cur = Cursor::new(block_bytes);
    let lit_ref = decode_literals(&mut cur, state, lit_buf)?;
    let literals: &[u8] = match lit_ref {
        LiteralsRef::Borrowed(slice) => slice,
        LiteralsRef::Buffered => lit_buf.as_slice(),
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
        check_output_budget(out.len(), literals.len(), most_output_bytes)?;
        out.try_extend_from_slice(literals)?;
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
        &mut state.ll_table,
        &mut state.has_ll,
        &LL_DEFAULT_NORM,
        LL_DEFAULT_LOG,
        LL_MAX_CODE,
        LL_MAX_LOG,
    )?;
    load_seq_table(
        &mut cur,
        of_mode,
        &mut state.of_table,
        &mut state.has_of,
        &OF_DEFAULT_NORM,
        OF_DEFAULT_LOG,
        OF_MAX_CODE,
        OF_MAX_LOG,
    )?;
    load_seq_table(
        &mut cur,
        ml_mode,
        &mut state.ml_table,
        &mut state.has_ml,
        &ML_DEFAULT_NORM,
        ML_DEFAULT_LOG,
        ML_MAX_CODE,
        ML_MAX_LOG,
    )?;

    let mut br = RevBitReader::new(cur.rest()).ok_or(ZstdError::CorruptSequences)?;
    let mut ll_state = usize::try_from(
        br.read_bits(u32::from(state.ll_table.accuracy_log()))
            .ok_or(ZstdError::CorruptSequences)?,
    )
    .map_err(|_| ZstdError::CorruptSequences)?;
    let mut of_state = usize::try_from(
        br.read_bits(u32::from(state.of_table.accuracy_log()))
            .ok_or(ZstdError::CorruptSequences)?,
    )
    .map_err(|_| ZstdError::CorruptSequences)?;
    let mut ml_state = usize::try_from(
        br.read_bits(u32::from(state.ml_table.accuracy_log()))
            .ok_or(ZstdError::CorruptSequences)?,
    )
    .map_err(|_| ZstdError::CorruptSequences)?;

    let mut lit_pos: usize = 0;
    let mut block_regen: usize = 0;

    for seq_idx in 0..num_seq {
        let ll_entry = state.ll_table.entry(ll_state).ok_or(ZstdError::CorruptSequences)?;
        let of_entry = state.of_table.entry(of_state).ok_or(ZstdError::CorruptSequences)?;
        let ml_entry = state.ml_table.entry(ml_state).ok_or(ZstdError::CorruptSequences)?;

        let of_code = of_entry.symbol;
        if usize::from(of_code) > OF_MAX_CODE {
            return Err(ZstdError::CorruptSequences);
        }
        let (ll_base, ll_bits) =
            ll_base_and_bits(ll_entry.symbol).ok_or(ZstdError::CorruptSequences)?;
        let (ml_base, ml_bits) =
            ml_base_and_bits(ml_entry.symbol).ok_or(ZstdError::CorruptSequences)?;

        // RFC 8878 §3.1.1.3.2.2 extra bits order: OF, then ML, then LL.
        let of_extra = br.read_bits(u32::from(of_code)).ok_or(ZstdError::CorruptSequences)?;
        let ml_extra = br.read_bits(u32::from(ml_bits)).ok_or(ZstdError::CorruptSequences)?;
        let ll_extra = br.read_bits(u32::from(ll_bits)).ok_or(ZstdError::CorruptSequences)?;

        let offset_value = (1u32 << u32::from(of_code))
            .checked_add(of_extra)
            .ok_or(ZstdError::CorruptSequences)?;
        let match_len = ml_base
            .checked_add(usize::try_from(ml_extra).map_err(|_| ZstdError::CorruptSequences)?)
            .ok_or(ZstdError::CorruptSequences)?;
        let lit_len = ll_base
            .checked_add(usize::try_from(ll_extra).map_err(|_| ZstdError::CorruptSequences)?)
            .ok_or(ZstdError::CorruptSequences)?;

        // RFC 8878 §3.1.1.3.2.2 state update order: LL, then ML, then OF.
        if seq_idx + 1 < num_seq {
            let ll_add = usize::try_from(
                br.read_bits(u32::from(ll_entry.num_bits)).ok_or(ZstdError::CorruptSequences)?,
            )
            .map_err(|_| ZstdError::CorruptSequences)?;
            let ml_add = usize::try_from(
                br.read_bits(u32::from(ml_entry.num_bits)).ok_or(ZstdError::CorruptSequences)?,
            )
            .map_err(|_| ZstdError::CorruptSequences)?;
            let of_add = usize::try_from(
                br.read_bits(u32::from(of_entry.num_bits)).ok_or(ZstdError::CorruptSequences)?,
            )
            .map_err(|_| ZstdError::CorruptSequences)?;

            ll_state = usize::from(ll_entry.baseline) + ll_add;
            ml_state = usize::from(ml_entry.baseline) + ml_add;
            of_state = usize::from(of_entry.baseline) + of_add;
        }

        // Copy literals for this sequence.
        let lit_end = lit_pos.checked_add(lit_len).ok_or(ZstdError::CorruptSequences)?;
        let lit_slice = literals.get(lit_pos..lit_end).ok_or(ZstdError::CorruptSequences)?;
        lit_pos = lit_end;

        let seq_out = lit_len.checked_add(match_len).ok_or(ZstdError::CorruptSequences)?;
        block_regen = block_regen.checked_add(seq_out).ok_or(ZstdError::CorruptSequences)?;
        if block_regen > MAX_BLOCK_SIZE {
            return Err(ZstdError::BlockTooLarge { size: block_regen });
        }
        check_output_budget(out.len(), seq_out, most_output_bytes)?;

        out.try_extend_from_slice(lit_slice)?;

        // Resolve offset and copy match (supporting overlapping RLE matches).
        let available = out.len().saturating_sub(frame_start);
        let actual_offset = resolve_offset(&mut state.rep, offset_value, lit_len, available)?;
        let max_off = usize::try_from(window_bytes).map_or(available, |w| available.min(w));
        if actual_offset == 0 || actual_offset > max_off {
            return Err(ZstdError::BadOffset { offset: actual_offset, available });
        }

        out.try_reserve(match_len)?;
        if actual_offset == 1 {
            let &byte = out.last().ok_or(ZstdError::CorruptSequences)?;
            for _ in 0..match_len {
                out.try_push(byte).map_err(|r| r.why)?;
            }
        } else {
            let mut src_idx = out.len().checked_sub(actual_offset).ok_or(ZstdError::BadOffset {
                offset: actual_offset,
                available,
            })?;
            for _ in 0..match_len {
                let &byte = out.get(src_idx).ok_or(ZstdError::CorruptSequences)?;
                out.try_push(byte).map_err(|r| r.why)?;
                src_idx += 1;
            }
        }
    }

    if !br.is_empty() {
        return Err(ZstdError::CorruptSequences);
    }

    // Copy trailing literals after the final sequence.
    let trailing = literals.get(lit_pos..).ok_or(ZstdError::CorruptSequences)?;
    if !trailing.is_empty() {
        block_regen = block_regen
            .checked_add(trailing.len())
            .ok_or(ZstdError::CorruptSequences)?;
        if block_regen > MAX_BLOCK_SIZE {
            return Err(ZstdError::BlockTooLarge { size: block_regen });
        }
        check_output_budget(out.len(), trailing.len(), most_output_bytes)?;
        out.try_extend_from_slice(trailing)?;
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

        if let Some(cs) = hdr.content_size {
            let hint = usize::try_from(cs).unwrap_or(MAX_PREALLOC_HINT).min(MAX_PREALLOC_HINT);
            out.try_reserve(hint)?;
        }

        let frame_start = out.len();
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
                    check_output_budget(out.len(), block_size, limits.most_output_bytes)?;
                    let raw = cur.read_slice(block_size)?;
                    out.try_extend_from_slice(raw)?;
                }
                1 => {
                    // RLE_Block
                    if block_size > MAX_BLOCK_SIZE {
                        return Err(ZstdError::BlockTooLarge { size: block_size });
                    }
                    check_output_budget(out.len(), block_size, limits.most_output_bytes)?;
                    let val = cur.read_u8()?;
                    out.try_reserve(block_size)?;
                    for _ in 0..block_size {
                        out.try_push(val).map_err(|r| r.why)?;
                    }
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
