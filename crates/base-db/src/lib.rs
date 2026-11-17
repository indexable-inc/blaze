//! The front end's one database, [`Db`], and the query inputs it holds: files, source
//! roots and the crate graph.
//!
//! A [`Db`] is a world the driver writes (plain data by stable key, [`world`]) and a snapshot
//! of `blaze_memo` inputs made from it at each commit ([`Edit::commit`]). The session's
//! singletons ([`LibraryRoots`], [`LocalRoots`], [`ProcessEnv`], the crate list and
//! [`FrontSettings`]) are in the session arena; each crate's inputs in its own.
//!
//! Cost (rule 67): a read of a singleton or a list ([`all_crates`], [`source_root_crates`],
//! [`primary_crates`]) is one input read and one index of the snapshot's table; the lists are
//! made once per commit, O(crates).

#![feature(allocator_api, allocator_ext)]
// `#[expect(blaze::..)]` names blaze's own lints; inert under rustc.
#![feature(register_tool)]
#![register_tool(blaze)]
#![forbid(unsafe_code)]

#[cfg(test)]
mod budget_tests;
mod change;
mod content;
pub mod crate_attrs;
mod db;
mod editioned_file_id;
mod extensions;
pub use extensions::SaidBlazeEdition;
mod file_set;
mod files;
mod handles;
mod input;
mod named;
mod reparse;
mod settings;
mod source_path;
mod table;
pub mod target;
#[cfg(test)]
mod tests;
mod view;
mod world;

use std::{str::FromStr as _, sync::atomic::AtomicUsize};

pub use crate::{
    change::FileChange,
    content::{CFG_OPTIONS_DOMAIN, InputId, cfg_options_content},
    crate_attrs::{
        CRATE_ATTR_FILE_NAME, CrateAttrError, applied_crate_attr_files, crate_attr_error,
        crate_attr_files, crate_attr_owner,
    },
    db::{
        Committed, Db, Edit, MakeGiven, Reader, RunOrder, Shared, par_for_each, par_for_each_small, par_map,
        set_run_order, WrittenCrate,
    },
    editioned_file_id::{EditionedFileId, NoText},
    files::{AliasError, Disk, ExpansionRead, Files, Port, ReadBy},
    handles::{
        FileSourceRootInput, FileText, GivenFile, INCLUDED_READ, IncludedFile, LOOKS_EVERY_TIME, RawFile, ReadError,
        SourceRootInput, VfsFileRoot, VfsRoot, VfsText, line_index,
    },
    input::{
        BuiltCrateData, BuiltDependency, BuiltFor, Crate, CrateBuilder, CrateBuilderId,
        CrateDataBuilder, CrateDisplayName, CrateGraphBuilder, CrateName, CrateNamed, CrateOrigin,
        CRATE_ITEM_KEY_DOMAIN, CrateSlot, CrateType, CrateTypes, CratesIdMap, DependencyBuilder, Env,
        ExtraCrateData, crate_item_key,
        GIVEN_MORE, Given, LangCrateOrigin, ProcMacroLoadingError, ProcMacroPaths, ReleaseChannel,
        SourceRoot, SourceRootId, TargetKind, UnnamedCrate, WorldDependency, take_given,
    },
    named::NAMED_PATHS_DOMAIN,
    reparse::Reparse,
    settings::{BorrowckMode, FrontSettings, FrontSettingsData, SettingField},
    view::{ViewChange, ViewError, ViewFile, ViewRoot},
    world::{CrateKey, Plant, WorldCrateData},
};
pub use crate::{
    file_set::{FileSet, FileSetConfig, FileSetConfigBuilder},
    source_path::{AnchoredPath, AnchoredPathBuf, VfsPath},
};
use blaze_hash::HashSet;
pub use blaze_ids::ContentId;
use blaze_ids::Hasher;
use blaze_memo::Database as _;
pub use blaze_vfs::outside::{Look, LookKind, Looks};
pub use paths::{AbsPath, AbsPathBuf};
pub use semver::{BuildMetadata, Prerelease, Version, VersionReq};
pub use span::FileId;
use std::alloc::Global;

pub type FxIndexSet<T> = indexmap::IndexSet<T, blaze_hash::FixedState>;
pub type FxIndexMap<K, V> = indexmap::IndexMap<K, V, blaze_hash::FixedState>;

