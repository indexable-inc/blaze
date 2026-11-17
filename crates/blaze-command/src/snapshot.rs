//! The databases a front task runs in: one blaze-memo snapshot per unit, made from the unit's
//! text and its dependencies' surface records, never from a dependency's text.
//!
//! # What a snapshot is
//!
//! A `base_db::Db` built with `blaze_memo::Reads::Kept` and blaze-item-hash's boundaries
//! (`reads::mark_boundaries`), so every query result keeps its read list and a body's trace is
//! shareable. Its crate graph is the unit and its transitive dependencies ([`Prepared::closure`]),
//! each crate as [`crate_graph`] adds it, with the key [`crate_keys`] computed for it before any
//! database existed; a snapshot whose committed crate has another key refuses
//! ([`KEY_MOVED`]): the crate the surface record names is not the crate this graph made.
//!
//! - A dependency is given from its surface record by the address it was read at
//!   (`blaze_front::surface::ReadAt::of` per table, `Opened::open_read`, then `give`, in the
//!   edit after the graph's): its def map pages, signatures, lang items and impl tables are read
//!   in place from the record, trusted by that address, with no file of it looked up or
//!   compared. The gift makes the crate's file ids (`hir_def::deps::keyed_files`); its root
//!   alone is set in edit 1, with its text. A crate an image serves is never resolved from
//!   source (`hir_def::nameres::crate_resolution` runs only for a crate no image serves), so it
//!   is never lowered here; the tests hold that with the memo's run log.
//! - Then one removable step sets each given crate's gathered texts on the ids its gift made
//!   ([`given_texts`]): a demanded body, a trait's or impl's items and non-lang attributes
//!   still read a dependency's text. It goes when the surface carries those.
//! - For a surface task the unit's own crate is built from its text: every file the command
//!   gathered ([`crate::front::Command::gather`]), the root first, and every file a late read
//!   includes, in a local root when the unit is a member, else a library root.
//! - For a body task the unit's own crate is given from its own surface too (the universe's
//!   own table), its texts set by the same step so that a body's syntax is there to lower, and
//!   the item store is installed (`hir_ty::body_store::install_trace`) before any query runs.
//!
//! A kept snapshot holds only the database the gift wrote: the `Opened` and its `ReadAt`s
//! borrow the request's tables and end inside [`open`]; the bytes the database reads are in
//! the process's holders ([`Holders`]), not the request's.
//!
//! # What is kept between tasks
//!
//! One opened body snapshot per (worker, unit): a body task takes one out of its unit's pool
//! ([`Pool::take`]) whose universe digest is the task's, runs in it, and puts it back
//! ([`Pool::put`]); a pool keeps at most [`KEPT`] and drops every snapshot of another
//! universe when one is put back (the universe moved under it, so it would never be taken
//! again). A surface task's snapshot is dropped after its task: a unit's surface is made once
//! per request. A program's plan and code tasks lease plan snapshots ([`Task::Plan`],
//! [`lease_plan`]) from the program's own pool, sized by the workers: no texts step, no find
//! index, and the given store swapped in per task, reused only where the task is given every
//! body the snapshot ever was ([`covers`]), since the memo keeps their answers.
//! The surface records' bytes are copied once per process into a 16-aligned holder
//! per record address ([`Holders`]), as `Opened` keeps its holder for the snapshot's life and the
//! host's bytes live only for the task.
//!
//! Nothing thread-bound is held across a wait (rule 27): a snapshot is a value moved out of the
//! pool's lock and back; no guard, thread-local borrow or `Rc` lives across the task. The pool's
//! lock is held only for a swap.
//!
//! At exit the process exits: `Command::exit` forgets the command, so no snapshot, holder or
//! installed store is torn down piece by piece.
//!
//! # Cost against the floor
//!
//! Opening a snapshot is O(closure crates) to build the graph, O(closure files) text inputs
//! set (each text copied once into the database: for a unit over std, std's every file; a
//! view of the gathered bytes, `Edit::set_view_root` over `blaze_vfs::Bytes`, would make that
//! no copy), one
//! `view` per surface (O(1) in the records' sizes) and three slots per crate given; then
//! blaze-front's blocker 3: serving a crate from its pages builds a `ModulesMap` with one entry
//! per module (`pages::served`), so the first query of a crate's def map is O(its modules). The
//! floor is one mapped lookup per fact a task reads. That gap is paid once per snapshot and
//! amortized over every body task the pool serves it to; a surface task pays it whole each time.
//! Every snapshot also builds its find index ([`Find`], text id to body and body to text id):
//! O(own bodies + own rows), once per snapshot; a surface task reads its own def map anyway.

#![blaze::sans_io]

use core::alloc::Allocator;
use std::alloc::Global;

use base_db::{
    AbsPathBuf, Crate, CrateDisplayName, CrateGraphBuilder, CrateName, CrateOrigin, CrateWorkspaceData, Db,
    DependencyBuilder, Env, FileId, FileSet, LangCrateOrigin, SourceRoot, SourceRootId, TargetKind, VfsPath,
};
use blaze_core::lock::Lock;
use blaze_format::Aligned;
use blaze_hash::{HashMap, HashSet};
use blaze_ids::{ContentId, Hasher};
use blaze_keyed::{
    driver::TaskFailed,
    front::{GivenBody, Table, Universe},
    kind::{Origin as PackageOrigin, Settings, ToolId},
    record::{StableId, Traced},
};
use blaze_unitkey::{image::Reading, read::Image, rows::BODY, unit};
use cfg::CfgOptions;
use hir_def::{DefWithBodyId, Lookup as _, deps::ImageBuf, src::HasSource};
use hir_ty::body_store::Installed;
use intern::Symbol;
// triomphe's `Arc` takes no allocator (`blaze::unwritten_allocator` judges none of it); named
// apart from std's, which does.
use triomphe::Arc as TArc;

use crate::{
    CommandError,
    front::{Command, UnitText},
    graph::{Graph, UnitKind},
};

/// A snapshot whose unit or dependency has no table in the task's universe.
pub const NO_TABLE: &str =
    "a snapshot: a crate of the unit's closure has no surface table in the task's universe (`Universe::tables`)";
/// A table whose owner is not the one the graph gave its unit.
pub const WRONG_OWNER: &str =
    "a snapshot: a surface table's owner is not the unit's `Graph` owner (`Owner::of_subject(crate key)`)";
