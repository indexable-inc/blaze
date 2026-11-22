//! x86-64 templates in CANONICAL LONG FORMS (WAVE-BACKEND.md piece X; backend-next.md D100 point
//! 6): the part of the x86-64 table that needs no LLVM.
//!
//! WHY LONG FORMS. x86-64's short forms move a register's bits with the register: `rax` has
//! one-byte-shorter opcodes, `rsp`/`r12` as a base need a SIB byte, `rbp`/`r13` as a base need a
//! displacement, a small displacement or immediate takes one byte instead of four. A template that
//! the composer fills with ANY register needs every register field at fixed bits, so every
//! template here is written in the one form valid for all registers:
//! - a REX prefix on every instruction that names a register (Intel SDM Vol. 2A 2.2.1: `0100WRXB`,
//!   immediately before the opcode, after any legacy prefix such as `66`); with REX present a byte
//!   register 4..7 is `spl`/`bpl`/`sil`/`dil`, never `ah`..`bh` (SDM Vol. 1 3.4.1.1, Vol. 2A
//!   Table 3-1), so byte forms are register-agnostic too;
//! - no `rax`-only short opcode (`05 id` and the like) and no `B8+rd` except `movabs`, which has no
//!   other form;
//! - memory always as ModRM `mod = 10` (disp32 follows), `rm = 100` (a SIB byte follows), SIB
//!   `scale = 00`, `index = 100` with REX.X = 0 (no index: SDM Vol. 2A Table 2-3), `base` the
//!   register (REX.B its bit 3): valid for every base, `rsp`, `rbp`, `r12`, `r13` included;
//! - a displacement or an immediate always 32 bits (`81 /n id`, never `83 /n ib`), except the
//!   shift count, whose only form is `ib` (`C1 /n ib`), and `movabs`'s `io`.
//! So a register operand is two fields: its low three bits in ModRM.reg, ModRM.rm, SIB.base or
//! SIB.index (`from_bit` 0), and its bit 3 in REX.R, REX.B or REX.X (`from_bit` 3). A short
//! variant is added only where a measurement pays for it (D100 point 6).
//!
//! THE FIELDS ARE PLACED BY THE RULES ABOVE, not discovered: these encoders are written from the
//! manual for this table and each places its own fields as it writes the byte that holds them.
//! T1's bit-flip discovery serves an encoder that exists apart from the table (enc.rs); here the
//! tests decode every filled template with an independent decoder instead (x86_64_tests.rs).
//!
//! THREE-ADDRESS OVER TWO-ADDRESS. LIR's `r = a op b` is `mov t, a; op t, b; mov r, t` with `t`
//! the composer's temporary [`TEMP0`] (a scratch register, `r10` or `r11`): correct whichever of
//! `r`, `a`, `b` the composer makes equal. A shift by a register goes through `cl` (`D3 /n`, the
//! only form), so it clobbers `rcx`, which is never allocatable.
//!
//! FUSED COMPARE AND BRANCH (D100 point 6): `br` of an `icmp` with one use is `cmp a, b; jcc then`.
//! Its operands: 0 the condition (made by the inner `icmp`: [`Form::None`], since it is no
//! register), 1 the `then` block, then the inner operation's two operands. `br` has two targets
//! and a template four operands, so the `else` edge is the composer's: a fall-through when `else`
//! is the next block, else a `jump` template after this one. (T1 meets the same limit on arm64:
//! one shape for both is the integrator's question to settle.)
//!
//! WAITS FOR T2 (the LLVM generator; staging/blaze-llvm src/table_gen.rs): the templates no
//! encoder here writes (floats and vectors, division and remainder with their `#DE` cases,
//! conversions, `select`, atomics, bulk operations, the address-folding `ptr_add` + load/store
//! fusions whose index width the pattern does not carry), and the differential test of every
//! template both sources give (D99). T1's `generate` writes the table bytes from [`Built`] rows
//! (its `Arch::X86_64` arm: wiring/blaze-emit/x86-table-1.md); T1's `discover` is not used here:
//! its branch origin is an instruction's START with one instruction length, while x86's rel32
//! counts from the END of instructions of many lengths.
//!
//! OPERAND CONVENTIONS are T1's (table_gen/mod.rs's module documentation), with x86's classes:
//! offsets and constants `Imm(S32)`, `iconst_wide` `Imm(I64)`, shift amounts `Shift5`/`Shift6`.
//! A `Site` field is the rip-relative disp32 alone (x86 has no `adrp` pair): the site is at the
//! field's first byte and its displacement counts from the field's end, which ends the
//! instruction; blaze-code's `SiteKind` lengths describe arm64's sites, so the segment layout's
//! x86-64 sites (`GotPc32`) are blaze-code's to add before an x86-64 record is placed.

