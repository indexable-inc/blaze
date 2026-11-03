//! Turning matched operands into instruction words.
//!
//! Every MIPS instruction is exactly one 32-bit word, so encoding is a matter
//! of ORing operand fields into the definition's fixed bits. What is not
//! trivial is the values the assembler cannot compute yet: those become
//! [`Fixup`]s carrying a [`FieldEncoding::Scatter`](crate::section::FieldEncoding::Scatter)
//! function that knows where
//! in the word the field lives.

use super::insn::{Def, Form};
use super::operand::{Imm, Operand, RelocMod, RelocMods};
use super::reg::{self, Reg};
use super::reloc;
use crate::arch::AsmCtx;
use crate::expr::ExprRef;
use crate::section::{Fixup, FixupKind, LinkValue, Variant};
use crate::source::Span;

// ---- field placement ------------------------------------------------------

pub const fn rs(r: u8) -> u32 {
    (r as u32) << 21
}
pub const fn rt(r: u8) -> u32 {
    (r as u32) << 16
}
pub const fn rd(r: u8) -> u32 {
    (r as u32) << 11
}
pub const fn sa(v: u32) -> u32 {
    (v & 0x1f) << 6
}
/// The flag number a COP1 branch tests and a conditional move reads, which
/// sits in the `rt` field just above the `tf` bit the mnemonic fixes.
pub const fn cc(n: u8) -> u32 {
    (n as u32) << 18
}
/// The flag number `c.cond.fmt` writes, which is three bits of the `sa` field
/// instead: the comparison itself needs both register fields above it.
pub const fn cc_lo(n: u8) -> u32 {
    (n as u32) << 8
}
pub const fn imm(v: i64) -> u32 {
    (v as u32) & 0xffff
}

// ---- scatter functions ----------------------------------------------------
//
// Each takes the word already emitted and the resolved value, and returns the
// patched word. They are `fn` pointers stored in the fixup, so the bit layout
// stays next to the instruction that uses it.

/// A plain 16-bit immediate in the low half of an I-format word.
pub(crate) fn field_imm16(word: u64, v: i64) -> u64 {
    (word & 0xffff_0000) | ((v as u64) & 0xffff)
}

/// `%hi`: bits 31..16, biased by 0x8000 so that adding the *sign-extended*
/// `%lo` of the same address gets back to the address itself.
pub(crate) fn field_hi16(word: u64, v: i64) -> u64 {
    (word & 0xffff_0000) | (((v.wrapping_add(0x8000) >> 16) as u64) & 0xffff)
}

/// I-format branch: a 16-bit field counted in instruction words.
fn field_branch16(word: u64, v: i64) -> u64 {
    (word & 0xffff_0000) | (((v >> 2) as u64) & 0xffff)
}

/// J-format target: 26 bits of *word index*, not a displacement. The hardware
/// forms the destination as `(delay_slot_pc & 0xf0000000) | (index << 2)`, so
/// the top four bits of the target come from where the jump sits, not from
/// the instruction. Only bits 27..2 are stored.
fn field_target26(word: u64, v: i64) -> u64 {
    (word & 0xfc00_0000) | (((v >> 2) as u64) & 0x03ff_ffff)
}

/// The fixup kind for a conditional branch. Branches are measured from the
/// delay slot, one word past the branch itself, which is what `adjust = 4`
/// says. 18 bits of value: 16 encoded plus the two the word alignment supplies.
///
/// The operand is always a target *address*, even when it is a bare number:
/// `beq $a0, $a1, 8` branches to address 8, as a label at 8 would. llvm-mc
/// reads a bare number as the displacement itself instead, so the corpus only
/// ever branches to labels.
pub fn branch_fixup() -> FixupKind {
    FixupKind::pcrel(4, 4)
        .with_field(18, 4)
        .with_reloc(reloc::PC16)
        .scatter(field_branch16)
}

