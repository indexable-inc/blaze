//! The x86-64 instructions the glue writes, encoded as Cranelift 0.136.2's assembler
//! (`cranelift-assembler-x64`, `mem.rs` and `rex.rs`) encodes them: legacy prefixes, then a
//! REX prefix only when a bit of it is set, ModRM with no displacement when it is 0 (but for
//! an `rbp`/`r13` base, which takes a disp8 of 0), a disp8 when it fits, and a SIB byte for an
//! `rsp`/`r12` base.

use crate::buf::Buf;

pub const RAX: u8 = 0;
pub const RCX: u8 = 1;
pub const RDX: u8 = 2;
pub const RBX: u8 = 3;
pub const RSP: u8 = 4;
pub const RBP: u8 = 5;
pub const RSI: u8 = 6;
pub const RDI: u8 = 7;
pub const R8: u8 = 8;
pub const R9: u8 = 9;
pub const R10: u8 = 10;
pub const R11: u8 = 11;
pub const R12: u8 = 12;
pub const R13: u8 = 13;
pub const R14: u8 = 14;
pub const R15: u8 = 15;

/// Condition codes, the low nibble of `jcc`/`cmovcc`.
pub const CC_AE: u8 = 0x3;
pub const CC_NZ: u8 = 0x5;

/// `[base + disp]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mem {
    pub base: u8,
    pub disp: i32,
}

pub const fn at(base: u8, disp: i32) -> Mem {
    Mem { base, disp }
}

/// The most bytes of one instruction with a `[base + disp32]` operand, a SIB byte, a legacy
/// prefix, a REX prefix and a two-byte opcode: what a bound counts per memory access.
pub const MEM_MAX: u32 = 1 + 1 + 2 + 1 + 1 + 4;
/// `movabs r64, imm64`.
pub const MOVABS: u32 = 10;

#[inline]
fn rex(buf: &mut Buf<'_>, w: bool, reg: u8, index: u8, base: u8) {
    let byte = 0x40 | (u8::from(w) << 3) | ((reg >> 3) << 2) | ((index >> 3) << 1) | (base >> 3);
    if byte != 0x40 {
        buf.put1(byte);
    }
}

#[inline]
const fn modrm(mode: u8, reg: u8, rm: u8) -> u8 {
    (mode << 6) | ((reg & 7) << 3) | (rm & 7)
}

#[inline]
fn mem(buf: &mut Buf<'_>, reg: u8, m: Mem) {
    let base = m.base & 7;
    let mode = if m.disp == 0 && base != 5 {
        0
    } else if i8::try_from(m.disp).is_ok() {
        1
    } else {
        2
    };
    buf.put1(modrm(mode, reg, base));
    if base == 4 {
        buf.put1(0x24);
    }
    match mode {
        1 => buf.put1(m.disp as u8),
        2 => buf.put4(m.disp as u32),
        _ => {}
    }
}

/// `prefix? REX opcode ModRM(reg, [m])`.
#[inline]
fn op_mem(buf: &mut Buf<'_>, prefix: Option<u8>, w: bool, opcode: &[u8], reg: u8, m: Mem) {
    if let Some(prefix) = prefix {
        buf.put1(prefix);
    }
    rex(buf, w, reg, 0, m.base);
    buf.put(opcode);
    mem(buf, reg, m);
}

/// `prefix? REX opcode ModRM(reg, rm)`, register to register.
#[inline]
fn op_rr(buf: &mut Buf<'_>, prefix: Option<u8>, w: bool, opcode: &[u8], reg: u8, rm: u8) {
    if let Some(prefix) = prefix {
        buf.put1(prefix);
    }
    rex(buf, w, reg, 0, rm);
    buf.put(opcode);
    buf.put1(modrm(3, reg, rm));
}

