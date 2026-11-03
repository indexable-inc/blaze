//! Effective addresses, extension words, and assembling the pieces of an
//! instruction into [`Variant`]s.
//!
//! A 68000 instruction is an opcode word, then the extension words of each
//! operand in order. An operand's encoding can change the opcode word — a
//! PC-relative operand is mode 7/2 with a 16-bit displacement but 7/3 with a
//! 68020 full extension word — so an operand is encoded to a list of
//! alternatives, each carrying its mode and register bits, and [`build`]
//! places them.
//!
//! # Choosing a size
//!
//! Sizes follow GNU as, the reference, as far as that is a property of the
//! value rather than of GNU as's relaxation machinery:
//!
//! - A constant gets the shortest form that holds it: `0(a0)` is `(a0)`, and
//!   an absolute address is `abs.W` whenever it survives sign-extension from
//!   16 bits.
//! - A value not known when the instruction is read — a forward reference or
//!   an external symbol — gets the form GNU as gives it, since nothing better
//!   can be known: `abs.L`, and a 32-bit base displacement on the 68020 (16
//!   bits on the 68000). With an index the 68000 has 8 bits, and the 68020
//!   gets 32 bits in GNU syntax but 16 in Motorola syntax, following
//!   `m68k-elf-as` and `m68k-elf-as --mri` respectively.
//! - A PC-relative reference is the exception, and relaxes: its distance is
//!   only known at layout, which is when [`crate::layout`] picks among the
//!   alternatives listed here.

use super::Cpu;
use super::float::Float;
use super::operand::{Base, Index, IndexAt, Mode, Operand, Value, Width};
use super::reloc;
use crate::arch::AsmCtx;
use crate::expr::ExprRef;
use crate::lexer::Dialect;
use crate::section::{Fixup, FixupKind, Variant};
use crate::source::Span;

// Addressing-mode classes, as the Motorola manuals group them.
pub const DN: u16 = 1;
pub const AN: u16 = 2;
pub const IND: u16 = 4;
pub const POST: u16 = 8;
pub const PRE: u16 = 16;
/// `d(An)`, indexed and memory-indirect modes on an address register or no
/// register at all.
pub const DISP: u16 = 32;
pub const ABS: u16 = 64;
pub const PCREL: u16 = 128;
pub const IMM: u16 = 256;

pub const MEM_ALT: u16 = IND | POST | PRE | DISP | ABS;
pub const DATA_ALT: u16 = DN | MEM_ALT;
pub const ALTERABLE: u16 = DATA_ALT | AN;
pub const CONTROL: u16 = IND | DISP | ABS | PCREL;
pub const DATA: u16 = DATA_ALT | PCREL | IMM;
pub const ALL: u16 = DATA | AN;

/// The addressing-mode class of an operand, or 0 for something that is not
/// an effective address at all (`sr`, a register list).
pub fn class(op: &Operand) -> u16 {
    match &op.mode {
        Mode::DReg(_) => DN,
        Mode::AReg(_) => AN,
        Mode::Ind(_) => IND,
        Mode::PostInc(_) => POST,
        Mode::PreDec(_) => PRE,
        Mode::Indexed { base: Base::Pc, .. } | Mode::MemInd { base: Base::Pc, .. } => PCREL,
        Mode::Indexed { .. } | Mode::MemInd { .. } => DISP,
        Mode::Abs(_) => ABS,
        Mode::Imm(..) => IMM,
        _ => 0,
    }
}

/// Operation size.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Sz {
    B,
    W,
    L,
}

impl Sz {
    /// The `ss` field most instructions use: 00, 01, 10.
    pub fn bits(self) -> u16 {
        match self {
            Sz::B => 0,
            Sz::W => 1,
            Sz::L => 2,
        }
    }
}

/// One way to encode an operand.
#[derive(Clone, Debug, Default)]
pub struct Alt {
    /// The 6-bit mode/register field, where the operand has one.
    pub field: u16,
    /// Extension words.
    pub bytes: Vec<u8>,
    /// Offsets are relative to the start of `bytes`.
    pub fixups: Vec<Fixup>,
}

