//! The template table through LLVM, with per-template validation (backend-next.md D99; the
//! backend wave's piece T2). The second generator of piece 0's table (crates/blaze-tier
//! table.rs), beside the encoders' (blaze-emit table_gen, T1): for each pattern T1 makes, a
//! one-instruction LIR function is lowered by D2 ([`crate::lower`]), called from a wrapper whose
//! inline assembly pins its arguments and its result to placeholder registers, compiled by D1
//! ([`crate::compile`], the named pipeline), read back by D3 ([`crate::read`]); the bytes between
//! the wrapper's two markers are the template, and its fields are found by T1's own bit-flip
//! discovery ([`blaze_emit::table_gen::discover`], the one shared function). The table is written
//! by T1's one writer ([`blaze_emit::table_gen::write_table`]) with [`Source::Llvm`].
//!
//! # The wrapper
//!
//! ```text
//! define void @blaze_t2_template() "frame-pointer"="all" {
//!   %in = call {i64, ...} asm sideeffect "hint #112", "={x0},={x1},..."()   ; every kept register
//!   %p0 = extractvalue {i64, ...} %in, <the index of operand 0's register>
//!   %a0 = trunc i64 %p0 to i8                                              ; below 64 bits
//!   %r = call i8 @bz..(i8 %a0, i8 %a1)                                       ; D2's function, inlined
//!   %z = zext i8 %r to i64
//!   call void asm sideeffect "hint #113", "{x10},{x0},..."(i64 %z, ...)     ; the result, and the rest
//!   ret void
//! }
//! ```
//!
//! Every allocatable register but the scratch pair x16/x17 comes out of the first marker and goes
//! into the second, so between the markers LLVM has the pinned operands and x16/x17 only: the
//! template's clobbers are exactly x16 and x17 (declared), and a template that needs more spills,
//! which the word check refuses by name. Values are held as LIR's baseline holds them (blaze-emit
//! table_gen's conventions): zero-extended to 64 bits; the `trunc` and `zext` say so to LLVM. The
//! markers are `hint #112` and `hint #113`, unallocated hints (no-ops), whose words appear once.
//!
//! # What is generated, and what is refused by name
//!
//! Integer `add`, `sub`, `mul`, `and`, `or`, `xor`, `shl`, `lshr`, `ashr`, `icmp` and the
//! conversions `zext`, `sext`, `trunc`, with register operands and at most one immediate of the
//! classes `U12`, `U12Lsl12`, `Logical32`, `Logical64`, `Shift5`, `Shift6`, `Mov16`, `I64`. Every
//! other pattern is a [`Refusal`] naming it ([`Why::Shape`]): memory, branches, sites, frames,
//! floats, fused forms, divisions (LIR's division traps; a trap is a branch the template cannot
//! hold). A pattern LLVM does not compile to code linear in its operands (`discover` refuses it),
//! whose code holds a branch or a memory access, or that fails validation is not admitted, by
//! name: [`Generated::refused`] lists every one with its reason.
//!
//! # Validation (D99's differential test)
//!
//! Each template is placed between the AAPCS argument registers and a `ret`, and run in this
//! process on the edge values of its width (0, 1, -1, the minimum, the maximum, a power of two;
//! every combination over its inputs) for three values of its immediate, and compared with LIR's
//! evaluator ([`blaze_lir::eval::run_in`]) over the same one-instruction function: with the result
//! in its own register, and once with the result in each input's register (the composer may give
//! the result an input's register: a template writes its result only after it has read every
//! input). Where T1 has a template for the pattern, T1's is run on the same inputs and must give
//! the same value. Running needs a host of the table's architecture: on another host generation
//! is refused ([`T2Error::Host`]).

use core::cell::RefCell;
use core::fmt::{self, Write as _};

use blaze_code::record::{Symbol, Target};
use blaze_emit::table_gen::{Code, GenError, Recipe, Row, Slot, SlotKind, Vals, discover, fill, recipes, write_table};
use blaze_inproc::exec::Executable;
use blaze_lir::builder::{Param, Variadic};
use blaze_lir::eval::{EvalError, Outcome, Val, run_in};
use blaze_lir::func::Conv;
use blaze_lir::ty::Ty;
use blaze_lir::{BlockKind, BuildError, Builder, Flags, Full, Func, IntCond, Op};
use blaze_own::{AString, AVec, Alloc, Vec};
use blaze_tier::table::{RESULT, TEMP0, TEMP1};
use blaze_tier::{Arch, Field, Form, ImmClass, Machine, Named, Names, Os, Outside, Pattern, Reg, RegSet, Source, TableHead};

use crate::compile::{LlvmError, compile, init};
use crate::lower::{LowerError, Root, lower_as};
use crate::names::{ir_name, ir_name_str};
use crate::pipeline::Pipeline;
use crate::read::{ReadError, read_object};
use crate::remarks::Remark;

