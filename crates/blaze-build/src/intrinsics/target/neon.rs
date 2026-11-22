//! The AArch64 NEON integer intrinsics the library declares that are one LIR vector operation
//! or a short fixed sequence of them: the table lookup `tbl1` and the register shifts
//! `ushl`/`sshl` (INTRINSICS.md 3.3: a target name with an LIR op maps to it).
//!
//! The pinned library declares them in `extern "llvm-intrinsic"` blocks: stdarch
//! `aarch64/neon/generated.rs` `vqtbl1q` (`link_name = "llvm.aarch64.neon.tbl1.v16i8"`,
//! `fn _vqtbl1q(a: int8x16_t, b: uint8x16_t) -> int8x16_t`; `vqtbl1q_u8`/`_s8`/`_p8` call it)
//! and `vqtbl1` (`"llvm.aarch64.neon.tbl1.v8i8"`, a 16-byte table and 8 indices);
//! `arm_shared/neon/generated.rs` `vshlq_u64` (`"llvm.aarch64.neon.ushl.v2i64"`,
//! `fn _vshlq_u64(a: uint64x2_t, b: int64x2_t) -> uint64x2_t`) and its kin at every integer
//! shape, `vshl*_s*` (`"llvm.aarch64.neon.sshl.<shape>"`).
//!
//! What each is, from the Arm A64 instruction set (the instructions the names lower to):
//! - `TBL` (one register): "Table vector Lookup ... reads each value from the vector elements in
//!   the index source ... uses each result as an index to perform a lookup in a table of bytes
//!   ... If an index is out of range for the table, the result for that lookup is 0". That is
//!   LIR's `swizzle` (op.rs: "The lanes of the first vector the second's lanes index (a table
//!   lookup); 0 for one out of range"), one operation at `v16i8`; at `v8i8` the 8 indices are
//!   widened to 16 by a shuffle, looked up, and the low 8 lanes kept.
//! - `USHL`/`SSHL`: "Unsigned/Signed Shift Left (register) ... takes each element in the vector
//!   of the first source ..., shifts each element by a value from the least significant byte of
//!   the corresponding element of the second source ..., places the results ...". The byte is
//!   signed: a negative amount shifts right (logically for `USHL`, arithmetically for `SSHL`),
//!   with no rounding. The shift is done at unbounded precision and truncated to the lane, so an
//!   amount of the lane's width or more gives 0, and one of minus the width or less gives 0
//!   (`USHL`) or the sign in every bit (`SSHL`). LIR's `shl`/`lshr`/`ashr` mask their amount to
//!   the width (op.rs), so the out-of-range lanes are chosen by `select`, never by the mask.
//! - `UMAXP`/`UMINP`/`SMAXP`/`SMINP` (stdarch `vpmaxq_u8` is `"llvm.aarch64.neon.umaxp.v16i8"`,
//!   `fn(a: uint8x16_t, b: uint8x16_t) -> uint8x16_t`, and kin at `v8i8`..`v4i32`): "creates a
//!   vector by concatenating the vector elements of the first source SIMD&FP register after the
//!   vector elements of the second source SIMD&FP register, reads each pair of adjacent vector
//!   elements from the concatenated vector, writes the largest [smallest] of each pair of
//!   values into a vector": result lane `e` is the larger of lanes `2e` and `2e + 1` of `a`'s
//!   lanes then `b`'s, compared unsigned (`U`) or signed (`S`). Two `shuffle`s (the even and odd
//!   lanes of the concatenation) and one `umax`/`umin`/`smax`/`smin`.
//!
//! Every operation is on LIR vectors; the Reference and Fast tiers take the function after
//! blaze-lir's legalize, which makes each lane by lane (as intrinsics/simd_vec.rs's rows).

use core::alloc::Allocator;

use blaze_lir::{Builder, Flags, Full, IntCond, Op, Scalar, Ty, Value};

