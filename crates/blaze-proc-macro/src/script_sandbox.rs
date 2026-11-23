//! A build script's view of the world: WASI preview 1 answered as Cargo's environment, with no
//! ambient authority (docs/v2/plan/design/build-scripts.md).
//!
//! - The package's tree is served read-only at what the crate sees (decision D96,
//!   [`ScriptRoot`]): a dependency's package at `/package`, a member's workspace at
//!   `/workspace` with the member at its dir; from a folder of the checkout or from bytes the
//!   caller read out of the package's record ([`Tree`]). Each path opened or looked at is
//!   recorded by its path from the tree's root with the digest of what was found: the read list.
//! - `OUT_DIR` (`/target/debug/build/<name>-<hash>/out`) is writable and in memory, bounded
//!   in bytes and files ([`RunLimits`]). Its ancestors are folders with one entry each. Any
//!   other path, a write outside `OUT_DIR` and a link out of the workspace stop the script
//!   ([`Refusal::Path`]).
//! - The environment is the explicit list `blaze_build_script::env` computed (what the Cargo
//!   Book documents, no `PATH`, `HOME` or user variable). It is whole in the run's key, so a
//!   changed value changes the key of exactly the units given it; it is not a read.
//! - The clock reads 0, randomness is zeros, there is no network and no waiting.
//! - Standard output is kept up to [`RunLimits::output`] bytes and [`RunLimits::line`] a line;
//!   standard error's last 64 KiB are kept for the error a failed script gives.
//! - A process spawn is one host call ([`ScriptSandbox::spawn`]): `rustc` is answered by
//!   blaze (`blaze_build_script::probe`), a C toolchain program by the C mode, anything else is
//!   refused. A refusal stops the script: it never returns an error the script could swallow.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use blaze_build_script::key::{CMode, RunLimits};
use blaze_build_script::git::{self, GitFiles};
use blaze_build_script::probe::{self, Row, Table};
use blaze_build_script::probe_ask::{self, Ask, Snippet, Verdict};
use blaze_expand::seen::{MemberDirError, SeenRoot};
use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_wasi::{Answer, Dirent, Errno, Filestat, Filetype, Open, ROOT_FD, Stop, System};

pub use crate::sandbox::Read;

/// The overflow observation names the first subject past this bound.
pub const READS_CAP: usize = 1 << 16;

/// A script's read-only view of its `links` dependencies' files (brief S1).
pub mod dep_mount;
use dep_mount::DepViews;

/// What a script sees as its package's root (decision D96), owned for [`Script`]: a
/// dependency's package is the tree, served at `/package`; a member's workspace is the tree,
/// served at `/workspace`, the member at its dir in it. Read only through [`ScriptRoot::seen`],
/// so the mount points keep one home (`blaze_expand::seen`, rule 30) and a dependency is never
/// mistaken for a workspace's root package (both had the folder `""`; rule 76).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptRoot {
    /// `None`: a dependency; `Some(dir)`: a member at `dir`, checked by `SeenRoot::member`.
    member: Option<Arc<str>>,
}

impl blaze_own::Share for ScriptRoot {}

impl ScriptRoot {
    /// A dependency's (registry, git, toolchain) script: it sees `/package`.
    pub const fn package() -> ScriptRoot {
        ScriptRoot { member: None }
    }

    /// A workspace member's script at `dir` under the workspace (`""`: its root package); a
    /// `dir` that is not a plain path under it is refused by name.
    pub fn member(dir: &str) -> Result<ScriptRoot, MemberDirError<'_>> {
        SeenRoot::member(dir)?;
        Ok(ScriptRoot { member: Some(Arc::from(dir)) })
    }

    pub fn seen(&self) -> SeenRoot<'_> {
        match &self.member {
            None => SeenRoot::Package,
            Some(dir) => SeenRoot::Member { dir },
        }
    }
}

/// How much of standard error is kept.
pub(crate) const STDERR_TAIL: usize = 64 << 10;

/// A served checkout's root, resolved once where the tree is made (rule 74: the proof every
/// read of the tree takes): absolute, with no link left in it. A read of the tree resolves the
/// node alone, an absolute path (one `realpath`, never a `getcwd`, rule 75), and holds it inside
/// by a prefix check against this; the root is never resolved again. Before 2026-11-20 each node
/// resolved the root too, a relative `.`, and so asked the process for its folder twice per read
/// (tempo's edit: `getcwd` 3.25% of the command's CPU, the reads a third of its opens).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RealRoot(Arc<Path>);

impl blaze_own::Share for RealRoot {}

/// Why a served checkout's root has no [`RealRoot`].
#[derive(Debug)]
pub enum RootUnresolved {
    /// [`RealRoot::resolve`] was given a relative path: a root is made absolute at the
    /// command's edge ([`RealRoot::of_start`]), never deeper.
    Relative(PathBuf),
    /// The path could not be resolved.
    Io { path: PathBuf, error: std::io::Error },
}

impl std::fmt::Display for RootUnresolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Relative(path) => write!(f, "the served tree's root `{}` is relative: a root is resolved from an absolute path only (script_sandbox.rs `RealRoot::resolve`)", path.display()),
            Self::Io { path, error } => write!(f, "cannot resolve the served tree's root `{}`: {error}", path.display()),
        }
    }
}

impl RealRoot {
    fn from_path(path: impl AsRef<Path>) -> RealRoot {
        RealRoot(Arc::from(path.as_ref()))
    }

    /// `absolute` with every link resolved: the one `realpath` of a served tree's root.
    pub fn resolve(absolute: &Path) -> Result<RealRoot, RootUnresolved> {
        if !absolute.is_absolute() {
            return Err(RootUnresolved::Relative(absolute.to_owned()));
        }
        std::fs::canonicalize(absolute).map(RealRoot::from_path).map_err(|error| RootUnresolved::Io { path: absolute.to_owned(), error })
    }

    /// `rel`, relative to the folder the process was started in (`.` for it), with every link
    /// resolved: the ONE resolution of the working directory a command makes, at its edge (one
    /// `realpath`, which asks the kernel for the folder once). Never `PWD`: an environment
    /// variable is ambient, can be stale or unset (`env -i`), and is never read for this (rule
    /// 75). `Err` names the path and the OS's error.
    pub fn of_start(rel: &Path, plant: impl blaze_own::planted::Planted<StartPlant>) -> Result<RealRoot, RootUnresolved> {
        if plant.has(StartPlant::LeftRelative) {
            return Ok(RealRoot::from_path(rel));
        }
        std::fs::canonicalize(rel).map(RealRoot::from_path).map_err(|error| RootUnresolved::Io { path: rel.to_owned(), error })
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// The bug a control plants in [`RealRoot::of_start`] (blaze_own::planted, D165).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StartPlant {
    /// As it is.
    None,
    /// The root is left as given, relative: every read of the tree then asks the process for
    /// its folder, and a prefix check against it refuses every node.
    LeftRelative,
}

impl blaze_own::planted::Plants for StartPlant {}

impl core::ops::Deref for RealRoot {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for RealRoot {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// The bugs a control plants in the served tree's reads (blaze_own::planted, D165).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TreePlant {
    /// The reads as they are.
    None,
    /// A resolved node is served without the prefix check against the root.
    PrefixUnchecked,
}

impl blaze_own::planted::Plants for TreePlant {}

/// The workspace's files.
#[derive(Clone, Debug)]
pub enum Tree {
    /// A folder on disk, read-only (a workspace member's checkout), at its resolved root.
    Dir(RealRoot),
    /// Files read out of a package's record: (path from the root, bytes), sorted by path.
    /// `git`: the commit when the record is a fetched git source's, whose `git` answers it
    /// gives (D54); `None` for a crate.
    Mem { files: Arc<Vec<(String, Vec<u8>)>>, git: Option<git::Oid> },
}

impl blaze_own::Share for Tree {}

/// A file or a folder of the tree.
enum Node<'t> {
    File(Cow<'t, [u8]>),
    Folder(Vec<(Vec<u8>, Filetype)>),
}

#[derive(Debug)]
enum TreeFault {
    Outside(PathBuf),
    Special(PathBuf),
    Io { path: PathBuf, op: &'static str, error: std::io::Error },
}

impl std::fmt::Display for TreeFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Outside(path) => write!(f, "`{}` is a link out of the served tree", path.display()),
            Self::Special(path) => write!(f, "`{}` in the served tree is neither a regular file nor a directory", path.display()),
            Self::Io { path, op, error } => write!(f, "cannot {op} `{}` in the served tree: {error}", path.display()),
        }
    }
}

fn missing(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory)
}

/// `rel` of the tree at `root`, every link resolved: one `realpath` of an absolute path, held
/// inside the root by a prefix check against the root resolved once ([`RealRoot`]). A link out
/// of the tree, or one whose target leaves it, is [`TreeFault::Outside`].
fn disk_node(root: &RealRoot, rel: &str, plant: impl blaze_own::planted::Planted<TreePlant>) -> Result<Option<(PathBuf, std::fs::Metadata)>, TreeFault> {
    let fault = |op, error| TreeFault::Io { path: PathBuf::from(rel), op, error };
    let real = match std::fs::canonicalize(root.path().join(rel)) {
        Ok(path) => path,
        Err(error) if missing(&error) => return Ok(None),
        Err(error) => return Err(fault("resolve", error)),
    };
    if !plant.has(TreePlant::PrefixUnchecked) && !real.starts_with(root.path()) {
        return Err(TreeFault::Outside(PathBuf::from(rel)));
    }
    // blaze's own folder and cargo's tagged build folder are no part of the checkout a script
    // sees (blaze_build_script::served): absent, reached by name or through a link. Without
    // this a run read `.blaze/v2/profile.txt`, which every command rewrites (turso c1851p).
    if let Some(std::path::Component::Normal(first)) = real.strip_prefix(root.path()).ok().and_then(|inner| inner.components().next())
        && !served_at_root(root, first.as_encoded_bytes())
    {
        return Ok(None);
    }
    let meta = std::fs::metadata(&real).map_err(|error| fault("stat", error))?;
    if !meta.is_file() && !meta.is_dir() {
        return Err(TreeFault::Special(PathBuf::from(rel)));
    }
    Ok(Some((real, meta)))
}

/// Whether the root entry `name` of the tree at `root` is served (blaze_build_script::served):
/// for a `target`, its `CACHEDIR.TAG`'s first bytes are read, as many as the signature has, so
/// every answer under `target` is a function of the tag (tagging it later moves them).
fn served_at_root(root: &RealRoot, name: &[u8]) -> bool {
    use blaze_build_script::served::{self, CACHEDIR_SIGNATURE, CARGO_TARGET, CACHEDIR_TAG, RootName};
    use std::io::Read as _;
    if served::root_name(name) != RootName::Target {
        return served::served_at_root(name, None);
    }
    let mut head = [0u8; CACHEDIR_SIGNATURE.len()];
    let mut len = 0;
    if let Ok(file) = std::fs::File::open(root.path().join(CARGO_TARGET).join(CACHEDIR_TAG)) {
        let mut bounded = file.take(head.len() as u64);
        while let Ok(read @ 1..) = bounded.read(&mut head[len..]) {
            len += read;
        }
        return served::served_at_root(name, Some(&head[..len]));
    }
    served::served_at_root(name, None)
}

pub const NOT_A_FILE: ContentId = ContentId([0xA1; 32]);
pub const NOT_A_FOLDER: ContentId = ContentId([0xA2; 32]);