use core::fmt;

use blaze_code::record::SiteKind;
use blaze_lir::{IntCond, Op};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::regs::x86_64::X86_64_SYSV;
use blaze_tier::table::{Field, FieldKind, Form, ImmClass, Inner, Pattern, RESULT, TEMP0, Template};
use blaze_tier::{RegClass, RegSet};

/// The longest template here is 15 bytes (`icmp` against an imm32: 7 + 4 + 4); room for twice
/// that.
pub const MAX_BYTES: usize = 32;
/// The most fields a template here has is 12 (`mov; op; mov` of two registers each); room for a
/// little more.
pub const MAX_FIELDS: usize = 16;

/// The registers a template using [`TEMP0`] may write besides its result: the scratch set the
/// composer takes the temporary from.
const TEMPS: RegSet = X86_64_SYSV.scratch;
/// `rcx`, the only shift-count register (`D3 /n` shifts by `cl`).
const RCX_HW: u8 = 1;
/// `rsp`, the base of a frame field.
const RSP_HW: u8 = 4;
/// What a call clobbers under System V: every general register but the callee-saved ones and
/// `rsp`, and every `xmm` register.
const CALL_CLOBBERS: RegSet = RegSet(0x0000_ffff_0000_ffff).minus(X86_64_SYSV.callee_saved).without(X86_64_SYSV.stack);

/// One template as the generator built it, fixed-size so building allocates only the list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Built {
    pub pattern: Pattern,
    bytes: [u8; MAX_BYTES],
    len: u8,
    fields: [Field; MAX_FIELDS],
    count: u8,
    pub clobbers: RegSet,
}

impl Built {
    /// The template's bytes, every field zero.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// Its fields in the order the bytes hold them.
    pub fn fields(&self) -> &[Field] {
        &self.fields[..usize::from(self.count)]
    }

    /// As the composer reads it.
    pub fn template(&self) -> Template<'_> {
        Template { pattern: self.pattern, bytes: self.bytes(), fields: self.fields(), clobbers: self.clobbers }
    }
}

/// Why the x86-64 templates could not be built: each names the pattern.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum X86Error {
    /// A template is past [`MAX_BYTES`] or [`MAX_FIELDS`].
    TooLong { pattern: Pattern, what: &'static str },
    /// Two templates for one pattern.
    Repeated { pattern: Pattern },
}

impl fmt::Display for X86Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            X86Error::TooLong { pattern, what } => write!(
                f,
                "the x86-64 template for `{}` at width {} variant {} is past its {what}",
                pattern.op.name(),
                pattern.width,
                pattern.variant
            ),
            X86Error::Repeated { pattern } => write!(
                f,
                "two x86-64 templates for `{}` at width {} variant {}",
                pattern.op.name(),
                pattern.width,
                pattern.variant
            ),
        }
    }
}

impl core::error::Error for X86Error {}

