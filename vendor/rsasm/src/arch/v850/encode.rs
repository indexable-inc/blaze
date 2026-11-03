//! Matching operands against table entries, and building the bytes.
//!
//! An entry either matches completely or reports a [`Miss`] saying how far it
//! got. The caller tries every entry for the mnemonic and, if none matches,
//! reports the miss that got furthest — which is what makes `mov r1, r2, r3`
//! complain about the third operand rather than about the first operand of
//! the immediate form.
//!
//! Matching does not emit diagnostics, so a failed attempt leaves no trace.
//! That is what lets the caller retry a failed `--arch v850` statement against
//! the RH850 entries, purely to say "this needs RH850" instead of something
//! vaguer.

use super::insn::{Disp, Entry, EpDisp, ImmF, PrepImm, Range, RegF, Slot};
use super::operand::{Arg, ArgKind, Imm, RelFn};
use super::{reg, reloc};
use crate::arch::AsmCtx;
use crate::section::{Fixup, FixupKind, Variant};
use crate::source::Span;

/// Why an entry did not match.
#[derive(Clone, Debug)]
pub struct Miss {
    /// How many operands matched before this one.
    pub progress: usize,
    pub span: Span,
    pub msg: String,
}

/// An instruction being assembled.
pub struct Enc {
    pub bytes: Vec<u8>,
    pub fixups: Vec<Fixup>,
    /// Bytes of the opcode words, before any trailing immediate.
    words: usize,
    bins_pos: i64,
}

impl Enc {
    pub fn new(word: u32, len: u8) -> Enc {
        Enc {
            bytes: word.to_le_bytes()[..len as usize].to_vec(),
            fixups: Vec::new(),
            words: len as usize,
            bins_pos: 0,
        }
    }

    /// ORs bits into words 0 and 1.
    fn or(&mut self, bits: u32) {
        for (i, b) in bits.to_le_bytes().iter().enumerate().take(self.words) {
            self.bytes[i] |= b;
        }
        // Table entries only name fields inside their own words: a 16-bit
        // instruction has no word 1, and whatever an operand appended after
        // word 0 is not one.
        debug_assert!(
            self.words >= 4 || bits >> (8 * self.words) == 0,
            "field outside the instruction"
        );
    }

    /// Applies a scatter function to `size` bytes at `offset`.
    fn patch(&mut self, offset: usize, size: usize, f: fn(u64, i64) -> u64, v: i64) {
        let dst = &mut self.bytes[offset..offset + size];
        let word = crate::arch::Endian::Little.read(dst);
        crate::arch::Endian::Little.write(dst, f(word, v));
    }

    fn append(&mut self, n: usize) {
        self.bytes.resize(self.bytes.len() + n, 0);
    }

    fn fixup(&mut self, offset: usize, imm: &Imm, kind: FixupKind) {
        self.fixups.push(Fixup {
            offset: offset as u32,
            expr: imm.expr,
            kind,
            span: imm.span,
        });
    }

    pub fn finish(self) -> Variant {
        Variant {
            bytes: self.bytes,
            fixups: self.fixups,
        }
    }
}

/// An immediate's value, if assembly time already knows it.
enum Val {
    Known(i64),
    Symbolic,
}

struct Matcher<'c, 'a> {
    cx: &'c AsmCtx<'a>,
    enc: Enc,
    mnemonic: &'static str,
    progress: usize,
    rh850: bool,
}

type R<T> = Result<T, Miss>;

