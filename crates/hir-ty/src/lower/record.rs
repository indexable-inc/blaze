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

use hir_def::{
    ExpressionStoreOwnerId, GenericDefId,
    expr_store::path::Names,
    signatures::FunctionSignature,
    type_ref::{RecordDefaults, TypeRefId},
};

use super::TyLoweringContext;
use crate::{RecordRefusal, TyLoweringDiagnostic, next_solver::Ty};

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
        let in_parameter = self.is_fn_parameter(source);
        for (at, default) in defaults.iter().enumerate() {
            let Some(default) = default else { continue };
            match in_parameter {
                true => _ = self.lower_const(default, tys[at], false),
                false => refuse(self, RecordRefusal::DefaultOutsideParameter(at as u32)),
            }
        }
        let list = hir_def::record::joined(order.iter().map(|&at| written[at]));
        let mut canonical = Vec::with_capacity_in(tys.len(), self.frame);
        canonical.extend(order.iter().map(|&at| tys[at]));
        crate::record::record_ty(self.interner, record, &list, &canonical)
    }

    /// Whether the type ref `ty` of the store being lowered is the whole type of a parameter
    /// of the function whose signature that store is: the one place a record's fields have
    /// defaults.
    pub(super) fn is_fn_parameter(&self, ty: TypeRefId) -> bool {
        match self.def {
            ExpressionStoreOwnerId::Signature(GenericDefId::FunctionId(function)) => {
                FunctionSignature::of(self.db, function).params.contains(&ty)
            }
            _ => false,
        }
    }
}
