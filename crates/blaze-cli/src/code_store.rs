//! Code records kept across commands: blaze-ladder's `CodeStore` over the ONE store a user, the
//! plain store the command already opens at `<home>/.blaze/v2/store` (store.rs,
//! `open_for_command`), so an entry made by one `blaze run` is found by the next one by its key,
//! read in place, and never emitted again (AGENTS.md, "The design": everything kept is a record,
//! keyed from text before the work, written once, read in place; a command answers and exits).
//!
//! What a get and a put are: `get(key)` is one probe of the store's lent records (this command's
//! own puts, then the committed segments, each mapped at its first probe); the bytes borrow the
//! [`Lent`], never this handle. `put(key, entry)` copies the entry once into the `Lent`'s arena;
//! it reaches the disk in the ONE commit after the answer is printed
//! (store/plain_place.rs `after_answer`), or earlier between instances when the arena passes
//! `COMMIT_BYTES`, as a drive record's put does. Equal bytes under a held key are no write
//! (rule 25); other bytes are the store's `TwoValues`, answered as
//! `StoreError::TwoValues`: the alarm that the key missed an input (rule 21), never overwritten.
//!
//! Every entry is filed under [`CODE_TAG`], apart from the drive's records (`DRIVE_TAG`), and in
//! `Namespace::LOCAL` only: an entry is the machine code of the program's own bodies, which never
//! leaves the machine unless an organisation opts in (the code review's "Trust" rule). A
//! toolchain-only entry made by a trusted builder for the shared namespace is later work.
//!
//! The key holds the code builder's identity, the edition and the emitting tier, and an
//! instance's entry carries its read list (blaze-ladder code_reads.rs), checked when the entry is
//! found. An entry is put under its CONTENT key, and its code key is a POINTER to it
//! (`Lent::point`, the plain store's `POINTER_TAG` record: the newest commit's value wins, a
//! stale entry's rebuild replaces it), committed with the entry it names.

use std::alloc::Global;

use blaze_cas::Key as CasKey;
use blaze_cas::plain::{Lent, LentError, Namespace, Tag};
use blaze_core::Driver;
use blaze_ladder::code_records::{CodeKey, CodeStore, StoreError};

use crate::store::plain_place::COMMIT_BYTES;

/// The tag every code entry is filed under in the plain store's index: its own number, apart
/// from the store's roots (`u16::MAX`), the startup image (`u16::MAX - 1`) and the drive's
/// records (`DRIVE_TAG`, `u16::MAX - 2`). A record under a code key with another tag is refused
/// by name, never read as code.
pub(crate) const CODE_TAG: Tag = Tag(u16::MAX - 3);

/// The code index over the command's store (the module doc). `'m` is the borrow of the
/// command's [`Lent`]; every byte a get returns lives as long.
pub(crate) struct HomeCodeStore<'m, D: Driver> {
    lent: &'m Lent<D, Global>,
    /// Commits made on the command's path (the arena past `COMMIT_BYTES`).
    #[allow(dead_code, reason = "read by the tests; the profile row that shows it is owed")]
    pub commits: u32,
}

