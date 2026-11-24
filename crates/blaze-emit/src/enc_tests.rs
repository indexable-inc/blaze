//! The instruction forms added for pairs, bit operations, atomics, bulk memory and
//! thread-locals, each against a word written by hand from its encoding diagram in the Arm
//! Architecture Reference Manual (DDI 0487), chapter C6 ("A64 Base Instruction Descriptions",
//! C6.2 the alphabetical list) and C7 for the SIMD forms. Each expected word is spelled as
//! binary, one `_`-separated group per field of the diagram, named in the comment beside it;
//! none came from an assembler.

use crate::enc::{self, Barrier, Cond, Lse, Size};

#[test]
fn new_encodings() {
    let cases: &[(u32, u32, &str)] = &[
        // C6.2 BL: 1 00101 imm26.
        (enc::bl(0), 0b1_00101_00000000000000000000000000, "bl #0"),
        (enc::bl(4), 0b1_00101_00000000000000000000000100, "bl #+16"),
        // C6.2 BIC / ORN / EON (shifted register, N=1): sf opc 01010 shift N=1 Rm imm6 Rn Rd.
        (enc::bic(0, 1, 2), 0b1_00_01010_00_1_00010_000000_00001_00000, "bic x0, x1, x2"),
        (enc::orn(0, 1, 2), 0b1_01_01010_00_1_00010_000000_00001_00000, "orn x0, x1, x2"),
        (enc::eon(0, 1, 2), 0b1_10_01010_00_1_00010_000000_00001_00000, "eon x0, x1, x2"),
        // C6.2 EOR (immediate): sf=1 opc=10 100100 N:immr:imms Rn Rd.
        (enc::eor_imm_nrs(0, 1, (1 << 12) | 0), 0b1_10_100100_1_000000_000000_00001_00000, "eor x0, x1, #1"),
        // C6.2 EXTR: sf 00 100111 N 0 Rm imms Rn Rd.
        (enc::extr(0, 1, 1, 7), 0b1_00_100111_1_0_00001_000111_00001_00000, "ror x0, x1, #7"),
        (enc::extr_w(0, 1, 1, 5), 0b0_00_100111_0_0_00001_000101_00001_00000, "ror w0, w1, #5"),
        // C6.2 CSINC / CSINV / CSNEG: sf op 0 11010100 Rm cond 0 o2 Rn Rd.
        (enc::csinc(0, 1, 2, Cond::Eq), 0b1_0_0_11010100_00010_0000_0_1_00001_00000, "csinc x0, x1, x2, eq"),
        (enc::csinv(0, 1, 2, Cond::Ne), 0b1_1_0_11010100_00010_0001_0_0_00001_00000, "csinv x0, x1, x2, ne"),
        (enc::csneg(0, 1, 2, Cond::Ge), 0b1_1_0_11010100_00010_1010_0_1_00001_00000, "csneg x0, x1, x2, ge"),
        // C6.2 STP / LDP (signed offset): 10 101 0 010 L imm7 Rt2 Rn Rt.
        (enc::stp_off(0, 1, 2, 16), 0b10_101_0_010_0_0000010_00001_00010_00000, "stp x0, x1, [x2, #16]"),
        (enc::ldp_off(0, 1, 2, 16), 0b10_101_0_010_1_0000010_00001_00010_00000, "ldp x0, x1, [x2, #16]"),
        // C6.2 LDR/STR/LDRSW (register, scaled): size 111 0 00 opc 1 Rm 011 1 10 Rn Rt.
        (enc::ldr_reg_scaled(Size::X, 0, 1, 2), 0b11_111_0_00_01_1_00010_011_1_10_00001_00000, "ldr x0, [x1, x2, lsl #3]"),
        (enc::str_reg_scaled(Size::W, 0, 1, 2), 0b10_111_0_00_00_1_00010_011_1_10_00001_00000, "str w0, [x1, x2, lsl #2]"),
        (enc::ldrsw_reg_scaled(0, 1, 2), 0b10_111_0_00_10_1_00010_011_1_10_00001_00000, "ldrsw x0, [x1, x2, lsl #2]"),
        // C6.2 TBZ / TBNZ: b5 011011 op b40 imm14 Rt.
        (enc::tbz(0, 3, 2), 0b0_011011_0_00011_00000000000010_00000, "tbz w0, #3, #+8"),
        (enc::tbnz(1, 35, 4), 0b1_011011_1_00011_00000000000100_00001, "tbnz x1, #35, #+16"),
        // C6.2 UMULL / SMULL.
        (enc::umull(0, 1, 2), 0b1_00_11011_1_01_00010_0_11111_00001_00000, "umull x0, w1, w2"),
        (enc::smull(0, 1, 2), 0b1_00_11011_0_01_00010_0_11111_00001_00000, "smull x0, w1, w2"),
        // C6.2 ADDS (shifted register): sf op S 01011 shift 0 Rm imm6 Rn Rd.
        (enc::adds(0, 1, 2), 0b1_0_1_01011_00_0_00010_000000_00001_00000, "adds x0, x1, x2"),
        // C6.2 SUBS (shifted register): op = 1.
        (enc::subs(0, 1, 2), 0b1_1_1_01011_00_0_00010_000000_00001_00000, "subs x0, x1, x2"),
        // C6.2 UMULH: sf 00 11011 U=1 10 Rm o0=0 Ra=11111 Rn Rd.
        (enc::umulh(0, 1, 2), 0b1_00_11011_1_10_00010_0_11111_00001_00000, "umulh x0, x1, x2"),
        // C6.2 SMULH: U = 0.
        (enc::smulh(0, 1, 2), 0b1_00_11011_0_10_00010_0_11111_00001_00000, "smulh x0, x1, x2"),
        // C6.2 ASR (immediate), an alias of SBFM: sf opc=00 100110 N immr=shift imms=63 Rn Rd.
        (enc::asr_imm(0, 1, 63), 0b1_00_100110_1_111111_111111_00001_00000, "asr x0, x1, #63"),
        (enc::asr_imm(9, 10, 5), 0b1_00_100110_1_000101_111111_01010_01001, "asr x9, x10, #5"),
        // C6.2 RORV: sf=0 0 S=0 11010110 Rm 0010 op2=11 Rn Rd.
        (enc::rorv_w(0, 1, 2), 0b0_0_0_11010110_00010_0010_11_00001_00000, "ror w0, w1, w2"),
        // C6.2 CLZ / RBIT / REV16 / REV: sf 1 S=0 11010110 opcode2=00000 opcode Rn Rd.
        (enc::clz(0, 1), 0b1_1_0_11010110_00000_000100_00001_00000, "clz x0, x1"),
        (enc::rbit(0, 1), 0b1_1_0_11010110_00000_000000_00001_00000, "rbit x0, x1"),
        (enc::rev16_w(0, 1), 0b0_1_0_11010110_00000_000001_00001_00000, "rev16 w0, w1"),
        (enc::rev_w(0, 1), 0b0_1_0_11010110_00000_000010_00001_00000, "rev w0, w1"),
        (enc::rev(0, 1), 0b1_1_0_11010110_00000_000011_00001_00000, "rev x0, x1"),
        // C6.2 AND (immediate): sf opc=00 100100 N immr imms Rn Rd; ~7 is 61 ones (imms = 60)
        // rotated right by 61.
        (enc::and_imm(11, 11, 1, 61, 60), 0b1_00_100100_1_111101_111100_01011_01011, "and x11, x11, #~7"),
        // C6.2 ORR (immediate): opc = 01; one bit (imms = 0) rotated right by 64 - bit.
        (enc::orr_imm(9, 9, 1, 56, 0), 0b1_01_100100_1_111000_000000_01001_01001, "orr x9, x9, #0x100"),
        (enc::orr_bit(9, 9, 8), 0b1_01_100100_1_111000_000000_01001_01001, "orr x9, x9, #0x100"),
        (enc::orr_bit(9, 9, 32), 0b1_01_100100_1_100000_000000_01001_01001, "orr x9, x9, #1 << 32"),
        // C7.2 FMOV (general): sf 0 S=0 11110 ftype=01 1 rmode=00 opcode Rn Rd; 111 to D, 110 from.
        (enc::fmov_to_d(0, 1), 0b1_0_0_11110_01_1_00_111_000000_00001_00000, "fmov d0, x1"),
        (enc::fmov_from_d(0, 1), 0b1_0_0_11110_01_1_00_110_000000_00001_00000, "fmov x0, d1"),
        // C7.2 CNT: 0 Q=0 U=0 01110 size=00 10000 00101 10 Rn Rd.
        (enc::cnt_8b(0, 1), 0b0_0_0_01110_00_10000_00101_10_00001_00000, "cnt v0.8b, v1.8b"),
        // C7.2 ADDV: 0 Q=0 U=0 01110 size=00 11000 11011 10 Rn Rd.
        (enc::addv_8b(0, 1), 0b0_0_0_01110_00_11000_11011_10_00001_00000, "addv b0, v1.8b"),
        // C6.2 LDR (register): size=11 111 V=0 00 opc=01 1 Rm option=011 S=1 10 Rn Rt.
        (enc::ldr_lsl3(0, 11, 10), 0b11_111_0_00_01_1_01010_011_1_10_01011_00000, "ldr x0, [x11, x10, lsl #3]"),
        // C6.2 LDAR / STLR: size 001000 o2=1 L o1=0 Rs=11111 o0=1 Rt2=11111 Rn Rt.
        (enc::ldar(Size::X, 0, 1), 0b11_001000_1_1_0_11111_1_11111_00001_00000, "ldar x0, [x1]"),
        (enc::ldar(Size::B, 0, 1), 0b00_001000_1_1_0_11111_1_11111_00001_00000, "ldarb w0, [x1]"),
        (enc::stlr(Size::X, 0, 1), 0b11_001000_1_0_0_11111_1_11111_00001_00000, "stlr x0, [x1]"),
        (enc::stlr(Size::H, 0, 1), 0b01_001000_1_0_0_11111_1_11111_00001_00000, "stlrh w0, [x1]"),
        // C6.2 LDXR / LDAXR: size 001000 o2=0 L=1 o1=0 Rs=11111 o0 Rt2=11111 Rn Rt.
        (enc::ldxr(Size::X, false, 0, 1), 0b11_001000_0_1_0_11111_0_11111_00001_00000, "ldxr x0, [x1]"),
        (enc::ldxr(Size::X, true, 0, 1), 0b11_001000_0_1_0_11111_1_11111_00001_00000, "ldaxr x0, [x1]"),
        (enc::ldxr(Size::H, true, 3, 4), 0b01_001000_0_1_0_11111_1_11111_00100_00011, "ldaxrh w3, [x4]"),
        // C6.2 STXR / STLXR: L=0, Rs the status register.
        (enc::stxr(Size::X, false, 2, 0, 1), 0b11_001000_0_0_0_00010_0_11111_00001_00000, "stxr w2, x0, [x1]"),
        (enc::stxr(Size::X, true, 2, 0, 1), 0b11_001000_0_0_0_00010_1_11111_00001_00000, "stlxr w2, x0, [x1]"),
        (enc::stxr(Size::B, true, 13, 12, 9), 0b00_001000_0_0_0_01101_1_11111_01001_01100, "stlxrb w13, w12, [x9]"),
        // C6.2 CLREX: 1101010100 0 00 011 0011 CRm=1111 010 11111.
        (enc::clrex(), 0b1101010100_0_00_011_0011_1111_010_11111, "clrex"),
        // C6.2 DMB: 1101010100 0 00 011 0011 CRm 1 01 11111; ISH = 1011, ISHLD = 1001.
        (enc::dmb(Barrier::Ish), 0b1101010100_0_00_011_0011_1011_1_01_11111, "dmb ish"),
        (enc::dmb(Barrier::IshLd), 0b1101010100_0_00_011_0011_1001_1_01_11111, "dmb ishld"),
        // C6.2 LDADD, LDCLR, LDEOR, LDSET, LDSMAX, LDSMIN, LDUMAX, LDUMIN, SWP:
        // size 111 V=0 00 A R 1 Rs o3 opc 00 Rn Rt.
        (enc::lse(Lse::Add, Size::X, true, true, 1, 0, 2), 0b11_111_0_00_1_1_1_00001_0_000_00_00010_00000, "ldaddal x1, x0, [x2]"),
        (enc::lse(Lse::Add, Size::B, false, false, 1, 0, 2), 0b00_111_0_00_0_0_1_00001_0_000_00_00010_00000, "ldaddb w1, w0, [x2]"),
        (enc::lse(Lse::Clr, Size::H, true, false, 10, 11, 9), 0b01_111_0_00_1_0_1_01010_0_001_00_01001_01011, "ldclrah w10, w11, [x9]"),
        (enc::lse(Lse::Eor, Size::W, false, true, 10, 11, 9), 0b10_111_0_00_0_1_1_01010_0_010_00_01001_01011, "ldeorl w10, w11, [x9]"),
        (enc::lse(Lse::Set, Size::X, false, false, 10, 11, 9), 0b11_111_0_00_0_0_1_01010_0_011_00_01001_01011, "ldset x10, x11, [x9]"),
        (enc::lse(Lse::Smax, Size::X, true, true, 10, 11, 9), 0b11_111_0_00_1_1_1_01010_0_100_00_01001_01011, "ldsmaxal x10, x11, [x9]"),
        (enc::lse(Lse::Smin, Size::W, true, false, 10, 11, 9), 0b10_111_0_00_1_0_1_01010_0_101_00_01001_01011, "ldsmina w10, w11, [x9]"),
        (enc::lse(Lse::Umax, Size::B, false, true, 10, 11, 9), 0b00_111_0_00_0_1_1_01010_0_110_00_01001_01011, "ldumaxlb w10, w11, [x9]"),
        (enc::lse(Lse::Umin, Size::H, false, false, 10, 11, 9), 0b01_111_0_00_0_0_1_01010_0_111_00_01001_01011, "lduminh w10, w11, [x9]"),
        (enc::lse(Lse::Swp, Size::X, true, true, 10, 11, 9), 0b11_111_0_00_1_1_1_01010_1_000_00_01001_01011, "swpal x10, x11, [x9]"),
        // C6.2 CAS, CASA, CASL, CASAL: size 0010001 L 1 Rs o0 11111 Rn Rt; L acquires, o0 releases.
        (enc::cas(Size::X, true, true, 0, 1, 2), 0b11_0010001_1_1_00000_1_11111_00010_00001, "casal x0, x1, [x2]"),
        (enc::cas(Size::X, false, false, 0, 1, 2), 0b11_0010001_0_1_00000_0_11111_00010_00001, "cas x0, x1, [x2]"),
        (enc::cas(Size::W, true, false, 12, 11, 9), 0b10_0010001_1_1_01100_0_11111_01001_01011, "casa w12, w11, [x9]"),
        (enc::cas(Size::B, false, true, 12, 11, 9), 0b00_0010001_0_1_01100_1_11111_01001_01011, "caslb w12, w11, [x9]"),
        // C6.2 MRS: 1101010100 L=1 1 o0 op1 CRn CRm op2 Rt; TPIDRRO_EL0 is op0=11 (o0 = 1),
        // op1=011, CRn=1101, CRm=0000, op2=011.
        (enc::mrs_tpidrro_el0(11), 0b1101010100_1_1_1_011_1101_0000_011_01011, "mrs x11, tpidrro_el0"),
        // C7.2 128-bit NEON SIMD encoders.
        (enc::ldp_q_off(0, 1, 2, 0), 0xad40_0440, "ldp q0, q1, [x2]"),
        (enc::stp_q_off(0, 1, 2, 1), 0xad00_8440, "stp q0, q1, [x2, #16]"),
        (enc::ldr_q_reg(0, 1, 2, true), 0x3ce2_7820, "ldr q0, [x1, x2, lsl #4]"),
        (enc::str_q_reg(0, 1, 2, false), 0x3ca2_6820, "str q0, [x1, x2]"),
        (enc::movi_zero_v(3), 0x6f00_e403, "movi v3.2d, #0"),
        (enc::dup_gen_v(3, 0, 1), 0x4e08_0c20, "dup v0.2d, x1"),
        (enc::dup_gen_v(2, 0, 1), 0x4e04_0c20, "dup v0.4s, w1"),
        (enc::dup_elem0_v(3, 0, 1), 0x4e08_0420, "dup v0.2d, v1.d[0]"),
        (enc::add_v(3, 0, 1, 2), 0x4ee2_8420, "add v0.2d, v1.2d, v2.2d"),
        (enc::add_v(2, 0, 1, 2), 0x4ea2_8420, "add v0.4s, v1.4s, v2.4s"),
        (enc::sub_v(3, 0, 1, 2), 0x6ee2_8420, "sub v0.2d, v1.2d, v2.2d"),
        (enc::mul_v(2, 0, 1, 2), 0x4ea2_9c20, "mul v0.4s, v1.4s, v2.4s"),
        (enc::mla_v(2, 0, 1, 2), 0x4ea2_9420, "mla v0.4s, v1.4s, v2.4s"),
        (enc::orr_v(0, 1, 2), 0x4ea2_1c20, "orr v0.16b, v1.16b, v2.16b"),
        (enc::eor_v(0, 1, 2), 0x6e22_1c20, "eor v0.16b, v1.16b, v2.16b"),
        (enc::fadd_v(true, 0, 1, 2), 0x4e62_d420, "fadd v0.2d, v1.2d, v2.2d"),
        (enc::fmul_v(false, 0, 1, 2), 0x6e22_dc20, "fmul v0.4s, v1.4s, v2.4s"),
        (enc::fmla_v(true, 0, 1, 2), 0x4e62_cc20, "fmla v0.2d, v1.2d, v2.2d"),
        (enc::addv_v(2, 0, 1), 0x4eb1_b820, "addv s0, v1.4s"),
        (enc::addp_d(0, 1), 0x5ef1_b820, "addp d0, v1.2d"),
        (enc::faddp_scalar(true, 0, 1), 0x7e70_d820, "faddp d0, v1.2d"),
        (enc::umov_lane(3, 0, 0, 1), 0x4e08_3c20, "umov x0, v1.d[0]"),
        (enc::umov_lane(3, 1, 0, 1), 0x4e18_3c20, "umov x0, v1.d[1]"),
        (enc::umov_lane(2, 0, 0, 1), 0x0e04_3c20, "umov w0, v1.s[0]"),
        (enc::ext_v(0, 1, 2, 8), 0x6e02_4020, "ext v0.16b, v1.16b, v2.16b, #8"),
        // ARMv8.9/v9.4 CSSC 1-instruction scalar intrinsics.
        (enc::cssc_minmax(false, false, 0, 1, 2), 0x9ac2_6020, "smax x0, x1, x2"),
        (enc::cssc_minmax(true, false, 0, 1, 2), 0x9ac2_6420, "umax x0, x1, x2"),
        (enc::cssc_minmax(false, true, 0, 1, 2), 0x9ac2_6820, "smin x0, x1, x2"),
        (enc::cssc_minmax(true, true, 0, 1, 2), 0x9ac2_6c20, "umin x0, x1, x2"),
        (enc::cssc_cnt(0, 1), 0xdac0_1c20, "cnt x0, x1"),
        (enc::cssc_abs(0, 1), 0xdac0_2020, "abs x0, x1"),
    ];
    for &(got, want, text) in cases {
        assert_eq!(got, want, "{text}: {got:#010x}, the manual's is {want:#010x}");
    }
}
