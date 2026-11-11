//! Type checking blaze's records (`#![blaze::records]`, docs/v2/plan/design/records.md): the
//! record value `{ a: 1, b }`, the record pattern `{ a, b: x }`, and the defaults a record
//! argument takes from its callee (`f({ a: 1, .. })`).
//!
//! hir-def lowers a record value to a struct literal, and a record pattern to a struct
//! pattern, whose path is core's `#[lang = "record"]` struct; here that path is read as "a
//! record", and the fields are named against the record's type (`crate::record`), never
//! against the struct's one field. A record's type comes from its literal when every field is
//! written, or from the type expected of it; it is never inferred from how a field is used.

use hir_def::{
    FunctionId, StructId,
    expr_store::path::Path,
    hir::{ExprId, PatId, RecordFieldPat, RecordLitField, RecordSpread},
    lang_item::LangItemTarget,
};
use hir_expand::name::Name;
use rustc_type_ir::inherent::{IntoKind as _, Ty as _};

use super::{
    Expectation, InferenceContext, InferenceDiagnostic, expr::ExprIsRead, pat::PatInfo,
};
use crate::{
    next_solver::{Ty, TyKind},
    record::{RecordFields, record_fields, record_ty},
};

/// Why a record value or a record pattern is refused
/// ([`InferenceDiagnostic::RecordRefused`]).
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub enum RecordProblem {
    /// The crate does not say `#![blaze::records]` plainly among the inner attributes that
    /// open its root: its file is parsed with the syntax only because another crate of its
    /// source root says it.
    NotEnabled,
    /// A value with `..` (`{ a: 1, ..base }`, `{ a: 1, .. }`), or a pattern with `..`, where
    /// no record type is known for it: a record's type comes from a literal that writes every
    /// field or from an annotation, never from how its fields are used.
    TypeUnknown,
    /// A value with `..` or a pattern, where the type expected is known and is no record.
    NotARecord,
    /// The record has no field of this name.
    NoSuchField(Name),
    /// The field is written twice.
    DuplicateField(Name),
    /// The field is not written, and nothing gives the rest: a value without `..`, or a
    /// pattern without `..`.
    MissingField(Name),
    /// `{ .., }` leaves the field out and `function`, the callee, declares no default for it.
    MissingDefault { field: Name, function: FunctionId },
    /// `{ a: 1, .. }` where the record is not an argument of a call to a function by name: a
    /// function pointer, a closure and a `Fn` value have no defaults, which belong to a
    /// function's declaration.
    NoCallee,
}

impl<'db, 'f> InferenceContext<'db, 'f> {
    /// The lang item `record`, when `path` is the path hir-def gives a record value or a
    /// record pattern, which are written with none.
    pub(super) fn record_path(&self, path: Path<'_>) -> Option<StructId> {
        match path {
            Path::LangItem(LangItemTarget::StructId(it), None)
                if self.lang_items.Record == Some(it) =>
            {
                Some(it)
            }
            _ => None,
        }
    }

    fn refuse_record(
        &self,
        node: impl Into<hir_def::hir::ExprOrPatIdPacked>,
        problem: RecordProblem,
    ) {
        self.push_diagnostic(InferenceDiagnostic::RecordRefused { node: node.into(), problem });
    }

    /// Whether the crate being checked turns records on.
    fn records_enabled(&self) -> bool {
        base_db::crate_extensions(self.db, self.resolver.krate()).records()
    }

    /// Notes each of `args` that is a record value as an argument of a call to `function`:
    /// argument `i` is the parameter `first + i` (`first` is 1 for a method call's arguments,
    /// after the receiver). Called before the arguments are checked.
    pub(super) fn note_record_arguments(
        &mut self,
        function: FunctionId,
        first: u32,
        args: &[ExprId],
    ) {
        for (at, &arg) in args.iter().enumerate() {
            if let hir_def::hir::Expr::RecordLit { path, .. } = self.store.expr(arg)
                && self.record_path(path).is_some()
            {
                self.record_callees.push((arg, function, first + at as u32));
            }
        }
    }

