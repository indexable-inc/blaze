//! RFC 8878 Zstandard frame and block compressor: reusable [`ZstdEncoder`] context,
//! 10-bit-tagged `hash7` Fast (`16K` = 64 KiB table, `minMatch = 7`, 1 MiB window) and
//! 10-bit-tagged Double-Fast `hash8` + `hash5` (`64K` + `32K` = 384 KiB tables, 2 MiB window)
//! LZ77 scanners, Treeless-reusing 4-stream Huffman literals, custom/RLE/predefined FSE sequence
//! tables, and zero-bounds-check reverse FSE bit-packing.

use core::alloc::Allocator;

use blaze_own::{
    FseSeqEncTables, Int, LZ77_DEF_BUCKETS, LZ77_FAST_HASH_SIZE, SeqOp, Vec,
    count_seq_symbols_fast, fse_pack_sequences_fast, lz77_scan_default, lz77_scan_fast,
};

use crate::dict::ZstdDict;
use crate::fse::{
    FseEncTable, LL_DEFAULT_LOG, LL_DEFAULT_NORM, LL_ENC_PACKED, LL_MAX_CODE, ML_DEFAULT_LOG,
    ML_DEFAULT_NORM, ML_ENC_PACKED, ML_MAX_CODE, OF_DEFAULT_LOG, OF_DEFAULT_NORM, encode_ll_fast,
    encode_ml_fast, encode_of_fast, normalize_fse_counts, write_fse_header,
};
use crate::huffman::{HuffEncCache, encode_huffman_literals_into};
use crate::xxh64::xxh64;
use crate::{CompressOpts, Level, MAX_BLOCK_SIZE, ZSTD_MAGIC, ZstdError};

/// 1 MiB sliding window for `Level::Fast` (matches C `zstd -1`, `wlog = 20`).
const MAX_WINDOW_FAST: usize = 1 << 20;
/// Window_Descriptor for exact 1 MiB window: exponent = 10, mantissa = 0 (`1 << (10 + 10)`).
const WINDOW_DESC_1M: u8 = 10u8 << 3;
/// 2 MiB sliding window for `Level::Default` (matches C `zstd -3`, `wlog = 21`).
const MAX_WINDOW_DEFAULT: usize = 1 << 21;
/// Window_Descriptor for exact 2 MiB window: exponent = 11, mantissa = 0 (`1 << (10 + 11)`).
const WINDOW_DESC_2M: u8 = 11u8 << 3;

#[inline]
fn ensure_zeroed<T: Copy, A: Allocator>(
    table: &mut Vec<T, A>,
    len: usize,
    zero: T,
) -> Result<(), ZstdError> {
    if table.len() >= len {
        if let Some(s) = table.as_mut_slice().get_mut(..len) {
            s.fill(zero);
        }
        return Ok(());
    }
    table.clear();
    table.try_reserve(len)?;
    let chunk = [zero; 128];
    let mut rem = len;
    while rem >= chunk.len() {
        table.try_extend_from_slice(&chunk)?;
        rem -= chunk.len();
    }
    if rem > 0
        && let Some(tail) = chunk.get(..rem)
    {
        table.try_extend_from_slice(tail)?;
    }
    Ok(())
}

#[inline]
fn write_block_header<A: Allocator>(
    out: &mut Vec<u8, A>,
    last_block: bool,
    block_type: u32,
    block_size: usize,
) -> Result<(), ZstdError> {
    let size_u32 = u32::try_from(block_size).map_err(|_| ZstdError::BlockTooLarge {
        size: block_size,
    })?;
    let last_bit: u32 = if last_block { 1 } else { 0 };
    let hdr = (size_u32 << 3) | ((block_type & 0x03) << 1) | last_bit;
    let [b0, b1, b2, _] = hdr.to_le_bytes();
    out.try_extend_from_slice(&[b0, b1, b2])?;
    Ok(())
}

