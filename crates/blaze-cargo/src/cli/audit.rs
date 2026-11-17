//! `blaze audit`: the driver of [`blaze_audit`], which decides everything; this reads the files
//! it decides on and writes what `pin` records. Commands:
//!
//! - `blaze audit [--json] [--today YYYY-MM-DD]`: every package `Cargo.lock` locks, checked
//!   against the RustSec advisory database the workspace pins and against yanked versions in
//!   the registry index blaze keeps, and every source checked for a strong pin (rule
//!   `weak_source_pin`: a git dependency's tree digest in `blaze.lock`, a registry package's
//!   checksum, no path dependency outside the workspace unless allowed). The exit code is
//!   nonzero on any error. `--today` names the day acceptances expire against (the wall
//!   clock's otherwise), so a report can be made again byte for byte.
//! - `blaze audit pin`: every git source's tree fetched fresh and hashed, and the advisory
//!   database's tree hashed, recorded in `blaze.lock` on trust at first use; each digest is
//!   printed, to be reviewed in the file's diff. A commit whose recorded tree differs from the
//!   one fetched is refused, never overwritten: one commit id has one tree unless SHA-1 was
//!   broken for it. The `[git]` table is made exactly the lock's git sources.
//! - `blaze audit fetch`: the database fetched at a commit, through [`blaze_audit::fetch`]'s
//!   machine. Its driver is not wired ([`blaze_audit::fetch::Refused::NotWired`]).
//!
//! The database is the directory `[workspace.metadata.blaze.audit] database` names (relative
//! to the workspace root), or `BLAZE_AUDIT_DB` when set, which wins. Its tree is hashed and
//! checked against `blaze.lock`'s `[advisories]` pin before one advisory is parsed.

use std::{
    alloc::Global,
    ffi::OsString,
    io::Read as _,
    path::{Path, PathBuf},
};

use blaze_audit::advisory::{Database, MOST_DATABASE_BYTES};
use blaze_audit::audit::{CRATES_IO, DatabaseInput, Index, verify_database};
use blaze_audit::cargo_lock::{Lock, MOST_LOCK_BYTES, Source};
use blaze_audit::date::Date;
use blaze_audit::fetch::{Effect, Fetch, RUSTSEC, Refused};
use blaze_audit::pins::{DatabasePin, GitPin, SourcePins, is_commit};
use blaze_audit::policy::Settings;
use blaze_audit::source::{OutsidePath, Trees, verify_git};
use blaze_audit::tree::{Kind, MOST_BYTES, MOST_FILES, TreeFile};
use blaze_audit::yanked::MOST_INDEX_BYTES;
use blaze_core::exit::Exit;
use blaze_fetch::{Algo, Digest};
use blaze_manifest::{Config, Context, Workspace, metadata::root_manifest};
use blaze_source::SourceId;
use blaze_units::lock::{BLAZE_LOCK, BlazeLock};

use super::say;

const USAGE: &str = "blaze audit [--json] [--today YYYY-MM-DD] | blaze audit pin | blaze audit fetch";

/// The environment variable that names the advisory database's directory, over the setting.
pub const DATABASE_ENV: &str = "BLAZE_AUDIT_DB";

/// The most bytes `blaze.lock` may hold: it is a person's file in the repository, read before
/// anything is trusted; the largest real lock of pins is some kilobytes, so `Cargo.lock`'s
/// bound is ample.
const MOST_BLAZE_LOCK_BYTES: usize = MOST_LOCK_BYTES;

/// The branch of RustSec's database `blaze audit fetch` asks for.
const RUSTSEC_REV: &str = "main";

/// `blaze audit ...`.
pub fn main(words: Vec<OsString>, color: bool) -> i32 {
    let refused = |what: &str, code: i32| {
        blaze_report::err!("{}", say::error(what, color));
        code
    };
    let mut words = words.into_iter();
    let mut verb = None;
    let mut json = false;
    let mut today = None;
    // Bounded: each turn takes a word of the command line.
    while let Some(word) = words.next() {
        match word.to_str() {
            Some("pin") | Some("fetch") | Some("check") if verb.is_none() => verb = word.to_str().map(str::to_owned),
            Some("--json") => json = true,
            Some("--today") => {
                let Some(text) = words.next().and_then(|it| it.into_string().ok()) else {
                    return refused(&format!("`--today` takes a day, YYYY-MM-DD: {USAGE}"), Exit::Usage.code());
                };
                match Date::parse(&text) {
                    Ok(day) => today = Some(day),
                    Err(why) => return refused(&format!("`--today {text}`: {why}"), Exit::Usage.code()),
                }
            }
            _ => return refused(&format!("{USAGE}, not `{}`", word.to_string_lossy()), Exit::Usage.code()),
        }
    }
    let done = match verb.as_deref() {
        None | Some("check") => check(json, today),
        Some("pin") if !json && today.is_none() => pin(color),
        Some("fetch") if !json && today.is_none() => fetch(),
        Some(verb) => return refused(&format!("`blaze audit {verb}` takes no options: {USAGE}"), Exit::Usage.code()),
    };
    match done {
        Ok(code) => code,
        Err(what) => refused(&what, Exit::Failed.code()),
    }
}

