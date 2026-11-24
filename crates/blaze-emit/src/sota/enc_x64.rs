//! Pure `#![no_std]` x86-64 System V AMD64 machine-code encoder for `TierName::Sota`.
//!
//! Hardware register numbers match `blaze_tier::regs::x86_64` (and Intel/AMD SDM Vol. 2):
//! `RAX=0, RCX=1, RDX=2, RBX=3, RSP=4, RBP=5, RSI=6, RDI=7, R8..R15=8..15`; `XMM0..XMM15=0..15`.

use core::alloc::Allocator;

use blaze_own::Vec;

pub(crate) type XReg = u8;

pub(crate) const RAX: XReg = 0;
pub(crate) const RCX: XReg = 1;
pub(crate) const RDX: XReg = 2;
pub(crate) const RBX: XReg = 3;
pub(crate) const RSP: XReg = 4;
pub(crate) const RBP: XReg = 5;
pub(crate) const RSI: XReg = 6;
pub(crate) const RDI: XReg = 7;
pub(crate) const R8: XReg = 8;
pub(crate) const R9: XReg = 9;
pub(crate) const R10: XReg = 10;
pub(crate) const R11: XReg = 11;
pub(crate) const R12: XReg = 12;
pub(crate) const R13: XReg = 13;
pub(crate) const R14: XReg = 14;
pub(crate) const R15: XReg = 15;

/// x86-64 condition codes (`cc` low nibble of `0F 8x` / `0F 4x` / `0F 9x`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Cc {
    O = 0x0,
    No = 0x1,
    B = 0x2,  // unsigned < (CF=1)
    Ae = 0x3, // unsigned >= (CF=0)
    E = 0x4,  // == (ZF=1)
    Ne = 0x5, // != (ZF=0)
    Be = 0x6, // unsigned <= (CF=1 || ZF=1)
    A = 0x7,  // unsigned > (CF=0 && ZF=0)
    P = 0xa,  // parity (unordered FP)
    Np = 0xb, // not parity (ordered FP)
    L = 0xc,  // signed <
    Ge = 0xd, // signed >=
    Le = 0xe, // signed <=
    G = 0xf,  // signed >
}

impl Cc {
    #[inline]
    pub(crate) const fn code(self) -> u8 {
        match self {
            Cc::O => 0x0,
            Cc::No => 0x1,
            Cc::B => 0x2,
            Cc::Ae => 0x3,
            Cc::E => 0x4,
            Cc::Ne => 0x5,
            Cc::Be => 0x6,
            Cc::A => 0x7,
            Cc::P => 0xa,
            Cc::Np => 0xb,
            Cc::L => 0xc,
            Cc::Ge => 0xd,
            Cc::Le => 0xe,
            Cc::G => 0xf,
        }
    }

    #[inline]
    pub(crate) const fn invert(self) -> Self {
        match self {
            Cc::O => Cc::No,
            Cc::No => Cc::O,
            Cc::B => Cc::Ae,
            Cc::Ae => Cc::B,
            Cc::E => Cc::Ne,
            Cc::Ne => Cc::E,
            Cc::Be => Cc::A,
            Cc::A => Cc::Be,
            Cc::P => Cc::Np,
            Cc::Np => Cc::P,
            Cc::L => Cc::Ge,
            Cc::Ge => Cc::L,
            Cc::Le => Cc::G,
            Cc::G => Cc::Le,
        }
    }
}

#[inline]
fn rex(w: bool, r: XReg, x: XReg, b: XReg) -> u8 {
    0x40 | (u8::from(w) << 3) | (((r >> 3) & 1) << 2) | (((x >> 3) & 1) << 1) | ((b >> 3) & 1)
}

#[inline]
fn emit_rex<A: Allocator>(buf: &mut Vec<u8, A>, w: bool, r: XReg, x: XReg, b: XReg) {
    let byte = rex(w, r, x, b);
    if byte != 0x40 {
        buf.push(byte);
    }
}

#[inline]
fn modrm(md: u8, reg: XReg, rm: XReg) -> u8 {
    (md << 6) | ((reg & 7) << 3) | (rm & 7)
}

#[inline]
fn sib(scale_log2: u8, idx: XReg, base: XReg) -> u8 {
    (scale_log2 << 6) | ((idx & 7) << 3) | (base & 7)
}

/// Emits ModRM + optional SIB + displacement for `[base + disp]`.
fn emit_mem_base_disp<A: Allocator>(buf: &mut Vec<u8, A>, reg: XReg, base: XReg, disp: i32) {
    let b_lo = base & 7;
    if disp == 0 && b_lo != 5 {
        buf.push(modrm(0, reg, base));
        if b_lo == 4 {
            buf.push(sib(0, 4, 4)); // RSP/R12 requires SIB
        }
    } else if let Ok(d8) = i8::try_from(disp) {
        buf.push(modrm(1, reg, base));
        if b_lo == 4 {
            buf.push(sib(0, 4, 4));
        }
        buf.push(d8.cast_unsigned());
    } else {
        buf.push(modrm(2, reg, base));
        if b_lo == 4 {
            buf.push(sib(0, 4, 4));
        }
        buf.extend_from_slice(&disp.to_le_bytes());
    }
}

/// Emits ModRM + SIB + displacement for `[base + idx*(1<<scale_log2) + disp]`.
/// `idx` MUST NOT be `RSP` (4).
fn emit_mem_sib<A: Allocator>(buf: &mut Vec<u8, A>, reg: XReg, base: XReg, idx: XReg, scale_log2: u8, disp: i32) {
    debug_assert_ne!(idx, RSP, "RSP cannot be SIB index register");
    let b_lo = base & 7;
    if disp == 0 && b_lo != 5 {
        buf.push(modrm(0, reg, 4));
        buf.push(sib(scale_log2, idx, base));
    } else if let Ok(d8) = i8::try_from(disp) {
        buf.push(modrm(1, reg, 4));
        buf.push(sib(scale_log2, idx, base));
        buf.push(d8.cast_unsigned());
    } else {
        buf.push(modrm(2, reg, 4));
        buf.push(sib(scale_log2, idx, base));
        buf.extend_from_slice(&disp.to_le_bytes());
    }
}

// --- Stack & Moves ---

pub(crate) fn push_r64<A: Allocator>(buf: &mut Vec<u8, A>, r: XReg) {
    if r >= 8 {
        buf.push(0x41);
    }
    buf.push(0x50 | (r & 7));
}

pub(crate) fn pop_r64<A: Allocator>(buf: &mut Vec<u8, A>, r: XReg) {
    if r >= 8 {
        buf.push(0x41);
    }
    buf.push(0x58 | (r & 7));
}

