//! The history's manifest: `manifest.json` in its directory, the one list of the Parquet files
//! a reader reads, a table at a time. A file is part of the history once the manifest lists it
//! and stops being part of it once the manifest no longer does: the rewrite of the manifest
//! (to a hidden name, then renamed over it) is the one commit point of every change.
//!
//! - A flush writes its files in place, then lists them in one commit
//!   ([`super::History::flush`]).
//! - A merge writes its output in place, then in one commit lists it and retires its inputs:
//!   a reader reads the inputs or the output, never both.
//! - The cap retires the oldest files.
//!
//! A file retired is unlisted at once and named in the manifest's [`Retired`] until it is
//! deleted, which waits until no reader's pin can still read it ([`super::retire`]): a reader
//! pins the history ([`super::Pin`]) before it reads the manifest, and holds the pin until its
//! last read of the files that manifest lists has ended.
//! - Another process's writer (`blaze debug time`'s derived tables, `blaze perf record`'s runs)
//!   writes its files to hidden names, then holding the lock renames them into place and lists
//!   them ([`super::write_listed`]).
//!
//! So a crash leaves at worst a file the manifest neither lists nor retires (a merge's output
//! written but not committed): garbage the history's next open deletes
//! ([`super::History::open`]). A retired file a crash left is deleted once no pin holds it. A file the manifest lists is never
//! missing, unless something other than blaze deleted it; the open refuses that, naming it.
//!
//! Writers take [`LOCK`] around each read-modify-write of the manifest, so two processes'
//! commits never lose each other's entries. Readers take no lock: the manifest is replaced
//! whole by a rename, so a reader reads one version of it.
//!
//! Each file's entry ([`Entry`]) says what a reader needs without opening it: its schema's
//! version, its rows, the builds and the instants (and the boot they count from) its rows
//! span. A reader of one build reads only the files whose entries can hold its rows
//! ([`Manifest::of_build`]): 2026-11-04, making the views over every file read the footers
//! of 6,000 files, 17 CPU-seconds, for a query of one build. A reader refuses a file of a
//! version it can't read as this one ([`schema::readable`]), naming it.
//!
//! What the history consumed of the files beside it that hold rows it writes is part of the
//! same commit ([`Consumed`]): the flush that writes a flight recorder's rows says so in the
//! commit that lists them, as does the postmortem that writes a dead daemon's, and the flush
//! that writes a crash record's row. So a crash between a commit and the mark or delete that
//! follows it never has the next open read those rows again (2026-11-04: it did, and wrote them
//! twice).
//!
//! A directory with no manifest is a history written before manifests (2026-11-04): its
//! manifest is made once from the files there, each classified by its name ([`listable`])
//! and its entry read from its footer ([`Entry::of_footer`]); a file it can't classify is
//! [`HistoryError::Unclassified`], never skipped, and the files a merged file already holds
//! the rows of (a crash between a merge's rename and its deletes, before manifests) are left
//! out, and deleted by a writer.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use arrow_array::{
    RecordBatch,
    cast::AsArray,
    types::{Int64Type, UInt64Type},
};
use parquet::file::{
    metadata::{ParquetMetaData, ParquetMetaDataReader},
    statistics::Statistics,
};
use serde_derive::{Deserialize, Serialize};
use stdx::file_lock::Locked;

use super::{HistoryError, compact::Name, io_error, put, retire::Retired};
use crate::{clock, perf, schema};

/// The manifest's file in the history's directory.
pub const FILE: &str = "manifest.json";

/// The file whose exclusive lock a writer holds around each change to the manifest.
pub const LOCK: &str = "manifest.lock";

/// The manifest's format: a reader refuses another, naming it. Format 2 (2026-11-04) added
/// [`Consumed`]; a manifest of format 1 reads as one that consumed nothing, and is written
/// as format 2 at its next commit.
pub const FORMAT: u32 = 2;

/// The oldest format this blaze reads.
const FORMAT_OLDEST: u32 = 1;

/// How long a writer waits for the manifest's lock before failing, naming it. A holder holds
/// it for one read and one rewrite of the manifest (and the renames of the files it lists),
/// milliseconds, so a wait this long is a holder that is stuck.
pub const LOCK_WAIT: Duration = Duration::from_secs(5);

/// The files of a history, by table.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// [`FORMAT`].
    format: u32,
    /// The number of the last flush the history committed (0: none): a reader that asked the
    /// daemon to flush checks the manifest it reads is that flush's or later.
    flushed: u64,
    /// Each table's files, by the table's name: file names in the history's directory, each
    /// `<stem>.<table>.parquet`, and each one's entry. A table with no file has no entry. A
    /// table this blaze does not know (a newer blaze's) is kept as written, and read by no view.
    tables: BTreeMap<String, BTreeMap<String, Entry>>,
    /// The history's generation and the files unlisted and not yet deleted
    /// ([`super::retire`]). Left out while nothing was ever retired, so a manifest of a history
    /// that never merged nor capped reads as it did before retirement.
    #[serde(default, skip_serializing_if = "Retired::never")]
    retired: Retired,
    /// What the history consumed of the flight recorders' mappings and the crash records, as
    /// of this manifest's files. Left out while empty.
    #[serde(default, skip_serializing_if = "Consumed::is_empty")]
    consumed: Consumed,
}

