//! The history: what the flight recorder holds, flushed to Parquet files that the pinned DuckDB
//! (`blaze debug sql`, never linked into blaze) reads through the views of `sql/views.sql`.
//!
//! A thread of the history's own, `blaze-drain`, takes what is new in the rings every [`DRAIN`]
//! ([`flight::read`] from the history's [`Cursor`]), whatever the writer is doing; a flush
//! takes what it drained and writes one file a table it has rows of, `<dir>/<build>-<seq>.<table>.parquet`:
//! `build` the highest build any of its rows is of, never below the build the flush is for (0
//! for a timer's flush of rows of no build), so a daemon numbers its builds past every build the
//! directory holds rows of ([`History::next_build`]), and
//! `seq` the flush's number, which only grows in a directory. A Parquet file holds one schema,
//! so a table a file keeps each file self-describing.
//!
//! Which files are the history is said by one file, the manifest (`manifest.json`,
//! [`manifest`]): each table's files, by name, each with an entry saying its schema version,
//! rows, and the builds, instants and boot its rows span ([`manifest::Entry`]), so a reader of
//! one build reads only the files that can hold its rows ([`manifest::Scope::Build`]) and a
//! file of a version this blaze can't read is refused, naming it ([`schema::readable`]).
//! Every reader reads exactly the files it lists
//! (the views are given them as an explicit list, [`crate::views::prelude`]); a file in the
//! directory it does not list is not part of the history. Each change is one rewrite of the
//! manifest, to a hidden name and then renamed over it, under its lock: a flush writes its
//! files, then lists them; a merge writes its output, then lists it and retires its inputs in
//! one commit; the cap retires the oldest files. So a reader never reads a merge's rows twice
//! or half of them, and a crash leaves at worst files the manifest neither lists nor retires,
//! which the next [`History::open`] deletes. A retired file is unlisted at once and deleted
//! only once no reader's [`Pin`] can still read it ([`retire`], [`History::reap`]): a reader
//! pins the history before it reads the manifest and holds the pin until its last read of the
//! files it lists has ended, so none is deleted under it. Beside them,
//! `schema.<table>.parquet` holds no rows and every column of each table, and heads each
//! view's list, so each view resolves even before a table has rows, with today's columns.
//!
//! Each file is made in memory, in a buffer kept across flushes, and written with one `write`
//! to a hidden name, synced to stable storage, then renamed, so DuckDB never reads half a file
//! and a power loss never leaves the manifest listing bytes the disk lost ([`durable`]: every
//! file and the manifest, with the directory synced around the manifest's rename). Past [`CAP_BYTES`] (or the
//! cap the history is opened with), the oldest builds are retired first, each whole: every
//! table and flush of a build at once, so a reader of a build reads all of it or none
//! ([`History::trim`]).
//!
//! A flush every [`EVERY`] would leave thousands of files a long build (a 52-minute self-build
//! left 3,250), and every reader reads each one's footer. So after each flush the
//! history merges files of one build name and one table into one,
//! `<build>-<first:08>-<last:08>-<level>.<table>.parquet`: the rows of that build's files of the
//! table whose seqs lie in `first..=last`, at `level` 1 or more (a flush's file is level 0).
//! Rows are copied as their column chunks, with no decode and no re-encode, so row groups stay
//! the size their flush wrote. While a build runs, each `FAN_IN` (16) files of a level
//! become one a level up, so a table holds at most `FAN_IN - 1` files a level and its levels
//! grow as log16 of its flushes; as the build ends (the flush of `Some(build)`), each of its
//! tables becomes one file. Only files this history wrote since it opened are merged: those
//! there before may be of another blaze's schema.
//!
//! Until 2026-11-04 readers globbed `*.<table>.parquet`, and a glob between a merge's rename
//! and its deletes counted the merged rows twice; a directory of that time, with no manifest,
//! has one made of its files on first open ([`manifest`]).
//! Until 2026-11-04 a merge also deleted its inputs at once, and a `blaze slow` racing it
//! failed on DuckDB's "Cannot open file"; its inputs are retired now, deleted once no pin
//! holds them.
//!
//! What a reader sees is what the history committed: rows the drain took and no flush has
//! written yet are in no file. A reader that asks for everything recorded so far asks the
//! daemon that writes the history to flush first (`blaze_api::Request::Flush`, which every
//! CLI reader of the history sends before it reads: blaze-cargo's `cli::observe`; the explorer
//! has the daemon flush before each query the same way), and the
//! flush's number it answers is in the manifest it then reads ([`Manifest::flushed`]): every
//! row recorded before the ask is in exactly one listed file, with no window of the rings
//! beside them to overlap the files.
//!
//! What a daemon that dies leaves readable: a build's `builds` row as it starts (outcome
//! `running`, [`History::build_started`]), and with each flush that wrote rows the spans every
//! thread was inside then (the `open` table). A build whose last row is its start, of a process
//! that no longer writes the history ([`writes`]), ended by its daemon's death; `blaze slow`
//! reports it so, with what was open at the last flush.
//!
//! And its last seconds: the daemon's history persists the flight recorder into
//! `flight-<pid>.map` beside the files ([`History::persist_recorder`], [`flight::postmortem`]),
//! and each flush marks there how far it wrote. A history that opens (and `blaze slow`, when no
//! daemon writes the history, [`recover_dead`]) reads every such file no live process holds:
//! the rows past the dead daemon's last flush become files of their own, of the dead process,
//! and each build it was running gets a `builds` row `crashed`, running to the last row read
//! back; then the file is deleted. A file of another layout is refused with an error naming
//! it, answered by the next flush, and stays.
//!
//! What a commit consumed is part of that commit ([`manifest::Consumed`]), so a crash between
//! a commit and what follows it never has a row written twice or lost: a flush marks in the
//! mapping how far it will have written before its commit, and the commit names the mapping's
//! pid and nonce with its seq, so a postmortem trusts the mark only if the manifest says that
//! flush committed; the commit that lists a dead mapping's rows names it recovered, so it is
//! deleted unread if a crash came before its delete; the commit that lists a crash record's
//! `panics` row names it, so it is deleted unread the same way. A flush whose tables' files
//! did not all write clears only the rows of those that did, and keeps the others for the
//! next flush; a file written and not yet listed is listed by the next commit.

use std::{
    collections::{BTreeSet, VecDeque},
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use parking_lot::Mutex;

use arrow_array::{
    Array, ArrayRef, DictionaryArray, Int64Array, RecordBatch, StringArray, UInt32Array,
    UInt64Array, types::UInt32Type,
};
use arrow_schema::ArrowError;
use parquet::{
    arrow::ArrowWriter,
    basic::{Compression, ZstdLevel},
    column::writer::ColumnCloseResult,
    errors::ParquetError,
    file::{
        metadata::{KeyValue, PageIndexPolicy, ParquetMetaData, ParquetMetaDataReader},
        properties::{WriterProperties, WriterVersion},
        writer::SerializedFileWriter,
    },
    schema::types::ColumnPath,
};

use crate::{
    batches::{self, Process},
    clock, crash,
    errors::{self, ErrorRow},
    facts::{
        self, CrateDepRow, KeyPartRow, LinkRow, MissRow, ProcessRow, ProvenanceRow, QueryCountRow,
        TestRunRow,
    },
    flight::{self, CounterRow, Cursor, Plot, Read, SpanRow},
    identity, logs, panics,
    sampler::{self, Sample},
    schema::{self, Column, Table, Type},
    symbols,
};

pub(crate) mod compact;
pub(crate) mod durable;
pub mod manifest;
mod pin;
pub(crate) mod retire;

use compact::Name;
pub use manifest::Manifest;
pub use pin::Pin;
pub use retire::PINS;

/// What the history keeps by default: 4 GiB, about 400 million rows at the ~10 bytes a row
/// takes compressed. A self-build's `check` and `build` wrote about 1 GiB together with every
/// span a row, which a 1 GiB cap trimmed from under the build it was asked about (profrows).
pub const CAP_BYTES: u64 = 4 << 30;

/// How often the daemon flushes while a session runs.
pub const EVERY: Duration = Duration::from_secs(5);

/// How often the history's drain thread reads the rings into the next flush, on a thread of its
/// own so a flush that takes seconds stops no drain. A thread's ring holds [`flight::CAP`]
/// rows, so only a thread writing more than `CAP * 20` rows a second (655 k) loses the oldest,
/// counted in [`Flushed::skipped`] and the `profile rows skipped` counter. With the drain on
/// the writer's thread every 250 ms, a 102 s self-build step lost 42 M rows while flushes of
/// seconds stopped it (profrows).
pub const DRAIN: Duration = Duration::from_millis(50);

/// Rows drained that make the history flush before [`EVERY`] is up ([`History::full`]): about
/// four row groups, so a busy build's rows are written as they come, not held for seconds.
pub const FLUSH_ROWS: usize = 1 << 19;

/// Rows drained and not yet written past which the drain thread stops reading until the writer
/// takes them: the memory a writer that falls behind may hold (about 200 bytes a row). What
/// the rings write over meanwhile is counted apart, as `profile rows skipped behind`.
pub const BACKLOG_ROWS: usize = 4 * FLUSH_ROWS;

/// Rows a row group holds at most: DuckDB's own row group size, the unit its scans split
/// across threads.
pub const ROW_GROUP_ROWS: usize = 122_880;

/// Where the daemon's history goes: `profile/` in its state directory.
pub fn dir_in(state: &Path) -> PathBuf {
    state.join("profile")
}

/// The file in a history's directory whose lock says which daemon writes it, and whose text
/// is that daemon's pid (blaze-cargo's `serve::observe` claims it).
pub const OWNER: &str = "owner.lock";

/// Whether the process `pid` still writes the history `dir`: its [`OWNER`] file names it, the
/// process is alive, and the file's lock is held (a pid the system gave again to another process
/// holds no lock). A build of a process that does not, whose last row is its start, ended by
/// that process's death.
pub fn writes(dir: &Path, pid: u32) -> bool {
    let path = dir.join(OWNER);
    let named = fs::read_to_string(&path).ok().and_then(|it| it.trim().parse::<u32>().ok());
    if named != Some(pid) || !alive(pid) {
        return false;
    }
    // A shared lock taken is one no owner holds; let go of when the file drops. An error other
    // than "held" says nothing of the owner, and the live pid the file names is taken as it.
    match fs::File::open(&path).map(|file| file.try_lock_shared()) {
        Ok(Ok(())) => false,
        Ok(Err(fs::TryLockError::WouldBlock)) | Ok(Err(fs::TryLockError::Error(_))) | Err(_) => {
            true
        }
    }
}

/// The process that writes the history `dir` now, if one does: the pid its [`OWNER`] file names,
/// when that process [`writes`] it. A reader asks that daemon to flush before it reads.
pub fn writer(dir: &Path) -> Option<u32> {
    let pid = fs::read_to_string(dir.join(OWNER)).ok()?.trim().parse::<u32>().ok()?;
    writes(dir, pid).then_some(pid)
}

/// Whether a process `pid` is alive: one this user may not signal is, one that is gone is not.
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    // SAFETY: signal 0 sends nothing: the kernel only checks that `pid` names a process.
    let sent = unsafe { libc::kill(pid, 0) };
    sent == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub fn alive(_pid: u32) -> bool {
    true
}

/// How long a daemon starting over a history waits for a reader that holds its [`OWNER`]
/// file while it writes a dead daemon's last rows ([`recover_dead`]): a mapping's rows, a few
/// files and their syncs, far less than this.
pub const RECOVERY_WAIT: Duration = Duration::from_secs(30);

/// The exclusive lock of `file`, waiting at most `wait` for another process to let go of it:
/// the file back once `wait` passed with it still held. The one bounded wait on a lock of the
/// history (its manifest's, its owner file's); the caller opens the span naming the lock.
#[allow(
    clippy::disallowed_methods,
    reason = "a bounded wait on another process's lock, which has no wake to park on"
)]
pub fn lock_within(
    file: fs::File,
    wait: Duration,
) -> io::Result<Result<stdx::file_lock::Locked, fs::File>> {
    let until = Instant::now() + wait;
    let mut file = file;
    let mut spin = blaze_graph::spin::SpinWait::new();
    loop {
        match stdx::file_lock::Locked::try_lock(file)? {
            Ok(locked) => return Ok(Ok(locked)),
            Err(back) if Instant::now() >= until => return Ok(Err(back)),
            Err(back) => file = back,
        }
        if !spin.spin() {
            spin = blaze_graph::spin::SpinWait::new();
        }
    }
}

/// Whether the threads `now` are inside the spans `written` were, how long each has run aside.
fn same_open(now: &[flight::ThreadNow], written: &[flight::ThreadNow]) -> bool {
    now.len() == written.len()
        && now.iter().zip(written).all(|(a, b)| {
            a.tid == b.tid
                && a.deeper == b.deeper
                && a.open.len() == b.open.len()
                && a.open.iter().zip(&b.open).all(|(a, b)| {
                    (a.name, a.target, &a.krate, &a.function, &a.unit, &a.awaits, a.owner)
                        == (b.name, b.target, &b.krate, &b.function, &b.unit, &b.awaits, b.owner)
                })
        })
}

/// How a build request ended (`builds.outcome`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Failed,
    Cancelled,
    Stalled,
    Running,
    /// Its daemon died while it ran: a postmortem of the daemon's flight recorder wrote this
    /// row, running to the last row it read back ([`flight::postmortem`]).
    Crashed,
}

impl Outcome {
    /// In the order of their numbers, which are their keys in the column's dictionary.
    pub const ALL: [Outcome; 6] = [
        Outcome::Ok,
        Outcome::Failed,
        Outcome::Cancelled,
        Outcome::Stalled,
        Outcome::Running,
        Outcome::Crashed,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Failed => "failed",
            Outcome::Cancelled => "cancelled",
            Outcome::Stalled => "stalled",
            Outcome::Running => "running",
            Outcome::Crashed => "crashed",
        }
    }
}

/// A row of the schema's `builds` table: one build request the daemon ran.
#[derive(Clone, Debug, PartialEq, Eq)] // clone: an ended row kept for its client's times, once a build
pub struct BuildRow {
    pub build_id: u64,
    pub commit: Option<String>,
    pub workspace: String,
    /// What was asked: build, test, run, check, with the arguments.
    pub command: String,
    /// When the daemon took it, on the schema's clock ([`crate::clock::mono_ns`]).
    pub started_ns: i64,
    pub dur_ns: Option<i64>,
    pub started_unix_ns: Option<i64>,
    pub outcome: Outcome,
    pub hits: Option<u64>,
    pub misses: Option<u64>,
    pub joined: Option<u64>,
    /// [`identity::tree_digest`] of `workspace`: given by [`History::build_ended`] when `None`
    /// and `workspace` is a directory.
    pub tree: Option<String>,
    /// [`identity::binary`]: given by [`History::build_ended`] when `None`.
    pub blaze: Option<String>,
    /// When the client that asked for it started and when it had the answer, on the schema's
    /// clock (`builds.client_started_ns`, `client_ended_ns`): given by
    /// [`History::client_rows`], `None` until then.
    pub client_ns: Option<(i64, i64)>,
    /// The daemon process's user and system CPU over the build, from getrusage
    /// (`builds.daemon_user_ns`, `daemon_system_ns`); `None` while it runs.
    pub daemon_cpu_ns: Option<(i64, i64)>,
}

/// What a client sent of a build it asked for, as it ended (`blaze_api::Request::ClientRows`):
/// its own `spans`, `events` and `folded` rows, each an Arrow IPC stream of that table's
/// schema with every row's `build_id` the build's, and when it started and ended.
#[derive(Debug, Default)]
pub struct ClientRows {
    pub build: u64,
    pub started_ns: i64,
    pub ended_ns: i64,
    pub spans: Vec<u8>,
    pub events: Vec<u8>,
    pub folded: Vec<u8>,
}

/// What a client's rows name their process (`process_name`): told from its daemon's by it
/// where one build's rows must be one process's (`time::load`).
pub const CLIENT_PROCESS: &str = "blaze client";

/// The ended builds' rows kept for their clients' times: past this many, a client that sends
/// its rows later than this many builds ended after its own has its times dropped, named.
const RECENT_BUILDS: usize = 64;

/// What a flush wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flushed {
    /// The flush's number, in its files' names.
    pub seq: u64,
    /// The build its files are named for, `<build>-<seq>.<table>.parquet`: the highest build
    /// any of its rows is of, never below the build it was for ([`History::flush`]).
    pub build: u64,
    pub spans: usize,
    pub counters: usize,
    pub builds: usize,
    pub samples: usize,
    pub switches: usize,
    pub events: usize,
    /// Rows of `event_fields`: the fields of the events written.
    pub event_fields: usize,
    /// DEBUG rows of events held for builds not yet ended, after the flush ([`logs::Hold`]).
    pub events_held: usize,
    /// DEBUG rows of events dropped since the history opened: of builds that ended well, or
    /// past what is held ([`logs::Hold`]).
    pub events_dropped: u64,
    pub errors: usize,
    pub links: usize,
    pub crate_deps: usize,
    pub query_counts: usize,
    pub misses: usize,
    /// Panics and recovered fatal signals ([`crate::panics`], [`crate::crash`]).
    pub panics: usize,
    pub processes: usize,
    pub test_runs: usize,
    /// Rows clients sent ([`History::client_rows`]), of every table.
    pub client: usize,
    pub key_parts: usize,
    pub provenance: usize,
    /// Spans the threads were inside as the flush read them.
    pub open: usize,
    pub folded: usize,
    pub files: u32,
    pub bytes: u64,
    /// Rows the rings wrote over before this flush read them, CPU samples included.
    pub skipped: u64,
    /// Old files retired to stay under the cap (deleted once no reader pins them).
    pub deleted: u32,
    /// Files merged into others, and retired, after the flush (deleted once no reader pins
    /// them).
    pub merged: u32,
    /// Files those merges made.
    pub compacted: u32,
}

