//! Turning a matched instruction definition into its 32-bit word.
//!
//! Everything here works on one word. Bit positions are given the way the
//! PowerPC manuals give them — numbered from the most significant bit, so
//! "bits 16:20" is the five-bit field whose least significant bit is word bit
//! 11 — and `at` does that conversion once, so the rest of the file can name
//! fields the way the manual does.

use super::insn::{Def, F, OPT1, OPTL, Resolved, Rot, Rot2, VRC};
use super::operand::{Mem, Operand, OperandKind, Value};
use super::reg::{RegClass, describe};
use super::reloc;
use crate::arch::{AsmCtx, Endian};
use crate::expr::{BinOp, ExprKind, ExprRef};
use crate::section::{Fixup, FixupKind, LinkValue, RelocSymbol, Variant};
use crate::source::Span;

/// The shift that puts a field whose manual bit range ends at `last` — counted
/// from the most significant bit of the word — in the right place.
const fn at(last: u32) -> u32 {
    31 - last
}

const RT: u32 = at(10); // RT / RS / BO / TO / crbD / VRT
const RA: u32 = at(15); // RA / BI / crbA / VRA
const RB: u32 = at(20); // RB / SH / crbB / VRB
const FRC: u32 = at(25); // FRC, MB of an M-form rotate, VRC
const ME: u32 = at(30);
const CRFD: u32 = at(8);
const CRFS: u32 = at(13);
const L_BIT: u32 = at(10);
const BH: u32 = at(20); // two bits at 19:20
const CRM: u32 = at(19); // eight bits at 12:19
const FLM: u32 = at(14); // eight bits at 7:14
const SPRF: u32 = at(20); // ten bits at 11:20
const LEV: u32 = at(26); // seven bits at 20:26

/// The `o` suffix sets OE, bit 21.
const OE_BIT: u64 = 1 << at(21);
/// The `.` suffix sets Rc, bit 31, except on the vector compares, where the
/// record bit is bit 21 and the result goes to CR6.
const RC_BIT: u64 = 1 << at(31);
const VRC_BIT: u64 = 1 << at(21);

// The VSX extension bits. A VSX register number is six bits: five in the
// same slot a VR would use, and the sixth down here, one bit per slot.
const TX: u32 = at(31);
const AX: u32 = at(29);
const BX: u32 = at(30);
const CX: u32 = at(28);
/// The DQ-form load and store target keeps its sixth bit at 28 instead, and
/// the 8RR-form prefixed target at 15.
const TX_DQ: u32 = at(28);
const TX_8RR: u32 = at(15);
/// A VSX register *pair* is named by its even first register, so the slot
/// holds four bits and the sixth bit moves up to 10.
const TX_PAIR: u32 = at(10);

/// The R bit of a prefixed instruction: bit 11 of the prefix word, which is
/// bit 52 of the pair read as one 64-bit value.
const PFX_R: u32 = 32 + at(11);

pub struct Encoder<'c, 'a> {
    cx: &'c mut AsmCtx<'a>,
    endian: Endian,
    /// The instruction word. A prefixed instruction keeps its prefix in the
    /// upper 32 bits, so one value covers both halves and a field can sit in
    /// either.
    word: u64,
    fixups: Vec<Fixup>,
    failed: bool,
    /// The table entry's name, and whether a `.` or `o` suffix was taken off
    /// it: the thread-local markers go only on the instructions a linker
    /// knows how to rewrite, which it recognises by their exact encoding.
    mnemonic: &'static str,
    suffixed: bool,
}

impl<'c, 'a> Encoder<'c, 'a> {
    pub fn new(cx: &'c mut AsmCtx<'a>, endian: Endian) -> Encoder<'c, 'a> {
        Encoder {
            cx,
            endian,
            word: 0,
            fixups: Vec::new(),
            failed: false,
            mnemonic: "",
            suffixed: false,
        }
    }

    /// Encodes one instruction, or reports why it cannot be encoded.
    pub fn encode(mut self, r: &Resolved, ops: &[Operand], span: Span) -> Option<Variant> {
        self.word = r.def.word;
        self.mnemonic = r.def.name;
        self.suffixed = r.rc || r.oe;
        if r.rc {
            self.word |= if r.def.flags & VRC != 0 {
                VRC_BIT
            } else {
                RC_BIT
            };
        }
        if r.oe {
            self.word |= OE_BIT;
        }

        let groups = self.split_operands(r.def, ops, span)?;
        for (f, group) in groups {
            self.field(f, group);
        }
        if r.def.ops.contains(&F::Pcrel) {
            self.check_pcrel(span);
        }
        if self.failed {
            return None;
        }
        // A prefixed instruction is two words, the prefix first, each written
        // in the target's byte order on its own.
        let bytes = if prefixed(r.def) {
            let mut b = self.endian.bytes(self.word >> 32, 4);
            b.extend_from_slice(&self.endian.bytes(self.word & 0xffff_ffff, 4));
            b
        } else {
            self.endian.bytes(self.word, 4)
        };
        Some(Variant {
            bytes,
            fixups: self.fixups,
        })
    }

