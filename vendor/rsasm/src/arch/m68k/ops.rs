//! Encoders for each instruction family.
//!
//! Every family encodes exactly the instruction that was written. GNU as
//! quietly substitutes cheaper ones — `move.l #1,d0` becomes `moveq`,
//! `add.w #1,d0` becomes `addq`, both checked against `m68k-elf-as` 2.47 —
//! and those are not equivalent encodings of the same instruction but
//! different instructions, with different flags on an address register and a
//! different length for anyone counting bytes in a patch or a jump table. An
//! Amiga or Atari programmer who wants `moveq` writes `moveq`, and vasm run
//! with `-no-opt` agrees with that reading. What *is* chosen here is the
//! shortest encoding of the same instruction: the size of an address or a
//! displacement, and the form of a branch.
//!
//! Where one mnemonic covers two opcodes the choice is by operand, as every
//! 68000 assembler makes it: `add` to an address register is `ADDA`, and
//! `add #imm` to memory is `ADDI`, because `ADD` itself can only reach memory
//! from a data register. `add #imm,d0` stays `ADD` with an immediate source,
//! as vasm `-no-opt` assembles it.

use super::branch::{self, BranchSize};
use super::encode::{self, *};
use super::insn::{BfShape, Def, Kind};
use super::operand::{Mode, Operand};
use super::table::{self, feature as f, rid};
use super::{Cpu, M68010UP, M68020UP, describe_arch, reloc};
use crate::arch::{AsmCtx, InsnRequest};
use crate::lexer::Dialect;
use crate::section::{Fixup, FixupKind, Variant};
use crate::source::Span;

pub struct Asm<'c, 'a> {
    pub cx: &'c mut AsmCtx<'a>,
    pub(crate) cpu: Cpu,
    /// The mnemonic as written, for messages.
    pub name: String,
    pub span: Span,
}

fn place(part: Vec<Alt>, place: Place) -> Part {
    Part { alts: part, place }
}

impl Asm<'_, '_> {
    fn err<T>(&mut self, span: Span, msg: impl Into<String>) -> Option<T> {
        self.cx.error(span, msg);
        None
    }

    /// Refuses what no CPU in `arch` has, naming what it needs.
    fn need(&mut self, arch: u32, span: Span, what: &str) -> Option<()> {
        if self.cpu.has(arch) {
            return Some(());
        }
        let cpu = self.cpu.describe();
        self.err(
            span,
            format!(
                "{what} needs {}; this target is a {cpu}",
                describe_arch(arch)
            ),
        )
    }