/// The operation of a saved checkout row. Read failures that stop the sandbox
/// cannot equal any answer it successfully served before.
#[derive(Clone, Copy)]
pub enum TreeRead {
    File,
    Listing,
    Stat,
}

impl TreeRead {
    fn observed(self, node: Option<&Node<'_>>) -> Option<ContentId> {
        match (self, node?) {
            (Self::File, Node::File(bytes)) => Some(file_answer(bytes)),
            (Self::File, Node::Folder(_)) => Some(NOT_A_FILE),
            (Self::Listing, Node::Folder(entries)) => Some(listing_digest(entries)),
            (Self::Listing, Node::File(_)) => Some(NOT_A_FOLDER),
            (Self::Stat, Node::File(bytes)) => Some(stat_answer(false, bytes.len() as u64)),
            (Self::Stat, Node::Folder(_)) => Some(stat_answer(true, 0)),
        }
    }

    pub fn answer(self, tree: &Tree, path: &str) -> Result<Option<ContentId>, UnknownSubject> {
        let node = tree.node(path).map_err(|why| UnknownSubject(why.to_string()))?;
        Ok(self.observed(node.as_ref()))
    }
}

#[derive(Clone, Copy)]
enum NodeRead {
    Bytes,
    Stat,
    Open,
    OpenDirectory,
}

impl NodeRead {
    fn open(how: &Open) -> Self {
        if how.directory() { Self::OpenDirectory } else { Self::Open }
    }

    fn observation(self, found: Option<&Node<'_>>) -> (&'static str, Option<ContentId>) {
        let (prefix, read) = match (self, found) {
            (Self::Stat, _) => ("stat:", TreeRead::Stat),
            (Self::OpenDirectory, _) | (Self::Open, Some(Node::Folder(_))) => ("dir:", TreeRead::Listing),
            (Self::Open | Self::Bytes, _) => ("file:", TreeRead::File),
        };
        (prefix, read.observed(found))
    }
}

impl Tree {
    /// Where this tree's `git` answers come from.
    pub fn vcs(&self) -> git::Vcs {
        match self {
            Tree::Mem { git: Some(commit), .. } => git::Vcs::Fetched(*commit),
            Tree::Dir(_) | Tree::Mem { git: None, .. } => git::Vcs::Served,
        }
    }

    /// Returns `true` when every component from the tree root to `rel` exists and is not a
    /// symlink (always `true` for in-memory trees). A direct path's content/stat/listing can
    /// only move when the checkout journal reports a change to `rel` or one of its ancestors.
    pub fn is_direct(&self, rel: &str) -> bool {
        match self {
            Tree::Mem { .. } => true,
            Tree::Dir(root) => {
                if rel.is_empty() {
                    return true;
                }
                if rel.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
                    return false;
                }
                match disk_node(root, rel, blaze_own::planted::NoPlant) {
                    Ok(Some((real, _))) => real == root.path().join(rel),
                    _ => false,
                }
            }
        }
    }

    /// The bytes of the file `rel` of the tree; `None` when it is absent, a folder or a link
    /// out of the tree.
    pub fn file(&self, rel: &str) -> Option<Vec<u8>> {
        self.file_bytes(rel).ok().flatten().map(Cow::into_owned)
    }

    fn file_bytes(&self, rel: &str) -> Result<Option<Cow<'_, [u8]>>, TreeFault> {
        match self {
            Tree::Dir(root) => {
                let Some((path, meta)) = disk_node(root, rel, blaze_own::planted::NoPlant)? else { return Ok(None) };
                if meta.is_dir() {
                    return Ok(None);
                }
                std::fs::read(path).map(|bytes| Some(Cow::Owned(bytes))).map_err(|error| TreeFault::Io { path: PathBuf::from(rel), op: "read", error })
            }
            Tree::Mem { files, .. } => Ok(files.binary_search_by(|(path, _)| path.as_str().cmp(rel)).ok().map(|at| Cow::Borrowed(files[at].1.as_slice()))),
        }
    }

    /// What `rel` is: its bytes or its entries; `None` when absent. `Err`: a link out of the
    /// tree.
    fn node(&self, rel: &str) -> Result<Option<Node<'_>>, TreeFault> {
        match self {
            Tree::Dir(root) => {
                let fault = |op, error| TreeFault::Io { path: PathBuf::from(rel), op, error };
                let Some((real, meta)) = disk_node(root, rel, blaze_own::planted::NoPlant)? else { return Ok(None) };
                if meta.is_dir() {
                    let dir = std::fs::read_dir(&real).map_err(|error| fault("list", error))?;
                    let mut entries = dir.map(|entry| {
                        let entry = entry.map_err(|error| fault("list", error))?;
                        let kind = match std::fs::metadata(entry.path()) {
                            Ok(meta) if meta.is_dir() => Filetype::Directory,
                            Ok(_) => Filetype::RegularFile,
                            Err(error) if missing(&error) => Filetype::RegularFile,
                            Err(error) => return Err(fault("stat a directory entry of", error)),
                        };
                        Ok((entry.file_name().into_encoded_bytes(), kind))
                    }).collect::<Result<Vec<_>, TreeFault>>()?;
                    // The root's listing names neither folder `disk_node` serves as absent.
                    if real.as_path() == root.path() {
                        entries.retain(|(name, _)| served_at_root(root, name));
                    }
                    entries.sort_by(|a, b| a.0.cmp(&b.0));
                    return Ok(Some(Node::Folder(entries)));
                }
                let bytes = std::fs::read(&real).map_err(|error| fault("read", error))?;
                Ok(Some(Node::File(Cow::Owned(bytes))))
            }
            Tree::Mem { files, .. } => {
                if let Ok(at) = files.binary_search_by(|(path, _)| path.as_str().cmp(rel)) {
                    return Ok(Some(Node::File(Cow::Borrowed(&files[at].1))));
                }
                let prefix = if rel.is_empty() { String::new() } else { format!("{rel}/") };
                let start = files.partition_point(|(path, _)| path.as_str() < prefix.as_str());
                let mut entries: Vec<(Vec<u8>, Filetype)> = Vec::new();
                for (path, _) in files[start..].iter().take_while(|(path, _)| path.starts_with(&prefix)) {
                    let below = &path[prefix.len()..];
                    let (name, filetype) = match below.split_once('/') {
                        Some((name, _)) => (name, Filetype::Directory),
                        None => (below, Filetype::RegularFile),
                    };
                    if entries.last().is_none_or(|(last, _)| last.as_slice() != name.as_bytes()) {
                        entries.push((name.as_bytes().to_vec(), filetype));
                    }
                }
                // Full paths sort "a.txt" before "a/f"; the served names sort "a" first.
                entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                Ok((!entries.is_empty() || rel.is_empty()).then_some(Node::Folder(entries)))
            }
        }
    }
}

/// One script's run, as the caller describes it. (`Debug` is written by hand in
/// spawn_route.rs: the executor is printed by its name.)
#[derive(Clone)]
pub struct Script {
    /// `name version`, for errors.
    pub package: String,
    pub tree: Tree,
    /// What the script sees as its root: where `tree` is served and the package's folder in it.
    pub root: ScriptRoot,
    /// The environment, `(name, value)` sorted by name, paths in it virtual.
    pub env: Vec<(String, String)>,
    /// The target the script is run for, which a try-compile's key holds.
    pub target: String,
    /// The host, for `rustc -vV`.
    pub host: String,
    pub c_mode: CMode,
    pub limits: RunLimits,
    /// The answers for the pinned nightly, sorted by key.
    pub table: Arc<Vec<Row>>,
    /// The probe checker and its records, when the command's setting turns it on
    /// (script_probes.rs): a `rustc` probe is then checked by blaze's front end and the table
    /// is not read. `None` (the default until the checker answers the real probes' ladder):
    /// the table answers.
    pub probes: Option<crate::script_probes::ScriptProbes>,
    /// The deterministic sandbox's executor, chosen once per command by its setting; `None`:
    /// the sandbox is off (D92, D94), and a spawn that needs it is refused by name.
    pub spawner: Option<Arc<dyn blaze_spawn::Executor + Send + Sync>>,
    /// The run's base key, which seeds a spawn's clock and randomness (`SpawnRequest::step`);
    /// set by `ScriptRuntime::fresh` from the base it was handed.
    pub step: ContentId,
    /// What the command's setting says about spawns: whether hvbox is allowed (only which
    /// refusal), the pinned tools, the guest userland, the derived facts' pins.
    pub spawn_setting: Arc<crate::spawn_route::SpawnSetting>,
    /// The read-only mounts of its `links` dependencies' files (brief S1); what they serve is
    /// in the run's key ([`DepViews::key`]). `DepViews::none()` for a script given none.
    pub deps: Arc<DepViews>,
}

/// The environment a wasm run is given, owned, for [`Script::env`]: `env`'s variables and
/// `NUM_JOBS` `1` (`Jobs::One`: a wasm run has no threads, and no jobserver), made in `alloc`
/// and copied out. The job count is never in the key: the key takes `env` ([`crate::script`]).
pub fn owned_env(env: &blaze_build_script::Env<'_>, alloc: blaze_own::Alloc<'_>) -> Vec<(String, String)> {
    let run = env.for_run(blaze_build_script::Jobs::One, alloc);
    run.vars.iter().map(|(name, value)| (name.as_str().to_owned(), value.as_str().to_owned())).collect()
}

impl Script {
    /// `OUT_DIR`, from the environment.
    pub fn out_dir(&self) -> &str {
        self.env.iter().find(|(name, _)| name == "OUT_DIR").map_or("", |(_, value)| value.as_str())
    }

    /// The folder the script runs in: its `CARGO_MANIFEST_DIR` (D96).
    pub fn cwd(&self) -> String {
        self.root.seen().to_string()
    }
}

/// Why a script was stopped by the sandbox: each names what it did.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// A path outside `/workspace` and `OUT_DIR`, a write outside `OUT_DIR`, a link out.
    Path(String),
    /// `OUT_DIR` grew past its bytes or files.
    OutDir(String),
    /// Standard output past its bytes, or a line past its length.
    Output(String),
    /// A spawn blaze does not answer.
    Spawn(SpawnRefusal),
    /// A wait (`poll_oneoff`).
    Wait,
}

