//! blaze's record types (`#![blaze::records]`, docs/v2/plan/design/records.md):
//! `{ a: A, b: B }` lowers to core's `Record<"a,b", (A, B)>`, the fields in canonical order
//! (`hir_def::record`), so that the order they are written in is no part of the type
//! (`crate::record` makes the type).
//!
//! A field's default (`= expr`) is no part of the type either. It is read only where the
//! record is the type of a function's parameter: there it is lowered as an anonymous constant
//! of the function's signature, type checked against the field's type like an array's length,
//! and `record_param_defaults` hands it to the calls that leave the field out. Anywhere else
//! a default is refused.

use base_db::Db;
use hir_def::{
    ExpressionStoreOwnerId, FunctionId, GenericDefId, ItemContainerId, Lookup,
    expr_store::path::Names,
    resolver::HasResolver,
    signatures::FunctionSignature,
    type_ref::{RecordDefaults, TypeRef, TypeRefId},
};
use hir_expand::name::Name;

use super::{LifetimeElisionKind, LifetimeLoweringMode, TyLoweringContext};
use crate::{
    RecordRefusal, TyLoweringDiagnostic,
    db::HirDatabase,
    generics::generics,
    next_solver::{ConstExt as _, StoredConst, StoredEarlyBinder, Ty},
};

/// The default of one field of a record parameter of a function: `mode: u32 = 0o644` of
/// `fn open(path: &Path, opts: { create: bool = false, mode: u32 = 0o644 })`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecordParamDefault {
    /// The parameter's index among the function's parameters, `self` first.
    pub param: u32,
    /// The field's name.
    pub field: Name,
    /// The default: a constant of the function's signature, which may name what the signature
    /// can and no parameter.
    pub value: StoredEarlyBinder<StoredConst>,
}

/// The function whose declaration gives `function`'s record parameters their defaults:
/// `function` itself, or for a method of a trait impl the trait's method of its name, since a
/// trait method's defaults are its declaration's and an impl writes none.
pub fn record_defaults_owner(db: &Db, function: FunctionId) -> FunctionId {
    let ItemContainerId::ImplId(impl_) = function.lookup(db).container else { return function };
    let Some(trait_ref) = db.impl_trait(impl_) else { return function };
    let trait_ = trait_ref.skip_binder().def_id.0;
    let name = &FunctionSignature::of(db, function).name;
    trait_.trait_items(db).method_by_name(name).unwrap_or(function)
}

/// Every default of the record parameters of `function`'s own declaration, in the order
/// written. A call reads one of them through [`record_param_default`].
#[blaze_query::tracked(returns(deref), mem_tag = Infer)]
pub fn record_param_defaults(db: &Db, function: FunctionId) -> Box<[RecordParamDefault]> {
    let data = FunctionSignature::of(db, function);
    let has_defaults = |param: &TypeRefId| match data.store.type_ref(*param) {
        TypeRef::Record { defaults, .. } => defaults.any(),
        _ => false,
    };
    if !data.params.iter().any(has_defaults) {
        return Box::default();
    }
    blaze_mem::frame::current(|frame| {
        let resolver = function.resolver(db);
        let generics = generics(db, function.into());
        let mut ctx = TyLoweringContext::new(
            db,
            &resolver,
            &data.store,
            ExpressionStoreOwnerId::Signature(function.into()),
            function.into(),
            &generics,
            LifetimeElisionKind::for_fn_params(data),
            LifetimeLoweringMode::Bound,
            frame,
        );
        let mut found = Vec::new(); // alloc: the memo's column
        for (index, &param) in data.params.iter().enumerate() {
            let TypeRef::Record { names, types, defaults } = data.store.type_ref(param) else {
                continue;
            };
            let names = data.store.names(names);
            for (at, default) in defaults.iter().enumerate() {
                let Some(default) = default else { continue };
                // The constant the signature's own lowering made for this default
                // (`TyLoweringContext::lower_record`): the same expression of the same store
                // against the same type.
                let ty = ctx.lower_ty(types[at]);
                let value = ctx.lower_const(default, ty, false);
                found.push(RecordParamDefault {
                    param: index as u32,
                    field: names[at],
                    value: StoredEarlyBinder::bind(ctx.interner, value.store()),
                });
            }
        }
        found.into_boxed_slice()
    })
}

