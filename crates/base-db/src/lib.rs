//! The front end's one database, [`Db`], and the query inputs it holds: files, source
//! roots and the crate graph.

#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![feature(allocator_api, allocator_ext)]
#![forbid(unsafe_code)]

#[cfg(feature = "in-rust-tree")]
extern crate rustc_driver as _;

mod change;
mod content;
pub mod crate_attrs;
mod db;
mod editioned_file_id;
mod file_set;
mod files;
mod input;
pub mod maintain;
mod named;
mod reparse;
mod source_path;
mod table;
pub mod target;
pub mod trees;
#[cfg(test)]
mod budget_tests;
#[cfg(test)]
mod tests;
mod view;

use std::{str::FromStr as _, sync::atomic::AtomicUsize};

pub use crate::{
    change::FileChange,
    content::{CFG_OPTIONS_DOMAIN, InputId, cfg_options_content},
    crate_attrs::{
        CRATE_ATTR_FILE_NAME, CrateAttrError, applied_crate_attr_files, crate_attr_error,
        crate_attr_files, crate_attr_owner,
    },
    db::{Db, Reader, RunOrder, Shared, executions, set_run_order},
    editioned_file_id::EditionedFileId,
    files::{AliasError, Files},
    input::{
        BuiltCrateData, BuiltDependency, Crate, CrateBuilder, CrateBuilderId, CrateDataBuilder,
        CrateDisplayName, CrateGraphBuilder, CrateName, CrateOrigin, CrateType, CrateTypes,
        CratesIdMap, CratesMap, DependencyBuilder, Env, ExtraCrateData, LangCrateOrigin,
        ProcMacroLoadingError, ProcMacroPaths, ReleaseChannel, SourceRoot, SourceRootId,
        TargetKind, UniqueCrateData,
    },
    maintain::{Maintained, Maintainer, Reads, Written},
    named::NAMED_PATHS_DOMAIN,
    reparse::Reparse,
    view::{ViewChange, ViewError, ViewFile, ViewRoot},
};
pub use crate::{
    file_set::{FileSet, FileSetConfig, FileSetConfigBuilder},
    source_path::{AnchoredPath, AnchoredPathBuf, VfsPath},
};
use blaze_hash::HashSet;
pub use blaze_ids::ContentId;
use blaze_ids::Hasher;
use blaze_query::db::{Durability, Setter};
pub use blaze_query::db::{par_for_each, par_map};
pub use paths::{AbsPath, AbsPathBuf};
pub use semver::{BuildMetadata, Prerelease, Version, VersionReq};
pub use span::FileId;
use triomphe::Arc;

pub type FxIndexSet<T> = indexmap::IndexSet<T, blaze_hash::FixedState>;
pub type FxIndexMap<K, V> =
    indexmap::IndexMap<K, V, blaze_hash::FixedState>;

#[macro_export]
macro_rules! impl_intern_key {
    ($id:ident, $loc:ident) => {
        #[blaze_query::interned(unsafe(no_lifetime))]
        #[derive(PartialOrd, Ord)]
        pub struct $id {
            #[returns(ref)]
            pub loc: $loc,
        }

        // If we derive this the query engine prints the values recursively, and this causes us to blow.
        impl ::std::fmt::Debug for $id {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.debug_tuple(stringify!($id))
                    .field(&format_args!("{:04x}", self.0.as_u32()))
                    .finish()
            }
        }
    };
}

pub const DEFAULT_FILE_TEXT_LRU_CAP: u16 = 16;
pub const DEFAULT_BORROWCK_LRU_CAP: u16 = 2024;

/// The set of roots for crates.io libraries.
/// Files in libraries are assumed to never change.
#[blaze_query::input(singleton, debug)]
pub struct LibraryRoots {
    #[returns(ref)]
    pub roots: HashSet<SourceRootId>,
}

/// The set of "local" (that is, from the current workspace) roots.
/// Files in local roots are assumed to change frequently.
#[blaze_query::input(singleton, debug)]
pub struct LocalRoots {
    #[returns(ref)]
    pub roots: HashSet<SourceRootId>,
}

/// The variables of the process a build is for, which `env!` and `option_env!` read where
/// the crate's own have nothing: those of whoever asked for the build.
/// Its content ID is [`Env::content`] of its variables.
#[blaze_query::input(singleton, debug)]
pub struct ProcessEnv {
    #[returns(ref)]
    #[get(vars_field)]
    #[set(set_vars_field)]
    vars: Env,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
}