/// A spawn the sandbox does not answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SpawnRefusal {
    /// A try-compile no row of the table answers: its key, arguments and snippet, so the row
    /// can be written (with its reason) from this, and the target its key was made for (a
    /// macro-side run asks for wasm32-wasip1: a row of another target never answers it).
    CannotAnswerYet { key: ContentId, args: Vec<String>, snippet: String, target: String },
    /// `rustc` asked something that is neither a version nor a compile (`--print`), or a
    /// compile whose input file is not in the script's view.
    RustcArgs { args: Vec<String> },
    /// `rustc` with a line blaze's reader of a probe refuses (`probe_ask::classify`): the
    /// argument at `at` (the line's end for a missing input) and why.
    RustcLine { args: Vec<String>, at: usize, why: probe_ask::Why },
    /// With the probe checker on (script_probes.rs): a `rustc` line blaze's checker does not
    /// take (`probe_check::read_line`, `probe_check::probe`): the argument at `at` and why.
    ProbeLine { args: Vec<String>, at: usize, why: blaze_build_script::probe_check::Why },
    /// With the probe checker on: the checker cannot say (D193 point 5: a body not taken to its
    /// end, a sysroot it lacks, ...): the question's key and the stop's words. Never an exit.
    ProbeUnknown { key: ContentId, args: Vec<String>, why: String },
    /// With the probe checker on: the probe records refused a read or a write, or a record
    /// under the key is not an answer: the key and the stop's words.
    ProbeRecords { key: ContentId, args: Vec<String>, why: String },
    /// A program the sandbox does not run (a shell command, a tool in a build, ...).
    Program { program: String, args: Vec<String> },
    /// A `git` question blaze does not answer from the checkout (D53): the command after `git`.
    Git { command: String, why: git::Refused },
    /// The spawn request is not the shim's encoding.
    Malformed(Malformed),
    /// A program that runs only in the deterministic sandbox, which is off here (D92, D94).
    NoSandbox { program: String, args: Vec<String>, why: blaze_spawn::NoSandbox },
    /// The executor refused the spawn: its refusal's Display.
    Executor(String),
    /// A derived fact (pkg-config, `sw_vers`, a compiler's identity) not answered: why.
    Fact { program: String, args: Vec<String>, why: String },
    /// A spawn for the executor from a script whose workspace is a folder on disk, whose
    /// digest the request's read grant needs and the router does not compute.
    TreeOnDisk { program: String, args: Vec<String> },
    /// A child's output sent somewhere the router does not take (a file, mode 3).
    Mode { program: String, stream: &'static str, mode: u8 },
    /// A spawn of a program the machine does not have (spawn_route.rs `program_found`, D192)
    /// that arrived without the shim's `found` question: the module was built with a process
    /// shim that cannot return `NotFound`, so it is not given an answer it would misread.
    NotFoundUnasked { program: String, args: Vec<String> },
    /// The host has no pinned base tool set to look a spawned program up in.
    NoToolSet(crate::spawn_route::NoToolSet),
}

/// How a spawn request is not the shim's encoding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Malformed {
    /// It ends at this byte inside a field.
    EndsAt(usize),
    /// This field is not UTF-8.
    NotUtf8(&'static str),
    /// More arguments than the shim ever sends.
    Arguments(u32),
    /// An empty argv: no program.
    NoProgram,
}

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Malformed::EndsAt(at) => write!(f, "it ends at byte {at}"),
            Malformed::NotUtf8(what) => write!(f, "{what} is not UTF-8"),
            Malformed::Arguments(n) => write!(f, "{n} arguments"),
            Malformed::NoProgram => f.write_str("no program"),
        }
    }
}

impl std::fmt::Display for SpawnRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnRefusal::CannotAnswerYet { key, args, snippet, target } => {
                let first = snippet.lines().next().unwrap_or("");
                write!(
                    f,
                    "asked the compiler `rustc {}` about a snippet blaze cannot answer yet for the target `{target}` (probe {:02x?}, first line {first:?}): no row of the pinned table for `{target}` answers it",
                    args.join(" "),
                    &key.0[..8]
                )
            }
            SpawnRefusal::RustcArgs { args } => write!(f, "asked the compiler `rustc {}`, which blaze does not answer", args.join(" ")),
            SpawnRefusal::RustcLine { args, at, why } => match args.get(*at) {
                Some(arg) => write!(f, "asked the compiler `rustc {}`: its argument {at}, `{arg}`, is {why}", args.join(" ")),
                None => write!(f, "asked the compiler `rustc {}`: {why}", args.join(" ")),
            },
            SpawnRefusal::ProbeLine { args, at, why } => match args.get(*at) {
                Some(arg) => write!(f, "asked the compiler `rustc {}`: its argument {at}, `{arg}`, is refused: {why}", args.join(" ")),
                None => write!(f, "asked the compiler `rustc {}`, which is refused: {why}", args.join(" ")),
            },
            SpawnRefusal::ProbeUnknown { key, args, why } => write!(f, "asked the compiler `rustc {}` (probe {:02x?}): {why}", args.join(" "), &key.0[..8]),
            SpawnRefusal::ProbeRecords { key, args, why } => write!(f, "asked the compiler `rustc {}` (probe {:02x?}): {why}", args.join(" "), &key.0[..8]),
            SpawnRefusal::Program { program, args } => write!(f, "spawned `{program} {}`, a program a build script may not run", args.join(" ")),
            SpawnRefusal::Git { command, why } => write!(f, "asked `git {command}`, which blaze does not answer from the checkout: {why}"),
            SpawnRefusal::Malformed(why) => write!(f, "made a spawn request blaze cannot read: {why}"),
            SpawnRefusal::NoSandbox { program, args, why } => write!(
                f,
                "spawned `{program} {}`, which runs only in the deterministic sandbox; the sandbox is off on this machine: {why} (D92). Not run natively (rule 3)",
                args.join(" ")
            ),
            SpawnRefusal::Executor(why) => write!(f, "spawned a program the deterministic sandbox refused: {why}"),
            SpawnRefusal::Fact { program, args, why } => write!(f, "spawned `{program} {}`, a derived fact blaze does not answer: {why}", args.join(" ")),
            SpawnRefusal::TreeOnDisk { program, args } => write!(
                f,
                "spawned `{program} {}` from a workspace member's folder, whose tree digest a sandboxed spawn needs and blaze does not compute yet",
                args.join(" ")
            ),
            SpawnRefusal::Mode { program, stream, mode } => write!(f, "spawned `{program}` with its {stream} sent to mode {mode} (a file), which blaze does not take"),
            SpawnRefusal::NotFoundUnasked { program, args } => write!(
                f,
                "spawned `{program} {}`, a program the pinned base tool set does not hold and blaze does not answer (D192): a machine without it says `NotFound`, but this module's process shim spawned without asking (`found`, guest/script_process.rs), so it was built before that question: rebuild the module",
                args.join(" ")
            ),
            SpawnRefusal::NoToolSet(why) => write!(f, "{why}"),
        }
    }
}

/// A file or folder the script opened.
enum Opened {
    /// A file of the workspace, or a folder's entries and its path from the root (what a path
    /// named from this descriptor is joined to: std's `DirEntry::metadata` names entries from
    /// the folder's descriptor).
    Ro { bytes: Vec<u8>, pos: u64, entries: Option<Vec<(Vec<u8>, Filetype)>>, folder: Vec<u8> },
    /// A file of `OUT_DIR`, by its path there.
    Out { path: String, pos: u64, append: bool, write: bool },
}

/// `OUT_DIR`'s contents by path from it (`None` a folder, else a file's bytes, the root at the
/// empty path), with a count of its edits. Read through `Deref`; changed only through
/// [`OutDir::edit`], so a deferred request's state ([`ScriptSandbox::snap_out`]) is taken again
/// exactly when `OUT_DIR` may differ from the last one taken.
pub(crate) struct OutDir {
    map: BTreeMap<String, Option<Vec<u8>>>,
    edits: u64,
}

impl OutDir {
    fn new() -> OutDir {
        let mut map = BTreeMap::new();
        map.insert(String::new(), None);
        OutDir { map, edits: 0 }
    }

    /// The contents to change: counted as an edit.
    pub(crate) fn edit(&mut self) -> &mut BTreeMap<String, Option<Vec<u8>>> {
        self.edits += 1;
        &mut self.map
    }

    pub(crate) fn edits(&self) -> u64 {
        self.edits
    }
}

impl core::ops::Deref for OutDir {
    type Target = BTreeMap<String, Option<Vec<u8>>>;

    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

/// One `OUT_DIR` state a deferred request saw: each entry's name and, for a file, its digest
/// (blaze-build-script outputs.rs `snap_digest`).
pub(crate) type OutState = Vec<(String, Option<[u8; 32]>)>;

/// One run's sandbox.
pub struct ScriptSandbox {
    pub(crate) script: Arc<Script>,
    opened: Vec<Option<Opened>>,
    pub(crate) out: OutDir,
    pub(crate) out_bytes: u64,
    pub(crate) stdout: Vec<u8>,
    line: u64,
    pub(crate) stderr: Vec<u8>,
    environ: Vec<u8>,
    pub(crate) reads: Vec<Read>,
    pub(crate) refused: Option<Refusal>,
    /// `proc_exit`'s code, when it exited.
    pub(crate) exit: Option<u32>,
    /// The C requests deferred, each argv joined by NUL, with the `OUT_DIR` state it saw (an
    /// index of `c_states`).
    pub(crate) c_requests: Vec<(String, u32)>,
    /// Each deferred process-tree request (c_tasks.rs `is_tree`: `sh configure`, `make`): its
    /// index in `c_requests`, the guest folder it was spawned in, and the environment the
    /// child was given (spawn_route.rs `child_env`, sorted by name, each name once).
    pub(crate) c_trees: Vec<(u32, String, Vec<(String, String)>)>,
    /// The `OUT_DIR` states the deferred requests saw, and the bytes of their files by digest
    /// (blaze-build-script outputs.rs, THE C INPUTS).
    pub(crate) c_states: Vec<OutState>,
    pub(crate) c_held: BTreeMap<[u8; 32], Vec<u8>>,
    c_held_bytes: u64,
    /// `OUT_DIR`'s edit count when the last state was taken, and that state.
    c_snapped: Option<(u64, u32)>,
    /// The last spawn's reply, read by the shim's `reply`.
    pub(crate) reply: Vec<u8>,
    pub(crate) spawns: u32,
}

fn digest(bytes: &[u8]) -> ContentId {
    ContentId::of("blaze build-script read", bytes)
}

fn listing_digest(entries: &[(Vec<u8>, Filetype)]) -> ContentId {
    listing_answer(entries.iter().map(|(name, kind)| (name.as_slice(), *kind == Filetype::Directory)))
}

fn stat_digest(filetype: Filetype, size: u64) -> ContentId {
    let mut bytes = vec![filetype as u8];
    bytes.extend_from_slice(&size.to_le_bytes());
    digest(&bytes)
}

/// The answer a run records for a `file:` read of a file holding `bytes`: what the checkout's
/// sweep computes for the same path (blaze-cli engine_script_reads.rs), so one cell holds one
/// digest whoever made it (rule 21).
pub fn file_answer(bytes: &[u8]) -> ContentId {
    digest(bytes)
}

/// The answer a run records for a `stat:` read: a folder's is its kind alone (the [`Filestat`]
/// handed out says size 0 and no time, inode or count, so nothing inside it is observed); a
/// file's is its kind and size.
pub fn stat_answer(is_dir: bool, size: u64) -> ContentId {
    match is_dir {
        true => stat_digest(Filetype::Directory, 0),
        false => stat_digest(Filetype::RegularFile, size),
    }
}

/// The answer a run records for a `dir:` read of a folder whose entries are `entries` (name,
/// whether it is a folder through any link), in name order: [`listing_digest`]'s bytes, one
/// level, each entry not a folder counted a regular file as [`Tree::node`] lists it.
pub fn listing_answer<'e>(entries: impl IntoIterator<Item = (&'e [u8], bool)>) -> ContentId {
    let mut names = blaze_ids::Hasher::new("blaze build-script read");
    for (name, is_dir) in entries {
        let kind = if is_dir { Filetype::Directory } else { Filetype::RegularFile };
        names.update(name).update(&[0, kind as u8]);
    }
    names.finish()
}

fn filestat(filetype: Filetype, size: u64) -> Filestat {
    Filestat { dev: 0, ino: 0, filetype, nlink: 1, size, atim: 0, mtim: 0, ctim: 0 }
}

/// Why a read's subject has no answer now.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnknownSubject(pub String);

