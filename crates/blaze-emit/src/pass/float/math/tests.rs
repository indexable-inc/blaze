//! The float functions' encodings (pass/float/math.rs) against the Arm ARM's (DDI 0487,
//! C7.2: FABS, FSQRT, FRINTN/P/M/Z/A, FMAXNM/FMINNM, FMAX/FMIN, FMADD; C6.2: UBFM as LSR, BFM
//! as BFI), each word the instruction's fixed bits with the fields named in its diagram.
//!
//! `encodings_match_the_arm_arm` prints "13 encodings match: frintn d0, d0 = 0x1e644000".

use super::{bfi1, fmadd, fone, lsr64};
use crate::enc;

#[test]
fn encodings_match_the_arm_arm() {
    let cases: [(&str, u32, u32); 13] = [
        ("fabs s0, s0", fone(0b000001, false, 0, 0), 0x1e20_c000),
        ("fsqrt d0, d0", fone(0b000011, true, 0, 0), 0x1e61_c000),
        ("frintn d0, d0", fone(0b001000, true, 0, 0), 0x1e64_4000),
        ("frintp d0, d0", fone(0b001001, true, 0, 0), 0x1e64_c000),
        ("frintm d0, d0", fone(0b001010, true, 0, 0), 0x1e65_4000),
        ("frintz d0, d0", fone(0b001011, true, 0, 0), 0x1e65_c000),
        ("frinta d0, d0", fone(0b001100, true, 0, 0), 0x1e66_4000),
        ("fmaxnm d0, d0, d1", enc::fbin(0b0110, true, 0, 0, 1), 0x1e61_6800),
        ("fminnm d0, d0, d1", enc::fbin(0b0111, true, 0, 0, 1), 0x1e61_7800),
        ("fmax d0, d0, d1", enc::fbin(0b0100, true, 0, 0, 1), 0x1e61_4800),
        ("fmadd d0, d0, d1, d2", fmadd(true, 0, 0, 1, 2), 0x1f41_0800),
        ("lsr x0, x0, #63", lsr64(0, 0, 63), 0xd37f_fc00),
        ("bfi x0, x1, #63, #1", bfi1(0, 1, 63), 0xb341_0020),
    ];
    for (name, got, want) in cases {
        assert_eq!(got, want, "{name}: {got:#010x}, the Arm ARM's {want:#010x}");
    }
    // FMIN is FMAX's opcode plus one; an `f32` FMADD clears the type bit.
    assert_eq!(enc::fbin(0b0101, true, 0, 0, 1), 0x1e61_5800);
    assert_eq!(fmadd(false, 0, 0, 1, 2), 0x1f01_0800);
    // `bfi x9, x10, #31, #1`: an `f32`'s sign bit (BFM immr 33).
    assert_eq!(bfi1(9, 10, 31), 0xb340_0000 | (33 << 16) | (10 << 5) | 9);
    println!("{} encodings match: frintn d0, d0 = {:#010x}", cases.len(), fone(0b001000, true, 0, 0));
}
