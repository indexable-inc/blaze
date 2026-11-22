//! The `asm!` mnemonic table (asm/table.rs): each row's statement against the word its Arm A64
//! encoding gives (Arm ARM DDI 0487, the instruction's encoding diagram), written beside it as
//! a binary literal grouped by the diagram's fields, so the expected word comes from the
//! layout and never from the encoder. The fields are named in each group's comment.

use crate::{asm::Dialect, asm_tests::assembled};

/// The one word `statement` assembles to, or the refusal's reason and token.
fn word(statement: &str) -> Result<u32, (&'static str, String)> {
    match assembled(Dialect::Darwin, statement) {
        Ok(words) => {
            assert_eq!(words.len(), 1, "{statement:?} must make exactly one word");
            Ok(words[0])
        }
        Err(e) => Err((e.why, e.token.text().to_owned())),
    }
}

/// Every row, by the layout.
///
/// Prints: `asm::table: N statements, each its A64 layout's word`.
#[test]
fn rows_match_the_a64_layouts() {
    #[rustfmt::skip]
    let rows: &[(&str, u32)] = &[
        // PC-relative offsets are signed words, measured from the branch.
        ("b 2f\n2:",                  0b000101_00000000000000000000000001),
        ("b.ne 2f\n2:",               0b01010100_0000000000000000001_0_0001),
        ("2: b.al 2b",                0b01010100_0000000000000000000_0_1110),
        ("cbz x5, 2f\n2:",            0b1_011010_0_0000000000000000001_00101),
        ("cbnz w4, 2f\n2:",           0b0_011010_1_0000000000000000001_00100),
        ("tbz w3, #31, 2f\n2:",       0b0_011011_0_11111_00000000000001_00011),
        ("tbnz x17, #32, 2f\n2:",     0b1_011011_1_00000_00000000000001_10001),
        ("sub w4, w4, #1",           0b0_1_0_100010_0_000000000001_00100_00100),
        ("add x2, x2, #1",           0b1_0_0_100010_0_000000000001_00010_00010),
        // size 001000 0 L pair Rs ordered Rt2 Rn Rt.
        ("ldxr w3, [x4]",            0b10_001000_0_1_0_11111_0_11111_00100_00011),
        ("ldaxr x5, [sp, #0]",       0b11_001000_0_1_0_11111_1_11111_11111_00101),
        ("stxr w7, x8, [x9]",        0b11_001000_0_0_0_00111_0_11111_01001_01000),
        ("stlxr w9, w2, [x3]",       0b10_001000_0_0_0_01001_1_11111_00011_00010),
        ("ldxp x3, x4, [x5]",        0b11_001000_0_1_1_11111_0_00100_00101_00011),
        ("ldxp w3, w4, [x5]",        0b10_001000_0_1_1_11111_0_00100_00101_00011),
        ("stxp w3, x4, x5, [x6]",    0b11_001000_0_0_1_00011_0_00101_00110_00100),
        ("stxp w3, w4, w5, [x6]",    0b10_001000_0_0_1_00011_0_00101_00110_00100),
        // System: 1101010100 L op0 op1 CRn CRm op2 Rt. Barriers and hints: L 0, op0 00,
        // op1 011, CRn 0010 (hints) or 0011 (barriers), Rt 11111.
        ("nop",                     0b1101010100_0_00_011_0010_0000_000_11111),
        ("isb",                     0b1101010100_0_00_011_0011_1111_110_11111),
        ("ISB SY",                  0b1101010100_0_00_011_0011_1111_110_11111),
        ("dsb sy",                  0b1101010100_0_00_011_0011_1111_100_11111),
        ("dsb nsh",                 0b1101010100_0_00_011_0011_0111_100_11111),
        ("dsb ish",                 0b1101010100_0_00_011_0011_1011_100_11111),
        ("dmb ishld",               0b1101010100_0_00_011_0011_1001_101_11111),
        ("dmb #11",                 0b1101010100_0_00_011_0011_1011_101_11111),
        ("sb",                      0b1101010100_0_00_011_0011_0000_111_11111),
        // MRS (L 1) and MSR register (L 0): op0 op1 CRn CRm op2 of the register.
        ("mrs x0, cntvct_el0",      0b1101010100_1_11_011_1110_0000_010_00000),
        ("mrs x0, s3_3_c14_c0_2",   0b1101010100_1_11_011_1110_0000_010_00000),
        ("mrs x9, ID_AA64ISAR0_EL1", 0b1101010100_1_11_000_0000_0110_000_01001),
        ("mrs x3, dit",             0b1101010100_1_11_011_0100_0010_101_00011),
        ("mrs x12, tpidrro_el0",    0b1101010100_1_11_011_1101_0000_011_01100),
        ("msr dit, x3",             0b1101010100_0_11_011_0100_0010_101_00011),
        ("msr tpidr_el0, x1",       0b1101010100_0_11_011_1101_0000_010_00001),
        // MSR immediate: 1101010100 0 00 op1 0100 CRm(imm) op2 11111.
        ("msr dit, #1",             0b1101010100_0_00_011_0100_0001_010_11111),
        ("msr dit, #0",             0b1101010100_0_00_011_0100_0000_010_11111),
        ("msr ssbs, #1",            0b1101010100_0_00_011_0100_0001_001_11111),
        // Data processing, 1 source: sf 1 S 11010110 opcode2 opcode Rn Rd; PACIB opcode2
        // 00001, opcode 000001 (Z 0).
        ("pacib x30, sp",           0b1_1_0_11010110_00001_000001_11111_11110),
        ("pacib x0, x1",            0b1_1_0_11010110_00001_000001_00001_00000),
        // Logical, shifted register: sf opc 01010 shift N Rm imm6 Rn Rd; ANDS opc 11 (TST is
        // Rd 11111), EOR opc 10.
        ("tst w0, w1",              0b0_11_01010_00_0_00001_000000_00000_11111),
        ("tst x2, x3",              0b1_11_01010_00_0_00011_000000_00010_11111),
        ("eor w0, w1, w2",          0b0_10_01010_00_0_00010_000000_00001_00000),
        ("eor x9, x10, x11",        0b1_10_01010_00_0_01011_000000_01010_01001),
        // Logical, immediate: sf opc 100100 N immr imms Rn Rd; the bitmask by DecodeBitMasks
        // (0xff: element 32 or 64 bits, 8 ones, imms 0b000111; 0x80000000: one one rotated
        // right 1; 0x5555..: element 2 bits, imms 0b111100).
        ("tst w0, 0xff",            0b0_11_100100_0_000000_000111_00000_11111),
        ("tst x0, 0xff",            0b1_11_100100_1_000000_000111_00000_11111),
        ("tst w9, 0xffff",          0b0_11_100100_0_000000_001111_01001_11111),
        ("tst w1, #0x80000000",     0b0_11_100100_0_000001_000000_00001_11111),
        ("tst x0, #0x5555555555555555", 0b1_11_100100_0_000000_111100_00000_11111),
        // Conditional select: sf op S 11010100 Rm cond 0 o2 Rn Rd (eq 0000, ne 0001, hs 0010).
        ("csel w1, w2, w3, ne",     0b0_0_0_11010100_00011_0001_0_0_00010_00001),
        ("csel x0, x1, x2, EQ",     0b1_0_0_11010100_00010_0000_0_0_00001_00000),
        ("csel x0, x1, x2, hs",     0b1_0_0_11010100_00010_0010_0_0_00001_00000),
        // Add/subtract, shifted register: sf op S 01011 shift 0 Rm imm6 Rn Rd; CMP is SUBS
        // (op 1, S 1) with Rd 11111.
        ("cmp x1, x2",              0b1_1_1_01011_00_0_00010_000000_00001_11111),
        ("cmp w3, wzr",             0b0_1_1_01011_00_0_11111_000000_00011_11111),
        // Add/subtract, immediate: sf op S 100010 sh imm12 Rn Rd; a negative CMP is CMN (op 0).
        ("cmp w0, 0",               0b0_1_1_100010_0_000000000000_00000_11111),
        ("cmp x5, #4095",           0b1_1_1_100010_0_111111111111_00101_11111),
        ("cmp sp, #16",             0b1_1_1_100010_0_000000010000_11111_11111),
        ("cmp w1, #-1",             0b0_0_1_100010_0_000000000001_00001_11111),
        // FP data processing, 3 source: 0001 1111 ftype o1 Rm o0 Ra Rn Rd (S 00, D 01, H 11).
        ("fmadd d0, d1, d2, d3",    0b00011111_01_0_00010_0_00011_00001_00000),
        ("fmadd s4, s5, s6, s7",    0b00011111_00_0_00110_0_00111_00101_00100),
        ("fmadd h0, h1, h2, h3",    0b00011111_11_0_00010_0_00011_00001_00000),
        // FP data processing, 1 source: M 0 S 11110 ftype 1 opcode 10000 Rn Rd; FRINTN
        // 001000, FSQRT 000011, FCVT 0001 opc (opc the destination's type).
        ("frintn d0, d1",           0b0_0_0_11110_01_1_001000_10000_00001_00000),
        ("frintn s2, s3",           0b0_0_0_11110_00_1_001000_10000_00011_00010),
        ("fsqrt d0, d0",            0b0_0_0_11110_01_1_000011_10000_00000_00000),
        ("fsqrt h1, h2",            0b0_0_0_11110_11_1_000011_10000_00010_00001),
        ("fcvt d0, s1",             0b0_0_0_11110_00_1_000101_10000_00001_00000),
        ("fcvt s0, d1",             0b0_0_0_11110_01_1_000100_10000_00001_00000),
        ("fcvt h0, s1",             0b0_0_0_11110_00_1_000111_10000_00001_00000),
        ("fcvt s2, h3",             0b0_0_0_11110_11_1_000100_10000_00011_00010),
        ("fcvt d4, h5",             0b0_0_0_11110_11_1_000101_10000_00101_00100),
        ("fcvt h6, d7",             0b0_0_0_11110_01_1_000111_10000_00111_00110),
        // CRC32: sf 0 0 11010110 Rm 010 C sz Rn Rd.
        ("crc32b w0, w0, w1",       0b0_0_0_11010110_00001_010_0_00_00000_00000),
        ("crc32h w2, w3, w4",       0b0_0_0_11010110_00100_010_0_01_00011_00010),
        ("crc32w w2, w3, w4",       0b0_0_0_11010110_00100_010_0_10_00011_00010),
        ("crc32x w0, w1, x2",       0b1_0_0_11010110_00010_010_0_11_00001_00000),
        ("crc32cb w0, w1, w2",      0b0_0_0_11010110_00010_010_1_00_00001_00000),
        ("crc32cx w0, w1, x2",      0b1_0_0_11010110_00010_010_1_11_00001_00000),
        // CMEQ (register): 0 Q 1 01110 size 1 Rm 10001 1 Rn Rd.
        ("cmeq v0.16b, v1.16b, v2.16b", 0b0_1_1_01110_00_1_00010_10001_1_00001_00000),
        ("cmeq v0.8b, v1.8b, v2.8b",    0b0_0_1_01110_00_1_00010_10001_1_00001_00000),
        ("cmeq v3.4s, v4.4s, v5.4s",    0b0_1_1_01110_10_1_00101_10001_1_00100_00011),
        ("cmeq v16.2d, v17.2d, v31.2d", 0b0_1_1_01110_11_1_11111_10001_1_10001_10000),
        // AND (vector): 0 Q 0 01110 00 1 Rm 00011 1 Rn Rd.
        ("and v0.16b, v1.16b, v2.16b",  0b0_1_0_01110_00_1_00010_00011_1_00001_00000),
        ("and v16.8b, v17.8b, v18.8b",  0b0_0_0_01110_00_1_10010_00011_1_10001_10000),
        // SHRN: 0 Q 0 011110 immh immb 1000 0 1 Rn Rd; immh:immb = 2 * esize - shift.
        ("shrn v0.8b, v0.8h, #4",   0b0_0_0_011110_0001_100_1000_0_1_00000_00000),
        ("shrn v0.8b, v1.8h, 8",    0b0_0_0_011110_0001_000_1000_0_1_00001_00000),
        ("shrn v1.4h, v2.4s, #16",  0b0_0_0_011110_0010_000_1000_0_1_00010_00001),
        ("shrn v1.2s, v2.2d, #1",   0b0_0_0_011110_0111_111_1000_0_1_00010_00001),
    ];
    for &(statement, expected) in rows {
        let got = word(statement).unwrap_or_else(|e| panic!("{statement:?} refused: {e:?}"));
        assert_eq!(got, expected, "{statement:?}: {got:#010x}, the layout's {expected:#010x}");
    }
    println!("asm::table: {} statements, each its A64 layout's word", rows.len());
}