/// The wrapper's name: the symbol read back from the object.
const WRAPPER: &str = "blaze_t2_template";
/// The one-instruction function's symbol (any fixed value: it is inlined into the wrapper).
const FUNC: Symbol = Symbol([0x72; 32]);
/// `hint #112` and `hint #113`: the markers around the template.
const START: u32 = 0xd503_2e1f;
const END: u32 = 0xd503_2e3f;
/// `ret`, and `orr x0, xzr, x<k>` (`mov x0, x<k>`) without `k`.
const RET: u32 = 0xd65f_03c0;
const MOV_X0: u32 = 0xaa00_03e0;
/// The registers LLVM is left between the markers, and so the clobbers of every template here.
/// x30 is not pinned: an asm output in the link register is where LLVM's allocator is least
/// sure to cope; a template may use it as a temporary, so it is declared clobbered.
const SCRATCH: RegSet = RegSet::EMPTY.with(Reg(16)).with(Reg(17)).with(Reg(30));
/// Where a validation run puts each input operand (0..4: the AAPCS argument registers x0..x3),
/// the result, and T1's temporaries.
const RUN_RESULT: u64 = 9;
const RUN_TEMP0: u64 = 10;
const RUN_TEMP1: u64 = 11;
/// The evaluator's fuel: the function is one instruction and a return.
const FUEL: u64 = 16;
/// The most bytes of a template and its run's two words.
const MOST_RUN: usize = blaze_emit::table_gen::MOST_BYTES + 8;

/// A planted bug the tests catch (rule 8): the one-instruction function built with its first two
/// operands swapped, so `sub` computes `b - a`; discovery passes, validation must refuse it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    SwapOperands,
}

/// Why one pattern's template was not admitted.
#[derive(Debug)]
pub enum Why<'a> {
    /// A form of pattern this generator does not make (memory, branches, sites, floats, fused
    /// forms, divisions, two immediates, a width LIR has no integer for).
    Shape { what: &'static str },
    /// The immediate's encoding names no constant (an invalid logical immediate).
    Constant { class: ImmClass, encoded: u64 },
    /// A placeholder register LLVM cannot be pinned to (x18 on macOS, x29, x30, x31).
    Register { reg: u64 },
    Build(BuildError),
    Lower(LowerError),
    Llvm(LlvmError<'a>),
    Read(ReadError),
    /// The markers are not each there once, start before end.
    Markers { starts: u32, ends: u32 },
    /// Word `at` of the template is a branch, trap or system instruction, or a memory access.
    Instruction { at: u32, word: u32, what: &'static str },
    /// `discover` refused LLVM's code (not linear in its operands, a bit two operands reach, ...).
    Discover(GenError),
    Eval(EvalError),
    /// The template disagrees with the evaluator: the inputs (by operand), the immediate's
    /// encoding, the register the result was in, the evaluator's value and the template's.
    Evaluator { inputs: [u64; 4], imm: u64, result: u64, want: u128, got: u64 },
    /// LLVM's template and T1's disagree on these inputs.
    Differs { inputs: [u64; 4], imm: u64, llvm: u64, encoders: u64 },
}

/// One pattern not admitted, and why.
#[derive(Debug)]
pub struct Refusal<'a> {
    pub pattern: Pattern,
    pub why: Why<'a>,
}

/// What generation stopped on: nothing about one pattern (that is a [`Refusal`]).
#[derive(Debug)]
pub enum T2Error {
    /// No LLVM generator for this machine here (x86-64 is piece X's).
    Machine { machine: Machine },
    /// Validation runs the templates in this process: the host is not of `machine`'s
    /// architecture.
    Host { machine: Machine },
    /// LLVM's targets did not register.
    Init(LlvmError<'static>),
    /// The recipe list or the table's writer refused.
    Gen(GenError),
    /// A validation run's code could not be mapped executable.
    Map { pattern: Pattern, error: blaze_inproc::Error },
}

/// A generated table's head, and every pattern not admitted.
pub struct Generated<'a> {
    pub head: TableHead,
    pub refused: AVec<'a, Refusal<'a>>,
}

impl fmt::Display for Why<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Why::Shape { what } => write!(f, "not generated through LLVM: {what}"),
            Why::Constant { class, encoded } => write!(f, "the {class:?} encoding {encoded:#x} names no constant"),
            Why::Register { reg } => write!(f, "x{reg} cannot be pinned for LLVM"),
            Why::Build(e) => write!(f, "the one-instruction LIR function did not build: {e:?}"),
            Why::Lower(e) => write!(f, "{e}"),
            Why::Llvm(e) => write!(f, "LLVM: {e}"),
            Why::Read(e) => write!(f, "reading LLVM's object: {e}"),
            Why::Markers { starts, ends } => {
                write!(f, "the wrapper's markers: {starts} starts and {ends} ends (one each, start first)")
            }
            Why::Instruction { at, word, what } => write!(f, "word {at} ({word:#010x}) is {what}"),
            Why::Discover(e) => write!(f, "{e}"),
            Why::Eval(e) => write!(f, "the evaluator: {e}"),
            Why::Evaluator { inputs, imm, result, want, got } => write!(
                f,
                "on inputs {inputs:?} and immediate {imm:#x}, result in x{result}: the evaluator gives {want:#x}, the template {got:#x}"
            ),
            Why::Differs { inputs, imm, llvm, encoders } => write!(
                f,
                "on inputs {inputs:?} and immediate {imm:#x}: LLVM's template gives {llvm:#x}, the encoders' {encoders:#x}"
            ),
        }
    }
}

