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
use blaze_build_script::probe::{self, Request, Row, Snippet, Table};
use blaze_expand::seen::{MemberDirError, SeenRoot};
use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_wasi::{Answer, Dirent, Errno, Filestat, Filetype, Open, ROOT_FD, Stop, System};

pub use crate::sandbox::Read;

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
    member: Option<String>,
}

impl ScriptRoot {
    /// A dependency's (registry, git, toolchain) script: it sees `/package`.
    pub const fn package() -> ScriptRoot {
        ScriptRoot { member: None }
    }

    /// A workspace member's script at `dir` under the workspace (`""`: its root package); a
    /// `dir` that is not a plain path under it is refused by name.
    pub fn member(dir: &str) -> Result<ScriptRoot, MemberDirError<'_>> {
        SeenRoot::member(dir)?;
        Ok(ScriptRoot { member: Some(dir.to_owned()) })
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

/// The workspace's files.
#[derive(Clone, Debug)]
pub enum Tree {
    /// A folder on disk, read-only (a workspace member's checkout).
    Dir(PathBuf),
    /// Files read out of a package's record: (path from the root, bytes), sorted by path.
    /// `git`: the commit when the record is a fetched git source's, whose `git` answers it
    /// gives (D54); `None` for a crate.
    Mem { files: Arc<Vec<(String, Vec<u8>)>>, git: Option<git::Oid> },
}

/// A file or a folder of the tree.
enum Node<'t> {
    File(Cow<'t, [u8]>),
    Folder(Vec<(Vec<u8>, Filetype)>),
}

impl Tree {
    /// Where this tree's `git` answers come from.
    pub fn vcs(&self) -> git::Vcs {
        match self {
            Tree::Mem { git: Some(commit), .. } => git::Vcs::Fetched(*commit),
            Tree::Dir(_) | Tree::Mem { git: None, .. } => git::Vcs::Served,
        }
    }

    /// The bytes of the file `rel` of the tree; `None` when it is absent, a folder or a link
    /// out of the tree.
    pub fn file(&self, rel: &str) -> Option<Vec<u8>> {
        match self.node(rel) {
            Ok(Some(Node::File(bytes))) => Some(bytes.into_owned()),
            _ => None,
        }
    }