    fn single<'o>(&mut self, ops: &'o [Operand]) -> Option<&'o Operand> {
        match ops {
            [a] => Some(a),
            _ => self.wrong_count(1),
        }
    }

    fn two<'o>(&mut self, ops: &'o [Operand]) -> Option<(&'o Operand, &'o Operand)> {
        match ops {
            [a, b] => Some((a, b)),
            _ => self.wrong_count(2),
        }
    }

    /// Reports that an instruction was given the wrong number of operands.
    fn wrong_count<T>(&mut self, n: usize) -> Option<T> {
        let what = match n {
            0 => "no operands".to_string(),
            1 => "one operand".to_string(),
            _ => format!("{n} operands"),
        };
        let name = self.name.clone();
        self.err(self.span, format!("`{name}` takes {what}"))
    }

    fn check(&mut self, op: &Operand, allowed: u16, role: &str) -> Option<()> {
        if encode::class(op) & allowed != 0 {
            return Some(());
        }
        let name = self.name.clone();
        self.err(
            op.span,
            format!("`{name}` cannot take {} as its {role}", op.describe()),
        )
    }

    fn ea(&mut self, op: &Operand, size: Sz) -> Option<Vec<Alt>> {
        let ecx = EaCtx {
            cpu: self.cpu,
            size,
            float: None,
        };
        encode::ea(self.cx, op, ecx)
    }

    fn low(&mut self, op: &Operand, size: Sz) -> Option<Part> {
        Some(Part::low(self.ea(op, size)?))
    }

    fn no_bitfield(&mut self, ops: &[Operand]) -> Option<()> {
        match ops.iter().find(|o| !o.brace.is_empty()) {
            Some(o) => {
                let span = o.span;
                self.err(
                    span,
                    "only the bit-field instructions take `{offset:width}`",
                )
            }
            None => Some(()),
        }
    }

    /// A constant immediate within `lo..=hi`.
    fn constant(&mut self, op: &Operand, lo: i64, hi: i64, what: &str) -> Option<i64> {
        let Mode::Imm(e, span) = op.mode else {
            let name = self.name.clone();
            return self.err(op.span, format!("`{name}` needs an immediate {what}"));
        };
        match self.cx.constant(e) {
            Some(v) if (lo..=hi).contains(&v) => Some(v),
            Some(v) => self.err(span, format!("{what} {v} is out of range ({lo} to {hi})")),
            None => self.err(
                span,
                format!("{what} must be a constant, known where it is written"),
            ),
        }
    }

    fn dreg(&mut self, op: &Operand, role: &str) -> Option<u8> {
        match op.mode {
            Mode::DReg(n) => Some(n),
            _ => {
                let name = self.name.clone();
                self.err(
                    op.span,
                    format!("`{name}` needs a data register as its {role}"),
                )
            }
        }
    }

    fn areg(&mut self, op: &Operand, role: &str) -> Option<u8> {
        match op.mode {
            Mode::AReg(n) => Some(n),
            _ => {
                let name = self.name.clone();
                self.err(
                    op.span,
                    format!("`{name}` needs an address register as its {role}"),
                )
            }
        }
    }

    /// Rejects a byte-sized operation on an address register, which the
    /// 68000 has no encoding for.
    fn no_byte_areg(&mut self, op: &Operand, size: Sz) -> Option<()> {
        if size == Sz::B && matches!(op.mode, Mode::AReg(_)) {
            return self.err(op.span, "an address register cannot be used as a byte");
        }
        Some(())
    }

    fn one(op: u16) -> Vec<Variant> {
        vec![Variant::new(op.to_be_bytes().to_vec())]
    }

    /// `stem` is the mnemonic without its size letter.
    pub fn assemble(
        &mut self,
        def: Def,
        size: Option<char>,
        stem: &str,
        req: &InsnRequest<'_>,
    ) -> Option<Vec<Variant>> {
        // Which CPUs have the instruction is what GNU's table says for the
        // same spelling, size letter included: `addl` is on ColdFire and
        // `addb` is not.
        let gnu = format!("{stem}{}", size.map_or(String::new(), String::from));
        let arch = table::HAND_ARCH
            .binary_search_by(|(n, _)| (*n).cmp(gnu.as_str()))
            .map_or(def.arch, |i| table::HAND_ARCH[i].1);
        let name = format!("`{}`", self.name);
        self.need(arch, req.mnemonic_span, &name)?;
        let ops = super::operand::parse_list(self.cx, &req.cursor())?;
        if !matches!(def.kind, Kind::Bf(..)) {
            self.no_bitfield(&ops)?;
        }
        let sz = match size {
            Some('b') => Some(Sz::B),
            Some('w') => Some(Sz::W),
            Some('l') => Some(Sz::L),
            _ => None,
        };
        use Kind::*;
        match def.kind {
            Move => self.move_(&ops, sz.unwrap_or(Sz::W), sz.is_some()),
            MoveA => {
                let (src, dst) = self.two(&ops)?;
                self.movea(src, dst, sz.unwrap_or(Sz::W))
            }
            MoveQ => self.moveq(&ops),
            MoveM => self.movem(&ops, sz.unwrap_or(Sz::W)),
            MoveC => self.movec(&ops),
            Lea => {
                let (src, dst) = self.two(&ops)?;
                self.check(src, CONTROL, "source")?;
                let n = self.areg(dst, "destination")?;
                let p = self.low(src, Sz::L)?;
                Some(build(0x41c0 | (n as u16) << 9, vec![p]))
            }
            Pea => {
                let op = self.single(&ops)?;
                self.check(op, CONTROL, "operand")?;
                let p = self.low(op, Sz::L)?;
                Some(build(0x4840, vec![p]))
            }
            Exg => self.exg(&ops),
            Swap | Ext | ExtB => {
                let op = self.single(&ops)?;
                let n = self.dreg(op, "operand")? as u16;
                Some(Self::one(match (def.kind, sz) {
                    (Swap, _) => 0x4840 | n,
                    (Ext, Some(Sz::L)) => 0x48c0 | n,
                    (Ext, _) => 0x4880 | n,
                    _ => 0x49c0 | n,
                }))
            }
            Unary(base) => {
                let size = sz.unwrap_or(Sz::W);
                let op = self.single(&ops)?;
                self.check(op, DATA_ALT, "operand")?;
                let p = self.low(op, size)?;
                Some(build(base | size.bits() << 6, vec![p]))
            }
            Tst => {
                let size = sz.unwrap_or(Sz::W);
                let op = self.single(&ops)?;
                // The 68020 widened `tst` to every mode but a byte-sized
                // address register.
                let allowed = if self.cpu.has(M68020UP | f::CPU32 | f::FIDO_A | f::MCFISA_A) {
                    ALL
                } else {
                    DATA_ALT
                };
                self.check(op, allowed, "operand")?;
                self.no_byte_areg(op, size)?;
                let p = self.low(op, size)?;
                Some(build(0x4a00 | size.bits() << 6, vec![p]))
            }
            UnaryB(base) => {
                let op = self.single(&ops)?;
                self.check(op, DATA_ALT, "operand")?;
                let p = self.low(op, Sz::B)?;
                Some(build(base, vec![p]))
            }
            AddSub(base) => self.addsub(&ops, base, sz.unwrap_or(Sz::W)),
            AddSubA(base) => {
                let (src, dst) = self.two(&ops)?;
                self.adda(src, dst, base, sz.unwrap_or(Sz::W))
            }
            Immed(base) => {
                let (src, dst) = self.two(&ops)?;
                self.immed(src, dst, base, sz)
            }
            Quick(base) => {
                let size = sz.unwrap_or(Sz::W);
                let (src, dst) = self.two(&ops)?;
                let q = self.constant(src, 1, 8, "quick immediate")? as u16;
                self.check(dst, ALTERABLE, "destination")?;
                self.no_byte_areg(dst, size)?;
                let p = self.low(dst, size)?;
                Some(build(base | (q & 7) << 9 | size.bits() << 6, vec![p]))
            }
            X(base, sized) => {
                let size = sz.unwrap_or(if sized { Sz::W } else { Sz::B });
                let (src, dst) = self.two(&ops)?;
                let ss = if sized { size.bits() << 6 } else { 0 };
                match (&src.mode, &dst.mode) {
                    (Mode::DReg(y), Mode::DReg(x)) => {
                        Some(Self::one(base | (*x as u16) << 9 | ss | *y as u16))
                    }
                    (Mode::PreDec(y), Mode::PreDec(x)) => {
                        Some(Self::one(base | (*x as u16) << 9 | ss | 8 | *y as u16))
                    }
                    _ => {
                        let name = self.name.clone();
                        self.err(
                            src.span.to(dst.span),
                            format!("`{name}` works between two data registers or two `-(An)`"),
                        )
                    }
                }
            }
            Cmp => self.cmp(&ops, sz.unwrap_or(Sz::W)),
            CmpA => {
                let (src, dst) = self.two(&ops)?;
                self.cmpa(src, dst, sz.unwrap_or(Sz::W))
            }
            CmpM => {
                let (src, dst) = self.two(&ops)?;
                self.cmpm(src, dst, sz.unwrap_or(Sz::W))
            }
            Logic(base) => self.logic(&ops, base, sz.unwrap_or(Sz::W)),
            Eor => self.eor(&ops, sz.unwrap_or(Sz::W)),
            // GNU as reads `dr:dq` as two operands, and so takes them written
            // with a comma as well.
            MulDiv(w, div, signed) if sz == Some(Sz::L) => {
                self.muldiv(&pair_up(ops), w, div, signed, Sz::L)
            }
            MulDiv(w, div, signed) => self.muldiv(&ops, w, div, signed, sz.unwrap_or(Sz::W)),
            DivL(signed) => self.divl(&pair_up(ops), signed),
            Chk => {
                let size = sz.unwrap_or(Sz::W);
                let (src, dst) = self.two(&ops)?;
                self.check(src, DATA, "source")?;
                let n = self.dreg(dst, "destination")? as u16;
                let bits = if size == Sz::L { 0x100 } else { 0x180 };
                let p = self.low(src, size)?;
                Some(build(0x4000 | n << 9 | bits, vec![p]))
            }
            Chk2(chk) => {
                let size = sz.unwrap_or(Sz::W);
                let (src, dst) = self.two(&ops)?;
                self.check(src, CONTROL, "source")?;
                let r = match dst.mode {
                    Mode::DReg(n) => n as u16,
                    Mode::AReg(n) => 8 | n as u16,
                    _ => {
                        let name = self.name.clone();
                        return self.err(dst.span, format!("`{name}` compares against a register"));
                    }
                };
                let ext = r << 12 | if chk { 0x800 } else { 0 };
                let p = self.low(src, size)?;
                Some(build(
                    0x00c0 | size.bits() << 9,
                    vec![Part::fixed(ext.to_be_bytes().to_vec()), p],
                ))
            }
            Shift(t, left) => self.shift(&ops, t, left, sz),
            Bit(t) => self.bit(&ops, t, sz),
            Scc(c) => {
                let op = self.single(&ops)?;
                self.check(op, DATA_ALT, "operand")?;
                let p = self.low(op, Sz::B)?;
                Some(build(0x50c0 | (c as u16) << 8, vec![p]))
            }
            Bcc(c, jb) => self.bcc(&ops, c, jb, size, req.mnemonic_span),
            DBcc(c) => {
                let (reg, target) = self.two(&ops)?;
                let n = self.dreg(reg, "counter")?;
                let e = self.target(target)?;
                let constant = self.cx.constant(e).is_some();
                Some(branch::dbcc(c, n, self.cpu, e, constant, target.span))
            }
            Jmp(base) => {
                let op = self.single(&ops)?;
                self.check(op, CONTROL, "target")?;
                let p = self.low(op, Sz::L)?;
                Some(build(base, vec![p]))
            }
            Trap => {
                let op = self.single(&ops)?;
                let v = self.constant(op, 0, 15, "trap vector")?;
                Some(Self::one(0x4e40 | v as u16))
            }
            Bkpt => {
                let op = self.single(&ops)?;
                let v = self.constant(op, 0, 7, "breakpoint number")?;
                Some(Self::one(0x4848 | v as u16))
            }
            Link => self.link(&ops, sz),
            Unlk => {
                let op = self.single(&ops)?;
                let n = self.areg(op, "operand")?;
                Some(Self::one(0x4e58 | n as u16))
            }
            Word(op) => {
                let imm = self.single(&ops)?;
                let Mode::Imm(e, span) = imm.mode else {
                    let name = self.name.clone();
                    return self.err(imm.span, format!("`{name}` needs an immediate"));
                };
                let (bytes, fixups) = encode::immediate(self.cx, e, Sz::W, span)?;
                Some(build(op, vec![Part::words(bytes, fixups)]))
            }
            Fixed(op) if ops.is_empty() => Some(Self::one(op)),
            Fixed(_) => self.wrong_count(0),
            Bf(op, shape) => self.bitfield(&ops, op, shape),
        }
    }

    // ---- data movement ----------------------------------------------------

    fn move_(&mut self, ops: &[Operand], size: Sz, sized: bool) -> Option<Vec<Variant>> {
        let (src, dst) = self.two(ops)?;
        // The status-register moves exist in one size each.
        let word_only = |this: &mut Self| -> Option<()> {
            if sized && size != Sz::W {
                let name = this.name.clone();
                return this.err(
                    src.span.to(dst.span),
                    format!("`{name}` to or from `sr` or `ccr` is always a word"),
                );
            }
            Some(())
        };
        match (&src.mode, &dst.mode) {
            (_, Mode::Sr) | (_, Mode::Ccr) => {
                word_only(self)?;
                self.check(src, DATA, "source")?;
                let op = if dst.mode.is_sr() { 0x46c0 } else { 0x44c0 };
                let p = self.low(src, Sz::W)?;
                Some(build(op, vec![p]))
            }
            (Mode::Sr, _) | (Mode::Ccr, _) => {
                word_only(self)?;
                let op = if src.mode.is_sr() {
                    0x40c0
                } else {
                    self.need(M68010UP | f::MCFISA_A, src.span, "`move` from `ccr`")?;
                    0x42c0
                };
                self.check(dst, DATA_ALT, "destination")?;
                let p = self.low(dst, Sz::W)?;
                Some(build(op, vec![p]))
            }
            (Mode::Usp, Mode::AReg(n)) => Some(Self::one(0x4e68 | *n as u16)),
            (Mode::AReg(n), Mode::Usp) => Some(Self::one(0x4e60 | *n as u16)),
            (Mode::Usp, _) | (_, Mode::Usp) => self.err(
                src.span.to(dst.span),
                "`usp` can only be moved to or from an address register",
            ),
            (_, Mode::AReg(_)) => self.movea(src, dst, size),
            _ => {
                self.check(src, ALL, "source")?;
                self.no_byte_areg(src, size)?;
                self.check(dst, DATA_ALT, "destination")?;
                let bits = match size {
                    Sz::B => 0x1000,
                    Sz::W => 0x3000,
                    Sz::L => 0x2000,
                };
                let s = self.low(src, size)?;
                let d = place(self.ea(dst, size)?, Place::MoveDst);
                Some(build(bits, vec![s, d]))
            }
        }
    }

    fn movea(&mut self, src: &Operand, dst: &Operand, size: Sz) -> Option<Vec<Variant>> {
        if size == Sz::B {
            return self.err(dst.span, "an address register cannot be used as a byte");
        }
        self.check(src, ALL, "source")?;
        let n = self.areg(dst, "destination")? as u16;
        let bits = if size == Sz::L { 0x2040 } else { 0x3040 };
        let p = self.low(src, size)?;
        Some(build(bits | n << 9, vec![p]))
    }

    /// `MOVEQ` is only ever what was written. See the module comment for why
    /// `move.l #1,d0` is not quietly turned into one.
    fn moveq(&mut self, ops: &[Operand]) -> Option<Vec<Variant>> {
        let (src, dst) = self.two(ops)?;
        let n = self.dreg(dst, "destination")? as u16;
        let Mode::Imm(e, span) = src.mode else {
            return self.err(src.span, "`moveq` needs an immediate source");
        };
        let op = 0x7000 | n << 9;
        match self.cx.constant(e) {
            Some(v) if (-128..=127).contains(&v) => Some(Self::one(op | (v as u8) as u16)),
            Some(v) => self.err(
                span,
                format!("`moveq` takes -128 to 127, and {v} is out of range"),
            ),
            None => {
                let mut k = FixupKind::data(1).with_reloc(reloc::R_68K_8);
                k.signed = true;
                Some(vec![Variant {
                    bytes: op.to_be_bytes().to_vec(),
                    fixups: vec![Fixup {
                        offset: 1,
                        expr: e,
                        kind: k,
                        span,
                    }],
                }])
            }
        }
    }

    /// `MOVEM` with its register mask.
    ///
    /// The mask is bit 0 = `d0` up to bit 15 = `a7`, except with a `-(An)`
    /// destination, where it is reversed: bit 0 is `a7`. The CPU pushes
    /// `a7` first when predecrementing, so the reversed order lets it walk
    /// the mask from bit 0 in both directions. A mask written as `#imm` is
    /// taken as the programmer's own bits, already in the right order.
    fn movem(&mut self, ops: &[Operand], size: Sz) -> Option<Vec<Variant>> {
        let (a, b) = self.two(ops)?;
        if size == Sz::B {
            return self.err(self.span, "`movem` moves words or longs");
        }
        // A register list, a single register, or `#mask`: bits, or an
        // immediate to take as written.
        let mask_of = |o: &Operand| match o.mode {
            Mode::RegList(m) if m <= 0xffff => Some(Ok(m as u16)),
            Mode::DReg(n) => Some(Ok(1u16 << n)),
            Mode::AReg(n) => Some(Ok(1u16 << (8 + n))),
            Mode::Imm(e, span) => Some(Err((e, span))),
            _ => None,
        };
        let long = if size == Sz::L { 0x40 } else { 0 };
        let (mask, mem, op, allowed, role) = match (mask_of(a), mask_of(b)) {
            (Some(m), None) => (m, b, 0x4880 | long, IND | DISP | ABS | PRE, "destination"),
            (None, Some(m)) => (
                m,
                a,
                0x4c80 | long,
                IND | DISP | ABS | PCREL | POST,
                "source",
            ),
            _ => {
                return self.err(
                    a.span.to(b.span),
                    "`movem` moves a register list to or from memory",
                );
            }
        };
        self.check(mem, allowed, role)?;
        let reverse = matches!(mem.mode, Mode::PreDec(_));
        let mask_part = match mask {
            Ok(bits) => mask_bytes(bits, reverse),
            Err((e, span)) => {
                let (bytes, fixups) = encode::immediate(self.cx, e, Sz::W, span)?;
                Part::words(bytes, fixups)
            }
        };
        let p = self.low(mem, size)?;
        Some(build(op, vec![mask_part, p]))
    }

    fn movec(&mut self, ops: &[Operand]) -> Option<Vec<Variant>> {
        let (a, b) = self.two(ops)?;
        let reg = |o: &Operand| match o.mode {
            Mode::DReg(n) => Some(n as u16),
            Mode::AReg(n) => Some(8 | n as u16),
            _ => None,
        };
        let ctl = |o: &Operand| match o.mode {
            Mode::Ctl(id) => Some(id),
            Mode::Usp => Some(rid::USP),
            _ => None,
        };
        // GNU as also takes the register's 12-bit code as a number.
        let (op, r, ctl_op) = match (&a.mode, &b.mode) {
            (Mode::Imm(..), _) if reg(b).is_some() => (0x4e7a, reg(b)?, a),
            (_, Mode::Imm(..)) if reg(a).is_some() => (0x4e7b, reg(a)?, b),
            _ => match (ctl(a), ctl(b)) {
                (Some(_), _) if reg(b).is_some() => (0x4e7a, reg(b)?, a),
                (_, Some(_)) if reg(a).is_some() => (0x4e7b, reg(a)?, b),
                _ => {
                    return self.err(
                        a.span.to(b.span),
                        "`movec` moves between a control register and a general register",
                    );
                }
            },
        };
        if matches!(ctl_op.mode, Mode::Imm(..)) {
            let code = self.constant(ctl_op, 0, 0xfff, "control register code")? as u16;
            let mut bytes = (op as u16).to_be_bytes().to_vec();
            bytes.extend_from_slice(&(r << 12 | code).to_be_bytes());
            return Some(vec![Variant::new(bytes)]);
        }
        let (id, span) = (ctl(ctl_op)?, ctl_op.span);
        let Some(code) = super::reg::movec(id, self.cpu.ctrl) else {
            let cpu = self.cpu.describe();
            return self.err(
                span,
                format!(
                    "`{}` is not a control register `movec` reaches on a {cpu}",
                    super::reg::name_of(id)
                ),
            );
        };
        let ext = r << 12 | code;
        let mut bytes = (op as u16).to_be_bytes().to_vec();
        bytes.extend_from_slice(&ext.to_be_bytes());
        Some(vec![Variant::new(bytes)])
    }

    fn exg(&mut self, ops: &[Operand]) -> Option<Vec<Variant>> {
        let (a, b) = self.two(ops)?;
        let w = match (&a.mode, &b.mode) {
            (Mode::DReg(x), Mode::DReg(y)) => 0xc140 | (*x as u16) << 9 | *y as u16,
            (Mode::AReg(x), Mode::AReg(y)) => 0xc148 | (*x as u16) << 9 | *y as u16,
            // The mixed form always names the data register first.
            (Mode::DReg(x), Mode::AReg(y)) | (Mode::AReg(y), Mode::DReg(x)) => {
                0xc188 | (*x as u16) << 9 | *y as u16
            }
            _ => return self.err(a.span.to(b.span), "`exg` exchanges two registers"),
        };
        Some(Self::one(w))
    }

    // ---- arithmetic -------------------------------------------------------

    fn addsub(&mut self, ops: &[Operand], base: u16, size: Sz) -> Option<Vec<Variant>> {
        let (src, dst) = self.two(ops)?;
        let immed = if base == 0xd000 { 0x0600 } else { 0x0400 };
        match (&src.mode, &dst.mode) {
            (_, Mode::AReg(_)) => self.adda(src, dst, base | 0xc0, size),
            (Mode::Imm(..), m) if !matches!(m, Mode::DReg(_)) => {
                self.immed(src, dst, immed, Some(size))
            }
            (_, Mode::DReg(n)) => {
                self.check(src, ALL, "source")?;
                self.no_byte_areg(src, size)?;
                let p = self.low(src, size)?;
                Some(build(base | (*n as u16) << 9 | size.bits() << 6, vec![p]))
            }
            (Mode::DReg(n), _) => {
                self.check(dst, MEM_ALT, "destination")?;
                let p = self.low(dst, size)?;
                Some(build(
                    base | 0x100 | (*n as u16) << 9 | size.bits() << 6,
                    vec![p],
                ))
            }
            _ => {
                let name = self.name.clone();
                self.err(
                    src.span.to(dst.span),
                    format!("`{name}` needs a data register on one side, or an immediate source"),
                )
            }
        }
    }

    fn adda(&mut self, src: &Operand, dst: &Operand, base: u16, size: Sz) -> Option<Vec<Variant>> {
        if size == Sz::B {
            return self.err(dst.span, "an address register cannot be used as a byte");
        }
        self.check(src, ALL, "source")?;
        let n = self.areg(dst, "destination")? as u16;
        let long = if size == Sz::L { 0x100 } else { 0 };
        let p = self.low(src, size)?;
        Some(build(base | n << 9 | long, vec![p]))
    }

    /// `addi`, `subi`, `cmpi`, `andi`, `ori`, `eori`: the immediate, then the
    /// destination's extension words. The logical ones also reach `ccr` and
    /// `sr`.
    fn immed(
        &mut self,
        src: &Operand,
        dst: &Operand,
        base: u16,
        size: Option<Sz>,
    ) -> Option<Vec<Variant>> {
        let Mode::Imm(e, span) = src.mode else {
            let name = self.name.clone();
            return self.err(src.span, format!("`{name}` needs an immediate source"));
        };
        let logical = matches!(base, 0x0000 | 0x0200 | 0x0a00);
        let special = match dst.mode {
            Mode::Ccr => Some((Sz::B, 0x3c)),
            Mode::Sr => Some((Sz::W, 0x7c)),
            _ => None,
        };
        if let Some((want, low)) = special {
            if !logical {
                let name = self.name.clone();
                return self.err(
                    dst.span,
                    format!("`{name}` cannot change {}", dst.describe()),
                );
            }
            if size.is_some_and(|s| s != want) {
                return self.err(
                    dst.span,
                    format!("{} is a {}", dst.describe(), size_name(want)),
                );
            }
            let (bytes, fixups) = encode::immediate(self.cx, e, want, span)?;
            return Some(build(base | low, vec![Part::words(bytes, fixups)]));
        }
        let size = size.unwrap_or(Sz::W);
        // The 68020 lets `cmpi` compare against PC-relative memory.
        let allowed = if base == 0x0c00 && self.cpu.has(M68020UP | f::CPU32 | f::FIDO_A) {
            DATA_ALT | PCREL
        } else {
            DATA_ALT
        };
        self.check(dst, allowed, "destination")?;
        let (bytes, fixups) = encode::immediate(self.cx, e, size, span)?;
        let imm = Part::words(bytes, fixups);
        let p = self.low(dst, size)?;
        Some(build(base | size.bits() << 6, vec![imm, p]))
    }

    fn cmp(&mut self, ops: &[Operand], size: Sz) -> Option<Vec<Variant>> {
        let (src, dst) = self.two(ops)?;
        match (&src.mode, &dst.mode) {
            (_, Mode::AReg(_)) => self.cmpa(src, dst, size),
            (Mode::Imm(..), m) if !matches!(m, Mode::DReg(_)) => {
                self.immed(src, dst, 0x0c00, Some(size))
            }
            (Mode::PostInc(_), Mode::PostInc(_)) => self.cmpm(src, dst, size),
            (_, Mode::DReg(n)) => {
                self.check(src, ALL, "source")?;
                self.no_byte_areg(src, size)?;
                let p = self.low(src, size)?;
                Some(build(0xb000 | (*n as u16) << 9 | size.bits() << 6, vec![p]))
            }
            _ => self.err(
                dst.span,
                "`cmp` compares against a register; use `cmpi` or `cmpm` for memory",
            ),
        }
    }

    fn cmpa(&mut self, src: &Operand, dst: &Operand, size: Sz) -> Option<Vec<Variant>> {
        if size == Sz::B {
            return self.err(dst.span, "an address register cannot be used as a byte");
        }
        self.check(src, ALL, "source")?;
        let n = self.areg(dst, "destination")? as u16;
        let long = if size == Sz::L { 0x100 } else { 0 };
        let p = self.low(src, size)?;
        Some(build(0xb0c0 | n << 9 | long, vec![p]))
    }

    fn cmpm(&mut self, src: &Operand, dst: &Operand, size: Sz) -> Option<Vec<Variant>> {
        match (&src.mode, &dst.mode) {
            (Mode::PostInc(y), Mode::PostInc(x)) => Some(Self::one(
                0xb108 | (*x as u16) << 9 | size.bits() << 6 | *y as u16,
            )),
            _ => self.err(
                src.span.to(dst.span),
                "`cmpm` compares `(Ay)+` with `(Ax)+`",
            ),
        }
    }

    fn logic(&mut self, ops: &[Operand], base: u16, size: Sz) -> Option<Vec<Variant>> {
        let (src, dst) = self.two(ops)?;
        let immed = if base == 0xc000 { 0x0200 } else { 0x0000 };
        match (&src.mode, &dst.mode) {
            (Mode::Imm(..), Mode::Ccr | Mode::Sr) => self.immed(src, dst, immed, None),
            (Mode::Imm(..), m) if !matches!(m, Mode::DReg(_)) => {
                self.immed(src, dst, immed, Some(size))
            }
            (_, Mode::DReg(n)) => {
                self.check(src, DATA, "source")?;
                let p = self.low(src, size)?;
                Some(build(base | (*n as u16) << 9 | size.bits() << 6, vec![p]))
            }
            (Mode::DReg(n), _) => {
                self.check(dst, MEM_ALT, "destination")?;
                let p = self.low(dst, size)?;
                Some(build(
                    base | 0x100 | (*n as u16) << 9 | size.bits() << 6,
                    vec![p],
                ))
            }
            _ => {
                let name = self.name.clone();
                self.err(
                    src.span.to(dst.span),
                    format!("`{name}` needs a data register on one side, or an immediate source"),
                )
            }
        }
    }

    fn eor(&mut self, ops: &[Operand], size: Sz) -> Option<Vec<Variant>> {
        let (src, dst) = self.two(ops)?;
        match (&src.mode, &dst.mode) {
            (Mode::Imm(..), Mode::Ccr | Mode::Sr) => self.immed(src, dst, 0x0a00, None),
            (Mode::Imm(..), _) => self.immed(src, dst, 0x0a00, Some(size)),
            (Mode::DReg(n), _) => {
                self.check(dst, DATA_ALT, "destination")?;
                let p = self.low(dst, size)?;
                Some(build(0xb100 | (*n as u16) << 9 | size.bits() << 6, vec![p]))
            }
            _ => self.err(
                src.span,
                "`eor` needs a data register or an immediate as its source",
            ),
        }
    }

    fn muldiv(
        &mut self,
        ops: &[Operand],
        word_op: u16,
        div: bool,
        signed: bool,
        size: Sz,
    ) -> Option<Vec<Variant>> {
        let (src, dst) = self.two(ops)?;
        self.check(src, DATA, "source")?;
        if size != Sz::L {
            let n = self.dreg(dst, "destination")? as u16;
            let p = self.low(src, Sz::W)?;
            return Some(build(word_op | n << 9, vec![p]));
        }
        let s = (signed as u16) << 11;
        // A 64-bit form names both registers, `dh:dl` (or `dr:dq`).
        let ext = match (dst.mode.clone(), div) {
            (Mode::DReg(l), false) => (l as u16) << 12 | s,
            (Mode::DReg(q), true) => (q as u16) << 12 | s | q as u16,
            (Mode::Pair(h, l), _) => (l as u16) << 12 | s | 0x400 | h as u16,
            _ => {
                return self.err(
                    dst.span,
                    "a 32-bit multiply or divide needs `dn` or `dh:dl` as its destination",
                );
            }
        };
        let op = if div { 0x4c40 } else { 0x4c00 };
        let p = self.low(src, Sz::L)?;
        Some(build(op, vec![Part::fixed(ext.to_be_bytes().to_vec()), p]))
    }

    /// `divul`/`divsl`: a 32-bit dividend with the remainder kept.
    fn divl(&mut self, ops: &[Operand], signed: bool) -> Option<Vec<Variant>> {
        let (src, dst) = self.two(ops)?;
        self.check(src, DATA, "source")?;
        let s = (signed as u16) << 11;
        let ext = match dst.mode {
            Mode::DReg(q) => (q as u16) << 12 | s | q as u16,
            Mode::Pair(r, q) => (q as u16) << 12 | s | r as u16,
            _ => {
                let name = self.name.clone();
                return self.err(
                    dst.span,
                    format!("`{name}` needs `dq` or `dr:dq` as its destination"),
                );
            }
        };
        let p = self.low(src, Sz::L)?;
        Some(build(
            0x4c40,
            vec![Part::fixed(ext.to_be_bytes().to_vec()), p],
        ))
    }

    // ---- shifts and bits --------------------------------------------------

    fn shift(
        &mut self,
        ops: &[Operand],
        t: u8,
        left: bool,
        sz: Option<Sz>,
    ) -> Option<Vec<Variant>> {
        let dir = (left as u16) << 8;
        let t = t as u16;
        match ops {
            [src, dst] => {
                let size = sz.unwrap_or(Sz::W);
                let Mode::DReg(y) = dst.mode else {
                    let name = self.name.clone();
                    return self.err(
                        dst.span,
                        format!(
                            "`{name}` with a count shifts a data register; memory is shifted \
                             by one bit, written with a single operand"
                        ),
                    );
                };
                let ss = size.bits() << 6;
                let w = match src.mode {
                    Mode::Imm(..) => {
                        let c = self.constant(src, 1, 8, "shift count")? as u16;
                        0xe000 | (c & 7) << 9 | dir | ss | t << 3 | y as u16
                    }
                    Mode::DReg(x) => 0xe020 | (x as u16) << 9 | dir | ss | t << 3 | y as u16,
                    _ => {
                        return self.err(
                            src.span,
                            "a shift count is an immediate from 1 to 8 or a data register",
                        );
                    }
                };
                Some(Self::one(w))
            }
            [op] => match op.mode {
                // A lone data register shifts by one, as vasm reads it.
                Mode::DReg(y) => {
                    let size = sz.unwrap_or(Sz::W);
                    Some(Self::one(
                        0xe000 | 1 << 9 | dir | size.bits() << 6 | t << 3 | y as u16,
                    ))
                }
                _ => {
                    if sz.is_some_and(|s| s != Sz::W) {
                        return self.err(op.span, "a memory shift is always a word");
                    }
                    self.check(op, MEM_ALT, "operand")?;
                    let p = self.low(op, Sz::W)?;
                    Some(build(0xe0c0 | t << 9 | dir, vec![p]))
                }
            },
            _ => {
                let name = self.name.clone();
                self.err(self.span, format!("`{name}` takes one or two operands"))
            }
        }
    }

    fn bit(&mut self, ops: &[Operand], t: u8, sz: Option<Sz>) -> Option<Vec<Variant>> {
        let (src, dst) = self.two(ops)?;
        let t = (t as u16) << 6;
        let btst = t == 0;
        // A data register is tested as a long, memory as a byte; a size, if
        // written, has to agree.
        match (sz, &dst.mode) {
            (Some(Sz::B), Mode::DReg(_)) => {
                return self.err(dst.span, "a bit number in a data register is a long");
            }
            (Some(Sz::L), m) if !matches!(m, Mode::DReg(_)) => {
                return self.err(dst.span, "a bit number in memory is a byte");
            }
            _ => {}
        }
        match src.mode {
            Mode::DReg(n) => {
                self.check(dst, if btst { DATA } else { DATA_ALT }, "destination")?;
                let p = self.low(dst, Sz::B)?;
                Some(build(0x0100 | (n as u16) << 9 | t, vec![p]))
            }
            Mode::Imm(e, span) => {
                self.check(
                    dst,
                    if btst { DATA & !IMM } else { DATA_ALT },
                    "destination",
                )?;
                let (bytes, fixups) = match self.cx.constant(e) {
                    Some(v) if (0..=255).contains(&v) => (vec![0, v as u8], vec![]),
                    Some(v) => {
                        return self
                            .err(span, format!("bit number {v} is out of range (0 to 255)"));
                    }
                    None => encode::immediate(self.cx, e, Sz::B, span)?,
                };
                let bitno = Part::words(bytes, fixups);
                let p = self.low(dst, Sz::B)?;
                Some(build(0x0800 | t, vec![bitno, p]))
            }
            _ => self.err(src.span, "a bit number is an immediate or a data register"),
        }
    }

    fn bitfield(&mut self, ops: &[Operand], op: u16, shape: BfShape) -> Option<Vec<Variant>> {
        let (ea_op, reg) = match shape {
            BfShape::Ea => {
                let a = self.single(ops)?;
                (a, 0)
            }
            BfShape::EaReg => {
                let (a, r) = self.two(ops)?;
                if !r.brace.is_empty() {
                    return self.err(r.span, "the bit field goes on the first operand");
                }
                (a, self.dreg(r, "destination")?)
            }
            BfShape::RegEa => {
                let (r, a) = self.two(ops)?;
                if !r.brace.is_empty() {
                    return self.err(r.span, "the bit field goes on the second operand");
                }
                (a, self.dreg(r, "source")?)
            }
        };
        let (off, width) = match ea_op.brace.as_slice() {
            [off, width] => (off, width),
            _ => return self.err(ea_op.span, "a bit-field operand needs `{offset:width}`"),
        };
        // `bftst`, `bfextu`, `bfexts` and `bfffo` only read.
        let reads = matches!(op, 0xe8c0 | 0xe9c0 | 0xebc0 | 0xedc0);
        let allowed = if reads {
            DN | CONTROL
        } else {
            DN | (CONTROL & !PCREL)
        };
        self.check(ea_op, allowed, "bit-field operand")?;
        let mut ext = (reg as u16) << 12;
        ext |= match off.mode {
            Mode::DReg(d) => 0x800 | (d as u16) << 6,
            _ => (self.bf_const(off, 0, 31, "bit-field offset")? as u16) << 6,
        };
        ext |= match width.mode {
            Mode::DReg(d) => 0x20 | d as u16,
            // A width of 32 is written as 0.
            _ => self.bf_const(width, 1, 32, "bit-field width")? as u16 & 31,
        };
        let p = self.low(ea_op, Sz::L)?;
        Some(build(op, vec![Part::fixed(ext.to_be_bytes().to_vec()), p]))
    }

    /// A bit-field offset or width written as a number, with or without `#`.
    fn bf_const(&mut self, op: &Operand, lo: i64, hi: i64, what: &str) -> Option<i64> {
        let (e, span) = match op.mode {
            Mode::Imm(e, span) => (e, span),
            Mode::Abs(v) if v.width.is_none() => (v.e, v.span),
            _ => return self.err(op.span, format!("{what} is a number or `d0`-`d7`")),
        };
        match self.cx.constant(e) {
            Some(v) if (lo..=hi).contains(&v) => Some(v),
            Some(v) => self.err(span, format!("{what} {v} is out of range ({lo} to {hi})")),
            None => self.err(span, format!("{what} must be a constant")),
        }
    }

    // ---- control flow -----------------------------------------------------

    fn target(&mut self, op: &Operand) -> Option<crate::expr::ExprRef> {
        match op.mode {
            Mode::Abs(v) if v.width.is_none() => Some(v.e),
            _ => {
                let name = self.name.clone();
                self.err(
                    op.span,
                    format!(
                        "`{name}` branches to a label or address, not {}",
                        op.describe()
                    ),
                )
            }
        }
    }

    fn bcc(
        &mut self,
        ops: &[Operand],
        cond: u8,
        jb: bool,
        size: Option<char>,
        mspan: Span,
    ) -> Option<Vec<Variant>> {
        let op = self.single(ops)?;
        // GNU's `jra` and `jbsr` take any control address as well, as `jmp`
        // and `jsr`.
        if jb && cond <= 1 && size.is_none() && !matches!(op.mode, Mode::Abs(_)) {
            self.check(op, CONTROL, "target")?;
            let p = self.low(op, Sz::L)?;
            return Some(build(if cond == 0 { 0x4ec0 } else { 0x4e80 }, vec![p]));
        }
        let bsize = match size {
            Some('s' | 'b') => BranchSize::Short,
            Some('w') => BranchSize::Word,
            Some('l') => {
                self.need(super::long_branches(cond), mspan, "a 32-bit branch")?;
                BranchSize::Long
            }
            // GNU as keeps an unsized `bra` at 16 bits and relaxes only its
            // `j` spellings; Motorola assemblers relax every branch.
            _ if self.cx.dialect == Dialect::Gas && !jb => BranchSize::Word,
            _ => BranchSize::Relax,
        };
        let e = self.target(op)?;
        let constant = self.cx.constant(e).is_some();
        Some(branch::bcc(cond, bsize, self.cpu, e, constant, op.span))
    }

    fn link(&mut self, ops: &[Operand], sz: Option<Sz>) -> Option<Vec<Variant>> {
        let (reg, imm) = self.two(ops)?;
        let n = self.areg(reg, "frame pointer")? as u16;
        let Mode::Imm(e, span) = imm.mode else {
            return self.err(imm.span, "`link` needs an immediate displacement");
        };
        // A displacement is a 32-bit number to GNU as, so `#$ffffffff` is -1.
        let constant = self.cx.constant(e).map(|v| {
            if (0x8000_0000..=0xffff_ffff).contains(&v) {
                v - (1 << 32)
            } else {
                v
            }
        });
        let long = match (sz, constant) {
            (Some(Sz::L), _) => true,
            (Some(_), _) => false,
            // Too wide for `link.w`: GNU as uses the 68020's `link.l`.
            (None, Some(v)) => !(-32768..=32767).contains(&v),
            (None, None) => false,
        };
        if long {
            self.need(
                M68020UP | f::CPU32 | f::FIDO_A,
                span,
                "a 32-bit `link` displacement",
            )?;
            let (bytes, fixups) = encode::immediate(self.cx, e, Sz::L, span)?;
            let part = Part::words(bytes, fixups);
            return Some(build(0x4808 | n, vec![part]));
        }
        let (bytes, fixups) = match constant {
            Some(v) if !(-32768..=32767).contains(&v) => {
                return self.err(
                    span,
                    format!("`link.w` displacement {v} does not fit in a word"),
                );
            }
            Some(v) => (((v as i16) as u16).to_be_bytes().to_vec(), Vec::new()),
            None => encode::immediate(self.cx, e, Sz::W, span)?,
        };
        let part = Part::words(bytes, fixups);
        Some(build(0x4e50 | n, vec![part]))
    }
}

/// `ea,dh,dl` as `ea,dh:dl`.
fn pair_up(mut ops: Vec<Operand>) -> Vec<Operand> {
    if let [_, h, l] = ops.as_slice()
        && let (Mode::DReg(h), Mode::DReg(l)) = (&h.mode, &l.mode)
        && ops[1].brace.is_empty()
    {
        let span = ops[1].span.to(ops[2].span);
        let mode = Mode::Pair(*h, *l);
        ops.truncate(1);
        ops.push(Operand {
            mode,
            span,
            brace: Vec::new(),
        });
    }
    ops
}

fn mask_bytes(mask: u16, reverse: bool) -> Part {
    let m = if reverse { mask.reverse_bits() } else { mask };
    Part::fixed(m.to_be_bytes().to_vec())
}

impl Mode {
    fn is_sr(&self) -> bool {
        matches!(self, Mode::Sr)
    }
}
