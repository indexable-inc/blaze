//! This module specifies the input to rust-analyzer. In some sense, this is
//! **the** most important module, because all other fancy stuff is strictly
//! derived from this input.
//!
//! Nothing here does IO: files come in as blaze-vfs views ([`crate::Db::set_view_root`]) and
//! the crate graph from `blaze-lower`.

use blaze_hash::hash_map::Entry;
use std::error::Error;
use std::str::FromStr;
use std::{fmt, mem, ops};

use blaze_core::lock::{Lock, LockGuard};

use crate::{AbsPathBuf, AnchoredPath, FileId, FileSet, VfsPath};
use blaze_hash::{FixedState, HashMap, HashSet};
use blaze_ids::{ContentId, Hasher, radix};
use blaze_lint_defs::LintOptions;
use blaze_mem::Frame;
use blaze_query::db::{Database as _, Durability, SealGroup, SealKind, Setter};
use cfg::{CfgOptions, HashableCfgOptions};
use intern::Symbol;
use la_arena::{Arena, Idx, RawIdx};
use span::Edition;
use std::alloc::Global;
use triomphe::Arc;

use crate::{
    CrateWorkspaceData, Db, EditionedFileId, FxIndexSet, all_crates,
    content::{self, InputId, cfg_options_content},
    maintain::Note,
    refresh_crate_graph_content, set_all_crates_with_durability,
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

/// The crate data from which we derive the `Crate`.
///
/// We want this to contain as little data as possible, because if it contains dependencies and
/// something changes, this crate and all of its dependencies ids are invalidated, which causes
/// pretty much everything to be recomputed. If the crate id is not invalidated, only this crate's
/// information needs to be recomputed.
///
/// *Most* different crates have different root files (actually, pretty much all of them).
/// Still, it is possible to have crates distinguished by other factors (e.g. dependencies).
/// So we store only the root file - unless we find that this crate has the same root file as
/// another crate, in which case we store all data for one of them (if one is a dependency of
/// the other, we store for it, because it has more dependencies to be invalidated).
#[derive(Debug, Clone, PartialEq, Eq, Hash)] // clone: setup: the crate graph, once per load
pub struct UniqueCrateData {
    root_file_id: FileId,
    disambiguator: Option<Box<(BuiltCrateData, HashableCfgOptions)>>,
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
}

pub type CrateDataBuilder = CrateData<CrateBuilderId>;
pub type BuiltCrateData = CrateData<Crate>;

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
    prelude: bool,
    sysroot: bool,
    private: bool,
}

pub type DependencyBuilder = Dependency<CrateBuilderId>;
pub type BuiltDependency = Dependency<Crate>;

impl DependencyBuilder {
    pub fn new(name: CrateName, crate_id: CrateBuilderId) -> Self {
        Self { name, crate_id, prelude: true, sysroot: false, private: false }
    }

    pub fn with_prelude(
        name: CrateName,
        crate_id: CrateBuilderId,
        prelude: bool,
        sysroot: bool,
    ) -> Self {
        Self { name, crate_id, prelude, sysroot, private: false }
    }
}

impl BuiltDependency {
    /// A dependency of a crate added after the graph was built, in the extern prelude.
    pub fn new(name: CrateName, crate_id: Crate) -> Self {
        Self { name, crate_id, prelude: true, sysroot: false, private: false }
    }

    /// A dependency of a crate added after the graph was built, saying whether it is in the
    /// extern prelude (not for cargo's `noprelude`) and whether it is a sysroot crate.
    pub fn with_prelude(name: CrateName, crate_id: Crate, prelude: bool, sysroot: bool) -> Self {
        Self { name, crate_id, prelude, sysroot, private: false }
    }
}