/// What a commit consumed of the files beside the history that hold rows it writes, so the
/// rows a listed file holds are never read from them again: a mark that follows the commit
/// (a mapping's flushed mark, a delete) is lost in a crash between the two, and this is not.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Consumed {
    /// Each flight recorder mapping (`flight-<pid>.map`) a commit wrote rows of, by its
    /// process. Forgotten once its mapping is gone ([`Held::prune_consumed`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub recorders: BTreeMap<u32, Recorder>,
    /// The crash records (`crash.<pid>`) whose `panics` rows a listed file holds, each by its
    /// process and the instant of its signal: the next open deletes such a record unread.
    /// Forgotten once the record is gone.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub crashes: BTreeSet<(u32, i64)>,
}

impl Consumed {
    pub fn is_empty(&self) -> bool {
        self.recorders.is_empty() && self.crashes.is_empty()
    }
}

/// What the history wrote of one flight recorder mapping ([`crate::flight::postmortem`]).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Recorder {
    /// The mapping's nonce, from its header: a mapping of a later process given the same pid
    /// has another.
    pub nonce: u64,
    /// The last flush of its process this manifest committed, 0 for none: a postmortem reads
    /// the rows past the marks of that flush when the mapping holds them.
    pub flushed: u64,
    /// A postmortem wrote its rows, listed by this manifest: the mapping is deleted unread.
    pub recovered: bool,
}

/// What a file listed holds, so a reader picks the files it reads without opening any.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// The version of the schema its rows are of ([`schema::VERSION`] when it was written).
    pub version: u32,
    /// Its rows.
    pub rows: u64,
    /// The lowest and highest `build_id` of its rows; `None`: no row of a build (no such
    /// column, or every one null). A range a footer can't bound is `[0, u64::MAX]`.
    pub builds: Option<[u64; 2]>,
    /// The earliest and latest instant of its rows (each of [`INSTANTS`] it has), on the clock
    /// of `boot`; `None`: no instant. A range a footer can't bound is `[i64::MIN, i64::MAX]`.
    pub t_ns: Option<[i64; 2]>,
    /// The boot its instants count from ([`clock::boot`]); `None`: unknown, any boot.
    pub boot: Option<String>,
    /// The process that wrote it; `None`: several (a merge of several), or unknown.
    pub pid: Option<u32>,
}

/// The columns of an instant on the machine's clock; the other `*_ns` columns are durations.
pub const INSTANTS: [&str; 6] = ["t_ns", "start_ns", "started_ns", "from_ns", "to_ns", "read_ns"];

impl Entry {
    /// The entry of a file of `table` this process writes with the rows of `parts`.
    pub fn of_batches(table: &schema::Table, parts: &[RecordBatch]) -> Entry {
        let mut entry = Entry {
            version: schema::VERSION,
            rows: 0,
            builds: None,
            t_ns: None,
            // alloc: a file's entry, once a file
            boot: clock::boot().map(str::to_owned),
            pid: Some(std::process::id()),
        };
        for part in parts {
            entry.rows += part.num_rows() as u64;
            if let Some(at) = table.column("build_id")
                && let Some(ids) = part.column(at).as_primitive_opt::<UInt64Type>()
            {
                for id in ids.iter().flatten() {
                    entry.builds = Some(widen(entry.builds, id, id));
                }
            }
            for name in INSTANTS {
                let Some(at) = table.column(name) else { continue };
                let Some(times) = part.column(at).as_primitive_opt::<Int64Type>() else { continue };
                for t in times.iter().flatten() {
                    entry.t_ns = Some(widen(entry.t_ns, t, t));
                }
            }
        }
        entry
    }

    /// The entry of the file at `path`, from its footer: its version from its
    /// `blaze.schema` key, its ranges from its row groups' statistics. A history adopted reads
    /// it once a file; so does a test that writes files with DuckDB, whose files have no
    /// `blaze.schema` (`version` says theirs). A footer with no `blaze.schema` and no
    /// `version` given is [`ManifestError::Unversioned`].
    pub(crate) fn of_footer(path: &Path, version: Option<u32>) -> Result<Entry, HistoryError> {
        let file = fs::File::open(path).map_err(io_error(path, "opening to read its footer"))?;
        let metadata = ParquetMetaDataReader::new().parse_and_finish(&file).map_err(|error| {
            HistoryError::Io {
                // alloc: an error
                path: path.to_owned(),
                doing: "reading its footer",
                error: io::Error::other(error),
            }
        })?;
        // alloc: an error
        let unversioned =
            || HistoryError::Manifest { path: path.to_owned(), why: ManifestError::Unversioned };
        let version = match footer_version(&metadata) {
            Some(it) => it,
            None => version.ok_or_else(unversioned)?,
        };
        let mut entry = Entry {
            version,
            rows: u64::try_from(metadata.file_metadata().num_rows()).unwrap_or(0),
            builds: None,
            t_ns: None,
            boot: None,
            pid: None,
        };
        let schema = metadata.file_metadata().schema_descr();
        let column = |name: &str| {
            (0..schema.num_columns()).find(|&at| schema.column(at).path().string() == name)
        };
        for group in metadata.row_groups() {
            if group.num_rows() == 0 {
                continue;
            }
            if let Some(at) = column("build_id") {
                entry.builds = match group.column(at).statistics() {
                    Some(Statistics::Int64(it))
                        if it.null_count_opt() == Some(group.num_rows() as u64) =>
                    {
                        entry.builds
                    }
                    // A `UBIGINT`, stored as INT64: build ids are far below 2^63.
                    Some(Statistics::Int64(it)) => match (it.min_opt(), it.max_opt()) {
                        (Some(&low), Some(&high)) => {
                            Some(widen(entry.builds, low as u64, high as u64))
                        }
                        _ => Some([0, u64::MAX]),
                    },
                    _ => Some([0, u64::MAX]),
                };
            }
            for name in INSTANTS {
                let Some(at) = column(name) else { continue };
                entry.t_ns = match group.column(at).statistics() {
                    Some(Statistics::Int64(it))
                        if it.null_count_opt() == Some(group.num_rows() as u64) =>
                    {
                        entry.t_ns
                    }
                    Some(Statistics::Int64(it)) => match (it.min_opt(), it.max_opt()) {
                        (Some(&low), Some(&high)) => Some(widen(entry.t_ns, low, high)),
                        _ => Some([i64::MIN, i64::MAX]),
                    },
                    _ => Some([i64::MIN, i64::MAX]),
                };
            }
        }
        Ok(entry)
    }

