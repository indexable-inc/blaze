//! Which proc-macro crates a build reaches and the module each one runs: the one home of the
//! proc-macro hook's registration, for every caller (blaze-world's `names` example, blaze-real's
//! `names_load`, and the command line's `MacroModule` task once it is wired). A module is never
//! compiled here: it is looked up in the module store the bootstrap tool (`proc_macro_modules`)
//! fills.

/// The files a macro called in a crate is shown: that crate's own package, whoever it is.
pub mod files;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::string::{String, ToString};
use std::vec::Vec;

#[cfg(feature = "procs-run")]
use blaze_id::CrateItemKey;
use blaze_manifest::lock::{Checksum, Lock, LockId, LockSource};
#[cfg(feature = "procs-run")]
use blaze_proc_macro::hook::{AddError, ProcMacros, Reached};
use blaze_proc_macro::module::{ClosureError, CrateId, LockedPackages, ModuleError, Source};
use blaze_unit_graph::Unit;

/// Why a macro crate's module identity could not be made: its module is not looked up, one
/// finding per crate.
#[derive(Debug)]
pub enum Unidentified {
    /// A git macro crate whose folder in the repository no module of the store names (the
    /// lock does not say it).
    GitFolderUnknown { package: String, version: String },
    /// A path macro crate whose folder the caller did not give.
    PathWithoutFolder { package: String, version: String },
    /// Its dependency closure could not be read from the lock.
    Closure(ClosureError),
}

impl core::fmt::Display for Unidentified {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Unidentified::GitFolderUnknown { package, version } => {
                write!(f, "{package} {version}: a git macro crate whose folder in its repository no module of the store names")
            }
            Unidentified::PathWithoutFolder { package, version } => write!(f, "{package} {version}: a path macro crate whose folder was not given"),
            Unidentified::Closure(error) => write!(f, "{error}"),
        }
    }
}

/// A macro crate's module identity, owned: what its module key is the digest of.
#[derive(Clone, Debug)]
pub struct MacroIdentity {
    pub package: String,
    pub version: String,
    source: OwnedSource,
    deps: String,
    features: Vec<String>,
}

#[derive(Clone, Debug)]
enum OwnedSource {
    Registry(String),
    Git { commit: String, path: String },
    Path(String),
}

impl MacroIdentity {
    /// `f` over the borrowed identity blaze-proc-macro keys by.
    pub fn with_id<R>(&self, f: impl FnOnce(&CrateId<'_>) -> R) -> R {
        let features: Vec<&str> = self.features.iter().map(String::as_str).collect();
        let source = match &self.source {
            OwnedSource::Registry(checksum) => Source::Registry { checksum },
            OwnedSource::Git { commit, path } => Source::Git { commit, path },
            OwnedSource::Path(files_sha256) => Source::Path { files_sha256 },
        };
        f(&CrateId { name: &self.package, version: &self.version, source, deps: &self.deps, features: &features })
    }
}

/// The git folders the store's modules were built from, by `name version`: read from the
/// module store's `identity` files (the lock does not name a git package's folder).
pub fn git_folders(modules: &Path) -> Result<BTreeMap<String, String>, ProcsError> {
    let listing = modules.join("proc-macro/modules");
    let mut git_folders: BTreeMap<String, String> = BTreeMap::new();
    let listed = match std::fs::read_dir(&listing) {
        Ok(listed) => listed,
        // An empty store: no module, so no git folder either.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(git_folders),
        Err(error) => return Err(ProcsError::Store { at: listing, error }),
    };
    for entry in listed {
        let entry = entry.map_err(|error| ProcsError::Store { at: listing.clone(), error })?;
        // A module folder without an identity file is not a git module's: only those name one.
        let Ok(identity) = std::fs::read_to_string(entry.path().join("identity")) else { continue };
        let field = |k: &str| identity.lines().find_map(|l| l.strip_prefix(k).map(str::to_string));
        if let (Some(name), Some(version), Some(git)) = (field("name "), field("version "), field("git "))
            && let Some((_, folder)) = git.split_once(' ')
        {
            git_folders.insert(std::format!("{name} {version}"), folder.to_string());
        }
    }
    Ok(git_folders)
}

/// The module identity of the macro crate `unit` of `lock` (its packages read once, `locked`):
/// a registry checksum, a git commit with the folder `git_folders` names, or a path package's
/// files digest from `path_files`: the hex of `blaze_proc_macro::module::files_digest` over the
/// package's files as the caller read them (blaze-cli: its `MacroFiles` leaf row, whose id the
/// module lookup's key holds; [`register_macro_crates`]: the folder read here), or `None` for a
/// path package it has no files of (then `PathWithoutFolder`, by name). Called at most once.
pub fn identify<D: AsRef<str>>(
    lock: &Lock<'_>,
    locked: &LockedPackages<'_, '_>,
    git_folders: &BTreeMap<String, String>,
    unit: &Unit<'_>,
    path_files: impl FnOnce(LockId) -> Result<Option<D>, ProcsError>,
) -> Result<Result<MacroIdentity, Unidentified>, ProcsError> {
    let (package, version) = (unit.name.as_str(), unit.version.as_str());
    let named = || (package.to_string(), version.to_string());
    let locked_package = lock.package(unit.package);
    let source = match &locked_package.source {
        LockSource::Registry { .. } => OwnedSource::Registry(match &locked_package.checksum {
            Checksum::Sha256(t) => t.as_str().to_string(),
            Checksum::Absent => String::new(),
        }),
        LockSource::Git { commit, .. } => match git_folders.get(&std::format!("{package} {version}")) {
            Some(folder) => OwnedSource::Git { commit: commit.as_str().to_string(), path: folder.clone() },
            None => {
                let (package, version) = named();
                return Ok(Err(Unidentified::GitFolderUnknown { package, version }));
            }
        },
        LockSource::Path => match path_files(unit.package)? {
            Some(files_sha256) => OwnedSource::Path(files_sha256.as_ref().to_string()),
            None => {
                let (package, version) = named();
                return Ok(Err(Unidentified::PathWithoutFolder { package, version }));
            }
        },
    };
    let deps = match locked.closure(package, version) {
        Ok(deps) => deps,
        Err(error) => return Ok(Err(Unidentified::Closure(error))),
    };
    let features = unit.features.iter().map(|f| f.as_str().to_string()).collect();
    Ok(Ok(MacroIdentity { package: package.to_string(), version: version.to_string(), source, deps, features }))
}

/// A macro crate the hook could not register: its macros' calls are refused, one finding per
/// crate.
#[cfg(feature = "procs-run")]
#[derive(Debug)]
pub enum Unregistered {
    /// Its module identity could not be made.
    Unidentified(Unidentified),
    /// The hook refused it (a module that does not read).
    Refused(AddError),
}

/// Why the registration could not be made at all.
#[derive(Debug)]
pub enum ProcsError {
    /// The module store's `proc-macro/modules` folder could not be listed.
    Store { at: PathBuf, error: std::io::Error },
    /// A path macro crate's files could not be read for its identity.
    Files { at: PathBuf, error: ModuleError },
}

impl core::fmt::Display for ProcsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ProcsError::Store { at, error } => write!(f, "the module store at {}: {error}", at.display()),
            ProcsError::Files { at, error } => write!(f, "the macro crate's files at {}: {error}", at.display()),
        }
    }
}

