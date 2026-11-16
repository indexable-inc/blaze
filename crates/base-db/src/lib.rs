//! The front end's one database, [`Db`], over the new item layer
//! (docs/v2/plan/design/bottom.md; how the type checker is reconnected to it:
//! docs/v2/plan/design/reconnect.md).
//!
//! A [`Db`] is one task's snapshot: the memo the queries run on, the records of every crate the
//! task may read ([`Records`], [`blaze_surface::Items`]), the text of the crate it compiles under
//! that crate's [`blaze_surface::HasText`], the task's table of local items, and the crate list.
//! A question about a crate without text that finds no record stops the task with a [`Need`]
//! naming the record; the driver makes it and enters the task again.

#![feature(allocator_api, register_tool)]
#![register_tool(blaze)]
#![forbid(unsafe_code)]

mod db;
mod editioned_file_id;
mod given;
mod input;
mod need;
mod records;
pub mod target;

use std::str::FromStr as _;
use std::sync::atomic::AtomicUsize;

pub use blaze_ids::ContentId;
pub use blaze_surface::{CrateItemKey, HasText, ItemKey, SurfaceError};
pub use semver::{BuildMetadata, Prerelease, Version, VersionReq};
pub use span::FileId;

pub use crate::db::{
    AnchoredPath, BorrowckMode, CFG_OPTIONS_DOMAIN, Db, DbError, FileText, FrontSettings, Includes,
    all_crates,
    cfg_options_content, line_index, relevant_crates,
};
pub use crate::editioned_file_id::EditionedFileId;
pub use crate::given::{Gifts, Given, GivenTwice};
pub use crate::input::{
    BuiltCrateData, BuiltDependency, Crate, CrateData, CrateDisplayName, CrateName, CrateNamed,
    CrateOrigin, CrateType, CrateTypes, Dependency, Env, ExtraCrateData, LangCrateOrigin,
    ProcMacroLoadingError, ReleaseChannel, TargetKind, UnknownChannel, UnnamedCrate,
};
pub use crate::need::{Need, Needed, OrNeed};
pub use crate::records::{InsertError, RecordKind, Records};

use blaze_ids::Hasher;

/// A crate as a record names it: its item key. The old layer's world key is gone; the name is
/// kept for the readers that only name the type.
pub type CrateKey = CrateItemKey;

pub type FxIndexMap<K, V> = indexmap::IndexMap<K, V, blaze_hash::FixedState>;
pub type FxIndexSet<T> = indexmap::IndexSet<T, blaze_hash::FixedState>;

static NEXT_NONCE: AtomicUsize = AtomicUsize::new(0);

/// What tells two databases of one process apart ([`Db::nonce`]).
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
    /// The crates the capability effects trust: `trusted` under `[workspace.metadata.blaze]`,
    /// sorted by name, each once.
    pub trusted: Box<[TrustedCrate]>,
}

/// A crate a workspace trusts for the capability effects, with who answers for it and why.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct TrustedCrate {
    /// The crate's name as code names it (`-` written `_`).
    pub name: Box<str>,
    pub owner: Box<str>,
    pub reason: Box<str>,
}

impl TrustedCrate {
    /// The key of the table under `[workspace.metadata.blaze]`.
    pub const KEY: &str = "trusted";
}

/// What one evaluation of compile-time code may spend before it stops with an error:
/// `const-eval` under `[workspace.metadata.blaze]`.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct ConstEvalBudget {
    /// Basic blocks run.
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
    /// Of the target, the toolchain's version, the const-eval budget and the trusted crates.
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
        self.toolchain.as_ref().is_some_and(|v| *v >= VERSION_187)
    }
}

pub fn toolchain_channel(db: &Db, krate: Crate) -> Option<ReleaseChannel> {
    krate.workspace_data(db).toolchain.as_ref().and_then(|v| ReleaseChannel::from_str(&v.pre).ok())
}

/// A variable of the crate's environment, as `env!` reads it.
pub fn env_var<'db>(db: &'db Db, krate: Crate, name: &str) -> Option<&'db str> {
    krate.env(db).get(name)
}

/// The environment a proc macro of `krate`'s calls sees: the crate's.
pub fn macro_env(db: &Db, krate: Crate) -> &Env {
    krate.env(db)
}

/// The files of `krate`'s command-line crate attributes (rustc's `-Z crate-attr`) that rustc
/// applies after the root's own inner attributes. The task's file table has no file per
/// attribute: empty for a crate without any, and for one with some a typed stop naming its
/// root module, never a crate read as if it had none (the blaze edition and the extensions are
/// inputs of their own, [`Crate::blaze_edition`] and [`Crate::extensions`]).
pub fn applied_crate_attr_files(db: &Db, krate: Crate) -> &'static [EditionedFileId] {
    if krate.data(db).crate_attrs.is_empty() {
        return &[];
    }
    let item = blaze_surface::Items::crate_root(db.items(), krate.key(db)).or_need().head().root_module;
    Err(SurfaceError::NotBuilt(blaze_surface::NotBuilt {
        method: "the files of a crate's command-line crate attributes",
        item,
    }))
    .or_need()
}
