//! Building SPARC instruction words.
//!
//! # The three formats
//!
//! Every SPARC instruction is one big-endian 32-bit word, and bits 31-30
//! (`op`) say which of three layouts the rest of it uses.
//!
//! ```text
//!  31 30 29         25 24     22 21                                    0
//! +-----+-------------+---------+---------------------------------------+
//! |  1  |                           disp30                              |  format 1
//! +-----+---------------------------------------------------------------+
//! |  0  | rd or a+cond|   op2   |           imm22 or disp22             |  format 2
//! +-----+-------------+---------+---------------------------------------+
//!  31 30 29         25 24       19 18      14 13 12                    0
//! +-----+-------------+-----------+----------+--+-----------------------+
//! | 2,3 |     rd      |    op3    |   rs1    |i | simm13, or rs2 in 4-0 |  format 3
//! +-----+-------------+-----------+----------+--+-----------------------+
//! ```
//!
//! Format 1 is `call` and nothing else: a 30-bit word offset, which reaches
//! anywhere in a 32-bit address space. Format 2 is `sethi` (with `op2` = 4,
//! `rd`, and 22 bits of constant) and the branches (with `op2` = 2, the annul
//! bit `a` and a four-bit condition packed into the `rd` field, and 22 bits
//! of word offset). Format 3 is everything else: the `i` bit chooses between
//! a second source register and a 13-bit signed immediate, which is the one
//! constraint that shapes most SPARC code — anything bigger has to be built
//! with `sethi`.

use super::insn::{Form, StateOp};
use super::operand::{Addr, Imm, ImmPart, Offset, Operand, OperandKind, TlsOp};
use super::reg::{self, FpWidth, Reg, RegClass};
use super::reloc;
use crate::arch::AsmCtx;
use crate::expr::ExprRef;
use crate::reloc::RelocClass;
use crate::section::{Fixup, FixupKind, LinkValue, RelocSymbol, Variant};
use crate::source::Span;

/// `op` field values, i.e. which format the word uses.
pub const OP_FORMAT2: u32 = 0;
pub const OP_CALL: u32 = 1;
pub const OP_ALU: u32 = 2;
pub const OP_MEM: u32 = 3;

/// `op2` values within format 2.
pub const OP2_BPCC: u32 = 1;
pub const OP2_BICC: u32 = 2;
pub const OP2_BPR: u32 = 3;
pub const OP2_SETHI: u32 = 4;
pub const OP2_FBPFCC: u32 = 5;
pub const OP2_FBFCC: u32 = 6;

/// `op3` of the two floating-point operation spaces, which hold everything
/// the `%f` registers do: `FPop1` is the arithmetic and `FPop2` the compares
/// and the conditional moves.
pub const OP3_FPOP1: u32 = 0x34;
pub const OP3_FPOP2: u32 = 0x35;

/// Bit 13: 1 selects the 13-bit signed immediate, 0 the `rs2` register.
pub const I_BIT: u32 = 1 << 13;

/// Bit 12 of a V9 shift: the count is 64-bit (`sllx`) rather than 32-bit.
pub const X_BIT: u32 = 1 << 12;

/// The inclusive range of the `simm13` field, which is where nearly every
/// SPARC size limit comes from.
pub const SIMM13: (i64, i64) = (-4096, 4095);

/// A fixup that has not been given its position in the fragment yet.
pub struct Pending {
    pub expr: ExprRef,
    pub kind: FixupKind,
    pub span: Span,
}

/// One assembled instruction word, plus the fixup it still needs.
pub struct Word {
    pub word: u32,
    pub fixup: Option<Pending>,
}

impl Word {
    pub fn plain(word: u32) -> Word {
        Word { word, fixup: None }
    }

    pub fn fixed(word: u32, fixup: Pending) -> Word {
        Word {
            word,
            fixup: Some(fixup),
        }
    }
}

/// Turns assembled words into the single [`Variant`] a fixed-width
/// architecture always produces. Multi-word synthetics such as `set` are one
/// variant of several words, not several variants.
pub fn variant(words: Vec<Word>) -> Variant {
    let mut bytes = Vec::with_capacity(words.len() * 4);
    let mut fixups = Vec::new();
    for (i, w) in words.into_iter().enumerate() {
        bytes.extend_from_slice(&w.word.to_be_bytes());
        if let Some(p) = w.fixup {
            fixups.push(Fixup {
                offset: (i * 4) as u32,
                expr: p.expr,
                kind: p.kind,
                span: p.span,
            });
        }
    }
    Variant { bytes, fixups }
}

pub fn one(word: Word) -> Vec<Variant> {
    vec![variant(vec![word])]
}

// ---- field assembly -------------------------------------------------------

pub fn format1(disp30: u32) -> u32 {
    (OP_CALL << 30) | (disp30 & 0x3fff_ffff)
}

pub fn format2(rd: u32, op2: u32, imm: u32) -> u32 {
    (OP_FORMAT2 << 30) | ((rd & 0x1f) << 25) | ((op2 & 7) << 22) | (imm & 0x3f_ffff)
}

/// `low` carries the `i` bit together with whatever it selects.
pub fn format3(op: u32, rd: u32, op3: u32, rs1: u32, low: u32) -> u32 {
    (op << 30) | ((rd & 0x1f) << 25) | ((op3 & 0x3f) << 19) | ((rs1 & 0x1f) << 14) | low
}

// ---- scatter functions ----------------------------------------------------
//
// Each takes the word already emitted and the resolved value, and returns the
// word with the value merged into its field. They are the only place that
// knows where a displacement lives, and they must leave every other bit of
// the word alone.

/// Format 1: 30 bits of word offset.
fn call30(word: u64, v: i64) -> u64 {
    (word & 0xc000_0000) | (((v >> 2) as u64) & 0x3fff_ffff)
}

