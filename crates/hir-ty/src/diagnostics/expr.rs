//! Various diagnostics for expressions that are collected together in one pass
//! through the body using inference results: mismatched arg counts, missing
//! fields, etc.

use base_db::Db;
use blaze_hash::HashSet;
use either::Either;
use hir_def::{
    AdtId, AssocItemId, CallableDefId, DefWithBodyId, EnumVariantId, HasModule, ItemContainerId,
    Lookup,
    attrs::AttrFlags,
    expr_store::scope::ExprScopes,
    lang_item::LangItems,
    resolver::{ValueNs, resolver_for_scope},
};
use intern::sym;
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind};
use syntax::{AstNode, ast};

use crate::{
    InferenceResult,
    db::HirDatabase,
    inhabitedness::is_ty_uninhabited_from,
    next_solver::{
        CallableIdWrapper, DbInterner, ParamEnv, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
};

pub(crate) use hir_def::{
    LocalFieldId, VariantId,
    expr_store::Body,
    hir::{Expr, ExprId, MatchArm, Pat, PatId, RecordSpread, Statement, Statements},
};

pub enum BodyValidationDiagnostic<'db> {
    RecordMissingFields {
        record: Either<ExprId, PatId>,
        variant: VariantId,
        missed_fields: Vec<LocalFieldId>,
    },
    ReplaceFilterMapNextWithFindMap {
        method_call_expr: ExprId,
    },
    RemoveTrailingReturn {
        return_expr: ExprId,
    },
    RemoveUnnecessaryElse {
        if_expr: ExprId,
    },
    UnusedMustUse {
        expr: ExprId,
        message: Option<&'db str>,
    },
}

impl<'db> BodyValidationDiagnostic<'db> {
    pub fn collect(
        db: &'db Db,
        owner: DefWithBodyId,
        validate_lints: bool,
    ) -> Vec<BodyValidationDiagnostic<'db>> {
        let _p = tracing::info_span!("BodyValidationDiagnostic::collect").entered();
        let infer = InferenceResult::of(db, owner);
        let body = Body::of(db, owner);
        let env = db.trait_environment(owner.generic_def(db));
        let interner = DbInterner::new_with(db, owner.krate(db));
        let infcx =
            interner.infer_ctxt().build(TypingMode::typeck_for_body(interner, owner.into()));
        let mut validator = ExprValidator {
            owner,
            body,
            infer,
            diagnostics: Vec::new(),
            validate_lints,
            env,
            infcx,
        };
        validator.validate_body();
        validator.diagnostics
    }
}

struct ExprValidator<'db> {
    owner: DefWithBodyId,
    body: &'db Body,
    infer: &'db InferenceResult<'db>,
    env: ParamEnv<'db>,
    diagnostics: Vec<BodyValidationDiagnostic<'db>>,
    validate_lints: bool,
    infcx: InferCtxt<'db>,
}