impl<Id> Dependency<Id> {
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

pub type CratesIdMap = HashMap<CrateBuilderId, Crate, Global>;

/// A crate. Its fields are set only through base-db ([`Crate::create`], [`Crate::set_env`] and
/// the other setters, [`CrateGraphBuilder::set_in_db`]), which keep [`Crate::content`] the
/// content ID of them all and of its dependencies' content IDs.
#[blaze_query::input]
#[derive(Debug, PartialOrd, Ord)]
pub struct Crate {
    #[returns(ref)]
    #[get(data_field)]
    #[set(set_data_field)]
    data: BuiltCrateData,
    /// Crate data that is not needed for analysis.
    ///
    /// This is split into a separate field to increase incrementality.
    #[returns(ref)]
    #[get(extra_data_field)]
    #[set(set_extra_data_field)]
    extra_data: ExtraCrateData,
    // This is in `Arc` because it is shared for all crates in a workspace.
    #[returns(ref)]
    #[get(workspace_data_field)]
    #[set(set_workspace_data_field)]
    workspace_data: Arc<CrateWorkspaceData>,
    #[returns(ref)]
    #[get(cfg_options_field)]
    #[set(set_cfg_options_field)]
    cfg_options: CfgOptions,
    #[returns(ref)]
    #[get(env_field)]
    #[set(set_env_field)]
    env: Env,
    #[returns(copy)]
    #[get(content_field)]
    #[set(set_content_field)]
    content: InputId,
    /// The files of its crate attributes ([`crate::crate_attrs`]), kept with its data: not in
    /// its content ID, which has the attributes' text.
    #[default]
    #[returns(ref)]
    #[get(attr_files_field)]
    #[set(set_attr_files_field)]
    pub(crate) attr_files: Box<[FileId]>,
    /// The lint flags its compile is given: rustc's `-A`/`-W`/`--force-warn`/`-D`/`-F` in order
    /// and `--cap-lints` (rustc_session's `lint_opts` and `lint_cap`). Read only by
    /// `hir_lint_levels`' level queries; in its content ID, which persisted results are keyed by.
    #[default]
    #[returns(ref)]
    #[get(lint_options_field)]
    #[set(set_lint_options_field)]
    lint_options: LintOptions,
    /// What the crates above base-db gave this crate, a slot a kind ([`Crate::give`]): which
    /// slots there are, added to when a kind is first given and never else. Not in its
    /// content ID.
    #[default]
    #[returns(ref)]
    #[get(slots_field)]
    #[set(set_slots_field)]
    slots: Vec<(std::any::TypeId, CrateSlot)>,
}

/// One thing given to one crate ([`Crate::give`]): an input of its own, of the crate's seal
/// group, so a write of it is a write of that crate's and of that kind alone.
#[blaze_query::input(debug)]
pub struct CrateSlot {
    #[returns(ref)]
    pub value: Given,
}

impl Crate {
    /// What was given to this crate as kind `K` ([`Crate::give`]), if it is a `T`; `None`
    /// when nothing was. Reads the crate's list of slots and the one slot: inside a query,
    /// both are recorded, and neither is written by a gift to another crate or of another
    /// kind.
    ///
    /// The invariant: what is "one value a crate" is kept a crate, never as an entry of one
    /// map of every crate. A map of every crate is an input every crate's rows read, so
    /// giving one crate its value was a write all of them verified after (the proc-macro
    /// map: a build of reth with nothing changed, 53 writes and 23 million rows verified,
    /// 2026-11-12), and no seal group could own it (`blaze_query::db::seal`).
    pub fn given<K: 'static, T: std::any::Any>(self, db: &Db) -> Option<&T> {
        let kind = std::any::TypeId::of::<K>();
        let slot = self.slots_field(db).iter().find(|it| it.0 == kind)?.1;
        slot.value(db).get::<T>()
    }