#[derive(Debug)]
pub enum HistoryError {
    Io {
        path: PathBuf,
        doing: &'static str,
        error: io::Error,
    },
    Parquet {
        path: PathBuf,
        table: &'static str,
        error: ParquetError,
    },
    Arrow {
        table: &'static str,
        error: ArrowError,
    },
    /// What a build was of could not be told: its tree's digest, or blaze's own hash.
    Identity {
        what: &'static str,
        path: PathBuf,
        error: io::Error,
    },
    /// The history `dir` has no schema file of `tables` (it was written before they existed),
    /// and writing them there failed: no view over it can be opened.
    SchemasMissing {
        dir: PathBuf,
        tables: Vec<&'static str>,
        error: Box<HistoryError>,
    },
    /// Merging files into `output` failed at the step `doing`, on `file`: an input, or the
    /// hidden file the output is written to. Every input stays in place.
    Compact {
        output: PathBuf,
        file: PathBuf,
        doing: &'static str,
        error: CompactError,
    },
    /// The manifest `path` can't be read, written or locked ([`manifest`]).
    Manifest {
        path: PathBuf,
        why: manifest::ManifestError,
    },
    /// A Parquet file of the history `dir` that is of no form blaze writes there, so whether
    /// it is part of the history can't be told: a directory with no manifest can't have one
    /// made of its files, and the open can't tell whether to keep or delete it.
    Unclassified {
        dir: PathBuf,
        file: String,
    },
    /// The flight recorder could not be kept past a crash, or a dead daemon's rows could not be
    /// read back ([`flight::postmortem`]).
    Postmortem(flight::postmortem::PostmortemError),
    /// What the client of build `build` sent of `table` can't be written: bytes that are not
    /// Arrow IPC, a batch not of the table's schema, or a row of another build.
    Client {
        build: u64,
        table: &'static str,
        why: String,
    },
}

/// Why a merge of history files failed.
#[derive(Debug)]
pub enum CompactError {
    Io(io::Error),
    Parquet(ParquetError),
    /// An input's Parquet schema is not the first input's. Every input is a file this process
    /// wrote, so this is a bug.
    SchemaDiffers,
}

impl fmt::Display for CompactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompactError::Io(error) => write!(f, "{error}"),
            CompactError::Parquet(error) => write!(f, "{error}"),
            CompactError::SchemaDiffers => write!(f, "its Parquet schema is not the first input's"),
        }
    }
}

impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HistoryError::Io { path, doing, error } => {
                write!(f, "the profile history: {doing} {}: {error}", path.display())
            }
            HistoryError::Parquet { path, table, error } => {
                write!(
                    f,
                    "the profile history: writing the {table} table to {}: {error}",
                    path.display()
                )
            }
            HistoryError::Arrow { table, error } => {
                write!(f, "the profile history: making the {table} table's rows: {error}")
            }
            HistoryError::Identity { what, path, error } => {
                write!(f, "the profile history: {what} {}: {error}", path.display())
            }
            HistoryError::SchemasMissing { dir, tables, error } => write!(
                f,
                "the profile history {} was written before the tables {} existed, and their schema files can't be \
                 written there, so no view over it opens (open it once with a blaze that can write it): {error}",
                dir.display(),
                tables.join(", ")
            ),
            HistoryError::Compact { output, file, doing, error } => write!(
                f,
                "the profile history: merging files into {}: {doing} {}: {error}",
                output.display(),
                file.display()
            ),
            HistoryError::Manifest { path, why } => {
                write!(f, "the profile history's manifest {}: {why}", path.display())
            }
            HistoryError::Unclassified { dir, file } => write!(
                f,
                "the profile history {} holds `{file}`, Parquet of no form blaze writes there, so whether it is part of \
                 the history can't be told: move it out of the directory",
                dir.display()
            ),
            HistoryError::Postmortem(error) => write!(f, "the profile history: {error}"),
            HistoryError::Client { build, table, why } => {
                write!(
                    f,
                    "the profile history: the {table} rows build {build}'s client sent: {why}"
                )
            }
        }
    }
}

impl std::error::Error for HistoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HistoryError::Io { error, .. } => Some(error),
            HistoryError::Parquet { error, .. } => Some(error),
            HistoryError::Arrow { error, .. } => Some(error),
            HistoryError::Identity { error, .. } => Some(error),
            HistoryError::SchemasMissing { error, .. } => Some(error.as_ref()),
            HistoryError::Compact { error: CompactError::Io(error), .. } => Some(error),
            HistoryError::Compact { error: CompactError::Parquet(error), .. } => Some(error),
            HistoryError::Compact { error: CompactError::SchemaDiffers, .. } => None,
            HistoryError::Manifest { .. } | HistoryError::Unclassified { .. } => None,
            HistoryError::Postmortem(error) => Some(error),
            HistoryError::Client { .. } => None,
        }
    }
}

fn io_error<'a>(
    path: &'a Path,
    doing: &'static str,
) -> impl FnOnce(io::Error) -> HistoryError + 'a {
    // alloc: an error
    move |error| HistoryError::Io { path: path.to_owned(), doing, error }
}

fn arrow_error(table: &'static Table) -> impl FnOnce(ArrowError) -> HistoryError {
    move |error| HistoryError::Arrow { table: table.name, error }
}

/// Whether a column's Parquet pages are dictionary-encoded: the strings, and the numbers that
/// repeat (processes, threads, depths, builds). Unique numbers (ids, instants, durations) are
/// not, since their dictionary would be as big as the column.
fn dictionary(column: &Column) -> bool {
    match column.ty {
        Type::Dict | Type::Stack | Type::Utf8 | Type::U16 | Type::U32 => true,
        Type::U64 => column.name == "build_id",
        Type::I64 | Type::F64 | Type::Pcs | Type::Bool => false,
    }
}

/// How each table's files are written: zstd at level 1 (fast enough to keep up with the
/// recorder: see docs/v2/plan/lanes/observe/history.md), Parquet 2 pages (delta-encoded
/// integers), DuckDB-sized row groups, page statistics for pruning by time and build.
fn properties(table: &Table) -> WriterProperties {
    let mut it = WriterProperties::builder()
        .set_writer_version(WriterVersion::PARQUET_2_0)
        .set_compression(Compression::ZSTD(ZstdLevel::default()))
        .set_max_row_group_row_count(Some(ROW_GROUP_ROWS))
        .set_dictionary_enabled(false);
    for column in table.columns.iter().filter(|it| dictionary(it)) {
        let path = match column.ty {
            // alloc: writer properties, made once a table when the history opens
            Type::Stack => ColumnPath::new(vec![column.name.into(), "list".into(), "frame".into()]),
            _ => ColumnPath::from(column.name),
        };
        it = it.set_column_dictionary_enabled(path, true);
    }
    it.build()
}

pub(crate) fn table_at(table: &Table) -> usize {
    match schema::TABLES.iter().position(|it| it.name == table.name) {
        Some(at) => at,
        None => unreachable!("{} is a table of the history", table.name),
    }
}

/// Writes record batches as Parquet files, reusing its buffer from file to file.
pub(crate) struct Writer {
    properties: Vec<WriterProperties>,
    buf: Vec<u8>,
}

impl Writer {
    /// The writer properties of the table at `table` in [`schema::TABLES`]: what a merge of its
    /// files writes with.
    pub(crate) fn properties(&self, table: usize) -> &WriterProperties {
        &self.properties[table]
    }

    pub(crate) fn new() -> Writer {
        // alloc: once a history
        Writer {
            properties: schema::TABLES.iter().map(|it| properties(it)).collect(),
            buf: Vec::with_capacity(1 << 20),
        }
    }

    /// Writes `parts`, rows of `table`, as the Parquet file `path`; its size.
    pub(crate) fn write(
        &mut self,
        path: &Path,
        table: &'static Table,
        parts: &[RecordBatch],
    ) -> Result<u64, HistoryError> {
        let (hidden, bytes) = self.write_hidden(path, table, parts)?;
        fs::rename(&hidden, path).map_err(io_error(path, "renaming into place"))?;
        Ok(bytes)
    }

    /// [`Writer::write`] to `path`'s hidden name only: the hidden file and its size, for
    /// [`write_listed`] to rename into place.
    fn write_hidden(
        &mut self,
        path: &Path,
        table: &'static Table,
        parts: &[RecordBatch],
    ) -> Result<(PathBuf, u64), HistoryError> {
        let parquet = |error| HistoryError::Parquet {
            // alloc: an error
            path: path.to_owned(),
            table: table.name,
            error,
        };
        let mut buf = std::mem::take(&mut self.buf);
        buf.clear();
        // clone: a table's writer properties, once a file
        let properties = self.properties[table_at(table)].clone();
        let mut writer = ArrowWriter::try_new(buf, batches::schema_of(table), Some(properties))
            .map_err(parquet)?;
        for part in parts {
            writer.write(part).map_err(parquet)?;
        }
        let buf = writer.into_inner().map_err(parquet)?;
        let hidden = hidden_of(path);
        let written = write_synced(&hidden, &buf);
        let bytes = buf.len() as u64;
        self.buf = buf;
        written.map(|()| (hidden, bytes))
    }

    /// An empty `schema.<table>.parquet` of each table in `dir`, each written over: a writer
    /// of the history makes them its own blaze's columns.
    pub(crate) fn schemas(&mut self, dir: &Path) -> Result<(), HistoryError> {
        for table in schema::TABLES {
            self.write(&schema_file(dir, table), table, &[])?;
        }
        Ok(())
    }
}

/// The empty file of `table` in the history `dir`, which every view reads, so a view
/// of a table no file has rows for reads as empty instead of failing.
fn schema_file(dir: &Path, table: &Table) -> PathBuf {
    // alloc: a file's name, once a table
    dir.join(format!("schema.{}.parquet", table.name))
}

/// Makes sure the history `dir` has the schema file of every table, as a reader needs before
/// it opens views over it: a history last written before a table existed has none of that
/// table's. Writes only those missing, leaving the rest as their writer wrote them. A
/// directory they can't be written to is [`HistoryError::SchemasMissing`], naming them.
pub fn ensure_schemas(dir: &Path) -> Result<(), HistoryError> {
    // alloc: the tables missing, once a reader opens views
    let mut missing: Vec<&'static Table> = Vec::new();
    for table in schema::TABLES {
        let path = schema_file(dir, table);
        if !path.try_exists().map_err(io_error(&path, "looking for"))? {
            missing.push(table);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let mut writer = Writer::new();
    for (i, table) in missing.iter().enumerate() {
        if let Err(error) = writer.write(&schema_file(dir, table), table, &[]) {
            return Err(HistoryError::SchemasMissing {
                // alloc: an error
                dir: dir.to_owned(),
                // alloc: an error
                tables: missing[i..].iter().map(|it| it.name).collect(),
                error: Box::new(error),
            });
        }
    }
    Ok(())
}

/// Writes `bytes` as the file `hidden` in one write and syncs it to stable storage
/// ([`durable::sync`]): what every file of the history is before it is renamed into place.
fn write_synced(hidden: &Path, bytes: &[u8]) -> Result<(), HistoryError> {
    use std::io::Write as _;
    let mut file = fs::File::create(hidden).map_err(io_error(hidden, "making"))?;
    file.write_all(bytes).map_err(io_error(hidden, "writing"))?;
    durable::sync(&file).map_err(io_error(hidden, "syncing to disk"))
}

/// Writes `bytes` as `path`, the manifest, in one commit: to a hidden name, synced; then the
/// directory synced, so every file it lists is on disk under its name before the manifest that
/// lists it; then renamed into place, and the directory synced again, so the commit is on disk
/// before anything that follows it (a reap's deletes, a mapping's or crash record's delete) can
/// be ([`durable`]). The hidden name is this process's: two processes committing in turn never
/// write into one file.
fn put(path: &Path, bytes: &[u8]) -> Result<(), HistoryError> {
    let hidden = hidden_of(path);
    let dir = path.parent().unwrap_or(Path::new("."));
    write_synced(&hidden, bytes)?;
    durable::sync_dir(dir).map_err(io_error(dir, "syncing to disk the directory of"))?;
    fs::rename(&hidden, path).map_err(io_error(path, "renaming into place"))?;
    durable::sync_dir(dir).map_err(io_error(dir, "syncing to disk the directory of"))
}

/// The hidden name `path` is written to before it is renamed into place:
/// `.<name>.<pid>.tmp`, this process's.
fn hidden_of(path: &Path) -> PathBuf {
    let name = path.file_name().and_then(|it| it.to_str()).unwrap_or("history");
    // alloc: a file's name, once a file
    path.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

/// Whether the hidden file `name` of a history's directory is one a write left half done: of a
/// process that is gone, or of a blaze before hidden names carried the writer's pid. One of
/// another live process (a `blaze perf record` writing beside the daemon) is its write under
/// way, and stays.
fn half_written(name: &str) -> bool {
    let Some(stem) = name.strip_prefix('.').and_then(|it| it.strip_suffix(".tmp")) else {
        return false;
    };
    if !(stem.contains(".parquet") || stem.starts_with(manifest::FILE)) {
        return false;
    }
    let pid = stem.rsplit_once('.').and_then(|(_, pid)| pid.parse::<u32>().ok());
    match pid {
        Some(pid) => pid == std::process::id() || !alive(pid),
        None => true,
    }
}

/// The name of the history file `path`, as the manifest lists it.
/// Bytes of the files of the history `dir` that are not Parquet and that the cap counts: each
/// flight recorder mapping (`flight-<pid>.map`: this process's, reserved whole as it is made,
/// and one a postmortem refused and left) and each crash record (`crash.<pid>`, one page, and
/// `crash.<pid>.refused`). A file that can't be read for its size counts as none: it is said
/// by the open or postmortem that refused it.
fn side_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.starts_with("crash.")
                    || (name.starts_with("flight-") && name.ends_with(".map"))
            })
        })
        .filter_map(|entry| entry.metadata().ok())
        .map(|it| it.len())
        .sum()
}

fn name_of(path: &Path) -> &str {
    path.file_name().and_then(|it| it.to_str()).unwrap_or_default()
}

/// Writes each of `files` (its path in the history `dir`, its table, its rows) and lists them
/// all in the manifest in one commit: for a writer of the history other than the daemon's
/// [`History`] (`blaze debug time`'s derived tables, `blaze perf record`'s runs). Each is
/// written to its hidden name first; then, holding the manifest's lock, each is renamed into
/// place and listed. The history's open sweeps the directory under that lock, so it never
/// deletes one placed and not yet listed; a reader sees all of them or none. The bytes written.
pub(crate) fn write_listed(
    dir: &Path,
    files: &[(&Path, &'static Table, &[RecordBatch])],
) -> Result<u64, HistoryError> {
    fs::create_dir_all(dir).map_err(io_error(dir, "making the directory"))?;
    let mut writer = Writer::new();
    // alloc: the hidden files, once a write
    let mut hidden = Vec::with_capacity(files.len());
    let mut bytes = 0;
    for &(path, table, parts) in files {
        let (at, size) = writer.write_hidden(path, table, parts)?;
        hidden.push(at);
        bytes += size;
    }
    let mut held = manifest::hold(dir)?;
    // Nothing is put over a retired file: a pinned reader may be reading it (a `blaze debug time`
    // racing the cap that retired its build).
    if let Some(&(path, ..)) =
        files.iter().find(|(path, ..)| held.manifest().retired().holds(name_of(path)))
    {
        // alloc: an error
        let file = name_of(path).to_owned();
        return Err(HistoryError::Manifest {
            path: dir.join(manifest::FILE),
            why: manifest::ManifestError::Relisted { file },
        });
    }
    for (&(path, table, parts), at) in files.iter().zip(&hidden) {
        fs::rename(at, path).map_err(io_error(path, "renaming into place"))?;
        held.add(name_of(path), manifest::Entry::of_batches(table, parts))?;
    }
    held.commit()?;
    Ok(bytes)
}

fn column(array: impl Array + 'static) -> ArrayRef {
    // alloc: a batch's column, once a flush
    Arc::new(array)
}

/// The `builds` rows of `rows`, all of the process `pid`.
pub fn builds(rows: &[BuildRow], pid: u32) -> Result<RecordBatch, ArrowError> {
    let outcomes = column(StringArray::from_iter_values(Outcome::ALL.iter().map(|it| it.name())));
    let outcome = UInt32Array::from_iter_values(rows.iter().map(|it| it.outcome as u32));
    // alloc: a batch's columns, once a flush
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter_values(rows.iter().map(|it| it.build_id))),
        column(StringArray::from_iter(rows.iter().map(|it| it.commit.as_deref()))),
        column(StringArray::from_iter_values(rows.iter().map(|it| it.workspace.as_str()))),
        column(StringArray::from_iter_values(rows.iter().map(|it| it.command.as_str()))),
        column(Int64Array::from_iter_values(rows.iter().map(|it| it.started_ns))),
        column(Int64Array::from_iter(rows.iter().map(|it| it.dur_ns))),
        column(Int64Array::from_iter(rows.iter().map(|it| it.started_unix_ns))),
        column(DictionaryArray::<UInt32Type>::try_new(outcome, outcomes)?),
        column(UInt64Array::from_iter(rows.iter().map(|it| it.hits))),
        column(UInt64Array::from_iter(rows.iter().map(|it| it.misses))),
        column(UInt64Array::from_iter(rows.iter().map(|it| it.joined))),
        column(StringArray::from_iter(rows.iter().map(|it| it.tree.as_deref()))),
        column(StringArray::from_iter(rows.iter().map(|it| it.blaze.as_deref()))),
        column(UInt32Array::from_value(pid, rows.len())),
        column(Int64Array::from_iter(rows.iter().map(|it| it.client_ns.map(|it| it.0)))),
        column(Int64Array::from_iter(rows.iter().map(|it| it.client_ns.map(|it| it.1)))),
        column(Int64Array::from_iter(rows.iter().map(|it| it.daemon_cpu_ns.map(|it| it.0)))),
        column(Int64Array::from_iter(rows.iter().map(|it| it.daemon_cpu_ns.map(|it| it.1)))),
    ];
    RecordBatch::try_new(batches::schema_of(&schema::BUILDS), columns)
}