impl Alt {
    pub fn field(mode: u16, reg: u8) -> Alt {
        Alt {
            field: (mode << 3) | reg as u16,
            ..Alt::default()
        }
    }

    pub fn words(bytes: Vec<u8>) -> Alt {
        Alt {
            bytes,
            ..Alt::default()
        }
    }
}

/// Where an operand's mode/register field goes in the opcode word.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Place {
    /// Extension words only.
    None,
    /// Bits 5-0, where nearly every instruction keeps its operand.
    Low,
    /// Bits 11-6, `MOVE`'s destination, with register and mode swapped.
    MoveDst,
}

pub struct Part {
    pub alts: Vec<Alt>,
    pub place: Place,
}

impl Part {
    pub fn fixed(bytes: Vec<u8>) -> Part {
        Part::words(bytes, Vec::new())
    }

    /// Extension words with fixups, such as an immediate.
    pub fn words(bytes: Vec<u8>, fixups: Vec<Fixup>) -> Part {
        Part {
            alts: vec![Alt {
                field: 0,
                bytes,
                fixups,
            }],
            place: Place::None,
        }
    }

    pub fn low(alts: Vec<Alt>) -> Part {
        Part {
            alts,
            place: Place::Low,
        }
    }
}

/// Combines an opcode word with its operands' alternatives into variants,
/// smallest first.
pub fn build(opcode: u16, parts: Vec<Part>) -> Vec<Variant> {
    let mut choice = vec![0usize; parts.len()];
    let mut out = Vec::new();
    loop {
        let mut word = opcode;
        let mut bytes = Vec::new();
        let mut fixups = Vec::new();
        bytes.extend_from_slice(&[0, 0]);
        for (part, &c) in parts.iter().zip(&choice) {
            let alt = &part.alts[c];
            match part.place {
                Place::None => {}
                Place::Low => word |= alt.field & 0x3f,
                Place::MoveDst => {
                    let mode = (alt.field >> 3) & 7;
                    let reg = alt.field & 7;
                    word |= (reg << 9) | (mode << 6);
                }
            }
            let base = bytes.len() as u32;
            fixups.extend(alt.fixups.iter().map(|f| Fixup {
                offset: f.offset + base,
                ..f.clone()
            }));
            bytes.extend_from_slice(&alt.bytes);
        }
        bytes[..2].copy_from_slice(&word.to_be_bytes());
        out.push(Variant { bytes, fixups });

        // Next combination, odometer style.
        let mut i = 0;
        loop {
            if i == parts.len() {
                out.sort_by_key(|v| v.bytes.len());
                return out;
            }
            choice[i] += 1;
            if choice[i] < parts[i].alts.len() {
                break;
            }
            choice[i] = 0;
            i += 1;
        }
    }
}

/// What the encoder needs to know besides the operand.
#[derive(Copy, Clone)]
pub struct EaCtx {
    pub(crate) cpu: Cpu,
    /// Width of an immediate operand.
    pub size: Sz,
    /// Or, for an FPU operand, its floating-point size, which an immediate
    /// takes instead.
    pub(crate) float: Option<Float>,
}

fn fixup(offset: u32, e: ExprRef, kind: FixupKind, span: Span) -> Fixup {
    Fixup {
        offset,
        expr: e,
        kind,
        span,
    }
}

fn abs_kind(size: u8) -> FixupKind {
    FixupKind::data(size).with_reloc(reloc::data(size, false).unwrap_or(0))
}

/// A PC-relative field `back` bytes past the start of the operand's extension
/// words. The 68000 measures PC-relative operands from the first extension
/// word, not from the end of the instruction.
fn pc_kind(size: u8, back: i8) -> FixupKind {
    FixupKind::pcrel(size, -back).with_reloc(reloc::data(size, true).unwrap_or(0))
}

fn fits_i8(v: i64) -> bool {
    (-128..=127).contains(&v)
}

fn fits_i16(v: i64) -> bool {
    (-32768..=32767).contains(&v)
}

fn fits_32(v: i64) -> bool {
    (-(1i64 << 31)..(1i64 << 32)).contains(&v)
}