#[inline]
fn write_raw_literals_header<A: Allocator>(
    out: &mut Vec<u8, A>,
    lit_len: usize,
) -> Result<(), ZstdError> {
    if lit_len < 32 {
        out.try_push(lit_len.truncate_to::<u8>() << 3).map_err(|r| r.why)?;
    } else if lit_len < 4_096 {
        let v = (lit_len.truncate_to::<u16>() << 4) | 0b0100;
        out.try_extend_from_slice(&v.to_le_bytes())?;
    } else {
        let len_u32 = u32::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
        let v = (len_u32 << 4) | 0b1100;
        let [b0, b1, b2, _] = v.to_le_bytes();
        out.try_extend_from_slice(&[b0, b1, b2])?;
    }
    Ok(())
}

#[inline]
fn write_num_sequences<A: Allocator>(out: &mut Vec<u8, A>, n: usize) -> Result<(), ZstdError> {
    if n < 128 {
        out.try_push(n.truncate_to::<u8>()).map_err(|r| r.why)?;
    } else if n < 0x7F00 {
        let hi = ((n >> 8) + 128).truncate_to::<u8>();
        let lo = n.truncate_to::<u8>();
        out.try_extend_from_slice(&[hi, lo])?;
    } else {
        let adj = n.checked_sub(0x7F00).ok_or(ZstdError::CorruptSequences)?;
        let adj_u16 = u16::try_from(adj).map_err(|_| ZstdError::CorruptSequences)?;
        let [lo, hi] = adj_u16.to_le_bytes();
        out.try_extend_from_slice(&[255u8, lo, hi])?;
    }
    Ok(())
}

/// 8-byte SWAR check whether every byte in `bytes` equals `bytes[0]`.
#[inline]
fn is_all_same(bytes: &[u8]) -> Option<u8> {
    let &first = bytes.first()?;
    let pattern = u64::from_ne_bytes([first; 8]);
    let (chunks, rem) = bytes.as_chunks::<8>();
    for &arr in chunks {
        if u64::from_ne_bytes(arr) != pattern {
            return None;
        }
    }
    for &b in rem {
        if b != first {
            return None;
        }
    }
    Some(first)
}

struct PredefinedEncTables {
    ll: FseEncTable,
    of: FseEncTable,
    ml: FseEncTable,
    packed: FseSeqEncTables,
}

impl PredefinedEncTables {
    fn build() -> Result<Self, ZstdError> {
        let mut ll = FseEncTable::new_empty();
        ll.rebuild_from_norm(&LL_DEFAULT_NORM, LL_DEFAULT_LOG)?;

        let mut of = FseEncTable::new_empty();
        of.rebuild_from_norm(&OF_DEFAULT_NORM, OF_DEFAULT_LOG)?;

        let mut ml = FseEncTable::new_empty();
        ml.rebuild_from_norm(&ML_DEFAULT_NORM, ML_DEFAULT_LOG)?;

        let mut packed = FseSeqEncTables::new_zeroed();
        ll.write_ll_into(&mut packed);
        of.write_of_into(&mut packed);
        ml.write_ml_into(&mut packed);

        Ok(Self { ll, of, ml, packed })
    }
}

/// Single-pass reverse FSE sequence packer directly from `SeqOp` via `blaze_own`'s zero-bounds-check
/// kernel (used by both `Level::Fast` and `Level::Default`).
#[inline]
fn pack_raw_sequences_fse<A: Allocator>(
    raw_seqs: &[SeqOp],
    ll_tbl: &FseEncTable,
    of_tbl: &FseEncTable,
    ml_tbl: &FseEncTable,
    packed: &FseSeqEncTables,
    comp_buf: &mut Vec<u8, A>,
) -> Result<(), ZstdError> {
    let last = raw_seqs.last().ok_or(ZstdError::CorruptSequences)?;
    let (l_ll_c, l_ll_e, l_ll_b) =
        encode_ll_fast(last.lit_len).ok_or(ZstdError::CorruptSequences)?;
    let (l_ml_c, l_ml_e, l_ml_b) =
        encode_ml_fast(last.match_len).ok_or(ZstdError::CorruptSequences)?;
    let (l_of_c, l_of_e, l_of_b) =
        encode_of_fast(last.offset).ok_or(ZstdError::CorruptSequences)?;

    let init_ll = ll_tbl.init_state_biased(l_ll_c).ok_or(ZstdError::CorruptFse)?;
    let init_of = of_tbl.init_state_biased(l_of_c).ok_or(ZstdError::CorruptFse)?;
    let init_ml = ml_tbl.init_state_biased(l_ml_c).ok_or(ZstdError::CorruptFse)?;

    fse_pack_sequences_fast(
        raw_seqs,
        &LL_ENC_PACKED,
        &ML_ENC_PACKED,
        packed,
        init_ll,
        init_of,
        init_ml,
        l_ll_e,
        l_ll_b,
        l_ml_e,
        l_ml_b,
        l_of_e,
        l_of_b,
        comp_buf,
    )?;
    Ok(())
}