/// A surface record whose head does not open.
pub const NOT_A_SURFACE: &str = "a snapshot: a table's bytes are not a surface record (`blaze_format::open::<Traced>`)";
/// The surfaces did not open or give (`blaze_front::surface::SurfaceError`).
pub const GIVE_FAILED: &str =
    "a snapshot: the universe's surface records do not open or give (`blaze_front::surface::Opened`)";
/// A committed crate whose key is not the one `crate_keys` computed.
pub const KEY_MOVED: &str =
    "a snapshot: a committed crate's key (`blaze_items::key::crate_key`) is not its unit's `Graph` key (`Unit::key`)";
/// A crate given from its surface with no files of its gift (`hir_def::deps::keyed_files`), or
/// with no root in the snapshot.
pub const NOT_KEYED: &str =
    "a snapshot: a crate given from its surface has no keyed files (`hir_def::deps::keyed_files`)";
/// The file arena is full.
pub const NO_FILE_ID: &str = "a snapshot: the database has no file id left (`Edit::new_file_id`)";
/// A file path that is not absolute.
pub const NOT_ABSOLUTE: &str = "a snapshot: a unit's file path is not absolute";
/// A file whose bytes are not UTF-8 (the database's file texts are `str`).
pub const NOT_TEXT: &str = "a snapshot: a unit's file (a module or an included file) is not UTF-8";
/// The item store would not install.
pub const NO_STORE: &str = "a snapshot: the item store did not install (`hir_ty::body_store::install_trace`)";
/// An image of the unit's own files does not open.
pub const IMAGE: &str = "a snapshot's find index: a file image of the unit does not open (`blaze_unitkey::read::Image`)";
/// Two bodies the def map has at one place.
pub const TWO_BODIES: &str = "a snapshot's find index: two bodies of the def map end where one item row of the image does";
/// Two rows of one stable id.
pub const TWO_ROWS: &str = "a snapshot's find index: two body rows of the unit have one stable id";
/// The unit's text was not gathered before the request.
pub const NOT_GATHERED: &str = "a front task: the unit's files were not gathered before the request (`Command::gather`): the command's certificate look held, so it gathered nothing, and the request's own look then missed (a file moved between the two; rule: a key is of the bytes the request read); run again";
/// A dependency name that is no crate name.
pub const BAD_NAME: &str = "a crate graph: a dependency's name is not a crate name (`base_db::CrateName::new`)";
/// The graph's dependencies make a cycle.
pub const CYCLE: &str = "a crate graph: a dependency makes a cycle (`CrateGraphBuilder::add_dep`)";
/// A dependency index not below its unit's.
pub const DEP_ORDER: &str = "a crate graph: a dependency's unit index is not below its dependent's (`Graph::units`)";

/// The most snapshots a unit's pool keeps: one per worker that ran its bodies at once; more
/// are dropped when put back.
pub const KEPT: usize = 8;

/// What a task's snapshot is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Task {
    /// The unit's own crate from its text.
    Surface,
    /// The unit's own crate from its own surface, the item store installed.
    Body,
    /// A program's plan or code task ([`lease_plan`]): the program unit's own crate from its own
    /// surface like a body's, and no store installed (the lease installs the given one), no find
    /// index (a plan finds no body by text id) and no texts step (a plan reads no text).
    Plan,
}

/// What `Command::new` decides once per unit, before any task.
pub struct Prepared<A: Allocator> {
    /// The unit's crate key (`Unit::key`, checked against [`crate_keys`] in `Command::new`):
    /// its identity in blaze-unitkey's keys, and what its surface record's meta names.
    pub crate_key: ContentId,
    pub tool: ToolId,
    pub settings: Settings,
    pub reading: Reading,
    /// The unit and its transitive dependencies, ascending (dependencies first).
    pub closure: Vec<u32, A>,
    /// Why every task of the unit is refused, decided before any task: a proc macro, a build
    /// script, or a direct dependency on one.
    pub refused: Option<&'static str>,
}

/// One opened snapshot. Field order is drop order: the item store is uninstalled before its
/// database goes.
pub struct Snapshot<A: Allocator> {
    pub installed: Option<Installed>,
    pub find: Find<A>,
    /// Every body record a plan snapshot was ever given ([`lease_plan`]), `(item, result)`,
    /// sorted by item, each once: what its memo may hold an answer of. Empty for the others.
    pub given: Vec<(StableId, ContentId), A>,
    /// The unit's own crate in `db`.
    pub own: Crate,
    /// The digest of the universe it was opened from ([`universe_digest`]).
    pub universe: ContentId,
    /// The own crate's file ids, one per gathered file in its order (`None` for one its
    /// crate's pages do not name; every one in a surface task): what the surface task checks
    /// deferred files against (`front::Deferred`).
    pub own_files: Vec<Option<FileId>, A>,
    pub db: Db,
}

/// The one index between blaze-unitkey's text ids and the def map's bodies of a snapshot's
/// own crate, in both directions, built once per snapshot ([`find`]): a body task finds its
/// item by text id ([`Find::get`]); a surface task names each body's text id
/// ([`blaze_front::surface::TextIds`]). A body with no row of its own in the unit's images (an
/// enum discriminant, a body a macro expansion made) is in neither direction.
pub struct Find<A: Allocator> {
    /// Sorted by text id, each once.
    pub rows: Vec<(StableId, DefWithBodyId), A>,
    /// The same pairs, sorted by definition.
    pub by_def: Vec<(DefWithBodyId, StableId), A>,
}

impl<A: Allocator> Find<A> {
    /// The body of text id `item`. One binary search.
    pub fn get(&self, item: StableId) -> Option<DefWithBodyId> {
        self.rows.binary_search_by(|(at, _)| at.0.0.cmp(&item.0.0)).ok().map(|at| self.rows[at].1)
    }
}

impl<A: Allocator> blaze_front::surface::TextIds for Find<A> {
    /// One binary search.
    fn text_id(&self, def: DefWithBodyId) -> Option<StableId> {
        self.by_def.binary_search_by(|(at, _)| at.cmp(&def)).ok().map(|at| self.by_def[at].1)
    }
}

/// A unit's kept body snapshots, or a program's kept plan snapshots.
pub struct Pool<A: Allocator> {
    kept: Lock<Vec<Snapshot<A>, A>>,
    /// The most it keeps.
    cap: usize,
}

impl<A: Allocator> Pool<A> {
    /// A unit's body pool, keeping at most [`KEPT`].
    pub fn new(alloc: A) -> Pool<A> {
        Pool::with_cap(KEPT, alloc)
    }