pub(crate) fn mov_rr<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, dst: XReg, src: XReg) {
    emit_rex(buf, w64, src, 0, dst);
    buf.push(0x89);
    buf.push(modrm(3, src, dst));
}

/// Optimal immediate materialization: `xor r32, r32` for 0, `mov r32, imm32` for u32,
/// sign-extended `mov r64, imm32` for i32, or `movabs r64, imm64`.
pub(crate) fn mov_imm64<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, val: u64) {
    if val == 0 {
        // `xor dst32, dst32` (2-3 bytes, zero-extends to 64-bit, breaks dependency chain!).
        emit_rex(buf, false, dst, 0, dst);
        buf.push(0x31);
        buf.push(modrm(3, dst, dst));
    } else if let Ok(v32) = u32::try_from(val) {
        // `mov dst32, imm32` (5-6 bytes, zero-extends to 64-bit!).
        emit_rex(buf, false, 0, 0, dst);
        buf.push(0xb8 | (dst & 7));
        buf.extend_from_slice(&v32.to_le_bytes());
    } else if let Ok(s32) = i32::try_from(val.cast_signed()) {
        // `mov dst64, sign_ext(imm32)` (7 bytes).
        buf.push(rex(true, 0, 0, dst));
        buf.push(0xc7);
        buf.push(modrm(3, 0, dst));
        buf.extend_from_slice(&s32.to_le_bytes());
    } else {
        // `movabs dst64, imm64` (10 bytes).
        buf.push(rex(true, 0, 0, dst));
        buf.push(0xb8 | (dst & 7));
        buf.extend_from_slice(&val.to_le_bytes());
    }
}

/// 3-operand `lea dst, [base + disp]` (`w64` = 64-bit or 32-bit zero-extending).
pub(crate) fn lea_base_disp<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, dst: XReg, base: XReg, disp: i32) {
    emit_rex(buf, w64, dst, 0, base);
    buf.push(0x8d);
    emit_mem_base_disp(buf, dst, base, disp);
}

/// 3-operand `lea dst, [base + idx*(1<<scale_log2) + disp]`: single-cycle `a + b*{1,2,4,8} + c`!
pub(crate) fn lea_sib<A: Allocator>(
    buf: &mut Vec<u8, A>,
    w64: bool,
    dst: XReg,
    base: XReg,
    idx: XReg,
    scale_log2: u8,
    disp: i32,
) {
    emit_rex(buf, w64, dst, idx, base);
    buf.push(0x8d);
    emit_mem_sib(buf, dst, base, idx, scale_log2, disp);
}

// --- ALU (Register-Register, Register-Imm, Unary, Shifts, Mul/Div) ---

/// Binary ALU reg-reg (`dst op= src`): `add=0x01, or=0x09, and=0x21, sub=0x29, xor=0x31, cmp=0x39`.
pub(crate) fn alu_rr<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, op: u8, dst: XReg, src: XReg) {
    emit_rex(buf, w64, src, 0, dst);
    buf.push(op);
    buf.push(modrm(3, src, dst));
}

/// Binary ALU reg-imm (`dst op= imm32`): `ext`: `add=0, or=1, and=4, sub=5, xor=6, cmp=7`.
pub(crate) fn alu_ri<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, ext: u8, dst: XReg, imm: i32) {
    emit_rex(buf, w64, 0, 0, dst);
    if let Ok(d8) = i8::try_from(imm) {
        buf.push(0x83);
        buf.push(modrm(3, ext, dst));
        buf.push(d8.cast_unsigned());
    } else {
        buf.push(0x81);
        buf.push(modrm(3, ext, dst));
        buf.extend_from_slice(&imm.to_le_bytes());
    }
}

pub(crate) fn test_rr<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, a: XReg, b: XReg) {
    emit_rex(buf, w64, b, 0, a);
    buf.push(0x85);
    buf.push(modrm(3, b, a));
}

pub(crate) fn test_ri<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, a: XReg, imm: u32) {
    emit_rex(buf, w64, 0, 0, a);
    buf.push(0xf7);
    buf.push(modrm(3, 0, a));
    buf.extend_from_slice(&imm.to_le_bytes());
}

pub(crate) fn neg_r<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, dst: XReg) {
    emit_rex(buf, w64, 0, 0, dst);
    buf.push(0xf7);
    buf.push(modrm(3, 3, dst));
}

pub(crate) fn not_r<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, dst: XReg) {
    emit_rex(buf, w64, 0, 0, dst);
    buf.push(0xf7);
    buf.push(modrm(3, 2, dst));
}

/// `imul dst, src` (2-operand signed/unsigned low-bits multiply).
pub(crate) fn imul_rr<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, dst: XReg, src: XReg) {
    emit_rex(buf, w64, dst, 0, src);
    buf.push(0x0f);
    buf.push(0xaf);
    buf.push(modrm(3, dst, src));
}

/// 3-operand `imul dst, src, imm32`!
pub(crate) fn imul_rri<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, dst: XReg, src: XReg, imm: i32) {
    emit_rex(buf, w64, dst, 0, src);
    if let Ok(d8) = i8::try_from(imm) {
        buf.push(0x6b);
        buf.push(modrm(3, dst, src));
        buf.push(d8.cast_unsigned());
    } else {
        buf.push(0x69);
        buf.push(modrm(3, dst, src));
        buf.extend_from_slice(&imm.to_le_bytes());
    }
}

/// `mul r64` (`RDX:RAX = RAX * src`, unsigned 128-bit product — powers magic-multiply `udiv` & `umulhi`!).
pub(crate) fn mul_r64<A: Allocator>(buf: &mut Vec<u8, A>, src: XReg) {
    buf.push(rex(true, 0, 0, src));
    buf.push(0xf7);
    buf.push(modrm(3, 4, src));
}

/// `imul r64` (`RDX:RAX = RAX * src`, signed 128-bit product — powers `smulhi`!).
pub(crate) fn imul_1op_r64<A: Allocator>(buf: &mut Vec<u8, A>, src: XReg) {
    buf.push(rex(true, 0, 0, src));
    buf.push(0xf7);
    buf.push(modrm(3, 5, src));
}

/// `div r64` (`RAX = RDX:RAX / src`, `RDX = RDX:RAX % src`, unsigned).
pub(crate) fn div_r64<A: Allocator>(buf: &mut Vec<u8, A>, src: XReg) {
    buf.push(rex(true, 0, 0, src));
    buf.push(0xf7);
    buf.push(modrm(3, 6, src));
}

/// `idiv r64` (`RAX = RDX:RAX / src`, `RDX = RDX:RAX % src`, signed).
pub(crate) fn idiv_r64<A: Allocator>(buf: &mut Vec<u8, A>, src: XReg) {
    buf.push(rex(true, 0, 0, src));
    buf.push(0xf7);
    buf.push(modrm(3, 7, src));
}

