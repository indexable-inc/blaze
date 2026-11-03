//! Matches parsed operands against the table's forms and emits the bytes.
//!
//! Every form of a mnemonic is tried; of those that accept the operands, the
//! shortest wins, ties going to the manual's order. Operand fields always go
//! through a fixup, so the core folds, range-checks and places them the same
//! way whether the value is a literal or a label resolved at layout time.
//!
//! # How an address picks `saddr` or `sfr`
//!
//! An address without a sigil is short direct addressing (FE20H–FF1FH) or
//! SFR addressing (FF00H–FFCFH and FFE0H–FFFFH), per Table 2-19 of the RA78K0
//! language manual (U17198EJ1V0UM00, page 68). Where both could apply —
//! FF00H–FF1FH — the short direct form is used, which is what note 2 of Table
//! 2-21 (page 71) says RA78K0 does and why U12326EJ4V0UM lists it first.
//!
//! The same Table 2-21 allows an `sfr` operand only as an *absolute* value,
//! referenced backwards, while `saddr` may be any expression. So an address
//! this backend can fold now is classified by its value, and anything else —
//! a label, a forward reference — is short direct addressing, range-checked
//! when the layout resolves it. The choice is made once, here, and never by
//! relaxation — even where the two forms differ in length (`SET1 saddr.bit` is
//! two bytes, `SET1 sfr.bit` three) — because RA78K0 does not make it late
//! either, and a layout-time switch would let a mistyped RAM label quietly
//! become an SFR access.
//!
//! The only size choice CA78K0 makes is its `BR` directive (section 3.7,
//! pages 114–116): `BR expr` with no sigil becomes `BR $expr` (2 bytes) when
//! the target is within −80H..+7FH of the next instruction, and `BR !expr`
//! (3 bytes) otherwise. That is two [`Variant`]s.

use super::form::{Code, Field, Form, Slot, Var, forms};
use super::operand::{self, Arg, BitBase, Operand};
use crate::arch::{AsmCtx, InsnRequest};
use crate::expr::{BinOp, ExprKind, ExprRef};
use crate::section::{Fixup, FixupKind, Variant};
use crate::source::Span;

/// Short direct addressing: FE20H–FF1FH.
pub const SADDR: (i64, i64) = (0xfe20, 0xff1f);
/// SFR addressing, less the FF00H–FF1FH part that short direct addressing
/// takes: FF20H–FFCFH and FFE0H–FFFFH. FFD0H–FFDFH has no SFR address
/// (U12326EJ4V0UM Table 4-1 note, page 32).
fn is_sfr(v: i64) -> bool {
    (0xff20..=0xffcf).contains(&v) || (0xffe0..=0xffff).contains(&v)
}
/// `CALLF !addr11`: 0800H–0FFFH.
pub const ADDR11: (i64, i64) = (0x0800, 0x0fff);
/// `CALLT [addr5]`: even addresses 40H–7EH.
pub const ADDR5: (i64, i64) = (0x40, 0x7e);

// Each address field is a fixup on the address *minus a bias* chosen so the
// valid range is exactly a signed power-of-two window, which is the only kind
// of range a fixup can check. The scatter function adds the bias back.
//
//   saddr   FE20H..FF1FH  = FEA0H + [-80H, 7FH]   -> low byte
//   sfr     FF00H..FFFFH  = FF80H + [-80H, 7FH]   -> low byte
//   addr11  0800H..0FFFH  = 0C00H + [-400H, 3FFH] -> fa10..fa0
//   addr5   40H..7EH, even = 60H + [-20H, 1EH]    -> ta4..ta0 in bits 5..1
const SADDR_BIAS: u64 = 0xfea0;
const SFR_BIAS: u64 = 0xff80;
const ADDR11_BIAS: u64 = 0x0c00;
const ADDR5_BIAS: u64 = 0x60;

fn scatter_saddr(_word: u64, v: i64) -> u64 {
    (v as u64).wrapping_add(SADDR_BIAS) & 0xff
}

fn scatter_sfr(_word: u64, v: i64) -> u64 {
    (v as u64).wrapping_add(SFR_BIAS) & 0xff
}

/// `CALLF`: `0 fa10–8 1100` then `fa7–0`, read little-endian as one word.
/// `form::parse_row` guarantees that layout for any row using `fa`.
fn scatter_addr11(word: u64, v: i64) -> u64 {
    let fa = (v as u64).wrapping_add(ADDR11_BIAS) & 0x7ff;
    (word & !0xff70) | ((fa >> 8) << 4) | ((fa & 0xff) << 8)
}