/// Format 2 `Bicc`: 22 bits of word offset.
fn disp22(word: u64, v: i64) -> u64 {
    (word & !0x3f_ffff) | (((v >> 2) as u64) & 0x3f_ffff)
}

/// Format 2 `BPcc`: 19 bits of word offset.
fn disp19(word: u64, v: i64) -> u64 {
    (word & !0x7_ffff) | (((v >> 2) as u64) & 0x7_ffff)
}

/// Format 2 `BPr`: 16 bits of word offset, split so that the low 14 stay in
/// the immediate field and the high 2 sit above `rs1`.
fn disp16(word: u64, v: i64) -> u64 {
    let d = ((v >> 2) as u64) & 0xffff;
    (word & !0x0030_3fff) | ((d >> 14) << 20) | (d & 0x3fff)
}

/// `sethi %hi(v)`: bits 31-10 of the value.
fn hi22(word: u64, v: i64) -> u64 {
    (word & !0x3f_ffff) | (((v as u64) >> 10) & 0x3f_ffff)
}

/// `sethi v`: the 22-bit field taken literally.
fn imm22(word: u64, v: i64) -> u64 {
    (word & !0x3f_ffff) | ((v as u64) & 0x3f_ffff)
}

/// `%lo(v)`: bits 9-0 of the value, dropped into a `simm13` field.
fn lo10(word: u64, v: i64) -> u64 {
    (word & !0x3ff) | ((v as u64) & 0x3ff)
}

/// A whole 13-bit signed immediate.
fn simm13(word: u64, v: i64) -> u64 {
    (word & !0x1fff) | ((v as u64) & 0x1fff)
}

// ---- fixup kinds ----------------------------------------------------------

/// `call`: PC-relative, counted in instructions, so 30 encoded bits carry 32
/// bits of byte displacement.
pub fn call30_fixup() -> FixupKind {
    FixupKind::pcrel(4, 0)
        .with_field(32, 4)
        .with_reloc(reloc::WDISP30)
        .scatter(call30)
}

pub fn disp22_fixup() -> FixupKind {
    FixupKind::pcrel(4, 0)
        .with_field(24, 4)
        .with_reloc(reloc::WDISP22)
        .scatter(disp22)
}

pub fn disp19_fixup() -> FixupKind {
    FixupKind::pcrel(4, 0)
        .with_field(21, 4)
        .with_reloc(reloc::WDISP19)
        .scatter(disp19)
}

pub fn disp16_fixup() -> FixupKind {
    FixupKind::pcrel(4, 0)
        .with_field(18, 4)
        .with_reloc(reloc::WDISP16)
        .scatter(disp16)
}

pub fn hi22_fixup() -> FixupKind {
    FixupKind::data(4).with_reloc(reloc::HI22).scatter(hi22)
}

pub fn imm22_fixup() -> FixupKind {
    FixupKind::data(4)
        .with_field(22, 1)
        .with_reloc(reloc::ABS22)
        .scatter(imm22)
}

pub fn lo10_fixup() -> FixupKind {
    FixupKind::data(4).with_reloc(reloc::LO10).scatter(lo10)
}

pub fn simm13_fixup() -> FixupKind {
    FixupKind::data(4)
        .signed()
        .with_field(13, 1)
        .with_reloc(reloc::ABS13)
        .scatter(simm13)
}

/// The field a thread-local operator names.
///
/// Nothing here computes one. The value is a variable's offset within a
/// thread's block, or into the GOT entry holding one, which only the linker
/// lays out; and the linker may rewrite the whole sequence into another
/// model, so even a variable defined in this file is relocated. Both
/// references leave the field exactly as the instruction was written, which
/// is what [`keep_word`] does should a flat image ever get this far.
pub fn tls_fixup(reloc: u32) -> FixupKind {
    FixupKind::data(4)
        .with_reloc(reloc)
        .with_class(RelocClass::ThreadLocal)
        .link(LinkValue::LinkerOnly(TLS_BLOCK))
        .linker_only()
        .scatter(keep_word)
}

/// A thread-local operator written after the last operand: a relocation
/// covering no bytes, which says what the instruction as a whole is for. The
/// symbol is named even where it is a local label, since the linker looks the
/// variable up by it.
pub fn tls_mark_fixup(reloc: u32) -> FixupKind {
    FixupKind::data(0)
        .with_reloc(reloc)
        .with_reloc_symbol(RelocSymbol::Symbol)
        .with_class(RelocClass::ThreadLocal)
        .link(LinkValue::LinkerOnly(TLS_BLOCK))
        .linker_only()
}

/// What a thread-local operator refers to, for the flat-binary refusal.
const TLS_BLOCK: &str = "a variable's place in a thread's block";

fn keep_word(w: u64, _: i64) -> u64 {
    w
}

// ---- operand slots --------------------------------------------------------

/// The `reg_or_imm` slot every format 3 instruction ends with.
///
/// Returns the low 14 bits of the word (the `i` bit and what it selects) and,
/// when the immediate is not a number yet, the fixup that will fill it in.
pub fn source(cx: &mut AsmCtx<'_>, op: &Operand) -> Option<(u32, Option<Pending>)> {
    if let Some(r) = op.int_reg() {
        return Some((u32::from(r.num), None));
    }
    let Some(imm) = op.imm() else {
        cx.error(
            op.span,
            format!(
                "expected an integer register or an immediate, found {}",
                op.describe()
            ),
        );
        return None;
    };
    immediate(cx, &imm)
}

/// Said where a thread-local mark was written as an operand. GNU as reads it
/// only after the last one, since it stands for the instruction and not for
/// any field of it.
fn mark_out_of_place(cx: &mut AsmCtx<'_>, span: Span, op: TlsOp) {
    cx.error(
        span,
        format!(
            "`%{}()` marks the whole instruction; write it after the last operand",
            op.name
        ),
    );
}

