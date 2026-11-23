//! How a value crosses a call, as both builders of a body plan it: the walk over the typeck
//! body (walk.rs) and blaze-body-build over the drop-elaborated body. One plan for both sides
//! of a call, the caller's arguments and results and the callee's parameters and return, so a
//! function built by one builder calls a function built by the other (lib.rs, "Rust ABI" and
//! "The C ABI").

use core::alloc::Allocator;

use blaze_lir::Ty;
use blaze_lir::builder::{Facts, Param};
use blaze_lir::func::{Ext, ParamKind};

/// How one value crosses a call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pass {
    /// Nothing: a value-less type, or a C zero-sized one.
    Ignore,
    /// One register of `ty`, widened by `ext`. `agg`: the walk holds it as a one-scalar
    /// aggregate, read and written at offset 0.
    Scalar { ty: Ty, ext: Ext, agg: bool },
    /// A wide pointer: the data pointer followed by its length or vtable pointer.
    Wide { metadata: Ty },
    /// Rust: an aggregate by the address of the caller's copy; a result by a hidden pointer
    /// first.
    Ref,
    /// C: a composite past 16 bytes by the address of a copy (`byval`), a result through
    /// `sret`.
    Memory { size: u32, align: u8 },
    /// C: a composite of `size` (1 to 16) bytes in `size.div_ceil(8)` `i64` registers, the
    /// last read at a width inside the size.
    Words { size: u32 },
}

/// The lowered signature of passes `inputs` and `ret`: a result pointer first (Rust's plain
/// pointer, C's `sret`), then each input's registers; both lists in `alloc`.
pub fn sig_of_in<A: Allocator + Copy>(inputs: &[Pass], ret: Pass, alloc: A) -> (Vec<Param, A>, Vec<Param, A>) {
    let mut params = Vec::with_capacity_in(inputs.len() + 1, alloc);
    let mut rets = Vec::with_capacity_in(2, alloc);
    let memory = |kind, size, align| Param { kind, facts: Facts { size, align, ..Param::of(Ty::PTR).facts }, ..Param::of(Ty::PTR) };
    match ret {
        Pass::Ignore => {}
        Pass::Ref => params.push(Param::of(Ty::PTR)),
        Pass::Memory { size, align } => params.push(memory(ParamKind::Sret, size, align)),
        Pass::Scalar { ty, ext, .. } => rets.push(Param { ext, ..Param::of(ty) }),
        Pass::Wide { metadata } => rets.extend([Param::of(Ty::PTR), Param::of(metadata)]),
        Pass::Words { size } => {
            for _ in 0..size.div_ceil(8) {
                rets.push(Param::of(Ty::I64));
            }
        }
    }
    for &p in inputs {
        match p {
            Pass::Ignore => {}
            Pass::Ref => params.push(Param::of(Ty::PTR)),
            Pass::Memory { size, align } => params.push(memory(ParamKind::Byval, size, align)),
            Pass::Scalar { ty, ext, .. } => params.push(Param { ext, ..Param::of(ty) }),
            Pass::Wide { metadata } => params.extend([Param::of(Ty::PTR), Param::of(metadata)]),
            Pass::Words { size } => {
                for _ in 0..size.div_ceil(8) {
                    params.push(Param::of(Ty::I64));
                }
            }
        }
    }
    (params, rets)
}
