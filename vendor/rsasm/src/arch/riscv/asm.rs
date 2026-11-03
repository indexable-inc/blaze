//! Turning parsed operands into instruction words.

use super::compress;
use super::encode::{self, Buf, Fix, Insn};
use super::insn;
use super::insn::{Def, Kind, RM, RV64};
use super::operand::{Imm, Mem, Modifier, Operands};
use super::pseudo::AUIPC;
use super::reg::{self, Reg};
use crate::arch::AsmCtx;
use crate::section::{FixupKind, RelocSymbol, Variant};
use crate::source::Span;

pub struct Asm<'c, 'a> {
    pub cx: &'c mut AsmCtx<'a>,
    pub xlen: u8,
    /// Whether the C extension may shorten what is emitted.
    pub rvc: bool,
    /// Whether the linker may relax what this instruction is part of, which
    /// decides both the `R_RISCV_RELAX` marks and what is left to the linker
    /// rather than resolved; see [`super::reloc::relaxable`] and
    /// [`super::reloc::deferred_when_relaxed`].
    relax: bool,
    pub span: Span,
    out: Buf,
    /// A shorter candidate for a single relaxable branch or jump. Layout picks
    /// this one unless the displacement turns out not to fit.
    alt: Option<Buf>,
    /// A longer candidate, for a conditional branch whose target neither the
    /// branch's own reach nor the assembler can be sure of: the opposite
    /// branch over a `jal`. Layout falls back to this one last.
    long: Option<Buf>,
}

impl<'c, 'a> Asm<'c, 'a> {
    pub fn new(
        cx: &'c mut AsmCtx<'a>,
        xlen: u8,
        rvc: bool,
        relax: bool,
        span: Span,
    ) -> Asm<'c, 'a> {
        Asm {
            cx,
            xlen,
            rvc,
            relax,
            span,
            out: Buf::default(),
            alt: None,
            long: None,
        }
    }

    pub fn error(&mut self, span: Span, msg: impl Into<String>) {
        self.cx.error(span, msg);
    }

    pub fn rv64(&self) -> bool {
        self.xlen == 64
    }

    /// Marks a fixup the linker may rewrite whatever its relocation, for the
    /// one case GNU as decides by where the instruction came from rather than
    /// by the relocation alone.
    ///
    /// `R_RISCV_GOT_HI20` is relaxable in the pair `la` and `lga` expand to,
    /// which a linker can turn into a direct reference, and nowhere else:
    /// `md_apply_fix` tests `source_macro`, since an `auipc` the source wrote
    /// itself with `%got_pcrel_hi` may be part of a sequence it does not
    /// recognise.
    pub fn may_relax(&self, kind: FixupKind) -> FixupKind {
        if self.relax {
            kind.marked_by(super::reloc::RELAX)
        } else {
            kind
        }
    }

    /// Emits a 32-bit word, shortening it where the C extension allows.
    pub fn emit(&mut self, word: u32) {
        let insn = match self.rvc.then(|| compress::compress(word, self.xlen)) {
            Some(Some(short)) => Insn::short(short),
            _ => Insn::full(word),
        };
        self.out.push(insn);
    }

    /// Emits a word that must keep its full width, because a fixup will patch
    /// a field the compressed form does not have.
    pub fn emit_fixed(&mut self, insn: Insn) {
        self.out.push(insn);
    }

    pub fn emit_pair(&mut self, first: u32, second: u32, fix: Fix) {
        self.out.push_pair(first, second, fix);
    }

    /// Records the two-byte candidate for a branch that may or may not reach.
    fn set_alt(&mut self, insn: Insn) {
        debug_assert!(self.out.is_empty(), "only a lone branch may relax");
        let mut buf = Buf::default();
        buf.push(insn);
        self.alt = Some(buf);
    }

