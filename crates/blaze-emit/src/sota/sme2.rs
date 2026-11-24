//! AArch64 Hardware SME2 (`FMOPA`/`SMOPA`/`UMOPA`) + NEON (`TBL`/`SDOT`/`UDOT`/`SMMLA`/`UMMLA`)
//! Instruction Encoders & SVL-Agnostic Executable GEMM Micro-Kernel Emitters.
//!
//! Every 32-bit instruction word in this module is pinned bit-for-bit against Apple's
//! `arm64` assembler (`-march=armv9-a+sme2+sme-f64f64+dotprod+i8mm`).
//!
//! # Micro-Kernel Architecture
//!
//! - **ABI**:
//!   - `f32` / `f64`:
//!     `extern "C" fn(c: *mut T, a: *const T, b: *const T, m: i64, n: i64, k: i64, flags: i64)`
//!   - `i8 -> i32` / `u8 -> u32`:
//!     `extern "C" fn(c: *mut Acc, a: *const In, b: *const In, m: i64, n: i64, k: i64, flags: i64)`
//!   where row-major `A` is `m × k`, `B` is `k × n`, `C` is `m × n`, and
//!   `flags & SME2_GEMM_FLAG_ACCUM != 0` computes `C += A * B` while `flags & 1 == 0`
//!   computes `C = A * B`.
//! - **4-Tile `2×2` `ZA` Blocking (`f32` `FMOPA` & `i8`/`u8` `SMOPA`/`UMOPA`)**:
//!   - Uses ALL 4 32-bit `ZA` tiles (`za0.s..=za3.s` = `2*SVL_s × 2*SVL_s`; `32 × 32` =
//!     1,024 `f32` FMAs / 2,048 FLOPs per `kk` step, or 4,096 INT8 MACs / 8,192 OPS per
//!     4-way `k`-group on 512-bit SVL Apple Silicon).
//!   - `k_block` sits outer to `j_loop` (`<= 32 KiB` 128-byte-aligned L1d-resident stack
//!     micro-panel with safe 4 KiB stack probing): `A`'s `2*SVL_s × k_left` panel is
//!     hardware-transposed ONCE via `za0`/`za1` and reused across every `j_tile`.
//!   - Inner `f32` `kk` loop has ZERO `mova` instructions: 4×-unrolled with SME2 4-vector
//!     `ld1w` for `A_pack` and 2-vector predicated `ld1w` (`whilelt pn9.s, ..., vlx2`) for `B`,
//!     emitting 16 back-to-back `FMOPA.S` instructions (8,192 FLOPs) per branch.
//!   - Inner `i8`/`u8` loop uses SVE `zip1.b` + `zip1.h`/`zip2.h` to interleave 4 rows of `B`
//!     into both left and right `2×2` `SMOPA`/`UMOPA` tiles (2×-unrolled = 8 `SMOPA`/`UMOPA`
//!     instructions = 16,384 INT8 OPS per branch).
//! - **4-Tile `1×4` `ZA` Blocking (`f64` `FMOPA`)**:
//!   - `za0.d` acts as a zero-stack in-register hardware transpose scratchpad while
//!     `za1.d..=za4.d` (`SVL_d × 4*SVL_d`; `8 × 32` = 256 FMAs / 512 FLOPs per `kk` step)
//!     accumulate 4 side-by-side double-precision tiles.
//! - **Arbitrary Dimensions Without Scalar Cleanup**:
//!   Streaming SVE `whilelt` predicates and SME2 predicate-as-counter (`whilelt pn`) guard
//!   every load, store, and outer product, handling arbitrary non-multiple `(m, n, k)` shapes
//!   cleanly with zero out-of-bounds memory accesses.
//! - **Apple Silicon AAPCS64 Streaming ABI**:
//!   Callee-saved GPRs (`x19..=x28`, `x29`, `x30`) and `d8..=d15` are saved BEFORE
//!   `smstart` and restored AFTER `smstop` (since `smstart`/`smstop` zero `v0..=v31`).

use blaze_own::{AVec, Alloc, Vec};

use crate::enc::{self, Cond, FP, LR, SP, Size, X0, X1, X2, X8, X9, X10, X11, X12, X13};

const X3: u32 = 3;
const X4: u32 = 4;
const X5: u32 = 5;
const X6: u32 = 6;
const X7: u32 = 7;

const X14: u32 = 14;
const X15: u32 = 15;
const X16: u32 = 16;
const X17: u32 = 17;
const X19: u32 = 19;
const X20: u32 = 20;
const X21: u32 = 21;
const X22: u32 = 22;
const X23: u32 = 23;
const X24: u32 = 24;
const X25: u32 = 25;
const X26: u32 = 26;
const X27: u32 = 27;
const X28: u32 = 28;

const D8: u32 = 8;
const D9: u32 = 9;
const D10: u32 = 10;
const D11: u32 = 11;
const D12: u32 = 12;
const D13: u32 = 13;
const D14: u32 = 14;
const D15: u32 = 15;

/// Flag bit in the `flags` argument of the SME2 GEMM micro-kernels:
/// when set (`flags & 1 != 0`), accumulate `C += A * B`; when clear, overwrite `C = A * B`.
pub const SME2_GEMM_FLAG_ACCUM: i64 = 1;

// ---------------------------------------------------------------------------
// 1. SME / SME2 Streaming Mode & ZA Array Control
// ---------------------------------------------------------------------------

/// `smstart` (`msr SVCRSMZA, #1`): enter Streaming SVE mode AND enable `ZA` storage.
#[inline]
pub const fn smstart() -> u32 {
    0xd503_477f
}

/// `smstop` (`msr SVCRSMZA, #0`): exit Streaming SVE mode AND disable `ZA` storage.
#[inline]
pub const fn smstop() -> u32 {
    0xd503_467f
}

/// `smstart sm` (`msr SVCRSM, #1`): enter Streaming SVE mode only.
#[inline]
pub const fn smstart_sm() -> u32 {
    0xd503_437f
}

/// `smstop sm` (`msr SVCRSM, #0`): exit Streaming SVE mode only.
#[inline]
pub const fn smstop_sm() -> u32 {
    0xd503_427f
}

/// `smstart za` (`msr SVCRZA, #1`): enable `ZA` storage only.
#[inline]
pub const fn smstart_za() -> u32 {
    0xd503_457f
}

/// `smstop za` (`msr SVCRZA, #0`): disable `ZA` storage only.
#[inline]
pub const fn smstop_za() -> u32 {
    0xd503_447f
}

/// `rdsvl xd, #imm6`: set `xd = imm6 * SVL_bytes` (`imm6` in `-32..=31`).
#[inline]
pub const fn rdsvl(xd: u32, imm6: i32) -> u32 {
    0x04bf_5800 | ((imm6.cast_unsigned() & 0x3f) << 5) | (xd & 31)
}

/// `addvl xd, xn, #imm6`: set `xd = xn + imm6 * SVL_bytes` (`imm6` in `-32..=31`, `xd`/`xn` may be `SP`).
#[inline]
pub const fn addvl(xd: u32, xn: u32, imm6: i32) -> u32 {
    0x0420_5000 | ((xn & 31) << 16) | ((imm6.cast_unsigned() & 0x3f) << 5) | (xd & 31)
}

/// `cntb xd`: set `xd = SVL_bytes` (8-bit element count per streaming vector).
#[inline]
pub const fn cntb(xd: u32) -> u32 {
    0x0420_e3e0 | (xd & 31)
}

/// `cntw xd`: set `xd = SVL_bytes / 4` (32-bit element count per streaming vector).
#[inline]
pub const fn cntw(xd: u32) -> u32 {
    0x04a0_e3e0 | (xd & 31)
}

/// `cntd xd`: set `xd = SVL_bytes / 8` (64-bit element count per streaming vector).
#[inline]
pub const fn cntd(xd: u32) -> u32 {
    0x04e0_e3e0 | (xd & 31)
}

/// `zero {za}`: clear the entire `ZA` array in one instruction.
#[inline]
pub const fn zero_za_all() -> u32 {
    0xc008_00ff
}

/// `zero { <tiles> }`: clear the 64-bit `ZA` tiles selected by `mask8` (bit `k` -> `za{k}.d`).
#[inline]
pub const fn zero_za_mask(mask8: u32) -> u32 {
    0xc008_0000 | (mask8 & 0xff)
}

/// `zero { za*.s }`: clear the 32-bit `ZA` tiles selected by `mask4` (bit `t` in `0..=3` -> `za{t}.s`).
///
/// Each 32-bit tile `za{t}.s` aliases 64-bit tiles `za{t}.d` and `za{t+4}.d`.
#[inline]
pub const fn zero_za_s_tiles(mask4: u32) -> u32 {
    let m = mask4 & 0xf;
    zero_za_mask(m | (m << 4))
}

/// `zero { za*.d }`: clear the 64-bit `ZA` tiles selected by `mask8` (bit `t` in `0..=7` -> `za{t}.d`).
#[inline]
pub const fn zero_za_d_tiles(mask8: u32) -> u32 {
    zero_za_mask(mask8)
}

/// `sub sp, sp, xm, uxtx`: dynamic stack adjustment by an unsigned 64-bit register offset.
#[inline]
const fn sub_sp_uxtx(rm: u32) -> u32 {
    0xcb20_63ff | ((rm & 31) << 16)
}

// ---------------------------------------------------------------------------
// 2. Streaming SVE Predicates, Vector Length Increments & Contiguous LD1/ST1
// ---------------------------------------------------------------------------

/// `ptrue pd.b, all`: set all 8-bit predicate lanes of `pd` (`0..=15`) to true.
#[inline]
pub const fn ptrue_b(pd: u32) -> u32 {
    0x2518_e3e0 | (pd & 15)
}

/// `ptrue pd.s, all`: set all 32-bit predicate lanes of `pd` (`0..=15`) to true.
#[inline]
pub const fn ptrue_s(pd: u32) -> u32 {
    0x2598_e3e0 | (pd & 15)
}

/// `ptrue pd.d, all`: set all 64-bit predicate lanes of `pd` (`0..=15`) to true.
#[inline]
pub const fn ptrue_d(pd: u32) -> u32 {
    0x25d8_e3e0 | (pd & 15)
}

/// `ptrue pn{pnd}.b`: set SME2 predicate-as-counter `pnd` (`8..=15`) to all-true 8-bit count.
#[inline]
pub const fn ptrue_pn_b(pnd: u32) -> u32 {
    0x2520_7810 | (pnd & 7)
}

/// `ptrue pn{pnd}.s`: set SME2 predicate-as-counter `pnd` (`8..=15`) to all-true 32-bit count.
#[inline]
pub const fn ptrue_pn_s(pnd: u32) -> u32 {
    0x25a0_7810 | (pnd & 7)
}

/// `whilelt pd.b, xn, xm`: signed 64-bit loop predicate (`xn + lane < xm`, 8-bit elements).
#[inline]
pub const fn whilelt_b(pd: u32, rn: u32, rm: u32) -> u32 {
    0x2520_1400 | ((rm & 31) << 16) | ((rn & 31) << 5) | (pd & 15)
}

/// `whilelt pd.s, xn, xm`: signed 64-bit loop predicate (`xn + lane < xm`, 32-bit elements).
#[inline]
pub const fn whilelt_s(pd: u32, rn: u32, rm: u32) -> u32 {
    0x25a0_1400 | ((rm & 31) << 16) | ((rn & 31) << 5) | (pd & 15)
}

/// `whilelt pd.d, xn, xm`: signed 64-bit loop predicate (`xn + lane < xm`, 64-bit elements).
#[inline]
pub const fn whilelt_d(pd: u32, rn: u32, rm: u32) -> u32 {
    0x25e0_1400 | ((rm & 31) << 16) | ((rn & 31) << 5) | (pd & 15)
}

/// `whilelt pn{pnd}.b, xn, xm, vlx2`: SME2 2-vector predicate-as-counter (`pnd` in `8..=15`, 8-bit).
#[inline]
pub const fn whilelt_pn_b_vlx2(pnd: u32, rn: u32, rm: u32) -> u32 {
    0x2520_4410 | ((rm & 31) << 16) | ((rn & 31) << 5) | (pnd & 7)
}

/// `whilelt pn{pnd}.s, xn, xm, vlx2`: SME2 2-vector predicate-as-counter (`pnd` in `8..=15`, 32-bit).
#[inline]
pub const fn whilelt_pn_s_vlx2(pnd: u32, rn: u32, rm: u32) -> u32 {
    0x25a0_4410 | ((rm & 31) << 16) | ((rn & 31) << 5) | (pnd & 7)
}

/// `whilelo pd.b, xn, xm`: unsigned 64-bit loop predicate (`xn + lane < xm`, 8-bit elements).
#[inline]
pub const fn whilelo_b(pd: u32, rn: u32, rm: u32) -> u32 {
    0x2520_1c00 | ((rm & 31) << 16) | ((rn & 31) << 5) | (pd & 15)
}

/// `whilelo pd.s, xn, xm`: unsigned 64-bit loop predicate (`xn + lane < xm`, 32-bit elements).
#[inline]
pub const fn whilelo_s(pd: u32, rn: u32, rm: u32) -> u32 {
    0x25a0_1c00 | ((rm & 31) << 16) | ((rn & 31) << 5) | (pd & 15)
}

/// `whilelo pd.d, xn, xm`: unsigned 64-bit loop predicate (`xn + lane < xm`, 64-bit elements).
#[inline]
pub const fn whilelo_d(pd: u32, rn: u32, rm: u32) -> u32 {
    0x25e0_1c00 | ((rm & 31) << 16) | ((rn & 31) << 5) | (pd & 15)
}

/// `ld1b {zt.b}, pg/z, [xn, #imm4_vl, mul vl]` (`imm4_vl` in `-8..=7`, `pg` in `0..=7`).
#[inline]
pub const fn ld1b_imm(zt: u32, pg: u32, rn: u32, imm4_vl: i32) -> u32 {
    0xa400_a000
        | ((imm4_vl.cast_unsigned() & 0xf) << 16)
        | ((pg & 7) << 10)
        | ((rn & 31) << 5)
        | (zt & 31)
}

/// `st1b {zt.b}, pg, [xn, #imm4_vl, mul vl]` (`imm4_vl` in `-8..=7`, `pg` in `0..=7`).
#[inline]
pub const fn st1b_imm(zt: u32, pg: u32, rn: u32, imm4_vl: i32) -> u32 {
    0xe400_e000
        | ((imm4_vl.cast_unsigned() & 0xf) << 16)
        | ((pg & 7) << 10)
        | ((rn & 31) << 5)
        | (zt & 31)
}

/// `ld1w {zt.s}, pg/z, [xn, #imm4_vl, mul vl]` (`imm4_vl` in `-8..=7`, `pg` in `0..=7`).
#[inline]
pub const fn ld1w_imm(zt: u32, pg: u32, rn: u32, imm4_vl: i32) -> u32 {
    0xa540_a000
        | ((imm4_vl.cast_unsigned() & 0xf) << 16)
        | ((pg & 7) << 10)
        | ((rn & 31) << 5)
        | (zt & 31)
}

/// `ld1w {zt.s}, pg/z, [xn, xm, lsl #2]` (`pg` in `0..=7`).
#[inline]
pub const fn ld1w_reg(zt: u32, pg: u32, rn: u32, rm: u32) -> u32 {
    0xa540_4000 | ((rm & 31) << 16) | ((pg & 7) << 10) | ((rn & 31) << 5) | (zt & 31)
}

/// `st1w {zt.s}, pg, [xn, #imm4_vl, mul vl]` (`imm4_vl` in `-8..=7`, `pg` in `0..=7`).
#[inline]
pub const fn st1w_imm(zt: u32, pg: u32, rn: u32, imm4_vl: i32) -> u32 {
    0xe540_e000
        | ((imm4_vl.cast_unsigned() & 0xf) << 16)
        | ((pg & 7) << 10)
        | ((rn & 31) << 5)
        | (zt & 31)
}

/// `st1w {zt.s}, pg, [xn, xm, lsl #2]` (`pg` in `0..=7`).
#[inline]
pub const fn st1w_reg(zt: u32, pg: u32, rn: u32, rm: u32) -> u32 {
    0xe540_4000 | ((rm & 31) << 16) | ((pg & 7) << 10) | ((rn & 31) << 5) | (zt & 31)
}

/// `ld1w {zt.s, z(t+1).s}, pn{pn}/z, [xn, #(imm4_2vl*2), mul vl]` (SME2 2-vector load).
///
/// `zt` must be even (`0..=30`), `pn` in `8..=15`, `imm4_2vl` in `-8..=7` (units of `2 * VL`).
#[inline]
pub const fn ld1w_x2_imm(zt: u32, pn: u32, rn: u32, imm4_2vl: i32) -> u32 {
    0xa040_4000
        | ((imm4_2vl.cast_unsigned() & 0xf) << 16)
        | ((pn & 7) << 10)
        | ((rn & 31) << 5)
        | (zt & 0x1e)
}

