//! The front end's one database, [`Db`]: every query of every front-end crate takes `&Db`.
//!
//! The type is declared here, in the lowest front-end crate, so that each crate above can name
//! it: `hir-expand`, `hir-def`, `hir-ty` and the rest declare their queries as tracked
//! functions over `&Db`, and a call to one is a static call whose memo hit inlines into the
//! caller (docs/v2/plan/design/frontend.md). Each ingredient is a `static` of the crate that
//! declares it, which the storage numbers when it is first used, so this crate names none of
//! them. A query whose code lives
//! above the crate that declares it is declared where its key and value types are, and its
//! body is a provider the crate above installs (rustc's `Providers`); its memo hit is static
//! all the same.
//!
//! There is one `Db` per lowered build and no handle on it: every task reads it through `&Db`,
//! each with its own query stack, and nothing clones it. A write takes `&mut Db`, so it happens
//! only once no reader holds `&Db`. A task that outlives a borrow holds a [`Reader`] of a
//! [`Shared`] database, whose write stops it and waits for it to let go.

use std::{
    cell::Cell,
    fmt, panic,
    sync::atomic::{AtomicBool, Ordering},
};

use crossbeam_queue::SegQueue;

use crate::{AbsPath, AbsPathBuf, AnchoredPath, VfsPath};
use blaze_ids::ContentId;
use blaze_mem::{Tag, frame};
use blaze_query::db::Durability;
use span::{FileId, TextSize};
use triomphe::Arc;

use crate::{
    Crate, CrateGraphBuilder, CratesMap, FileSourceRootInput, FileText, Files, IncludedFile,
    LibraryRoots, LocalRoots, Nonce, SourceRoot, SourceRootId, SourceRootInput, ViewChange,
    ViewError, ViewRoot,
    editioned_file_id::Parses,
    maintain::{Maintenance, Note, Trigger},
    set_all_crates_with_durability,
};

/// The front end's database. See the module.
///
/// It has no `Default`: a build's is [`Db::new`] and a test's [`Db::for_tests`], and which one
/// is never left to a default. A crate's tests name it `TestDB` (`type TestDB = Db;`), so that
/// the engine's accessors, generic over the database, take `&TestDB` as they take `&Db`.
pub struct Db {
    storage: blaze_query::db::Storage,
    files: Arc<Files>,
    crates_map: Arc<CratesMap>,
    nonce: Nonce,
    run: Run,
    /// The events the query engine reports, kept only while [`Db::log`] runs; `None` but in a database
    /// made by [`Db::for_tests`].
    log: Option<std::sync::Arc<EventLog>>,
    /// The maintainers' state and the writes they have yet to see ([`crate::maintain`]).
    pub(crate) maintenance: Maintenance,
    /// What the files' parses are read from besides parsing.
    pub(crate) parses: Parses,
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

/// The events a logged database keeps while [`Db::log`] runs, and the queries a maintainer's
/// apply executes, which it must not. The hook reads `active` and `applying` before it pushes,
/// so an event outside both costs two relaxed loads.
#[derive(Default)]
struct EventLog {
    active: AtomicBool,
    events: SegQueue<blaze_query::db::Event>,
    applying: AtomicBool,
    executed: SegQueue<blaze_query::db::DatabaseKeyIndex>,
}

impl EventLog {
    fn record(&self, f: impl FnOnce()) -> Vec<blaze_query::db::Event> {
        while self.events.pop().is_some() {}
        self.active.store(true, Ordering::Release);
        f();
        self.active.store(false, Ordering::Release);
        // alloc: tests only
        std::iter::from_fn(|| self.events.pop()).collect()
    }
}

pub mod executions;
mod shared;
#[cfg(test)]
mod tests;

pub use shared::{Reader, Shared};

impl Db {
    /// The database of a build: no crate, no source root, and its batches run as plane tasks
    /// of the process's pool. What the crates above need set before a query runs (proc-macro
    /// inputs, attribute expansion) they set on it, as `load-cargo` does.
    pub fn new() -> Db {
        Db::with(Run::Pool, None)
    }

