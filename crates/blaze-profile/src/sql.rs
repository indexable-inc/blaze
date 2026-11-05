//! `blaze debug sql`: a query over the history, run by the pinned DuckDB CLI (`nix run
//! .#duckdb`'s package; DuckDB is never linked into blaze), its answer read back as Arrow.
//!
//! DuckDB's CLI has no Arrow output without extensions, and nothing here downloads one, so
//! the query runs as `COPY (<query>) TO '<scratch>/answer.parquet'` after the views of
//! [`views::PRELUDE`], and the file is read back with the `parquet` crate.
//!
//! The views read the files the history's manifest lists, as it is when the query starts
//! ([`history::manifest`]): every row the history committed, each once. Rows the daemon has
//! recorded and not yet flushed are in no file; a reader that must see them asks the daemon
//! to flush first (`blaze_api::Request::Flush`, which the CLI's `history_source` sends), and
//! the flush's manifest holds them. One path, so nothing is read twice: until 2026-11-04,
//! `--live` wrote the recorder's last seconds beside the history, and the rows of those
//! seconds a flush had also written were read twice.
//!
//! The history is pinned ([`history::Pin`]) before its manifest is read, and stays pinned
//! until DuckDB has exited: the views name the files that manifest lists one by one, and the
//! history's writer deletes none of them meanwhile, though a merge or the cap retires them.

use std::{
    fmt,
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use arrow_array::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

pub use crate::history::manifest::Scope;
use crate::{
    history::{self, HistoryError, Pin, manifest},
    views::{self, ViewError},
};

/// What a query runs over.
#[derive(Clone, Copy, Debug)]
pub struct Query<'a> {
    /// The DuckDB CLI to run: an absolute path, from the caller (`nix run .#duckdb`'s
    /// package); never looked up on `PATH`.
    pub duckdb: &'a Path,
    /// The history's directory ([`history::History::dir`]).
    pub history: &'a Path,
    /// Which of its files the views read: every one, or a build's ([`Scope::Build`]), which
    /// reads the footers of a build's files and not of every file.
    pub scope: Scope,
    /// One query: a `SELECT`, `WITH`, or anything else `COPY (...)` takes.
    pub sql: &'a str,
}

#[derive(Debug)]
pub enum SqlError {
    /// The DuckDB given is not an absolute path to a file.
    NoDuckdb {
        path: PathBuf,
        why: String,
    },
    Views(ViewError),
    /// A directory the views read lacks a table's schema file that could not be written
    /// ([`history::ensure_schemas`]), or its manifest can't be read.
    History(HistoryError),
    Io {
        path: PathBuf,
        doing: &'static str,
        error: io::Error,
    },
    /// DuckDB refused the query: its own message, the query, and the script it ran (kept for
    /// reading).
    Duckdb {
        message: String,
        sql: String,
        script: PathBuf,
    },
    /// DuckDB's answer did not read back.
    Answer {
        path: PathBuf,
        error: String,
    },
    /// The caller stopped the query ([`run_with`]); DuckDB was killed.
    Stopped,
}

impl SqlError {
    /// The error's kind on the explorer's wire (`explore-wire.md`).
    pub fn kind(&self) -> &'static str {
        match self {
            SqlError::Views(error) => error.kind(),
            SqlError::Stopped => "cancelled",
            _ => "sql",
        }
    }
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlError::NoDuckdb { path, why } => write!(
                f,
                "blaze debug sql runs the DuckDB `nix run .#duckdb` pins, and {} {why}",
                path.display()
            ),
            SqlError::Views(error) => write!(f, "{error}"),
            SqlError::History(error) => write!(f, "{error}"),
            SqlError::Io { path, doing, error } => {
                write!(f, "blaze debug sql: {doing} {}: {error}", path.display())
            }
            SqlError::Duckdb { message, sql, script } => write!(
                f,
                "DuckDB refused the query: {message}\nthe query:\n{sql}\nthe script DuckDB ran: {}",
                script.display()
            ),
            SqlError::Answer { path, error } => {
                write!(f, "reading DuckDB's answer {}: {error}", path.display())
            }
            SqlError::Stopped => f.write_str("the query was stopped before DuckDB answered"),
        }
    }
}

impl std::error::Error for SqlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SqlError::Views(error) => Some(error),
            SqlError::History(error) => Some(error),
            SqlError::Io { error, .. } => Some(error),
            _ => None,
        }
    }
}

impl From<ViewError> for SqlError {
    fn from(error: ViewError) -> SqlError {
        SqlError::Views(error)
    }
}

fn io_error<'a>(path: &'a Path, doing: &'static str) -> impl FnOnce(io::Error) -> SqlError + 'a {
    // alloc: an error
    move |error| SqlError::Io { path: path.to_owned(), doing, error }
}

/// A directory of one query's files, deleted when the query succeeds; kept when it fails, so
/// the error's script can be read.
struct Scratch {
    dir: PathBuf,
    keep: bool,
}

