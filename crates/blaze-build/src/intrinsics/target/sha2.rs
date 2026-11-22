//! The AArch64 SHA-256 and SHA-512 intrinsics (FEAT_SHA256, FEAT_SHA512): the hash updates
//! `sha256h`, `sha256h2`, `sha512h`, `sha512h2` and the schedule updates `sha256su0`,
//! `sha256su1`, `sha512su0`, `sha512su1`, each built as LIR scalar operations on the lanes of its
//! operands (an `extract` per lane read, an `insert` per lane written), from Arm's pseudocode.
//! The `sha2` crate's aarch64 backends (reached by tempo through `sha2` behind `cpufeatures`)
//! are written with them.
//!
//! Why LIR and not the instruction: the emitter has no vector registers yet (intrinsics/simd.rs),
//! and the Reference tier must compute the same bits; four rounds of SHA-256 are about 120 LIR
//! scalar operations, which the Fast tier compiles in proportion. When the emitter gains `SHA256H`
//! and kin, they become one instruction each at that seam, and this module stays the reference.
//!
//! The pinned library declares them in `extern "unadjusted"` blocks of stdarch:
//! - `arm_shared/neon/generated.rs` `vsha256hq_u32(hash_abcd, hash_efgh, wk)`
//!   (`link_name = "llvm.aarch64.crypto.sha256h"`), `vsha256h2q_u32(hash_efgh, hash_abcd, wk)`
//!   (`"llvm.aarch64.crypto.sha256h2"`), `vsha256su0q_u32(w0_3, w4_7)`
//!   (`"llvm.aarch64.crypto.sha256su0"`), `vsha256su1q_u32(tw0_3, w8_11, w12_15)`
//!   (`"llvm.aarch64.crypto.sha256su1"`), every operand a `uint32x4_t`;
//! - `aarch64/neon/generated.rs` `vsha512hq_u64(a, b, c)` (`"llvm.aarch64.crypto.sha512h"`),
//!   `vsha512h2q_u64(a, b, c)` (`"llvm.aarch64.crypto.sha512h2"`), `vsha512su0q_u64(a, b)`
//!   (`"llvm.aarch64.crypto.sha512su0"`), `vsha512su1q_u64(a, b, c)`
//!   (`"llvm.aarch64.crypto.sha512su1"`), every operand a `uint64x2_t`.
//!
//! The operands are, in order, the instruction's `Vd` (read and written), `Vn` and `Vm`. Arm's
//! pseudocode (lane `i` is `Elem[v, i]`, bits `32i+31:32i` or `64i+63:64i`):
//! - `SHA256H`: `SHA256hash(X = Vd, Y = Vn, W = Vm, part1 = TRUE)`; `SHA256H2`:
//!   `SHA256hash(X = Vn, Y = Vd, W = Vm, part1 = FALSE)`. `SHA256hash` runs four rounds, `for e
//!   = 0 to 3 { chs = SHAchoose(Y[0], Y[1], Y[2]); maj = SHAmajority(X[0], X[1], X[2]); t = Y[3]
//!   + SHAhashSIGMA1(Y[0]) + chs + W[e]; X[3] = t + X[3]; Y[3] = t + SHAhashSIGMA0(X[0]) + maj;
//!   <Y, X> = ROL(Y : X, 32) }`, and returns `X` (part 1) or `Y`. With `X = [a, b, c, d]` and
//!   `Y = [e, f, g, h]` that is FIPS 180-4's round four times (the rotation is its renaming),
//!   built so: `choose(x, y, z) = ((y ^ z) & x) ^ z`, `majority(x, y, z) = (x & y) | ((x | y) &
//!   z)`, `SIGMA0(x) = ror(x, 2) ^ ror(x, 13) ^ ror(x, 22)`, `SIGMA1(x) = ror(x, 6) ^ ror(x, 11)
//!   ^ ror(x, 25)`.
//! - `SHA256SU0`: `T = Vn[0] : Vd[3..1]`; `result[e] = (ror(T[e], 7) ^ ror(T[e], 18) ^ (T[e] >>
//!   3)) + Vd[e]`.
//! - `SHA256SU1`: `T0 = Vm[0] : Vn[3..1]`, `T1 = Vm[3..2]`; for `e` 0 and 1, `result[e] =
//!   sigma1(T1[e]) + Vd[e] + T0[e]`; then `T1 = result[1..0]` and for `e` 2 and 3 the same with
//!   `T1[e - 2]`; `sigma1(x) = ror(x, 17) ^ ror(x, 19) ^ (x >> 10)`.
//! - `SHA512H` (`W = Vd, X = Vn, Y = Vm`): `hi = ((Y[1] & X[0]) ^ (!Y[1] & X[1])) + Sigma1(Y[1])
//!   + W[1]`; `tmp = hi + Y[0]`; `lo = ((tmp & Y[1]) ^ (!tmp & X[0])) + Sigma1(tmp) + W[0]`;
//!   `Sigma1(x) = ror(x, 14) ^ ror(x, 18) ^ ror(x, 41)`.
//! - `SHA512H2` (`W = Vd, X = Vn, Y = Vm`): `hi = ((X[0] & Y[1]) ^ (X[0] & Y[0]) ^ (Y[1] &
//!   Y[0])) + Sigma0(Y[0]) + W[1]`; `lo = ((hi & Y[0]) ^ (hi & Y[1]) ^ (Y[1] & Y[0])) +
//!   Sigma0(hi) + W[0]`; `Sigma0(x) = ror(x, 28) ^ ror(x, 34) ^ ror(x, 39)`.
//! - `SHA512SU0` (`W = Vd, X = Vn`): `lo = W[0] + sigma0(W[1])`, `hi = W[1] + sigma0(X[0])`;
//!   `sigma0(x) = ror(x, 1) ^ ror(x, 8) ^ (x >> 7)`.
//! - `SHA512SU1` (`W = Vd, X = Vn, Y = Vm`): `hi = W[1] + sigma1(X[1]) + Y[1]`, `lo = W[0] +
//!   sigma1(X[0]) + Y[0]`; `sigma1(x) = ror(x, 19) ^ ror(x, 61) ^ (x >> 6)`.
//!
//! Every addition wraps (the pseudocode's `+` on bit strings). None has undefined behaviour:
//! every input is a valid vector of bits.