impl fmt::Display for Refusal<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = self.pattern;
        write!(f, "`{}` at width {} variant {} {:?}: {}", p.op.name(), p.width, p.variant, p.operands, self.why)
    }
}

impl fmt::Display for T2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            T2Error::Machine { machine } => {
                write!(f, "no LLVM template generator for {:?} on {:?}", machine.arch, machine.os)
            }
            T2Error::Host { machine } => {
                write!(f, "validating {:?} templates runs them: this host is not {:?}", machine.arch, machine.arch)
            }
            T2Error::Init(e) => write!(f, "LLVM's targets: {e}"),
            T2Error::Gen(e) => write!(f, "{e}"),
            T2Error::Map { pattern, error } => {
                write!(f, "mapping a validation run of `{}` at width {}: {error:?}", pattern.op.name(), pattern.width)
            }
        }
    }
}

/// `machine`'s table generated through LLVM for `cpu`, appended to `out` in piece 0's form with
/// [`Source::Llvm`]; its head and every pattern of T1's not admitted, with why. Deterministic:
/// the same LLVM gives the same bytes.
pub fn generate<'a>(machine: Machine, cpu: &str, out: &mut AVec<'a, u8>, alloc: Alloc<'a>) -> Result<Generated<'a>, T2Error> {
    generate_some(machine, cpu, &|_| true, Plant::None, out, alloc)
}

/// [`generate`] over the patterns `keep` takes (the tests take a few: each pattern is about
/// twenty compiles).
pub(crate) fn generate_some<'a>(
    machine: Machine,
    cpu: &str,
    keep: &dyn Fn(&Pattern) -> bool,
    plant: Plant,
    out: &mut AVec<'a, u8>,
    alloc: Alloc<'a>,
) -> Result<Generated<'a>, T2Error> {
    if machine.arch != Arch::Arm64 {
        return Err(T2Error::Machine { machine });
    }
    if !cfg!(target_arch = "aarch64") {
        return Err(T2Error::Host { machine });
    }
    init().map_err(T2Error::Init)?;
    let mut rows: AVec<'a, Row> = Vec::new_in(alloc);
    let mut bytes: AVec<'a, u8> = Vec::new_in(alloc);
    let mut fields: AVec<'a, Field> = Vec::new_in(alloc);
    let mut refused: AVec<'a, Refusal<'a>> = Vec::new_in(alloc);
    let mut fatal: Option<T2Error> = None;
    let cx = Cx { machine, cpu, plant, alloc };
    recipes(machine, &mut |recipe: &Recipe<'_>| {
        if fatal.is_some() || !keep(&recipe.pattern) {
            return Ok(());
        }
        let (bytes_at, fields_at) = (bytes.len(), fields.len());
        match one(&cx, recipe, &mut bytes, &mut fields) {
            Ok(Ok(())) => rows.push(Row {
                pattern: recipe.pattern,
                bytes_at,
                bytes_len: bytes.len() - bytes_at,
                fields_at,
                fields_len: fields.len() - fields_at,
                clobbers: SCRATCH,
            }),
            Ok(Err(why)) => {
                bytes.truncate(bytes_at);
                fields.truncate(fields_at);
                refused.push(Refusal { pattern: recipe.pattern, why });
            }
            Err(error) => fatal = Some(error),
        }
        Ok(())
    })
    .map_err(T2Error::Gen)?;
    if let Some(error) = fatal {
        return Err(error);
    }
    let head = write_table(machine, Source::Llvm, &mut rows, &bytes, &fields, out).map_err(T2Error::Gen)?;
    Ok(Generated { head, refused })
}

/// What every pattern's generation reads.
struct Cx<'c, 'a> {
    machine: Machine,
    cpu: &'c str,
    plant: Plant,
    alloc: Alloc<'a>,
}

/// What kind of operation a pattern is, as this generator builds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Binary,
    Icmp(IntCond),
    Convert,
}