/// An address the CPU reaches by sign-extending 16 bits, which includes the
/// top of the address space written as an unsigned number: GNU as picks
/// `abs.W` for `$FFFF8000` as well as for `-2`.
pub fn fits_abs_w(v: i64) -> bool {
    fits_i16(v) || (0xffff_8000..=0xffff_ffff).contains(&v)
}

/// Writes an immediate of `size` bytes. A byte immediate still occupies a
/// word, with the value in the low byte; GNU as sign-extends a negative one
/// into the high byte, which the CPU ignores.
pub fn immediate(
    cx: &mut AsmCtx<'_>,
    e: ExprRef,
    size: Sz,
    span: Span,
) -> Option<(Vec<u8>, Vec<Fixup>)> {
    match cx.constant(e) {
        Some(v) => {
            let ok = match size {
                Sz::B => (-128..=255).contains(&v),
                Sz::W => (-32768..=65535).contains(&v),
                Sz::L => fits_32(v),
            };
            if !ok {
                cx.error(
                    span,
                    format!("immediate {v} does not fit in a {}", size_name(size)),
                );
                return None;
            }
            Some(match size {
                Sz::B | Sz::W => ((v as i16) as u16).to_be_bytes().to_vec(),
                Sz::L => (v as u32).to_be_bytes().to_vec(),
            })
            .map(|b| (b, Vec::new()))
        }
        None => Some(match size {
            // GNU as writes the whole addend as the word and relocates its
            // low byte, so the high byte of `#sym-2` is `ff`.
            Sz::B => {
                let high = ((addend(cx, e) as i16) >> 8) as u8;
                (vec![high, 0], vec![fixup(1, e, abs_kind(1), span)])
            }
            Sz::W => (vec![0, 0], vec![fixup(0, e, abs_kind(2), span)]),
            Sz::L => (vec![0; 4], vec![fixup(0, e, abs_kind(4), span)]),
        }),
    }
}

/// The number added to the symbols of an expression as written, `-2` in
/// `ext-2`, whether or not the symbols are defined yet.
fn addend(cx: &AsmCtx<'_>, e: ExprRef) -> i64 {
    use crate::expr::{BinOp, ExprKind, UnOp};
    match cx.exprs.get(e).kind {
        ExprKind::Int(n) => n as i64,
        ExprKind::Binary(BinOp::Add, a, b) => addend(cx, a).wrapping_add(addend(cx, b)),
        ExprKind::Binary(BinOp::Sub, a, b) => addend(cx, a).wrapping_sub(addend(cx, b)),
        ExprKind::Unary(UnOp::Neg, a) => addend(cx, a).wrapping_neg(),
        ExprKind::Unary(UnOp::Plus, a) => addend(cx, a),
        _ => 0,
    }
}

pub fn size_name(size: Sz) -> &'static str {
    match size {
        Sz::B => "byte",
        Sz::W => "word",
        Sz::L => "long",
    }
}

/// The 68020's addressing modes, which CPU32 and Fido also have and the
/// 68000, 68010 and ColdFire do not.
fn need_020(cx: &mut AsmCtx<'_>, cpu: Cpu, span: Span, what: &str) -> Option<()> {
    if cpu.wide() {
        return Some(());
    }
    cx.error(
        span,
        format!(
            "{what} needs a 68020 or later; this target is a {}",
            cpu.describe()
        ),
    );
    None
}

/// A base displacement or outer displacement, sized.
enum Field {
    Null,
    Word(Vec<u8>, Option<Fixup>),
    Long(Vec<u8>, Option<Fixup>),
}

impl Field {
    fn bd_bits(&self) -> u16 {
        match self {
            Field::Null => 1,
            Field::Word(..) => 2,
            Field::Long(..) => 3,
        }
    }
}

/// ColdFire's index register is always a long, and scaled by 8 only on the
/// cores with an FPU. An index written without a size is a long there in
/// Motorola source too, where it is otherwise a word.
fn coldfire_index(cx: &mut AsmCtx<'_>, ix: &mut Index, cpu: Cpu) -> Option<()> {
    if ix.scale == 8 && !cpu.has(super::table::feature::CFLOAT) {
        cx.error(
            ix.span,
            format!(
                "an index scaled by 8 needs a ColdFire with an FPU; this target is a {}",
                cpu.describe()
            ),
        );
        return None;
    }
    if ix.sized && !ix.long {
        cx.error(ix.span, "a ColdFire index register is a long");
        return None;
    }
    ix.long = true;
    Some(())
}