/// Every x86-64 template this module writes, sorted by [`Pattern`] (the table's order), each
/// pattern once.
pub fn templates<'a>(alloc: Alloc<'a>) -> Result<AVec<'a, Built>, X86Error> {
    let mut out: AVec<'a, Built> = Vec::new_in(alloc);
    for width in [32u8, 64] {
        let w = width == 64;
        // `r = a op b`, reg-reg, MR form (`op r/m, r`: SDM "Op/En MR", ModRM.rm the destination,
        // ModRM.reg the source): ADD 01 /r, OR 09 /r, AND 21 /r, SUB 29 /r, XOR 31 /r.
        for (op, opcode) in [(Op::Add, 0x01u8), (Op::Or, 0x09), (Op::And, 0x21), (Op::Sub, 0x29), (Op::Xor, 0x31)] {
            let mut a = Asm::new(pattern(op, width, 0, gpr(), [gpr(), gpr(), Form::None, Form::None]));
            mov_rr(&mut a, w, Slot::Op(TEMP0), Slot::Op(0))?;
            mr(&mut a, w, &[opcode], Slot::Op(TEMP0), Slot::Op(1))?;
            mov_rr(&mut a, w, Slot::Op(RESULT), Slot::Op(TEMP0))?;
            out.push(a.done(TEMPS));
        }
        // `mul`: IMUL r, r/m is 0F AF /r, RM form (ModRM.reg the destination, ModRM.rm the source);
        // the low half of the product is the same signed or unsigned.
        {
            let mut a = Asm::new(pattern(Op::Mul, width, 0, gpr(), [gpr(), gpr(), Form::None, Form::None]));
            mov_rr(&mut a, w, Slot::Op(TEMP0), Slot::Op(0))?;
            rm(&mut a, w, &[0x0f, 0xaf], Slot::Op(TEMP0), Slot::Op(1))?;
            mov_rr(&mut a, w, Slot::Op(RESULT), Slot::Op(TEMP0))?;
            out.push(a.done(TEMPS));
        }
        // `r = a op imm`: 81 /n id (op r/m, imm32; with REX.W the imm32 is sign-extended to 64
        // bits): ADD /0, OR /1, AND /4, SUB /5, XOR /6.
        for (op, ext) in [(Op::Add, 0u8), (Op::Or, 1), (Op::And, 4), (Op::Sub, 5), (Op::Xor, 6)] {
            let mut a = Asm::new(pattern(op, width, 0, gpr(), [gpr(), Form::Imm(ImmClass::S32), Form::None, Form::None]));
            mov_rr(&mut a, w, Slot::Op(TEMP0), Slot::Op(0))?;
            ext_r(&mut a, w, &[0x81], ext, Slot::Op(TEMP0))?;
            a.zeros(4, 1, FieldKind::Imm)?;
            mov_rr(&mut a, w, Slot::Op(RESULT), Slot::Op(TEMP0))?;
            out.push(a.done(TEMPS));
        }
        // Shifts and rotates by an immediate: C1 /n ib (ROL /0, ROR /1, SHL /4, SHR /5, SAR /7);
        // the count is one byte, masked by the processor to 5 bits (32) or 6 bits (REX.W).
        let count = if w { ImmClass::Shift6 } else { ImmClass::Shift5 };
        for (op, ext) in [(Op::Rotl, 0u8), (Op::Rotr, 1), (Op::Shl, 4), (Op::Lshr, 5), (Op::Ashr, 7)] {
            let mut a = Asm::new(pattern(op, width, 0, gpr(), [gpr(), Form::Imm(count), Form::None, Form::None]));
            mov_rr(&mut a, w, Slot::Op(TEMP0), Slot::Op(0))?;
            ext_r(&mut a, w, &[0xc1], ext, Slot::Op(TEMP0))?;
            a.zeros(1, 1, FieldKind::Imm)?;
            mov_rr(&mut a, w, Slot::Op(RESULT), Slot::Op(TEMP0))?;
            out.push(a.done(TEMPS));
        }
        // The same by a register: D3 /n shifts r/m by `cl` (masked as above): `mov rcx, b` first.
        for (op, ext) in [(Op::Rotl, 0u8), (Op::Rotr, 1), (Op::Shl, 4), (Op::Lshr, 5), (Op::Ashr, 7)] {
            let mut a = Asm::new(pattern(op, width, 0, gpr(), [gpr(), gpr(), Form::None, Form::None]));
            mov_rr(&mut a, w, Slot::Op(TEMP0), Slot::Op(0))?;
            mov_rr(&mut a, true, Slot::Hw(RCX_HW), Slot::Op(1))?;
            ext_r(&mut a, w, &[0xd3], ext, Slot::Op(TEMP0))?;
            mov_rr(&mut a, w, Slot::Op(RESULT), Slot::Op(TEMP0))?;
            out.push(a.done(TEMPS.with(blaze_tier::Reg(RCX_HW))));
        }
        // `neg` (F7 /3) and `not` (F7 /2), one operand.
        for (op, ext) in [(Op::Not, 2u8), (Op::Neg, 3)] {
            let mut a = Asm::new(pattern(op, width, 0, gpr(), [gpr(), Form::None, Form::None, Form::None]));
            mov_rr(&mut a, w, Slot::Op(TEMP0), Slot::Op(0))?;
            ext_r(&mut a, w, &[0xf7], ext, Slot::Op(TEMP0))?;
            mov_rr(&mut a, w, Slot::Op(RESULT), Slot::Op(TEMP0))?;
            out.push(a.done(TEMPS));
        }
        for cond in IntCond::ALL {
            let variant = cond as u8;
            let cc = condition_code(cond);
            // `r = icmp cond a, b`: CMP r/m, r is 39 /r (flags of r/m - r: ModRM.rm = a,
            // ModRM.reg = b); SETcc r/m8 is 0F 90+cc /0 (REX present: any of the 16 low bytes);
            // MOVZX r64, r/m8 is REX.W 0F B6 /r (RM form).
            let mut a = Asm::new(pattern(Op::Icmp, width, variant, gpr(), [gpr(), gpr(), Form::None, Form::None]));
            mr(&mut a, w, &[0x39], Slot::Op(0), Slot::Op(1))?;
            setcc_movzx(&mut a, cc)?;
            out.push(a.done(TEMPS));
            // Against an immediate: CMP r/m, imm32 is 81 /7 id.
            let mut a = Asm::new(pattern(Op::Icmp, width, variant, gpr(), [gpr(), Form::Imm(ImmClass::S32), Form::None, Form::None]));
            ext_r(&mut a, w, &[0x81], 7, Slot::Op(0))?;
            a.zeros(4, 1, FieldKind::Imm)?;
            setcc_movzx(&mut a, cc)?;
            out.push(a.done(TEMPS));
            // Fused `br (icmp cond a, b), then`: the compare, then Jcc rel32 (0F 80+cc cd), the
            // displacement from the END of the Jcc (SDM: "relative to the next instruction").
            let inner = Some(Inner { op: Op::Icmp, variant, operand: 0 });
            let mut a = Asm::new(Pattern {
                op: Op::Br,
                width,
                variant: 0,
                result: Form::None,
                operands: [Form::None, Form::Block, gpr(), gpr()],
                inner,
            });
            mr(&mut a, w, &[0x39], Slot::Op(2), Slot::Op(3))?;
            jcc(&mut a, cc, 1)?;
            out.push(a.done(RegSet::EMPTY));
            let mut a = Asm::new(Pattern {
                op: Op::Br,
                width,
                variant: 0,
                result: Form::None,
                operands: [Form::None, Form::Block, gpr(), Form::Imm(ImmClass::S32)],
                inner,
            });
            ext_r(&mut a, w, &[0x81], 7, Slot::Op(2))?;
            a.zeros(4, 3, FieldKind::Imm)?;
            jcc(&mut a, cc, 1)?;
            out.push(a.done(RegSet::EMPTY));
        }
        // `r = iconst`: REX.W C7 /0 id (sign-extended imm32) at 64; at 32, C7 /0 id writes the
        // 32 bits and zeroes bits 32..64 (SDM Vol. 1 3.4.1.1: a 32-bit result is zero-extended).
        {
            let mut a = Asm::new(pattern(Op::Iconst, width, 0, gpr(), [Form::Imm(ImmClass::S32), Form::None, Form::None, Form::None]));
            ext_r(&mut a, w, &[0xc7], 0, Slot::Op(RESULT))?;
            a.zeros(4, 0, FieldKind::Imm)?;
            out.push(a.done(RegSet::EMPTY));
        }
    }
    // `iconst` of 1, 8 and 16 bits: the 32-bit form (bits above the width zero, as LIR holds
    // them: op.rs `Iconst`).
    for width in [1u8, 8, 16] {
        let mut a = Asm::new(pattern(Op::Iconst, width, 0, gpr(), [Form::Imm(ImmClass::S32), Form::None, Form::None, Form::None]));
        ext_r(&mut a, false, &[0xc7], 0, Slot::Op(RESULT))?;
        a.zeros(4, 0, FieldKind::Imm)?;
        out.push(a.done(RegSet::EMPTY));
    }
    // `iconst_wide`: MOV r64, imm64 is REX.W B8+rd io: the register's low bits in the opcode's
    // low three bits, its bit 3 in REX.B (the one form whose register is in the opcode byte).
    {
        let mut a = Asm::new(pattern(Op::IconstWide, 64, 0, gpr(), [Form::Imm(ImmClass::I64), Form::None, Form::None, Form::None]));
        a.rex(true, Slot::Ext(0), Slot::Ext(0), Slot::Op(RESULT))?;
        let at = a.len * 8;
        let low = a.low3(Slot::Op(RESULT), at)?;
        a.byte(0xb8 | low)?;
        a.zeros(8, 0, FieldKind::Imm)?;
        out.push(a.done(RegSet::EMPTY));
    }
    // Loads `r = load a, offset`: operand 0 the address, operand 1 the offset (disp32).
    // 8 and 16 bits: MOVZX r64, r/m8 (REX.W 0F B6 /r) and r/m16 (REX.W 0F B7 /r); 32 bits: MOV
    // r32, r/m32 (8B /r, zero-extended); 64 bits: REX.W 8B /r.
    for (width, w, opcode) in [(8u8, true, &[0x0fu8, 0xb6][..]), (16, true, &[0x0f, 0xb7][..]), (32, false, &[0x8b][..]), (64, true, &[0x8b][..])] {
        let mut a = Asm::new(pattern(Op::Load, width, 0, gpr(), [gpr(), Form::Imm(ImmClass::S32), Form::None, Form::None]));
        mem(&mut a, w, false, opcode, Slot::Op(RESULT), Slot::Op(0), 1, FieldKind::Imm)?;
        out.push(a.done(RegSet::EMPTY));
        // Fused with its `slot_addr` (T1's convention, table_gen/mod.rs): `[None, None, Slot,
        // None]`, the slot's field a frame field of the slot's offset plus the access's, in bytes
        // (`scale` 0: x86's disp32 counts bytes); the base is rsp: SIB.base 100 with REX.B 0.
        let mut a = Asm::new(Pattern {
            op: Op::Load,
            width,
            variant: 0,
            result: gpr(),
            operands: [Form::None, Form::None, Form::Slot, Form::None],
            inner: Some(Inner { op: Op::SlotAddr, variant: 0, operand: 0 }),
        });
        mem(&mut a, w, false, opcode, Slot::Op(RESULT), Slot::Hw(RSP_HW), 2, FieldKind::Frame { scale: 0 })?;
        out.push(a.done(RegSet::EMPTY));
    }
    // Stores `store v, a, offset`: operand 0 the value, 1 the address, 2 the offset. 8 bits: MOV
    // r/m8, r8 is 88 /r (REX present: sil/dil/spl/bpl, never ah..bh); 16 bits: 66 then REX then
    // 89 /r (the operand-size prefix BEFORE REX: SDM Vol. 2A 2.2.1); 32: 89 /r; 64: REX.W 89 /r.
    for (width, w, p66, opcode) in [(8u8, false, false, 0x88u8), (16, false, true, 0x89), (32, false, false, 0x89), (64, true, false, 0x89)] {
        let mut a = Asm::new(Pattern {
            op: Op::Store,
            width,
            variant: 0,
            result: Form::None,
            operands: [gpr(), gpr(), Form::Imm(ImmClass::S32), Form::None],
            inner: None,
        });
        mem(&mut a, w, p66, &[opcode], Slot::Op(0), Slot::Op(1), 2, FieldKind::Imm)?;
        out.push(a.done(RegSet::EMPTY));
        // Fused with its `slot_addr`: `[value, None, None, Slot]`, as the load above.
        let mut a = Asm::new(Pattern {
            op: Op::Store,
            width,
            variant: 0,
            result: Form::None,
            operands: [gpr(), Form::None, Form::None, Form::Slot],
            inner: Some(Inner { op: Op::SlotAddr, variant: 0, operand: 1 }),
        });
        mem(&mut a, w, p66, &[opcode], Slot::Op(0), Slot::Hw(RSP_HW), 3, FieldKind::Frame { scale: 0 })?;
        out.push(a.done(RegSet::EMPTY));
    }
    // `br v, then` on a boolean register: TEST r/m8, r8 (84 /r) with itself, then JNE rel32
    // (0F 85 cd). A boolean is 0 or 1 in its low byte (the `icmp` templates' MOVZX makes it so).
    {
        let mut a = Asm::new(Pattern {
            op: Op::Br,
            width: 1,
            variant: 0,
            result: Form::None,
            operands: [gpr(), Form::Block, Form::None, Form::None],
            inner: None,
        });
        mr(&mut a, false, &[0x84], Slot::Op(0), Slot::Op(0))?;
        jcc(&mut a, 0x5, 1)?;
        out.push(a.done(RegSet::EMPTY));
    }
    // `jump`: JMP rel32 is E9 cd, from the end of the instruction; no register, so no REX.
    {
        let mut a = Asm::new(Pattern {
            op: Op::Jump,
            width: 0,
            variant: 0,
            result: Form::None,
            operands: [Form::Block, Form::None, Form::None, Form::None],
            inner: None,
        });
        a.byte(0xe9)?;
        a.rel32(0)?;
        out.push(a.done(RegSet::EMPTY));
    }
    // `call` through the callee's import word: CALL r/m64 is FF /2; ModRM `mod = 00, rm = 101`
    // in 64-bit mode is [rip + disp32] (SDM Vol. 2A 2.2.1.6), from the END of the instruction,
    // which this field ends: the site's relocation is x86-64 `GotPc32` with addend -4 when the
    // field's own offset is the base. REX (W ignored by FF /2 in 64-bit mode; kept 0) for the
    // canonical form.
    {
        let mut a = Asm::new(Pattern {
            op: Op::Call,
            width: 0,
            variant: 0,
            result: Form::None,
            operands: [Form::Site(SiteKind::Call), Form::None, Form::None, Form::None],
            inner: None,
        });
        a.rex(false, Slot::Ext(0), Slot::Ext(0), Slot::Ext(0))?;
        a.byte(0xff)?;
        a.modrm(0b00, Slot::Ext(2), Slot::Ext(0b101))?;
        a.zeros(4, 0, FieldKind::Site(SiteKind::Call))?;
        out.push(a.done(CALL_CLOBBERS));
        // `call_indirect f`: FF /2 with ModRM `mod = 11`, rm the register.
        let mut a = Asm::new(Pattern {
            op: Op::CallIndirect,
            width: 0,
            variant: 0,
            result: Form::None,
            operands: [gpr(), Form::None, Form::None, Form::None],
            inner: None,
        });
        ext_r(&mut a, false, &[0xff], 2, Slot::Op(0))?;
        out.push(a.done(CALL_CLOBBERS));
    }
    // A function's or a static's address from its import word: MOV r64, [rip + disp32] is
    // REX.W 8B /r with ModRM `mod = 00, rm = 101` (REX.B is not consulted for this form).
    for op in [Op::FuncAddr, Op::DataAddr] {
        let mut a = Asm::new(pattern(op, 64, 0, gpr(), [Form::Site(SiteKind::Load), Form::None, Form::None, Form::None]));
        a.rex(true, Slot::Op(RESULT), Slot::Ext(0), Slot::Ext(0))?;
        a.byte(0x8b)?;
        a.modrm(0b00, Slot::Op(RESULT), Slot::Ext(0b101))?;
        a.zeros(4, 0, FieldKind::Site(SiteKind::Load))?;
        out.push(a.done(RegSet::EMPTY));
    }
    // `unreachable`: UD2 is 0F 0B (no register, so no REX).
    {
        let mut a = Asm::new(Pattern {
            op: Op::Unreachable,
            width: 0,
            variant: 0,
            result: Form::None,
            operands: [Form::None; 4],
            inner: None,
        });
        a.byte(0x0f)?;
        a.byte(0x0b)?;
        out.push(a.done(RegSet::EMPTY));
    }
    out.sort_unstable_by(|x, y| x.pattern.cmp(&y.pattern));
    for pair in out.windows(2) {
        if pair[0].pattern == pair[1].pattern {
            return Err(X86Error::Repeated { pattern: pair[0].pattern });
        }
    }
    Ok(out)
}

