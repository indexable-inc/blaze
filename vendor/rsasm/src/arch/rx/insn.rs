//! The instruction set: RXv1, as GNU as 2.47 assembles it for its default CPU.
//!
//! Each arm below corresponds to a production in binutils'
//! `gas/config/rx-parse.y`, and every encoding was checked byte for byte
//! against `rx-elf-as`. Where GNU as's grammar and the RX manual disagree,
//! this follows GNU as, since its output is what the corpus records.

use super::branch::{self, Kind, Width};
use super::encode::{self, Enc, Place, Range, Rung};
use super::operand::{self, OpKind, Operand};
use super::reg::{self, MemEx, Size};
use super::reloc;
use crate::arch::{AsmCtx, InsnRequest};
use crate::expr::ExprRef;
use crate::section::Variant;
use crate::source::Span;

type Out = Option<Vec<Variant>>;

/// What a statement looks like once split up.
struct Stmt<'a> {
    /// The mnemonic without its size suffix.
    m: &'a str,
    /// The size suffix, with its dot (`.l`), if any.
    suffix: Option<&'a str>,
    ops: Vec<Operand>,
    span: Span,
}

impl Stmt<'_> {
    fn kinds(&self) -> Vec<OpKind> {
        self.ops.iter().map(|o| o.kind).collect()
    }

    fn span_of(&self, i: usize) -> Span {
        self.ops.get(i).map_or(self.span, |o| o.span)
    }

    fn name(&self) -> String {
        format!("{}{}", self.m, self.suffix.unwrap_or(""))
    }
}

pub fn assemble(cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Out {
    let full = cx.name(req.mnemonic).to_ascii_lowercase();
    let (m, suffix) = match full.find('.') {
        Some(i) => (&full[..i], Some(&full[i..])),
        None => (full.as_str(), None),
    };
    let ops = operand::parse_all(cx, req.operands, req.span)?;
    let s = Stmt {
        m,
        suffix,
        ops,
        span: req.span,
    };
    let out = dispatch(cx, &s)?;
    if s.ops.iter().any(|o| o.width.is_some()) {
        check_bit_lengths(cx, &s, &out)?;
    }
    Some(out)
}

/// Checks CC-RX's bit length specifiers, `#imm:8` and `dsp:16[r1]`.
///
/// CC-RX uses a field of the width a specifier names even where a shorter
/// form fits (R20UT3248EJ0115 §5.1.5 (3), page 460). rsasm always takes the
/// shortest form, as GNU as does, so a specifier is accepted where it names
/// the width that form has anyway, and refused where CC-RX would assemble
/// something else.
///
/// Which field an operand went into is not something the encoders report, so
/// it is found by probing: the instruction is assembled again with the value
/// replaced by one that needs exactly the named width, and by one that needs
/// the next width up. The specifier is the shortest form's when the real
/// value gives the same length as the first and the second is longer or does
/// not assemble at all. The fixed fields of 1 to 5 bits (bit numbers, shift
/// counts, `#uimm:4`) have no shorter alternative, so there the value only
/// has to fit.
fn check_bit_lengths(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, out: &[Variant]) -> Option<()> {
    for (i, op) in s.ops.iter().enumerate() {
        let Some((width, wspan)) = op.width else {
            continue;
        };
        let (e, disp) = match op.kind {
            OpKind::Imm(e) => (e, false),
            OpKind::Mem { disp: Some(e), .. } => (e, true),
            _ => {
                cx.error(
                    wspan,
                    "a bit length specifier goes on an immediate or a displacement",
                );
                return None;
            }
        };
        let refuse = |cx: &mut AsmCtx<'_>, why: String| {
            cx.error(
                wspan,
                format!(
                    "`:{width}` {why}; rsasm assembles the shortest form, as GNU as does, \
                     so it only accepts the specifier that form has"
                ),
            );
            None
        };
        let Some(v) = encode::known(cx, e) else {
            // A symbol is a 32-bit immediate with a relocation: the widest
            // candidate, which is the one layout takes for it. A difference
            // of labels is sized by its value, which is not known yet.
            let symbol = matches!(encode::classify(cx, e), encode::Val::Sym(_));
            let len = match out.last() {
                Some(widest) if symbol && !disp && width == 32 => Some(widest.bytes.len()),
                _ => None,
            };
            if len.is_some() && len == probe_len(cx, s, i, 0x1000_0000) {
                continue;
            }
            return refuse(
                cx,
                "cannot be checked on a value that is not a constant".into(),
            );
        };
        if !disp && width <= 5 {
            let (lo, hi) = if width == 1 {
                (1, 2)
            } else {
                (0, (1 << width) - 1)
            };
            if !(lo..=hi).contains(&v) {
                cx.error(
                    wspan,
                    format!("{v} does not fit a `:{width}` immediate ({lo} to {hi})"),
                );
                return None;
            }
            if width != 4 {
                continue;
            }
        }
        // Values that need exactly each width, of the sign of `v`. `#uimm:4`
        // is probed too, since most instructions have no such form and take
        // a small value as `#simm:8`.
        let ladder: &[(u8, i64)] = match (disp, v < 0) {
            (true, _) => &[(5, 4), (8, 128), (16, 1024)],
            (false, false) => &[
                (4, 10),
                (8, 100),
                (16, 1000),
                (24, 100_000),
                (32, 0x1000_0000),
            ],
            (false, true) => &[(8, -100), (16, -1000), (24, -100_000), (32, -0x1000_0000)],
        };
        let Some(at) = ladder.iter().position(|&(w, _)| w == width) else {
            let what = if disp {
                "is not a displacement width (`:5`, `:8` or `:16`)"
            } else {
                "is not a width for this value"
            };
            return refuse(cx, what.into());
        };
        let len = match out {
            [one] => Some(one.bytes.len()),
            _ => None,
        };
        let named = probe_len(cx, s, i, ladder[at].1);
        let next = match ladder.get(at + 1) {
            Some(&(_, rep)) => probe_len(cx, s, i, rep),
            None => None,
        };
        let shortest = len.is_some() && len == named && next.is_none_or(|n| Some(n) > named);
        if !shortest {
            return refuse(cx, format!("is not the width of the shortest form for {v}"));
        }
    }
    Some(())
}

