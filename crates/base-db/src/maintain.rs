//! Write-side view maintenance (docs/v2/plan/design/dbsp.md §3): every write to a [`Db`] input
//! ends by running the [`Maintainer`]s over what it [`Written`], and each maintainer updates
//! its state by that delta and sets the query inputs whose slice changed. Queries never read a
//! maintainer's state, only the inputs it sets, so every query stays a pure function of inputs.
//!
//! A write method of [`Db`] notes what it changed into the database's pending [`Written`];
//! outside a [`Db::batch`] it then runs the maintainers, inside one the batch's end does, once,
//! over everything the batch wrote. Maintainers run in `order`, each once per pass. Writes a
//! maintainer makes itself are not noted and run no maintenance: what a maintainer sets is its
//! own output, not a change to the program.
//!
//! A maintainer runs in two phases, a step at a time ([`Maintainer`]): its reads, on `&Db`,
//! name what the step reads as batches of [`Reads`], which run as one batch on the database's
//! workers; then its apply, on `&mut Db`, diffs what they computed and sets inputs. Apply reads
//! only what the reads did: a query apply executes is a bug, and a database that logs its
//! events ([`Db::for_tests`], [`Db::logged`]) stops the write with it, so the serial phase
//! cannot grow back. A step's reads see what every earlier step applied: a maintainer whose
//! work for one crate reads what it set for another (name resolution, by the crate graph) asks
//! for another step ([`Reads::again`]).
//!
//! A panic in a write or a maintainer drops every maintainer's state: the next write maintains
//! from empty, which is the cold run, not another path.

use std::{
    any::{Any, TypeId},
    panic::{self, AssertUnwindSafe},
    sync::OnceLock,
};

use blaze_dbsp::Work;
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_mem::{Tag, frame};
use parking_lot::{Mutex, MutexGuard};
use smallvec::SmallVec;
use span::FileId;

use crate::{Crate, Db, IncludedFile, SourceRoot, SourceRootId};

#[cfg(test)]
mod tests;

/// What one write changed: what a maintainer reads to find its delta. Each list is sorted and
/// has each item once; a file in `added` or `removed` is not also in `set`.
#[derive(Clone, Copy, Debug)]
pub struct Written<'a> {
    /// Files whose text changed.
    pub set: &'a [FileId],
    pub added: &'a [FileId],
    pub removed: &'a [FileId],
    /// Source roots whose file sets changed (a `mod` probe may now hit).
    pub roots: &'a [SourceRootId],
    /// The crate graph, cfg or env changed, or proc macros set by [`Db::note_crates_changed`].
    pub crates: bool,
    /// Crates given proc macros in place of those they had ([`Db::note_proc_macros`]).
    pub proc_macros: &'a [Crate],
    /// Files `include_bytes!` or `include_str!` read whose bytes changed.
    pub included: &'a [IncludedFile],
}

impl Written<'_> {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
            && self.added.is_empty()
            && self.removed.is_empty()
            && self.roots.is_empty()
            && !self.crates
            && self.proc_macros.is_empty()
            && self.included.is_empty()
    }
}