/// The fixup kind for `j` / `jal`. The value is the absolute target, and it is
/// range-checked as a full 64-bit address: `jal 0x80001000` from kernel code
/// at 0x80000000 is perfectly legal, even though the target does not fit in 28
/// bits. What would *not* be legal is a target in a different 256 MB region
/// from the delay slot, for a label. That check needs the instruction's own
/// address, so rsasm makes it only in a flat image, where it does what GNU ld
/// does at link time; alignment is always checked.
pub fn jump_fixup() -> FixupKind {
    FixupKind::data(4)
        .with_field(64, 4)
        .with_reloc(reloc::R26)
        .link(LinkValue::Region {
            bits: 28,
            numbers: false,
        })
        .scatter(field_target26)
}

/// `%higher`: bits 47..32, biased so that the three fields below it
/// reconstruct the address; and `%highest`, bits 63..48, biased again for
/// the same reason.
fn field_higher(word: u64, v: i64) -> u64 {
    (word & 0xffff_0000) | (((v.wrapping_add(0x8000_8000) >> 32) as u64) & 0xffff)
}

fn field_highest(word: u64, v: i64) -> u64 {
    (word & 0xffff_0000) | (((v.wrapping_add(0x8000_8000_8000) >> 48) as u64) & 0xffff)
}

/// The fixup for a 16-bit field, given the chain of operators on it.
///
/// A chain of more than one is an n64 composite, whose relocation types are
/// packed in the order the ABI applies them; the field itself is then
/// whatever the innermost operator would put there, which for a composite
/// is nothing, since n64 keeps its addend in the relocation entry.
fn imm16_fixup(mods: RelocMods) -> FixupKind {
    let base = FixupKind::data(4).scatter(field_imm16);
    let inner = mods.inner();
    let kind = match inner {
        RelocMod::Hi => FixupKind::data(4)
            .with_reloc(reloc::HI16)
            .scatter(field_hi16),
        // `%lo` deliberately truncates, so the field takes any 32-bit value.
        RelocMod::Lo => base.with_reloc(reloc::LO16),
        // A bare immediate must actually fit; it gets the same relocation as
        // `%lo` because both name the low half of an address.
        RelocMod::None => base.with_field(16, 1).with_reloc(reloc::LO16),
        RelocMod::Higher => FixupKind::data(4)
            .with_reloc(reloc::HIGHER)
            .scatter(field_higher),
        RelocMod::Highest => FixupKind::data(4)
            .with_reloc(reloc::HIGHEST)
            .scatter(field_highest),
        // Both references relocate `%half` of a label rather than resolve
        // it, as they do the other parts of an address.
        RelocMod::Half => base
            .with_reloc(reloc::R16)
            .with_addend_limits(-0x8000, 0x7fff)
            .relocated_in_objects(),
        RelocMod::TlsGd => tls_fixup(reloc::TLS_GD, false),
        RelocMod::TlsLdm => tls_fixup(reloc::TLS_LDM, false),
        RelocMod::DtprelHi => tls_fixup(reloc::TLS_DTPREL_HI16, true),
        RelocMod::DtprelLo => tls_fixup(reloc::TLS_DTPREL_LO16, false),
        RelocMod::Gottprel => tls_fixup(reloc::TLS_GOTTPREL, false),
        RelocMod::TprelHi => tls_fixup(reloc::TLS_TPREL_HI16, true),
        RelocMod::TprelLo => tls_fixup(reloc::TLS_TPREL_LO16, false),
        // The position-independent operators. Each names a place in the GOT
        // or a distance from `$gp`, neither of which exists before the
        // link, so none of them is ever computed here. `%neg` is in the
        // same boat: it only ever negates one of the others.
        m => {
            let got = !matches!(m, RelocMod::GpRel | RelocMod::Neg);
            let high = matches!(m, RelocMod::GotHi | RelocMod::CallHi);
            let k = got_fixup(m.reloc(), high, got);
            if signed_addend(m) {
                k.with_addend_limits(-0x8000, 0x7fff)
            } else {
                k
            }
        }
    };
    kind.with_reloc(mods.reloc())
}

/// Whether BFD's howto for this operator is `complain_overflow_signed`, so
/// that GNU as answers "relocation overflow" for an addend wider than the
/// field it goes in. The operators that hold a whole offset from `$gp` are
/// marked so; the halves of one — `%got_hi`, `%got_lo` and their `%call`
/// twins — are not, holding part of a value being the point of them.
fn signed_addend(m: RelocMod) -> bool {
    matches!(
        m,
        RelocMod::Got
            | RelocMod::Call16
            | RelocMod::GotDisp
            | RelocMod::GotPage
            | RelocMod::GotOfst
            | RelocMod::GpRel
    )
}