/// Encodes an effective address to its alternatives, smallest first.
pub fn ea(cx: &mut AsmCtx<'_>, op: &Operand, ecx: EaCtx) -> Option<Vec<Alt>> {
    let adjusted;
    let op = match &op.mode {
        Mode::Indexed {
            base,
            disp,
            index: Some(ix),
        } if ecx.cpu.coldfire() => {
            let mut ix = *ix;
            coldfire_index(cx, &mut ix, ecx.cpu)?;
            adjusted = Operand {
                mode: Mode::Indexed {
                    base: *base,
                    disp: *disp,
                    index: Some(ix),
                },
                span: op.span,
                brace: Vec::new(),
            };
            &adjusted
        }
        _ => op,
    };
    let span = op.span;
    Some(match &op.mode {
        Mode::DReg(n) => vec![Alt::field(0, *n)],
        Mode::AReg(n) => vec![Alt::field(1, *n)],
        Mode::Ind(n) => vec![Alt::field(2, *n)],
        Mode::PostInc(n) => vec![Alt::field(3, *n)],
        Mode::PreDec(n) => vec![Alt::field(4, *n)],
        Mode::Imm(e, s) if ecx.float.is_some() => {
            // An integer where a float is wanted is its own bit pattern,
            // zero-extended, as GNU as writes it.
            let len = ecx.float.map_or(4, Float::bytes_len);
            let Some(v) = cx.constant(*e) else {
                cx.error(
                    *s,
                    "a floating-point immediate must be a number, known where it is written",
                );
                return None;
            };
            let wide = (v as u64 as u128).to_be_bytes();
            vec![Alt {
                field: 0o74,
                bytes: wide[16 - len..].to_vec(),
                fixups: vec![],
            }]
        }
        Mode::Imm(e, s) => {
            let (bytes, fixups) = immediate(cx, *e, ecx.size, *s)?;
            vec![Alt {
                field: 0o74,
                bytes,
                fixups,
            }]
        }
        Mode::FImm(v, s) => {
            let Some(kind) = ecx.float else {
                cx.error(
                    *s,
                    "a floating-point immediate needs a floating-point size: `.s`, `.d`, `.x` or `.p`",
                );
                return None;
            };
            vec![Alt {
                field: 0o74,
                bytes: kind.bytes(*v),
                fixups: vec![],
            }]
        }
        Mode::Abs(v) => vec![absolute(cx, v)?],
        Mode::Indexed { base, disp, index } => indexed(cx, *base, disp, index, ecx, span)?,
        Mode::MemInd {
            base,
            bd,
            index,
            od,
        } => {
            need_020(cx, ecx.cpu, span, "memory-indirect addressing")?;
            if ecx.cpu.has(super::table::feature::CPU32) {
                cx.error(span, "memory-indirect addressing is not available on CPU32");
                return None;
            }
            vec![mem_indirect(cx, *base, bd, index, od, span)?]
        }
        _ => {
            cx.error(
                span,
                format!("{} is not an effective address", op.describe()),
            );
            return None;
        }
    })
}