/// A write-side view maintainer, registered with `inventory::submit!` by the crate with the code
/// (as `hir-ty/src/providers.rs` registers MIR): nothing installs anything.
///
/// Each pass runs it a step at a time from step 0: `reads`, then the batches it asked for on
/// the database's workers, then `apply`; another step follows while `reads` asks for one.
pub struct Maintainer {
    /// When it runs among the others: lower first. hir-def's name resolution is `100`, hir-ty's
    /// impls and trait candidates `200`.
    pub order: u16,
    pub name: &'static str,
    /// What the step reads: pure, on `&Db`. It may read its state ([`Db::lend_state`]) and
    /// queries, and asks for the step's reads as batches ([`Reads::each`]), which may own what
    /// they compute and hand it to `apply`.
    pub reads: fn(&Db, &Written<'_>, &mut Reads),
    /// The step's writes: what the reads computed diffed against the state, and the inputs
    /// that changed set. It executes no query: what it reads, the reads read first.
    pub apply: fn(&mut Db, &Written<'_>, &Reads),
}

/// What one step of a maintainer reads ([`Maintainer::reads`]): batches of keys, each read by a
/// function on `&Db`, all of a step's run as one batch on the database's workers in no order
/// (as [`Db::run_all`] runs its tasks). A key is the read's own: it may carry what the read
/// computes to `apply`, which takes it back ([`Reads::take`]).
pub struct Reads {
    step: u32,
    again: bool,
    // dyn: a batch per kind of key a step reads, a few a step
    batches: Vec<Box<dyn Batch>>,
}

impl Reads {
    fn new(step: u32) -> Reads {
        Reads { step, again: false, batches: Vec::new() } // alloc: none until a batch is asked for
    }

    /// Which step of this pass it is, from 0.
    pub fn step(&self) -> u32 {
        self.step
    }

    /// Asks for another step after this one's apply: its reads see what this one set.
    pub fn again(&mut self) {
        self.again = true;
    }

    /// Reads each of `keys` with `read`, in a frame tagged `tag`: `read` is handed the write too.
    pub fn each<K: Send + 'static>(
        &mut self,
        tag: Tag,
        keys: impl IntoIterator<Item = K>,
        read: fn(&Db, &Written<'_>, &mut K),
    ) {
        // alloc: a step's keys, once per batch asked for
        let keys: Box<[Mutex<Option<K>>]> = keys.into_iter().map(|key| Mutex::new(Some(key))).collect();
        if !keys.is_empty() {
            // alloc: a batch, once per kind of key a step reads
            self.batches.push(Box::new(Each { tag, read, keys }));
        }
    }

    /// The keys of every batch of `K` after their reads, in the order they were asked for: each
    /// is taken once.
    pub fn take<K: Send + 'static>(&self) -> impl Iterator<Item = K> + '_ {
        self.batches
            .iter()
            .filter_map(|batch| batch.as_any().downcast_ref::<Each<K>>())
            .flat_map(|batch| batch.keys.iter())
            .filter_map(|slot| lock(slot).take())
    }

    /// How many keys the step reads.
    pub fn len(&self) -> usize {
        self.batches.iter().map(|batch| batch.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    /// Runs every batch's reads as one batch on `db`'s workers.
    fn run(&self, db: &Db, written: &Written<'_>) {
        let mut starts = Vec::with_capacity(self.batches.len()); // alloc: a step's batches' starts, a few
        let mut count = 0;
        for batch in &self.batches {
            starts.push(count);
            count += batch.len();
        }
        db.run_every(count, |db, at| {
            let which = starts.partition_point(|&start| start <= at) - 1;
            let batch = &self.batches[which];
            frame::task(batch.tag(), |_| batch.read(db, written, at - starts[which]));
        });
    }
}

/// A batch of [`Reads`], its key type erased.
trait Batch: Send + Sync {
    fn len(&self) -> usize;
    fn tag(&self) -> Tag;
    fn read(&self, db: &Db, written: &Written<'_>, at: usize);
    // dyn: the batch, to find its key type again
    fn as_any(&self) -> &dyn Any;
}

struct Each<K> {
    tag: Tag,
    read: fn(&Db, &Written<'_>, &mut K),
    /// Each key out of its slot while its read runs: no lock is held across a query, which may
    /// move the read to another thread.
    keys: Box<[Mutex<Option<K>>]>,
}

impl<K: Send + 'static> Batch for Each<K> {
    fn len(&self) -> usize {
        self.keys.len()
    }

    fn tag(&self) -> Tag {
        self.tag
    }

    fn read(&self, db: &Db, written: &Written<'_>, at: usize) {
        let slot = &self.keys[at];
        let mut key = lock(slot).take().expect("each key of a batch is read once");
        (self.read)(db, written, &mut key);
        *lock(slot) = Some(key);
    }

    // dyn: the batch, to find its key type again
    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock()
}

inventory::collect!(Maintainer);

/// What a maintainer did for one crate in one maintenance pass ([`Db::report_work`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Maintained {
    /// Which pass since the records were last taken, from 0: one write outside a batch, or one
    /// batch.
    pub write: u32,
    /// [`Maintainer::name`].
    pub maintainer: &'static str,
    pub krate: Crate,
    pub work: Work,
}

/// The maintainers linked into this binary, in `order` (then name), sorted once.
pub fn maintainers() -> &'static [&'static Maintainer] {
    static SORTED: OnceLock<Box<[&'static Maintainer]>> = OnceLock::new();
    SORTED.get_or_init(|| {
        // alloc: once per process, the sorted list of linked maintainers
        let mut all: Vec<&'static Maintainer> = inventory::iter::<Maintainer>().collect();
        all.sort_by_key(|maintainer| (maintainer.order, maintainer.name));
        for pair in all.windows(2) {
            assert!(
                pair[0].name != pair[1].name,
                "two maintainers are named `{}`",
                pair[0].name
            );
        }
        all.into_boxed_slice()
    })
}

/// The [`Db`] field behind maintenance.
#[derive(Default)]
pub(crate) struct Maintenance {
    /// Behind a lock so a maintainer's reads, on `&Db`, can have it lent ([`Db::lend_state`]).
    // dyn: write-side state, one lookup per maintainer per write
    state: Mutex<HashMap<TypeId, Box<dyn Any + Send + Sync>>>,
    /// Whether a maintainer's reads are running: only they are lent state on `&Db`.
    reading: bool,
    pending: Pending,
    /// How many [`Db::batch`]es are open.
    depth: u32,
    /// Whether the maintainers are running: their own writes are not noted.
    maintaining: bool,
    /// Whether [`Db::report_work`] keeps its records ([`Db::keep_maintained`]).
    keep: bool,
    /// The passes run since the records were last taken.
    passes: u32,
    maintained: Vec<Maintained>,
}

/// A pending [`Written`], owned. Its buffers are kept from one pass to the next, and made with
/// room for [`PENDING_ROOM`] of each, so the first writes of a new database grow none of them.
struct Pending {
    set: Vec<FileId>,
    added: Vec<FileId>,
    removed: Vec<FileId>,
    roots: Vec<SourceRootId>,
    crates: bool,
    proc_macros: Vec<Crate>,
    included: Vec<IncludedFile>,
}

/// What each of [`Pending`]'s lists has room for when the database is made: an edit's files.
const PENDING_ROOM: usize = 64;

impl Default for Pending {
    fn default() -> Pending {
        // alloc: once a database, as it is made.
        Pending {
            set: Vec::with_capacity(PENDING_ROOM),
            added: Vec::with_capacity(PENDING_ROOM),
            removed: Vec::with_capacity(PENDING_ROOM),
            roots: Vec::with_capacity(PENDING_ROOM),
            crates: false,
            proc_macros: Vec::new(), // alloc: grows to the proc-macro crates, kept from one pass to the next
            included: Vec::with_capacity(PENDING_ROOM),
        }
    }
}

impl Pending {
    /// No lists and no room: what stands in for the pending writes while the maintainers read
    /// them.
    const fn empty() -> Pending {
        Pending {
            set: Vec::new(), // alloc: none, an empty list
            added: Vec::new(), // alloc: see above
            removed: Vec::new(), // alloc: see above
            roots: Vec::new(), // alloc: see above
            crates: false,
            proc_macros: Vec::new(), // alloc: see above
            included: Vec::new(), // alloc: see above
        }
    }

    fn is_empty(&self) -> bool {
        self.written().is_empty()
    }

    fn written(&self) -> Written<'_> {
        Written {
            set: &self.set,
            added: &self.added,
            removed: &self.removed,
            roots: &self.roots,
            crates: self.crates,
            proc_macros: &self.proc_macros,
            included: &self.included,
        }
    }

    /// Sorts and dedups each list, and takes the files added or removed out of `set`.
    fn normalize(&mut self) {
        for files in [&mut self.set, &mut self.added, &mut self.removed] {
            files.sort_unstable();
            files.dedup();
        }
        self.roots.sort_unstable();
        self.roots.dedup();
        self.proc_macros.sort_unstable();
        self.proc_macros.dedup();
        let (added, removed) = (&self.added, &self.removed);
        self.set.retain(|file| {
            added.binary_search(file).is_err() && removed.binary_search(file).is_err()
        });
    }

    fn clear(&mut self) {
        self.set.clear();
        self.added.clear();
        self.removed.clear();
        self.roots.clear();
        self.crates = false;
        self.proc_macros.clear();
        self.included.clear();
    }
}

/// What a write method notes it changed ([`Db::note`]).
pub(crate) enum Note<'a> {
    Set(FileId),
    View { set: &'a [FileId], added: &'a [FileId], removed: &'a [FileId], roots: &'a [SourceRootId] },
    Root(SourceRootId),
    Crates,
    ProcMacros(Crate),
    Included(IncludedFile),
}

impl Db {
    /// Runs `f` with this database and its write-side state `T` (made by `Default`); the state is
    /// taken out for the call, so `f` may read queries with `&*db` and set inputs with `db`.
    /// No query can reach it. If `f` unwinds, the state is dropped, and the next call starts
    /// from `T::default()`.
    ///
    /// # Panics
    ///
    /// When `f` asks for the same `T` again: it is out for this call.
    pub fn with_state<T: Default + Send + Sync + 'static, R>(
        &mut self,
        f: impl FnOnce(&mut Db, &mut T) -> R,
    ) -> R {
        let mut state = take_state::<T>(self.maintenance.state.get_mut());
        let ran = panic::catch_unwind(AssertUnwindSafe(|| f(self, &mut *state)));
        put_state(self.maintenance.state.get_mut(), state, ran)
    }

    /// [`Db::with_state`] for a maintainer's reads, which have the database by `&`: `f` gets the
    /// state `T` alone, and may run queries and batches with the `&Db` it has. Two reads running
    /// at once may not ask for one state.
    ///
    /// # Panics
    ///
    /// Outside a maintainer's reads, or when `T` is out already.
    pub fn lend_state<T: Default + Send + Sync + 'static, R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        assert!(self.maintenance.reading, "`Db::lend_state` outside a maintainer's reads");
        let mut state = take_state::<T>(&mut lock(&self.maintenance.state));
        let ran = panic::catch_unwind(AssertUnwindSafe(|| f(&mut *state)));
        put_state(&mut lock(&self.maintenance.state), state, ran)
    }