/// `cqo` (`RDX:RAX = sign_extend(RAX)`).
pub(crate) fn cqo<A: Allocator>(buf: &mut Vec<u8, A>) {
    buf.push(0x48);
    buf.push(0x99);
}

/// Shift/rotate by immediate `imm8`: `ext`: `rol=0, ror=1, shl=4, shr=5, sar=7`.
pub(crate) fn shift_ri<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, ext: u8, dst: XReg, imm: u8) {
    emit_rex(buf, w64, 0, 0, dst);
    if imm == 1 {
        buf.push(0xd1);
        buf.push(modrm(3, ext, dst));
    } else {
        buf.push(0xc1);
        buf.push(modrm(3, ext, dst));
        buf.push(imm);
    }
}

/// Shift/rotate by `CL`: `ext`: `rol=0, ror=1, shl=4, shr=5, sar=7`.
pub(crate) fn shift_cl<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, ext: u8, dst: XReg) {
    emit_rex(buf, w64, 0, 0, dst);
    buf.push(0xd3);
    buf.push(modrm(3, ext, dst));
}

/// `cmovcc dst, src` (conditional move: `if cc { dst = src }`).
pub(crate) fn cmovcc<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, cc: Cc, dst: XReg, src: XReg) {
    emit_rex(buf, w64, dst, 0, src);
    buf.push(0x0f);
    buf.push(0x40 | cc.code());
    buf.push(modrm(3, dst, src));
}

/// `setcc dst8` followed by zero-extending `movzx dst32, dst8`.
pub(crate) fn setcc_zext<A: Allocator>(buf: &mut Vec<u8, A>, cc: Cc, dst: XReg) {
    if dst >= 4 {
        buf.push(rex(false, 0, 0, dst));
    }
    buf.push(0x0f);
    buf.push(0x90 | cc.code());
    buf.push(modrm(3, 0, dst));
    movzx_b(buf, dst, dst);
}

pub(crate) fn movzx_b<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, src: XReg) {
    if dst >= 8 || src >= 4 {
        buf.push(rex(false, dst, 0, src));
    }
    buf.push(0x0f);
    buf.push(0xb6);
    buf.push(modrm(3, dst, src));
}

pub(crate) fn movzx_w<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, src: XReg) {
    emit_rex(buf, false, dst, 0, src);
    buf.push(0x0f);
    buf.push(0xb7);
    buf.push(modrm(3, dst, src));
}

pub(crate) fn movsx_b<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, src: XReg) {
    buf.push(rex(true, dst, 0, src));
    buf.push(0x0f);
    buf.push(0xbe);
    buf.push(modrm(3, dst, src));
}

pub(crate) fn movsx_w<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, src: XReg) {
    buf.push(rex(true, dst, 0, src));
    buf.push(0x0f);
    buf.push(0xbf);
    buf.push(modrm(3, dst, src));
}

pub(crate) fn movsxd<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, src: XReg) {
    buf.push(rex(true, dst, 0, src));
    buf.push(0x63);
    buf.push(modrm(3, dst, src));
}

pub(crate) fn bswap<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, dst: XReg) {
    emit_rex(buf, w64, 0, 0, dst);
    buf.push(0x0f);
    buf.push(0xc8 | (dst & 7));
}

/// `popcnt dst, src` (SSE4.2 / base on modern x86-64).
pub(crate) fn popcnt<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, dst: XReg, src: XReg) {
    buf.push(0xf3);
    emit_rex(buf, w64, dst, 0, src);
    buf.push(0x0f);
    buf.push(0xb8);
    buf.push(modrm(3, dst, src));
}

/// `lzcnt dst, src` (ABM / BMI1 / x86-64-v3).
pub(crate) fn lzcnt<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, dst: XReg, src: XReg) {
    buf.push(0xf3);
    emit_rex(buf, w64, dst, 0, src);
    buf.push(0x0f);
    buf.push(0xbd);
    buf.push(modrm(3, dst, src));
}

/// `tzcnt dst, src` (BMI1 / x86-64-v3).
pub(crate) fn tzcnt<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, dst: XReg, src: XReg) {
    buf.push(0xf3);
    emit_rex(buf, w64, dst, 0, src);
    buf.push(0x0f);
    buf.push(0xbc);
    buf.push(modrm(3, dst, src));
}

// --- Memory Loads & Stores ---

/// Load `[base + disp]` into `dst` (bytes: 1, 2, 4, or 8; zero-extended to 64-bit).
pub(crate) fn load_disp<A: Allocator>(buf: &mut Vec<u8, A>, bytes: u8, dst: XReg, base: XReg, disp: i32) {
    match bytes {
        1 => {
            emit_rex(buf, false, dst, 0, base);
            buf.push(0x0f);
            buf.push(0xb6);
            emit_mem_base_disp(buf, dst, base, disp);
        }
        2 => {
            emit_rex(buf, false, dst, 0, base);
            buf.push(0x0f);
            buf.push(0xb7);
            emit_mem_base_disp(buf, dst, base, disp);
        }
        4 => {
            emit_rex(buf, false, dst, 0, base);
            buf.push(0x8b);
            emit_mem_base_disp(buf, dst, base, disp);
        }
        _ => {
            buf.push(rex(true, dst, 0, base));
            buf.push(0x8b);
            emit_mem_base_disp(buf, dst, base, disp);
        }
    }
}

/// Store `src` into `[base + disp]` (bytes: 1, 2, 4, or 8).
pub(crate) fn store_disp<A: Allocator>(buf: &mut Vec<u8, A>, bytes: u8, src: XReg, base: XReg, disp: i32) {
    match bytes {
        1 => {
            if src >= 4 || base >= 8 {
                buf.push(rex(false, src, 0, base));
            }
            buf.push(0x88);
            emit_mem_base_disp(buf, src, base, disp);
        }
        2 => {
            buf.push(0x66);
            emit_rex(buf, false, src, 0, base);
            buf.push(0x89);
            emit_mem_base_disp(buf, src, base, disp);
        }
        4 => {
            emit_rex(buf, false, src, 0, base);
            buf.push(0x89);
            emit_mem_base_disp(buf, src, base, disp);
        }
        _ => {
            buf.push(rex(true, src, 0, base));
            buf.push(0x89);
            emit_mem_base_disp(buf, src, base, disp);
        }
    }
}

