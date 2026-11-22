//! `asm!` operands checked (INTRINSICS.md P9b): the Reference, inline-assembly, "Operand type"
//! and "Register operands". The body form is blaze-lower's `AsmForm` converted (its
//! `typeck/asm.rs`): pieces, operands, options and `clobber_abi` names, the text of every
//! piece and name in [`BodyView::asm_text`] (already cooked: escapes applied, `{{` one brace).
//!
//! The architecture is the body's, read by the lowering as `cfg` atoms at the `asm!` itself
//! (main's ruling (A)): the record is filed under those reads, so the table used here is part
//! of what the record's key says.
//!
//! The rules:
//! - "Operand type": `in`/`out`/`lateout`/`inout`/`inlateout` take a register class or an
//!   explicit register; an output is "an assignable place" or `_` (discarded); `sym <path>`
//!   "must refer to a fn or static"; `const <expr>` "must be an integer constant expression";
//!   `label <block>`: "the block must have unit type or diverge".
//! - "Register operands", "Only the following types are allowed as operands for inline
//!   assembly: integers (signed and unsigned), floating-point numbers, pointers (thin only),
//!   function pointers, SIMD vector types", "Pointer-sized integers and pointers ... are treated
//!   as integers of the target's pointer width" (64 bits on both tabled targets), and the
//!   per-class table:
//!   AArch64 `reg`: i8 i16 i32 f32 i64 f64; `vreg`, `vreg_low16` (`neon`): i8 i16 i32 f32 i64
//!   f64 i8x8 i16x4 i32x2 i64x1 f32x2 f64x1 i8x16 i16x8 i32x4 i64x2 f32x4 f64x2.
//!   x86-64 `reg`, `reg_abcd`: i16 i32 f32 i64 f64; `reg_byte`: i8; `xmm_reg` (`sse`): i32 i64
//!   f32 f64 and the 128-bit vectors.
//!   `neon` and `sse` are in every tabled target's base set (blaze-scope task/lower.rs:
//!   `AARCH64_APPLE_DARWIN` holds `neon`; x86-64's base holds `sse2`); classes that need a
//!   feature beyond the base (`ymm_reg`, `zmm_reg`, `kreg`) are refused by name, since the
//!   function's `target_feature` set is not an input of this check yet.
//! - inout split (`inout(reg) a => b`): "<in expr> and <out expr> may have different types";
//!   both must be allowed by the class, and of one register width (rustc: "incompatible types
//!   for asm inout argument"): checked as one asm type.
//! - "Register names": a reserved register is no operand (AArch64: `x19` "used internally by
//!   LLVM as a base pointer", `x29`/`fp` the frame pointer, `sp`, `xzr`; x86-64: `bx`'s family
//!   "used internally by LLVM", `bp`'s family the frame pointer, `sp`'s, `ip`'s); AArch64
//!   `x18` is reserved only on some operating systems, which the record has not read: refused
//!   by name. "It is a compile-time error to use the same explicit register for two input
//!   operands or two output operands."
//! - The type is `!` with `noreturn`, else `()`.
//!
//! Template modifiers are not checked here (blaze-lower kept them; the builder, P10, maps
//! them): a modifier the class lacks reaches the builder, which refuses it by name.

use crate::body::{BodyView, Expr};
use crate::diag::{About, At, DiagCode};
use crate::error::{CheckError, Stop};
use crate::ids::{ExprId, FloatTy, IntTy, List, Name, UintTy};
use crate::machine::{Expectation, Flow, Frame, Lower, Machine, Then};
use crate::ty::{Cause, Ty, TyView, TypeCtx};
use crate::world::{Def, TypeAttrs, World};

/// The architecture an `asm!` is checked for; the lowering's `target_arch` reads chose it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AsmArch {
    Aarch64,
    X86_64,
}

/// `noreturn` in [`Expr::Asm`]'s `options`: the bits are blaze-lower `AsmForm`'s, which equal
/// blaze-lir `AsmRec`'s.
pub const ASM_NORETURN: u16 = 1 << 4;

/// How an operand passes its value (the Reference, "Operand type").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AsmKind {
    In,
    Out,
    LateOut,
    InOut,
    InLateOut,
    Sym,
    Const,
    Label,
}

