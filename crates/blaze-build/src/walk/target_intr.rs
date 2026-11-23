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
use blaze_lir::{Flags, Op, Ty, Value};
use blaze_typeck::ExprId;

use super::{Repr, V, Walk, malformed};
use crate::BuildError;
use crate::intrinsics;
use crate::intrinsics::target::Target;
use crate::intrinsics::target::float::FloatCall;
use crate::intrinsics::target::neon::NeonCall;
use crate::intrinsics::target::sha2::Sha2Fn;
use crate::intrinsics::target::sha3::Sha3Call;

/// The most operands a generic float function takes (`llvm.fma`).
const MOST_OPERANDS: usize = 3;

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

    /// NEON integer call `e` (`t` is `Target::Neon(call)`): each operand loaded whole as the
    /// LIR vector of its shape from its place, the call's operations applied
    /// ([`intrinsics::target::neon::build`]), the result stored whole into a temporary.
    fn neon_intrinsic(&mut self, e: ExprId, list: &[ExprId], t: Target, call: NeonCall) -> Result<(), BuildError> {
        let refuse = move |why: &'static str| BuildError::Target { target: t, expr: Some(e), why };
        let shapes = call.operands();
        if list.len() != shapes.len() {
            return Err(refuse("a call with other than its function's two operands"));
        }
        let base = self.vals.len().checked_sub(shapes.len()).ok_or(malformed("an intrinsic's arguments missing from the value stack (a build bug)"))?;
        let mut loaded = [Value(0); 2];
        for (i, (&a, shape)) in list.iter().zip(shapes).enumerate() {
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
        self.truncate_vals(base);
        let rt = self.expr_ty(e)?;
        let rrepr = self.repr_at(e, rt)?;
        let Repr::Agg(_) = rrepr else { return Err(refuse("a result that is not the vector its name's shape says")) };
        let (size, align) = self.size_align(rrepr);
        if u64::from(size) != call.shape().bytes() {
            return Err(refuse("a result whose size is not that of the vector its name's shape says"));
        }
        let r = intrinsics::target::neon::build(&mut self.b, call, loaded[0], loaded[1]);
        let to = self.temp(rrepr);
        self.b.store(Flags::NONE, r, to.0, to.1, MemAttrs::aligned(align));
        self.push_val(V::Place(to.0, to.1), None);
        Ok(())
    }

    /// SHA3 call `e` (`t` is `Target::Sha3(call)`): each vector operand loaded whole as the LIR
    /// vector of the call's shape from its place, `xar`'s rotate amount read as the constant it
    /// must be ([`intrinsics::target::sha3::rotate_amount`]), the call's operations applied
    /// ([`intrinsics::target::sha3::build`]), the result stored whole into a temporary.
    fn sha3_intrinsic(&mut self, e: ExprId, list: &[ExprId], t: Target, call: Sha3Call) -> Result<(), BuildError> {
        let refuse = move |why: &'static str| BuildError::Target { target: t, expr: Some(e), why };
        let vectors = call.vectors();
        let n = vectors + usize::from(call.takes_imm());
        if list.len() != n {
            return Err(refuse("a call with other than its function's number of operands"));
        }
        let shape = call.shape();
        let base = self.vals.len().checked_sub(n).ok_or(malformed("an intrinsic's arguments missing from the value stack (a build bug)"))?;
        let mut loaded = [Value(0); 3];
        let mut imm = 0;
        for (i, &a) in list.iter().enumerate() {
            let v = self.vals[base + i];
            let at = self.adjusted_ty(a)?;
            let repr = self.repr_at(a, at)?;
            if i == vectors {
                let ty = repr.scalar().ok_or(refuse("a rotate amount that is not a scalar"))?;
                let x = self.scalar(v, ty)?;
                imm = intrinsics::target::sha3::rotate_amount(&self.b, x).map_err(|r| refuse(r.why()))?;
                continue;
            }
            let Repr::Agg(_) = repr else { return Err(refuse("an operand that is not the vector its name's shape says")) };
            let (size, align) = self.size_align(repr);
            if u64::from(size) != shape.bytes() {
                return Err(refuse("an operand whose size is not that of the vector its name's shape says"));
            }
            let (p, off) = self.place(v, repr)?;
            loaded[i] = self.b.load(shape.ty(), Flags::NONE, p, off, MemAttrs::aligned(align));
        }
        self.truncate_vals(base);
        let rt = self.expr_ty(e)?;
        let rrepr = self.repr_at(e, rt)?;
        let Repr::Agg(_) = rrepr else { return Err(refuse("a result that is not the vector its name's shape says")) };
        let (size, align) = self.size_align(rrepr);
        if u64::from(size) != shape.bytes() {
            return Err(refuse("a result whose size is not that of the vector its name's shape says"));
        }
        let r = intrinsics::target::sha3::build(&mut self.b, call, &loaded[..vectors], imm)
            .ok_or(malformed("a SHA3 call built with other than its operand count (a build bug)"))?;
        let to = self.temp(rrepr);
        self.b.store(Flags::NONE, r, to.0, to.1, MemAttrs::aligned(align));
        self.push_val(V::Place(to.0, to.1), None);
        Ok(())
    }

    /// SHA-256 or SHA-512 call `e` (`t` is `Target::Sha2(f)`): each operand loaded whole as the
    /// LIR vector of the function's shape from its place, the rule built on its lanes
    /// ([`intrinsics::target::sha2::build`]), the result stored whole into a temporary.
    fn sha2_intrinsic(&mut self, e: ExprId, list: &[ExprId], t: Target, f: Sha2Fn) -> Result<(), BuildError> {
        let refuse = move |why: &'static str| BuildError::Target { target: t, expr: Some(e), why };
        let n = f.vectors();
        if list.len() != n {
            return Err(refuse("a call with other than its function's number of operands"));
        }
        let shape = f.shape();
        let base = self.vals.len().checked_sub(n).ok_or(malformed("an intrinsic's arguments missing from the value stack (a build bug)"))?;
        let mut loaded = [Value(0); 3];
        for (i, &a) in list.iter().enumerate() {
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
        self.truncate_vals(base);
        let rt = self.expr_ty(e)?;
        let rrepr = self.repr_at(e, rt)?;
        let Repr::Agg(_) = rrepr else { return Err(refuse("a result that is not the vector its name's shape says")) };
        let (size, align) = self.size_align(rrepr);
        if u64::from(size) != shape.bytes() {
            return Err(refuse("a result whose size is not that of the vector its name's shape says"));
        }
        let r = intrinsics::target::sha2::build(&mut self.b, f, &loaded[..n]).ok_or(malformed("a SHA-2 call built with other than its operand count (a build bug)"))?;
        let to = self.temp(rrepr);
        self.b.store(Flags::NONE, r, to.0, to.1, MemAttrs::aligned(align));
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
