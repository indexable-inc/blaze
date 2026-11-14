//! Item identities (docs/v2/plan/design/item-hash.md): the a-priori hash a body's inference, MIR
//! and borrow check are filed under in blaze's store, and the rest of what
//! `hir_ty::body_store` asks above hir-ty.
//!
//! - [`identity`]: a body's key, made before the body is lowered
//!   (docs/v2/plan/design/end-state/engine.md 4.2, the `Fn` record; restore.md, "The entry's
//!   key, made before lowering"): its crate's settings, its owner by key, its module chain and
//!   its item's own text. Nothing it resolves is in it: what a step resolves is a fact of the
//!   step's shape, negatives included. One tracked query per body, with early cutoff.
//! - [`reads`]: what each read of a step stands for (section 6): covered by the key, one
//!   fact, or refused.
//! - [`values`]: each fact's value now (section 5), lane items' and this crate's own.
//! - [`keys`]: the item key of a definition a record names, and back.
//! - [`impls`]: the identities of impls and head defs that the impl-set hashes name them by,
//!   installed once per database ([`impls::install`]).
//! - [`presentation`](mod@presentation): the names and positions of a body that reach the output (section 3.7),
//!   hashed apart from its identity, for codegen to key debuginfo, symbols and locations by.
//!
//! [`Bodies`] is what the driver gives `hir_ty::body_store::install`.

#![forbid(unsafe_code)]
#![feature(allocator_api, register_tool)]
// `#[blaze::renders_positions]` and `#[expect(blaze::reads_positions)]`; inert under rustc.
#![register_tool(blaze)]

pub mod identity;
pub mod impls;
pub mod keys;
pub mod presentation;
pub mod reads;
pub mod values;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use base_db::Db;
use blaze_ids::ContentId;
use hir_def::DefWithBodyId;
use hir_ty::{
    body_store::{BodyFacts, Classified, Step},
    next_solver::{SolverDefId, goal_store::FactRef},
};

/// Lane item-hash's side of `hir_ty::body_store`: a body's steps are filed under its key made
/// before lowering ([`identity::early_identity`]) and every name its steps look up is a fact
/// of their shape ([`reads::classify`]). The identity made from the lowered, resolved body
/// and the switch between the two (`RecordedNames`, `reads::Names`) are deleted
/// (end-state/engine.md section 5); `.clippy.toml` bans their names.
pub struct Bodies;

impl BodyFacts for Bodies {
    fn identity(&self, db: &Db, def: DefWithBodyId) -> Option<ContentId> {
        identity::early_identity(db, def).ok()
    }

    fn classify(
        &self,
        db: &Db,
        def: DefWithBodyId,
        step: Step,
        read: blaze_query::db::DatabaseKeyIndex,
        kind: blaze_query::db::ReadKind,
        name: &'static str,
    ) -> Classified {
        reads::classify(db, def, step, read, kind, name)
    }

    fn value(&self, db: &Db, def: DefWithBodyId, fact: FactRef) -> Option<ContentId> {
        values::value_in(db, def, fact)
    }

    fn sealed(
        &self,
        db: &Db,
        def: DefWithBodyId,
        group: blaze_query::db::SealGroup,
        facts: &mut Vec<FactRef>,
    ) -> bool {
        reads::sealed(db, def, group, facts)
    }

    fn def_key<'db>(&self, db: &'db Db, body: DefWithBodyId, def: SolverDefId<'db>) -> ContentId {
        keys::def_key(db, body, def)
    }

    fn def_of<'db>(
        &self,
        db: &'db Db,
        body: DefWithBodyId,
        key: ContentId,
    ) -> Option<SolverDefId<'db>> {
        keys::def_of(db, body, key)
    }
}
