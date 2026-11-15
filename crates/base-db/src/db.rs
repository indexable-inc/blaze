//! The front end's one database, [`Db`]: every query of every front-end crate takes `&Db`.
//!
//! The type is declared here, in the lowest front-end crate, so that each crate above can name
//! it: `hir-expand`, `hir-def`, `hir-ty` and the rest declare their queries as
//! `#[blaze_memo::tracked]` functions over `&Db`, and a call to one is a static call whose memo
//! hit inlines into the caller (docs/v2/plan/design/frontend.md).
//!
//! A `Db` is the driver's world (plain owned data by stable key, [`crate::world`]) and the
//! current snapshot: a [`blaze_memo::Memo`] whose inputs are made from the world and the table
//! from each key to its input. Every query takes `&Db`; nothing below takes `&mut Db` but
//! [`Db::edit`], whose [`Edit`] writes the world and, at [`Edit::commit`], makes the next
//! snapshot. A task that outlives a borrow holds a [`Reader`] of a [`Shared`] database: it
//! holds the snapshot it was made at, never the world, so a write never waits for it.
//!
//! An [`Edit`] is a transaction: its writes take effect only at [`Edit::commit`], and an edit
//! dropped uncommitted (an early return, a `?`, a panic, or a commit that panicked) discards
//! every one, file IDs, crate keys, named paths, gifts and makers among them. The edit reads
//! its own writes (`Edit::written_*`, [`Edit::crate_key`]); a query reads the last commit's
//! snapshot. A commit that moved nothing (no write, or each a write of the value held) makes no
//! snapshot and moves no epoch ([`Committed::moved`]). So a batch of writes that must land
//! together is one edit, owned by the outermost caller and passed down as `&mut Edit`.
//!
//! Why an undo log and not a staged layer. A staged layer (writes kept beside the world,
//! applied at the commit) would have every read of the world look in it first, and the commit
//! write each staged value a second time; the world's own structures (a root's trie, the
//! crates' content IDs refreshed through their dependents, the named paths) would each need a
//! staged twin. With an undo log the setters write the world in place, as before, and each
//! keeps what it replaced (moved out, never copied, except where a write edits a value in
//! place: a column's row is cloned once). Reads of the world are unchanged, the commit adds
//! nothing, and only a discarded edit pays, replaying the log backwards.
//!
//! Cost (rule 67): a lookup ([`Db::file_text`], [`Db::krate`], ...) is one index of a vector of
//! the snapshot's table. [`Db::edit`] is O(1) (it notes the counters and copies the marks,
//! empty after a commit). A setter costs what it did before plus one log entry holding the
//! value it replaced: O(1) beyond the write, and a source root's file set is saved once per
//! edit, the copy its copy-on-write takes anyway. [`Edit::commit`] is [`crate::world`]'s
//! commit plus clearing the log, O(the edit's writes); one that moved nothing is O(1) and
//! makes nothing. A discarded edit is O(its writes), plus hashing again each root whose files
//! it moved (the floor of a root written). No path copies the world. The commit builds the
//! next snapshot beside the old one and only then takes the old table over, so a panic in it
//! leaves the world, the old snapshot and the table whole; the old snapshot, asked for its
//! next once, makes the commit after start a new session ([`world::Fresh`]).

use std::{alloc::Global, cell::Cell, fmt, panic};

use blaze_ids::ContentId;
use blaze_mem::{Tag, frame};
use span::{FileId, TextSize};
use triomphe::Arc;

use blaze_memo::Fact;

use crate::{
    AbsPath, AbsPathBuf, AnchoredPath, Crate, CrateBuilderId, CrateGraphBuilder, CratesIdMap, Env,
    ExtraCrateData, FileSourceRootInput, FileText, Files, Given, IncludedFile, Looks, Nonce,
    SourceRoot, SourceRootId, SourceRootInput, VfsPath, ViewChange, ViewError, ViewRoot,
    settings::FrontSettingsData,
    world::{self, CrateKey, Plant, Snap, World, WorldCrateData},
};
use blaze_lint_defs::LintOptions;
use cfg::CfgOptions;

/// What [`Edit::set_given`] runs each time its arena is made: given the snapshot's builder and
/// the arena's inputs, it sets inputs of its caller's types there.
// dyn: a maker of a crate above this one, which base-db only keeps and runs
pub type MakeGiven = Box<dyn Fn(&blaze_memo::Builder, &blaze_memo::Inputs<'_>) + Send + Sync, Global>;

/// The front end's database. See the module.
///
/// It has no `Default`: a build's is [`Db::new`] and a test's [`Db::for_tests`], each given the
/// session's fact boundaries by its caller, and which one is never left to a default.
pub struct Db {
    snap: std::sync::Arc<Snap, Global>,
    files: Arc<Files>,
    /// The world; `None` in a [`Reader`]'s copy, which only reads its snapshot.
    world: Option<Box<World, Global>>,
    nonce: Nonce,
    run: Run,
}

/// Where [`Db::run_all`] runs its tasks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run {
    /// As plane tasks on the process's one worker pool (`blaze_graph::exec::shared`).
    Pool,
    /// On the calling thread, in the order [`RunOrder`] says.
    Here,
}

/// The order a database made by [`Db::for_tests`] runs a batch of [`Db::run_all`] in. It has
/// no plane: the tasks run on the calling thread, and tests vary the order to show that no
/// result depends on it, as none may on the plane's scheduling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOrder {
    InOrder,
    Reversed,
    /// Every `n`th task from the first, then every `n`th from the second, and so on.
    Strided(usize),
    /// None of them: each result is computed by the query that reads it, as it reads it.
    LeftToReaders,
    /// As a build's database runs them: on the pool this thread is a worker of (the process's
    /// from a thread of no pool), through `blaze_graph::par`'s loops, so a test meets what a
    /// task giving way on the loop's own thread does there. A test runs under a pool of one
    /// worker (`Pool::install`) to have every task on that thread, in order.
    OnPool,
}

thread_local! {
    static RUN_ORDER: Cell<RunOrder> = const { Cell::new(RunOrder::InOrder) };
}

/// Has the test databases of this thread run their batches in `order` from now on, and
/// returns the order they ran them in until now.
pub fn set_run_order(order: RunOrder) -> RunOrder {
    RUN_ORDER.replace(order)
}

mod shared;
#[cfg(test)]
mod tests;

pub use shared::{Reader, Shared};

/// The scheduler's single flight: a database that several tasks drive.
fn scheduled() -> std::sync::Arc<dyn blaze_memo::Flight, Global> {
    std::sync::Arc::new_in(blaze_memo::Scheduled::new(), Global)
}

/// One task's single flight: a test's database, which one task drives.
fn alone() -> std::sync::Arc<dyn blaze_memo::Flight, Global> {
    std::sync::Arc::new_in(blaze_memo::Alone::new(), Global)
}

/// Whether `payload` is a task's give-way ([`blaze_memo::Cancelled::GaveWay`]).
fn gave_way(payload: &(dyn std::any::Any + Send)) -> bool {
    matches!(payload.downcast_ref::<blaze_memo::Cancelled>(), Some(blaze_memo::Cancelled::GaveWay))
}

/// `task(at)` for each `at` in `0..count`, each run in a job on the pool this thread's loops
/// run on. A task that gave way ([`blaze_memo::Cancelled::GaveWay`]: its wait would have
/// closed a cycle of tasks) runs again here once the others ended, on this thread, where it
/// meets the cycle on its own stack; a rerun that gives way again unwinds to this caller. The
/// first other panic of a job is raised once every job has ended.
pub fn par_for_each(count: usize, task: impl Fn(usize) + Sync) {
    let again = blaze_graph::par::for_each_giving_way(count, &task, gave_way);
    for at in again {
        task(at);
    }
}