/// The field of a thread-local operator, given the relocation both references
/// write for it and whether it is the high half of an offset.
///
/// Nothing here computes one: the value is an offset into a thread's block,
/// or the offset of a GOT entry holding one, and the linker lays both out —
/// so even a variable defined in this file is relocated, and a flat image
/// has no answer at all. What the field does hold under `REL` is the addend,
/// which GNU as writes the same way as for `%hi` and `%lo`: biased by 0x8000
/// on a high half, plain on a low one. llvm-mc drops the addend of a
/// thread-local relocation instead (see `tools/mc-diff/mips-relocs.txt`).
fn tls_fixup(reloc: u32, high: bool) -> FixupKind {
    FixupKind::data(4)
        .with_reloc(reloc)
        .with_class(crate::reloc::RelocClass::ThreadLocal)
        .linker_only()
        .scatter(if high { field_hi16 } else { field_imm16 })
}

/// The field of a position-independent operator, which is the same story as
/// [`tls_fixup`]: the GOT is the linker's, and `$gp` points into it, so
/// neither an object nor a flat image can work the value out. Under `REL`
/// the field holds the addend, biased by 0x8000 on the half `%got_hi` and
/// `%call_hi` name, exactly as `%hi` writes it, and plain everywhere else.
/// `got` says the value is a place in the GOT rather than a distance from
/// `$gp`, which is all a writer that does not number relocations as ELF
/// does could tell them apart by.
fn got_fixup(reloc: u32, high: bool, got: bool) -> FixupKind {
    let class = if got {
        crate::reloc::RelocClass::Got
    } else {
        crate::reloc::RelocClass::Plain
    };
    FixupKind::data(4)
        .with_reloc(reloc)
        .with_class(class)
        .linker_only()
        .scatter(if high { field_hi16 } else { field_imm16 })
}

// ---- the word builder -----------------------------------------------------

/// The instruction words of one statement, plus the fixups into them.
///
/// Most statements produce one word; macro expansions such as `li` produce
/// two, and the fixup offsets have to follow.
pub struct Words {
    words: Vec<u32>,
    fixups: Vec<Fixup>,
    endian: crate::arch::Endian,
}

impl Words {
    pub fn new(endian: crate::arch::Endian) -> Words {
        Words {
            words: Vec::new(),
            fixups: Vec::new(),
            endian,
        }
    }

    pub fn push(&mut self, word: u32) {
        self.words.push(word);
    }

    /// Emits `word` with `expr` to be patched into it once it is known.
    pub fn push_fixup(&mut self, word: u32, expr: ExprRef, kind: FixupKind, span: Span) {
        let offset = (self.words.len() * 4) as u32;
        self.words.push(word);
        self.fixups.push(Fixup {
            offset,
            expr,
            kind,
            span,
        });
    }

    pub fn finish(self) -> Variant {
        let mut bytes = Vec::with_capacity(self.words.len() * 4);
        for w in &self.words {
            bytes.extend_from_slice(&self.endian.bytes(*w as u64, 4));
        }
        Variant {
            bytes,
            fixups: self.fixups,
        }
    }
}

// ---- immediates -----------------------------------------------------------

/// What a 16-bit immediate field accepts.
///
/// GNU as and llvm-mc both take either spelling of the same bit pattern, so
/// `ori $a0, $a0, 0xffff` and `addiu $a0, $a0, -1` are each fine and mean
/// what they look like.
pub const IMM16_LO: i64 = -0x8000;
pub const IMM16_HI: i64 = 0xffff;