/// The set of roots for crates.io libraries.
/// Files in libraries are assumed to never change.
#[blaze_memo::input(singleton, debug)]
pub struct LibraryRoots {
    #[returns(ref)]
    pub roots: HashSet<SourceRootId, Global>,
}

/// The set of "local" (that is, from the current workspace) roots.
/// Files in local roots are assumed to change frequently.
#[blaze_memo::input(singleton, debug)]
pub struct LocalRoots {
    #[returns(ref)]
    pub roots: HashSet<SourceRootId, Global>,
}

/// The variables of the process a build is for, which `env!` and `option_env!` read where
/// the crate's own have nothing: those of whoever asked for the build
/// ([`Edit::set_process_env`]). Its content ID is [`Env::content`] of its variables.
#[blaze_memo::input(singleton, debug)]
pub struct ProcessEnv {
    #[returns(ref)]
    #[get(vars_field)]
    vars: Env,
    #[returns(copy)]
    #[get(content_field)]
    content: InputId,
}

impl ProcessEnv {
    pub fn vars<'db, Db: ?Sized + blaze_memo::Database>(self, db: &'db Db) -> &'db Env {
        self.vars_field(db)
    }

    pub fn content<Db: ?Sized + blaze_memo::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

/// The variable `key` of the process the build is for.
///
/// Read through a memo a variable ([`process_var`]), never the map itself: a query that
/// reads one variable reads that variable's memo, whose value is the same when another
/// variable changes, so a reader of it is checked and kept. Until 2026-11-11 every reader read
/// [`ProcessEnv`]'s one field of all the variables, and a request whose environment differed
/// in any variable, read by nothing, had every reader of any variable (each `env!` and
/// `option_env!` expansion, and all that reads those) verified or run again.
pub fn process_env_var(db: &Db, key: &str) -> Option<String> {
    process_var(db, intern::Symbol::intern(key)).map(str::to_owned)
}

/// The process's variable `name` ([`ProcessEnv`]): the one reader of the map of all of them.
/// Its key covers what it reads (rule keys-cover-every-input): the name is its argument and
/// the map its one read. Homed in the session arena (a name names no crate), kept there.
#[blaze_memo::tracked(returns(copy))]
fn process_var<'db>(db: &'db Db, name: intern::Symbol) -> Option<&'db str> {
    let value = ProcessEnv::try_get(db)?.vars(db).get(name.as_str())?;
    Some(blaze_memo::keep_str(db.memo(), &value))
}

/// What of the process's variables the database's crates read: each `env!` or `option_env!`
/// the `env!` log has ([`Files::env_read`]) that read the process's variable, by crate and
/// name, sorted. A read by a crate of the standard library, which reads its own variables
/// alone, or of a variable the crate has of its own ([`own_env_var`]), read none of the
/// process's ([`env_var`]). The log is never cleared, so it holds every read any value of the
/// database was made from, and a few more: what a front end kept from one build to the next is
/// keyed by in the process's environment. By [`CrateKey`]: the log outlives every snapshot.
pub fn process_env_reads(db: &Db) -> Vec<(CrateKey, String)> {
    let mut read = db.files().env_read();
    read.retain(|(key, name)| {
        let krate = db.krate(*key);
        !krate.data(db).origin.is_lang() && own_env_var(db, krate, name).is_none()
    });
    read
}

/// `CARGO_PRIMARY_PACKAGE`, which cargo sets to `1` for the crates of the packages a build was
/// asked for (`is_primary_package`). It is of the request, not of the crate: never among a
/// crate's variables ([`Crate::env`]), so no crate's content, identity or image key holds it.
/// A crate sees it through [`own_env_var`], and whatever is keyed by what a crate made covers
/// it through the `env!` log, as the crate's read of it with its value then
/// ([`primary_crates`]).
pub const PRIMARY_PACKAGE: &str = "CARGO_PRIMARY_PACKAGE";

/// The crates of the request's primary packages ([`PRIMARY_PACKAGE`],
/// [`Edit::set_primary_crates`]), sorted. None of the standard library's: it reads its own
/// variables alone, as a std built elsewhere does.
///
/// The driver's read: it is no input, so a query reads [`is_primary`] of its crate instead,
/// and a request that makes another crate primary drops that crate's arena alone.
pub fn primary_crates(db: &Db) -> std::sync::Arc<[Crate]> {
    std::sync::Arc::clone(&db.snap().table.primary)
}

