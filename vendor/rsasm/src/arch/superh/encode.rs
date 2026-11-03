//! Matching operands against the opcode table, and filling in the fields.
//!
//! Every SH instruction is one 16-bit word laid down in the target's byte
//! order, so a fixup always covers exactly that word and patches a field of
//! it through a [`FieldEncoding::Scatter`](crate::section::FieldEncoding::Scatter)
//! function. The scatter functions read and return the word as a number, so
//! the same function serves `sh` and `shl`.
//!
//! # Scaled displacements
//!
//! A displacement field counts in units of the operand size, not in bytes:
//! `mov.l @(disp,rn)` stores `disp / 4` in four bits and so reaches 0-60,
//! `mov.w` stores `disp / 2` and reaches 0-30, and `mov.b` reaches 0-15. The
//! same holds for the eight-bit `@(disp,gbr)` fields. The source always
//! writes the displacement in bytes, so a `mov.l` displacement that is not a
//! multiple of four has no encoding at all; it is an error, never rounded.
//! The fields are unsigned, so a negative displacement is an error too.

use super::cpu;
use super::insn::{Arg, Entry};
use super::operand::{Kind, Operand, Value};
use super::pcrel;
use super::reg::Reg;
use crate::arch::{AsmCtx, Endian};
use crate::expr::ExprRef;
use crate::section::{Fixup, FixupKind, Variant};
use crate::source::Span;

// ---- the word builder -----------------------------------------------------

/// A fixup whose position in the fragment is not decided yet.
pub struct Pending {
    pub expr: ExprRef,
    pub kind: FixupKind,
    pub span: Span,
}

/// The instruction words of one variant, plus the fixups into them.
pub struct Words {
    endian: Endian,
    bytes: Vec<u8>,
    fixups: Vec<Fixup>,
}

impl Words {
    pub fn new(endian: Endian) -> Words {
        Words {
            endian,
            bytes: Vec::new(),
            fixups: Vec::new(),
        }
    }

    /// Emits `word` together with the fixups that patch it.
    pub fn push(&mut self, word: u16, pending: impl IntoIterator<Item = Pending>) {
        let offset = self.bytes.len() as u32;
        self.bytes
            .extend_from_slice(&self.endian.bytes(word as u64, 2));
        for p in pending {
            self.fixups.push(Fixup {
                offset,
                expr: p.expr,
                kind: p.kind,
                span: p.span,
            });
        }
    }

    pub fn finish(self) -> Variant {
        Variant {
            bytes: self.bytes,
            fixups: self.fixups,
        }
    }
}

// ---- scatter functions ----------------------------------------------------
//
// Each takes the word already emitted and the resolved value and returns the
// patched word, leaving every bit outside its field alone.

fn low8(word: u64, v: i64) -> u64 {
    (word & !0xff) | (v as u64 & 0xff)
}

fn disp4_by1(word: u64, v: i64) -> u64 {
    (word & !0xf) | (v as u64 & 0xf)
}

fn disp4_by2(word: u64, v: i64) -> u64 {
    (word & !0xf) | ((v >> 1) as u64 & 0xf)
}

fn disp4_by4(word: u64, v: i64) -> u64 {
    (word & !0xf) | ((v >> 2) as u64 & 0xf)
}

pub fn disp8_by2(word: u64, v: i64) -> u64 {
    (word & !0xff) | ((v >> 1) as u64 & 0xff)
}

pub fn disp8_by4(word: u64, v: i64) -> u64 {
    (word & !0xff) | ((v >> 2) as u64 & 0xff)
}

// ---- immediates and displacements -----------------------------------------

/// The eight-bit immediate fields.
///
/// Some are sign-extended by the CPU (`mov`, `add`, `cmp/eq`) and some are
/// not (`and`, `tst`, `trapa`), but both spellings of a bit pattern are
/// accepted everywhere, so `and #-1, r0` and `mov #255, r0` mean what they
/// look like at the bit level. GNU as is laxer still and takes down to -255.
const IMM8: (i64, i64) = (-128, 255);

fn imm8(cx: &mut AsmCtx<'_>, v: Value, word: &mut u16, pending: &mut Vec<Pending>) -> Option<()> {
    match cx.constant(v.expr) {
        Some(n) => {
            if !(IMM8.0..=IMM8.1).contains(&n) {
                cx.error(
                    v.span,
                    format!(
                        "immediate {n} does not fit in 8 bits ({} to {})",
                        IMM8.0, IMM8.1
                    ),
                );
                return None;
            }
            *word |= (n & 0xff) as u16;
        }
        None => pending.push(Pending {
            expr: v.expr,
            kind: FixupKind::data(2).with_field(8, 1).scatter(low8),
            span: v.span,
        }),
    }
    Some(())
}

