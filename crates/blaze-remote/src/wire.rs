//! Little-endian fields of the bucket's objects. Every decoder checks an object's whole length
//! before it reads a field, so these index without a further check.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_ids::ContentId;

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