/// Whether `krate` is of a primary package of the build ([`primary_crates`]).
///
/// Read from the crate's own flag ([`Crate::given`], given by [`Edit::set_primary_crates`]),
/// never from the list: the list is one value for every crate, so a request that makes another
/// crate primary would be a write every crate that asked had read.
pub fn is_primary(db: &Db, krate: Crate) -> bool {
    !krate.data(db).origin.is_lang()
        && krate.given::<PrimaryFlag, bool>(db).copied().unwrap_or(false)
}

/// The kind of a crate's flag "of a primary package" ([`Edit::give`]).
pub(crate) struct PrimaryFlag;

/// The variable `name` as `krate` has it of its own: its variables ([`Crate::env`]), then
/// [`PRIMARY_PACKAGE`] when it is primary ([`is_primary`]).
pub fn own_env_var(db: &Db, krate: Crate, name: &str) -> Option<String> {
    krate
        .env(db)
        .get(name)
        .or_else(|| (name == PRIMARY_PACKAGE && is_primary(db, krate)).then(|| "1".to_owned()))
}

/// The variable `name` as `env!` and `option_env!` in `krate` read it, as rustc reads them:
/// what cargo set for the crate ([`own_env_var`]), then the process the build is for. The
/// standard library's crates read their own alone, as a std built elsewhere does: no
/// workspace's environment is in their identity.
pub fn env_var(db: &Db, krate: Crate, name: &str) -> Option<String> {
    let own = own_env_var(db, krate, name);
    match krate.data(db).origin.is_lang() {
        true => own,
        false => own.or_else(|| process_env_var(db, name)),
    }
}

/// The variables a proc macro called from a crate with the variables `env` is given, as rustc
/// gives them: the crate's, and [`PRIMARY_PACKAGE`] when it is `primary`. One rule for the
/// front end ([`macro_env`]) and for whoever works an expansion's reads out again with no
/// database (a unit record's lookup).
pub fn given_macro_env(env: &Env, primary: bool) -> std::borrow::Cow<'_, Env> {
    match primary {
        false => std::borrow::Cow::Borrowed(env),
        true => {
            let mut given = env.clone(); // clone: a primary crate's variables, once per crate
            given.set(PRIMARY_PACKAGE, "1");
            std::borrow::Cow::Owned(given)
        }
    }
}

/// [`given_macro_env`] of `krate` in `db`: what its proc-macro calls are given.
pub fn macro_env(db: &Db, krate: Crate) -> &Env {
    match is_primary(db, krate) {
        false => krate.env(db),
        true => primary_macro_env(db, krate),
    }
}

/// [`macro_env`] of a primary crate, made once per crate: an `Env` owns its map, dropped with
/// the crate's arena.
#[blaze_memo::tracked(returns(ref), owns)]
fn primary_macro_env(db: &Db, krate: Crate) -> Env {
    given_macro_env(krate.env(db), true).into_owned()
}

static NEXT_NONCE: AtomicUsize = AtomicUsize::new(0);

/// What tells two databases of one process apart ([`Db::nonce`]), and two id spaces of one
/// database: what a holder of ids outside the memo keys by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nonce(usize);

impl Default for Nonce {
    #[inline]
    fn default() -> Self {
        Nonce::new()
    }
}

impl Nonce {
    #[inline]
    pub const fn invalid() -> Nonce {
        Nonce(usize::MAX)
    }

    #[inline]
    pub fn new() -> Nonce {
        Nonce(NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
    }
}

/// Crate related data shared by the whole workspace.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct CrateWorkspaceData {
    pub target: Result<target::TargetData, target::TargetLoadError>,
    /// Toolchain version used to compile the crate.
    pub toolchain: Option<Version>,
    /// How far the evaluation of one constant of the workspace may go.
    pub const_eval: ConstEvalBudget,
    /// The crates the capability effects (`ambient`, `unsafe`, `global_alloc`; hir-ty-pure's
    /// `effect_attrs`) trust: `trusted` under `[workspace.metadata.blaze]`, sorted by name, each
    /// once. Empty for the toolchain's crates, whose trust is their origin's.
    pub trusted: Box<[TrustedCrate]>,
}