impl AsmKind {
    /// Whether the register is read at the start.
    pub const fn is_input(self) -> bool {
        matches!(self, AsmKind::In | AsmKind::InOut | AsmKind::InLateOut)
    }

    /// Whether the register is written at the end.
    pub const fn is_output(self) -> bool {
        matches!(self, AsmKind::Out | AsmKind::LateOut | AsmKind::InOut | AsmKind::InLateOut)
    }
}

/// A register operand's `<reg>`: a range of [`BodyView::asm_text`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AsmReg {
    /// `in(reg)`: the class's name.
    Class { first: u32, len: u32 },
    /// `in("x0")`: the string's contents.
    Explicit { first: u32, len: u32 },
}

/// One operand: its kind, its register (only the five register kinds have one), its
/// expression (the input or the place; `_` is [`Expr::Underscore`]; a `const`'s expression; a
/// `sym`'s path; a `label`'s block) and, for `inout(..) a => b`, the output place `b`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AsmOperand {
    pub kind: AsmKind,
    pub reg: Option<AsmReg>,
    pub expr: ExprId,
    pub out_expr: Option<ExprId>,
}

/// One template piece: text (a range of [`BodyView::asm_text`], cooked), a placeholder (the
/// operand's position in the `asm!`'s operands and its modifier), or the `\n` between two
/// template strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AsmPiece {
    Text { first: u32, len: u32 },
    Operand { index: u32, modifier: Option<char> },
    Line,
}

/// A register class of a tabled architecture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AsmClass {
    /// AArch64 `reg` (`x0`..`x30`).
    A64Reg,
    /// AArch64 `vreg` or `vreg_low16` (`v0`..`v31`; the low half is the same types).
    A64Vreg,
    /// x86-64 `reg` or `reg_abcd`.
    X86Reg,
    /// x86-64 `reg_byte`.
    X86RegByte,
    /// x86-64 `xmm_reg`.
    X86Xmm,
}

impl AsmClass {
    /// The class's name, for a finding.
    fn name(self) -> Name {
        Name::of(match self {
            AsmClass::A64Reg | AsmClass::X86Reg => "reg",
            AsmClass::A64Vreg => "vreg",
            AsmClass::X86RegByte => "reg_byte",
            AsmClass::X86Xmm => "xmm_reg",
        })
    }

    /// Whether the class takes a value of asm type `t` (the module's table).
    fn allows(self, t: AsmTy) -> bool {
        use Scalar::*;
        match (self, t) {
            (AsmClass::A64Reg, AsmTy::Scalar(s)) => matches!(s, I8 | I16 | I32 | I64 | F32 | F64),
            (AsmClass::A64Vreg, AsmTy::Scalar(s)) => matches!(s, I8 | I16 | I32 | I64 | F32 | F64),
            (AsmClass::A64Vreg, AsmTy::Vector { elem, lanes }) => {
                matches!(elem, I8 | I16 | I32 | I64 | F32 | F64) && matches!(elem.bits() * lanes, 64 | 128)
            }
            (AsmClass::X86Reg, AsmTy::Scalar(s)) => matches!(s, I16 | I32 | I64 | F32 | F64),
            (AsmClass::X86RegByte, AsmTy::Scalar(s)) => s == I8,
            (AsmClass::X86Xmm, AsmTy::Scalar(s)) => matches!(s, I32 | I64 | F32 | F64),
            (AsmClass::X86Xmm, AsmTy::Vector { elem, lanes }) => {
                matches!(elem, I8 | I16 | I32 | I64 | F32 | F64) && elem.bits() * lanes == 128
            }
            _ => false,
        }
    }
}

/// A scalar as inline assembly sees it: signedness is not part of it ("integers (signed and
/// unsigned)").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scalar {
    I8,
    I16,
    I32,
    I64,
    I128,
    F16,
    F32,
    F64,
    F128,
}

impl Scalar {
    const fn bits(self) -> u32 {
        match self {
            Scalar::I8 => 8,
            Scalar::I16 | Scalar::F16 => 16,
            Scalar::I32 | Scalar::F32 => 32,
            Scalar::I64 | Scalar::F64 => 64,
            Scalar::I128 | Scalar::F128 => 128,
        }
    }
}

