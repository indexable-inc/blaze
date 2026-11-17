//! The target-specific pieces of LIR to LLVM IR text (port27/tempo-run/INTRINSICS.md P11;
//! design 3.3, 3.4 and 3.5): a target intrinsic's declaration ([`intrinsic_decl`]), an `asm!`
//! block's inline-asm callee with its template and constraint string ([`asm_call`]) and its
//! call-site attributes ([`asm_attrs`]), the IR names of LIR's scalar and vector types
//! ([`scalar_ty`], [`vector_ty`]: `i32`, `half`, `fp128`, `<4 x i32>`), and a function's
//! `"target-features"` attribute ([`features_attr`]). Pure and sans-IO: LIR records in, text
//! out; nothing here allocates except [`features_attr`] (in the caller's allocator) and
//! [`asm_operand_tys`] (into the caller's list).
//!
//! Not yet called by [`crate::lower`]: its `Op::TargetIntrinsic`, `Op::Asm` and `Op::AsmBr`
//! arms still refuse by name. Its caller writes, for an `asm`:
//! `call <outputs> <asm_call text>(<arguments>)<asm_attrs text>`, where `<outputs>` is `void`,
//! the one output's type, or `{ .. }` of the outputs in operand order, and the arguments are,
//! in operand order, every `in`, `inout` and `inlateout` operand's value and every `sym`
//! operand's address (`ptr @name`). A `noreturn` block is followed by `unreachable`.
//!
//! The `asm!` forms are rustc's (ref-rust-1115 compiler/rustc_codegen_llvm/src/asm.rs:
//! `codegen_inline_asm`, `reg_to_llvm`, `modifier_to_llvm`), which the Reference
//! (inline-assembly.html) describes: the code tempo's dependencies were tested with. Two
//! choices differ in form, not in meaning, and are said where they are made: `clobber_abi`'s
//! registers are written as `~{..}` clobbers where rustc adds discarded late outputs, and an
//! explicit input register that `clobber_abi` also clobbers is refused (rustc's discarded output
//! would be needed there). AArch64 only: x86-64 `asm!` is refused by name.

use core::fmt::{self, Write};

use blaze_code::record::Symbol;
use blaze_lir::func::{AsmOperandKind, AsmOperandRec, AsmPieceRec, AsmRec, IntrinsicId, ParamKind, ParamRec, Sig, SigId, SigRec, Value};
use blaze_lir::ty::{Form, Scalar};
use blaze_lir::{Op, Ty, View};
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_tier::{Arch, Machine, Os};

use crate::lower::LowerError;

// ---- Types.

/// The IR name of a LIR scalar: `i1` .. `i128`, `half`, `float`, `double`, `fp128`, `ptr`
/// (design 3.5: `f16` is `half`, `f128` is `fp128`). `Scalar::None` has none: `void` is
/// written by the caller where no value is.
pub const fn scalar_ty(elem: Scalar) -> Option<&'static str> {
    match elem {
        Scalar::None => None,
        Scalar::I1 => Some("i1"),
        Scalar::I8 => Some("i8"),
        Scalar::I16 => Some("i16"),
        Scalar::I32 => Some("i32"),
        Scalar::I64 => Some("i64"),
        Scalar::I128 => Some("i128"),
        Scalar::F16 => Some("half"),
        Scalar::F32 => Some("float"),
        Scalar::F64 => Some("double"),
        Scalar::F128 => Some("fp128"),
        Scalar::Ptr => Some("ptr"),
    }
}

/// Writes the IR type of a scalar or fixed vector `ty` (`i32`, `<4 x i32>`, `<8 x half>`,
/// `fp128`). A scalable vector, a compound and no type are refused by name (`body` is the
/// function the type is of).
pub fn vector_ty(ty: Ty, body: Symbol, out: &mut impl Write) -> Result<(), LowerError> {
    let refuse = |what| LowerError::Refused { body, inst: None, what };
    let elem = scalar_ty(ty.elem()).ok_or(refuse("a value of no type"))?;
    match ty.form() {
        Form::Scalar => out.write_str(elem)?,
        Form::Vector => write!(out, "<{} x {elem}>", ty.lanes())?,
        Form::Scalable => return Err(refuse("a scalable vector")),
        Form::Compound => return Err(refuse("a compound where a scalar or vector is wanted")),
    }
    Ok(())
}

/// A scalar, a vector, or a compound of those (`{ <16 x i8>, <16 x i8> }`): an intrinsic's
/// parameter or return. A compound holding a compound is refused, as in [`crate::lower`].
fn value_ty(view: View<'_>, ty: Ty, body: Symbol, out: &mut impl Write) -> Result<(), LowerError> {
    if ty.form() != Form::Compound {
        return vector_ty(ty, body, out);
    }
    out.write_str("{ ")?;
    for (j, &member) in view.members(ty).iter().enumerate() {
        if j > 0 {
            out.write_str(", ")?;
        }
        vector_ty(Ty(member), body, out)?;
    }
    out.write_str(" }")?;
    Ok(())
}