/// A NEON integer function of the set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NeonFn {
    /// `tbl1`: a lookup in one 16-byte table.
    Tbl1,
    /// `ushl`: shift left by a signed amount, right shifts logical.
    Ushl,
    /// `sshl`: shift left by a signed amount, right shifts arithmetic.
    Sshl,
    /// `umaxp`: the unsigned larger of each adjacent pair of the operands concatenated.
    Umaxp,
    /// `uminp`: the unsigned smaller of each adjacent pair.
    Uminp,
    /// `smaxp`: the signed larger of each adjacent pair.
    Smaxp,
    /// `sminp`: the signed smaller of each adjacent pair.
    Sminp,
}

/// The integer vector type an intrinsic is overloaded at: its name's last part.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntShape {
    V8I8,
    V16I8,
    V4I16,
    V8I16,
    V2I32,
    V4I32,
    V1I64,
    V2I64,
}

/// Every integer shape, narrowest lanes first.
pub const SHAPES: [IntShape; 8] =
    [IntShape::V8I8, IntShape::V16I8, IntShape::V4I16, IntShape::V8I16, IntShape::V2I32, IntShape::V4I32, IntShape::V1I64, IntShape::V2I64];

/// A call of a NEON integer intrinsic: the function at a shape. Made only from [`NAMES`]'s
/// rows ([`named`], [`ALL`]), so every value has its link name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NeonCall {
    func: NeonFn,
    shape: IntShape,
}