/// A crate a workspace trusts for the capability effects: a `#![blaze::capabilities]` crate may
/// reach its code although that code reaches ambient IO, `unsafe` or the global heap, because a
/// person (`owner`) said why (`reason`).
///
/// ```toml
/// [workspace.metadata.blaze.trusted]
/// bstd = { owner = "runtime", reason = "the capabilities themselves: their syscalls and arenas" }
/// ```
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct TrustedCrate {
    /// The crate's name as code names it (`-` written `_`).
    pub name: Box<str>,
    /// Who answers for it.
    pub owner: Box<str>,
    /// Why it is trusted.
    pub reason: Box<str>,
}

impl TrustedCrate {
    /// The key of the table under `[workspace.metadata.blaze]`.
    pub const KEY: &str = "trusted";
}

/// What one evaluation of a constant, static or other compile-time code may spend before it
/// stops with an error: `const-eval` under `[workspace.metadata.blaze]` in the root manifest.
///
/// ```toml
/// [workspace.metadata.blaze]
/// const-eval = { steps = 4_000_000_000, memory = 2_147_483_648 }
/// ```
///
/// rustc's evaluation has no limit of its own past its `long_running_const_eval` lint (at 2
/// million steps, allowed away by `#[allow]`; a step there is a call or a loop's back edge, not
/// a block), so the default is far beyond what code rustc takes without that `#[allow]` ever
/// spends: it is there to end a loop that never would.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct ConstEvalBudget {
    /// Basic blocks run, every one: a bound on any run, loops or not. rustc's lint counts
    /// fewer, only the blocks that end in a call or close a loop.
    pub steps: u64,
    /// Bytes of the evaluation's stack and heap together.
    pub memory: u64,
}

impl ConstEvalBudget {
    /// The key of the table under `[workspace.metadata.blaze]`.
    pub const KEY: &str = "const-eval";
}

impl Default for ConstEvalBudget {
    fn default() -> Self {
        ConstEvalBudget { steps: 1_000_000_000, memory: 1 << 30 }
    }
}

impl CrateWorkspaceData {
    /// Of the target ([`target::TargetData::content`], or 1 and why it could not be loaded),
    /// the toolchain's version, the const-eval budget, and the trusted crates (how many, then
    /// each one's name, owner and reason).
    pub const DOMAIN: &'static str = "blaze v2 crate workspace data 2";

    pub fn content(&self) -> ContentId {
        let mut hasher = Hasher::new(CrateWorkspaceData::DOMAIN);
        match &self.target {
            Ok(target) => hasher.update(&[0]).id(&target.content()),
            Err(error) => hasher.update(&[1]).part(error.to_string().as_bytes()),
        };
        hasher.opt_part(self.toolchain.as_ref().map(Version::to_string));
        hasher
            .update(&self.const_eval.steps.to_le_bytes())
            .update(&self.const_eval.memory.to_le_bytes())
            .update(&(self.trusted.len() as u64).to_le_bytes());
        for trusted in &self.trusted {
            hasher
                .part(trusted.name.as_bytes())
                .part(trusted.owner.as_bytes())
                .part(trusted.reason.as_bytes());
        }
        hasher.finish()
    }

    pub fn is_atleast_187(&self) -> bool {
        const VERSION_187: Version = Version {
            major: 1,
            minor: 87,
            patch: 0,
            pre: Prerelease::EMPTY,
            build: BuildMetadata::EMPTY,
        };
        self.toolchain.as_ref().map_or(false, |v| *v >= VERSION_187)
    }
}

pub fn toolchain_channel(db: &Db, krate: Crate) -> Option<ReleaseChannel> {
    krate.workspace_data(db).toolchain.as_ref().and_then(|v| ReleaseChannel::from_str(&v.pre).ok())
}