    /// The candidate a branch takes when its own reach is not enough, or when
    /// the target is a symbol this object cannot see: the branch with its
    /// condition inverted, over a `jal x0` that carries the relocation.
    /// The opposite branch over a `j`, for a target the plain branch cannot
    /// be shown to reach.
    ///
    /// `short_inverse` is the two-byte form of that opposite branch where
    /// the C extension has one. Taking it moves the jump, and so the
    /// distance the branch skips, from eight bytes to six; GNU as and
    /// llvm-mc both do this, and a fuzz run against them is what found it.
    fn set_long(&mut self, word: u32, short_inverse: Option<u16>, target: &Imm) {
        let mut buf = Buf::default();
        match short_inverse {
            Some(w) => buf.push(Insn::short(encode::cb_imm(u64::from(w), 6) as u16)),
            // The B-type immediate for a displacement of 8, which jumps over
            // the four-byte `j`: `imm[4:1]` is `0100` and every other bit is
            // zero.
            None => buf.push(Insn::full((word ^ 0x0000_1000) | 0x0000_0400)),
        }
        buf.push(Insn::full(0x0000_006f).with_fix(target.expr, encode::kind_jal(), target.span));
        self.long = Some(buf);
    }

    pub fn finish(self) -> Vec<Variant> {
        // Shortest first: layout takes the first candidate whose fixups all
        // fit, and grows the choice when one does not.
        let mut out = Vec::with_capacity(3);
        if let Some(short) = self.alt {
            out.push(short.finish());
        }
        out.push(self.out.finish());
        if let Some(long) = self.long {
            out.push(long.finish());
        }
        // GNU as decides an `R_RISCV_RELAX`, and whether a reference to this
        // section is still the assembler's to resolve, in `md_apply_fix`:
        // from the relocation the field ended up with and from whether
        // `.option relax` was in force on the line, which is exactly what is
        // known here.
        if self.relax {
            for variant in &mut out {
                for fixup in &mut variant.fixups {
                    if super::reloc::relaxable(fixup.kind.reloc) {
                        fixup.kind = fixup.kind.marked_by(super::reloc::RELAX);
                    }
                    if let Some(sized) = super::reloc::deferred_when_relaxed(fixup.kind.reloc) {
                        fixup.kind = if sized {
                            fixup.kind.relocated_when_relaxed()
                        } else {
                            fixup.kind.relocated_in_objects()
                        };
                        // A linker that deletes bytes moves the labels after
                        // them and adjusts the symbols it knows, but not an
                        // offset into a section, so such a reference has to
                        // name the label itself; GNU as says the same with
                        // `TC_FORCE_RELOCATION_LOCAL`. A pair's low half
                        // already names the label on its `auipc`.
                        if fixup.kind.reloc_symbol == RelocSymbol::Section {
                            fixup.kind = fixup.kind.with_reloc_symbol(RelocSymbol::Symbol);
                        }
                    }
                }
            }
        }
        out
    }

    // ---- immediates -------------------------------------------------------

    /// An immediate that has to be known now, with its range checked.
    pub fn constant(&mut self, imm: &Imm, lo: i64, hi: i64, what: &str) -> Option<i64> {
        if imm.modifier.is_some() {
            self.error(imm.span, format!("{what} cannot use a relocation modifier"));
            return None;
        }
        let Some(v) = self.cx.constant(imm.expr) else {
            self.error(imm.span, format!("{what} must be known at assembly time"));
            return None;
        };
        if v < lo || v > hi {
            self.error(
                imm.span,
                format!("{what} must be between {lo} and {hi}, but is {v}"),
            );
            return None;
        }
        Some(v)
    }

    /// Places a 12-bit immediate into an I- or S-type word.
    fn imm12(&mut self, word: u32, imm: &Imm, store: bool) -> Option<Insn> {
        let kind = match imm.modifier {
            Some(Modifier::Lo) => encode::kind_abs_lo12(store),
            Some(Modifier::PcrelLo) => encode::kind_lo12(store),
            Some(Modifier::TprelLo) => encode::kind_tprel_lo12(store),
            // A descriptor is loaded and added through I-type instructions,
            // and GNU as reads neither half of one in a store.
            Some(m @ (Modifier::TlsdescLoadLo | Modifier::TlsdescAddLo)) if store => {
                self.error(
                    imm.span,
                    format!("{} belongs in a load or an add, not a store", name(m)),
                );
                return None;
            }
            Some(Modifier::TlsdescLoadLo) => {
                encode::kind_tlsdesc_lo12(super::reloc::TLSDESC_LOAD_LO12)
            }
            Some(Modifier::TlsdescAddLo) => {
                encode::kind_tlsdesc_lo12(super::reloc::TLSDESC_ADD_LO12)
            }
            // Neither mark fills anything in, so neither is an immediate the
            // instruction it was written on can hold.
            Some(m @ Modifier::TprelAdd) => {
                self.error(
                    imm.span,
                    format!("{} marks an `add` of the thread pointer", name(m)),
                );
                return None;
            }
            Some(m @ Modifier::TlsdescCall) => {
                self.error(
                    imm.span,
                    format!(
                        "{} marks the `jalr` of a descriptor, as its third operand",
                        name(m)
                    ),
                );
                return None;
            }
            Some(m) => {
                self.error(imm.span, format!("{} does not fit a 12-bit field", name(m)));
                return None;
            }
            None => match self.cx.constant(imm.expr) {
                Some(v) => {
                    if !(-2048..=2047).contains(&v) {
                        self.error(
                            imm.span,
                            format!("immediate {v} does not fit a signed 12-bit field"),
                        );
                        return None;
                    }
                    let full = if store {
                        encode::s_imm(word as u64, v)
                    } else {
                        encode::i_imm(word as u64, v)
                    };
                    return Some(Insn::full(full as u32));
                }
                None if store => encode::kind_s(),
                None => encode::kind_i(),
            },
        };
        Some(Insn::full(word).with_fix(imm.expr, kind, imm.span))
    }

