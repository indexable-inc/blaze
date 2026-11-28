//! The macro's view of the world: WASI preview 1 answered with no ambient authority.
//!
//! - The clock reads 0 and randomness is zeros: an expansion is a function of its inputs.
//! - There are no arguments, no environment list and no network. A variable is read one at a
//!   time through the shim's `getenv`, from the calling crate's variables the caller gave
//!   ([`PackageEnv`]: set, unset, or unknown), and each answered read is recorded; a read of an
//!   unknown one stops the run, naming it ([`EnvRefused`], package_env.rs).
//! - Files are served at the package's mount row, [`Files::seen`] (decision D96): a fetched
//!   package's own files at [`PACKAGE`], nothing above it; a workspace member's workspace at
//!   `/workspace`, the member at its dir in it (no machine path reaches a key or an output).
//!   `CARGO_MANIFEST_DIR` and `CARGO_MANIFEST_PATH` are derived from that row and nothing else
//!   (`seen_env`), and the macro runs in it. Each path opened or looked at is recorded by its
//!   path from the served tree's root with the digest of what was found. A path that leaves the
//!   tree (by `..` or a link) stops the run with [`ProcLimit::Paths`], naming the path.
//! - Standard output and error are dropped.

use std::path::{Path, PathBuf};

use blaze_expand::ProcLimit;
use blaze_ids::ContentId;
use blaze_wasi::{Answer, Dirent, Errno, Filestat, Filetype, Open, ROOT_FD, Stop, System};

use crate::package_env::{EnvAnswer, EnvRefused, PackageEnv};

/// The virtual folder a fetched package's files are served at, and its `CARGO_MANIFEST_DIR`
/// (a member's is `/workspace/<dir>`: [`Files::seen`]).
pub const PACKAGE: &str = "/package";

pub mod seen_env;
pub use seen_env::SeenMismatch;

/// File bytes in a [`Files::Listed`] sandbox tree: zero-copy static slice for raw files,
/// owned bytes when decompressed from a `BLZFPK05` pack, or an unpopulated git submodule.
#[derive(Debug, PartialEq, Eq)]
pub enum ListedBytes {
    Static(&'static [u8]),
    Owned(blaze_own::Vec<u8, blaze_own::SharedAlloc<'static>>),
    /// An unpopulated git submodule (`0o160000`) pinned at this 20-byte commit SHA-1.
    Submodule([u8; 20]),
}

impl ListedBytes {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            ListedBytes::Static(s) => s,
            ListedBytes::Owned(s) => s.as_slice(),
            ListedBytes::Submodule(_) => &[],
        }
    }
}

impl core::ops::Deref for ListedBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for ListedBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl From<&'static [u8]> for ListedBytes {
    fn from(s: &'static [u8]) -> Self {
        ListedBytes::Static(s)
    }
}

impl From<blaze_own::Vec<u8, blaze_own::SharedAlloc<'static>>> for ListedBytes {
    fn from(s: blaze_own::Vec<u8, blaze_own::SharedAlloc<'static>>) -> Self {
        ListedBytes::Owned(s)
    }
}

impl PartialEq<[u8]> for ListedBytes {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_slice() == other
    }
}

impl PartialEq<&[u8]> for ListedBytes {
    fn eq(&self, other: &&[u8]) -> bool {
        self.as_slice() == *other
    }
}

/// A package's own files, the only ones its macros see.
#[derive(Clone)]
pub enum Files {
    /// A folder on disk: a workspace member's own.
    Folder(PathBuf),
    /// A record's files (a fetched package's, read in place from its fetch record), by their
    /// path from the package's folder, sorted by path, each once.
    Listed(std::sync::Arc<[(String, ListedBytes)]>),
    /// A workspace member's (D96 (3)): its workspace's folder on disk, served at `/workspace`
    /// with the member at `root`'s dir in it; paths are from the workspace's folder.
    Member { workspace: PathBuf, root: crate::script_sandbox::ScriptRoot },
}

impl std::fmt::Debug for Files {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Files::Folder(dir) => write!(f, "Folder({})", dir.display()),
            Files::Listed(files) => write!(f, "Listed({} files)", files.len()),
            Files::Member { workspace, root } => write!(f, "Member({} at {})", workspace.display(), root.seen()),
        }
    }
}

/// What a path of a package is.
enum Found {
    Folder,
    File(u64),
    Link,
}

impl Files {
    /// What a macro served these files sees as its package's root (D96): `/package` for a
    /// dependency's, `/workspace/<dir>` for a member's.
    pub fn seen(&self) -> blaze_expand::seen::SeenRoot<'_> {
        match self {
            Files::Folder(_) | Files::Listed(_) => blaze_expand::seen::SeenRoot::Package,
            Files::Member { root, .. } => root.seen(),
        }
    }

    /// The canonical path on disk of `rel` (a path from the served tree's root `root`) together
    /// with the canonicalized `root`, when it is inside the served tree. `Err`: a link out of the
    /// tree, named by the path as the macro saw it (rule 25: no path of this machine in the words,
    /// which a names record's row may hold).
    ///
    /// `root` may be relative to the folder blaze was started in, and `""` IS that folder: the
    /// checkout's form (`FetchEnv::checkout`, "" when blaze runs in the workspace's root), which
    /// a member's `Files::Member { workspace, .. }` is (blaze-cli proc_callers.rs `shown`). The OS names it `.`: `canonicalize("")` is
    /// `ENOENT`, which refused every file of every member of a checkout so started, `chdir` into
    /// `/workspace/<dir>` first (the shim's, at each call; MX 2026-11-17).
    fn real(&self, root: &Path, rel: &str) -> Result<Option<(PathBuf, PathBuf)>, String> {
        let root = if root.as_os_str().is_empty() { Path::new(".") } else { root };
        let seen = self.seen();
        let shown = || if rel.is_empty() { seen.mount().to_owned() } else { format!("{}/{rel}", seen.mount()) };
        let Ok(real) = std::fs::canonicalize(root.join(rel)) else { return Ok(None) };
        let Ok(top) = std::fs::canonicalize(root) else {
            return Err(format!("opened {}, but the folder {} is served from cannot be resolved", shown(), seen.mount()));
        };
        if !real.starts_with(&top) {
            return Err(format!("opened {}, a link out of {}, the tree the macro is served", shown(), seen.mount()));
        }
        Ok(Some((real, top)))
    }

    /// The listed files under the folder `rel` (`""`: the package's).
    fn under<'f>(files: &'f [(String, ListedBytes)], rel: &str) -> impl Iterator<Item = (&'f str, &'f ListedBytes)> + 'f {
        let prefix = if rel.is_empty() { String::new() } else { format!("{rel}/") };
        let start = files.partition_point(|(path, _)| path.as_str() < prefix.as_str());
        let n = prefix.len();
        files[start..]
            .iter()
            .take_while(move |(path, _)| path.starts_with(prefix.as_str()))
            .map(move |(path, bytes)| (&path[n..], bytes))
    }

    fn submodule_covering(&self, rel: &str) -> Option<(&str, [u8; 20])> {
        let Files::Listed(files) = self else { return None };
        for (path, bytes) in files.iter() {
            let ListedBytes::Submodule(commit) = bytes else { continue };
            if rel == path.as_str()
                || (rel.len() > path.len()
                    && rel.starts_with(path.as_str())
                    && rel.as_bytes().get(path.len()) == Some(&b'/'))
            {
                return Some((path.as_str(), *commit));
            }
        }
        None
    }

    fn submodule_hex(commit: &[u8; 20]) -> [u8; 40] {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = [0u8; 40];
        for (i, &byte) in commit.iter().enumerate() {
            if let Some(hi) = out.get_mut(i * 2) {
                *hi = *HEX.get(usize::from(byte >> 4)).unwrap_or(&b'0');
            }
            if let Some(lo) = out.get_mut(i * 2 + 1) {
                *lo = *HEX.get(usize::from(byte & 0x0f)).unwrap_or(&b'0');
            }
        }
        out
    }

    fn find(&self, rel: &str, follow: bool) -> Result<Option<Found>, String> {
        match self {
            Files::Folder(root) | Files::Member { workspace: root, .. } => {
                if follow {
                    return Ok(self.real(root, rel)?.and_then(|(real, _)| stat_of(&real, true)));
                }
                if rel.is_empty() {
                    return Ok(self.real(root, rel)?.map(|_| Found::Folder));
                }
                let (parent, name) = rel.rsplit_once('/').unwrap_or(("", rel));
                if matches!(name, "" | "." | "..") {
                    return Ok(self.real(root, rel)?.and_then(|(real, _)| stat_of(&real, true)));
                }
                Ok(self.real(root, parent)?.and_then(|(parent_real, _)| stat_of(&parent_real.join(name), false)))
            }
            Files::Listed(files) => {
                if let Some((path, commit)) = self.submodule_covering(rel) {
                    let hex = Self::submodule_hex(&commit);
                    return Err(format!("`{rel}` is inside unpopulated git submodule `{path}` at commit {}", core::str::from_utf8(&hex).unwrap_or("")));
                }
                if let Ok(at) = files.binary_search_by(|(path, _)| path.as_str().cmp(rel)) {
                    return Ok(Some(Found::File(files[at].1.len() as u64)));
                }
                Ok((rel.is_empty() || Files::under(files, rel).next().is_some()).then_some(Found::Folder))
            }
        }
    }

    /// The bytes of the file `rel`; `None` when absent.
    fn bytes(&self, rel: &str) -> Result<Option<Vec<u8>>, String> {
        match self {
            Files::Folder(root) | Files::Member { workspace: root, .. } => Ok(self.real(root, rel)?.and_then(|(real, _)| std::fs::read(real).ok())),
            Files::Listed(files) => {
                if let Some((path, commit)) = self.submodule_covering(rel) {
                    let hex = Self::submodule_hex(&commit);
                    return Err(format!("`{rel}` is inside unpopulated git submodule `{path}` at commit {}", core::str::from_utf8(&hex).unwrap_or("")));
                }
                Ok(files.binary_search_by(|(path, _)| path.as_str().cmp(rel)).ok().map(|at| files[at].1.to_vec()))
            }
        }
    }

    /// The canonical in-tree path and entries of the folder `rel`, sorted by name.
    fn listing(&self, rel: &str) -> Result<(PathBuf, Vec<(Vec<u8>, Filetype)>), String> {
        match self {
            Files::Folder(root) | Files::Member { workspace: root, .. } => Ok(match self.real(root, rel)? {
                Some((real, top)) => (real.strip_prefix(&top).map(Path::to_path_buf).unwrap_or_default(), listing(&real)),
                None => Default::default(),
            }),
            Files::Listed(files) => {
                if let Some((path, commit)) = self.submodule_covering(rel) {
                    let hex = Self::submodule_hex(&commit);
                    return Err(format!("`{rel}` is inside unpopulated git submodule `{path}` at commit {}", core::str::from_utf8(&hex).unwrap_or("")));
                }
                let mut entries: Vec<(Vec<u8>, Filetype)> = Vec::new();
                for (rest, bytes) in Files::under(files, rel) {
                    let (name, filetype) = match rest.split_once('/') {
                        Some((name, _)) => (name, Filetype::Directory),
                        None if matches!(bytes, ListedBytes::Submodule(_)) => (rest, Filetype::Directory),
                        None => (rest, Filetype::RegularFile),
                    };
                    if entries.last().map(|(last, _)| last.as_slice()) != Some(name.as_bytes()) {
                        entries.push((name.as_bytes().to_vec(), filetype));
                    }
                }
                // Sorted by full path, a folder's `name/` sorts after `name.x`: sort by name.
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                entries.dedup_by(|a, b| a.0 == b.0);
                Ok((PathBuf::from(rel), entries))
            }
        }
    }
}

