//! A value's derived [`std::hash::Hash`] as its content, for types whose `Hash` covers what
//! their `Eq` compares and writes no address (a foreign type such as `rustc_abi`'s layouts):
//! `Consed::hash_content` is then `hash_std(self, hasher)`.
//!
//! Every integer is written little-endian at a fixed width, `usize` and `isize` as 64 bits, so
//! the content is the same on every target and in every run. A `Hash` that writes an address
//! (an `Interned` of `crates/intern`, a raw pointer) is not deterministic and must not be fed
//! this way; feed such parts by content ID.

use std::hash::{self, Hash};

use blaze_ids::Hasher;

/// Feeds what `value`'s [`Hash`] writes to `hasher`.
pub fn hash_std<T: Hash + ?Sized>(value: &T, hasher: &mut Hasher) {
    value.hash(&mut Writer(hasher));
}

struct Writer<'a>(&'a mut Hasher);

impl hash::Hasher for Writer<'_> {
    /// The first 64 bits of what was fed so far: no `Hash` impl reads it, but the trait asks.
    fn finish(&self) -> u64 {
        let mut word = [0; 8];
        word.copy_from_slice(&self.0.finish().0[..8]);
        u64::from_le_bytes(word)
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    fn write_u8(&mut self, it: u8) {
        self.0.update(&[it]);
    }

    fn write_u16(&mut self, it: u16) {
        self.0.update(&it.to_le_bytes());
    }

    fn write_u32(&mut self, it: u32) {
        self.0.update(&it.to_le_bytes());
    }

    fn write_u64(&mut self, it: u64) {
        self.0.update(&it.to_le_bytes());
    }

    fn write_u128(&mut self, it: u128) {
        self.0.update(&it.to_le_bytes());
    }

    fn write_usize(&mut self, it: usize) {
        self.0.update(&(it as u64).to_le_bytes());
    }

    fn write_i8(&mut self, it: i8) {
        self.0.update(&it.to_le_bytes());
    }

    fn write_i16(&mut self, it: i16) {
        self.0.update(&it.to_le_bytes());
    }

    fn write_i32(&mut self, it: i32) {
        self.0.update(&it.to_le_bytes());
    }

    fn write_i64(&mut self, it: i64) {
        self.0.update(&it.to_le_bytes());
    }

    fn write_i128(&mut self, it: i128) {
        self.0.update(&it.to_le_bytes());
    }

    fn write_isize(&mut self, it: isize) {
        self.0.update(&(it as i64).to_le_bytes());
    }
}
