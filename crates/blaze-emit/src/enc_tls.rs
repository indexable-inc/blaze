//! The instruction words the indexed thread-local access (pass/tls_index.rs) needs beyond
//! enc.rs's forms, each from its encoding diagram in the Arm Architecture Reference Manual
//! (DDI 0487), chapter C6, and pinned by enc_tls_tests.rs against a word written by hand from
//! that diagram.

use crate::enc::{self, Reg};

/// `mrs xt, tpidr_el0`: C6.2 MRS, `1101010100 L=1 1 o0 op1 CRn CRm op2 Rt`, the system register
/// TPIDR_EL0 being op0 3 (o0 = 1), op1 3, CRn 13, CRm 0, op2 2 (D19.2, "TPIDR_EL0, EL0 Read/Write
/// Software Thread ID Register"). Linux points it at the thread's TLS block, which holds the
/// launcher's own thread-local root.
pub(crate) const fn mrs_tpidr_el0(rt: Reg) -> u32 {
    0xd53b_d040 | rt
}

/// `and xd, xn, #0x7fff_ffff_ffff_ffff`: C6.2 AND (immediate), `sf=1 opc=00 100100 N immr imms Rn
/// Rd`, the bitmask immediate of 63 ones (`imms` = 62, the count less one, with `N` = 1 for a
/// 64-bit element) not rotated (`immr` = 0): it clears bit 63, the descriptor root's flag.
pub(crate) const fn and_clear_bit63(rd: Reg, rn: Reg) -> u32 {
    enc::and_imm(rd, rn, 1, 0, 62)
}
