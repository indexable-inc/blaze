//! The identities `hir_ty::method_resolution::delta`'s impl-set and candidate-set hashes name
//! impls and head defs by (docs/v2/plan/design/item-hash.md section 5), installed once per
//! database by [`install`]: what an item is, never where it is written, so moving an impl or
//! renaming the modules around it keeps every set hash that holds it.

use base_db::Db;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, hash::signature_hash, key};
use hir_def::{ImplId, Lookup as _, src::HasSource as _};
use hir_ty::method_resolution::delta::{HeadDef, install_identities};
use syntax::AstNode as _;

const IMPL_DOMAIN: &str = "blaze v2 impl identity 1";
const HEAD_DOMAIN: &str = "blaze v2 head identity 1";

/// Installs [`impl_identity`] and [`head_identity`] as `db`'s: before any set hash is asked of it.
pub fn install(db: &mut Db) {
    install_identities(db, impl_identity, head_identity);
}

/// An impl's identity: its crate and its signature hash (its header, generics, where clauses and
/// associated items' signatures and types, by the keys of what they name), so an edit a solver
/// reads changes it and a move does not. Two impls of one crate with one signature have one
/// identity: a solver tells them apart by nothing, and a set hash counts each it holds, so adding
/// or removing either changes it. An impl whose signature has no hash is of a program rustc
/// rejects (the build stops at it); it is by its key and its text, so any edit of it changes it.
pub fn impl_identity(db: &Db, imp: ImplId) -> ContentId {
    let item = ItemId::Impl(imp);
    let mut h = Hasher::new(IMPL_DOMAIN);
    h.id(&key::crate_key(db, item.krate(db)));
    match signature_hash(db, item) {
        Ok(signature) => {
            h.part(b"signature").id(&signature.0);
        }
        Err(why) => {
            h.part(b"unhashable").id(&key::item_key(db, item).0).part(why.what.as_bytes());
            imp.lookup(db).source(db).value.syntax().text().for_each_chunk(|chunk| {
                h.update(chunk.as_bytes());
            });
        }
    }
    h.finish()
}

/// A head def's identity (the ADT, trait or alias a row of impls is for): its item key, the same
/// in every database. Not its signature: two structs of one shape head two rows.
pub fn head_identity(db: &Db, def: HeadDef) -> ContentId {
    let item = match def {
        HeadDef::Adt(it) => ItemId::Adt(it),
        HeadDef::Trait(it) => ItemId::Trait(it),
        HeadDef::Alias(it) => ItemId::Alias(it),
    };
    Hasher::new(HEAD_DOMAIN).id(&key::item_key(db, item).0).finish()
}