/// A pattern this generator makes: its kind, its inputs' and result's widths, and per operand
/// slot whether it is a register, the immediate (with its class), or absent.
#[derive(Clone, Copy, Debug)]
struct Shape {
    op: Op,
    kind: Kind,
    in_bits: u32,
    out_bits: u32,
    operands: [Form; 4],
    /// The immediate's operand number and class, if one.
    imm: Option<(u8, ImmClass)>,
}

impl Shape {
    fn of<'w>(p: &Pattern) -> Result<Shape, Why<'w>> {
        let refuse = |what| Err(Why::Shape { what });
        if p.inner.is_some() {
            return refuse("a fused pattern");
        }
        if p.result != Form::Reg(blaze_tier::RegClass::Gpr) {
            return refuse("a result that is not one integer register");
        }
        let width = u32::from(p.width);
        let (kind, in_bits, out_bits) = match p.op {
            Op::Add | Op::Sub | Op::Mul | Op::And | Op::Or | Op::Xor | Op::Shl | Op::Lshr | Op::Ashr => {
                if p.variant != 0 {
                    return refuse("a variant of a binary operation");
                }
                (Kind::Binary, width, width)
            }
            Op::Icmp => match IntCond::from_word(u32::from(p.variant)) {
                Some(cond) => (Kind::Icmp(cond), width, 1),
                None => return refuse("an icmp variant that is no condition"),
            },
            Op::Zext | Op::Sext | Op::Trunc => (Kind::Convert, u32::from(p.variant), width),
            _ => return refuse("an operation other than integer arithmetic, a compare or a conversion"),
        };
        for bits in [in_bits, out_bits] {
            if !matches!(bits, 1 | 8 | 16 | 32 | 64) {
                return refuse("a width other than 1, 8, 16, 32 or 64");
            }
        }
        let mut imm = None;
        let mut count = 0;
        for (k, form) in p.operands.iter().enumerate() {
            match *form {
                Form::None => {}
                Form::Reg(blaze_tier::RegClass::Gpr) => count += 1,
                Form::Imm(class) => {
                    if imm.is_some() {
                        return refuse("two immediates");
                    }
                    if !matches!(
                        class,
                        ImmClass::U12
                            | ImmClass::U12Lsl12
                            | ImmClass::Logical32
                            | ImmClass::Logical64
                            | ImmClass::Shift5
                            | ImmClass::Shift6
                            | ImmClass::Mov16
                            | ImmClass::I64
                    ) {
                        return refuse("an immediate class of memory or of x86-64");
                    }
                    imm = Some((k as u8, class));
                    count += 1;
                }
                _ => return refuse("an operand that is a float register, a site, a block or a slot"),
            }
            // Operands are a prefix: none after the first absent one.
            if *form == Form::None && p.operands[k..].iter().any(|later| *later != Form::None) {
                return refuse("an absent operand before a present one");
            }
        }
        let want = match kind {
            Kind::Binary | Kind::Icmp(_) => 2,
            Kind::Convert => 1,
        };
        if count != want {
            return refuse("an operand count the operation does not take");
        }
        Ok(Shape { op: p.op, kind, in_bits, out_bits, operands: p.operands, imm })
    }

    /// The register operands, in operand order.
    fn regs(&self) -> impl Iterator<Item = u8> + '_ {
        (0u8..4).filter(|&k| matches!(self.operands[usize::from(k)], Form::Reg(_)))
    }
}

const fn mask(bits: u32) -> u64 {
    if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 }
}

/// The constant an immediate's encoding names, or `None` (an invalid logical immediate).
fn constant(class: ImmClass, encoded: u64) -> Option<u64> {
    match class {
        ImmClass::U12 | ImmClass::Shift5 | ImmClass::Shift6 | ImmClass::Mov16 | ImmClass::I64 => Some(encoded),
        ImmClass::U12Lsl12 => Some(encoded << 12),
        ImmClass::Logical32 => logical(encoded, 32),
        ImmClass::Logical64 => logical(encoded, 64),
        _ => None,
    }
}

/// The Arm ARM's `DecodeBitMasks` (`N:immr:imms` in `encoded`'s low 13 bits) for a register of
/// `size` bits: the element of `imms + 1` ones rotated right by `immr`, repeated.
pub(crate) fn logical(encoded: u64, size: u32) -> Option<u64> {
    let (n, immr, imms) = ((encoded >> 12) & 1, ((encoded >> 6) & 0x3f) as u32, (encoded & 0x3f) as u32);
    if size == 32 && n == 1 {
        return None;
    }
    // len: the highest set bit of N:NOT(imms), seven bits.
    let combined = (n << 6) | u64::from(!imms & 0x3f);
    if combined == 0 {
        return None;
    }
    let len = 63 - combined.leading_zeros();
    if len < 1 {
        return None;
    }
    let levels = (1u32 << len) - 1;
    if imms & levels == levels {
        return None;
    }
    let (s, r, esize) = (imms & levels, immr & levels, 1u32 << len);
    let welem = mask(s + 1);
    let emask = mask(esize);
    let rotated = if r == 0 { welem } else { ((welem >> r) | (welem << (esize - r))) & emask };
    let mut value = 0u64;
    let mut at = 0;
    while at < size {
        value |= rotated << at;
        at += esize;
    }
    Some(value & mask(size))
}

