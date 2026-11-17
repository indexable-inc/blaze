//! The AArch64 SHA3 extension's four bitwise intrinsics (FEAT_SHA3): `eor3`, `bcax`, `rax1` and
//! `xar`, each a short fixed sequence of LIR vector operations (INTRINSICS.md 3.3: a target name
//! with an LIR op maps to it). Keccak-f[1600] on aarch64 (the `keccak` and `sha3` crates behind
//! `cpufeatures`, reached by tempo through alloy's `keccak256`) is written with them.
//!
//! The pinned library declares them in `extern "llvm-intrinsic"` blocks of stdarch
//! `aarch64/neon/generated.rs`, under `#[target_feature(enable = "neon,sha3")]`:
//! - `veor3q_{u,s}{8,16,32,64}` (`link_name = "llvm.aarch64.crypto.eor3{u,s}.<shape>"`,
//!   `fn(a, b, c) -> _` at `v16i8`, `v8i16`, `v4i32`, `v2i64`);
//! - `vbcaxq_{u,s}{8,16,32,64}` (`"llvm.aarch64.crypto.bcax{u,s}.<shape>"`, the same shapes);
//! - `vrax1q_u64` (`"llvm.aarch64.crypto.rax1"`, `fn(a: uint64x2_t, b: uint64x2_t)`);
//! - `vxarq_u64::<IMM6>` (`"llvm.aarch64.crypto.xar"`, `fn(a, b, n: i64)`, called with
//!   `IMM6 as i64` after `static_assert_uimm_bits!(IMM6, 6)`).
//!
//! What each is, from the Arm A64 instruction set (the instructions the names lower to):
//! - `EOR3`: "Three-way Exclusive OR performs a three-way exclusive OR of the values in the three
//!   source SIMD&FP registers": `a ^ b ^ c`. Bitwise, so the signed and unsigned names and every
//!   lane width are the same bits.
//! - `BCAX`: "Bit Clear and Exclusive OR performs a bitwise AND of the 128-bit vector in a source
//!   SIMD&FP register and the complement of the vector in another source SIMD&FP register, then
//!   performs a bitwise exclusive OR of the resulting vector and the vector in a third source":
//!   `vbcaxq(a, b, c)` is `a ^ (b & !c)` (ACLE: "`a EOR (b AND NOT c)`").
//! - `RAX1`: "Rotate and Exclusive OR rotates each 64-bit element of the 128-bit vector in a
//!   source SIMD&FP register left by 1, performs a bitwise exclusive OR of the resulting 128-bit
//!   vector and the vector in another source": `vrax1q_u64(a, b)` is `a ^ rotl(b, 1)` lane by lane.
//! - `XAR`: "Exclusive OR and Rotate performs a bitwise exclusive OR of the 128-bit vectors in the
//!   two source SIMD&FP registers, rotates each 64-bit element of the resulting 128-bit vector
//!   right by the value specified by a 6-bit immediate value": `rotr(a ^ b, imm)` lane by lane.
//!   The immediate is encoded in the instruction, so it must be a constant below 64; anything else
//!   is refused by name.
//!
//! None of them has undefined behaviour: every input is a valid vector of bits. The Reference and
//! Fast tiers take the function after blaze-lir's legalize, which makes each lane by lane.

use core::alloc::Allocator;

use blaze_lir::{Builder, Flags, Full, Op, Ty, Value};

use super::neon::IntShape;

/// A function of the SHA3 extension.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sha3Fn {
    /// `eor3`: three-way exclusive or.
    Eor3,
    /// `bcax`: bit clear and exclusive or, `a ^ (b & !c)`.
    Bcax,
    /// `rax1`: `a ^ rotl(b, 1)` at 64-bit lanes.
    Rax1,
    /// `xar`: `rotr(a ^ b, imm)` at 64-bit lanes, `imm` a constant below 64.
    Xar,
}

/// A call of a SHA3 intrinsic: the function at a shape. Made only from [`NAMES`]' rows
/// ([`named`], [`all`]), so every value has its link name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sha3Call {
    func: Sha3Fn,
    shape: IntShape,
    /// The `s` spelling of `eor3`/`bcax` (the same bits as `u`; kept so the link name is exact).
    signed: bool,
}

const fn row(func: Sha3Fn, shape: IntShape, signed: bool) -> Sha3Call {
    Sha3Call { func, shape, signed }
}

