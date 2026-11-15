//! This module specifies the input to rust-analyzer. In some sense, this is
//! **the** most important module, because all other fancy stuff is strictly
//! derived from this input.
//!
//! Nothing here does IO: files come in as blaze-vfs views ([`crate::Edit::set_view_root`]) and
//! the crate graph from `blaze-lower`.
//!
//! The crate graph is the world's ([`crate::world::Crates`], by [`CrateKey`]); a [`Crate`] is
//! the snapshot's input made from a crate's entry each time its arena is made, in that arena.
//! Cost (rule 67): a getter is one input read (an index into the arena and the fact noted); a
//! crate's content ID is hashed when a field or a dependency's content moved, never on a read;
//! [`Crate::transitive_deps`] is memoized once per crate per snapshot (O(the crates it
//! reaches)), [`Crate::transitive_rev_deps`] walks the list each call (O(crates + edges)) into
//! the caller's buffer.

use std::error::Error;
use std::str::FromStr;
use std::{fmt, mem, ops};

use crate::{AbsPathBuf, AnchoredPath, FileId, FileSet, VfsPath};
use blaze_hash::{FixedState, HashMap, HashSet};
use blaze_ids::{ContentId, Hasher, radix};
use blaze_lint_defs::LintOptions;
use blaze_mem::Frame;
use blaze_memo::{Database as _, Id as _};
use cfg::CfgOptions;
use intern::Symbol;
use la_arena::{Arena, Idx, RawIdx};
use span::Edition;
use std::alloc::{Allocator, Global};
use triomphe::Arc;

use crate::{
    CrateWorkspaceData, Db, EditionedFileId, all_crates,
    content::{self, InputId, cfg_options_content},
    world::CrateKey,
};

pub type ProcMacroPaths =
    HashMap<CrateBuilderId, Result<(String, AbsPathBuf), ProcMacroLoadingError>, Global>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)] // clone: an error, cold
pub enum ProcMacroLoadingError {
    Disabled,
    FailedToBuild,
    ExpectedProcMacroArtifact,
    MissingDylibPath,
    NotYetBuilt,
    /// blaze-macro-eval cannot run the crate's macros; the message names the crate and why.
    Refused(Box<str>),
}
impl ProcMacroLoadingError {
    pub fn is_hard_error(&self) -> bool {
        match self {
            ProcMacroLoadingError::Disabled | ProcMacroLoadingError::NotYetBuilt => false,
            ProcMacroLoadingError::ExpectedProcMacroArtifact
            | ProcMacroLoadingError::FailedToBuild
            | ProcMacroLoadingError::MissingDylibPath
            | ProcMacroLoadingError::Refused(_) => true,
        }
    }
}

impl Error for ProcMacroLoadingError {}
impl fmt::Display for ProcMacroLoadingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcMacroLoadingError::ExpectedProcMacroArtifact => {
                write!(f, "proc-macro crate did not build proc-macro artifact")
            }
            ProcMacroLoadingError::Disabled => write!(f, "proc-macro expansion is disabled"),
            ProcMacroLoadingError::FailedToBuild => write!(f, "proc-macro failed to build"),
            ProcMacroLoadingError::MissingDylibPath => {
                write!(
                    f,
                    "proc-macro crate built but the dylib path is missing, this indicates a problem with your build system."
                )
            }
            ProcMacroLoadingError::NotYetBuilt => write!(f, "proc-macro not yet built"),
            ProcMacroLoadingError::Refused(msg) => f.write_str(msg),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceRootId(pub u32);

/// Files are grouped into source roots. A source root is a directory on the
/// file systems which is watched for changes. Typically it corresponds to a
/// Rust crate. Source roots *might* be nested: in this case, a file belongs to
/// the nearest enclosing source root. Paths to files are always relative to a
/// source root, and the analyzer does not know the root path of the source root at
/// all. So, a file from one source root can't refer to a file in another source
/// root by path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRoot {
    /// Sysroot or crates.io library.
    ///
    /// Libraries are considered mostly immutable, this assumption is used to
    /// optimize the query engine's dependency structure
    pub is_library: bool,
    file_set: FileSet,
}

impl SourceRoot {
    /// A source root's content ID is of `is_library` and the hash of its trie's root node. The
    /// trie is over the [`SourceRoot::PATH_DOMAIN`] hashes of its files' paths, a hex digit a
    /// level from the first: a node with at most [`SourceRoot::LEAF`] files (or at the 64th
    /// digit) is a leaf, hashed with [`SourceRoot::LEAF_DOMAIN`] as its file count and its
    /// files sorted by path, each as its path and its content ID
    /// ([`FileText::content`](crate::FileText::content), or
    /// [`FileText::no_text`](crate::FileText::no_text)); any other has a child for each of
    /// [`SourceRoot::FANOUT`] digits and is hashed with [`SourceRoot::NODE_DOMAIN`] as its
    /// children's hashes in order, one with no file as an empty leaf's. A function of the
    /// (path, content ID) pairs and `is_library` alone, kept current in `O(log files)` an edit.
    pub const DOMAIN: &'static str = "blaze v2 source root 2";
    pub const LEAF_DOMAIN: &'static str = "blaze v2 source root leaf 1";
    pub const NODE_DOMAIN: &'static str = "blaze v2 source root node 1";
    pub const PATH_DOMAIN: &'static str = "blaze v2 source root path 1";
    pub const FANOUT: usize = 16;
    pub const LEAF: usize = 16;

    pub fn new_local(file_set: FileSet) -> SourceRoot {
        SourceRoot { is_library: false, file_set }
    }

    pub fn new_library(file_set: FileSet) -> SourceRoot {
        SourceRoot { is_library: true, file_set }
    }

    pub fn path_for_file(&self, file: &FileId) -> Option<&VfsPath> {
        self.file_set.path_for_file(file)
    }

    pub fn file_for_path(&self, path: &VfsPath) -> Option<&FileId> {
        self.file_set.file_for_path(path)
    }

    pub fn resolve_path(&self, path: AnchoredPath<'_>) -> Option<FileId> {
        self.file_set.resolve_path(path)
    }

    pub fn iter(&self) -> impl Iterator<Item = FileId> + '_ {
        self.file_set.iter()
    }

    /// See [`FileSet::insert`].
    pub(crate) fn insert_file(&mut self, file: FileId, path: VfsPath) {
        self.file_set.insert(file, path);
    }

    /// See [`FileSet::remove`].
    pub(crate) fn remove_file(&mut self, file: FileId) -> Option<VfsPath> {
        self.file_set.remove(file)
    }

    /// See [`FileSet::alias`].
    pub(crate) fn alias_file(&mut self, alias: FileId, of: FileId) -> bool {
        self.file_set.alias(alias, of)
    }

    /// See [`FileSet::aliases`].
    pub fn aliases(&self) -> impl Iterator<Item = (FileId, FileId)> + '_ {
        self.file_set.aliases()
    }

    /// See [`FileSet::own_path`].
    pub(crate) fn own_path(&self, file: &FileId) -> Option<&VfsPath> {
        self.file_set.own_path(file)
    }

    /// See [`FileSet::alias_target`].
    pub(crate) fn alias_target(&self, alias: &FileId) -> Option<FileId> {
        self.file_set.alias_target(alias)
    }

    pub(crate) fn len(&self) -> usize {
        self.file_set.len()
    }
}

#[derive(Default, Clone)] // clone: setup: the crate graph, once per load
pub struct CrateGraphBuilder {
    arena: Arena<CrateBuilder>,
}

pub type CrateBuilderId = Idx<CrateBuilder>;

impl ops::Index<CrateBuilderId> for CrateGraphBuilder {
    type Output = CrateBuilder;

    fn index(&self, index: CrateBuilderId) -> &Self::Output {
        &self.arena[index]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrateBuilder {
    pub basic: CrateDataBuilder,
    pub extra: ExtraCrateData,
    pub cfg_options: CfgOptions,
    pub env: Env,
    ws_data: Arc<CrateWorkspaceData>,
}

impl CrateBuilder {
    /// What the crate shares with every crate of its target.
    pub(crate) fn workspace(&self) -> &Arc<CrateWorkspaceData> {
        &self.ws_data
    }
}

impl fmt::Debug for CrateGraphBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.arena.iter().map(|(id, data)| (u32::from(id.into_raw()), data)))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CrateName(Symbol);

impl CrateName {
    /// Creates a crate name, checking for dashes in the string provided.
    /// Dashes are not allowed in the crate names,
    /// hence the input string is returned as `Err` for those cases.
    pub fn new(name: &str) -> Result<CrateName, &str> {
        if name.contains('-') { Err(name) } else { Ok(Self(Symbol::intern(name))) }
    }

    /// Creates a crate name, unconditionally replacing the dashes with underscores.
    pub fn normalize_dashes(name: &str) -> CrateName {
        Self(Symbol::intern(&name.replace('-', "_")))
    }

    pub fn symbol(&self) -> &Symbol {
        &self.0
    }
}

impl fmt::Display for CrateName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl ops::Deref for CrateName {
    type Target = Symbol;
    fn deref(&self) -> &Symbol {
        &self.0
    }
}

/// A crate set up with no name where its name is needed ([`Crate::rust_name`]): in a path, a
/// symbol, a key or a lookup by name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct UnnamedCrate {
    /// The crate's content ID ([`Crate::content`]), which tells it from every other crate.
    pub content: ContentId,
}

impl fmt::Display for UnnamedCrate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{unnamed crate {}}}", self.content)
    }
}

impl Error for UnnamedCrate {}

/// A crate as [`Crate::named`] says it: its name, or the [`UnnamedCrate`] it is.
#[derive(Clone, Copy, Debug)]
pub struct CrateNamed<'db>(Result<&'db Symbol, UnnamedCrate>);

impl fmt::Display for CrateNamed<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Ok(name) => f.write_str(name.as_str()),
            Err(unnamed) => unnamed.fmt(f),
        }
    }
}

