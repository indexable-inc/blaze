//! The table-driven encoder, for every instruction the two hand-written
//! encoders do not: the saturating and packing group, the parallel
//! arithmetic, the bitfield moves, the load/store exclusives, the halfword
//! and dual multiplies, the divides, the hint and barrier space, the
//! coprocessor instructions and the system forms.
//!
//! Each mnemonic is a run of [`Form`]s from [`super::table`], which is
//! generated from GNU's own disassembler tables. A form is an opcode word
//! and a list of [`Op`]s saying what its operands are and where their bits
//! go, so encoding is a walk down that list. The forms of one mnemonic are
//! sorted narrow Thumb first, then wide Thumb, then A32, and the first that
//! fits wins — which is what picks a Thumb instruction's width, and what
//! `.n` and `.w` narrow the choice of.
//!
//! A walk may also leave a fixup behind, so that an operand only layout can
//! work out still reaches its field. One operand in the table needs that:
//! the address of a coprocessor transfer, where `vldr sd, label`,
//! `vstr sd, label`, `ldc p1, c2, label` and their relatives name a label
//! instead of a base register. It is the only one GNU as reads as an
//! address — `parse_address_main` makes a bare one PC-relative for the
//! loads, the stores, the preloads and these, and for nothing else — so
//! every other operand here is still read as the constant it is, which the
//! walk needs while it is choosing a form.

use super::insn::{AL, Width};
use super::operand::{Index, MemOffset, Operand, OperandKind, Shift, ShiftAmt, VecKind, VecReg};
use super::reg::{self, Reg};
use super::table::{self, Field, Form, Op, Set};
use super::{Insn, THUMB_BITS};
use crate::arch::AsmCtx;
use crate::expr::ExprRef;
use crate::section::{Fixup, FixupKind, LinkValue, Variant};
use crate::source::Span;

/// The forms of a mnemonic, following the spellings GNU as shares.
pub fn forms(name: &str) -> Option<&'static [Form]> {
    let name = match table::SPELLINGS.binary_search_by(|(a, _)| (*a).cmp(name)) {
        Ok(i) => table::SPELLINGS[i].1,
        Err(_) => name,
    };
    let lo = table::FORMS.partition_point(|f| f.name < name);
    let hi = table::FORMS.partition_point(|f| f.name <= name);
    (lo < hi).then(|| &table::FORMS[lo..hi])
}

/// Assembles `ins` from its table forms, or reports why none of them fit.
pub fn assemble(cx: &mut AsmCtx<'_>, ins: &Insn<'_>, at: u16) -> Option<Vec<Variant>> {
    let name = table::FORMS[at as usize].name;
    // `vldr sN, =expr` loads from the literal pool, which no form in the
    // table describes; see [`super::vfp`].
    if name == "vldr" && super::vfp::is_literal_load(ins) {
        return super::vfp::literal_load(cx, ins);
    }
    let thumb = cx.state.bits == THUMB_BITS;
    // Two things GNU as's own encoders settle that no row of either table
    // says, and that a target older than this backend's own reaches.
    match name {
        // `swp` and `swpb` are not instructions an ARMv8 core has at all:
        // `do_swap` refuses them where the earlier cores assemble them.
        "swp" | "swpb"
            if super::cpu::has(cx.state, super::cpu_data::V8) && !super::cpu::is_any(cx.state) =>
        {
            let what = super::cpu::selected_name(cx.state);
            cx.error(
                ins.span,
                format!("`{}` is not an instruction {what} has", ins.text),
            );
            return None;
        }
        // The hint encoding of `nop` is ARMv6K's. Before it `do_nop`
        // assembles `mov r0, r0`, and `do_t_nop` the 16-bit `mov r8, r8`
        // before Thumb-2; a hint number written out leaves only the hint.
        // A target with no `nop` at all -- a core with no Thumb, asked
        // for one in Thumb -- falls through to the forms and is refused
        // there.
        "nop"
            if ins.ops.is_empty()
                && !thumb
                && has_nop(cx, false)
                && !super::cpu::has(cx.state, super::cpu_data::V6K) =>
        {
            return Some(vec![Variant {
                bytes: ((u32::from(ins.cond) << 28) | 0x01a0_0000)
                    .to_le_bytes()
                    .to_vec(),
                fixups: Vec::new(),
            }]);
        }
        "nop"
            if ins.ops.is_empty()
                && thumb
                && ins.width != Width::Wide
                && has_nop(cx, true)
                && !super::cpu::has(cx.state, super::cpu_data::V6T2) =>
        {
            return Some(vec![Variant {
                bytes: 0x46c0u16.to_le_bytes().to_vec(),
                fixups: Vec::new(),
            }]);
        }
        _ => {}
    }
    let all = forms(name)?;
    let mut best: Option<(&Form, usize)> = None;
    // The first form the target does not have, for the diagnostic where no
    // form it does have fits.
    let mut absent: Option<u16> = None;
    for form in all {
        if !wanted(form.set, thumb, ins.width) {
            continue;
        }
        if !super::cpu::supports_form(cx.state, form.feats, form.unit) {
            absent = absent.or(Some(if super::cpu::supports(cx.state, form.feats) {
                form.unit
            } else {
                form.feats
            }));
            continue;
        }
        match encode(cx, ins, form, false) {
            Ok((word, fixups)) => return Some(emit(form, word, fixups)),
            Err(used) => {
                if best.is_none_or(|(_, n)| used > n) {
                    best = Some((form, used));
                }
            }
        }
    }
    // Nothing was tried because the target has none of the forms that could
    // have fitted: GNU as reports that case as "selected processor does not
    // support", whatever the operands were.
    if let Some(feats) = absent.filter(|_| best.is_none()) {
        let msg = super::cpu::unsupported(cx.state, ins.text, feats, thumb);
        cx.error(ins.span, msg);
        return None;
    }
    // The diagnostic comes from the form that read the most of what was
    // written, which is the one the source most nearly spells.
    match best {
        Some((form, _)) => {
            let _ = encode(cx, ins, form, true);
        }
        None => {
            let what = match (thumb, ins.width) {
                (true, Width::Narrow) => " as a 16-bit instruction",
                (true, Width::Wide) => " as a 32-bit instruction",
                (true, _) => " in Thumb",
                _ => " in ARM",
            };
            cx.error(ins.span, format!("`{}` cannot be encoded{what}", ins.text));
        }
    }
    None
}

/// Whether the target has `nop` at all in this instruction set, which it
/// has not where it has no Thumb; every `nop` form in the table is ARMv6K's
/// hint, so the older spellings have to ask the mnemonic instead.
fn has_nop(cx: &AsmCtx<'_>, thumb: bool) -> bool {
    super::cpu::supports(cx.state, super::cpu::mnemonic_feats("nop", thumb))
}

/// Whether a form's instruction set is the one being assembled, and its
/// width the one a `.n` or `.w` asked for.
fn wanted(set: Set, thumb: bool, width: Width) -> bool {
    match set {
        Set::Arm => !thumb,
        Set::T16 => thumb && width != Width::Wide,
        Set::T32 => thumb && width != Width::Narrow,
    }
}

fn emit(form: &Form, word: u32, fixups: Vec<Fixup>) -> Vec<Variant> {
    let bytes = match form.set {
        Set::T16 => (word as u16).to_le_bytes().to_vec(),
        // A 32-bit Thumb instruction is two little-endian halfwords, the
        // first on top of the word the table holds.
        Set::T32 => {
            let mut v = ((word >> 16) as u16).to_le_bytes().to_vec();
            v.extend_from_slice(&(word as u16).to_le_bytes());
            v
        }
        Set::Arm => word.to_le_bytes().to_vec(),
    };
    vec![Variant { bytes, fixups }]
}

