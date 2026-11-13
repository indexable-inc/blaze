//! The walk of one body as it was before the walk was written down as records
//! ([`super::super::body::store_marks`]): the same rules, each name marked on the pass as it is
//! met. Kept as the reference the records' replay is checked equal to
//! (`dead::tests::the_marks_equal_the_whole_pass`), on every fixture of the pass.
//!
//! Follows `compiler/rustc_passes/src/dead.rs` (`MarkSymbolVisitor`) (MIT or Apache-2.0).

use std::mem;

use either::Either;
use hir::{InFile, Name};
use hir_def::{
    AdtId, DefWithBodyId, FieldId, VariantId,
    expr_store::{
        Body, BodySourceMap, ExpressionStore, HygieneId, StoreVisitor, StoreVisitorExt, path::Path,
    },
    hir::{
        AsmOperand, BindingId, ClosureKind, CoroutineSource, Expr, ExprId, ExprOrPatId, Pat, PatId,
        RecordLitField,
        type_ref::{TypeRef, TypeRefId},
    },
    resolver::ValueNs,
};
use hir_ty::{Adjust, InferenceResult, next_solver::Ty};
use smallvec::SmallVec;
use syntax::{
    SyntaxNodePtr,
    view::{self as ast, AstNode as _},
};

use super::super::{
    super::{
        defs::{DefIdx, Defs, Id},
        res::Res,
    },
    Mark,
};
use crate::{
    lints::names::uses::{BodyPaths, Named, Site},
    lower::{Found, Phase},
    model::Diag,
};

/// A path resolved ([`BodyPaths::resolve`]), in the crate's numbering.
#[derive(Clone, Copy)]
struct Resolved {
    res: Res,
    /// What its qualifier names, as the walk over syntax resolves the qualifier's own node.
    qualifier: Res,
    /// rustc's resolver resolves it (`QPath::Resolved`), not typeck through a type.
    resolved: bool,
}

pub(super) struct Walk<'w, 'k, 'c, 'a, 'db> {
    m: &'w mut Mark<'k, 'c, 'a, 'db>,
    store: &'w ExpressionStore,
    source_map: &'w BodySourceMap,
    infer: &'db InferenceResult<'db>,
    /// The body's paths, resolved; its resolver at the block the walk is in.
    paths: BodyPaths<'db>,
    /// The expression whose scope a path in the pattern or type being walked resolves in; none
    /// for the parameters, which see no scope of the body.
    scope: Option<ExprId>,
    /// What `necessary_variants` resolved of a match arm's patterns, taken by the walk of the
    /// arm.
    arm_paths: SmallVec<[(PatId, Resolved); 4]>,
}

impl<'w, 'k, 'c, 'a, 'db> Walk<'w, 'k, 'c, 'a, 'db> {
    pub(super) fn new(
        m: &'w mut Mark<'k, 'c, 'a, 'db>,
        owner: DefWithBodyId,
        body: &'w Body,
        source_map: &'w BodySourceMap,
    ) -> Self {
        let paths = BodyPaths::new(m.k.cx.db, owner);
        Walk {
            store: &body.store,
            source_map,
            infer: paths.infer,
            paths,
            scope: None,
            arm_paths: SmallVec::new(),
            m,
        }
    }

    /// The parameters' patterns (also the one an `async fn` copies each into), then the body.
    pub(super) fn run(mut self, body: &Body) {
        for param in &body.params {
            self.on_pat(param.formal);
            if param.user_written != param.formal {
                self.on_pat(param.user_written);
            }
        }
        self.on_expr(body.root_expr());
    }

