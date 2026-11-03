//! The two immediate encodings ARM uses in place of a plain constant field.

/// Encodes `v` as an A32 modified immediate, or `None` if it has no encoding.
///
/// A data-processing instruction has twelve bits for its immediate and spends
/// them on an 8-bit value plus a 4-bit rotation, giving `imm8 ROR (2 *
/// rotate)`. So the constant must be an eight-bit value sitting at an even bit
/// position — `0xff000000` yes, `0x000001ff` no — and the rotation may wrap
/// around the top of the word, which is what lets `0xf000000f` be encoded.
///
/// Rotations are tried smallest first, matching what GNU as and LLVM emit for
/// the constants that have more than one encoding (`4` is `4 ROR 0`, not
/// `1 ROR 30`).
pub fn modified(v: u32) -> Option<u32> {
    for rotate in 0..16u32 {
        // Rotating the value *left* undoes the encoding's rotate-right.
        let imm8 = v.rotate_left(2 * rotate);
        if imm8 <= 0xff {
            return Some((rotate << 8) | imm8);
        }
    }
    None
}

/// Encodes `v` as a T32 `ThumbExpandImm`, or `None`.
///
/// Thumb-2 also has twelve bits but spends them differently: four repeating
/// byte patterns for the constants that show up in bit manipulation, and
/// otherwise an 8-bit value with its top bit set, rotated right by any amount
/// from 8 to 31. The odd rotations that A32 cannot express are available here,
/// but `0x000001ff` still is not.
pub fn thumb_expand(v: u32) -> Option<u32> {
    if v <= 0xff {
        return Some(v);
    }
    // 0x00XY00XY, 0xXY00XY00 and 0xXYXYXYXY, with XY nonzero: the all-zero
    // spelling of each of these is architecturally unpredictable.
    let b0 = v & 0xff;
    let b1 = (v >> 8) & 0xff;
    if b0 != 0 && v == b0 | (b0 << 16) {
        return Some(0x100 | b0);
    }
    if b1 != 0 && v == (b1 << 8) | (b1 << 24) {
        return Some(0x200 | b1);
    }
    if b0 != 0 && v == b0 * 0x0101_0101 {
        return Some(0x300 | b0);
    }
    // An 8-bit value with bit 7 set, right-rotated. Because the top bit is
    // always set the eight significant bits sit immediately below the value's
    // highest one, so there is only ever one candidate to check.
    let msb = 31 - v.leading_zeros();
    if msb < 8 {
        return None;
    }
    let shift = msb - 7;
    if v & ((1 << shift) - 1) != 0 {
        return None;
    }
    let imm8 = v >> shift;
    let rotate = 32 - shift;
    Some((rotate << 7) | (imm8 & 0x7f))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modified_immediates_pick_the_smallest_rotation() {
        assert_eq!(modified(0), Some(0));
        assert_eq!(modified(4), Some(4));
        assert_eq!(modified(0xff), Some(0xff));
        // 0x1000 = 1 ROR 20, so rotate = 10.
        assert_eq!(modified(0x1000), Some((10 << 8) | 1));
        // 0xff000000 = 0xff ROR 8.
        assert_eq!(modified(0xff00_0000), Some((4 << 8) | 0xff));
        // A rotation that wraps around the top of the word.
        assert_eq!(modified(0xf000_000f), Some((2 << 8) | 0xff));
        assert_eq!(modified(0x1ff), None);
        assert_eq!(modified(0x101), None);
    }

    #[test]
    fn thumb_expand_covers_the_repeating_patterns() {
        assert_eq!(thumb_expand(0xab), Some(0xab));
        assert_eq!(thumb_expand(0x00ab_00ab), Some(0x1ab));
        assert_eq!(thumb_expand(0xab00_ab00), Some(0x2ab));
        assert_eq!(thumb_expand(0xabab_abab), Some(0x3ab));
        // 300 = 0x12c = 0x96 ROR 31.
        assert_eq!(thumb_expand(300), Some((31 << 7) | 0x16));
        assert_eq!(thumb_expand(0x1ff), None);
    }
}