// ---- Target intrinsics (design 3.3).

/// A target intrinsic's signature, read from the function that calls it.
#[derive(Clone, Copy, Debug)]
pub struct SigView<'v> {
    /// The calling function: what a refusal names.
    pub body: Symbol,
    /// Its columns (a compound's members are read from them).
    pub view: View<'v>,
    pub sig: Sig<'v>,
}

impl<'v> SigView<'v> {
    /// Intrinsic row `id` of `view`: its `llvm.*` name and its signature.
    pub fn of_intrinsic(view: View<'v>, id: IntrinsicId, body: Symbol) -> Result<(&'v str, SigView<'v>), LowerError> {
        let refuse = |what| LowerError::Refused { body, inst: None, what };
        let rec = *view.intrinsics.get(id.index()).ok_or(refuse("a target intrinsic past the `intrinsics` column"))?;
        let name = core::str::from_utf8(view.intrinsic_name(id)).map_err(|_| refuse("a target intrinsic whose name is not UTF-8"))?;
        if rec.sig as usize >= view.sigs.len() {
            return Err(refuse("a target intrinsic whose signature is past the `sigs` column"));
        }
        Ok((name, SigView { body, view, sig: view.sig(SigId(rec.sig)) }))
    }
}

/// Writes `declare <ret> @<name>(<params>)` and a newline: a target intrinsic, printed
/// verbatim (design 3.3: "every other `llvm.*` becomes `Op::TargetIntrinsic` with its
/// signature, printed verbatim to LLVM"). No parameter attribute is written: LLVM gives an
/// intrinsic's declaration its own attributes (`immarg` among them) from its table when the
/// name is read, and checks the types against it. A name that is not `llvm.` and an LLVM
/// identifier, a variadic signature and an `sret` or `byval` parameter are refused by name.
pub fn intrinsic_decl(name: &str, sig: &SigView<'_>, out: &mut impl Write) -> Result<(), LowerError> {
    let body = sig.body;
    let refuse = |what| LowerError::Refused { body, inst: None, what };
    let rest = name.strip_prefix("llvm.").ok_or(refuse("a target intrinsic whose name does not start with `llvm.`"))?;
    let identifier = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b'$');
    if rest.is_empty() || !rest.bytes().all(identifier) {
        return Err(refuse("a target intrinsic whose name is not an LLVM identifier"));
    }
    if sig.sig.bits & SigRec::VARIADIC != 0 {
        return Err(refuse("a variadic target intrinsic"));
    }
    let plain = |param: &ParamRec| match ParamKind::from_byte(param.kind) {
        Some(ParamKind::Value) => Ok(()),
        Some(ParamKind::Sret) => Err(refuse("a target intrinsic with an `sret` return")),
        Some(ParamKind::Byval) => Err(refuse("a target intrinsic with a `byval` parameter")),
        None => Err(refuse("a target intrinsic parameter of no known kind")),
    };
    out.write_str("declare ")?;
    match sig.sig.rets {
        [] => out.write_str("void")?,
        [one] => {
            plain(one)?;
            value_ty(sig.view, Ty(one.ty), body, out)?;
        }
        many => {
            out.write_str("{ ")?;
            for (j, ret) in many.iter().enumerate() {
                plain(ret)?;
                if j > 0 {
                    out.write_str(", ")?;
                }
                vector_ty(Ty(ret.ty), body, out)?;
            }
            out.write_str(" }")?;
        }
    }
    write!(out, " @{name}(")?;
    for (j, param) in sig.sig.params.iter().enumerate() {
        plain(param)?;
        if j > 0 {
            out.write_str(", ")?;
        }
        value_ty(sig.view, Ty(param.ty), body, out)?;
    }
    out.write_str(")\n")?;
    Ok(())
}

// ---- Inline assembly (design 3.4).

/// One `asm` instruction's block, read: what [`asm_call`] and [`asm_attrs`] print.
#[derive(Clone, Copy, Debug)]
pub struct AsmView<'v> {
    /// The function holding the block, and the instruction: what a refusal names.
    pub body: Symbol,
    pub inst: Option<u32>,
    /// The target: its architecture picks the constraint forms, its OS whether x18 is
    /// reserved.
    pub machine: Machine,
    /// [`AsmRec`]'s option bits.
    pub options: u32,
    pub pieces: &'v [AsmPieceRec],
    pub operands: &'v [AsmOperandRec],
    /// The function's `asm_text` column, which the pieces and operands index.
    pub text: &'v [u8],
    /// The block's `clobber_abi` names, comma-separated (`C`), empty for none.
    pub clobber_abi: &'v [u8],
    /// Per operand, its value's type (an `in`'s argument, an output's result member, an
    /// `inout`'s one type); [`Ty::NONE`] for `sym`.
    pub tys: &'v [Ty],
    /// The width of a pointer.
    pub ptr_bits: u32,
}