use core::alloc::Allocator;

use blaze_lir::{Builder, Flags, Full, Op, Ty, Value};

use super::neon::IntShape;

/// A function of the SHA-256 and SHA-512 extensions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sha2Fn {
    Sha256h,
    Sha256h2,
    Sha256su0,
    Sha256su1,
    Sha512h,
    Sha512h2,
    Sha512su0,
    Sha512su1,
}

/// The set: every function and its link name (one home for the spelling).
const NAMES: [(Sha2Fn, &str); 8] = [
    (Sha2Fn::Sha256h, "llvm.aarch64.crypto.sha256h"),
    (Sha2Fn::Sha256h2, "llvm.aarch64.crypto.sha256h2"),
    (Sha2Fn::Sha256su0, "llvm.aarch64.crypto.sha256su0"),
    (Sha2Fn::Sha256su1, "llvm.aarch64.crypto.sha256su1"),
    (Sha2Fn::Sha512h, "llvm.aarch64.crypto.sha512h"),
    (Sha2Fn::Sha512h2, "llvm.aarch64.crypto.sha512h2"),
    (Sha2Fn::Sha512su0, "llvm.aarch64.crypto.sha512su0"),
    (Sha2Fn::Sha512su1, "llvm.aarch64.crypto.sha512su1"),
];

/// Every function of the set, in [`NAMES`]' order.
pub fn all() -> impl Iterator<Item = Sha2Fn> {
    NAMES.iter().map(|&(f, _)| f)
}

/// The SHA-2 intrinsic whose link name is exactly `link_name`, if any.
pub fn named(link_name: &str) -> Option<Sha2Fn> {
    NAMES.iter().find(|&&(_, name)| name == link_name).map(|&(f, _)| f)
}

impl Sha2Fn {
    /// The `link_name` the library declares it by.
    pub fn link_name(self) -> &'static str {
        // Every variant has its row (tested by `every_sha2_name_round_trips`).
        NAMES.iter().find(|&&(f, _)| f == self).map(|&(_, name)| name).expect("every function is a row of `NAMES`")
    }

    /// The shape of every operand and of the result: four 32-bit lanes for SHA-256, two 64-bit
    /// lanes for SHA-512.
    pub fn shape(self) -> IntShape {
        match self {
            Sha2Fn::Sha256h | Sha2Fn::Sha256h2 | Sha2Fn::Sha256su0 | Sha2Fn::Sha256su1 => IntShape::V4I32,
            Sha2Fn::Sha512h | Sha2Fn::Sha512h2 | Sha2Fn::Sha512su0 | Sha2Fn::Sha512su1 => IntShape::V2I64,
        }
    }

    /// How many vector operands it takes.
    pub fn vectors(self) -> usize {
        match self {
            Sha2Fn::Sha256su0 | Sha2Fn::Sha512su0 => 2,
            _ => 3,
        }
    }
}