    /// The entry of a merge of files with `entries`: their rows, the union of their ranges,
    /// and a boot and process only where all of them agree.
    pub(crate) fn merged<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Option<Entry> {
        let mut entries = entries.into_iter();
        // clone: a merge's entry, once a merge
        let mut out = entries.next()?.clone();
        for it in entries {
            out.version = out.version.max(it.version);
            out.rows += it.rows;
            out.builds = match (out.builds, it.builds) {
                (Some(a), Some(b)) => Some(widen(Some(a), b[0], b[1])),
                (a, b) => a.or(b),
            };
            out.t_ns = match (out.t_ns, it.t_ns) {
                (Some(a), Some(b)) => Some(widen(Some(a), b[0], b[1])),
                (a, b) => a.or(b),
            };
            if out.boot != it.boot {
                out.boot = None;
            }
            if out.pid != it.pid {
                out.pid = None;
            }
        }
        Some(out)
    }
}

/// `range` widened to hold `low..=high`.
fn widen<T: Ord + Copy>(range: Option<[T; 2]>, low: T, high: T) -> [T; 2] {
    match range {
        Some([a, b]) => [a.min(low), b.max(high)],
        None => [low, high],
    }
}

/// The `blaze.schema` a file's footer says: a key of its own, or one of its Arrow schema's.
fn footer_version(metadata: &ParquetMetaData) -> Option<u32> {
    let file = metadata.file_metadata();
    let own = file.key_value_metadata().into_iter().flatten().find(|it| it.key == "blaze.schema");
    if let Some(value) = own.and_then(|it| it.value.as_deref()) {
        return value.parse().ok();
    }
    let arrow =
        parquet::arrow::parquet_to_arrow_schema(file.schema_descr(), file.key_value_metadata())
            .ok()?;
    arrow.metadata().get("blaze.schema")?.parse().ok()
}

/// Which files a reader reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Every file listed.
    All,
    /// The files that can hold rows of the build ([`Manifest::of_build`]).
    Build(u64),
}

/// Why a manifest can't be used.
#[derive(Debug)]
pub enum ManifestError {
    Json(serde_json::Error),
    /// Written in a format this blaze does not read.
    Format(u32),
    /// An entry that is no file a manifest lists: not a plain name, not of the table it is
    /// listed under, or (of a table this blaze knows) of no form blaze writes.
    Entry {
        table: String,
        file: String,
    },
    /// A file it lists is not in the directory: something but the history deleted it.
    Missing {
        file: String,
    },
    /// Another process held its lock past [`LOCK_WAIT`].
    Locked,
    /// A file whose footer says no `blaze.schema`: no blaze writer wrote it.
    Unversioned,
    /// Its retired files are not ones a writer retires: `why`.
    Retired(&'static str),
    /// It would list (or lists) `file`, which it retired and a reader may still read: a file
    /// put in its place would change under that reader, and a manifest that lists and retires
    /// one name is no manifest a reader can trust.
    Relisted {
        file: String,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Json(error) => write!(
                f,
                "it is not a manifest's JSON (torn, or not written by blaze): refused, never read as an empty \
                 history (persisted-state-round-trips): {error}"
            ),
            ManifestError::Format(format) => {
                write!(
                    f,
                    "it is of format {format}, and this blaze reads formats {FORMAT_OLDEST} to {FORMAT}"
                )
            }
            ManifestError::Entry { table, file } => write!(
                f,
                "it lists `{file}` under the table {table}, which is no file of that table blaze lists"
            ),
            ManifestError::Missing { file } => write!(
                f,
                "it lists `{file}`, which is not in the directory: something other than the history deleted it, and its rows are gone (delete the directory to start a history afresh)"
            ),
            ManifestError::Locked => write!(
                f,
                "another process held its lock ({LOCK}) for over {LOCK_WAIT:?}, far past one commit"
            ),
            ManifestError::Unversioned => write!(
                f,
                "its footer says no `blaze.schema` version: no blaze writer wrote it, so no reader can say what its rows mean (delete it, or the directory)"
            ),
            ManifestError::Retired(why) => write!(f, "what it says is retired can't be: {why}"),
            ManifestError::Relisted { file } => write!(
                f,
                "it would list `{file}`, which it retired and a reader may still be reading: a file written again takes a new name, never a retired one's"
            ),
        }
    }
}

