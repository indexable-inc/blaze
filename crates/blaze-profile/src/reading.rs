//! The last reading of a process's open spans in a history ([`last`]), read from its `open`
//! files without DuckDB: what the CLI says a daemon that died was inside, as `blaze slow` says
//! it of a build its daemon's death ended ([`crate::slow::Died`]).

use std::{
    alloc::Global,
    fmt, fs,
    path::{Path, PathBuf},
};

use blaze_parquet::ParquetError;

use crate::{
    columns::{Batch, Column},
    history::{HistoryError, Pin},
    schema,
    slow::{DiedSpan, DiedThread},
};

/// How many of the newest `open` files are looked in: a flush writes one only when the threads'
/// open spans changed, so a daemon's last reading is among the newest unless others wrote since.
pub const FILES: usize = 64;

/// A reading of a process's threads' open spans.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reading {
    /// When it was read, on the history's clock.
    pub read_ns: i64,
    /// Each thread inside a span then, in the order read; each span's `for_ns` up to the reading.
    pub threads: Vec<DiedThread>,
}

#[derive(Debug)]
pub enum ReadingError {
    Io {
        path: PathBuf,
        doing: &'static str,
        error: std::io::Error,
    },
    Parquet {
        path: PathBuf,
        error: ParquetError,
    },
    /// A column of an `open` file is missing or of another type.
    Column {
        path: PathBuf,
        column: &'static str,
    },
    /// The history could not be pinned and its manifest read ([`Pin`]).
    History(HistoryError),
}

impl fmt::Display for ReadingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadingError::Io { path, doing, error } => {
                write!(f, "the profile history: {doing} {}: {error}", path.display())
            }
            ReadingError::Parquet { path, error } => {
                write!(f, "the profile history: reading {}: {error}", path.display())
            }
            ReadingError::Column { path, column } => {
                write!(
                    f,
                    "the profile history: {} has no column `{column}` of the type the open table has",
                    path.display()
                )
            }
            ReadingError::History(error) => {
                write!(f, "pinning the history to read its last open spans: {error}")
            }
        }
    }
}

impl std::error::Error for ReadingError {}

/// The last reading of the open spans of the process `pid` in the history `dir`, among the
/// newest [`FILES`] `open` files its manifest lists (by their latest instant, then name);
/// `None` where they hold none of it.
pub fn last(dir: &Path, pid: u32) -> Result<Option<Reading>, ReadingError> {
    // Held to the end: a merge or the cap retires `open` files, and none the manifest it read
    // lists is deleted meanwhile.
    let pin = Pin::take(dir).map_err(ReadingError::History)?;
    let manifest = pin.manifest();
    // alloc: the history's open files, once a reading
    let mut files: Vec<(i64, &str)> = manifest
        .entries(schema::OPEN.name)
        .filter(|(_, entry)| entry.pid.is_none_or(|it| it == pid))
        .map(|(name, entry)| (entry.t_ns.map_or(i64::MIN, |it| it[1]), name))
        .collect();
    files.sort_unstable_by(|a, b| b.cmp(a));
    // alloc: the files' paths, once a reading
    let files: Vec<PathBuf> = files.iter().take(FILES).map(|(_, name)| dir.join(name)).collect();
    let mut best: Option<Reading> = None;
    for path in &files {
        if let Some(reading) = of_file(path, pid)?
            && best.as_ref().is_none_or(|it| reading.read_ns > it.read_ns)
        {
            best = Some(reading);
        }
    }
    Ok(best)
}

/// The last reading of `pid` in the `open` file `path`.
fn of_file(path: &Path, pid: u32) -> Result<Option<Reading>, ReadingError> {
    // alloc: one open file's bytes, once a file a reading
    let bytes = fs::read(path).map_err(|error| ReadingError::Io {
        path: path.to_owned(),
        doing: "reading",
        error,
    })?;
    let parquet = |error| ReadingError::Parquet { path: path.to_owned(), error };
    let file = blaze_parquet::File::open(&bytes, Global).map_err(parquet)?;
    let mut out: Option<Reading> = None;
    for batch in &file.read_all().map_err(parquet)? {
        rows(batch, path, pid, &mut out)?;
    }
    Ok(out)
}

fn rows(batch: &Batch, path: &Path, pid: u32, out: &mut Option<Reading>) -> Result<(), ReadingError> {
    let column = |column: &'static str| {
        batch.column_by_name(column).ok_or(ReadingError::Column { path: path.to_owned(), column })
    };
    let typed = |column: &'static str| ReadingError::Column { path: path.to_owned(), column };
    let process = column("process")?.u32s().ok_or_else(|| typed("process"))?;
    let thread = column("thread")?.u32s().ok_or_else(|| typed("thread"))?;
    let read = column("read_ns")?.i64s().ok_or_else(|| typed("read_ns"))?;
    let start = column("start_ns")?.i64s().ok_or_else(|| typed("start_ns"))?;
    let owner = column("owner")?.u64s().ok_or_else(|| typed("owner"))?;
    let (thread_name, name, function, awaits) =
        (column("thread_name")?, column("name")?, column("function")?, column("awaits")?);
    for row in 0..batch.num_rows() {
        if process.is_null(row) || process.value(row) != pid || read.is_null(row) {
            continue;
        }
        let read_ns = read.value(row);
        match out {
            Some(it) if it.read_ns > read_ns => continue,
            Some(it) if it.read_ns == read_ns => {}
            _ => *out = Some(Reading { read_ns, threads: Vec::new() }), // alloc: once a reading
        }
        let Some(reading) = out else { continue };
        let tid = thread.value(row);
        if reading.threads.last().is_none_or(|it| it.thread != tid) {
            // alloc: once a thread a reading
            reading.threads.push(DiedThread {
                thread: tid,
                name: text(thread_name, row),
                spans: Vec::new(),
            });
        }
        let span = DiedSpan {
            name: text(name, row).unwrap_or_default(),
            function: text(function, row),
            awaits: text(awaits, row),
            owner: owner.get(row),
            for_ns: start.get(row).map_or(0, |start| read_ns - start),
        };
        if let Some(last) = reading.threads.last_mut() {
            last.spans.push(span);
        }
    }
    Ok(())
}

/// A text cell, of a plain or a dictionary column.
fn text(column: &Column, row: usize) -> Option<String> {
    column.text(row).map(str::to_owned) // alloc: a cell
}
