//! `blaze_native_load::a64`: each decoder against words encoded by hand from the Arm ARM's
//! fields, at the edges of each range (the last displacement a field holds, and that the next
//! one has no encoding: it wraps to the other end).

use blaze_native_load::a64;

const P: u64 = 0x1_0000_0000;

/// BL: `100101 imm26`, target `P + SignExtend(imm26:'00')`.
/// - imm26 = 0x1ff_ffff (2^25 - 1): 0x9400_0000 | 0x01ff_ffff = 0x95ff_ffff, P + 0x7ff_fffc
///   (2^27 - 4): the last forward displacement.
/// - imm26 = 0x200_0000 (-2^25): 0x9600_0000, P - 0x800_0000 (-2^27): the last backward one.
/// - +2^27 would need imm26 = 2^25, which is 0x200_0000 again: no encoding (the refusal edge
///   of BRANCH26 / CALL26; layout.rs veneers such a branch).
/// - B: `000101 imm26`, 0x1400_0001: P + 4; not a BL.
#[test]
fn branch26_edges() {
    assert_eq!(a64::branch26(0x95ff_ffff, P), Some(P + 0x7ff_fffc));
    assert_eq!(a64::branch26(0x9600_0000, P), Some(P - 0x800_0000));
    assert_eq!(a64::branch26(0x9400_0001, P), Some(P + 4));
    assert_eq!(a64::branch26(0x97ff_ffff, P), Some(P - 4));
    assert_eq!(a64::branch26(0x1400_0001, P), Some(P + 4));
    assert!(a64::is_bl(0x95ff_ffff) && !a64::is_bl(0x1400_0001));
    assert_eq!(a64::branch26(0xd503_201f, P), None, "NOP is no branch");
    println!("branch26_edges: +0x7fffffc and -0x8000000 decode; +0x8000000 has no encoding");
}

/// ADRP: `1 immlo:2 10000 immhi:19 Rd`, page `(P & !0xfff) + SignExtend(immhi:immlo) << 12`.
/// - imm = 0xf_ffff (2^20 - 1): immlo = 3 (bits 30..29: 0x6000_0000), immhi = 0x3_ffff
///   (bits 23..5: 0x3_ffff << 5 = 0x7f_ffe0): 0x9000_0000 | 0x6000_0000 | 0x7f_ffe0 = 0xf07f_ffe0,
///   page P + 0xffff_f000 (4 GiB - 4 KiB): the last forward page.
/// - imm = 0x10_0000 (-2^20): immlo = 0, immhi = 0x4_0000 (0x4_0000 << 5 = 0x80_0000):
///   0x9080_0000, page P - 0x1_0000_0000: the last backward page.
/// - imm = 1: immlo = 1 (0x2000_0000): 0xb000_0000, the next page.
#[test]
fn adrp_edges() {
    assert_eq!(a64::adrp(0xf07f_ffe0, P + 0x123), Some(P + 0xffff_f000));
    assert_eq!(a64::adrp(0x9080_0000, 2 * P), Some(P));
    assert_eq!(a64::adrp(0xb000_0000, P + 0xfff), Some(P + 0x1000));
    assert_eq!(a64::adrp(0x1000_0000, P), None, "ADR (op = 0) is not ADRP");
    println!("adrp_edges: +0xfffff000 and -0x100000000 pages decode");
}

/// The low twelve bits, and their scale by the access size:
/// - ADD x0, x0, #0xfff: 0x9100_0000 | 0xfff << 10 = 0x913f_fc00 -> 0xfff; with `sh = 1`
///   (0x9140_0000) the immediate is shifted by 12, which PAGEOFF12 never writes: None.
/// - LDR x0, [x0, #0x7ff8]: size 11, imm12 0xfff: 0xf940_0000 | 0x3f_fc00 = 0xf97f_fc00 -> 0xfff << 3.
/// - LDRB w0, [x0, #0xfff]: size 00: 0x3940_0000 | 0x3f_fc00 = 0x397f_fc00 -> 0xfff.
/// - LDR q0, [x0, #16]: size 00, V 1, opc 11: 0x3dc0_0000 | 1 << 10 = 0x3dc0_0400 -> 1 << 4.
/// - LDR w0, [x0, #4]: size 10: 0xb940_0000 | 1 << 10 = 0xb940_0400 -> 1 << 2.
#[test]
fn lo12_scales() {
    assert_eq!(a64::add_lo12(0x913f_fc00), Some(0xfff));
    assert_eq!(a64::add_lo12(0x9140_0400), None);
    assert_eq!(a64::ldst_lo12(0xf97f_fc00), Some(0x7ff8));
    assert_eq!(a64::ldst_lo12(0x397f_fc00), Some(0xfff));
    assert_eq!(a64::ldst_lo12(0x3dc0_0400), Some(16));
    assert_eq!(a64::ldst_lo12(0xb940_0400), Some(4));
    assert_eq!(a64::ldst_lo12(0x913f_fc00), None, "an ADD is no load");
    // adrp x16, P + 0x5000; ldr x16, [x16, #0x18]: ADRP imm 5 = immlo 1, immhi 1:
    // 0x9000_0010 | 0x2000_0000 | 1 << 5 = 0xb000_0030; LDR 0xf940_0000 | 3 << 10 | 16 << 5 | 16 = 0xf940_0e10.
    assert_eq!(a64::page_pair(0xb000_0030, P, 0xf940_0e10), Some(P + 0x5018));
    println!("lo12_scales: add 0xfff, ldr x 0x7ff8, ldrb 0xfff, ldr q 16, ldr w 4");
}

/// B.cond `01010100 imm19 0 cond`, CBZ `sf 011010 0 imm19 Rt`, TBZ `b5 011011 0 b40 imm14 Rt`:
/// - B.EQ imm19 = 0x3_ffff: 0x5400_0000 | 0x3_ffff << 5 = 0x547f_ffe0 -> P + 0xf_fffc;
///   imm19 = 0x4_0000 (-2^18): 0x5480_0000 -> P - 0x10_0000.
/// - CBZ x0, imm19 = 1: 0xb400_0020 -> P + 4.
/// - TBZ w0, #0, imm14 = 0x1fff: 0x3600_0000 | 0x1fff << 5 = 0x3603_ffe0 -> P + 0x7ffc;
///   imm14 = 0x2000 (-2^13): 0x3604_0000 -> P - 0x8000.
#[test]
fn short_branch_edges() {
    assert_eq!(a64::branch19(0x547f_ffe0, P), Some(P + 0xf_fffc));
    assert_eq!(a64::branch19(0x5480_0000, P), Some(P - 0x10_0000));
    assert_eq!(a64::branch19(0xb400_0020, P), Some(P + 4));
    assert_eq!(a64::branch14(0x3603_ffe0, P), Some(P + 0x7ffc));
    assert_eq!(a64::branch14(0x3604_0000, P), Some(P - 0x8000));
    assert_eq!(a64::branch14(0x547f_ffe0, P), None);
    println!("short_branch_edges: b.cond +-1 MiB, tbz +-32 KiB");
}