impl<'db> ExprValidator<'db> {
    #[inline]
    fn db(&self) -> &'db Db {
        self.infcx.interner.db
    }

    fn validate_body(&mut self) {
        let db = self.db();
        let mut filter_map_next_checker = None;
        // we'll pass &mut self while iterating over body.exprs, so they need to be disjoint
        let body = self.body;

        if matches!(self.owner, DefWithBodyId::FunctionId(_)) {
            self.check_for_trailing_return(body.root_expr(), body);
        }

        for (id, expr) in body.exprs() {
            let missing = record_literal_missing_fields(db, self.infer, id, expr).or_else(|| {
                Some((record_literal_spread_only(self.infer, id, expr)?, Vec::new()))
            });
            if let Some((variant, missed_fields)) = missing {
                self.diagnostics.push(BodyValidationDiagnostic::RecordMissingFields {
                    record: Either::Left(id),
                    variant,
                    missed_fields,
                });
            }

            match expr {
                Expr::Call { .. } | Expr::MethodCall { .. } => {
                    self.validate_call(id, expr, &mut filter_map_next_checker);
                }
                Expr::Closure { body: body_expr, .. } => {
                    self.check_for_trailing_return(body_expr, body);
                }
                Expr::If { .. } => {
                    self.check_for_unnecessary_else(id, expr);
                }
                Expr::Block { statements, .. } => {
                    self.validate_block(statements);
                }
                _ => {}
            }
        }
    }

    fn validate_call(
        &mut self,
        call_id: ExprId,
        expr: Expr<'_>,
        filter_map_next_checker: &mut Option<FilterMapNextChecker<'db>>,
    ) {
        if !self.validate_lints {
            return;
        }
        // Check that the number of arguments matches the number of parameters.

        if self.infer.exprs_have_type_mismatches() {
            // FIXME: Due to shortcomings in the current type system implementation, only emit
            // this diagnostic if there are no type mismatches in the containing function.
        } else if let Expr::MethodCall { receiver, .. } = expr {
            let (callee, _) = match self.infer.method_resolution(call_id) {
                Some(it) => it,
                None => return,
            };

            let checker = filter_map_next_checker.get_or_insert_with(|| {
                FilterMapNextChecker::new(self.infcx.interner.lang_items(), self.db())
            });

            if checker.check(call_id, &receiver, &callee).is_some() {
                self.diagnostics.push(BodyValidationDiagnostic::ReplaceFilterMapNextWithFindMap {
                    method_call_expr: call_id,
                });
            }

            if let Some(receiver_ty) = self.infer.type_of_expr_with_adjust(receiver) {
                checker.prev_receiver_ty = Some(receiver_ty);
            }
        }
    }

    /// `unused_must_use` is rustc's lint, warned by default: it runs whatever
    /// `validate_lints` (rust-analyzer's own style lints) says.
    fn validate_block(&mut self, statements: Statements<'_>) {
        let mut diags = std::mem::take(&mut self.diagnostics);
        for stmt in statements {
            if let Statement::Expr { expr: stmt_expr, has_semi: true } = stmt {
                self.check_unused_must_use(stmt_expr, &mut diags);
            }
        }
        self.diagnostics = diags;
    }

    fn check_unused_must_use(
        &self,
        mut expr: ExprId,
        acc: &mut Vec<BodyValidationDiagnostic<'db>>,
    ) {
        // As rustc's `UnusedResults`: the value of the statement, through the tails of
        // blocks (`unsafe` and labeled ones too) but not into `if`, `match` or `const`.
        while let Expr::Block { tail: Some(tail), .. } = self.body.expr(expr) {
            expr = tail;
        }

        let fn_def = match self.body.expr(expr) {
            Expr::Call { callee, .. } if self.names_an_item(callee) => {
                let callee_ty = self.infer.expr_ty(callee);
                if let TyKind::FnDef(CallableIdWrapper(CallableDefId::FunctionId(func)), _) =
                    callee_ty.kind()
                {
                    Some(func.into())
                } else {
                    None
                }
            }
            Expr::MethodCall { .. } => {
                self.infer.method_resolution(expr).map(|(func, _)| func.into())
            }
            _ => None,
        };
        let ty_def = self
            .infer
            .type_of_expr_with_adjust(expr)
            .and_then(|ty| self.must_use_adt(ty))
            .map(Into::into);
        let must_use_diag = |owner| {
            AttrFlags::must_use_message(self.db(), owner?)
                .map(|message| BodyValidationDiagnostic::UnusedMustUse { expr, message })
        };
        if let Some(diag) = must_use_diag(fn_def).or_else(|| must_use_diag(ty_def)) {
            acc.push(diag);
        }
    }

    /// Whether `callee` is a path to an item, as rustc's `check_fn_must_use` asks: a local
    /// that holds a function is not one.
    fn names_an_item(&self, callee: ExprId) -> bool {
        let Expr::Path(path) = self.body.expr(callee) else { return false };
        let db = self.db();
        let scope = ExprScopes::of(db, self.owner).scope_for(callee);
        let resolved = resolver_for_scope(db, self.owner, scope).resolve_path_in_value_ns_fully(
            db,
            path,
            self.body.expr_path_hygiene(callee),
        );
        !matches!(resolved, Some(ValueNs::LocalBinding(_)))
    }

    /// The ADT whose `#[must_use]` makes a value of `ty` one to use, as rustc's
    /// `is_ty_must_use` has it: none for an uninhabited type, and `Result<T, E>` and
    /// `ControlFlow<B, T>` with `E` or `B` uninhabited are what `T` is.
    fn must_use_adt(&self, ty: Ty<'db>) -> Option<AdtId> {
        let TyKind::Adt(adt, args) = ty.kind() else { return None };
        let db = self.db();
        let module = self.owner.module(db);
        let uninhabited = |ty| is_ty_uninhabited_from(&self.infcx, ty, module, self.env);
        if uninhabited(ty) {
            return None;
        }
        let lang = self.infcx.interner.lang_items();
        let enum_of = |variant: Option<EnumVariantId>| variant.map(|it| it.lookup(db).parent);
        let id = adt.def_id();
        if let AdtId::EnumId(it) = id {
            if Some(it) == enum_of(lang.ResultOk) && uninhabited(args.type_at(1)) {
                return self.must_use_adt(args.type_at(0));
            }
            if Some(it) == enum_of(lang.ControlFlowBreak) && uninhabited(args.type_at(0)) {
                return self.must_use_adt(args.type_at(1));
            }
        }
        Some(id)
    }

    fn check_for_trailing_return(&mut self, body_expr: ExprId, body: &Body) {
        if !self.validate_lints {
            return;
        }
        match body.expr(body_expr) {
            Expr::Block { statements, tail, .. } => {
                let last_stmt = tail.or_else(|| match statements.last()? {
                    Statement::Expr { expr, .. } => Some(expr),
                    _ => None,
                });
                if let Some(last_stmt) = last_stmt {
                    self.check_for_trailing_return(last_stmt, body);
                }
            }
            Expr::If { then_branch, else_branch, .. } => {
                self.check_for_trailing_return(then_branch, body);
                if let Some(else_branch) = else_branch {
                    self.check_for_trailing_return(else_branch, body);
                }
            }
            Expr::Match { arms, .. } => {
                for arm in arms.iter() {
                    let MatchArm { expr, .. } = arm;
                    self.check_for_trailing_return(*expr, body);
                }
            }
            Expr::Return { .. } => {
                self.diagnostics.push(BodyValidationDiagnostic::RemoveTrailingReturn {
                    return_expr: body_expr,
                });
            }
            _ => (),
        }
    }

    fn check_for_unnecessary_else(&mut self, id: ExprId, expr: Expr<'_>) {
        if !self.validate_lints {
            return;
        }
        if let Expr::If { condition: _, then_branch, else_branch } = expr {
            if else_branch.is_none() {
                return;
            }
            if let Expr::Block { statements, tail, .. } = self.body.expr(then_branch) {
                let last_then_expr = tail.or_else(|| match statements.last()? {
                    Statement::Expr { expr, .. } => Some(expr),
                    _ => None,
                });
                if let Some(last_then_expr) = last_then_expr
                    && let Some(last_then_expr_ty) =
                        self.infer.type_of_expr_with_adjust(last_then_expr)
                    && last_then_expr_ty.is_never()
                {
                    // Only look at sources if the then branch diverges and we have an else branch.
                    let source_map = &Body::with_source_map(self.db(), self.owner).1;
                    let Ok(source_ptr) = source_map.expr_syntax(id) else {
                        return;
                    };
                    let root = source_ptr.file_syntax(self.db());
                    let either::Left(ast::Expr::IfExpr(if_expr)) = source_ptr.value.to_node(&root)
                    else {
                        return;
                    };
                    let mut top_if_expr = if_expr;
                    loop {
                        let parent = top_if_expr.syntax().parent();
                        let has_parent_expr_stmt_or_stmt_list =
                            parent.as_ref().is_some_and(|node| {
                                ast::ExprStmt::can_cast(node.kind())
                                    | ast::StmtList::can_cast(node.kind())
                            });
                        if has_parent_expr_stmt_or_stmt_list {
                            // Only emit diagnostic if parent or direct ancestor is either
                            // an expr stmt or a stmt list.
                            break;
                        }
                        let Some(parent_if_expr) = parent.and_then(ast::IfExpr::cast) else {
                            // Bail if parent is neither an if expr, an expr stmt nor a stmt list.
                            return;
                        };
                        // Check parent if expr.
                        top_if_expr = parent_if_expr;
                    }

                    self.diagnostics
                        .push(BodyValidationDiagnostic::RemoveUnnecessaryElse { if_expr: id })
                }
            }
        }
    }
}

