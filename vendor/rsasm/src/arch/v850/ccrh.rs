//! Instructions in the syntax of Renesas CC-RH.
//!
//! CC-RH writes RH850 instructions much as GNU as does — `mov 5, r10`,
//! `ld.w 4[sp], r11` — but it is not a plain assembler. When an immediate or
//! displacement does not fit the instruction written, it *expands* the
//! instruction into a sequence that does the same thing: `mov 0x10, r10`
//! becomes `movea 0x10, r0, r10`, and `add 0x12345, r10` a 48-bit `mov` into
//! `r1` and `add r1, r10`. GNU as would instead widen `mov` to 48 bits and
//! truncate `add`'s operand. Every rule here is one of the tables of
//! R20UT3516EJ0113 §5.9, "Extension of Assembly Language" (pages 500-543),
//! and the equivalent GNU-syntax sequence is what `tools/xas-diff` checks it
//! against.
//!
//! The expansions depend on the operand's value, which is why a value must be
//! known when the instruction is read. A label is written with a sigil that
//! says what is wanted of it (Table 5.21, page 494); see
//! `operand::Parser::ccrh_reference` for how those map onto GNU as's
//! relocation functions. A bare label means its offset within its section,
//! which no ELF relocation expresses, except as a branch target, where it is
//! the usual PC-relative displacement.
//!
//! # Not covered
//!
//! - `$label` and `%label`, gp- and ep-relative references: the RH850 ELF ABI
//!   has no relocations for them, so nothing could carry them.
//! - The 32-bit PC-relative `jr32`/`jarl32` to a label, for the same reason.
//! - Expansions whose choice depends on which section a label lives in
//!   (`$label` again), or on `-Xasm_far_jump`, which is taken to be off.

use super::branch;
use super::insn::{Disp, Entry, ImmF, Slot};
use super::operand::{self, Arg, ArgKind, Imm, RelFn};
use super::reg::{self, EP, SP};
use super::reloc;
use crate::arch::{AsmCtx, InsnRequest};
use crate::section::Variant;
use crate::source::Span;

/// `ArchState::features` bit set by `$NOMACRO` and cleared by `$MACRO`.
pub const FEATURE_NOMACRO: u64 = 2;

const R0: u8 = 0;
/// The register CC-RH reserves for its expansions (page 492).
const R1: u8 = 1;

/// GNU as refuses `zdaoff()` in a displacement ("relocation used on an
/// instruction which does not support it"), so there is no checked GNU
/// equivalent of CC-RH's `!label[reg]` to produce.
const ZDA_DISPLACEMENT: &str = "`!label` cannot be a displacement: the RH850 ELF ABI has no 16-bit \
                                absolute relocation for one; write `#label[reg]`";

/// What the branch handling made of a mnemonic.
enum Branch {
    /// Not a branch.
    No,
    /// A branch that went into `steps` like any instruction.
    Steps,
    /// A branch whose candidates layout chooses between.
    Done(Vec<Variant>),
    /// An error was reported.
    Failed,
}

/// Which table entries an expanded instruction may use.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Force {
    Any,
    /// The 48-bit `mov` (`mov32`).
    Mov48,
    /// The 23-bit displacement of `ld23`/`st23`.
    Disp23,
    /// The 22-bit `jr`/`jarl`, or `jarl [reg]`.
    Disp22,
    /// The 48-bit `jr32`/`jarl32`.
    Disp32,
}

impl Force {
    fn allows(self, e: &Entry) -> bool {
        let has = |f: &dyn Fn(&Slot) -> bool| e.slots.iter().any(f);
        match self {
            Force::Any => true,
            Force::Mov48 => has(&|s| matches!(s, Slot::Imm(ImmF::Imm32))),
            Force::Disp23 => has(&|s| matches!(s, Slot::Mem(Disp::D23 { .. }, _))),
            Force::Disp22 => !has(&|s| matches!(s, Slot::Imm(ImmF::Disp32))),
            Force::Disp32 => has(&|s| matches!(s, Slot::Imm(ImmF::Disp32))),
        }
    }
}

/// One instruction of an expansion, in GNU as terms.
struct Step {
    mnemonic: &'static str,
    args: Vec<Arg>,
    force: Force,
}

/// An operand's value, as far as choosing an expansion needs it.
#[derive(Copy, Clone)]
enum Val {
    /// A number, as the 32-bit value CC-RH computes with.
    Const(i64),
    /// `#label`: a 32-bit address.
    Abs32(Imm),
    /// `!label`, or `HIGHW1`/`LOWW`/`HIGHW` of a label: a 16-bit value that
    /// needs no expansion.
    Short(Imm),
}

/// Assembles one CC-RH instruction.
pub fn assemble(
    cx: &mut AsmCtx<'_>,
    req: &InsnRequest<'_>,
    mnemonic: &str,
    rh850: bool,
) -> Option<Vec<Variant>> {
    // `pushsp rh, rt` is CC-RH's spelling of GNU as's `pushsp rh-rt` (page
    // 542); the two-operand form is what the table already has.
    let ranges = matches!(mnemonic, "pushsp" | "popsp" | "dbpush");
    let args = operand::parse_operands(cx, req.operands, req.span, ranges)?;
    let mut x = Expander {
        cx,
        span: req.span,
        mnemonic_span: req.mnemonic_span,
        rh850,
        steps: Vec::new(),
    };
    if let Some(variants) = x.expand(mnemonic, args)? {
        return Some(variants);
    }
    x.finish(mnemonic)
}

struct Expander<'c, 'a> {
    cx: &'c mut AsmCtx<'a>,
    span: Span,
    mnemonic_span: Span,
    rh850: bool,
    steps: Vec<Step>,
}

/// `v` read as the 32-bit value CC-RH evaluates expressions to (page 384),
/// sign-extended so that range checks see `0xFFFFFFFF` as -1.
fn as32(v: i64) -> i64 {
    v as u32 as i32 as i64
}

fn fits(v: i64, lo: i64, hi: i64) -> bool {
    (lo..=hi).contains(&v)
}

fn fits16(v: i64) -> bool {
    fits(v, -0x8000, 0x7fff)
}

fn fits5(v: i64) -> bool {
    fits(v, -16, 15)
}

/// The sign-extended low half, which is what a 16-bit field paired with
/// `HIGHW1` holds.
fn loww(v: i64) -> i64 {
    v as i16 as i64
}

fn highw(v: i64) -> i64 {
    (v >> 16) & 0xffff
}

fn highw1(v: i64) -> i64 {
    (((v >> 16) & 0xffff) + ((v >> 15) & 1)) & 0xffff
}

