//! The walk of one expression store as the front end lowered it: rustc's `intravisit` of a
//! body's or a signature's HIR with `MarkSymbolVisitor`'s methods, over the store's columns
//! rather than its syntax, written down as [`StoreMark`]s in the front end's ids.
//!
//! A store is a body (of a function, a constant, a static, a discriminant), a signature (its
//! generics, its parameters' and return types, its bounds, the constants in them), or the
//! fields of a struct, a union or a variant (their types). Each is walked once and kept by the
//! database ([`store_marks`]): a body edit walks that body again, and nothing reads its marks
//! again unless they changed. What a mark makes live is decided by the replay
//! ([`super::Mark::replay`]): the crate's numbering, the levels and what reached the
//! definition are its, not the record's, so the record holds no position and no number.
//!
//! A name is read from what inference recorded where it recorded one (a method call, a field,
//! an associated item named through a type, an enum variant), else resolved with the one
//! [`Resolver`] the walk keeps at the scope it is in, as inference resolves it: moved into each
//! block as the walk enters it, and into the scope of each path for its resolution
//! ([`uses::resolve_in`], the one resolution of every store's paths). A signature is not
//! inferred as a whole: each anonymous constant in it is, and its inference is read for what
//! is in it, found the first time the walk asks (most signatures ask none). No syntax is read.
//!
//! The store is what rustc lowers to, so the walk sees what rustc's does: macro calls expanded,
//! a destructuring assignment as its pattern (`E::A = x` assigns to a pattern and constructs
//! nothing), and the qualifier and generic arguments of every path, also a pattern's.
//!
//! Follows `compiler/rustc_passes/src/dead.rs` (`MarkSymbolVisitor`) and
//! `compiler/rustc_hir/src/intravisit.rs` (`walk_item`, `walk_generics`) (MIT or Apache-2.0).

use rustc_type_ir::inherent::IntoKind as _;
use std::mem;

use base_db::Db;
use either::Either;
use hir::Name;
use hir_def::{
    AdtId, DefWithBodyId, EnumVariantId, ExpressionStoreOwnerId, FieldId, GenericDefId,
    VariantId,
    expr_store::{Body, ExpressionStore, HygieneId, StoreVisitor, StoreVisitorExt, path::Path},
    hir::{
        AsmOperand, BindingId, ClosureKind, CoroutineSource, Expr, ExprId, ExprOrPatId, OffsetOf,
        Pat, PatId, RecordLitField,
        generics::{GenericParams, TypeOrConstParamData, WherePredicate},
        type_ref::{TypeBound, TypeRef, TypeRefId},
    },
    resolver::{HasResolver as _, Resolver, TypeNs, ValueNs},
    signatures::{
        ConstSignature, EnumSignature, EnumVariants, FunctionSignature, ImplSignature,
        StaticSignature, StructSignature, TraitSignature, TypeAliasSignature, UnionSignature,
        VariantFields,
    },
};
use hir_ty::{
    Adjust, InferenceResult,
    db::{AnonConstId, HirDatabase as _},
    next_solver::{DbInterner, TyKind},
};
use smallvec::SmallVec;

use crate::lints::names::uses::{self, Named, Resolved, Site, StorePaths};

/// One thing the walk of a store does to liveness, in the front end's ids: what the replay
/// ([`super::Mark::replay`]) does in the crate's numbering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum StoreMark {
    /// rustc's `handle_res` of what a path, a method call or a type names: in a pattern
    /// (`in_pat`, where only a constant or a type alias is used), and, of a variant, whether
    /// a match arm being walked needs it (`ignored`, not constructed by the arm).
    Named { named: Named, in_pat: bool, ignored: bool },
    /// The ADT a pattern, or the place a destructuring assignment writes, is of
    /// (`check_def_id`).
    Adt(AdtId),
    /// A field read (`insert_def_id`).
    Field(FieldId),
    /// A closure, a coroutine, a `const` block or an anonymous constant walked: live, unless
    /// it is a closure with an `allow` of its own that what reached it came with (decided by
    /// the replay, which reads the level).
    Anon { owner: ExpressionStoreOwnerId, expr: ExprId, closure: bool },
    /// rustc's `check_for_self_assign`: `assign` writes `lhs` to the same place it reads. The
    /// replay lints it (where it is written, its type as the crate prints it).
    SelfAssign { owner: ExpressionStoreOwnerId, assign: ExprId, lhs: ExprId },
    /// A bug planted for a test's oracle: the length of the store's text, which an edit that
    /// changes no mark still changes.
    #[cfg(test)]
    Planted(u32),
}