/// What the workspace says: its root, members by name, path dependencies outside it, and its
/// `[workspace.metadata.blaze.audit]` settings.
struct Here {
    root: PathBuf,
    members: Vec<String, Global>,
    outside: Vec<OutsidePath, Global>,
    settings: Settings,
}

fn here() -> Result<Here, String> {
    let cwd = std::env::current_dir().map_err(|error| format!("the current directory: {error}"))?;
    // What cargo reads of the environment: its variables are UTF-8 (`std::env::var`).
    let env = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)));
    let read = || -> anyhow::Result<Here> {
        let ctx = Context::new(Config::load(&cwd, env, &[])?);
        let manifest = root_manifest(None, &ctx)?;
        let ws = Workspace::new(&manifest, &ctx)?;
        let root = ws.root().to_path_buf();
        let mut members = Vec::new_in(Global);
        let mut outside = Vec::new_in(Global);
        for (_, member) in ws.members() {
            members.push(member.package_id.name.clone());
            // A member's direct path dependencies: one outside the root is read from a
            // directory nothing pins.
            for dep in &member.dependencies {
                if dep.source_id.is_path()
                    && let Some(path) = dep.source_id.local_path()
                    && !path.starts_with(&root)
                {
                    outside.push(OutsidePath { package: dep.name.clone(), path: path.display().to_string() });
                }
            }
        }
        members.sort();
        members.dedup();
        outside.sort();
        outside.dedup();
        let table = ws.custom_metadata().and_then(|it| it.get("blaze")).and_then(|it| it.get("audit"));
        let settings = Settings::parse(table).map_err(|why| anyhow::anyhow!("{why}"))?;
        Ok(Here { root, members, outside, settings })
    };
    read().map_err(|error| format!("{error:#}"))
}

/// The bytes of `path`, at most `most` of them (one more is read, so a longer file is
/// refused by name); `None` when there is no file.
fn read_bounded(path: &Path, most: usize) -> Result<Option<Vec<u8, Global>>, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to read `{}`: {error}", path.display())),
    };
    let mut bytes = Vec::new_in(Global);
    file.take(most as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read `{}`: {error}", path.display()))?;
    if bytes.len() > most {
        return Err(format!("`{}` holds more than the {most} bytes it may", path.display()));
    }
    Ok(Some(bytes))
}

fn cargo_lock(root: &Path) -> Result<Lock, String> {
    let path = root.join("Cargo.lock");
    let Some(bytes) = read_bounded(&path, MOST_LOCK_BYTES)? else {
        return Err(format!("`{}` is not there: `blaze audit` checks what a lock file locks; `blaze generate-lockfile` writes one", path.display()));
    };
    Lock::parse(&bytes).map_err(|why| format!("`{}`: {why}", path.display()))
}

/// `blaze.lock` at `root` as read, with its path as errors show it; empty when there is none.
fn blaze_lock(root: &Path) -> Result<(PathBuf, BlazeLock), String> {
    let path = root.join(BLAZE_LOCK);
    let shown = path.display().to_string();
    let lock = match read_bounded(&path, MOST_BLAZE_LOCK_BYTES)? {
        None => BlazeLock::default(),
        Some(bytes) => {
            let text = String::from_utf8(bytes.to_vec()).map_err(|error| format!("`{shown}` is not UTF-8: {error}"))?;
            BlazeLock::parse(&text, &shown)?
        }
    };
    Ok((path, lock))
}

/// One file of a tree as read from the disk: its `/`-separated path under the root, its kind
/// and its bytes (a symlink's are where it points).
struct OnDisk {
    path: String,
    kind: Kind,
    bytes: Vec<u8, Global>,
}