/// Choose between `Predefined_Mode (0)`, `RLE_Mode (1)`, `FSE_Compressed_Mode (2)`, or
/// `Repeat_Mode (3)` for one sequence code stream (`LL`, `OF`, or `ML`).
enum SeqTableChoice {
    Predefined,
    Rle(u8),
    Custom { norm: [i16; 64], max_sym: usize, log: u8 },
    Repeat,
}

#[inline]
fn symbol_mask(counts: &[u32]) -> u64 {
    let mut mask: u64 = 0;
    for (sym, &c) in counts.iter().take(64).enumerate() {
        if c > 0 {
            mask |= 1u64 << sym;
        }
    }
    mask
}

fn plan_seq_table(
    counts: &[u32],
    cur_mask: u64,
    total: u32,
    target_log: u8,
    custom_enc: &mut FseEncTable,
) -> SeqTableChoice {
    if cur_mask.count_ones() == 1 {
        let only_sym = cur_mask.trailing_zeros().truncate_to::<u8>();
        if custom_enc.set_rle(only_sym).is_ok() {
            return SeqTableChoice::Rle(only_sym);
        }
        return SeqTableChoice::Predefined;
    }
    let mut norm = [0i16; 64];
    if let Some(max_sym) = normalize_fse_counts(counts, total, target_log, &mut norm)
        && let Some(norm_slice) = norm.get(..=max_sym)
        && custom_enc.rebuild_from_norm(norm_slice, target_log).is_ok()
    {
        return SeqTableChoice::Custom { norm, max_sym, log: target_log };
    }
    SeqTableChoice::Predefined
}

/// Bitmasks of symbols present in the last committed custom/RLE FSE table for each sequence stream.
/// A zero mask indicates no reusable custom/RLE table is currently committed for that stream.
#[derive(Copy, Clone)]
struct SeqMasks {
    ll: u64,
    of: u64,
    ml: u64,
}

impl SeqMasks {
    const EMPTY: Self = Self { ll: 0, of: 0, ml: 0 };
}

/// Per-block entropy and scratch state split out of [`ZstdEncoder`] so the LZ77 hash-table array
/// borrow can be validated once per frame before the block loop without conflicting with
/// [`BlockEncState::encode_compressed_block`].
struct BlockEncState<A: Allocator + Copy> {
    predef: PredefinedEncTables,
    custom_ll: FseEncTable,
    custom_of: FseEncTable,
    custom_ml: FseEncTable,
    packed_active: FseSeqEncTables,
    committed_fse: SeqMasks,
    literals_buf: Vec<u8, A>,
    raw_seqs: Vec<SeqOp, A>,
    comp_buf: Vec<u8, A>,
    huff_payload: Vec<u8, A>,
    huff_cache: HuffEncCache,
    alloc: A,
}