    /// Hands each pattern field the operands it consumes, honouring the forms
    /// whose first or last operand may be left out.
    fn split_operands<'o>(
        &mut self,
        def: &Def,
        ops: &'o [Operand],
        span: Span,
    ) -> Option<Vec<(F, &'o [Operand])>> {
        let full: usize = def.ops.iter().copied().map(arity).sum();
        let short = full.saturating_sub(1);
        let pat = if ops.len() == full {
            def.ops
        } else if ops.len() == short && def.flags & OPT1 != 0 {
            def.ops.split_first().map_or(&[][..], |(_, rest)| rest)
        } else if ops.len() == short && def.flags & OPTL != 0 {
            def.ops.split_last().map_or(&[][..], |(_, rest)| rest)
        } else {
            let want = if def.flags & (OPT1 | OPTL) != 0 {
                format!("{short} or {full}")
            } else {
                full.to_string()
            };
            self.cx.error(
                span,
                format!(
                    "`{}` takes {want} operand(s), but {} were given",
                    def.name,
                    ops.len()
                ),
            );
            return None;
        };

        let mut out = Vec::with_capacity(pat.len());
        let mut rest = ops;
        for f in pat {
            let (group, tail) = rest.split_at(arity(*f).min(rest.len()));
            out.push((*f, group));
            rest = tail;
        }
        Some(out)
    }

    fn field(&mut self, f: F, group: &[Operand]) {
        let Some(op) = group.first() else {
            // `split_operands` guarantees the counts line up.
            return;
        };
        match f {
            F::Rt => {
                let v = self.gpr(op);
                self.put(RT, v);
            }
            F::Ra => {
                let v = self.gpr(op);
                self.put(RA, v);
            }
            F::Rb => {
                let v = match self.tls_operand(op) {
                    Some(e) => self.tls_register(op, e),
                    None => self.gpr(op),
                };
                self.put(RB, v);
            }
            F::RtRb => {
                let v = self.gpr(op);
                self.put(RT, v);
                self.put(RB, v);
            }
            F::Ft => {
                let v = self.fpr(op);
                self.put(RT, v);
            }
            F::Fa => {
                let v = self.fpr(op);
                self.put(RA, v);
            }
            F::Fb => {
                let v = self.fpr(op);
                self.put(RB, v);
            }
            F::Fc => {
                let v = self.fpr(op);
                self.put(FRC, v);
            }
            F::Simm => self.imm16(op, Range16::Signed),
            F::SimmU => self.imm16(op, Range16::Either),
            F::Uimm => self.imm16(op, Range16::Unsigned),
            F::NegSimm => match self.constant(op, "immediate") {
                // `subi rD, rA, v` is `addi rD, rA, -v`, so the range is the
                // signed one mirrored.
                Some(v) if (-32767..=32768).contains(&v) => self.word |= (-v) as u64 & 0xffff,
                Some(v) => self.reject(
                    op,
                    format!("immediate {v} is out of range: must be -32767 to 32768"),
                ),
                None => {}
            },
            F::Sh5 => {
                let v = self.small(op, 31, "shift count");
                self.put(RB, v);
            }
            F::Mb5 => {
                let v = self.small(op, 31, "mask begin");
                self.put(FRC, v);
            }
            F::Me5 => {
                let v = self.small(op, 31, "mask end");
                self.put(ME, v);
            }
            F::Sh6 => {
                if let Some(v) = self.small(op, 63, "shift count") {
                    self.word |= md_sh(v) as u64;
                }
            }
            F::M6 => {
                if let Some(v) = self.small(op, 63, "mask bound") {
                    self.word |= md_m(v) as u64;
                }
            }
            F::CrfD => {
                let v = self.crf(op);
                self.put(CRFD, v);
            }
            F::CrfS => {
                let v = self.crf(op);
                self.put(CRFS, v);
            }
            F::CrbD => {
                let v = self.small(op, 31, "CR bit");
                self.put(RT, v);
            }
            F::CrbA => {
                let v = self.small(op, 31, "CR bit");
                self.put(RA, v);
            }
            F::CrbB => {
                let v = self.small(op, 31, "CR bit");
                self.put(RB, v);
            }
            F::CrbAB => {
                let v = self.small(op, 31, "CR bit");
                self.put(RA, v);
                self.put(RB, v);
            }
            F::CrbAll => {
                let v = self.small(op, 31, "CR bit");
                self.put(RT, v);
                self.put(RA, v);
                self.put(RB, v);
            }
            // The definition already holds the BI of the condition *within* a
            // CR field; naming a field moves it up by four bits per field.
            F::CrfBi => {
                if let Some(n) = self.crf(op) {
                    self.word |= ((n * 4) as u64) << RA;
                }
            }
            F::Bo => {
                let v = self.small(op, 31, "BO");
                self.put(RT, v);
            }
            F::Bi => {
                let v = self.small(op, 31, "BI");
                self.put(RA, v);
            }
            F::Bh => {
                let v = self.small(op, 3, "branch hint");
                self.put(BH, v);
            }
            F::To => {
                let v = self.small(op, 31, "trap condition");
                self.put(RT, v);
            }
            F::Crm => {
                let v = self.small(op, 255, "CR mask");
                self.put(CRM, v);
            }
            F::Flm => {
                let v = self.small(op, 255, "FPSCR field mask");
                self.put(FLM, v);
            }
            F::L => {
                let v = self.small(op, 1, "comparison width");
                self.put(L_BIT, v);
            }
            F::Lev => {
                let v = self.small(op, 127, "system call level");
                self.put(LEV, v);
            }
            F::Spr => {
                if let Some(n) = self.spr(op) {
                    // The ten-bit SPR number is stored with its two five-bit
                    // halves swapped, a quirk inherited from POWER.
                    self.word |= ((((n & 0x1f) << 5) | (n >> 5)) as u64) << SPRF;
                }
            }
            F::MemD => self.mem(op, Disp::D),
            F::MemDS => self.mem(op, Disp::Ds),
            F::Rel24 => self.branch(op, 26, true),
            F::Abs24 => self.branch(op, 26, false),
            F::Rel14 => self.branch(op, 16, true),
            F::Abs14 => self.branch(op, 16, false),
            F::RotN(kind) => {
                if let Some(n) = self.constant(op, "shift count") {
                    let r = rot1(kind, n);
                    self.rotate(op, r);
                }
            }
            F::Vt => {
                let v = self.vr(op);
                self.put(RT, v);
            }
            F::Va => {
                let v = self.vr(op);
                self.put(RA, v);
            }
            F::Vb => {
                let v = self.vr(op);
                self.put(RB, v);
            }
            F::Vc => {
                let v = self.vr(op);
                self.put(FRC, v);
            }
            F::VaVb => {
                let v = self.vr(op);
                self.put(RA, v);
                self.put(RB, v);
            }
            F::Xt => self.vsx(op, RT, TX),
            F::Xa => self.vsx(op, RA, AX),
            F::Xb => self.vsx(op, RB, BX),
            F::Xc => self.vsx(op, FRC, CX),
            F::Xab => {
                self.vsx(op, RA, AX);
                self.vsx(op, RB, BX);
            }
            F::Xtq => self.vsx(op, RT, TX_DQ),
            F::Xts => self.vsx(op, RT, TX_8RR),
            F::Xtop => self.vsx(op, RT, at(5)),
            F::Xtp => self.vsx_pair(op, RT, TX_PAIR),
            F::Xap => self.vsx_pair(op, RA, AX),
            F::Xbp => self.vsx_pair(op, RB, BX),
            F::Rc => {
                let v = self.gpr(op);
                self.put(FRC, v);
            }
            F::L1 => {
                if self.small(op, 1, "L") == Some(0) {
                    self.word &= !(1 << L_BIT);
                }
            }
            F::Uim(bits, lsb) => {
                let v = self.small(op, (1i64 << bits) - 1, "immediate");
                self.put(lsb as u32, v);
            }
            F::Sim(bits, lsb) => {
                let hi = (1i64 << (bits - 1)) - 1;
                self.signed_field(op, -hi - 1, hi, bits, lsb);
            }
            F::SimU(bits, lsb) => {
                self.signed_field(op, -(1i64 << (bits - 1)), (1i64 << bits) - 1, bits, lsb)
            }
            F::DmEx => match self.constant(op, "doubleword selector") {
                // Written 0 or 1, encoded as the two-bit DM that `xxpermdi`
                // takes: `xxspltd vT, vB, 1` is `xxpermdi vT, vB, vB, 3`.
                Some(v @ (0 | 1)) => self.word |= ((v as u64) * 3) << at(23),
                Some(v) => self.reject(op, format!("doubleword selector {v} must be 0 or 1")),
                None => {}
            },
            F::Dcmxs => {
                if let Some(v) = self.small(op, 127, "data class mask") {
                    // Seven bits in three pieces: 16:20 hold the low five,
                    // bit 29 the next and bit 25 the top one, since the
                    // slot it shares with `xvtstdc*`'s VSX register is full.
                    self.word |= (((v & 0x1f) << RA) | ((v & 0x20) >> 3) | (v & 0x40)) as u64;
                }
            }
            F::Dx | F::NegDx => {
                let neg = f == F::NegDx;
                // Either sign, as for `lis`; `subpcis` mirrors the range.
                let (lo, hi) = if neg {
                    (-65535, 32768)
                } else {
                    (-32768, 65535)
                };
                match self.constant(op, "immediate") {
                    Some(v) if (lo..=hi).contains(&v) => {
                        self.word |= dx_field(if neg { -v } else { v });
                    }
                    Some(v) => self.reject(
                        op,
                        format!("immediate {v} is out of range: must be {lo} to {hi}"),
                    ),
                    None => {}
                }
            }
            F::MemDQ => self.mem(op, Disp::Dq),
            F::MemD34 => self.mem(op, Disp::D34),
            F::Simm34 => self.imm34(op, false),
            F::NegSimm34 => self.imm34(op, true),
            F::Imm32 => match self.constant(op, "immediate") {
                // Both references take a 32-bit value written with its sign
                // extended by hand, or with a borrow past the top, and keep
                // the low 32 bits: GNU as allows -2^32 to 2^33-1 so that a
                // 64-bit host reads `~0` and `0xffffffff` alike.
                Some(v) if (-0x1_0000_0000..=0x1_ffff_ffff).contains(&v) => {
                    let v = v as u64;
                    self.word |= ((v & 0xffff_0000) << 16) | (v & 0xffff);
                }
                Some(v) => self.reject(
                    op,
                    format!("immediate {v} is out of range: must be -4294967296 to 8589934591"),
                ),
                None => {}
            },
            F::Pcrel => {
                if let Some(v) = self.small(op, 1, "R") {
                    self.word |= (v as u64) << PFX_R;
                }
            }
            F::RotNB(kind) => {
                let (Some(n), Some(b)) = (
                    self.constant(op, "field width"),
                    group
                        .get(1)
                        .and_then(|o| self.constant(o, "starting bit position")),
                ) else {
                    return;
                };
                let r = rot2(kind, n, b);
                self.rotate(op, r);
            }
        }
    }

    /// The R bit and what it applies to have to agree. R says the
    /// displacement is from the instruction, so it leaves no room for a base
    /// register, which both references refuse; and a PC-relative relocation
    /// in a field the instruction reads as `rA`-relative would resolve to
    /// nonsense, which llvm-mc refuses (GNU as does not).
    fn check_pcrel(&mut self, span: Span) {
        let r = (self.word >> PFX_R) & 1 == 1;
        let ra = (self.word >> RA) & 0x1f;
        if r && ra != 0 {
            self.cx.error(
                span,
                format!("the R operand can only be 1 when the base register is 0, not r{ra}"),
            );
            self.failed = true;
        } else if !r && self.fixups.iter().any(|f| f.kind.pcrel) {
            self.cx.error(
                span,
                "a PC-relative reference needs the R operand, the last, to be 1",
            );
            self.failed = true;
        } else if r
            && self
                .fixups
                .iter()
                .any(|f| matches!(f.kind.reloc, reloc::TPREL34 | reloc::DTPREL34))
        {
            // The mirror image, which llvm-mc refuses and GNU as does not: a
            // thread-local offset is added to a register, never to the
            // instruction's address.
            self.cx.error(
                span,
                "a thread-local offset is not relative to the instruction, so the R operand, the last, must be 0",
            );
            self.failed = true;
        }
    }

    fn put(&mut self, shift: u32, value: Option<u32>) {
        if let Some(v) = value {
            self.word |= (v as u64) << shift;
        }
    }

    /// A six-bit VSX register: five bits in `slot` and the sixth in `ext`.
    fn vsx(&mut self, op: &Operand, slot: u32, ext: u32) {
        if let Some(v) = self.vsr(op) {
            self.word |= (((v & 0x1f) << slot) | ((v >> 5) << ext)) as u64;
        }
    }

    /// A VSX register pair, named by the even register it starts at. The low
    /// bit of the number is not encoded, so an odd one is an error rather
    /// than something to round.
    fn vsx_pair(&mut self, op: &Operand, slot: u32, ext: u32) {
        let Some(v) = self.vsr(op) else { return };
        if v % 2 != 0 {
            return self.reject(
                op,
                format!("vs{v} is not a register pair: it must be even-numbered"),
            );
        }
        self.word |= (((v & 0x1e) << slot) | ((v >> 5) << ext)) as u64;
    }

    /// A signed immediate field of `bits` bits at `lsb`, accepting anything
    /// from `lo` to `hi` so that a field written either way round — GNU as
    /// takes `xxspltib`'s byte as -128 to 255 — is read the same.
    fn signed_field(&mut self, op: &Operand, lo: i64, hi: i64, bits: u8, lsb: u8) {
        match self.constant(op, "immediate") {
            Some(v) if (lo..=hi).contains(&v) => {
                let mask = (1u64 << bits) - 1;
                self.word |= (v as u64 & mask) << lsb;
            }
            Some(v) => self.reject(
                op,
                format!("immediate {v} is out of range: must be {lo} to {hi}"),
            ),
            None => {}
        }
    }

    fn rotate(&mut self, op: &Operand, r: Result<RotFields, String>) {
        match r {
            Ok(RotFields::M { sh, mb, me }) => {
                self.word |= ((sh << RB) | (mb << FRC) | (me << ME)) as u64;
            }
            Ok(RotFields::Md { sh, m }) => self.word |= (md_sh(sh) | md_m(m)) as u64,
            Err(msg) => self.reject(op, msg),
        }
    }

    // ---- operand readers --------------------------------------------------

    fn reject(&mut self, op: &Operand, msg: impl Into<String>) {
        self.cx.error(op.span, msg);
        self.failed = true;
    }

    fn expected(&mut self, op: &Operand, what: &str) {
        let found = op.describe();
        self.reject(op, format!("expected {what}, found {found}"));
    }

    /// The plain value of an operand; a memory reference has none.
    fn plain(op: &Operand) -> Option<Value> {
        match op.kind {
            OperandKind::Value(v) => Some(v),
            OperandKind::Mem(_) => None,
        }
    }

    /// A register operand, written either as a name of `class` or as the bare
    /// number that PowerPC assembly usually uses.
    fn reg_in(&mut self, op: &Operand, class: RegClass, what: &str) -> Option<u32> {
        match Self::plain(op) {
            Some(Value::Reg(r)) if r.class == class => Some(r.num as u32),
            Some(Value::Reg(r)) => {
                let name = describe(r);
                self.reject(op, format!("`{name}` is not {what}"));
                None
            }
            Some(Value::Expr(e)) => match self.cx.constant(e) {
                Some(v) if (0..=31).contains(&v) => Some(v as u32),
                Some(v) => {
                    self.reject(
                        op,
                        format!("register number {v} is out of range: must be 0 to 31"),
                    );
                    None
                }
                None => {
                    self.reject(op, format!("expected {what}"));
                    None
                }
            },
            None => {
                self.expected(op, what);
                None
            }
        }
    }

    fn gpr(&mut self, op: &Operand) -> Option<u32> {
        self.reg_in(op, RegClass::Gpr, "a general-purpose register")
    }

    fn fpr(&mut self, op: &Operand) -> Option<u32> {
        self.reg_in(op, RegClass::Fpr, "a floating-point register")
    }

    fn vr(&mut self, op: &Operand) -> Option<u32> {
        self.reg_in(op, RegClass::Vr, "a vector register")
    }

    /// A VSX register, `vs0`-`vs63`. The bank is twice as wide as every other,
    /// so a bare number here reaches 63.
    fn vsr(&mut self, op: &Operand) -> Option<u32> {
        match Self::plain(op) {
            Some(Value::Reg(r)) if r.class == RegClass::Vsr => Some(r.num as u32),
            Some(Value::Expr(e)) => match self.cx.constant(e) {
                Some(v) if (0..=63).contains(&v) => Some(v as u32),
                Some(v) => {
                    self.reject(
                        op,
                        format!("register number {v} is out of range: must be 0 to 63"),
                    );
                    None
                }
                None => {
                    self.reject(op, "expected a VSX register");
                    None
                }
            },
            _ => {
                self.expected(op, "a VSX register");
                None
            }
        }
    }

    /// A CR field number, written `cr3` or `3`.
    fn crf(&mut self, op: &Operand) -> Option<u32> {
        match Self::plain(op) {
            Some(Value::Reg(r)) if r.class == RegClass::Cr => Some(r.num as u32),
            Some(Value::Expr(e)) => match self.cx.constant(e) {
                Some(v) if (0..=7).contains(&v) => Some(v as u32),
                Some(v) => {
                    self.reject(
                        op,
                        format!("condition register field {v} is out of range: must be 0 to 7"),
                    );
                    None
                }
                None => {
                    self.reject(op, "expected a condition register field");
                    None
                }
            },
            _ => {
                self.expected(op, "a condition register field");
                None
            }
        }
    }

    /// An SPR number, written `lr`, `ctr`, `xer` or as a number.
    fn spr(&mut self, op: &Operand) -> Option<u32> {
        match Self::plain(op) {
            Some(Value::Reg(r)) if r.class == RegClass::Spr => Some(r.num as u32),
            Some(Value::Expr(e)) => match self.cx.constant(e) {
                Some(v) if (0..=1023).contains(&v) => Some(v as u32),
                Some(v) => {
                    self.reject(
                        op,
                        format!("special-purpose register {v} is out of range: must be 0 to 1023"),
                    );
                    None
                }
                None => {
                    self.reject(op, "expected a special-purpose register");
                    None
                }
            },
            _ => {
                self.expected(op, "a special-purpose register");
                None
            }
        }
    }

    /// A small unsigned constant field.
    fn small(&mut self, op: &Operand, max: i64, what: &str) -> Option<u32> {
        match self.constant(op, what) {
            Some(v) if (0..=max).contains(&v) => Some(v as u32),
            Some(v) => {
                self.reject(
                    op,
                    format!("{what} {v} is out of range: must be 0 to {max}"),
                );
                None
            }
            None => None,
        }
    }

    fn constant(&mut self, op: &Operand, what: &str) -> Option<i64> {
        match Self::plain(op) {
            Some(Value::Expr(e)) => match self.cx.constant(e) {
                Some(v) => Some(v),
                None => {
                    self.reject(op, format!("{what} must be a constant"));
                    None
                }
            },
            _ => {
                self.expected(op, &format!("a constant {what}"));
                None
            }
        }
    }

    // ---- immediates, displacements and branch targets ---------------------

    /// A 16-bit immediate in bits 16:31, resolved now or left to a fixup.
    fn imm16(&mut self, op: &Operand, range: Range16) {
        let Some(Value::Expr(e)) = Self::plain(op) else {
            self.expected(op, "an immediate");
            return;
        };
        match self.halfword_value(op, e) {
            Folded::Invalid => {}
            Folded::Truncated(v) => self.word |= v as u64 & 0xffff,
            Folded::Symbolic => self.halfword_fixup(e, op.span, Disp::D),
            Folded::Plain(v) if range.bounds().contains(&v) => self.word |= v as u64 & 0xffff,
            Folded::Plain(v) => {
                let b = range.bounds();
                self.reject(
                    op,
                    format!(
                        "immediate {v} is out of range: must be {} to {}",
                        b.start(),
                        b.end()
                    ),
                );
            }
        }
    }

    /// A `d(rA)` reference: the base into RA, the displacement into the low
    /// halfword. A DS-form displacement owns only 14 of those 16 bits, so it
    /// must be a multiple of four and is merged rather than overwritten.
    fn mem(&mut self, op: &Operand, disp_kind: Disp) {
        let OperandKind::Mem(Mem {
            disp,
            base,
            base_span,
        }) = op.kind
        else {
            self.expected(op, "a memory operand of the form `d(rA)`");
            return;
        };
        let base_op = Operand {
            kind: OperandKind::Value(base),
            span: base_span,
        };
        let v = self.gpr(&base_op);
        self.put(RA, v);

        let Some(e) = disp else { return };
        if disp_kind == Disp::D34 {
            return self.imm34_value(op, e, false);
        }
        let v = match self.halfword_value(op, e) {
            Folded::Invalid => return,
            Folded::Symbolic => return self.halfword_fixup(e, op.span, disp_kind),
            Folded::Plain(v) if !Range16::Signed.bounds().contains(&v) => {
                return self.reject(
                    op,
                    format!("displacement {v} is out of range: must be -32768 to 32767"),
                );
            }
            Folded::Plain(v) | Folded::Truncated(v) => v,
        };
        let step = disp_kind.step();
        if v % step != 0 {
            return self.reject(
                op,
                format!(
                    "displacement {v} must be a multiple of {step} in a {} instruction",
                    disp_kind.form()
                ),
            );
        }
        self.word |= v as u64 & 0xffff;
    }

    /// A 34-bit immediate, as `pli` and `paddi` take it.
    fn imm34(&mut self, op: &Operand, neg: bool) {
        let Some(Value::Expr(e)) = Self::plain(op) else {
            self.expected(op, "an immediate");
            return;
        };
        self.imm34_value(op, e, neg);
    }

    /// The 34-bit field of a prefixed instruction, resolved now or left to a
    /// relocation. Its top 18 bits live in the prefix word and the rest in the
    /// suffix, so nothing about it is contiguous.
    fn imm34_value(&mut self, op: &Operand, e: ExprRef, neg: bool) {
        let Some(v) = self.cx.constant(e) else {
            return self.imm34_fixup(e, op.span, neg);
        };
        if self.modifier(e).is_some() {
            self.reject(op, "a relocation modifier must apply to a symbol here");
            return;
        }
        let v = if neg { -v } else { v };
        if !(-0x2_0000_0000..=0x1_ffff_ffff).contains(&v) {
            let (lo, hi) = if neg {
                (-0x1_ffff_ffffi64, 0x2_0000_0000i64)
            } else {
                (-0x2_0000_0000, 0x1_ffff_ffff)
            };
            return self.reject(
                op,
                format!("immediate is out of range: must be {lo} to {hi}"),
            );
        }
        self.word |= d34_bits(v);
    }

    /// The relocation a symbolic 34-bit field needs. `@pcrel` makes the field
    /// relative to the instruction itself, which is what the R bit in the
    /// prefix says too; the assembler does not set R from the modifier, since
    /// both references make the source write it.
    fn imm34_fixup(&mut self, e: ExprRef, span: Span, neg: bool) {
        if neg {
            self.cx
                .error(span, "a negated immediate must be a constant");
            self.failed = true;
            return;
        }
        if self.cx.state.bits < 64 {
            self.cx.error(
                span,
                "a symbol in a 34-bit field needs a 64-bit object: the relocations exist only for PowerPC64",
            );
            self.failed = true;
            return;
        }
        // The thread-local forms are those both references write. llvm-mc
        // has no `@got@dtprel@pcrel`, which GNU as writes as
        // R_PPC64_GOT_DTPREL_PCREL34, and both put something other than a
        // 34-bit relocation in the field for the halves (`@tprel@l`): GNU as
        // a halfword relocation on the suffix's low half, llvm-mc bytes that
        // change from run to run.
        let (reloc, pcrel) = match self.modifier(e).as_deref() {
            None => (reloc::D34, false),
            Some("pcrel") => (reloc::PCREL34, true),
            Some("got@pcrel") => (reloc::GOT_PCREL34, true),
            Some("tprel") => (reloc::TPREL34, false),
            Some("dtprel") => (reloc::DTPREL34, false),
            Some("got@tlsgd@pcrel") => (reloc::GOT_TLSGD_PCREL34, true),
            Some("got@tlsld@pcrel") => (reloc::GOT_TLSLD_PCREL34, true),
            Some("got@tprel@pcrel") => (reloc::GOT_TPREL_PCREL34, true),
            Some(other) => {
                self.cx.error(
                    span,
                    format!("relocation modifier `@{other}` is not supported here"),
                );
                self.failed = true;
                return;
            }
        };
        // llvm-mc leaves a PC-relative prefixed reference to the linker even
        // where it could resolve it, since the linker may rewrite the
        // instruction; GNU as resolves one to a local label.
        let mut kind = if pcrel {
            FixupKind::pcrel(8, 0).relocated_in_objects()
        } else {
            FixupKind::data(8).signed()
        };
        kind = kind
            .with_reloc(reloc)
            .with_field(34, 1)
            .scatter(if self.endian == Endian::Big {
                d34_scatter_be
            } else {
                d34_scatter_le
            });
        kind = match reloc {
            reloc::GOT_PCREL34 => kind.link(LinkValue::LinkerOnly(GOT)),
            reloc::TPREL34 => kind.link(LinkValue::LinkerOnly(TP)).linker_only(),
            reloc::DTPREL34 => kind.link(LinkValue::LinkerOnly(DTP)).linker_only(),
            reloc::GOT_TLSGD_PCREL34 | reloc::GOT_TLSLD_PCREL34 | reloc::GOT_TPREL_PCREL34 => {
                kind.link(LinkValue::LinkerOnly(TLS_GOT)).linker_only()
            }
            _ => kind,
        };
        self.fixups.push(Fixup {
            offset: 0,
            expr: e,
            kind,
            span,
        });
    }

    /// Reads a value destined for the low halfword, applying a modifier that
    /// names a half of a known value — `@l`, `@ha`, `@higher` and the rest —
    /// where it is written on a constant.
    ///
    /// The core treats relocation modifiers as annotations that only choose
    /// a relocation, so a constant reaches the backend unmodified; without
    /// this, `lis 3, 0x12348000@ha` would be rejected and `lis 3, 0x8000@ha`
    /// silently encoded as 0x8000 rather than 1.
    fn halfword_value(&mut self, op: &Operand, e: ExprRef) -> Folded {
        let Some((name, inner)) = self.applied_modifier(e) else {
            return match self.cx.constant(e) {
                // A modifier buried inside arithmetic on a constant has no
                // meaning the linker could supply either.
                Some(_) if self.modifier(e).is_some() => {
                    self.reject(op, "a relocation modifier must apply to the whole operand");
                    Folded::Invalid
                }
                Some(v) => Folded::Plain(v),
                None => Folded::Symbolic,
            };
        };
        let Some(v) = self.cx.constant(inner) else {
            return Folded::Symbolic;
        };
        let Some(half) = halfword_modifier(Some(&name)) else {
            self.reject(
                op,
                format!("relocation modifier `@{name}` is not supported here"),
            );
            return Folded::Invalid;
        };
        // The halves above bit 31 exist only in PowerPC64's table, and GNU as
        // will not read the spelling at all in 32-bit code.
        if half.ppc32 == 0 && self.cx.state.bits < 64 {
            self.reject(op, wider_object(&name));
            return Folded::Invalid;
        }
        match half.link {
            Linked::Part(fold) => Folded::Truncated(fold(v)),
            Linked::Tls(what) => {
                self.reject(
                    op,
                    format!(
                        "relocation modifier `@{name}` names {what}, which only the linker knows, so it needs a symbol rather than a number"
                    ),
                );
                Folded::Invalid
            }
            Linked::Whole | Linked::Table(_) => {
                self.reject(
                    op,
                    format!(
                        "relocation modifier `@{name}` names an entry the linker builds, so it needs a symbol rather than a number"
                    ),
                );
                Folded::Invalid
            }
        }
    }

    /// A relocation against the low halfword of the instruction word.
    ///
    /// ELF puts `r_offset` on the halfword itself rather than on the
    /// instruction, so on a big-endian target the fixup starts two bytes in
    /// and on a little-endian one at the instruction's first byte.
    fn halfword_fixup(&mut self, e: ExprRef, span: Span, disp: Disp) {
        // The DS relocations exist only for 64-bit objects; GNU as writes the
        // plain halfword ones in 32-bit code, where llvm-mc writes numbers
        // `R_PPC_*` does not define.
        let split = disp != Disp::D && self.cx.state.bits == 64;
        let name = self.modifier(e);
        let Some(half) = halfword_modifier(name.as_deref()) else {
            let name = name.unwrap_or_default();
            self.cx.error(
                span,
                format!("relocation modifier `@{name}` is not supported here"),
            );
            self.failed = true;
            return;
        };
        let reloc = match (self.cx.state.bits == 64, split) {
            // Only for the modifiers a 64-bit object has a DS form of, though:
            // llvm-mc refuses the others in 32-bit code too, and GNU as warns
            // that `@ha` and `@h` are unsupported on the instruction.
            (false, _) if disp != Disp::D && half.ppc64_split == 0 => 0,
            (false, _) => half.ppc32,
            (true, false) => half.ppc64,
            (true, true) => half.ppc64_split,
        };
        if reloc == 0 {
            let name = name.unwrap_or_default();
            self.cx.error(
                span,
                if half.ppc32 == 0 && self.cx.state.bits < 64 {
                    wider_object(&name)
                } else {
                    format!(
                        "relocation modifier `@{name}` has no relocation for a {} displacement, whose low bits belong to the opcode",
                        disp.form()
                    )
                },
            );
            self.failed = true;
            return;
        }
        // GNU as for PowerPC32 reads `x+4@got@tprel` as a GOT entry for
        // `x+4`, which its thread-local GOT relocations cannot express, and
        // refuses it; `x@got@tprel+4`, an entry for `x` and an offset from
        // it, it takes. A 64-bit object takes both.
        if self.cx.state.bits < 64
            && matches!(half.link, Linked::Tls(TLS_GOT))
            && self.modified_number(e).is_some_and(|v| v != 0)
        {
            let name = name.unwrap_or_default();
            self.cx.error(
                span,
                format!(
                    "`sym+offset@{name}` is not supported in a 32-bit object; write `sym@{name}+offset`"
                ),
            );
            self.failed = true;
            return;
        }
        let mut kind = FixupKind::data(2).with_reloc(reloc);
        // What the linker does with a modifier that names a half of the
        // address itself, for a flat image or a value that resolves while
        // assembling. The rest name a GOT or TOC entry, which is the
        // linker's to build and a flat image never has.
        match half.link {
            Linked::Whole => {}
            Linked::Part(fold) => kind = kind.link(LinkValue::Split(fold)),
            Linked::Table(needs) => kind = kind.link(LinkValue::LinkerOnly(needs)),
            Linked::Tls(what) => kind = kind.link(LinkValue::LinkerOnly(what)).linker_only(),
        }
        // A DS- or DQ-form halfword cannot be overwritten whole: its low two
        // or four bits belong to the opcode. Both take the same relocation,
        // which is what the references write.
        match disp {
            Disp::D | Disp::D34 => {}
            Disp::Ds => kind = kind.with_field(16, 4).scatter(ds_field),
            Disp::Dq => kind = kind.with_field(16, 16).scatter(dq_field),
        }
        self.fixups.push(Fixup {
            offset: if self.endian == Endian::Big { 2 } else { 0 },
            expr: e,
            kind,
            span,
        });
    }

    /// A branch target. The fixup covers the whole instruction word, since the
    /// AA and LK bits share the displacement field's bytes.
    ///
    /// `@plt` routes the call through the procedure linkage table and
    /// `@local` tells the linker to keep it direct; both are relocations of
    /// their own, and both exist only in PowerPC32's table. Everything else
    /// the references spell on a branch is refused, for the reasons
    /// [`Encoder::branch_reloc`] gives. A call to `__tls_get_addr` may also
    /// name the thread-local variable it resolves, in parentheses after the
    /// target; see [`Encoder::tls_call`].
    fn branch(&mut self, op: &Operand, bits: u8, pcrel: bool) {
        let e = match op.kind {
            OperandKind::Value(Value::Expr(e)) => e,
            // `bl __tls_get_addr(x@tlsgd)` reads as a memory operand.
            OperandKind::Mem(Mem {
                disp: Some(target),
                base,
                base_span,
            }) => match self.tls_call(op, target, base, base_span) {
                Some(e) => e,
                None => return,
            },
            _ => {
                self.expected(op, "a branch target");
                return;
            }
        };
        let (scatter, rel, abs): (fn(u64, i64) -> u64, u32, u32) = if bits == 26 {
            (i_form, reloc::REL24, reloc::ADDR24)
        } else {
            (b_form, reloc::REL14, reloc::ADDR14)
        };
        let modified = self.modifier(e);
        let rel = match &modified {
            None => rel,
            Some(m) => match self.branch_reloc(op, m, bits, pcrel) {
                Some(r) => r,
                None => return,
            },
        };
        let mut kind = if pcrel {
            FixupKind::pcrel(4, 0).with_reloc(rel)
        } else {
            FixupKind::data(4).signed().with_reloc(abs)
        };
        // Whether the call goes through the PLT is the linker's to decide, so
        // the relocation has to reach it even where the target is in this
        // section and the distance is already known. llvm-mc keeps it for
        // `@local` too; GNU as resolves that one away.
        if modified.is_some() {
            kind = kind.relocated_in_objects();
        }
        self.fixups.push(Fixup {
            offset: 0,
            expr: e,
            kind: kind.with_field(bits, 4).scatter(scatter),
            span: op.span,
        });
    }

    /// The relocation a modifier on a branch target selects, or `None` having
    /// reported why there is none.
    ///
    /// Only `@plt` and `@local` on a 24-bit PC-relative branch in a 32-bit
    /// object survive. The rest are refused because the two references
    /// disagree about them and one of the two answers cannot be linked:
    ///
    /// * In a 64-bit object `powerpc64-linux-gnu-as` does not read `@plt` at
    ///   all, and llvm-mc writes R_PPC_PLTREL24's number 18, which PowerPC64
    ///   leaves undefined — GNU ld refuses the object with "unsupported
    ///   relocation type 0x12". The ELFv2 way to call through the PLT is a
    ///   plain `bl` and a `nop`, which the linker turns into a stub.
    /// * On a 14-bit conditional branch, or an absolute one, GNU as writes
    ///   the 24-bit PC-relative PLTREL24 into a field that is neither, and
    ///   llvm-mc drops the modifier for a plain REL14 — the quiet answer
    ///   that links and then reaches the wrong place at run time.
    /// * `@notoc` is R_PPC64_REL24_P9NOTOC to GNU as and R_PPC64_REL24_NOTOC
    ///   to llvm-mc: two different relocations for one spelling.
    /// * The halfword modifiers (`bl foo@ha`) are GNU as putting an
    ///   ADDR16 relocation on an instruction that has no halfword field;
    ///   llvm-mc refuses them.
    fn branch_reloc(&mut self, op: &Operand, name: &str, bits: u8, pcrel: bool) -> Option<u32> {
        let reloc = match name {
            "plt" => reloc::PLTREL24,
            "local" => reloc::LOCAL24PC,
            _ => {
                self.reject(
                    op,
                    format!("relocation modifier `@{name}` is not supported on a branch target"),
                );
                return None;
            }
        };
        if bits != 26 || !pcrel {
            self.reject(
                op,
                format!(
                    "relocation modifier `@{name}` needs a 24-bit PC-relative branch, which this is not"
                ),
            );
            return None;
        }
        if self.cx.state.bits >= 64 {
            self.reject(
                op,
                format!(
                    "relocation modifier `@{name}` exists only in PowerPC32's table: an ELFv2 call is a plain `bl`, which the linker routes through a stub or not as it sees fit"
                ),
            );
            return None;
        }
        Some(reloc)
    }

    // ---- thread-local markers ---------------------------------------------

    /// The expression of an operand written as `sym@tls` or `sym@tls@pcrel`.
    fn tls_operand(&self, op: &Operand) -> Option<ExprRef> {
        let Some(Value::Expr(e)) = Self::plain(op) else {
            return None;
        };
        matches!(self.modifier(e).as_deref(), Some("tls" | "tls@pcrel")).then_some(e)
    }

    /// `sym@tls` as the last register of an `add` or an indexed load or
    /// store: the thread pointer, marked with R_PPC_TLS or R_PPC64_TLS so
    /// that a linker which turns the access into another model knows which
    /// instruction to rewrite. The register is r13 in a 64-bit object and r2
    /// in a 32-bit one, and the relocation covers no bytes: it sits on the
    /// instruction's first byte, or on the second for `@tls@pcrel`, which is
    /// how the POWER10 sequences tell the linker the GOT entry was loaded
    /// PC-relative.
    ///
    /// The instructions are the ones llvm-mc accepts the operand on; GNU as
    /// takes it on the recording and update forms too (`add.`, `lwzux`),
    /// which no linker rewrites.
    fn tls_register(&mut self, op: &Operand, e: ExprRef) -> Option<u32> {
        const MARKED: &[&str] = &[
            "add", "lbzx", "lhzx", "lhax", "lwzx", "lwax", "ldx", "stbx", "sthx", "stwx", "stdx",
            "lfsx", "lfdx", "stfsx", "stfdx",
        ];
        let wide = self.cx.state.bits == 64;
        let pcrel = self.modifier(e).as_deref() == Some("tls@pcrel");
        if !MARKED.contains(&self.mnemonic) || self.suffixed {
            self.reject(
                op,
                "`@tls` marks only `add` and the indexed loads and stores a linker rewrites, with no `.` or `o` suffix",
            );
            return None;
        }
        if pcrel && !wide {
            self.reject(op, wider_object("tls@pcrel"));
            return None;
        }
        // A bare symbol: llvm-mc refuses `x+4@tls`, which only names the
        // variable the instruction reaches anyway.
        let bare = self
            .applied_modifier(e)
            .is_some_and(|(_, inner)| matches!(self.cx.exprs.get(inner).kind, ExprKind::Sym(_)));
        if self.cx.constant(e).is_some() || !bare {
            self.reject(
                op,
                "`@tls` marks the use of a thread-local variable, so it needs a symbol with no offset",
            );
            return None;
        }
        self.tls_marker(e, reloc::TLS, u32::from(pcrel), op.span);
        Some(if wide { 13 } else { 2 })
    }

    /// `bl __tls_get_addr(sym@tlsgd)`, and the same with `@tlsld`: the call
    /// the general- and local-dynamic models make, whose argument names the
    /// variable the linker is to resolve. The argument becomes a relocation
    /// of its own, R_PPC_TLSGD or R_PPC64_TLSGD, covering no bytes, and comes
    /// first; the call's own relocation follows it at the same offset, which
    /// is the order both references write. Answers with the call's target.
    ///
    /// Both references take the argument only on a `bl`, llvm-mc only to
    /// `__tls_get_addr` itself (GNU as takes any name it starts with), and
    /// GNU as only with the modifier on the whole argument: `x+4@tlsgd`, not
    /// `x@tlsgd+4`.
    fn tls_call(
        &mut self,
        op: &Operand,
        target: ExprRef,
        base: Value,
        base_span: Span,
    ) -> Option<ExprRef> {
        let Value::Expr(arg) = base else {
            self.expected(op, "a branch target");
            return None;
        };
        let wide = self.cx.state.bits == 64;
        let reloc = match (self.modifier(arg).as_deref(), wide) {
            (Some("tlsgd"), false) => reloc::TLSGD_32,
            (Some("tlsld"), false) => reloc::TLSLD_32,
            (Some("tlsgd"), true) => reloc::TLSGD_64,
            (Some("tlsld"), true) => reloc::TLSLD_64,
            _ => {
                self.cx.error(
                    base_span,
                    "a call's argument in parentheses must be `sym@tlsgd` or `sym@tlsld`",
                );
                self.failed = true;
                return None;
            }
        };
        if self.mnemonic != "bl" || self.suffixed {
            self.reject(
                op,
                "a `(sym@tlsgd)` or `(sym@tlsld)` argument goes only on a `bl`",
            );
            return None;
        }
        if !self.calls_tls_get_addr(target) {
            self.reject(
                op,
                "a `(sym@tlsgd)` or `(sym@tlsld)` argument marks only a call to `__tls_get_addr`",
            );
            return None;
        }
        if self.cx.constant(arg).is_some() {
            self.cx.error(
                base_span,
                "the argument names a thread-local variable, so it needs a symbol rather than a number",
            );
            self.failed = true;
            return None;
        }
        if self.applied_modifier(arg).is_none() && self.modifier_on_symbol(arg) {
            self.cx.error(
                base_span,
                "the modifier must apply to the whole argument: write `sym+n@tlsgd`, not `sym@tlsgd+n`",
            );
            self.failed = true;
            return None;
        }
        self.tls_marker(arg, reloc, 0, base_span);
        Some(target)
    }

    /// A relocation that marks the instruction at `offset` rather than fill a
    /// field in it. It names the symbol even where that is a local label
    /// outside a thread-local section, as GNU as does, since the linker looks
    /// the variable up by it.
    fn tls_marker(&mut self, e: ExprRef, reloc: u32, offset: u32, span: Span) {
        self.fixups.push(Fixup {
            offset,
            expr: e,
            kind: FixupKind::data(0)
                .with_reloc(reloc)
                .with_reloc_symbol(RelocSymbol::Symbol)
                .linker_only(),
            span,
        });
    }

    /// Whether a call's target is `__tls_get_addr`, give or take a `@plt`
    /// and a constant offset.
    fn calls_tls_get_addr(&self, e: ExprRef) -> bool {
        match &self.cx.exprs.get(e).kind {
            ExprKind::Sym(n) => self.cx.name(*n) == "__tls_get_addr",
            ExprKind::Modifier(_, inner) => self.calls_tls_get_addr(*inner),
            ExprKind::Binary(BinOp::Add | BinOp::Sub, a, b) => {
                self.calls_tls_get_addr(*a) && self.cx.constant(*b).is_some()
            }
            _ => false,
        }
    }

    /// Whether a modifier inside `e` applies to something that is not a
    /// number, as the `x@tlsgd` of `x@tlsgd+4` does; in `x+4@tlsgd` it
    /// applies to the 4, and the sum is what the relocation names.
    fn modifier_on_symbol(&self, e: ExprRef) -> bool {
        match &self.cx.exprs.get(e).kind {
            ExprKind::Modifier(_, inner) => self.cx.constant(*inner).is_none(),
            ExprKind::Unary(_, a) => self.modifier_on_symbol(*a),
            ExprKind::Binary(_, a, b) => self.modifier_on_symbol(*a) || self.modifier_on_symbol(*b),
            _ => false,
        }
    }

    /// The number a modifier inside `e` applies to, as the `@got@tprel` of
    /// `x+4@got@tprel` applies to the 4.
    fn modified_number(&self, e: ExprRef) -> Option<i64> {
        match &self.cx.exprs.get(e).kind {
            ExprKind::Modifier(_, inner) => self.cx.constant(*inner),
            ExprKind::Unary(_, a) => self.modified_number(*a),
            ExprKind::Binary(_, a, b) => self
                .modified_number(*a)
                .or_else(|| self.modified_number(*b)),
            _ => None,
        }
    }

    /// The modifier chain applied to the whole of `e`, joined the way
    /// [`Encoder::modifier`] joins one, with what it is applied to.
    fn applied_modifier(&self, e: ExprRef) -> Option<(String, ExprRef)> {
        match &self.cx.exprs.get(e).kind {
            ExprKind::Modifier(n, inner) => {
                let name = self.cx.name(*n).to_ascii_lowercase();
                Some(match self.applied_modifier(*inner) {
                    Some((first, base)) => (format!("{first}@{name}"), base),
                    None => (name, *inner),
                })
            }
            _ => None,
        }
    }

    /// The `@`-modifier applied anywhere in an expression, lowercased. A
    /// stack of them comes back joined the way the source spells it:
    /// `sym@got@pcrel` is `got@pcrel`.
    fn modifier(&self, e: ExprRef) -> Option<String> {
        match &self.cx.exprs.get(e).kind {
            ExprKind::Modifier(n, inner) => {
                let name = self.cx.name(*n).to_ascii_lowercase();
                Some(match self.modifier(*inner) {
                    Some(first) => format!("{first}@{name}"),
                    None => name,
                })
            }
            ExprKind::Unary(_, a) => self.modifier(*a),
            ExprKind::Binary(_, a, b) => self.modifier(*a).or_else(|| self.modifier(*b)),
            _ => None,
        }
    }
}

