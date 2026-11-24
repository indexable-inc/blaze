//! AArch64 Hardware SME2 (`FMOPA`) + NEON (`TBL`/`SDOT`/`UDOT`/`SMMLA`/`UMMLA`)
//! Instruction Encoders & SVL-Agnostic Executable GEMM Micro-Kernel Emitters.
//!
//! Every 32-bit instruction word in this module is pinned bit-for-bit against Apple's
//! `arm64` assembler (`-march=armv9-a+sme2+sme-f64f64+dotprod+i8mm`).
//!
//! # Micro-Kernel Architecture (`emit_sme2_gemm_f32` / `emit_sme2_gemm_f64`)
//!
//! - **ABI**:
//!   `extern "C" fn(c: *mut T, a: *const T, b: *const T, m: i64, n: i64, k: i64, flags: i64)`
//!   where row-major `A` is `m × k`, `B` is `k × n`, `C` is `m × n`, and
//!   `flags & SME2_GEMM_FLAG_ACCUM != 0` computes `C += A * B` while `flags & 1 == 0`
//!   computes `C = A * B`.
//! - **Zero-Stack In-Register `ZA` Transpose**:
//!   `za0` acts as a hardware transpose scratchpad for `SVL × SVL` blocks of `A`
//!   (loaded horizontally via `ld1w`/`ld1d` + `mova za0h`, then streamed out vertically
//!   via `mova z0, za0v` at 1 cycle per `k` step).
//! - **Multi-Tile Outer-Product Blocking**:
//!   - `f32`: 3 side-by-side accumulator tiles (`za1.s..=za3.s` = `SVL_s × 3*SVL_s`;
//!     `16 × 48` = 768 FMAs / 1,536 FLOPs per `kk` iteration on 512-bit SVL Apple Silicon).
//!   - `f64`: 4 side-by-side accumulator tiles (`za1.d..=za4.d` = `SVL_d × 4*SVL_d`;
//!     `8 × 32` = 256 FMAs / 512 FLOPs per `kk` iteration on 512-bit SVL Apple Silicon).
//! - **Arbitrary Dimensions Without Scalar Cleanup**:
//!   Streaming SVE `whilelt` predicates (`p0..p5`) guard every load, store, `mova`, and
//!   `fmopa`, handling arbitrary non-multiple `(m, n, k)` shapes cleanly with zero
//!   out-of-bounds memory accesses.
//! - **Apple Silicon AAPCS64 Streaming ABI**:
//!   Callee-saved GPRs (`x19..=x28`, `x29`, `x30`) and `d8..=d15` are saved BEFORE
//!   `smstart` and restored AFTER `smstop` (since `smstart`/`smstop` zero `v0..=v31`).

use blaze_own::{AVec, Alloc, Vec};

use crate::enc::{self, Cond, FP, LR, SP, X0, X1, X2, X8, X9, X10, X11, X12, X13};

const X3: u32 = 3;
const X4: u32 = 4;
const X5: u32 = 5;
const X6: u32 = 6;
const X7: u32 = 7;

const X14: u32 = 14;
const X15: u32 = 15;
const X16: u32 = 16;
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

// ---------------------------------------------------------------------------
// 2. Streaming SVE Predicates, Vector Length Increments & Contiguous LD1/ST1
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// 3. SME `FMOPA` Outer-Product & `MOVA` Tile Read/Write
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
// 5. Standalone SVL-Agnostic SME2 `FMOPA` GEMM Micro-Kernel Emitters
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum GemmPrec {
    F32,
    F64,
}

impl GemmPrec {
    const fn elem_shift(self) -> u32 {
        match self {
            Self::F32 => 2,
            Self::F64 => 3,
        }
    }

    const fn tile_count(self) -> u32 {
        match self {
            Self::F32 => 3,
            Self::F64 => 4,
        }
    }

    const fn k_pred(self) -> u32 {
        match self {
            Self::F32 => 4,
            Self::F64 => 5,
        }
    }

