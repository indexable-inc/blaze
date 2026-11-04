//! Signatures and their regions.
//!
//! A function's generics in Charon are its late-bound lifetimes, as charon-driver declares them
//! for a function (`late_bound: true`, variance unknown): region parameter `i` is the bound
//! variable `i` of the signature's binder, so a `ReBound` region in the signature is
//! `Region::Var` of that index ([`crate::types`]). Everything else a signature can bind (type or
//! const parameters, early-bound lifetimes, where clauses) is refused.

use charon_lib::ast::{
    Abi, FunSig, GenericParams as CGenericParams, RegionId, RegionParam, Variance,
};
use hir_def::{
    CallableDefId, FunctionId, GenericDefId, hir::generics::GenericParams,
    signatures::FunctionSignature,
};
use hir_ty::next_solver::{BoundRegionKind, BoundVariableKind};
use rustc_type_ir::inherent::{Safety as _, SliceLike as _};

use crate::{
    Unsupported,
    items::Ctx,
    refuse,
    types::{self, Regions},
    why,
};

/// A function's Charon generics and signature.
pub(crate) struct Signature {
    pub(crate) generics: CGenericParams,
    pub(crate) sig: FunSig,
}

impl Signature {
    /// The number of region parameters: what a call gives as region arguments.
    pub(crate) fn region_count(&self) -> usize {
        self.generics.regions.len()
    }
}

/// The signature of `func`; `item` is its path, for errors. Every refusal is collected.
pub(crate) fn signature<'db>(
    ctx: &mut Ctx<'db>,
    func: FunctionId,
    item: &str,
) -> Result<Signature, Vec<Unsupported>> {
    let mut errors = Vec::new(); // alloc: errors, once per signature
    let params = GenericParams::of(ctx.db, GenericDefId::FunctionId(func));
    if params.len_type_or_consts() > 0 {
        errors.push(refuse(item, "a type or const generic parameter", why::GENERICS));
    }
    if params.len_early_bound_lifetimes() > 0 {
        errors.push(refuse(
            item,
            "an early-bound lifetime parameter (one a where clause or bound names)",
            why::GENERICS,
        ));
    }
    if !params.has_no_predicates() {
        errors.push(refuse(item, "a where clause or bound", why::GENERICS));
    }
    let data = FunctionSignature::of(ctx.db, func);
    if data.is_async() {
        errors.push(refuse(item, "an `async fn`", why::CONTROL));
    }
    if data.is_unsafe() {
        errors.push(refuse(item, "an `unsafe fn`", why::UNSAFE));
    }
    if data.is_varargs() {
        errors.push(refuse(item, "a C-variadic function", why::UNSAFE));
    }

    // The solver's signature, not `callable_item_signature`: each lifetime the parameters leave
    // out is a late-bound one of its own there, as in rustc's `fn_sig`, where the other keeps
    // it an error region for inference.
    let poly = hir_ty::solver_fn_sig(ctx.db, CallableDefId::FunctionId(func)).skip_binder();
    let mut generics = CGenericParams::empty();
    for (index, bound) in poly.bound_vars().iter().enumerate() {
        match bound {
            BoundVariableKind::Region(kind) => {
                let name = region_name(ctx, kind);
                let id = RegionId::from_usize(index);
                generics.regions.push(RegionParam {
                    late_bound: true,
                    ..RegionParam::new(id, name, Variance::Unknown)
                });
            }
            BoundVariableKind::Ty(_)
            | BoundVariableKind::Const
            | BoundVariableKind::Reflected(_) => {
                errors.push(refuse(
                    item,
                    "a late-bound type or const variable in the signature",
                    why::GENERICS,
                ));
            }
        }
    }

    let sig = poly.skip_binder();
    if sig.abi() != rustc_abi::ExternAbi::Rust {
        // alloc: an error
        errors.push(refuse(item, format!("the ABI `{:?}`", sig.abi()), why::UNSAFE));
    }
    if !sig.safety().is_safe() {
        errors.push(refuse(item, "an unsafe signature", why::UNSAFE));
    }
    let mut inputs = Vec::new(); // alloc: a Charon signature's inputs
    for input in sig.inputs().iter() {
        match types::ty(ctx, item, *input, &mut Regions::Signature) {
            Ok(it) => inputs.push(it),
            Err(err) => errors.push(err),
        }
    }
    let output = match types::ty(ctx, item, sig.output(), &mut Regions::Signature) {
        Ok(it) => Some(it),
        Err(err) => {
            errors.push(err);
            None
        }
    };
    match output {
        Some(output) if errors.is_empty() => Ok(Signature {
            generics,
            sig: FunSig { is_unsafe: false, abi: Abi::Rust, is_variadic: false, inputs, output },
        }),
        _ => Err(errors),
    }
}

/// A late-bound lifetime's name as charon-driver gives it: the name with its tick (`'a`), none
/// for an anonymous or elided (`'_`) one.
fn region_name<'db>(ctx: &Ctx<'db>, kind: BoundRegionKind<'db>) -> Option<String> {
    let name = kind.get_name(ctx.interner)?;
    let name = name.0.as_str();
    (name != "'_").then(|| name.to_owned()) // alloc: a region's name
}