/// A file of the history the cap counts: written by a flush or a merge, retired oldest first
/// (by the last flush it holds rows of, `name.last`).
struct Kept {
    name: Name,
    bytes: u64,
    path: PathBuf,
    /// Its entry in the manifest.
    entry: manifest::Entry,
    /// Written by this history since it opened, so it may be merged: a file there before may be
    /// of another blaze's schema.
    ours: bool,
    /// The manifest lists it. A flush's file whose commit failed is in place and not yet
    /// listed: the next flush lists it first, and no merge nor the cap takes it meanwhile.
    listed: bool,
}

/// A merge as its driver runs it: the paths of the files merged, in seq order, and the name of
/// the file they become.
struct Job {
    inputs: Vec<PathBuf>,
    output: Name,
}

/// The key of a merged file's metadata that says how many rows its merge read from its inputs'
/// footers, in decimal: what the gate (`gate::check`, `merges_exact`) holds the file's own row
/// count to, long after the inputs are deleted. A merge of merged files says its own.
pub(crate) const MERGED_ROWS: &str = "blaze.merged_rows";

/// Writes the rows of the Parquet files `inputs`, all of one schema, as the file `output`,
/// copying their column chunks as they are, with `properties` (the table's); its size. It is
/// streamed to a hidden name and renamed into place, so a reader never sees half of it; on a
/// failure the hidden file is deleted and `output` is not made. Its metadata says the rows the
/// inputs' footers held ([`MERGED_ROWS`]).
pub(crate) fn concat(
    output: &Path,
    inputs: &[PathBuf],
    properties: &WriterProperties,
) -> Result<u64, HistoryError> {
    let name = output.file_name().and_then(|it| it.to_str()).unwrap_or("history");
    // alloc: a file's name, once a merge
    let hidden = output.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    // Synced before it is renamed: the commit that lists it then retires its inputs, which a
    // reap deletes, so its bytes must be on disk first ([`durable`]).
    let written = concat_into(output, &hidden, inputs, properties).and_then(|()| {
        fs::File::open(&hidden).and_then(|file| durable::sync(&file)).map_err(|error| {
            HistoryError::Compact {
                // alloc: an error
                output: output.to_owned(),
                // alloc: an error
                file: hidden.clone(),
                doing: "syncing to disk",
                error: CompactError::Io(error),
            }
        })?;
        fs::rename(&hidden, output).map_err(|error| HistoryError::Compact {
            // alloc: an error
            output: output.to_owned(),
            // alloc: an error
            file: hidden.clone(),
            doing: "renaming into place",
            error: CompactError::Io(error),
        })
    });
    if let Err(error) = written {
        // The open of the next history deletes a hidden file this leaves.
        _ = fs::remove_file(&hidden);
        return Err(error);
    }
    let bytes = fs::metadata(output).map_err(|error| HistoryError::Compact {
        // alloc: an error
        output: output.to_owned(),
        // alloc: an error
        file: output.to_owned(),
        doing: "reading the size of",
        error: CompactError::Io(error),
    })?;
    Ok(bytes.len())
}

/// [`concat`] into the hidden file `hidden`.
fn concat_into(
    output: &Path,
    hidden: &Path,
    inputs: &[PathBuf],
    properties: &WriterProperties,
) -> Result<(), HistoryError> {
    let fail = |file: &Path, doing: &'static str, error: CompactError| HistoryError::Compact {
        // alloc: an error
        output: output.to_owned(),
        // alloc: an error
        file: file.to_owned(),
        doing,
        error,
    };
    let footer = |path: &Path| -> Result<(fs::File, ParquetMetaData), HistoryError> {
        let file = fs::File::open(path)
            .map_err(|error| fail(path, "opening the input", CompactError::Io(error)))?;
        let metadata = ParquetMetaDataReader::new()
            .with_page_index_policy(PageIndexPolicy::Optional)
            .parse_and_finish(&file)
            .map_err(|error| {
                fail(path, "reading the footer of the input", CompactError::Parquet(error))
            })?;
        Ok((file, metadata))
    };
    let Some(first) = inputs.first() else { return Ok(()) };
    let (first_file, first_metadata) = footer(first)?;
    let schema = first_metadata.file_metadata().schema_descr().root_schema_ptr();
    let out = fs::File::create(hidden)
        .map_err(|error| fail(hidden, "making", CompactError::Io(error)))?;
    // alloc: writer properties, once a merge
    let properties = Arc::new(properties.clone());
    let mut writer = SerializedFileWriter::new(out, Arc::clone(&schema), properties)
        .map_err(|error| fail(hidden, "starting to write", CompactError::Parquet(error)))?;
    // The Arrow schema and blaze's own keys (`blaze.schema`, `blaze.table`, `blaze.clock`); an
    // input's own count of merged rows is its, not this merge's.
    for kv in first_metadata
        .file_metadata()
        .key_value_metadata()
        .into_iter()
        .flatten()
        .filter(|it| it.key != MERGED_ROWS)
    {
        // clone: a key and its value, a few a merge
        writer.append_key_value_metadata(kv.clone());
    }
    let mut rows: i64 = 0;
    let mut next = Some((first_file, first_metadata));
    for path in inputs {
        let (file, metadata) = match next.take() {
            Some(it) => it,
            None => footer(path)?,
        };
        if metadata.file_metadata().schema() != &*schema {
            return Err(fail(
                path,
                "checking the schema of the input",
                CompactError::SchemaDiffers,
            ));
        }
        rows += metadata.file_metadata().num_rows();
        for (rg_at, row_group) in metadata.row_groups().iter().enumerate() {
            let index = metadata.page_index_for_row_group(rg_at);
            let mut group = writer.next_row_group().map_err(|error| {
                fail(hidden, "starting a row group of", CompactError::Parquet(error))
            })?;
            for (column_at, column) in row_group.columns().iter().enumerate() {
                let close = ColumnCloseResult {
                    bytes_written: column.compressed_size().cast_unsigned(),
                    rows_written: row_group.num_rows().cast_unsigned(),
                    // clone: a column chunk's metadata, once a chunk copied
                    metadata: column.clone(),
                    bloom_filter: None,
                    column_index: index.column_index(column_at).cloned(),
                    offset_index: index.offset_index(column_at).cloned(),
                };
                group.append_column(&file, close).map_err(|error| {
                    fail(path, "copying a column chunk of the input", CompactError::Parquet(error))
                })?;
            }
            group.close().map_err(|error| {
                fail(hidden, "ending a row group of", CompactError::Parquet(error))
            })?;
        }
    }
    // alloc: a key and its value, once a merge
    writer.append_key_value_metadata(KeyValue::new(MERGED_ROWS.to_owned(), rows.to_string()));
    writer.close().map_err(|error| fail(hidden, "ending", CompactError::Parquet(error)))?;
    Ok(())
}

/// The Parquet history of this process's flight recorder, in one directory.
pub struct History {
    dir: PathBuf,
    cap: u64,
    /// What the drain thread drained since the last flush took it.
    drained: Arc<Drained>,
    /// The drain thread: told to end by dropping `stop`, joined as the history drops.
    drainer: Option<(Sender<()>, JoinHandle<()>)>,
    /// A message each time the drained rows pass [`FLUSH_ROWS`].
    full: Receiver<()>,
    /// The rows a flush took from the drain: what it writes.
    read: Read,
    /// The spans lost, rows skipped and rows skipped behind the last flush wrote as counters.
    told: (u64, u64, u64),
    /// The reported bugs the last flush wrote: the process's total, and each site's count in
    /// `stdx::assert::reported::sites`'s order, which only grows.
    bugs_told: (u64, Vec<u64>),
    /// A flush's spans of other builds than the one it is for.
    others: Vec<SpanRow>,
    writer: Writer,
    process: Process<'static>,
    /// The next flush's number.
    seq: u64,
    /// Builds that started (outcome running) or ended since the last flush: written by the next.
    ended: Vec<BuildRow>,
    /// Errors builds said since the last flush: written by the next.
    errors: Vec<ErrorRow>,
    /// Images builds linked, and the crate graphs they built, since the last flush.
    links: Vec<LinkRow>,
    crate_deps: Vec<CrateDepRow>,
    /// What the query engine counted by each build's end, since the last flush.
    query_counts: Vec<QueryCountRow>,
    /// What builds computed again, and why, since the last flush.
    misses: Vec<MissRow>,
    /// Children reaped ([`facts::take_processes`]) and tests run since the last flush.
    processes: Vec<ProcessRow>,
    test_runs: Vec<TestRunRow>,
    /// What clients sent since the last flush, by table: written by the next.
    client_spans: Vec<RecordBatch>,
    client_events: Vec<RecordBatch>,
    client_folded: Vec<RecordBatch>,
    /// The highest build a client sent rows of since the last flush, 0 for none.
    client_build: u64,
    /// The last [`RECENT_BUILDS`] builds that ended, newest last: a client's times are given
    /// to its build's row, written again with them.
    recent: VecDeque<BuildRow>,
    /// Clients' times of builds that had not ended as they came, by build: given to its row
    /// as it ends. At most [`RECENT_BUILDS`], oldest dropped.
    pending_clients: VecDeque<(u64, (i64, i64))>,
    /// The parts of the keys of what builds computed again, since the last flush.
    key_parts: Vec<KeyPartRow>,
    /// The query engine's run log at each build's end, since the last flush.
    provenance: Vec<ProvenanceRow>,
    /// The files written, oldest first, and their bytes all told.
    kept: VecDeque<Kept>,
    total: u64,
    /// Bytes of the directory's other files the cap counts ([`side_bytes`]): the flight
    /// recorder's mappings (this process's, reserved whole, and any a postmortem refused) and
    /// the crash records. The cap trims the Parquet files to `cap` less these.
    side: u64,
    /// The highest build any file of the directory is named for, or a flush was for.
    last_build: u64,
    /// The ring a flush reads CPU samples from ([`History::open`]), the samples of it read so
    /// far, and a flush's new ones.
    sampler: &'static sampler::Ring,
    sampled: u64,
    samples: Vec<Sample>,
    /// The code images the samples' addresses are in that this history wrote rows of.
    images: symbols::ImagesWritten,
    /// [`identity::binary`], once a history: every build of this process ran it.
    blaze: String,
    /// A build's identity that could not be told since the last flush: the next flush writes
    /// its row without it, then answers this.
    unidentified: Option<HistoryError>,
    /// The threads' open spans the last flush wrote, their times left out ([`same_open`]).
    open_written: Vec<flight::ThreadNow>,
    /// Whether flushes write this process's panics ([`History::record_panics`]), the panics
    /// read so far, and a flush's new ones.
    panics_on: bool,
    panicked: u64,
    raised: Vec<panics::Raised>,
    /// The fatal signals of gone processes found as the history opened ([`crash::recover`]):
    /// their rows go with the next flush, their files once the commit that lists that flush's
    /// `panics` file consumed them (`crash_listing`).
    crashes: crash::Recovered,
    /// Crash records whose rows a flush wrote, for the next commit to consume and then delete.
    crash_listing: crash::Written,
    /// What failed after a flush's commit (a table that could not be written, a merge, the
    /// cap, a reap, a dead daemon's rows, a build's identity): the flush committed the rest
    /// and its number stands; each is reported through [`History::take_failed`].
    deferred: Vec<HistoryError>,
    /// What a flush writes of DEBUG events ([`History::events_policy`]).
    hold: logs::Hold,
    /// Where the drain was in the rings as the last flush took its rows: what the flight
    /// recorder's mapping marks written ([`flight::flush_pending`]).
    cursor: Cursor,
    /// Why a dead daemon's rows could not be read back as this history opened: the next flush
    /// answers it, so a damaged mapping never stops the daemon from starting.
    postmortem: Option<HistoryError>,
    /// The recorder is persisted to this history's directory ([`History::persist_recorder`]),
    /// and the last flush wrote: a history that drops so deletes the mapping.
    persisted: bool,
    clean: bool,
    /// The manifest may name retired files still on disk: set as this history retires files,
    /// or opens on a manifest that names some, so a flush that retired nothing takes no look
    /// at the manifest for [`History::reap`].
    reaping: bool,
}

/// The files of the history `dir` as its open finds them, under the manifest's lock: what a
/// crash left is deleted (each hidden file a write left half done, and each file of a form blaze
/// writes that the manifest neither lists nor retires: a merge's output not yet committed), a
/// retired file is left for [`History::reap`], and a file the manifest lists that is
/// not there is [`manifest::ManifestError::Missing`]. A directory with no manifest has one made
/// of its files first ([`manifest::hold`]). The files the cap counts (the manifest's flushes',
/// merges' and derived tables', oldest first; not a perf run's), the highest build they are
/// named for, the last flush the manifest says committed, and the crash records whose rows it
/// lists ([`manifest::Consumed::crashes`]).
#[allow(clippy::type_complexity, reason = "one caller, which takes it apart at once")]
fn sweep(
    dir: &Path,
) -> Result<(VecDeque<Kept>, u64, u64, BTreeSet<(u32, i64)>), HistoryError> {
    let mut held = manifest::hold(dir)?;
    // alloc: once a history
    let mut present: BTreeSet<String> = BTreeSet::new();
    for entry in fs::read_dir(dir).map_err(io_error(dir, "listing"))? {
        let entry = entry.map_err(io_error(dir, "listing"))?;
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            if name.to_string_lossy().ends_with(".parquet") {
                // alloc: an error
                return Err(HistoryError::Unclassified {
                    dir: dir.to_owned(),
                    file: name.to_string_lossy().into_owned(),
                });
            }
            continue;
        };
        match manifest::kind(name) {
            manifest::Kind::Hidden if half_written(name) => {
                fs::remove_file(&path).map_err(io_error(&path, "deleting a half-written file"))?;
            }
            manifest::Kind::Listable if held.manifest().contains(name) => {
                // alloc: a file's name, once a history
                present.insert(name.to_owned());
            }
            // Retired and not yet deleted: a reader's pin may hold it ([`History::reap`]).
            manifest::Kind::Listable if held.manifest().retired().holds(name) => {}
            manifest::Kind::Listable => match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(io_error(
                        &path,
                        "deleting a file no manifest lists (a crash left it)",
                    )(error));
                }
            },
            // A newer blaze's table, listed: kept as written, read by no view of this blaze.
            manifest::Kind::Unclassified if held.manifest().contains(name) => {
                // alloc: a file's name, once a history
                present.insert(name.to_owned());
            }
            // A newer blaze's table, retired and not yet deleted: as any retired file.
            manifest::Kind::Unclassified if held.manifest().retired().holds(name) => {}
            manifest::Kind::Unclassified => {
                // alloc: an error
                return Err(HistoryError::Unclassified {
                    dir: dir.to_owned(),
                    file: name.to_owned(),
                });
            }
            manifest::Kind::Hidden | manifest::Kind::Schema | manifest::Kind::Other => {}
        }
    }
    if let Some(missing) = held.manifest().all().find(|it| !present.contains(*it)) {
        return Err(HistoryError::Manifest {
            path: dir.join(manifest::FILE),
            // alloc: an error
            why: manifest::ManifestError::Missing { file: missing.to_owned() },
        });
    }
    // alloc: once a history
    let mut kept = Vec::new();
    let mut last_build = 0;
    for name in held.manifest().all() {
        let Some(named) = Name::parse(name) else { continue };
        last_build = last_build.max(named.build);
        let path = dir.join(name);
        let bytes = fs::metadata(&path).map_err(io_error(&path, "reading the size of"))?.len();
        let Some(entry) = held.manifest().entry(name) else { continue };
        // clone: a file's entry, once a history
        kept.push(Kept {
            name: named,
            bytes,
            path,
            entry: entry.clone(),
            ours: false,
            listed: true,
        });
    }
    kept.sort_unstable_by(|a, b| {
        (a.name.last, name_of(&a.path)).cmp(&(b.name.last, name_of(&b.path)))
    });
    let flushed = held.manifest().flushed();
    // What a past commit consumed of a mapping or crash record since deleted is forgotten.
    held.prune_consumed(dir);
    // clone: the crash records consumed, once a history
    let crashes = held.manifest().consumed().crashes.clone();
    held.commit()?;
    Ok((kept.into(), last_build, flushed, crashes))
}

