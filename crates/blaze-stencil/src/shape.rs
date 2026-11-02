//! The stencil set: which stencils exist, each named by a [`Shape`] (operation, controlling
//! type, auxiliary code, where each operand comes from, where the result goes). One
//! definition read by the generator (`build.rs` includes this file with `#[path]`), the
//! patcher's selection and the tests; it uses nothing but `std`.
//!
//! [`each`] lists the whole set in one deterministic order, [`each_on`] the part one
//! instruction set has; [`Shape::key`] packs a shape in 28 bits, which the library's perfect
//! index maps to its stencil.

use std::fmt;

/// Bumped with any change to what a shape means or which shapes exist: the library's header
/// carries it, and a library of another version is refused.
pub const VERSION: u32 = 4;

/// The registers the stencils' convention (`preserve_none`) passes `frame` and `acc` in, by
/// hardware number. The generator checks LLVM's choice against these (its probe stencil), and
/// the crate's tests that `arch::x64::X64::FRAME` and `arch::aarch64::Aarch64::FRAME` agree.
pub mod pinned {
    /// `r12` and `r13`.
    pub const X64: (u8, u8) = (12, 13);
    /// `x20` and `x21`.
    pub const AARCH64: (u8, u8) = (20, 21);
}

/// A value's type, as stencils see it: the types the IR builder makes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Ty {
    I8,
    I16,
    I32,
    I64,
    I128,
    F16,
    F32,
    F64,
    F128,
    I8x16,
    I16x8,
    I32x4,
    I64x2,
    F32x4,
    F64x2,
}

impl Ty {
    pub const ALL: [Ty; 15] = [
        Ty::I8,
        Ty::I16,
        Ty::I32,
        Ty::I64,
        Ty::I128,
        Ty::F16,
        Ty::F32,
        Ty::F64,
        Ty::F128,
        Ty::I8x16,
        Ty::I16x8,
        Ty::I32x4,
        Ty::I64x2,
        Ty::F32x4,
        Ty::F64x2,
    ];

    pub const fn from_u8(it: u8) -> Option<Ty> {
        if (it as usize) < Ty::ALL.len() { Some(Ty::ALL[it as usize]) } else { None }
    }

    /// Bytes of the value.
    pub const fn bytes(self) -> u8 {
        match self {
            Ty::I8 => 1,
            Ty::I16 | Ty::F16 => 2,
            Ty::I32 | Ty::F32 => 4,
            Ty::I64 | Ty::F64 => 8,
            _ => 16,
        }
    }

    /// Bytes of its slot: 8, or 16 for what is wider than 64 bits.
    pub const fn slot(self) -> u8 {
        if self.bytes() > 8 { 16 } else { 8 }
    }

    /// Which accumulator holds it, if any: the integer one for integers up to 64 bits, the
    /// float one for `f32`, `f64` and vectors. `i128`, `f16` and `f128` are never held.
    pub const fn acc(self) -> Option<Acc> {
        match self {
            Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64 => Some(Acc::Int),
            Ty::F32 | Ty::F64 => Some(Acc::Float),
            Ty::I8x16 | Ty::I16x8 | Ty::I32x4 | Ty::I64x2 | Ty::F32x4 | Ty::F64x2 => {
                Some(Acc::Float)
            }
            Ty::I128 | Ty::F16 | Ty::F128 => None,
        }
    }

    pub const fn is_int(self) -> bool {
        matches!(self, Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64 | Ty::I128)
    }

    /// An integer an immediate can be: up to 64 bits.
    pub const fn is_narrow_int(self) -> bool {
        matches!(self, Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64)
    }

    pub const fn is_float(self) -> bool {
        matches!(self, Ty::F16 | Ty::F32 | Ty::F64 | Ty::F128)
    }

    pub const fn is_vector(self) -> bool {
        matches!(self, Ty::I8x16 | Ty::I16x8 | Ty::I32x4 | Ty::I64x2 | Ty::F32x4 | Ty::F64x2)
    }

    /// The lane type of a vector, the type itself otherwise.
    pub const fn lane(self) -> Ty {
        match self {
            Ty::I8x16 => Ty::I8,
            Ty::I16x8 => Ty::I16,
            Ty::I32x4 => Ty::I32,
            Ty::I64x2 => Ty::I64,
            Ty::F32x4 => Ty::F32,
            Ty::F64x2 => Ty::F64,
            it => it,
        }
    }

    pub const fn lanes(self) -> u8 {
        if self.is_vector() { 16 / self.lane().bytes() } else { 1 }
    }

    /// The integer vector of the same lanes (a float vector's compare mask), or the type.
    pub const fn int_lanes(self) -> Ty {
        match self {
            Ty::F32x4 => Ty::I32x4,
            Ty::F64x2 => Ty::I64x2,
            it => it,
        }
    }

    /// The integer vector of lanes twice as wide and half as many, if any.
    pub const fn wider(self) -> Option<Ty> {
        match self {
            Ty::I8x16 => Some(Ty::I16x8),
            Ty::I16x8 => Some(Ty::I32x4),
            Ty::I32x4 => Some(Ty::I64x2),
            _ => None,
        }
    }

    /// The integer vector of lanes half as wide and twice as many, if any.
    pub const fn narrower(self) -> Option<Ty> {
        match self {
            Ty::I16x8 => Some(Ty::I8x16),
            Ty::I32x4 => Some(Ty::I16x8),
            Ty::I64x2 => Some(Ty::I32x4),
            _ => None,
        }
    }

    /// Cranelift's name of the type, for errors and the generator's symbol names.
    pub const fn name(self) -> &'static str {
        match self {
            Ty::I8 => "i8",
            Ty::I16 => "i16",
            Ty::I32 => "i32",
            Ty::I64 => "i64",
            Ty::I128 => "i128",
            Ty::F16 => "f16",
            Ty::F32 => "f32",
            Ty::F64 => "f64",
            Ty::F128 => "f128",
            Ty::I8x16 => "i8x16",
            Ty::I16x8 => "i16x8",
            Ty::I32x4 => "i32x4",
            Ty::I64x2 => "i64x2",
            Ty::F32x4 => "f32x4",
            Ty::F64x2 => "f64x2",
        }
    }
}

/// A set of [`Ty`], one bit each by its number.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct TySet(pub u16);

impl TySet {
    pub const NONE: TySet = TySet(0);
    pub const INTS: TySet = TySet::of(&[Ty::I8, Ty::I16, Ty::I32, Ty::I64]);
    pub const INTS_WIDE: TySet = TySet::of(&[Ty::I8, Ty::I16, Ty::I32, Ty::I64, Ty::I128]);
    pub const FLOATS: TySet = TySet::of(&[Ty::F32, Ty::F64]);
    pub const INT_VECTORS: TySet = TySet::of(&[Ty::I8x16, Ty::I16x8, Ty::I32x4, Ty::I64x2]);
    pub const FLOAT_VECTORS: TySet = TySet::of(&[Ty::F32x4, Ty::F64x2]);
    pub const VECTORS: TySet = TySet::INT_VECTORS.union(TySet::FLOAT_VECTORS);
    /// `f32`, `f64` and their vectors: what float arithmetic is of.
    pub const FLOAT_MATH: TySet = TySet::FLOATS.union(TySet::FLOAT_VECTORS);
    /// The integers, `i128` and the integer vectors: what integer arithmetic is of.
    pub const INT_MATH: TySet = TySet::INTS_WIDE.union(TySet::INT_VECTORS);
    /// What the bitwise ops take: every type but `f16` and `f128`.
    pub const BITS: TySet = TySet::INT_MATH.union(TySet::FLOAT_MATH);
    pub const ALL: TySet = TySet((1 << Ty::ALL.len()) - 1);

    pub const fn of(tys: &[Ty]) -> TySet {
        let mut bits = 0u16;
        let mut i = 0;
        while i < tys.len() {
            bits |= 1 << tys[i] as u16;
            i += 1;
        }
        TySet(bits)
    }

    pub const fn union(self, other: TySet) -> TySet {
        TySet(self.0 | other.0)
    }

    pub const fn contains(self, ty: Ty) -> bool {
        self.0 & (1 << ty as u16) != 0
    }

    pub fn iter(self) -> impl Iterator<Item = Ty> {
        Ty::ALL.into_iter().filter(move |it| self.contains(*it))
    }
}