/// Origin of the crates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)] // clone: setup: the crate graph, once per load
pub enum CrateOrigin {
    /// Crates that are from the rustc workspace.
    Rustc { name: Symbol },
    /// Crates that are workspace members.
    Local { repo: Option<String>, name: Option<Symbol> },
    /// Crates that are non member libraries.
    Library { repo: Option<String>, name: Symbol },
    /// Crates that are provided by the language, like std, core, proc-macro, ...
    Lang(LangCrateOrigin),
}

impl CrateOrigin {
    pub fn is_local(&self) -> bool {
        matches!(self, CrateOrigin::Local { .. })
    }

    pub fn is_lib(&self) -> bool {
        matches!(self, CrateOrigin::Library { .. })
    }

    pub fn is_lang(&self) -> bool {
        matches!(self, CrateOrigin::Lang { .. })
    }

    /// What a crate's content ID and keys hash of its origin: a tag, then its repo and name.
    pub fn hash_into<'h>(&self, hasher: &'h mut Hasher) -> &'h mut Hasher {
        match self {
            CrateOrigin::Rustc { name } => hasher.update(&[0]).part(name.as_str().as_bytes()),
            CrateOrigin::Local { repo, name } => hasher
                .update(&[1])
                .opt_part(repo.as_deref())
                .opt_part(name.as_ref().map(Symbol::as_str)),
            CrateOrigin::Library { repo, name } => {
                hasher.update(&[2]).opt_part(repo.as_deref()).part(name.as_str().as_bytes())
            }
            CrateOrigin::Lang(lang) => hasher.update(&[3, lang.tag()]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LangCrateOrigin {
    Alloc,
    Core,
    ProcMacro,
    Std,
    Test,
    Dependency,
    Other,
}

impl LangCrateOrigin {
    /// The byte a hash of the origin writes for it.
    pub fn tag(self) -> u8 {
        match self {
            LangCrateOrigin::Alloc => 0,
            LangCrateOrigin::Core => 1,
            LangCrateOrigin::ProcMacro => 2,
            LangCrateOrigin::Std => 3,
            LangCrateOrigin::Test => 4,
            LangCrateOrigin::Dependency => 5,
            LangCrateOrigin::Other => 6,
        }
    }
}

impl From<&str> for LangCrateOrigin {
    fn from(s: &str) -> Self {
        match s {
            "alloc" => LangCrateOrigin::Alloc,
            "core" => LangCrateOrigin::Core,
            "proc-macro" | "proc_macro" => LangCrateOrigin::ProcMacro,
            "std" => LangCrateOrigin::Std,
            "test" => LangCrateOrigin::Test,
            _ => LangCrateOrigin::Other,
        }
    }
}

impl fmt::Display for LangCrateOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            LangCrateOrigin::Alloc => "alloc",
            LangCrateOrigin::Core => "core",
            LangCrateOrigin::ProcMacro => "proc_macro",
            LangCrateOrigin::Std => "std",
            LangCrateOrigin::Test => "test",
            LangCrateOrigin::Other | LangCrateOrigin::Dependency => "other",
        };
        f.write_str(text)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CrateDisplayName {
    /// The name we use to display various paths (with `_`).
    crate_name: CrateName,
    /// The name as self-declared by the crate. For example, the name declared in the manifest of
    /// the crate. This may contain dashes `-`.
    canonical_name: Symbol,
}

impl CrateDisplayName {
    pub fn canonical_name(&self) -> &Symbol {
        &self.canonical_name
    }
    pub fn crate_name(&self) -> &CrateName {
        &self.crate_name
    }
}

impl From<CrateName> for CrateDisplayName {
    fn from(crate_name: CrateName) -> CrateDisplayName {
        // clone: a crate name is built once (setup).
        let canonical_name = crate_name.0.clone();
        CrateDisplayName { crate_name, canonical_name }
    }
}

impl fmt::Display for CrateDisplayName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.crate_name.fmt(f)
    }
}

impl ops::Deref for CrateDisplayName {
    type Target = Symbol;
    fn deref(&self) -> &Symbol {
        &self.crate_name
    }
}

impl CrateDisplayName {
    pub fn from_canonical_name(canonical_name: &str) -> CrateDisplayName {
        let crate_name = CrateName::normalize_dashes(canonical_name);
        CrateDisplayName { crate_name, canonical_name: Symbol::intern(canonical_name) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetKind {
    Bin,
    /// Any kind of Cargo lib crate-type (dylib, rlib, proc-macro, ...).
    Lib {
        /// Is this target a proc-macro
        is_proc_macro: bool,
    },
    Example,
    Test,
    Bench,
    /// Cargo calls this kind `custom-build`
    BuildScript,
    Other,
}

impl TargetKind {
    pub fn is_executable(self) -> bool {
        matches!(self, TargetKind::Bin | TargetKind::Example)
    }

    pub fn is_proc_macro(self) -> bool {
        matches!(self, TargetKind::Lib { is_proc_macro: true })
    }

    /// The byte a crate's content ID and dependency key write for it.
    pub fn tag(self) -> u8 {
        match self {
            TargetKind::Bin => 0,
            TargetKind::Lib { is_proc_macro: false } => 1,
            TargetKind::Lib { is_proc_macro: true } => 2,
            TargetKind::Example => 3,
            TargetKind::Test => 4,
            TargetKind::Bench => 5,
            TargetKind::BuildScript => 6,
            TargetKind::Other => 7,
        }
    }

    /// If this is a valid cargo target, returns the name cargo uses in command line arguments
    /// and output, otherwise None.
    /// <https://docs.rs/cargo_metadata/latest/cargo_metadata/enum.TargetKind.html>
    pub fn as_cargo_target(self) -> Option<&'static str> {
        match self {
            TargetKind::Bin => Some("bin"),
            TargetKind::Lib { is_proc_macro: true } => Some("proc-macro"),
            TargetKind::Lib { is_proc_macro: false } => Some("lib"),
            TargetKind::Example => Some("example"),
            TargetKind::Test => Some("test"),
            TargetKind::Bench => Some("bench"),
            TargetKind::BuildScript => Some("custom-build"),
            TargetKind::Other => None,
        }
    }

    /// The crate types cargo passes (`--crate-type`) for a target of this kind that names none
    /// in its manifest; none for [`TargetKind::Other`], which is no cargo target, so rustc reads
    /// `#![crate_type]` and else builds a binary.
    pub fn default_crate_types(self) -> CrateTypes {
        match self {
            TargetKind::Lib { is_proc_macro: true } => CrateTypes::of(&[CrateType::ProcMacro]),
            TargetKind::Lib { is_proc_macro: false } => CrateTypes::of(&[CrateType::Lib]),
            TargetKind::Bin
            | TargetKind::Example
            | TargetKind::Test
            | TargetKind::Bench
            | TargetKind::BuildScript => CrateTypes::of(&[CrateType::Bin]),
            TargetKind::Other => CrateTypes::default(),
        }
    }
}

/// One of rustc's `--crate-type`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum CrateType {
    Bin,
    Lib,
    Rlib,
    Dylib,
    Cdylib,
    Staticlib,
    ProcMacro,
}

impl CrateType {
    pub const ALL: [CrateType; 7] = [
        CrateType::Bin,
        CrateType::Lib,
        CrateType::Rlib,
        CrateType::Dylib,
        CrateType::Cdylib,
        CrateType::Staticlib,
        CrateType::ProcMacro,
    ];

    /// rustc's name of it, as `--crate-type` and `#![crate_type]` take it.
    pub fn as_str(self) -> &'static str {
        match self {
            CrateType::Bin => "bin",
            CrateType::Lib => "lib",
            CrateType::Rlib => "rlib",
            CrateType::Dylib => "dylib",
            CrateType::Cdylib => "cdylib",
            CrateType::Staticlib => "staticlib",
            CrateType::ProcMacro => "proc-macro",
        }
    }
}

/// The crate types a crate is built as, a set: a bit per [`CrateType`], by its number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CrateTypes(u8);

impl CrateTypes {
    pub fn of(types: &[CrateType]) -> CrateTypes {
        CrateTypes(types.iter().fold(0, |bits, &it| bits | 1 << it as u8))
    }

    pub fn contains(self, it: CrateType) -> bool {
        self.0 & 1 << it as u8 != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The types, in [`CrateType`]'s order.
    pub fn iter(self) -> impl Iterator<Item = CrateType> {
        CrateType::ALL.into_iter().filter(move |&it| self.contains(it))
    }

    /// The set as its bits, for hashing and persisting.
    pub fn bits(self) -> u8 {
        self.0
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ReleaseChannel {
    Stable,
    Beta,
    Nightly,
}

impl ReleaseChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            ReleaseChannel::Stable => "stable",
            ReleaseChannel::Beta => "beta",
            ReleaseChannel::Nightly => "nightly",
        }
    }
}

impl FromStr for ReleaseChannel {
    type Err = ();