/// [`par_for_each`], run on the calling thread alone where `small` predicts that the loop
/// costs less than its dispatch ([`blaze_graph::par::Small`]).
pub fn par_for_each_small(small: &blaze_graph::par::Small, count: usize, task: impl Fn(usize) + Sync) {
    if count == 0 {
        return;
    }
    let here = small.here(count);
    let began = std::time::Instant::now();
    if here {
        let mut again: Vec<usize, Global> = Vec::new_in(Global);
        for at in 0..count {
            let ran = panic::catch_unwind(panic::AssertUnwindSafe(|| task(at)));
            let Err(payload) = ran else { continue };
            match gave_way(&*payload) {
                true => {
                    again.push(at);
                    // As the pool's loop: the next task is not the same ask again.
                    blaze_graph::pump::rerun_later();
                }
                false => panic::resume_unwind(payload),
            }
        }
        for at in again {
            task(at);
        }
    } else {
        par_for_each(count, task);
    }
    small.ran(count, began.elapsed().as_nanos() as u64, here);
}

/// `f(at)` for each `at` in `0..count`, in order, run as [`par_for_each`] runs its tasks.
#[expect(clippy::disallowed_types, reason = "a result's slot, each locked once by the task that fills it")]
pub fn par_map<R: Send>(count: usize, f: impl Fn(usize) -> R + Sync) -> Vec<R> {
    let mut made: Vec<blaze_core::lock::Lock<Option<R>>, Global> = Vec::with_capacity_in(count, Global);
    made.extend((0..count).map(|_| blaze_core::lock::Lock::new("db.par_map.slot", None)));
    par_for_each(count, |at| {
        let it = f(at);
        *made[at].lock() = Some(it);
    });
    let mut out = Vec::with_capacity_in(count, Global);
    out.extend(made.into_iter().map(|it| it.into_inner().expect("every task ran")));
    out
}

/// What a commit did ([`Edit::commit`]).
#[derive(Debug)]
pub struct Committed {
    /// The keys of the crates of the last graph set in the edit ([`Edit::set_crate_graph`]),
    /// empty when none was.
    pub crates: CratesIdMap,
    /// The arena indices the commit made again.
    pub dropped: Box<[u32]>,
    /// Whether the edit moved anything, so a snapshot was made: false for one that wrote
    /// nothing or only values the world held, which keeps the snapshot and its epoch.
    pub moved: bool,
}

impl Db {
    /// The database of a build: no crate, no source root, and its batches run as plane tasks
    /// of the process's pool, its first (empty) snapshot committed. `boundaries` marks the
    /// session's fact boundaries on each session's first builder: a build passes
    /// `blaze_item_hash::reads::mark_boundaries`. `reads` is whether its queries keep read lists
    /// ([`blaze_memo::record`] answers only under [`blaze_memo::Reads::Kept`]): a build that
    /// files body records keeps them, every other build passes [`blaze_memo::Reads::Off`].
    pub fn new(boundaries: fn(&mut blaze_memo::Builder), reads: blaze_memo::Reads) -> Db {
        Db::with(Run::Pool, scheduled(), None, boundaries, reads, Files::default())
    }

    /// A build's database ([`Db::new`]) opened as a snapshot: one with no [`crate::Disk`], so
    /// nothing it runs reads a file from disk (rule 14). Its files are those an edit gives it;
    /// a `mod` whose file is not among them is an unresolved module and an `include!` of one
    /// the include's error, each naming the path, never a read. What a command's snapshot of a
    /// unit is (blaze-command `snapshot::open`). `reads` as for [`Db::new`].
    pub fn sealed(boundaries: fn(&mut blaze_memo::Builder), reads: blaze_memo::Reads) -> Db {
        Db::with(Run::Pool, scheduled(), None, boundaries, reads, Files::sealed())
    }

    /// [`Db::sealed`] that keeps a run log, as [`Db::logged`] is to [`Db::new`]: a command's
    /// snapshot under its tests.
    pub fn sealed_logged(boundaries: fn(&mut blaze_memo::Builder), reads: blaze_memo::Reads) -> Db {
        let log = std::sync::Arc::new_in(blaze_memo::testing::RunLog::new(), Global);
        Db::with(Run::Pool, scheduled(), Some(log), boundaries, reads, Files::sealed())
    }

    /// A database for tests ([`Db::for_tests`]) with no [`crate::Disk`], as [`Db::sealed`]: a
    /// test of what a snapshot does where a file is not among its files.
    pub fn for_tests_sealed(boundaries: fn(&mut blaze_memo::Builder)) -> Db {
        let log = std::sync::Arc::new_in(blaze_memo::testing::RunLog::new(), Global);
        Db::with(Run::Here, alone(), Some(log), boundaries, blaze_memo::Reads::Off, Files::sealed())
    }

    /// A build's database ([`Db::new`]) that keeps a [`blaze_memo::testing::RunLog`] of the
    /// queries it runs ([`Db::runs`]): for a harness that counts what a request of a real build
    /// runs (`crates/hir-firewall`). `reads` as for [`Db::new`].
    pub fn logged(boundaries: fn(&mut blaze_memo::Builder), reads: blaze_memo::Reads) -> Db {
        let log = std::sync::Arc::new_in(blaze_memo::testing::RunLog::new(), Global);
        Db::with(Run::Pool, scheduled(), Some(log), boundaries, reads, Files::default())
    }

    /// A database for tests: its batches run on the calling thread in this thread's
    /// [`RunOrder`], it keeps a run log ([`Db::runs`]), and it has empty library and local
    /// roots and an empty crate graph, committed, as a fixture expects to find them. It keeps
    /// no read lists ([`blaze_memo::Reads::Off`]); a test that checks them takes
    /// [`Db::for_tests_with_reads`].
    ///
    /// One task drives it: its single flight is one task's ([`blaze_memo::Alone`]), which
    /// refuses, naming the key, an ask by a second task of a key the first one runs. A test that
    /// several tasks or threads drive (a build driver on a pool, a simulation's tasks) takes
    /// [`Db::for_tests_driven`].
    pub fn for_tests(boundaries: fn(&mut blaze_memo::Builder)) -> Db {
        let log = std::sync::Arc::new_in(blaze_memo::testing::RunLog::new(), Global);
        Db::with(Run::Here, alone(), Some(log), boundaries, blaze_memo::Reads::Off, Files::default())
    }

    /// A database for tests ([`Db::for_tests`]) whose queries keep read lists
    /// ([`blaze_memo::Reads::Kept`]), so [`blaze_memo::record`] answers on it: for a test that
    /// checks what a query read.
    pub fn for_tests_with_reads(boundaries: fn(&mut blaze_memo::Builder)) -> Db {
        let log = std::sync::Arc::new_in(blaze_memo::testing::RunLog::new(), Global);
        Db::with(Run::Here, alone(), Some(log), boundaries, blaze_memo::Reads::Kept, Files::default())
    }

    /// A database for tests ([`Db::for_tests`]: its batches in this thread's [`RunOrder`], a
    /// run log, `reads` as for [`Db::new`]) that several tasks or threads drive at once: a
    /// build driver on a pool, a simulation's tasks. Its single flight is the scheduler's
    /// ([`blaze_memo::Scheduled`]), as a build's database's is: an asker of a key another task
    /// runs waits for that run and gets its value.
    pub fn for_tests_driven(
        boundaries: fn(&mut blaze_memo::Builder),
        reads: blaze_memo::Reads,
    ) -> Db {
        let log = std::sync::Arc::new_in(blaze_memo::testing::RunLog::new(), Global);
        Db::with(Run::Here, scheduled(), Some(log), boundaries, reads, Files::default())
    }