/// The two accumulators: registers a result stays in for the next stencil
/// (stencil.md, "Shapes").
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Acc {
    Int,
    Float,
}

/// Where an operand comes from.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Src {
    /// Not an operand of this op (the operand count is the op's).
    None,
    Slot,
    /// The accumulator of its type: the previous stencil's result.
    Acc,
    /// A 32-bit immediate, sign-extended to the operand's type: an `Imm(n)` hole.
    Imm,
}

impl Src {
    pub const ALL: [Src; 4] = [Src::None, Src::Slot, Src::Acc, Src::Imm];

    pub const fn from_u8(it: u8) -> Option<Src> {
        match it {
            0 => Some(Src::None),
            1 => Some(Src::Slot),
            2 => Some(Src::Acc),
            3 => Some(Src::Imm),
            _ => None,
        }
    }
}

/// Where result 0 goes; any other result always goes to its slot.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Dst {
    /// No result (stores, branches, traps, fences).
    None,
    Slot,
    /// Only the accumulator: its one use is the next stencil.
    Acc,
    /// The accumulator and its slot: the next stencil is one of several uses.
    Both,
}

impl Dst {
    pub const ALL: [Dst; 4] = [Dst::None, Dst::Slot, Dst::Acc, Dst::Both];

    pub const fn from_u8(it: u8) -> Option<Dst> {
        match it {
            0 => Some(Dst::None),
            1 => Some(Dst::Slot),
            2 => Some(Dst::Acc),
            3 => Some(Dst::Both),
            _ => None,
        }
    }
}

/// An integer condition, in the order of Cranelift's `IntCC`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum IntCc {
    Equal,
    NotEqual,
    SignedLessThan,
    SignedGreaterThanOrEqual,
    SignedGreaterThan,
    SignedLessThanOrEqual,
    UnsignedLessThan,
    UnsignedGreaterThanOrEqual,
    UnsignedGreaterThan,
    UnsignedLessThanOrEqual,
}

impl IntCc {
    pub const COUNT: u8 = 10;
    pub const ALL: [IntCc; 10] = [
        IntCc::Equal,
        IntCc::NotEqual,
        IntCc::SignedLessThan,
        IntCc::SignedGreaterThanOrEqual,
        IntCc::SignedGreaterThan,
        IntCc::SignedLessThanOrEqual,
        IntCc::UnsignedLessThan,
        IntCc::UnsignedGreaterThanOrEqual,
        IntCc::UnsignedGreaterThan,
        IntCc::UnsignedLessThanOrEqual,
    ];

    pub const fn from_u8(it: u8) -> Option<IntCc> {
        if it < IntCc::COUNT { Some(IntCc::ALL[it as usize]) } else { None }
    }

    /// Whether it compares as signed integers.
    pub const fn signed(self) -> bool {
        matches!(
            self,
            IntCc::SignedLessThan
                | IntCc::SignedGreaterThanOrEqual
                | IntCc::SignedGreaterThan
                | IntCc::SignedLessThanOrEqual
        )
    }
}

/// A float condition, in the order of Cranelift's `FloatCC`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum FloatCc {
    Ordered,
    Unordered,
    Equal,
    NotEqual,
    OrderedNotEqual,
    UnorderedOrEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    UnorderedOrLessThan,
    UnorderedOrLessThanOrEqual,
    UnorderedOrGreaterThan,
    UnorderedOrGreaterThanOrEqual,
}

impl FloatCc {
    pub const COUNT: u8 = 14;
    pub const ALL: [FloatCc; 14] = [
        FloatCc::Ordered,
        FloatCc::Unordered,
        FloatCc::Equal,
        FloatCc::NotEqual,
        FloatCc::OrderedNotEqual,
        FloatCc::UnorderedOrEqual,
        FloatCc::LessThan,
        FloatCc::LessThanOrEqual,
        FloatCc::GreaterThan,
        FloatCc::GreaterThanOrEqual,
        FloatCc::UnorderedOrLessThan,
        FloatCc::UnorderedOrLessThanOrEqual,
        FloatCc::UnorderedOrGreaterThan,
        FloatCc::UnorderedOrGreaterThanOrEqual,
    ];

    pub const fn from_u8(it: u8) -> Option<FloatCc> {
        if it < FloatCc::COUNT { Some(FloatCc::ALL[it as usize]) } else { None }
    }
}

/// An atomic read-modify-write, in the order of Cranelift's `AtomicRmwOp`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum RmwOp {
    Add,
    Sub,
    And,
    Nand,
    Or,
    Xor,
    Xchg,
    Umin,
    Umax,
    Smin,
    Smax,
}

impl RmwOp {
    pub const COUNT: u8 = 11;
    pub const ALL: [RmwOp; 11] = [
        RmwOp::Add,
        RmwOp::Sub,
        RmwOp::And,
        RmwOp::Nand,
        RmwOp::Or,
        RmwOp::Xor,
        RmwOp::Xchg,
        RmwOp::Umin,
        RmwOp::Umax,
        RmwOp::Smin,
        RmwOp::Smax,
    ];

    pub const fn from_u8(it: u8) -> Option<RmwOp> {
        if it < RmwOp::COUNT { Some(RmwOp::ALL[it as usize]) } else { None }
    }
}

/// How a confined access ([`Op::LoadConfined`], [`Op::StoreConfined`]) adds its offset to
/// the 32-bit address: `aux` bits 5..7.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Disp {
    /// The offset is 0: no add.
    Zero,
    /// An `add` of the `Offset` hole's low 12 bits, for an offset of 1 to 4095: AArch64's
    /// `add #:lo12:`.
    Lo12,
    /// An `add` of all 32 bits of the `Offset` hole.
    Imm32,
}

impl Disp {
    pub const fn from_u8(it: u8) -> Option<Disp> {
        match it {
            0 => Some(Disp::Zero),
            1 => Some(Disp::Lo12),
            2 => Some(Disp::Imm32),
            _ => None,
        }
    }

    /// Whether `isa`'s library has it: x86-64's `lea` takes any 32-bit offset as one
    /// instruction, so it has no `Lo12`.
    pub const fn on(self, isa: Isa) -> bool {
        !matches!((self, isa), (Disp::Lo12, Isa::X86_64))
    }
}

/// The loads [`Op::LoadConfined`] fuses, by their number in its `aux`'s low 5 bits.
pub const CONFINED_LOADS: [Op; 14] = [
    Op::Load,
    Op::Uload8,
    Op::Sload8,
    Op::Uload16,
    Op::Sload16,
    Op::Uload32,
    Op::Sload32,
    Op::Uload8x8,
    Op::Sload8x8,
    Op::Uload16x4,
    Op::Sload16x4,
    Op::Uload32x2,
    Op::Sload32x2,
    Op::AtomicLoad,
];

/// The stores [`Op::StoreConfined`] fuses, likewise.
pub const CONFINED_STORES: [Op; 5] =
    [Op::Store, Op::Istore8, Op::Istore16, Op::Istore32, Op::AtomicStore];

/// The instruction set a library is of: which part of the set has stencils
/// ([`Op::on`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Isa {
    X86_64,
    Aarch64,
}

impl Isa {
    pub const ALL: [Isa; 2] = [Isa::X86_64, Isa::Aarch64];
}