/// Tries one entry against the operands.
pub fn match_entry(
    cx: &AsmCtx<'_>,
    e: &'static Entry,
    args: &[Arg],
    rh850: bool,
    whole: Span,
) -> R<Enc> {
    let mut m = Matcher {
        cx,
        enc: Enc::new(e.word, e.len),
        mnemonic: e.name,
        progress: 0,
        rh850,
    };
    for (i, slot) in e.slots.iter().enumerate() {
        m.progress = i;
        let Some(arg) = args.get(i) else {
            let at = args.last().map_or(whole, |a| a.span);
            return Err(m.miss(
                at,
                format!(
                    "`{}` needs {} operand(s), but {} were given",
                    e.name,
                    e.slots.len(),
                    args.len()
                ),
            ));
        };
        m.slot(slot, arg)?;
    }
    if let Some(extra) = args.get(e.slots.len()) {
        m.progress = e.slots.len();
        return Err(m.miss(
            extra.span,
            format!(
                "unexpected operand; this form of `{}` takes {}",
                e.name,
                e.slots.len()
            ),
        ));
    }
    Ok(m.enc)
}

/// Register-list bits, indexed by instruction bit.
///
/// The list field of `prepare` and `dispose` is bit 0 of word 0 plus bits
/// 31..21 of the two-word value. The silicon assigns registers to those bits
/// in the order the instruction pushes them, which has nothing to do with
/// register numbers: bit 0 is r30, bit 21 is r31, then r29, r28, r23, r22,
/// r21, r20, r27, r26, r25 and r24. Anything outside r20-r31 cannot be saved
/// this way at all.
const LIST_BITS: [(u8, u32); 12] = [
    (30, 0),
    (31, 21),
    (29, 22),
    (28, 23),
    (23, 24),
    (22, 25),
    (21, 26),
    (20, 27),
    (27, 28),
    (26, 29),
    (25, 30),
    (24, 31),
];

impl Matcher<'_, '_> {
    fn miss(&self, span: Span, msg: impl Into<String>) -> Miss {
        Miss {
            progress: self.progress,
            span,
            msg: msg.into(),
        }
    }

