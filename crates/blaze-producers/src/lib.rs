//! Each producer's source digest (decision D105, piece 2; docs/v2/plan/design/producer-id.md):
//! the sans-IO core the entry's build step drives. Given the workspace's manifests, lock,
//! pins folder and source files AS VALUES (bytes the driver read), it finds each producer's
//! crates from the manifests (the crates whose `[package.metadata.blaze] producer` names it,
//! and every workspace crate they depend on, transitively, by normal and build dependencies)
//! and digests each row: never a hand-kept list.
//!
//! A row is sound only when the producing function lives in a crate that names the row: then
//! every line it can run is in that crate's closure, and a change anywhere in it moves the row
//! (rule 21). A crate's own files without its closure (the `producer-own` list, until
//! 2026-11-17) is refused by name ([`Refused::OwnFilesOnly`]): its function calls into crates
//! the row left out, so two builds wrote different bytes under one key. No binary keys a record
//! by these rows today: every producing function is in blaze-cli, whose closure is the binary,
//! so the entry keys every row by the binary's linker id (blaze-cli `producers.rs`, `home.rs`
//! `row_of` has the table). The walk is for a producer whose function moves into its own crate.
//!
//! The walk is a state machine (rule 14): [`Walk::next`] says what it needs ([`Step::Read`] a
//! file, [`Step::List`] a directory, both by path relative to the workspace root), the driver
//! answers ([`Walk::read`], [`Walk::listed`]), and [`Walk::finish`] gives the [`Digests`] and
//! the paths the driver tells its builder to watch. Nothing of the machine reaches a digest
//! (rule 25): paths are workspace-relative, every list is sorted before it is digested, and a
//! directory's listing order does not matter.
//!
//! What a row covers, for each crate of its closure (sorted by directory): every file under the
//! crate's directory except its top-level `tests/`, `benches/`, `examples/` and `target/` and
//! any name starting with `.` (a `.DS_Store` is the machine's, not source), each as (path,
//! length, bytes); then the `[workspace.dependencies]` entries its crates name (each value
//! whole), `[workspace.package] edition`, the `Cargo.lock` rows of every outside crate reached
//! (every version of each name, and their dependencies, transitively) and every file under `pins/` (the pinned toolchain's facts, which
//! blaze-pins includes from outside its own directory, so its crate's files alone miss them).
//! A lock row with no source is a path crate (a `[patch]` under vendor/, or a member): it joins
//! the closure and is digested by its files, as a member is. A name the lock lacks (a lock of
//! another moment than the sources) refuses that row alone ([`Digest::Refused`]); an
//! `optional = true` dependency the lock lacks is one no feature turned on, so not built and not
//! an input (vendor/quinn-proto's `rustls-platform-verifier`).
//! It over-approximates at crate grain (say so, never written around): a crate's `cfg(test)`
//! files, every target's dependencies and every optional dependency count; each is a needless
//! miss, never a stale answer.

#![cfg_attr(not(test), no_std)]

mod lock;
mod manifest;
#[cfg(test)]
mod tests;

use core::fmt;

use blaze_ids::{
    ContentId, Hasher,
    producer::Row,
};
use blaze_own::{Alloc, SortedMap, String, Vec, VecDeque};

use crate::{
    lock::LockRow,
    manifest::{Dep, Root, Target},
};

/// A text in the walk's allocator.
pub type Text<'a> = String<Alloc<'a>>;

pub(crate) fn text<'a>(s: &str, alloc: Alloc<'a>) -> Text<'a> {
    String::from_str_in(s, alloc)
}

/// The rows a build digests, in [`Row::FROM_SOURCE`] order.
pub const ROWS: [Row; Row::COUNT - 1] = Row::FROM_SOURCE;

const FILE_DOMAIN: &str = "blaze producers file 1";
const CRATE_DOMAIN: &str = "blaze producers crate 1";
const ROW_DOMAIN: &str = "blaze producers row 2";
const PINS_DOMAIN: &str = "blaze producers pins 1";

/// The root manifest, the lock and the pins folder (pins/README.md), relative to the workspace
/// root.
pub const ROOT_MANIFEST: &str = "Cargo.toml";
pub const LOCK: &str = "Cargo.lock";
pub const PINS: &str = "pins";

