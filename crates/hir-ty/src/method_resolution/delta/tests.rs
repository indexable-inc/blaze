//! Tests of the maintained impl tables and trait candidates (docs/v2/plan/design/dbsp.md §12),
//! and the helpers they share.
//!
//! The reference is today's pull: the tables built from every module's
//! `TraitImpls::for_module`/`InherentImpls::for_module` in `DefMap::modules` order, which is
//! what `for_crate` was before the slices. [`check`] compares every reader of the slices with
//! it, and every slice with the arrangement made from scratch over `module_impls` and the
//! traits' items, in the same database.

mod batch;
mod cost;
mod random;

use std::fmt::Write as _;

use base_db::{Crate, Db, Maintained, all_crates};
use blaze_dbsp::Work;
use blaze_hash::HashMap;
use blaze_mem::Tag;
use hir_def::{
    BuiltinDeriveImplId, ImplId, ModuleDefId, ModuleId, TraitId,
    lang_item::lang_items,
    nameres::{crate_def_map, module_impls},
    signatures::TraitSignature,
};
use hir_expand::{fixture, name::Name};
use smallvec::SmallVec;
use span::FileId;

use super::{
    head::{Head, try_head},
    install_identities,
    key::{ImplKey, assoc_names, derive_key, impl_key},
    publish::{
        candidate_rows_hash, candidate_slice_hash, directory, inherent_rows_hash,
        inherent_slice_hash, location, module_rank, trait_rows_hash, trait_slice_hash,
    },
    read::{inherent_bodies, trait_bodies},
    state::NAME,
};
use crate::method_resolution::{
    InherentImpls, InherentImplsBuilder, SelfTyKey, TraitImpls, TraitImplsBuilder,
};

/// A database of a fixture, maintained as it was loaded, with the fixture's files in order.
pub(super) struct Loaded {
    pub(super) db: Db,
    pub(super) files: Vec<FileId>,
}

impl Loaded {
    pub(super) fn new(fixture: &str) -> Loaded {
        let fx = fixture::db(fixture);
        let files = fx.files().iter().map(|it| it.file_id(fx.db())).collect();
        let mut db = fx.into_db();
        install_identities(&mut db, location::impl_identity, location::def_identity);
        Loaded { db, files }
    }

    /// The crate whose root is the fixture's first file.
    pub(super) fn krate(&self) -> Crate {
        let root = self.files[0];
        *all_crates(&self.db)
            .iter()
            .find(|krate| krate.data(&self.db).root_file_id == root)
            .expect("a crate has the fixture's first file for its root")
    }
}

/// The trait named `name` declared in a module of `krate`.
pub(super) fn trait_named(db: &Db, krate: Crate, name: &str) -> TraitId {
    declared_traits(db, krate)
        .into_iter()
        .find(|&it| TraitSignature::of(db, it).name.as_str() == name)
        .unwrap_or_else(|| panic!("no trait `{name}` in {krate:?}"))
}

/// The traits declared in `krate`'s modules, in module order.
fn declared_traits(db: &Db, krate: Crate) -> Vec<TraitId> {
    let mut out = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        out.extend(module.scope(db).declarations().filter_map(|def| match def {
            ModuleDefId::TraitId(it) => Some(it),
            _ => None,
        }));
    }
    out
}

/// The `impls` maintainer's records of `krate` kept since the last take, one per write.
pub(super) fn take_work(db: &mut Db, krate: Crate) -> Vec<Work> {
    let mut records: Vec<Maintained> = Vec::new();
    db.take_maintained(&mut records);
    records
        .into_iter()
        .filter(|it| it.maintainer == NAME && it.krate == krate)
        .map(|it| it.work)
        .collect()
}

/// The `Work` of the one write `f` makes to `db`, for `krate`.
pub(super) fn work_of(db: &mut Db, krate: Crate, f: impl FnOnce(&mut Db)) -> Work {
    db.keep_maintained(true);
    let mut stale = Vec::new();
    db.take_maintained(&mut stale);
    f(db);
    let work = take_work(db, krate);
    db.keep_maintained(false);
    assert_eq!(work.len(), 1, "the edit is one write: {work:?}");
    work[0]
}