/// The `cc` nibble of Jcc and SETcc for each LIR condition (SDM Vol. 2A, Jcc's table: E 4, NE 5,
/// B 2, BE 6, A 7, AE 3, L C, LE E, G F, GE D).
pub const fn condition_code(cond: IntCond) -> u8 {
    match cond {
        IntCond::Eq => 0x4,
        IntCond::Ne => 0x5,
        IntCond::Ult => 0x2,
        IntCond::Ule => 0x6,
        IntCond::Ugt => 0x7,
        IntCond::Uge => 0x3,
        IntCond::Slt => 0xc,
        IntCond::Sle => 0xe,
        IntCond::Sgt => 0xf,
        IntCond::Sge => 0xd,
    }
}

const fn gpr() -> Form {
    Form::Reg(RegClass::Gpr)
}

const fn pattern(op: Op, width: u8, variant: u8, result: Form, operands: [Form; 4]) -> Pattern {
    Pattern { op, width, variant, result, operands, inner: None }
}

/// Where an encoding's register bits come from.
#[derive(Clone, Copy)]
enum Slot {
    /// A register operand by its operand number: the bits are fields.
    Op(u8),
    /// A fixed register by its hardware number 0..16.
    Hw(u8),
    /// A fixed three-bit code: an opcode extension `/n` in ModRM.reg, or ModRM.rm's 100 (a SIB
    /// follows) and 101 (rip-relative), SIB.index's 100 (none). Its REX bit is 0.
    Ext(u8),
}