/// The set: every call and its link name (one home for the spelling).
const NAMES: [(Sha3Call, &str); 18] = [
    (row(Sha3Fn::Eor3, IntShape::V16I8, false), "llvm.aarch64.crypto.eor3u.v16i8"),
    (row(Sha3Fn::Eor3, IntShape::V8I16, false), "llvm.aarch64.crypto.eor3u.v8i16"),
    (row(Sha3Fn::Eor3, IntShape::V4I32, false), "llvm.aarch64.crypto.eor3u.v4i32"),
    (row(Sha3Fn::Eor3, IntShape::V2I64, false), "llvm.aarch64.crypto.eor3u.v2i64"),
    (row(Sha3Fn::Eor3, IntShape::V16I8, true), "llvm.aarch64.crypto.eor3s.v16i8"),
    (row(Sha3Fn::Eor3, IntShape::V8I16, true), "llvm.aarch64.crypto.eor3s.v8i16"),
    (row(Sha3Fn::Eor3, IntShape::V4I32, true), "llvm.aarch64.crypto.eor3s.v4i32"),
    (row(Sha3Fn::Eor3, IntShape::V2I64, true), "llvm.aarch64.crypto.eor3s.v2i64"),
    (row(Sha3Fn::Bcax, IntShape::V16I8, false), "llvm.aarch64.crypto.bcaxu.v16i8"),
    (row(Sha3Fn::Bcax, IntShape::V8I16, false), "llvm.aarch64.crypto.bcaxu.v8i16"),
    (row(Sha3Fn::Bcax, IntShape::V4I32, false), "llvm.aarch64.crypto.bcaxu.v4i32"),
    (row(Sha3Fn::Bcax, IntShape::V2I64, false), "llvm.aarch64.crypto.bcaxu.v2i64"),
    (row(Sha3Fn::Bcax, IntShape::V16I8, true), "llvm.aarch64.crypto.bcaxs.v16i8"),
    (row(Sha3Fn::Bcax, IntShape::V8I16, true), "llvm.aarch64.crypto.bcaxs.v8i16"),
    (row(Sha3Fn::Bcax, IntShape::V4I32, true), "llvm.aarch64.crypto.bcaxs.v4i32"),
    (row(Sha3Fn::Bcax, IntShape::V2I64, true), "llvm.aarch64.crypto.bcaxs.v2i64"),
    (row(Sha3Fn::Rax1, IntShape::V2I64, false), "llvm.aarch64.crypto.rax1"),
    (row(Sha3Fn::Xar, IntShape::V2I64, false), "llvm.aarch64.crypto.xar"),
];

/// Every call of the set, in [`NAMES`]' order.
pub fn all() -> impl Iterator<Item = Sha3Call> {
    NAMES.iter().map(|&(call, _)| call)
}

/// The SHA3 intrinsic whose link name is exactly `link_name`, if any.
pub fn named(link_name: &str) -> Option<Sha3Call> {
    NAMES.iter().find(|&&(_, name)| name == link_name).map(|&(call, _)| call)
}

/// Why a SHA3 call's immediate was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImmRefused {
    /// `xar`'s rotate amount is not a constant (the instruction encodes it).
    NotConstant,
    /// `xar`'s rotate amount is 64 or more (`static_assert_uimm_bits!(IMM6, 6)`).
    PastSixBits,
}

impl ImmRefused {
    pub fn why(self) -> &'static str {
        match self {
            ImmRefused::NotConstant => "a rotate amount that is not a constant (`xar` encodes it in the instruction)",
            ImmRefused::PastSixBits => "a rotate amount of 64 or more (`vxarq_u64`'s `IMM6` is 6 bits)",
        }
    }
}

impl Sha3Call {
    pub fn func(self) -> Sha3Fn {
        self.func
    }

    /// The shape of every vector operand and of the result.
    pub fn shape(self) -> IntShape {
        self.shape
    }

    /// The `link_name` the library declares it by.
    pub fn link_name(self) -> &'static str {
        // A `Sha3Call` is made only from a row of `NAMES` (its fields are private).
        NAMES.iter().find(|&&(call, _)| call == self).map(|&(_, name)| name).expect("every call is a row of `NAMES`")
    }

    /// How many vector operands it takes (each of [`Sha3Call::shape`]).
    pub fn vectors(self) -> usize {
        match self.func {
            Sha3Fn::Eor3 | Sha3Fn::Bcax => 3,
            Sha3Fn::Rax1 | Sha3Fn::Xar => 2,
        }
    }

    /// Whether a scalar `i64` immediate follows the vectors (`xar`'s rotate amount).
    pub fn takes_imm(self) -> bool {
        self.func == Sha3Fn::Xar
    }
}