impl History {
    /// The history in `dir`, made if it isn't there, holding at most `cap` bytes of files. Its
    /// first flush takes everything the rings still hold. A flush's number continues from the
    /// manifest's; what a crash left is deleted ([`sweep`]), and the retired files no reader's
    /// pin holds ([`History::reap`]). Its flushes
    /// read CPU samples from `sampler`: the process's ring ([`sampler::process`]) in a daemon,
    /// a ring of a test's own in a test, so what a test's flush writes never depends on a
    /// sampler that something else in its process started.
    pub fn open(
        dir: &Path,
        cap: u64,
        sampler: &'static sampler::Ring,
    ) -> Result<History, HistoryError> {
        let (mut kept, mut last_build, flushed, crashes_listed) = sweep(dir)?;
        let mut seq = kept.iter().map(|it| it.name.last).max().unwrap_or(0).max(flushed) + 1;
        let mut total: u64 = kept.iter().map(|it| it.bytes).sum();
        // Each gone process's fatal signal: a row for the first flush. A record refused (another
        // layout, another blaze) is set aside and said; it never stops the history opening.
        let mut crashes = crash::recover(dir);
        // A record whose row a listed file holds: a crash came between that commit and its
        // delete. Deleted unread, never written twice.
        crashes.drop_written(&crashes_listed);
        for refused in &crashes.refused {
            tracing::warn!(target: "blaze", "{refused}");
        }
        // A build of a daemon that crashed before a flush named a file for it keeps its number.
        last_build = crashes.rows.iter().filter_map(|it| it.build_id).fold(last_build, u64::max);
        let mut writer = Writer::new();
        writer.schemas(dir)?;
        // The last rows of daemons that died before they flushed them, as files of their own
        // before this history writes any ([`recover_into`]).
        let (recovered, postmortem) = recover_into(dir, &mut writer, &mut seq);
        for file in recovered {
            last_build = last_build.max(file.name.build);
            total += file.bytes;
            kept.push_back(file);
        }
        let blaze = identity::binary().map_err(|error| HistoryError::Identity {
            what: "hashing blaze's own executable",
            path: std::env::current_exe().unwrap_or_default(),
            error,
        })?;
        let drained = Arc::new(Drained::default()); // alloc: once a history
        let (full_to, full) = crossbeam_channel::bounded(1);
        let (stop, stopped) = crossbeam_channel::bounded::<()>(0);
        let draining = Arc::clone(&drained);
        let drainer = std::thread::Builder::new()
            .name("blaze-drain".to_owned()) // alloc: once a history
            .spawn(move || drain(&draining, &stopped, &full_to))
            .map_err(io_error(
                dir,
                "starting the drain thread (`blaze-drain`) of the history in",
            ))?;
        let mut history = History {
            // alloc: once a history
            dir: dir.to_owned(),
            cap,
            drained,
            drainer: Some((stop, drainer)),
            full,
            read: Read::default(),
            told: (0, 0, 0),
            // alloc: once a history; a site a bug reported in the process
            bugs_told: (0, Vec::new()),
            // alloc: once a history
            others: Vec::new(),
            writer,
            process: Process::this(),
            seq,
            // alloc: once a history
            ended: Vec::new(),
            // alloc: once a history
            errors: Vec::new(),
            // alloc: once a history
            links: Vec::new(),
            // alloc: once a history
            crate_deps: Vec::new(),
            // alloc: once a history
            query_counts: Vec::new(),
            // alloc: once a history
            misses: Vec::new(),
            // alloc: once a history
            processes: Vec::new(),
            // alloc: once a history
            test_runs: Vec::new(),
            // alloc: once a history
            client_spans: Vec::new(),
            // alloc: once a history
            client_events: Vec::new(),
            // alloc: once a history
            client_folded: Vec::new(),
            client_build: 0,
            // alloc: once a history
            recent: VecDeque::with_capacity(RECENT_BUILDS),
            // alloc: once a history
            pending_clients: VecDeque::new(),
            // alloc: once a history
            key_parts: Vec::new(),
            // alloc: once a history
            provenance: Vec::new(),
            kept: kept.into(),
            total,
            side: side_bytes(dir),
            last_build,
            sampler,
            sampled: 0,
            // alloc: once a history
            samples: Vec::new(),
            images: symbols::ImagesWritten::new(),
            blaze,
            unidentified: None,
            // alloc: once a history
            open_written: Vec::new(),
            panics_on: false,
            panicked: 0,
            // alloc: once a history
            raised: Vec::new(),
            crashes,
            crash_listing: crash::Written::default(),
            // alloc: once a history
            deferred: Vec::new(),
            hold: logs::Hold::new(logs::Policy::default()),
            cursor: Cursor::default(),
            postmortem,
            persisted: false,
            clean: false,
            // The first reap looks: a manifest a crash left may name retired files.
            reaping: true,
        };
        history.trim()?;
        history.reap()?;
        Ok(history)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Has each flush write the panics this process raised since the last ([`crate::panics`]):
    /// a daemon's history, the one of its process. Off, as in a test's history, a flush writes
    /// only the fatal signals it recovered, so what a test's flush writes never depends on a
    /// panic of another test of its process.
    pub fn record_panics(&mut self) {
        self.panics_on = true;
    }

    /// The crash records the history refused as it opened, each set aside as
    /// `crash.<pid>.refused` ([`crash::recover`]).
    pub fn refused_crashes(&self) -> &[crash::CrashError] {
        &self.crashes.refused
    }

    /// Keeps this process's flight recorder past a crash, in a file of this history's
    /// directory ([`flight::persist`]): the next history to open here writes the rows this one
    /// had not, and marks the builds that were running `crashed`. Once a process: the daemon's
    /// history, as it opens.
    pub fn persist_recorder(&mut self) -> Result<(), HistoryError> {
        flight::persist(&self.dir, &self.blaze, self.process.name)
            .map_err(HistoryError::Postmortem)?;
        self.persisted = true;
        // The mapping is reserved whole as it is made: the cap counts it from now on, and the
        // crash record armed beside it.
        self.side = side_bytes(&self.dir);
        self.trim()?;
        Ok(())
    }

    /// The first build id no file of the history names: where the daemon starts numbering its
    /// builds, so a build id stays unique across the daemon's restarts. Never 0, which the
    /// recorder takes for "no build".
    pub fn next_build(&self) -> u32 {
        u32::try_from(self.last_build + 1).unwrap_or(u32::MAX)
    }

    /// Notes a build that started, `row` its row with outcome running and no duration, for the
    /// next flush to write: a build its daemon never ends (a kill, a crash) is then one row in
    /// the history, which a reader tells from one still running by its process ([`writes`]).
    pub fn build_started(&mut self, mut row: BuildRow) {
        if row.blaze.is_none() {
            // clone: a hash, once a build
            row.blaze = Some(self.blaze.clone());
        }
        flight::postmortem::build_started(&flight::postmortem::RunningBuild {
            build_id: row.build_id,
            started_ns: row.started_ns,
            started_unix_ns: row.started_unix_ns,
            commit: row.commit.as_deref(),
            workspace: &row.workspace,
            command: &row.command,
        });
        self.ended.push(row);
    }

    /// Notes a build that ended, for the next flush to write; the flush for its build
    /// ([`History::flush`] with `Some(row.build_id)`) also gives its spans the build's commit.
    /// The row's `blaze` and `tree` are told here when not given: on the history's thread, never
    /// the build's. A tree that can't be digested leaves `tree` null, and the next flush answers
    /// why.
    pub fn build_ended(&mut self, mut row: BuildRow) {
        flight::postmortem::build_ended(row.build_id);
        if row.blaze.is_none() {
            // clone: a hash, once a build
            row.blaze = Some(self.blaze.clone());
        }
        let workspace = Path::new(&row.workspace);
        if row.tree.is_none() && workspace.is_dir() {
            match identity::tree_digest(workspace) {
                Ok(tree) => row.tree = Some(tree),
                Err(error) => {
                    self.unidentified = Some(HistoryError::Identity {
                        what: "digesting the source tree of the build's workspace",
                        // alloc: an error
                        path: workspace.to_owned(),
                        error,
                    });
                }
            }
        }
        if let Ok(build) = u32::try_from(row.build_id) {
            match row.outcome {
                Outcome::Failed | Outcome::Stalled => self.hold.ended(build, true),
                Outcome::Ok | Outcome::Cancelled => self.hold.ended(build, false),
                Outcome::Running => {}
                // Written only by a postmortem, never ended live; troubled as a failure is.
                Outcome::Crashed => self.hold.ended(build, true),
            }
        }
        if let Some(at) = self.pending_clients.iter().position(|it| it.0 == row.build_id) {
            row.client_ns = self.pending_clients.remove(at).map(|it| it.1);
        }
        if self.recent.len() == RECENT_BUILDS {
            self.recent.pop_front();
        }
        // clone: the row kept for its client's times, once a build
        self.recent.push_back(row.clone());
        self.ended.push(row);
    }

    /// Sets what the flushes write of DEBUG events from now on ([`logs::Policy`]): the daemon's
    /// [`logs::POLICY_VAR`]. A history opens under [`logs::Policy::Info`].
    pub fn events_policy(&mut self, policy: logs::Policy) {
        if self.hold.policy() != policy {
            self.hold = logs::Hold::new(policy);
        }
    }

    /// Has the next flush write the DEBUG events held for builds that have not ended: the
    /// history is closing under them (the daemon stops, or its builds' requests are gone).
    pub fn release_held(&mut self) {
        self.hold.release_all();
    }

    /// Notes what the client of a build sent as it ended, for the next flush to write: its
    /// rows, each table's batches checked against the table's schema and every row's build
    /// against the build, and its times, given to the build's row, which is written again
    /// with them (the `builds` view keeps that one). A build not ended yet gets them as it
    /// ends. A batch that does not fit is the error, naming the build and table; nothing of
    /// that client is noted then.
    pub fn client_rows(&mut self, client: ClientRows) -> Result<(), HistoryError> {
        let build = client.build;
        let spans = client_batches(build, &schema::SPANS, &client.spans)?;
        let events = client_batches(build, &schema::EVENTS, &client.events)?;
        let folded = client_batches(build, &schema::FOLDED, &client.folded)?;
        self.client_spans.extend(spans);
        self.client_events.extend(events);
        self.client_folded.extend(folded);
        self.client_build = self.client_build.max(build);
        let times = (client.started_ns, client.ended_ns);
        match self.recent.iter_mut().rev().find(|it| it.build_id == build) {
            Some(row) => {
                row.client_ns = Some(times);
                // clone: the build's row written again with its client's times, once a build
                self.ended.push(row.clone());
            }
            None => {
                if self.pending_clients.len() == RECENT_BUILDS {
                    self.pending_clients.pop_front();
                }
                self.pending_clients.push_back((build, times));
            }
        }
        Ok(())
    }

    /// Notes children reaped, for the next flush to write beside those the drivers queued
    /// ([`facts::reaped`]).
    pub fn processes_reaped(&mut self, rows: impl IntoIterator<Item = ProcessRow>) {
        self.processes.extend(rows);
    }

    /// Notes the tests a build ran, one row a test, for the next flush to write.
    pub fn tests_ran(&mut self, rows: impl IntoIterator<Item = TestRunRow>) {
        self.test_runs.extend(rows);
    }

    /// Notes errors a build said ([`errors::rows`]), for the next flush to write.
    pub fn errored(&mut self, rows: impl IntoIterator<Item = ErrorRow>) {
        self.errors.extend(rows);
    }

    /// Notes the images a build linked and how, for the next flush to write.
    pub fn linked(&mut self, rows: impl IntoIterator<Item = LinkRow>) {
        self.links.extend(rows);
    }

    /// Notes the crate graph a build built, one row an edge, for the next flush to write.
    pub fn crate_graph(&mut self, rows: impl IntoIterator<Item = CrateDepRow>) {
        self.crate_deps.extend(rows);
    }

    /// Notes what the query engine counted by a build's end, one row a query function, for
    /// the next flush to write.
    pub fn query_counted(&mut self, rows: impl IntoIterator<Item = QueryCountRow>) {
        self.query_counts.extend(rows);
    }

    /// Notes what a build computed again and why, one row a thing, for the next flush to
    /// write.
    pub fn missed(&mut self, rows: impl IntoIterator<Item = MissRow>) {
        self.misses.extend(rows);
    }

    /// Notes the parts of the key of each thing a build computed again, for the next flush to
    /// write.
    pub fn keyed(&mut self, rows: impl IntoIterator<Item = KeyPartRow>) {
        self.key_parts.extend(rows);
    }

    /// Notes the query engine's run log as a build ended (why each run ran), for the next
    /// flush to write.
    pub fn ran(&mut self, rows: impl IntoIterator<Item = ProvenanceRow>) {
        self.provenance.extend(rows);
    }

    /// Writes what the rings hold that is new since the last flush, and the builds that ended
    /// since, one file a table with rows, named for the highest build any of its rows is of and
    /// never below `build` (`None`: the timer's flush, named 0 when it holds no build's rows).
    /// Then merges files this history wrote ([`Flushed::merged`]; each table of `build` into one
    /// file when `build` is given, as its build ends) and retires the oldest files past the cap,
    /// then deletes the retired files no reader pins.
    /// A flush whose commit fails is the error, naming why; the files it wrote are listed by
    /// the next. Once its commit is done, its number stands: what fails after it (a table's
    /// file that could not be written, whose rows of the rings are lost and whose other rows
    /// the next flush writes; a merge; the cap; a reap; a dead daemon's rows) is said through
    /// [`History::take_failed`].
    pub fn flush(&mut self, build: Option<u64>) -> Result<Flushed, HistoryError> {
        // The record cost of the history itself: encoding and writing a flush's Parquet had no
        // span, so a slow flush read as uncovered time on the history's thread.
        let _span = tracing::info_span!(target: "blaze", "profile flush").entered();
        let (late, behind) = self.drained.take(&mut self.read, &mut self.cursor);
        // DEBUG events are held for their build's end, written if it was in trouble.
        self.hold.sort(&mut self.read);
        // The flight recorder's own losses, as counters: a query sees whether a stretch with
        // no span is one the recorder dropped. Written with each build's flush and whenever
        // they grew, so an idle daemon's timer writes no file for them.
        // Counted since the process began; the row of a build's flush carries the build.
        let (t_ns, now) = (self.read.at_ns, (self.read.lost, late + behind, behind));
        if build.is_some() || now != self.told {
            self.told = now;
            let build = build.and_then(|it| u32::try_from(it).ok()).unwrap_or(0);
            self.read.counters.extend(
                [
                    (Plot::ProfileSpansLost, now.0),
                    (Plot::ProfileRowsSkipped, now.1),
                    (Plot::ProfileRowsBehind, now.2),
                ]
                .map(|(plot, value)| CounterRow {
                    build,
                    plot,
                    t_ns,
                    value,
                    fiber: None,
                }),
            );
        }
        // The recorder's own measured cost ([`flight::cost`]), with each build's flush only: it
        // grows with every span (this flush's own), so writing it whenever it grew would give an
        // idle daemon's timer a file each tick. A build's delta runs from the flush of the build
        // before it, so it holds the idle stretch between them too.
        if let Some(of) = build {
            let cost = flight::cost();
            let build = u32::try_from(of).unwrap_or(0);
            self.read.counters.extend(
                [
                    (Plot::ProfileCostNs, cost.ns),
                    (Plot::ProfileSpansMade, cost.spans),
                    (Plot::ProfileCpuReads, cost.cpu_reads),
                ]
                .map(|(plot, value)| CounterRow {
                    build,
                    plot,
                    t_ns,
                    value,
                    fiber: None,
                }),
            );
        }
        self.told_bugs(build, t_ns, stdx::assert::reported::total(), stdx::assert::reported::sites);
        facts::take_processes(&mut self.processes);
        let written = self.write(build);
        self.clean = written.is_ok();
        // Written or not, the rows drained are this flush's: none goes to the next.
        self.read.clear();
        written
    }

    /// The bugs the process reported and went on past (`stdx::assert::reported`), for the
    /// flush of `build` at `t_ns`: the [`Plot::BlazeBugs`] counter, with each build's flush and
    /// whenever it grew, and an `errors` row of kind `blaze-bug` for each site that reported
    /// since the last flush, naming its value kind, its site and how many more times.
    /// `total` and `sites` are `stdx::assert::reported`'s; `sites` is read only when `total`
    /// grew.
    fn told_bugs(
        &mut self,
        build: Option<u64>,
        t_ns: i64,
        total: u64,
        sites: impl FnOnce() -> Vec<stdx::assert::reported::Site>,
    ) {
        let grew = total != self.bugs_told.0;
        let build32 = build.and_then(|it| u32::try_from(it).ok()).unwrap_or(0);
        if build.is_some() || grew {
            self.read.counters.push(CounterRow {
                build: build32,
                plot: Plot::BlazeBugs,
                t_ns,
                value: total,
                fiber: None,
            });
        }
        if !grew {
            return;
        }
        self.bugs_told.0 = total;
        for (at, site) in sites().into_iter().enumerate() {
            if at == self.bugs_told.1.len() {
                self.bugs_told.1.push(0);
            }
            let more = site.count - self.bugs_told.1[at];
            if more == 0 {
                continue;
            }
            self.bugs_told.1[at] = site.count;
            let detail = match (&site.thread, &site.backtrace) {
                // alloc: a row a site of a bug, once a flush it grew in
                (thread, Some(stack)) => format!(
                    "first on thread `{}`:\n{stack}",
                    thread.as_deref().unwrap_or("unnamed")
                ),
                // alloc: as above
                (thread, None) => {
                    format!("first on thread `{}`", thread.as_deref().unwrap_or("unnamed"))
                }
            };
            self.errors.push(ErrorRow {
                build_id: build.unwrap_or(0),
                t_ns: Some(t_ns),
                krate: None,
                unit: None,
                // alloc: as above
                kind: errors::Kind::BlazeBug.name().to_owned(),
                // alloc: as above
                function: Some(site.kind.to_owned()),
                // alloc: as above
                message: format!(
                    "a blaze bug: `{}` {}, {more} more time{}",
                    site.kind,
                    site.what,
                    if more == 1 { "" } else { "s" }
                ),
                detail: Some(detail),
                // alloc: as above
                file: Some(site.at.file().to_owned()),
                line: Some(site.at.line()),
            });
        }
    }

    /// A message each time the rows drained since the last flush pass [`FLUSH_ROWS`]: the
    /// history's keeper flushes then, before [`EVERY`] is up.
    pub fn full(&self) -> &Receiver<()> {
        &self.full
    }

    /// [`History::flush`] of the rows drained.
    fn write(&mut self, build: Option<u64>) -> Result<Flushed, HistoryError> {
        // The flush of a build's end merges each of its tables into one file.
        let ended = build;
        let seq = self.seq;
        self.seq += 1;
        // The panics since the last flush, and the crashes recovered as the history opened.
        self.raised.clear();
        let panics_skipped = match self.panics_on {
            true => panics::since(&mut self.panicked, &mut self.raised),
            false => 0,
        };
        let mut panic_rows = panics::rows(&self.raised);
        // clone: the rows of crashes recovered, once a flush until one writes them
        panic_rows.extend(self.crashes.rows.iter().cloned());
        let named =
            panic_rows.iter().filter_map(|it| it.build_id).fold(self.named_for(build), u64::max);
        // The spans of `build` are given its commit; the rest (other builds', and none's) none.
        let commit = build
            .and_then(|build| self.ended.iter().find(|it| it.build_id == build))
            .and_then(|it| it.commit.as_deref());
        self.others.clear();
        if let (Some(build), Some(_)) = (build, commit) {
            let others = &mut self.others;
            self.read.spans.retain(|it| {
                let of = u64::from(it.build) == build;
                if !of {
                    others.push(*it);
                }
                of
            });
        }
        let of_build = Process { commit, ..self.process };
        let client_rows =
            |parts: &[RecordBatch]| parts.iter().map(RecordBatch::num_rows).sum::<usize>();
        let client = client_rows(&self.client_spans)
            + client_rows(&self.client_events)
            + client_rows(&self.client_folded);
        let mut spans = vec![
            batches::spans(&self.read.spans, of_build).map_err(arrow_error(&schema::SPANS))?,
            batches::spans(&self.others, self.process).map_err(arrow_error(&schema::SPANS))?,
        ]; // alloc: a flush's parts, once a flush
        spans.append(&mut self.client_spans);
        let counters = [batches::counters(&self.read.counters, self.process)
            .map_err(arrow_error(&schema::COUNTERS))?];
        let builds = [builds(&self.ended, self.process.pid).map_err(arrow_error(&schema::BUILDS))?];
        let errors = [errors::batch(&self.errors).map_err(arrow_error(&schema::ERRORS))?];
        let links = [facts::links(&self.links).map_err(arrow_error(&schema::LINKS))?];
        let crate_deps =
            [facts::crate_deps(&self.crate_deps).map_err(arrow_error(&schema::CRATE_DEPS))?];
        let query_counts =
            [facts::query_counts(&self.query_counts)
                .map_err(arrow_error(&schema::QUERY_COUNTS))?];
        let misses = [facts::misses(&self.misses).map_err(arrow_error(&schema::MISSES))?];
        let key_parts =
            [facts::key_parts(&self.key_parts).map_err(arrow_error(&schema::KEY_PARTS))?];
        let provenance =
            [facts::provenance(&self.provenance).map_err(arrow_error(&schema::PROVENANCE))?];
        let switches = [batches::switches(&self.read.switches, self.process)
            .map_err(arrow_error(&schema::SWITCHES))?];
        let mut events = vec![
            batches::events(&self.read.events, self.process)
                .map_err(arrow_error(&schema::EVENTS))?,
        ]; // alloc: as `spans`
        events.append(&mut self.client_events);
        let event_fields = [batches::event_fields(&self.read.fields, self.process)
            .map_err(arrow_error(&schema::EVENT_FIELDS))?];
        let mut folded = vec![
            batches::folded(&self.read.folds, self.process)
                .map_err(arrow_error(&schema::FOLDED))?,
        ]; // alloc: as `spans`
        folded.append(&mut self.client_folded);
        let processes = [facts::processes(&self.processes, self.process.pid)
            .map_err(arrow_error(&schema::PROCESSES))?];
        let test_runs =
            [facts::test_runs(&self.test_runs).map_err(arrow_error(&schema::TEST_RUNS))?];
        self.samples.clear();
        let samples_skipped = self.sampler.since(&mut self.sampled, &mut self.samples);
        let samples = [self.samples_batch().map_err(arrow_error(&schema::SAMPLES))?];
        // The images the samples' and this process's panics' addresses are in, and each
        // recovered crash's executable, of its own process.
        let seen_ns = clock::mono_ns();
        let stacks = self.samples.iter().map(|it| &it.pcs[..usize::from(it.depth)]);
        let stacks = stacks.chain(self.raised.iter().map(panics::Raised::pcs));
        let mut images = vec![
            self.images
                .batch(stacks, seen_ns, self.process)
                .map_err(arrow_error(&schema::IMAGES))?,
        ]; // alloc: as `spans`
        for (pid, image) in &self.crashes.images {
            let process = Process { pid: *pid, commit: None, ..self.process };
            images.push(
                batches::images(std::slice::from_ref(image), None, seen_ns, process)
                    .map_err(arrow_error(&schema::IMAGES))?,
            );
        }
        let rows = |parts: &[RecordBatch]| parts.iter().map(RecordBatch::num_rows).sum::<usize>();
        // What every thread is inside, with each flush where that changed: the last reading is
        // what a daemon that died was doing, which no `spans` row holds (they are written as
        // they close), and a deadlocked one's stays its last. An idle daemon's flushes write none.
        let now = flight::threads_now();
        let open = match same_open(&now, &self.open_written) {
            false => {
                let batch = batches::open(&now, clock::mono_ns(), self.process)
                    .map_err(arrow_error(&schema::OPEN))?;
                self.open_written = now;
                batch
            }
            true => RecordBatch::new_empty(batches::schema_of(&schema::OPEN)),
        };
        let open = [open];
        let panics = [panics::batch(&panic_rows).map_err(arrow_error(&schema::PANICS))?];
        let mut flushed = Flushed {
            seq,
            spans: rows(&spans),
            counters: rows(&counters),
            builds: rows(&builds),
            samples: rows(&samples),
            switches: rows(&switches),
            events: rows(&events),
            event_fields: rows(&event_fields),
            events_held: self.hold.held(),
            events_dropped: self.hold.dropped(),
            errors: rows(&errors),
            links: rows(&links),
            crate_deps: rows(&crate_deps),
            query_counts: rows(&query_counts),
            misses: rows(&misses),
            key_parts: rows(&key_parts),
            provenance: rows(&provenance),
            open: rows(&open),
            folded: rows(&folded),
            panics: rows(&panics),
            processes: rows(&processes),
            test_runs: rows(&test_runs),
            client,
            skipped: self.read.skipped + samples_skipped + panics_skipped,
            ..Flushed::default()
        };
        let build = named;
        flushed.build = build;
        self.last_build = self.last_build.max(build);
        let tables: [(&'static Table, &[RecordBatch]); 20] = [
            (&schema::SPANS, &spans),
            (&schema::COUNTERS, &counters),
            (&schema::BUILDS, &builds),
            (&schema::SAMPLES, &samples),
            (&schema::SWITCHES, &switches),
            (&schema::EVENTS, &events),
            (&schema::ERRORS, &errors),
            (&schema::LINKS, &links),
            (&schema::CRATE_DEPS, &crate_deps),
            (&schema::QUERY_COUNTS, &query_counts),
            (&schema::MISSES, &misses),
            (&schema::KEY_PARTS, &key_parts),
            (&schema::PROVENANCE, &provenance),
            (&schema::OPEN, &open),
            (&schema::FOLDED, &folded),
            (&schema::PANICS, &panics),
            (&schema::EVENT_FIELDS, &event_fields),
            (&schema::PROCESSES, &processes),
            (&schema::TEST_RUNS, &test_runs),
            (&schema::IMAGES, &images),
        ];
        // Each table's file is written to its place, then all are listed in one commit: a
        // reader sees all of the flush or none of it. A table whose file can't be written
        // loses only its own rows; the others are written still.
        let mut wrote = [false; schema::TABLES.len()];
        let mut failed = None;
        for (table, parts) in tables {
            if rows(parts) == 0 {
                wrote[table_at(table)] = true;
                continue;
            }
            let name = Name { build, table: table_at(table), first: seq, last: seq, level: 0 };
            let path = self.dir.join(name.file_name());
            match self.writer.write(&path, table, parts) {
                Ok(bytes) => {
                    let entry = manifest::Entry::of_batches(table, parts);
                    self.kept.push_back(Kept { name, bytes, path, entry, ours: true, listed: false });
                    self.total += bytes;
                    flushed.files += 1;
                    flushed.bytes += bytes;
                    wrote[table_at(table)] = true;
                }
                Err(error) => _ = failed.get_or_insert(error),
            }
        }
        // What the flush wrote leaves memory table by table: a table whose file was written
        // holds those rows now (listed by this commit, or by the next if this one fails), and
        // one whose file was not keeps them for the next flush. Until 2026-11-04 a failed
        // table kept every table's rows, which the next flush wrote a second time beside the
        // files already listed.
        let done = |table: &Table| wrote[table_at(table)];
        if done(&schema::BUILDS) {
            self.ended.clear();
        }
        if done(&schema::ERRORS) {
            self.errors.clear();
        }
        if done(&schema::LINKS) {
            self.links.clear();
        }
        if done(&schema::CRATE_DEPS) {
            self.crate_deps.clear();
        }
        if done(&schema::QUERY_COUNTS) {
            self.query_counts.clear();
        }
        if done(&schema::MISSES) {
            self.misses.clear();
        }
        if done(&schema::PROCESSES) {
            self.processes.clear();
        }
        if done(&schema::TEST_RUNS) {
            self.test_runs.clear();
        }
        if done(&schema::KEY_PARTS) {
            self.key_parts.clear();
        }
        if done(&schema::PROVENANCE) {
            self.provenance.clear();
        }
        // The clients' batches went into this flush's parts.
        self.client_build = 0;
        // A crash's records are consumed by the commit that lists its `panics` row, and only
        // then deleted; a record whose row was not written is written by the next flush.
        if done(&schema::PANICS) {
            let mut written = self.crashes.take_written();
            self.crash_listing.append(&mut written);
        }
        if done(&schema::IMAGES) {
            self.crashes.images.clear();
        }
        // The rows the flush took, marked in the recorder's mapping as this flush's, pending
        // its commit: a postmortem reads past them once the manifest says this flush
        // committed, so a crash on either side of the commit writes no row twice and loses
        // none. The DEBUG rows still held for builds not ended stay readable before the marks.
        let recorder = flight::postmortem::recorder();
        if recorder.is_some() {
            // alloc: once a flush, at most `logs::HOLD_BUILDS`
            let held_builds: Vec<u32> = self.hold.held_builds().collect();
            flight::flush_pending(&self.cursor, self.hold.held_ids(), &held_builds, seq);
        }
        // The commit point. A commit that fails (another process held the manifest's lock
        // past its bound) leaves this flush's files in place, unlisted: the next flush lists
        // them first, and a crash before it leaves them to the next open's sweep, and their
        // rings' rows to the postmortem, which reads past the last committed marks.
        if let Err(error) = self.list(seq, recorder) {
            self.deferred.extend(failed);
            return Err(error);
        }
        flight::flush_committed(seq, self.read.at_ns);
        if !self.crash_listing.is_empty() {
            // One that can't be deleted is consumed already: the next open deletes it unread.
            if let Err(error) = self.crash_listing.delete() {
                tracing::warn!(target: "blaze", "{error}");
            }
            // Crash records deleted since the cap last counted them.
            self.side = side_bytes(&self.dir);
        }
        // Committed: what fails from here on is said apart, and the flush's number stands.
        if let Some(error) = failed {
            self.deferred.push(error);
        }
        if let Err(error) = self.compact(ended, &mut flushed) {
            self.deferred.push(error);
        }
        match self.trim() {
            Ok(deleted) => flushed.deleted = deleted,
            Err(error) => self.deferred.push(error),
        }
        // After every retirement is committed, so a reader either read the manifest that
        // retired a file or pins it.
        if let Err(error) = self.reap() {
            self.deferred.push(error);
        }
        let postmortem = self
            .postmortem
            .take()
            .or_else(|| flight::postmortem::failed().map(HistoryError::Postmortem));
        self.deferred.extend(postmortem);
        self.deferred.extend(self.unidentified.take());
        Ok(flushed)
    }

    /// Lists every file of the history not yet listed (this flush's, and any a failed commit
    /// left), notes flush `seq` committed, of this process's recorder when it is persisted
    /// (`recorder`, its pid and nonce), and consumes the crash records whose rows those files
    /// hold: one commit.
    fn list(&mut self, seq: u64, recorder: Option<(u32, u64)>) -> Result<(), HistoryError> {
        let mut held = manifest::hold(&self.dir)?;
        for kept in self.kept.iter().filter(|it| !it.listed) {
            // clone: a file's entry, once a file
            held.add(name_of(&kept.path), kept.entry.clone())?;
        }
        held.flush_committed(seq);
        if let Some((pid, nonce)) = recorder {
            held.recorder_flushed(pid, nonce, seq);
        }
        held.crashes_written(self.crash_listing.ids.iter().copied());
        held.commit()?;
        drop(held);
        for kept in &mut self.kept {
            kept.listed = true;
        }
        Ok(())
    }

    /// What failed after the commits of the flushes since the last call, each an error
    /// naming what failed: the flushes' own numbers stand ([`History::flush`]). The daemon
    /// reports each, as it does a flush that failed.
    pub fn take_failed(&mut self) -> Vec<HistoryError> {
        std::mem::take(&mut self.deferred)
    }

    /// Merges the files this history wrote since it opened as [`compact::plan`] says, `ended`
    /// the build whose end this flush is for, counting the files merged and made in `flushed`.
    /// Each merge's output is renamed into place, then listed in one commit of the manifest that
    /// retires its inputs (deleted once no pin holds them, [`History::reap`]); a merge that
    /// fails leaves every input listed and ends the compaction.
    fn compact(&mut self, ended: Option<u64>, flushed: &mut Flushed) -> Result<(), HistoryError> {
        // alloc: once a flush
        let ours: Vec<usize> =
            (0..self.kept.len()).filter(|&at| self.kept[at].ours && self.kept[at].listed).collect();
        // alloc: once a flush
        let names: Vec<Name> = ours.iter().map(|&at| self.kept[at].name).collect();
        let merges = compact::plan(&names, ended);
        if merges.is_empty() {
            return Ok(());
        }
        // The paths first: `kept` changes as each merge is done.
        let jobs: Vec<Job> = merges
            .into_iter()
            .map(|merge| Job {
                // clone: a merge's paths, once a merge
                inputs: merge.inputs.iter().map(|&at| self.kept[ours[at]].path.clone()).collect(),
                output: merge.output,
            })
            // alloc: once a flush that compacts
            .collect();
        for job in jobs {
            let _span = tracing::info_span!(target: "blaze", "profile merge").entered();
            let path = self.dir.join(job.output.file_name());
            let properties = &self.writer.properties[job.output.table];
            let bytes = concat(&path, &job.inputs, properties)?;
            let entry = manifest::Entry::merged(
                self.kept.iter().filter(|it| job.inputs.contains(&it.path)).map(|it| &it.entry),
            )
            .ok_or_else(|| HistoryError::Compact {
                // clone: an error
                output: path.clone(),
                // clone: an error
                file: path.clone(),
                doing: "taking the entries of its inputs, of which the history keeps none",
                error: CompactError::SchemaDiffers,
            })?;
            // The commit point: the output listed and its inputs retired at once, so a reader
            // reads the rows once. A commit that fails leaves the output unlisted, which the
            // next open deletes, and every input listed.
            let mut held = manifest::hold(&self.dir)?;
            // clone: a file's entry, once a merge
            held.add(name_of(&path), entry.clone())?;
            held.retire(job.inputs.iter().map(|it| name_of(it)));
            self.reaping = true;
            held.commit()?;
            drop(held);
            let mut bytes_gone = 0;
            self.kept.retain(|it| {
                let merged = job.inputs.contains(&it.path);
                if merged {
                    bytes_gone += it.bytes;
                }
                !merged
            });
            self.total -= bytes_gone;
            let at = self.kept.partition_point(|it| it.name.last <= job.output.last);
            self.kept.insert(
                at,
                Kept { name: job.output, bytes, path, entry, ours: true, listed: true },
            );
            self.total += bytes;
            flushed.compacted += 1;
            flushed.merged += u32::try_from(job.inputs.len()).unwrap_or(u32::MAX);
        }
        Ok(())
    }

    /// Deletes the retired files no reader's pin holds, then commits that they are gone: how
    /// many. Called only once every retirement is committed, so a reader either read the
    /// manifest that retired a file or holds it with its pin ([`retire`]). A file is deleted
    /// before the manifest stops naming it, so a crash between leaves a name the next reap
    /// finds gone.
    fn reap(&mut self) -> Result<u32, HistoryError> {
        if !self.reaping {
            return Ok(0);
        }
        let mut held = manifest::hold(&self.dir)?;
        if held.manifest().retired().files.is_empty() {
            self.reaping = false;
            return Ok(0);
        }
        let _span = tracing::info_span!(target: "blaze", "profile reap").entered();
        let pins = pin::pinned(&self.dir)?;
        let reapable = held.manifest().retired().reapable(&pins);
        if reapable.is_empty() {
            return Ok(0);
        }
        for &at in &reapable {
            let path = self.dir.join(&held.manifest().retired().files[at].0);
            match fs::remove_file(&path) {
                Ok(()) => {}
                // Gone already (a crash after the delete, before the commit), as it should be.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(io_error(&path, "deleting a retired file no reader pins")(error));
                }
            }
        }
        held.reaped(&reapable);
        held.commit()?;
        self.reaping = !held.manifest().retired().files.is_empty();
        Ok(u32::try_from(reapable.len()).unwrap_or(u32::MAX))
    }

    /// The build a flush's files are named for: the highest build any of its rows is of, and
    /// never below `build`, the build it is for. [`History::next_build`] numbers the next
    /// daemon's builds past every name, so a build whose rows only a timer's flush wrote (its
    /// daemon killed, or stopped on a stall, before its end was flushed) keeps its number. Until
    /// 2026-11-03 a timer's flush was named 0, and four daemons in one home each numbered a
    /// build 1, which `blaze slow` read as one build of 72 workers lasting 2296 s.
    fn named_for(&self, build: Option<u64>) -> u64 {
        let facts = [
            self.ended.iter().map(|it| it.build_id).max(),
            self.errors.iter().map(|it| it.build_id).max(),
            self.links.iter().map(|it| it.build_id).max(),
            self.crate_deps.iter().map(|it| it.build_id).max(),
            self.query_counts.iter().map(|it| it.build_id).max(),
            self.misses.iter().map(|it| it.build_id).max(),
            self.processes.iter().map(|it| it.build_id).max(),
            self.test_runs.iter().map(|it| it.build_id).max(),
            Some(self.client_build),
            self.key_parts.iter().map(|it| it.build_id).max(),
            self.provenance.iter().map(|it| it.build_id).max(),
        ];
        facts
            .into_iter()
            .flatten()
            .fold(build.unwrap_or(0).max(self.read.highest_build()), u64::max)
    }

    /// The flush's CPU samples as a `samples` batch, their frames named. A flush with no new
    /// samples names nothing.
    fn samples_batch(&self) -> Result<RecordBatch, ArrowError> {
        if self.samples.is_empty() {
            return Ok(RecordBatch::new_empty(batches::schema_of(&schema::SAMPLES)));
        }
        // Every address gets a frame, `[code in no image]` at worst.
        let frames =
            symbols::frames(self.samples.iter().map(|it| &it.pcs[..usize::from(it.depth)]));
        batches::samples(&self.samples, &frames, (Instant::now(), clock::mono_ns()), self.process)
    }

    /// Retires the oldest builds until the rest fit under the cap: a build's files go all at
    /// once, every table of it, never one table of a build, so a reader of a build reads all of
    /// it or none. The files of no build (named 0: flushes of an idle daemon) go one at a time.
    /// A build is as old as its newest file's flush; the newest flush's build is kept. Retired
    /// in one commit of the manifest: deleted once no pin holds them ([`History::reap`]). How
    /// many.
    fn trim(&mut self) -> Result<u32, HistoryError> {
        // The mappings and crash records take their room first; the Parquet files the rest.
        let cap = self.cap.saturating_sub(self.side);
        if self.total <= cap {
            return Ok(0);
        }
        let newest = self.kept.back().map_or(0, |it| it.name.last);
        // Each unit the cap deletes whole: (its newest flush, its build, the file of no build).
        // alloc: once a flush past the cap
        let mut units: std::collections::BTreeMap<(u64, Option<usize>), (u64, u64)> =
            std::collections::BTreeMap::new();
        // A file not yet listed is the next flush's to list ([`Kept::listed`]), not the cap's.
        for (at, kept) in self.kept.iter().enumerate().filter(|(_, it)| it.listed) {
            let key = (kept.name.build, (kept.name.build == 0).then_some(at));
            let unit = units.entry(key).or_insert((0, 0));
            unit.0 = unit.0.max(kept.name.last);
            unit.1 += kept.bytes;
        }
        // alloc: once a flush past the cap
        let mut order: Vec<(u64, (u64, Option<usize>), u64)> =
            units.into_iter().map(|(key, (last, bytes))| (last, key, bytes)).collect();
        order.sort_unstable();
        let mut total = self.total;
        // alloc: once a flush past the cap
        let mut gone: BTreeSet<(u64, Option<usize>)> = BTreeSet::new();
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
        if gone.is_empty() {
            return Ok(0);
        }
        let goes = |at: usize, kept: &Kept| {
            kept.listed && gone.contains(&(kept.name.build, (kept.name.build == 0).then_some(at)))
        };
        let mut held = manifest::hold(&self.dir)?;
        held.retire(
            self.kept
                .iter()
                .enumerate()
                .filter(|&(at, kept)| goes(at, kept))
                .map(|(_, kept)| name_of(&kept.path)),
        );
        self.reaping = true;
        held.commit()?;
        drop(held);
        let before = self.kept.len();
        let mut at = 0;
        let mut bytes = 0;
        self.kept.retain(|kept| {
            let retired = goes(at, kept);
            at += 1;
            if retired {
                bytes += kept.bytes;
            }
            !retired
        });
        self.total -= bytes;
        Ok(u32::try_from(before - self.kept.len()).unwrap_or(u32::MAX))
    }
}

impl Drop for History {
    /// Ends the drain thread and waits for it: it holds no rows of its own past a drain.
    fn drop(&mut self) {
        if let Some((stop, drainer)) = self.drainer.take() {
            drop(stop);
            if drainer.join().is_err() {
                tracing::error!(target: "blaze", "the profile history's drain thread (`blaze-drain`) of {} panicked", self.dir.display());
            }
        }
        // Every row written by the last flush: no postmortem is owed. One that failed leaves
        // the mapping for the next history to read.
        if self.persisted && self.clean {
            flight::postmortem::forget();
        }
        // What failed after a commit and no one took is said, never dropped unseen.
        for error in self.deferred.drain(..) {
            tracing::error!(target: "blaze", "{error}");
        }
    }
}

/// What the drain thread drained and the history has not flushed yet, and where it is in the
/// rings: one reader, so the drain thread and a flush take turns under one lock, held for a
/// read of the rings or a swap of the rows, never for a write.
#[derive(Default)]
struct Drained(Mutex<Draining>);

#[derive(Default)]
struct Draining {
    cursor: Cursor,
    read: Read,
    /// Rows the rings wrote over before a read took them, since the history opened: with the
    /// drain running (`late`, a thread outran [`DRAIN`]) and after a drain the backlog stopped
    /// (`behind`, the recorder outran the writer).
    late: u64,
    behind: u64,
    /// The last drain was stopped by a full backlog: what the next read finds written over is
    /// the writer's.
    stopped: bool,
}

impl Draining {
    /// Reads what is new in the rings into `read`, counting what was written over by why.
    fn read_now(&mut self) {
        let before = self.read.skipped;
        flight::read(&mut self.cursor, &mut self.read);
        let skipped = self.read.skipped - before;
        if std::mem::take(&mut self.stopped) {
            self.behind += skipped;
        } else {
            self.late += skipped;
        }
    }
}

impl Drained {
    /// Reads the rings up to now and swaps what was drained into `into` (which is clear),
    /// with where the drain is now into `cursor`: the rows skipped so far, late and behind.
    fn take(&self, into: &mut Read, cursor: &mut Cursor) -> (u64, u64) {
        let mut at = self.0.lock();
        at.read_now();
        std::mem::swap(&mut at.read, into);
        cursor.clone_from(&at.cursor);
        (at.late, at.behind)
    }
}

/// The drain thread: a read of the rings every [`DRAIN`] until `stop` is dropped, telling
/// `full` each time the rows not yet flushed pass [`FLUSH_ROWS`]. Past [`BACKLOG_ROWS`] it
/// reads nothing until a flush takes them, and what the rings write over meanwhile is counted
/// as behind.
fn drain(drained: &Drained, stop: &Receiver<()>, full: &Sender<()>) {
    // Each read is due a [`DRAIN`] after the last began, not after it ended: a read of busy
    // rings takes a while, and the rings fill meanwhile.
    let mut due = Instant::now() + DRAIN;
    loop {
        match stop.recv_deadline(due) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }
        due = Instant::now() + DRAIN;
        let _span = tracing::info_span!(target: "blaze", "profile drain").entered();
        let mut at = drained.0.lock();
        if at.read.rows() >= BACKLOG_ROWS {
            at.stopped = true;
        } else {
            at.read_now();
        }
        let rows = at.read.rows();
        drop(at);
        if rows >= FLUSH_ROWS {
            _ = full.try_send(());
        }
    }
}