/// What the read `subject` is answered by now in `tree`: the answer a run would record.
pub fn answer_now(tree: &Tree, subject: &str) -> Result<Option<ContentId>, UnknownSubject> {
    let unknown = || UnknownSubject(subject.to_owned());
    let (kind, rel) = subject.split_once(':').ok_or_else(unknown)?;
    if matches!(kind, "git" | "git-scope") {
        let (folder, command) = match kind {
            "git" => {
                let (folder, command) = rel.split_once('\n').ok_or_else(unknown)?;
                (folder, Some(command))
            }
            _ => (rel, None),
        };
        let words: Vec<&str> = command.into_iter().flat_map(|words| words.split(' ')).collect();
        let scratch = blaze_own::Erased::new(words.allocator());
        return Ok(match command {
            Some(_) => git_answer(tree, folder, &words, scratch.handle()).ok(),
            None => Some(scope_answer(status_scope(tree, folder, scratch.handle()))),
        });
    }
    let op = match kind {
        "file" => TreeRead::File,
        "dir" => TreeRead::Listing,
        "stat" => TreeRead::Stat,
        _ => return Err(unknown()),
    };
    op.answer(tree, rel)
}

/// Where a path the script named is.
enum Place {
    /// In the workspace: the path from its root.
    Work(String),
    /// In `OUT_DIR`: the path from it, empty for `OUT_DIR` itself.
    Out(String),
    /// An ancestor of `OUT_DIR` (`/`, `/target`, ...): its one entry.
    Above(Vec<u8>),
    /// Under a `links` dependency's read-only mount: its index in `Script::deps` and the path
    /// from the mount (brief S1).
    Dep { mount: usize, rel: String },
    /// An ancestor of a dependency's mount (and maybe of `OUT_DIR`): its entries, sorted.
    Ancestor(Vec<(Vec<u8>, Filetype)>),
}

/// `a/./b/../c` from `/`, as parts; `None` when it climbs above `/`.
fn parts(path: &str) -> Option<Vec<&str>> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts)
}

impl ScriptSandbox {
    pub fn new(script: Arc<Script>) -> ScriptSandbox {
        let mut environ = Vec::new();
        for (name, value) in &script.env {
            environ.extend_from_slice(name.as_bytes());
            environ.push(b'=');
            environ.extend_from_slice(value.as_bytes());
            environ.push(0);
        }
        ScriptSandbox {
            script,
            opened: Vec::new(),
            out: OutDir::new(),
            out_bytes: 0,
            stdout: Vec::new(),
            line: 0,
            stderr: Vec::new(),
            environ,
            reads: Vec::new(),
            refused: None,
            exit: None,
            c_requests: Vec::new(),
            c_trees: Vec::new(),
            c_states: Vec::new(),
            c_held: BTreeMap::new(),
            c_held_bytes: 0,
            c_snapped: None,
            reply: Vec::new(),
            spawns: 0,
        }
    }

    pub fn script(&self) -> &Script {
        &self.script
    }

    /// The files the script left in `OUT_DIR`, by path from it, sorted.
    pub fn out_files(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.out.iter().filter_map(|(path, bytes)| bytes.as_deref().map(|b| (path.as_str(), b)))
    }

    /// `OUT_DIR` as it is now, as a deferred request's state: the last state again while
    /// `OUT_DIR` was not edited since it was taken, or when it holds the same; else a new one,
    /// each file's bytes kept once by digest. A request runs LATER what the script asked NOW,
    /// and the script may remove or rewrite what it read (outputs.rs, THE C INPUTS).
    /// The bytes held are capped at `OUT_DIR`'s own byte limit (rule 18): past it the script
    /// is refused by name.
    pub(crate) fn snap_out(&mut self) -> Result<u32, Stop> {
        if let Some((edits, state)) = self.c_snapped
            && edits == self.out.edits()
        {
            return Ok(state);
        }
        let limit = self.script.limits.out_dir_bytes;
        let mut entries: OutState = Vec::with_capacity(self.out.len());
        let mut over = None;
        for (name, bytes) in self.out.iter() {
            if name.is_empty() {
                continue;
            }
            match bytes {
                None => entries.push((name.clone(), None)),
                Some(bytes) => {
                    let digest = blaze_build_script::outputs::snap_digest(bytes);
                    if !self.c_held.contains_key(&digest) {
                        self.c_held_bytes += bytes.len() as u64;
                        if self.c_held_bytes > limit {
                            over = Some(name.as_str());
                            break;
                        }
                        self.c_held.insert(digest, bytes.clone());
                    }
                    entries.push((name.clone(), Some(digest)));
                }
            }
        }
        if let Some(name) = over {
            let msg = format!(
                "deferred C requests whose OUT_DIR inputs, kept as each request saw them, pass {limit} bytes (at {name})"
            );
            return Err(self.refuse(Refusal::OutDir(msg)));
        }
        let state = match self.c_states.last() {
            Some(last) if *last == entries => self.c_states.len() - 1,
            _ => {
                self.c_states.push(entries);
                self.c_states.len() - 1
            }
        };
        let state = u32::try_from(state).expect("snap_out: fewer states than spawns, and spawns are counted in u32");
        self.c_snapped = Some((self.out.edits(), state));
        Ok(state)
    }

    pub(crate) fn record(&mut self, mut subject: String, digest: Option<ContentId>) {
        if self.reads.len() > READS_CAP {
            return; // the overflow witness already refuses this whole run
        }
        if let Some(previous) = self.reads.iter().find(|it| it.subject == subject) {
            if previous.digest == digest {
                return;
            }
            // Both answers contributed to one run. A single cell can never
            // prove both. Keep the conflicting subject for the engine's note.
            subject.insert_str(0, "changed-during-run:");
            if self.reads.iter().any(|it| it.subject == subject) {
                return;
            }
        }
        if self.reads.len() == READS_CAP {
            subject.insert_str(0, "read-limit:");
        }
        self.reads.push(Read { subject, digest });
    }

    fn record_node(&mut self, mut rel: String, op: NodeRead, found: Option<&Node<'_>>) {
        let (prefix, answer) = op.observation(found);
        rel.insert_str(0, prefix);
        self.record(rel, answer);
    }

    pub(crate) fn refuse(&mut self, refusal: Refusal) -> Stop {
        let why = match &refusal {
            Refusal::Path(why) | Refusal::OutDir(why) | Refusal::Output(why) => why.clone(),
            Refusal::Spawn(spawn) => spawn.to_string(),
            Refusal::Wait => "waited (`poll_oneoff`, as `thread::sleep` does), which a build script may not".to_owned(),
        };
        self.refused.get_or_insert(refusal);
        Stop(why)
    }

    /// Where `path` (from `/`, as wasi-libc passes it to the root's descriptor) is.
    fn place(&mut self, path: &[u8]) -> Result<Place, Stop> {
        let Ok(text) = std::str::from_utf8(path) else {
            return Err(self.refuse(Refusal::Path(format!("named the path {:?}, which is not UTF-8", String::from_utf8_lossy(path)))));
        };
        let Some(parts) = parts(text) else {
            return Err(self.refuse(Refusal::Path(format!("named /{text}, which climbs above the root"))));
        };
        let mount = self.script.root.seen().mount();
        if parts.first().is_some_and(|first| mount.strip_prefix('/') == Some(*first)) {
            return Ok(Place::Work(parts[1..].join("/")));
        }
        let out_dir = self.script.out_dir().to_owned();
        let out: Vec<&str> = out_dir.split('/').filter(|it| !it.is_empty()).collect();
        if parts.len() >= out.len() && parts[..out.len()] == out[..] {
            return Ok(Place::Out(parts[out.len()..].join("/")));
        }
        if let Some(place) = self.script.deps.place(&parts, &out) {
            return Ok(place);
        }
        if parts.len() < out.len() && out[..parts.len()] == parts[..] {
            return Ok(Place::Above(out[parts.len()].as_bytes().to_vec()));
        }
        Err(self.refuse(Refusal::Path(format!("named /{}, outside {mount} and OUT_DIR ({out_dir})", parts.join("/")))))
    }

    /// `path` named from the descriptor `at`, as a path from the root: the root's descriptor
    /// (wasi-libc passes every absolute path from it) or an open folder's (std names a
    /// `read_dir` entry from its folder's). Any other descriptor is not a folder: ENOTDIR.
    fn from(&self, at: u32, path: &[u8]) -> Result<Vec<u8>, Errno> {
        if at == ROOT_FD {
            return Ok(path.to_vec());
        }
        let opened = at.checked_sub(ROOT_FD + 1).and_then(|at| self.opened.get(at as usize)).and_then(Option::as_ref);
        match opened {
            Some(Opened::Ro { entries: Some(_), folder, .. }) => {
                let mut joined = Vec::with_capacity(folder.len() + 1 + path.len());
                joined.extend_from_slice(folder);
                joined.push(b'/');
                joined.extend_from_slice(path);
                Ok(joined)
            }
            Some(_) => Err(Errno::NOTDIR),
            None => Err(Errno::BADF),
        }
    }

    fn slot(&mut self, opened: Opened) -> u32 {
        let slot = match self.opened.iter().position(Option::is_none) {
            Some(slot) => slot,
            None => {
                self.opened.push(None);
                self.opened.len() - 1
            }
        };
        self.opened[slot] = Some(opened);
        ROOT_FD + 1 + slot as u32
    }

    fn opened(&mut self, fd: u32) -> Option<&mut Opened> {
        fd.checked_sub(ROOT_FD + 1).and_then(|at| self.opened.get_mut(at as usize)).and_then(Option::as_mut)
    }

    /// `OUT_DIR`'s entries under `dir`, sorted.
    fn out_listing(&self, dir: &str) -> Vec<(Vec<u8>, Filetype)> {
        let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        self.out
            .iter()
            .filter(|(path, _)| !path.is_empty() && path.starts_with(&prefix) && !path[prefix.len()..].contains('/'))
            .map(|(path, bytes)| (path[prefix.len()..].as_bytes().to_vec(), if bytes.is_some() { Filetype::RegularFile } else { Filetype::Directory }))
            .collect()
    }

    fn out_parent_exists(&self, path: &str) -> bool {
        let parent = path.rsplit_once('/').map_or("", |(dir, _)| dir);
        matches!(self.out.get(parent), Some(None))
    }

    fn work_write(&mut self, rel: &str) -> Stop {
        let mount = self.script.root.seen().mount();
        self.refuse(Refusal::Path(format!("wrote {mount}/{rel}, outside OUT_DIR, the only folder a build script may write")))
    }

    /// `path_create_directory`.
    pub fn create_dir(&mut self, at: u32, path: &[u8]) -> Answer<()> {
        let path = match self.from(at, path) {
            Ok(path) => path,
            Err(errno) => return Ok(Err(errno)),
        };
        match self.place(&path)? {
            Place::Out(rel) if rel.is_empty() => Ok(Err(Errno::EXIST)),
            Place::Out(rel) => {
                if self.out.contains_key(&rel) {
                    return Ok(Err(Errno::EXIST));
                }
                if !self.out_parent_exists(&rel) {
                    return Ok(Err(Errno::NOENT));
                }
                self.count_file(&rel)?;
                self.out.edit().insert(rel, None);
                Ok(Ok(()))
            }
            Place::Above(_) | Place::Ancestor(_) => Ok(Err(Errno::EXIST)),
            Place::Work(rel) => Err(self.work_write(&rel)),
            Place::Dep { mount, rel } => Err(self.dep_write(mount, &rel)),
        }
    }