/// The most integer conversions [`rotate_amount`] looks through (`IMM6 as i64` is one `sext` of
/// the `i32` constant).
const MOST_CONVERSIONS: usize = 4;

/// `xar`'s rotate amount from the value the walk built for `IMM6 as i64`: an `iconst`, or a
/// `sext`/`zext`/`trunc` of one (at most [`MOST_CONVERSIONS`] deep), below 64. A value below 64
/// is the same at every integer width (`i8` and up) and under each of the three, so the
/// constant's own value, unsigned at its own width, is the amount; a negative one reads as
/// 2^(width-1) or more and is refused (a `trunc` of a wide constant to below 64 is refused too:
/// a refusal, never a wrong amount).
pub fn rotate_amount<A: Allocator + Clone>(b: &Builder<A, Full>, imm: Value) -> Result<u32, ImmRefused> {
    let view = b.func().view();
    if b.ty(imm) != Ty::I64 {
        return Err(ImmRefused::NotConstant);
    }
    let mut i = imm.index();
    for _ in 0..=MOST_CONVERSIONS {
        if i >= view.len() {
            return Err(ImmRefused::NotConstant);
        }
        match view.op(i) {
            // blaze-lir builder.rs `iconst`: one word, the value itself at 32 bits or fewer, else a
            // value that fits an `i32`, sign-extended; any other is an `iconst_wide`.
            Op::Iconst => {
                let word = view.operands(i).word();
                let bits = view.ty(Value(i as u32)).bits(64);
                let amount = if bits <= 32 { u64::from(word) } else { word as i32 as i64 as u64 };
                if amount >= 64 {
                    return Err(ImmRefused::PastSixBits);
                }
                return Ok(amount as u32);
            }
            // A constant that does not fit an `i32`: 2^31 or more, or negative.
            Op::IconstWide => return Err(ImmRefused::PastSixBits),
            Op::Sext | Op::Zext | Op::Trunc => i = view.operands(i).value().index(),
            _ => return Err(ImmRefused::NotConstant),
        }
    }
    Err(ImmRefused::NotConstant)
}

/// `call` of `ops` (its [`Sha3Call::vectors`] LIR vectors of its shape) and, for `xar`, the
/// rotate amount `imm` (from [`rotate_amount`]): the result, an LIR vector of its shape. See the
/// module doc for each function's rule. `None` (nothing built) when `ops` is not
/// [`Sha3Call::vectors`] long.
pub fn build<A: Allocator + Clone>(b: &mut Builder<A, Full>, call: Sha3Call, ops: &[Value], imm: u32) -> Option<Value> {
    if ops.len() != call.vectors() {
        return None;
    }
    let ty = call.shape.ty();
    let lane = Ty::scalar(call.shape.elem());
    let splat = |b: &mut Builder<A, Full>, bits: u128| {
        let c = b.iconst(lane, bits);
        b.convert(Op::Splat, Flags::NONE, ty, c)
    };
    Some(match (call.func, ops) {
        (Sha3Fn::Eor3, &[a, x, c]) => {
            let ax = b.binary(Op::Xor, Flags::NONE, a, x);
            b.binary(Op::Xor, Flags::NONE, ax, c)
        }
        (Sha3Fn::Bcax, &[a, x, c]) => {
            // `iconst` truncates to the lane: all ones.
            let ones = splat(b, u128::MAX);
            let not_c = b.binary(Op::Xor, Flags::NONE, c, ones);
            let kept = b.binary(Op::And, Flags::NONE, x, not_c);
            b.binary(Op::Xor, Flags::NONE, a, kept)
        }
        (Sha3Fn::Rax1, &[a, x]) => {
            let one = splat(b, 1);
            let rotated = b.binary(Op::Rotl, Flags::NONE, x, one);
            b.binary(Op::Xor, Flags::NONE, a, rotated)
        }
        (Sha3Fn::Xar, &[a, x]) => {
            let mixed = b.binary(Op::Xor, Flags::NONE, a, x);
            if imm == 0 {
                mixed
            } else {
                let by = splat(b, u128::from(imm));
                b.binary(Op::Rotr, Flags::NONE, mixed, by)
            }
        }
        // The count was checked above: every function's arm takes its `vectors()` operands.
        _ => return None,
    })
}
