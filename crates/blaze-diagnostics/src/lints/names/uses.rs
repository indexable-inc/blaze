//! What a body's paths name, read from HIR: the one home every names pass resolves a body's
//! paths through (unused imports, the stability checks, dead code), never the live tree.
//!
//! A path is resolved from what inference recorded for it (a method call, an associated item
//! named through a type, an enum variant, a value path resolved whole or type-relative), else
//! with one [`Resolver`] moved into the scope of the path, as `hir::view::Semantics` resolves
//! the node it was lowered from (inference first, then values first in an expression and types
//! first elsewhere), with no `SourceAnalyzer` built, no syntax node found and no path lowered
//! again. What it names is a hir-def id ([`Named`]); each pass maps it to its own numbering.
//!
//! [`BodyPaths::resolve`] is the resolution; a caller keeps one [`BodyPaths`] per body and moves
//! its resolver into each block as it enters it (`resolver.update_to_inner_scope`), so a path
//! whose scope is the block's pays no move.

use hir::Name;
use hir_def::{
    AdtId, AssocItemId, DefWithBodyId, EnumVariantId, ExpressionStoreOwnerId, HasModule as _,
    ImplId, Lookup as _, TypeAliasId, VariantId,
    expr_store::{HygieneId, path::Path},
    hir::{ExprId, ExprOrPatId},
    resolver::{HasResolver, Resolver, TypeNs, ValueNs},
};
use hir_ty::{
    InferenceResult,
    method_resolution::{CandidateId, InherentImpls},
    next_solver::SimplifiedType,
};

use base_db::Db;

/// Where a path is written, which decides how `hir::view::Semantics` resolves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Site {
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

/// What a path names, as hir-def ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Named {
    /// What the resolver finds in the value namespace (`Self` as a constructor is
    /// [`Named::SelfCtor`]).
    Value(ValueNs),
    /// What the resolver finds in the type namespace, or the associated type a type-relative
    /// type path names (`TypeNs::TypeAliasId`).
    Type(TypeNs),
    /// What inference resolved a type-relative path to (typeck's `type_dependent_def_id`).
    Candidate(CandidateId),
    /// A variant or its constructor, as inference recorded it.
    Variant(EnumVariantId),
    /// `Self` alone as a value or a pattern: rustc's `Res::SelfCtor`.
    SelfCtor,
    None,
}

/// A path resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Resolved {
    pub(crate) named: Named,
    /// What its qualifier names in the type namespace, as the walk over syntax resolves the
    /// qualifier's own node: the type its leading segments name, or the enum of a variant.
    pub(crate) qualifier: Named,
    /// rustc's resolver resolves it (`QPath::Resolved`), not typeck through a type
    /// (`QPath::TypeRelative`: `Self::A`, `Alias::A`, `<E>::A`).
    pub(crate) resolved: bool,
}

impl Resolved {
    pub(crate) const NONE: Resolved =
        Resolved { named: Named::None, qualifier: Named::None, resolved: false };
}

/// One body's paths, resolved: the body's inference and one resolver, moved by its holder into
/// each block it enters.
pub(crate) struct BodyPaths<'db> {
    pub(crate) db: &'db Db,
    pub(crate) owner: DefWithBodyId,
    pub(crate) infer: &'db InferenceResult<'db>,
    /// At the scope the holder's walk is in: the body's own scope until a block moves it.
    pub(crate) resolver: Resolver<'db>,
}

impl<'db> BodyPaths<'db> {
    pub(crate) fn new(db: &'db Db, owner: DefWithBodyId) -> Self {
        BodyPaths { db, owner, infer: InferenceResult::of(db, owner), resolver: owner.resolver(db) }
    }

    /// `path`, lowered at `at` (none for an `asm!` symbol or a type), written in the scope of
    /// the expression `scope` (none for a parameter, which sees no scope of the body), resolved
    /// as `hir::view::Semantics` resolves the node it was lowered from.
    pub(crate) fn resolve(
        &mut self,
        scope: Option<ExprId>,
        at: Option<ExprOrPatId>,
        path: Path<'_>,
        site: Site,
        hygiene: HygieneId,
    ) -> Resolved {
        let mut store = StorePaths {
            db: self.db,
            owner: self.owner.into(),
            infer: Some(self.infer),
            resolver: &mut self.resolver,
        };
        resolve_in(&mut store, scope, at, path, site, hygiene)
    }

