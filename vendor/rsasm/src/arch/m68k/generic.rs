//! The table-driven encoder, for every instruction [`super::ops`] does not
//! encode by hand: the 68881/68882 and ColdFire FPUs, the 68851 and on-chip
//! MMUs, `cas`/`cas2`, `callm`/`rtm`, `pack`/`unpk`, `move16`, `trapcc`,
//! `movep`/`moves`, the 68040's cache instructions, CPU32's `bgnd` and the
//! ColdFire additions.
//!
//! Each instruction is a list of [`Form`]s from GNU's table, tried in order;
//! the first whose operands all fit, on a CPU that has it, is assembled. A
//! form's `args` string holds two characters per operand, the kind of operand
//! it takes and the place its value goes, and both are read exactly as
//! `tc-m68k.c`'s `m68k_ip` reads them — its first switch decides whether an
//! operand fits, its second what it writes. `include/opcode/m68k.h` documents
//! the letters.
//!
//! Where GNU as would warn and write something anyway — a trap vector out of
//! range becomes 0, a symbol where a constant belongs becomes its addend —
//! this refuses with an error instead.
//!
//! The same matcher checks, on ColdFire, what the hand-written encoders wrote
//! against GNU's ColdFire forms of the instruction: see [`coldfire_check`].

use super::encode::{self, EaCtx, Part, Place, Sz, build};
use super::float::Float;
use super::operand::{Base, Mode, Operand};
use super::reloc;
use super::table::{self, Form, rid};
use super::{Cpu, describe_arch};
use crate::arch::{AsmCtx, InsnRequest};
use crate::section::{Fixup, FixupKind, Variant};
use crate::source::Span;

/// The forms of a mnemonic GNU's table spells this way (no dot, size letter
/// included), following its aliases.
pub fn forms(name: &str) -> Option<&'static [Form]> {
    let name = match table::ALIASES.binary_search_by(|(a, _)| (*a).cmp(name)) {
        Ok(i) => table::ALIASES[i].1,
        Err(_) => name,
    };
    let lo = table::FORMS.partition_point(|x| x.name < name);
    let hi = table::FORMS.partition_point(|x| x.name <= name);
    (lo < hi).then(|| &table::FORMS[lo..hi])
}

/// An operand as `tc-m68k.c` classifies it for matching.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum G {
    DReg(u8),
    AReg(u8),
    FReg(u8),
    /// GNU's `CONTROL`: any other register.
    Ctl(u16),
    Abs,
    Imm,
    /// A float literal, which GNU as holds as a bignum.
    Big,
    Ind(u8),
    Inc(u8),
    Dec(u8),
    /// A displacement from an address register or the PC, no index.
    Disp {
        pc: bool,
        reg: Option<u8>,
    },
    /// Everything with a full extension word or an index: GNU's `BASE`,
    /// `PRE` and `POST`.
    Full {
        pc: bool,
        memind: bool,
    },
    List(u32),
}

impl G {
    fn of(op: &Operand) -> G {
        match &op.mode {
            Mode::DReg(n) => G::DReg(*n),
            Mode::AReg(n) => G::AReg(*n),
            Mode::FReg(n) => G::FReg(*n),
            Mode::Sr => G::Ctl(rid::SR),
            Mode::Ccr => G::Ctl(rid::CCR),
            Mode::Usp => G::Ctl(rid::USP),
            Mode::Ctl(id) => G::Ctl(*id),
            Mode::Abs(_) => G::Abs,
            Mode::Imm(..) => G::Imm,
            Mode::FImm(..) => G::Big,
            Mode::Ind(n) => G::Ind(*n),
            Mode::PostInc(n) => G::Inc(*n),
            Mode::PreDec(n) => G::Dec(*n),
            Mode::Indexed {
                base, index: None, ..
            } if *base != Base::None => G::Disp {
                pc: *base == Base::Pc,
                reg: match base {
                    Base::A(n) => Some(*n),
                    _ => None,
                },
            },
            Mode::Indexed { base, .. } => G::Full {
                pc: *base == Base::Pc,
                memind: false,
            },
            Mode::MemInd { base, .. } => G::Full {
                pc: *base == Base::Pc,
                memind: true,
            },
            Mode::RegList(m) => G::List(*m),
            // Spread out before matching; see `flatten`.
            Mode::Pair(..) | Mode::Colon(..) => G::List(0),
        }
    }

