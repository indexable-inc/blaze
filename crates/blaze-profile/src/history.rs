//! The history: what the flight recorder holds, flushed to Parquet files that the pinned DuckDB
//! (`blaze debug sql`, never linked into blaze) reads through the views of `sql/views.sql`.
//!
//! A flush takes what is new in the rings since the last ([`flight::read`] from the history's
//! [`Cursor`]) and writes one file a table it has rows of, `<dir>/<build>-<seq>.<table>.parquet`:
//! `build` the build the flush is for (0 for the timer's flushes, which hold whatever ran), and
//! `seq` the flush's number, which only grows in a directory. A Parquet file holds one schema,
//! so a table a file keeps each file self-describing, and the views glob `*.<table>.parquet`.
//! Beside them, `schema.<table>.parquet` holds no rows and every column of each table, so each
//! glob matches a file even before a table has rows, with today's columns.
//!
//! Each file is made in memory, in a buffer kept across flushes, and written with one `write`
//! to a hidden name, then renamed, so DuckDB never reads half a file. Past [`CAP_BYTES`] (or the
//! cap the history is opened with), the oldest files are deleted first.

use std::{
    collections::VecDeque,
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

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
    facts::{self, CrateDepRow, LinkRow},
    flight::{self, Cursor, Read, SpanRow},
    identity,
    sampler::{self, Sample},
    schema::{self, Column, Table, Type},
    symbols,
};

/// What the history keeps by default: 1 GiB, about 50 million spans at the ~20 bytes a span
/// takes compressed, days of a busy daemon.
pub const CAP_BYTES: u64 = 1 << 30;

/// How often the daemon flushes while a session runs. A thread's ring holds [`flight::CAP`]
/// rows, so a thread writing more than `CAP / 5` rows a second between flushes loses the
/// oldest, counted in [`Flushed::skipped`].
pub const EVERY: Duration = Duration::from_secs(5);

/// Rows a row group holds at most: DuckDB's own row group size, the unit its scans split
/// across threads.
pub const ROW_GROUP_ROWS: usize = 122_880;

/// Where the daemon's history goes: `profile/` in its state directory.
pub fn dir_in(state: &Path) -> PathBuf {
    state.join("profile")
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
    pub spans: usize,
    pub counters: usize,
    pub builds: usize,
    pub samples: usize,
    pub switches: usize,
    pub errors: usize,
    pub links: usize,
    pub crate_deps: usize,
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

    /// An empty `schema.<table>.parquet` of each table in `dir`.
    pub(crate) fn schemas(&mut self, dir: &Path) -> Result<(), HistoryError> {
        for table in schema::TABLES {
            // alloc: a file's name, once a table when the history opens
            self.write(&dir.join(format!("schema.{}.parquet", table.name)), table, &[])?;
        }
        Ok(())
    }
}

/// Writes `bytes` as `path` in one write, under a hidden name first and renamed into place.
fn put(path: &Path, bytes: &[u8]) -> Result<(), HistoryError> {
    let name = path.file_name().and_then(|it| it.to_str()).unwrap_or("history");
    // alloc: a file's name, once a file
    let hidden = path.with_file_name(format!(".{name}.tmp"));
    fs::write(&hidden, bytes).map_err(io_error(&hidden, "writing"))?;
    fs::rename(&hidden, path).map_err(io_error(path, "renaming into place"))
}

fn column(array: impl Array + 'static) -> ArrayRef {
    // alloc: a batch's column, once a flush
    Arc::new(array)
}

/// The `builds` rows of `rows`.
pub fn builds(rows: &[BuildRow]) -> Result<RecordBatch, ArrowError> {
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
    cursor: Cursor,
    read: Read,
    /// A flush's spans of other builds than the one it is for.
    others: Vec<SpanRow>,
    writer: Writer,
    process: Process<'static>,
    /// The next flush's number.
    seq: u64,
    /// Builds that ended since the last flush: written by the next.
    ended: Vec<BuildRow>,
    /// Errors builds said since the last flush: written by the next.
    errors: Vec<ErrorRow>,
    /// Images builds linked, and the crate graphs they built, since the last flush.
    links: Vec<LinkRow>,
    crate_deps: Vec<CrateDepRow>,
    /// The files written, oldest first, and their bytes all told.
    kept: VecDeque<Kept>,
    total: u64,
    /// The highest build any file of the directory is named for, or a flush was for.
    last_build: u64,
    /// The CPU sampler's samples taken so far, and a flush's new ones.
    sampled: u64,
    samples: Vec<Sample>,
    /// [`identity::binary`], once a history: every build of this process ran it.
    blaze: String,
    /// A build's identity that could not be told since the last flush: the next flush writes
    /// its row without it, then answers this.
    unidentified: Option<HistoryError>,
}