    /// Places a 20-bit immediate into a `lui` or `auipc`.
    fn imm20(&mut self, word: u32, imm: &Imm) -> Option<Insn> {
        let kind = match imm.modifier {
            Some(Modifier::Hi) => encode::kind_hi20(false),
            Some(Modifier::PcrelHi) => encode::kind_hi20(true),
            Some(Modifier::GotPcrelHi) => encode::kind_got_hi20(),
            // GNU as reads every thread-local high half in either `lui` or
            // `auipc`, though only one of the two makes an address the rest
            // of the sequence can use; llvm-mc pairs each with its own
            // instruction and refuses the other.
            Some(Modifier::TprelHi) => encode::kind_tls_hi20(super::reloc::TPREL_HI20),
            Some(Modifier::TlsIePcrelHi) => encode::kind_tls_hi20(super::reloc::TLS_GOT_HI20),
            Some(Modifier::TlsGdPcrelHi) => encode::kind_tls_hi20(super::reloc::TLS_GD_HI20),
            Some(Modifier::TlsdescHi) => encode::kind_tls_hi20(super::reloc::TLSDESC_HI20),
            Some(m) => {
                self.error(
                    imm.span,
                    format!("{} belongs in the low half of an address", name(m)),
                );
                return None;
            }
            None => match self.cx.constant(imm.expr) {
                Some(v) => {
                    if !(-(1 << 19)..(1 << 20)).contains(&v) {
                        self.error(
                            imm.span,
                            format!("immediate {v} does not fit the 20-bit field of `lui`"),
                        );
                        return None;
                    }
                    return Some(Insn::full(encode::u_imm(word as u64, v) as u32));
                }
                None => encode::kind_u(),
            },
        };
        Some(Insn::full(word).with_fix(imm.expr, kind, imm.span))
    }

    pub fn no_modifier(&mut self, imm: &Imm, what: &str) -> Option<()> {
        if imm.modifier.is_some() {
            self.error(imm.span, format!("{what} cannot use a relocation modifier"));
            return None;
        }
        Some(())
    }

    // ---- branches and jumps ----------------------------------------------

    /// A conditional branch, with its compressed candidate when it has one.
    pub fn branch(&mut self, base: u32, rs1: Reg, rs2: Reg, target: &Imm) -> Option<()> {
        self.no_modifier(target, "a branch target")?;
        let word = encode::rs2(encode::rs1(base, rs1.bits()), rs2.bits());
        // `c.beqz` and `c.bnez` compare against zero only and reach +-256
        // bytes rather than +-4 KiB, so both forms go to layout, which keeps
        // the short one only if the target turns out to be close enough.
        let funct3 = (base >> 12) & 7;
        let mut short_inverse = None;
        if self.rvc && funct3 <= 1 {
            let (test, other) = if rs2 == reg::ZERO {
                (rs1, rs2)
            } else {
                (rs2, rs1)
            };
            if other == reg::ZERO && test.is_popular() {
                let short = compress::c_branch(funct3 == 0, test);
                self.set_alt(Insn::short(short).with_fix(
                    target.expr,
                    encode::kind_cb(),
                    target.span,
                ));
                // `c.beqz` and `c.bnez` are each other's opposite, so a
                // branch that has a two-byte form has a two-byte opposite.
                short_inverse = Some(compress::c_branch(funct3 != 0, test));
            }
        }
        self.emit_fixed(Insn::full(word).with_fix(target.expr, encode::kind_branch(), target.span));
        // A conditional branch reaches +-4 KiB, which is not far enough for a
        // target in another object, nor for one further off in this one:
        // both references write the branch inverted over a `j`, whose
        // +-1 MiB the linker can also relax further. Layout takes it only
        // when the plain branch does not reach, which for a symbol this
        // object cannot see is always.
        self.set_long(word, short_inverse, target);
        Some(())
    }

