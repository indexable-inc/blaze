//! The negative-fact indexes (docs/v2/plan/lanes/items.md section 6.2): the impls of a trait by
//! simplified self type, and what a name is in a scope, each answering its question both ways
//! ("no impl", "nothing by that name" are answers too).
//!
//! Each is built from tracked queries in layers, so an edit reruns only the groupings it touches
//! and every layer above backdates where its answer stayed the same:
//!
//! - impls: one crate's impls of one trait ([`trait_impls_in`], from hir-ty's per-module
//!   bucket `TraitImpls::for_crate_and_trait`, which reruns only when a module's impls of that
//!   trait change), then every crate's impls of the trait ([`trait_impls`]), then the impls for
//!   one simplified self type ([`impls_of`]) and the blanket ones ([`blanket_impls_of`]);
//! - inherent impls: one module's by simplified self type ([`inherent_impls_in_module`]), the
//!   modules of a crate with impls for each type ([`inherent_impl_modules`],
//!   [`inherent_modules_of`]) and one module's for one type ([`module_inherent_impls_of`]),
//!   then every crate's for one self type ([`inherent_impls_of`]); a frozen crate's come from
//!   its one frozen table ([`inherent_impls_in`]);
//! - traits with an item of a name, for method candidates: one crate's for one name, from
//!   hir-ty's maintained candidate slice ([`traits_named`]), then every crate's
//!   ([`traits_with`]); a frozen crate's per module ([`traits_in_module`], [`traits_in`]);
//! - names: one scope's, all of them ([`scope_names`]), from which [`scope`] picks one.
//!
//! Impls are found as hir-ty's `TraitImpls` and `InherentImpls` find them, in every crate:
//! the crate's modules, the bodies that may hold impls, and built-in derive impls. Traits are
//! found in every crate's modules; a trait declared in a body is visible only in that body,
//! whose own edit reruns whatever read it.
//!
//! Each index is columns (docs/v2/plan/design/data.md): the searched column apart, so a binary
//! search touches only it, keys and the cold payloads beside it, and lists as runs of one
//! flat array ([`Runs`], [`ScopeNames`]).
//!
//! Each column is allocated once at its final length; the rows it is sorted from are the
//! building query's, in its task's frame. Scopes' frames are tagged [`Tag::Nameres`] (what a
//! name is in a scope is name resolution's answer), the impl and trait indexes' [`Tag::Other`].

// alloc: only with `KeyHashing`, which folds a key's own words: no SipHash.
use std::collections::HashMap;

use base_db::{Db, Crate};
use blaze_ids::{ContentId, Hasher};
use blaze_mem::{Frame, Tag, frame};
use either::Either;
use hir_def::{
    AdtId, AssocItemId, BuiltinDeriveImplId, EnumId, HasModule, ImplId, ModuleDefId, ModuleId,
    TraitId, VariantId,
    builtin_type::BuiltinType,
    item_scope::{ImportOrExternCrate, ImportOrGlob},
    nameres::crate_def_map,
    signatures::VariantFields,
    visibility::Visibility,
};
use hir_expand::name::Name;
use hir_ty::{
    db::HirDatabase,
    method_resolution::{InherentImpls, TraitImpls, delta, frozen},
    next_solver::{DbInterner, SimplifiedType as HirSimplifiedType},
};
use rustc_type_ir::fast_reject::{TreatParams, simplify_type};

use crate::{
    ImplIndexKey, ItemId, ItemKey, NAME_DOMAIN, NameHash, ScopeIndexKey, SignatureHash,
    SimplifiedType,
    hash::{self, Unhashable},
    key::{self, KeyHashing, Resolved},
    table::column,
};

const IMPLS_DOMAIN: &str = "blaze v2 index impls 2";
const SCOPE_DOMAIN: &str = "blaze v2 index scope 1";
const BUILTIN_DERIVE_DOMAIN: &str = "blaze v2 builtin derive signature 1";

/// The hash of each of `names`, in order, as [`key::name_hash`] gives it, in one batched call.
pub(crate) fn name_hashes<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<NameHash> {
    let hashes = frame::task(Tag::Other, |frame| {
        let mut inputs: Vec<&[u8], &Frame> = Vec::new_in(frame);
        inputs.extend(names.into_iter().map(str::as_bytes));
        ContentId::of_many(NAME_DOMAIN, &inputs)
    });
    // alloc: none: collected in place, in the buffer `of_many` returned.
    hashes.into_iter().map(NameHash).collect()
}

/// The impls of `key.trait_` whose simplified self type is `key.self_ty`, by key and
/// signature hash, sorted; empty is "no impl". Answers `Implements` both ways.
pub fn impls(db: &Db, key: ImplIndexKey) -> Result<ImplEntry, Unhashable> {
    match resolve_trait(db, key.trait_) {
        Some(trait_) => entry(db, impls_of(db, trait_, key.self_ty).iter()),
        None => Ok(ImplEntry::TraitAbsent),
    }
}

/// The impls of `trait_` whose self type simplifies to nothing (`impl<T> Trait for T`): they
/// unify with any self type.
pub fn blanket_impls(db: &Db, trait_: ItemKey) -> Result<ImplEntry, Unhashable> {
    match resolve_trait(db, trait_) {
        Some(trait_) => entry(db, blanket_impls_of(db, trait_).iter()),
        None => Ok(ImplEntry::TraitAbsent),
    }
}

