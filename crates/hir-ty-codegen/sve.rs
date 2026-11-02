//! AArch64's scalable vectors (SVE and SVE2): `core::arch::aarch64`'s `svint32_t`, `svbool_t`, the
//! tuples of them and the internal `nxv2i8`-like types, whose size is a multiple of a vector
//! length only the machine knows.
//!
//! Cranelift has no type for them, so a scalable value lives in memory as its register image:
//! the bytes `str z` writes of a data vector (VL / 8 of them), those `str p` writes of a
//! predicate (VL / 64), the vectors of a tuple one after another, each in a place sized for the
//! largest vector length the architecture allows (2048 bits). Every place of a scalable type is
//! such storage of blaze's own (a local, the copy of an argument, a return slot), so a copy of
//! the whole of it is exact at any vector length. Between blaze's functions a scalable value is
//! passed and returned by reference. What reads one through a pointer, or passes one where
//! foreign code expects rustc's convention (in Z and P registers), is refused.
//!
//! Each operation is a call of an assembled function ([`stub`]) that loads its scalable operands
//! from the places it is handed (`ldr z`, `ldr p`), runs the instruction LLVM selects for the
//! intrinsic, and stores the result (`str z`, `str p`) or returns a scalar.

#![forbid(unsafe_code)]

use std::fmt::{self, Write};

use base_db::Db;
use cranelift_codegen::ir::{AbiParam, InstBuilder, Signature, Value};
use hir_def::FunctionId;
use hir_ty::{
    db::HirDatabase,
    intrinsic::{Intrinsic, SimdOp},
    layout::{InternedLayout, Layout, intern_layout},
    mir::{Operand, OperandKind, Rvalue, StatementKind},
    next_solver::{ConstKind, DbInterner, GenericArgs, Ty, TyKind},
    traits::StoredParamEnvAndCrate,
};
use rustc_abi::{BackendRepr, Size};
use rustc_type_ir::TypeVisitableExt;
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind, SliceLike as _};
use smallvec::SmallVec;

use super::{CodegenError, FunctionCx, MEM, Result, ptr};

/// The bytes of a Z register at the largest vector length, 2048 bits.
pub(super) const DATA_MAX: u64 = 256;
/// The bytes of a P register then: one bit per byte of a Z register.
pub(super) const PREDICATE_MAX: u64 = DATA_MAX / 8;

/// What a scalable vector type holds, from its layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Shape {
    /// The bits of a lane: 1 for a predicate (`bool` lanes).
    pub(super) bits: u64,
    pub(super) float: bool,
    pub(super) signed: bool,
    /// Lanes per 128 bits of vector length.
    pub(super) count: u64,
    /// Vectors of a tuple, 1 for a vector alone.
    pub(super) vectors: u64,
}

impl Shape {
    pub(super) fn of(layout: &Layout) -> Option<Shape> {
        let BackendRepr::SimdScalableVector { element, count, number_of_vectors } =
            layout.backend_repr
        else {
            return None;
        };
        let (bits, float, signed) = if element.is_bool() {
            (1, false, false)
        } else {
            let primitive = element.primitive();
            let float = matches!(primitive, rustc_abi::Primitive::Float(_));
            let signed = matches!(primitive, rustc_abi::Primitive::Int(_, true));
            // A lane's size does not depend on the data layout's pointer width here: only
            // integers and floats are lanes of a scalable vector.
            let bits = match primitive {
                rustc_abi::Primitive::Int(int, _) => int.size().bits(),
                rustc_abi::Primitive::Float(float) => float.size().bits(),
                rustc_abi::Primitive::Pointer(_) => return None,
            };
            (bits, float, signed)
        };
        let vectors = u64::from(number_of_vectors.0);
        Some(Shape { bits, float, signed, count: count.as_u64(), vectors })
    }

    /// What a vector of 128 bits of NEON's holds (`float32x4_t`, `int8x16_t`), as the shape
    /// of a scalable vector of as many lanes per 128 bits.
    pub(super) fn fixed(layout: &Layout) -> Option<Shape> {
        let BackendRepr::SimdVector { element, count } = layout.backend_repr else {
            return None;
        };
        let (bits, float, signed) = match element.primitive() {
            rustc_abi::Primitive::Int(int, signed) => (int.size().bits(), false, signed),
            rustc_abi::Primitive::Float(float) => (float.size().bits(), true, true),
            rustc_abi::Primitive::Pointer(_) => return None,
        };
        let count = count.as_u64();
        (layout.size.bits() == 128 && bits * count == 128).then_some(Shape {
            bits,
            float,
            signed,
            count,
            vectors: 1,
        })
    }

    pub(super) fn predicate(self) -> bool {
        self.bits == 1
    }

    /// The bytes of the storage of one vector of it.
    pub(super) fn vector_storage(self) -> u64 {
        if self.predicate() { PREDICATE_MAX } else { DATA_MAX }
    }

    pub(super) fn storage(self) -> u64 {
        self.vector_storage() * self.vectors
    }

    /// The size of an element the register holds a lane in: its own for a data vector whose
    /// lanes fill the register (`nxv4i32`), wider for an unpacked one (`nxv2i8`'s lanes each
    /// in the low bits of 64), and for a predicate that of the data its lanes govern.
    pub(super) fn container_bits(self) -> u64 {
        128 / self.count
    }

    /// The instruction suffix of the elements the register holds.
    pub(super) fn suffix(self) -> Option<&'static str> {
        Some(match self.container_bits() {
            8 => "b",
            16 => "h",
            32 => "s",
            64 => "d",
            _ => return None,
        })
    }

    /// Whether `part` is LLVM's name of the type, `nxv4i32`, `nxv2f64`, `nxv16i1`, as an
    /// intrinsic's name has it.
    fn is_llvm(self, part: &str) -> bool {
        self.is_named("nxv", part)
    }

    /// Whether `part` is LLVM's name of the vector of 128 bits of NEON's with its lanes:
    /// `v4i32`, `v2f64`.
    fn is_llvm_fixed(self, part: &str) -> bool {
        self.is_named("v", part)
    }

    /// Whether `part` is `{prefix}{count}{i or f}{bits}`, each number in decimal.
    fn is_named(self, prefix: &str, part: &str) -> bool {
        let kind = if self.float { 'f' } else { 'i' };
        part.strip_prefix(prefix)
            .and_then(|it| strip_decimal(it, self.count))
            .and_then(|it| it.strip_prefix(kind))
            .and_then(|it| strip_decimal(it, self.bits))
            .is_some_and(str::is_empty)
    }

    /// The letter of a load or store of one lane of memory: `ld1b` to `ld1d`.
    fn memory_size(self) -> Option<&'static str> {
        Some(match self.bits {
            8 => "b",
            16 => "h",
            32 => "w",
            64 => "d",
            _ => return None,
        })
    }

    /// Whether two types have one register image for the same value, so that rustc's
    /// `transmute` (LLVM's `bitcast`, which on a little-endian target reinterprets the register)
    /// is a copy of the image: two data vectors whose lanes fill the register, or two of one
    /// shape. An unpacked vector's `bitcast` to another shape moves lanes, which a copy of the
    /// register image does not, and a predicate is never bitcast to another kind.
    pub(super) fn same_image(self, other: Shape) -> bool {
        if self.vectors != other.vectors {
            return false;
        }
        let full = |it: Shape| !it.predicate() && it.bits * it.count == 128;
        (full(self) && full(other)) || (self.bits == other.bits && self.count == other.count)
    }
}

/// The rest of `s` after the decimal digits of `n`, as `{n}` writes them (no leading zeros).
fn strip_decimal(s: &str, n: u64) -> Option<&str> {
    let len = s.bytes().take_while(u8::is_ascii_digit).count();
    let (digits, rest) = s.split_at(len);
    let canonical = len == 1 || (len > 1 && !digits.starts_with('0'));
    (canonical && digits.parse::<u64>() == Ok(n)).then_some(rest)
}

/// The layout codegen gives a scalable type: rustc's, sized for its storage at the largest
/// vector length rather than at the least. Interned as the database's are, so it lives as
/// long as they do.
pub(super) fn storage(layout: &'static InternedLayout) -> &'static Layout {
    let Some(shape) = Shape::of(layout) else { return layout };
    // clone: a scalable type's layout only, once per type on the layout cache's miss
    let mut storage = layout.0.clone();
    storage.size = Size::from_bytes(shape.storage());
    // alloc: a scalable type's layout only, once per type on the layout cache's miss
    intern_layout(storage).get()
}

/// What an operand of a stub is.
#[derive(Clone, Copy, Debug)]
pub(super) enum Arg {
    /// A scalable vector, handed over as the address of its image.
    Vector(Shape),
    /// An integer, pointer or fieldless enum of so many bits, in a general register, and its
    /// value when it is a constant, which an instruction may take in its text instead.
    Int(u64, Option<i128>),
    /// A float of so many bits, in the low bits of a vector register.
    Float(u64),
    /// A vector of 128 bits of NEON's, of the lanes of the shape, handed over as the address
    /// of its bytes.
    Fixed(Shape),
}

/// What a stub gives.
#[derive(Clone, Copy, Debug)]
pub(super) enum Ret {
    /// Nothing.
    Unit,
    /// A scalable vector, written to the address handed over first.
    Vector(Shape),
    /// An integer of so many bits, in `x0`.
    Int(u64),
    /// A float of so many bits, in the low bits of `v0`.
    Float(u64),
}

/// A register an operand or the result of a stub is in.
#[derive(Clone, Copy, Debug)]
enum Reg {
    /// A general register, its `w` name for up to 32 bits.
    X(u32, u64),
    /// A vector register with a float of so many bits in its low bits.
    F(u32, u64),
    /// A data vector, or the first of the consecutive registers of a tuple.
    Z(u32, Shape),
    P(u32, Shape),
}

impl Reg {
    /// Its name alone: `p0` of a governing predicate, `z1` of the low bits of a float.
    fn bare(self) -> RegName {
        RegName(self, View::Bare)
    }

    fn number(self) -> u32 {
        match self {
            Reg::X(n, _) | Reg::F(n, _) | Reg::Z(n, _) | Reg::P(n, _) => n,
        }
    }

    /// Its name with the size of its elements, as an instruction takes it: `z1.s`, `p0.h`,
    /// `w2`, `s0`.
    fn full(self) -> RegName {
        RegName(self, View::Full)
    }

    /// The registers of a vector or tuple as a list: `{ z0.s }`, `{ z0.s, z1.s }`,
    /// `{ z0.s - z2.s }`.
    fn list(self) -> RegName {
        RegName(self, View::List)
    }

    /// The register taken as elements of 128 bits, or of `suffix`.
    fn with(self, suffix: &'static str) -> RegName {
        RegName(self, View::With(suffix))
    }
}

/// How a register is named in an instruction's text.
#[derive(Clone, Copy, Debug)]
enum View {
    Bare,
    Full,
    List,
    With(&'static str),
}

/// A register's name as [`Reg::bare`], [`Reg::full`], [`Reg::list`] or [`Reg::with`] give
/// it, written straight into the stub's text.
#[derive(Clone, Copy, Debug)]
struct RegName(Reg, View);

impl fmt::Display for RegName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let RegName(reg, view) = *self;
        match (view, reg) {
            (View::Bare, Reg::X(n, 64)) => write!(f, "x{n}"),
            (View::Bare, Reg::X(n, _)) => write!(f, "w{n}"),
            (View::Bare, Reg::F(n, _) | Reg::Z(n, _)) => write!(f, "z{n}"),
            (View::Bare, Reg::P(n, _)) => write!(f, "p{n}"),
            (View::Full | View::List, Reg::X(..)) => write!(f, "{}", reg.bare()),
            (View::Full | View::List, Reg::F(n, bits)) => write!(f, "{}", scalar(n, bits)),
            (View::Full | View::List, Reg::P(_, shape)) | (View::Full, Reg::Z(_, shape)) => {
                write!(f, "{}.{}", reg.bare(), shape.suffix().unwrap_or("?"))
            }
            (View::List, Reg::Z(n, shape)) => {
                let suffix = shape.suffix().unwrap_or("?");
                match shape.vectors {
                    1 => write!(f, "{{ z{n}.{suffix} }}"),
                    2 => write!(f, "{{ z{n}.{suffix}, z{}.{suffix} }}", n + 1),
                    k => write!(f, "{{ z{n}.{suffix} - z{}.{suffix} }}", n + k as u32 - 1),
                }
            }
            (View::With(suffix), _) => write!(f, "{}.{suffix}", reg.bare()),
        }
    }
}

