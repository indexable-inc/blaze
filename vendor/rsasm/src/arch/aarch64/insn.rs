//! Mnemonic dispatch and the per-family encoders.
//!
//! A64 has no operand-encoding table worth the name: every family has its own
//! fixed field layout, and the interesting work is in the aliases, which are
//! defined by the ARM ARM as rewrites onto a base form (`cmp` is `subs` to the
//! zero register, `lsl #n` is a `ubfm`, `mov` is one of four different
//! instructions depending on its operands). Writing those rewrites out is
//! clearer than a table that would need an escape hatch for each of them.
//!
//! The forms that are one each rather than a family are here for the same
//! reason: each of them constrains an operand in a way a measured row could
//! not say. `ld64b` names the first of eight consecutive registers, so only
//! an even one up to `x22` will do; `bfc` computes one immediate from the
//! other and bounds their sum; `rprfm` reads a name or a number scattered
//! over six bits of the word; and `udf` has no opcode bits at all.
//!
//! Two whole families are here on the same grounds. FEAT_MOPS's 132 memory
//! copies and sets write an address back with no offset for the `!` to apply
//! to and a register back with no brackets at all, and both references
//! refuse any two of their three registers naming one register; FEAT_CMPBR's
//! thirty compare-and-branches end in a target, which is a fixup, and a
//! table row has nowhere to put one.

use super::encode::{const_in_range, field, logical_imm, word, word_fixup};
use super::operand::{ExtendOp, Mem, MemKind, Operand, OperandKind, RelocOp, ShiftOp, TlsLdst};
use super::reg::{self, Reg, RegClass};
use super::{encode, sysreg};
use crate::arch::{AsmCtx, InsnRequest};
use crate::cursor::Cursor;
use crate::expr::{ExprKind, ExprRef};
use crate::lexer::{Punct, TokKind, Token};
use crate::section::{LinkValue, Variant};
use crate::source::Span;

/// Everything one `assemble` call needs, so the family encoders take two
/// arguments instead of five.
pub struct Insn<'a, 't> {
    pub mnemonic: &'a str,
    pub ops: &'a [Operand<'t>],
    pub span: Span,
}

impl Insn<'_, '_> {
    /// Checks the operand count, reporting the accepted counts on failure.
    fn arity(&self, cx: &mut AsmCtx<'_>, want: &[usize]) -> bool {
        if want.contains(&self.ops.len()) {
            return true;
        }
        let list: Vec<String> = want.iter().map(|n| n.to_string()).collect();
        cx.error(
            self.span,
            format!(
                "`{}` takes {} operand(s), but {} were given",
                self.mnemonic,
                list.join(" or "),
                self.ops.len()
            ),
        );
        false
    }

    fn op(&self, i: usize) -> Option<&Operand<'_>> {
        self.ops.get(i)
    }

    /// A general-purpose register operand.
    ///
    /// Register 31 is the zero register in most fields, so `sp` is refused
    /// here; the few fields that mean the stack pointer use [`Self::gpr_or_sp`].
    fn gpr(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Reg> {
        let r = self.gpr_or_sp(cx, i)?;
        if r.is_sp() {
            cx.error(
                self.ops[i].span,
                format!(
                    "operand {} of `{}` cannot be the stack pointer",
                    i + 1,
                    self.mnemonic
                ),
            );
            return None;
        }
        Some(r)
    }

    /// A general-purpose register operand where `sp`/`wsp` is also allowed.
    fn gpr_or_sp(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Reg> {
        let op = self.op(i)?;
        match op.reg() {
            Some(r) if r.is_gpr() => Some(r),
            _ => {
                cx.error(
                    op.span,
                    format!(
                        "operand {} of `{}` must be a general-purpose register, found {}",
                        i + 1,
                        self.mnemonic,
                        op.describe()
                    ),
                );
                None
            }
        }
    }

    /// A register of any class.
    fn any_reg(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Reg> {
        let op = self.op(i)?;
        match op.reg() {
            Some(r) => Some(r),
            None => {
                cx.error(
                    op.span,
                    format!(
                        "operand {} of `{}` must be a register",
                        i + 1,
                        self.mnemonic
                    ),
                );
                None
            }
        }
    }

    /// A 64-bit general-purpose register, where register 31 is the zero
    /// register: what the one-register forms of the newest extensions take.
    fn xreg(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<u32> {
        let r = self.gpr(cx, i)?;
        if r.class != RegClass::X {
            cx.error(
                self.ops[i].span,
                format!("`{}` takes a 64-bit register", self.mnemonic),
            );
            return None;
        }
        Some(u32::from(r.num))
    }

    fn cond(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<u8> {
        let op = self.op(i)?;
        match op.cond() {
            Some(c) => Some(c),
            None => {
                cx.error(op.span, "expected a condition code");
                None
            }
        }
    }

    fn expr(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<ExprRef> {
        let op = self.op(i)?;
        op.expr(cx)
    }

    /// A PC-relative target with no relocation operator on it, with any
    /// difference of two labels folded into the number it is.
    ///
    /// GNU as folds such a difference as it reads the line, and what is left
    /// decides the field: a number goes in as the offset a number written
    /// there would be, where a label is measured from the instruction. So
    /// `adr x0, b - a` points four bytes past itself when the two labels are
    /// four apart, and `b (b - a) * 2`, which no relocation could carry,
    /// assembles at all. Under a relocation operator nothing is folded: the
    /// operator names an access only the linker makes, and GNU as refuses a
    /// difference there.
    fn pcrel_expr(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<ExprRef> {
        let e = self.expr(cx, i)?;
        Some(crate::arch::fold_differences(cx, e))
    }

    /// A constant operand with a range check.
    fn imm(&self, cx: &mut AsmCtx<'_>, i: usize, lo: i64, hi: i64, what: &str) -> Option<i64> {
        let e = self.expr(cx, i)?;
        const_in_range(cx, e, lo, hi, what)
    }

    fn mem(&self, cx: &mut AsmCtx<'_>, i: usize) -> Option<Mem> {
        let op = self.op(i)?;
        match op.mem() {
            Some(m) => Some(m.clone()),
            None => {
                cx.error(op.span, "expected an address in `[...]`");
                None
            }
        }
    }

    /// Rejects mixing `w` and `x` operands, which is the commonest A64 typo.
    fn same_width(&self, cx: &mut AsmCtx<'_>, regs: &[Reg]) -> Option<RegClass> {
        let first = regs.first()?.class;
        for r in regs {
            if r.class != first {
                cx.error(
                    self.span,
                    format!("`{}` cannot mix 32-bit and 64-bit registers", self.mnemonic),
                );
                return None;
            }
        }
        Some(first)
    }

    fn no_sp(&self, cx: &mut AsmCtx<'_>, r: Reg) -> Option<()> {
        if r.is_sp() {
            cx.error(
                self.span,
                format!("`{}` cannot use the stack pointer here", self.mnemonic),
            );
            return None;
        }
        Some(())
    }

    /// The mirror of [`Self::no_sp`], for fields where register 31 is the stack
    /// pointer: writing `xzr` there would silently name `sp` instead.
    fn no_zr(&self, cx: &mut AsmCtx<'_>, r: Reg) -> Option<()> {
        if r.is_zr() {
            cx.error(
                self.span,
                format!(
                    "`{}` cannot use the zero register here: this field would read it as the stack pointer",
                    self.mnemonic
                ),
            );
            return None;
        }
        Some(())
    }
}

fn one(w: u32) -> Option<Vec<Variant>> {
    Some(vec![word(w)])
}

fn one_fixup(w: u32, e: ExprRef, k: crate::section::FixupKind, span: Span) -> Option<Vec<Variant>> {
    Some(vec![word_fixup(w, e, k, span)])
}

/// A PC-relative target. Written as a number rather than a label, it is the
/// offset from the instruction, not an address: `b #16` branches 16 bytes
/// ahead wherever it is, to GNU as and llvm-mc alike. So a number goes
/// straight into the field, range-checked as the fixup would have been.
fn pcrel(
    cx: &mut AsmCtx<'_>,
    w: u32,
    e: ExprRef,
    k: crate::section::FixupKind,
    span: Span,
) -> Option<Vec<Variant>> {
    let Some(v) = cx.constant(e).filter(|_| !names_symbol(cx, e)) else {
        return one_fixup(w, e, k, span);
    };
    let crate::section::FieldEncoding::Scatter(place) = k.encoding else {
        return one_fixup(w, e, k, span);
    };
    let bits = u32::from(k.value_bits);
    let (lo, hi) = (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1);
    if !(lo..=hi).contains(&v) {
        cx.error(span, format!("offset {v} is out of range {lo}..={hi}"));
        return None;
    }
    if v % i64::from(k.value_align) != 0 {
        cx.error(
            span,
            format!("offset {v} is not a multiple of {}", k.value_align),
        );
        return None;
    }
    one(place(u64::from(w), v) as u32)
}

/// True for a mnemonic the encoders here handle, whatever else the
/// generated table has under the same name: `add` is handwritten for the
/// general-purpose registers and a table form for vectors. Keep in step with
/// the dispatch in [`assemble`].
pub(crate) fn handwritten(mnemonic: &str) -> bool {
    mnemonic
        .strip_prefix("b.")
        .or_else(|| mnemonic.strip_prefix("bc."))
        .is_some_and(|c| reg::cond(c).is_some())
        || mnemonic
            .strip_prefix('b')
            .is_some_and(|c| c.len() == 2 && reg::cond(c).is_some())
        || matches!(
            mnemonic,
            "add"
                | "adds"
                | "sub"
                | "subs"
                | "cmp"
                | "cmn"
                | "neg"
                | "negs"
                | "adc"
                | "adcs"
                | "sbc"
                | "sbcs"
                | "ngc"
                | "ngcs"
                | "and"
                | "ands"
                | "orr"
                | "eor"
                | "bic"
                | "bics"
                | "orn"
                | "eon"
                | "tst"
                | "mvn"
                | "mov"
                | "movz"
                | "movn"
                | "movk"
                | "sbfm"
                | "ubfm"
                | "bfm"
                | "sbfx"
                | "ubfx"
                | "bfxil"
                | "sbfiz"
                | "ubfiz"
                | "bfi"
                | "sxtb"
                | "sxth"
                | "sxtw"
                | "uxtb"
                | "uxth"
                | "lsl"
                | "lsr"
                | "asr"
                | "ror"
                | "lslv"
                | "lsrv"
                | "asrv"
                | "rorv"
                | "extr"
                | "mul"
                | "mneg"
                | "smull"
                | "umull"
                | "smnegl"
                | "umnegl"
                | "smulh"
                | "umulh"
                | "madd"
                | "msub"
                | "smaddl"
                | "umaddl"
                | "smsubl"
                | "umsubl"
                | "sdiv"
                | "udiv"
                | "rbit"
                | "rev"
                | "rev16"
                | "rev32"
                | "rev64"
                | "clz"
                | "cls"
                | "csel"
                | "csinc"
                | "csinv"
                | "csneg"
                | "cset"
                | "csetm"
                | "cinc"
                | "cinv"
                | "cneg"
                | "ccmp"
                | "ccmn"
                | "b"
                | "bl"
                | "cbz"
                | "cbnz"
                | "tbz"
                | "tbnz"
                | "br"
                | "blr"
                | "ret"
                | "eret"
                | "drps"
                | "adr"
                | "adrp"
                | "hint"
                | "dmb"
                | "dsb"
                | "isb"
                | "clrex"
                | "sys"
                | "sysl"
                | "svc"
                | "hvc"
                | "smc"
                | "brk"
                | "hlt"
                | "dcps1"
                | "dcps2"
                | "dcps3"
                | "mrs"
                | "msr"
                | "smstart"
                | "smstop"
                | "zero"
                | "bfc"
                | "ctz"
                | "rmif"
                | "setf8"
                | "setf16"
                | "cfinv"
                | "axflag"
                | "xaflag"
                | "udf"
                | "wfet"
                | "wfit"
                | "maddpt"
                | "msubpt"
                | "ld64b"
                | "st64b"
                | "st64bv"
                | "st64bv0"
                | "rprfm"
                // One or two forms each, from the newest extensions:
                // FEAT_GCS, FEAT_TME, FEAT_ITE, FEAT_TEV and FEAT_POE2.
                | "gcspushm"
                | "gcspopm"
                | "gcsss1"
                | "gcsss2"
                | "gcspushx"
                | "gcspopx"
                | "gcspopcx"
                | "gcsstr"
                | "gcssttr"
                | "tstart"
                | "ttest"
                | "tcancel"
                | "trcit"
                | "tenter"
                | "texit"
                | "tchangeb"
                | "tchangef"
        )
        || is_mops(mnemonic)
        || cmpbr_name(mnemonic).is_some()
        || loads(mnemonic)
        // The system instructions and the aliases of `hint`, whose names are
        // in the generated tables rather than written out here.
        || sysreg::is_sys_ins(mnemonic)
        || sysreg::is_hint(mnemonic)
}

/// True for the handwritten loads and stores, which take the scalar SIMD
/// registers (`ldr d0, [x0]`) and leave only the SVE forms to the table.
pub(crate) fn loads(mnemonic: &str) -> bool {
    matches!(
        mnemonic,
        "ldr"
            | "str"
            | "ldrb"
            | "strb"
            | "ldrh"
            | "strh"
            | "ldrsb"
            | "ldrsh"
            | "ldrsw"
            | "ldur"
            | "stur"
            | "ldurb"
            | "sturb"
            | "ldurh"
            | "sturh"
            | "ldursb"
            | "ldursh"
            | "ldursw"
            | "prfm"
            | "prfum"
            | "ldp"
            | "stp"
            | "ldpsw"
            | "ldnp"
            | "stnp"
    )
}

/// Entry point: resolves a mnemonic to a family and encodes it.
pub fn assemble(
    cx: &mut AsmCtx<'_>,
    req: &InsnRequest<'_>,
    mnemonic: &str,
    ops: &[Operand<'_>],
) -> Option<Vec<Variant>> {
    let i = Insn {
        mnemonic,
        ops,
        span: req.span,
    };

    // `b.<cond>` and the `b<cond>` spelling GNU as also accepts, and
    // `bc.<cond>`, which neither reference spells without the dot.
    if let Some(rest) = mnemonic.strip_prefix("bc.")
        && let Some(c) = reg::cond(rest)
    {
        return branch_cond(cx, &i, c, true);
    }
    if let Some(rest) = mnemonic.strip_prefix("b.")
        && let Some(c) = reg::cond(rest)
    {
        return branch_cond(cx, &i, c, false);
    }
    if let Some(rest) = mnemonic.strip_prefix('b')
        && rest.len() == 2
        && let Some(c) = reg::cond(rest)
    {
        return branch_cond(cx, &i, c, false);
    }

    match mnemonic {
        "add" | "adds" | "sub" | "subs" => addsub(cx, &i),
        "cmp" | "cmn" => cmp(cx, &i),
        "neg" | "negs" => neg(cx, &i),
        "adc" | "adcs" | "sbc" | "sbcs" => addsub_carry(cx, &i),
        "ngc" | "ngcs" => ngc(cx, &i),

        "and" | "ands" | "orr" | "eor" | "bic" | "bics" | "orn" | "eon" => logic(cx, &i),
        "tst" => tst(cx, &i),
        "mvn" => mvn(cx, &i),
        "mov" => mov(cx, &i),
        "movz" | "movn" | "movk" => movw(cx, &i),

        "sbfm" | "ubfm" | "bfm" => bitfield_raw(cx, &i),
        "sbfx" | "ubfx" | "bfxil" => bitfield_extract(cx, &i),
        "sbfiz" | "ubfiz" | "bfi" | "bfc" => bitfield_insert(cx, &i),
        "sxtb" | "sxth" | "sxtw" | "uxtb" | "uxth" => extend(cx, &i),
        "lsl" | "lsr" | "asr" | "ror" => shift(cx, &i),
        "lslv" | "lsrv" | "asrv" | "rorv" => shift_reg(cx, &i, mnemonic.trim_end_matches('v')),
        "extr" => extr(cx, &i),

        "mul" | "mneg" | "smull" | "umull" | "smnegl" | "umnegl" | "smulh" | "umulh" => {
            mul_alias(cx, &i)
        }
        "madd" | "msub" | "smaddl" | "umaddl" | "smsubl" | "umsubl" | "maddpt" | "msubpt" => {
            madd(cx, &i)
        }
        "sdiv" | "udiv" => div(cx, &i),

        "rbit" | "rev" | "rev16" | "rev32" | "rev64" | "clz" | "cls" | "ctz" => dp1(cx, &i),

        "csel" | "csinc" | "csinv" | "csneg" => csel(cx, &i),
        "cset" | "csetm" => cset(cx, &i),
        "cinc" | "cinv" | "cneg" => cinc(cx, &i),
        "ccmp" | "ccmn" => ccmp(cx, &i),

        "b" | "bl" => branch(cx, &i),
        "cbz" | "cbnz" => cbz(cx, &i),
        _ if cmpbr_name(mnemonic).is_some() => {
            let (size, cc) = cmpbr_name(mnemonic)?;
            cmpbr(cx, &i, size, cc)
        }
        "tbz" | "tbnz" => tbz(cx, &i),
        "br" | "blr" | "ret" => branch_reg(cx, &i),
        "eret" => {
            i.arity(cx, &[0]).then_some(())?;
            one(0xd69f_03e0)
        }
        "drps" => {
            i.arity(cx, &[0]).then_some(())?;
            one(0xd6bf_03e0)
        }

        "adr" | "adrp" => adr(cx, &i),

        "ldr" | "str" | "ldrb" | "strb" | "ldrh" | "strh" | "ldrsb" | "ldrsh" | "ldrsw"
        | "ldur" | "stur" | "ldurb" | "sturb" | "ldurh" | "sturh" | "ldursb" | "ldursh"
        | "ldursw" | "prfm" | "prfum" => ldst(cx, &i),
        "ldp" | "stp" | "ldpsw" | "ldnp" | "stnp" => ldst_pair(cx, &i),
        "ld64b" | "st64b" | "st64bv" | "st64bv0" => ldst64b(cx, &i),
        "rprfm" => rprfm(cx, &i),

        "hint" => hint(cx, &i),
        "dmb" | "dsb" | "isb" | "clrex" => barrier(cx, &i),
        "svc" | "hvc" | "smc" | "brk" | "hlt" | "dcps1" | "dcps2" | "dcps3" => exception(cx, &i),
        "udf" => udf(cx, &i),
        "wfet" | "wfit" => wfxt(cx, &i),
        "rmif" | "setf8" | "setf16" | "cfinv" | "axflag" | "xaflag" => flags(cx, &i),
        "mrs" => mrs(cx, &i),
        "msr" => msr(cx, &i),
        "sys" | "sysl" => sys_raw(cx, &i),
        "smstart" | "smstop" => sme_mode(cx, &i),

        "gcspushm" | "gcspopm" | "gcsss1" | "gcsss2" | "gcspushx" | "gcspopx" | "gcspopcx"
        | "gcsstr" | "gcssttr" => gcs(cx, &i),
        "tstart" | "ttest" | "tcancel" => tme(cx, &i),
        "trcit" => trcit(cx, &i),
        "tenter" | "texit" | "tchangeb" | "tchangef" => tev(cx, &i),

        // `dc civac, x0` and `esb` and their like: a name in the generated
        // system tables is all these mnemonics are.
        _ if sysreg::is_sys_ins(mnemonic) => sys_alias(cx, &i),
        _ if sysreg::is_hint(mnemonic) => hint_alias(cx, &i),

        _ => {
            cx.error(
                req.mnemonic_span,
                format!("unknown instruction `{mnemonic}`"),
            );
            None
        }
    }
}

// ---- add / subtract --------------------------------------------------------

const ADDSUB_IMM: u32 = 0x1100_0000;
const ADDSUB_SHIFT: u32 = 0x0b00_0000;
const ADDSUB_EXT: u32 = 0x0b20_0000;

/// The `op` (subtract) and `S` (set flags) bits shared by the add/sub family.
fn addsub_bits(mnemonic: &str) -> (u32, u32) {
    let sub = mnemonic.starts_with("sub") || mnemonic.starts_with("neg") || mnemonic == "cmp";
    let s = mnemonic.ends_with('s');
    (u32::from(sub), u32::from(s))
}

/// `add`/`adds`/`sub`/`subs` with three or four operands.
fn addsub(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3, 4]).then_some(())?;
    let rd = i.gpr_or_sp(cx, 0)?;
    let rn = i.gpr_or_sp(cx, 1)?;
    i.same_width(cx, &[rd, rn])?;
    let (op, s) = addsub_bits(i.mnemonic);
    encode_addsub(cx, i, op, s, rd, rn, 2)
}

/// `cmp`/`cmn`, which are `subs`/`adds` writing to the zero register.
fn cmp(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2, 3]).then_some(())?;
    let rn = i.gpr_or_sp(cx, 0)?;
    let rd = Reg::zero(rn.class);
    let (op, _) = addsub_bits(i.mnemonic);
    encode_addsub(cx, i, op, 1, rd, rn, 1)
}

/// `neg`/`negs`, which are `sub`/`subs` from the zero register.
fn neg(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2, 3]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rm = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rm])?;
    let (_, s) = addsub_bits(i.mnemonic);
    let shift = shift_operand(
        cx,
        i,
        2,
        rd.class,
        &[ShiftOp::Lsl, ShiftOp::Lsr, ShiftOp::Asr],
    )?;
    one(field(rd.sf(), 31, 1)
        | field(1, 30, 1)
        | field(s, 29, 1)
        | ADDSUB_SHIFT
        | field(shift.0, 22, 2)
        | field(rm.num as u32, 16, 5)
        | field(shift.1, 10, 6)
        | field(31, 5, 5)
        | field(rd.num as u32, 0, 5))
}