/// Load `[base + idx*(1<<scale_log2)]` into `dst` (SIB scaled addressing!).
pub(crate) fn load_sib<A: Allocator>(
    buf: &mut Vec<u8, A>,
    bytes: u8,
    dst: XReg,
    base: XReg,
    idx: XReg,
    scale_log2: u8,
) {
    match bytes {
        1 => {
            emit_rex(buf, false, dst, idx, base);
            buf.push(0x0f);
            buf.push(0xb6);
            emit_mem_sib(buf, dst, base, idx, scale_log2, 0);
        }
        2 => {
            emit_rex(buf, false, dst, idx, base);
            buf.push(0x0f);
            buf.push(0xb7);
            emit_mem_sib(buf, dst, base, idx, scale_log2, 0);
        }
        4 => {
            emit_rex(buf, false, dst, idx, base);
            buf.push(0x8b);
            emit_mem_sib(buf, dst, base, idx, scale_log2, 0);
        }
        _ => {
            buf.push(rex(true, dst, idx, base));
            buf.push(0x8b);
            emit_mem_sib(buf, dst, base, idx, scale_log2, 0);
        }
    }
}

/// Store `src` into `[base + idx*(1<<scale_log2)]`.
pub(crate) fn store_sib<A: Allocator>(
    buf: &mut Vec<u8, A>,
    bytes: u8,
    src: XReg,
    base: XReg,
    idx: XReg,
    scale_log2: u8,
) {
    match bytes {
        1 => {
            if src >= 4 || base >= 8 || idx >= 8 {
                buf.push(rex(false, src, idx, base));
            }
            buf.push(0x88);
            emit_mem_sib(buf, src, base, idx, scale_log2, 0);
        }
        2 => {
            buf.push(0x66);
            emit_rex(buf, false, src, idx, base);
            buf.push(0x89);
            emit_mem_sib(buf, src, base, idx, scale_log2, 0);
        }
        4 => {
            emit_rex(buf, false, src, idx, base);
            buf.push(0x89);
            emit_mem_sib(buf, src, base, idx, scale_log2, 0);
        }
        _ => {
            buf.push(rex(true, src, idx, base));
            buf.push(0x89);
            emit_mem_sib(buf, src, base, idx, scale_log2, 0);
        }
    }
}

/// `mfence` (SeqCst full memory barrier).
pub(crate) fn mfence<A: Allocator>(buf: &mut Vec<u8, A>) {
    buf.extend_from_slice(&[0x0f, 0xae, 0xf0]);
}

/// `lock xadd [ptr], val` (returns old value in `val`).
pub(crate) fn lock_xadd64<A: Allocator>(buf: &mut Vec<u8, A>, ptr: XReg, val: XReg) {
    buf.push(0xf0); // LOCK
    buf.push(rex(true, val, 0, ptr));
    buf.push(0x0f);
    buf.push(0xc1);
    emit_mem_base_disp(buf, val, ptr, 0);
}

/// `xchg [ptr], val` (implicit LOCK on x86-64; returns old value in `val`).
pub(crate) fn xchg64<A: Allocator>(buf: &mut Vec<u8, A>, ptr: XReg, val: XReg) {
    buf.push(rex(true, val, 0, ptr));
    buf.push(0x87);
    emit_mem_base_disp(buf, val, ptr, 0);
}

// --- Scalar SSE2 Floating-Point (`f32`/`f64`) ---

pub(crate) fn movfp_rr<A: Allocator>(buf: &mut Vec<u8, A>, double: bool, dst: XReg, src: XReg) {
    buf.push(if double { 0xf2 } else { 0xf3 });
    emit_rex(buf, false, dst, 0, src);
    buf.push(0x0f);
    buf.push(0x10);
    buf.push(modrm(3, dst, src));
}

pub(crate) fn load_fp_disp<A: Allocator>(buf: &mut Vec<u8, A>, double: bool, dst: XReg, base: XReg, disp: i32) {
    buf.push(if double { 0xf2 } else { 0xf3 });
    emit_rex(buf, false, dst, 0, base);
    buf.push(0x0f);
    buf.push(0x10);
    emit_mem_base_disp(buf, dst, base, disp);
}

pub(crate) fn store_fp_disp<A: Allocator>(buf: &mut Vec<u8, A>, double: bool, src: XReg, base: XReg, disp: i32) {
    buf.push(if double { 0xf2 } else { 0xf3 });
    emit_rex(buf, false, src, 0, base);
    buf.push(0x0f);
    buf.push(0x11);
    emit_mem_base_disp(buf, src, base, disp);
}

/// Scalar SSE2 binary op: `add=0x58, mul=0x59, sub=0x5c, div=0x5e`.
pub(crate) fn fp_bin<A: Allocator>(buf: &mut Vec<u8, A>, double: bool, op: u8, dst: XReg, src: XReg) {
    buf.push(if double { 0xf2 } else { 0xf3 });
    emit_rex(buf, false, dst, 0, src);
    buf.push(0x0f);
    buf.push(op);
    buf.push(modrm(3, dst, src));
}

pub(crate) fn ucomis<A: Allocator>(buf: &mut Vec<u8, A>, double: bool, a: XReg, b: XReg) {
    if double {
        buf.push(0x66);
    }
    emit_rex(buf, false, a, 0, b);
    buf.push(0x0f);
    buf.push(0x2e);
    buf.push(modrm(3, a, b));
}

/// `movq xmm, gpr` (or `movd` for `f32`).
pub(crate) fn mov_gpr_to_xmm<A: Allocator>(buf: &mut Vec<u8, A>, double: bool, dst_xmm: XReg, src_gpr: XReg) {
    buf.push(0x66);
    emit_rex(buf, double, dst_xmm, 0, src_gpr);
    buf.push(0x0f);
    buf.push(0x6e);
    buf.push(modrm(3, dst_xmm, src_gpr));
}

/// `movq gpr, xmm` (or `movd` for `f32`).
pub(crate) fn mov_xmm_to_gpr<A: Allocator>(buf: &mut Vec<u8, A>, double: bool, dst_gpr: XReg, src_xmm: XReg) {
    buf.push(0x66);
    emit_rex(buf, double, src_xmm, 0, dst_gpr);
    buf.push(0x0f);
    buf.push(0x7e);
    buf.push(modrm(3, src_xmm, dst_gpr));
}

// --- Control Flow, Calls, Relocations, Traps ---

/// `jmp rel32` (returns byte offset of the 4-byte displacement to patch).
pub(crate) fn jmp_rel32<A: Allocator>(buf: &mut Vec<u8, A>) -> usize {
    buf.push(0xe9);
    let at = buf.len();
    buf.extend_from_slice(&0i32.to_le_bytes());
    at
}