/// `ld1w {zt.s-z(t+3).s}, pn{pn}/z, [xn, #(imm4_4vl*4), mul vl]` (SME2 4-vector load).
///
/// `zt` must be a multiple of 4 (`0..=28`), `pn` in `8..=15`, `imm4_4vl` in `-8..=7` (units of `4 * VL`).
#[inline]
pub const fn ld1w_x4_imm(zt: u32, pn: u32, rn: u32, imm4_4vl: i32) -> u32 {
    0xa040_c000
        | ((imm4_4vl.cast_unsigned() & 0xf) << 16)
        | ((pn & 7) << 10)
        | ((rn & 31) << 5)
        | (zt & 0x1c)
}

/// `st1w {zt.s, z(t+1).s}, pn{pn}, [xn, #(imm4_2vl*2), mul vl]` (SME2 2-vector store).
///
/// `zt` must be even (`0..=30`), `pn` in `8..=15`, `imm4_2vl` in `-8..=7` (units of `2 * VL`).
#[inline]
pub const fn st1w_x2_imm(zt: u32, pn: u32, rn: u32, imm4_2vl: i32) -> u32 {
    0xa060_4000
        | ((imm4_2vl.cast_unsigned() & 0xf) << 16)
        | ((pn & 7) << 10)
        | ((rn & 31) << 5)
        | (zt & 0x1e)
}

/// `st1w {zt.s-z(t+3).s}, pn{pn}, [xn, #(imm4_4vl*4), mul vl]` (SME2 4-vector store).
///
/// `zt` must be a multiple of 4 (`0..=28`), `pn` in `8..=15`, `imm4_4vl` in `-8..=7` (units of `4 * VL`).
#[inline]
pub const fn st1w_x4_imm(zt: u32, pn: u32, rn: u32, imm4_4vl: i32) -> u32 {
    0xa060_c000
        | ((imm4_4vl.cast_unsigned() & 0xf) << 16)
        | ((pn & 7) << 10)
        | ((rn & 31) << 5)
        | (zt & 0x1c)
}

/// `ld1d {zt.d}, pg/z, [xn, #imm4_vl, mul vl]` (`imm4_vl` in `-8..=7`, `pg` in `0..=7`).
#[inline]
pub const fn ld1d_imm(zt: u32, pg: u32, rn: u32, imm4_vl: i32) -> u32 {
    0xa5e0_a000
        | ((imm4_vl.cast_unsigned() & 0xf) << 16)
        | ((pg & 7) << 10)
        | ((rn & 31) << 5)
        | (zt & 31)
}

/// `ld1d {zt.d}, pg/z, [xn, xm, lsl #3]` (`pg` in `0..=7`).
#[inline]
pub const fn ld1d_reg(zt: u32, pg: u32, rn: u32, rm: u32) -> u32 {
    0xa5e0_4000 | ((rm & 31) << 16) | ((pg & 7) << 10) | ((rn & 31) << 5) | (zt & 31)
}

/// `st1d {zt.d}, pg, [xn, #imm4_vl, mul vl]` (`imm4_vl` in `-8..=7`, `pg` in `0..=7`).
#[inline]
pub const fn st1d_imm(zt: u32, pg: u32, rn: u32, imm4_vl: i32) -> u32 {
    0xe5e0_e000
        | ((imm4_vl.cast_unsigned() & 0xf) << 16)
        | ((pg & 7) << 10)
        | ((rn & 31) << 5)
        | (zt & 31)
}

/// `st1d {zt.d}, pg, [xn, xm, lsl #3]` (`pg` in `0..=7`).
#[inline]
pub const fn st1d_reg(zt: u32, pg: u32, rn: u32, rm: u32) -> u32 {
    0xe5e0_4000 | ((rm & 31) << 16) | ((pg & 7) << 10) | ((rn & 31) << 5) | (zt & 31)
}

/// `incb xd`: add `SVL_bytes` (8-bit lane count) to `xd`.
#[inline]
pub const fn incb(xd: u32) -> u32 {
    0x0430_e3e0 | (xd & 31)
}

/// `incb xd, all, mul #mul`: add `mul * SVL_bytes` (`mul` in `1..=16`) to `xd`.
#[inline]
pub const fn incb_mul(xd: u32, mul: u32) -> u32 {
    let imm4 = mul.saturating_sub(1) & 0xf;
    0x0430_e3e0 | (imm4 << 16) | (xd & 31)
}

/// `incw xd`: add `SVL_bytes / 4` (32-bit lane count) to `xd`.
#[inline]
pub const fn incw(xd: u32) -> u32 {
    0x04b0_e3e0 | (xd & 31)
}

/// `incw xd, all, mul #mul`: add `mul * (SVL_bytes / 4)` (`mul` in `1..=16`) to `xd`.
#[inline]
pub const fn incw_mul(xd: u32, mul: u32) -> u32 {
    let imm4 = mul.saturating_sub(1) & 0xf;
    0x04b0_e3e0 | (imm4 << 16) | (xd & 31)
}

/// `incd xd`: add `SVL_bytes / 8` (64-bit lane count) to `xd`.
#[inline]
pub const fn incd(xd: u32) -> u32 {
    0x04f0_e3e0 | (xd & 31)
}

/// `incd xd, all, mul #mul`: add `mul * (SVL_bytes / 8)` (`mul` in `1..=16`) to `xd`.
#[inline]
pub const fn incd_mul(xd: u32, mul: u32) -> u32 {
    let imm4 = mul.saturating_sub(1) & 0xf;
    0x04f0_e3e0 | (imm4 << 16) | (xd & 31)
}

/// `zip1 zd.b, zn.b, zm.b`: interleave low half bytes of `zn` and `zm`.
#[inline]
pub const fn zip1_b(zd: u32, zn: u32, zm: u32) -> u32 {
    0x0520_6000 | ((zm & 31) << 16) | ((zn & 31) << 5) | (zd & 31)
}

/// `zip1 zd.h, zn.h, zm.h`: interleave low half 16-bit elements of `zn` and `zm`.
#[inline]
pub const fn zip1_h(zd: u32, zn: u32, zm: u32) -> u32 {
    0x0560_6000 | ((zm & 31) << 16) | ((zn & 31) << 5) | (zd & 31)
}

/// `zip2 zd.h, zn.h, zm.h`: interleave high half 16-bit elements of `zn` and `zm`.
#[inline]
pub const fn zip2_h(zd: u32, zn: u32, zm: u32) -> u32 {
    0x0560_6400 | ((zm & 31) << 16) | ((zn & 31) << 5) | (zd & 31)
}

/// `mov zd.b, #0` (`dup zd.b, #0`): zero all lanes of streaming vector `zd`.
#[inline]
pub const fn dup_zero_b(zd: u32) -> u32 {
    0x2538_c000 | (zd & 31)
}

// ---------------------------------------------------------------------------
// 3. SME `FMOPA` / `SMOPA` / `UMOPA` Outer-Product & `MOVA` Tile Read/Write
// ---------------------------------------------------------------------------

/// `fmopa za{zada}.s, pn/m, pm/m, zn.s, zm.s`:
/// single-precision outer-product accumulate (`zada` in `0..=3`, `pn`/`pm` in `0..=7`).
#[inline]
pub const fn fmopa_s(zada: u32, pn: u32, pm: u32, zn: u32, zm: u32) -> u32 {
    0x8080_0000
        | ((zm & 31) << 16)
        | ((pm & 7) << 13)
        | ((pn & 7) << 10)
        | ((zn & 31) << 5)
        | (zada & 3)
}

/// `fmopa za{zada}.d, pn/m, pm/m, zn.d, zm.d`:
/// double-precision outer-product accumulate (`FEAT_SME_F64F64`; `zada` in `0..=7`, `pn`/`pm` in `0..=7`).
#[inline]
pub const fn fmopa_d(zada: u32, pn: u32, pm: u32, zn: u32, zm: u32) -> u32 {
    0x80c0_0000
        | ((zm & 31) << 16)
        | ((pm & 7) << 13)
        | ((pn & 7) << 10)
        | ((zn & 31) << 5)
        | (zada & 7)
}

/// `smopa za{zada}.s, pn/m, pm/m, zn.b, zm.b`:
/// signed `i8`×`i8` -> `i32` 4-way outer-product accumulate (`zada` in `0..=3`, `pn`/`pm` in `0..=7`).
#[inline]
pub const fn smopa_s(zada: u32, pn: u32, pm: u32, zn: u32, zm: u32) -> u32 {
    0xa080_0000
        | ((zm & 31) << 16)
        | ((pm & 7) << 13)
        | ((pn & 7) << 10)
        | ((zn & 31) << 5)
        | (zada & 3)
}

/// `umopa za{zada}.s, pn/m, pm/m, zn.b, zm.b`:
/// unsigned `u8`×`u8` -> `u32` 4-way outer-product accumulate (`zada` in `0..=3`, `pn`/`pm` in `0..=7`).
#[inline]
pub const fn umopa_s(zada: u32, pn: u32, pm: u32, zn: u32, zm: u32) -> u32 {
    0xa1a0_0000
        | ((zm & 31) << 16)
        | ((pm & 7) << 13)
        | ((pn & 7) << 10)
        | ((zn & 31) << 5)
        | (zada & 3)
}

/// `mova zd.s, pg/m, za{tile}h.s[w{12+rv}, #imm2]` (horizontal 32-bit tile slice to vector).
///
/// `tile` in `0..=3`, `rv` in `0..=3` (`w12..=w15`), `imm2` in `0..=3`, `pg` in `0..=7`.
#[inline]
pub const fn mova_tile_to_z_h_s(zd: u32, pg: u32, tile: u32, rv: u32, imm2: u32) -> u32 {
    0xc082_0000
        | ((rv & 3) << 13)
        | ((pg & 7) << 10)
        | ((tile & 3) << 7)
        | ((imm2 & 3) << 5)
        | (zd & 31)
}

/// `mova zd.s, pg/m, za{tile}v.s[w{12+rv}, #imm2]` (vertical 32-bit tile slice to vector).
///
/// `tile` in `0..=3`, `rv` in `0..=3` (`w12..=w15`), `imm2` in `0..=3`, `pg` in `0..=7`.
#[inline]
pub const fn mova_tile_to_z_v_s(zd: u32, pg: u32, tile: u32, rv: u32, imm2: u32) -> u32 {
    0xc082_8000
        | ((rv & 3) << 13)
        | ((pg & 7) << 10)
        | ((tile & 3) << 7)
        | ((imm2 & 3) << 5)
        | (zd & 31)
}

/// `mova za{tile}h.s[w{12+rv}, #imm2], pg/m, zn.s` (vector to horizontal 32-bit tile slice).
///
/// `tile` in `0..=3`, `rv` in `0..=3` (`w12..=w15`), `imm2` in `0..=3`, `pg` in `0..=7`.
#[inline]
pub const fn mova_z_to_tile_h_s(tile: u32, rv: u32, imm2: u32, pg: u32, zn: u32) -> u32 {
    0xc080_0000
        | ((rv & 3) << 13)
        | ((pg & 7) << 10)
        | ((zn & 31) << 5)
        | ((tile & 3) << 2)
        | (imm2 & 3)
}

/// `mova za{tile}v.s[w{12+rv}, #imm2], pg/m, zn.s` (vector to vertical 32-bit tile slice).
///
/// `tile` in `0..=3`, `rv` in `0..=3` (`w12..=w15`), `imm2` in `0..=3`, `pg` in `0..=7`.
#[inline]
pub const fn mova_z_to_tile_v_s(tile: u32, rv: u32, imm2: u32, pg: u32, zn: u32) -> u32 {
    0xc080_8000
        | ((rv & 3) << 13)
        | ((pg & 7) << 10)
        | ((zn & 31) << 5)
        | ((tile & 3) << 2)
        | (imm2 & 3)
}

/// `mova zd.d, pg/m, za{tile}h.d[w{12+rv}, #imm1]` (horizontal 64-bit tile slice to vector).
///
/// `tile` in `0..=7`, `rv` in `0..=3` (`w12..=w15`), `imm1` in `0..=1`, `pg` in `0..=7`.
#[inline]
pub const fn mova_tile_to_z_h_d(zd: u32, pg: u32, tile: u32, rv: u32, imm1: u32) -> u32 {
    0xc0c2_0000
        | ((rv & 3) << 13)
        | ((pg & 7) << 10)
        | ((tile & 7) << 6)
        | ((imm1 & 1) << 5)
        | (zd & 31)
}

/// `mova zd.d, pg/m, za{tile}v.d[w{12+rv}, #imm1]` (vertical 64-bit tile slice to vector).
///
/// `tile` in `0..=7`, `rv` in `0..=3` (`w12..=w15`), `imm1` in `0..=1`, `pg` in `0..=7`.
#[inline]
pub const fn mova_tile_to_z_v_d(zd: u32, pg: u32, tile: u32, rv: u32, imm1: u32) -> u32 {
    0xc0c2_8000
        | ((rv & 3) << 13)
        | ((pg & 7) << 10)
        | ((tile & 7) << 6)
        | ((imm1 & 1) << 5)
        | (zd & 31)
}

/// `mova za{tile}h.d[w{12+rv}, #imm1], pg/m, zn.d` (vector to horizontal 64-bit tile slice).
///
/// `tile` in `0..=7`, `rv` in `0..=3` (`w12..=w15`), `imm1` in `0..=1`, `pg` in `0..=7`.
#[inline]
pub const fn mova_z_to_tile_h_d(tile: u32, rv: u32, imm1: u32, pg: u32, zn: u32) -> u32 {
    0xc0c0_0000
        | ((rv & 3) << 13)
        | ((pg & 7) << 10)
        | ((zn & 31) << 5)
        | ((tile & 7) << 1)
        | (imm1 & 1)
}

/// `mova za{tile}v.d[w{12+rv}, #imm1], pg/m, zn.d` (vector to vertical 64-bit tile slice).
///
/// `tile` in `0..=7`, `rv` in `0..=3` (`w12..=w15`), `imm1` in `0..=1`, `pg` in `0..=7`.
#[inline]
pub const fn mova_z_to_tile_v_d(tile: u32, rv: u32, imm1: u32, pg: u32, zn: u32) -> u32 {
    0xc0c0_8000
        | ((rv & 3) << 13)
        | ((pg & 7) << 10)
        | ((zn & 31) << 5)
        | ((tile & 7) << 1)
        | (imm1 & 1)
}

// ---------------------------------------------------------------------------
// 4. NEON SIMD Idiom Encoders (`TBL`, `SDOT`, `UDOT`, `SMMLA`, `UMMLA`)
// ---------------------------------------------------------------------------

/// `tbl vd.16b, {vn.16b}, vm.16b`: 1-register (16-byte) vector table lookup.
#[inline]
pub const fn tbl1_16b(vd: u32, vn: u32, vm: u32) -> u32 {
    0x4e00_0000 | ((vm & 31) << 16) | ((vn & 31) << 5) | (vd & 31)
}

/// `tbl vd.16b, {vn.16b, v(n+1).16b}, vm.16b`: 2-register (32-byte) vector table lookup.
#[inline]
pub const fn tbl2_16b(vd: u32, vn: u32, vm: u32) -> u32 {
    0x4e00_2000 | ((vm & 31) << 16) | ((vn & 31) << 5) | (vd & 31)
}

/// `tbl vd.16b, {vn.16b, v(n+1).16b, v(n+2).16b}, vm.16b`: 3-register (48-byte) vector table lookup.
#[inline]
pub const fn tbl3_16b(vd: u32, vn: u32, vm: u32) -> u32 {
    0x4e00_4000 | ((vm & 31) << 16) | ((vn & 31) << 5) | (vd & 31)
}

/// `tbl vd.16b, {vn.16b, v(n+1).16b, v(n+2).16b, v(n+3).16b}, vm.16b`: 4-register (64-byte) vector table lookup.
#[inline]
pub const fn tbl4_16b(vd: u32, vn: u32, vm: u32) -> u32 {
    0x4e00_6000 | ((vm & 31) << 16) | ((vn & 31) << 5) | (vd & 31)
}

/// `sdot vd.4s, vn.16b, vm.16b`: signed `i8`×`i8` -> `i32` 4-way dot product accumulate (`FEAT_DotProd`).
#[inline]
pub const fn sdot_4s(vd: u32, vn: u32, vm: u32) -> u32 {
    0x4e80_9400 | ((vm & 31) << 16) | ((vn & 31) << 5) | (vd & 31)
}

/// `udot vd.4s, vn.16b, vm.16b`: unsigned `u8`×`u8` -> `u32` 4-way dot product accumulate (`FEAT_DotProd`).
#[inline]
pub const fn udot_4s(vd: u32, vn: u32, vm: u32) -> u32 {
    0x6e80_9400 | ((vm & 31) << 16) | ((vn & 31) << 5) | (vd & 31)
}

/// `smmla vd.4s, vn.16b, vm.16b`: signed `2×8` × `8×2` `i8` -> `2×2` `i32` matrix multiply-accumulate (`FEAT_I8MM`).
#[inline]
pub const fn smmla_4s(vd: u32, vn: u32, vm: u32) -> u32 {
    0x4e80_a400 | ((vm & 31) << 16) | ((vn & 31) << 5) | (vd & 31)
}