/// The operation of a stencil: Cranelift's opcodes that a stencil implements, with the
/// `_imm` forms folded into their base op (an [`Src::Imm`] operand), `select_spectre_guard`
/// into [`Op::Select`], `stack_addr` kept as a frame-relative op, and the fusions
/// [`Op::BrIcmp`], [`Op::SelectIcmp`], [`Op::LoadConfined`] and [`Op::StoreConfined`]. Opcodes
/// not here are the glue's (`arch::Glue`) or refused with an error naming them
/// (`patch/select.rs`).
///
/// Operand types are [`Shape::arg`]'s and result types [`Shape::result`]'s. The amount of a
/// shift or rotate (operand 1) is read as its low 8 bits, so any integer type of amount takes
/// the same stencil (its type is given as `i8`): Cranelift masks it below the width anyway.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Op {
    // Moves and constants.
    /// A slot to a slot: block arguments on edges.
    Copy,
    /// `iconst` into a slot or accumulator (one no shape takes as an [`Src::Imm`]): `aux` 0
    /// when the value fits 32 bits sign-extended, 1 for an `Imm0` hole of all 64 (`i64` only).
    Iconst,
    /// `f16const`, `f32const`, `f64const`: the bits as `Imm0`, `aux` as for [`Op::Iconst`].
    Fconst,
    /// `vconst`, `f128const`: the 16 bytes in the function's constant area (`Const`).
    Vconst,
    // Integer arithmetic.
    Iadd,
    Isub,
    Ineg,
    Iabs,
    Imul,
    Umulhi,
    Smulhi,
    Udiv,
    Sdiv,
    Urem,
    Srem,
    SqmulRoundSat,
    AvgRound,
    UaddSat,
    SaddSat,
    UsubSat,
    SsubSat,
    Smin,
    Umin,
    Smax,
    Umax,
    // Arithmetic with an overflow or carry result (result 1, an `i8` in its slot).
    UaddOverflow,
    SaddOverflow,
    UsubOverflow,
    SsubOverflow,
    UmulOverflow,
    SmulOverflow,
    /// Operand 2 is the `i8` carry in, from its slot.
    UaddOverflowCin,
    SaddOverflowCin,
    UsubOverflowBin,
    SsubOverflowBin,
    /// Adds, trapping on unsigned overflow (`Trap`).
    UaddOverflowTrap,
    // Bits.
    Band,
    Bor,
    Bxor,
    Bnot,
    Rotl,
    Rotr,
    Ishl,
    Ushr,
    Sshr,
    Bitrev,
    Clz,
    Cls,
    Ctz,
    Bswap,
    Popcnt,
    Bitselect,
    /// `aux` is the operand's [`Ty`]; `ty` the result's.
    Bmask,
    // Comparisons and selects.
    /// `aux` is the [`IntCc`].
    Icmp,
    /// `aux` is the [`FloatCc`].
    Fcmp,
    /// `select c, x, y`: `aux` is the condition's [`Ty`].
    Select,
    /// `select (icmp cc a b), a, b` (the `min`/`max` idiom of any condition): operands `a`
    /// and `b`; `aux` is the [`IntCc`] in its low 4 bits and `ty` (the compared type) in its
    /// high 4.
    SelectIcmp,
    // Floats.
    Fadd,
    Fsub,
    Fmul,
    Fdiv,
    Sqrt,
    /// `aux` 1 is AArch64's `fmsub`/`fmls`, `z - x * y` with `x`'s sign flipped before the
    /// multiply, which Cranelift lowers `fma x (fneg y) z` to: operands `[x, y, z]`
    /// ([`Aux::Negated`]).
    Fma,
    Fneg,
    Fabs,
    Fcopysign,
    Fmin,
    Fmax,
    Ceil,
    Floor,
    Trunc,
    Nearest,
    // Conversions: `aux` is the source [`Ty`] where it can be more than one.
    Bitcast,
    Ireduce,
    Uextend,
    Sextend,
    Fpromote,
    Fdemote,
    Fvdemote,
    FvpromoteLow,
    FcvtToUint,
    FcvtToSint,
    FcvtToUintSat,
    FcvtToSintSat,
    FcvtFromUint,
    FcvtFromSint,
    /// `i128` to its two `i64` halves (results 0 and 1).
    Isplit,
    /// Two `i64` halves to an `i128` (`ty` is `i128`).
    Iconcat,
    /// `ty` is the result; the operands are the vectors of lanes twice as wide.
    Snarrow,
    Unarrow,
    Uunarrow,
    /// `ty` is the result; the operand is the vector of lanes half as wide.
    SwidenLow,
    SwidenHigh,
    UwidenLow,
    UwidenHigh,
    IaddPairwise,
    /// `aux` is the scalar's [`Ty`]; `ty` the vector's.
    ScalarToVector,
    // Vectors.
    Splat,
    Swizzle,
    /// The 16-byte mask in the function's constant area (`Const`).
    Shuffle,
    /// `aux` is the lane.
    Insertlane,
    /// `aux` is the lane; `ty` the vector's.
    Extractlane,
    VanyTrue,
    VallTrue,
    /// `ty` is the vector's; `aux` is the result's [`Ty`].
    VhighBits,
    Blendv,
    X86Pshufb,
    X86Pmulhrsw,
    /// `ty` is the result `i16x8`; the operands are `i8x16`.
    X86Pmaddubsw,
    /// `ty` is the result `i32x4`; the operand is `f32x4`.
    X86Cvtt2dq,
    // Memory: the access's offset is the `Offset` hole. `ty` is the value's type; for the
    // narrow loads and stores, the wide type (`uload8.i32`: `ty` is `i32`). The address is
    // an `i64` operand; a store's value is operand 0 and its address operand 1, as in
    // Cranelift.
    Load,
    Store,
    Uload8,
    Sload8,
    Istore8,
    Uload16,
    Sload16,
    Istore16,
    Uload32,
    Sload32,
    Istore32,
    Uload8x8,
    Sload8x8,
    Uload16x4,
    Sload16x4,
    Uload32x2,
    Sload32x2,
    /// The frame register plus `Offset` (the stack slot's place plus the offset).
    StackAddr,
    /// Atomics are sequentially consistent and have no offset; operands in Cranelift's
    /// order (`atomic_store x, p`; `atomic_rmw p, x`; `atomic_cas p, e, x`).
    AtomicLoad,
    AtomicStore,
    /// `aux` is the [`RmwOp`].
    AtomicRmw,
    AtomicCas,
    Fence,
    /// A load at `iadd vmctx, uextend.i64 (ireduce.i32 (iadd_imm p, off))` with offset 0, the
    /// address of the macro tier's confinement: operands `[p, vmctx]`. `aux` is the load's
    /// number in [`CONFINED_LOADS`] in its low 5 bits and the [`Disp`] of `off` (the `Offset`
    /// hole) above; `ty` is the plain load's.
    LoadConfined,
    /// A store at the same address: operands `[x, p, vmctx]`, `aux` from
    /// [`CONFINED_STORES`].
    StoreConfined,
    // Control.
    /// To `Next`.
    Jump,
    /// To `Target` when the condition (of type `ty`) is not zero, else to `Next`.
    Brif,
    /// `brif (icmp cc a b)`: `aux` is the [`IntCc`]; `ty` the compared type.
    BrIcmp,
    /// To `Trap` when the condition is zero.
    Trapz,
    /// To `Trap` when the condition is not zero.
    Trapnz,
}

impl Op {
    /// The number of ops: every `Op as u8` is below it.
    pub const COUNT: u8 = Op::Trapnz as u8 + 1;

    pub fn from_u8(it: u8) -> Option<Op> {
        INFO.get(usize::from(it)).map(|info| info.op)
    }

    /// Every op, in the order of their numbers.
    pub fn all() -> impl Iterator<Item = Op> {
        INFO.iter().map(|info| info.op)
    }

    /// What the op takes and gives, and which shapes of it exist.
    pub fn info(self) -> &'static Info {
        &INFO[self as usize]
    }

    /// Whether `isa`'s library has stencils of it. On x86-64's baseline (SSE2), Cranelift
    /// lowers `ceil`, `floor`, `trunc`, `nearest` and `fma` to libcalls, and so does the glue.
    /// `blendv` (SSE4.1), `x86_pshufb`, `x86_pmulhrsw` and `x86_pmaddubsw` (SSSE3) have no
    /// lowering on either baseline, so no IR Cranelift compiles has them; `x86_cvtt2dq` is
    /// x86-64's alone. Without SSSE3 Cranelift does `swizzle` and the general `shuffle` with
    /// the `X86Pshufb` libcall, and a byte shuffle by a mask known only when patched has no
    /// stackless SSE2 form, so on x86-64 both are the glue's.
    pub const fn on(self, isa: Isa) -> bool {
        if matches!(self, Op::Blendv | Op::X86Pshufb | Op::X86Pmulhrsw | Op::X86Pmaddubsw) {
            return false;
        }
        match isa {
            Isa::X86_64 => !matches!(
                self,
                Op::Ceil | Op::Floor | Op::Trunc | Op::Nearest | Op::Fma | Op::Swizzle | Op::Shuffle
            ),
            Isa::Aarch64 => !matches!(self, Op::X86Cvtt2dq),
        }
    }
}