/// The set: every call and its link name (one home for the spelling). `tbl1` is at the two
/// shapes the library declares it at; the shifts at every integer shape; the pairwise
/// maximum and minimum at the six the library declares (no 64-bit lanes: A64 has none).
const NAMES: [(NeonCall, &str); 42] = [
    (NeonCall { func: NeonFn::Tbl1, shape: IntShape::V8I8 }, "llvm.aarch64.neon.tbl1.v8i8"),
    (NeonCall { func: NeonFn::Tbl1, shape: IntShape::V16I8 }, "llvm.aarch64.neon.tbl1.v16i8"),
    (NeonCall { func: NeonFn::Ushl, shape: IntShape::V8I8 }, "llvm.aarch64.neon.ushl.v8i8"),
    (NeonCall { func: NeonFn::Ushl, shape: IntShape::V16I8 }, "llvm.aarch64.neon.ushl.v16i8"),
    (NeonCall { func: NeonFn::Ushl, shape: IntShape::V4I16 }, "llvm.aarch64.neon.ushl.v4i16"),
    (NeonCall { func: NeonFn::Ushl, shape: IntShape::V8I16 }, "llvm.aarch64.neon.ushl.v8i16"),
    (NeonCall { func: NeonFn::Ushl, shape: IntShape::V2I32 }, "llvm.aarch64.neon.ushl.v2i32"),
    (NeonCall { func: NeonFn::Ushl, shape: IntShape::V4I32 }, "llvm.aarch64.neon.ushl.v4i32"),
    (NeonCall { func: NeonFn::Ushl, shape: IntShape::V1I64 }, "llvm.aarch64.neon.ushl.v1i64"),
    (NeonCall { func: NeonFn::Ushl, shape: IntShape::V2I64 }, "llvm.aarch64.neon.ushl.v2i64"),
    (NeonCall { func: NeonFn::Sshl, shape: IntShape::V8I8 }, "llvm.aarch64.neon.sshl.v8i8"),
    (NeonCall { func: NeonFn::Sshl, shape: IntShape::V16I8 }, "llvm.aarch64.neon.sshl.v16i8"),
    (NeonCall { func: NeonFn::Sshl, shape: IntShape::V4I16 }, "llvm.aarch64.neon.sshl.v4i16"),
    (NeonCall { func: NeonFn::Sshl, shape: IntShape::V8I16 }, "llvm.aarch64.neon.sshl.v8i16"),
    (NeonCall { func: NeonFn::Sshl, shape: IntShape::V2I32 }, "llvm.aarch64.neon.sshl.v2i32"),
    (NeonCall { func: NeonFn::Sshl, shape: IntShape::V4I32 }, "llvm.aarch64.neon.sshl.v4i32"),
    (NeonCall { func: NeonFn::Sshl, shape: IntShape::V1I64 }, "llvm.aarch64.neon.sshl.v1i64"),
    (NeonCall { func: NeonFn::Sshl, shape: IntShape::V2I64 }, "llvm.aarch64.neon.sshl.v2i64"),
    (NeonCall { func: NeonFn::Umaxp, shape: IntShape::V8I8 }, "llvm.aarch64.neon.umaxp.v8i8"),
    (NeonCall { func: NeonFn::Umaxp, shape: IntShape::V16I8 }, "llvm.aarch64.neon.umaxp.v16i8"),
    (NeonCall { func: NeonFn::Umaxp, shape: IntShape::V4I16 }, "llvm.aarch64.neon.umaxp.v4i16"),
    (NeonCall { func: NeonFn::Umaxp, shape: IntShape::V8I16 }, "llvm.aarch64.neon.umaxp.v8i16"),
    (NeonCall { func: NeonFn::Umaxp, shape: IntShape::V2I32 }, "llvm.aarch64.neon.umaxp.v2i32"),
    (NeonCall { func: NeonFn::Umaxp, shape: IntShape::V4I32 }, "llvm.aarch64.neon.umaxp.v4i32"),
    (NeonCall { func: NeonFn::Uminp, shape: IntShape::V8I8 }, "llvm.aarch64.neon.uminp.v8i8"),
    (NeonCall { func: NeonFn::Uminp, shape: IntShape::V16I8 }, "llvm.aarch64.neon.uminp.v16i8"),
    (NeonCall { func: NeonFn::Uminp, shape: IntShape::V4I16 }, "llvm.aarch64.neon.uminp.v4i16"),
    (NeonCall { func: NeonFn::Uminp, shape: IntShape::V8I16 }, "llvm.aarch64.neon.uminp.v8i16"),
    (NeonCall { func: NeonFn::Uminp, shape: IntShape::V2I32 }, "llvm.aarch64.neon.uminp.v2i32"),
    (NeonCall { func: NeonFn::Uminp, shape: IntShape::V4I32 }, "llvm.aarch64.neon.uminp.v4i32"),
    (NeonCall { func: NeonFn::Smaxp, shape: IntShape::V8I8 }, "llvm.aarch64.neon.smaxp.v8i8"),
    (NeonCall { func: NeonFn::Smaxp, shape: IntShape::V16I8 }, "llvm.aarch64.neon.smaxp.v16i8"),
    (NeonCall { func: NeonFn::Smaxp, shape: IntShape::V4I16 }, "llvm.aarch64.neon.smaxp.v4i16"),
    (NeonCall { func: NeonFn::Smaxp, shape: IntShape::V8I16 }, "llvm.aarch64.neon.smaxp.v8i16"),
    (NeonCall { func: NeonFn::Smaxp, shape: IntShape::V2I32 }, "llvm.aarch64.neon.smaxp.v2i32"),
    (NeonCall { func: NeonFn::Smaxp, shape: IntShape::V4I32 }, "llvm.aarch64.neon.smaxp.v4i32"),
    (NeonCall { func: NeonFn::Sminp, shape: IntShape::V8I8 }, "llvm.aarch64.neon.sminp.v8i8"),
    (NeonCall { func: NeonFn::Sminp, shape: IntShape::V16I8 }, "llvm.aarch64.neon.sminp.v16i8"),
    (NeonCall { func: NeonFn::Sminp, shape: IntShape::V4I16 }, "llvm.aarch64.neon.sminp.v4i16"),
    (NeonCall { func: NeonFn::Sminp, shape: IntShape::V8I16 }, "llvm.aarch64.neon.sminp.v8i16"),
    (NeonCall { func: NeonFn::Sminp, shape: IntShape::V2I32 }, "llvm.aarch64.neon.sminp.v2i32"),
    (NeonCall { func: NeonFn::Sminp, shape: IntShape::V4I32 }, "llvm.aarch64.neon.sminp.v4i32"),
];

/// Every call of the set, in [`NAMES`]' order.
pub fn all() -> impl Iterator<Item = NeonCall> {
    NAMES.iter().map(|&(call, _)| call)
}

/// The NEON integer intrinsic whose link name is exactly `link_name`, if any.
pub fn named(link_name: &str) -> Option<NeonCall> {
    NAMES.iter().find(|&&(_, name)| name == link_name).map(|&(call, _)| call)
}