    const fn cnt_lanes(self, xd: u32) -> u32 {
        match self {
            Self::F32 => cntw(xd),
            Self::F64 => cntd(xd),
        }
    }

    const fn inc_one(self, xd: u32) -> u32 {
        match self {
            Self::F32 => incw(xd),
            Self::F64 => incd(xd),
        }
    }

    const fn inc_tiles(self, xd: u32) -> u32 {
        match self {
            Self::F32 => incw_mul(xd, self.tile_count()),
            Self::F64 => incd_mul(xd, self.tile_count()),
        }
    }

    const fn whilelt(self, pd: u32, rn: u32, rm: u32) -> u32 {
        match self {
            Self::F32 => whilelt_s(pd, rn, rm),
            Self::F64 => whilelt_d(pd, rn, rm),
        }
    }

    const fn ld1_imm(self, zt: u32, pg: u32, rn: u32, imm4_vl: i32) -> u32 {
        match self {
            Self::F32 => ld1w_imm(zt, pg, rn, imm4_vl),
            Self::F64 => ld1d_imm(zt, pg, rn, imm4_vl),
        }
    }

    const fn st1_imm(self, zt: u32, pg: u32, rn: u32, imm4_vl: i32) -> u32 {
        match self {
            Self::F32 => st1w_imm(zt, pg, rn, imm4_vl),
            Self::F64 => st1d_imm(zt, pg, rn, imm4_vl),
        }
    }

    const fn zero_accum_tiles(self) -> u32 {
        match self {
            // za1.s, za2.s, za3.s
            Self::F32 => zero_za_s_tiles(0b1110),
            // za1.d, za2.d, za3.d, za4.d
            Self::F64 => zero_za_d_tiles(0b0001_1110),
        }
    }

    const fn mova_z_to_h(self, tile: u32, pg: u32, zn: u32) -> u32 {
        self.mova_z_to_h_imm(tile, 0, pg, zn)
    }

    const fn mova_z_to_h_imm(self, tile: u32, imm: u32, pg: u32, zn: u32) -> u32 {
        match self {
            Self::F32 => mova_z_to_tile_h_s(tile, 0, imm, pg, zn),
            Self::F64 => mova_z_to_tile_h_d(tile, 0, imm, pg, zn),
        }
    }

    const fn mova_h_to_z(self, zd: u32, pg: u32, tile: u32) -> u32 {
        match self {
            Self::F32 => mova_tile_to_z_h_s(zd, pg, tile, 0, 0),
            Self::F64 => mova_tile_to_z_h_d(zd, pg, tile, 0, 0),
        }
    }

    const fn mova_v_to_z(self, zd: u32, pg: u32, tile: u32) -> u32 {
        self.mova_v_to_z_imm(zd, pg, tile, 0)
    }

    const fn mova_v_to_z_imm(self, zd: u32, pg: u32, tile: u32, imm: u32) -> u32 {
        match self {
            Self::F32 => mova_tile_to_z_v_s(zd, pg, tile, 0, imm),
            Self::F64 => mova_tile_to_z_v_d(zd, pg, tile, 0, imm),
        }
    }

    const fn fmopa(self, tile: u32, pn: u32, pm: u32, zn: u32, zm: u32) -> u32 {
        match self {
            Self::F32 => fmopa_s(tile, pn, pm, zn, zm),
            Self::F64 => fmopa_d(tile, pn, pm, zn, zm),
        }
    }
}

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