    fn expr(&mut self, expr: ExprId) {
        let store = self.store;
        let defs = self.m.k.defs;
        match store.expr(expr) {
            Expr::Path(path) => {
                let r = self.resolve(
                    Some(ExprOrPatId::ExprId(expr)),
                    path,
                    Site::Value,
                    store.expr_path_hygiene(expr),
                );
                self.handle(r);
            }
            Expr::MethodCall { .. } => {
                if let Some((function, _)) = self.infer.method_resolution(expr) {
                    self.m.handle_res(Res::method(defs, function));
                }
            }
            Expr::Field { .. } => {
                if let Some(Either::Left(field)) = self.infer.field_resolution(expr)
                    && let Some(field) = defs.get(Id::Field(field))
                {
                    self.m.insert_def_id(field);
                }
            }
            Expr::RecordLit { path, fields, .. } => {
                let r = self.resolve(
                    Some(ExprOrPatId::ExprId(expr)),
                    path,
                    Site::RecordExpr,
                    HygieneId::ROOT,
                );
                self.handle(r);
                self.union_fields(expr, fields);
            }
            Expr::OffsetOf(_) => self.offset_of(expr),
            Expr::InlineAsm(asm) => {
                for (_, operand) in asm.operands.iter() {
                    if let AsmOperand::Sym(path) = operand {
                        let r =
                            self.resolve(None, store.path_at(*path), Site::Value, HygieneId::ROOT);
                        self.handle(r);
                    }
                }
            }
            Expr::Assignment { target, value } => {
                match store.pat(target) {
                    Pat::Expr(lhs) => {
                        self.handle_assign(lhs);
                        self.self_assign(expr, lhs, value);
                    }
                    _ => self.on_pat(target),
                }
                self.on_expr(value);
                return;
            }
            Expr::Match { expr: scrutinee, arms } => {
                self.on_expr(scrutinee);
                for arm in arms {
                    self.arm(arm.pat, arm.guard, arm.expr);
                }
                return;
            }
            Expr::Closure { closure_kind, .. } => {
                if !matches!(
                    closure_kind,
                    ClosureKind::Coroutine { source: CoroutineSource::Fn, .. }
                ) {
                    self.anon(expr, true, |this| store.visit_expr_children(expr, &mut *this));
                    return;
                }
            }
            Expr::Const(inner) => {
                self.anon(expr, false, |this| this.on_expr(inner));
                return;
            }
            Expr::Block { .. } => {
                let (db, owner) = (self.paths.db, self.paths.owner);
                let guard = self.paths.resolver.update_to_inner_scope(db, owner, expr);
                store.visit_expr_children(expr, &mut *self);
                self.paths.resolver.reset_to_guard(guard);
                return;
            }
            _ => {}
        }
        store.visit_expr_children(expr, &mut *self);
    }

    /// rustc's `visit_arm`: the variants the arm's pattern needs are not constructed by it.
    fn arm(&mut self, pat: PatId, guard: Option<ExprId>, expr: ExprId) {
        let len = self.m.ignore_variants.len();
        let mut stack = mem::take(&mut self.m.ignore_variants);
        self.necessary_variants(pat, &mut stack, len);
        self.m.ignore_variants = stack;
        self.on_pat(pat);
        if let Some(guard) = guard {
            self.on_expr(guard);
        }
        self.on_expr(expr);
        self.m.ignore_variants.truncate(len);
        self.arm_paths.clear();
    }

    /// rustc's `necessary_variants`: the variants a pattern's paths name (resolved, not
    /// through a type), outside `|` patterns, added to `out` past `from` once each.
    fn necessary_variants(&mut self, pat: PatId, out: &mut Vec<DefIdx>, from: usize) {
        let store = self.store;
        let (path, site) = match store.pat(pat) {
            Pat::Or(_) => return,
            Pat::Path(path) => (Some(path), Site::PathPat),
            Pat::TupleStruct { path, .. } => (Some(path), Site::TupleStructPat),
            Pat::Record { path, .. } => (Some(path), Site::RecordPat),
            _ => (None, Site::PathPat),
        };
        if let Some(path) = path {
            let r = self.resolve(
                Some(ExprOrPatId::PatId(pat)),
                path,
                site,
                store.pat_path_hygiene(pat),
            );
            self.arm_paths.push((pat, r));
            if r.resolved
                && let Res::Variant(variant) = r.res
                && !out[from..].contains(&variant)
            {
                out.push(variant);
            }
        }
        store.walk_pats_shallow(pat, |child| self.necessary_variants(child, out, from));
    }