    /// Gives this crate `value` as kind `K` (a type of the giver's, naming what it is), of
    /// durability `durability`: whether anything was written. The kind's first gift makes its
    /// slot, in the crate's seal group, and adds it to the crate's list; later ones write the
    /// slot alone. The giver compares what the crate holds first: two [`Given`] are equal
    /// only when they are one value.
    pub fn give<K: 'static>(self, db: &mut Db, value: Given, durability: Durability) -> bool {
        let kind = std::any::TypeId::of::<K>();
        let found = self.slots_field(&*db).iter().find(|it| it.0 == kind).map(|it| it.1);
        match found {
            Some(slot) => {
                if *slot.value(&*db) == value {
                    return false;
                }
                slot.set_value(db).with_durability(durability).to(value);
                // The one case that is a write of `CrateSlot` to the engine: a kind's first
                // gift below makes the input (no write) and writes the crate's `slots`.
                count_given(std::any::type_name::<K>());
            }
            None => {
                let slot = CrateSlot::new(&*db, durability, value);
                if let Some(group) = self.seal_group_of(&*db) {
                    slot.seal_group(db, group);
                }
                let mut slots = self.slots_field(&*db).clone(); // clone: a crate's few slots, once a kind
                slots.push((kind, slot)); // alloc: as above
                // `HIGH`: the list only gains a slot, the first time a kind is given.
                self.set_slots_field(db).with_durability(Durability::HIGH).to(slots);
            }
        }
        true
    }

    /// This crate's seal group (`blaze_query::db::seal`), made with the crate ([`seal_crate`]):
    /// what an input made for one crate by a crate above base-db is given, so a write of it
    /// opens that crate's group and no other.
    pub fn seal_group_of(self, db: &Db) -> Option<SealGroup> {
        db.crates_map().lock().seal.get(&self).copied()
    }

    /// The seal group of the source root this crate's root file is in (`Files::seal_root`):
    /// the group the crate's own files are inputs of, which the crate's group opens with.
    /// `None` when the root has none yet.
    pub fn seal_root_group_of(self, db: &Db) -> Option<SealGroup> {
        let root = db.file_source_root(self.data(db).root_file_id).source_root_id(db);
        db.files().lock().seal_roots.get(&root).copied()
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

/// One write of a slot of `kind`: a [`Crate::give`] that set a slot the crate already had,
/// which is what the engine counts as a write of `CrateSlot` (making a slot is none). Every [`CrateSlot`] is one
/// input to the query engine, whose count of writes is by input, so who wrote a slot (a
/// plan's proc macros, a resolution blaze published) is told by its kind, counted here.
fn count_given(kind: &'static str) {
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
/// kind as `std::any::type_name` of [`Crate::give`]'s `K`), and counts from nothing again:
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
/// holds first (a set of an equal value opens no view, `InputSetter::to`).
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
    /// [`Env::content`]. [`UniqueCrateData`] adds nothing: its root file is hashed as its
    /// path, and its disambiguator is the crate's own data and cfg. A dependency is its
    /// crate's ID, its name and a byte each of prelude, sysroot and private. Its
    /// [`LintOptions`] follow, each flag's lint and level and then the cap.
    pub const DOMAIN: &'static str = "blaze v2 crate 7";

    pub fn data<'db, Db: ?Sized + blaze_query::db::Database>(
        self,
        db: &'db Db,
    ) -> &'db BuiltCrateData {
        self.data_field(db)
    }

    pub fn extra_data<'db, Db: ?Sized + blaze_query::db::Database>(
        self,
        db: &'db Db,
    ) -> &'db ExtraCrateData {
        self.extra_data_field(db)
    }

    pub fn workspace_data<'db, Db: ?Sized + blaze_query::db::Database>(
        self,
        db: &'db Db,
    ) -> &'db Arc<CrateWorkspaceData> {
        self.workspace_data_field(db)
    }

    pub fn cfg_options<'db, Db: ?Sized + blaze_query::db::Database>(
        self,
        db: &'db Db,
    ) -> &'db CfgOptions {
        self.cfg_options_field(db)
    }

    pub fn env<'db, Db: ?Sized + blaze_query::db::Database>(self, db: &'db Db) -> &'db Env {
        self.env_field(db)
    }

    pub fn content<Db: ?Sized + blaze_query::db::Database>(self, db: &Db) -> ContentId {
        self.content_field(db).id()
    }

    pub fn lint_options<'db, Db: ?Sized + blaze_query::db::Database>(
        self,
        db: &'db Db,
    ) -> &'db LintOptions {
        self.lint_options_field(db)
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
    pub fn rust_name<'db, Db: ?Sized + blaze_query::db::Database>(
        self,
        db: &'db Db,
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
    pub fn named<'db, Db: ?Sized + blaze_query::db::Database>(
        self,
        db: &'db Db,
    ) -> CrateNamed<'db> {
        CrateNamed(self.rust_name(db))
    }

    /// A crate outside a [`CrateGraphBuilder`], all its fields with `durability`. Its root
    /// file must be in a source root already, and its dependencies made.
    pub fn create(
        db: &mut Db,
        data: BuiltCrateData,
        extra: ExtraCrateData,
        workspace: Arc<CrateWorkspaceData>,
        cfg: CfgOptions,
        env: Env,
        durability: Durability,
    ) -> Crate {
        Crate::create_noting(db, "Crate::create", data, extra, workspace, cfg, env, durability, false)
    }

    /// [`Crate::create`] for a crate that is put into the graph beside the crates there
    /// ([`crate::add_crates`]), noted as that crate added ([`crate::Written::crates_added`]) and
    /// not as a change of the whole graph: no crate there depends on it or was made from it, so
    /// what the maintainers hold of every other crate stands. What a plan lowered into a front
    /// end that holds other plans' crates makes its own with.
    pub fn create_added(
        db: &mut Db,
        data: BuiltCrateData,
        extra: ExtraCrateData,
        workspace: Arc<CrateWorkspaceData>,
        cfg: CfgOptions,
        env: Env,
        durability: Durability,
    ) -> Crate {
        Crate::create_noting(db, "Crate::create_added", data, extra, workspace, cfg, env, durability, true)
    }

    /// [`Crate::create`] as the write method `write`, noted as one crate added (`added`) or as
    /// a change of the graph.
    #[allow(clippy::too_many_arguments, reason = "a crate's fields, as `Crate::create` takes them")]
    fn create_noting(
        db: &mut Db,
        write: &'static str,
        data: BuiltCrateData,
        extra: ExtraCrateData,
        workspace: Arc<CrateWorkspaceData>,
        cfg: CfgOptions,
        env: Env,
        durability: Durability,
        added: bool,
    ) -> Crate {
        db.write(write, |db| {
            let root = root_path(&*db, data.root_file_id);
            let lints = LintOptions::default();
            let content =
                Crate::content_of(&*db, root, &data, &extra, &workspace, &cfg, &env, &lints);
            let krate =
                Crate::new(&*db, durability, data, extra, workspace, cfg, env, InputId(content));
            db.crates_map().lock().note(krate, root, true);
            // Not in the graph yet, so this moves no graph content and notes nothing of it.
            db.set_crate_attr_files(krate, false, Some(durability));
            let map = db.crates_map();
            seal_crate(db, &mut map.lock(), krate);
            db.note(match added {
                true => Note::CrateAdded(krate),
                false => Note::Crates,
            });
            krate
        })
    }

    pub fn set_data(self, db: &mut Db, data: BuiltCrateData) {
        db.write_crates("Crate::set_data", |db| {
            self.set_data_field(db).to(data);
            db.set_crate_attr_files(self, false, None);
            seal_crate_depends(db, self);
            refresh_contents(db, self);
        })
    }

    pub fn set_extra_data(self, db: &mut Db, extra: ExtraCrateData) {
        db.write_crates("Crate::set_extra_data", |db| {
            self.set_extra_data_field(db).to(extra);
            refresh_contents(db, self);
        })
    }

    pub fn set_workspace_data(self, db: &mut Db, workspace: Arc<CrateWorkspaceData>) {
        db.write_crates("Crate::set_workspace_data", |db| {
            self.set_workspace_data_field(db).to(workspace);
            refresh_contents(db, self);
        })
    }

    pub fn set_cfg_options(self, db: &mut Db, cfg: CfgOptions) {
        db.write_crates("Crate::set_cfg_options", |db| {
            self.set_cfg_options_field(db).to(cfg);
            refresh_contents(db, self);
        })
    }

    pub fn set_env(self, db: &mut Db, env: Env) {
        db.write_crates("Crate::set_env", |db| {
            self.set_env_field(db).to(env);
            refresh_contents(db, self);
        })
    }

    pub fn set_lint_options(self, db: &mut Db, options: LintOptions) {
        db.write_crates("Crate::set_lint_options", |db| {
            self.set_lint_options_field(db).to(options);
            refresh_contents(db, self);
        })
    }

    /// `root` is the path of `data.root_file_id` ([`root_path`]).
    fn content_of(
        db: &Db,
        root: &VfsPath,
        data: &BuiltCrateData,
        extra: &ExtraCrateData,
        workspace: &CrateWorkspaceData,
        cfg: &CfgOptions,
        env: &Env,
        lints: &LintOptions,
    ) -> ContentId {
        let as_it_is = |bytes: &[u8]| -> Vec<u8> { bytes.to_vec() }; // alloc: a path, once per crate content ID (setup)
        Crate::content_with(
            Crate::DOMAIN,
            root,
            data,
            extra,
            workspace,
            cfg,
            env,
            lints,
            &as_it_is,
            &|dep| dep.content(db),
        )
    }

    /// The domain of [`Crate::ported_content`].
    pub const PORTED_DOMAIN: &'static str = "blaze v2 crate ported 1";

    /// [`Crate::content`] with every path it holds written as the database's portable keys
    /// write paths ([`crate::Files::ported`]: its root file's path, the directory its proc
    /// macros run in, each variable's value), and each crate it depends on as `dep` gives it
    /// (its own ported content, the caller's memo). Everything else as the content ID has it,
    /// from the one function both are made by ([`Crate::content_with`]), so a field one covers
    /// the other does. What a key that must be the same for the same sources in another
    /// directory holds of a crate's settings (docs/v2/plan/design/store.md, "Content keys");
    /// what reads a path's own bytes (`env!("CARGO_MANIFEST_DIR")`) is the reader's to key.
    pub fn ported_content(self, db: &Db, dep: &dyn Fn(Crate) -> ContentId) -> ContentId {
        let data = self.data(db);
        let files = db.files();
        Crate::content_with(
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

    /// What [`Crate::content_of`] and [`Crate::ported_content`] hash: `text` writes what may
    /// hold a path, `dep` gives each crate depended on.
    fn content_with(
        domain: &'static str,
        root: &VfsPath,
        data: &BuiltCrateData,
        extra: &ExtraCrateData,
        workspace: &CrateWorkspaceData,
        cfg: &CfgOptions,
        env: &Env,
        lints: &LintOptions,
        text: &dyn Fn(&[u8]) -> Vec<u8>,
        dep_content: &dyn Fn(Crate) -> ContentId,
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

/// Sets the content ID of `krate`, and of every crate that depends on it whose ID changes
/// with it, after one of its fields was set.
fn refresh_contents(db: &mut Db, krate: Crate) {
    if !refresh_content(db, krate) {
        return;
    }
    let mut changed = HashSet::new_in(Global);
    changed.insert(krate);
    // In topological order: a crate's dependencies are refreshed before it.
    for dependent in all_crates(&*db).iter().copied() {
        if dependent != krate
            && dependent.data(&*db).dependencies.iter().any(|dep| changed.contains(&dep.crate_id))
            && refresh_content(db, dependent)
        {
            changed.insert(dependent);
        }
    }
    refresh_crate_graph_content(db);
}

/// Sets `krate`'s content ID from its fields as they are; whether it changed.
fn refresh_content(db: &mut Db, krate: Crate) -> bool {
    let root = root_path(&*db, krate.data(&*db).root_file_id);
    let content = Crate::content_of(
        &*db,
        root,
        krate.data(&*db),
        krate.extra_data(&*db),
        krate.workspace_data(&*db),
        krate.cfg_options(&*db),
        krate.env(&*db),
        krate.lint_options(&*db),
    );
    let changed = krate.content(&*db) != content;
    db.crates_map().lock().note(krate, root, changed);
    if changed {
        krate.set_content_field(db).to(InputId(content));
    }
    changed
}

#[cfg(test)]
impl Crate {
    /// The content ID hashing the crate's fields as they are now gives.
    pub(crate) fn content_from_fields(self, db: &Db) -> ContentId {
        let data = self.data(db);
        let (extra, workspace) = (self.extra_data(db), self.workspace_data(db));
        let root = root_path(db, data.root_file_id);
        let (cfg, env, lints) = (self.cfg_options(db), self.env(db), self.lint_options(db));
        Crate::content_of(db, root, data, extra, workspace, cfg, env, lints)
    }
}

/// How many crate content IDs base-db has made in `db`.
#[cfg(test)]
pub(crate) fn crates_hashed(db: &Db) -> u64 {
    db.crates_map().lock().clock
}

impl Crate {
    /// The crate and every crate it depends on, directly or not, in the order a walk from it
    /// first reaches them.
    ///
    /// **Warning**: do not use this query in `hir-*` crates! It kills incrementality across crate metadata modifications.
    pub fn transitive_deps(self, db: &Db) -> &[Crate] {
        transitive_deps(db, self)
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

    /// The crate and every crate that depends on it, directly or not, in the order a walk from
    /// it first reaches them.
    ///
    /// **Warning**: do not use this query in `hir-*` crates! It kills incrementality across crate metadata modifications.
    pub fn transitive_rev_deps(self, db: &Db) -> &[Crate] {
        transitive_rev_deps(db, self)
    }
}

#[blaze_query::tracked(returns(deref))]
fn transitive_deps(db: &Db, krate: Crate) -> Box<[Crate]> {
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
        Box::from(&deps[..]) // alloc: the memo's value at its size.
    })
}

#[blaze_query::tracked(returns(deref))]
fn transitive_loaded_deps(db: &Db, krate: Crate) -> Box<[Crate]> {
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
        Box::from(&deps[..]) // alloc: the memo's value at its size.
    })
}