/// What `aux` means for an op, and so which values of it exist.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Aux {
    /// Always 0.
    None,
    /// An [`IntCc`].
    IntCc,
    /// A [`FloatCc`].
    FloatCc,
    /// An [`IntCc`] and the compared integer [`Ty`] ([`Op::SelectIcmp`]).
    IntCcTy,
    /// A lane of `ty`: below `ty.lanes()`.
    Lane,
    /// A source or condition [`Ty`], from the set given.
    Ty(TySet),
    /// An [`RmwOp`].
    Rmw,
    /// 0 for a 32-bit immediate sign-extended, 1 for one of 64 bits (64-bit types only).
    Wide,
    /// A confined access and its [`Disp`] ([`Op::LoadConfined`], [`Op::StoreConfined`]).
    Confined,
    /// 0, or 1 for the form with an operand's sign flipped ([`Op::Fma`]).
    Negated,
}

/// What an op takes and gives, and which shapes of it exist.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Info {
    pub op: Op,
    /// Cranelift's name of the opcode it implements (the base opcode for the `_imm` forms).
    pub name: &'static str,
    /// Value operands, at most 3.
    pub args: u8,
    /// Results, at most 2.
    pub results: u8,
    /// The controlling types it has a stencil for.
    pub tys: TySet,
    pub aux: Aux,
    /// Bit `n`: operand `n` may be [`Src::Imm`] (where its type is an integer up to 64 bits).
    pub imm: u8,
    /// Bit `n`: operand `n` may be [`Src::Acc`] (where its type has an accumulator).
    pub acc: u8,
    /// Whether result 0 may go to an accumulator ([`Dst::Acc`], [`Dst::Both`]).
    pub dst_acc: bool,
    /// Whether result 0 may go to the accumulator and its slot at once ([`Dst::Both`]): not
    /// for compares (their one use is mostly the next stencil) nor lane moves, and never for
    /// a vector, which keeps the set near 7,000 stencils.
    pub both: bool,
    /// Whether it has a `Trap` hole.
    pub traps: bool,
    /// Whether it has a `Target` hole (a branch).
    pub branches: bool,
}

impl Info {
    /// An op of `args` operands and `results` results of the types `tys`, every operand
    /// from a slot or an accumulator, result 0 to a slot or an accumulator.
    const fn new(op: Op, name: &'static str, args: u8, results: u8, tys: TySet) -> Info {
        Info {
            op,
            name,
            args,
            results,
            tys,
            aux: Aux::None,
            imm: 0,
            acc: (1 << args) - 1,
            dst_acc: results > 0,
            both: results > 0,
            traps: false,
            branches: false,
        }
    }

    /// Operands and result in slots or immediates only: ops too rare to cache.
    const fn no_acc(self) -> Info {
        Info { acc: 0, dst_acc: false, both: false, ..self }
    }

    const fn no_both(self) -> Info {
        Info { both: false, ..self }
    }

    const fn aux(self, aux: Aux) -> Info {
        Info { aux, ..self }
    }

    const fn imm(self, imm: u8) -> Info {
        Info { imm, ..self }
    }

    const fn acc(self, acc: u8) -> Info {
        Info { acc, ..self }
    }

    const fn traps(self) -> Info {
        Info { traps: true, ..self }
    }

    const fn branches(self) -> Info {
        Info { branches: true, ..self }
    }
}

const fn unary(op: Op, name: &'static str, tys: TySet) -> Info {
    Info::new(op, name, 1, 1, tys)
}

const fn binary(op: Op, name: &'static str, tys: TySet) -> Info {
    Info::new(op, name, 2, 1, tys)
}

/// A binary op whose right operand may be an immediate (its `_imm` form).
const fn binary_imm(op: Op, name: &'static str, tys: TySet) -> Info {
    binary(op, name, tys).imm(0b10)
}

const fn conversion(op: Op, name: &'static str, tys: TySet, from: TySet) -> Info {
    unary(op, name, tys).aux(Aux::Ty(from))
}

const fn load(op: Op, name: &'static str, tys: TySet) -> Info {
    unary(op, name, tys)
}

const fn store(op: Op, name: &'static str, tys: TySet) -> Info {
    Info::new(op, name, 2, 0, tys).imm(0b01)
}

const fn ty(ty: Ty) -> TySet {
    TySet::of(&[ty])
}

const I8: TySet = ty(Ty::I8);
const I16_UP: TySet = TySet::of(&[Ty::I16, Ty::I32, Ty::I64]);
const I32_UP: TySet = TySet::of(&[Ty::I32, Ty::I64]);
const I16_TO_I128: TySet = TySet::of(&[Ty::I16, Ty::I32, Ty::I64, Ty::I128]);
const NARROWED: TySet = TySet::of(&[Ty::I8x16, Ty::I16x8, Ty::I32x4]);
const WIDENED: TySet = TySet::of(&[Ty::I16x8, Ty::I32x4, Ty::I64x2]);
const SAT_LANES: TySet = TySet::INT_VECTORS;
const MUL_TYS: TySet = TySet::INTS_WIDE.union(TySet::INT_VECTORS);
// Neither backend has a vector rule (x64's `pmulhw` and AArch64's `smull2` lower widening
// multiplies, not `umulhi`/`smulhi`).
const MULHI_TYS: TySet = TySet::INTS;
const FCVT_TO: TySet = TySet::INTS.union(TySet::of(&[Ty::I32x4, Ty::I64x2]));
const FCVT_FROM: TySet = TySet::INTS.union(TySet::of(&[Ty::I32x4, Ty::I64x2]));
const FLOAT_SOURCES: TySet = TySet::FLOAT_MATH;
const BLEND_TYS: TySet = TySet::of(&[Ty::I8x16, Ty::I32x4, Ty::I64x2, Ty::F32x4, Ty::F64x2]);