/// The crate graph's list: one input of its content ID in the list's own open arena
/// (`world::LIST_ARENA`), which every reader of the list reads ([`list_input`]: its handle is
/// kept in the snapshot's table, as a crate's is; not a singleton, which the engine keeps in
/// the session arena alone), beside the list itself in the snapshot's table ([`all_crates`], [`source_root_crates`], a [`listed_by_prefix`] that
/// found none). The list's content is of each listed crate's item key and root file's path in
/// order (`world::List`, [`CRATE_GRAPH_DOMAIN`]), so a crate added, taken out, put elsewhere
/// or written where its item key moves ([`crate_item_key`]: its cfg, edition, dependencies,
/// ...) makes that arena again, which drops the runs that read it and their borrowers: no
/// crate declares it, and the session does not hold it. A write of a crate's env, workspace
/// data or lints, or of what of its data and extra data the item key leaves out, moves its
/// content ID, not the list: it drops that crate's arena and its dependents' alone.
#[blaze_memo::input(debug)]
pub(crate) struct AllCrates {
    #[returns(copy)]
    content: InputId,
}

/// The snapshot's crate-list input ([`AllCrates`], in `world::LIST_ARENA`): `None` before the
/// first commit made the list's arena. A lookup of the table; reading its `content` is the
/// read of the list (a borrow of the list's arena).
fn list_input(db: &Db) -> Option<AllCrates> {
    db.snap().table.list_input
}

/// The domain of the crate list's digest (`world::List`): each listed crate's item key and
/// root file's path, in topological order.
pub const CRATE_GRAPH_DOMAIN: &str = "blaze v2 crate graph 2";

/// The content ID of [`all_crates`].
pub fn all_crates_content(db: &Db) -> ContentId {
    match list_input(db) {
        Some(all) => all.content(db).id(),
        None => {
            let mut hasher = Hasher::new(CRATE_GRAPH_DOMAIN);
            content::len(&mut hasher, 0);
            hasher.finish()
        }
    }
}

/// Returns the crates in topological order.
///
/// **Warning**: do not use this query in `hir-*` crates! It kills incrementality across crate metadata modifications.
pub fn all_crates(db: &Db) -> std::sync::Arc<[Crate]> {
    match list_input(db) {
        Some(all) => {
            // The read: the list's content, of the list's own arena (`world::LIST_ARENA`).
            _ = all.content(db);
            std::sync::Arc::clone(&db.snap().table.all)
        }
        None => std::sync::Arc::default(),
    }
}

/// The prefix a crate's item key is found by ([`listed_by_prefix`]): its first 8 bytes read
/// little-endian, the crate prefix every item key of the crate begins with (blaze-items'
/// `crate_prefix`).
pub fn key_prefix(item_key: &ContentId) -> u64 {
    u64::from_le_bytes(*item_key.0.first_chunk().expect("a content ID is 32 bytes"))
}

/// The listed crate whose item key ([`crate_item_key`]) begins with `prefix`
/// ([`key_prefix`]), if one does: one binary search of the snapshot's table, made each commit
/// the list moved.
///
/// What it reads is what its answer depends on, and no more:
/// - a crate found is read through its own [`Crate`] input (its data, an input of its own
///   arena): the asker borrows that crate, whose arena a write that moves its item key off
///   `prefix` (its cfg, its dependencies) or takes it out of the list marks. A second crate
///   that took the prefix is a clash, which stops the build ([`listed_clash`]);
/// - none found reads the list ([`AllCrates`], its own open arena): a crate that takes the
///   prefix moves the list, so the runs that found none are dropped with it.
///
/// So a build script's cfg written drops that crate, its dependents and the runs that found
/// no crate; never a crate that found another crate's key. Until round 28 every lookup read a
/// session query of every crate's key (`crates_by_prefix`), so each cfg written, and each
/// crate added, dropped every arena.
///
/// # Panics
///
/// Where the list has a clash ([`listed_clash`]): a key resolved in a build that check stops.
pub fn listed_by_prefix(db: &Db, prefix: u64) -> Option<Crate> {
    let table = &db.snap().table;
    if let Some([krate, other]) = table.clash {
        panic!(
            "base-db: a crate key looked up in a crate list where the crates {krate:?} and {other:?} share \
             the prefix of their item keys (`listed_clash`, which a build checks before any key is resolved)"
        );
    }
    match table.by_prefix.binary_search_by_key(&prefix, |&(it, _)| it) {
        Ok(at) => {
            let key = table.by_prefix[at].1;
            let krate = db.crate_at(key).unwrap_or_else(|| {
                panic!(
                    "base-db: the crate list's prefix index names the crate {key:?}, which this snapshot's \
                     table has no crate for (the index and the table are made in one commit)"
                )
            });
            // The read: the found crate's own input, so the asker borrows its arena.
            _ = krate.data(db);
            Some(krate)
        }
        Err(_) => {
            if let Some(all) = list_input(db) {
                _ = all.content(db);
            }
            None
        }
    }
}