impl Scratch {
    fn new() -> Result<Scratch, SqlError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        // alloc: a query's directory, once a query
        let dir = std::env::temp_dir().join(format!("blaze-sql-{}-{n}", std::process::id()));
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(&dir, "clearing the query's directory")(error)),
        }
        fs::create_dir_all(&dir).map_err(io_error(&dir, "making the query's directory"))?;
        Ok(Scratch { dir, keep: false })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.keep {
            // A directory under the system's temporary one that is left behind is only space.
            _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// What stops a query under way: a message on it, or every sender of it gone.
pub type Stop = crossbeam_channel::Receiver<()>;

/// Runs `query` over the history; its answer as Arrow record batches.
pub fn run(query: &Query<'_>) -> Result<Vec<RecordBatch>, SqlError> {
    run_with(query, &crossbeam_channel::never())
}

/// [`run`], killing DuckDB and answering [`SqlError::Stopped`] once `stop` says to: what the
/// explorer does when the page cancels a query.
pub fn run_with(query: &Query<'_>, stop: &Stop) -> Result<Vec<RecordBatch>, SqlError> {
    let mut scratch = Scratch::new()?;
    let answer = run_in(&scratch.dir, query.duckdb, &[query.sql], query.history, query.scope, stop)
        .map(only);
    scratch.keep = answer.as_ref().is_err_and(|it| !matches!(it, SqlError::Stopped));
    answer
}

/// Runs each of `sqls` over the files of `scope` of the history at `history`, in one DuckDB:
/// their answers, in order. The views are made once for all of them, and making them reads the
/// footer of every file they read (17 CPU-seconds and 0.7 GB over the 6,000 files of one home
/// with [`Scope::All`], 2026-11-04; a build's files with [`Scope::Build`]), so a reader with
/// several queries that do not hang on each other's answers asks them together.
pub fn run_all(
    duckdb: &Path,
    history: &Path,
    scope: Scope,
    sqls: &[&str],
) -> Result<Vec<Vec<RecordBatch>>, SqlError> {
    if sqls.is_empty() {
        // alloc: nothing, no query asked
        return Ok(Vec::new());
    }
    let mut scratch = Scratch::new()?;
    let answers = run_in(&scratch.dir, duckdb, sqls, history, scope, &crossbeam_channel::never());
    scratch.keep = answers.is_err();
    answers
}

/// The answer of a run of one query.
fn only(mut answers: Vec<Vec<RecordBatch>>) -> Vec<RecordBatch> {
    answers.pop().unwrap_or_default()
}

/// Waits for `child` to end, or for `stop`: its exit status, or `None` once it is killed for
/// `stop`. A thread of its own hears the exit without reaping the child, so the child's
/// number stays its own until `child` reaps it here, and a kill can reach no other process.
#[cfg(any(unix, windows))]
fn ended_or_stopped(child: &mut Child, stop: &Stop) -> io::Result<Option<ExitStatus>> {
    #[cfg(unix)]
    let watched = child.id();
    #[cfg(windows)]
    let watched = std::os::windows::io::AsRawHandle::as_raw_handle(child) as usize;
    let (exited, heard) = crossbeam_channel::bounded::<io::Result<()>>(1);
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            // alloc: a thread's name, once a query
            .name("duckdb exit".to_owned())
            .spawn_scoped(scope, move || _ = exited.send(exit_heard(watched)))?;
        crossbeam_channel::select! {
            recv(heard) -> seen => {
                seen.map_err(|_| io::Error::other("the thread that waits for DuckDB's exit ended saying nothing"))??;
                child.wait().map(Some)
            }
            recv(stop) -> _ => {
                // One already gone is fine: either way it is reaped by the wait.
                _ = child.kill();
                child.wait().map(|_| None)
            }
        }
    })
}

/// WASI (the wasm32-wasip1 seed) has no processes, so no DuckDB child can be waited for: an
/// error naming the call, never a wait that can't end.
#[cfg(target_os = "wasi")]
fn ended_or_stopped(_child: &mut Child, _stop: &Stop) -> io::Result<Option<ExitStatus>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "DuckDB: waiting for a child's exit is refused on WASI (the wasm32-wasip1 seed): no processes",
    ))
}