    fn slot(&mut self, slot: &Slot, arg: &Arg) -> R<()> {
        match *slot {
            Slot::Reg(f) => {
                let ArgKind::Reg(r) = arg.kind else {
                    return Err(self.expected(arg, "a register"));
                };
                self.reg(f, r, arg.span)
            }
            Slot::Base(f) => match arg.kind {
                ArgKind::Bracket(r) | ArgKind::Reg(r) => self.reg(f, r, arg.span),
                _ => Err(self.expected(arg, "a register in brackets, such as `[r1]`")),
            },
            Slot::Mem(d, f) => {
                let ArgKind::Mem { disp, base } = arg.kind else {
                    return Err(
                        self.expected(arg, "a displacement and base register, such as `4[r1]`")
                    );
                };
                self.reg(f, base, arg.span)?;
                self.disp(d, &disp)
            }
            Slot::Ep(d) => {
                let ArgKind::Mem { disp, base } = arg.kind else {
                    return Err(self.expected(arg, "a displacement from ep, such as `4[ep]`"));
                };
                if base != reg::EP {
                    return Err(self.miss(
                        arg.span,
                        format!(
                            "`{}` addresses memory relative to ep only, not `{}`",
                            self.mnemonic,
                            reg::gpr_name(base)
                        ),
                    ));
                }
                self.ep_disp(d, &disp)
            }
            Slot::Imm(k) => {
                let ArgKind::Imm(imm) = arg.kind else {
                    return Err(self.expected(arg, "an immediate"));
                };
                self.imm(k, &imm)
            }
            Slot::Cond { shift, allow_sa } => {
                let v = self.named(arg, "condition", 15, reg::condition)?;
                if !allow_sa && v == reg::COND_SA as i64 {
                    return Err(self.miss(
                        arg.span,
                        format!("`{}` cannot test the `sa` condition", self.mnemonic),
                    ));
                }
                self.enc.or((v as u32) << shift);
                Ok(())
            }
            Slot::FloatCond => {
                let v = self.named(arg, "floating-point condition", 15, reg::float_condition)?;
                self.enc.or((v as u32) << 27);
                Ok(())
            }
            Slot::Fff => {
                let v = self.plain(arg, "condition flag", 0, 7)?;
                self.enc.or((v as u32) << 17);
                Ok(())
            }
            Slot::OldSysReg { shift } => {
                let v = self.named(arg, "system register", 31, |n| reg::sysreg(n, false))?;
                self.enc.or((v as u32) << shift);
                Ok(())
            }
            Slot::SysReg { shift } => {
                let rh850 = self.rh850;
                let v = self.named(arg, "system register", 1023, |n| reg::sysreg(n, rh850))?;
                // A number above 31 carries a group: `selID * 32 + regID`.
                self.enc
                    .or(((v as u32 & 0x1f) << shift) | ((v as u32 >> 5) << 27));
                Ok(())
            }
            Slot::SelId => {
                let v = self.plain(arg, "system register group (selID)", 0, 31)?;
                self.enc.or((v as u32) << 27);
                Ok(())
            }
            Slot::VReg { shift } => {
                let r = match arg.kind {
                    ArgKind::Imm(Imm { ident: Some(n), .. }) => reg::vector_reg(self.cx.name(n)),
                    _ => None,
                };
                let Some(r) = r else {
                    return Err(self.expected(arg, "a virtualisation register, `vr0` to `vr31`"));
                };
                self.enc.or((r as u32) << shift);
                Ok(())
            }
            Slot::CacheOp => {
                let v = self.named(arg, "cache operation", 127, reg::cache_op)?;
                self.enc
                    .or((((v as u32) & 0x60) >> 5) << 11 | ((v as u32) & 0x1f) << 27);
                Ok(())
            }
            Slot::PrefOp => {
                let v = self.named(arg, "prefetch operation", 31, reg::pref_op)?;
                self.enc.or((v as u32) << 27);
                Ok(())
            }
            Slot::List => self.list(arg),
            Slot::Sp => match arg.kind {
                ArgKind::Reg(reg::SP) => Ok(()),
                _ => Err(self.expected(arg, "`sp`")),
            },
            Slot::PrepImm(k) => {
                let ArgKind::Imm(imm) = arg.kind else {
                    return Err(self.expected(arg, "an immediate"));
                };
                self.prep_imm(k, &imm)
            }
            Slot::BinsPos => {
                let v = self.plain(arg, "bit position", 0, 31)?;
                self.enc.bins_pos = v;
                Ok(())
            }
            Slot::BinsWidth => {
                let width = self.plain(arg, "bit-field width", 1, 32)?;
                let lsb = self.enc.bins_pos;
                let msb = lsb + width - 1;
                if msb > 31 {
                    return Err(self.miss(
                        arg.span,
                        format!("a {width}-bit field at bit {lsb} runs past bit 31"),
                    ));
                }
                // Three opcodes cover a field wholly in the upper half, one
                // straddling bit 16, and one wholly in the lower half; each
                // stores msb and lsb modulo 16. The lsb's four bits are split
                // around the opcode: bit 3 at bit 27, bits 2..0 at 18..16.
                let opc: u32 = if lsb >= 16 {
                    0x0090
                } else if msb >= 16 {
                    0x00b0
                } else {
                    0x00d0
                };
                let (msb, lsb) = (msb as u32 & 0xf, lsb as u32 & 0xf);
                self.enc
                    .or((opc | msb << 12 | (lsb & 8) << 8 | (lsb & 7) << 1) << 16);
                Ok(())
            }
        }
    }

    fn expected(&self, arg: &Arg, what: &str) -> Miss {
        self.miss(
            arg.span,
            format!("expected {what}, but found {}", arg.describe()),
        )
    }

    fn reg(&mut self, f: RegF, r: u8, span: Span) -> R<()> {
        if f.not_r0 && r == 0 {
            return Err(self.miss(span, "register r0 cannot be used here"));
        }
        if f.even && !r.is_multiple_of(2) {
            return Err(self.miss(
                span,
                format!(
                    "`{}` is odd; a double-precision operand names the even register of a pair",
                    reg::gpr_name(r)
                ),
            ));
        }
        self.enc.or((r as u32) << f.shift);
        Ok(())
    }

