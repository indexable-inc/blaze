//! The AArch64 resolver and entries, as instruction words.
//!
//! The resolver's frame, from the caller's stack pointer `s` at the entry:
//!
//! | at | what |
//! |---|---|
//! | `s - 16` (`x29`) | the caller's `x29` and `x30` (the return address) |
//! | `x29 - 128 ..` | `x0` to `x15` |
//! | below | `q0` to `q31` (512 bytes); with SVE, `z0` to `z31` (32 vector lengths), then `p0` to `p15` and `FFR` (3 vector lengths) |
//!
//! Every word is spelled out from the Arm ARM's encodings by the helpers below; the tests
//! check them against known instructions.

use super::{CELL, ENTRY, RESOLVER};

/// `bti c; movz w16, #0; movk w16, #0, lsl #16; b resolver`: `bti c` takes the `br x16` of
/// blaze-link's slot stub on pages with BTI, and is a hint elsewhere.
pub(crate) const ENTRY_TEMPLATE: [u8; ENTRY as usize] = {
    let words = [BTI_C, movz_w(16, 0), movk_w16(16, 0), 0x1400_0000];
    let mut out = [0; ENTRY as usize];
    let mut i = 0;
    while i < 4 {
        let bytes = words[i].to_le_bytes();
        out[4 * i] = bytes[0];
        out[4 * i + 1] = bytes[1];
        out[4 * i + 2] = bytes[2];
        out[4 * i + 3] = bytes[3];
        i += 1;
    }
    out
};

pub(crate) fn fill_entry(entry: &mut [u8], at: u32, id: u32) {
    put(entry, 4, movz_w(16, id & 0xffff));
    put(entry, 8, movk_w16(16, id >> 16));
    put(entry, 12, b(i64::from(RESOLVER) - i64::from(at + 12)));
}

fn put(bytes: &mut [u8], at: usize, word: u32) {
    bytes[at..at + 4].copy_from_slice(&word.to_le_bytes());
}

pub(crate) const BTI_C: u32 = 0xd503_245f;
const SP: u32 = 31;

pub(crate) const fn movz_w(rd: u32, imm16: u32) -> u32 {
    0x5280_0000 | (imm16 & 0xffff) << 5 | rd
}

/// `movk wd, #imm16, lsl #16`.
pub(crate) const fn movk_w16(rd: u32, imm16: u32) -> u32 {
    0x72a0_0000 | (imm16 & 0xffff) << 5 | rd
}

/// `b` by `offset` bytes from the instruction.
pub(crate) const fn b(offset: i64) -> u32 {
    0x1400_0000 | ((offset >> 2) as u32 & 0x03ff_ffff)
}