    /// `path_unlink_file` (`dir: false`) and `path_remove_directory` (`dir: true`).
    pub fn remove(&mut self, at: u32, path: &[u8], dir: bool) -> Answer<()> {
        let path = match self.from(at, path) {
            Ok(path) => path,
            Err(errno) => return Ok(Err(errno)),
        };
        match self.place(&path)? {
            Place::Out(rel) if !rel.is_empty() => match self.out.get(&rel) {
                None => Ok(Err(Errno::NOENT)),
                Some(Some(_)) if dir => Ok(Err(Errno::NOTDIR)),
                Some(None) if !dir => Ok(Err(Errno::ISDIR)),
                Some(None) if !self.out_listing(&rel).is_empty() => Ok(Err(Errno::NOTEMPTY)),
                Some(_) => {
                    if let Some(Some(bytes)) = self.out.edit().remove(&rel) {
                        self.out_bytes -= bytes.len() as u64;
                    }
                    Ok(Ok(()))
                }
            },
            Place::Work(rel) => Err(self.work_write(&rel)),
            Place::Dep { mount, rel } => Err(self.dep_write(mount, &rel)),
            _ => Ok(Err(Errno::PERM)),
        }
    }

    /// `path_rename`, within `OUT_DIR`.
    pub fn rename(&mut self, at: u32, from: &[u8], to_at: u32, to: &[u8]) -> Answer<()> {
        let (from, to) = match (self.from(at, from), self.from(to_at, to)) {
            (Ok(from), Ok(to)) => (from, to),
            (Err(errno), _) | (_, Err(errno)) => return Ok(Err(errno)),
        };
        let (from, to) = match (self.place(&from)?, self.place(&to)?) {
            (Place::Out(from), Place::Out(to)) if !from.is_empty() && !to.is_empty() => (from, to),
            (Place::Work(rel), _) | (_, Place::Work(rel)) => return Err(self.work_write(&rel)),
            (Place::Dep { mount, rel }, _) | (_, Place::Dep { mount, rel }) => return Err(self.dep_write(mount, &rel)),
            _ => return Ok(Err(Errno::PERM)),
        };
        let Some(Some(_)) = self.out.get(&from) else {
            // A folder is not renamed: no script of tempo's renames one.
            return Ok(Err(if self.out.contains_key(&from) { Errno::NOTSUP } else { Errno::NOENT }));
        };
        if !self.out_parent_exists(&to) || matches!(self.out.get(&to), Some(None)) {
            return Ok(Err(Errno::NOENT));
        }
        let bytes = self.out.edit().remove(&from).flatten().unwrap_or_default();
        if let Some(Some(old)) = self.out.edit().insert(to, Some(bytes)) {
            self.out_bytes -= old.len() as u64;
        }
        Ok(Ok(()))
    }

    /// `fd_filestat_set_size` of a file of `OUT_DIR`.
    pub fn set_size(&mut self, fd: u32, size: u64) -> Answer<()> {
        let path = match self.opened(fd) {
            Some(Opened::Out { path, write: true, .. }) => path.clone(),
            Some(_) => return Ok(Err(Errno::BADF)),
            None => return Ok(Err(Errno::BADF)),
        };
        let now = self.out.get(&path).and_then(|it| it.as_ref()).map_or(0, |b| b.len() as u64);
        self.grow(size.saturating_sub(now))?;
        if let Some(Some(bytes)) = self.out.edit().get_mut(&path) {
            bytes.resize(size as usize, 0);
        }
        self.out_bytes = self.out_bytes - now.min(self.out_bytes) + size.min(now) + size.saturating_sub(now);
        Ok(Ok(()))
    }

    pub(crate) fn count_file(&mut self, rel: &str) -> Result<(), Stop> {
        let limit = self.script.limits.out_dir_files;
        if self.out.len() > limit as usize {
            return Err(self.refuse(Refusal::OutDir(format!("made {rel} in OUT_DIR, past its limit of {limit} files and folders"))));
        }
        Ok(())
    }

    /// Admits `more` bytes into `OUT_DIR`.
    pub(crate) fn grow(&mut self, more: u64) -> Result<(), Stop> {
        let limit = self.script.limits.out_dir_bytes;
        if self.out_bytes.saturating_add(more) > limit {
            return Err(self.refuse(Refusal::OutDir(format!("wrote more than {limit} bytes into OUT_DIR"))));
        }
        Ok(())
    }

    pub(crate) fn stdout_write(&mut self, bytes: &[u8]) -> Result<(), Stop> {
        let limits = self.script.limits;
        if self.stdout.len() as u64 + bytes.len() as u64 > limits.output {
            return Err(self.refuse(Refusal::Output(format!("printed more than {} bytes", limits.output))));
        }
        for &byte in bytes {
            if byte == b'\n' {
                self.line = 0;
            } else {
                self.line += 1;
                if self.line > limits.line {
                    return Err(self.refuse(Refusal::Output(format!("printed a line longer than {} bytes", limits.line))));
                }
            }
        }
        self.stdout.extend_from_slice(bytes);
        Ok(())
    }

    /// A file's bytes for a spawn's snippet: `path` as the script named it, from `cwd`.
    pub(crate) fn snippet_file(&mut self, cwd: &str, path: &str) -> Result<Option<Vec<u8>>, Stop> {
        let full = if path.starts_with('/') { path.to_owned() } else { format!("{cwd}/{path}") };
        Ok(match self.place(full.as_bytes())? {
            Place::Out(rel) => self.out.get(&rel).cloned().flatten(),
            Place::Work(rel) => {
                let script = Arc::clone(&self.script);
                let found = match script.tree.node(&rel) {
                    Ok(found) => found,
                    Err(why) => return Err(self.refuse(Refusal::Path(why.to_string()))),
                };
                self.record_node(rel, NodeRead::Bytes, found.as_ref());
                match found {
                    Some(Node::File(bytes)) => Some(bytes.into_owned()),
                    _ => None,
                }
            }
            Place::Dep { mount, rel } => self.dep_file(mount, rel)?,
            Place::Above(_) | Place::Ancestor(_) => None,
        })
    }

    /// Whether `program` is one the script's machine has (spawn_route.rs `program_found`, D192),
    /// in a frame of its own; a host with no base tool set stops the script by name.
    fn look_up(&mut self, program: &str) -> Result<bool, Stop> {
        let frame = match blaze_mem::Frame::new(blaze_mem::Tag::Other) {
            Ok(frame) => frame,
            Err(e) => {
                let why = format!("no frame to read pins/base-tools.json in: {e}");
                let no_set = crate::spawn_route::NoToolSet { program: program.to_owned(), host: self.script.host.clone(), why };
                return Err(self.refuse(Refusal::Spawn(SpawnRefusal::NoToolSet(no_set))));
            }
        };
        let erased = blaze_own::Erased::new(&frame);
        match crate::spawn_route::program_found(program, &self.script.host, erased.handle()) {
            Ok(found) => Ok(found),
            Err(no_set) => Err(self.refuse(Refusal::Spawn(SpawnRefusal::NoToolSet(no_set)))),
        }
    }

    /// The shim's `found` question (guest/script_process.rs, asked by `Command::spawn` before
    /// any child exists): whether the machine has `program`. A program it does not have is the
    /// OS's `NotFound` to the script (D192), and the read `absent-program\n<host>\n<name>` is
    /// recorded with its answer, so a later list that holds the program reruns the script
    /// (rule 21; fact_pins.rs `fact_now` answers it again).
    pub fn found(&mut self, program: &[u8]) -> Result<bool, Stop> {
        let Ok(program) = core::str::from_utf8(program) else {
            return Err(self.refuse(Refusal::Spawn(SpawnRefusal::Malformed(Malformed::NotUtf8("the program")))));
        };
        let found = self.look_up(program)?;
        if !found {
            let subject = crate::spawn_route::absent_subject(&self.script.host, program);
            let digest = crate::spawn_route::absent_digest(&self.script.host, program);
            self.record(subject, Some(digest));
        }
        Ok(found)
    }

