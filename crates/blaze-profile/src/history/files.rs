//! The history's files as a sans-IO core (AGENTS.md rule 14, sans-io-cores): a synchronous
//! state machine that owns no file, lock or clock. It is told what to do (open the history,
//! commit a flush's files, write another writer's files, pin it for a reader, recover what dead
//! daemons left) and hands back [`Effect`]s, which a driver runs ([`super::drive`]) and answers
//! with [`Completion`]s; time comes only from those answers. Every write, sync, rename, delete
//! and lock of the history goes through it, so blaze-core's simulator sees each one: unsynced
//! bytes torn at a crash, a rename's durability, a failed write, a full disk, a held lock. A
//! durability fix is a change to the effects this emits, proven by the simulation test
//! (`files/tests.rs`), never a direct syscall.
//!
//! What it asks of its process beyond IO goes through [`Host`], a type parameter, so none of it
//! is the core's: which process this is and whether another is alive, what a crash record's
//! bytes say (its stack named by this executable's symbols), and a dead daemon's flight
//! recorder mapping read back as Parquet files ([`Dead`], [`Recovery`]). Encoding Parquet stays
//! with the driver, which hands this the bytes of each file ([`Placed`]); a merge copies column
//! chunks in memory ([`super::merge`]).
//!
//! Every file is placed the same way: made at a hidden name (`.<name>.<pid>.tmp`), its bytes
//! appended, synced, then renamed into place, which each driver makes durable as it completes
//! (it syncs the directory). The manifest is placed so too, and its rename is the one commit
//! point of every change ([`super::manifest`]); what that commit consumed (a flight recorder's
//! flush, a dead mapping, a crash record) is in the same commit, never in a mark after it.
//!
//! Operations run one at a time, in the order asked, each a machine of rounds: a step asks for
//! a batch of effects, and resumes once every one of them completed, with their completions in
//! the order asked. A job that fails lets go of every lock it holds before its outcome is told.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_core::{Completion, Done, Effect, FileName, IoError, LockMode, Mapping, Meta, Tag};
use parquet::file::properties::WriterProperties;

use super::{
    CompactError, HistoryError, OWNER,
    compact::{self, Name},
    manifest::{self, Entry, Held, Manifest, ManifestError, Recorder},
    merge,
    retire::{self, PINS, PinFile},
};
use crate::{batches::ImageRow, crash, flight::postmortem, panics::PanicRow};

#[cfg(test)]
mod tests;

/// What a core needs of its process that is not IO of the history: the driver's to answer.
pub(crate) trait Host {
    /// This process's id: its hidden files' names carry it.
    fn pid(&self) -> u32;
    /// Whether the process `pid` is alive (a hidden file of a live writer is its write under
    /// way; a crash record of a live process is not a crash).
    fn alive(&self, pid: u32) -> bool;
    /// What the crash record at `path`, whose bytes are `bytes`, says.
    fn crash(&mut self, path: &Path, bytes: &[u8]) -> CrashRead;
    /// The rows of the dead daemon's mapping `dead`, as files of flush [`Dead::seq`].
    fn recover(&mut self, dead: Dead) -> Recovery;
}

/// What a crash record says ([`Host::crash`]).
pub(crate) enum CrashRead {
    /// Nothing recorded: the record is deleted.
    Nothing,
    /// A fatal signal: a row for the next flush.
    Row(CrashRow),
    /// Of another layout or another blaze: set aside as `crash.<pid>.refused`, and said.
    Refused(crash::CrashError),
    /// Not read, through no fault of its own (this executable could not be hashed): left, and said.
    Unread(crash::CrashError),
}

/// A crash record's row, its process and the instant of its signal (what the manifest names
/// it by once a listed file holds the row), and the image its stack is in.
pub(crate) struct CrashRow {
    pub(crate) pid: u32,
    pub(crate) t_ns: i64,
    pub(crate) row: PanicRow,
    pub(crate) image: Option<ImageRow>,
}

/// A bug the simulation test plants to show its oracle catches it: never in a build of blaze.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    None,
    /// A file is renamed into place with no sync before: a crash after the commit leaves the
    /// manifest listing a torn file.
    NoSyncBeforeRename,
    /// What a flush consumed (its recorder's flush, its crash records) is left out of its
    /// commit and committed after it: a crash between the two reads those rows again.
    MarkAfterCommit,
}

/// A file this history counts against its cap: written by a flush, a merge or a postmortem,
/// retired oldest first (by the last flush it holds rows of, `name.last`).
#[derive(Clone, Debug)]
pub(crate) struct Kept {
    pub(crate) name: Name,
    /// Its name in the history's directory.
    pub(crate) file: String,
    pub(crate) bytes: u64,
    /// Its entry in the manifest.
    pub(crate) entry: Entry,
    /// Written by this history since it opened, so it may be merged: a file there before may
    /// be of another blaze's schema.
    pub(crate) ours: Ours,
    /// The manifest lists it. A flush's file whose commit failed is in place and not yet
    /// listed: the next flush lists it first, and no merge nor the cap takes it meanwhile.
    pub(crate) listed: Listed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ours {
    Ours,
    Theirs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Listed {
    Listed,
    Unlisted,
}

/// A file to place in the history: its name there, its bytes, and its entry.
pub(crate) struct Placed {
    pub(crate) file: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) entry: Entry,
}

/// A flush to commit ([`Files::flush`]).
pub(crate) struct Flush {
    /// The flush's number.
    pub(crate) seq: u64,
    /// One file a table with rows, each named as [`Name`] names a flush's file.
    pub(crate) files: Vec<Placed>,
    /// This process's flight recorder, its pid and nonce, when it is persisted: the commit
    /// says the recorder's flush `seq` committed.
    pub(crate) recorder: Option<(u32, u64)>,
    /// Crash records whose `panics` rows the file at this position of `files` holds: the
    /// commit that lists it consumes them, when it was placed.
    pub(crate) crashes: Option<(usize, crash::Written)>,
    /// The build whose end this flush is for: each of its tables is merged into one file.
    pub(crate) ended: Option<u64>,
}

/// What a flush did ([`Outcome::Flushed`]).
#[derive(Debug)]
pub(crate) struct Flushed {
    /// Each file of the flush, as [`Flush::files`]: `None` placed, else why not.
    pub(crate) placed: Vec<Option<HistoryError>>,
    /// The commit that lists the files: once it is done, the flush's number stands.
    pub(crate) committed: Result<(), HistoryError>,
    pub(crate) files: u32,
    pub(crate) bytes: u64,
    /// Files merged into others, and the files those merges made.
    pub(crate) merged: u32,
    pub(crate) compacted: u32,
    /// Files the cap retired.
    pub(crate) deleted: u32,
    /// What failed after the commit: a merge, the cap, a reap.
    pub(crate) deferred: Vec<HistoryError>,
    /// Crash records consumed that could not be deleted: the next open deletes them unread.
    pub(crate) undeleted: Vec<crash::CrashError>,
}

impl Flushed {
    fn empty() -> Flushed {
        Flushed {
            // alloc: empty until the flush's files are placed
            placed: Vec::new(),
            committed: Ok(()),
            files: 0,
            bytes: 0,
            merged: 0,
            compacted: 0,
            deleted: 0,
            // alloc: empty until something fails
            deferred: Vec::new(),
            // alloc: empty until something fails
            undeleted: Vec::new(),
        }
    }
}

/// What an open found ([`Outcome::Opened`]).
#[derive(Debug)]
pub(crate) struct Opened {
    /// The next flush's number.
    pub(crate) seq: u64,
    /// The highest build any file is named for, or a crash record was of.
    pub(crate) last_build: u64,
    /// The fatal signals of gone processes: rows for the first flush.
    pub(crate) crashes: crash::Recovered,
    /// Why a dead daemon's rows could not be read back: the next flush answers it.
    pub(crate) postmortem: Option<HistoryError>,
}

/// A pin a reader holds ([`Outcome::Pinned`]): let go of by [`Files::unpin`].
#[derive(Debug)]
pub(crate) struct Pinned {
    pub(crate) lock: Tag,
    pub(crate) lock_file: FileName,
    pub(crate) said: FileName,
    pub(crate) manifest: Manifest,
}

/// A dead daemon's flight recorder mapping, locked and mapped by the core, for the host to
/// read back ([`postmortem::Found`]).
pub(crate) struct Dead {
    pub(crate) pid: u32,
    /// Its path, for errors.
    pub(crate) path: PathBuf,
    pub(crate) mapping: Mapping,
    /// What the manifest says the history consumed of the mapping of this pid (of any nonce:
    /// the host compares it with the mapping's).
    pub(crate) recorder: Option<Recorder>,
    /// The flush number its files take.
    pub(crate) seq: u64,
}

/// The host's answer to a [`Dead`].
pub(crate) enum Recovery {
    /// The manifest says its rows are listed already: the mapping is deleted unread.
    Already,
    /// Its rows, as files of flush [`Dead::seq`], each named as [`Name`] names a flush's, and
    /// its nonce (0: none, a mapping whose process died making it).
    Files { nonce: u64, files: Vec<Placed> },
    /// It can't be read: it stays, and the error names it.
    Refused(HistoryError),
}

/// What an operation came to, one an operation asked for, in the order asked.
#[derive(Debug)]
pub(crate) enum Outcome {
    Opened(Result<Opened, HistoryError>),
    Flushed(Flushed),
    /// Of [`Files::recount`]: the files the cap retired.
    Recounted(Result<u32, HistoryError>),
    /// Of [`Files::write_listed`]: the bytes written.
    WroteListed(Result<u64, HistoryError>),
    Read(Result<Manifest, HistoryError>),
    Schemas(Result<(), HistoryError>),
    /// Of [`Files::recover_dead`]: the files written.
    RecoveredDead(Result<usize, HistoryError>),
    Pinned(Result<Pinned, HistoryError>),
    Unpinned,
}

/// What the effects in flight wait for, for the driver's span ([`super::drive`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wait {
    /// Nothing is in flight.
    Nothing,
    /// Another writer's hold of the manifest's lock.
    ManifestLock,
    /// Bytes reaching stable storage.
    Sync,
    /// A merge's reads and writes.
    Merge,
    /// A reap's look at the pins and its deletes.
    Reap,
    /// Any other read or write of the history's files.
    Files,
}

/// How long a writer waits for the manifest's lock between tries: doubling from the first,
/// up to the last, until [`manifest::LOCK_WAIT`] has passed on the driver's clock.
const LOCK_RETRY_FIRST: Duration = Duration::from_millis(1);
const LOCK_RETRY_MOST: Duration = Duration::from_millis(50);

/// The core of a history's files in one directory under the driver's root.
pub(crate) struct Files<H: Host> {
    host: H,
    /// The driver's root, for the paths errors name.
    root: PathBuf,
    /// The history's directory, under the driver's root.
    dir: FileName,
    state: State,
    io: Io,
    running: Option<Running>,
    queue: VecDeque<Job>,
    /// The effects of the running round, and their completions as they come.
    round: Vec<(Tag, Option<Done>)>,
    /// What the round waits for.
    wait: Wait,
    outcomes: VecDeque<Outcome>,
    #[cfg(test)]
    plant: Plant,
}

/// What the core keeps of the history between operations.
struct State {
    cap: u64,
    /// The files written, oldest first, and their bytes all told.
    kept: VecDeque<Kept>,
    total: u64,
    /// Bytes of the directory's files the cap counts that are not Parquet: the flight
    /// recorders' mappings and the crash records.
    side: u64,
    last_build: u64,
    /// The next flush number a postmortem's files take.
    seq: u64,
    /// The manifest may name retired files still on disk: a reap looks.
    reaping: bool,
    /// The manifest as this core last read or committed it.
    manifest: Manifest,
    /// Crash records a flush wrote the rows of: the next commit consumes them, then they go.
    listing: crash::Written,
    /// Each table's writer properties, by its place in [`crate::schema::TABLES`]: what a
    /// merge writes with.
    properties: Vec<WriterProperties>,
}

/// The effects asked for and not yet handed to the driver, the tags, and the clock.
struct Io {
    next: u64,
    asked: Vec<Effect>,
    now: Duration,
    wait: Wait,
}

/// A step of a machine: more effects asked for, or its end.
enum Step<T> {
    Pending,
    Ready(T),
}

/// A job running: the locks it holds, and the error it failed with while they are let go of.
struct Running {
    job: Job,
    locks: Vec<Tag>,
    failing: Option<HistoryError>,
}

/// What a machine's step sees: the core's host, names and IO, and the job's locks.
struct Cx<'a, H: Host> {
    host: &'a mut H,
    root: &'a Path,
    dir: &'a FileName,
    io: &'a mut Io,
    /// The locks the running job holds: let go of if it fails.
    locks: &'a mut Vec<Tag>,
    #[cfg(test)]
    plant: Plant,
}

impl<H: Host> Cx<'_, H> {
    /// The file `name` of the history's directory (a plain name, or `pins/<name>`).
    fn file(&self, name: &str) -> FileName {
        // alloc: a file's name, once an effect on it
        let joined = format!("{}/{name}", self.dir);
        match FileName::new(&joined) {
            Some(file) => file,
            None => unreachable!("`{name}` is a file's name in the history's directory"),
        }
    }

    /// The file `name` of [`PINS`].
    fn pin_file(&self, name: &str) -> FileName {
        // alloc: a file's name, once a pin
        self.file(&format!("{PINS}/{name}"))
    }

    /// The hidden name `name` is written to before it is renamed into place:
    /// `.<name>.<pid>.tmp`, this process's.
    fn hidden(&self, name: &str) -> FileName {
        // alloc: a file's name, once a file placed
        self.file(&format!(".{name}.{}.tmp", self.host.pid()))
    }

    /// The path of the file `name` of the history's directory, for an error.
    fn path(&self, name: &str) -> PathBuf {
        self.root.join(self.dir.as_str()).join(name)
    }

    fn path_of(&self, file: &FileName) -> PathBuf {
        self.root.join(file.as_str())
    }

    fn dir_path(&self) -> PathBuf {
        self.root.join(self.dir.as_str())
    }

    /// Asks for the effect `make` makes with a new tag, which it returns.
    fn ask(&mut self, wait: Wait, make: impl FnOnce(Tag) -> Effect) -> Tag {
        self.io.next += 1;
        let tag = Tag(self.io.next);
        self.io.asked.push(make(tag));
        if matches!(self.io.wait, Wait::Nothing | Wait::Files) {
            self.io.wait = wait;
        }
        tag
    }

    /// Lets go of the lock `lock`, asked now: its completion comes with the round.
    fn unlock(&mut self, lock: Tag) {
        self.locks.retain(|it| *it != lock);
        self.ask(Wait::Files, |tag| Effect::Unlock { lock, tag });
    }

    fn now(&self) -> Duration {
        self.io.now
    }

    /// The name of `file` in the history's directory, when it is directly in it.
    fn name_in<'f>(&self, file: &'f FileName) -> Option<&'f str> {
        let rest = file.as_str().strip_prefix(self.dir.as_str())?.strip_prefix('/')?;
        (!rest.contains('/')).then_some(rest)
    }

    /// The name of `file` in [`PINS`], when it is directly in it.
    fn name_in_pins<'f>(&self, file: &'f FileName) -> Option<&'f str> {
        let rest = file.as_str().strip_prefix(self.dir.as_str())?.strip_prefix('/')?;
        let rest = rest.strip_prefix(PINS)?.strip_prefix('/')?;
        (!rest.contains('/')).then_some(rest)
    }

    /// Whether a placed file is synced before its rename: always, but where a test plants
    /// the bug of not.
    fn syncs(&self) -> bool {
        #[cfg(test)]
        if self.plant == Plant::NoSyncBeforeRename {
            return false;
        }
        true
    }

    /// Whether a flush's commit holds what it consumed: always, but where a test plants the
    /// bug of a mark after it.
    fn consumes_in_commit(&self) -> bool {
        #[cfg(test)]
        if self.plant == Plant::MarkAfterCommit {
            return false;
        }
        true
    }
}

/// The error of an effect on `path` that failed doing `doing`.
fn io_failed(path: PathBuf, doing: &'static str, error: IoError) -> HistoryError {
    HistoryError::Io { path, doing, error: io::Error::other(error.0) }
}

