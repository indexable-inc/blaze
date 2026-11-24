//! The variant table of the items record: "which enum is this variant of, and at which index"
//! as ONE search. A variant has no item row (it is no item of a module) and the names record
//! keeps variants by their enum, so before this section a reader that held only a variant's
//! key read every enum of the variant's crate (each item row of kind enum, then its variant
//! list) to find it: a crate scan on the first ask about a crate. The section is that
//! question's index, written from the same lists.
//!
//! The section starts at the offset in header field [`F_VARIANTS_AT`] and has the count of rows
//! in header field [`F_VARIANTS_COUNT`], after everything the record held before it (no
//! existing field or row changes: [`append`] writes after the last byte, 16-aligned). One
//! 72-byte row per variant of every enum the crate's names hold, sorted by (variant key, enum
//! key, index): the variant's key 32; its enum's key 32; its index in the enum's list (the
//! declaration order, under the crate's cfg) u32; 4 zero bytes.
//!
//! One variant key under two enums is a writer's bug upstream (a key names one thing): it is
//! kept as two rows and [`ItemsRecord::variant_home`] refuses it by name, never answers one.

use blaze_hash::HashMap;
use blaze_id::{ContentId, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::RecordBytes;

use super::cols::{OptId, OptInt, VariantCol, VariantColsBuf, encode_opt_id, encode_opt_u64, patch_dir, read_dir};
use super::{ItemsRecord, ItemsRecordError, key_at, pad, put32, u32_of};
use crate::task::Variant;

/// The header field holding how many rows the section has.
pub(crate) const F_VARIANTS_COUNT: usize = 29;
/// The header field holding the section's offset.
pub(crate) const F_VARIANTS_AT: usize = 30;

/// Columnar variant table opened once in [`ItemsRecord::open`]: sorted 32-byte variant keys for
/// direct binary search, deduplicated enum keys (`IdView`), and bit-packed declaration indices.
#[derive(Clone, Copy, Debug)]
pub(super) struct VariantsCols<'a> {
    count: usize,
    keys: &'a [u8],
    enum_keys: OptId<'a>,
    indices: OptInt<'a>,
}

impl<'a> VariantsCols<'a> {
    pub(super) const fn empty() -> Self {
        VariantsCols { count: 0, keys: &[], enum_keys: OptId::empty(), indices: OptInt::empty(0) }
    }

    pub(super) fn open(bytes: &'a [u8], base: usize, count: usize) -> Result<Self, ItemsRecordError> {
        if count == 0 {
            return Ok(Self::empty());
        }
        let Some(keys_bytes) = count.checked_mul(32) else {
            return Err(ItemsRecordError::Short { what: "the variant table", at: base });
        };
        let (keys, [enum_b, idx_b]) = read_dir::<2>(bytes, base, keys_bytes, "the variant table")?;
        let enum_keys = OptId::open(enum_b, count, "the variant table's enum keys", base)?;
        let indices = OptInt::open(idx_b, count, 0, "the variant table's indices", base)?;
        Ok(VariantsCols { count, keys, enum_keys, indices })
    }
}

/// Appends the section for `enums` (each enum's variants in declaration order; the map's order
/// is not read: the rows are sorted here) to `out`, a record whose header is written, and sets
/// its count and offset in the header.
pub(super) fn append<'a>(out: &mut RecordBytes<'_>, enums: &HashMap<ItemKey, AVec<'a, Variant<&'a str>>, Alloc<'a>>, alloc: Alloc<'_>) -> Result<(), ItemsRecordError> {
    let mut rows: AVec<'_, (ItemKey, ItemKey, u32)> = AVec::new_in(alloc);
    for (owner, variants) in enums.iter() {
        for (index, v) in variants.iter().enumerate() {
            rows.push((v.key, *owner, u32_of(index, "an enum's variants")?));
        }
    }
    rows.sort_unstable();
    pad(out);
    let at = u32_of(out.len(), "the variant table")?;
    let count = u32_of(rows.len(), "the variant table's rows")?;
    if !rows.is_empty() {
        let dir_at = out.len();
        put32(out, 0);
        put32(out, 0);
        let mut buf = VariantColsBuf::with_capacity_in(rows.len(), alloc);
        for (variant, owner, index) in rows.iter() {
            out.extend_from_slice(&variant.0.0);
            buf.push(VariantCol { owner: owner.0.0, index: u64::from(*index) });
        }
        let mut scratch = AVec::new_in(alloc);
        let enum_len = encode_opt_id(buf.owners(), "the variant table's enum keys", alloc, &mut scratch, out)?;
        let idx_len = encode_opt_u64(buf.indices(), 0, "the variant table's indices", alloc, &mut scratch, out)?;
        patch_dir(out, dir_at, [enum_len, idx_len]);
    }
    let field = 8 + 4 * F_VARIANTS_COUNT;
    out[field..field + 4].copy_from_slice(&count.to_le_bytes());
    let field = 8 + 4 * F_VARIANTS_AT;
    out[field..field + 4].copy_from_slice(&at.to_le_bytes());
    Ok(())
}

impl<'a> ItemsRecord<'a> {
    /// How many rows the variant table has: one per variant of the crate's enums.
    pub fn variant_rows(&self) -> usize {
        self.variants.count
    }

    /// Row `i` of the variant table (sorted by variant key): the variant, its enum, its index.
    pub fn variant_at(&self, i: usize) -> Result<Option<(ItemKey, ItemKey, u32)>, ItemsRecordError> {
        if i >= self.variants.count {
            return Ok(None);
        }
        let variant = key_at(self.variants.keys, i * 32);
        let owner = ItemKey(ContentId(*self.variants.enum_keys.get(i)));
        let index = self.variants.indices.get_u32(i);
        Ok(Some((variant, owner, index)))
    }

    /// The enum `variant` is a variant of, and its index in the enum's declaration order: one
    /// binary search of the section (no item row, no variant list, no text is read).
    /// `Ok(None)`: no variant of the crate has that key.
    pub fn variant_home(&self, variant: ItemKey) -> Result<Option<(ItemKey, u32)>, ItemsRecordError> {
        let count = self.variants.count;
        let keys = self.variants.keys;
        // The first row whose key is not below `variant`.
        let (mut lo, mut hi) = (0usize, count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let r = mid * 32;
            if keys[r..r + 32] < variant.0.0[..] {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let r = lo * 32;
        if lo >= count || keys[r..r + 32] != variant.0.0[..] {
            return Ok(None);
        }
        // The rows are sorted: a second enum holding the key is the next row.
        if lo + 1 < count && keys[r + 32..r + 64] == variant.0.0[..] {
            return Err(ItemsRecordError::Short { what: "a variant's enum (two rows of the variant table hold the key)", at: lo });
        }
        let owner = ItemKey(ContentId(*self.variants.enum_keys.get(lo)));
        let index = self.variants.indices.get_u32(lo);
        Ok(Some((owner, index)))
    }
}