    /// A build's database ([`Db::new`]) that keeps the query engine's events while [`Db::log`] runs: for
    /// a harness that counts what a request of a real build executes (`crates/hir-firewall`).
    /// A build never makes one: each event takes a lock.
    pub fn logged() -> Db {
        let log = std::sync::Arc::new(EventLog::default()); // alloc: a logged database's setup
        Db::with(Run::Pool, Some(log))
    }

    /// A database for tests: its batches run on the calling thread in this thread's
    /// [`RunOrder`], it keeps the query engine's events while [`Db::log`] runs, and it has empty library
    /// and local roots and an empty crate graph, as a fixture expects to find them.
    pub fn for_tests() -> Db {
        let log = std::sync::Arc::new(EventLog::default()); // alloc: a test database's setup
        let mut db = Db::with(Run::Here, Some(log));
        _ = LibraryRoots::new(&mut db, Durability::MEDIUM, Default::default());
        _ = LocalRoots::new(&mut db, Durability::MEDIUM, Default::default());
        CrateGraphBuilder::default().set_in_db(&mut db);
        db
    }

    fn with(run: Run, log: Option<std::sync::Arc<EventLog>>) -> Db {
        blaze_query::db::name_rows(name_row);
        let traced = executions::traced();
        let callback = (log.is_some() || traced).then(|| {
            let log = log.clone(); // clone: an `Arc` of the test log, once per database
            // alloc: setup, once per database: the engine's event hook
            Box::new(move |_: &blaze_query::db::Storage, event: blaze_query::db::Event| {
                if traced {
                    executions::record(&event);
                }
                if let Some(log) = &log
                    && log.applying.load(Ordering::Relaxed)
                    && let blaze_query::db::EventKind::WillExecute { database_key } = event.kind
                {
                    log.executed.push(database_key);
                }
                if let Some(log) = &log
                    && log.active.load(Ordering::Relaxed)
                {
                    log.events.push(event);
                }
            }) as blaze_query::db::Hook
        });
        let mut db = Db {
            storage: blaze_query::db::Storage::new(callback),
            files: Default::default(),
            crates_map: Default::default(),
            nonce: Nonce::new(),
            run,
            log,
            maintenance: Maintenance::default(),
            parses: Parses::new(crate::trees::installed().cloned()),
        };
        set_all_crates_with_durability(&mut db, std::iter::empty(), Durability::HIGH);
        crate::settings::set_from_env(&mut db);
        db
    }

    /// The files, source roots and what was read from outside them.
    #[inline]
    pub fn files(&self) -> &Files {
        &self.files
    }

    /// [`Db::files`] to keep past a borrow of the database: the one table every clone of it
    /// shares, so a file it gains later is there too (the code generator names the files
    /// `include!` reads outside the VFS through it while their bodies are lowered).
    #[inline]
    pub fn shared_files(&self) -> Arc<Files> {
        Arc::clone(&self.files)
    }

    /// Text of the file.
    #[inline]
    pub fn file_text(&self, file_id: FileId) -> FileText {
        self.files.file_text(file_id)
    }

    /// Whether [`Db::file_text`] has the file: one that is not UTF-8 has no text.
    #[inline]
    pub fn has_file_text(&self, file_id: FileId) -> bool {
        self.files.has_file_text(file_id)
    }

    /// [`Files::raw_file`]: the bytes of a file of the VFS that has no text.
    #[inline]
    pub fn raw_file(&self, file_id: FileId) -> Option<IncludedFile> {
        self.files.raw_file(file_id)
    }

    /// [`Files::file_content`]: the content ID a file has in its source root, text or bytes.
    #[inline]
    pub fn file_content(&self, file_id: FileId) -> ContentId {
        self.files.file_content(self, file_id)
    }

    /// [`Files::new_file_id`]: where a caller that numbers its own files takes each new one.
    pub fn new_file_id(&mut self) -> Option<FileId> {
        self.files.new_file_id()
    }