/// `jcc rel32` (returns byte offset of the 4-byte displacement to patch).
pub(crate) fn jcc_rel32<A: Allocator>(buf: &mut Vec<u8, A>, cc: Cc) -> usize {
    buf.push(0x0f);
    buf.push(0x80 | cc.code());
    let at = buf.len();
    buf.extend_from_slice(&0i32.to_le_bytes());
    at
}

/// `call rel32` (direct PC-relative call for release export; returns byte offset of disp32 for `RelocKind::Pc32`).
pub(crate) fn call_rel32<A: Allocator>(buf: &mut Vec<u8, A>) -> usize {
    buf.push(0xe8);
    let at = buf.len();
    buf.extend_from_slice(&0i32.to_le_bytes());
    at
}

/// `call [rip + disp32]` (GOT slot call for JIT; returns byte offset of disp32 for `RelocKind::GotPc32`).
pub(crate) fn call_rip_got<A: Allocator>(buf: &mut Vec<u8, A>) -> usize {
    buf.push(0xff);
    buf.push(0x15);
    let at = buf.len();
    buf.extend_from_slice(&0i32.to_le_bytes());
    at
}

/// `mov dst, [rip + disp32]` (GOT load; returns byte offset of disp32 for `RelocKind::GotPc32`).
pub(crate) fn mov_rip_got<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg) -> usize {
    buf.push(rex(true, dst, 0, 0));
    buf.push(0x8b);
    buf.push(modrm(0, dst, 5));
    let at = buf.len();
    buf.extend_from_slice(&0i32.to_le_bytes());
    at
}

/// `lea dst, [rip + disp32]` (PC-relative address; returns byte offset of disp32 for `RelocKind::Pc32`).
pub(crate) fn lea_rip_rel<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg) -> usize {
    buf.push(rex(true, dst, 0, 0));
    buf.push(0x8d);
    buf.push(modrm(0, dst, 5));
    let at = buf.len();
    buf.extend_from_slice(&0i32.to_le_bytes());
    at
}

pub(crate) fn call_r64<A: Allocator>(buf: &mut Vec<u8, A>, r: XReg) {
    emit_rex(buf, false, 0, 0, r);
    buf.push(0xff);
    buf.push(modrm(3, 2, r));
}

pub(crate) fn jmp_r64<A: Allocator>(buf: &mut Vec<u8, A>, r: XReg) {
    emit_rex(buf, false, 0, 0, r);
    buf.push(0xff);
    buf.push(modrm(3, 4, r));
}

pub(crate) fn ret<A: Allocator>(buf: &mut Vec<u8, A>) {
    buf.push(0xc3);
}

pub(crate) fn ud2<A: Allocator>(buf: &mut Vec<u8, A>) {
    buf.extend_from_slice(&[0x0f, 0x0b]);
}

/// Emits optimal multi-byte Intel/AMD NOP padding up to `n` bytes (for 16-byte loop header alignment).
pub(crate) fn emit_nops<A: Allocator>(buf: &mut Vec<u8, A>, mut n: usize) {
    const NOPS: [&[u8]; 9] = [
        &[],
        &[0x90],
        &[0x66, 0x90],
        &[0x0f, 0x1f, 0x00],
        &[0x0f, 0x1f, 0x40, 0x00],
        &[0x0f, 0x1f, 0x44, 0x00, 0x00],
        &[0x66, 0x0f, 0x1f, 0x44, 0x00, 0x00],
        &[0x0f, 0x1f, 0x80, 0x00, 0x00, 0x00, 0x00],
        &[0x0f, 0x1f, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
    ];
    while n > 0 {
        let k = n.min(8);
        if let Some(seq) = NOPS.get(k) {
            buf.extend_from_slice(seq);
        }
        n -= k;
    }
}

// -----------------------------------------------------------------------------
// 128-bit SSE2 Vector Encoders for `TierName::Sota` x86-64 Auto-Vectorization
// -----------------------------------------------------------------------------

/// `movdqu xmm, [base + disp]` (unaligned 128-bit vector load: `F3 [REX] 0F 6F /r`).
pub(crate) fn movdqu_load<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, base: XReg, disp: i32) {
    buf.push(0xf3);
    emit_rex(buf, false, dst, 0, base);
    buf.extend_from_slice(&[0x0f, 0x6f]);
    emit_mem_base_disp(buf, dst, base, disp);
}

/// `movdqu [base + disp], xmm` (unaligned 128-bit vector store: `F3 [REX] 0F 7F /r`).
pub(crate) fn movdqu_store<A: Allocator>(buf: &mut Vec<u8, A>, src: XReg, base: XReg, disp: i32) {
    buf.push(0xf3);
    emit_rex(buf, false, src, 0, base);
    buf.extend_from_slice(&[0x0f, 0x7f]);
    emit_mem_base_disp(buf, src, base, disp);
}

/// `movdqa dst, src` (128-bit XMM register-to-register copy: `66 [REX] 0F 6F /r`).
pub(crate) fn movdqa_rr<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, src: XReg) {
    buf.push(0x66);
    emit_rex(buf, false, dst, 0, src);
    buf.extend_from_slice(&[0x0f, 0x6f, modrm(3, dst, src)]);
}

/// `66 [REX] 0F <opc> /r`: packed integer SSE2 ALU (`paddb=FC`, `paddw=FD`, `paddd=FE`, `paddq=D4`,
/// `psubb=F8`, `psubw=F9`, `psubd=FA`, `psubq=FB`, `pand=DB`, `por=EB`, `pxor=EF`, `pmullw=D5`).
pub(crate) fn vec_int_bin<A: Allocator>(buf: &mut Vec<u8, A>, opc: u8, dst: XReg, src: XReg) {
    buf.push(0x66);
    emit_rex(buf, false, dst, 0, src);
    buf.extend_from_slice(&[0x0f, opc, modrm(3, dst, src)]);
}

/// Packed FP SSE/SSE2 ALU: `addps/pd=0x58`, `mulps/pd=0x59`, `subps/pd=0x5C`.
pub(crate) fn vec_fp_bin<A: Allocator>(buf: &mut Vec<u8, A>, double: bool, opc: u8, dst: XReg, src: XReg) {
    if double {
        buf.push(0x66);
    }
    emit_rex(buf, false, dst, 0, src);
    buf.extend_from_slice(&[0x0f, opc, modrm(3, dst, src)]);
}

/// `pshufd dst, src, imm8` (`66 [REX] 0F 70 /r ib`).
pub(crate) fn pshufd<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, src: XReg, imm8: u8) {
    buf.push(0x66);
    emit_rex(buf, false, dst, 0, src);
    buf.extend_from_slice(&[0x0f, 0x70, modrm(3, dst, src), imm8]);
}