    /// A pool keeping at most `cap`: a program's plan pool, sized by the workers that run its
    /// plan and code tasks at once.
    pub fn with_cap(cap: usize, alloc: A) -> Pool<A> {
        Pool { kept: Lock::new("command.snapshot.pool", Vec::new_in(alloc)), cap }
    }

    /// A kept snapshot of `universe`, moved out of the pool; `None` when none is free.
    pub fn take(&self, universe: ContentId) -> Option<Snapshot<A>> {
        self.take_where(universe, |_| true)
    }

    /// A kept snapshot of `universe` that `fits`, moved out of the pool; `None` when none is
    /// free. `fits` runs under the pool's lock: it reads, never waits.
    pub fn take_where(&self, universe: ContentId, fits: impl Fn(&Snapshot<A>) -> bool) -> Option<Snapshot<A>> {
        let mut kept = self.kept.lock();
        let at = kept.iter().position(|it| it.universe == universe && fits(it))?;
        Some(kept.swap_remove(at))
    }

    /// Puts `snapshot` back: every kept one of another universe is dropped, and none past
    /// its cap. The dropped ones are dropped after the lock is released.
    pub fn put(&self, snapshot: Snapshot<A>, alloc: A) {
        let mut dropped = Vec::new_in(alloc);
        {
            let mut kept = self.kept.lock();
            let mut at = 0;
            while at < kept.len() {
                match kept[at].universe == snapshot.universe {
                    true => at += 1,
                    false => dropped.push(kept.swap_remove(at)),
                }
            }
            match kept.len() < self.cap {
                true => kept.push(snapshot),
                false => dropped.push(snapshot),
            }
        }
        drop(dropped);
    }
}

/// The surface records' 16-aligned copies, one per record address for the process.
pub struct Holders<A: Allocator> {
    held: Lock<HashMap<ContentId, ImageBuf, A>>,
}

impl<A: Allocator + Clone> Holders<A> {
    pub fn new(alloc: A) -> Holders<A> {
        Holders { held: Lock::new("command.snapshot.holders", HashMap::new_in(alloc)) }
    }

    /// The holder of the record at `at`, copied from `bytes` the first time. Cost: one hash
    /// lookup, plus one copy of the record once per process.
    pub fn hold(&self, at: ContentId, bytes: &[u8]) -> ImageBuf {
        let mut held = self.held.lock();
        if let Some(holder) = held.get(&at) {
            return ImageBuf::clone(holder);
        }
        let holder: ImageBuf = std::sync::Arc::new_in(Aligned::copy_from(bytes), Global);
        held.insert(at, ImageBuf::clone(&holder));
        holder
    }
}

/// The address a surface record is filed at: its head's key.
pub fn surface_address(bytes: &[u8]) -> Result<ContentId, TaskFailed> {
    let root = blaze_format::open::<Traced>(bytes).map_err(|_| refused(NOT_A_SURFACE))?;
    Ok(root.head.key)
}

pub fn refused(what: &'static str) -> TaskFailed {
    TaskFailed::Refused { what }
}

/// Every unit's crate key, in unit order: `base_db::crate_item_key` of the crate
/// [`crate_graph`] adds for it (the key does not cover the root file, so a placeholder one
/// stands in). `Graph::units[u].owner` must be `Owner::of_subject(&keys[u])`; `Command::new`
/// refuses a graph where it is not.
pub fn crate_keys<A: Allocator + Clone>(
    graph: &Graph<A>,
    workspace: &TArc<CrateWorkspaceData>,
    cfgs: &[CfgOptions],
    alloc: A,
) -> Result<Vec<ContentId, A>, CommandError> {
    let mut all = Vec::with_capacity_in(graph.units.len(), alloc.clone());
    all.extend(0..graph.units.len() as u32);
    let (builder, built) = crate_graph(graph, &all, |unit| FileId::from_raw(unit), workspace, cfgs, alloc.clone())
        .map_err(|(unit, rule)| CommandError::Unit { unit, rule })?;
    let mut keys: Vec<ContentId, A> = Vec::with_capacity_in(built.len(), alloc);
    for &(_, id) in built.iter() {
        let crate_ = &builder[id];
        let key = base_db::crate_item_key(&crate_.basic, &crate_.extra, &crate_.cfg_options, &|dep| {
            // Every dependency was added before its dependent, so its key is there.
            keys[u32::from(dep.into_raw()) as usize]
        });
        keys.push(key);
    }
    Ok(keys)
}

/// The crate graph of `units` (ascending, each dependency of each in it): each unit's crate,
/// rooted at `root_of(unit)`, with its dependencies. Returns the graph and each unit's crate;
/// a refusal names the unit and the rule.
pub fn crate_graph<A: Allocator + Clone>(
    graph: &Graph<A>,
    units: &[u32],
    mut root_of: impl FnMut(u32) -> FileId,
    workspace: &TArc<CrateWorkspaceData>,
    cfgs: &[CfgOptions],
    alloc: A,
) -> Result<(CrateGraphBuilder, Vec<(u32, base_db::CrateBuilderId), A>), (u32, &'static str)> {
    let mut builder = CrateGraphBuilder::default();
    let mut built: Vec<(u32, base_db::CrateBuilderId), A> = Vec::with_capacity_in(units.len(), alloc);
    for &at in units {
        let unit = &graph.units[at as usize];
        let fail = |rule: &'static str| (at, rule);
        let cwd = parent(&unit.root);
        let cwd = triomphe::Arc::new(AbsPathBuf::try_from(cwd).map_err(|_| fail(NOT_ABSOLUTE))?);
        let env: Env = unit.env.iter().map(|(name, value)| (String::from(&**name), String::from(&**value))).collect();
        let id = builder.add_crate_root(
            root_of(at),
            unit.edition,
            Some(CrateDisplayName::from_canonical_name(&unit.name)),
            // The package (its name and version as the graph names it): two packages' crates
            // of one name and settings key apart.
            Some(String::from(&*unit.package)),
            cfgs[at as usize].clone(),
            None,
            env,
            origin(graph, at),
            Vec::new_in(Global),
            target_kind(unit.kind),
            cwd,
            triomphe::Arc::clone(workspace),
        );
        for dep in unit.deps.iter() {
            if dep.unit >= at {
                return Err(fail(DEP_ORDER));
            }
            let Some(&(_, to)) = built.iter().find(|(unit, _)| *unit == dep.unit) else {
                return Err(fail(DEP_ORDER));
            };
            let name = CrateName::new(&dep.name).map_err(|_| fail(BAD_NAME))?;
            // The edge's marks as the graph resolved them from cargo's edge, by the one rule
            // (`blaze_units::schema::UnitDep::marks`): a std root given to a user crate is
            // `noprelude` (reached by the injected `extern crate std` or an `extern crate`),
            // and a toolchain crate's declared dependency is no sysroot edge (the facade
            // `rustc-std-workspace-core`'s `pub use core::*;` under `#![no_core]`).
            let marks = dep.marks;
            builder
                .add_dep(id, DependencyBuilder::with_prelude(name, to, marks.prelude, marks.sysroot))
                .map_err(|_| fail(CYCLE))?;
        }
        built.push((at, id));
    }
    Ok((builder, built))
}