impl ProcessEnv {
    pub fn vars<'db, Db: ?Sized + blaze_query::db::Database>(self, db: &'db Db) -> &'db Env {
        self.vars_field(db)
    }

    pub fn content<Db: ?Sized + blaze_query::db::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

/// The variable `key` of the process the build is for.
pub fn process_env_var(db: &Db, key: &str) -> Option<String> {
    ProcessEnv::try_get(db)?.vars(db).get(key)
}

/// Has builds from here on be for a process with the variables `vars`.
pub fn set_process_env(db: &mut Db, vars: Env) {
    db.batch(|db| {
        let content = InputId(vars.content());
        match ProcessEnv::try_get(&*db) {
            Some(env) => {
                env.set_vars_field(db).to(vars);
                if env.content_field(&*db) != content {
                    env.set_content_field(db).to(content);
                    db.note(maintain::Note::Crates);
                }
            }
            None => {
                ProcessEnv::new(db, vars, content);
                db.note(maintain::Note::Crates);
            }
        }
    })
}

/// A file's text. Set only through [`Files`] (and the [`Db`] methods that call it),
/// which keeps [`FileText::content`] the content ID of the text and its appended module.
#[blaze_query::input(debug)]
pub struct FileText {
    #[returns(ref)]
    #[get(text_field)]
    #[set(set_text_field)]
    text: syntax::Text,
    pub file_id: FileId,
    /// The module a crate root's text ends with that the compiler wrote, not the file on disk:
    /// blaze's test harness or list of proc macros. rustc injects its own after it applies
    /// the crate's `#![cfg]`, so a root that `#![cfg]` takes away still has it.
    #[default]
    #[returns(copy)]
    #[get(appended_root_module_field)]
    #[set(set_appended_root_module_field)]
    appended_root_module: Option<&'static str>,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
    /// What [`EditionedFileId::parse`] makes the text's parse from: set with the text, and not
    /// in the content ID, as the parse is the same whatever it holds.
    #[returns(ref)]
    #[get(reparse_field)]
    #[set(set_reparse_field)]
    reparse: Reparse,
}

impl FileText {
    /// A file text's content ID is of the blaze-vfs ID of its text's bytes
    /// (`ContentId::of(blaze_vfs::FILE_DOMAIN, ..)`), then of its appended root module (0, or 1
    /// and the module), which changes what the crate root is. Building on the vfs ID means a
    /// view's file, whose vfs ID is given and checked, is hashed once. The file's ID and path
    /// are not in it.
    pub const DOMAIN: &'static str = "blaze v2 file text 2";
    /// The content ID a file of a source root has when it has no text (bytes that are not
    /// UTF-8, or none loaded): what the front end sees of it is that it has none.
    pub const NO_TEXT_DOMAIN: &'static str = "blaze v2 file without text 1";

    pub fn content_of(text: &str, appended_root_module: Option<&str>) -> ContentId {
        FileText::content_of_file(
            ContentId::of(blaze_vfs::FILE_DOMAIN, text.as_bytes()),
            appended_root_module,
        )
    }

    /// [`FileText::content_of`] from the blaze-vfs ID of the text's bytes, without reading them.
    pub fn content_of_file(file: ContentId, appended_root_module: Option<&str>) -> ContentId {
        let mut hasher = Hasher::new(FileText::DOMAIN);
        hasher.id(&file).opt_part(appended_root_module);
        hasher.finish()
    }

    /// What [`FileText::content_of_file`] hashes under [`FileText::DOMAIN`], appended to `out`:
    /// for hashing many in one batch.
    pub(crate) fn content_bytes(file: &ContentId, appended_root_module: Option<&str>, out: &mut Vec<u8>) {
        out.extend_from_slice(&file.0);
        content::opt_bytes(out, appended_root_module);
    }

    pub fn no_text() -> ContentId {
        files::no_text()
    }

    /// The text, which the file's parse tree shares ([`EditionedFileId::parse`]): the VFS's
    /// bytes as they are, a heap file's allocation or a range of the mapped image.
    pub fn text<'db, Db: ?Sized + blaze_query::db::Database>(
        self,
        db: &'db Db,
    ) -> &'db syntax::Text {
        self.text_field(db)
    }

    /// See the field: the module the compiler appends to a crate root.
    pub fn appended_root_module<Db: ?Sized + blaze_query::db::Database>(
        self,
        db: &Db,
    ) -> Option<&'static str> {
        self.appended_root_module_field(db)
    }

    pub fn content<Db: ?Sized + blaze_query::db::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