    /// rustc's `visit_pat`: what a pattern's paths name, its ADT, and the fields it binds.
    fn pat(&mut self, pat: PatId) {
        let store = self.store;
        self.m.in_pat = true;
        match store.pat(pat) {
            Pat::Record { path, args, .. } => {
                self.pat_adt(pat);
                let r = self.pat_path(pat, path, Site::RecordPat);
                self.handle(r);
                StoreVisitorExt::on_path(self, path);
                if let Some(variant) = self.infer.variant_resolution_for_pat(pat) {
                    for arg in args {
                        if !matches!(store.pat(arg.pat), Pat::Wild)
                            && let Some(field) = self.field(
                                variant,
                                &arg.name,
                                self.store.record_field_hygiene(arg.pat),
                            )
                        {
                            self.m.insert_def_id(field);
                        }
                    }
                }
                for arg in args {
                    self.on_pat(arg.pat);
                }
            }
            Pat::TupleStruct { path, args, ellipsis } => {
                let adt = self.pat_adt(pat);
                let r = self.pat_path(pat, path, Site::TupleStructPat);
                self.handle(r);
                StoreVisitorExt::on_path(self, path);
                if let Some(variant) = self.m.variant_of(r.res, adt) {
                    let wildcard = args.iter().map(|&it| matches!(store.pat(it), Pat::Wild));
                    self.m.tuple_fields(
                        variant,
                        wildcard,
                        args.len(),
                        ellipsis.map(|it| it as usize),
                    );
                }
                for &arg in args {
                    self.on_pat(arg);
                }
            }
            Pat::Path(path) => {
                self.pat_adt(pat);
                let r = self.pat_path(pat, path, Site::PathPat);
                self.handle(r);
                StoreVisitorExt::on_path(self, path);
            }
            Pat::Expr(expr) => {
                self.m.in_pat = false;
                self.handle_assign(expr);
            }
            _ => store.visit_pat_children(pat, &mut *self),
        }
        self.m.in_pat = false;
    }

    fn pat_path(&mut self, pat: PatId, path: Path<'_>, site: Site) -> Resolved {
        if let Some(at) = self.arm_paths.iter().position(|&(it, _)| it == pat) {
            return self.arm_paths.swap_remove(at).1;
        }
        self.resolve(Some(ExprOrPatId::PatId(pat)), path, site, self.store.pat_path_hygiene(pat))
    }

    /// The ADT a pattern is of is live (rustc's `check_def_id(adt.did())` of a struct, tuple
    /// struct or path pattern).
    fn pat_adt(&mut self, pat: PatId) -> Option<DefIdx> {
        let ty = self.infer.type_of_expr_or_pat(ExprOrPatId::PatId(pat))?;
        let (adt, _) = ty.as_adt()?;
        let def = self.m.k.defs.get(Id::Adt(adt))?;
        self.m.check_def_id(def);
        Some(def)
    }

    /// A field of `variant` by the name it is written with, if it is the crate's.
    fn field(&self, variant: VariantId, name: &Name, hygiene: HygieneId) -> Option<DefIdx> {
        let db = self.m.k.cx.db;
        let local_id = variant.fields(db).field_written(db, variant, name, hygiene)?;
        self.m.k.defs.get(Id::Field(FieldId { parent: variant, local_id }))
    }

    fn ty(&mut self, ty: TypeRefId) {
        let store = self.store;
        match store.type_ref(ty) {
            TypeRef::Path(path) => {
                let r = self.resolve(None, path, Site::Type, store.type_path_hygiene(ty));
                self.handle(r);
            }
            TypeRef::TypeParam(param) => self.m.handle_res(Res::type_param(self.m.k.defs, param)),
            _ => {}
        }
        store.visit_type_ref_children(ty, &mut *self);
    }

    fn handle(&mut self, r: Resolved) {
        self.m.handle_res(r.res);
        self.m.handle_res(r.qualifier);
    }