    /// Runs `f` with maintenance held back, then runs each maintainer once over everything `f`
    /// wrote. Every write method outside a batch is its own batch; a batch inside another, or
    /// inside a maintainer, is part of the one around it.
    pub fn batch<R>(&mut self, f: impl FnOnce(&mut Db) -> R) -> R {
        self.maintenance.depth += 1;
        let ran = panic::catch_unwind(AssertUnwindSafe(|| f(self)));
        self.maintenance.depth -= 1;
        match ran {
            Ok(result) => {
                if self.maintenance.depth == 0 && !self.maintenance.maintaining {
                    self.maintain();
                }
                result
            }
            Err(panic) => {
                // The inputs may be half written: what is maintained no longer follows them.
                self.forget_maintained_state();
                panic::resume_unwind(panic)
            }
        }
    }

    /// Notes that an input of the crate graph changed that base-db does not set itself (a
    /// crate's proc macros), as a write: outside a [`Db::batch`] the maintainers run now.
    pub fn note_crates_changed(&mut self) {
        self.batch(|db| db.note(Note::Crates));
    }

    /// Notes that `krate` was given proc macros in place of those it had, as a write: outside a
    /// [`Db::batch`] the maintainers run now. Unlike [`Db::note_crates_changed`] it leaves the
    /// other crates' maintained state warm: a crate's list reaches only its own def map, and
    /// the crates depending on it through that.
    pub fn note_proc_macros(&mut self, krate: Crate) {
        self.batch(|db| db.note(Note::ProcMacros(krate)));
    }