/// The one-instruction LIR function of `shape` with the immediate's encoding `imm` (where it has
/// one): its register operands are its parameters, in operand order.
fn build<'a>(shape: &Shape, imm: u64, plant: Plant, alloc: Alloc<'a>) -> Result<Func<Alloc<'a>>, Why<'a>> {
    let shape_err = |what| Why::Shape { what };
    let in_ty = Ty::int(shape.in_bits).ok_or(shape_err("an input width LIR has no integer for"))?;
    let out_ty = Ty::int(shape.out_bits).ok_or(shape_err("a result width LIR has no integer for"))?;
    let mut b = Builder::<Alloc<'a>, Full>::new_in(64, alloc);
    let params = [Param::of(in_ty); 4];
    let count = shape.regs().count();
    let sig = b.sig(Conv::C, Variadic::No, &params[..count], &[Param::of(out_ty)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let mut values = [None; 4];
    for k in shape.regs() {
        values[usize::from(k)] = Some(b.param(in_ty));
    }
    if let Some((k, class)) = shape.imm {
        let value = constant(class, imm).ok_or(Why::Constant { class, encoded: imm })?;
        values[usize::from(k)] = Some(b.iconst(in_ty, u128::from(value & mask(shape.in_bits))));
    }
    if plant == Plant::SwapOperands {
        values.swap(0, 1);
    }
    let first = values[0].ok_or(shape_err("no first operand"))?;
    let r = match shape.kind {
        Kind::Binary => b.binary(shape.op, Flags::NONE, first, values[1].ok_or(shape_err("no second operand"))?),
        Kind::Icmp(cond) => b.icmp(cond, first, values[1].ok_or(shape_err("no second operand"))?),
        Kind::Convert => b.convert(shape.op, Flags::NONE, out_ty, first),
    };
    b.ret(&[r]);
    b.finish_checked().map_err(Why::Build)
}

/// The one-instruction function names nothing outside.
struct NoNames;

impl Names for NoNames {
    fn name(&self, _: Outside) -> Option<Named<'_>> {
        None
    }
}

/// The registers kept live across the template, as a bit set: every allocatable one but x16/x17
/// (x0..x15, x18 off macOS, x19..x28; x29 is the frame pointer, x30 is declared clobbered,
/// 31 is `sp`).
fn kept(machine: Machine) -> u32 {
    let mut set = 0xffffu32 | (0x3ff << 19);
    if machine.os != Os::MacOs {
        set |= 1 << 18;
    }
    set
}

/// A register LLVM can be pinned to.
fn pinnable(machine: Machine, reg: u64) -> bool {
    reg <= 28 && !(reg == 18 && machine.os == Os::MacOs)
}

/// One LLVM type's text for an integer of `bits`.
struct IntTy(u32);

impl fmt::Display for IntTy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "i{}", self.0)
    }
}

/// The LLVM struct type of `n` `i64`s: what the first marker's outputs come back as.
struct Tuple(u32);

impl fmt::Display for Tuple {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{")?;
        for j in 0..self.0 {
            f.write_str(if j == 0 { "i64" } else { ", i64" })?;
        }
        f.write_str("}")
    }
}

