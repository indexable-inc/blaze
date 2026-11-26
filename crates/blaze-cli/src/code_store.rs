//! Code records kept across commands: blaze-ladder's `CodeStore` over the ONE store a user, the
//! plain store the command already opens at `<home>/.blaze/` plain_place.rs `STORE_DIR`, the
//! folder of this blaze's forms (`v2/store/f5s3`, D160) (store.rs,
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
//! Every entry is filed under [`ReservedTag::Code`], apart from the drive's records (`ReservedTag::Drive`), and in
//! `Namespace::LOCAL` only: an entry is the machine code of the program's own bodies, which never
//! leaves the machine unless an organisation opts in (the code review's "Trust" rule). A
//! toolchain-only entry made by a trusted builder for the shared namespace is later work.
//!
//! The key holds the code builder's identity, the edition and the emitting tier, and an
//! instance's entry carries its read list (blaze-ladder code_reads.rs), checked when the entry is
//! found. An entry is put under its CONTENT key, and its code key is a POINTER to it
//! (`Lent::point`, the plain store's `ReservedTag::Pointer` record: the newest commit's value wins, a
//! stale entry's rebuild replaces it), committed with the entry it names.

use core::cell::RefCell;

use blaze_cas::Key as CasKey;
use blaze_cas::plain::{CommittedLend, INDEX_KEYS_MOST, Lent, LentError, Namespace};
use blaze_core::Driver;
use blaze_hash::{BuildKeyHasher, Map, MapOpts};
use blaze_ids::ContentId;
use blaze_keyed::kind::ReservedTag;
use blaze_keyed::store::Put;
use blaze_ladder::code_records::{CodeClass, CodeKey, CodeStore, StoreError};
use blaze_own::Vec as OwnVec;

use crate::store::plain_place::{COMMIT_BYTES, StoreAlloc};

// Every code entry is filed under [`ReservedTag::Code`] in the plain store's index (blaze-keyed's
// registry of reserved tags). A record under a code key with another tag is refused by name,
// never read as code.

/// The code index over the command's store (the module doc). `'m` is the borrow of the
/// command's [`Lent`]; every byte a get returns lives as long.
pub(crate) struct HomeCodeStore<'m, D: Driver> {
    lent: &'m Lent<D, StoreAlloc>,
    /// Commits made on the command's path (the arena past `COMMIT_BYTES`).
    #[allow(dead_code, reason = "read by the tests; the profile row that shows it is owed")]
    pub commits: u32,
    /// Every key this run's code came from or went to: each entry found or put, each pointer
    /// found or pointed and its target. The program's root names exactly these and the start
    /// list's (store/roots.rs `keep_program`): code reaches the store through this handle
    /// alone, so what a later run reads is named by construction, never by remembering. A
    /// toolchain-class write goes to `toolchain` instead (store/roots.rs `keep_toolchain`); a
    /// hit of either class is the program's too (a get does not know its class, and naming a
    /// toolchain record from the program root as well only keeps it longer).
    reached: RefCell<std::vec::Vec<CasKey, StoreAlloc>>,
    /// The toolchain-class keys this run wrote: entries and pointers with their targets.
    toolchain: std::vec::Vec<CasKey, StoreAlloc>,
    /// Generation of `lent` when this handle was opened: if no mid-walk commit or external flip
    /// moved past it, `ensure_persisted` takes an O(1) fast path.
    opened_gen: u64,
    /// Code entries already read from `lent` (or confirmed present via committed `Put::Same`)
    /// during this command, borrowing `lent`'s immutable segment mmaps / kept arenas directly
    /// with zero payload copies. Prevents a mid-command `lent.commit()` that adopts a newer
    /// manifest (after concurrent `blaze gc` swept an unrooted pre-existing segment) from making
    /// an already-stepped entry disappear (`MissingEntry`) before the command finishes reading it,
    /// and enables [`HomeCodeStore::ensure_persisted`] to rewrite any swept entry before rooting.
    lent_entries: RefCell<Map<[u8; 32], &'m [u8], BuildKeyHasher, StoreAlloc>>,
    /// Pointer associations (`code_key -> content_key`) observed or written during this command so
    /// [`HomeCodeStore::ensure_persisted`] can rewrite any swept pointer before `keep_program`
    /// roots it.
    lent_pointers: RefCell<Map<[u8; 32], CasKey, BuildKeyHasher, StoreAlloc>>,
    /// Test-only switch for the DST planted-bug control (`SkipLentEntries`).
    #[cfg(test)]
    use_lent_entries: bool,
}