    fn from_str(str: &str) -> Result<Self, Self::Err> {
        Ok(match str {
            "" | "stable" => ReleaseChannel::Stable,
            "nightly" => ReleaseChannel::Nightly,
            _ if str.starts_with("beta") => ReleaseChannel::Beta,
            _ => return Err(()),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)] // clone: setup: the crate graph, once per load
pub struct CrateData<Id> {
    pub root_file_id: FileId,
    pub edition: Edition,
    /// The dependencies of this crate.
    ///
    /// Note that this may contain more dependencies than the crate actually uses.
    /// A common example is the test crate which is included but only actually is active when
    /// declared in source via `extern crate test`.
    pub dependencies: Vec<Dependency<Id>>,
    pub origin: CrateOrigin,
    /// The crate attributes given on the command line (rustc's `-Z crate-attr`), each with
    /// `#![` and `]` around it: each is a file of its own, see [`crate::crate_attrs`].
    pub crate_attrs: Box<[Box<str>]>,
    /// What the crate is: a `--test` crate is a [`TargetKind::Test`] (a bench's a
    /// [`TargetKind::Bench`]), whatever target it is built from.
    pub target_kind: TargetKind,
    /// The kind of the manifest's target the crate is built from: an example, a binary or a
    /// library built with `--test` is a [`TargetKind::Test`] crate of an `Example`, `Bin` or
    /// `Lib` target. With its name it says which of its package's targets the crate is, as
    /// cargo's units tell targets apart (`tests/fetch.rs` and `examples/fetch.rs` are two).
    /// [`TargetKind::Other`] for no cargo target; equal to `target_kind` when not `--test`.
    pub manifest_kind: TargetKind,
    /// The crate types it is built as (rustc's `--crate-type`s): cargo's target's
    /// `crate_types`. Empty when none is given, as for rustc with no `--crate-type`.
    pub crate_types: CrateTypes,
    /// The working directory to run proc-macros in invoked in the context of this crate.
    /// This is the workspace root of the cargo workspace for workspace members, the crate manifest
    /// dir otherwise.
    // FIXME: This ought to be a `VfsPath` or something opaque.
    pub proc_macro_cwd: Arc<AbsPathBuf>,
    /// What pins its sources, when they cannot change: the digest of the tree it is read from
    /// (a tree the toolchain ships, a registry package at its checksum, a git package at the
    /// commit its lock file pins), as the driver's one rule decides it (blaze-cargo's
    /// `serve::sealed::pin`) and the planner writes it, at the crate's durability. `None` for
    /// a workspace member, a path dependency and a doctest crate, whatever their origin.
    /// `hir_def::deps::sealed` reads it and nothing else. Not in the crate's content ID: it is
    /// a function of the sources, which every key over the crate already hashes (its root's
    /// path, its files), and it says where the crate's records are kept, never what they hold.
    pub sealed: Option<ContentId>,
}

pub type CrateDataBuilder = CrateData<CrateBuilderId>;
pub type BuiltCrateData = CrateData<Crate>;

impl CrateData<Crate> {
    /// This data (a crate's, as a snapshot's input holds it) with each dependency by its
    /// world key ([`Crate::key`]): what [`crate::Edit::set_crate_data`] takes, so a caller
    /// changes one field of a crate's data by reading it, setting the field and giving it back.
    /// Every field is kept as it is, the dependencies' prelude, sysroot and private flags too.
    pub fn keyed(&self, db: &Db) -> crate::WorldCrateData {
        let mut dependencies = Vec::with_capacity_in(self.dependencies.len(), Global);
        dependencies.extend(self.dependencies.iter().map(|dep| Dependency {
            crate_id: dep.crate_id.key(db),
            name: dep.name.clone(), // clone: a crate's name, once per dependency (setup)
            prelude: dep.prelude,
            sysroot: dep.sysroot,
            private: dep.private,
        }));
        CrateData {
            root_file_id: self.root_file_id,
            edition: self.edition,
            dependencies,
            origin: self.origin.clone(), // clone: as above
            crate_attrs: self.crate_attrs.clone(), // clone: as above
            target_kind: self.target_kind,
            manifest_kind: self.manifest_kind,
            crate_types: self.crate_types,
            proc_macro_cwd: self.proc_macro_cwd.clone(), // clone: an `Arc`
            sealed: self.sealed,
        }
    }
}

/// Crate data unrelated to analysis.
#[derive(Debug, Clone, PartialEq, Eq)] // clone: setup: the crate graph, once per load
pub struct ExtraCrateData {
    pub version: Option<String>,
    /// A name used in the package's project declaration: for Cargo projects,
    /// its `[package].name` can be different for other project types or even
    /// absent (a dummy crate for the code snippet, for example).
    ///
    /// For purposes of analysis, crates are anonymous (only names in
    /// `Dependency` matters), this name should only be used for UI.
    pub display_name: Option<CrateDisplayName>,
    /// The cfg options that could be used by the crate
    pub potential_cfg_options: Option<CfgOptions>,
    /// What it is built for, as cargo's units tell two builds of one package apart.
    pub built_for: BuiltFor,
    /// Stable unit identities of the build-script runs this crate reads, in dependency order.
    pub build_script_runs: Box<[String]>,
}

/// What a crate is built for: the host (a build without `--target`, or a build script and
/// what it depends on), a target triple, or a triple for blaze's wasm proc macros. Two crates
/// alike in all else but this are two builds, as cargo keeps them (its `CompileKind`): a
/// `--target` of the host's own triple builds the standard library for the host and the
/// target, and a `--target wasm32-wasip1` builds it for the target and for the proc macros,
/// each with its own `OUT_DIR` and build script run. A crate's keys cover it.
///
/// The front end's copy of `blaze_units::schema::BuiltFor`, the source of truth, which this
/// crate cannot depend on: a build's crates get it only from `blaze_lower::crates::built_for`,
/// and it prints the same text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)] // clone: setup: the crate graph, once per load
pub struct BuiltFor {
    /// The triple; none for the host's own units.
    pub platform: Option<Symbol>,
    /// Whether it is built to run as a wasm proc macro (or for one).
    pub macro_wasm: bool,
}

impl BuiltFor {
    /// What a crate's content ID and keys hash of it: the triple, or none, then the flag.
    pub fn hash_into<'h>(&self, hasher: &'h mut Hasher) -> &'h mut Hasher {
        hasher
            .opt_part(self.platform.as_ref().map(Symbol::as_str))
            .update(&[u8::from(self.macro_wasm)])
    }
}

impl fmt::Display for BuiltFor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.platform, self.macro_wasm) {
            (None, _) => f.write_str("the host"),
            (Some(triple), false) => f.write_str(triple.as_str()),
            (Some(triple), true) => write!(f, "{} (proc macros)", triple.as_str()),
        }
    }
}

#[derive(Default, Clone, PartialEq, Eq)] // clone: setup: the crate graph, once per load
pub struct Env {
    entries: HashMap<String, String, Global>,
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct EnvDebug<'s>(Vec<(&'s String, &'s String)>);

        impl fmt::Debug for EnvDebug<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_map().entries(self.0.iter().copied()).finish()
            }
        }
        f.debug_struct("Env")
            .field("entries", &{
                // alloc: `Debug` output (the edge).
                let mut entries: Vec<_> = self.entries.iter().collect();
                entries.sort();
                EnvDebug(entries)
            })
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Dependency<Id> {
    pub crate_id: Id,
    pub name: CrateName,
    pub(crate) prelude: bool,
    pub(crate) sysroot: bool,
    pub(crate) private: bool,
}

pub type DependencyBuilder = Dependency<CrateBuilderId>;
pub type BuiltDependency = Dependency<Crate>;

/// A dependency as the world keeps it: on a crate by its key ([`crate::Edit::create_crate`]'s
/// data, [`crate::WorldCrateData`]).
pub type WorldDependency = Dependency<CrateKey>;

/// One home for the constructors of every form (a graph builder's, a snapshot's, the world's):
/// the form is the crate id's type.
impl<Id> Dependency<Id> {
    /// A dependency on `crate_id` by `name`, in the extern prelude, not a sysroot one.
    pub fn new(name: CrateName, crate_id: Id) -> Self {
        Self { name, crate_id, prelude: true, sysroot: false, private: false }
    }

    /// A dependency on `crate_id` by `name`, saying whether it is in the extern prelude (not
    /// for cargo's `noprelude`) and whether it is a sysroot crate.
    pub fn with_prelude(name: CrateName, crate_id: Id, prelude: bool, sysroot: bool) -> Self {
        Self { name, crate_id, prelude, sysroot, private: false }
    }

    /// The dependency, private (rustc's `--extern priv:name`, which cargo passes for a
    /// dependency without `public = true` under `-Zpublic-dependency`) or not.
    pub fn with_private(self, private: bool) -> Self {
        Self { private, ..self }
    }

    /// Whether this dependency is to be added to the depending crate's extern prelude.
    pub fn is_prelude(&self) -> bool {
        self.prelude
    }

    /// Whether this dependency is a sysroot injected one.
    pub fn is_sysroot(&self) -> bool {
        self.sysroot
    }

    /// Whether this dependency is private (`--extern priv:name`): what
    /// `exported_private_dependencies` reads.
    pub fn is_private(&self) -> bool {
        self.private
    }
}

/// Each crate of a [`CrateGraphBuilder`] as the world keyed it ([`crate::Edit::set_crate_graph`]).
pub type CratesIdMap = HashMap<CrateBuilderId, CrateKey, Global>;

/// A crate, as one snapshot holds it: an input the world makes in the crate's own arena from
/// its [`CrateKey`]'s entry each time that arena is made. The world keeps the entry and
/// [`Crate::content`], the content ID of its fields and of its dependencies' content IDs
/// ([`keyed_content`]); nothing here writes a field.
#[blaze_memo::input(debug)]
pub struct Crate {
    pub data: BuiltCrateData,
    /// Crate data that is not needed for analysis.
    pub extra_data: ExtraCrateData,
    /// Shared by every crate of a workspace.
    pub workspace_data: Arc<CrateWorkspaceData>,
    pub cfg_options: CfgOptions,
    pub env: Env,
    #[returns(copy)]
    #[get(content_field)]
    content: InputId,
    /// The files of its crate attributes ([`crate::crate_attrs`]), kept with its data: not in
    /// its content ID, which has the attributes' text.
    pub(crate) attr_files: Box<[FileId]>,
    /// The lint flags its compile is given: rustc's `-A`/`-W`/`--force-warn`/`-D`/`-F` in order
    /// and `--cap-lints` (rustc_session's `lint_opts` and `lint_cap`). Read only by
    /// `hir_lint_levels`' level queries; in its content ID, which persisted results are keyed by.
    pub lint_options: LintOptions,
    /// What the crates above base-db gave this crate, a slot a kind
    /// ([`crate::Edit::give`]). Not in its content ID.
    #[get(slots_field)]
    slots: Vec<(std::any::TypeId, CrateSlot)>,
    /// The `OUT_DIR`s of the other listed crates of its root file's source root, each without
    /// a trailing separator, its own left out, sorted, each once ([`crate::other_out_dirs`]).
    /// Made by the world each commit from every crate's variables (`world::out_dirs_of`); a
    /// commit where they moved marks this crate's arena alone. Not in its content ID: what it
    /// selects is in [`crate::crate_root_content`].
    #[get(out_dirs_field)]
    out_dirs: crate::world::Dirs,
}