/// `ummla vd.4s, vn.16b, vm.16b`: unsigned `2×8` × `8×2` `u8` -> `2×2` `u32` matrix multiply-accumulate (`FEAT_I8MM`).
#[inline]
pub const fn ummla_4s(vd: u32, vn: u32, vm: u32) -> u32 {
    0x6e80_a400 | ((vm & 31) << 16) | ((vn & 31) << 5) | (vd & 31)
}

// ---------------------------------------------------------------------------
// 5. Standalone SVL-Agnostic SME2 GEMM Micro-Kernel Emitters
// ---------------------------------------------------------------------------

struct SmeBuf<'a> {
    words: AVec<'a, u32>,
}

impl<'a> SmeBuf<'a> {
    fn new_in(cap: usize, alloc: Alloc<'a>) -> Self {
        Self { words: Vec::with_capacity_in(cap, alloc) }
    }

    #[inline]
    fn pos(&self) -> usize {
        self.words.len()
    }

    #[inline]
    fn emit(&mut self, word: u32) {
        self.words.push(word);
    }

    #[inline]
    fn rel_words(at: usize, target: usize) -> Option<i32> {
        let a = i32::try_from(at).ok()?;
        let t = i32::try_from(target).ok()?;
        t.checked_sub(a)
    }

    #[inline]
    fn emit_b_to(&mut self, target: usize) {
        let at = self.pos();
        if let Some(delta) = Self::rel_words(at, target) {
            self.emit(enc::b(delta));
        }
    }

    #[inline]
    fn emit_b_cond_to(&mut self, cond: Cond, target: usize) {
        let at = self.pos();
        if let Some(delta) = Self::rel_words(at, target) {
            self.emit(enc::b_cond(cond, delta));
        }
    }

    #[inline]
    fn patch_b(&mut self, at: usize, target: usize) {
        if let Some(delta) = Self::rel_words(at, target)
            && let Some(slot) = self.words.get_mut(at)
        {
            *slot = enc::b(delta);
        }
    }

    #[inline]
    fn patch_b_cond(&mut self, at: usize, cond: Cond, target: usize) {
        if let Some(delta) = Self::rel_words(at, target)
            && let Some(slot) = self.words.get_mut(at)
        {
            *slot = enc::b_cond(cond, delta);
        }
    }

    #[inline]
    fn patch_cbz(&mut self, at: usize, rt: u32, target: usize) {
        if let Some(delta) = Self::rel_words(at, target)
            && let Some(slot) = self.words.get_mut(at)
        {
            *slot = enc::cbz(rt, delta);
        }
    }

    #[inline]
    fn patch_cbnz(&mut self, at: usize, rt: u32, target: usize) {
        if let Some(delta) = Self::rel_words(at, target)
            && let Some(slot) = self.words.get_mut(at)
        {
            *slot = enc::cbnz(rt, delta);
        }
    }

    #[inline]
    fn patch_tbz(&mut self, at: usize, rt: u32, bit: u32, target: usize) {
        if let Some(delta) = Self::rel_words(at, target)
            && let Some(slot) = self.words.get_mut(at)
        {
            *slot = enc::tbz(rt, bit, delta);
        }
    }

    #[inline]
    fn patch_tbnz(&mut self, at: usize, rt: u32, bit: u32, target: usize) {
        if let Some(delta) = Self::rel_words(at, target)
            && let Some(slot) = self.words.get_mut(at)
        {
            *slot = enc::tbnz(rt, bit, delta);
        }
    }

    fn into_words(self) -> AVec<'a, u32> {
        self.words
    }
}

fn words_to_bytes<'a>(words: &[u32], alloc: Alloc<'a>) -> AVec<'a, u8> {
    let mut out = Vec::with_capacity_in(words.len().saturating_mul(4), alloc);
    for &w in words {
        for byte in w.to_le_bytes() {
            out.push(byte);
        }
    }
    out
}

/// Emits early-exit check + AAPCS64 streaming prologue saving FP/LR, X19..=X28, D8..=D15,
/// copying arguments `(c, a, b, m, n, k, flags)` into `X19..=X25`, and executing `smstart`.
fn emit_gemm_prologue(buf: &mut SmeBuf<'_>) {
    // Early exit before `smstart` for empty output or no-op `k <= 0`:
    //   if m <= 0 || n <= 0 || (k <= 0 && (flags & 1) != 0) { return; }
    buf.emit(enc::cmp_imm(X3, 0));
    let br_m_le = buf.pos();
    buf.emit(enc::b_cond(Cond::Le, 0));

    buf.emit(enc::cmp_imm(X4, 0));
    let br_n_le = buf.pos();
    buf.emit(enc::b_cond(Cond::Le, 0));

    buf.emit(enc::cmp_imm(X5, 0));
    let br_k_gt = buf.pos();
    buf.emit(enc::b_cond(Cond::Gt, 0));

    let br_k_accum = buf.pos();
    buf.emit(enc::tbnz(X6, 0, 0));

    let br_to_prologue = buf.pos();
    buf.emit(enc::b(0));

    let early_ret = buf.pos();
    buf.emit(enc::ret());

    // Prologue: save FP/LR, X19..=X28, and D8..=D15 BEFORE `smstart`
    // (160-byte 16B-aligned frame; `smstart` zeroes `v0..=v31`).
    let prologue = buf.pos();
    buf.patch_b_cond(br_m_le, Cond::Le, early_ret);
    buf.patch_b_cond(br_n_le, Cond::Le, early_ret);
    buf.patch_b_cond(br_k_gt, Cond::Gt, prologue);
    buf.patch_tbnz(br_k_accum, X6, 0, early_ret);
    buf.patch_b(br_to_prologue, prologue);

    buf.emit(enc::stp_pre(FP, LR, SP, -160));
    buf.emit(enc::add_imm(FP, SP, 0, false));
    buf.emit(enc::stp_off(X19, X20, SP, 16));
    buf.emit(enc::stp_off(X21, X22, SP, 32));
    buf.emit(enc::stp_off(X23, X24, SP, 48));
    buf.emit(enc::stp_off(X25, X26, SP, 64));
    buf.emit(enc::stp_off(X27, X28, SP, 80));
    buf.emit(enc::stp_d_off(D8, D9, SP, 96));
    buf.emit(enc::stp_d_off(D10, D11, SP, 112));
    buf.emit(enc::stp_d_off(D12, D13, SP, 128));
    buf.emit(enc::stp_d_off(D14, D15, SP, 144));

    buf.emit(enc::mov(X19, X0)); // c_base
    buf.emit(enc::mov(X20, X1)); // a_base
    buf.emit(enc::mov(X21, X2)); // b_base
    buf.emit(enc::mov(X22, X3)); // m
    buf.emit(enc::mov(X23, X4)); // n
    buf.emit(enc::mov(X24, X5)); // k
    buf.emit(enc::mov(X25, X6)); // flags

    buf.emit(smstart());
}

/// Emits `smstop`, optional dynamic stack release (`mov sp, fp`), callee-saved restore, and `ret`.
fn emit_gemm_epilogue(buf: &mut SmeBuf<'_>, restore_sp: bool) {
    buf.emit(smstop());

    if restore_sp {
        buf.emit(enc::add_imm(SP, FP, 0, false)); // mov sp, x29
    }

    buf.emit(enc::ldp_d_off(D14, D15, SP, 144));
    buf.emit(enc::ldp_d_off(D12, D13, SP, 128));
    buf.emit(enc::ldp_d_off(D10, D11, SP, 112));
    buf.emit(enc::ldp_d_off(D8, D9, SP, 96));
    buf.emit(enc::ldp_off(X27, X28, SP, 80));
    buf.emit(enc::ldp_off(X25, X26, SP, 64));
    buf.emit(enc::ldp_off(X23, X24, SP, 48));
    buf.emit(enc::ldp_off(X21, X22, SP, 32));
    buf.emit(enc::ldp_off(X19, X20, SP, 16));
    buf.emit(enc::ldp_post(FP, LR, SP, 160));
    buf.emit(enc::ret());
}

/// Probes and allocates `X13` bytes (`>= 128`, 16B-aligned) on `SP` in 4 KiB chunks,
/// returning a 128-byte-aligned base pointer in `X17` (cache-line aligned on Apple Silicon).
fn emit_stack_probe_align_128(buf: &mut SmeBuf<'_>) {
    let probe_loop = buf.pos();
    buf.emit(enc::subs_imm(X14, X13, 1, true)); // X14 = X13 - 4096
    let br_probe_last = buf.pos();
    buf.emit(enc::b_cond(Cond::Ls, 0)); // if X13 <= 4096 -> probe_last
    buf.emit(enc::sub_imm(SP, SP, 1, true)); // sp -= 4096
    buf.emit(enc::str(Size::X, X14, SP, 0)); // touch guard page
    buf.emit(enc::mov(X13, X14)); // X13 -= 4096
    buf.emit_b_to(probe_loop);

    let probe_last = buf.pos();
    buf.patch_b_cond(br_probe_last, Cond::Ls, probe_last);
    buf.emit(sub_sp_uxtx(X13)); // sp -= X13 (always in 128..=4096)
    buf.emit(enc::str(Size::X, X14, SP, 0)); // touch bottom page

    // X17 = (sp + 127) & !127 (128-byte aligned within [sp, sp + 128))
    buf.emit(enc::add_imm(X17, SP, 127, false));
    buf.emit(enc::and_imm(X17, X17, 1, 57, 56)); // and x17, x17, #-128
}

/// Computes `X13 = kb_max_groups` from `X26 = svl_s` such that `2 * svl_bytes * kb_max_groups <= 32 KiB`:
/// - `svl_s <= 16` (SVL <= 512b, e.g. Apple M4/M5): `16 * svl_s` (= 256 -> 32 KiB)
/// - `svl_s == 32` (SVL = 1024b): `4 * svl_s` (= 128 -> 32 KiB)
/// - `svl_s > 32` (SVL = 2048b): `svl_s` (= 64 -> 32 KiB)
fn emit_compute_kb_max_groups(buf: &mut SmeBuf<'_>, rd: u32) {
    buf.emit(enc::cmp_imm(X26, 16));
    buf.emit(enc::lsl_imm(X13, X26, 4));
    buf.emit(enc::lsl_imm(X14, X26, 2));
    buf.emit(enc::csel(rd, X13, X14, Cond::Le));
    buf.emit(enc::cmp_imm(X26, 32));
    buf.emit(enc::csel(rd, rd, X26, Cond::Le));
}

/// Drains `X11` (`1..=svl_s`) vertical 32-bit columns from `za0v.s` (top) and `za1v.s` (bot)
/// into the 128-byte-aligned `A_pack` cursor `X9`, using `p5 = ptrue.s` and `pn8 = ptrue pn8.s`.
///
/// Advances `X9` by `X11 * 2 * SVL_bytes`.
fn emit_drain_za01v_to_apack(buf: &mut SmeBuf<'_>) {
    buf.emit(enc::movz(X12, 0, 0)); // c = 0
    buf.emit(enc::sub_imm(X13, X11, 3, false)); // sub_k - 3
    buf.emit(enc::cmp(X12, X13));
    let br_tail = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));

    // 4x-unrolled drain: 8 MOVA + 2 4-vector ST1W (512B per iteration!).
    let drain4 = buf.pos();
    buf.emit(mova_tile_to_z_v_s(0, 5, 0, 0, 0));
    buf.emit(mova_tile_to_z_v_s(1, 5, 1, 0, 0));
    buf.emit(mova_tile_to_z_v_s(2, 5, 0, 0, 1));
    buf.emit(mova_tile_to_z_v_s(3, 5, 1, 0, 1));
    buf.emit(mova_tile_to_z_v_s(4, 5, 0, 0, 2));
    buf.emit(mova_tile_to_z_v_s(5, 5, 1, 0, 2));
    buf.emit(mova_tile_to_z_v_s(6, 5, 0, 0, 3));
    buf.emit(mova_tile_to_z_v_s(7, 5, 1, 0, 3));
    buf.emit(st1w_x4_imm(0, 8, X9, 0));
    buf.emit(st1w_x4_imm(4, 8, X9, 1));
    buf.emit(addvl(X9, X9, 8));
    buf.emit(enc::add_imm(X12, X12, 4, false));
    buf.emit(enc::cmp(X12, X13));
    buf.emit_b_cond_to(Cond::Lt, drain4);

    let tail = buf.pos();
    buf.patch_b_cond(br_tail, Cond::Ge, tail);
    buf.emit(enc::cmp(X12, X11));
    let br_done = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));

    let drain1 = buf.pos();
    buf.emit(mova_tile_to_z_v_s(0, 5, 0, 0, 0));
    buf.emit(mova_tile_to_z_v_s(1, 5, 1, 0, 0));
    buf.emit(st1w_x2_imm(0, 8, X9, 0));
    buf.emit(addvl(X9, X9, 2));
    buf.emit(enc::add_imm(X12, X12, 1, false));
    buf.emit(enc::cmp(X12, X11));
    buf.emit_b_cond_to(Cond::Lt, drain1);

    let done = buf.pos();
    buf.patch_b_cond(br_done, Cond::Ge, done);
}

/// Loads `top_rows` (`X1`) rows of `C[i.., j..]` into `za0h.s, za1h.s` and `bot_rows` (`X2`)
/// rows into `za2h.s, za3h.s` using 2-vector `ld1w` (`pn9`), `X9 = c_ij`, `X27 = c_row_bytes`, `p5 = ptrue.s`.
fn emit_load_c_2x2_s(buf: &mut SmeBuf<'_>) {
    buf.emit(enc::mov(X11, X9)); // c_ptr = c_ij
    buf.emit(enc::movz(X12, 0, 0)); // r = 0
    buf.emit(enc::sub_imm(X13, X1, 1, false));
    buf.emit(enc::cmp(X12, X13));
    let br_top_tail = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));

    let top_loop2 = buf.pos();
    buf.emit(enc::add(X14, X11, X27));
    buf.emit(ld1w_x2_imm(0, 9, X11, 0));
    buf.emit(ld1w_x2_imm(2, 9, X14, 0));
    buf.emit(mova_z_to_tile_h_s(0, 0, 0, 5, 0));
    buf.emit(mova_z_to_tile_h_s(1, 0, 0, 5, 1));
    buf.emit(mova_z_to_tile_h_s(0, 0, 1, 5, 2));
    buf.emit(mova_z_to_tile_h_s(1, 0, 1, 5, 3));
    buf.emit(enc::add(X11, X14, X27));
    buf.emit(enc::add_imm(X12, X12, 2, false));
    buf.emit(enc::cmp(X12, X13));
    buf.emit_b_cond_to(Cond::Lt, top_loop2);

    let top_tail = buf.pos();
    buf.patch_b_cond(br_top_tail, Cond::Ge, top_tail);
    buf.emit(enc::cmp(X12, X1));
    let br_top_done = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));
    buf.emit(ld1w_x2_imm(0, 9, X11, 0));
    buf.emit(mova_z_to_tile_h_s(0, 0, 0, 5, 0));
    buf.emit(mova_z_to_tile_h_s(1, 0, 0, 5, 1));

    let top_done = buf.pos();
    buf.patch_b_cond(br_top_done, Cond::Ge, top_done);

    // Bottom strip (when `bot_rows > 0`, `top_rows == svl_s` which is even, so `X11 == &C[i + svl_s, j]`).
    let br_no_bot = buf.pos();
    buf.emit(enc::cbz(X2, 0));

    buf.emit(enc::movz(X12, 0, 0));
    buf.emit(enc::sub_imm(X13, X2, 1, false));
    buf.emit(enc::cmp(X12, X13));
    let br_bot_tail = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));

    let bot_loop2 = buf.pos();
    buf.emit(enc::add(X14, X11, X27));
    buf.emit(ld1w_x2_imm(0, 9, X11, 0));
    buf.emit(ld1w_x2_imm(2, 9, X14, 0));
    buf.emit(mova_z_to_tile_h_s(2, 0, 0, 5, 0));
    buf.emit(mova_z_to_tile_h_s(3, 0, 0, 5, 1));
    buf.emit(mova_z_to_tile_h_s(2, 0, 1, 5, 2));
    buf.emit(mova_z_to_tile_h_s(3, 0, 1, 5, 3));
    buf.emit(enc::add(X11, X14, X27));
    buf.emit(enc::add_imm(X12, X12, 2, false));
    buf.emit(enc::cmp(X12, X13));
    buf.emit_b_cond_to(Cond::Lt, bot_loop2);

    let bot_tail = buf.pos();
    buf.patch_b_cond(br_bot_tail, Cond::Ge, bot_tail);
    buf.emit(enc::cmp(X12, X2));
    let br_bot_done = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));
    buf.emit(ld1w_x2_imm(0, 9, X11, 0));
    buf.emit(mova_z_to_tile_h_s(2, 0, 0, 5, 0));
    buf.emit(mova_z_to_tile_h_s(3, 0, 0, 5, 1));

    let done = buf.pos();
    buf.patch_cbz(br_no_bot, X2, done);
    buf.patch_b_cond(br_bot_done, Cond::Ge, done);
}