/// The shared tail of add/sub: an immediate, a shifted register or an extended
/// register, starting at operand `at`.
fn encode_addsub(
    cx: &mut AsmCtx<'_>,
    i: &Insn<'_, '_>,
    op: u32,
    s: u32,
    rd: Reg,
    rn: Reg,
    at: usize,
) -> Option<Vec<Variant>> {
    let src = i.op(at)?;
    let sf = field(rd.sf(), 31, 1) | field(op, 30, 1) | field(s, 29, 1);
    // A flag-setting add or subtract writes register 31 as the zero register.
    if s == 1 {
        i.no_sp(cx, rd)?;
    }

    if let Some(rm) = src.reg() {
        if !rm.is_gpr() || rm.is_sp() {
            cx.error(
                src.span,
                "expected a general-purpose register other than the stack pointer",
            );
            return None;
        }
        // The extended-register form is the only one that can name the stack
        // pointer, so a bare `add sp, sp, x0` has to use it. There, `lsl`
        // means "extend from the operation's own width", which is `uxtx` at 64
        // bits and `uxtw` at 32.
        let uses_sp = rd.is_sp() || rn.is_sp();
        let natural = if rd.class == RegClass::X {
            ExtendOp::Uxtx
        } else {
            ExtendOp::Uxtw
        };
        let extend = match i.op(at + 1).map(|o| &o.kind) {
            Some(OperandKind::Extend(e, amount)) => Some((*e, *amount)),
            Some(OperandKind::Shift(ShiftOp::Lsl, amount)) if uses_sp => {
                i.same_width(cx, &[rd, rn, rm])?;
                Some((natural, Some(*amount)))
            }
            _ => None,
        };
        if extend.is_some() || uses_sp {
            i.no_zr(cx, rn)?;
            if s == 0 {
                i.no_zr(cx, rd)?;
            }
            let (ext, amount) = match extend {
                Some((e, a)) => (e, a),
                None => {
                    i.same_width(cx, &[rd, rn, rm])?;
                    (natural, None)
                }
            };
            if rd.class == RegClass::W && rm.class != RegClass::W {
                cx.error(
                    src.span,
                    "a 32-bit operation cannot extend a 64-bit register",
                );
                return None;
            }
            if ext.source_class() != rm.class {
                cx.error(
                    src.span,
                    format!(
                        "`{}` needs a {} register",
                        ext.name(),
                        ext.source_class().letter()
                    ),
                );
                return None;
            }
            let amount = match amount {
                Some(e) => const_in_range(cx, e, 0, 4, "an extend amount")? as u32,
                None => 0,
            };
            if i.ops.len() > at + 2 {
                cx.error(i.ops[at + 2].span, "too many operands");
                return None;
            }
            return one(sf
                | ADDSUB_EXT
                | field(rm.num as u32, 16, 5)
                | field(ext.code(), 13, 3)
                | field(amount, 10, 3)
                | field(rn.num as u32, 5, 5)
                | field(rd.num as u32, 0, 5));
        }
        i.same_width(cx, &[rd, rn, rm])?;
        i.no_sp(cx, rd)?;
        i.no_sp(cx, rn)?;
        let (kind, amount) = shift_operand(
            cx,
            i,
            at + 1,
            rd.class,
            &[ShiftOp::Lsl, ShiftOp::Lsr, ShiftOp::Asr],
        )?;
        return one(sf
            | ADDSUB_SHIFT
            | field(kind, 22, 2)
            | field(rm.num as u32, 16, 5)
            | field(amount, 10, 6)
            | field(rn.num as u32, 5, 5)
            | field(rd.num as u32, 0, 5));
    }

    // `add x0, x0, :lo12:sym`: the second half of an `adrp` pair, or a piece
    // of a thread-local access. Only a plain add makes sense — the linker
    // writes the bits as an unsigned offset.
    if let OperandKind::Reloc(rop, e) = src.kind {
        let (kind, hi12) = match rop {
            RelocOp::Lo12 => (encode::fixup_lo12_add(), false),
            RelocOp::Tls(t) if t.add != 0 => (encode::fixup_tls(t.add), t.hi12),
            _ => {
                cx.error(
                    src.span,
                    format!("`{}` is not valid on `{}`", rop.name(), i.mnemonic),
                );
                return None;
            }
        };
        if op != 0 || s != 0 {
            cx.error(
                src.span,
                format!("`{}` needs a plain `add`, not `{}`", rop.name(), i.mnemonic),
            );
            return None;
        }
        // GNU as takes `lsl #0` or `lsl #12` after the operator and sets the
        // shift as written, which llvm-mc refuses. After `:tprel_hi12:` it
        // takes any `lsl` it can encode at all, and shifts by twelve anyway.
        let shift12 = match i.op(at + 1) {
            None => false,
            Some(next) => {
                let OperandKind::Shift(ShiftOp::Lsl, amount) = next.kind else {
                    cx.error(next.span, "expected `lsl #0` or `lsl #12`");
                    return None;
                };
                let max = if hi12 { 63 } else { 12 };
                match const_in_range(cx, amount, 0, max, "an add/sub immediate shift")? {
                    _ if hi12 => true,
                    0 => false,
                    12 => true,
                    n => {
                        cx.error(
                            next.span,
                            format!("an add/sub immediate can only be shifted by 0 or 12, not {n}"),
                        );
                        return None;
                    }
                }
            }
        };
        if i.ops.len() > at + 2 {
            cx.error(i.ops[at + 2].span, "too many operands");
            return None;
        }
        return one_fixup(
            sf | ADDSUB_IMM
                | field(u32::from(shift12 || hi12), 22, 1)
                | field(rn.num as u32, 5, 5)
                | field(rd.num as u32, 0, 5),
            e,
            kind,
            src.span,
        );
    }

    // An immediate, optionally shifted left by 12.
    let e = src.expr(cx)?;
    let mut shift12 = 0;
    if let Some(next) = i.op(at + 1) {
        match next.kind {
            OperandKind::Shift(ShiftOp::Lsl, amount) => {
                match const_in_range(cx, amount, 0, 12, "an add/sub immediate shift")? {
                    0 => {}
                    12 => shift12 = 1,
                    n => {
                        cx.error(
                            next.span,
                            format!("an add/sub immediate can only be shifted by 0 or 12, not {n}"),
                        );
                        return None;
                    }
                }
            }
            _ => {
                cx.error(next.span, "expected `lsl #0` or `lsl #12`");
                return None;
            }
        }
    }
    if i.ops.len() > at + 2 {
        cx.error(i.ops[at + 2].span, "too many operands");
        return None;
    }
    let Some(v) = cx.constant(e) else {
        cx.error(src.span, "an add/sub immediate must be a constant");
        return None;
    };
    // Both register fields of the immediate form read 31 as the stack pointer,
    // apart from the destination of a flag-setting one.
    i.no_zr(cx, rn)?;
    if s == 0 {
        i.no_zr(cx, rd)?;
    }
    // Without an explicit shift the assembler is free to pick one: a negative
    // immediate flips add into sub and back, and a multiple of 4096 moves into
    // the shifted field. Both are what llvm-mc and GNU as do, and existing
    // code relies on `sub sp, sp, #-16` and `add x0, x0, #0x10000`.
    let (op, mut v) = match (shift12, v.checked_neg()) {
        (0, Some(n)) if v < 0 => (op ^ 1, n),
        _ => (op, v),
    };
    if shift12 == 0 && v > 4095 && v & 0xfff == 0 {
        shift12 = 1;
        v >>= 12;
    }
    if !(0..=4095).contains(&v) {
        cx.error(
            src.span,
            format!(
                "an add/sub immediate must be 0..=4095, or a multiple of 4096 up to {:#x}, but is {}",
                4095 << 12,
                if shift12 == 1 { format!("{v} << 12") } else { v.to_string() }
            ),
        );
        return None;
    }
    one(field(rd.sf(), 31, 1)
        | field(op, 30, 1)
        | field(s, 29, 1)
        | ADDSUB_IMM
        | field(shift12, 22, 1)
        | field(v as u32, 10, 12)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

/// `adc`/`sbc` and their flag-setting forms.
fn addsub_carry(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    let rm = i.gpr(cx, 2)?;
    i.same_width(cx, &[rd, rn, rm])?;
    let op = u32::from(i.mnemonic.starts_with("sbc"));
    let s = u32::from(i.mnemonic.ends_with('s'));
    one(field(rd.sf(), 31, 1)
        | field(op, 30, 1)
        | field(s, 29, 1)
        | 0x1a00_0000
        | field(rm.num as u32, 16, 5)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

/// `ngc`, which is `sbc` from the zero register.
fn ngc(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rm = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rm])?;
    let s = u32::from(i.mnemonic.ends_with('s'));
    one(field(rd.sf(), 31, 1)
        | field(1, 30, 1)
        | field(s, 29, 1)
        | 0x1a00_0000
        | field(rm.num as u32, 16, 5)
        | field(31, 5, 5)
        | field(rd.num as u32, 0, 5))
}

/// Reads an optional trailing shift operand, returning `(kind, amount)`.
fn shift_operand(
    cx: &mut AsmCtx<'_>,
    i: &Insn<'_, '_>,
    at: usize,
    class: RegClass,
    allowed: &[ShiftOp],
) -> Option<(u32, u32)> {
    let Some(op) = i.op(at) else {
        return Some((0, 0));
    };
    let OperandKind::Shift(kind, amount) = op.kind else {
        cx.error(op.span, "expected a shift such as `lsl #3`");
        return None;
    };
    if !allowed.contains(&kind) {
        let names: Vec<&str> = allowed.iter().map(|s| s.name()).collect();
        cx.error(
            op.span,
            format!("`{}` only accepts {}", i.mnemonic, names.join(", ")),
        );
        return None;
    }
    let max = if class == RegClass::X { 63 } else { 31 };
    let n = const_in_range(cx, amount, 0, max, "a shift amount")?;
    if i.ops.len() > at + 1 {
        cx.error(i.ops[at + 1].span, "too many operands");
        return None;
    }
    Some((kind.code(), n as u32))
}

// ---- logical ---------------------------------------------------------------

const LOGIC_SHIFT: u32 = 0x0a00_0000;
const LOGIC_IMM: u32 = 0x1200_0000;

/// `(opc, N)` for the logical family. `N` inverts the second operand.
fn logic_bits(mnemonic: &str) -> Option<(u32, u32)> {
    Some(match mnemonic {
        "and" => (0, 0),
        "bic" => (0, 1),
        "orr" => (1, 0),
        "orn" => (1, 1),
        "eor" => (2, 0),
        "eon" => (2, 1),
        "ands" => (3, 0),
        "bics" => (3, 1),
        _ => return None,
    })
}

fn logic(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3, 4]).then_some(())?;
    // Only the immediate forms can write the stack pointer; the register forms
    // check again once they know which form they are.
    let rd = i.gpr_or_sp(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rn])?;
    let (opc, n) = logic_bits(i.mnemonic)?;
    encode_logic(cx, i, opc, n, rd, rn, 2)
}

