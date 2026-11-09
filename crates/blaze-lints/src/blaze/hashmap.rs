//! `default_hasher` (was `std_hashmap`): a hash collection whose hasher nobody chose, or chose
//! to seed at random (AGENTS.md rules 12 and 25: the hasher is an explicit choice, and output
//! never depends on a per-process seed).
//!
//! A collection's hasher is the type parameter of its ADT that has a default implementing
//! `BuildHasher`, or that is bounded by `BuildHasher` where the ADT declares it
//! ([`hasher_params`]): std's `HashMap` and `HashSet`, hashbrown's, indexmap's, dashmap's,
//! papaya's and any other ADT made that way, found by its declaration, never by its name. Two
//! forms:
//!
//! - A written type (a signature, a field, a `let`'s type, an alias, a turbofish): a path type
//!   naming such an ADT whose hasher argument is left out, so it is the default, or is a
//!   randomly seeded hasher ([`RANDOM`]: std's `RandomState`, foldhash's `fast` and `quality`
//!   `RandomState`, ahash's, rustc-hash's `FxRandomState`; hashbrown's `DefaultHashBuilder` is
//!   foldhash's), resolved through aliases.
//! - An expression that makes one: a call whose value is such a collection hashing with a
//!   randomly seeded hasher and whose callee is the collection's own constructor or a trait's
//!   method (`HashMap::new()`, `Default::default()`, `.collect()`), with no turbofish on the
//!   call, as that is a written type of its own.
//!
//! Not flagged: an alias that fixes a deterministic hasher (`blaze_hash::HashMap`, `HashSet`,
//! `ByHash`, `rustc_hash::FxHashMap`), a written deterministic hasher
//! (`BuildHasherDefault<FixedHasher>`, `blaze_hash::FixedState`), a hasher parameter `S` the
//! caller picks. The type form does not see an ADT written with no generic arguments at all
//! (every parameter defaulted), nor an alias that leaves a hasher parameter of its own to a
//! default; their constructors are still found when the default is seeded at random.

use base_db::Db;
use hir_def::{AdtId, FunctionId, ItemContainerId, TraitId};
use hir_ty::next_solver::Ty;
use syntax::{TextRange, view as ast};

use ast::{AstNode as _, HasGenericArgs as _, HasName as _};

use super::{Known, facts};
use crate::check::Cx;

/// The randomly seeded hashers of crates other than std, by crate and name: foldhash's
/// `fast::RandomState` and `quality::RandomState` (hashbrown's default), ahash's
/// `RandomState`, rustc-hash's `FxRandomState`.
const RANDOM: &[(&str, &str)] =
    &[("foldhash", "RandomState"), ("ahash", "RandomState"), ("rustc_hash", "FxRandomState")];

/// What every finding says after naming the item and the type.
const CHOICE: &str = "the hasher is a choice: `blaze_hash::HashMap`/`HashSet` (fixed-seed foldhash, \
    deterministic, the default for blaze), `blaze_hash::ByHash`/`ByHashSet` (on `blaze_hash::KeyHasher`) \
    when keys are already content hashes, `blaze_hash::Blake3Hasher` for persisted content ids; a random \
    seed (`RandomState`) only where keys come from an untrusted party and HashDoS matters, written \
    explicitly with `#[expect(blaze::default_hasher, reason = \"..\")]` saying so";

/// std's map types, its random hasher, and `BuildHasher` and `Hash`.
#[derive(Clone, Copy)]
pub(super) struct Std {
    map: Option<AdtId>,
    set: Option<AdtId>,
    random: Option<AdtId>,
    build_hasher: Option<TraitId>,
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
            build_hasher: build_hasher(cx),
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

    /// Whether `adt` is std's map or set, with any hasher.
    pub(super) fn is_map_or_set(self, adt: AdtId) -> bool {
        self.hasher_at(adt).is_some()
    }

    /// Whether `ty` is std's map or set hashing with std's `RandomState`: what `hot_alloc`
    /// leaves to this lint.
    pub(super) fn is_random_map(self, ty: Ty<'_>) -> bool {
        let Some((adt, args)) = ty.as_adt() else { return false };
        let Some(at) = self.hasher_at(adt) else { return false };
        args.types()
            .nth(at)
            .and_then(|it| it.as_adt())
            .is_some_and(|(hasher, _)| self.random == Some(hasher))
    }
}

