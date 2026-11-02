//! The walk of one body as the front end lowered it: rustc's `intravisit` of a body's HIR with
//! `MarkSymbolVisitor`'s methods, over the expression store's columns rather than its syntax.
//!
//! A name is read from what inference recorded where it recorded one (a method call, a field,
//! an associated item named through a type, an enum variant), else resolved with the one
//! [`Resolver`] the walk keeps at the scope it is in, as inference resolves it: moved into each
//! block as the walk enters it, and into the scope of each path for its resolution. Each path
//! resolves as `hir::view::Semantics` resolves the node it was lowered from (inference first, then
//! values first in an expression and types first elsewhere), with no `SourceAnalyzer` built,
//! no syntax node found and no path lowered again for it. Syntax is read only for a
//! self-assignment about to be linted, an `offset_of!`, and a closure or an anonymous constant
//! when a definition is in one.
//!
//! The store is what rustc lowers to, so the walk sees what rustc's does: macro calls expanded,
//! a destructuring assignment as its pattern (`E::A = x` assigns to a pattern and constructs
//! nothing), and the qualifier and generic arguments of every path, also a pattern's.
//!
//! Follows `compiler/rustc_passes/src/dead.rs` (`MarkSymbolVisitor`) (MIT or Apache-2.0).

use std::mem;

use either::Either;
use hir::{InFile, Name};
use hir_def::{
    AdtId, DefWithBodyId, FieldId, VariantId,
    expr_store::{Body, BodySourceMap, ExpressionStore, HygieneId, StoreVisitor, StoreVisitorExt, path::Path},
    hir::{
        AsmOperand, BindingId, ClosureKind, CoroutineSource, Expr, ExprId, ExprOrPatId, Pat, PatId, RecordLitField,
        type_ref::{TypeRef, TypeRefId},
    },
    resolver::{HasResolver, Resolver, TypeNs, ValueNs},
};
use hir_ty::{Adjust, InferenceResult, next_solver::Ty};
use smallvec::SmallVec;
use syntax::{
    SyntaxNodePtr,
    view::{self as ast, AstNode as _},
};

use super::{
    super::{
        defs::{DefIdx, Id, Kind, Parent},
        res::Res,
    },
    Mark,
};
use crate::{
    lower::{Found, Phase},
    model::Diag,
};

/// Where a path is, which decides how `hir::view::Semantics` resolves it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Site {
    Value,
    PathPat,
    TupleStructPat,
    RecordPat,
    RecordExpr,
    Type,
}

impl Site {
    /// Inference's resolution of an associated item named through a type is read first.
    fn assoc(self) -> bool {
        matches!(self, Site::Value | Site::PathPat)
    }

    /// `Self` alone here is the constructor (rustc's `Res::SelfCtor`).
    fn self_ctor(self) -> bool {
        matches!(self, Site::Value | Site::PathPat | Site::TupleStructPat)
    }
}

/// A path resolved.
#[derive(Clone, Copy)]
struct Resolved {
    res: Res,
    /// What its qualifier names, as the walk over syntax resolves the qualifier's own node.
    qualifier: Res,
    /// rustc's resolver resolves it (`QPath::Resolved`), not typeck through a type
    /// (`QPath::TypeRelative`: `Self::A`, `Alias::A`, `<E>::A`).
    resolved: bool,
}

impl Resolved {
    const NONE: Resolved = Resolved { res: Res::None, qualifier: Res::None, resolved: false };
}

