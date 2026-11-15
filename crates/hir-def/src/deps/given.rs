//! What a crate is read from, kept a crate (restore.md, "A slot a crate for what a crate is
//! read from"), on the session memo (docs/v2/plan/design/front-end-on-memo.md, rules 4 and 5).
//!
//! Each kind of record a crate is read from (its image, its frozen def map and lang items,
//! its def-map pages, hir-ty's impl tables, its surface) is given to the crate as one slot of
//! the crate's own arena ([`base_db::Edit::give_by`]), by the driver in an edit of the world,
//! and read inside a query from the crate's slot ([`base_db::Crate::given`]). Nothing is given
//! during a build: a snapshot's inputs are fixed for its life, and a change is the next
//! commit, which makes again the arenas of the crates whose slots moved.
//!
//! A slot is never an entry of one map of every crate: a map of every crate is an input every
//! crate's entries read, so a new image of one crate would drop every arena that read the map.
//!
//! The invariant: a crate's slot of a kind holds what the driver last gave it, or nothing; the
//! fact it is read by is the one the driver gave with it, so a read list names the slot's
//! digest and nothing of another crate's.

use base_db::{Crate, Db, Edit, Given};
use blaze_memo::Fact;

/// Gives `krate` (a crate of the edit's last commit) `value` as its slot of kind `K`, read by
/// `fact`: the one way a slot is given. A value equal to the one the slot holds gives
/// nothing, so the crate's arena moves only when what it is read from did; whether it was
/// given.
///
/// The fact is the giver's, not drawn per gift: two values a slot held under one fact are the
/// giver's promise that a reader of the slot alone sees no difference (each part read of it
/// notes a fact of its own, as an image's items do).
pub fn give<K: 'static, T: std::any::Any + Send + Sync + PartialEq>(
    edit: &mut Edit<'_>,
    krate: Crate,
    fact: Fact,
    value: T,
) -> bool {
    let db = edit.db();
    if krate.given::<K, T>(db) == Some(&value) {
        return false;
    }
    let key = krate.key(db);
    edit.give_by::<K>(key, fact, Given::new(value));
    true
}

/// [`give`] for a located kind ([`base_db::Located`]: a gift that says where a value can be
/// read, which every reader computes alike without it): kept in the snapshot's table, read by
/// [`Crate::located`], its first gift to a crate marking no arena. A value equal to the one
/// held gives nothing; whether it was given.
pub fn give_located<K: base_db::Located, T: std::any::Any + Send + Sync + PartialEq>(
    edit: &mut Edit<'_>,
    krate: Crate,
    fact: Fact,
    value: T,
) -> bool {
    let db = edit.db();
    if krate.located::<K, T>(db) == Some(&value) {
        return false;
    }
    let key = krate.key(db);
    edit.give_located::<K>(key, fact, Given::new(value));
    true
}

/// [`give`] of each crate of `entries` in `edit`: how many crates were given. For a driver
/// that gives one kind to many crates at once (`nameres::frozen`'s, hir-ty's impl tables).
pub fn give_each<K: 'static, T: std::any::Any + Send + Sync + PartialEq>(
    edit: &mut Edit<'_>,
    entries: impl IntoIterator<Item = (Crate, Fact, T)>,
) -> usize {
    let mut given = 0;
    for (krate, fact, value) in entries {
        given += usize::from(give::<K, T>(edit, krate, fact, value));
    }
    given
}

/// [`give_located`] of each crate of `entries` in `edit`: how many crates were given.
pub fn give_each_located<K: base_db::Located, T: std::any::Any + Send + Sync + PartialEq>(
    edit: &mut Edit<'_>,
    entries: impl IntoIterator<Item = (Crate, Fact, T)>,
) -> usize {
    let mut given = 0;
    for (krate, fact, value) in entries {
        given += usize::from(give_located::<K, T>(edit, krate, fact, value));
    }
    given
}

/// The kind of a crate's slot holding its *surface*: the digest of everything a crate above
/// it may read of it (cutoff-1112's, made by the lowering: `blaze_lower::Lowered::surface_of`),
/// which a dependent's image key takes in place of this crate's key (restore.md, "Restart
/// reads records (d)", E), so a body edit below moves no key above.
pub struct SurfaceSlot;

/// The fact a crate's surface slot is read by: the slot's id, of the crate's own stable id, and
/// the surface itself as its digest (a surface is a digest already).
pub fn surface_fact(krate_id: base_db::ContentId, surface: base_db::ContentId) -> Fact {
    Fact { id: base_db::ContentId::of("hir-def surface slot", &krate_id.0), digest: surface }
}

/// Gives `krate`, whose stable id is `krate_id`, its surface in `edit`, read by
/// [`surface_fact`]: whether it moved.
///
/// The invariant: a key that reads a dependency's surface ([`surface`]) reads the one the
/// lowering worked out from the files the snapshot holds; the driver gives it in the edit that
/// commits them, before any key is asked.
pub fn give_surface(
    edit: &mut Edit<'_>,
    krate: Crate,
    krate_id: base_db::ContentId,
    surface: base_db::ContentId,
) -> bool {
    give::<SurfaceSlot, base_db::ContentId>(edit, krate, surface_fact(krate_id, surface), surface)
}

/// `krate`'s surface, a read of its own slot; `None` when the lowering gave none (a front end
/// no driver keys: its crates have no image key either).
pub fn surface(db: &Db, krate: Crate) -> Option<base_db::ContentId> {
    krate.given::<SurfaceSlot, base_db::ContentId>(db).copied()
}