/// The first two listed crates whose item keys share a prefix ([`key_prefix`]), by key; none
/// where every prefix is one crate's. Read off the snapshot's table, outside any run: what a
/// build checks once its crate graph is set, before any key is resolved.
pub fn listed_clash(db: &Db) -> Option<[CrateKey; 2]> {
    db.snap().table.clash
}

/// A kind of gift that says where a value can be read, never what it is: the kind's promise,
/// stated once here, is that every reader of the gift computes the same value with it as
/// without it (an image's lang items are the ones the crate's def map collects; a crate's
/// constants filed under a key are the ones its evaluation gives). So a gift of the kind is
/// no input of any arena: it lives in the snapshot's table ([`Crate::located`]), and its first
/// gift to a crate marks no arena, however much the crate's arena holds
/// ([`Edit::give_located`]). A gift that replaces one under another fact, or a withdrawal
/// ([`Edit::take_located`]), marks the crate: the value held may be one the inputs no longer
/// give, and whatever read it goes with the crate's arena (each reader of a crate's gift
/// reads it in that crate's arena or in a dependent's, each of which borrows it).
///
/// A kind given located is given and read that way only: [`Edit::give`] or [`Edit::give_by`]
/// of a kind a crate holds located stops, as does [`Edit::give_located`] of one it holds as a
/// slot.
pub trait Located: 'static {}

/// The kind of the planted-bug control of the located-gift tests.
#[cfg(test)]
pub(crate) struct LocatedForTests;

#[cfg(test)]
impl Located for LocatedForTests {}

/// Crates whose root file is in `id`, in the crate graph's order.
///
/// What it is made from is read: the list ([`AllCrates`]: a crate's root file is in its
/// content) and the root ([`SourceRootInput::content`]: a file moved into or out of it is in
/// its content). One index of the snapshot's table.
pub fn source_root_crates(db: &Db, id: SourceRootId) -> &[Crate] {
    let Some(all) = list_input(db) else { return &[] };
    _ = all.content(db);
    _ = db.source_root(id).content(db);
    db.snap().table.root_crates.get(&id).map_or(&[], |it| &it[..])
}

/// The domain of [`crate_root_content`] for a crate whose source root holds another crate's
/// `OUT_DIR`.
pub const OWN_ROOT_DOMAIN: &str = "blaze v2 source root content without other crates' OUT_DIRs 1";

/// The `OUT_DIR`s of the other crates of `krate`'s source root, sorted, each without a
/// trailing separator: the directories of that root whose files `krate` is not made from.
/// Kept in the crate's arena.
///
/// A field of the crate's own input, made by the world each commit (`world::out_dirs_of`):
/// until round 28 this query read the list of its root's crates and each one's variables,
/// so every crate's arena read the crate list, and a crate added anywhere (the second `hi`
/// crate of `check` after `run`) dropped every arena. Now a commit marks a crate's arena when
/// its own dirs move (a crate of its root added, taken out or given another `OUT_DIR`), and
/// nothing else of the list reaches it.
///
/// A build script's run is applied by putting the files it left in `OUT_DIR` into its
/// package's source root (`blaze_lower::Lowered::ran`), where the crates given that `OUT_DIR`
/// (the package's library and binaries) `include!` them. The build script's own crate
/// (`build.rs`) is of the same source root and is given no `OUT_DIR`: those files are what
/// its run makes, never what it is made from. Decided by the crates' env, which the plan
/// sets before any run, so it is the same before a run is applied and after.
#[blaze_memo::tracked(returns(copy))]
pub fn other_out_dirs<'db>(db: &'db Db, krate: Crate) -> &'db [&'db str] {
    let memo = db.memo();
    blaze_memo::keep_iter(memo, krate.out_dirs(db).iter().map(|dir| blaze_memo::keep_str(memo, dir)))
}

/// Whether `path` is a file below one of `dirs` ([`other_out_dirs`]).
pub fn below_any(path: &str, dirs: &[&str]) -> bool {
    dirs.iter().any(|dir| path.strip_prefix(dir).is_some_and(|rest| rest.starts_with(['/', '\\'])))
}