/// One thing given to one crate ([`crate::Edit::give`]): an input of its own, in the crate's
/// arena, so a gift is a change of that crate's alone.
#[blaze_memo::input(debug)]
pub struct CrateSlot {
    pub value: Given,
}

impl Crate {
    /// The crate's other `OUT_DIR`s as the world made them at the last commit (the field's
    /// getter is this module's own): what [`crate::other_out_dirs`] keeps for its callers.
    pub(crate) fn out_dirs(self, db: &Db) -> &crate::world::Dirs {
        self.out_dirs_field(db)
    }

    /// What was given to this crate as kind `K` ([`crate::Edit::give`]), if it is a `T`;
    /// `None` when nothing was. Reads the crate's list of slots and the one slot, both facts of
    /// the crate's own arena: a gift to another crate or of another kind is not read.
    ///
    /// The invariant: what is "one value a crate" is kept a crate, never as an entry of one
    /// map of every crate. A map of every crate is an input every crate's rows read, so
    /// giving one crate its value was a write all of them verified after (the proc-macro
    /// map: a build of reth with nothing changed, 53 writes and 23 million rows verified,
    /// 2026-11-12).
    pub fn given<K: 'static, T: std::any::Any>(self, db: &Db) -> Option<&T> {
        let kind = std::any::TypeId::of::<K>();
        let slot = self.slots_field(db).iter().find(|it| it.0 == kind)?.1;
        slot.value(db).get::<T>()
    }

    /// What was given to this crate as the located kind `K` ([`crate::Edit::give_located`]), if
    /// it is a `T`; `None` when nothing was. Read off the snapshot's table, no input of any
    /// arena: no reader depends on it, which the kind's promise makes sound ([`crate::Located`]:
    /// a reader computes the same value with the gift as without it). The giver's fact is
    /// listed as read (`blaze_memo::read`), so a read list kept for a store names it.
    pub fn located<K: crate::Located, T: std::any::Any>(self, db: &Db) -> Option<&T> {
        let kind = std::any::TypeId::of::<K>();
        let row = db.snap().table.located.get(self.key(db).0 as usize)?;
        let (_, fact, value) = row.iter().find(|it| it.0 == kind)?;
        blaze_memo::read(blaze_memo::Database::memo(db), *fact);
        value.get::<T>()
    }

    /// The world's key of this crate: what an edit names it by, the same in every snapshot.
    pub fn key(self, db: &Db) -> CrateKey {
        db.snap().table.key_of(self.gid().arena())
    }
}

/// The most kinds of [`CrateSlot`] whose writes are counted by kind ([`take_given`]); a write
/// of a kind past them is counted under [`GIVEN_MORE`], never dropped.
const GIVEN_KINDS: usize = 32;

/// The name [`take_given`] gives the writes of kinds past [`GIVEN_KINDS`].
pub const GIVEN_MORE: &str = "(a kind past the 32 counted)";