/// A name that may be none, written as nothing then.
#[derive(Clone, Copy, Debug)]
struct OrEmpty<T>(Option<T>);

impl<T: fmt::Display> fmt::Display for OrEmpty<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(it) => it.fmt(f),
            None => Ok(()),
        }
    }
}

/// The name of the low bits of a vector register: its view letter and number, `b0` to `d0`.
#[derive(Clone, Copy, Debug)]
struct Scalar(char, u32);

impl fmt::Display for Scalar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.0, self.1)
    }
}

/// The name of the low `bits` of vector register `n`: `b0` to `d0`.
fn scalar(n: u32, bits: u64) -> Scalar {
    let view = match bits {
        8 => 'b',
        16 => 'h',
        32 => 's',
        _ => 'd',
    };
    Scalar(view, n)
}

/// The registers a stub has its operands in. The signature [`stub`] is called with has the
/// address of a scalable result first, then the integers, floats and the addresses of the
/// scalable operands, each kind in the order of the operands, by AAPCS64: an integer or
/// address in the next general register, a float in the next vector register. So the
/// integers are in the registers rustc's convention has them in. The stub then puts
/// each operand where rustc's convention for SVE has it (AAPCS64's for scalable vectors), so
/// that the instruction LLVM selects for rustc reads the same registers: floats and data
/// vectors in the vector registers in the order of the operands, predicates in P registers
/// in theirs.
struct Registers {
    /// The register each operand is in.
    operands: SmallVec<[Reg; 8]>,
    /// The register of the result before it is stored or returned.
    result: Option<Reg>,
    /// The general register the result's address is in.
    address: Option<&'static str>,
    /// Each float's move from the vector register it arrives in to its own, in the order of
    /// the floats.
    moves: SmallVec<[(u32, u32); 4]>,
    /// The general register of the first scalable or NEON operand's address.
    first_address: u32,
}

impl Registers {
    fn new(args: &[Arg], ret: Ret) -> std::result::Result<Registers, String> {
        let mut x = 0u32;
        let (address, result) = match ret {
            Ret::Vector(shape) => {
                x += 1;
                let reg = if shape.predicate() { Reg::P(0, shape) } else { Reg::Z(0, shape) };
                (Some("x0"), Some(reg))
            }
            Ret::Int(bits) => (None, Some(Reg::X(0, bits))),
            Ret::Float(bits) => (None, Some(Reg::F(0, bits))),
            Ret::Unit => (None, None),
        };
        if let Some(shape) = Some(ret).and_then(|it| match it {
            Ret::Vector(shape) => Some(shape),
            _ => None,
        }) && (shape.suffix().is_none() || (shape.predicate() && shape.vectors != 1))
        {
            // alloc: the error's message
            return Err(format!("a result of {shape:?}"));
        }
        // The vector register next taken, the floats so far, and the P register next taken.
        let (mut v, mut floats, mut p) = (0u32, 0u32, 0u32);
        // The general register of the next integer, and of the next scalable operand's address.
        let first_address = x + args.iter().filter(|it| matches!(it, Arg::Int(..))).count() as u32;
        let mut x = (x, first_address);
        let mut moves: SmallVec<[(u32, u32); 4]> = SmallVec::new();
        let mut operands: SmallVec<[Reg; 8]> = SmallVec::with_capacity(args.len());
        for arg in args {
            operands.push(match *arg {
                Arg::Int(bits, _) => {
                    x.0 += 1;
                    Reg::X(x.0 - 1, bits)
                }
                Arg::Float(bits) => {
                    // The float arrives in the vector register of its count among the floats.
                    if floats != v {
                        moves.push((floats, v));
                    }
                    floats += 1;
                    v += 1;
                    Reg::F(v - 1, bits)
                }
                // In the low 128 bits of the next vector register, as rustc passes it.
                Arg::Fixed(shape) => {
                    x.1 += 1;
                    v += 1;
                    Reg::Z(v - 1, shape)
                }
                Arg::Vector(shape) => {
                    if shape.suffix().is_none() {
                        // alloc: the error's message
                        return Err(format!("an operand of {shape:?}"));
                    }
                    x.1 += 1;
                    if shape.predicate() {
                        if shape.vectors != 1 {
                            // alloc: the error's message
                            return Err("a tuple of predicates".to_owned());
                        }
                        p += 1;
                        Reg::P(p - 1, shape)
                    } else {
                        v += shape.vectors as u32;
                        Reg::Z(v - shape.vectors as u32, shape)
                    }
                }
            });
        }
        // Beyond z7 the registers' low halves are the callee's to keep (d8 to d15).
        if x.1 > 8 || v > 8 || p > 16 {
            // alloc: the error's message
            return Err("more operands than registers".to_owned());
        }
        Ok(Registers { operands, result, address, moves, first_address })
    }

    /// Writes what moves the floats and loads the scalable and NEON operands of `args` to
    /// their registers.
    fn write_loads(&self, args: &[Arg], out: &mut String) {
        // Each float moves up to its place, the last first, so that none is overwritten
        // before it moves.
        for &(from, to) in self.moves.iter().rev() {
            writeln!(out, "mov z{to}.d, z{from}.d").unwrap();
        }
        let mut address = self.first_address;
        for (arg, reg) in args.iter().zip(&self.operands) {
            match (*arg, *reg) {
                (Arg::Fixed(_), reg) => {
                    writeln!(out, "ldr q{}, [x{address}]", reg.number()).unwrap();
                    address += 1;
                }
                (Arg::Vector(_), Reg::P(p, _)) => {
                    writeln!(out, "ldr p{p}, [x{address}]").unwrap();
                    address += 1;
                }
                (Arg::Vector(shape), Reg::Z(v, _)) => {
                    for at in 0..shape.vectors {
                        // The storage of each vector of a tuple is DATA_MAX bytes apart.
                        if at == 0 {
                            writeln!(out, "ldr z{v}, [x{address}]").unwrap();
                        } else {
                            writeln!(out, "add x16, x{address}, #{}", at * DATA_MAX).unwrap();
                            writeln!(out, "ldr z{}, [x16]", v + at as u32).unwrap();
                        }
                    }
                    address += 1;
                }
                _ => {}
            }
        }
    }
}

/// What the assembler is told the stubs' instructions are of.
const FEATURES: &[&str] =
    &["sve", "sve2", "sve2-aes", "sve2-sha3", "sve2-sm4", "sve2-bitperm", "f32mm", "f64mm", "i8mm"];

/// The bytes a stub's text is begun with room for: the header's 250 and a few lines.
const STUB_CAPACITY: usize = 512;

/// The start of a stub's text, which [`write_end`] finishes: the features and the label,
/// in a buffer that takes the moves and loads and the body after it.
fn begin() -> String {
    // alloc: the stub's text, the output
    let mut text = String::with_capacity(STUB_CAPACITY);
    for feature in FEATURES {
        writeln!(text, ".arch_extension {feature}").unwrap();
    }
    writeln!(text, ".globl blaze_asm_block\nblaze_asm_block:").unwrap();
    text
}

/// The end of a stub's text after its body: the store of the result from `z0` or `p0` (and
/// on, of a tuple), and the return.
fn write_end(text: &mut String, registers: &Registers, ret: Ret) {
    if let (Ret::Vector(shape), Some(address)) = (ret, registers.address) {
        for at in 0..shape.vectors {
            if shape.predicate() {
                writeln!(text, "str p{at}, [{address}]").unwrap();
            } else if at == 0 {
                writeln!(text, "str z0, [{address}]").unwrap();
            } else {
                writeln!(text, "add x16, {address}, #{}\nstr z{at}, [x16]", at * DATA_MAX).unwrap();
            }
        }
    }
    text.push_str("ret\n");
}

/// The name AArch64's assembler gives an `svpattern`, a 5-bit immediate.
fn pattern(imm: i128) -> std::result::Result<Pattern, String> {
    Ok(match imm {
        0 => Pattern::Named("pow2"),
        1..=8 => Pattern::Vl(imm),
        9 => Pattern::Named("vl16"),
        10 => Pattern::Named("vl32"),
        11 => Pattern::Named("vl64"),
        12 => Pattern::Named("vl128"),
        13 => Pattern::Named("vl256"),
        29 => Pattern::Named("mul4"),
        30 => Pattern::Named("mul3"),
        31 => Pattern::Named("all"),
        14..=28 => Pattern::Immediate(imm),
        // alloc: the error's message
        _ => return Err(format!("pattern {imm}")),
    })
}

/// An `svpattern` as [`pattern`] names it: `all`, `vl7`, `#14`.
#[derive(Clone, Copy, Debug)]
enum Pattern {
    Named(&'static str),
    Vl(i128),
    Immediate(i128),
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Pattern::Named(name) => f.write_str(name),
            Pattern::Vl(imm) => write!(f, "vl{imm}"),
            Pattern::Immediate(imm) => write!(f, "#{imm}"),
        }
    }
}

/// A pattern and the multiple of its count, as an instruction takes them: `vl7, mul #16`.
fn counted(imm: i128, multiple: i128) -> std::result::Result<Counted, String> {
    let pattern = pattern(imm)?;
    match multiple {
        1..=16 => Ok(Counted(pattern, multiple)),
        // alloc: the error's message
        _ => Err(format!("a multiple {multiple}")),
    }
}

/// A pattern and the multiple of its count, the multiple left out when it is 1.
#[derive(Clone, Copy, Debug)]
struct Counted(Pattern, i128);

impl fmt::Display for Counted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Counted(pattern, 1) => write!(f, "{pattern}"),
            Counted(pattern, multiple) => write!(f, "{pattern}, mul #{multiple}"),
        }
    }
}

/// The name AArch64's assembler gives an `svprfop`, a 4-bit immediate.
fn prefetch(imm: i128) -> std::result::Result<Prefetch, String> {
    let level = |it: i128| it / 2 % 3 + 1;
    let policy = |it: i128| if it % 2 == 0 { "keep" } else { "strm" };
    Ok(match imm {
        0..=5 => Prefetch::Named("pldl", level(imm), policy(imm)),
        8..=13 => Prefetch::Named("pstl", level(imm - 8), policy(imm)),
        6 | 7 | 14 | 15 => Prefetch::Immediate(imm),
        // alloc: the error's message
        _ => return Err(format!("prefetch operation {imm}")),
    })
}

