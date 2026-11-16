//! The crate graph as the type checker reads it: a [`Crate`] is a memo input in its own arena,
//! made by the driver from the unit's facts (cfg, env, edition, origin: what Cargo and the
//! toolchain say) and the crate's [`CrateItemKey`], the key every item key of the crate begins
//! with. Which crates a crate sees by which name is the `CrateGraph` record
//! ([`blaze_surface::Items::crate_graph`]); the driver writes [`CrateData::dependencies`] from
//! it, so the two never disagree.
//!
//! Nothing here does IO and nothing here keys a record: a record names a crate by its
//! [`CrateItemKey`] ([`Crate::key`]), never by this id, which is local to the snapshot.

use std::alloc::Global;
use std::str::FromStr;
use std::{fmt, ops};

use blaze_hash::HashMap;
use blaze_ids::Hasher;
use blaze_surface::{ContentId, CrateItemKey};
use cfg::CfgOptions;
use intern::Symbol;
use span::{Edition, FileId};
use triomphe::Arc;

use crate::{CrateWorkspaceData, Db};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CrateName(Symbol);

impl CrateName {
    /// A crate name, refused as `Err` where it holds a dash.
    pub fn new(name: &str) -> Result<CrateName, &str> {
        if name.contains('-') { Err(name) } else { Ok(Self(Symbol::intern(name))) }
    }

    /// A crate name, each dash written as an underscore.
    pub fn normalize_dashes(name: &str) -> CrateName {
        let mut text = blaze_mem::String::with_capacity_in(name.len(), Global);
        for c in name.chars() {
            text.push(if c == '-' { '_' } else { c });
        }
        Self(Symbol::intern(&text))
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

/// A crate set up with no name where its name is needed ([`Crate::rust_name`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct UnnamedCrate {
    /// The crate's key, which tells it from every other crate.
    pub content: ContentId,
}

impl fmt::Display for UnnamedCrate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{unnamed crate {}}}", self.content)
    }
}

impl std::error::Error for UnnamedCrate {}

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

/// Why a proc-macro crate has no expanders.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProcMacroLoadingError {
    Disabled,
    FailedToBuild,
    ExpectedProcMacroArtifact,
    MissingDylibPath,
    NotYetBuilt,
    /// blaze-macro-eval cannot run the crate's macros; the message names the crate and why.
    Refused(Box<str, Global>),
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

impl std::error::Error for ProcMacroLoadingError {}

impl fmt::Display for ProcMacroLoadingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcMacroLoadingError::ExpectedProcMacroArtifact => {
                f.write_str("proc-macro crate did not build proc-macro artifact")
            }
            ProcMacroLoadingError::Disabled => f.write_str("proc-macro expansion is disabled"),
            ProcMacroLoadingError::FailedToBuild => f.write_str("proc-macro failed to build"),
            ProcMacroLoadingError::MissingDylibPath => {
                f.write_str("proc-macro crate built but its module is missing")
            }
            ProcMacroLoadingError::NotYetBuilt => f.write_str("proc-macro not yet built"),
            ProcMacroLoadingError::Refused(msg) => f.write_str(msg),
        }
    }
}