enum ActiveTable<'a> {
    Fast(&'a mut [u32; LZ77_FAST_HASH_SIZE]),
    Default(&'a mut [u64; LZ77_DEF_BUCKETS]),
}

#[derive(Copy, Clone)]
enum BlockFseMode {
    Predefined { reuse: bool },
    Custom { fast_mode: bool, allow_repeat: bool },
}

/// Reusable RFC 8878 Zstandard compressor context.
///
/// Reuses prebuilt FSE tables, LZ77 hash table, Huffman table cache (for `Treeless_Literals_Block`),
/// and block scratch buffers across calls with zero per-call workspace allocation.
pub struct ZstdEncoder<A: Allocator + Copy> {
    block: BlockEncState<A>,
    hash_table: Vec<u32, A>,
    hash_table_dfast: Vec<u64, A>,
    dict_work: Vec<u8, A>,
    dict_tag: (usize, usize, u64),
}

impl<A: Allocator + Copy> ZstdEncoder<A> {
    /// Create a reusable Zstandard compressor context backed by `alloc`.
    pub fn try_new_in(alloc: A) -> Result<Self, ZstdError> {
        Ok(Self {
            block: BlockEncState {
                predef: PredefinedEncTables::build()?,
                custom_ll: FseEncTable::new_empty(),
                custom_of: FseEncTable::new_empty(),
                custom_ml: FseEncTable::new_empty(),
                packed_active: FseSeqEncTables::new_zeroed(),
                committed_fse: SeqMasks::EMPTY,
                literals_buf: Vec::new_in(alloc),
                raw_seqs: Vec::new_in(alloc),
                comp_buf: Vec::new_in(alloc),
                huff_payload: Vec::new_in(alloc),
                huff_cache: HuffEncCache::new(),
                alloc,
            },
            hash_table: Vec::new_in(alloc),
            hash_table_dfast: Vec::new_in(alloc),
            dict_work: Vec::new_in(alloc),
            dict_tag: (0, 0, 0),
        })
    }

    /// Compress `input` into a freshly allocated RFC 8878 Zstandard frame in this encoder's
    /// allocator.
    pub fn compress(&mut self, input: &[u8], opts: CompressOpts) -> Result<Vec<u8, A>, ZstdError> {
        let init_cap = (input.len() / 2).max(256).min(input.len().saturating_add(64));
        let mut out = Vec::try_with_capacity_in(init_cap, self.block.alloc)?;
        self.compress_into(input, opts, &mut out)?;
        Ok(out)
    }

    /// Compress `input` with pre-hashed raw-content dictionary `dict` into a freshly allocated
    /// RFC 8878 Zstandard frame in this encoder's allocator.
    pub fn compress_with_dict<D: Allocator + Copy>(
        &mut self,
        input: &[u8],
        dict: &ZstdDict<D>,
        opts: CompressOpts,
    ) -> Result<Vec<u8, A>, ZstdError> {
        let init_cap = (input.len() / 2).max(256).min(input.len().saturating_add(64));
        let mut out = Vec::try_with_capacity_in(init_cap, self.block.alloc)?;
        self.compress_with_dict_into(input, dict, opts, &mut out)?;
        Ok(out)
    }

    /// Compress `input`, appending a complete RFC 8878 Zstandard frame into `out`.
    pub fn compress_into(
        &mut self,
        input: &[u8],
        opts: CompressOpts,
        out: &mut Vec<u8, A>,
    ) -> Result<(), ZstdError> {
        self.compress_inner(input, None, opts, out)
    }

    /// Compress `input` using pre-hashed raw-content dictionary `dict`, appending a complete
    /// RFC 8878 Zstandard frame into `out`.
    pub fn compress_with_dict_into<D: Allocator + Copy>(
        &mut self,
        input: &[u8],
        dict: &ZstdDict<D>,
        opts: CompressOpts,
        out: &mut Vec<u8, A>,
    ) -> Result<(), ZstdError> {
        if dict.bytes().is_empty() {
            return self.compress_inner(input, None, opts, out);
        }
        self.compress_inner(
            input,
            Some((dict.bytes(), dict.fast_table(), dict.default_table(), dict.hash())),
            opts,
            out,
        )
    }

    fn compress_inner(
        &mut self,
        input: &[u8],
        dict: Option<(&[u8], &[u32], &[u64], u64)>,
        opts: CompressOpts,
        out: &mut Vec<u8, A>,
    ) -> Result<(), ZstdError> {
        // 1. Frame magic
        out.try_extend_from_slice(&ZSTD_MAGIC.to_le_bytes())?;

        // 2. Frame header descriptor + optional Window_Descriptor + Frame_Content_Size
        let fast_mode = matches!(opts.level, Level::Fast);
        let has_dict = dict.is_some();
        let len_u64 = input.len().widen_to::<u64>();
        let single_segment = !has_dict && input.len() <= MAX_BLOCK_SIZE;
        let fcs_flag: u8 = if single_segment && len_u64 <= 255 {
            0
        } else if (256..=65_791).contains(&len_u64) {
            1
        } else if len_u64 <= u64::from(u32::MAX) {
            2
        } else {
            3
        };

        let mut fhd: u8 = fcs_flag << 6;
        if single_segment {
            fhd |= 0x20;
        }
        if opts.checksum {
            fhd |= 0x04;
        }
        out.try_push(fhd).map_err(|r| r.why)?;

        if !single_segment {
            let window_desc = if fast_mode { WINDOW_DESC_1M } else { WINDOW_DESC_2M };
            out.try_push(window_desc).map_err(|r| r.why)?;
        }

        match fcs_flag {
            0 => {
                out.try_push(len_u64.truncate_to::<u8>()).map_err(|r| r.why)?;
            }
            1 => {
                out.try_extend_from_slice(&(len_u64 - 256).truncate_to::<u16>().to_le_bytes())?;
            }
            2 => {
                out.try_extend_from_slice(&len_u64.truncate_to::<u32>().to_le_bytes())?;
            }
            _ => {
                out.try_extend_from_slice(&len_u64.to_le_bytes())?;
            }
        }

        // 3. Blocks
        if input.is_empty() {
            write_block_header(out, true, 0, 0)?;
        } else {
            let dict_len = if let Some((db, ft, dt, dh)) = dict {
                let tag = (db.as_ptr().addr(), db.len(), dh);
                if self.dict_tag != tag || self.dict_work.len() < db.len() {
                    self.dict_work.clear();
                    self.dict_work.try_reserve(db.len().saturating_add(input.len()))?;
                    self.dict_work.try_extend_from_slice(db)?;
                    self.dict_tag = tag;
                } else {
                    self.dict_work.truncate(db.len());
                    self.dict_work.try_reserve(input.len())?;
                }
                self.dict_work.try_extend_from_slice(input)?;
                if fast_mode {
                    if self.hash_table.len() != LZ77_FAST_HASH_SIZE {
                        self.hash_table.clear();
                        self.hash_table.try_extend_from_slice(ft)?;
                    } else {
                        self.hash_table.as_mut_slice().copy_from_slice(ft);
                    }
                } else if self.hash_table_dfast.len() != LZ77_DEF_BUCKETS {
                    self.hash_table_dfast.clear();
                    self.hash_table_dfast.try_extend_from_slice(dt)?;
                } else {
                    self.hash_table_dfast.as_mut_slice().copy_from_slice(dt);
                }
                db.len()
            } else {
                if fast_mode {
                    ensure_zeroed(&mut self.hash_table, LZ77_FAST_HASH_SIZE, 0u32)?;
                } else {
                    ensure_zeroed(&mut self.hash_table_dfast, LZ77_DEF_BUCKETS, 0u64)?;
                }
                0
            };
            self.block.huff_cache.reset();
            self.block.committed_fse = SeqMasks::EMPTY;

            // Hoist the frame scan slice and fixed-length hash-table array borrow once per frame
            // so the per-block loop pays zero slice/array `Option`/`Result` checks.
            let scan_input: &[u8] = if dict_len > 0 { self.dict_work.as_slice() } else { input };
            let mut active_table = if fast_mode {
                let table_arr: &mut [u32; LZ77_FAST_HASH_SIZE] = self
                    .hash_table
                    .as_mut_slice()
                    .get_mut(..LZ77_FAST_HASH_SIZE)
                    .and_then(|s| s.try_into().ok())
                    .ok_or(ZstdError::CorruptSequences)?;
                ActiveTable::Fast(table_arr)
            } else {
                let table_arr: &mut [u64; LZ77_DEF_BUCKETS] = self
                    .hash_table_dfast
                    .as_mut_slice()
                    .get_mut(..LZ77_DEF_BUCKETS)
                    .and_then(|s| s.try_into().ok())
                    .ok_or(ZstdError::CorruptSequences)?;
                ActiveTable::Default(table_arr)
            };

            let mut rep = [1usize, 4usize, 8usize];
            let mut emitted_predef_fse = false;
            let mut block_idx: usize = 0;
            let scan_end = dict_len.saturating_add(input.len());
            let mut block_start: usize = dict_len;

            while block_start < scan_end {
                let allow_repeat = if fast_mode {
                    (block_idx & 7) != 0
                } else {
                    (block_idx & 1) != 0
                };
                block_idx = block_idx.wrapping_add(1);
                let block_end = block_start.saturating_add(MAX_BLOCK_SIZE).min(scan_end);
                let last_block = block_end == scan_end;
                let block_len = block_end.saturating_sub(block_start);
                let input_block = input
                    .get(block_start.saturating_sub(dict_len)..block_end.saturating_sub(dict_len))
                    .ok_or(ZstdError::CorruptLiterals)?;

                if input_block.len() >= 4
                    && let Some(byte) = is_all_same(input_block)
                {
                    write_block_header(out, last_block, 1, block_len)?;
                    out.try_push(byte).map_err(|r| r.why)?;
                    block_start = block_end;
                    continue;
                }

                let mut trial_rep = rep;
                match &mut active_table {
                    ActiveTable::Fast(table_arr) => {
                        lz77_scan_fast(
                            scan_input,
                            block_start,
                            block_end,
                            MAX_WINDOW_FAST,
                            *table_arr,
                            &mut trial_rep,
                            &mut self.block.literals_buf,
                            &mut self.block.raw_seqs,
                        )?;
                    }
                    ActiveTable::Default(table_arr) => {
                        lz77_scan_default(
                            scan_input,
                            block_start,
                            block_end,
                            MAX_WINDOW_DEFAULT,
                            *table_arr,
                            &mut trial_rep,
                            &mut self.block.literals_buf,
                            &mut self.block.raw_seqs,
                        )?;
                    }
                }

                if !self.block.raw_seqs.is_empty() {
                    let fse_mode = if self.block.raw_seqs.len() >= 128 {
                        BlockFseMode::Custom { fast_mode, allow_repeat }
                    } else {
                        BlockFseMode::Predefined { reuse: emitted_predef_fse }
                    };
                    let next_fse = self.block.encode_compressed_block(fse_mode)?;
                    if self.block.comp_buf.len() < block_len {
                        write_block_header(out, last_block, 2, self.block.comp_buf.len())?;
                        out.try_extend_from_slice(self.block.comp_buf.as_slice())?;
                        rep = trial_rep;
                        self.block.huff_cache.commit();
                        self.block.committed_fse = next_fse;
                        emitted_predef_fse = matches!(fse_mode, BlockFseMode::Predefined { .. });
                        block_start = block_end;
                        continue;
                    }
                    self.block.huff_cache.discard();
                    self.block.committed_fse = SeqMasks::EMPTY;
                }

                write_block_header(out, last_block, 0, input_block.len())?;
                out.try_extend_from_slice(input_block)?;
                block_start = block_end;
            }
        }

        // 4. Optional trailing 32-bit XXH64 content checksum
        if opts.checksum {
            let hash = xxh64(input, 0);
            out.try_extend_from_slice(&hash.truncate_to::<u32>().to_le_bytes())?;
        }

        Ok(())
    }
}

impl<A: Allocator + Copy> BlockEncState<A> {
    fn encode_compressed_block(&mut self, fse_mode: BlockFseMode) -> Result<SeqMasks, ZstdError> {
        let (fast_mode, allow_repeat) = match fse_mode {
            BlockFseMode::Custom { fast_mode, allow_repeat } => (fast_mode, allow_repeat),
            BlockFseMode::Predefined { .. } => (false, false),
        };
        self.comp_buf.clear();
        if !encode_huffman_literals_into(
            self.literals_buf.as_slice(),
            fast_mode,
            &mut self.huff_cache,
            &mut self.huff_payload,
            &mut self.comp_buf,
            self.alloc,
        )? {
            write_raw_literals_header(&mut self.comp_buf, self.literals_buf.len())?;
            self.comp_buf.try_extend_from_slice(self.literals_buf.as_slice())?;
        }

        match fse_mode {
            BlockFseMode::Predefined { reuse } => {
                write_num_sequences(&mut self.comp_buf, self.raw_seqs.len())?;
                let mode_byte: u8 = if reuse { 0xFC } else { 0x00 };
                self.comp_buf.try_push(mode_byte).map_err(|r| r.why)?;
                pack_raw_sequences_fse(
                    self.raw_seqs.as_slice(),
                    &self.predef.ll,
                    &self.predef.of,
                    &self.predef.ml,
                    &self.predef.packed,
                    &mut self.comp_buf,
                )?;
                return Ok(SeqMasks::EMPTY);
            }
            BlockFseMode::Custom { .. } => {}
        }

        let mut ll_counts = [0u32; 64];
        let mut of_counts = [0u32; 32];
        let mut ml_counts = [0u32; 64];
        let raw_seqs = self.raw_seqs.as_slice();
        count_seq_symbols_fast(
            raw_seqs,
            &LL_ENC_PACKED,
            &ML_ENC_PACKED,
            &mut ll_counts,
            &mut of_counts,
            &mut ml_counts,
        );

        write_num_sequences(&mut self.comp_buf, raw_seqs.len())?;

        let (ll_tbl, of_tbl, ml_tbl, packed, next_masks): (
            &FseEncTable,
            &FseEncTable,
            &FseEncTable,
            &FseSeqEncTables,
            SeqMasks,
        ) = {
            let total = u32::try_from(raw_seqs.len()).map_err(|_| ZstdError::CorruptSequences)?;

            let (ll_log, of_log, ml_log) =
                if raw_seqs.len() >= 1_024 { (8, 7, 8) } else { (6, 6, 6) };
            let ll_slice = ll_counts.get(..=LL_MAX_CODE).unwrap_or(&[]);
            let of_slice: &[u32] = &of_counts;
            let ml_slice = ml_counts.get(..=ML_MAX_CODE).unwrap_or(&[]);

            let ll_mask = symbol_mask(ll_slice);
            let of_mask = symbol_mask(of_slice);
            let ml_mask = symbol_mask(ml_slice);

            let ll_choice = if allow_repeat
                && self.committed_fse.ll != 0
                && (ll_mask & !self.committed_fse.ll) == 0
            {
                SeqTableChoice::Repeat
            } else {
                plan_seq_table(ll_slice, ll_mask, total, ll_log, &mut self.custom_ll)
            };
            let of_choice = if allow_repeat
                && self.committed_fse.of != 0
                && (of_mask & !self.committed_fse.of) == 0
            {
                SeqTableChoice::Repeat
            } else {
                plan_seq_table(of_slice, of_mask, total, of_log, &mut self.custom_of)
            };
            let ml_choice = if allow_repeat
                && self.committed_fse.ml != 0
                && (ml_mask & !self.committed_fse.ml) == 0
            {
                SeqTableChoice::Repeat
            } else {
                plan_seq_table(ml_slice, ml_mask, total, ml_log, &mut self.custom_ml)
            };

            let ll_mode: u8 = match ll_choice {
                SeqTableChoice::Predefined => 0,
                SeqTableChoice::Rle(_) => 1,
                SeqTableChoice::Custom { .. } => 2,
                SeqTableChoice::Repeat => 3,
            };
            let of_mode: u8 = match of_choice {
                SeqTableChoice::Predefined => 0,
                SeqTableChoice::Rle(_) => 1,
                SeqTableChoice::Custom { .. } => 2,
                SeqTableChoice::Repeat => 3,
            };
            let ml_mode: u8 = match ml_choice {
                SeqTableChoice::Predefined => 0,
                SeqTableChoice::Rle(_) => 1,
                SeqTableChoice::Custom { .. } => 2,
                SeqTableChoice::Repeat => 3,
            };
            let mode_byte = (ll_mode << 6) | (of_mode << 4) | (ml_mode << 2);
            self.comp_buf.try_push(mode_byte).map_err(|r| r.why)?;

            // Emit FSE / RLE table descriptions in RFC 8878 §3.1.1.3.2 order: LL, OF, ML.
            for choice in [&ll_choice, &of_choice, &ml_choice] {
                match *choice {
                    SeqTableChoice::Predefined | SeqTableChoice::Repeat => {}
                    SeqTableChoice::Rle(sym) => {
                        self.comp_buf.try_push(sym).map_err(|r| r.why)?;
                    }
                    SeqTableChoice::Custom { ref norm, max_sym, log } => {
                        let slice = norm.get(..=max_sym).ok_or(ZstdError::CorruptFse)?;
                        write_fse_header(slice, log, &mut self.comp_buf)?;
                    }
                }
            }

            let ll_t = match ll_choice {
                SeqTableChoice::Predefined => &self.predef.ll,
                SeqTableChoice::Rle(_) | SeqTableChoice::Custom { .. } | SeqTableChoice::Repeat => {
                    &self.custom_ll
                }
            };
            let of_t = match of_choice {
                SeqTableChoice::Predefined => &self.predef.of,
                SeqTableChoice::Rle(_) | SeqTableChoice::Custom { .. } | SeqTableChoice::Repeat => {
                    &self.custom_of
                }
            };
            let ml_t = match ml_choice {
                SeqTableChoice::Predefined => &self.predef.ml,
                SeqTableChoice::Rle(_) | SeqTableChoice::Custom { .. } | SeqTableChoice::Repeat => {
                    &self.custom_ml
                }
            };

            let next_masks = SeqMasks {
                ll: match ll_choice {
                    SeqTableChoice::Predefined => 0,
                    SeqTableChoice::Repeat => self.committed_fse.ll,
                    SeqTableChoice::Rle(_) | SeqTableChoice::Custom { .. } => {
                        self.custom_ll.present_mask()
                    }
                },
                of: match of_choice {
                    SeqTableChoice::Predefined => 0,
                    SeqTableChoice::Repeat => self.committed_fse.of,
                    SeqTableChoice::Rle(_) | SeqTableChoice::Custom { .. } => {
                        self.custom_of.present_mask()
                    }
                },
                ml: match ml_choice {
                    SeqTableChoice::Predefined => 0,
                    SeqTableChoice::Repeat => self.committed_fse.ml,
                    SeqTableChoice::Rle(_) | SeqTableChoice::Custom { .. } => {
                        self.custom_ml.present_mask()
                    }
                },
            };

            let packed_ref: &FseSeqEncTables = if mode_byte == 0 {
                &self.predef.packed
            } else {
                if !matches!(ll_choice, SeqTableChoice::Repeat) {
                    ll_t.write_ll_into(&mut self.packed_active);
                }
                if !matches!(of_choice, SeqTableChoice::Repeat) {
                    of_t.write_of_into(&mut self.packed_active);
                }
                if !matches!(ml_choice, SeqTableChoice::Repeat) {
                    ml_t.write_ml_into(&mut self.packed_active);
                }
                &self.packed_active
            };
            (ll_t, of_t, ml_t, packed_ref, next_masks)
        };

        pack_raw_sequences_fse(raw_seqs, ll_tbl, of_tbl, ml_tbl, packed, &mut self.comp_buf)?;
        Ok(next_masks)
    }
}

pub(crate) fn compress_in_inner<A: Allocator + Copy>(
    input: &[u8],
    opts: CompressOpts,
    alloc: A,
) -> Result<Vec<u8, A>, ZstdError> {
    let mut enc = ZstdEncoder::try_new_in(alloc)?;
    enc.compress(input, opts)
}