impl Default for Manifest {
    fn default() -> Manifest {
        Manifest::new()
    }
}

impl Manifest {
    /// A manifest listing nothing.
    pub fn new() -> Manifest {
        // alloc: none until a file is listed
        Manifest {
            format: FORMAT,
            flushed: 0,
            tables: BTreeMap::new(),
            retired: Retired::default(),
            consumed: Consumed::default(),
        }
    }

    /// The number of the last flush the history committed.
    pub fn flushed(&self) -> u64 {
        self.flushed
    }

    /// The history's generation and the files it unlisted and has not yet deleted.
    pub fn retired(&self) -> &Retired {
        &self.retired
    }

    /// What the history consumed of the flight recorders' mappings and the crash records.
    pub fn consumed(&self) -> &Consumed {
        &self.consumed
    }

    /// What this manifest says the history wrote of the mapping of process `pid` with
    /// `nonce`; `None` for a mapping it wrote nothing of (or of another process given that pid).
    pub fn recorder(&self, pid: u32, nonce: u64) -> Option<Recorder> {
        self.consumed.recorders.get(&pid).copied().filter(|it| it.nonce == nonce)
    }

    /// The files of `table`, in name order.
    pub fn files<'a>(&'a self, table: &str) -> impl Iterator<Item = &'a str> + 'a {
        self.tables.get(table).into_iter().flat_map(BTreeMap::keys).map(String::as_str)
    }

    /// The files of `table` with their entries, in name order.
    pub fn entries<'a>(&'a self, table: &str) -> impl Iterator<Item = (&'a str, &'a Entry)> + 'a {
        self.tables.get(table).into_iter().flatten().map(|(name, entry)| (name.as_str(), entry))
    }

    /// Every file it lists, by table, then by name.
    pub fn all(&self) -> impl Iterator<Item = &str> {
        self.tables.values().flat_map(BTreeMap::keys).map(String::as_str)
    }

    /// The entry of the file `name`, when it lists it.
    pub fn entry(&self, name: &str) -> Option<&Entry> {
        self.tables.get(table_of(name)?)?.get(name)
    }

    /// Whether it lists the file `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.entry(name).is_some()
    }

    /// The files of `table` a reader of `scope` reads.
    pub fn scoped<'a>(
        &'a self,
        table: &str,
        scope: Scope,
    ) -> impl Iterator<Item = (&'a str, &'a Entry)> + 'a {
        let window = match scope {
            Scope::All => None,
            Scope::Build(build) => Some((build, self.window_of(build))),
        };
        self.entries(table).filter(move |(_, entry)| match &window {
            None => true,
            Some((build, window)) => holds(entry, *build, window),
        })
    }

    /// The files of `build`: [`Manifest::scoped`] of [`Scope::Build`]. A file can hold rows of
    /// the build when its builds' range holds it, or when its instants overlap the build's
    /// window on the same boot (the rows of no build a build's window takes: `blaze slow`'s
    /// spans of no build, the samples, the counters, the threads' open spans). A file of no
    /// known boot, or whose ranges a footer could not bound, is read: a reader reads more files,
    /// never fewer.
    pub fn of_build(&self, table: &str, build: u64) -> impl Iterator<Item = &str> {
        self.scoped(table, Scope::Build(build)).map(|(name, _)| name)
    }

    /// The window of `build`: on each boot, the earliest and latest instant of the files whose
    /// builds' range is that build alone, widened by a flush ([`super::EVERY`]) at its end, so
    /// the counters the flush after its end took are in it.
    fn window_of(&self, build: u64) -> Vec<(Option<&str>, [i64; 2])> {
        // alloc: a reader's window, once a query
        let mut out: Vec<(Option<&str>, [i64; 2])> = Vec::new();
        for entry in self.tables.values().flat_map(BTreeMap::values) {
            let (Some([low, high]), Some(t)) = (entry.builds, entry.t_ns) else { continue };
            if low != build || high != build {
                continue;
            }
            let boot = entry.boot.as_deref();
            match out.iter_mut().find(|it| it.0 == boot) {
                Some(it) => it.1 = widen(Some(it.1), t[0], t[1]),
                None => out.push((boot, t)),
            }
        }
        let flush = i64::try_from(super::EVERY.as_nanos()).unwrap_or(i64::MAX);
        for it in &mut out {
            it.1[1] = it.1[1].saturating_add(flush);
        }
        out
    }

    /// Lists the file `name`, of a form blaze writes ([`listable`]), with its entry.
    /// A retired name is [`ManifestError::Relisted`].
    pub(crate) fn insert(&mut self, name: &str, entry: Entry) -> Result<(), ManifestError> {
        if self.retired.holds(name) {
            // alloc: an error
            return Err(ManifestError::Relisted { file: name.to_owned() });
        }
        let Some(table) = listable(name) else {
            return Err(ManifestError::Entry {
                // alloc: an error
                table: table_of(name).unwrap_or("of none").to_owned(),
                // alloc: an error
                file: name.to_owned(),
            });
        };
        // alloc: a file's entry, once a file listed
        self.tables.entry(table.name.to_owned()).or_default().insert(name.to_owned(), entry);
        Ok(())
    }

    /// Unlists the file `name`; a table left with no file loses its entry.
    pub(crate) fn remove(&mut self, name: &str) {
        let Some(table) = table_of(name) else { return };
        if let Some(files) = self.tables.get_mut(table) {
            files.remove(name);
            if files.is_empty() {
                self.tables.remove(table);
            }
        }
    }

    /// Unlists each of `names` and retires it, at one new generation ([`Retired::retire`]).
    pub(crate) fn retire<'a>(&mut self, names: impl IntoIterator<Item = &'a str>) {
        // alloc: the names retired, once a commit that retires files
        let names: Vec<String> = names.into_iter().map(str::to_owned).collect();
        for name in &names {
            self.remove(name);
        }
        self.retired.retire(names);
    }

    /// Notes that the flush `seq` committed.
    pub(crate) fn flush_committed(&mut self, seq: u64) {
        self.flushed = self.flushed.max(seq);
    }

    /// Its bytes: deterministic, as each map and set is in order.
    pub(crate) fn encode(&self) -> Result<Vec<u8>, ManifestError> {
        let mut bytes = serde_json::to_vec_pretty(self).map_err(ManifestError::Json)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// The manifest `bytes` hold, each entry checked.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Manifest, ManifestError> {
        let mut manifest: Manifest =
            serde_json::from_slice(bytes).map_err(ManifestError::Json)?;
        if !(FORMAT_OLDEST..=FORMAT).contains(&manifest.format) {
            return Err(ManifestError::Format(manifest.format));
        }
        // Read as this format: it is written so at its next commit.
        manifest.format = FORMAT;
        manifest.retired.check().map_err(ManifestError::Retired)?;
        // By shape alone, as listed files of a table this blaze does not know are taken: a
        // newer blaze's file it merged or capped is retired as any other. Never a schema
        // file, a hidden file or anything not Parquet, nor a reader's pin: a reap deletes it.
        if manifest.retired.files.iter().any(|(it, _)| !retirable(it)) {
            return Err(ManifestError::Retired(
                "a retired file is named as no Parquet file of a table (`<stem>.<table>.parquet`)",
            ));
        }
        if let Some((name, _)) = manifest.retired.files.iter().find(|(it, _)| manifest.contains(it))
        {
            // clone: an error
            return Err(ManifestError::Relisted { file: name.clone() });
        }
        for (table, files) in &manifest.tables {
            let known = schema::TABLES.iter().any(|it| it.name == table.as_str());
            for (file, entry) in files {
                if entry.version == 0 {
                    // clone: an error
                    return Err(ManifestError::Entry { table: table.clone(), file: file.clone() });
                }
                let fits = table_of(file) == Some(table.as_str())
                    && !file.starts_with('.')
                    && !file.contains(['/', '\\'])
                    && (!known || listable(file).is_some());
                if !fits {
                    // clone: an error
                    return Err(ManifestError::Entry { table: table.clone(), file: file.clone() });
                }
            }
        }
        Ok(manifest)
    }
}