/// One [`Info`] per op, by its number: each row from Cranelift's lowering rules of the opcode
/// on x64 and AArch64 (which types it takes) and the IR builder's use of it.
const TABLE: [Info; Op::Trapnz as usize + 1] = [
    // Only edges' block-argument moves are copies, slot to slot: no accumulator lives across
    // a branch.
    unary(Op::Copy, "copy", TySet::ALL).no_acc(),
    Info::new(Op::Iconst, "iconst", 0, 1, TySet::INTS).aux(Aux::Wide),
    Info::new(Op::Fconst, "fconst", 0, 1, TySet::of(&[Ty::F16, Ty::F32, Ty::F64])).aux(Aux::Wide),
    Info::new(Op::Vconst, "vconst", 0, 1, TySet::VECTORS.union(ty(Ty::F128))),
    binary_imm(Op::Iadd, "iadd", TySet::INT_MATH),
    binary_imm(Op::Isub, "isub", TySet::INT_MATH),
    unary(Op::Ineg, "ineg", TySet::INT_MATH),
    unary(Op::Iabs, "iabs", TySet::INT_MATH),
    binary_imm(Op::Imul, "imul", MUL_TYS),
    binary_imm(Op::Umulhi, "umulhi", MULHI_TYS),
    binary_imm(Op::Smulhi, "smulhi", MULHI_TYS),
    binary_imm(Op::Udiv, "udiv", TySet::INTS).traps(),
    binary_imm(Op::Sdiv, "sdiv", TySet::INTS).traps(),
    binary_imm(Op::Urem, "urem", TySet::INTS).traps(),
    binary_imm(Op::Srem, "srem", TySet::INTS).traps(),
    binary(Op::SqmulRoundSat, "sqmul_round_sat", TySet::of(&[Ty::I16x8, Ty::I32x4])),
    binary(Op::AvgRound, "avg_round", TySet::INT_VECTORS),
    binary(Op::UaddSat, "uadd_sat", SAT_LANES),
    binary(Op::SaddSat, "sadd_sat", SAT_LANES),
    binary(Op::UsubSat, "usub_sat", SAT_LANES),
    binary(Op::SsubSat, "ssub_sat", SAT_LANES),
    binary_imm(Op::Smin, "smin", TySet::INTS.union(TySet::INT_VECTORS)),
    binary_imm(Op::Umin, "umin", TySet::INTS.union(TySet::INT_VECTORS)),
    binary_imm(Op::Smax, "smax", TySet::INTS.union(TySet::INT_VECTORS)),
    binary_imm(Op::Umax, "umax", TySet::INTS.union(TySet::INT_VECTORS)),
    Info::new(Op::UaddOverflow, "uadd_overflow", 2, 2, TySet::INTS_WIDE).imm(0b10),
    Info::new(Op::SaddOverflow, "sadd_overflow", 2, 2, TySet::INTS_WIDE).imm(0b10),
    Info::new(Op::UsubOverflow, "usub_overflow", 2, 2, TySet::INTS_WIDE).imm(0b10),
    Info::new(Op::SsubOverflow, "ssub_overflow", 2, 2, TySet::INTS_WIDE).imm(0b10),
    Info::new(Op::UmulOverflow, "umul_overflow", 2, 2, TySet::INTS).imm(0b10),
    Info::new(Op::SmulOverflow, "smul_overflow", 2, 2, TySet::INTS).imm(0b10),
    Info::new(Op::UaddOverflowCin, "uadd_overflow_cin", 3, 2, TySet::INTS).imm(0b010).acc(0b011),
    Info::new(Op::SaddOverflowCin, "sadd_overflow_cin", 3, 2, TySet::INTS).imm(0b010).acc(0b011),
    Info::new(Op::UsubOverflowBin, "usub_overflow_bin", 3, 2, TySet::INTS).imm(0b010).acc(0b011),
    Info::new(Op::SsubOverflowBin, "ssub_overflow_bin", 3, 2, TySet::INTS).imm(0b010).acc(0b011),
    binary_imm(Op::UaddOverflowTrap, "uadd_overflow_trap", I32_UP).traps(),
    binary_imm(Op::Band, "band", TySet::BITS),
    binary_imm(Op::Bor, "bor", TySet::BITS),
    binary_imm(Op::Bxor, "bxor", TySet::BITS),
    unary(Op::Bnot, "bnot", TySet::BITS),
    binary_imm(Op::Rotl, "rotl", TySet::INTS_WIDE),
    binary_imm(Op::Rotr, "rotr", TySet::INTS_WIDE),
    binary_imm(Op::Ishl, "ishl", TySet::INT_MATH),
    binary_imm(Op::Ushr, "ushr", TySet::INT_MATH),
    binary_imm(Op::Sshr, "sshr", TySet::INT_MATH),
    unary(Op::Bitrev, "bitrev", TySet::INTS_WIDE),
    unary(Op::Clz, "clz", TySet::INTS_WIDE),
    unary(Op::Cls, "cls", TySet::INTS_WIDE),
    unary(Op::Ctz, "ctz", TySet::INTS_WIDE),
    unary(Op::Bswap, "bswap", I16_TO_I128),
    unary(Op::Popcnt, "popcnt", TySet::INTS_WIDE.union(ty(Ty::I8x16))),
    Info::new(Op::Bitselect, "bitselect", 3, 1, TySet::INTS_WIDE.union(TySet::VECTORS)),
    conversion(Op::Bmask, "bmask", TySet::INTS_WIDE, TySet::INTS_WIDE),
    binary_imm(Op::Icmp, "icmp", TySet::INT_MATH).aux(Aux::IntCc).no_both(),
    binary(Op::Fcmp, "fcmp", TySet::FLOAT_MATH).aux(Aux::FloatCc).no_both(),
    Info::new(Op::Select, "select", 3, 1, TySet::ALL).aux(Aux::Ty(TySet::INTS)).imm(0b110),
    binary_imm(Op::SelectIcmp, "select_icmp", TySet::INTS).aux(Aux::IntCcTy).no_both(),
    binary(Op::Fadd, "fadd", TySet::FLOAT_MATH),
    binary(Op::Fsub, "fsub", TySet::FLOAT_MATH),
    binary(Op::Fmul, "fmul", TySet::FLOAT_MATH),
    binary(Op::Fdiv, "fdiv", TySet::FLOAT_MATH),
    unary(Op::Sqrt, "sqrt", TySet::FLOAT_MATH),
    Info::new(Op::Fma, "fma", 3, 1, TySet::FLOAT_MATH).aux(Aux::Negated),
    unary(Op::Fneg, "fneg", TySet::FLOAT_MATH),
    unary(Op::Fabs, "fabs", TySet::FLOAT_MATH),
    binary(Op::Fcopysign, "fcopysign", TySet::FLOAT_MATH),
    binary(Op::Fmin, "fmin", TySet::FLOAT_MATH),
    binary(Op::Fmax, "fmax", TySet::FLOAT_MATH),
    unary(Op::Ceil, "ceil", TySet::FLOAT_MATH),
    unary(Op::Floor, "floor", TySet::FLOAT_MATH),
    unary(Op::Trunc, "trunc", TySet::FLOAT_MATH),
    unary(Op::Nearest, "nearest", TySet::FLOAT_MATH),
    conversion(Op::Bitcast, "bitcast", TySet::ALL, TySet::ALL),
    conversion(Op::Ireduce, "ireduce", TySet::INTS, TySet::INTS_WIDE),
    conversion(Op::Uextend, "uextend", I16_TO_I128, TySet::INTS),
    conversion(Op::Sextend, "sextend", I16_TO_I128, TySet::INTS),
    unary(Op::Fpromote, "fpromote", ty(Ty::F64)),
    unary(Op::Fdemote, "fdemote", ty(Ty::F32)),
    unary(Op::Fvdemote, "fvdemote", ty(Ty::F32x4)),
    unary(Op::FvpromoteLow, "fvpromote_low", ty(Ty::F64x2)),
    conversion(Op::FcvtToUint, "fcvt_to_uint", TySet::INTS, TySet::FLOATS).traps(),
    conversion(Op::FcvtToSint, "fcvt_to_sint", TySet::INTS, TySet::FLOATS).traps(),
    conversion(Op::FcvtToUintSat, "fcvt_to_uint_sat", FCVT_TO, FLOAT_SOURCES),
    conversion(Op::FcvtToSintSat, "fcvt_to_sint_sat", FCVT_TO, FLOAT_SOURCES),
    conversion(Op::FcvtFromUint, "fcvt_from_uint", TySet::FLOAT_MATH, FCVT_FROM),
    conversion(Op::FcvtFromSint, "fcvt_from_sint", TySet::FLOAT_MATH, FCVT_FROM),
    Info::new(Op::Isplit, "isplit", 1, 2, ty(Ty::I128)),
    binary(Op::Iconcat, "iconcat", ty(Ty::I128)),
    binary(Op::Snarrow, "snarrow", NARROWED),
    binary(Op::Unarrow, "unarrow", NARROWED),
    binary(Op::Uunarrow, "uunarrow", NARROWED),
    unary(Op::SwidenLow, "swiden_low", WIDENED),
    unary(Op::SwidenHigh, "swiden_high", WIDENED),
    unary(Op::UwidenLow, "uwiden_low", WIDENED),
    unary(Op::UwidenHigh, "uwiden_high", WIDENED),
    binary(Op::IaddPairwise, "iadd_pairwise", NARROWED),
    conversion(Op::ScalarToVector, "scalar_to_vector", TySet::VECTORS, TySet::INTS.union(TySet::FLOATS))
        .imm(0b1),
    unary(Op::Splat, "splat", TySet::VECTORS).imm(0b1),
    binary(Op::Swizzle, "swizzle", ty(Ty::I8x16)).acc(0),
    binary(Op::Shuffle, "shuffle", ty(Ty::I8x16)).acc(0),
    binary(Op::Insertlane, "insertlane", TySet::VECTORS).aux(Aux::Lane).acc(0b01).no_both(),
    unary(Op::Extractlane, "extractlane", TySet::VECTORS).aux(Aux::Lane).no_both(),
    unary(Op::VanyTrue, "vany_true", TySet::INT_VECTORS),
    unary(Op::VallTrue, "vall_true", TySet::INT_VECTORS),
    conversion(Op::VhighBits, "vhigh_bits", TySet::VECTORS, TySet::INTS),
    Info::new(Op::Blendv, "blendv", 3, 1, BLEND_TYS),
    binary(Op::X86Pshufb, "x86_pshufb", ty(Ty::I8x16)).acc(0),
    binary(Op::X86Pmulhrsw, "x86_pmulhrsw", ty(Ty::I16x8)),
    binary(Op::X86Pmaddubsw, "x86_pmaddubsw", ty(Ty::I16x8)),
    unary(Op::X86Cvtt2dq, "x86_cvtt2dq", ty(Ty::I32x4)),
    load(Op::Load, "load", TySet::ALL),
    store(Op::Store, "store", TySet::ALL),
    load(Op::Uload8, "uload8", I16_UP),
    load(Op::Sload8, "sload8", I16_UP),
    store(Op::Istore8, "istore8", I16_UP),
    load(Op::Uload16, "uload16", I32_UP),
    load(Op::Sload16, "sload16", I32_UP),
    store(Op::Istore16, "istore16", I32_UP),
    load(Op::Uload32, "uload32", ty(Ty::I64)),
    load(Op::Sload32, "sload32", ty(Ty::I64)),
    store(Op::Istore32, "istore32", ty(Ty::I64)),
    load(Op::Uload8x8, "uload8x8", ty(Ty::I16x8)),
    load(Op::Sload8x8, "sload8x8", ty(Ty::I16x8)),
    load(Op::Uload16x4, "uload16x4", ty(Ty::I32x4)),
    load(Op::Sload16x4, "sload16x4", ty(Ty::I32x4)),
    load(Op::Uload32x2, "uload32x2", ty(Ty::I64x2)),
    load(Op::Sload32x2, "sload32x2", ty(Ty::I64x2)),
    Info::new(Op::StackAddr, "stack_addr", 0, 1, ty(Ty::I64)),
    load(Op::AtomicLoad, "atomic_load", TySet::INTS).no_acc(),
    store(Op::AtomicStore, "atomic_store", TySet::INTS).no_acc(),
    Info::new(Op::AtomicRmw, "atomic_rmw", 2, 1, TySet::INTS).aux(Aux::Rmw).imm(0b10).no_acc(),
    Info::new(Op::AtomicCas, "atomic_cas", 3, 1, TySet::INTS_WIDE).no_acc(),
    Info::new(Op::Fence, "fence", 0, 0, I8),
    // `vmctx` lives across the whole function: never in an accumulator.
    Info::new(Op::LoadConfined, "load_confined", 2, 1, TySet::ALL).aux(Aux::Confined).acc(0b01),
    Info::new(Op::StoreConfined, "store_confined", 3, 0, TySet::ALL)
        .aux(Aux::Confined)
        .imm(0b001)
        .acc(0b011),
    Info::new(Op::Jump, "jump", 0, 0, I8),
    Info::new(Op::Brif, "brif", 1, 0, TySet::INTS_WIDE).branches(),
    Info::new(Op::BrIcmp, "br_icmp", 2, 0, TySet::INTS_WIDE).imm(0b10).aux(Aux::IntCc).branches(),
    Info::new(Op::Trapz, "trapz", 1, 0, TySet::INTS_WIDE).traps(),
    Info::new(Op::Trapnz, "trapnz", 1, 0, TySet::INTS_WIDE).traps(),
];