/// The same slot, given an already-extracted immediate.
fn immediate(cx: &mut AsmCtx<'_>, imm: &Imm) -> Option<(u32, Option<Pending>)> {
    match imm.part {
        // `%lo()` always fits: it is ten bits of a value the linker knows.
        ImmPart::Lo => Some((
            I_BIT,
            Some(Pending {
                expr: imm.expr,
                kind: lo10_fixup(),
                span: imm.span,
            }),
        )),
        ImmPart::Hi => {
            cx.error(
                imm.span,
                "`%hi()` produces 22 bits and only fits in `sethi`; use `%lo()` here",
            );
            None
        }
        // Both references take any of the ten thread-local field operators
        // in any field, high or low: the relocation names a step of a model
        // rather than a part of a value, and the linker writes whatever
        // field it finds. The marks are the exception; they go after the
        // last operand, not in place of one.
        ImmPart::Tls(op) if !op.mark => Some((
            I_BIT,
            Some(Pending {
                expr: imm.expr,
                kind: tls_fixup(op.reloc),
                span: imm.span,
            }),
        )),
        ImmPart::Tls(op) => {
            mark_out_of_place(cx, imm.span, op);
            None
        }
        ImmPart::Whole => match cx.constant(imm.expr) {
            Some(v) => {
                if v < SIMM13.0 || v > SIMM13.1 {
                    cx.error(
                        imm.span,
                        format!(
                            "immediate {v} does not fit the 13-bit signed field ({}..{}); \
                             build it with `sethi`/`or` or use `set`",
                            SIMM13.0, SIMM13.1
                        ),
                    );
                    return None;
                }
                Some((I_BIT | (v as u32 & 0x1fff), None))
            }
            None => Some((
                I_BIT,
                Some(Pending {
                    expr: imm.expr,
                    kind: simm13_fixup(),
                    span: imm.span,
                }),
            )),
        },
    }
}

/// An address: the `rs1` field plus the same low 14 bits as [`source`].
pub fn address(cx: &mut AsmCtx<'_>, a: &Addr) -> Option<(u32, u32, Option<Pending>)> {
    let rs1 = u32::from(a.base.num);
    match a.offset {
        // `[%g1]` encodes as `%g1 + %g0` with the `i` bit clear. An explicit
        // `[%g1 + 0]` sets it instead; the two words differ, and both GNU as
        // and llvm-mc make the same distinction.
        Offset::None => Some((rs1, 0, None)),
        Offset::Reg(r) => Some((rs1, u32::from(r.num), None)),
        Offset::Imm(imm) => {
            let (low, fixup) = immediate(cx, &imm)?;
            Some((rs1, low, fixup))
        }
    }
}

/// A register operand that must be an integer register.
pub fn int_reg(cx: &mut AsmCtx<'_>, op: &Operand, what: &str) -> Option<Reg> {
    match op.int_reg() {
        Some(r) => Some(r),
        None => {
            cx.error(
                op.span,
                format!(
                    "expected an integer register as the {what}, found {}",
                    op.describe()
                ),
            );
            None
        }
    }
}

/// A register operand that must be a float register of `width`, returned as
/// the five-bit field value it encodes to.
///
/// The width is what makes `%f32` legal or not: only a double or quad
/// operand can reach the upper half of V9's register file, and only a
/// register that starts a pair or a quad can name one.
pub fn float_field(cx: &mut AsmCtx<'_>, op: &Operand, width: FpWidth, what: &str) -> Option<u32> {
    let Some(r) = op.float_reg() else {
        cx.error(
            op.span,
            format!(
                "expected a float register as the {what}, found {}",
                op.describe()
            ),
        );
        return None;
    };
    // The upper half of the register file arrived with V9; a V8 target has
    // only 32 of them however wide the operand is.
    if r.num >= 32 && cx.state.bits < 64 {
        cx.error(
            op.span,
            format!(
                "`{}` is a SPARC V9 register; this target has only `%f0`-`%f31`",
                reg::name_of(r)
            ),
        );
        return None;
    }
    if !width.allows(r.num) {
        let limit = match width {
            FpWidth::Single => "`%f0`-`%f31`",
            FpWidth::Double => "an even `%f0`-`%f62`",
            FpWidth::Quad => "a multiple of four from `%f0` to `%f60`",
        };
        cx.error(
            op.span,
            format!(
                "`{}` cannot name a {}-precision operand; the {what} must be {limit}",
                reg::name_of(r),
                width.name(),
            ),
        );
        return None;
    }
    Some(reg::fp_field(width, r.num))
}

/// The `rd` and `op3` of a `%fsr` or `%fq` transfer.
///
/// These are opcodes of their own rather than a register in a field: `ld`
/// and `st` reach `%fsr` at `op3` 0x21 and 0x25, `ldx` and `stx` reach all 64
/// bits of it at the same pair with `rd` = 1, and `std %fq` reads the
/// exception queue at 0x26. Anything else -- `ldd [x], %fsr`, `ldx [x], %fq`
/// -- is a combination no opcode exists for, and GNU as refuses it too.
fn state_reg(
    cx: &mut AsmCtx<'_>,
    m: &str,
    op: &Operand,
    r: Reg,
    state: Option<StateOp>,
    store: bool,
) -> Option<(u32, u32)> {
    let fsr = r.num == 0;
    match state {
        Some(StateOp::Fsr(rd)) if fsr => Some((u32::from(rd), if store { 0x25 } else { 0x21 })),
        Some(StateOp::Fq) if !fsr => Some((0, 0x26)),
        _ => {
            cx.error(
                op.span,
                format!("`{m}` cannot transfer `{}`", reg::name_of(r)),
            );
            None
        }
    }
}

