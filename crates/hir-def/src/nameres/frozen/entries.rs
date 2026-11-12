//! Where per-item entries are kept (AGENTS.md rule 65): one value an item's answer, under a key
//! made of the item's own content and what the answer read; never a crate's key.
//!
//! [`super::segments`] is how an entry's bytes are written and read; this is where they are
//! kept. Every records lane goes through [`entry`] and [`keep`], so the storage form (a store
//! value an entry, entries packed by family) changes behind [`Entries`] alone.
//!
//! The invariant: equal keys are equal bytes. A key covers every input of its entry, so a
//! `put` under a key the store has is the same bytes and may be dropped.

use std::{fmt::Write as _, sync::Arc};

use base_db::Db;
use blaze_core::lock::Lock;
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use std::alloc::Global;

type R<T = ()> = Result<T, String>;

pub(crate) mod items;
#[cfg(test)]
mod tests;

/// The driver's holder of entries: the store, through its pool.
pub trait Entries: Send + Sync + 'static {
    /// Calls `with` once with the bytes kept under `key`, held only while it runs, and says
    /// `true`; `false`, with no call, when nothing is kept under it. `Err` when bytes are kept
    /// and cannot be read.
    ///
    /// `set` is where the entry was packed when it was kept (a file's content and its crate's
    /// settings: what most entries of one file share), a hint of where to look first and
    /// never part of what the entry is: an entry is found by `key` under any `set`, so a set
    /// that moved (its file was edited) still finds the entries of the items that did not.
    // dyn: one call per entry read
    fn get(&self, set: &ContentId, key: &ContentId, with: &mut dyn FnMut(&[u8])) -> R<bool>;

    /// Keeps `bytes` under `key`, packed with the entries of `set`.
    fn put(&self, set: &ContentId, key: ContentId, bytes: &[u8]) -> R;
}

/// Where this database's entries are kept, set once by its driver before any crate is served
/// from them. One value a database, never one a crate: a row that reads it depends on no
/// other crate's records.
#[blaze_query::input(singleton, debug)]
pub struct EntryStore {
    #[returns(ref)]
    pub held: Holder,
}

/// An [`Entries`] as an input's value: equal only to itself.
#[derive(Clone)]
// dyn: the driver's one holder
pub struct Holder(pub Arc<dyn Entries>);

impl PartialEq for Holder {
    fn eq(&self, other: &Holder) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Holder {}

impl std::fmt::Debug for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Entries")
    }
}

/// Has `db` keep and find its entries in `entries`: those of this crate's families, and
/// hir-expand's macro definitions (`hir_expand::declarative::stored`), which reach the same
/// store through [`ForExpansion`].
// dyn: the driver's one holder
pub fn set(db: &mut Db, entries: Arc<dyn Entries>) {
    use blaze_query::db::{Durability, Setter};
    // alloc: the one bridge of a database, when its driver sets its entry store
    hir_expand::declarative::stored::set(db, Arc::new(ForExpansion));
    match EntryStore::try_get(&*db) {
        Some(it) => {
            it.set_held(db).with_durability(Durability::HIGH).to(Holder(entries));
        }
        None => {
            _ = EntryStore::new(db, Durability::HIGH, Holder(entries));
        }
    }
}

/// Where the entries of `db` are kept, if its driver said.
// dyn: the driver's one holder
pub fn of(db: &Db) -> Option<&dyn Entries> {
    EntryStore::try_get(db).map(|it| &*it.held(db).0)
}

/// The entry of `family` under `key`, read by `decode`: `Ok(None)` when none is kept (the
/// caller says whether that is an error), `Err` naming the family and the key when one is
/// kept and does not read, or when the database has nowhere to look. Tells the engine the
/// value is a record's (`row::STORED`).
pub fn entry<T>(
    db: &Db,
    family: &'static str,
    set: &ContentId,
    key: ContentId,
    decode: impl FnOnce(&[u8]) -> R<T>,
) -> R<Option<T>> {
    // alloc: error path
    let entries = of(db).ok_or_else(|| format!("{family} entry {key:?}: no entry store was set"))?;
    let mut decode = Some(decode);
    let mut out = None;
    let found = entries
        .get(set, &key, &mut |bytes| {
            if let Some(decode) = decode.take() {
                out = Some(decode(bytes));
            }
        })
        // alloc: error path
        .map_err(|why| format!("{family} entry {key:?}: {why}"))?;
    match (found, out) {
        (false, _) => Ok(None),
        (true, Some(Ok(it))) => {
            blaze_query::db::report_stored_read(db);
            Ok(Some(it))
        }
        // alloc: error path
        (true, Some(Err(why))) => Err(format!("{family} entry {key:?} does not read: {why}")),
        // alloc: error path
        (true, None) => Err(format!("{family} entry {key:?}: said kept, and nothing was read")),
    }
}

/// Keeps `bytes` as the entry of `family` under `key`, packed with the entries of `set`.
pub fn keep(db: &Db, family: &'static str, set: &ContentId, key: ContentId, bytes: &[u8]) -> R {
    // alloc: error path
    let entries = of(db).ok_or_else(|| format!("{family} entry {key:?}: no entry store was set"))?;
    // alloc: error path
    entries.put(set, key, bytes).map_err(|why| format!("keeping {family} entry {key:?}: {why}"))
}