fn emit_sme2_gemm_words<'a>(prec: GemmPrec, alloc: Alloc<'a>) -> AVec<'a, u32> {
    let mut buf = SmeBuf::new_in(192, alloc);
    let elem_shift = prec.elem_shift();
    let tiles = prec.tile_count();
    let k_pred = prec.k_pred();

    // ------------------------------------------------------------------
    // Early exit before `smstart` for empty output or no-op `k <= 0`:
    //   if m <= 0 || n <= 0 || (k <= 0 && (flags & 1) != 0) { return; }
    // ------------------------------------------------------------------
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

    // ------------------------------------------------------------------
    // Prologue: save FP/LR, X19..=X28, and D8..=D15 BEFORE `smstart`
    // (160-byte 16B-aligned frame; `smstart` zeroes `v0..=v31`).
    // ------------------------------------------------------------------
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

    // Preserve inputs across loops in X19..=X25.
    buf.emit(enc::mov(X19, X0)); // c_base
    buf.emit(enc::mov(X20, X1)); // a_base
    buf.emit(enc::mov(X21, X2)); // b_base
    buf.emit(enc::mov(X22, X3)); // m
    buf.emit(enc::mov(X23, X4)); // n
    buf.emit(enc::mov(X24, X5)); // k
    buf.emit(enc::mov(X25, X6)); // flags

    // Enter Streaming SVE + ZA mode.
    buf.emit(smstart());

    // Query SVL lane count (`SVL_s` or `SVL_d`) and precompute row byte strides.
    buf.emit(prec.cnt_lanes(X26)); // svl
    buf.emit(enc::lsl_imm(X27, X23, elem_shift)); // n_bytes = n * sizeof(T)
    buf.emit(enc::lsl_imm(X28, X24, elem_shift)); // k_bytes = k * sizeof(T)

    // ------------------------------------------------------------------
    // Outer `i` loop: `i = 0, svl, 2*svl, ... < m`
    // ------------------------------------------------------------------
    buf.emit(enc::movz(X0, 0, 0)); // i = 0
    let i_loop = buf.pos();

    buf.emit(prec.whilelt(0, X0, X22)); // p0 = rows [i, i + svl)
    buf.emit(enc::sub(X13, X22, X0)); // m - i
    buf.emit(enc::cmp(X13, X26));
    buf.emit(enc::csel(X1, X13, X26, Cond::Lt)); // rows_left = min(m - i, svl)
    buf.emit(enc::mul(X13, X0, X28)); // i * k_bytes
    buf.emit(enc::add(X2, X20, X13)); // a_i_row = &A[i, 0]
    buf.emit(enc::mul(X13, X0, X27)); // i * n_bytes
    buf.emit(enc::add(X3, X19, X13)); // c_i_row = &C[i, 0]

    // ------------------------------------------------------------------
    // Middle `j` loop: `j = 0, tiles*svl, ... < n`
    // ------------------------------------------------------------------
    buf.emit(enc::movz(X4, 0, 0)); // j = 0
    let j_loop = buf.pos();

    // Predicates p1..=p{tiles} for side-by-side column strips.
    buf.emit(prec.whilelt(1, X4, X23));
    buf.emit(enc::add(X13, X4, X26));
    buf.emit(prec.whilelt(2, X13, X23));
    buf.emit(enc::add(X13, X13, X26));
    buf.emit(prec.whilelt(3, X13, X23));
    if tiles == 4 {
        buf.emit(enc::add(X13, X13, X26));
        buf.emit(prec.whilelt(4, X13, X23));
    }

    buf.emit(enc::add_lsl(X5, X3, X4, elem_shift)); // c_ij = &C[i, j]
    buf.emit(enc::add_lsl(X6, X21, X4, elem_shift)); // b_0j = &B[0, j]

    // Initialize accumulator tiles: overwrite -> `zero {za1..zaT}`, accum -> load C.
    let br_zero_c = buf.pos();
    buf.emit(enc::tbz(X25, 0, 0));

    // Accumulate mode: load `rows_left` rows of C into `za1..=za{tiles}`.
    buf.emit(enc::mov(X11, X5)); // c_ptr = c_ij
    buf.emit(enc::movz(X12, 0, 0)); // r = 0
    let load_c_loop = buf.pos();
    for t in 1..=tiles {
        let vl_off = t.saturating_sub(1).cast_signed();
        buf.emit(prec.ld1_imm(t, t, X11, vl_off));
    }
    for t in 1..=tiles {
        buf.emit(prec.mova_z_to_h(t, t, t));
    }
    buf.emit(enc::add(X11, X11, X27)); // c_ptr += n_bytes
    buf.emit(enc::add_imm(X12, X12, 1, false)); // r += 1
    buf.emit(enc::cmp(X12, X1));
    buf.emit_b_cond_to(Cond::Lo, load_c_loop);

    let br_init_done = buf.pos();
    buf.emit(enc::b(0));

    let init_zero = buf.pos();
    buf.patch_tbz(br_zero_c, X25, 0, init_zero);
    buf.emit(prec.zero_accum_tiles());

    let init_done = buf.pos();
    buf.patch_b(br_init_done, init_done);

    // ------------------------------------------------------------------
    // `k_block` loop: `k_idx = 0, svl, 2*svl, ... < k`
    // ------------------------------------------------------------------
    buf.emit(enc::movz(X7, 0, 0)); // k_idx = 0
    buf.emit(enc::cmp(X7, X24));
    let br_skip_k = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));

    let k_loop = buf.pos();
    buf.emit(prec.whilelt(k_pred, X7, X24)); // p_k = active k lanes
    buf.emit(enc::sub(X13, X24, X7)); // k - k_idx
    buf.emit(enc::cmp(X13, X26));
    buf.emit(enc::csel(X8, X13, X26, Cond::Lt)); // k_left = min(k - k_idx, svl)

    // Phase 1: load `rows_left` rows of A[i..i+rows_left, k_idx..k_idx+k_left]
    // horizontally into `za0` (in-register hardware transpose buffer), 2x unrolled.
    buf.emit(enc::add_lsl(X9, X2, X7, elem_shift)); // a_ptr = &A[i, k_idx]
    buf.emit(enc::movz(X12, 0, 0)); // r = 0
    buf.emit(enc::sub_imm(X13, X1, 1, false)); // rows_left - 1
    buf.emit(enc::cmp(X12, X13));
    let br_a_tail = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));
    let load_a_loop2 = buf.pos();
    buf.emit(enc::add(X14, X9, X28)); // a_ptr + k_bytes
    buf.emit(prec.ld1_imm(0, k_pred, X9, 0));
    buf.emit(prec.ld1_imm(1, k_pred, X14, 0));
    buf.emit(prec.mova_z_to_h_imm(0, 0, k_pred, 0));
    buf.emit(prec.mova_z_to_h_imm(0, 1, k_pred, 1));
    buf.emit(enc::add(X9, X14, X28)); // a_ptr += 2 * k_bytes
    buf.emit(enc::add_imm(X12, X12, 2, false)); // r += 2
    buf.emit(enc::cmp(X12, X13));
    buf.emit_b_cond_to(Cond::Lt, load_a_loop2);
    let a_tail = buf.pos();
    buf.patch_b_cond(br_a_tail, Cond::Ge, a_tail);
    buf.emit(enc::cmp(X12, X1));
    let br_a_done = buf.pos();
    buf.emit(enc::b_cond(Cond::Hs, 0));
    buf.emit(prec.ld1_imm(0, k_pred, X9, 0));
    buf.emit(prec.mova_z_to_h(0, k_pred, 0));
    let a_done = buf.pos();
    buf.patch_b_cond(br_a_done, Cond::Hs, a_done);

    // Phase 2: 4x-unrolled (`f32`, 12 FMOPAs/iter = 3,072 FMAs = 6,144 FLOPs/branch!) or
    // 2x-unrolled (`f64`, 8 FMOPAs/iter) double-buffered stream of `k_left` vertical columns of `A`
    // from `za0v` and row slices of `B` into `tiles` parallel `FMOPA` outer products!
    buf.emit(enc::mul(X13, X7, X27)); // k_idx * n_bytes
    buf.emit(enc::add(X10, X6, X13)); // b_ptr = &B[k_idx, j]
    buf.emit(enc::movz(X12, 0, 0)); // kk = 0
    if matches!(prec, GemmPrec::F32) {
        buf.emit(enc::sub_imm(X14, X8, 3, false)); // k_left - 3
        buf.emit(enc::cmp(X12, X14));
        let br_kk4_tail = buf.pos();
        buf.emit(enc::b_cond(Cond::Ge, 0));
        let kk_loop4 = buf.pos();
        buf.emit(enc::add(X13, X10, X27)); // b_ptr_1 = b_ptr + n_bytes
        buf.emit(enc::add(X15, X13, X27)); // b_ptr_2 = b_ptr + 2*n_bytes
        buf.emit(enc::add(X16, X15, X27)); // b_ptr_3 = b_ptr + 3*n_bytes
        // Wave 0 & 1 loads
        buf.emit(prec.mova_v_to_z_imm(0, 0, 0, 0));
        for t in 1..=tiles {
            buf.emit(prec.ld1_imm(t, t, X10, t.saturating_sub(1).cast_signed()));
        }
        buf.emit(prec.mova_v_to_z_imm(4, 0, 0, 1));
        for t in 1..=tiles {
            buf.emit(prec.ld1_imm(4 + t, t, X13, t.saturating_sub(1).cast_signed()));
        }
        // Wave 2 & 3 loads + interleaved Wave 0 FMOPAs to overlap load latency with matrix unit!
        buf.emit(prec.mova_v_to_z_imm(8, 0, 0, 2));
        for t in 1..=tiles {
            buf.emit(prec.fmopa(t, 0, t, 0, t));
        }
        for t in 1..=tiles {
            buf.emit(prec.ld1_imm(8 + t, t, X15, t.saturating_sub(1).cast_signed()));
        }
        buf.emit(prec.mova_v_to_z_imm(12, 0, 0, 3));
        for t in 1..=tiles {
            buf.emit(prec.fmopa(t, 0, t, 4, 4 + t));
        }
        for t in 1..=tiles {
            buf.emit(prec.ld1_imm(12 + t, t, X16, t.saturating_sub(1).cast_signed()));
        }
        for t in 1..=tiles {
            buf.emit(prec.fmopa(t, 0, t, 8, 8 + t));
        }
        for t in 1..=tiles {
            buf.emit(prec.fmopa(t, 0, t, 12, 12 + t));
        }
        buf.emit(enc::add(X10, X16, X27)); // b_ptr += 4 * n_bytes
        buf.emit(enc::add_imm(X12, X12, 4, false)); // kk += 4
        buf.emit(enc::cmp(X12, X14));
        buf.emit_b_cond_to(Cond::Lt, kk_loop4);
        let kk4_tail = buf.pos();
        buf.patch_b_cond(br_kk4_tail, Cond::Ge, kk4_tail);
    }
    buf.emit(enc::sub_imm(X14, X8, 1, false)); // k_left - 1
    buf.emit(enc::cmp(X12, X14));
    let br_kk_tail = buf.pos();
    buf.emit(enc::b_cond(Cond::Ge, 0));
    let kk_loop2 = buf.pos();
    buf.emit(enc::add(X13, X10, X27)); // b_ptr_1 = b_ptr + n_bytes
    buf.emit(prec.mova_v_to_z_imm(0, 0, 0, 0)); // z0 = A[.., k_idx + kk]
    for t in 1..=tiles {
        let vl_off = t.saturating_sub(1).cast_signed();
        buf.emit(prec.ld1_imm(t, t, X10, vl_off)); // z1..z{tiles} = B[kk, j..]
    }
    buf.emit(prec.mova_v_to_z_imm(8, 0, 0, 1)); // z8 = A[.., k_idx + kk + 1]
    for t in 1..=tiles {
        let vl_off = t.saturating_sub(1).cast_signed();
        buf.emit(prec.ld1_imm(8 + t, t, X13, vl_off)); // z9..z{8+tiles} = B[kk+1, j..]
    }
    for t in 1..=tiles {
        buf.emit(prec.fmopa(t, 0, t, 0, t)); // za{t} += z0 (outer) z{t}
    }
    for t in 1..=tiles {
        buf.emit(prec.fmopa(t, 0, t, 8, 8 + t)); // za{t} += z8 (outer) z{8+t}
    }
    buf.emit(enc::add(X10, X13, X27)); // b_ptr += 2 * n_bytes
    buf.emit(enc::add_imm(X12, X12, 2, false)); // kk += 2
    buf.emit(enc::cmp(X12, X14));
    buf.emit_b_cond_to(Cond::Lt, kk_loop2);
    let kk_tail = buf.pos();
    buf.patch_b_cond(br_kk_tail, Cond::Ge, kk_tail);
    buf.emit(enc::cmp(X12, X8));
    let br_kk_done = buf.pos();
    buf.emit(enc::b_cond(Cond::Hs, 0));
    buf.emit(prec.mova_v_to_z(0, 0, 0));
    for t in 1..=tiles {
        let vl_off = t.saturating_sub(1).cast_signed();
        buf.emit(prec.ld1_imm(t, t, X10, vl_off));
    }
    for t in 1..=tiles {
        buf.emit(prec.fmopa(t, 0, t, 0, t));
    }
    let kk_done = buf.pos();
    buf.patch_b_cond(br_kk_done, Cond::Hs, kk_done);

    buf.emit(prec.inc_one(X7)); // k_idx += svl
    buf.emit(enc::cmp(X7, X24));
    buf.emit_b_cond_to(Cond::Lt, k_loop);

    // ------------------------------------------------------------------
    // Store `rows_left` rows of `za1..=za{tiles}` back to `C[i.., j..]`.
    // ------------------------------------------------------------------
    let store_c = buf.pos();
    buf.patch_b_cond(br_skip_k, Cond::Ge, store_c);

    buf.emit(enc::mov(X11, X5)); // c_ptr = c_ij
    buf.emit(enc::movz(X12, 0, 0)); // r = 0
    let store_c_loop = buf.pos();
    for t in 1..=tiles {
        buf.emit(prec.mova_h_to_z(t, t, t));
    }
    for t in 1..=tiles {
        let vl_off = t.saturating_sub(1).cast_signed();
        buf.emit(prec.st1_imm(t, t, X11, vl_off));
    }
    buf.emit(enc::add(X11, X11, X27)); // c_ptr += n_bytes
    buf.emit(enc::add_imm(X12, X12, 1, false)); // r += 1
    buf.emit(enc::cmp(X12, X1));
    buf.emit_b_cond_to(Cond::Lo, store_c_loop);

    buf.emit(prec.inc_tiles(X4)); // j += tiles * svl
    buf.emit(enc::cmp(X4, X23));
    buf.emit_b_cond_to(Cond::Lt, j_loop);

    buf.emit(prec.inc_one(X0)); // i += svl
    buf.emit(enc::cmp(X0, X22));
    buf.emit_b_cond_to(Cond::Lt, i_loop);

    // ------------------------------------------------------------------
    // Epilogue: exit Streaming SVE + ZA, then restore D8..=D15 & GPRs.
    // ------------------------------------------------------------------
    buf.emit(smstop());

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

    buf.into_words()
}