/// Compares everything the slices give for `krate` with the reference; the differences, one
/// line each, into `failures`.
pub(super) fn check(db: &Db, krate: Crate, what: &str, failures: &mut String) {
    check_tables(db, krate, what, failures);
    check_slices(db, krate, what, failures);
    check_candidates(db, krate, what, failures);
}

/// The readers against today's pull over `for_module`.
fn check_tables(db: &Db, krate: Crate, what: &str, failures: &mut String) {
    blaze_mem::frame::task(Tag::Infer, |frame| {
        let mut traits = TraitImplsBuilder::new(frame);
        let mut inherent = InherentImplsBuilder::new(frame);
        for (module, _) in crate_def_map(db, krate).modules(db) {
            TraitImpls::for_module(db, module).merge_into(&mut traits);
            InherentImpls::for_module(db, module).merge_into(&mut inherent);
        }
        let traits = traits.finish();
        let inherent = inherent.finish();
        if **TraitImpls::for_crate(db, krate) != traits {
            _ = writeln!(failures, "{what}: TraitImpls::for_crate differs from the pull");
        }
        if *InherentImpls::for_crate(db, krate) != inherent {
            _ = writeln!(failures, "{what}: InherentImpls::for_crate differs from the pull");
        }
        let mut asked: Vec<TraitId> = traits.trait_indices().map(|(it, _)| it).collect();
        asked.extend(declared_traits(db, krate));
        for trait_ in asked {
            let mut alone = TraitImplsBuilder::new(frame);
            traits.merge_trait_into(trait_, &mut alone);
            if *TraitImpls::for_crate_and_trait(db, krate, trait_) != alone.finish() {
                _ = writeln!(failures, "{what}: for_crate_and_trait({trait_:?}) differs");
            }
        }
        for (self_ty, impls) in inherent.iter() {
            if InherentImpls::in_crate_for_self_ty(db, krate, SelfTyKey(*self_ty)) != impls {
                _ = writeln!(failures, "{what}: in_crate_for_self_ty({self_ty:?}) differs");
            }
        }
    });
}

/// Every trait and inherent slice against the rows made from scratch over `module_impls`, by
/// module in `DefMap::modules` order (a slice's position order, sorted by module rank, is
/// that), with the body modules from every module's body tables; the directory's body modules
/// likewise; and each hash against its rows'.
fn check_slices(db: &Db, krate: Crate, what: &str, failures: &mut String) {
    type TraitExpected = (
        Vec<(ModuleId, ImplId, Option<Head>)>,
        Vec<(ModuleId, BuiltinDeriveImplId, Head)>,
        Vec<ModuleId>,
    );
    let lang = lang_items(db, krate);
    let mut by_trait: HashMap<TraitId, TraitExpected> = HashMap::default();
    let mut by_head: HashMap<Head, (Vec<(ModuleId, ImplId)>, Vec<ModuleId>)> = HashMap::default();
    let mut body_modules = Vec::new();
    for (module, _) in crate_def_map(db, krate).modules(db) {
        for &trait_ in trait_bodies(db, module).traits.iter() {
            by_trait.entry(trait_).or_default().2.push(module);
        }
        let inherent = inherent_bodies(db, module);
        for (self_ty, _) in inherent.iter() {
            let head = try_head(self_ty).expect("a body's inherent impl has a head");
            by_head.entry(head).or_default().1.push(module);
        }
        if inherent.iter().next().is_some() {
            body_modules.push(module);
        }
        let impls = module_impls(db, module);
        for &imp in impls.trait_impls.iter() {
            if let ImplKey::Trait { trait_, head } = impl_key(db, imp, true) {
                by_trait.entry(trait_).or_default().0.push((module, imp, head));
            }
        }
        for &imp in impls.inherent_impls.iter() {
            if let ImplKey::Inherent(head) = impl_key(db, imp, false) {
                by_head.entry(head).or_default().0.push((module, imp));
            }
        }
        for &imp in impls.builtin_derive_impls.iter() {
            if let Some((trait_, head)) = derive_key(db, imp, lang) {
                by_trait.entry(trait_).or_default().1.push((module, imp, head));
            }
        }
    }
    let directory = directory(db, krate);
    body_modules.sort_unstable();
    if **directory.body_modules(db) != *body_modules {
        _ = writeln!(failures, "{what}: the directory's body modules differ from scratch");
    }
    for (&trait_, slice) in directory.traits(db).iter() {
        let rows = slice.rows(db);
        let mut expected = by_trait.remove(&trait_).unwrap_or_default();
        expected.2.sort_unstable();
        let mut impls = rows.impls.to_vec();
        impls.sort_by_key(|&(module, ..)| module_rank(db, module));
        let mut derives = rows.derives.to_vec();
        derives.sort_by_key(|&(module, ..)| module_rank(db, module));
        if (impls, derives, rows.bodies.to_vec()) != expected {
            _ = writeln!(failures, "{what}: the slice of {trait_:?} differs from scratch");
        }
        if trait_slice_hash(db, *slice) != trait_rows_hash(db, trait_, rows) {
            _ = writeln!(failures, "{what}: the hash of {trait_:?}'s slice is not its rows'");
        }
    }
    for (trait_, rows) in by_trait {
        _ = writeln!(failures, "{what}: {trait_:?} has no slice for its rows {rows:?}");
    }
    for (&head, slice) in directory.heads(db).iter() {
        let rows = slice.rows(db);
        let mut expected = by_head.remove(&head).unwrap_or_default();
        expected.1.sort_unstable();
        let mut impls: Vec<_> =
            rows.impls.iter().map(|(module, imp, _)| (*module, *imp)).collect();
        impls.sort_by_key(|&(module, _)| module_rank(db, module));
        if (impls, rows.bodies.to_vec()) != expected {
            _ = writeln!(failures, "{what}: the slice of {head:?} differs from scratch");
        }
        let ordered = rows.impls.windows(2).all(|it| it[0].0 != it[1].0 || it[0].2 < it[1].2);
        if !ordered {
            _ = writeln!(failures, "{what}: the slice of {head:?} is out of position order");
        }
        if inherent_slice_hash(db, *slice) != inherent_rows_hash(db, head, rows) {
            _ = writeln!(failures, "{what}: the hash of {head:?}'s slice is not its rows'");
        }
    }
    for (head, rows) in by_head {
        _ = writeln!(failures, "{what}: {head:?} has no slice for its rows {rows:?}");
    }
}