/// `CALLT`: `11 ta4–0 1`. The address is `40H + 2 * ta`, so `ta << 1` is
/// simply the address less 40H.
fn scatter_addr5(word: u64, v: i64) -> u64 {
    let addr = (v as u64).wrapping_add(ADDR5_BIAS);
    (word & !0x3e) | (addr.wrapping_sub(0x40) & 0x3e)
}

/// The values bound to one form's operands.
#[derive(Default, Clone, Copy)]
struct Bound {
    r: u32,
    p: u32,
    b: u32,
    n: u32,
    /// `Data`: `#byte` or the displacement of `[HL+byte]`.
    data: Option<(ExprRef, Span)>,
    word: Option<(ExprRef, Span)>,
    saddr: Option<(ExprRef, Span, bool)>,
    sfr: Option<(ExprRef, Span, bool)>,
    addr16: Option<(ExprRef, Span)>,
    addr11: Option<(ExprRef, Span)>,
    addr5: Option<(ExprRef, Span)>,
    rel: Option<(ExprRef, Span)>,
}

/// Why a form did not take the operands.
enum Miss {
    /// The operand is simply a different kind.
    Shape,
    /// The operand is the right kind but its value is not allowed. Reported if
    /// no other form matches.
    Value(Span, String),
}

pub fn assemble(
    cx: &mut AsmCtx<'_>,
    insn: &InsnRequest<'_>,
    mnemonic: &str,
) -> Option<Vec<Variant>> {
    let upper = mnemonic.to_ascii_uppercase();
    let mut candidates: Vec<&'static Form> =
        forms().iter().filter(|f| f.row.mnemonic == upper).collect();
    if candidates.is_empty() {
        cx.error(
            insn.mnemonic_span,
            format!("unknown 78K0 instruction `{mnemonic}`"),
        );
        return None;
    }
    let args = operand::parse_all(cx, insn.operands, insn.span)?;

    if upper == "BR"
        && let [
            Arg {
                op: Operand::Bare(e),
                span,
            },
        ] = args.as_slice()
    {
        return br_directive(cx, &candidates, *e, *span);
    }

    // Shortest first; the sort is stable, so equal lengths keep table order.
    candidates.sort_by_key(|f| f.len());
    let mut first_miss = None;
    for form in &candidates {
        if form.slots.len() != args.len() {
            continue;
        }
        match bind(cx, form, &args) {
            Ok(bound) => return Some(vec![emit(cx, form, &bound)]),
            Err(Miss::Value(span, msg)) => {
                first_miss.get_or_insert((span, msg));
            }
            Err(Miss::Shape) => {}
        }
    }
    if let Some((span, msg)) = first_miss {
        cx.error(span, msg);
        return None;
    }
    let mut accepted: Vec<&str> = Vec::new();
    for f in forms().iter().filter(|f| f.row.mnemonic == upper) {
        let text = if f.row.operands.is_empty() {
            "no operands"
        } else {
            f.row.operands
        };
        if !accepted.contains(&text) {
            accepted.push(text);
        }
    }
    cx.error(
        insn.span,
        format!(
            "invalid operands for `{upper}`; it takes {}",
            accepted.join(" | ")
        ),
    );
    None
}