/// A scaled, unsigned displacement of `field_bits` bits in the low bits of
/// the word.
fn displacement(
    cx: &mut AsmCtx<'_>,
    mnemonic: &str,
    v: Value,
    field_bits: u32,
    scale: u8,
    word: &mut u16,
    pending: &mut Vec<Pending>,
) -> Option<()> {
    let scale_i = scale as i64;
    let max = ((1i64 << field_bits) - 1) * scale_i;
    match cx.constant(v.expr) {
        Some(n) => {
            if n % scale_i != 0 {
                cx.error(
                    v.span,
                    format!(
                        "displacement {n} is not a multiple of {scale}: `{mnemonic}` counts \
                         its displacement in {scale}-byte steps"
                    ),
                );
                return None;
            }
            if !(0..=max).contains(&n) {
                cx.error(
                    v.span,
                    format!("displacement {n} is out of range for `{mnemonic}` (0 to {max})"),
                );
                return None;
            }
            *word |= (n / scale_i) as u16;
        }
        None => {
            // The field holds `field_bits` bits of *count*, so the value in
            // bytes needs one more bit per doubling of the scale. The core's
            // unsigned range also admits a small negative band, which the
            // constant path above rejects but a label difference cannot.
            let value_bits = field_bits + scale.trailing_zeros();
            let f = match (field_bits, scale) {
                (4, 1) => disp4_by1,
                (4, 2) => disp4_by2,
                (4, _) => disp4_by4,
                (_, 1) => low8,
                (_, 2) => disp8_by2,
                _ => disp8_by4,
            };
            pending.push(Pending {
                expr: v.expr,
                kind: FixupKind::data(2)
                    .with_field(value_bits as u8, scale)
                    .scatter(f),
                span: v.span,
            });
        }
    }
    Some(())
}

// ---- matching ---------------------------------------------------------------

/// Whether `op` can fill the slot `arg`, before any range checks.
fn matches(arg: Arg, op: &Operand) -> bool {
    match (arg, op.kind) {
        (Arg::RegN | Arg::RegM, Kind::Reg(Reg::Gpr(_))) => true,
        (Arg::R0, Kind::Reg(Reg::Gpr(0))) => true,
        (Arg::Imm8, Kind::Imm(_)) => true,
        (Arg::IndN | Arg::IndM, Kind::Ind(_)) => true,
        (Arg::IncN | Arg::IncM, Kind::PostInc(_)) => true,
        (Arg::DecN | Arg::DecM, Kind::PreDec(_)) => true,
        (Arg::R0IdxN | Arg::R0IdxM, Kind::R0Index(_)) => true,
        (Arg::DispN(_) | Arg::DispM(_), Kind::Disp(..)) => true,
        (Arg::GbrDisp(_), Kind::GbrDisp(_)) => true,
        (Arg::R0Gbr, Kind::R0Gbr) => true,
        (Arg::PcRel(_), Kind::Addr(_) | Kind::PcDisp(_)) => true,
        (Arg::Branch8 | Arg::Branch12, Kind::Addr(_)) => true,
        (Arg::Ctl(c), Kind::Reg(Reg::Ctl(d))) => c == d,
        (Arg::BankM, Kind::Reg(Reg::Bank(_))) => true,
        (Arg::FrN | Arg::FrM, Kind::Reg(Reg::Fr(_))) => true,
        (Arg::Fr0, Kind::Reg(Reg::Fr(0))) => true,
        (Arg::DrN | Arg::DrM, Kind::Reg(Reg::Dr(_))) => true,
        (Arg::FvN | Arg::FvM, Kind::Reg(Reg::Fv(_))) => true,
        (Arg::Xmtrx, Kind::Reg(Reg::Xmtrx)) => true,
        _ => false,
    }
}

/// Fills the bits `op` contributes in slot `arg`. PC-relative slots are not
/// handled here: they decide the whole variant list.
fn place(
    cx: &mut AsmCtx<'_>,
    mnemonic: &str,
    arg: Arg,
    op: &Operand,
    word: &mut u16,
    pending: &mut Vec<Pending>,
) -> Option<()> {
    let n = |r: u8| (r as u16) << 8;
    let m = |r: u8| (r as u16) << 4;
    match (arg, op.kind) {
        (Arg::RegN, Kind::Reg(Reg::Gpr(r)))
        | (Arg::IndN, Kind::Ind(r))
        | (Arg::IncN, Kind::PostInc(r))
        | (Arg::DecN, Kind::PreDec(r))
        | (Arg::R0IdxN, Kind::R0Index(r))
        | (Arg::FrN, Kind::Reg(Reg::Fr(r)))
        | (Arg::DrN, Kind::Reg(Reg::Dr(r)))
        | (Arg::FvN, Kind::Reg(Reg::Fv(r))) => *word |= n(r),
        (Arg::RegM, Kind::Reg(Reg::Gpr(r)))
        | (Arg::IndM, Kind::Ind(r))
        | (Arg::IncM, Kind::PostInc(r))
        | (Arg::DecM, Kind::PreDec(r))
        | (Arg::R0IdxM, Kind::R0Index(r))
        | (Arg::FrM, Kind::Reg(Reg::Fr(r)))
        | (Arg::DrM, Kind::Reg(Reg::Dr(r))) => *word |= m(r),
        // `fipr fvm,fvn` shares one nibble: fvn (0, 4, 8, 12) in its top
        // half, fvm's index (0-3) in its bottom half.
        (Arg::FvM, Kind::Reg(Reg::Fv(r))) => *word |= n(r >> 2),
        (Arg::BankM, Kind::Reg(Reg::Bank(r))) => *word |= m(r | 8),
        (Arg::Imm8, Kind::Imm(v)) => imm8(cx, v, word, pending)?,
        (Arg::DispN(scale), Kind::Disp(v, r)) => {
            *word |= n(r);
            displacement(cx, mnemonic, v, 4, scale, word, pending)?;
        }
        (Arg::DispM(scale), Kind::Disp(v, r)) => {
            *word |= m(r);
            displacement(cx, mnemonic, v, 4, scale, word, pending)?;
        }
        (Arg::GbrDisp(scale), Kind::GbrDisp(v)) => {
            displacement(cx, mnemonic, v, 8, scale, word, pending)?;
        }
        _ => {}
    }
    Some(())
}