/// Every file under `dir`, symlinks not followed, nothing of a `.git` read; at most
/// [`MOST_FILES`] files of at most `most_bytes` together, refused by name past either.
fn tree_files(dir: &Path, most_bytes: u64) -> Result<Vec<OnDisk, Global>, String> {
    use std::os::unix::fs::PermissionsExt as _;
    let shown = |path: &Path| path.display().to_string();
    let mut files = Vec::new_in(Global);
    let mut bytes = 0u64;
    let mut dirs: Vec<(PathBuf, String), Global> = Vec::new_in(Global);
    dirs.push((dir.to_path_buf(), String::new()));
    // Bounded: each directory is taken once (symlinks are not followed), and the files are
    // capped at MOST_FILES.
    while let Some((at, prefix)) = dirs.pop() {
        let entries = std::fs::read_dir(&at).map_err(|error| format!("failed to list `{}`: {error}", shown(&at)))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("failed to list `{}`: {error}", shown(&at)))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(format!("`{}` has a name that is not UTF-8, which no tree's listing names", shown(&entry.path())));
            };
            if name.eq_ignore_ascii_case(".git") {
                continue;
            }
            let path = match prefix.is_empty() {
                true => name.to_owned(),
                false => format!("{prefix}/{name}"),
            };
            let full = entry.path();
            let meta = std::fs::symlink_metadata(&full).map_err(|error| format!("failed to read `{}`: {error}", shown(&full)))?;
            let kind = if meta.file_type().is_symlink() {
                Kind::Link
            } else if meta.is_dir() {
                dirs.push((full, path));
                continue;
            } else if meta.is_file() {
                match meta.permissions().mode() & 0o111 {
                    0 => Kind::File,
                    _ => Kind::Exec,
                }
            } else {
                return Err(format!("`{}` is neither a file, a directory nor a symlink, which no source tree holds", shown(&full)));
            };
            if files.len() >= MOST_FILES {
                return Err(format!("`{}` holds more than the {MOST_FILES} files a source tree may", shown(dir)));
            }
            let content = match kind {
                Kind::Link => {
                    let target = std::fs::read_link(&full).map_err(|error| format!("failed to read `{}`: {error}", shown(&full)))?;
                    let Some(target) = target.to_str() else {
                        return Err(format!("the symlink `{}` points at a path that is not UTF-8", shown(&full)));
                    };
                    let mut it = Vec::with_capacity_in(target.len(), Global);
                    it.extend_from_slice(target.as_bytes());
                    it
                }
                _ => {
                    let left = usize::try_from(most_bytes.saturating_sub(bytes)).unwrap_or(usize::MAX);
                    read_bounded(&full, left)
                        .map_err(|why| format!("{why}: `{}` holds more than the {most_bytes} bytes a tree may", shown(dir)))?
                        .ok_or_else(|| format!("`{}` went away while it was read", shown(&full)))?
                }
            };
            bytes = bytes.saturating_add(content.len() as u64);
            if bytes > most_bytes {
                return Err(format!("`{}` holds more than the {most_bytes} bytes a tree may", shown(dir)));
            }
            files.push(OnDisk { path, kind, bytes: content });
        }
    }
    Ok(files)
}

/// The digest in `algo` of the tree `files` are.
fn digest_files(dir: &Path, files: &[OnDisk], algo: Algo) -> Result<Digest, String> {
    let mut tree: Vec<TreeFile<'_>, Global> = Vec::with_capacity_in(files.len(), Global);
    tree.extend(files.iter().map(|it| TreeFile { path: &it.path, kind: it.kind, bytes: &it.bytes }));
    blaze_audit::tree::digest(&tree, algo).map_err(|why| format!("`{}`: {why}", dir.display()))
}

/// The checkouts blaze keeps on disk, hashed where a pin is checked.
struct Checkouts {
    sources: PathBuf,
}

/// The directory of `url`'s checkout of `commit` under `sources`, as the load names it.
fn checkout_dir(sources: &Path, url: &str, commit: &str) -> Result<PathBuf, String> {
    if !is_commit(commit) {
        return Err(format!("`{commit}` of `{url}` is no whole commit id"));
    }
    let id = SourceId::from_url(&format!("git+{url}")).map_err(|error| format!("the git source `{url}`: {error:#}"))?;
    Ok(sources.join("git").join("checkouts").join(id.git_ident()).join(commit))
}