    /// Records what `maintainer` did for `krate` in this pass, while [`Db::keep_maintained`] is
    /// on; a maintainer reports each crate it went over once per pass, zeros included.
    pub fn report_work(&mut self, maintainer: &'static str, krate: Crate, work: Work) {
        let maintenance = &mut self.maintenance;
        if maintenance.keep {
            let write = maintenance.passes.saturating_sub(1);
            // alloc: a record per crate per pass, only while a request keeps them
            maintenance.maintained.push(Maintained { write, maintainer, krate, work });
        }
    }

    /// Whether [`Db::report_work`] keeps its records from now on (off at first): the daemon
    /// turns it on for a request that prints them, a cost test for what it asserts.
    pub fn keep_maintained(&mut self, keep: bool) {
        self.maintenance.keep = keep;
        if !keep {
            self.maintenance.maintained.clear();
        }
    }

    /// Appends the records kept since the last call to `out` and forgets them; the next pass is
    /// `write` 0 again.
    pub fn take_maintained(&mut self, out: &mut Vec<Maintained>) {
        out.append(&mut self.maintenance.maintained);
        self.maintenance.passes = 0;
    }

    /// Notes what a write method changed; nothing while the maintainers run.
    pub(crate) fn note(&mut self, note: Note<'_>) {
        let maintenance = &mut self.maintenance;
        if maintenance.maintaining {
            return;
        }
        let pending = &mut maintenance.pending;
        match note {
            Note::Set(file) => pending.set.push(file),
            Note::View { set, added, removed, roots } => {
                pending.set.extend_from_slice(set);
                pending.added.extend_from_slice(added);
                pending.removed.extend_from_slice(removed);
                pending.roots.extend_from_slice(roots);
            }
            Note::Root(root) => pending.roots.push(root),
            Note::Crates => pending.crates = true,
            Note::ProcMacros(krate) => pending.proc_macros.push(krate),
            Note::Included(file) => {
                if !pending.included.contains(&file) {
                    pending.included.push(file);
                }
            }
        }
    }