    pub fn set_file_text(&mut self, file_id: FileId, text: &str) {
        let files = Arc::clone(&self.files);
        self.write_file("Db::set_file_text", file_id, |db| files.set_file_text(db, file_id, text));
    }

    pub fn set_file_text_with_durability(
        &mut self,
        file_id: FileId,
        text: &str,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        self.write_file("Db::set_file_text_with_durability", file_id, |db| {
            files.set_file_text_with_durability(db, file_id, text, durability)
        });
    }

    /// Contents of the source root.
    #[inline]
    pub fn source_root(&self, id: SourceRootId) -> SourceRootInput {
        self.files.source_root(id)
    }

    /// Source root of the file.
    #[inline]
    pub fn file_source_root(&self, id: FileId) -> FileSourceRootInput {
        self.files.file_source_root(self, id)
    }

    /// See [`Files::reload_included_file`].
    pub fn reload_included_file(&mut self, path: &AbsPath) -> bool {
        let files = Arc::clone(&self.files);
        self.write("Db::reload_included_file", |db| {
            let changed = files.reload_included_file(db, path);
            if changed && let Some(file) = files.find_included(path) {
                db.note(Note::Included(file));
                // The text `include!` parses of it changed with it: a file no module is in.
                if let Some(source) = files.included_source_file(path) {
                    db.note(Note::Set(source));
                }
            }
            changed
        })
    }

    /// See [`Files::set_included_file`].
    pub fn set_included_file(&mut self, path: &AbsPath, bytes: std::sync::Arc<[u8]>) -> bool {
        let files = Arc::clone(&self.files);
        self.write("Db::set_included_file", |db| {
            let changed = files.set_included_file(db, path, bytes);
            if changed && let Some(file) = files.find_included(path) {
                db.note(Note::Included(file));
                if let Some(source) = files.included_source_file(path) {
                    db.note(Note::Set(source));
                }
            }
            changed
        })
    }

    /// Reads again each file outside the VFS that `include_bytes!` or `include_str!` read,
    /// through the one reader that read it first ([`Files::reload_included_file`]), and sets
    /// those whose read moved in one batch named by `trigger`: so exactly their readers run
    /// again. A read moves when the bytes do, or when a file becomes unreadable or readable
    /// again, or fails another way: [`IncludedFile`] holds the `Result`, and the macro that
    /// reads it reports the error naming the path, as rustc does. The paths set, sorted; when
    /// none moved, nothing is written and no maintenance pass runs. O(the files' bytes), once
    /// per call: for whoever owns the database across changes on disk (a daemon's kept front
    /// end, each job), as a file outside the VFS can change with no change of the VFS.
    pub fn reload_included_files(&mut self, trigger: Trigger<'_>) -> Vec<AbsPathBuf> {
        let files = Arc::clone(&self.files);
        let span = tracing::info_span!(
            target: "blaze",
            "included files read again",
            files = tracing::field::Empty,
            moved = tracing::field::Empty,
        )
        .entered();
        let mut read = 0_u64;
        let db: &Db = self;
        let mut moved: Vec<AbsPathBuf> = files
            .included_paths()
            .inspect(|_| read += 1)
            .filter(|path| !files.included_is_read(db, path))
            .map(|path| path.to_path_buf()) // alloc: a path that moved, the result
            .collect(); // alloc: none unless a file moved; the paths are the result
        span.record("files", read);
        if moved.is_empty() {
            span.record("moved", 0_u64);
            return moved;
        }
        moved.sort();
        // Read once more as each is set: one changed back since is not set, nor listed.
        self.batch(trigger, |db| moved.retain(|path| db.reload_included_file(path)));
        span.record("moved", moved.len() as u64);
        moved
    }