#[blaze_query::tracked(returns(deref))]
fn transitive_rev_deps(db: &Db, krate: Crate) -> Box<[Crate]> {
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
        Box::from(&order[..]) // alloc: the memo's value at its size.
    })
}

/// The mapping from [`UniqueCrateData`] to their [`Crate`] input, and what each crate's content
/// ID was last made from. Read and written only by the setters, on `&mut` of the database
/// ([`CrateGraphBuilder::set_in_db`] locks it once for the whole graph); no query reads it.
#[derive(Debug)]
pub struct CratesMap(Lock<Crates>);

impl Default for CratesMap {
    fn default() -> CratesMap {
        CratesMap(Lock::new("db.crates_map", Crates::default()))
    }
}

impl CratesMap {
    #[track_caller]
    fn lock(&self) -> LockGuard<'_, Crates> {
        self.0.lock()
    }
}

#[derive(Debug, Default)]
struct Crates {
    /// Each crate's seal group ([`seal_crate`]).
    seal: HashMap<Crate, SealGroup, Global>,
    by_data: HashMap<UniqueCrateData, Crate, Global>,
    /// For each crate whose content ID base-db made, when and from which root path: what lets
    /// [`CrateGraphBuilder::set_in_db`] keep the ID of a crate whose fields are what they were
    /// without hashing it.
    stamps: HashMap<Crate, Stamp, Global>,
    /// Ticks once per content ID made.
    clock: u64,
}