/// `prefix? REX opcode ModRM(reg, [rip + disp32])`; the offset of the disp32, which is
/// relative to the end of the instruction only when nothing follows it.
#[inline]
pub fn rip(buf: &mut Buf<'_>, prefix: Option<u8>, w: bool, opcode: &[u8], reg: u8) -> u32 {
    if let Some(prefix) = prefix {
        buf.put1(prefix);
    }
    rex(buf, w, reg, 0, 0);
    buf.put(opcode);
    buf.put1(modrm(0, reg, 5));
    let at = buf.len();
    buf.put4(0);
    at
}

/// `mov r64, [m]`.
pub fn load(buf: &mut Buf<'_>, dst: u8, m: Mem) {
    op_mem(buf, None, true, &[0x8b], dst, m);
}

/// `mov [m], r64`.
pub fn store(buf: &mut Buf<'_>, m: Mem, src: u8) {
    op_mem(buf, None, true, &[0x89], src, m);
}

/// A load of `bits` (8, 16, 32 or 64) extended to 64: `movzx`/`movsx`/`movsxd`, or `mov
/// r32` for 32 bits zero-extended.
pub fn load_ext(buf: &mut Buf<'_>, dst: u8, m: Mem, bits: u32, signed: bool) {
    match (bits, signed) {
        (8, false) => op_mem(buf, None, false, &[0x0f, 0xb6], dst, m),
        (8, true) => op_mem(buf, None, true, &[0x0f, 0xbe], dst, m),
        (16, false) => op_mem(buf, None, false, &[0x0f, 0xb7], dst, m),
        (16, true) => op_mem(buf, None, true, &[0x0f, 0xbf], dst, m),
        (32, false) => op_mem(buf, None, false, &[0x8b], dst, m),
        (32, true) => op_mem(buf, None, true, &[0x63], dst, m),
        _ => load(buf, dst, m),
    }
}

/// `lea r64, [m]`.
pub fn lea(buf: &mut Buf<'_>, dst: u8, m: Mem) {
    op_mem(buf, None, true, &[0x8d], dst, m);
}

/// `mov dst, src`, 64 bits.
pub fn mov(buf: &mut Buf<'_>, dst: u8, src: u8) {
    op_rr(buf, None, true, &[0x89], src, dst);
}

/// A constant into `dst` as Cranelift's `Inst::imm` writes it: `mov r32, imm32` when it
/// zero-extends from 32 bits, `mov r64, simm32` when it sign-extends, `movabs` otherwise.
pub fn imm(buf: &mut Buf<'_>, dst: u8, value: u64) {
    if u32::try_from(value).is_ok() {
        rex(buf, false, 0, 0, dst);
        buf.put1(0xb8 | (dst & 7));
        buf.put4(value as u32);
    } else if i32::try_from(value as i64).is_ok() {
        rex(buf, true, 0, 0, dst);
        buf.put1(0xc7);
        buf.put1(modrm(3, 0, dst));
        buf.put4(value as u32);
    } else {
        movabs(buf, dst, value);
    }
}

/// `movabs dst, imm64`; the offset of the immediate.
pub fn movabs(buf: &mut Buf<'_>, dst: u8, value: u64) -> u32 {
    rex(buf, true, 0, 0, dst);
    buf.put1(0xb8 | (dst & 7));
    let at = buf.len();
    buf.put8(value);
    at
}

/// The `/digit` of the group-1 ALU operations.
pub const ADD: u8 = 0;
pub const AND: u8 = 4;
pub const SUB: u8 = 5;
pub const CMP: u8 = 7;

/// `op r64, imm`: the imm8 form when it fits (`addq_mi_sxb`), else imm32.
pub fn alu_imm(buf: &mut Buf<'_>, op: u8, dst: u8, value: i32) {
    rex(buf, true, 0, 0, dst);
    if let Ok(byte) = i8::try_from(value) {
        buf.put1(0x83);
        buf.put1(modrm(3, op, dst));
        buf.put1(byte as u8);
    } else {
        buf.put1(0x81);
        buf.put1(modrm(3, op, dst));
        buf.put4(value as u32);
    }
}