/// The condition code of `setfgt`, `cmovz` and the rest: Table 5.27 (page
/// 507), which the `cmov`, `sasf`, `adf` and `sbf` forms share.
fn condition(suffix: &str) -> Option<i64> {
    Some(match suffix {
        "gt" => 0xf,
        "ge" => 0xe,
        "lt" => 0x6,
        "le" => 0x7,
        "h" => 0xb,
        "nl" | "nc" => 0x9,
        "l" | "c" => 0x1,
        "nh" => 0x3,
        "e" | "z" => 0x2,
        "ne" | "nz" => 0xa,
        "v" => 0x0,
        "nv" => 0x8,
        "n" => 0x4,
        "p" => 0xc,
        "t" => 0x5,
        "sa" => 0xd,
        _ => return None,
    })
}

/// The floating-point condition of `cmpfeq.s` and the rest: Table 5.29
/// (page 543).
fn float_condition(suffix: &str) -> Option<i64> {
    Some(match suffix {
        "f" => 0x0,
        "un" => 0x1,
        "eq" => 0x2,
        "ueq" => 0x3,
        "olt" => 0x4,
        "ult" => 0x5,
        "ole" => 0x6,
        "ule" => 0x7,
        "sf" => 0x8,
        "ngle" => 0x9,
        "seq" => 0xa,
        "ngl" => 0xb,
        "lt" => 0xc,
        "nge" => 0xd,
        "le" => 0xe,
        "ngt" => 0xf,
        _ => return None,
    })
}

/// The registers of CC-RH's numeric `prepare`/`dispose` list, bit 11 first
/// (page 540): the order the instruction's own list field uses.
const LIST_ORDER: [u8; 12] = [30, 24, 25, 26, 27, 20, 21, 22, 23, 28, 29, 31];

/// The loads and stores, which share their displacement rules (page 501).
fn is_load_store(m: &str) -> Option<(&'static str, bool, bool)> {
    // (GNU mnemonic, is a store, 23-bit forced)
    Some(match m {
        "ld.b" | "ld23.b" => ("ld.b", false, m.starts_with("ld23")),
        "ld.bu" | "ld23.bu" => ("ld.bu", false, m.starts_with("ld23")),
        "ld.h" | "ld23.h" => ("ld.h", false, m.starts_with("ld23")),
        "ld.hu" | "ld23.hu" => ("ld.hu", false, m.starts_with("ld23")),
        "ld.w" | "ld23.w" => ("ld.w", false, m.starts_with("ld23")),
        "ld.dw" | "ld23.dw" => ("ld.dw", false, true),
        "st.b" | "st23.b" => ("st.b", true, m.starts_with("st23")),
        "st.h" | "st23.h" => ("st.h", true, m.starts_with("st23")),
        "st.w" | "st23.w" => ("st.w", true, m.starts_with("st23")),
        "st.dw" | "st23.dw" => ("st.dw", true, true),
        _ => return None,
    })
}

impl Expander<'_, '_> {
    fn error(&mut self, span: Span, msg: impl Into<String>) -> Option<()> {
        self.cx.error(span, msg);
        None
    }

    fn step(&mut self, mnemonic: &'static str, args: Vec<Arg>) {
        self.steps.push(Step {
            mnemonic,
            args,
            force: Force::Any,
        });
    }

    fn forced(&mut self, mnemonic: &'static str, args: Vec<Arg>, force: Force) {
        self.steps.push(Step {
            mnemonic,
            args,
            force,
        });
    }

    fn reg(&self, r: u8) -> Arg {
        Arg {
            kind: ArgKind::Reg(r),
            span: self.span,
        }
    }

    fn num_imm(&mut self, v: i64) -> Imm {
        Imm {
            expr: self.cx.exprs.int(v as u64, self.span),
            func: RelFn::None,
            ident: None,
            span: self.span,
        }
    }

    fn num(&mut self, v: i64) -> Arg {
        let imm = self.num_imm(v);
        Arg {
            kind: ArgKind::Imm(imm),
            span: self.span,
        }
    }

    fn imm(&self, imm: Imm) -> Arg {
        Arg {
            kind: ArgKind::Imm(imm),
            span: imm.span,
        }
    }

    fn with(&self, imm: Imm, func: RelFn) -> Arg {
        self.imm(Imm { func, ..imm })
    }

    fn mem(&self, disp: Imm, base: u8) -> Arg {
        Arg {
            kind: ArgKind::Mem { disp, base },
            span: disp.span,
        }
    }

    /// The value of an operand that selects an expansion.
    fn value(&mut self, arg: &Arg) -> Option<Val> {
        let ArgKind::Imm(imm) = arg.kind else {
            self.error(
                arg.span,
                format!("expected an immediate, but found {}", arg.describe()),
            )?;
            unreachable!()
        };
        match imm.func {
            RelFn::None | RelFn::HiLo | RelFn::ZdaOff => {
                if let Some(v) = self.cx.constant(imm.expr) {
                    return Some(Val::Const(as32(v)));
                }
            }
            _ => {}
        }
        match imm.func {
            RelFn::HiLo => Some(Val::Abs32(imm)),
            RelFn::ZdaOff | RelFn::Hi | RelFn::Lo | RelFn::Hi0 => Some(Val::Short(imm)),
            RelFn::None => {
                self.cx.error(
                    imm.span,
                    "CC-RH chooses how to assemble this from the operand's value, so it must be a \
                     constant defined before this line; for a label, write `#label` for its \
                     address or `!label` for a 16-bit one",
                );
                None
            }
            other => {
                self.cx.error(
                    imm.span,
                    format!("`{}` is GNU as syntax, not CC-RH", other.spelling()),
                );
                None
            }
        }
    }

    /// Loads a value that is too wide for 16 bits into `x`: `movhi` when its
    /// low half is zero, else the 48-bit `mov`.
    fn load_wide(&mut self, v: i64, x: u8) {
        if v & 0xffff == 0 {
            let hi = self.num(highw(v));
            let (r0, rx) = (self.reg(R0), self.reg(x));
            self.step("movhi", vec![hi, r0, rx]);
        } else {
            let k = self.num(v);
            let rx = self.reg(x);
            self.forced("mov", vec![k, rx], Force::Mov48);
        }
    }