fn br_directive(
    cx: &mut AsmCtx<'_>,
    candidates: &[&'static Form],
    e: ExprRef,
    span: Span,
) -> Option<Vec<Variant>> {
    let find = |slot| {
        candidates
            .iter()
            .find(|f| f.slots.as_slice() == [slot])
            .copied()
    };
    let (Some(rel), Some(abs)) = (find(Slot::Rel), find(Slot::Addr16)) else {
        // Unreachable while the table has both `BR` rows; the tests check it.
        cx.error(
            span,
            "`BR` without a sigil needs both `BR $addr16` and `BR !addr16`",
        );
        return None;
    };
    let short = Bound {
        rel: Some((e, span)),
        ..Bound::default()
    };
    let long = Bound {
        addr16: Some((e, span)),
        ..Bound::default()
    };
    Some(vec![emit_plain(rel, &short), emit_plain(abs, &long)])
}

fn hex(v: i64) -> String {
    if v < 0 {
        format!("-{:X}H", v.unsigned_abs())
    } else if format!("{v:X}").starts_with(|c: char| c.is_ascii_alphabetic()) {
        format!("0{v:X}H")
    } else {
        format!("{v:X}H")
    }
}

/// Tries to bind every operand to the form's slots.
fn bind(cx: &mut AsmCtx<'_>, form: &Form, args: &[Arg]) -> Result<Bound, Miss> {
    // Shapes first, so that a value complaint is only ever about a form whose
    // every operand is of the right kind: `MOV A,0FD00H` should hear about
    // the address, not that `MOV r,A` does not take A.
    if !form
        .slots
        .iter()
        .zip(args)
        .all(|(s, a)| shape_fits(*s, a.op))
    {
        return Err(Miss::Shape);
    }
    let mut b = Bound::default();
    for (slot, arg) in form.slots.iter().zip(args) {
        bind_one(cx, form, *slot, arg, &mut b)?;
    }
    Ok(b)
}

/// Whether an operand is the kind of thing a slot takes, ignoring its value.
fn shape_fits(slot: Slot, op: Operand) -> bool {
    matches!(
        (slot, op),
        (Slot::R, Operand::Reg8(_))
            | (Slot::Rp, Operand::Reg16(_))
            | (Slot::Reg(_), Operand::Reg8(_))
            | (Slot::Ax, Operand::Reg16(_))
            | (Slot::Sp, Operand::Sp)
            | (Slot::Psw, Operand::Psw)
            | (Slot::Cy, Operand::Cy)
            | (Slot::De, Operand::De)
            | (Slot::Hl, Operand::Hl)
            | (Slot::HlB, Operand::HlB)
            | (Slot::HlC, Operand::HlC)
            | (Slot::One, Operand::Bare(_))
            | (Slot::Byte | Slot::Word, Operand::Imm(_))
            | (Slot::HlByte, Operand::HlByte(_))
            | (
                Slot::Saddr | Slot::Saddrp | Slot::Sfr | Slot::Sfrp,
                Operand::Bare(_)
            )
            | (
                Slot::SaddrBit | Slot::SfrBit,
                Operand::Bit(BitBase::Addr(_), _)
            )
            | (Slot::ABit, Operand::Bit(BitBase::A, _))
            | (Slot::PswBit, Operand::Bit(BitBase::Psw, _))
            | (Slot::HlBit, Operand::Bit(BitBase::Hl, _))
            | (Slot::Addr16 | Slot::Addr11, Operand::Abs(_))
            | (Slot::Addr5, Operand::Ind(_))
            | (Slot::Rel, Operand::Rel(_))
            | (Slot::Bank, Operand::Bank(_))
    )
}

fn bind_one(
    cx: &mut AsmCtx<'_>,
    form: &Form,
    slot: Slot,
    arg: &Arg,
    b: &mut Bound,
) -> Result<(), Miss> {
    let span = arg.span;
    match (slot, arg.op) {
        (Slot::R, Operand::Reg8(r)) => {
            if !form.values(Var::R).contains(&(r as u32)) {
                return Err(Miss::Value(
                    span,
                    format!(
                        "`{} {}` does not take A as `r`",
                        form.row.mnemonic, form.row.operands
                    ),
                ));
            }
            b.r = r as u32;
        }
        (Slot::Rp, Operand::Reg16(p)) => {
            if !form.values(Var::P).contains(&(p as u32)) {
                return Err(Miss::Value(
                    span,
                    format!(
                        "`{} {}` takes BC, DE or HL as `rp`",
                        form.row.mnemonic, form.row.operands
                    ),
                ));
            }
            b.p = p as u32;
        }
        (Slot::Reg(want), Operand::Reg8(r)) if want == r => {}
        (Slot::Ax, Operand::Reg16(0)) => {}
        (Slot::Sp, Operand::Sp) | (Slot::Psw, Operand::Psw) | (Slot::Cy, Operand::Cy) => {}
        (Slot::De, Operand::De)
        | (Slot::Hl, Operand::Hl)
        | (Slot::HlB, Operand::HlB)
        | (Slot::HlC, Operand::HlC) => {}
        (Slot::One, Operand::Bare(e)) => match cx.constant(e) {
            Some(1) => {}
            _ => {
                return Err(Miss::Value(
                    span,
                    format!("`{}` rotates by exactly 1: write `A,1`", form.row.mnemonic),
                ));
            }
        },
        (Slot::Byte, Operand::Imm(e)) | (Slot::HlByte, Operand::HlByte(e)) => {
            b.data = Some((e, span));
        }
        (Slot::Word, Operand::Imm(e)) => b.word = Some((e, span)),
        (Slot::Saddr | Slot::Saddrp, Operand::Bare(e)) => {
            let even = slot == Slot::Saddrp;
            short_direct(cx, form, e, span, even)?;
            b.saddr = Some((e, span, even));
        }
        (Slot::Sfr | Slot::Sfrp, Operand::Bare(e)) => {
            let even = slot == Slot::Sfrp;
            sfr(cx, e, span, even)?;
            b.sfr = Some((e, span, even));
        }
        (Slot::SaddrBit, Operand::Bit(BitBase::Addr(e), bit)) => {
            short_direct(cx, form, e, span, false)?;
            b.saddr = Some((e, span, false));
            b.b = bit as u32;
        }
        (Slot::SfrBit, Operand::Bit(BitBase::Addr(e), bit)) => {
            sfr(cx, e, span, false)?;
            b.sfr = Some((e, span, false));
            b.b = bit as u32;
        }
        (Slot::ABit, Operand::Bit(BitBase::A, bit))
        | (Slot::PswBit, Operand::Bit(BitBase::Psw, bit))
        | (Slot::HlBit, Operand::Bit(BitBase::Hl, bit)) => b.b = bit as u32,
        (Slot::Addr16, Operand::Abs(e)) => {
            if let Some(v) = cx.constant(e)
                && !(0..=0xffff).contains(&v)
            {
                return Err(Miss::Value(
                    span,
                    format!("address {} is out of range (0H to 0FFFFH)", hex(v)),
                ));
            }
            b.addr16 = Some((e, span));
        }
        (Slot::Addr11, Operand::Abs(e)) => {
            if let Some(v) = cx.constant(e)
                && !(ADDR11.0..=ADDR11.1).contains(&v)
            {
                return Err(Miss::Value(
                    span,
                    format!("`CALLF` target {} is out of range (0800H to 0FFFH)", hex(v)),
                ));
            }
            b.addr11 = Some((e, span));
        }
        (Slot::Addr5, Operand::Ind(e)) => {
            if let Some(v) = cx.constant(e)
                && (!(ADDR5.0..=ADDR5.1).contains(&v) || v % 2 != 0)
            {
                return Err(Miss::Value(
                    span,
                    format!(
                        "`CALLT` table address {} is out of range (an even address, 40H to 7EH)",
                        hex(v)
                    ),
                ));
            }
            b.addr5 = Some((e, span));
        }
        (Slot::Rel, Operand::Rel(e)) => b.rel = Some((e, span)),
        (Slot::Bank, Operand::Bank(n)) => b.n = n as u32,
        _ => return Err(Miss::Shape),
    }
    Ok(())
}

/// Accepts an address as short direct addressing, or explains why not.
fn short_direct(
    cx: &mut AsmCtx<'_>,
    form: &Form,
    e: ExprRef,
    span: Span,
    even: bool,
) -> Result<(), Miss> {
    // Not yet known: short direct addressing, checked at layout.
    let Some(v) = cx.constant(e) else {
        return Ok(());
    };
    if (SADDR.0..=SADDR.1).contains(&v) {
        if even && v % 2 != 0 {
            return Err(Miss::Value(
                span,
                format!("16-bit short direct address {} must be even", hex(v)),
            ));
        }
        return Ok(());
    }
    // An address in SFR space belongs to the mnemonic's `sfr` form, if it has
    // one, and any complaint about it (an odd `sfrp`) should come from there.
    let has_sfr = forms().iter().any(|f| {
        f.row.mnemonic == form.row.mnemonic
            && f.slots
                .iter()
                .any(|s| matches!(s, Slot::Sfr | Slot::Sfrp | Slot::SfrBit))
    });
    if has_sfr && is_sfr(v) {
        return Err(Miss::Shape);
    }
    if (0xffd0..=0xffdf).contains(&v) {
        return Err(Miss::Value(
            span,
            format!(
                "address {} is in 0FFD0H to 0FFDFH, which has no short direct or SFR \
                 addressing; write `!{}` for absolute addressing",
                hex(v),
                hex(v)
            ),
        ));
    }
    let has_addr16 = forms()
        .iter()
        .any(|f| f.row.mnemonic == form.row.mnemonic && f.slots.contains(&Slot::Addr16));
    let hint = if has_addr16 {
        format!("; write `!{}` for absolute addressing", hex(v))
    } else {
        String::new()
    };
    Err(Miss::Value(
        span,
        format!(
            "address {} is out of range for short direct addressing (0FE20H to 0FF1FH){hint}",
            hex(v)
        ),
    ))
}

/// Accepts an address as SFR addressing, which must be absolute and known.
fn sfr(cx: &mut AsmCtx<'_>, e: ExprRef, span: Span, even: bool) -> Result<(), Miss> {
    match cx.constant(e) {
        Some(v) if is_sfr(v) => {
            if even && v % 2 != 0 {
                return Err(Miss::Value(
                    span,
                    format!("16-bit SFR address {} must be even", hex(v)),
                ));
            }
            Ok(())
        }
        _ => Err(Miss::Shape),
    }
}

fn biased(cx: &mut AsmCtx<'_>, e: ExprRef, bias: u64, span: Span) -> ExprRef {
    let k = cx.exprs.int(bias, span);
    cx.exprs.alloc(ExprKind::Binary(BinOp::Sub, e, k), span)
}

fn emit(cx: &mut AsmCtx<'_>, form: &Form, b: &Bound) -> Variant {
    let mut b = *b;
    // The biased expressions need the arena, so build them before the bytes.
    if let Some((e, span, even)) = b.saddr {
        b.saddr = Some((biased(cx, e, SADDR_BIAS, span), span, even));
    }
    if let Some((e, span, even)) = b.sfr {
        b.sfr = Some((biased(cx, e, SFR_BIAS, span), span, even));
    }
    if let Some((e, span)) = b.addr11 {
        b.addr11 = Some((biased(cx, e, ADDR11_BIAS, span), span));
    }
    if let Some((e, span)) = b.addr5 {
        b.addr5 = Some((biased(cx, e, ADDR5_BIAS, span), span));
    }
    emit_plain(form, &b)
}

/// Lays out the bytes. Address fields in `b` must already be biased, except
/// that forms with none (the `BR` directive's) can come here directly.
fn emit_plain(form: &Form, b: &Bound) -> Variant {
    let len = form.len();
    let mut bytes = Vec::with_capacity(len);
    let mut fixups = Vec::new();
    let mut fix = |at: usize, (expr, span): (ExprRef, Span), kind: FixupKind| {
        fixups.push(Fixup {
            offset: at as u32,
            expr,
            kind,
            span,
        });
    };
    for (i, code) in form.codes.iter().enumerate() {
        match code {
            Code::Bits { fixed, .. } => {
                let mut byte = *fixed;
                byte = code.place(Var::R, b.r, byte);
                byte = code.place(Var::P, b.p, byte);
                byte = code.place(Var::B, b.b, byte);
                byte = code.place(Var::N, b.n, byte);
                if matches!(code, Code::Bits { vars, .. } if vars.contains(&Some(Var::F)))
                    && let Some(a) = b.addr11
                {
                    fix(
                        i,
                        a,
                        FixupKind::data(2)
                            .signed()
                            .with_field(11, 1)
                            .scatter(scatter_addr11),
                    );
                }
                if matches!(code, Code::Bits { vars, .. } if vars.contains(&Some(Var::T)))
                    && let Some(a) = b.addr5
                {
                    fix(
                        i,
                        a,
                        FixupKind::data(1)
                            .signed()
                            .with_field(6, 2)
                            .scatter(scatter_addr5),
                    );
                }
                bytes.push(byte);
            }
            Code::Field(f) => {
                match f {
                    Field::Data => {
                        if let Some(d) = b.data {
                            fix(i, d, FixupKind::data(1));
                        }
                    }
                    Field::LowByte => {
                        if let Some(w) = b.word {
                            fix(i, w, FixupKind::data(2));
                        }
                    }
                    Field::LowAddr => {
                        if let Some(a) = b.addr16 {
                            fix(i, a, FixupKind::data(2));
                        }
                    }
                    Field::SaddrOffset => {
                        if let Some((e, span, even)) = b.saddr {
                            let align = if even { 2 } else { 1 };
                            let kind = FixupKind::data(1)
                                .signed()
                                .with_field(8, align)
                                .scatter(scatter_saddr);
                            fix(i, (e, span), kind);
                        }
                    }
                    Field::SfrOffset => {
                        if let Some((e, span, even)) = b.sfr {
                            let align = if even { 2 } else { 1 };
                            let kind = FixupKind::data(1)
                                .signed()
                                .with_field(8, align)
                                .scatter(scatter_sfr);
                            fix(i, (e, span), kind);
                        }
                    }
                    Field::Jdisp => {
                        // Relative to the next instruction, which starts
                        // `len - i` bytes past this field.
                        if let Some(r) = b.rel {
                            fix(i, r, FixupKind::pcrel(1, (len - i) as i8));
                        }
                    }
                    // Written by the fixups on `Low byte`, `Low addr` and the
                    // `fa10–8` byte.
                    Field::HighByte | Field::HighAddr | Field::Fa7_0 => {}
                }
                bytes.push(0);
            }
        }
    }
    Variant { bytes, fixups }
}
