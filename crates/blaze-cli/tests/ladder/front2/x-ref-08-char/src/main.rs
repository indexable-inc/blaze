//! x-ref-08: a `char` is a Unicode scalar value; `as u32` gives it, `u8 as char` makes one.
//!
//! The Reference, "Textual types": "The type `char` is a Unicode scalar value"; "Type cast
//! expressions": `char` to integer is a "Primitive to integer cast" of the scalar value;
//! "u8 to char cast": "Casts to the char with the corresponding code point." core's
//! `char::from_u32`: "Returns `None` if the input is not a valid `char`" (surrogates
//! 0xD800..=0xDFFF and values above 0x10FFFF are not).
//! 1. 'A' as u32 = 65.     2. 'é' as u32 = 0xE9 = 233.     3. '😀' as u32 = 0x1F600 = 128512.
//! 4. '\u{10FFFF}' as u32 = 1114111.
//! 5. 97u8 as char == 'a'; 0xE9u8 as char == 'é'.
//! 6. from_u32(0xD800) is None; from_u32(0x41) is Some('A'); `len_utf8` of '😀' is 4.
//! 7. 'é' as u8 truncates the scalar 0xE9 to its low byte: 0xE9.
//! A wrong builder that casts the UTF-8 bytes instead of the scalar exits 2 (0xC3A9).

fn code(c: char) -> u32 {
    c as u32
}

fn main() -> std::entry::Status {
    let status = if code('A') != 65 {
        1
    } else if code('é') != 233 {
        2
    } else if code('😀') != 128512 {
        3
    } else if code('\u{10FFFF}') != 1114111 {
        4
    } else if 97u8 as char != 'a' || 0xE9u8 as char != 'é' {
        5
    } else if char::from_u32(0xD800).is_some() || char::from_u32(0x41) != Some('A') || '😀'.len_utf8() != 4 {
        6
    } else if 'é' as u8 != 0xE9 {
        7
    } else {
        0
    };
    std::entry::Status(status)
}
