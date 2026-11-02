//! What the impl tables are read through (design §7): a crate's table of one trait or of one
//! self type is its slice's rows, which the maintainer keeps, merged by module with the impls of
//! the blocks in the bodies of the modules the slice lists, which stay pulled per module
//! ([`trait_bodies`], [`inherent_bodies`]). No reader goes over the crate's modules: which
//! modules' bodies hold impls of a key is the maintainer's ([`body_part`]).
//!
//! The order is today's, which [`TraitImpls::for_crate`] made by going over the modules in
//! `DefMap::modules` order, each module's own impls (scope order, which is position order
//! within a module) before its derives and those before its bodies' impls. A slice holds its
//! rows in position order; position order interleaves a module's impls with its child
//! modules', so readers sort them by module rank first. The rank comes from the module tree,
//! which a reader reads anyway, so moving a module reorders the tables exactly as before.

use base_db::{Crate, Db};
use blaze_dbsp::Pos;
use blaze_ids::{ContentId, Hasher};
use blaze_mem::Frame;
use either::Either;
use hir_def::{ImplId, ModuleId, TraitId, lang_item::lang_items, nameres::crate_def_map};

use super::{
    head::{Head, simplified, try_head},
    publish::{
        IMPL_SET_DOMAIN, InherentRows, TraitRows, derive_identity, directory, head_identity,
        impl_identity, inherent_slice, trait_slice,
    },
};
use crate::{
    method_resolution::{
        InherentImpls, InherentImplsBuilder, SelfTyKey, TraitImpls, TraitImplsBuilder,
        impl_holders, trace_holders, tracing_holders,
    },
    next_solver::SimplifiedType,
};

/// The trait impls of the blocks in the bodies `module` holds, as [`TraitImpls::for_module`]
/// adds them after the module's own.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub(in crate::method_resolution) fn trait_bodies<'db>(
    db: &'db Db,
    module: ModuleId,
) -> TraitImpls<'db> {
    blaze_mem::frame::current(|frame| {
        let lang_items = lang_items(db, module.krate(db));
        let mut builder = TraitImplsBuilder::new(frame);
        TraitImpls::collect_bodies(db, module, lang_items, &mut builder);
        builder.finish()
    })
}

/// The inherent impls of the blocks in the bodies `module` holds, as
/// [`InherentImpls::for_module`] adds them after the module's own.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub(in crate::method_resolution) fn inherent_bodies<'db>(
    db: &'db Db,
    module: ModuleId,
) -> InherentImpls<'db> {
    blaze_mem::frame::current(|frame| {
        let mut builder = InherentImplsBuilder::new(frame);
        InherentImpls::collect_bodies(db, module, &mut builder);
        builder.finish()
    })
}

/// What the bodies of `module` give the slices: the traits and the heads of the impls of their
/// blocks, each sorted, and whether a body of the module may hold an impl at all (it has impl
/// holders: an `impl`, a macro call or an attribute that may write one), which is what makes the
/// maintainer read it again after a write to another module.
// clone: a changed part, kept to diff the next write against; once per module whose part changed
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::method_resolution) struct BodyPart {
    pub(in crate::method_resolution) traits: Box<[TraitId]>,
    pub(in crate::method_resolution) heads: Box<[Head]>,
    pub(in crate::method_resolution) holds: bool,
}

/// [`BodyPart`] of `module`, a module of its crate's `DefMap`: equal unless a trait or head
/// of its bodies' impls comes or goes, or its bodies gain or lose their impl holders.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub(in crate::method_resolution) fn body_part(db: &Db, module: ModuleId) -> BodyPart {
    blaze_mem::frame::current(|frame| {
        let scope = module.def_map(db).module(db, module).scope(db);
        let holders = impl_holders(db, frame, module.krate(db), scope);
        if holders.is_empty() {
            return BodyPart::default();
        }
        if tracing_holders() {
            trace_holders(db, module, &holders);
        }
        let mut heads: Vec<Head, &Frame> = Vec::new_in(frame);
        heads.extend(inherent_bodies(db, module).iter().map(|(self_ty, _)| {
            try_head(self_ty).unwrap_or_else(|| {
                panic!("impls: an inherent impl in a body of {module:?} has the self type \
                        {self_ty:?}, which no impl header names")
            })
        }));
        heads.sort_unstable();
        heads.dedup();
        BodyPart {
            // clone: alloc: the memo's column, once at its length (none when empty)
            traits: trait_bodies(db, module).traits.clone(),
            // alloc: as above
            heads: heads.iter().copied().collect(),
            holds: true,
        }
    })
}