    /// GNU's `opP->reg == PC`, which only a displacement or base mode has.
    fn pc(self) -> bool {
        matches!(self, G::Disp { pc: true, .. } | G::Full { pc: true, .. })
    }

    fn imm(self) -> bool {
        matches!(self, G::Imm | G::Big)
    }
}

/// Spreads out what GNU as reads as separate operands: the two sides of a
/// colon, and the contents of a `{...}` after the operand they follow.
fn flatten(op: Operand, out: &mut Vec<Operand>) {
    let brace = op.brace;
    match op.mode {
        Mode::Pair(h, l) => {
            for n in [h, l] {
                out.push(Operand {
                    mode: Mode::DReg(n),
                    span: op.span,
                    brace: Vec::new(),
                });
            }
        }
        Mode::Colon(a, b) => {
            flatten(*a, out);
            flatten(*b, out);
        }
        mode => out.push(Operand {
            mode,
            span: op.span,
            brace: Vec::new(),
        }),
    }
    out.extend(brace);
}

/// `isbyte`, `issbyte`, `isword` and `issword` from `tc-m68k.c`.
fn fits_place(place: u8, v: i64) -> bool {
    let v = v as i32 as i64;
    match place {
        b'b' => (-255..=255).contains(&v),
        b'B' => (-128..=127).contains(&v),
        b'w' => (-65535..=65535).contains(&v),
        b'W' => (-32768..=32767).contains(&v),
        _ => true,
    }
}

struct Matcher<'c, 'a> {
    cx: &'c AsmCtx<'a>,
    cpu: Cpu,
}

impl Matcher<'_, '_> {
    fn constant(&self, op: &Operand) -> Option<i64> {
        match op.mode {
            Mode::Imm(e, _) => self.cx.constant(e),
            _ => None,
        }
    }

