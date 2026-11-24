//! Pre-hashed RFC 8878 §5 raw-content dictionary for compressing many small related records
//! (such as Rust source files or columnar records) without re-hashing the dictionary per frame.

use core::alloc::Allocator;

use blaze_own::{
    LZ77_DEF_BUCKETS, LZ77_FAST_HASH_SIZE, Vec, lz77_preload_default, lz77_preload_fast,
};

use crate::ZstdError;
use crate::xxh64::xxh64;

/// Maximum raw-content dictionary size (128 KiB, well within both fast 512 KiB and default 2 MiB
/// LZ77 windows).
pub const MAX_DICT_BYTES: usize = 128 * 1024;

/// A pre-hashed RFC 8878 raw-content dictionary.
///
/// Built once from representative bytes (e.g. concatenated Rust source idioms or a pack's
/// embedded dictionary) and reused across arbitrarily many [`crate::ZstdEncoder::compress_with_dict_into`]
/// calls: each frame copies the pre-hashed LZ77 table in one `memcpy` and matches directly into
/// the dictionary prefix.
pub struct ZstdDict<A: Allocator + Copy> {
    bytes: Vec<u8, A>,
    hash: u64,
    fast_table: Vec<u32, A>,
    default_table: Vec<u64, A>,
}

impl<A: Allocator + Copy> ZstdDict<A> {
    /// Build a pre-hashed raw-content dictionary from `dict_bytes` in `alloc`.
    ///
    /// If `dict_bytes` exceeds [`MAX_DICT_BYTES`], only its trailing [`MAX_DICT_BYTES`] are kept
    /// (the closest prefix to the start of compressed frames).
    pub fn try_new_in(dict_bytes: &[u8], alloc: A) -> Result<Self, ZstdError> {
        let slice = if dict_bytes.len() > MAX_DICT_BYTES {
            dict_bytes
                .get(dict_bytes.len() - MAX_DICT_BYTES..)
                .unwrap_or(&[])
        } else {
            dict_bytes
        };

        let mut bytes = Vec::try_with_capacity_in(slice.len(), alloc)?;
        bytes.try_extend_from_slice(slice)?;

        let mut fast_table = Vec::try_with_capacity_in(LZ77_FAST_HASH_SIZE, alloc)?;
        let z32 = [0u32; 256];
        let mut rem = LZ77_FAST_HASH_SIZE;
        while rem >= z32.len() {
            fast_table.try_extend_from_slice(&z32)?;
            rem -= z32.len();
        }
        if rem > 0
            && let Some(tail) = z32.get(..rem)
        {
            fast_table.try_extend_from_slice(tail)?;
        }
        if let Some(arr) = fast_table
            .as_mut_slice()
            .get_mut(..LZ77_FAST_HASH_SIZE)
            .and_then(|s| <&mut [u32; LZ77_FAST_HASH_SIZE]>::try_from(s).ok())
        {
            lz77_preload_fast(slice, arr);
        }

        let mut default_table = Vec::try_with_capacity_in(LZ77_DEF_BUCKETS, alloc)?;
        let z64 = [0u64; 128];
        let mut rem = LZ77_DEF_BUCKETS;
        while rem >= z64.len() {
            default_table.try_extend_from_slice(&z64)?;
            rem -= z64.len();
        }
        if rem > 0
            && let Some(tail) = z64.get(..rem)
        {
            default_table.try_extend_from_slice(tail)?;
        }
        if let Some(arr) = default_table
            .as_mut_slice()
            .get_mut(..LZ77_DEF_BUCKETS)
            .and_then(|s| <&mut [u64; LZ77_DEF_BUCKETS]>::try_from(s).ok())
        {
            lz77_preload_default(slice, arr);
        }

        let hash = xxh64(slice, 0);
        Ok(Self { bytes, hash, fast_table, default_table })
    }

    /// Raw dictionary bytes (at most [`MAX_DICT_BYTES`]).
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    #[inline]
    pub(crate) fn hash(&self) -> u64 {
        self.hash
    }

    #[inline]
    pub(crate) fn fast_table(&self) -> &[u32] {
        self.fast_table.as_slice()
    }

    #[inline]
    pub(crate) fn default_table(&self) -> &[u64] {
        self.default_table.as_slice()
    }
}