/// Puts `value` into `field`, whose pieces run from the value's low bits up.
fn place(word: &mut u32, field: Field, value: u32) {
    let mut left = value;
    for (lsb, width) in field {
        let mask = (1u32 << width) - 1;
        *word |= (left & mask) << lsb;
        left >>= width;
    }
}

/// The number of bits a field holds.
fn width_of(field: Field) -> u32 {
    field.iter().map(|(_, w)| u32::from(*w)).sum()
}

/// The state of one attempt to fit the written operands to a form.
struct Walk<'a, 'b, 'c> {
    cx: &'a mut AsmCtx<'b>,
    ins: &'a Insn<'c>,
    word: u32,
    /// The next written operand to read.
    at: usize,
    /// The operand read last, for `!` and for a register pair's second half.
    prev: Option<usize>,
    /// Which registers each operand may hold, in written order; see
    /// [`Form::regs`].
    regs: &'static [u8],
    /// A shift written on the register just read, which the next `Op` must
    /// be the one that takes it.
    shift: Option<(Shift, u32)>,
    /// The `#lsb` of a bitfield instruction, which its `#width` is measured
    /// from.
    lsb: u32,
    /// The second half of a register pair the source left out, which is
    /// still one of the registers the instruction uses.
    implied: Option<Reg>,
    /// The vector register read last, for the forms that write it twice and
    /// for the second half of a `vmov` pair.
    vec: Option<VecReg>,
    /// Whether a NEON form's registers are quadword, once one of them has
    /// said so.
    quad: Option<bool>,
    /// What layout has to fill in once the addresses are known, which for
    /// these forms is the offset of a coprocessor transfer that names a
    /// label.
    fixups: Vec<Fixup>,
    /// Whether to report the first thing that does not fit.
    report: bool,
    failed: bool,
}

impl Walk<'_, '_, '_> {
    fn op(&self) -> Option<&Operand> {
        self.ins.ops.get(self.at)
    }

    fn fail<T>(&mut self, msg: impl FnOnce() -> String) -> Option<T> {
        if self.report && !self.failed {
            let span = self.op().map_or(self.ins.span, |o| o.span);
            self.cx.error(span, msg());
        }
        self.failed = true;
        None
    }

    fn take(&mut self) {
        self.prev = Some(self.at);
        self.at += 1;
    }

    /// The constant value of the operand about to be read.
    fn constant(&mut self) -> Option<i64> {
        let Some(op) = self.op().cloned() else {
            return self.fail(|| "expected an immediate".into());
        };
        let (OperandKind::Imm(e) | OperandKind::Braced(e)) = op.kind else {
            let what = op.describe();
            return self.fail(|| format!("expected an immediate, found {what}"));
        };
        match self.cx.constant(e) {
            Some(v) => Some(v),
            None => self.fail(|| "this immediate must be a constant expression".into()),
        }
    }

    /// Whether the register the operand about to be read holds is one this
    /// form may put there.
    fn allowed(&mut self, r: Reg) -> Option<()> {
        let class = self.regs.get(self.at).copied().unwrap_or(255);
        let writeback = self.op().is_some_and(|o| o.writeback);
        let bad = match class {
            1 => r == reg::PC,
            2 => r == reg::PC || r == reg::SP,
            // The base of a VFP block transfer may be the PC in A32 as long
            // as the instruction does not write it back.
            3 => r == reg::PC && writeback,
            _ => false,
        };
        if bad {
            let name = reg::name_of(r);
            let what = if class == 2 {
                "neither `pc` nor `sp`"
            } else if class == 3 {
                "anything but `pc` where the base is written back"
            } else {
                "anything but `pc`"
            };
            return self.fail(|| format!("`{name}` is not allowed here: {what}"));
        }
        Some(())
    }

    /// A register operand, taking a shift written on it for the `Op` after.
    fn register(&mut self, bits: u8) -> Option<Reg> {
        let Some(op) = self.op().cloned() else {
            return self.fail(|| "expected a register".into());
        };
        let r = match op.kind {
            OperandKind::Reg(r) => r,
            OperandKind::Shifted { rm, shift, amount } => {
                let n = match amount {
                    ShiftAmt::Imm(n) => n,
                    ShiftAmt::None => 0,
                    ShiftAmt::Reg(_) => {
                        return self.fail(|| {
                            "this instruction cannot take a register shift amount".into()
                        });
                    }
                };
                self.shift = Some((shift, n));
                rm
            }
            _ => {
                let what = op.describe();
                return self.fail(|| format!("expected a register, found {what}"));
            }
        };
        if u32::from(r) >= 1 << bits {
            let name = reg::name_of(r);
            return self.fail(|| format!("`{name}` is not one of r0-r7 here"));
        }
        self.allowed(r)?;
        self.take();
        Some(r)
    }

    /// Puts an immediate into a field, checking its range the way the field
    /// and its scale allow.
    fn immediate(&mut self, field: Field, scale: u8, bias: u8) -> Option<()> {
        let v = self.constant()?;
        let scale = i64::from(scale);
        let bias = i64::from(bias);
        let hi = ((1i64 << width_of(field)) - 1 + bias) * scale;
        if v < bias * scale || v > hi || v % scale != 0 {
            let step = if scale == 1 {
                String::new()
            } else {
                format!(" in steps of {scale}")
            };
            return self.fail(|| {
                format!(
                    "immediate {v} is out of range ({} to {hi}{step})",
                    bias * scale
                )
            });
        }
        place(&mut self.word, field, (v / scale - bias) as u32);
        self.take();
        Some(())
    }

    /// A vector register of the kind the form wants, and the field it goes
    /// in. A quadword register is held as the first of the two `d`
    /// registers it covers.
    fn vec_register(&mut self, kind: u8, field: Field) -> Option<VecReg> {
        let Some(op) = self.op().cloned() else {
            return self.fail(|| "expected a vector register".into());
        };
        let Some(v) = op.vec() else {
            let what = op.describe();
            return self.fail(|| format!("expected a vector register, found {what}"));
        };
        let want = match kind {
            0 => VecKind::S,
            2 => VecKind::Q,
            // Kind 3 is the double-or-quad a NEON form picks with bit 6 of
            // its own word: every such operand of one instruction has to be
            // the same width, and the first one written settles it.
            3 => match self.quad {
                Some(true) => VecKind::Q,
                Some(false) => VecKind::D,
                None => match v.kind {
                    VecKind::Q => VecKind::Q,
                    _ => VecKind::D,
                },
            },
            _ => VecKind::D,
        };
        if v.kind != want {
            let letter = want.letter();
            return self.fail(|| format!("expected a `{letter}` register here"));
        }
        if v.lane.is_some() {
            return self.fail(|| "this operand takes a whole register, not a lane".into());
        }
        if kind == 3 {
            self.quad = Some(want == VecKind::Q);
            if want == VecKind::Q {
                self.word |= 1 << 6;
            }
        }
        // A quadword register is held as the number of the first of the two
        // `d` registers it covers.
        let n = if want == VecKind::Q { v.n * 2 } else { v.n };
        if u32::from(n) >= 1 << width_of(field) {
            let name = format!("{}{}", want.letter(), v.n);
            return self.fail(|| format!("`{name}` is out of range for this instruction"));
        }
        place(&mut self.word, field, u32::from(n));
        self.vec = Some(v);
        self.take();
        Some(v)
    }

    /// A bare word operand — a coprocessor register, a barrier option, the
    /// interrupt flags — as it was written.
    fn word_operand(&mut self) -> Option<String> {
        match self.op().and_then(|o| o.word.clone()) {
            Some(w) => Some(w),
            None => self.fail(|| "expected a keyword operand".into()),
        }
    }

    /// The shift written on the register just read, if any.
    fn pending_shift(&mut self) -> Option<(Shift, u32)> {
        self.shift.take()
    }
}