/// The length of `s` assembled with the value of operand `i` replaced by
/// `value`, or `None` if that does not assemble to one fixed encoding. The
/// probe's diagnostics are discarded.
fn probe_len(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, i: usize, value: i64) -> Option<usize> {
    let e = cx.exprs.int(value as u64, s.span);
    let mut ops = s.ops.clone();
    ops[i].kind = match ops[i].kind {
        OpKind::Mem { base, ext, .. } => OpKind::Mem {
            disp: Some(e),
            base,
            ext,
        },
        _ => OpKind::Imm(e),
    };
    let probe = Stmt {
        m: s.m,
        suffix: s.suffix,
        ops,
        span: s.span,
    };
    let saved = cx.diags.take();
    let relaxable = cx.relaxable;
    let out = dispatch(cx, &probe);
    cx.relaxable = relaxable;
    cx.diags.take();
    for d in saved {
        cx.diags.emit(d);
    }
    match out.as_deref() {
        Some([one]) => Some(one.bytes.len()),
        _ => None,
    }
}

fn dispatch(cx: &mut AsmCtx<'_>, s: &Stmt<'_>) -> Out {
    match s.m {
        "brk" => fixed(cx, s, &[0x00]),
        "dbt" => fixed(cx, s, &[0x01]),
        "rts" => fixed(cx, s, &[0x02]),
        "nop" => fixed(cx, s, &[0x03]),
        "rtfi" => fixed(cx, s, &[0x7f, 0x94]),
        "rte" => fixed(cx, s, &[0x7f, 0x95]),
        "wait" => fixed(cx, s, &[0x7f, 0x96]),
        "satr" => fixed(cx, s, &[0x7f, 0x93]),
        "scmpu" => fixed(cx, s, &[0x7f, 0x83]),
        "smovu" => fixed(cx, s, &[0x7f, 0x87]),
        "smovb" => fixed(cx, s, &[0x7f, 0x8b]),
        "smovf" => fixed(cx, s, &[0x7f, 0x8f]),
        "suntil" => string_op(cx, s, 0x80),
        "swhile" => string_op(cx, s, 0x84),
        "sstr" => string_op(cx, s, 0x88),
        "rmpa" => string_op(cx, s, 0x8c),

        "mov" => mov(cx, s),
        "movu" => movu(cx, s),
        "push" => push(cx, s),
        "pop" => one_reg(cx, s, 0x7e, 0xb0),
        "pushc" | "popc" => pushc(cx, s),
        "pushm" | "popm" => pushm(cx, s),
        "rtsd" => rtsd(cx, s),
        "xchg" => xchg_like(cx, s, 16),
        "itof" => xchg_like(cx, s, 17),
        "utof" => xchg_like(cx, s, 21),

        "sub" => subadd(cx, s, 0),
        "add" => subadd(cx, s, 2),
        "mul" => subadd(cx, s, 3),
        "and" => subadd(cx, s, 4),
        "or" => subadd(cx, s, 5),
        "cmp" => cmp(cx, s),
        "sbb" => adc_sbb(cx, s, 0),
        "adc" => adc_sbb(cx, s, 2),
        "neg" => unary(cx, s, 1, 1),
        "abs" => unary(cx, s, 3, 2),
        "not" => unary(cx, s, 14, 0),
        "max" => dp20(cx, s, 4),
        "min" => dp20(cx, s, 5),
        "emul" => dp20(cx, s, 6),
        "emulu" => dp20(cx, s, 7),
        "div" => dp20(cx, s, 8),
        "divu" => dp20(cx, s, 9),
        "tst" => dp20(cx, s, 12),
        "xor" => dp20(cx, s, 13),
        "stz" => store_cond(cx, s, 14),
        "stnz" => store_cond(cx, s, 15),

        "shlr" => shift(cx, s, 0),
        "shar" => shift(cx, s, 1),
        "shll" => shift(cx, s, 2),
        "rotr" => rotate(cx, s, 4),
        "rotl" => rotate(cx, s, 6),
        "revw" => two_reg_fd(cx, s, 0x65),
        "revl" => two_reg_fd(cx, s, 0x67),
        "rorc" => one_reg(cx, s, 0x7e, 0x40),
        "rolc" => one_reg(cx, s, 0x7e, 0x50),
        "sat" => one_reg(cx, s, 0x7e, 0x30),

        "bset" => bit_op(cx, s, 0),
        "bclr" => bit_op(cx, s, 1),
        "btst" => bit_op(cx, s, 2),
        "bnot" => bit_op(cx, s, 3),

        "bra" => bra_bsr(cx, s, Kind::Bra, 0x40),
        "bsr" => bra_bsr(cx, s, Kind::Bsr, 0x50),
        "jmp" => one_reg(cx, s, 0x7f, 0x00),
        "jsr" => one_reg(cx, s, 0x7f, 0x10),
        "int" => int(cx, s),
        "mvtipl" => mvtipl(cx, s),
        "setpsw" => psw_flag(cx, s, 0xa0),
        "clrpsw" => psw_flag(cx, s, 0xb0),
        "mvtc" => mvtc(cx, s),
        "mvfc" => mvfc(cx, s),

        "fsub" => float(cx, s, 0, true),
        "fcmp" => float(cx, s, 1, true),
        "fadd" => float(cx, s, 2, true),
        "fmul" => float(cx, s, 3, true),
        "fdiv" => float(cx, s, 4, true),
        "ftoi" => float(cx, s, 5, false),
        "round" => float(cx, s, 6, false),
        "fsqrt" => float(cx, s, 8, false),
        "ftou" => float(cx, s, 9, false),

        "mulhi" => two_reg_fd(cx, s, 0x00),
        "mullo" => two_reg_fd(cx, s, 0x01),
        "machi" => two_reg_fd(cx, s, 0x04),
        "maclo" => two_reg_fd(cx, s, 0x05),
        "mvtachi" => acc_reg(cx, s, 0x17, 0x00),
        "mvtaclo" => acc_reg(cx, s, 0x17, 0x10),
        "mvfachi" => acc_reg(cx, s, 0x1f, 0x00),
        "mvfaclo" => acc_reg(cx, s, 0x1f, 0x10),
        "mvfacmi" => acc_reg(cx, s, 0x1f, 0x20),
        "racw" => racw(cx, s),

        m => {
            // The conditional families are a prefix plus a condition name,
            // which GNU as tries before its opcode table.
            if let Some(c) = m.strip_prefix('b').and_then(reg::cond) {
                return bcnd(cx, s, c);
            }
            if let Some(c) = m.strip_prefix("bm").and_then(reg::cond) {
                return bmcnd(cx, s, c);
            }
            if let Some(c) = m.strip_prefix("sc").and_then(reg::cond) {
                return sccnd(cx, s, c);
            }
            if V2_ONLY.contains(&m) {
                cx.error(
                    s.span,
                    format!("`{m}` is an RXv2 or RXv3 instruction; this target is RXv1"),
                );
                return None;
            }
            cx.error(s.span, format!("unknown instruction `{}`", s.name()));
            None
        }
    }
}