/// The most keys one run's code may reach: one index's (blaze-cas `INDEX_KEYS_MOST`); one more
/// is refused by name, never dropped from the root.
const REACHED_MOST: usize = INDEX_KEYS_MOST;

impl<'m, D: Driver> HomeCodeStore<'m, D> {
    pub(crate) fn new(lent: &'m Lent<D, StoreAlloc>) -> HomeCodeStore<'m, D> {
        let alloc = *lent.alloc();
        let opened_gen = lent.generation();
        let lent_entries = RefCell::new(Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: None }));
        let lent_pointers = RefCell::new(Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: None }));
        HomeCodeStore {
            lent,
            commits: 0,
            opened_gen,
            reached: RefCell::new(std::vec::Vec::new_in(alloc)), toolchain: std::vec::Vec::new_in(alloc),
            lent_entries,
            lent_pointers,
            #[cfg(test)]
            use_lent_entries: true,
        }
    }

    /// Test-only control (rule 23): disables `lent_entries` lookup so the DST oracle proves
    /// mid-walk manifest adoption after concurrent GC causes `MissingEntry` without it.
    #[cfg(test)]
    pub(crate) fn disable_lent_entries_for_test(&mut self) {
        self.use_lent_entries = false;
    }

    /// Ensures every cached content entry and pointer observed by this handle is still present in
    /// `lent`'s current manifest (or uncommitted arena under the active lease) before the caller
    /// persists closing roots (`keep_program` / `keep_toolchain`).
    ///
    /// Fast path: when neither this process nor any concurrent writer flipped since `HomeCodeStore::new`,
    /// returns `Ok(0)` after a single 64-byte `head` generation check. When a concurrent GC swept
    /// a pre-existing unrooted segment and a mid-walk or sync commit adopted that manifest, rewrites
    /// every swept content record (from `lent`'s immutable mmaps in `lent_entries`) and its pointer
    /// association (`lent_pointers`) in deterministic key order under the active lease so the
    /// closing root never has holes.
    pub(crate) fn ensure_persisted(&mut self) -> Result<usize, StoreError> {
        let sentinel = CodeKey(ContentId([0; 32]));
        if self.lent.generation() == self.opened_gen {
            let disk_gen = self.lent.head_generation_on_disk().map_err(|error| refused(sentinel, &error))?;
            if disk_gen == self.opened_gen {
                return Ok(0);
            }
            if self.lent.needs_commit() {
                self.lent.commit().map_err(|error| refused(sentinel, &error))?;
                self.commits += 1;
            }
        }
        if self.lent.generation() == self.opened_gen {
            return Ok(0);
        }
        let alloc = *self.lent.alloc();
        let mut entries: OwnVec<([u8; 32], &'m [u8]), StoreAlloc> = OwnVec::with_capacity_in(self.lent_entries.borrow().len(), alloc);
        for (&raw, &bytes) in self.lent_entries.borrow().iter() {
            entries.push((raw, bytes));
        }
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut restored = 0usize;
        for (raw, bytes) in entries {
            let cas = CasKey(ContentId(raw));
            let code = CodeKey(ContentId(raw));
            match self.lent.tag_of(&cas).map_err(|error| refused(code, &error))? {
                Some(tag) if ReservedTag::Code.is(tag) => {}
                Some(_) => return Err(StoreError::Failed { key: code, what: "a record under a code key is filed under another tag than ReservedTag::Code: not a code entry" }),
                None => {
                    if self.lent.uncommitted_bytes() > COMMIT_BYTES {
                        self.lent.commit().map_err(|error| refused(code, &error))?;
                        self.commits += 1;
                    }
                    self.lent.put(&cas, Namespace::LOCAL, ReservedTag::Code.tag(), bytes).map_err(|error| refused(code, &error))?;
                    restored += 1;
                }
            }
        }
        let mut pointers: OwnVec<([u8; 32], CasKey), StoreAlloc> = OwnVec::with_capacity_in(self.lent_pointers.borrow().len(), alloc);
        for (&raw, &target) in self.lent_pointers.borrow().iter() {
            pointers.push((raw, target));
        }
        pointers.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for (raw, target) in pointers {
            let cas = CasKey(ContentId(raw));
            let code = CodeKey(ContentId(raw));
            let needs_point = match self.lent.pointed(ReservedTag::Code.tag(), &cas) {
                Ok(Some(cur)) if cur == target => false,
                Ok(Some(_)) | Ok(None) | Err(LentError::TargetGone { .. }) => true,
                Err(error) => return Err(refused(code, &error)),
            };
            if needs_point {
                if self.lent.uncommitted_bytes() > COMMIT_BYTES {
                    self.lent.commit().map_err(|error| refused(code, &error))?;
                    self.commits += 1;
                }
                self.lent.point(ReservedTag::Code.tag(), &cas, &target).map_err(|error| refused(code, &error))?;
                restored += 1;
            }
        }
        Ok(restored)
    }

    /// The toolchain-class keys this run wrote, sorted, each once (the toolchain root's new
    /// index, store/roots.rs `keep_toolchain`).
    pub(crate) fn toolchain_made(&self) -> std::vec::Vec<CasKey, StoreAlloc> {
        let mut keys = self.toolchain.clone();
        keys.sort_unstable();
        keys.dedup();
        keys
    }

    /// `keys` written as `class` by `key`'s write: the toolchain's set or the program's.
    fn wrote(&mut self, class: CodeClass, key: CodeKey, keys: &[CasKey]) -> Result<(), StoreError> {
        match class {
            CodeClass::Program => self.reach(key, keys),
            CodeClass::Toolchain => {
                if self.toolchain.len() + keys.len() > REACHED_MOST {
                    return Err(StoreError::Failed { key, what: "this run made more toolchain records than one index of the toolchain's root holds (blaze-cas INDEX_KEYS_MOST)" });
                }
                self.toolchain.extend_from_slice(keys);
                Ok(())
            }
        }
    }

    /// The keys this run's code reached, sorted, each once (the program root's walk half).
    pub(crate) fn reached(&self) -> std::vec::Vec<CasKey, StoreAlloc> {
        let mut keys = self.reached.borrow().clone();
        keys.sort_unstable();
        keys.dedup();
        keys
    }

    /// `keys` recorded as reached by `key`'s ask; past [`REACHED_MOST`] (after folding
    /// repeats) refused, naming the key.
    fn reach(&self, key: CodeKey, keys: &[CasKey]) -> Result<(), StoreError> {
        let mut reached = self.reached.borrow_mut();
        if reached.len() + keys.len() > REACHED_MOST {
            reached.sort_unstable();
            reached.dedup();
            if reached.len() + keys.len() > REACHED_MOST {
                return Err(StoreError::Failed { key, what: "this run's code reached more records than one index of the program's root holds (blaze-cas INDEX_KEYS_MOST)" });
            }
        }
        reached.extend_from_slice(keys);
        Ok(())
    }
}