/// Fits the written operands to `form`, returning the encoded word or how
/// many of them were read before it stopped fitting.
fn encode(
    cx: &mut AsmCtx<'_>,
    ins: &Insn<'_>,
    form: &Form,
    report: bool,
) -> Result<(u32, Vec<Fixup>), usize> {
    let mut w = Walk {
        cx,
        ins,
        word: form.word,
        at: 0,
        prev: None,
        implied: None,
        vec: None,
        quad: None,
        fixups: Vec::new(),
        regs: form.regs,
        shift: None,
        lsb: 0,
        report,
        failed: false,
    };
    let ok = (|| {
        // A32 keeps the condition in the top four bits; a form that has none
        // has them filled in already, and cannot be predicated.
        if form.set == Set::Arm {
            if form.cond {
                w.word |= u32::from(ins.cond) << 28;
            } else if ins.cond_written && ins.cond != AL {
                // `al` is let through: it is the condition an unconditional
                // instruction has anyway, and writing it changes nothing.
                let text = ins.text;
                w.fail(|| format!("`{text}` cannot be conditional"))?;
            }
        } else if ins.cond_written && ins.cond != AL {
            let text = ins.text;
            w.fail(|| format!("`{text}` is conditional, which in Thumb takes an `it` block"))?;
        }
        if ins.set_flags {
            let text = ins.text;
            w.fail(|| format!("`{text}` cannot set the flags"))?;
        }
        for op in form.ops {
            step(&mut w, form, *op)?;
        }
        // A rotation by zero is no rotation, so a form with no place for
        // one still takes it: `sxtb r5, r2, ror #0` is the 16-bit `sxtb`.
        if !matches!(w.shift, None | Some((Shift::Ror, 0))) {
            w.fail(|| "this instruction cannot take a shift".into())?;
        }
        if !form
            .ops
            .iter()
            .any(|o| matches!(o, Op::Writeback(_) | Op::SpBase(..)))
            && ins.ops.iter().any(|o| o.writeback)
        {
            let text = ins.text;
            w.fail(|| format!("`{text}` does not write a base register back"))?;
        }
        if w.at != ins.ops.len() {
            let extra = ins.ops.len();
            let text = ins.text;
            w.fail(|| format!("`{text}` does not take {extra} operands"))?;
        }
        Some(())
    })();
    match ok {
        Some(()) => Ok((w.word, w.fixups)),
        None => Err(w.at),
    }
}