/// What a unit is to the front end, from where its package came (`Unit::origin`): the
/// toolchain's crates are lang (`core`, `alloc`, `std`, `proc_macro`, `test` by their crate
/// name, every other of the toolchain's a lang dependency, as `compiler_builtins`); a
/// registry package is a library; anything else (a member, a path, git) is local.
fn origin<A: Allocator + Clone>(graph: &Graph<A>, at: u32) -> CrateOrigin {
    let unit = &graph.units[at as usize];
    match unit.origin {
        PackageOrigin::Toolchain => CrateOrigin::Lang(match &*unit.name {
            "core" => LangCrateOrigin::Core,
            "alloc" => LangCrateOrigin::Alloc,
            "std" => LangCrateOrigin::Std,
            "proc_macro" => LangCrateOrigin::ProcMacro,
            "test" => LangCrateOrigin::Test,
            _ => LangCrateOrigin::Dependency,
        }),
        PackageOrigin::Registry { .. } => CrateOrigin::Library { repo: None, name: Symbol::intern(&unit.package) },
        PackageOrigin::Private => CrateOrigin::Local { repo: None, name: Some(Symbol::intern(&unit.package)) },
    }
}

fn target_kind(kind: UnitKind) -> TargetKind {
    match kind {
        UnitKind::Lib => TargetKind::Lib { is_proc_macro: false },
        UnitKind::ProcMacro => TargetKind::Lib { is_proc_macro: true },
        UnitKind::Bin => TargetKind::Bin,
        UnitKind::Test => TargetKind::Test,
        UnitKind::BuildScript => TargetKind::BuildScript,
    }
}

/// The directory of `path` (`/` separated): everything before its last `/`.
pub fn parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(at) => &path[..at],
        None => "",
    }
}

/// The digest of what a snapshot of `unit` opened from `universe` reads of it: each closure
/// crate's surface address, then the unit's late answers.
pub fn universe_digest(addresses: &[(u32, ContentId)], universe: &Universe<'_>) -> ContentId {
    let mut hasher = Hasher::new("blaze-command snapshot universe");
    for (unit, at) in addresses {
        hasher.update(&unit.to_le_bytes()).id(at);
    }
    for fact in universe.outside {
        hasher.id(&fact.id).id(&fact.digest);
    }
    hasher.finish()
}

/// The tables a snapshot of `unit` gives, with their addresses: every closure crate's, the
/// unit's own only for a body task. A missing table or one of another owner is refused by name.
pub fn given<'u, A: Allocator + Clone>(
    command: &Command<'_, A>,
    unit: u32,
    universe: &'u Universe<'u>,
    task: Task,
    alloc: A,
) -> Result<Vec<(u32, ContentId, &'u Table<'u>), A>, TaskFailed> {
    let prepared = &command.prepared[unit as usize];
    let mut out = Vec::with_capacity_in(prepared.closure.len(), alloc);
    for &member in prepared.closure.iter() {
        if member == unit && task == Task::Surface {
            continue;
        }
        let owner = command.graph.units[member as usize].owner;
        let table = universe.table(owner).ok_or(refused(NO_TABLE))?;
        if table.unit != member || table.owner != owner {
            return Err(refused(WRONG_OWNER));
        }
        out.push((member, surface_address(table.surface)?, table));
    }
    Ok(out)
}

