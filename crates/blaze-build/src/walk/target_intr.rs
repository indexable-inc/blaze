//! A call of a target intrinsic in the walk (intrinsics/target.rs, the one home of the set;
//! INTRINSICS.md 3.3, P7b): a declaration of an `extern "llvm-intrinsic"` block that front2
//! tabled as `Intrinsic::Target`, its arguments on top of the value stack.
//!
//! Four kinds, by [`Target`]:
//! - a NEON integer function (`llvm.aarch64.neon.tbl1.v16i8`, `llvm.aarch64.neon.ushl.v2i64`
//!   and kin, intrinsics/target/neon.rs): each operand loaded whole as its shape's LIR vector
//!   from its place, the result stored whole into a temporary; an operand or result whose size
//!   is not its shape's is refused by name, as for a float function;
//! - a SHA3 bitwise function (`llvm.aarch64.crypto.bcaxu.v2i64`, `llvm.aarch64.crypto.xar`
//!   and kin, intrinsics/target/sha3.rs): as a NEON one, its vectors loaded whole; `xar`'s
//!   trailing `i64` rotate amount must be a constant below 64;
//! - a barrier or hint (`llvm.aarch64.isb` and kin, THREADS.md D1): `fn(i32 imm)`, the
//!   immediate a constant, no result; built as LIR's `target_intrinsic` by
//!   [`intrinsics::target::build`];
//! - a generic float function (`llvm.roundeven.v4f32` and kin, intrinsics/target/float.rs):
//!   its LIR float operation, lane by lane. A scalar shape (`.f32`, `.f64`) takes and gives
//!   one LIR scalar; a vector shape (`.v4f32` and kin) takes each operand by its address (a
//!   `repr(simd)` value is held in memory, intrinsics/simd.rs) and gives a temporary holding the
//!   result's lanes. An operand or result whose size is not the shape's is refused by name:
//!   the name, not the declared Rust type, says what LLVM would be asked for.

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::{Builder, Flags, Full, Op, Ty, Value};
use blaze_typeck::ExprId;

use super::{Repr, V, Walk, malformed};
use crate::BuildError;
use crate::intrinsics;
use crate::intrinsics::target::Target;
use crate::intrinsics::target::float::FloatCall;
use crate::intrinsics::target::neon::{IntShape, NeonCall};
use crate::intrinsics::target::sha2::Sha2Fn;
use crate::intrinsics::target::sha3::Sha3Call;

/// The most operands a generic float function takes (`llvm.fma`).
const MOST_OPERANDS: usize = 3;

/// Shape and arity description of a whole-vector target intrinsic (`neon`, `sha3`, `sha2`).
struct VectorIntrinsic<'a> {
    operands: &'a [IntShape],
    result: IntShape,
    takes_imm: bool,
    arity_why: &'static str,
}