/// `stp xt, xt2, [xn, #offset]` and `ldp`, 64-bit, signed offset.
pub(crate) const fn stp_x(rt: u32, rt2: u32, rn: u32, offset: i32) -> u32 {
    0xa900_0000 | ((offset / 8) as u32 & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt
}

pub(crate) const fn ldp_x(rt: u32, rt2: u32, rn: u32, offset: i32) -> u32 {
    0xa940_0000 | ((offset / 8) as u32 & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt
}

/// `stp qt, qt2, [xn, #offset]` and `ldp`, 128-bit, signed offset.
pub(crate) const fn stp_q(rt: u32, rt2: u32, rn: u32, offset: i32) -> u32 {
    0xad00_0000 | ((offset / 16) as u32 & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt
}

pub(crate) const fn ldp_q(rt: u32, rt2: u32, rn: u32, offset: i32) -> u32 {
    0xad40_0000 | ((offset / 16) as u32 & 0x7f) << 15 | rt2 << 10 | rn << 5 | rt
}

/// `sub xd, xn, #imm12` and `add`, 64-bit, unshifted.
pub(crate) const fn sub_imm(rd: u32, rn: u32, imm12: u32) -> u32 {
    0xd100_0000 | (imm12 & 0xfff) << 10 | rn << 5 | rd
}

pub(crate) const fn add_imm(rd: u32, rn: u32, imm12: u32) -> u32 {
    0x9100_0000 | (imm12 & 0xfff) << 10 | rn << 5 | rd
}

/// `str xzr, [sp]`: touches the page the stack grew into.
pub(crate) const PROBE_SP: u32 = 0xf900_03ff;

/// `addvl xd, xn, #imm`: adds `imm` vector lengths.
pub(crate) const fn addvl(rd: u32, rn: u32, imm: i32) -> u32 {
    0x0420_5000 | rn << 16 | (imm as u32 & 0x3f) << 5 | rd
}

/// The `imm9` of SVE's vector and predicate loads and stores: high six bits at 16, low
/// three at 10.
const fn imm9(imm: i32) -> u32 {
    let imm = imm as u32 & 0x1ff;
    (imm >> 3) << 16 | (imm & 7) << 10
}

/// `str zt, [xn, #imm, mul vl]` and `ldr`.
pub(crate) const fn str_z(zt: u32, rn: u32, imm: i32) -> u32 {
    0xe580_4000 | imm9(imm) | rn << 5 | zt
}

pub(crate) const fn ldr_z(zt: u32, rn: u32, imm: i32) -> u32 {
    0x8580_4000 | imm9(imm) | rn << 5 | zt
}

/// `str pt, [xn, #imm, mul vl]` and `ldr`.
pub(crate) const fn str_p(pt: u32, rn: u32, imm: i32) -> u32 {
    0xe580_0000 | imm9(imm) | rn << 5 | pt
}

pub(crate) const fn ldr_p(pt: u32, rn: u32, imm: i32) -> u32 {
    0x8580_0000 | imm9(imm) | rn << 5 | pt
}

/// `rdffr pd.b` and `wrffr pn.b`.
pub(crate) const fn rdffr(pd: u32) -> u32 {
    0x2519_f000 | pd
}

pub(crate) const fn wrffr(pn: u32) -> u32 {
    0x2528_9000 | pn << 5
}

/// `ldr xt, label`, `offset` bytes from the instruction.
pub(crate) const fn ldr_literal(rt: u32, offset: i64) -> u32 {
    0x5800_0000 | ((offset >> 2) as u32 & 0x7ffff) << 5 | rt
}

/// `cbz xt, label`.
pub(crate) const fn cbz_x(rt: u32, offset: i64) -> u32 {
    0xb400_0000 | ((offset >> 2) as u32 & 0x7ffff) << 5 | rt
}

pub(crate) const STP_FP_LR_PRE: u32 = 0xa9bf_7bfd; // stp x29, x30, [sp, #-16]!
pub(crate) const MOV_FP_SP: u32 = 0x9100_03fd; // mov x29, sp
pub(crate) const MOV_SP_FP: u32 = 0x9100_03bf; // mov sp, x29
pub(crate) const LDP_FP_LR_POST: u32 = 0xa8c1_7bfd; // ldp x29, x30, [sp], #16
pub(crate) const MOV_W0_W16: u32 = 0x2a10_03e0; // mov w0, w16
pub(crate) const MOV_X17_X0: u32 = 0xaa00_03f1; // mov x17, x0
pub(crate) const BLR_X16: u32 = 0xd63f_0200;
pub(crate) const BR_X17: u32 = 0xd61f_0220;
pub(crate) const BRK_1: u32 = 0xd420_0020;

/// The bytes the general registers take.
const GENERAL: u32 = 128;
/// The bytes `q0` to `q31` take.
const VECTORS: u32 = 512;
/// The vector lengths the SVE `z` registers take, allocated four at a time (at most 1 KiB
/// at the largest vector length) with a probe after each.
const Z_LENGTHS: i32 = 32;
const Z_STEP: i32 = 4;
/// The vector lengths `p0` to `p15` and `FFR` take: 17 predicate lengths, an eighth of a
/// vector length each.
const P_LENGTHS: i32 = 3;

/// Appends the resolver at `out.len()`, which is [`RESOLVER`].
pub(crate) fn resolver(out: &mut Vec<u8>, sve: bool) {
    debug_assert_eq!(out.len(), RESOLVER as usize);
    let mut words: Vec<u32> = Vec::with_capacity(160);
    words.extend([STP_FP_LR_PRE, MOV_FP_SP, sub_imm(SP, SP, GENERAL)]);
    for pair in 0..8 {
        words.push(stp_x(2 * pair, 2 * pair + 1, SP, 16 * pair as i32));
    }
    if sve {
        for _ in 0..Z_LENGTHS / Z_STEP {
            words.extend([addvl(SP, SP, -Z_STEP), PROBE_SP]);
        }
        for z in 0..32 {
            words.push(str_z(z, SP, z as i32));
        }
        words.extend([addvl(SP, SP, -P_LENGTHS), PROBE_SP]);
        for p in 0..16 {
            words.push(str_p(p, SP, p as i32));
        }
        words.extend([rdffr(0), str_p(0, SP, 16)]);
    } else {
        words.push(sub_imm(SP, SP, VECTORS));
        for pair in 0..16 {
            words.push(stp_q(2 * pair, 2 * pair + 1, SP, 32 * pair as i32));
        }
    }
    words.push(MOV_W0_W16);
    let ldr = words.len();
    words.push(0);
    words.extend([BLR_X16, MOV_X17_X0]);
    let cbz = words.len();
    words.push(0);
    if sve {
        words.extend([ldr_p(0, SP, 16), wrffr(0)]);
        for p in 0..16 {
            words.push(ldr_p(p, SP, p as i32));
        }
        words.push(addvl(SP, SP, P_LENGTHS));
        for z in 0..32 {
            words.push(ldr_z(z, SP, z as i32));
        }
        words.push(addvl(SP, SP, Z_LENGTHS));
    } else {
        for pair in 0..16 {
            words.push(ldp_q(2 * pair, 2 * pair + 1, SP, 32 * pair as i32));
        }
        words.push(add_imm(SP, SP, VECTORS));
    }
    for pair in 0..8 {
        words.push(ldp_x(2 * pair, 2 * pair + 1, SP, 16 * pair as i32));
    }
    words.extend([MOV_SP_FP, LDP_FP_LR_POST, BR_X17]);
    let trap = words.len();
    words.push(BRK_1);

    let at = |index: usize| i64::from(RESOLVER) + 4 * index as i64;
    words[ldr] = ldr_literal(16, i64::from(CELL) - at(ldr));
    words[cbz] = cbz_x(17, at(trap) - at(cbz));
    out.reserve(4 * words.len());
    for word in words {
        out.extend_from_slice(&word.to_le_bytes());
    }
}