/// The error of a completion no such effect has: a driver's bug, named.
fn unexpected(path: PathBuf, doing: &'static str, done: &Done) -> HistoryError {
    HistoryError::Io {
        path,
        doing,
        // alloc: an error
        error: io::Error::other(format!(
            "the driver answered {done:?}, which no such effect answers"
        )),
    }
}

/// `done`, or the error of its effect on `path` doing `doing`.
fn check(
    done: Done,
    path: impl FnOnce() -> PathBuf,
    doing: &'static str,
) -> Result<Done, HistoryError> {
    match done {
        Done::Failed(error) => Err(io_failed(path(), doing, error)),
        done => Ok(done),
    }
}

/// The completions of the round a step resumes with.
fn round(done: Option<Vec<Done>>) -> Vec<Done> {
    match done {
        Some(done) => done,
        None => unreachable!("a step that asked for effects resumes with their completions"),
    }
}

/// The one completion of a round of one effect.
fn only(done: Option<Vec<Done>>) -> Done {
    let mut done = round(done);
    match (done.pop(), done.is_empty()) {
        (Some(it), true) => it,
        (Some(_), false) | (None, _) => unreachable!("a round of one effect completes once"),
    }
}

/// A step that asked for nothing goes on as if its round completed, empty.
fn nothing_asked() -> Option<Vec<Done>> {
    // alloc: empty, allocates nothing
    Some(Vec::new())
}

fn manifest_error<H: Host>(cx: &Cx<'_, H>, why: ManifestError) -> HistoryError {
    HistoryError::Manifest { path: cx.path(manifest::FILE), why }
}

/// The completion of an unlock, checked.
fn unlocked(done: Done, path: PathBuf) -> Result<(), HistoryError> {
    match check(done, || path.clone(), "letting go of the lock of")? {
        Done::Unlocked => Ok(()),
        other => Err(unexpected(path, "letting go of the lock of", &other)),
    }
}

/// The listing `done` of the history's directory: its top-level files by name, with their
/// sizes, or the error.
fn listing<H: Host>(cx: &Cx<'_, H>, done: Done) -> Result<Vec<(String, Meta)>, HistoryError> {
    match check(done, || cx.dir_path(), "listing")? {
        Done::Listed(entries) => Ok(entries
            .into_iter()
            .filter(|(_, meta)| !meta.dir)
            // alloc: a file's name, once a listing
            .filter_map(|(file, meta)| Some((cx.name_in(&file)?.to_owned(), meta)))
            // alloc: once a listing
            .collect()),
        other => Err(unexpected(cx.dir_path(), "listing", &other)),
    }
}

/// Bytes of the files the cap counts that are not Parquet: each flight recorder mapping
/// (`flight-<pid>.map`: this process's, reserved whole as it is made, and one a postmortem
/// refused and left) and each crash record (`crash.<pid>`, one page, and `crash.<pid>.refused`).
fn side_bytes(listing: &[(String, Meta)]) -> u64 {
    listing
        .iter()
        .filter(|(name, _)| name.starts_with("crash.") || postmortem::pid_of(name).is_some())
        .map(|(_, meta)| meta.len)
        .sum()
}

/// Whether the hidden file `name` of a history's directory is one a write left half done: of a
/// process that is gone, of this one, or of a blaze before hidden names carried the writer's
/// pid. One of another live process (a `blaze perf record` writing beside the daemon) is its
/// write under way, and stays.
fn half_written(host: &impl Host, name: &str) -> bool {
    let Some(stem) = name.strip_prefix('.').and_then(|it| it.strip_suffix(".tmp")) else {
        return false;
    };
    if !(stem.contains(".parquet") || stem.starts_with(manifest::FILE)) {
        return false;
    }
    match stem.rsplit_once('.').and_then(|(_, pid)| pid.parse::<u32>().ok()) {
        Some(pid) => pid == host.pid() || !host.alive(pid),
        None => true,
    }
}

/// Marks in `held` what a flush consumed: its recorder's flush `seq`, and the crash records
/// whose rows it wrote.
fn consume(recorder: Option<(u32, u64)>, seq: u64, st: &State, held: &mut Held) {
    if let Some((pid, nonce)) = recorder {
        held.recorder_flushed(pid, nonce, seq);
    }
    held.crashes_written(st.listing.ids.iter().copied());
}

// --- Reading the manifest -------------------------------------------------------------

#[derive(Clone, Copy)]
enum Reading {
    Start,
    Stat,
    Read,
    List,
    Adopt,
}

/// A manifest read: as written, or adopted from the files of a directory with none.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Adoption {
    Written,
    Adopted,
}

/// A read of the manifest, as a reader takes it (no lock) or as a writer does under the lock:
/// a directory with no manifest gives the manifest its files make ([`manifest::adopt`]),
/// written by no reader. Its footers are read from mappings of its files, the retired files of
/// a blaze before manifests from [`manifest::LEGACY_RETIRED`], and the generation starts at the
/// highest any pin in [`PINS`] said.
struct Read {
    stage: Reading,
    names: Vec<String>,
    legacy: bool,
    said: u64,
}

impl Read {
    fn new() -> Read {
        // alloc: empty until a history is adopted
        Read { stage: Reading::Start, names: Vec::new(), legacy: false, said: 0 }
    }

    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        mut done: Option<Vec<Done>>,
    ) -> Step<Result<(Manifest, Adoption), HistoryError>> {
        loop {
            match self.stage {
                Reading::Start => {
                    let file = cx.file(manifest::FILE);
                    cx.ask(Wait::Files, |tag| Effect::Stat { file, tag });
                    self.stage = Reading::Stat;
                    return Step::Pending;
                }
                Reading::Stat => {
                    let doing = "looking for the manifest";
                    match check(only(done.take()), || cx.path(manifest::FILE), doing) {
                        Ok(Done::Stat(Some(_))) => {
                            let file = cx.file(manifest::FILE);
                            cx.ask(Wait::Files, |tag| Effect::ReadAll { file, tag });
                            self.stage = Reading::Read;
                        }
                        Ok(Done::Stat(None)) => {
                            let dir = cx.dir.clone(); // clone: a refcount
                            cx.ask(Wait::Files, |tag| Effect::List { dir, tag });
                            self.stage = Reading::List;
                        }
                        Ok(other) => {
                            return Step::Ready(Err(unexpected(
                                cx.path(manifest::FILE),
                                doing,
                                &other,
                            )));
                        }
                        Err(error) => return Step::Ready(Err(error)),
                    }
                    return Step::Pending;
                }
                Reading::Read => {
                    let doing = "reading the manifest";
                    return Step::Ready(
                        match check(only(done.take()), || cx.path(manifest::FILE), doing) {
                            Ok(Done::Read(bytes)) => match Manifest::decode(&bytes) {
                                Ok(it) => Ok((it, Adoption::Written)),
                                Err(why) => Err(manifest_error(cx, why)),
                            },
                            Ok(other) => Err(unexpected(cx.path(manifest::FILE), doing, &other)),
                            Err(error) => Err(error),
                        },
                    );
                }
                Reading::List => {
                    let entries = match check(only(done.take()), || cx.dir_path(), "listing") {
                        Ok(Done::Listed(entries)) => entries,
                        Ok(other) => {
                            return Step::Ready(Err(unexpected(cx.dir_path(), "listing", &other)));
                        }
                        Err(error) => return Step::Ready(Err(error)),
                    };
                    for (file, meta) in &entries {
                        if meta.dir {
                            continue;
                        }
                        if let Some(name) = cx.name_in(file) {
                            match manifest::kind(name) {
                                // alloc: a file's name, once a history is adopted
                                manifest::Kind::Listable => self.names.push(name.to_owned()),
                                manifest::Kind::Unclassified => {
                                    return Step::Ready(Err(HistoryError::Unclassified {
                                        dir: cx.dir_path(),
                                        // alloc: an error
                                        file: name.to_owned(),
                                    }));
                                }
                                manifest::Kind::Other if name == manifest::LEGACY_RETIRED => {
                                    self.legacy = true;
                                }
                                manifest::Kind::Schema
                                | manifest::Kind::Hidden
                                | manifest::Kind::Other => {}
                            }
                        } else if let Some(PinFile::Said { generation, .. }) =
                            cx.name_in_pins(file).and_then(PinFile::parse)
                        {
                            self.said = self.said.max(generation);
                        }
                    }
                    self.names.sort_unstable();
                    for at in 0..self.names.len() {
                        let file = cx.file(&self.names[at]);
                        cx.ask(Wait::Files, |tag| Effect::Map { file, tag });
                    }
                    if self.legacy {
                        let file = cx.file(manifest::LEGACY_RETIRED);
                        cx.ask(Wait::Files, |tag| Effect::ReadAll { file, tag });
                    }
                    self.stage = Reading::Adopt;
                    if self.names.is_empty() && !self.legacy {
                        done = nothing_asked();
                        continue;
                    }
                    return Step::Pending;
                }
                Reading::Adopt => {
                    let mut done = round(done.take()).into_iter();
                    let names = std::mem::take(&mut self.names);
                    // alloc: the footers' mappings, once a history is adopted
                    let mut footers = Vec::with_capacity(names.len());
                    for name in &names {
                        let doing = "opening to read its footer";
                        let Some(it) = done.next() else {
                            unreachable!("one completion an effect")
                        };
                        match check(it, || cx.path(name), doing) {
                            Ok(Done::Mapped(mapping)) => footers.push(mapping),
                            Ok(other) => {
                                return Step::Ready(Err(unexpected(cx.path(name), doing, &other)));
                            }
                            Err(error) => return Step::Ready(Err(error)),
                        }
                    }
                    let legacy = match self.legacy {
                        true => {
                            let doing = "reading the retired files of a history before manifests";
                            let path = cx.path(manifest::LEGACY_RETIRED);
                            let Some(it) = done.next() else {
                                unreachable!("one completion an effect")
                            };
                            match check(it, || path.clone(), doing) {
                                Ok(Done::Read(bytes)) => Some(bytes),
                                Ok(other) => {
                                    return Step::Ready(Err(unexpected(path, doing, &other)));
                                }
                                Err(error) => return Step::Ready(Err(error)),
                            }
                        }
                        false => None,
                    };
                    let adopted = manifest::adopt(
                        &cx.dir_path(),
                        names,
                        footers,
                        legacy.as_deref(),
                        self.said,
                    );
                    return Step::Ready(adopted.map(|it| (it, Adoption::Adopted)));
                }
            }
        }
    }
}

// --- Holding the manifest's lock -------------------------------------------------------

#[derive(Clone, Copy)]
enum Holding {
    Start,
    Lock,
    Wait,
    Read,
}

/// The manifest's lock taken, waiting at most [`manifest::LOCK_WAIT`] on the driver's clock for
/// another writer to let go of it, then the manifest read under it ([`Read`]). The lock is in
/// the job's locks from the moment it is taken.
struct Hold {
    stage: Holding,
    since: Duration,
    retry: Duration,
    lock: Tag,
    read: Read,
}

impl Hold {
    fn new() -> Hold {
        Hold {
            stage: Holding::Start,
            since: Duration::ZERO,
            retry: LOCK_RETRY_FIRST,
            lock: Tag(0),
            read: Read::new(),
        }
    }

    fn take_lock<H: Host>(&mut self, cx: &mut Cx<'_, H>) {
        let file = cx.file(manifest::LOCK);
        self.lock =
            cx.ask(Wait::ManifestLock, |tag| Effect::Lock { file, mode: LockMode::Exclusive, tag });
        self.stage = Holding::Lock;
    }

    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        mut done: Option<Vec<Done>>,
    ) -> Step<Result<Held, HistoryError>> {
        loop {
            match self.stage {
                Holding::Start => {
                    self.since = cx.now();
                    self.take_lock(cx);
                    return Step::Pending;
                }
                Holding::Lock => {
                    let doing = "locking the manifest's lock";
                    match check(only(done.take()), || cx.path(manifest::LOCK), doing) {
                        Ok(Done::Locked) => {
                            cx.locks.push(self.lock);
                            self.stage = Holding::Read;
                        }
                        Ok(Done::Contended) => {
                            if cx.now().saturating_sub(self.since) >= manifest::LOCK_WAIT {
                                return Step::Ready(Err(manifest_error(cx, ManifestError::Locked)));
                            }
                            let after = self.retry;
                            self.retry = (self.retry * 2).min(LOCK_RETRY_MOST);
                            cx.ask(Wait::ManifestLock, |tag| Effect::Timer { after, tag });
                            self.stage = Holding::Wait;
                            return Step::Pending;
                        }
                        Ok(other) => {
                            return Step::Ready(Err(unexpected(
                                cx.path(manifest::LOCK),
                                doing,
                                &other,
                            )));
                        }
                        Err(error) => return Step::Ready(Err(error)),
                    }
                }
                Holding::Wait => {
                    let doing = "waiting to lock the manifest's lock";
                    if let Err(error) = check(only(done.take()), || cx.path(manifest::LOCK), doing)
                    {
                        return Step::Ready(Err(error));
                    }
                    self.take_lock(cx);
                    return Step::Pending;
                }
                Holding::Read => {
                    return match self.read.resume(cx, done.take()) {
                        Step::Pending => Step::Pending,
                        Step::Ready(Err(error)) => Step::Ready(Err(error)),
                        Step::Ready(Ok((manifest, Adoption::Written))) => {
                            Step::Ready(Ok(Held::read(manifest)))
                        }
                        // Written at the next commit, so the next open sweeps against it, and the
                        // file of a blaze before manifests deleted after.
                        Step::Ready(Ok((manifest, Adoption::Adopted))) => {
                            Step::Ready(Ok(Held::adopted(manifest)))
                        }
                    };
                }
            }
        }
    }
}

// --- Committing the manifest ------------------------------------------------------------

#[derive(Clone, Copy)]
enum Committing {
    Start,
    Truncate,
    Append,
    Sync,
    Rename,
    Legacy,
}

/// The manifest written, when it changed: to this process's hidden name, synced, renamed over
/// the last, which is durable once done (so every file it lists, each renamed into place
/// before, is too). A manifest adopted from a directory of a blaze before manifests deletes
/// that blaze's retired list once committed.
struct Commit {
    stage: Committing,
    bytes: Vec<u8>,
}

impl Commit {
    fn new() -> Commit {
        // alloc: empty until a commit encodes
        Commit { stage: Committing::Start, bytes: Vec::new() }
    }

    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        held: &mut Held,
        done: Option<Vec<Done>>,
    ) -> Step<Result<(), HistoryError>> {
        let hidden = cx.hidden(manifest::FILE);
        let failed = |cx: &Cx<'_, H>, done: Option<Vec<Done>>, doing| {
            check(only(done), || cx.path(manifest::FILE), doing).err()
        };
        match self.stage {
            Committing::Start => {
                if !held.changed() {
                    return Step::Ready(Ok(()));
                }
                self.bytes = match held.encode() {
                    Ok(it) => it,
                    Err(why) => return Step::Ready(Err(manifest_error(cx, why))),
                };
                cx.ask(Wait::Files, |tag| Effect::Truncate { file: hidden, len: 0, tag });
                self.stage = Committing::Truncate;
            }
            Committing::Truncate => {
                if let Some(error) = failed(cx, done, "making the manifest's hidden file") {
                    return Step::Ready(Err(error));
                }
                let bytes = std::mem::take(&mut self.bytes);
                cx.ask(Wait::Files, |tag| Effect::Append { file: hidden, bytes, tag });
                self.stage = Committing::Append;
            }
            Committing::Append => {
                if let Some(error) = failed(cx, done, "writing the manifest") {
                    return Step::Ready(Err(error));
                }
                if cx.syncs() {
                    // durable: the manifest's bytes are on disk before the rename that commits it (the DST's planted bug is this sync left out)
                    cx.ask(Wait::Sync, |tag| Effect::Sync { file: hidden, tag });
                    self.stage = Committing::Sync;
                } else {
                    let to = cx.file(manifest::FILE);
                    // durable: the manifest's rename is the commit of a flush, a merge or a retire
                    cx.ask(Wait::Sync, |tag| Effect::Rename { from: hidden, to, tag });
                    self.stage = Committing::Rename;
                }
            }
            Committing::Sync => {
                if let Some(error) = failed(cx, done, "syncing the manifest to disk") {
                    return Step::Ready(Err(error));
                }
                let to = cx.file(manifest::FILE);
                // durable: the manifest's rename is the commit of a flush, a merge or a retire
                cx.ask(Wait::Sync, |tag| Effect::Rename { from: hidden, to, tag });
                self.stage = Committing::Rename;
            }
            Committing::Rename => {
                if let Some(error) = failed(cx, done, "renaming the manifest into place") {
                    return Step::Ready(Err(error));
                }
                let adopted = held.adopting();
                held.committed();
                if !adopted {
                    return Step::Ready(Ok(()));
                }
                // The manifest now says what it said; a reader reads the manifest, never it.
                let file = cx.file(manifest::LEGACY_RETIRED);
                cx.ask(Wait::Files, |tag| Effect::Remove { file, tag });
                self.stage = Committing::Legacy;
            }
            Committing::Legacy => {
                let doing = "deleting the retired files of a history before manifests, adopted";
                if let Err(error) = check(only(done), || cx.path(manifest::LEGACY_RETIRED), doing) {
                    return Step::Ready(Err(error));
                }
                return Step::Ready(Ok(()));
            }
        }
        Step::Pending
    }
}