/// The store's refusal for `key` as the ladder's typed error: two values stay `TwoValues`
/// (rule 21); every other stop is `Failed`, naming what the store was doing.
fn refused(key: CodeKey, error: &LentError) -> StoreError {
    match error {
        LentError::TwoValues { .. } => StoreError::TwoValues { key },
        LentError::Store { doing, .. } => StoreError::Failed { key, what: *doing },
        LentError::Arena { .. } => StoreError::Failed { key, what: "the store's arena could not take the entry: its allocator refused" },
        LentError::ArenaReserve { .. } => StoreError::Failed { key, what: "the store's shared arena could not take the entry: its reservation was used up" },
        LentError::NotWritable { .. } => StoreError::Failed { key, what: "the local namespace is not writable by this store" },
        LentError::PointerTag { .. } => StoreError::Failed { key, what: "a code key holds a record where a pointer was asked, or a pointer where an entry was" },
        LentError::PointsAtNothing { .. } => StoreError::Failed { key, what: "a code key was pointed at an entry the store does not hold" },
        LentError::TargetGone { .. } => StoreError::Failed { key, what: "a code key's pointer names an entry the store no longer holds" },
        LentError::TargetTag { .. } => StoreError::Failed { key, what: "a code key's pointer names a record filed under another tag than ReservedTag::Code" },
        LentError::PointerForm { .. } => StoreError::Failed { key, what: "a code key's pointer record does not hold one key" },
    }
}