    /// `jal rd, target`, with `c.j` or `c.jal` where they apply.
    pub fn jal(&mut self, rd: Reg, target: &Imm) -> Option<()> {
        self.no_modifier(target, "a jump target")?;
        let word = encode::rd(0x0000_006f, rd.bits());
        if self.rvc {
            // RV64 reuses the `c.jal` encoding for `c.addiw`, so only RV32 can
            // shorten a call that links through `ra`.
            let link = rd == reg::RA && self.xlen == 32;
            if rd == reg::ZERO || link {
                let short = compress::c_jump(link);
                self.set_alt(Insn::short(short).with_fix(
                    target.expr,
                    encode::kind_cj(),
                    target.span,
                ));
            }
        }
        self.emit_fixed(Insn::full(word).with_fix(target.expr, encode::kind_jal(), target.span));
        Some(())
    }

    /// The `auipc`/`jalr` pair behind `call`, `tail` and `jump`.
    pub fn call_pair(&mut self, first: u32, second: u32, target: &Imm) {
        self.emit_pair(
            first,
            second,
            Fix {
                expr: target.expr,
                kind: encode::kind_call(),
                span: target.span,
            },
        );
    }

    /// An `auipc` and the instruction that takes the low half of the same
    /// address from it, as `la`, `lga` and `lw a0, sym` expand to. Unlike
    /// `call`'s pair, each half has a relocation of its own.
    pub fn auipc_split(
        &mut self,
        auipc: u32,
        second: u32,
        target: &Imm,
        hi: FixupKind,
        lo: FixupKind,
    ) {
        // The low half's relocation names a label at the `auipc`, which the
        // core puts at the start of the fragment.
        debug_assert!(
            self.out.is_empty(),
            "an `auipc` pair must start its fragment"
        );
        self.emit_fixed(Insn::full(auipc).with_fix(target.expr, hi, target.span));
        self.emit_fixed(Insn::full(second).with_fix(target.expr, lo, target.span));
    }

    /// `lw a0, sym` and `sw a0, sym, t0`: an access to a symbol's address
    /// through an `auipc` into `base`, which a load can share with its
    /// destination but a store or a floating-point load cannot.
    ///
    /// llvm-mc takes this form only for a symbol, refusing a plain number
    /// where the `(reg)` was left off, and so does this.
    fn symbol_access(&mut self, word: u32, base: Reg, target: &Imm, store: bool) -> Option<()> {
        self.no_modifier(target, "a symbol address")?;
        if self.cx.constant(target.expr).is_some() {
            self.error(target.span, "expected an address of the form `offset(reg)`");
            return None;
        }
        let auipc = encode::rd(AUIPC, base.bits());
        let second = encode::rs1(word, base.bits());
        self.auipc_split(
            auipc,
            second,
            target,
            encode::kind_hi20(true),
            encode::kind_pair_lo12(store),
        );
        Some(())
    }

    /// `op rd, rs1, rs2`, for pseudo-instructions that expand to one.
    pub fn r_type(&mut self, base: u32, rd: Reg, rs1: Reg, rs2: Reg) {
        let w = encode::rs2(
            encode::rs1(encode::rd(base, rd.bits()), rs1.bits()),
            rs2.bits(),
        );
        self.emit(w);
    }

    /// `op rd, rs1, imm` with the immediate already known.
    pub fn i_const(&mut self, base: u32, rd: Reg, rs1: Reg, imm: i64) {
        let w = encode::rs1(encode::rd(base, rd.bits()), rs1.bits());
        self.emit(encode::i_imm(w as u64, imm) as u32);
    }

