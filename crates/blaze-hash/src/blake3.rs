//! A value's derived [`std::hash::Hash`] fed to a BLAKE3 state as bytes the same on every
//! target and in every run: every integer written little-endian at a fixed width, `usize` and
//! `isize` as 64 bits.
//!
//! [`fixed_width_writes!`](crate::fixed_width_writes) is the `write_u8`..`write_isize` half of a
//! `std::hash::Hasher` that does this, for any hasher with its own `write`; [`Blake3Hasher`] is
//! one over a [`Blake3`] state (`blaze_ids::Hasher`), so a type whose `Hash` covers what its
//! `Eq` compares and writes no address (a foreign type such as `rustc_abi`'s layouts) is hashed
//! as content. A `Hash` that writes an address (an `Interned` of `crates/intern`, a raw pointer)
//! is not deterministic and must not be fed this way; feed such parts by content ID.

/// The integer methods of a `std::hash::Hasher`, each writing its argument's little-endian
/// bytes through `self.write`, `usize` as `u64` and `isize` as `i64`. Goes inside the
/// `impl std::hash::Hasher for ..` block, beside its own `write` and `finish`.
#[macro_export]
macro_rules! fixed_width_writes {
    () => {
        #[inline]
        fn write_u8(&mut self, it: u8) {
            self.write(&[it]);
        }

        #[inline]
        fn write_u16(&mut self, it: u16) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_u32(&mut self, it: u32) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_u64(&mut self, it: u64) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_u128(&mut self, it: u128) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_usize(&mut self, it: usize) {
            self.write(&(it as u64).to_le_bytes());
        }

        #[inline]
        fn write_i8(&mut self, it: i8) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_i16(&mut self, it: i16) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_i32(&mut self, it: i32) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_i64(&mut self, it: i64) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_i128(&mut self, it: i128) {
            self.write(&it.to_le_bytes());
        }

        #[inline]
        fn write_isize(&mut self, it: isize) {
            self.write(&(it as i64).to_le_bytes());
        }
    };
}

/// A BLAKE3 state a value's `Hash` can be fed to: `blaze_ids::Hasher` is the one, kept there
/// because a content ID's domain and output are blaze-ids'.
pub trait Blake3 {
    /// Feeds `bytes`.
    fn update(&mut self, bytes: &[u8]);

    /// The first 64 bits of the hash of what was fed so far, little-endian.
    fn first_word(&self) -> u64;
}

/// A `std::hash::Hasher` feeding a [`Blake3`] state, integers at a fixed width.
pub struct Blake3Hasher<'a, B: Blake3 + ?Sized>(pub &'a mut B);

impl<B: Blake3 + ?Sized> std::hash::Hasher for Blake3Hasher<'_, B> {
    /// The first 64 bits of what was fed so far: no `Hash` impl reads it, but the trait asks.
    fn finish(&self) -> u64 {
        self.0.first_word()
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    crate::fixed_width_writes!();
}