    /// The type of the record value `expr` with the fields `fields`, and after them `spread`:
    /// nothing, a base record (`..base`), or the callee's defaults (`..`).
    pub(super) fn infer_record_value(
        &mut self,
        expr: ExprId,
        record: StructId,
        expected: &Expectation<'db>,
        fields: &[RecordLitField],
        spread: RecordSpread,
    ) -> Ty<'db> {
        let error = self.types.types.error;
        let expected_ty = expected
            .only_has_type(&mut self.table)
            .map(|ty| self.table.try_structurally_resolve_type(expr.into(), ty));
        let expected_record = expected_ty.and_then(|ty| record_fields(self.lang_items, ty));
        if !self.records_enabled() {
            self.refuse_record(expr, RecordProblem::NotEnabled);
            self.infer_record_rest(fields, spread);
            return error;
        }
        match (spread, expected_record) {
            // Every field is written and no record is expected: the literal is the type.
            (RecordSpread::None, None) => {
                let mut tys = Vec::with_capacity_in(fields.len(), self.frame);
                for field in fields {
                    tys.push(self.infer_expr_no_expect(field.expr, ExprIsRead::Yes));
                }
                let mut names = Vec::with_capacity_in(fields.len(), self.frame);
                names.extend(fields.iter().map(|field| field.name.as_str()));
                let order = match hir_def::record::canonical_order(&names) {
                    Ok(order) => order,
                    Err(at) => {
                        self.refuse_record(expr, RecordProblem::DuplicateField(fields[at].name));
                        return error;
                    }
                };
                let list = hir_def::record::joined(order.iter().map(|&at| names[at]));
                let mut canonical = Vec::with_capacity_in(tys.len(), self.frame);
                canonical.extend(order.iter().map(|&at| tys[at]));
                record_ty(self.interner(), record, &list, &canonical)
            }
            // A record is expected: each field written is one of its fields and coerces to
            // that field's type, and the rest come from `spread`.
            (_, Some(of)) => {
                let expected_ty = expected_ty.unwrap_or(error);
                let written = self.check_record_fields(expr, &of, fields);
                match spread {
                    RecordSpread::None => {
                        for (at, name) in of.names().enumerate() {
                            if !written[at] {
                                let field = Name::new_root(name);
                                self.refuse_record(expr, RecordProblem::MissingField(field));
                            }
                        }
                    }
                    RecordSpread::Expr(base) => {
                        self.infer_expr_coerce(
                            base,
                            &Expectation::has_type(expected_ty),
                            ExprIsRead::Yes,
                        );
                    }
                    RecordSpread::FieldDefaults => self.check_record_defaults(expr, &of, &written),
                }
                expected_ty
            }
            // `..` with no record known for the value.
            (RecordSpread::Expr(_) | RecordSpread::FieldDefaults, None) => {
                match expected_ty {
                    Some(ty) if ty.is_ty_error() => {}
                    Some(ty) if !matches!(ty.kind(), TyKind::Infer(_)) => {
                        self.refuse_record(expr, RecordProblem::NotARecord)
                    }
                    _ => self.refuse_record(expr, RecordProblem::TypeUnknown),
                }
                self.infer_record_rest(fields, spread);
                error
            }
        }
    }

    /// Infers what a refused record value holds, with nothing expected of it, so that each
    /// expression in it has a type.
    fn infer_record_rest(&mut self, fields: &[RecordLitField], spread: RecordSpread) {
        for field in fields {
            self.infer_expr_no_expect(field.expr, ExprIsRead::Yes);
        }
        if let RecordSpread::Expr(base) = spread {
            self.infer_expr_no_expect(base, ExprIsRead::Yes);
        }
    }

    /// Checks the fields written in the record value `expr` against the record `of`: each is
    /// one of its fields, written once, and coerces to the field's type. Which of the
    /// record's fields, in canonical order, are written.
    fn check_record_fields(
        &mut self,
        expr: ExprId,
        of: &RecordFields<'db>,
        fields: &[RecordLitField],
    ) -> Vec<bool, &'f crate::Frame> {
        let mut written = Vec::with_capacity_in(of.len(), self.frame);
        written.resize(of.len(), false);
        for field in fields {
            match of.index_of(field.name.as_str()) {
                Some(at) if !written[at] => {
                    written[at] = true;
                    let ty = of.ty(at).unwrap_or(self.types.types.error);
                    self.infer_expr_coerce(field.expr, &Expectation::has_type(ty), ExprIsRead::Yes);
                }
                Some(_) => {
                    self.refuse_record(expr, RecordProblem::DuplicateField(field.name));
                    self.infer_expr_no_expect(field.expr, ExprIsRead::Yes);
                }
                None => {
                    self.refuse_record(expr, RecordProblem::NoSuchField(field.name));
                    self.infer_expr_no_expect(field.expr, ExprIsRead::Yes);
                }
            }
        }
        written
    }

    /// `{ a: 1, .. }`: each field of `of` not `written` takes its default from the function
    /// the record is an argument of, which must declare one.
    fn check_record_defaults(&mut self, expr: ExprId, of: &RecordFields<'db>, written: &[bool]) {
        let callee = self.record_callees.iter().find(|it| it.0 == expr).map(|it| (it.1, it.2));
        let Some((function, param)) = callee else {
            self.refuse_record(expr, RecordProblem::NoCallee);
            return;
        };
        for (at, name) in of.names().enumerate() {
            if written[at] {
                continue;
            }
            let field = Name::new_root(name);
            if crate::record_param_default(self.db, function, param, field).is_none() {
                self.refuse_record(expr, RecordProblem::MissingDefault { field, function });
            }
        }
    }

    /// The type of the record pattern `pat` matched against a value of type `expected`: the
    /// record `expected` is, each field's pattern checked against the field's type. Without
    /// `..` (`has_rest`) the pattern names every field, and so gives the record's fields when
    /// `expected` is not known yet.
    pub(super) fn infer_record_pattern(
        &mut self,
        pat: PatId,
        record: StructId,
        fields: &[RecordFieldPat],
        has_rest: bool,
        expected: Ty<'db>,
        pat_info: PatInfo,
    ) -> Ty<'db> {
        let error = self.types.types.error;
        let refuse = |this: &mut Self, problem: Option<RecordProblem>| {
            if let Some(problem) = problem {
                this.refuse_record(pat, problem);
            }
            for field in fields {
                this.infer_pat(field.pat, error, pat_info);
            }
            error
        };
        if !self.records_enabled() {
            return refuse(self, Some(RecordProblem::NotEnabled));
        }
        let expected = self.table.try_structurally_resolve_type(pat.into(), expected);
        let of = match record_fields(self.lang_items, expected) {
            Some(of) => of,
            None if expected.is_ty_error() => return refuse(self, None),
            // The pattern names every field: its names are the record's, and each field's
            // type is whatever its pattern and the value make it.
            None if !has_rest && matches!(expected.kind(), TyKind::Infer(_)) => {
                let mut names = Vec::with_capacity_in(fields.len(), self.frame);
                names.extend(fields.iter().map(|field| field.name.as_str()));
                let order = match hir_def::record::canonical_order(&names) {
                    Ok(order) => order,
                    Err(at) => {
                        return refuse(self, Some(RecordProblem::DuplicateField(fields[at].name)));
                    }
                };
                let list = hir_def::record::joined(order.iter().map(|&at| names[at]));
                let mut tys = Vec::with_capacity_in(fields.len(), self.frame);
                for _ in fields {
                    tys.push(self.table.next_ty_var(pat.into()));
                }
                let ty = record_ty(self.interner(), record, &list, &tys);
                let _ = self.demand_eqtype(pat.into(), expected, ty);
                match record_fields(self.lang_items, ty) {
                    Some(of) => of,
                    None => return refuse(self, None),
                }
            }
            None if matches!(expected.kind(), TyKind::Infer(_)) => {
                return refuse(self, Some(RecordProblem::TypeUnknown));
            }
            None => return refuse(self, Some(RecordProblem::NotARecord)),
        };
        let mut named = Vec::with_capacity_in(of.len(), self.frame);
        named.resize(of.len(), false);
        for field in fields {
            match of.index_of(field.name.as_str()) {
                Some(at) if !named[at] => {
                    named[at] = true;
                    self.infer_pat(field.pat, of.ty(at).unwrap_or(error), pat_info);
                }
                Some(_) => {
                    self.refuse_record(pat, RecordProblem::DuplicateField(field.name));
                    self.infer_pat(field.pat, error, pat_info);
                }
                None => {
                    self.refuse_record(pat, RecordProblem::NoSuchField(field.name));
                    self.infer_pat(field.pat, error, pat_info);
                }
            }
        }
        if !has_rest {
            for (at, name) in of.names().enumerate() {
                if !named[at] {
                    self.refuse_record(pat, RecordProblem::MissingField(Name::new_root(name)));
                }
            }
        }
        expected
    }
}