struct FilterMapNextChecker<'db> {
    filter_map_function_id: Option<hir_def::FunctionId>,
    next_function_id: Option<hir_def::FunctionId>,
    prev_filter_map_expr_id: Option<ExprId>,
    prev_receiver_ty: Option<Ty<'db>>,
}

impl<'db> FilterMapNextChecker<'db> {
    fn new(lang_items: &'db LangItems, db: &'db Db) -> Self {
        // Find and store the FunctionIds for Iterator::filter_map and Iterator::next
        let (next_function_id, filter_map_function_id) = match lang_items.IteratorNext {
            Some(next_function_id) => (
                Some(next_function_id),
                match next_function_id.lookup(db).container {
                    ItemContainerId::TraitId(iterator_trait_id) => {
                        let iterator_trait_items = &iterator_trait_id.trait_items(db).items;
                        iterator_trait_items.iter().find_map(|(name, it)| match it {
                            &AssocItemId::FunctionId(id) if *name == sym::filter_map => Some(id),
                            _ => None,
                        })
                    }
                    _ => None,
                },
            ),
            None => (None, None),
        };
        Self {
            filter_map_function_id,
            next_function_id,
            prev_filter_map_expr_id: None,
            prev_receiver_ty: None,
        }
    }

    // check for instances of .filter_map(..).next()
    fn check(
        &mut self,
        current_expr_id: ExprId,
        receiver_expr_id: &ExprId,
        function_id: &hir_def::FunctionId,
    ) -> Option<()> {
        if *function_id == self.filter_map_function_id? {
            self.prev_filter_map_expr_id = Some(current_expr_id);
            return None;
        }

        if *function_id == self.next_function_id?
            && let Some(prev_filter_map_expr_id) = self.prev_filter_map_expr_id
        {
            let is_dyn_trait = self
                .prev_receiver_ty
                .as_ref()
                .is_some_and(|it| it.strip_references().dyn_trait().is_some());
            if *receiver_expr_id == prev_filter_map_expr_id && !is_dyn_trait {
                return Some(());
            }
        }

        self.prev_filter_map_expr_id = None;
        None
    }
}