/// Writes of crate slots since the last [`take_given`], by kind: the kind's type name, set by
/// its first write, and its writes. Process-wide, as the query engine's counts are.
static GIVEN: [(std::sync::OnceLock<&'static str>, std::sync::atomic::AtomicU64); GIVEN_KINDS] =
    [const { (std::sync::OnceLock::new(), std::sync::atomic::AtomicU64::new(0)) }; GIVEN_KINDS];
static GIVEN_PAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One write of a slot of `kind`: a gift ([`crate::Edit::give`]) that changed a slot the
/// crate already had (a kind's first gift is none). Who wrote a slot (a plan's proc macros, a
/// resolution blaze published) is told by its kind, counted here.
pub(crate) fn count_given(kind: &'static str) {
    use std::sync::atomic::Ordering;
    // Bounded: `GIVEN_KINDS` entries.
    for (name, count) in &GIVEN {
        if *name.get_or_init(|| kind) == kind {
            count.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    GIVEN_PAST.fetch_add(1, Ordering::Relaxed);
}

/// Gives `each` every kind of crate slot written since the last take with its writes (the
/// kind as `std::any::type_name` of [`crate::Edit::give`]'s `K`), and counts from nothing again:
/// what a build's account attributes its `CrateSlot` writes by (blaze-profile's `account`).
pub fn take_given(mut each: impl FnMut(&'static str, u64)) {
    use std::sync::atomic::Ordering;
    for (name, count) in &GIVEN {
        let Some(name) = name.get() else { break };
        let writes = count.swap(0, Ordering::Relaxed);
        if writes > 0 {
            each(name, writes);
        }
    }
    let past = GIVEN_PAST.swap(0, Ordering::Relaxed);
    if past > 0 {
        each(GIVEN_MORE, past);
    }
}

/// What a crate above base-db gives one crate and base-db only keeps: a value of a type of
/// that crate's, behind one field of the [`Crate`] it is of. Two are equal when they are the
/// same value (the same allocation) or both nothing: whoever gives one compares what it
/// holds first (a gift of an equal value changes nothing).
#[derive(Clone, Default)]
pub struct Given(Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>); // dyn: a type of a crate above this one, read back by that crate alone

impl Given {
    /// `value`, given.
    pub fn new<T: std::any::Any + Send + Sync>(value: T) -> Given {
        Given(Some(std::sync::Arc::new(value))) // alloc: once per value given to a crate
    }

    /// The value given, if one was and it is a `T`.
    pub fn get<T: std::any::Any>(&self) -> Option<&T> {
        self.0.as_deref()?.downcast_ref()
    }
}

impl PartialEq for Given {
    fn eq(&self, other: &Given) -> bool {
        match (&self.0, &other.0) {
            (None, None) => true,
            (Some(a), Some(b)) => std::sync::Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl fmt::Debug for Given {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_some() { "Given(..)" } else { "Given(nothing)" })
    }
}

impl Crate {
    /// A crate's content ID is of its root file's path, its [`BuiltCrateData`] (each
    /// dependency as its crate's content ID, so a crate's ID covers everything it depends on),
    /// its [`ExtraCrateData`], [`CrateWorkspaceData::content`], [`cfg_options_content`] and
    /// [`Env::content`]. A dependency is its
    /// crate's ID, its name and a byte each of prelude, sysroot and private. Its
    /// [`LintOptions`] follow, each flag's lint and level and then the cap.
    pub const DOMAIN: &'static str = "blaze v2 crate 7";

    /// The crate's content ID ([`Crate::DOMAIN`]), as the world made it.
    pub fn content<D: ?Sized + blaze_memo::Database>(self, db: &D) -> ContentId {
        self.content_field(db).id()
    }

    /// The crate's name as Rust code and rustc know it: its `--crate-name`, the manifest's
    /// target name with `-` written `_`. Paths, symbols and keys write a crate by it, a lookup
    /// of a crate by name compares it, and a message names a crate by it ([`Crate::named`]).
    /// The package's name as its manifest spells it, `-` and all, is
    /// [`CrateDisplayName::canonical_name`], which only keys hash.
    ///
    /// A crate set up with no name (a test fixture's, a detached file's) is an
    /// [`UnnamedCrate`]: what needs its name refuses, naming the crate and what needed it,
    /// never writes a stand-in (`std`, `?`, `{crate}`), which names another crate or none.
    pub fn rust_name<'db, D: ?Sized + blaze_memo::Database>(
        self,
        db: &'db D,
    ) -> Result<&'db Symbol, UnnamedCrate> {
        match &self.extra_data(db).display_name {
            Some(name) => Ok(name.crate_name().symbol()),
            None => Err(UnnamedCrate { content: self.content(db) }),
        }
    }

    /// The crate as a message, a trace or a span names it to a person: its
    /// [`rust_name`](Crate::rust_name), or for a crate with none the [`UnnamedCrate`] it is,
    /// by its content ID. Never the database's id, a number of one process, which would put
    /// that process into the keys and binaries a message is written into.
    pub fn named<'db, D: ?Sized + blaze_memo::Database>(
        self,
        db: &'db D,
    ) -> CrateNamed<'db> {
        CrateNamed(self.rust_name(db))
    }

    /// The domain of [`Crate::ported_content`].
    pub const PORTED_DOMAIN: &'static str = "blaze v2 crate ported 1";

    /// [`Crate::content`] with every path it holds written as the database's portable keys
    /// write paths ([`crate::Files::ported`]: its root file's path, the directory its proc
    /// macros run in, each variable's value), and each crate it depends on as `dep` gives it
    /// (its own ported content, the caller's memo). Everything else as the content ID has it,
    /// from the one function both are made by ([`content_with`]), so a field one covers
    /// the other does. What a key that must be the same for the same sources in another
    /// directory holds of a crate's settings (docs/v2/plan/design/store.md, "Content keys");
    /// what reads a path's own bytes (`env!("CARGO_MANIFEST_DIR")`) is the reader's to key.
    pub fn ported_content(self, db: &Db, dep: &dyn Fn(Crate) -> ContentId) -> ContentId {
        let data = self.data(db);
        let files = db.files();
        content_with(
            Crate::PORTED_DOMAIN,
            root_path(db, data.root_file_id),
            data,
            self.extra_data(db),
            self.workspace_data(db),
            self.cfg_options(db),
            self.env(db),
            self.lint_options(db),
            &|bytes| files.ported(bytes).into_owned(), // alloc: a path, once per crate's ported content
            dep,
        )
    }
}

/// [`Crate::content`] of a crate the world holds by key: `root` is the path of
/// `data.root_file_id`, `dep` gives each dependency's content ID by its key. The same bytes
/// as a snapshot's crate of the same fields hashes to ([`content_with`] for both).
#[allow(clippy::too_many_arguments, reason = "a crate's fields, as the world's entry holds them")]
pub(crate) fn keyed_content(
    root: &VfsPath,
    data: &CrateData<CrateKey>,
    extra: &ExtraCrateData,
    workspace: &CrateWorkspaceData,
    cfg: &CfgOptions,
    env: &Env,
    lints: &LintOptions,
    dep: &dyn Fn(CrateKey) -> ContentId,
) -> ContentId {
    // A path, once per crate content ID hashed (a field or a dependency's content moved).
    let as_it_is = |bytes: &[u8]| -> Vec<u8, Global> { bytes.to_vec_in(Global) };
    content_with(Crate::DOMAIN, root, data, extra, workspace, cfg, env, lints, &as_it_is, dep)
}

/// What [`keyed_content`] and [`Crate::ported_content`] hash, of a crate whose dependencies are
/// `Dep`s (a key of the world's, or a snapshot's [`Crate`]): `text` writes what may hold a
/// path, `dep_content` gives each crate depended on. Nothing of `Dep` itself is hashed.
#[allow(clippy::too_many_arguments, reason = "a crate's fields and how to write two of them")]
fn content_with<Dep: Copy>(
    domain: &'static str,
    root: &VfsPath,
    data: &CrateData<Dep>,
    extra: &ExtraCrateData,
    workspace: &CrateWorkspaceData,
    cfg: &CfgOptions,
    env: &Env,
    lints: &LintOptions,
    text: &dyn Fn(&[u8]) -> Vec<u8>,
    dep_content: &dyn Fn(Dep) -> ContentId,
) -> ContentId {
    let mut hasher = Hasher::new(domain);
    hasher.part(&text(&content::path_key(root)));
    hasher.update(&(data.edition.number() as u64).to_le_bytes());
    content::len(&mut hasher, data.dependencies.len());
    for dep in &data.dependencies {
        hasher.id(&dep_content(dep.crate_id)).part(dep.name.as_str().as_bytes()).update(&[
            dep.prelude as u8,
            dep.sysroot as u8,
            dep.private as u8,
        ]);
    }
    data.origin.hash_into(&mut hasher);
    content::len(&mut hasher, data.crate_attrs.len());
    for attr in &data.crate_attrs {
        hasher.part(attr.as_bytes());
    }
    hasher
        .update(&[data.target_kind.tag(), data.manifest_kind.tag(), data.crate_types.bits()])
        .part(&text(data.proc_macro_cwd.as_str().as_bytes()))
        .opt_part(extra.version.as_deref());
    match &extra.display_name {
        None => hasher.update(&[0]),
        Some(name) => hasher
            .update(&[1])
            .part(name.crate_name().as_str().as_bytes())
            .part(name.canonical_name().as_str().as_bytes()),
    };
    match &extra.potential_cfg_options {
        None => hasher.update(&[0]),
        Some(cfg) => hasher.update(&[1]).id(&cfg_options_content(cfg)),
    };
    extra.built_for.hash_into(&mut hasher);
    content::len(&mut hasher, extra.build_script_runs.len());
    for run in &extra.build_script_runs {
        hasher.part(&text(run.as_bytes()));
    }
    hasher.id(&workspace.content()).id(&cfg_options_content(cfg)).id(&env.content_with(text));
    content::len(&mut hasher, lints.opts.len());
    for (lint, level) in &lints.opts {
        hasher.part(lint.as_bytes()).update(&[*level as u8]);
    }
    match lints.cap {
        None => hasher.update(&[0]),
        Some(cap) => hasher.update(&[1, cap as u8]),
    };
    hasher.finish()
}

/// The domain of a crate's item key ([`crate_item_key`]): blaze-items' `CRATE_DOMAIN`, where
/// the key's preimage is described. Persisted item keys are of it: changing it moves them all.
pub const CRATE_ITEM_KEY_DOMAIN: &str = "blaze v2 crate key 5";

/// A crate's item key, blaze-items' `crate_key`: its content, never its id or anything that
/// holds a path, so one crate has one key across checkouts. The one home of its hash: a
/// snapshot's crate (`Dep` = [`Crate`]) and the world's (`Dep` = [`CrateKey`], for the session
/// list [`CrateItemKeys`]) both come here, with `dep_key` giving each dependency's own.
///
/// What it is of, and why (blaze-items' module doc has the rest): the display name and
/// version; the origin; what it is built for (two builds of one package and cfg for the host,
/// a target or the wasm proc macros are two crates) and its build script runs (runs apart in
/// their profile are told apart by a cfg atom); which target of its package it is (the
/// manifest's target kind and what it is built as); its edition, `-Z crate-attr`s and each
/// dependency by name, flags and key; and its cfg atoms, sorted as byte strings.
#[expect(
    blaze::narrowed_len,
    reason = "a cfg atom's end is a u32 offset into the crate's cfg bytes, and nothing checks a crate's cfg is under 4 GiB: a larger one needs a typed refusal where the crate is loaded"
)]
pub fn crate_item_key<Dep: Copy>(
    data: &CrateData<Dep>,
    extra: &ExtraCrateData,
    cfg: &CfgOptions,
    dep_key: &dyn Fn(Dep) -> ContentId,
) -> ContentId {
    use blaze_format::cursor::Put as _;
    use blaze_mem::scratch;
    let mut h = Hasher::new(CRATE_ITEM_KEY_DOMAIN);
    h.opt_part(extra.display_name.as_ref().map(|name| name.canonical_name().as_str()))
        .opt_part(extra.version.as_deref());
    data.origin.hash_into(&mut h);
    // What it is built for: the host, a target or the wasm proc macros. Two builds of one
    // package and cfg for two of these (std for the host and for `--target` of the host's own
    // triple, or for `wasm32-wasip1` and its proc macros) differ in what the key leaves out,
    // their `OUT_DIR` and build script runs, so they are two crates. Within one of these, the
    // runs of one build script apart only in their `OUT_DIR` are one run in the front end, so
    // their crates are one; runs apart in their profile's `OPT_LEVEL` or `DEBUG` too (a
    // release build's host package as a normal and as a `build-override` dependency) may give
    // other output, so their crates are two, told apart by the cfg atom
    // `blaze::build_script_profile` below (`blaze_lower`'s `run_classes`).
    extra.built_for.hash_into(&mut h);
    h.update(&(extra.build_script_runs.len() as u64).to_le_bytes());
    for run in &extra.build_script_runs {
        h.part(run.as_bytes());
    }
    // Which of its package's targets it is, as cargo's units tell them apart: the kind of
    // the manifest's target with its name above, and what it is built as (`--test` makes any
    // target a test). An example and an integration test of one name are two crates, both
    // `Test` under `--all-targets`.
    h.update(&[data.target_kind.tag(), data.manifest_kind.tag()]);
    // What a path in it resolves to: its edition, its `-Z crate-attr`s and each dependency by
    // its name, its flags and its own key. One package of one cfg built twice in one graph (a
    // build dependency reached from the host side and the target side) has two `Crate`s only
    // when these differ, and its items then name different things.
    h.update(&[data.edition as u8]);
    h.update(&(data.crate_attrs.len() as u64).to_le_bytes());
    for attr in data.crate_attrs.iter() {
        h.part(attr.as_bytes());
    }
    h.update(&(data.dependencies.len() as u64).to_le_bytes());
    for dep in &data.dependencies {
        h.part(dep.name.symbol().as_str().as_bytes())
            .update(&[u8::from(dep.is_prelude()), u8::from(dep.is_sysroot()), u8::from(dep.is_private())])
            .id(&dep_key(dep.crate_id));
    }
    // Every atom's bytes in one buffer, sorted as byte strings; an atom is its start and end
    // in the buffer, packed into a `u64`.
    let range = |atom: u64| (atom >> 32) as usize..(atom as u32) as usize;
    scratch::with_bytes(|bytes| {
        scratch::with_u64s(|atoms| {
            for atom in cfg {
                let start = bytes.len();
                match atom {
                    cfg::CfgAtom::Flag(name) => {
                        bytes.push(0);
                        bytes.put_bytes_u64(name.as_str().as_bytes());
                    }
                    cfg::CfgAtom::KeyValue { key, value } => {
                        bytes.push(1);
                        bytes.put_bytes_u64(key.as_str().as_bytes());
                        bytes.put_bytes_u64(value.as_str().as_bytes());
                    }
                }
                let end = u32::try_from(bytes.len()).expect("base-db: a crate's cfg under 4 GiB");
                atoms.push(u64::from(start as u32) << 32 | u64::from(end));
            }
            atoms.sort_unstable_by(|&a, &b| bytes[range(a)].cmp(&bytes[range(b)]));
            h.update(&(atoms.len() as u64).to_le_bytes());
            for &atom in atoms.iter() {
                h.part(&bytes[range(atom)]);
            }
        })
    });
    h.finish()
}

/// The domain of each hash [`Crate::settings_parts`] gives: of a message, never of a key.
const SETTING_PART: &str = "blaze v2 crate setting part 1";