fn absolute(cx: &mut AsmCtx<'_>, v: &Value) -> Option<Alt> {
    // There is no byte-sized absolute address: the two modes are `abs.W` and
    // `abs.L`, and GNU as calls the size unsupported rather than widening it
    // the way a displacement widens.
    if v.width == Some(Width::B) {
        cx.error(
            v.span,
            "an absolute address has no byte form; write `:w` or `:l`",
        );
        return None;
    }
    match cx.constant(v.e) {
        Some(n) => {
            let short = match v.width {
                Some(Width::W) => {
                    if !fits_abs_w(n) {
                        cx.error(
                            v.span,
                            format!("address {n:#x} cannot be reached with `abs.W`"),
                        );
                        return None;
                    }
                    true
                }
                // A byte is already refused above.
                Some(Width::L | Width::B) => false,
                None => fits_abs_w(n),
            };
            if !short && !fits_32(n) {
                cx.error(v.span, format!("address {n:#x} does not fit in 32 bits"));
                return None;
            }
            Some(if short {
                Alt {
                    field: 0o70,
                    bytes: ((n as i16) as u16).to_be_bytes().to_vec(),
                    fixups: vec![],
                }
            } else {
                Alt {
                    field: 0o71,
                    bytes: (n as u32).to_be_bytes().to_vec(),
                    fixups: vec![],
                }
            })
        }
        None => Some(if v.width == Some(Width::W) {
            Alt {
                field: 0o70,
                bytes: vec![0; 2],
                fixups: vec![fixup(0, v.e, abs_kind(2), v.span)],
            }
        } else {
            Alt {
                field: 0o71,
                bytes: vec![0; 4],
                fixups: vec![fixup(0, v.e, abs_kind(4), v.span)],
            }
        }),
    }
}

fn index_bits(ix: &Index) -> u16 {
    let scale = match ix.scale {
        1 => 0,
        2 => 1,
        4 => 2,
        _ => 3,
    };
    ((ix.reg as u16) << 12) | ((ix.long as u16) << 11) | (scale << 9)
}

/// A full-format extension word and what follows it.
fn full(index: Option<&Index>, suppress_base: bool, bd: Field, iis: u16, od: Field) -> Alt {
    let mut ext = 0x0100 | bd.bd_bits() << 4 | iis;
    match index {
        Some(ix) => ext |= index_bits(ix),
        None => ext |= 0x40,
    }
    if suppress_base {
        ext |= 0x80;
    }
    let mut alt = Alt::words(ext.to_be_bytes().to_vec());
    for f in [bd, od] {
        match f {
            Field::Null => {}
            Field::Word(b, fx) | Field::Long(b, fx) => {
                let at = alt.bytes.len() as u32;
                if let Some(mut fx) = fx {
                    fx.offset += at;
                    // A PC-relative field is measured from the extension
                    // word, which is `at` bytes before it.
                    if fx.kind.pcrel {
                        fx.kind.adjust = -(at as i8);
                    }
                    alt.fixups.push(fx);
                }
                alt.bytes.extend_from_slice(&b);
            }
        }
    }
    alt
}

/// A displacement field of known width from a value: its bytes, plus a
/// fixup when the value is not a constant. `pc` makes it PC-relative.
fn disp_field(cx: &mut AsmCtx<'_>, v: &Value, long: bool, pc: bool) -> Field {
    let size = if long { 4 } else { 2 };
    let (bytes, fx) = match (pc, cx.constant(v.e)) {
        (false, Some(n)) => {
            let b = if long {
                (n as u32).to_be_bytes().to_vec()
            } else {
                ((n as i16) as u16).to_be_bytes().to_vec()
            };
            (b, None)
        }
        (false, None) => {
            let mut k = abs_kind(size);
            k.signed = !long;
            (vec![0; size as usize], Some(fixup(0, v.e, k, v.span)))
        }
        (true, _) => (
            vec![0; size as usize],
            Some(fixup(0, v.e, pc_kind(size, 0), v.span)),
        ),
    };
    if long {
        Field::Long(bytes, fx)
    } else {
        Field::Word(bytes, fx)
    }
}