#[derive(Debug)]
struct Stamp {
    /// When the crate's content ID was last made.
    hashed: u64,
    /// When it last changed: a crate hashed before this, of a crate depending on this one, is
    /// out of date.
    changed: u64,
    /// The path of the root file it was made with ([`root_path`]).
    root: VfsPath,
}

impl Crates {
    /// Notes that `krate`'s content ID was just made, with `root` its root file's path, and
    /// whether it `changed`. Returns when it last changed.
    fn note(&mut self, krate: Crate, root: &VfsPath, changed: bool) -> u64 {
        self.clock += 1;
        let clock = self.clock;
        match self.stamps.entry(krate) {
            Entry::Occupied(mut entry) => {
                let stamp = entry.get_mut();
                stamp.hashed = clock;
                if changed {
                    stamp.changed = clock;
                }
                if stamp.root != *root {
                    // clone: a crate's root path, when it is first hashed or its path moved (setup).
                    stamp.root = root.clone();
                }
                stamp.changed
            }
            Entry::Vacant(entry) => {
                // clone: as above.
                entry.insert(Stamp { hashed: clock, changed: clock, root: root.clone() });
                clock
            }
        }
    }

    /// When `krate`'s content ID last changed, if it was made from `root` after every
    /// dependency's last changed (`deps_changed`, the latest of them): then it is what hashing
    /// the crate again would give, as long as its fields are what they were.
    fn fresh(&self, krate: Crate, root: &VfsPath, deps_changed: u64) -> Option<u64> {
        let stamp = self.stamps.get(&krate)?;
        (stamp.root == *root && deps_changed < stamp.hashed).then_some(stamp.changed)
    }
}