/// What `key.name` is in `key.scope`; "nothing" is an answer. Answers `Resolves` both ways.
pub fn scope(db: &Db, key: ScopeIndexKey) -> Result<ScopeEntry, Unhashable> {
    let Some(resolved) = key::resolve(db, key.scope) else { return Ok(ScopeEntry::ScopeAbsent) };
    let owner = match resolved {
        Resolved::Module(it) => ScopeOwner::Module(it),
        Resolved::Item(ItemId::Adt(AdtId::EnumId(it))) => ScopeOwner::Enum(it),
        Resolved::Item(ItemId::Adt(AdtId::StructId(it))) => ScopeOwner::Variant(it.into()),
        Resolved::Item(ItemId::Adt(AdtId::UnionId(it))) => ScopeOwner::Variant(it.into()),
        Resolved::Variant(it) => ScopeOwner::Variant(it.into()),
        Resolved::Item(ItemId::Trait(it)) => ScopeOwner::Trait(it),
        Resolved::Item(ItemId::Impl(it)) => ScopeOwner::Impl(it),
        Resolved::Item(_)
        | Resolved::Closure { .. }
        | Resolved::Opaque { .. }
        | Resolved::BuiltinDerive(_)
        | Resolved::Ctor(_)
        | Resolved::AnonConst { .. } => {
            unreachable!("{key:?} names something that is not a scope")
        }
    };
    Ok(scope_names(db, owner, ()).get(key.name))
}

/// The impls an index holds for one question, by key with each one's signature hash.
#[derive(PartialEq, Eq, Debug)]
pub enum ImplEntry {
    /// The trait the question names no longer exists.
    TraitAbsent,
    /// Sorted by key; empty is "no impl".
    Impls(Box<[(ItemKey, SignatureHash)]>),
}

impl ImplEntry {
    pub fn value(&self) -> ContentId {
        let mut hasher = Hasher::new(IMPLS_DOMAIN);
        match self {
            ImplEntry::TraitAbsent => {
                hasher.part(b"absent");
            }
            ImplEntry::Impls(impls) => {
                hasher.part(b"impls").update(&(impls.len() as u64).to_le_bytes());
                for (key, signature) in impls.iter() {
                    hasher.id(&key.0).id(&signature.0);
                }
            }
        }
        hasher.finish()
    }
}

/// What a name is in a scope.
#[derive(PartialEq, Eq, Debug)]
pub enum ScopeEntry {
    /// The scope the question names no longer exists.
    ScopeAbsent,
    /// Nothing by that name.
    Nothing,
    /// In a module or block, in each namespace, and the `macro_rules!` of that name in textual
    /// scope at its end, in the order they are defined (the last one shadows the others).
    Module {
        types: Option<Binding>,
        values: Option<Binding>,
        macros: Option<Binding>,
        legacy_macros: Box<[ItemKey]>,
    },
    /// In an enum: its variant of that name, at its position.
    Variant { variant: ItemKey, position: u32 },
    /// In a struct, union or variant: its field of that name.
    Field { position: u32, visibility: Vis },
    /// In a trait or an impl: its associated items of that name (a type and a value may share
    /// one), sorted by key.
    Assoc(Box<[(ItemKey, Vis)]>),
}

/// What a name means in one namespace of a module.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Binding {
    pub target: Target,
    pub visibility: Vis,
    pub through: Through,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Item(ItemKey),
    /// A primitive type (`u8`, `str`), by its name.
    Builtin(NameHash),
}

/// How a name came to be in a module: an explicit item or import shadows a glob.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Through {
    Declared,
    Import,
    Glob,
    ExternCrate,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vis {
    Public,
    /// Visible in the crate of this key.
    Crate(ContentId),
    /// Visible in this module and the ones in it.
    Module(ItemKey),
}

const _: () = assert!(size_of::<Vis>() == 33);
const _: () = assert!(size_of::<Binding>() == 67);
const _: () = assert!(size_of::<Option<Binding>>() == 67);

impl ScopeEntry {
    pub fn value(&self) -> ContentId {
        let mut hasher = Hasher::new(SCOPE_DOMAIN);
        match self {
            ScopeEntry::ScopeAbsent => {
                hasher.part(b"absent");
            }
            ScopeEntry::Nothing => {
                hasher.part(b"nothing");
            }
            ScopeEntry::Module { types, values, macros, legacy_macros } => {
                hasher.part(b"module");
                for binding in [types, values, macros] {
                    write_binding(&mut hasher, binding.as_ref());
                }
                hasher.update(&(legacy_macros.len() as u64).to_le_bytes());
                for it in legacy_macros.iter() {
                    hasher.id(&it.0);
                }
            }
            ScopeEntry::Variant { variant, position } => {
                hasher.part(b"variant").id(&variant.0).update(&position.to_le_bytes());
            }
            ScopeEntry::Field { position, visibility } => {
                hasher.part(b"field").update(&position.to_le_bytes());
                write_vis(&mut hasher, *visibility);
            }
            ScopeEntry::Assoc(items) => {
                hasher.part(b"assoc").update(&(items.len() as u64).to_le_bytes());
                for &(key, visibility) in items.iter() {
                    hasher.id(&key.0);
                    write_vis(&mut hasher, visibility);
                }
            }
        }
        hasher.finish()
    }
}

