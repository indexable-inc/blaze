//! LEB128 as the wasm binary format writes numbers, the 5-byte padded form an object leaves
//! where the linker writes an index or an address, and the framing of sections and names.

pub(super) use blaze_format::leb::{
    PADDED, len_u64 as uleb_len, padded_i32 as padded_sleb, padded_u32 as padded_uleb,
    put_i64 as sleb_into, put_u64 as uleb_into, write_i64 as sleb, write_u64 as uleb,
};
use blaze_format::cursor::Put;

pub(super) fn name(out: &mut Vec<u8>, name: &str) {
    out.put_bytes_uleb(name.as_bytes());
}

/// A section: its id, its size, then `contents`.
pub(super) fn section(out: &mut Vec<u8>, id: u8, contents: &[u8]) {
    out.put_u8(id);
    out.put_bytes_uleb(contents);
}

/// A custom section: id 0, its size, its name, then `payload`.
pub(super) fn custom(out: &mut Vec<u8>, custom: &str, payload: &[u8]) {
    out.push(0);
    uleb(out, (uleb_len(custom.len() as u64) + custom.len() + payload.len()) as u64);
    name(out, custom);
    out.extend_from_slice(payload);
}

/// `i32.const value; end`: a constant expression.
pub(super) fn i32_const(out: &mut Vec<u8>, value: i32) {
    out.push(0x41);
    sleb(out, value.into());
    out.push(0x0b);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wasm::reader::Reader;

    fn u(value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        uleb(&mut out, value);
        out
    }

    fn s(value: i64) -> Vec<u8> {
        let mut out = Vec::new();
        sleb(&mut out, value);
        out
    }

    #[test]
    fn round_trips() {
        let unsigned = [0, 1, 127, 128, 300, 65_535, 1 << 20, u32::MAX];
        for value in unsigned {
            assert_eq!(Reader::new(&u(value.into())).uleb32().unwrap(), value);
            assert_eq!(Reader::new(&padded_uleb(value)).uleb32().unwrap(), value);
        }
        let signed = [0, 1, -1, 63, 64, -64, -65, 1 << 20, i32::MIN, i32::MAX];
        for value in signed {
            assert_eq!(Reader::new(&s(value.into())).sleb32().unwrap(), value);
            assert_eq!(Reader::new(&padded_sleb(value)).sleb32().unwrap(), value);
        }
    }

    #[test]
    fn writes_into_room_allocate_nothing() {
        let mut out = Vec::with_capacity(64);
        let mut place = [0; 10];
        let written = blaze_alloc_count::assert_at_most(0, "LEB128 writes into room", || {
            uleb(&mut out, 624_485);
            sleb(&mut out, -123_456);
            i32_const(&mut out, -2);
            name(&mut out, "ab");
            (padded_uleb(5), padded_sleb(-1), uleb_into(&mut place, u64::MAX))
        });
        assert_eq!(written, (padded_uleb(5), padded_sleb(-1), 10));
        assert_eq!(out, [0xe5, 0x8e, 0x26, 0xc0, 0xbb, 0x78, 0x41, 0x7e, 0x0b, 2, b'a', b'b']);
    }

    #[test]
    fn framing() {
        let mut out = Vec::new();
        custom(&mut out, "ab", &[7]);
        assert_eq!(out, [0, 4, 2, b'a', b'b', 7]);
        let mut out = Vec::new();
        i32_const(&mut out, -2);
        assert_eq!(out, [0x41, 0x7e, 0x0b]);
    }
}