/// Waits until the child `pid` has ended, leaving it to be reaped by its `Child`.
#[cfg(unix)]
fn exit_heard(pid: u32) -> io::Result<()> {
    let pid = libc::id_t::try_from(pid).map_err(io::Error::other)?;
    loop {
        // SAFETY: all zeros is a `siginfo_t` of nothing, for the call to write over.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is live for the call to write. `WNOWAIT` leaves the child unreaped, so
        // its number names it alone until its `Child` waits for it.
        if unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) } == 0
        {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Waits until the process of `handle` has ended.
#[cfg(windows)]
#[expect(
    clippy::disallowed_methods,
    reason = "a child's exit on a scoped thread of its own, no pool thread or fiber; blocking child waits move to the driver in step 5 of docs/v2/plan/design/deadline-waits.md"
)]
fn exit_heard(handle: usize) -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::WAIT_OBJECT_0,
        System::Threading::{INFINITE, WaitForSingleObject},
    };
    // SAFETY: the process handle of a `Child` that outlives this call: `ended_or_stopped` waits
    // on a scoped thread, and its `Child` is borrowed for the whole scope.
    match unsafe { WaitForSingleObject(handle as windows_sys::Win32::Foundation::HANDLE, INFINITE) }
    {
        WAIT_OBJECT_0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// DuckDB's threads for one query. Left alone, DuckDB takes every core: a `blaze slow` on
/// 2026-11-04 ran it at about 1,750% CPU and froze other work on the shared Mac. Reads of the
/// history share the machine with builds, so they are capped.
pub const THREADS: u32 = 8;

/// DuckDB's memory for one query. Left alone, DuckDB takes 80% of RAM (a `blaze slow` reached
/// 22 GB in one query, 2026-11-03). Past this, an operator spills to the query's scratch
/// directory ([`script`]'s `temp_directory`), and one that can't spill fails the query with
/// DuckDB's error naming it: never a quiet second route.
pub const MEMORY_LIMIT: &str = "4GB";

/// The DuckDB script of `sqls` over the files of `scope` the manifest of the history `history`
/// lists: no extension installed, DuckDB's threads and memory capped ([`THREADS`],
/// [`MEMORY_LIMIT`]) with spills in `scratch`, the views, then each query copied to its answer
/// in `scratch` as Parquet ([`answer_in`]).
fn script(
    sqls: &[&str],
    history: &Path,
    manifest: &manifest::Manifest,
    scope: Scope,
    scratch: &Path,
) -> Result<String, SqlError> {
    let prelude = views::prelude(history, manifest, scope)?;
    // In the scratch directory: removed with the query's other files.
    let spill = scratch.join("spill");
    let spill = spill.to_str().ok_or_else(|| SqlError::Answer {
        // alloc: an error
        path: spill.clone(),
        // alloc: an error
        error: "the query's spill directory is not UTF-8, and the script DuckDB runs names it"
            .to_owned(),
    })?;
    // alloc: the script, once a query
    let mut out = String::with_capacity(
        prelude.len() + sqls.iter().map(|it| it.len() + 256).sum::<usize>() + 256,
    );
    out.push_str("SET autoinstall_known_extensions = false;\nSET enable_progress_bar = false;\n");
    // Writing to a `String` does not fail.
    _ = writeln!(out, "SET threads = {THREADS};\nSET memory_limit = '{MEMORY_LIMIT}';");
    out.push_str("SET temp_directory = ");
    views::quote(spill, &mut out);
    out.push_str(";\n");
    // A query that reads a file another read already reads its footer from memory.
    out.push_str("SET parquet_metadata_cache = true;\n");
    out.push_str(&prelude);
    for (i, sql) in sqls.iter().enumerate() {
        let answer = answer_in(scratch, i);
        let answer = answer.to_str().ok_or_else(|| SqlError::Answer {
            // clone: an error
            path: answer.clone(),
            // alloc: an error
            error: "its path is not UTF-8, and the script DuckDB runs names it".to_owned(),
        })?;
        let sql = sql.trim().trim_end_matches(';').trim_end();
        out.push_str("\nCOPY (\n");
        out.push_str(sql);
        out.push_str("\n) TO ");
        views::quote(answer, &mut out);
        out.push_str(" (FORMAT parquet, COMPRESSION zstd);\n");
    }
    Ok(out)
}

/// Where the answer of the `i`th query of a script goes.
fn answer_in(scratch: &Path, i: usize) -> PathBuf {
    if i == 0 {
        scratch.join("answer.parquet")
    } else {
        // alloc: a path, once a query
        scratch.join(format!("answer-{i}.parquet"))
    }
}

fn run_in(
    scratch: &Path,
    duckdb: &Path,
    sqls: &[&str],
    history: &Path,
    scope: Scope,
    stop: &Stop,
) -> Result<Vec<Vec<RecordBatch>>, SqlError> {
    let no_duckdb = |why: &str| SqlError::NoDuckdb {
        // alloc: an error
        path: duckdb.to_owned(),
        // alloc: an error
        why: why.to_owned(),
    };
    if !duckdb.is_absolute() {
        return Err(no_duckdb("is not an absolute path: it is never looked up on PATH"));
    }
    match fs::metadata(duckdb) {
        Ok(it) if it.is_file() => {}
        Ok(_) => return Err(no_duckdb("is not a file")),
        // alloc: an error
        Err(error) => return Err(no_duckdb(&format!("can't be read: {error}"))),
    }
    // Each view's list leads with its table's schema file, so the history gets the schema file
    // of every table before a view opens over it. Then the pin, which reads the manifest once:
    // the files the views read are the ones it lists, and the pin, held until DuckDB has exited
    // (dropped at the end of this function, on every path), keeps each on disk until then,
    // though a merge or the cap retires it meanwhile.
    history::ensure_schemas(history).map_err(SqlError::History)?;
    let pin = Pin::take(history).map_err(SqlError::History)?;
    let script_path = scratch.join("query.sql");
    let init = scratch.join("init.sql");
    fs::write(&script_path, script(sqls, history, pin.manifest(), scope, scratch)?)
        .map_err(io_error(&script_path, "writing"))?;
    // An empty init file, so the user's ~/.duckdbrc does not change what the query means.
    fs::write(&init, b"").map_err(io_error(&init, "writing"))?;
    let stdin = fs::File::open(&script_path).map_err(io_error(&script_path, "opening"))?;
    // To a file, not a pipe: the wait is for the exit or `stop`, so nothing drains a pipe meanwhile.
    let stderr_path = scratch.join("stderr");
    let stderr_file = fs::File::create(&stderr_path).map_err(io_error(&stderr_path, "making"))?;
    let mut command = Command::new(duckdb);
    command
        .arg("-batch")
        .arg("-bail")
        .arg("-init")
        .arg(&init)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file));
    // A DuckDB just written (a fresh download, a test's stand-in) can be busy for the moment a
    // process another thread forked holds the writer's copy: its exec is made again, as every
    // spawn of blaze's is.
    let mut child =
        blaze_core::busy::while_busy(|| command.spawn()).map_err(io_error(duckdb, "running"))?;
    let waited = ended_or_stopped(&mut child, stop);
    if waited.is_err() {
        // Not heard to end: killed and reaped before the pin goes, so it reads no file after.
        _ = child.kill();
        _ = child.wait();
    }
    let Some(status) = waited.map_err(io_error(duckdb, "waiting for"))? else {
        return Err(SqlError::Stopped);
    };
    // DuckDB has exited (or was killed and reaped): it reads no file of the pin any more.
    drop(pin);
    let stderr = fs::read(&stderr_path).map_err(io_error(&stderr_path, "reading"))?;
    let refused = |message: String| SqlError::Duckdb {
        message,
        // alloc: an error
        sql: sqls.join(";\n"),
        // clone: an error
        script: script_path.clone(),
    };
    // alloc: DuckDB's message, once a query
    let stderr = String::from_utf8_lossy(&stderr);
    if !status.success() {
        // alloc: an error
        return Err(refused(format!("{} ({status})", stderr.trim())));
    }
    // alloc: the answers, once a script
    let mut answers = Vec::with_capacity(sqls.len());
    for i in 0..sqls.len() {
        let answer = answer_in(scratch, i);
        if !answer.exists() {
            // alloc: an error
            return Err(refused(format!(
                "it wrote no answer to query {} of {}, and said: {}",
                i + 1,
                sqls.len(),
                stderr.trim()
            )));
        }
        answers.push(read_answer(&answer)?);
    }
    Ok(answers)
}

