//! Item identities (docs/v2/plan/design/item-hash.md): the a-priori hash a body's inference, MIR
//! and borrow check are filed under in blaze's store, and the rest of what
//! `hir_ty::body_store` asks above hir-ty.
//!
//! - [`identity`]: a body's identity (section 3): its crate's inputs, its owner's interface, its
//!   own HIR as `hir_def::item_hash` writes it with no spans, each id of it by what it is in
//!   every database, what each of its paths resolves to, the traits in scope and the module
//!   chain. One tracked query per body, with early cutoff.
//! - [`reads`]: what each read of a step stands for (section 6): covered by the identity, one
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

/// Lane item-hash's side of `hir_ty::body_store`.
pub struct Bodies;

impl BodyFacts for Bodies {
    fn identity(&self, db: &Db, def: DefWithBodyId) -> Option<ContentId> {
        identity::identity(db, def).ok()
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

    fn value(&self, db: &Db, fact: FactRef) -> Option<ContentId> {
        values::value(db, fact)
    }

    fn def_key<'db>(&self, db: &'db Db, body: DefWithBodyId, def: SolverDefId<'db>) -> ContentId {
        keys::def_key(db, body, def)
    }

    fn def_of<'db>(&self, db: &'db Db, body: DefWithBodyId, key: ContentId) -> Option<SolverDefId<'db>> {
        keys::def_of(db, body, key)
    }
}