const NO_FIELD: Field = Field { at: 0, width: 0, operand: 0, from_bit: 0, kind: FieldKind::Imm };

/// A template being written: its bytes and the fields placed as the bytes are written.
struct Asm {
    pattern: Pattern,
    bytes: [u8; MAX_BYTES],
    len: usize,
    fields: [Field; MAX_FIELDS],
    count: usize,
}

impl Asm {
    fn new(pattern: Pattern) -> Asm {
        Asm { pattern, bytes: [0; MAX_BYTES], len: 0, fields: [NO_FIELD; MAX_FIELDS], count: 0 }
    }

    fn done(&self, clobbers: RegSet) -> Built {
        Built {
            pattern: self.pattern,
            bytes: self.bytes,
            len: self.len as u8,
            fields: self.fields,
            count: self.count as u8,
            clobbers,
        }
    }

    fn byte(&mut self, byte: u8) -> Result<(), X86Error> {
        let Some(slot) = self.bytes.get_mut(self.len) else {
            return Err(X86Error::TooLong { pattern: self.pattern, what: "bytes" });
        };
        *slot = byte;
        self.len += 1;
        Ok(())
    }

    fn field(&mut self, at: usize, width: u8, operand: u8, from_bit: u8, kind: FieldKind) -> Result<(), X86Error> {
        let Some(slot) = self.fields.get_mut(self.count) else {
            return Err(X86Error::TooLong { pattern: self.pattern, what: "fields" });
        };
        // `at` is below MAX_BYTES * 8 = 256 bits: it fits the row's u16.
        *slot = Field { at: at as u16, width, operand, from_bit, kind };
        self.count += 1;
        Ok(())
    }

