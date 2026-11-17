//! The words of the indexed thread-local access (pass/tls_index.rs), each against a word
//! written by hand from its encoding diagram in the Arm Architecture Reference Manual (DDI 0487),
//! chapter C6 ("A64 Base Instruction Descriptions"). Each expected word is spelled as binary, one
//! `_`-separated group per field of the diagram, the assembly text in the comment beside it; none
//! came from an assembler. The two new forms are enc_tls.rs's; the rest are enc.rs's forms at the
//! registers the sequence uses.
//!
//! Prints nothing; passes as `test enc_tls_tests::the_indexed_access_words ... ok`.

use crate::{
    enc::{self, Cond, Size},
    enc_tls,
};

#[test]
fn the_indexed_access_words() {
    let cases: &[(u32, u32, &str)] = &[
        // C6.2 MRS: 1101010100 L=1 1 o0=1 op1=011 CRn=1101 CRm=0000 op2=010 Rt (TPIDR_EL0).
        (enc_tls::mrs_tpidr_el0(11), 0b1101010100_1_1_1_011_1101_0000_010_01011, "mrs x11, tpidr_el0"),
        (enc_tls::mrs_tpidr_el0(0), 0b1101010100_1_1_1_011_1101_0000_010_00000, "mrs x0, tpidr_el0"),
        // C6.2 MRS, TPIDRRO_EL0: op2 = 011 (the macOS form, beside it for contrast).
        (enc::mrs_tpidrro_el0(11), 0b1101010100_1_1_1_011_1101_0000_011_01011, "mrs x11, tpidrro_el0"),
        // C6.2 AND (immediate): sf=1 opc=00 100100 N=1 immr=000000 imms=111110 Rn Rd.
        (enc_tls::and_clear_bit63(10, 10), 0b1_00_100100_1_000000_111110_01010_01010, "and x10, x10, #0x7fffffffffffffff"),
        // C6.2 CMP (shifted register), an alias of SUBS: sf=1 op=1 S=1 01011 shift=00 0 Rm imm6 Rn Rd=11111.
        (enc::cmp(13, 14), 0b1_1_1_01011_00_0_01110_000000_01101_11111, "cmp x13, x14"),
        // C6.2 B.cond: 0101010 0 imm19 0 cond (HS = CS = 0010), 6 words ahead.
        (enc::b_cond(Cond::Hs, 6), 0b0101010_0_0000000000000000110_0_0010, "b.hs .+24"),
        // C6.2 CBZ: sf=1 011010 op=0 imm19 Rt, 5 words ahead.
        (enc::cbz(12, 5), 0b1_011010_0_0000000000000000101_01100, "cbz x12, .+20"),
        // C6.2 CBNZ: op = 1.
        (enc::cbnz(0, 7), 0b1_011010_1_0000000000000000111_00000, "cbnz x0, .+28"),
        // C6.2 LDR (register): size=11 111 V=0 00 opc=01 1 Rm option=011 S=1 10 Rn Rt.
        (enc::ldr_lsl3(0, 14, 13), 0b11_111_0_00_01_1_01101_011_1_10_01110_00000, "ldr x0, [x14, x13, lsl #3]"),
        // C6.2 LDR (immediate, unsigned offset): size=11 111 V=0 01 opc=01 imm12 Rn Rt.
        (enc::ldr(Size::X, 10, 9, 1), 0b11_111_0_01_01_000000000001_01001_01010, "ldr x10, [x9, #8]"),
        (enc::ldr(Size::X, 12, 11, 0), 0b11_111_0_01_01_000000000000_01011_01100, "ldr x12, [x11]"),
        // C6.2 ADD (immediate): sf=1 op=0 S=0 100010 sh=0 imm12 Rn Rd.
        (enc::add_imm(14, 12, 32, false), 0b1_0_0_100010_0_000000100000_01100_01110, "add x14, x12, #32"),
        // C6.2 ADD (shifted register): sf=1 op=0 S=0 01011 shift=00 0 Rm imm6 Rn Rd.
        (enc::add(11, 11, 10), 0b1_0_0_01011_00_0_01010_000000_01011_01011, "add x11, x11, x10"),
    ];
    for &(got, want, text) in cases {
        assert_eq!(got, want, "{text}: got {got:#010x}, the diagram gives {want:#010x}");
    }
}