    /// The value of an immediate that no relocation can carry: it must be
    /// known now.
    fn constant(&self, imm: &Imm, what: &str) -> R<i64> {
        if imm.func != RelFn::None {
            return Err(self.unsupported(imm, &format!("the {what}")));
        }
        match self.cx.constant(imm.expr) {
            Some(v) => Ok(v),
            None => Err(self.miss(imm.span, format!("the {what} must be a constant"))),
        }
    }

    fn value(&self, imm: &Imm) -> Val {
        match self.cx.constant(imm.expr) {
            Some(v) => Val::Known(imm.func.apply(v)),
            None => Val::Symbolic,
        }
    }

    fn range(&self, span: Span, what: &str, v: i64, lo: i64, hi: i64) -> R<()> {
        if v < lo || v > hi {
            return Err(self.miss(span, format!("{what} {v} is out of range ({lo} to {hi})")));
        }
        Ok(())
    }

    fn aligned(&self, span: Span, what: &str, v: i64, align: i64) -> R<()> {
        if v % align != 0 {
            return Err(self.miss(span, format!("{what} {v} is not a multiple of {align}")));
        }
        Ok(())
    }

    /// A plain constant operand in `lo..=hi`.
    fn plain(&self, arg: &Arg, what: &str, lo: i64, hi: i64) -> R<i64> {
        let ArgKind::Imm(imm) = arg.kind else {
            return Err(self.expected(arg, &format!("a {what}")));
        };
        let v = self.constant(&imm, what)?;
        self.range(imm.span, what, v, lo, hi)?;
        Ok(v)
    }

    /// A name from `lookup`, or a number in `0..=max`.
    fn named(
        &self,
        arg: &Arg,
        what: &str,
        max: i64,
        lookup: impl Fn(&str) -> Option<u8>,
    ) -> R<i64> {
        let ArgKind::Imm(imm) = arg.kind else {
            return Err(self.expected(arg, &format!("a {what}")));
        };
        if let Some(n) = imm.ident
            && let Some(v) = lookup(self.cx.name(n))
        {
            return Ok(v as i64);
        }
        if imm.func == RelFn::None
            && let Some(v) = self.cx.constant(imm.expr)
        {
            self.range(imm.span, what, v, 0, max)?;
            return Ok(v);
        }
        let shown = imm
            .ident
            .map(|n| format!(" `{}`", self.cx.name(n)))
            .unwrap_or_default();
        Err(self.miss(imm.span, format!("unknown {what}{shown}")))
    }

    fn list(&mut self, arg: &Arg) -> R<()> {
        let mask = match arg.kind {
            ArgKind::List(m) => m,
            // A number stands for the list too, bit n meaning r(20+n).
            ArgKind::Imm(imm) => {
                let v = self.constant(&imm, "register list")?;
                if !(0..=0xfff).contains(&v) {
                    return Err(self.miss(
                        imm.span,
                        format!("register list {v:#x} has bits above bit 11 set; only r20-r31 can be listed"),
                    ));
                }
                (v as u32) << 20
            }
            _ => return Err(self.expected(arg, "a register list such as `{r20-r29, r31}`")),
        };
        let mut bits = 0u32;
        for (r, bit) in LIST_BITS {
            if mask & (1 << r) != 0 {
                bits |= 1 << bit;
            }
        }
        self.enc.or(bits);
        Ok(())
    }

    /// Rejects relocation functions the RH850 ABI cannot express, with the
    /// reason, so the caller only has to handle the ones it can.
    fn unsupported(&self, imm: &Imm, field: &str) -> Miss {
        let why = match imm.func {
            RelFn::SdaOff | RelFn::TdaOff => {
                "; the RH850 ABI has no relocation for it here, and GNU as silently \
                 substitutes an absolute one"
            }
            RelFn::CtOff => "; the RH850 ABI has no `callt` table relocation",
            RelFn::HiLo => "; it needs a 32-bit field",
            RelFn::Lo23 => "; it needs a 23-bit displacement",
            _ => "",
        };
        self.miss(
            imm.span,
            format!("`{}` cannot be used for {field}{why}", imm.func.spelling()),
        )
    }