/// A constant that must fit an `n`-bit signed field, for the narrow immediate
/// slots that V9's conditional moves carve out of format 3.
pub fn small_signed(cx: &mut AsmCtx<'_>, imm: &Imm, bits: u32, what: &str) -> Option<i64> {
    if imm.part != ImmPart::Whole {
        cx.error(
            imm.span,
            format!("{} cannot be used as {what}", imm.part.spelling()),
        );
        return None;
    }
    let Some(v) = cx.constant(imm.expr) else {
        cx.error(
            imm.span,
            format!("{what} must be a constant; there is no relocation for this field"),
        );
        return None;
    };
    let lo = -(1i64 << (bits - 1));
    let hi = (1i64 << (bits - 1)) - 1;
    if v < lo || v > hi {
        cx.error(
            imm.span,
            format!("{what} {v} does not fit the {bits}-bit signed field ({lo}..{hi})"),
        );
        return None;
    }
    Some(v)
}

/// A shift count: five bits for a 32-bit shift, six for a V9 64-bit one.
pub fn shift_count(cx: &mut AsmCtx<'_>, imm: &Imm, x: bool) -> Option<u32> {
    let max = if x { 63 } else { 31 };
    if imm.part != ImmPart::Whole {
        cx.error(
            imm.span,
            format!("a shift count cannot use {}", imm.part.spelling()),
        );
        return None;
    }
    let Some(v) = cx.constant(imm.expr) else {
        cx.error(imm.span, "a shift count must be a constant");
        return None;
    };
    if !(0..=max).contains(&v) {
        cx.error(
            imm.span,
            format!("shift count {v} is out of range (0..{max})"),
        );
        return None;
    }
    Some(v as u32)
}

/// `sethi`'s 22-bit field, from `%hi(x)`, a thread-local operator, or a plain
/// expression.
pub fn sethi_field(cx: &mut AsmCtx<'_>, imm: &Imm) -> Option<Pending> {
    let kind = match imm.part {
        ImmPart::Hi => hi22_fixup(),
        ImmPart::Tls(op) if op.mark => {
            mark_out_of_place(cx, imm.span, op);
            return None;
        }
        ImmPart::Tls(op) => tls_fixup(op.reloc),
        _ => imm22_fixup(),
    };
    Some(Pending {
        expr: imm.expr,
        kind,
        span: imm.span,
    })
}

// ---- one instruction ------------------------------------------------------

/// Checks the operand count, reporting which counts the mnemonic accepts.
fn arity(cx: &mut AsmCtx<'_>, m: &str, span: Span, ops: &[Operand], want: &[usize]) -> bool {
    if want.contains(&ops.len()) {
        return true;
    }
    let want: Vec<String> = want.iter().map(|n| n.to_string()).collect();
    cx.error(
        span,
        format!(
            "`{m}` takes {} operand(s), but {} were given",
            want.join(" or "),
            ops.len()
        ),
    );
    false
}

/// The address operand of `jmpl`, `jmp`, `call`, `flush` and `return`, which
/// SPARC writes without brackets.
pub fn target(cx: &mut AsmCtx<'_>, op: &Operand) -> Option<Addr> {
    if let Some(imm) = op.imm() {
        return Some(Addr {
            base: reg::G0,
            offset: Offset::Imm(imm),
        });
    }
    match op.as_addr() {
        Some(a) => Some(a),
        None => {
            cx.error(
                op.span,
                format!(
                    "expected an address such as `%o7 + 8`, found {}",
                    op.describe()
                ),
            );
            None
        }
    }
}

/// The `cc2` and `cc1cc0` fields, from a `%icc` / `%xcc` / `%fccN` operand.
fn cc_fields(cx: &mut AsmCtx<'_>, op: &Operand) -> Option<(u32, u32)> {
    match op.reg() {
        Some(r) if r.class == RegClass::Icc => Some((1, u32::from(r.num))),
        Some(r) if r.class == RegClass::Fcc => Some((0, u32::from(r.num))),
        _ => {
            cx.error(
                op.span,
                format!(
                    "expected a condition-code register (`%icc`, `%xcc`), found {}",
                    op.describe()
                ),
            );
            None
        }
    }
}

/// The bank number of a `%fccN` operand, for the instructions that take no
/// other kind.
fn fcc_field(cx: &mut AsmCtx<'_>, op: &Operand) -> Option<u32> {
    match op.reg() {
        Some(r) if r.class == RegClass::Fcc => Some(u32::from(r.num)),
        _ => {
            cx.error(
                op.span,
                format!(
                    "expected a floating-point condition-code register (`%fcc0`-`%fcc3`), found {}",
                    op.describe()
                ),
            );
            None
        }
    }
}

/// The two-bit bank a V9 `Tcc` names. Only the integer pair can be named:
/// there is no trap on a floating-point comparison.
fn trap_cc(cx: &mut AsmCtx<'_>, op: &Operand) -> Option<u32> {
    if cx.state.bits < 64 {
        cx.error(
            op.span,
            "a trap that names a condition-code register is a SPARC V9 instruction; \
             this target is V8",
        );
        return None;
    }
    let (cc2, cc10) = cc_fields(cx, op)?;
    if cc2 == 0 {
        cx.error(
            op.span,
            "a trap tests the integer condition codes; name `%icc` or `%xcc`",
        );
        return None;
    }
    Some(cc10)
}

/// Why a conditional move's name does not go with the bank it was given.
fn not_a_condition(m: &str, cc2: u32) -> String {
    format!(
        "`{m}` is not a condition of {}",
        if cc2 == 1 { "`%icc`" } else { "a `%fcc`" }
    )
}

/// `opf_low` for the floating-point move of a given width. The
/// register-tested form is the same three values four higher.
fn fp_move_opf(width: FpWidth) -> u32 {
    match width {
        FpWidth::Single => 1,
        FpWidth::Double => 2,
        FpWidth::Quad => 3,
    }
}