/// Mnemonics GNU as knows but refuses for its default RX600 CPU.
const V2_ONLY: &[&str] = &[
    "bfmov", "bfmovz", "emaca", "emsba", "emula", "maclh", "movco", "movli", "msbhi", "msblh",
    "msblo", "mullh", "mvfacgu", "mvtacgu", "racl", "rdacl", "rdacw", "rstr", "save", "dabs",
    "dadd", "dcmp", "ddiv", "dmov", "dmul", "dneg", "dpopm", "dpushm", "dround", "dsqrt", "dsub",
    "dtof", "dtoi", "dtou", "mvfdc", "mvfdr", "mvtdc", "ftod", "itod", "utod",
];

// ---- diagnostics and operand helpers ----------------------------------------

fn bad_operands(cx: &mut AsmCtx<'_>, s: &Stmt<'_>) -> Out {
    let what = if s.ops.is_empty() {
        "no operands".to_string()
    } else {
        s.ops
            .iter()
            .map(|o| o.describe())
            .collect::<Vec<_>>()
            .join(", ")
    };
    cx.error(
        s.span,
        format!("invalid operands for `{}`: {what}", s.name()),
    );
    None
}

fn no_suffix(cx: &mut AsmCtx<'_>, s: &Stmt<'_>) -> Option<()> {
    match s.suffix {
        None => Some(()),
        Some(x) => {
            cx.error(
                s.span,
                format!(
                    "`{}` does not take a size suffix; `{x}` is not allowed",
                    s.m
                ),
            );
            None
        }
    }
}

/// `.b`, `.w` or `.l`, defaulting to `.l`.
fn bwl(cx: &mut AsmCtx<'_>, s: &Stmt<'_>) -> Option<Size> {
    match s.suffix {
        None | Some(".l") => Some(Size::L),
        Some(".w") => Some(Size::W),
        Some(".b") => Some(Size::B),
        Some(x) => {
            cx.error(
                s.span,
                format!("`{}` takes `.b`, `.w` or `.l`, not `{x}`", s.m),
            );
            None
        }
    }
}

/// A memory operand whose size is given by the mnemonic, so it must not have
/// a suffix of its own.
fn plain_mem(cx: &mut AsmCtx<'_>, o: &Operand) -> Option<(Option<ExprRef>, u8)> {
    match o.kind {
        OpKind::Mem {
            disp,
            base,
            ext: None,
        } => Some((disp, base)),
        OpKind::Mem { ext: Some(_), .. } => {
            cx.error(
                o.span,
                "a size suffix is not allowed on this operand; put it on the mnemonic",
            );
            None
        }
        _ => None,
    }
}

fn fixed(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, bytes: &[u8]) -> Out {
    no_suffix(cx, s)?;
    if !s.ops.is_empty() {
        return bad_operands(cx, s);
    }
    Enc::new(bytes).one()
}

/// `op1, op2|reg`: a register in the low nibble of the second byte.
fn one_reg(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, op1: u8, op2: u8) -> Out {
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [OpKind::Reg(r)] => Enc::new(&[op1, op2 | r]).one(),
        _ => bad_operands(cx, s),
    }
}

/// `fd op src<<4|dst`.
fn two_reg_fd(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, op: u8) -> Out {
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [OpKind::Reg(a), OpKind::Reg(b)] => Enc::new(&[0xfd, op, a << 4 | b]).one(),
        _ => bad_operands(cx, s),
    }
}

fn string_op(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, op: u8) -> Out {
    let size = bwl(cx, s)?;
    if !s.ops.is_empty() {
        return bad_operands(cx, s);
    }
    Enc::new(&[0x7f, op | size as u8]).one()
}

/// A bit-field value that must be a constant in `0..2^bits`.
fn uconst(cx: &mut AsmCtx<'_>, e: ExprRef, span: Span, what: &str, bits: u32) -> Option<u32> {
    encode::constant_in(cx, e, span, what, 0, (1 << bits) - 1).map(|v| v as u32)
}

// ---- transfer -----------------------------------------------------------------