fn encode_logic(
    cx: &mut AsmCtx<'_>,
    i: &Insn<'_, '_>,
    opc: u32,
    n: u32,
    rd: Reg,
    rn: Reg,
    at: usize,
) -> Option<Vec<Variant>> {
    let src = i.op(at)?;
    let head = field(rd.sf(), 31, 1) | field(opc, 29, 2);

    if let Some(rm) = src.reg() {
        if !rm.is_gpr() {
            cx.error(src.span, "expected a general-purpose register");
            return None;
        }
        i.same_width(cx, &[rd, rn, rm])?;
        i.no_sp(cx, rd)?;
        i.no_sp(cx, rn)?;
        i.no_sp(cx, rm)?;
        let (kind, amount) = shift_operand(
            cx,
            i,
            at + 1,
            rd.class,
            &[ShiftOp::Lsl, ShiftOp::Lsr, ShiftOp::Asr, ShiftOp::Ror],
        )?;
        return one(head
            | LOGIC_SHIFT
            | field(kind, 22, 2)
            | field(n, 21, 1)
            | field(rm.num as u32, 16, 5)
            | field(amount, 10, 6)
            | field(rn.num as u32, 5, 5)
            | field(rd.num as u32, 0, 5));
    }

    // The inverted mnemonics have no immediate encoding of their own: with an
    // immediate, `bic`, `bics`, `orn` and `eon` are `and`, `ands`, `orr` and
    // `eor` of the complement, which is what both references assemble them to
    // (GNU as takes only `bic` that way; llvm-mc takes all four).
    let invert = n != 0;
    if i.ops.len() > at + 1 {
        cx.error(i.ops[at + 1].span, "too many operands");
        return None;
    }
    let e = src.expr(cx)?;
    let Some(v) = cx.constant(e) else {
        cx.error(src.span, "a logical immediate must be a constant");
        return None;
    };
    let bits = if rd.class == RegClass::X { 64 } else { 32 };
    let mut masked = if bits == 32 {
        v as u32 as u64
    } else {
        v as u64
    };
    if invert {
        masked = !masked;
        if bits == 32 {
            masked &= 0xffff_ffff;
        }
    }
    let Some((imm_n, immr, imms)) = logical_imm(masked, bits) else {
        cx.error(
            src.span,
            if invert {
                format!(
                    "{v:#x} is not a valid `{}` immediate: its complement must be a repeating run of ones, which the field holds",
                    i.mnemonic
                )
            } else {
                format!(
                    "{v:#x} is not a valid logical immediate: the field holds only a repeating run of ones"
                )
            },
        );
        return None;
    };
    // The immediate forms write to the stack pointer, not the zero register —
    // except `ands`, whose flag-setting destination is the zero register.
    if opc == 3 {
        i.no_sp(cx, rd)?;
    } else {
        i.no_zr(cx, rd)?;
    }
    one(head
        | LOGIC_IMM
        | field(imm_n, 22, 1)
        | field(immr, 16, 6)
        | field(imms, 10, 6)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

/// `tst`, which is `ands` to the zero register.
fn tst(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2, 3]).then_some(())?;
    let rn = i.gpr(cx, 0)?;
    encode_logic(cx, i, 3, 0, Reg::zero(rn.class), rn, 1)
}

/// `mvn`, which is `orn` from the zero register.
fn mvn(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2, 3]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    encode_logic(cx, i, 1, 1, rd, Reg::zero(rd.class), 1)
}

// ---- moves -----------------------------------------------------------------

const MOVW: u32 = 0x1280_0000;

fn movw(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2, 3]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let opc = match i.mnemonic {
        "movn" => 0,
        "movz" => 2,
        _ => 3,
    };
    let head = field(rd.sf(), 31, 1) | field(opc, 29, 2) | MOVW | field(rd.num as u32, 0, 5);

    // `movz x0, :abs_g1_nc:sym`: the operator names the group, so it sets the
    // shift as well as the relocation and there is nothing left to write.
    let src = i.op(1)?;
    if let OperandKind::Reloc(RelocOp::Movw(g), e) = src.kind {
        if rd.class == RegClass::W && g.group >= 2 {
            cx.error(
                src.span,
                format!("`{}` names bits a 32-bit register has not got", g.op),
            );
            return None;
        }
        if i.mnemonic == "movk" && !g.movk {
            cx.error(
                src.span,
                format!(
                    "`{}` may need the value negated, which `movk` cannot do",
                    g.op
                ),
            );
            return None;
        }
        if let Some(shift) = i.op(2) {
            cx.error(shift.span, "a relocation operator sets the shift itself");
            return None;
        }
        return Some(vec![word_fixup(
            head | field(u32::from(g.group), 21, 2),
            e,
            encode::fixup_movw(g),
            src.span,
        )]);
    }

    let v = i.imm(cx, 1, -0x8000, 0xffff, "a move-wide immediate")?;
    let mut hw = 0u32;
    if let Some(op) = i.op(2) {
        let OperandKind::Shift(ShiftOp::Lsl, amount) = op.kind else {
            cx.error(op.span, "expected `lsl #0`, `#16`, `#32` or `#48`");
            return None;
        };
        let n = const_in_range(cx, amount, 0, 48, "a move-wide shift")?;
        if n % 16 != 0 {
            cx.error(op.span, "a move-wide shift must be 0, 16, 32 or 48");
            return None;
        }
        hw = n as u32 / 16;
    }
    if rd.class == RegClass::W && hw > 1 {
        cx.error(i.span, "a 32-bit move-wide can only shift by 0 or 16");
        return None;
    }
    one(head | field(hw, 21, 2) | field(v as u32, 5, 16))
}

/// `mov`, which is four different instructions.
///
/// Between registers it is `orr Rd, ZR, Rm`, except that the stack pointer can
/// only be named by `add Rd, Rn, #0`. With an immediate the assembler picks the
/// first of `movz`, `movn` and `orr Rd, ZR, #imm` that can hold the value —
/// the order GNU as and llvm-mc both use.
fn mov(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let rd = i.gpr_or_sp(cx, 0)?;
    let src = i.op(1)?;

    if let Some(rm) = src.reg() {
        if !rm.is_gpr() {
            cx.error(src.span, "expected a general-purpose register");
            return None;
        }
        i.same_width(cx, &[rd, rm])?;
        if rd.is_sp() || rm.is_sp() {
            i.no_zr(cx, rd)?;
            i.no_zr(cx, rm)?;
            return one(field(rd.sf(), 31, 1)
                | ADDSUB_IMM
                | field(rm.num as u32, 5, 5)
                | field(rd.num as u32, 0, 5));
        }
        return one(field(rd.sf(), 31, 1)
            | field(1, 29, 2)
            | LOGIC_SHIFT
            | field(rm.num as u32, 16, 5)
            | field(31, 5, 5)
            | field(rd.num as u32, 0, 5));
    }

    i.no_sp(cx, rd)?;
    let e = src.expr(cx)?;
    let Some(v) = cx.constant(e) else {
        cx.error(src.span, "a `mov` immediate must be a constant");
        return None;
    };
    let bits = if rd.class == RegClass::X { 64 } else { 32 };
    let value = if bits == 32 {
        v as u32 as u64
    } else {
        v as u64
    };
    let sf = field(rd.sf(), 31, 1);
    let rd_bits = field(rd.num as u32, 0, 5);

    // movz: the value is one 16-bit chunk, everything else zero.
    for hw in 0..(bits / 16) {
        let shift = hw * 16;
        if value >> shift << shift == value && (value >> shift) <= 0xffff {
            return one(sf
                | field(2, 29, 2)
                | MOVW
                | field(hw, 21, 2)
                | field((value >> shift) as u32, 5, 16)
                | rd_bits);
        }
    }
    // movn: the *inverse* is one 16-bit chunk.
    let inverted = if bits == 32 {
        !value & 0xffff_ffff
    } else {
        !value
    };
    for hw in 0..(bits / 16) {
        let shift = hw * 16;
        if inverted >> shift << shift == inverted && (inverted >> shift) <= 0xffff {
            return one(sf
                | MOVW
                | field(hw, 21, 2)
                | field((inverted >> shift) as u32, 5, 16)
                | rd_bits);
        }
    }
    // orr Rd, ZR, #imm, for the bitmask patterns.
    if let Some((imm_n, immr, imms)) = logical_imm(value, bits) {
        return one(sf
            | field(1, 29, 2)
            | LOGIC_IMM
            | field(imm_n, 22, 1)
            | field(immr, 16, 6)
            | field(imms, 10, 6)
            | field(31, 5, 5)
            | rd_bits);
    }
    cx.error(
        src.span,
        format!("{v:#x} cannot be moved in one instruction; use `movz`/`movk`, or a literal pool"),
    );
    None
}

// ---- bitfield --------------------------------------------------------------

const BITFIELD: u32 = 0x1300_0000;

fn bitfield_opc(mnemonic: &str) -> u32 {
    match mnemonic {
        "sbfm" | "sbfx" | "sbfiz" | "asr" | "sxtb" | "sxth" | "sxtw" => 0,
        "bfm" | "bfxil" | "bfi" | "bfc" => 1,
        _ => 2,
    }
}

fn emit_bitfield(rd: Reg, rn: Reg, opc: u32, immr: u32, imms: u32) -> Option<Vec<Variant>> {
    let n = rd.sf();
    one(field(rd.sf(), 31, 1)
        | field(opc, 29, 2)
        | BITFIELD
        | field(n, 22, 1)
        | field(immr, 16, 6)
        | field(imms, 10, 6)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

/// `sbfm`/`ubfm`/`bfm` written out in full.
fn bitfield_raw(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[4]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rn])?;
    let max = if rd.class == RegClass::X { 63 } else { 31 };
    let immr = i.imm(cx, 2, 0, max, "`immr`")? as u32;
    let imms = i.imm(cx, 3, 0, max, "`imms`")? as u32;
    emit_bitfield(rd, rn, bitfield_opc(i.mnemonic), immr, imms)
}