fn step(w: &mut Walk<'_, '_, '_>, form: &Form, op: Op) -> Option<()> {
    match op {
        Op::Reg(lsb, bits) => {
            let r = w.register(bits)?;
            w.word |= u32::from(r) << lsb;
        }
        Op::RegTwice(a, b) => {
            let r = w.register(4)?;
            w.word |= (u32::from(r) << a) | (u32::from(r) << b);
        }
        Op::Base(lsb, bits) => {
            let r = base_register(w)?;
            if u32::from(r) >= 1 << bits {
                let name = reg::name_of(r);
                return w.fail(|| format!("`{name}` cannot be a base register here"));
            }
            w.word |= u32::from(r) << lsb;
        }
        // The second half of a register pair carries no bits, and GNU as
        // takes the spelling that leaves it out. The first half has to be an
        // even register, which is what makes the pair a pair.
        Op::Next => {
            let first = w.prev.and_then(|i| w.ins.ops[i].reg());
            if let Some(r) = first
                && (r % 2 != 0 || r == reg::LR)
            {
                return w
                    .fail(|| "the first of the pair must be an even register below `lr`".into());
            }
            let want = first.map(|r| r.wrapping_add(1));
            w.implied = want;
            if let Some(r) = w.op().and_then(|o| o.reg())
                && Some(r) == want
            {
                w.take();
            }
        }
        Op::Imm(field, scale, bias) => w.immediate(field, scale, bias)?,
        Op::OptImm(field) => {
            if w.at < w.ins.ops.len() {
                w.immediate(field, 1, 0)?;
            }
        }
        Op::Hint(field) => {
            if w.at < w.ins.ops.len() {
                if !matches!(w.op().map(|o| &o.kind), Some(OperandKind::Braced(_))) {
                    return w.fail(|| "a hint number is written in braces".into());
                }
                w.immediate(field, 1, 0)?;
            }
        }
        Op::Lsb(field) => {
            let v = w.constant()?;
            if !(0..32).contains(&v) {
                return w.fail(|| format!("bit position {v} is out of range (0 to 31)"));
            }
            w.lsb = v as u32;
            place(&mut w.word, field, w.lsb);
            w.take();
        }
        Op::Msb(field) => {
            let v = w.constant()?;
            let lsb = w.lsb;
            let last = i64::from(lsb) + v - 1;
            if v < 1 || last > 31 {
                return w.fail(|| format!("a field of {v} bits does not fit above bit {lsb}"));
            }
            place(&mut w.word, field, last as u32);
            w.take();
        }
        Op::Width(field) => {
            let v = w.constant()?;
            let lsb = w.lsb;
            if v < 1 || i64::from(lsb) + v > 32 {
                return w.fail(|| format!("a field of {v} bits does not fit above bit {lsb}"));
            }
            place(&mut w.word, field, (v - 1) as u32);
            w.take();
        }
        Op::Rotate(lsb) => {
            if let Some((shift, n)) = w.pending_shift() {
                if shift != Shift::Ror || n % 8 != 0 || n > 24 {
                    return w.fail(|| "the only rotation here is `ror` by 8, 16 or 24".into());
                }
                w.word |= (n / 8) << lsb;
            }
        }
        Op::SatShift(lsb, bits, asr, lsb2, bits2) => {
            sat_shift(w, form, lsb, bits, asr, lsb2, bits2)?
        }
        Op::Coproc(lsb) => {
            let name = w.word_operand()?;
            let Some(n) = name.strip_prefix('p').and_then(|d| d.parse::<u32>().ok()) else {
                return w.fail(|| format!("expected a coprocessor number, found `{name}`"));
            };
            if n > 15 {
                return w.fail(|| format!("there is no coprocessor `p{n}`"));
            }
            w.word |= n << lsb;
            w.take();
        }
        Op::CReg(lsb) => {
            let name = w.word_operand()?;
            let Some(n) = name
                .strip_prefix("cr")
                .or(name.strip_prefix('c'))
                .and_then(|d| d.parse::<u32>().ok())
            else {
                return w.fail(|| format!("expected a coprocessor register, found `{name}`"));
            };
            if n > 15 {
                return w.fail(|| format!("there is no coprocessor register `c{n}`"));
            }
            w.word |= n << lsb;
            w.take();
        }
        Op::Distinct => {
            let a = w.prev.and_then(|i| w.ins.ops[i].reg());
            let b = w
                .prev
                .and_then(|i| i.checked_sub(1))
                .and_then(|i| w.ins.ops[i].reg());
            if a.is_some() && a == b {
                return w.fail(|| "the two registers must be different".into());
            }
        }
        Op::FirstDistinct => {
            let first = w.ins.ops.first().and_then(|o| o.reg());
            let clash = w.ins.ops[1..]
                .iter()
                .filter_map(|o| match o.kind {
                    OperandKind::Reg(r) => Some(r),
                    OperandKind::Mem(m) => Some(m.base),
                    _ => None,
                })
                .chain(w.implied)
                .any(|r| Some(r) == first);
            if clash {
                return w.fail(|| "the status register must differ from the others".into());
            }
        }
        Op::ApsrNzcv => {
            let name = w.word_operand()?;
            if !name.eq_ignore_ascii_case("apsr_nzcv") {
                return w.fail(|| format!("expected `APSR_nzcv`, found `{name}`"));
            }
            w.take();
        }
        Op::Barrier => {
            if w.at >= w.ins.ops.len() {
                // No option written is a full system barrier.
                w.word |= 15;
                return Some(());
            }
            // `do_barrier`: an option, or the number one stands for.
            let named = w.op().and_then(|o| o.word.clone());
            let v = match named {
                Some(name) => {
                    let Some(v) = barrier_option(&name) else {
                        return w.fail(|| format!("`{name}` is not a barrier option"));
                    };
                    // ISB has only the one named option, which its opcode
                    // nibble says; a plain number is not checked.
                    if w.word & 0xf0 == 0x60 && v != 15 {
                        return w.fail(|| "`isb` takes only the `sy` option".into());
                    }
                    v
                }
                None => {
                    let n = w.constant()?;
                    if !(0..=15).contains(&n) {
                        return w.fail(|| format!("barrier option {n} is out of range (0 to 15)"));
                    }
                    n as u32
                }
            };
            w.word |= v;
            w.take();
        }
        Op::Writeback(lsb) => {
            let wrote = w.prev.is_some_and(|i| w.ins.ops[i].writeback);
            if lsb == 255 {
                // The form always writes back, so the `!` has to be there.
                if !wrote {
                    return w.fail(|| "this instruction writes its base register back".into());
                }
            } else if wrote {
                w.word |= 1 << lsb;
            }
        }
        Op::SpBase(lsb, wb) => {
            w.word |= u32::from(reg::SP) << lsb;
            if let Some(r) = w.op().and_then(|o| o.reg()) {
                if r != reg::SP {
                    return w.fail(|| "the base register of `srs` must be `sp`".into());
                }
                if w.ins.ops[w.at].writeback {
                    w.word |= 1 << wb;
                }
                w.take();
            }
        }
        Op::IntFlags(lsb) => {
            let name = w.word_operand()?;
            let mut bits = 0;
            for ch in name.chars() {
                bits |= match ch {
                    'f' => 1,
                    'i' => 2,
                    'a' => 4,
                    _ => return w.fail(|| format!("`{name}` is not a list of `a`, `i` and `f`")),
                };
            }
            if bits == 0 {
                return w.fail(|| "expected at least one of `a`, `i` and `f`".into());
            }
            w.word |= bits << lsb;
            w.take();
        }
        Op::Endian(lsb) => {
            let name = w.word_operand()?;
            match name.as_str() {
                "le" => {}
                "be" => w.word |= 1 << lsb,
                _ => return w.fail(|| format!("expected `be` or `le`, found `{name}`")),
            }
            w.take();
        }
        Op::IdxMem(base, index, shift) => table_branch(w, base, index, shift)?,
        Op::OffMem(base, field, scale) => offset_mem(w, base, field, scale)?,
        Op::CoprocMem => coproc_mem(w, form.set == Set::T32)?,
        Op::Vfp(kind, field) => {
            w.vec_register(kind, field)?;
        }
        Op::VfpSame(kind, field) => {
            let want = w.vec;
            let got = w.vec_register(kind, field)?;
            if want.is_some_and(|v| v.n != got.n) {
                return w.fail(|| "both operands must be the same register".into());
            }
        }
        Op::VfpNext => {
            let want = w.vec.map(|v| VecReg { n: v.n + 1, ..v });
            let Some(op) = w.op().cloned() else {
                return w.fail(|| "expected the second register of the pair".into());
            };
            if op.vec() != want {
                let letter = want.map_or('s', |v| v.kind.letter());
                let n = want.map_or(0, |v| v.n);
                return w.fail(|| format!("the second of the pair must be `{letter}{n}`"));
            }
            w.take();
        }
        Op::VfpLane(kind, field, lane) => vfp_lane(w, kind, field, lane)?,
        Op::VfpList(kind, first, count) => vfp_list(w, kind, first, count)?,
        Op::VfpMem => vfp_mem(w, form.set == Set::T32)?,
        Op::VfpImm(field) => w.immediate(field, 1, 0)?,
        Op::VfpFix(field) => vfp_fix(w, field)?,
        Op::PosImm(field) => {
            let top = (1i64 << width_of(field)) - 1;
            let v = w.constant()?;
            if v < 1 || v > top {
                return w.fail(|| format!("a shift here is 1 to {top}, not {v}"));
            }
            place(&mut w.word, field, v as u32);
            w.take();
        }
        Op::NegImm(field) => {
            // `vshr.s8 d0, d1, #8` empties the field and `#1` fills it.
            let width = width_of(field);
            let top = 1i64 << width;
            let v = w.constant()?;
            if v < 1 || v > top {
                return w.fail(|| format!("a shift here is 1 to {top}, not {v}"));
            }
            place(&mut w.word, field, (top - v) as u32);
            w.take();
        }
        Op::SizeImm(field, base) => {
            let mut got = 0;
            let mut shift = 0;
            for (lsb, bits) in field {
                got |= ((w.word >> lsb) & ((1 << bits) - 1)) << shift;
                shift += u32::from(*bits);
            }
            let want = i64::from(u32::from(base) << got);
            let v = w.constant()?;
            if v != want {
                return w.fail(|| format!("this operand is `{want}` here, not `{v}`"));
            }
            w.take();
        }
        Op::Scalar => scalar(w)?,
        Op::NeonStruct(kind, n, size) => neon_struct(w, kind, n, u32::from(size))?,
        Op::VfpTwice(kind, first, second) => {
            let v = w.vec_register(kind, first)?;
            let n = if v.kind == VecKind::Q { v.n * 2 } else { v.n };
            place(&mut w.word, second, u32::from(n));
        }
        Op::NeonImm(class, size) => {
            neon_immediate(w, class, u32::from(size), form.set == Set::T32)?;
        }
        Op::TblList(first, count) => tbl_list(w, first, count)?,
        Op::Fixed(want) => {
            let v = w.constant()?;
            if v != i64::from(want) {
                return w.fail(|| format!("this operand is `{want}` here, not `{v}`"));
            }
            w.take();
        }
        Op::Zero => {
            let v = w.constant()?;
            if v != 0 {
                return w.fail(|| "this operand is the constant zero".into());
            }
            w.take();
        }
        Op::Named(name) => {
            let got = w.word_operand()?;
            if got != name {
                return w.fail(|| format!("expected `{name}`, found `{got}`"));
            }
            w.take();
        }
    }
    Some(())
}

/// The base register of a `[rn]` operand, which must have nothing else in it.
fn base_register(w: &mut Walk<'_, '_, '_>) -> Option<Reg> {
    let Some(op) = w.op().cloned() else {
        return w.fail(|| "expected `[rn]`".into());
    };
    let OperandKind::Mem(mem) = op.kind else {
        let what = op.describe();
        return w.fail(|| format!("expected `[rn]`, found {what}"));
    };
    if !matches!(mem.offset, MemOffset::None) || mem.index != Index::Offset || op.writeback {
        return w.fail(|| "this instruction addresses `[rn]` with no offset".into());
    }
    w.allowed(mem.base)?;
    w.take();
    Some(mem.base)
}