/// Stores `top_rows` (`X1`) rows from `za0h.s, za1h.s` and `bot_rows` (`X2`) rows from `za2h.s, za3h.s`
/// back to `C[i.., j..]` using 2-vector `st1w` (`pn9`), `X9 = c_ij`, `X27 = c_row_bytes`, `p5 = ptrue.s`.
fn emit_store_c_2x2_s(buf: &mut SmeBuf<'_>) {
    buf.emit(enc::mov(X11, X9)); // c_ptr = c_ij
    buf.emit(enc::movz(X12, 0, 0)); // r = 0
    buf.emit(enc::sub_imm(X13, X1, 1, false));
    buf.emit(enc::cmp(X12, X13));
    let br_top_tail = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));

    let top_loop2 = buf.pos();
    buf.emit(enc::add(X14, X11, X27));
    buf.emit(mova_tile_to_z_h_s(0, 5, 0, 0, 0));
    buf.emit(mova_tile_to_z_h_s(1, 5, 1, 0, 0));
    buf.emit(mova_tile_to_z_h_s(2, 5, 0, 0, 1));
    buf.emit(mova_tile_to_z_h_s(3, 5, 1, 0, 1));
    buf.emit(st1w_x2_imm(0, 9, X11, 0));
    buf.emit(st1w_x2_imm(2, 9, X14, 0));
    buf.emit(enc::add(X11, X14, X27));
    buf.emit(enc::add_imm(X12, X12, 2, false));
    buf.emit(enc::cmp(X12, X13));
    buf.emit_b_cond_to(Cond::Lt, top_loop2);

    let top_tail = buf.pos();
    buf.patch_b_cond(br_top_tail, Cond::Ge, top_tail);
    buf.emit(enc::cmp(X12, X1));
    let br_top_done = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));
    buf.emit(mova_tile_to_z_h_s(0, 5, 0, 0, 0));
    buf.emit(mova_tile_to_z_h_s(1, 5, 1, 0, 0));
    buf.emit(st1w_x2_imm(0, 9, X11, 0));

    let top_done = buf.pos();
    buf.patch_b_cond(br_top_done, Cond::Ge, top_done);

    let br_no_bot = buf.pos();
    buf.emit(enc::cbz(X2, 0));

    buf.emit(enc::movz(X12, 0, 0));
    buf.emit(enc::sub_imm(X13, X2, 1, false));
    buf.emit(enc::cmp(X12, X13));
    let br_bot_tail = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));

    let bot_loop2 = buf.pos();
    buf.emit(enc::add(X14, X11, X27));
    buf.emit(mova_tile_to_z_h_s(0, 5, 2, 0, 0));
    buf.emit(mova_tile_to_z_h_s(1, 5, 3, 0, 0));
    buf.emit(mova_tile_to_z_h_s(2, 5, 2, 0, 1));
    buf.emit(mova_tile_to_z_h_s(3, 5, 3, 0, 1));
    buf.emit(st1w_x2_imm(0, 9, X11, 0));
    buf.emit(st1w_x2_imm(2, 9, X14, 0));
    buf.emit(enc::add(X11, X14, X27));
    buf.emit(enc::add_imm(X12, X12, 2, false));
    buf.emit(enc::cmp(X12, X13));
    buf.emit_b_cond_to(Cond::Lt, bot_loop2);

    let bot_tail = buf.pos();
    buf.patch_b_cond(br_bot_tail, Cond::Ge, bot_tail);
    buf.emit(enc::cmp(X12, X2));
    let br_bot_done = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));
    buf.emit(mova_tile_to_z_h_s(0, 5, 2, 0, 0));
    buf.emit(mova_tile_to_z_h_s(1, 5, 3, 0, 0));
    buf.emit(st1w_x2_imm(0, 9, X11, 0));

    let done = buf.pos();
    buf.patch_cbz(br_no_bot, X2, done);
    buf.patch_b_cond(br_bot_done, Cond::Ge, done);
}

/// Emits the standalone SVL-agnostic 4-tile `2×2` AArch64 SME2 `f32` `FMOPA` GEMM micro-kernel
/// as instruction words (`za0.s..=za3.s` = `2*SVL_s × 2*SVL_s`, L1-packed `A`, 4×-unrolled 16-`FMOPA` inner loop).
///
/// Signature:
/// `extern "C" fn(c: *mut f32, a: *const f32, b: *const f32, m: i64, n: i64, k: i64, flags: i64)`
pub fn emit_sme2_gemm_f32_words<'a>(alloc: Alloc<'a>) -> AVec<'a, u32> {
    let mut buf = SmeBuf::new_in(320, alloc);

    emit_gemm_prologue(&mut buf);

    // Query SVL_s and precompute row strides & constant predicates.
    buf.emit(cntw(X26)); // svl_s
    buf.emit(enc::lsl_imm(X27, X23, 2)); // n_bytes = n * 4
    buf.emit(enc::lsl_imm(X28, X24, 2)); // k_bytes = k * 4
    buf.emit(ptrue_s(5)); // p5 = all-true 32-bit predicate
    buf.emit(ptrue_pn_s(8)); // pn8 = all-true 32-bit predicate-as-counter

    // Compute kb_max (<= 256 on 512b SVL -> <= 32 KiB L1d-resident A_pack) in X7,
    // clamp allocation to min(max(k, 0), kb_max), probe stack in 4 KiB chunks, align X17 to 128B.
    emit_compute_kb_max_groups(&mut buf, X7);
    buf.emit(enc::movz(X14, 0, 0));
    buf.emit(enc::cmp_imm(X24, 0));
    buf.emit(enc::csel(X15, X24, X14, Cond::Gt)); // k_pos = max(k, 0)
    buf.emit(enc::cmp(X15, X7));
    buf.emit(enc::csel(X15, X15, X7, Cond::Lt)); // kb_alloc = min(k_pos, kb_max)
    buf.emit(enc::mul(X13, X15, X26)); // kb_alloc * svl_s
    buf.emit(enc::lsl_imm(X13, X13, 3)); // kb_alloc * 2 * svl_bytes
    buf.emit(enc::add_imm(X13, X13, 128, false)); // + 128B alignment slack
    emit_stack_probe_align_128(&mut buf);

    // ------------------------------------------------------------------
    // Outer `i` loop: `i = 0, 2*svl_s, ... < m`
    // ------------------------------------------------------------------
    buf.emit(enc::movz(X0, 0, 0)); // i = 0
    let i_loop = buf.pos();

    buf.emit(whilelt_s(0, X0, X22)); // p0 = top rows [i, i + svl_s)
    buf.emit(enc::sub(X13, X22, X0)); // m - i
    buf.emit(enc::cmp(X13, X26));
    buf.emit(enc::csel(X1, X13, X26, Cond::Lt)); // top_rows = min(m - i, svl_s)

    buf.emit(enc::add(X14, X0, X26)); // i + svl_s
    buf.emit(whilelt_s(3, X14, X22)); // p3 = bot rows [i + svl_s, i + 2*svl_s)
    buf.emit(enc::sub(X13, X22, X14)); // m - (i + svl_s)
    buf.emit(enc::movz(X15, 0, 0));
    buf.emit(enc::cmp_imm(X13, 0));
    buf.emit(enc::csel(X13, X13, X15, Cond::Gt)); // max(m - i - svl_s, 0)
    buf.emit(enc::cmp(X13, X26));
    buf.emit(enc::csel(X2, X13, X26, Cond::Lt)); // bot_rows = clamp(m - i - svl_s, 0, svl_s)

    buf.emit(enc::mul(X13, X0, X27));
    buf.emit(enc::add(X3, X19, X13)); // c_i_row = &C[i, 0]
    buf.emit(enc::mul(X13, X0, X28));
    buf.emit(enc::add(X4, X20, X13)); // a_i_row = &A[i, 0]

    // ------------------------------------------------------------------
    // Middle `k_block` loop: `k_blk = 0, kb_max, ... < k` (runs once for k <= 256!)
    // ------------------------------------------------------------------
    buf.emit(enc::movz(X5, 0, 0)); // k_blk = 0
    let k_block_loop = buf.pos();

    buf.emit(enc::sub(X13, X24, X5)); // k - k_blk
    buf.emit(enc::movz(X14, 0, 0));
    buf.emit(enc::cmp_imm(X13, 0));
    buf.emit(enc::csel(X13, X13, X14, Cond::Gt)); // max(k - k_blk, 0)
    buf.emit(enc::cmp(X13, X7));
    buf.emit(enc::csel(X6, X13, X7, Cond::Lt)); // k_left = clamp(k - k_blk, 0, kb_max)

    // Phase 1: hardware-transpose `A[i..i+2*svl_s, k_blk..k_blk+k_left]` ONCE into `A_pack` (`X17`)
    // using `za0` (top) and `za1` (bot) before sweeping all `j_tiles`.
    buf.emit(enc::cmp_imm(X6, 0));
    let br_skip_pack_a = buf.pos();
    buf.emit(enc::b_cond(Cond::Le, 0));

    buf.emit(enc::movz(X8, 0, 0)); // ks = 0
    buf.emit(enc::mov(X9, X17)); // a_pack_dst = a_pack_base
    let pack_a_chunk_loop = buf.pos();

    buf.emit(whilelt_s(4, X8, X6)); // p4 = active k lanes in chunk
    buf.emit(enc::sub(X13, X6, X8)); // k_left - ks
    buf.emit(enc::cmp(X13, X26));
    buf.emit(enc::csel(X11, X13, X26, Cond::Lt)); // sub_k = min(k_left - ks, svl_s)
    buf.emit(zero_za_s_tiles(0b0011)); // clear za0.s, za1.s so partial m-tiles zero-pad cleanly

    // Load top `top_rows` (`X1`) rows into `za0h.s` (2x unrolled).
    buf.emit(enc::add(X10, X5, X8)); // k_blk + ks
    buf.emit(enc::add_lsl(X15, X4, X10, 2)); // a_ptr = &A[i, k_blk + ks]
    buf.emit(enc::movz(X12, 0, 0)); // r = 0
    buf.emit(enc::sub_imm(X13, X1, 1, false));
    buf.emit(enc::cmp(X12, X13));
    let br_pack_top_tail = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));

    let pack_top_loop2 = buf.pos();
    buf.emit(enc::add(X14, X15, X28));
    buf.emit(ld1w_imm(0, 4, X15, 0));
    buf.emit(ld1w_imm(1, 4, X14, 0));
    buf.emit(mova_z_to_tile_h_s(0, 0, 0, 4, 0));
    buf.emit(mova_z_to_tile_h_s(0, 0, 1, 4, 1));
    buf.emit(enc::add(X15, X14, X28));
    buf.emit(enc::add_imm(X12, X12, 2, false));
    buf.emit(enc::cmp(X12, X13));
    buf.emit_b_cond_to(Cond::Lt, pack_top_loop2);

    let pack_top_tail = buf.pos();
    buf.patch_b_cond(br_pack_top_tail, Cond::Ge, pack_top_tail);
    buf.emit(enc::cmp(X12, X1));
    let br_pack_top_done = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));
    buf.emit(ld1w_imm(0, 4, X15, 0));
    buf.emit(mova_z_to_tile_h_s(0, 0, 0, 4, 0));

    let pack_top_done = buf.pos();
    buf.patch_b_cond(br_pack_top_done, Cond::Ge, pack_top_done);

    // Load bottom `bot_rows` (`X2`) rows into `za1h.s` (skip when `bot_rows == 0`;
    // otherwise `top_rows == svl_s` (even), so `X15` is already `&A[i + svl_s, k_blk + ks]`).
    let br_pack_no_bot = buf.pos();
    buf.emit(enc::cbz(X2, 0));

    buf.emit(enc::movz(X12, 0, 0));
    buf.emit(enc::sub_imm(X13, X2, 1, false));
    buf.emit(enc::cmp(X12, X13));
    let br_pack_bot_tail = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));

    let pack_bot_loop2 = buf.pos();
    buf.emit(enc::add(X14, X15, X28));
    buf.emit(ld1w_imm(0, 4, X15, 0));
    buf.emit(ld1w_imm(1, 4, X14, 0));
    buf.emit(mova_z_to_tile_h_s(1, 0, 0, 4, 0));
    buf.emit(mova_z_to_tile_h_s(1, 0, 1, 4, 1));
    buf.emit(enc::add(X15, X14, X28));
    buf.emit(enc::add_imm(X12, X12, 2, false));
    buf.emit(enc::cmp(X12, X13));
    buf.emit_b_cond_to(Cond::Lt, pack_bot_loop2);

    let pack_bot_tail = buf.pos();
    buf.patch_b_cond(br_pack_bot_tail, Cond::Ge, pack_bot_tail);
    buf.emit(enc::cmp(X12, X2));
    let br_pack_bot_done = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));
    buf.emit(ld1w_imm(0, 4, X15, 0));
    buf.emit(mova_z_to_tile_h_s(1, 0, 0, 4, 0));

    let pack_bot_done = buf.pos();
    buf.patch_cbz(br_pack_no_bot, X2, pack_bot_done);
    buf.patch_b_cond(br_pack_bot_done, Cond::Ge, pack_bot_done);

    // Drain `sub_k` (`X11`) vertical columns from `za0v.s` & `za1v.s` into `A_pack` (`X9`).
    emit_drain_za01v_to_apack(&mut buf);

    buf.emit(incw(X8)); // ks += svl_s
    buf.emit(enc::cmp(X8, X6));
    buf.emit_b_cond_to(Cond::Lt, pack_a_chunk_loop);

    let pack_a_done = buf.pos();
    buf.patch_b_cond(br_skip_pack_a, Cond::Le, pack_a_done);

    // ------------------------------------------------------------------
    // Phase 2: `j` loop (`j = 0, 2*svl_s, ... < n`) across all 4 ZA tiles
    // ------------------------------------------------------------------
    buf.emit(enc::movz(X8, 0, 0)); // j = 0
    let j_loop = buf.pos();

    buf.emit(whilelt_s(1, X8, X23)); // p1 = left cols [j, j + svl_s)
    buf.emit(enc::add(X13, X8, X26));
    buf.emit(whilelt_s(2, X13, X23)); // p2 = right cols [j + svl_s, j + 2*svl_s)
    buf.emit(whilelt_pn_s_vlx2(9, X8, X23)); // pn9 = 2-vector counter for B & C

    buf.emit(enc::add_lsl(X9, X3, X8, 2)); // c_ij = &C[i, j]

    // Overwrite on `k_blk == 0 && (flags & 1) == 0`; otherwise accumulate into existing C.
    let br_load_c_kblk = buf.pos();
    buf.emit(enc::cbnz(X5, 0));
    let br_load_c_flag = buf.pos();
    buf.emit(enc::tbnz(X25, 0, 0));

    buf.emit(zero_za_all()); // 1-cycle zero of all 4 tiles za0..za3
    let br_init_c_done = buf.pos();
    buf.emit(enc::b(0));

    let load_c = buf.pos();
    buf.patch_cbnz(br_load_c_kblk, X5, load_c);
    buf.patch_tbnz(br_load_c_flag, X25, 0, load_c);
    emit_load_c_2x2_s(&mut buf);

    let init_c_done = buf.pos();
    buf.patch_b(br_init_c_done, init_c_done);

    // Inner `kk` loop: 4x-unrolled, ZERO `mova`, 2x 4-reg LD1W (`A_pack`) + 4x 2-reg LD1W (`B`)
    // feeding 16 `FMOPA.S` instructions (4,096 f32 FMAs = 8,192 FLOPs per loop branch!).
    buf.emit(enc::mul(X13, X5, X27)); // k_blk * n_bytes
    buf.emit(enc::add(X10, X21, X13));
    buf.emit(enc::add_lsl(X10, X10, X8, 2)); // b_ptr = &B[k_blk, j]
    buf.emit(enc::mov(X11, X17)); // a_pack_ptr = a_pack_base
    buf.emit(enc::movz(X12, 0, 0)); // kk = 0

    buf.emit(enc::sub_imm(X14, X6, 3, false)); // k_left - 3
    buf.emit(enc::cmp(X12, X14));
    let br_kk4_tail = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));

    let kk_loop4 = buf.pos();
    buf.emit(enc::add(X13, X10, X27)); // b_ptr + 1*n_bytes
    buf.emit(enc::add(X15, X13, X27)); // b_ptr + 2*n_bytes
    buf.emit(enc::add(X16, X15, X27)); // b_ptr + 3*n_bytes

    buf.emit(ld1w_x4_imm(0, 8, X11, 0)); // z0..z3 = A_pack[kk+0..kk+1] (256B)
    buf.emit(ld1w_x2_imm(8, 9, X10, 0)); // z8, z9 = B[kk+0] (128B)
    buf.emit(ld1w_x2_imm(10, 9, X13, 0)); // z10, z11 = B[kk+1] (128B)
    buf.emit(ld1w_x4_imm(4, 8, X11, 1)); // z4..z7 = A_pack[kk+2..kk+3] (256B)
    buf.emit(ld1w_x2_imm(12, 9, X15, 0)); // z12, z13 = B[kk+2] (128B)

    // Wave 0 (4 FMOPAs = 1,024 FMAs)
    buf.emit(fmopa_s(0, 0, 1, 0, 8));
    buf.emit(fmopa_s(1, 0, 2, 0, 9));
    buf.emit(fmopa_s(2, 3, 1, 1, 8));
    buf.emit(fmopa_s(3, 3, 2, 1, 9));

    buf.emit(ld1w_x2_imm(14, 9, X16, 0)); // z14, z15 = B[kk+3] (128B)

    // Wave 1 (4 FMOPAs = 1,024 FMAs)
    buf.emit(fmopa_s(0, 0, 1, 2, 10));
    buf.emit(fmopa_s(1, 0, 2, 2, 11));
    buf.emit(fmopa_s(2, 3, 1, 3, 10));
    buf.emit(fmopa_s(3, 3, 2, 3, 11));

    // Wave 2 (4 FMOPAs = 1,024 FMAs)
    buf.emit(fmopa_s(0, 0, 1, 4, 12));
    buf.emit(fmopa_s(1, 0, 2, 4, 13));
    buf.emit(fmopa_s(2, 3, 1, 5, 12));
    buf.emit(fmopa_s(3, 3, 2, 5, 13));

    // Wave 3 (4 FMOPAs = 1,024 FMAs)
    buf.emit(fmopa_s(0, 0, 1, 6, 14));
    buf.emit(fmopa_s(1, 0, 2, 6, 15));
    buf.emit(fmopa_s(2, 3, 1, 7, 14));
    buf.emit(fmopa_s(3, 3, 2, 7, 15));

    buf.emit(addvl(X11, X11, 8)); // a_pack_ptr += 4 * 2 * SVL_bytes
    buf.emit(enc::add(X10, X16, X27)); // b_ptr += 4 * n_bytes
    buf.emit(enc::add_imm(X12, X12, 4, false)); // kk += 4
    buf.emit(enc::cmp(X12, X14));
    buf.emit_b_cond_to(Cond::Lt, kk_loop4);

    let kk4_tail = buf.pos();
    buf.patch_b_cond(br_kk4_tail, Cond::Ge, kk4_tail);
    buf.emit(enc::cmp(X12, X6));
    let br_kk_done = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));

    let kk_loop1 = buf.pos();
    buf.emit(ld1w_x2_imm(0, 8, X11, 0));
    buf.emit(ld1w_x2_imm(8, 9, X10, 0));
    buf.emit(fmopa_s(0, 0, 1, 0, 8));
    buf.emit(fmopa_s(1, 0, 2, 0, 9));
    buf.emit(fmopa_s(2, 3, 1, 1, 8));
    buf.emit(fmopa_s(3, 3, 2, 1, 9));
    buf.emit(addvl(X11, X11, 2));
    buf.emit(enc::add(X10, X10, X27));
    buf.emit(enc::add_imm(X12, X12, 1, false));
    buf.emit(enc::cmp(X12, X6));
    buf.emit_b_cond_to(Cond::Lt, kk_loop1);

    let kk_done = buf.pos();
    buf.patch_b_cond(br_kk_done, Cond::Ge, kk_done);

    // Store `za0..za3` back to `C[i.., j..]`.
    emit_store_c_2x2_s(&mut buf);

    buf.emit(incw_mul(X8, 2)); // j += 2 * svl_s
    buf.emit(enc::cmp(X8, X23));
    buf.emit_b_cond_to(Cond::Lt, j_loop);

    buf.emit(enc::add(X5, X5, X7)); // k_blk += kb_max
    buf.emit(enc::cmp(X5, X24));
    buf.emit_b_cond_to(Cond::Lt, k_block_loop);

    buf.emit(incw_mul(X0, 2)); // i += 2 * svl_s
    buf.emit(enc::cmp(X0, X22));
    buf.emit_b_cond_to(Cond::Lt, i_loop);

    emit_gemm_epilogue(&mut buf, true);

    buf.into_words()
}