const _: () = {
    let mut i = 0;
    while i < TABLE.len() {
        assert!(TABLE[i].op as usize == i, "the info table is out of the ops' order");
        assert!(TABLE[i].args <= 3 && TABLE[i].results <= 2);
        i += 1;
    }
};

static INFO: [Info; Op::Trapnz as usize + 1] = TABLE;

/// One stencil's name: 4 bytes, packed by [`Shape::key`] into 28 bits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Shape {
    pub op: Op,
    pub ty: Ty,
    pub aux: u8,
    pub srcs: [Src; 3],
    pub dst: Dst,
}

impl Shape {
    /// `op` in bits 20..28, `ty` 16..20, `aux` 8..16, the three sources 2 bits each from bit 2,
    /// `dst` in bits 0..2.
    pub const fn key(self) -> u32 {
        (self.op as u32) << 20
            | (self.ty as u32) << 16
            | (self.aux as u32) << 8
            | (self.srcs[0] as u32) << 6
            | (self.srcs[1] as u32) << 4
            | (self.srcs[2] as u32) << 2
            | self.dst as u32
    }

    pub fn from_key(key: u32) -> Option<Shape> {
        if key >> 28 != 0 {
            return None;
        }
        let field = |shift: u32, bits: u32| ((key >> shift) & ((1 << bits) - 1)) as u8;
        Some(Shape {
            op: Op::from_u8(field(20, 8))?,
            ty: Ty::from_u8(field(16, 4))?,
            aux: field(8, 8),
            srcs: [
                Src::from_u8(field(6, 2))?,
                Src::from_u8(field(4, 2))?,
                Src::from_u8(field(2, 2))?,
            ],
            dst: Dst::from_u8(field(0, 2))?,
        })
    }

    /// `aux` as a type, for the ops whose `aux` is one.
    pub fn aux_ty(self) -> Ty {
        Ty::from_u8(self.aux & 0xf).unwrap_or(self.ty)
    }

    /// The type of operand `n`.
    pub fn arg(self, n: usize) -> Ty {
        let ty = self.ty;
        match self.op {
            Op::Select if n == 0 => self.aux_ty(),
            Op::Ishl | Op::Ushr | Op::Sshr | Op::Rotl | Op::Rotr if n == 1 => Ty::I8,
            Op::Bmask
            | Op::Bitcast
            | Op::Ireduce
            | Op::Uextend
            | Op::Sextend
            | Op::FcvtToUint
            | Op::FcvtToSint
            | Op::FcvtToUintSat
            | Op::FcvtToSintSat
            | Op::FcvtFromUint
            | Op::FcvtFromSint
            | Op::ScalarToVector => self.aux_ty(),
            Op::Fpromote => Ty::F32,
            Op::Fdemote => Ty::F64,
            Op::Fvdemote => Ty::F64x2,
            Op::FvpromoteLow => Ty::F32x4,
            Op::Iconcat => Ty::I64,
            Op::Snarrow | Op::Unarrow | Op::Uunarrow => ty.wider().unwrap_or(ty),
            Op::SwidenLow | Op::SwidenHigh | Op::UwidenLow | Op::UwidenHigh => {
                ty.narrower().unwrap_or(ty)
            }
            Op::Splat => ty.lane(),
            Op::Insertlane if n == 1 => ty.lane(),
            Op::X86Pmaddubsw => Ty::I8x16,
            Op::X86Cvtt2dq => Ty::F32x4,
            Op::Load
            | Op::Uload8
            | Op::Sload8
            | Op::Uload16
            | Op::Sload16
            | Op::Uload32
            | Op::Sload32
            | Op::Uload8x8
            | Op::Sload8x8
            | Op::Uload16x4
            | Op::Sload16x4
            | Op::Uload32x2
            | Op::Sload32x2
            | Op::AtomicLoad
            | Op::LoadConfined => Ty::I64,
            Op::Store | Op::Istore8 | Op::Istore16 | Op::Istore32 | Op::AtomicStore if n == 1 => {
                Ty::I64
            }
            Op::StoreConfined if n > 0 => Ty::I64,
            Op::AtomicRmw | Op::AtomicCas if n == 0 => Ty::I64,
            Op::UaddOverflowCin | Op::SaddOverflowCin | Op::UsubOverflowBin | Op::SsubOverflowBin
                if n == 2 =>
            {
                Ty::I8
            }
            _ => ty,
        }
    }

    /// The type of result `k`.
    pub fn result(self, k: usize) -> Ty {
        let ty = self.ty;
        match self.op {
            Op::Icmp | Op::Fcmp if ty.is_vector() => ty.int_lanes(),
            Op::Icmp | Op::Fcmp | Op::VanyTrue | Op::VallTrue => Ty::I8,
            Op::UaddOverflow
            | Op::SaddOverflow
            | Op::UsubOverflow
            | Op::SsubOverflow
            | Op::UmulOverflow
            | Op::SmulOverflow
            | Op::UaddOverflowCin
            | Op::SaddOverflowCin
            | Op::UsubOverflowBin
            | Op::SsubOverflowBin
                if k == 1 =>
            {
                Ty::I8
            }
            Op::Isplit => Ty::I64,
            Op::Extractlane => ty.lane(),
            Op::VhighBits => self.aux_ty(),
            _ => ty,
        }
    }

    /// The plain access a confined op fuses and how it adds its offset; `None` for any other
    /// op, or an `aux` that names none.
    pub fn confined(self) -> Option<(Op, Disp)> {
        let accesses: &[Op] = match self.op {
            Op::LoadConfined => &CONFINED_LOADS,
            Op::StoreConfined => &CONFINED_STORES,
            _ => return None,
        };
        let access = *accesses.get(usize::from(self.aux & 0x1f))?;
        Some((access, Disp::from_u8(self.aux >> 5)?))
    }