/// DuckDB's answer at `answer`, as Arrow record batches.
fn read_answer(answer: &Path) -> Result<Vec<RecordBatch>, SqlError> {
    let answer_error = |error: String| SqlError::Answer {
        // alloc: an error
        path: answer.to_owned(),
        error,
    };
    let file = fs::File::open(answer).map_err(io_error(answer, "opening DuckDB's answer"))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .and_then(|it| it.build())
        // alloc: an error
        .map_err(|error| answer_error(error.to_string()))?;
    // alloc: the answer, once a query
    let mut batches = Vec::new();
    for batch in reader {
        // alloc: an error
        batches.push(batch.map_err(|error| answer_error(error.to_string()))?);
    }
    Ok(batches)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use arrow_array::cast::AsArray;
    use serde_json::{Map, Value};

    use super::*;
    use crate::{
        flight,
        history::{BuildRow, CAP_BYTES, History, HistoryError, Outcome},
        schema,
    };

    #[test]
    fn the_script_installs_nothing_and_copies_one_query_out() {
        let mut listed = manifest::Manifest::new();
        let entry = manifest::Entry {
            version: schema::VERSION,
            rows: 1,
            builds: Some([1, 1]),
            t_ns: None,
            boot: None,
            pid: None,
        };
        listed.insert("1-00000001.spans.parquet", entry).unwrap();
        let script = script(
            &["SELECT 1 ;\n", "SELECT 2"],
            Path::new("/h"),
            &listed,
            Scope::All,
            Path::new("/tmp/it's"),
        )
        .unwrap();
        assert!(script.starts_with("SET autoinstall_known_extensions = false;"));
        assert!(script.contains("SET threads = 8;\nSET memory_limit = '4GB';\nSET temp_directory = '/tmp/it''s/spill';\n"), "{script}");
        assert!(script.contains("read_parquet(['/h/schema.spans.parquet', '/h/1-00000001.spans.parquet'], union_by_name = true)"));
        assert!(
            script.ends_with(
                "COPY (\nSELECT 1\n) TO '/tmp/it''s/answer.parquet' (FORMAT parquet, COMPRESSION zstd);\n\
                 \nCOPY (\nSELECT 2\n) TO '/tmp/it''s/answer-1.parquet' (FORMAT parquet, COMPRESSION zstd);\n"
            ),
            "{script}"
        );
    }

    #[test]
    fn a_stopped_query_kills_duckdb_and_says_so() {
        // A stand-in for DuckDB that never answers; the query must end anyway.
        let dir = std::env::temp_dir().join(format!("blaze-sql-stop-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("duckdb");
        fs::write(&fake, "#!/bin/sh\nexec sleep 600\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (stopper, stop) = crossbeam_channel::bounded(1);
        let started = std::time::Instant::now();
        let query = Query { duckdb: &fake, history: &dir, scope: Scope::All, sql: "SELECT 1" };
        let error = std::thread::scope(|scope| {
            scope.spawn(move || {
                blaze_graph::spin::busy(Duration::from_millis(50));
                stopper.send(()).unwrap();
            });
            run_with(&query, &stop).unwrap_err()
        });
        assert!(matches!(error, SqlError::Stopped), "{error}");
        assert_eq!(error.kind(), "cancelled");
        assert!(started.elapsed() < Duration::from_secs(5), "stopped at its stop, not a limit");
        _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_query_that_ends_is_heard_at_its_exit_and_a_dropped_stop_stops() {
        let started = std::time::Instant::now();
        let mut child = Command::new("/bin/sh").args(["-c", "exit 3"]).spawn().unwrap();
        let status = ended_or_stopped(&mut child, &crossbeam_channel::never()).unwrap().unwrap();
        assert_eq!(status.code(), Some(3));
        assert!(started.elapsed() < Duration::from_secs(5), "heard after {:?}", started.elapsed());
        let mut child = Command::new("/bin/sh").args(["-c", "exec sleep 600"]).spawn().unwrap();
        let (stopper, stop) = crossbeam_channel::bounded::<()>(1);
        drop(stopper);
        assert!(ended_or_stopped(&mut child, &stop).unwrap().is_none());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "stopped after {:?}",
            started.elapsed()
        );
    }

    /// A history as a blaze from before `events` and `query_counts` left it: every schema file
    /// but theirs.
    fn old_history(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-sql-{name}-{}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        drop(History::open(&dir, CAP_BYTES, crate::sampler::unsampled()).unwrap());
        for table in ["events", "query_counts"] {
            fs::remove_file(dir.join(format!("schema.{table}.parquet"))).unwrap();
        }
        dir
    }

    /// A stand-in for DuckDB that runs nothing and answers nothing: a query that reaches it got
    /// past the views' files.
    #[cfg(unix)]
    fn silent_duckdb(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let fake = dir.join("duckdb");
        fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        fake
    }

    /// The `blaze debug sql` path over a history written before a table existed writes that
    /// table's schema file before a view opens, so the query reaches DuckDB instead of failing
    /// on a glob that matches nothing.
    #[cfg(unix)]
    #[test]
    fn a_query_over_a_history_older_than_a_table_writes_its_schema_file_first() {
        let dir = old_history("old");
        let fake = silent_duckdb(&dir);
        let error = run(&Query {
            duckdb: &fake,
            history: &dir,
            scope: Scope::All,
            sql: "SELECT count(*) FROM events",
        })
        .unwrap_err();
        assert!(matches!(error, SqlError::Duckdb { .. }), "the query reached DuckDB: {error}");
        for table in schema::TABLES {
            assert!(dir.join(format!("schema.{}.parquet", table.name)).exists(), "{}", table.name);
        }
        _ = fs::remove_dir_all(&dir);
    }

    /// Where the missing schema files can't be written, the query is refused naming them and
    /// the directory. A directory where the file's hidden name goes blocks the write as any
    /// user, root too.
    #[cfg(unix)]
    #[test]
    fn a_history_whose_missing_schema_files_cant_be_written_is_refused_naming_them() {
        let dir = old_history("blocked");
        fs::create_dir(dir.join(format!(".schema.events.parquet.{}.tmp", std::process::id())))
            .unwrap();
        let fake = silent_duckdb(&dir);
        let error =
            run(&Query { duckdb: &fake, history: &dir, scope: Scope::All, sql: "SELECT 1" })
                .unwrap_err();
        match &error {
            SqlError::History(HistoryError::SchemasMissing { dir: named, tables, .. }) => {
                assert_eq!(named, &dir);
                assert_eq!(tables, &["events", "query_counts"]);
            }
            _ => panic!("{error}"),
        }
        let said = error.to_string();
        assert!(
            said.contains("events, query_counts") && said.contains(&dir.display().to_string()),
            "{said}"
        );
        _ = fs::remove_dir_all(&dir);
    }

    /// Over a history older than `events` and `query_counts`, both views read as empty.
    #[test]
    #[ignore = "needs the pinned DuckDB: BLAZE_DUCKDB=$(nix build --print-out-paths .#duckdb)/bin/duckdb"]
    fn duckdb_reads_a_table_newer_than_the_history_as_empty() {
        let duckdb = PathBuf::from(
            std::env::var_os("BLAZE_DUCKDB").expect("BLAZE_DUCKDB names the pinned DuckDB CLI"),
        );
        let dir = old_history("newer");
        for table in ["events", "query_counts"] {
            let sql = format!("SELECT count(*) AS n FROM {table}");
            let batches =
                run(&Query { duckdb: &duckdb, history: &dir, scope: Scope::All, sql: &sql })
                    .unwrap_or_else(|error| panic!("{table}: {error}"));
            assert_eq!(
                batches[0].column(0).as_primitive::<arrow_array::types::Int64Type>().value(0),
                0,
                "{table}"
            );
        }
        _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_duckdb_not_named_by_an_absolute_path_is_an_error() {
        let query = Query {
            duckdb: Path::new("duckdb"),
            history: Path::new("/nowhere"),
            scope: Scope::All,
            sql: "SELECT 1",
        };
        let error = run(&query).unwrap_err();
        assert!(matches!(error, SqlError::NoDuckdb { .. }), "{error}");
        assert!(error.to_string().contains("never looked up on PATH"));
    }

    /// Runs the DuckDB `BLAZE_DUCKDB` names (`nix build .#duckdb`'s `bin/duckdb`) over a
    /// flushed history: every view, and a count of the spans flushed.
    #[test]
    #[ignore = "needs the pinned DuckDB: BLAZE_DUCKDB=$(nix build --print-out-paths .#duckdb)/bin/duckdb"]
    fn duckdb_reads_a_flushed_history_through_every_view() {
        let duckdb =
            std::env::var_os("BLAZE_DUCKDB").expect("BLAZE_DUCKDB names the pinned DuckDB CLI");
        let duckdb = PathBuf::from(duckdb);
        crate::install();
        std::thread::spawn(|| {
            let _build = flight::building(77);
            let _outer = tracing::info_span!(target: "blaze", "sql outer").entered();
            let _inner =
                tracing::info_span!(target: "blaze", "sql inner", function = "app::main").entered();
            flight::plot(flight::Plot::Queued, 3);
            for (job, deepest) in [(1, 40 << 10), (2, 900 << 10)] {
                flight::fiber_stack(flight::FiberStack {
                    fiber: 3,
                    job,
                    reserved: 16 << 20,
                    deepest,
                    touched: 900 << 10,
                    grown: 0,
                });
            }
        })
        .join()
        .unwrap();
        let dir = std::env::temp_dir().join(format!("blaze-sql-test-{}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        let mut history = History::open(&dir, CAP_BYTES, crate::sampler::unsampled()).unwrap();
        history.build_ended(BuildRow {
            build_id: 77,
            commit: Some("feedbeef".into()),
            workspace: "/work/app".into(),
            command: "build".into(),
            started_ns: 0,
            dur_ns: Some(1),
            started_unix_ns: None,
            outcome: Outcome::Ok,
            hits: Some(1),
            misses: Some(1),
            joined: Some(0),
            tree: None,
            blaze: None,
            client_ns: None,
            daemon_cpu_ns: None,
        });
        history.flush(Some(77)).unwrap();

        let count = |sql: &str| {
            let batches = run(&Query { duckdb: &duckdb, history: &dir, scope: Scope::All, sql })
                .unwrap_or_else(|error| panic!("{error}"));
            batches[0].column(0).as_primitive::<arrow_array::types::Int64Type>().value(0)
        };
        assert!(
            count("SELECT count(*) AS n FROM spans WHERE build_id = 77 AND name = 'sql inner'")
                >= 1
        );
        assert_eq!(
            count("SELECT count(*) AS n FROM builds WHERE build_id = 77 AND commit = 'feedbeef'"),
            1
        );
        assert!(
            count(
                "SELECT count(*) AS n FROM passes WHERE build_id = 77 AND function = 'app::main'"
            ) >= 1
        );
        assert_eq!(
            count("SELECT count(*) AS n FROM provenance"),
            0,
            "an empty table reads from its schema file"
        );
        assert_eq!(
            count("SELECT count(*) AS n FROM fiber_stacks WHERE build_id = 77 AND fiber = 3"),
            2
        );
        let mut build = Map::new();
        build.insert("build".into(), Value::from(77));
        let ranked =
            views::render(views::find("jobs_by_stack_high_water").unwrap(), &build).unwrap();
        let deepest = format!("SELECT CAST(job AS BIGINT) AS job FROM ({ranked}) WHERE rank = 1");
        assert_eq!(count(&deepest), 2, "the job that went deepest ranks first");

        assert!(count("SELECT count(*) AS n FROM counters WHERE name = 'pool queued'") >= 1);
        let one = run(&Query {
            duckdb: &duckdb,
            history: &dir,
            scope: Scope::Build(77),
            sql: "SELECT count(*) AS n FROM builds WHERE build_id = 77",
        })
        .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            one[0].column(0).as_primitive::<arrow_array::types::Int64Type>().value(0),
            1,
            "a build's scope reads its files"
        );

        for view in views::VIEWS {
            let mut args = Map::new();
            args.insert("build".into(), Value::from(77));
            args.insert("earlier".into(), Value::from("feed"));
            args.insert("later".into(), Value::from("feedbeef"));
            args.retain(|key, _| view.args.iter().any(|it| it.name == key));
            let sql = views::render(view, &args).unwrap();
            let batches =
                run(&Query { duckdb: &duckdb, history: &dir, scope: Scope::All, sql: &sql })
                    .unwrap_or_else(|error| panic!("the view {} runs: {error}", view.name));
            assert!(
                batches
                    .iter()
                    .all(|it| it.num_columns() > 0 && it.column(0).len() == it.num_rows())
            );
        }

        for view in views::VIEWS.iter().filter(|it| it.args.iter().all(|arg| arg.default.is_some()))
        {
            // alloc: a query, once a view
            let sql = format!("SELECT count(*) AS n FROM {}", view.name);
            run(&Query { duckdb: &duckdb, history: &dir, scope: Scope::All, sql: &sql })
                .unwrap_or_else(|error| {
                    panic!("`FROM {}` reads the view as a table: {error}", view.name)
                });
        }

        let error = run(&Query {
            duckdb: &duckdb,
            history: &dir,
            scope: Scope::All,
            sql: "SELECT no_such_column FROM spans",
        })
        .unwrap_err();
        match &error {
            SqlError::Duckdb { message, script, .. } => {
                assert!(message.contains("no_such_column"), "DuckDB's own message: {message}");
                assert!(script.exists(), "a failed query's script is kept");
            }
            _ => panic!("{error}"),
        }
    }

    /// A build of two jobs: job 2 (slot 2) runs `unit b` from 10 to 100 ns and waits from 20 to 60
    /// for a key job 1 (slot 1, owner 2) runs in `compile a` from 0 to 60. Its critical path is
    /// job 1's run, then job 2's after its wait: the wait's owner is followed across lanes.
    #[test]
    #[ignore = "needs the pinned DuckDB: BLAZE_DUCKDB=$(nix build --print-out-paths .#duckdb)/bin/duckdb"]
    fn the_critical_path_crosses_from_a_wait_to_the_job_that_ran_what_it_waited_on() {
        let duckdb = PathBuf::from(
            std::env::var_os("BLAZE_DUCKDB").expect("BLAZE_DUCKDB names the pinned DuckDB CLI"),
        );
        let dir = std::env::temp_dir().join(format!("blaze-sql-critical-{}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        History::open(&dir, CAP_BYTES, crate::sampler::unsampled()).unwrap();
        let fixture = format!(
            "COPY (SELECT * FROM (VALUES (1::UBIGINT, 0::BIGINT, 100::BIGINT, 'ok', '/work', 'build')) \
                AS t(build_id, started_ns, dur_ns, outcome, workspace, command)) TO '{d}/1-00000001.builds.parquet';\n\
             COPY (SELECT * FROM (VALUES \
                (1::UBIGINT, 7::UINTEGER, 10::UINTEGER, 1::UBIGINT, 0::USMALLINT, 'compile a', 'a', NULL::VARCHAR, NULL::UBIGINT, 0::BIGINT, 60::BIGINT), \
                (1, 7, 11, 2, 0, 'unit b', 'b', NULL, NULL, 10, 90), \
                (1, 7, 11, 3, 1, 'wait', NULL, 'key a', 2, 20, 40)) \
                AS t(build_id, process, thread, span_id, depth, name, crate, awaits, owner, start_ns, dur_ns)) TO '{d}/1-00000001.spans.parquet';\n\
             COPY (SELECT * FROM (VALUES \
                (1::UBIGINT, 7::UINTEGER, 10::UINTEGER, 0::BIGINT, 'started', 1::UBIGINT), \
                (1, 7, 11, 10, 'started', 2), (1, 7, 11, 21, 'parked', 2), (1, 7, 10, 60, 'ended', 1), \
                (1, 7, 11, 60, 'resumed', 2), (1, 7, 11, 100, 'ended', 2)) \
                AS t(build_id, process, thread, t_ns, event, job)) TO '{d}/1-00000001.switches.parquet';\n",
            d = dir.display()
        );
        let written = Command::new(&duckdb)
            .args(["-batch", "-bail", "-init", "/dev/null", "-c", &fixture])
            .output()
            .unwrap();
        assert!(written.status.success(), "{}", String::from_utf8_lossy(&written.stderr));
        crate::history::list_written(&dir);

        let mut args = Map::new();
        args.insert("build".into(), Value::from(1));
        let path = views::render(views::find("critical_path").unwrap(), &args).unwrap();
        let sql = format!(
            "SELECT CAST(step AS BIGINT) AS step, CAST(lane AS VARCHAR) AS lane, CAST(what AS VARCHAR) AS what, \
             CAST(from_ns AS BIGINT) AS from_ns, CAST(to_ns AS BIGINT) AS to_ns, CAST(path_pct AS DOUBLE) AS pct FROM ({path})"
        );
        let batches = run(&Query { duckdb: &duckdb, history: &dir, scope: Scope::All, sql: &sql })
            .unwrap_or_else(|error| panic!("{error}"));
        let mut steps = Vec::new();
        for batch in &batches {
            let int = |at: usize| {
                batch.column(at).as_primitive::<arrow_array::types::Int64Type>().clone()
            };
            let text = |at: usize| batch.column(at).as_string::<i32>().clone();
            let (step, lane, what, from, to) = (int(0), text(1), text(2), int(3), int(4));
            let pct = batch.column(5).as_primitive::<arrow_array::types::Float64Type>().clone();
            for i in 0..batch.num_rows() {
                steps.push((
                    step.value(i),
                    lane.value(i).to_owned(),
                    what.value(i).to_owned(),
                    from.value(i),
                    to.value(i),
                ));
                assert!((pct.value(i) - 100.0).abs() < 0.01, "the path is the whole build");
            }
        }
        assert_eq!(
            steps,
            [
                (1, "job 1".to_owned(), "compile a".to_owned(), 0, 60),
                (0, "job 2".to_owned(), "unit b".to_owned(), 60, 100),
            ]
        );
        _ = fs::remove_dir_all(&dir);
    }

    /// Two workers of a build from 0 to 100 ns. Worker 10 begins job 1 at 0, which joins the
    /// graph as it parks at 40; it searches under a `worker` span and resumes the job at 60 to
    /// its end at 100. The job's span moved with it and closed on another thread, so only the
    /// job's switch rows (its `since_ns`, its `resumed`) cover 0 to 40 and 60 to 100: covered
    /// throughout. Worker 11 runs job 2 from 0 and parks it at 50, then is in no span,
    /// no run and no idle to the build's end: half its time is uncovered, which the gate fails.
    #[test]
    #[ignore = "needs the pinned DuckDB: BLAZE_DUCKDB=$(nix build --print-out-paths .#duckdb)/bin/duckdb"]
    fn a_worker_outside_every_span_run_and_idle_is_uncovered() {
        let duckdb = PathBuf::from(
            std::env::var_os("BLAZE_DUCKDB").expect("BLAZE_DUCKDB names the pinned DuckDB CLI"),
        );
        let dir = std::env::temp_dir().join(format!("blaze-sql-uncovered-{}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        History::open(&dir, CAP_BYTES, crate::sampler::unsampled()).unwrap();
        let fixture = format!(
            "COPY (SELECT * FROM (VALUES (1::UBIGINT, 0::BIGINT, 100::BIGINT, 'ok', '/work', 'build')) \
                AS t(build_id, started_ns, dur_ns, outcome, workspace, command)) TO '{d}/1-00000001.builds.parquet';\n\
             COPY (SELECT * FROM (VALUES \
                (1::UBIGINT, 7::UINTEGER, 10::UINTEGER, 4::UBIGINT, 0::USMALLINT, 'worker', 40::BIGINT, 20::BIGINT), \
                (1, 7, 11, 3, 0, 'job', 0, 50)) \
                AS t(build_id, process, thread, span_id, depth, name, start_ns, dur_ns)) TO '{d}/1-00000001.spans.parquet';\n\
             COPY (SELECT * FROM (VALUES \
                (1::UBIGINT, 7::UINTEGER, 10::UINTEGER, 'worker 10', 40::BIGINT, 'started', 1::UBIGINT, 0::BIGINT), \
                (1, 7, 10, 'worker 10', 40, 'parked', 1, NULL), (1, 7, 10, 'worker 10', 60, 'resumed', 1, NULL), \
                (1, 7, 10, 'worker 10', 100, 'ended', 1, NULL), \
                (1, 7, 11, 'worker 11', 0, 'started', 2, 0), (1, 7, 11, 'worker 11', 50, 'parked', 2, NULL)) \
                AS t(build_id, process, thread, thread_name, t_ns, event, job, since_ns)) TO '{d}/1-00000001.switches.parquet';\n",
            d = dir.display()
        );
        let written = Command::new(&duckdb)
            .args(["-batch", "-bail", "-init", "/dev/null", "-c", &fixture])
            .output()
            .unwrap();
        assert!(written.status.success(), "{}", String::from_utf8_lossy(&written.stderr));
        crate::history::list_written(&dir);

        let mut args = Map::new();
        args.insert("build".into(), Value::from(1));
        let uncovered = views::render(views::find("uncovered_per_worker").unwrap(), &args).unwrap();
        let sql = format!(
            "SELECT CAST(thread AS BIGINT) AS thread, CAST(uncovered_pct AS DOUBLE) AS pct FROM ({uncovered}) ORDER BY thread"
        );
        let batches = run(&Query { duckdb: &duckdb, history: &dir, scope: Scope::All, sql: &sql })
            .unwrap_or_else(|error| panic!("{error}"));
        let mut workers = Vec::new();
        for batch in &batches {
            let thread = batch.column(0).as_primitive::<arrow_array::types::Int64Type>().clone();
            let pct = batch.column(1).as_primitive::<arrow_array::types::Float64Type>().clone();
            workers.extend((0..batch.num_rows()).map(|i| (thread.value(i), pct.value(i))));
        }
        assert_eq!(workers, [(10, 0.0), (11, 50.0)]);
        _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_critical_path_fixture_names_only_columns_of_the_schema() {
        for (table, columns) in [
            (
                &schema::BUILDS,
                &["build_id", "started_ns", "dur_ns", "outcome", "workspace", "command"][..],
            ),
            (
                &schema::SPANS,
                &[
                    "build_id", "process", "thread", "span_id", "depth", "name", "crate", "awaits",
                    "owner", "start_ns", "dur_ns",
                ],
            ),
            (
                &schema::SWITCHES,
                &["build_id", "process", "thread", "thread_name", "t_ns", "event", "job"],
            ),
        ] {
            for column in columns {
                assert!(table.column(column).is_some(), "{}.{column}", table.name);
            }
        }
    }
}