/// An `svprfop` as [`prefetch`] names it: `pldl1keep`, `pstl3strm`, `#6`.
#[derive(Clone, Copy, Debug)]
enum Prefetch {
    /// The kind, the cache level and the policy.
    Named(&'static str, i128, &'static str),
    Immediate(i128),
}

impl fmt::Display for Prefetch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Prefetch::Named(kind, level, policy) => write!(f, "{kind}{level}{policy}"),
            Prefetch::Immediate(imm) => write!(f, "#{imm}"),
        }
    }
}

/// What a gather or scatter of lanes of memory size `size` (`b` to `d`) does before and the
/// address it takes, as LLVM selects them, of `how` (the parts of its name after `gather` or
/// `scatter`), the scalar register `base` and the vector one `offsets`: the base plus each
/// offset (`sxtw`, `uxtw`: of 32 bits, extended), in elements with `index`, or the scalar plus
/// each address with `scalar.offset`. Of the non-temporal ones (`non_temporal`), which take
/// only `[z0.d, x0]`, LLVM shifts the indices before: `lsl z1.d, z1.d, #3`.
fn gathered<'a>(
    non_temporal: bool,
    how: &[&'a str],
    size: &str,
    base: RegName,
    offsets: Reg,
) -> Option<Gathered<'a>> {
    let Reg::Z(_, shape) = offsets else { return None };
    let shift = "bhwd".find(size)?;
    let z = offsets.full();
    let wide = shape.suffix() == Some("d");
    let narrow = shape.suffix() == Some("s");
    let (before, address) = match (how, non_temporal) {
        ([], true) if wide => (None, Address::VectorFirst),
        (["scalar", "offset"], true) if wide || narrow => (None, Address::VectorFirst),
        ([] | ["scalar", "offset"], false) if wide => (None, Address::ScalarFirst),
        (["scalar", "offset"], false) if narrow => (None, Address::Extended("uxtw")),
        (["index"], true) if wide && shift > 0 => (Some(shift), Address::VectorFirst),
        (["index"], false) if wide && shift > 0 => (None, Address::Shifted("lsl", shift)),
        (["uxtw"], true) if narrow => (None, Address::VectorFirst),
        ([extend @ ("sxtw" | "uxtw")], false) if narrow => (None, Address::Extended(*extend)),
        ([extend @ ("sxtw" | "uxtw"), "index"], false) if narrow && shift > 0 => {
            (None, Address::Shifted(*extend, shift))
        }
        _ => return None,
    };
    Some(Gathered { before, base, z, address })
}

/// What [`gathered`] gives: the shift of the offsets done before, if any, and the address.
#[derive(Clone, Copy, Debug)]
struct Gathered<'a> {
    /// The shift of the indices done before: `lsl z1.d, z1.d, #3`.
    before: Option<usize>,
    base: RegName,
    z: RegName,
    address: Address<'a>,
}

/// The form of a gather's or scatter's address.
#[derive(Clone, Copy, Debug)]
enum Address<'a> {
    /// `[z, base]`.
    VectorFirst,
    /// `[base, z]`.
    ScalarFirst,
    /// `[base, z, sxtw]`.
    Extended(&'a str),
    /// `[base, z, sxtw #2]`.
    Shifted(&'a str, usize),
}

impl Gathered<'_> {
    /// Writes the shift done before, if any, as its own line.
    fn write_before(&self, out: &mut String) {
        if let Some(shift) = self.before {
            let z = self.z;
            writeln!(out, "lsl {z}, {z}, #{shift}").unwrap();
        }
    }
}

impl fmt::Display for Gathered<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Gathered { base, z, .. } = *self;
        match self.address {
            Address::VectorFirst => write!(f, "[{z}, {base}]"),
            Address::ScalarFirst => write!(f, "[{base}, {z}]"),
            Address::Extended(extend) => write!(f, "[{base}, {z}, {extend}]"),
            Address::Shifted(extend, shift) => write!(f, "[{base}, {z}, {extend} #{shift}]"),
        }
    }
}

/// Whether `pg` is a predicate that governs the lanes of `data`, one vector of each.
fn lanes_of(pg: Shape, data: Shape) -> bool {
    pg.predicate() && pg.vectors == 1 && data.vectors == 1 && pg.count == data.count
}

/// The memory-size letter of a lane as wide as its container (`s` to `w`).
fn suffix_size(suffix: &str) -> &'static str {
    match suffix {
        "b" => "b",
        "h" => "h",
        "s" => "w",
        _ => "d",
    }
}

/// The suffix `b`, `h`, `s` or `d` of an LLVM intrinsic's predicate type, `nxv16i1` to `nxv2i1`.
fn predicate_suffix(ty: &str) -> Option<&'static str> {
    Some(match ty {
        "nxv16i1" => "b",
        "nxv8i1" => "h",
        "nxv4i1" => "s",
        "nxv2i1" => "d",
        _ => return None,
    })
}

/// Whether a part of an intrinsic's name says a type it is overloaded on (`nxv4i32`, `i64`,
/// `v4f32`, `f32f64`, `p0`) rather than what it does (`wide`, `x`, `z`, `lane`).
fn is_type(part: &str) -> bool {
    let rest = (part.strip_prefix("nxv").or_else(|| part.strip_prefix('v')))
        .map(|it| it.trim_start_matches(|c: char| c.is_ascii_digit()));
    let scalars = |mut it: &str| {
        let mut any = false;
        while let Some(after) = it.strip_prefix(['i', 'f']) {
            let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            if digits == 0 {
                return false;
            }
            it = &after[digits..];
            any = true;
        }
        any && it.is_empty()
    };
    part == "p0" || rest.is_some_and(scalars) || scalars(part)
}

/// One letter per operand, and after `>` one for the result: `P` a predicate, `V` a data
/// vector, `T` a tuple of them, `N` a vector of NEON's, `I` an integer, `C` a constant one, `F`
/// a float, `-` nothing.
fn signature(args: &[Arg], ret: Ret) -> Letters {
    let vector = |shape: Shape| {
        if shape.predicate() {
            b'P'
        } else if shape.vectors > 1 {
            b'T'
        } else {
            b'V'
        }
    };
    let mut signature = Letters(SmallVec::with_capacity(args.len() + 2));
    signature.0.extend(args.iter().map(|it| match *it {
        Arg::Vector(shape) => vector(shape),
        Arg::Int(_, Some(_)) => b'C',
        Arg::Int(..) => b'I',
        Arg::Float(_) => b'F',
        Arg::Fixed(_) => b'N',
    }));
    signature.0.push(b'>');
    signature.0.push(match ret {
        Ret::Unit => b'-',
        Ret::Vector(shape) => vector(shape),
        Ret::Int(_) => b'I',
        Ret::Float(_) => b'F',
    });
    signature
}

/// The ASCII letters of a [`signature`], on the stack.
struct Letters(SmallVec<[u8; 16]>);

impl Letters {
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("a signature is ASCII letters")
    }
}

/// Each binary operation on the active lanes, the inactive ones of the first operand:
/// `add z0.s, p0/m, z0.s, z1.s`.
const MERGING: &[&str] = &[
    "add", "addp", "and", "asr", "bic", "eor", "fabd", "fadd", "faddp", "fdiv", "fdivr", "fmax",
    "fmaxnm", "fmaxnmp", "fmaxp", "fmin", "fminnm", "fminnmp", "fminp", "fmul", "fmulx", "fscale",
    "fsub", "fsubr", "lsl", "lsr", "mul", "orr", "sabd", "sdiv", "sdivr", "shadd", "shsub",
    "shsubr", "smax", "smaxp", "smin", "sminp", "smulh", "sqadd", "sqrshl", "sqshl", "sqsub",
    "sqsubr", "srhadd", "srshl", "sub", "subr", "suqadd", "uabd", "udiv", "udivr", "uhadd",
    "uhsub", "uhsubr", "umax", "umaxp", "umin", "uminp", "umulh", "uqadd", "uqrshl", "uqshl",
    "uqsub", "uqsubr", "urhadd", "urshl", "usqadd",
];

/// Each operation of two vectors (or predicates) whole: `zip1 z0.s, z0.s, z1.s`,
/// `saddlb z0.d, z0.s, z1.s`.
const BINARY: &[&str] = &[
    "addhnb", "aesd", "aese", "bdep", "bext", "bgrp", "frecps", "frsqrts", "ftsmul", "ftssel",
    "histseg", "pmul", "pmullb", "pmullt", "raddhnb", "rax1", "rsubhnb", "sabdlb", "sabdlt",
    "saddlb", "saddlbt", "saddlt", "saddwb", "saddwt", "sm4e", "sm4ekey", "smullb", "smullt",
    "sqadd", "sqdmulh", "sqdmullb", "sqdmullt", "sqrdmulh", "sqsub", "ssublb", "ssublbt", "ssublt",
    "ssubltb", "ssubwb", "ssubwt", "subhnb", "trn1", "trn2", "uabdlb", "uabdlt", "uaddlb",
    "uaddlt", "uaddwb", "uaddwt", "umullb", "umullt", "uqadd", "uqsub", "usublb", "usublt",
    "usubwb", "usubwt", "uzp1", "uzp2", "zip1", "zip2", "add", "sub", "mul", "and", "orr", "eor",
    "bic", "fadd", "fsub", "fmul", "smulh", "umulh", "sqdmulh", "sqrdmulh",
];

/// Each operation into the first operand of two more: `sdot z0.s, z1.b, z2.b`.
const ACCUMULATING: &[&str] = &[
    "adclb",
    "adclt",
    "addhnt",
    "eorbt",
    "eortb",
    "fmmla",
    "raddhnt",
    "rsubhnt",
    "saba",
    "sabalb",
    "sabalt",
    "sbclb",
    "sbclt",
    "sdot",
    "smlalb",
    "smlalt",
    "smlslb",
    "smlslt",
    "smmla",
    "sqdmlalb",
    "sqdmlalbt",
    "sqdmlalt",
    "sqdmlslb",
    "sqdmlslbt",
    "sqdmlslt",
    "sqrdmlah",
    "sqrdmlsh",
    "subhnt",
    "tbx",
    "uaba",
    "uabalb",
    "uabalt",
    "udot",
    "umlalb",
    "umlalt",
    "umlslb",
    "umlslt",
    "ummla",
    "usdot",
    "usmmla",
    "sudot",
    "fmla",
    "fmls",
    "mla",
    "mls",
    "smlalb",
    "smlalt",
    "umlalb",
    "umlalt",
    "fmlalb",
    "fmlalt",
    "fmlslb",
    "fmlslt",
];

/// Each bitwise operation of three vectors, which LLVM writes on elements of 64 bits whatever
/// the type: `bcax z0.d, z0.d, z1.d, z2.d`.
const BITWISE3: &[&str] = &["bcax", "bsl", "bsl1n", "bsl2n", "eor3", "nbsl"];

/// Each operation of one vector on the active lanes, the inactive ones of the first operand:
/// `abs z0.s, p0/m, z1.s`.
const UNARY: &[&str] = &[
    "abs", "cls", "clz", "cnot", "cnt", "fabs", "fcvt", "fcvtlt", "fcvtnt", "fcvtx", "fcvtxnt",
    "fcvtzs", "fcvtzu", "flogb", "fneg", "frecpx", "frinta", "frinti", "frintm", "frintn",
    "frintp", "frintx", "frintz", "fsqrt", "neg", "not", "rbit", "revb", "revh", "revw", "scvtf",
    "sqabs", "sqneg", "sxtb", "sxth", "sxtw", "ucvtf", "urecpe", "ursqrte", "uxtb", "uxth", "uxtw",
];