/// Assembles everything except the branches, which need their `,a` suffix
/// stripped before the operand list can be split on commas.
pub fn encode(
    cx: &mut AsmCtx<'_>,
    m: &str,
    span: Span,
    form: Form,
    ops: &[Operand],
) -> Option<Vec<Variant>> {
    match form {
        Form::Alu(op3) => {
            if !arity(cx, m, span, ops, &[3]) {
                return None;
            }
            let rs1 = int_reg(cx, &ops[0], "first source")?;
            let (low, fixup) = source(cx, &ops[1])?;
            let rd = int_reg(cx, &ops[2], "destination")?;
            let word = format3(
                OP_ALU,
                u32::from(rd.num),
                u32::from(op3),
                u32::from(rs1.num),
                low,
            );
            Some(one(pack(word, fixup)))
        }

        Form::Shift { op3, x } => {
            if !arity(cx, m, span, ops, &[3]) {
                return None;
            }
            let rs1 = int_reg(cx, &ops[0], "source")?;
            let xb = if x { X_BIT } else { 0 };
            let low = if let Some(r) = ops[1].int_reg() {
                xb | u32::from(r.num)
            } else if let Some(imm) = ops[1].imm() {
                I_BIT | xb | shift_count(cx, &imm, x)?
            } else {
                cx.error(ops[1].span, "expected a shift count or a register");
                return None;
            };
            let rd = int_reg(cx, &ops[2], "destination")?;
            let word = format3(
                OP_ALU,
                u32::from(rd.num),
                u32::from(op3),
                u32::from(rs1.num),
                low,
            );
            Some(one(Word::plain(word)))
        }

        Form::Mem(f) => {
            if !arity(cx, m, span, ops, &[2]) {
                return None;
            }
            let (data, mem) = if f.store {
                (&ops[0], &ops[1])
            } else {
                (&ops[1], &ops[0])
            };
            // `ld`/`st`/`ldd`/`std` choose their opcode from the register
            // class: the float forms are separate instructions, and `%fsr`
            // and `%fq` are two more.
            let (rd, op3) = match (data.reg(), f.fop3) {
                (Some(r), Some(fop3)) if r.is_float() => (
                    float_field(cx, data, f.fwidth, "data register")?,
                    u32::from(fop3),
                ),
                (Some(r), _) if r.is_int() && !f.float_only => (u32::from(r.num), u32::from(f.op3)),
                (Some(r), _) if r.class == RegClass::FpState => {
                    state_reg(cx, m, data, r, f.state, f.store)?
                }
                _ => {
                    let want = if f.float_only {
                        "a float register"
                    } else if f.fop3.is_some() {
                        "an integer or float register"
                    } else {
                        "an integer register"
                    };
                    cx.error(
                        data.span,
                        format!("`{m}` needs {want} here, found {}", data.describe()),
                    );
                    return None;
                }
            };
            // Unlike `jmpl` and `flush`, a load or store always brackets
            // its address, so a bare register is a mistake rather than the
            // `[reg]` shorthand.
            let OperandKind::Mem(addr) = mem.kind else {
                cx.error(
                    mem.span,
                    format!(
                        "expected an address in brackets, such as `[%o0 + 4]`, found {}",
                        mem.describe()
                    ),
                );
                return None;
            };
            let (rs1, low, fixup) = address(cx, &addr)?;
            let word = format3(OP_MEM, rd, op3, rs1, low);
            Some(one(pack(word, fixup)))
        }

        Form::Call => {
            if !arity(cx, m, span, ops, &[1]) {
                return None;
            }
            // `call %o7` and `call %g1 + %g2` are the indirect form, which is
            // `jmpl` leaving the return address in `%o7`.
            if let Some(addr) = ops[0].as_addr() {
                let (rs1, low, fixup) = address(cx, &addr)?;
                let word = format3(OP_ALU, u32::from(reg::O7.num), 0x38, rs1, low);
                return Some(one(pack(word, fixup)));
            }
            let Some(imm) = ops[0].imm() else {
                cx.error(ops[0].span, "expected a call target");
                return None;
            };
            if imm.part != ImmPart::Whole {
                cx.error(
                    imm.span,
                    format!("a call target cannot use {}", imm.part.spelling()),
                );
                return None;
            }
            Some(one(Word::fixed(
                format1(0),
                Pending {
                    expr: imm.expr,
                    kind: call30_fixup(),
                    span: imm.span,
                },
            )))
        }

        Form::Sethi => {
            if !arity(cx, m, span, ops, &[2]) {
                return None;
            }
            let Some(imm) = ops[0].imm() else {
                cx.error(ops[0].span, "`sethi` takes a 22-bit constant or `%hi(x)`");
                return None;
            };
            if imm.part == ImmPart::Lo {
                cx.error(
                    imm.span,
                    "`sethi` writes the high 22 bits; `%lo()` is for `or`",
                );
                return None;
            }
            let rd = int_reg(cx, &ops[1], "destination")?;
            let word = format2(u32::from(rd.num), OP2_SETHI, 0);
            let field = sethi_field(cx, &imm)?;
            Some(one(Word::fixed(word, field)))
        }

        Form::Jmpl => {
            if !arity(cx, m, span, ops, &[2]) {
                return None;
            }
            let addr = target(cx, &ops[0])?;
            let (rs1, low, fixup) = address(cx, &addr)?;
            let rd = int_reg(cx, &ops[1], "link register")?;
            let word = format3(OP_ALU, u32::from(rd.num), 0x38, rs1, low);
            Some(one(pack(word, fixup)))
        }

        Form::Window(op3) => {
            // `save` and `restore` may be written bare, which means
            // `save %g0, %g0, %g0`: rotate the window and add nothing.
            if ops.is_empty() {
                return Some(one(Word::plain(format3(OP_ALU, 0, u32::from(op3), 0, 0))));
            }
            if !arity(cx, m, span, ops, &[0, 3]) {
                return None;
            }
            let rs1 = int_reg(cx, &ops[0], "first source")?;
            let (low, fixup) = source(cx, &ops[1])?;
            let rd = int_reg(cx, &ops[2], "destination")?;
            let word = format3(
                OP_ALU,
                u32::from(rd.num),
                u32::from(op3),
                u32::from(rs1.num),
                low,
            );
            Some(one(pack(word, fixup)))
        }

        Form::Return => {
            if !arity(cx, m, span, ops, &[1]) {
                return None;
            }
            let addr = target(cx, &ops[0])?;
            let (rs1, low, fixup) = address(cx, &addr)?;
            Some(one(pack(format3(OP_ALU, 0, 0x39, rs1, low), fixup)))
        }

        Form::Flush => {
            if !arity(cx, m, span, ops, &[1]) {
                return None;
            }
            let addr = target(cx, &ops[0])?;
            let (rs1, low, fixup) = address(cx, &addr)?;
            Some(one(pack(format3(OP_ALU, 0, 0x3b, rs1, low), fixup)))
        }

        Form::Unimp => {
            if !arity(cx, m, span, ops, &[1]) {
                return None;
            }
            let Some(imm) = ops[0].imm() else {
                cx.error(ops[0].span, "expected a 22-bit constant");
                return None;
            };
            Some(one(Word::fixed(
                format2(0, 0, 0),
                Pending {
                    expr: imm.expr,
                    kind: imm22_fixup(),
                    span: imm.span,
                },
            )))
        }

        Form::Trap(cond) => {
            // V9 made `Tcc` name the bank it tests, in a two-bit field
            // carved out of the immediate. `%icc` is zero there, so the V8
            // spelling without a bank is the same word.
            let (cc, ops) = if ops.first().is_some_and(Operand::is_cc) {
                (trap_cc(cx, &ops[0])? << 11, &ops[1..])
            } else {
                (0, ops)
            };
            if !arity(cx, m, span, ops, &[1, 2]) {
                return None;
            }
            // GNU's disassembler prints the two-operand form as one
            // address, `t<cc> %g1 + %g2`, which is the same instruction.
            if ops.len() == 1
                && ops[0].imm().is_none()
                && let Some(addr) = ops[0].as_addr()
                && !matches!(addr.offset, Offset::None)
            {
                let (rs1, low, fixup) = address(cx, &addr)?;
                let word = format3(OP_ALU, u32::from(cond), 0x3a, rs1, cc | low);
                return Some(one(pack(word, fixup)));
            }
            let (rs1, src) = if ops.len() == 2 {
                (int_reg(cx, &ops[0], "source")?.num, &ops[1])
            } else {
                (reg::G0.num, &ops[0])
            };
            let (low, fixup) = source(cx, src)?;
            let word = format3(OP_ALU, u32::from(cond), 0x3a, u32::from(rs1), cc | low);
            Some(one(pack(word, fixup)))
        }

        Form::ReadAsr => {
            if !arity(cx, m, span, ops, &[2]) {
                return None;
            }
            let Some(asr) = ops[0].reg().filter(|r| r.class == RegClass::Asr) else {
                cx.error(ops[0].span, "`rd` reads a state register such as `%y`");
                return None;
            };
            let rd = int_reg(cx, &ops[1], "destination")?;
            Some(one(Word::plain(format3(
                OP_ALU,
                u32::from(rd.num),
                0x28,
                u32::from(asr.num),
                0,
            ))))
        }

        Form::WriteAsr => {
            if !arity(cx, m, span, ops, &[2, 3]) {
                return None;
            }
            let last = ops.len() - 1;
            let Some(asr) = ops[last].reg().filter(|r| r.class == RegClass::Asr) else {
                cx.error(ops[last].span, "`wr` writes a state register such as `%y`");
                return None;
            };
            // The two-operand form writes `%g0 ^ src`, i.e. just `src`.
            let (rs1, src) = if ops.len() == 3 {
                (int_reg(cx, &ops[0], "first source")?.num, &ops[1])
            } else {
                (reg::G0.num, &ops[0])
            };
            let (low, fixup) = source(cx, src)?;
            let word = format3(OP_ALU, u32::from(asr.num), 0x30, u32::from(rs1), low);
            Some(one(pack(word, fixup)))
        }

        Form::FpBin { opf, src, dst } => {
            if !arity(cx, m, span, ops, &[3]) {
                return None;
            }
            let rs1 = float_field(cx, &ops[0], src, "first source")?;
            let rs2 = float_field(cx, &ops[1], src, "second source")?;
            let rd = float_field(cx, &ops[2], dst, "destination")?;
            Some(one(Word::plain(format3(
                OP_ALU,
                rd,
                OP3_FPOP1,
                rs1,
                (u32::from(opf) << 5) | rs2,
            ))))
        }

        Form::FpUn { opf, src, dst } => {
            if !arity(cx, m, span, ops, &[2]) {
                return None;
            }
            let rs2 = float_field(cx, &ops[0], src, "source")?;
            let rd = float_field(cx, &ops[1], dst, "destination")?;
            Some(one(Word::plain(format3(
                OP_ALU,
                rd,
                OP3_FPOP1,
                0,
                (u32::from(opf) << 5) | rs2,
            ))))
        }

        Form::FpCmp { opf, width } => {
            if !arity(cx, m, span, ops, &[2, 3]) {
                return None;
            }
            // V9 gave the floating-point unit four condition-code banks, and
            // a compare says which one it writes in the field an arithmetic
            // instruction would use for its destination. Left out, it is
            // `%fcc0`, which is the V8 instruction unchanged.
            let (fcc, rest) = if ops.len() == 3 {
                if cx.state.bits < 64 {
                    cx.error(
                        ops[0].span,
                        format!(
                            "`{m}` with a condition-code register is a SPARC V9 \
                             instruction; this target is V8"
                        ),
                    );
                    return None;
                }
                (fcc_field(cx, &ops[0])?, &ops[1..])
            } else {
                (0, ops)
            };
            let rs1 = float_field(cx, &rest[0], width, "first source")?;
            let rs2 = float_field(cx, &rest[1], width, "second source")?;
            Some(one(Word::plain(format3(
                OP_ALU,
                fcc,
                OP3_FPOP2,
                rs1,
                (u32::from(opf) << 5) | rs2,
            ))))
        }

        Form::FpMovCc { icc, fcc, width } => {
            if !arity(cx, m, span, ops, &[3]) {
                return None;
            }
            let (cc2, cc10) = cc_fields(cx, &ops[0])?;
            let Some(cond) = (if cc2 == 1 { icc } else { fcc }) else {
                cx.error(span, not_a_condition(m, cc2));
                return None;
            };
            // `opf_cc` numbers all six banks in one three-bit field, the
            // four `%fcc`s first and the integer pair above them, rather
            // than splitting them across the word as `MOVcc` does.
            let opf_cc = if cc2 == 1 { 4 + cc10 } else { cc10 };
            let rs2 = float_field(cx, &ops[1], width, "source")?;
            let rd = float_field(cx, &ops[2], width, "destination")?;
            Some(one(Word::plain(format3(
                OP_ALU,
                rd,
                OP3_FPOP2,
                u32::from(cond),
                (opf_cc << 11) | (fp_move_opf(width) << 5) | rs2,
            ))))
        }

        Form::FpMovReg { rcond, width } => {
            if !arity(cx, m, span, ops, &[3]) {
                return None;
            }
            let rs1 = int_reg(cx, &ops[0], "tested register")?;
            let rs2 = float_field(cx, &ops[1], width, "source")?;
            let rd = float_field(cx, &ops[2], width, "destination")?;
            // The register-tested move is the same opcode with bit 13 clear,
            // and its `opf_low` is four above the condition-code form's.
            Some(one(Word::plain(format3(
                OP_ALU,
                rd,
                OP3_FPOP2,
                u32::from(rs1.num),
                (u32::from(rcond) << 10) | ((fp_move_opf(width) + 4) << 5) | rs2,
            ))))
        }

        Form::MovCc { icc, fcc } => {
            if !arity(cx, m, span, ops, &[3]) {
                return None;
            }
            let (cc2, cc10) = cc_fields(cx, &ops[0])?;
            // `cc2` is 1 for `%icc` and `%xcc` and 0 for a `%fccN`, which is
            // also which condition table the name is read from.
            let Some(cond) = (if cc2 == 1 { icc } else { fcc }) else {
                cx.error(span, not_a_condition(m, cc2));
                return None;
            };
            // `cc1cc0` sits at bits 12-11, inside what would be the immediate
            // field, so a conditional move only has eleven bits of constant.
            let low = if let Some(r) = ops[1].int_reg() {
                (cc10 << 11) | u32::from(r.num)
            } else if let Some(imm) = ops[1].imm() {
                let v = small_signed(cx, &imm, 11, "a conditional move's immediate")?;
                I_BIT | (cc10 << 11) | (v as u32 & 0x7ff)
            } else {
                cx.error(ops[1].span, "expected a register or an immediate");
                return None;
            };
            let rd = int_reg(cx, &ops[2], "destination")?;
            Some(one(Word::plain(format3(
                OP_ALU,
                u32::from(rd.num),
                0x2c,
                (cc2 << 4) | u32::from(cond),
                low,
            ))))
        }

        Form::MovReg(rcond) => {
            if !arity(cx, m, span, ops, &[3]) {
                return None;
            }
            let rs1 = int_reg(cx, &ops[0], "tested register")?;
            let low = if let Some(r) = ops[1].int_reg() {
                (u32::from(rcond) << 10) | u32::from(r.num)
            } else if let Some(imm) = ops[1].imm() {
                let v = small_signed(cx, &imm, 10, "a register-conditional move's immediate")?;
                I_BIT | (u32::from(rcond) << 10) | (v as u32 & 0x3ff)
            } else {
                cx.error(ops[1].span, "expected a register or an immediate");
                return None;
            };
            let rd = int_reg(cx, &ops[2], "destination")?;
            Some(one(Word::plain(format3(
                OP_ALU,
                u32::from(rd.num),
                0x2f,
                u32::from(rs1.num),
                low,
            ))))
        }

        // Branches never reach here: they are assembled by `branch` below,
        // after their `,a` / `,pt` suffixes have been consumed.
        Form::Branch { .. } | Form::BranchFloat(_) | Form::BranchReg(_) => None,
    }
}