    fn imm(&mut self, k: ImmF, imm: &Imm) -> R<()> {
        match k {
            ImmF::Bits { shift, bits, range } => {
                let what = "immediate";
                let v = self.constant(imm, what)?;
                let (lo, hi) = match range {
                    Range::Either => (-(1i64 << (bits - 1)), (1i64 << bits) - 1),
                    Range::Unsigned => (0, (1i64 << bits) - 1),
                    Range::SignedOrWider => (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1),
                };
                self.range(imm.span, what, v, lo, hi)?;
                self.enc.or(((v as u32) & ((1u32 << bits) - 1)) << shift);
                Ok(())
            }
            ImmF::NonZero { shift, bits } => {
                let v = self.constant(imm, "immediate")?;
                self.range(imm.span, "immediate", v, 1, (1i64 << bits) - 1)?;
                self.enc.or((v as u32) << shift);
                Ok(())
            }
            ImmF::Imm16 => self.imm16(imm),
            ImmF::Imm9 { signed } => {
                let (lo, hi) = if signed { (-256, 255) } else { (0, 511) };
                let v = self.constant(imm, "immediate")?;
                self.range(imm.span, "immediate", v, lo, hi)?;
                // Bits 4..0 go below reg1's position in word 0 and bits 8..5
                // at 21..18, either side of the opcode.
                let v = v as u32;
                self.enc.or((v & 0x1f) | ((v & 0x1e0) << 13));
                Ok(())
            }
            ImmF::Vector8 => {
                let v = self.constant(imm, "vector")?;
                self.range(imm.span, "vector", v, 0, 255)?;
                let v = v as u32;
                self.enc.or((v & 0x1f) | ((v & 0xe0) << 22));
                Ok(())
            }
            ImmF::Imm10 => {
                let v = self.constant(imm, "immediate")?;
                self.range(imm.span, "immediate", v, 0, 1023)?;
                let v = v as u32;
                self.enc.or((v & 0x1f) | ((v >> 5) & 0x1f) << 27);
                Ok(())
            }
            ImmF::Imm32 => {
                let at = self.enc.bytes.len();
                match imm.func {
                    RelFn::None | RelFn::HiLo => {}
                    _ => return Err(self.unsupported(imm, "a 32-bit immediate")),
                }
                match (self.value(imm), imm.func) {
                    (Val::Known(v), _) => {
                        self.range(imm.span, "immediate", v, i32::MIN as i64, u32::MAX as i64)?;
                        self.enc.append(4);
                        self.enc.patch(at, 4, reloc::imm32, v);
                    }
                    (Val::Symbolic, RelFn::HiLo) => {
                        self.enc.append(4);
                        self.enc
                            .fixup(at, imm, FixupKind::data(4).with_reloc(reloc::WORD));
                    }
                    (Val::Symbolic, _) => {
                        // GNU as refuses this too; `hilo()` says which 32 bits
                        // are meant, and matches what `movhi`/`movea` pairs
                        // spell as `hi()`/`lo()`.
                        return Err(self.miss(
                            imm.span,
                            format!(
                                "`{}` takes a constant; write `hilo(sym)` to load a symbol's address",
                                self.mnemonic
                            ),
                        ));
                    }
                }
                Ok(())
            }
            ImmF::Disp22 => {
                if imm.func != RelFn::None {
                    return Err(self.unsupported(imm, "a branch displacement"));
                }
                match self.cx.constant(imm.expr) {
                    // A number is the displacement itself, as in GNU as; only
                    // a symbol is an address to branch to.
                    Some(v) => {
                        self.range(imm.span, "displacement", v, -0x20_0000, 0x1f_ffff)?;
                        self.aligned(imm.span, "displacement", v, 2)?;
                        self.enc.patch(0, 4, reloc::disp22, v);
                    }
                    None => self.enc.fixup(
                        0,
                        imm,
                        FixupKind::pcrel(4, 0)
                            .with_field(22, 2)
                            .with_reloc(reloc::PCR22)
                            .scatter(reloc::disp22),
                    ),
                }
                Ok(())
            }
            ImmF::Disp32 => {
                // Only reached for numbers: a symbol always takes the 22-bit
                // form, which is what GNU as does.
                let v = self.constant(imm, "displacement")?;
                self.range(
                    imm.span,
                    "displacement",
                    v,
                    i32::MIN as i64,
                    i32::MAX as i64,
                )?;
                self.aligned(imm.span, "displacement", v, 2)?;
                let at = self.enc.bytes.len();
                self.enc.append(4);
                self.enc.patch(at, 4, reloc::imm32, v);
                Ok(())
            }
        }
    }