    /// REX = `0100WRXB` (SDM Vol. 2A 2.2.1.2): W 64-bit operand size; R extends ModRM.reg; X
    /// extends SIB.index; B extends ModRM.rm, SIB.base or the opcode's register.
    fn rex(&mut self, w: bool, r: Slot, x: Slot, b: Slot) -> Result<(), X86Error> {
        let at = self.len * 8;
        let mut byte = 0x40 | (u8::from(w) << 3);
        for (slot, bit) in [(r, 2u8), (x, 1), (b, 0)] {
            match slot {
                Slot::Op(operand) => self.field(at + usize::from(bit), 1, operand, 3, FieldKind::Reg)?,
                Slot::Hw(hw) => byte |= ((hw >> 3) & 1) << bit,
                Slot::Ext(_) => {}
            }
        }
        self.byte(byte)
    }

    /// The low three bits of `slot` at bit `at`: a field for an operand (0 in the bytes), the
    /// bits themselves for a fixed register or code.
    fn low3(&mut self, slot: Slot, at: usize) -> Result<u8, X86Error> {
        match slot {
            Slot::Op(operand) => {
                self.field(at, 3, operand, 0, FieldKind::Reg)?;
                Ok(0)
            }
            Slot::Hw(hw) => Ok(hw & 7),
            Slot::Ext(code) => Ok(code & 7),
        }
    }