/// `BuildHasher`, from `core` or, for a crate that names `core` only through `std`, `std`.
fn build_hasher(cx: &Cx<'_, '_>) -> Option<TraitId> {
    cx.std_trait(&["core", "hash", "BuildHasher"])
        .or_else(|| cx.std_trait(&["std", "hash", "BuildHasher"]))
        .map(TraitId::from)
}

/// The type parameters of `adt` that are its hasher, as bits by their index among its type
/// parameters (lifetimes and consts left out, as `GenericArgs::types` counts them): a parameter
/// whose default implements `build_hasher`, or that `adt` bounds by it. Once per ADT.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn hasher_params(db: &Db, adt: AdtId, build_hasher: TraitId) -> u32 {
    let build_hasher = hir::Trait::from(build_hasher);
    let mut bits = 0;
    let params = hir::GenericDef::Adt(hir::Adt::from(adt)).type_or_const_params(db);
    for (at, param) in params.into_iter().filter_map(|it| it.as_type_param(db)).enumerate() {
        let hasher = param.default(db).is_some_and(|it| it.impls_trait(db, build_hasher, &[]))
            || param.trait_bounds(db).contains(&build_hasher);
        if hasher && at < u32::BITS as usize {
            bits |= 1 << at;
        }
    }
    bits
}

/// Whether `adt` is a hasher seeded at random per process.
fn is_random(db: &Db, std_random: Option<AdtId>, adt: AdtId) -> bool {
    std_random == Some(adt) || {
        let adt = hir::Adt::from(adt);
        RANDOM.iter().any(|(krate, name)| facts::adt_is(db, adt, krate, name))
    }
}

/// `adt` as a message names it: its name and the crate it is of.
fn said(db: &Db, adt: hir::Adt) -> String {
    let krate = adt.module(db).krate(db);
    let of = krate
        .base()
        .extra_data(db)
        .display_name
        .as_ref()
        .map_or_else(|| "?".to_owned(), |it| it.crate_name().symbol().as_str().to_owned()); // alloc: a message's part
    format!("`{}` of `{of}`", adt.name(db).as_str()) // alloc: a message's part
}

/// The message of a finding about `ty` in `item`, after `why` it is one.
fn message(item: &str, ty: &str, why: &str) -> String {
    format!("{item}: `{ty}` {why}; {CHOICE}") // alloc: a message, made once a finding is
}