/// The default of the field `field` of parameter `param` (`self` first) of `function`, from the
/// declaration that gives it ([`record_defaults_owner`]): what a call `f({ .., })` that leaves
/// the field out passes. `None` when the field has no default, and must be written.
///
/// The key (rule `keys-cover-every-input`) is the function, the parameter and the field, and
/// the value is that one constant: a caller reads it only for a field it leaves out, so a
/// changed default recomputes the callers that leave that field out and no other. A default
/// is no part of the record's type or of the function's signature type.
#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub fn record_param_default(
    db: &Db,
    function: FunctionId,
    param: u32,
    field: Name,
) -> Option<StoredEarlyBinder<StoredConst>> {
    let owner = record_defaults_owner(db, function);
    record_param_defaults(db, owner)
        .iter()
        .find(|default| default.param == param && default.field == field)
        .map(|default| default.value)
}

impl<'db> TyLoweringContext<'db, '_> {
    /// The record type written at `source` with the fields `names`, `types` and `defaults`, in
    /// the order written, or the error type once a refusal is reported.
    pub(super) fn lower_record(
        &mut self,
        source: TypeRefId,
        names: Names,
        types: &[TypeRefId],
        defaults: RecordDefaults<'_>,
    ) -> Ty<'db> {
        let error = self.types.types.error;
        let refuse = |this: &mut Self, reason| {
            this.push_diagnostic(TyLoweringDiagnostic::RecordRefused { source, reason });
        };
        // The fields' types first: their own errors are reported whatever becomes of the
        // record.
        let mut tys = Vec::with_capacity_in(types.len(), self.frame);
        for &ty in types {
            tys.push(self.lower_ty(ty));
        }
        if !base_db::crate_extensions(self.db, self.resolver.krate()).records() {
            refuse(self, RecordRefusal::NotEnabled);
            return error;
        }
        let Some(record) = self.lang_items.Record else {
            refuse(self, RecordRefusal::NoLangItem);
            return error;
        };
        let store = self.store;
        let names = store.names(names);
        let mut written = Vec::with_capacity_in(names.len(), self.frame);
        written.extend(names.iter().map(|name| name.as_str()));
        let order = match hir_def::record::canonical_order(&written) {
            Ok(order) => order,
            Err(at) => {
                refuse(self, RecordRefusal::DuplicateField(at as u32));
                return error;
            }
        };
        // Defaults: constants of the function whose parameter this is, checked against their
        // fields' types here so that their errors are the signature's.
        let parameter_of = self.fn_parameter(source);
        if let Some(function) = parameter_of
            && !FunctionSignature::of(self.db, function).abi.is_rustic_abi()
        {
            // No ABI but Rust's says how a record is passed.
            refuse(self, RecordRefusal::ExternAbi);
            return error;
        }
        let in_trait_impl = parameter_of
            .is_some_and(|function| record_defaults_owner(self.db, function) != function);
        for (at, default) in defaults.iter().enumerate() {
            let Some(default) = default else { continue };
            match parameter_of {
                Some(_) if in_trait_impl => {
                    refuse(self, RecordRefusal::DefaultInTraitImpl(at as u32))
                }
                Some(_) => _ = self.lower_const(default, tys[at], false),
                None => refuse(self, RecordRefusal::DefaultOutsideParameter(at as u32)),
            }
        }
        let list = hir_def::record::joined(order.iter().map(|&at| written[at]));
        let mut canonical = Vec::with_capacity_in(tys.len(), self.frame);
        canonical.extend(order.iter().map(|&at| tys[at]));
        crate::record::record_ty(self.interner, record, &list, &canonical)
    }

    /// The function whose signature is being lowered, when the type ref `ty` of its store is
    /// the whole type of one of its parameters: the one place a record's fields have defaults.
    pub(super) fn fn_parameter(&self, ty: TypeRefId) -> Option<FunctionId> {
        match self.def {
            ExpressionStoreOwnerId::Signature(GenericDefId::FunctionId(function))
                if FunctionSignature::of(self.db, function).params.contains(&ty) =>
            {
                Some(function)
            }
            _ => None,
        }
    }
}