fn indexed(
    cx: &mut AsmCtx<'_>,
    base: Base,
    disp: &Option<Value>,
    index: &Option<Index>,
    ecx: EaCtx,
    span: Span,
) -> Option<Vec<Alt>> {
    let cpu = ecx.cpu;
    if let Some(ix) = index
        && ix.scale != 1
        && !cpu.scales()
    {
        need_020(cx, cpu, ix.span, "a scaled index")?;
    }
    let reg = match base {
        Base::A(n) => n,
        _ => 0,
    };

    match base {
        Base::None => {
            need_020(cx, cpu, span, "an operand with no base register")?;
            let bd = match disp {
                None => Field::Null,
                Some(v) => sized_disp(cx, v, index.is_some())?,
            };
            let mut alt = full(index.as_ref(), true, bd, 0, Field::Null);
            alt.field = 0o60;
            Some(vec![alt])
        }
        Base::A(_) => {
            let Some(v) = disp else {
                return Some(vec![match index {
                    None => Alt::field(2, reg),
                    Some(ix) => brief(0o60 | reg as u16, ix, 0),
                }]);
            };
            // A byte displacement lives in the low eight bits of a brief
            // extension word, and so needs the index register that shares
            // that word; see `no_brief_word`.
            if v.width == Some(Width::B)
                && let Some(ix) = index
            {
                return Some(vec![byte_brief(cx, 0o60 | reg as u16, ix, v)]);
            }
            let constant = cx.constant(v.e);
            // Forms in order of preference, each allowed only if the value
            // fits and the CPU has it. A byte reaching here has no index
            // register, so it is the word `d(An)` already is.
            let long = match (v.width, constant) {
                (Some(Width::L), _) => true,
                (Some(Width::W | Width::B), Some(n)) => {
                    if !fits_i16(n) {
                        cx.error(v.span, format!("displacement {n} does not fit in a word"));
                        return None;
                    }
                    false
                }
                (Some(Width::W | Width::B), None) => false,
                (None, Some(n)) => match index {
                    None if n == 0 => return Some(vec![Alt::field(2, reg)]),
                    Some(ix) if fits_i8(n) => {
                        return Some(vec![brief(0o60 | reg as u16, ix, n)]);
                    }
                    None if fits_i16(n) => {
                        return Some(vec![word_disp(reg, n)]);
                    }
                    _ => {
                        if !cpu.wide() {
                            let what = if index.is_some() {
                                "an 8-bit"
                            } else {
                                "a 16-bit"
                            };
                            cx.error(
                                v.span,
                                format!(
                                    "displacement {n} does not fit in {what} field; \
                                     wider displacements need a 68020 or later"
                                ),
                            );
                            return None;
                        }
                        !fits_i16(n)
                    }
                },
                // Not known yet. GNU as settles this without looking at the
                // eventual value; see the module comment.
                (None, None) => {
                    if !cpu.wide() {
                        return Some(vec![match index {
                            None => {
                                let mut k = abs_kind(2);
                                k.signed = true;
                                Alt {
                                    field: 0o50 | reg as u16,
                                    bytes: vec![0, 0],
                                    fixups: vec![fixup(0, v.e, k, v.span)],
                                }
                            }
                            Some(ix) => {
                                let mut alt = brief(0o60 | reg as u16, ix, 0);
                                let mut k = abs_kind(1);
                                k.signed = true;
                                alt.fixups.push(fixup(1, v.e, k, v.span));
                                alt
                            }
                        }]);
                    }
                    // Without an index it is 32 bits in both of GNU as's
                    // modes. With one it is 32 bits natively but 16 under
                    // `--mri`, so the Motorola dialect takes the latter.
                    index.is_none() || cx.dialect == Dialect::Gas
                }
            };
            if !long && index.is_none() {
                // An explicit `.w`: the ordinary 16-bit displacement mode.
                if let Field::Word(bytes, fx) | Field::Long(bytes, fx) =
                    disp_field(cx, v, false, false)
                {
                    return Some(vec![Alt {
                        field: 0o50 | reg as u16,
                        bytes,
                        fixups: fx.into_iter().collect(),
                    }]);
                }
            }
            need_020(cx, cpu, v.span, "a displacement this wide")?;
            let bd = disp_field(cx, v, long, false);
            let mut alt = full(index.as_ref(), false, bd, 0, Field::Null);
            alt.field = 0o60 | reg as u16;
            Some(vec![alt])
        }
        Base::Pc => pc_relative(cx, disp, index, cpu, span),
    }
}

/// A 16-bit displacement or a 32-bit base displacement, for the no-base form.
fn sized_disp(cx: &mut AsmCtx<'_>, v: &Value, indexed: bool) -> Option<Field> {
    no_brief_word(cx, v);
    let long = match (v.width, cx.constant(v.e)) {
        (Some(w), _) => w == Width::L,
        (None, Some(n)) => !fits_i16(n),
        (None, None) => !indexed,
    };
    Some(disp_field(cx, v, long, false))
}