    /// Answers one spawn the shim made (`guest/script_process.rs`'s encoding): the reply, an
    /// exit code and the two outputs, or the stop of the script.
    pub fn spawn(&mut self, request: &[u8]) -> Result<(), Stop> {
        self.spawns += 1;
        let spawned = match crate::spawn_route::decode_full(request) {
            Ok(spawned) => spawned,
            Err(why) => return Err(self.refuse(Refusal::Spawn(SpawnRefusal::Malformed(why)))),
        };
        let Some((program, args)) = spawned.argv.split_first() else {
            return Err(self.refuse(Refusal::Spawn(SpawnRefusal::Malformed(Malformed::NoProgram))));
        };
        let args_text: Vec<&str> = args.iter().map(String::as_str).collect();
        let owned = || args.to_vec();
        let name = program.rsplit('/').next().unwrap_or(program);
        let found = self.look_up(program)?;
        let routed = crate::spawn_route::route(name, &args_text, self.script.c_mode, found, self.script.spawner.is_some());
        let (code, stdout, stderr): (i32, Vec<u8>, Vec<u8>) = match routed {
            crate::spawn_route::Route::NotFound => {
                return Err(self.refuse(Refusal::Spawn(SpawnRefusal::NotFoundUnasked { program: program.clone(), args: owned() })));
            }
            crate::spawn_route::Route::Rustc if self.script.probes.is_some() => {
                let probes = self.script.probes.as_ref().map(blaze_own::Share::share).expect("the arm's guard: the checker is on");
                self.checked_probe(&probes, &args_text, &spawned, args)?
            }
            crate::spawn_route::Route::Rustc => match probe_ask::classify(&args_text, Alloc::global()) {
                Err(refused) => return Err(self.refuse(Refusal::Spawn(SpawnRefusal::RustcLine { args: owned(), at: refused.at, why: refused.why }))),
                Ok(Ask::Version { verbose }) => (0, probe::version(verbose, &self.script.host, Alloc::global()).as_bytes().to_vec(), Vec::new()),
                // `--print cfg` and the rest: the target's cfg rows are the toolchain record's,
                // not handed to the script host yet; refused by name, never guessed.
                Ok(Ask::Print { .. }) => return Err(self.refuse(Refusal::Spawn(SpawnRefusal::RustcArgs { args: owned() }))),
                Ok(Ask::Compile(compile)) => {
                    let script_cwd;
                    let cwd = match spawned.cwd.as_deref() {
                        Some(cwd) => cwd,
                        None => {
                            script_cwd = self.script.cwd();
                            &script_cwd
                        }
                    };
                    let owned_bytes;
                    let bytes: Option<&[u8]> = match compile.input {
                        Snippet::Stdin => Some(spawned.stdin.as_slice()),
                        Snippet::File(path) => {
                            owned_bytes = self.snippet_file(cwd, path)?;
                            owned_bytes.as_deref()
                        }
                    };
                    let Some(bytes) = bytes else {
                        return Err(self.refuse(Refusal::Spawn(SpawnRefusal::RustcArgs { args: owned() })));
                    };
                    // probe_ask.rs: no output file is written; every probe of tempo's reads
                    // only the status.
                    let answered = probe_ask::answer(&compile, &self.script.host, bytes, &Table { rows: &self.script.table });
                    match answered.verdict {
                        Verdict::Accepted(_) => (0, Vec::new(), Vec::new()),
                        Verdict::Rejected(rejection) => (1, Vec::new(), rejection.stderr(&compile, bytes, Alloc::global()).as_bytes().to_vec()),
                        Verdict::Unanswered(_) => {
                            let snippet = String::from_utf8_lossy(bytes).into_owned();
                            let (key, target) = (answered.key, answered.target.to_owned());
                            return Err(self.refuse(Refusal::Spawn(SpawnRefusal::CannotAnswerYet { key, args: owned(), snippet, target })));
                        }
                    }
                }
            },
            crate::spawn_route::Route::CargoVersion => (0, probe::cargo_version(Alloc::global()).as_bytes().to_vec(), Vec::new()),
            crate::spawn_route::Route::Git => {
                let Some(owned_words) = git_words(name, &args_text) else {
                    // A shell's `-c "git ..."` with quoting, a format or a glob (`log -1
                    // --pretty=format:'%cI'`, `describe --match "v*"`): no row reads it yet.
                    let rest = args_text.get(1).and_then(|command| command.strip_prefix("git ")).unwrap_or("");
                    return Err(self.refuse(Refusal::Spawn(SpawnRefusal::Git { command: rest.to_owned(), why: git::Refused::CannotAnswerYet })));
                };
                let words: Vec<&str> = owned_words.iter().map(String::as_str).collect();
                let command = words.join(" ");
                let folder = match &spawned.cwd {
                    None => Some(self.script.root.seen().folder().to_owned()),
                    Some(cwd) => self.script.root.seen().in_tree(cwd).map(str::to_owned),
                };
                let script = Arc::clone(&self.script);
                let scratch = blaze_own::Erased::new(words.allocator());
                let alloc = scratch.handle();
                let answered = folder.as_deref().ok_or(git::Refused::NoRepository).and_then(|folder| {
                    if folder.contains('\n') || words.iter().any(|word| word.is_empty() || word.contains(char::is_whitespace)) {
                        return Err(git::Refused::CannotAnswerYet);
                    }
                    let status = matches!(git::question(&words), git::Question::Status { .. });
                    let work = if status { status_scope(&script.tree, folder, alloc) } else { folder };
                    let answer = git_fact(&script.tree, work, &words, alloc)?;
                    if status && !folder.is_empty() && matches!(script.tree, Tree::Dir(_)) {
                        let mut binding = folder.to_owned();
                        binding.insert_str(0, "git-scope:");
                        self.record(binding, Some(scope_answer(work)));
                    }
                    Ok((work, answer))
                });
                match answered {
                    Ok((work, answer)) => {
                        self.record(git_subject(work, command), Some(answer_digest(&answer)));
                        (answer.code, answer.stdout.as_bytes().to_vec(), answer.stderr.as_bytes().to_vec())
                    }
                    Err(why) => return Err(self.refuse(Refusal::Spawn(SpawnRefusal::Git { command, why }))),
                }
            }
            crate::spawn_route::Route::Fact(fact) => return self.spawn_fact(&fact, program, args, &spawned),
            crate::spawn_route::Route::Deferred => {
                // Its inputs the script made are captured now, by content: the script may
                // remove or rewrite them before the request runs (outputs.rs, THE C INPUTS).
                let state = self.snap_out()?;
                // Its relative paths made the guest paths they meant in the spawn's folder
                // (c_tasks.rs `rooted`): the request runs later, elsewhere.
                let script_cwd = self.script.cwd();
                let cwd = match spawned.cwd.as_deref() {
                    None => script_cwd,
                    Some(cwd) if cwd.starts_with('/') => cwd.to_owned(),
                    Some(cwd) => format!("{script_cwd}/{cwd}"),
                };
                let argv: Vec<&str> = spawned.argv.iter().map(String::as_str).collect();
                let mut text = String::new();
                blaze_build_script::c_tasks::rooted(&argv, &cwd, &mut text).expect("script_sandbox: writing to a String does not fail");
                // A process tree runs later with the folder and environment it was spawned
                // with (outputs.rs `CTree`): `configure` reads CC, CFLAGS and the rest.
                if blaze_build_script::c_tasks::is_tree(program) {
                    let env = crate::spawn_route::child_env(&self.script.env, &spawned);
                    self.c_trees.push((self.c_requests.len() as u32, cwd, env));
                }
                self.c_requests.push((text, state));
                (0, Vec::new(), Vec::new())
            }
            crate::spawn_route::Route::Executor => return self.spawn_executor(program, args, &spawned),
            crate::spawn_route::Route::Refused => {
                let why = self.script.spawn_setting.no_sandbox();
                return Err(self.refuse(Refusal::Spawn(SpawnRefusal::NoSandbox { program: program.clone(), args: owned(), why })));
            }
        };
        self.set_reply(code, &stdout, &stderr);
        Ok(())
    }
}

/// `git`'s arguments when the spawn is `git ...` or a shell's `-c "git ..."` (how smolgen and
/// vergen call it) with one plain git command; `None` for anything else.
pub(crate) fn git_words(name: &str, args: &[&str]) -> Option<Vec<String>> {
    if name == "git" {
        return Some(args.iter().map(|arg| (*arg).to_owned()).collect());
    }
    match args {
        ["-c", command] if matches!(name, "sh" | "bash") => {
            git::shell_git(command, Alloc::global()).map(|words| words.iter().map(|word| word.as_str().to_owned()).collect())
        }
        _ => None,
    }
}

/// D54: a `git` answer is a derived fact. The run's record lists the question
/// ([`git_subject`]) and the digest of its answer ([`answer_digest`]), not the `.git` files read
/// to make it; a later command recomputes the answer ([`answer_now`]) and reruns the script
/// only when it changed, so an edit that leaves the answer alone reruns nothing (early cutoff).
fn git_fact<'a>(tree: &Tree, folder: &str, words: &[&str], alloc: Alloc<'a>) -> Result<git::Answer<'a>, git::Refused> {
    git_question(tree, folder, git::question(words), alloc).map(|(answer, _)| answer)
}

/// `question` asked in `folder` on the tree granted to the script, through the one reader a
/// run's spawn reads it by ([`GitView`]), with what it listed and read of the ignore files.
pub fn git_question<'a>(tree: &Tree, folder: &str, question: git::Question, alloc: Alloc<'a>) -> Result<(git::Answer<'a>, GitCounts), git::Refused> {
    let mut view = GitView::for_fact(tree, alloc);
    let answer = git::answer(question, folder, tree.vcs(), &mut view, alloc);
    match view.fault {
        Some(why) => Err(why),
        None => answer.map(|answer| (answer, view.counts)),
    }
}

/// `status --porcelain` in `work` (a worktree, [`status_scope`]) after a change, on the tree
/// granted to the script, through the same reader as [`git_question`] (E3, blaze-build-script
/// git.rs `status_after`): byte-equal to its answer; the answer, how it was made, and what it
/// listed and read of the ignore files.
#[allow(clippy::too_many_arguments, reason = "the tree, the question, the previous answer and its proof, the place now, the allocator and the plant")]
pub fn status_after<'a>(
    tree: &Tree,
    work: &str,
    untracked: bool,
    all: bool,
    previous: Option<&git::StatusAnswer<'_>>,
    covered: Option<&git::CoveredSince<'_>>,
    now: git::Gen,
    alloc: Alloc<'a>,
    plant: impl blaze_own::planted::Planted<git::StatusPlant>,
) -> Result<(git::After<'a>, GitCounts), git::Refused> {
    // A fetched source has no `.git` and no journal: its status is its commit's, answered by
    // [`git_question`] alone. The checkout sweep serves only a checkout.
    if let git::Vcs::Fetched(_) = tree.vcs() {
        return Err(git::Refused::CannotAnswerYet);
    }
    let mut view = GitView::for_fact(tree, alloc);
    let after = git::status_after(work, untracked, all, previous, covered, now, &mut view, alloc, plant);
    match view.fault {
        Some(why) => Err(why),
        None => after.map(|after| (after, view.counts)),
    }
}

/// Replay on the very tree granted to the script. In particular status sees the
/// same lstat fields, link targets, config, index and listing as the spawn.
pub fn git_answer(tree: &Tree, folder: &str, words: &[&str], alloc: Alloc<'_>) -> Result<ContentId, git::Refused> {
    git_fact(tree, folder, words, alloc).map(|answer| answer_digest(&answer))
}

/// The nearest marker consumed by git.rs `find`, relative to the granted tree.
/// A .git directory without a readable HEAD does not bind the question.
pub fn status_scope<'f>(tree: &Tree, mut folder: &'f str, alloc: Alloc<'_>) -> &'f str {
    let mut marker = blaze_own::String::new_in(alloc);
    while !folder.is_empty() {
        marker.clear();
        marker.push_str(folder);
        marker.push_str("/.git");
        if !matches!(tree.file_bytes(&marker), Ok(None)) {
            break;
        }
        marker.push_str("/HEAD");
        if !matches!(tree.file_bytes(&marker), Ok(None)) {
            break;
        }
        folder = folder.rsplit_once('/').map_or("", |(parent, _)| parent);
    }
    folder
}

pub fn scope_answer(work: &str) -> ContentId {
    ContentId::of("blaze build-script git worktree 1", work.as_bytes())
}

/// The validated words as a read's subject, using the command's existing buffer.
fn git_subject(folder: &str, mut command: String) -> String {
    command.reserve("git:\n".len() + folder.len());
    command.insert(0, '\n');
    command.insert_str(0, folder);
    command.insert_str(0, "git:");
    command
}

/// A git answer's digest as a run's read records it: the digest of
/// [`git::Answer::digest_bytes`], whatever made the answer.
pub fn answer_digest(answer: &git::Answer<'_>) -> ContentId {
    let mut bytes = blaze_own::Vec::new_in(*answer.stdout.allocator());
    answer.digest_bytes(&mut bytes);
    digest(&bytes)
}

/// The served tree as `git` reads it: every file asked for is a tracked read (`file:<path>`),
/// an absent one too, so a new commit reruns exactly the scripts that asked.
///
/// A path is reached without resolving the root or any link: `rel` must be plain, and every
/// folder on its way is proven a real folder by one `lstat`, remembered for the view's life, so
/// `status` over n index entries in f folders costs n + f `lstat`s and nothing else. A link
/// standing for a folder makes every path under it absent, as git's
/// `has_symlink_leading_path` does (git reports those entries ` D`): no stat, list or read can
/// pass through a link, so none can reach outside the tree. The last part is never followed.
pub(crate) struct GitView<'s> {
    tree: &'s Tree,
    reads: Option<&'s mut Vec<Read>>,
    /// Folders (relative to the root) proven real by `lstat`; sorted by bytes.
    real_folders: blaze_own::AVec<'s, blaze_own::AString<'s>>,
    fault: Option<git::Refused>,
    counts: GitCounts,
}

/// What one git answer asked of the tree that grows with the checkout rather than with the
/// change: the folders listed and the ignore files read (`.gitignore`, `info/exclude`). A
/// status after a change to tracked files asks neither (git_objects.rs `status_after`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct GitCounts {
    pub lists: u32,
    pub ignore_reads: u32,
}

impl GitCounts {
    pub fn add(&mut self, other: GitCounts) {
        self.lists = self.lists.saturating_add(other.lists);
        self.ignore_reads = self.ignore_reads.saturating_add(other.ignore_reads);
    }
}

impl<'s> GitView<'s> {
    #[cfg(test)]
    pub(crate) fn new(tree: &'s Tree, reads: &'s mut Vec<Read>, alloc: Alloc<'s>) -> Self {
        GitView { reads: Some(reads), ..Self::for_fact(tree, alloc) }
    }