    /// `op rd, rs1, shamt`. A shift is not an I-type: `funct7` sits in the
    /// top of what would be the immediate, so the amount goes in on its own
    /// rather than through `i_imm`, which would wipe it out.
    pub fn shift_const(&mut self, base: u32, rd: Reg, rs1: Reg, shamt: u32) {
        let w = encode::rs1(encode::rd(base, rd.bits()), rs1.bits());
        self.emit(w | (shamt << 20));
    }

    /// `op rd, rs1, imm` where the immediate came from the source.
    pub fn i_expr(&mut self, base: u32, rd: Reg, rs1: Reg, imm: &Imm) -> Option<()> {
        let w = encode::rs1(encode::rd(base, rd.bits()), rs1.bits());
        let insn = self.imm12(w, imm, false)?;
        self.emit_or_compress(insn);
        Some(())
    }

    // ---- the table --------------------------------------------------------

    /// `extra` carries bits the mnemonic itself set, such as the `.aq` and
    /// `.rl` ordering flags of an atomic.
    pub fn encode_def(
        &mut self,
        def: &Def,
        name: &str,
        ops: &Operands<'_>,
        extra: u32,
    ) -> Option<()> {
        if def.flags & RV64 != 0 && !self.rv64() {
            self.error(
                self.span,
                format!("`{name}` is an RV64 instruction, but the target is RV32"),
            );
            return None;
        }
        // A rounding mode is an optional extra operand, so it is taken off the
        // end before the fixed operands are counted.
        let (rm, count) = self.rounding_mode(def, ops);
        let base = match rm {
            Some(m) => encode::funct3(def.base, m),
            None => def.base,
        } | extra;
        self.encode_form(def, base, name, ops, count)
    }

    /// Splits a trailing rounding mode off the operand list.
    fn rounding_mode(&mut self, def: &Def, ops: &Operands<'_>) -> (Option<u32>, usize) {
        let n = ops.len();
        if def.flags & RM == 0 || n == 0 {
            return (None, n);
        }
        match ops
            .word(self.cx, n - 1)
            .as_deref()
            .and_then(insn::rounding_mode)
        {
            Some(m) => (Some(m), n - 1),
            None => (None, n),
        }
    }