impl<'v> AsmView<'v> {
    /// Instruction `inst` of `view`, an `asm`, with `tys` filled by [`asm_operand_tys`].
    pub fn of(view: View<'v>, inst: usize, body: Symbol, machine: Machine, tys: &'v [Ty]) -> Result<AsmView<'v>, LowerError> {
        let at = Some(inst as u32);
        let refuse = |what| LowerError::Refused { body, inst: at, what };
        if view.op(inst) != Op::Asm {
            return Err(refuse("an instruction that is not `asm` read as an `asm!` block"));
        }
        let id = view.operands(inst).asm();
        let rec = *view.asms.get(id.index()).ok_or(refuse("an `asm!` block past the `asms` column"))?;
        Ok(AsmView {
            body,
            inst: at,
            machine,
            options: rec.options,
            pieces: view.asm_pieces_of(id),
            operands: view.asm_operands_of(id),
            text: view.asm_text,
            clobber_abi: view.asm_str(rec.clobbers_first, rec.clobbers_len),
            tys,
            ptr_bits: view.head.ptr_bits,
        })
    }
}

/// Pushes onto `out` the type of each operand of `asm` instruction `inst` ([`AsmView::tys`]):
/// inputs are the instruction's values in operand order, outputs the members of its result
/// (none, the one output's type, or a compound in operand order). An `asm_br` (a `label`
/// operand) is refused by name; so is a block whose values or result do not match its operands,
/// and an `inout` whose input and output types differ (LLVM ties operands of one type).
pub fn asm_operand_tys(view: View<'_>, inst: usize, body: Symbol, out: &mut AVec<'_, Ty>) -> Result<(), LowerError> {
    let refuse = |what| LowerError::Refused { body, inst: Some(inst as u32), what };
    match view.op(inst) {
        Op::Asm => {}
        Op::AsmBr => return Err(refuse("`asm!` with a `label` operand (`asm_br`, LLVM's `callbr`)")),
        _ => return Err(refuse("an instruction that is not `asm` read as an `asm!` block")),
    }
    let mut words = view.operands(inst);
    let id = words.asm();
    let values = words.values();
    let operands = view.asm_operands_of(id);
    let result = view.ty(Value(inst as u32));
    let mut gives = 0usize;
    let mut takes = 0usize;
    for operand in operands {
        let kind = AsmOperandKind::from_byte(operand.kind).ok_or(refuse("an `asm!` operand of no known kind"))?;
        gives += usize::from(kind.gives());
        takes += usize::from(kind.takes());
    }
    if takes != values.len() {
        return Err(refuse("an `asm` whose values are not one per input operand"));
    }
    let one = [result.0];
    let outputs: &[u16] = match gives {
        0 if result.is_none() => &[],
        1 if result.form() != Form::Compound && !result.is_none() => &one,
        _ if result.form() == Form::Compound && view.members(result).len() == gives => view.members(result),
        _ => return Err(refuse("an `asm` whose result is not one member per output operand")),
    };
    let (mut next_in, mut next_out) = (0usize, 0usize);
    for operand in operands {
        let kind = AsmOperandKind::from_byte(operand.kind).ok_or(refuse("an `asm!` operand of no known kind"))?;
        let ty = match kind {
            AsmOperandKind::In => {
                next_in += 1;
                view.ty(values.get(next_in - 1))
            }
            AsmOperandKind::Out | AsmOperandKind::LateOut => {
                next_out += 1;
                Ty(outputs[next_out - 1])
            }
            AsmOperandKind::InOut | AsmOperandKind::InLateOut => {
                next_in += 1;
                next_out += 1;
                let (input, output) = (view.ty(values.get(next_in - 1)), Ty(outputs[next_out - 1]));
                if input != output {
                    return Err(refuse("an `inout` operand whose input and output types differ"));
                }
                output
            }
            AsmOperandKind::Sym => Ty::NONE,
            AsmOperandKind::Label => return Err(refuse("a `label` operand in an `asm` (only `asm_br` has labels)")),
        };
        out.try_push(ty).map_err(|_| LowerError::Text)?;
    }
    Ok(())
}

/// An AArch64 register class (the Reference's "Register operands" table: `reg` is `x[0-30]`,
/// `vreg` `v[0-31]`, `vreg_low16` `v[0-15]`; `preg` "Only clobbers").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Reg,
    Vreg,
    VregLow16,
}

/// Where an operand goes: a class LLVM allocates in, or one explicit register by number.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    Class(Class),
    Gpr(u8),
    Vreg(u8),
}