    /// Notes a source root set from `old` (`None` for a new root) to `new`: the root, and the
    /// files that came into it or left it. `O(files of both)`, on a root's set, which is setup
    /// or a view change.
    pub(crate) fn note_root(&mut self, id: SourceRootId, old: Option<&SourceRoot>, new: &SourceRoot) {
        let maintenance = &mut self.maintenance;
        if maintenance.maintaining {
            return;
        }
        let pending = &mut maintenance.pending;
        pending.roots.push(id);
        match old {
            Some(old) => {
                pending.added.extend(new.iter().filter(|file| old.path_for_file(file).is_none()));
                pending.removed.extend(old.iter().filter(|file| new.path_for_file(file).is_none()));
            }
            None => pending.added.extend(new.iter()),
        }
    }

    /// Runs `f` as a write of crate inputs: the crate graph's content ID (of every crate's
    /// data, cfg, env and dependencies) before and after tells whether it changed anything.
    pub(crate) fn write_crates<R>(&mut self, f: impl FnOnce(&mut Db) -> R) -> R {
        self.batch(|db| {
            let before = crate::all_crates_content(&*db);
            let result = f(db);
            if crate::all_crates_content(&*db) != before {
                db.note(Note::Crates);
            }
            result
        })
    }

    /// Runs `f` as a write of `file`'s text input, noted when its content ID changed, and of
    /// its aliases' ([`crate::Files::alias`]), which every write of it writes too.
    pub(crate) fn write_file<R>(&mut self, file: FileId, f: impl FnOnce(&mut Db) -> R) -> R {
        self.batch(|db| {
            let before = db.files().entry_content(&*db, file);
            let aliases = db.files().aliases_of(file);
            let mut was: SmallVec<[ContentId; 2]> = SmallVec::new();
            was.extend(aliases.iter().map(|&alias| db.files().entry_content(&*db, alias)));
            let result = f(db);
            if db.files().entry_content(&*db, file) != before {
                db.note(Note::Set(file));
            }
            for (&alias, was) in aliases.iter().zip(was) {
                if db.files().entry_content(&*db, alias) != was {
                    db.note(Note::Set(alias));
                }
            }
            result
        })
    }