/// Where a crate comes from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CrateOrigin {
    /// Crates that are from the rustc workspace.
    Rustc { name: Symbol },
    /// Crates that are workspace members.
    Local { repo: Option<Box<str, Global>>, name: Option<Symbol> },
    /// Crates that are non member libraries.
    Library { repo: Option<Box<str, Global>>, name: Symbol },
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

    /// What a key hashes of the origin: a tag, then its repo and name.
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
        f.write_str(match self {
            LangCrateOrigin::Alloc => "alloc",
            LangCrateOrigin::Core => "core",
            LangCrateOrigin::ProcMacro => "proc_macro",
            LangCrateOrigin::Std => "std",
            LangCrateOrigin::Test => "test",
            LangCrateOrigin::Other | LangCrateOrigin::Dependency => "other",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CrateDisplayName {
    /// The name paths use (with `_`).
    crate_name: CrateName,
    /// The name as the manifest declares it, dashes kept.
    canonical_name: Symbol,
}

impl CrateDisplayName {
    pub fn canonical_name(&self) -> &Symbol {
        &self.canonical_name
    }

    pub fn crate_name(&self) -> &CrateName {
        &self.crate_name
    }

    pub fn from_canonical_name(canonical_name: &str) -> CrateDisplayName {
        let crate_name = CrateName::normalize_dashes(canonical_name);
        CrateDisplayName { crate_name, canonical_name: Symbol::intern(canonical_name) }
    }
}

impl From<CrateName> for CrateDisplayName {
    fn from(crate_name: CrateName) -> CrateDisplayName {
        let canonical_name = crate_name.0.clone(); // clone: an interned symbol, once per crate
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetKind {
    Bin,
    Lib { is_proc_macro: bool },
    Example,
    Test,
    Bench,
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

    fn bit(self) -> u8 {
        match self {
            CrateType::Bin => 1,
            CrateType::Lib => 1 << 1,
            CrateType::Rlib => 1 << 2,
            CrateType::Dylib => 1 << 3,
            CrateType::Cdylib => 1 << 4,
            CrateType::Staticlib => 1 << 5,
            CrateType::ProcMacro => 1 << 6,
        }
    }
}

/// A set of [`CrateType`]s, one bit each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CrateTypes(u8);

impl CrateTypes {
    pub fn of(types: &[CrateType]) -> CrateTypes {
        CrateTypes(types.iter().fold(0, |bits, it| bits | it.bit()))
    }

    pub fn contains(self, it: CrateType) -> bool {
        self.0 & it.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = CrateType> {
        CrateType::ALL.into_iter().filter(move |&it| self.contains(it))
    }

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

/// A channel name that is none of `stable`, `beta*` or `nightly`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownChannel;

impl FromStr for ReleaseChannel {
    type Err = UnknownChannel;

    fn from_str(str: &str) -> Result<Self, Self::Err> {
        Ok(match str {
            "" | "stable" => ReleaseChannel::Stable,
            "nightly" => ReleaseChannel::Nightly,
            _ if str.starts_with("beta") => ReleaseChannel::Beta,
            _ => return Err(UnknownChannel),
        })
    }
}

/// One dependency edge: the crate depended on and the name the dependent knows it by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Dependency<Id> {
    pub crate_id: Id,
    pub name: CrateName,
    pub prelude: bool,
    pub sysroot: bool,
    pub private: bool,
}

pub type BuiltDependency = Dependency<Crate>;

impl<Id> Dependency<Id> {
    pub fn new(name: CrateName, crate_id: Id) -> Self {
        Self { name, crate_id, prelude: true, sysroot: false, private: false }
    }

    pub fn is_prelude(&self) -> bool {
        self.prelude
    }

    pub fn is_sysroot(&self) -> bool {
        self.sysroot
    }

    pub fn is_private(&self) -> bool {
        self.private
    }
}

/// What the unit says of a crate.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CrateData<Id> {
    /// The crate root's number in the task's file table ([`crate::Db::file_content`]).
    pub root_file_id: FileId,
    pub edition: Edition,
    pub dependencies: Vec<Dependency<Id>, Global>,
    pub origin: CrateOrigin,
    pub crate_attrs: Box<[Box<str, Global>], Global>,
    pub target_kind: TargetKind,
    pub manifest_kind: TargetKind,
    pub crate_types: CrateTypes,
}

pub type BuiltCrateData = CrateData<Crate>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraCrateData {
    pub version: Option<Box<str, Global>>,
    pub display_name: Option<CrateDisplayName>,
    pub potential_cfg_options: Option<CfgOptions>,
}

#[derive(Default, Clone, PartialEq, Eq)]
pub struct Env {
    entries: HashMap<Box<str, Global>, Box<str, Global>, Global>,
}

impl Env {
    pub fn set(&mut self, name: &str, value: &str) {
        let key = blaze_mem::text::into_std_boxed_str(blaze_mem::String::from_str_in(name, Global));
        let text = blaze_mem::text::into_std_boxed_str(blaze_mem::String::from_str_in(value, Global));
        self.entries.insert(key, text);
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries.get(name).map(|it| &**it)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entries, sorted by name (never the map's order: rule 25).
    pub fn sorted(&self) -> Vec<(&str, &str), Global> {
        let mut entries = Vec::with_capacity_in(self.entries.len(), Global);
        entries.extend(self.entries.iter().map(|(k, v)| (&**k, &**v)));
        entries.sort_unstable();
        entries
    }
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.sorted()).finish()
    }
}

/// A crate of the task's snapshot: an input in the crate's own arena.
#[blaze_memo::input(debug)]
pub struct Crate {
    /// The crate's key: the prefix of every item key of the crate (blaze-id).
    #[returns(copy)]
    pub key: CrateItemKey,
    pub data: BuiltCrateData,
    pub extra_data: ExtraCrateData,
    pub workspace_data: Arc<CrateWorkspaceData>,
    pub cfg_options: CfgOptions,
    pub env: Env,
    /// The blaze extensions the crate opts into (its `#![blaze::…]` attributes, as the driver
    /// read them from the crate root's surface).
    #[returns(copy)]
    pub extensions: span::Extensions,
}

impl Crate {
    /// What tells the crate from every other one: its key, which covers its cfg, edition and
    /// dependencies (docs/v2/plan/design/identity.md).
    pub fn content<D: ?Sized + blaze_memo::Database>(self, db: &D) -> ContentId {
        self.key(db).0
    }

    pub fn rust_name<'db, D: ?Sized + blaze_memo::Database>(
        self,
        db: &'db D,
    ) -> Result<&'db Symbol, UnnamedCrate> {
        match &self.extra_data(db).display_name {
            Some(name) => Ok(name.crate_name().symbol()),
            None => Err(UnnamedCrate { content: self.content(db) }),
        }
    }

    pub fn named<'db, D: ?Sized + blaze_memo::Database>(self, db: &'db D) -> CrateNamed<'db> {
        CrateNamed(self.rust_name(db))
    }

    /// The crate and every crate it depends on, directly or not, each once, in the order a walk
    /// from it first reaches them.
    pub fn transitive_deps(self, db: &Db) -> &[Crate] {
        transitive_deps(db, self)
    }
}

#[blaze_memo::tracked(returns(ref), owns)]
fn transitive_deps(db: &Db, krate: Crate) -> Box<[Crate], Global> {
    let mut seen = Vec::new_in(Global);
    let mut work = Vec::new_in(Global);
    work.push(krate);
    while let Some(at) = work.pop() {
        if seen.contains(&at) {
            continue;
        }
        seen.push(at);
        work.extend(at.data(db).dependencies.iter().rev().map(|dep| dep.crate_id));
    }
    seen.into_boxed_slice()
}