/// The wrapper (module doc) appended to `ir`: inputs pinned by `vals`, the result to
/// `vals[RESULT]`.
fn wrapper<'w>(ir: &mut AString<'_>, shape: &Shape, machine: Machine, vals: &Vals) -> Result<(), Why<'w>> {
    let text = |_: fmt::Error| Why::Lower(LowerError::Text);
    let result = vals[usize::from(RESULT)];
    let mut live = kept(machine);
    for k in shape.regs() {
        live |= 1u32 << vals[usize::from(k)];
    }
    let index = |reg: u64| (live & ((1u32 << reg) - 1)).count_ones();
    let tuple = Tuple(live.count_ones());
    let name = ir_name(FUNC);
    write!(ir, "\ndefine void @{WRAPPER}() \"frame-pointer\"=\"all\" {{\n  %in = call {tuple} asm sideeffect \"hint #112\", \"")
        .map_err(text)?;
    let mut first = true;
    for reg in 0..31u64 {
        if live >> reg & 1 == 1 {
            write!(ir, "{}={{x{reg}}}", if first { "" } else { "," }).map_err(text)?;
            first = false;
        }
    }
    ir.push_str("\"()\n");
    let (in_ty, out_ty) = (IntTy(shape.in_bits), IntTy(shape.out_bits));
    for k in shape.regs() {
        writeln!(ir, "  %p{k} = extractvalue {tuple} %in, {}", index(vals[usize::from(k)])).map_err(text)?;
        if shape.in_bits < 64 {
            writeln!(ir, "  %a{k} = trunc i64 %p{k} to {in_ty}").map_err(text)?;
        } else {
            writeln!(ir, "  %a{k} = add i64 %p{k}, 0").map_err(text)?;
        }
    }
    write!(ir, "  %r = call {out_ty} @{}(", ir_name_str(&name)).map_err(text)?;
    for (j, k) in shape.regs().enumerate() {
        write!(ir, "{}{in_ty} %a{k}", if j == 0 { "" } else { ", " }).map_err(text)?;
    }
    ir.push_str(")\n");
    if shape.out_bits < 64 {
        writeln!(ir, "  %z = zext {out_ty} %r to i64").map_err(text)?;
    } else {
        ir.push_str("  %z = add i64 %r, 0\n");
    }
    for reg in 0..31u64 {
        if live >> reg & 1 == 1 && reg != result {
            writeln!(ir, "  %k{reg} = extractvalue {tuple} %in, {}", index(reg)).map_err(text)?;
        }
    }
    write!(ir, "  call void asm sideeffect \"hint #113\", \"{{x{result}}}").map_err(text)?;
    for reg in 0..31u64 {
        if live >> reg & 1 == 1 && reg != result {
            write!(ir, ",{{x{reg}}}").map_err(text)?;
        }
    }
    ir.push_str("\"(i64 %z");
    for reg in 0..31u64 {
        if live >> reg & 1 == 1 && reg != result {
            write!(ir, ", i64 %k{reg}").map_err(text)?;
        }
    }
    ir.push_str(")\n  ret void\n}\n");
    Ok(())
}

/// Refuses a word a template must not hold: the branch, exception and system group (a block
/// LLVM laid out beyond the template, a call, a trap) and the loads and stores (a spill).
fn check_words<'w>(words: &[u8]) -> Result<(), Why<'w>> {
    for (at, w) in words.chunks_exact(4).enumerate() {
        let word = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
        let what = if (word >> 26) & 0b111 == 0b101 {
            "a branch, trap or system instruction (a block or call the template cannot hold)"
        } else if (word >> 25) & 0b101 == 0b100 {
            "a memory access (a spill: the pins left LLVM too few registers)"
        } else {
            continue;
        };
        return Err(Why::Instruction { at: at as u32, word, what });
    }
    Ok(())
}

/// The wrapper names nothing outside its object.
fn no_name(_: &str) -> Option<Target> {
    None
}