impl Trees for Checkouts {
    fn tree(&mut self, url: &str, commit: &str, algo: Algo) -> Result<Option<Digest>, String> {
        let dir = checkout_dir(&self.sources, url, commit)?;
        match std::fs::symlink_metadata(&dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("failed to read `{}`: {error}", dir.display())),
            Ok(meta) if !meta.is_dir() => return Err(format!("`{}` is not a directory", dir.display())),
            Ok(_) => {}
        }
        let files = tree_files(&dir, MOST_BYTES)?;
        digest_files(&dir, &files, algo).map(Some)
    }
}

/// The sparse index files blaze keeps (`<sources>/registry/index/<dir>/.cache/<path>`, as
/// served).
struct Indexes {
    sources: PathBuf,
}

impl Index for Indexes {
    fn file(&mut self, registry: &str, package: &str) -> Option<Vec<u8, Global>> {
        // A name from `Cargo.lock` becomes a path: only a registry name's characters.
        if package.is_empty() || !package.bytes().all(|it| it.is_ascii_alphanumeric() || it == b'-' || it == b'_') {
            return None;
        }
        let id = match CRATES_IO.contains(&registry) {
            true => SourceId::crates_io_sparse(),
            false => SourceId::from_url(registry).ok()?,
        };
        if !id.is_sparse() {
            return None;
        }
        let at = self
            .sources
            .join("registry")
            .join("index")
            .join(id.registry_dir_name())
            .join(".cache")
            .join(blaze_units::index::index_path(package));
        read_bounded(&at, MOST_INDEX_BYTES).ok().flatten()
    }
}

fn sources() -> Result<PathBuf, String> {
    crate::serve::sources().map_err(|error| format!("{error:#}"))
}

/// The advisory database's directory: `BLAZE_AUDIT_DB`, else the setting under `root`.
fn database_dir(root: &Path, settings: &Settings) -> Option<PathBuf> {
    match std::env::var_os(DATABASE_ENV) {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => settings.database.as_ref().map(|it| root.join(it)),
    }
}

fn today_now() -> Date {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |it| it.as_secs());
    Date::from_unix_days(secs / 86_400)
}

/// `blaze audit [--json] [--today DAY]`.
fn check(json: bool, today: Option<Date>) -> Result<i32, String> {
    let here = here()?;
    let lock = cargo_lock(&here.root)?;
    let (_, blaze_lock) = blaze_lock(&here.root)?;
    let pins = blaze_lock.sources;
    let sources = sources()?;
    let database_dir = database_dir(&here.root, &here.settings);
    let database: Database;
    let input = match &database_dir {
        None => DatabaseInput::Unset(format!(
            "no advisory database is set: set `[workspace.metadata.blaze.audit] database` to the path of a clone of \
             {RUSTSEC} (or `{DATABASE_ENV}`), then run `blaze audit pin`"
        )),
        Some(dir) => {
            let shown = dir.display().to_string();
            let algo = pins.advisories.as_ref().map_or(Algo::Blake3, |it| it.tree.algo());
            let files = tree_files(dir, MOST_DATABASE_BYTES as u64)?;
            let found = digest_files(dir, &files, algo)?;
            match verify_database(&pins, &shown, &found) {
                Err(refused) => DatabaseInput::Refused(refused),
                Ok(()) => {
                    let mut named: Vec<(&str, &[u8]), Global> = Vec::with_capacity_in(files.len(), Global);
                    named.extend(files.iter().map(|it| (it.path.as_str(), &it.bytes[..])));
                    database = Database::parse(&named).map_err(|why| format!("the advisory database at `{shown}`: {why}"))?;
                    DatabaseInput::Read { path: shown, tree: found, database: &database }
                }
            }
        }
    };
    let mut trees = Checkouts { sources: sources.clone() };
    let mut index = sources.join("registry").join("index").is_dir().then(|| Indexes { sources: sources.clone() });
    let report = blaze_audit::audit(blaze_audit::Input {
        lock: &lock,
        members: &here.members,
        outside: &here.outside,
        pins: &pins,
        settings: &here.settings,
        database: input,
        trees: &mut trees,
        index: index.as_mut().map(|it| it as &mut dyn Index),
        today: today.unwrap_or_else(today_now),
    });
    let said = match json {
        true => {
            let mut line = report.json();
            line.push('\n');
            line
        }
        false => report.text(),
    };
    blaze_report::out!("{said}");
    Ok(match report.ok {
        true => 0,
        false => Exit::Failed.code(),
    })
}

