//! Columnar encoding helpers for [`super::ItemsRecord`] tables over [`blaze_columns::enc`]
//! and single-allocation [`blaze_own::soa!`] column buffers (one `len`, one `capacity`, one
//! backing block per table).
//!
//! Each columnar section starts with a fixed `[u32; K]` directory of sub-column byte lengths,
//! followed by any fixed-width sorted key array (`count * 32` bytes) and the `K` encoded
//! sub-columns end to end. A sub-column whose values are all equal to its default is omitted
//! (length 0), so small crates and sparse fields pay zero bytes.

use blaze_columns::enc::{EncError, IdView, IntPolicy, IntView, encode_ids, encode_ints};
use blaze_own::{AVec, Alloc};
use blaze_surface::RecordBytes;

use super::{ItemsRecordError, u32_of};

static ZERO_ID: [u8; 32] = [0u8; 32];

blaze_own::soa! {
    /// Columnar write buffer for `ItemsRecord`'s item metadata (12 fields, 1 allocation).
    pub(super) struct ItemCol, ItemColsBuf, ItemColRef {
        pub module: u64 => modules / modules_mut,
        pub code: u64 => codes / codes_mut,
        pub outline: u64 => outlines / outlines_mut,
        pub name_at: u64 => name_ats / name_ats_mut,
        pub meta: u64 => metas / metas_mut,
        pub start: u64 => starts / starts_mut,
        pub len: u64 => lens / lens_mut,
        pub header_start: u64 => header_starts / header_starts_mut,
        pub header_len: u64 => header_lens / header_lens_mut,
        pub impl_row: u64 => impl_rows / impl_rows_mut,
        pub abi_at: u64 => abi_ats / abi_ats_mut,
        pub abi_len: u64 => abi_lens / abi_lens_mut,
    }
}

blaze_own::soa! {
    /// Columnar write buffer for `ItemsRecord`'s member metadata (6 fields, 1 allocation).
    pub(super) struct MemberCol, MemberColsBuf, MemberColRef {
        pub code: u64 => codes / codes_mut,
        pub outline: u64 => outlines / outlines_mut,
        pub name_at: u64 => name_ats / name_ats_mut,
        pub meta: u64 => metas / metas_mut,
        pub start: u64 => starts / starts_mut,
        pub len: u64 => lens / lens_mut,
    }
}

blaze_own::soa! {
    /// Columnar write buffer for `ItemsRecord`'s impl metadata (9 fields, 1 allocation).
    pub(super) struct ImplCol, ImplColsBuf, ImplColRef {
        pub module: u64 => modules / modules_mut,
        pub trait_at: u64 => trait_ats / trait_ats_mut,
        pub trait_len: u64 => trait_lens / trait_lens_mut,
        pub self_at: u64 => self_ats / self_ats_mut,
        pub self_len: u64 => self_lens / self_lens_mut,
        pub meta: u64 => metas / metas_mut,
        pub roots: u64 => roots / roots_mut,
        pub trait_root: [u8; 32] => trait_roots / trait_roots_mut,
        pub self_root: [u8; 32] => self_roots / self_roots_mut,
    }
}

blaze_own::soa! {
    /// Columnar write buffer for `ItemsRecord`'s impl bucket index (4 fields, 1 allocation).
    pub(super) struct BucketCol, BucketColsBuf, BucketColRef {
        pub meta: u64 => metas / metas_mut,
        pub trait_key: [u8; 32] => trait_keys / trait_keys_mut,
        pub head_key: [u8; 32] => head_keys / head_keys_mut,
        pub impl_row: u64 => impl_rows / impl_rows_mut,
    }
}

blaze_own::soa! {
    /// Columnar write buffer for `ItemsRecord`'s variant table (2 fields, 1 allocation).
    pub(super) struct VariantCol, VariantColsBuf, VariantColRef {
        pub owner: [u8; 32] => owners / owners_mut,
        pub index: u64 => indices / indices_mut,
    }
}

fn map_enc(err: EncError, what: &'static str) -> ItemsRecordError {
    match err {
        EncError::TooLong { .. } | EncError::Alloc(_) => ItemsRecordError::TooBig { what },
        EncError::Corrupt { at, .. } | EncError::Buffer { index: at, .. } => ItemsRecordError::Short { what, at },
    }
}

/// Appends an optional integer column: 0 bytes when every value equals `default`, else a
/// constant-time [`IntView`] column under [`IntPolicy::CONSTANT`].
pub(super) fn encode_opt_u64(
    values: &[u64],
    default: u64,
    what: &'static str,
    alloc: Alloc<'_>,
    scratch: &mut AVec<'_, u8>,
    out: &mut RecordBytes<'_>,
) -> Result<u32, ItemsRecordError> {
    if values.iter().all(|&v| v == default) {
        return Ok(0);
    }
    scratch.clear();
    let choice = encode_ints(values, IntPolicy::CONSTANT, alloc, scratch).map_err(|e| map_enc(e, what))?;
    out.extend_from_slice(scratch.as_slice());
    u32_of(choice.bytes, what)
}