impl<D: Driver> CodeStore for HomeCodeStore<'_, D> {
    fn get(&self, key: CodeKey) -> Result<Option<&[u8]>, StoreError> {
        #[cfg(test)]
        let use_cache = self.use_lent_entries;
        #[cfg(not(test))]
        let use_cache = true;
        if use_cache && let Some(&bytes) = self.lent_entries.borrow().get(&key.0.0) {
            return Ok(Some(bytes));
        }
        match self.lent.get(&CasKey(key.0)) {
            Ok(None) => Ok(None),
            Ok(Some(lend)) if ReservedTag::Code.is(lend.tag) => {
                self.reach(key, &[CasKey(key.0)])?;
                self.lent_entries.borrow_mut().insert(key.0.0, lend.bytes);
                Ok(Some(lend.bytes))
            }
            Ok(Some(_)) => Err(StoreError::Failed { key, what: "a record under a code key is filed under another tag than ReservedTag::Code: not a code entry" }),
            // A code key held as a pointer has no entry under it: its entry is the pointer's
            // target, read through `pointed` (`entry_of` asks it first).
            Err(LentError::PointerTag { .. }) => Ok(None),
            Err(error) => Err(refused(key, &error)),
        }
    }

    fn put(&mut self, class: CodeClass, key: CodeKey, bytes: &[u8]) -> Result<(), StoreError> {
        if self.lent.uncommitted_bytes() > COMMIT_BYTES {
            self.lent.commit().map_err(|error| refused(key, &error))?;
            self.commits += 1;
        }
        let wrote = self.lent.put(&CasKey(key.0), Namespace::LOCAL, ReservedTag::Code.tag(), bytes).map_err(|error| refused(key, &error))?;
        if wrote == Put::Same
            && let Ok(CommittedLend::Found(lend)) = self.lent.get_committed(&CasKey(key.0))
            && ReservedTag::Code.is(lend.tag)
        {
            self.lent_entries.borrow_mut().insert(key.0.0, lend.bytes);
        }
        self.wrote(class, key, &[CasKey(key.0)])
    }

    /// The content key the code key `key` points at (`Lent::pointed`, the newest commit's
    /// value), its target an entry under [`ReservedTag::Code`]. A target compaction let go of (a
    /// program root that expired, store/roots.rs) is the cache's miss: the instance is built again, put, and `key` pointed anew; that is the
    /// path every miss takes, not another route.
    fn pointed(&self, key: CodeKey) -> Result<Option<CodeKey>, StoreError> {
        #[cfg(test)]
        let use_cache = self.use_lent_entries;
        #[cfg(not(test))]
        let use_cache = true;
        if use_cache && self.lent_entries.borrow().contains_key(&key.0.0) {
            return Ok(None);
        }
        match self.lent.pointed(ReservedTag::Code.tag(),&CasKey(key.0)) {
            // A pointer is kept by naming its name AND its target (blaze-keyed `ReservedTag::Pointer`: reach
            // does not follow a pointer), so both are this run's.
            Ok(Some(target)) => {
                self.reach(key, &[CasKey(key.0), target])?;
                self.lent_pointers.borrow_mut().insert(key.0.0, target);
                Ok(Some(CodeKey(target.0)))
            }
            Ok(None) => Ok(None),
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
    fn point(&mut self, class: CodeClass, key: CodeKey, content: CodeKey) -> Result<(), StoreError> {
        if self.lent.uncommitted_bytes() > COMMIT_BYTES {
            self.lent.commit().map_err(|error| refused(key, &error))?;
            self.commits += 1;
        }
        self.lent.point(ReservedTag::Code.tag(),&CasKey(key.0), &CasKey(content.0)).map_err(|error| refused(key, &error))?;
        self.lent_pointers.borrow_mut().insert(key.0.0, CasKey(content.0));
        self.wrote(class, key, &[CasKey(key.0), CasKey(content.0)])
    }
}

/// Entries kept across processes, keys that move with every input they cover, and the planted
/// keys the store's refusal catches.
#[cfg(test)]
#[path = "code_store_tests.rs"]
mod tests;