fn write_binding(hasher: &mut Hasher, binding: Option<&Binding>) {
    let Some(binding) = binding else {
        hasher.part(b"none");
        return;
    };
    match binding.target {
        Target::Item(key) => hasher.part(b"item").id(&key.0),
        Target::Builtin(name) => hasher.part(b"builtin").id(&name.0),
    };
    write_vis(hasher, binding.visibility);
    hasher.part(match binding.through {
        Through::Declared => b"declared".as_slice(),
        Through::Import => b"import",
        Through::Glob => b"glob",
        Through::ExternCrate => b"extern crate",
    });
}

fn write_vis(hasher: &mut Hasher, vis: Vis) {
    match vis {
        Vis::Public => hasher.part(b"pub"),
        Vis::Crate(krate) => hasher.part(b"pub(crate)").id(&krate),
        Vis::Module(module) => hasher.part(b"pub(in)").id(&module.0),
    };
}

fn vis(db: &Db, visibility: Visibility) -> Vis {
    match visibility {
        Visibility::Public => Vis::Public,
        Visibility::PubCrate(krate) => Vis::Crate(key::crate_key(db, krate)),
        Visibility::Module(module, _) => Vis::Module(key::module_key(db, module)),
    }
}

/// An impl as the indexes hold it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum AnyImpl {
    Impl(ImplId),
    BuiltinDerive(BuiltinDeriveImplId),
}

const _: () = assert!(size_of::<AnyImpl>() == 8);

/// An impl's signature hash: an impl fact's value is computed inside a body's inference, which
/// an interface hash, holding what bodies leak, may reach. A built-in derive impl has no item
/// of its own: it is what the derive writes for its type, so it is its key (which names the
/// trait) and the type's signature.
pub(crate) fn impl_signature(
    db: &Db,
    imp: AnyImpl,
) -> Result<SignatureHash, Unhashable> {
    match imp {
        AnyImpl::Impl(it) => hash::signature_hash(db, ItemId::Impl(it)),
        AnyImpl::BuiltinDerive(it) => {
            let adt = hash::signature_hash(db, ItemId::Adt(it.loc(db).adt))?;
            let key = key::builtin_derive_key(db, it);
            Ok(SignatureHash(Hasher::new(BUILTIN_DERIVE_DOMAIN).id(&key.0).id(&adt.0).finish()))
        }
    }
}

/// `impls` with their signature hashes, in the order given.
pub(crate) fn entry<I>(db: &Db, impls: I) -> Result<ImplEntry, Unhashable>
where
    I: IntoIterator<Item = (ItemKey, AnyImpl)>,
    I::IntoIter: ExactSizeIterator,
{
    let impls = impls.into_iter();
    let mut found = Vec::with_capacity(impls.len());
    for (key, imp) in impls {
        found.push((key, impl_signature(db, imp)?));
    }
    Ok(ImplEntry::Impls(found.into_boxed_slice()))
}

/// The value of `Implements { trait_, self_ty }`: the impls for that self type and the
/// blanket ones.
pub(crate) fn implements_value(
    db: &Db,
    domain: &'static str,
    trait_: TraitId,
    self_ty: SimplifiedType,
) -> Result<ContentId, Unhashable> {
    let (impls, blanket) = implements_entries(db, trait_, self_ty)?;
    Ok(Hasher::new(domain).part(b"present").id(&impls).id(&blanket).finish())
}

/// The values of the impls of `trait_` for `self_ty` and of its blanket impls, hashed once
/// for every fact that reads them.
#[blaze_query::tracked(returns(copy))]
fn implements_entries(
    db: &Db,
    trait_: TraitId,
    self_ty: SimplifiedType,
) -> Result<(ContentId, ContentId), Unhashable> {
    let impls = entry(db, impls_of(db, trait_, self_ty).iter())?;
    let blanket = entry(db, blanket_impls_of(db, trait_).iter())?;
    Ok((impls.value(), blanket.value()))
}

/// The value of every impl of `trait_`, in the order of [`trait_impls`]: what a blanket impl's
/// coherence reads, hashed once per trait rather than once per blanket impl.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn all_impls_value(
    db: &Db,
    trait_: TraitId,
) -> Result<ContentId, Unhashable> {
    Ok(entry(db, trait_impls(db, trait_).all().iter())?.value())
}

pub(crate) fn resolve_trait(db: &Db, trait_: ItemKey) -> Option<TraitId> {
    match key::resolve(db, trait_)? {
        Resolved::Item(ItemId::Trait(it)) => Some(it),
        _ => unreachable!("{trait_:?} names something that is not a trait"),
    }
}

/// The hash of an impl's simplified self type, as hir-ty's `TraitImpls` and `InherentImpls`
/// find it: `None` for a blanket impl.
pub(crate) fn impl_self_ty(db: &Db, imp: ImplId) -> Option<SimplifiedType> {
    let self_ty = match db.impl_trait(imp) {
        Some(trait_ref) => trait_ref.instantiate_identity().skip_norm_wip().self_ty(),
        None => db.impl_self_ty(imp).instantiate_identity().skip_norm_wip(),
    };
    let simplified =
        simplify_type(DbInterner::new_no_crate(db), self_ty, TreatParams::InstantiateWithInfer)?;
    Some(key::simplified(db, &simplified))
}