    /// Whether an operand of kind `k` and place `p` can be `op`: `m68k_ip`'s
    /// first switch.
    fn fits(&self, k: u8, p: u8, op: &Operand) -> bool {
        use G::*;
        let g = G::of(op);
        let ctl = |ids: &[u16]| matches!(g, Ctl(id) if ids.contains(&id));
        match k {
            b'!' => {
                !matches!(
                    g,
                    DReg(_) | AReg(_) | FReg(_) | Ctl(_) | Inc(_) | Dec(_) | List(_)
                ) && !g.imm()
            }
            b'<' => {
                !matches!(g, DReg(_) | AReg(_) | FReg(_) | Ctl(_) | Dec(_) | List(_)) && !g.imm()
            }
            b'>' => match g {
                DReg(_) | AReg(_) | FReg(_) | Ctl(_) | Imm | Big | Inc(_) | List(_) => false,
                Abs => true,
                _ => !g.pc(),
            },
            b'b' => !matches!(
                g,
                Imm | Big | Abs | AReg(_) | FReg(_) | Ctl(_) | List(_) | Full { memind: true, .. }
            ),
            b'p' => match g {
                DReg(_) | AReg(_) | Ind(_) | Inc(_) | Dec(_) => true,
                Disp { .. } => !g.pc(),
                _ => false,
            },
            b'q' => match g {
                DReg(_) | Ind(_) | Inc(_) | Dec(_) => true,
                Disp { .. } => !g.pc(),
                _ => false,
            },
            // ColdFire's `move` operands.
            b'm' => matches!(g, DReg(_) | AReg(_) | Ind(_) | Inc(_) | Dec(_)),
            b'n' => matches!(g, Disp { .. }),
            b'o' => matches!(g, Full { .. } | Abs) || g.imm(),
            b'Q' => self
                .constant(op)
                .is_some_and(|v| (1..=8).contains(&(v as u32))),
            b'J' => match g {
                Ctl(id) => {
                    (rid::USP..=rid::MBO).contains(&id)
                        && super::reg::movec(id, self.cpu.ctrl).is_some()
                }
                _ => false,
            },
            b'v' => match g {
                DReg(_) | Ind(_) | Inc(_) | Dec(_) | Abs => true,
                Disp { .. } => !g.pc(),
                _ => false,
            },
            b'w' => !matches!(
                g,
                Imm | Big
                    | Abs
                    | AReg(_)
                    | DReg(_)
                    | FReg(_)
                    | Ctl(_)
                    | List(_)
                    | Full { memind: true, .. }
            ),
            b'y' => matches!(g, Ind(_)) || matches!(g, Disp { pc: false, .. }),
            b'z' => matches!(g, Ind(_) | Disp { .. }),
            b'#' => match g {
                Imm => self.constant(op).is_none_or(|v| fits_place(p, v)),
                // A float is a bignum, which only the unchecked places take.
                Big => !matches!(p, b'b' | b'B' | b'w' | b'W'),
                _ => false,
            },
            b'^' | b'T' | b'k' => g.imm(),
            b'$' => {
                !matches!(g, AReg(_) | Ctl(_) | FReg(_) | List(_))
                    && !g.imm()
                    && !(g != Abs && g.pc())
            }
            b'%' => !matches!(g, Ctl(_) | FReg(_) | List(_)) && !g.imm() && !(g != Abs && g.pc()),
            b'&' => match g {
                DReg(_) | AReg(_) | FReg(_) | Ctl(_) | Imm | Big | Inc(_) | Dec(_) | List(_) => {
                    false
                }
                Abs => true,
                _ => !g.pc(),
            },
            b'*' => !matches!(g, Ctl(_) | FReg(_) | List(_)),
            b'+' => matches!(g, Inc(_)),
            b'-' => matches!(g, Dec(_)),
            b'/' => {
                !matches!(g, AReg(_) | Ctl(_) | FReg(_) | Inc(_) | Dec(_) | List(_)) && !g.imm()
            }
            b';' => !matches!(g, AReg(_) | Ctl(_) | FReg(_) | List(_)),
            b'?' => match g {
                AReg(_) | Ctl(_) | FReg(_) | Inc(_) | Dec(_) | Imm | Big | List(_) => false,
                Abs => true,
                _ => !g.pc(),
            },
            b'@' => !matches!(g, AReg(_) | Ctl(_) | FReg(_) | List(_)) && !g.imm(),
            b'~' => match g {
                DReg(_) | AReg(_) | Ctl(_) | FReg(_) | Imm | Big | List(_) => false,
                Abs => true,
                _ => !g.pc(),
            },
            b'|' => !matches!(g, Ctl(_) | FReg(_) | DReg(_) | AReg(_) | List(_)) && !g.imm(),
            b'3' => ctl(&[rid::TT0, rid::TT1]),
            b'A' => matches!(g, AReg(_)),
            b'a' => matches!(g, Ind(_)),
            b'B' | b'_' => g == Abs,
            b'C' => ctl(&[rid::CCR]),
            b'd' => matches!(g, Disp { reg: Some(_), .. }),
            b'D' => matches!(g, DReg(_)),
            b'F' => matches!(g, FReg(_)),
            b'l' | b'L' => match g {
                DReg(_) | AReg(_) | FReg(_) => p != b'8',
                Ctl(rid::FPI | rid::FPS | rid::FPC) => p == b'8',
                List(m) => match p {
                    b'8' => m & 0x0ff_ffff == 0,
                    b'3' => m & 0x700_0000 == 0,
                    _ => true,
                },
                _ => false,
            },
            b'M' => self.constant(op).is_some_and(|v| (-128..=127).contains(&v)),
            b'R' => matches!(g, DReg(_) | AReg(_)),
            b'r' => match (&op.mode, g) {
                (_, Ind(_)) => true,
                (
                    Mode::Indexed {
                        base: Base::None,
                        disp: None,
                        index: Some(ix),
                    },
                    _,
                ) => ix.scale == 1 && !ix.sized,
                _ => false,
            },
            b's' => ctl(&[rid::FPI, rid::FPS, rid::FPC]),
            b'S' => ctl(&[rid::SR]),
            b't' => self
                .constant(op)
                .is_some_and(|v| (0..=7).contains(&(v as u32))),
            b'U' => ctl(&[rid::USP]),
            b'x' => self
                .constant(op)
                .is_some_and(|v| v as u32 == u32::MAX || (1..=7).contains(&(v as u32))),
            b'f' => ctl(&[rid::SFC, rid::DFC]),
            b'0' => ctl(&[rid::TC]),
            b'1' => ctl(&[rid::AC]),
            b'2' => ctl(&[rid::CAL, rid::VAL, rid::SCC]),
            b'V' => ctl(&[rid::VAL]),
            b'W' => ctl(&[rid::DRP, rid::SRP, rid::CRP]),
            b'X' => matches!(g, Ctl(id) if (rid::BAD0..=rid::BAD7).contains(&id)
                || (rid::BAC0..=rid::BAC7).contains(&id)),
            b'Y' => ctl(&[rid::PSR]),
            b'Z' => ctl(&[rid::PCSR]),
            b'c' => ctl(&[rid::NC, rid::IC, rid::DC, rid::BC]),
            _ => false,
        }
    }
}