/// What no row takes is refused, naming the operand when the shape fits and the value does
/// not, and the statement when no shape fits.
///
/// Prints: `asm::table refusals: N, each naming its token`.
#[test]
fn refusals_name_the_token() {
    let none = "operands no form of the mnemonic in the assembler's table takes";
    let cases: &[(&str, &str, &str)] = &[
        ("ldxr x3, [xzr]", none, "ldxr x3, [xzr]"),
        ("ldxr x3, [x2,#8]", none, "ldxr x3, [x2,#8]"),
        ("stxr x1, x2, [x3]", none, "stxr x1, x2, [x3]"),
        ("stlxr w2, x2, [x3]", "exclusive store status overlaps data or address", "w2"),
        ("ldxp w1, x2, [x3]", none, "ldxp w1, x2, [x3]"),
        ("ldxp w2, w2, [x3]", "exclusive pair load has overlapping destinations", "w2"),
        ("stxp w3, x1, x2, [x3]", "exclusive store status overlaps data or address", "w3"),
        ("csel w0, x1, w2, ne", none, "csel w0, x1, w2, ne"),
        ("and x0, x1, x2", none, "and x0, x1, x2"),
        ("crc32b w0, w0, x1", none, "crc32b w0, w0, x1"),
        ("eor w0, w1, w2, lsl #3", none, "eor w0, w1, w2, lsl #3"),
        ("cmp x0, #4096", "a `cmp` immediate outside -4095..=4095 (the `lsl #12` form is not in the table)", "#4096"),
        ("cmp xzr, #1", "the zero register in `cmp`'s immediate form (31 is `sp` there)", "xzr"),
        ("mrs x0, foo_el1", "a system register outside the assembler's table", "foo_el1"),
        ("mrs x0, s1_0_c0_c0_0", "a system register outside the assembler's table", "s1_0_c0_c0_0"),
        ("msr cntvct_el0, x0", "a read-only system register written by `msr`", "cntvct_el0"),
        ("msr spsel, #1", "a PSTATE field outside the assembler's table (`dit`, `ssbs`, `tco`)", "spsel"),
        ("msr dit, #2", "a PSTATE field's immediate outside 0..=1", "#2"),
        ("shrn v0.8b, v0.8h, #9", "a `shrn` shift outside 1 to the destination element's bits", "#9"),
        ("tst w0, #0", "an immediate no logical immediate encodes (a repeated, rotated run of ones)", "#0"),
        ("tst w0, #0x100000000", "an immediate no logical immediate encodes (a repeated, rotated run of ones)", "#0x100000000"),
        ("csel x0, x1, x2, al", "a condition outside `eq`..`le` (`al` and `nv` are not taken)", "al"),
        ("pacib x0, xzr", "`xzr` as `pacib`'s modifier (31 is `sp` there; the zero modifier is `pacizb`)", "xzr"),
        ("dsb foo", "a barrier option `dmb` and `dsb` have not", "foo"),
        ("bl foo", "a mnemonic the baseline tier's assembler has no row for", "bl"),
    ];
    for &(statement, why, token) in cases {
        match word(statement) {
            Ok(w) => panic!("{statement:?} assembled to {w:#010x}"),
            Err((got_why, got_token)) => {
                assert_eq!(got_why, why, "{statement:?}");
                // The token keeps its first 24 bytes.
                let kept = &token[..token.len().min(24)];
                assert_eq!(got_token, kept, "{statement:?}");
            }
        }
    }
    println!("asm::table refusals: {}, each naming its token", cases.len());
}