impl IntShape {
    /// The lane's LIR scalar.
    pub fn elem(self) -> Scalar {
        match self {
            IntShape::V8I8 | IntShape::V16I8 => Scalar::I8,
            IntShape::V4I16 | IntShape::V8I16 => Scalar::I16,
            IntShape::V2I32 | IntShape::V4I32 => Scalar::I32,
            IntShape::V1I64 | IntShape::V2I64 => Scalar::I64,
        }
    }

    /// The lane's width in bits.
    pub fn width(self) -> u32 {
        match self.elem() {
            Scalar::I8 => 8,
            Scalar::I16 => 16,
            Scalar::I32 => 32,
            _ => 64,
        }
    }

    /// The number of lanes.
    pub fn lanes(self) -> u32 {
        match self {
            IntShape::V1I64 => 1,
            IntShape::V2I32 | IntShape::V2I64 => 2,
            IntShape::V4I16 | IntShape::V4I32 => 4,
            IntShape::V8I8 | IntShape::V8I16 => 8,
            IntShape::V16I8 => 16,
        }
    }

    /// The bytes a value of the shape holds: 8 or 16 (a D or Q register).
    pub fn bytes(self) -> u64 {
        u64::from(self.lanes() * self.width() / 8)
    }

    /// The LIR vector type `<lanes x elem>`.
    pub fn ty(self) -> Ty {
        // Every shape is a power-of-two lane count of at most 128 bits, which LIR has
        // (`Ty::vector`: at most `VECTOR_BITS`).
        Ty::vector(self.elem(), self.lanes()).expect("a NEON shape is an LIR vector")
    }
}

impl NeonCall {
    pub fn func(self) -> NeonFn {
        self.func
    }

    pub fn shape(self) -> IntShape {
        self.shape
    }

    /// The `link_name` the library declares it by.
    pub fn link_name(self) -> &'static str {
        // A `NeonCall` is made only from a row of `NAMES` (its fields are private).
        NAMES.iter().find(|&&(call, _)| call == self).map(|&(_, name)| name).expect("every call is a row of `NAMES`")
    }

    /// The shapes of its two operands, in order; the result is of [`NeonCall::shape`].
    pub fn operands(self) -> [IntShape; 2] {
        match self.func {
            // The table is always 16 bytes; the indices are of the result's shape.
            NeonFn::Tbl1 => [IntShape::V16I8, self.shape],
            NeonFn::Ushl | NeonFn::Sshl | NeonFn::Umaxp | NeonFn::Uminp | NeonFn::Smaxp | NeonFn::Sminp => [self.shape, self.shape],
        }
    }
}

/// `call` of the operands `a` and `x`, LIR vectors of [`NeonCall::operands`]' types: the result,
/// an LIR vector of the call's shape. See the module doc for each function's rule.
pub fn build<A: Allocator + Clone>(b: &mut Builder<A, Full>, call: NeonCall, a: Value, x: Value) -> Value {
    match call.func {
        NeonFn::Tbl1 => table_lookup(b, call.shape, a, x),
        NeonFn::Ushl => shift(b, call.shape, false, a, x),
        NeonFn::Sshl => shift(b, call.shape, true, a, x),
        NeonFn::Umaxp => pairwise(b, call.shape, Op::Umax, a, x),
        NeonFn::Uminp => pairwise(b, call.shape, Op::Umin, a, x),
        NeonFn::Smaxp => pairwise(b, call.shape, Op::Smax, a, x),
        NeonFn::Sminp => pairwise(b, call.shape, Op::Smin, a, x),
    }
}

/// The most lanes of a shape ([`IntShape::V16I8`]).
const MOST_LANES: usize = 16;