/// Assembles `name` (as written, for messages) by the first of its `forms`
/// that fits the operands.
pub fn assemble(
    cx: &mut AsmCtx<'_>,
    cpu: Cpu,
    name: &str,
    forms: &[Form],
    req: &InsnRequest<'_>,
) -> Option<Vec<Variant>> {
    let parsed = super::operand::parse_list(cx, &req.cursor())?;
    let mut ops = Vec::new();
    for op in parsed {
        flatten(op, &mut ops);
    }
    let mut ok_arch = 0;
    let mut chosen = None;
    let m = Matcher { cx, cpu };
    for form in forms {
        let args = form.args.as_bytes();
        // The coprocessor number of an FPU instruction is an operand GNU as
        // supplies itself.
        let skip = usize::from(args.first() == Some(&b'I'));
        ok_arch |= form.arch;
        if args.len() / 2 - skip != ops.len() || form.arch & cpu.arch == 0 {
            continue;
        }
        let fits = args[skip * 2..]
            .chunks(2)
            .zip(&ops)
            .all(|(kp, op)| m.fits(kp[0], kp[1], op));
        if fits {
            chosen = Some(form);
            break;
        }
    }
    let Some(form) = chosen else {
        if ok_arch & cpu.arch == 0 {
            let cpu = cpu.describe();
            cx.error(
                req.mnemonic_span,
                format!(
                    "`{name}` needs {}; this target is a {cpu}",
                    describe_arch(ok_arch)
                ),
            );
        } else if ops.is_empty() {
            cx.error(req.span, format!("`{name}` needs operands"));
        } else {
            let what: Vec<&str> = ops.iter().map(Operand::describe).collect();
            cx.error(req.span, format!("`{name}` cannot take {}", join(&what)));
        }
        return None;
    };
    Encoder {
        cx,
        cpu,
        words: [(form.opcode >> 16) as u16, form.opcode as u16, 0],
        nwords: form.words as usize,
        inserted: Vec::new(),
        appended: Vec::new(),
        coproc_branch: None,
    }
    .encode(form, &ops)
}

/// On a ColdFire, whether what the hand-written encoders wrote for `name`
/// (GNU's spelling of the mnemonic, size letter included) is an instruction
/// the CPU has.
///
/// Those encoders choose among the 68000's forms, and ColdFire dropped many
/// of them — `addi` to memory, `movem` with `-(An)`, a shift of memory. So
/// the first word written, and the second where the form has one, must match
/// one of GNU's ColdFire forms of the mnemonic under its mask, with the
/// operands that form takes.
pub fn coldfire_check(
    cx: &mut AsmCtx<'_>,
    cpu: Cpu,
    name: &str,
    written: &str,
    variant: &Variant,
    req: &InsnRequest<'_>,
) -> Option<()> {
    let name = match table::CF_ALIASES.binary_search_by(|(a, _)| (*a).cmp(name)) {
        Ok(i) => table::CF_ALIASES[i].1,
        Err(_) => name,
    };
    let lo = table::CF_FORMS.partition_point(|x| x.form.name < name);
    let hi = table::CF_FORMS.partition_point(|x| x.form.name <= name);
    if lo == hi {
        // A spelling of rsasm's own, which the CPU check has already let by.
        return Some(());
    }
    let parsed = super::operand::parse_list(cx, &req.cursor())?;
    let mut ops = Vec::new();
    for op in parsed {
        flatten(op, &mut ops);
    }
    let word = |i: usize| {
        variant
            .bytes
            .get(2 * i..2 * i + 2)
            .map_or(0, |b| u16::from_be_bytes([b[0], b[1]]) as u32)
    };
    let written_words = (word(0) << 16) | word(1);
    let m = Matcher { cx, cpu };
    let fits = table::CF_FORMS[lo..hi].iter().any(|cf| {
        let form = &cf.form;
        let mask = if form.words == 2 {
            cf.mask
        } else {
            cf.mask & 0xffff_0000
        };
        let args = form.args.as_bytes();
        form.arch & cpu.arch != 0
            && written_words & mask == form.opcode & mask
            && args.len() / 2 == ops.len()
            && args
                .chunks(2)
                .zip(&ops)
                .all(|(kp, op)| m.fits(kp[0], kp[1], op))
    });
    if fits {
        return Some(());
    }
    let what: Vec<&str> = ops.iter().map(Operand::describe).collect();
    let with = if what.is_empty() {
        String::new()
    } else {
        format!(" with {}", join(&what))
    };
    cx.error(
        req.span,
        format!(
            "`{written}`{with} is not an instruction a {} has",
            cpu.describe()
        ),
    );
    None
}