    /// The confined op of the plain access `access` with its offset added as `disp`: its op and
    /// `aux`; `None` when no confined op fuses `access`.
    pub fn confining(access: Op, disp: Disp) -> Option<(Op, u8)> {
        let (op, accesses): (Op, &[Op]) = if CONFINED_LOADS.contains(&access) {
            (Op::LoadConfined, &CONFINED_LOADS)
        } else {
            (Op::StoreConfined, &CONFINED_STORES)
        };
        let at = accesses.iter().position(|it| *it == access)?;
        Some((op, at as u8 | (disp as u8) << 5))
    }

    /// Whether `aux` is one of the op's for the type.
    fn aux_ok(self) -> bool {
        let info = self.op.info();
        let (ty, aux) = (self.ty, self.aux);
        match info.aux {
            Aux::None => aux == 0,
            Aux::IntCc => aux < IntCc::COUNT,
            Aux::FloatCc => aux < FloatCc::COUNT,
            Aux::IntCcTy => aux & 0xf < IntCc::COUNT && aux >> 4 == ty as u8,
            Aux::Lane => aux < ty.lanes(),
            Aux::Rmw => aux < RmwOp::COUNT,
            Aux::Wide => aux == 0 || (aux == 1 && ty.bytes() == 8),
            Aux::Negated => aux < 2,
            Aux::Confined => {
                self.confined().is_some_and(|(access, _)| access.info().tys.contains(ty))
            }
            Aux::Ty(set) => {
                let Some(src) = Ty::from_u8(aux) else { return false };
                set.contains(src)
                    && match self.op {
                        Op::Bitcast => src.bytes() == ty.bytes() && src != ty,
                        Op::Ireduce => src.bytes() > ty.bytes(),
                        Op::Uextend | Op::Sextend => src.bytes() < ty.bytes(),
                        Op::FcvtToUint
                        | Op::FcvtToSint
                        | Op::FcvtToUintSat
                        | Op::FcvtToSintSat
                        | Op::FcvtFromUint
                        | Op::FcvtFromSint => {
                            src.is_vector() == ty.is_vector() && src.lanes() == ty.lanes()
                        }
                        Op::ScalarToVector => src == ty.lane(),
                        Op::VhighBits => u32::from(src.bytes()) * 8 >= u32::from(ty.lanes()),
                        _ => true,
                    }
            }
        }
    }

    /// Whether Cranelift lowers the shape's instruction on `isa` at the baseline the stencil
    /// backend tiers up to (`tests/host.rs`'s ISA: x86-64 with SSE2, AArch64 with NEON). The
    /// [`TABLE`]'s type sets are the union of both backends' rules; these are the combinations
    /// one of them has none for in `isa/{x64,aarch64}/lower.isle` of Cranelift 0.137, which a
    /// stencil would let run where the function could never be tiered up.
    fn lowered_on(self, isa: Isa) -> bool {
        let (ty, x64) = (self.ty, isa == Isa::X86_64);
        match self.op {
            // New in 0.137; neither backend has a rule yet.
            Op::UaddOverflowCin | Op::SaddOverflowCin | Op::UsubOverflowBin | Op::SsubOverflowBin => {
                false
            }
            // x64 has `pavgb`/`pavgw` and the saturating byte and word adds and subtracts only.
            Op::AvgRound | Op::UaddSat | Op::SaddSat | Op::UsubSat | Op::SsubSat => {
                !x64 || matches!(ty, Ty::I8x16 | Ty::I16x8)
            }
            Op::Fcopysign => !(x64 && ty.is_vector()),
            // AArch64 multiplies lanes of 32 bits or fewer with `mul`; x64 has no `i8x16` rule.
            Op::Imul => !(x64 && ty == Ty::I8x16),
            // x64 has no rule narrowing `i64x2` operands, and none for `uunarrow`.
            Op::Snarrow | Op::Unarrow | Op::SqmulRoundSat => !(x64 && ty == Ty::I32x4),
            Op::Uunarrow => !x64,
            // x64 converts a float to 32 or 64 bits only, and a vector to `i32x4` only.
            Op::FcvtToUint | Op::FcvtToSint => !(x64 && matches!(ty, Ty::I8 | Ty::I16)),
            Op::FcvtToUintSat | Op::FcvtToSintSat => !(x64 && matches!(ty, Ty::I8 | Ty::I16 | Ty::I64x2)),
            // Of vectors, x64 has no `one` or `ueq`, and AArch64 neither those nor the
            // unordered orderings.
            Op::Fcmp if ty.is_vector() => match FloatCc::from_u8(self.aux) {
                Some(FloatCc::OrderedNotEqual | FloatCc::UnorderedOrEqual) => false,
                Some(
                    FloatCc::UnorderedOrLessThan
                    | FloatCc::UnorderedOrLessThanOrEqual
                    | FloatCc::UnorderedOrGreaterThan
                    | FloatCc::UnorderedOrGreaterThanOrEqual,
                ) => x64,
                _ => true,
            },
            _ => true,
        }
    }

    /// Whether the shape is one of the set of some instruction set ([`Shape::exists_on`]).
    pub fn exists(self) -> bool {
        Isa::ALL.into_iter().any(|isa| self.exists_on(isa))
    }

    /// Whether the shape is one of `isa`'s set: its op is `isa`'s and has the type, `aux` is
    /// in range, the sources and the destination are ones the op's [`Info`] allows for the
    /// types, at most one source is an accumulator, and it is not pruned.
    pub fn exists_on(self, isa: Isa) -> bool {
        let info = self.op.info();
        if !self.op.on(isa) || !info.tys.contains(self.ty) || !self.aux_ok() || !self.lowered_on(isa) {
            return false;
        }
        if let Some((_, disp)) = self.confined()
            && !disp.on(isa)
        {
            return false;
        }
        let mut accs = 0;
        for (n, src) in self.srcs.into_iter().enumerate() {
            let arg = n < usize::from(info.args);
            let ok = match src {
                Src::None => !arg,
                Src::Slot => arg,
                Src::Acc => arg && info.acc & (1 << n) != 0 && self.arg(n).acc().is_some(),
                Src::Imm => arg && info.imm & (1 << n) != 0 && self.arg(n).is_narrow_int(),
            };
            if !ok {
                return false;
            }
            accs += u8::from(src == Src::Acc);
        }
        if accs > 1 {
            return false;
        }
        let ok = match self.dst {
            Dst::None => info.results == 0,
            Dst::Slot => info.results > 0,
            Dst::Acc | Dst::Both => {
                info.results > 0 && info.dst_acc && self.result(0).acc().is_some()
            }
        };
        ok && self.kept()
    }

    /// Whether a shape the op allows is kept in the set: the base shape (operands from slots
    /// or immediates, the result to its slot) always is, as selection falls back to it; the
    /// accumulator forms are pruned where selection would seldom make them.
    fn kept(self) -> bool {
        let base = !self.srcs.contains(&Src::Acc) && matches!(self.dst, Dst::None | Dst::Slot);
        if base {
            return true;
        }
        // `select` of a condition wider than the `i8` an `icmp` gives is rare: base only.
        if self.op == Op::Select && self.aux != Ty::I8 as u8 {
            return false;
        }
        // A confined atomic takes its operands from slots, as the plain one does.
        if let Some((access, _)) = self.confined()
            && access.info().acc == 0
        {
            return false;
        }
        if self.dst == Dst::Both {
            let info = self.op.info();
            let vector = self.ty.is_vector()
                || self.result(0).is_vector()
                || (info.args > 0 && self.arg(0).is_vector());
            return info.both && !vector;
        }
        true
    }
}

impl fmt::Display for Shape {
    /// The stencil's name in errors: `iadd.i64.0.sa.s` (op, type, aux, sources,
    /// destination).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let src = |it: Src| match it {
            Src::None => "",
            Src::Slot => "s",
            Src::Acc => "a",
            Src::Imm => "i",
        };
        let dst = match self.dst {
            Dst::None => "n",
            Dst::Slot => "s",
            Dst::Acc => "a",
            Dst::Both => "b",
        };
        write!(
            f,
            "{}.{}.{}.{}{}{}.{dst}",
            self.op.info().name,
            self.ty.name(),
            self.aux,
            src(self.srcs[0]),
            src(self.srcs[1]),
            src(self.srcs[2]),
        )
    }
}