#[derive(Clone, Copy)]
enum Changing {
    Hold,
    Commit,
    Unlock,
}

/// A change to the manifest under its lock, in one commit: the lock taken and the manifest read
/// ([`Hold`]), a change made to it, committed ([`Commit`]), the lock let go of. What the
/// commit left is the core's manifest.
struct Change {
    stage: Changing,
    hold: Hold,
    held: Option<Held>,
    commit: Commit,
}

impl Change {
    fn new() -> Change {
        Change { stage: Changing::Hold, hold: Hold::new(), held: None, commit: Commit::new() }
    }

    /// Steps the change; `change` is called once, with the manifest held, and may refuse it.
    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        st: &mut State,
        mut done: Option<Vec<Done>>,
        change: impl FnOnce(&mut Cx<'_, H>, &mut State, &mut Held) -> Result<(), HistoryError>,
    ) -> Step<Result<(), HistoryError>> {
        let mut change = Some(change);
        loop {
            match self.stage {
                Changing::Hold => match self.hold.resume(cx, done.take()) {
                    Step::Pending => return Step::Pending,
                    Step::Ready(Err(error)) => return Step::Ready(Err(error)),
                    Step::Ready(Ok(mut held)) => {
                        let Some(change) = change.take() else {
                            unreachable!("a change is made once")
                        };
                        if let Err(error) = change(cx, st, &mut held) {
                            return Step::Ready(Err(error));
                        }
                        self.held = Some(held);
                        self.stage = Changing::Commit;
                    }
                },
                Changing::Commit => {
                    let Some(held) = self.held.as_mut() else {
                        unreachable!("held before its commit")
                    };
                    match self.commit.resume(cx, held, done.take()) {
                        Step::Pending => return Step::Pending,
                        Step::Ready(Err(error)) => return Step::Ready(Err(error)),
                        Step::Ready(Ok(())) => {
                            // clone: the manifest, once a commit
                            st.manifest = held.manifest().clone();
                            cx.unlock(self.hold.lock);
                            self.stage = Changing::Unlock;
                            return Step::Pending;
                        }
                    }
                }
                Changing::Unlock => {
                    return Step::Ready(unlocked(only(done.take()), cx.path(manifest::LOCK)));
                }
            }
        }
    }
}

// --- Placing files ------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Placing {
    Start,
    Truncate,
    Append,
    Sync,
    Rename,
}

/// Whether a placed file is renamed into place, or left at its hidden name for its caller to
/// rename under the manifest's lock ([`Files::write_listed`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Then {
    Rename,
    Stay,
}

/// One file being placed.
struct Slot {
    name: String,
    bytes: Vec<u8>,
    len: u64,
    failed: Option<HistoryError>,
}

/// Files placed, all at once: each made at its hidden name, its bytes appended, synced, and (as
/// [`Then`] says) renamed into place, durably. One that fails leaves the others going.
struct Place {
    stage: Placing,
    then: Then,
    slots: Vec<Slot>,
    /// The slot of each effect of the round, in order.
    issued: Vec<usize>,
}

impl Place {
    fn new(files: impl IntoIterator<Item = (String, Vec<u8>)>, then: Then) -> Place {
        let slots = files
            .into_iter()
            .map(|(name, bytes)| Slot { len: bytes.len() as u64, name, bytes, failed: None })
            // alloc: once a placing
            .collect();
        // alloc: once a placing
        Place { stage: Placing::Start, then, slots, issued: Vec::new() }
    }

    fn resume<H: Host>(&mut self, cx: &mut Cx<'_, H>, mut done: Option<Vec<Done>>) -> Step<()> {
        loop {
            if let Some(done) = done.take() {
                let doing = match self.stage {
                    Placing::Start | Placing::Truncate => "making",
                    Placing::Append => "writing",
                    Placing::Sync => "syncing to disk",
                    Placing::Rename => "renaming into place",
                };
                for (at, done) in self.issued.drain(..).zip(done) {
                    if let Done::Failed(error) = done {
                        let path = match self.stage {
                            Placing::Rename => cx.path(&self.slots[at].name),
                            Placing::Start
                            | Placing::Truncate
                            | Placing::Append
                            | Placing::Sync => cx.path_of(&cx.hidden(&self.slots[at].name)),
                        };
                        self.slots[at].failed = Some(io_failed(path, doing, error));
                    }
                }
            }
            let next = match self.stage {
                Placing::Start => Placing::Truncate,
                Placing::Truncate => Placing::Append,
                Placing::Append if cx.syncs() => Placing::Sync,
                Placing::Append | Placing::Sync => match self.then {
                    Then::Rename => Placing::Rename,
                    Then::Stay => return Step::Ready(()),
                },
                Placing::Rename => return Step::Ready(()),
            };
            self.stage = next;
            for at in 0..self.slots.len() {
                if self.slots[at].failed.is_some() {
                    continue;
                }
                let hidden = cx.hidden(&self.slots[at].name);
                match next {
                    Placing::Start => continue,
                    Placing::Truncate => {
                        cx.ask(Wait::Files, |tag| Effect::Truncate { file: hidden, len: 0, tag });
                    }
                    Placing::Append => {
                        let bytes = std::mem::take(&mut self.slots[at].bytes);
                        cx.ask(Wait::Files, |tag| Effect::Append { file: hidden, bytes, tag });
                    }
                    Placing::Sync => {
                        // durable: a flushed table's bytes are on disk before the manifest lists the file
                        cx.ask(Wait::Sync, |tag| Effect::Sync { file: hidden, tag });
                    }
                    Placing::Rename => {
                        let to = cx.file(&self.slots[at].name);
                        // durable: a flushed table takes its name on disk before the manifest lists it
                        cx.ask(Wait::Sync, |tag| Effect::Rename { from: hidden, to, tag });
                    }
                }
                self.issued.push(at);
            }
            if self.issued.is_empty() {
                done = nothing_asked();
                continue;
            }
            return Step::Pending;
        }
    }

    /// Each file's name, size, and why it was not placed.
    fn take(&mut self) -> Vec<(String, u64, Option<HistoryError>)> {
        // alloc: once a placing
        self.slots.drain(..).map(|it| (it.name, it.len, it.failed)).collect()
    }
}

// --- The cap ------------------------------------------------------------------------------

/// The unit the cap retires a kept file with: its build, or the file alone when of no build.
fn unit_of(at: usize, kept: &Kept) -> (u64, Option<usize>) {
    (kept.name.build, (kept.name.build == 0).then_some(at))
}

/// Whether the kept file at `at` goes with the units `gone`.
fn goes(gone: &BTreeSet<(u64, Option<usize>)>, at: usize, kept: &Kept) -> bool {
    kept.listed == Listed::Listed && gone.contains(&unit_of(at, kept))
}

/// The oldest builds retired until the rest fit under the cap: a build's files go all at once,
/// every table of it, so a reader of a build reads all of it or none. The files of no build
/// (named 0: flushes of an idle daemon) go one at a time. A build is as old as its newest
/// file's flush; the newest flush's build is kept. Retired in one commit, deleted once no pin
/// holds them ([`Reap`]). How many.
struct Trim {
    gone: Option<BTreeSet<(u64, Option<usize>)>>,
    change: Change,
}

impl Trim {
    fn new() -> Trim {
        Trim { gone: None, change: Change::new() }
    }

    /// The units past the cap, oldest first, or none.
    fn plan(st: &State) -> BTreeSet<(u64, Option<usize>)> {
        // alloc: empty unless past the cap
        let mut gone = BTreeSet::new();
        // The mappings and crash records take their room first; the Parquet files the rest.
        let cap = st.cap.saturating_sub(st.side);
        if st.total <= cap {
            return gone;
        }
        let newest = st.kept.back().map_or(0, |it| it.name.last);
        // Each unit: its newest flush, and its bytes.
        // alloc: once a flush past the cap
        let mut units: BTreeMap<(u64, Option<usize>), (u64, u64)> = BTreeMap::new();
        // A file not yet listed is the next flush's to list, not the cap's.
        for (at, kept) in st.kept.iter().enumerate().filter(|(_, it)| it.listed == Listed::Listed) {
            let unit = units.entry(unit_of(at, kept)).or_insert((0, 0));
            unit.0 = unit.0.max(kept.name.last);
            unit.1 += kept.bytes;
        }
        // alloc: once a flush past the cap
        let mut order: Vec<(u64, (u64, Option<usize>), u64)> =
            units.into_iter().map(|(key, (last, bytes))| (last, key, bytes)).collect();
        order.sort_unstable();
        let mut total = st.total;
        for (last, key, bytes) in order {
            if total <= cap {
                break;
            }
            if last == newest {
                continue;
            }
            total -= bytes;
            gone.insert(key);
        }
        gone
    }

    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        st: &mut State,
        done: Option<Vec<Done>>,
    ) -> Step<Result<u32, HistoryError>> {
        let gone = self.gone.get_or_insert_with(|| Trim::plan(st));
        if gone.is_empty() {
            return Step::Ready(Ok(0));
        }
        let step = self.change.resume(cx, st, done, |_, st, held| {
            held.retire(
                st.kept
                    .iter()
                    .enumerate()
                    .filter(|&(at, kept)| goes(gone, at, kept))
                    .map(|(_, kept)| kept.file.as_str()),
            );
            st.reaping = true;
            Ok(())
        });
        match step {
            Step::Pending => Step::Pending,
            Step::Ready(Err(error)) => Step::Ready(Err(error)),
            Step::Ready(Ok(())) => {
                let before = st.kept.len();
                let mut at = 0;
                let mut bytes = 0;
                st.kept.retain(|kept| {
                    let retired = goes(gone, at, kept);
                    at += 1;
                    if retired {
                        bytes += kept.bytes;
                    }
                    !retired
                });
                st.total -= bytes;
                Step::Ready(Ok(u32::try_from(before - st.kept.len()).unwrap_or(u32::MAX)))
            }
        }
    }
}

// --- Reaping retired files ----------------------------------------------------------------

#[derive(Clone, Copy)]
enum Reaping {
    Start,
    Hold,
    StatPins,
    ListPins,
    LockPins,
    RemoveStale,
    UnlockStale,
    RemoveRetired,
    Commit,
    Unlock,
}

/// The retired files no reader's pin holds deleted, then the manifest committed without them:
/// how many. Run only once every retirement is committed, so a reader either read the manifest
/// that retired a file or holds it with its pin ([`super::retire`]). A file is deleted before
/// the manifest stops naming it, so a crash between leaves a name the next reap finds gone. A
/// pin's lock file no one holds is a dead reader's: it is deleted while the reap holds its
/// lock, with what that reader said, and so are a dead process's half-made pins.
struct Reap {
    stage: Reaping,
    hold: Hold,
    held: Option<Held>,
    commit: Commit,
    /// The pins' file names, sorted.
    pins: Vec<String>,
    /// Each pin file looked at and the lock asked of it.
    looked: Vec<(String, Tag)>,
    /// The locks of dead readers' files taken, let go of once they are deleted.
    stale: Vec<Tag>,
    live: Vec<String>,
    reapable: Vec<usize>,
}

impl Reap {
    fn new() -> Reap {
        Reap {
            stage: Reaping::Start,
            hold: Hold::new(),
            held: None,
            commit: Commit::new(),
            // alloc: empty until a reap looks at the pins
            pins: Vec::new(),
            // alloc: as above
            looked: Vec::new(),
            // alloc: as above
            stale: Vec::new(),
            // alloc: as above
            live: Vec::new(),
            // alloc: as above
            reapable: Vec::new(),
        }
    }