fn join(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [a] => a.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

struct Encoder<'c, 'a> {
    cx: &'c mut AsmCtx<'a>,
    cpu: Cpu,
    /// The opcode words, which operands are installed into.
    words: [u16; 3],
    nwords: usize,
    /// Extension words GNU as's `insop` puts straight after the opcode words,
    /// ahead of any earlier ones.
    inserted: Vec<Part>,
    /// Extension words in operand order, GNU as's `addword`.
    appended: Vec<Part>,
    /// A `Bc` branch, which chooses its own length.
    coproc_branch: Option<(crate::expr::ExprRef, Span)>,
}

impl Encoder<'_, '_> {
    fn err<T>(&mut self, span: Span, msg: impl Into<String>) -> Option<T> {
        self.cx.error(span, msg);
        None
    }

    /// `install_operand`: a value into its place in the opcode words.
    fn install(&mut self, place: u8, val: u16) {
        let w = &mut self.words;
        match place {
            b's' => w[0] |= val & 0xff,
            b'd' | b'i' => w[0] |= val << 9,
            b'e' => w[0] |= val << 6,
            b'1' => w[1] |= val << 12,
            b'2' => w[1] |= val << 6,
            b'3' | b'C' => w[1] |= val,
            b'4' => w[2] |= val << 12,
            b'5' => w[2] |= val << 6,
            b'6' => {
                // `cas2`'s third word exists because this place does.
                self.nwords = 3;
                w[2] |= val;
            }
            b'7' => w[1] |= val << 7,
            b'8' => w[1] |= val << 10,
            b'9' => w[1] |= val << 5,
            b't' => w[1] |= (val << 10) | (val << 7),
            b'k' => w[1] |= val << 4,
            _ => {}
        }
    }

    /// A constant immediate in `lo..=hi`, for the places that take one.
    fn value(&mut self, op: &Operand, lo: i64, hi: i64, what: &str) -> Option<i64> {
        let (e, span) = match op.mode {
            Mode::Imm(e, span) => (e, span),
            Mode::Abs(v) => (v.e, v.span),
            _ => return self.err(op.span, format!("{what} must be an immediate")),
        };
        match self.cx.constant(e) {
            Some(v) if (lo..=hi).contains(&v) => Some(v),
            Some(v) => self.err(span, format!("{what} {v} is out of range ({lo} to {hi})")),
            None => self.err(
                span,
                format!("{what} must be a constant, known where it is written"),
            ),
        }
    }

    fn encode(mut self, form: &Form, ops: &[Operand]) -> Option<Vec<Variant>> {
        let args = form.args.as_bytes();
        let mut ops = ops.iter();
        for kp in args.chunks(2) {
            let (k, p) = (kp[0], kp[1]);
            if k == b'I' {
                // Coprocessor 1, the FPU: `m68k_float_copnum`.
                self.install(p, 1);
                continue;
            }
            let Some(op) = ops.next() else {
                break;
            };
            self.operand(k, p, op)?;
        }
        self.finish()
    }

    fn operand(&mut self, k: u8, p: u8, op: &Operand) -> Option<()> {
        match k {
            b'*' | b'~' | b'%' | b';' | b'@' | b'!' | b'&' | b'$' | b'?' | b'/' | b'<' | b'>'
            | b'b' | b'p' | b'q' | b'v' | b'w' | b'y' | b'z' | b'|' => self.general(p, op),
            b'#' | b'^' => self.immediate(p, op),
            b'+' | b'-' | b'A' | b'a' => {
                let n = match op.mode {
                    Mode::PostInc(n) | Mode::PreDec(n) | Mode::AReg(n) | Mode::Ind(n) => n,
                    _ => 0,
                };
                self.install(p, n as u16);
                Some(())
            }
            b'D' => {
                if let Mode::DReg(n) = op.mode {
                    self.install(p, n as u16);
                }
                Some(())
            }
            b'F' => {
                if let Mode::FReg(n) = op.mode {
                    self.install(p, n as u16);
                }
                Some(())
            }
            b'R' => {
                let n = match op.mode {
                    Mode::DReg(n) => n,
                    Mode::AReg(n) => 8 | n,
                    _ => 0,
                };
                self.install(p, n as u16);
                Some(())
            }
            b'r' => {
                let n = match &op.mode {
                    Mode::Ind(n) => 8 | *n,
                    Mode::Indexed {
                        index: Some(ix), ..
                    } => ix.reg,
                    _ => 0,
                };
                self.install(p, n as u16);
                Some(())
            }
            b'B' => self.branch(p, op),
            b'd' => {
                let Mode::Indexed {
                    base: Base::A(n),
                    disp: Some(v),
                    ..
                } = op.mode
                else {
                    return Some(());
                };
                self.install(b's', n as u16);
                let d = match self.cx.constant(v.e) {
                    Some(d) if (-32768..=32767).contains(&d) => d,
                    Some(d) => {
                        return self
                            .err(v.span, format!("displacement {d} does not fit in a word"));
                    }
                    None => {
                        return self.err(
                            v.span,
                            "this displacement must be a constant, known where it is written",
                        );
                    }
                };
                self.appended
                    .push(Part::fixed((d as u16).to_be_bytes().to_vec()));
                Some(())
            }
            b'T' => {
                let v = self.value(op, 0, 15, "a vector")?;
                self.install(p, v as u16);
                Some(())
            }
            b't' => {
                let v = self.value(op, 0, 7, "a level")?;
                self.install(p, v as u16);
                Some(())
            }
            b'k' => {
                let v = self.value(op, -64, 63, "a k-factor")?;
                self.install(p, v as u16 & 0x7f);
                Some(())
            }
            b'x' => {
                let v = self.value(op, -1, 7, "a `mov3q` immediate")?;
                // -1 is written as 0.
                self.install(p, v.max(0) as u16);
                Some(())
            }
            b'M' => {
                let v = self.value(op, -128, 127, "an 8-bit immediate")?;
                self.install(p, v as u8 as u16);
                Some(())
            }
            b'l' | b'L' => self.reglist(k, p, op),
            b's' => {
                let v = match op.mode {
                    Mode::Ctl(rid::FPI) => 1,
                    Mode::Ctl(rid::FPS) => 2,
                    _ => 4,
                };
                self.install(p, v);
                Some(())
            }
            b'c' => {
                let v = match op.mode {
                    Mode::Ctl(rid::NC) => 0,
                    Mode::Ctl(rid::DC) => 1,
                    Mode::Ctl(rid::IC) => 2,
                    _ => 3,
                };
                self.install(p, v);
                Some(())
            }
            b'f' => {
                let v = u16::from(matches!(op.mode, Mode::Ctl(rid::DFC)));
                self.install(p, v);
                Some(())
            }
            b'0' | b'1' | b'2' => {
                let v = match op.mode {
                    Mode::Ctl(rid::CAL) => 4,
                    Mode::Ctl(rid::VAL) => 5,
                    Mode::Ctl(rid::SCC) => 6,
                    Mode::Ctl(rid::AC) => 7,
                    _ => 0,
                };
                self.install(p, v);
                Some(())
            }
            b'3' => {
                let v = if matches!(op.mode, Mode::Ctl(rid::TT1)) {
                    3
                } else {
                    2
                };
                self.install(p, v);
                Some(())
            }
            b'W' => {
                let v = match op.mode {
                    Mode::Ctl(rid::DRP) => 1,
                    Mode::Ctl(rid::SRP) => 2,
                    _ => 3,
                };
                self.install(p, v);
                Some(())
            }
            b'X' => {
                let v = match op.mode {
                    Mode::Ctl(id) if (rid::BAD0..=rid::BAD7).contains(&id) => {
                        4 << 10 | (id - rid::BAD0) << 2
                    }
                    Mode::Ctl(id) => 5 << 10 | (id - rid::BAC0) << 2,
                    _ => 0,
                };
                self.install(p, v);
                Some(())
            }
            b'_' => {
                let Mode::Abs(v) = op.mode else {
                    return Some(());
                };
                let (bytes, fixups) = match self.cx.constant(v.e) {
                    Some(n) => ((n as u32).to_be_bytes().to_vec(), Vec::new()),
                    None => (
                        vec![0; 4],
                        vec![Fixup {
                            offset: 0,
                            expr: v.e,
                            kind: FixupKind::data(4).with_reloc(reloc::R_68K_32),
                            span: v.span,
                        }],
                    ),
                };
                self.appended.push(Part::words(bytes, fixups));
                Some(())
            }
            // `C`, `S`, `U`, `V`, `Y` and `Z` only say which register it is.
            _ => Some(()),
        }
    }

    /// A general effective address, placed in the low six bits of the first
    /// word (or `MOVE`'s destination bits for place `d`), its extension words
    /// after the opcode's.
    fn general(&mut self, p: u8, op: &Operand) -> Option<()> {
        let (size, float) = match p {
            b'b' => (Sz::B, None),
            b'w' => (Sz::W, None),
            b'l' => (Sz::L, None),
            b'f' => (Sz::L, Some(Float::Single)),
            b'F' => (Sz::L, Some(Float::Double)),
            b'x' => (Sz::L, Some(Float::Extended)),
            b'p' => (Sz::L, Some(Float::Packed)),
            _ => {
                if matches!(op.mode, Mode::Imm(..) | Mode::FImm(..)) {
                    return self.err(op.span, "this instruction needs a size for an immediate");
                }
                (Sz::L, None)
            }
        };
        // An address stays absolute, as it does for the integer instructions:
        // GNU as would reach a label in the same section PC-relatively where
        // the operand is only read, which changes what the instruction does
        // once the code is moved.
        let ecx = EaCtx {
            cpu: self.cpu,
            size,
            float,
        };
        let alts = encode::ea(self.cx, op, ecx)?;
        let place = if p == b'd' {
            Place::MoveDst
        } else {
            Place::Low
        };
        self.appended.push(Part { alts, place });
        Some(())
    }

    /// `#` and `^`: an immediate written as extension words, or installed.
    fn immediate(&mut self, p: u8, op: &Operand) -> Option<()> {
        let (e, span) = match op.mode {
            Mode::Imm(e, span) => (e, span),
            _ => {
                return self.err(
                    op.span,
                    "a floating-point immediate needs a floating-point size",
                );
            }
        };
        let c = self.cx.constant(e);
        match p {
            b'b' | b'w' | b'W' | b'l' => {
                let size = if p == b'l' { Sz::L } else { Sz::W };
                if let Some(v) = c
                    && !fits_place(p, v)
                {
                    return self.err(span, format!("immediate {v} is out of range"));
                }
                let (bytes, fixups) = match c {
                    Some(v) if p == b'l' => ((v as u32).to_be_bytes().to_vec(), Vec::new()),
                    Some(v) => ((v as u16).to_be_bytes().to_vec(), Vec::new()),
                    // A byte's relocation is on the low byte of its word.
                    None if p == b'b' => encode::immediate(self.cx, e, Sz::B, span)?,
                    None => encode::immediate(self.cx, e, size, span)?,
                };
                self.inserted.insert(0, Part::words(bytes, fixups));
                Some(())
            }
            b'3' => {
                let v = self.value(op, i64::MIN, i64::MAX, "a register mask")?;
                self.install(p, v as u16 & 0xff);
                Some(())
            }
            b'C' => {
                let v = self.value(op, 0, 127, "a constant ROM offset")?;
                self.install(p, v as u16);
                Some(())
            }
            _ => Some(()),
        }
    }

    fn reglist(&mut self, k: u8, p: u8, op: &Operand) -> Option<()> {
        let mask: u32 = match op.mode {
            Mode::DReg(n) => 1 << n,
            Mode::AReg(n) => 1 << (8 + n),
            Mode::FReg(n) => 1 << (16 + n),
            Mode::Ctl(rid::FPI) => 1 << 24,
            Mode::Ctl(rid::FPS) => 1 << 25,
            Mode::Ctl(rid::FPC) => 1 << 26,
            Mode::RegList(m) => m,
            _ => 0,
        };
        match p {
            b'8' => {
                if mask & 0x0ff_ffff != 0 {
                    return self.err(
                        op.span,
                        "this register list holds only `fpcr`, `fpsr` and `fpiar`",
                    );
                }
                self.install(p, (mask >> 24) as u16);
            }
            _ => {
                if mask & 0x700_ffff != 0 {
                    return self.err(op.span, "this register list holds only `fp0`-`fp7`");
                }
                let m = (mask >> 16) as u8;
                self.install(p, if k == b'l' { m.reverse_bits() } else { m } as u16);
            }
        }
        Some(())
    }

    /// `B`: a coprocessor branch or decrement-and-branch displacement.
    fn branch(&mut self, p: u8, op: &Operand) -> Option<()> {
        let Mode::Abs(v) = op.mode else {
            return Some(());
        };
        if v.width.is_some() {
            return self.err(v.span, "a branch target takes no `.w` or `.l`");
        }
        let pc =
            |size: u8| FixupKind::pcrel(size, 0).with_reloc(reloc::data(size, true).unwrap_or(0));
        match p {
            b'W' | b'w' => {
                self.appended.push(Part::words(
                    vec![0, 0],
                    vec![Fixup {
                        offset: 0,
                        expr: v.e,
                        kind: pc(2),
                        span: v.span,
                    }],
                ));
            }
            b'C' => {
                self.appended.push(Part::words(
                    vec![0; 4],
                    vec![Fixup {
                        offset: 0,
                        expr: v.e,
                        kind: pc(4),
                        span: v.span,
                    }],
                ));
            }
            // `c`: word or long, whichever reaches. A number, which no
            // relaxation can place, is always the long form.
            _ => {
                if self.cx.constant(v.e).is_some() {
                    self.words[self.nwords - 1] |= 0x40;
                    return self.branch(b'C', op);
                }
                self.coproc_branch = Some((v.e, v.span));
            }
        }
        Some(())
    }

    fn finish(self) -> Option<Vec<Variant>> {
        let mut head = Vec::new();
        for w in &self.words[1..self.nwords] {
            head.extend_from_slice(&w.to_be_bytes());
        }
        if let Some((e, span)) = self.coproc_branch {
            // The two lengths differ in the opcode word's bit 6 as well as in
            // their extension, so they are built whole rather than as
            // alternatives of one part.
            let mut out = Vec::new();
            for long in [false, true] {
                let size = if long { 4 } else { 2 };
                let w0 = self.words[0] | if long { 0x40 } else { 0 };
                let mut bytes = w0.to_be_bytes().to_vec();
                bytes.extend_from_slice(&head);
                let offset = bytes.len() as u32;
                bytes.extend(std::iter::repeat_n(0, size as usize));
                out.push(Variant {
                    bytes,
                    fixups: vec![Fixup {
                        offset,
                        expr: e,
                        kind: FixupKind::pcrel(size, 0)
                            .with_reloc(reloc::data(size, true).unwrap_or(0)),
                        span,
                    }],
                });
            }
            return Some(out);
        }
        let mut parts = Vec::new();
        if !head.is_empty() {
            parts.push(Part::fixed(head));
        }
        parts.extend(self.inserted);
        parts.extend(self.appended);
        Some(build(self.words[0], parts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-written encoders and the table must not both claim a name,
    /// or which one runs would depend on the order they are asked in.
    #[test]
    fn table_and_hand_written_encoders_are_disjoint() {
        for form in table::FORMS {
            assert!(
                super::super::insn::resolve(form.name).is_err(),
                "`{}` is in the table and in insn.rs",
                form.name
            );
        }
        for (alias, _) in table::ALIASES {
            assert!(
                super::super::insn::resolve(alias).is_err(),
                "alias `{alias}` is in the table and in insn.rs"
            );
        }
    }

    #[test]
    fn tables_are_sorted() {
        assert!(table::FORMS.windows(2).all(|w| w[0].name <= w[1].name));
        assert!(table::ALIASES.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(table::HAND_ARCH.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn lookup() {
        assert_eq!(forms("faddx").map(<[Form]>::len), Some(2));
        assert!(forms("fmovm").is_some());
        assert!(forms("cas").is_some_and(|f| f[0].name == "casw"));
        assert!(forms("fadd").is_none());
    }
}