/// Scalar operations at one lane type, appended to the builder.
struct Lanes<'b, A: Allocator + Clone> {
    b: &'b mut Builder<A, Full>,
    ty: Ty,
}

impl<A: Allocator + Clone> Lanes<'_, A> {
    fn op(&mut self, op: Op, x: Value, y: Value) -> Value {
        self.b.binary(op, Flags::NONE, x, y)
    }

    fn add(&mut self, x: Value, y: Value) -> Value {
        self.op(Op::Add, x, y)
    }

    fn add3(&mut self, x: Value, y: Value, z: Value) -> Value {
        let xy = self.add(x, y);
        self.add(xy, z)
    }

    fn and(&mut self, x: Value, y: Value) -> Value {
        self.op(Op::And, x, y)
    }

    fn xor(&mut self, x: Value, y: Value) -> Value {
        self.op(Op::Xor, x, y)
    }

    fn not(&mut self, x: Value) -> Value {
        // `iconst` truncates to the lane: all ones.
        let ones = self.b.iconst(self.ty, u128::MAX);
        self.xor(x, ones)
    }

    fn ror(&mut self, x: Value, n: u32) -> Value {
        let by = self.b.iconst(self.ty, u128::from(n));
        self.op(Op::Rotr, x, by)
    }

    /// `ror(x, r0) ^ ror(x, r1) ^ ror(x, r2)`.
    fn three_rotations(&mut self, x: Value, [r0, r1, r2]: [u32; 3]) -> Value {
        let (a, b, c) = (self.ror(x, r0), self.ror(x, r1), self.ror(x, r2));
        let ab = self.xor(a, b);
        self.xor(ab, c)
    }

    /// `ror(x, r0) ^ ror(x, r1) ^ (x >> shift)`.
    fn small_sigma(&mut self, x: Value, r0: u32, r1: u32, shift: u32) -> Value {
        let (a, b) = (self.ror(x, r0), self.ror(x, r1));
        let by = self.b.iconst(self.ty, u128::from(shift));
        let c = self.op(Op::Lshr, x, by);
        let ab = self.xor(a, b);
        self.xor(ab, c)
    }

    /// The lanes of vector `v`.
    fn lanes<const N: usize>(&mut self, v: Value) -> [Value; N] {
        core::array::from_fn(|i| self.b.extract(v, i as u32))
    }

    /// A vector of `like`'s type whose lanes are `lanes`.
    fn vector<const N: usize>(&mut self, like: Value, lanes: [Value; N]) -> Value {
        let mut v = like;
        for (i, lane) in lanes.into_iter().enumerate() {
            v = self.b.insert(v, lane, i as u32);
        }
        v
    }
}