/// How many bytes of each value [`differ`] shows from the first that differs.
const SHOWN: usize = 16;

/// How two values under one key differ, for the refusal of the second (rules 19 and 21): their
/// lengths, the first byte that differs, and [`SHOWN`] bytes of each from there. Every holder
/// of entries says its refusal with this, so the next key that misses an input is read from
/// the message: which value is longer, and how far in they part.
pub fn differ(kept: &[u8], given: &[u8]) -> String {
    let at = kept
        .iter()
        .zip(given)
        .position(|(kept, given)| kept != given)
        .unwrap_or(kept.len().min(given.len()));
    let shown = |bytes: &[u8]| {
        // alloc: error path
        let mut text = String::new();
        for byte in &bytes[at..bytes.len().min(at + SHOWN)] {
            _ = write!(text, "{byte:02x}");
        }
        if text.is_empty() {
            text.push_str("its end");
        }
        text
    };
    // alloc: error path
    format!(
        "the value kept is {} bytes and the one given {}, and they first differ at byte {at}, \
         where the kept one has {} and the given one {}",
        kept.len(),
        given.len(),
        shown(kept),
        shown(given)
    )
}

/// hir-expand's way to the entry store: it is below this crate and keeps a macro
/// definition's tokens by the same [`entry`] and [`keep`] as every other family. An entry
/// that does not read, or a store that refuses a put, is [`super::Unreadable`] naming the
/// defining crate, the family and the key.
struct ForExpansion;

impl hir_expand::declarative::stored::Kept for ForExpansion {
    // dyn: as `Entries::get`
    fn get(
        &self,
        db: &Db,
        krate: base_db::Crate,
        family: &'static str,
        set: &ContentId,
        key: ContentId,
        with: &mut dyn FnMut(&[u8]) -> R,
    ) -> bool {
        match entry(db, family, set, key, |bytes| with(bytes)) {
            Ok(found) => found.is_some(),
            Err(why) => super::Unreadable::of(db, krate, family, why).throw(),
        }
    }

    fn put(
        &self,
        db: &Db,
        krate: base_db::Crate,
        family: &'static str,
        set: &ContentId,
        key: ContentId,
        bytes: &[u8],
    ) {
        if let Err(why) = keep(db, family, set, key, bytes) {
            super::Unreadable::of(db, krate, family, why).throw()
        }
    }
}

/// Entries in memory, counting what is asked of them: a test's, in this crate and above it
/// (the count tests of rule 65: edit one item of N, N-1 hits).
pub struct Memory {
    held: Lock<HashMap<ContentId, Vec<u8>, Global>>,
    counts: Lock<Counts>,
    /// Whether a get finds what was put ([`Memory::new`]) or nothing
    /// ([`Memory::within_one_build`]).
    finds: bool,
}

/// What was asked of a [`Memory`] since [`Memory::take`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub hits: u64,
    pub misses: u64,
    /// Puts under a key not held before.
    pub made: u64,
    /// Puts under a key already held.
    pub again: u64,
}

impl Memory {
    pub fn new() -> Arc<Memory> {
        // alloc: a test's store
        Arc::new(Memory {
            held: Lock::new("entries.memory.held", HashMap::new_in(Global)),
            counts: Lock::new("entries.memory.counts", Counts::default()),
            finds: true,
        })
    }

    /// Entries as the driver holds them while one build runs: a get finds nothing of what
    /// that build put (its puts are written at its end), so two items of one key each make
    /// their value and each put it, and the second put is checked against the first. A store
    /// that serves a put back at once never sees that second put: it hands the first item's
    /// value to the second item.
    pub fn within_one_build() -> Arc<Memory> {
        // alloc: a test's store
        Arc::new(Memory {
            held: Lock::new("entries.memory.held", HashMap::new_in(Global)),
            counts: Lock::new("entries.memory.counts", Counts::default()),
            finds: false,
        })
    }

    /// The counts since the last call.
    pub fn take(&self) -> Counts {
        std::mem::take(&mut *self.counts.lock())
    }
}

impl Entries for Memory {
    // dyn: as `Entries::get`
    fn get(&self, _set: &ContentId, key: &ContentId, with: &mut dyn FnMut(&[u8])) -> R<bool> {
        // clone: a test's copy, so no lock is held while `with` runs
        let found = self.held.lock().get(key).filter(|_| self.finds).cloned();
        let mut counts = self.counts.lock();
        match found {
            Some(bytes) => {
                counts.hits += 1;
                drop(counts);
                with(&bytes);
                Ok(true)
            }
            None => {
                counts.misses += 1;
                Ok(false)
            }
        }
    }

    fn put(&self, _set: &ContentId, key: ContentId, bytes: &[u8]) -> R {
        let mut held = self.held.lock();
        let mut counts = self.counts.lock();
        match held.get(&key) {
            Some(was) if was.as_slice() == bytes => counts.again += 1,
            // alloc: error path
            Some(was) => {
                return Err(format!("two values under key {key:?} (rule 21): {}", differ(was, bytes)));
            }
            None => {
                counts.made += 1;
                // alloc: a test's copy
                held.insert(key, bytes.to_vec());
            }
        }
        Ok(())
    }
}