    fn encode_form(
        &mut self,
        def: &Def,
        base: u32,
        name: &str,
        ops: &Operands<'_>,
        count: usize,
    ) -> Option<()> {
        let want: &[usize] = match def.kind {
            Kind::Nullary => &[0],
            Kind::Fence => &[0, 2],
            Kind::U | Kind::F2 | Kind::FToX | Kind::XToF | Kind::Load | Kind::AmoLoad => &[2],
            // The third operand is the scratch register of the symbol form.
            Kind::Store | Kind::FLoad | Kind::FStore => &[2, 3],
            Kind::Jal => &[1, 2],
            Kind::Jalr => &[1, 2, 3],
            Kind::F4 => &[4],
            // `add rd, rs1, tp, %tprel_add(sym)` carries the mark that ties
            // the add of the thread pointer to the rest of a local-exec
            // sequence. Only `add` takes it, as in GNU as.
            Kind::R if name == "add" => &[3, 4],
            _ => &[3],
        };
        if !want.contains(&count) {
            let list: Vec<String> = want.iter().map(|n| n.to_string()).collect();
            self.error(
                ops.span,
                format!(
                    "`{name}` takes {} operand(s), but {count} were given",
                    list.join(" or ")
                ),
            );
            return None;
        }

        match def.kind {
            Kind::R => {
                let (rd, rs1, rs2) = (
                    ops.xreg(self.cx, 0)?,
                    ops.xreg(self.cx, 1)?,
                    ops.xreg(self.cx, 2)?,
                );
                let w = encode::rs2(
                    encode::rs1(encode::rd(base, rd.bits()), rs1.bits()),
                    rs2.bits(),
                );
                if count == 4 {
                    let mark = ops.imm(self.cx, 3)?;
                    if mark.modifier != Some(Modifier::TprelAdd) {
                        self.error(mark.span, "expected `%tprel_add(symbol)`");
                        return None;
                    }
                    // The mark keeps the instruction at full width: `add a0,
                    // a0, tp` has a two-byte form, and a relocation on it
                    // would have nothing to name.
                    self.emit_fixed(Insn::full(w).with_fix(
                        mark.expr,
                        encode::kind_tprel_add(),
                        mark.span,
                    ));
                    return Some(());
                }
                self.emit(w);
            }
            Kind::I => {
                let (rd, rs1) = (ops.xreg(self.cx, 0)?, ops.xreg(self.cx, 1)?);
                let imm = ops.imm(self.cx, 2)?;
                let w = encode::rs1(encode::rd(base, rd.bits()), rs1.bits());
                let insn = self.imm12(w, &imm, false)?;
                self.emit_or_compress(insn);
            }
            Kind::Shift | Kind::ShiftW => {
                let (rd, rs1) = (ops.xreg(self.cx, 0)?, ops.xreg(self.cx, 1)?);
                let imm = ops.imm(self.cx, 2)?;
                let max = if def.kind == Kind::ShiftW {
                    31
                } else {
                    self.xlen as i64 - 1
                };
                let n = self.constant(&imm, 0, max, "a shift amount")?;
                let w = encode::rs1(encode::rd(base, rd.bits()), rs1.bits()) | ((n as u32) << 20);
                self.emit(w);
            }
            Kind::Load | Kind::FLoad => {
                let rd = if def.kind == Kind::Load {
                    ops.xreg(self.cx, 0)?
                } else {
                    ops.freg(self.cx, 0)?
                };
                // `lw a0, sym` loads through its own destination; `flw` needs
                // an integer register to hold the address instead.
                let symbol_form = if def.kind == Kind::Load {
                    !ops.ends_in_group(1)
                } else {
                    count == 3
                };
                if symbol_form {
                    let via = if def.kind == Kind::Load {
                        rd
                    } else {
                        ops.xreg(self.cx, 2)?
                    };
                    let target = ops.imm(self.cx, 1)?;
                    let w = encode::rd(base, rd.bits());
                    return self.symbol_access(w, via, &target, false);
                }
                let mem = ops.mem(self.cx, 1)?;
                let w = encode::rs1(encode::rd(base, rd.bits()), mem.base.bits());
                let insn = self.mem_offset(w, &mem, false)?;
                self.emit_or_compress(insn);
            }
            Kind::Store | Kind::FStore => {
                let rs2 = if def.kind == Kind::Store {
                    ops.xreg(self.cx, 0)?
                } else {
                    ops.freg(self.cx, 0)?
                };
                if count == 3 {
                    let target = ops.imm(self.cx, 1)?;
                    let tmp = ops.xreg(self.cx, 2)?;
                    let w = encode::rs2(base, rs2.bits());
                    return self.symbol_access(w, tmp, &target, true);
                }
                let mem = ops.mem(self.cx, 1)?;
                let w = encode::rs1(encode::rs2(base, rs2.bits()), mem.base.bits());
                let insn = self.mem_offset(w, &mem, true)?;
                self.emit_or_compress(insn);
            }
            Kind::Branch => {
                let (rs1, rs2) = (ops.xreg(self.cx, 0)?, ops.xreg(self.cx, 1)?);
                let target = ops.imm(self.cx, 2)?;
                self.branch(base, rs1, rs2, &target)?;
            }
            Kind::U => {
                let rd = ops.xreg(self.cx, 0)?;
                let imm = ops.imm(self.cx, 1)?;
                let insn = self.imm20(encode::rd(base, rd.bits()), &imm)?;
                self.emit_or_compress(insn);
            }
            Kind::Jal => {
                let (rd, target) = if count == 1 {
                    (reg::RA, ops.imm(self.cx, 0)?)
                } else {
                    (ops.xreg(self.cx, 0)?, ops.imm(self.cx, 1)?)
                };
                self.jal(rd, &target)?;
            }
            Kind::Jalr => self.jalr(base, ops, count)?,
            Kind::Amo | Kind::AmoLoad => {
                let rd = ops.xreg(self.cx, 0)?;
                let (rs2, mem) = if def.kind == Kind::Amo {
                    (ops.xreg(self.cx, 1)?, ops.mem(self.cx, 2)?)
                } else {
                    (reg::ZERO, ops.mem(self.cx, 1)?)
                };
                // The A extension has no displacement field at all.
                if let Some(off) = &mem.off
                    && self.cx.constant(off.expr) != Some(0)
                {
                    self.error(
                        off.span,
                        "an atomic instruction addresses `(reg)` with no offset",
                    );
                    return None;
                }
                let w = encode::rs1(
                    encode::rs2(encode::rd(base, rd.bits()), rs2.bits()),
                    mem.base.bits(),
                );
                self.emit(w);
            }
            Kind::F3 => {
                let (rd, rs1, rs2) = (
                    ops.freg(self.cx, 0)?,
                    ops.freg(self.cx, 1)?,
                    ops.freg(self.cx, 2)?,
                );
                let w = encode::rs2(
                    encode::rs1(encode::rd(base, rd.bits()), rs1.bits()),
                    rs2.bits(),
                );
                self.emit(w);
            }
            Kind::F2 => {
                let (rd, rs1) = (ops.freg(self.cx, 0)?, ops.freg(self.cx, 1)?);
                self.emit(encode::rs1(encode::rd(base, rd.bits()), rs1.bits()));
            }
            Kind::F4 => {
                let (rd, rs1, rs2, rs3) = (
                    ops.freg(self.cx, 0)?,
                    ops.freg(self.cx, 1)?,
                    ops.freg(self.cx, 2)?,
                    ops.freg(self.cx, 3)?,
                );
                let w = encode::rs3(
                    encode::rs2(
                        encode::rs1(encode::rd(base, rd.bits()), rs1.bits()),
                        rs2.bits(),
                    ),
                    rs3.bits(),
                );
                self.emit(w);
            }
            Kind::FCmp => {
                let (rd, rs1, rs2) = (
                    ops.xreg(self.cx, 0)?,
                    ops.freg(self.cx, 1)?,
                    ops.freg(self.cx, 2)?,
                );
                let w = encode::rs2(
                    encode::rs1(encode::rd(base, rd.bits()), rs1.bits()),
                    rs2.bits(),
                );
                self.emit(w);
            }
            Kind::FToX => {
                let (rd, rs1) = (ops.xreg(self.cx, 0)?, ops.freg(self.cx, 1)?);
                self.emit(encode::rs1(encode::rd(base, rd.bits()), rs1.bits()));
            }
            Kind::XToF => {
                let (rd, rs1) = (ops.freg(self.cx, 0)?, ops.xreg(self.cx, 1)?);
                self.emit(encode::rs1(encode::rd(base, rd.bits()), rs1.bits()));
            }
            Kind::Csr | Kind::CsrI => {
                let rd = ops.xreg(self.cx, 0)?;
                let csr = self.csr(ops, 1)?;
                let w = encode::rd(base, rd.bits()) | (csr << 20);
                let w = if def.kind == Kind::Csr {
                    encode::rs1(w, ops.xreg(self.cx, 2)?.bits())
                } else {
                    let imm = ops.imm(self.cx, 2)?;
                    let v = self.constant(&imm, 0, 31, "a CSR immediate")?;
                    encode::rs1(w, v as u32)
                };
                self.emit(w);
            }
            Kind::Fence => {
                let (pred, succ) = if count == 2 {
                    (self.fence_set(ops, 0)?, self.fence_set(ops, 1)?)
                } else {
                    (0xf, 0xf)
                };
                self.emit(base | (pred << 24) | (succ << 20));
            }
            // Binutils has two `unimp` rows: `c.unimp` for the C extension
            // and the four-byte one for the base ISA, and it takes the
            // shorter where it can. This is that choice, not a compression:
            // `csrrw x0, cycle, x0`, the same word written out, stays wide.
            Kind::Nullary if name == "unimp" && self.rvc => {
                self.emit_fixed(Insn::short(compress::C_UNIMP));
            }
            Kind::Nullary => self.emit(base),
        }
        Some(())
    }