/// `punpcklqdq dst, src` (`66 [REX] 0F 6C /r`): duplicates low 64-bit lane into both halves.
pub(crate) fn punpcklqdq<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, src: XReg) {
    buf.push(0x66);
    emit_rex(buf, false, dst, 0, src);
    buf.extend_from_slice(&[0x0f, 0x6c, modrm(3, dst, src)]);
}

/// `psrldq xmm, imm8` (`66 [REX] 0F 73 /3 ib`): logical right shift of 128-bit vector by `imm8` bytes.
pub(crate) fn psrldq<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, imm8: u8) {
    buf.push(0x66);
    emit_rex(buf, false, 0, 0, dst);
    buf.extend_from_slice(&[0x0f, 0x73, modrm(3, 3, dst), imm8]);
}

// -----------------------------------------------------------------------------
// x86-64 Native Target-CPU Intrinsics: VEX3, BMI2 (`mulx`/`shlx`/`shrx`/`sarx`),
// FMA3 (`vfmadd213sd`/`ss`/`pd`/`ps`), and AVX2 3-operand non-destructive SIMD!
// -----------------------------------------------------------------------------

#[inline]
fn emit_vex3<A: Allocator>(
    buf: &mut Vec<u8, A>,
    w: bool,
    r: XReg,
    x: XReg,
    b: XReg,
    mmmmm: u8,
    vvvv: XReg,
    l256: bool,
    pp: u8,
) {
    let b1 = (u8::from((r >> 3) == 0) << 7)
        | (u8::from((x >> 3) == 0) << 6)
        | (u8::from((b >> 3) == 0) << 5)
        | (mmmmm & 0x1f);
    let b2 = (u8::from(w) << 7) | (((!vvvv) & 0x0f) << 3) | (u8::from(l256) << 2) | (pp & 3);
    buf.extend_from_slice(&[0xc4, b1, b2]);
}

/// BMI2 `mulx hi, lo, src`: computes `(hi:lo) = rdx * src` without touching `rax` or `rflags`!
pub(crate) fn bmi2_mulx<A: Allocator>(buf: &mut Vec<u8, A>, hi: XReg, lo: XReg, src: XReg) {
    emit_vex3(buf, true, hi, 0, src, 2, lo, false, 3); // VEX.LZ.F2.0F38.W1 F6 /r
    buf.extend_from_slice(&[0xf6, modrm(3, hi, src)]);
}

/// BMI2 3-operand variable shift (`shlx`: pp=1, `shrx`: pp=3, `sarx`: pp=2) — ZERO `rcx` clobber!
pub(crate) fn bmi2_shiftx<A: Allocator>(buf: &mut Vec<u8, A>, w64: bool, pp: u8, dst: XReg, src: XReg, cnt: XReg) {
    emit_vex3(buf, w64, dst, 0, src, 2, cnt, false, pp); // VEX.LZ.{66,F2,F3}.0F38.W{0,1} F7 /r
    buf.extend_from_slice(&[0xf7, modrm(3, dst, src)]);
}

/// FMA3 `vfmadd213{sd,ss,pd,ps} dst, src1, src2`: `dst = dst * src1 + src2`!
pub(crate) fn fma3_vfmadd213<A: Allocator>(
    buf: &mut Vec<u8, A>,
    double: bool,
    packed: bool,
    dst: XReg,
    src1: XReg,
    src2: XReg,
) {
    fma3_vfmadd213_w(buf, false, double, packed, dst, src1, src2);
}

/// FMA3 `vfmadd213` supporting both 128-bit (`l256=false`) and 256-bit YMM (`l256=true`).
pub(crate) fn fma3_vfmadd213_w<A: Allocator>(
    buf: &mut Vec<u8, A>,
    l256: bool,
    double: bool,
    packed: bool,
    dst: XReg,
    src1: XReg,
    src2: XReg,
) {
    emit_vex3(buf, double, dst, 0, src2, 2, src1, l256, 1); // VEX.{128,256}.66.0F38.W{0,1} A8/A9 /r
    buf.extend_from_slice(&[if packed { 0xa8 } else { 0xa9 }, modrm(3, dst, src2)]);
}

/// FMA3 `vfmadd231{sd,ss,pd,ps} dst, src1, src2`: `dst = src1 * src2 + dst` (zero-copy accumulator)!
pub(crate) fn fma3_vfmadd231<A: Allocator>(
    buf: &mut Vec<u8, A>,
    double: bool,
    packed: bool,
    dst: XReg,
    src1: XReg,
    src2: XReg,
) {
    fma3_vfmadd231_w(buf, false, double, packed, dst, src1, src2);
}

/// FMA3 `vfmadd231` supporting both 128-bit (`l256=false`) and 256-bit YMM (`l256=true`).
pub(crate) fn fma3_vfmadd231_w<A: Allocator>(
    buf: &mut Vec<u8, A>,
    l256: bool,
    double: bool,
    packed: bool,
    dst: XReg,
    src1: XReg,
    src2: XReg,
) {
    emit_vex3(buf, double, dst, 0, src2, 2, src1, l256, 1); // VEX.{128,256}.66.0F38.W{0,1} B8/B9 /r
    buf.extend_from_slice(&[if packed { 0xb8 } else { 0xb9 }, modrm(3, dst, src2)]);
}

/// FMA3 `vfmadd231{sd,ss,pd,ps} dst, src1, [base + disp]`: `dst = src1 * mem + dst`!
pub(crate) fn fma3_vfmadd231_mem<A: Allocator>(
    buf: &mut Vec<u8, A>,
    double: bool,
    packed: bool,
    dst: XReg,
    src1: XReg,
    base: XReg,
    disp: i32,
) {
    fma3_vfmadd231_mem_w(buf, false, double, packed, dst, src1, base, disp);
}

/// FMA3 `vfmadd231` memory form supporting both 128-bit and 256-bit YMM.
pub(crate) fn fma3_vfmadd231_mem_w<A: Allocator>(
    buf: &mut Vec<u8, A>,
    l256: bool,
    double: bool,
    packed: bool,
    dst: XReg,
    src1: XReg,
    base: XReg,
    disp: i32,
) {
    emit_vex3(buf, double, dst, 0, base, 2, src1, l256, 1);
    buf.push(if packed { 0xb8 } else { 0xb9 });
    emit_mem_base_disp(buf, dst, base, disp);
}

/// AVX2 3-operand non-destructive 128-bit or 256-bit integer SIMD (`vpaddq`, `vpxor`, `vpcmpeqd`, `vpcmpgtd`, ...).
pub(crate) fn avx_vec_int_bin_w<A: Allocator>(
    buf: &mut Vec<u8, A>,
    l256: bool,
    opc: u8,
    dst: XReg,
    src1: XReg,
    src2: XReg,
) {
    emit_vex3(buf, false, dst, 0, src2, 1, src1, l256, 1); // VEX.{128,256}.66.0F.WIG <opc> /r
    buf.extend_from_slice(&[opc, modrm(3, dst, src2)]);
}