/// Emits the standalone SVL-agnostic AArch64 SME2 `f32` `FMOPA` GEMM micro-kernel as little-endian machine code bytes.
///
/// Signature:
/// `extern "C" fn(c: *mut f32, a: *const f32, b: *const f32, m: i64, n: i64, k: i64, flags: i64)`
pub fn emit_sme2_gemm_f32<'a>(alloc: Alloc<'a>) -> AVec<'a, u8> {
    let words = emit_sme2_gemm_f32_words(alloc);
    words_to_bytes(&words, alloc)
}

/// Emits the standalone SVL-agnostic AArch64 SME2 `f64` `FMOPA` GEMM micro-kernel as instruction words
/// (requires `FEAT_SME_F64F64`; 4-tile `za1.d..=za4.d` with `za0.d` transpose scratchpad).
///
/// Signature:
/// `extern "C" fn(c: *mut f64, a: *const f64, b: *const f64, m: i64, n: i64, k: i64, flags: i64)`
pub fn emit_sme2_gemm_f64_words<'a>(alloc: Alloc<'a>) -> AVec<'a, u32> {
    let mut buf = SmeBuf::new_in(192, alloc);
    const TILES: u32 = 4;
    const K_PRED: u32 = 5;

    emit_gemm_prologue(&mut buf);

    // Query SVL_d and precompute row byte strides.
    buf.emit(cntd(X26)); // svl_d
    buf.emit(enc::lsl_imm(X27, X23, 3)); // n_bytes = n * 8
    buf.emit(enc::lsl_imm(X28, X24, 3)); // k_bytes = k * 8

    // Outer `i` loop: `i = 0, svl_d, 2*svl_d, ... < m`
    buf.emit(enc::movz(X0, 0, 0)); // i = 0
    let i_loop = buf.pos();

    buf.emit(whilelt_d(0, X0, X22)); // p0 = rows [i, i + svl_d)
    buf.emit(enc::sub(X13, X22, X0)); // m - i
    buf.emit(enc::cmp(X13, X26));
    buf.emit(enc::csel(X1, X13, X26, Cond::Lt)); // rows_left = min(m - i, svl_d)
    buf.emit(enc::mul(X13, X0, X28)); // i * k_bytes
    buf.emit(enc::add(X2, X20, X13)); // a_i_row = &A[i, 0]
    buf.emit(enc::mul(X13, X0, X27)); // i * n_bytes
    buf.emit(enc::add(X3, X19, X13)); // c_i_row = &C[i, 0]

    // Middle `j` loop: `j = 0, 4*svl_d, ... < n`
    buf.emit(enc::movz(X4, 0, 0)); // j = 0
    let j_loop = buf.pos();

    buf.emit(whilelt_d(1, X4, X23));
    buf.emit(enc::add(X13, X4, X26));
    buf.emit(whilelt_d(2, X13, X23));
    buf.emit(enc::add(X13, X13, X26));
    buf.emit(whilelt_d(3, X13, X23));
    buf.emit(enc::add(X13, X13, X26));
    buf.emit(whilelt_d(4, X13, X23));

    buf.emit(enc::add_lsl(X5, X3, X4, 3)); // c_ij = &C[i, j]
    buf.emit(enc::add_lsl(X6, X21, X4, 3)); // b_0j = &B[0, j]

    // Initialize accumulator tiles: overwrite -> `zero {za1.d..za4.d}`, accum -> load C.
    let br_zero_c = buf.pos();
    buf.emit(enc::tbz(X25, 0, 0));

    buf.emit(enc::mov(X11, X5));
    buf.emit(enc::movz(X12, 0, 0));
    let load_c_loop = buf.pos();
    for t in 1..=TILES {
        let vl_off = t.saturating_sub(1).cast_signed();
        buf.emit(ld1d_imm(t, t, X11, vl_off));
    }
    for t in 1..=TILES {
        buf.emit(mova_z_to_tile_h_d(t, 0, 0, t, t));
    }
    buf.emit(enc::add(X11, X11, X27));
    buf.emit(enc::add_imm(X12, X12, 1, false));
    buf.emit(enc::cmp(X12, X1));
    buf.emit_b_cond_to(Cond::Lo, load_c_loop);

    let br_init_done = buf.pos();
    buf.emit(enc::b(0));

    let init_zero = buf.pos();
    buf.patch_tbz(br_zero_c, X25, 0, init_zero);
    buf.emit(zero_za_d_tiles(0b0001_1110));

    let init_done = buf.pos();
    buf.patch_b(br_init_done, init_done);

    // `k_block` loop: `k_idx = 0, svl_d, 2*svl_d, ... < k`
    buf.emit(enc::movz(X7, 0, 0));
    buf.emit(enc::cmp(X7, X24));
    let br_skip_k = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));

    let k_loop = buf.pos();
    buf.emit(whilelt_d(K_PRED, X7, X24));
    buf.emit(enc::sub(X13, X24, X7));
    buf.emit(enc::cmp(X13, X26));
    buf.emit(enc::csel(X8, X13, X26, Cond::Lt));

    // Phase 1: load `rows_left` rows of A horizontally into `za0.d` (2x unrolled).
    buf.emit(enc::add_lsl(X9, X2, X7, 3));
    buf.emit(enc::movz(X12, 0, 0));
    buf.emit(enc::sub_imm(X13, X1, 1, false));
    buf.emit(enc::cmp(X12, X13));
    let br_a_tail = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));
    let load_a_loop2 = buf.pos();
    buf.emit(enc::add(X14, X9, X28));
    buf.emit(ld1d_imm(0, K_PRED, X9, 0));
    buf.emit(ld1d_imm(1, K_PRED, X14, 0));
    buf.emit(mova_z_to_tile_h_d(0, 0, 0, K_PRED, 0));
    buf.emit(mova_z_to_tile_h_d(0, 0, 1, K_PRED, 1));
    buf.emit(enc::add(X9, X14, X28));
    buf.emit(enc::add_imm(X12, X12, 2, false));
    buf.emit(enc::cmp(X12, X13));
    buf.emit_b_cond_to(Cond::Lt, load_a_loop2);
    let a_tail = buf.pos();
    buf.patch_b_cond(br_a_tail, Cond::Ge, a_tail);
    buf.emit(enc::cmp(X12, X1));
    let br_a_done = buf.pos();
    buf.emit(enc::b_cond(Cond::Hs, 0));
    buf.emit(ld1d_imm(0, K_PRED, X9, 0));
    buf.emit(mova_z_to_tile_h_d(0, 0, 0, K_PRED, 0));
    let a_done = buf.pos();
    buf.patch_b_cond(br_a_done, Cond::Hs, a_done);

    // Phase 2: 2x-unrolled (8 FMOPA.D/iter) vertical `za0v.d` + `B` rows into `za1.d..=za4.d`.
    buf.emit(enc::mul(X13, X7, X27));
    buf.emit(enc::add(X10, X6, X13));
    buf.emit(enc::movz(X12, 0, 0));
    buf.emit(enc::sub_imm(X14, X8, 1, false));
    buf.emit(enc::cmp(X12, X14));
    let br_kk_tail = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));
    let kk_loop2 = buf.pos();
    buf.emit(enc::add(X13, X10, X27));
    buf.emit(mova_tile_to_z_v_d(0, 0, 0, 0, 0));
    for t in 1..=TILES {
        let vl_off = t.saturating_sub(1).cast_signed();
        buf.emit(ld1d_imm(t, t, X10, vl_off));
    }
    buf.emit(mova_tile_to_z_v_d(8, 0, 0, 0, 1));
    for t in 1..=TILES {
        let vl_off = t.saturating_sub(1).cast_signed();
        buf.emit(ld1d_imm(8 + t, t, X13, vl_off));
    }
    for t in 1..=TILES {
        buf.emit(fmopa_d(t, 0, t, 0, t));
    }
    for t in 1..=TILES {
        buf.emit(fmopa_d(t, 0, t, 8, 8 + t));
    }
    buf.emit(enc::add(X10, X13, X27));
    buf.emit(enc::add_imm(X12, X12, 2, false));
    buf.emit(enc::cmp(X12, X14));
    buf.emit_b_cond_to(Cond::Lt, kk_loop2);
    let kk_tail = buf.pos();
    buf.patch_b_cond(br_kk_tail, Cond::Ge, kk_tail);
    buf.emit(enc::cmp(X12, X8));
    let br_kk_done = buf.pos();
    buf.emit(enc::b_cond(Cond::Hs, 0));
    buf.emit(mova_tile_to_z_v_d(0, 0, 0, 0, 0));
    for t in 1..=TILES {
        let vl_off = t.saturating_sub(1).cast_signed();
        buf.emit(ld1d_imm(t, t, X10, vl_off));
    }
    for t in 1..=TILES {
        buf.emit(fmopa_d(t, 0, t, 0, t));
    }
    let kk_done = buf.pos();
    buf.patch_b_cond(br_kk_done, Cond::Hs, kk_done);

    buf.emit(incd(X7));
    buf.emit(enc::cmp(X7, X24));
    buf.emit_b_cond_to(Cond::Lt, k_loop);

    // Store `rows_left` rows of `za1.d..=za4.d` back to `C[i.., j..]`.
    let store_c = buf.pos();
    buf.patch_b_cond(br_skip_k, Cond::Ge, store_c);

    buf.emit(enc::mov(X11, X5));
    buf.emit(enc::movz(X12, 0, 0));
    let store_c_loop = buf.pos();
    for t in 1..=TILES {
        buf.emit(mova_tile_to_z_h_d(t, t, t, 0, 0));
    }
    for t in 1..=TILES {
        let vl_off = t.saturating_sub(1).cast_signed();
        buf.emit(st1d_imm(t, t, X11, vl_off));
    }
    buf.emit(enc::add(X11, X11, X27));
    buf.emit(enc::add_imm(X12, X12, 1, false));
    buf.emit(enc::cmp(X12, X1));
    buf.emit_b_cond_to(Cond::Lo, store_c_loop);

    buf.emit(incd_mul(X4, TILES));
    buf.emit(enc::cmp(X4, X23));
    buf.emit_b_cond_to(Cond::Lt, j_loop);

    buf.emit(incd(X0));
    buf.emit(enc::cmp(X0, X22));
    buf.emit_b_cond_to(Cond::Lt, i_loop);

    emit_gemm_epilogue(&mut buf, false);

    buf.into_words()
}

/// Emits the standalone SVL-agnostic AArch64 SME2 `f64` `FMOPA` GEMM micro-kernel as little-endian machine code bytes
/// (requires `FEAT_SME_F64F64`).
///
/// Signature:
/// `extern "C" fn(c: *mut f64, a: *const f64, b: *const f64, m: i64, n: i64, k: i64, flags: i64)`
pub fn emit_sme2_gemm_f64<'a>(alloc: Alloc<'a>) -> AVec<'a, u8> {
    let words = emit_sme2_gemm_f64_words(alloc);
    words_to_bytes(&words, alloc)
}

#[inline]
const fn int8_mopa_s(signed: bool, zada: u32, pn: u32, pm: u32, zn: u32, zm: u32) -> u32 {
    if signed {
        smopa_s(zada, pn, pm, zn, zm)
    } else {
        umopa_s(zada, pn, pm, zn, zm)
    }
}