/// Each comparison of the active lanes, the inactive ones false: `cmpeq p0.s, p0/z, z0.s, z1.s`.
const COMPARE: &[&str] = &[
    "cmpeq", "cmpge", "cmpgt", "cmphi", "cmphs", "cmple", "cmplo", "cmpls", "cmplt", "cmpne",
    "facge", "facgt", "fcmpeq", "fcmpge", "fcmpgt", "fcmpne", "fcmpuo", "match", "nmatch",
];

/// Each operation of two predicates on the active lanes, the inactive ones false:
/// `and p0.b, p0/z, p1.b, p2.b`.
const PREDICATE: &[&str] =
    &["and", "bic", "brkpa", "brkpb", "eor", "nand", "nor", "orn", "orr", "sel"];

/// Each multiply-add of the active lanes, the inactive ones of the first operand:
/// `fmla z0.s, p0/m, z1.s, z2.s`.
const FUSED: &[&str] = &[
    "fmad", "fmla", "fmls", "fmsb", "fnmad", "fnmla", "fnmls", "fnmsb", "mad", "mla", "mls", "msb",
];

/// Each predicate of the lanes from the first while a comparison of two counters holds.
const WHILE: &[&str] =
    &["whilege", "whilegt", "whilehi", "whilehs", "whilele", "whilelo", "whilels", "whilelt"];

/// Each operation of one vector or predicate whole: `sunpklo z0.d, z0.s`.
const WHOLE: &[&str] = &[
    "aesimc", "aesmc", "fexpa", "frecpe", "frsqrte", "sqxtnb", "sqxtunb", "sunpkhi", "sunpklo",
    "uqxtnb", "uunpkhi", "uunpklo", "punpkhi", "punpklo", "rev",
];

/// Each narrowing into the odd lanes of the first operand: `sqxtnt z0.s, z1.d`.
const NARROW_TOP: &[&str] = &["sqxtnt", "sqxtunt", "uqxtnt"];

/// Each shift by an immediate of a vector whole: `sshllb z0.d, z0.s, #3`.
const SHIFT: &[&str] = &[
    "sshllb",
    "sshllt",
    "ushllb",
    "ushllt",
    "shrnb",
    "rshrnb",
    "sqshrnb",
    "sqrshrnb",
    "uqshrnb",
    "uqrshrnb",
    "sqshrunb",
    "sqrshrunb",
];

/// Each shift by an immediate into the first operand: `ssra z0.s, z1.s, #3`.
const SHIFT_INTO: &[&str] = &[
    "shrnt",
    "rshrnt",
    "sqshrnt",
    "sqrshrnt",
    "uqshrnt",
    "uqrshrnt",
    "sqshrunt",
    "sqrshrunt",
    "sri",
    "sli",
    "ssra",
    "usra",
    "srsra",
    "ursra",
];

/// Each shift by an immediate of the active lanes, the inactive ones of the first operand:
/// `asrd z0.s, p0/m, z0.s, #3`.
const SHIFT_MERGING: &[&str] = &["asrd", "srshr", "urshr", "sqshlu"];

/// Each reduction of the active lanes to a scalar of the elements' size, moved to `w0` or
/// `x0`: `smaxv s0, p0, z0.s; fmov w0, s0`.
const REDUCE: &[&str] = &["andv", "eorv", "orv", "smaxv", "sminv", "umaxv", "uminv"];

/// Each reduction of the active lanes to a float: `faddv s0, p0, z0.s`.
const REDUCE_FLOAT: &[&str] = &["faddv", "fmaxnmv", "fmaxv", "fminnmv", "fminv", "lasta", "lastb"];

/// The assembly of the stub that does what the LLVM intrinsic `name` (`aarch64.sve.cntd`,
/// without `llvm.`) does to operands `args`, giving `ret`. An error says what has no stub,
/// which the caller refuses with.
pub(super) fn stub(name: &str, args: &[Arg], ret: Ret) -> std::result::Result<String, String> {
    let mut text = begin();
    let (registers, _) = selected_into(&mut text, name, args, ret)?;
    write_end(&mut text, &registers, ret);
    Ok(text)
}

/// The registers of a stub's operands and the instructions LLVM selects for the intrinsic,
/// before the store of the result.
#[cfg(test)]
fn selected(
    name: &str,
    args: &[Arg],
    ret: Ret,
) -> std::result::Result<(Registers, String), String> {
    // alloc: test only (`#[cfg(test)]`)
    let mut text = String::new();
    let (registers, body) = selected_into(&mut text, name, args, ret)?;
    Ok((registers, text.split_off(body)))
}