/// Whether a file with `entry` can hold rows of `build`, whose window is `window`.
fn holds(entry: &Entry, build: u64, window: &[(Option<&str>, [i64; 2])]) -> bool {
    if entry.builds.is_some_and(|[low, high]| low <= build && build <= high) {
        return true;
    }
    let Some([low, high]) = entry.t_ns else { return false };
    window.iter().any(|(boot, [from, to])| {
        let same_boot = entry.boot.is_none() || boot.is_none() || entry.boot.as_deref() == *boot;
        same_boot && low <= *to && *from <= high
    })
}

/// Whether `name` is shaped as a file a manifest may retire: `<stem>.<table>.parquet` of any
/// table, known or not, and neither hidden nor a schema file ([`kind`]).
fn retirable(name: &str) -> bool {
    table_of(name).is_some()
        && !name.contains(['/', '\\'])
        && matches!(kind(name), Kind::Listable | Kind::Unclassified)
}

/// The table a file's name `<stem>.<table>.parquet` says, whether or not this blaze knows it.
fn table_of(name: &str) -> Option<&str> {
    let (_, table) = name.strip_suffix(".parquet")?.split_once('.')?;
    (!table.is_empty()
        && table.bytes().all(|it| it.is_ascii_lowercase() || it.is_ascii_digit() || it == b'_'))
    .then_some(table)
}