    /// `path` at `at`, resolved in the scope the walk is at ([`BodyPaths::resolve`]), in the
    /// crate's numbering.
    fn resolve(
        &mut self,
        at: Option<ExprOrPatId>,
        path: Path<'_>,
        site: Site,
        hygiene: HygieneId,
    ) -> Resolved {
        let r = self.paths.resolve(self.scope, at, path, site, hygiene);
        let defs = self.m.k.defs;
        Resolved {
            res: res(defs, r.named),
            qualifier: res(defs, r.qualifier),
            resolved: r.resolved,
        }
    }

    /// rustc's `mark_as_used_if_union`: the fields a union expression names, of a union of
    /// more than one field, are read. A literal naming no field is no union's, and is not typed.
    fn union_fields(&mut self, expr: ExprId, fields: &[RecordLitField]) {
        if fields.is_empty() {
            return;
        }
        let Some(ty) = self.infer.type_of_expr_or_pat(ExprOrPatId::ExprId(expr)) else { return };
        let Some((AdtId::UnionId(union), _)) = ty.as_adt() else { return };
        let variant = VariantId::UnionId(union);
        if variant.fields(self.m.k.cx.db).fields().len() <= 1 {
            return;
        }
        for field in fields {
            let hygiene = self.store.record_field_hygiene(field.expr);
            if let Some(field) = self.field(variant, &field.name, hygiene) {
                self.m.insert_def_id(field);
            }
        }
    }

    /// The fields an `offset_of!` names are read. The front end resolves them only from the
    /// syntax.
    fn offset_of(&mut self, expr: ExprId) {
        let cx = self.m.k.cx;
        let Some(ptr) = self.node(expr) else { return };
        let Some(it) = ast::OffsetOfExpr::cast(cx.syntax_view(ptr).value) else { return };
        for name in it.fields() {
            if let Some((Either::Right(field), _)) = cx.vsema.resolve_offset_of_field(name)
                && let Some(field) = self.m.k.defs.get(Id::Field(FieldId::from(field)))
            {
                self.m.insert_def_id(field);
            }
        }
    }

    /// rustc's `handle_assign` of a place written: its fields are not read, unless it is
    /// reached through a dereference.
    fn handle_assign(&mut self, expr: ExprId) {
        if let Expr::Field { expr: base, .. } = self.store.expr(expr)
            && !self.derefs(expr)
        {
            return self.handle_assign(base);
        }
        self.on_expr(expr);
    }

    fn derefs(&self, expr: ExprId) -> bool {
        self.infer
            .expr_adjustment(expr)
            .is_some_and(|it| it.iter().any(|it| matches!(it.kind, Adjust::Deref(_))))
    }

    /// rustc's `check_for_self_assign`: `x = x` or `a.b = a.b`, not from a macro's expansion.
    /// The two sides' shapes and names are compared before anything is resolved.
    fn self_assign(&mut self, assign: ExprId, lhs: ExprId, rhs: ExprId) {
        if !self.same_place(lhs, rhs) {
            return;
        }
        let cx = self.m.k.cx;
        let Some(at) = self.node(assign) else { return };
        if crate::lints::syntax::from_expansion(cx, cx.syntax_view(at)) {
            return;
        }
        if !self.m.self_assigns.insert(at) {
            return;
        }
        let Some(ty) = self.infer.type_of_expr_or_pat(ExprOrPatId::ExprId(lhs)) else { return };
        let what = match self.store.expr(lhs) {
            Expr::Field { .. } => "field",
            _ => "variable",
        };
        // alloc: the edge, a lint that fires
        let message = blaze_report::say!(
            "useless assignment of ",
            blaze_report::say::Words(what),
            " of type ",
            blaze_report::say::Code(&self.print(ty)),
            " to itself"
        )
        .flat();
        let mut diag = Diag::lint("dead_code", message, cx.span_of(at));
        crate::lints::code_fact(cx, &mut diag, at);
        self.m.found.push(Found::lint(diag, Phase::Late, at));
    }