/// A crate's top-level directories that are not in its binary.
const NOT_BUILT: [&str; 4] = ["tests", "benches", "examples", "target"];

/// What the walk needs next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step<'w> {
    /// The bytes of the file at this path (relative to the workspace root): answer
    /// [`Walk::read`] with `Some(bytes)`, or `None` when there is no such file.
    Read(&'w str),
    /// The entries of the directory at this path: answer [`Walk::listed`].
    List(&'w str),
    /// Nothing more: call [`Walk::finish`].
    Done,
}

/// One entry of a listed directory: its name (one path component) and whether it is a
/// directory (a symbolic link answers for what it points at).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry<'e> {
    pub name: &'e str,
    pub is_dir: bool,
}

/// The walk's answer: each row's digest, and the paths whose change can move one.
pub struct Digests<'a> {
    /// In [`ROWS`] order.
    pub rows: [(Row, Digest<'a>); Row::COUNT - 1],
    /// Relative to the workspace root, sorted, each once: the root manifest, the lock, the
    /// pins folder, every crate manifest read, and each top-level entry of each digested
    /// crate (`src`, `build.rs`, `Cargo.toml`, ...; a directory stands for all under it).
    pub watch: Vec<Text<'a>, Alloc<'a>>,
}

/// One row's answer.
#[derive(Debug)]
pub enum Digest<'a> {
    /// The digest of the row's sources.
    Source([u8; 32]),
    /// The row's sources cannot be said ([`Refused::NotLocked`], [`Refused::NoPathCrate`]): this
    /// row alone; every other row stands.
    Refused(Refused<'a>),
}

impl Digest<'_> {
    /// The digest, when the row has one.
    pub fn source(&self) -> Option<&[u8; 32]> {
        match self {
            Digest::Source(digest) => Some(digest),
            Digest::Refused(_) => None,
        }
    }
}