    /// `files`: the database's files, with a [`crate::Disk`] or with none ([`Db::sealed`]).
    fn with(
        run: Run,
        flight: std::sync::Arc<dyn blaze_memo::Flight, Global>,
        log: Option<std::sync::Arc<blaze_memo::testing::RunLog, Global>>,
        boundaries: fn(&mut blaze_memo::Builder),
        reads: blaze_memo::Reads,
        files: Files,
    ) -> Db {
        let mut world = World::new(boundaries, log, flight, reads);
        let files: Arc<Files> = Arc::new(files);
        // Empty roots and an empty graph: what a fixture expects to find, and a build's first
        // snapshot holds the same.
        world::settle(&mut world, &files);
        let made = world::commit(&mut world, &files, None);
        world.arenas = made.arenas.expect("a first commit starts a session");
        files.clear_marks();
        files.forget();
        world.forget();
        Db {
            snap: std::sync::Arc::new_in(made.snap, Global),
            files,
            world: Some(Box::new_in(world, Global)),
            nonce: Nonce::new(),
            run,
        }
    }

    /// The edit that writes the world: nothing a query reads changes until
    /// [`Edit::commit`].
    ///
    /// # Panics
    ///
    /// On a [`Reader`]'s database, which holds a snapshot and no world.
    pub fn edit(&mut self) -> Edit<'_> {
        assert!(self.world.is_some(), "`Db::edit` on a reader's database: it holds no world");
        self.files.begin();
        Edit { db: self, crates: None, done: false }
    }

    fn world(&self) -> &World {
        self.world.as_deref().expect("a reader's database holds no world")
    }

    fn world_mut(&mut self) -> &mut World {
        self.world.as_deref_mut().expect("a reader's database holds no world")
    }

    /// This database's snapshot.
    #[inline]
    pub(crate) fn snap(&self) -> &Snap {
        &self.snap
    }

    /// The commits before this snapshot: what a file read outside the VFS is versioned by.
    #[inline]
    pub(crate) fn seq(&self) -> u64 {
        self.snap.seq
    }

    /// The crate the world has as `key`, in this snapshot.
    ///
    /// # Panics
    ///
    /// Where this snapshot has no such crate.
    #[inline]
    pub fn krate(&self, key: CrateKey) -> Crate {
        match self.snap.table.crates.get(key.0 as usize) {
            Some(Some(krate)) => *krate,
            _ => panic!("this snapshot has no crate {key:?}"),
        }
    }

    /// The crate the world has as `key` in this snapshot, or `None` where it has none: a key
    /// kept across a commit that removed its crate. What a holder of keys from an earlier
    /// snapshot asks, so that a stale key is its typed error and never a panic.
    #[inline]
    pub fn crate_at(&self, key: CrateKey) -> Option<Crate> {
        self.snap.table.crates.get(key.0 as usize).copied().flatten()
    }

    /// The epoch of this snapshot: what tells a cached answer of another snapshot apart.
    #[inline]
    pub fn snapshot_epoch(&self) -> blaze_memo::Epoch {
        self.snap.memo.snapshot_epoch()
    }

    /// This database's nonce: another for every other `Db` of the process, and the same for
    /// one `Db` across every commit while its id space lasts. What tells two databases apart,
    /// and what a holder of ids outside the memo keys by: within one nonce an
    /// [`blaze_memo::ArenaId`] (and so a [`blaze_memo::Gid`]) names one making of an arena
    /// for ever, across every commit and every new session (blaze-memo's id space, which a
    /// new session goes on in). The one commit that starts a new id space, once an arena
    /// index has made its last generation in the old one ([`blaze_memo::MAX_GENERATION`]` + 1`
    /// makings of one index in the database's life), gives the database another nonce, and
    /// so a reader's copy made after it has the new one with the new snapshot.
    #[inline]
    pub fn nonce(&self) -> Nonce {
        self.nonce
    }

    /// The queries run since the log was last taken, on a database made by [`Db::for_tests`]
    /// or [`Db::logged`]; `None` on a build's.
    pub fn runs(&self) -> Option<&blaze_memo::testing::RunLog> {
        self.world().log.as_deref()
    }

    /// The files, source roots and what was read from outside them.
    #[inline]
    pub fn files(&self) -> &Files {
        &self.files
    }

    /// [`Db::files`] to keep past a borrow of the database: the one table every reader of it
    /// shares, so a file it gains later is there too.
    #[inline]
    pub fn shared_files(&self) -> Arc<Files> {
        Arc::clone(&self.files)
    }

    /// The text of the file.
    ///
    /// # Panics
    ///
    /// Where this snapshot has no text of it.
    #[inline]
    pub fn file_text(&self, file_id: FileId) -> FileText {
        self.files.file_text(self, file_id)
    }

    /// Whether [`Db::file_text`] has the file: one that is not UTF-8 has no text.
    #[inline]
    pub fn has_file_text(&self, file_id: FileId) -> bool {
        self.files.has_file_text(self, file_id)
    }

    /// [`Files::raw_file`]: the bytes of a file of the VFS that has no text.
    #[inline]
    pub fn raw_file(&self, file_id: FileId) -> Option<IncludedFile> {
        self.files.raw_file(self, file_id)
    }

    /// [`Files::file_content`]: the content ID a file has in its source root, text or bytes.
    #[inline]
    pub fn file_content(&self, file_id: FileId) -> ContentId {
        self.files.file_content(self, file_id)
    }

    /// Contents of the source root.
    #[inline]
    pub fn source_root(&self, id: SourceRootId) -> SourceRootInput {
        self.files.source_root(self, id)
    }

    /// Source root of the file.
    #[inline]
    pub fn file_source_root(&self, id: FileId) -> FileSourceRootInput {
        self.files.file_source_root(self, id)
    }

    /// The file of the VFS that `path` names relative to its anchor: in the anchor's source
    /// root, or else one another source root names (a `#[path]` module outside its package),
    /// tracked through [`crate::named`] so a root that names it later reruns the reader.
    pub fn resolve_path(&self, path: AnchoredPath<'_>) -> Option<FileId> {
        let root = self.file_source_root(path.anchor).source_root_id(self);
        let root = self.source_root(root).source_root_ref(self);
        if let Some(file) = root.resolve_path(path) {
            return Some(file);
        }
        // clone: only for a path outside the anchor's source root, rare; `join` makes one too.
        let mut target = root.path_for_file(&path.anchor)?.clone();
        target.pop();
        let target = target.join(path.path)?;
        let file = crate::named::lookup(self, &target)?;
        let there = self.source_root(self.file_source_root(file).source_root_id(self));
        (there.source_root_ref(self).file_for_path(&target) == Some(&file)).then_some(file)
    }

    /// The file a source root names at `path`, tracked as [`Db::resolve_path`]'s lookup outside
    /// the anchor's root is.
    pub fn file_for_path(&self, path: &VfsPath) -> Option<FileId> {
        let file = crate::named::lookup(self, path)?;
        let there = self.source_root(self.file_source_root(file).source_root_id(self));
        (there.source_root_ref(self).file_for_path(path) == Some(&file)).then_some(file)
    }

    /// The path the file's source root has it at.
    pub fn path_for_file(&self, file: FileId) -> Option<&VfsPath> {
        let root = self.source_root(self.file_source_root(file).source_root_id(self));
        root.source_root_ref(self).path_for_file(&file)
    }

    /// The file `path` names relative to its anchor, when it is not one of the files of the
    /// VFS, as `krate` reads it (`include_bytes!`, `include_str!`): `None` when the anchor has
    /// no path on disk to be relative to, or the database has no disk ([`Db::sealed`]).
    pub fn included_file(&self, krate: Crate, path: AnchoredPath<'_>) -> Option<IncludedFile> {
        let disk = self.files.disk()?;
        let root = self.source_root(self.file_source_root(path.anchor).source_root_id(self));
        let root = root.source_root_ref(self);
        let anchor = root.path_for_file(&path.anchor)?.as_path()?;
        let path = anchor.parent()?.absolutize(path.path);
        Some(self.files.included_file(disk, self, krate.key(self), &path))
    }

    /// The file outside the VFS that `path` names relative to its anchor, as `include!` in
    /// `krate` parses it ([`Files::included_source`]), with the read it is made from: `None`
    /// when the anchor has no path on disk to be relative to, or the database has no disk
    /// ([`Db::sealed`]). rustc's `resolve_path`: a relative path is joined to the directory of
    /// the file the call is in, an absolute one is taken as it is; blaze names the file by that
    /// path normalized, as the VFS names its own.
    pub fn included_source(
        &self,
        krate: Crate,
        path: AnchoredPath<'_>,
    ) -> Option<(IncludedFile, Result<FileId, FileId>)> {
        let disk = self.files.disk()?;
        let root = self.source_root(self.file_source_root(path.anchor).source_root_id(self));
        let root = root.source_root_ref(self);
        let anchor = root.path_for_file(&path.anchor)?.as_path()?;
        let path = anchor.parent()?.absolutize(path.path);
        Some(self.files.included_source(disk, self, krate.key(self), &path, root.is_library))
    }

    /// The file outside the VFS that a module declaration (`mod m;`) in `krate` names at `path`
    /// relative to its anchor, read from disk as rustc reads every module's file: a `#[path]`
    /// module outside every loaded source root, or a module of such a file. Made as `include!`
    /// makes its file ([`Db::included_source`]): the read is noted as `krate`'s
    /// ([`crate::Files::included_by`], which its keys cover) and the caller reads it, so a file
    /// that appears, goes or is edited reruns it once whoever owns the database reads the disk
    /// again ([`Edit::reload_included_files`]). `None` when there is no module file to parse
    /// there: the anchor has no path on disk, the read fails (no file there), the bytes are not
    /// UTF-8, the file ID it would have is taken by a file of the view, or the database has no
    /// disk ([`Db::sealed`]: a snapshot's module files are the unit's files and no other); the
    /// declaration is then an unresolved module, an error naming the paths looked for.
    pub fn module_source(&self, krate: Crate, path: AnchoredPath<'_>) -> Option<FileId> {
        let (file, source) = self.included_source(krate, path)?;
        std::str::from_utf8(file.bytes(self)?).ok()?;
        source.ok()
    }

    /// The path [`Db::included_file`] reads, as rustc writes it in an error: the anchor's
    /// directory joined with `path`, not normalized.
    pub fn included_path(&self, path: AnchoredPath<'_>) -> Option<AbsPathBuf> {
        let root = self.source_root(self.file_source_root(path.anchor).source_root_id(self));
        let root = root.source_root_ref(self);
        let anchor = root.path_for_file(&path.anchor)?.as_path()?;
        Some(anchor.parent()?.join(path.path))
    }

    /// Notes that `krate` names a file [`Db::resolve_path`] has nothing for, for whoever owns
    /// the VFS to load: a frozen def map's file that is not loaded. An anchor with no path on
    /// disk has no file next to it to load, so nothing is noted.
    pub fn include_source(&self, path: AnchoredPath<'_>, krate: Crate) {
        let root_id = self.file_source_root(path.anchor).source_root_id(self);
        let root = self.source_root(root_id);
        let root = root.source_root_ref(self);
        if let Some(anchor) = root.path_for_file(&path.anchor).and_then(|it| it.as_path())
            && let Some(parent) = anchor.parent()
        {
            self.files.include_source(&parent.absolutize(path.path), root_id, krate.key(self));
        }
    }

    /// Notes that `env!` or `option_env!` asked for the variable `key` in `krate`.
    #[inline]
    pub fn env_is_read(&self, krate: Crate, key: &str) {
        self.files.env_is_read(krate.key(self), key);
    }

    /// Notes that `env!` in `krate` found the variable `key` unset, which rustc turns the
    /// crate down for.
    #[inline]
    pub fn env_is_unset(&self, krate: Crate, key: &str) {
        self.files.env_is_unset(krate.key(self), key);
    }

    /// Runs `task(db, i)` for every `i` in `0..count`, and returns once each has run: the
    /// tasks of a round of macro expansion, or the def maps of a crate's dependencies. Each
    /// task asks queries and nothing else, so neither the order nor the threads they run on
    /// changes a result, and the caller reads what they computed afterwards in its own order.
    ///
    /// A build's database runs them as plane tasks on the one worker pool, each reading this
    /// database by reference, and the calling thread pump-waits for them. A test database runs
    /// them here, in [`RunOrder`].
    ///
    /// Each task runs in a frame of its own, tagged `tag` (the caller's subsystem), with the
    /// database attached. On the pool, a task that gave way ([`blaze_memo::Cancelled::GaveWay`])
    /// runs again on this thread once the others ended ([`par_for_each`]), and the first other
    /// panic of a task goes on from here once every task has run.
    ///
    /// The call is a span `run_all` and each task a span `run_all task`, both naming the
    /// caller's file and line in `function`, so a profile says whose tasks a stretch ran.
    #[track_caller]
    pub fn run_all<F: Fn(&Db, usize) + Sync>(&self, tag: Tag, count: usize, task: F) {
        self.run_tasks(std::panic::Location::caller(), count, false, None, |db, at| {
            frame::task(tag, |_| task(db, at))
        });
    }

    /// [`Db::run_all`] for tasks that may be an edit's few: where `small` predicts the batch
    /// costs less than its dispatch they run on the calling thread, one after the other, with
    /// no job submitted ([`par_for_each_small`]); a larger or dearer batch runs as
    /// [`Db::run_all`]'s does. A test database runs them here in [`RunOrder`] either way, and
    /// under [`RunOrder::OnPool`] as a build's database does.
    #[track_caller]
    pub fn run_all_small<F: Fn(&Db, usize) + Sync>(
        &self,
        small: &blaze_graph::par::Small,
        tag: Tag,
        count: usize,
        task: F,
    ) {
        self.run_tasks(std::panic::Location::caller(), count, false, Some(small), |db, at| {
            frame::task(tag, |_| task(db, at))
        });
    }

    /// [`Db::run_all`] for tasks every one of which must run, under
    /// [`RunOrder::LeftToReaders`] too (in order): what they compute is their result, and no
    /// reader asks for it.
    #[track_caller]
    pub fn run_every<F: Fn(&Db, usize) + Sync>(&self, count: usize, task: F) {
        self.run_tasks(std::panic::Location::caller(), count, true, None, task);
    }

    /// Runs `task(db, item)` on every one of `items` at once, each in a frame of its own tagged
    /// `tag`, and returns once each has run. Every task runs, under
    /// [`RunOrder::LeftToReaders`] too (in order). A build's database runs them on the pool
    /// (`blaze_graph::par`); a test database runs them here, in [`RunOrder`] (on the pool too
    /// under [`RunOrder::OnPool`]). As the pool's loop does, a task that gave way
    /// ([`blaze_memo::Cancelled::GaveWay`]) runs again once the others have, in index order,
    /// and a give-way of that second run goes on to the caller.
    pub fn run_each<T: Send>(&self, tag: Tag, items: &mut [T], task: impl Fn(&Db, &mut T) + Sync) {
        let run = |item: &mut T| frame::task(tag, |_| blaze_memo::attach(self, || task(self, item)));
        let order = RUN_ORDER.get();
        match self.run {
            Run::Pool => blaze_graph::par::for_each(items, run),
            Run::Here if order == RunOrder::OnPool => blaze_graph::par::for_each(items, run),
            Run::Here => {
                let mut again: Vec<usize, Global> = Vec::new_in(Global);
                let mut once = |at: usize, item: &mut T| {
                    let ran = panic::catch_unwind(panic::AssertUnwindSafe(|| run(item)));
                    if let Err(payload) = ran {
                        match gave_way(&*payload) {
                            true => {
                                again.push(at);
                                // As the pool's loop: the next item is not the same ask again.
                                blaze_graph::pump::rerun_later();
                            }
                            false => panic::resume_unwind(payload),
                        }
                    }
                };
                match order {
                    RunOrder::InOrder | RunOrder::LeftToReaders | RunOrder::OnPool => {
                        items.iter_mut().enumerate().for_each(|(at, item)| once(at, item));
                    }
                    RunOrder::Reversed => {
                        items.iter_mut().enumerate().rev().for_each(|(at, item)| once(at, item))
                    }
                    RunOrder::Strided(stride) => {
                        let stride = stride.max(1);
                        for first in 0..stride {
                            items
                                .iter_mut()
                                .enumerate()
                                .skip(first)
                                .step_by(stride)
                                .for_each(|(at, item)| once(at, item));
                        }
                    }
                }
                again.sort_unstable();
                for at in again {
                    run(&mut items[at]);
                }
            }
        }
    }

    fn run_tasks<F: Fn(&Db, usize) + Sync>(
        &self,
        caller: &'static std::panic::Location<'static>,
        count: usize,
        every: bool,
        small: Option<&blaze_graph::par::Small>,
        task: F,
    ) {
        if count == 0 {
            return;
        }
        // Whether the batch runs on this thread, with no job submitted: said on the span, so
        // a profile tells an edit's inline chain from a dispatch (rule 16).
        let here = small.is_some_and(|small| small.here(count));
        let all = tracing::info_span!(target: "blaze", "run_all", function = tracing::field::Empty, count, here);
        // Only while a subscriber records the span, which interns it.
        let site = (!all.is_disabled()).then(|| format!("{}:{}", caller.file(), caller.line()));
        if let Some(site) = &site {
            all.record("function", site.as_str());
        }
        let _all = all.entered();
        // Each task with its database attached, as a query's run is: it runs between queries,
        // on whichever worker, and formats against the database it was given.
        let run = |db: &Db, at: usize| {
            let span =
                tracing::info_span!(target: "blaze", "run_all task", function = site.as_deref());
            let _span = span.entered();
            blaze_memo::attach(db, || task(db, at))
        };
        let pooled = |count: usize| match small {
            Some(small) => par_for_each_small(small, count, |at| run(self, at)),
            None => par_for_each(count, |at| run(self, at)),
        };
        match self.run {
            Run::Pool => pooled(count),
            Run::Here => match RUN_ORDER.get() {
                RunOrder::OnPool => pooled(count),
                RunOrder::InOrder => (0..count).for_each(|at| run(self, at)),
                RunOrder::Reversed => (0..count).rev().for_each(|at| run(self, at)),
                RunOrder::Strided(stride) => {
                    let stride = stride.max(1);
                    (0..stride)
                        .flat_map(|first| (first..count).step_by(stride))
                        .for_each(|at| run(self, at));
                }
                RunOrder::LeftToReaders if every => (0..count).for_each(|at| run(self, at)),
                RunOrder::LeftToReaders => {}
            },
        }
    }

    /// The line and column of `offset` in `file`, both from 0, as rustc counts them (columns
    /// in chars): what a proc macro's `Span::line` and `Span::column` answer, less 1. `Err`
    /// past the text's end, and in a text with nothing after its byte order mark, where rustc
    /// says line 0.
    pub fn line_column(&self, file: FileId, offset: TextSize) -> Result<(u32, u32), ()> {
        let text = self.file_text(file);
        let place = crate::line_index(self, text).place(offset).ok_or(())?;
        let column = place.char_column(text.text(self)).ok_or(())?;
        Ok((place.line().checked_sub(1).ok_or(())?, column - 1))
    }

    /// A reader's copy: the same snapshot and files, no world.
    pub(crate) fn reader_copy(&self) -> Db {
        Db {
            snap: std::sync::Arc::clone(&self.snap),
            files: Arc::clone(&self.files),
            world: None,
            nonce: self.nonce,
            run: self.run,
        }
    }

    /// Makes the next snapshot from the world ([`world::commit`]), when the edit moved
    /// anything; one that moved nothing keeps the snapshot (and its epoch). Every write of the
    /// edit is kept after this ([`Files::forget`], [`World::forget`]); a panic in it (a maker's)
    /// leaves them to the edit's drop, which puts the world back as the last commit left it.
    fn commit(&mut self, crates: Option<CratesIdMap>) -> Committed {
        let span = tracing::info_span!(
            target: "blaze",
            "db commit",
            moved = tracing::field::Empty,
            dropped = tracing::field::Empty,
            fresh = tracing::field::Empty,
            marked = tracing::field::Empty,
            causes = tracing::field::Empty,
        )
        .entered();
        let crates = crates.unwrap_or_else(|| CratesIdMap::new_in(Global));
        let world = self.world.as_deref_mut().expect("a reader's database holds no world");
        world::settle(world, &self.files);
        let moved = self.files.has_marks() || world.moved();
        span.record("moved", moved);
        if !moved {
            // Bytes set for a file outside the VFS that no crate read: a row of them is made,
            // which no query of this snapshot read, so it stays.
            self.files.publish_included(self.snap.seq);
            self.files.forget();
            world.forget();
            return Committed { crates, dropped: Box::default(), moved: false };
        }
        // The old snapshot's table is taken over in place when no reader holds it.
        let made = world::commit(world, &self.files, Some(&mut self.snap));
        span.record("dropped", made.dropped.len() as u64);
        // Why every arena was made again, when the commit carried none (`world::Fresh`), and
        // each arena marked with why: the first `world::MARKS_NAMED`, then a count. Written
        // only when the span is recorded.
        span.record("marked", made.marked.len() as u64);
        if !span.is_disabled() {
            if let Some(fresh) = made.fresh {
                span.record("fresh", tracing::field::display(fresh));
            }
            let mut causes: Vec<u8, Global> = Vec::new_in(Global);
            world::describe(world, &made.marked, made.fresh, &mut causes)
                .expect("writing to a Vec does not fail");
            span.record("causes", &*String::from_utf8_lossy(&causes));
        }
        if let Some(arenas) = made.arenas {
            world.arenas = arenas;
        }
        let seq = made.snap.seq;
        // A new id space: its ids may be ones an earlier snapshot of this database handed out,
        // so the database is another to every holder of ids, which keys by its nonce.
        if made.renewed {
            self.nonce = Nonce::new();
        }
        // The old snapshot is released by the assignment below: with it, every entry the
        // dropped arenas held (a sealed std crate's def maps, bodies and types among them).
        // Its own span, so a commit's time splits into making the next snapshot (`db commit
        // make`, in `world::commit`) and freeing what the old one dropped.
        let _release = tracing::info_span!(target: "blaze", "db commit release", dropped = made.dropped.len() as u64)
            .entered();
        // The old snapshot released here (unless a reader holds it), so the file sets the edit
        // changed where they are are written in place in the new one's inputs, before any
        // query runs on it: nothing after this point can fail, so a commit that panicked
        // above left every set as the old snapshot reads it, and its edit's drop clears the
        // pending changes (`Files::apply_pending`).
        self.snap = std::sync::Arc::new_in(made.snap, Global);
        self.files.apply_pending(&mut self.snap);
        self.files.clear_marks();
        self.files.publish_included(seq);
        self.files.forget();
        world.forget();
        drop(_release);
        Committed { crates, dropped: made.dropped, moved: true }
    }

    /// Plants a bug for a test's control ([`Plant`]); [`Plant::None`] everywhere else.
    #[doc(hidden)]
    pub fn plant(&mut self, plant: Plant) {
        self.world_mut().plant = plant;
    }
}