/// Assembles `ops` as one of `entries`, all of which share a mnemonic.
pub fn encode(
    cx: &mut AsmCtx<'_>,
    mnemonic: &str,
    entries: &[&'static Entry],
    ops: &[Operand],
    span: Span,
    endian: Endian,
) -> Option<Vec<Variant>> {
    for op in ops {
        if let Kind::Reg(Reg::Reserved(name)) = op.kind {
            cx.error(
                op.span,
                format!(
                    "`{name}` is an SH-2A or SH-DSP register, which this backend does not assemble"
                ),
            );
            return None;
        }
    }

    let Some(entry) = entries
        .iter()
        .copied()
        .find(|e| e.args.len() == ops.len() && e.args.iter().zip(ops).all(|(a, o)| matches(*a, o)))
    else {
        no_match(cx, mnemonic, entries, ops, span);
        return None;
    };

    // GNU as keeps the intersection of the CPU sets of the forms it has
    // picked so far, and refuses a form that would leave no CPU in it.
    if !cpu::valid(cpu::remaining(cx.state) & entry.arch) {
        cx.error(
            span,
            format!(
                "`{mnemonic}` in this form needs {}, which the selected SH variant lacks",
                cpu::describe(entry.arch)
            ),
        );
        return None;
    }
    cpu::record(cx.state, entry.arch);

    let mut word = entry.word;
    let mut pending = Vec::new();
    let mut target = None;
    for (arg, op) in entry.args.iter().zip(ops) {
        match arg {
            Arg::PcRel(_) | Arg::Branch8 | Arg::Branch12 => target = Some((*arg, op)),
            _ => place(cx, mnemonic, *arg, op, &mut word, &mut pending)?,
        }
    }
    // A PC-relative operand decides the variant list itself: only registers
    // ever sit beside one, so everything else is already in `word`.
    let Some((arg, op)) = target else {
        let mut w = Words::new(endian);
        w.push(word, pending);
        return Some(vec![w.finish()]);
    };
    match (arg, op.kind) {
        (Arg::PcRel(scale), kind) => pcrel::load(cx, mnemonic, word, scale, kind, op.span, endian),
        (Arg::Branch8, Kind::Addr(v)) => Some(pcrel::cond_branch(word, v, endian)),
        (_, Kind::Addr(v)) => Some(vec![pcrel::branch(word, v, endian)]),
        // `matches` admits only an address to a branch slot.
        _ => {
            cx.error(op.span, format!("`{mnemonic}` needs a label here"));
            None
        }
    }
}

/// Explains why no form of `mnemonic` accepts `ops`, listing the forms.
fn no_match(
    cx: &mut AsmCtx<'_>,
    mnemonic: &str,
    entries: &[&'static Entry],
    ops: &[Operand],
    span: Span,
) {
    let found = if ops.is_empty() {
        "no operands".to_string()
    } else {
        ops.iter()
            .map(Operand::describe)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let forms = entries
        .iter()
        .map(|e| {
            let args = e
                .args
                .iter()
                .map(|a| a.spelling())
                .collect::<Vec<_>>()
                .join(",");
            if args.is_empty() {
                format!("`{mnemonic}`")
            } else {
                format!("`{mnemonic} {args}`")
            }
        })
        .collect::<Vec<_>>();
    let forms_text = forms.join(", ");
    let mut msg =
        format!("invalid operands for `{mnemonic}`: found {found}; expected {forms_text}");
    // The commonest slip: a number with no `#` reads as a PC-relative address.
    let wants_imm = |i: usize| entries.iter().any(|e| e.args.get(i) == Some(&Arg::Imm8));
    if ops
        .iter()
        .enumerate()
        .any(|(i, o)| matches!(o.kind, Kind::Addr(_)) && wants_imm(i))
    {
        msg.push_str(" (an immediate needs a `#`)");
    }
    cx.error(span, msg);
}