fn mov(cx: &mut AsmCtx<'_>, s: &Stmt<'_>) -> Out {
    use OpKind::*;
    let sz = bwl(cx, s)?;
    let z = sz.code() as u8;
    let k = s.kinds();
    match k[..] {
        [Imm(e), Reg(r)] => {
            if sz != Size::L {
                cx.error(
                    s.span,
                    format!(
                        "`{}` cannot load a register from an immediate; registers are \
                         always loaded whole, with `mov` or `mov.l`",
                        s.name()
                    ),
                );
                return None;
            }
            let rungs = vec![
                Rung::new(Enc::new(&[0x66, r]), Place::Nibble(8)),
                uimm8_rung(Enc::new(&[0x75, 0x40 | r])).const_only(),
                Rung::new(Enc::new(&[0xfb, r << 4 | 0x02]), imm(12)),
            ];
            encode::immediate(cx, rungs, e, s.span_of(0))
        }
        [Imm(e), Mem { .. }] => {
            if s.suffix.is_none() {
                cx.error(
                    s.span,
                    "`mov` of an immediate to memory needs a size: `mov.b`, `mov.w` or `mov.l`",
                );
                return None;
            }
            let (disp, base) = plain_mem(cx, &s.ops[1])?;
            let mut rungs = Vec::new();
            // The short form: a register among r0-r7, a written displacement of
            // at most 31 units, and an unsigned 8-bit immediate. It stores a
            // byte even for `.w` and `.l`, zero-extended.
            if base <= 7
                && disp.is_some()
                && let Some(d) = encode::disp5(cx, disp, sz)
            {
                // `0011 11ss dbbb dddd`: the displacement's top bit sits
                // above the register.
                let mut enc = Enc::new(&[0x3c + z, 0]);
                enc.field(d >> 4, 8, 1)
                    .field(base as u32, 9, 3)
                    .field(d, 12, 4);
                rungs.push(uimm8_rung(enc).const_only());
            }
            // For `.b` the length-code bits are preset to 1, a byte, which
            // is what a plain byte immediate reads as.
            let preset = if sz == Size::B && disp.is_some() {
                0x04
            } else {
                0
            };
            let mut enc = Enc::new(&[0xf8, base << 4 | z | preset]);
            // Only the long form reads a displacement in GNU as's grammar,
            // and only when one is written; which form is taken is known
            // below.
            let short_available = !rungs.is_empty();
            if disp.is_some() {
                encode::disp(cx, &mut enc, 6, disp, sz, s.span_of(1))?;
                cx.relaxable = false;
            }
            rungs.push(match sz {
                // A byte move's long form stores its immediate as a plain byte;
                // only word and long moves get a length code.
                Size::B => Rung::new(
                    enc,
                    Place::Bytes {
                        n: 1,
                        range: encode::Range::Either,
                        reloc: reloc::DIR8S,
                    },
                ),
                Size::W => Rung::new(enc, Place::Imm { li: 12, bits: 16 }),
                Size::L => Rung::new(enc, imm(12)),
            });
            // A `.b` store to `[reg]` is the exception: GNU as gives it a
            // length-coded immediate too.
            if sz == Size::B && disp.is_none() {
                let last = rungs.pop().expect("a long form was pushed");
                rungs.push(Rung::new(last.enc, Place::Imm { li: 12, bits: 8 }));
            } else if disp.is_some_and(|d| encode::known(cx, d).is_some()) {
                let last = rungs.pop().expect("a long form was pushed");
                rungs.push(last.after_displacement());
            }
            let out = encode::immediate(cx, rungs, e, s.span_of(0))?;
            let short = short_available && out.len() == 1 && out[0].bytes[0] & 0xfc == 0x3c;
            cx.relaxable |= disp.is_some() && !short;
            Some(out)
        }
        [Reg(a), Reg(b)] => Enc::new(&[0xcf | z << 4, a << 4 | b]).one(),
        [Reg(src), Mem { .. }] => {
            let (disp, base) = plain_mem(cx, &s.ops[1])?;
            if disp.is_some()
                && src <= 7
                && base <= 7
                && let Some(d) = encode::disp5(cx, disp, sz)
            {
                return short_mov(0x80 | z << 4, base, src, d);
            }
            let mut enc = Enc::new(&[0xc3 | z << 4, base << 4 | src]);
            // `[reg]` has a grammar rule of its own, which reads no
            // displacement.
            if disp.is_some() {
                encode::disp(cx, &mut enc, 4, disp, sz, s.span_of(1))?;
            }
            enc.one()
        }
        [Mem { .. }, Reg(dst)] => {
            let (disp, base) = plain_mem(cx, &s.ops[0])?;
            if disp.is_some()
                && dst <= 7
                && base <= 7
                && let Some(d) = encode::disp5(cx, disp, sz)
            {
                return short_mov(0x88 | z << 4, base, dst, d);
            }
            let mut enc = Enc::new(&[0xcc | z << 4, base << 4 | dst]);
            if disp.is_some() {
                encode::disp(cx, &mut enc, 6, disp, sz, s.span_of(0))?;
            }
            enc.one()
        }
        [Mem { .. }, Mem { .. }] => {
            let (sd, sb) = plain_mem(cx, &s.ops[0])?;
            let (dd, db) = plain_mem(cx, &s.ops[1])?;
            let mut enc = Enc::new(&[0xc0 | z << 4, sb << 4 | db]);
            encode::disp(cx, &mut enc, 6, sd, sz, s.span_of(0))?;
            encode::disp(cx, &mut enc, 4, dd, sz, s.span_of(1))?;
            enc.one()
        }
        [Reg(src), PostInc(b)] => Enc::new(&[0xfd, 0x20 | z, b << 4 | src]).one(),
        [Reg(src), PreDec(b)] => Enc::new(&[0xfd, 0x24 | z, b << 4 | src]).one(),
        [PostInc(b), Reg(dst)] => Enc::new(&[0xfd, 0x28 | z, b << 4 | dst]).one(),
        [PreDec(b), Reg(dst)] => Enc::new(&[0xfd, 0x2c | z, b << 4 | dst]).one(),
        [Reg(src), Index { index, base }] => {
            Enc::new(&[0xfe, z << 4 | index, base << 4 | src]).one()
        }
        [Index { index, base }, Reg(dst)] => {
            Enc::new(&[0xfe, 0x40 | z << 4 | index, base << 4 | dst]).one()
        }
        _ => bad_operands(cx, s),
    }
}

/// The short `mov` between a register and `disp5[reg]`, both in r0-r7. The
/// five displacement bits are split around the two register fields:
/// `oooo oSdd dbbb drrr`.
fn short_mov(op: u8, base: u8, reg: u8, d: u32) -> Out {
    let mut enc = Enc::new(&[op, 0]);
    enc.field(d >> 2, 5, 3)
        .field(d >> 1, 8, 1)
        .field(base as u32, 9, 3)
        .field(d, 12, 1)
        .field(reg as u32, 13, 3);
    enc.one()
}

fn imm(li: u32) -> Place {
    Place::Imm { li, bits: 32 }
}

/// An unsigned byte immediate, the form behind `mov #uimm8, reg` and friends.
fn uimm8_rung(enc: Enc) -> Rung {
    Rung::new(
        enc,
        Place::Bytes {
            n: 1,
            range: Range::Unsigned,
            reloc: reloc::DIR8U,
        },
    )
}