    /// Which retired files no live pin holds, then their deletes asked: whether any was.
    fn remove_retired<H: Host>(&mut self, cx: &mut Cx<'_, H>) -> bool {
        // alloc: once a reap
        let live: Vec<&str> = self.live.iter().map(String::as_str).collect();
        // alloc: once a reap
        let said: Vec<(&str, u64)> = self
            .pins
            .iter()
            .filter_map(|name| match PinFile::parse(name) {
                Some(PinFile::Said { id, generation }) => Some((id, generation)),
                Some(PinFile::Lock { .. } | PinFile::Making { .. }) | None => None,
            })
            .collect();
        let pins = retire::pinned(&live, &said);
        let Some(held) = self.held.as_ref() else { unreachable!("held through a reap") };
        self.reapable = held.manifest().retired().reapable(&pins);
        for &at in &self.reapable {
            let file = cx.file(&held.manifest().retired().files[at].0);
            cx.ask(Wait::Reap, |tag| Effect::Remove { file, tag });
        }
        !self.reapable.is_empty()
    }

    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        st: &mut State,
        mut done: Option<Vec<Done>>,
    ) -> Step<Result<u32, HistoryError>> {
        loop {
            match self.stage {
                Reaping::Start => {
                    if !st.reaping {
                        return Step::Ready(Ok(0));
                    }
                    self.stage = Reaping::Hold;
                }
                Reaping::Hold => match self.hold.resume(cx, done.take()) {
                    Step::Pending => return Step::Pending,
                    Step::Ready(Err(error)) => return Step::Ready(Err(error)),
                    Step::Ready(Ok(held)) => {
                        let nothing = held.manifest().retired().files.is_empty();
                        self.held = Some(held);
                        if nothing {
                            self.stage = Reaping::Commit;
                            continue;
                        }
                        let file = cx.file(PINS);
                        cx.ask(Wait::Reap, |tag| Effect::Stat { file, tag });
                        self.stage = Reaping::StatPins;
                        return Step::Pending;
                    }
                },
                Reaping::StatPins => {
                    let doing = "looking for the readers' pins";
                    match check(only(done.take()), || cx.path(PINS), doing) {
                        // No reader ever pinned.
                        Ok(Done::Stat(None)) => {
                            self.stage = Reaping::RemoveRetired;
                            if self.remove_retired(cx) {
                                return Step::Pending;
                            }
                            self.stage = Reaping::Commit;
                        }
                        Ok(Done::Stat(Some(_))) => {
                            let dir = cx.file(PINS);
                            cx.ask(Wait::Reap, |tag| Effect::List { dir, tag });
                            self.stage = Reaping::ListPins;
                            return Step::Pending;
                        }
                        Ok(other) => {
                            return Step::Ready(Err(unexpected(cx.path(PINS), doing, &other)));
                        }
                        Err(error) => return Step::Ready(Err(error)),
                    }
                }
                Reaping::ListPins => {
                    let entries = match check(only(done.take()), || cx.path(PINS), "listing") {
                        Ok(Done::Listed(entries)) => entries,
                        Ok(other) => {
                            return Step::Ready(Err(unexpected(cx.path(PINS), "listing", &other)));
                        }
                        Err(error) => return Step::Ready(Err(error)),
                    };
                    for (file, meta) in &entries {
                        if let Some(name) = cx.name_in_pins(file).filter(|_| !meta.dir) {
                            // alloc: a pin's name, once a reap
                            self.pins.push(name.to_owned());
                        }
                    }
                    self.pins.sort_unstable();
                    for at in 0..self.pins.len() {
                        let look = match PinFile::parse(&self.pins[at]) {
                            Some(PinFile::Lock { .. }) => true,
                            // Pins nothing until it is named; a dead process's is deleted (never
                            // a live process's: it may be between making the file and locking it).
                            Some(it @ PinFile::Making { .. }) => {
                                it.pid().is_some_and(|pid| !cx.host.alive(pid))
                            }
                            Some(PinFile::Said { .. }) | None => false,
                        };
                        if look {
                            let file = cx.pin_file(&self.pins[at]);
                            let tag = cx.ask(Wait::Reap, |tag| Effect::Lock {
                                file,
                                mode: LockMode::Exclusive,
                                tag,
                            });
                            // clone: a pin's name, once a reap
                            self.looked.push((self.pins[at].clone(), tag));
                        }
                    }
                    self.stage = Reaping::LockPins;
                    if self.looked.is_empty() {
                        done = nothing_asked();
                        continue;
                    }
                    return Step::Pending;
                }
                Reaping::LockPins => {
                    let looked = std::mem::take(&mut self.looked);
                    let mut removes = false;
                    for ((name, lock), done) in looked.into_iter().zip(round(done.take())) {
                        let doing = "locking a reader's pin";
                        match check(done, || cx.path(PINS).join(&name), doing) {
                            Ok(Done::Locked) => {
                                // A dead reader's: deleted while this holds its lock, so no
                                // reader takes it meanwhile.
                                cx.locks.push(lock);
                                self.stale.push(lock);
                                let file = cx.pin_file(&name);
                                cx.ask(Wait::Reap, |tag| Effect::Remove { file, tag });
                                removes = true;
                            }
                            Ok(Done::Contended) => {
                                if let Some(PinFile::Lock { id }) = PinFile::parse(&name) {
                                    // alloc: a live pin's id, once a reap
                                    self.live.push(id.to_owned());
                                }
                            }
                            Ok(other) => {
                                return Step::Ready(Err(unexpected(
                                    cx.path(PINS).join(&name),
                                    doing,
                                    &other,
                                )));
                            }
                            Err(error) => return Step::Ready(Err(error)),
                        }
                    }
                    // What a reader whose lock file is not held said: a dead reader's, or one
                    // that ended between the listing and the look (it deletes its own first).
                    // One whose lock file the listing missed while it was named into place
                    // then pins every file until it ends: never fewer.
                    for at in 0..self.pins.len() {
                        if let Some(PinFile::Said { id, .. }) = PinFile::parse(&self.pins[at])
                            && !self.live.iter().any(|it| it == id)
                        {
                            let file = cx.pin_file(&self.pins[at]);
                            cx.ask(Wait::Reap, |tag| Effect::Remove { file, tag });
                            removes = true;
                        }
                    }
                    self.stage = Reaping::RemoveStale;
                    if !removes {
                        done = nothing_asked();
                        continue;
                    }
                    return Step::Pending;
                }
                Reaping::RemoveStale => {
                    for done in round(done.take()) {
                        if let Err(error) =
                            check(done, || cx.path(PINS), "deleting a dead reader's pin file")
                        {
                            return Step::Ready(Err(error));
                        }
                    }
                    let stale = std::mem::take(&mut self.stale);
                    for &lock in &stale {
                        cx.unlock(lock);
                    }
                    self.stage = Reaping::UnlockStale;
                    if stale.is_empty() {
                        done = nothing_asked();
                        continue;
                    }
                    return Step::Pending;
                }
                Reaping::UnlockStale => {
                    for done in round(done.take()) {
                        if let Err(error) = unlocked(done, cx.path(PINS)) {
                            return Step::Ready(Err(error));
                        }
                    }
                    self.stage = Reaping::RemoveRetired;
                    if self.remove_retired(cx) {
                        return Step::Pending;
                    }
                    self.stage = Reaping::Commit;
                }
                Reaping::RemoveRetired => {
                    let reapable = std::mem::take(&mut self.reapable);
                    let Some(held) = self.held.as_mut() else {
                        unreachable!("held through a reap")
                    };
                    for (&at, done) in reapable.iter().zip(round(done.take())) {
                        if let Done::Failed(error) = done {
                            let path = cx.path(&held.manifest().retired().files[at].0);
                            return Step::Ready(Err(io_failed(
                                path,
                                "deleting a retired file no reader pins",
                                error,
                            )));
                        }
                    }
                    held.reaped(&reapable);
                    self.reapable = reapable;
                    self.stage = Reaping::Commit;
                }
                Reaping::Commit => {
                    let Some(held) = self.held.as_mut() else {
                        unreachable!("held before its commit")
                    };
                    match self.commit.resume(cx, held, done.take()) {
                        Step::Pending => return Step::Pending,
                        Step::Ready(Err(error)) => return Step::Ready(Err(error)),
                        Step::Ready(Ok(())) => {
                            // clone: the manifest, once a commit
                            st.manifest = held.manifest().clone();
                            st.reaping = !held.manifest().retired().files.is_empty();
                            cx.unlock(self.hold.lock);
                            self.stage = Reaping::Unlock;
                            return Step::Pending;
                        }
                    }
                }
                Reaping::Unlock => {
                    if let Err(error) = unlocked(only(done.take()), cx.path(manifest::LOCK)) {
                        return Step::Ready(Err(error));
                    }
                    return Step::Ready(Ok(u32::try_from(self.reapable.len()).unwrap_or(u32::MAX)));
                }
            }
        }
    }
}

// --- Merging ------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Merging {
    Next,
    Map,
    Write,
    Sync,
    Rename,
    Change,
}

/// A merge as [`compact::plan`] says: its inputs' names in seq order, and the name of the
/// file they become.
struct Merge {
    inputs: Vec<String>,
    output: Name,
}

/// The merges after a flush, one at a time: each output streamed to its hidden name a piece at
/// a time, synced, renamed into place, then listed in one commit that retires its inputs
/// (deleted once no pin holds them, [`Reap`]): a reader reads the inputs or the output, never
/// both. A merge that fails leaves every input listed and ends the compaction; a commit that
/// fails leaves the output unlisted, which the next open deletes.
struct Compact {
    stage: Merging,
    merges: VecDeque<Merge>,
    merge: Option<Merge>,
    concat: Option<merge::Concat>,
    bytes: u64,
    entry: Option<Entry>,
    change: Change,
    merged: u32,
    compacted: u32,
}

impl Compact {
    fn new(st: &State, ended: Option<u64>) -> Compact {
        // alloc: once a flush
        let ours: Vec<usize> = (0..st.kept.len())
            .filter(|&at| st.kept[at].ours == Ours::Ours && st.kept[at].listed == Listed::Listed)
            .collect();
        // alloc: once a flush
        let names: Vec<Name> = ours.iter().map(|&at| st.kept[at].name).collect();
        let merges = compact::plan(&names, ended)
            .into_iter()
            .map(|merge| Merge {
                // clone: a merge's names, once a merge
                inputs: merge.inputs.iter().map(|&at| st.kept[ours[at]].file.clone()).collect(),
                output: merge.output,
            })
            // alloc: once a flush that compacts
            .collect();
        Compact {
            stage: Merging::Next,
            merges,
            merge: None,
            concat: None,
            bytes: 0,
            entry: None,
            change: Change::new(),
            merged: 0,
            compacted: 0,
        }
    }

    fn current(&self) -> &Merge {
        match self.merge.as_ref() {
            Some(merge) => merge,
            None => unreachable!("a merge under way"),
        }
    }

    fn fail<H: Host>(
        &self,
        cx: &Cx<'_, H>,
        file: PathBuf,
        doing: &'static str,
        error: CompactError,
    ) -> HistoryError {
        HistoryError::Compact {
            output: cx.path(&self.current().output.file_name()),
            file,
            doing,
            error,
        }
    }

    /// The error of a merge's write of its hidden file that failed doing `doing`.
    fn failed_write<H: Host>(
        &self,
        cx: &Cx<'_, H>,
        done: Done,
        doing: &'static str,
    ) -> Option<HistoryError> {
        let Done::Failed(error) = done else { return None };
        let hidden = cx.path_of(&cx.hidden(&self.current().output.file_name()));
        Some(self.fail(cx, hidden, doing, CompactError::Io(io::Error::other(error.0))))
    }

    /// The error of [`merge::Concat`] failing.
    fn concat_failed<H: Host>(&self, cx: &Cx<'_, H>, failed: merge::Failed) -> HistoryError {
        let merge = self.current();
        let file = match failed.input {
            Some(at) => cx.path(&merge.inputs[at]),
            None => cx.path_of(&cx.hidden(&merge.output.file_name())),
        };
        self.fail(cx, file, failed.doing, failed.error)
    }

    /// Asks for the next piece of the output, or its sync once all are written.
    fn write_next<H: Host>(&mut self, cx: &mut Cx<'_, H>) -> Result<(), HistoryError> {
        let Some(concat) = self.concat.as_mut() else { unreachable!("a merge under way") };
        let piece = concat.next();
        let hidden = cx.hidden(&self.current().output.file_name());
        match piece {
            Err(failed) => Err(self.concat_failed(cx, failed)),
            Ok(Some(bytes)) => {
                self.bytes += bytes.len() as u64;
                cx.ask(Wait::Merge, |tag| Effect::Append { file: hidden, bytes, tag });
                self.stage = Merging::Write;
                Ok(())
            }
            Ok(None) => {
                // The inputs' mappings let go of before anything retires them.
                self.concat = None;
                // Synced before it is renamed: the commit that lists it retires its inputs,
                // which a reap deletes, so its bytes must be on disk first.
                if cx.syncs() {
                    // durable: a merge's output is on disk before the commit that retires its inputs
                    cx.ask(Wait::Sync, |tag| Effect::Sync { file: hidden, tag });
                    self.stage = Merging::Sync;
                } else {
                    let to = cx.file(&self.current().output.file_name());
                    // durable: a merge's output takes its name on disk before the commit that retires its inputs
                    cx.ask(Wait::Sync, |tag| Effect::Rename { from: hidden, to, tag });
                    self.stage = Merging::Rename;
                }
                Ok(())
            }
        }
    }

    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        st: &mut State,
        mut done: Option<Vec<Done>>,
    ) -> Step<Result<(), HistoryError>> {
        loop {
            match self.stage {
                Merging::Next => {
                    let Some(merge) = self.merges.pop_front() else { return Step::Ready(Ok(())) };
                    for input in &merge.inputs {
                        let file = cx.file(input);
                        cx.ask(Wait::Merge, |tag| Effect::Map { file, tag });
                    }
                    self.merge = Some(merge);
                    self.stage = Merging::Map;
                    return Step::Pending;
                }
                Merging::Map => {
                    // alloc: once a merge
                    let mut inputs = Vec::with_capacity(self.current().inputs.len());
                    for (at, done) in round(done.take()).into_iter().enumerate() {
                        let path = cx.path(&self.current().inputs[at]);
                        let mapping = match done {
                            Done::Mapped(it) => it,
                            Done::Failed(error) => {
                                let error = CompactError::Io(io::Error::other(error.0));
                                return Step::Ready(Err(self.fail(
                                    cx,
                                    path,
                                    "opening the input",
                                    error,
                                )));
                            }
                            other => {
                                return Step::Ready(Err(unexpected(
                                    path,
                                    "opening the input",
                                    &other,
                                )));
                            }
                        };
                        match merge::Input::of(mapping) {
                            Ok(it) => inputs.push(it),
                            Err(error) => {
                                return Step::Ready(Err(self.fail(
                                    cx,
                                    path,
                                    "reading the footer of the input",
                                    error,
                                )));
                            }
                        }
                    }
                    let table = self.current().output.table;
                    let Some(properties) = st.properties.get(table) else {
                        unreachable!("a merge's table is one of the history's")
                    };
                    match merge::Concat::new(inputs, properties) {
                        Ok(it) => self.concat = Some(it),
                        Err(failed) => return Step::Ready(Err(self.concat_failed(cx, failed))),
                    }
                    let hidden = cx.hidden(&self.current().output.file_name());
                    self.bytes = 0;
                    cx.ask(Wait::Merge, |tag| Effect::Truncate { file: hidden, len: 0, tag });
                    self.stage = Merging::Write;
                    return Step::Pending;
                }
                Merging::Write => {
                    if let Some(error) = self.failed_write(cx, only(done.take()), "writing") {
                        return Step::Ready(Err(error));
                    }
                    if let Err(error) = self.write_next(cx) {
                        return Step::Ready(Err(error));
                    }
                    return Step::Pending;
                }
                Merging::Sync => {
                    if let Some(error) = self.failed_write(cx, only(done.take()), "syncing to disk")
                    {
                        return Step::Ready(Err(error));
                    }
                    let hidden = cx.hidden(&self.current().output.file_name());
                    let to = cx.file(&self.current().output.file_name());
                    // durable: a merge's output takes its name on disk before the commit that retires its inputs
                    cx.ask(Wait::Sync, |tag| Effect::Rename { from: hidden, to, tag });
                    self.stage = Merging::Rename;
                    return Step::Pending;
                }
                Merging::Rename => {
                    if let Some(error) =
                        self.failed_write(cx, only(done.take()), "renaming into place")
                    {
                        return Step::Ready(Err(error));
                    }
                    let merge = self.current();
                    let entry = Entry::merged(
                        st.kept
                            .iter()
                            .filter(|it| merge.inputs.contains(&it.file))
                            .map(|it| &it.entry),
                    );
                    let Some(entry) = entry else {
                        let path = cx.path(&merge.output.file_name());
                        let doing =
                            "taking the entries of its inputs, of which the history keeps none";
                        return Step::Ready(Err(self.fail(
                            cx,
                            path,
                            doing,
                            CompactError::SchemaDiffers,
                        )));
                    };
                    self.entry = Some(entry);
                    self.change = Change::new();
                    self.stage = Merging::Change;
                }
                Merging::Change => {
                    let Some(merge) = self.merge.as_ref() else {
                        unreachable!("a merge under way")
                    };
                    let Some(entry) = self.entry.as_ref() else { unreachable!("a merge's entry") };
                    // The commit point: the output listed and its inputs retired at once, so a
                    // reader reads the rows once.
                    let step = self.change.resume(cx, st, done.take(), |cx, st, held| {
                        // clone: a file's entry, once a merge
                        held.add(&merge.output.file_name(), entry.clone())
                            .map_err(|why| manifest_error(cx, why))?;
                        held.retire(merge.inputs.iter().map(String::as_str));
                        st.reaping = true;
                        Ok(())
                    });
                    match step {
                        Step::Pending => return Step::Pending,
                        Step::Ready(Err(error)) => return Step::Ready(Err(error)),
                        Step::Ready(Ok(())) => {}
                    }
                    let Some(merge) = self.merge.take() else { unreachable!("a merge under way") };
                    let Some(entry) = self.entry.take() else { unreachable!("a merge's entry") };
                    let mut bytes_gone = 0;
                    st.kept.retain(|it| {
                        let merged = merge.inputs.contains(&it.file);
                        if merged {
                            bytes_gone += it.bytes;
                        }
                        !merged
                    });
                    st.total -= bytes_gone;
                    let at = st.kept.partition_point(|it| it.name.last <= merge.output.last);
                    st.kept.insert(
                        at,
                        Kept {
                            name: merge.output,
                            file: merge.output.file_name(),
                            bytes: self.bytes,
                            entry,
                            ours: Ours::Ours,
                            listed: Listed::Listed,
                        },
                    );
                    st.total += self.bytes;
                    self.compacted += 1;
                    self.merged += u32::try_from(merge.inputs.len()).unwrap_or(u32::MAX);
                    self.stage = Merging::Next;
                }
            }
        }
    }
}

// --- Recovering dead daemons' rows --------------------------------------------------------

#[derive(Clone, Copy)]
enum Recovering {
    Start,
    Lock,
    Map,
    Next,
    Place,
    Change,
    Remove,
    Released,
}

/// The rows each dead daemon's flight recorder mapping holds past its last flush, written as
/// files of their own, one flush number a daemon, and each mapping deleted once its rows are
/// written and listed. A mapping a live process holds (its lock contended) is left alone. A
/// mapping that can't be read stays, and the first error names it; the others are read.
///
/// Each mapping's files are listed in one commit that names the mapping recovered, before it is
/// deleted: a crash before the commit leaves unlisted files the next open deletes and the
/// mapping it reads again; one after it, a mapping the next open deletes unread. Never a row
/// twice.
struct Recover {
    stage: Recovering,
    /// Each mapping's pid, then with the lock asked of it.
    named: Vec<u32>,
    locks: Vec<(u32, Tag)>,
    /// The mappings locked and mapped.
    found: VecDeque<(u32, Tag, Mapping)>,
    /// The mapping under way: its pid and lock.
    at: Option<(u32, Tag)>,
    nonce: u64,
    place: Option<Place>,
    /// The files of the mapping under way, placed: name, size, entry.
    files: Vec<(String, u64, Entry)>,
    change: Change,
    written: usize,
    failed: Option<HistoryError>,
}