/// The marks of the store of `owner`, in the order the walk makes them: a body's parameters'
/// patterns then its expression; a signature's generics (each parameter's default and a const
/// parameter's type, then each where predicate's bounded type and bound) then the item's own
/// types (a function's parameters' and return types, a constant's or a static's type, an
/// impl's self type and trait, a type alias's bounds and type); a variant's fields' types and
/// default values. Kept by the database: the marks of a store stay equal (and nothing reads
/// them again) while what the store names does.
#[blaze_query::tracked(returns(deref))]
// `_: ()`: a store owner is an enum of ids, not an id; with a second argument the engine
// interns the pair as the key (as hir-ty's `closure_exprs`).
pub(crate) fn store_marks(db: &Db, owner: ExpressionStoreOwnerId, _: ()) -> Box<[StoreMark]> {
    #[cfg(test)]
    super::super::tests::executed(db, owner);
    let store = ExpressionStore::of(db, owner);
    let infer = match owner {
        ExpressionStoreOwnerId::Body(body) => Infer::Body(InferenceResult::of(db, body)),
        ExpressionStoreOwnerId::Signature(_) | ExpressionStoreOwnerId::VariantFields(_) => {
            Infer::Anons(None)
        }
    };
    let mut walk = Walk {
        db,
        owner,
        store,
        resolver: owner.resolver(db),
        infer,
        scope: None,
        in_pat: false,
        ignore_variants: SmallVec::new(),
        arm_paths: SmallVec::new(),
        out: Vec::new(), // alloc: the record, kept by the database, one entry a mark
    };
    match owner {
        ExpressionStoreOwnerId::Body(body) => walk.body(body),
        ExpressionStoreOwnerId::Signature(def) => walk.signature(def),
        ExpressionStoreOwnerId::VariantFields(variant) => walk.fields(variant),
    }
    #[cfg(test)]
    if let Some(len) = super::super::tests::planted_text_len(db, owner) {
        walk.out.push(StoreMark::Planted(len));
    }
    walk.out.into_boxed_slice()
}

/// What inference recorded for the expressions being walked.
enum Infer<'db> {
    /// A body's: one inference of the whole body.
    Body(&'db InferenceResult<'db>),
    /// A signature's or a variant's fields': each anonymous constant inferred on its own, by
    /// its root expression, found the first time the walk asks.
    Anons(Option<Box<[(ExprId, &'db InferenceResult<'db>)]>>),
}

/// The anonymous constants of the store of `owner` (a signature's or a variant's fields'),
/// each by its root expression with its inference.
fn anon_consts<'db>(
    db: &'db Db,
    owner: ExpressionStoreOwnerId,
) -> Box<[(ExprId, &'db InferenceResult<'db>)]> {
    let of = |anon: AnonConstId<'db>| {
        let loc = anon.loc(db);
        (loc.owner == owner).then(|| (loc.expr, InferenceResult::of(db, anon)))
    };
    match owner {
        ExpressionStoreOwnerId::Signature(def) => {
            // alloc: once per signature whose anonymous constants a walk asks about
            AnonConstId::all_from_signature(db, def).filter_map(of).collect()
        }
        ExpressionStoreOwnerId::VariantFields(variant) => {
            let anons = db.field_types_with_diagnostics(variant).defined_anon_consts();
            // alloc: once per variant whose anonymous constants a walk asks about
            anons.iter().copied().filter_map(of).collect()
        }
        ExpressionStoreOwnerId::Body(body) => {
            panic!("{body:?}: a body is inferred whole, not by its anonymous constants")
        }
    }
}