/// What a run read, in the order of its first read: a subject and the digest of its answer
/// (`None`: absent).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Read {
    pub subject: String,
    pub digest: Option<ContentId>,
}

/// The package a call is expanded in.
#[derive(Clone, Debug)]
pub struct Package {
    /// The package's files, served at their row ([`Files::seen`]); the path variables are that
    /// row's (`seen_env`). Made by [`Package::new`].
    pub files: Files,
    /// The variables of the calling crate the macro may read (cargo's `CARGO_*` of the crate
    /// and the like, paths in them under the root it is served), each set, unset or unknown
    /// (package_env.rs; the truth's home is blaze-scope's `CrateEnv`).
    pub env: PackageEnv,
}

impl Package {
    /// The folder the macro runs in: the root it sees (`CARGO_MANIFEST_DIR`, D96).
    pub fn cwd(&self) -> String {
        self.files.seen().to_string()
    }
}

/// A file or folder the macro opened.
struct Opened {
    /// Its whole bytes, read when opened.
    bytes: Vec<u8>,
    pos: u64,
    /// A folder's normalized path from the served tree's root (`""` for the root) and its sorted entries.
    dir: Option<(String, Vec<(Vec<u8>, Filetype)>)>,
}

/// One run's sandbox.
pub struct Sandbox {
    package: std::sync::Arc<Package>,
    files: Vec<Option<Opened>>,
    pub(crate) reads: Vec<Read>,
    /// Why the sandbox stopped the run, when a limit did.
    pub(crate) refused: Option<(ProcLimit, String)>,
    /// The first variable read whose value this build does not know: the run stops with it.
    pub(crate) env_refused: Option<EnvRefused>,
}

fn digest(bytes: &[u8]) -> ContentId {
    ContentId::of("blaze proc-macro read", bytes)
}