/// `sbfx`/`ubfx`/`bfxil`: extract `width` bits starting at `lsb`.
fn bitfield_extract(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[4]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rn])?;
    let max = if rd.class == RegClass::X { 63 } else { 31 };
    let lsb = i.imm(cx, 2, 0, max, "the bit position")?;
    let width = i.imm(cx, 3, 1, max + 1 - lsb, "the field width")?;
    emit_bitfield(
        rd,
        rn,
        bitfield_opc(i.mnemonic),
        lsb as u32,
        (lsb + width - 1) as u32,
    )
}

/// `sbfiz`/`ubfiz`/`bfi`: insert `width` bits *at* `lsb`, which rotates the
/// source down rather than up, so `immr` counts backwards from the register
/// width.
///
/// `bfc` is the same insert reading the zero register, so it clears the field
/// instead of filling it and names one operand fewer.
fn bitfield_insert(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    let clear = i.mnemonic == "bfc";
    i.arity(cx, if clear { &[3] } else { &[4] }).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = if clear {
        Reg {
            class: rd.class,
            num: 31,
            sp: false,
        }
    } else {
        let rn = i.gpr(cx, 1)?;
        i.same_width(cx, &[rd, rn])?;
        rn
    };
    let at = usize::from(!clear);
    let width_bits = if rd.class == RegClass::X { 64 } else { 32 };
    let lsb = i.imm(cx, at + 1, 0, width_bits - 1, "the bit position")?;
    let width = i.imm(cx, at + 2, 1, width_bits - lsb, "the field width")?;
    emit_bitfield(
        rd,
        rn,
        bitfield_opc(i.mnemonic),
        ((width_bits - lsb) % width_bits) as u32,
        (width - 1) as u32,
    )
}

/// `sxtb`/`sxth`/`sxtw`/`uxtb`/`uxth`, all bitfield moves from bit 0.
fn extend(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    // The source is always a `w` register: these read the low 8, 16 or 32 bits.
    if rn.class != RegClass::W {
        cx.error(
            i.ops[1].span,
            format!("`{}` reads a 32-bit register", i.mnemonic),
        );
        return None;
    }
    let imms = match i.mnemonic {
        "sxtb" | "uxtb" => 7,
        "sxth" | "uxth" => 15,
        _ => 31,
    };
    if i.mnemonic == "sxtw" && rd.class != RegClass::X {
        cx.error(i.span, "`sxtw` writes a 64-bit register");
        return None;
    }
    if matches!(i.mnemonic, "uxtb" | "uxth") && rd.class != RegClass::W {
        cx.error(i.span, format!("`{}` writes a 32-bit register", i.mnemonic));
        return None;
    }
    // `sbfm` reads its source at the destination's width, so the source
    // register number is reused with the destination's class.
    let src = Reg {
        class: rd.class,
        num: rn.num,
        sp: false,
    };
    emit_bitfield(rd, src, bitfield_opc(i.mnemonic), 0, imms)
}

/// `lsl`/`lsr`/`asr`/`ror`, by an immediate (a bitfield move or `extr`) or by a
/// register (the variable-shift instructions).
fn shift(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    if i.op(2).and_then(|o| o.reg()).is_some() {
        return shift_reg(cx, i, i.mnemonic);
    }
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rn])?;
    let width = if rd.class == RegClass::X { 64 } else { 32 };
    let n = i.imm(cx, 2, 0, width - 1, "a shift amount")? as u32;
    let width = width as u32;
    match i.mnemonic {
        // `lsl #n` keeps the low width-n bits and moves them up: ubfm with
        // immr = -n mod width.
        "lsl" => emit_bitfield(rd, rn, 2, (width - n) % width, width - 1 - n),
        "lsr" => emit_bitfield(rd, rn, 2, n, width - 1),
        "asr" => emit_bitfield(rd, rn, 0, n, width - 1),
        _ => one(field(rd.sf(), 31, 1)
            | 0x1380_0000
            | field(rd.sf(), 22, 1)
            | field(rn.num as u32, 16, 5)
            | field(n, 10, 6)
            | field(rn.num as u32, 5, 5)
            | field(rd.num as u32, 0, 5)),
    }
}