    /// `f` with the resolver moved into the scope of `scope`, then back.
    pub(crate) fn in_scope<R>(
        &mut self,
        scope: Option<ExprId>,
        f: impl FnOnce(&Resolver<'db>) -> R,
    ) -> R {
        in_scope_of(self.db, &mut self.resolver, self.owner.into(), scope, f)
    }
}

/// The paths of one expression store (a body's, or a signature's: its types and the anonymous
/// constants in them), as [`resolve_in`] resolves them: the store's owner, what inference
/// recorded for the expressions being walked (none where nothing was inferred: a signature's
/// types), and one resolver at the scope the holder's walk is in.
pub(crate) struct StorePaths<'r, 'db> {
    pub(crate) db: &'db Db,
    pub(crate) owner: ExpressionStoreOwnerId,
    pub(crate) infer: Option<&'db InferenceResult<'db>>,
    pub(crate) resolver: &'r mut Resolver<'db>,
}

/// `path`, lowered at `at` (none for an `asm!` symbol or a type) in the store of `paths`,
/// written in the scope of the expression `scope` (none for what sees no scope of the store: a
/// parameter, a signature's type), resolved as `hir::view::Semantics` resolves the node it was
/// lowered from: what inference recorded first, if there is inference, then the resolver. The
/// one resolution of every store's paths ([`BodyPaths::resolve`] is it for a body).
pub(crate) fn resolve_in(
    paths: &mut StorePaths<'_, '_>,
    scope: Option<ExprId>,
    at: Option<ExprOrPatId>,
    path: Path<'_>,
    site: Site,
    hygiene: HygieneId,
) -> Resolved {
    let Some(mod_path) = path.mod_path() else { return Resolved::NONE };
    let db = paths.db;
    let infer = paths.infer;
    let segments = mod_path.segments();
    let qualified = segments.len() > 1;
    let anchored = matches!(path, Path::Normal(it) if it.type_anchor.is_some());
    let self_ctor = site.self_ctor() && mod_path.is_Self();
    let inferred = at.zip(infer).and_then(|(at, infer)| {
        if site.assoc()
            && let Some((candidate, _)) = infer.assoc_resolutions_for_expr_or_pat(at)
        {
            return Some(Named::Candidate(candidate));
        }
        match infer.variant_resolution_for_expr_or_pat(at)? {
            VariantId::EnumVariantId(it) if site != Site::Type => Some(Named::Variant(it)),
            _ => None,
        }
    });
    if let Some(named) = inferred
        && !qualified
    {
        return Resolved { named, qualifier: Named::None, resolved: !anchored };
    }
    let need = inferred.is_none();
    // What inference recorded is what the resolver finds at the path's scope
    // (`dead::mark::body::tests::recorded_value_paths_are_what_the_resolver_finds`): a value
    // path resolved whole, or the type its leading segments name when it is type-relative.
    let recorded = at.zip(infer).filter(|_| need && site == Site::Value);
    let recorded_value = recorded.and_then(|(at, infer)| infer.value_resolution(at));
    let recorded_qualifier = match recorded_value {
        None if qualified => recorded
            .and_then(|(at, infer)| infer.qualifier_resolution(at))
            .map(|(ty, segments)| (ty, Some(segments as usize))),
        _ => None,
    };
    let (value, ty) = match (recorded_value, recorded_qualifier) {
        (Some(value), _) if !qualified => (Some(value), None),
        (None, Some(ty)) => (None, Some(ty)),
        _ => in_scope_of(db, paths.resolver, paths.owner, scope, |r| {
            let mut value = None;
            let mut ty = None;
            if site == Site::Value {
                if need {
                    value = recorded_value
                        .or_else(|| r.resolve_path_in_value_ns_fully(db, path, hygiene));
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
    let named = match inferred {
        Some(named) => named,
        None => full(db, site, value, ty, segments, self_ctor),
    };
    let (qualifier, resolved) = match ty {
        _ if !qualified => (Named::None, true),
        Some((ty, Some(_))) => (
            Named::Type(ty),
            matches!(ty, TypeNs::ModuleId(_) | TypeNs::AdtId(AdtId::EnumId(_))),
        ),
        Some((TypeNs::EnumVariantId(it), None)) => {
            (Named::Type(TypeNs::AdtId(AdtId::EnumId(it.lookup(db).parent))), true)
        }
        _ => (Named::None, true),
    };
    Resolved { named, qualifier, resolved: resolved && !anchored }
}

/// `f` with `resolver` (of the store of `owner`) moved into the scope of `scope`, then back.
pub(crate) fn in_scope_of<'db, R>(
    db: &'db Db,
    resolver: &mut Resolver<'db>,
    owner: ExpressionStoreOwnerId,
    scope: Option<ExprId>,
    f: impl FnOnce(&Resolver<'db>) -> R,
) -> R {
    let Some(scope) = scope else { return f(resolver) };
    let guard = resolver.update_to_inner_scope(db, owner, scope);
    let r = f(resolver);
    resolver.reset_to_guard(guard);
    r
}

/// What a path names when inference recorded nothing for it.
fn full(
    db: &Db,
    site: Site,
    value: Option<ValueNs>,
    ty: Option<(TypeNs, Option<usize>)>,
    segments: &[Name],
    self_ctor: bool,
) -> Named {
    if site == Site::Value
        && let Some(value) = value
    {
        return named_value(value, self_ctor);
    }
    match ty {
        Some((ty, None)) => return named_type(ty, self_ctor),
        // A type path's value is never resolved whole when its type stops one segment short
        // (`resolve_path_in_value_ns_fully` resolves no associated item), so an associated type
        // found here is all the path names.
        Some((ty, Some(at))) if site == Site::Type && at + 1 == segments.len() => {
            if let Some(alias) = assoc_ty(db, ty, &segments[at]) {
                return Named::Type(TypeNs::TypeAliasId(alias));
            }
        }
        _ => {}
    }
    value.map_or(Named::None, |it| named_value(it, self_ctor))
}

fn named_value(value: ValueNs, self_ctor: bool) -> Named {
    match value {
        ValueNs::ImplSelf(_) if self_ctor => Named::SelfCtor,
        it => Named::Value(it),
    }
}

fn named_type(ty: TypeNs, self_ctor: bool) -> Named {
    match ty {
        TypeNs::SelfType(_) if self_ctor => Named::SelfCtor,
        it => Named::Type(it),
    }
}

/// The associated type `name` of a trait, an impl, or an inherent impl of an ADT, as `hir`
/// resolves a type-relative type path (`Trait::A`, `Self::A`, `S::A`).
pub(crate) fn assoc_ty(db: &Db, ty: TypeNs, name: &Name) -> Option<TypeAliasId> {
    match ty {
        TypeNs::TraitId(it) => it.trait_items(db).associated_type_by_name(name),
        TypeNs::SelfType(it) => impl_assoc_ty(db, it, name),
        TypeNs::AdtId(adt) | TypeNs::AdtSelfType(adt) => {
            let module = adt.module(db);
            let self_ty = SimplifiedType::Adt(adt.into());
            let mut found = None;
            InherentImpls::for_self_ty_in_crate_and_block(
                db,
                module.krate(db),
                module.block(db),
                &self_ty,
                &mut |impls| {
                    if found.is_none() {
                        found = impls.iter().find_map(|&it| impl_assoc_ty(db, it, name));
                    }
                },
            );
            found
        }
        _ => None,
    }
}

fn impl_assoc_ty(db: &Db, impl_: ImplId, name: &Name) -> Option<TypeAliasId> {
    impl_.impl_items(db).items.iter().find_map(|(it, item)| match item {
        AssocItemId::TypeAliasId(alias) if it == name => Some(*alias),
        _ => None,
    })
}

/// Every definition `owner`'s body names, through [`BodyPaths::resolve`]: what each path of its
/// expressions, patterns and types names and what its qualifier names, the type or module
/// each proper prefix of a path names (resolved in the type namespace at the path's scope, as
/// rustc resolves a parent segment, [`Named::None`] when it is not resolved whole), the method
/// each method call resolves to, and the struct, union or variant whose field a field access
/// names (a struct expression's or pattern's fields are named by its path). A path or a method
/// call that resolves to nothing is [`Named::None`] in the list, so a reader can tell it from
/// one naming nothing it looks at; a lang item's path (lowering's own, never written) is not in
/// it, nor is the associated type a binding names (`Iterator<Item = T>`: the trait is).
/// Deduplicated, in the order the walk of the body first comes to each.
pub(crate) fn named(db: &Db, owner: DefWithBodyId) -> Box<[Named]> {
    named_walk::named(db, owner)
}

mod named_walk {
    use std::alloc::Global;

    use either::Either;
    use hir_def::{
        AdtId, DefWithBodyId, VariantId,
        expr_store::{Body, ExpressionStore, HygieneId, StoreVisitor, path::Path},
        hir::{
            AsmOperand, Expr, ExprId, ExprOrPatId, Pat, PatId,
            type_ref::{TypeRef, TypeRefId},
        },
        resolver::{TypeNs, ValueNs},
    };
    use hir_expand::mod_path::ModPath;

    use base_db::Db;

    use super::{BodyPaths, Named, Site};

    pub(super) fn named(db: &Db, owner: DefWithBodyId) -> Box<[Named]> {
        let body = Body::of(db, owner);
        let mut walk = Walk {
            store: &body.store,
            paths: BodyPaths::new(db, owner),
            scope: None,
            seen: blaze_hash::HashSet::new_in(Global), // alloc: per body named, one entry a distinct def
            out: Vec::new(), // alloc: the result, one entry a distinct def
        };
        for param in &body.params {
            walk.on_pat(param.formal);
            if param.user_written != param.formal {
                walk.on_pat(param.user_written);
            }
        }
        walk.on_expr(body.root_expr());
        walk.out.into_boxed_slice()
    }

    struct Walk<'s, 'db> {
        store: &'s ExpressionStore,
        paths: BodyPaths<'db>,
        /// The expression whose scope a path in what is walked resolves in; none for the
        /// parameters, which see no scope of the body.
        scope: Option<ExprId>,
        seen: blaze_hash::HashSet<Named, Global>,
        out: Vec<Named>,
    }

    impl Walk<'_, '_> {
        fn push(&mut self, named: Named) {
            if self.seen.insert(named) {
                self.out.push(named);
            }
        }

        /// What `path`, lowered at `at`, names, what its qualifier names, and what each of its
        /// proper prefixes names in the type namespace.
        fn path(&mut self, at: Option<ExprOrPatId>, path: Path<'_>, site: Site, hygiene: HygieneId) {
            let Some(mod_path) = path.mod_path() else { return };
            let r = self.paths.resolve(self.scope, at, path, site, hygiene);
            self.push(r.named);
            if r.qualifier != Named::None {
                self.push(r.qualifier);
            }
            let segments = mod_path.segments();
            let db = self.paths.db;
            for len in 1..segments.len() {
                // clone: the leading segments' names (interned symbols) for the prefix's own path
                let prefix = ModPath::from_segments(mod_path.kind, segments[..len].iter().cloned());
                let found = self.paths.in_scope(self.scope, |r| {
                    r.resolve_path_in_type_ns_hygienic(db, Path::BarePath(&prefix), hygiene)
                });
                self.push(match found {
                    Some((ty, None, ..)) => Named::Type(ty),
                    _ => Named::None,
                });
            }
        }

        fn expr(&mut self, expr: ExprId) {
            let store = self.store;
            let infer = self.paths.infer;
            match store.expr(expr) {
                Expr::Path(path) => self.path(
                    Some(ExprOrPatId::ExprId(expr)),
                    path,
                    Site::Value,
                    store.expr_path_hygiene(expr),
                ),
                Expr::MethodCall { .. } => match infer.method_resolution(expr) {
                    Some((function, _)) => self.push(Named::Value(ValueNs::FunctionId(function))),
                    None => self.push(Named::None),
                },
                Expr::Field { .. } => match infer.field_resolution(expr) {
                    Some(Either::Left(field)) => self.push(variant_named(field.parent)),
                    Some(Either::Right(_)) => {}
                    None => self.push(Named::None),
                },
                Expr::RecordLit { path, .. } => self.path(
                    Some(ExprOrPatId::ExprId(expr)),
                    path,
                    Site::RecordExpr,
                    HygieneId::ROOT,
                ),
                Expr::InlineAsm(asm) => {
                    for (_, operand) in asm.operands.iter() {
                        if let AsmOperand::Sym(path) = operand {
                            self.path(None, store.path_at(*path), Site::Value, HygieneId::ROOT);
                        }
                    }
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

        fn pat(&mut self, pat: PatId) {
            let store = self.store;
            let (path, site) = match store.pat(pat) {
                Pat::Record { path, .. } => (path, Site::RecordPat),
                Pat::TupleStruct { path, .. } => (path, Site::TupleStructPat),
                Pat::Path(path) => (path, Site::PathPat),
                _ => {
                    store.visit_pat_children(pat, &mut *self);
                    return;
                }
            };
            self.path(Some(ExprOrPatId::PatId(pat)), path, site, store.pat_path_hygiene(pat));
            store.visit_pat_children(pat, &mut *self);
        }

        fn ty(&mut self, ty: TypeRefId) {
            let store = self.store;
            if let TypeRef::Path(path) = store.type_ref(ty) {
                self.path(None, path, Site::Type, store.type_path_hygiene(ty));
            }
            store.visit_type_ref_children(ty, &mut *self);
        }
    }

    impl StoreVisitor for Walk<'_, '_> {
        fn on_expr(&mut self, expr: ExprId) {
            let scope = self.scope.replace(expr);
            stdx::ensure_sufficient_stack(|| self.expr(expr));
            self.scope = scope;
        }

        fn on_pat(&mut self, pat: PatId) {
            stdx::ensure_sufficient_stack(|| self.pat(pat));
        }

        fn on_type(&mut self, ty: TypeRefId) {
            self.ty(ty);
        }
    }

    /// What a field's struct, union or variant is named as.
    fn variant_named(variant: VariantId) -> Named {
        match variant {
            VariantId::StructId(it) => Named::Type(TypeNs::AdtId(AdtId::StructId(it))),
            VariantId::UnionId(it) => Named::Type(TypeNs::AdtId(AdtId::UnionId(it))),
            VariantId::EnumVariantId(it) => Named::Variant(it),
        }
    }
}