    /// What `rel` is: its bytes or its entries; `None` when absent. `Err`: a link out of the
    /// tree.
    fn node(&self, rel: &str) -> Result<Option<Node<'_>>, String> {
        match self {
            Tree::Dir(root) => {
                let path = root.join(rel);
                let Ok(real) = std::fs::canonicalize(&path) else { return Ok(None) };
                let root = std::fs::canonicalize(root).map_err(|e| format!("the workspace folder {} cannot be resolved: {e}", root.display()))?;
                if !real.starts_with(&root) {
                    return Err(format!("`{rel}` (from the served tree's root) is a link out of the tree"));
                }
                if real.is_dir() {
                    let mut entries = Vec::new();
                    if let Ok(dir) = std::fs::read_dir(&real) {
                        for entry in dir.flatten() {
                            let filetype = if entry.path().is_dir() { Filetype::Directory } else { Filetype::RegularFile };
                            entries.push((entry.file_name().into_encoded_bytes(), filetype));
                        }
                    }
                    entries.sort_by(|a, b| a.0.cmp(&b.0));
                    return Ok(Some(Node::Folder(entries)));
                }
                Ok(std::fs::read(&real).ok().map(|bytes| Node::File(Cow::Owned(bytes))))
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

/// `env`'s variables owned, for [`Script::env`].
pub fn owned_env(env: &blaze_build_script::Env<'_>) -> Vec<(String, String)> {
    env.vars.iter().map(|(name, value)| (name.as_str().to_owned(), value.as_str().to_owned())).collect()
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
    /// `rustc` asked something that is neither a version nor a compile (`--print`).
    RustcArgs { args: Vec<String> },
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

/// One run's sandbox.
pub struct ScriptSandbox {
    pub(crate) script: Arc<Script>,
    opened: Vec<Option<Opened>>,
    /// `OUT_DIR`'s contents by path from it: `None` a folder, else a file's bytes.
    pub(crate) out: BTreeMap<String, Option<Vec<u8>>>,
    pub(crate) out_bytes: u64,
    pub(crate) stdout: Vec<u8>,
    line: u64,
    pub(crate) stderr: Vec<u8>,
    environ: Vec<u8>,
    pub(crate) reads: Vec<Read>,
    pub(crate) refused: Option<Refusal>,
    /// `proc_exit`'s code, when it exited.
    pub(crate) exit: Option<u32>,
    /// The C requests deferred, each argv joined by NUL.
    pub(crate) c_requests: Vec<String>,
    /// The last spawn's reply, read by the shim's `reply`.
    pub(crate) reply: Vec<u8>,
    pub(crate) spawns: u32,
}

fn digest(bytes: &[u8]) -> ContentId {
    ContentId::of("blaze build-script read", bytes)
}

fn listing_digest(entries: &[(Vec<u8>, Filetype)]) -> ContentId {
    let mut names = Vec::new();
    for (name, filetype) in entries {
        names.extend_from_slice(name);
        names.push(0);
        names.push(*filetype as u8);
    }
    digest(&names)
}

fn stat_digest(filetype: Filetype, size: u64) -> ContentId {
    let mut bytes = vec![filetype as u8];
    bytes.extend_from_slice(&size.to_le_bytes());
    digest(&bytes)
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
    if kind == "git" {
        // Recomputed, not reread: a refusal now is an answer that differs (D54).
        let (folder, command) = rel.split_once('\n').ok_or_else(unknown)?;
        let words: Vec<&str> = command.split(' ').collect();
        return Ok(git_fact(tree, folder, &words, Alloc::global()).ok().map(|answer| answer_digest(&answer)));
    }
    let node = tree.node(rel).map_err(|_| unknown())?;
    Ok(match (kind, node) {
        ("file", Some(Node::File(bytes))) => Some(digest(&bytes)),
        ("file", _) => None,
        ("dir", Some(Node::Folder(entries))) => Some(listing_digest(&entries)),
        ("dir", _) => None,
        ("stat", Some(Node::File(bytes))) => Some(stat_digest(Filetype::RegularFile, bytes.len() as u64)),
        ("stat", Some(Node::Folder(_))) => Some(stat_digest(Filetype::Directory, 0)),
        ("stat", None) => None,
        _ => return Err(unknown()),
    })
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
        let mut out = BTreeMap::new();
        out.insert(String::new(), None);
        ScriptSandbox {
            script,
            opened: Vec::new(),
            out,
            out_bytes: 0,
            stdout: Vec::new(),
            line: 0,
            stderr: Vec::new(),
            environ,
            reads: Vec::new(),
            refused: None,
            exit: None,
            c_requests: Vec::new(),
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

    pub(crate) fn record(&mut self, subject: String, digest: Option<ContentId>) {
        if !self.reads.iter().any(|it| it.subject == subject) {
            self.reads.push(Read { subject, digest });
        }
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
                let mut joined = folder.clone();
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
                self.out.insert(rel, None);
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
                    if let Some(Some(bytes)) = self.out.remove(&rel) {
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
        let bytes = self.out.remove(&from).flatten().unwrap_or_default();
        if let Some(Some(old)) = self.out.insert(to, Some(bytes)) {
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
        if let Some(Some(bytes)) = self.out.get_mut(&path) {
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
                    Err(why) => return Err(self.refuse(Refusal::Path(why))),
                };
                let bytes = match found {
                    Some(Node::File(bytes)) => Some(bytes.to_vec()),
                    _ => None,
                };
                self.record(format!("file:{rel}"), bytes.as_deref().map(digest));
                bytes
            }
            Place::Dep { mount, rel } => self.dep_file(mount, &rel)?,
            Place::Above(_) | Place::Ancestor(_) => None,
        })
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
        let routed = crate::spawn_route::route(name, &args_text, self.script.c_mode, self.script.spawner.is_some());
        let (code, stdout, stderr): (i32, Vec<u8>, Vec<u8>) = match routed {
            crate::spawn_route::Route::Rustc => match probe::request("rustc", &args_text) {
                Request::Version { verbose } => (0, probe::version(verbose, &self.script.host, Alloc::global()).as_bytes().to_vec(), Vec::new()),
                Request::Compile => {
                    let found = probe::snippet(&args_text);
                    let cwd = spawned.cwd.clone().unwrap_or_else(|| self.script.cwd());
                    let bytes = match found {
                        Some((_, Snippet::Stdin)) => Some(spawned.stdin.clone()),
                        Some((_, Snippet::File(path))) => self.snippet_file(&cwd, path)?,
                        None => None,
                    };
                    let Some(bytes) = bytes else {
                        return Err(self.refuse(Refusal::Spawn(SpawnRefusal::RustcArgs { args: owned() })));
                    };
                    let key = probe::compile_key(&self.script.target, &args_text, found.map(|(at, _)| at), &bytes);
                    match (Table { rows: &self.script.table }).answer(&key) {
                        Some(true) => (0, Vec::new(), Vec::new()),
                        Some(false) => (1, Vec::new(), b"error: the pinned nightly rejects this probe (blaze's table of answers)\n".to_vec()),
                        None => {
                            let snippet = String::from_utf8_lossy(&bytes).into_owned();
                            return Err(self.refuse(Refusal::Spawn(SpawnRefusal::CannotAnswerYet { key, args: owned(), snippet, target: self.script.target.clone() })));
                        }
                    }
                }
                Request::Print | Request::Other { .. } => return Err(self.refuse(Refusal::Spawn(SpawnRefusal::RustcArgs { args: owned() }))),
            },
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
                let answered = match folder.as_deref().map(|folder| (folder, git_subject(folder, &words))) {
                    Some((folder, Some(subject))) => git_fact(&script.tree, folder, &words, Alloc::global()).map(|answer| (subject, answer)),
                    Some((_, None)) => Err(git::Refused::CannotAnswerYet),
                    None => Err(git::Refused::NoRepository),
                };
                match answered {
                    Ok((subject, answer)) => {
                        self.record(subject, Some(answer_digest(&answer)));
                        (answer.code, answer.stdout.as_bytes().to_vec(), answer.stderr.as_bytes().to_vec())
                    }
                    Err(why) => return Err(self.refuse(Refusal::Spawn(SpawnRefusal::Git { command, why }))),
                }
            }
            crate::spawn_route::Route::Fact(fact) => return self.spawn_fact(&fact, program, args, &spawned),
            crate::spawn_route::Route::Deferred => {
                self.c_requests.push(spawned.argv.join("\0"));
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
    // The files behind the fact are not the record's reads: the question is.
    let mut behind = Vec::new();
    git::answer(git::question(words), folder, tree.vcs(), &mut GitView::new(tree, &mut behind, alloc), alloc)
}

/// A `git` question as a read's subject: `git:<folder>\n<words>`; `None` when a word is empty
/// or holds whitespace (or the folder a newline), which would not read back the same.
fn git_subject(folder: &str, words: &[&str]) -> Option<String> {
    if folder.contains('\n') || words.iter().any(|word| word.is_empty() || word.contains(char::is_whitespace)) {
        return None;
    }
    Some(format!("git:{folder}\n{}", words.join(" ")))
}

fn answer_digest(answer: &git::Answer<'_>) -> ContentId {
    let mut bytes = blaze_own::Vec::new_in(Alloc::global());
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
    reads: &'s mut Vec<Read>,
    /// Folders (relative to the root) proven real by `lstat`; sorted by bytes.
    real_folders: blaze_own::AVec<'s, blaze_own::AString<'s>>,
}

impl<'s> GitView<'s> {
    pub(crate) fn new(tree: &'s Tree, reads: &'s mut Vec<Read>, alloc: Alloc<'s>) -> Self {
        GitView { tree, reads, real_folders: blaze_own::Vec::new_in(alloc) }
    }

    /// `root/rel` when `rel` is plain (no empty, `.` or `..` part) and each folder before its
    /// last part is a real folder, not a link (one `lstat` per folder, once per view); `None`
    /// otherwise, which callers read as absent.
    fn inside(&mut self, root: &Path, rel: &str) -> Option<PathBuf> {
        if rel.split('/').any(|part| matches!(part, "" | "." | "..")) {
            return None;
        }
        for (end, _) in rel.match_indices('/') {
            let folder = &rel[..end];
            let Err(at) = self.real_folders.binary_search_by(|known| known.as_bytes().cmp(folder.as_bytes())) else { continue };
            if !std::fs::symlink_metadata(root.join(folder)).ok()?.file_type().is_dir() {
                return None;
            }
            let alloc = *self.real_folders.allocator();
            self.real_folders.insert(at, blaze_own::String::from_str_in(folder, alloc));
        }
        Some(root.join(rel))
    }
}

impl GitFiles for GitView<'_> {
    fn read(&mut self, rel: &str, into: &mut blaze_own::AVec<'_, u8>) -> bool {
        let bytes = self.tree.file(rel);
        let subject = format!("file:{rel}");
        if !self.reads.iter().any(|it| it.subject == subject) {
            self.reads.push(Read { subject, digest: bytes.as_deref().map(digest) });
        }
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
                let meta = std::fs::symlink_metadata(self.inside(root, rel)?).ok()?;
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
        let tree: &Tree = self.tree;
        if let Tree::Dir(root) = tree {
            // Names only: no stat per entry. The folder itself must be real, not a link.
            let Some(path) = self.inside(root, rel) else { return };
            if !std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_dir()) {
                return;
            }
            let Ok(dir) = std::fs::read_dir(&path) else { return };
            let start = into.len();
            for entry in dir.flatten() {
                if let Ok(name) = entry.file_name().into_string() {
                    into.push(blaze_own::String::from_str_in(&name, alloc));
                }
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

    /// `readlink` of `rel`'s last part, reached like every path here (each folder on the way a
    /// real folder, no link passed through): the link's own text, never followed, so nothing
    /// outside the tree is read. Like `stat`, not a read of the record: the git answer is (D54).
    /// A record's tree (`Tree::Mem`) holds no links.
    fn read_link(&mut self, rel: &str, into: &mut blaze_own::AVec<'_, u8>) -> Result<(), git::LinkUnread> {
        let tree: &Tree = self.tree;
        let Tree::Dir(root) = tree else { return Err(git::LinkUnread::NotALink) };
        let path = self.inside(root, rel).ok_or(git::LinkUnread::NotALink)?;
        let target = std::fs::read_link(&path).map_err(|e| match e.kind() {
            // EINVAL (not a link) and ENOENT: what git sees as a type change.
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
        if let Some(Some(file)) = self.out.get_mut(&path) {
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
                    Err(why) => return Err(self.refuse(Refusal::Path(why))),
                };
                let opened = match found {
                    None => {
                        self.record(format!("file:{rel}"), None);
                        return Ok(Err(Errno::NOENT));
                    }
                    Some(Node::Folder(entries)) => {
                        self.record(format!("dir:{rel}"), Some(listing_digest(&entries)));
                        Opened::Ro { bytes: Vec::new(), pos: 0, entries: Some(entries), folder: path.to_vec() }
                    }
                    Some(Node::File(bytes)) => {
                        if how.directory() {
                            return Ok(Err(Errno::NOTDIR));
                        }
                        self.record(format!("file:{rel}"), Some(digest(&bytes)));
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
            Place::Dep { mount, rel } => self.dep_open(mount, &rel, path, how),
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
                        self.out.insert(rel.clone(), Some(Vec::new()));
                    }
                }
                if how.truncate()
                    && let Some(Some(bytes)) = self.out.get_mut(&rel)
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
                    Err(why) => return Err(self.refuse(Refusal::Path(why))),
                };
                let found = match found {
                    Some(Node::File(bytes)) => Some((Filetype::RegularFile, bytes.len() as u64)),
                    Some(Node::Folder(_)) => Some((Filetype::Directory, 0)),
                    None => None,
                };
                self.record(format!("stat:{rel}"), found.map(|(filetype, size)| stat_digest(filetype, size)));
                Ok(found.map(|(filetype, size)| filestat(filetype, size)).ok_or(Errno::NOENT))
            }
            Place::Above(_) | Place::Ancestor(_) => Ok(Ok(filestat(Filetype::Directory, 0))),
            Place::Dep { mount, rel } => self.dep_stat(mount, &rel),
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

/// A folder of the checkout as a [`Tree::Dir`].
pub fn dir_tree(root: &Path) -> Tree {
    Tree::Dir(root.to_owned())
}

#[cfg(test)]
mod side_refusal_tests;