    /// The 16-bit immediate in word 1.
    fn imm16(&mut self, imm: &Imm) -> R<()> {
        let (reloc, scatter): (u32, fn(u64, i64) -> u64) = match imm.func {
            RelFn::None => (reloc::HWORD, reloc::imm16),
            RelFn::Lo => (reloc::WLO, reloc::lo16),
            RelFn::ZdaOff => (reloc::HWORD, reloc::lo16),
            RelFn::Hi => (reloc::WHI1, reloc::hi16),
            RelFn::Hi0 => (reloc::WHI, reloc::hi0_16),
            _ => return Err(self.unsupported(imm, "a 16-bit immediate")),
        };
        match self.value(imm) {
            Val::Known(v) => {
                self.range(imm.span, "immediate", v, -0x8000, 0xffff)?;
                self.enc.patch(2, 2, reloc::imm16, v);
            }
            Val::Symbolic => {
                let kind = if imm.func == RelFn::None {
                    FixupKind::data(2).with_reloc(reloc)
                } else if imm.func == RelFn::ZdaOff {
                    // An offset from address 0 is the address itself, which
                    // GNU ld requires to fit a signed 16-bit field
                    // (`R_V810_HWORD` in bfd/elf32-v850.c), rather than
                    // truncating it the way `lo()` is.
                    FixupKind::data(2).signed().with_reloc(reloc)
                } else {
                    // The function picks bits out of a wider value, so the
                    // value itself is not range-checked.
                    FixupKind::data(2)
                        .with_field(64, 1)
                        .with_reloc(reloc)
                        .scatter(scatter)
                };
                self.enc.fixup(2, imm, kind);
            }
        }
        Ok(())
    }