fn pack(word: u32, fixup: Option<Pending>) -> Word {
    match fixup {
        Some(f) => Word::fixed(word, f),
        None => Word::plain(word),
    }
}

/// The `,a` / `,pn` / `,pt` suffixes a branch mnemonic can carry.
#[derive(Copy, Clone, Default, Debug)]
pub struct BranchSuffix {
    /// `,a`: annul the delay slot when the branch is *not* taken (or always,
    /// for the unconditional `ba,a`).
    pub annul: bool,
    /// `,pn` / `,pt`: the V9 static prediction hint. Taken is the default.
    pub predict: Option<bool>,
}

/// What a conditional branch's mnemonic said, apart from its suffixes.
#[derive(Copy, Clone, Debug)]
pub struct BranchKind {
    pub cond: u8,
    /// `bp<cc>`, which is the V9 predicted form whatever the operands are.
    pub predicted: bool,
    /// `fb<cc>`, which is the other pair of opcodes and the other condition
    /// table; the two families are the same word otherwise, and neither
    /// accepts the other's condition-code bank.
    pub float: bool,
}

/// A conditional branch: `Bicc` / `FBfcc` with 22 bits of displacement or,
/// with a condition-code operand, the V9 `BPcc` / `FBPfcc`, which spend
/// three of those bits on the bank and the prediction hint.
pub fn branch(
    cx: &mut AsmCtx<'_>,
    m: &str,
    span: Span,
    BranchKind {
        cond,
        predicted,
        float,
    }: BranchKind,
    sfx: BranchSuffix,
    ops: &[Operand],
) -> Option<Vec<Variant>> {
    let a = u32::from(sfx.annul) << 4;
    let rd = a | u32::from(cond);

    let banked = predicted || (ops.len() == 2 && ops[0].is_cc());
    let (target_op, imm_bits) = if banked {
        // `be %icc, x` shares its mnemonic with the V8 `be x`, so the table
        // cannot mark it V9-only; the operand is what gives it away.
        if cx.state.bits < 64 {
            cx.error(
                span,
                format!(
                    "`{m}` with a condition-code register is a SPARC V9 branch; this target is V8"
                ),
            );
            return None;
        }
        if !arity(cx, m, span, ops, &[2]) {
            return None;
        }
        let (cc2, cc10) = cc_fields(cx, &ops[0])?;
        if (cc2 == 0) != float {
            cx.error(
                ops[0].span,
                if float {
                    "a floating-point branch tests a `%fcc`; `%icc` and `%xcc` need `b<cc>`"
                } else {
                    "floating-point condition codes need `fb<cc>`"
                },
            );
            return None;
        }
        let p = u32::from(sfx.predict.unwrap_or(true));
        (&ops[1], (cc10 << 20) | (p << 19))
    } else {
        if !arity(cx, m, span, ops, &[1]) {
            return None;
        }
        if sfx.predict.is_some() {
            cx.error(
                span,
                if float {
                    "`,pn` and `,pt` need a `%fcc` operand"
                } else {
                    "`,pn` and `,pt` need a `%icc` or `%xcc` operand"
                },
            );
            return None;
        }
        (&ops[0], 0)
    };

    let Some(imm) = target_op.imm().filter(|i| i.part == ImmPart::Whole) else {
        cx.error(target_op.span, "expected a branch target");
        return None;
    };
    let op2 = match (banked, float) {
        (false, false) => OP2_BICC,
        (false, true) => OP2_FBFCC,
        (true, false) => OP2_BPCC,
        (true, true) => OP2_FBPFCC,
    };
    let kind = if banked {
        disp19_fixup()
    } else {
        disp22_fixup()
    };
    Some(one(Word::fixed(
        format2(rd, op2, imm_bits),
        Pending {
            expr: imm.expr,
            kind,
            span: imm.span,
        },
    )))
}

