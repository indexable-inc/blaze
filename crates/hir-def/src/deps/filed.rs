//! Values filed by what a crate read, as an image is ([`super::serve`]): the names it read in
//! the first free of the slots under a key ([`reads::slot`]), and the value under the key,
//! those names and the values they had ([`reads::image_key`]). An image is filed so, and so are
//! values made from a crate's bodies (`blaze_diagnostics::cache`, `hir_ty::check_cache`), whose
//! reads ([`checked_reads`]) are the expansions of its bodies besides those its image covers.

use base_db::{Crate, Db};
use blaze_ids::ContentId;
use hir_expand::MacroCallId;

use crate::{
    deps::{
        mode,
        reads::{self, Reads, SLOTS},
        serve::{ImageStore, Refused, crate_name},
        signatures,
    },
    expr_store::{Body, ExpressionStore},
    item_scope::ItemScope,
    nameres::{crate_def_map, demand::scope_body_owners},
};

/// The reads of `krate` as `db` checked it: [`Reads::of`] with, besides the calls its
/// signatures name, every macro call of each of its bodies, and of the signatures and bodies of
/// the items of every block in them. Run after the check, so the `env!` log has the bodies'
/// variables. An expansion whose reads are not known is a refusal, as for images.
///
/// A crate read from an image leaves out the calls of its modules and its signatures: the
/// image's key ([`super::Image::key`]) covers what they read, and asking would expand the
/// macros serving skips. So the key a value of such a crate is filed under ([`file_by_reads`])
/// must cover its image.
pub fn checked_reads(db: &Db, krate: Crate) -> Result<Reads, Refused> {
    let imaged = mode(db, krate).is_some();
    // alloc: setup: the calls of one crate, walked once as a check of it is kept
    let mut walk = Walk { db, calls: Vec::new(), scopes: Vec::new() };
    for (_, module) in crate_def_map(db, krate).modules(db) {
        walk.scope(module.scope(db), !imaged);
    }
    while let Some(scope) = walk.scopes.pop() {
        walk.scope(scope, true);
    }
    Reads::of_calls(db, krate, walk.calls)
}

/// The macro calls of a crate's bodies and signatures, and the scopes of the blocks in them
/// still to walk.
struct Walk<'db> {
    db: &'db Db,
    calls: Vec<MacroCallId>,
    scopes: Vec<&'db ItemScope>,
}

impl<'db> Walk<'db> {
    /// The calls of the bodies of `scope`'s items and, with `named`, of the scope and of its
    /// items' signatures. The blocks of the signatures are walked either way: what they declare
    /// is not in an image.
    fn scope(&mut self, scope: &'db ItemScope, named: bool) {
        if named {
            self.calls.extend(scope.all_macro_calls());
        }
        // alloc: setup: the items of one scope, once per check kept
        let mut items = Vec::new();
        signatures::scope_items(self.db, scope, &mut items);
        for item in items {
            if let Some(store) = signatures::store_of(self.db, item) {
                self.store(store, named);
            }
        }
        for owner in scope_body_owners(self.db, scope) {
            self.store(&Body::of(self.db, owner).store, true);
        }
    }

    /// The calls of `store` when `calls`, and the scopes of its blocks.
    fn store(&mut self, store: &'db ExpressionStore, calls: bool) {
        if calls {
            self.calls.extend_from_slice(store.macro_calls());
        }
        let db = self.db;
        for (_, def_map) in store.blocks(db) {
            self.scopes.extend(def_map.modules(db).map(|(_, module)| module.scope(db)));
        }
    }
}

/// Files `reads` (of `krate`, made in `db`) under `key`: the key itself when they are empty,
/// else the first slot of `key` that holds them or is free ([`keep_reads`]), and the key a
/// value made with them goes under ([`reads::image_key`] of their values). `Ok(Err(..))` when
/// every slot holds other reads.
pub fn file_by_reads(
    db: &Db,
    krate: Crate,
    key: ContentId,
    reads: &Reads,
    // dyn: the driver's store, one call per crate kept
    store: &dyn ImageStore,
) -> Result<Result<ContentId, Refused>, String> {
    if reads.is_empty() {
        return Ok(Ok(key));
    }
    let record = reads.encode();
    if let Some(why) = keep_reads(db, krate, key, &record, store)? {
        return Ok(Err(why));
    }
    Ok(Ok(reads::image_key(key, &record, reads.values_then(db))))
}

/// The first of `key`, then the filed key of each slot of `key` with its reads' values now, for
/// which `found` gives `Some`: as [`super::serve`] looks for an image. `None` when none does. A
/// slot that does not decode is an error naming the crate.
pub fn find_by_reads<T>(
    db: &Db,
    krate: Crate,
    key: ContentId,
    // dyn: the driver's store, one call per crate per session
    store: &dyn ImageStore,
    found: impl FnMut(ContentId) -> Result<Option<T>, String>,
) -> Result<Option<T>, String> {
    Ok(search(db, krate, key, store, found)?.ok())
}

/// [`find_by_reads`], and when nothing is found whether a slot of `key` holds reads: `Err(true)`
/// when something they read changed.
pub(super) fn search<T>(
    db: &Db,
    krate: Crate,
    key: ContentId,
    // dyn: the driver's store, one call per crate per session
    store: &dyn ImageStore,
    mut found: impl FnMut(ContentId) -> Result<Option<T>, String>,
) -> Result<Result<T, bool>, String> {
    if let Some(it) = found(key)? {
        return Ok(Ok(it));
    }
    let name = || crate_name(db, krate);
    let mut kept = false;
    for n in 0..SLOTS {
        let at = reads::slot(key, n);
        let record = store.get(&at).map_err(|why| {
            // alloc: error text
            format!("reading the reads of crate {} (key {at:?}): {why}", name())
        })?;
        let Some(record) = record else { break };
        let reads = Reads::decode(record.bytes()).map_err(|why| {
            // alloc: error text
            format!("the reads of crate {} in slot {n} of key {key:?}: {why}", name())
        })?;
        let values = reads.values_now(db, krate, store).map_err(|why| {
            // alloc: error text
            format!("the values of the reads of crate {} in slot {n}: {why}", name())
        })?;
        if let Some(it) = found(reads::image_key(key, record.bytes(), values))? {
            return Ok(Ok(it));
        }
        kept = true;
    }
    Ok(Err(kept))
}

/// Keeps `record`, the reads of `krate`, in the first slot of `key` that holds it or is free.
/// Two writers that keep other reads in one slot at once each see the other's and move on.
fn keep_reads(
    db: &Db,
    krate: Crate,
    key: ContentId,
    record: &[u8],
    // dyn: the driver's store, one call per crate kept
    store: &dyn ImageStore,
) -> Result<Option<Refused>, String> {
    let error = |what: &str, why: String| {
        // alloc: error text
        format!("{what} the reads of crate {} (key {key:?}): {why}", crate_name(db, krate))
    };
    for n in 0..SLOTS {
        let at = reads::slot(key, n);
        match store.get(&at).map_err(|why| error("reading", why))? {
            Some(kept) if kept.bytes() == record => return Ok(None),
            Some(_) => continue,
            // alloc: the slot's bytes, once per crate kept
            None => match store.put(at, record.to_vec()) {
                Ok(()) => return Ok(None),
                Err(why) => match store.get(&at).map_err(|why| error("reading", why))? {
                    Some(kept) if kept.bytes() == record => return Ok(None),
                    Some(_) => continue,
                    None => return Err(error("keeping", why)),
                },
            },
        }
    }
    Ok(Some(Refused::Reads { why: "every slot of its key holds other reads" }))
}