    pub fn set_file_source_root_with_durability(
        &mut self,
        id: FileId,
        source_root_id: SourceRootId,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        self.write("Db::set_file_source_root_with_durability", |db| {
            files.set_file_source_root_with_durability(db, id, source_root_id, durability);
            db.note(Note::Root(source_root_id));
        });
    }

    pub fn set_source_root_with_durability(
        &mut self,
        source_root_id: SourceRootId,
        source_root: Arc<SourceRoot>,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        self.write("Db::set_source_root_with_durability", |db| {
            let old = files
                .has_source_root(source_root_id)
                .then(|| db.source_root(source_root_id).source_root(&*db));
            let new = Arc::clone(&source_root);
            files.set_source_root_with_durability(db, source_root_id, source_root, durability);
            db.note_root(source_root_id, old.as_deref(), &new);
        });
    }

    /// See [`Files::set_appended_root_module`].
    pub fn set_appended_root_module(
        &mut self,
        file_id: FileId,
        module: Option<&'static str>,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        self.write_file("Db::set_appended_root_module", file_id, |db| {
            files.set_appended_root_module(db, file_id, module, durability)
        });
    }

    /// See [`Files::set_crate_attr_files`].
    pub(crate) fn set_crate_attr_files(
        &mut self,
        krate: Crate,
        gone: bool,
        durability: Option<Durability>,
    ) {
        let files = Arc::clone(&self.files);
        self.write_crates("Db::set_crate_attr_files", |db| {
            files.set_crate_attr_files(db, krate, gone, durability)
        });
    }

    /// A new file with `of`'s path and text, whose appended root module is its own: see
    /// [`Files::alias`]. [`Files::aliases_of`] lists them.
    pub fn alias(&mut self, of: FileId) -> Result<FileId, crate::AliasError> {
        let files = Arc::clone(&self.files);
        self.write("Db::alias", |db| {
            let (alias, roots) = files.alias(db, of)?;
            db.note(Note::View { set: &[], added: &[alias], removed: &[], roots: &roots });
            Ok(alias)
        })
    }

    /// Has `alias` append a module (`appended`: its name, and the text after its file's) to its
    /// file's text, or nothing, in one write: its text is its file's and the suffix, kept
    /// across every write of its file, and its appended root module the name.
    pub fn set_alias_appended(
        &mut self,
        alias: FileId,
        appended: Option<(&'static str, &str)>,
    ) -> Result<(), crate::AliasError> {
        let files = Arc::clone(&self.files);
        self.write_file("Db::set_alias_appended", alias, |db| {
            files.set_alias_appended(db, alias, appended)
        })
    }

    /// Sets the inputs of one view's files, writing only those that changed: see
    /// [`Files::set_view_root`].
    pub fn set_view_root(&mut self, view: ViewRoot<'_>) -> Result<ViewChange, ViewError> {
        let files = Arc::clone(&self.files);
        self.write("Db::set_view_root", |db| {
            let change = files.set_view_root(db, view)?;
            db.note(Note::View {
                set: &change.set,
                added: &change.added,
                removed: &change.removed,
                roots: &change.roots,
            });
            Ok(change)
        })
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
    /// no path on disk to be relative to.
    pub fn included_file(&self, krate: Crate, path: AnchoredPath<'_>) -> Option<IncludedFile> {
        let root = self.source_root(self.file_source_root(path.anchor).source_root_id(self));
        let root = root.source_root_ref(self);
        let anchor = root.path_for_file(&path.anchor)?.as_path()?;
        let durability = if root.is_library { Durability::HIGH } else { Durability::LOW };
        let path = anchor.parent()?.absolutize(path.path);
        Some(self.files.included_file(self, krate, &path, durability))
    }

    /// The file outside the VFS that `path` names relative to its anchor, as `include!` in
    /// `krate` parses it ([`Files::included_source`]), with the read it is made from: `None`
    /// when the anchor has no path on disk to be relative to. rustc's `resolve_path`: a
    /// relative path is joined to the directory of the file the call is in, an absolute one is
    /// taken as it is; blaze names the file by that path normalized, as the VFS names its own.
    pub fn included_source(
        &self,
        krate: Crate,
        path: AnchoredPath<'_>,
    ) -> Option<(IncludedFile, Result<FileId, FileId>)> {
        let root = self.source_root(self.file_source_root(path.anchor).source_root_id(self));
        let root = root.source_root_ref(self);
        let anchor = root.path_for_file(&path.anchor)?.as_path()?;
        let durability = if root.is_library { Durability::HIGH } else { Durability::LOW };
        let path = anchor.parent()?.absolutize(path.path);
        Some(self.files.included_source(self, krate, &path, durability, root.is_library))
    }

    /// The file outside the VFS that a module declaration (`mod m;`) in `krate` names at `path`
    /// relative to its anchor, read from disk as rustc reads every module's file: a `#[path]`
    /// module outside every loaded source root, or a module of such a file. Made as `include!`
    /// makes its file ([`Db::included_source`]): the read is an input `krate` is noted as
    /// having read ([`crate::Files::included_by`], which its keys cover) and the caller reads
    /// it, so a file that appears, goes or is edited reruns it once whoever owns the database
    /// reads the disk again ([`Db::reload_included_files`]). Its own one-file source root is
    /// the anchor its own modules resolve from. `None` when there is no module file to parse
    /// there: the anchor has no path on disk, the read fails (no file there), the bytes are not
    /// UTF-8, or the file ID it would have is taken by a file of the view; the declaration is
    /// then an unresolved module, an error naming the paths looked for.
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
    /// the VFS to load: a frozen def map's file that is not loaded. A module declaration reads
    /// such a file itself ([`Db::module_source`]), as `include!` does
    /// ([`Db::included_source`]). An anchor with no path on disk has no file next to it to
    /// load, so nothing is noted.
    pub fn include_source(&self, path: AnchoredPath<'_>, krate: Crate) {
        let root_id = self.file_source_root(path.anchor).source_root_id(self);
        let root = self.source_root(root_id);
        let root = root.source_root_ref(self);
        if let Some(anchor) = root.path_for_file(&path.anchor).and_then(|it| it.as_path())
            && let Some(parent) = anchor.parent()
        {
            self.files.include_source(&parent.absolutize(path.path), root_id, krate);
        }
    }

    /// Notes that `env!` or `option_env!` asked for the variable `key` in `krate`.
    #[inline]
    pub fn env_is_read(&self, krate: Crate, key: &str) {
        self.files.env_is_read(krate, key);
    }

    /// Notes that `env!` in `krate` found the variable `key` unset, which rustc turns the
    /// crate down for.
    #[inline]
    pub fn env_is_unset(&self, krate: Crate, key: &str) {
        self.files.env_is_unset(krate, key);
    }

    #[doc(hidden)]
    pub fn crates_map(&self) -> Arc<CratesMap> {
        Arc::clone(&self.crates_map)
    }

    /// Runs `task(db, i)` for every `i` in `0..count`, and returns once each has run: the
    /// tasks of a round of macro expansion, or the def maps of a crate's dependencies. Each
    /// task asks queries and nothing else, so neither the order nor the threads they run on
    /// changes a result, and the caller reads what they computed afterwards in its own order.
    ///
    /// A build's database runs them as plane tasks on the one worker pool, each reading this
    /// database by reference, and the calling thread pump-waits for them
    /// (docs/v2/plan.md, "One database, shared by every thread"): no handle per
    /// thread, no thread of its own. A test database runs them here, in [`RunOrder`].
    ///
    /// Each task runs in a frame of its own, tagged `tag` (the caller's subsystem): its body
    /// runs outside the query engine's executor, and a task the pump runs on a waiting thread stays out of
    /// the waiting caller's frame.
    ///
    /// On the pool, a task that gave way ([`blaze_query::db::Cancelled::WouldWaitOnItself`])
    /// runs again on this thread once the others ended ([`blaze_query::db::par_for_each`]), and
    /// the first other panic of a task goes on from here once every task has run.
    ///
    /// The call is a span `run_all` and each task a span `run_all task`, both naming the
    /// caller's file and line in `function`, so a profile says whose tasks a stretch ran.
    #[track_caller]
    pub fn run_all<F: Fn(&Db, usize) + Sync>(&self, tag: Tag, count: usize, task: F) {
        self.run_tasks(std::panic::Location::caller(), count, false, |db, at| {
            frame::task(tag, |_| task(db, at))
        });
    }

    /// [`Db::run_all`] for a maintainer's reads, each task in its own frame: under
    /// [`RunOrder::LeftToReaders`] too it runs them, in order, since what they compute is
    /// what the maintainer's apply diffs, and no reader asks for it.
    #[track_caller]
    pub(crate) fn run_every<F: Fn(&Db, usize) + Sync>(&self, count: usize, task: F) {
        self.run_tasks(std::panic::Location::caller(), count, true, task);
    }

    /// Runs `task(db, item)` on every one of `items` at once, each in a frame of its own tagged
    /// `tag`, and returns once each has run: a maintainer's apply working on each crate's own
    /// state over the memos its reads made, which it then applies to the database in `items`'
    /// order. Every task runs, under [`RunOrder::LeftToReaders`] too (in order), as
    /// [`Db::run_all`]'s for a maintainer's reads do: what a task does to its item is its
    /// result, and no reader asks for it.
    ///
    /// A build's database runs them on the pool (`blaze_graph::par`, inside a
    /// [`blaze_query::db::pump_barrier`]); a test database runs them here, in [`RunOrder`]
    /// (on the pool too under [`RunOrder::OnPool`]). As the pool's loop does for a task on any
    /// of its threads, its caller's included, a task that gave way
    /// ([`blaze_query::db::Cancelled::WouldWaitOnItself`]) runs again once the others have, in
    /// index order, and a give-way of that second run goes on to the caller: so a test sees
    /// the rerun a maintainer's task must survive.
    pub fn run_each<T: Send>(&self, tag: Tag, items: &mut [T], task: impl Fn(&Db, &mut T) + Sync) {
        let run = |item: &mut T| frame::task(tag, |_| task(self, item));
        let order = RUN_ORDER.get();
        match self.run {
            Run::Pool => blaze_query::db::pump_barrier(|| blaze_graph::par::for_each(items, run)),
            Run::Here if order == RunOrder::OnPool => {
                blaze_query::db::pump_barrier(|| blaze_graph::par::for_each(items, run));
            }
            Run::Here => {
                let mut again = Vec::new(); // alloc: a test database's tasks that gave way, few
                let mut once = |at: usize, item: &mut T| {
                    let ran = panic::catch_unwind(panic::AssertUnwindSafe(|| run(item)));
                    if let Err(payload) = ran {
                        match payload.downcast_ref::<blaze_query::db::Cancelled>() {
                            Some(blaze_query::db::Cancelled::WouldWaitOnItself { .. }) => {
                                again.push(at);
                                // As the pool's loop: the next item is not the same ask again.
                                blaze_graph::pump::rerun_later();
                            }
                            _ => panic::resume_unwind(payload),
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
        task: F,
    ) {
        if count == 0 {
            return;
        }
        let all = tracing::info_span!(target: "blaze", "run_all", function = tracing::field::Empty, count);
        // alloc: only while a subscriber records the span, which interns it
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
            blaze_query::db::attach(db, || task(db, at))
        };
        match self.run {
            Run::Pool => blaze_query::db::par_for_each(count, |at| run(self, at)),
            Run::Here => match RUN_ORDER.get() {
                RunOrder::OnPool => blaze_query::db::par_for_each(count, |at| run(self, at)),
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

    /// This database's nonce and the revision it is at: what tells a cached answer of another
    /// database or revision apart.
    #[inline]
    pub fn nonce_and_revision(&self) -> (Nonce, blaze_query::db::View) {
        (self.nonce, self.storage.view())
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

    /// The events of `f`, in the order they happened. A test that wants the order of the work
    /// alone, without a batch's tasks run ahead of their readers, runs `f` under
    /// [`RunOrder::LeftToReaders`] ([`set_run_order`]).
    ///
    /// # Panics
    ///
    /// On a database not made by [`Db::for_tests`] or [`Db::logged`]: a build's keeps no
    /// events.
    pub fn log(&self, f: impl FnOnce()) -> Vec<blaze_query::db::Event> {
        let log = self
            .log
            .as_ref()
            .expect("`Db::log` on a database made by neither `Db::for_tests` nor `Db::logged`");
        log.record(f)
    }

    /// [`Db::log`] of a write: `f` gets the database to set inputs on, so the events include
    /// what the write-side maintainers ran in it.
    pub fn log_mut(&mut self, f: impl FnOnce(&mut Db)) -> Vec<blaze_query::db::Event> {
        // clone: one count of the log, tests only, so `f` can borrow the database.
        let log = self
            .log
            .clone()
            .expect("`Db::log_mut` on a database made by neither `Db::for_tests` nor `Db::logged`");
        log.record(|| f(self))
    }

    /// Starts noting the queries executed, on a logged database: a maintainer's apply is
    /// starting.
    pub(crate) fn begin_apply(&self) {
        if let Some(log) = &self.log {
            while log.executed.pop().is_some() {}
            log.applying.store(true, Ordering::Release);
        }
    }

    /// Stops noting the queries executed, and names those executed since [`Db::begin_apply`],
    /// in order: none on a database that logs nothing.
    pub(crate) fn end_apply(&self) -> Vec<String> {
        let Some(log) = &self.log else { return Vec::new() }; // alloc: empty, allocates nothing
        log.applying.store(false, Ordering::Release);
        std::iter::from_fn(|| log.executed.pop())
            // alloc: a logged database's check, tests only
            .map(|key| self.storage.ingredient_name(key.ingredient_index()).to_owned())
            .collect() // alloc: a logged database's check, tests only
    }

    /// The names of the queries `f` executed, as the query engine names them, in order.
    pub fn log_executed(&self, f: impl FnOnce()) -> Vec<String> {
        self.log(f)
            .into_iter()
            .filter_map(|event| match event.kind {
                blaze_query::db::EventKind::WillExecute { database_key } => {
                    Some(self.storage.ingredient_name(database_key.ingredient_index()).to_owned())
                }
                _ => None,
            })
            .collect() // alloc: tests only
    }
}

impl fmt::Debug for Db {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("base_db::Db").field("run", &self.run).finish_non_exhaustive()
    }
}

/// The profile's name of an input's row ([`blaze_query::db::RowNamer`]): a file's text by the
/// path its source root has it at (its `FileId` where it has none), so the run log says which
/// file's edit made a query run again. Called with the database the log is taken with
/// attached; any other row is left to its ID.
fn name_row(ingredient: &'static str, id: blaze_query::db::Id, out: &mut String) -> bool {
    use std::fmt::Write as _;
    blaze_query::db::with_attached_db::<Db, _>(|db| {
        let Some(db) = db else { return false };
        match ingredient {
            "FileText" => {
                let Some(text) = <FileText as blaze_query::db::FromIdIn>::from_id_in(
                    blaze_query::db::Database::storage(db),
                    id,
                ) else {
                    return false;
                };
                let file = text.file_id(db);
                match db.path_for_file(*file) {
                    Some(path) => _ = write!(out, "{path}"),
                    None => _ = write!(out, "{file:?}"),
                }
                true
            }
            _ => false,
        }
    })
}

impl blaze_query::db::Database for Db {
    #[inline]
    fn storage(&self) -> &blaze_query::db::Storage {
        &self.storage
    }

    #[inline]
    fn storage_mut(&mut self) -> &mut blaze_query::db::Storage {
        &mut self.storage
    }
}

impl panic::RefUnwindSafe for Db {}