    /// Runs every maintainer once over the pending writes, then clears them.
    fn maintain(&mut self) {
        if self.maintenance.pending.is_empty() {
            return;
        }
        let all = maintainers();
        if all.is_empty() {
            self.maintenance.pending.clear();
            return;
        }
        self.maintenance.pending.normalize();
        self.maintenance.passes = self.maintenance.passes.saturating_add(1);
        let pending = std::mem::replace(&mut self.maintenance.pending, Pending::empty());
        self.maintenance.maintaining = true;
        let _span = tracing::info_span!(target: "blaze", "maintain").entered();
        let ran = panic::catch_unwind(AssertUnwindSafe(|| {
            let written = pending.written();
            for maintainer in all {
                let _span = tracing::info_span!(target: "blaze", "maintainer", function = maintainer.name).entered();
                self.run_maintainer(maintainer, &written);
            }
        }));
        self.maintenance.maintaining = false;
        self.maintenance.reading = false;
        self.end_apply();
        let mut pending = pending;
        pending.clear();
        self.maintenance.pending = pending;
        if let Err(panic) = ran {
            self.forget_maintained_state();
            panic::resume_unwind(panic);
        }
    }

    /// Runs `maintainer` a step at a time over `written`.
    fn run_maintainer(&mut self, maintainer: &Maintainer, written: &Written<'_>) {
        let mut step = 0;
        loop {
            let mut reads = Reads::new(step);
            {
                let _span = tracing::info_span!(target: "blaze", "maintainer reads", function = maintainer.name, step).entered();
                self.maintenance.reading = true;
                (maintainer.reads)(&*self, written, &mut reads);
                reads.run(&*self, written);
                self.maintenance.reading = false;
            }
            {
                let _span = tracing::info_span!(target: "blaze", "maintainer apply", function = maintainer.name, step).entered();
                self.begin_apply();
                (maintainer.apply)(self, written, &reads);
                let executed = self.end_apply();
                assert!(
                    executed.is_empty(),
                    "maintainer `{}`, step {step}: its apply executed {executed:?}, which its reads \
                     did not read: they move to its reads",
                    maintainer.name,
                );
            }
            if !reads.again {
                return;
            }
            step += 1;
        }
    }

    /// Drops every maintainer's state, so the next write maintains from empty, and has it
    /// re-check everything.
    fn forget_maintained_state(&mut self) {
        self.maintenance.state.get_mut().clear();
        self.maintenance.pending.crates = true;
    }
}

/// What a state's slot holds while [`Db::with_state`] has it out.
struct InUse;

/// Takes the state `T` out of `states` (made by `Default` if there is none), leaving its slot
/// marked out.
fn take_state<T: Default + Send + Sync + 'static>(
    // dyn: write-side state, one lookup per maintainer per write
    states: &mut HashMap<TypeId, Box<dyn Any + Send + Sync>>,
) -> Box<T> {
    let id = TypeId::of::<T>();
    match states.get_mut(&id) {
        Some(slot) => {
            // One placeholder per state while it is out: a zero-sized `Box` allocates nothing.
            let taken = std::mem::replace(slot, Box::new(InUse)); // alloc: zero-sized, allocates nothing
            match taken.downcast::<T>() {
                Ok(state) => state,
                Err(_) => panic!(
                    "`Db::with_state::<{}>` inside a call that has it out",
                    std::any::type_name::<T>()
                ),
            }
        }
        None => {
            states.insert(id, Box::new(InUse)); // alloc: zero-sized, allocates nothing
            // alloc: a maintainer's state, once per database (and after a panic dropped it)
            Box::new(T::default())
        }
    }
}

/// Puts `state` back in `states` after the call `ran`, or drops it if the call unwound, and
/// goes on with the call's result or its panic.
fn put_state<T: Send + Sync + 'static, R>(
    // dyn: write-side state, one lookup per maintainer per write
    states: &mut HashMap<TypeId, Box<dyn Any + Send + Sync>>,
    state: Box<T>,
    ran: std::thread::Result<R>,
) -> R {
    let id = TypeId::of::<T>();
    match ran {
        Ok(result) => {
            let state: Box<dyn Any + Send + Sync> = state; // dyn: write-side state, one lookup per maintainer per write
            states.insert(id, state);
            result
        }
        Err(panic) => {
            states.remove(&id);
            drop(state);
            panic::resume_unwind(panic)
        }
    }
}