    /// `jalr rs1`, `jalr rd, rs1`, `jalr rd, off(rs1)` and `jalr rd, rs1, off`
    /// are all spellings of the same instruction.
    fn jalr(&mut self, base: u32, ops: &Operands<'_>, count: usize) -> Option<()> {
        let (rd, mem) = match count {
            1 => (reg::RA, self.address_or_reg(ops, 0)?),
            // `jalr rs, off` links into `ra` and adds the offset to `rs`,
            // where `jalr rd, rs` links into `rd`: which one it is depends on
            // whether the second operand is a register, exactly as it does in
            // GNU as.
            2 if !ops.is_reg(self.cx, 1) && !ops.looks_like_mem(self.cx, 1) => {
                let rs1 = ops.xreg(self.cx, 0)?;
                let off = ops.imm(self.cx, 1)?;
                (
                    reg::RA,
                    Mem {
                        base: rs1,
                        off: Some(off),
                        span: ops.piece_span(1),
                    },
                )
            }
            2 => (ops.xreg(self.cx, 0)?, self.address_or_reg(ops, 1)?),
            _ => {
                let rd = ops.xreg(self.cx, 0)?;
                let rs1 = ops.xreg(self.cx, 1)?;
                let off = ops.imm(self.cx, 2)?;
                // `jalr rd, rs1, %tlsdesc_call(label)` is the call through a
                // descriptor, and this spelling is the only one GNU as reads
                // it in. The mark sits where the displacement would, but it
                // fills nothing in: it only tells the linker which `jalr`
                // ends the sequence.
                if off.modifier == Some(Modifier::TlsdescCall) {
                    let w = encode::rs1(encode::rd(base, rd.bits()), rs1.bits());
                    self.emit_fixed(Insn::full(w).with_fix(
                        off.expr,
                        encode::kind_tlsdesc_call(),
                        off.span,
                    ));
                    return Some(());
                }
                (
                    rd,
                    Mem {
                        base: rs1,
                        off: Some(off),
                        span: ops.piece_span(2),
                    },
                )
            }
        };
        let w = encode::rs1(encode::rd(base, rd.bits()), mem.base.bits());
        let insn = self.mem_offset(w, &mem, false)?;
        self.emit_or_compress(insn);
        Some(())
    }