/// Every candidate slice against the traits declared in the crate's modules by item name.
fn check_candidates(db: &Db, krate: Crate, what: &str, failures: &mut String) {
    let mut by_name: HashMap<Name, Vec<TraitId>> = HashMap::default();
    let mut names = SmallVec::new();
    for trait_ in declared_traits(db, krate) {
        assoc_names(db, trait_, &mut names);
        for &name in names.iter() {
            by_name.entry(name).or_default().push(trait_);
        }
    }
    for (&name, slice) in directory(db, krate).names(db).iter() {
        let rows = slice.rows(db);
        let mut expected = by_name.remove(&name).unwrap_or_default();
        expected.sort_unstable();
        if *rows.traits != *expected {
            let name = name.as_str();
            _ = writeln!(failures, "{what}: the candidates of `{name}` differ from scratch");
        }
        if candidate_slice_hash(db, *slice) != candidate_rows_hash(db, name, rows) {
            let name = name.as_str();
            _ = writeln!(failures, "{what}: the hash of `{name}`'s candidates is not its rows'");
        }
    }
    for (name, traits) in by_name {
        let name = name.as_str();
        _ = writeln!(failures, "{what}: `{name}` has no candidate slice for {traits:?}");
    }
}

/// Every impl of `krate`'s modules with its key, as made from scratch.
pub(super) fn impl_keys(db: &Db, krate: Crate) -> HashMap<ImplId, ImplKey> {
    let mut out = HashMap::default();
    for (module, _) in crate_def_map(db, krate).modules(db) {
        let impls = module_impls(db, module);
        out.extend(impls.trait_impls.iter().map(|&imp| (imp, impl_key(db, imp, true))));
        out.extend(impls.inherent_impls.iter().map(|&imp| (imp, impl_key(db, imp, false))));
    }
    out
}

/// Every trait declared in `krate`'s modules with its item names.
pub(super) fn trait_names(db: &Db, krate: Crate) -> HashMap<TraitId, SmallVec<[Name; 8]>> {
    let mut out = HashMap::default();
    for trait_ in declared_traits(db, krate) {
        let mut names = SmallVec::new();
        assoc_names(db, trait_, &mut names);
        out.insert(trait_, names);
    }
    out
}