/// LLVM's code for `shape` with operands as `vals` says: the words between the markers.
fn encode_llvm<'a>(cx: &Cx<'_, 'a>, shape: &Shape, vals: &Vals, code: &mut Code) -> Result<(), Why<'a>> {
    let result = vals[usize::from(RESULT)];
    for reg in shape.regs().map(|k| vals[usize::from(k)]).chain([result]) {
        if !pinnable(cx.machine, reg) {
            return Err(Why::Register { reg });
        }
    }
    let imm = shape.imm.map_or(0, |(k, _)| vals[usize::from(k)]);
    let func = build(shape, imm, cx.plant, cx.alloc)?;
    // Internal: once inlined into the wrapper it is not emitted, so the object defines the
    // wrapper alone (a record is one function: read.rs refuses a second definition).
    let lowered = lower_as(func.view(), &NoNames, FUNC, Root::Internal, &[], &[], cx.alloc).map_err(Why::Lower)?;
    let mut ir = lowered.ir;
    wrapper(&mut ir, shape, cx.machine, vals)?;
    let mut object: AVec<'a, u8> = Vec::new_in(cx.alloc);
    let mut remarks: AVec<'a, Remark> = Vec::new_in(cx.alloc);
    compile(ir.as_bytes(), &cx.machine, cx.cpu, Pipeline::BlazeO2, &mut object, &mut remarks, cx.alloc).map_err(Why::Llvm)?;
    let emitted = read_object(&object, WRAPPER, &no_name, cx.alloc).map_err(Why::Read)?;
    let (mut starts, mut ends, mut from, mut to) = (0u32, 0u32, 0usize, 0usize);
    for (at, w) in emitted.code.chunks_exact(4).enumerate() {
        match u32::from_le_bytes([w[0], w[1], w[2], w[3]]) {
            START => {
                starts += 1;
                from = (at + 1) * 4;
            }
            END => {
                ends += 1;
                to = at * 4;
            }
            _ => {}
        }
    }
    if starts != 1 || ends != 1 || from > to {
        return Err(Why::Markers { starts, ends });
    }
    let template = &emitted.code[from..to];
    check_words(template)?;
    for w in template.chunks_exact(4) {
        code.word(u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
    }
    Ok(())
}

/// One pattern: its template found through LLVM and validated, appended to `bytes` and `fields`;
/// `Ok(Err(why))` when it is not admitted (the caller cuts what was appended).
fn one<'a>(
    cx: &Cx<'_, 'a>,
    recipe: &Recipe<'_>,
    bytes: &mut AVec<'a, u8>,
    fields: &mut AVec<'a, Field>,
) -> Result<Result<(), Why<'a>>, T2Error> {
    let shape = match Shape::of(&recipe.pattern) {
        Ok(shape) => shape,
        Err(why) => return Ok(Err(why)),
    };
    // LLVM's template has the pattern's operands and its result; never T1's temporaries.
    let mut slots = [Slot { operand: 0, kind: SlotKind::Reg, bits: 0 }; 5];
    let mut count = 0;
    for slot in recipe.slots {
        let operand_present = slot.operand < 4 && recipe.pattern.operands[usize::from(slot.operand)] != Form::None;
        if (operand_present || slot.operand == RESULT) && matches!(slot.kind, SlotKind::Reg | SlotKind::Imm) && count < 5 {
            slots[count] = *slot;
            count += 1;
        }
    }
    let slots = &slots[..count];
    let stash: RefCell<Option<Why<'a>>> = RefCell::new(None);
    let encode = |vals: &Vals, code: &mut Code| {
        if stash.borrow().is_some() {
            return;
        }
        if let Err(why) = encode_llvm(cx, &shape, vals, code) {
            *stash.borrow_mut() = Some(why);
        }
    };
    let llvm = Recipe { pattern: recipe.pattern, slots, site: None, clobbers: SCRATCH, insn_bytes: 4, encode: &encode };
    let (bytes_at, fields_at) = (bytes.len(), fields.len());
    let found = discover(&llvm, bytes, fields);
    if let Some(why) = stash.into_inner() {
        return Ok(Err(why));
    }
    if let Err(error) = found {
        return Ok(Err(Why::Discover(error)));
    }
    // T1's template for the same pattern, from its own recipe.
    let mut t1_bytes: AVec<'a, u8> = Vec::new_in(cx.alloc);
    let mut t1_fields: AVec<'a, Field> = Vec::new_in(cx.alloc);
    let t1 = match discover(recipe, &mut t1_bytes, &mut t1_fields) {
        Ok(()) => match check_words(&t1_bytes) {
            Ok(()) => Some((&t1_bytes[..], &t1_fields[..])),
            Err(why) => return Ok(Err(why)),
        },
        Err(_) => None,
    };
    let imm_bits = slots.iter().find(|s| s.kind == SlotKind::Imm).map_or(0, |s| u32::from(s.bits));
    validate(cx, &shape, recipe.pattern, imm_bits, &bytes[bytes_at..], &fields[fields_at..], t1)
}

/// The edge values of `bits` (0, 1, -1, the minimum, the maximum, a power of two), distinct.
fn edges(bits: u32) -> ([u64; 6], usize) {
    let m = mask(bits);
    let all = [0, 1, m, (1u64 << (bits - 1)) & m, m >> 1, (1u64 << (bits / 2)) & m];
    let mut out = [0u64; 6];
    let mut n = 0;
    for v in all {
        if !out[..n].contains(&v) {
            out[n] = v;
            n += 1;
        }
    }
    (out, n)
}

/// A template filled for a run: inputs in x0..x3, the result in `result`, T1's temporaries in
/// x10/x11, the immediate `imm`; then `mov x0, x<result>` and `ret`. Mapped executable.
fn placed(pattern: Pattern, template: &[u8], fields: &[Field], shape: &Shape, imm: u64, result: u64) -> Result<Executable, T2Error> {
    let mut vals: Vals = [0; blaze_emit::table_gen::OPERANDS];
    for k in shape.regs() {
        vals[usize::from(k)] = u64::from(k);
    }
    if let Some((k, _)) = shape.imm {
        vals[usize::from(k)] = imm;
    }
    vals[usize::from(RESULT)] = result;
    vals[usize::from(TEMP0)] = RUN_TEMP0;
    vals[usize::from(TEMP1)] = RUN_TEMP1;
    let mut code = [0u8; MOST_RUN];
    let len = template.len();
    code[..len].copy_from_slice(template);
    fill(&mut code[..len], fields, &vals);
    let mut at = len;
    if result != 0 {
        code[at..at + 4].copy_from_slice(&(MOV_X0 | ((result as u32) << 16)).to_le_bytes());
        at += 4;
    }
    code[at..at + 4].copy_from_slice(&RET.to_le_bytes());
    at += 4;
    Executable::new(&code[..at], 0).map_err(|error| T2Error::Map { pattern, error })
}