/// The table of `name` when it is a file a manifest lists: a flush's or a merge's, or one of
/// `blaze debug time`'s derived tables (each a [`Name`]), or a perf run's
/// (`perf-<run>.<perf_runs|perf_functions>.parquet`). `None` for any other name, the schema
/// files' among them.
pub fn listable(name: &str) -> Option<&'static schema::Table> {
    if let Some(named) = Name::parse(name) {
        return Some(schema::TABLES[named.table]);
    }
    let (stem, table) = name.strip_suffix(".parquet")?.split_once('.')?;
    let run = stem.strip_prefix("perf-")?;
    let table =
        [&schema::PERF_RUNS, &schema::PERF_FUNCTIONS].into_iter().find(|it| it.name == table)?;
    perf::rows::is_run(run).then_some(table)
}

/// What a file of the history's directory is, by its name.
pub(crate) enum Kind {
    /// A file a manifest lists ([`listable`]).
    Listable,
    /// `schema.<table>.parquet`: no rows, every column; never listed, read by every view.
    Schema,
    /// A hidden file: one being written, or one a crash left half written.
    Hidden,
    /// Not Parquet: the owner's lock, the manifest and its lock, `blaze why`'s ledger, the
    /// flight recorder's map (`flight-<pid>.map`) and the crash records (`crash.<pid>`).
    Other,
    /// Parquet of no form blaze writes here.
    Unclassified,
}

pub(crate) fn kind(name: &str) -> Kind {
    if name.starts_with('.') {
        return Kind::Hidden;
    }
    if !name.ends_with(".parquet") {
        return Kind::Other;
    }
    if listable(name).is_some() {
        return Kind::Listable;
    }
    let schema_of = name.strip_prefix("schema.").and_then(|it| it.strip_suffix(".parquet"));
    if schema_of.is_some_and(|table| !table.is_empty() && !table.contains('.')) {
        return Kind::Schema;
    }
    Kind::Unclassified
}

/// The manifest of the history `dir`, as a reader takes it, with no lock: the manifest is
/// replaced whole, so this is one version of it. A directory with no manifest (a history no
/// blaze with manifests has written to) gives the manifest its files make ([`adopted`]),
/// written by no reader: the history's next writer writes it.
pub fn read(dir: &Path) -> Result<Manifest, HistoryError> {
    let path = dir.join(FILE);
    match fs::read(&path) {
        Ok(bytes) => Manifest::decode(&bytes).map_err(|why| HistoryError::Manifest { path, why }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => adopted(dir),
        Err(error) => Err(io_error(&path, "reading the manifest")(error)),
    }
}

/// The manifest the files of `dir` make, for a history written before manifests. Every Parquet
/// file not hidden and not a schema file is listed, or is [`HistoryError::Unclassified`],
/// except the ones that left the history: those [`LEGACY_RETIRED`] names (a blaze of pins with
/// no manifest retired them; their generations kept), and those a merged file holds the rows
/// of, retired now. None is deleted here: a reader of a blaze before manifests may have it
/// pinned, so it waits for a reap like any retired file.
///
/// The generation starts at the highest that file or any pin in [`super::PINS`] says, never 0:
/// a reader still running that said generation 7 holds only files retired after 7, so a file
/// retired here at 1 would be deleted under it.
fn adopted(dir: &Path) -> Result<Manifest, HistoryError> {
    // alloc: the directory's files, once a history is adopted
    let mut names: Vec<String> = Vec::new();
    for entry in fs::read_dir(dir).map_err(io_error(dir, "listing"))? {
        let entry = entry.map_err(io_error(dir, "listing"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            if name.to_string_lossy().ends_with(".parquet") {
                return Err(HistoryError::Unclassified {
                    // alloc: an error
                    dir: dir.to_owned(),
                    file: name.to_string_lossy().into_owned(),
                });
            }
            continue;
        };
        match kind(name) {
            // alloc: a file's name, once a history is adopted
            Kind::Listable => names.push(name.to_owned()),
            Kind::Unclassified => {
                // alloc: an error
                return Err(HistoryError::Unclassified {
                    dir: dir.to_owned(),
                    file: name.to_owned(),
                });
            }
            Kind::Schema | Kind::Hidden | Kind::Other => {}
        }
    }
    names.sort_unstable();
    let legacy = legacy_retired(dir)?;
    // alloc: once a history is adopted
    let mut retired = Retired {
        generation: legacy.generation.max(super::pin::said_most(dir)?),
        files: Vec::new(),
    };
    for (name, generation) in legacy.files {
        // One gone already was reaped before the writer that retired it stopped.
        if let Ok(at) = names.binary_search(&name) {
            names.remove(at);
            retired.files.push((name, generation));
        }
    }
    // alloc: once a history is adopted
    let parsed: Vec<(usize, Name)> =
        names.iter().enumerate().filter_map(|(at, it)| Some((at, Name::parse(it)?))).collect();
    // alloc: once a history is adopted
    let named: Vec<Name> = parsed.iter().map(|it| it.1).collect();
    // alloc: once a history is adopted
    let gone: BTreeSet<usize> =
        super::compact::superseded(&named).into_iter().map(|at| parsed[at].0).collect();
    let mut manifest = Manifest::new();
    manifest.retired = retired;
    // alloc: once a history is adopted
    let mut superseded = Vec::new();
    for (at, name) in names.into_iter().enumerate() {
        if gone.contains(&at) {
            superseded.push(name);
            continue;
        }
        if let Some(named) = Name::parse(&name) {
            manifest.flush_committed(named.last);
        }
        let entry = Entry::of_footer(&dir.join(&name), None)?;
        manifest
            .insert(&name, entry)
            .map_err(|why| HistoryError::Manifest { path: dir.join(FILE), why })?;
    }
    manifest.retire(superseded.iter().map(String::as_str));
    manifest.retired.check().map_err(|why| HistoryError::Manifest {
        path: dir.join(LEGACY_RETIRED),
        why: ManifestError::Retired(why),
    })?;
    Ok(manifest)
}

/// The file in which a blaze of pins with no manifest (main from 2026-11-04, #1696) kept its
/// generation and retired files. Read once, when its history is adopted, then deleted.
pub const LEGACY_RETIRED: &str = "retired";

/// What [`LEGACY_RETIRED`] says, or nothing retired at generation 0 when it is not there.
fn legacy_retired(dir: &Path) -> Result<Retired, HistoryError> {
    const HEADER: &str = "blaze profile history retired v1";
    let path = dir.join(LEGACY_RETIRED);
    let text = match fs::read_to_string(&path) {
        Ok(it) => it,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Retired::default()),
        Err(error) => {
            return Err(
                io_error(&path, "reading the retired files of a history before manifests")(error),
            );
        }
    };
    let refused =
        |why| HistoryError::Manifest { path: path.clone(), why: ManifestError::Retired(why) };
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return Err(refused("its first line is not `blaze profile history retired v1`"));
    }
    let generation = lines
        .next()
        .and_then(|it| it.strip_prefix("generation "))
        .and_then(|it| it.parse().ok())
        .ok_or_else(|| refused("its second line is not `generation <number>`"))?;
    // alloc: once a history is adopted
    let mut files = Vec::new();
    for line in lines {
        let (at, name) =
            line.split_once(' ').ok_or_else(|| refused("a line is not `<generation> <file>`"))?;
        let at = at.parse().map_err(|_| refused("a line's generation is not a number"))?;
        if listable(name).is_none() {
            return Err(refused("a line names no file of a form blaze lists"));
        }
        // alloc: a file's name, once a history is adopted
        files.push((name.to_owned(), at));
    }
    let retired = Retired { generation, files };
    retired.check().map_err(refused)?;
    Ok(retired)
}