    /// Loads any value into `x` the way the operation instructions' tables
    /// do: a 5-bit `mov` if `short` allows it, `movea` from `r0` for 16
    /// bits, then [`Self::load_wide`]. `#label` is a 48-bit `mov`, and a
    /// 16-bit reference a `movea`.
    fn load(&mut self, v: Val, x: u8, short: bool) {
        match v {
            Val::Const(v) if short && fits5(v) => {
                let (k, rx) = (self.num(v), self.reg(x));
                self.step("mov", vec![k, rx]);
            }
            Val::Const(v) if fits16(v) => {
                let (k, r0, rx) = (self.num(v), self.reg(R0), self.reg(x));
                self.step("movea", vec![k, r0, rx]);
            }
            Val::Const(v) => self.load_wide(v, x),
            Val::Short(imm) => {
                let (k, r0, rx) = (self.imm(imm), self.reg(R0), self.reg(x));
                self.step("movea", vec![k, r0, rx]);
            }
            Val::Abs32(imm) => {
                let (k, rx) = (self.imm(imm), self.reg(x));
                self.forced("mov", vec![k, rx], Force::Mov48);
            }
        }
    }

    fn reg_of(&mut self, arg: &Arg) -> Option<u8> {
        match arg.kind {
            ArgKind::Reg(r) => Some(r),
            _ => {
                self.error(
                    arg.span,
                    format!("expected a register, but found {}", arg.describe()),
                )?;
                None
            }
        }
    }

    fn arity(&mut self, m: &str, args: &[Arg], n: usize) -> Option<()> {
        if args.len() != n {
            return self.error(
                self.span,
                format!("`{m}` takes {n} operand(s), but {} were given", args.len()),
            );
        }
        Some(())
    }

    /// Expands `m`. Returns `Some(Some(variants))` for a branch, whose
    /// candidates layout chooses between, and `Some(None)` once `steps` holds
    /// the instructions to assemble.
    fn expand(&mut self, m: &str, mut args: Vec<Arg>) -> Option<Option<Vec<Variant>>> {
        // ---- condition-suffixed spellings (pages 507, 519 and 543) ---------
        for (prefix, base) in [
            ("setf", "setf"),
            ("sasf", "sasf"),
            ("adf", "adf"),
            ("sbf", "sbf"),
            ("cmov", "cmov"),
        ] {
            if let Some(suffix) = m.strip_prefix(prefix)
                && !suffix.is_empty()
                && let Some(cc) = condition(suffix)
            {
                if cc == 0xd && matches!(base, "adf" | "sbf") {
                    self.error(
                        self.mnemonic_span,
                        format!("`{m}`: `adf` and `sbf` cannot test `sa`"),
                    )?;
                }
                let k = self.num(cc);
                args.insert(0, k);
                return self.expand(base, args);
            }
        }
        if let Some(rest) = m.strip_prefix("cmpf")
            && let Some((suffix, width)) = rest.rsplit_once('.')
            && let Some(cc) = float_condition(suffix)
            && matches!(width, "s" | "d")
        {
            let k = self.num(cc);
            args.insert(0, k);
            let base = if width == "s" { "cmpf.s" } else { "cmpf.d" };
            self.step(base, args);
            return Some(None);
        }

        // ---- branches (pages 533-537) --------------------------------------
        match self.branch(m, &args) {
            Branch::No => {}
            Branch::Steps => return Some(None),
            Branch::Done(v) => return Some(Some(v)),
            Branch::Failed => return None,
        }

        if let Some((base, store, d23)) = is_load_store(m) {
            self.load_store(base, store, d23, args)?;
            return Some(None);
        }

        match m {
            "mov" | "mov32" => self.mov(m, args)?,
            "movea" => self.movea(args)?,
            "add" | "mulh" => self.add(m, args)?,
            "addi" | "mulhi" => self.addi(m, args)?,
            "cmp" | "satadd" if args.len() == 2 => self.via_r1(m, args, true)?,
            "mul" | "mulu" if args.len() == 3 => self.mul(m, args)?,
            "divh" if args.len() == 2 => self.div2(args)?,
            "divh" | "div" | "divhu" | "divu" if args.len() == 3 => self.div3(m, args)?,
            "satsub" if args.len() == 2 => self.satsub(args)?,
            "satsubi" => self.satsubi(args)?,
            "and" | "or" | "xor" => self.logic(m, args)?,
            "andi" | "ori" | "xori" => self.logic_imm(m, args)?,
            "not" | "satsubr" | "sub" | "subr" | "tst" => self.unary_like(m, args)?,
            "cmov" => self.cmov(args)?,
            "set1" | "clr1" | "not1" | "tst1" => self.bit(m, args)?,
            "sld.b" | "sld.bu" | "sld.h" | "sld.hu" | "sld.w" | "sst.b" | "sst.h" | "sst.w" => {
                self.short_ep(m, args)?
            }
            "push" | "pop" => self.push_pop(m, args)?,
            "pushm" | "popm" => self.push_pop_many(m, args)?,
            "prepare" => self.prepare(args)?,
            "dispose" => self.dispose(args)?,
            _ => {
                let name = self.static_name(m)?;
                self.step(name, args);
            }
        }
        Some(None)
    }