    /// ModRM = `mod(7:6) reg(5:3) rm(2:0)` (SDM Vol. 2A 2.1.5).
    fn modrm(&mut self, md: u8, reg: Slot, rm: Slot) -> Result<(), X86Error> {
        let at = self.len * 8;
        let reg = self.low3(reg, at + 3)?;
        let rm = self.low3(rm, at)?;
        self.byte((md << 6) | (reg << 3) | rm)
    }

    /// SIB = `scale(7:6) index(5:3) base(2:0)` (SDM Vol. 2A 2.1.5, Table 2-3), scale 00.
    fn sib(&mut self, index: Slot, base: Slot) -> Result<(), X86Error> {
        let at = self.len * 8;
        let index = self.low3(index, at + 3)?;
        let base = self.low3(base, at)?;
        self.byte((index << 3) | base)
    }

    /// `n` zero bytes holding one field of operand `operand` from its bit 0, little-endian (an
    /// x86 displacement or immediate is stored low byte first).
    fn zeros(&mut self, n: u8, operand: u8, kind: FieldKind) -> Result<(), X86Error> {
        self.field(self.len * 8, n * 8, operand, 0, kind)?;
        for _ in 0..n {
            self.byte(0)?;
        }
        Ok(())
    }

    /// A rel32 to block operand `operand`: the displacement counts from the end of these four
    /// bytes, which end the instruction (Jcc rel32, JMP rel32).
    fn rel32(&mut self, operand: u8) -> Result<(), X86Error> {
        let from = self.len + 4;
        if from > usize::from(u8::MAX) {
            return Err(X86Error::TooLong { pattern: self.pattern, what: "branch origin" });
        }
        self.zeros(4, operand, FieldKind::Branch { scale: 0, from: from as u8 })
    }
}