fn word_disp(reg: u8, n: i64) -> Alt {
    Alt {
        field: 0o50 | reg as u16,
        bytes: ((n as i16) as u16).to_be_bytes().to_vec(),
        fixups: vec![],
    }
}

/// A brief extension word: index register, size, scale, and an 8-bit
/// displacement.
fn brief(field: u16, ix: &Index, disp: i64) -> Alt {
    let ext = index_bits(ix) | (disp as u8 as u16);
    Alt {
        field,
        bytes: ext.to_be_bytes().to_vec(),
        fixups: vec![],
    }
}

/// A brief extension word whose displacement was written `:b`, whether or not
/// the value is known yet.
///
/// The size was asked for, so nothing wider is considered and a constant that
/// does not fit is truncated rather than refused: GNU as warns and carries on,
/// and its `isbyte` takes the whole of -255 to 255, which is every value a
/// byte can be read as signed or unsigned.
fn byte_brief(cx: &mut AsmCtx<'_>, field: u16, ix: &Index, v: &Value) -> Alt {
    let Some(n) = cx.constant(v.e) else {
        let mut alt = brief(field, ix, 0);
        let mut k = abs_kind(1);
        k.signed = true;
        alt.fixups.push(fixup(1, v.e, k, v.span));
        return alt;
    };
    if !(-255..=255).contains(&n) {
        cx.diags
            .warning(v.span, format!("displacement {n} does not fit in a byte"));
    }
    brief(field, ix, n)
}

/// GNU as's `:b not permitted; defaulting to :w`.
///
/// The only field a byte displacement fits in is the low eight bits of a brief
/// extension word, and that word holds nothing else: no wide displacement, no
/// outer displacement and no suppressed base register. An operand that needs a
/// full extension word for any of those reasons has nowhere to put a byte, so
/// GNU as warns and assembles a word, which leaves the program the source
/// asked for apart from the width of one field.
///
/// A plain `d(An)` or `d(PC)` is the one place it says nothing: the word it
/// would widen to is the mode that operand already has.
fn no_brief_word(cx: &mut AsmCtx<'_>, v: &Value) {
    if v.width == Some(Width::B) {
        cx.diags.warning(
            v.span,
            "this operand needs a full extension word, which has no byte displacement; \
             `:b` is assembled as `:w`",
        );
    }
}

/// Whether `disp(pc)` names the address to reach or the displacement itself.
///
/// In Motorola syntax it is always the address: `label(pc)` reaches `label`,
/// and so `8(pc)` reaches address 8 (vasm agrees). GNU as reads a *constant*
/// as the raw displacement — `%pc@(8)` is eight bytes on — and a symbolic one
/// as the address.
fn pc_is_target(cx: &AsmCtx<'_>, v: &Value) -> bool {
    cx.dialect != Dialect::Gas || cx.constant(v.e).is_none()
}