/// An edit of the world. See [`Db::edit`].
#[must_use = "an edit changes nothing until it is committed"]
pub struct Edit<'db> {
    db: &'db mut Db,
    /// The keys of the last graph set in this edit.
    crates: Option<CratesIdMap>,
    /// Whether [`Edit::commit`] kept its writes; its drop puts them back otherwise.
    done: bool,
}

impl Edit<'_> {
    fn files(&self) -> Arc<Files> {
        Arc::clone(&self.db.files)
    }

    fn world(&mut self) -> &mut World {
        self.db.world_mut()
    }

    /// The database as it was at the last commit: what a setter that reads before it writes
    /// reads.
    pub fn db(&self) -> &Db {
        self.db
    }

    /// The commit this edit's writes are stamped with: the next snapshot's.
    fn seq(&self) -> u64 {
        self.db.seq() + 1
    }

    /// The source root `id` as the world has it now, with this edit's writes (a root set, a
    /// view's files placed): what a writer that finds a file by its path between two of its
    /// writes reads, where [`Edit::db`] has the last commit's. None for a root never set.
    pub fn written_source_root(&self, id: SourceRootId) -> Option<Arc<SourceRoot>> {
        self.db.files.written_root(id)
    }

    /// `file`'s text and its [`FileText::content`] as the world has them now, with this
    /// edit's writes; none for a file with no text (never given one, its text taken away, or
    /// bytes that are not UTF-8).
    pub fn written_file_text(&self, file: FileId) -> Option<(syntax::Text, ContentId)> {
        self.db.files.written_text(file)
    }

    /// `krate`'s fields as the world has them now, with this edit's writes (a crate this edit
    /// made among them): what a writer that decides from them whether its own write moves
    /// anything, or that makes a crate from another's, reads.
    ///
    /// # Panics
    ///
    /// Where the world has no crate `krate`.
    pub fn written_crate(&self, krate: CrateKey) -> WrittenCrate<'_> {
        let entry = self.db.world().crates.get(krate);
        WrittenCrate {
            data: &entry.data,
            extra: &entry.extra,
            workspace: &entry.workspace,
            cfg: &entry.cfg,
            env: &entry.env,
        }
    }

    /// The crates the crate graph lists ([`crate::all_crates`]) as the world has them now, with
    /// this edit's writes, in the graph's order: what a writer that takes some out
    /// ([`Edit::set_all_crates`]) starts from.
    pub fn written_all_crates(&self) -> Vec<CrateKey, Global> {
        let crates = &self.db.world().crates;
        let mut listed = Vec::new_in(Global);
        listed.extend(crates.keys().filter(|&key| crates.get(key).listed));
        listed
    }

    /// The crates of the primary packages ([`crate::primary_crates`]) as the world has them
    /// now, with this edit's writes: sorted, each once.
    pub fn written_primary_crates(&self) -> &[CrateKey] {
        &self.db.world().session.primary
    }

    /// The gift of kind `K` that `krate` holds as the world has it now, with this edit's writes
    /// ([`Edit::give`]), as a `T`: none where it has no gift of that kind, or one of another
    /// type. What a giver that compares before it gives reads, so a gift given earlier in the
    /// same edit counts.
    ///
    /// # Panics
    ///
    /// Where the world has no crate `krate`.
    pub fn written_given<K: 'static, T: std::any::Any>(&self, krate: CrateKey) -> Option<&T> {
        self.db.world().crates.gift(krate, std::any::TypeId::of::<K>())?.get::<T>()
    }

    pub fn set_file_text(&mut self, file: FileId, text: &str) -> &mut Self {
        self.db.files.set_file_text(file, text);
        self
    }

    pub fn set_file_source_root(&mut self, file: FileId, root: SourceRootId) -> &mut Self {
        self.db.files.set_file_source_root(file, root);
        self
    }

    /// See [`Files::set_file_given`]: `file` known by `content` alone (a file of a crate given
    /// by its address), or no longer (`None`).
    pub fn set_file_given(&mut self, file: FileId, content: Option<ContentId>) -> &mut Self {
        self.db.files.set_file_given(file, content);
        self
    }

    pub fn set_source_root(&mut self, id: SourceRootId, root: Arc<SourceRoot>) -> &mut Self {
        self.db.files.set_source_root(id, root);
        self
    }

    /// Puts each of `files` in the source root `id` at its path, the root's other files kept:
    /// the set as this edit has it, copied once, each file inserted ([`SourceRoot::insert_file`]'s
    /// rules: a path names the last file put at it), set again. What a crate given by its
    /// address names its files by (`hir_def::deps::give_keyed`), so a path resolved from one of
    /// them (an `include!`, a `#[path]`) finds the file its pages name, never a file on disk.
    /// Cost: one copy of the root's set an edit, O(its files + `files`).
    ///
    /// # Panics
    ///
    /// Where the world has no source root `id`.
    pub fn put_in_source_root(&mut self, id: SourceRootId, files: impl IntoIterator<Item = (FileId, VfsPath)>) -> &mut Self {
        let written = self.db.files.written_root(id).unwrap_or_else(|| panic!("no source root {id:?} to put files in"));
        // clone: the root's set, once an edit that puts files in it
        let mut root: SourceRoot = (*written).clone();
        for (file, path) in files {
            root.insert_file(file, path);
        }
        self.set_source_root(id, Arc::new(root))
    }

    /// See [`Files::set_appended_root_module`].
    pub fn set_appended_root_module(&mut self, file: FileId, module: Option<&'static str>) -> &mut Self {
        self.db.files.set_appended_root_module(file, module);
        self
    }

    /// A new file with `of`'s path and text, whose appended root module is its own: see
    /// [`Files::alias`].
    pub fn alias(&mut self, of: FileId) -> Result<FileId, crate::AliasError> {
        self.db.files.alias(of)
    }

    /// Has `alias` append a module (`appended`: its name, and the text after its file's) to its
    /// file's text, or nothing: see [`Files::set_alias_appended`].
    pub fn set_alias_appended(
        &mut self,
        alias: FileId,
        appended: Option<(&'static str, &str)>,
    ) -> Result<(), crate::AliasError> {
        self.db.files.set_alias_appended(alias, appended)
    }

    /// Sets the inputs of one view's files, writing only those that changed: see
    /// [`Files::set_view_root`].
    pub fn set_view_root(&mut self, view: ViewRoot<'_>) -> Result<ViewChange, ViewError> {
        self.db.files.set_view_root(view)
    }

    /// [`Edit::set_view_root`] for a view read from a tree, whose IDs are not hashed again: see
    /// [`Files::set_view_root_from_tree`].
    pub fn set_view_root_from_tree(&mut self, view: ViewRoot<'_>) -> Result<ViewChange, ViewError> {
        self.db.files.set_view_root_from_tree(view)
    }

    /// [`Files::new_file_id`]: where a caller that numbers its own files takes each new one.
    pub fn new_file_id(&mut self) -> Option<FileId> {
        self.db.files.new_file_id()
    }

    /// See [`Files::reload_included_file`]; `looks` is the list of the build it is read for.
    /// Whether the read moved: never in a database with no [`crate::Disk`] ([`Db::sealed`]),
    /// whose files outside the VFS are only those it was handed, with no disk to move under them.
    pub fn reload_included_file(&mut self, path: &AbsPath, looks: &Looks) -> bool {
        let seq = self.seq();
        let files = &self.db.files;
        files.disk().is_some_and(|disk| files.reload_included_file(disk, seq, path, looks))
    }

    /// See [`Files::set_included_file`]. Whether the read moved.
    pub fn set_included_file(&mut self, path: &AbsPath, bytes: std::sync::Arc<[u8]>) -> bool {
        let seq = self.seq();
        self.db.files.set_included_file(seq, path, bytes)
    }

    /// The read of the file outside the VFS at `path` set to `bytes`, if any crate read it: for
    /// a file whose bytes are known before they are on disk and that was read from disk before
    /// then (a file a build script's run left in `OUT_DIR`). Unlike
    /// [`Edit::set_included_file`] it makes no read of a file nothing read. Whether a read
    /// changed.
    pub fn settle_included_file(&mut self, path: &AbsPath, bytes: std::sync::Arc<[u8]>) -> bool {
        // Read by a crate, or set earlier in this edit ([`Edit::set_included_file`]).
        if self.db.files.find_included(path).is_none() && !self.db.files.is_staged_included(path) {
            return false;
        }
        // Held from here on: what is read of it is these bytes, not the disk's
        // ([`IncludedFile::hold`]), under the name the front end has it by and the place it
        // is on the disk.
        let content = IncludedFile::content_of(Ok(&bytes[..]));
        let at: &std::path::Path = path.as_ref();
        IncludedFile::hold(at, content);
        IncludedFile::hold(&self.db.files.on_disk(at), content);
        self.set_included_file(path, bytes)
    }

    /// Reads again each file outside the VFS that `include_bytes!` or `include_str!` read,
    /// through the one reader that read it first ([`Files::reload_included_file`]), and sets
    /// those whose read moved: so at the commit exactly their readers' arenas are made again.
    /// The paths set, sorted. O(the files' bytes), once per call. Each look at the disk it
    /// makes is pushed to `looks`, the list of the build it is called for.
    pub fn reload_included_files(&mut self, looks: &Looks) -> Vec<AbsPathBuf> {
        let files = self.files();
        let span = tracing::info_span!(
            target: "blaze",
            "included files read again",
            files = tracing::field::Empty,
            moved = tracing::field::Empty,
        )
        .entered();
        let mut read = 0_u64;
        let db: &Db = self.db;
        // Empty unless a file moved: the paths are the result. A database with no disk
        // ([`Db::sealed`]) has nothing on it to move.
        let mut moved: Vec<AbsPathBuf, Global> = Vec::new_in(Global);
        if let Some(disk) = files.disk() {
            moved.extend(
                files
                    .included_paths()
                    .inspect(|_| read += 1)
                    .filter(|path| !files.included_is_read(disk, db, path, looks))
                    .map(|path| path.to_path_buf()),
            );
        }
        span.record("files", read);
        moved.sort();
        for path in moved.iter().take(MOVED_NAMED) {
            tracing::info!(
                target: "blaze",
                path = %path,
                on_disk = %files.on_disk(path.as_ref()).display(),
                "an included file outside the VFS moved on disk"
            );
        }
        // Read once more as each is set: one changed back since is not set, nor listed.
        moved.retain(|path| self.reload_included_file(path, looks));
        span.record("moved", moved.len() as u64);
        moved
    }

    /// Sets the crate graph's crates, writing only what differs ([`world::Crates::set_graph`]).
    /// The builder ids' keys are in the commit's [`Committed::crates`].
    pub fn set_crate_graph(&mut self, graph: CrateGraphBuilder) -> &mut Self {
        let files = self.files();
        let map = self.world().crates.set_graph(&files, graph);
        self.crates = Some(map);
        self
    }

    /// A crate outside a [`CrateGraphBuilder`]: put after every crate there, not listed until
    /// [`Edit::add_crates`] or [`Edit::set_all_crates`]. Its root file must be in a source root
    /// already, and its dependencies made.
    pub fn create_crate(
        &mut self,
        data: WorldCrateData,
        extra: ExtraCrateData,
        workspace: Arc<crate::CrateWorkspaceData>,
        cfg: CfgOptions,
        env: Env,
    ) -> CrateKey {
        let files = self.files();
        self.world().crates.create(&files, data, extra, workspace, cfg, env)
    }

    /// Puts `added`, crates just made with [`Edit::create_crate`], into the crate graph's list
    /// after the crates there, in the order given.
    ///
    /// # Panics
    ///
    /// When one of `added` is listed already.
    pub fn add_crates(&mut self, added: &[CrateKey]) -> &mut Self {
        self.world().crates.add(added);
        self
    }

    /// Lists exactly `crates` ([`crate::all_crates`]).
    pub fn set_all_crates(&mut self, crates: &[CrateKey]) -> &mut Self {
        self.world().crates.set_list(crates);
        self
    }

    /// Sets one of `krate`'s fields ([`world::Crates::set_field`]): an equal value writes
    /// nothing.
    fn set_crate_field(&mut self, krate: CrateKey, field: world::Field) -> &mut Self {
        let files = self.files();
        self.world().crates.set_field(&files, krate, field);
        self
    }

    pub fn set_crate_data(&mut self, krate: CrateKey, data: WorldCrateData) -> &mut Self {
        self.set_crate_field(krate, world::Field::Data(data))
    }

    pub fn set_crate_extra_data(&mut self, krate: CrateKey, extra: ExtraCrateData) -> &mut Self {
        self.set_crate_field(krate, world::Field::Extra(extra))
    }

    pub fn set_crate_workspace_data(
        &mut self,
        krate: CrateKey,
        workspace: Arc<crate::CrateWorkspaceData>,
    ) -> &mut Self {
        self.set_crate_field(krate, world::Field::Workspace(workspace))
    }

    pub fn set_crate_cfg_options(&mut self, krate: CrateKey, cfg: CfgOptions) -> &mut Self {
        self.set_crate_field(krate, world::Field::Cfg(cfg))
    }

    pub fn set_crate_env(&mut self, krate: CrateKey, env: Env) -> &mut Self {
        self.set_crate_field(krate, world::Field::Env(env))
    }

    pub fn set_crate_lint_options(&mut self, krate: CrateKey, options: LintOptions) -> &mut Self {
        self.set_crate_field(krate, world::Field::Lints(options))
    }

    /// Gives `krate` `value` as kind `K` (a type of the giver's, naming what it is), its slot
    /// read by a fact drawn for this gift (of this process alone). The giver compares what the
    /// crate holds first: two [`Given`] are equal only when they are one value, and an equal
    /// one marks nothing.
    ///
    /// `krate` may be a crate this edit adds: [`Edit::crate_key`] of its builder id after
    /// [`Edit::set_crate_graph`], or [`Edit::create_crate`]'s key; the commit makes both.
    pub fn give<K: 'static>(&mut self, krate: CrateKey, value: Given) -> &mut Self {
        let kind = std::any::TypeId::of::<K>();
        self.world().crates.give(krate, kind, std::any::type_name::<K>(), value);
        self
    }

    /// [`Edit::give`], its slot read by the giver's `fact` (stable wherever the giver's is, so a
    /// read list naming it may be filed): a gift under the fact the slot holds marks nothing,
    /// whatever its value, and the slot keeps the value it has. The fact is the giver's
    /// promise that two values it held under one fact are seen alike by every reader.
    pub fn give_by<K: 'static>(&mut self, krate: CrateKey, fact: Fact, value: Given) -> &mut Self {
        let kind = std::any::TypeId::of::<K>();
        self.world().crates.give_by(krate, kind, std::any::type_name::<K>(), fact, value);
        self
    }

    /// Takes `krate`'s gift of kind `K` away ([`Edit::give`]): from this commit its arena has
    /// no slot of the kind, and `Crate::given::<K, _>` answers `None`. A gift lasts until it is
    /// replaced or taken, so this is how a giver withdraws one (a dependency's image whose
    /// files moved). Whether anything moved: a crate holding none moves nothing. Undone with
    /// the edit, like every write.
    pub fn take_given<K: 'static>(&mut self, krate: CrateKey) -> bool {
        self.world().crates.take_gift(krate, std::any::TypeId::of::<K>())
    }

    /// Gives `krate` `value` as the located kind `K` under the giver's `fact`
    /// ([`crate::Located`]): read by [`crate::Crate::located`] off the next snapshot's table,
    /// no slot of any arena. The first gift of the kind to the crate marks no arena, however
    /// much its arena holds; one under the fact held moves nothing; one under another fact
    /// replaces it and marks the crate, as [`Edit::take_located`] does.
    ///
    /// # Panics
    ///
    /// Where the world has no crate `krate`, or it holds a gift of kind `K` given as a slot
    /// ([`Edit::give`], [`Edit::give_by`]).
    pub fn give_located<K: crate::Located>(&mut self, krate: CrateKey, fact: Fact, value: Given) -> &mut Self {
        let kind = std::any::TypeId::of::<K>();
        self.world().crates.give_located(krate, kind, std::any::type_name::<K>(), fact, value);
        self
    }

    /// Takes `krate`'s located gift of kind `K` away ([`Edit::give_located`]): from this
    /// commit [`crate::Crate::located`] answers `None`, and the crate's arena is marked, so every
    /// run that read the value goes. Whether anything moved: a crate holding none moves nothing.
    pub fn take_located<K: crate::Located>(&mut self, krate: CrateKey) -> bool {
        self.world().crates.take_gift(krate, std::any::TypeId::of::<K>())
    }

    /// Has the arena of `krate` (the session arena for `None`) run `make` each time it is made,
    /// from the next commit on, after the world's own inputs there: how a crate above base-db
    /// sets inputs of its own types (a singleton, the driver's holder of something) in an
    /// arena, made again with it. `fact` names what is made: one of the fact id held there takes
    /// its place, and one of the fact held marks nothing (its `make` is dropped; the giver
    /// promises two makers under one fact make inputs no reader tells apart).
    ///
    /// # Panics
    ///
    /// Where the world has no crate `krate`.
    #[track_caller]
    pub fn set_given(&mut self, krate: Option<CrateKey>, fact: Fact, make: MakeGiven) -> &mut Self {
        // Where it is given: what a commit's span names when this maker marks its arena.
        let maker = world::Maker { fact, make, at: std::panic::Location::caller() };
        let id = maker.fact.id;
        let world = self.world();
        match krate {
            Some(key) => _ = world.crates.set_given(key, maker),
            None => {
                if let Some(was) = world::put_maker(&mut world.session.makers, maker) {
                    world.crates.undo.push(world::Undo::Maker(None, id, was));
                    world.session.moved = true;
                }
            }
        }
        self
    }

    /// The key of the crate of `built`, of the crate graph set in this edit
    /// ([`Edit::set_crate_graph`]): a crate this edit adds has its key before the commit, so a
    /// gift to it ([`Edit::give`]) is in the same commit. The same key
    /// [`Committed::crates`] holds after.
    ///
    /// # Panics
    ///
    /// Where no graph was set in this edit, or the last one set has no crate `built`.
    pub fn crate_key(&self, built: CrateBuilderId) -> CrateKey {
        let Some(map) = &self.crates else {
            panic!("`Edit::crate_key({built:?})`: no crate graph was set in this edit (`Edit::set_crate_graph`)")
        };
        match map.get(&built) {
            Some(&key) => key,
            None => panic!("`Edit::crate_key({built:?})`: the crate graph set in this edit has no such crate"),
        }
    }

    /// The front end's settings ([`crate::FrontSettings`]).
    pub fn set_settings(&mut self, settings: FrontSettingsData) -> &mut Self {
        let world = self.world();
        if world.session.settings != settings {
            let was = std::mem::replace(&mut world.session.settings, settings);
            world.crates.undo.push(world::Undo::Session(world::SessionField::Settings(was)));
            world.session.moved = true;
        }
        self
    }

    /// The roots of crates.io libraries ([`crate::LibraryRoots`]).
    pub fn set_library_roots(&mut self, roots: blaze_hash::HashSet<SourceRootId, Global>) -> &mut Self {
        let world = self.world();
        if world.session.library_roots != roots {
            let was = std::mem::replace(&mut world.session.library_roots, roots);
            world.crates.undo.push(world::Undo::Session(world::SessionField::LibraryRoots(was)));
            world.session.moved = true;
        }
        self
    }

    /// The workspace's roots ([`crate::LocalRoots`]).
    pub fn set_local_roots(&mut self, roots: blaze_hash::HashSet<SourceRootId, Global>) -> &mut Self {
        let world = self.world();
        if world.session.local_roots != roots {
            let was = std::mem::replace(&mut world.session.local_roots, roots);
            world.crates.undo.push(world::Undo::Session(world::SessionField::LocalRoots(was)));
            world.session.moved = true;
        }
        self
    }

    /// Has builds from here on be for a process with the variables `vars`
    /// ([`crate::ProcessEnv`]).
    pub fn set_process_env(&mut self, vars: Env) -> &mut Self {
        let world = self.world();
        if world.session.process_env.as_ref() != Some(&vars) {
            let was = std::mem::replace(&mut world.session.process_env, Some(vars));
            world.crates.undo.push(world::Undo::Session(world::SessionField::ProcessEnv(was)));
            world.session.moved = true;
        }
        self
    }

    /// Has builds from here on take `crates` as those of the primary packages
    /// ([`crate::PrimaryCrates`]): each crate whose membership moved gets its flag
    /// ([`crate::is_primary`]), a gift that marks that crate's arena alone.
    pub fn set_primary_crates(&mut self, mut crates: Vec<CrateKey, Global>) -> &mut Self {
        crates.sort_unstable();
        crates.dedup();
        let was = &self.world().session.primary;
        if *was == crates {
            return self;
        }
        let mut moved: Vec<(CrateKey, bool), Global> = Vec::new_in(Global);
        moved.extend(was.iter().filter(|it| crates.binary_search(it).is_err()).map(|&it| (it, false)));
        moved.extend(crates.iter().filter(|it| was.binary_search(it).is_err()).map(|&it| (it, true)));
        for (krate, primary) in moved {
            self.give::<crate::PrimaryFlag>(krate, Given::new(primary));
        }
        let world = self.world();
        let was = std::mem::replace(&mut world.session.primary, crates);
        world.crates.undo.push(world::Undo::Session(world::SessionField::Primary(was)));
        self
    }

    /// Makes the next snapshot from this edit's writes, all of them at once: see
    /// [`crate::world`]. An edit that moved nothing (no write, or each of a value held) makes
    /// none: the snapshot and its epoch stay, and [`Committed::moved`] is false.
    pub fn commit(mut self) -> Committed {
        let crates = self.crates.take();
        let committed = self.db.commit(crates);
        self.done = true;
        committed
    }
}