/// [`CrateGraphBuilder::set_in_db`]'s walk of the graph, each crate after its dependencies.
struct Walk<'a, 'f> {
    graph: &'a CrateGraphBuilder,
    crates: &'a mut Crates,
    all: FxIndexSet<Crate>,
    visited: CratesIdMap,
    root_files: HashSet<FileId, Global>,
    /// Each builder crate walked, by its index: its crate, and when that crate's content ID
    /// last changed ([`Stamp::changed`]).
    done: &'f mut [Option<(Crate, u64)>],
    frame: &'f Frame,
}

impl Walk<'_, '_> {
    fn go(&mut self, db: &mut Db, source: CrateBuilderId) -> (Crate, u64) {
        let at = source.into_raw().into_u32() as usize;
        if let Some(done) = self.done[at] {
            return done;
        }
        let graph = self.graph;
        let krate = &graph[source];
        let mut deps = Vec::with_capacity_in(krate.basic.dependencies.len(), self.frame);
        let mut deps_changed = 0;
        for dep in &krate.basic.dependencies {
            let (id, changed) = self.go(db, dep.crate_id);
            deps.push(id);
            deps_changed = deps_changed.max(changed);
        }
        let root_file_id = krate.basic.root_file_id;
        let key = match self.root_files.insert(root_file_id) {
            true => UniqueCrateData { root_file_id, disambiguator: None },
            false => UniqueCrateData {
                root_file_id,
                // alloc: a second crate on one root file is keyed by its data (setup).
                disambiguator: Some(Box::new((
                    built_data(krate, &deps),
                    krate.cfg_options.to_hashable(),
                ))),
            },
        };
        let (input, changed) = match self.crates.by_data.get(&key) {
            Some(&old) => (old, self.update(db, old, krate, &deps, deps_changed)),
            None => {
                let made = self.create(db, krate, &deps);
                self.crates.by_data.insert(key, made.0);
                made
            }
        };
        db.set_crate_attr_files(input, false, Some(graph_durability(&krate.basic.origin)));
        self.all.insert(input);
        self.visited.insert(source, input);
        self.done[at] = Some((input, changed));
        (input, changed)
    }