    fn disp(&mut self, d: Disp, imm: &Imm) -> R<()> {
        match d {
            Disp::D16 { strict } => {
                let (reloc, scatter): (u32, fn(u64, i64) -> u64) = match imm.func {
                    RelFn::None => (reloc::HWORD, reloc::imm16),
                    RelFn::Lo => (reloc::WLO, reloc::lo16),
                    RelFn::ZdaOff => (reloc::HWORD, reloc::lo16),
                    _ => return Err(self.unsupported(imm, "a 16-bit displacement")),
                };
                match self.value(imm) {
                    Val::Known(v) => {
                        let hi = if strict { 0x7fff } else { 0xffff };
                        self.range(imm.span, "displacement", v, -0x8000, hi)?;
                        self.enc.patch(2, 2, reloc::imm16, v);
                    }
                    Val::Symbolic => {
                        // `ld.b` and `st.b` read the displacement as signed;
                        // the bit instructions accept either reading, as
                        // they do for a number.
                        let kind = if imm.func == RelFn::None && strict {
                            FixupKind::data(2).signed().with_reloc(reloc)
                        } else if imm.func == RelFn::None {
                            FixupKind::data(2).with_reloc(reloc)
                        } else {
                            FixupKind::data(2)
                                .with_field(64, 1)
                                .with_reloc(reloc)
                                .scatter(scatter)
                        };
                        self.enc.fixup(2, imm, kind);
                    }
                }
                Ok(())
            }
            Disp::D16Even => {
                match imm.func {
                    RelFn::None | RelFn::Lo => {}
                    _ => return Err(self.unsupported(imm, "this displacement")),
                }
                match self.value(imm) {
                    Val::Known(v) => {
                        self.range(imm.span, "displacement", v, -0x8000, 0x7fff)?;
                        self.aligned(imm.span, "displacement", v, 2)?;
                        self.enc.patch(2, 2, reloc::even16, v);
                    }
                    Val::Symbolic => {
                        let kind = if imm.func == RelFn::Lo {
                            FixupKind::data(2)
                                .with_field(64, 2)
                                .with_reloc(reloc::WLO_1)
                        } else {
                            // No RH850 relocation for a bare symbol here.
                            FixupKind::data(2).signed().with_field(16, 2)
                        };
                        self.enc.fixup(2, imm, kind.scatter(reloc::even16));
                    }
                }
                Ok(())
            }
            Disp::D16Split => {
                match imm.func {
                    RelFn::None | RelFn::Lo => {}
                    _ => return Err(self.unsupported(imm, "this displacement")),
                }
                match self.value(imm) {
                    Val::Known(v) => {
                        self.range(imm.span, "displacement", v, -0x8000, 0x7fff)?;
                        self.enc.patch(0, 4, reloc::split16, v);
                    }
                    Val::Symbolic => {
                        let kind = if imm.func == RelFn::Lo {
                            FixupKind::data(4).with_field(64, 1).with_reloc(reloc::BLO)
                        } else {
                            FixupKind::data(4).signed().with_field(16, 1)
                        };
                        self.enc.fixup(0, imm, kind.scatter(reloc::split16));
                    }
                }
                Ok(())
            }
            Disp::D23 { even } => {
                match imm.func {
                    RelFn::None | RelFn::Lo23 => {}
                    _ => return Err(self.unsupported(imm, "a 23-bit displacement")),
                }
                let scatter = if even {
                    reloc::disp23_even
                } else {
                    reloc::disp23
                };
                let align = if even { 2 } else { 1 };
                self.enc.append(2);
                match self.value(imm) {
                    Val::Known(v) => {
                        // `lo23()` asks for truncation, so only a bare value
                        // is range-checked.
                        if imm.func == RelFn::None {
                            self.range(imm.span, "displacement", v, -0x40_0000, 0x3f_ffff)?;
                        }
                        self.aligned(imm.span, "displacement", v, align)?;
                        self.enc.patch(2, 4, scatter, v);
                    }
                    Val::Symbolic => {
                        let bits = if imm.func == RelFn::None { 23 } else { 64 };
                        let kind = FixupKind::data(4)
                            .signed()
                            .with_field(bits, align as u8)
                            .with_reloc(reloc::WLO23)
                            .scatter(scatter);
                        self.enc.fixup(2, imm, kind);
                    }
                }
                Ok(())
            }
            Disp::D32 => {
                let v = self.constant(imm, "32-bit displacement")?;
                self.range(
                    imm.span,
                    "displacement",
                    v,
                    i32::MIN as i64,
                    u32::MAX as i64,
                )?;
                self.aligned(imm.span, "displacement", v, 2)?;
                let at = self.enc.bytes.len();
                self.enc.append(4);
                self.enc.patch(at, 4, reloc::imm32, v);
                Ok(())
            }
        }
    }