    fn for_fact(tree: &'s Tree, alloc: Alloc<'s>) -> Self {
        GitView { tree, reads: None, real_folders: blaze_own::Vec::new_in(alloc), fault: None, counts: GitCounts::default() }
    }

    /// A read of `rel` counted when it is an ignore file.
    fn count_read(&mut self, rel: &str) {
        if rel.rsplit('/').next() == Some(".gitignore") || rel.ends_with("info/exclude") {
            self.counts.ignore_reads = self.counts.ignore_reads.saturating_add(1);
        }
    }

    /// `root/rel` when `rel` is plain (no empty, `.` or `..` part) and each folder before its
    /// last part is a real folder, not a link (one `lstat` per folder, once per view); `None`
    /// otherwise, which callers read as absent.
    fn inside(&mut self, root: &Path, rel: &str) -> Option<PathBuf> {
        match self.reach(root, rel) {
            Reach::At(path) => Some(path),
            Reach::Absent | Reach::ThroughLink => None,
        }
    }

    /// [`Self::inside`], telling a path absent from one behind a link or a non-folder.
    fn reach(&mut self, root: &Path, rel: &str) -> Reach {
        if rel.split('/').any(|part| matches!(part, "" | "." | "..")) {
            return Reach::Absent;
        }
        for (end, _) in rel.match_indices('/') {
            let folder = &rel[..end];
            let Err(at) = self.real_folders.binary_search_by(|known| known.as_bytes().cmp(folder.as_bytes())) else { continue };
            match std::fs::symlink_metadata(root.join(folder)) {
                Ok(meta) if meta.is_dir() => {}
                Ok(meta) if meta.file_type().is_symlink() => return Reach::ThroughLink,
                Ok(_) => return Reach::Absent,
                Err(error) if missing(&error) => return Reach::Absent,
                Err(_) => {
                    self.fault.get_or_insert(git::Refused::CannotAnswerYet);
                    return Reach::Absent;
                }
            }
            let alloc = *self.real_folders.allocator();
            self.real_folders.insert(at, blaze_own::String::from_str_in(folder, alloc));
        }
        Reach::At(root.join(rel))
    }
}

impl GitView<'_> {
    /// Notes the read of `rel` (`None`: absent) when the view keeps reads; whether it was found.
    fn noted(&mut self, rel: &str, bytes: Option<&[u8]>) -> bool {
        if let Some(reads) = &mut self.reads {
            let subject = ["file:", rel].concat();
            if !reads.iter().any(|it| it.subject == subject) {
                reads.push(Read { subject, digest: bytes.map(digest) });
            }
        }
        bytes.is_some()
    }
}

/// Where a plain path of a served tree is, as [`GitView`] reaches it without following links.
enum Reach {
    At(PathBuf),
    Absent,
    /// A folder on the way is a link.
    ThroughLink,
}

impl GitFiles for GitView<'_> {
    /// A served tree's file is reached as [`Self::stat`] reaches it: each folder on the way
    /// proven real by one `lstat` per view, the file itself one `lstat` and one read; no
    /// `realpath`, no `getcwd` (tempo's edit: this read through [`disk_node`] was a third of the
    /// command's opens). A link on the way, or as the file, is refused, never followed: git
    /// reads its own files and an ignore or attributes file through no link either, and a link
    /// out of the tree was refused before as well.
    fn read(&mut self, rel: &str, into: &mut blaze_own::AVec<'_, u8>) -> bool {
        self.count_read(rel);
        let tree: &Tree = self.tree;
        if let Tree::Dir(root) = tree {
            let path = match self.reach(root, rel) {
                Reach::At(path) => path,
                Reach::Absent => return self.noted(rel, None),
                Reach::ThroughLink => {
                    self.fault.get_or_insert(git::Refused::CannotAnswerYet);
                    return false;
                }
            };
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_file() => {}
                Ok(meta) if meta.is_dir() => return self.noted(rel, None),
                Ok(_) => {
                    self.fault.get_or_insert(git::Refused::CannotAnswerYet);
                    return false;
                }
                Err(error) if missing(&error) => return self.noted(rel, None),
                Err(_) => {
                    self.fault.get_or_insert(git::Refused::CannotAnswerYet);
                    return false;
                }
            }
            let start = into.len();
            use std::io::Read as _;
            let read = std::fs::File::open(&path).and_then(|mut file| {
                let mut chunk = [0u8; 16 << 10];
                loop {
                    let n = file.read(&mut chunk)?;
                    if n == 0 {
                        return Ok(());
                    }
                    if into.try_reserve(n).is_err() {
                        return Err(std::io::Error::from(std::io::ErrorKind::OutOfMemory));
                    }
                    into.extend_from_slice(&chunk[..n]);
                }
            });
            if read.is_err() {
                into.truncate(start);
                self.fault.get_or_insert(git::Refused::CannotAnswerYet);
                return false;
            }
            return self.noted(rel, Some(&into[start..]));
        }
        let bytes = match tree.file_bytes(rel) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.fault.get_or_insert(git::Refused::CannotAnswerYet);
                return false;
            }
        };
        self.noted(rel, bytes.as_deref());
        let Some(bytes) = bytes else { return false };
        into.extend_from_slice(&bytes);
        true
    }

    fn read_range(&mut self, rel: &str, at: u64, len: u64, into: &mut blaze_own::AVec<'_, u8>) -> bool {
        let Ok(len) = usize::try_from(len) else { return false };
        let tree: &Tree = self.tree;
        match tree {
            Tree::Dir(root) => {
                use std::os::unix::fs::FileExt;
                let Some(path) = self.inside(root, rel) else { return false };
                // The last part is not followed either: a link here is not a file.
                if !std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_file()) {
                    return false;
                }
                let Ok(file) = std::fs::File::open(path) else { return false };
                let start = into.len();
                if into.try_reserve(len).is_err() {
                    return false;
                }
                into.resize(start + len, 0);
                if file.read_exact_at(&mut into[start..], at).is_err() {
                    into.truncate(start);
                    return false;
                }
                true
            }
            Tree::Mem { .. } => {
                let Some(bytes) = self.tree.file(rel) else { return false };
                let Some(range) = usize::try_from(at).ok().and_then(|at| bytes.get(at..at.checked_add(len)?)) else { return false };
                into.extend_from_slice(range);
                true
            }
        }
    }

    fn stat(&mut self, rel: &str) -> Option<git::FileStat> {
        let tree: &Tree = self.tree;
        match tree {
            Tree::Dir(root) => {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                let meta = match std::fs::symlink_metadata(self.inside(root, rel)?) {
                    Ok(meta) => meta,
                    Err(error) if missing(&error) => return None,
                    Err(_) => {
                        self.fault.get_or_insert(git::Refused::CannotAnswerYet);
                        return None;
                    }
                };
                let kind = match meta.file_type() {
                    t if t.is_symlink() => git::FileKind::Symlink,
                    t if t.is_dir() => git::FileKind::Dir,
                    t if t.is_file() => git::FileKind::File,
                    _ => git::FileKind::Other,
                };
                Some(git::FileStat {
                    kind,
                    size: meta.len(),
                    mtime: (meta.mtime(), meta.mtime_nsec() as u32),
                    ctime: (meta.ctime(), meta.ctime_nsec() as u32),
                    ino: meta.ino(),
                    uid: meta.uid(),
                    gid: meta.gid(),
                    executable: meta.permissions().mode() & 0o100 != 0,
                })
            }
            Tree::Mem { .. } => match self.tree.node(rel) {
                Ok(Some(Node::File(bytes))) => Some(git::FileStat { kind: git::FileKind::File, size: bytes.len() as u64, mtime: (0, 0), ctime: (0, 0), ino: 0, uid: 0, gid: 0, executable: false }),
                Ok(Some(Node::Folder(_))) => Some(git::FileStat { kind: git::FileKind::Dir, size: 0, mtime: (0, 0), ctime: (0, 0), ino: 0, uid: 0, gid: 0, executable: false }),
                _ => None,
            },
        }
    }

    fn list<'a>(&mut self, rel: &str, alloc: Alloc<'a>, into: &mut blaze_own::AVec<'a, blaze_own::AString<'a>>) {
        self.counts.lists = self.counts.lists.saturating_add(1);
        let tree: &Tree = self.tree;
        if let Tree::Dir(root) = tree {
            // Names only: no stat per entry. The folder itself must be real, not a link.
            let child;
            let path = if rel.is_empty() {
                root.path()
            } else {
                let Some(path) = self.inside(root, rel) else { return };
                child = path;
                &child
            };
            if !rel.is_empty() {
                match std::fs::symlink_metadata(path) {
                    Ok(meta) if meta.is_dir() => {}
                    Ok(_) => return,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                    Err(_) => {
                        self.fault.get_or_insert(git::Refused::CannotAnswerYet);
                        return;
                    }
                }
            }
            let Ok(dir) = std::fs::read_dir(path) else {
                self.fault.get_or_insert(git::Refused::CannotAnswerYet);
                return;
            };
            let start = into.len();
            for entry in dir {
                let Ok(entry) = entry else {
                    self.fault.get_or_insert(git::Refused::CannotAnswerYet);
                    return;
                };
                let Ok(name) = entry.file_name().into_string() else {
                    self.fault.get_or_insert(git::Refused::QuotedPath);
                    return;
                };
                into.push(blaze_own::String::from_str_in(&name, alloc));
            }
            into[start..].sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            return;
        }
        let Ok(Some(Node::Folder(entries))) = self.tree.node(rel) else { return };
        for (name, _) in entries {
            if let Ok(name) = core::str::from_utf8(&name) {
                into.push(blaze_own::String::from_str_in(name, alloc));
            }
        }
    }

    fn within<'p>(&self, path: &'p str) -> Option<&'p str> {
        let Tree::Dir(root) = self.tree else { return None };
        let rest = path.strip_prefix(root.to_str()?)?;
        if rest.is_empty() {
            return Some(rest);
        }
        rest.strip_prefix('/')
    }

    /// A link contributes its own bytes even when its target is outside the tree.
    fn read_link(&mut self, rel: &str, into: &mut blaze_own::AVec<'_, u8>) -> Result<(), git::LinkUnread> {
        let tree: &Tree = self.tree;
        let Tree::Dir(root) = tree else { return Err(git::LinkUnread::NotALink) };
        let path = self.inside(root, rel).ok_or(git::LinkUnread::NotALink)?;
        let target = std::fs::read_link(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::InvalidInput | std::io::ErrorKind::NotFound => git::LinkUnread::NotALink,
            _ => git::LinkUnread::Unreadable,
        })?;
        into.extend_from_slice(target.as_os_str().as_encoded_bytes());
        Ok(())
    }
}

impl System for ScriptSandbox {
    fn args(&mut self) -> Result<(&[u8], usize), Stop> {
        Ok((b"build-script-build\0", 1))
    }

    fn environ(&mut self) -> Result<(&[u8], usize), Stop> {
        Ok((&self.environ, self.script.env.len()))
    }

    fn clock_res(&mut self, _id: u32) -> Answer<u64> {
        Ok(Ok(1))
    }

    fn clock_time(&mut self, _id: u32, _fuel_left: Option<u64>) -> Answer<u64> {
        Ok(Ok(0))
    }

    fn random(&mut self, into: &mut [u8]) -> Result<(), Stop> {
        into.fill(0);
        Ok(())
    }

