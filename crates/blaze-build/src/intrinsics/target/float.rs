//! The generic LLVM float intrinsics the library's NEON code declares, each an LIR float
//! operation applied lane by lane (INTRINSICS.md 3.3, P7b: "a generic name with an LIR op maps
//! to it").
//!
//! The pinned library declares them in `extern "llvm-intrinsic"` blocks under
//! `link_name = "llvm.<fn>.<shape>"`: stdarch `aarch64/neon/generated.rs` `vrndn_f32`
//! (`link_name = "llvm.roundeven.v2f32"`), `vrndnq_f64` (`"llvm.roundeven.v2f64"`),
//! `vabsq_f32` (`"llvm.fabs.v4f32"`), `vsqrtq_f32` (`"llvm.sqrt.v4f32"`), `vrndm*`
//! (`"llvm.floor.*"`), `vrndp*` (`"llvm.ceil.*"`), `vrnd*` (`"llvm.trunc.*"`), `vrnda*`
//! (`"llvm.round.*"`), `vrndi*` (`"llvm.nearbyint.*"`), `vrndx*` (`"llvm.rint.*"`),
//! `vminnm*`/`vmaxnm*` (`"llvm.minnum.*"`/`"llvm.maxnum.*"`), `vmin*`/`vmax*` on floats
//! (`"llvm.minimum.*"`/`"llvm.maximum.*"`), `vfma*` (`"llvm.fma.*"`).
//!
//! What each is, from LLVM's LangRef (the meaning the library's declaration asks for):
//! `llvm.roundeven` "rounds ... to the nearest integer in floating-point format, rounding
//! halfway cases to even"; `llvm.nearbyint` and `llvm.rint` round "according to the current
//! rounding mode", and Rust code runs in the default floating-point environment only (the
//! Reference, "Expressions", "Floating-point": no way to change the rounding mode is offered;
//! `core::f32::round_ties_even` is documented as this operation), so both are ties-to-even
//! here: LIR's `nearest`; `llvm.fabs`, `llvm.sqrt`, `llvm.floor`, `llvm.ceil`, `llvm.trunc`,
//! `llvm.round` ("rounding halfway cases away from zero") are LIR's `fabs`, `sqrt`, `floor`,
//! `ceil`, `ftrunc`, `fround`; `llvm.minnum`/`llvm.maxnum` (IEEE `minNum`) are `fmin_num`/
//! `fmax_num`; `llvm.minimum`/`llvm.maximum` (IEEE 754-2019, NaN propagating) are `fminimum`/
//! `fmaximum`; `llvm.copysign` is `fcopysign`; `llvm.fma` ("fused multiply-add ... without
//! intermediate rounding") is `fma`. Each is applied to every lane, as blaze builds every
//! vector operation today (intrinsics/simd.rs: "No vector register is used"); the result is
//! the same per lane.
//!
//! The shapes are the NEON ones over `f32` and `f64` ([`Shape`]); an `f16` shape (`.f16`,
//! `.v4f16`, `.v8f16`) is not in the set: it is refused by name as every other `llvm.*` name
//! outside it is (`UnknownTarget`), as the walk builds no `f16` lane yet.

use blaze_lir::{Op, Ty};

/// A generic float function of the set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FloatFn {
    Roundeven,
    Nearbyint,
    Rint,
    Fabs,
    Sqrt,
    Floor,
    Ceil,
    Trunc,
    Round,
    Minnum,
    Maxnum,
    Minimum,
    Maximum,
    Copysign,
    Fma,
}

/// Every function, in the order of [`NAMES`]' rows.
pub const FNS: [FloatFn; 15] = [
    FloatFn::Roundeven,
    FloatFn::Nearbyint,
    FloatFn::Rint,
    FloatFn::Fabs,
    FloatFn::Sqrt,
    FloatFn::Floor,
    FloatFn::Ceil,
    FloatFn::Trunc,
    FloatFn::Round,
    FloatFn::Minnum,
    FloatFn::Maxnum,
    FloatFn::Minimum,
    FloatFn::Maximum,
    FloatFn::Copysign,
    FloatFn::Fma,
];

/// The type an intrinsic is overloaded at: its name's last part.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shape {
    F32,
    F64,
    V2F32,
    V4F32,
    V1F64,
    V2F64,
}

/// Every shape, in the order of [`NAMES`]' columns.
pub const SHAPES: [Shape; 6] = [Shape::F32, Shape::F64, Shape::V2F32, Shape::V4F32, Shape::V1F64, Shape::V2F64];