    fn address_or_reg(&mut self, ops: &Operands<'_>, i: usize) -> Option<Mem> {
        if ops.looks_like_mem(self.cx, i) {
            return ops.mem(self.cx, i);
        }
        let base = ops.xreg(self.cx, i)?;
        Some(Mem {
            base,
            off: None,
            span: ops.piece_span(i),
        })
    }

    fn mem_offset(&mut self, word: u32, mem: &Mem, store: bool) -> Option<Insn> {
        match &mem.off {
            Some(imm) => self.imm12(word, imm, store),
            None => Some(Insn::full(word)),
        }
    }

    /// Emits an instruction that may still be shortened, which is only true
    /// when its immediate turned out to be a constant.
    fn emit_or_compress(&mut self, insn: Insn) {
        if insn.fix.is_none() {
            self.emit(insn.word);
        } else {
            self.emit_fixed(insn);
        }
    }

    pub fn csr(&mut self, ops: &Operands<'_>, i: usize) -> Option<u32> {
        if let Some(word) = ops.word(self.cx, i)
            && let Some(n) = insn::csr(&word)
        {
            return Some(n);
        }
        let imm = ops.imm(self.cx, i)?;
        let v = self.constant(&imm, 0, 4095, "a CSR number")?;
        Some(v as u32)
    }

    fn fence_set(&mut self, ops: &Operands<'_>, i: usize) -> Option<u32> {
        let span = ops.piece_span(i);
        match ops.word(self.cx, i).as_deref().and_then(insn::fence_set) {
            Some(bits) => Some(bits),
            None => {
                self.error(span, "expected some combination of the letters `iorw`");
                None
            }
        }
    }
}

fn name(m: Modifier) -> &'static str {
    match m {
        Modifier::Hi => "`%hi`",
        Modifier::Lo => "`%lo`",
        Modifier::PcrelHi => "`%pcrel_hi`",
        Modifier::PcrelLo => "`%pcrel_lo`",
        Modifier::GotPcrelHi => "`%got_pcrel_hi`",
        Modifier::TprelHi => "`%tprel_hi`",
        Modifier::TprelLo => "`%tprel_lo`",
        Modifier::TprelAdd => "`%tprel_add`",
        Modifier::TlsIePcrelHi => "`%tls_ie_pcrel_hi`",
        Modifier::TlsGdPcrelHi => "`%tls_gd_pcrel_hi`",
        Modifier::TlsdescHi => "`%tlsdesc_hi`",
        Modifier::TlsdescLoadLo => "`%tlsdesc_load_lo`",
        Modifier::TlsdescAddLo => "`%tlsdesc_add_lo`",
        Modifier::TlsdescCall => "`%tlsdesc_call`",
    }
}