/// `ssat`, `usat`, `pkhbt` and `pkhtb`'s optional shift: `lsl` or `asr` by a
/// constant, with the kind in one bit where both are allowed.
fn sat_shift(
    w: &mut Walk<'_, '_, '_>,
    form: &Form,
    lsb: u8,
    bits: u8,
    asr: u8,
    lsb2: u8,
    bits2: u8,
) -> Option<()> {
    let Some((shift, n)) = w.pending_shift() else {
        return Some(());
    };
    let asr_only = (64..128).contains(&asr);
    let mut is_asr = match shift {
        Shift::Lsl if !asr_only => false,
        Shift::Asr if asr != 255 => true,
        _ => {
            let want = if asr_only {
                "`asr`"
            } else if asr == 255 {
                "`lsl`"
            } else {
                "`lsl` or `asr`"
            };
            return w.fail(|| format!("the only shift here is {want}"));
        }
    };
    // A shift of zero is `lsl` whatever the source named it, as it is
    // everywhere else in the instruction set; where the form's own opcode
    // says `asr`, that turns it into the `lsl` instruction.
    if n == 0 {
        is_asr = false;
        // Only A32 rewrites the opcode: `md_apply_fix` clears the type bits
        // of the shift relocation, which `do_t_pkhbt` has no equivalent of.
        if asr_only && form.set == Set::Arm {
            w.word &= !(1 << (asr - 64));
        } else if asr_only {
            is_asr = true;
        }
    }
    // `asr #32` is written out and encoded as zero, except in the Thumb
    // saturations, whose own encoder takes only 0 to 31.
    let thirty_two = form.set == Set::Arm || asr >= 64;
    let n = if is_asr && n == 32 && thirty_two {
        0
    } else {
        n
    };
    let total = u32::from(bits) + if lsb2 == 255 { 0 } else { u32::from(bits2) };
    if n >= 1 << total {
        return w.fail(|| format!("shift amount {n} is out of range"));
    }
    if is_asr && asr < 32 {
        w.word |= 1 << asr;
    }
    w.word |= (n & ((1 << bits) - 1)) << lsb;
    if lsb2 != 255 {
        w.word |= (n >> bits) << lsb2;
    }
    Some(())
}

/// `tbb [rn, rm]` and `tbh [rn, rm, lsl #1]`.
fn table_branch(w: &mut Walk<'_, '_, '_>, base: u8, index: u8, shift: u8) -> Option<()> {
    let Some(op) = w.op().cloned() else {
        return w.fail(|| "expected `[rn, rm]`".into());
    };
    let OperandKind::Mem(mem) = op.kind else {
        let what = op.describe();
        return w.fail(|| format!("expected `[rn, rm]`, found {what}"));
    };
    let MemOffset::Reg {
        rm,
        add: true,
        shift: kind,
        amount,
        ..
    } = mem.offset
    else {
        return w.fail(|| "expected an index register".into());
    };
    if mem.index != Index::Offset || kind != Shift::Lsl || amount != u32::from(shift) {
        let want = if shift == 0 { "" } else { ", lsl #1" };
        return w.fail(|| format!("this instruction addresses `[rn, rm{want}]`"));
    }
    // `do_t_tb`: the table may be at the PC, but not on the stack, and the
    // index is neither.
    if mem.base == reg::SP {
        return w.fail(|| "`sp` cannot hold the branch table".into());
    }
    if rm == reg::SP || rm == reg::PC {
        let name = reg::name_of(rm);
        return w.fail(|| format!("`{name}` cannot be the branch table index"));
    }
    w.word |= u32::from(mem.base) << base;
    w.word |= u32::from(rm) << index;
    w.take();
    Some(())
}

/// `[rn]` or `[rn, #imm]` with a scaled unsigned offset, the Thumb-2
/// exclusive loads' addressing.
fn offset_mem(w: &mut Walk<'_, '_, '_>, base: u8, field: Field, scale: u8) -> Option<()> {
    let Some(op) = w.op().cloned() else {
        return w.fail(|| "expected a memory operand".into());
    };
    let OperandKind::Mem(mem) = op.kind else {
        let what = op.describe();
        return w.fail(|| format!("expected a memory operand, found {what}"));
    };
    let off = match mem.offset {
        MemOffset::None => 0,
        MemOffset::Imm(v) => v,
        _ => return w.fail(|| "this instruction takes no index register".into()),
    };
    let scale = i64::from(scale);
    let hi = ((1i64 << width_of(field)) - 1) * scale;
    if mem.index != Index::Offset || op.writeback {
        return w.fail(|| "this instruction does not write the base register back".into());
    }
    if off < 0 || off > hi || off % scale != 0 {
        return w.fail(|| format!("offset {off} is out of range (0 to {hi} in steps of {scale})"));
    }
    w.allowed(mem.base)?;
    w.word |= u32::from(mem.base) << base;
    place(&mut w.word, field, (off / scale) as u32);
    w.take();
    Some(())
}

/// A coprocessor transfer that names a label: `vldr sd, label`,
/// `ldc p1, c2, label` and the rest of the group.
///
/// GNU as's `parse_address_main` turns the bare address into `[pc, #label -
/// PC]`, and `encode_arm_cp_address` then encodes it as any other
/// PC-relative coprocessor transfer: P and U set, the PC as the base, and
/// eight bits counting words, so the label is 1020 bytes away at most. That
/// is the literal pool's own encoding, which [`super::vfp`] writes for
/// `vldr sd, =value` and whose fields these share; in Thumb the PC is
/// rounded down to a word first, as it is for every Thumb load.
///
/// Neither reference relocates the field, so what resolves is what GNU as
/// resolves: a label in the fixup's own section, weak ones aside.
fn pcrel_coproc(w: &mut Walk<'_, '_, '_>, thumb: bool, e: ExprRef, span: Span) -> Option<()> {
    if let Some(msg) = super::encode::pcrel_number(w.cx, e) {
        return w.fail(|| msg);
    }
    w.word |= (1 << 24) | (1 << 23) | (u32::from(reg::PC) << 16);
    let kind = if thumb {
        FixupKind::pcrel(4, 4)
            .with_pc_align(4)
            .scatter(super::vfp::scatter_thumb)
    } else {
        FixupKind::pcrel(4, 8).scatter(super::vfp::scatter_arm)
    }
    .with_field(0, 4)
    .with_limits(-1020, 1020)
    .link(LinkValue::Interwork(super::IW_STRONG_ONLY));
    w.fixups.push(Fixup {
        offset: 0,
        expr: e,
        kind,
        span,
    });
    w.take();
    Some(())
}

/// `ldc` and `stc`'s addressing: `[rn, #±imm8*4]` with or without writeback,
/// `[rn], #±imm8*4`, the unindexed `[rn], {imm8}` that hands the byte to
/// the coprocessor, and a bare label.
fn coproc_mem(w: &mut Walk<'_, '_, '_>, thumb: bool) -> Option<()> {
    let Some(op) = w.op().cloned() else {
        return w.fail(|| "expected a memory operand".into());
    };
    if let OperandKind::Imm(e) = op.kind {
        return pcrel_coproc(w, thumb, e, op.span);
    }
    let OperandKind::Mem(mem) = op.kind else {
        let what = op.describe();
        return w.fail(|| format!("expected a memory operand, found {what}"));
    };
    if mem.base == reg::PC && mem.index != Index::Offset {
        return w.fail(|| "a PC-relative address cannot write `pc` back".into());
    }
    w.word |= u32::from(mem.base) << 16;
    if let MemOffset::Unindexed(v) = mem.offset {
        if !(0..=255).contains(&v) {
            return w.fail(|| format!("option {v} does not fit in a byte"));
        }
        // P clear, W clear, U set: the offset field is the coprocessor's.
        w.word |= (1 << 23) | (v as u32);
        w.take();
        return Some(());
    }
    let off = match mem.offset {
        MemOffset::None => 0,
        MemOffset::Imm(v) => v,
        _ => return w.fail(|| "this instruction takes no index register".into()),
    };
    if off % 4 != 0 || off.unsigned_abs() > 1020 {
        return w.fail(|| format!("offset {off} is out of range (-1020 to 1020 in steps of 4)"));
    }
    match mem.index {
        Index::Offset => w.word |= 1 << 24,
        Index::PreIndex => w.word |= (1 << 24) | (1 << 21),
        Index::PostIndex => w.word |= 1 << 21,
    }
    if off >= 0 {
        w.word |= 1 << 23;
    }
    w.word |= (off.unsigned_abs() / 4) as u32;
    w.take();
    Some(())
}