/// Emits the standalone SVL-agnostic 4-tile `2×2` AArch64 SME2 INT8 (`SMOPA`/`UMOPA`) GEMM
/// micro-kernel as instruction words (`za0.s..=za3.s` = `2*SVL_s × 2*SVL_s` `i32`/`u32` accumulators).
fn emit_sme2_gemm_int8_words<'a>(signed: bool, alloc: Alloc<'a>) -> AVec<'a, u32> {
    let mut buf = SmeBuf::new_in(384, alloc);

    emit_gemm_prologue(&mut buf);

    // Query SVL_s and precompute strides & constant predicates.
    // Note: X23 = n (also B row byte stride), X24 = k (also A row byte stride).
    buf.emit(cntw(X26)); // svl_s
    buf.emit(enc::lsl_imm(X27, X23, 2)); // c_row_bytes = n * 4 (also n*4 for whilelt.b)
    buf.emit(enc::lsl_imm(X28, X22, 2)); // m_bytes = m * 4 (for whilelt.b)
    buf.emit(ptrue_s(5)); // p5 = all-true 32-bit predicate
    buf.emit(ptrue_pn_s(8)); // pn8 = all-true 32-bit predicate-as-counter

    // Compute kb_max_bytes = kb_max_groups * 4 (<= 1024 B on 512b SVL -> <= 32 KiB A_pack) in X7.
    emit_compute_kb_max_groups(&mut buf, X7);
    buf.emit(enc::lsl_imm(X7, X7, 2)); // kb_max_bytes = kb_max_groups * 4

    // Allocate A_pack on stack: ceil(min(max(k, 0), kb_max_bytes) / 4) * 2 * svl_bytes + 128.
    buf.emit(enc::movz(X14, 0, 0));
    buf.emit(enc::cmp_imm(X24, 0));
    buf.emit(enc::csel(X15, X24, X14, Cond::Gt)); // k_pos = max(k, 0)
    buf.emit(enc::cmp(X15, X7));
    buf.emit(enc::csel(X15, X15, X7, Cond::Lt)); // kb_alloc_bytes
    buf.emit(enc::add_imm(X15, X15, 3, false));
    buf.emit(enc::lsr_imm(X15, X15, 2)); // kb_alloc_groups = ceil(kb_alloc_bytes / 4)
    buf.emit(enc::mul(X13, X15, X26)); // kb_alloc_groups * svl_s
    buf.emit(enc::lsl_imm(X13, X13, 3)); // kb_alloc_groups * 2 * svl_bytes
    buf.emit(enc::add_imm(X13, X13, 128, false)); // + 128B alignment slack
    emit_stack_probe_align_128(&mut buf);

    // ------------------------------------------------------------------
    // Outer `i` loop: `i = 0, 2*svl_s, ... < m`
    // ------------------------------------------------------------------
    buf.emit(enc::movz(X0, 0, 0)); // i = 0
    let i_loop = buf.pos();

    // Byte predicates for 4-way SMOPA/UMOPA (4 predicate bits per active row!).
    buf.emit(enc::lsl_imm(X13, X0, 2)); // i * 4
    buf.emit(whilelt_b(0, X13, X28)); // p0 = top rows [i, i + svl_s)
    buf.emit(enc::sub(X13, X22, X0));
    buf.emit(enc::cmp(X13, X26));
    buf.emit(enc::csel(X1, X13, X26, Cond::Lt)); // top_rows = min(m - i, svl_s)

    buf.emit(enc::add(X14, X0, X26)); // i + svl_s
    buf.emit(enc::lsl_imm(X15, X14, 2)); // (i + svl_s) * 4
    buf.emit(whilelt_b(3, X15, X28)); // p3 = bot rows [i + svl_s, i + 2*svl_s)
    buf.emit(enc::sub(X13, X22, X14));
    buf.emit(enc::movz(X15, 0, 0));
    buf.emit(enc::cmp_imm(X13, 0));
    buf.emit(enc::csel(X13, X13, X15, Cond::Gt));
    buf.emit(enc::cmp(X13, X26));
    buf.emit(enc::csel(X2, X13, X26, Cond::Lt)); // bot_rows = clamp(m - i - svl_s, 0, svl_s)

    buf.emit(enc::mul(X13, X0, X27));
    buf.emit(enc::add(X3, X19, X13)); // c_i_row = &C[i, 0]
    buf.emit(enc::mul(X13, X0, X24));
    buf.emit(enc::add(X4, X20, X13)); // a_i_row = &A[i, 0]

    // ------------------------------------------------------------------
    // Middle `k_block` loop: `k_blk = 0, kb_max_bytes, ... < k`
    // ------------------------------------------------------------------
    buf.emit(enc::movz(X5, 0, 0)); // k_blk = 0
    let k_block_loop = buf.pos();

    buf.emit(enc::sub(X13, X24, X5)); // k - k_blk
    buf.emit(enc::movz(X14, 0, 0));
    buf.emit(enc::cmp_imm(X13, 0));
    buf.emit(enc::csel(X13, X13, X14, Cond::Gt));
    buf.emit(enc::cmp(X13, X7));
    buf.emit(enc::csel(X6, X13, X7, Cond::Lt)); // k_left_bytes

    // Phase 1: hardware-transpose `A[i..i+2*svl_s, k_blk..k_blk+k_left_bytes]` into `A_pack` (`X17`)
    // at 32-bit (4-byte group) granularity via `za0`/`za1` so `za0v.s[q]` is the exact `zn.b` operand!
    buf.emit(enc::cmp_imm(X6, 0));
    let br_skip_pack_a = buf.pos();
    buf.emit(enc::b_cond(Cond::Le, 0));

    buf.emit(enc::movz(X8, 0, 0)); // ks = 0 (bytes)
    buf.emit(enc::mov(X9, X17)); // a_pack_dst = a_pack_base
    let pack_a_chunk_loop = buf.pos();

    buf.emit(whilelt_b(4, X8, X6)); // p4 = active k bytes in chunk (zero-pads tail k % 4!)
    buf.emit(enc::sub(X13, X6, X8)); // k_left_bytes - ks
    buf.emit(enc::lsl_imm(X14, X26, 2)); // svl_b = 4 * svl_s
    buf.emit(enc::cmp(X13, X14));
    buf.emit(enc::csel(X11, X13, X14, Cond::Lt)); // sub_k_bytes
    buf.emit(enc::add_imm(X11, X11, 3, false));
    buf.emit(enc::lsr_imm(X11, X11, 2)); // sub_k_groups = ceil(sub_k_bytes / 4) in 1..=svl_s
    buf.emit(zero_za_s_tiles(0b0011));

    // Load top `top_rows` rows of A bytes into `za0h.s` (2x unrolled; `ld1b` zeroes inactive bytes).
    buf.emit(enc::add(X10, X5, X8)); // k_blk + ks
    buf.emit(enc::add(X15, X4, X10)); // a_ptr = &A[i, k_blk + ks]
    buf.emit(enc::movz(X12, 0, 0));
    buf.emit(enc::sub_imm(X13, X1, 1, false));
    buf.emit(enc::cmp(X12, X13));
    let br_pack_top_tail = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));

    let pack_top_loop2 = buf.pos();
    buf.emit(enc::add(X14, X15, X24));
    buf.emit(ld1b_imm(0, 4, X15, 0));
    buf.emit(ld1b_imm(1, 4, X14, 0));
    buf.emit(mova_z_to_tile_h_s(0, 0, 0, 5, 0));
    buf.emit(mova_z_to_tile_h_s(0, 0, 1, 5, 1));
    buf.emit(enc::add(X15, X14, X24));
    buf.emit(enc::add_imm(X12, X12, 2, false));
    buf.emit(enc::cmp(X12, X13));
    buf.emit_b_cond_to(Cond::Lt, pack_top_loop2);

    let pack_top_tail = buf.pos();
    buf.patch_b_cond(br_pack_top_tail, Cond::Ge, pack_top_tail);
    buf.emit(enc::cmp(X12, X1));
    let br_pack_top_done = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));
    buf.emit(ld1b_imm(0, 4, X15, 0));
    buf.emit(mova_z_to_tile_h_s(0, 0, 0, 5, 0));

    let pack_top_done = buf.pos();
    buf.patch_b_cond(br_pack_top_done, Cond::Ge, pack_top_done);

    // Load bottom `bot_rows` rows of A bytes into `za1h.s`.
    let br_pack_no_bot = buf.pos();
    buf.emit(enc::cbz(X2, 0));

    buf.emit(enc::movz(X12, 0, 0));
    buf.emit(enc::sub_imm(X13, X2, 1, false));
    buf.emit(enc::cmp(X12, X13));
    let br_pack_bot_tail = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));

    let pack_bot_loop2 = buf.pos();
    buf.emit(enc::add(X14, X15, X24));
    buf.emit(ld1b_imm(0, 4, X15, 0));
    buf.emit(ld1b_imm(1, 4, X14, 0));
    buf.emit(mova_z_to_tile_h_s(1, 0, 0, 5, 0));
    buf.emit(mova_z_to_tile_h_s(1, 0, 1, 5, 1));
    buf.emit(enc::add(X15, X14, X24));
    buf.emit(enc::add_imm(X12, X12, 2, false));
    buf.emit(enc::cmp(X12, X13));
    buf.emit_b_cond_to(Cond::Lt, pack_bot_loop2);

    let pack_bot_tail = buf.pos();
    buf.patch_b_cond(br_pack_bot_tail, Cond::Ge, pack_bot_tail);
    buf.emit(enc::cmp(X12, X2));
    let br_pack_bot_done = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));
    buf.emit(ld1b_imm(0, 4, X15, 0));
    buf.emit(mova_z_to_tile_h_s(1, 0, 0, 5, 0));

    let pack_bot_done = buf.pos();
    buf.patch_cbz(br_pack_no_bot, X2, pack_bot_done);
    buf.patch_b_cond(br_pack_bot_done, Cond::Ge, pack_bot_done);

    // Drain `sub_k_groups` (`X11`) vertical 32-bit columns into `A_pack` (`X9`).
    emit_drain_za01v_to_apack(&mut buf);

    buf.emit(incb(X8)); // ks += svl_b
    buf.emit(enc::cmp(X8, X6));
    buf.emit_b_cond_to(Cond::Lt, pack_a_chunk_loop);

    let pack_a_done = buf.pos();
    buf.patch_b_cond(br_skip_pack_a, Cond::Le, pack_a_done);

    // ------------------------------------------------------------------
    // Phase 2: `j` loop (`j = 0, 2*svl_s, ... < n`) across all 4 ZA tiles
    // ------------------------------------------------------------------
    buf.emit(enc::movz(X8, 0, 0)); // j = 0
    let j_loop = buf.pos();

    // Byte predicates for 4-way SMOPA/UMOPA columns + `p6` for `ld1b` of B + `pn9` for 32-bit C.
    buf.emit(enc::lsl_imm(X13, X8, 2)); // j * 4
    buf.emit(whilelt_b(1, X13, X27)); // p1 = left cols [j, j + svl_s)
    buf.emit(enc::add(X14, X8, X26)); // j + svl_s
    buf.emit(enc::lsl_imm(X15, X14, 2)); // (j + svl_s) * 4
    buf.emit(whilelt_b(2, X15, X27)); // p2 = right cols [j + svl_s, j + 2*svl_s)
    buf.emit(whilelt_b(6, X8, X23)); // p6 = active B bytes [j, min(n, j + 4*svl_s))
    buf.emit(whilelt_pn_s_vlx2(9, X8, X23)); // pn9 = 2-vector counter for 32-bit C

    buf.emit(enc::add_lsl(X9, X3, X8, 2)); // c_ij = &C[i, j]

    // Overwrite on `k_blk == 0 && (flags & 1) == 0`; otherwise accumulate into existing C.
    let br_load_c_kblk = buf.pos();
    buf.emit(enc::cbnz(X5, 0));
    let br_load_c_flag = buf.pos();
    buf.emit(enc::tbnz(X25, 0, 0));

    buf.emit(zero_za_all());
    let br_init_c_done = buf.pos();
    buf.emit(enc::b(0));

    let load_c = buf.pos();
    buf.patch_cbnz(br_load_c_kblk, X5, load_c);
    buf.patch_tbnz(br_load_c_flag, X25, 0, load_c);
    emit_load_c_2x2_s(&mut buf);

    let init_c_done = buf.pos();
    buf.patch_b(br_init_c_done, init_c_done);

    // Inner `kk` loop over `k_left_bytes`:
    // 2x-unrolled (8 k-rows = 8 SMOPA/UMOPAs = 8,192 INT8 MACs = 16,384 OPS per branch!)
    // + 1-group (4 k-rows) + arbitrary 1..3-row tail.
    buf.emit(enc::mul(X13, X5, X23)); // k_blk * n
    buf.emit(enc::add(X10, X21, X13));
    buf.emit(enc::add(X10, X10, X8)); // b_ptr = &B[k_blk, j]
    buf.emit(enc::mov(X11, X17)); // a_pack_ptr = a_pack_base
    buf.emit(enc::movz(X12, 0, 0)); // kk_bytes = 0

    // 2-group (8 k-row) unrolled hot loop
    buf.emit(enc::sub_imm(X14, X6, 7, false)); // k_left_bytes - 7
    buf.emit(enc::cmp(X12, X14));
    let br_kk8_tail = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));

    let kk_loop8 = buf.pos();
    buf.emit(ld1w_x4_imm(0, 8, X11, 0)); // z0..z3 = A_pack[g0_top, g0_bot, g1_top, g1_bot]

    // Group 0: load 4 rows of B and interleave via zip1.b + zip1.h/zip2.h
    buf.emit(enc::add(X13, X10, X23));
    buf.emit(enc::add(X15, X13, X23));
    buf.emit(enc::add(X16, X15, X23));
    buf.emit(ld1b_imm(4, 6, X10, 0));
    buf.emit(ld1b_imm(5, 6, X13, 0));
    buf.emit(ld1b_imm(6, 6, X15, 0));
    buf.emit(ld1b_imm(7, 6, X16, 0));
    buf.emit(enc::add(X10, X16, X23)); // b_ptr += 4*n

    buf.emit(zip1_b(8, 4, 5));
    buf.emit(zip1_b(9, 6, 7));
    buf.emit(zip1_h(10, 8, 9)); // b0_g0 (cols 0..svl_s, 4 k-rows interleaved)
    buf.emit(zip2_h(11, 8, 9)); // b1_g0 (cols svl_s..2*svl_s, 4 k-rows interleaved)

    // Group 1: overlap next 4 B loads with Group 0's 4 SMOPA/UMOPAs!
    buf.emit(enc::add(X13, X10, X23));
    buf.emit(enc::add(X15, X13, X23));
    buf.emit(enc::add(X16, X15, X23));
    buf.emit(ld1b_imm(12, 6, X10, 0));
    buf.emit(ld1b_imm(13, 6, X13, 0));
    buf.emit(ld1b_imm(14, 6, X15, 0));
    buf.emit(ld1b_imm(15, 6, X16, 0));
    buf.emit(enc::add(X10, X16, X23)); // b_ptr += 4*n

    // Group 0: 4 SMOPA/UMOPAs (4,096 INT8 MACs)
    buf.emit(int8_mopa_s(signed, 0, 0, 1, 0, 10));
    buf.emit(int8_mopa_s(signed, 1, 0, 2, 0, 11));
    buf.emit(int8_mopa_s(signed, 2, 3, 1, 1, 10));
    buf.emit(int8_mopa_s(signed, 3, 3, 2, 1, 11));

    buf.emit(zip1_b(8, 12, 13));
    buf.emit(zip1_b(9, 14, 15));
    buf.emit(zip1_h(10, 8, 9)); // b0_g1
    buf.emit(zip2_h(11, 8, 9)); // b1_g1

    // Group 1: 4 SMOPA/UMOPAs (4,096 INT8 MACs)
    buf.emit(int8_mopa_s(signed, 0, 0, 1, 2, 10));
    buf.emit(int8_mopa_s(signed, 1, 0, 2, 2, 11));
    buf.emit(int8_mopa_s(signed, 2, 3, 1, 3, 10));
    buf.emit(int8_mopa_s(signed, 3, 3, 2, 3, 11));

    buf.emit(addvl(X11, X11, 4)); // a_pack_ptr += 2 groups * 2 * SVL_bytes
    buf.emit(enc::add_imm(X12, X12, 8, false)); // kk_bytes += 8
    buf.emit(enc::cmp(X12, X14));
    buf.emit_b_cond_to(Cond::Lt, kk_loop8);

    // Single full-group (4 k-row) tail
    let kk8_tail = buf.pos();
    buf.patch_b_cond(br_kk8_tail, Cond::Ge, kk8_tail);
    buf.emit(enc::sub_imm(X14, X6, 3, false)); // k_left_bytes - 3
    buf.emit(enc::cmp(X12, X14));
    let br_kk4_tail = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));

    let kk_loop4 = buf.pos();
    buf.emit(ld1w_x2_imm(0, 8, X11, 0));
    buf.emit(enc::add(X13, X10, X23));
    buf.emit(enc::add(X15, X13, X23));
    buf.emit(enc::add(X16, X15, X23));
    buf.emit(ld1b_imm(4, 6, X10, 0));
    buf.emit(ld1b_imm(5, 6, X13, 0));
    buf.emit(ld1b_imm(6, 6, X15, 0));
    buf.emit(ld1b_imm(7, 6, X16, 0));
    buf.emit(enc::add(X10, X16, X23));
    buf.emit(zip1_b(8, 4, 5));
    buf.emit(zip1_b(9, 6, 7));
    buf.emit(zip1_h(10, 8, 9));
    buf.emit(zip2_h(11, 8, 9));
    buf.emit(int8_mopa_s(signed, 0, 0, 1, 0, 10));
    buf.emit(int8_mopa_s(signed, 1, 0, 2, 0, 11));
    buf.emit(int8_mopa_s(signed, 2, 3, 1, 1, 10));
    buf.emit(int8_mopa_s(signed, 3, 3, 2, 1, 11));
    buf.emit(addvl(X11, X11, 2));
    buf.emit(enc::add_imm(X12, X12, 4, false));
    buf.emit(enc::cmp(X12, X14));
    buf.emit_b_cond_to(Cond::Lt, kk_loop4);

    // Partial tail (`1..=3` remaining k-rows when `k % 4 != 0`)
    let kk4_tail = buf.pos();
    buf.patch_b_cond(br_kk4_tail, Cond::Ge, kk4_tail);
    buf.emit(enc::sub(X14, X6, X12)); // k_rem = k_left_bytes - kk_bytes
    buf.emit(enc::cmp_imm(X14, 0));
    let br_kk_done = buf.pos();
    buf.emit(enc::b_cond(Cond::Le, 0));

    buf.emit(dup_zero_b(4));
    buf.emit(dup_zero_b(5));
    buf.emit(dup_zero_b(6));
    buf.emit(dup_zero_b(7));
    buf.emit(ld1b_imm(4, 6, X10, 0)); // row 0 (k_rem >= 1)
    buf.emit(enc::cmp_imm(X14, 2));
    let br_tail_lt2 = buf.pos();
    buf.emit(enc::b_cond(Cond::Lt, 0));
    buf.emit(enc::add(X10, X10, X23));
    buf.emit(ld1b_imm(5, 6, X10, 0)); // row 1 (k_rem >= 2)
    buf.emit(enc::cmp_imm(X14, 3));
    let br_tail_lt3 = buf.pos();
    buf.emit(enc::b_cond(Cond::Lt, 0));
    buf.emit(enc::add(X10, X10, X23));
    buf.emit(ld1b_imm(6, 6, X10, 0)); // row 2 (k_rem >= 3)

    let tail_zip = buf.pos();
    buf.patch_b_cond(br_tail_lt2, Cond::Lt, tail_zip);
    buf.patch_b_cond(br_tail_lt3, Cond::Lt, tail_zip);

    buf.emit(ld1w_x2_imm(0, 8, X11, 0));
    buf.emit(zip1_b(8, 4, 5));
    buf.emit(zip1_b(9, 6, 7));
    buf.emit(zip1_h(10, 8, 9));
    buf.emit(zip2_h(11, 8, 9));
    buf.emit(int8_mopa_s(signed, 0, 0, 1, 0, 10));
    buf.emit(int8_mopa_s(signed, 1, 0, 2, 0, 11));
    buf.emit(int8_mopa_s(signed, 2, 3, 1, 1, 10));
    buf.emit(int8_mopa_s(signed, 3, 3, 2, 1, 11));

    let kk_done = buf.pos();
    buf.patch_b_cond(br_kk_done, Cond::Le, kk_done);

    // Store `za0..za3` back to `C[i.., j..]`.
    emit_store_c_2x2_s(&mut buf);

    buf.emit(incw_mul(X8, 2)); // j += 2 * svl_s
    buf.emit(enc::cmp(X8, X23));
    buf.emit_b_cond_to(Cond::Lt, j_loop);

    buf.emit(enc::add(X5, X5, X7)); // k_blk += kb_max_bytes
    buf.emit(enc::cmp(X5, X24));
    buf.emit_b_cond_to(Cond::Lt, k_block_loop);

    buf.emit(incw_mul(X0, 2)); // i += 2 * svl_s
    buf.emit(enc::cmp(X0, X22));
    buf.emit_b_cond_to(Cond::Lt, i_loop);

    emit_gemm_epilogue(&mut buf, true);

    buf.into_words()
}