/// One row of link names: `llvm.<fn>.<shape>` for every shape, in [`SHAPES`]' order.
macro_rules! row {
    ($f:literal) => {
        [
            concat!("llvm.", $f, ".f32"),
            concat!("llvm.", $f, ".f64"),
            concat!("llvm.", $f, ".v2f32"),
            concat!("llvm.", $f, ".v4f32"),
            concat!("llvm.", $f, ".v1f64"),
            concat!("llvm.", $f, ".v2f64"),
        ]
    };
}

/// The link names: row by [`FNS`], column by [`SHAPES`] (one home for the spelling).
const NAMES: [[&str; 6]; 15] = [
    row!("roundeven"),
    row!("nearbyint"),
    row!("rint"),
    row!("fabs"),
    row!("sqrt"),
    row!("floor"),
    row!("ceil"),
    row!("trunc"),
    row!("round"),
    row!("minnum"),
    row!("maxnum"),
    row!("minimum"),
    row!("maximum"),
    row!("copysign"),
    row!("fma"),
];

/// A call of a generic float intrinsic: the function at a shape.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FloatCall {
    pub func: FloatFn,
    pub shape: Shape,
}

impl FloatFn {
    /// The LIR operation each lane is: see the module doc for each one's meaning.
    pub fn op(self) -> Op {
        match self {
            FloatFn::Roundeven | FloatFn::Nearbyint | FloatFn::Rint => Op::Nearest,
            FloatFn::Fabs => Op::Fabs,
            FloatFn::Sqrt => Op::Sqrt,
            FloatFn::Floor => Op::Floor,
            FloatFn::Ceil => Op::Ceil,
            FloatFn::Trunc => Op::Ftrunc,
            FloatFn::Round => Op::Fround,
            FloatFn::Minnum => Op::FminNum,
            FloatFn::Maxnum => Op::FmaxNum,
            FloatFn::Minimum => Op::Fminimum,
            FloatFn::Maximum => Op::Fmaximum,
            FloatFn::Copysign => Op::Fcopysign,
            FloatFn::Fma => Op::Fma,
        }
    }

    /// How many operands it takes, each of the call's shape.
    pub fn arity(self) -> usize {
        match self {
            FloatFn::Minnum | FloatFn::Maxnum | FloatFn::Minimum | FloatFn::Maximum | FloatFn::Copysign => 2,
            FloatFn::Fma => 3,
            _ => 1,
        }
    }

    fn row(self) -> usize {
        // `FNS` holds every function once.
        FNS.iter().position(|&f| f == self).expect("every function is in `FNS`")
    }
}

impl Shape {
    /// The lane's LIR type.
    pub fn elem(self) -> Ty {
        match self {
            Shape::F32 | Shape::V2F32 | Shape::V4F32 => Ty::F32,
            Shape::F64 | Shape::V1F64 | Shape::V2F64 => Ty::F64,
        }
    }

    /// The lane's size in bytes.
    pub fn stride(self) -> u64 {
        match self {
            Shape::F32 | Shape::V2F32 | Shape::V4F32 => 4,
            Shape::F64 | Shape::V1F64 | Shape::V2F64 => 8,
        }
    }

    /// The lanes of a vector shape; `None` for a scalar one (held as one LIR scalar, not in
    /// memory).
    pub fn lanes(self) -> Option<u64> {
        match self {
            Shape::F32 | Shape::F64 => None,
            Shape::V2F32 | Shape::V2F64 => Some(2),
            Shape::V4F32 => Some(4),
            Shape::V1F64 => Some(1),
        }
    }

    fn column(self) -> usize {
        // `SHAPES` holds every shape once.
        SHAPES.iter().position(|&s| s == self).expect("every shape is in `SHAPES`")
    }
}

impl FloatCall {
    /// The `link_name` the library declares it by.
    pub fn link_name(self) -> &'static str {
        NAMES[self.func.row()][self.shape.column()]
    }
}

/// The generic float intrinsic whose link name is exactly `link_name`, if any.
pub fn named(link_name: &str) -> Option<FloatCall> {
    for (r, row) in NAMES.iter().enumerate() {
        for (c, name) in row.iter().enumerate() {
            if *name == link_name {
                return Some(FloatCall { func: FNS[r], shape: SHAPES[c] });
            }
        }
    }
    None
}