/// What inference recorded for `expr` of the store of `owner`: the body's, or that of the
/// anonymous constant of a signature (or of fields) it is in, if it is in one.
pub(super) fn inference_at(
    db: &Db,
    owner: ExpressionStoreOwnerId,
    expr: ExprId,
) -> Option<&InferenceResult<'_>> {
    match owner {
        ExpressionStoreOwnerId::Body(body) => Some(InferenceResult::of(db, body)),
        ExpressionStoreOwnerId::Signature(_) | ExpressionStoreOwnerId::VariantFields(_) => {
            let root = ExpressionStore::of(db, owner).find_root_for_expr(expr);
            anon_consts(db, owner).iter().find(|&&(it, _)| it == root).map(|&(_, it)| it)
        }
    }
}

pub(super) struct Walk<'s, 'db> {
    db: &'db Db,
    owner: ExpressionStoreOwnerId,
    store: &'s ExpressionStore,
    /// At the scope the walk is in: the store's own until a block moves it.
    resolver: Resolver<'db>,
    infer: Infer<'db>,
    /// The expression whose scope a path in the pattern or type being walked resolves in; none
    /// for the parameters and a signature's types, which see no scope of the store.
    scope: Option<ExprId>,
    /// rustc's `in_pat`.
    in_pat: bool,
    /// rustc's `ignore_variant_stack`: the variants the match arms being walked need.
    ignore_variants: SmallVec<[EnumVariantId; 4]>,
    /// What `necessary_variants` resolved of a match arm's patterns, taken by the walk of the
    /// arm.
    arm_paths: SmallVec<[(PatId, Resolved); 4]>,
    out: Vec<StoreMark>,
}

impl<'s, 'db> Walk<'s, 'db> {
    /// The parameters' patterns (also the one an `async fn` copies each into), then the body;
    /// a discriminant is an anonymous constant of its own.
    fn body(&mut self, owner: DefWithBodyId) {
        let body = Body::of(self.db, owner);
        for param in &body.params {
            self.on_pat(param.formal);
            if param.user_written != param.formal {
                self.on_pat(param.user_written);
            }
        }
        let root = body.root_expr();
        match owner {
            DefWithBodyId::VariantId(_) => self.anon(root, false, |this| this.on_expr(root)),
            _ => self.on_expr(root),
        }
    }

    /// rustc's `walk_item` of an item's signature, its body (when it has one) left to
    /// [`Walk::body`] and its fields to [`Walk::fields`].
    fn signature(&mut self, def: GenericDefId) {
        let db = self.db;
        match def {
            GenericDefId::FunctionId(it) => {
                let sig = FunctionSignature::of(db, it);
                self.generics(&sig.generic_params);
                for &param in &sig.params {
                    self.on_type(param);
                }
                self.on_type_opt(sig.ret_type);
            }
            GenericDefId::ConstId(it) => self.on_type(ConstSignature::of(db, it).type_ref),
            GenericDefId::StaticId(it) => self.on_type(StaticSignature::of(db, it).type_ref),
            GenericDefId::AdtId(AdtId::StructId(it)) => {
                self.generics(&StructSignature::of(db, it).generic_params)
            }
            GenericDefId::AdtId(AdtId::UnionId(it)) => {
                self.generics(&UnionSignature::of(db, it).generic_params)
            }
            GenericDefId::AdtId(AdtId::EnumId(it)) => {
                self.generics(&EnumSignature::of(db, it).generic_params)
            }
            GenericDefId::TraitId(it) => self.generics(&TraitSignature::of(db, it).generic_params),
            GenericDefId::TypeAliasId(it) => {
                let sig = TypeAliasSignature::of(db, it);
                self.generics(&sig.generic_params);
                self.bounds(&sig.bounds);
                self.on_type_opt(sig.ty);
            }
            GenericDefId::ImplId(it) => {
                let sig = ImplSignature::of(db, it);
                self.generics(&sig.generic_params);
                self.on_type(sig.self_ty);
                if let Some(trait_) = &sig.target_trait {
                    self.on_type(trait_.path.type_ref());
                }
            }
        }
    }