fn movu(cx: &mut AsmCtx<'_>, s: &Stmt<'_>) -> Out {
    use OpKind::*;
    let sz = match s.suffix {
        None | Some(".w") => Size::W,
        Some(".b") => Size::B,
        Some(x) => {
            cx.error(s.span, format!("`movu` takes `.b` or `.w`, not `{x}`"));
            return None;
        }
    };
    let z = sz.code() as u8;
    match s.kinds()[..] {
        [Reg(a), Reg(b)] => Enc::new(&[0x5b | z << 2, a << 4 | b]).one(),
        [Mem { .. }, Reg(dst)] => {
            let (disp, base) = plain_mem(cx, &s.ops[0])?;
            if disp.is_some()
                && dst <= 7
                && base <= 7
                && let Some(d) = encode::disp5(cx, disp, sz)
            {
                let mut enc = Enc::new(&[0xb0 | z << 3, 0]);
                enc.field(d >> 2, 5, 3)
                    .field(d >> 1, 8, 1)
                    .field(base as u32, 9, 3)
                    .field(d, 12, 1)
                    .field(dst as u32, 13, 3);
                return enc.one();
            }
            let mut enc = Enc::new(&[0x58 | z << 2, base << 4 | dst]);
            if disp.is_some() {
                encode::disp(cx, &mut enc, 6, disp, sz, s.span_of(0))?;
            }
            enc.one()
        }
        [PostInc(b), Reg(dst)] => Enc::new(&[0xfd, 0x38 | z, b << 4 | dst]).one(),
        [PreDec(b), Reg(dst)] => Enc::new(&[0xfd, 0x3c | z, b << 4 | dst]).one(),
        [Index { index, base }, Reg(dst)] => {
            Enc::new(&[0xfe, 0xc0 | z << 4 | index, base << 4 | dst]).one()
        }
        _ => bad_operands(cx, s),
    }
}

fn push(cx: &mut AsmCtx<'_>, s: &Stmt<'_>) -> Out {
    let sz = bwl(cx, s)?;
    let z = sz.code() as u8;
    match s.kinds()[..] {
        [OpKind::Reg(r)] => Enc::new(&[0x7e, 0x80 | z << 4 | r]).one(),
        [OpKind::Mem { .. }] => {
            let (disp, base) = plain_mem(cx, &s.ops[0])?;
            let mut enc = Enc::new(&[0xf4, base << 4 | 0x08 | z]);
            encode::disp(cx, &mut enc, 6, disp, sz, s.span_of(0))?;
            enc.one()
        }
        _ => bad_operands(cx, s),
    }
}

fn pushc(cx: &mut AsmCtx<'_>, s: &Stmt<'_>) -> Out {
    no_suffix(cx, s)?;
    let OpKind::Creg(c) = (match s.kinds()[..] {
        [k] => k,
        _ => return bad_operands(cx, s),
    }) else {
        return bad_operands(cx, s);
    };
    if c.v2() {
        cx.error(
            s.span_of(0),
            "`extb` is an RXv2 control register; this target is RXv1",
        );
        return None;
    }
    if c.num >= 16 {
        cx.error(
            s.span_of(0),
            format!("`{}` can only use the first 16 control registers", s.m),
        );
        return None;
    }
    let op = if s.m == "pushc" { 0xc0 } else { 0xe0 };
    Enc::new(&[0x7e, op | c.num]).one()
}

/// Checks a `first-last` register range for `pushm`, `popm` and `rtsd`.
fn range_ok(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, i: usize, a: u8, b: u8) -> Option<()> {
    if a == 0 {
        cx.error(
            s.span_of(i),
            format!("`{}` cannot include r0, the stack pointer", s.m),
        );
        return None;
    }
    if a > b {
        cx.error(
            s.span_of(i),
            format!("register range r{a}-r{b} is backwards; the first must be the lower"),
        );
        return None;
    }
    Some(())
}

fn pushm(cx: &mut AsmCtx<'_>, s: &Stmt<'_>) -> Out {
    no_suffix(cx, s)?;
    let OpKind::Range(a, b) = (match s.kinds()[..] {
        [k] => k,
        _ => return bad_operands(cx, s),
    }) else {
        return bad_operands(cx, s);
    };
    range_ok(cx, s, 0, a, b)?;
    let push = s.m == "pushm";
    // A range of one register is plain `push.l`/`pop`.
    if a == b {
        let op = if push { 0xa0 } else { 0xb0 };
        return Enc::new(&[0x7e, op | a]).one();
    }
    Enc::new(&[if push { 0x6e } else { 0x6f }, a << 4 | b]).one()
}

fn rtsd(cx: &mut AsmCtx<'_>, s: &Stmt<'_>) -> Out {
    no_suffix(cx, s)?;
    let k = s.kinds();
    let (e, range) = match k[..] {
        [OpKind::Imm(e)] => (e, None),
        [OpKind::Imm(e), OpKind::Range(a, b)] => (e, Some((a, b))),
        _ => return bad_operands(cx, s),
    };
    let span = s.span_of(0);
    let v = encode::constant_in(cx, e, span, "`rtsd` stack adjustment", 0, 1020)?;
    if v % 4 != 0 {
        cx.error(
            span,
            format!("`rtsd` stack adjustment {v} is not a multiple of 4"),
        );
        return None;
    }
    let units = (v / 4) as u8;
    match range {
        None => Enc::new(&[0x67, units]).one(),
        Some((a, b)) => {
            range_ok(cx, s, 1, a, b)?;
            Enc::new(&[0x3f, a << 4 | b, units]).one()
        }
    }
}

// ---- arithmetic ---------------------------------------------------------------

/// A memory source operand with its size suffix, defaulting to `.l`.
fn memex(o: &Operand) -> Option<(Option<ExprRef>, u8, MemEx)> {
    match o.kind {
        OpKind::Mem { disp, base, ext } => Some((disp, base, ext.unwrap_or(MemEx::L))),
        _ => None,
    }
}

/// The memory-source forms shared by the two-operand arithmetic instructions:
/// `.ub` has its own opcode `short`, every other size goes through the `06`
/// prefix with the size in its second byte. `prefix` builds the long form's
/// bytes given that size code; `dsp_long` is where its displacement length
/// lives.
fn mem_source(
    cx: &mut AsmCtx<'_>,
    s: &Stmt<'_>,
    short: impl FnOnce(u8, u8) -> (Enc, u32),
    long: impl FnOnce(u8, u8, u32) -> (Enc, u32),
) -> Out {
    let (disp, base, ext) = memex(&s.ops[0]).expect("caller matched a memory operand");
    let OpKind::Reg(dst) = s.ops[1].kind else {
        return bad_operands(cx, s);
    };
    let (mut enc, pos) = if ext == MemEx::Ub {
        short(base, dst)
    } else {
        long(base, dst, ext.code())
    };
    encode::disp(cx, &mut enc, pos, disp, ext.size(), s.span_of(0))?;
    enc.one()
}