impl Recover {
    /// Recovers the mappings named in `listing`, the history's directory's files.
    fn new(listing: &[(String, Meta)]) -> Recover {
        // alloc: once a postmortem
        let mut named: Vec<u32> =
            listing.iter().filter_map(|(name, _)| postmortem::pid_of(name)).collect();
        named.sort_unstable();
        Recover {
            stage: Recovering::Start,
            named,
            // alloc: empty until a mapping is found
            locks: Vec::new(),
            found: VecDeque::new(),
            at: None,
            nonce: 0,
            place: None,
            // alloc: empty until a mapping is read
            files: Vec::new(),
            change: Change::new(),
            written: 0,
            failed: None,
        }
    }

    fn note(&mut self, error: HistoryError) {
        self.failed.get_or_insert(error);
    }

    fn mapping_error<H: Host>(
        cx: &Cx<'_, H>,
        pid: u32,
        doing: &'static str,
        error: IoError,
    ) -> HistoryError {
        HistoryError::Postmortem(postmortem::PostmortemError::Io {
            path: cx.path(&postmortem::file_name(pid)),
            doing,
            error: io::Error::other(error.0),
        })
    }

    /// Ends the mapping under way: lets go of its lock, and of the manifest's if a change of it
    /// failed holding it. The round's completions are taken by [`Recovering::Released`].
    fn release<H: Host>(&mut self, cx: &mut Cx<'_, H>) {
        let manifest = self.change.hold.lock;
        if cx.locks.contains(&manifest) {
            cx.unlock(manifest);
        }
        if let Some((_, lock)) = self.at {
            cx.unlock(lock);
        }
        self.stage = Recovering::Released;
    }

    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        st: &mut State,
        mut done: Option<Vec<Done>>,
    ) -> Step<(usize, Option<HistoryError>)> {
        loop {
            match self.stage {
                Recovering::Start => {
                    if self.named.is_empty() {
                        return Step::Ready((0, None));
                    }
                    for at in 0..self.named.len() {
                        let pid = self.named[at];
                        let file = cx.file(&postmortem::file_name(pid));
                        let tag = cx.ask(Wait::Files, |tag| Effect::Lock {
                            file,
                            mode: LockMode::Exclusive,
                            tag,
                        });
                        self.locks.push((pid, tag));
                    }
                    self.stage = Recovering::Lock;
                    return Step::Pending;
                }
                Recovering::Lock => {
                    let locks = std::mem::take(&mut self.locks);
                    for ((pid, lock), done) in locks.into_iter().zip(round(done.take())) {
                        match done {
                            Done::Locked => {
                                cx.locks.push(lock);
                                self.locks.push((pid, lock));
                                let file = cx.file(&postmortem::file_name(pid));
                                cx.ask(Wait::Files, |tag| Effect::Map { file, tag });
                            }
                            // Its process is alive, or another reader holds it.
                            Done::Contended => {}
                            Done::Failed(error) => {
                                let error = Recover::mapping_error(cx, pid, "locking", error);
                                self.note(error);
                            }
                            other => {
                                let error = unexpected(
                                    cx.path(&postmortem::file_name(pid)),
                                    "locking",
                                    &other,
                                );
                                self.note(error);
                            }
                        }
                    }
                    self.stage = Recovering::Map;
                    if self.locks.is_empty() {
                        done = nothing_asked();
                        continue;
                    }
                    return Step::Pending;
                }
                Recovering::Map => {
                    let locks = std::mem::take(&mut self.locks);
                    let mut unlocks = false;
                    for ((pid, lock), done) in locks.into_iter().zip(round(done.take())) {
                        match done {
                            Done::Mapped(mapping) => self.found.push_back((pid, lock, mapping)),
                            Done::Failed(error) => {
                                let error = Recover::mapping_error(cx, pid, "opening", error);
                                self.note(error);
                                cx.unlock(lock);
                                unlocks = true;
                            }
                            other => {
                                let error = unexpected(
                                    cx.path(&postmortem::file_name(pid)),
                                    "opening",
                                    &other,
                                );
                                self.note(error);
                                cx.unlock(lock);
                                unlocks = true;
                            }
                        }
                    }
                    self.stage = Recovering::Released;
                    if unlocks {
                        return Step::Pending;
                    }
                    done = nothing_asked();
                }
                Recovering::Released => {
                    for done in round(done.take()) {
                        if let Err(error) = unlocked(done, cx.dir_path()) {
                            self.note(error);
                        }
                    }
                    self.at = None;
                    self.place = None;
                    self.files.clear();
                    self.stage = Recovering::Next;
                }
                Recovering::Next => {
                    let Some((pid, lock, mapping)) = self.found.pop_front() else {
                        return Step::Ready((self.written, self.failed.take()));
                    };
                    self.at = Some((pid, lock));
                    let dead = Dead {
                        pid,
                        path: cx.path(&postmortem::file_name(pid)),
                        mapping,
                        recorder: st.manifest.consumed().recorders.get(&pid).copied(),
                        seq: st.seq,
                    };
                    match cx.host.recover(dead) {
                        Recovery::Already => {
                            let file = cx.file(&postmortem::file_name(pid));
                            cx.ask(Wait::Files, |tag| Effect::Remove { file, tag });
                            self.stage = Recovering::Remove;
                            return Step::Pending;
                        }
                        Recovery::Refused(error) => {
                            self.note(error);
                            self.release(cx);
                            return Step::Pending;
                        }
                        Recovery::Files { nonce, files } => {
                            self.nonce = nonce;
                            // alloc: once a postmortem
                            let mut placing = Vec::with_capacity(files.len());
                            for file in files {
                                // clone: a file's name, kept with its entry until it is listed
                                self.files.push((
                                    file.file.clone(),
                                    file.bytes.len() as u64,
                                    file.entry,
                                ));
                                placing.push((file.file, file.bytes));
                            }
                            self.place = Some(Place::new(placing, Then::Rename));
                            self.stage = Recovering::Place;
                        }
                    }
                }
                Recovering::Place => {
                    let Some(place) = self.place.as_mut() else {
                        unreachable!("placing a postmortem's files")
                    };
                    match place.resume(cx, done.take()) {
                        Step::Pending => return Step::Pending,
                        Step::Ready(()) => {
                            if let Some(error) = place.take().into_iter().find_map(|it| it.2) {
                                // Placed files stay unlisted, the next open's to delete; the
                                // mapping stays for the next to read.
                                self.note(error);
                                self.release(cx);
                                return Step::Pending;
                            }
                            self.change = Change::new();
                            self.stage = Recovering::Change;
                        }
                    }
                }
                Recovering::Change => {
                    let Some((pid, _)) = self.at else { unreachable!("a mapping under way") };
                    let nonce = self.nonce;
                    let files = &self.files;
                    let step = self.change.resume(cx, st, done.take(), |cx, _, held| {
                        for (name, _, entry) in files {
                            // clone: a file's entry, once listed
                            held.add(name, entry.clone()).map_err(|why| manifest_error(cx, why))?;
                        }
                        if nonce != 0 {
                            held.recorder_recovered(pid, nonce);
                        }
                        Ok(())
                    });
                    match step {
                        Step::Pending => return Step::Pending,
                        Step::Ready(Err(error)) => {
                            self.note(error);
                            self.release(cx);
                            return Step::Pending;
                        }
                        Step::Ready(Ok(())) => {}
                    }
                    for (name, bytes, entry) in std::mem::take(&mut self.files) {
                        let Some(named) = Name::parse(&name) else {
                            unreachable!("a postmortem's file `{name}` is named as a flush's")
                        };
                        st.last_build = st.last_build.max(named.build);
                        st.total += bytes;
                        // Not this history's to merge: of another process, as a file there
                        // before it opened.
                        st.kept.push_back(Kept {
                            name: named,
                            file: name,
                            bytes,
                            entry,
                            ours: Ours::Theirs,
                            listed: Listed::Listed,
                        });
                        self.written += 1;
                    }
                    // The next mapping's files take the next flush number.
                    st.seq += 1;
                    let file = cx.file(&postmortem::file_name(pid));
                    cx.ask(Wait::Files, |tag| Effect::Remove { file, tag });
                    self.stage = Recovering::Remove;
                    return Step::Pending;
                }
                Recovering::Remove => {
                    let Some((pid, _)) = self.at else { unreachable!("a mapping under way") };
                    if let Done::Failed(error) = only(done.take()) {
                        let error =
                            Recover::mapping_error(cx, pid, "deleting, its rows written,", error);
                        self.note(error);
                    }
                    self.release(cx);
                    return Step::Pending;
                }
            }
        }
    }
}

// --- Jobs ---------------------------------------------------------------------------------

/// An operation asked of the core, run one at a time, in the order asked.
enum Job {
    Open(Box<Open>),
    Flush(Box<FlushJob>),
    Recount(Box<Recount>),
    WriteListed(Box<WriteListed>),
    Read(Read),
    Schemas(Box<Schemas>),
    RecoverDead(Box<RecoverDead>),
    Pin(Box<PinJob>),
    Unpin(Box<Unpin>),
}

impl Job {
    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        st: &mut State,
        done: Option<Vec<Done>>,
    ) -> Step<Result<Outcome, HistoryError>> {
        match self {
            Job::Open(job) => job.resume(cx, st, done),
            Job::Flush(job) => job.resume(cx, st, done),
            Job::Recount(job) => job.resume(cx, st, done),
            Job::WriteListed(job) => job.resume(cx, st, done),
            Job::Read(read) => match read.resume(cx, done) {
                Step::Pending => Step::Pending,
                Step::Ready(result) => Step::Ready(Ok(Outcome::Read(result.map(|it| it.0)))),
            },
            Job::Schemas(job) => job.resume(cx, done),
            Job::RecoverDead(job) => job.resume(cx, st, done),
            Job::Pin(job) => job.resume(cx, done),
            Job::Unpin(job) => job.resume(cx, done),
        }
    }

    /// The outcome of the job when it failed with `error`, its locks let go of.
    fn failed(&mut self, error: HistoryError) -> Outcome {
        match self {
            Job::Open(_) => Outcome::Opened(Err(error)),
            Job::Flush(job) => Outcome::Flushed(job.failed(error)),
            Job::Recount(_) => Outcome::Recounted(Err(error)),
            Job::WriteListed(_) => Outcome::WroteListed(Err(error)),
            Job::Read(_) => Outcome::Read(Err(error)),
            Job::Schemas(_) => Outcome::Schemas(Err(error)),
            Job::RecoverDead(_) => Outcome::RecoveredDead(Err(error)),
            Job::Pin(_) => Outcome::Pinned(Err(error)),
            Job::Unpin(_) => Outcome::Unpinned,
        }
    }
}

// --- Open ---------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Opening {
    Hold,
    List,
    Sweep,
    Crashes,
    Commit,
    Unlock,
    Schemas,
    Recover,
    Trim,
    Reap,
}

/// What the open does with a crash record read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CrashFile {
    /// Nothing recorded, or its row listed already: deleted.
    Delete,
    /// Refused: renamed `crash.<pid>.refused`.
    SetAside,
}

/// The history as its open finds it, under the manifest's lock: what a crash left is deleted
/// (each hidden file a write left half done, and each file of a form blaze writes that the
/// manifest neither lists nor retires: a merge's output not yet committed), a retired file is
/// left for the reap, and a file the manifest lists that is not there is
/// [`ManifestError::Missing`]. Each gone process's crash record is read: a row for the first
/// flush, or deleted unread when the manifest says a listed file holds its row; one refused is
/// set aside as `crash.<pid>.refused` and said. Then the schema files are written over, the
/// rows dead daemons left are recovered ([`Recover`]), and the cap and the reap run.
struct Open {
    stage: Opening,
    hold: Hold,
    held: Option<Held>,
    commit: Commit,
    listing: Vec<(String, Meta)>,
    /// The sweep's deletes, then the crash records read and what is done with each.
    removes: Vec<String>,
    crash_reads: Vec<String>,
    crash_files: Vec<(String, CrashFile)>,
    crashes: crash::Recovered,
    schemas: Option<Place>,
    schema_files: Vec<(String, Vec<u8>)>,
    recover: Option<Recover>,
    postmortem: Option<HistoryError>,
    trim: Trim,
    reap: Reap,
}

impl Open {
    /// The sweep of the listing under the held manifest: the deletes and crash records asked
    /// for, and the core's state taken from the manifest.
    fn sweep<H: Host>(&mut self, cx: &mut Cx<'_, H>, st: &mut State) -> Result<(), HistoryError> {
        let Some(held) = self.held.as_mut() else { unreachable!("held through the open") };
        // alloc: once a history
        let mut present: BTreeSet<&str> = BTreeSet::new();
        for (name, _) in &self.listing {
            let listed = held.manifest().contains(name);
            let retired = held.manifest().retired().holds(name);
            match manifest::kind(name) {
                // clone: a name, once a history
                manifest::Kind::Hidden if half_written(&*cx.host, name) => {
                    self.removes.push(name.clone())
                }
                // A newer blaze's table, listed: kept as written, read by no view of this blaze.
                manifest::Kind::Listable | manifest::Kind::Unclassified if listed => {
                    present.insert(name);
                }
                // Retired and not yet deleted: a reader's pin may hold it ([`Reap`]).
                manifest::Kind::Listable | manifest::Kind::Unclassified if retired => {}
                // clone: a name, once a history
                manifest::Kind::Listable => self.removes.push(name.clone()),
                manifest::Kind::Unclassified => {
                    // clone: an error
                    return Err(HistoryError::Unclassified {
                        dir: cx.dir_path(),
                        file: name.clone(),
                    });
                }
                manifest::Kind::Hidden | manifest::Kind::Schema | manifest::Kind::Other => {}
            }
        }
        if let Some(missing) = held.manifest().all().find(|it| !present.contains(*it)) {
            // alloc: an error
            let why = ManifestError::Missing { file: missing.to_owned() };
            return Err(manifest_error(cx, why));
        }
        // The files the cap counts, oldest first: the flushes', merges' and derived tables';
        // not a perf run's.
        st.kept.clear();
        st.last_build = 0;
        for name in held.manifest().all() {
            let (Some(named), Some(entry)) = (Name::parse(name), held.manifest().entry(name))
            else {
                continue;
            };
            st.last_build = st.last_build.max(named.build);
            let bytes =
                self.listing.iter().find(|(it, _)| it == name).map_or(0, |(_, meta)| meta.len);
            st.kept.push_back(Kept {
                name: named,
                // alloc: a file's name, once a history
                file: name.to_owned(),
                bytes,
                // clone: a file's entry, once a history
                entry: entry.clone(),
                ours: Ours::Theirs,
                listed: Listed::Listed,
            });
        }
        st.kept
            .make_contiguous()
            .sort_unstable_by(|a, b| (a.name.last, &a.file).cmp(&(b.name.last, &b.file)));
        st.total = st.kept.iter().map(|it| it.bytes).sum();
        st.side = side_bytes(&self.listing);
        let last = st.kept.iter().map(|it| it.name.last).max().unwrap_or(0);
        st.seq = last.max(held.manifest().flushed()) + 1;
        // What a past commit consumed of a mapping or crash record since deleted is forgotten.
        let listing = &self.listing;
        held.prune_consumed(|name| !listing.iter().any(|(it, _)| it == name));
        // Each gone process's crash record.
        for (name, _) in &self.listing {
            let Some(pid) = crash::pid_of(name) else { continue };
            if pid == cx.host.pid() || cx.host.alive(pid) {
                continue;
            }
            // clone: a name, once a crash record
            self.crash_reads.push(name.clone());
        }
        for name in &self.removes {
            let file = cx.file(name);
            cx.ask(Wait::Files, |tag| Effect::Remove { file, tag });
        }
        for name in &self.crash_reads {
            let file = cx.file(name);
            cx.ask(Wait::Files, |tag| Effect::ReadAll { file, tag });
        }
        Ok(())
    }