/// `umaxp`/`uminp`/`smaxp`/`sminp` of `a` and `x`, both of `shape`: `op` (`umax`, `umin`, `smax`
/// or `smin`) of each adjacent pair of `a`'s lanes followed by `x`'s. Arm's pseudocode: `concat
/// = operand2:operand1; Elem[result, e] = Max(Elem[concat, 2*e], Elem[concat, 2*e+1])`, the
/// operands read unsigned or signed by the name; so the even lanes of the concatenation against
/// the odd ones, each a `shuffle` (whose mask indexes `a`'s lanes, then `x`'s).
fn pairwise<A: Allocator + Clone>(b: &mut Builder<A, Full>, shape: IntShape, op: Op, a: Value, x: Value) -> Value {
    let n = shape.lanes() as usize;
    let mut even = [0u32; MOST_LANES];
    let mut odd = [0u32; MOST_LANES];
    for e in 0..n {
        even[e] = 2 * e as u32;
        odd[e] = 2 * e as u32 + 1;
    }
    let ty = shape.ty();
    let first = b.shuffle(ty, a, x, &even[..n]);
    let second = b.shuffle(ty, a, x, &odd[..n]);
    b.binary(op, Flags::NONE, first, second)
}

/// `tbl1`: `table` a `<16 x i8>`, `index` of `shape` (`V8I8` or `V16I8`, the rows of [`NAMES`]).
fn table_lookup<A: Allocator + Clone>(b: &mut Builder<A, Full>, shape: IntShape, table: Value, index: Value) -> Value {
    let wide = IntShape::V16I8.ty();
    match shape {
        IntShape::V16I8 => b.binary(Op::Swizzle, Flags::NONE, table, index),
        // Eight indices: widened to sixteen (lanes 8..16 repeat 0..8, so every widened lane is
        // an index of the call), looked up, and the low eight lanes kept.
        _ => {
            const LOW: [u32; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
            const TWICE: [u32; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7];
            let widened = b.shuffle(wide, index, index, &TWICE);
            let looked = b.binary(Op::Swizzle, Flags::NONE, table, widened);
            b.shuffle(shape.ty(), looked, looked, &LOW)
        }
    }
}

/// `ushl` (`signed` false) or `sshl` (`signed` true) of `a` by `by`, both of `shape`.
fn shift<A: Allocator + Clone>(b: &mut Builder<A, Full>, shape: IntShape, signed: bool, a: Value, by: Value) -> Value {
    let ty = shape.ty();
    let lane = Ty::scalar(shape.elem());
    let width = shape.width();
    let splat = |b: &mut Builder<A, Full>, bits: u128| {
        let c = b.iconst(lane, bits);
        b.convert(Op::Splat, Flags::NONE, ty, c)
    };
    // The amount: the signed least significant byte of each lane of `by`.
    let amount = match shape.elem() {
        Scalar::I8 => by,
        _ => {
            let bytes = Ty::vector(Scalar::I8, shape.lanes()).expect("a NEON shape's lane count of bytes is an LIR vector");
            let low = b.convert(Op::Trunc, Flags::NONE, bytes, by);
            b.convert(Op::Sext, Flags::NONE, ty, low)
        }
    };
    let zero = splat(b, 0);
    // `iconst` truncates to the lane: `-width` in two's complement.
    let (up_to, down_to) = (splat(b, u128::from(width)), splat(b, u128::from(width).wrapping_neg()));
    let left = b.binary(Op::Shl, Flags::NONE, a, amount);
    // `0 - amount`, the right shift's amount; at `-128` in an `i8` lane it wraps, but that lane
    // is past `-width` and chosen below.
    let negated = b.binary(Op::Sub, Flags::NONE, zero, amount);
    let right_op = if signed { Op::Ashr } else { Op::Lshr };
    let right = b.binary(right_op, Flags::NONE, a, negated);
    let is_left = b.icmp(IntCond::Sge, amount, zero);
    let shifted = b.select(Flags::NONE, is_left, left, right);
    // A right shift by the width or more: every bit shifted out (0), or the sign in every bit.
    let fill = if signed {
        let top = splat(b, u128::from(width - 1));
        b.binary(Op::Ashr, Flags::NONE, a, top)
    } else {
        zero
    };
    let past_right = b.icmp(IntCond::Sle, amount, down_to);
    let kept = b.select(Flags::NONE, past_right, fill, shifted);
    // A left shift by the width or more: every bit shifted out.
    let past_left = b.icmp(IntCond::Sge, amount, up_to);
    b.select(Flags::NONE, past_left, zero, kept)
}