/// Appends an optional 32-byte ID column: 0 bytes when every row is zero, else a deduplicated
/// [`IdView`] column.
pub(super) fn encode_opt_id(
    rows: &[[u8; 32]],
    what: &'static str,
    alloc: Alloc<'_>,
    scratch: &mut AVec<'_, u8>,
    out: &mut RecordBytes<'_>,
) -> Result<u32, ItemsRecordError> {
    if rows.iter().all(|r| *r == ZERO_ID) {
        return Ok(0);
    }
    scratch.clear();
    let written = encode_ids::<32>(rows, alloc, scratch).map_err(|e| map_enc(e, what))?;
    out.extend_from_slice(scratch.as_slice());
    u32_of(written, what)
}

/// Writes the `[u32; K]` sub-column byte-length directory at `dir_at` of `out`.
pub(super) fn patch_dir<const K: usize>(out: &mut RecordBytes<'_>, dir_at: usize, lens: [u32; K]) {
    for (i, len) in lens.into_iter().enumerate() {
        let pos = dir_at + i * 4;
        if let Some(dst) = out.get_mut(pos..pos + 4) {
            dst.copy_from_slice(&len.to_le_bytes());
        }
    }
}

/// Slices a columnar section starting at `base` of `bytes`: `[u32; K]` directory, optional
/// `keys_bytes` slice immediately after it, and the `K` sub-column byte slices.
pub(super) fn read_dir<'a, const K: usize>(
    bytes: &'a [u8],
    base: usize,
    keys_bytes: usize,
    what: &'static str,
) -> Result<(&'a [u8], [&'a [u8]; K]), ItemsRecordError> {
    let dir_bytes = K * 4;
    let Some(dir_end) = base.checked_add(dir_bytes) else {
        return Err(ItemsRecordError::Short { what, at: base });
    };
    let Some(dir) = bytes.get(base..dir_end) else {
        return Err(ItemsRecordError::Short { what, at: base });
    };
    let mut lens = [0usize; K];
    for (i, slot) in lens.iter_mut().enumerate() {
        let Some(chunk) = dir.get(i * 4..i * 4 + 4).and_then(<[u8]>::first_chunk::<4>) else {
            return Err(ItemsRecordError::Short { what, at: base + i * 4 });
        };
        let Ok(len) = usize::try_from(u32::from_le_bytes(*chunk)) else {
            return Err(ItemsRecordError::Short { what, at: base + i * 4 });
        };
        *slot = len;
    }
    let Some(keys_end) = dir_end.checked_add(keys_bytes) else {
        return Err(ItemsRecordError::Short { what, at: dir_end });
    };
    let Some(keys) = bytes.get(dir_end..keys_end) else {
        return Err(ItemsRecordError::Short { what, at: dir_end });
    };
    let mut cols: [&'a [u8]; K] = [&[]; K];
    let mut pos = keys_end;
    for (slot, len) in cols.iter_mut().zip(lens) {
        let Some(end) = pos.checked_add(len) else {
            return Err(ItemsRecordError::Short { what, at: pos });
        };
        let Some(slice) = bytes.get(pos..end) else {
            return Err(ItemsRecordError::Short { what, at: pos });
        };
        *slot = slice;
        pos = end;
    }
    Ok((keys, cols))
}

/// An optional constant-time integer column checked once on open.
#[derive(Clone, Copy, Debug)]
pub(super) struct OptInt<'a> {
    view: Option<IntView<'a>>,
    default: u64,
}

impl<'a> OptInt<'a> {
    pub(super) const fn empty(default: u64) -> Self {
        OptInt { view: None, default }
    }

    pub(super) fn open(bytes: &'a [u8], min_len: usize, default: u64, what: &'static str, at: usize) -> Result<Self, ItemsRecordError> {
        if bytes.is_empty() {
            return Ok(OptInt { view: None, default });
        }
        let view = IntView::open(bytes).map_err(|_| ItemsRecordError::Short { what, at })?;
        if view.len() < min_len {
            return Err(ItemsRecordError::Short { what, at });
        }
        Ok(OptInt { view: Some(view), default })
    }

    #[inline]
    pub(super) fn get(&self, i: usize) -> u64 {
        match self.view {
            Some(view) => view.get(i),
            None => self.default,
        }
    }

    #[inline]
    pub(super) fn get_u32(&self, i: usize) -> u32 {
        u32::try_from(self.get(i)).unwrap_or(u32::MAX)
    }
}

/// An optional 32-byte ID dictionary column checked once on open.
#[derive(Clone, Copy, Debug)]
pub(super) struct OptId<'a> {
    view: Option<IdView<'a, 32>>,
}

impl<'a> OptId<'a> {
    pub(super) const fn empty() -> Self {
        OptId { view: None }
    }

    pub(super) fn open(bytes: &'a [u8], min_len: usize, what: &'static str, at: usize) -> Result<Self, ItemsRecordError> {
        if bytes.is_empty() {
            return Ok(OptId { view: None });
        }
        let view = IdView::<32>::open(bytes).map_err(|_| ItemsRecordError::Short { what, at })?;
        if view.len() < min_len {
            return Err(ItemsRecordError::Short { what, at });
        }
        Ok(OptId { view: Some(view) })
    }

    #[inline]
    pub(super) fn get(&self, i: usize) -> &'a [u8; 32] {
        match self.view {
            Some(view) => view.get(i),
            None => &ZERO_ID,
        }
    }
}