impl Place {
    /// Whether its values are in general registers (`reg`, `xN`) rather than vector ones.
    const fn general(self) -> bool {
        matches!(self, Place::Class(Class::Reg) | Place::Gpr(_))
    }
}

/// One operand, resolved and checked.
#[derive(Clone, Copy, Debug)]
struct Operand {
    kind: AsmOperandKind,
    /// `None` for `sym`.
    place: Option<Place>,
    /// Its value's size in bytes (0 for `sym`).
    bytes: u32,
}

/// x18 is reserved where rustc's `target_reserves_x18` says so (ref-rust-1115
/// compiler/rustc_target/src/asm/aarch64.rs:100: Android, Fuchsia, OHOS, `is_like_darwin`,
/// `is_like_windows`): of blaze's targets, macOS. The Reference: "x18 ... is an OS-reserved
/// register on some AArch64 targets"; "On AArch64 x18 only included in the clobber list if it
/// is not considered as a reserved register on the target".
const fn x18_reserved(machine: Machine) -> bool {
    matches!(machine.os, Os::MacOs)
}

/// `N` of a register name's number part: `0`, or no leading zero, at most `max`.
fn number(digits: &str, max: u8) -> Option<u8> {
    let bytes = digits.as_bytes();
    let ok = matches!(bytes.len(), 1 | 2) && bytes.iter().all(u8::is_ascii_digit) && (bytes.len() == 1 || bytes[0] != b'0');
    if !ok {
        return None;
    }
    let n = bytes.iter().fold(0u8, |acc, &d| acc * 10 + (d - b'0'));
    (n <= max).then_some(n)
}

/// An explicit register by any of its names (the Reference's "Register names" table:
/// `x[0-30]`/`w[0-30]`, `x30`/`lr`, `v[0-31]` with `b h s d q` aliases), with the
/// Reference's refusals ("Register operands"): the frame pointer x29, the base pointer x19,
/// `sp`, the zero register, x18 where reserved, and the clobber-only `p0`-`p15` and `ffr`.
fn explicit(name: &str, machine: Machine) -> Result<Place, &'static str> {
    match name {
        "lr" | "wlr" => return Ok(Place::Gpr(30)),
        "fp" | "wfp" | "x29" | "w29" => return Err("the frame pointer x29 as an `asm!` operand"),
        "x19" | "w19" => return Err("x19, LLVM's base pointer, as an `asm!` operand"),
        "sp" | "wsp" => return Err("the stack pointer as an `asm!` operand"),
        "xzr" | "wzr" => return Err("the zero register as an `asm!` operand"),
        "ffr" => return Err("`ffr` as an `asm!` operand: the Reference allows it only as a clobber"),
        _ => {}
    }
    let head = *name.as_bytes().first().ok_or("an `asm!` operand with an empty register name")?;
    let digits = name.get(1..).ok_or("an unknown AArch64 register")?;
    match head {
        b'x' | b'w' => {
            let n = number(digits, 30).ok_or("an unknown AArch64 register")?;
            if n == 18 && x18_reserved(machine) {
                return Err("x18, reserved by the OS on this target, as an `asm!` operand");
            }
            Ok(Place::Gpr(n))
        }
        b'v' | b'b' | b'h' | b's' | b'd' | b'q' => {
            Ok(Place::Vreg(number(digits, 31).ok_or("an unknown AArch64 register")?))
        }
        b'z' => Err("an SVE `z` register as an `asm!` operand (a scalable vector)"),
        b'p' => Err("an SVE `p` register as an `asm!` operand: the Reference allows it only as a clobber"),
        _ => Err("an unknown AArch64 register"),
    }
}

/// The text `asm_text[first..][..len]`, as UTF-8.
fn asm_str<'v>(rec: &AsmView<'v>, first: u32, len: u32) -> Result<&'v str, &'static str> {
    let end = first.checked_add(len).ok_or("`asm!` text past the `asm_text` column")?;
    let bytes = rec.text.get(first as usize..end as usize).ok_or("`asm!` text past the `asm_text` column")?;
    core::str::from_utf8(bytes).map_err(|_| "`asm!` text that is not UTF-8")
}