/// `op r64, imm32`, always the imm32 form (`addq_mi_sxl`).
pub fn alu_imm32(buf: &mut Buf<'_>, op: u8, dst: u8, value: i32) {
    rex(buf, true, 0, 0, dst);
    buf.put1(0x81);
    buf.put1(modrm(3, op, dst));
    buf.put4(value as u32);
}

/// `add dst, src`, 64 bits (`addq_rm`).
pub fn add(buf: &mut Buf<'_>, dst: u8, src: u8) {
    op_rr(buf, None, true, &[0x03], dst, src);
}

/// `or dst, src`, 64 bits (`orq_rm`).
pub fn or(buf: &mut Buf<'_>, dst: u8, src: u8) {
    op_rr(buf, None, true, &[0x0b], dst, src);
}

/// `and dst, src`, 64 bits (`andq_rm`).
pub fn and(buf: &mut Buf<'_>, dst: u8, src: u8) {
    op_rr(buf, None, true, &[0x23], dst, src);
}

/// `sbb dst, src`, 64 bits (`sbbq_rm`).
pub fn sbb(buf: &mut Buf<'_>, dst: u8, src: u8) {
    op_rr(buf, None, true, &[0x1b], dst, src);
}

/// `shl dst, imm8`, 64 bits.
pub fn shl_imm(buf: &mut Buf<'_>, dst: u8, by: u8) {
    rex(buf, true, 0, 0, dst);
    buf.put1(0xc1);
    buf.put1(modrm(3, 4, dst));
    buf.put1(by);
}

/// `movzx dst32, byte [base + index]`; `base` is not `rbp` or `r13`.
pub fn load_u8_index(buf: &mut Buf<'_>, dst: u8, base: u8, index: u8) {
    debug_assert!(base & 7 != 5 && index != RSP);
    rex(buf, false, dst, index, base);
    buf.put(&[0x0f, 0xb6]);
    buf.put1(modrm(0, dst, 4));
    buf.put1(((index & 7) << 3) | (base & 7));
}

/// `cmp a, b`, 64 bits (`cmpq_rm`): the flags of `a - b`.
pub fn cmp(buf: &mut Buf<'_>, a: u8, b: u8) {
    op_rr(buf, None, true, &[0x3b], a, b);
}

/// `cmovcc dst, src`, 64 bits.
pub fn cmov(buf: &mut Buf<'_>, cc: u8, dst: u8, src: u8) {
    op_rr(buf, None, true, &[0x0f, 0x40 | cc], dst, src);
}

/// `movsxd dst, dword [base + index * 4]`; `base` is not `rbp` or `r13`.
pub fn movsxd_index4(buf: &mut Buf<'_>, dst: u8, base: u8, index: u8) {
    debug_assert!(base & 7 != 5 && index != RSP);
    rex(buf, true, dst, index, base);
    buf.put1(0x63);
    buf.put1(modrm(0, dst, 4));
    buf.put1((2 << 6) | ((index & 7) << 3) | (base & 7));
}

/// `mov dword [m], imm32`.
pub fn store_imm32(buf: &mut Buf<'_>, m: Mem, value: u32) {
    op_mem(buf, None, false, &[0xc7], 0, m);
    buf.put4(value);
}

/// `add qword [rip + disp32], 1`; the offset of the disp32, 5 bytes before the end.
pub fn add_rip_one(buf: &mut Buf<'_>) -> u32 {
    let at = rip(buf, None, true, &[0x83], ADD);
    buf.put1(1);
    at
}

pub fn push(buf: &mut Buf<'_>, reg: u8) {
    rex(buf, false, 0, 0, reg);
    buf.put1(0x50 | (reg & 7));
}