fn subadd(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, sub: u8) -> Out {
    use OpKind::*;
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [Reg(a), Reg(b)] => Enc::new(&[0x43 + (sub << 2), a << 4 | b]).one(),
        [Reg(a), Reg(b), Reg(c)] => Enc::new(&[0xff, sub << 4 | c, a << 4 | b]).one(),
        [Mem { .. }, Reg(_)] => mem_source(
            cx,
            s,
            |b, d| (Enc::new(&[0x40 + (sub << 2), b << 4 | d]), 6),
            |b, d, x| (Enc::new(&[0x06, (x as u8) << 6 | sub << 2, b << 4 | d]), 14),
        ),
        [Imm(e), Reg(r)] => {
            let short = |op| Rung::new(Enc::new(&[op, r]), Place::Nibble(8));
            let rungs = match sub {
                // `sub #imm` beyond four bits is `add` of the negation.
                0 => vec![
                    short(0x60),
                    Rung::new(Enc::new(&[0x70, r << 4 | r]), imm(6)).negated(),
                ],
                2 => vec![
                    short(0x62),
                    Rung::new(Enc::new(&[0x70, r << 4 | r]), imm(6)),
                ],
                _ => vec![
                    short(0x60 + sub),
                    Rung::new(Enc::new(&[0x74, (sub - 2) << 4 | r]), imm(6)),
                ],
            };
            encode::immediate(cx, rungs, e, s.span_of(0))
        }
        [Imm(e), Reg(a), Reg(b)] if sub == 2 => {
            let rungs = vec![Rung::new(Enc::new(&[0x70, a << 4 | b]), imm(6))];
            encode::immediate(cx, rungs, e, s.span_of(0))
        }
        _ => bad_operands(cx, s),
    }
}

fn cmp(cx: &mut AsmCtx<'_>, s: &Stmt<'_>) -> Out {
    use OpKind::*;
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [Reg(a), Reg(b)] => Enc::new(&[0x47, a << 4 | b]).one(),
        [Mem { .. }, Reg(_)] => mem_source(
            cx,
            s,
            |b, d| (Enc::new(&[0x44, b << 4 | d]), 6),
            |b, d, x| (Enc::new(&[0x06, (x as u8) << 6 | 0x04, b << 4 | d]), 14),
        ),
        [Imm(e), Reg(r)] => {
            let rungs = vec![
                Rung::new(Enc::new(&[0x61, r]), Place::Nibble(8)),
                uimm8_rung(Enc::new(&[0x75, 0x50 | r])).const_only(),
                Rung::new(Enc::new(&[0x74, r]), imm(6)),
            ];
            encode::immediate(cx, rungs, e, s.span_of(0))
        }
        _ => bad_operands(cx, s),
    }
}

/// `fd 70 op<<4|reg` followed by a length-coded immediate: the immediate form
/// of `adc`, `max`, `min`, `emul`, `emulu`, `div`, `divu`, `tst`, `xor`, `stz`
/// and `stnz`.
fn dp20_imm(cx: &mut AsmCtx<'_>, e: ExprRef, r: u8, op: u8, span: Span) -> Out {
    let rungs = vec![Rung::new(Enc::new(&[0xfd, 0x70, op << 4 | r]), imm(12))];
    encode::immediate(cx, rungs, e, span)
}

/// The `fc`/`06` register and memory forms of the "dp20" group. `op` is the
/// group index GNU as calls `sub_op`.
fn dp20_rm(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, op: u8) -> Out {
    use OpKind::*;
    match s.kinds()[..] {
        [Reg(a), Reg(b)] => Enc::new(&[0xfc, 0x03 + (op << 2), a << 4 | b]).one(),
        [Mem { .. }, Reg(_)] => mem_source(
            cx,
            s,
            |b, d| (Enc::new(&[0xfc, op << 2, b << 4 | d]), 14),
            |b, d, x| (Enc::new(&[0x06, 0x20 | (x as u8) << 6, op, b << 4 | d]), 14),
        ),
        _ => bad_operands(cx, s),
    }
}

fn dp20(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, op: u8) -> Out {
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [OpKind::Imm(e), OpKind::Reg(r)] => dp20_imm(cx, e, r, op, s.span_of(0)),
        [OpKind::Reg(_), OpKind::Reg(_), OpKind::Reg(_)] if s.m == "xor" => {
            cx.error(
                s.span,
                "three-operand `xor` is an RXv3 instruction; this target is RXv1",
            );
            None
        }
        _ => dp20_rm(cx, s, op),
    }
}

/// `xchg`, `itof` and `utof`: the dp20 register and memory forms, no
/// immediate.
fn xchg_like(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, op: u8) -> Out {
    no_suffix(cx, s)?;
    dp20_rm(cx, s, op)
}

fn adc_sbb(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, op: u8) -> Out {
    use OpKind::*;
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [Reg(a), Reg(b)] => Enc::new(&[0xfc, 0x03 + (op << 2), a << 4 | b]).one(),
        [Mem { disp, base, ext }, Reg(dst)] => {
            // Only a long memory operand: the carry instructions have no
            // byte or word forms.
            if !matches!(ext, None | Some(MemEx::L)) {
                cx.error(
                    s.span_of(0),
                    format!("`{}` only takes a long (`.l`) memory operand", s.m),
                );
                return None;
            }
            let mut enc = Enc::new(&[0x06, 0xa0, op, base << 4 | dst]);
            encode::disp(cx, &mut enc, 14, disp, Size::L, s.span_of(0))?;
            enc.one()
        }
        [Imm(e), Reg(r)] if op == 2 => dp20_imm(cx, e, r, 2, s.span_of(0)),
        [Imm(e), Reg(r)] => {
            // There is no `sbb #imm`: GNU as assembles `adc #~imm`, which
            // subtracts `imm` plus the borrow. That needs the value now.
            let span = s.span_of(0);
            let Some(v) = encode::known(cx, e) else {
                cx.error(span, "`sbb` cannot use a symbolic immediate");
                return None;
            };
            let rungs = vec![Rung::new(Enc::new(&[0xfd, 0x70, 0x20 | r]), imm(12))];
            encode::const_immediate(cx, rungs, v.wrapping_neg().wrapping_sub(1), span)
                .map(|v| vec![v])
        }
        _ => bad_operands(cx, s),
    }
}