fn pc_relative(
    cx: &mut AsmCtx<'_>,
    disp: &Option<Value>,
    index: &Option<Index>,
    cpu: Cpu,
    span: Span,
) -> Option<Vec<Alt>> {
    let Some(v) = disp else {
        // `(pc)` or `%pc@`: displacement zero.
        return Some(vec![match index {
            None => Alt {
                field: 0o72,
                bytes: vec![0, 0],
                fixups: vec![],
            },
            Some(ix) => brief(0o73, ix, 0),
        }]);
    };

    if !pc_is_target(cx, v) {
        let n = cx.constant(v.e).unwrap_or(0);
        return match index {
            // A byte displacement is the brief extension word's, and with no
            // index register there is no brief extension word; see
            // `no_brief_word`.
            Some(ix) if v.width == Some(Width::B) => Some(vec![byte_brief(cx, 0o73, ix, v)]),
            None if fits_i16(n) && v.width != Some(Width::L) => Some(vec![Alt {
                field: 0o72,
                ..word_disp(0, n)
            }]),
            Some(ix) if fits_i8(n) && v.width.is_none() => Some(vec![brief(0o73, ix, n)]),
            _ => {
                need_020(cx, cpu, span, "a PC displacement this wide")?;
                let long = v.width == Some(Width::L) || !fits_i16(n);
                let bd = disp_field(cx, v, long, false);
                let mut alt = full(index.as_ref(), false, bd, 0, Field::Null);
                alt.field = 0o73;
                Some(vec![alt])
            }
        };
    }

    let word = || Alt {
        field: 0o72,
        bytes: vec![0, 0],
        fixups: vec![fixup(0, v.e, pc_kind(2, 0), v.span)],
    };
    let brief_pc = |ix: &Index| {
        let mut alt = brief(0o73, ix, 0);
        alt.fixups.push(fixup(1, v.e, pc_kind(1, 1), v.span));
        alt
    };
    let wide = |cx: &mut AsmCtx<'_>, long: bool| {
        let bd = disp_field(cx, v, long, true);
        let mut alt = full(index.as_ref(), false, bd, 0, Field::Null);
        alt.field = 0o73;
        alt
    };

    let mut alts = Vec::new();
    match (index, v.width) {
        // A byte displacement is the brief extension word's alone, and with an
        // index register that word is already what `d(PC,Xn)` uses; with none,
        // it widens to the word `d(PC)` has; see `no_brief_word`.
        (Some(ix), Some(Width::B)) => alts.push(brief_pc(ix)),
        (None, Some(Width::W | Width::B)) => alts.push(word()),
        (Some(_), Some(Width::W)) => {
            need_020(cx, cpu, span, "a 16-bit indexed PC displacement")?;
            alts.push(wide(cx, false));
        }
        (_, Some(Width::L)) => {
            need_020(cx, cpu, span, "a 32-bit PC displacement")?;
            alts.push(wide(cx, true));
        }
        // A suffix on a PC-relative operand and no size to go with it is the
        // one place GNU as picks a width without weighing the alternatives:
        // the 32-bit base displacement, whether or not the symbol is one it
        // could reach from here, so that `move.l near@PLT(%pc),%d0` is the
        // same instruction wherever `near` is.
        (_, None) if cpu.wide() && cx.find_modifier_for(v.e).is_some() => {
            alts.push(wide(cx, true));
        }
        (None, None) => {
            alts.push(word());
            if cpu.wide() {
                alts.push(wide(cx, true));
            }
        }
        (Some(ix), None) => {
            alts.push(brief_pc(ix));
            if cpu.wide() {
                alts.push(wide(cx, false));
                alts.push(wide(cx, true));
            }
        }
    }
    Some(alts)
}

fn mem_indirect(
    cx: &mut AsmCtx<'_>,
    base: Base,
    bd: &Option<Value>,
    index: &Option<(Index, IndexAt)>,
    od: &Option<Value>,
    _span: Span,
) -> Option<Alt> {
    let pc = base == Base::Pc;
    // GNU as sizes both displacements alike: a value not known yet is 32
    // bits, a constant the shortest of null, word and long that holds it.
    let field = |cx: &mut AsmCtx<'_>, v: &Option<Value>, pc: bool| {
        let Some(v) = v else {
            return Field::Null;
        };
        no_brief_word(cx, v);
        let target = pc && pc_is_target(cx, v);
        let long = match (v.width, cx.constant(v.e)) {
            (Some(w), _) => w == Width::L,
            (None, _) if target => true,
            (None, Some(0)) => return Field::Null,
            (None, Some(n)) => !fits_i16(n),
            (None, None) => true,
        };
        disp_field(cx, v, long, target)
    };
    let bd_field = field(cx, bd, pc);
    let od_field = field(cx, od, false);
    let od_bits = od_field.bd_bits();
    let iis = match index {
        Some((_, IndexAt::Post)) => 4 | od_bits,
        _ => od_bits,
    };
    let mut alt = full(
        index.as_ref().map(|(i, _)| i),
        base == Base::None,
        bd_field,
        iis,
        od_field,
    );
    alt.field = match base {
        Base::A(n) => 0o60 | n as u16,
        Base::None => 0o60,
        Base::Pc => 0o73,
    };
    Some(alt)
}
