//! Little-endian integers at a byte offset: the one copy of the `u32_at`/`u64_at` helpers that
//! read fixed-layout records and headers in place (a store's records, an image's tables, an
//! object file's headers). It is here, in a crate with no dependencies, so the crates below
//! `blaze-format` (`blaze-core`, the runtimes linked into a patched program, the syntax tree)
//! read with it too; a sequence of fields is `blaze_format::cursor::Reader`'s.
//!
//! [`get_u32`] and its kin are checked: `None` when the bytes end first, at any offset (no
//! overflow). [`u32`] and its kin are for bytes already checked to hold the record, and panic
//! on a read past the end.

macro_rules! at {
    ($($int:ident $get:ident $len:literal),* $(,)?) => {$(
        #[doc = concat!("The little-endian `", stringify!($int), "` at `at`, or `None` when fewer than ", stringify!($len), " bytes start there.")]
        #[inline]
        pub const fn $get(bytes: &[u8], at: usize) -> Option<$int> {
            match bytes.split_at_checked(at) {
                Some((_, rest)) => match rest.first_chunk::<$len>() {
                    Some(word) => Some($int::from_le_bytes(*word)),
                    None => None,
                },
                None => None,
            }
        }

        #[doc = concat!("The little-endian `", stringify!($int), "` at `at`, of bytes known to hold it. Panics when fewer than ", stringify!($len), " bytes start there.")]
        #[inline]
        #[track_caller]
        pub fn $int(bytes: &[u8], at: usize) -> $int {
            match $get(bytes, at) {
                Some(it) => it,
                None => panic!(concat!("a ", stringify!($len), "-byte read at {} of {} bytes"), at, bytes.len()),
            }
        }
    )*};
}

at!(u16 get_u16 2, u32 get_u32 4, u64 get_u64 8, u128 get_u128 16);

#[cfg(test)]
mod tests {
    use super::*;

    /// The copies every crate carried (`blaze-cas`, `blaze-query`, `blaze-image`,
    /// `blaze-premap`, `blaze-link`'s `macho/unwind.rs`): panicking reads.
    fn old_u32(bytes: &[u8], at: usize) -> u32 {
        std::primitive::u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
    }

    fn old_u64(bytes: &[u8], at: usize) -> u64 {
        std::primitive::u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
    }

    /// The checked copies (`syntax`, `syntax-tree`, `base-db`'s `trees.rs`).
    fn old_get_u32(bytes: &[u8], at: usize) -> Option<u32> {
        Some(std::primitive::u32::from_le_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?))
    }

    fn old_get_u16(bytes: &[u8], at: usize) -> Option<u16> {
        Some(std::primitive::u16::from_le_bytes(bytes.get(at..at.checked_add(2)?)?.try_into().ok()?))
    }

    fn old_get_u64(bytes: &[u8], at: usize) -> Option<u64> {
        Some(std::primitive::u64::from_le_bytes(bytes.get(at..at.checked_add(8)?)?.try_into().ok()?))
    }

    #[test]
    fn every_offset_of_short_and_long_bytes_reads_as_the_copies_did() {
        let bytes: Vec<u8> = (0..40u8).map(|it| it.wrapping_mul(37) ^ 0xa5).collect();
        for len in 0..bytes.len() {
            let bytes = &bytes[..len];
            for at in (0..len + 10).chain([usize::MAX - 3, usize::MAX]) {
                assert_eq!(get_u16(bytes, at), old_get_u16(bytes, at), "u16 at {at} of {len}");
                assert_eq!(get_u32(bytes, at), old_get_u32(bytes, at), "u32 at {at} of {len}");
                assert_eq!(get_u64(bytes, at), old_get_u64(bytes, at), "u64 at {at} of {len}");
                if at.checked_add(4).is_some_and(|end| end <= len) {
                    assert_eq!(u32(bytes, at), old_u32(bytes, at));
                }
                if at.checked_add(8).is_some_and(|end| end <= len) {
                    assert_eq!(u64(bytes, at), old_u64(bytes, at));
                }
            }
        }
    }

    #[test]
    fn a_u128_is_two_u64s() {
        let bytes: Vec<u8> = (0..16).collect();
        let (low, high) = (u64(&bytes, 0), u64(&bytes, 8));
        assert_eq!(u128(&bytes, 0), std::primitive::u128::from(low) | std::primitive::u128::from(high) << 64);
        assert_eq!(get_u128(&bytes, 1), None);
    }

    #[test]
    #[should_panic(expected = "a 4-byte read at 2 of 5 bytes")]
    fn a_read_past_the_end_panics() {
        u32(&[0; 5], 2);
    }

    #[test]
    fn reads_are_const() {
        const HEADER: Option<u32> = get_u32(b"\x01\x00\x00\x00rest", 0);
        assert_eq!(HEADER, Some(1));
    }
}