    /// The deletes and the crash records' bytes come back: each record read by the host, and
    /// what is done with each file asked.
    fn swept<H: Host>(&mut self, cx: &mut Cx<'_, H>, done: Vec<Done>) -> Result<(), HistoryError> {
        let mut done = done.into_iter();
        for name in &self.removes {
            let Some(it) = done.next() else { unreachable!("one completion an effect") };
            if let Done::Failed(error) = it {
                let doing = match manifest::kind(name) {
                    manifest::Kind::Hidden => "deleting a half-written file",
                    manifest::Kind::Listable
                    | manifest::Kind::Schema
                    | manifest::Kind::Other
                    | manifest::Kind::Unclassified => {
                        "deleting a file no manifest lists (a crash left it)"
                    }
                };
                return Err(io_failed(cx.path(name), doing, error));
            }
        }
        let Some(held) = self.held.as_ref() else { unreachable!("held through the open") };
        for (name, it) in std::mem::take(&mut self.crash_reads).into_iter().zip(done) {
            let path = cx.path(&name);
            let bytes = match it {
                Done::Read(bytes) => bytes,
                Done::Failed(error) => {
                    let error = io::Error::other(error.0);
                    self.crashes.refused.push(crash::CrashError::Io {
                        path,
                        doing: "reading",
                        error,
                    });
                    continue;
                }
                other => {
                    // alloc: an error
                    let error = io::Error::other(format!("the driver answered {other:?}"));
                    self.crashes.refused.push(crash::CrashError::Io {
                        path,
                        doing: "reading",
                        error,
                    });
                    continue;
                }
            };
            match cx.host.crash(&path, &bytes) {
                CrashRead::Nothing => self.crash_files.push((name, CrashFile::Delete)),
                // A record whose row a listed file holds: a crash came between that commit and
                // its delete. Deleted unread, never written twice.
                CrashRead::Row(crash)
                    if held.manifest().consumed().crashes.contains(&(crash.pid, crash.t_ns)) =>
                {
                    self.crash_files.push((name, CrashFile::Delete));
                }
                CrashRead::Row(crash) => {
                    self.crashes.ids.push((crash.pid, crash.t_ns));
                    self.crashes.rows.push(crash.row);
                    if let Some(image) = crash.image {
                        self.crashes.images.push((crash.pid, image));
                    }
                    self.crashes.files.push(name);
                }
                CrashRead::Refused(refused) => {
                    self.crashes.refused.push(refused);
                    self.crash_files.push((name, CrashFile::SetAside));
                }
                CrashRead::Unread(error) => self.crashes.refused.push(error),
            }
        }
        for (name, what) in &self.crash_files {
            let file = cx.file(name);
            match what {
                CrashFile::Delete => {
                    cx.ask(Wait::Files, |tag| Effect::Remove { file, tag });
                }
                CrashFile::SetAside => {
                    // alloc: a file's name, once a record refused
                    let to = cx.file(&format!("{name}.refused"));
                    // durable: a refused crash record is set aside once: a restart must not find it under its old name and report it again
                    cx.ask(Wait::Files, |tag| Effect::Rename { from: file, to, tag });
                }
            }
        }
        Ok(())
    }

    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        st: &mut State,
        mut done: Option<Vec<Done>>,
    ) -> Step<Result<Outcome, HistoryError>> {
        loop {
            match self.stage {
                Opening::Hold => match self.hold.resume(cx, done.take()) {
                    Step::Pending => return Step::Pending,
                    Step::Ready(Err(error)) => return Step::Ready(Err(error)),
                    Step::Ready(Ok(held)) => {
                        self.held = Some(held);
                        let dir = cx.dir.clone(); // clone: a refcount
                        cx.ask(Wait::Files, |tag| Effect::List { dir, tag });
                        self.stage = Opening::List;
                        return Step::Pending;
                    }
                },
                Opening::List => {
                    self.listing = match listing(cx, only(done.take())) {
                        Ok(it) => it,
                        Err(error) => return Step::Ready(Err(error)),
                    };
                    if let Err(error) = self.sweep(cx, st) {
                        return Step::Ready(Err(error));
                    }
                    self.stage = Opening::Sweep;
                    if self.removes.is_empty() && self.crash_reads.is_empty() {
                        done = nothing_asked();
                        continue;
                    }
                    return Step::Pending;
                }
                Opening::Sweep => {
                    if let Err(error) = self.swept(cx, round(done.take())) {
                        return Step::Ready(Err(error));
                    }
                    self.stage = Opening::Crashes;
                    if self.crash_files.is_empty() {
                        done = nothing_asked();
                        continue;
                    }
                    return Step::Pending;
                }
                Opening::Crashes => {
                    for ((name, what), it) in self.crash_files.iter().zip(round(done.take())) {
                        if let Done::Failed(error) = it {
                            let doing = match what {
                                CrashFile::Delete => {
                                    "deleting a crash record with nothing recorded, or whose row the history lists"
                                }
                                CrashFile::SetAside => "setting aside a refused crash record",
                            };
                            let error = io::Error::other(error.0);
                            self.crashes.refused.push(crash::CrashError::Io {
                                path: cx.path(name),
                                doing,
                                error,
                            });
                        }
                    }
                    // A build of a daemon that crashed before a flush named a file for it keeps
                    // its number.
                    st.last_build = self
                        .crashes
                        .rows
                        .iter()
                        .filter_map(|it| it.build_id)
                        .fold(st.last_build, u64::max);
                    self.stage = Opening::Commit;
                }
                Opening::Commit => {
                    let Some(held) = self.held.as_mut() else {
                        unreachable!("held through the open")
                    };
                    match self.commit.resume(cx, held, done.take()) {
                        Step::Pending => return Step::Pending,
                        Step::Ready(Err(error)) => return Step::Ready(Err(error)),
                        Step::Ready(Ok(())) => {
                            // clone: the manifest, once a history
                            st.manifest = held.manifest().clone();
                            // The first reap looks: a manifest a crash left may name retired files.
                            st.reaping = true;
                            cx.unlock(self.hold.lock);
                            self.stage = Opening::Unlock;
                            return Step::Pending;
                        }
                    }
                }
                Opening::Unlock => {
                    if let Err(error) = unlocked(only(done.take()), cx.path(manifest::LOCK)) {
                        return Step::Ready(Err(error));
                    }
                    // An empty `schema.<table>.parquet` of each table, each written over: a
                    // writer of the history makes them its own blaze's columns.
                    self.schemas =
                        Some(Place::new(std::mem::take(&mut self.schema_files), Then::Rename));
                    self.stage = Opening::Schemas;
                }
                Opening::Schemas => {
                    let Some(place) = self.schemas.as_mut() else {
                        unreachable!("placing the schema files")
                    };
                    match place.resume(cx, done.take()) {
                        Step::Pending => return Step::Pending,
                        Step::Ready(()) => {
                            if let Some(error) = place.take().into_iter().find_map(|it| it.2) {
                                return Step::Ready(Err(error));
                            }
                            // The last rows of daemons that died before they flushed them, as
                            // files of their own before this history writes any.
                            self.recover = Some(Recover::new(&self.listing));
                            self.stage = Opening::Recover;
                        }
                    }
                }
                Opening::Recover => {
                    let Some(recover) = self.recover.as_mut() else { unreachable!("recovering") };
                    match recover.resume(cx, st, done.take()) {
                        Step::Pending => return Step::Pending,
                        Step::Ready((_, failed)) => {
                            self.postmortem = failed;
                            self.stage = Opening::Trim;
                        }
                    }
                }
                Opening::Trim => match self.trim.resume(cx, st, done.take()) {
                    Step::Pending => return Step::Pending,
                    Step::Ready(Err(error)) => return Step::Ready(Err(error)),
                    Step::Ready(Ok(_)) => self.stage = Opening::Reap,
                },
                Opening::Reap => match self.reap.resume(cx, st, done.take()) {
                    Step::Pending => return Step::Pending,
                    Step::Ready(Err(error)) => return Step::Ready(Err(error)),
                    Step::Ready(Ok(_)) => {
                        return Step::Ready(Ok(Outcome::Opened(Ok(Opened {
                            seq: st.seq,
                            last_build: st.last_build,
                            crashes: std::mem::take(&mut self.crashes),
                            postmortem: self.postmortem.take(),
                        }))));
                    }
                },
            }
        }
    }
}

// --- Flush --------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flushing {
    Place,
    Change,
    /// A planted bug's second commit, of what the first left out.
    LateChange,
    RemoveCrashes,
    Side,
    Compact,
    Trim,
    Reap,
    /// The locks a failed step left held let go of, then [`FlushJob::after`].
    Release,
}

/// A flush's files placed, then all listed in one commit with what it consumed; then the crash
/// records it consumed deleted, the merges, the cap, the reap. What fails after the commit is
/// deferred: the flush's number stands.
struct FlushJob {
    stage: Flushing,
    after: Flushing,
    seq: u64,
    recorder: Option<(u32, u64)>,
    crashes: Option<(usize, crash::Written)>,
    ended: Option<u64>,
    entries: Vec<Entry>,
    place: Place,
    change: Change,
    committed: bool,
    out: Flushed,
    removing: Vec<String>,
    compact: Option<Compact>,
    trim: Trim,
    reap: Reap,
}

impl FlushJob {
    fn failed(&mut self, error: HistoryError) -> Flushed {
        let mut out = std::mem::replace(&mut self.out, Flushed::empty());
        match self.committed {
            false => out.committed = Err(error),
            true => out.deferred.push(error),
        }
        out
    }

    /// `error` deferred, after the commit: the locks the failed step left held let go of
    /// before the flush goes on with `next`. Whether a round was asked.
    fn defer<H: Host>(&mut self, cx: &mut Cx<'_, H>, error: HistoryError, next: Flushing) -> bool {
        self.out.deferred.push(error);
        let locks = std::mem::take(&mut *cx.locks);
        for &lock in &locks {
            cx.ask(Wait::Files, |tag| Effect::Unlock { lock, tag });
        }
        self.after = next;
        self.stage = match locks.is_empty() {
            true => next,
            false => Flushing::Release,
        };
        !locks.is_empty()
    }

    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        st: &mut State,
        mut done: Option<Vec<Done>>,
    ) -> Step<Result<Outcome, HistoryError>> {
        loop {
            match self.stage {
                Flushing::Place => match self.place.resume(cx, done.take()) {
                    Step::Pending => return Step::Pending,
                    Step::Ready(()) => {
                        let entries = std::mem::take(&mut self.entries);
                        // alloc: empty unless a file is not placed
                        let mut unplaced = Vec::new();
                        for (at, ((name, len, failed), entry)) in
                            self.place.take().into_iter().zip(entries).enumerate()
                        {
                            if let Some(error) = failed {
                                // A crash record's row in a file not placed is the next flush's.
                                if self.crashes.as_ref().is_some_and(|(it, _)| *it == at) {
                                    self.crashes = None;
                                }
                                self.out.placed.push(Some(error));
                                unplaced.push(name);
                                continue;
                            }
                            let Some(named) = Name::parse(&name) else {
                                unreachable!("a flush's file `{name}` is named as a flush's")
                            };
                            st.last_build = st.last_build.max(named.build);
                            st.kept.push_back(Kept {
                                name: named,
                                file: name,
                                bytes: len,
                                entry,
                                ours: Ours::Ours,
                                listed: Listed::Unlisted,
                            });
                            st.total += len;
                            self.out.files += 1;
                            self.out.bytes += len;
                            self.out.placed.push(None);
                        }
                        // A crash's records are consumed by the commit that lists its `panics`
                        // row, and only then deleted.
                        if let Some((_, mut written)) = self.crashes.take() {
                            st.listing.append(&mut written);
                        }
                        // The commit says the recorder's flush `seq` is listed, rows of a file
                        // not placed too: a crash between it and the next flush's commit would
                        // lose them, as a postmortem reads past that flush's marks. So a flush
                        // with a file not placed is not committed, as a commit that failed: its
                        // placed files stay unlisted, and so do the crash records they consume,
                        // for the next flush's commit; a crash before it leaves them to the next
                        // open's sweep and every row of this flush to the postmortem.
                        if !unplaced.is_empty() {
                            return Step::Ready(Err(HistoryError::Unplaced {
                                dir: cx.dir_path(),
                                seq: self.seq,
                                files: unplaced,
                            }));
                        }
                        self.stage = Flushing::Change;
                    }
                },
                Flushing::Change => {
                    let consumes = cx.consumes_in_commit();
                    let (seq, recorder) = (self.seq, self.recorder);
                    // The commit point. A commit that fails (another process held the
                    // manifest's lock past its bound) leaves this flush's files in place,
                    // unlisted: the next flush lists them first, and a crash before it leaves
                    // them to the next open's sweep, and their rings' rows to the postmortem,
                    // which reads past the last committed marks.
                    let step = self.change.resume(cx, st, done.take(), |cx, st, held| {
                        // Every file not yet listed: this flush's, and any a failed commit left.
                        for kept in st.kept.iter().filter(|it| it.listed == Listed::Unlisted) {
                            // clone: a file's entry, once a file
                            held.add(&kept.file, kept.entry.clone())
                                .map_err(|why| manifest_error(cx, why))?;
                        }
                        held.flush_committed(seq);
                        if consumes {
                            consume(recorder, seq, st, held);
                        }
                        Ok(())
                    });
                    match step {
                        Step::Pending => return Step::Pending,
                        Step::Ready(Err(error)) => return Step::Ready(Err(error)),
                        Step::Ready(Ok(())) => {}
                    }
                    self.committed = true;
                    for kept in &mut st.kept {
                        kept.listed = Listed::Listed;
                    }
                    self.stage = match consumes {
                        true => Flushing::RemoveCrashes,
                        false => {
                            self.change = Change::new();
                            Flushing::LateChange
                        }
                    };
                }
                Flushing::LateChange => {
                    let (seq, recorder) = (self.seq, self.recorder);
                    let step = self.change.resume(cx, st, done.take(), |_, st, held| {
                        consume(recorder, seq, st, held);
                        Ok(())
                    });
                    match step {
                        Step::Pending => return Step::Pending,
                        Step::Ready(Err(error)) => {
                            if self.defer(cx, error, Flushing::RemoveCrashes) {
                                return Step::Pending;
                            }
                        }
                        Step::Ready(Ok(())) => self.stage = Flushing::RemoveCrashes,
                    }
                }
                Flushing::RemoveCrashes => {
                    st.listing.ids.clear();
                    self.removing = std::mem::take(&mut st.listing.files);
                    if self.removing.is_empty() {
                        self.compact = Some(Compact::new(st, self.ended));
                        self.stage = Flushing::Compact;
                        continue;
                    }
                    for at in 0..self.removing.len() {
                        let file = cx.file(&self.removing[at]);
                        cx.ask(Wait::Files, |tag| Effect::Remove { file, tag });
                    }
                    self.stage = Flushing::Side;
                    return Step::Pending;
                }
                Flushing::Side => {
                    let done = round(done.take());
                    if !self.removing.is_empty() {
                        // The deletes' completions: one that failed is consumed already, and
                        // the next open deletes it unread.
                        for (name, it) in std::mem::take(&mut self.removing).into_iter().zip(done) {
                            if let Done::Failed(error) = it {
                                self.out.undeleted.push(crash::CrashError::Io {
                                    path: cx.path(&name),
                                    doing: "deleting a recovered crash record",
                                    error: io::Error::other(error.0),
                                });
                            }
                        }
                        // Crash records deleted since the cap last counted them.
                        let dir = cx.dir.clone(); // clone: a refcount
                        cx.ask(Wait::Files, |tag| Effect::List { dir, tag });
                        return Step::Pending;
                    }
                    // The listing: one that fails leaves the count as it was, said.
                    let Some(it) = done.into_iter().next() else {
                        unreachable!("one completion an effect")
                    };
                    match listing(cx, it) {
                        Ok(listing) => st.side = side_bytes(&listing),
                        Err(error) => self.out.deferred.push(error),
                    }
                    self.compact = Some(Compact::new(st, self.ended));
                    self.stage = Flushing::Compact;
                }
                Flushing::Compact => {
                    let Some(compact) = self.compact.as_mut() else { unreachable!("compacting") };
                    let step = compact.resume(cx, st, done.take());
                    let (merged, compacted) = (compact.merged, compact.compacted);
                    match step {
                        Step::Pending => return Step::Pending,
                        Step::Ready(result) => {
                            self.out.merged = merged;
                            self.out.compacted = compacted;
                            self.compact = None;
                            self.stage = Flushing::Trim;
                            if let Err(error) = result
                                && self.defer(cx, error, Flushing::Trim)
                            {
                                return Step::Pending;
                            }
                        }
                    }
                }
                Flushing::Trim => match self.trim.resume(cx, st, done.take()) {
                    Step::Pending => return Step::Pending,
                    Step::Ready(Ok(deleted)) => {
                        self.out.deleted = deleted;
                        self.stage = Flushing::Reap;
                    }
                    Step::Ready(Err(error)) => {
                        if self.defer(cx, error, Flushing::Reap) {
                            return Step::Pending;
                        }
                    }
                },
                // After every retirement is committed, so a reader either read the manifest that
                // retired a file or pins it.
                Flushing::Reap => match self.reap.resume(cx, st, done.take()) {
                    Step::Pending => return Step::Pending,
                    Step::Ready(Ok(_)) => {
                        let out = std::mem::replace(&mut self.out, Flushed::empty());
                        return Step::Ready(Ok(Outcome::Flushed(out)));
                    }
                    // The runner lets go of what the reap held, then [`FlushJob::failed`]
                    // defers it: the commit is done.
                    Step::Ready(Err(error)) => return Step::Ready(Err(error)),
                },
                Flushing::Release => {
                    for it in round(done.take()) {
                        if let Err(error) = unlocked(it, cx.dir_path()) {
                            self.out.deferred.push(error);
                        }
                    }
                    self.stage = self.after;
                }
            }
        }
    }
}