/// The manifest of a history, held under its lock by a writer: changed in memory, then
/// [`Held::commit`]ted. The lock is let go of as this drops.
pub(crate) struct Held {
    path: PathBuf,
    manifest: Manifest,
    /// The manifest as its file holds it: a commit writes only a change.
    written: Manifest,
    _lock: Locked,
}

/// The manifest of the history `dir`, its lock held, the directory made if it is not there. A
/// directory with no manifest has one made of its files ([`adopted`]) and written now, and the
/// files a merged file holds the rows of deleted.
pub(crate) fn hold(dir: &Path) -> Result<Held, HistoryError> {
    fs::create_dir_all(dir).map_err(io_error(dir, "making the directory"))?;
    let lock = lock(dir)?;
    let path = dir.join(FILE);
    let (manifest, adopting) = match fs::read(&path) {
        Ok(bytes) => {
            // clone: an error
            let manifest = Manifest::decode(&bytes)
                .map_err(|why| HistoryError::Manifest { path: path.clone(), why })?;
            (manifest, false)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (adopted(dir)?, true),
        Err(error) => return Err(io_error(&path, "reading the manifest")(error)),
    };
    let written = match adopting {
        // The manifest the directory's files make is written once, so the next open of the
        // history sweeps against it.
        true => Manifest { format: 0, ..Manifest::new() },
        // clone: the manifest, once a commit
        false => manifest.clone(),
    };
    // alloc: once a commit
    let mut held = Held { path, manifest, written, _lock: lock };
    held.commit()?;
    if adopting {
        // The manifest now says what it said; a reader reads the manifest, never it.
        let legacy = dir.join(LEGACY_RETIRED);
        match fs::remove_file(&legacy) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(io_error(
                    &legacy,
                    "deleting the retired files of a history before manifests, adopted",
                )(error));
            }
        }
    }
    Ok(held)
}

impl Held {
    pub(crate) fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Lists the file `name` of the history's directory, with its entry.
    pub(crate) fn add(&mut self, name: &str, entry: Entry) -> Result<(), HistoryError> {
        // clone: an error
        self.manifest
            .insert(name, entry)
            .map_err(|why| HistoryError::Manifest { path: self.path.clone(), why })
    }

