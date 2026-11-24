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

/// Pushes the lowered signature of passes `inputs` (with optional [`blaze_lir::func::ParamFactsRec`]
/// bitmasks `ptr_facts`, indexed by `inputs`) and `ret`: a result pointer first (Rust's plain
/// pointer, C's `sret`), then each input's registers.
fn fill_sig(
    inputs: &[Pass],
    ptr_facts: &[u8],
    ret: Pass,
    mut push_param: impl FnMut(Param),
    mut push_ret: impl FnMut(Param),
) {
    let memory = |kind, size, align| Param { kind, facts: Facts { size, align, ..Param::of(Ty::PTR).facts }, ..Param::of(Ty::PTR) };
    let ptr_with_facts = |ext, bits| Param { ext, facts: Facts { bits, ..Param::of(Ty::PTR).facts }, ..Param::of(Ty::PTR) };
    match ret {
        Pass::Ignore => {}
        Pass::Ref => push_param(Param::of(Ty::PTR)),
        Pass::Memory { size, align } => push_param(memory(ParamKind::Sret, size, align)),
        Pass::Scalar { ty, ext, .. } => push_ret(Param { ext, ..Param::of(ty) }),
        Pass::Wide { metadata } => {
            push_ret(Param::of(Ty::PTR));
            push_ret(Param::of(metadata));
        }
        Pass::Words { size } => {
            for _ in 0..size.div_ceil(8) {
                push_ret(Param::of(Ty::I64));
            }
        }
    }
    for (i, &p) in inputs.iter().enumerate() {
        let bits = ptr_facts.get(i).copied().unwrap_or(0);
        match p {
            Pass::Ignore => {}
            Pass::Ref => {
                if bits != 0 {
                    push_param(ptr_with_facts(Ext::None, bits));
                } else {
                    push_param(Param::of(Ty::PTR));
                }
            }
            Pass::Memory { size, align } => push_param(memory(ParamKind::Byval, size, align)),
            Pass::Scalar { ty, ext, .. } => {
                if ty == Ty::PTR && bits != 0 {
                    push_param(ptr_with_facts(ext, bits));
                } else {
                    push_param(Param { ext, ..Param::of(ty) });
                }
            }
            Pass::Wide { metadata } => {
                if bits != 0 {
                    push_param(ptr_with_facts(Ext::None, bits));
                } else {
                    push_param(Param::of(Ty::PTR));
                }
                push_param(Param::of(metadata));
            }
            Pass::Words { size } => {
                for _ in 0..size.div_ceil(8) {
                    push_param(Param::of(Ty::I64));
                }
            }
        }
    }
}

/// The lowered signature of passes `inputs` and `ret`: a result pointer first (Rust's plain
/// pointer, C's `sret`), then each input's registers; both lists in `alloc`.
pub fn sig_of_in<A: Allocator + Copy>(inputs: &[Pass], ret: Pass, alloc: A) -> (Vec<Param, A>, Vec<Param, A>) {
    let mut params = Vec::with_capacity_in(inputs.len() + 1, alloc);
    let mut rets = Vec::with_capacity_in(2, alloc);
    fill_sig(inputs, &[], ret, |p| params.push(p), |r| rets.push(r));
    (params, rets)
}

/// Like [`sig_of_in`], attaching optional [`blaze_lir::func::ParamFactsRec`] bitmasks from
/// `ptr_facts` (indexed by `inputs`) to pointer parameters (`Ty::PTR` scalars, wide-pointer data
/// pointers, and by-reference aggregates); both lists in `alloc`.
pub fn sig_of_with_ptr_facts_in<A: Allocator + Copy>(
    inputs: &[Pass],
    ptr_facts: &[u8],
    ret: Pass,
    alloc: A,
) -> (blaze_own::Vec<Param, A>, blaze_own::Vec<Param, A>) {
    let mut params = blaze_own::Vec::with_capacity_in(inputs.len() * 2 + 1, alloc);
    let mut rets = blaze_own::Vec::with_capacity_in(2, alloc);
    fill_sig(inputs, ptr_facts, ret, |p| params.push(p), |r| rets.push(r));
    (params, rets)
}

#[cfg(test)]
mod tests {
    use blaze_lir::Ty;
    use blaze_lir::builder::{Facts, Param};
    use blaze_lir::func::{Ext, ParamFactsRec};

    use super::{Pass, sig_of_in, sig_of_with_ptr_facts_in};

    #[test]
    fn ptr_facts_attach_to_wide_data_and_scalar_ptrs_only() {
        let alloc = blaze_own::testing::heap().alloc();
        let ro_slice = ParamFactsRec::NOALIAS | ParamFactsRec::NONNULL | ParamFactsRec::READONLY;
        let mut_ref = ParamFactsRec::NOALIAS | ParamFactsRec::NONNULL;
        let inputs = [
            Pass::Scalar { ty: Ty::F32, ext: Ext::None, agg: false },
            Pass::Wide { metadata: Ty::I64 },
            Pass::Wide { metadata: Ty::I64 },
            Pass::Scalar { ty: Ty::PTR, ext: Ext::None, agg: false },
            Pass::Scalar { ty: Ty::PTR, ext: Ext::None, agg: false },
        ];
        // Deliberately pass non-zero bits for the f32 scalar (must be ignored) and omit the
        // trailing entry (must default to 0).
        let facts = [mut_ref, ro_slice, mut_ref, mut_ref];
        let (params, rets) = sig_of_with_ptr_facts_in(&inputs, &facts, Pass::Ignore, alloc);
        let ptr_facts = |bits| Param { facts: Facts { bits, ..Param::of(Ty::PTR).facts }, ..Param::of(Ty::PTR) };
        assert_eq!(
            params.as_slice(),
            &[
                Param::of(Ty::F32),
                ptr_facts(ro_slice),
                Param::of(Ty::I64),
                ptr_facts(mut_ref),
                Param::of(Ty::I64),
                ptr_facts(mut_ref),
                Param::of(Ty::PTR),
            ],
        );
        assert!(rets.is_empty());

        // An aggregate `Pass::Ref` return inserts a plain hidden result pointer at index 0 while
        // keeping input pointer facts aligned with their shifted positions.
        let (sret_params, sret_rets) = sig_of_with_ptr_facts_in(
            &[Pass::Wide { metadata: Ty::I64 }],
            &[mut_ref],
            Pass::Ref,
            alloc,
        );
        assert_eq!(
            sret_params.as_slice(),
            &[Param::of(Ty::PTR), ptr_facts(mut_ref), Param::of(Ty::I64)],
        );
        assert!(sret_rets.is_empty());

        // `sig_of_in` agrees with `sig_of_with_ptr_facts_in` given empty facts.
        let (plain_params, plain_rets) = sig_of_in(&inputs, Pass::Ignore, alloc);
        let (empty_params, empty_rets) = sig_of_with_ptr_facts_in(&inputs, &[], Pass::Ignore, alloc);
        assert_eq!(plain_params.as_slice(), empty_params.as_slice());
        assert_eq!(plain_rets.as_slice(), empty_rets.as_slice());
    }
}