/// Rows sorted by a searched column, then key, stored as columns. `searched` holds each
/// distinct searched value once, sorted, and is all a lookup's binary search touches.
/// `starts[i]..starts[i + 1]` is the run of `searched[i]` in `keys` and `values`, and the
/// rows before `starts[0]` have no searched value (a trait's blanket impls). `values` is
/// read only after the search.
#[derive(PartialEq, Eq, Hash, Debug)]
pub(crate) struct Runs<S, T> {
    searched: Box<[S]>,
    starts: Box<[u32]>,
    keys: Box<[ItemKey]>,
    values: Box<[T]>,
}

impl<S: Copy + Ord, T: Copy> Runs<S, T> {
    /// The columns of `rows`, which it sorts.
    fn new(rows: &mut [(Option<S>, ItemKey, T)]) -> Self {
        rows.sort_unstable_by_key(|&(at, key, _)| (at, key));
        let opens = |row: usize| rows[row].0.is_some() && (row == 0 || rows[row - 1].0 != rows[row].0);
        let runs = (0..rows.len()).filter(|&row| opens(row)).count();
        let mut searched = Vec::with_capacity(runs);
        let mut starts = Vec::with_capacity(runs + 1);
        for (row, &(at, _, _)) in rows.iter().enumerate() {
            if let Some(at) = at
                && opens(row)
            {
                searched.push(at);
                starts.push(row_index(row));
            }
        }
        starts.push(row_index(rows.len()));
        Runs {
            searched: searched.into_boxed_slice(),
            starts: starts.into_boxed_slice(),
            keys: column(rows.iter().map(|&(_, key, _)| key)),
            values: column(rows.iter().map(|&(_, _, value)| value)),
        }
    }

    /// The rows whose searched value is `at`, sorted by key; `None` is the rows with none.
    fn run(&self, at: Option<S>) -> Columns<'_, T> {
        let rows = match at {
            None => 0..self.starts[0] as usize,
            Some(at) => match self.searched.binary_search(&at) {
                Ok(i) => self.starts[i] as usize..self.starts[i + 1] as usize,
                Err(_) => 0..0,
            },
        };
        // clone: a range, two indexes.
        Columns { keys: &self.keys[rows.clone()], values: &self.values[rows] }
    }

    /// Every row, in order.
    pub(crate) fn all(&self) -> Columns<'_, T> {
        Columns { keys: &self.keys, values: &self.values }
    }

    /// Every row with its searched value, in order.
    fn rows(&self) -> impl Iterator<Item = (Option<S>, ItemKey, T)> + '_ {
        let none = self.run(None).iter().map(|(key, value)| (None, key, value));
        let runs = self.searched.iter().enumerate().flat_map(move |(i, &at)| {
            let rows = self.starts[i] as usize..self.starts[i + 1] as usize;
            // clone: a range, two indexes.
            Columns { keys: &self.keys[rows.clone()], values: &self.values[rows] }
                .iter()
                .map(move |(key, value)| (Some(at), key, value))
        });
        none.chain(runs)
    }
}

fn row_index(row: usize) -> u32 {
    u32::try_from(row).expect("an index has fewer than 2^32 rows")
}

/// Rows as two columns of one length: keys, and what each key is.
#[derive(Clone, Copy)]
pub(crate) struct Columns<'a, T> {
    keys: &'a [ItemKey],
    values: &'a [T],
}

impl<'a, T: Copy> Columns<'a, T> {
    pub(crate) fn iter(self) -> impl ExactSizeIterator<Item = (ItemKey, T)> + 'a {
        self.keys.iter().copied().zip(self.values.iter().copied())
    }
}

/// One question's rows, sorted by key, as two columns.
#[derive(PartialEq, Eq, Hash, Debug)]
pub(crate) struct Keyed<T> {
    keys: Box<[ItemKey]>,
    values: Box<[T]>,
}

impl<T: Copy> Keyed<T> {
    /// The columns of `rows`, which it sorts.
    fn sorted(rows: &mut [(ItemKey, T)]) -> Self {
        rows.sort_unstable_by_key(|&(key, _)| key);
        Keyed::of(rows.iter().copied())
    }

    fn of(rows: impl ExactSizeIterator<Item = (ItemKey, T)>) -> Self {
        let mut keys = Vec::with_capacity(rows.len());
        let mut values = Vec::with_capacity(rows.len());
        for (key, value) in rows {
            keys.push(key);
            values.push(value);
        }
        Keyed { keys: keys.into_boxed_slice(), values: values.into_boxed_slice() }
    }

    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = (ItemKey, T)> + '_ {
        Columns { keys: &self.keys, values: &self.values }.iter()
    }
}

/// The impls of `trait_` in `krate`, as [`trait_impls`] holds them.
#[blaze_query::tracked(returns(ref))]
fn trait_impls_in(
    db: &Db,
    krate: Crate,
    trait_: TraitId,
) -> Runs<SimplifiedType, AnyImpl> {
    frame::task(Tag::Other, |frame| {
        let mut found: Vec<_, &Frame> = Vec::new_in(frame);
        TraitImpls::for_crate_and_trait(db, krate, trait_).for_trait(trait_, |impls| match impls {
            Either::Left(impls) => found.extend(impls.iter().map(|&it| {
                (impl_self_ty(db, it), key::item_key(db, ItemId::Impl(it)), AnyImpl::Impl(it))
            })),
            Either::Right(derives) => found.extend(derives.iter().map(|&it| {
                let adt = HirSimplifiedType::Adt(it.loc(db).adt.into());
                (
                    Some(key::simplified(db, &adt)),
                    key::builtin_derive_key(db, it),
                    AnyImpl::BuiltinDerive(it),
                )
            })),
        });
        Runs::new(&mut found)
    })
}