/// The variable-shift instructions, whose canonical names end in `v`.
fn shift_reg(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>, base: &str) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    let rm = i.gpr(cx, 2)?;
    i.same_width(cx, &[rd, rn, rm])?;
    let opcode = match base {
        "lsl" => 0b1000,
        "lsr" => 0b1001,
        "asr" => 0b1010,
        _ => 0b1011,
    };
    one(field(rd.sf(), 31, 1)
        | 0x1ac0_0000
        | field(rm.num as u32, 16, 5)
        | field(opcode, 10, 6)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

fn extr(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[4]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    let rm = i.gpr(cx, 2)?;
    i.same_width(cx, &[rd, rn, rm])?;
    let max = if rd.class == RegClass::X { 63 } else { 31 };
    let lsb = i.imm(cx, 3, 0, max, "the rotate amount")? as u32;
    one(field(rd.sf(), 31, 1)
        | 0x1380_0000
        | field(rd.sf(), 22, 1)
        | field(rm.num as u32, 16, 5)
        | field(lsb, 10, 6)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

// ---- multiply and divide ---------------------------------------------------

const DP3: u32 = 0x1b00_0000;

/// `(op31, o0, widening)` for the three-source multiply family.
fn madd_bits(mnemonic: &str) -> Option<(u32, u32, bool)> {
    Some(match mnemonic {
        "madd" | "mul" => (0, 0, false),
        "msub" | "mneg" => (0, 1, false),
        // FEAT_CPA's checked forms, which carry a pointer's upper bits
        // through the arithmetic and so exist only at 64 bits.
        "maddpt" => (3, 0, false),
        "msubpt" => (3, 1, false),
        "smaddl" | "smull" => (1, 0, true),
        "smsubl" | "smnegl" => (1, 1, true),
        "smulh" => (2, 0, true),
        "umaddl" | "umull" => (5, 0, true),
        "umsubl" | "umnegl" => (5, 1, true),
        "umulh" => (6, 0, true),
        _ => return None,
    })
}

fn emit_madd(rd: Reg, rn: Reg, rm: Reg, ra: u8, op31: u32, o0: u32) -> Option<Vec<Variant>> {
    one(field(rd.sf(), 31, 1)
        | DP3
        | field(op31, 21, 3)
        | field(rm.num as u32, 16, 5)
        | field(o0, 15, 1)
        | field(ra as u32, 10, 5)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

/// `mul`, `mneg`, `smull` and friends: `madd`-family with `Ra` = zero register.
fn mul_alias(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    let rm = i.gpr(cx, 2)?;
    let (op31, o0, widening) = madd_bits(i.mnemonic)?;
    let high = matches!(i.mnemonic, "smulh" | "umulh");
    if widening && !high {
        // The `l` forms multiply two 32-bit registers into a 64-bit one.
        if rd.class != RegClass::X || rn.class != RegClass::W || rm.class != RegClass::W {
            cx.error(
                i.span,
                format!(
                    "`{}` takes an `x` destination and two `w` sources",
                    i.mnemonic
                ),
            );
            return None;
        }
    } else {
        i.same_width(cx, &[rd, rn, rm])?;
        if high && rd.class != RegClass::X {
            cx.error(i.span, format!("`{}` is 64-bit only", i.mnemonic));
            return None;
        }
    }
    emit_madd(rd, rn, rm, 31, op31, o0)
}

fn madd(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[4]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    let rm = i.gpr(cx, 2)?;
    let ra = i.gpr(cx, 3)?;
    let (op31, o0, widening) = madd_bits(i.mnemonic)?;
    if widening {
        if rd.class != RegClass::X
            || rn.class != RegClass::W
            || rm.class != RegClass::W
            || ra.class != RegClass::X
        {
            cx.error(
                i.span,
                format!(
                    "`{}` takes `x` for its destination and accumulator and `w` for its multiplicands",
                    i.mnemonic
                ),
            );
            return None;
        }
    } else {
        i.same_width(cx, &[rd, rn, rm, ra])?;
        if i.mnemonic.ends_with("pt") && rd.class != RegClass::X {
            cx.error(i.span, format!("`{}` is 64-bit only", i.mnemonic));
            return None;
        }
    }
    emit_madd(rd, rn, rm, ra.num, op31, o0)
}

fn div(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    let rm = i.gpr(cx, 2)?;
    i.same_width(cx, &[rd, rn, rm])?;
    let opcode = if i.mnemonic == "sdiv" { 3 } else { 2 };
    one(field(rd.sf(), 31, 1)
        | 0x1ac0_0000
        | field(rm.num as u32, 16, 5)
        | field(opcode, 10, 6)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

/// The one-source data-processing group: bit and byte reversal, counting
/// leading zeros or sign bits, and counting trailing zeros.
fn dp1(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rn])?;
    let x = rd.class == RegClass::X;
    let opcode = match i.mnemonic {
        "rbit" => 0,
        "rev16" => 1,
        // `rev` reverses the whole register, so it is opcode 2 at 32 bits and
        // opcode 3 at 64; `rev32` is the 64-bit-only "reverse each word" form.
        "rev32" => {
            if !x {
                cx.error(i.span, "`rev32` is 64-bit only");
                return None;
            }
            2
        }
        "rev" => {
            if x {
                3
            } else {
                2
            }
        }
        "rev64" => {
            if !x {
                cx.error(i.span, "`rev64` is 64-bit only");
                return None;
            }
            3
        }
        "clz" => 4,
        "cls" => 5,
        // FEAT_CSSC added the count-trailing-zeros that completes the group;
        // its `abs`, `cnt` and the min/max forms share their mnemonics with
        // SIMD and SVE, and so are the generated table's.
        _ => 6,
    };
    one(field(rd.sf(), 31, 1)
        | 0x5ac0_0000
        | field(opcode, 10, 6)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

// ---- conditional -----------------------------------------------------------

const CONDSEL: u32 = 0x1a80_0000;

fn emit_condsel(rd: Reg, rn: Reg, rm: Reg, cond: u8, op: u32, op2: u32) -> Option<Vec<Variant>> {
    one(field(rd.sf(), 31, 1)
        | field(op, 30, 1)
        | CONDSEL
        | field(rm.num as u32, 16, 5)
        | field(cond as u32, 12, 4)
        | field(op2, 10, 2)
        | field(rn.num as u32, 5, 5)
        | field(rd.num as u32, 0, 5))
}

fn csel(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[4]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    let rm = i.gpr(cx, 2)?;
    i.same_width(cx, &[rd, rn, rm])?;
    let cond = i.cond(cx, 3)?;
    let (op, op2) = match i.mnemonic {
        "csel" => (0, 0),
        "csinc" => (0, 1),
        "csinv" => (1, 0),
        _ => (1, 1),
    };
    emit_condsel(rd, rn, rm, cond, op, op2)
}

/// The condition an alias inverts. `al`/`nv` have no inverse, and the ARM ARM
/// makes that an error rather than silently encoding `nv`.
fn invert(cx: &mut AsmCtx<'_>, span: Span, cond: u8) -> Option<u8> {
    if cond >= 14 {
        cx.error(
            span,
            "`al` and `nv` cannot be inverted, so this alias has no encoding",
        );
        return None;
    }
    Some(cond ^ 1)
}

/// `cset`/`csetm`: set a register from the *inverse* condition of a select
/// between two zero registers.
fn cset(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let cond = i.cond(cx, 1)?;
    let cond = invert(cx, i.ops[1].span, cond)?;
    let zr = Reg::zero(rd.class);
    let (op, op2) = if i.mnemonic == "cset" { (0, 1) } else { (1, 0) };
    emit_condsel(rd, zr, zr, cond, op, op2)
}

/// `cinc`/`cinv`/`cneg`: the two-source selects with both sources the same.
fn cinc(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    let rn = i.gpr(cx, 1)?;
    i.same_width(cx, &[rd, rn])?;
    let cond = i.cond(cx, 2)?;
    let cond = invert(cx, i.ops[2].span, cond)?;
    let (op, op2) = match i.mnemonic {
        "cinc" => (0, 1),
        "cinv" => (1, 0),
        _ => (1, 1),
    };
    emit_condsel(rd, rn, rn, cond, op, op2)
}

fn ccmp(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[4]).then_some(())?;
    let rn = i.gpr(cx, 0)?;
    let nzcv = i.imm(cx, 2, 0, 15, "the flags value")? as u32;
    let cond = i.cond(cx, 3)?;
    let op = u32::from(i.mnemonic == "ccmp");
    let head = field(rn.sf(), 31, 1)
        | field(op, 30, 1)
        | field(1, 29, 1)
        | 0x1a40_0000
        | field(cond as u32, 12, 4)
        | field(rn.num as u32, 5, 5)
        | nzcv;
    let src = i.op(1)?;
    if src.reg().is_some() {
        let rm = i.gpr(cx, 1)?;
        i.same_width(cx, &[rn, rm])?;
        return one(head | field(rm.num as u32, 16, 5));
    }
    let imm = i.imm(cx, 1, 0, 31, "a conditional-compare immediate")? as u32;
    one(head | field(imm, 16, 5) | field(1, 11, 1))
}

// ---- branches --------------------------------------------------------------

fn branch(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[1]).then_some(())?;
    let e = i.pcrel_expr(cx, 0)?;
    let (base, kind) = if i.mnemonic == "bl" {
        (0x9400_0000, encode::fixup_call())
    } else {
        (0x1400_0000, encode::fixup_b())
    };
    pcrel(cx, base, e, kind, i.ops[0].span)
}

/// `b.<cond>`, and FEAT_HBC's `bc.<cond>`, which is the same branch with the
/// hint that the condition's outcome is consistent: one bit apart, and the
/// same 19-bit field and relocation.
fn branch_cond(
    cx: &mut AsmCtx<'_>,
    i: &Insn<'_, '_>,
    cond: u8,
    consistent: bool,
) -> Option<Vec<Variant>> {
    i.arity(cx, &[1]).then_some(())?;
    let e = i.pcrel_expr(cx, 0)?;
    pcrel(
        cx,
        0x5400_0000 | field(u32::from(consistent), 4, 1) | field(cond as u32, 0, 4),
        e,
        encode::fixup_b19(),
        i.ops[0].span,
    )
}

fn cbz(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let rt = i.gpr(cx, 0)?;
    let base = if i.mnemonic == "cbz" {
        0x3400_0000
    } else {
        0x3500_0000
    };
    let w = field(rt.sf(), 31, 1) | base | field(rt.num as u32, 0, 5);
    if let Some((e, kind)) = literal_operator(cx, &i.ops[1], encode::fixup_b19())? {
        return pcrel(cx, w, e, kind, i.ops[1].span);
    }
    let e = i.pcrel_expr(cx, 1)?;
    pcrel(cx, w, e, encode::fixup_b19(), i.ops[1].span)
}

fn tbz(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    let rt = i.gpr(cx, 0)?;
    let max = if rt.class == RegClass::X { 63 } else { 31 };
    let bit = i.imm(cx, 1, 0, max, "the bit number")? as u32;
    let base = if i.mnemonic == "tbz" {
        0x3600_0000
    } else {
        0x3700_0000
    };
    // The bit number is split: its top bit doubles as the register-width bit.
    let w = field(bit >> 5, 31, 1) | base | field(bit & 31, 19, 5) | field(rt.num as u32, 0, 5);
    if let Some((e, kind)) = literal_operator(cx, &i.ops[2], encode::fixup_b14())? {
        return pcrel(cx, w, e, kind, i.ops[2].span);
    }
    let e = i.pcrel_expr(cx, 2)?;
    pcrel(cx, w, e, encode::fixup_b14(), i.ops[2].span)
}

/// `ldr x0, :got:v`, `ldr x0, :gottprel:v` and `ldr x0, :tlsdesc:v`: a
/// relocation operator in a PC-relative literal field, with the relocation
/// that names the GOT slot or the thread-local access. `None` when the
/// operand is something else.
///
/// A plain number under the operator is an offset from the instruction, as
/// it would be without one, and gets the instruction's own field, `plain`:
/// GNU as folds it before it looks at the operator.
///
/// GNU as reads the target of `cbz`, `cbnz`, `tbz` and `tbnz` with the parser
/// it reads a literal load's with, so those take the operators as well and
/// get the load's relocation, although what a linker writes for it covers
/// `tbz`'s bit number; llvm-mc ignores the operator on all four and writes
/// their plain branch relocation. rsasm follows GNU as.
fn literal_operator(
    cx: &mut AsmCtx<'_>,
    op: &Operand<'_>,
    plain: crate::section::FixupKind,
) -> Option<Option<(ExprRef, crate::section::FixupKind)>> {
    let (e, reloc) = match op.kind {
        OperandKind::Reloc(RelocOp::Got, e) => (e, encode::fixup_got_ld_lit()),
        OperandKind::Reloc(RelocOp::Tls(t), e) => {
            if t.literal == 0 {
                cx.error(
                    op.span,
                    format!("`{}` is not valid on a literal load", t.op),
                );
                return None;
            }
            (e, encode::fixup_tls(t.literal))
        }
        _ => return Some(None),
    };
    if !names_symbol(cx, e) {
        return Some(Some((e, plain)));
    }
    Some(Some((e, reloc)))
}

fn branch_reg(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    let default_lr = i.mnemonic == "ret";
    i.arity(cx, if default_lr { &[0, 1] } else { &[1] })
        .then_some(())?;
    let rn = match i.op(0) {
        Some(_) => {
            let r = i.gpr(cx, 0)?;
            if r.class != RegClass::X {
                cx.error(i.ops[0].span, "a branch target register must be 64-bit");
                return None;
            }
            r.num
        }
        None => 30,
    };
    let base = match i.mnemonic {
        "br" => 0xd61f_0000,
        "blr" => 0xd63f_0000,
        _ => 0xd65f_0000,
    };
    one(base | field(rn as u32, 5, 5))
}

fn adr(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    if rd.class != RegClass::X {
        cx.error(i.ops[0].span, "`adr` and `adrp` write a 64-bit register");
        return None;
    }
    let target = i.op(1)?;
    let (e, kind) = match (i.mnemonic, &target.kind) {
        ("adrp", OperandKind::Reloc(RelocOp::Got, e)) => (*e, encode::fixup_got_page()),
        (_, OperandKind::Reloc(RelocOp::Tls(t), e)) => {
            let reloc = if i.mnemonic == "adrp" { t.adrp } else { t.adr };
            if reloc == 0 {
                cx.error(
                    target.span,
                    format!("`{}` is not valid on `{}`", t.op, i.mnemonic),
                );
                return None;
            }
            // As in a literal load, GNU as takes a plain number under the
            // operator as the offset `adr` would without one; see
            // `literal_operator`. On `adrp` it refuses one.
            if i.mnemonic == "adr" && !names_symbol(cx, *e) {
                (*e, encode::fixup_adr())
            } else {
                (*e, encode::fixup_tls(reloc))
            }
        }
        ("adrp", _) => {
            let e = target.expr(cx)?;
            // A bare number is a count of pages from here, to GNU as and
            // llvm-mc alike, rather than an address to take the page of.
            let kind = if names_symbol(cx, e) {
                encode::fixup_adrp()
            } else {
                encode::fixup_adrp().link(LinkValue::Plain)
            };
            (e, kind)
        }
        _ => (i.pcrel_expr(cx, 1)?, encode::fixup_adr()),
    };
    let base = if i.mnemonic == "adrp" {
        0x9000_0000
    } else {
        0x1000_0000
    };
    if i.mnemonic == "adr" && !kind.always_reloc {
        return pcrel(cx, base | field(rd.num as u32, 0, 5), e, kind, target.span);
    }
    one_fixup(base | field(rd.num as u32, 0, 5), e, kind, target.span)
}

/// Whether an expression refers to a symbol or a position, rather than being
/// arithmetic on numbers alone.
fn names_symbol(cx: &AsmCtx<'_>, e: ExprRef) -> bool {
    match cx.exprs.get(e).kind {
        ExprKind::Int(_) => false,
        ExprKind::Unary(_, x) | ExprKind::Modifier(_, x) => names_symbol(cx, x),
        ExprKind::Binary(_, l, r) => names_symbol(cx, l) || names_symbol(cx, r),
        _ => true,
    }
}

// ---- loads and stores ------------------------------------------------------

/// The fields a load or store needs, once the mnemonic and the data register
/// have been looked at together.
struct LdStForm {
    /// The two-bit `size` field.
    size: u32,
    /// SIMD/FP register file.
    v: bool,
    /// The two-bit `opc` field.
    opc: u32,
    /// log2 of the access width, which scales the unsigned immediate offset
    /// and selects the `S` bit of a register offset.
    scale: u32,
    /// Only the unscaled 9-bit form exists (`ldur` and friends).
    unscaled_only: bool,
}

fn ldst_form(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>, rt: Option<Reg>) -> Option<LdStForm> {
    let m = i.mnemonic;
    let unscaled_only = m.starts_with("ldur") || m.starts_with("stur") || m == "prfum";
    // `ldur`/`stur` differ from `ldr`/`str` only in the addressing form.
    let base: &str = match m {
        "ldur" => "ldr",
        "stur" => "str",
        "ldurb" => "ldrb",
        "sturb" => "strb",
        "ldurh" => "ldrh",
        "sturh" => "strh",
        "ldursb" => "ldrsb",
        "ldursh" => "ldrsh",
        "ldursw" => "ldrsw",
        "prfum" => "prfm",
        other => other,
    };
    let load = base.starts_with("ld") || base == "prfm";
    let (size, v, opc, scale) = match base {
        "ldrb" | "strb" => (0, false, u32::from(load), 0),
        "ldrh" | "strh" => (1, false, u32::from(load), 1),
        "ldrsb" | "ldrsh" | "ldrsw" => {
            let rt = rt?;
            let size = match base {
                "ldrsb" => 0,
                "ldrsh" => 1,
                _ => 2,
            };
            if base == "ldrsw" && rt.class != RegClass::X {
                cx.error(i.span, "`ldrsw` writes a 64-bit register");
                return None;
            }
            // `opc` bit 0 selects a 32-bit destination for the sign-extending
            // loads, which is the reverse of the usual load/store convention.
            (size, false, 2 | u32::from(rt.class == RegClass::W), size)
        }
        "prfm" => (3, false, 2, 3),
        _ => {
            let rt = rt?;
            match rt.class {
                RegClass::W => (2, false, u32::from(load), 2),
                RegClass::X => (3, false, u32::from(load), 3),
                // A 128-bit access sets the high bit of `opc` instead of using
                // a fourth `size` value, which is already taken.
                RegClass::Q => (0, true, 2 | u32::from(load), 4),
                c => (
                    match c {
                        RegClass::B => 0,
                        RegClass::H => 1,
                        RegClass::S => 2,
                        _ => 3,
                    },
                    true,
                    u32::from(load),
                    c.bytes().trailing_zeros(),
                ),
            }
        }
    };
    Some(LdStForm {
        size,
        v,
        opc,
        scale,
        unscaled_only,
    })
}

fn ldst(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    // `ldr x0, =value` puts the value in a literal pool and loads it from
    // there; `ldr x0, label` is a PC-relative literal load, not an
    // addressing mode.
    if let Some(OperandKind::Literal(e)) = i.op(1).map(|o| &o.kind) {
        return pool_load(cx, i, *e);
    }
    if matches!(i.mnemonic, "ldr" | "ldrsw" | "prfm") && i.op(1).is_some_and(|o| o.mem().is_none())
    {
        return ldst_literal(cx, i);
    }
    let prefetch = i.mnemonic.starts_with("prf");
    let (rt_num, rt) = if prefetch {
        (prefetch_hint(cx, i)?, None)
    } else {
        let r = i.any_reg(cx, 0)?;
        if r.is_sp() {
            cx.error(
                i.ops[0].span,
                "the stack pointer cannot be loaded or stored",
            );
            return None;
        }
        (r.num, Some(r))
    };
    let form = ldst_form(cx, i, rt)?;
    let mem = i.mem(cx, 1)?;
    let head = field(form.size, 30, 2)
        | field(u32::from(form.v), 26, 1)
        | field(form.opc, 22, 2)
        | field(mem.base.num as u32, 5, 5)
        | field(rt_num as u32, 0, 5);

    match &mem.kind {
        MemKind::Offset(off) => {
            let value = match off {
                None => Some(0),
                Some(e) => cx.constant(*e),
            };
            // A symbolic offset has no relocation here; `:lo12:` is the way to
            // write one, and `ldr x0, label` is the PC-relative literal form.
            let Some(v) = value else {
                cx.error(mem.span, "a load/store offset must be a constant");
                return None;
            };
            let scaled = v >> form.scale;
            if !form.unscaled_only
                && v >= 0
                && scaled << form.scale == v
                && (0..=4095).contains(&scaled)
            {
                return one(head | 0x3900_0000 | field(scaled as u32, 10, 12));
            }
            if !(-256..=255).contains(&v) {
                cx.error(
                    mem.span,
                    format!(
                        "offset {v} is out of range: it must be -256..=255, \
                         or a multiple of {} up to {}",
                        1 << form.scale,
                        4095u32 << form.scale
                    ),
                );
                return None;
            }
            one(head | 0x3800_0000 | field(v as u32, 12, 9))
        }
        MemKind::OffsetReloc(rop, e) => {
            if form.unscaled_only {
                cx.error(
                    mem.span,
                    format!(
                        "`{}` has no 12-bit offset field for `{}`",
                        i.mnemonic,
                        rop.name()
                    ),
                );
                return None;
            }
            let kind = match rop {
                RelocOp::Lo12 => encode::fixup_lo12_ldst(form.scale),
                RelocOp::Tls(t) => match t.ldst {
                    TlsLdst::Scaled(r) if form.scale < 4 => {
                        encode::fixup_tls(r[form.scale as usize])
                    }
                    TlsLdst::Any(r) => encode::fixup_tls(r),
                    TlsLdst::Scaled(_) => {
                        cx.error(
                            mem.span,
                            format!("`{}` has no relocation for a 16-byte access", t.op),
                        );
                        return None;
                    }
                    TlsLdst::None => {
                        cx.error(
                            mem.span,
                            format!("`{}` is not valid for a load or store", t.op),
                        );
                        return None;
                    }
                },
                // A GOT slot is 64 bits wide, but GNU as writes the 64-bit
                // slot's relocation whatever the access reads of it, as it
                // does for `:gottprel_lo12:`, and so does llvm-mc for
                // Darwin's `@GOTPAGEOFF`; only llvm-mc's ELF refuses an
                // access that is not the 64-bit load. The field is still the
                // access's own, so it keeps the access's scale.
                RelocOp::GotLo12 => encode::fixup_got_lo12(form.scale),
                _ => {
                    cx.error(
                        mem.span,
                        format!("`{}` is not valid for this load or store", rop.name()),
                    );
                    return None;
                }
            };
            one_fixup(head | 0x3900_0000, *e, kind, mem.span)
        }
        MemKind::PreIndex(e) | MemKind::PostIndex(e) => {
            let pre = matches!(mem.kind, MemKind::PreIndex(_));
            let v = const_in_range(cx, *e, -256, 255, "a pre/post-index offset")?;
            let kind = if pre { 3 } else { 1 };
            one(head | 0x3800_0000 | field(v as u32, 12, 9) | field(kind, 10, 2))
        }
        MemKind::Reg(index) => {
            // `S` picks between shifting by 0 and by the access size. For a
            // byte access both mean 0, and an explicit `lsl #0` still selects
            // `S=1` — so the scale test has to come before the zero test.
            let s = match index.amount {
                None => 0,
                Some(n) if n as u32 == form.scale => 1,
                Some(0) => 0,
                Some(n) => {
                    cx.error(
                        mem.span,
                        format!(
                            "this access can only shift its index by {} (or not at all), not {n}",
                            form.scale
                        ),
                    );
                    return None;
                }
            };
            one(head
                | 0x3820_0000
                | field(index.reg.num as u32, 16, 5)
                | field(index.ext.code(), 13, 3)
                | field(s, 12, 1)
                | field(2, 10, 2))
        }
    }
}

/// `ldr x0, label`: a PC-relative load from a literal pool.
///
/// `ldrsw` and `prfm` share the encoding, which is why they reach here: the
/// two-bit `opc` picks between a 32-bit load, a 64-bit one, a sign-extending
/// word load and a prefetch, and the prefetch's `Rt` is its hint rather than
/// a register.
fn ldst_literal(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    let (opc, v, rt_num) = if i.mnemonic == "prfm" {
        (3, false, prefetch_hint(cx, i)?)
    } else {
        let rt = i.any_reg(cx, 0)?;
        let (opc, v) = match (i.mnemonic, rt.class) {
            ("ldrsw", RegClass::X) => (2, false),
            ("ldrsw", _) => {
                cx.error(i.ops[0].span, "`ldrsw` writes a 64-bit register");
                return None;
            }
            (_, RegClass::W) => (0, false),
            (_, RegClass::X) => (1, false),
            (_, RegClass::S) => (0, true),
            (_, RegClass::D) => (1, true),
            (_, RegClass::Q) => (2, true),
            _ => {
                cx.error(
                    i.ops[0].span,
                    "this register cannot be loaded from a literal",
                );
                return None;
            }
        };
        (opc, v, rt.num)
    };
    let w =
        field(opc, 30, 2) | 0x1800_0000 | field(u32::from(v), 26, 1) | field(rt_num as u32, 0, 5);
    if let Some((e, kind)) = literal_operator(cx, &i.ops[1], encode::fixup_ld_lit())? {
        return pcrel(cx, w, e, kind, i.ops[1].span);
    }
    let e = i.pcrel_expr(cx, 1)?;
    pcrel(cx, w, e, encode::fixup_ld_lit(), i.ops[1].span)
}

/// `ldr <rt>, =value`: the value goes in the section's literal pool and the
/// instruction loads it from there.
///
/// GNU as, whose source this syntax comes from, always makes an entry, even
/// for a value `movz` could hold; llvm-mc assembles `ldr x0, =1` as
/// `mov x0, #1` instead. rsasm follows GNU as here, as it does for ARM: what
/// a pool holds and where it goes is decided across instructions, and the
/// source that uses pools was written for GNU as.
fn pool_load(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>, e: ExprRef) -> Option<Vec<Variant>> {
    if !matches!(i.mnemonic, "ldr" | "ldrsw") {
        cx.error(
            i.ops[1].span,
            format!(
                "`{}` cannot load from a literal pool; only `ldr` and `ldrsw` can",
                i.mnemonic
            ),
        );
        return None;
    }
    let rt = i.any_reg(cx, 0)?;
    // The entry is as wide as the register the load fills, and `ldrsw`
    // sign-extends a word.
    let (opc, v, size) = match (i.mnemonic, rt.class) {
        ("ldrsw", RegClass::X) => (2, false, 4),
        ("ldrsw", _) => {
            cx.error(i.ops[0].span, "`ldrsw` writes a 64-bit register");
            return None;
        }
        (_, RegClass::W) => (0, false, 4),
        (_, RegClass::X) => (1, false, 8),
        (_, RegClass::S) => (0, true, 4),
        (_, RegClass::D) => (1, true, 8),
        (_, RegClass::Q) => (2, true, 16),
        _ => {
            cx.error(
                i.ops[0].span,
                "this register cannot be loaded from a literal pool",
            );
            return None;
        }
    };
    // A constant is kept as a number, so that two uses of the same value
    // share an entry; anything else is an expression the entry relocates.
    let value = match cx.constant(e) {
        Some(n) => crate::arch::Literal::Const(n),
        None => crate::arch::Literal::Expr(e),
    };
    let entry = cx.literal(value, size, i.ops[1].span);
    let kind = encode::fixup_ld_lit()
        .with_range_hint("the literal pool is too far away; put an `.ltorg` nearer");
    one_fixup(
        field(opc, 30, 2) | 0x1800_0000 | field(u32::from(v), 26, 1) | field(rt.num as u32, 0, 5),
        entry,
        kind,
        i.ops[1].span,
    )
}

fn prefetch_hint(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<u8> {
    let op = i.op(0)?;
    if let Some(n) = op.word() {
        let text = cx.name(n).to_ascii_lowercase();
        // `p<type><target><policy>`: load/store/instruction, L1/L2/L3, and
        // keep/strm. The five bits are laid out in exactly that order.
        let ty = match text.get(..3) {
            Some("pld") => Some(0),
            Some("pli") => Some(1),
            Some("pst") => Some(2),
            _ => None,
        };
        let rest = match text.get(3..) {
            Some("l1keep") => Some((0, 0)),
            Some("l1strm") => Some((0, 1)),
            Some("l2keep") => Some((1, 0)),
            Some("l2strm") => Some((1, 1)),
            Some("l3keep") => Some((2, 0)),
            Some("l3strm") => Some((2, 1)),
            _ => None,
        };
        let (Some(ty), Some((level, policy))) = (ty, rest) else {
            cx.error(op.span, format!("`{text}` is not a prefetch hint"));
            return None;
        };
        return Some(ty << 3 | level << 1 | policy);
    }
    let v = i.imm(cx, 0, 0, 31, "a prefetch operation")?;
    Some(v as u8)
}

fn ldst_pair(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    let rt = i.any_reg(cx, 0)?;
    let rt2 = i.any_reg(cx, 1)?;
    if rt.class != rt2.class {
        cx.error(i.span, "both registers of a pair must be the same width");
        return None;
    }
    if rt.is_sp() || rt2.is_sp() {
        cx.error(i.span, "the stack pointer cannot be loaded or stored");
        return None;
    }
    let load = i.mnemonic.starts_with("ld");
    let (opc, v, scale) = match (i.mnemonic, rt.class) {
        ("ldpsw", RegClass::X) => (1, false, 2),
        ("ldpsw", _) => {
            cx.error(i.span, "`ldpsw` writes 64-bit registers");
            return None;
        }
        (_, RegClass::W) => (0, false, 2),
        (_, RegClass::X) => (2, false, 3),
        (_, RegClass::S) => (0, true, 2),
        (_, RegClass::D) => (1, true, 3),
        (_, RegClass::Q) => (2, true, 4),
        _ => {
            cx.error(i.ops[0].span, "this register cannot be part of a pair");
            return None;
        }
    };
    let mem = i.mem(cx, 2)?;
    let (kind, off) = match &mem.kind {
        MemKind::Offset(off) => (
            if matches!(i.mnemonic, "ldnp" | "stnp") {
                0
            } else {
                2
            },
            *off,
        ),
        MemKind::PreIndex(e) => (3, Some(*e)),
        MemKind::PostIndex(e) => (1, Some(*e)),
        MemKind::Reg(_) => {
            cx.error(mem.span, "a pair access cannot use a register offset");
            return None;
        }
        MemKind::OffsetReloc(rop, _) => {
            cx.error(
                mem.span,
                format!("a pair offset has no relocation for `{}`", rop.name()),
            );
            return None;
        }
    };
    let v_off = match off {
        None => 0,
        Some(e) => {
            let Some(n) = cx.constant(e) else {
                cx.error(mem.span, "a pair offset must be a constant");
                return None;
            };
            n
        }
    };
    let unit = 1i64 << scale;
    if v_off % unit != 0 {
        cx.error(
            mem.span,
            format!("a pair offset must be a multiple of {unit}, but is {v_off}"),
        );
        return None;
    }
    let scaled = v_off / unit;
    if !(-64..=63).contains(&scaled) {
        cx.error(
            mem.span,
            format!(
                "offset {v_off} is out of range: a pair offset must be {}..={} in steps of {unit}",
                -64 * unit,
                63 * unit
            ),
        );
        return None;
    }
    one(field(opc, 30, 2)
        | 0x2800_0000
        | field(u32::from(v), 26, 1)
        | field(kind, 23, 3)
        | field(u32::from(load), 22, 1)
        | field(scaled as u32, 15, 7)
        | field(rt2.num as u32, 10, 5)
        | field(mem.base.num as u32, 5, 5)
        | field(rt.num as u32, 0, 5))
}

/// The `[Xn|SP]` of an access that addresses through a base register and
/// nothing else. The zero register is not one of these: the field reads 31
/// as the stack pointer.
///
/// `zero_offset` says whether an explicit `#0` may be written there. GNU as
/// reads one in the 64-byte accesses and llvm-mc refuses it; neither reads
/// one in `rprfm`.
fn base_only(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>, at: usize, zero_offset: bool) -> Option<Reg> {
    let mem = i.mem(cx, at)?;
    let plain = match mem.kind {
        MemKind::Offset(None) => true,
        MemKind::Offset(Some(e)) => zero_offset && cx.constant(e) == Some(0),
        _ => false,
    };
    if !plain {
        cx.error(
            mem.span,
            format!("`{}` addresses through a base register only", i.mnemonic),
        );
        return None;
    }
    i.no_zr(cx, mem.base)?;
    if mem.base.class != RegClass::X {
        cx.error(mem.span, "an address register is 64-bit");
        return None;
    }
    Some(mem.base)
}

/// FEAT_LS64's 64-byte accesses, which move 512 bits through eight
/// consecutive registers named by the first of them. Only an even register
/// up to `x22` can start such a run, which is what both references demand:
/// `x22` leaves `x29` as the last of the eight, and the odd numbering is not
/// encodable at all.
fn ldst64b(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    let status = matches!(i.mnemonic, "st64bv" | "st64bv0");
    i.arity(cx, if status { &[3] } else { &[2] })
        .then_some(())?;
    let rs = if status {
        let rs = i.gpr(cx, 0)?;
        if rs.class != RegClass::X {
            cx.error(i.ops[0].span, "the status register is 64-bit");
            return None;
        }
        u32::from(rs.num)
    } else {
        31
    };
    let at = usize::from(status);
    let rt = i.gpr(cx, at)?;
    if rt.class != RegClass::X || rt.num % 2 != 0 || rt.num > 22 {
        cx.error(
            i.ops[at].span,
            format!(
                "`{}` transfers eight registers, so it starts at an even `x` register up to `x22`",
                i.mnemonic
            ),
        );
        return None;
    }
    let base = base_only(cx, i, at + 1, true)?;
    let opcode = match i.mnemonic {
        "ld64b" => 0xd0,
        "st64b" => 0x90,
        "st64bv" => 0xb0,
        _ => 0xa0,
    };
    one(0xf820_0000
        | field(rs, 16, 5)
        | field(opcode, 8, 8)
        | field(base.num as u32, 5, 5)
        | field(rt.num as u32, 0, 5))
}

/// FEAT_RPRFM's range prefetch: an operation, the stride that follows it and
/// the address it starts from.
///
/// The operation is a six-bit number whose halves are far apart in the word,
/// or one of the four names the architecture has given values so far; the
/// unnamed values are written as numbers, as both references print them.
fn rprfm(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    let op = i.op(0)?;
    let value = match op.word() {
        Some(n) => match cx.name(n).to_ascii_lowercase().as_str() {
            "pldkeep" => 0,
            "pstkeep" => 1,
            "pldstrm" => 4,
            "pststrm" => 5,
            text => {
                cx.error(
                    op.span,
                    format!("`{text}` is not a range prefetch operation"),
                );
                return None;
            }
        },
        None => i.imm(cx, 0, 0, 63, "a range prefetch operation")?,
    } as u32;
    let rm = i.gpr(cx, 1)?;
    if rm.class != RegClass::X {
        cx.error(i.ops[1].span, "the stride register is 64-bit");
        return None;
    }
    let base = base_only(cx, i, 2, false)?;
    one(0xf8a0_4818
        | field(rm.num as u32, 16, 5)
        | field(value >> 5, 15, 1)
        | field((value >> 3) & 3, 12, 2)
        | field(base.num as u32, 5, 5)
        | field(value & 7, 0, 3))
}

// ---- compare and branch ----------------------------------------------------

/// The condition field of a FEAT_CMPBR comparison of two registers, and
/// whether they swap.
///
/// Only six of the ten comparisons have an encoding. The other four are the
/// same comparison the other way round, and both references read them as
/// that: `cblt w0, w1` is `cbgt w1, w0`.
fn cmpbr_reg_cond(cc: &str) -> Option<(u32, bool)> {
    Some(match cc {
        "gt" => (0, false),
        "ge" => (1, false),
        "hi" => (2, false),
        "hs" => (3, false),
        "eq" => (6, false),
        "ne" => (7, false),
        "lt" => (0, true),
        "le" => (1, true),
        "lo" => (2, true),
        "ls" => (3, true),
        _ => return None,
    })
}

/// The condition field of a comparison against a number, and what the
/// mnemonic adds to the number it is written with.
///
/// Here it is the inclusive comparisons that have no encoding, since the
/// number can be moved instead: `cbge w0, #1` is `cbgt w0, #0`, which is why
/// its range is 1 to 64 where `cbgt`'s is 0 to 63.
fn cmpbr_imm_cond(cc: &str) -> Option<(u32, i64)> {
    Some(match cc {
        "gt" => (0, 0),
        "lt" => (1, 0),
        "hi" => (2, 0),
        "lo" => (3, 0),
        "eq" => (6, 0),
        "ne" => (7, 0),
        "ge" => (0, -1),
        "le" => (1, 1),
        "hs" => (2, -1),
        "ls" => (3, 1),
        _ => return None,
    })
}

/// The size field and the condition name of a FEAT_CMPBR mnemonic, or `None`
/// for a name that is not one of them.
///
/// `cbb<cc>` compares the low byte of two registers and `cbh<cc>` their low
/// halfword; `cb<cc>` compares the whole of them. Neither reference takes
/// `cs` or `cc` for `hs` and `lo` here, although both take them after `b.`.
fn cmpbr_name(mnemonic: &str) -> Option<(u32, &str)> {
    let rest = mnemonic.strip_prefix("cb")?;
    let (size, cc) = match rest.len() {
        2 => (0, rest),
        3 => (
            match rest.as_bytes()[0] {
                b'b' => 0b10,
                b'h' => 0b11,
                _ => return None,
            },
            &rest[1..],
        ),
        _ => return None,
    };
    cmpbr_reg_cond(cc).map(|_| (size, cc))
}

/// FEAT_CMPBR's compare-and-branch: two registers, or a register and a
/// six-bit number, compared and branched on in one instruction.
///
/// The target reaches 1KB either way, which is the narrowest branch field
/// A64 has, and neither reference can relocate one: llvm-mc refuses a target
/// it cannot resolve itself and GNU as writes `R_AARCH64_NONE`, which no
/// linker fills in. [`encode::fixup_b9`] therefore has no relocation, so a
/// target rsasm cannot resolve is an error rather than a branch to itself.
fn cmpbr(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>, size: u32, cc: &str) -> Option<Vec<Variant>> {
    i.arity(cx, &[3]).then_some(())?;
    let rt = i.gpr(cx, 0)?;
    let w = if i.op(1).and_then(|o| o.reg()).is_some() {
        let (cond, swap) = cmpbr_reg_cond(cc)?;
        let rm = i.gpr(cx, 1)?;
        if rm.class != rt.class {
            cx.error(
                i.ops[1].span,
                format!("`{}` compares two registers of one width", i.mnemonic),
            );
            return None;
        }
        if size != 0 && rt.class == RegClass::X {
            cx.error(
                i.ops[0].span,
                format!(
                    "`{}` compares part of a register, so it takes the 32-bit name",
                    i.mnemonic
                ),
            );
            return None;
        }
        let (first, second) = if swap { (rm, rt) } else { (rt, rm) };
        0x7400_0000
            | field(rt.sf(), 31, 1)
            | field(cond, 21, 3)
            | field(u32::from(second.num), 16, 5)
            | field(size, 14, 2)
            | field(u32::from(first.num), 0, 5)
    } else {
        if size != 0 {
            cx.error(
                i.ops[1].span,
                format!("`{}` compares two registers", i.mnemonic),
            );
            return None;
        }
        let (cond, bias) = cmpbr_imm_cond(cc)?;
        let value = i.imm(cx, 1, -bias, 63 - bias, "a comparison value")?;
        0x7500_0000
            | field(rt.sf(), 31, 1)
            | field(cond, 21, 3)
            | field((value + bias) as u32, 15, 6)
            | field(u32::from(rt.num), 0, 5)
    };
    let e = i.pcrel_expr(cx, 2)?;
    pcrel(cx, w, e, encode::fixup_b9(), i.ops[2].span)
}

// ---- memory copy and memory set --------------------------------------------

/// Which registers a FEAT_MOPS mnemonic names.
#[derive(Copy, Clone, PartialEq, Eq)]
enum MopsForm {
    /// `cpyfp [x0]!, [x1]!, x2!`: a destination address, a source address and
    /// a size, each counted on by the instruction and so each written back.
    Copy,
    /// `setp [x0]!, x1!, x2`: a destination address and a size, both written
    /// back, and the byte to store, which is not.
    Set,
    /// `setgop [x0]!, x1!`: FEAT_MOPS_GO's granule-only set, which writes
    /// tags rather than data and so names no byte at all.
    GranuleOnly,
}

/// The opcode and operand shape of a FEAT_MOPS mnemonic, or `None` for a name
/// that is not one.
///
/// There are 132 of these because every hint is spelled into the name: a copy
/// names one for its source and one for its destination, a set names one
/// pair, and each operation has a prologue, a main body and an epilogue that
/// resume one another. binutils builds the names from macros for that reason,
/// and this builds them from the same pieces rather than listing them.
fn mops(mnemonic: &str) -> Option<(u32, MopsForm)> {
    // The hints, in the order a name spells them. A copy's source hints
    // reach bits 13-12 and its destination hints bits 15-14; a set has one
    // pair of hints over both fields at once.
    const SOURCE: [&str; 4] = ["", "wt", "rt", "t"];
    const DEST: [&str; 4] = ["", "wn", "rn", "n"];
    const SET: [&str; 4] = ["", "t", "n", "tn"];

    let (rest, base, form) = if let Some(rest) = mnemonic.strip_prefix("cpyf") {
        (rest, 0x1900_0400, MopsForm::Copy)
    } else if let Some(rest) = mnemonic.strip_prefix("cpy") {
        (rest, 0x1d00_0400, MopsForm::Copy)
    } else if let Some(rest) = mnemonic.strip_prefix("setgo") {
        // The granule-only set is the tagged set with the byte's register
        // field reading the zero register and the bit at 10 clear.
        (rest, 0x1dc0_0000 | field(31, 16, 5), MopsForm::GranuleOnly)
    } else if let Some(rest) = mnemonic.strip_prefix("setg") {
        (rest, 0x1dc0_0400, MopsForm::Set)
    } else {
        (mnemonic.strip_prefix("set")?, 0x19c0_0400, MopsForm::Set)
    };
    let (stage, hints) = rest.split_at_checked(1)?;
    let stage = match stage {
        "p" => 0,
        "m" => 1,
        "e" => 2,
        _ => return None,
    };
    if form == MopsForm::Copy {
        // A copy holds its stage above the registers, in bits 23-22.
        let (source, dest) = SOURCE.iter().enumerate().find_map(|(s, src)| {
            let tail = hints.strip_prefix(src)?;
            let d = DEST.iter().position(|dst| *dst == tail)?;
            Some((s as u32, d as u32))
        })?;
        return Some((
            base | field(stage, 22, 2) | field(dest, 14, 2) | field(source, 12, 2),
            form,
        ));
    }
    // A set holds its stage where a copy holds its destination hints.
    let hints = SET.iter().position(|h| *h == hints)? as u32;
    Some((base | field(stage, 14, 2) | field(hints, 12, 2), form))
}

/// True for one of FEAT_MOPS's 132 names, which `mod.rs` dispatches before
/// the shared operand parser runs.
pub(crate) fn is_mops(mnemonic: &str) -> bool {
    mops(mnemonic).is_some()
}

/// FEAT_MOPS's memory copies and memory sets.
///
/// These are here rather than in the measured table for two reasons a table
/// row cannot hold. Each address is written back, so it is spelled `[x0]!`
/// with no offset for the `!` to apply to, and so is the size, spelled `x2!`
/// with no brackets at all; and both references refuse any two of the
/// registers naming one register, since an instruction that overwrote its own
/// size or source could not resume.
pub(crate) fn mops_insn(
    cx: &mut AsmCtx<'_>,
    req: &InsnRequest<'_>,
    mnemonic: &str,
) -> Option<Vec<Variant>> {
    let (base, form) = mops(mnemonic)?;
    let want: usize = match form {
        MopsForm::Copy | MopsForm::Set => 3,
        MopsForm::GranuleOnly => 2,
    };
    let cur = Cursor::new(req.operands);
    let pieces = cur.split_commas();
    if pieces.len() != want {
        cx.error(
            req.span,
            format!(
                "`{mnemonic}` takes {want} operand(s), but {} were given",
                pieces.len()
            ),
        );
        return None;
    }
    // In the word the destination address is `Rd`, the size is `Rn`, and
    // `Rs` is the source address of a copy or the byte of a set.
    let dest = mops_address(cx, mnemonic, pieces[0])?;
    let (size, source) = match form {
        MopsForm::Copy => (
            mops_counted(cx, mnemonic, pieces[2], true)?,
            mops_address(cx, mnemonic, pieces[1])?,
        ),
        MopsForm::Set => (
            mops_counted(cx, mnemonic, pieces[1], true)?,
            mops_counted(cx, mnemonic, pieces[2], false)?,
        ),
        MopsForm::GranuleOnly => (mops_counted(cx, mnemonic, pieces[1], true)?, 31),
    };
    let named: &[u32] = match form {
        MopsForm::GranuleOnly => &[dest, size],
        _ => &[dest, size, source],
    };
    for (k, r) in named.iter().enumerate() {
        if named[..k].contains(r) {
            cx.error(
                req.span,
                format!(
                    "`{mnemonic}` names one register twice: each of its registers \
                     is counted on, so they have to differ"
                ),
            );
            return None;
        }
    }
    one(base | field(source, 16, 5) | field(size, 5, 5) | field(dest, 0, 5))
}

/// `[x0]!`: one of the addresses a copy or a set works through, which it
/// always writes back. Register 31 is refused by both references here, in
/// either spelling, since an address is no use read as zero.
fn mops_address(cx: &mut AsmCtx<'_>, mnemonic: &str, toks: &[Token]) -> Option<u32> {
    let reg = match toks {
        [open, name, close, bang]
            if open.is_punct(Punct::LBracket)
                && close.is_punct(Punct::RBracket)
                && bang.is_punct(Punct::Bang) =>
        {
            match name.kind {
                TokKind::Ident(n) => reg::lookup(&cx.name(n).to_ascii_lowercase()),
                _ => None,
            }
        }
        _ => None,
    };
    match reg {
        Some(r) if r.class == RegClass::X && r.num != 31 => Some(u32::from(r.num)),
        _ => {
            cx.error(
                mops_span(toks),
                format!("`{mnemonic}` takes an address written back, `[x0]!`, over `x0` to `x30`"),
            );
            None
        }
    }
}

/// `x2!`, the size a copy or a set counts down, or the `x2` a set stores. The
/// zero register is a register like any other in both fields.
fn mops_counted(
    cx: &mut AsmCtx<'_>,
    mnemonic: &str,
    toks: &[Token],
    writeback: bool,
) -> Option<u32> {
    let name = match toks {
        [name] if !writeback => Some(name),
        [name, bang] if writeback && bang.is_punct(Punct::Bang) => Some(name),
        _ => None,
    };
    let reg = match name.map(|t| t.kind) {
        Some(TokKind::Ident(n)) => reg::lookup(&cx.name(n).to_ascii_lowercase()),
        _ => None,
    };
    match reg {
        Some(r) if r.class == RegClass::X && !r.is_sp() => Some(u32::from(r.num)),
        _ => {
            cx.error(
                mops_span(toks),
                if writeback {
                    format!("`{mnemonic}` takes a size written back, `x2!`")
                } else {
                    format!("`{mnemonic}` takes a 64-bit register for the byte it stores")
                },
            );
            None
        }
    }
}

fn mops_span(toks: &[Token]) -> Span {
    match (toks.first(), toks.last()) {
        (Some(a), Some(b)) => a.span.to(b.span),
        _ => Span::DUMMY,
    }
}

// ---- the newest extensions, one or two forms each ---------------------------

/// FEAT_GCS's guarded call stack.
///
/// `gcspushm` and `gcsss1` push, `gcspopm` and `gcsss2` pop, the two stores
/// write through the stack's own permissions, and the three `x` forms move
/// the exception-return state, which is not a value anything names: they
/// have no operand at all. `gcspopm` with no operand is the same word as
/// `gcspopm xzr`, which is how both references print it, so the register is
/// optional here rather than a form of its own.
fn gcs(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    // The two stores name a register and an address, and are the only forms
    // here outside the system encoding space.
    if let Some(store) = match i.mnemonic {
        "gcsstr" => Some(0xd91f_0c00u32),
        "gcssttr" => Some(0xd91f_1c00),
        _ => None,
    } {
        i.arity(cx, &[2]).then_some(())?;
        let rt = i.xreg(cx, 0)?;
        let addr = base_only(cx, i, 1, true)?;
        return one(store | field(u32::from(addr.num), 5, 5) | field(rt, 0, 5));
    }
    if let Some(word) = match i.mnemonic {
        "gcspushx" => Some(0xd508_779fu32),
        "gcspopcx" => Some(0xd508_77bf),
        "gcspopx" => Some(0xd508_77df),
        _ => None,
    } {
        i.arity(cx, &[0]).then_some(())?;
        return one(word);
    }
    let base: u32 = match i.mnemonic {
        "gcspushm" => 0xd50b_7700,
        "gcsss1" => 0xd50b_7740,
        "gcspopm" => 0xd52b_7720,
        _ => 0xd52b_7760,
    };
    if i.mnemonic == "gcspopm" && i.ops.is_empty() {
        return one(base | field(31, 0, 5));
    }
    i.arity(cx, &[1]).then_some(())?;
    one(base | field(i.xreg(cx, 0)?, 0, 5))
}

/// FEAT_TME's transactions: `tstart` and `ttest` write a result to a
/// register, and `tcancel` names the 16-bit reason it fails with.
///
/// llvm-mc 22 does not know TME at all, so these encodings and the corpus
/// lines for them are GNU as's.
fn tme(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[1]).then_some(())?;
    if i.mnemonic == "tcancel" {
        let reason = i.imm(cx, 0, 0, 0xffff, "a transaction cancellation reason")? as u32;
        return one(0xd460_0000 | field(reason, 5, 16));
    }
    let base: u32 = if i.mnemonic == "tstart" {
        0xd523_3060
    } else {
        0xd523_3160
    };
    one(base | field(i.xreg(cx, 0)?, 0, 5))
}

/// FEAT_ITE's trace instrumentation, which records one register's value in
/// the trace stream.
fn trcit(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[1]).then_some(())?;
    one(0xd50b_72e0 | field(i.xreg(cx, 0)?, 0, 5))
}

/// FEAT_TEV's and FEAT_POE2's thread switches: `tenter` and `texit` enter
/// and leave a thread, and `tchangef` and `tchangeb` move to the one in
/// front of or behind the current one, named by a register or by a number.
///
/// Each takes an optional `nb` at the end, which says the switch is not
/// balanced by its opposite.
fn tev(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    match i.mnemonic {
        "texit" => {
            i.arity(cx, &[0, 1]).then_some(())?;
            one(0xd6ff_03e0 | field(not_balanced(cx, i, 0)?, 10, 1))
        }
        "tenter" => {
            i.arity(cx, &[1, 2]).then_some(())?;
            let index = i.imm(cx, 0, 0, 127, "a thread index")? as u32;
            one(0xd4e0_0000 | field(not_balanced(cx, i, 1)?, 17, 1) | field(index, 5, 7))
        }
        _ => {
            i.arity(cx, &[2, 3]).then_some(())?;
            let forward = i.mnemonic == "tchangef";
            let rd = i.xreg(cx, 0)?;
            let nb = field(not_balanced(cx, i, 2)?, 17, 1);
            let (base, source) = match i.op(1).and_then(|o| o.reg()) {
                Some(_) => (
                    if forward { 0xd580_0000u32 } else { 0xd584_0000 },
                    i.xreg(cx, 1)?,
                ),
                None => (
                    if forward { 0xd590_0000 } else { 0xd594_0000 },
                    i.imm(cx, 1, 0, 127, "a thread index")? as u32,
                ),
            };
            one(base | nb | field(source, 5, 7) | field(rd, 0, 5))
        }
    }
}

/// The `nb` a thread switch may end with, or 0 where it does not.
fn not_balanced(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>, at: usize) -> Option<u32> {
    let Some(op) = i.ops.get(at) else {
        return Some(0);
    };
    match op.word() {
        Some(n) if cx.name(n).eq_ignore_ascii_case("nb") => Some(1),
        _ => {
            cx.error(
                op.span,
                format!("the last operand of `{}` is `nb` or nothing", i.mnemonic),
            );
            None
        }
    }
}

// ---- system ----------------------------------------------------------------

/// `hint #n`, the instruction every no-operand alias in the generated table
/// is one of; the named spellings go through [`hint_alias`].
fn hint(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[1]).then_some(())?;
    let imm = i.imm(cx, 0, 0, 127, "a hint number")? as u32;
    one(0xd503_201f | field(imm, 5, 7))
}

/// The name of an operand written as a bare word, or as the one register
/// that is a name here (`chkfeat x16`).
fn option_name(cx: &mut AsmCtx<'_>, op: &Operand<'_>) -> Option<String> {
    if let Some(n) = op.word() {
        return Some(cx.name(n).to_ascii_lowercase());
    }
    op.reg().map(|r| r.name())
}

/// The aliases of `hint` and of the barriers: a mnemonic on its own (`esb`,
/// `sb`, `paciasp`) or with one named operand (`psb csync`, `bti c`), whose
/// word the generated table holds.
fn hint_alias(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[0, 1]).then_some(())?;
    let name = match i.op(0) {
        None => String::new(),
        Some(op) => match option_name(cx, op) {
            Some(text) => text,
            None => {
                cx.error(op.span, format!("expected a `{}` operand", i.mnemonic));
                return None;
            }
        },
    };
    let span = i.op(0).map_or(i.span, |op| op.span);
    // A name the target has not is reported by `hint` itself, so the error
    // below is only for a name no target has.
    if !sysreg::knows_hint(i.mnemonic, &name) {
        let options = sysreg::hint_options(i.mnemonic);
        cx.error(
            span,
            if options.is_empty() {
                format!("`{}` takes no operand", i.mnemonic)
            } else if name.is_empty() {
                format!("`{}` takes {}", i.mnemonic, options.join(" or "))
            } else {
                format!(
                    "`{name}` is not an operand of `{}`; it takes {}",
                    i.mnemonic,
                    options.join(" or ")
                )
            },
        );
        return None;
    }
    one(sysreg::hint(cx, span, i.mnemonic, &name)?)
}

/// `dmb`, `dsb`, `isb` and `clrex`, whose option is a name in the table or
/// the `CRm` number itself. `dsb` takes a fifth bit for the nXS variants,
/// which have names of their own and only four values.
fn barrier(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[0, 1]).then_some(())?;
    if i.op(0).is_none_or(|op| option_name(cx, op).is_some()) {
        return hint_alias(cx, i);
    }
    let nxs = i.mnemonic == "dsb";
    let imm = i.imm(cx, 0, 0, if nxs { 31 } else { 15 }, "a barrier option")? as u32;
    if imm > 15 {
        // `dsb #16` and its three neighbours are the nXS barriers; nothing
        // between them is a barrier at all.
        let name = match imm {
            16 => "oshnxs",
            20 => "nshnxs",
            24 => "ishnxs",
            28 => "synxs",
            _ => {
                cx.error(
                    i.ops[0].span,
                    "the nXS barriers are `dsb` 16, 20, 24 and 28",
                );
                return None;
            }
        };
        return one(sysreg::hint(cx, i.ops[0].span, "dsb", name)?);
    }
    let op2 = match i.mnemonic {
        "clrex" => 2,
        "dsb" => 4,
        "dmb" => 5,
        _ => 6,
    };
    one(0xd503_301f | field(imm, 8, 4) | field(op2, 5, 3))
}

/// `dc`, `ic`, `at`, `tlbi` and the rest: a name for a `sys` word, which
/// decides whether an address register follows it.
fn sys_alias(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[1, 2]).then_some(())?;
    let op = i.op(0)?;
    let Some(name) = op.word().map(|n| cx.name(n).to_ascii_lowercase()) else {
        cx.error(op.span, format!("expected a `{}` operand name", i.mnemonic));
        return None;
    };
    let has_register = i.op(1).is_some();
    let Some((bits, xt)) = sysreg::sys_ins(cx, op.span, i.mnemonic, &name, has_register) else {
        cx.error(
            op.span,
            format!("`{name}` is not an operand of `{}`", i.mnemonic),
        );
        return None;
    };
    let rt = match (i.op(1), xt) {
        (Some(second), sysreg::Xt::None) => {
            cx.error(
                second.span,
                format!("`{} {name}` takes no register", i.mnemonic),
            );
            return None;
        }
        (None, sysreg::Xt::Needs) => {
            cx.error(
                i.span,
                format!("`{} {name}` needs an address register", i.mnemonic),
            );
            return None;
        }
        // A name that takes no register, or may have none, has `xzr` in the
        // field, which is what GNU as writes there.
        (None, _) => 31,
        (Some(_), _) => {
            let r = i.gpr(cx, 1)?;
            if r.class != RegClass::X {
                cx.error(
                    i.ops[1].span,
                    "a system instruction takes a 64-bit register",
                );
                return None;
            }
            u32::from(r.num)
        }
    };
    one(bits | field(rt, 0, 5))
}

