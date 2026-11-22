//! The crate's promises, each with its oracle and, for the stateful parts, a planted bug:
//!
//! - `hpack_rfc`: every example of RFC 7541 Appendix C, byte for byte, with the dynamic table's
//!   state after each; the Huffman code complete and prefix-free; the static table's entries.
//! - `hpack_props`: for generated field lists, encode then decode is the identity and the two
//!   dynamic tables stay equal (plant: the decoder's eviction off by one).
//! - `frames`: the frame header's layout, the bytes our preface makes, the stream state table.
//! - `conn_props`: a client and a server wired back to back deliver every body byte once and in
//!   order, whatever the windows, frame sizes and the cuts in the bytes, and end with no stream
//!   left; no window leaves 0..=2^31-1 (plant: a WINDOW_UPDATE applied to the wrong stream).
//! - `attacks`: rapid reset, the CONTINUATION flood, the SETTINGS and PING floods, the zero-window
//!   stall, the header list bomb: each ends in its typed error within its cap.

use std::{string::String, vec::Vec as StdVec};

use blaze_own::{AVec, Alloc};

mod attacks;
mod conn_props;
mod frames;
mod hpack_props;
mod hpack_rfc;

/// The allocator every test passes in.
pub(crate) fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// The bytes a hex string names; spaces are ignored.
pub(crate) fn hex(text: &str) -> StdVec<u8> {
    let digits: StdVec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    assert!(digits.len() % 2 == 0, "odd hex string `{text}`");
    let nibble = |d: u8| match d {
        b'0'..=b'9' => d - b'0',
        b'a'..=b'f' => d - b'a' + 10,
        b'A'..=b'F' => d - b'A' + 10,
        _ => panic!("not hex: `{}` in `{text}`", char::from(d)),
    };
    digits.chunks(2).map(|pair| nibble(pair[0]) << 4 | nibble(pair[1])).collect()
}

/// The bytes of `pieces`, one after another.
pub(crate) fn cat(pieces: &[&[u8]]) -> StdVec<u8> {
    pieces.concat()
}

/// An AVec's bytes as a std vector, for comparing.
pub(crate) fn bytes(v: &AVec<'_, u8>) -> StdVec<u8> {
    v.as_slice().to_vec()
}

/// A field list as text, for messages.
pub(crate) fn show(fields: &[(StdVec<u8>, StdVec<u8>)]) -> String {
    let mut out = String::new();
    for (name, value) in fields {
        out.push_str(&String::from_utf8_lossy(name));
        out.push_str(": ");
        out.push_str(&String::from_utf8_lossy(value));
        out.push('\n');
    }
    out
}