#[cfg(test)]
std::thread_local! {
    /// The planted bug: the snapshot does not give its dependencies, so each is lowered from
    /// its root file.
    pub static LOWER_DEPS: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// The oracle: crates resolved from source (`crate_resolution` runs) in the snapshots
    /// this thread ran tasks in, since the test last reset it.
    pub static RESOLVED: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

/// Adds the crates `db` resolved from source since the last call to [`RESOLVED`].
#[cfg(test)]
pub fn count_resolved(db: &Db) {
    let Some(log) = db.runs() else { return };
    let runs = log.take();
    let resolved = runs.iter().filter(|ran| ran.query.ends_with("crate_resolution")).count();
    RESOLVED.with(|it| it.set(it.get() + resolved));
}

#[cfg(not(test))]
fn new_db() -> Db {
    Db::sealed(blaze_item_hash::reads::mark_boundaries, blaze_memo::Reads::Kept)
}

#[cfg(test)]
fn new_db() -> Db {
    Db::sealed_logged(blaze_item_hash::reads::mark_boundaries, blaze_memo::Reads::Kept)
}

/// Opens a snapshot of `unit` for `task` from `universe`.
///
/// The rule it relies on (blaze-memo's arenas): an id is valid in the snapshot it was read
/// from and in a later one only while its arena is carried; a commit that replaces an arena
/// (giving a crate its image does, for that crate) kills every id of the old one, and using
/// one panics in `Memo::slot` ("arena N.0 is dead"). So no arena id (`Crate`, a def id) is
/// kept across a commit: the gifts' `Crate`s live only inside the edit that gives, and every
/// id the snapshot keeps is read from `CrateKey`s after the last commit, before any query.
/// Two commits, not one: `give_image` takes a `Crate`, which exists only after the graph's
/// commit.
///
/// Three commits for a given snapshot: the graph (with each given crate's root), the gifts,
/// then the given crates' texts on the ids the gifts made ([`given_texts`], the removable step).
///
/// Cost: O(closure crates + own files) to set the graph and files, one `view` and three slots
/// per surface given, the item store installed (body), the find index (surface and body), and
/// the texts step's copy of every given crate's files (O(closure text), never parsed here;
/// not for a plan).
pub fn open<A: Allocator + Clone + Send + Sync>(
    command: &Command<'_, A>,
    unit: u32,
    universe: &Universe<'_>,
    task: Task,
) -> Result<Snapshot<A>, TaskFailed> {
    let tables = given(command, unit, universe, task, command.alloc.clone())?;
    open_from(command, unit, universe, task, &tables)
}

/// The digest of the universe a snapshot given `tables` (its [`given`]) is opened from.
fn digest_of<A: Allocator + Clone>(tables: &[(u32, ContentId, &Table<'_>)], universe: &Universe<'_>, alloc: A) -> ContentId {
    let mut addresses = Vec::with_capacity_in(tables.len(), alloc);
    addresses.extend(tables.iter().map(|&(member, at, _)| (member, at)));
    universe_digest(&addresses, universe)
}

/// [`open`] over the tables its [`given`] chose.
fn open_from<A: Allocator + Clone + Send + Sync>(
    command: &Command<'_, A>,
    unit: u32,
    universe: &Universe<'_>,
    task: Task,
    tables: &[(u32, ContentId, &Table<'_>)],
) -> Result<Snapshot<A>, TaskFailed> {
    let alloc = command.alloc.clone();
    let graph = command.graph;
    let prepared = &command.prepared[unit as usize];
    let text = command.text(unit).ok_or(refused(NOT_GATHERED))?;
    let digest = digest_of(tables, universe, alloc.clone());

    let mut db = new_db();
    // Edit 1: the roots and files, then the crate graph.
    let mut edit = db.edit();
    let mut local = HashSet::new_in(Global);
    let mut library = HashSet::new_in(Global);
    let mut roots: Vec<(u32, FileId), A> = Vec::with_capacity_in(prepared.closure.len(), alloc.clone());
    // The unit's own file ids, one per gathered file in its order (`None` for one its crate's
    // pages do not name): what the find index matches the def map's bodies in.
    let mut own_files: Vec<Option<FileId>, A> = Vec::with_capacity_in(text.files.len(), alloc.clone());
    for (at, &member) in prepared.closure.iter().enumerate() {
        let root_id = SourceRootId(at as u32);
        let mut set = FileSet::default();
        let member_text = command.text(member).ok_or(refused(NOT_GATHERED))?;
        let root = match member == unit && task == Task::Surface {
            // A surface task's own crate is built from its text: every file, with its text.
            true => {
                for file in member_text.files.iter() {
                    own_files.push(Some(put_file(&mut edit, &mut set, root_id, &file.path, &file.text)?));
                }
                for included in member_text.included.iter() {
                    put_file(&mut edit, &mut set, root_id, &included.path, &included.text)?;
                }
                own_files.first().copied().flatten().ok_or(refused(NOT_GATHERED))?
            }
            // A crate given from its surface (every dependency; the own crate of a body task):
            // its root alone, with its text (its blaze edition is read off it). Its other files
            // get their ids from the gift ([`given_texts`]).
            false => {
                let root = member_text.files.first().ok_or(refused(NOT_GATHERED))?;
                let root = put_file(&mut edit, &mut set, root_id, &root.path, &root.text)?;
                if member == unit {
                    own_files.push(Some(root));
                    own_files.extend(member_text.files.iter().skip(1).map(|_| None));
                }
                root
            }
        };
        roots.push((member, root));
        let source = match graph.units[member as usize].member {
            true => {
                local.insert(root_id);
                SourceRoot::new_local(set)
            }
            false => {
                library.insert(root_id);
                SourceRoot::new_library(set)
            }
        };
        edit.set_source_root(root_id, triomphe::Arc::new(source));
    }
    edit.set_local_roots(local);
    edit.set_library_roots(library);
    // Every closure unit has its root in `roots`; the closure is what `roots` was made from.
    let (builder, built) = crate_graph(
        graph,
        &prepared.closure,
        |member| roots.iter().find(|(it, _)| *it == member).map_or(FileId::from_raw(0), |(_, file)| *file),
        &command.workspace,
        &command.cfgs,
        alloc.clone(),
    )
    .map_err(|(_, rule)| refused(rule))?;
    edit.set_crate_graph(builder);
    let mut keys = Vec::with_capacity_in(built.len(), alloc.clone());
    let mut primary = Vec::new_in(Global);
    for &(member, id) in built.iter() {
        let key = edit.crate_key(id);
        keys.push((member, key));
        if graph.units[member as usize].member {
            primary.push(key);
        }
    }
    // The units' env is all a late read sees: the process's own is not read.
    edit.set_process_env(core::iter::empty::<(String, String)>().collect());
    edit.set_primary_crates(primary);
    // Which front end computes body identities: without it every body's early identity is
    // `Err("no front-end build is set")` (blaze-item-hash identity.rs:103-104) and the item
    // store refuses it as `Refusal::NoIdentity`. The same build `install_trace` is given.
    // blaze-cargo gives the front end's code identity here instead (`serve/unit/deps.rs`, `front_end()`).
    blaze_item_hash::identity::give_build(&mut edit, graph.tool);
    _ = edit.commit();

    // Edit 2: every given crate from its surface record. Its `Crate` ids are read from the
    // first commit's snapshot and used only inside this edit (`give_image` takes a `Crate`,
    // which exists only once the graph is committed, so the gifts cannot be in edit 1).
    #[cfg(test)]
    let plant = LOWER_DEPS.with(|it| it.get());
    #[cfg(not(test))]
    let plant = false;
    if !plant {
        let mut before: Vec<(ContentId, Crate), A> = Vec::with_capacity_in(keys.len(), alloc.clone());
        for &(member, key) in keys.iter() {
            before.push((command.prepared[member as usize].crate_key, db.krate(key)));
        }
        // Each surface with its proof of the address it was read at: a crate given so trusts its
        // pages by that address, and no file of it is looked up or compared for its def map,
        // signatures, lang or diagnostic items. The `Opened` borrows the universe's tables and
        // ends with this block: the snapshot keeps only the database `give` wrote (and the
        // holders it took, which are the process's, not the request's).
        let mut surfaces = Vec::with_capacity_in(tables.len(), alloc.clone());
        for &(_, at, table) in tables.iter() {
            let read = blaze_front::surface::ReadAt::of(table).map_err(|_| refused(GIVE_FAILED))?;
            surfaces.push((command.holders.hold(at, table.surface), read));
        }
        let opened = blaze_front::surface::Opened::open_read(&surfaces).map_err(|_| refused(GIVE_FAILED))?;
        let mut edit = db.edit();
        opened
            .give(&mut edit, |key| before.iter().find(|(it, _)| it == key).map(|(_, krate)| *krate))
            .map_err(|_| refused(GIVE_FAILED))?;
        _ = edit.commit();

        // Edit 3, the one step that sets a given crate's texts ([`given_texts`]): each gathered
        // file's text on the id its gift made for it. The `Crate` ids are read after the gift's
        // commit and not kept past this step's. A plan reads no text: no step, no commit.
        if task != Task::Plan {
            let mut texts: Vec<(FileId, &str), A> = Vec::new_in(alloc.clone());
            let mut name: Vec<u8, A> = Vec::new_in(alloc.clone());
            for &(member, key) in keys.iter() {
                if member == unit && task == Task::Surface {
                    continue;
                }
                let krate = db.crate_at(key).ok_or(refused(KEY_MOVED))?;
                let root = roots.iter().find(|(it, _)| *it == member).map(|(_, file)| *file).ok_or(refused(NOT_KEYED))?;
                let member_text = command.text(member).ok_or(refused(NOT_GATHERED))?;
                let ids = match member == unit {
                    true => Some(&mut own_files),
                    false => None,
                };
                given_texts(&db, krate, root, member_text, &mut name, &mut texts, ids)?;
            }
            let mut edit = db.edit();
            for &(file, text) in texts.iter() {
                edit.set_file_text(file, text);
            }
            _ = edit.commit();
        }
    }

    // Every `Crate` id the snapshot keeps, read again after the last commit: a commit that gives
    // a crate drops that crate's arena and makes it again, so an id read before it is dead
    // (`blaze_memo::Memo::slot`). The `CrateKey`s are the world's own and outlive commits.
    let mut crates: Vec<(ContentId, Crate), A> = Vec::with_capacity_in(keys.len(), alloc.clone());
    for &(member, key) in keys.iter() {
        let krate = db.crate_at(key).ok_or(refused(KEY_MOVED))?;
        crates.push((command.prepared[member as usize].crate_key, krate));
    }
    let own = crates
        .iter()
        .find(|(key, _)| *key == prepared.crate_key)
        .map(|(_, krate)| *krate)
        .ok_or(refused(KEY_MOVED))?;

    // The item store, before any query of the snapshot. A plan's is the given store, which
    // its lease installs per task ([`lease_plan`]).
    let installed = match task {
        Task::Surface | Task::Plan => None,
        Task::Body => Some(
            hir_ty::body_store::install_trace(&db, std::sync::Arc::new_in(blaze_item_hash::Bodies, Global), graph.tool)
                .map_err(|_| refused(NO_STORE))?,
        ),
    };
    for &(key, krate) in crates.iter() {
        if blaze_items::key::crate_key(&db, krate) != key {
            return Err(refused(KEY_MOVED));
        }
    }
    // A body task finds its item by text id, a surface task names each body's text id in its
    // rows (`blaze_front::surface::TextIds`); a plan does neither.
    let find = match task {
        Task::Surface | Task::Body => find(command, unit, text, &db, own, &own_files, alloc.clone())?,
        Task::Plan => Find { rows: Vec::new_in(alloc.clone()), by_def: Vec::new_in(alloc.clone()) },
    };
    Ok(Snapshot { installed, find, given: Vec::new_in(alloc), own, universe: digest, own_files, db })
}

// --- a program's plan snapshots ---------------------------------------------------------------

/// A plan snapshot of a unit that is no program's of the command (`Command::add_program`).
pub const NOT_A_PROGRAM_UNIT: &str = "a plan snapshot: its unit is no program's the command links (`Command::add_program`)";

/// A plan snapshot leased to one plan or code task ([`lease_plan`]): put back in its program's
/// pool when dropped.
pub struct Leased<'c, A: Allocator + Clone> {
    snapshot: Option<Snapshot<A>>,
    pool: &'c Pool<A>,
    alloc: A,
}

impl<A: Allocator + Clone> core::ops::Deref for Leased<'_, A> {
    type Target = Snapshot<A>;

    fn deref(&self) -> &Snapshot<A> {
        // Some from the lease to the drop: only `Drop` takes it.
        self.snapshot.as_ref().expect("a leased snapshot is there until it is put back")
    }
}

impl<A: Allocator + Clone> Drop for Leased<'_, A> {
    fn drop(&mut self) {
        if let Some(snapshot) = self.snapshot.take() {
            self.pool.put(snapshot, self.alloc.clone());
        }
    }
}

/// Whether a kept plan snapshot that was given `seen` may serve a task given `bodies`: every
/// body it was given is given now with the same result. Its memo may hold an answer of any of
/// `seen` (the built MIR is memoized per owner per snapshot, and the given store's answer is
/// no input a swap invalidates), so a task not given one of them would read it with no
/// `NoBody` and keep it out of `bodies`, where its read list's MIR facts come from
/// (`program::plan`): a key missing an input (rule 21). Floor: one merge, O(seen + bodies).
fn covers(bodies: &[GivenBody<'_>], seen: &[(StableId, ContentId)]) -> bool {
    let mut at = 0;
    for &(item, result) in seen {
        while at < bodies.len() && bodies[at].item < item {
            at += 1;
        }
        match bodies.get(at) {
            Some(body) if body.item == item && body.result == result => at += 1,
            _ => return false,
        }
    }
    true
}

/// `seen` with `bodies` added: sorted by item, each once (`bodies` are sorted by item, each
/// once, and [`covers`] held, so an item in both has one result).
fn merge_given<A: Allocator + Clone>(seen: &mut Vec<(StableId, ContentId), A>, bodies: &[GivenBody<'_>], alloc: A) {
    let mut out = Vec::with_capacity_in(seen.len() + bodies.len(), alloc);
    let mut at = 0;
    for body in bodies {
        while at < seen.len() && seen[at].0 < body.item {
            out.push(seen[at]);
            at += 1;
        }
        if seen.get(at).is_some_and(|it| it.0 == body.item) {
            at += 1;
        }
        out.push((body.item, body.result));
    }
    out.extend_from_slice(&seen[at..]);
    *seen = out;
}

/// A plan snapshot of program unit `unit` over `universe` for a task given `bodies` (sorted by
/// item, each once, as `Front::plan` and `Front::code` give them), with the given store over
/// `bodies` installed: one kept in the program's pool when one of this universe [`covers`]
/// `bodies`, else a new one ([`open`] with [`Task::Plan`]). The lease puts it back when dropped.
///
/// What `program.rs`'s `open_given` becomes: `snapshot::lease_plan(command, program, universe,
/// bodies)` in place of its open, its trace store's install and drop, and its given store's
/// install; the plan and code functions take `&*snapshot`.
///
/// Cost against the floor (one lookup per fact the plan reads): on a kept snapshot, one
/// [`given`] (a table lookup and a record head per closure crate, for the digest), the pool's
/// lock and a [`covers`] merge per kept snapshot, the given store swapped (one uninstall, one
/// install: a copy of each given record into its holder, `mir::given`), and one merge of the
/// given list; then the describe on a warm memo. On a new one, [`open`]'s O(closure crates)
/// graph and gifts, with no texts step, no find index and no trace store. What still costs a
/// whole open: a task whose bodies do not cover a kept snapshot's, which is every first task
/// of an instance once its pool holds only other instances' snapshots (a plan and its code
/// share one, and so do the instances with no body and the retries of one plan).
pub fn lease_plan<'c, A: Allocator + Clone + Send + Sync>(
    command: &'c Command<'_, A>,
    unit: u32,
    universe: &Universe<'_>,
    bodies: &[GivenBody<'_>],
) -> Result<Leased<'c, A>, TaskFailed> {
    let alloc = command.alloc.clone();
    let pool = command.plan_pool(unit).ok_or(refused(NOT_A_PROGRAM_UNIT))?;
    let tables = given(command, unit, universe, Task::Plan, alloc.clone())?;
    let digest = digest_of(&tables, universe, alloc.clone());
    let kept = pool.take_where(digest, |snapshot| covers(bodies, &snapshot.given));
    let mut snapshot = match kept {
        Some(snapshot) => snapshot,
        None => open_from(command, unit, universe, Task::Plan, &tables)?,
    };
    // One store per database: the last task's goes before this one's is installed.
    drop(snapshot.installed.take());
    let records = blaze_front::body::mir::given(bodies, Global).map_err(refused)?;
    let installed =
        hir_ty::body_store::install_given(&snapshot.db, std::sync::Arc::new_in(blaze_item_hash::Bodies, Global), command.graph.tool, records)
            .map_err(|why| {
                command.tell(unit, format_args!("{}: {why}", crate::program::NO_GIVEN_STORE));
                refused(crate::program::NO_GIVEN_STORE)
            })?;
    snapshot.installed = Some(installed);
    merge_given(&mut snapshot.given, bodies, alloc.clone());
    Ok(Leased { snapshot: Some(snapshot), pool, alloc })
}

/// One file of a snapshot: a new file id with `bytes` as its text, in source root `root` at
/// `path` (absolute). A path already in `set` keeps its one id: a file gathered as two modules
/// (two `#[path]`s naming it, `front::gather`) is one file of the database, which the def map
/// makes two modules of, as rustc does.
fn put_file(
    edit: &mut base_db::Edit<'_>,
    set: &mut FileSet,
    root: SourceRootId,
    path: &[u8],
    bytes: &[u8],
) -> Result<FileId, TaskFailed> {
    let path = core::str::from_utf8(path).map_err(|_| refused(NOT_TEXT))?;
    let bytes = core::str::from_utf8(bytes).map_err(|_| refused(NOT_TEXT))?;
    let path = VfsPath::from(AbsPathBuf::try_from(path).map_err(|_| refused(NOT_ABSOLUTE))?);
    if let Some(&file) = set.file_for_path(&path) {
        return Ok(file);
    }
    let file = edit.new_file_id().ok_or(refused(NO_FILE_ID))?;
    edit.set_file_text(file, bytes);
    edit.set_file_source_root(file, root);
    set.insert(file, path);
    Ok(file)
}

/// The one step that gives a crate given from its surface its texts: for each file it
/// gathered (and each it includes), the file id its gift made for it
/// (`hir_def::deps::keyed_files`, by its name in the crate's pages: its path from the crate
/// root's directory, as `hir_def::nameres::frozen`'s `file_name` writes it; the root also by
/// the pages' root mark), pushed to `into` with its text. The root's own id already has its
/// text (edit 1) and is not pushed again. A file the pages name that was not gathered keeps no
/// text, and one gathered that the pages do not name is left out: a read of the first is the
/// typed `Unreadable` of a keyed file, naming the crate and the file. `ids`, for the own crate
/// of a body task, gets each gathered file's id in order (`None` where the pages name none).
///
/// What still reads a given crate's text, and so why this step is here: a demanded body of it;
/// the items of a trait or impl no entry store holds; and the attribute flags and values of
/// its items that are not lang or diagnostic items. Real crates read their dependencies'
/// attributes and impl items all the time.
///
/// To go: when the surface record carries attributes and trait and impl items, a dependency's
/// text is set only for a body of it a body task demands; this step then sets nothing.
///
/// Cost: one binary search per gathered file (O(F log F) for a crate's F files) and a copy of
/// every given crate's every file per snapshot: for a unit over std, std's whole source, the
/// same bytes the unkeyed give copied. The floor is no copy for a crate no demanded body reads
/// (none for a surface task), so this is O(closure text) over a floor of zero until the step
/// goes.
fn given_texts<'t, A: Allocator, B: Allocator, C: Allocator>(
    db: &Db,
    krate: Crate,
    root: FileId,
    text: &'t UnitText<A>,
    name: &mut Vec<u8, B>,
    into: &mut Vec<(FileId, &'t str), C>,
    mut ids: Option<&mut Vec<Option<FileId>, C>>,
) -> Result<(), TaskFailed> {
    let keyed = hir_def::deps::keyed_files(db, krate).ok_or(refused(NOT_KEYED))?;
    let first = text.files.first().ok_or(refused(NOT_GATHERED))?;
    let dir = match first.path.iter().rposition(|&it| it == b'/') {
        Some(at) => &first.path[..at],
        None => return Err(refused(NOT_ABSOLUTE)),
    };
    let marked_root = keyed.iter().find(|it| it.is_root()).map(|it| it.file);
    let mut id_of = |path: &[u8], is_root: bool| -> Option<FileId> {
        page_name(dir, path, name);
        let named = keyed.binary_search_by(|it| it.path.as_bytes().cmp(&name[..])).ok().map(|at| keyed[at].file);
        match (named, is_root) {
            (Some(file), _) => Some(file),
            (None, true) => marked_root,
            (None, false) => None,
        }
    };
    if let Some(ids) = ids.as_deref_mut() {
        ids.clear();
    }
    for (at, file) in text.files.iter().enumerate() {
        let id = id_of(&file.path, at == 0);
        if let Some(ids) = ids.as_deref_mut() {
            ids.push(id);
        }
        if let Some(id) = id.filter(|&id| id != root) {
            into.push((id, core::str::from_utf8(&file.text).map_err(|_| refused(NOT_TEXT))?));
        }
    }
    for included in text.included.iter() {
        if let Some(id) = id_of(&included.path, false).filter(|&id| id != root) {
            into.push((id, core::str::from_utf8(&included.text).map_err(|_| refused(NOT_TEXT))?));
        }
    }
    Ok(())
}

/// `path`'s name from the directory `dir` into `into`: the path below it, with a `..` for each
/// part of `dir` it is not under, its parts joined by `/` (hir-def's `frozen::relative`, which
/// names a crate's files in its pages).
fn page_name<B: Allocator>(dir: &[u8], path: &[u8], into: &mut Vec<u8, B>) {
    fn parts(of: &[u8]) -> impl Iterator<Item = &[u8]> + Clone {
        of.split(|&it| it == b'/').filter(|it| !it.is_empty())
    }
    into.clear();
    let from = parts(dir);
    let to = parts(path);
    let common = from.clone().zip(to.clone()).take_while(|(a, b)| a == b).count();
    let mut push = |part: &[u8]| {
        if !into.is_empty() {
            into.push(b'/');
        }
        into.extend_from_slice(part);
    };
    for _ in from.skip(common) {
        push(b"..");
    }
    for part in to.skip(common) {
        push(part);
    }
}

/// Whether `def` is in module `module` (`a::b`, `` for the crate root) or a module inside it,
/// by the names of its module's chain up to the crate root. Of a file gathered under two module
/// paths, neither is a prefix of the other (that file would be its own ancestor, which the
/// gather refuses as `front::CIRCULAR`), so this tells its two defs apart.
fn in_module(def: DefWithBodyId, module: &[u8], db: &Db) -> bool {
    use hir_def::HasModule as _;
    let mut names = Vec::new_in(Global);
    let mut at = Some(def.module(db).to_db(db));
    while let Some(here) = at {
        if let Some(name) = here.name(db) {
            names.push(name);
        }
        at = here.containing_module(db);
    }
    let mut chain = names.iter().rev();
    module
        .split(|&byte| byte == b':')
        .filter(|segment| !segment.is_empty())
        // A segment the gather suffixed (`repr{1}`, `mods::module_twice`) is the module it names.
        .all(|segment| {
            chain.next().is_some_and(|name| name.as_str().as_bytes() == blaze_unitkey::mods::unsuffixed(segment))
        })
}

/// Where a body is: its file and its node's byte range.
fn place<L: HasSource>(loc: &L, db: &Db) -> Option<(FileId, u32, u32)> {
    let ptr = loc.ast_ptr(db);
    let file = ptr.file_id.file_id()?.file_id(db);
    let range = ptr.value.text_range();
    Some((file, u32::from(range.start()), u32::from(range.end())))
}

/// The find index: each active body row of the unit's images (blaze-unitkey's stable id), the
/// def map's body that ends where the row ends in the same file and starts at or before it (a
/// node's range takes in leading doc comments the row's tokens leave out). A row with no body
/// is left out, so asking it is `body::front`'s `NO_BODY`; two bodies at one row, or two rows
/// of one id, are refused by name.
///
/// Cost: one `crate_body_owners` (the own def map, which lowers no body) and one `ast_ptr` per
/// body, plus one pass over each own image's rows: O(bodies + rows), once per snapshot.
fn find<A: Allocator + Clone>(
    command: &Command<'_, A>,
    unit: u32,
    text: &UnitText<A>,
    db: &Db,
    own: Crate,
    files: &[Option<FileId>],
    alloc: A,
) -> Result<Find<A>, TaskFailed> {
    let owners = hir_def::nameres::demand::crate_body_owners(db, own);
    let mut places: Vec<(FileId, u32, u32, DefWithBodyId), A> = Vec::with_capacity_in(owners.len(), alloc.clone());
    for &def in owners.iter() {
        let at = match def {
            DefWithBodyId::FunctionId(id) => place(id.lookup(db), db),
            DefWithBodyId::ConstId(id) => place(id.lookup(db), db),
            DefWithBodyId::StaticId(id) => place(id.lookup(db), db),
            // A discriminant: no item row of its own.
            DefWithBodyId::VariantId(_) => None,
        };
        if let Some((file, start, end)) = at {
            places.push((file, end, start, def));
        }
    }
    places.sort_unstable_by_key(|&(file, end, start, _)| (file.index(), end, start));
    let cfg = &command.cfgs[unit as usize];
    let mut rows = Vec::new_in(alloc.clone());
    let mut alive = Vec::new_in(alloc.clone());
    for (at, file) in text.files.iter().enumerate() {
        // A file the own crate's pages name no id for holds no body of its def map.
        let Some(file_id) = files.get(at).copied().flatten() else {
            continue;
        };
        let image = command.image(&file.image).ok_or(refused(NOT_GATHERED))?;
        let image = Image::open(image, &file.image).map_err(|_| refused(IMAGE))?;
        unit::active(&image, cfg, &mut alive).map_err(|_| refused(IMAGE))?;
        // A file gathered as two modules is one file id (`put_file`) with each def twice, once
        // per module: a row takes the def of its own file's module. O(files) per file, paid
        // only by the files of a crate that has such a file.
        let shared = files.iter().filter(|&&it| it == Some(file_id)).count() > 1;
        for (row, item) in image.items().iter().enumerate() {
            if item.flags & BODY == 0 || !alive[row] {
                continue;
            }
            let syntax = image.syntax(row as u32).map_err(|_| refused(IMAGE))?;
            let (start, end) = syntax.range;
            let first = places.partition_point(|&(file, at, _, _)| (file.index(), at) < (file_id.index(), end));
            let mut found = None;
            for &(place_file, at, def_start, def) in &places[first..] {
                if place_file != file_id || at != end {
                    break;
                }
                if shared && !in_module(def, &file.module, db) {
                    continue;
                }
                if def_start <= start {
                    if found.is_some() {
                        return Err(refused(TWO_BODIES));
                    }
                    found = Some(def);
                }
            }
            if let Some(def) = found {
                let stable = image.stable_id(&file.module, row as u32).map_err(|_| refused(IMAGE))?;
                rows.push((stable, def));
            }
        }
    }
    rows.sort_unstable_by(|a: &(StableId, DefWithBodyId), b| a.0.0.0.cmp(&b.0.0.0));
    if rows.windows(2).any(|pair| pair[0].0.0.0 == pair[1].0.0.0) {
        return Err(refused(TWO_ROWS));
    }
    let mut by_def = Vec::with_capacity_in(rows.len(), alloc);
    by_def.extend(rows.iter().map(|&(stable, def)| (def, stable)));
    by_def.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    // A body matched by two rows: the end-offset match is no longer one to one.
    if by_def.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(refused(TWO_BODIES));
    }
    Ok(Find { rows, by_def })
}