/// A `CRn`/`CRm` operand: `c0` through `c15`.
fn creg(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>, at: usize) -> Option<u32> {
    let op = i.op(at)?;
    let name = op.word().map(|n| cx.name(n).to_ascii_lowercase());
    let number = name
        .as_deref()
        .and_then(|t| t.strip_prefix('c'))
        // `c01` is not a name for `c1`, any more than `x01` is for `x1`.
        .filter(|rest| *rest == "0" || !rest.starts_with('0'))
        .and_then(|rest| rest.parse::<u32>().ok())
        .filter(|n| *n < 16);
    match number {
        Some(n) => Some(n),
        None => {
            cx.error(op.span, "expected `c0` through `c15`");
            None
        }
    }
}

/// `sys #op1, Cn, Cm, #op2{, Xt}` and `sysl Xt, #op1, Cn, Cm, #op2`: the
/// system instruction with no name at all, which every `dc`, `ic`, `at` and
/// `tlbi` is an alias of.
fn sys_raw(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    let reading = i.mnemonic == "sysl";
    i.arity(cx, if reading { &[5] } else { &[4, 5] })
        .then_some(())?;
    let at = usize::from(reading);
    let op1 = i.imm(cx, at, 0, 7, "`op1`")? as u32;
    let crn = creg(cx, i, at + 1)?;
    let crm = creg(cx, i, at + 2)?;
    let op2 = i.imm(cx, at + 3, 0, 7, "`op2`")? as u32;
    let rt = match i.op(if reading { 0 } else { 4 }) {
        None => 31,
        Some(_) => {
            let r = i.gpr(cx, if reading { 0 } else { 4 })?;
            if r.class != RegClass::X {
                cx.error(i.span, "a system instruction takes a 64-bit register");
                return None;
            }
            u32::from(r.num)
        }
    };
    let base = if reading { sysreg::SYSL } else { sysreg::SYS };
    one(base | field(op1, 16, 3) | field(crn, 12, 4) | field(crm, 8, 4) | field(op2, 5, 3) | rt)
}