/// Emits the standalone SVL-agnostic 4-tile `2×2` AArch64 SME2 signed INT8 (`SMOPA`) GEMM
/// micro-kernel as instruction words (`i8 × i8 -> i32`).
///
/// Signature:
/// `extern "C" fn(c: *mut i32, a: *const i8, b: *const i8, m: i64, n: i64, k: i64, flags: i64)`
pub fn emit_sme2_gemm_i8_i32_words<'a>(alloc: Alloc<'a>) -> AVec<'a, u32> {
    emit_sme2_gemm_int8_words(true, alloc)
}

/// Emits the standalone SVL-agnostic 4-tile `2×2` AArch64 SME2 signed INT8 (`SMOPA`) GEMM
/// micro-kernel as little-endian machine code bytes (`i8 × i8 -> i32`).
///
/// Signature:
/// `extern "C" fn(c: *mut i32, a: *const i8, b: *const i8, m: i64, n: i64, k: i64, flags: i64)`
pub fn emit_sme2_gemm_i8_i32<'a>(alloc: Alloc<'a>) -> AVec<'a, u8> {
    let words = emit_sme2_gemm_i8_i32_words(alloc);
    words_to_bytes(&words, alloc)
}

/// Emits the standalone SVL-agnostic 4-tile `2×2` AArch64 SME2 unsigned INT8 (`UMOPA`) GEMM
/// micro-kernel as instruction words (`u8 × u8 -> u32`).
///
/// Signature:
/// `extern "C" fn(c: *mut u32, a: *const u8, b: *const u8, m: i64, n: i64, k: i64, flags: i64)`
pub fn emit_sme2_gemm_u8_u32_words<'a>(alloc: Alloc<'a>) -> AVec<'a, u32> {
    emit_sme2_gemm_int8_words(false, alloc)
}