/// An operand's type as the class table reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AsmTy {
    Scalar(Scalar),
    Vector { elem: Scalar, lanes: u32 },
}

/// What a class name or an explicit register names on an architecture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegLookup {
    /// A class, and for an explicit register its bank and number (two operands on one
    /// register are found by them).
    Class(AsmClass, Option<(u8, u8)>),
    /// No class or register of the architecture.
    Unknown,
    /// A reserved register (the Reference, "Register names").
    Reserved,
    /// Not tabled yet, by name.
    Unported(&'static str),
}

const GPR: u8 = 0;
const VEC: u8 = 1;

/// `prefix` then `n` in decimal is `name`.
fn numbered(name: &str, prefix: &str, max: u8) -> Option<u8> {
    let digits = name.strip_prefix(prefix)?;
    if digits.is_empty() || digits.len() > 2 || (digits.len() == 2 && digits.starts_with('0')) {
        return None;
    }
    let n: u8 = digits.bytes().try_fold(0u8, |acc, b| if b.is_ascii_digit() { Some(acc * 10 + (b - b'0')) } else { None })?;
    (n <= max).then_some(n)
}

fn class_named(arch: AsmArch, name: &str) -> RegLookup {
    let class = match (arch, name) {
        (AsmArch::Aarch64, "reg") => AsmClass::A64Reg,
        (AsmArch::Aarch64, "vreg" | "vreg_low16") => AsmClass::A64Vreg,
        (AsmArch::X86_64, "reg" | "reg_abcd") => AsmClass::X86Reg,
        (AsmArch::X86_64, "reg_byte") => AsmClass::X86RegByte,
        (AsmArch::X86_64, "xmm_reg") => AsmClass::X86Xmm,
        (AsmArch::X86_64, "ymm_reg" | "zmm_reg" | "kreg" | "kreg0") => {
            return RegLookup::Unported("an `asm!` register class that needs a target feature beyond the base set");
        }
        // Clobber-only classes ("Only clobbers") and every other name: no operand class.
        _ => return RegLookup::Unknown,
    };
    RegLookup::Class(class, None)
}

fn explicit_named(arch: AsmArch, name: &str) -> RegLookup {
    match arch {
        AsmArch::Aarch64 => {
            let gpr = numbered(name, "x", 30).or_else(|| numbered(name, "w", 30)).or(match name {
                "fp" => Some(29),
                "lr" => Some(30),
                _ => None,
            });
            if let Some(n) = gpr {
                return match n {
                    18 => RegLookup::Unported("`x18` as an `asm!` operand (reserved on some operating systems; the record reads none)"),
                    19 | 29 => RegLookup::Reserved,
                    _ => RegLookup::Class(AsmClass::A64Reg, Some((GPR, n))),
                };
            }
            if matches!(name, "sp" | "wsp" | "xzr" | "wzr") {
                return RegLookup::Reserved;
            }
            let v = ["v", "q", "d", "s", "h", "b"].into_iter().find_map(|p| numbered(name, p, 31));
            match v {
                Some(n) => RegLookup::Class(AsmClass::A64Vreg, Some((VEC, n))),
                None => RegLookup::Unknown,
            }
        }
        AsmArch::X86_64 => {
            // rax rcx rdx (numbers 0 1 2), rsi rdi (6 7), then r8..r15, in each width.
            const LEGACY: [(&str, &str, &str, &str, u8); 5] = [
                ("rax", "eax", "ax", "al", 0),
                ("rcx", "ecx", "cx", "cl", 1),
                ("rdx", "edx", "dx", "dl", 2),
                ("rsi", "esi", "si", "sil", 6),
                ("rdi", "edi", "di", "dil", 7),
            ];
            for (q, d, w, b, n) in LEGACY {
                if name == q || name == d || name == w {
                    return RegLookup::Class(AsmClass::X86Reg, Some((GPR, n)));
                }
                if name == b {
                    return RegLookup::Class(AsmClass::X86RegByte, Some((GPR, n)));
                }
            }
            if matches!(
                name,
                "rbx" | "ebx" | "bx" | "bl" | "rbp" | "ebp" | "bp" | "bpl" | "rsp" | "esp" | "sp" | "spl" | "rip" | "eip" | "ip"
            ) {
                return RegLookup::Reserved;
            }
            if matches!(name, "ah" | "ch" | "dh" | "bh") {
                return RegLookup::Unported("a high-byte register (`ah`..`dh`) as an `asm!` operand");
            }
            for (suffix, class) in [("b", AsmClass::X86RegByte), ("w", AsmClass::X86Reg), ("d", AsmClass::X86Reg), ("", AsmClass::X86Reg)] {
                let Some(stem) = name.strip_suffix(suffix) else { continue };
                if let Some(n) = numbered(stem, "r", 15).filter(|&n| n >= 8) {
                    return RegLookup::Class(class, Some((GPR, n)));
                }
            }
            match numbered(name, "xmm", 15) {
                Some(n) => RegLookup::Class(AsmClass::X86Xmm, Some((VEC, n))),
                None => RegLookup::Unknown,
            }
        }
    }
}

