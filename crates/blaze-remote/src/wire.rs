//! Little-endian fields of the bucket's objects. Every decoder checks an object's whole length
//! before it reads a field, so these index without a further check.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_ids::ContentId;

use crate::trust::Why;

pub(crate) fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

pub(crate) fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

pub(crate) fn id_at(bytes: &[u8], at: usize) -> ContentId {
    ContentId(bytes[at..at + 32].try_into().expect("32 bytes"))
}

pub(crate) fn key_at(bytes: &[u8], at: usize) -> [u8; 32] {
    bytes[at..at + 32].try_into().expect("32 bytes")
}

pub(crate) fn sig_at(bytes: &[u8], at: usize) -> [u8; 64] {
    bytes[at..at + 64].try_into().expect("64 bytes")
}

pub(crate) fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(crate) fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(crate) fn hex(id: &ContentId) -> String {
    blaze_format::hex::encode(&id.0)
}

/// A list of at most `MOST` items: the one bound an encoder and its decoder share. A value
/// that a decoder would refuse cannot be built (rule 76, correct by construction), so every
/// value that exists round-trips. Read as a slice.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Bounded<T, const MOST: usize>(Vec<T>);

impl<T, const MOST: usize> Bounded<T, MOST> {
    /// `items`, or [`Why::TooMany`] when there are more than `MOST`.
    pub fn new(items: Vec<T>) -> Result<Bounded<T, MOST>, Why> {
        if items.len() > MOST {
            return Err(Why::TooMany { count: items.len() as u64, most: MOST as u64 });
        }
        Ok(Bounded(items))
    }

    /// No items.
    pub fn empty() -> Bounded<T, MOST> {
        Bounded(Vec::new())
    }

    pub fn into_vec(self) -> Vec<T> {
        self.0
    }
}

impl<T, const MOST: usize> std::ops::Deref for Bounded<T, MOST> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<'a, T, const MOST: usize> IntoIterator for &'a Bounded<T, MOST> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> std::slice::Iter<'a, T> {
        self.0.iter()
    }
}
