//! RFC 8878 Zstandard frame and block compressor: reusable [`ZstdEncoder`] context,
//! 5-byte SWAR / 2-way associative LZ77 with 1 MiB window, Treeless-reusing 4-stream Huffman
//! literals, custom per-block & RLE FSE sequence tables, and zero-pre-zeroing [`ByteCursor`]
//! bit-packing.

use core::alloc::Allocator;

use blaze_own::{
    LZ77_DEF_BUCKETS, LZ77_FAST_HASH_SIZE, SeqOp, Vec, fse_pack_sequences_fast, lz77_scan_default,
    lz77_scan_fast,
};

use crate::dict::ZstdDict;
use crate::fse::{
    FseDecTable, FseEncTable, LL_DEFAULT_LOG, LL_DEFAULT_NORM, LL_ENC_PACKED, LL_MAX_CODE,
    ML_DEFAULT_LOG, ML_DEFAULT_NORM, ML_ENC_PACKED, ML_MAX_CODE, OF_DEFAULT_LOG, OF_DEFAULT_NORM,
    OF_MAX_CODE, encode_ll_fast, encode_ll_raw, encode_ml_fast, encode_ml_raw, encode_of_fast,
    encode_of_raw, normalize_fse_counts, write_fse_header,
};
use crate::huffman::{HuffEncCache, encode_huffman_literals_into};
use crate::xxh64::xxh64;
use crate::{CompressOpts, Level, MAX_BLOCK_SIZE, ZSTD_MAGIC, ZstdError};

/// 4 MiB sliding window for multi-block frames (well within `ZstdLimits::USUAL` = 8 MiB).
const MAX_WINDOW: usize = 1 << 22;
/// Window_Descriptor for exact 4 MiB window: exponent = 12, mantissa = 0 (`1 << (10 + 12)`).
const WINDOW_DESC_4M: u8 = 12u8 << 3;