/// Writes to `out` the moves and loads of a stub's operands and the instructions LLVM selects
/// for the intrinsic, before the store of the result. It gives the registers and where in
/// `out` the instructions begin.
fn selected_into(
    out: &mut String,
    name: &str,
    args: &[Arg],
    ret: Ret,
) -> std::result::Result<(Registers, usize), String> {
    // alloc: the error's message
    let unsupported = || Err(format!("LLVM intrinsic `{name}`"));
    // The parts of the name after `aarch64.sve.`; LLVM's own reversal of the lanes of a vector
    // or predicate is SVE's `rev`, and its insertion of one in another our `insert`.
    let mut parts: SmallVec<[&str; 8]> = SmallVec::new();
    if let Some(it) = name.strip_prefix("vector.reverse.") {
        parts.push("rev");
        parts.extend(it.split('.'));
    } else if let Some(it) = name.strip_prefix("vector.insert.") {
        parts.push("insert");
        parts.extend(it.split('.'));
    } else if let Some(it) = name.strip_prefix("aarch64.sve.") {
        parts.extend(it.split('.'));
    } else {
        return unsupported();
    }
    let registers = Registers::new(args, ret)?;
    // Each type the name is overloaded on is of an operand or the result.
    let shapes = || {
        args.iter()
            .filter_map(|it| match it {
                Arg::Vector(shape) => Some(*shape),
                _ => None,
            })
            .chain(match ret {
                Ret::Vector(shape) => Some(shape),
                _ => None,
            })
    };
    if parts.iter().any(|it| it.starts_with("nxv") && !shapes().any(|shape| shape.is_llvm(it))) {
        return unsupported();
    }
    let n = parts[0];
    let mut modifiers: SmallVec<[&str; 8]> = SmallVec::new();
    modifiers.extend(parts[1..].iter().copied().filter(|it| !is_type(it)));
    let letters = signature(args, ret);
    let signature = letters.as_str();
    registers.write_loads(args, out);
    let body_start = out.len();
    let o = |index: usize| registers.operands[index];
    let full = |index: usize| o(index).full();
    let bare = |index: usize| o(index).bare();
    let constant = |index: usize| match args[index] {
        Arg::Int(_, Some(value)) => value,
        _ => unreachable!("the signature has a constant there"),
    };
    let r = OrEmpty(registers.result.map(Reg::full));
    let shape_of = |index: usize| match args[index] {
        Arg::Vector(shape) => shape,
        _ => unreachable!("the signature has a vector there"),
    };
    let written: fmt::Result = match (n, &modifiers[..], signature) {
        // The elements of a vector, of a pattern of them: `cntb` to `cntd`. The count of a
        // whole vector of bytes is `rdvl x0, #1` in LLVM's code, the same number.
        // LLVM folds the count of a fixed number of elements that every vector length has.
        ("cntb" | "cnth" | "cntw" | "cntd", [], "C>I") if matches!(ret, Ret::Int(64)) => {
            let least = 16 >> "bhwd".find(&n[3..]).unwrap();
            match constant(0) {
                count @ 1..=8 if count <= least => write!(out, "mov w0, #{count}\n"),
                9 if least == 16 => out.write_str("mov w0, #16\n"),
                imm => write!(out, "{n} x0, {}\n", pattern(imm)?),
            }
        }
        // A predicate of the elements a pattern picks.
        ("ptrue", [], "C>P") => write!(out, "ptrue {r}, {}\n", pattern(constant(0))?),
        // The lanes of a predicate of wider elements as one of bytes: each lane's bit where
        // the bit of its first byte is, and the bits between them clear, which those of the
        // image of `nxv4i1` need not be.
        ("convert", ["to", "svbool"], "P>P") => {
            let Some(suffix) = predicate_suffix(parts[3]) else { return unsupported() };
            write!(out, "ptrue p1.{suffix}\nand p0.b, p1/z, p0.b, p0.b\n")
        }
        // A predicate of bytes read as one of wider elements: an instruction on those reads
        // the bit of each element's first byte, and those between are left as they are.
        ("convert", ["from", "svbool"], "P>P") => {
            if predicate_suffix(parts[3]).is_none() {
                return unsupported();
            }
            Ok(())
        }
        // The lanes up to the first that a write through one address and a read through the
        // other could overlap at.
        ("whilerw" | "whilewr", [size @ ("b" | "h" | "s" | "d")], "II>P")
            if predicate_suffix(parts[2]) == Some(*size) =>
        {
            write!(out, "{n} {r}, {}, {}\n", bare(0), bare(1))
        }
        // The active lanes of a predicate that are also of the governing one.
        ("cntp", [], "PP>I") if shape_of(0) == shape_of(1) && matches!(ret, Ret::Int(64)) => {
            write!(out, "cntp x0, {}, {}\n", bare(0), full(1))
        }
        // Each active lane read from memory, the others zero: of an unpacked vector, each in
        // the low bits of its element, which the load zero-extends into.
        ("ld1" | "ldnt1" | "ldff1" | "ldnf1" | "ld1rq" | "ld1ro", [], "PI>V") => {
            let Ret::Vector(data) = ret else { return unsupported() };
            let (Some(size), Some(suffix)) = (data.memory_size(), data.suffix()) else {
                return unsupported();
            };
            // LLVM has none of these of an unpacked vector.
            let whole = matches!(n, "ldnt1" | "ld1rq" | "ld1ro");
            if !lanes_of(shape_of(0), data) || (whole && size != suffix_size(suffix)) {
                return unsupported();
            }
            write!(out, "{n}{size} {{ {r} }}, {}/z, [{}]\n", bare(0), bare(1))
        }
        // The lanes of two, three or four vectors read from memory one after another.
        ("ld2" | "ld3" | "ld4", ["sret"], "PI>T") => {
            let (Ret::Vector(data), Some(result)) = (ret, registers.result) else {
                return unsupported();
            };
            let (Some(size), Some(suffix)) = (data.memory_size(), data.suffix()) else {
                return unsupported();
            };
            let vectors = u64::from(n.as_bytes()[2] - b'0');
            if data.vectors != vectors || size != suffix_size(suffix) {
                return unsupported();
            }
            write!(out, "{n}{size} {}, {}/z, [{}]\n", result.list(), bare(0), bare(1))
        }
        // Each active lane written to memory, of an unpacked vector the low bits of each element.
        ("st1" | "stnt1", [], "VPI>-") => {
            let data = shape_of(0);
            let (Some(size), Some(suffix)) = (data.memory_size(), data.suffix()) else {
                return unsupported();
            };
            if !lanes_of(shape_of(1), data) || (n == "stnt1" && size != suffix_size(suffix)) {
                return unsupported();
            }
            write!(out, "{n}{size} {}, {}, [{}]\n", o(0).list(), bare(1), bare(2))
        }
        // The lanes of two, three or four vectors written to memory one after another.
        ("st2" | "st3" | "st4", [], "VVPI>-" | "VVVPI>-" | "VVVVPI>-") => {
            let vectors = usize::from(n.as_bytes()[2] - b'0');
            let data = shape_of(0);
            let (Some(size), Some(suffix)) = (data.memory_size(), data.suffix()) else {
                return unsupported();
            };
            if args.len() != vectors + 2 || size != suffix_size(suffix) {
                return unsupported();
            }
            let tuple = Reg::Z(o(0).number(), Shape { vectors: vectors as u64, ..data });
            write!(out, "{n}{size} {}, {}, [{}]\n", tuple.list(), bare(vectors), bare(vectors + 1))
        }
        // A prefetch of the active lanes' memory.
        ("prf", [], "PIC>-") => {
            let Some(size) = shape_of(0).suffix().map(suffix_size) else { return unsupported() };
            write!(out, "prf{size} {}, {}, [{}]\n", prefetch(constant(2))?, bare(0), bare(1))
        }
        // Each active lane read from an address of its own, the others zero.
        ("ld1" | "ldff1" | "ldnt1", ["gather", how @ ..], "PIV>V" | "PVI>V") => {
            let Ret::Vector(data) = ret else { return unsupported() };
            let (base, offsets) = if signature == "PIV>V" { (1, 2) } else { (2, 1) };
            let Some(size) = data.memory_size() else { return unsupported() };
            let Some(address) = gathered(n == "ldnt1", how, size, bare(base), o(offsets)) else {
                return unsupported();
            };
            if !lanes_of(shape_of(0), data) || data.suffix() != shape_of(offsets).suffix() {
                return unsupported();
            }
            address.write_before(out);
            write!(out, "{n}{size} {{ {r} }}, {}/z, {address}\n", bare(0))
        }
        // Each active lane written to an address of its own.
        ("st1" | "stnt1", ["scatter", how @ ..], "VPIV>-" | "VPVI>-") => {
            let data = shape_of(0);
            let (base, offsets) = if signature == "VPIV>-" { (2, 3) } else { (3, 2) };
            let Some(size) = data.memory_size() else { return unsupported() };
            let Some(address) = gathered(n == "stnt1", how, size, bare(base), o(offsets)) else {
                return unsupported();
            };
            if !lanes_of(shape_of(1), data) || data.suffix() != shape_of(offsets).suffix() {
                return unsupported();
            }
            address.write_before(out);
            write!(out, "{n}{size} {}, {}, {address}\n", o(0).list(), bare(1))
        }
        // A prefetch of each active lane's address, the base plus each offset in elements.
        // LLVM writes those of bytes unshifted.
        ("prfb" | "prfh" | "prfw" | "prfd", ["gather", extend @ .., "index"], "PIVC>-") => {
            let offsets = shape_of(2);
            let extend = match (extend, offsets.suffix()) {
                ([], Some("d")) => "lsl",
                (["sxtw" | "uxtw"], Some("s")) => extend[0],
                _ => return unsupported(),
            };
            if !lanes_of(shape_of(0), offsets) {
                return unsupported();
            }
            let operation = prefetch(constant(3))?;
            write!(out, "{n} {operation}, {}, [{}, {}", bare(0), bare(1), full(2)).unwrap();
            match ("bhwd".find(&n[3..]).unwrap(), extend) {
                (0, "lsl") => {}
                (0, _) => write!(out, ", {extend}").unwrap(),
                (k, _) => write!(out, ", {extend} #{k}").unwrap(),
            }
            out.write_str("]\n")
        }
        // A prefetch of each active lane's address, a vector of them, at no offset.
        ("prfb" | "prfh" | "prfw" | "prfd", ["gather", "scalar", "offset"], "PVCC>-")
            if lanes_of(shape_of(0), shape_of(1)) && constant(2) == 0 =>
        {
            write!(out, "{n} {}, {}, [{}]\n", prefetch(constant(3))?, bare(0), full(1))
        }
        // The same at the offset of a scalar, which LLVM writes as a prefetch of bytes at
        // the scalar plus each address of the low 32 bits, of whatever size.
        ("prfb" | "prfh" | "prfw" | "prfd", ["gather", "scalar", "offset"], "PVIC>-")
            if lanes_of(shape_of(0), shape_of(1)) =>
        {
            let operation = prefetch(constant(3))?;
            write!(out, "prfb {operation}, {}, [{}, {}, uxtw]\n", bare(0), bare(2), full(1))
        }
        (_, [] | ["wide"], "PVV>V") if MERGING.contains(&n) => {
            write!(out, "{n} {r}, {}/m, {}, {}\n", bare(0), full(1), full(2))
        }
        // The pairs of the second operand's lanes added to the active lanes of the first.
        ("sadalp" | "uadalp", [], "PVV>V") => write!(out, "{n} {r}, {}/m, {}\n", bare(0), full(2)),
        // The last active lane of the second operand (or that after it) in each of the
        // first's, and the active lanes of the first followed by the second's.
        ("clasta" | "clastb" | "splice", [], "PVV>V") => {
            write!(out, "{n} {r}, {}, {}, {}\n", bare(0), full(1), full(2))
        }
        ("histcnt", [], "PVV>V") => {
            write!(out, "{n} {r}, {}/z, {}, {}\n", bare(0), full(1), full(2))
        }
        ("compact", [], "PV>V") => write!(out, "{n} {r}, {}, {}\n", bare(0), full(1)),
        (_, [] | ["x"], "VV>V" | "PP>P") if BINARY.contains(&n) => {
            write!(out, "{n} {r}, {}, {}\n", full(0), full(1))
        }
        // The products of polynomials, each twice as wide as its factors.
        ("pmullb" | "pmullt", ["pair"], "VV>V") => {
            let wide = match shape_of(0).suffix() {
                Some("b") => "h",
                Some("s") => "d",
                Some("d") => "q",
                _ => return unsupported(),
            };
            write!(out, "{n} {}, {}, {}\n", o(0).with(wide), full(0), full(1))
        }
        // Of predicates read as of wider elements, the size in the name.
        (
            "zip1" | "zip2" | "uzp1" | "uzp2" | "trn1" | "trn2" | "rev",
            [size @ ("b16" | "b32" | "b64")],
            "PP>P" | "P>P",
        ) => {
            let suffix = match *size {
                "b16" => "h",
                "b32" => "s",
                _ => "d",
            };
            write!(out, "{n} p0.{suffix}, ").unwrap();
            for it in 0..args.len() {
                if it > 0 {
                    out.push_str(", ");
                }
                write!(out, "{}", o(it).with(suffix)).unwrap();
            }
            out.write_str("\n")
        }
        // The same of the 128-bit pieces of the vectors.
        ("zip1q" | "zip2q" | "uzp1q" | "uzp2q" | "trn1q" | "trn2q", [], "VV>V") => {
            let q = |index: usize| o(index).with("q");
            write!(out, "{} {}, {}, {}\n", &n[..n.len() - 1], q(0), q(0), q(1))
        }
        ("tbl", [], "VV>V") => write!(out, "tbl {r}, {}, {}\n", o(0).list(), full(1)),
        ("tbl2", [], "VVV>V") => {
            let tuple = Reg::Z(o(0).number(), Shape { vectors: 2, ..shape_of(0) });
            write!(out, "tbl {r}, {}, {}\n", tuple.list(), full(2))
        }
        // Addresses of the first operand's plus the second's lanes times 1, 2, 4 or 8.
        ("adrb" | "adrh" | "adrw" | "adrd", [], "VV>V") => {
            let shift = match n {
                "adrb" => "",
                "adrh" => ", lsl #1",
                "adrw" => ", lsl #2",
                _ => ", lsl #3",
            };
            write!(out, "adr {r}, [{}, {}{shift}]\n", full(0), full(1))
        }
        (_, [] | ["x"], "VVV>V") if ACCUMULATING.contains(&n) => {
            write!(out, "{n} {r}, {}, {}\n", full(1), full(2))
        }
        (_, [], "VVV>V") if BITWISE3.contains(&n) => {
            let d = |index: usize| o(index).with("d");
            write!(out, "{n} {}, {}, {}, {}\n", d(0), d(0), d(1), d(2))
        }
        (_, [], "VPV>V") if UNARY.contains(&n) => {
            write!(out, "{n} {r}, {}/m, {}\n", bare(1), full(2))
        }
        (_, [] | ["wide"], "PVV>P") if COMPARE.contains(&n) => {
            let (head, n) = match n.strip_prefix("fcmp") {
                Some(it) => ("fcm", it),
                None => ("", n),
            };
            write!(out, "{head}{n} {r}, {}/z, {}, {}\n", bare(0), full(1), full(2))
        }
        (_, ["z"], "PPP>P") if PREDICATE.contains(&n) => {
            write!(out, "{n} {r}, {}/z, {}, {}\n", bare(0), full(1), full(2))
        }
        // Each lane up to the first active true one (`brka` with it), on the active lanes.
        ("brka" | "brkb", [], "PPP>P") => write!(out, "{n} {r}, {}/m, {}\n", bare(1), full(2)),
        ("brka" | "brkb", ["z"], "PP>P") => write!(out, "{n} {r}, {}/z, {}\n", bare(0), full(1)),
        ("brkn", ["z"], "PPP>P") => {
            write!(
                out,
                "brkn {}, {}/z, {}, {}\nmov p0.b, {}\n",
                full(2),
                bare(0),
                full(1),
                full(2),
                full(2)
            )
        }
        // The first active lane set, or the next after the last set one.
        ("pfirst" | "pnext", [], "PP>P") => {
            write!(out, "{n} {}, {}, {}\nmov p0.b, {}\n", full(1), bare(0), full(1), o(1).with("b"))
        }
        (_, [], "PVVV>V") if FUSED.contains(&n) => {
            write!(out, "{n} {r}, {}/m, {}, {}\n", bare(0), full(2), full(3))
        }
        (_, [], "II>P") if WHILE.contains(&n) => {
            write!(out, "{n} {r}, {}, {}\n", bare(0), bare(1))
        }
        (_, [] | ["x"], "V>V" | "P>P") if WHOLE.contains(&n) => {
            write!(out, "{n} {r}, {}\n", full(0))
        }
        (_, [], "VV>V") if NARROW_TOP.contains(&n) => write!(out, "{n} {r}, {}\n", full(1)),
        (_, [], "VC>V") if SHIFT.contains(&n) => {
            write!(out, "{n} {r}, {}, #{}\n", full(0), constant(1))
        }
        (_, [], "VVC>V") if SHIFT_INTO.contains(&n) => {
            write!(out, "{n} {r}, {}, #{}\n", full(1), constant(2))
        }
        (_, [], "PVC>V") if SHIFT_MERGING.contains(&n) => {
            write!(out, "{n} {r}, {}/m, {}, #{}\n", bare(0), full(1), constant(2))
        }
        // The lanes of each pair rotated and added, or exclusive-or rotated right.
        ("xar" | "cadd" | "sqcadd" | "ftmad", [] | ["x"], "VVC>V") => {
            write!(out, "{n} {r}, {}, {}, #{}\n", full(0), full(1), constant(2))
        }
        // The lanes of the two vectors from the given one of the first on, in bytes.
        ("ext", [], "VVC>V") => {
            let bytes = shape_of(0).container_bits() as i128 / 8 * constant(2);
            let b = |index: usize| o(index).with("b");
            write!(out, "ext {}, {}, {}, #{bytes}\n", b(0), b(0), b(1))
        }
        // An operation with a lane of each 128 bits of the second operand.
        (_, ["lane"], "VVC>V") => {
            write!(out, "{n} {r}, {}, {}[{}]\n", full(0), full(1), constant(2))
        }
        (_, ["lane"] | ["lane", "x"], "VVVC>V") => {
            write!(out, "{n} {r}, {}, {}[{}]\n", full(1), full(2), constant(3))
        }
        // An accumulation of complex products rotated by 0 to 270 degrees.
        (_, [] | ["x"], "VVVC>V") => {
            write!(out, "{n} {r}, {}, {}, #{}\n", full(1), full(2), constant(3))
        }
        (_, ["lane"] | ["lane", "x"], "VVVCC>V") => {
            write!(out, "{n} {r}, {}, {}[{}], #{}\n", full(1), full(2), constant(3), constant(4))
        }
        ("fcadd", [], "PVVC>V") => {
            write!(out, "fcadd {r}, {}/m, {}, {}, #{}\n", bare(0), full(1), full(2), constant(3))
        }
        ("fcmla", [], "PVVVC>V") => {
            write!(out, "fcmla {r}, {}/m, {}, {}, #{}\n", bare(0), full(2), full(3), constant(4))
        }
        // A reduction to an integer: of the sums, to 64 bits (and of signed lanes of 64 bits
        // the same as of unsigned ones), of the others, to the elements' size.
        ("saddv" | "uaddv", [], "PV>I") if matches!(ret, Ret::Int(64)) => {
            let n = if shape_of(1).container_bits() == 64 { "uaddv" } else { n };
            let d = scalar(o(1).number(), 64);
            write!(out, "{n} {d}, {}, {}\nfmov x0, {d}\n", bare(0), full(1))
        }
        (_, [], "PV>I") if REDUCE.contains(&n) => {
            let bits = shape_of(1).container_bits();
            let at = o(1).number();
            let (x, v) = if bits == 64 { ("x0", Scalar('d', at)) } else { ("w0", Scalar('s', at)) };
            write!(out, "{n} {}, {}, {}\nfmov {x}, {v}\n", scalar(at, bits), bare(0), full(1))
        }
        ("lasta" | "lastb", [], "PV>I") => write!(out, "{n} {r}, {}, {}\n", bare(0), full(1)),
        (_, [], "PV>F") if REDUCE_FLOAT.contains(&n) => {
            let Ret::Float(bits) = ret else { return unsupported() };
            write!(out, "{n} {}, {}, {}\n", scalar(o(1).number(), bits), bare(0), full(1))
        }
        // The last active lane, or that after it, else the scalar; a sum in order.
        ("clasta" | "clastb", ["n"], "PFV>F") | ("fadda", [], "PFV>F") => {
            write!(out, "{n} {}, {}, {}, {}\n", full(1), bare(0), full(1), full(2))
        }
        // Of an integer, the last active lane (or that after it) else the scalar: LLVM takes
        // bytes in a general register, wider lanes through a vector register.
        ("clasta" | "clastb", ["n"], "PIV>I") => {
            let bits = shape_of(2).container_bits();
            let lanes = full(2);
            if bits == 8 {
                write!(out, "{n} w0, {}, w0, {lanes}\n", bare(0))
            } else {
                let at = o(2).number() + 1;
                let (x, v) =
                    if bits == 64 { ("x0", Scalar('d', at)) } else { ("w0", Scalar('s', at)) };
                let lane = scalar(at, bits);
                write!(
                    out,
                    "fmov {v}, {x}\n{n} {lane}, {}, {lane}, {lanes}\nfmov {x}, {v}\n",
                    bare(0)
                )
            }
        }
        // A predicated broadcast of a scalar, and one of a scalar to every lane.
        ("dup", [], "VPI>V" | "VPF>V") => write!(out, "mov {r}, {}/m, {}\n", bare(1), full(2)),
        ("dup", ["x"], "I>V" | "F>V") => write!(out, "mov {r}, {}\n", full(0)),
        // A predicate of every lane or none, by the low bit of a `bool`.
        ("dup", ["x"], "I>P") => {
            let x = o(0).number();
            write!(out, "sbfx x8, x{x}, #0, #1\nwhilelo {r}, xzr, x8\n")
        }
        // Each 128 bits of the vector the given piece of it, by a table of the lanes of 64 bits.
        ("dupq", ["lane"], "VI>V") => {
            let index = o(1).number();
            write!(
                out,
                "index z1.d, #0, #1\nadd x8, x{index}, x{index}\nmov z2.d, x8\n\
                 and z1.d, z1.d, #0x1\norr z1.d, z1.d, z2.d\ntbl z0.d, {{ {} }}, z1.d\n",
                o(0).with("d")
            )
        }
        // LLVM's insertion of a vector of NEON's at the first lane (`svdupq_n_s32`'s): its lanes
        // in the first 128 bits, those after them of the scalable operand. At another lane it
        // is past the end of a vector of 128 bits.
        ("insert", [], "VNC>V") => {
            let (Arg::Fixed(fixed), Ret::Vector(data)) = (args[1], ret) else {
                return unsupported();
            };
            let same =
                |it: Shape| (it.bits, it.float, it.count) == (data.bits, data.float, data.count);
            let Some(suffix) = data.suffix() else { return unsupported() };
            if constant(2) != 0
                || data.vectors != 1
                || data.predicate()
                || shape_of(0) != data
                || !same(fixed)
                || !parts.get(2).is_some_and(|it| fixed.is_llvm_fixed(it))
            {
                return unsupported();
            }
            write!(out, "ptrue p0.{suffix}, vl{}\nmov {r}, p0/m, {}\n", data.count, full(1))
        }
        ("index", [], "II>V") => write!(out, "index {r}, {}, {}\n", bare(0), bare(1)),
        ("insr", [], "VI>V" | "VF>V") => write!(out, "insr {r}, {}\n", full(1)),
        // Whether any, the first or the last active lane is set.
        ("ptest", [test @ ("any" | "first" | "last")], "PP>I") => {
            let condition = match *test {
                "any" => "ne",
                "first" => "mi",
                _ => "lo",
            };
            write!(out, "ptest {}, {}\ncset w0, {condition}\n", bare(0), o(1).with("b"))
        }
        // The first-fault register: set, written and read.
        ("setffr", [], ">-") => out.write_str("setffr\n"),
        ("wrffr", [], "P>-") => write!(out, "wrffr {}\n", full(0)),
        ("rdffr", [], ">P") => write!(out, "rdffr {r}\n"),
        ("rdffr", ["z"], "P>P") => write!(out, "rdffr {r}, {}/z\n", bare(0)),
        // The scalar or each lane moved by the number of active lanes, saturating.
        // The scalar or each lane moved by a multiple of the count of a pattern's elements,
        // saturating: of a signed 32-bit scalar, to 64 bits and back.
        (_, [width], "ICC>I")
            if matches!(&n[..n.len() - 1], "sqinc" | "sqdec" | "uqinc" | "uqdec")
                && "bhwd".contains(&n[n.len() - 1..])
                && matches!(*width, "n32" | "n64") =>
        {
            let by = counted(constant(1), constant(2))?;
            match (n.as_bytes()[0], *width) {
                (b's', "n32") => write!(out, "{n} x0, w0, {by}\n"),
                (b'u', "n32") => write!(out, "{n} w0, {by}\n"),
                _ => write!(out, "{n} x0, {by}\n"),
            }
        }
        (_, [], "VCC>V")
            if matches!(&n[..n.len() - 1], "sqinc" | "sqdec" | "uqinc" | "uqdec")
                && "hwd".contains(&n[n.len() - 1..]) =>
        {
            write!(out, "{n} {r}, {}\n", counted(constant(1), constant(2))?)
        }
        ("sqincp" | "sqdecp" | "uqincp" | "uqdecp", [], "VP>V") => {
            write!(out, "{n} {r}, {}\n", full(1))
        }
        ("sqincp" | "sqdecp" | "uqincp" | "uqdecp", ["n64"], "IP>I") => {
            write!(out, "{n} x0, {}\n", full(1))
        }
        ("sqincp" | "sqdecp", ["n32"], "IP>I") => write!(out, "{n} x0, {}, w0\n", full(1)),
        ("uqincp" | "uqdecp", ["n32"], "IP>I") => write!(out, "{n} w0, {}\n", full(1)),
        _ => return unsupported(),
    };
    written.unwrap();
    Ok((registers, body_start))
}