/// The batches of `table` in `bytes`, an Arrow IPC stream build `build`'s client sent (none for
/// no bytes), each made again under the table's own schema so it writes beside the daemon's,
/// and refused, naming why, when a column does not fit or a row is of another build.
fn client_batches(
    build: u64,
    table: &'static Table,
    bytes: &[u8],
) -> Result<Vec<RecordBatch>, HistoryError> {
    let refused = |why: String| HistoryError::Client { build, table: table.name, why };
    if bytes.is_empty() {
        return Ok(Vec::new()); // alloc: empty, allocates nothing
    }
    let schema = batches::schema_of(table);
    let at = table
        .column("build_id")
        .unwrap_or_else(|| unreachable!("{} has a build_id column", table.name));
    let mut out = crate::live::decode(bytes)
        .map_err(|error| refused(format!("not an Arrow IPC stream: {error}")))?;
    for batch in &mut out {
        // clone: the columns' `Arc`s, once a batch
        *batch = RecordBatch::try_new(schema.clone(), batch.columns().to_vec())
            .map_err(|error| refused(format!("not of the table's schema: {error}")))?;
        let builds = batch.column(at).as_any().downcast_ref::<UInt64Array>();
        let Some(builds) = builds else {
            return Err(refused("its build_id column is not u64".to_owned()));
        };
        if let Some(other) = builds.iter().find(|it| *it != Some(build)) {
            return Err(refused(format!(
                "a row of build {other:?}, not of the build it was sent for"
            )));
        }
    }
    Ok(out)
}