/// `f` of `ops` (its [`Sha2Fn::vectors`] LIR vectors of its shape, `Vd` first): the result, an
/// LIR vector of its shape. See the module doc for each rule. `None` (nothing built) when `ops`
/// is not [`Sha2Fn::vectors`] long.
pub fn build<A: Allocator + Clone>(b: &mut Builder<A, Full>, f: Sha2Fn, ops: &[Value]) -> Option<Value> {
    if ops.len() != f.vectors() {
        return None;
    }
    let ty = Ty::scalar(f.shape().elem());
    let mut l = Lanes { b, ty };
    Some(match (f, ops) {
        (Sha2Fn::Sha256h | Sha2Fn::Sha256h2, &[d, n, m]) => {
            let (x, y) = if f == Sha2Fn::Sha256h { (d, n) } else { (n, d) };
            let [mut a, mut bb, mut c, mut dd] = l.lanes::<4>(x);
            let [mut e, mut ff, mut g, mut h] = l.lanes::<4>(y);
            let w = l.lanes::<4>(m);
            for &wk in &w {
                // choose(e, f, g) = ((f ^ g) & e) ^ g
                let fg = l.xor(ff, g);
                let fge = l.and(fg, e);
                let ch = l.xor(fge, g);
                // majority(a, b, c) = (a & b) | ((a | b) & c)
                let ab = l.and(a, bb);
                let a_or_b = l.op(Op::Or, a, bb);
                let abc = l.and(a_or_b, c);
                let maj = l.op(Op::Or, ab, abc);
                let s1 = l.three_rotations(e, [6, 11, 25]);
                let hs1 = l.add(h, s1);
                let t = l.add3(hs1, ch, wk);
                let s0 = l.three_rotations(a, [2, 13, 22]);
                let s0maj = l.add(s0, maj);
                let new_a = l.add(t, s0maj);
                let new_e = l.add(t, dd);
                (h, g, ff, e) = (g, ff, e, new_e);
                (dd, c, bb, a) = (c, bb, a, new_a);
            }
            if f == Sha2Fn::Sha256h { l.vector(d, [a, bb, c, dd]) } else { l.vector(d, [e, ff, g, h]) }
        }
        (Sha2Fn::Sha256su0, &[d, n]) => {
            let dv = l.lanes::<4>(d);
            let [n0, ..] = l.lanes::<4>(n);
            let t = [dv[1], dv[2], dv[3], n0];
            let mut out = dv;
            for (o, (&te, &de)) in out.iter_mut().zip(t.iter().zip(&dv)) {
                let s = l.small_sigma(te, 7, 18, 3);
                *o = l.add(s, de);
            }
            l.vector(d, out)
        }
        (Sha2Fn::Sha256su1, &[d, n, m]) => {
            let dv = l.lanes::<4>(d);
            let nv = l.lanes::<4>(n);
            let mv = l.lanes::<4>(m);
            let t0 = [nv[1], nv[2], nv[3], mv[0]];
            let mut out = dv;
            for e in 0..4 {
                let from = if e < 2 { mv[e + 2] } else { out[e - 2] };
                let s = l.small_sigma(from, 17, 19, 10);
                out[e] = l.add3(s, dv[e], t0[e]);
            }
            l.vector(d, out)
        }
        (Sha2Fn::Sha512h, &[w, x, y]) => {
            let [w0, w1] = l.lanes::<2>(w);
            let [x0, x1] = l.lanes::<2>(x);
            let [y0, y1] = l.lanes::<2>(y);
            let s1 = l.three_rotations(y1, [14, 18, 41]);
            let yx = l.and(y1, x0);
            let not_y = l.not(y1);
            let nyx = l.and(not_y, x1);
            let ch = l.xor(yx, nyx);
            let hi = l.add3(ch, s1, w1);
            let tmp = l.add(hi, y0);
            let s1 = l.three_rotations(tmp, [14, 18, 41]);
            let ty1 = l.and(tmp, y1);
            let not_t = l.not(tmp);
            let ntx = l.and(not_t, x0);
            let ch = l.xor(ty1, ntx);
            let lo = l.add3(ch, s1, w0);
            l.vector(w, [lo, hi])
        }
        (Sha2Fn::Sha512h2, &[w, x, y]) => {
            let [w0, w1] = l.lanes::<2>(w);
            let [x0, _] = l.lanes::<2>(x);
            let [y0, y1] = l.lanes::<2>(y);
            let y1y0 = l.and(y1, y0);
            let s0 = l.three_rotations(y0, [28, 34, 39]);
            let (p, q) = (l.and(x0, y1), l.and(x0, y0));
            let pq = l.xor(p, q);
            let maj = l.xor(pq, y1y0);
            let hi = l.add3(maj, s0, w1);
            let s0 = l.three_rotations(hi, [28, 34, 39]);
            let (p, q) = (l.and(hi, y0), l.and(hi, y1));
            let pq = l.xor(p, q);
            let maj = l.xor(pq, y1y0);
            let lo = l.add3(maj, s0, w0);
            l.vector(w, [lo, hi])
        }
        (Sha2Fn::Sha512su0, &[w, x]) => {
            let [w0, w1] = l.lanes::<2>(w);
            let [x0, _] = l.lanes::<2>(x);
            let s = l.small_sigma(w1, 1, 8, 7);
            let lo = l.add(w0, s);
            let s = l.small_sigma(x0, 1, 8, 7);
            let hi = l.add(w1, s);
            l.vector(w, [lo, hi])
        }
        (Sha2Fn::Sha512su1, &[w, x, y]) => {
            let [w0, w1] = l.lanes::<2>(w);
            let [x0, x1] = l.lanes::<2>(x);
            let [y0, y1] = l.lanes::<2>(y);
            let s = l.small_sigma(x1, 19, 61, 6);
            let hi = l.add3(w1, s, y1);
            let s = l.small_sigma(x0, 19, 61, 6);
            let lo = l.add3(w0, s, y0);
            l.vector(w, [lo, hi])
        }
        // The count was checked above: every function's arm takes its `vectors()` operands.
        _ => return None,
    })
}