/// How many operands a pattern field consumes. Only the two-immediate rotate
/// mnemonics take more than one.
fn arity(f: F) -> usize {
    match f {
        F::RotNB(_) => 2,
        _ => 1,
    }
}

/// True for a prefixed (POWER10) instruction, which is eight bytes: two words,
/// the prefix first. Nothing else sets any bit above 31.
pub fn prefixed(def: &Def) -> bool {
    def.word >> 32 != 0
}

/// The shape of a displacement field, which decides both what it must be a
/// multiple of and which relocation a symbol in it takes.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Disp {
    /// The whole low halfword, bits 16:31.
    D,
    /// Bits 16:29: a multiple of four.
    Ds,
    /// Bits 16:27: a multiple of sixteen.
    Dq,
    /// 34 bits spread across a prefixed instruction's two words.
    D34,
}

impl Disp {
    /// What the displacement must be a multiple of.
    fn step(self) -> i64 {
        match self {
            Disp::D | Disp::D34 => 1,
            Disp::Ds => 4,
            Disp::Dq => 16,
        }
    }

    /// The form's name, for the diagnostic.
    fn form(self) -> &'static str {
        match self {
            Disp::D => "D-form",
            Disp::Ds => "DS-form",
            Disp::Dq => "DQ-form",
            Disp::D34 => "prefixed",
        }
    }
}

