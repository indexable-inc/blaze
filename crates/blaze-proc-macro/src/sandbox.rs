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

/// A package's own files, the only ones its macros see.
#[derive(Clone)]
pub enum Files {
    /// A folder on disk: a workspace member's own.
    Folder(PathBuf),
    /// A record's files (a fetched package's, read in place from its fetch record), by their
    /// path from the package's folder, sorted by path, each once.
    Listed(std::sync::Arc<[(String, &'static [u8])]>),
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

    /// The folder on disk of `rel` (a path from the served tree's root `root`), when it is in
    /// it. `Err`: a link out of the tree, named by the path as the macro saw it (rule 25: no
    /// path of this machine in the words, which a names record's row may hold).
    ///
    /// `root` may be relative to the folder blaze was started in, and `""` IS that folder: the
    /// checkout's form (`FetchEnv::checkout`, "" when blaze runs in the workspace's root), which
    /// a member's `Files::Member { workspace, .. }` is (blaze-cli proc_callers.rs `shown`). The OS names it `.`: `canonicalize("")` is
    /// `ENOENT`, which refused every file of every member of a checkout so started, `chdir` into
    /// `/workspace/<dir>` first (the shim's, at each call; MX 2026-11-17).
    fn real(&self, root: &Path, rel: &str) -> Result<Option<PathBuf>, String> {
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
        Ok(Some(real))
    }

    /// The listed files under the folder `rel` (`""`: the package's).
    fn under<'f>(files: &'f [(String, &'static [u8])], rel: &str) -> impl Iterator<Item = &'f str> + 'f {
        let prefix = if rel.is_empty() { String::new() } else { format!("{rel}/") };
        let start = files.partition_point(|(path, _)| path.as_str() < prefix.as_str());
        let n = prefix.len();
        files[start..].iter().map(|(path, _)| path.as_str()).take_while(move |path| path.starts_with(prefix.as_str())).map(move |path| &path[n..])
    }

    fn find(&self, rel: &str) -> Result<Option<Found>, String> {
        match self {
            Files::Folder(root) | Files::Member { workspace: root, .. } => {
                Ok(self.real(root, rel)?.and_then(|real| stat_of(&real)).map(|(filetype, size)| if filetype == Filetype::Directory { Found::Folder } else { Found::File(size) }))
            }
            Files::Listed(files) => {
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
            Files::Folder(root) | Files::Member { workspace: root, .. } => Ok(self.real(root, rel)?.and_then(|real| std::fs::read(real).ok())),
            Files::Listed(files) => Ok(files.binary_search_by(|(path, _)| path.as_str().cmp(rel)).ok().map(|at| files[at].1.to_vec())),
        }
    }

    /// The entries of the folder `rel`, sorted by name.
    fn listing(&self, rel: &str) -> Result<Vec<(Vec<u8>, Filetype)>, String> {
        match self {
            Files::Folder(root) | Files::Member { workspace: root, .. } => Ok(self.real(root, rel)?.map(|real| listing(&real)).unwrap_or_default()),
            Files::Listed(files) => {
                let mut entries: Vec<(Vec<u8>, Filetype)> = Vec::new();
                for rest in Files::under(files, rel) {
                    let (name, filetype) = match rest.split_once('/') {
                        Some((name, _)) => (name, Filetype::Directory),
                        None => (rest, Filetype::RegularFile),
                    };
                    if entries.last().map(|(last, _)| last.as_slice()) != Some(name.as_bytes()) {
                        entries.push((name.as_bytes().to_vec(), filetype));
                    }
                }
                // Sorted by full path, a folder's `name/` sorts after `name.x`: sort by name.
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                entries.dedup_by(|a, b| a.0 == b.0);
                Ok(entries)
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
    /// A folder's entries, sorted.
    entries: Option<Vec<(Vec<u8>, Filetype)>>,
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
            let filetype = if entry.path().is_dir() { Filetype::Directory } else { Filetype::RegularFile };
            entries.push((entry.file_name().into_encoded_bytes(), filetype));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

/// What a folder's read is answered by: its names, in order.
fn listing_digest(entries: &[(Vec<u8>, Filetype)]) -> ContentId {
    let mut names = Vec::new();
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

fn stat_of(path: &Path) -> Option<(Filetype, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let filetype = if meta.is_dir() { Filetype::Directory } else { Filetype::RegularFile };
    Some((filetype, meta.len()))
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
        "file" => files.bytes(rest).ok().flatten().map(|bytes| digest(&bytes)),
        "dir" => match files.find(rest) {
            Ok(Some(Found::Folder)) => files.listing(rest).ok().map(|entries| listing_digest(&entries)),
            _ => None,
        },
        "stat" if rest.is_empty() => Some(stat_digest(Filetype::Directory, 0)),
        "stat" => match files.find(rest) {
            Ok(Some(Found::Folder)) => Some(stat_digest(Filetype::Directory, 0)),
            Ok(Some(Found::File(size))) => Some(stat_digest(Filetype::RegularFile, size)),
            _ => None,
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
        let rel = self.resolve(path.trim_start_matches('/').as_bytes())?;
        let bytes = self.package.files.bytes(&rel).map_err(|why| self.refuse(why))?;
        self.record(format!("file:{rel}"), bytes.as_deref().map(digest));
        Ok(())
    }

    fn refuse(&mut self, why: String) -> Stop {
        let stop = Stop(why.clone());
        self.refused.get_or_insert((ProcLimit::Paths, why));
        stop
    }

    /// `path`, relative to the root `/`, as a path from the package's folder: `..` and `.`
    /// taken out; one outside [`PACKAGE`] is refused, naming it.
    fn resolve(&mut self, path: &[u8]) -> Result<String, Stop> {
        let Ok(text) = std::str::from_utf8(path) else {
            return Err(self.refuse(format!("named the path {:?}, which is not UTF-8", String::from_utf8_lossy(path))));
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
        let seen = self.package.files.seen();
        let mount = seen.mount();
        let rel = seen.in_tree(&full).map(str::to_owned);
        match rel {
            Some(rel) => Ok(rel),
            // From the root `/`: a relative path the module asked (wasi-libc's, from its cwd) is
            // shown absolute, one written absolute as it was.
            None => {
                let slash = if text.starts_with('/') { "" } else { "/" };
                Err(self.refuse(format!("named {slash}{text}, outside {mount}, the tree the macro is served, the only one it may read")))
            }
        }
    }

    /// What `rel` is in the package, refused when a link takes it out of the package.
    fn found(&mut self, rel: &str) -> Result<Option<Found>, Stop> {
        self.package.files.find(rel).map_err(|why| self.refuse(why))
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
        if file.entries.is_some() {
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
                Some(file) if file.entries.is_some() => Ok(Filetype::Directory),
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
                Some(file) if file.entries.is_some() => Ok(filestat(Filetype::Directory, 0)),
                Some(file) => Ok(filestat(Filetype::RegularFile, file.bytes.len() as u64)),
                None => Err(Errno::BADF),
            },
        })
    }

    fn open(&mut self, at: u32, path: &[u8], how: Open) -> Answer<u32> {
        if at != ROOT_FD {
            return Ok(Err(Errno::NOTCAPABLE));
        }
        if how.create() || how.truncate() || how.write() || how.append() {
            return Ok(Err(Errno::ROFS));
        }
        let rel = self.resolve(path)?;
        let Some(found) = self.found(&rel)? else {
            self.record(format!("file:{rel}"), None);
            return Ok(Err(Errno::NOENT));
        };
        let opened = if let Found::Folder = found {
            let entries = self.package.files.listing(&rel).map_err(|why| self.refuse(why))?;
            self.record(format!("dir:{rel}"), Some(listing_digest(&entries)));
            Opened { bytes: Vec::new(), pos: 0, entries: Some(entries) }
        } else {
            if how.directory() {
                return Ok(Err(Errno::NOTDIR));
            }
            let Some(bytes) = self.package.files.bytes(&rel).map_err(|why| self.refuse(why))? else {
                self.record(format!("file:{rel}"), None);
                return Ok(Err(Errno::IO));
            };
            self.record(format!("file:{rel}"), Some(digest(&bytes)));
            Opened { bytes, pos: 0, entries: None }
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

    fn path_stat(&mut self, at: u32, path: &[u8], _follow: bool) -> Answer<Filestat> {
        if at != ROOT_FD {
            return Ok(Err(Errno::NOTCAPABLE));
        }
        let rel = self.resolve(path)?;
        if rel.is_empty() {
            return Ok(Ok(filestat(Filetype::Directory, 0)));
        }
        let found = match self.found(&rel)? {
            Some(Found::Folder) => Some((Filetype::Directory, 0)),
            Some(Found::File(size)) => Some((Filetype::RegularFile, size)),
            None => None,
        };
        self.record(format!("stat:{rel}"), found.map(|(filetype, size)| stat_digest(filetype, size)));
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
        let Some(entries) = &file.entries else { return Ok(Err(Errno::NOTDIR)) };
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
        assert_eq!(sandbox.resolve(b"/workspace/crates/m/Cargo.toml").ok().as_deref(), Some("crates/m/Cargo.toml"));
        assert_eq!(sandbox.resolve(b"workspace/Cargo.toml").ok().as_deref(), Some("Cargo.toml"));
        assert!(sandbox.resolve(b"/package/Cargo.toml").is_err(), "a member's macro is not served /package");
        let mut dep = Sandbox::new(Arc::new(dependency));
        assert_eq!(dep.resolve(b"/package/src/lib.rs").ok().as_deref(), Some("src/lib.rs"));
        assert!(dep.resolve(b"/workspace/x").is_err());
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