/// Which list lengths and register strides each of the structure loads and
/// stores has an encoding for, from `do_neon_ld_st_interleave`: indexed by
/// the stride less one, then the list length less one, then `<n>` less one,
/// and holding the four bits the instruction carries at bits 8-11. 255
/// stands for a list the instruction has no encoding for.
static INTERLEAVE: [u8; 32] = [
    0x7, 255, 0xa, 255, 0x6, 255, 0x2, 255, // vld1 / vst1
    255, 255, 0x8, 0x9, 255, 255, 0x3, 255, // vld2 / vst2
    255, 255, 255, 255, 0x4, 0x5, 255, 255, // vld3 / vst3
    255, 255, 255, 255, 255, 255, 0x0, 0x1, // vld4 / vst4
];

/// The register list of a structure transfer, as the first register, how
/// many there are, whether they step by two, the lane they name and whether
/// that lane is every one of them.
struct StructList {
    first: u8,
    count: u8,
    stride: u32,
    lane: Option<u32>,
    all: bool,
}

/// A structure transfer: `vld1` to `vld4` and `vst1` to `vst4`, whose
/// register list, element size and alignment together say which of the three
/// encodings it is and what goes in the fields.
fn neon_struct(w: &mut Walk<'_, '_, '_>, kind: u8, n: u8, size: u32) -> Option<()> {
    let Some(op) = w.op().cloned() else {
        return w.fail(|| "expected a register list".into());
    };
    let OperandKind::VecList {
        kind: VecKind::D,
        first,
        count,
        lane,
        spaced,
        all,
        degenerate: _,
    } = op.kind
    else {
        let what = op.describe();
        return w.fail(|| format!("expected a list of `d` registers, found {what}"));
    };
    let list = StructList {
        first,
        count,
        stride: if spaced { 2 } else { 1 },
        lane,
        all,
    };
    w.take();
    let Some(mem) = w.op().cloned() else {
        return w.fail(|| "expected an address".into());
    };
    let OperandKind::Mem(mem) = mem.kind else {
        let what = mem.describe();
        return w.fail(|| format!("expected an address, found {what}"));
    };
    let logsize = size.trailing_zeros() - 3;
    match kind {
        0 => interleave(w, &list, n, size, logsize, mem.align)?,
        1 => single_lane(w, &list, n, size, logsize, mem.align)?,
        _ => copy_to_lanes(w, &list, n, size, logsize, mem.align)?,
    }
    if mem.base == 15 {
        return w.fail(|| "the program counter is not a base register here".into());
    }
    w.word |= u32::from(list.first & 0xF) << 12;
    w.word |= u32::from(list.first >> 4) << 22;
    w.word |= u32::from(mem.base) << 16;
    match mem.offset {
        // `[rn], rm`: the base is stepped by a register after the transfer,
        // and the two registers the field cannot hold mean the other two
        // addressing modes.
        MemOffset::Reg { rm, .. } if mem.index == Index::PostIndex => {
            if rm == 13 || rm == 15 {
                return w.fail(|| "this register cannot step the base here".into());
            }
            w.word |= u32::from(rm);
        }
        MemOffset::None if mem.index != Index::PostIndex => {
            w.word |= if mem.index == Index::PreIndex {
                0xD
            } else {
                0xF
            };
        }
        _ => return w.fail(|| "this instruction takes `[rn]`, `[rn]!` or `[rn], rm`".into()),
    }
    w.take();
    Some(())
}

/// `vld1.8 {d0-d3}, [r0]`: whole registers, one structure element to each
/// in turn.
fn interleave(
    w: &mut Walk<'_, '_, '_>,
    list: &StructList,
    n: u8,
    size: u32,
    logsize: u32,
    align: Option<u32>,
) -> Option<()> {
    if list.lane.is_some() || list.all {
        return w.fail(|| "this form takes whole registers".into());
    }
    let alignbits = match align {
        None => 0,
        Some(64) => 1,
        Some(128) if list.count == 2 || list.count == 4 => 2,
        Some(256) if list.count == 4 => 3,
        Some(_) => return w.fail(|| "bad alignment".into()),
    };
    if n > 1 && size == 64 {
        return w.fail(|| "this instruction has no 64-bit elements".into());
    }
    let idx = (list.stride - 1) | (u32::from(list.count - 1) << 1) | (u32::from(n - 1) << 3);
    let typebits = INTERLEAVE[idx as usize];
    if typebits == 255 {
        return w.fail(|| "bad list type for this instruction".into());
    }
    w.word |= alignbits << 4;
    w.word |= logsize << 6;
    w.word |= u32::from(typebits) << 8;
    Some(())
}

/// The alignment a structure transfer may promise for this element size, as
/// whether the encoding's alignment bit is set. The pairs are the ones
/// `neon_alignment_bit` is called with.
fn lane_align(
    w: &mut Walk<'_, '_, '_>,
    size: u32,
    align: Option<u32>,
    allowed: &[(u32, u32)],
) -> Option<bool> {
    let Some(align) = align else {
        return Some(false);
    };
    if allowed.contains(&(size, align)) {
        Some(true)
    } else {
        w.fail(|| "unsupported alignment for this instruction".into())
    }
}

/// `vld2.16 {d0[1], d1[1]}, [r0]`: one element of each register.
fn single_lane(
    w: &mut Walk<'_, '_, '_>,
    list: &StructList,
    n: u8,
    size: u32,
    logsize: u32,
    align: Option<u32>,
) -> Option<()> {
    let Some(lane) = list.lane else {
        return w.fail(|| "this form takes one lane of each register".into());
    };
    if list.count != n {
        return w.fail(|| format!("this instruction takes {n} registers"));
    }
    if lane >= 64 / size {
        return w.fail(|| format!("lane {lane} is out of range for this element size"));
    }
    if n != 1 && list.stride == 2 && size == 8 {
        return w.fail(|| "a stride of two needs elements wider than a byte".into());
    }
    let alignbits = match n {
        1 => {
            let on = lane_align(w, size, align, &[(16, 16), (32, 32)])?;
            match (on, size) {
                (true, 16) => 1,
                (true, 32) => 3,
                _ => 0,
            }
        }
        2 => u32::from(lane_align(w, size, align, &[(8, 16), (16, 32), (32, 64)])?),
        3 => {
            if align.is_some() {
                return w.fail(|| "this instruction takes no alignment".into());
            }
            0
        }
        _ => {
            let on = lane_align(w, size, align, &[(8, 32), (16, 64), (32, 64), (32, 128)])?;
            match (on, size) {
                (true, 32) if align == Some(128) => 2,
                (true, _) => 1,
                (false, _) => 0,
            }
        }
    };
    w.word |= alignbits << 4;
    if n != 1 && list.stride == 2 {
        w.word |= 1 << (4 + logsize);
    }
    w.word |= lane << (logsize + 5);
    w.word |= logsize << 10;
    Some(())
}