/// An edit dropped uncommitted (an early return, a `?`, a panic) puts back every write it made
/// ([`Files::rollback`], [`World::rollback`]): files, roots, crates, keys, gifts and makers, as
/// the last commit left them. So does one whose commit panicked (a maker's panic): the
/// snapshot it would have made is dropped half made, and the old one stays.
impl Drop for Edit<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let _span = tracing::info_span!(target: "blaze", "db edit discarded").entered();
        self.db.files.rollback();
        self.db.world_mut().rollback();
    }
}

/// A crate's fields as the world has them, with the writes of the edit read through
/// ([`Edit::written_crate`]).
#[derive(Clone, Copy, Debug)]
pub struct WrittenCrate<'w> {
    pub data: &'w WorldCrateData,
    pub extra: &'w ExtraCrateData,
    pub workspace: &'w Arc<crate::CrateWorkspaceData>,
    pub cfg: &'w CfgOptions,
    pub env: &'w Env,
}

/// The most moved files one [`Edit::reload_included_files`] names in events.
const MOVED_NAMED: usize = 16;

impl fmt::Debug for Db {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("base_db::Db")
            .field("run", &self.run)
            .field("epoch", &self.snapshot_epoch())
            .finish_non_exhaustive()
    }
}

impl blaze_memo::Database for Db {
    #[inline]
    fn memo(&self) -> &blaze_memo::Memo {
        &self.snap.memo
    }
}

impl panic::RefUnwindSafe for Db {}