/// V9 `BPr`: branch on the value of a whole register, with the displacement
/// split across two fields.
pub fn branch_reg(
    cx: &mut AsmCtx<'_>,
    m: &str,
    span: Span,
    rcond: u8,
    sfx: BranchSuffix,
    ops: &[Operand],
) -> Option<Vec<Variant>> {
    if !arity(cx, m, span, ops, &[2]) {
        return None;
    }
    let rs1 = int_reg(cx, &ops[0], "tested register")?;
    let Some(imm) = ops[1].imm().filter(|i| i.part == ImmPart::Whole) else {
        cx.error(ops[1].span, "expected a branch target");
        return None;
    };
    let rd = (u32::from(sfx.annul) << 4) | u32::from(rcond);
    let p = u32::from(sfx.predict.unwrap_or(true));
    let word = format2(rd, OP2_BPR, (p << 19) | (u32::from(rs1.num) << 14));
    Some(one(Word::fixed(
        word,
        Pending {
            expr: imm.expr,
            kind: disp16_fixup(),
            span: imm.span,
        },
    )))
}

/// Padding for `.align`: real `nop`s, so a jump into the padding still runs.
/// `nop` is `sethi 0, %g0`. A tail shorter than a word cannot be an
/// instruction, so it is zeroed.
pub fn nop_bytes(len: usize) -> Vec<u8> {
    let nop = format2(u32::from(reg::G0.num), OP2_SETHI, 0).to_be_bytes();
    let mut out = Vec::with_capacity(len);
    while out.len() + 4 <= len {
        out.extend_from_slice(&nop);
    }
    out.resize(len, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nop_is_sethi_zero_into_g0() {
        assert_eq!(nop_bytes(4), vec![0x01, 0x00, 0x00, 0x00]);
        assert_eq!(nop_bytes(8)[4..], [0x01, 0x00, 0x00, 0x00]);
        // A partial word cannot hold an instruction.
        assert_eq!(nop_bytes(6)[4..], [0x00, 0x00]);
        assert!(nop_bytes(0).is_empty());
    }

    #[test]
    fn scattered_displacements_leave_the_opcode_alone() {
        // `ba` with a 22-bit field: the opcode half must survive a negative
        // displacement, which is what a naive sign-extended OR would corrupt.
        let ba = u64::from(format2(8, OP2_BICC, 0));
        assert_eq!(disp22(ba, -4), ba | 0x3f_ffff);
        assert_eq!(disp22(ba, 4 * 3), ba | 3);
        // `call` keeps only bits 31-30.
        assert_eq!(call30(u64::from(format1(0)), -4), 0x7fff_ffff);
        // The 16-bit `BPr` field really is split in two.
        assert_eq!(disp16(0, 4 * 0x4000), 0x0010_0000);
        assert_eq!(disp16(0, 4), 1);
    }

    #[test]
    fn hi_and_lo_partition_a_32_bit_constant() {
        let v = 0x0001_2345;
        assert_eq!(hi22(0, v), 0x48);
        assert_eq!(lo10(0, v), 0x345);
        // Reassembling the two halves gives the original value back.
        assert_eq!((hi22(0, v) << 10) | lo10(0, v), v as u64);
        // A negative value is treated as its 32-bit two's complement.
        assert_eq!(hi22(0, -5000), 0x3f_fffb);
    }
}