/// A folder's entries, sorted by name.
fn listing(real: &Path) -> Vec<(Vec<u8>, Filetype)> {
    let mut entries = Vec::new();
    if let Ok(dir) = std::fs::read_dir(real) {
        for entry in dir.flatten() {
            let filetype = match entry.file_type() {
                Ok(ft) if ft.is_symlink() => Filetype::SymbolicLink,
                Ok(ft) if ft.is_dir() => Filetype::Directory,
                _ => Filetype::RegularFile,
            };
            entries.push((entry.file_name().into_encoded_bytes(), filetype));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

/// What a folder's read is answered by: its canonical in-tree path and its names, in order.
fn listing_digest(canon: &Path, entries: &[(Vec<u8>, Filetype)]) -> ContentId {
    let mut names = Vec::new();
    names.extend_from_slice(canon.as_os_str().as_encoded_bytes());
    names.push(0);
    for (name, filetype) in entries {
        names.extend_from_slice(name);
        names.push(0);
        names.push(*filetype as u8);
    }
    digest(&names)
}

/// What a stat is answered by: the type and the size.
fn stat_digest(filetype: Filetype, size: u64) -> ContentId {
    let mut bytes = vec![filetype as u8];
    bytes.extend_from_slice(&size.to_le_bytes());
    digest(&bytes)
}

fn stat_of(path: &Path, follow: bool) -> Option<Found> {
    let meta = if follow { std::fs::metadata(path) } else { std::fs::symlink_metadata(path) }.ok()?;
    let ft = meta.file_type();
    Some(if ft.is_symlink() {
        Found::Link
    } else if ft.is_dir() {
        Found::Folder
    } else {
        Found::File(meta.len())
    })
}

/// The subject of the read `Span::file` makes (server.rs): the file holding the call, as the
/// host shows it (blaze-expand `ProcCall::file`, D96). A fact of the call, not of the package:
/// [`answer_now`] knows no answer to it, and the table values it against the call it finds a
/// row for (table.rs `list_holds`).
pub const CALL_FILE: &str = "place:file";

/// What the read [`CALL_FILE`] is answered by: the shown path's bytes. The path is the
/// member's workspace-relative one or a dependency's `<name>-<version>/<path>`, never the
/// machine's folder (rule 25).
pub fn call_file_digest(shown: &str) -> ContentId {
    digest(shown.as_bytes())
}

/// Why a read's subject has no answer now: a subject of no known form, or a variable whose
/// value this build does not know in the package (package_env.rs).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnknownSubject(pub String);

/// What the read `subject` (as a [`Read`] names it) is answered by now, in `package`: the
/// same answer a run would record. `None`: absent.
pub fn answer_now(package: &Package, subject: &str) -> Result<Option<ContentId>, UnknownSubject> {
    let unknown = || UnknownSubject(subject.to_owned());
    let (kind, rest) = subject.split_once(':').ok_or_else(unknown)?;
    let files = &package.files;
    // A link out of the package now answers as absent: the run that reads it is refused.
    Ok(match kind {
        // Set: the value's digest (`""` is a value); unset: absent. Unknown now: no answer, so
        // a kept answer that read the name is not taken here, and the run that reads it again
        // is refused by name.
        "env" if let Some(row) = package.row_var(rest) => Some(digest(row.as_bytes())),
        "env" => match package.env.get(rest) {
            EnvAnswer::Set(value) => Some(digest(value.as_bytes())),
            EnvAnswer::Unset => None,
            EnvAnswer::Unknown => return Err(unknown()),
        },
        "file" => files.bytes(rest).map_err(UnknownSubject)?.map(|bytes| digest(&bytes)),
        "dir" => match files.find(rest, true).map_err(UnknownSubject)? {
            Some(Found::Folder) => {
                let (canon, entries) = files.listing(rest).map_err(UnknownSubject)?;
                Some(listing_digest(&canon, &entries))
            }
            _ => None,
        },
        "stat" | "lstat" if rest.is_empty() => Some(stat_digest(Filetype::Directory, 0)),
        "stat" | "lstat" => match files.find(rest, kind == "stat").map_err(UnknownSubject)? {
            Some(Found::Folder) => Some(stat_digest(Filetype::Directory, 0)),
            Some(Found::File(size)) => Some(stat_digest(Filetype::RegularFile, size)),
            Some(Found::Link) => Some(stat_digest(Filetype::SymbolicLink, 0)),
            None => None,
        },
        _ => return Err(unknown()),
    })
}

impl Sandbox {
    pub fn new(package: std::sync::Arc<Package>) -> Sandbox {
        Sandbox { package, files: Vec::new(), reads: Vec::new(), refused: None, env_refused: None }
    }

    pub fn package(&self) -> &Package {
        &self.package
    }

    fn record(&mut self, subject: String, digest: Option<ContentId>) {
        if !self.reads.iter().any(|it| it.subject == subject) {
            self.reads.push(Read { subject, digest });
        }
    }

    /// The variable `name`, recorded: `Ok(Some(value))` set, `Ok(None)` unset (the macro's
    /// `Err(NotPresent)`). One whose value this build does not know is never answered: it is
    /// not recorded, the sandbox keeps the first such refusal for the run's error, and the
    /// caller stops the run with it.
    pub fn var(&mut self, name: &str) -> Result<Option<String>, EnvRefused> {
        // D96: `CARGO_MANIFEST_DIR` / `_PATH` from the package's mount row, the one fact the
        // sandbox serves its files by (seen_env.rs), never a value given apart from it.
        let row = self.package.row_var(name);
        let value = match self.package.env.get(name) {
            _ if row.is_some() => row,
            EnvAnswer::Set(value) => Some(value.to_owned()),
            EnvAnswer::Unset => None,
            EnvAnswer::Unknown => {
                let refused = EnvRefused { name: name.to_owned(), krate: self.package.env.crate_name().map(str::to_owned) };
                self.env_refused.get_or_insert_with(|| refused.clone());
                return Err(refused);
            }
        };
        self.record(format!("env:{name}"), value.as_deref().map(|it| digest(it.as_bytes())));
        Ok(value)
    }

    /// A `track_path` of the macro: the file's digest, recorded like an open.
    pub fn track_path(&mut self, path: &str) -> Result<(), Stop> {
        let Ok(rel) = self.resolve_at(ROOT_FD, path.trim_start_matches('/').as_bytes())? else {
            return Ok(());
        };
        let bytes = self.package.files.bytes(&rel).map_err(|why| self.refuse(why))?;
        self.record(format!("file:{rel}"), bytes.as_deref().map(digest));
        Ok(())
    }

    fn refuse(&mut self, why: String) -> Stop {
        let stop = Stop(why.clone());
        self.refused.get_or_insert((ProcLimit::Paths, why));
        stop
    }

    /// `path`, named from the directory descriptor `at` (`ROOT_FD` for `/`, or an open folder's
    /// descriptor), as a path from the served tree's root: `..` and `.` taken out; a path that
    /// leaves the tree, climbs above `/`, or passes an absolute path to a non-root descriptor is
    /// refused, naming it.
    fn resolve_at(&mut self, at: u32, path: &[u8]) -> Answer<String> {
        let mount = self.package.files.seen().mount();
        let dir = if at == ROOT_FD {
            None
        } else {
            let slot = at.checked_sub(ROOT_FD + 1).and_then(|i| usize::try_from(i).ok());
            match slot.and_then(|i| self.files.get(i)).and_then(Option::as_ref) {
                Some(Opened { dir: Some((folder, _)), .. }) => Some(folder.as_str()),
                Some(_) => return Ok(Err(Errno::NOTDIR)),
                None => return Ok(Err(Errno::BADF)),
            }
        };
        let Ok(text) = std::str::from_utf8(path) else {
            return Err(self.refuse(format!("named the path {:?}, which is not UTF-8", String::from_utf8_lossy(path))));
        };
        let joined;
        let text = if let Some(folder) = dir {
            if text.starts_with('/') {
                return Err(self.refuse(format!("named absolute path {text} from a directory descriptor, outside {mount}, the tree the macro is served")));
            }
            if text.is_empty() {
                return Ok(Err(Errno::NOENT));
            }
            joined = if folder.is_empty() { format!("{mount}/{text}") } else { format!("{mount}/{folder}/{text}") };
            joined.as_str()
        } else {
            text
        };
        let mut parts: Vec<&str> = Vec::new();
        for part in text.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    if parts.pop().is_none() {
                        return Err(self.refuse(format!("named {text:?}, which leaves the root")));
                    }
                }
                part => parts.push(part),
            }
        }
        // The one home of the mount points: `SeenRoot::in_tree` (a member's path is from its
        // workspace's folder, a dependency's from its package's).
        let full = format!("/{}", parts.join("/"));
        let rel = self.package.files.seen().in_tree(&full).map(str::to_owned);
        match rel {
            Some(rel) => Ok(Ok(rel)),
            // From the root `/`: a relative path the module asked (wasi-libc's, from its cwd) is
            // shown absolute, one written absolute as it was.
            None => {
                let slash = if text.starts_with('/') { "" } else { "/" };
                Err(self.refuse(format!("named {slash}{text}, outside {mount}, the tree the macro is served, the only one it may read")))
            }
        }
    }

    /// What `rel` is in the package, refused when a link takes it out of the package.
    fn found(&mut self, rel: &str, follow: bool) -> Result<Option<Found>, Stop> {
        self.package.files.find(rel, follow).map_err(|why| self.refuse(why))
    }

    fn opened(&mut self, fd: u32) -> Option<&mut Opened> {
        fd.checked_sub(ROOT_FD + 1).and_then(|at| self.files.get_mut(at as usize)).and_then(Option::as_mut)
    }

}

fn filestat(filetype: Filetype, size: u64) -> Filestat {
    Filestat { dev: 0, ino: 0, filetype, nlink: 1, size, atim: 0, mtim: 0, ctim: 0 }
}

impl System for Sandbox {
    fn args(&mut self) -> Result<(&[u8], usize), Stop> {
        Ok((&[], 0))
    }