    /// Unlists each of `names` and retires it ([`Manifest::retire`]): deleted once no pin holds
    /// it ([`Held::reaped`]).
    pub(crate) fn retire<'a>(&mut self, names: impl IntoIterator<Item = &'a str>) {
        self.manifest.retire(names);
    }

    /// Forgets the retired files at `at` (positions in [`Retired::files`], ascending), deleted.
    pub(crate) fn reaped(&mut self, at: &[usize]) {
        self.manifest.retired.reaped(at);
    }

    pub(crate) fn flush_committed(&mut self, seq: u64) {
        self.manifest.flush_committed(seq);
    }

    /// Notes that this commit holds the rows of flush `seq` of the process `pid` whose
    /// flight recorder mapping has `nonce`: a postmortem of it reads past that flush's marks.
    pub(crate) fn recorder_flushed(&mut self, pid: u32, nonce: u64, seq: u64) {
        let recorders = &mut self.manifest.consumed.recorders;
        match recorders.get_mut(&pid) {
            Some(it) if it.nonce == nonce => it.flushed = it.flushed.max(seq),
            _ => _ = recorders.insert(pid, Recorder { nonce, flushed: seq, recovered: false }),
        }
    }

    /// Notes that this commit holds the rows a postmortem read back of the mapping of `pid`
    /// with `nonce`: the next to find it deletes it unread.
    pub(crate) fn recorder_recovered(&mut self, pid: u32, nonce: u64) {
        let flushed = self.manifest.recorder(pid, nonce).map_or(0, |it| it.flushed);
        self.manifest.consumed.recorders.insert(pid, Recorder { nonce, flushed, recovered: true });
    }

    /// Notes that this commit holds the `panics` rows of the crash records `ids` (each its
    /// process and the instant of its signal).
    pub(crate) fn crashes_written(&mut self, ids: impl IntoIterator<Item = (u32, i64)>) {
        self.manifest.consumed.crashes.extend(ids);
    }

    /// Forgets what was consumed of a mapping or crash record no longer in `dir`: nothing can
    /// read it again. One whose presence can't be told is kept, which costs nothing.
    pub(crate) fn prune_consumed(&mut self, dir: &Path) {
        let gone = |name: String| matches!(dir.join(name).try_exists(), Ok(false));
        let consumed = &mut self.manifest.consumed;
        consumed.recorders.retain(|&pid, _| !gone(crate::flight::postmortem::file_name(pid)));
        consumed.crashes.retain(|&(pid, _)| !gone(crate::crash::file_name(pid)));
    }

    /// Writes the manifest when it changed: to a hidden name, then renamed over the last.
    pub(crate) fn commit(&mut self) -> Result<(), HistoryError> {
        if self.manifest == self.written {
            return Ok(());
        }
        // clone: an error
        let bytes = self
            .manifest
            .encode()
            .map_err(|why| HistoryError::Manifest { path: self.path.clone(), why })?;
        put(&self.path, &bytes)?;
        // clone: the manifest, once a commit
        self.written = self.manifest.clone();
        Ok(())
    }
}

/// The exclusive lock of `dir`'s manifest, waiting at most [`LOCK_WAIT`] for another process
/// to let go of it ([`super::lock_within`]).
fn lock(dir: &Path) -> Result<Locked, HistoryError> {
    let path = dir.join(LOCK);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(io_error(&path, "opening the manifest's lock"))?;
    let file = match Locked::try_lock(file) {
        Ok(Ok(locked)) => return Ok(locked),
        Ok(Err(back)) => back,
        Err(error) => return Err(io_error(&path, "locking the manifest's lock")(error)),
    };
    let _span = tracing::info_span!(target: "blaze", "waits on the profile history's manifest lock", dir = %dir.display())
        .entered();
    match super::lock_within(file, LOCK_WAIT) {
        Ok(Ok(locked)) => Ok(locked),
        Ok(Err(_held)) => Err(HistoryError::Manifest { path, why: ManifestError::Locked }),
        Err(error) => Err(io_error(&path, "locking the manifest's lock")(error)),
    }
}

/// Lists each Parquet file `dir` holds that its manifest does not and that is of a form blaze
/// writes: what a test that writes a history's files with DuckDB does, so its files are read.
#[cfg(test)]
pub(crate) fn list_written(dir: &Path) {
    let mut held = hold(dir).unwrap();
    for entry in fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if matches!(kind(&name), Kind::Listable) && !held.manifest().contains(&name) {
            let entry = Entry::of_footer(&dir.join(&name), Some(schema::VERSION)).unwrap();
            held.add(&name, entry).unwrap();
        }
    }
    held.commit().unwrap();
}

#[cfg(test)]
mod consumed {
    use super::*;

    /// persisted-state-round-trips: what a commit consumed reads back as written, byte for
    /// byte, and a manifest of the format before it reads as one that consumed nothing.
    #[test]
    fn a_manifest_reads_back_what_it_consumed_byte_for_byte() {
        let mut manifest = Manifest::new();
        manifest.flushed = 12;
        manifest.consumed.recorders.insert(7, Recorder { nonce: 3, flushed: 12, recovered: false });
        manifest.consumed.recorders.insert(9, Recorder { nonce: 5, flushed: 0, recovered: true });
        manifest.consumed.crashes.insert((11, -4));
        let bytes = manifest.encode().unwrap();
        let read = Manifest::decode(&bytes).unwrap();
        assert_eq!(read, manifest);
        assert_eq!(read.encode().unwrap(), bytes, "the encoding is deterministic");
        assert_eq!(read.recorder(7, 3).map(|it| it.flushed), Some(12));
        assert!(read.recorder(7, 4).is_none(), "another mapping of the same pid is not this one");
        assert!(read.recorder(9, 5).is_some_and(|it| it.recovered));

        let old = Manifest::decode(br#"{"format":1,"flushed":4,"tables":{}}"#).unwrap();
        assert_eq!(old.format, FORMAT, "read as this format, written so at its next commit");
        assert!(old.consumed().is_empty());
    }
}