pub fn pop(buf: &mut Buf<'_>, reg: u8) {
    rex(buf, false, 0, 0, reg);
    buf.put1(0x58 | (reg & 7));
}

/// `call rel32`; the offset of the rel32.
pub fn call_rel32(buf: &mut Buf<'_>) -> u32 {
    buf.put1(0xe8);
    let at = buf.len();
    buf.put4(0);
    at
}

/// `call reg`.
pub fn call_reg(buf: &mut Buf<'_>, reg: u8) {
    rex(buf, false, 0, 0, reg);
    buf.put1(0xff);
    buf.put1(modrm(3, 2, reg));
}

/// `call qword [m]`.
pub fn call_mem(buf: &mut Buf<'_>, m: Mem) {
    op_mem(buf, None, false, &[0xff], 2, m);
}

/// `jmp reg`.
pub fn jmp_reg(buf: &mut Buf<'_>, reg: u8) {
    rex(buf, false, 0, 0, reg);
    buf.put1(0xff);
    buf.put1(modrm(3, 4, reg));
}

/// `jcc rel32`; the offset of the rel32.
pub fn jcc(buf: &mut Buf<'_>, cc: u8) -> u32 {
    buf.put(&[0x0f, 0x80 | cc]);
    let at = buf.len();
    buf.put4(0);
    at
}

pub fn ret(buf: &mut Buf<'_>) {
    buf.put1(0xc3);
}

/// `ret imm16`: returns, then pops `bytes` of stack arguments (`retq_i`).
pub fn ret_imm(buf: &mut Buf<'_>, bytes: u16) {
    buf.put1(0xc2);
    buf.put(&bytes.to_le_bytes());
}

/// `jmp rel32`; the offset of the rel32.
pub fn jmp_rel32(buf: &mut Buf<'_>) -> u32 {
    buf.put1(0xe9);
    let at = buf.len();
    buf.put4(0);
    at
}

pub fn ud2(buf: &mut Buf<'_>) {
    buf.put(&[0x0f, 0x0b]);
}

pub fn int3(buf: &mut Buf<'_>) {
    buf.put1(0xcc);
}

/// The SSE moves between an XMM register and memory: `(prefix, opcode)` of the load; the
/// store's opcode is the load's plus one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sse {
    /// `movss`, 4 bytes.
    Ss,
    /// `movsd`, 8 bytes.
    Sd,
    /// `movdqu`, 16 bytes.
    Dqu,
}

impl Sse {
    /// The move of a value of `bytes` in an XMM register.
    pub fn of(bytes: u32) -> Sse {
        match bytes {
            0..=4 => Sse::Ss,
            5..=8 => Sse::Sd,
            _ => Sse::Dqu,
        }
    }

    fn code(self) -> (u8, u8) {
        match self {
            Sse::Ss => (0xf3, 0x10),
            Sse::Sd => (0xf2, 0x10),
            Sse::Dqu => (0xf3, 0x6f),
        }
    }
}

/// `movss`/`movsd`/`movdqu xmm, [m]`.
pub fn xload(buf: &mut Buf<'_>, sse: Sse, xmm: u8, m: Mem) {
    let (prefix, opcode) = sse.code();
    op_mem(buf, Some(prefix), false, &[0x0f, opcode], xmm, m);
}

/// `movss`/`movsd`/`movdqu [m], xmm`.
pub fn xstore(buf: &mut Buf<'_>, sse: Sse, m: Mem, xmm: u8) {
    let (prefix, opcode) = sse.code();
    let opcode = if sse == Sse::Dqu { 0x7f } else { opcode + 1 };
    op_mem(buf, Some(prefix), false, &[0x0f, opcode], xmm, m);
}

/// `movq xmm, r64`.
pub fn movq_to_xmm(buf: &mut Buf<'_>, xmm: u8, src: u8) {
    op_rr(buf, Some(0x66), true, &[0x0f, 0x6e], xmm, src);
}