/// The impls of `trait_` in every crate, by simplified self type (the blanket ones as their
/// own run, first), then key.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn trait_impls(db: &Db, trait_: TraitId) -> Runs<SimplifiedType, AnyImpl> {
    frame::task(Tag::Other, |frame| {
        let mut rows: Vec<_, &Frame> = Vec::new_in(frame);
        // `krate` and the crates that depend on it: the only ones that can hold an impl of
        // one of its traits, which the impl names.
        for &krate in trait_.krate(db).transitive_rev_deps(db) {
            rows.extend(trait_impls_in(db, krate, trait_).rows());
        }
        Runs::new(&mut rows)
    })
}

/// The impls of `trait_` whose simplified self type is `self_ty`, sorted by key.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn impls_of(
    db: &Db,
    trait_: TraitId,
    self_ty: SimplifiedType,
) -> Keyed<AnyImpl> {
    Keyed::of(trait_impls(db, trait_).run(Some(self_ty)).iter())
}

/// The blanket impls of `trait_`, sorted by key.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn blanket_impls_of(db: &Db, trait_: TraitId) -> Keyed<AnyImpl> {
    Keyed::of(trait_impls(db, trait_).run(None).iter())
}

/// The inherent impls of a frozen `krate`, from its one frozen table, by the hash of their
/// simplified self type, then key.
#[blaze_query::tracked(returns(ref))]
fn inherent_impls_in(db: &Db, krate: Crate) -> Runs<SimplifiedType, ImplId> {
    inherent_rows(db, InherentImpls::for_crate(db, krate))
}

/// The inherent impls of `module` and of the blocks in the bodies it holds, by the hash of
/// their simplified self type, then key: equal unless one of them is added, removed or
/// changes its self type.
#[blaze_query::tracked(returns(ref))]
fn inherent_impls_in_module(db: &Db, module: ModuleId) -> Runs<SimplifiedType, ImplId> {
    inherent_rows(db, InherentImpls::for_module(db, module))
}

fn inherent_rows(db: &Db, impls: &InherentImpls<'_>) -> Runs<SimplifiedType, ImplId> {
    frame::task(Tag::Other, |frame| {
        let mut rows: Vec<_, &Frame> = Vec::new_in(frame);
        for (self_ty, impls) in impls.iter() {
            let self_ty = key::simplified(db, self_ty);
            rows.extend(
                impls.iter().map(|&it| (Some(self_ty), key::item_key(db, ItemId::Impl(it)), it)),
            );
        }
        Runs::new(&mut rows)
    })
}

/// The modules of `krate` with an inherent impl for each simplified self type, by its hash,
/// then the module's key: equal unless a module gains its first impl for a type or loses its
/// last.
#[blaze_query::tracked(returns(ref))]
fn inherent_impl_modules(db: &Db, krate: Crate) -> Runs<SimplifiedType, ModuleId> {
    frame::task(Tag::Other, |frame| {
        let mut rows: Vec<_, &Frame> = Vec::new_in(frame);
        for (module, _) in crate_def_map(db, krate).modules(db) {
            let in_module = inherent_impls_in_module(db, module);
            if in_module.searched.is_empty() {
                continue;
            }
            let key = key::module_key(db, module);
            rows.extend(in_module.searched.iter().map(|&self_ty| (Some(self_ty), key, module)));
        }
        Runs::new(&mut rows)
    })
}

/// The modules of `krate` with an inherent impl for `self_ty`: equal unless that list changes,
/// whatever other types gain.
#[blaze_query::tracked(returns(deref))]
fn inherent_modules_of(db: &Db, krate: Crate, self_ty: SimplifiedType) -> Box<[ModuleId]> {
    let modules = inherent_impl_modules(db, krate).run(Some(self_ty));
    column(modules.iter().map(|(_, module)| module))
}

/// The inherent impls of `module` for `self_ty`, sorted by key: equal unless one for that
/// type is added there or removed.
#[blaze_query::tracked(returns(ref))]
fn module_inherent_impls_of(
    db: &Db,
    module: ModuleId,
    self_ty: SimplifiedType,
) -> Keyed<ImplId> {
    Keyed::of(inherent_impls_in_module(db, module).run(Some(self_ty)).iter())
}

/// The inherent impls of every crate for `self_ty`, sorted by key. Reads, in each crate, only
/// the modules with impls for that type.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn inherent_impls_of(
    db: &Db,
    self_ty: SimplifiedType,
    _: (),
) -> Keyed<ImplId> {
    frame::task(Tag::Other, |frame| {
        let mut rows: Vec<_, &Frame> = Vec::new_in(frame);
        for &krate in base_db::all_crates(db).iter() {
            if frozen::served(db, krate) {
                rows.extend(inherent_impls_in(db, krate).run(Some(self_ty)).iter());
                continue;
            }
            for &module in inherent_modules_of(db, krate, self_ty) {
                rows.extend(module_inherent_impls_of(db, module, self_ty).iter());
            }
        }
        Keyed::sorted(&mut rows)
    })
}