/// Emits the standalone SVL-agnostic AArch64 SME2 `f32` `FMOPA` GEMM micro-kernel as instruction words.
///
/// Signature:
/// `extern "C" fn(c: *mut f32, a: *const f32, b: *const f32, m: i64, n: i64, k: i64, flags: i64)`
pub fn emit_sme2_gemm_f32_words<'a>(alloc: Alloc<'a>) -> AVec<'a, u32> {
    emit_sme2_gemm_words(GemmPrec::F32, alloc)
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
/// (requires `FEAT_SME_F64F64`).
///
/// Signature:
/// `extern "C" fn(c: *mut f64, a: *const f64, b: *const f64, m: i64, n: i64, k: i64, flags: i64)`
pub fn emit_sme2_gemm_f64_words<'a>(alloc: Alloc<'a>) -> AVec<'a, u32> {
    emit_sme2_gemm_words(GemmPrec::F64, alloc)
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
        assert_eq!(cntw(0), 0x04a0_e3e0);
        assert_eq!(cntw(19), 0x04a0_e3f3);
        assert_eq!(cntd(0), 0x04e0_e3e0);
        assert_eq!(cntd(19), 0x04e0_e3f3);
        assert_eq!(incw(0), 0x04b0_e3e0);
        assert_eq!(incw_mul(19, 2), 0x04b1_e3f3);
        assert_eq!(incw_mul(19, 3), 0x04b2_e3f3);
        assert_eq!(incd(0), 0x04f0_e3e0);
        assert_eq!(incd_mul(19, 4), 0x04f3_e3f3);

        // Streaming SVE predicates & loads/stores
        assert_eq!(ptrue_s(0), 0x2598_e3e0);
        assert_eq!(ptrue_s(1), 0x2598_e3e1);
        assert_eq!(ptrue_d(0), 0x25d8_e3e0);
        assert_eq!(ptrue_d(7), 0x25d8_e3e7);
        assert_eq!(whilelt_s(0, 0, 1), 0x25a1_1400);
        assert_eq!(whilelt_d(0, 0, 1), 0x25e1_1400);
        assert_eq!(whilelt_s(1, 2, 3), 0x25a3_1441);
        assert_eq!(whilelo_s(0, 0, 1), 0x25a1_1c00);
        assert_eq!(whilelo_d(0, 0, 1), 0x25e1_1c00);
        assert_eq!(ld1w_imm(0, 0, 0, 0), 0xa540_a000);
        assert_eq!(ld1w_imm(0, 0, 0, 1), 0xa541_a000);
        assert_eq!(ld1w_imm(0, 0, 0, -1), 0xa54f_a000);
        assert_eq!(ld1w_reg(1, 1, 2, 3), 0xa543_4441);
        assert_eq!(st1w_imm(0, 0, 0, 0), 0xe540_e000);
        assert_eq!(st1w_imm(0, 0, 0, 1), 0xe541_e000);
        assert_eq!(st1w_reg(1, 1, 2, 3), 0xe543_4441);
        assert_eq!(ld1d_imm(0, 0, 0, 0), 0xa5e0_a000);
        assert_eq!(ld1d_imm(0, 0, 0, 1), 0xa5e1_a000);
        assert_eq!(ld1d_reg(1, 1, 2, 3), 0xa5e3_4441);
        assert_eq!(st1d_imm(0, 0, 0, 0), 0xe5e0_e000);
        assert_eq!(st1d_imm(0, 0, 0, 1), 0xe5e1_e000);
        assert_eq!(st1d_reg(1, 1, 2, 3), 0xe5e3_4441);

        // SME FMOPA outer products
        assert_eq!(fmopa_s(0, 0, 1, 0, 1), 0x8081_2000);
        assert_eq!(fmopa_s(1, 0, 1, 0, 1), 0x8081_2001);
        assert_eq!(fmopa_s(2, 0, 1, 0, 1), 0x8081_2002);
        assert_eq!(fmopa_s(3, 0, 1, 2, 3), 0x8083_2043);
        assert_eq!(fmopa_s(0, 2, 3, 4, 5), 0x8085_6880);
        assert_eq!(fmopa_d(0, 0, 1, 0, 1), 0x80c1_2000);
        assert_eq!(fmopa_d(7, 0, 1, 2, 3), 0x80c3_2047);
        assert_eq!(fmopa_d(6, 2, 3, 4, 5), 0x80c5_6886);

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

        for (name, words) in [("f32", f32_words.as_slice()), ("f64", f64_words.as_slice())] {
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

        // Verify 21 FMOPA.S instructions in f32 kernel (4x + 2x + 1x unrolled * 3 tiles)
        // and 12 FMOPA.D instructions in f64 kernel (2x + 1x unrolled * 4 tiles).
        let f32_fmopas = f32_words.iter().filter(|&&w| (w & 0xffe0_0000) == 0x8080_0000).count();
        let f64_fmopas = f64_words.iter().filter(|&&w| (w & 0xffe0_0000) == 0x80c0_0000).count();
        assert_eq!(f32_fmopas, 21);
        assert_eq!(f64_fmopas, 12);
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
}