impl Crate {
    /// What [`Crate::content`] is made of, a setting at a time, each by what it is and a hash
    /// of its own: a message that the crate's settings moved (`blaze why`, a unit's record
    /// that missed) names the one that did, where the content ID says only that one of them
    /// has. Each environment variable is a part of its own, by name. Read on a miss; never
    /// part of a key, which [`Crate::content`] alone is.
    pub fn settings_parts(self, db: &Db) -> Vec<(String, ContentId)> {
        let data = self.data(db);
        let (extra, workspace) = (self.extra_data(db), self.workspace_data(db));
        let of = |fill: &dyn Fn(&mut Hasher)| {
            let mut hasher = Hasher::new(SETTING_PART);
            fill(&mut hasher);
            hasher.finish()
        };
        // alloc: once a miss, a part a setting
        let mut parts = vec![
            (
                "its crate's root file's path".to_owned(),
                of(&|h| {
                    h.part(&content::path_key(root_path(db, data.root_file_id)));
                }),
            ),
            (
                "its crate's edition".to_owned(),
                of(&|h| {
                    h.update(&(data.edition.number() as u64).to_le_bytes());
                }),
            ),
            (
                "its crate's origin (which package it is of)".to_owned(),
                of(&|h| {
                    data.origin.hash_into(h);
                }),
            ),
            (
                "its crate's kind of target and crate types".to_owned(),
                of(&|h| {
                    h.update(&[
                        data.target_kind.tag(),
                        data.manifest_kind.tag(),
                        data.crate_types.bits(),
                    ]);
                }),
            ),
            (
                "the directory its crate's proc macros run in".to_owned(),
                of(&|h| {
                    h.part(data.proc_macro_cwd.as_str().as_bytes());
                }),
            ),
            (
                "its crate's name and version".to_owned(),
                of(&|h| {
                    h.opt_part(extra.version.as_deref());
                    if let Some(name) = &extra.display_name {
                        h.part(name.crate_name().as_str().as_bytes())
                            .part(name.canonical_name().as_str().as_bytes());
                    }
                }),
            ),
            (
                "what its crate is built for (the host, a target, the macro host)".to_owned(),
                of(&|h| {
                    extra.built_for.hash_into(h);
                }),
            ),
            (
                "its crate's build script runs".to_owned(),
                of(&|h| {
                    for run in &extra.build_script_runs {
                        h.part(run.as_bytes());
                    }
                }),
            ),
            (
                "what its crate shares with every crate of its target (the target's layout, the toolchain, the const-eval budget, the trusted crates)".to_owned(),
                workspace.content(),
            ),
            ("its crate's cfg".to_owned(), cfg_options_content(self.cfg_options(db))),
            (
                "its crate's lint levels".to_owned(),
                of(&|h| {
                    let lints = self.lint_options(db);
                    for (lint, level) in &lints.opts {
                        h.part(lint.as_bytes()).update(&[*level as u8]);
                    }
                    if let Some(cap) = lints.cap {
                        h.update(&[1, cap as u8]);
                    }
                }),
            ),
        ];
        for (name, value) in self.env(db).iter() {
            parts.push((
                format!("its crate's environment variable `{name}`"),
                of(&|h| {
                    h.part(value.as_bytes());
                }),
            ));
        }
        parts
    }
}

/// The path of a crate's root file: its identity in the crate's content ID.
fn root_path(db: &Db, file: FileId) -> &VfsPath {
    let root_id = db.file_source_root(file).source_root_id(db);
    let root = db.source_root(root_id).source_root_ref(db);
    match root.path_for_file(&file) {
        Some(path) => path,
        None => panic!(
            "the root file {file:?} of a crate is not in its source root {root_id:?}: \
             set the source roots before the crates"
        ),
    }
}

#[cfg(test)]
impl Crate {
    /// The content ID hashing the crate's fields as the snapshot holds them gives: what the
    /// world's ([`Crate::content`]) must equal.
    pub(crate) fn content_from_fields(self, db: &Db) -> ContentId {
        let data = self.data(db);
        let (extra, workspace) = (self.extra_data(db), self.workspace_data(db));
        let root = root_path(db, data.root_file_id);
        let (cfg, env, lints) = (self.cfg_options(db), self.env(db), self.lint_options(db));
        let as_it_is = |bytes: &[u8]| -> Vec<u8, Global> { bytes.to_vec_in(Global) };
        let dep = |dep: Crate| dep.content(db);
        content_with(Crate::DOMAIN, root, data, extra, workspace, cfg, env, lints, &as_it_is, &dep)
    }
}

impl Crate {
    /// The crate and every crate it depends on, directly or not, in the order a walk from it
    /// first reaches them.
    ///
    /// **Warning**: do not use this query in `hir-*` crates! It kills incrementality across crate metadata modifications.
    pub fn transitive_deps(self, db: &Db) -> &[Crate] {
        transitive_deps(db, self)
    }

    /// The crate and every crate that depends on it, directly or not, in the order a walk from
    /// it first reaches them, written into `out` (cleared first): the caller's buffer, so a
    /// warm one makes a call allocate nothing. Not memoized, and cannot be: a query keyed by
    /// the crate lives in its arena, which may read only its dependencies and the session
    /// (`blaze_memo`'s declared borrows), never a dependent. A walk of the snapshot's list
    /// each call (O(crates + edges)), its scratch in a frame, so a reader of it reads every
    /// crate's data.
    ///
    /// **Warning**: do not use this in `hir-*` crates! It kills incrementality across crate
    /// metadata modifications.
    pub fn transitive_rev_deps<A: Allocator>(self, db: &Db, out: &mut Vec<Crate, A>) {
        transitive_rev_deps(db, self, out);
    }

    /// Whether a crate that depends on this one loads its macros alone: a proc-macro crate.
    /// rustc loads such a crate for the host and resolves none of its dependencies into the
    /// dependent's crate graph (rustc_metadata's `resolve_crate_deps` returns an empty map
    /// for a proc-macro crate), and its metadata holds nothing but the macros; so its impls,
    /// its lang items and every crate it depends on (its own `std`, built for the host or for
    /// `wasm32-wasip1`) are never the dependent's. Every walk from a crate to the crates whose
    /// items it sees stops at one ([`Self::transitive_loaded_deps`]).
    pub fn loads_only_macros(self, db: &Db) -> bool {
        self.data(db).target_kind.is_proc_macro()
    }

    /// The crate and every crate whose items it sees, directly or not, in the order a walk
    /// from it first reaches them: [`Self::transitive_deps`], but never into a crate whose
    /// dependents load only its macros ([`Self::loads_only_macros`]) nor through one.
    pub fn transitive_loaded_deps(self, db: &Db) -> &[Crate] {
        transitive_loaded_deps(db, self)
    }

    /// The crate's item key ([`crate_item_key`], each dependency by its own): what names this
    /// crate in bytes another database reads, the same in every database of the same crate.
    /// blaze-items' `crate_key` is this.
    pub fn item_key(self, db: &Db) -> ContentId {
        item_key(db, self)
    }
}

#[blaze_memo::tracked(returns(copy))]
fn item_key(db: &Db, krate: Crate) -> ContentId {
    crate_item_key(krate.data(db), krate.extra_data(db), krate.cfg_options(db), &|dep: Crate| item_key(db, dep))
}

#[blaze_memo::tracked(returns(copy))]
fn transitive_deps<'db>(db: &'db Db, krate: Crate) -> &'db [Crate] {
    blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
        let mut worklist = Vec::new_in(frame);
        worklist.push(krate);
        let mut seen: hashbrown::HashSet<Crate, FixedState, &Frame> =
            hashbrown::HashSet::with_hasher_in(FixedState, frame);
        let mut deps = Vec::new_in(frame);
        while let Some(krate) = worklist.pop() {
            if !seen.insert(krate) {
                continue;
            }
            deps.push(krate);
            worklist.extend(krate.data(db).dependencies.iter().map(|dep| dep.crate_id));
        }
        blaze_memo::keep_slice(db.memo(), &deps)
    })
}

#[blaze_memo::tracked(returns(copy))]
fn transitive_loaded_deps<'db>(db: &'db Db, krate: Crate) -> &'db [Crate] {
    blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
        let mut worklist = Vec::new_in(frame);
        worklist.push(krate);
        let mut seen: hashbrown::HashSet<Crate, FixedState, &Frame> =
            hashbrown::HashSet::with_hasher_in(FixedState, frame);
        let mut deps = Vec::new_in(frame);
        while let Some(krate) = worklist.pop() {
            if !seen.insert(krate) {
                continue;
            }
            deps.push(krate);
            worklist.extend(
                krate
                    .data(db)
                    .dependencies
                    .iter()
                    .map(|dep| dep.crate_id)
                    .filter(|dep| !dep.loads_only_macros(db)),
            );
        }
        blaze_memo::keep_slice(db.memo(), &deps)
    })
}

/// [`Crate::transitive_rev_deps`]: a plain walk, outside any query run, its scratch in a frame
/// and its answer written into the caller's `out`.
fn transitive_rev_deps<A: Allocator>(db: &Db, krate: Crate, out: &mut Vec<Crate, A>) {
    blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
        let all = all_crates(db);
        // The reverse edges as CSR: for each crate (by its place in `all`), the crates that
        // depend on it.
        let mut place: hashbrown::HashMap<Crate, u32, FixedState, &Frame> =
            hashbrown::HashMap::with_capacity_and_hasher_in(all.len(), FixedState, frame);
        place.extend(all.iter().enumerate().map(|(at, &krate)| (krate, at as u32)));
        let starts = frame.alloc_slice_fill_with(all.len() + 1, |_| 0u32);
        for &from in all.iter() {
            for dep in &from.data(db).dependencies {
                starts[place[&dep.crate_id] as usize + 1] += 1;
            }
        }
        for at in 1..starts.len() {
            starts[at] += starts[at - 1];
        }
        let next = frame.alloc_slice_copy(&starts[..all.len()]);
        let edges = frame.alloc_slice_fill_with(starts[all.len()] as usize, |_| krate);
        for &from in all.iter() {
            for dep in &from.data(db).dependencies {
                let to = place[&dep.crate_id] as usize;
                edges[next[to] as usize] = from;
                next[to] += 1;
            }
        }
        let seen = frame.alloc_slice_fill_with(all.len(), |_| false);
        let mut order = Vec::new_in(frame);
        let mut worklist = Vec::new_in(frame);
        worklist.push(krate);
        seen[place[&krate] as usize] = true;
        while let Some(krate) = worklist.pop() {
            order.push(krate);
            let at = place[&krate] as usize;
            for &rev_dep in &edges[starts[at] as usize..starts[at + 1] as usize] {
                let slot = &mut seen[place[&rev_dep] as usize];
                if !*slot {
                    *slot = true;
                    worklist.push(rev_dep);
                }
            }
        }
        out.clear();
        out.extend_from_slice(&order);
    })
}

