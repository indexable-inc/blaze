//! Little- and big-endian fields appended to a file being written, and set at an offset once
//! their value is known. The file formats' fields are little-endian except the code signature's,
//! which `<kern/cs_blobs.h>` defines big-endian.

use blaze_own::AVec;

pub fn u8(out: &mut AVec<'_, u8>, value: u8) {
    out.push(value);
}

pub fn u16(out: &mut AVec<'_, u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn u32(out: &mut AVec<'_, u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn u64(out: &mut AVec<'_, u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn be32(out: &mut AVec<'_, u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

pub fn be64(out: &mut AVec<'_, u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// A fixed-length name field (a segment's or section's 16 bytes), zero-padded.
pub fn name16(out: &mut AVec<'_, u8>, name: &str) {
    let mut field = [0u8; 16];
    field[..name.len()].copy_from_slice(name.as_bytes());
    out.extend_from_slice(&field);
}

/// Zeros until `out.len()` is a multiple of `align` (a power of two).
pub fn pad(out: &mut AVec<'_, u8>, align: usize) {
    let len = (out.len() + align - 1) & !(align - 1);
    out.resize(len, 0);
}

pub fn set32(out: &mut [u8], at: usize, value: u32) {
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub fn set64(out: &mut [u8], at: usize, value: u64) {
    out[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

pub fn get16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

pub fn get32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

pub fn get64(bytes: &[u8], at: usize) -> u64 {
    u64::from(get32(bytes, at)) | (u64::from(get32(bytes, at + 4)) << 32)
}

pub fn get_be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

pub fn get_be64(bytes: &[u8], at: usize) -> u64 {
    (u64::from(get_be32(bytes, at)) << 32) | u64::from(get_be32(bytes, at + 4))
}

/// An unsigned LEB128 (`LC_FUNCTION_STARTS`' deltas).
pub fn uleb(out: &mut AVec<'_, u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}