/// A part of a module's table, in the order a module adds its parts.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Part {
    Impl,
    Derive,
    Bodies,
}

/// The rank of `module` in `krate`'s `DefMap::modules` order.
fn ranker(db: &Db, krate: Crate) -> impl Fn(ModuleId) -> u32 + '_ {
    let def_map = crate_def_map(db, krate);
    move |module| {
        def_map.module_rank(db, module).unwrap_or_else(|| {
            panic!("impls: module {module:?} of a slice is not among the modules of {krate:?}")
        })
    }
}

/// Adds `krate`'s impls of `trait_` to `builder` in the order [`TraitImpls::for_crate`] holds
/// them: the slice's rows and the bodies' tables, by module.
pub(in crate::method_resolution) fn trait_impls_into<'db>(
    db: &'db Db,
    krate: Crate,
    trait_: TraitId,
    builder: &mut TraitImplsBuilder<'db, '_>,
) {
    let Some(slice) = trait_slice(db, krate, trait_) else { return };
    let TraitRows { impls, derives, bodies } = slice.rows(db);
    if impls.is_empty() && derives.is_empty() && bodies.is_empty() {
        return;
    }
    let rank = ranker(db, krate);
    let frame = builder.frame;
    // alloc: frame: the merge order, once at its length
    let mut order: Vec<(u32, Part, u32), &Frame> =
        Vec::with_capacity_in(impls.len() + derives.len() + bodies.len(), frame);
    order.extend(impls.iter().zip(0..).map(|(&(module, ..), i)| (rank(module), Part::Impl, i)));
    order.extend(derives.iter().zip(0..).map(|(&(module, ..), i)| (rank(module), Part::Derive, i)));
    order.extend(bodies.iter().zip(0..).map(|(&module, i)| (rank(module), Part::Bodies, i)));
    order.sort_unstable();
    let t = builder.trait_(trait_);
    for &(_, part, i) in order.iter() {
        let i = i as usize;
        match part {
            Part::Impl => {
                let (_, imp, head) = impls[i];
                match head {
                    Some(head) => {
                        let row = builder.row(t, simplified(head));
                        builder.push_impl(row, imp);
                    }
                    None => builder.push_blanket(t, imp),
                }
            }
            Part::Derive => {
                let (_, imp, head) = derives[i];
                let row = builder.row(t, simplified(head));
                builder.push_derive(row, imp);
            }
            Part::Bodies => trait_bodies(db, bodies[i]).merge_trait_into(trait_, builder),
        }
    }
}

/// `krate`'s trait impls of every trait, as [`TraitImpls::for_crate`] held them: each trait's
/// table merged. A trait's rows and blanket impls are its own, so merging trait by trait gives
/// the crate-wide traversal's table. Every trait with an impl, in a body or not, has a slice.
pub(in crate::method_resolution) fn trait_impls_of_crate<'db>(
    db: &'db Db,
    frame: &Frame,
    krate: Crate,
) -> TraitImpls<'db> {
    let sliced = directory(db, krate).traits(db);
    // alloc: frame: the traits, once at their count
    let mut traits: Vec<TraitId, &Frame> = Vec::with_capacity_in(sliced.len(), frame);
    traits.extend(sliced.keys().copied());
    traits.sort_unstable();
    let mut builder = TraitImplsBuilder::new(frame);
    for &trait_ in traits.iter() {
        TraitImpls::for_crate_and_trait(db, krate, trait_).merge_trait_into(trait_, &mut builder);
    }
    builder.finish()
}