/// `neg`, `abs` and `not`: in place (`7e`), or from one register to another.
fn unary(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, op: u8, op2: u8) -> Out {
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [OpKind::Reg(r)] => Enc::new(&[0x7e, op2 << 4 | r]).one(),
        [OpKind::Reg(a), OpKind::Reg(b)] => Enc::new(&[0xfc, 0x03 + (op << 2), a << 4 | b]).one(),
        _ => bad_operands(cx, s),
    }
}

fn store_cond(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, op: u8) -> Out {
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [OpKind::Imm(e), OpKind::Reg(r)] => dp20_imm(cx, e, r, op, s.span_of(0)),
        [OpKind::Reg(_), OpKind::Reg(_)] => {
            cx.error(
                s.span,
                format!(
                    "`{}` between registers is an RXv2 instruction; this target is RXv1",
                    s.m
                ),
            );
            None
        }
        _ => bad_operands(cx, s),
    }
}

// ---- shifts ---------------------------------------------------------------------

fn shift(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, op: u8) -> Out {
    use OpKind::*;
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [Imm(e), Reg(r)] => {
            let n = uconst(cx, e, s.span_of(0), "shift count", 5)?;
            let mut enc = Enc::new(&[0x68 + (op << 1), r]);
            enc.field(n, 7, 5);
            enc.one()
        }
        [Imm(e), Reg(a), Reg(b)] => {
            let n = uconst(cx, e, s.span_of(0), "shift count", 5)?;
            Enc::new(&[0xfd, 0x80 | op << 5 | n as u8, a << 4 | b]).one()
        }
        [Reg(a), Reg(b)] => Enc::new(&[0xfd, 0x60 + op, a << 4 | b]).one(),
        _ => bad_operands(cx, s),
    }
}

fn rotate(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, op: u8) -> Out {
    use OpKind::*;
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [Imm(e), Reg(r)] => {
            let n = uconst(cx, e, s.span_of(0), "rotation count", 5)?;
            let mut enc = Enc::new(&[0xfd, 0x68 + op, r]);
            enc.field(n, 15, 5);
            enc.one()
        }
        [Reg(a), Reg(b)] => Enc::new(&[0xfd, 0x60 + op, a << 4 | b]).one(),
        _ => bad_operands(cx, s),
    }
}

// ---- bit manipulation -------------------------------------------------------------

/// `bset` (0), `bclr` (1), `btst` (2) and `bnot` (3).
fn bit_op(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, op: u8) -> Out {
    use OpKind::*;
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [Imm(e), Reg(r)] => {
            let n = uconst(cx, e, s.span_of(0), "bit number", 5)?;
            if op == 3 {
                let mut enc = Enc::new(&[0xfd, 0xe0, 0xf0 | r]);
                enc.field(n, 11, 5);
                return enc.one();
            }
            let mut enc = Enc::new(&[0x78 + (op << 1), r]);
            enc.field(n, 7, 5);
            enc.one()
        }
        [Imm(e), Mem { disp, base, ext }] => {
            byte_mem_suffix(cx, s, 1, ext)?;
            let n = uconst(cx, e, s.span_of(0), "bit number in a byte", 3)?;
            let mut enc = if op == 3 {
                let mut enc = Enc::new(&[0xfc, 0xe0, base << 4 | 0x0f]);
                enc.field(n, 11, 3);
                enc
            } else {
                // bset f0 _0, bclr f0 _8, btst f4 _0.
                let (b0, b1) = [(0xf0, 0x00), (0xf0, 0x08), (0xf4, 0x00)][op as usize];
                Enc::new(&[b0, base << 4 | b1 | n as u8])
            };
            let pos = if op == 3 { 14 } else { 6 };
            encode::disp(cx, &mut enc, pos, disp, Size::B, s.span_of(1))?;
            enc.one()
        }
        [Reg(bit), Reg(r)] => Enc::new(&[0xfc, 0x63 + (op << 2), r << 4 | bit]).one(),
        [Reg(bit), Mem { disp, base, ext }] => {
            byte_mem_suffix(cx, s, 1, ext)?;
            let mut enc = Enc::new(&[0xfc, 0x60 + (op << 2), base << 4 | bit]);
            encode::disp(cx, &mut enc, 14, disp, Size::B, s.span_of(1))?;
            enc.one()
        }
        _ => bad_operands(cx, s),
    }
}

/// Bit operations address memory a byte at a time; `.b` may be written.
fn byte_mem_suffix(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, i: usize, ext: Option<MemEx>) -> Option<()> {
    match ext {
        None | Some(MemEx::B) => Some(()),
        Some(_) => {
            cx.error(
                s.span_of(i),
                format!("`{}` works on a byte of memory; only `.b` is allowed", s.m),
            );
            None
        }
    }
}

fn bmcnd(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, c: u8) -> Out {
    use OpKind::*;
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [Imm(e), Reg(r)] => {
            let n = uconst(cx, e, s.span_of(0), "bit number", 5)?;
            let mut enc = Enc::new(&[0xfd, 0xe0, c << 4 | r]);
            enc.field(n, 11, 5);
            enc.one()
        }
        [Imm(e), Mem { disp, base, ext }] => {
            byte_mem_suffix(cx, s, 1, ext)?;
            let n = uconst(cx, e, s.span_of(0), "bit number in a byte", 3)?;
            let mut enc = Enc::new(&[0xfc, 0xe0, base << 4 | c]);
            enc.field(n, 11, 3);
            encode::disp(cx, &mut enc, 14, disp, Size::B, s.span_of(1))?;
            enc.one()
        }
        _ => bad_operands(cx, s),
    }
}

fn sccnd(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, c: u8) -> Out {
    use OpKind::*;
    let sz = bwl(cx, s)?;
    match s.kinds()[..] {
        [Reg(r)] => {
            // A register is always set whole, and GNU as insists on saying so.
            if s.suffix != Some(".l") {
                cx.error(
                    s.span,
                    format!("`{}` to a register must be written `{}.l`", s.m, s.m),
                );
                return None;
            }
            Enc::new(&[0xfc, 0xdb, r << 4 | c]).one()
        }
        [Mem { .. }] => {
            let (disp, base) = plain_mem(cx, &s.ops[0])?;
            let mut enc = Enc::new(&[0xfc, 0xd0 | (sz.code() as u8) << 2, base << 4 | c]);
            encode::disp(cx, &mut enc, 14, disp, sz, s.span_of(0))?;
            enc.one()
        }
        _ => bad_operands(cx, s),
    }
}

