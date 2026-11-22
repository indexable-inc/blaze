//! [`Put`] for blaze's own `blaze_own::Vec<u8, A>`, the buffer in the caller's allocator that
//! replaces `alloc`'s `Vec<u8>` (the bstd move): the same bytes as cursor.rs's impl for
//! `Vec<u8>`, field for field, so a writer moves by changing its buffer's type alone.

use core::alloc::Allocator;

use super::Put;
use crate::leb;

impl<A: Allocator> Put for blaze_own::Vec<u8, A> {
    #[inline]
    fn put_u8(&mut self, value: u8) {
        self.push(value);
    }

    #[inline]
    fn put_u16(&mut self, value: u16) {
        self.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    fn put_u32(&mut self, value: u32) {
        self.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    fn put_u64(&mut self, value: u64) {
        self.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    fn put_u128(&mut self, value: u128) {
        self.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    fn put_i8(&mut self, value: i8) {
        self.push(value as u8);
    }

    #[inline]
    fn put_i16(&mut self, value: i16) {
        self.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    fn put_i32(&mut self, value: i32) {
        self.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    fn put_i64(&mut self, value: i64) {
        self.extend_from_slice(&value.to_le_bytes());
    }

    #[inline]
    fn put_uleb(&mut self, value: u64) {
        leb::write_u64_in(self, value);
    }

    #[inline]
    fn put_sleb(&mut self, value: i64) {
        leb::write_i64_in(self, value);
    }

    #[inline]
    fn put_bytes(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }

    #[inline]
    fn put_bytes_u32(&mut self, bytes: &[u8]) {
        let len = u32::try_from(bytes.len()).expect("at most u32::MAX bytes behind a u32 length");
        self.put_u32(len);
        self.extend_from_slice(bytes);
    }

    #[inline]
    fn put_bytes_u64(&mut self, bytes: &[u8]) {
        self.put_u64(bytes.len() as u64);
        self.extend_from_slice(bytes);
    }

    #[inline]
    fn put_bytes_uleb(&mut self, bytes: &[u8]) {
        leb::write_u64_in(self, bytes.len() as u64);
        self.extend_from_slice(bytes);
    }

    #[inline]
    fn put_cstr(&mut self, bytes: &[u8]) {
        assert!(!bytes.contains(&0), "a C string holding a NUL");
        self.extend_from_slice(bytes);
        self.push(0);
    }

    #[inline]
    fn pad_to(&mut self, multiple: usize) {
        self.resize(self.len().next_multiple_of(multiple), 0);
    }

    #[inline]
    fn patch_u32(&mut self, at: usize, value: u32) {
        self[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    #[inline]
    fn patch_u64(&mut self, at: usize, value: u64) {
        self[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
}