/// `vld1.8 {d0[], d1[]}, [r0]`: one element copied over every lane, which
/// only a load does.
fn copy_to_lanes(
    w: &mut Walk<'_, '_, '_>,
    list: &StructList,
    n: u8,
    size: u32,
    logsize: u32,
    align: Option<u32>,
) -> Option<()> {
    if !list.all {
        return w.fail(|| "this form copies one element over every lane".into());
    }
    let on = match n {
        1 => {
            if list.stride == 2 {
                return w.fail(|| "these registers step by one".into());
            }
            let on = lane_align(w, size, align, &[(16, 16), (32, 32)])?;
            match list.count {
                1 => {}
                2 => w.word |= 1 << 5,
                _ => return w.fail(|| "bad list length".into()),
            }
            w.word |= logsize << 6;
            on
        }
        2 => {
            let on = lane_align(w, size, align, &[(8, 16), (16, 32), (32, 64)])?;
            if list.count != 2 {
                return w.fail(|| "bad list length".into());
            }
            if list.stride == 2 {
                w.word |= 1 << 5;
            }
            w.word |= logsize << 6;
            on
        }
        3 => {
            if align.is_some() {
                return w.fail(|| "this instruction takes no alignment".into());
            }
            if list.count != 3 {
                return w.fail(|| "bad list length".into());
            }
            if list.stride == 2 {
                w.word |= 1 << 5;
            }
            w.word |= logsize << 6;
            false
        }
        _ => {
            let on = lane_align(w, size, align, &[(8, 32), (16, 64), (32, 64), (32, 128)])?;
            if list.count != 4 {
                return w.fail(|| "bad list length".into());
            }
            if list.stride == 2 {
                w.word |= 1 << 5;
            }
            if size == 32 && align == Some(128) {
                w.word |= 3 << 6;
            } else {
                w.word |= logsize << 6;
            }
            on
        }
    };
    w.word |= u32::from(on) << 4;
    Some(())
}

/// Whether every byte of `imm` is all ones or all zeroes, which is the one
/// pattern a 64-bit `vmov` immediate can hold.
fn bits_same_in_bytes(imm: u32) -> bool {
    (0..4).all(|i| {
        let byte = (imm >> (i * 8)) & 0xFF;
        byte == 0 || byte == 0xFF
    })
}

/// That pattern as the four bits the encoding holds.
fn squash_bits(imm: u32) -> u32 {
    (imm & 1) | ((imm >> 7) & 2) | ((imm >> 14) & 4) | ((imm >> 21) & 8)
}

/// The low `size` bits of `hi:lo`, inverted.
pub(super) fn invert_size(lo: &mut u32, hi: &mut u32, size: u32) {
    match size {
        8 => *lo = !*lo & 0xFF,
        16 => *lo = !*lo & 0xFFFF,
        64 => {
            *hi = !*hi;
            *lo = !*lo;
        }
        _ => *lo = !*lo,
    }
}

/// `cmode` and the eight immediate bits for `vmov` and `vmvn`, following
/// `neon_cmode_for_move_imm`: the value is a byte somewhere in the element,
/// a byte with ones below it, or a byte pattern repeated over the element.
/// `op` starts as 1 for `vmvn` and the encoding may flip it.
pub(super) fn cmode_for_move(mut lo: u32, hi: u32, op: &mut u32, size: u32) -> Option<(u32, u32)> {
    if size == 64 {
        if bits_same_in_bytes(hi) && bits_same_in_bytes(lo) {
            if *op == 1 {
                return None;
            }
            *op = 1;
            return Some((0xE, (squash_bits(hi) << 4) | squash_bits(lo)));
        }
        if hi != lo {
            return None;
        }
    }
    if size >= 32 {
        if lo == lo & 0x0000_00FF {
            return Some((0x0, lo));
        } else if lo == lo & 0x0000_FF00 {
            return Some((0x2, lo >> 8));
        } else if lo == lo & 0x00FF_0000 {
            return Some((0x4, lo >> 16));
        } else if lo == lo & 0xFF00_0000 {
            return Some((0x6, lo >> 24));
        } else if lo == (lo & 0x0000_FF00) | 0x0000_00FF {
            return Some((0xC, (lo >> 8) & 0xFF));
        } else if lo == (lo & 0x00FF_0000) | 0x0000_FFFF {
            return Some((0xD, (lo >> 16) & 0xFF));
        }
        if lo & 0xFFFF != lo >> 16 {
            return None;
        }
        lo &= 0xFFFF;
    }
    if size >= 16 {
        if lo == lo & 0x0000_00FF {
            return Some((0x8, lo));
        } else if lo == lo & 0x0000_FF00 {
            return Some((0xA, lo >> 8));
        }
        if lo & 0xFF != lo >> 8 {
            return None;
        }
        lo &= 0xFF;
    }
    if lo == lo & 0xFF {
        // There is no `vmvn` of a byte: it would be a `vmov` of the
        // complement, which is what the caller falls back to.
        if *op == 1 {
            return None;
        }
        return Some((0xE, lo));
    }
    None
}

/// `cmode` and the immediate bits for `vorr` and `vbic`, following
/// `neon_cmode_for_logic_imm`. A byte-sized immediate is the halfword that
/// repeats it, which leaves nothing but zero in range.
fn cmode_for_logic(mut imm: u32, mut size: u32) -> Option<(u32, u32)> {
    if size == 8 {
        imm |= imm << 8;
        size = 16;
    }
    if size >= 32 {
        if imm == imm & 0x0000_00FF {
            return Some((0x1, imm));
        } else if imm == imm & 0x0000_FF00 {
            return Some((0x3, imm >> 8));
        } else if imm == imm & 0x00FF_0000 {
            return Some((0x5, imm >> 16));
        } else if imm == imm & 0xFF00_0000 {
            return Some((0x7, imm >> 24));
        }
        if imm & 0xFFFF != imm >> 16 {
            return None;
        }
        imm &= 0xFFFF;
    }
    if imm == imm & 0x0000_00FF {
        Some((0x9, imm))
    } else if imm == imm & 0x0000_FF00 {
        Some((0xB, imm >> 8))
    } else {
        None
    }
}

/// The NEON modified immediate. One encoding holds every immediate the
/// vector moves and the vector logic take: four bits of `cmode` and one
/// `op` bit say which of the patterns the eight immediate bits stand for,
/// and GNU as picks them from the value written -- turning a `vmov` into a
/// `vmvn` of the complement, or the other way round, when only one of the
/// two has the pattern.
fn neon_immediate(w: &mut Walk<'_, '_, '_>, class: u8, size: u32, thumb: bool) -> Option<()> {
    let value = w.constant()? as u64;
    let mut lo = value as u32;
    let mut hi = if size == 64 { (value >> 32) as u32 } else { 0 };
    let (cmode, immbits, op) = if class < 2 {
        if size < 32 && lo & !((1 << size) - 1) != 0 {
            return w.fail(|| "immediate has bits set outside the element size".into());
        }
        let mut op = u32::from(class == 1);
        match cmode_for_move(lo, hi, &mut op, size) {
            Some((cmode, bits)) => (cmode, bits, op),
            None => {
                // The complement may have a pattern where the value has
                // none, which turns a `vmov` into a `vmvn` and back.
                invert_size(&mut lo, &mut hi, size);
                op ^= 1;
                match cmode_for_move(lo, hi, &mut op, size) {
                    Some((cmode, bits)) => (cmode, bits, op),
                    None => {
                        return w.fail(|| "no vector immediate holds this value".into());
                    }
                }
            }
        }
    } else {
        if size == 64 && hi != lo {
            return w.fail(|| "a 64-bit immediate here repeats its low word".into());
        }
        if class >= 4 {
            // `vand` is `vbic` of the complement, and `vorn` is `vorr` of
            // it; both are spellings GNU as takes and never prints.
            invert_size(&mut lo, &mut hi, size);
        }
        // `vbic` and `vand` are `vorr` and `vorn` with the `op` bit set.
        let op = u32::from(class == 3 || class == 4);
        match cmode_for_logic(lo, size) {
            Some((cmode, bits)) => (cmode, bits, op),
            None => return w.fail(|| "no vector immediate holds this value".into()),
        }
    };
    w.word |= cmode << 8;
    w.word |= op << 5;
    w.word |= immbits & 0xF;
    w.word |= ((immbits >> 4) & 7) << 16;
    // The top bit of the immediate is bit 24 of an A32 word, and a T32 NEON
    // instruction has a top byte of its own, where it is bit 28.
    w.word |= ((immbits >> 7) & 1) << if thumb { 28 } else { 24 };
    w.take();
    Some(())
}