/// The stub of `simd_select` on scalable vectors: each lane of `on_true` where the mask's is
/// set, else of `on_false`. LLVM's `select` of them is SVE's `sel`.
pub(super) fn select(mask: Shape, value: Shape) -> std::result::Result<String, String> {
    if mask.vectors != 1 || value.vectors != 1 || mask.count != value.count || !mask.predicate() {
        // alloc: the error's message
        return Err("`simd_select` of scalable vectors of other shapes".to_owned());
    }
    let ret = Ret::Vector(value);
    let args = [Arg::Vector(mask), Arg::Vector(value), Arg::Vector(value)];
    let registers = Registers::new(&args, ret)?;
    let suffix = match value.suffix() {
        _ if value.predicate() => None,
        Some(suffix) => Some(suffix),
        // alloc: the error's message
        None => return Err(format!("`simd_select` of {value:?}")),
    };
    let mut text = begin();
    registers.write_loads(&args, &mut text);
    match suffix {
        None => text.push_str("sel p0.b, p0, p1.b, p2.b\n"),
        Some(suffix) => write!(text, "sel z0.{suffix}, p0, z0.{suffix}, z1.{suffix}\n").unwrap(),
    }
    write_end(&mut text, &registers, ret);
    Ok(text)
}

/// The stub of `simd_cast` between scalable vectors of integers with as many lanes: LLVM's
/// `sext` or `zext` (by the signedness of the source) is SVE's `sxt*`/`uxt*` of each lane in its
/// container, and its `trunc` leaves the register as it is, the bits above a lane's being
/// none of the narrower type's. `None` when the image is the same.
pub(super) fn cast(from: Shape, to: Shape) -> std::result::Result<Option<String>, String> {
    let ints = |it: Shape| !it.float && !it.predicate() && it.vectors == 1;
    if !ints(from) || !ints(to) || from.count != to.count {
        // alloc: the error's message
        return Err(format!("`simd_cast` from {from:?} to {to:?}"));
    }
    if to.bits <= from.bits {
        return Ok(None);
    }
    // alloc: the error's message
    let Some(suffix) = to.suffix() else { return Err(format!("`simd_cast` to {to:?}")) };
    let width = match from.bits {
        8 => "b",
        16 => "h",
        32 => "w",
        // alloc: the error's message
        _ => return Err(format!("`simd_cast` from {from:?}")),
    };
    let extend = if from.signed { "sxt" } else { "uxt" };
    let ret = Ret::Vector(to);
    let args = [Arg::Vector(from)];
    let registers = Registers::new(&args, ret)?;
    let mut text = begin();
    registers.write_loads(&args, &mut text);
    write!(text, "ptrue p0.{suffix}\n{extend}{width} z0.{suffix}, p0/m, z0.{suffix}\n").unwrap();
    write_end(&mut text, &registers, ret);
    Ok(Some(text))
}