/// A register operand's lookup on `arch`.
fn lookup(body: &BodyView<'_>, arch: AsmArch, reg: AsmReg) -> Result<RegLookup, CheckError> {
    let (first, len, explicit) = match reg {
        AsmReg::Class { first, len } => (first, len, false),
        AsmReg::Explicit { first, len } => (first, len, true),
    };
    let text = body.asm_text_at(first, len)?;
    Ok(if explicit { explicit_named(arch, text) } else { class_named(arch, text) })
}

/// The name a finding about `reg` carries.
fn reg_name(body: &BodyView<'_>, reg: AsmReg) -> Result<Name, CheckError> {
    let (AsmReg::Class { first, len } | AsmReg::Explicit { first, len }) = reg;
    Ok(Name::of(body.asm_text_at(first, len)?))
}

/// A check after fallback (rustc's `InlineAsmCtxt`, run once every type is known).
#[derive(Clone, Copy, Debug)]
pub(crate) enum AsmCheck {
    /// A register operand: its expression's type against `class`; for a split `inout`, the
    /// output's too, which must be the same asm type.
    Reg { expr: ExprId, ty: Ty, class: AsmClass, out: Option<(ExprId, Ty)> },
    /// A `const` operand's type: an integer.
    Const { expr: ExprId, ty: Ty },
}

