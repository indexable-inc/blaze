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
//! so a table a file keeps each file self-describing, and the views glob `*.<table>.parquet`.
//! Beside them, `schema.<table>.parquet` holds no rows and every column of each table, so each
//! glob matches a file even before a table has rows, with today's columns.
//!
//! Each file is made in memory, in a buffer kept across flushes, and written with one `write`
//! to a hidden name, then renamed, so DuckDB never reads half a file. Past [`CAP_BYTES`] (or the
//! cap the history is opened with), the oldest files are deleted first.
//!
//! What a daemon that dies leaves readable: a build's `builds` row as it starts (outcome
//! `running`, [`History::build_started`]), and with each flush that wrote rows the spans every
//! thread was inside then (the `open` table). A build whose last row is its start, of a process
//! that no longer writes the history ([`writes`]), ended by its daemon's death; `blaze slow`
//! reports it so, with what was open at the last flush.

use std::{
    collections::VecDeque,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use parking_lot::Mutex;

use arrow_array::{
    Array, ArrayRef, DictionaryArray, Int64Array, RecordBatch, StringArray, UInt32Array, UInt64Array,
    types::UInt32Type,
};
use arrow_schema::ArrowError;
use parquet::{
    arrow::ArrowWriter,
    basic::{Compression, ZstdLevel},
    errors::ParquetError,
    file::properties::{WriterProperties, WriterVersion},
    schema::types::ColumnPath,
};

use crate::{
    batches::{self, Process},
    clock,
    errors::{self, ErrorRow},
    facts::{self, CrateDepRow, LinkRow, MissRow, QueryCountRow},
    flight::{self, CounterRow, Cursor, Plot, Read, SpanRow},
    identity,
    sampler::{self, Sample},
    schema::{self, Column, Table, Type},
    symbols,
};

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
        Ok(Err(fs::TryLockError::WouldBlock)) | Ok(Err(fs::TryLockError::Error(_))) | Err(_) => true,
    }
}

/// Whether a process `pid` is alive: one this user may not signal is, one that is gone is not.
#[cfg(unix)]
fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    // SAFETY: signal 0 sends nothing: the kernel only checks that `pid` names a process.
    let sent = unsafe { libc::kill(pid, 0) };
    sent == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn alive(_pid: u32) -> bool {
    true
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
}

impl Outcome {
    /// In the order of their numbers, which are their keys in the column's dictionary.
    pub const ALL: [Outcome; 5] = [Outcome::Ok, Outcome::Failed, Outcome::Cancelled, Outcome::Stalled, Outcome::Running];

    pub const fn name(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Failed => "failed",
            Outcome::Cancelled => "cancelled",
            Outcome::Stalled => "stalled",
            Outcome::Running => "running",
        }
    }
}

/// A row of the schema's `builds` table: one build request the daemon ran.
#[derive(Debug, PartialEq, Eq)]
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
}

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
    pub errors: usize,
    pub links: usize,
    pub crate_deps: usize,
    pub query_counts: usize,
    pub misses: usize,
    /// Spans the threads were inside as the flush read them.
    pub open: usize,
    pub folded: usize,
    pub files: u32,
    pub bytes: u64,
    /// Rows the rings wrote over before this flush read them, CPU samples included.
    pub skipped: u64,
    /// Old files deleted to stay under the cap.
    pub deleted: u32,
}