    /// Sets each field of `old` that differs from `krate`'s (`deps` its dependencies' crates),
    /// then its content ID unless it is fresh ([`Crates::fresh`]). Returns when the content
    /// ID last changed.
    fn update(
        &mut self,
        db: &mut Db,
        old: Crate,
        krate: &CrateBuilder,
        deps: &[Crate],
        deps_changed: u64,
    ) -> u64 {
        let durability = graph_durability(&krate.basic.origin);
        let mut set = false;
        if !same_data(&krate.basic, deps, old.data(db)) {
            old.set_data_field(db).with_durability(durability).to(built_data(krate, deps));
            set = true;
        }
        if krate.extra != *old.extra_data(db) {
            // clone: only a field that changed is set (setup).
            old.set_extra_data_field(db).with_durability(durability).to(krate.extra.clone());
            set = true;
        }
        if krate.cfg_options != *old.cfg_options(db) {
            // clone: only a field that changed is set (setup).
            old.set_cfg_options_field(db).with_durability(durability).to(krate.cfg_options.clone());
            set = true;
        }
        if krate.env != *old.env(db) {
            // clone: only a field that changed is set (setup).
            old.set_env_field(db).with_durability(durability).to(krate.env.clone());
            set = true;
        }
        if krate.ws_data != *old.workspace_data(db) {
            // clone: only a field that changed is set (setup).
            old.set_workspace_data_field(db).with_durability(durability).to(krate.ws_data.clone());
            set = true;
        }
        let root = root_path(&*db, krate.basic.root_file_id);
        if !set && let Some(changed) = self.crates.fresh(old, root, deps_changed) {
            return changed;
        }
        let content = Crate::content_of(
            &*db,
            root,
            old.data(&*db),
            old.extra_data(&*db),
            old.workspace_data(&*db),
            old.cfg_options(&*db),
            old.env(&*db),
            old.lint_options(&*db),
        );
        let changed = content != old.content(&*db);
        let stamp = self.crates.note(old, root, changed);
        if changed {
            old.set_content_field(db).with_durability(durability).to(InputId(content));
        }
        stamp
    }

    /// A crate made of `krate` (`deps` its dependencies' crates), and when its content ID
    /// changed: now.
    fn create(&mut self, db: &mut Db, krate: &CrateBuilder, deps: &[Crate]) -> (Crate, u64) {
        let data = built_data(krate, deps);
        let root = root_path(&*db, data.root_file_id);
        let content = Crate::content_of(
            &*db,
            root,
            &data,
            &krate.extra,
            &krate.ws_data,
            &krate.cfg_options,
            &krate.env,
            &LintOptions::default(),
        );
        let input = Crate::new(
            &*db,
            graph_durability(&krate.basic.origin),
            data,
            // clone: a new crate's fields (setup).
            krate.extra.clone(),
            // clone: a new crate's fields (setup).
            krate.ws_data.clone(),
            // clone: a new crate's fields (setup).
            krate.cfg_options.clone(),
            // clone: a new crate's fields (setup).
            krate.env.clone(),
            InputId(content),
        );
        let changed = self.crates.note(input, root, true);
        seal_crate(db, &mut *self.crates, input);
        (input, changed)
    }
}

/// Makes `krate`'s seal group (`blaze_query::db::seal`) as the crate is made: the crate's own
/// fields are its inputs; it depends on its dependencies' groups and on its source root's,
/// and it opens with its source root's, so an edit of one of its files opens it at the write.
/// A workspace member's group is sealed after two builds that wrote nothing of it, any other
/// crate's at once (`SealKind`).
///
/// A crate whose dependencies are set again later keeps the group, with the new ones added
/// ([`seal_crate_depends`]).
fn seal_crate(db: &mut Db, crates: &mut Crates, krate: Crate) {
    let data = krate.data(&*db);
    let kind = match data.origin.is_local() {
        true => SealKind::Local,
        false => SealKind::Library,
    };
    let root_file = data.root_file_id;
    let deps: Vec<SealGroup> = // alloc: a crate's dependencies' groups, once a crate
        data.dependencies.iter().filter_map(|dep| crates.seal.get(&dep.crate_id).copied()).collect();
    let name = match krate.rust_name(&*db) {
        Ok(name) => name.as_str().to_owned(), // alloc: a group's name, once a crate
        Err(_) => String::from("a crate with no name"), // alloc: as above
    };
    let files = db.shared_files();
    let root = {
        let mut writer = files.lock();
        let root = files.file_source_root(&*db, root_file).source_root_id(&*db);
        files.seal_root(&mut writer, db, root)
    };
    let storage = db.storage_mut();
    let group = storage.seal_group(&name, kind, &deps);
    if let Some(root) = root {
        storage.seal_group_opens_with(root, group);
    }
    krate.seal_group(db, group);
    crates.seal.insert(krate, group);
}

/// Adds the groups of `krate`'s dependencies, as its data names them now, to what its seal
/// group depends on: after its data was set again.
fn seal_crate_depends(db: &mut Db, krate: Crate) {
    let map = db.crates_map();
    let crates = map.lock();
    let Some(&group) = crates.seal.get(&krate) else { return };
    let deps: Vec<SealGroup> = krate // alloc: a crate's dependencies' groups, at a change of them
        .data(&*db)
        .dependencies
        .iter()
        .filter_map(|dep| crates.seal.get(&dep.crate_id).copied())
        .collect();
    drop(crates);
    db.storage_mut().seal_group_depends(group, &deps);
}