/// AVX2 3-operand `0F38` integer SIMD (`vpcmpeqq=0x29`, `vpcmpgtq=0x37`, `vpmulld=0x40`).
pub(crate) fn avx_vec_int_0f38<A: Allocator>(
    buf: &mut Vec<u8, A>,
    l256: bool,
    opc: u8,
    dst: XReg,
    src1: XReg,
    src2: XReg,
) {
    emit_vex3(buf, false, dst, 0, src2, 2, src1, l256, 1); // VEX.{128,256}.66.0F38.WIG <opc> /r
    buf.extend_from_slice(&[opc, modrm(3, dst, src2)]);
}

/// AVX 3-operand non-destructive 128-bit or 256-bit FP SIMD (`vaddpd/ps`, `vmulpd/ps`, `vsubpd/ps`).
pub(crate) fn avx_vec_fp_bin_w<A: Allocator>(
    buf: &mut Vec<u8, A>,
    l256: bool,
    double: bool,
    opc: u8,
    dst: XReg,
    src1: XReg,
    src2: XReg,
) {
    emit_vex3(buf, false, dst, 0, src2, 1, src1, l256, u8::from(double));
    buf.extend_from_slice(&[opc, modrm(3, dst, src2)]);
}

/// AVX `vmovdqu xmm/ymm, [base + disp]` (`VEX.{128,256}.F3.0F.WIG 6F /r`).
pub(crate) fn vmovdqu_load<A: Allocator>(buf: &mut Vec<u8, A>, l256: bool, dst: XReg, base: XReg, disp: i32) {
    emit_vex3(buf, false, dst, 0, base, 1, 0, l256, 2);
    buf.push(0x6f);
    emit_mem_base_disp(buf, dst, base, disp);
}

/// AVX `vmovdqu [base + disp], xmm/ymm` (`VEX.{128,256}.F3.0F.WIG 7F /r`).
pub(crate) fn vmovdqu_store<A: Allocator>(buf: &mut Vec<u8, A>, l256: bool, src: XReg, base: XReg, disp: i32) {
    emit_vex3(buf, false, src, 0, base, 1, 0, l256, 2);
    buf.push(0x7f);
    emit_mem_base_disp(buf, src, base, disp);
}

/// AVX `vmovdqa dst, src` (`VEX.{128,256}.66.0F.WIG 6F /r`).
pub(crate) fn vmovdqa_rr<A: Allocator>(buf: &mut Vec<u8, A>, l256: bool, dst: XReg, src: XReg) {
    emit_vex3(buf, false, dst, 0, src, 1, 0, l256, 1);
    buf.extend_from_slice(&[0x6f, modrm(3, dst, src)]);
}

/// AVX2 `vpbroadcast{b,w,d,q} dst, src_xmm` (`vsize`: `0:b, 1:w, 2:d, 3:q`).
pub(crate) fn avx2_vpbroadcast<A: Allocator>(buf: &mut Vec<u8, A>, l256: bool, vsize: u32, dst: XReg, src_xmm: XReg) {
    let opc = match vsize {
        0 => 0x78u8,
        1 => 0x79,
        2 => 0x58,
        _ => 0x59,
    };
    emit_vex3(buf, false, dst, 0, src_xmm, 2, 0, l256, 1); // VEX.{128,256}.66.0F38.W0 <opc> /r
    buf.extend_from_slice(&[opc, modrm(3, dst, src_xmm)]);
}

/// AVX2 `vextracti128 dst_xmm, src_ymm, imm8` (`VEX.256.66.0F3A.W0 39 /r ib`).
pub(crate) fn avx2_vextracti128<A: Allocator>(buf: &mut Vec<u8, A>, dst_xmm: XReg, src_ymm: XReg, imm8: u8) {
    emit_vex3(buf, false, src_ymm, 0, dst_xmm, 3, 0, true, 1);
    buf.extend_from_slice(&[0x39, modrm(3, src_ymm, dst_xmm), imm8]);
}

/// AVX2 4-operand non-destructive masked vector select `vpblendvb dst, src_false, src_true, mask`
/// (`VEX.{128,256}.66.0F3A.W0 4C /r /is4`).
pub(crate) fn avx2_vpblendvb<A: Allocator>(
    buf: &mut Vec<u8, A>,
    l256: bool,
    dst: XReg,
    src_false: XReg,
    src_true: XReg,
    mask: XReg,
) {
    emit_vex3(buf, false, dst, 0, src_true, 3, src_false, l256, 1);
    buf.extend_from_slice(&[0x4c, modrm(3, dst, src_true), (mask & 0x0f) << 4]);
}

/// AVX `vzeroupper` (`C5 F8 77`): clears upper YMM halves before returning/calling into SSE code.
pub(crate) fn vzeroupper<A: Allocator>(buf: &mut Vec<u8, A>) {
    buf.extend_from_slice(&[0xc5, 0xf8, 0x77]);
}

// -----------------------------------------------------------------------------
// x86-64 Carry-Chain (`adc`/`sbb`/`adcx`/`adox`/`mulx`), SSSE3/AVX2 `vpshufb` LUT,
// and 512-bit EVEX AVX-512 (`zmm`, 64 bytes/instruction = 256 B/iter 4x unrolled!)
// -----------------------------------------------------------------------------

/// `adc dst, src` (64-bit add with carry flag: `REX.W 11 /r`, `rm = dst, reg = src`).
#[allow(dead_code)]
pub(crate) fn adc_rr<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, src: XReg) {
    buf.push(rex(true, src, 0, dst));
    buf.extend_from_slice(&[0x11, modrm(3, src, dst)]);
}

/// `sbb dst, src` (64-bit subtract with borrow flag: `REX.W 19 /r`, `rm = dst, reg = src`).
#[allow(dead_code)]
pub(crate) fn sbb_rr<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, src: XReg) {
    buf.push(rex(true, src, 0, dst));
    buf.extend_from_slice(&[0x19, modrm(3, src, dst)]);
}

/// `adcx dst, src` (64-bit ADX dual carry-chain add using CF only: `66 REX.W 0F 38 F6 /r`).
#[allow(dead_code)]
pub(crate) fn adcx_rr<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, src: XReg) {
    buf.push(0x66);
    buf.push(rex(true, dst, 0, src));
    buf.extend_from_slice(&[0x0f, 0x38, 0xf6, modrm(3, dst, src)]);
}