    fn environ(&mut self) -> Result<(&[u8], usize), Stop> {
        // The variables are read one at a time (`getenv`); the list is empty, so a macro that
        // walks it reads nothing unrecorded.
        Ok((&[], 0))
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
            1 | 2 => Ok(Ok(bytes.len())),
            _ => Ok(Err(Errno::BADF)),
        }
    }

    fn read(&mut self, fd: u32, into: &mut [u8]) -> Answer<usize> {
        let Some(file) = self.opened(fd) else { return Ok(Err(Errno::BADF)) };
        if file.dir.is_some() {
            return Ok(Err(Errno::ISDIR));
        }
        let at = (file.pos as usize).min(file.bytes.len());
        let n = into.len().min(file.bytes.len() - at);
        into[..n].copy_from_slice(&file.bytes[at..at + n]);
        file.pos += n as u64;
        Ok(Ok(n))
    }

    fn fd_filetype(&mut self, fd: u32) -> Answer<Filetype> {
        Ok(match fd {
            0..=2 => Ok(Filetype::CharacterDevice),
            ROOT_FD => Ok(Filetype::Directory),
            _ => match self.opened(fd) {
                Some(file) if file.dir.is_some() => Ok(Filetype::Directory),
                Some(_) => Ok(Filetype::RegularFile),
                None => Err(Errno::BADF),
            },
        })
    }

    fn set_fd_flags(&mut self, _fd: u32, _flags: u32) -> Answer<()> {
        Ok(Ok(()))
    }

    fn close(&mut self, fd: u32) -> Answer<()> {
        let slot = fd.checked_sub(ROOT_FD + 1).and_then(|at| self.files.get_mut(at as usize));
        Ok(match slot.and_then(Option::take) {
            Some(_) => Ok(()),
            None => Err(Errno::BADF),
        })
    }

    fn seek(&mut self, fd: u32, offset: i64, whence: u32) -> Answer<u64> {
        let Some(file) = self.opened(fd) else { return Ok(Err(Errno::BADF)) };
        let base = match whence {
            0 => 0,
            1 => file.pos as i64,
            2 => file.bytes.len() as i64,
            _ => return Ok(Err(Errno::INVAL)),
        };
        let Some(pos) = base.checked_add(offset).filter(|it| *it >= 0) else { return Ok(Err(Errno::INVAL)) };
        file.pos = pos as u64;
        Ok(Ok(file.pos))
    }

    fn fd_stat(&mut self, fd: u32) -> Answer<Filestat> {
        Ok(match fd {
            0..=2 => Ok(filestat(Filetype::CharacterDevice, 0)),
            ROOT_FD => Ok(filestat(Filetype::Directory, 0)),
            _ => match self.opened(fd) {
                Some(file) if file.dir.is_some() => Ok(filestat(Filetype::Directory, 0)),
                Some(file) => Ok(filestat(Filetype::RegularFile, file.bytes.len() as u64)),
                None => Err(Errno::BADF),
            },
        })
    }

    fn open(&mut self, at: u32, path: &[u8], how: Open) -> Answer<u32> {
        let rel = match self.resolve_at(at, path)? {
            Ok(rel) => rel,
            Err(errno) => return Ok(Err(errno)),
        };
        if how.create() || how.truncate() || how.write() || how.append() {
            return Ok(Err(Errno::ROFS));
        }
        let Some(found) = self.found(&rel, true)? else {
            self.record(format!("stat:{rel}"), None);
            return Ok(Err(Errno::NOENT));
        };
        let opened = match found {
            Found::Folder => {
                let (canon, entries) = self.package.files.listing(&rel).map_err(|why| self.refuse(why))?;
                let Some(folder) = canon.to_str() else {
                    return Err(self.refuse(format!("opened {rel:?}, whose resolved path is not UTF-8")));
                };
                self.record(format!("dir:{rel}"), Some(listing_digest(&canon, &entries)));
                Opened { bytes: Vec::new(), pos: 0, dir: Some((folder.to_owned(), entries)) }
            }
            Found::File(size) => {
                if how.directory() {
                    self.record(format!("stat:{rel}"), Some(stat_digest(Filetype::RegularFile, size)));
                    return Ok(Err(Errno::NOTDIR));
                }
                let Some(bytes) = self.package.files.bytes(&rel).map_err(|why| self.refuse(why))? else {
                    self.record(format!("stat:{rel}"), None);
                    return Ok(Err(Errno::IO));
                };
                self.record(format!("file:{rel}"), Some(digest(&bytes)));
                Opened { bytes, pos: 0, dir: None }
            }
            Found::Link => {
                self.record(format!("lstat:{rel}"), Some(stat_digest(Filetype::SymbolicLink, 0)));
                return Ok(Err(Errno::LOOP));
            }
        };
        let slot = match self.files.iter().position(Option::is_none) {
            Some(slot) => slot,
            None => {
                self.files.push(None);
                self.files.len() - 1
            }
        };
        self.files[slot] = Some(opened);
        Ok(Ok(ROOT_FD + 1 + slot as u32))
    }

    fn path_stat(&mut self, at: u32, path: &[u8], follow: bool) -> Answer<Filestat> {
        let rel = match self.resolve_at(at, path)? {
            Ok(rel) => rel,
            Err(errno) => return Ok(Err(errno)),
        };
        if rel.is_empty() {
            return Ok(Ok(filestat(Filetype::Directory, 0)));
        }
        let found = match self.found(&rel, follow)? {
            Some(Found::Folder) => Some((Filetype::Directory, 0)),
            Some(Found::File(size)) => Some((Filetype::RegularFile, size)),
            Some(Found::Link) => Some((Filetype::SymbolicLink, 0)),
            None => None,
        };
        let kind = if follow { "stat" } else { "lstat" };
        self.record(format!("{kind}:{rel}"), found.map(|(filetype, size)| stat_digest(filetype, size)));
        Ok(match found {
            Some((filetype, size)) => Ok(filestat(filetype, size)),
            None => Err(Errno::NOENT),
        })
    }

    fn readlink(&mut self, _at: u32, _path: &[u8]) -> Answer<Vec<u8>> {
        // A link is followed by the host, inside the workspace, never shown.
        Ok(Err(Errno::INVAL))
    }

    fn dir_entry(&mut self, fd: u32, cookie: u64) -> Answer<Option<Dirent<'_>>> {
        let Some(file) = self.opened(fd) else { return Ok(Err(Errno::BADF)) };
        let Some((_, entries)) = &file.dir else { return Ok(Err(Errno::NOTDIR)) };
        Ok(Ok(entries.get(cookie as usize).map(|(name, filetype)| Dirent { ino: 0, filetype: *filetype, name })))
    }

    fn poll_oneoff(&mut self) -> Stop {
        Stop("waited (`poll_oneoff`, as `thread::sleep` does), which an expansion may not".to_owned())
    }

    fn exit(&mut self, code: u32) -> Stop {
        Stop(format!("exited with code {code} (`std::process::exit`), which an expansion may not"))
    }
}