/// `opcode` in MR form: REX(W, R = src, B = dst), the opcode, ModRM `mod = 11, reg = src,
/// rm = dst` (SDM "Op/En MR": operand 1 ModRM:r/m, operand 2 ModRM:reg).
fn mr(a: &mut Asm, w: bool, opcode: &[u8], dst: Slot, src: Slot) -> Result<(), X86Error> {
    a.rex(w, src, Slot::Ext(0), dst)?;
    for &byte in opcode {
        a.byte(byte)?;
    }
    a.modrm(0b11, src, dst)
}

/// `opcode` in RM form: REX(W, R = dst, B = src), the opcode, ModRM `mod = 11, reg = dst,
/// rm = src` (SDM "Op/En RM": operand 1 ModRM:reg, operand 2 ModRM:r/m).
fn rm(a: &mut Asm, w: bool, opcode: &[u8], dst: Slot, src: Slot) -> Result<(), X86Error> {
    a.rex(w, dst, Slot::Ext(0), src)?;
    for &byte in opcode {
        a.byte(byte)?;
    }
    a.modrm(0b11, dst, src)
}

/// MOV r/m, r: 89 /r in MR form (REX.W for 64 bits; a 32-bit move zero-extends).
fn mov_rr(a: &mut Asm, w: bool, dst: Slot, src: Slot) -> Result<(), X86Error> {
    mr(a, w, &[0x89], dst, src)
}

/// `opcode /ext` on a register: REX(W, B = target), the opcode, ModRM `mod = 11, reg = ext,
/// rm = target` ("Op/En M" / "MI").
fn ext_r(a: &mut Asm, w: bool, opcode: &[u8], ext: u8, target: Slot) -> Result<(), X86Error> {
    a.rex(w, Slot::Ext(0), Slot::Ext(0), target)?;
    for &byte in opcode {
        a.byte(byte)?;
    }
    a.modrm(0b11, Slot::Ext(ext), target)
}

/// `opcode` with a memory operand `[base + disp32]` in the canonical long form: [66] REX(W,
/// R = reg, X = 0, B = base), the opcode, ModRM `mod = 10, reg, rm = 100`, SIB `scale = 00,
/// index = 100 (none), base`, disp32 as a field of `disp_operand`.
fn mem(a: &mut Asm, w: bool, p66: bool, opcode: &[u8], reg: Slot, base: Slot, disp_operand: u8, disp: FieldKind) -> Result<(), X86Error> {
    if p66 {
        a.byte(0x66)?;
    }
    a.rex(w, reg, Slot::Ext(0), base)?;
    for &byte in opcode {
        a.byte(byte)?;
    }
    a.modrm(0b10, reg, Slot::Ext(0b100))?;
    a.sib(Slot::Ext(0b100), base)?;
    a.zeros(4, disp_operand, disp)
}

/// SETcc TEMP0b (REX 0F 90+cc, ModRM `mod = 11, reg = 000, rm = TEMP0`), then MOVZX r64, TEMP0b
/// (REX.W 0F B6 /r, RM form): the flags' condition as 0 or 1 in the whole result register.
fn setcc_movzx(a: &mut Asm, cc: u8) -> Result<(), X86Error> {
    ext_r(a, false, &[0x0f, 0x90 | cc], 0, Slot::Op(TEMP0))?;
    rm(a, true, &[0x0f, 0xb6], Slot::Op(RESULT), Slot::Op(TEMP0))
}

/// Jcc rel32: 0F 80+cc cd (no register, so no REX).
fn jcc(a: &mut Asm, cc: u8, block: u8) -> Result<(), X86Error> {
    a.byte(0x0f)?;
    a.byte(0x80 | cc)?;
    a.rel32(block)
}