#[derive(Debug)]
pub enum HistoryError {
    Io { path: PathBuf, doing: &'static str, error: io::Error },
    Parquet { path: PathBuf, table: &'static str, error: ParquetError },
    Arrow { table: &'static str, error: ArrowError },
    /// What a build was of could not be told: its tree's digest, or blaze's own hash.
    Identity { what: &'static str, path: PathBuf, error: io::Error },
    /// The history `dir` has no schema file of `tables` (it was written before they existed),
    /// and writing them there failed: no view over it can be opened.
    SchemasMissing { dir: PathBuf, tables: Vec<&'static str>, error: Box<HistoryError> },
}

impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HistoryError::Io { path, doing, error } => {
                write!(f, "the profile history: {doing} {}: {error}", path.display())
            }
            HistoryError::Parquet { path, table, error } => {
                write!(f, "the profile history: writing the {table} table to {}: {error}", path.display())
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
        }
    }
}

fn io_error<'a>(path: &'a Path, doing: &'static str) -> impl FnOnce(io::Error) -> HistoryError + 'a {
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
        Type::I64 | Type::F64 => false,
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

fn table_at(table: &Table) -> usize {
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
    pub(crate) fn new() -> Writer {
        // alloc: once a history
        Writer { properties: schema::TABLES.iter().map(|it| properties(it)).collect(), buf: Vec::with_capacity(1 << 20) }
    }

    /// Writes `parts`, rows of `table`, as the Parquet file `path`; its size.
    pub(crate) fn write(
        &mut self,
        path: &Path,
        table: &'static Table,
        parts: &[RecordBatch],
    ) -> Result<u64, HistoryError> {
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
        let mut writer = ArrowWriter::try_new(buf, batches::schema_of(table), Some(properties)).map_err(parquet)?;
        for part in parts {
            writer.write(part).map_err(parquet)?;
        }
        let buf = writer.into_inner().map_err(parquet)?;
        let written = put(path, &buf);
        let bytes = buf.len() as u64;
        self.buf = buf;
        written.map(|()| bytes)
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

/// The empty file of `table` in the history `dir`, which every view's glob matches, so a view
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

/// Writes `bytes` as `path` in one write, under a hidden name first and renamed into place.
/// The hidden name is this process's: a reader writing a missing schema file
/// ([`ensure_schemas`]) while a daemon opens the same history must not write into its file.
fn put(path: &Path, bytes: &[u8]) -> Result<(), HistoryError> {
    let name = path.file_name().and_then(|it| it.to_str()).unwrap_or("history");
    // alloc: a file's name, once a file
    let hidden = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&hidden, bytes).map_err(io_error(&hidden, "writing"))?;
    fs::rename(&hidden, path).map_err(io_error(path, "renaming into place"))
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
    ];
    RecordBatch::try_new(batches::schema_of(&schema::BUILDS), columns)
}

/// A file of the history the cap counts: written by a flush, deleted oldest first.
struct Kept {
    seq: u64,
    bytes: u64,
    path: PathBuf,
}

/// The `seq` of a history file's name, `<build>-<seq>.<table>.parquet`.
/// The build and the flush's number a history file is named for.
fn named(name: &str) -> Option<(u64, u64)> {
    let (stem, table) = name.strip_suffix(".parquet")?.split_once('.')?;
    schema::TABLES.iter().find(|it| it.name == table)?;
    let (build, seq) = stem.split_once('-')?;
    Some((build.parse().ok()?, seq.parse().ok()?))
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
    /// The files written, oldest first, and their bytes all told.
    kept: VecDeque<Kept>,
    total: u64,
    /// The highest build any file of the directory is named for, or a flush was for.
    last_build: u64,
    /// The ring a flush reads CPU samples from ([`History::open`]), the samples of it read so
    /// far, and a flush's new ones.
    sampler: &'static sampler::Ring,
    sampled: u64,
    samples: Vec<Sample>,
    /// [`identity::binary`], once a history: every build of this process ran it.
    blaze: String,
    /// A build's identity that could not be told since the last flush: the next flush writes
    /// its row without it, then answers this.
    unidentified: Option<HistoryError>,
    /// The threads' open spans the last flush wrote, their times left out ([`same_open`]).
    open_written: Vec<flight::ThreadNow>,
}

impl History {
    /// The history in `dir`, made if it isn't there, holding at most `cap` bytes of files. Its
    /// first flush takes everything the rings still hold. A flush's number continues from the
    /// files already there; the hidden files a write left half done are deleted. Its flushes
    /// read CPU samples from `sampler`: the process's ring ([`sampler::process`]) in a daemon,
    /// a ring of a test's own in a test, so what a test's flush writes never depends on a
    /// sampler that something else in its process started.
    pub fn open(dir: &Path, cap: u64, sampler: &'static sampler::Ring) -> Result<History, HistoryError> {
        fs::create_dir_all(dir).map_err(io_error(dir, "making the directory"))?;
        // alloc: once a history
        let mut kept = Vec::new();
        let mut last_build = 0;
        for entry in fs::read_dir(dir).map_err(io_error(dir, "listing"))? {
            let entry = entry.map_err(io_error(dir, "listing"))?;
            let path = entry.path();
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // `.<file>.parquet.<pid>.tmp`, and `.<file>.parquet.tmp` of a blaze before the pid.
            if name.starts_with('.') && name.ends_with(".tmp") && name.contains(".parquet.") {
                fs::remove_file(&path).map_err(io_error(&path, "deleting a half-written file"))?;
                continue;
            }
            let Some((build, seq)) = named(name) else { continue };
            last_build = last_build.max(build);
            let bytes = entry.metadata().map_err(io_error(&path, "reading the size of"))?.len();
            kept.push(Kept { seq, bytes, path });
        }
        kept.sort_unstable_by_key(|it| it.seq);
        let seq = kept.last().map_or(1, |it| it.seq + 1);
        let total = kept.iter().map(|it| it.bytes).sum();
        let mut writer = Writer::new();
        writer.schemas(dir)?;
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
            .map_err(io_error(dir, "starting the drain thread (`blaze-drain`) of the history in"))?;
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
            kept: kept.into(),
            total,
            last_build,
            sampler,
            sampled: 0,
            // alloc: once a history
            samples: Vec::new(),
            blaze,
            unidentified: None,
            // alloc: once a history
            open_written: Vec::new(),
        };
        history.trim()?;
        Ok(history)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
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
        self.ended.push(row);
    }

    /// Notes a build that ended, for the next flush to write; the flush for its build
    /// ([`History::flush`] with `Some(row.build_id)`) also gives its spans the build's commit.
    /// The row's `blaze` and `tree` are told here when not given: on the history's thread, never
    /// the build's. A tree that can't be digested leaves `tree` null, and the next flush answers
    /// why.
    pub fn build_ended(&mut self, mut row: BuildRow) {
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
        self.ended.push(row);
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

    /// Writes what the rings hold that is new since the last flush, and the builds that ended
    /// since, one file a table with rows, named for the highest build any of its rows is of and
    /// never below `build` (`None`: the timer's flush, named 0 when it holds no build's rows).
    /// Then deletes the oldest files past the cap. A failed write is an error naming the
    /// file; the rows it held are not written again.
    pub fn flush(&mut self, build: Option<u64>) -> Result<Flushed, HistoryError> {
        let (late, behind) = self.drained.take(&mut self.read);
        // The flight recorder's own losses, as counters: a query sees whether a stretch with
        // no span is one the recorder dropped. Written with each build's flush and whenever
        // they grew, so an idle daemon's timer writes no file for them.
        // Counted since the process began; the row of a build's flush carries the build.
        let (t_ns, now) = (self.read.at_ns, (self.read.lost, late + behind, behind));
        if build.is_some() || now != self.told {
            self.told = now;
            let build = build.and_then(|it| u32::try_from(it).ok()).unwrap_or(0);
            self.read.counters.extend(
                [(Plot::ProfileSpansLost, now.0), (Plot::ProfileRowsSkipped, now.1), (Plot::ProfileRowsBehind, now.2)]
                    .map(|(plot, value)| CounterRow { build, plot, t_ns, value, fiber: None }),
            );
        }
        self.told_bugs(build, t_ns, stdx::assert::reported::total(), stdx::assert::reported::sites);
        let written = self.write(build);
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
            self.read.counters.push(CounterRow { build: build32, plot: Plot::BlazeBugs, t_ns, value: total, fiber: None });
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
                (thread, Some(stack)) => format!("first on thread `{}`:\n{stack}", thread.as_deref().unwrap_or("unnamed")),
                // alloc: as above
                (thread, None) => format!("first on thread `{}`", thread.as_deref().unwrap_or("unnamed")),
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
                message: format!("a blaze bug: `{}` {}, {more} more time{}", site.kind, site.what, if more == 1 { "" } else { "s" }),
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
        let seq = self.seq;
        self.seq += 1;
        let named = self.named_for(build);
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
        let spans = [
            batches::spans(&self.read.spans, of_build).map_err(arrow_error(&schema::SPANS))?,
            batches::spans(&self.others, self.process).map_err(arrow_error(&schema::SPANS))?,
        ];
        let counters =
            [batches::counters(&self.read.counters, self.process).map_err(arrow_error(&schema::COUNTERS))?];
        let builds = [builds(&self.ended, self.process.pid).map_err(arrow_error(&schema::BUILDS))?];
        let errors = [errors::batch(&self.errors).map_err(arrow_error(&schema::ERRORS))?];
        let links = [facts::links(&self.links).map_err(arrow_error(&schema::LINKS))?];
        let crate_deps = [facts::crate_deps(&self.crate_deps).map_err(arrow_error(&schema::CRATE_DEPS))?];
        let query_counts =
            [facts::query_counts(&self.query_counts).map_err(arrow_error(&schema::QUERY_COUNTS))?];
        let misses = [facts::misses(&self.misses).map_err(arrow_error(&schema::MISSES))?];
        let switches =
            [batches::switches(&self.read.switches, self.process).map_err(arrow_error(&schema::SWITCHES))?];
        let events = [batches::events(&self.read.events, self.process).map_err(arrow_error(&schema::EVENTS))?];
        let folded = [batches::folded(&self.read.folds, self.process).map_err(arrow_error(&schema::FOLDED))?];
        self.samples.clear();
        let samples_skipped = self.sampler.since(&mut self.sampled, &mut self.samples);
        let samples = [self.samples_batch().map_err(arrow_error(&schema::SAMPLES))?];
        let rows = |parts: &[RecordBatch]| parts.iter().map(RecordBatch::num_rows).sum::<usize>();
        // What every thread is inside, with each flush where that changed: the last reading is
        // what a daemon that died was doing, which no `spans` row holds (they are written as
        // they close), and a deadlocked one's stays its last. An idle daemon's flushes write none.
        let now = flight::threads_now();
        let open = match same_open(&now, &self.open_written) {
            false => {
                let batch = batches::open(&now, clock::mono_ns(), self.process).map_err(arrow_error(&schema::OPEN))?;
                self.open_written = now;
                batch
            }
            true => RecordBatch::new_empty(batches::schema_of(&schema::OPEN)),
        };
        let open = [open];
        let mut flushed = Flushed {
            seq,
            spans: rows(&spans),
            counters: rows(&counters),
            builds: rows(&builds),
            samples: rows(&samples),
            switches: rows(&switches),
            events: rows(&events),
            errors: rows(&errors),
            links: rows(&links),
            crate_deps: rows(&crate_deps),
            query_counts: rows(&query_counts),
            misses: rows(&misses),
            open: rows(&open),
            folded: rows(&folded),
            skipped: self.read.skipped + samples_skipped,
            ..Flushed::default()
        };
        let build = named;
        flushed.build = build;
        self.last_build = self.last_build.max(build);
        let tables: [(&'static Table, &[RecordBatch]); 13] = [
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
            (&schema::OPEN, &open),
            (&schema::FOLDED, &folded),
        ];
        for (table, parts) in tables {
            if rows(parts) == 0 {
                continue;
            }
            // alloc: a file's name, once a file
            let path = self.dir.join(format!("{build}-{seq:08}.{}.parquet", table.name));
            let bytes = self.writer.write(&path, table, parts)?;
            self.kept.push_back(Kept { seq, bytes, path });
            self.total += bytes;
            flushed.files += 1;
            flushed.bytes += bytes;
        }
        self.ended.clear();
        self.errors.clear();
        self.links.clear();
        self.crate_deps.clear();
        self.query_counts.clear();
        self.misses.clear();
        flushed.deleted = self.trim()?;
        match self.unidentified.take() {
            Some(error) => Err(error),
            None => Ok(flushed),
        }
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
        ];
        facts.into_iter().flatten().fold(build.unwrap_or(0).max(self.read.highest_build()), u64::max)
    }

    /// The flush's CPU samples as a `samples` batch, their frames named. A flush with no new
    /// samples names nothing.
    fn samples_batch(&self) -> Result<RecordBatch, ArrowError> {
        if self.samples.is_empty() {
            return Ok(RecordBatch::new_empty(batches::schema_of(&schema::SAMPLES)));
        }
        let pcs: Vec<u64> =
            // alloc: a flush's addresses, once a flush
            self.samples.iter().flat_map(|it| it.pcs[..usize::from(it.depth)].iter().copied()).collect();
        // Every address gets a name, the bare address at worst.
        let names = symbols::names(&pcs);
        batches::samples(&self.samples, &names, (Instant::now(), clock::mono_ns()), self.process)
    }

    /// Deletes the oldest files until the rest fit under the cap, keeping at least the newest
    /// flush's files, every table of it.
    fn trim(&mut self) -> Result<u32, HistoryError> {
        let mut deleted = 0;
        let newest = self.kept.back().map_or(0, |it| it.seq);
        while self.total > self.cap && self.kept.front().is_some_and(|it| it.seq != newest) {
            let oldest = &self.kept[0];
            match fs::remove_file(&oldest.path) {
                Ok(()) => {}
                // Gone already, as the cap wants it.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(HistoryError::Io {
                        // clone: an error
                        path: oldest.path.clone(),
                        doing: "deleting the oldest file past the history's cap",
                        error,
                    });
                }
            }
            self.total -= oldest.bytes;
            self.kept.pop_front();
            deleted += 1;
        }
        Ok(deleted)
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
    /// Reads the rings up to now and swaps what was drained into `into` (which is clear):
    /// the rows skipped so far, late and behind.
    fn take(&self, into: &mut Read) -> (u64, u64) {
        let mut at = self.0.lock();
        at.read_now();
        std::mem::swap(&mut at.read, into);
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

/// The last `window` of the rings as `<dir>/live.<table>.parquet`, a file of every table (empty
/// where the rings hold none of it) beside `schema.<table>.parquet`, for `blaze debug sql
/// --live` to query beside the history.
pub fn write_live(window: Duration, dir: &Path) -> Result<Flushed, HistoryError> {
    fs::create_dir_all(dir).map_err(io_error(dir, "making the directory"))?;
    let mut read = Read::default();
    flight::window(window, &mut read);
    let process = Process::this();
    let spans = batches::spans(&read.spans, process).map_err(arrow_error(&schema::SPANS))?;
    let counters = batches::counters(&read.counters, process).map_err(arrow_error(&schema::COUNTERS))?;
    let switches = batches::switches(&read.switches, process).map_err(arrow_error(&schema::SWITCHES))?;
    let events = batches::events(&read.events, process).map_err(arrow_error(&schema::EVENTS))?;
    let mut writer = Writer::new();
    writer.schemas(dir)?;
    let mut flushed = Flushed {
        spans: spans.num_rows(),
        counters: counters.num_rows(),
        switches: switches.num_rows(),
        events: events.num_rows(),
        ..Flushed::default()
    };
    let tables: [(&'static Table, RecordBatch); 4] = [
        (&schema::SPANS, spans),
        (&schema::COUNTERS, counters),
        (&schema::SWITCHES, switches),
        (&schema::EVENTS, events),
    ];
    for (table, batch) in tables {
        // alloc: a file's name, once a file
        let path = dir.join(format!("live.{}.parquet", table.name));
        flushed.bytes += writer.write(&path, table, std::slice::from_ref(&batch))?;
        flushed.files += 1;
    }
    Ok(flushed)
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
        ParquetRecordBatchReaderBuilder::try_new(file).unwrap().build().unwrap().map(Result::unwrap).collect()
    }

    fn same(written: &RecordBatch, read: &[RecordBatch]) {
        assert_eq!(read.len(), 1, "a small table reads back as one batch");
        let read = &read[0];
        assert_eq!(read.num_rows(), written.num_rows());
        assert_eq!(read.schema().fields(), written.schema().fields(), "the Arrow types come back, dictionaries too");
        for (i, field) in written.schema().fields().iter().enumerate() {
            assert!(read.column(i).as_ref() == written.column(i).as_ref(), "{} reads back the same", field.name());
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
        }
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
        for (table, batch) in [(&schema::SPANS, &spans), (&schema::COUNTERS, &counters), (&schema::BUILDS, &builds)] {
            let path = dir.join(format!("1-00000001.{}.parquet", table.name));
            let bytes = writer.write(&path, table, std::slice::from_ref(batch)).unwrap();
            assert_eq!(bytes, fs::metadata(&path).unwrap().len());
            let read = read_back(&path);
            same(batch, &read);
            // arrow-rs gives the batches it reads no schema metadata; the file's schema has it.
            let file = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(&path).unwrap()).unwrap();
            assert_eq!(file.schema().metadata()["blaze.table"], table.name);
        }
        let read = read_back(&dir.join("1-00000001.builds.parquet"));
        let outcome = read[0].column(schema::BUILDS.column("outcome").unwrap()).as_dictionary::<UInt32Type>();
        let names = outcome.values().as_string::<i32>();
        assert_eq!(names.value(outcome.keys().value(1) as usize), "stalled");
        assert!(fs::read_dir(&dir).unwrap().all(|it| !it.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
    }

    /// Each flush says the process's reported bugs: the counter with each build's flush and
    /// whenever it grew, and a `blaze-bug` row for each site that reported since the last,
    /// naming the value kind, the site and how many more times.
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
            (row.kind.as_str(), row.function.as_deref(), row.message.as_str(), row.detail.as_deref()),
            (
                "blaze-bug",
                Some("AdtDef"),
                "a blaze bug: `AdtDef` formatted with no database attached, 2 more times",
                Some("first on thread `driver`")
            )
        );
        assert_eq!((row.file.as_deref(), row.line, row.t_ns), (Some(at.file()), Some(at.line()), Some(20)));
        history.told_bugs(None, 30, 2, || panic!("the sites are read only when the total grew"));
        assert_eq!(said(&mut history), (Vec::new(), Vec::new()), "a timer's flush with nothing new says nothing");
        history.told_bugs(None, 40, 3, || vec![site(3)]);
        let (counters, errors) = said(&mut history);
        assert_eq!(counters, [(0, 3)]);
        assert_eq!(errors.iter().map(|it| it.message.as_str()).collect::<Vec<_>>(), [
            "a blaze bug: `AdtDef` formatted with no database attached, 1 more time"
        ]);
    }

    #[test]
    fn a_builds_row_names_the_tree_it_built_and_the_blaze_that_ran_it() {
        let dir = scratch("identity");
        let workspace = dir.join("work");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
        let mut history = History::open(&dir.join("profile"), CAP_BYTES, sampler::unsampled()).unwrap();
        let built = |history: &mut History, build_id| {
            let row = BuildRow {
                workspace: workspace.to_str().unwrap().into(),
                tree: None,
                blaze: None,
                ..row(build_id, Outcome::Ok)
            };
            history.build_ended(row);
            let flushed = history.flush(Some(build_id)).unwrap();
            let read = read_back(&dir.join(format!("profile/{}-{:08}.builds.parquet", flushed.build, flushed.seq)));
            let string = |name: &str| {
                read[0].column(schema::BUILDS.column(name).unwrap()).as_string::<i32>().value(0).to_owned()
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
            CrateDepRow { build_id: 61, krate: "app".into(), dep: "serde".into(), kind: DepKind::Normal },
            CrateDepRow { build_id: 61, krate: "app".into(), dep: "cc".into(), kind: DepKind::Build },
        ]);
        let flushed = history.flush(Some(61)).unwrap();
        assert_eq!((flushed.links, flushed.crate_deps), (1, 2));
        let links = read_back(&dir.join(format!("{}-{:08}.links.parquet", flushed.build, flushed.seq)));
        let image = links[0].column(schema::LINKS.column("image").unwrap()).as_string::<i32>().value(0);
        assert_eq!(image, "app (bin)");
        let deps = read_back(&dir.join(format!("{}-{:08}.crate_deps.parquet", flushed.build, flushed.seq)));
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
        };
        history.query_counted([mir, QueryCountRow { query: "infer_body_query", executed: 7566, ..mir }]);
        let flushed = history.flush(Some(63)).unwrap();
        assert_eq!(flushed.query_counts, 2);
        let read = read_back(&dir.join(format!("{}-{:08}.query_counts.parquet", flushed.build, flushed.seq)));
        let batch = &read[0];
        let count = |name: &str, row: usize| {
            batch.column(schema::QUERY_COUNTS.column(name).unwrap()).as_primitive::<UInt64Type>().value(row)
        };
        let query = batch.column(schema::QUERY_COUNTS.column("query").unwrap()).as_dictionary::<UInt32Type>();
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
                "reads_most"
            ]
            .map(|it| count(it, 0)),
            [63, 6384, 10, 2, 3, 4, 5, 6, 7, 8],
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
            let names = batch.column(schema::SPANS.column("name").unwrap()).as_dictionary::<UInt32Type>();
            let commits = batch.column(schema::SPANS.column("commit").unwrap()).as_dictionary::<UInt32Type>();
            (0..batch.num_rows()).any(|row| {
                let name = names.values().as_string::<i32>().value(names.keys().value(row) as usize);
                let commit = commits.keys().is_valid(row).then(|| commits.values().as_string::<i32>().value(commits.keys().value(row) as usize));
                name == "history flushed" && commit == Some("0123abc")
            })
        });
        assert!(found, "the span is in the build's file, with the build's commit");
        let builds = read_back(&dir.join(format!("{}-{:08}.builds.parquet", flushed.build, flushed.seq)));
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
        let spans = read_back(&dir.join(format!("{}-{:08}.spans.parquet", flushed.build, flushed.seq)));
        let ours: usize = spans
            .iter()
            .map(|batch| {
                let names = batch.column(schema::SPANS.column("name").unwrap()).as_dictionary::<UInt32Type>();
                let threads = batch.column(schema::SPANS.column("thread").unwrap()).as_primitive::<UInt32Type>();
                (0..batch.num_rows())
                    .filter(|&row| {
                        threads.value(row) == tid && names.values().as_string::<i32>().value(names.keys().value(row) as usize) == "history drained"
                    })
                    .count()
            })
            .sum();
        assert_eq!(ours, bursts * burst, "{} rows written, {} rows a ring", bursts * burst, flight::CAP);
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
        assert!(!dir.join(".1-00000099.spans.parquet.tmp").exists(), "a half-written file is deleted");
        // The first flush takes what the rings hold, other tests' builds' rows among them.
        assert!(first.build >= 1, "{first:?}");
        assert_eq!(u64::from(history.next_build()), first.build + 1, "a restarted daemon numbers its builds past the history's");
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
        assert!(timer.build >= u64::from(KILLED), "the timer's flush is named for the build its rows are of: {timer:?}");
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
        assert_eq!(symbols::ASKED.with(std::cell::Cell::get), asked, "a flush with no samples asked for names");
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
        assert_eq!(history.flush(None).unwrap().samples, 1, "the sample kept into the history's ring");
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
        assert!(!name(&flushes[0]).exists() && !name(&flushes[1]).exists(), "the older builds' files are deleted");
        assert!(name(&flushes[2]).exists(), "the newest flush's files stay, though they are past the cap alone");
        assert!(dir.join("schema.spans.parquet").exists(), "the schema files are not the cap's");
        assert!(history.kept.iter().all(|it| it.seq == flushes[2].seq), "only the newest flush is kept");
    }

    /// The flush's cost a span, in release: what docs/v2/plan/lanes/observe/history.md bounds.
    #[test]
    #[ignore = "a measurement: blaze test -p blaze-profile --release -- --ignored --nocapture what_a_flush_costs"]
    fn what_a_flush_costs() {
        let functions: Vec<flight::Sym> = (0..1000).map(|i| flight::intern(&format!("app::f{i}"))).collect();
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
        assert_eq!(named("41-00000007.spans.parquet"), Some((41, 7)));
        assert_eq!(named("0-00000012.counters.parquet"), Some((0, 12)));
        assert_eq!(named("schema.spans.parquet"), None);
        assert_eq!(named("live.spans.parquet"), None);
        assert_eq!(named("1-2.tiles.parquet"), None, "tiles are not kept");
        assert_eq!(named("1-2.spans.parquet.tmp"), None);
    }

    #[test]
    fn the_live_window_is_a_file_of_every_table() {
        crate::install();
        std::thread::spawn(|| {
            let _span = tracing::info_span!(target: "blaze", "history live").entered();
        })
        .join()
        .unwrap();
        let dir = scratch("live");
        let flushed = write_live(Duration::from_secs(60), &dir).unwrap();
        assert!(flushed.spans >= 1);
        for table in schema::TABLES {
            assert!(dir.join(format!("schema.{}.parquet", table.name)).exists());
        }
        let spans = read_back(&dir.join("live.spans.parquet"));
        assert_eq!(spans.iter().map(|it| it.num_rows()).sum::<usize>(), flushed.spans);
    }
}