/// Emits the standalone SVL-agnostic 4-tile `2×2` AArch64 SME2 unsigned INT8 (`UMOPA`) GEMM
/// micro-kernel as little-endian machine code bytes (`u8 × u8 -> u32`).
///
/// Signature:
/// `extern "C" fn(c: *mut u32, a: *const u8, b: *const u8, m: i64, n: i64, k: i64, flags: i64)`
pub fn emit_sme2_gemm_u8_u32<'a>(alloc: Alloc<'a>) -> AVec<'a, u8> {
    let words = emit_sme2_gemm_u8_u32_words(alloc);
    words_to_bytes(&words, alloc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sme2_and_neon_encodings_match_apple_assembler() {
        // SME / SME2 streaming mode & ZA control
        assert_eq!(smstart(), 0xd503_477f);
        assert_eq!(smstop(), 0xd503_467f);
        assert_eq!(smstart_sm(), 0xd503_437f);
        assert_eq!(smstop_sm(), 0xd503_427f);
        assert_eq!(smstart_za(), 0xd503_457f);
        assert_eq!(smstop_za(), 0xd503_447f);
        assert_eq!(zero_za_all(), 0xc008_00ff);
        assert_eq!(zero_za_s_tiles(0b0001), 0xc008_0011); // zero {za0.s}
        assert_eq!(zero_za_s_tiles(0b1110), 0xc008_00ee); // zero {za1.s, za2.s, za3.s}
        assert_eq!(zero_za_d_tiles(0b0001_1110), 0xc008_001e); // zero {za1.d..za4.d}
        assert_eq!(rdsvl(0, 1), 0x04bf_5820);
        assert_eq!(rdsvl(8, 2), 0x04bf_5848);
        assert_eq!(addvl(0, 0, 1), 0x0420_5020);
        assert_eq!(addvl(10, 10, 2), 0x042a_504a);
        assert_eq!(addvl(10, 10, 8), 0x042a_510a);
        assert_eq!(addvl(10, 11, -1), 0x042b_57ea);
        assert_eq!(addvl(SP, SP, -2), 0x043f_57df);
        assert_eq!(sub_sp_uxtx(X13), 0xcb2d_63ff);
        assert_eq!(sub_sp_uxtx(X14), 0xcb2e_63ff);
        assert_eq!(cntb(0), 0x0420_e3e0);
        assert_eq!(cntb(19), 0x0420_e3f3);
        assert_eq!(cntw(0), 0x04a0_e3e0);
        assert_eq!(cntw(19), 0x04a0_e3f3);
        assert_eq!(cntd(0), 0x04e0_e3e0);
        assert_eq!(cntd(19), 0x04e0_e3f3);
        assert_eq!(incb(0), 0x0430_e3e0);
        assert_eq!(incb_mul(19, 2), 0x0431_e3f3);
        assert_eq!(incb_mul(19, 4), 0x0433_e3f3);
        assert_eq!(incw(0), 0x04b0_e3e0);
        assert_eq!(incw_mul(19, 2), 0x04b1_e3f3);
        assert_eq!(incw_mul(19, 3), 0x04b2_e3f3);
        assert_eq!(incd(0), 0x04f0_e3e0);
        assert_eq!(incd_mul(19, 4), 0x04f3_e3f3);

        // Streaming SVE predicates, SME2 predicate-as-counter & loads/stores
        assert_eq!(ptrue_b(0), 0x2518_e3e0);
        assert_eq!(ptrue_b(7), 0x2518_e3e7);
        assert_eq!(ptrue_s(0), 0x2598_e3e0);
        assert_eq!(ptrue_s(1), 0x2598_e3e1);
        assert_eq!(ptrue_d(0), 0x25d8_e3e0);
        assert_eq!(ptrue_d(7), 0x25d8_e3e7);
        assert_eq!(ptrue_pn_b(8), 0x2520_7810);
        assert_eq!(ptrue_pn_s(8), 0x25a0_7810);
        assert_eq!(ptrue_pn_s(9), 0x25a0_7811);
        assert_eq!(ptrue_pn_s(15), 0x25a0_7817);
        assert_eq!(whilelt_b(0, 0, 1), 0x2521_1400);
        assert_eq!(whilelt_b(1, 2, 3), 0x2523_1441);
        assert_eq!(whilelt_s(0, 0, 1), 0x25a1_1400);
        assert_eq!(whilelt_d(0, 0, 1), 0x25e1_1400);
        assert_eq!(whilelt_s(1, 2, 3), 0x25a3_1441);
        assert_eq!(whilelt_pn_b_vlx2(8, 0, 1), 0x2521_4410);
        assert_eq!(whilelt_pn_b_vlx2(9, 4, 23), 0x2537_4491);
        assert_eq!(whilelt_pn_s_vlx2(8, 0, 1), 0x25a1_4410);
        assert_eq!(whilelt_pn_s_vlx2(9, 4, 23), 0x25b7_4491);
        assert_eq!(whilelt_pn_s_vlx2(15, 19, 28), 0x25bc_4677);
        assert_eq!(whilelo_b(0, 0, 1), 0x2521_1c00);
        assert_eq!(whilelo_b(2, 4, 5), 0x2525_1c82);
        assert_eq!(whilelo_s(0, 0, 1), 0x25a1_1c00);
        assert_eq!(whilelo_d(0, 0, 1), 0x25e1_1c00);
        assert_eq!(ld1b_imm(0, 0, 0, 0), 0xa400_a000);
        assert_eq!(ld1b_imm(0, 0, 0, 1), 0xa401_a000);
        assert_eq!(ld1b_imm(0, 0, 0, -1), 0xa40f_a000);
        assert_eq!(ld1b_imm(5, 3, 8, 2), 0xa402_ad05);
        assert_eq!(st1b_imm(0, 0, 0, 0), 0xe400_e000);
        assert_eq!(st1b_imm(0, 0, 0, 1), 0xe401_e000);
        assert_eq!(st1b_imm(5, 3, 8, 2), 0xe402_ed05);
        assert_eq!(ld1w_imm(0, 0, 0, 0), 0xa540_a000);
        assert_eq!(ld1w_imm(0, 0, 0, 1), 0xa541_a000);
        assert_eq!(ld1w_imm(0, 0, 0, -1), 0xa54f_a000);
        assert_eq!(ld1w_reg(1, 1, 2, 3), 0xa543_4441);
        assert_eq!(st1w_imm(0, 0, 0, 0), 0xe540_e000);
        assert_eq!(st1w_imm(0, 0, 0, 1), 0xe541_e000);
        assert_eq!(st1w_reg(1, 1, 2, 3), 0xe543_4441);
        assert_eq!(ld1w_x2_imm(0, 8, 0, 0), 0xa040_4000);
        assert_eq!(ld1w_x2_imm(2, 8, 0, 0), 0xa040_4002);
        assert_eq!(ld1w_x2_imm(8, 9, 10, 0), 0xa040_4548);
        assert_eq!(ld1w_x2_imm(30, 15, 28, 7), 0xa047_5f9e);
        assert_eq!(ld1w_x2_imm(0, 8, 0, -1), 0xa04f_4000);
        assert_eq!(ld1w_x4_imm(0, 8, 0, 0), 0xa040_c000);
        assert_eq!(ld1w_x4_imm(4, 8, 10, 1), 0xa041_c144);
        assert_eq!(ld1w_x4_imm(28, 15, 28, 7), 0xa047_df9c);
        assert_eq!(st1w_x2_imm(0, 8, 0, 0), 0xa060_4000);
        assert_eq!(st1w_x2_imm(2, 8, 10, 1), 0xa061_4142);
        assert_eq!(st1w_x2_imm(8, 9, 11, 0), 0xa060_4568);
        assert_eq!(st1w_x2_imm(30, 15, 28, 7), 0xa067_5f9e);
        assert_eq!(st1w_x4_imm(0, 8, 0, 0), 0xa060_c000);
        assert_eq!(st1w_x4_imm(4, 8, 10, 1), 0xa061_c144);
        assert_eq!(st1w_x4_imm(28, 15, 28, 7), 0xa067_df9c);
        assert_eq!(ld1d_imm(0, 0, 0, 0), 0xa5e0_a000);
        assert_eq!(ld1d_imm(0, 0, 0, 1), 0xa5e1_a000);
        assert_eq!(ld1d_reg(1, 1, 2, 3), 0xa5e3_4441);
        assert_eq!(st1d_imm(0, 0, 0, 0), 0xe5e0_e000);
        assert_eq!(st1d_imm(0, 0, 0, 1), 0xe5e1_e000);
        assert_eq!(st1d_reg(1, 1, 2, 3), 0xe5e3_4441);

        // SVE interleave & zero
        assert_eq!(zip1_b(0, 1, 2), 0x0522_6020);
        assert_eq!(zip1_b(10, 12, 13), 0x052d_618a);
        assert_eq!(zip1_h(4, 0, 1), 0x0561_6004);
        assert_eq!(zip2_h(5, 0, 1), 0x0561_6405);
        assert_eq!(zip1_h(8, 10, 11), 0x056b_6148);
        assert_eq!(zip2_h(9, 10, 11), 0x056b_6549);
        assert_eq!(dup_zero_b(0), 0x2538_c000);
        assert_eq!(dup_zero_b(12), 0x2538_c00c);
        assert_eq!(dup_zero_b(15), 0x2538_c00f);

        // SME FMOPA / SMOPA / UMOPA outer products
        assert_eq!(fmopa_s(0, 0, 1, 0, 1), 0x8081_2000);
        assert_eq!(fmopa_s(1, 0, 1, 0, 1), 0x8081_2001);
        assert_eq!(fmopa_s(2, 0, 1, 0, 1), 0x8081_2002);
        assert_eq!(fmopa_s(3, 0, 1, 2, 3), 0x8083_2043);
        assert_eq!(fmopa_s(0, 2, 3, 4, 5), 0x8085_6880);
        assert_eq!(fmopa_d(0, 0, 1, 0, 1), 0x80c1_2000);
        assert_eq!(fmopa_d(7, 0, 1, 2, 3), 0x80c3_2047);
        assert_eq!(fmopa_d(6, 2, 3, 4, 5), 0x80c5_6886);
        assert_eq!(smopa_s(0, 0, 1, 0, 1), 0xa081_2000);
        assert_eq!(smopa_s(1, 0, 1, 0, 1), 0xa081_2001);
        assert_eq!(smopa_s(2, 0, 1, 0, 1), 0xa081_2002);
        assert_eq!(smopa_s(3, 0, 1, 2, 3), 0xa083_2043);
        assert_eq!(smopa_s(0, 2, 3, 4, 5), 0xa085_6880);
        assert_eq!(umopa_s(0, 0, 1, 0, 1), 0xa1a1_2000);
        assert_eq!(umopa_s(1, 0, 1, 0, 1), 0xa1a1_2001);
        assert_eq!(umopa_s(3, 0, 1, 2, 3), 0xa1a3_2043);
        assert_eq!(umopa_s(2, 2, 3, 4, 5), 0xa1a5_6882);

        // SME MOVA tile-to-vector and vector-to-tile (32-bit & 64-bit, horizontal & vertical)
        assert_eq!(mova_tile_to_z_h_s(0, 0, 0, 0, 0), 0xc082_0000);
        assert_eq!(mova_tile_to_z_h_s(0, 0, 1, 0, 2), 0xc082_00c0);
        assert_eq!(mova_tile_to_z_v_s(0, 0, 1, 0, 2), 0xc082_80c0);
        assert_eq!(mova_tile_to_z_v_s(0, 0, 0, 1, 0), 0xc082_a000);
        assert_eq!(mova_tile_to_z_h_s(5, 3, 2, 2, 3), 0xc082_4d65);
        assert_eq!(mova_tile_to_z_v_s(5, 3, 2, 2, 3), 0xc082_cd65);
        assert_eq!(mova_tile_to_z_v_s(0, 7, 3, 3, 3), 0xc082_fde0);

        assert_eq!(mova_z_to_tile_h_s(0, 0, 0, 0, 0), 0xc080_0000);
        assert_eq!(mova_z_to_tile_h_s(1, 0, 2, 0, 0), 0xc080_0006);
        assert_eq!(mova_z_to_tile_v_s(1, 0, 2, 0, 0), 0xc080_8006);
        assert_eq!(mova_z_to_tile_h_s(2, 2, 3, 3, 5), 0xc080_4cab);
        assert_eq!(mova_z_to_tile_v_s(2, 2, 3, 3, 5), 0xc080_ccab);
        assert_eq!(mova_z_to_tile_v_s(3, 3, 3, 7, 31), 0xc080_ffef);

        assert_eq!(mova_tile_to_z_h_d(0, 0, 0, 0, 0), 0xc0c2_0000);
        assert_eq!(mova_tile_to_z_h_d(0, 0, 3, 1, 1), 0xc0c2_20e0);
        assert_eq!(mova_tile_to_z_v_d(0, 0, 0, 0, 0), 0xc0c2_8000);
        assert_eq!(mova_tile_to_z_h_d(5, 3, 5, 2, 1), 0xc0c2_4d65);
        assert_eq!(mova_tile_to_z_v_d(5, 3, 5, 2, 1), 0xc0c2_cd65);
        assert_eq!(mova_tile_to_z_v_d(31, 7, 7, 3, 1), 0xc0c2_fdff);

        assert_eq!(mova_z_to_tile_h_d(0, 0, 0, 0, 0), 0xc0c0_0000);
        assert_eq!(mova_z_to_tile_h_d(3, 1, 1, 0, 0), 0xc0c0_2007);
        assert_eq!(mova_z_to_tile_h_d(5, 2, 1, 3, 5), 0xc0c0_4cab);
        assert_eq!(mova_z_to_tile_v_d(5, 2, 1, 3, 5), 0xc0c0_ccab);
        assert_eq!(mova_z_to_tile_v_d(7, 3, 1, 7, 31), 0xc0c0_ffef);

        // NEON SIMD idiom encoders
        assert_eq!(tbl1_16b(0, 1, 2), 0x4e02_0020);
        assert_eq!(tbl2_16b(7, 8, 10), 0x4e0a_2107);
        assert_eq!(tbl3_16b(7, 8, 11), 0x4e0b_4107);
        assert_eq!(tbl4_16b(0, 1, 5), 0x4e05_6020);
        assert_eq!(tbl4_16b(7, 8, 12), 0x4e0c_6107);
        assert_eq!(sdot_4s(0, 1, 2), 0x4e82_9420);
        assert_eq!(udot_4s(0, 1, 2), 0x6e82_9420);
        assert_eq!(smmla_4s(0, 1, 2), 0x4e82_a420);
        assert_eq!(ummla_4s(0, 1, 2), 0x6e82_a420);
    }

    #[test]
    fn emitted_sme2_kernels_have_balanced_streaming_section() {
        let f32_words = emit_sme2_gemm_f32_words(blaze_own::testing::heap().alloc());
        let f64_words = emit_sme2_gemm_f64_words(blaze_own::testing::heap().alloc());
        let i8_words = emit_sme2_gemm_i8_i32_words(blaze_own::testing::heap().alloc());
        let u8_words = emit_sme2_gemm_u8_u32_words(blaze_own::testing::heap().alloc());

        for (name, words) in [
            ("f32", f32_words.as_slice()),
            ("f64", f64_words.as_slice()),
            ("i8_i32", i8_words.as_slice()),
            ("u8_u32", u8_words.as_slice()),
        ] {
            let start_pos = words.iter().position(|&w| w == smstart());
            let stop_pos = words.iter().position(|&w| w == smstop());
            assert!(start_pos.is_some(), "{name} kernel must contain smstart");
            assert!(stop_pos.is_some(), "{name} kernel must contain smstop");
            assert!(
                start_pos.unwrap() < stop_pos.unwrap(),
                "{name} kernel must execute smstart before smstop"
            );
            assert_eq!(words.last().copied(), Some(enc::ret()));
        }

        // Verify 2x2 4-tile FMOPA.S (4x + 1x unrolled * 4 tiles = 20 >= 16),
        // 1x4 FMOPA.D (2x + 1x unrolled * 4 tiles = 12), and 2x2 SMOPA/UMOPA (4*4 = 16 >= 12).
        let f32_fmopas = f32_words.iter().filter(|&&w| (w & 0xffe0_0000) == 0x8080_0000).count();
        let f64_fmopas = f64_words.iter().filter(|&&w| (w & 0xffe0_0000) == 0x80c0_0000).count();
        let i8_smopas = i8_words.iter().filter(|&&w| (w & 0xffe0_0000) == 0xa080_0000).count();
        let u8_umopas = u8_words.iter().filter(|&&w| (w & 0xffe0_0000) == 0xa1a0_0000).count();
        assert!(f32_fmopas >= 16);
        assert_eq!(f64_fmopas, 12);
        assert!(i8_smopas >= 12);
        assert!(u8_umopas >= 12);
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    unsafe extern "C" {
        fn sysctlbyname(
            name: *const core::ffi::c_char,
            oldp: *mut core::ffi::c_void,
            oldlenp: *mut usize,
            newp: *const core::ffi::c_void,
            newlen: usize,
        ) -> core::ffi::c_int;
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn has_macos_feature(name: &core::ffi::CStr) -> bool {
        let mut val: i32 = 0;
        let mut len = core::mem::size_of::<i32>();
        // SAFETY: `name` is a valid NUL-terminated C string; `val` and `len` point to valid
        // writable stack storage matching `len`; `newp` is null and `newlen` is 0 (read-only query).
        let rc = unsafe {
            sysctlbyname(
                name.as_ptr(),
                (&raw mut val).cast::<core::ffi::c_void>(),
                &raw mut len,
                core::ptr::null(),
                0,
            )
        };
        rc == 0 && val != 0
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn ref_gemm_f32(c: &mut [f32], a: &[f32], b: &[f32], m: usize, n: usize, k: usize, accum: bool) {
        for i in 0..m {
            for j in 0..n {
                let mut sum = if accum { f64::from(c[i * n + j]) } else { 0.0 };
                for p in 0..k {
                    sum += f64::from(a[i * k + p]) * f64::from(b[p * n + j]);
                }
                c[i * n + j] = sum as f32;
            }
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn ref_gemm_f64(c: &mut [f64], a: &[f64], b: &[f64], m: usize, n: usize, k: usize, accum: bool) {
        for i in 0..m {
            for j in 0..n {
                let mut sum = if accum { c[i * n + j] } else { 0.0 };
                for p in 0..k {
                    sum += a[i * k + p] * b[p * n + j];
                }
                c[i * n + j] = sum;
            }
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn ref_gemm_i8_i32(c: &mut [i32], a: &[i8], b: &[i8], m: usize, n: usize, k: usize, accum: bool) {
        for i in 0..m {
            for j in 0..n {
                let mut sum = if accum { c[i * n + j] } else { 0 };
                for p in 0..k {
                    sum = sum.wrapping_add(i32::from(a[i * k + p]).wrapping_mul(i32::from(b[p * n + j])));
                }
                c[i * n + j] = sum;
            }
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn ref_gemm_u8_u32(c: &mut [u32], a: &[u8], b: &[u8], m: usize, n: usize, k: usize, accum: bool) {
        for i in 0..m {
            for j in 0..n {
                let mut sum = if accum { c[i * n + j] } else { 0 };
                for p in 0..k {
                    sum = sum.wrapping_add(u32::from(a[i * k + p]).wrapping_mul(u32::from(b[p * n + j])));
                }
                c[i * n + j] = sum;
            }
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn sme2_gemm_f32_executes_correctly_on_hardware() {
        if !has_macos_feature(c"hw.optional.arm.FEAT_SME2") {
            return;
        }

        let alloc = blaze_own::testing::heap().alloc();
        let code = emit_sme2_gemm_f32(alloc);
        let exec = blaze_inproc::exec::Executable::new(&code, 0)
            .expect("W^X executable mapping for SME2 f32 kernel must succeed");
        type GemmF32Fn = extern "C" fn(*mut f32, *const f32, *const f32, i64, i64, i64, i64);
        // SAFETY: `code` is valid AArch64 SME2 machine code matching `GemmF32Fn`'s C ABI,
        // and `exec` stays alive for all calls below.
        let kernel: GemmF32Fn = unsafe { exec.as_fn() };

        let shapes: &[(usize, usize, usize)] = &[
            (16, 16, 16),
            (64, 64, 64),
            (256, 256, 256),
            (33, 65, 67),
            (35, 37, 260),
            (19, 37, 23),
            (1, 1, 1),
            (3, 50, 17),
            (48, 48, 32),
            (4, 4, 0),
            (0, 16, 16),
        ];

        for &(m, n, k) in shapes {
            let mut a: AVec<'static, f32> = Vec::with_capacity_in(m * k, alloc);
            for idx in 0..m * k {
                a.push((((idx.wrapping_mul(37).wrapping_add(11)) % 97) as f32 - 48.0) * 0.03125);
            }
            let mut b: AVec<'static, f32> = Vec::with_capacity_in(k * n, alloc);
            for idx in 0..k * n {
                b.push((((idx.wrapping_mul(53).wrapping_add(7)) % 89) as f32 - 44.0) * 0.03125);
            }

            for &accum in &[false, true] {
                let mut got: AVec<'static, f32> = Vec::with_capacity_in(m * n, alloc);
                let mut expected: AVec<'static, f32> = Vec::with_capacity_in(m * n, alloc);
                for idx in 0..m * n {
                    let v = (((idx.wrapping_mul(19).wrapping_add(3)) % 61) as f32 - 30.0) * 0.125;
                    got.push(v);
                    expected.push(v);
                }

                let flags = if accum { SME2_GEMM_FLAG_ACCUM } else { 0 };
                kernel(
                    got.as_mut_ptr(),
                    a.as_ptr(),
                    b.as_ptr(),
                    m as i64,
                    n as i64,
                    k as i64,
                    flags,
                );
                ref_gemm_f32(&mut expected, &a, &b, m, n, k, accum);

                for idx in 0..m * n {
                    let diff = (got[idx] - expected[idx]).abs();
                    let scale = expected[idx].abs().max(1.0);
                    assert!(
                        diff / scale < 1e-5,
                        "SME2 f32 GEMM mismatch at shape=({m},{n},{k}) accum={accum} idx={idx}: got={} expected={} rel={}",
                        got[idx],
                        expected[idx],
                        diff / scale,
                    );
                }
            }
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn sme2_gemm_f64_executes_correctly_on_hardware() {
        if !has_macos_feature(c"hw.optional.arm.FEAT_SME2")
            || !has_macos_feature(c"hw.optional.arm.FEAT_SME_F64F64")
        {
            return;
        }

        let alloc = blaze_own::testing::heap().alloc();
        let code = emit_sme2_gemm_f64(alloc);
        let exec = blaze_inproc::exec::Executable::new(&code, 0)
            .expect("W^X executable mapping for SME2 f64 kernel must succeed");
        type GemmF64Fn = extern "C" fn(*mut f64, *const f64, *const f64, i64, i64, i64, i64);
        // SAFETY: `code` is valid AArch64 SME2+F64F64 machine code matching `GemmF64Fn`'s C ABI,
        // and `exec` stays alive for all calls below.
        let kernel: GemmF64Fn = unsafe { exec.as_fn() };

        let shapes: &[(usize, usize, usize)] = &[
            (8, 8, 8),
            (16, 16, 16),
            (64, 64, 64),
            (19, 37, 23),
            (1, 1, 1),
            (3, 35, 11),
            (4, 4, 0),
            (0, 8, 8),
        ];

        for &(m, n, k) in shapes {
            let mut a: AVec<'static, f64> = Vec::with_capacity_in(m * k, alloc);
            for idx in 0..m * k {
                a.push((((idx.wrapping_mul(37).wrapping_add(11)) % 97) as f64 - 48.0) * 0.03125);
            }
            let mut b: AVec<'static, f64> = Vec::with_capacity_in(k * n, alloc);
            for idx in 0..k * n {
                b.push((((idx.wrapping_mul(53).wrapping_add(7)) % 89) as f64 - 44.0) * 0.03125);
            }

            for &accum in &[false, true] {
                let mut got: AVec<'static, f64> = Vec::with_capacity_in(m * n, alloc);
                let mut expected: AVec<'static, f64> = Vec::with_capacity_in(m * n, alloc);
                for idx in 0..m * n {
                    let v = (((idx.wrapping_mul(19).wrapping_add(3)) % 61) as f64 - 30.0) * 0.125;
                    got.push(v);
                    expected.push(v);
                }

                let flags = if accum { SME2_GEMM_FLAG_ACCUM } else { 0 };
                kernel(
                    got.as_mut_ptr(),
                    a.as_ptr(),
                    b.as_ptr(),
                    m as i64,
                    n as i64,
                    k as i64,
                    flags,
                );
                ref_gemm_f64(&mut expected, &a, &b, m, n, k, accum);

                for idx in 0..m * n {
                    let diff = (got[idx] - expected[idx]).abs();
                    let scale = expected[idx].abs().max(1.0);
                    assert!(
                        diff / scale < 1e-12,
                        "SME2 f64 GEMM mismatch at shape=({m},{n},{k}) accum={accum} idx={idx}: got={} expected={} rel={}",
                        got[idx],
                        expected[idx],
                        diff / scale,
                    );
                }
            }
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn sme2_gemm_i8_i32_executes_correctly_on_hardware() {
        if !has_macos_feature(c"hw.optional.arm.FEAT_SME2") {
            return;
        }

        let code = emit_sme2_gemm_i8_i32(blaze_own::testing::heap().alloc());
        let exec = blaze_inproc::exec::Executable::new(&code, 0)
            .expect("W^X executable mapping for SME2 i8->i32 SMOPA kernel must succeed");
        type GemmI8Fn = extern "C" fn(*mut i32, *const i8, *const i8, i64, i64, i64, i64);
        // SAFETY: `code` is valid AArch64 SME2 machine code matching `GemmI8Fn`'s C ABI,
        // and `exec` stays alive for all calls below.
        let kernel: GemmI8Fn = unsafe { exec.as_fn() };

        let shapes: &[(usize, usize, usize)] = &[
            (16, 16, 16),
            (32, 32, 32),
            (64, 64, 64),
            (33, 65, 67),
            (35, 37, 1030),
            (19, 37, 23),
            (1, 1, 1),
            (3, 50, 17),
            (4, 4, 0),
            (0, 16, 16),
        ];

        for &(m, n, k) in shapes {
            let a: std::vec::Vec<i8> = (0..m * k)
                .map(|idx| (((idx.wrapping_mul(37).wrapping_add(11)) % 251) as i32 - 125) as i8)
                .collect();
            let b: std::vec::Vec<i8> = (0..k * n)
                .map(|idx| (((idx.wrapping_mul(53).wrapping_add(7)) % 241) as i32 - 120) as i8)
                .collect();

            for &accum in &[false, true] {
                let init_c: std::vec::Vec<i32> = (0..m * n)
                    .map(|idx| ((idx.wrapping_mul(19).wrapping_add(3)) % 1009) as i32 - 500)
                    .collect();
                let mut got = init_c.clone();
                let mut expected = init_c;

                let flags = if accum { SME2_GEMM_FLAG_ACCUM } else { 0 };
                kernel(
                    got.as_mut_ptr(),
                    a.as_ptr(),
                    b.as_ptr(),
                    m as i64,
                    n as i64,
                    k as i64,
                    flags,
                );
                ref_gemm_i8_i32(&mut expected, &a, &b, m, n, k, accum);

                for idx in 0..m * n {
                    assert_eq!(
                        got[idx], expected[idx],
                        "SME2 i8->i32 SMOPA mismatch at shape=({m},{n},{k}) accum={accum} idx={idx}"
                    );
                }
            }
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn sme2_gemm_u8_u32_executes_correctly_on_hardware() {
        if !has_macos_feature(c"hw.optional.arm.FEAT_SME2") {
            return;
        }

        let code = emit_sme2_gemm_u8_u32(blaze_own::testing::heap().alloc());
        let exec = blaze_inproc::exec::Executable::new(&code, 0)
            .expect("W^X executable mapping for SME2 u8->u32 UMOPA kernel must succeed");
        type GemmU8Fn = extern "C" fn(*mut u32, *const u8, *const u8, i64, i64, i64, i64);
        // SAFETY: `code` is valid AArch64 SME2 machine code matching `GemmU8Fn`'s C ABI,
        // and `exec` stays alive for all calls below.
        let kernel: GemmU8Fn = unsafe { exec.as_fn() };

        let shapes: &[(usize, usize, usize)] = &[
            (16, 16, 16),
            (32, 32, 32),
            (64, 64, 64),
            (33, 65, 67),
            (35, 37, 1030),
            (19, 37, 23),
            (1, 1, 1),
            (3, 50, 17),
            (4, 4, 0),
            (0, 16, 16),
        ];

        for &(m, n, k) in shapes {
            let a: std::vec::Vec<u8> = (0..m * k)
                .map(|idx| ((idx.wrapping_mul(37).wrapping_add(11)) % 251) as u8)
                .collect();
            let b: std::vec::Vec<u8> = (0..k * n)
                .map(|idx| ((idx.wrapping_mul(53).wrapping_add(7)) % 241) as u8)
                .collect();

            for &accum in &[false, true] {
                let init_c: std::vec::Vec<u32> = (0..m * n)
                    .map(|idx| ((idx.wrapping_mul(19).wrapping_add(3)) % 1009) as u32)
                    .collect();
                let mut got = init_c.clone();
                let mut expected = init_c;

                let flags = if accum { SME2_GEMM_FLAG_ACCUM } else { 0 };
                kernel(
                    got.as_mut_ptr(),
                    a.as_ptr(),
                    b.as_ptr(),
                    m as i64,
                    n as i64,
                    k as i64,
                    flags,
                );
                ref_gemm_u8_u32(&mut expected, &a, &b, m, n, k, accum);

                for idx in 0..m * n {
                    assert_eq!(
                        got[idx], expected[idx],
                        "SME2 u8->u32 UMOPA mismatch at shape=({m},{n},{k}) accum={accum} idx={idx}"
                    );
                }
            }
        }
    }
}