/// Places a 16-bit immediate: folds it into `word` if it is already known,
/// and otherwise records a fixup.
pub fn place_imm16(
    cx: &mut AsmCtx<'_>,
    w: &mut Words,
    word: u32,
    v: Imm,
    what: &str,
) -> Option<()> {
    // A chain of operators is never worked out here, whatever it wraps:
    // GNU as resolves a constant by the innermost operator alone, and only
    // when the fixup is not part of a composite one.
    let single = v.mods.as_slice().len() <= 1;
    match v.mods.inner() {
        RelocMod::None => match cx.constant(v.expr) {
            Some(n) => {
                if !(IMM16_LO..=IMM16_HI).contains(&n) {
                    cx.error(
                        v.span,
                        format!("{what} {n} does not fit in a 16-bit field (-32768 to 65535)"),
                    );
                    return None;
                }
                w.push(word | imm(n));
            }
            None => w.push_fixup(word, v.expr, imm16_fixup(v.mods), v.span),
        },
        // The parts of an address a file can settle for itself, which GNU
        // as's `calculate_reloc` is the list of.
        RelocMod::Hi if single => match cx.constant(v.expr) {
            Some(n) => w.push(word | imm(n.wrapping_add(0x8000) >> 16)),
            None => w.push_fixup(word, v.expr, imm16_fixup(v.mods), v.span),
        },
        RelocMod::Lo if single => match cx.constant(v.expr) {
            Some(n) => w.push(word | imm(n)),
            None => w.push_fixup(word, v.expr, imm16_fixup(v.mods), v.span),
        },
        RelocMod::Higher if single => match cx.constant(v.expr) {
            Some(n) => w.push(word | imm(n.wrapping_add(0x8000_8000) >> 32)),
            None => w.push_fixup(word, v.expr, imm16_fixup(v.mods), v.span),
        },
        RelocMod::Highest if single => match cx.constant(v.expr) {
            Some(n) => {
                w.push(word | imm(n.wrapping_add(0x8000_8000_8000) >> 48));
            }
            None => w.push_fixup(word, v.expr, imm16_fixup(v.mods), v.span),
        },
        // `%half` of an address relocates like the other parts of one, but
        // of a number there is nothing to follow: GNU as leaves the value
        // alone in the word where the instruction should be, and llvm-mc
        // does not read `%half` at all.
        RelocMod::Half if cx.constant(v.expr).is_some() => {
            cx.error(
                v.span,
                "`%half` of a number is refused: no reference assembles it \
                 (GNU as writes the value in place of the instruction), so write \
                 the number itself",
            );
            return None;
        }
        // Everything else names something only the linker lays out — a
        // place in the GOT, a distance from `$gp`, an offset within a
        // thread's block — so a number is refused rather than written,
        // which is GNU as's "unsupported constant in relocation". A
        // thread-local access is left to the layout pass, which has the
        // fuller message for one, and so is anything with a symbol in it.
        m if !is_thread_local(m) && cx.constant(v.expr).is_some() => {
            cx.error(
                v.span,
                format!(
                    "`{}` of a number has nothing to relocate: only the linker knows \
                     where the GOT and `_gp` are",
                    v.mods.written()
                ),
            );
            return None;
        }
        _ => w.push_fixup(word, v.expr, imm16_fixup(v.mods), v.span),
    }
    Some(())
}

/// Whether an operator names a place a thread's block, which the layout
/// pass refuses a number for with a message of its own.
fn is_thread_local(m: RelocMod) -> bool {
    matches!(
        m,
        RelocMod::TlsGd
            | RelocMod::TlsLdm
            | RelocMod::DtprelHi
            | RelocMod::DtprelLo
            | RelocMod::Gottprel
            | RelocMod::TprelHi
            | RelocMod::TprelLo
    )
}

/// Places a memory operand's displacement, which defaults to zero.
fn place_disp(cx: &mut AsmCtx<'_>, w: &mut Words, word: u32, disp: Option<Imm>) -> Option<()> {
    match disp {
        None => {
            w.push(word);
            Some(())
        }
        Some(d) => place_imm16(cx, w, word, d, "displacement"),
    }
}

// ---- operand access -------------------------------------------------------

/// The operand list of one statement, with the checks every accessor needs.
pub struct Args<'o> {
    pub mnemonic: &'o str,
    pub ops: &'o [Operand],
    pub span: Span,
}

