//! QUIC's variable-length integers (RFC 9000, 16), which HTTP/3 frames, stream headers,
//! settings and capsules are written in.

#![blaze::sans_io]

/// The largest value a variable-length integer holds.
pub(crate) const MAX: u64 = (1 << 62) - 1;

/// How many bytes `value` takes.
pub(crate) fn len(value: u64) -> usize {
    match value {
        0..=63 => 1,
        64..=16_383 => 2,
        16_384..=1_073_741_823 => 4,
        _ => 8,
    }
}

/// Appends `value`, which is at most [`MAX`]: every value the core writes is a `u32`, a stream
/// ID or a length, all below it.
pub(crate) fn put(out: &mut Vec<u8>, value: u64) {
    debug_assert!(value <= MAX, "{value} is past QUIC's variable-length integers");
    match len(value) {
        1 => out.push(value as u8),
        2 => out.extend_from_slice(&(0x4000 | value as u16).to_be_bytes()),
        4 => out.extend_from_slice(&(0x8000_0000 | value as u32).to_be_bytes()),
        _ => out.extend_from_slice(&(0xc000_0000_0000_0000 | (value & MAX)).to_be_bytes()),
    }
}

/// The integer at the start of `bytes` and how many bytes it took; `None` while it is not all
/// there.
pub(crate) fn get(bytes: &[u8]) -> Option<(u64, usize)> {
    let first = *bytes.first()?;
    let n = 1usize << (first >> 6);
    let raw = bytes.get(..n)?;
    let mut value = u64::from(first & 0x3f);
    for byte in &raw[1..] {
        value = (value << 8) | u64::from(*byte);
    }
    Some((value, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 9000, appendix A.1's examples.
    #[test]
    fn the_rfc_examples_round_trip() {
        let cases: [(&[u8], u64); 4] = [
            (&[0xc2, 0x19, 0x7c, 0x5e, 0xff, 0x14, 0xe8, 0x8c], 151_288_809_941_952_652),
            (&[0x9d, 0x7f, 0x3e, 0x7d], 494_878_333),
            (&[0x7b, 0xbd], 15_293),
            (&[0x25], 37),
        ];
        for (bytes, value) in cases {
            assert_eq!(get(bytes), Some((value, bytes.len())));
            let mut out = Vec::new();
            put(&mut out, value);
            assert_eq!(out, bytes);
        }
        assert_eq!(get(&[0x40, 0x25]), Some((37, 2)), "a longer encoding than needed still reads");
        assert_eq!(get(&[0x9d, 0x7f]), None, "a cut integer waits for the rest");
        assert_eq!(get(&[]), None);
    }
}
