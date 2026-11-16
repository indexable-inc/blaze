//! x-ref-07: a `str`'s length is in BYTES of its UTF-8 encoding, not in characters.
//!
//! The Reference, "Textual types": "A value of type `str` is represented the same way as `[u8]`,
//! a slice of 8-bit unsigned bytes" holding UTF-8; core's `str::len`: "Returns the length of
//! `self`. This length is in bytes, not `char`s or graphemes." UTF-8 (RFC 3629) encodes
//! U+0000..U+007F in 1 byte, U+0080..U+07FF in 2, U+0800..U+FFFF in 3, U+10000.. in 4.
//! 1. "a" (U+0061): 1.         2. "é" (U+00E9): 2.        3. "日本" (U+65E5, U+672C): 3 + 3 = 6.
//! 4. "aé日😀" (U+1F600 last): 1 + 2 + 3 + 4 = 10.
//! 5. the bytes of "é": U+00E9 = 000 1110 1001 -> 110_00011 10_101001 = 0xC3, 0xA9.
//! 6. a string with an escape: "\u{E9}" is the same text as "é": len 2, equal (`==`).
//! A wrong builder that stores the char count as the length exits 2 (1).

fn main() -> std::entry::Status {
    let e = "é";
    let b = e.as_bytes();
    let status = if "a".len() != 1 {
        1
    } else if e.len() != 2 {
        2
    } else if "日本".len() != 6 {
        3
    } else if "aé日😀".len() != 10 {
        4
    } else if b[0] != 0xC3 || b[1] != 0xA9 {
        5
    } else if "\u{E9}".len() != 2 || "\u{E9}" != e {
        6
    } else {
        0
    };
    std::entry::Status(status)
}