    /// A `&'static str` for a mnemonic the table knows.
    fn static_name(&mut self, m: &str) -> Option<&'static str> {
        match super::insn::entries(m).next() {
            Some(e) => Some(e.name),
            None => {
                self.error(self.mnemonic_span, format!("unknown instruction `{m}`"))?;
                None
            }
        }
    }

    /// `mov imm, reg` (page 517), and `mov32`, which always takes 48 bits.
    fn mov(&mut self, m: &str, args: Vec<Arg>) -> Option<()> {
        self.arity(m, &args, 2)?;
        if matches!(args[0].kind, ArgKind::Reg(_)) {
            if m == "mov32" {
                return self.error(args[0].span, "`mov32` takes an immediate");
            }
            self.step("mov", args);
            return Some(());
        }
        let reg = self.reg_of(&args[1])?;
        let v = self.value(&args[0])?;
        let rx = self.reg(reg);
        match v {
            // `r0` as the destination, and `mov32`, always get the 48-bit form.
            Val::Const(v) if m == "mov32" || reg == R0 => {
                let k = self.num(v);
                self.forced("mov", vec![k, rx], Force::Mov48);
            }
            Val::Abs32(imm) => self.forced("mov", vec![self.imm(imm), rx], Force::Mov48),
            _ if m == "mov32" || reg == R0 => {
                return self.error(
                    args[0].span,
                    "a 16-bit reference cannot be the 32-bit immediate of the 48-bit `mov`",
                );
            }
            Val::Const(_)
            | Val::Short(Imm {
                func: RelFn::ZdaOff,
                ..
            }) => self.load(v, reg, true),
            Val::Short(imm) => {
                return self.error(
                    imm.span,
                    "a separated half of a label cannot be moved with `mov`; use `movhi` or `movea`",
                );
            }
        }
        Some(())
    }

    /// `movea imm, reg1, reg2` (page 518).
    fn movea(&mut self, args: Vec<Arg>) -> Option<()> {
        self.arity("movea", &args, 3)?;
        let v = self.value(&args[0])?;
        let (r1, r2) = (self.reg_of(&args[1])?, self.reg_of(&args[2])?);
        match v {
            Val::Const(v) if !fits16(v) => {
                let (reg1, reg2) = (self.reg(r1), self.reg(r2));
                if v & 0xffff == 0 {
                    let hi = self.num(highw(v));
                    self.step("movhi", vec![hi, reg1, reg2]);
                } else {
                    let (hi, lo) = (self.num(highw1(v)), self.num(loww(v)));
                    let (tmp, tmp2) = (self.reg(R1), self.reg(R1));
                    self.step("movhi", vec![hi, reg1, tmp]);
                    self.step("movea", vec![lo, tmp2, reg2]);
                }
            }
            Val::Abs32(imm) => {
                let (reg1, reg2, tmp) = (self.reg(r1), self.reg(r2), self.reg(R1));
                self.step("movhi", vec![self.with(imm, RelFn::Hi), reg1, tmp]);
                self.step("movea", vec![self.with(imm, RelFn::Lo), tmp, reg2]);
            }
            Val::Const(v) => {
                let (k, reg1, reg2) = (self.num(v), self.reg(r1), self.reg(r2));
                self.step("movea", vec![k, reg1, reg2]);
            }
            Val::Short(imm) => {
                let (reg1, reg2) = (self.reg(r1), self.reg(r2));
                self.step("movea", vec![self.imm(imm), reg1, reg2]);
            }
        }
        Some(())
    }

    /// `add imm, reg` and `mulh imm, reg` (page 504).
    fn add(&mut self, m: &str, args: Vec<Arg>) -> Option<()> {
        self.arity(m, &args, 2)?;
        if matches!(args[0].kind, ArgKind::Reg(_)) {
            self.step(if m == "add" { "add" } else { "mulh" }, args);
            return Some(());
        }
        let (op, opi) = if m == "add" {
            ("add", "addi")
        } else {
            ("mulh", "mulhi")
        };
        let reg = self.reg_of(&args[1])?;
        let v = self.value(&args[0])?;
        let rx = self.reg(reg);
        match v {
            Val::Const(v) if fits5(v) => {
                let k = self.num(v);
                self.step(op, vec![k, rx]);
            }
            Val::Const(v) if fits16(v) => {
                let (k, rx2) = (self.num(v), self.reg(reg));
                self.step(opi, vec![k, rx, rx2]);
            }
            Val::Short(imm) => self.step(opi, vec![self.imm(imm), rx, self.reg(reg)]),
            _ => {
                self.load(v, R1, false);
                self.step(op, vec![self.reg(R1), rx]);
            }
        }
        Some(())
    }

    /// `addi imm, reg1, reg2` and `mulhi` (pages 505-506).
    fn addi(&mut self, m: &str, args: Vec<Arg>) -> Option<()> {
        self.arity(m, &args, 3)?;
        let (opi, op) = if m == "addi" {
            ("addi", "add")
        } else {
            ("mulhi", "mulh")
        };
        let v = self.value(&args[0])?;
        let (r1, r2) = (self.reg_of(&args[1])?, self.reg_of(&args[2])?);
        match v {
            Val::Const(v) if fits16(v) => {
                let (k, a, b) = (self.num(v), self.reg(r1), self.reg(r2));
                self.step(opi, vec![k, a, b]);
            }
            Val::Short(imm) => {
                let (a, b) = (self.reg(r1), self.reg(r2));
                self.step(opi, vec![self.imm(imm), a, b]);
            }
            _ => {
                if r2 == R0 && m == "mulhi" {
                    return self.error(args[2].span, "`mulhi` cannot store its result in r0");
                }
                self.three_operand(v, r1, r2, op, false)?;
            }
        }
        Some(())
    }

    /// The shape the three-operand expansions share (pages 505, 524 and
    /// 528): the value goes into `r1` when the destination is `r0` or the
    /// source, and straight into the destination otherwise.
    ///
    /// `rev` is `satsubi`'s variant, which has no `r0` case and computes
    /// `reg1 - value` with `satsubr` instead of `satsub`.
    fn three_operand(
        &mut self,
        v: Val,
        r1: u8,
        r2: u8,
        op: &'static str,
        short: bool,
    ) -> Option<()> {
        let tmp = if r2 == R0 || r2 == r1 { R1 } else { r2 };
        self.load_for_three(v, tmp, short);
        let (a, b) = match (r2 == R0, r2 == r1) {
            (true, _) => (self.reg(r1), self.reg(R1)),
            (false, true) => (self.reg(R1), self.reg(r2)),
            (false, false) => (self.reg(r1), self.reg(r2)),
        };
        self.step(op, vec![a, b]);
        Some(())
    }

    /// The load of the three-operand expansions: [`Self::load`] without its
    /// 16-bit `movea`, which `andi` does use, hence `short`.
    fn load_for_three(&mut self, v: Val, x: u8, short: bool) {
        match v {
            Val::Const(v) if short => self.load(Val::Const(v), x, true),
            Val::Const(v) => self.load_wide(v, x),
            other => self.load(other, x, false),
        }
    }

    /// `cmp imm, reg` (page 516) and `satadd imm, reg` (page 521): the 5-bit
    /// form, else the value in `r1`.
    fn via_r1(&mut self, m: &str, args: Vec<Arg>, five: bool) -> Option<()> {
        let name = self.static_name(m)?;
        if matches!(args[0].kind, ArgKind::Reg(_)) {
            self.step(name, args);
            return Some(());
        }
        let v = self.value(&args[0])?;
        let reg = self.reg_of(&args[1])?;
        match v {
            Val::Const(v) if five && fits5(v) => {
                let (k, rx) = (self.num(v), self.reg(reg));
                self.step(name, vec![k, rx]);
            }
            _ => {
                self.load(v, R1, false);
                let (a, b) = (self.reg(R1), self.reg(reg));
                self.step(name, vec![a, b]);
            }
        }
        Some(())
    }

    /// `mul imm9, reg2, reg3` (page 508) and `mulu` (pages 509-510).
    fn mul(&mut self, m: &str, args: Vec<Arg>) -> Option<()> {
        let name = self.static_name(m)?;
        if matches!(args[0].kind, ArgKind::Reg(_)) {
            self.step(name, args);
            return Some(());
        }
        let v = self.value(&args[0])?;
        let (r2, r3) = (self.reg_of(&args[1])?, self.reg_of(&args[2])?);
        let (lo, hi) = if m == "mul" { (-256, 255) } else { (0, 511) };
        match v {
            Val::Const(v) if fits(v, lo, hi) => {
                let (k, a, b) = (self.num(v), self.reg(r2), self.reg(r3));
                self.step(name, vec![k, a, b]);
            }
            _ => {
                // `mulu` puts -16 to -1 in `r1` with the 5-bit `mov`.
                self.load(v, R1, m == "mulu");
                let (t, a, b) = (self.reg(R1), self.reg(r2), self.reg(r3));
                self.step(name, vec![t, a, b]);
            }
        }
        Some(())
    }

    /// `divh imm, reg` (pages 511-512).
    fn div2(&mut self, args: Vec<Arg>) -> Option<()> {
        if matches!(args[0].kind, ArgKind::Reg(_)) {
            self.step("divh", args);
            return Some(());
        }
        let v = self.value(&args[0])?;
        let reg = self.reg_of(&args[1])?;
        if let Val::Const(0) = v {
            return self.error(args[0].span, "`divh 0, reg` divides by zero");
        }
        self.load(v, R1, true);
        let (t, b) = (self.reg(R1), self.reg(reg));
        self.step("divh", vec![t, b]);
        Some(())
    }

    /// `divh imm, reg2, reg3`, `div`, `divhu` and `divu` (pages 512-515): 0 is
    /// `r0`, anything else goes through `r1`.
    fn div3(&mut self, m: &str, args: Vec<Arg>) -> Option<()> {
        let name = self.static_name(m)?;
        if matches!(args[0].kind, ArgKind::Reg(_)) {
            self.step(name, args);
            return Some(());
        }
        let v = self.value(&args[0])?;
        let (r2, r3) = (self.reg_of(&args[1])?, self.reg_of(&args[2])?);
        let t = if let Val::Const(0) = v {
            R0
        } else {
            self.load(v, R1, true);
            R1
        };
        let (t, a, b) = (self.reg(t), self.reg(r2), self.reg(r3));
        self.step(name, vec![t, a, b]);
        Some(())
    }

    /// `satsub imm, reg` (pages 522-523).
    fn satsub(&mut self, args: Vec<Arg>) -> Option<()> {
        if matches!(args[0].kind, ArgKind::Reg(_)) {
            self.step("satsub", args);
            return Some(());
        }
        let v = self.value(&args[0])?;
        let reg = self.reg_of(&args[1])?;
        let rx = self.reg(reg);
        match v {
            Val::Const(0) => self.step("satsub", vec![self.reg(R0), rx]),
            Val::Const(v) if fits16(v) => {
                let k = self.num(v);
                self.step("satsubi", vec![k, rx, self.reg(reg)]);
            }
            Val::Short(imm) => self.step("satsubi", vec![self.imm(imm), rx, self.reg(reg)]),
            _ => {
                self.load(v, R1, false);
                self.step("satsub", vec![self.reg(R1), rx]);
            }
        }
        Some(())
    }

    /// `satsubi imm, reg1, reg2` (pages 524-525).
    fn satsubi(&mut self, args: Vec<Arg>) -> Option<()> {
        self.arity("satsubi", &args, 3)?;
        let v = self.value(&args[0])?;
        let (r1, r2) = (self.reg_of(&args[1])?, self.reg_of(&args[2])?);
        match v {
            Val::Const(v) if fits16(v) => {
                let (k, a, b) = (self.num(v), self.reg(r1), self.reg(r2));
                self.step("satsubi", vec![k, a, b]);
            }
            Val::Short(imm) => {
                let (a, b) = (self.reg(r1), self.reg(r2));
                self.step("satsubi", vec![self.imm(imm), a, b]);
            }
            _ if r2 == r1 => {
                self.load_for_three(v, R1, false);
                let (a, b) = (self.reg(R1), self.reg(r2));
                self.step("satsub", vec![a, b]);
            }
            _ => {
                self.load_for_three(v, r2, false);
                let (a, b) = (self.reg(r1), self.reg(r2));
                self.step("satsubr", vec![a, b]);
            }
        }
        Some(())
    }

    /// `and imm, reg`, `or` and `xor` (pages 526-527).
    fn logic(&mut self, m: &str, args: Vec<Arg>) -> Option<()> {
        let name = self.static_name(m)?;
        if matches!(args[0].kind, ArgKind::Reg(_)) {
            self.step(name, args);
            return Some(());
        }
        self.arity(m, &args, 2)?;
        let opi = match m {
            "and" => "andi",
            "or" => "ori",
            _ => "xori",
        };
        let v = self.value(&args[0])?;
        let reg = self.reg_of(&args[1])?;
        let rx = self.reg(reg);
        match v {
            Val::Const(0) => self.step(name, vec![self.reg(R0), rx]),
            Val::Const(v) if fits(v, 1, 0xffff) => {
                let k = self.num(v);
                self.step(opi, vec![k, rx, self.reg(reg)]);
            }
            Val::Short(imm) => self.step(opi, vec![self.imm(imm), rx, self.reg(reg)]),
            _ => {
                self.load(v, R1, true);
                self.step(name, vec![self.reg(R1), rx]);
            }
        }
        Some(())
    }

    /// `andi imm, reg1, reg2`, `ori` and `xori` (pages 528-530).
    fn logic_imm(&mut self, m: &str, args: Vec<Arg>) -> Option<()> {
        self.arity(m, &args, 3)?;
        let (opi, op) = match m {
            "andi" => ("andi", "and"),
            "ori" => ("ori", "or"),
            _ => ("xori", "xor"),
        };
        let v = self.value(&args[0])?;
        let (r1, r2) = (self.reg_of(&args[1])?, self.reg_of(&args[2])?);
        match v {
            Val::Const(v) if fits(v, 0, 0xffff) => {
                let (k, a, b) = (self.num(v), self.reg(r1), self.reg(r2));
                self.step(opi, vec![k, a, b]);
            }
            Val::Short(imm) => {
                let (a, b) = (self.reg(r1), self.reg(r2));
                self.step(opi, vec![self.imm(imm), a, b]);
            }
            _ => self.three_operand(v, r1, r2, op, true)?,
        }
        Some(())
    }

    /// `not imm, reg`, `satsubr`, `sub`, `subr` and `tst` (pages 531-532).
    fn unary_like(&mut self, m: &str, args: Vec<Arg>) -> Option<()> {
        let name = self.static_name(m)?;
        if matches!(args[0].kind, ArgKind::Reg(_)) {
            self.step(name, args);
            return Some(());
        }
        self.arity(m, &args, 2)?;
        let v = self.value(&args[0])?;
        let reg = self.reg_of(&args[1])?;
        let t = if let Val::Const(0) = v {
            R0
        } else {
            self.load(v, R1, true);
            R1
        };
        let (t, rx) = (self.reg(t), self.reg(reg));
        self.step(name, vec![t, rx]);
        Some(())
    }

    /// `cmov cccc, imm, reg2, reg3` (pages 519-520).
    fn cmov(&mut self, args: Vec<Arg>) -> Option<()> {
        self.arity("cmov", &args, 4)?;
        if matches!(args[1].kind, ArgKind::Reg(_)) {
            self.step("cmov", args);
            return Some(());
        }
        let v = self.value(&args[1])?;
        match v {
            Val::Const(v) if fits5(v) => {
                let mut args = args;
                args[1] = self.num(v);
                self.step("cmov", args);
            }
            _ => {
                self.load(v, R1, false);
                let mut args = args;
                args[1] = self.reg(R1);
                self.step("cmov", args);
            }
        }
        Some(())
    }

    /// A `disp[reg]` operand, filling in what CC-RH lets the source leave
    /// out: a missing displacement is 0, and a missing `[reg]` is `[r0]`
    /// (page 501).
    fn memory(&mut self, arg: &Arg) -> Option<(Imm, u8)> {
        match arg.kind {
            ArgKind::Mem { disp, base } => Some((disp, base)),
            ArgKind::Bracket(base) => Some((self.num_imm(0), base)),
            ArgKind::Imm(imm) => Some((imm, R0)),
            _ => {
                self.error(
                    arg.span,
                    format!("expected a memory operand, but found {}", arg.describe()),
                )?;
                None
            }
        }
    }

    /// `ld.*`, `st.*` and their 23-bit `ld23`/`st23` spellings (pages
    /// 501-502).
    fn load_store(
        &mut self,
        m: &'static str,
        store: bool,
        d23: bool,
        args: Vec<Arg>,
    ) -> Option<()> {
        self.arity(m, &args, 2)?;
        let (mem_i, reg_i) = if store { (1, 0) } else { (0, 1) };
        let reg = self.reg_of(&args[reg_i])?;
        let (disp, base) = self.memory(&args[mem_i])?;
        let darg = self.imm(disp);
        let v = self.value(&darg)?;
        let emit = |x: &mut Self, disp: Arg, base: u8, force: Force| {
            let ArgKind::Imm(d) = disp.kind else {
                unreachable!()
            };
            let mem = x.mem(d, base);
            let r = x.reg(reg);
            let args = if store { vec![r, mem] } else { vec![mem, r] };
            x.forced(m, args, force);
        };
        match v {
            Val::Const(v) if d23 => {
                if !fits(v, -0x40_0000, 0x3f_ffff) {
                    return self.error(disp.span, format!("displacement {v} does not fit 23 bits"));
                }
                let k = self.num(v);
                emit(self, k, base, Force::Disp23);
            }
            Val::Const(v) if fits16(v) => {
                let k = self.num(v);
                emit(self, k, base, Force::Any);
            }
            Val::Const(v) if fits(v, -0x40_0000, 0x3f_ffff) => {
                let k = self.num(v);
                emit(self, k, base, Force::Disp23);
            }
            Val::Const(v) => {
                let (hi, b, t) = (self.num(highw1(v)), self.reg(base), self.reg(R1));
                self.step("movhi", vec![hi, b, t]);
                let lo = self.num(loww(v));
                emit(self, lo, R1, Force::Any);
            }
            Val::Abs32(imm) => {
                let (b, t) = (self.reg(base), self.reg(R1));
                self.step("movhi", vec![self.with(imm, RelFn::Hi), b, t]);
                let lo = self.with(imm, RelFn::Lo);
                emit(self, lo, R1, Force::Any);
            }
            Val::Short(imm) if imm.func == RelFn::ZdaOff => {
                return self.error(imm.span, ZDA_DISPLACEMENT);
            }
            Val::Short(imm) => {
                let k = self.imm(imm);
                emit(self, k, base, if d23 { Force::Disp23 } else { Force::Any });
            }
        }
        Some(())
    }

    /// `set1 bit#3, disp[reg]` and the other bit instructions (page 538).
    fn bit(&mut self, m: &str, args: Vec<Arg>) -> Option<()> {
        let name = self.static_name(m)?;
        self.arity(m, &args, 2)?;
        if matches!(args[0].kind, ArgKind::Reg(_)) {
            self.step(name, args);
            return Some(());
        }
        let (disp, base) = self.memory(&args[1])?;
        let darg = self.imm(disp);
        let v = self.value(&darg)?;
        let bit = args[0];
        match v {
            Val::Const(v) if fits16(v) => {
                let k = self.num_imm(v);
                self.step(name, vec![bit, self.mem(k, base)]);
            }
            Val::Const(v) => {
                let (hi, b, t) = (self.num(highw1(v)), self.reg(base), self.reg(R1));
                self.step("movhi", vec![hi, b, t]);
                let lo = self.num_imm(loww(v));
                self.step(name, vec![bit, self.mem(lo, R1)]);
            }
            Val::Abs32(imm) => {
                let (b, t) = (self.reg(base), self.reg(R1));
                self.step("movhi", vec![self.with(imm, RelFn::Hi), b, t]);
                let lo = Imm {
                    func: RelFn::Lo,
                    ..imm
                };
                self.step(name, vec![bit, self.mem(lo, R1)]);
            }
            Val::Short(imm) if imm.func == RelFn::ZdaOff => {
                return self.error(imm.span, ZDA_DISPLACEMENT);
            }
            Val::Short(imm) => self.step(name, vec![bit, self.mem(imm, base)]),
        }
        Some(())
    }

    /// `sld.*`/`sst.*`, whose `[ep]` may be left out (page 503).
    fn short_ep(&mut self, m: &str, mut args: Vec<Arg>) -> Option<()> {
        let name = self.static_name(m)?;
        let i = if m.starts_with("sst") { 1 } else { 0 };
        if let Some(Arg {
            kind: ArgKind::Imm(imm),
            ..
        }) = args.get(i).copied()
        {
            args[i] = self.mem(imm, EP);
        }
        self.step(name, args);
        Some(())
    }

    /// `push`/`pop`, which the device does not have (page 539).
    fn push_pop(&mut self, m: &str, args: Vec<Arg>) -> Option<()> {
        self.arity(m, &args, 1)?;
        let reg = self.reg_of(&args[0])?;
        let (sp, r) = (self.reg(SP), self.reg(reg));
        let zero = self.num_imm(0);
        if m == "push" {
            let k = self.num(-4);
            self.step("add", vec![k, sp]);
            self.step("st.w", vec![r, self.mem(zero, SP)]);
        } else {
            self.step("ld.w", vec![self.mem(zero, SP), r]);
            let k = self.num(4);
            self.step("add", vec![k, sp]);
        }
        Some(())
    }

    /// `pushm`/`popm` (page 539).
    fn push_pop_many(&mut self, m: &str, args: Vec<Arg>) -> Option<()> {
        if args.is_empty() {
            return self.error(self.span, format!("`{m}` needs at least one register"));
        }
        let mut regs = Vec::with_capacity(args.len());
        for a in &args {
            regs.push(self.reg_of(a)?);
        }
        let n = regs.len() as i64;
        let sp = self.reg(SP);
        if m == "pushm" {
            let k = self.num(-4 * n);
            self.step("addi", vec![k, sp, sp]);
            for (i, &r) in regs.iter().enumerate().rev() {
                let d = self.num_imm(4 * i as i64);
                let r = self.reg(r);
                self.step("st.w", vec![r, self.mem(d, SP)]);
            }
        } else {
            for (i, &r) in regs.iter().enumerate() {
                let d = self.num_imm(4 * i as i64);
                let r = self.reg(r);
                self.step("ld.w", vec![self.mem(d, SP), r]);
            }
            let k = self.num(4 * n);
            self.step("addi", vec![k, sp, sp]);
        }
        Some(())
    }

    /// A CC-RH register list: registers separated by commas, or a 12-bit
    /// number in the instruction's own bit order (page 540). Returns the list
    /// in GNU as's form, bit `n` for `rn`.
    fn list(&mut self, args: &[Arg]) -> Option<Arg> {
        let span = args.first().map_or(self.span, |a| a.span);
        let mut mask = 0u32;
        match args {
            [
                Arg {
                    kind: ArgKind::List(m),
                    ..
                },
            ] => mask = *m,
            [
                Arg {
                    kind: ArgKind::Imm(imm),
                    ..
                },
            ] => {
                let Some(v) = self.cx.constant(imm.expr) else {
                    self.error(imm.span, "a register list must be a constant")?;
                    return None;
                };
                if !(0..=0xfff).contains(&v) {
                    self.cx.diags.warning(
                        imm.span,
                        format!(
                            "register list {v:#x} is wider than 12 bits; only the low 12 are used"
                        ),
                    );
                }
                for (i, r) in LIST_ORDER.iter().enumerate() {
                    if v & (1 << (11 - i)) != 0 {
                        mask |= 1 << r;
                    }
                }
            }
            _ => {
                for a in args {
                    let r = self.reg_of(a)?;
                    if r < 20 {
                        // Page 541: a register the instruction cannot save is
                        // warned about and left out.
                        self.cx.diags.warning(
                            a.span,
                            format!(
                                "`{}` cannot be in a register list, and is ignored",
                                reg::gpr_name(r)
                            ),
                        );
                        continue;
                    }
                    mask |= 1 << r;
                }
            }
        }
        Some(Arg {
            kind: ArgKind::List(mask),
            span,
        })
    }

    /// The stack adjustment of `prepare`/`dispose`, in bytes. Up to 127 fits
    /// the instruction, stored as words; anything larger is left to an
    /// instruction of its own (page 541). Returns the instruction's operand.
    fn frame(&mut self, arg: &Arg) -> Option<(Arg, Option<i64>)> {
        let ArgKind::Imm(imm) = arg.kind else {
            self.error(arg.span, "expected the stack frame size")?;
            return None;
        };
        let Some(v) = self.cx.constant(imm.expr) else {
            self.error(imm.span, "the stack frame size must be a constant")?;
            return None;
        };
        if !(0..=0xffff_ffff).contains(&v) {
            self.error(imm.span, format!("stack frame size {v} is out of range"))?;
        }
        if v > 127 {
            let zero = self.num(0);
            return Some((zero, Some(v)));
        }
        if v % 4 != 0 {
            self.cx.diags.warning(
                imm.span,
                format!(
                    "stack frame size {v} is not a multiple of 4; its low two bits are ignored"
                ),
            );
        }
        Some((self.num(v >> 2), None))
    }

    /// `prepare list, imm1[, imm2 | sp]` (pages 540-541).
    fn prepare(&mut self, args: Vec<Arg>) -> Option<()> {
        // The list is the registers before the frame size, or a number
        // written in their place.
        let n = match args.first().map(|a| a.kind) {
            Some(ArgKind::Imm(_)) => 1,
            _ => args
                .iter()
                .take_while(|a| matches!(a.kind, ArgKind::Reg(_) | ArgKind::List(_)))
                .count(),
        };
        if n == 0 || n >= args.len() {
            return self.error(
                self.span,
                "`prepare` needs a register list and a stack frame size",
            );
        };
        let list = self.list(&args[..n])?;
        let (frame, extra) = self.frame(&args[n])?;
        let rest = &args[n + 1..];
        if extra.is_some() && matches!(rest.first().map(|a| a.kind), Some(ArgKind::Reg(SP))) {
            return self.error(
                args[n].span,
                "with `sp` as its last operand, `prepare` takes a stack frame of 0 to 127 bytes",
            );
        }
        let mut ops = vec![list, frame];
        ops.extend_from_slice(rest);
        self.step("prepare", ops);
        if let Some(v) = extra {
            let sp = self.reg(SP);
            if fits16(v) {
                let k = self.num(-v);
                self.step("movea", vec![k, sp, sp]);
            } else {
                // Unlike the operation instructions' tables, this one has no
                // `movhi` case: the value always takes the 48-bit `mov`.
                let (k, t) = (self.num(v), self.reg(R1));
                self.forced("mov", vec![k, t], Force::Mov48);
                self.step("sub", vec![t, sp]);
            }
        }
        Some(())
    }

    /// `dispose imm1, list[, [reg]]` (pages 540-541).
    fn dispose(&mut self, args: Vec<Arg>) -> Option<()> {
        if args.len() < 2 {
            return self.error(
                self.span,
                "`dispose` needs a stack frame size and a register list",
            );
        }
        let end = match args.last().map(|a| a.kind) {
            Some(ArgKind::Bracket(_)) => args.len() - 1,
            _ => args.len(),
        };
        let (frame, extra) = self.frame(&args[0])?;
        let list = self.list(&args[1..end])?;
        if let Some(v) = extra {
            let sp = self.reg(SP);
            if fits16(v) {
                let k = self.num(v);
                self.step("movea", vec![k, sp, sp]);
            } else {
                let (k, t) = (self.num(v), self.reg(R1));
                self.forced("mov", vec![k, t], Force::Mov48);
                self.step("add", vec![t, sp]);
            }
        }
        let mut ops = vec![frame, list];
        ops.extend_from_slice(&args[end..]);
        self.step("dispose", ops);
        Some(())
    }

    /// The branch instructions: `Bcond` and its `jcond`, `bcond9` and
    /// `bcond17` spellings, `jbr`, `jr`, `jarl` and `jmp` (pages 533-537).
    fn branch(&mut self, m: &str, args: &[Arg]) -> Branch {
        match m {
            // `jmp disp32` without a register is `jmp disp32[r0]`, and `jmp32`
            // another name for `jmp` (page 535).
            "jmp" | "jmp32" => {
                let args = match args {
                    [
                        Arg {
                            kind: ArgKind::Imm(imm),
                            ..
                        },
                    ] => vec![self.mem(*imm, R0)],
                    _ => args.to_vec(),
                };
                self.step("jmp", args);
                return Branch::Steps;
            }
            // Without `-Xasm_far_jump`, `jr` and `jarl` are the 22-bit forms;
            // `jr22`, `jr32` and the `jarl` ones pick a width (pages 536-537).
            "jr" | "jr22" | "jarl" | "jarl22" | "jr32" | "jarl32" => {
                let base = if m.starts_with("jr") { "jr" } else { "jarl" };
                let force = if m.ends_with("32") {
                    Force::Disp32
                } else {
                    Force::Disp22
                };
                self.forced(base, args.to_vec(), force);
                return Branch::Steps;
            }
            _ => {}
        }
        let (name, width) = if let Some(b) = m.strip_suffix("17") {
            (b, 17)
        } else if let Some(b) = m.strip_suffix('9') {
            (b, 9)
        } else {
            (m, 0)
        };
        // `jcond` is `bcond`, and `jbr` is `br`.
        let name = match name.strip_prefix('j') {
            Some(rest) if !rest.is_empty() => {
                format!("b{}", rest.strip_prefix('b').unwrap_or(rest))
            }
            _ => name.to_string(),
        };
        if matches!(name.as_str(), "bt" | "bf") {
            if m.starts_with('j') || width != 0 {
                return Branch::No;
            }
            self.cx.error(
                self.mnemonic_span,
                format!("`{m}` cannot be used in CC-RH (page 534); write `bz` or `bnz`"),
            );
            return Branch::Failed;
        }
        let Some(cc) = branch::condition(&name) else {
            return Branch::No;
        };
        if width == 17 && cc == 0x5 {
            self.cx.error(
                self.mnemonic_span,
                "there is no `br17`; `jr` is the longer `br`",
            );
            return Branch::Failed;
        }
        // A number is the displacement itself, whose width CC-RH picks
        // (page 534).
        if let [
            Arg {
                kind: ArgKind::Imm(imm),
                ..
            },
        ] = args
            && imm.func == RelFn::None
            && let Some(v) = self.cx.constant(imm.expr)
        {
            return match self.literal_branch(cc, v, width, imm.span) {
                Some(v) => Branch::Done(v),
                None => Branch::Failed,
            };
        }
        let Some(variants) = branch::bcond(self.cx, &name, cc, args, self.span, self.rh850) else {
            return Branch::Failed;
        };
        let kept: Vec<Variant> = match width {
            9 => variants
                .into_iter()
                .filter(|v| v.bytes.len() == 2)
                .collect(),
            17 => variants
                .into_iter()
                .filter(|v| v.bytes.len() == 4)
                .collect(),
            _ => variants,
        };
        if kept.is_empty() {
            self.cx.error(
                self.mnemonic_span,
                format!("`{m}` needs the RH850 instruction set"),
            );
            return Branch::Failed;
        }
        Branch::Done(kept)
    }

    /// A branch written with a numeric displacement, sized as CC-RH sizes it
    /// (page 534): 9 bits if it fits, then 17, then a `jr` behind the inverted
    /// condition. `bsa` has no inverse, so its long form skips over a `br`.
    fn literal_branch(&mut self, cc: u8, v: i64, width: u32, span: Span) -> Option<Vec<Variant>> {
        let even = v % 2 == 0;
        let word9 =
            |cc: u8, d: i64| reloc::disp9(0x0580 | cc as u64, d).to_le_bytes()[..2].to_vec();
        let (len, bytes) = if even && fits(v, -0x100, 0xfe) && width != 17 {
            (2, word9(cc, v))
        } else if even && fits(v, -0x1_0000, 0xfffe) && width != 9 && cc != 0x5 {
            (
                4,
                reloc::disp17(0x0001_07e0 | cc as u64, v).to_le_bytes()[..4].to_vec(),
            )
        } else if even && width == 0 && fits(v, -0x20_0000, 0x1f_ffff) {
            let mut b = Vec::new();
            match cc {
                0x5 => b.extend_from_slice(&reloc::disp22(0x0780, v).to_le_bytes()[..4]),
                0xd => {
                    b.extend(word9(0xd, 4));
                    b.extend(word9(0x5, 6));
                    b.extend_from_slice(&reloc::disp22(0x0780, v - 4).to_le_bytes()[..4]);
                }
                _ => {
                    b.extend(word9(cc ^ 8, 6));
                    b.extend_from_slice(&reloc::disp22(0x0780, v - 2).to_le_bytes()[..4]);
                }
            }
            (b.len(), b)
        } else {
            self.cx.error(
                span,
                format!("branch displacement {v} is out of range or odd"),
            );
            return None;
        };
        let _ = len;
        Some(vec![Variant::new(bytes)])
    }

    /// Assembles the steps and joins them into one encoding.
    fn finish(self, original: &str) -> Option<Vec<Variant>> {
        let nomacro = self.cx.state.features & FEATURE_NOMACRO != 0;
        if nomacro
            && (self.steps.len() > 1
                || self.steps.first().is_some_and(|s| {
                    s.mnemonic != original || s.force == Force::Mov48 || s.force == Force::Disp23
                }))
            && !matches!(original, "mov32")
            && !original.starts_with("ld23")
            && !original.starts_with("st23")
        {
            self.cx.error(
                self.span,
                "this operand needs CC-RH to expand the instruction, and `$NOMACRO` is in effect",
            );
            return None;
        }
        let single = self.steps.len() == 1;
        let mut bytes = Vec::new();
        let mut fixups = Vec::new();
        for step in &self.steps {
            let variants = super::encode_insn(
                self.cx,
                step.mnemonic,
                &step.args,
                self.span,
                self.mnemonic_span,
                self.rh850,
                |e| step.force.allows(e),
            )?;
            if single {
                return Some(variants);
            }
            let [v] = variants.as_slice() else {
                self.cx
                    .error(self.span, "an expansion cannot contain a relaxable branch");
                return None;
            };
            for f in &v.fixups {
                let mut f = f.clone();
                f.offset += bytes.len() as u32;
                fixups.push(f);
            }
            bytes.extend_from_slice(&v.bytes);
        }
        Some(vec![Variant { bytes, fixups }])
    }
}