/// Operand `i`, resolved: its kind, where it goes and its size, with every form this lowering
/// does not take refused by name.
fn operand(rec: &AsmView<'_>, i: usize) -> Result<Operand, &'static str> {
    let op = rec.operands[i];
    let kind = AsmOperandKind::from_byte(op.kind).ok_or("an `asm!` operand of no known kind")?;
    match kind {
        AsmOperandKind::Sym => return Ok(Operand { kind, place: None, bytes: 0 }),
        AsmOperandKind::Label => return Err("a `label` operand (`asm_br`, LLVM's `callbr`)"),
        _ => {}
    }
    let ty = *rec.tys.get(i).ok_or("an `asm!` operand with no type")?;
    let bits = match ty.form() {
        Form::Scalar | Form::Vector => ty.bits(rec.ptr_bits),
        Form::Scalable => return Err("a scalable vector as an `asm!` operand"),
        Form::Compound => return Err("a compound as an `asm!` operand"),
    };
    if bits == 0 || bits % 8 != 0 {
        return Err("an `asm!` operand of no whole number of bytes");
    }
    let bytes = bits / 8;
    let reg = asm_str(rec, op.reg_first, op.reg_len)?;
    let place = match op.class {
        1 => match reg {
            "reg" => Place::Class(Class::Reg),
            "vreg" => Place::Class(Class::Vreg),
            "vreg_low16" => Place::Class(Class::VregLow16),
            "preg" => return Err("a `preg` operand: the Reference allows that class only as a clobber"),
            _ => return Err("an unknown AArch64 register class"),
        },
        0 => explicit(reg, rec.machine)?,
        _ => return Err("an `asm!` operand neither a register nor a class"),
    };
    if place.general() {
        if !matches!(bytes, 1 | 2 | 4 | 8) {
            return Err("an `asm!` operand of more than 8 bytes in a general register");
        }
    } else {
        if !matches!(bytes, 1 | 2 | 4 | 8 | 16) {
            return Err("an `asm!` operand of more than 16 bytes in a vector register");
        }
        // rustc_codegen_llvm asm.rs `llvm_fixup_input`/`llvm_fixup_output`: an `i8` in `vreg`
        // and any scalar in `vreg_low16` are widened to a vector around the call; that is not
        // done here, so those forms are refused rather than handed to LLVM unwidened.
        if bytes == 1 && ty.form() == Form::Scalar {
            return Err("an 8-bit scalar in a vector register (rustc widens it to `i8x8`)");
        }
        if place == Place::Class(Class::VregLow16) && ty.form() == Form::Scalar {
            return Err("a scalar in `vreg_low16` (rustc widens it to a vector)");
        }
    }
    Ok(Operand { kind, place: Some(place), bytes })
}

/// Writes `place`'s constraint for a value of `bytes` bytes (rustc's `reg_to_llvm`): a class
/// is `r`, `w` or `x`; an explicit general register `{xN}` for 8 bytes else `{wN}`; an
/// explicit vector register `{qN}`, `{dN}`, `{sN}` or `{hN}` by size.
fn place_text(out: &mut impl Write, place: Place, bytes: u32) -> fmt::Result {
    match place {
        Place::Class(Class::Reg) => out.write_char('r'),
        Place::Class(Class::Vreg) => out.write_char('w'),
        Place::Class(Class::VregLow16) => out.write_char('x'),
        Place::Gpr(n) => write!(out, "{{{}{n}}}", if bytes == 8 { 'x' } else { 'w' }),
        Place::Vreg(n) => {
            let class = match bytes {
                16 => 'q',
                8 => 'd',
                4 => 's',
                _ => 'h',
            };
            write!(out, "{{{class}{n}}}")
        }
    }
}

/// Whether operand `i` is late (`lateout`, `inlateout`): LLVM's `=` rather than `=&`.
const fn late(kind: AsmOperandKind) -> bool {
    matches!(kind, AsmOperandKind::LateOut | AsmOperandKind::InLateOut)
}

/// Whether operand `kind` is an argument of the call: an input value or a `sym` address.
const fn argument(kind: AsmOperandKind) -> bool {
    kind.takes() || matches!(kind, AsmOperandKind::Sym)
}

/// The counts of output operands before `i` and of argument operands before `i`.
fn before(rec: &AsmView<'_>, i: usize) -> (u32, u32) {
    let mut counts = (0u32, 0u32);
    for op in &rec.operands[..i] {
        if let Some(kind) = AsmOperandKind::from_byte(op.kind) {
            counts.0 += u32::from(kind.gives());
            counts.1 += u32::from(argument(kind));
        }
    }
    counts
}

/// Option bits [`AsmRec::NAMED`] knows.
fn known_options() -> u32 {
    AsmRec::NAMED.iter().fold(0, |all, it| all | it.0)
}

/// The block's `clobber_abi`: whether it names AArch64's C clobber set (the Reference's
/// table: `"C"`, `"system"`, `"efiapi"` clobber `x[0-17], x18*, x30, v[0-31], p[0-15], ffr`).
fn clobbers_c(rec: &AsmView<'_>) -> Result<bool, &'static str> {
    let text = core::str::from_utf8(rec.clobber_abi).map_err(|_| "a `clobber_abi` that is not UTF-8")?;
    if text.is_empty() {
        return Ok(false);
    }
    for abi in text.split(',') {
        match abi.trim() {
            "C" | "system" | "efiapi" => {}
            _ => return Err("a `clobber_abi` the Reference does not list for AArch64"),
        }
    }
    Ok(true)
}

/// The registers of the C clobber set in order: `x0`-`x17`, `x18` unless reserved, `x30`,
/// `v0`-`v31`, `p0`-`p15`, `ffr`; each as its clobber text's class letter and number
/// (`ffr` is `('f', 0)`).
fn c_clobber(k: u32, machine: Machine) -> Option<(char, u8)> {
    let x18 = u32::from(!x18_reserved(machine));
    let gprs = 18 + x18 + 1;
    match k {
        0..=17 => Some(('x', k as u8)),
        _ if k < gprs => Some(('x', if k == 18 && x18 == 1 { 18 } else { 30 })),
        _ if k < gprs + 32 => Some(('q', (k - gprs) as u8)),
        _ if k < gprs + 48 => Some(('p', (k - gprs - 32) as u8)),
        _ if k == gprs + 48 => Some(('f', 0)),
        _ => None,
    }
}

/// Checks the whole block before anything is written: the target, the options, every operand
/// and the clobbers.
fn check_asm(rec: &AsmView<'_>) -> Result<(), &'static str> {
    if rec.machine.arch != Arch::Arm64 {
        return Err("`asm!` for x86-64 (only AArch64 constraints are lowered)");
    }
    let options = rec.options;
    if options & !known_options() != 0 {
        return Err("an `asm!` option of no known meaning");
    }
    // The Reference ("Options"): "att_syntax: This option is only valid on x86".
    if options & AsmRec::ATT_SYNTAX != 0 {
        return Err("`att_syntax` on AArch64");
    }
    if options & AsmRec::MAY_UNWIND != 0 {
        return Err("`asm!` with `may_unwind` (unwinding is not lowered)");
    }
    // The Reference: "The pure option must be combined with either the nomem or readonly
    // options"; and `nomem` and `readonly` are two answers to one question.
    if options & AsmRec::PURE != 0 && options & (AsmRec::NOMEM | AsmRec::READONLY) == 0 {
        return Err("`pure` without `nomem` or `readonly`");
    }
    if options & AsmRec::NOMEM != 0 && options & AsmRec::READONLY != 0 {
        return Err("`nomem` together with `readonly`");
    }
    if rec.tys.len() != rec.operands.len() {
        return Err("an `asm!` block whose operand types are not one per operand");
    }
    for i in 0..rec.operands.len() {
        operand(rec, i)?;
    }
    for piece in rec.pieces {
        match piece.kind {
            AsmPieceRec::TEXT => {
                asm_str(rec, piece.a, piece.b)?;
            }
            AsmPieceRec::OPERAND => {
                let at = piece.a as usize;
                if at >= rec.operands.len() {
                    return Err("an `asm!` template naming an operand past its operands");
                }
                modifier(operand(rec, at)?, piece.b)?;
            }
            _ => return Err("an `asm!` template piece of no known kind"),
        }
    }
    if clobbers_c(rec)? {
        // An explicit `in` register the set also clobbers: rustc makes the clobber a
        // discarded late output, which may share the input's register; a `~{..}` clobber
        // may not, and leaving it out would let LLVM think the input survives.
        for i in 0..rec.operands.len() {
            let op = operand(rec, i)?;
            if op.kind == AsmOperandKind::In && op.place.is_some_and(|place| clobbered(rec, place)) {
                return Err("an explicit input register that `clobber_abi` also clobbers");
            }
        }
    }
    Ok(())
}

/// Whether the C clobber set holds explicit register `place`.
fn clobbered(rec: &AsmView<'_>, place: Place) -> bool {
    match place {
        Place::Gpr(n) => n <= 17 || n == 30 || (n == 18 && !x18_reserved(rec.machine)),
        Place::Vreg(_) => true,
        Place::Class(_) => false,
    }
}

/// The LLVM modifier for a template reference with modifier byte `b` (0: none) to `op`
/// (rustc's `modifier_to_llvm`; the Reference's "Template modifiers" table: `reg` takes `w`
/// and `x`, `vreg` takes `v`, `b`, `h`, `s`, `d`, `q`, and `v` is LLVM's default so none is
/// written). `Ok(None)` writes no modifier.
fn modifier(op: Operand, b: u32) -> Result<Option<char>, &'static str> {
    let Some(place) = op.place else {
        return match b {
            0 => Ok(None),
            _ => Err("a template modifier on a `sym` operand"),
        };
    };
    let m = match b {
        0 => return Ok(None),
        _ => char::from_u32(b).ok_or("a template modifier that is not a character")?,
    };
    match (place.general(), m) {
        (true, 'w' | 'x') => Ok(Some(m)),
        (false, 'v') => Ok(None),
        (false, 'b' | 'h' | 's' | 'd' | 'q') => Ok(Some(m)),
        _ => Err("an AArch64 template modifier the Reference does not list for its class"),
    }
}

/// Writes the comma before every constraint but the first.
fn separate(out: &mut impl Write, first: &mut bool) -> fmt::Result {
    if core::mem::replace(first, false) { Ok(()) } else { out.write_char(',') }
}

/// Writes `bytes` inside an LLVM string literal: printable ASCII but `"` and `\` as itself,
/// every other byte as `\XX`; with `dollars`, each `$` doubled (LLVM's template escape, as
/// rustc writes a template's text pieces).
fn string_bytes(out: &mut impl Write, bytes: &[u8], dollars: bool) -> fmt::Result {
    for &byte in bytes {
        match byte {
            b'$' if dollars => out.write_str("$$")?,
            b'"' | b'\\' => write!(out, "\\{byte:02X}")?,
            0x20..=0x7e => out.write_char(char::from(byte))?,
            _ => write!(out, "\\{byte:02X}")?,
        }
    }
    Ok(())
}

/// Writes the inline-asm callee of one `asm!` block: `asm [sideeffect] [alignstack]
/// "<template>", "<constraints>"`, the way rustc's `codegen_inline_asm` builds it (see the
/// module's documentation for what the caller writes around it).
///
/// - `sideeffect` unless `pure`, `alignstack` unless `nostack`.
/// - The template's text with `$` doubled; an operand as `${N}` or `${N:m}` (N its place in
///   the constraint list: outputs first, then arguments), a `sym` as `${N:c}`.
/// - The constraints: each output in operand order (`=&` + its place, `=` when late), then
///   each argument in operand order (an `in` its place; an `inout` the number of its output,
///   tied, except a late one in an explicit register, which repeats the register as Clang
///   does; a `sym` `s`), then `clobber_abi`'s registers not already an explicit output
///   (`~{xN}`, `~{qN}`, `~{pN}`, `~{ffr}`), `~{cc}` unless `preserves_flags`, `~{memory}`
///   unless `nomem`.
///
/// Every form not taken is refused by name before anything is written.
pub fn asm_call(rec: &AsmView<'_>, out: &mut impl Write) -> Result<(), LowerError> {
    let refuse = |what| LowerError::Refused { body: rec.body, inst: rec.inst, what };
    check_asm(rec).map_err(refuse)?;
    let resolved = |i: usize| operand(rec, i).map_err(refuse);
    let n = rec.operands.len();
    let outputs = before(rec, n).0;

    out.write_str("asm ")?;
    if rec.options & AsmRec::PURE == 0 {
        out.write_str("sideeffect ")?;
    }
    if rec.options & AsmRec::NOSTACK == 0 {
        out.write_str("alignstack ")?;
    }

    // The template.
    out.write_char('"')?;
    for piece in rec.pieces {
        if piece.kind == AsmPieceRec::TEXT {
            let text = asm_str(rec, piece.a, piece.b).map_err(refuse)?;
            string_bytes(out, text.as_bytes(), true)?;
            continue;
        }
        let at = piece.a as usize;
        let op = resolved(at)?;
        let (outs_before, args_before) = before(rec, at);
        let index = if op.kind.gives() { outs_before } else { outputs + args_before };
        match (op.place, modifier(op, piece.b).map_err(refuse)?) {
            (None, _) => write!(out, "${{{index}:c}}")?,
            (Some(_), Some(m)) => write!(out, "${{{index}:{m}}}")?,
            (Some(_), None) => write!(out, "${{{index}}}")?,
        }
    }
    out.write_str("\", \"")?;

    // The constraints.
    let mut first = true;
    for i in 0..n {
        let op = resolved(i)?;
        let Some(place) = op.place else { continue };
        if op.kind.gives() {
            separate(out, &mut first)?;
            out.write_str(if late(op.kind) { "=" } else { "=&" })?;
            place_text(out, place, op.bytes)?;
        }
    }
    for i in 0..n {
        let op = resolved(i)?;
        match (op.kind, op.place) {
            (AsmOperandKind::Sym, _) => {
                separate(out, &mut first)?;
                out.write_char('s')?;
            }
            (AsmOperandKind::In, Some(place)) => {
                separate(out, &mut first)?;
                place_text(out, place, op.bytes)?;
            }
            (AsmOperandKind::InLateOut, Some(place @ (Place::Gpr(_) | Place::Vreg(_)))) => {
                separate(out, &mut first)?;
                place_text(out, place, op.bytes)?;
            }
            (AsmOperandKind::InOut | AsmOperandKind::InLateOut, Some(_)) => {
                separate(out, &mut first)?;
                write!(out, "{}", before(rec, i).0)?;
            }
            _ => {}
        }
    }
    if clobbers_c(rec).map_err(refuse)? {
        let mut k = 0u32;
        while let Some((class, number)) = c_clobber(k, rec.machine) {
            k += 1;
            let explicit_output = (0..n).any(|i| {
                let Ok(op) = operand(rec, i) else { return false };
                op.kind.gives()
                    && match (class, op.place) {
                        ('x', Some(Place::Gpr(m))) | ('q', Some(Place::Vreg(m))) => m == number,
                        _ => false,
                    }
            });
            if explicit_output {
                continue;
            }
            separate(out, &mut first)?;
            match class {
                'f' => out.write_str("~{ffr}")?,
                _ => write!(out, "~{{{class}{number}}}")?,
            }
        }
    }
    if rec.options & AsmRec::PRESERVES_FLAGS == 0 {
        separate(out, &mut first)?;
        out.write_str("~{cc}")?;
    }
    if rec.options & AsmRec::NOMEM == 0 {
        separate(out, &mut first)?;
        out.write_str("~{memory}")?;
    }
    out.write_char('"')?;
    Ok(())
}

/// Writes the call site's memory and return attributes, each after a space, as rustc's
/// `codegen_inline_asm` gives them: `pure` + `nomem`: `memory(none) willreturn`; `pure` +
/// `readonly`: `memory(read) willreturn`; `nomem` alone: `memory(inaccessiblemem:
/// readwrite)`; `readonly` alone: `memory(read, inaccessiblemem: readwrite)` (rustc's
/// `ReadOnlyNotPure`, `MemoryEffects::readOnly() | MemoryEffects::inaccessibleMemOnly()` in
/// rustc_llvm's RustWrapper.cpp); otherwise nothing.
pub fn asm_attrs(rec: &AsmView<'_>, out: &mut impl Write) -> Result<(), LowerError> {
    let refuse = |what| LowerError::Refused { body: rec.body, inst: rec.inst, what };
    check_asm(rec).map_err(refuse)?;
    let (pure, nomem, readonly) =
        (rec.options & AsmRec::PURE != 0, rec.options & AsmRec::NOMEM != 0, rec.options & AsmRec::READONLY != 0);
    match (pure, nomem, readonly) {
        (true, true, _) => out.write_str(" memory(none) willreturn")?,
        (true, false, _) => out.write_str(" memory(read) willreturn")?,
        (false, true, _) => out.write_str(" memory(inaccessiblemem: readwrite)")?,
        (false, false, true) => out.write_str(" memory(read, inaccessiblemem: readwrite)")?,
        (false, false, false) => {}
    }
    Ok(())
}

// ---- Target features (design 3.1, 3.3).

/// A function's target features, as LIR's `features` column holds them: LLVM's names,
/// comma-separated, each enabled (`+aes,+sha3`). Stands in for blaze-tier's feature bit set
/// (P8, not yet in the tree); the attribute written is the same.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Features<'v> {
    pub list: &'v [u8],
}

/// The function attribute `"target-features"="+a,+b"` for `f`: its names sorted and each
/// once, so equal sets give equal bytes in any order; empty for no features (the module's
/// target CPU gives the base set). A name not `+` and an LLVM feature name (`-name`, which
/// `#[target_feature]` never asks for, among them) is refused by name; `body` is the function.
pub fn features_attr<'a>(f: Features<'_>, body: Symbol, alloc: Alloc<'a>) -> Result<AString<'a>, LowerError> {
    let refuse = |what| LowerError::Refused { body, inst: None, what };
    let list = core::str::from_utf8(f.list).map_err(|_| refuse("target features that are not UTF-8"))?;
    let mut names: AVec<'_, &str> = Vec::new_in(alloc);
    if !list.is_empty() {
        for item in list.split(',') {
            let name = match item.strip_prefix('+') {
                Some(name) => name,
                None if item.starts_with('-') => return Err(refuse("a disabled target feature (`-name`)")),
                None => return Err(refuse("a target feature not written `+name`")),
            };
            let feature = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-');
            if name.is_empty() || !name.bytes().all(feature) {
                return Err(refuse("a target feature whose name is not an LLVM feature name"));
            }
            names.try_push(name).map_err(|_| LowerError::Text)?;
        }
    }
    names.sort_unstable();
    let mut text = String::new_in(alloc);
    if names.is_empty() {
        return Ok(text);
    }
    text.try_push_str("\"target-features\"=\"").map_err(|_| LowerError::Text)?;
    let mut last: Option<&str> = None;
    for &name in names.iter() {
        if last == Some(name) {
            continue;
        }
        if last.is_some() {
            text.try_push(',').map_err(|_| LowerError::Text)?;
        }
        text.try_push('+').map_err(|_| LowerError::Text)?;
        text.try_push_str(name).map_err(|_| LowerError::Text)?;
        last = Some(name);
    }
    text.try_push('"').map_err(|_| LowerError::Text)?;
    Ok(text)
}