/// The kind of a crate's inputs when a [`CrateGraphBuilder`] makes it: `HIGH` for a library's
/// (the toolchain's crates, registry and git dependencies), as blaze-lower states them
/// ([`Crate::create`]), so [`crate::crate_durability`] of such a crate reads nothing below
/// `HIGH`; `MEDIUM` for a workspace crate's, its structure.
fn graph_durability(origin: &CrateOrigin) -> Durability {
    match origin {
        CrateOrigin::Local { .. } => Durability::MEDIUM,
        CrateOrigin::Library { .. } | CrateOrigin::Lang(_) | CrateOrigin::Rustc { .. } => {
            Durability::HIGH
        }
    }
}

/// `krate`'s data with `deps` its dependencies' crates: built only for a crate that is new or
/// whose data changed.
fn built_data(krate: &CrateBuilder, deps: &[Crate]) -> BuiltCrateData {
    let basic = &krate.basic;
    let dependencies = basic
        .dependencies
        .iter()
        .zip(deps)
        .map(|(dep, &crate_id)| BuiltDependency {
            crate_id,
            // clone: a dependency's name, in the data of a crate that changed (setup).
            name: dep.name.clone(),
            prelude: dep.prelude,
            sysroot: dep.sysroot,
            private: dep.private,
        })
        // alloc: as above.
        .collect();
    BuiltCrateData {
        root_file_id: basic.root_file_id,
        edition: basic.edition,
        dependencies,
        // clone: as above.
        origin: basic.origin.clone(),
        // clone: as above.
        crate_attrs: basic.crate_attrs.clone(),
        target_kind: basic.target_kind,
        manifest_kind: basic.manifest_kind,
        crate_types: basic.crate_types,
        // clone: as above.
        proc_macro_cwd: basic.proc_macro_cwd.clone(),
    }
}

/// Whether `old` is [`built_data`] of `new` and `deps`, told without building it.
fn same_data(new: &CrateDataBuilder, deps: &[Crate], old: &BuiltCrateData) -> bool {
    let CrateData {
        root_file_id,
        edition,
        dependencies,
        origin,
        crate_attrs,
        target_kind,
        manifest_kind,
        crate_types,
        proc_macro_cwd,
    } = new;
    *root_file_id == old.root_file_id
        && *edition == old.edition
        && *target_kind == old.target_kind
        && *manifest_kind == old.manifest_kind
        && *crate_types == old.crate_types
        && *origin == old.origin
        && *crate_attrs == old.crate_attrs
        && *proc_macro_cwd == old.proc_macro_cwd
        && dependencies.len() == old.dependencies.len()
        && dependencies.iter().zip(deps).zip(&old.dependencies).all(|((new, &crate_id), old)| {
            let Dependency { crate_id: _, name, prelude, sysroot, private } = new;
            crate_id == old.crate_id
                && *name == old.name
                && *prelude == old.prelude
                && *sysroot == old.sysroot
                && *private == old.private
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

    /// Sets the graph's crates in `db`, writing only what differs from what is set (the query engine does
    /// not compare an input's new value with its old one). A crate whose fields, root path and
    /// dependencies' content IDs are what its content ID was made from is not hashed again.
    pub fn set_in_db(self, db: &mut Db) -> CratesIdMap {
        db.write_crates("CrateGraphBuilder::set_in_db", |db| self.set_in_db_inner(db))
    }

    fn set_in_db_inner(self, db: &mut Db) -> CratesIdMap {
        let old_all_crates = all_crates(db);
        let crates_map = db.crates_map();
        let mut crates = crates_map.lock();
        let (visited, all) = blaze_mem::frame::task(blaze_mem::Tag::Parse, |frame| {
            let len = self.arena.len();
            let mut walk = Walk {
                graph: &self,
                crates: &mut *crates,
                // For some reason in some repositories we have duplicate crates, so we use a set and not `Vec`.
                // We use an `IndexSet` because the list needs to be topologically sorted.
                all: FxIndexSet::with_capacity_and_hasher(len, FixedState),
                visited: HashMap::with_capacity_in(len, Global),
                root_files: HashSet::with_capacity_in(len, Global),
                done: frame.alloc_slice_fill_with(len, |_| None),
                frame,
            };
            for krate in self.iter() {
                walk.go(db, krate);
            }
            (walk.visited, walk.all)
        });
        drop(crates);

        for &krate in old_all_crates.iter() {
            if !all.contains(&krate) {
                db.set_crate_attr_files(krate, true, Some(Durability::MEDIUM));
            }
        }
        if old_all_crates.len() != all.len()
            || old_all_crates.iter().any(|&krate| !all.contains(&krate))
        {
            set_all_crates_with_durability(db, all, Durability::MEDIUM);
        }
        refresh_crate_graph_content(db);
        visited
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