    fn write(&mut self, fd: u32, bytes: &[u8]) -> Answer<usize> {
        match fd {
            1 => {
                self.stdout_write(bytes)?;
                return Ok(Ok(bytes.len()));
            }
            2 => {
                self.stderr.extend_from_slice(bytes);
                if self.stderr.len() > STDERR_TAIL {
                    self.stderr.drain(..self.stderr.len() - STDERR_TAIL);
                }
                return Ok(Ok(bytes.len()));
            }
            _ => {}
        }
        let (path, pos, append) = match self.opened(fd) {
            Some(Opened::Out { path, pos, append, write: true }) => (path.clone(), *pos, *append),
            _ => return Ok(Err(Errno::BADF)),
        };
        let len = self.out.get(&path).and_then(|it| it.as_ref()).map_or(0, |b| b.len() as u64);
        let start = if append { len } else { pos };
        let end = start + bytes.len() as u64;
        self.grow(end.saturating_sub(len))?;
        if let Some(Some(file)) = self.out.edit().get_mut(&path) {
            if (file.len() as u64) < end {
                file.resize(end as usize, 0);
            }
            file[start as usize..end as usize].copy_from_slice(bytes);
        }
        self.out_bytes += end.saturating_sub(len);
        if let Some(Opened::Out { pos, .. }) = self.opened(fd) {
            *pos = end;
        }
        Ok(Ok(bytes.len()))
    }

    fn read(&mut self, fd: u32, into: &mut [u8]) -> Answer<usize> {
        if fd == 0 {
            return Ok(Ok(0));
        }
        let (bytes, pos): (&[u8], &mut u64) = match fd.checked_sub(ROOT_FD + 1).and_then(|at| self.opened.get_mut(at as usize)).and_then(Option::as_mut) {
            Some(Opened::Ro { entries: Some(_), .. }) => return Ok(Err(Errno::ISDIR)),
            Some(Opened::Ro { bytes, pos, .. }) => (bytes, pos),
            Some(Opened::Out { path, pos, .. }) => match self.out.get(path.as_str()) {
                Some(Some(bytes)) => (bytes, pos),
                _ => return Ok(Err(Errno::ISDIR)),
            },
            None => return Ok(Err(Errno::BADF)),
        };
        let at = (*pos as usize).min(bytes.len());
        let n = into.len().min(bytes.len() - at);
        into[..n].copy_from_slice(&bytes[at..at + n]);
        *pos += n as u64;
        Ok(Ok(n))
    }

    fn fd_filetype(&mut self, fd: u32) -> Answer<Filetype> {
        Ok(Ok(match fd {
            0..=2 => Filetype::CharacterDevice,
            ROOT_FD => Filetype::Directory,
            _ => match self.fd_stat(fd)? {
                Ok(stat) => stat.filetype,
                Err(errno) => return Ok(Err(errno)),
            },
        }))
    }

    fn set_fd_flags(&mut self, _fd: u32, _flags: u32) -> Answer<()> {
        Ok(Ok(()))
    }

    fn close(&mut self, fd: u32) -> Answer<()> {
        let slot = fd.checked_sub(ROOT_FD + 1).and_then(|at| self.opened.get_mut(at as usize));
        Ok(match slot.and_then(Option::take) {
            Some(_) => Ok(()),
            None => Err(Errno::BADF),
        })
    }

    fn seek(&mut self, fd: u32, offset: i64, whence: u32) -> Answer<u64> {
        let size = match self.fd_stat(fd)? {
            Ok(stat) => stat.size,
            Err(errno) => return Ok(Err(errno)),
        };
        let pos = match self.opened(fd) {
            Some(Opened::Ro { pos, .. } | Opened::Out { pos, .. }) => pos,
            None => return Ok(Err(Errno::BADF)),
        };
        let base = match whence {
            0 => 0,
            1 => *pos as i64,
            2 => size as i64,
            _ => return Ok(Err(Errno::INVAL)),
        };
        let Some(new) = base.checked_add(offset).filter(|it| *it >= 0) else { return Ok(Err(Errno::INVAL)) };
        *pos = new as u64;
        Ok(Ok(new as u64))
    }

    fn fd_stat(&mut self, fd: u32) -> Answer<Filestat> {
        Ok(match fd {
            0..=2 => Ok(filestat(Filetype::CharacterDevice, 0)),
            ROOT_FD => Ok(filestat(Filetype::Directory, 0)),
            _ => match fd.checked_sub(ROOT_FD + 1).and_then(|at| self.opened.get(at as usize)).and_then(Option::as_ref) {
                Some(Opened::Ro { entries: Some(_), .. }) => Ok(filestat(Filetype::Directory, 0)),
                Some(Opened::Ro { bytes, .. }) => Ok(filestat(Filetype::RegularFile, bytes.len() as u64)),
                Some(Opened::Out { path, .. }) => match self.out.get(path.as_str()) {
                    Some(Some(bytes)) => Ok(filestat(Filetype::RegularFile, bytes.len() as u64)),
                    Some(None) => Ok(filestat(Filetype::Directory, 0)),
                    None => Err(Errno::NOENT),
                },
                None => Err(Errno::BADF),
            },
        })
    }

    fn open(&mut self, at: u32, path: &[u8], how: Open) -> Answer<u32> {
        let path = match self.from(at, path) {
            Ok(path) => path,
            Err(errno) => return Ok(Err(errno)),
        };
        let path = path.as_slice();
        let writes = how.create() || how.truncate() || how.write() || how.append();
        match self.place(path)? {
            Place::Work(rel) => {
                if writes {
                    return Err(self.work_write(&rel));
                }
                let script = Arc::clone(&self.script);
                let found = match script.tree.node(&rel) {
                    Ok(found) => found,
                    Err(why) => return Err(self.refuse(Refusal::Path(why.to_string()))),
                };
                self.record_node(rel, NodeRead::open(&how), found.as_ref());
                let opened = match found {
                    None => return Ok(Err(Errno::NOENT)),
                    Some(Node::Folder(entries)) => {
                        Opened::Ro { bytes: Vec::new(), pos: 0, entries: Some(entries), folder: path.to_vec() }
                    }
                    Some(Node::File(bytes)) => {
                        if how.directory() {
                            return Ok(Err(Errno::NOTDIR));
                        }
                        Opened::Ro { bytes: bytes.into_owned(), pos: 0, entries: None, folder: Vec::new() }
                    }
                };
                Ok(Ok(self.slot(opened)))
            }
            Place::Above(entry) => {
                if writes {
                    return Err(self.refuse(Refusal::Path("wrote above OUT_DIR, outside it".to_owned())));
                }
                Ok(Ok(self.slot(Opened::Ro { bytes: Vec::new(), pos: 0, entries: Some(vec![(entry, Filetype::Directory)]), folder: path.to_vec() })))
            }
            Place::Dep { mount, rel } => self.dep_open(mount, rel, path, how),
            Place::Ancestor(entries) => self.ancestor_open(entries, path, writes),
            Place::Out(rel) => {
                match self.out.get(&rel) {
                    Some(None) => {
                        if writes && !how.directory() {
                            return Ok(Err(Errno::ISDIR));
                        }
                        let entries = self.out_listing(&rel);
                        return Ok(Ok(self.slot(Opened::Ro { bytes: Vec::new(), pos: 0, entries: Some(entries), folder: path.to_vec() })));
                    }
                    Some(Some(_)) if how.create() && how.exclusive() => return Ok(Err(Errno::EXIST)),
                    Some(Some(_)) if how.directory() => return Ok(Err(Errno::NOTDIR)),
                    Some(Some(_)) => {}
                    None if !how.create() => return Ok(Err(Errno::NOENT)),
                    None => {
                        if !self.out_parent_exists(&rel) {
                            return Ok(Err(Errno::NOENT));
                        }
                        self.count_file(&rel)?;
                        self.out.edit().insert(rel.clone(), Some(Vec::new()));
                    }
                }
                if how.truncate()
                    && let Some(Some(bytes)) = self.out.edit().get_mut(&rel)
                {
                    self.out_bytes -= bytes.len() as u64;
                    bytes.clear();
                }
                let write = how.write() || how.append();
                Ok(Ok(self.slot(Opened::Out { path: rel, pos: 0, append: how.append(), write })))
            }
        }
    }

    fn path_stat(&mut self, at: u32, path: &[u8], _follow: bool) -> Answer<Filestat> {
        let path = match self.from(at, path) {
            Ok(path) => path,
            Err(errno) => return Ok(Err(errno)),
        };
        match self.place(&path)? {
            Place::Work(rel) => {
                let script = Arc::clone(&self.script);
                let found = match script.tree.node(&rel) {
                    Ok(found) => found,
                    Err(why) => return Err(self.refuse(Refusal::Path(why.to_string()))),
                };
                self.record_node(rel, NodeRead::Stat, found.as_ref());
                let found = match found {
                    Some(Node::File(bytes)) => Some((Filetype::RegularFile, bytes.len() as u64)),
                    Some(Node::Folder(_)) => Some((Filetype::Directory, 0)),
                    None => None,
                };
                Ok(found.map(|(filetype, size)| filestat(filetype, size)).ok_or(Errno::NOENT))
            }
            Place::Above(_) | Place::Ancestor(_) => Ok(Ok(filestat(Filetype::Directory, 0))),
            Place::Dep { mount, rel } => self.dep_stat(mount, rel),
            Place::Out(rel) => Ok(match self.out.get(&rel) {
                Some(Some(bytes)) => Ok(filestat(Filetype::RegularFile, bytes.len() as u64)),
                Some(None) => Ok(filestat(Filetype::Directory, 0)),
                None => Err(Errno::NOENT),
            }),
        }
    }

    fn readlink(&mut self, _at: u32, _path: &[u8]) -> Answer<Vec<u8>> {
        Ok(Err(Errno::INVAL))
    }

    fn dir_entry(&mut self, fd: u32, cookie: u64) -> Answer<Option<Dirent<'_>>> {
        let Some(file) = self.opened(fd) else { return Ok(Err(Errno::BADF)) };
        let Opened::Ro { entries: Some(entries), .. } = file else { return Ok(Err(Errno::NOTDIR)) };
        Ok(Ok(entries.get(cookie as usize).map(|(name, filetype)| Dirent { ino: 0, filetype: *filetype, name })))
    }

    fn poll_oneoff(&mut self) -> Stop {
        self.refuse(Refusal::Wait)
    }

    fn exit(&mut self, code: u32) -> Stop {
        self.exit = Some(code);
        Stop(format!("exited with code {code}"))
    }
}

/// A folder's files as a [`Tree::Mem`]: every file under `dir` of a record, by path from `dir`;
/// `git` is the commit when the record is a fetched git source's.
pub fn mem_tree<'f>(files: impl IntoIterator<Item = (&'f str, &'f [u8])>, dir: &str, git: Option<git::Oid>) -> Tree {
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    let mut out: Vec<(String, Vec<u8>)> = files.into_iter().filter_map(|(path, bytes)| path.strip_prefix(prefix.as_str()).map(|rel| (rel.to_owned(), bytes.to_vec()))).collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Tree::Mem { files: Arc::new(out), git }
}

/// A folder of the checkout as a [`Tree::Dir`], at the root the command resolved once.
pub fn dir_tree(root: &RealRoot) -> Tree {
    Tree::Dir(blaze_own::Share::share(root))
}

#[cfg(test)]
mod side_refusal_tests;
#[cfg(all(test, unix))]
mod escape_tests;
#[cfg(all(test, unix))]
mod served_tests;
