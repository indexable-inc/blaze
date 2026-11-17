//! The `_in` forms (a buffer in the caller's allocator, the bstd move) against the forms they
//! replace, on `alloc`'s `Vec` and `String`: equal bytes for equal input, or a caller that moves
//! changes what it writes. A wrong `_in` body (a missed `truncate` after the 8-byte store, a sign
//! byte of the unsigned form, the nibbles swapped) fails here.
//!
//! Expected: `test own_tests::in_forms_write_the_bytes_the_old_forms_write ... ok`.


use crate::cursor::Put;
use crate::{hex, leb};

/// Values at every length boundary of LEB128 (1 to 10 bytes, both signs), so both paths of
/// `append` run: the 8-byte store with room for it and the plain extend without.
const UNSIGNED: [u64; 12] =
    [0, 0x7f, 0x80, 0x3fff, 0x4000, (1 << 49) - 1, 1 << 49, (1 << 56) - 1, 1 << 56, 1 << 63, u64::MAX - 1, u64::MAX];
const SIGNED: [i64; 12] = [0, 63, 64, -64, -65, 8191, -8193, (1 << 55) - 1, -(1 << 55), i64::MAX, i64::MIN, -1];

#[test]
fn in_forms_write_the_bytes_the_old_forms_write() {
    let alloc = blaze_own::testing::heap().alloc();
    // `lead` bytes already written, the capacity exact or 16 over: whether the 8-byte store
    // has room turns on both.
    for lead in [0usize, 3, 9] {
        for spare in [0usize, 16] {
            for &value in &UNSIGNED {
                let mut old: Vec<u8> = Vec::with_capacity(lead + leb::len_u64(value) + spare);
                let mut new = blaze_own::Vec::with_capacity_in(lead + leb::len_u64(value) + spare, alloc);
                old.resize(lead, 0xaa);
                new.resize(lead, 0xaa);
                leb::write_u64(&mut old, value);
                leb::write_u64_in(&mut new, value);
                assert_eq!(new.as_slice(), old.as_slice(), "write_u64_in of {value:#x} after {lead} bytes");
                old.put_uleb(value);
                new.put_uleb(value);
                old.put_bytes_u32(&value.to_le_bytes());
                new.put_bytes_u32(&value.to_le_bytes());
                old.pad_to(8);
                new.pad_to(8);
                assert_eq!(new.as_slice(), old.as_slice(), "Put on blaze_own::Vec of {value:#x}");
            }
            for &value in &SIGNED {
                let mut old: Vec<u8> = Vec::with_capacity(lead + leb::len_i64(value) + spare);
                let mut new = blaze_own::Vec::with_capacity_in(lead + leb::len_i64(value) + spare, alloc);
                old.resize(lead, 0xaa);
                new.resize(lead, 0xaa);
                leb::write_i64(&mut old, value);
                leb::write_i64_in(&mut new, value);
                assert_eq!(new.as_slice(), old.as_slice(), "write_i64_in of {value} after {lead} bytes");
                old.put_sleb(value);
                new.put_sleb(value);
                assert_eq!(new.as_slice(), old.as_slice(), "put_sleb on blaze_own::Vec of {value}");
            }
        }
    }
    let bytes: [u8; 6] = [0x00, 0x0f, 0xf0, 0x5a, 0xa5, 0xff];
    for len in 0..=bytes.len() {
        let old = hex::encode(&bytes[..len]);
        let new = hex::encode_in(&bytes[..len], alloc);
        assert_eq!(new.as_str(), old.as_str(), "hex::encode_in of {len} bytes");
        let mut appended = blaze_own::String::new_in(alloc);
        appended.push_str("x");
        hex::encode_into_in(&bytes[..len], &mut appended);
        assert_eq!(appended.as_str(), ["x", old.as_str()].concat(), "hex::encode_into_in of {len} bytes");
    }
}