pub(super) struct Walk<'w, 'k, 'c, 'a, 'db> {
    m: &'w mut Mark<'k, 'c, 'a, 'db>,
    owner: DefWithBodyId,
    store: &'w ExpressionStore,
    source_map: &'w BodySourceMap,
    infer: &'w InferenceResult<'db>,
    resolver: Resolver<'db>,
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
        let db = m.k.cx.db;
        Walk {
            owner,
            store: &body.store,
            source_map,
            infer: InferenceResult::of(db, owner),
            resolver: owner.resolver(db),
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
                let r = self.resolve(Some(ExprOrPatId::ExprId(expr)), path, Site::Value, store.expr_path_hygiene(expr));
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
                let r = self.resolve(Some(ExprOrPatId::ExprId(expr)), path, Site::RecordExpr, HygieneId::ROOT);
                self.handle(r);
                self.union_fields(expr, fields);
            }
            Expr::OffsetOf(_) => self.offset_of(expr),
            Expr::InlineAsm(asm) => {
                for (_, operand) in asm.operands.iter() {
                    if let AsmOperand::Sym(path) = operand {
                        let r = self.resolve(None, store.path_at(*path), Site::Value, HygieneId::ROOT);
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
                if !matches!(closure_kind, ClosureKind::Coroutine { source: CoroutineSource::Fn, .. }) {
                    self.anon(expr, true, |this| store.visit_expr_children(expr, &mut *this));
                    return;
                }
            }
            Expr::Const(inner) => {
                self.anon(expr, false, |this| this.on_expr(inner));
                return;
            }
            Expr::Block { .. } => {
                let guard = self.resolver.update_to_inner_scope(self.m.k.cx.db, self.owner, expr);
                store.visit_expr_children(expr, &mut *self);
                self.resolver.reset_to_guard(guard);
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
            let r = self.resolve(Some(ExprOrPatId::PatId(pat)), path, site, store.pat_path_hygiene(pat));
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
                            && let Some(field) = self.field(variant, &arg.name)
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
                    self.m.tuple_fields(variant, wildcard, args.len(), ellipsis.map(|it| it as usize));
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

    /// A field of `variant` by name, if it is the crate's.
    fn field(&self, variant: VariantId, name: &Name) -> Option<DefIdx> {
        let local_id = variant.fields(self.m.k.cx.db).field(name)?;
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

    /// `path` at `at`, resolved as `hir::view::Semantics` resolves the node it was lowered from.
    fn resolve(&mut self, at: Option<ExprOrPatId>, path: Path<'_>, site: Site, hygiene: HygieneId) -> Resolved {
        let Some(mod_path) = path.mod_path() else { return Resolved::NONE };
        let defs = self.m.k.defs;
        let db = self.m.k.cx.db;
        let segments = mod_path.segments();
        let qualified = segments.len() > 1;
        let anchored = matches!(path, Path::Normal(it) if it.type_anchor.is_some());
        let self_ctor = site.self_ctor() && mod_path.is_Self();
        let inferred = at.and_then(|at| {
            if site.assoc()
                && let Some((candidate, _)) = self.infer.assoc_resolutions_for_expr_or_pat(at)
            {
                return Some(Res::candidate(defs, candidate));
            }
            match self.infer.variant_resolution_for_expr_or_pat(at)? {
                VariantId::EnumVariantId(it) if site != Site::Type => Some(Res::variant(defs, it)),
                _ => None,
            }
        });
        if let Some(res) = inferred
            && !qualified
        {
            return Resolved { res, qualifier: Res::None, resolved: !anchored };
        }
        let need = inferred.is_none();
        // What inference recorded is what the resolver finds at the path's scope
        // (`tests::recorded_value_paths_are_what_the_resolver_finds`): a value path resolved
        // whole, or the type its leading segments name when it is type-relative.
        let recorded = at.filter(|_| need && site == Site::Value);
        let recorded_value = recorded.and_then(|at| self.infer.value_resolution(at));
        let recorded_qualifier = match recorded_value {
            None if qualified => recorded
                .and_then(|at| self.infer.qualifier_resolution(at))
                .map(|(ty, segments)| (ty, Some(segments as usize))),
            _ => None,
        };
        let (value, ty) = match (recorded_value, recorded_qualifier) {
            (Some(value), _) if !qualified => (Some(value), None),
            (None, Some(ty)) => (None, Some(ty)),
            _ => self.in_scope(|r| {
                let mut value = None;
                let mut ty = None;
                if site == Site::Value {
                    if need {
                        value = recorded_value.or_else(|| r.resolve_path_in_value_ns_fully(db, path, hygiene));
                    }
                    if qualified || (need && value.is_none()) {
                        ty = r.resolve_path_in_type_ns(db, path).map(|(it, remaining, _)| (it, remaining));
                    }
                } else {
                    ty = r.resolve_path_in_type_ns(db, path).map(|(it, remaining, _)| (it, remaining));
                    if need && !matches!(ty, Some((_, None))) {
                        value = r.resolve_path_in_value_ns_fully(db, path, hygiene);
                    }
                }
                (value, ty)
            }),
        };
        let res = match inferred {
            Some(res) => res,
            None => self.full(site, value, ty, segments, self_ctor),
        };
        let (qualifier, resolved) = match ty {
            _ if !qualified => (Res::None, true),
            Some((ty, Some(_))) => {
                (Res::ty(defs, ty, false), matches!(ty, TypeNs::ModuleId(_) | TypeNs::AdtId(AdtId::EnumId(_))))
            }
            Some((TypeNs::EnumVariantId(it), None)) => (self.enum_of(it), true),
            _ => (Res::None, true),
        };
        Resolved { res, qualifier, resolved: resolved && !anchored }
    }

    /// What a path names when inference recorded nothing for it.
    fn full(
        &self,
        site: Site,
        value: Option<ValueNs>,
        ty: Option<(TypeNs, Option<usize>)>,
        segments: &[Name],
        self_ctor: bool,
    ) -> Res {
        let defs = self.m.k.defs;
        if site == Site::Value
            && let Some(value) = value
        {
            return Res::value(defs, value, self_ctor);
        }
        match ty {
            Some((ty, None)) => return Res::ty(defs, ty, self_ctor),
            Some((ty, Some(at))) if site == Site::Type && at + 1 == segments.len() => {
                let assoc = self.assoc_ty(ty, &segments[at]);
                if assoc != Res::None {
                    return assoc;
                }
            }
            _ => {}
        }
        value.map_or(Res::None, |it| Res::value(defs, it, self_ctor))
    }

    /// The associated type `name` of a trait, an impl, or an inherent impl of an ADT, as `hir`
    /// resolves a type-relative type path (`Trait::A`, `Self::A`, `S::A`).
    fn assoc_ty(&self, ty: TypeNs, name: &Name) -> Res {
        let defs = self.m.k.defs;
        let named = |owner: DefIdx| {
            defs.children(owner)
                .iter()
                .copied()
                .find(|&it| defs[it].kind == Kind::AssocTy && defs[it].name.as_ref() == Some(name))
        };
        let found = match ty {
            TypeNs::TraitId(it) => defs.get(Id::Trait(it)).and_then(named),
            TypeNs::SelfType(it) => defs.get(Id::Impl(it)).and_then(named),
            TypeNs::AdtId(it) | TypeNs::AdtSelfType(it) => defs.get(Id::Adt(it)).and_then(|adt| {
                defs.impls()
                    .iter()
                    .filter(|it| it.self_adt == Some(adt) && defs[it.def].kind == (Kind::Impl { of_trait: false }))
                    .find_map(|it| named(it.def))
            }),
            _ => None,
        };
        found.map_or(Res::None, |it| Res::item(defs, it))
    }

    /// The enum a variant is of, when it is the crate's.
    fn enum_of(&self, variant: hir_def::EnumVariantId) -> Res {
        let defs = self.m.k.defs;
        match defs.get(Id::Variant(variant)).map(|it| defs[it].parent) {
            Some(Parent::Def(it)) => Res::Def(it),
            _ => Res::None,
        }
    }

    /// `f` with the resolver moved into the scope the walk is at.
    fn in_scope<R>(&mut self, f: impl FnOnce(&Resolver<'db>) -> R) -> R {
        let Some(scope) = self.scope else { return f(&self.resolver) };
        let guard = self.resolver.update_to_inner_scope(self.m.k.cx.db, self.owner, scope);
        let r = f(&self.resolver);
        self.resolver.reset_to_guard(guard);
        r
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
            if let Some(field) = self.field(variant, &field.name) {
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
        self.infer.expr_adjustment(expr).is_some_and(|it| it.iter().any(|it| matches!(it.kind, Adjust::Deref(_))))
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
        let message = format!("useless assignment of {what} of type `{}` to itself", self.print(ty));
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
                same_name && matches!((self.local(lhs, l), self.local(rhs, r)), (Some(a), Some(b)) if a == b)
            }
            (Expr::Field { expr: l, name: a }, Expr::Field { expr: r, name: b }) => a == b && self.same_place(l, r),
            _ => false,
        }
    }

    fn local(&mut self, expr: ExprId, path: Path<'_>) -> Option<BindingId> {
        let db = self.m.k.cx.db;
        let hygiene = self.store.expr_path_hygiene(expr);
        let scope = self.scope.replace(expr);
        let value = self.in_scope(|r| r.resolve_path_in_value_ns_fully(db, path, hygiene));
        self.scope = scope;
        match value? {
            ValueNs::LocalBinding(it) => Some(it),
            _ => None,
        }
    }

    /// A type of the body as rustc prints it, through the type of its owner.
    fn print(&self, ty: Ty<'db>) -> String {
        let db = self.m.k.cx.db;
        let owner = match self.owner {
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
        if k.defs.in_anon
            && let Some(ptr) = self.node(expr)
        {
            let own = closure && self.m.propagated && k.levels.at(k.cx, k.cx.syntax_view(ptr)).is_allow_or_expect();
            if !own {
                self.m.live.anon.insert(ptr);
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

#[cfg(test)]
mod tests {
    use hir_def::{
        expr_store::{ExpressionStore, scope::ExprScopes},
        hir::{Expr, ExprOrPatId},
        nameres::demand::crate_body_owners,
        resolver::resolver_for_scope,
    };
    use hir_ty::{InferBodyId, InferenceResult};

    use crate::lower::break_outside_of_loop::tests::with_file;

    /// Value paths of each kind: locals, a closure's parameter and capture, a const parameter,
    /// functions, constants, statics and constructors through a module, and type-relative paths
    /// through a struct, a type parameter and an alias.
    const PATHS: &str = r#"
mod m { pub fn f() {} pub const C: u8 = 0; pub static S: u8 = 0; }
pub struct Unit;
pub struct Tuple(pub u8);
pub struct S;
impl S { pub fn new() -> S { S } pub const K: u8 = 1; }
pub trait Tr { fn t() -> u8; }
impl Tr for S { fn t() -> u8 { 0 } }
pub enum E { A, B(u8) }
type Alias = E;
fn f() {}
pub fn g<T: Tr, const N: usize>(x: u8) -> u8 {
    let y = x;
    let _ = m::f;
    let _ = m::C + m::S;
    let _ = Unit;
    let _ = Tuple;
    let _ = S::new;
    let _ = S::K;
    let _ = T::t;
    let _ = Alias::A;
    let _ = E::B;
    let _ = N;
    let f = 1;
    let _ = f;
    let c = |z: u8| z + y;
    c(y)
}
"#;

    /// What inference records of a value path, resolved whole or type-relative, is what the
    /// resolver finds at the path's own scope: the condition for `Walk::resolve` to read it.
    #[test]
    fn recorded_value_paths_are_what_the_resolver_finds() {
        with_file(PATHS, |cx, _| {
            let db = cx.db;
            let (mut values, mut qualifiers) = (0, 0);
            for owner in crate_body_owners(db, cx.krate.base()) {
                let infer = InferenceResult::of(db, InferBodyId::DefWithBodyId(owner));
                let store = ExpressionStore::of(db, owner.into());
                let scopes = ExprScopes::of(db, owner);
                for (expr, it) in store.exprs() {
                    let Expr::Path(path) = it else { continue };
                    let resolver = resolver_for_scope(db, owner, scopes.scope_for(expr));
                    let hygiene = store.expr_path_hygiene(expr);
                    let at = ExprOrPatId::ExprId(expr);
                    let qualifier = infer.qualifier_resolution(at);
                    // A type-relative path has its value recorded too, which no resolver finds.
                    if let Some(value) = infer.value_resolution(at)
                        && qualifier.is_none()
                    {
                        assert_eq!(Some(value), resolver.resolve_path_in_value_ns_fully(db, path, hygiene), "{expr:?}");
                        values += 1;
                    }
                    if let Some((ty, segments)) = qualifier {
                        assert_eq!(resolver.resolve_path_in_value_ns_fully(db, path, hygiene), None, "{expr:?}");
                        let found = resolver.resolve_path_in_type_ns(db, path).map(|(it, remaining, _)| (it, remaining));
                        assert_eq!(found, Some((ty, Some(segments as usize))), "{expr:?}");
                        qualifiers += 1;
                    }
                }
            }
            assert!(values >= 10, "{values} value paths recorded");
            assert!(qualifiers >= 3, "{qualifiers} type-relative paths recorded");
        });
    }
}