fn exception(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    let dcps = i.mnemonic.starts_with("dcps");
    i.arity(cx, if dcps { &[0, 1] } else { &[1] })
        .then_some(())?;
    let imm = match i.op(0) {
        Some(_) => i.imm(cx, 0, 0, 0xffff, "an exception number")? as u32,
        None => 0,
    };
    let base = match i.mnemonic {
        "svc" => 0xd400_0001,
        "hvc" => 0xd400_0002,
        "smc" => 0xd400_0003,
        "brk" => 0xd420_0000,
        "hlt" => 0xd440_0000,
        "dcps1" => 0xd4a0_0001,
        "dcps2" => 0xd4a0_0002,
        _ => 0xd4a0_0003,
    };
    one(base | field(imm, 5, 16))
}

/// `udf #imm16`, the word the architecture promises will never decode.
///
/// Nothing in it is an opcode: the sixteen bits of the immediate are the low
/// half of the word and the high half is zero, so `udf #0` assembles to four
/// zero bytes.
fn udf(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[1]).then_some(())?;
    let imm = i.imm(cx, 0, 0, 0xffff, "an undefined-instruction number")? as u32;
    one(field(imm, 0, 16))
}

/// FEAT_WFxT's `wfet` and `wfit`, which wait as `wfe` and `wfi` do but give
/// up when the register's timeout passes. Their operand is why they are not
/// aliases of `hint` like the rest of that group.
fn wfxt(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[1]).then_some(())?;
    let rd = i.gpr(cx, 0)?;
    if rd.class != RegClass::X {
        cx.error(
            i.ops[0].span,
            format!("`{}` takes a 64-bit register", i.mnemonic),
        );
        return None;
    }
    let base = if i.mnemonic == "wfet" {
        0xd503_1000
    } else {
        0xd503_1020
    };
    one(base | field(rd.num as u32, 0, 5))
}