    /// `check_for_self_assign_helper`: the same local (a one-segment path on each side with the
    /// same name, resolved only then), or the same field of the same place.
    fn same_place(&mut self, lhs: ExprId, rhs: ExprId) -> bool {
        let store = self.store;
        match (store.expr(lhs), store.expr(rhs)) {
            (Expr::Path(l), Expr::Path(r)) => {
                let same_name = matches!((ident(l), ident(r)), (Some(a), Some(b)) if a == b);
                same_name
                    && matches!((self.local(lhs, l), self.local(rhs, r)), (Some(a), Some(b)) if a == b)
            }
            (Expr::Field { expr: l, name: a }, Expr::Field { expr: r, name: b }) => {
                a == b && self.same_place(l, r)
            }
            _ => false,
        }
    }

    fn local(&mut self, expr: ExprId, path: Path<'_>) -> Option<BindingId> {
        let db = self.paths.db;
        let hygiene = self.store.expr_path_hygiene(expr);
        let value = self
            .paths
            .in_scope(Some(expr), |r| r.resolve_path_in_value_ns_fully(db, path, hygiene));
        match value? {
            ValueNs::LocalBinding(it) => Some(it),
            _ => None,
        }
    }

    /// A type of the body as rustc prints it, through the type of its owner.
    fn print(&self, ty: Ty<'db>) -> String {
        let db = self.m.k.cx.db;
        let owner = match self.paths.owner {
            DefWithBodyId::FunctionId(it) => hir::Function::from(it).ret_type(db),
            DefWithBodyId::ConstId(it) => hir::Const::from(it).ty(db),
            DefWithBodyId::StaticId(it) => hir::Static::from(it).ty(db),
            DefWithBodyId::VariantId(it) => hir::EnumVariant::from(it).parent_enum(db).ty(db),
        };
        self.m.k.cx.ty(&owner.derived_from_next_solver(ty))
    }

    /// A closure (live unless an `allow` it has came with what reached it) or an anonymous
    /// constant (live), walked by `walk` out of any pattern it is in. Which are live matters
    /// only when a definition is in one.
    fn anon(&mut self, expr: ExprId, closure: bool, walk: impl FnOnce(&mut Self)) {
        let k = self.m.k;
        if k.defs.in_anon {
            // Its level is read at its node only when an `allow` there can make it its own.
            let own = closure
                && self.m.propagated
                && self.node(expr).is_some_and(|ptr| {
                    k.levels.at(k.cx, k.cx.syntax_view(ptr)).is_allow_or_expect()
                });
            if !own {
                self.m.live.anon.insert((self.paths.owner.into(), expr));
            }
        }
        let in_pat = mem::replace(&mut self.m.in_pat, false);
        walk(self);
        self.m.in_pat = in_pat;
    }

    /// The node `expr` is written as, if it is written (not made by lowering).
    fn node(&self, expr: ExprId) -> Option<InFile<SyntaxNodePtr>> {
        let source = self.source_map.expr_syntax(expr).ok()?;
        Some(source.map(|it| it.syntax_node_ptr()))
    }
}

impl StoreVisitor for Walk<'_, '_, '_, '_, '_> {
    fn on_expr(&mut self, expr: ExprId) {
        let scope = self.scope.replace(expr);
        stdx::ensure_sufficient_stack(|| self.expr(expr));
        self.scope = scope;
    }

    fn on_anon_const_expr(&mut self, expr: ExprId) {
        self.anon(expr, false, |this| this.on_expr(expr));
    }

    fn on_pat(&mut self, pat: PatId) {
        stdx::ensure_sufficient_stack(|| self.pat(pat));
    }

    fn on_type(&mut self, ty: TypeRefId) {
        self.ty(ty);
    }
}

/// A one-segment path's name: a local's, if it names one.
fn ident(path: Path<'_>) -> Option<&Name> {
    match path {
        Path::BarePath(it) => it.as_ident(),
        _ => None,
    }
}

/// What a resolved path names, in the crate's numbering: `Self` as a constructor is already
/// [`Named::SelfCtor`], so nothing here is mapped as one.
fn res(defs: &Defs, named: Named) -> Res {
    match named {
        Named::Value(it) => Res::value(defs, it, false),
        Named::Type(it) => Res::ty(defs, it, false),
        Named::Candidate(it) => Res::candidate(defs, it),
        Named::Variant(it) => Res::variant(defs, it),
        Named::SelfCtor => Res::SelfCtor,
        Named::None => Res::None,
    }
}