    /// rustc's `walk_generics`: each parameter's default and a const parameter's type, then
    /// each where predicate (a bound written on a parameter is one) with its bounded type.
    fn generics(&mut self, generics: &GenericParams) {
        for (_, param) in generics.iter_type_or_consts() {
            match param {
                TypeOrConstParamData::TypeParamData(it) => self.on_type_opt(it.default),
                TypeOrConstParamData::ConstParamData(it) => {
                    self.on_type(it.ty);
                    if let Some(default) = &it.default {
                        self.on_anon_const_expr(default.expr);
                    }
                }
            }
        }
        for predicate in generics.where_predicates() {
            match predicate {
                WherePredicate::TypeBound { target, bound, .. } => {
                    self.on_type(*target);
                    self.bounds(std::slice::from_ref(bound));
                }
                WherePredicate::Lifetime { .. } => {}
            }
        }
    }

    /// The traits `bounds` name, each a path walked as a type.
    fn bounds(&mut self, bounds: &[TypeBound]) {
        for bound in bounds {
            match *bound {
                TypeBound::Path(path, _) | TypeBound::ForLifetime(_, path) => {
                    self.on_type(path.type_ref())
                }
                TypeBound::Lifetime(_) | TypeBound::Use(_) | TypeBound::Error => {}
            }
        }
    }

    /// The fields of `variant`: each one's type, then its default value (an anonymous
    /// constant).
    fn fields(&mut self, variant: VariantId) {
        let fields = VariantFields::of(self.db, variant);
        for (_, field) in fields.fields().iter() {
            self.on_type(field.type_ref);
            if let Some(default) = &field.default_value {
                self.on_anon_const_expr(default.expr);
            }
        }
    }

    fn mark(&mut self, mark: StoreMark) {
        self.out.push(mark);
    }

    /// rustc's `handle_res` of `named`, as the walk is: in a pattern or not, inside the match
    /// arms that need which variants.
    fn named(&mut self, named: Named) {
        if named == Named::None {
            return;
        }
        let ignored = variant(named).is_some_and(|it| self.ignore_variants.contains(&it));
        self.mark(StoreMark::Named { named, in_pat: self.in_pat, ignored });
    }

    /// What inference recorded for `at`: the body's, or the anonymous constant's it is in.
    fn infer(&mut self, at: ExprOrPatId) -> Option<&'db InferenceResult<'db>> {
        let (db, owner, store) = (self.db, self.owner, self.store);
        match &mut self.infer {
            Infer::Body(it) => Some(*it),
            Infer::Anons(by_root) => {
                let root = match at {
                    ExprOrPatId::ExprId(it) => store.find_root_for_expr(it),
                    ExprOrPatId::PatId(it) => store.find_root_for_pat(it),
                };
                let by_root = by_root.get_or_insert_with(|| anon_consts(db, owner));
                by_root.iter().find(|&&(it, _)| it == root).map(|&(_, it)| it)
            }
        }
    }