/// The traits declared in `module` of a frozen crate, by the hash of each name one of their
/// items has, then key, each pair once: equal unless such a trait or the names of its items
/// change. A maintained crate's are its candidate slices ([`traits_named`]).
#[blaze_query::tracked(returns(deref))]
fn traits_in_module(db: &Db, module: ModuleId) -> Box<[(NameHash, ItemKey, TraitId)]> {
    frame::task(Tag::Other, |frame| {
        let mut names: Vec<&str, &Frame> = Vec::new_in(frame);
        let mut traits: Vec<_, &Frame> = Vec::new_in(frame);
        for def in module.def_map(db).module(db, module).scope(db).declarations() {
            let ModuleDefId::TraitId(trait_) = def else { continue };
            let key = key::item_key(db, ItemId::Trait(trait_));
            for (name, _) in trait_.trait_items(db).items.iter() {
                names.push(name.as_str());
                traits.push((key, trait_));
            }
        }
        let mut found: Vec<_, &Frame> = Vec::with_capacity_in(traits.len(), frame);
        found.extend(
            name_hashes(names.iter().copied())
                .into_iter()
                .zip(traits.iter().copied())
                .map(|(name, (key, trait_))| (name, key, trait_)),
        );
        found.sort_unstable_by_key(|&(name, key, _)| (name, key));
        found.dedup_by_key(|&mut (name, key, _)| (name, key));
        column(found.iter().copied())
    })
}

/// The traits declared in `krate`'s modules, by the hash of each name one of their items
/// has, then key: reruns only when some module's [`traits_in_module`] changes. A trait is
/// declared in one module, so the modules' rows never repeat one another.
#[blaze_query::tracked(returns(ref))]
fn traits_in(db: &Db, krate: Crate) -> Runs<NameHash, TraitId> {
    frame::task(Tag::Other, |frame| {
        let mut found: Vec<_, &Frame> = Vec::new_in(frame);
        for (module, _) in crate_def_map(db, krate).modules(db) {
            let rows = traits_in_module(db, module);
            found.extend(rows.iter().map(|&(name, key, trait_)| (Some(name), key, trait_)));
        }
        Runs::new(&mut found)
    })
}

/// Every name a trait item of maintained `krate` has (hir-ty's candidate directory), by hash,
/// then name: reruns only when the crate's traits gain a name never seen in it.
#[blaze_query::tracked(returns(deref))]
fn candidate_names(db: &Db, krate: Crate) -> Box<[(NameHash, Name)]> {
    frame::task(Tag::Other, |frame| {
        let mut names: Vec<Name, &Frame> = Vec::new_in(frame);
        names.extend(delta::candidate_names(db, krate));
        let hashes = name_hashes(names.iter().map(Name::as_str));
        let mut rows: Vec<(NameHash, Name), &Frame> = Vec::with_capacity_in(names.len(), frame);
        rows.extend(hashes.into_iter().zip(names.iter().copied()));
        rows.sort_unstable();
        column(rows.iter().copied())
    })
}

/// The traits declared in maintained `krate`'s modules with an item whose name hashes to
/// `name`, sorted by key: the candidate slice of that name alone, so a trait gaining an item of
/// another name leaves this as it was.
#[blaze_query::tracked(returns(ref))]
fn traits_named(db: &Db, krate: Crate, name: NameHash) -> Keyed<TraitId> {
    frame::task(Tag::Other, |frame| {
        let names = candidate_names(db, krate);
        let start = names.partition_point(|&(it, _)| it < name);
        let mut rows: Vec<(ItemKey, TraitId), &Frame> = Vec::new_in(frame);
        for &(_, named) in names[start..].iter().take_while(|&&(it, _)| it == name) {
            rows.extend(
                delta::candidate_traits(db, krate, named)
                    .iter()
                    .map(|&trait_| (key::item_key(db, ItemId::Trait(trait_)), trait_)),
            );
        }
        rows.sort_unstable_by_key(|&(key, _)| key);
        rows.dedup_by_key(|&mut (key, _)| key);
        Keyed::of(rows.iter().copied())
    })
}

/// The traits of every crate with an item whose name hashes to `name`, sorted by key.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn traits_with(db: &Db, name: NameHash, _: ()) -> Keyed<TraitId> {
    frame::task(Tag::Other, |frame| {
        let mut rows: Vec<_, &Frame> = Vec::new_in(frame);
        for &krate in base_db::all_crates(db).iter() {
            if frozen::served(db, krate) {
                rows.extend(traits_in(db, krate).run(Some(name)).iter());
            } else {
                rows.extend(traits_named(db, krate, name).iter());
            }
        }
        Keyed::sorted(&mut rows)
    })
}

/// A scope names are looked up in.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum ScopeOwner {
    Module(ModuleId),
    Enum(EnumId),
    Variant(VariantId),
    Trait(TraitId),
    Impl(ImplId),
}

/// One scope's names as columns: `names`, sorted, is all a lookup's binary search touches;
/// what the name at a position is sits at that position in `entries`.
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct ScopeNames {
    names: Box<[NameHash]>,
    entries: ScopeColumns,
}