impl<'m, D: Driver> HomeCodeStore<'m, D> {
    pub(crate) fn new(lent: &'m Lent<D, Global>) -> HomeCodeStore<'m, D> {
        HomeCodeStore { lent, commits: 0 }
    }
}

/// The store's refusal for `key` as the ladder's typed error: two values stay `TwoValues`
/// (rule 21); every other stop is `Failed`, naming what the store was doing.
fn refused(key: CodeKey, error: &LentError) -> StoreError {
    match error {
        LentError::TwoValues { .. } => StoreError::TwoValues { key },
        LentError::Store { doing, .. } => StoreError::Failed { key, what: *doing },
        LentError::Arena { .. } => StoreError::Failed { key, what: "the store's arena could not take the entry: its allocator refused" },
        LentError::NotWritable { .. } => StoreError::Failed { key, what: "the local namespace is not writable by this store" },
        LentError::PointerTag { .. } => StoreError::Failed { key, what: "a code key holds a record where a pointer was asked, or a pointer where an entry was" },
        LentError::PointsAtNothing { .. } => StoreError::Failed { key, what: "a code key was pointed at an entry the store does not hold" },
        LentError::TargetGone { .. } => StoreError::Failed { key, what: "a code key's pointer names an entry the store no longer holds" },
        LentError::TargetTag { .. } => StoreError::Failed { key, what: "a code key's pointer names a record filed under another tag than CODE_TAG" },
        LentError::PointerForm { .. } => StoreError::Failed { key, what: "a code key's pointer record does not hold one key" },
    }
}

impl<D: Driver> CodeStore for HomeCodeStore<'_, D> {
    fn get(&self, key: CodeKey) -> Result<Option<&[u8]>, StoreError> {
        match self.lent.get(&CasKey(key.0)) {
            Ok(None) => Ok(None),
            Ok(Some(lend)) if lend.tag == CODE_TAG => Ok(Some(lend.bytes)),
            Ok(Some(_)) => Err(StoreError::Failed { key, what: "a record under a code key is filed under another tag than CODE_TAG: not a code entry" }),
            // A code key held as a pointer has no entry under it: its entry is the pointer's
            // target, read through `pointed` (`entry_of` asks it first).
            Err(LentError::PointerTag { .. }) => Ok(None),
            Err(error) => Err(refused(key, &error)),
        }
    }

    fn put(&mut self, key: CodeKey, bytes: &[u8]) -> Result<(), StoreError> {
        if self.lent.uncommitted_bytes() > COMMIT_BYTES {
            self.lent.commit().map_err(|error| refused(key, &error))?;
            self.commits += 1;
        }
        self.lent.put(&CasKey(key.0), Namespace::LOCAL, CODE_TAG, bytes).map_err(|error| refused(key, &error))?;
        Ok(())
    }

    /// The content key the code key `key` points at (`Lent::pointed`, the newest commit's
    /// value), its target an entry under [`CODE_TAG`]. A target compaction let go of over the
    /// store's cap (pruning keeps what roots name, and a run's entries are not rooted yet) is
    /// the cache's miss: the instance is built again, put, and `key` pointed anew; that is the
    /// path every miss takes, not another route.
    fn pointed(&self, key: CodeKey) -> Result<Option<CodeKey>, StoreError> {
        match self.lent.pointed(CODE_TAG, &CasKey(key.0)) {
            Ok(found) => Ok(found.map(|target| CodeKey(target.0))),
            Err(LentError::TargetGone { .. }) => Ok(None),
            // A code key that holds a plain entry, no pointer (the thread-local getter's and
            // ender's, put under their code keys by `RecordRun::getter`/`ender`): no target, as
            // `MemoryStore::pointed` answers; `entry_of` then reads the entry under the key.
            Err(LentError::PointerTag { .. }) => Ok(None),
            Err(error) => Err(refused(key, &error)),
        }
    }

    /// `key` pointed at the entry put under `content` (`Lent::point`): in the same commit as
    /// that entry, unless a commit came between the two (then the entry is durable first).
    fn point(&mut self, key: CodeKey, content: CodeKey) -> Result<(), StoreError> {
        if self.lent.uncommitted_bytes() > COMMIT_BYTES {
            self.lent.commit().map_err(|error| refused(key, &error))?;
            self.commits += 1;
        }
        self.lent.point(CODE_TAG, &CasKey(key.0), &CasKey(content.0)).map_err(|error| refused(key, &error))?;
        Ok(())
    }
}

/// Entries kept across processes, keys that move with every input they cover, and the planted
/// keys the store's refusal catches.
#[cfg(test)]
#[path = "code_store_tests.rs"]
mod tests;