    fn expr(&mut self, expr: ExprId) {
        let store = self.store;
        let at = ExprOrPatId::ExprId(expr);
        match store.expr(expr) {
            Expr::Path(path) => {
                let r = self.resolve(Some(at), path, Site::Value, store.expr_path_hygiene(expr));
                self.handle(r);
            }
            Expr::MethodCall { .. } => {
                if let Some((function, _)) =
                    self.infer(at).and_then(|it| it.method_resolution(expr))
                {
                    self.named(Named::Value(ValueNs::FunctionId(function)));
                }
            }
            Expr::Field { .. } => {
                if let Some(Either::Left(field)) =
                    self.infer(at).and_then(|it| it.field_resolution(expr))
                {
                    self.mark(StoreMark::Field(field));
                }
            }
            Expr::RecordLit { path, fields, .. } => {
                let r = self.resolve(Some(at), path, Site::RecordExpr, HygieneId::ROOT);
                self.handle(r);
                self.union_fields(expr, fields);
            }
            Expr::OffsetOf(offset) => self.offset_of(expr, offset),
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
                let guard = self.resolver.update_to_inner_scope(self.db, self.owner, expr);
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
        let len = self.ignore_variants.len();
        let mut stack = mem::take(&mut self.ignore_variants);
        self.necessary_variants(pat, &mut stack, len);
        self.ignore_variants = stack;
        self.on_pat(pat);
        if let Some(guard) = guard {
            self.on_expr(guard);
        }
        self.on_expr(expr);
        self.ignore_variants.truncate(len);
        self.arm_paths.clear();
    }

    /// rustc's `necessary_variants`: the variants a pattern's paths name (resolved, not
    /// through a type), outside `|` patterns, added to `out` past `from` once each.
    fn necessary_variants(
        &mut self,
        pat: PatId,
        out: &mut SmallVec<[EnumVariantId; 4]>,
        from: usize,
    ) {
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
                && let Some(variant) = variant(r.named)
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
        self.in_pat = true;
        match store.pat(pat) {
            Pat::Record { path, args, .. } => {
                self.pat_adt(pat);
                let r = self.pat_path(pat, path, Site::RecordPat);
                self.handle(r);
                StoreVisitorExt::on_path(self, path);
                let variant = self
                    .infer(ExprOrPatId::PatId(pat))
                    .and_then(|it| it.variant_resolution_for_pat(pat));
                if let Some(variant) = variant {
                    for arg in args {
                        if !matches!(store.pat(arg.pat), Pat::Wild)
                            && let Some(field) =
                                self.field(variant, &arg.name, store.record_field_hygiene(arg.pat))
                        {
                            self.mark(StoreMark::Field(field));
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
                if let Some(variant) = variant_of(r.named, adt) {
                    let wildcard = args.iter().map(|&it| matches!(store.pat(it), Pat::Wild));
                    self.tuple_fields(variant, wildcard, args.len(), ellipsis.map(|it| it as usize));
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
                self.in_pat = false;
                self.handle_assign(expr);
            }
            _ => store.visit_pat_children(pat, &mut *self),
        }
        self.in_pat = false;
    }

    fn pat_path(&mut self, pat: PatId, path: Path<'_>, site: Site) -> Resolved {
        if let Some(at) = self.arm_paths.iter().position(|&(it, _)| it == pat) {
            return self.arm_paths.swap_remove(at).1;
        }
        self.resolve(Some(ExprOrPatId::PatId(pat)), path, site, self.store.pat_path_hygiene(pat))
    }

    /// The ADT a pattern is of is live (rustc's `check_def_id(adt.did())` of a struct, tuple
    /// struct or path pattern).
    fn pat_adt(&mut self, pat: PatId) -> Option<AdtId> {
        let at = ExprOrPatId::PatId(pat);
        let ty = self.infer(at)?.type_of_expr_or_pat(at)?;
        let (adt, _) = ty.as_adt()?;
        self.mark(StoreMark::Adt(adt));
        Some(adt)
    }

    /// A field of `variant` by the name it is written with.
    fn field(&self, variant: VariantId, name: &Name, hygiene: HygieneId) -> Option<FieldId> {
        let db = self.db;
        let local_id = variant.fields(db).field_written(db, variant, name, hygiene)?;
        Some(FieldId { parent: variant, local_id })
    }

    /// The fields of `variant` that the subpatterns of a tuple pattern bind: whether each of its
    /// `bound` subpatterns other than `..` is a `_`, in order, and how many come before its `..`.
    fn tuple_fields(
        &mut self,
        variant: VariantId,
        wildcard: impl Iterator<Item = bool>,
        bound: usize,
        dotdot: Option<usize>,
    ) {
        let fields = VariantFields::of(self.db, variant).fields();
        let len = fields.len();
        let dotdot = dotdot.unwrap_or(bound);
        let missing = len.saturating_sub(bound);
        for (index, is_wildcard) in wildcard.enumerate() {
            if is_wildcard {
                continue;
            }
            let index = if index < dotdot { index } else { index + missing };
            if let Some((local_id, _)) = fields.iter().nth(index) {
                self.mark(StoreMark::Field(FieldId { parent: variant, local_id }));
            }
        }
    }

    fn ty(&mut self, ty: TypeRefId) {
        let store = self.store;
        match store.type_ref(ty) {
            TypeRef::Path(path) => {
                let r = self.resolve(None, path, Site::Type, store.type_path_hygiene(ty));
                self.handle(r);
            }
            TypeRef::TypeParam(param) => self.named(Named::Type(TypeNs::GenericParam(param))),
            _ => {}
        }
        store.visit_type_ref_children(ty, &mut *self);
    }

    fn handle(&mut self, r: Resolved) {
        self.named(r.named);
        self.named(r.qualifier);
    }

    /// `path` at `at`, resolved in the scope the walk is at ([`uses::resolve_in`]). In a
    /// signature, inference (of the anonymous constant the path is in) is read only for a path
    /// that may name something through a type: one of one segment resolves the same without.
    fn resolve(
        &mut self,
        at: Option<ExprOrPatId>,
        path: Path<'_>,
        site: Site,
        hygiene: HygieneId,
    ) -> Resolved {
        let through_type = matches!(path, Path::Normal(it) if it.type_anchor.is_some())
            || path.mod_path().is_some_and(|it| it.segments().len() > 1);
        let whole = matches!(self.infer, Infer::Body(_));
        let infer = match at {
            Some(at) if whole || through_type => self.infer(at),
            _ => None,
        };
        let mut paths =
            StorePaths { db: self.db, owner: self.owner, infer, resolver: &mut self.resolver };
        uses::resolve_in(&mut paths, self.scope, at, path, site, hygiene)
    }

    /// rustc's `mark_as_used_if_union`: the fields a union expression names, of a union of
    /// more than one field, are read. A literal naming no field is no union's, and is not typed.
    fn union_fields(&mut self, expr: ExprId, fields: &[RecordLitField]) {
        if fields.is_empty() {
            return;
        }
        let at = ExprOrPatId::ExprId(expr);
        let Some(ty) = self.infer(at).and_then(|it| it.type_of_expr_or_pat(at)) else { return };
        let Some((AdtId::UnionId(union), _)) = ty.as_adt() else { return };
        let variant = VariantId::UnionId(union);
        if variant.fields(self.db).fields().len() <= 1 {
            return;
        }
        for field in fields {
            let hygiene = self.store.record_field_hygiene(field.expr);
            if let Some(field) = self.field(variant, &field.name, hygiene) {
                self.mark(StoreMark::Field(field));
            }
        }
    }

    /// The fields an `offset_of!` names are read: each looked up in the type the one before
    /// it is of, from the container's type as inference recorded it, as inference checks
    /// them (`check_offset_of_privacy`); a variant names the fields after it.
    fn offset_of(&mut self, expr: ExprId, offset: &OffsetOf) {
        let db = self.db;
        let Some(infer) = self.infer(ExprOrPatId::ExprId(expr)) else { return };
        let Some(mut ty) = infer.type_of_type_placeholder(offset.container) else { return };
        let interner = DbInterner::new_no_crate(db);
        let mut variant = None;
        for (index, name) in offset.fields.iter().enumerate() {
            let (of, args) = match ty.kind() {
                TyKind::Tuple(tys) => {
                    match name.as_tuple_index().and_then(|it| tys.as_slice().get(it)) {
                        Some(&it) => ty = it,
                        None => return,
                    }
                    continue;
                }
                TyKind::Adt(adt, args) => match (adt.def_id(), variant.take()) {
                    (_, Some(variant)) => (VariantId::EnumVariantId(variant), args),
                    (AdtId::EnumId(it), None) => {
                        let ctxt = offset.field_hygiene(index).syntax_context();
                        let Some(it) = EnumVariants::variant_written(db, it, name, ctxt) else {
                            return;
                        };
                        variant = Some(it);
                        continue;
                    }
                    (AdtId::StructId(it), None) => (it.into(), args),
                    (AdtId::UnionId(it), None) => (it.into(), args),
                },
                _ => return,
            };
            let hygiene = offset.field_hygiene(index);
            let Some(local_id) = of.fields(db).field_written(db, of, name, hygiene) else {
                return;
            };
            self.mark(StoreMark::Field(FieldId { parent: of, local_id }));
            ty = db.field_types(of)[local_id].ty().instantiate(interner, args).skip_norm_wip();
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

    fn derefs(&mut self, expr: ExprId) -> bool {
        self.infer(ExprOrPatId::ExprId(expr))
            .and_then(|it| it.expr_adjustment(expr))
            .is_some_and(|it| it.iter().any(|it| matches!(it.kind, Adjust::Deref(_))))
    }

    /// rustc's `check_for_self_assign`: `x = x` or `a.b = a.b`. The two sides' shapes and
    /// names are compared before anything is resolved; where it is written (not in a macro's
    /// expansion) and its type are the replay's.
    fn self_assign(&mut self, assign: ExprId, lhs: ExprId, rhs: ExprId) {
        if self.same_place(lhs, rhs) {
            self.mark(StoreMark::SelfAssign { owner: self.owner, assign, lhs });
        }
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
        let db = self.db;
        let hygiene = self.store.expr_path_hygiene(expr);
        let value = uses::in_scope_of(db, &mut self.resolver, self.owner, Some(expr), |r| {
            r.resolve_path_in_value_ns_fully(db, path, hygiene)
        });
        match value? {
            ValueNs::LocalBinding(it) => Some(it),
            _ => None,
        }
    }

    /// A closure or an anonymous constant, walked by `walk` out of any pattern it is in.
    fn anon(&mut self, expr: ExprId, closure: bool, walk: impl FnOnce(&mut Self)) {
        self.mark(StoreMark::Anon { owner: self.owner, expr, closure });
        let in_pat = mem::replace(&mut self.in_pat, false);
        walk(self);
        self.in_pat = in_pat;
    }
}

impl StoreVisitor for Walk<'_, '_> {
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

/// The variant `named` names, if it names one (the replay maps it to `Res::Variant`).
pub(super) fn variant(named: Named) -> Option<EnumVariantId> {
    match named {
        Named::Variant(it)
        | Named::Value(ValueNs::EnumVariantId(it))
        | Named::Type(TypeNs::EnumVariantId(it)) => Some(it),
        _ => None,
    }
}

/// The variant a tuple struct pattern binds the fields of: rustc's `adt.variant_of_res(res)` of
/// the pattern's type `adt`, the variant its path names, else the struct (also for `Self`). A
/// path that resolves to nothing binds no field.
fn variant_of(named: Named, adt: Option<AdtId>) -> Option<VariantId> {
    if let Some(it) = variant(named) {
        return Some(VariantId::EnumVariantId(it));
    }
    match (named, adt) {
        (Named::None, _) => None,
        (_, Some(AdtId::StructId(it))) => Some(VariantId::StructId(it)),
        _ => None,
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
                        assert_eq!(
                            Some(value),
                            resolver.resolve_path_in_value_ns_fully(db, path, hygiene),
                            "{expr:?}"
                        );
                        values += 1;
                    }
                    if let Some((ty, segments)) = qualifier {
                        assert_eq!(
                            resolver.resolve_path_in_value_ns_fully(db, path, hygiene),
                            None,
                            "{expr:?}"
                        );
                        let found = resolver
                            .resolve_path_in_type_ns(db, path)
                            .map(|(it, remaining, _)| (it, remaining));
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