/// The content of `krate`'s source root as `krate` is made from it: what a key or an identity
/// of the crate holds for "every file of its source root".
///
/// The invariant: it does not change when a build script's run is applied for a crate the
/// run's `OUT_DIR` was not given to.
///
/// The root's own content ID ([`SourceRootInput::content`]) for a crate whose root holds no
/// other crate's `OUT_DIR` (every crate but a build script's, and those of a package whose
/// script is not run for them). For a crate whose root does ([`other_out_dirs`]): each
/// other file's path and content, sorted by path. Until 2026-11-11 every key took the
/// root's content whole, so a library package's build script was keyed by the files its own
/// run left: its unit identity and its image key moved once it had run, and the first build
/// after a cold one compiled each such script again, asked the store for its image and its
/// dependencies', and so read the standard library from images it had resolved a build
/// before.
///
/// Why leaving them out covers every input (rule 21). A crate reads a file of its source
/// root by a path its source or its env gives it. `OUT_DIR` is a directory of the build
/// directory named by a hash of the build's own plan; the crate's env does not hold it
/// (that is what [`other_out_dirs`] selects by) and its source cannot name it before the
/// plan that hashes that source exists.
#[blaze_memo::tracked(returns(copy))]
pub fn crate_root_content(db: &Db, krate: Crate) -> ContentId {
    let root = db.source_root(db.file_source_root(krate.data(db).root_file_id).source_root_id(db));
    let others = other_out_dirs(db, krate);
    if others.is_empty() {
        return root.content(db);
    }
    let set = root.source_root_ref(db);
    blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
        let mut files = Vec::new_in(frame);
        for file in set.iter() {
            // A file with no path on disk is under no directory.
            let path = set.path_for_file(&file);
            if path.and_then(|it| it.as_path()).is_some_and(|it| below_any(it.as_str(), others)) {
                continue;
            }
            files.push((path.map(|it| it.to_string()).unwrap_or_default(), db.file_content(file)));
        }
        files.sort_unstable();
        let mut hasher = Hasher::new(OWN_ROOT_DOMAIN);
        hasher.update(&[u8::from(set.is_library)]).update(&(files.len() as u64).to_le_bytes());
        for (path, content) in &files {
            hasher.part(path.as_bytes()).id(content);
        }
        hasher.finish()
    })
}

/// The domain of [`crate_root_ported`].
const PORTED_ROOT_DOMAIN: &str = "blaze v2 crate ported files 1";

/// [`crate_root_content`] for a key that must not say where the checkout is: the same files
/// (every file of `krate`'s source root but those under another crate's `OUT_DIR`,
/// [`other_out_dirs`], the one selection both make), each as its path written by
/// [`Files::ported`] and its content ID, sorted by that path. A file with no path on disk is
/// under no directory and has the empty path, as there.
///
/// The invariants: [`crate_root_content`]'s (a run applied for another crate leaves it), and
/// the same files in another checkout, whose paths differ only in where the build's
/// directories are, give the same ID.
#[blaze_memo::tracked(returns(copy))]
pub fn crate_root_ported(db: &Db, krate: Crate) -> ContentId {
    let root = db.source_root(db.file_source_root(krate.data(db).root_file_id).source_root_id(db));
    let others = other_out_dirs(db, krate);
    let set = root.source_root_ref(db);
    let mut files: Vec<(Vec<u8>, ContentId), Global> = Vec::new_in(Global);
    for file in set.iter() {
        let path = set.path_for_file(&file);
        if path.and_then(|it| it.as_path()).is_some_and(|it| below_any(it.as_str(), others)) {
            continue;
        }
        let path = path.map(|it| it.to_string()).unwrap_or_default();
        files.push((db.files().ported(path.as_bytes()).into_owned(), db.file_content(file)));
    }
    files.sort_unstable();
    let mut hasher = Hasher::new(PORTED_ROOT_DOMAIN);
    hasher.update(&[u8::from(set.is_library)]).update(&(files.len() as u64).to_le_bytes());
    for (path, content) in &files {
        hasher.part(path).id(content);
    }
    hasher.finish()
}

pub fn relevant_crates(db: &Db, file_id: FileId) -> &[Crate] {
    source_root_crates(db, db.file_source_root(file_id).source_root_id(db))
}