/// `krate`'s inherent impls for one type, in the order [`InherentImpls::for_crate`] holds them.
pub(in crate::method_resolution) fn inherent_impls_for<'db>(
    db: &'db Db,
    krate: Crate,
    self_ty: SelfTyKey<'db>,
) -> Box<[ImplId]> {
    let slice = try_head(&self_ty.0).and_then(|head| inherent_slice(db, krate, head));
    let Some(slice) = slice else { return Box::default() };
    let InherentRows { impls: own, bodies } = slice.rows(db);
    if bodies.is_empty() && own.windows(2).all(|it| it[0].0 == it[1].0) {
        // alloc: the memo's value, once at its length (none when empty)
        return own.iter().map(|&(_, imp, _)| imp).collect();
    }
    blaze_mem::frame::current(|frame| {
        let rank = ranker(db, krate);
        // alloc: frame: the merge order, once at its length
        let mut order: Vec<(u32, Part, u32), &Frame> =
            Vec::with_capacity_in(own.len() + bodies.len(), frame);
        order.extend(own.iter().zip(0..).map(|(&(module, ..), i)| (rank(module), Part::Impl, i)));
        order.extend(bodies.iter().zip(0..).map(|(&module, i)| (rank(module), Part::Bodies, i)));
        order.sort_unstable();
        let mut found = Vec::new_in(frame);
        for &(_, part, i) in order.iter() {
            let i = i as usize;
            match part {
                Part::Bodies => found.extend_from_slice(
                    inherent_bodies(db, bodies[i]).for_self_ty(&self_ty.0),
                ),
                _ => found.push(own[i].1),
            }
        }
        // alloc: the memo's value, once at its length
        found.iter().copied().collect()
    })
}

/// `krate`'s inherent impls, as [`InherentImpls::for_crate`] held them: every slice's rows and
/// every module's bodies, by module, a module's own impls in position order. The self types
/// are numbered as first seen, which within one module is position order across slices.
pub(in crate::method_resolution) fn inherent_impls_of_crate<'db>(
    db: &'db Db,
    frame: &Frame,
    krate: Crate,
) -> InherentImpls<'db> {
    enum Source {
        Own(Head, ImplId),
        Bodies(ModuleId),
    }
    let directory = directory(db, krate);
    let heads = directory.heads(db);
    let bodies = directory.body_modules(db);
    let rank = ranker(db, krate);
    let root = Pos::root();
    // alloc: frame: the merge order, once at the crate's impl count
    let mut order: Vec<(u32, Part, &Pos, Source), &Frame> = Vec::new_in(frame);
    for (&head, slice) in heads.iter() {
        for (module, imp, pos) in slice.rows(db).impls.iter() {
            order.push((rank(*module), Part::Impl, pos, Source::Own(head, *imp)));
        }
    }
    for &module in bodies.iter() {
        order.push((rank(module), Part::Bodies, &root, Source::Bodies(module)));
    }
    order.sort_unstable_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
    let mut builder = InherentImplsBuilder::new(frame);
    for (_, _, _, source) in order.iter() {
        match *source {
            Source::Own(head, imp) => {
                let group = builder.group(simplified(head));
                builder.push(group, imp);
            }
            Source::Bodies(module) => inherent_bodies(db, module).merge_into(&mut builder),
        }
    }
    builder.finish()
}

/// The hash of `krate`'s body-block impls of `trait_` (for [`super::impl_set_hash`]): the
/// tables of the body modules its slice lists, in module order, with each impl's identity and
/// head.
#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub(in crate::method_resolution) fn body_impl_set_hash(
    db: &Db,
    krate: Crate,
    trait_: TraitId,
) -> ContentId {
    let mut hasher = Hasher::new(IMPL_SET_DOMAIN);
    hasher.part(b"bodies");
    let bodies = match trait_slice(db, krate, trait_) {
        Some(slice) => &*slice.rows(db).bodies,
        None => &[][..],
    };
    if bodies.is_empty() {
        return hasher.finish();
    }
    let rank = ranker(db, krate);
    blaze_mem::frame::current(|frame| {
        // alloc: frame: the modules in rank order, once at their count
        let mut order: Vec<(u32, ModuleId), &Frame> = Vec::with_capacity_in(bodies.len(), frame);
        order.extend(bodies.iter().map(|&module| (rank(module), module)));
        order.sort_unstable();
        for &(_, module) in order.iter() {
            hash_table_trait(db, &mut hasher, trait_bodies(db, module), trait_, None);
        }
    });
    hasher.finish()
}