impl<'db> FunctionCx<'_, 'db> {
    /// The value of a constant operand of an integer or fieldless enum type, as the type's
    /// signedness extends it: `core::arch` gives an SVE intrinsic its immediates as such, as
    /// a const generic `svpattern`.
    fn sve_constant(&mut self, operand: &Operand, ty: Ty<'db>) -> Result<Option<i128>> {
        let layout = self.layout(ty)?;
        let BackendRepr::Scalar(scalar) = layout.backend_repr else { return Ok(None) };
        let size = layout.size;
        let signed = matches!(scalar.primitive(), rustc_abi::Primitive::Int(_, true));
        // The value of the first `size` bytes, when there are as many.
        let read = |bytes: &[u8]| {
            let len = size.bytes_usize();
            (bytes.len() >= len).then(|| {
                let mut all = [0; 16];
                all[..len].copy_from_slice(&bytes[..len]);
                u128::from_le_bytes(all)
            })
        };
        let bits = match ty.kind() {
            TyKind::Int(_) | TyKind::Uint(_) => self.immediate(operand)?,
            TyKind::Adt(..) => match &operand.kind {
                OperandKind::Constant { konst, .. } => match konst.as_ref().kind() {
                    ConstKind::Value(value) => {
                        let blob = self.valtree_blob(ty, value.value)?;
                        if blob.relocs.is_empty() && blob.fn_relocs.is_empty() {
                            read(&blob.bytes)
                        } else {
                            None
                        }
                    }
                    ConstKind::Alias(_, named) => read(&self.eval_const(named)?.memory),
                    _ => None,
                },
                OperandKind::Allocation { allocation } => read(&allocation.as_ref().memory),
                // A const generic argument is moved to a local of its own first.
                OperandKind::Copy(place) | OperandKind::Move(place)
                    if place.projection.as_slice().is_empty() =>
                {
                    let Some(body) = self.body else { return Ok(None) };
                    let mut assigned = body.basic_blocks.iter().flat_map(|(_, block)| {
                        block.statements.iter().filter_map(|it| match &it.kind {
                            StatementKind::Assign(to, rvalue) if to.local == place.local => {
                                Some((to, rvalue))
                            }
                            _ => None,
                        })
                    });
                    let (Some((to, Rvalue::Use(from))), None) = (assigned.next(), assigned.next())
                    else {
                        return Ok(None);
                    };
                    if to != place || self.operand_ty(from)? != ty {
                        return Ok(None);
                    }
                    return self.sve_constant(from, ty);
                }
                _ => None,
            },
            _ => None,
        };
        Ok(bits.map(|bits| {
            let shift = 128 - size.bits();
            if signed { ((bits << shift) as i128) >> shift } else { bits as i128 }
        }))
    }

    /// A call of the LLVM intrinsic `name` (`aarch64.sve.*`) as a call of its stub.
    pub(super) fn sve_intrinsic(
        &mut self,
        name: &str,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        let mut kinds: SmallVec<[Arg; 8]> = SmallVec::with_capacity(args.len());
        let mut values: SmallVec<[Value; 8]> = SmallVec::with_capacity(args.len() + 1);
        let mut params: SmallVec<[AbiParam; 8]> = SmallVec::with_capacity(args.len());
        // The stub takes the addresses of the scalable operands after the scalars.
        let mut addresses: SmallVec<[Value; 8]> = SmallVec::new();
        for arg in args {
            let ty = self.operand_ty(arg)?;
            if let Some(shape) = self.scalable(ty)? {
                let (addr, _) = self.operand(arg)?;
                kinds.push(Arg::Vector(shape));
                addresses.push(addr);
                continue;
            }
            let layout = self.layout(ty)?;
            if let Some(shape) = Shape::fixed(&layout) {
                let (addr, _) = self.operand(arg)?;
                kinds.push(Arg::Fixed(shape));
                addresses.push(addr);
                continue;
            }
            let BackendRepr::Scalar(_) = layout.backend_repr else {
                unsupported!("LLVM intrinsic `{name}` with an operand of {ty:?}");
            };
            let constant = self.sve_constant(arg, ty)?;
            let (value, scalar) = self.load_scalar(arg)?;
            let bits = u64::from(scalar.ty.bits());
            kinds.push(if scalar.ty.is_float() {
                Arg::Float(bits)
            } else {
                Arg::Int(bits, constant)
            });
            values.push(value);
            params.push(AbiParam::new(scalar.ty));
        }
        params.extend(addresses.iter().map(|_| AbiParam::new(ptr())));
        values.extend(addresses);
        let mut sig = self.cg.make_signature();
        let ret = if let Some(shape) = self.scalable(dst_ty)? {
            sig.params.push(AbiParam::new(ptr()));
            values.insert(0, dst);
            Ret::Vector(shape)
        } else if self.layout(dst_ty)?.is_zst() {
            Ret::Unit
        } else {
            let scalar = self.scalar_of(dst_ty)?;
            sig.returns.push(AbiParam::new(scalar.ty));
            let bits = u64::from(scalar.ty.bits());
            if scalar.ty.is_float() { Ret::Float(bits) } else { Ret::Int(bits) }
        };
        sig.params.extend(params);
        let text = stub(name, &kinds, ret).map_err(CodegenError::Unsupported)?;
        self.call_stub(text, &sig, &values, ret, dst);
        Ok(())
    }

    fn call_stub(&mut self, text: String, sig: &Signature, values: &[Value], ret: Ret, dst: Value) {
        let callee = self.cg.assembly(text, sig);
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        let call = self.builder.ins().call(callee, values);
        if let Ret::Int(_) | Ret::Float(_) = ret {
            let result = self.builder.inst_results(call)[0];
            self.builder.ins().store(MEM, result, dst, 0);
        }
    }

    /// The Rust intrinsics `core::arch`'s SVE functions use on scalable vectors. `false` when
    /// `intrinsic` is none of them ([`on_scalable`]) or has no scalable vector here.
    pub(super) fn scalable_intrinsic(
        &mut self,
        intrinsic: Intrinsic,
        generic_args: GenericArgs<'db>,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<bool> {
        let ours = on_scalable(intrinsic);
        let Some(to) = self.scalable(dst_ty)?.filter(|_| ours) else { return Ok(false) };
        let mut operands: SmallVec<[(Value, Option<Shape>); 4]> =
            SmallVec::with_capacity(args.len());
        for arg in args {
            let (addr, ty) = self.operand(arg)?;
            operands.push((addr, self.scalable(ty)?));
        }
        let vector = Size::from_bytes(DATA_MAX);
        match (intrinsic, &operands[..]) {
            (
                Intrinsic::SveTupleCreate2
                | Intrinsic::SveTupleCreate3
                | Intrinsic::SveTupleCreate4,
                _,
            ) => {
                if operands.len() as u64 != to.vectors
                    || operands
                        .iter()
                        .any(|(_, it)| it.is_none_or(|it| it.vectors != 1 || it.predicate()))
                {
                    unsupported!("`{intrinsic}` of {dst_ty:?}");
                }
                for (at, &(src, _)) in operands.iter().enumerate() {
                    let place = self.builder.ins().iadd_imm(dst, at as i64 * DATA_MAX as i64);
                    self.copy(place, src, vector);
                }
            }
            (Intrinsic::SveTupleGet | Intrinsic::SveTupleSet, _) => {
                let Some(index) = generic_args.iter().nth(2).and_then(|it| it.konst()) else {
                    unsupported!("`{intrinsic}` without its index");
                };
                let ConstKind::Value(index) = index.kind() else {
                    unsupported!("`{intrinsic}` with an index not evaluated");
                };
                let index = index.value.inner().to_leaf().to_bits(Size::from_bytes(4)) as i32;
                match (intrinsic, &operands[..]) {
                    (Intrinsic::SveTupleGet, [(tuple, Some(of))])
                        if (0..of.vectors as i32).contains(&index) && to.vectors == 1 =>
                    {
                        let place =
                            self.builder.ins().iadd_imm(*tuple, index as i64 * DATA_MAX as i64);
                        self.copy(dst, place, vector);
                    }
                    (Intrinsic::SveTupleSet, [(tuple, Some(of)), (value, Some(one))])
                        if (0..of.vectors as i32).contains(&index)
                            && one.vectors == 1
                            && *of == to =>
                    {
                        self.copy(dst, *tuple, Size::from_bytes(to.storage()));
                        let place =
                            self.builder.ins().iadd_imm(dst, index as i64 * DATA_MAX as i64);
                        self.copy(place, *value, vector);
                    }
                    _ => unsupported!("`{intrinsic}` of {dst_ty:?} at {index}"),
                }
            }
            (
                Intrinsic::Simd(SimdOp::Select),
                [(mask, Some(mask_shape)), (a, Some(value)), (b, Some(_))],
            ) => {
                let text = select(*mask_shape, *value).map_err(CodegenError::Unsupported)?;
                let mut sig = self.cg.make_signature();
                sig.params.extend([AbiParam::new(ptr()); 4]);
                self.call_stub(text, &sig, &[dst, *mask, *a, *b], Ret::Vector(to), dst);
            }
            (Intrinsic::Simd(SimdOp::Cast), [(src, Some(from))]) => {
                match cast(*from, to).map_err(CodegenError::Unsupported)? {
                    None => self.copy(dst, *src, Size::from_bytes(to.storage())),
                    Some(text) => {
                        let mut sig = self.cg.make_signature();
                        sig.params.extend([AbiParam::new(ptr()); 2]);
                        self.call_stub(text, &sig, &[dst, *src], Ret::Vector(to), dst);
                    }
                }
            }
            _ => unsupported!("intrinsic `{intrinsic}` on scalable vectors"),
        }
        Ok(true)
    }
}