/// An operand of a float call as the walk holds it.
#[derive(Clone, Copy)]
enum Operand {
    /// A scalar shape's value.
    Scalar(Value),
    /// A vector shape's address.
    At(Value),
}

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Target intrinsic call `e` of `t`, its arguments `list` on top of the value stack.
    pub(super) fn target_intrinsic(&mut self, e: ExprId, list: &[ExprId], t: Target) -> Result<(), BuildError> {
        match t {
            Target::Float(call) => self.float_intrinsic(e, list, t, call),
            Target::Neon(call) => self.neon_intrinsic(e, list, t, call),
            Target::Sha3(call) => self.sha3_intrinsic(e, list, t, call),
            Target::Sha2(f) => self.sha2_intrinsic(e, list, t, f),
            Target::Crc32(f) => self.crc32_intrinsic(e, list, t, f),
            Target::Pmull(f) => self.pmull_intrinsic(e, list, t, f),
            Target::Wasm64MemoryGrow => {
                let &[mem_arg, pages_arg] = list else {
                    return Err(BuildError::Target { target: t, expr: Some(e), why: "llvm.wasm.memory.grow.i64 expects (u32, usize)" });
                };
                let base = self.vals.len().checked_sub(2).ok_or(malformed("an intrinsic's arguments missing from the value stack (a build bug)"))?;
                let mem_v = self.vals[base];
                let pages_v = self.vals[base + 1];
                self.truncate_vals(base);
                let out = if self.live {
                    let mem_ty = self.adjusted_ty(mem_arg)?;
                    let mem_repr = self.repr_at(mem_arg, mem_ty)?;
                    let mem_s = mem_repr.scalar().ok_or(BuildError::Target { target: t, expr: Some(e), why: "memory index is not scalar" })?;
                    let mem = self.scalar(mem_v, mem_s)?;
                    let pages_ty = self.adjusted_ty(pages_arg)?;
                    let pages_repr = self.repr_at(pages_arg, pages_ty)?;
                    let pages_s = pages_repr.scalar().ok_or(BuildError::Target { target: t, expr: Some(e), why: "pages delta is not scalar" })?;
                    let pages = self.scalar(pages_v, pages_s)?;
                    let sig = self.b.sig(
                        blaze_lir::func::Conv::C,
                        blaze_lir::builder::Variadic::No,
                        &[blaze_lir::builder::Param::of(Ty::I32), blaze_lir::builder::Param::of(Ty::I64)],
                        &[blaze_lir::builder::Param::of(Ty::I64)],
                    );
                    let id = self.b.intrinsic(t.link_name(), sig);
                    V::Scalar(self.b.target_intrinsic(id, &[mem, pages]))
                } else {
                    V::None
                };
                self.push_val(out, None);
                Ok(())
            }
            Target::Wasm64MemorySize => {
                let &[mem_arg] = list else {
                    return Err(BuildError::Target { target: t, expr: Some(e), why: "llvm.wasm.memory.size.i64 expects (u32)" });
                };
                let base = self.vals.len().checked_sub(1).ok_or(malformed("an intrinsic's arguments missing from the value stack (a build bug)"))?;
                let mem_v = self.vals[base];
                self.truncate_vals(base);
                let out = if self.live {
                    let mem_ty = self.adjusted_ty(mem_arg)?;
                    let mem_repr = self.repr_at(mem_arg, mem_ty)?;
                    let mem_s = mem_repr.scalar().ok_or(BuildError::Target { target: t, expr: Some(e), why: "memory index is not scalar" })?;
                    let mem = self.scalar(mem_v, mem_s)?;
                    let sig = self.b.sig(
                        blaze_lir::func::Conv::C,
                        blaze_lir::builder::Variadic::No,
                        &[blaze_lir::builder::Param::of(Ty::I32)],
                        &[blaze_lir::builder::Param::of(Ty::I64)],
                    );
                    let id = self.b.intrinsic(t.link_name(), sig);
                    V::Scalar(self.b.target_intrinsic(id, &[mem]))
                } else {
                    V::None
                };
                self.push_val(out, None);
                Ok(())
            }
            // A barrier or hint (THREADS.md D1, intrinsics/target.rs): `fn(i32 imm)`, the
            // immediate a constant (the library passes `arg::SY` and the like), no result.
            _ => {
                let &[arg] = list else {
                    return Err(BuildError::Target { target: t, expr: Some(e), why: "a call of other than its one immediate argument" });
                };
                let base = self.vals.len().checked_sub(1).ok_or(malformed("an intrinsic's arguments missing from the value stack (a build bug)"))?;
                let v = self.vals[base];
                self.truncate_vals(base);
                // After a diverging argument the call is unreachable: nothing to build.
                if self.live {
                    let at = self.adjusted_ty(arg)?;
                    let repr = self.repr_at(arg, at)?;
                    let ty = repr.scalar().ok_or(BuildError::Target { target: t, expr: Some(e), why: "an immediate that is not a scalar" })?;
                    let imm = self.scalar(v, ty)?;
                    intrinsics::target::build(&mut self.b, t, imm).map_err(|error| match error {
                        BuildError::Target { target, why, .. } => BuildError::Target { target, expr: Some(e), why },
                        other => other,
                    })?;
                }
                self.push_val(V::None, None);
                Ok(())
            }
        }
    }

    /// Generic float call `e` (`t` is `Target::Float(call)`).
    fn float_intrinsic(&mut self, e: ExprId, list: &[ExprId], t: Target, call: FloatCall) -> Result<(), BuildError> {
        let refuse = move |why: &'static str| BuildError::Target { target: t, expr: Some(e), why };
        let n = call.func.arity();
        if list.len() != n {
            return Err(refuse("a call with other than its function's number of operands"));
        }
        let shape = call.shape;
        let elem = shape.elem();
        // The bytes a vector shape's value holds, and its lanes' alignment (log2).
        let lanes = shape.lanes();
        let stride = shape.stride();
        let align = stride.trailing_zeros() as u8;
        let base = self.vals.len().checked_sub(n).ok_or(malformed("an intrinsic's arguments missing from the value stack (a build bug)"))?;
        let mut ops: [Option<Operand>; MOST_OPERANDS] = [None; MOST_OPERANDS];
        for (i, &a) in list.iter().enumerate() {
            let v = self.vals[base + i];
            let at = self.adjusted_ty(a)?;
            let repr = self.repr_at(a, at)?;
            ops[i] = Some(match lanes {
                None => {
                    if repr.scalar() != Some(elem) {
                        return Err(refuse("an operand that is not the float its name's shape says"));
                    }
                    Operand::Scalar(self.scalar(v, elem)?)
                }
                Some(count) => {
                    let Repr::Agg(_) = repr else { return Err(refuse("an operand that is not the vector its name's shape says")) };
                    if u64::from(self.size_align(repr).0) != count * stride {
                        return Err(refuse("an operand whose size is not that of the vector its name's shape says"));
                    }
                    let place = self.place(v, repr)?;
                    Operand::At(self.address(place))
                }
            });
        }
        self.truncate_vals(base);
        let rt = self.expr_ty(e)?;
        let rrepr = self.repr_at(e, rt)?;
        let op = call.func.op();
        let out = match lanes {
            None => {
                if rrepr.scalar() != Some(elem) {
                    return Err(refuse("a result that is not the float its name's shape says"));
                }
                V::Scalar(self.lane(op, &ops[..n], None)?)
            }
            Some(count) => {
                let Repr::Agg(_) = rrepr else { return Err(refuse("a result that is not the vector its name's shape says")) };
                if u64::from(self.size_align(rrepr).0) != count * stride {
                    return Err(refuse("a result whose size is not that of the vector its name's shape says"));
                }
                let to = self.temp(rrepr);
                for lane in 0..count {
                    // A NEON vector is at most 16 bytes (`Shape`): the offset fits.
                    let off = (lane * stride) as i32;
                    let r = self.lane(op, &ops[..n], Some((elem, off, align)))?;
                    self.b.store(Flags::NONE, r, to.0, to.1 + off, MemAttrs::aligned(align));
                }
                V::Place(to.0, to.1)
            }
        };
        self.push_val(out, None);
        Ok(())
    }

    /// Whole-vector target intrinsic call `e` (`neon`, `sha3`, `sha2`): each vector operand
    /// loaded whole as its shape's LIR vector from its place, an optional trailing SHA3 rotate
    /// immediate read as its constant, `build` applied to the loaded vectors, and the result
    /// stored whole into a temporary.
    fn vector_intrinsic(
        &mut self,
        e: ExprId,
        list: &[ExprId],
        t: Target,
        spec: VectorIntrinsic<'_>,
        build: impl FnOnce(&mut Builder<A, Full>, &[Value], u32) -> Result<Value, BuildError>,
    ) -> Result<(), BuildError> {
        let refuse = move |why: &'static str| BuildError::Target { target: t, expr: Some(e), why };
        let vectors = spec.operands.len();
        let n = vectors + usize::from(spec.takes_imm);
        if list.len() != n {
            return Err(refuse(spec.arity_why));
        }
        let base = self.vals.len().checked_sub(n).ok_or(malformed("an intrinsic's arguments missing from the value stack (a build bug)"))?;
        let mut loaded = [Value(0); 3];
        for (i, (&a, &shape)) in list.iter().zip(spec.operands).enumerate() {
            let v = self.vals[base + i];
            let at = self.adjusted_ty(a)?;
            let repr = self.repr_at(a, at)?;
            let Repr::Agg(_) = repr else { return Err(refuse("an operand that is not the vector its name's shape says")) };
            let (size, align) = self.size_align(repr);
            if u64::from(size) != shape.bytes() {
                return Err(refuse("an operand whose size is not that of the vector its name's shape says"));
            }
            let (p, off) = self.place(v, repr)?;
            loaded[i] = self.b.load(shape.ty(), Flags::NONE, p, off, MemAttrs::aligned(align));
        }
        let imm = if spec.takes_imm {
            let a = list[vectors];
            let v = self.vals[base + vectors];
            let at = self.adjusted_ty(a)?;
            let repr = self.repr_at(a, at)?;
            let ty = repr.scalar().ok_or(refuse("a rotate amount that is not a scalar"))?;
            let x = self.scalar(v, ty)?;
            intrinsics::target::sha3::rotate_amount(&self.b, x).map_err(|r| refuse(r.why()))?
        } else {
            0
        };
        self.truncate_vals(base);
        let rt = self.expr_ty(e)?;
        let rrepr = self.repr_at(e, rt)?;
        let Repr::Agg(_) = rrepr else { return Err(refuse("a result that is not the vector its name's shape says")) };
        let (size, align) = self.size_align(rrepr);
        if u64::from(size) != spec.result.bytes() {
            return Err(refuse("a result whose size is not that of the vector its name's shape says"));
        }
        let r = build(&mut self.b, &loaded[..vectors], imm)?;
        let to = self.temp(rrepr);
        self.b.store(Flags::NONE, r, to.0, to.1, MemAttrs::aligned(align));
        self.push_val(V::Place(to.0, to.1), None);
        Ok(())
    }

    /// NEON integer call `e` (`t` is `Target::Neon(call)`): each operand loaded whole as the
    /// LIR vector of its shape from its place, the call's operations applied
    /// ([`intrinsics::target::neon::build`]), the result stored whole into a temporary.
    fn neon_intrinsic(&mut self, e: ExprId, list: &[ExprId], t: Target, call: NeonCall) -> Result<(), BuildError> {
        let operands = call.operands();
        let spec = VectorIntrinsic {
            operands,
            result: call.shape(),
            takes_imm: false,
            arity_why: "a call with other than its function's number of operands",
        };
        self.vector_intrinsic(e, list, t, spec, |b, v, _| {
            intrinsics::target::neon::build(b, call, v)
                .ok_or(malformed("a NEON call built with other than its operand count (a build bug)"))
        })
    }

    /// SHA3 call `e` (`t` is `Target::Sha3(call)`): each vector operand loaded whole as the LIR
    /// vector of the call's shape from its place, `xar`'s rotate amount read as the constant it
    /// must be ([`intrinsics::target::sha3::rotate_amount`]), the call's operations applied
    /// ([`intrinsics::target::sha3::build`]), the result stored whole into a temporary.
    fn sha3_intrinsic(&mut self, e: ExprId, list: &[ExprId], t: Target, call: Sha3Call) -> Result<(), BuildError> {
        let shape = call.shape();
        let operands = [shape; 3];
        let spec = VectorIntrinsic {
            operands: &operands[..call.vectors()],
            result: shape,
            takes_imm: call.takes_imm(),
            arity_why: "a call with other than its function's number of operands",
        };
        self.vector_intrinsic(e, list, t, spec, |b, v, imm| {
            intrinsics::target::sha3::build(b, call, v, imm)
                .ok_or(malformed("a SHA3 call built with other than its operand count (a build bug)"))
        })
    }

    /// SHA-256 or SHA-512 call `e` (`t` is `Target::Sha2(f)`): each operand loaded whole as the
    /// LIR vector of the function's shape from its place, the rule built on its lanes
    /// ([`intrinsics::target::sha2::build`]), the result stored whole into a temporary.
    fn sha2_intrinsic(&mut self, e: ExprId, list: &[ExprId], t: Target, f: Sha2Fn) -> Result<(), BuildError> {
        let shape = f.shape();
        let operands = [shape; 3];
        let spec = VectorIntrinsic {
            operands: &operands[..f.vectors()],
            result: shape,
            takes_imm: false,
            arity_why: "a call with other than its function's number of operands",
        };
        self.vector_intrinsic(e, list, t, spec, |b, v, _| {
            intrinsics::target::sha2::build(b, f, v).ok_or(malformed("a SHA-2 call built with other than its operand count (a build bug)"))
        })
    }

    /// AArch64 CRC32 / CRC32C call `e` (`t` is `Target::Crc32(f)`): `(crc: u32, data: u32|u64) -> u32`.
    fn crc32_intrinsic(&mut self, e: ExprId, list: &[ExprId], t: Target, f: intrinsics::target::crc32::Crc32Fn) -> Result<(), BuildError> {
        let &[crc_arg, data_arg] = list else {
            return Err(BuildError::Target { target: t, expr: Some(e), why: "a CRC32 call with other than two operands" });
        };
        let base = self.vals.len().checked_sub(2).ok_or(malformed("an intrinsic's arguments missing from the value stack (a build bug)"))?;
        let crc_v = self.vals[base];
        let data_v = self.vals[base + 1];
        self.truncate_vals(base);
        if !self.live {
            self.push_val(V::None, None);
            return Ok(());
        }
        let crc_ty = self.adjusted_ty(crc_arg)?;
        let data_ty = self.adjusted_ty(data_arg)?;
        let crc_repr = self.repr_at(crc_arg, crc_ty)?;
        let data_repr = self.repr_at(data_arg, data_ty)?;
        let want_data = f.data_ty();
        if crc_repr.scalar() != Some(Ty::I32) || data_repr.scalar() != Some(want_data) {
            return Err(BuildError::Target { target: t, expr: Some(e), why: "CRC32 operand types do not match" });
        }
        let crc = self.scalar(crc_v, Ty::I32)?;
        let data = self.scalar(data_v, want_data)?;
        let res = intrinsics::target::crc32::build(&mut self.b, f, crc, data);
        self.push_val(V::Scalar(res), None);
        Ok(())
    }

    /// AArch64 NEON polynomial multiply-long call `e` (`t` is `Target::Pmull(f)`):
    /// - `Pmull64`: `(a: u64, b: u64) -> int8x16_t` (16-byte result, little-endian `[lo, hi]`).
    /// - `PmullV8I16`: `(a: poly8x8_t, b: poly8x8_t) -> poly16x8_t` (8 lanes of `u8 x u8 -> u16`).
    fn pmull_intrinsic(&mut self, e: ExprId, list: &[ExprId], t: Target, f: intrinsics::target::pmull::PmullFn) -> Result<(), BuildError> {
        use intrinsics::target::pmull::PmullFn;

        let &[lhs_arg, rhs_arg] = list else {
            return Err(BuildError::Target { target: t, expr: Some(e), why: "a PMULL call with other than two operands" });
        };
        let base = self.vals.len().checked_sub(2).ok_or(malformed("an intrinsic's arguments missing from the value stack (a build bug)"))?;
        let lhs_v = self.vals[base];
        let rhs_v = self.vals[base + 1];
        self.truncate_vals(base);
        if !self.live {
            self.push_val(V::None, None);
            return Ok(());
        }
        let lhs_ty = self.adjusted_ty(lhs_arg)?;
        let rhs_ty = self.adjusted_ty(rhs_arg)?;
        let lhs_repr = self.repr_at(lhs_arg, lhs_ty)?;
        let rhs_repr = self.repr_at(rhs_arg, rhs_ty)?;
        let rt = self.expr_ty(e)?;
        let rrepr = self.repr_at(e, rt)?;
        let Repr::Agg(_) = rrepr else {
            return Err(BuildError::Target { target: t, expr: Some(e), why: "PMULL result is not a 16-byte vector" });
        };
        let (rsize, ralign) = self.size_align(rrepr);
        if rsize != 16 {
            return Err(BuildError::Target { target: t, expr: Some(e), why: "PMULL result size is not 16 bytes" });
        }
        let to = self.temp(rrepr);
        match f {
            PmullFn::Pmull64 => {
                if lhs_repr.scalar() != Some(Ty::I64) || rhs_repr.scalar() != Some(Ty::I64) {
                    return Err(BuildError::Target { target: t, expr: Some(e), why: "pmull64 operand types are not u64" });
                }
                let lhs = self.scalar(lhs_v, Ty::I64)?;
                let rhs = self.scalar(rhs_v, Ty::I64)?;
                let (lo, hi) = intrinsics::target::pmull::build_pmull64(&mut self.b, lhs, rhs);
                let align8 = ralign.min(8);
                self.b.store(Flags::NONE, lo, to.0, to.1, MemAttrs::aligned(align8));
                self.b.store(Flags::NONE, hi, to.0, to.1 + 8, MemAttrs::aligned(align8));
            }
            PmullFn::PmullV8I16 => {
                let (Repr::Agg(_), Repr::Agg(_)) = (lhs_repr, rhs_repr) else {
                    return Err(BuildError::Target { target: t, expr: Some(e), why: "pmull.v8i16 operands are not 8-byte vectors" });
                };
                let (lsize, _) = self.size_align(lhs_repr);
                let (r_in_size, _) = self.size_align(rhs_repr);
                if lsize != 8 || r_in_size != 8 {
                    return Err(BuildError::Target { target: t, expr: Some(e), why: "pmull.v8i16 operand size is not 8 bytes" });
                }
                let (lp, loff) = self.place(lhs_v, lhs_repr)?;
                let (rp, roff) = self.place(rhs_v, rhs_repr)?;
                let align2 = ralign.min(2);
                for lane in 0i32..8 {
                    let lb = self.b.load(Ty::I8, Flags::NONE, lp, loff + lane, MemAttrs::aligned(1));
                    let rb = self.b.load(Ty::I8, Flags::NONE, rp, roff + lane, MemAttrs::aligned(1));
                    let prod = intrinsics::target::pmull::build_pmull8(&mut self.b, lb, rb);
                    self.b.store(Flags::NONE, prod, to.0, to.1 + lane * 2, MemAttrs::aligned(align2));
                }
            }
        }
        self.push_val(V::Place(to.0, to.1), None);
        Ok(())
    }

    /// One lane of `op` over `ops` (one, two or three, by the function's arity): each
    /// operand's scalar, or for a vector shape (`at` = the lane's type, byte offset and
    /// alignment) its lane loaded from its address.
    fn lane(&mut self, op: Op, ops: &[Option<Operand>], at: Option<(Ty, i32, u8)>) -> Result<Value, BuildError> {
        let mut vals = [None; MOST_OPERANDS];
        for (slot, o) in vals.iter_mut().zip(ops) {
            *slot = Some(match (*o, at) {
                (Some(Operand::Scalar(x)), None) => x,
                (Some(Operand::At(p)), Some((ty, off, align))) => self.b.load(ty, Flags::NONE, p, off, MemAttrs::aligned(align)),
                _ => return Err(malformed("a float intrinsic's operand not held as its shape says (a build bug)")),
            });
        }
        Ok(match vals {
            [Some(a), None, None] => self.b.unary(op, Flags::NONE, a),
            [Some(a), Some(b), None] => self.b.binary(op, Flags::NONE, a, b),
            [Some(a), Some(b), Some(c)] => self.b.ternary(op, Flags::NONE, a, b, c),
            _ => return Err(malformed("a float intrinsic of no operands (a build bug)")),
        })
    }
}