impl<'b, 'w, 'c, W: World, C: TypeCtx> Machine<'b, 'w, 'c, '_, W, C> {
    /// `asm!`: the registers named (state 0), then each operand's expression in order (`f.a`
    /// the operand, `f.b` 1 while a split `inout`'s output is checked, `f.t` its input's
    /// type); its type is `!` with `noreturn`, else `()`.
    pub(crate) fn expr_asm(&mut self, mut f: Frame, operands: List, arch: AsmArch, options: u16) -> Result<Flow, Stop> {
        let ops = self.body.asm_operand_list(operands)?;
        if f.state == 0 {
            self.asm_registers(ops, arch)?;
            f.a = 0;
            f.b = 0;
        } else {
            let o = *ops.get(f.a as usize).ok_or(CheckError::Malformed(crate::error::Malformed::Expr(ExprId(f.a))))?;
            let t = self.ret;
            let class = match o.reg {
                Some(reg) => match lookup(&self.body, arch, reg)? {
                    RegLookup::Class(c, _) => Some(c),
                    _ => None,
                },
                None => None,
            };
            match o.kind {
                AsmKind::InOut | AsmKind::InLateOut if f.b == 0 => {
                    let out = o.out_expr.filter(|&x| !matches!(self.body.expr(x), Ok(Expr::Underscore)));
                    if let Some(x) = out {
                        f.b = 1;
                        f.t = t;
                        self.check_place_code(x, DiagCode::InvalidLhs)?;
                        self.resume(f, 1)?;
                        self.push_expr(x, Expectation::None, Then::Nothing)?;
                        return Ok(Flow::Wait);
                    }
                    if let Some(class) = class {
                        self.asms.push(AsmCheck::Reg { expr: o.expr, ty: t, class, out: None });
                    }
                }
                AsmKind::InOut | AsmKind::InLateOut => {
                    if let (Some(class), Some(x)) = (class, o.out_expr) {
                        self.asms.push(AsmCheck::Reg { expr: o.expr, ty: f.t, class, out: Some((x, t)) });
                    }
                }
                AsmKind::In | AsmKind::Out | AsmKind::LateOut => {
                    if let Some(class) = class {
                        self.asms.push(AsmCheck::Reg { expr: o.expr, ty: t, class, out: None });
                    }
                }
                AsmKind::Const => self.asms.push(AsmCheck::Const { expr: o.expr, ty: t }),
                AsmKind::Sym => {
                    let at = At::Expr(o.expr);
                    let def = self.defs.iter().rev().find(|(a, _)| *a == at).map(|(_, d)| *d);
                    let named = matches!(self.body.expr(o.expr)?, Expr::Path(_));
                    if !(named && matches!(def, Some(Def::Fn(_) | Def::Static(..)))) && !self.is_err(t) {
                        self.diag(DiagCode::AsmOperandType, at, About::found_named(t, Name::of("sym")));
                    }
                }
                // Coerced to `()` where it was pushed.
                AsmKind::Label => {}
            }
            f.a += 1;
            f.b = 0;
        }
        while let Some(&o) = ops.get(f.a as usize) {
            if o.kind.is_output() && !o.kind.is_input() && self.body.expr(o.expr)? == Expr::Underscore {
                // `out(reg) _`: discarded, nothing to check (its slot is never reached).
                f.a += 1;
                continue;
            }
            if matches!(o.kind, AsmKind::Out | AsmKind::LateOut) {
                self.check_place_code(o.expr, DiagCode::InvalidLhs)?;
            }
            self.resume(f, 1)?;
            if o.kind == AsmKind::Label {
                let to = self.common.unit;
                self.push_expr(o.expr, Expectation::HasType(to), Then::Coerce { to, two_phase: false })?;
            } else {
                self.push_expr(o.expr, Expectation::None, Then::Nothing)?;
            }
            return Ok(Flow::Wait);
        }
        Ok(Flow::Done(if options & ASM_NORETURN != 0 { self.common.never } else { self.common.unit }))
    }

    /// Each register named: a class or register of `arch`, not reserved, and no explicit
    /// register twice among the inputs or among the outputs.
    fn asm_registers(&mut self, ops: &[AsmOperand], arch: AsmArch) -> Result<(), Stop> {
        for (i, o) in ops.iter().enumerate() {
            let Some(reg) = o.reg else { continue };
            let at = At::Expr(o.expr);
            match lookup(&self.body, arch, reg)? {
                RegLookup::Class(_, None) => {}
                RegLookup::Class(_, Some(slot)) => {
                    for p in &ops[..i] {
                        let Some(prev) = p.reg else { continue };
                        let RegLookup::Class(_, Some(other)) = lookup(&self.body, arch, prev)? else { continue };
                        let both_in = p.kind.is_input() && o.kind.is_input();
                        let both_out = p.kind.is_output() && o.kind.is_output();
                        if other == slot && (both_in || both_out) {
                            let name = reg_name(&self.body, reg)?;
                            self.diag(DiagCode::AsmRegister, at, About::name(name));
                        }
                    }
                }
                RegLookup::Unknown | RegLookup::Reserved => {
                    let name = reg_name(&self.body, reg)?;
                    self.diag(DiagCode::AsmRegister, at, About::name(name));
                }
                RegLookup::Unported(what) => return Err(CheckError::Unported(what).into()),
            }
        }
        Ok(())
    }

    /// Every deferred `asm!` check, in the order the walk met them (after fallback, as casts).
    pub(crate) fn check_asms(&mut self) -> Result<(), Stop> {
        let mut i = 0;
        while i < self.asms.len() {
            let c = self.asms[i];
            i += 1;
            match c {
                AsmCheck::Const { expr, ty } => {
                    let t = self.ty.resolve(ty)?;
                    if self.is_err(t) {
                        continue;
                    }
                    if !matches!(self.view(t), TyView::Int(_) | TyView::Uint(_)) {
                        self.diag(DiagCode::AsmOperandType, At::Expr(expr), About::found_named(t, Name::of("const")));
                    }
                }
                AsmCheck::Reg { expr, ty, class, out } => {
                    let at = At::Expr(expr);
                    let t = self.structurally_resolve(ty, at)?;
                    if self.is_err(t) {
                        continue;
                    }
                    let a = self.asm_ty(t, expr)?;
                    let ok = a.is_some_and(|a| class.allows(a));
                    if !ok {
                        self.diag(DiagCode::AsmOperandType, at, About::found_named(t, class.name()));
                        continue;
                    }
                    let Some((x, ot)) = out else { continue };
                    let o = self.structurally_resolve(ot, At::Expr(x))?;
                    if self.is_err(o) {
                        continue;
                    }
                    if self.asm_ty(o, x)? != a {
                        self.diag(DiagCode::AsmOperandType, At::Expr(x), About::Types { expected: Some(t), found: o, name: Some(class.name()) });
                    }
                }
            }
        }
        Ok(())
    }

    /// `t` as inline assembly sees it; `None` for a type no class takes.
    fn asm_ty(&mut self, t: Ty, e: ExprId) -> Result<Option<AsmTy>, Stop> {
        if let Some(s) = self.asm_scalar(t)? {
            return Ok(Some(AsmTy::Scalar(s)));
        }
        let TyView::Adt(k, args) = self.view(t) else { return Ok(None) };
        let TypeAttrs(bits) = self.reads.attrs(k)?;
        if bits & TypeAttrs::SIMD == 0 {
            return Ok(None);
        }
        // `repr(simd)`: one array field (blaze-build layout.rs `ReprAttrs::simd`).
        let Some(adt) = self.reads.adt(k)? else { return Ok(None) };
        let Some(v) = adt.variants.first() else { return Ok(None) };
        let Some(&field) = adt.fields.get(v.fields.range()).and_then(|f| f.first()) else { return Ok(None) };
        let ft = self.lower(adt.types, field.ty, Lower::Args(args), Cause::Expr(e))?;
        let ft = self.ty.resolve(ft)?;
        let TyView::Array(elem, len) = self.view(ft) else { return Ok(None) };
        let TyView::ConstValue { bits: lanes, .. } = self.view(len) else { return Ok(None) };
        let Some(elem) = self.asm_scalar(elem)? else { return Ok(None) };
        let Ok(lanes) = u32::try_from(lanes) else { return Ok(None) };
        Ok(Some(AsmTy::Vector { elem, lanes }))
    }

    /// An integer, a float, a thin pointer or a fn pointer as its scalar (pointers and
    /// `usize`/`isize` are 64 bits on every tabled target).
    fn asm_scalar(&mut self, t: Ty) -> Result<Option<Scalar>, Stop> {
        Ok(Some(match self.view(t) {
            TyView::Int(i) => match i {
                IntTy::I8 => Scalar::I8,
                IntTy::I16 => Scalar::I16,
                IntTy::I32 => Scalar::I32,
                IntTy::I64 | IntTy::Isize => Scalar::I64,
                IntTy::I128 => Scalar::I128,
            },
            TyView::Uint(u) => match u {
                UintTy::U8 => Scalar::I8,
                UintTy::U16 => Scalar::I16,
                UintTy::U32 => Scalar::I32,
                UintTy::U64 | UintTy::Usize => Scalar::I64,
                UintTy::U128 => Scalar::I128,
            },
            TyView::Float(f) => match f {
                FloatTy::F16 => Scalar::F16,
                FloatTy::F32 => Scalar::F32,
                FloatTy::F64 => Scalar::F64,
                FloatTy::F128 => Scalar::F128,
            },
            TyView::FnPtr(..) => Scalar::I64,
            TyView::Ptr(_, pointee) => {
                let pointee = self.ty.resolve(pointee)?;
                match self.view(pointee) {
                    TyView::Slice(_) | TyView::Str | TyView::Dynamic(_) => return Ok(None),
                    // Thin only when sized; a parameter's sizedness is not asked here yet.
                    TyView::Param(_) | TyView::Alias { .. } | TyView::Opaque(..) => {
                        return Err(CheckError::Unported("a raw pointer to a generic type as an `asm!` operand (its sizedness is not asked yet)").into());
                    }
                    _ => Scalar::I64,
                }
            }
            _ => return Ok(None),
        }))
    }
}