impl CrateGraphBuilder {
    pub fn add_crate_root(
        &mut self,
        root_file_id: FileId,
        edition: Edition,
        display_name: Option<CrateDisplayName>,
        version: Option<String>,
        mut cfg_options: CfgOptions,
        mut potential_cfg_options: Option<CfgOptions>,
        mut env: Env,
        origin: CrateOrigin,
        crate_attrs: Vec<String>,
        target_kind: TargetKind,
        proc_macro_cwd: Arc<AbsPathBuf>,
        ws_data: Arc<CrateWorkspaceData>,
    ) -> CrateBuilderId {
        env.entries.shrink_to_fit();
        cfg_options.shrink_to_fit();
        if let Some(potential_cfg_options) = &mut potential_cfg_options {
            potential_cfg_options.shrink_to_fit();
        }
        let crate_attrs: Vec<_> = crate_attrs
            .into_iter()
            // alloc: a crate's attributes, once per crate added to a graph (setup).
            .map(|raw_attr| format!("#![{raw_attr}]").into_boxed_str())
            .collect(); // alloc: as above.
        self.arena.alloc(CrateBuilder {
            basic: CrateData {
                root_file_id,
                edition,
                // alloc: none until a dependency is added (setup).
                dependencies: Vec::new(),
                origin,
                crate_attrs: crate_attrs.into_boxed_slice(),
                target_kind,
                manifest_kind: target_kind,
                crate_types: target_kind.default_crate_types(),
                proc_macro_cwd,
                // A builder's crate is sealed only when its maker says what pins it
                // ([`CrateGraphBuilder::seal`]).
                sealed: None,
            },
            extra: ExtraCrateData {
                version,
                display_name,
                potential_cfg_options,
                built_for: BuiltFor::default(),
                build_script_runs: Box::new([]),
            },
            cfg_options,
            env,
            ws_data,
        })
    }

    /// Seals `krate`: `pin` is the digest of the tree its sources are read from
    /// ([`CrateData::sealed`]). What a maker of a graph that knows its trees calls (a test's
    /// fixture, for a crate it stands in for a registry package with).
    pub fn seal(&mut self, krate: CrateBuilderId, pin: ContentId) {
        self.arena[krate].basic.sealed = Some(pin);
    }

    pub fn add_dep(
        &mut self,
        from: CrateBuilderId,
        dep: DependencyBuilder,
    ) -> Result<(), CyclicDependenciesError> {
        let _p = tracing::info_span!(target: "blaze", "add_dep").entered();

        // Check if adding a dep from `from` to `to` creates a cycle. To figure
        // that out, look for a  path in the *opposite* direction, from `to` to
        // `from`.
        if let Some(path) = self.find_path(&mut HashSet::new_in(Global), dep.crate_id, from) {
            let path =
                // clone: the cycle's error (an error path).
                path.into_iter().map(|it| (it, self[it].extra.display_name.clone())).collect();
            let err = CyclicDependenciesError { path };
            assert!(err.from().0 == from && err.to().0 == dep.crate_id);
            return Err(err);
        }

        self.arena[from].basic.dependencies.push(dep);
        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = CrateBuilderId> + '_ {
        self.arena.iter().map(|(idx, _)| idx)
    }

    /// Returns an iterator over all transitive dependencies of the given crate,
    /// including the crate itself.
    pub fn transitive_deps(&self, of: CrateBuilderId) -> impl Iterator<Item = CrateBuilderId> {
        // alloc: a walk of the builder's graph (setup).
        let mut worklist = vec![of];
        let mut deps = HashSet::new_in(Global);

        while let Some(krate) = worklist.pop() {
            if !deps.insert(krate) {
                continue;
            }

            worklist.extend(self[krate].basic.dependencies.iter().map(|dep| dep.crate_id));
        }

        deps.into_iter()
    }

    /// Returns all crates in the graph, sorted in topological order (i.e. dependencies of a crate
    /// come before the crate itself).
    fn crates_in_topological_order(&self) -> Vec<CrateBuilderId> {
        // alloc: a walk of the builder's graph (setup).
        let mut res = Vec::new();
        let mut visited = HashSet::new_in(Global);

        for krate in self.iter() {
            go(self, &mut visited, &mut res, krate);
        }

        return res;

        fn go(
            graph: &CrateGraphBuilder,
            visited: &mut HashSet<CrateBuilderId, Global>,
            res: &mut Vec<CrateBuilderId>,
            source: CrateBuilderId,
        ) {
            if !visited.insert(source) {
                return;
            }
            for dep in graph[source].basic.dependencies.iter() {
                go(graph, visited, res, dep.crate_id)
            }
            res.push(source)
        }
    }

    /// Extends this crate graph by adding a complete second crate
    /// graph and adjust the ids in the [`ProcMacroPaths`] accordingly.
    ///
    /// This will deduplicate the crates of the graph where possible.
    /// Furthermore dependencies are sorted by crate id to make deduplication easier.
    ///
    /// Returns a map mapping `other`'s IDs to the new IDs in `self`.
    pub fn extend(
        &mut self,
        mut other: CrateGraphBuilder,
        proc_macros: &mut ProcMacroPaths,
    ) -> HashMap<CrateBuilderId, CrateBuilderId, Global> {
        // Sorting here is a bit pointless because the input is likely already sorted.
        // However, the overhead is small and it makes the `extend` method harder to misuse.
        self.arena.iter_mut().for_each(|(_, data)| {
            radix::sort_by_key(&mut data.basic.dependencies, |dep| {
                dep.crate_id.into_raw().into_u32()
            })
        });

        let m = self.arena.len();
        let topo = other.crates_in_topological_order();
        let mut id_map: HashMap<CrateBuilderId, CrateBuilderId, Global> = HashMap::new_in(Global);
        for topo in topo {
            let crate_data = &mut other.arena[topo];

            crate_data
                .basic
                .dependencies
                .iter_mut()
                .for_each(|dep| dep.crate_id = id_map[&dep.crate_id]);
            radix::sort_by_key(&mut crate_data.basic.dependencies, |dep| {
                dep.crate_id.into_raw().into_u32()
            });

            let find = self.arena.iter().take(m).find_map(|(k, v)| (v == crate_data).then_some(k));
            // clone: a crate of the graph being merged in (setup).
            let new_id = find.unwrap_or_else(|| self.arena.alloc(crate_data.clone()));
            id_map.insert(topo, new_id);
        }

        *proc_macros =
            // alloc: as above.
            mem::take(proc_macros).into_iter().map(|(id, macros)| (id_map[&id], macros)).collect();
        id_map
    }

    fn find_path(
        &self,
        visited: &mut HashSet<CrateBuilderId, Global>,
        from: CrateBuilderId,
        to: CrateBuilderId,
    ) -> Option<Vec<CrateBuilderId>> {
        if !visited.insert(from) {
            return None;
        }

        if from == to {
            // alloc: the cycle's path (an error path).
            return Some(vec![to]);
        }

        for dep in &self[from].basic.dependencies {
            let crate_id = dep.crate_id;
            if let Some(mut path) = self.find_path(visited, crate_id, to) {
                path.push(from);
                return Some(path);
            }
        }

        None
    }

    /// Sets a variable `env!` reads in `krate`, as a build script of it would.
    pub fn set_env(&mut self, krate: CrateBuilderId, key: &str, value: &str) {
        self.arena[krate].env.set(key, value);
    }

    /// Sets a flag `cfg` reads in `krate`, as a build script of it would.
    pub fn set_cfg(&mut self, krate: CrateBuilderId, flag: Symbol) {
        self.arena[krate].cfg_options.insert_atom(flag);
    }

    pub fn insert_cfg(&mut self, krate: CrateBuilderId, cfg: &cfg::CfgAtom) {
        match *cfg {
            cfg::CfgAtom::Flag(flag) => self.arena[krate].cfg_options.insert_atom(flag),
            cfg::CfgAtom::KeyValue { key, value } => {
                self.arena[krate].cfg_options.insert_key_value(key, value)
            }
        }
    }

    /// Removes all crates from this crate graph except for the ones in `to_keep` and fixes up the dependencies.
    /// Returns a mapping from old crate ids to new crate ids.
    pub fn remove_crates_except(
        &mut self,
        to_keep: &[CrateBuilderId],
    ) -> Vec<Option<CrateBuilderId>> {
        // alloc: once per `remove_crates_except` (setup).
        let mut id_map = vec![None; self.arena.len()];
        self.arena = std::mem::take(&mut self.arena)
            .into_iter()
            .filter_map(|(id, data)| if to_keep.contains(&id) { Some((id, data)) } else { None })
            .enumerate()
            .map(|(new_id, (id, data))| {
                id_map[id.into_raw().into_u32() as usize] =
                    Some(CrateBuilderId::from_raw(RawIdx::from_u32(new_id as u32)));
                data
            })
            .collect(); // alloc: as above.
        for (_, data) in self.arena.iter_mut() {
            data.basic.dependencies.iter_mut().for_each(|dep| {
                dep.crate_id =
                    id_map[dep.crate_id.into_raw().into_u32() as usize].expect("crate was filtered")
            });
        }
        id_map
    }

    pub fn shrink_to_fit(&mut self) {
        self.arena.shrink_to_fit();
    }
}

impl Crate {
    /// The crate's root file, read in the crate's edition and under its syntax extensions
    /// ([`Crate::extensions`]).
    pub fn root_file_id(self, db: &Db) -> EditionedFileId {
        let data = self.data(db);
        let extensions = self.extensions(db);
        EditionedFileId::new_with_extensions(db, data.root_file_id, data.edition, extensions)
    }
}

impl Extend<(String, String)> for Env {
    fn extend<T: IntoIterator<Item = (String, String)>>(&mut self, iter: T) {
        self.entries.extend(iter);
    }
}

impl FromIterator<(String, String)> for Env {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        Env { entries: FromIterator::from_iter(iter) }
    }
}

impl Env {
    pub const DOMAIN: &'static str = "blaze v2 env 1";

    /// Of its variables sorted by name, each as its name and value.
    pub fn content(&self) -> ContentId {
        self.content_with(&|value| value.to_vec()) // alloc: a value, once per crate content ID (setup)
    }

    /// [`Env::content`] with each value as `text` writes it (a portable key's paths,
    /// [`Crate::ported_content`]).
    pub fn content_with(&self, text: &dyn Fn(&[u8]) -> Vec<u8>) -> ContentId {
        // alloc: an env's variables sorted, once per crate content ID (setup).
        let mut entries: Vec<_> = self.entries.iter().collect();
        entries.sort_unstable();
        let mut hasher = Hasher::new(Env::DOMAIN);
        content::len(&mut hasher, entries.len());
        for (key, value) in entries {
            hasher.part(key.as_bytes()).part(&text(value.as_bytes()));
        }
        hasher.finish()
    }