/// For a reader of the history `dir` with no daemon writing it (`blaze slow`): writes the last
/// rows of every daemon that died before it flushed them, as [`History::open`] does, each file
/// listed in the manifest ([`write_recovered`]). Nothing while a daemon holds the history
/// ([`OWNER`]): it did so as it opened, and numbers its own files. The files written, counted.
pub fn recover_dead(dir: &Path) -> Result<usize, HistoryError> {
    let path = dir.join(OWNER);
    let file = match fs::OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        // No daemon ever wrote here, so none left a mapping.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(io_error(&path, "opening the owner file of")(error)),
    };
    // Held while the files are written, so no daemon starts numbering files meanwhile: one
    // that starts in these seconds sees the owner file held and its pid dead, and waits for
    // this to let go (`blaze serve`'s claim, at most [`RECOVERY_WAIT`]).
    let _owner = match stdx::file_lock::Locked::try_lock(file)
        .map_err(io_error(&path, "locking the owner file of"))?
    {
        Ok(owner) => owner,
        Err(_held) => return Ok(0),
    };
    // The next flush number is past every file the manifest lists and its last flush, as
    // `History::open` numbers it: an unlisted file is a crash's garbage the next open deletes.
    let listed = manifest::read(dir)?;
    let mut seq = listed
        .all()
        .filter_map(Name::parse)
        .map(|it| it.last)
        .max()
        .unwrap_or(0)
        .max(listed.flushed())
        + 1;
    let (written, failed) = recover_into(dir, &mut Writer::new(), &mut seq);
    match failed {
        Some(error) => Err(error),
        None => Ok(written.len()),
    }
}

/// Writes the rows each dead daemon's flight recorder mapping in `dir` holds past its last
/// flush ([`flight::postmortem`]), one flush number `seq` a daemon, and deletes each mapping
/// once its rows are written and listed: the files written, and the first error. A mapping that
/// can't be read (another layout, a damaged file) stays, and the error names it; the others are
/// read.
fn recover_into(
    dir: &Path,
    writer: &mut Writer,
    seq: &mut u64,
) -> (Vec<Kept>, Option<HistoryError>) {
    // alloc: once a history, a file a table of a dead daemon
    let mut written = Vec::new();
    let found = match flight::postmortem::dead(dir) {
        Ok(found) => found,
        Err(error) => return (written, Some(HistoryError::Postmortem(error))),
    };
    let mut failed = None;
    for found in found {
        match recover_one(dir, writer, seq, &found) {
            Ok(files) => {
                written.extend(files);
                if let Err(error) = found.remove() {
                    failed.get_or_insert(HistoryError::Postmortem(error));
                }
            }
            Err(error) => _ = failed.get_or_insert(error),
        }
    }
    (written, failed)
}

/// Writes the rows of the dead daemon's mapping `found` (it holds the mapping's lock) as files
/// of flush `seq`, listed in the commit that names the mapping recovered: the files written.
/// A mapping the manifest names recovered already (a crash came between that commit and the
/// mapping's delete) is not read again: no files.
fn recover_one(
    dir: &Path,
    writer: &mut Writer,
    seq: &mut u64,
    found: &flight::postmortem::Found,
) -> Result<Vec<Kept>, HistoryError> {
    let nonce = found.nonce().map_err(HistoryError::Postmortem)?;
    // Read with the mapping's lock held, so no other reader of it commits meanwhile.
    let listed = match nonce {
        Some(nonce) => manifest::read(dir)?.recorder(found.pid(), nonce),
        None => None,
    };
    if listed.is_some_and(|it| it.recovered) {
        // alloc: empty, allocates nothing
        return Ok(Vec::new());
    }
    // Each read holds the postmortem's names until it drops: one at a time.
    let recovered =
        found.read(listed.map(|it| it.flushed)).map_err(HistoryError::Postmortem)?;
    let files = write_recovered(dir, writer, *seq, &recovered)?;
    *seq += 1;
    Ok(files)
}