impl History {
    /// The history in `dir`, made if it isn't there, holding at most `cap` bytes of files. Its
    /// first flush takes everything the rings still hold. A flush's number continues from the
    /// files already there; the hidden files a write left half done are deleted.
    pub fn open(dir: &Path, cap: u64) -> Result<History, HistoryError> {
        fs::create_dir_all(dir).map_err(io_error(dir, "making the directory"))?;
        // alloc: once a history
        let mut kept = Vec::new();
        let mut last_build = 0;
        for entry in fs::read_dir(dir).map_err(io_error(dir, "listing"))? {
            let entry = entry.map_err(io_error(dir, "listing"))?;
            let path = entry.path();
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') && name.ends_with(".parquet.tmp") {
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
        let mut history = History {
            // alloc: once a history
            dir: dir.to_owned(),
            cap,
            cursor: Cursor::default(),
            read: Read::default(),
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
            kept: kept.into(),
            total,
            last_build,
            sampled: 0,
            // alloc: once a history
            samples: Vec::new(),
            blaze,
            unidentified: None,
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

    /// Writes what the rings hold that is new since the last flush, and the builds that ended
    /// since, one file a table with rows, named for `build` (`None`: the timer's flush, named
    /// 0). Then deletes the oldest files past the cap. A failed write is an error naming the
    /// file; the rows it held are not written again.
    pub fn flush(&mut self, build: Option<u64>) -> Result<Flushed, HistoryError> {
        self.read.clear();
        flight::read(&mut self.cursor, &mut self.read);
        let seq = self.seq;
        self.seq += 1;
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
        let builds = [builds(&self.ended).map_err(arrow_error(&schema::BUILDS))?];
        let errors = [errors::batch(&self.errors).map_err(arrow_error(&schema::ERRORS))?];
        let links = [facts::links(&self.links).map_err(arrow_error(&schema::LINKS))?];
        let crate_deps = [facts::crate_deps(&self.crate_deps).map_err(arrow_error(&schema::CRATE_DEPS))?];
        let switches =
            [batches::switches(&self.read.switches, self.process).map_err(arrow_error(&schema::SWITCHES))?];
        self.samples.clear();
        let samples_skipped = sampler::since(&mut self.sampled, &mut self.samples);
        let samples = [self.samples_batch().map_err(arrow_error(&schema::SAMPLES))?];
        let rows = |parts: &[RecordBatch]| parts.iter().map(RecordBatch::num_rows).sum::<usize>();
        let mut flushed = Flushed {
            seq,
            spans: rows(&spans),
            counters: rows(&counters),
            builds: rows(&builds),
            samples: rows(&samples),
            switches: rows(&switches),
            errors: rows(&errors),
            links: rows(&links),
            crate_deps: rows(&crate_deps),
            skipped: self.read.skipped + samples_skipped,
            ..Flushed::default()
        };
        let build = build.unwrap_or(0);
        self.last_build = self.last_build.max(build);
        let tables: [(&'static Table, &[RecordBatch]); 8] = [
            (&schema::SPANS, &spans),
            (&schema::COUNTERS, &counters),
            (&schema::BUILDS, &builds),
            (&schema::SAMPLES, &samples),
            (&schema::SWITCHES, &switches),
            (&schema::ERRORS, &errors),
            (&schema::LINKS, &links),
            (&schema::CRATE_DEPS, &crate_deps),
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
        flushed.deleted = self.trim()?;
        match self.unidentified.take() {
            Some(error) => Err(error),
            None => Ok(flushed),
        }
    }

    /// The flush's CPU samples as a `samples` batch, their frames named.
    fn samples_batch(&self) -> Result<RecordBatch, ArrowError> {
        let pcs: Vec<u64> =
            // alloc: a flush's addresses, once a flush
            self.samples.iter().flat_map(|it| it.pcs[..usize::from(it.depth)].iter().copied()).collect();
        let mut names = symbols::names(&pcs);
        for pc in pcs {
            // alloc: a name for an address no symbol covers, once an address a flush
            names.entry(pc).or_insert_with(|| format!("{pc:#x}"));
        }
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
    let mut writer = Writer::new();
    writer.schemas(dir)?;
    let mut flushed = Flushed {
        spans: spans.num_rows(),
        counters: counters.num_rows(),
        switches: switches.num_rows(),
        ..Flushed::default()
    };
    let tables: [(&'static Table, RecordBatch); 3] =
        [(&schema::SPANS, spans), (&schema::COUNTERS, counters), (&schema::SWITCHES, switches)];
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
    use crate::flight::{Allocated, CounterRow, FrameCopied, Plot};

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
                query_kind: 0,
            })
            .collect();
        let counters = batches::counters(&counters, process).unwrap();
        let builds = builds(&[row(1, Outcome::Ok), row(2, Outcome::Stalled)]).unwrap();

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

    #[test]
    fn a_builds_row_names_the_tree_it_built_and_the_blaze_that_ran_it() {
        let dir = scratch("identity");
        let workspace = dir.join("work");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
        let mut history = History::open(&dir.join("profile"), CAP_BYTES).unwrap();
        let built = |history: &mut History, build_id| {
            let row = BuildRow {
                workspace: workspace.to_str().unwrap().into(),
                tree: None,
                blaze: None,
                ..row(build_id, Outcome::Ok)
            };
            history.build_ended(row);
            let flushed = history.flush(Some(build_id)).unwrap();
            let read = read_back(&dir.join(format!("profile/{build_id}-{:08}.builds.parquet", flushed.seq)));
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
        let mut history = History::open(&dir, CAP_BYTES).unwrap();
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
        let links = read_back(&dir.join(format!("61-{:08}.links.parquet", flushed.seq)));
        let image = links[0].column(schema::LINKS.column("image").unwrap()).as_string::<i32>().value(0);
        assert_eq!(image, "app (bin)");
        let deps = read_back(&dir.join(format!("61-{:08}.crate_deps.parquet", flushed.seq)));
        assert_eq!(deps.iter().map(RecordBatch::num_rows).sum::<usize>(), 2);

        let again = history.flush(Some(62)).unwrap();
        assert_eq!((again.links, again.crate_deps), (0, 0), "a flush writes each row once");
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
        let mut history = History::open(&dir, CAP_BYTES).unwrap();
        for table in schema::TABLES {
            let path = dir.join(format!("schema.{}.parquet", table.name));
            let read = read_back(&path);
            assert!(read.iter().all(|it| it.num_rows() == 0), "{} holds no rows", path.display());
        }
        history.build_ended(row(41, Outcome::Ok));
        let flushed = history.flush(Some(41)).unwrap();
        assert!(flushed.spans >= 1 && flushed.builds == 1 && flushed.files >= 2, "{flushed:?}");
        let path = dir.join(format!("41-{:08}.spans.parquet", flushed.seq));
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
        let builds = read_back(&dir.join(format!("41-{:08}.builds.parquet", flushed.seq)));
        assert_eq!(builds[0].num_rows(), 1);

        let again = history.flush(None).unwrap();
        assert_eq!(again.builds, 0, "a build's row is written once");
        assert!(again.seq > flushed.seq);
    }

    #[test]
    fn a_history_opened_again_numbers_on_from_its_files() {
        let dir = scratch("reopen");
        let mut history = History::open(&dir, CAP_BYTES).unwrap();
        history.build_ended(row(1, Outcome::Failed));
        let first = history.flush(Some(1)).unwrap();
        drop(history);
        fs::write(dir.join(".1-00000099.spans.parquet.tmp"), b"half").unwrap();
        let mut history = History::open(&dir, CAP_BYTES).unwrap();
        assert!(!dir.join(".1-00000099.spans.parquet.tmp").exists(), "a half-written file is deleted");
        assert_eq!(history.next_build(), 2, "a restarted daemon numbers its builds past the history's");
        history.build_ended(row(2, Outcome::Ok));
        let second = history.flush(Some(2)).unwrap();
        assert!(second.seq > first.seq, "{first:?} then {second:?}");
        assert!(dir.join(format!("1-{:08}.builds.parquet", first.seq)).exists());
        assert!(dir.join(format!("2-{:08}.builds.parquet", second.seq)).exists());
    }

    #[test]
    fn past_the_cap_the_oldest_files_go_first() {
        let dir = scratch("cap");
        let mut history = History::open(&dir, 1).unwrap();
        let mut seqs = Vec::new();
        for build in 1..=3 {
            history.build_ended(row(build, Outcome::Ok));
            let flushed = history.flush(Some(build)).unwrap();
            seqs.push(flushed.seq);
        }
        let name = |build: u64, seq: u64| dir.join(format!("{build}-{seq:08}.builds.parquet"));
        assert!(!name(1, seqs[0]).exists() && !name(2, seqs[1]).exists(), "the older builds' files are deleted");
        assert!(name(3, seqs[2]).exists(), "the newest flush's files stay, though they are past the cap alone");
        assert!(dir.join("schema.spans.parquet").exists(), "the schema files are not the cap's");
        assert!(history.kept.iter().all(|it| it.seq == seqs[2]), "only the newest flush is kept");
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