/// FEAT_FlagM and FEAT_FlagM2, which read and write the condition flags
/// directly: `rmif` rotates a register into them under a mask, `setf8` and
/// `setf16` set them from a narrow result, and the other three rearrange
/// what is already there. They share a feature rather than an operand shape,
/// so each is written out here.
fn flags(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    match i.mnemonic {
        "rmif" => {
            i.arity(cx, &[3]).then_some(())?;
            let rn = i.gpr(cx, 0)?;
            if rn.class != RegClass::X {
                cx.error(i.ops[0].span, "`rmif` rotates a 64-bit register");
                return None;
            }
            let rotate = i.imm(cx, 1, 0, 63, "the rotation")? as u32;
            let mask = i.imm(cx, 2, 0, 15, "the flag mask")? as u32;
            one(0xba00_0400 | field(rotate, 15, 6) | field(rn.num as u32, 5, 5) | mask)
        }
        "setf8" | "setf16" => {
            i.arity(cx, &[1]).then_some(())?;
            let rn = i.gpr(cx, 0)?;
            if rn.class != RegClass::W {
                cx.error(
                    i.ops[0].span,
                    format!("`{}` reads a 32-bit register", i.mnemonic),
                );
                return None;
            }
            let base = if i.mnemonic == "setf8" {
                0x3a00_080d
            } else {
                0x3a00_480d
            };
            one(base | field(rn.num as u32, 5, 5))
        }
        _ => {
            i.arity(cx, &[0]).then_some(())?;
            one(match i.mnemonic {
                "cfinv" => 0xd500_401f,
                "axflag" => 0xd500_405f,
                _ => 0xd500_403f,
            })
        }
    }
}

fn mrs(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let rt = i.gpr(cx, 0)?;
    if rt.class != RegClass::X {
        cx.error(i.ops[0].span, "`mrs` writes a 64-bit register");
        return None;
    }
    let enc = sysreg::register(cx, i.op(1)?, Some(false))?;
    one(0xd530_0000 | enc | field(rt.num as u32, 0, 5))
}

/// `smstart`/`smstop`: the `msr svcr…` writes that enter and leave SME's
/// streaming mode (`sm`), enable its ZA storage (`za`), or with no operand,
/// both. `CRm` holds which in its middle bits and start or stop in its low one.
fn sme_mode(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[0, 1]).then_some(())?;
    let which = match i.op(0) {
        None => 0b11,
        Some(op) => match op
            .word()
            .map(|n| cx.name(n).to_ascii_lowercase())
            .as_deref()
        {
            Some("sm") => 0b01,
            Some("za") => 0b10,
            _ => {
                cx.error(
                    op.span,
                    format!("`{}` takes `sm`, `za` or nothing", i.mnemonic),
                );
                return None;
            }
        },
    };
    let start = u32::from(i.mnemonic == "smstart");
    one(0xd503_407f | field(which << 1 | start, 8, 4))
}

/// `zero {za}`, which clears SME's ZA storage. The list is the whole
/// operand: the older per-tile spellings and SME2's `zt0` are not taken.
pub(crate) fn sme_zero(cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
    let toks = req.operands;
    let za = match toks {
        [open, name, close] if open.is_punct(Punct::LBrace) && close.is_punct(Punct::RBrace) => {
            matches!(name.kind, TokKind::Ident(n) if cx.name(n).eq_ignore_ascii_case("za"))
        }
        _ => false,
    };
    if !za {
        let span = toks
            .first()
            .map_or(req.span, |t| t.span.to(toks[toks.len() - 1].span));
        cx.error(span, "`zero` takes `{za}`");
        return None;
    }
    one(0xc008_00ff)
}

fn msr(cx: &mut AsmCtx<'_>, i: &Insn<'_, '_>) -> Option<Vec<Variant>> {
    i.arity(cx, &[2]).then_some(())?;
    let dst = i.op(0)?;
    // `msr <pstate-field>, #imm` is a different encoding from `msr <sysreg>,
    // Xt`. `spsel` names both a PSTATE field and a system register, so the
    // source operand decides.
    if let Some(n) = dst.word()
        && i.op(1).is_some_and(|o| o.reg().is_none())
    {
        let text = cx.name(n).to_ascii_lowercase();
        if let Some((word, lsb, max)) = sysreg::pstate_field(&text) {
            // The field decides how much of `CRm` the immediate is: a bit
            // for the one-bit fields and for SME's mode switches, all four
            // for `daifset` and `daifclr`.
            let imm = i.imm(cx, 1, 0, max, "a PSTATE value")? as u32;
            return one(word | imm << lsb);
        }
    }
    let enc = sysreg::register(cx, dst, Some(true))?;
    let rt = i.gpr(cx, 1)?;
    if rt.class != RegClass::X {
        cx.error(i.ops[1].span, "`msr` reads a 64-bit register");
        return None;
    }
    one(0xd510_0000 | enc | field(rt.num as u32, 0, 5))
}