/// Whether `intrinsic` is one [`FunctionCx::scalable_intrinsic`] lowers on scalable vectors:
/// the `sve_tuple_*` ones, `simd_select` and `simd_cast`.
fn on_scalable(intrinsic: Intrinsic) -> bool {
    matches!(
        intrinsic,
        Intrinsic::SveTupleCreate2
            | Intrinsic::SveTupleCreate3
            | Intrinsic::SveTupleCreate4
            | Intrinsic::SveTupleGet
            | Intrinsic::SveTupleSet
            | Intrinsic::Simd(SimdOp::Select | SimdOp::Cast)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const S32: Shape = Shape { bits: 32, float: false, signed: true, count: 4, vectors: 1 };
    const B4: Shape = Shape { bits: 1, float: false, signed: false, count: 4, vectors: 1 };
    const B16: Shape = Shape { bits: 1, float: false, signed: false, count: 16, vectors: 1 };

    #[test]
    fn counts_take_their_pattern() {
        let text = stub("aarch64.sve.cntd", &[Arg::Int(32, Some(31))], Ret::Int(64)).unwrap();
        assert!(text.contains("cntd x0, all\nret"), "{text}");
        // Every vector length has 16 bytes, which LLVM folds.
        let text = stub("aarch64.sve.cntb", &[Arg::Int(32, Some(9))], Ret::Int(64)).unwrap();
        assert!(text.contains("mov w0, #16\n"), "{text}");
        let text = stub("aarch64.sve.cntw", &[Arg::Int(32, Some(7))], Ret::Int(64)).unwrap();
        assert!(text.contains("cntw x0, vl7\n"), "{text}");
    }

    #[test]
    fn scalable_intrinsics_are_the_tuples_select_and_cast() {
        let named = |name: &str| {
            let intrinsic = Intrinsic::from_symbol(&intern::Symbol::intern(name));
            intrinsic.unwrap_or_else(|| panic!("{name}"))
        };
        for name in [
            "sve_tuple_create2",
            "sve_tuple_create3",
            "sve_tuple_create4",
            "sve_tuple_get",
            "sve_tuple_set",
            "simd_select",
            "simd_cast",
        ] {
            assert!(on_scalable(named(name)), "{name}");
        }
        for name in ["simd_as", "simd_select_bitmask", "simd_add", "transmute"] {
            assert!(!on_scalable(named(name)), "{name}");
        }
    }

    #[test]
    fn predicate_conversion_clears_the_bits_between_lanes() {
        let text =
            stub("aarch64.sve.convert.to.svbool.nxv4i1", &[Arg::Vector(B4)], Ret::Vector(B16))
                .unwrap();
        assert!(text.contains("ldr p0, [x1]\nptrue p1.s\nand p0.b"), "{text}");
        assert!(text.contains("str p0, [x0]\nret"), "{text}");
    }

    #[test]
    fn whilerw_takes_two_addresses() {
        let args = [Arg::Int(64, None), Arg::Int(64, None)];
        let text = stub("aarch64.sve.whilerw.s.nxv4i1.p0", &args, Ret::Vector(B4)).unwrap();
        assert!(text.contains(".arch_extension sve2\n"), "{text}");
        assert!(text.contains("whilerw p0.s, x1, x2\n"), "{text}");
        assert!(stub("aarch64.sve.whilerw.d.nxv4i1.p0", &args, Ret::Vector(B4)).is_err());
    }

    #[test]
    fn loads_and_stores_of_unpacked_vectors_take_the_lane_size() {
        let bytes_in_words = Shape { bits: 8, float: false, signed: true, count: 4, vectors: 1 };
        let ptr = Arg::Int(64, None);
        let text =
            stub("aarch64.sve.ld1.nxv4i8", &[Arg::Vector(B4), ptr], Ret::Vector(bytes_in_words))
                .unwrap();
        assert!(text.contains("ldr p0, [x2]\nld1b { z0.s }, p0/z, [x1]\nstr z0, [x0]\n"), "{text}");
        let args = [Arg::Vector(S32), Arg::Vector(B4), ptr];
        let text = stub("aarch64.sve.st1.nxv4i32", &args, Ret::Unit).unwrap();
        assert!(
            text.contains("ldr z0, [x1]\nldr p0, [x2]\nst1w { z0.s }, p0, [x0]\nret"),
            "{text}"
        );
        assert!(stub("aarch64.sve.st1.nxv4i8", &args, Ret::Unit).is_err());
    }

    #[test]
    fn insertion_of_a_neon_vector_loads_its_128_bits() {
        let args = [Arg::Vector(S32), Arg::Fixed(S32), Arg::Int(64, Some(0))];
        let name = "vector.insert.nxv4i32.v4i32";
        let text = stub(name, &args, Ret::Vector(S32)).unwrap();
        assert!(
            text.contains("ldr z0, [x2]\nldr q1, [x3]\nptrue p0.s, vl4\nmov z0.s, p0/m, z1.s\n"),
            "{text}"
        );
        let past = [Arg::Vector(S32), Arg::Fixed(S32), Arg::Int(64, Some(4))];
        assert!(stub(name, &past, Ret::Vector(S32)).is_err());
        let halves = Shape { bits: 16, count: 8, ..S32 };
        let other = [Arg::Vector(S32), Arg::Fixed(halves), Arg::Int(64, Some(0))];
        assert!(stub("vector.insert.nxv4i32.v8i16", &other, Ret::Vector(S32)).is_err());
    }

    #[test]
    fn select_of_words() {
        let text = select(B4, S32).unwrap();
        assert!(text.contains("ldr z0, [x2]\nldr z1, [x3]\nsel z0.s, p0, z0.s, z1.s\n"), "{text}");
    }

    /// A type of `sve-llvm.txt`: `nxv4i32`, `nxv4i32x2`, `nxv4i1`, `ptr`, `i32`, `f64`, `void`.
    fn fixture_type(ty: &str) -> Option<std::result::Result<Shape, (char, u64)>> {
        if let Some(rest) = ty.strip_prefix("nxv") {
            let (rest, vectors) = match rest.split_once('x') {
                Some((rest, vectors)) => (rest, vectors.parse().ok()?),
                None => (rest, 1),
            };
            let at = rest.find(['i', 'f'])?;
            let count = rest[..at].parse().ok()?;
            let bits = rest[at + 1..].parse().ok()?;
            let float = rest.as_bytes()[at] == b'f';
            return Some(Ok(Shape { bits, float, signed: !float, count, vectors }));
        }
        Some(Err(match ty {
            "ptr" => ('i', 64),
            "void" => ('-', 0),
            _ => (ty.chars().next()?, ty[1..].parse().ok()?),
        }))
    }

    /// The registers of a line of assembly in turn, each as its file (`x`, `v` or `p`) and
    /// number, and the line with each one's number taken out.
    fn registers_of(line: &str) -> (String, Vec<(char, u32)>) {
        let mut out = String::new();
        let mut found = Vec::new();
        let bytes = line.as_bytes();
        let mut at = 0;
        while at < bytes.len() {
            let start = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
            let digits = bytes[at + 1..].iter().take_while(|it| it.is_ascii_digit()).count();
            let end = at + 1 + digits;
            let whole = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
            let file = match bytes[at] {
                b'x' | b'w' => Some('x'),
                b'z' | b'v' | b'b' | b'h' | b's' | b'd' | b'q' => Some('v'),
                b'p' => Some('p'),
                _ => None,
            };
            if let (true, true, true, Some(file)) = (start, digits > 0, whole, file) {
                out.push(bytes[at] as char);
                out.push('#');
                found.push((file, line[at + 1..end].parse().unwrap()));
                at = end;
            } else {
                out.push(bytes[at] as char);
                at += 1;
            }
        }
        (out, found)
    }

    /// Each stub of an intrinsic of `core::arch`'s SVE is the code rustc's LLVM selects for it
    /// (`sve-llvm.txt`: each called as the functions of `core::arch` call it, its operands in
    /// registers by rustc's convention, as `rustc --emit asm` gave it), up to which registers
    /// it takes: those of each operand and the result where the stub has them, the others any
    /// that are free.
    #[test]
    fn stubs_are_what_llvm_selects() {
        let mut failed = Vec::new();
        for line in include_str!("sve-llvm.txt").lines() {
            let fields: Vec<&str> = line.split('\t').collect();
            let [name, operands, result, llvm] = fields[..] else { panic!("{line}") };
            let mut args = Vec::new();
            // Where rustc has each operand that is not a constant.
            let mut theirs = Vec::new();
            let (mut x, mut v, mut p) = (0, 0, 0);
            for operand in operands.split(',').filter(|it| !it.is_empty()) {
                let (ty, value) = match operand.split_once('=') {
                    Some((ty, value)) => (ty, Some(value.parse::<i128>().unwrap())),
                    None => (operand, None),
                };
                // A vector of NEON's, `v4f32`, in the next vector register.
                let fixed = ty.strip_prefix('v').and_then(|it| fixture_type(&format!("nxv{it}")));
                if let Some(Ok(shape)) = fixed {
                    theirs.push(vec![('v', v)]);
                    v += 1;
                    args.push(Arg::Fixed(shape));
                    continue;
                }
                match fixture_type(ty).unwrap() {
                    Ok(shape) if shape.predicate() => {
                        theirs.push(vec![('p', p)]);
                        p += 1;
                        args.push(Arg::Vector(shape));
                    }
                    Ok(shape) => {
                        theirs.push((0..shape.vectors as u32).map(|it| ('v', v + it)).collect());
                        v += shape.vectors as u32;
                        args.push(Arg::Vector(shape));
                    }
                    Err(('f', bits)) => {
                        theirs.push(vec![('v', v)]);
                        v += 1;
                        args.push(Arg::Float(bits));
                    }
                    Err((_, bits)) => {
                        theirs.push(if value.is_some() { vec![] } else { vec![('x', x)] });
                        x += u32::from(value.is_none());
                        args.push(Arg::Int(bits, value));
                    }
                }
            }
            let ret = match fixture_type(result).unwrap() {
                Ok(shape) => Ret::Vector(shape),
                Err(('-', _)) => Ret::Unit,
                Err(('f', bits)) => Ret::Float(bits),
                Err((_, bits)) => Ret::Int(bits),
            };
            let full = if name.starts_with("vector.") {
                name.to_owned()
            } else {
                format!("aarch64.sve.{name}")
            };
            let (registers, body) = match selected(&full, &args, ret) {
                Ok(it) => it,
                Err(e) => {
                    failed.push(format!("refused: {line}: {e}"));
                    continue;
                }
            };
            // The registers of the stub's operands and result for rustc's.
            let mut map = std::collections::HashMap::new();
            let mut back = std::collections::HashMap::new();
            let mut pair =
                |their: (char, u32), our: (char, u32)| match (map.get(&their), back.get(&our)) {
                    (None, None) => {
                        map.insert(their, our);
                        back.insert(our, their);
                        true
                    }
                    (Some(&a), Some(&b)) => a == our && b == their,
                    _ => false,
                };
            let file = |reg: Reg| match reg {
                Reg::X(..) => 'x',
                Reg::F(..) | Reg::Z(..) => 'v',
                Reg::P(..) => 'p',
            };
            let mut seeded = true;
            for (index, their) in theirs.iter().enumerate() {
                let reg = registers.operands[index];
                for (at, &their) in their.iter().enumerate() {
                    seeded &= pair(their, (file(reg), reg.number() + at as u32));
                }
            }
            if let Some(reg) = registers.result {
                seeded &= pair((file(reg), 0), (file(reg), 0));
            }
            let theirs: Vec<&str> = llvm.split("; ").filter(|it| !it.is_empty()).collect();
            let ours: Vec<&str> = body.lines().collect();
            let mut same = seeded && theirs.len() == ours.len();
            for (their, our) in theirs.iter().zip(&ours) {
                let (their_text, their_regs) = registers_of(their);
                let (our_text, our_regs) = registers_of(&our.replace('\t', " "));
                same &= their_text == our_text && their_regs.len() == our_regs.len();
                for (&a, &b) in their_regs.iter().zip(&our_regs) {
                    same &= pair(a, b);
                }
                // The callee keeps x19 to x29 and the low halves of v8 to v15, and the result's
                // address is read after the body.
                same &= our_regs.iter().all(|&(file, n)| match file {
                    'x' => n < 19 && (n != 0 || registers.address.is_none()),
                    'v' => !(8..16).contains(&n),
                    _ => true,
                });
            }
            if !same {
                failed.push(format!("differs: {line}\n    ours: {}", ours.join("; ")));
            }
        }
        assert!(failed.is_empty(), "{} of the stubs:\n{}", failed.len(), failed.join("\n"));
    }

    #[test]
    fn images_of_full_vectors_are_one() {
        let f64s = Shape { bits: 64, float: true, signed: true, count: 2, vectors: 1 };
        let i8s_unpacked = Shape { bits: 8, float: false, signed: true, count: 2, vectors: 1 };
        let i16s_unpacked = Shape { bits: 16, float: false, signed: true, count: 2, vectors: 1 };
        assert!(S32.same_image(f64s));
        assert!(!i8s_unpacked.same_image(i16s_unpacked));
        assert!(!B4.same_image(B16));
    }
}