/// `adox dst, src` (64-bit ADX dual carry-chain add using OF only: `F3 REX.W 0F 38 F6 /r`).
#[allow(dead_code)]
pub(crate) fn adox_rr<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, src: XReg) {
    buf.push(0xf3);
    buf.push(rex(true, dst, 0, src));
    buf.extend_from_slice(&[0x0f, 0x38, 0xf6, modrm(3, dst, src)]);
}

/// `mulx hi, lo, src` (BMI2 flagless 64x64->128 multiply `rdx * src -> hi:lo`: `VEX.LZ.F2.0F38.W1 F6 /r`).
#[allow(dead_code)]
pub(crate) fn mulx_r64<A: Allocator>(buf: &mut Vec<u8, A>, hi: XReg, lo: XReg, src: XReg) {
    emit_vex3(buf, true, hi, 0, src, 2, lo, false, 3);
    buf.extend_from_slice(&[0xf6, modrm(3, hi, src)]);
}

/// SSSE3 `pshufb dst_xmm, idx_xmm` (16-byte vectorized LUT lookup: `66 [REX] 0F 38 00 /r`).
#[allow(dead_code)]
pub(crate) fn pshufb_xmm<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, idx: XReg) {
    buf.push(0x66);
    emit_rex(buf, false, dst, 0, idx);
    buf.extend_from_slice(&[0x0f, 0x38, 0x00, modrm(3, dst, idx)]);
}

/// AVX2 `vpshufb dst, tbl, idx` (16B/32B non-destructive vectorized LUT: `VEX.{128,256}.66.0F38.W0 00 /r`).
#[allow(dead_code)]
pub(crate) fn avx2_vpshufb<A: Allocator>(buf: &mut Vec<u8, A>, l256: bool, dst: XReg, tbl: XReg, idx: XReg) {
    emit_vex3(buf, false, dst, 0, idx, 2, tbl, l256, 1);
    buf.extend_from_slice(&[0x00, modrm(3, dst, idx)]);
}

/// Emits a 4-byte EVEX prefix for 512-bit AVX-512 (`zmm0..zmm15`, `L'L = 10` = 512-bit, `opmask = 0`).
/// `mm`: `1 = 0F`, `2 = 0F38`, `3 = 0F3A`; `pp`: `0 = None`, `1 = 66`, `2 = F3`, `3 = F2`.
pub(crate) fn emit_evex512<A: Allocator>(
    buf: &mut Vec<u8, A>,
    w: bool,
    reg: XReg,
    idx: XReg,
    rm: XReg,
    mm: u8,
    vvvv: XReg,
    pp: u8,
) {
    let r = u8::from(reg < 8) << 7;
    let x = u8::from(idx < 8) << 6;
    let b = u8::from(rm < 8) << 5;
    let r_prime = 1u8 << 4; // reg < 16 => R' = 1
    let p0 = r | x | b | r_prime | (mm & 0x03);
    let p1 = (u8::from(w) << 7) | (((!vvvv) & 0x0f) << 3) | 0x04 | (pp & 0x03);
    // z=0, L'L=10 (512-bit = 0x40), b=0, V'=1 (0x08, vvvv < 16), aaa=0 (unmasked)
    let p2 = 0x48u8;
    buf.extend_from_slice(&[0x62, p0, p1, p2]);
}

/// AVX-512 `vmovdqu64 zmm, [base + disp]` (`EVEX.512.F3.0F.W1 6F /r`, 64-byte load!).
/// Note: EVEX disp8 is compressed (`disp8 * 64`), so `disp` scaled by 64 fits in 1 byte!
pub(crate) fn avx512_vmovdqu64_load<A: Allocator>(buf: &mut Vec<u8, A>, dst: XReg, base: XReg, disp: i32) {
    emit_evex512(buf, true, dst, 0, base, 1, 0, 2);
    buf.push(0x6f);
    if disp == 0 && (base & 7) != 5 {
        emit_mem_base_disp(buf, dst, base, 0);
    } else if disp % 64 == 0
        && let Ok(d8) = i8::try_from(disp / 64)
    {
        // Compressed EVEX disp8 * 64!
        let d8 = d8.cast_unsigned();
        if (base & 7) == 4 {
            buf.extend_from_slice(&[modrm(1, dst, 4), 0x24, d8]);
        } else {
            buf.extend_from_slice(&[modrm(1, dst, base), d8]);
        }
    } else {
        emit_mem_base_disp(buf, dst, base, disp);
    }
}

/// AVX-512 `vmovdqu64 [base + disp], zmm` (`EVEX.512.F3.0F.W1 7F /r`, 64-byte store!).
pub(crate) fn avx512_vmovdqu64_store<A: Allocator>(buf: &mut Vec<u8, A>, src: XReg, base: XReg, disp: i32) {
    emit_evex512(buf, true, src, 0, base, 1, 0, 2);
    buf.push(0x7f);
    if disp == 0 && (base & 7) != 5 {
        emit_mem_base_disp(buf, src, base, 0);
    } else if disp % 64 == 0
        && let Ok(d8) = i8::try_from(disp / 64)
    {
        let d8 = d8.cast_unsigned();
        if (base & 7) == 4 {
            buf.extend_from_slice(&[modrm(1, src, 4), 0x24, d8]);
        } else {
            buf.extend_from_slice(&[modrm(1, src, base), d8]);
        }
    } else {
        emit_mem_base_disp(buf, src, base, disp);
    }
}

/// AVX-512 512-bit integer ALU (`vpaddq`: `W=1, opc=0xD4`; `vpaddd`: `W=0, opc=0xFE`;
/// `vpxord`: `W=0, opc=0xEF`; `vpord`: `W=0, opc=0xEB`; `vpandd`: `W=0, opc=0xDB`).
pub(crate) fn avx512_int_bin<A: Allocator>(
    buf: &mut Vec<u8, A>,
    w64: bool,
    opc: u8,
    dst: XReg,
    src1: XReg,
    src2: XReg,
) {
    emit_evex512(buf, w64, dst, 0, src2, 1, src1, 1);
    buf.extend_from_slice(&[opc, modrm(3, dst, src2)]);
}

/// AVX-512 `vextracti64x4 ymm_dst, zmm_src, imm8` (`EVEX.512.66.0F3A.W1 3B /r ib`).
pub(crate) fn avx512_vextracti64x4<A: Allocator>(buf: &mut Vec<u8, A>, dst_ymm: XReg, src_zmm: XReg, imm8: u8) {
    emit_evex512(buf, true, src_zmm, 0, dst_ymm, 3, 0, 1);
    buf.extend_from_slice(&[0x3b, modrm(3, src_zmm, dst_ymm), imm8]);
}