/// A halfword operand after constant folding.
enum Folded {
    /// An ordinary constant, still subject to the field's range check.
    Plain(i64),
    /// A constant cut down to one halfword by `@l`, `@ha`, `@higher` or
    /// another of the modifiers that name a half of a value, which fits by
    /// construction.
    Truncated(i64),
    /// Not known until link time: needs a relocation.
    Symbolic,
    /// Already diagnosed.
    Invalid,
}

/// Which spellings a 16-bit immediate field accepts.
#[derive(Copy, Clone)]
enum Range16 {
    Signed,
    Unsigned,
    /// Either reading. `lis 3, 0xffff` and `lis 3, -1` are the same word, and
    /// sources loading the high half of an address rely on the unsigned
    /// spelling; GNU as and llvm-mc both accept it there and only there.
    Either,
}

impl Range16 {
    fn bounds(self) -> std::ops::RangeInclusive<i64> {
        match self {
            Range16::Signed => -32768..=32767,
            Range16::Unsigned => 0..=65535,
            Range16::Either => -32768..=65535,
        }
    }
}

/// I-form: a 24-bit field of word offsets in bits 6:29, leaving the AA and LK
/// bits at the bottom of the word untouched.
fn i_form(word: u64, v: i64) -> u64 {
    (word & 0xfc00_0003) | (((v >> 2) as u64 & 0x00ff_ffff) << 2)
}