// --- Recount ------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Recounting {
    Start,
    List,
    Trim,
    Reap,
}

/// The side files counted again (the recorder's mapping was just made), then the cap and the
/// reap.
struct Recount {
    stage: Recounting,
    trim: Trim,
    reap: Reap,
    deleted: u32,
}

impl Recount {
    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        st: &mut State,
        mut done: Option<Vec<Done>>,
    ) -> Step<Result<Outcome, HistoryError>> {
        loop {
            match self.stage {
                Recounting::Start => {
                    let dir = cx.dir.clone(); // clone: a refcount
                    cx.ask(Wait::Files, |tag| Effect::List { dir, tag });
                    self.stage = Recounting::List;
                    return Step::Pending;
                }
                Recounting::List => {
                    match listing(cx, only(done.take())) {
                        Ok(listing) => st.side = side_bytes(&listing),
                        Err(error) => return Step::Ready(Err(error)),
                    }
                    self.stage = Recounting::Trim;
                }
                Recounting::Trim => match self.trim.resume(cx, st, done.take()) {
                    Step::Pending => return Step::Pending,
                    Step::Ready(Err(error)) => return Step::Ready(Err(error)),
                    Step::Ready(Ok(deleted)) => {
                        self.deleted = deleted;
                        self.stage = Recounting::Reap;
                    }
                },
                Recounting::Reap => {
                    return match self.reap.resume(cx, st, done.take()) {
                        Step::Pending => Step::Pending,
                        Step::Ready(Err(error)) => Step::Ready(Err(error)),
                        Step::Ready(Ok(_)) => Step::Ready(Ok(Outcome::Recounted(Ok(self.deleted)))),
                    };
                }
            }
        }
    }
}

// --- Another writer's files ---------------------------------------------------------------

#[derive(Clone, Copy)]
enum Listing {
    Place,
    Hold,
    Rename,
    Commit,
    Unlock,
}

/// Files of a writer of the history other than the daemon's (`blaze debug time`'s derived
/// tables, `blaze perf record`'s runs): each placed at its hidden name, then, holding the
/// manifest's lock, renamed into place and listed in one commit. The open sweeps the directory
/// under that lock, so it never deletes one placed and not yet listed; a reader sees all of
/// them or none. Nothing is put over a retired file: a pinned reader may be reading it.
struct WriteListed {
    stage: Listing,
    entries: Vec<(String, Entry)>,
    place: Place,
    hold: Hold,
    held: Option<Held>,
    commit: Commit,
    bytes: u64,
}

impl WriteListed {
    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        st: &mut State,
        mut done: Option<Vec<Done>>,
    ) -> Step<Result<Outcome, HistoryError>> {
        loop {
            match self.stage {
                Listing::Place => match self.place.resume(cx, done.take()) {
                    Step::Pending => return Step::Pending,
                    Step::Ready(()) => {
                        for (_, len, failed) in self.place.take() {
                            if let Some(error) = failed {
                                return Step::Ready(Err(error));
                            }
                            self.bytes += len;
                        }
                        self.stage = Listing::Hold;
                    }
                },
                Listing::Hold => match self.hold.resume(cx, done.take()) {
                    Step::Pending => return Step::Pending,
                    Step::Ready(Err(error)) => return Step::Ready(Err(error)),
                    Step::Ready(Ok(held)) => {
                        if let Some((name, _)) = self
                            .entries
                            .iter()
                            .find(|(name, _)| held.manifest().retired().holds(name))
                        {
                            // clone: an error
                            let why = ManifestError::Relisted { file: name.clone() };
                            return Step::Ready(Err(manifest_error(cx, why)));
                        }
                        for at in 0..self.entries.len() {
                            let from = cx.hidden(&self.entries[at].0);
                            let to = cx.file(&self.entries[at].0);
                            // durable: recovered files take their names on disk before the manifest that lists them is committed
                            cx.ask(Wait::Sync, |tag| Effect::Rename { from, to, tag });
                        }
                        self.held = Some(held);
                        self.stage = Listing::Rename;
                        if self.entries.is_empty() {
                            done = nothing_asked();
                            continue;
                        }
                        return Step::Pending;
                    }
                },
                Listing::Rename => {
                    for ((name, _), it) in self.entries.iter().zip(round(done.take())) {
                        if let Done::Failed(error) = it {
                            return Step::Ready(Err(io_failed(
                                cx.path(name),
                                "renaming into place",
                                error,
                            )));
                        }
                    }
                    let Some(held) = self.held.as_mut() else {
                        unreachable!("held through a write")
                    };
                    for (name, entry) in std::mem::take(&mut self.entries) {
                        if let Err(why) = held.add(&name, entry) {
                            return Step::Ready(Err(manifest_error(cx, why)));
                        }
                    }
                    self.stage = Listing::Commit;
                }
                Listing::Commit => {
                    let Some(held) = self.held.as_mut() else {
                        unreachable!("held before its commit")
                    };
                    match self.commit.resume(cx, held, done.take()) {
                        Step::Pending => return Step::Pending,
                        Step::Ready(Err(error)) => return Step::Ready(Err(error)),
                        Step::Ready(Ok(())) => {
                            // clone: the manifest, once a commit
                            st.manifest = held.manifest().clone();
                            cx.unlock(self.hold.lock);
                            self.stage = Listing::Unlock;
                            return Step::Pending;
                        }
                    }
                }
                Listing::Unlock => {
                    if let Err(error) = unlocked(only(done.take()), cx.path(manifest::LOCK)) {
                        return Step::Ready(Err(error));
                    }
                    return Step::Ready(Ok(Outcome::WroteListed(Ok(self.bytes))));
                }
            }
        }
    }
}

// --- Schema files -------------------------------------------------------------------------

/// The schema file of every table there, as a reader needs before it opens views over the
/// history: one missing (a history last written before its table existed) is written, the rest
/// left as their writer wrote them. One that can't be written is
/// [`HistoryError::SchemasMissing`], naming every table that was missing and why the first
/// write failed.
struct Schemas {
    /// Each schema file's name, its table and its bytes.
    files: Vec<(String, &'static str, Vec<u8>)>,
    looked: bool,
    missing: Vec<usize>,
    place: Option<Place>,
}

impl Schemas {
    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        done: Option<Vec<Done>>,
    ) -> Step<Result<Outcome, HistoryError>> {
        if let Some(place) = self.place.as_mut() {
            return match place.resume(cx, done) {
                Step::Pending => Step::Pending,
                Step::Ready(()) => {
                    // Every table the history had no schema file of, as a reader needs all of
                    // them before any view opens: those written meanwhile too, so the error
                    // names the same tables whichever of the writes failed.
                    // alloc: an error's tables
                    let tables: Vec<&'static str> =
                        self.missing.iter().map(|&at| self.files[at].1).collect();
                    match place.take().into_iter().find_map(|it| it.2) {
                        None => Step::Ready(Ok(Outcome::Schemas(Ok(())))),
                        Some(error) => {
                            Step::Ready(Ok(Outcome::Schemas(Err(HistoryError::SchemasMissing {
                                dir: cx.dir_path(),
                                tables,
                                error: Box::new(error),
                            }))))
                        }
                    }
                }
            };
        }
        if !self.looked {
            self.looked = true;
            if self.files.is_empty() {
                return Step::Ready(Ok(Outcome::Schemas(Ok(()))));
            }
            for at in 0..self.files.len() {
                let file = cx.file(&self.files[at].0);
                cx.ask(Wait::Files, |tag| Effect::Stat { file, tag });
            }
            return Step::Pending;
        }
        for (at, it) in round(done).into_iter().enumerate() {
            match check(it, || cx.path(&self.files[at].0), "looking for") {
                Ok(Done::Stat(None)) => self.missing.push(at),
                Ok(Done::Stat(Some(_))) => {}
                Ok(other) => {
                    return Step::Ready(Err(unexpected(
                        cx.path(&self.files[at].0),
                        "looking for",
                        &other,
                    )));
                }
                Err(error) => return Step::Ready(Err(error)),
            }
        }
        if self.missing.is_empty() {
            return Step::Ready(Ok(Outcome::Schemas(Ok(()))));
        }
        let files: Vec<(String, Vec<u8>)> = self
            .missing
            .iter()
            // clone: a schema file's name; its bytes taken
            .map(|&at| (self.files[at].0.clone(), std::mem::take(&mut self.files[at].2)))
            // alloc: the files missing, once a reader opens views
            .collect();
        let mut place = Place::new(files, Then::Rename);
        let step = place.resume(cx, None);
        self.place = Some(place);
        match step {
            Step::Pending => Step::Pending,
            Step::Ready(()) => unreachable!("a placing of a file asks for its making"),
        }
    }
}

// --- Recovering for a reader --------------------------------------------------------------

#[derive(Clone, Copy)]
enum Dying {
    Start,
    Stat,
    Lock,
    Read,
    List,
    Recover,
    Unlock,
}

/// For a reader of the history with no daemon writing it (`blaze slow`): the last rows of
/// every daemon that died before it flushed them, as the open recovers them. Nothing while a
/// daemon holds the history ([`OWNER`]): it did so as it opened, and numbers its own files. The
/// owner's lock is held while the files are written, so no daemon starts numbering files
/// meanwhile: one that starts in these seconds sees the owner file held and its pid dead, and
/// waits for this to let go (`blaze serve`'s claim, at most [`super::RECOVERY_WAIT`]).
struct RecoverDead {
    stage: Dying,
    owner: Tag,
    read: Read,
    recover: Option<Recover>,
    result: Option<(usize, Option<HistoryError>)>,
}

impl RecoverDead {
    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        st: &mut State,
        mut done: Option<Vec<Done>>,
    ) -> Step<Result<Outcome, HistoryError>> {
        loop {
            match self.stage {
                Dying::Start => {
                    let file = cx.file(OWNER);
                    cx.ask(Wait::Files, |tag| Effect::Stat { file, tag });
                    self.stage = Dying::Stat;
                    return Step::Pending;
                }
                Dying::Stat => {
                    let doing = "looking for the owner file of";
                    match check(only(done.take()), || cx.path(OWNER), doing) {
                        // No daemon ever wrote here, so none left a mapping.
                        Ok(Done::Stat(None)) => {
                            return Step::Ready(Ok(Outcome::RecoveredDead(Ok(0))));
                        }
                        Ok(Done::Stat(Some(_))) => {}
                        Ok(other) => {
                            return Step::Ready(Err(unexpected(cx.path(OWNER), doing, &other)));
                        }
                        Err(error) => return Step::Ready(Err(error)),
                    }
                    let file = cx.file(OWNER);
                    self.owner = cx.ask(Wait::Files, |tag| Effect::Lock {
                        file,
                        mode: LockMode::Exclusive,
                        tag,
                    });
                    self.stage = Dying::Lock;
                    return Step::Pending;
                }
                Dying::Lock => {
                    let doing = "locking the owner file of";
                    match check(only(done.take()), || cx.path(OWNER), doing) {
                        Ok(Done::Locked) => cx.locks.push(self.owner),
                        // A daemon writes the history: it recovered as it opened.
                        Ok(Done::Contended) => {
                            return Step::Ready(Ok(Outcome::RecoveredDead(Ok(0))));
                        }
                        Ok(other) => {
                            return Step::Ready(Err(unexpected(cx.path(OWNER), doing, &other)));
                        }
                        Err(error) => return Step::Ready(Err(error)),
                    }
                    self.stage = Dying::Read;
                }
                Dying::Read => match self.read.resume(cx, done.take()) {
                    Step::Pending => return Step::Pending,
                    Step::Ready(Err(error)) => return Step::Ready(Err(error)),
                    Step::Ready(Ok((manifest, _))) => {
                        // The next flush number is past every file the manifest lists and its
                        // last flush, as the open numbers it: an unlisted file is a crash's
                        // garbage the next open deletes.
                        let last = manifest
                            .all()
                            .filter_map(Name::parse)
                            .map(|it| it.last)
                            .max()
                            .unwrap_or(0);
                        st.seq = last.max(manifest.flushed()) + 1;
                        st.manifest = manifest;
                        let dir = cx.dir.clone(); // clone: a refcount
                        cx.ask(Wait::Files, |tag| Effect::List { dir, tag });
                        self.stage = Dying::List;
                        return Step::Pending;
                    }
                },
                Dying::List => {
                    match listing(cx, only(done.take())) {
                        Ok(listing) => self.recover = Some(Recover::new(&listing)),
                        Err(error) => return Step::Ready(Err(error)),
                    }
                    self.stage = Dying::Recover;
                }
                Dying::Recover => {
                    let Some(recover) = self.recover.as_mut() else { unreachable!("recovering") };
                    match recover.resume(cx, st, done.take()) {
                        Step::Pending => return Step::Pending,
                        Step::Ready(result) => {
                            self.result = Some(result);
                            cx.unlock(self.owner);
                            self.stage = Dying::Unlock;
                            return Step::Pending;
                        }
                    }
                }
                Dying::Unlock => {
                    if let Err(error) = unlocked(only(done.take()), cx.path(OWNER)) {
                        return Step::Ready(Err(error));
                    }
                    let Some((written, failed)) = self.result.take() else {
                        unreachable!("recovered")
                    };
                    return Step::Ready(Ok(Outcome::RecoveredDead(match failed {
                        Some(error) => Err(error),
                        None => Ok(written),
                    })));
                }
            }
        }
    }
}

// --- Pins ---------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Pinning {
    Start,
    Stat,
    List,
    Clear,
    Lock,
    Rename,
    Read,
    Say,
}

/// A reader's pin taken ([`super::Pin`]): its lock file made and locked under a hidden name and
/// named into [`PINS`], then the manifest read, then the generation it is of said (the order
/// [`super::retire`] proves safe). The files of a dead process that had this pid and number are
/// deleted first: they say nothing of this pin. The pin's lock is held past the job, until
/// [`Files::unpin`].
struct PinJob {
    stage: Pinning,
    id: String,
    lock: Tag,
    read: Read,
    manifest: Option<Manifest>,
    said: Option<FileName>,
}