/// `blaze audit pin`: trust at first use, recorded in `blaze.lock` for review.
fn pin(color: bool) -> Result<i32, String> {
    let here = here()?;
    let lock = cargo_lock(&here.root)?;
    let (path, mut blaze_lock) = blaze_lock(&here.root)?;
    let old = blaze_lock.sources.clone();
    let sources = sources()?;
    let mut fresh = SourcePins::default();
    let mut refused: Vec<String, Global> = Vec::new_in(Global);
    let mut fetcher = None;
    for package in &lock.packages {
        let Source::Git { url, commit, .. } = &package.source else { continue };
        let named = format!("{} v{}", package.name, package.version);
        if fresh.git(url, commit).is_some() {
            continue;
        }
        let at = match checkout_dir(&sources, url, commit) {
            Ok(at) => at,
            Err(why) => {
                refused.push(format!("`{named}`: {why}; `cargo update -p {}` locks a whole commit", package.name));
                continue;
            }
        };
        if fetcher.is_none() {
            let state = crate::serve::state().map_err(|error| format!("{error:#}"))?;
            fetcher = Some(crate::serve::Fetcher::new(state.join("fetch"))?);
        }
        let Some(fetcher) = &fetcher else { continue };
        let found = match fetcher.unpinned_git_digest(url, commit, &at.display().to_string()) {
            Ok(found) => found,
            Err(error) => {
                refused.push(format!("`{named}`: fetching `{url}` at {commit} to pin its tree: {error:#}"));
                continue;
            }
        };
        if let Some(was) = old.git(url, commit) {
            if was.tree.algo() != found.algo() {
                refused.push(format!(
                    "`{named}`: `{}` pins `{url}` at {commit} by {}, which `blaze audit pin` cannot check (it records \
                     BLAKE3): remove that line to pin it again, and review the diff",
                    path.display(),
                    was.tree
                ));
                continue;
            }
            if let Err(why) = verify_git(&was.tree, &found, &named, url, commit) {
                refused.push(why.to_string());
                continue;
            }
        }
        blaze_report::out!("{named}: {url}#{commit} {found}\n");
        fresh.set_git(GitPin { url: url.clone(), commit: commit.clone(), tree: found });
    }
    match database_dir(&here.root, &here.settings) {
        None => fresh.advisories = None,
        Some(dir) => {
            let files = tree_files(&dir, MOST_DATABASE_BYTES as u64)?;
            let tree = digest_files(&dir, &files, Algo::Blake3)?;
            blaze_report::out!("advisory database {}: {tree}\n", dir.display());
            fresh.advisories = Some(match &old.advisories {
                Some(was) if was.tree == tree => was.clone(),
                _ => DatabasePin { from: None, tree },
            });
        }
    }
    if !refused.is_empty() {
        for why in &refused {
            blaze_report::err!("{}", say::error(why, color));
        }
        return Err(format!("`{}` is not written: {} sources could not be pinned", path.display(), refused.len()));
    }
    let moves = fresh.moves(&old);
    if moves.is_empty() {
        blaze_report::out!("`{}` already pins every source as it is\n", path.display());
        return Ok(0);
    }
    blaze_lock.sources = fresh;
    let text = blaze_lock.render();
    let partial = path.with_extension("lock.partial");
    std::fs::write(&partial, text.as_bytes()).map_err(|error| format!("failed to write `{}`: {error}", partial.display()))?;
    std::fs::rename(&partial, &path).map_err(|error| format!("failed to write `{}`: {error}", path.display()))?;
    for line in &moves {
        blaze_report::out!("{line}\n");
    }
    blaze_report::out!(
        "`{}` written: these digests are trusted as of now, at first use; review its diff before you commit it\n",
        path.display()
    );
    Ok(0)
}

/// `blaze audit fetch`: the machine started, and its first effect refused, as its driver is
/// not wired.
fn fetch() -> Result<i32, String> {
    let here = here()?;
    let (_, blaze_lock) = blaze_lock(&here.root)?;
    let mut machine = Fetch::new(RUSTSEC, RUSTSEC_REV, &blaze_lock.sources);
    match machine.start() {
        Effect::Fetch { .. } => Err(Refused::NotWired.to_string()),
        other => Err(format!("`blaze audit fetch` began with {other:?}, not a fetch: a bug of its machine")),
    }
}