/// Why the walk stopped; each names the path or row (rule 19) and refers to the spec.
#[derive(Debug)]
pub enum Refused<'a> {
    /// A file the walk needs is not there (a member's manifest, the lock, the pins folder, a
    /// file listed a moment before).
    Missing { path: Text<'a> },
    /// A manifest or lock that is not UTF-8.
    NotUtf8 { path: Text<'a> },
    /// A manifest or lock that is no TOML.
    Toml { path: Text<'a>, error: blaze_toml::Error },
    /// A form of a manifest field the walk does not handle, by name.
    Shape { path: Text<'a>, what: &'static str },
    /// A `path =` dependency that is absolute or leaves the workspace.
    Escapes { path: Text<'a>, dep: Text<'a> },
    /// A `producer` list naming no source row.
    UnknownRow { path: Text<'a>, name: Text<'a> },
    /// A `producer-own` list: a crate's own files without its closure, less than its producing
    /// function runs (rule 21).
    OwnFilesOnly { path: Text<'a> },
    /// No crate names this row as its `producer`.
    NoRoot { row: Row },
    /// A `x.workspace = true` with no `[workspace.dependencies] x`.
    NoWorkspaceDep { path: Text<'a>, key: Text<'a> },
    /// An outside crate a closure reaches with no `Cargo.lock` row: that row's refusal alone.
    NotLocked { name: Text<'a> },
    /// A `Cargo.lock` row with no source (a path crate) a closure reaches, whose name no crate the
    /// walk read has (no member, path dependency or `[patch]` path): that row's refusal alone.
    NoPathCrate { name: Text<'a> },
    /// The driver answered a step the walk did not ask (a driver bug).
    Unasked { answered: &'static str },
}

impl fmt::Display for Refused<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Missing { path } => write!(f, "{path}: not there, and the producer digests need it"),
            Refused::NotUtf8 { path } => write!(f, "{path}: not UTF-8"),
            Refused::Toml { path, error } => write!(f, "{path}: not TOML at byte {}: {error}", error.at),
            Refused::Shape { path, what } => write!(f, "{path}: the producer digests read {what}, and found another form"),
            Refused::Escapes { path, dep } => {
                write!(f, "{path}: dependency `{dep}`'s path is absolute or leaves the workspace (rule 25)")
            }
            Refused::UnknownRow { path, name } => write!(
                f,
                "{path}: `[package.metadata.blaze]` names producer `{name}`, which is no source row (expander, lowering, body-checker, names, code-builder)"
            ),
            Refused::OwnFilesOnly { path } => write!(
                f,
                "{path}: `[package.metadata.blaze] producer-own` digests a crate's own files without the crates its code calls, so a key would miss code that makes its value (rule 21): name the row in `producer`, which covers the whole closure"
            ),
            Refused::NoRoot { row } => write!(f, "no crate's `[package.metadata.blaze] producer` names `{}`", row.name()),
            Refused::NoWorkspaceDep { path, key } => {
                write!(f, "{path}: `{key}.workspace = true` with no `[workspace.dependencies] {key}`")
            }
            Refused::NotLocked { name } => write!(f, "Cargo.lock: no row for `{name}`, which a producer's crates use"),
            Refused::NoPathCrate { name } => write!(
                f,
                "Cargo.lock: `{name}` has a row with no source (a path crate), and no member, path dependency or `[patch]` path read is named so"
            ),
            Refused::Unasked { answered } => write!(f, "the driver answered `{answered}`, which the walk did not ask"),
        }?;
        f.write_str(" (docs/v2/plan/design/producer-id.md)")
    }
}

impl core::error::Error for Refused<'_> {}

impl Refused<'_> {
    /// The workspace-relative file it is about, if one: a driver whose walk stopped watches it,
    /// so the fix runs the step again.
    pub fn path(&self) -> Option<&str> {
        match self {
            Refused::Missing { path }
            | Refused::NotUtf8 { path }
            | Refused::Toml { path, .. }
            | Refused::Shape { path, .. }
            | Refused::Escapes { path, .. }
            | Refused::UnknownRow { path, .. }
            | Refused::OwnFilesOnly { path }
            | Refused::NoWorkspaceDep { path, .. } => Some(path.as_str()),
            Refused::NoRoot { .. } | Refused::NotLocked { .. } | Refused::NoPathCrate { .. } | Refused::Unasked { .. } => None,
        }
    }
}

/// A crate the walk knows, by its directory.
struct Crate<'a> {
    dir: Text<'a>,
    name: Text<'a>,
    producer: u8,
    /// Each dependency, and whether it is `optional = true`.
    deps: Vec<(Dep<'a>, bool), Alloc<'a>>,
    /// Its files' (path, digest), filled in the files phase.
    files: Vec<(Text<'a>, ContentId), Alloc<'a>>,
}

/// What one row covers, fixed once every manifest is read.
struct Plan<'a> {
    /// Crate indices of the closure, sorted by directory.
    crates: Vec<usize, Alloc<'a>>,
    /// Indices into the root's `[workspace.dependencies]`, sorted, each once.
    ws: Vec<usize, Alloc<'a>>,
    /// The lock rows reached (indices into the sorted lock), ascending.
    locked: Vec<usize, Alloc<'a>>,
    /// Why the row cannot be digested, if it cannot.
    refused: Option<Refused<'a>>,
}

enum Job {
    Root,
    /// List a `<dir>/*` member glob's directory.
    Members,
    /// A crate's manifest.
    Manifest,
    Lock,
    /// The pins folder, or a directory under it.
    PinDir,
    /// A file under the pins folder.
    PinFile,
    /// A directory of crate `.0`; top level when `.1`.
    Dir(usize, bool),
    /// A file of crate `.0`.
    File(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Manifests,
    Files,
    Done,
}

/// The walk (see the crate's docs).
pub struct Walk<'a> {
    alloc: Alloc<'a>,
    phase: Phase,
    /// Jobs not yet asked, each with its path.
    queue: VecDeque<(Job, Text<'a>), Alloc<'a>>,
    /// The job asked and not yet answered.
    asked: Option<(Job, Text<'a>)>,
    root: Option<Root<'a>>,
    crates: Vec<Crate<'a>, Alloc<'a>>,
    by_dir: SortedMap<Text<'a>, usize, Alloc<'a>>,
    plans: Vec<Plan<'a>, Alloc<'a>>,
    lock: Option<Vec<LockRow<'a>, Alloc<'a>>>,
    /// Each file under the pins folder, with its digest (path and bytes).
    pins: Vec<(Text<'a>, ContentId), Alloc<'a>>,
    /// Whether the pins folder itself was listed.
    pins_listed: bool,
    watch: Vec<Text<'a>, Alloc<'a>>,
}

impl<'a> Walk<'a> {
    /// A walk from the root manifest.
    pub fn new(alloc: Alloc<'a>) -> Walk<'a> {
        let mut queue = VecDeque::new_in(alloc);
        queue.push_back((Job::Root, text(ROOT_MANIFEST, alloc)));
        Walk {
            alloc,
            phase: Phase::Manifests,
            queue,
            asked: None,
            root: None,
            crates: Vec::new_in(alloc),
            by_dir: SortedMap::new_in(alloc),
            plans: Vec::new_in(alloc),
            lock: None,
            pins: Vec::new_in(alloc),
            pins_listed: false,
            watch: Vec::new_in(alloc),
        }
    }

    /// What the walk needs next. Asking again before answering gives the same step.
    pub fn next(&mut self) -> Result<Step<'_>, Refused<'a>> {
        while self.asked.is_none() {
            if let Some(job) = self.queue.pop_front() {
                self.asked = Some(job);
                break;
            }
            match self.phase {
                Phase::Manifests => {
                    self.plan()?;
                    self.phase = Phase::Files;
                }
                Phase::Files => self.phase = Phase::Done,
                Phase::Done => return Ok(Step::Done),
            }
        }
        Ok(match &self.asked {
            Some((Job::Members | Job::Dir(..) | Job::PinDir, path)) => Step::List(path.as_str()),
            Some((_, path)) => Step::Read(path.as_str()),
            None => Step::Done,
        })
    }

    /// The answer to [`Step::Read`]: the file's bytes, or `None` when it is not there.
    pub fn read<'b>(&mut self, bytes: Option<&'b [u8]>) -> Result<(), Refused<'a>> {
        let alloc = self.alloc;
        let Some((job, path)) = self.asked.take() else { return Err(Refused::Unasked { answered: "read" }) };
        if matches!(job, Job::Members | Job::Dir(..) | Job::PinDir) {
            self.asked = Some((job, path));
            return Err(Refused::Unasked { answered: "read" });
        }
        // `job` is a read from here on.
        let Some(bytes) = bytes else { return Err(Refused::Missing { path }) };
        let utf8 = |bytes: &'b [u8]| -> Result<&'b str, Refused<'a>> {
            core::str::from_utf8(bytes).map_err(|_| Refused::NotUtf8 { path: path.clone() })
        };
        match job {
            Job::Root => {
                let root = manifest::root(utf8(bytes)?, alloc)?;
                for member in root.members.iter() {
                    match member.strip_suffix("/*") {
                        Some(dir) if !dir.contains('*') => self.queue.push_back((Job::Members, text(dir, alloc))),
                        _ if !member.contains('*') && !member.contains('?') && !member.contains('[') => {
                            self.want_crate(member.as_str())
                        }
                        _ => {
                            return Err(Refused::Shape {
                                path: text(ROOT_MANIFEST, alloc),
                                what: "`workspace.members` entries as a directory or `<dir>/*`",
                            });
                        }
                    }
                }
                // The `[patch]` path crates: a source-less lock row a closure reaches may be one.
                for dir in root.patches.iter() {
                    self.want_crate(dir.as_str());
                }
                // The lock before the plan: the plan follows outside crates into path crates.
                self.queue.push_back((Job::Lock, text(LOCK, alloc)));
                self.watch.push(path);
                self.root = Some(root);
            }
            Job::Manifest => {
                let dir = path.strip_suffix("/Cargo.toml").unwrap_or("");
                let member = manifest::member(dir, path.as_str(), utf8(bytes)?, alloc)?;
                let root = self.root.as_ref().ok_or(Refused::Unasked { answered: "a manifest before the root" })?;
                // Path crates outside the members are read too: a closure may reach them.
                let mut reach: Vec<Text<'a>, Alloc<'a>> = Vec::new_in(alloc);
                for (dep, _) in member.deps.iter() {
                    match dep {
                        Dep::Path(dir) => reach.push(dir.clone()),
                        Dep::Workspace(key) => match ws_dep(root, key) {
                            Some(at) => {
                                if let Target::Path(dir) = &root.deps[at].target {
                                    reach.push(dir.clone());
                                }
                            }
                            None => return Err(Refused::NoWorkspaceDep { path: path.clone(), key: key.clone() }),
                        },
                        Dep::Outside(_) => {}
                    }
                }
                let at = *self.by_dir.get(dir).ok_or(Refused::Unasked { answered: "a manifest of no crate asked" })?;
                let krate = &mut self.crates[at];
                krate.name = member.name;
                krate.producer = member.producer;
                krate.deps = member.deps;
                for dir in reach.iter() {
                    self.want_crate(dir.as_str());
                }
                self.watch.push(path);
            }
            Job::Lock => {
                self.lock = Some(lock::rows(utf8(bytes)?, alloc)?);
                self.watch.push(path);
            }
            Job::PinFile => {
                let id = Hasher::new(FILE_DOMAIN).part(path.as_bytes()).part(bytes).finish();
                self.pins.push((path, id));
            }
            Job::File(at) => {
                let id = Hasher::new(FILE_DOMAIN).part(path.as_bytes()).part(bytes).finish();
                self.crates[at].files.push((path, id));
            }
            job @ (Job::Members | Job::Dir(..) | Job::PinDir) => {
                self.asked = Some((job, path));
                return Err(Refused::Unasked { answered: "read" });
            }
        }
        Ok(())
    }

    /// The answer to [`Step::List`]: the directory's entries in any order, or `None` when there
    /// is no such directory.
    pub fn listed(&mut self, entries: Option<&[Entry<'_>]>) -> Result<(), Refused<'a>> {
        let alloc = self.alloc;
        let Some((job, path)) = self.asked.take() else { return Err(Refused::Unasked { answered: "listed" }) };
        let Some(entries) = entries else { return Err(Refused::Missing { path }) };
        let mut sorted: Vec<Entry<'_>, Alloc<'a>> = Vec::new_in(alloc);
        sorted.extend_from_slice(entries);
        sorted.sort_unstable_by(|a, b| a.name.cmp(b.name));
        match job {
            Job::Members => {
                let excluded = |dir: &str| self.root.as_ref().is_some_and(|root| root.exclude.iter().any(|e| e.as_str() == dir));
                let mut wanted: Vec<Text<'a>, Alloc<'a>> = Vec::new_in(alloc);
                for entry in sorted.iter().filter(|e| e.is_dir && !e.name.starts_with('.')) {
                    let dir = child(&path, entry.name, alloc);
                    if !excluded(dir.as_str()) {
                        wanted.push(dir);
                    }
                }
                for dir in wanted.iter() {
                    self.want_crate(dir.as_str());
                }
            }
            Job::Dir(at, top) => {
                for entry in sorted.iter() {
                    if entry.name.starts_with('.') || (top && entry.is_dir && NOT_BUILT.contains(&entry.name)) {
                        continue;
                    }
                    let full = child(&path, entry.name, alloc);
                    if top {
                        self.watch.push(full.clone());
                    }
                    let job = if entry.is_dir { Job::Dir(at, false) } else { Job::File(at) };
                    self.queue.push_back((job, full));
                }
            }
            // Every file under the pins folder, at any depth (`.` names are the machine's, and a
            // `.md` file is prose about the pins, never a pin: pins/README.md).
            Job::PinDir => {
                for entry in sorted.iter().filter(|e| !e.name.starts_with('.') && (e.is_dir || !e.name.ends_with(".md"))) {
                    let full = child(&path, entry.name, alloc);
                    let job = if entry.is_dir { Job::PinDir } else { Job::PinFile };
                    self.queue.push_back((job, full));
                }
                if path.as_str() == PINS {
                    self.pins_listed = true;
                    self.watch.push(path);
                }
            }
            job => {
                self.asked = Some((job, path));
                return Err(Refused::Unasked { answered: "listed" });
            }
        }
        Ok(())
    }

    /// The digests, once [`Walk::next`] said [`Step::Done`].
    pub fn finish(mut self) -> Result<Digests<'a>, Refused<'a>> {
        let alloc = self.alloc;
        if self.phase != Phase::Done || self.asked.is_some() {
            return Err(Refused::Unasked { answered: "finish before done" });
        }
        let (Some(root), Some(lock), true) = (self.root.as_ref(), self.lock.as_ref(), self.pins_listed) else {
            return Err(Refused::Unasked { answered: "finish before done" });
        };
        // The pins' digest: every file under the folder, sorted by path (each file's digest
        // holds its path, so a move moves it).
        self.pins.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        let mut pins = Hasher::new(PINS_DOMAIN);
        pins.part(&(self.pins.len() as u64).to_le_bytes());
        for (_, id) in self.pins.iter() {
            pins.id(id);
        }
        let pins = pins.finish();
        // Each crate's digest: its files sorted by path.
        let mut crate_ids: Vec<ContentId, Alloc<'a>> = Vec::new_in(alloc);
        for krate in self.crates.iter_mut() {
            krate.files.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
            let mut hasher = Hasher::new(CRATE_DOMAIN);
            hasher.part(krate.dir.as_bytes()).part(krate.name.as_bytes());
            hasher.part(&(krate.files.len() as u64).to_le_bytes());
            for (_, id) in krate.files.iter() {
                hasher.id(id);
            }
            crate_ids.push(hasher.finish());
        }
        let mut rows = ROWS.map(|row| (row, Digest::Source([0u8; 32])));
        for (at, plan) in self.plans.iter_mut().enumerate() {
            let row = ROWS[at];
            if let Some(why) = plan.refused.take() {
                rows[at].1 = Digest::Refused(why);
                continue;
            }
            let mut hasher = Hasher::new(ROW_DOMAIN);
            hasher.part(row.name().as_bytes());
            for &krate in plan.crates.iter() {
                hasher.part(b"crate").id(&crate_ids[krate]);
            }
            for &ws in plan.ws.iter() {
                hasher.part(root.deps[ws].key.as_bytes()).id(&root.deps[ws].digest);
            }
            match &root.edition {
                Some(edition) => hasher.part(b"edition").id(edition),
                None => hasher.part(b"no edition"),
            };
            // The lock rows the plan reached, in the lock's sorted order (a path crate's row too:
            // its name and version; its files are in the crates above).
            for &reached in plan.locked.iter() {
                let row = &lock[reached];
                hasher.part(row.name.as_bytes()).part(row.version.as_bytes());
                hasher.opt_part(row.source.as_deref()).opt_part(row.checksum.as_deref());
            }
            hasher.part(b"pins").id(&pins);
            rows[at].1 = Digest::Source(hasher.finish().0);
        }
        let mut watch = core::mem::replace(&mut self.watch, Vec::new_in(alloc));
        watch.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        watch.dedup();
        Ok(Digests { rows, watch })
    }

    /// The paths read so far (unsorted): what a driver watches when the walk stopped early.
    pub fn watched(&self) -> &[Text<'a>] {
        &self.watch[..]
    }

    /// Asks for the manifest of the crate at `dir` once, and knows the crate by it.
    fn want_crate(&mut self, dir: &str) {
        let alloc = self.alloc;
        if self.by_dir.contains_key(dir) {
            return;
        }
        let at = self.crates.len();
        self.crates.push(Crate {
            dir: text(dir, alloc),
            name: Text::new_in(alloc),
            producer: 0,
            deps: Vec::new_in(alloc),
            files: Vec::new_in(alloc),
        });
        self.by_dir.insert(text(dir, alloc), at);
        self.queue.push_back((Job::Manifest, child(dir, "Cargo.toml", alloc)));
    }

    /// Every manifest and the lock are read: fix each row's crates and lock rows by one work
    /// list over the dependencies, then ask for the pins folder and every digested crate's
    /// directory.
    fn plan(&mut self) -> Result<(), Refused<'a>> {
        let alloc = self.alloc;
        let root = self.root.as_ref().ok_or(Refused::Unasked { answered: "a plan before the root" })?;
        let lock = self.lock.as_ref().ok_or(Refused::Unasked { answered: "a plan before the lock" })?;
        let mut digested = Vec::new_in(alloc);
        digested.resize(self.crates.len(), false);
        for row in ROWS {
            let bit = 1u8 << row.index();
            let mut seen = Vec::new_in(alloc);
            seen.resize(self.crates.len(), false);
            let mut work: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
            for (at, krate) in self.crates.iter().enumerate() {
                if krate.producer & bit != 0 {
                    seen[at] = true;
                    work.push(at);
                }
            }
            if work.is_empty() {
                return Err(Refused::NoRoot { row });
            }
            let mut ws = Vec::new_in(alloc);
            // Names to find in the lock: each outside crate a closure reaches (and whether the
            // manifest marks it optional), and theirs.
            let mut names: Vec<(Text<'a>, bool), Alloc<'a>> = Vec::new_in(alloc);
            let mut taken = Vec::new_in(alloc);
            taken.resize(lock.len(), false);
            let mut refused = None;
            // Bounded: each turn takes a crate or a name off its list; a crate goes on once
            // (`seen`) and a lock row's dependencies once (`taken`), so both lists end.
            'walk: loop {
                if let Some(at) = work.pop() {
                    let krate = &self.crates[at];
                    let manifest = child(krate.dir.as_str(), "Cargo.toml", alloc);
                    for (dep, optional) in krate.deps.iter() {
                        let dir = match dep {
                            Dep::Path(dir) => dir,
                            Dep::Outside(name) => {
                                names.push((name.clone(), *optional));
                                continue;
                            }
                            Dep::Workspace(key) => {
                                let Some(entry) = ws_dep(root, key) else {
                                    return Err(Refused::NoWorkspaceDep { path: manifest, key: key.clone() });
                                };
                                ws.push(entry);
                                match &root.deps[entry].target {
                                    Target::Path(dir) => dir,
                                    Target::Outside(name) => {
                                        names.push((name.clone(), *optional));
                                        continue;
                                    }
                                }
                            }
                        };
                        let next = *self
                            .by_dir
                            .get(dir.as_str())
                            .ok_or_else(|| Refused::Missing { path: child(dir, "Cargo.toml", alloc) })?;
                        if !seen[next] {
                            seen[next] = true;
                            work.push(next);
                        }
                    }
                } else if let Some((name, optional)) = names.pop() {
                    // Every row of the name (each version the lock holds): one with a source is
                    // hashed and its dependencies followed; one without is a path crate, found by
                    // its package name among the crates read and digested by its files.
                    let mut found = false;
                    for (reached, row) in lock.iter().enumerate() {
                        if row.name != name.as_str() {
                            continue;
                        }
                        found = true;
                        if taken[reached] {
                            continue;
                        }
                        taken[reached] = true;
                        if row.source.is_some() {
                            for dep in row.deps.iter() {
                                names.push((dep.clone(), false));
                            }
                            continue;
                        }
                        let mut named = false;
                        for (next, krate) in self.crates.iter().enumerate() {
                            if krate.name.as_str() == name.as_str() {
                                named = true;
                                if !seen[next] {
                                    seen[next] = true;
                                    work.push(next);
                                }
                            }
                        }
                        if !named {
                            refused = Some(Refused::NoPathCrate { name: name.clone() });
                            break 'walk;
                        }
                    }
                    // An optional dependency the lock lacks: no feature turned it on, so it is
                    // not built (cargo's lock is the resolved graph) and not an input.
                    if !found && !optional {
                        refused = Some(Refused::NotLocked { name });
                        break 'walk;
                    }
                } else {
                    break;
                }
            }
            let mut crates = Vec::new_in(alloc);
            for at in 0..self.crates.len() {
                if seen[at] {
                    digested[at] = true;
                    crates.push(at);
                }
            }
            crates.sort_unstable_by(|&a, &b| self.crates[a].dir.as_str().cmp(self.crates[b].dir.as_str()));
            ws.sort_unstable();
            ws.dedup();
            let mut locked = Vec::new_in(alloc);
            for (reached, &is) in taken.iter().enumerate() {
                if is {
                    locked.push(reached);
                }
            }
            self.plans.push(Plan { crates, ws, locked, refused });
        }
        self.queue.push_back((Job::PinDir, text(PINS, alloc)));
        for (at, krate) in self.crates.iter().enumerate() {
            if digested[at] {
                self.queue.push_back((Job::Dir(at, true), krate.dir.clone()));
            }
        }
        Ok(())
    }
}

/// `dir/name`, or `name` at the root.
fn child<'a>(dir: &str, name: &str, alloc: Alloc<'a>) -> Text<'a> {
    let mut out = Text::new_in(alloc);
    if !dir.is_empty() {
        out.push_str(dir);
        out.push_str("/");
    }
    out.push_str(name);
    out
}

/// The index of `[workspace.dependencies] key`.
fn ws_dep(root: &Root<'_>, key: &str) -> Option<usize> {
    root.deps.binary_search_by(|dep| dep.key.as_str().cmp(key)).ok()
}