/// Calls the code at `exec`'s first byte with four integer arguments; `None` on a host that is
/// not arm64 (generation refuses such a host before any run).
#[cfg(target_arch = "aarch64")]
fn call(exec: &Executable, args: [u64; 4]) -> Option<u64> {
    // SAFETY: `exec` holds a leaf function of the C convention made by `placed`: a template that
    // `check_words` found to hold no branch, trap, system instruction or memory access, filled
    // to read x0..x3 and write its result register, x10, x11, x16 or x17 (all caller-saved),
    // then `mov x0` and `ret`; it is mapped executable for as long as `exec` lives.
    let f = unsafe { core::mem::transmute::<usize, extern "C" fn(u64, u64, u64, u64) -> u64>(exec.code_address() as usize) };
    Some(f(args[0], args[1], args[2], args[3]))
}

#[cfg(not(target_arch = "aarch64"))]
fn call(_: &Executable, _: [u64; 4]) -> Option<u64> {
    None
}

/// The validation (module doc) of LLVM's `template` with `fields`, and T1's where there is one.
#[allow(clippy::too_many_arguments)]
fn validate<'a>(
    cx: &Cx<'_, 'a>,
    shape: &Shape,
    pattern: Pattern,
    imm_bits: u32,
    template: &[u8],
    fields: &[Field],
    t1: Option<(&[u8], &[Field])>,
) -> Result<Result<(), Why<'a>>, T2Error> {
    let imms: [u64; 3] = match shape.imm {
        Some(_) => [1, 0x5555_5555_5555_5555 & mask(imm_bits), mask(imm_bits)],
        None => [0; 3],
    };
    let regs = shape.regs().count() as u32;
    let (edge, n) = edges(shape.in_bits);
    let combos = (n as u32).pow(regs);
    let mut tried = 0;
    for (j, &imm) in imms.iter().enumerate() {
        if shape.imm.is_none() && j > 0 {
            break;
        }
        if let Some((_, class)) = shape.imm
            && constant(class, imm).is_none()
        {
            continue;
        }
        tried += 1;
        let func = match build(shape, imm, Plant::None, cx.alloc) {
            Ok(func) => func,
            Err(why) => return Ok(Err(why)),
        };
        // The result in its own register, then in each input's.
        let mut results = [RUN_RESULT; 5];
        let mut runs = 1;
        for k in shape.regs() {
            results[runs] = u64::from(k);
            runs += 1;
        }
        let mut execs: [Option<Executable>; 5] = [None, None, None, None, None];
        for (slot, &result) in execs.iter_mut().zip(&results[..runs]) {
            *slot = Some(placed(pattern, template, fields, shape, imm, result)?);
        }
        let t1_exec = match t1 {
            Some((b, f)) => Some(placed(pattern, b, f, shape, imm, RUN_RESULT)?),
            None => None,
        };
        for combo in 0..combos {
            let mut inputs = [0u64; 4];
            let mut args: AVec<'a, Val> = Vec::new_in(cx.alloc);
            let mut digit = combo;
            for k in shape.regs() {
                let v = edge[(digit % n as u32) as usize];
                digit /= n as u32;
                inputs[usize::from(k)] = v;
                args.push(Val::Int(u128::from(v)));
            }
            let want = match run_in(func.view(), &args, FUEL, cx.alloc) {
                Ok(Outcome::Returned(values)) => match values.first() {
                    Some(Val::Int(v)) => *v,
                    _ => return Ok(Err(Why::Shape { what: "a function whose evaluator result is not one integer" })),
                },
                // A trap or undefined behaviour: any template is right there.
                Ok(_) => continue,
                Err(e) => return Ok(Err(Why::Eval(e))),
            };
            let mut llvm_value = 0;
            for (exec, &result) in execs.iter().zip(&results[..runs]) {
                let Some(exec) = exec else { continue };
                let got = call(exec, inputs).ok_or(T2Error::Host { machine: cx.machine })?;
                if u128::from(got) != want {
                    return Ok(Err(Why::Evaluator { inputs, imm, result, want, got }));
                }
                llvm_value = got;
            }
            if let Some(exec) = &t1_exec {
                let encoders = call(exec, inputs).ok_or(T2Error::Host { machine: cx.machine })?;
                if encoders != llvm_value {
                    return Ok(Err(Why::Differs { inputs, imm, llvm: llvm_value, encoders }));
                }
            }
        }
    }
    if tried == 0 {
        return Ok(Err(Why::Shape { what: "an immediate class with no valid value to validate at" }));
    }
    Ok(Ok(()))
}