    pub fn set(&mut self, env: &str, value: impl Into<String>) {
        // alloc: setting a variable (setup).
        self.entries.insert(env.to_owned(), value.into());
    }

    pub fn get(&self, env: &str) -> Option<String> {
        self.entries.get(env).cloned()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(key, value)| (&**key, &**value))
    }

    pub fn extend_from_other(&mut self, other: &Env) {
        // alloc: as above.
        self.entries.extend(other.entries.iter().map(|(x, y)| (x.to_owned(), y.to_owned())));
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn insert(&mut self, k: impl Into<String>, v: impl Into<String>) -> Option<String> {
        self.entries.insert(k.into(), v.into())
    }

    pub fn contains_key(&self, arg: &str) -> bool {
        self.entries.contains_key(arg)
    }
}

impl From<Env> for Vec<(String, String)> {
    fn from(env: Env) -> Vec<(String, String)> {
        // alloc: the edge: the env handed out as a list.
        let mut entries: Vec<_> = env.entries.into_iter().collect();
        entries.sort();
        entries
    }
}

impl<'a> IntoIterator for &'a Env {
    type Item = (&'a String, &'a String);
    type IntoIter = blaze_hash::hash_map::Iter<'a, String, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}

/// The crate graph had a cycle. This is typically a bug, and
/// rust-analyzer logs a warning when it encounters a cycle. Generally
/// rust-analyzer will continue working OK in the presence of cycle,
/// but it's better to have an accurate crate graph.
///
/// ## dev-dependencies
///
/// Note that it's actually legal for a Cargo package (i.e. a thing
/// with a `Cargo.toml`) to depend on itself in dev-dependencies. This
/// can enable additional features, and is typically used when a
/// project wants features to be enabled in tests. Dev-dependencies
/// are not propagated, so they aren't visible to package that depend
/// on this one.
///
/// <https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#development-dependencies>
///
/// However, rust-analyzer constructs its crate graph from Cargo
/// metadata, so it can end up producing a cyclic crate graph from a
/// well-formed package graph.
///
/// <https://github.com/rust-lang/rust-analyzer/issues/14167>
#[derive(Debug)]
pub struct CyclicDependenciesError {
    path: Vec<(CrateBuilderId, Option<CrateDisplayName>)>,
}

impl CyclicDependenciesError {
    fn from(&self) -> &(CrateBuilderId, Option<CrateDisplayName>) {
        self.path.first().unwrap()
    }
    fn to(&self) -> &(CrateBuilderId, Option<CrateDisplayName>) {
        self.path.last().unwrap()
    }
}

impl fmt::Display for CyclicDependenciesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let render = |(id, name): &(CrateBuilderId, Option<CrateDisplayName>)| match name {
            // alloc: the cycle's message (an error path).
            Some(it) => format!("{it}({id:?})"),
            // alloc: as above.
            None => format!("{id:?}"),
        };
        // alloc: as above.
        let path = self.path.iter().rev().map(render).collect::<Vec<String>>().join(" -> ");
        write!(
            f,
            "cyclic deps: {} -> {}, alternative path: {}",
            render(self.from()),
            render(self.to()),
            path
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::AbsPathBuf;
    use triomphe::Arc;

    use crate::{CrateWorkspaceData, DependencyBuilder, TargetKind};

    use super::{CrateGraphBuilder, CrateName, CrateOrigin, Edition::Edition2018, Env, FileId};

    fn empty_ws_data() -> Arc<CrateWorkspaceData> {
        Arc::new(CrateWorkspaceData {
            target: Err("".into()),
            toolchain: None,
            const_eval: Default::default(),
            trusted: Box::default(),
        })
    }

    #[test]
    fn detect_cyclic_dependency_indirect() {
        let mut graph = CrateGraphBuilder::default();
        let crate1 = graph.add_crate_root(
            FileId::from_raw(1u32),
            Edition2018,
            None,
            None,
            Default::default(),
            Default::default(),
            Env::default(),
            CrateOrigin::Local { repo: None, name: None },
            Vec::new(),
            TargetKind::Lib { is_proc_macro: false },
            Arc::new(AbsPathBuf::assert_utf8(std::env::current_dir().unwrap())),
            empty_ws_data(),
        );
        let crate2 = graph.add_crate_root(
            FileId::from_raw(2u32),
            Edition2018,
            None,
            None,
            Default::default(),
            Default::default(),
            Env::default(),
            CrateOrigin::Local { repo: None, name: None },
            Vec::new(),
            TargetKind::Lib { is_proc_macro: false },
            Arc::new(AbsPathBuf::assert_utf8(std::env::current_dir().unwrap())),
            empty_ws_data(),
        );
        let crate3 = graph.add_crate_root(
            FileId::from_raw(3u32),
            Edition2018,
            None,
            None,
            Default::default(),
            Default::default(),
            Env::default(),
            CrateOrigin::Local { repo: None, name: None },
            Vec::new(),
            TargetKind::Lib { is_proc_macro: false },
            Arc::new(AbsPathBuf::assert_utf8(std::env::current_dir().unwrap())),
            empty_ws_data(),
        );
        assert!(
            graph
                .add_dep(crate1, DependencyBuilder::new(CrateName::new("crate2").unwrap(), crate2,))
                .is_ok()
        );
        assert!(
            graph
                .add_dep(crate2, DependencyBuilder::new(CrateName::new("crate3").unwrap(), crate3,))
                .is_ok()
        );
        assert!(
            graph
                .add_dep(crate3, DependencyBuilder::new(CrateName::new("crate1").unwrap(), crate1,))
                .is_err()
        );
    }

    #[test]
    fn detect_cyclic_dependency_direct() {
        let mut graph = CrateGraphBuilder::default();
        let crate1 = graph.add_crate_root(
            FileId::from_raw(1u32),
            Edition2018,
            None,
            None,
            Default::default(),
            Default::default(),
            Env::default(),
            CrateOrigin::Local { repo: None, name: None },
            Vec::new(),
            TargetKind::Lib { is_proc_macro: false },
            Arc::new(AbsPathBuf::assert_utf8(std::env::current_dir().unwrap())),
            empty_ws_data(),
        );
        let crate2 = graph.add_crate_root(
            FileId::from_raw(2u32),
            Edition2018,
            None,
            None,
            Default::default(),
            Default::default(),
            Env::default(),
            CrateOrigin::Local { repo: None, name: None },
            Vec::new(),
            TargetKind::Lib { is_proc_macro: false },
            Arc::new(AbsPathBuf::assert_utf8(std::env::current_dir().unwrap())),
            empty_ws_data(),
        );
        assert!(
            graph
                .add_dep(crate1, DependencyBuilder::new(CrateName::new("crate2").unwrap(), crate2,))
                .is_ok()
        );
        assert!(
            graph
                .add_dep(crate2, DependencyBuilder::new(CrateName::new("crate2").unwrap(), crate2,))
                .is_err()
        );
    }

    #[test]
    fn it_works() {
        let mut graph = CrateGraphBuilder::default();
        let crate1 = graph.add_crate_root(
            FileId::from_raw(1u32),
            Edition2018,
            None,
            None,
            Default::default(),
            Default::default(),
            Env::default(),
            CrateOrigin::Local { repo: None, name: None },
            Vec::new(),
            TargetKind::Lib { is_proc_macro: false },
            Arc::new(AbsPathBuf::assert_utf8(std::env::current_dir().unwrap())),
            empty_ws_data(),
        );
        let crate2 = graph.add_crate_root(
            FileId::from_raw(2u32),
            Edition2018,
            None,
            None,
            Default::default(),
            Default::default(),
            Env::default(),
            CrateOrigin::Local { repo: None, name: None },
            Vec::new(),
            TargetKind::Lib { is_proc_macro: false },
            Arc::new(AbsPathBuf::assert_utf8(std::env::current_dir().unwrap())),
            empty_ws_data(),
        );
        let crate3 = graph.add_crate_root(
            FileId::from_raw(3u32),
            Edition2018,
            None,
            None,
            Default::default(),
            Default::default(),
            Env::default(),
            CrateOrigin::Local { repo: None, name: None },
            Vec::new(),
            TargetKind::Lib { is_proc_macro: false },
            Arc::new(AbsPathBuf::assert_utf8(std::env::current_dir().unwrap())),
            empty_ws_data(),
        );
        assert!(
            graph
                .add_dep(crate1, DependencyBuilder::new(CrateName::new("crate2").unwrap(), crate2,))
                .is_ok()
        );
        assert!(
            graph
                .add_dep(crate2, DependencyBuilder::new(CrateName::new("crate3").unwrap(), crate3,))
                .is_ok()
        );
    }

    #[test]
    fn dashes_are_normalized() {
        let mut graph = CrateGraphBuilder::default();
        let crate1 = graph.add_crate_root(
            FileId::from_raw(1u32),
            Edition2018,
            None,
            None,
            Default::default(),
            Default::default(),
            Env::default(),
            CrateOrigin::Local { repo: None, name: None },
            Vec::new(),
            TargetKind::Lib { is_proc_macro: false },
            Arc::new(AbsPathBuf::assert_utf8(std::env::current_dir().unwrap())),
            empty_ws_data(),
        );
        let crate2 = graph.add_crate_root(
            FileId::from_raw(2u32),
            Edition2018,
            None,
            None,
            Default::default(),
            Default::default(),
            Env::default(),
            CrateOrigin::Local { repo: None, name: None },
            Vec::new(),
            TargetKind::Lib { is_proc_macro: false },
            Arc::new(AbsPathBuf::assert_utf8(std::env::current_dir().unwrap())),
            empty_ws_data(),
        );
        assert!(
            graph
                .add_dep(
                    crate1,
                    DependencyBuilder::new(
                        CrateName::normalize_dashes("crate-name-with-dashes"),
                        crate2,
                    )
                )
                .is_ok()
        );
        assert_eq!(
            graph.arena[crate1].basic.dependencies,
            vec![
                DependencyBuilder::new(CrateName::new("crate_name_with_dashes").unwrap(), crate2,)
            ]
        );
    }
}