/// A finding of the written form: its message, and the swap to blaze_hash's collection when
/// one can be offered.
pub(super) struct Written {
    pub(super) message: String,
    pub(super) fix: Option<(TextRange, &'static str)>,
}

/// The finding of the written type `ty`, if it names a hash collection whose hasher is left
/// to its default or seeded at random.
pub(super) fn check_type(cx: &Cx<'_, '_>, ty: ast::PathType<'_>) -> Option<Written> {
    let path = ty.path()?;
    let segment = path.segment()?;
    let list = segment.generic_arg_list()?;
    let written = |at: usize| {
        list.generic_args()
            .filter_map(|it| match it {
                ast::GenericArg::TypeArg(it) => it.ty(),
                _ => None,
            })
            .nth(at)
    };
    // Most path types (`Vec<T>`, `Option<T>`) are no collection's: only one with a type
    // argument is resolved.
    written(0)?;
    let adt = resolved_adt(cx, path)?;
    let db = cx.db();
    let std = Std::new(cx);
    let bits = hasher_params(db, adt, std.build_hasher?);
    if bits == 0 {
        return None;
    }
    let params = hir::GenericDef::Adt(hir::Adt::from(adt)).type_or_const_params(db);
    let types = params.into_iter().filter_map(|it| it.as_type_param(db));
    let mut why = None;
    let mut defaulted = false;
    for (at, param) in types.enumerate().take(u32::BITS as usize) {
        if bits & (1 << at) == 0 {
            continue;
        }
        match written(at) {
            None => {
                let default = param.default(db).and_then(|it| it.as_adt());
                why = Some(match default {
                    Some(it) => format!("leaves its hasher to the default, {}", said(db, it)),
                    None => "leaves its hasher to the default".to_owned(), // alloc: a message's part
                });
                defaulted = true;
                break;
            }
            Some(hasher) => {
                let random = cx.resolve_type(hasher).and_then(|it| it.as_adt());
                if let Some(random) =
                    random.filter(|it| is_random(db, std.random, AdtId::from(*it)))
                {
                    why = Some(format!(
                        "hashes with {}, seeded at random per process",
                        said(db, random)
                    ));
                    break;
                }
            }
        }
    }
    let why = why?;
    let range = cx.range(ty.syntax())?;
    let item = item_of(ty.syntax());
    let message = message(&item, cx.snippet(range), &why);
    let fix = match defaulted {
        true => swap(cx, &std, adt, path, list, written(0)),
        false => None,
    };
    Some(Written { message, fix })
}

/// The swap of std's `HashMap` or `HashSet`, written with its hasher left out, for
/// blaze_hash's: the path before the generic arguments, when the crate depends on blaze_hash
/// and the key type is `Hash`.
fn swap(
    cx: &Cx<'_, '_>,
    std: &Std,
    adt: AdtId,
    path: ast::Path<'_>,
    list: ast::GenericArgList<'_>,
    key: Option<ast::Type<'_>>,
) -> Option<(TextRange, &'static str)> {
    let to = match Some(adt) {
        it if it == std.map => "blaze_hash::HashMap",
        it if it == std.set => "blaze_hash::HashSet",
        _ => return None,
    };
    let db = cx.db();
    let krate = cx.krate();
    if facts::crate_is(db, krate, "blaze_hash")
        || facts::dep_type(db, krate.base(), "blaze_hash", &["HashMap"]).is_none()
    {
        return None;
    }
    let hash = cx
        .std_trait(&["core", "hash", "Hash"])
        .or_else(|| cx.std_trait(&["std", "hash", "Hash"]))?;
    let key = cx.resolve_type(key?)?;
    if !key.impls_trait(db, hash, &[]) {
        return None;
    }
    let path = cx.range(path.syntax())?;
    let args = cx.range(list.syntax())?;
    Some((TextRange::new(path.start(), args.start()), to))
}

/// How a message names the item `node` is written in: a field with its type, or the item.
fn item_of(node: ast::Node<'_>) -> String {
    let mut field = None;
    for it in node.ancestors() {
        if let Some(it) = ast::RecordField::cast(it) {
            field = field.or_else(|| it.name());
            continue;
        }
        let Some(item) = ast::Item::cast(it) else { continue };
        let (kind, name) = match item {
            ast::Item::Fn(it) => ("fn", it.name()),
            ast::Item::Struct(it) => ("struct", it.name()),
            ast::Item::Enum(it) => ("enum", it.name()),
            ast::Item::Union(it) => ("union", it.name()),
            ast::Item::TypeAlias(it) => ("type", it.name()),
            ast::Item::Const(it) => ("const", it.name()),
            ast::Item::Static(it) => ("static", it.name()),
            ast::Item::Trait(it) => ("trait", it.name()),
            ast::Item::Impl(_) => return "this impl".to_owned(), // alloc: a message's part
            _ => continue,
        };
        let name = name.map_or("_", |it| it.text());
        // alloc: a message's part
        return match field {
            Some(field) => format!("the field `{}` of `{kind} {name}`", field.text()),
            None => format!("`{kind} {name}`"),
        };
    }
    "this item".to_owned() // alloc: a message's part
}

fn resolved_adt(cx: &Cx<'_, '_>, path: ast::Path<'_>) -> Option<AdtId> {
    match cx.path(path)? {
        hir::PathResolution::Def(hir::ModuleDef::Adt(it)) => Some(AdtId::from(it)),
        _ => None,
    }
}

/// The finding of a call of `callee` in the item `item` names whose value has type `ty`, if it makes a hash
/// collection hashing with a randomly seeded hasher; `turbofish` whether the call writes
/// generic arguments.
pub(super) fn check_construct(
    k: &Known<'_>,
    item: impl FnOnce() -> String,
    callee: FunctionId,
    ty: Ty<'_>,
    turbofish: bool,
) -> Option<String> {
    if turbofish {
        return None;
    }
    let (adt, args) = ty.as_adt()?;
    // The random hasher first, a compare of ids: the trait query only for a type that has one.
    let (at, hasher) = args.types().enumerate().find_map(|(at, it)| {
        let (hasher, _) = it.as_adt()?;
        is_random(k.db, k.std.random, hasher).then_some((at, hasher))
    })?;
    if at >= u32::BITS as usize || hasher_params(k.db, adt, k.std.build_hasher?) & (1 << at) == 0 {
        return None;
    }
    if !constructs(k, callee, adt) {
        return None;
    }
    let collection = hir::Adt::from(adt).name(k.db);
    let why = format!(
        "is made by `{}` and hashes with {}, seeded at random per process",
        hir::Function::from(callee).name(k.db).as_str(),
        said(k.db, hir::Adt::from(hasher))
    );
    Some(message(&item(), collection.as_str(), &why))
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