// ---- branches and system ------------------------------------------------------------

fn bra_bsr(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, kind: Kind, reg_op: u8) -> Out {
    match s.kinds()[..] {
        // `bra r1` / `bra.l r1`: a register-relative branch.
        [OpKind::Reg(r)] => {
            if !matches!(s.suffix, None | Some(".l")) {
                cx.error(
                    s.span,
                    format!("`{}` to a register takes no size but `.l`", s.m),
                );
                return None;
            }
            Enc::new(&[0x7f, reg_op | r]).one()
        }
        [OpKind::Expr(t)] => branch_to(cx, s, kind, t),
        _ => bad_operands(cx, s),
    }
}

fn bcnd(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, c: u8) -> Out {
    match s.kinds()[..] {
        [OpKind::Expr(t)] => branch_to(cx, s, Kind::Cond(c), t),
        _ => bad_operands(cx, s),
    }
}

fn branch_to(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, kind: Kind, t: ExprRef) -> Out {
    let width = match s.suffix {
        None => Width::Auto,
        Some(x) => match Width::from_suffix(x) {
            Some(w) => w,
            None => {
                cx.error(s.span, format!("`{}` is not a branch size", x));
                return None;
            }
        },
    };
    branch::assemble(cx, kind, width, t, &s.name(), s.span)
}

fn int(cx: &mut AsmCtx<'_>, s: &Stmt<'_>) -> Out {
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [OpKind::Imm(e)] => {
            let rungs = vec![uimm8_rung(Enc::new(&[0x75, 0x60]))];
            encode::immediate(cx, rungs, e, s.span_of(0))
        }
        _ => bad_operands(cx, s),
    }
}

fn mvtipl(cx: &mut AsmCtx<'_>, s: &Stmt<'_>) -> Out {
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [OpKind::Imm(e)] => {
            let n = uconst(cx, e, s.span_of(0), "interrupt priority level", 4)?;
            Enc::new(&[0x75, 0x70, n as u8]).one()
        }
        _ => bad_operands(cx, s),
    }
}

fn psw_flag(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, op: u8) -> Out {
    no_suffix(cx, s)?;
    let flag = match s.ops[..] {
        [Operand { ident: Some(n), .. }] => reg::flag(cx.name(n)),
        _ => None,
    };
    match flag {
        Some(f) => Enc::new(&[0x7f, op | f]).one(),
        None => {
            cx.error(
                s.span,
                format!("`{}` takes one flag: `c`, `z`, `s`, `o`, `i` or `u`", s.m),
            );
            None
        }
    }
}

fn creg_ok(cx: &mut AsmCtx<'_>, span: Span, c: reg::Creg) -> Option<()> {
    if c.v2() {
        cx.error(
            span,
            "`extb` is an RXv2 control register; this target is RXv1",
        );
        return None;
    }
    Some(())
}

fn mvtc(cx: &mut AsmCtx<'_>, s: &Stmt<'_>) -> Out {
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [OpKind::Reg(r), OpKind::Creg(c)] => {
            creg_ok(cx, s.span_of(1), c)?;
            Enc::new(&[0xfd, 0x68 | c.num >> 4, r << 4 | (c.num & 0xf)]).one()
        }
        [OpKind::Imm(e), OpKind::Creg(c)] => {
            creg_ok(cx, s.span_of(1), c)?;
            let rungs = vec![Rung::new(Enc::new(&[0xfd, 0x73, c.num]), imm(12))];
            encode::immediate(cx, rungs, e, s.span_of(0))
        }
        _ => bad_operands(cx, s),
    }
}

fn mvfc(cx: &mut AsmCtx<'_>, s: &Stmt<'_>) -> Out {
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [OpKind::Creg(c), OpKind::Reg(r)] => {
            creg_ok(cx, s.span_of(0), c)?;
            Enc::new(&[0xfd, 0x6a | c.num >> 4, (c.num & 0xf) << 4 | r]).one()
        }
        _ => bad_operands(cx, s),
    }
}

// ---- floating point and DSP -----------------------------------------------------------

/// The single-precision instructions. `with_imm` is whether GNU as accepts an
/// immediate source, which it takes as a raw 32-bit pattern.
fn float(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, op: u8, with_imm: bool) -> Out {
    use OpKind::*;
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [Imm(e), Reg(r)] if with_imm => {
            let rungs = vec![Rung::new(
                Enc::new(&[0xfd, 0x72, op << 4 | r]),
                Place::Bytes {
                    n: 4,
                    range: encode::Range::Either,
                    reloc: reloc::DIR32,
                },
            )];
            encode::immediate(cx, rungs, e, s.span_of(0))
        }
        [Reg(a), Reg(b)] => Enc::new(&[0xfc, 0x83 + (op << 2), a << 4 | b]).one(),
        [Mem { disp, base, ext }, Reg(dst)] => {
            if !matches!(ext, None | Some(MemEx::L)) {
                cx.error(
                    s.span_of(0),
                    format!("`{}` only takes a long (`.l`) memory operand", s.m),
                );
                return None;
            }
            let mut enc = Enc::new(&[0xfc, 0x80 + (op << 2), base << 4 | dst]);
            encode::disp(cx, &mut enc, 14, disp, Size::L, s.span_of(0))?;
            enc.one()
        }
        _ => bad_operands(cx, s),
    }
}

/// `mvtachi` and friends: a register in the low nibble of `fd op2 x_`.
fn acc_reg(cx: &mut AsmCtx<'_>, s: &Stmt<'_>, op: u8, op2: u8) -> Out {
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [OpKind::Reg(r)] => Enc::new(&[0xfd, op, op2 | r]).one(),
        _ => bad_operands(cx, s),
    }
}

fn racw(cx: &mut AsmCtx<'_>, s: &Stmt<'_>) -> Out {
    no_suffix(cx, s)?;
    match s.kinds()[..] {
        [OpKind::Imm(e)] => {
            let n = encode::constant_in(cx, e, s.span_of(0), "`racw` shift", 1, 2)?;
            Enc::new(&[0xfd, 0x18, if n == 2 { 0x10 } else { 0x00 }]).one()
        }
        _ => bad_operands(cx, s),
    }
}
