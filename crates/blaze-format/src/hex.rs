//! Hexadecimal, the one implementation: digests, checksums, tokens and keys written as text
//! (`Cargo.lock`'s and `.crate` checksums, a webhook's signature, the global cache's bundle
//! checksums and publisher keys, the proc-macro module files' digests).
//!
//! [`encode`] writes lowercase, two digits a byte, high nibble first. [`decode`] reads either
//! case, as cargo's `hex` crate does a checksum; [`decode_lower`] reads only what [`encode`]
//! writes, for a token compared as text. Both take exactly `2 * N` digits and nothing else (no
//! sign, prefix or space).
//!
//! Every caller writes or reads at most a few hundred bytes at once, off any inner loop, so a
//! table lookup per digit is the bound: the decode looks both digits of a byte up in one
//! 256-entry table and checks for a bad digit once, at the end.

const DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Not a digit, in [`NIBBLES`] and [`LOWER_NIBBLES`]: any value above 15 would do.
const BAD: u8 = 0x10;

/// The value of each byte as a hex digit of either case, or [`BAD`].
const NIBBLES: [u8; 256] = nibbles(true);
/// The value of each byte as a lowercase hex digit, or [`BAD`].
const LOWER_NIBBLES: [u8; 256] = nibbles(false);

const fn nibbles(upper: bool) -> [u8; 256] {
    let mut table = [BAD; 256];
    let mut at = 0;
    while at < 10 {
        table[b'0' as usize + at] = at as u8;
        at += 1;
    }
    let mut at = 0;
    while at < 6 {
        table[b'a' as usize + at] = 10 + at as u8;
        if upper {
            table[b'A' as usize + at] = 10 + at as u8;
        }
        at += 1;
    }
    table
}

/// `bytes` in lowercase hex.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 * bytes.len());
    encode_into(bytes, &mut out);
    out
}

/// Appends `bytes` to `out` in lowercase hex.
pub fn encode_into(bytes: &[u8], out: &mut String) {
    out.reserve(2 * bytes.len());
    for &byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
}

/// Whether `text` is what [`encode`] writes of `bytes`, compared without writing it.
pub fn is_encoding(text: &str, bytes: &[u8]) -> bool {
    text.len() == 2 * bytes.len()
        && text.as_bytes().as_chunks::<2>().0.iter().zip(bytes).all(|(pair, &byte)| {
            *pair == [DIGITS[usize::from(byte >> 4)], DIGITS[usize::from(byte & 15)]]
        })
}

/// The `N` bytes `text` writes in hex of either case: `None` unless it is exactly `2 * N`
/// digits.
pub fn decode<const N: usize>(text: impl AsRef<[u8]>) -> Option<[u8; N]> {
    decode_with(&NIBBLES, text.as_ref())
}

/// The `N` bytes `text` writes in lowercase hex: `None` unless it is exactly `2 * N` lowercase
/// digits.
pub fn decode_lower<const N: usize>(text: impl AsRef<[u8]>) -> Option<[u8; N]> {
    decode_with(&LOWER_NIBBLES, text.as_ref())
}