impl PinJob {
    fn making(&self) -> String {
        PinFile::Making { id: &self.id }.file_name()
    }

    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        mut done: Option<Vec<Done>>,
    ) -> Step<Result<Outcome, HistoryError>> {
        loop {
            match self.stage {
                Pinning::Start => {
                    let file = cx.file(PINS);
                    cx.ask(Wait::Files, |tag| Effect::Stat { file, tag });
                    self.stage = Pinning::Stat;
                    return Step::Pending;
                }
                Pinning::Stat => {
                    let doing = "looking for the directory of readers' pins";
                    match check(only(done.take()), || cx.path(PINS), doing) {
                        Ok(Done::Stat(Some(_))) => {
                            let dir = cx.file(PINS);
                            cx.ask(Wait::Files, |tag| Effect::List { dir, tag });
                            self.stage = Pinning::List;
                            return Step::Pending;
                        }
                        Ok(Done::Stat(None)) => {
                            self.stage = Pinning::Clear;
                            done = nothing_asked();
                        }
                        Ok(other) => {
                            return Step::Ready(Err(unexpected(cx.path(PINS), doing, &other)));
                        }
                        Err(error) => return Step::Ready(Err(error)),
                    }
                }
                Pinning::List => {
                    let entries = match check(only(done.take()), || cx.path(PINS), "listing") {
                        Ok(Done::Listed(entries)) => entries,
                        Ok(other) => {
                            return Step::Ready(Err(unexpected(cx.path(PINS), "listing", &other)));
                        }
                        Err(error) => return Step::Ready(Err(error)),
                    };
                    let mut clears = false;
                    for (file, _) in &entries {
                        let Some(name) = cx.name_in_pins(file) else { continue };
                        if PinFile::parse(name).is_some_and(|it| it.id() == self.id) {
                            let file = cx.pin_file(name);
                            cx.ask(Wait::Files, |tag| Effect::Remove { file, tag });
                            clears = true;
                        }
                    }
                    self.stage = Pinning::Clear;
                    if !clears {
                        done = nothing_asked();
                        continue;
                    }
                    return Step::Pending;
                }
                Pinning::Clear => {
                    for it in round(done.take()) {
                        if let Err(error) =
                            check(it, || cx.path(PINS), "deleting a dead reader's pin file")
                        {
                            return Step::Ready(Err(error));
                        }
                    }
                    let making = cx.pin_file(&self.making());
                    self.lock = cx.ask(Wait::Files, |tag| Effect::Lock {
                        file: making,
                        mode: LockMode::Exclusive,
                        tag,
                    });
                    self.stage = Pinning::Lock;
                    return Step::Pending;
                }
                Pinning::Lock => {
                    let path = cx.path(PINS).join(self.making());
                    match check(only(done.take()), || path.clone(), "making a reader's pin") {
                        Ok(Done::Locked) => cx.locks.push(self.lock),
                        Ok(Done::Contended) => {
                            return Step::Ready(Err(HistoryError::Io {
                                path,
                                doing: "locking a reader's pin, which another holds",
                                error: io::ErrorKind::WouldBlock.into(),
                            }));
                        }
                        Ok(other) => {
                            return Step::Ready(Err(unexpected(
                                path,
                                "locking a reader's pin",
                                &other,
                            )));
                        }
                        Err(error) => return Step::Ready(Err(error)),
                    }
                    let from = cx.pin_file(&self.making());
                    let to = cx.pin_file(&PinFile::Lock { id: &self.id }.file_name());
                    // durable: review: a reader's pin is a lock file of a live process; it waits for the directory's sync and may not need to survive a crash
                    cx.ask(Wait::Files, |tag| Effect::Rename { from, to, tag });
                    self.stage = Pinning::Rename;
                    return Step::Pending;
                }
                Pinning::Rename => {
                    let path = cx.path(PINS).join(self.making());
                    if let Err(error) =
                        check(only(done.take()), || path, "naming a reader's pin into place")
                    {
                        return Step::Ready(Err(error));
                    }
                    self.stage = Pinning::Read;
                }
                Pinning::Read => match self.read.resume(cx, done.take()) {
                    Step::Pending => return Step::Pending,
                    Step::Ready(Err(error)) => return Step::Ready(Err(error)),
                    Step::Ready(Ok((manifest, _))) => {
                        let generation = manifest.retired().generation;
                        let said =
                            cx.pin_file(&PinFile::Said { id: &self.id, generation }.file_name());
                        self.said = Some(said.clone()); // clone: a refcount
                        cx.ask(Wait::Files, |tag| Effect::Truncate { file: said, len: 0, tag });
                        self.manifest = Some(manifest);
                        self.stage = Pinning::Say;
                        return Step::Pending;
                    }
                },
                Pinning::Say => {
                    let Some(said) = self.said.take() else { unreachable!("said") };
                    let doing = "saying the generation a reader read";
                    if let Err(error) = check(only(done.take()), || cx.path_of(&said), doing) {
                        return Step::Ready(Err(error));
                    }
                    let Some(manifest) = self.manifest.take() else { unreachable!("read") };
                    // The pin's lock stays held past the job, until the reader lets go of it.
                    cx.locks.retain(|it| *it != self.lock);
                    return Step::Ready(Ok(Outcome::Pinned(Ok(Pinned {
                        lock: self.lock,
                        lock_file: cx.pin_file(&PinFile::Lock { id: &self.id }.file_name()),
                        said,
                        manifest,
                    }))));
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Unpinning {
    Said,
    LockFile,
    Unlock,
    Done,
}

/// A pin let go of: what it said deleted, then its lock file while still locked, then the lock,
/// so the writer never finds a live reader's lock file unlocked. A file left behind is a stale
/// pin the writer deletes once it finds it unlocked; so each step goes on past a failure.
struct Unpin {
    stage: Unpinning,
    pin: Pinned,
}

impl Unpin {
    fn resume<H: Host>(
        &mut self,
        cx: &mut Cx<'_, H>,
        _done: Option<Vec<Done>>,
    ) -> Step<Result<Outcome, HistoryError>> {
        match self.stage {
            Unpinning::Said => {
                let file = self.pin.said.clone(); // clone: a refcount
                cx.ask(Wait::Files, |tag| Effect::Remove { file, tag });
                self.stage = Unpinning::LockFile;
            }
            Unpinning::LockFile => {
                let file = self.pin.lock_file.clone(); // clone: a refcount
                cx.ask(Wait::Files, |tag| Effect::Remove { file, tag });
                self.stage = Unpinning::Unlock;
            }
            Unpinning::Unlock => {
                let lock = self.pin.lock;
                cx.ask(Wait::Files, |tag| Effect::Unlock { lock, tag });
                self.stage = Unpinning::Done;
            }
            Unpinning::Done => return Step::Ready(Ok(Outcome::Unpinned)),
        }
        Step::Pending
    }
}

// --- The core -----------------------------------------------------------------------------

impl<H: Host> Files<H> {
    /// The core of the history in `dir` under the driver's root `root`, with each table's
    /// writer properties (what its merges write with), by its place in
    /// [`crate::schema::TABLES`].
    pub(crate) fn new(
        host: H,
        root: PathBuf,
        dir: FileName,
        properties: Vec<WriterProperties>,
    ) -> Files<H> {
        Files {
            host,
            root,
            dir,
            state: State {
                cap: u64::MAX,
                kept: VecDeque::new(),
                total: 0,
                side: 0,
                last_build: 0,
                seq: 1,
                reaping: false,
                manifest: Manifest::new(),
                listing: crash::Written::default(),
                properties,
            },
            // alloc: once a core
            io: Io { next: 0, asked: Vec::new(), now: Duration::ZERO, wait: Wait::Nothing },
            running: None,
            queue: VecDeque::new(),
            // alloc: once a core
            round: Vec::new(),
            wait: Wait::Nothing,
            outcomes: VecDeque::new(),
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    #[cfg(test)]
    pub(crate) fn plant(&mut self, plant: Plant) {
        self.plant = plant;
    }

    pub(crate) fn host(&self) -> &H {
        &self.host
    }

    pub(crate) fn host_mut(&mut self) -> &mut H {
        &mut self.host
    }

    /// The history's directory under the driver's root.
    pub(crate) fn dir(&self) -> &FileName {
        &self.dir
    }

    /// The files the cap counts, oldest first.
    pub(crate) fn kept(&self) -> impl Iterator<Item = &Kept> {
        self.state.kept.iter()
    }

    /// The bytes those files take.
    pub(crate) fn total(&self) -> u64 {
        self.state.total
    }

    /// The highest build any file is named for, or a flush was for.
    pub(crate) fn last_build(&self) -> u64 {
        self.state.last_build
    }

    /// Notes that a flush named for `build` ran.
    pub(crate) fn named(&mut self, build: u64) {
        self.state.last_build = self.state.last_build.max(build);
    }

    fn push(&mut self, job: Job) {
        self.queue.push_back(job);
        if self.running.is_none() {
            self.resume(None);
        }
    }

    /// Opens the history, holding at most `cap` bytes, with `schemas` (each table's schema
    /// file's name and its bytes) written over: [`Outcome::Opened`].
    pub(crate) fn open(&mut self, cap: u64, schemas: Vec<(String, Vec<u8>)>) {
        self.state.cap = cap;
        self.push(Job::Open(Box::new(Open {
            stage: Opening::Hold,
            hold: Hold::new(),
            held: None,
            commit: Commit::new(),
            // alloc: empty until the listing
            listing: Vec::new(),
            // alloc: as above
            removes: Vec::new(),
            // alloc: as above
            crash_reads: Vec::new(),
            // alloc: as above
            crash_files: Vec::new(),
            crashes: crash::Recovered::default(),
            schemas: None,
            schema_files: schemas,
            recover: None,
            postmortem: None,
            trim: Trim::new(),
            reap: Reap::new(),
        })));
    }

    /// Commits `flush`: [`Outcome::Flushed`].
    pub(crate) fn flush(&mut self, flush: Flush) {
        let (files, entries): (Vec<(String, Vec<u8>)>, Vec<Entry>) =
            flush.files.into_iter().map(|it| ((it.file, it.bytes), it.entry)).unzip();
        self.push(Job::Flush(Box::new(FlushJob {
            stage: Flushing::Place,
            after: Flushing::Reap,
            seq: flush.seq,
            recorder: flush.recorder,
            crashes: flush.crashes,
            ended: flush.ended,
            entries,
            place: Place::new(files, Then::Rename),
            change: Change::new(),
            committed: false,
            out: Flushed::empty(),
            // alloc: empty until crash records are deleted
            removing: Vec::new(),
            compact: None,
            trim: Trim::new(),
            reap: Reap::new(),
        })));
    }

    /// Counts the side files again, then the cap and the reap: [`Outcome::Recounted`].
    pub(crate) fn recount(&mut self) {
        self.push(Job::Recount(Box::new(Recount {
            stage: Recounting::Start,
            trim: Trim::new(),
            reap: Reap::new(),
            deleted: 0,
        })));
    }

    /// Writes `files` and lists them in one commit: [`Outcome::WroteListed`].
    pub(crate) fn write_listed(&mut self, files: Vec<Placed>) {
        // alloc: once a write
        let mut entries = Vec::with_capacity(files.len());
        // alloc: once a write
        let mut placing = Vec::with_capacity(files.len());
        for file in files {
            // clone: a file's name, listed after it is placed
            entries.push((file.file.clone(), file.entry));
            placing.push((file.file, file.bytes));
        }
        self.push(Job::WriteListed(Box::new(WriteListed {
            stage: Listing::Place,
            entries,
            place: Place::new(placing, Then::Stay),
            hold: Hold::new(),
            held: None,
            commit: Commit::new(),
            bytes: 0,
        })));
    }

    /// Reads the manifest as a reader does, with no lock: [`Outcome::Read`].
    pub(crate) fn read(&mut self) {
        self.push(Job::Read(Read::new()));
    }

    /// Writes the schema files missing of `files` (each its name, its table, its bytes):
    /// [`Outcome::Schemas`].
    pub(crate) fn ensure_schemas(&mut self, files: Vec<(String, &'static str, Vec<u8>)>) {
        // alloc: empty until one is missing
        self.push(Job::Schemas(Box::new(Schemas {
            files,
            looked: false,
            missing: Vec::new(),
            place: None,
        })));
    }

    /// Recovers dead daemons' rows for a reader: [`Outcome::RecoveredDead`].
    pub(crate) fn recover_dead(&mut self) {
        self.push(Job::RecoverDead(Box::new(RecoverDead {
            stage: Dying::Start,
            owner: Tag(0),
            read: Read::new(),
            recover: None,
            result: None,
        })));
    }

    /// Pins the history for a reader, `id` the pin's (`<pid>-<n>`): [`Outcome::Pinned`].
    pub(crate) fn pin(&mut self, id: String) {
        self.push(Job::Pin(Box::new(PinJob {
            stage: Pinning::Start,
            id,
            lock: Tag(0),
            read: Read::new(),
            manifest: None,
            said: None,
        })));
    }

    /// Lets go of `pin`: [`Outcome::Unpinned`].
    pub(crate) fn unpin(&mut self, pin: Pinned) {
        self.push(Job::Unpin(Box::new(Unpin { stage: Unpinning::Said, pin })));
    }

    /// Moves the effects asked for since the last call into `out`, for the driver to run.
    pub(crate) fn effects(&mut self, out: &mut Vec<Effect>) {
        out.append(&mut self.io.asked);
    }

    /// The next outcome, in the order asked.
    pub(crate) fn outcome(&mut self) -> Option<Outcome> {
        self.outcomes.pop_front()
    }

    /// Whether an operation is running or waiting to.
    pub(crate) fn busy(&self) -> bool {
        self.running.is_some() || !self.queue.is_empty()
    }

    /// What the effects in flight wait for.
    pub(crate) fn waits_on(&self) -> Wait {
        self.wait
    }

    /// One effect's completion, at `now` on the driver's clock. A completion of no effect of
    /// the round is not this core's, and is left.
    pub(crate) fn complete(&mut self, completion: Completion, now: Duration) {
        self.io.now = self.io.now.max(now);
        let Some(slot) = self.round.iter_mut().find(|it| it.0 == completion.tag) else { return };
        slot.1 = Some(completion.done);
        if self.round.iter().all(|it| it.1.is_some()) {
            // alloc: a round's completions, once a round
            let done = std::mem::take(&mut self.round).into_iter().filter_map(|it| it.1).collect();
            self.resume(Some(done));
        }
    }

    /// Steps the running job with `done`, its round's completions, starting the next job as
    /// each ends, until one waits on a round or none is left.
    fn resume(&mut self, mut done: Option<Vec<Done>>) {
        loop {
            if self.running.is_none() {
                let Some(job) = self.queue.pop_front() else {
                    self.wait = Wait::Nothing;
                    return;
                };
                // alloc: a job's locks, a few
                self.running = Some(Running { job, locks: Vec::new(), failing: None });
                done = None;
            }
            let Some(running) = self.running.as_mut() else { unreachable!("a job runs") };
            self.io.wait = Wait::Nothing;
            let before = self.io.asked.len();
            let ended = match running.failing.take() {
                // The round that let go of a failed job's locks completed.
                Some(error) => Some(running.job.failed(error)),
                None => {
                    let mut cx = Cx {
                        host: &mut self.host,
                        root: &self.root,
                        dir: &self.dir,
                        io: &mut self.io,
                        locks: &mut running.locks,
                        #[cfg(test)]
                        plant: self.plant,
                    };
                    match running.job.resume(&mut cx, &mut self.state, done.take()) {
                        Step::Pending if self.io.asked.len() > before => None,
                        Step::Pending => {
                            // A step that waits on nothing would wait forever (bounded-progress):
                            // the core's bug, failed by name.
                            let error = HistoryError::Io {
                                path: self.root.join(self.dir.as_str()),
                                doing: "stepping the history's files",
                                error: io::Error::other(
                                    "a step of the history's core waited on no effect",
                                ),
                            };
                            Some(running.job.failed(error))
                        }
                        Step::Ready(Ok(outcome)) => Some(outcome),
                        Step::Ready(Err(error)) => {
                            // A job that fails lets go of every lock it holds before it ends.
                            for lock in std::mem::take(&mut running.locks) {
                                self.io.next += 1;
                                let tag = Tag(self.io.next);
                                self.io.asked.push(Effect::Unlock { lock, tag });
                            }
                            match self.io.asked.len() > before {
                                true => {
                                    running.failing = Some(error);
                                    None
                                }
                                false => Some(running.job.failed(error)),
                            }
                        }
                    }
                }
            };
            match ended {
                Some(outcome) => {
                    self.outcomes.push_back(outcome);
                    self.running = None;
                    done = None;
                }
                None => {
                    // alloc: a round's tags, once a round
                    self.round =
                        self.io.asked[before..].iter().map(|it| (it.tag(), None)).collect();
                    self.wait = match self.io.wait {
                        Wait::Nothing => Wait::Files,
                        wait => wait,
                    };
                    return;
                }
            }
        }
    }
}