#[cfg(test)]
mod seen_tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::{Files, Package, Sandbox};
    use crate::package_env::PackageEnv;
    use crate::script_sandbox::ScriptRoot;

    /// D96 (3): a member's macro runs in `/workspace/<dir>` and opens its workspace's files by
    /// their path from the workspace's folder; `/package` is not its tree. A dependency's runs
    /// in `/package`. The cargo book: "CARGO_MANIFEST_DIR — The directory containing the
    /// manifest of your package." No macro runs (plan only). Expected line:
    /// `test sandbox::seen_tests::a_member_s_macro_runs_in_its_workspace_dir ... ok`.
    #[test]
    fn a_member_s_macro_runs_in_its_workspace_dir() {
        let root = ScriptRoot::member("crates/m").unwrap_or_else(|e| panic!("{e}"));
        let member = Package::new(Files::Member { workspace: PathBuf::from("/nonexistent-ws"), root }, PackageEnv::default()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(member.cwd(), "/workspace/crates/m");
        let dependency = Package::new(Files::Listed(Arc::from(Vec::new())), PackageEnv::default()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(dependency.cwd(), "/package");
        let mut sandbox = Sandbox::new(Arc::new(member));
        assert_eq!(sandbox.resolve_at(blaze_wasi::ROOT_FD, b"/workspace/crates/m/Cargo.toml").ok().and_then(Result::ok).as_deref(), Some("crates/m/Cargo.toml"));
        assert_eq!(sandbox.resolve_at(blaze_wasi::ROOT_FD, b"workspace/Cargo.toml").ok().and_then(Result::ok).as_deref(), Some("Cargo.toml"));
        assert!(sandbox.resolve_at(blaze_wasi::ROOT_FD, b"/package/Cargo.toml").is_err(), "a member's macro is not served /package");
        let mut dep = Sandbox::new(Arc::new(dependency));
        assert_eq!(dep.resolve_at(blaze_wasi::ROOT_FD, b"/package/src/lib.rs").ok().and_then(Result::ok).as_deref(), Some("src/lib.rs"));
        assert!(dep.resolve_at(blaze_wasi::ROOT_FD, b"/workspace/x").is_err());

        // Package-local submodules in `Files::Listed` appear as directories in parent listings
        // and fail closed on find/bytes/listing.
        let oid = [0xcd; 20];
        let with_sub = Files::Listed(Arc::from([
            (String::from("Cargo.toml"), super::ListedBytes::Static(b"[package]\n")),
            (String::from("v8"), super::ListedBytes::Submodule(oid)),
        ]));
        assert!(with_sub.bytes("Cargo.toml").expect("manifest").is_some());
        let (_, root_list) = with_sub.listing("").expect("root listing");
        assert!(root_list.iter().any(|(name, ft)| name.as_slice() == b"v8" && *ft == blaze_wasi::Filetype::Directory));
        for bad in ["v8", "v8/include/v8.h"] {
            assert!(with_sub.find(bad, true).is_err(), "{bad} find must fail closed");
            assert!(with_sub.find(bad, false).is_err(), "{bad} lstat find must fail closed");
            assert!(with_sub.bytes(bad).is_err(), "{bad} bytes must fail closed");
            assert!(with_sub.listing(bad).is_err(), "{bad} listing must fail closed");
        }
        let sub_pkg = Package::new(with_sub, PackageEnv::default()).unwrap_or_else(|e| panic!("{e}"));
        assert!(super::answer_now(&sub_pkg, "file:v8/include/v8.h").is_err(), "answer_now file must propagate submodule refusal");
        assert!(super::answer_now(&sub_pkg, "dir:v8").is_err(), "answer_now dir must propagate submodule refusal");
        assert!(super::answer_now(&sub_pkg, "stat:v8").is_err(), "answer_now stat must propagate submodule refusal");
        assert!(super::answer_now(&sub_pkg, "lstat:v8").is_err(), "answer_now lstat must propagate submodule refusal");
    }

    /// MX (2026-11-17): every proc-macro call written in a workspace member at a non-empty dir
    /// was refused, `Limit(Paths)` (port27/mini/serde: `crates/app` deriving `Serialize`; tempo:
    /// 321 call groups), while the same crate as a root package expanded. The command serves a
    /// member's workspace from the checkout as `FetchEnv::checkout` names it, `""` when blaze
    /// runs in the workspace's root (blaze-cli proc_callers.rs `shown`), and the shim, at each
    /// call, enters `CARGO_MANIFEST_DIR` (`std::env::set_current_dir("/workspace/<dir>")`, which
    /// wasi-libc's `chdir` asks as `path_filestat_get(root fd, "workspace/<dir>")`). For dir `""`
    /// that stat is the tree's root and asks no file; for `crates/app` it asked `Files::real`,
    /// whose `canonicalize("")` is `ENOENT`: refused. The Reference, "Procedural Macros": a proc
    /// macro is run on the crate that invokes it; cargo's `CARGO_MANIFEST_DIR` is "the directory
    /// containing the manifest of your package", wherever the package is in its workspace.
    ///
    /// Here the workspace is `""` as the command gives it: cargo runs a test in its package's
    /// folder (crates/blaze-proc-macro), so the member is its `src`. The shim's calls, in order:
    /// the stat of the member's folder, an open of a file the macro names relative to it (as
    /// wasi-libc joins it to its cwd), a stat of the member's folder by its absolute path.
    /// CONTROLS: a path above the workspace is still refused by name, as the macro wrote it;
    /// with `Files::real`'s `""` arm taken out the first assertion fails with "cannot be
    /// resolved". Expected line:
    /// `test sandbox::seen_tests::a_member_of_a_checkout_named_here_is_entered_and_read ... ok`.
    #[test]
    fn a_member_of_a_checkout_named_here_is_entered_and_read() {
        use blaze_wasi::{Filetype, Open, ROOT_FD, System};
        let root = ScriptRoot::member("src").unwrap_or_else(|e| panic!("{e}"));
        let member = Package::new(Files::Member { workspace: PathBuf::new(), root }, PackageEnv::default()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(member.cwd(), "/workspace/src");
        let mut sandbox = Sandbox::new(Arc::new(member));
        let entered = sandbox.path_stat(ROOT_FD, b"workspace/src", true);
        assert!(
            matches!(&entered, Ok(Ok(stat)) if stat.filetype == Filetype::Directory),
            "the shim's chdir into the member's folder: {:?}, refused {:?}",
            entered.as_ref().map_err(|stop| &stop.0).map(|it| it.as_ref().map(|s| s.filetype)),
            sandbox.refused
        );
        let how = Open { dirflags: 1, oflags: 0, rights_base: 1 << 1, rights_inheriting: 0, fdflags: 0 };
        let fd = sandbox.open(ROOT_FD, b"workspace/src/sandbox.rs", how);
        let Ok(Ok(fd)) = fd else { panic!("the member's file: {:?}, refused {:?}", fd.map_err(|stop| stop.0), sandbox.refused) };
        let mut head = [0u8; 3];
        assert!(matches!(sandbox.read(fd, &mut head), Ok(Ok(3))));
        assert_eq!(&head, b"//!", "the member's own file is read");
        assert!(matches!(sandbox.path_stat(ROOT_FD, b"/workspace/src", true), Ok(Ok(_))));
        assert!(sandbox.refused.is_none(), "{:?}", sandbox.refused);
        // The controls: climbing out of the workspace is refused, naming the path as written.
        let above = sandbox.path_stat(ROOT_FD, b"/workspace/../etc/passwd", true);
        let Err(stop) = above else { panic!("a path above the workspace was answered") };
        assert!(stop.0.contains("/workspace/../etc/passwd") && stop.0.contains("outside /workspace"), "{}", stop.0);
        assert!(!stop.0.contains(env!("CARGO_MANIFEST_DIR")), "the words hold no path of this machine: {}", stop.0);
    }
}

#[cfg(test)]
mod dir_fd_tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use blaze_expand::ProcLimit;
    use blaze_wasi::{Errno, Filetype, Open, ROOT_FD, System};

    use super::{Files, ListedBytes, Package, Sandbox, answer_now};
    use crate::package_env::PackageEnv;
    use crate::script_sandbox::ScriptRoot;

    const READ_ONLY: Open = Open { dirflags: 1, oflags: 0, rights_base: 1 << 1, rights_inheriting: 0, fdflags: 0 };
    const WRITE_ONLY: Open = Open { dirflags: 1, oflags: 1, rights_base: 1 << 6, rights_inheriting: 0, fdflags: 0 };

    fn read_all(sandbox: &mut Sandbox, fd: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            match sandbox.read(fd, &mut buf) {
                Ok(Ok(0)) => return out,
                Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
                other => panic!("unexpected read result: {other:?}"),
            }
        }
    }

    fn collect_dir_entries(sandbox: &mut Sandbox, fd: u32) -> Vec<(Vec<u8>, Filetype)> {
        let mut out = Vec::new();
        let mut cookie = 0u64;
        loop {
            match sandbox.dir_entry(fd, cookie) {
                Ok(Ok(Some(ent))) => {
                    out.push((ent.name.to_vec(), ent.filetype));
                    cookie += 1;
                }
                Ok(Ok(None)) => return out,
                Ok(Err(errno)) => panic!("unexpected dir_entry errno: {errno:?}"),
                Err(stop) => panic!("unexpected dir_entry stop: {stop}"),
            }
        }
    }

    struct TempTree {
        base: PathBuf,
        ws: PathBuf,
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn make_workspace(tag: &str) -> TempTree {
        static COUNT: AtomicU32 = AtomicU32::new(0);
        let n = COUNT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("blaze-sandbox-dirfd-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ws = base.join("ws");
        std::fs::create_dir_all(base.join("outside")).expect("outside dir");
        std::fs::write(base.join("outside/secret.txt"), b"top-secret").expect("outside file");
        for (rel, content) in [
            ("Cargo.toml", b"[workspace]\nmembers = [\"crates/m\"]\n".as_slice()),
            ("crates/m/Cargo.toml", b"[package]\nname = \"m\"\n".as_slice()),
            ("crates/m/parts/a.rs", b"pub fn a() {}\n".as_slice()),
            ("crates/m/parts/sub/b.rs", b"pub fn b() {}\n".as_slice()),
            ("crates/m/parts/sub/deep/c.rs", b"pub fn c() {}\n".as_slice()),
        ] {
            let at = ws.join(rel);
            std::fs::create_dir_all(at.parent().expect("parent")).expect("dir");
            std::fs::write(&at, content).expect("file");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink("sub/b.rs", ws.join("crates/m/parts/inside_link")).expect("inside symlink");
            symlink("../../../../outside/secret.txt", ws.join("crates/m/parts/escape")).expect("escape symlink");
        }
        TempTree { base, ws }
    }

    fn member_sandbox(tree: &TempTree) -> (Arc<Package>, Sandbox) {
        let root = ScriptRoot::member("crates/m").unwrap_or_else(|e| panic!("{e}"));
        let pkg = Arc::new(
            Package::new(Files::Member { workspace: tree.ws.clone(), root }, PackageEnv::default())
                .unwrap_or_else(|e| panic!("{e}")),
        );
        let sandbox = Sandbox::new(Arc::clone(&pkg));
        (pkg, sandbox)
    }

    /// Positive: open a directory via `ROOT_FD`, iterate `fd_readdir`, then call `path_stat` and
    /// `open` relative to the opened dir fd on a child file and nested subdirectories
    /// (`dir_fd -> sub_fd -> deep_fd -> c.rs`), verifying bytes AND Rule 21 recorded read parity.
    #[test]
    fn dir_fd_readdir_stat_and_nested_open_record_normalized_reads() {
        let tree = make_workspace("pos");
        let (pkg, mut sandbox) = member_sandbox(&tree);

        let dir_fd = sandbox
            .open(ROOT_FD, b"/workspace/crates/m/parts", READ_ONLY)
            .expect("not stopped")
            .expect("parts dir opens");
        let entries = collect_dir_entries(&mut sandbox, dir_fd);
        assert!(entries.iter().any(|(name, ft)| name.as_slice() == b"a.rs" && *ft == Filetype::RegularFile));
        assert!(entries.iter().any(|(name, ft)| name.as_slice() == b"sub" && *ft == Filetype::Directory));

        // Child file: path_stat + open relative to dir_fd
        let a_stat = sandbox.path_stat(dir_fd, b"a.rs", false).expect("not stopped").expect("a.rs stat");
        assert_eq!(a_stat.filetype, Filetype::RegularFile);
        assert_eq!(a_stat.size, b"pub fn a() {}\n".len() as u64);
        let a_fd = sandbox.open(dir_fd, b"a.rs", READ_ONLY).expect("not stopped").expect("a.rs open");
        assert_eq!(read_all(&mut sandbox, a_fd), b"pub fn a() {}\n");

        // Nested subdir: dir_fd -> sub_fd -> b.rs AND sub_fd -> deep_fd -> c.rs
        let sub_stat = sandbox.path_stat(dir_fd, b"sub", false).expect("not stopped").expect("sub stat");
        assert_eq!(sub_stat.filetype, Filetype::Directory);
        let sub_fd = sandbox.open(dir_fd, b"sub", READ_ONLY).expect("not stopped").expect("sub open");
        let sub_entries = collect_dir_entries(&mut sandbox, sub_fd);
        assert_eq!(sub_entries, vec![(b"b.rs".to_vec(), Filetype::RegularFile), (b"deep".to_vec(), Filetype::Directory)]);

        let b_fd = sandbox.open(sub_fd, b"b.rs", READ_ONLY).expect("not stopped").expect("b.rs open");
        assert_eq!(read_all(&mut sandbox, b_fd), b"pub fn b() {}\n");

        let deep_fd = sandbox.open(sub_fd, b"deep", READ_ONLY).expect("not stopped").expect("deep open");
        let c_stat = sandbox.path_stat(deep_fd, b"c.rs", true).expect("not stopped").expect("c.rs stat");
        assert_eq!(c_stat.filetype, Filetype::RegularFile);
        assert_eq!(c_stat.size, b"pub fn c() {}\n".len() as u64);
        let c_fd = sandbox.open(deep_fd, b"c.rs", READ_ONLY).expect("not stopped").expect("c.rs open");
        assert_eq!(read_all(&mut sandbox, c_fd), b"pub fn c() {}\n");

        // Inside-workspace symlink relative to dir_fd succeeds and records normalized path.
        #[cfg(unix)]
        {
            let link_fd = sandbox.open(dir_fd, b"inside_link", READ_ONLY).expect("not stopped").expect("inside symlink");
            assert_eq!(read_all(&mut sandbox, link_fd), b"pub fn b() {}\n");
        }

        // Absent child file/stat relative to dir_fd returns ENOENT and records type-sensitive absence.
        assert_eq!(sandbox.open(dir_fd, b"missing.rs", READ_ONLY).expect("not stopped"), Err(Errno::NOENT));
        assert_eq!(sandbox.path_stat(dir_fd, b"missing_stat.rs", true).expect("not stopped"), Err(Errno::NOENT));

        // Normalized read tracking and Rule 21 `answer_now` parity.
        let subjects: Vec<&str> = sandbox.reads.iter().map(|r| r.subject.as_str()).collect();
        for expected in [
            "dir:crates/m/parts",
            "lstat:crates/m/parts/a.rs",
            "file:crates/m/parts/a.rs",
            "lstat:crates/m/parts/sub",
            "dir:crates/m/parts/sub",
            "file:crates/m/parts/sub/b.rs",
            "dir:crates/m/parts/sub/deep",
            "stat:crates/m/parts/sub/deep/c.rs",
            "file:crates/m/parts/sub/deep/c.rs",
            "stat:crates/m/parts/missing.rs",
            "stat:crates/m/parts/missing_stat.rs",
        ] {
            assert!(subjects.contains(&expected), "missing recorded read {expected:?} in {subjects:?}");
        }
        for read in &sandbox.reads {
            assert_eq!(
                answer_now(&pkg, &read.subject),
                Ok(read.digest),
                "answer_now parity mismatch for {}",
                read.subject
            );
        }
        assert!(sandbox.refused.is_none(), "{:?}", sandbox.refused);
    }

    /// Positive: opening `"."` at the mount root (via `ROOT_FD` and via an open root dir fd where
    /// `folder` is empty) and relative to a subdirectory fd works cleanly on `Files::Listed`,
    /// while `open(ROOT_FD, b".")` stays refused (outside `/package`).
    #[test]
    fn dot_open_and_stat_work_at_mount_root_and_subdir_fd() {
        let listed = Files::Listed(Arc::from([
            (String::from("Cargo.toml"), ListedBytes::Static(b"[package]\nname = \"dep\"\n")),
            (String::from("src/lib.rs"), ListedBytes::Static(b"pub mod sub;\n")),
            (String::from("src/sub/mod.rs"), ListedBytes::Static(b"pub const X: u32 = 1;\n")),
        ]));
        let pkg = Arc::new(Package::new(listed, PackageEnv::default()).unwrap_or_else(|e| panic!("{e}")));
        let mut sandbox = Sandbox::new(Arc::clone(&pkg));

        // Opening `/package/.` via ROOT_FD opens the package root (folder == "").
        let root_fd = sandbox.open(ROOT_FD, b"/package/.", READ_ONLY).expect("not stopped").expect("package/. opens");
        let root_entries = collect_dir_entries(&mut sandbox, root_fd);
        assert_eq!(
            root_entries,
            vec![(b"Cargo.toml".to_vec(), Filetype::RegularFile), (b"src".to_vec(), Filetype::Directory)]
        );

        // Opening and statting "." relative to the root dir fd (empty folder).
        let dot_root_fd = sandbox.open(root_fd, b".", READ_ONLY).expect("not stopped").expect("root_fd . opens");
        assert_eq!(collect_dir_entries(&mut sandbox, dot_root_fd), root_entries);
        let dot_root_stat = sandbox.path_stat(root_fd, b".", true).expect("not stopped").expect("root_fd . stat");
        assert_eq!(dot_root_stat.filetype, Filetype::Directory);

        let manifest_fd = sandbox.open(root_fd, b"Cargo.toml", READ_ONLY).expect("not stopped").expect("Cargo.toml");
        assert_eq!(read_all(&mut sandbox, manifest_fd), b"[package]\nname = \"dep\"\n");

        // Opening and statting "." relative to a subdirectory fd, plus nested traversal.
        let src_fd = sandbox.open(root_fd, b"src", READ_ONLY).expect("not stopped").expect("src opens");
        let dot_src_fd = sandbox.open(src_fd, b".", READ_ONLY).expect("not stopped").expect("src_fd . opens");
        assert_eq!(
            collect_dir_entries(&mut sandbox, dot_src_fd),
            vec![(b"lib.rs".to_vec(), Filetype::RegularFile), (b"sub".to_vec(), Filetype::Directory)]
        );
        let dot_src_stat = sandbox.path_stat(src_fd, b".", true).expect("not stopped").expect("src_fd . stat");
        assert_eq!(dot_src_stat.filetype, Filetype::Directory);

        let sub_fd = sandbox.open(src_fd, b"sub", READ_ONLY).expect("not stopped").expect("sub opens");
        let mod_fd = sandbox.open(sub_fd, b"mod.rs", READ_ONLY).expect("not stopped").expect("mod.rs opens");
        assert_eq!(read_all(&mut sandbox, mod_fd), b"pub const X: u32 = 1;\n");

        // `answer_now` parity for all recorded reads.
        for read in &sandbox.reads {
            assert_eq!(answer_now(&pkg, &read.subject), Ok(read.digest), "parity for {}", read.subject);
        }

        // Control: `ROOT_FD` itself is `/`, outside `/package`, so `open(ROOT_FD, b".")` stays refused.
        let Err(stop) = sandbox.open(ROOT_FD, b".", READ_ONLY) else {
            panic!("opening `/` via ROOT_FD `.` must be refused");
        };
        assert!(stop.0.contains("outside /package"), "{}", stop.0);
        assert!(matches!(sandbox.refused, Some((ProcLimit::Paths, _))));
    }

    /// Negative security controls: invalid/closed fd (`BADF`), regular file fd (`NOTDIR`),
    /// empty path (`NOENT`), write flags (`ROFS`), `..` traversal outside workspace, absolute
    /// path via subdir fd, and symlink inside subdir pointing outside workspace are all blocked.
    #[test]
    fn dir_fd_negative_security_controls() {
        let tree = make_workspace("neg");
        let ws_str = tree.ws.to_string_lossy().into_owned();

        // 1. Invalid, stdio, and closed fds -> Errno::BADF; regular file fd -> Errno::NOTDIR;
        //    empty path -> Errno::NOENT; write flags -> Errno::ROFS.
        {
            let (_pkg, mut sandbox) = member_sandbox(&tree);
            for bad_fd in [0, 1, 2, 99] {
                assert_eq!(sandbox.open(bad_fd, b"a.rs", READ_ONLY).expect("not stopped"), Err(Errno::BADF));
                assert_eq!(sandbox.path_stat(bad_fd, b"a.rs", true).expect("not stopped"), Err(Errno::BADF));
            }
            let dir_fd = sandbox.open(ROOT_FD, b"/workspace/crates/m/parts", READ_ONLY).expect("not stopped").expect("dir");
            let file_fd = sandbox.open(dir_fd, b"a.rs", READ_ONLY).expect("not stopped").expect("file");

            // Regular file fd used as dir fd -> NOTDIR.
            assert_eq!(sandbox.open(file_fd, b"b.rs", READ_ONLY).expect("not stopped"), Err(Errno::NOTDIR));
            assert_eq!(sandbox.path_stat(file_fd, b"b.rs", true).expect("not stopped"), Err(Errno::NOTDIR));

            // Empty path on dir fd -> NOENT; write flags on dir fd -> ROFS.
            assert_eq!(sandbox.open(dir_fd, b"", READ_ONLY).expect("not stopped"), Err(Errno::NOENT));
            assert_eq!(sandbox.path_stat(dir_fd, b"", true).expect("not stopped"), Err(Errno::NOENT));
            assert_eq!(sandbox.open(dir_fd, b"a.rs", WRITE_ONLY).expect("not stopped"), Err(Errno::ROFS));

            // Closed dir fd -> BADF.
            assert_eq!(sandbox.close(dir_fd).expect("not stopped"), Ok(()));
            assert_eq!(sandbox.open(dir_fd, b"a.rs", READ_ONLY).expect("not stopped"), Err(Errno::BADF));
            assert_eq!(sandbox.path_stat(dir_fd, b"a.rs", true).expect("not stopped"), Err(Errno::BADF));
            assert!(sandbox.refused.is_none());
        }

        // 2. Path traversal (`..`) escaping `/workspace` via subdir fd -> refused with ProcLimit::Paths.
        for escape_path in [b"../../../../outside/secret.txt".as_slice(), b"../../../../../../../etc/passwd".as_slice()] {
            let (_pkg, mut sandbox) = member_sandbox(&tree);
            let dir_fd = sandbox.open(ROOT_FD, b"/workspace/crates/m/parts", READ_ONLY).expect("not stopped").expect("dir");
            let Err(open_stop) = sandbox.open(dir_fd, escape_path, READ_ONLY) else {
                panic!("traversal open {:?} must be refused", std::str::from_utf8(escape_path));
            };
            assert!(!open_stop.0.contains(&ws_str), "must not leak host path: {}", open_stop.0);
            assert!(matches!(sandbox.refused, Some((ProcLimit::Paths, _))));

            let (_pkg, mut sandbox) = member_sandbox(&tree);
            let dir_fd = sandbox.open(ROOT_FD, b"/workspace/crates/m/parts", READ_ONLY).expect("not stopped").expect("dir");
            let Err(stat_stop) = sandbox.path_stat(dir_fd, escape_path, true) else {
                panic!("traversal stat {:?} must be refused", std::str::from_utf8(escape_path));
            };
            assert!(!stat_stop.0.contains(&ws_str), "must not leak host path: {}", stat_stop.0);
            assert!(matches!(sandbox.refused, Some((ProcLimit::Paths, _))));
        }

        // 3. Traversal escaping `/workspace` from root dir fd (`folder == ""`) -> refused.
        {
            let (_pkg, mut sandbox) = member_sandbox(&tree);
            let root_dir_fd = sandbox.open(ROOT_FD, b"/workspace", READ_ONLY).expect("not stopped").expect("ws root");
            let Err(stop) = sandbox.open(root_dir_fd, b"../outside/secret.txt", READ_ONLY) else {
                panic!("root dir fd `..` escape must be refused");
            };
            assert!(stop.0.contains("outside /workspace"), "{}", stop.0);
            assert!(matches!(sandbox.refused, Some((ProcLimit::Paths, _))));
        }

        // 4. Absolute paths via subdir fd -> refused (even if pointing under `/workspace`).
        for abs_path in [b"/etc/passwd".as_slice(), b"/workspace/crates/m/parts/a.rs".as_slice()] {
            let (_pkg, mut sandbox) = member_sandbox(&tree);
            let dir_fd = sandbox.open(ROOT_FD, b"/workspace/crates/m/parts", READ_ONLY).expect("not stopped").expect("dir");
            let Err(open_stop) = sandbox.open(dir_fd, abs_path, READ_ONLY) else {
                panic!("absolute open {:?} via dir_fd must be refused", std::str::from_utf8(abs_path));
            };
            assert!(open_stop.0.contains("absolute path"), "{}", open_stop.0);
            assert!(matches!(sandbox.refused, Some((ProcLimit::Paths, _))));

            let (_pkg, mut sandbox) = member_sandbox(&tree);
            let dir_fd = sandbox.open(ROOT_FD, b"/workspace/crates/m/parts", READ_ONLY).expect("not stopped").expect("dir");
            let Err(stat_stop) = sandbox.path_stat(dir_fd, abs_path, true) else {
                panic!("absolute stat {:?} via dir_fd must be refused", std::str::from_utf8(abs_path));
            };
            assert!(stat_stop.0.contains("absolute path"), "{}", stat_stop.0);
            assert!(matches!(sandbox.refused, Some((ProcLimit::Paths, _))));
        }

        // 5. Symlink inside subdir pointing outside workspace accessed via subdir fd:
        //    `open` and `path_stat(..., follow=true)` are refused; `path_stat(..., follow=false)`
        //    preserves final-component symlink metadata without dereferencing the target.
        #[cfg(unix)]
        {
            let (_pkg, mut sandbox) = member_sandbox(&tree);
            let dir_fd = sandbox.open(ROOT_FD, b"/workspace/crates/m/parts", READ_ONLY).expect("not stopped").expect("dir");
            let Err(stat_stop) = sandbox.path_stat(dir_fd, b"escape", true) else {
                panic!("symlink escape stat (follow=true) must be refused");
            };
            assert!(stat_stop.0.contains("a link out of /workspace"), "{}", stat_stop.0);
            assert!(!stat_stop.0.contains(&ws_str), "must not leak host path: {}", stat_stop.0);
            assert!(matches!(sandbox.refused, Some((ProcLimit::Paths, _))));

            let (_pkg, mut sandbox) = member_sandbox(&tree);
            let dir_fd = sandbox.open(ROOT_FD, b"/workspace/crates/m/parts", READ_ONLY).expect("not stopped").expect("dir");
            let lstat = sandbox.path_stat(dir_fd, b"escape", false).expect("lstat does not dereference target").expect("symlink exists");
            assert_eq!(lstat.filetype, Filetype::SymbolicLink);
            assert_eq!(lstat.size, 0, "symlink size must not leak target path length");

            let (_pkg, mut sandbox) = member_sandbox(&tree);
            let dir_fd = sandbox.open(ROOT_FD, b"/workspace/crates/m/parts", READ_ONLY).expect("not stopped").expect("dir");
            let Err(open_stop) = sandbox.open(dir_fd, b"escape", READ_ONLY) else {
                panic!("symlink escape open must be refused");
            };
            assert!(open_stop.0.contains("a link out of /workspace"), "{}", open_stop.0);
            assert!(!open_stop.0.contains(&ws_str), "must not leak host path: {}", open_stop.0);
            assert!(matches!(sandbox.refused, Some((ProcLimit::Paths, _))));
        }
    }

    /// Codex P1: an absent `open` (via `ROOT_FD` or via dir-fd) records a type-sensitive `stat:`
    /// read so that when the absent path later becomes a directory (or file), `answer_now` changes
    /// and invalidates any cached expansion. Likewise `O_DIRECTORY` on a regular file (`NOTDIR`)
    /// records a type-sensitive `stat:` read that changes if replaced by a directory.
    #[test]
    fn absent_open_and_notdir_record_type_sensitive_reads_that_invalidate_on_dir_creation() {
        let tree = make_workspace("absent_becomes_dir");
        let (pkg, mut sandbox) = member_sandbox(&tree);
        let dir_fd = sandbox.open(ROOT_FD, b"/workspace/crates/m/parts", READ_ONLY).expect("not stopped").expect("dir");

        // Probe absent paths via both dir-fd and ROOT_FD.
        assert_eq!(sandbox.open(dir_fd, b"future_dir", READ_ONLY).expect("not stopped"), Err(Errno::NOENT));
        assert_eq!(sandbox.open(ROOT_FD, b"/workspace/crates/m/parts/future_root_dir", READ_ONLY).expect("not stopped"), Err(Errno::NOENT));

        // Probe regular file with O_DIRECTORY -> NOTDIR.
        let dir_only = Open { dirflags: 1, oflags: 2, rights_base: 1 << 1, rights_inheriting: 0, fdflags: 0 };
        assert_eq!(sandbox.open(dir_fd, b"a.rs", dir_only).expect("not stopped"), Err(Errno::NOTDIR));

        let future_dir_read = sandbox.reads.iter().find(|r| r.subject == "stat:crates/m/parts/future_dir").expect("stat read for dir-fd absent open").clone();
        let future_root_read = sandbox.reads.iter().find(|r| r.subject == "stat:crates/m/parts/future_root_dir").expect("stat read for ROOT_FD absent open").clone();
        let notdir_read = sandbox.reads.iter().find(|r| r.subject == "stat:crates/m/parts/a.rs").expect("stat read for NOTDIR open").clone();
        assert_eq!(future_dir_read.digest, None);
        assert_eq!(future_root_read.digest, None);
        assert!(notdir_read.digest.is_some());
        assert_eq!(answer_now(&pkg, &future_dir_read.subject), Ok(None));
        assert_eq!(answer_now(&pkg, &future_root_read.subject), Ok(None));
        assert_eq!(answer_now(&pkg, &notdir_read.subject), Ok(notdir_read.digest));

        // Turn both absent paths into directories, and replace `a.rs` with a directory.
        std::fs::create_dir(tree.ws.join("crates/m/parts/future_dir")).expect("create future_dir");
        std::fs::create_dir(tree.ws.join("crates/m/parts/future_root_dir")).expect("create future_root_dir");
        std::fs::remove_file(tree.ws.join("crates/m/parts/a.rs")).expect("remove a.rs");
        std::fs::create_dir(tree.ws.join("crates/m/parts/a.rs")).expect("dir at a.rs");

        // Every recorded read must now disagree with `answer_now` (cache invalidation!).
        assert_ne!(answer_now(&pkg, &future_dir_read.subject), Ok(future_dir_read.digest), "absent dir-fd open must invalidate when path becomes a directory");
        assert_ne!(answer_now(&pkg, &future_root_read.subject), Ok(future_root_read.digest), "absent ROOT_FD open must invalidate when path becomes a directory");
        assert_ne!(answer_now(&pkg, &notdir_read.subject), Ok(notdir_read.digest), "NOTDIR open must invalidate when file becomes a directory");
    }

    /// Codex P2 (symlink dir-fd `..` resolution): opening a directory through an in-tree symlink
    /// `alias -> real/sub` stores the canonical in-tree target `crates/m/real/sub` on the opened
    /// dir fd, so `../sibling.txt` resolves to `crates/m/real/sibling.txt` (NOT `crates/m/sibling.txt`),
    /// retargeting `alias` invalidates the `dir:` digest even if the entries are identical, and
    /// `..` climbing out of `/workspace` is still refused.
    #[cfg(unix)]
    #[test]
    fn symlink_dir_fd_resolves_dotdot_from_canonical_target_and_blocks_escapes() {
        use std::os::unix::fs::symlink;

        let tree = make_workspace("symlink_dirfd");
        let m = tree.ws.join("crates/m");
        std::fs::create_dir_all(m.join("real/sub")).expect("real/sub");
        std::fs::create_dir_all(m.join("other/sub")).expect("other/sub");
        std::fs::write(m.join("real/sub/item.rs"), b"real item").expect("real item");
        std::fs::write(m.join("other/sub/item.rs"), b"other item").expect("other item");
        std::fs::write(m.join("real/sibling.txt"), b"from-real-sibling").expect("real sibling");
        std::fs::write(m.join("sibling.txt"), b"from-wrong-lexical-sibling").expect("lexical sibling");
        symlink("real/sub", m.join("alias")).expect("alias symlink");

        let (pkg, mut sandbox) = member_sandbox(&tree);
        let alias_fd = sandbox
            .open(ROOT_FD, b"/workspace/crates/m/alias", READ_ONLY)
            .expect("not stopped")
            .expect("alias dir opens");

        // `../sibling.txt` relative to `alias` (`-> real/sub`) must read `real/sibling.txt`, NOT `sibling.txt`.
        let sib_fd = sandbox.open(alias_fd, b"../sibling.txt", READ_ONLY).expect("not stopped").expect("../sibling.txt opens");
        assert_eq!(read_all(&mut sandbox, sib_fd), b"from-real-sibling");
        assert!(
            sandbox.reads.iter().any(|r| r.subject == "file:crates/m/real/sibling.txt"),
            "read key must record resolved target path, got {:?}",
            sandbox.reads
        );
        assert!(
            !sandbox.reads.iter().any(|r| r.subject == "file:crates/m/sibling.txt"),
            "must not read lexical sibling"
        );
        for read in &sandbox.reads {
            assert_eq!(answer_now(&pkg, &read.subject), Ok(read.digest), "parity for {}", read.subject);
        }

        // Retargeting `alias` to `other/sub` (which has the exact same entry name `item.rs`!)
        // must still change `answer_now("dir:crates/m/alias")` so cached expansions invalidate.
        let alias_dir_read = sandbox.reads.iter().find(|r| r.subject == "dir:crates/m/alias").expect("dir read").clone();
        std::fs::remove_file(m.join("alias")).expect("unlink alias");
        symlink("other/sub", m.join("alias")).expect("retarget alias");
        assert_ne!(
            answer_now(&pkg, &alias_dir_read.subject),
            Ok(alias_dir_read.digest),
            "retargeting directory symlink must invalidate dir read even when listings match"
        );

        // Escape control: climbing past `/workspace` via `alias_fd` (canonical depth 4:
        // `crates/m/real/sub`) is still refused.
        let Err(stop) = sandbox.open(alias_fd, b"../../../../../outside/secret.txt", READ_ONLY) else {
            panic!("escape via symlink dir fd must be refused");
        };
        assert!(stop.0.contains("outside /workspace"), "{}", stop.0);
        assert!(matches!(sandbox.refused, Some((ProcLimit::Paths, _))));
    }

    /// Codex P2 (`follow=false` on `path_stat`): `path_stat(..., follow=false)` preserves
    /// final-component `Filetype::SymbolicLink` for directory symlinks and dangling symlinks
    /// (both via `ROOT_FD` and via dir-fd), while `follow=true` follows directory symlinks to
    /// `Directory`, reports `NOENT` for dangling symlinks, and refuses out-of-tree targets.
    /// Intermediate directory symlinks escaping the tree are refused even when `follow=false`.
    #[cfg(unix)]
    #[test]
    fn path_stat_honors_no_follow_for_dir_and_dangling_symlinks_and_blocks_intermediate_escapes() {
        use std::os::unix::fs::symlink;

        let tree = make_workspace("nofollow");
        let ws_str = tree.ws.to_string_lossy().into_owned();
        let parts = tree.ws.join("crates/m/parts");
        symlink("sub", parts.join("dir_link")).expect("dir symlink");
        symlink("does_not_exist", parts.join("dangling")).expect("dangling symlink");
        symlink("../../../../outside", parts.join("out_dir_link")).expect("out-of-tree dir symlink");

        let (pkg, mut sandbox) = member_sandbox(&tree);
        let dir_fd = sandbox.open(ROOT_FD, b"/workspace/crates/m/parts", READ_ONLY).expect("not stopped").expect("dir");

        // `fd_readdir` listing also reports symlinks as `SymbolicLink`, not `Directory`/`RegularFile`.
        let entries = collect_dir_entries(&mut sandbox, dir_fd);
        assert!(entries.iter().any(|(n, ft)| n.as_slice() == b"dir_link" && *ft == Filetype::SymbolicLink), "{entries:?}");
        assert!(entries.iter().any(|(n, ft)| n.as_slice() == b"dangling" && *ft == Filetype::SymbolicLink), "{entries:?}");

        // Directory symlink: follow=false -> SymbolicLink (`lstat:`), follow=true -> Directory (`stat:`).
        let lstat_dir = sandbox.path_stat(dir_fd, b"dir_link", false).expect("not stopped").expect("dir_link lstat");
        assert_eq!(lstat_dir.filetype, Filetype::SymbolicLink);
        let stat_dir = sandbox.path_stat(dir_fd, b"dir_link", true).expect("not stopped").expect("dir_link stat");
        assert_eq!(stat_dir.filetype, Filetype::Directory);
        // Same via ROOT_FD.
        let root_lstat_dir = sandbox.path_stat(ROOT_FD, b"/workspace/crates/m/parts/dir_link", false).expect("not stopped").expect("root dir_link lstat");
        assert_eq!(root_lstat_dir.filetype, Filetype::SymbolicLink);

        // Dangling symlink: follow=false -> SymbolicLink (`lstat:`), follow=true -> NOENT (`stat:`).
        let lstat_dangling = sandbox.path_stat(dir_fd, b"dangling", false).expect("not stopped").expect("dangling lstat");
        assert_eq!(lstat_dangling.filetype, Filetype::SymbolicLink);
        assert_eq!(sandbox.path_stat(dir_fd, b"dangling", true).expect("not stopped"), Err(Errno::NOENT));

        // Verify both `lstat:` and `stat:` subjects are recorded and match `answer_now`.
        let subjects: Vec<&str> = sandbox.reads.iter().map(|r| r.subject.as_str()).collect();
        for expected in [
            "lstat:crates/m/parts/dir_link",
            "stat:crates/m/parts/dir_link",
            "lstat:crates/m/parts/dangling",
            "stat:crates/m/parts/dangling",
        ] {
            assert!(subjects.contains(&expected), "missing {expected:?} in {subjects:?}");
        }
        for read in &sandbox.reads {
            assert_eq!(answer_now(&pkg, &read.subject), Ok(read.digest), "parity for {}", read.subject);
        }

        // Security control: intermediate out-of-tree directory symlink is STILL refused even with follow=false.
        for follow in [false, true] {
            let (_pkg, mut sb) = member_sandbox(&tree);
            let fd = sb.open(ROOT_FD, b"/workspace/crates/m/parts", READ_ONLY).expect("not stopped").expect("dir");
            let Err(stop) = sb.path_stat(fd, b"out_dir_link/secret.txt", follow) else {
                panic!("intermediate out-of-tree symlink stat (follow={follow}) must be refused");
            };
            assert!(stop.0.contains("a link out of /workspace"), "{}", stop.0);
            assert!(!stop.0.contains(&ws_str), "must not leak host path: {}", stop.0);
            assert!(matches!(sb.refused, Some((ProcLimit::Paths, _))));
        }

        // Security control: crafted `lstat:..` directly into `answer_now` cannot escape `top`.
        assert!(answer_now(&pkg, "lstat:..").is_err(), "crafted `lstat:..` must fail closed");
    }
}