/// Where the lines of a file's text are ([`span::LineIndex`]): made the first time a
/// diagnostic, `line!`/`column!`, a caller location, a proc macro's `Span::line` or debug info
/// asks for a line or a column of the text, once per text, and never on load.
#[blaze_query::tracked(returns(ref))]
pub fn line_index(db: &Db, text: FileText) -> span::LineIndex {
    span::LineIndex::new(text.text(db))
}

/// Why a file `include_bytes!` or `include_str!` names could not be read: the error's kind and
/// the OS's code where it has one, which is what rustc's "couldn't read" message prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReadError {
    kind: std::io::ErrorKind,
    os: Option<i32>,
}

impl ReadError {
    pub fn of(error: &std::io::Error) -> ReadError {
        ReadError { kind: error.kind(), os: error.raw_os_error() }
    }
}

impl std::fmt::Display for ReadError {
    /// As `std::io::Error` writes itself: "No such file or directory (os error 2)".
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.os {
            Some(code) => std::fmt::Display::fmt(&std::io::Error::from_raw_os_error(code), f),
            None => std::fmt::Display::fmt(&self.kind, f),
        }
    }
}

/// The contents of a file named by `include_bytes!` or `include_str!`, or why it can't be read.
#[blaze_query::input(debug)]
pub struct IncludedFile {
    #[returns(ref)]
    #[get(read_field)]
    #[set(set_read_field)]
    read: Result<std::sync::Arc<[u8]>, ReadError>,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
}

impl IncludedFile {
    /// Of its bytes (1 and the bytes), or of why it could not be read (0 and the error's
    /// text): a message that names the error changes with it.
    pub const DOMAIN: &'static str = "blaze v2 included file 2";

    pub fn content_of(read: Result<&[u8], &ReadError>) -> ContentId {
        let mut hasher = Hasher::new(IncludedFile::DOMAIN);
        match read {
            // alloc: the error's text, on the error path
            Err(error) => hasher.update(&[0]).part(error.to_string().as_bytes()),
            Ok(bytes) => hasher.update(&[1]).part(bytes),
        };
        hasher.finish()
    }

    pub fn read<'db, Db: ?Sized + blaze_query::db::Database>(
        self,
        db: &'db Db,
    ) -> &'db Result<std::sync::Arc<[u8]>, ReadError> {
        self.read_field(db)
    }

    pub fn bytes<'db, Db: ?Sized + blaze_query::db::Database>(
        self,
        db: &'db Db,
    ) -> Option<&'db std::sync::Arc<[u8]>> {
        self.read_field(db).as_ref().ok()
    }

    pub fn content<Db: ?Sized + blaze_query::db::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

#[blaze_query::input(debug)]
pub struct FileSourceRootInput {
    #[returns(copy)]
    pub source_root_id: SourceRootId,
}

/// A source root. Its content ID is of its files' paths and content IDs and `is_library`
/// ([`SourceRoot::DOMAIN`]), kept by [`Files`] as either changes.
#[blaze_query::input(debug)]
pub struct SourceRootInput {
    #[returns(ref)]
    #[get(source_root_field)]
    #[set(set_source_root_field)]
    source_root: Arc<SourceRoot>,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
}

impl SourceRootInput {
    pub fn source_root<Db: ?Sized + blaze_query::db::Database>(self, db: &Db) -> Arc<SourceRoot> {
        // clone: the owned copy this API hands out; a reader that only looks uses `source_root_ref`.
        self.source_root_field(db).clone()
    }

    /// [`SourceRootInput::source_root`] without a clone of the `Arc` (a write to the count
    /// every reader of the root shares).
    pub fn source_root_ref<'db, Db: ?Sized + blaze_query::db::Database>(self, db: &'db Db) -> &'db SourceRoot {
        self.source_root_field(db)
    }

    pub fn content<Db: ?Sized + blaze_query::db::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }
}

