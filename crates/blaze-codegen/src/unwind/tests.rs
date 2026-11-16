//! The compact encodings of frames clang lays out are the ones clang's assembler wrote for
//! them (`clang -O2 -c` on macOS arm64, read back with `objdump --unwind-info`), and every
//! frame no compact encoding holds exactly keeps DWARF.

use super::{FrameRule::*, *};

/// clang's rows after `stp x29, x30, [sp, #0x40]; add x29, sp, #0x40` with the pairs
/// `x19`..`x26` below: `.cfi_def_cfa w29, 16; .cfi_offset w30, -8; w29, -16; w19, -24; ..`.
fn clang_frame(pairs: &[(u16, u16)]) -> Vec<(u32, FrameRule)> {
    let mut rows = vec![(0x18, Cfa(29, 16)), (0x18, Offset(30, -8)), (0x18, Offset(29, -16))];
    let mut next = -24;
    for &(first, second) in pairs {
        rows.push((0x18, Offset(first, next)));
        rows.push((0x18, Offset(second, next - 8)));
        next -= 16;
    }
    rows
}

#[test]
fn clangs_frames_give_clangs_encodings() {
    // `long f(long, long, long, long)` calling `g` four times: 0x0400000f.
    let x = clang_frame(&[(19, 20), (21, 22), (23, 24), (25, 26)]);
    assert_eq!(compact_aarch64(&x), Some(0x0400_000f));
    // `double k(double, double, double)`: `d8`..`d11`, 0x04000300.
    let d = clang_frame(&[(72, 73), (74, 75)]);
    assert_eq!(compact_aarch64(&d), Some(0x0400_0300));
    // A frame record and nothing else.
    assert_eq!(compact_aarch64(&clang_frame(&[])), Some(compact::FRAME));
    // A pair not saved leaves no gap: the next saved pair takes its place (as `d8` takes
    // -24 above).
    assert_eq!(compact_aarch64(&clang_frame(&[(21, 22)])), Some(compact::FRAME | 0x002));
    // A leaf with no rows: frameless, no stack (clang's `leaf`: 0x02000000).
    assert_eq!(compact_aarch64(&[]), Some(compact::FRAMELESS));
}

#[test]
fn a_prologue_and_epilogue_around_the_body_keep_its_encoding() {
    let rows = [
        // stp x29, x30, [sp, #-32]!
        (4, CfaOffset(32)),
        (4, Offset(29, -16)),
        (4, Offset(30, -8)),
        // mov x29, sp ... add x29, sp, #16
        (8, Cfa(29, 16)),
        // stp x20, x19, [sp]
        (12, Offset(19, -24)),
        (12, Offset(20, -32)),
        // An epilogue in the middle: remembered, unwound to entry, restored.
        (40, RememberState),
        (40, Cfa(31, 0)),
        (40, Restore(19)),
        (40, Restore(20)),
        (40, Restore(29)),
        (40, Restore(30)),
        (44, RestoreState),
    ];
    assert_eq!(compact_aarch64(&rows), Some(compact::FRAME | 0x001));
}

#[test]
fn a_frameless_function_gives_its_stack_size() {
    let rows = [(4, CfaOffset(32)), (20, CfaOffset(0))];
    assert_eq!(compact_aarch64(&rows), Some(compact::FRAMELESS | (2 << 12)));
    // Two sizes, or a size not of 16-byte units, are DWARF's.
    assert_eq!(compact_aarch64(&[(4, CfaOffset(32)), (8, CfaOffset(48))]), None);
    assert_eq!(compact_aarch64(&[(4, CfaOffset(24))]), None);
    // A frameless function that saves is not one the encoding holds.
    assert_eq!(compact_aarch64(&[(4, CfaOffset(16)), (4, Offset(19, -16))]), None);
}

#[test]
fn what_the_encoding_cannot_hold_keeps_dwarf() {
    let refused = |rows: &[(u32, FrameRule)], why: &str| {
        assert_eq!(compact_aarch64(rows), None, "{why}");
    };
    let mut signed = clang_frame(&[(19, 20)]);
    signed.insert(0, (0, RaSignState(true)));
    refused(&signed, "a signed return address");
    // Cranelift's own layout: `stp x19, x20, [sp, #-16]!` puts `x19` above `x20`.
    let mut swapped = clang_frame(&[]);
    swapped.extend([(0x18, Offset(19, -32)), (0x18, Offset(20, -24))]);
    refused(&swapped, "a pair in the other order");
    let mut lone = clang_frame(&[]);
    lone.push((0x18, Offset(19, -24)));
    refused(&lone, "half a pair");
    let mut x18 = clang_frame(&[(19, 20)]);
    x18.push((0x18, Offset(18, -40)));
    refused(&x18, "a register outside the pairs");
    let mut gap = clang_frame(&[]);
    gap.extend([(0x18, Offset(21, -40)), (0x18, Offset(22, -48))]);
    refused(&gap, "a gap where x19/x20 would go");
    refused(&[(4, Cfa(1, 0))], "a CFA on another register");
    refused(&[(4, Cfa(29, 32))], "a CFA on x29 at another offset");
    refused(&[(4, RestoreState)], "a restore with nothing remembered");
    refused(&[(4, Undefined(30))], "the end of the stack");
    refused(&[(4, Offset(200, -8))], "a register past the tracked ones");
    // The frame changes in the body: two states on x29 with other saves.
    let mut changed = clang_frame(&[(19, 20)]);
    changed.extend([
        (0x40, Restore(19)),
        (0x40, Restore(20)),
        (0x40, Offset(21, -24)),
        (0x40, Offset(22, -32)),
    ]);
    refused(&changed, "a body whose saves change");
}