/// What each name of a scope is, one column per part (a scope's names are all of one kind).
/// `starts[i]..starts[i + 1]` is name `i`'s run in the flat columns after it.
#[derive(PartialEq, Eq, Debug)]
enum ScopeColumns {
    Module {
        types: Box<[Option<Binding>]>,
        values: Box<[Option<Binding>]>,
        macros: Box<[Option<Binding>]>,
        starts: Box<[u32]>,
        legacy_macros: Box<[ItemKey]>,
    },
    Variant {
        variants: Box<[ItemKey]>,
        positions: Box<[u32]>,
    },
    Field {
        positions: Box<[u32]>,
        visibilities: Box<[Vis]>,
    },
    Assoc {
        starts: Box<[u32]>,
        keys: Box<[ItemKey]>,
        visibilities: Box<[Vis]>,
    },
}

impl ScopeNames {
    fn get(&self, name: NameHash) -> ScopeEntry {
        let Ok(at) = self.names.binary_search(&name) else { return ScopeEntry::Nothing };
        let run = |starts: &[u32]| starts[at] as usize..starts[at + 1] as usize;
        match &self.entries {
            ScopeColumns::Module { types, values, macros, starts, legacy_macros } => {
                ScopeEntry::Module {
                    types: types[at],
                    values: values[at],
                    macros: macros[at],
                    legacy_macros: legacy_macros[run(starts)].into(),
                }
            }
            ScopeColumns::Variant { variants, positions } => {
                ScopeEntry::Variant { variant: variants[at], position: positions[at] }
            }
            ScopeColumns::Field { positions, visibilities } => {
                ScopeEntry::Field { position: positions[at], visibility: visibilities[at] }
            }
            ScopeColumns::Assoc { starts, keys, visibilities } => {
                let items = run(starts);
                // clone: a range, two indexes.
                let keys = keys[items.clone()].iter().copied();
                // alloc: `ScopeEntry` owns its items; see v2/requests/mem-backend-items.md.
                ScopeEntry::Assoc(keys.zip(visibilities[items].iter().copied()).collect())
            }
        }
    }
}

/// Every name of `owner` with what it is there, sorted by the name's hash.
#[blaze_query::tracked(returns(ref))]
fn scope_names(db: &Db, owner: ScopeOwner, _: ()) -> ScopeNames {
    frame::task(Tag::Nameres, |frame| scope_names_in(db, owner, frame))
}

fn scope_names_in(db: &Db, owner: ScopeOwner, frame: &Frame) -> ScopeNames {
    match owner {
        ScopeOwner::Module(module) => module_names(db, module, frame),
        ScopeOwner::Enum(it) => {
            let variants = &it.enum_variants(db).variants;
            let mut rows: Vec<_, &Frame> = Vec::with_capacity_in(variants.len(), frame);
            rows.extend(
                name_hashes(variants.keys().map(|name| name.as_str()))
                    .into_iter()
                    .zip(variants.values())
                    .enumerate()
                    .map(|(position, (name, &(variant, _)))| {
                        (name, key::variant_key(db, variant), position as u32)
                    }),
            );
            rows.sort_unstable_by_key(|&(name, _, _)| name);
            ScopeNames {
                names: column(rows.iter().map(|it| it.0)),
                entries: ScopeColumns::Variant {
                    variants: column(rows.iter().map(|it| it.1)),
                    positions: column(rows.iter().map(|it| it.2)),
                },
            }
        }
        ScopeOwner::Variant(variant) => {
            let visibilities = VariantFields::field_visibilities(db, variant);
            let fields = variant.fields(db).fields();
            let mut rows: Vec<_, &Frame> = Vec::with_capacity_in(fields.len(), frame);
            rows.extend(
                name_hashes(fields.iter().map(|(_, data)| data.name.as_str()))
                    .into_iter()
                    .zip(fields.iter())
                    .map(|(name, (field, _))| {
                        let visibility =
                            visibilities.get(field).expect("every field has a visibility");
                        (name, u32::from(field.into_raw()), vis(db, *visibility))
                    }),
            );
            rows.sort_unstable_by_key(|&(name, position, _)| (name, position));
            ScopeNames {
                names: column(rows.iter().map(|it| it.0)),
                entries: ScopeColumns::Field {
                    positions: column(rows.iter().map(|it| it.1)),
                    visibilities: column(rows.iter().map(|it| it.2)),
                },
            }
        }
        ScopeOwner::Trait(it) => assoc_names(db, &it.trait_items(db).items, frame),
        ScopeOwner::Impl(it) => assoc_names(db, &it.impl_items(db).items, frame),
    }
}

/// A trait's or impl's items by name, each name's run sorted by key (a type and a value may
/// share a name).
fn assoc_names(
    db: &Db,
    items: &[(hir_expand::name::Name, AssocItemId)],
    frame: &Frame,
) -> ScopeNames {
    let mut rows: Vec<_, &Frame> = Vec::with_capacity_in(items.len(), frame);
    rows.extend(name_hashes(items.iter().map(|(name, _)| name.as_str())).into_iter().zip(items).map(
        |(name, &(_, item))| {
            (name, key::item_key(db, ItemId::from(item)), vis(db, item.assoc_visibility(db)))
        },
    ));
    rows.sort_unstable_by_key(|&(name, key, _)| (name, key));
    let opens = |row: usize| row == 0 || rows[row - 1].0 != rows[row].0;
    let runs = (0..rows.len()).filter(|&row| opens(row)).count();
    let mut names = Vec::with_capacity(runs);
    let mut starts = Vec::with_capacity(runs + 1);
    for (row, &(name, _, _)) in rows.iter().enumerate() {
        if opens(row) {
            names.push(name);
            starts.push(row_index(row));
        }
    }
    starts.push(row_index(rows.len()));
    ScopeNames {
        names: names.into_boxed_slice(),
        entries: ScopeColumns::Assoc {
            starts: starts.into_boxed_slice(),
            keys: column(rows.iter().map(|it| it.1)),
            visibilities: column(rows.iter().map(|it| it.2)),
        },
    }
}

