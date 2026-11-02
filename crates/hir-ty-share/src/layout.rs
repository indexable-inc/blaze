//! A layout as bytes: everything of `rustc_abi::LayoutData` the code generator reads (fields
//! and their offsets, variants, tag encoding, niches, the backend representation with its
//! signedness and valid ranges, size and alignments, `repr(C)`), and not its randomization
//! seed, which only says how the fields were shuffled and is in their offsets already.

use std::hash::Hash;

use blaze_ids::Hasher;
use hir_ty::layout::Layout;
use rustc_abi::LayoutData;

/// Hashes what `layout` is into `out`.
pub(crate) fn write(layout: &Layout, out: &mut Hasher) {
    let LayoutData {
        fields,
        variants,
        backend_repr,
        largest_niche,
        uninhabited,
        align,
        size,
        max_repr_align,
        unadjusted_abi_align,
        repr_c,
        randomization_seed: _,
    } = layout;
    let mut feed = Feed(out);
    fields.hash(&mut feed);
    variants.hash(&mut feed);
    backend_repr.hash(&mut feed);
    largest_niche.hash(&mut feed);
    uninhabited.hash(&mut feed);
    align.hash(&mut feed);
    size.hash(&mut feed);
    max_repr_align.hash(&mut feed);
    unadjusted_abi_align.hash(&mut feed);
    repr_c.hash(&mut feed);
}

/// What a derived `Hash` writes, as bytes hashed into a [`Hasher`], every integer little-endian and every `usize` and
/// `isize` 8 bytes: the same on every host. A derived `Hash` writes its enum discriminants and
/// fields in declaration order, which the vendored `rustc_abi` fixes.
pub(crate) struct Feed<'a>(pub(crate) &'a mut Hasher);

impl std::hash::Hasher for Feed<'_> {
    fn finish(&self) -> u64 {
        unreachable!("a layout is written as bytes; nothing asks for its `u64`")
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    fn write_u8(&mut self, value: u8) {
        self.0.update(&[value]);
    }

    fn write_u16(&mut self, value: u16) {
        self.0.update(&value.to_le_bytes());
    }

    fn write_u32(&mut self, value: u32) {
        self.0.update(&value.to_le_bytes());
    }

    fn write_u64(&mut self, value: u64) {
        self.0.update(&value.to_le_bytes());
    }

    fn write_u128(&mut self, value: u128) {
        self.0.update(&value.to_le_bytes());
    }

    fn write_usize(&mut self, value: usize) {
        self.0.update(&(value as u64).to_le_bytes());
    }

    fn write_i8(&mut self, value: i8) {
        self.0.update(&[value as u8]);
    }

    fn write_i16(&mut self, value: i16) {
        self.0.update(&value.to_le_bytes());
    }

    fn write_i32(&mut self, value: i32) {
        self.0.update(&value.to_le_bytes());
    }

    fn write_i64(&mut self, value: i64) {
        self.0.update(&value.to_le_bytes());
    }

    fn write_i128(&mut self, value: i128) {
        self.0.update(&value.to_le_bytes());
    }

    fn write_isize(&mut self, value: isize) {
        self.0.update(&(value as i64).to_le_bytes());
    }
}