#[inline]
fn ensure_zero_u64<A: Allocator>(table: &mut Vec<u64, A>, len: usize) -> Result<(), ZstdError> {
    if table.len() >= len {
        if let Some(s) = table.as_mut_slice().get_mut(..len) {
            s.fill(0);
        }
        return Ok(());
    }
    table.clear();
    table.try_reserve(len)?;
    let chunk = [0u64; 128];
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
fn ensure_zero_hash<A: Allocator>(table: &mut Vec<u32, A>, len: usize) -> Result<(), ZstdError> {
    if table.len() >= len {
        if let Some(s) = table.as_mut_slice().get_mut(..len) {
            s.fill(0);
        }
        return Ok(());
    }
    table.clear();
    table.try_reserve(len)?;
    let chunk = [0u32; 256];
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
        let len_u8 = u8::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
        out.try_push(len_u8 << 3).map_err(|r| r.why)?;
    } else if lit_len < 4_096 {
        let len_u16 = u16::try_from(lit_len).map_err(|_| ZstdError::CorruptLiterals)?;
        let v = (len_u16 << 4) | 0b0100;
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
        let b0 = u8::try_from(n).map_err(|_| ZstdError::CorruptSequences)?;
        out.try_push(b0).map_err(|r| r.why)?;
    } else if n < 0x7F00 {
        let hi = u8::try_from((n >> 8) + 128).map_err(|_| ZstdError::CorruptSequences)?;
        let lo = u8::try_from(n & 0xFF).map_err(|_| ZstdError::CorruptSequences)?;
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
}

impl PredefinedEncTables {
    fn build() -> Result<Self, ZstdError> {
        let mut dec = FseDecTable::new_empty();
        dec.build_from_norm(&LL_DEFAULT_NORM, LL_DEFAULT_LOG)?;
        let ll = FseEncTable::build_from_dec(&dec)?;

        dec.build_from_norm(&OF_DEFAULT_NORM, OF_DEFAULT_LOG)?;
        let of = FseEncTable::build_from_dec(&dec)?;

        dec.build_from_norm(&ML_DEFAULT_NORM, ML_DEFAULT_LOG)?;
        let ml = FseEncTable::build_from_dec(&dec)?;

        Ok(Self { ll, of, ml })
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
    comp_buf: &mut Vec<u8, A>,
) -> Result<(), ZstdError> {
    let last = raw_seqs.last().ok_or(ZstdError::CorruptSequences)?;
    let (l_ll_c, l_ll_e, l_ll_b) =
        encode_ll_fast(last.lit_len).ok_or(ZstdError::CorruptSequences)?;
    let (l_ml_c, l_ml_e, l_ml_b) =
        encode_ml_fast(last.match_len).ok_or(ZstdError::CorruptSequences)?;
    let (l_of_c, l_of_e, l_of_b) =
        encode_of_fast(last.offset).ok_or(ZstdError::CorruptSequences)?;

    let init_ll = ll_tbl.init_state(l_ll_c).ok_or(ZstdError::CorruptFse)?;
    let init_of = of_tbl.init_state(l_of_c).ok_or(ZstdError::CorruptFse)?;
    let init_ml = ml_tbl.init_state(l_ml_c).ok_or(ZstdError::CorruptFse)?;

    fse_pack_sequences_fast(
        raw_seqs,
        &LL_ENC_PACKED,
        &ML_ENC_PACKED,
        ll_tbl.pack_view(),
        of_tbl.pack_view(),
        ml_tbl.pack_view(),
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

/// Choose between `RLE_Mode (1)`, `FSE_Compressed_Mode (2)`, or `Predefined_Mode (0)` for one
/// sequence code stream (`LL`, `OF`, or `ML`).
enum SeqTableChoice {
    Predefined,
    Rle(u8),
    Custom { norm: [i16; 64], max_sym: usize, log: u8 },
}

fn plan_seq_table(
    counts: &[u32],
    total: u32,
    target_log: u8,
    scratch_dec: &mut FseDecTable,
    custom_enc: &mut FseEncTable,
) -> SeqTableChoice {
    let mut distinct: usize = 0;
    let mut only_sym: u8 = 0;
    for (sym, &c) in counts.iter().enumerate() {
        if c > 0 {
            distinct += 1;
            only_sym = u8::try_from(sym).unwrap_or(0);
        }
    }
    if distinct == 1 {
        if custom_enc.set_rle(only_sym).is_ok() {
            return SeqTableChoice::Rle(only_sym);
        }
        return SeqTableChoice::Predefined;
    }
    let mut norm = [0i16; 64];
    if let Some(max_sym) = normalize_fse_counts(counts, total, target_log, &mut norm)
        && let Some(norm_slice) = norm.get(..=max_sym)
        && scratch_dec.build_from_norm(norm_slice, target_log).is_ok()
        && custom_enc.rebuild_from_dec(scratch_dec).is_ok()
    {
        return SeqTableChoice::Custom { norm, max_sym, log: target_log };
    }
    SeqTableChoice::Predefined
}

/// Reusable RFC 8878 Zstandard compressor context.
///
/// Reuses prebuilt FSE tables, LZ77 hash table, Huffman table cache (for `Treeless_Literals_Block`),
/// and block scratch buffers across calls with zero per-call workspace allocation.
pub struct ZstdEncoder<A: Allocator + Copy> {
    predef: PredefinedEncTables,
    scratch_dec: FseDecTable,
    custom_ll: FseEncTable,
    custom_of: FseEncTable,
    custom_ml: FseEncTable,
    hash_table: Vec<u32, A>,
    hash_table_2way: Vec<u64, A>,
    dict_work: Vec<u8, A>,
    dict_tag: (usize, usize, u64),
    literals_buf: Vec<u8, A>,
    raw_seqs: Vec<SeqOp, A>,
    comp_buf: Vec<u8, A>,
    huff_payload: Vec<u8, A>,
    huff_cache: HuffEncCache,
    alloc: A,
}

impl<A: Allocator + Copy> ZstdEncoder<A> {
    /// Create a reusable Zstandard compressor context backed by `alloc`.
    pub fn try_new_in(alloc: A) -> Result<Self, ZstdError> {
        Ok(Self {
            predef: PredefinedEncTables::build()?,
            scratch_dec: FseDecTable::new_empty(),
            custom_ll: FseEncTable::new_empty(),
            custom_of: FseEncTable::new_empty(),
            custom_ml: FseEncTable::new_empty(),
            hash_table: Vec::new_in(alloc),
            hash_table_2way: Vec::new_in(alloc),
            dict_work: Vec::new_in(alloc),
            dict_tag: (0, 0, 0),
            literals_buf: Vec::new_in(alloc),
            raw_seqs: Vec::new_in(alloc),
            comp_buf: Vec::new_in(alloc),
            huff_payload: Vec::new_in(alloc),
            huff_cache: HuffEncCache::new(),
            alloc,
        })
    }

    /// Compress `input` into a freshly allocated RFC 8878 Zstandard frame in this encoder's
    /// allocator.
    pub fn compress(&mut self, input: &[u8], opts: CompressOpts) -> Result<Vec<u8, A>, ZstdError> {
        let init_cap = (input.len() / 2).max(256).min(input.len().saturating_add(64));
        let mut out = Vec::try_with_capacity_in(init_cap, self.alloc)?;
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
        let mut out = Vec::try_with_capacity_in(init_cap, self.alloc)?;
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
        let has_dict = dict.is_some();
        let len_u64 = u64::try_from(input.len()).map_err(|_| ZstdError::CorruptSequences)?;
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
            out.try_push(WINDOW_DESC_4M).map_err(|r| r.why)?;
        }

        match fcs_flag {
            0 => {
                let b = u8::try_from(len_u64).map_err(|_| ZstdError::CorruptSequences)?;
                out.try_push(b).map_err(|r| r.why)?;
            }
            1 => {
                let adj = u16::try_from(len_u64 - 256).map_err(|_| ZstdError::CorruptSequences)?;
                out.try_extend_from_slice(&adj.to_le_bytes())?;
            }
            2 => {
                let v = u32::try_from(len_u64).map_err(|_| ZstdError::CorruptSequences)?;
                out.try_extend_from_slice(&v.to_le_bytes())?;
            }
            _ => {
                out.try_extend_from_slice(&len_u64.to_le_bytes())?;
            }
        }

        // 3. Blocks
        if input.is_empty() {
            write_block_header(out, true, 0, 0)?;
        } else {
            let fast_mode = matches!(opts.level, Level::Fast);
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
                } else if self.hash_table_2way.len() != LZ77_DEF_BUCKETS {
                    self.hash_table_2way.clear();
                    self.hash_table_2way.try_extend_from_slice(dt)?;
                } else {
                    self.hash_table_2way.as_mut_slice().copy_from_slice(dt);
                }
                db.len()
            } else {
                if fast_mode {
                    ensure_zero_hash(&mut self.hash_table, LZ77_FAST_HASH_SIZE)?;
                } else {
                    ensure_zero_u64(&mut self.hash_table_2way, LZ77_DEF_BUCKETS)?;
                }
                0
            };
            self.huff_cache.reset();

            let mut rep = [1usize, 4usize, 8usize];
            let mut emitted_predef_fse = false;
            let mut has_fast_custom_fse = false;
            let scan_end = dict_len.saturating_add(input.len());
            let mut block_start: usize = dict_len;

            while block_start < scan_end {
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
                {
                    let scan_input: &[u8] = if dict_len > 0 { self.dict_work.as_slice() } else { input };
                    if fast_mode {
                        let table_arr: &mut [u32; LZ77_FAST_HASH_SIZE] = self
                            .hash_table
                            .as_mut_slice()
                            .get_mut(..LZ77_FAST_HASH_SIZE)
                            .and_then(|s| s.try_into().ok())
                            .ok_or(ZstdError::CorruptSequences)?;
                        lz77_scan_fast(
                            scan_input,
                            block_start,
                            block_end,
                            MAX_WINDOW,
                            table_arr,
                            &mut trial_rep,
                            &mut self.literals_buf,
                            &mut self.raw_seqs,
                        )?;
                    } else {
                        let table_arr: &mut [u64; LZ77_DEF_BUCKETS] = self
                            .hash_table_2way
                            .as_mut_slice()
                            .get_mut(..LZ77_DEF_BUCKETS)
                            .and_then(|s| s.try_into().ok())
                            .ok_or(ZstdError::CorruptSequences)?;
                        lz77_scan_default(
                            scan_input,
                            block_start,
                            block_end,
                            MAX_WINDOW,
                            table_arr,
                            &mut trial_rep,
                            &mut self.literals_buf,
                            &mut self.raw_seqs,
                        )?;
                    }
                }

                if !self.raw_seqs.is_empty() {
                    let use_custom_fse = !fast_mode && self.raw_seqs.len() >= 128;
                    let built_fast_custom = self.encode_compressed_block(
                        fast_mode,
                        use_custom_fse,
                        emitted_predef_fse && !use_custom_fse,
                        has_fast_custom_fse,
                    )?;
                    if self.comp_buf.len() < block_len {
                        write_block_header(out, last_block, 2, self.comp_buf.len())?;
                        out.try_extend_from_slice(self.comp_buf.as_slice())?;
                        rep = trial_rep;
                        self.huff_cache.commit();
                        if built_fast_custom {
                            has_fast_custom_fse = true;
                            emitted_predef_fse = false;
                        } else if !has_fast_custom_fse {
                            emitted_predef_fse = !use_custom_fse;
                        }
                        block_start = block_end;
                        continue;
                    }
                    self.huff_cache.discard();
                }

                write_block_header(out, last_block, 0, input_block.len())?;
                out.try_extend_from_slice(input_block)?;
                block_start = block_end;
            }
        }

        // 4. Optional trailing 32-bit XXH64 content checksum
        if opts.checksum {
            let hash = xxh64(input, 0);
            let [c0, c1, c2, c3, ..] = hash.to_le_bytes();
            out.try_extend_from_slice(&[c0, c1, c2, c3])?;
        }

        Ok(())
    }

    fn encode_compressed_block(
        &mut self,
        fast_mode: bool,
        use_custom_fse: bool,
        reuse_predef_fse: bool,
        has_fast_custom_fse: bool,
    ) -> Result<bool, ZstdError> {
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

        if fast_mode {
            write_num_sequences(&mut self.comp_buf, self.raw_seqs.len())?;
            if has_fast_custom_fse {
                // Repeat all three block-0 seeded custom FSE tables (`0xFC`).
                self.comp_buf.try_push(0xFC).map_err(|r| r.why)?;
                pack_raw_sequences_fse(
                    self.raw_seqs.as_slice(),
                    &self.custom_ll,
                    &self.custom_of,
                    &self.custom_ml,
                    &mut self.comp_buf,
                )?;
                return Ok(false);
            }
            if self.raw_seqs.len() >= 128
                && self.try_build_fast_seeded_fse()?
            {
                pack_raw_sequences_fse(
                    self.raw_seqs.as_slice(),
                    &self.custom_ll,
                    &self.custom_of,
                    &self.custom_ml,
                    &mut self.comp_buf,
                )?;
                return Ok(true);
            }
            let mode_byte: u8 = if reuse_predef_fse { 0xFC } else { 0x00 };
            self.comp_buf.try_push(mode_byte).map_err(|r| r.why)?;
            pack_raw_sequences_fse(
                self.raw_seqs.as_slice(),
                &self.predef.ll,
                &self.predef.of,
                &self.predef.ml,
                &mut self.comp_buf,
            )?;
            return Ok(false);
        }

        if !use_custom_fse {
            write_num_sequences(&mut self.comp_buf, self.raw_seqs.len())?;
            let mode_byte: u8 = if reuse_predef_fse { 0xFC } else { 0x00 };
            self.comp_buf.try_push(mode_byte).map_err(|r| r.why)?;
            pack_raw_sequences_fse(
                self.raw_seqs.as_slice(),
                &self.predef.ll,
                &self.predef.of,
                &self.predef.ml,
                &mut self.comp_buf,
            )?;
            return Ok(false);
        }

        let mut ll_counts = [0u32; LL_MAX_CODE + 1];
        let mut of_counts = [0u32; OF_MAX_CODE + 1];
        let mut ml_counts = [0u32; ML_MAX_CODE + 1];
        for raw in self.raw_seqs.as_slice() {
            let (ll_c, _, _) = encode_ll_raw(raw.lit_len);
            let (ml_c, _, _) = encode_ml_raw(raw.match_len);
            let (of_c, _, _) = encode_of_raw(raw.offset);
            if let Some(s) = ll_counts.get_mut(usize::from(ll_c)) {
                *s += 1;
            }
            if let Some(s) = of_counts.get_mut(usize::from(of_c)) {
                *s += 1;
            }
            if let Some(s) = ml_counts.get_mut(usize::from(ml_c)) {
                *s += 1;
            }
        }

        let raw_seqs = self.raw_seqs.as_slice();
        write_num_sequences(&mut self.comp_buf, raw_seqs.len())?;

        let (ll_tbl, of_tbl, ml_tbl): (&FseEncTable, &FseEncTable, &FseEncTable) = {
            let total = u32::try_from(raw_seqs.len()).map_err(|_| ZstdError::CorruptSequences)?;

            let (ll_log, of_log, ml_log) =
                if raw_seqs.len() >= 1_024 { (8, 7, 8) } else { (6, 6, 6) };
            let ll_choice = plan_seq_table(
                &ll_counts,
                total,
                ll_log,
                &mut self.scratch_dec,
                &mut self.custom_ll,
            );
            let of_choice = plan_seq_table(
                &of_counts,
                total,
                of_log,
                &mut self.scratch_dec,
                &mut self.custom_of,
            );
            let ml_choice = plan_seq_table(
                &ml_counts,
                total,
                ml_log,
                &mut self.scratch_dec,
                &mut self.custom_ml,
            );

            let ll_mode: u8 = match ll_choice {
                SeqTableChoice::Predefined => 0,
                SeqTableChoice::Rle(_) => 1,
                SeqTableChoice::Custom { .. } => 2,
            };
            let of_mode: u8 = match of_choice {
                SeqTableChoice::Predefined => 0,
                SeqTableChoice::Rle(_) => 1,
                SeqTableChoice::Custom { .. } => 2,
            };
            let ml_mode: u8 = match ml_choice {
                SeqTableChoice::Predefined => 0,
                SeqTableChoice::Rle(_) => 1,
                SeqTableChoice::Custom { .. } => 2,
            };
            let mode_byte = (ll_mode << 6) | (of_mode << 4) | (ml_mode << 2);
            self.comp_buf.try_push(mode_byte).map_err(|r| r.why)?;

            // Emit FSE / RLE table descriptions in RFC 8878 §3.1.1.3.2 order: LL, OF, ML.
            for choice in [&ll_choice, &of_choice, &ml_choice] {
                match *choice {
                    SeqTableChoice::Predefined => {}
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
                SeqTableChoice::Rle(_) | SeqTableChoice::Custom { .. } => &self.custom_ll,
            };
            let of_t = match of_choice {
                SeqTableChoice::Predefined => &self.predef.of,
                SeqTableChoice::Rle(_) | SeqTableChoice::Custom { .. } => &self.custom_of,
            };
            let ml_t = match ml_choice {
                SeqTableChoice::Predefined => &self.predef.ml,
                SeqTableChoice::Rle(_) | SeqTableChoice::Custom { .. } => &self.custom_ml,
            };
            (ll_t, of_t, ml_t)
        };

        pack_raw_sequences_fse(raw_seqs, ll_tbl, of_tbl, ml_tbl, &mut self.comp_buf)?;
        Ok(false)
    }

    /// Build block-0 custom FSE tables for `Level::Fast` with every valid `LL`, `OF` (512 KiB
    /// window), and `ML` code seeded to at least 1 so blocks 1..N can unconditionally reuse them
    /// via `Repeat` mode (`0xFC`) in a single zero-histogram pass.
    fn try_build_fast_seeded_fse(&mut self) -> Result<bool, ZstdError> {
        let mut ll_counts = [1u32; LL_MAX_CODE + 1];
        let mut of_counts = [0u32; OF_MAX_CODE + 1];
        let mut ml_counts = [1u32; ML_MAX_CODE + 1];
        const FAST_MAX_OF_CODE: usize = 20;
        if let Some(s) = of_counts.get_mut(..=FAST_MAX_OF_CODE) {
            s.fill(1);
        }

        for raw in self.raw_seqs.as_slice() {
            let (ll_c, _, _) = encode_ll_raw(raw.lit_len);
            let (ml_c, _, _) = encode_ml_raw(raw.match_len);
            let (of_c, _, _) = encode_of_raw(raw.offset);
            if let Some(s) = ll_counts.get_mut(usize::from(ll_c)) {
                *s = s.saturating_add(1);
            }
            if let Some(s) = of_counts.get_mut(usize::from(of_c)) {
                *s = s.saturating_add(1);
            }
            if let Some(s) = ml_counts.get_mut(usize::from(ml_c)) {
                *s = s.saturating_add(1);
            }
        }

        let seq_n = u32::try_from(self.raw_seqs.len()).unwrap_or(u32::MAX);
        let ll_total = seq_n.saturating_add(u32::try_from(LL_MAX_CODE + 1).unwrap_or(0));
        let of_total = seq_n.saturating_add(u32::try_from(FAST_MAX_OF_CODE + 1).unwrap_or(0));
        let ml_total = seq_n.saturating_add(u32::try_from(ML_MAX_CODE + 1).unwrap_or(0));

        let mut ll_norm = [0i16; LL_MAX_CODE + 1];
        let mut of_norm = [0i16; OF_MAX_CODE + 1];
        let mut ml_norm = [0i16; ML_MAX_CODE + 1];

        let ll_log: u8 = 8;
        let of_log: u8 = 7;
        let ml_log: u8 = 8;

        let Some(ll_max) = normalize_fse_counts(&ll_counts, ll_total, ll_log, &mut ll_norm) else {
            return Ok(false);
        };
        let Some(of_max) = normalize_fse_counts(&of_counts, of_total, of_log, &mut of_norm) else {
            return Ok(false);
        };
        let Some(ml_max) = normalize_fse_counts(&ml_counts, ml_total, ml_log, &mut ml_norm) else {
            return Ok(false);
        };

        let ll_slice = ll_norm.get(..=ll_max).ok_or(ZstdError::CorruptFse)?;
        let of_slice = of_norm.get(..=of_max).ok_or(ZstdError::CorruptFse)?;
        let ml_slice = ml_norm.get(..=ml_max).ok_or(ZstdError::CorruptFse)?;

        self.scratch_dec.build_from_norm(ll_slice, ll_log)?;
        self.custom_ll.rebuild_from_dec(&self.scratch_dec)?;
        self.scratch_dec.build_from_norm(of_slice, of_log)?;
        self.custom_of.rebuild_from_dec(&self.scratch_dec)?;
        self.scratch_dec.build_from_norm(ml_slice, ml_log)?;
        self.custom_ml.rebuild_from_dec(&self.scratch_dec)?;

        // Mode byte: LL=FSE(2), OF=FSE(2), ML=FSE(2) -> (2<<6)|(2<<4)|(2<<2) = 0xA8.
        self.comp_buf.try_push(0xA8).map_err(|r| r.why)?;
        write_fse_header(ll_slice, ll_log, &mut self.comp_buf)?;
        write_fse_header(of_slice, of_log, &mut self.comp_buf)?;
        write_fse_header(ml_slice, ml_log, &mut self.comp_buf)?;
        Ok(true)
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