/// B-form: the same idea with a 14-bit field in bits 16:29, so the BO and BI
/// fields above it survive as well.
fn b_form(word: u64, v: i64) -> u64 {
    (word & 0xffff_0003) | (((v >> 2) as u64 & 0x3fff) << 2)
}

/// What a relocation modifier does to a 16-bit field.
struct Half {
    /// The `R_PPC_*` relocation, or zero — `R_PPC_NONE` — where PowerPC32
    /// has none and GNU as refuses the spelling in 32-bit code.
    ppc32: u32,
    /// The `R_PPC64_*` relocation for a D-form field, which is a whole
    /// halfword.
    ppc64: u32,
    /// The `R_PPC64_*` relocation for a DS- or DQ-form field, whose low bits
    /// belong to the opcode. Zero for every modifier both references refuse
    /// there, which is all of them but the whole value and its `@l` half:
    /// `@l`, `@got`, `@got@l`, `@toc`, `@toc@l`, and the same two of
    /// `@tprel`, `@dtprel`, `@got@tprel` and `@got@dtprel`. The GOT entries of
    /// the two dynamic models are only ever built with an `addi`, and neither
    /// reference has a DS form of them.
    ppc64_split: u32,
    /// What the field holds once the address is known.
    link: Linked,
}

/// What a 16-bit field's modifier makes of the address it names, where a
/// flat image or a value that resolves while assembling means the assembler
/// has to work it out rather than leave it to a relocation.
#[derive(Copy, Clone)]
enum Linked {
    /// The value itself, range-checked like any other displacement.
    Whole,
    /// One halfword of it, which fits by construction.
    Part(fn(i64) -> i64),
    /// The offset of an entry only the linker can build; the string says
    /// which, for the diagnostic.
    Table(&'static str),
    /// Something about a thread-local variable, which only the linker knows
    /// once it has laid out the thread-local block, and which it may compute
    /// differently again when it rewrites the access into another model; the
    /// string says what, for the diagnostic. It is always left to the linker,
    /// however near the variable is.
    Tls(&'static str),
}

/// The table of modifiers a 16-bit field takes, or `None` for a spelling
/// neither reference reads there.
///
/// `None` as the name is the field with no modifier at all. The numbers are
/// what `powerpc64-linux-gnu-as` and llvm-mc both write for
/// `addi 3, 3, foo@<name>` and, for `ppc64_split`, for `ld 3, foo@<name>(2)`.
///
/// The thread-local rows are the halves of the two offsets — `@tprel` from
/// the thread pointer, `@dtprel` within the module's block — and of the GOT
/// entries the initial-exec (`@got@tprel`), general-dynamic (`@got@tlsgd`)
/// and local-dynamic (`@got@tlsld`, `@got@dtprel`) models load. Both
/// references agree on every one in both word sizes. The rest of those
/// models, the `@tls` operand and the `(sym@tlsgd)` argument of a call, mark
/// an instruction rather than fill a field; see [`Encoder::tls_register`] and
/// [`Encoder::tls_call`].
///
/// What the references read here and this table leaves out:
///
/// * `@plt@l`, `@plt@h` and `@plt@ha`, the halves of a PLT entry's address,
///   and the `@sectoff` and `@sdarel` families. Only GNU as reads them, and
///   the PowerPC harnesses run against llvm-mc, so nothing rsasm wrote for
///   them could be checked.
fn halfword_modifier(name: Option<&str>) -> Option<Half> {
    // The half of a value each spelling selects. The `a` forms
    // pre-compensate for the half below them being sign-extended when it is
    // added back, which is what a `lis` and `addi` pair needs.
    const LO: fn(i64) -> i64 = |v| v & 0xffff;
    const HI: fn(i64) -> i64 = |v| (v >> 16) & 0xffff;
    const HA: fn(i64) -> i64 = |v| (v.wrapping_add(0x8000) >> 16) & 0xffff;
    const HIGHER: fn(i64) -> i64 = |v| (v >> 32) & 0xffff;
    const HIGHERA: fn(i64) -> i64 = |v| (v.wrapping_add(0x8000) >> 32) & 0xffff;
    const HIGHEST: fn(i64) -> i64 = |v| (v >> 48) & 0xffff;
    const HIGHESTA: fn(i64) -> i64 = |v| (v.wrapping_add(0x8000) >> 48) & 0xffff;

    let half = |ppc32, ppc64, ppc64_split, fold: fn(i64) -> i64| Half {
        ppc32,
        ppc64,
        ppc64_split,
        link: Linked::Part(fold),
    };
    let linked = |ppc32, ppc64, ppc64_split, needs| Half {
        ppc32,
        ppc64,
        ppc64_split,
        link: Linked::Table(needs),
    };
    let tls = |ppc32, ppc64, ppc64_split, what| Half {
        ppc32,
        ppc64,
        ppc64_split,
        link: Linked::Tls(what),
    };
    use reloc::*;
    let Some(name) = name else {
        return Some(Half {
            ppc32: ADDR16,
            ppc64: ADDR16,
            ppc64_split: ADDR16_DS,
            link: Linked::Whole,
        });
    };
    Some(match name {
        "l" => half(ADDR16_LO, ADDR16_LO, ADDR16_LO_DS, LO),
        "h" => half(ADDR16_HI, ADDR16_HI, 0, HI),
        "ha" => half(ADDR16_HA, ADDR16_HA, 0, HA),
        "high" => half(0, ADDR16_HIGH, 0, HI),
        "higha" => half(0, ADDR16_HIGHA, 0, HA),
        "higher" => half(0, ADDR16_HIGHER, 0, HIGHER),
        "highera" => half(0, ADDR16_HIGHERA, 0, HIGHERA),
        "highest" => half(0, ADDR16_HIGHEST, 0, HIGHEST),
        "highesta" => half(0, ADDR16_HIGHESTA, 0, HIGHESTA),
        "got" => linked(GOT16, GOT16, GOT16_DS, GOT),
        "got@l" => linked(GOT16_LO, GOT16_LO, GOT16_LO_DS, GOT),
        "got@h" => linked(GOT16_HI, GOT16_HI, 0, GOT),
        "got@ha" => linked(GOT16_HA, GOT16_HA, 0, GOT),
        "toc" => linked(0, TOC16, TOC16_DS, TOC),
        "toc@l" => linked(0, TOC16_LO, TOC16_LO_DS, TOC),
        "toc@h" => linked(0, TOC16_HI, 0, TOC),
        "toc@ha" => linked(0, TOC16_HA, 0, TOC),
        "tprel" => tls(TPREL16, TPREL16, TPREL16_DS, TP),
        "tprel@l" => tls(TPREL16_LO, TPREL16_LO, TPREL16_LO_DS, TP),
        "tprel@h" => tls(TPREL16_HI, TPREL16_HI, 0, TP),
        "tprel@ha" => tls(TPREL16_HA, TPREL16_HA, 0, TP),
        "tprel@high" => tls(0, TPREL16_HIGH, 0, TP),
        "tprel@higha" => tls(0, TPREL16_HIGHA, 0, TP),
        "tprel@higher" => tls(0, TPREL16_HIGHER, 0, TP),
        "tprel@highera" => tls(0, TPREL16_HIGHERA, 0, TP),
        "tprel@highest" => tls(0, TPREL16_HIGHEST, 0, TP),
        "tprel@highesta" => tls(0, TPREL16_HIGHESTA, 0, TP),
        "dtprel" => tls(DTPREL16, DTPREL16, DTPREL16_DS, DTP),
        "dtprel@l" => tls(DTPREL16_LO, DTPREL16_LO, DTPREL16_LO_DS, DTP),
        "dtprel@h" => tls(DTPREL16_HI, DTPREL16_HI, 0, DTP),
        "dtprel@ha" => tls(DTPREL16_HA, DTPREL16_HA, 0, DTP),
        "dtprel@high" => tls(0, DTPREL16_HIGH, 0, DTP),
        "dtprel@higha" => tls(0, DTPREL16_HIGHA, 0, DTP),
        "dtprel@higher" => tls(0, DTPREL16_HIGHER, 0, DTP),
        "dtprel@highera" => tls(0, DTPREL16_HIGHERA, 0, DTP),
        "dtprel@highest" => tls(0, DTPREL16_HIGHEST, 0, DTP),
        "dtprel@highesta" => tls(0, DTPREL16_HIGHESTA, 0, DTP),
        "got@tprel" => tls(GOT_TPREL16, GOT_TPREL16, GOT_TPREL16, TLS_GOT),
        "got@tprel@l" => tls(GOT_TPREL16_LO, GOT_TPREL16_LO, GOT_TPREL16_LO, TLS_GOT),
        "got@tprel@h" => tls(GOT_TPREL16_HI, GOT_TPREL16_HI, 0, TLS_GOT),
        "got@tprel@ha" => tls(GOT_TPREL16_HA, GOT_TPREL16_HA, 0, TLS_GOT),
        "got@dtprel" => tls(GOT_DTPREL16, GOT_DTPREL16, GOT_DTPREL16, TLS_GOT),
        "got@dtprel@l" => tls(GOT_DTPREL16_LO, GOT_DTPREL16_LO, GOT_DTPREL16_LO, TLS_GOT),
        "got@dtprel@h" => tls(GOT_DTPREL16_HI, GOT_DTPREL16_HI, 0, TLS_GOT),
        "got@dtprel@ha" => tls(GOT_DTPREL16_HA, GOT_DTPREL16_HA, 0, TLS_GOT),
        "got@tlsgd" => tls(GOT_TLSGD16, GOT_TLSGD16, 0, TLS_GOT),
        "got@tlsgd@l" => tls(GOT_TLSGD16_LO, GOT_TLSGD16_LO, 0, TLS_GOT),
        "got@tlsgd@h" => tls(GOT_TLSGD16_HI, GOT_TLSGD16_HI, 0, TLS_GOT),
        "got@tlsgd@ha" => tls(GOT_TLSGD16_HA, GOT_TLSGD16_HA, 0, TLS_GOT),
        "got@tlsld" => tls(GOT_TLSLD16, GOT_TLSLD16, 0, TLS_GOT),
        "got@tlsld@l" => tls(GOT_TLSLD16_LO, GOT_TLSLD16_LO, 0, TLS_GOT),
        "got@tlsld@h" => tls(GOT_TLSLD16_HI, GOT_TLSLD16_HI, 0, TLS_GOT),
        "got@tlsld@ha" => tls(GOT_TLSLD16_HA, GOT_TLSLD16_HA, 0, TLS_GOT),
        _ => return None,
    })
}

/// What the linker builds for the modifiers that name a table entry rather
/// than a part of the address itself.
const GOT: &str = "a GOT entry";
const TOC: &str = "a TOC entry";

/// What the thread-local modifiers name, for the diagnostics.
const TP: &str = "a thread-local variable's offset from the thread pointer";
const DTP: &str = "a thread-local variable's offset in its module's block";
const TLS_GOT: &str = "a GOT entry for a thread-local variable";

/// The diagnostic for a modifier PowerPC32's relocation table has no number
/// for, which is every half above bit 31 and everything about the TOC.
fn wider_object(name: &str) -> String {
    format!(
        "relocation modifier `@{name}` needs a 64-bit object: PowerPC32's relocations do not reach past bit 31 and it has no TOC"
    )
}

/// DS-form displacement: 14 bits of a halfword whose low two bits are opcode.
fn ds_field(half: u64, v: i64) -> u64 {
    (half & 0x3) | (v as u64 & 0xfffc)
}

/// DQ-form displacement: 12 bits of a halfword whose low four bits are opcode
/// and, on the VSX loads and stores, the target register's sixth bit.
fn dq_field(half: u64, v: i64) -> u64 {
    (half & 0xf) | (v as u64 & 0xfff0)
}

/// A 34-bit value placed in a prefixed instruction read as one 64-bit
/// quantity: its top 18 bits in the prefix word's low half, the rest in the
/// suffix's.
fn d34_bits(v: i64) -> u64 {
    let v = v as u64;
    (((v >> 16) & 0x3_ffff) << 32) | (v & 0xffff)
}

/// The same as a fixup scatter. The two words reach memory as separate
/// four-byte quantities, so reading all eight as one integer gives the prefix
/// first on a big-endian target and second on a little-endian one, and the
/// two byte orders need different functions.
fn d34_scatter_be(word: u64, v: i64) -> u64 {
    (word & !0x3_ffff_0000_ffff) | d34_bits(v)
}

fn d34_scatter_le(word: u64, v: i64) -> u64 {
    d34_scatter_be(word.rotate_left(32), v).rotate_left(32)
}

/// `addpcis`'s 16-bit immediate, which the ISA splits into d1 (bits 16:20),
/// d0 (6:15) and d2 (bit 31) so that its two register fields keep their
/// usual places.
fn dx_field(v: i64) -> u64 {
    let v = v as u64 & 0xffff;
    (v & 0xffc1) | ((v & 0x3e) << 15)
}

/// The six-bit SH of an MD- or XS-form rotate is split in two: its low five
/// bits go where the M-form's whole SH goes (16:20), and its top bit lands
/// alone in bit 30, immediately above Rc.
fn md_sh(sh: u32) -> u32 {
    ((sh & 0x1f) << RB) | ((sh >> 5) << ME)
}

/// The six-bit MB or ME of an MD-form rotate is split the other way round: its
/// low five bits occupy 21:25 and its top bit sits in bit 26. Read as plain
/// binary the field is therefore `m[1:5] || m[0]`, not `m`.
fn md_m(m: u32) -> u32 {
    ((m & 0x1f) << FRC) | ((m >> 5) << at(26))
}

/// The rotate fields an extended mnemonic expands to.
enum RotFields {
    /// 32-bit rotates: SH, MB and ME, five bits each.
    M { sh: u32, mb: u32, me: u32 },
    /// 64-bit rotates: SH and the one mask bound the form has, six bits each.
    Md { sh: u32, m: u32 },
}

/// Expands an extended rotate that takes a single shift amount.
///
/// Each of these is one `rlwinm`, `rldicl` or `rldicr` whose mask is a
/// function of the shift: `slwi rA, rS, n` keeps the bits that did not fall
/// off the left end, `clrlwi` rotates by nothing and masks, and so on. Which
/// base instruction is used is fixed by the table entry; this only computes
/// the fields.
fn rot1(kind: Rot, n: i64) -> Result<RotFields, String> {
    use Rot::*;
    let wide = matches!(kind, Sldi | Srdi | Clrldi | Clrrdi | Rotldi | Rotrdi);
    let w: i64 = if wide { 64 } else { 32 };
    if !(0..w).contains(&n) {
        return Err(format!(
            "shift count {n} is out of range: must be 0 to {}",
            w - 1
        ));
    }
    let (n, w) = (n as u32, w as u32);
    Ok(match kind {
        // A left shift by n is a rotate by n keeping bits 0..=31-n; the mask
        // is what stops the bits that rotated round from coming back in.
        Slwi => RotFields::M {
            sh: n,
            mb: 0,
            me: 31 - n,
        },
        // A right shift is a left rotate by 32-n. The modulo matters only for
        // n = 0, where a rotate by 32 does not fit the five-bit field.
        Srwi => RotFields::M {
            sh: (32 - n) % 32,
            mb: n,
            me: 31,
        },
        Clrlwi => RotFields::M {
            sh: 0,
            mb: n,
            me: 31,
        },
        Clrrwi => RotFields::M {
            sh: 0,
            mb: 0,
            me: 31 - n,
        },
        Rotlwi => RotFields::M {
            sh: n,
            mb: 0,
            me: 31,
        },
        Rotrwi => RotFields::M {
            sh: (32 - n) % 32,
            mb: 0,
            me: 31,
        },
        // The 64-bit forms carry only one mask bound. Whether it is read as MB
        // or as ME is decided by the opcode the table entry names: `rldicl`
        // masks from the left, `rldicr` from the right.
        Sldi => RotFields::Md { sh: n, m: 63 - n },
        Srdi => RotFields::Md {
            sh: (w - n) % w,
            m: n,
        },
        Clrldi => RotFields::Md { sh: 0, m: n },
        Clrrdi => RotFields::Md { sh: 0, m: 63 - n },
        Rotldi => RotFields::Md { sh: n, m: 0 },
        Rotrdi => RotFields::Md {
            sh: (w - n) % w,
            m: 0,
        },
    })
}

/// Expands an extended rotate that names a field of `n` bits starting at bit
/// `b`, counting from the most significant bit as the ISA does.
fn rot2(kind: Rot2, n: i64, b: i64) -> Result<RotFields, String> {
    use Rot2::*;
    let wide = matches!(kind, Extldi | Extrdi | Insrdi);
    let w: i64 = if wide { 64 } else { 32 };
    if !(1..=w).contains(&n) {
        return Err(format!("field width {n} is out of range: must be 1 to {w}"));
    }
    if !(0..w).contains(&b) {
        return Err(format!(
            "bit position {b} is out of range: must be 0 to {}",
            w - 1
        ));
    }
    if n + b > w {
        return Err(format!(
            "a {n}-bit field starting at bit {b} runs past the end of a {w}-bit register"
        ));
    }
    let (n, b, w) = (n as u32, b as u32, w as u32);
    Ok(match kind {
        Extlwi => RotFields::M {
            sh: b,
            mb: 0,
            me: n - 1,
        },
        Extrwi => RotFields::M {
            sh: (b + n) % 32,
            mb: 32 - n,
            me: 31,
        },
        Inslwi => RotFields::M {
            sh: (32 - b) % 32,
            mb: b,
            me: b + n - 1,
        },
        Insrwi => RotFields::M {
            sh: (32 - (b + n)) % 32,
            mb: b,
            me: b + n - 1,
        },
        Extldi => RotFields::Md { sh: b, m: n - 1 },
        Extrdi => RotFields::Md {
            sh: (b + n) % w,
            m: 64 - n,
        },
        Insrdi => RotFields::Md {
            sh: (w - (b + n)) % w,
            m: b,
        },
    })
}

/// Padding for `.align` in code: PowerPC's canonical no-op is `ori 0, 0, 0`.
///
/// A run of padding that is not a whole number of words can only arise after
/// sub-word data, so the odd bytes are the tail of a partial word and go
/// first, as zeros; the whole words that follow are real no-ops and stay
/// executable.
pub fn nop_bytes(endian: Endian, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len % 4];
    let word = endian.bytes(0x6000_0000, 4);
    while out.len() + 4 <= len {
        out.extend_from_slice(&word);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md_fields_split_the_way_the_isa_describes() {
        // `rldicl 3, 4, 40, 33`: SH 40 is 0b101000, MB 33 is 0b100001.
        assert_eq!(md_sh(40), (8 << 11) | 2);
        assert_eq!(md_m(33), (1 << 6) | (1 << 5));
        // Values below 32 keep the plain layout of the 32-bit forms.
        assert_eq!(md_sh(5), 5 << 11);
        assert_eq!(md_m(6), 6 << 6);
    }

    #[test]
    fn branch_scatter_keeps_the_opcode_and_link_bits() {
        // `bl` is 0x48000001; a +4096 displacement must not disturb LK.
        assert_eq!(i_form(0x4800_0001, 4096), 0x4800_1001);
        assert_eq!(i_form(0x4800_0000, -4), 0x4bff_fffc);
        // `beq` is 0x41820000; BO and BI must survive.
        assert_eq!(b_form(0x4182_0000, 256), 0x4182_0100);
        assert_eq!(b_form(0x4182_0000, -8), 0x4182_fff8);
    }

    #[test]
    fn ds_displacement_leaves_the_opcode_bits_alone() {
        // `ldu` carries a 1 in the low two bits of its displacement halfword.
        assert_eq!(ds_field(0x0001, 16), 0x0011);
        assert_eq!(ds_field(0x0001, -8), 0xfff9);
    }

    #[test]
    fn nop_padding_is_whole_words_of_ori_zero() {
        assert_eq!(
            nop_bytes(Endian::Big, 8),
            vec![0x60, 0, 0, 0, 0x60, 0, 0, 0]
        );
        assert_eq!(nop_bytes(Endian::Little, 4), vec![0, 0, 0, 0x60]);
        // An odd tail belongs to the partial word before it, so it leads.
        assert_eq!(nop_bytes(Endian::Big, 2), vec![0, 0]);
        assert_eq!(nop_bytes(Endian::Big, 7), vec![0, 0, 0, 0x60, 0, 0, 0]);
    }

    #[test]
    fn extended_rotates_match_their_definitions() {
        // `slwi 3, 4, 5` is `rlwinm 3, 4, 5, 0, 26`.
        let RotFields::M { sh, mb, me } = rot1(Rot::Slwi, 5).expect("valid") else {
            panic!("32-bit form")
        };
        assert_eq!((sh, mb, me), (5, 0, 26));
        // `srdi 3, 4, 5` is `rldicl 3, 4, 59, 5`.
        let RotFields::Md { sh, m } = rot1(Rot::Srdi, 5).expect("valid") else {
            panic!("64-bit form")
        };
        assert_eq!((sh, m), (59, 5));
        // `insrdi 3, 4, 5, 6` is `rldimi 3, 4, 53, 6`.
        let RotFields::Md { sh, m } = rot2(Rot2::Insrdi, 5, 6).expect("valid") else {
            panic!("64-bit form")
        };
        assert_eq!((sh, m), (53, 6));
        assert!(rot1(Rot::Slwi, 32).is_err());
        assert!(rot2(Rot2::Extlwi, 0, 0).is_err());
        assert!(rot2(Rot2::Extlwi, 8, 28).is_err());
    }
}