pub fn record_literal_missing_fields<'db>(
    db: &'db Db,
    infer: &InferenceResult<'db>,
    id: ExprId,
    expr: Expr<'_>,
) -> Option<(VariantId, Vec<LocalFieldId>)> {
    let (fields, spread) = match expr {
        Expr::RecordLit { fields, spread, .. } => (fields, spread),
        _ => return None,
    };

    let variant_def = infer.variant_resolution_for_expr(id)?;
    if let VariantId::UnionId(_) = variant_def {
        return None;
    }

    let variant_data = variant_def.fields(db);

    let specified_fields: HashSet<_> = fields.iter().map(|f| &f.name).collect();
    // don't show missing fields if:
    // - has ..expr
    // - or has default value + ..
    // - or already in code
    let missed_fields: Vec<LocalFieldId> = variant_data
        .fields()
        .iter()
        .filter_map(|(f, d)| {
            if specified_fields.contains(&d.name)
                || matches!(spread, RecordSpread::Expr(_))
                || (d.default_value.is_some() && matches!(spread, RecordSpread::FieldDefaults))
            {
                None
            } else {
                Some(f)
            }
        })
        .collect();
    if missed_fields.is_empty() {
        return None;
    }
    Some((variant_def, missed_fields))
}

/// A struct expression with `..` and no base that leaves out no field without a default:
/// rustc still gives E0797 for it (the `..` is needless, or needs `default_field_values`).
fn record_literal_spread_only(
    infer: &InferenceResult<'_>,
    id: ExprId,
    expr: Expr<'_>,
) -> Option<VariantId> {
    let Expr::RecordLit { spread: RecordSpread::FieldDefaults, .. } = expr else { return None };
    infer.variant_resolution_for_expr(id).filter(|it| !matches!(it, VariantId::UnionId(_)))
}

pub fn record_pattern_missing_fields<'db>(
    db: &'db Db,
    infer: &InferenceResult<'db>,
    id: PatId,
    pat: Pat<'_>,
) -> Option<(VariantId, Vec<LocalFieldId>)> {
    let (fields, ellipsis) = match pat {
        Pat::Record { path: _, args, ellipsis } => (args, ellipsis),
        _ => return None,
    };

    let variant_def = infer.variant_resolution_for_pat(id)?;
    if let VariantId::UnionId(_) = variant_def {
        return None;
    }

    let variant_data = variant_def.fields(db);

    let specified_fields: HashSet<_> = fields.iter().map(|f| &f.name).collect();
    // don't show missing fields if:
    // - in code
    // - or has ..
    let missed_fields: Vec<LocalFieldId> = variant_data
        .fields()
        .iter()
        .filter_map(
            |(f, d)| {
                if specified_fields.contains(&d.name) || ellipsis { None } else { Some(f) }
            },
        )
        .collect();
    if missed_fields.is_empty() {
        return None;
    }
    Some((variant_def, missed_fields))
}
