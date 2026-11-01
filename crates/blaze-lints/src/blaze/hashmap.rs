//! `std_hashmap`: std's `HashMap` or `HashSet` hashing with `RandomState` (SipHash, seeded per
//! map).
//!
//! Two forms. A written type: a path type naming std's `HashMap` or `HashSet` whose hasher
//! argument is left to its default or names `RandomState` (in a signature, a field, a `let`'s
//! type, a turbofish). An expression that makes one: a call whose value is such a map and whose
//! callee is the map's own constructor or a trait's method (`HashMap::new()`,
//! `HashSet::with_capacity(n)`, `Default::default()`, `.collect()`), with no turbofish on the
//! call, as that is a written type of its own. A map hashing with anything else (`HashMap`,
//! a hasher parameter `S`) is not flagged.

use hir_def::{AdtId, FunctionId, ItemContainerId};
use hir_ty::next_solver::Ty;
use syntax::view as ast;

use ast::HasGenericArgs as _;

use super::Known;
use crate::check::Cx;

/// std's map types and their default hasher.
#[derive(Clone, Copy)]
pub(super) struct Std {
    map: Option<AdtId>,
    set: Option<AdtId>,
    random: Option<AdtId>,
}

impl Std {
    pub(super) fn new(cx: &Cx<'_, '_>) -> Std {
        let adt = |path: &[&str]| match cx.std_def(path) {
            Some(hir::ModuleDef::Adt(it)) => Some(AdtId::from(it)),
            _ => None,
        };
        Std {
            map: adt(&["std", "collections", "HashMap"]),
            set: adt(&["std", "collections", "HashSet"]),
            random: adt(&["std", "hash", "RandomState"]),
        }
    }

    /// Which argument of `adt` is its hasher, if it is std's map or set.
    fn hasher_at(self, adt: AdtId) -> Option<usize> {
        match Some(adt) {
            it if it == self.map => Some(2),
            it if it == self.set => Some(1),
            _ => None,
        }
    }

    fn is_random(self, adt: AdtId) -> bool {
        self.random == Some(adt)
    }

    /// Whether `adt` is std's map or set, with any hasher.
    pub(super) fn is_map_or_set(self, adt: AdtId) -> bool {
        self.hasher_at(adt).is_some()
    }

    /// Whether `ty` is std's map or set hashing with `RandomState`.
    pub(super) fn is_random_map(self, ty: Ty<'_>) -> bool {
        let Some((adt, args)) = ty.as_adt() else { return false };
        let Some(at) = self.hasher_at(adt) else { return false };
        args.types()
            .nth(at)
            .and_then(|it| it.as_adt())
            .is_some_and(|(hasher, _)| self.is_random(hasher))
    }
}

/// The finding of the written type `ty`, if it names std's map or set with `RandomState`.
pub(super) fn check_type(cx: &Cx<'_, '_>, ty: ast::PathType<'_>) -> Option<String> {
    let path = ty.path()?;
    let segment = path.segment()?;
    let name = segment.name_ref()?.text();
    if !matches!(name, "HashMap" | "HashSet") {
        return None;
    }
    let std = Std::new(cx);
    let adt = resolved_adt(cx, path)?;
    let at = std.hasher_at(adt)?;
    let hasher = segment.generic_arg_list().and_then(|list| {
        list.generic_args()
            .filter_map(|it| match it {
                ast::GenericArg::TypeArg(it) => it.ty(),
                _ => None,
            })
            .nth(at)
    });
    let random = match hasher {
        None => true,
        Some(ast::Type::PathType(it)) => {
            it.path().and_then(|it| resolved_adt(cx, it)).is_some_and(|it| std.is_random(it))
        }
        Some(_) => false,
    };
    random.then(|| format!("std's `{name}` hashes with SipHash (`RandomState`)"))
}

fn resolved_adt(cx: &Cx<'_, '_>, path: ast::Path<'_>) -> Option<AdtId> {
    match cx.path(path)? {
        hir::PathResolution::Def(hir::ModuleDef::Adt(it)) => Some(AdtId::from(it)),
        _ => None,
    }
}

/// The finding of a call of `callee` whose value has type `ty`, if it makes std's map or set
/// with `RandomState`; `turbofish` whether the call writes generic arguments.
pub(super) fn check_construct(
    k: &Known<'_>,
    callee: FunctionId,
    ty: Ty<'_>,
    turbofish: bool,
) -> Option<String> {
    if turbofish || !k.std.is_random_map(ty) {
        return None;
    }
    let (adt, _) = ty.as_adt()?;
    let name = match Some(adt) == k.std.set {
        true => "HashSet",
        false => "HashMap",
    };
    constructs(k, callee, adt)
        .then(|| format!("this makes a std `{name}`, which hashes with SipHash (`RandomState`)"))
}

/// Whether a call of `callee` whose value is an `adt` makes it: `callee` is a trait's method
/// other than `clone` (`Default::default()`, `.collect()`) or a method of `adt`'s own impl
/// (`HashMap::new()`), not a function that hands a value on (`mem::take`).
pub(super) fn constructs(k: &Known<'_>, callee: FunctionId, adt: AdtId) -> bool {
    match callee.loc(k.db).container {
        ItemContainerId::TraitId(it) => k.clone.is_none_or(|(clone, _)| clone != it),
        ItemContainerId::ImplId(it) => {
            let self_ty = hir::Impl::from(it).self_ty(k.db);
            self_ty.as_adt().is_some_and(|it| AdtId::from(it) == adt)
        }
        _ => false,
    }
}
