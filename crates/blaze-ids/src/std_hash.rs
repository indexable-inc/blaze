//! A value's derived [`std::hash::Hash`] fed to a [`Hasher`] through
//! [`blaze_hash::Blake3Hasher`], integers at a fixed width: [`Hasher::std`].

use std::hash::Hash;

pub use blaze_hash::fixed_width_writes;
use blaze_hash::{Blake3, Blake3Hasher};

use crate::Hasher;

impl Blake3 for Hasher {
    #[inline]
    fn update(&mut self, bytes: &[u8]) {
        Hasher::update(self, bytes);
    }

    fn first_word(&self) -> u64 {
        let mut word = [0; 8];
        word.copy_from_slice(&self.finish().0[..8]);
        u64::from_le_bytes(word)
    }
}

impl Hasher {
    /// Feeds what `value`'s [`Hash`] writes, integers at a fixed width.
    pub fn std<T: Hash + ?Sized>(&mut self, value: &T) -> &mut Hasher {
        value.hash(&mut Blake3Hasher(self));
        self
    }
}

#[cfg(test)]
mod tests {
    use std::hash::{Hash, Hasher as _};

    use hegel::TestCase;
    use hegel::generators as gs;

    use crate::Hasher;

    /// Bytes written through [`fixed_width_writes!`](crate::fixed_width_writes), to compare.
    struct Bytes(Vec<u8>);

    impl std::hash::Hasher for Bytes {
        fn finish(&self) -> u64 {
            0
        }

        fn write(&mut self, bytes: &[u8]) {
            self.0.extend_from_slice(bytes);
        }

        crate::fixed_width_writes!();
    }

    fn bytes<T: Hash + ?Sized>(value: &T) -> Vec<u8> {
        let mut out = Bytes(Vec::new());
        value.hash(&mut out);
        out.0
    }

    #[test]
    fn std_writes_fixed_width_little_endian() {
        let mut fed = Hasher::new("blaze v2 ids test std 1");
        fed.std(&(7usize, -2isize, "ab", 3u16));
        let mut expected = Hasher::new("blaze v2 ids test std 1");
        expected
            .update(&7u64.to_le_bytes())
            .update(&(-2i64).to_le_bytes())
            .update(b"ab")
            .update(&[0xff])
            .update(&3u16.to_le_bytes());
        assert_eq!(fed.finish(), expected.finish());
    }

    #[test]
    fn every_width() {
        let mut out = Bytes(Vec::new());
        out.write_u8(1);
        out.write_i8(-1);
        out.write_u16(0x0302);
        out.write_i16(-2);
        out.write_u32(0x0706_0504);
        out.write_i32(-3);
        out.write_u64(8);
        out.write_i64(-4);
        out.write_u128(9);
        out.write_i128(-5);
        out.write_usize(10);
        out.write_isize(-6);
        let mut want = vec![1, 0xff, 2, 3];
        want.extend((-2i16).to_le_bytes());
        want.extend([4, 5, 6, 7]);
        want.extend((-3i32).to_le_bytes());
        want.extend(8u64.to_le_bytes());
        want.extend((-4i64).to_le_bytes());
        want.extend(9u128.to_le_bytes());
        want.extend((-5i128).to_le_bytes());
        want.extend(10u64.to_le_bytes());
        want.extend((-6i64).to_le_bytes());
        assert_eq!(out.0, want);
    }

    /// Against the reference: the hand-written forwarders `blaze-hashcons`, `hir-ty-share`,
    /// `hir-ty-codegen` and the old query engine each carried, here as the bytes they wrote.
    #[hegel::test]
    fn matches_the_reference(tc: TestCase) {
        let a: u8 = tc.draw(gs::integers());
        let b: i16 = tc.draw(gs::integers());
        let c: u32 = tc.draw(gs::integers());
        let d: i64 = tc.draw(gs::integers());
        let e: u128 = tc.draw(gs::integers());
        let f: usize = tc.draw(gs::integers());
        let g: isize = tc.draw(gs::integers());
        let s: String = tc.draw(gs::text());
        let mut want = vec![a];
        want.extend(b.to_le_bytes());
        want.extend(c.to_le_bytes());
        want.extend(d.to_le_bytes());
        want.extend(e.to_le_bytes());
        want.extend((f as u64).to_le_bytes());
        want.extend((g as i64 as u64).to_le_bytes());
        want.extend(s.as_bytes());
        want.push(0xff);
        assert_eq!(bytes(&(a, b, c, d, e, f, g, s.as_str())), want);
    }
}