/// The rows of `recovered`, a dead daemon's, as files of flush `seq`, written through
/// `writer` as a flush writes its own: of the dead process (its pid and name), and a `builds`
/// row `crashed` for each build it was running, from its start to its last row read back.
fn write_recovered(
    dir: &Path,
    writer: &mut Writer,
    seq: u64,
    recovered: &flight::postmortem::Recovered,
) -> Result<Vec<Kept>, HistoryError> {
    let process = Process { pid: recovered.pid, name: &recovered.process, commit: None };
    let read = &recovered.read;
    // alloc: a row a build the dead daemon was running, once a postmortem
    let crashed: Vec<BuildRow> = recovered
        .builds
        .iter()
        .map(|build| BuildRow {
            build_id: build.build_id,
            // clone: a build's texts, once a postmortem
            commit: build.commit.clone(),
            workspace: build.workspace.clone(),
            command: build.command.clone(),
            started_ns: build.started_ns,
            dur_ns: Some(recovered.last_ns.max(build.started_ns) - build.started_ns),
            started_unix_ns: build.started_unix_ns,
            outcome: Outcome::Crashed,
            hits: None,
            misses: None,
            joined: None,
            tree: None,
            // clone: a hash, once a build
            blaze: (!recovered.blaze.is_empty()).then(|| recovered.blaze.clone()),
            client_ns: None,
            daemon_cpu_ns: None,
        })
        .collect();
    let tables: [(&'static Table, RecordBatch); 7] = [
        (
            &schema::SPANS,
            batches::spans(&read.spans, process).map_err(arrow_error(&schema::SPANS))?,
        ),
        (
            &schema::COUNTERS,
            batches::counters(&read.counters, process).map_err(arrow_error(&schema::COUNTERS))?,
        ),
        (&schema::BUILDS, builds(&crashed, recovered.pid).map_err(arrow_error(&schema::BUILDS))?),
        (
            &schema::SWITCHES,
            batches::switches(&read.switches, process).map_err(arrow_error(&schema::SWITCHES))?,
        ),
        (
            &schema::EVENTS,
            batches::events(&read.events, process).map_err(arrow_error(&schema::EVENTS))?,
        ),
        (
            &schema::EVENT_FIELDS,
            batches::event_fields(&read.fields, process)
                .map_err(arrow_error(&schema::EVENT_FIELDS))?,
        ),
        (
            &schema::FOLDED,
            batches::folded(&read.folds, process).map_err(arrow_error(&schema::FOLDED))?,
        ),
    ];
    let build = read.highest_build().max(crashed.iter().map(|it| it.build_id).max().unwrap_or(0));
    // alloc: once a postmortem
    let mut kept = Vec::new();
    for (table, batch) in tables {
        if batch.num_rows() == 0 {
            continue;
        }
        let name = Name { build, table: table_at(table), first: seq, last: seq, level: 0 };
        let path = dir.join(name.file_name());
        let parts = std::slice::from_ref(&batch);
        let bytes = writer.write(&path, table, parts)?;
        let mut entry = manifest::Entry::of_batches(table, parts);
        // The dead daemon's rows: its pid, and a boot no longer known (the machine may have
        // restarted since), so a build-scoped read matches them on any boot.
        entry.pid = Some(recovered.pid);
        entry.boot = None;
        // Not this history's to merge: of another process, as a file there before it opened.
        kept.push(Kept { name, bytes, path, entry, ours: false, listed: true });
    }
    // Listed in one commit once all are written, which names the mapping recovered, before
    // the caller deletes it: a crash before the commit leaves unlisted files the next open
    // deletes and the mapping it reads again; one after it, a mapping the next open deletes
    // unread. Never a row twice.
    let mut held = manifest::hold(dir)?;
    for file in &kept {
        // clone: a file's entry, once a file
        held.add(name_of(&file.path), file.entry.clone())?;
    }
    if recovered.nonce != 0 {
        held.recorder_recovered(recovered.pid, recovered.nonce);
    }
    held.commit()?;
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use arrow_array::{Array, cast::AsArray};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    use super::*;
    use crate::flight::{Allocated, FrameCopied};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-history-{}-{name}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read_back(path: &Path) -> Vec<RecordBatch> {
        let file = fs::File::open(path).unwrap();
        ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .build()
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn same(written: &RecordBatch, read: &[RecordBatch]) {
        assert_eq!(read.len(), 1, "a small table reads back as one batch");
        let read = &read[0];
        assert_eq!(read.num_rows(), written.num_rows());
        assert_eq!(
            read.schema().fields(),
            written.schema().fields(),
            "the Arrow types come back, dictionaries too"
        );
        for (i, field) in written.schema().fields().iter().enumerate() {
            assert!(
                read.column(i).as_ref() == written.column(i).as_ref(),
                "{} reads back the same",
                field.name()
            );
        }
    }

    fn row(build_id: u64, outcome: Outcome) -> BuildRow {
        BuildRow {
            build_id,
            commit: Some("0123abc".into()),
            workspace: "/work/app".into(),
            command: "build --release".into(),
            started_ns: 1_000 * build_id as i64,
            dur_ns: Some(25),
            started_unix_ns: None,
            outcome,
            hits: Some(10),
            misses: Some(2),
            joined: None,
            tree: Some("7".repeat(64)),
            blaze: Some("b".repeat(64)),
            client_ns: None,
            daemon_cpu_ns: None,
        }
    }

    #[test]
    fn a_child_process_row_and_a_test_run_row_read_back_column_for_column() {
        use crate::facts::{ProcessRow, TestOutcome, TestRunRow};
        let dir = scratch("processes-test-runs");
        let mut history = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        let child = ProcessRow {
            build_id: 71,
            pid: 4242,
            kind: "build_script",
            unit: Some("cc v1.2.0 (build script run)".into()),
            program: "/target/debug/build/cc-1/build-script-build".into(),
            parent_span: 0x1_0000_0007,
            spawned_ns: 1_000,
            exec_ns: Some(1_400),
            exited_ns: 90_000,
            sandbox_ns: Some(300),
            user_ns: 50_000,
            system_ns: 9_000,
            peak_rss: Some(12 << 20),
            minor_faults: Some(800),
            major_faults: Some(2),
            voluntary_switches: Some(31),
            involuntary_switches: Some(4),
            code: Some(0),
            signal: None,
        };
        // A child no spawner labelled, killed by a signal: no build, no span, no code.
        let unlabelled = ProcessRow {
            build_id: 0,
            kind: "unlabelled",
            unit: None,
            parent_span: 0,
            sandbox_ns: None,
            code: None,
            signal: Some(9),
            ..child.clone()
        };
        let test = TestRunRow {
            build_id: 71,
            test: "parse::tests::a_number".into(),
            krate: Some("app".into()),
            binary: "app-0123abcd".into(),
            outcome: TestOutcome::Passed,
            dur_ns: 3_000_000,
            retries: 1,
            ended_ns: 95_000,
        };
        history.processes_reaped([child.clone(), unlabelled.clone()]);
        history.tests_ran([test.clone()]);
        let flushed = history.flush(Some(71)).unwrap();
        assert_eq!((flushed.processes, flushed.test_runs), (2, 1));
        let file =
            |table: &str| dir.join(format!("{}-{:08}.{table}.parquet", flushed.build, flushed.seq));
        let pid = Process::this().pid;
        same(&facts::processes(&[child, unlabelled], pid).unwrap(), &read_back(&file("processes")));
        same(&facts::test_runs(&[test]).unwrap(), &read_back(&file("test_runs")));
        let read = read_back(&file("processes"));
        let builds = read[0].column(schema::PROCESSES.column("build_id").unwrap());
        assert!(!builds.is_null(0) && builds.is_null(1), "a child of no build has a null build_id");

        let again = history.flush(Some(72)).unwrap();
        assert_eq!((again.processes, again.test_runs), (0, 0), "a flush writes each row once");
        _ = fs::remove_dir_all(&dir);
    }

    /// A client's span of `build`, its columns otherwise none.
    fn client_span(build: u32) -> SpanRow {
        SpanRow {
            build,
            thread: 1,
            thread_name: 0,
            span_id: 5,
            parent: 0,
            depth: 0,
            site: 0,
            start_ns: 600,
            dur_ns: 80_000,
            alloc: None,
            copied: None,
            krate: 0,
            function: 0,
            unit: 0,
            self_cpu_ns: None,
            typed: crate::span::Said::EMPTY.typed,
            folded_spans: 0,
            folded_ns: 0,
        }
    }

    #[test]
    fn a_clients_rows_are_written_with_its_builds_row_given_its_times() {
        let dir = scratch("client-rows");
        let mut history = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        history.build_ended(BuildRow { daemon_cpu_ns: Some((7, 3)), ..row(81, Outcome::Ok) });
        history.flush(Some(81)).unwrap();
        let span = client_span(81);
        let spans =
            batches::spans(&[span], Process { pid: 9, name: "client", commit: None }).unwrap();
        let mut bytes = Vec::new();
        crate::live::encode(&spans, &mut bytes).unwrap();
        history
            .client_rows(ClientRows {
                build: 81,
                started_ns: 500,
                ended_ns: 90_000,
                spans: bytes,
                ..ClientRows::default()
            })
            .unwrap();
        let flushed = history.flush(None).unwrap();
        assert_eq!((flushed.builds, flushed.client, flushed.build), (1, 1, 81));
        let read = read_back(&dir.join(format!("81-{:08}.builds.parquet", flushed.seq)));
        let int = |name: &str| {
            read[0]
                .column(schema::BUILDS.column(name).unwrap())
                .as_primitive::<arrow_array::types::Int64Type>()
                .value(0)
        };
        assert_eq!((int("client_started_ns"), int("client_ended_ns")), (500, 90_000));
        assert_eq!((int("daemon_user_ns"), int("daemon_system_ns")), (7, 3));

        let mut other = Vec::new();
        let span = client_span(82);
        crate::live::encode(
            &batches::spans(&[span], Process { pid: 9, name: "client", commit: None }).unwrap(),
            &mut other,
        )
        .unwrap();
        let refused =
            history.client_rows(ClientRows { build: 81, spans: other, ..ClientRows::default() });
        assert!(
            matches!(refused, Err(HistoryError::Client { build: 81, table: "spans", .. })),
            "{refused:?}"
        );
        _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flushed_table_reads_back_as_the_same_rows() {
        let dir = scratch("round-trip");
        let span = |i: u64| SpanRow {
            build: (i % 3) as u32,
            thread: 7,
            thread_name: 0,
            span_id: 100 + i,
            parent: if i == 0 { 0 } else { 100 },
            depth: (i != 0) as u16,
            site: 0,
            start_ns: 1_000 + i as i64,
            dur_ns: 50 - i as i64,
            alloc: (i % 2 == 0).then_some(Allocated { bytes: 64 * i, count: i }),
            copied: (i % 3 == 0).then_some(FrameCopied { copies: i, bytes: 1024 * i }),
            krate: 0,
            function: 0,
            unit: 0,
            self_cpu_ns: (i % 2 == 1).then_some(10 * i),
            typed: {
                use crate::span::{Column as _, Said, Value as _, col};
                let mut said = Said::EMPTY;
                said.set(col::Owner::AT, (i % 4 == 0).then_some(i).word());
                said.set(col::Steps::AT, (i % 5 == 0).then_some(1000 + i).word());
                said.set(col::PeakBytes::AT, (i % 5 == 0).then_some(4096 * i).word());
                said.typed
            },
            folded_spans: 0,
            folded_ns: 0,
        };
        let spans: Vec<SpanRow> = (0..40).map(span).collect();
        let process = Process { pid: 9, name: "blaze test", commit: Some("abc") };
        let spans = batches::spans(&spans, process).unwrap();
        let counters: Vec<CounterRow> = (0..10)
            .map(|i| CounterRow {
                build: 1,
                plot: Plot::ALL[i % Plot::ALL.len()],
                t_ns: i as i64,
                value: i as u64 * 3,
                fiber: None,
            })
            .collect();
        let counters = batches::counters(&counters, process).unwrap();
        let builds = builds(&[row(1, Outcome::Ok), row(2, Outcome::Stalled)], 7).unwrap();

        let mut writer = Writer::new();
        for (table, batch) in
            [(&schema::SPANS, &spans), (&schema::COUNTERS, &counters), (&schema::BUILDS, &builds)]
        {
            let path = dir.join(format!("1-00000001.{}.parquet", table.name));
            let bytes = writer.write(&path, table, std::slice::from_ref(batch)).unwrap();
            assert_eq!(bytes, fs::metadata(&path).unwrap().len());
            let read = read_back(&path);
            same(batch, &read);
            // arrow-rs gives the batches it reads no schema metadata; the file's schema has it.
            let file =
                ParquetRecordBatchReaderBuilder::try_new(fs::File::open(&path).unwrap()).unwrap();
            assert_eq!(file.schema().metadata()["blaze.table"], table.name);
        }
        let read = read_back(&dir.join("1-00000001.builds.parquet"));
        let outcome =
            read[0].column(schema::BUILDS.column("outcome").unwrap()).as_dictionary::<UInt32Type>();
        let names = outcome.values().as_string::<i32>();
        assert_eq!(names.value(outcome.keys().value(1) as usize), "stalled");
        assert!(
            fs::read_dir(&dir).unwrap().all(|it| !it
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );
    }

    /// Each flush says the process's reported bugs: the counter with each build's flush and
    /// whenever it grew, and a `blaze-bug` row for each site that reported since the last,
    /// naming the value kind, the site and how many more times.
    #[test]
    fn a_merge_says_the_rows_its_inputs_held_and_holds_them() {
        let dir = scratch("merged-rows");
        let process = Process { pid: 9, name: "blaze test", commit: None };
        let counters = |n: u64| {
            let rows: Vec<CounterRow> = (0..n)
                .map(|i| CounterRow {
                    build: 1,
                    plot: Plot::MemoHits,
                    t_ns: i as i64,
                    value: i,
                    fiber: None,
                })
                .collect();
            batches::counters(&rows, process).unwrap()
        };
        let mut writer = Writer::new();
        let (a, b) =
            (dir.join("1-00000001.counters.parquet"), dir.join("1-00000002.counters.parquet"));
        writer.write(&a, &schema::COUNTERS, &[counters(3)]).unwrap();
        writer.write(&b, &schema::COUNTERS, &[counters(4)]).unwrap();
        let table = table_at(&schema::COUNTERS);
        let footer = |path: &Path| {
            let metadata = ParquetMetaDataReader::new()
                .parse_and_finish(&fs::File::open(path).unwrap())
                .unwrap();
            let said: Vec<Option<String>> = metadata
                .file_metadata()
                .key_value_metadata()
                .into_iter()
                .flatten()
                .filter(|it| it.key == MERGED_ROWS)
                .map(|it| it.value.clone())
                .collect();
            (metadata.file_metadata().num_rows(), said)
        };
        let merged = dir.join("1-00000001-00000002-1.counters.parquet");
        concat(&merged, &[a.clone(), b], writer.properties(table)).unwrap();
        assert_eq!(footer(&merged), (7, vec![Some("7".to_owned())]));
        // A merge of a merged file says its own count, once.
        let again = dir.join("1-00000001-00000002-2.counters.parquet");
        concat(&again, &[merged, a], writer.properties(table)).unwrap();
        assert_eq!(footer(&again), (10, vec![Some("10".to_owned())]));
        _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flush_says_each_site_that_reported_a_bug_since_the_last() {
        use stdx::assert::reported::Site;
        let dir = scratch("bugs");
        let mut history = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        let at = std::panic::Location::caller();
        let site = |count| Site {
            kind: "AdtDef",
            what: "formatted with no database attached",
            at,
            count,
            thread: Some(Arc::from("driver")),
            backtrace: None,
        };
        let said = |history: &mut History| {
            let counters: Vec<(u32, u64)> = history
                .read
                .counters
                .drain(..)
                .filter(|it| it.plot == Plot::BlazeBugs)
                .map(|it| (it.build, it.value))
                .collect();
            let errors: Vec<ErrorRow> = history.errors.drain(..).collect();
            (counters, errors)
        };
        history.told_bugs(Some(3), 10, 0, Vec::new);
        assert_eq!(said(&mut history), (vec![(3, 0)], Vec::new()), "a build's flush says 0");
        history.told_bugs(None, 20, 2, || vec![site(2)]);
        let (counters, errors) = said(&mut history);
        assert_eq!(counters, [(0, 2)]);
        let [row] = &errors[..] else { panic!("one row: {errors:?}") };
        assert_eq!(
            (
                row.kind.as_str(),
                row.function.as_deref(),
                row.message.as_str(),
                row.detail.as_deref()
            ),
            (
                "blaze-bug",
                Some("AdtDef"),
                "a blaze bug: `AdtDef` formatted with no database attached, 2 more times",
                Some("first on thread `driver`")
            )
        );
        assert_eq!(
            (row.file.as_deref(), row.line, row.t_ns),
            (Some(at.file()), Some(at.line()), Some(20))
        );
        history.told_bugs(None, 30, 2, || panic!("the sites are read only when the total grew"));
        assert_eq!(
            said(&mut history),
            (Vec::new(), Vec::new()),
            "a timer's flush with nothing new says nothing"
        );
        history.told_bugs(None, 40, 3, || vec![site(3)]);
        let (counters, errors) = said(&mut history);
        assert_eq!(counters, [(0, 3)]);
        assert_eq!(
            errors.iter().map(|it| it.message.as_str()).collect::<Vec<_>>(),
            ["a blaze bug: `AdtDef` formatted with no database attached, 1 more time"]
        );
    }

    #[test]
    fn a_builds_row_names_the_tree_it_built_and_the_blaze_that_ran_it() {
        let dir = scratch("identity");
        let workspace = dir.join("work");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
        let mut history =
            History::open(&dir.join("profile"), CAP_BYTES, sampler::unsampled()).unwrap();
        let built = |history: &mut History, build_id| {
            let row = BuildRow {
                workspace: workspace.to_str().unwrap().into(),
                tree: None,
                blaze: None,
                ..row(build_id, Outcome::Ok)
            };
            history.build_ended(row);
            let flushed = history.flush(Some(build_id)).unwrap();
            let read = read_back(
                &dir.join(format!("profile/{}-{:08}.builds.parquet", flushed.build, flushed.seq)),
            );
            let string = |name: &str| {
                read[0]
                    .column(schema::BUILDS.column(name).unwrap())
                    .as_string::<i32>()
                    .value(0)
                    .to_owned()
            };
            (string("tree"), string("blaze"))
        };
        let (tree, blaze) = built(&mut history, 51);
        assert_eq!(tree, identity::tree_digest(&workspace).unwrap());
        assert_eq!(blaze, identity::binary().unwrap());

        fs::write(workspace.join("src/main.rs"), "fn main() { }\n").unwrap();
        let (edited, same_blaze) = built(&mut history, 52);
        assert_ne!(edited, tree, "a build of an edited tree names another tree");
        assert_eq!(same_blaze, blaze);
        _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_images_a_build_linked_and_its_crate_graph_are_written_at_the_next_flush() {
        use crate::facts::{CrateDepRow, DepKind, LinkOutcome, LinkRow};
        let dir = scratch("facts");
        let mut history = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        history.linked([LinkRow {
            build_id: 61,
            image: "app (bin)".into(),
            krate: Some("app".into()),
            outcome: LinkOutcome::Patched,
            dur_ns: Some(7),
        }]);
        history.crate_graph([
            CrateDepRow {
                build_id: 61,
                krate: "app".into(),
                dep: "serde".into(),
                kind: DepKind::Normal,
            },
            CrateDepRow {
                build_id: 61,
                krate: "app".into(),
                dep: "cc".into(),
                kind: DepKind::Build,
            },
        ]);
        let flushed = history.flush(Some(61)).unwrap();
        assert_eq!((flushed.links, flushed.crate_deps), (1, 2));
        let links =
            read_back(&dir.join(format!("{}-{:08}.links.parquet", flushed.build, flushed.seq)));
        let image =
            links[0].column(schema::LINKS.column("image").unwrap()).as_string::<i32>().value(0);
        assert_eq!(image, "app (bin)");
        let deps = read_back(
            &dir.join(format!("{}-{:08}.crate_deps.parquet", flushed.build, flushed.seq)),
        );
        assert_eq!(deps.iter().map(RecordBatch::num_rows).sum::<usize>(), 2);

        let again = history.flush(Some(62)).unwrap();
        assert_eq!((again.links, again.crate_deps), (0, 0), "a flush writes each row once");
        _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_builds_query_counts_are_written_at_the_next_flush_column_for_column() {
        use arrow_array::types::{UInt32Type, UInt64Type};

        use crate::facts::QueryCountRow;
        let dir = scratch("query-counts");
        let mut history = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        let mir = QueryCountRow {
            build_id: 63,
            query: "mir_body_query",
            executed: 6384,
            verified: 10,
            reran: 2,
            evicted_asked: 3,
            evicted: 4,
            dropped: 5,
            reads_recorded: 6,
            reads_kept: 7,
            reads_most: 8,
            written: 9,
            ran_new: 10,
            ran_changed: 11,
            ran_untracked: 12,
            ran_cycle: 13,
        };
        history.query_counted([
            mir,
            QueryCountRow { query: "infer_body_query", executed: 7566, ..mir },
        ]);
        let flushed = history.flush(Some(63)).unwrap();
        assert_eq!(flushed.query_counts, 2);
        let read = read_back(
            &dir.join(format!("{}-{:08}.query_counts.parquet", flushed.build, flushed.seq)),
        );
        let batch = &read[0];
        let count = |name: &str, row: usize| {
            batch
                .column(schema::QUERY_COUNTS.column(name).unwrap())
                .as_primitive::<UInt64Type>()
                .value(row)
        };
        let query = batch
            .column(schema::QUERY_COUNTS.column("query").unwrap())
            .as_dictionary::<UInt32Type>();
        let names = query.values().as_string::<i32>();
        assert_eq!(names.value(query.keys().value(1) as usize), "infer_body_query");
        assert_eq!(
            [
                "build_id",
                "executed",
                "verified",
                "reran",
                "evicted_asked",
                "evicted",
                "dropped",
                "reads_recorded",
                "reads_kept",
                "reads_most",
                "written",
                "ran_new",
                "ran_changed",
                "ran_untracked",
                "ran_cycle"
            ]
            .map(|it| count(it, 0)),
            [63, 6384, 10, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13],
            "each count in its own column"
        );
        assert_eq!(count("executed", 1), 7566);

        let again = history.flush(Some(64)).unwrap();
        assert_eq!(again.query_counts, 0, "a flush writes each row once");
        _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flush_writes_the_recorders_new_rows_and_the_builds_that_ended() {
        crate::install();
        std::thread::spawn(|| {
            let _build = flight::building(41);
            let _span = tracing::info_span!(target: "blaze", "history flushed").entered();
        })
        .join()
        .unwrap();
        let dir = scratch("flush");
        let mut history = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        for table in schema::TABLES {
            let path = dir.join(format!("schema.{}.parquet", table.name));
            let read = read_back(&path);
            assert!(read.iter().all(|it| it.num_rows() == 0), "{} holds no rows", path.display());
        }
        history.build_ended(row(41, Outcome::Ok));
        let flushed = history.flush(Some(41)).unwrap();
        assert!(flushed.spans >= 1 && flushed.builds == 1 && flushed.files >= 2, "{flushed:?}");
        let path = dir.join(format!("{}-{:08}.spans.parquet", flushed.build, flushed.seq));
        let spans = read_back(&path);
        let found = spans.iter().any(|batch| {
            let names =
                batch.column(schema::SPANS.column("name").unwrap()).as_dictionary::<UInt32Type>();
            let commits =
                batch.column(schema::SPANS.column("commit").unwrap()).as_dictionary::<UInt32Type>();
            (0..batch.num_rows()).any(|row| {
                let name =
                    names.values().as_string::<i32>().value(names.keys().value(row) as usize);
                let commit = commits.keys().is_valid(row).then(|| {
                    commits.values().as_string::<i32>().value(commits.keys().value(row) as usize)
                });
                name == "history flushed" && commit == Some("0123abc")
            })
        });
        assert!(found, "the span is in the build's file, with the build's commit");
        let builds =
            read_back(&dir.join(format!("{}-{:08}.builds.parquet", flushed.build, flushed.seq)));
        assert_eq!(builds[0].num_rows(), 1);

        let again = history.flush(None).unwrap();
        assert_eq!(again.builds, 0, "a build's row is written once");
        assert!(again.seq > flushed.seq);
    }

    /// A thread writes three rings' worth of rows between two flushes, a burst a drain: the
    /// history's own drain thread reads them as they come, so the flush has every one and
    /// skipped none of them (before 2026-11-02 the rings were read only by the loop that
    /// flushed, and a self-build skipped 42 M rows while it flushed).
    #[test]
    fn rows_written_between_flushes_past_a_rings_size_are_all_flushed() {
        crate::install();
        let dir = scratch("drained");
        let mut history = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        let bursts = 24;
        let burst = flight::CAP / 8;
        let tid = std::thread::spawn(move || {
            let _build = flight::building(43);
            for _ in 0..bursts {
                for _ in 0..burst {
                    let _span = tracing::info_span!(target: "blaze", "history drained").entered();
                }
                std::thread::sleep(3 * DRAIN);
            }
            crate::sampler::os_tid() as u32
        })
        .join()
        .unwrap();
        history.build_ended(row(43, Outcome::Ok));
        let flushed = history.flush(Some(43)).unwrap();
        let spans =
            read_back(&dir.join(format!("{}-{:08}.spans.parquet", flushed.build, flushed.seq)));
        let ours: usize = spans
            .iter()
            .map(|batch| {
                let names = batch
                    .column(schema::SPANS.column("name").unwrap())
                    .as_dictionary::<UInt32Type>();
                let threads = batch
                    .column(schema::SPANS.column("thread").unwrap())
                    .as_primitive::<UInt32Type>();
                (0..batch.num_rows())
                    .filter(|&row| {
                        threads.value(row) == tid
                            && names
                                .values()
                                .as_string::<i32>()
                                .value(names.keys().value(row) as usize)
                                == "history drained"
                    })
                    .count()
            })
            .sum();
        assert_eq!(
            ours,
            bursts * burst,
            "{} rows written, {} rows a ring",
            bursts * burst,
            flight::CAP
        );
    }

    #[test]
    fn a_history_opened_again_numbers_on_from_its_files() {
        let dir = scratch("reopen");
        let mut history = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        history.build_ended(row(1, Outcome::Failed));
        let first = history.flush(Some(1)).unwrap();
        drop(history);
        fs::write(dir.join(".1-00000099.spans.parquet.tmp"), b"half").unwrap();
        let mut history = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        assert!(
            !dir.join(".1-00000099.spans.parquet.tmp").exists(),
            "a half-written file is deleted"
        );
        // The first flush takes what the rings hold, other tests' builds' rows among them.
        assert!(first.build >= 1, "{first:?}");
        assert_eq!(
            u64::from(history.next_build()),
            first.build + 1,
            "a restarted daemon numbers its builds past the history's"
        );
        let next = u64::from(history.next_build());
        history.build_ended(row(next, Outcome::Ok));
        let second = history.flush(Some(next)).unwrap();
        assert!(second.seq > first.seq, "{first:?} then {second:?}");
        assert!(dir.join(format!("{}-{:08}.builds.parquet", first.build, first.seq)).exists());
        assert!(dir.join(format!("{}-{:08}.builds.parquet", second.build, second.seq)).exists());
    }

    /// Two daemons in one home, one after the other: the first is stopped while its build runs,
    /// so only the timer's flush wrote that build's rows. The second numbers its builds past
    /// it, so the two daemons' builds never share a number (before 2026-11-03 that flush was
    /// named 0, the second daemon numbered its first build as the first's, and `blaze slow`
    /// merged them into one build of both daemons' workers).
    #[test]
    fn a_build_only_a_timers_flush_wrote_is_numbered_past_by_the_next_daemon() {
        crate::install();
        // Past any build another test of this process writes rows of.
        const KILLED: u32 = 7_000_001;
        let dir = scratch("killed-mid-build");
        let mut first = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        std::thread::spawn(|| {
            let _build = flight::building(KILLED);
            let _span = tracing::info_span!(target: "blaze", "history killed mid-build").entered();
        })
        .join()
        .unwrap();
        let timer = first.flush(None).unwrap();
        assert!(timer.spans >= 1, "{timer:?}");
        assert!(
            timer.build >= u64::from(KILLED),
            "the timer's flush is named for the build its rows are of: {timer:?}"
        );
        drop(first);
        let second = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        assert!(
            second.next_build() > KILLED,
            "the next daemon's first build is {}, a number the stopped daemon's rows already hold",
            second.next_build()
        );
        _ = fs::remove_dir_all(&dir);
    }

    /// A flush with no new CPU samples asks the symbolizer nothing. The history reads a ring of
    /// the test's own: until 2026-11-03 it read the process's ring, and a run of this test saw
    /// 8192 samples in each of its two flushes. A sample kept into its ring is the next
    /// flush's, so the flushes read that ring and no other.
    #[test]
    fn a_flush_with_no_new_samples_names_nothing() {
        static OWN: sampler::Ring = sampler::Ring::new();
        let dir = scratch("idle");
        let mut history = History::open(&dir, CAP_BYTES, &OWN).unwrap();
        let asked = symbols::ASKED.with(std::cell::Cell::get);
        let first = history.flush(None).unwrap();
        let idle = history.flush(None).unwrap();
        assert_eq!((first.samples, idle.samples), (0, 0), "the test's ring holds no samples");
        assert_eq!(
            symbols::ASKED.with(std::cell::Cell::get),
            asked,
            "a flush with no samples asked for names"
        );
        let sample = Sample {
            at: Instant::now(),
            tid: 1,
            depth: 0,
            pcs: [0; sampler::DEPTH],
            span: 0,
            build: 0,
            on_cpu: true,
        };
        OWN.keep(&[sample]);
        assert_eq!(
            history.flush(None).unwrap().samples,
            1,
            "the sample kept into the history's ring"
        );
        _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn past_the_cap_the_oldest_files_go_first() {
        let dir = scratch("cap");
        let mut history = History::open(&dir, 1, sampler::unsampled()).unwrap();
        let mut flushes = Vec::new();
        for build in 1..=3 {
            history.build_ended(row(build, Outcome::Ok));
            flushes.push(history.flush(Some(build)).unwrap());
        }
        let name = |it: &Flushed| dir.join(format!("{}-{:08}.builds.parquet", it.build, it.seq));
        assert!(
            !name(&flushes[0]).exists() && !name(&flushes[1]).exists(),
            "the older builds' files are deleted"
        );
        assert!(
            name(&flushes[2]).exists(),
            "the newest flush's files stay, though they are past the cap alone"
        );
        assert!(dir.join("schema.spans.parquet").exists(), "the schema files are not the cap's");
        assert!(
            history.kept.iter().all(|it| it.name.last == flushes[2].seq),
            "only the newest flush is kept"
        );
    }

    /// The listable files in `dir`, by name: what is on disk beside the manifest.
    fn on_disk(dir: &Path) -> BTreeSet<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|it| it.unwrap().file_name().into_string().unwrap())
            .filter(|it| matches!(manifest::kind(it), manifest::Kind::Listable))
            .collect()
    }

    /// The rows of `table` in the files the manifest of `dir` lists: what a reader reads.
    fn listed_rows(dir: &Path, table: &Table) -> usize {
        let manifest = manifest::read(dir).unwrap();
        manifest
            .files(table.name)
            .map(|it| read_back(&dir.join(it)).iter().map(RecordBatch::num_rows).sum::<usize>())
            .sum()
    }

    /// The manifest after flushes and a build's merge is the one the reopened history reads,
    /// byte for byte, and lists exactly the files on disk.
    #[test]
    fn the_manifest_round_trips_through_flushes_a_merge_and_a_reopen() {
        let dir = scratch("manifest-round-trip");
        let mut history = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        // The first flush takes what the rings hold, other tests' builds' rows among them.
        history.flush(None).unwrap();
        for _ in 0..3 {
            history.build_ended(row(5, Outcome::Ok));
            history.flush(None).unwrap();
        }
        history.build_ended(row(5, Outcome::Ok));
        let ended = history.flush(Some(5)).unwrap();
        assert!(ended.compacted >= 1, "the build's end merges its files: {ended:?}");
        drop(history);
        let bytes = fs::read(dir.join(manifest::FILE)).unwrap();
        let written = Manifest::decode(&bytes).unwrap();
        assert_eq!(written.encode().unwrap(), bytes, "the encoding is deterministic");
        assert_eq!(on_disk(&dir), written.all().map(str::to_owned).collect::<BTreeSet<_>>());
        assert_eq!(written.flushed(), ended.seq);
        assert_eq!(listed_rows(&dir, &schema::BUILDS), 4, "each build row once");
        let entry = written.entries(schema::BUILDS.name).next().unwrap().1;
        assert_eq!((entry.version, entry.rows, entry.builds), (schema::VERSION, 4, Some([5, 5])));
        assert_eq!(entry.boot.as_deref(), clock::boot());

        let history = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        assert_eq!(
            fs::read(dir.join(manifest::FILE)).unwrap(),
            bytes,
            "a reopen reads the manifest and changes nothing"
        );
        assert_eq!(on_disk(&dir), written.all().map(str::to_owned).collect::<BTreeSet<_>>());
        assert!(history.seq > ended.seq, "the next flush numbers past the manifest's");
    }

    /// A merge committed (its output listed, its inputs retired) and crashed before its
    /// inputs were deleted reads each row once, before the commit, after it, and after the
    /// reopen that deletes the inputs, which no pin holds.
    #[test]
    fn a_merge_committed_and_not_deleted_reads_each_row_once() {
        let dir = scratch("merged-not-deleted");
        let mut history = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        history.flush(None).unwrap();
        history.build_ended(row(7, Outcome::Ok));
        let one = history.flush(None).unwrap();
        history.build_ended(row(7, Outcome::Failed));
        let two = history.flush(None).unwrap();
        drop(history);
        let input = |it: &Flushed| dir.join(format!("{}-{:08}.builds.parquet", it.build, it.seq));
        let inputs = [input(&one), input(&two)];
        assert_eq!(listed_rows(&dir, &schema::BUILDS), 2);

        let output = Name {
            build: two.build,
            table: table_at(&schema::BUILDS),
            first: one.seq,
            last: two.seq,
            level: 1,
        };
        let path = dir.join(output.file_name());
        concat(&path, &inputs, &Writer::new().properties[output.table]).unwrap();
        assert_eq!(
            listed_rows(&dir, &schema::BUILDS),
            2,
            "an output not yet committed is read by no reader"
        );

        let mut held = manifest::hold(&dir).unwrap();
        let entries: Vec<manifest::Entry> =
            inputs.iter().map(|it| held.manifest().entry(name_of(it)).unwrap().clone()).collect();
        held.add(name_of(&path), manifest::Entry::merged(&entries).unwrap()).unwrap();
        held.retire(inputs.iter().map(|it| name_of(it)));
        held.commit().unwrap();
        drop(held);
        assert!(inputs.iter().all(|it| it.exists()), "the crash left the inputs");
        assert_eq!(listed_rows(&dir, &schema::BUILDS), 2, "the inputs are unlisted, so read once");

        drop(History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap());
        assert!(
            inputs.iter().all(|it| !it.exists()),
            "the open deletes the retired files no pin holds"
        );
        assert!(path.exists());
        assert!(manifest::read(&dir).unwrap().retired().files.is_empty(), "and forgets them");
        assert_eq!(listed_rows(&dir, &schema::BUILDS), 2);
    }

    /// A reader's pin keeps the files the manifest it read lists on disk while the cap (or a
    /// merge) retires them; they go at the first flush after the pin drops.
    #[test]
    fn a_pinned_reader_keeps_retired_files_until_it_lets_go() {
        let dir = scratch("pinned-merge");
        let mut history = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        history.flush(None).unwrap();
        history.build_ended(row(5, Outcome::Running));
        history.flush(None).unwrap();
        let pin = Pin::take(&dir).unwrap();
        let read: Vec<String> =
            pin.manifest().files(schema::BUILDS.name).map(str::to_owned).collect();
        assert!(!read.is_empty());
        let next = u64::from(history.next_build());
        history.build_ended(row(next, Outcome::Ok));
        // Past the cap, the older builds' files the pin read are retired, not deleted.
        history.cap = 1;
        history.flush(Some(next)).unwrap();
        let now = manifest::read(&dir).unwrap();
        assert!(
            read.iter().any(|it| now.retired().holds(it)),
            "the cap retired a file the pin read"
        );
        for name in &read {
            assert!(dir.join(name).exists(), "{name} was deleted under a pinned reader");
        }
        drop(pin);
        history.flush(None).unwrap();
        let after = manifest::read(&dir).unwrap();
        for (name, _) in &now.retired().files {
            assert!(!dir.join(name).exists(), "{name} stayed after the last pin let go");
        }
        assert!(after.retired().files.is_empty());
    }

    /// A history from before manifests gets one made of its files, written once by its next
    /// writer, listing the same files with entries read from their footers.
    #[test]
    fn a_history_with_no_manifest_is_adopted_from_its_files() {
        let dir = scratch("adopted");
        let mut history = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        history.build_ended(row(3, Outcome::Ok));
        history.flush(Some(3)).unwrap();
        drop(history);
        let written = manifest::read(&dir).unwrap();
        fs::remove_file(dir.join(manifest::FILE)).unwrap();

        let adopted = manifest::read(&dir).unwrap();
        assert!(!dir.join(manifest::FILE).exists(), "a reader writes no manifest");
        assert_eq!(adopted.all().collect::<Vec<_>>(), written.all().collect::<Vec<_>>());
        assert_eq!(adopted.flushed(), written.flushed());
        let (name, entry) = adopted.entries(schema::BUILDS.name).next().unwrap();
        let was = written.entry(name).unwrap();
        assert_eq!(
            (entry.version, entry.rows, entry.builds, entry.t_ns),
            (was.version, was.rows, was.builds, was.t_ns)
        );

        drop(History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap());
        assert!(dir.join(manifest::FILE).exists(), "the next writer writes it");
        assert_eq!(manifest::read(&dir).unwrap(), adopted);
    }

    /// A Parquet file of no form blaze writes stops the open, named, rather than being skipped
    /// or deleted.
    #[test]
    fn a_parquet_file_blaze_did_not_write_stops_the_open_naming_it() {
        let dir = scratch("unclassified");
        drop(History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap());
        fs::write(dir.join("notes.parquet"), b"not ours").unwrap();
        let Err(error) = History::open(&dir, CAP_BYTES, sampler::unsampled()) else {
            panic!("the open went on")
        };
        assert!(
            matches!(&error, HistoryError::Unclassified { file, .. } if file == "notes.parquet"),
            "{error}"
        );
        assert!(dir.join("notes.parquet").exists(), "left where it was");
    }

    /// The cap deletes a build whole: every table of it, every flush of it, at once.
    #[test]
    fn past_the_cap_a_build_goes_whole() {
        let dir = scratch("cap-whole");
        let mut history = History::open(&dir, CAP_BYTES, sampler::unsampled()).unwrap();
        // The first flush takes what the rings hold, other tests' builds' rows among them.
        history.flush(None).unwrap();
        history.build_ended(row(1, Outcome::Ok));
        let one = history.flush(None).unwrap();
        history.build_ended(row(1, Outcome::Failed));
        history.flush(None).unwrap();
        let old = one.build;
        assert!(
            history.kept.iter().filter(|it| it.name.build == old).count() >= 2,
            "two flushes of build {old}"
        );
        history.cap = 1;
        let next = u64::from(history.next_build());
        history.build_ended(row(next, Outcome::Ok));
        history.flush(Some(next)).unwrap();
        assert!(
            history.kept.iter().all(|it| it.name.build != old),
            "every file of build {old} is gone"
        );
        let manifest = manifest::read(&dir).unwrap();
        assert!(
            manifest.all().all(|it| Name::parse(it).is_none_or(|it| it.build != old)),
            "and unlisted"
        );
    }

    /// The flush's cost a span, in release: what docs/v2/plan/lanes/observe/history.md bounds.
    #[test]
    #[ignore = "a measurement: blaze test -p blaze-profile --release -- --ignored --nocapture what_a_flush_costs"]
    fn what_a_flush_costs() {
        let functions: Vec<flight::Sym> =
            (0..1000).map(|i| flight::intern(&format!("app::f{i}"))).collect();
        let rows: Vec<SpanRow> = (0..1_000_000u64)
            .map(|i| SpanRow {
                build: 1,
                thread: (i % 16) as u32,
                thread_name: 0,
                span_id: i,
                parent: i / 4,
                depth: (i % 8) as u16,
                site: 0,
                start_ns: (i * 1000) as i64,
                dur_ns: ((i * 7919) % 100_000) as i64,
                alloc: Some(Allocated { bytes: i % 4096, count: i % 17 }),
                copied: Some(FrameCopied { copies: 0, bytes: 0 }),
                krate: 0,
                function: functions[(i % 1000) as usize],
                unit: 0,
                self_cpu_ns: Some(i % 50_000),
                typed: crate::span::Said::EMPTY.typed,
                folded_spans: 0,
                folded_ns: 0,
            })
            .collect();
        let dir = scratch("cost");
        let mut writer = Writer::new();
        let start = std::time::Instant::now();
        let batch = batches::spans(&rows, Process::this()).unwrap();
        let arrow = start.elapsed();
        let path = dir.join("1-00000001.spans.parquet");
        let bytes = writer.write(&path, &schema::SPANS, std::slice::from_ref(&batch)).unwrap();
        let whole = start.elapsed();
        let n = rows.len() as f64;
        println!(
            "{} spans: {:.0} ns a span to Arrow, {:.0} ns a span to Parquet, {:.1} bytes a span",
            rows.len(),
            arrow.as_nanos() as f64 / n,
            (whole - arrow).as_nanos() as f64 / n,
            bytes as f64 / n,
        );
    }

    #[test]
    fn a_file_name_gives_its_seq() {
        let named =
            |name: &str| Name::parse(name).map(|it| (it.build, it.first, it.last, it.level));
        assert_eq!(named("41-00000007.spans.parquet"), Some((41, 7, 7, 0)));
        assert_eq!(named("0-00000012.counters.parquet"), Some((0, 12, 12, 0)));
        assert_eq!(
            named("3-00000001-00000016-1.spans.parquet"),
            Some((3, 1, 16, 1)),
            "a merged file"
        );
        assert_eq!(
            named("3-00000001-00000016-0.spans.parquet"),
            None,
            "a merged file is of level 1 or more"
        );
        assert_eq!(named("3-00000009-00000002-1.spans.parquet"), None);
        assert_eq!(named("schema.spans.parquet"), None);
        assert_eq!(named("live.spans.parquet"), None);
        assert_eq!(named("1-2.tiles.parquet"), None, "tiles are not kept");
        assert_eq!(named("1-2.spans.parquet.tmp"), None);
    }
}