/// One name of a module while its columns are built; `legacy` is its position in the
/// module's legacy macros.
#[derive(Default)]
struct ModuleName {
    types: Option<Binding>,
    values: Option<Binding>,
    macros: Option<Binding>,
    legacy: Option<usize>,
}

fn module_names(db: &Db, module: ModuleId, frame: &Frame) -> ScopeNames {
    let scope = module.def_map(db).module(db, module).scope(db);
    let mut entries: Vec<_, &Frame> = Vec::new_in(frame);
    entries.extend(scope.entries().filter(|(_, per_ns)| !per_ns.is_none()));
    let mut legacy: Vec<_, &Frame> = Vec::new_in(frame);
    legacy.extend(scope.legacy_macros());
    let names = name_hashes(
        entries
            .iter()
            .map(|(name, _)| name.as_str())
            .chain(legacy.iter().map(|(name, _)| name.as_str())),
    );
    let (entry_names, legacy_names) = names.split_at(entries.len());
    // alloc: one table at its final size per module scope; std's map takes no allocator.
    let mut by_name: HashMap<NameHash, ModuleName, KeyHashing> =
        HashMap::with_capacity_and_hasher(names.len(), KeyHashing);
    let through_ns = |import: Option<ImportOrExternCrate>| match import {
        None => Through::Declared,
        Some(ImportOrExternCrate::Import(_)) => Through::Import,
        Some(ImportOrExternCrate::Glob(_)) => Through::Glob,
        Some(ImportOrExternCrate::ExternCrate(_)) => Through::ExternCrate,
    };
    for (&name, (_, per_ns)) in entry_names.iter().zip(entries) {
        let at = by_name.entry(name).or_default();
        at.types = per_ns.types.map(|it| Binding {
            target: target(db, it.def),
            visibility: vis(db, it.vis),
            through: through_ns(it.import),
        });
        at.values = per_ns.values.map(|it| Binding {
            target: target(db, it.def),
            visibility: vis(db, it.vis),
            through: match it.import {
                None => Through::Declared,
                Some(ImportOrGlob::Import(_)) => Through::Import,
                Some(ImportOrGlob::Glob(_)) => Through::Glob,
            },
        });
        at.macros = per_ns.macros.map(|it| Binding {
            target: Target::Item(key::item_key(db, ItemId::Macro(it.def))),
            visibility: vis(db, it.vis),
            through: through_ns(it.import),
        });
    }
    for (position, &name) in legacy_names.iter().enumerate() {
        by_name.entry(name).or_default().legacy = Some(position);
    }
    let mut rows: Vec<_, &Frame> = Vec::with_capacity_in(by_name.len(), frame);
    rows.extend(by_name);
    rows.sort_unstable_by_key(|&(name, _)| name);
    let mut starts = Vec::with_capacity(rows.len() + 1);
    let count = rows.iter().filter_map(|(_, it)| it.legacy).map(|at| legacy[at].1.len()).sum();
    let mut legacy_macros = Vec::with_capacity(count);
    for (_, it) in rows.iter() {
        starts.push(row_index(legacy_macros.len()));
        if let Some(position) = it.legacy {
            let (_, macros) = legacy[position];
            legacy_macros.extend(macros.iter().map(|&it| key::item_key(db, ItemId::Macro(it))));
        }
    }
    starts.push(row_index(legacy_macros.len()));
    ScopeNames {
        names: column(rows.iter().map(|&(name, _)| name)),
        entries: ScopeColumns::Module {
            types: column(rows.iter().map(|(_, it)| it.types)),
            values: column(rows.iter().map(|(_, it)| it.values)),
            macros: column(rows.iter().map(|(_, it)| it.macros)),
            starts: starts.into_boxed_slice(),
            legacy_macros: legacy_macros.into_boxed_slice(),
        },
    }
}

fn target(db: &Db, def: ModuleDefId) -> Target {
    let item = |id| Target::Item(key::item_key(db, id));
    match def {
        ModuleDefId::ModuleId(it) => Target::Item(key::module_key(db, it)),
        ModuleDefId::FunctionId(it) => item(ItemId::Function(it)),
        ModuleDefId::AdtId(it) => item(ItemId::Adt(it)),
        ModuleDefId::EnumVariantId(it) => Target::Item(key::variant_key(db, it)),
        ModuleDefId::ConstId(it) => item(ItemId::Const(it)),
        ModuleDefId::StaticId(it) => item(ItemId::Static(it)),
        ModuleDefId::TraitId(it) => item(ItemId::Trait(it)),
        ModuleDefId::TypeAliasId(it) => item(ItemId::Alias(it)),
        ModuleDefId::MacroId(it) => item(ItemId::Macro(it)),
        ModuleDefId::BuiltinType(it) => {
            let (name, _) = BuiltinType::all_builtin_types()
                .into_iter()
                .find(|&(_, ty)| ty == it)
                .expect("every builtin type has a name");
            Target::Builtin(key::name_hash(name.as_str()))
        }
    }
}