fn decode_with<const N: usize>(table: &[u8; 256], text: &[u8]) -> Option<[u8; N]> {
    if text.len() != 2 * N {
        return None;
    }
    let mut out = [0; N];
    let mut bad = 0;
    for (byte, pair) in out.iter_mut().zip(text.as_chunks::<2>().0) {
        let (high, low) = (table[usize::from(pair[0])], table[usize::from(pair[1])]);
        bad |= high | low;
        *byte = (high << 4) | (low & 15);
    }
    (bad & BAD == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use hegel::TestCase;
    use hegel::generators as gs;

    use super::*;

    /// blaze-registry's `hex` as it was, the reference encoder.
    fn encode_reference(bytes: &[u8]) -> String {
        bytes.iter().map(|it| format!("{it:02x}")).collect()
    }

    /// blaze-global's `bundle::unhex` as it was (`to_digit(16)`, either case), the reference
    /// decoder.
    fn decode_reference<const N: usize>(text: &[u8]) -> Option<[u8; N]> {
        if text.len() != 2 * N {
            return None;
        }
        let digit = |it: u8| (it as char).to_digit(16).map(|it| it as u8);
        let mut out = [0; N];
        for (at, pair) in text.chunks_exact(2).enumerate() {
            out[at] = (digit(pair[0])? << 4) | digit(pair[1])?;
        }
        Some(out)
    }

    /// blaze-inproc's `handshake::unhex` as it was (lowercase only), the reference for
    /// [`decode_lower`].
    fn decode_lower_reference<const N: usize>(text: &[u8]) -> Option<[u8; N]> {
        if text.len() != 2 * N {
            return None;
        }
        let digit = |it: u8| match it {
            b'0'..=b'9' => Some(it - b'0'),
            b'a'..=b'f' => Some(it - b'a' + 10),
            _ => None,
        };
        let mut out = [0; N];
        for (index, pair) in text.chunks_exact(2).enumerate() {
            out[index] = (digit(pair[0])? << 4) | digit(pair[1])?;
        }
        Some(out)
    }

    #[test]
    fn a_digest_is_written_as_its_hexadecimal() {
        let mut out = String::new();
        encode_into(&[0x00, 0x0f, 0xa5, 0xff], &mut out);
        assert_eq!(out, "000fa5ff");
        assert_eq!(encode(&[0x00, 0xab, 0x7f]), "00ab7f");
        assert!(is_encoding("000fa5ff", &[0x00, 0x0f, 0xa5, 0xff]));
        assert!(!is_encoding("000FA5FF", &[0x00, 0x0f, 0xa5, 0xff]));
        assert!(!is_encoding("0fa5ff", &[0x00, 0x0f, 0xa5, 0xff]));
        assert!(!is_encoding("000fa5fe", &[0x00, 0x0f, 0xa5, 0xff]));
    }

    #[test]
    fn a_token_reads_back_only_as_written() {
        let token: [u8; 16] = core::array::from_fn(|it| it as u8);
        assert_eq!(decode_lower(b"000102030405060708090a0b0c0d0e0f"), Some(token));
        assert_eq!(decode_lower::<16>(b"000102030405060708090a0b0c0d0e0"), None);
        assert_eq!(decode_lower::<16>(b"000102030405060708090a0b0c0d0eFF"), None);
        assert_eq!(decode_lower::<16>(b"zz0102030405060708090a0b0c0d0e0f"), None);
        assert_eq!(decode::<16>(b"000102030405060708090a0b0c0d0eFF").map(|it| it[15]), Some(0xff));
    }

    #[test]
    fn a_checksum_is_its_digits_and_nothing_else() {
        let checksum = "75".repeat(32);
        assert_eq!(decode::<32>(&checksum), Some([0x75; 32]));
        assert_eq!(decode::<32>(&checksum[1..]), None);
        // `u8::from_str_radix`, which two of the copies used, also took `+5` for a byte.
        assert_eq!(decode::<32>(format!("+5{}", &checksum[2..])), None);
        assert_eq!(decode::<32>(format!(" 5{}", &checksum[2..])), None);
        assert_eq!(decode::<1>("g0"), None);
    }

    #[test]
    fn writing_and_reading_allocate_only_the_text() {
        let bytes = [0xa5; 32];
        let mut out = String::with_capacity(64);
        blaze_alloc_count::assert_at_most(0, "hex into a buffer with room, and a decode", || {
            encode_into(&bytes, &mut out);
            assert_eq!(decode::<32>(&out), Some(bytes));
            assert!(is_encoding(&out, &bytes));
        });
    }

    #[hegel::test]
    fn encodes_as_the_copies_did(tc: TestCase) {
        let bytes: Vec<u8> = tc.draw(gs::binary().max_size(79));
        let text = encode(&bytes);
        assert_eq!(&text, &encode_reference(&bytes));
        assert!(is_encoding(&text, &bytes));
        let mut fixed = [0u8; 8];
        let n = bytes.len().min(8);
        fixed[..n].copy_from_slice(&bytes[..n]);
        assert_eq!(decode::<8>(encode(&fixed)), Some(fixed));
        assert_eq!(decode_lower::<8>(encode(&fixed)), Some(fixed));
    }

    #[hegel::test]
    fn decodes_as_the_copies_did(tc: TestCase) {
        let text: Vec<u8> = tc.draw(gs::vecs(hegel::one_of!(
            gs::sampled_from(b"09afAFg+".to_vec()),
            gs::integers::<u8>(),
        )).max_size(19));
        assert_eq!(decode::<8>(&text), decode_reference::<8>(&text));
        assert_eq!(decode_lower::<8>(&text), decode_lower_reference::<8>(&text));
        assert_eq!(decode::<4>(&text), decode_reference::<4>(&text));
    }
}