/// The hash of the impls of `trait_` for one self type in `krate`, derives and blanket impls
/// left out: what a result that read one row keys on.
#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub fn impl_row_hash<'db>(
    db: &'db Db,
    krate: Crate,
    trait_: TraitId,
    self_ty: SelfTyKey<'db>,
) -> ContentId {
    let mut hasher = Hasher::new(IMPL_SET_DOMAIN);
    hasher.part(b"row");
    let table = TraitImpls::for_crate_and_trait(db, krate, trait_);
    hash_table_trait(db, &mut hasher, table, trait_, Some(Either::Right(&self_ty.0)));
    hasher.finish()
}

/// The hash of the blanket impls of `trait_` in `krate`.
#[blaze_query::tracked(returns(copy), mem_tag = Infer)]
pub fn blanket_hash(db: &Db, krate: Crate, trait_: TraitId) -> ContentId {
    let mut hasher = Hasher::new(IMPL_SET_DOMAIN);
    hasher.part(b"blanket");
    let table = TraitImpls::for_crate_and_trait(db, krate, trait_);
    hash_table_trait(db, &mut hasher, table, trait_, Some(Either::Left(())));
    hasher.finish()
}

/// Hashes `table`'s impls of `trait_` into `hasher`: all of them (`None`), its blanket impls
/// (`Left`) or its row for one self type (`Right`).
fn hash_table_trait<'db>(
    db: &'db Db,
    hasher: &mut Hasher,
    table: &TraitImpls<'db>,
    trait_: TraitId,
    only: Option<Either<(), &SimplifiedType<'db>>>,
) {
    let Some(t) = table.trait_index(trait_) else { return };
    // Impls and rows go in by their identities' order: the table orders them by interned ids,
    // which two databases give out in different orders.
    let identities = |impls: &[ImplId]| {
        // alloc: the identities of one row's impls, once per row hashed
        let mut ids: Vec<ContentId> = impls.iter().map(|&imp| impl_identity(db, imp)).collect();
        ids.sort_unstable();
        ids
    };
    if !matches!(only, Some(Either::Right(_))) {
        let blanket = identities(table.blanket.run(t));
        hasher.update(&(blanket.len() as u64).to_le_bytes());
        for id in &blanket {
            hasher.id(id);
        }
    }
    if matches!(only, Some(Either::Left(()))) {
        return;
    }
    let mut rows = Vec::new(); // alloc: one hash per row, once per table hashed
    for (self_ty, (impls, derives)) in table.rows_at(t) {
        if let Some(Either::Right(only)) = only
            && only != self_ty
        {
            continue;
        }
        let Some(head) = try_head(self_ty) else {
            panic!(
                "impls: a row of {trait_:?} has the self type {self_ty:?}, which no impl header \
                 names"
            )
        };
        let mut row = Hasher::new(IMPL_SET_DOMAIN);
        head_identity(db, &mut row, head);
        let ids = identities(impls);
        row.update(&(ids.len() as u64).to_le_bytes());
        for id in &ids {
            row.id(id);
        }
        if only.is_none() {
            row.update(&(derives.len() as u64).to_le_bytes());
            for &imp in derives {
                derive_identity(db, &mut row, imp);
            }
        }
        rows.push(row.finish());
    }
    rows.sort_unstable();
    hasher.update(&(rows.len() as u64).to_le_bytes());
    for row in &rows {
        hasher.id(row);
    }
}