    fn ep_disp(&mut self, d: EpDisp, imm: &Imm) -> R<()> {
        let v = self.constant(imm, "displacement")?;
        let s = imm.span;
        let bits = match d {
            EpDisp::D7 => {
                self.range(s, "displacement", v, -64, 127)?;
                v & 0x7f
            }
            EpDisp::D8Half => {
                self.range(s, "displacement", v, 0, 254)?;
                self.aligned(s, "displacement", v, 2)?;
                v >> 1
            }
            EpDisp::D8Word => {
                self.range(s, "displacement", v, 0, 252)?;
                self.aligned(s, "displacement", v, 4)?;
                v >> 1
            }
            EpDisp::D4 => {
                self.range(s, "displacement", v, -8, 15)?;
                v & 0xf
            }
            EpDisp::D5Half => {
                self.range(s, "displacement", v, 0, 30)?;
                self.aligned(s, "displacement", v, 2)?;
                v >> 1
            }
        };
        self.enc.or(bits as u32);
        Ok(())
    }

    /// `prepare`'s value for ep. Each of the three forms refuses what a later
    /// one can hold, so a value lands in the narrowest.
    fn prep_imm(&mut self, k: PrepImm, imm: &Imm) -> R<()> {
        let at = self.enc.bytes.len();
        match k {
            PrepImm::Lo => {
                match imm.func {
                    RelFn::None | RelFn::Lo => {}
                    _ => return Err(self.unsupported(imm, "a sign-extended 16-bit value")),
                }
                match self.value(imm) {
                    Val::Known(v) => {
                        self.range(imm.span, "value", v, i32::MIN as i64, u32::MAX as i64)?;
                        // The CPU sign-extends, and the value is a 32-bit one.
                        if v as u32 as i32 != v as u32 as i16 as i32 {
                            return Err(self.miss(
                                imm.span,
                                format!("value {v:#x} does not fit in 16 bits sign-extended"),
                            ));
                        }
                        self.enc.append(2);
                        self.enc.patch(at, 2, reloc::imm16, v);
                    }
                    Val::Symbolic => {
                        self.enc.append(2);
                        self.enc.fixup(
                            at,
                            imm,
                            FixupKind::data(2)
                                .with_field(64, 1)
                                .with_reloc(reloc::WLO)
                                .scatter(reloc::lo16),
                        );
                    }
                }
            }
            PrepImm::Hi => {
                let (reloc, scatter): (u32, fn(u64, i64) -> u64) = match imm.func {
                    RelFn::None => (0, reloc::imm16),
                    RelFn::Hi => (reloc::WHI1, reloc::hi16),
                    RelFn::Hi0 => (reloc::WHI, reloc::hi0_16),
                    _ => return Err(self.unsupported(imm, "a shifted 16-bit value")),
                };
                match self.value(imm) {
                    Val::Known(v) => {
                        let stored = if imm.func == RelFn::None {
                            if v as u32 & 0xffff != 0 {
                                return Err(self.miss(
                                    imm.span,
                                    format!("value {v:#x} has low bits set, so it is not a shifted 16-bit value"),
                                ));
                            }
                            (v as u32 >> 16) as i64
                        } else {
                            v
                        };
                        self.enc.append(2);
                        self.enc.patch(at, 2, reloc::imm16, stored);
                    }
                    Val::Symbolic if reloc != 0 => {
                        self.enc.append(2);
                        self.enc.fixup(
                            at,
                            imm,
                            FixupKind::data(2)
                                .with_field(64, 1)
                                .with_reloc(reloc)
                                .scatter(scatter),
                        );
                    }
                    Val::Symbolic => {
                        return Err(self.miss(imm.span, "the value must be a constant"));
                    }
                }
            }
            PrepImm::Full => {
                match imm.func {
                    RelFn::None | RelFn::HiLo => {}
                    _ => return Err(self.unsupported(imm, "a 32-bit value")),
                }
                self.enc.append(4);
                match self.value(imm) {
                    Val::Known(v) => {
                        self.range(imm.span, "value", v, i32::MIN as i64, u32::MAX as i64)?;
                        self.enc.patch(at, 4, reloc::imm32, v);
                    }
                    Val::Symbolic => {
                        self.enc
                            .fixup(at, imm, FixupKind::data(4).with_reloc(reloc::WORD));
                    }
                }
            }
        }
        Ok(())
    }
}