/// A NEON scalar, `d0[1]`. The register and the lane share bits 0-3 and
/// bit 5: the element size in bits 20-21 says how many of those bits the
/// register takes, and the lane sits above it.
fn scalar(w: &mut Walk<'_, '_, '_>) -> Option<()> {
    let Some(v) = w.op().cloned().and_then(|o| o.vec()) else {
        return w.fail(|| "expected a scalar, as in `d0[1]`".into());
    };
    let Some(lane) = v.lane else {
        return w.fail(|| "this operand names a lane, as in `d0[1]`".into());
    };
    if v.kind != VecKind::D {
        return w.fail(|| "a scalar is a lane of a `d` register".into());
    }
    let size = (w.word >> 20) & 3;
    let regs = 4 << size;
    if u32::from(v.n) >= regs {
        return w.fail(|| format!("a {}-bit scalar lives in d0 to d{}", 8 << size, regs - 1));
    }
    let lanes = 8 >> size;
    if lane >= lanes {
        return w.fail(|| format!("this register holds {lanes} lanes of that size"));
    }
    let raw = u32::from(v.n) | (lane << (size + 2));
    w.word |= raw & 0xF;
    w.word |= (raw >> 4) << 5;
    w.vec = Some(v);
    w.take();
    Some(())
}

/// `vtbl`'s table, `{d0-d3}`: one to four `d` registers, the count held one
/// less than it is written.
fn tbl_list(w: &mut Walk<'_, '_, '_>, first: Field, count: Field) -> Option<()> {
    let Some(op) = w.op().cloned() else {
        return w.fail(|| "expected a register list".into());
    };
    let OperandKind::VecList {
        kind: VecKind::D,
        first: reg,
        count: n,
        lane: None,
        spaced: false,
        all: false,
        degenerate,
    } = op.kind
    else {
        let what = op.describe();
        return w.fail(|| format!("expected a list of `d` registers, found {what}"));
    };
    if degenerate {
        return w.fail(|| "a register range runs from one register to another".into());
    }
    if n < 1 || u32::from(n - 1) >= 1 << width_of(count) {
        return w.fail(|| "a table holds one to four registers".into());
    }
    if u32::from(reg) + u32::from(n) > 32 {
        return w.fail(|| "this list runs past `d31`".into());
    }
    place(&mut w.word, first, u32::from(reg));
    place(&mut w.word, count, u32::from(n - 1));
    w.take();
    Some(())
}

/// A vector register with a lane index, `d0[1]`.
fn vfp_lane(w: &mut Walk<'_, '_, '_>, kind: u8, field: Field, lane: Field) -> Option<()> {
    let Some(v) = w.op().cloned().and_then(|o| o.vec()) else {
        return w.fail(|| "expected a vector register with a lane".into());
    };
    let want = if kind == 0 { VecKind::S } else { VecKind::D };
    let Some(index) = v.lane else {
        return w.fail(|| "this operand names a lane, as in `d0[1]`".into());
    };
    if v.kind != want {
        let letter = want.letter();
        return w.fail(|| format!("expected a `{letter}` register here"));
    }
    if u32::from(v.n) >= 1 << width_of(field) || index >= 1 << width_of(lane) {
        return w.fail(|| format!("`{}{}[{index}]` has no encoding here", want.letter(), v.n));
    }
    place(&mut w.word, field, u32::from(v.n));
    place(&mut w.word, lane, index);
    w.vec = Some(v);
    w.take();
    Some(())
}

/// `{s0-s3}` or `{d0-d3}`: the first register and how many there are.
fn vfp_list(w: &mut Walk<'_, '_, '_>, kind: u8, first: Field, count: Field) -> Option<()> {
    let Some(op) = w.op().cloned() else {
        return w.fail(|| "expected a vector register list".into());
    };
    let OperandKind::VecList {
        kind: got,
        first: reg,
        count: n,
        lane: None,
        spaced: false,
        all: false,
        degenerate,
    } = op.kind
    else {
        let what = op.describe();
        return w.fail(|| format!("expected a vector register list, found {what}"));
    };
    if degenerate {
        return w.fail(|| "a register range runs from one register to another".into());
    }
    let want = if kind == 0 { VecKind::S } else { VecKind::D };
    if got != want {
        let letter = want.letter();
        return w.fail(|| format!("this list holds `{letter}` registers"));
    }
    if u32::from(reg) >= 1 << width_of(first) || u32::from(n) >= 1 << width_of(count) {
        return w.fail(|| "this register list is too long".into());
    }
    place(&mut w.word, first, u32::from(reg));
    place(&mut w.word, count, u32::from(n));
    w.take();
    Some(())
}

/// `vldr` and `vstr` address `[rn, #±imm8*4]`, and write no base back.
fn vfp_mem(w: &mut Walk<'_, '_, '_>, thumb: bool) -> Option<()> {
    let Some(op) = w.op().cloned() else {
        return w.fail(|| "expected a memory operand".into());
    };
    // `do_neon_ldr_str`: a store through the PC is deprecated in A32 and
    // UNPREDICTABLE in T32, and a label is a PC-relative address however it
    // was written.
    let store_through_pc = thumb && w.word & (1 << 20) == 0;
    if let OperandKind::Imm(e) = op.kind {
        if store_through_pc {
            return w.fail(|| "a store cannot address through `pc` here".into());
        }
        return pcrel_coproc(w, thumb, e, op.span);
    }
    let OperandKind::Mem(mem) = op.kind else {
        let what = op.describe();
        return w.fail(|| format!("expected a memory operand, found {what}"));
    };
    if mem.index != Index::Offset {
        return w.fail(|| "this instruction does not write its base register back".into());
    }
    if store_through_pc && mem.base == reg::PC {
        return w.fail(|| "a store cannot address through `pc` here".into());
    }
    let off = match mem.offset {
        MemOffset::None => 0,
        MemOffset::Imm(v) => v,
        _ => return w.fail(|| "this instruction takes no index register".into()),
    };
    if off % 4 != 0 || off.unsigned_abs() > 1020 {
        return w.fail(|| format!("offset {off} is out of range (-1020 to 1020 in steps of 4)"));
    }
    w.word |= u32::from(mem.base) << 16;
    if off >= 0 {
        w.word |= 1 << 23;
    }
    w.word |= (off.unsigned_abs() / 4) as u32;
    w.take();
    Some(())
}

/// A `vcvt` fixed-point size, which the field holds as `32 - n` or `16 - n`
/// by the bit in the form's own word that says which.
fn vfp_fix(w: &mut Walk<'_, '_, '_>, field: Field) -> Option<()> {
    let from = if w.word & (1 << 7) != 0 { 32 } else { 16 };
    let v = w.constant()?;
    if v < 1 || v > from {
        return w.fail(|| format!("a fixed-point size is 1 to {from}, not {v}"));
    }
    place(&mut w.word, field, (from - v) as u32);
    w.take();
    Some(())
}

/// The memory-barrier options, as their 4-bit encodings.
pub fn barrier_option(name: &str) -> Option<u32> {
    Some(match name {
        "sy" => 15,
        "st" => 14,
        "ish" => 11,
        "ishst" => 10,
        "nsh" | "un" => 7,
        "nshst" | "unst" => 6,
        "osh" => 3,
        "oshst" => 2,
        _ => return None,
    })
}