/// What the registration made: each registered crate's caller index and module key, and the
/// crates it could not register.
#[cfg(feature = "procs-run")]
pub struct Registered {
    pub keys: Vec<(usize, CrateItemKey)>,
    pub unregistered: Vec<(usize, Unregistered)>,
}

/// Registers in `procs` each proc-macro unit of `crates` (the caller's index for it, and the
/// unit) by its lock identity: a registry checksum, a path package's files as they are now (its
/// folder from `path_dir`), a git commit with the folder the store's module identity names.
/// `modules` is the module store (its `proc-macro/modules`). A crate with no module is
/// registered without one; the hook answers its calls with a refusal naming it.
#[cfg(feature = "procs-run")]
pub fn register_macro_crates<'u>(
    procs: &mut ProcMacros<'_>,
    modules: &Path,
    lock: &Lock<'_>,
    crates: impl IntoIterator<Item = (usize, &'u Unit<'u>)>,
    path_dir: impl Fn(LockId) -> Option<PathBuf>,
) -> Result<Registered, ProcsError> {
    let git_folders = git_folders(modules)?;
    let locked = LockedPackages::of(lock);
    let mut made = Registered { keys: Vec::new(), unregistered: Vec::new() };
    for (index, unit) in crates {
        // A path package's files as they are now, from the folder `path_dir` gives.
        let path_files = |id| match path_dir(id) {
            None => Ok(None),
            Some(dir) => {
                let mut listed = blaze_proc_macro::module::package_files(&dir).map_err(|error| ProcsError::Files { at: dir.clone(), error })?;
                Ok(Some(blaze_proc_macro::module::files_digest(&mut listed)))
            }
        };
        let identity = match identify(lock, &locked, &git_folders, unit, path_files)? {
            Ok(identity) => identity,
            Err(why) => {
                made.unregistered.push((index, Unregistered::Unidentified(why)));
                continue;
            }
        };
        let added = identity.with_id(|id| {
            let reached = Reached { name: id.name, version: id.version, source: id.source, features: id.features };
            procs.add_reached(modules, &locked, &reached)
        });
        match added {
            Ok(key) => made.keys.push((index, key)),
            Err(error) => made.unregistered.push((index, Unregistered::Refused(error))),
        }
    }
    Ok(made)
}