/// The `aux` values to try for an op of type `ty`: a superset of the valid ones, in order.
fn auxes(info: &Info, ty: Ty) -> impl Iterator<Item = u8> {
    let (start, end) = match info.aux {
        Aux::None => (0, 1),
        Aux::IntCc => (0, IntCc::COUNT),
        Aux::FloatCc => (0, FloatCc::COUNT),
        Aux::IntCcTy => ((ty as u8) << 4, ((ty as u8) << 4) + IntCc::COUNT),
        Aux::Lane => (0, ty.lanes()),
        Aux::Ty(_) => (0, Ty::ALL.len() as u8),
        Aux::Rmw => (0, RmwOp::COUNT),
        Aux::Wide | Aux::Negated => (0, 2),
        Aux::Confined => (0, 3 << 5),
    };
    start..end
}

/// Calls `f` with every shape of some instruction set's set ([`Shape::exists`]), in one
/// order: by op number, then type number, then `aux`, then sources and destination by their
/// numbers.
pub fn each(mut f: impl FnMut(Shape)) {
    walk(|shape| {
        if shape.exists() {
            f(shape);
        }
    });
}

/// Calls `f` with every shape of `isa`'s set, in [`each`]'s order. The generator makes one
/// stencil for each; [`Shape::exists_on`] holds exactly for these.
pub fn each_on(isa: Isa, mut f: impl FnMut(Shape)) {
    walk(|shape| {
        if shape.exists_on(isa) {
            f(shape);
        }
    });
}

/// Every candidate shape, in [`each`]'s order.
fn walk(mut f: impl FnMut(Shape)) {
    static SRCS: [Src; 4] = Src::ALL;
    for op in Op::all() {
        let info = op.info();
        let args = usize::from(info.args);
        for ty in info.tys.iter() {
            for aux in auxes(info, ty) {
                let choices =
                    |n: usize| -> &'static [Src] { if n < args { &SRCS[1..] } else { &SRCS[..1] } };
                for s0 in choices(0) {
                    for s1 in choices(1) {
                        for s2 in choices(2) {
                            for dst in Dst::ALL {
                                f(Shape { op, ty, aux, srcs: [*s0, *s1, *s2], dst });
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Shape> {
        let mut shapes = Vec::new();
        each(|shape| shapes.push(shape));
        shapes
    }

    #[test]
    fn the_set_is_in_order_and_has_no_shape_twice() {
        let shapes = all();
        assert!(shapes.len() > 1000);
        for pair in shapes.windows(2) {
            assert!(pair[0].key() < pair[1].key(), "{} then {}", pair[0], pair[1]);
        }
    }

    #[test]
    fn every_key_round_trips() {
        for shape in all() {
            assert_eq!(Shape::from_key(shape.key()), Some(shape), "{shape}");
            assert!(shape.key() >> 28 == 0);
        }
    }

    #[test]
    fn exists_holds_exactly_for_what_each_lists() {
        for isa in Isa::ALL {
            let mut listed = std::collections::HashSet::new();
            each_on(isa, |shape| {
                assert!(shape.exists_on(isa), "{shape}");
                listed.insert(shape.key());
            });
            walk(|shape| assert_eq!(shape.exists_on(isa), listed.contains(&shape.key()), "{shape}"));
        }
    }

    #[test]
    fn at_most_one_operand_comes_from_an_accumulator() {
        each(|shape| assert!(shape.srcs.iter().filter(|it| **it == Src::Acc).count() <= 1));
    }

    #[test]
    fn immediates_are_of_integers_up_to_64_bits() {
        each(|shape| {
            for (n, src) in shape.srcs.iter().enumerate() {
                if *src == Src::Imm {
                    assert!(shape.arg(n).is_narrow_int(), "{shape}");
                }
            }
        });
    }

    #[test]
    fn f16_and_f128_have_only_moves_loads_stores_selects_and_bitcasts() {
        let allowed = [
            Op::Copy,
            Op::Fconst,
            Op::Vconst,
            Op::Select,
            Op::Bitcast,
            Op::Load,
            Op::Store,
            Op::LoadConfined,
            Op::StoreConfined,
        ];
        each(|shape| {
            if matches!(shape.ty, Ty::F16 | Ty::F128) {
                assert!(allowed.contains(&shape.op), "{shape}");
            }
        });
    }

    #[test]
    fn baseline_libcalls_have_no_x86_stencils() {
        each_on(Isa::X86_64, |shape| {
            assert!(!matches!(
                shape.op,
                Op::Ceil | Op::Floor | Op::Trunc | Op::Nearest | Op::Fma | Op::Swizzle | Op::Shuffle
            ));
        });
        each_on(Isa::Aarch64, |shape| assert!(!shape.op.info().name.starts_with("x86_")));
    }

    #[test]
    fn every_shape_has_its_base_shape() {
        for isa in Isa::ALL {
            each_on(isa, |shape| {
                let mut base = shape;
                for src in &mut base.srcs {
                    if *src == Src::Acc {
                        *src = Src::Slot;
                    }
                }
                if matches!(base.dst, Dst::Acc | Dst::Both) {
                    base.dst = Dst::Slot;
                }
                assert!(base.exists_on(isa), "{shape} without its base {base}");
            });
        }
    }

    #[test]
    fn each_isa_has_about_7000_stencils() {
        for isa in Isa::ALL {
            let mut n = 0;
            each_on(isa, |_| n += 1);
            assert!((6000..8000).contains(&n), "{isa:?} has {n}");
        }
    }

    #[test]
    fn br_icmp_has_no_result_and_fuses_every_condition() {
        for cc in IntCc::ALL {
            let shape = Shape {
                op: Op::BrIcmp,
                ty: Ty::I64,
                aux: cc as u8,
                srcs: [Src::Slot, Src::Imm, Src::None],
                dst: Dst::None,
            };
            assert!(shape.exists(), "{shape}");
        }
    }

    #[test]
    fn confined_ops_name_each_access_and_how_it_adds_its_offset() {
        for access in CONFINED_LOADS.into_iter().chain(CONFINED_STORES) {
            for disp in [Disp::Zero, Disp::Lo12, Disp::Imm32] {
                let (op, aux) = Shape::confining(access, disp).unwrap();
                let shape = Shape { op, ty: Ty::I64, aux, srcs: [Src::None; 3], dst: Dst::None };
                assert_eq!(shape.confined(), Some((access, disp)), "{shape}");
            }
        }
        assert_eq!(Shape::confining(Op::Iadd, Disp::Zero), None);
        for isa in Isa::ALL {
            let mut lo12 = false;
            each_on(isa, |shape| {
                let Some((access, disp)) = shape.confined() else { return };
                assert!(disp.on(isa), "{shape} on {isa:?}");
                assert!(access.info().tys.contains(shape.ty), "{shape}");
                lo12 |= disp == Disp::Lo12;
            });
            assert_eq!(lo12, isa == Isa::Aarch64, "{isa:?}");
        }
    }

    #[test]
    fn the_info_table_is_in_number_order() {
        for (i, info) in INFO.iter().enumerate() {
            assert_eq!(info.op as usize, i);
            assert_eq!(Op::from_u8(i as u8), Some(info.op));
        }
        assert_eq!(Op::from_u8(Op::COUNT), None);
    }

    #[test]
    fn a_shape_out_of_the_set_does_not_exist() {
        let iadd = Shape {
            op: Op::Iadd,
            ty: Ty::I64,
            aux: 0,
            srcs: [Src::Slot, Src::Imm, Src::None],
            dst: Dst::Acc,
        };
        assert!(iadd.exists());
        assert!(!Shape { srcs: [Src::Acc, Src::Acc, Src::None], ..iadd }.exists());
        assert!(!Shape { srcs: [Src::Imm, Src::Slot, Src::None], ..iadd }.exists());
        assert!(!Shape { srcs: [Src::Slot, Src::Slot, Src::Slot], ..iadd }.exists());
        assert!(!Shape { aux: 1, ..iadd }.exists());
        assert!(!Shape { ty: Ty::F16, ..iadd }.exists());
        assert!(!Shape { ty: Ty::I128, dst: Dst::Acc, srcs: [Src::Slot; 3], ..iadd }.exists());
    }
}