static NEXT_NONCE: AtomicUsize = AtomicUsize::new(0);

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
/// million steps, allowed away by `#[allow]`), so the default is far beyond what code rustc
/// takes without that `#[allow]` ever spends: it is there to end a loop that never would.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct ConstEvalBudget {
    /// Basic blocks run, as rustc counts the terminators it evaluates.
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
    /// the toolchain's version, and the const-eval budget.
    pub const DOMAIN: &'static str = "blaze v2 crate workspace data 1";

    pub fn content(&self) -> ContentId {
        let mut hasher = Hasher::new(CrateWorkspaceData::DOMAIN);
        match &self.target {
            Ok(target) => hasher.update(&[0]).id(&target.content()),
            // alloc: the error's text, once per workspace's content ID (setup, an error path).
            Err(error) => hasher.update(&[1]).part(error.to_string().as_bytes()),
        };
        hasher.opt_part(self.toolchain.as_ref().map(Version::to_string));
        hasher
            .update(&self.const_eval.steps.to_le_bytes())
            .update(&self.const_eval.memory.to_le_bytes())
            .finish()
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

#[blaze_query::input(singleton, debug)]
struct AllCrates {
    #[returns(clone)]
    crates: std::sync::Arc<[Crate]>,
    #[returns(copy)]
    content: InputId,
}

/// The crate graph's content ID is of its crates' content IDs, in topological order.
pub const CRATE_GRAPH_DOMAIN: &str = "blaze v2 crate graph 1";

fn crate_graph_content(db: &Db, crates: &[Crate]) -> ContentId {
    let mut hasher = Hasher::new(CRATE_GRAPH_DOMAIN);
    content::len(&mut hasher, crates.len());
    for krate in crates {
        hasher.id(&krate.content(db));
    }
    hasher.finish()
}

/// The content ID of [`all_crates`].
pub fn all_crates_content(db: &Db) -> ContentId {
    match AllCrates::try_get(db) {
        Some(all) => all.content(db).id(),
        None => crate_graph_content(db, &[]),
    }
}

/// Sets the crate graph's content ID from its crates' as they are.
pub(crate) fn refresh_crate_graph_content(db: &mut Db) {
    let Some(all) = AllCrates::try_get(&*db) else { return };
    let content = InputId(crate_graph_content(&*db, &all.crates(&*db)));
    if all.content(&*db) != content {
        all.set_content(db).to(content);
    }
}

pub fn set_all_crates_with_durability(
    db: &mut Db,
    crates: impl IntoIterator<Item = Crate>,
    durability: Durability,
) {
    // alloc: the crate graph's list, once per set (setup).
    let crates: std::sync::Arc<[Crate]> = crates.into_iter().collect();
    db.write_crates(|db| {
        let content = InputId(crate_graph_content(&*db, &crates));
        match AllCrates::try_get(&*db) {
            Some(all) => {
                all.set_crates(db).with_durability(durability).to(crates);
                all.set_content(db).with_durability(durability).to(content);
            }
            None => {
                _ = AllCrates::builder(crates, content).durability(durability).new(db);
            }
        }
    })
}

/// Returns the crates in topological order.
///
/// **Warning**: do not use this query in `hir-*` crates! It kills incrementality across crate metadata modifications.
pub fn all_crates(db: &Db) -> std::sync::Arc<[Crate]> {
    AllCrates::try_get(db).map_or(std::sync::Arc::default(), |all_crates| all_crates.crates(db))
}

// FIXME: VFS rewrite should allow us to get rid of this wrapper
#[doc(hidden)]
#[blaze_query::interned]
pub struct InternedSourceRootId {
    #[returns(copy)]
    pub id: SourceRootId,
}

/// Crates whose root file is in `id`.
pub fn source_root_crates(db: &Db, id: SourceRootId) -> &[Crate] {
    #[blaze_query::tracked(returns(deref))]
    pub fn source_root_crates(db: &Db, root: SourceRootInput) -> Box<[Crate]> {
        let crates = AllCrates::get(db).crates(db);
        blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
            let mut found = Vec::new_in(frame);
            found.extend(crates.iter().copied().filter(|&krate| {
                let root_file = krate.data(db).root_file_id;
                db.source_root(db.file_source_root(root_file).source_root_id(db)) == root
            }));
            Box::from(&found[..]) // alloc: the memo's value, made again only when the crate graph changes.
        })
    }
    source_root_crates(db, db.source_root(id))
}

pub fn relevant_crates(db: &Db, file_id: FileId) -> &[Crate] {
    source_root_crates(db, db.file_source_root(file_id).source_root_id(db))
}
