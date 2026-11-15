//! What a crate is read from, kept a crate (restore.md, "A slot a crate for what a crate is
//! read from").
//!
//! Each kind of record a crate is read from (its image, its frozen def map and lang items,
//! its def-map pages, hir-ty's impl tables) is set as one map of every crate, which the code
//! outside queries reads: the serve that adds to it, the withdrawals, the maintainers' passes.
//! A read there records no edge. A row that read the map would depend on every crate's entry,
//! so a serve of one crate would be a write every served crate's rows verify after, and no
//! seal group could own the row. So the write that sets a map also gives each crate whose
//! entry moved its own entry ([`give_changed`], through [`base_db::Crate::give`]), and every
//! read inside a query is of the crate's slot ([`base_db::Crate::given`]).
//!
//! The invariant: after a map's `set`, a crate's slot of that kind holds the crate's entry of
//! the map, or nothing when the map has none; both are written in the one write that calls
//! `set`, so no view has one without the other.

use base_db::{Crate, Db, Given};
use blaze_hash::HashMap;
use blaze_query::db::Durability;
use std::alloc::Global;

#[cfg(test)]
thread_local! {
    /// The bug a test planted: every crate of the map is given its entry again, moved or not
    /// (what one map of every crate did to its readers).
    pub(crate) static GIVES_EVERY_CRATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Gives each crate whose entry differs between `was` and `now` its entry of `now` as kind
/// `K`, or nothing when `now` has none: how many crates were given. A crate whose entry is
/// equal in both is not written, so no row that read its slot is verified again. Crates are
/// given in their order, so the slots a first gift makes are numbered alike by every database
/// that sets the same maps.
///
/// Called by the `set` of each map of what crates are read from, in the write that sets the
/// map, before the map is set.
pub fn give_changed<K: 'static, T>(
    db: &mut Db,
    was: Option<&HashMap<Crate, T, Global>>,
    now: &HashMap<Crate, T, Global>,
) -> usize
where
    T: PartialEq + Clone + Send + Sync + 'static,
{
    #[cfg(test)]
    let every = GIVES_EVERY_CRATE.get();
    #[cfg(not(test))]
    let every = false;
    // alloc: the crates whose entry moved, once per write that sets a map of them
    let mut moved: Vec<(Crate, Option<&T>)> = Vec::new();
    for (krate, entry) in now {
        if every || was.and_then(|it| it.get(krate)) != Some(entry) {
            moved.push((*krate, Some(entry)));
        }
    }
    if let Some(was) = was {
        for krate in was.keys() {
            if !now.contains_key(krate) {
                moved.push((*krate, None));
            }
        }
    }
    moved.sort_unstable_by_key(|it| it.0);
    let given = moved.len();
    for (krate, entry) in moved {
        let value = match entry {
            // clone: the entry's handles (a count of a buffer, a key), once per crate given
            Some(entry) => Given::new(entry.clone()),
            None => Given::default(),
        };
        krate.give::<K>(db, value, Durability::HIGH);
    }
    given
}

/// The kind of a crate's slot holding its *surface*: the digest of everything a crate above
/// it may read of it (cutoff-1112's, made by the lowering: `blaze_lower::Lowered::surface_of`),
/// which a dependent's image key takes in place of this crate's key (restore.md, "Restart
/// reads records (d)", E), so a body edit below moves no key above.
pub struct SurfaceSlot;

/// Gives `krate` its surface, in a write: nothing is written when it holds that one already,
/// so no key that read it is made again. `durability` is the crate's own (a library's `HIGH`,
/// a workspace member's `LOW`: its surface moves with a signature edit).
///
/// The invariant: a key that reads a dependency's surface ([`surface`]) reads the one the
/// lowering worked out from the files the database holds at that view; the lowering gives it
/// in the write that changes those files, before any key is asked.
pub fn set_surface(db: &mut Db, krate: Crate, surface: base_db::ContentId, durability: Durability) {
    if krate.given::<SurfaceSlot, base_db::ContentId>(db) == Some(&surface) {
        return;
    }
    krate.give::<SurfaceSlot>(db, Given::new(surface), durability);
}

/// `krate`'s surface, a tracked read of its own slot; `None` when the lowering gave none (a
/// front end no driver keys: its crates have no image key either).
pub fn surface(db: &Db, krate: Crate) -> Option<base_db::ContentId> {
    krate.given::<SurfaceSlot, base_db::ContentId>(db).copied()
}