impl Args<'_> {
    /// Requires an exact operand count.
    pub fn arity(&self, cx: &mut AsmCtx<'_>, n: usize) -> Option<()> {
        self.arity_between(cx, n, n)
    }

    pub fn arity_between(&self, cx: &mut AsmCtx<'_>, lo: usize, hi: usize) -> Option<()> {
        if self.ops.len() < lo || self.ops.len() > hi {
            let want = if lo == hi {
                format!("{lo}")
            } else {
                format!("{lo} to {hi}")
            };
            cx.error(
                self.span,
                format!(
                    "`{}` takes {want} operand(s), but {} were given",
                    self.mnemonic,
                    self.ops.len()
                ),
            );
            return None;
        }
        Some(())
    }

    fn at(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<&Operand> {
        match self.ops.get(i) {
            Some(o) => Some(o),
            None => {
                cx.error(self.span, format!("`{}`: missing operand", self.mnemonic));
                None
            }
        }
    }

    /// An integer register operand, recorded in the object's `.reginfo`;
    /// see [`super::abi`]. Use [`Args::gpr_unused`] where the encoding does
    /// not keep it.
    pub fn gpr(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Reg> {
        let r = self.gpr_unused(cx, i)?;
        super::abi::mark(cx.state, r);
        Some(r)
    }

    /// An integer register operand the caller may not encode: `bge $4, $zero`
    /// is `bgez $4`, which has no field for the second register, so the
    /// register masks must not count it.
    pub fn gpr_unused(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Reg> {
        let o = self.at(cx, i)?;
        match o.gpr() {
            Some(r) => Some(r),
            None => {
                cx.error(
                    o.span,
                    format!(
                        "`{}`: operand {} must be an integer register, found {}",
                        self.mnemonic,
                        i + 1,
                        o.describe()
                    ),
                );
                None
            }
        }
    }

    /// A floating-point register operand, recorded in `ri_cprmask[1]`.
    /// `wide` says the operand holds a 64-bit value and so names a register
    /// pair, which the caller reads from [`Def::wide_fpr`]; see
    /// [`super::abi`].
    pub fn fpr(&self, cx: &mut AsmCtx<'_>, i: usize, wide: bool) -> Option<Reg> {
        let o = self.at(cx, i)?;
        match o.fpr() {
            Some(r) => {
                super::abi::mark_fpr(cx.state, r, wide);
                Some(r)
            }
            None => {
                cx.error(
                    o.span,
                    format!(
                        "`{}`: operand {} must be a floating-point register, found {}",
                        self.mnemonic,
                        i + 1,
                        o.describe()
                    ),
                );
                None
            }
        }
    }

    /// A condition-flag operand, which no register mask counts; see
    /// [`super::abi::mark`].
    pub fn fcc(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Reg> {
        let o = self.at(cx, i)?;
        match o.fcc() {
            Some(r) => Some(r),
            None => {
                cx.error(
                    o.span,
                    format!(
                        "`{}`: operand {} must be a floating-point condition flag, \
                         $fcc0 to $fcc7, found {}",
                        self.mnemonic,
                        i + 1,
                        o.describe()
                    ),
                );
                None
            }
        }
    }

    /// The flag a form that can leave `$fcc0` implied was given, and how many
    /// operands naming it took.
    fn leading_fcc(&self, cx: &mut AsmCtx<'_>, written: bool) -> Option<(Reg, usize)> {
        if written {
            Some((self.fcc(cx, 0)?, 1))
        } else {
            Some((Reg::fcc(0), 0))
        }
    }

    pub fn imm(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Imm> {
        let o = self.at(cx, i)?;
        match o.imm() {
            Some(v) => Some(v),
            None => {
                cx.error(
                    o.span,
                    format!(
                        "`{}`: operand {} must be an immediate, found {}",
                        self.mnemonic,
                        i + 1,
                        o.describe()
                    ),
                );
                None
            }
        }
    }

    /// An `offset(base)` operand; the base register goes into `.reginfo`.
    pub fn mem(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<super::operand::Mem> {
        let o = self.at(cx, i)?;
        match o.mem() {
            Some(m) => {
                super::abi::mark(cx.state, m.base);
                Some(m)
            }
            // `lw $a0, 8` with no base register is the commonest slip here,
            // and `describe` alone would not say what is missing.
            None => {
                cx.error(
                    o.span,
                    format!(
                        "`{}`: operand {} must be `offset(base)`, found {}",
                        self.mnemonic,
                        i + 1,
                        o.describe()
                    ),
                );
                None
            }
        }
    }

    /// A branch or jump target, which is one of the fields no relocation
    /// operator fits: both references refuse `beq $4, $5, %got(x)` and
    /// `j %got(x)`, since neither a GOT slot nor half an address is a place
    /// to jump to.
    pub fn target(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Imm> {
        let v = self.imm(cx, i)?;
        self.plain(cx, v, "a branch or jump target")?;
        Some(v)
    }

    /// Requires an immediate with no relocation operator on it.
    fn plain(&self, cx: &mut AsmCtx<'_>, v: Imm, what: &str) -> Option<()> {
        if v.mods.is_empty() {
            return Some(());
        }
        cx.error(
            v.span,
            format!(
                "`{}` takes no relocation operator on {what}, so `{}` has nowhere to go",
                self.mnemonic,
                v.mods.written()
            ),
        );
        None
    }

    /// A constant in `0..=max`, for shift amounts and trap codes.
    pub fn small_const(&self, cx: &mut AsmCtx<'_>, i: usize, max: u32, what: &str) -> Option<u32> {
        let v = self.imm(cx, i)?;
        self.plain(cx, v, what)?;
        let Some(n) = cx.constant(v.expr) else {
            cx.error(v.span, format!("{what} must be a constant"));
            return None;
        };
        if n < 0 || n > max as i64 {
            cx.error(v.span, format!("{what} {n} is out of range (0 to {max})"));
            return None;
        }
        Some(n as u32)
    }
}

// ---- the encoder ----------------------------------------------------------

/// Encodes one table-driven instruction.
pub fn encode(
    cx: &mut AsmCtx<'_>,
    def: &Def,
    a: &Args<'_>,
    endian: crate::arch::Endian,
    is64: bool,
) -> Option<Variant> {
    if def.is64 && !is64 {
        cx.error(
            a.span,
            format!(
                "`{}` is a 64-bit instruction; this target is 32-bit MIPS",
                a.mnemonic
            ),
        );
        return None;
    }
    let mut w = Words::new(endian);
    let base = def.word;
    match def.form {
        Form::RdRsRt => {
            a.arity(cx, 3)?;
            let (d, s, t) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?, a.gpr(cx, 2)?);
            w.push(base | rd(d.num) | rs(s.num) | rt(t.num));
        }
        Form::RdRtSa => {
            a.arity(cx, 3)?;
            let (d, t) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?);
            let n = a.small_const(cx, 2, 31, "shift amount")?;
            w.push(base | rd(d.num) | rt(t.num) | sa(n));
        }
        Form::RdRtRs => {
            a.arity(cx, 3)?;
            let (d, t, s) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?, a.gpr(cx, 2)?);
            w.push(base | rd(d.num) | rt(t.num) | rs(s.num));
        }
        Form::RsRt => {
            a.arity(cx, 2)?;
            let (s, t) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?);
            w.push(base | rs(s.num) | rt(t.num));
        }
        Form::Div => {
            a.arity_between(cx, 2, 3)?;
            // Two operands: a plain divide. llvm-mc instead expands this
            // spelling into a divide guarded by a trap on a zero divisor. That
            // guard is a policy choice rather than an encoding, and silently
            // adding instructions is exactly what this backend avoids, so the
            // corpus compares only the three-operand form.
            let skip = if a.ops.len() == 3 {
                let z = a.gpr(cx, 0)?;
                if z != reg::ZERO {
                    let span = a.ops.first().map_or(a.span, |o| o.span);
                    cx.error(
                        span,
                        format!(
                            "`{}` leaves its result in HI and LO, so a destination \
                             operand must be $zero",
                            a.mnemonic
                        ),
                    );
                    return None;
                }
                1
            } else {
                0
            };
            let (s, t) = (a.gpr(cx, skip)?, a.gpr(cx, skip + 1)?);
            w.push(base | rs(s.num) | rt(t.num));
        }
        Form::Trap => {
            a.arity_between(cx, 2, 3)?;
            let (s, t) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?);
            let code = match a.ops.len() {
                3 => a.small_const(cx, 2, 0x3ff, "trap code")?,
                _ => 0,
            };
            w.push(base | rs(s.num) | rt(t.num) | (code << 6));
        }
        Form::Rd => {
            a.arity(cx, 1)?;
            w.push(base | rd(a.gpr(cx, 0)?.num));
        }
        Form::Rs => {
            a.arity(cx, 1)?;
            w.push(base | rs(a.gpr(cx, 0)?.num));
        }
        Form::Jalr => {
            a.arity_between(cx, 1, 2)?;
            let (d, s) = if a.ops.len() == 1 {
                (reg::RA, a.gpr(cx, 0)?)
            } else {
                (a.gpr(cx, 0)?, a.gpr(cx, 1)?)
            };
            w.push(base | rd(d.num) | rs(s.num));
        }
        Form::RtRsImm => {
            a.arity(cx, 3)?;
            let (t, s) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?);
            let v = a.imm(cx, 2)?;
            place_imm16(cx, &mut w, base | rt(t.num) | rs(s.num), v, "immediate")?;
        }
        Form::RtImm => {
            a.arity(cx, 2)?;
            let t = a.gpr(cx, 0)?;
            let v = a.imm(cx, 1)?;
            place_imm16(cx, &mut w, base | rt(t.num), v, "immediate")?;
        }
        Form::RsImm => {
            a.arity(cx, 2)?;
            let s = a.gpr(cx, 0)?;
            let v = a.imm(cx, 1)?;
            place_imm16(cx, &mut w, base | rs(s.num), v, "immediate")?;
        }
        Form::RtMem => {
            a.arity(cx, 2)?;
            let t = a.gpr(cx, 0)?;
            let m = a.mem(cx, 1)?;
            place_disp(cx, &mut w, base | rt(t.num) | rs(m.base.num), m.disp)?;
        }
        Form::FtMem => {
            a.arity(cx, 2)?;
            let t = a.fpr(cx, 0, def.wide_fpr(0))?;
            let m = a.mem(cx, 1)?;
            place_disp(cx, &mut w, base | rt(t.num) | rs(m.base.num), m.disp)?;
        }
        Form::RsRtOff => {
            a.arity(cx, 3)?;
            let (s, t) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?);
            let target = a.target(cx, 2)?;
            w.push_fixup(
                base | rs(s.num) | rt(t.num),
                target.expr,
                branch_fixup(),
                target.span,
            );
        }
        Form::RsOff => {
            a.arity(cx, 2)?;
            let s = a.gpr(cx, 0)?;
            // A linking branch writes `$ra` before anything can read the
            // register it tested, so testing `$ra` itself is unpredictable:
            // an exception that restarts the instruction would find the link
            // in place of the value. GNU as refuses the form and llvm-mc
            // assembles it; rsasm follows GNU as.
            if s == reg::RA && def.links() {
                let span = a.ops.first().map_or(a.span, |o| o.span);
                cx.error(
                    span,
                    format!(
                        "`{}` links through $ra, so the register it tests may not be $ra",
                        a.mnemonic
                    ),
                );
                return None;
            }
            let target = a.target(cx, 1)?;
            w.push_fixup(base | rs(s.num), target.expr, branch_fixup(), target.span);
        }
        Form::CcOff => {
            a.arity_between(cx, 1, 2)?;
            let (flag, skip) = a.leading_fcc(cx, a.ops.len() == 2)?;
            let target = a.target(cx, skip)?;
            w.push_fixup(
                base | cc(flag.num),
                target.expr,
                branch_fixup(),
                target.span,
            );
        }
        Form::Off26 => {
            a.arity(cx, 1)?;
            let target = a.target(cx, 0)?;
            w.push_fixup(base, target.expr, jump_fixup(), target.span);
        }
        Form::Nullary => {
            a.arity(cx, 0)?;
            // `nop` is `sll $zero, $zero, 0`, so it names a register where
            // `ssnop` and `eret` are opcodes of their own and name none.
            if base == 0 {
                super::abi::mark_zero(cx.state);
            }
            w.push(base);
        }
        Form::Break => {
            a.arity_between(cx, 0, 2)?;
            // `break` splits its 20 bits into two fields, and a single operand
            // fills only the upper one; that is what debuggers look at.
            let code1 = match a.ops.len() {
                0 => 0,
                _ => a.small_const(cx, 0, 0x3ff, "break code")?,
            };
            let code2 = match a.ops.len() {
                2 => a.small_const(cx, 1, 0x3ff, "break code")?,
                _ => 0,
            };
            w.push(base | (code1 << 16) | (code2 << 6));
        }
        Form::Code20 => {
            a.arity_between(cx, 0, 1)?;
            let code = match a.ops.len() {
                0 => 0,
                _ => a.small_const(cx, 0, 0xf_ffff, "code")?,
            };
            w.push(base | (code << 6));
        }
        Form::Sync => {
            a.arity_between(cx, 0, 1)?;
            let stype = match a.ops.len() {
                0 => 0,
                _ => a.small_const(cx, 0, 31, "sync type")?,
            };
            w.push(base | sa(stype));
        }
        Form::FdFsFt => {
            a.arity(cx, 3)?;
            let (fd, fs, ft) = (
                a.fpr(cx, 0, def.wide_fpr(0))?,
                a.fpr(cx, 1, def.wide_fpr(1))?,
                a.fpr(cx, 2, def.wide_fpr(2))?,
            );
            w.push(base | sa(fd.num as u32) | rd(fs.num) | rt(ft.num));
        }
        Form::FdFs => {
            a.arity(cx, 2)?;
            let (fd, fs) = (
                a.fpr(cx, 0, def.wide_fpr(0))?,
                a.fpr(cx, 1, def.wide_fpr(1))?,
            );
            w.push(base | sa(fd.num as u32) | rd(fs.num));
        }
        Form::CcFsFt => {
            a.arity_between(cx, 2, 3)?;
            let (flag, skip) = a.leading_fcc(cx, a.ops.len() == 3)?;
            let (fs, ft) = (
                a.fpr(cx, skip, def.wide_fpr(0))?,
                a.fpr(cx, skip + 1, def.wide_fpr(1))?,
            );
            w.push(base | cc_lo(flag.num) | rd(fs.num) | rt(ft.num));
        }
        Form::RtFs => {
            a.arity(cx, 2)?;
            let t = a.gpr(cx, 0)?;
            let fs = a.fpr(cx, 1, def.wide_fpr(1))?;
            w.push(base | rt(t.num) | rd(fs.num));
        }
        Form::RdRsCc => {
            a.arity(cx, 3)?;
            let (d, s) = (a.gpr(cx, 0)?, a.gpr(cx, 1)?);
            let flag = a.fcc(cx, 2)?;
            w.push(base | rd(d.num) | rs(s.num) | cc(flag.num));
        }
        Form::FdFsCc => {
            a.arity(cx, 3)?;
            let (fd, fs) = (
                a.fpr(cx, 0, def.wide_fpr(0))?,
                a.fpr(cx, 1, def.wide_fpr(1))?,
            );
            let flag = a.fcc(cx, 2)?;
            w.push(base | sa(fd.num as u32) | rd(fs.num) | cc(flag.num));
        }
        Form::RtRdSel => {
            a.arity_between(cx, 2, 3)?;
            let t = a.gpr(cx, 0)?;
            let d = a.gpr(cx, 1)?;
            let sel = match a.ops.len() {
                3 => a.small_const(cx, 2, 7, "coprocessor register select")?,
                _ => 0,
            };
            w.push(base | rt(t.num) | rd(d.num) | sel);
        }
        Form::RtHwr => {
            a.arity(cx, 2)?;
            let t = a.gpr(cx, 0)?;
            // The second operand numbers a hardware register, not one of the
            // integer file, so `.reginfo` does not count it: llvm-mc's mask
            // for `rdhwr $3, $29` names $3 alone.
            let h = a.gpr_unused(cx, 1)?;
            w.push(base | rt(t.num) | rd(h.num));
        }
    }
    Some(w.finish())
}
