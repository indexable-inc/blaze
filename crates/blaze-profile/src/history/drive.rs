//! The driver of the history's core ([`super::files`]): it runs the effects the core asks for
//! on this target's native driver ([`blaze_core::native`]), answers the core's [`Host`] with
//! this process (its pid, which processes are alive, crash records named by this executable's
//! symbols, a dead daemon's flight recorder read back and encoded as Parquet), and opens the
//! span naming each wait. It owns no rule of the history's files: every write, sync, rename,
//! delete and lock is the core's effect, run here as asked.
//!
//! Each operation is one job of the core driven to its outcome ([`Core::job`]): the loop
//! submits what the core asked, polls for completions with a bound, hands each to the core
//! with the driver's clock, and ends once the core is idle. A core busy with nothing in flight,
//! or a wait with no completion for [`STALL`], is a typed error naming the history and what it
//! waited on (bounded-progress): never an open loop.

use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use arrow_array::RecordBatch;
use blaze_core::{Completion, Driver, Effect, FileName, wait::Deadline};

use super::{
    BuildRow, HistoryError, Outcome as BuildOutcome, Writer, arrow_error, builds,
    compact::Name,
    files::{CrashRead, CrashRow, Dead, Files, Host, Outcome, Placed, Recovery, Wait},
    manifest::{Entry, Manifest},
    table_at,
};
use crate::{
    batches::{self, Process},
    crash::{self, CrashError},
    flight::postmortem,
    schema::{self, Table},
};

/// Threads the native driver runs file calls on (kqueue's and IOCP's pool): the history's
/// effects are a few at a time, a round of one job.
const THREADS: usize = 2;

/// The longest one poll waits for a completion before the loop looks at its bound again.
const POLL: Duration = Duration::from_secs(1);

/// How long the drive waits with effects in flight and none completing before it fails,
/// naming what it waited on. Far past any one effect of the history (a sync of a merge's
/// output, a lock the core retries on its own timers until `manifest::LOCK_WAIT`).
const STALL: Duration = Duration::from_secs(300);

// --- The host ---------------------------------------------------------------------------

/// This process as the core's [`Host`].
pub(crate) struct Live {
    /// The history's directory: the paths a dead daemon's files are named by in errors.
    dir: PathBuf,
    /// What a dead daemon's rows are encoded with.
    writer: Writer,
    /// This executable's hash ([`crate::identity::binary_hash`]), once a crash record asked
    /// for it; its error kept as kind and text, `io::Error` being no clone.
    ours: Option<Result<[u8; 32], (io::ErrorKind, String)>>,
}

impl Live {
    pub(crate) fn new(dir: &Path) -> Live {
        Live {
            // alloc: a path, once a core
            dir: dir.to_owned(),
            writer: Writer::new(),
            ours: None,
        }
    }

    /// The rows of the dead daemon `dead`, read back as files of its flush, or why not.
    fn recovered(&mut self, dead: Dead) -> Result<Recovery, HistoryError> {
        let Dead { pid, path, mapping, recorder, seq } = dead;
        let found = postmortem::Found::of(path, pid, mapping);
        let nonce = found.nonce().map_err(HistoryError::Postmortem)?;
        // What the manifest consumed of this mapping: of this pid and this nonce, a mapping of
        // a later process given the same pid being another.
        let listed = nonce.and_then(|nonce| recorder.filter(|it| it.nonce == nonce));
        if listed.is_some_and(|it| it.recovered) {
            return Ok(Recovery::Already);
        }
        // Each read holds the postmortem's names until it drops: one at a time.
        let recovered =
            found.read(listed.map(|it| it.flushed)).map_err(HistoryError::Postmortem)?;
        let files = recovered_files(&self.dir, &self.writer, seq, &recovered)?;
        Ok(Recovery::Files { nonce: recovered.nonce, files })
    }
}

impl Host for Live {
    fn pid(&self) -> u32 {
        std::process::id()
    }

    fn alive(&self, pid: u32) -> bool {
        super::alive(pid)
    }

    fn crash(&mut self, path: &Path, bytes: &[u8]) -> CrashRead {
        let ours = self.ours.get_or_insert_with(|| {
            // alloc: an error's text, once a core
            crate::identity::binary_hash().map_err(|error| (error.kind(), error.to_string()))
        });
        let hash = match ours {
            Ok(hash) => *hash,
            Err((kind, text)) => {
                return CrashRead::Unread(CrashError::OursUnhashed {
                    // alloc: an error
                    path: path.to_owned(),
                    // clone: an error's text, once a record it leaves unread
                    error: io::Error::new(*kind, text.clone()),
                });
            }
        };
        match crash::read(path, bytes, &hash) {
            Ok(None) => CrashRead::Nothing,
            Ok(Some(crash)) => CrashRead::Row(CrashRow {
                pid: crash.pid,
                t_ns: crash.t_ns,
                row: crash.row(),
                image: crash.image(),
            }),
            Err(refused) => CrashRead::Refused(refused),
        }
    }

    fn recover(&mut self, dead: Dead) -> Recovery {
        match self.recovered(dead) {
            Ok(recovery) => recovery,
            Err(error) => Recovery::Refused(error),
        }
    }
}

/// The rows of `recovered`, a dead daemon's, as files of flush `seq` in the history `dir`,
/// encoded through `writer` as a flush encodes its own: of the dead process (its pid and
/// name), and a `builds` row `crashed` for each build it was running, from its start to its
/// last row read back.
fn recovered_files(
    dir: &Path,
    writer: &Writer,
    seq: u64,
    recovered: &postmortem::Recovered,
) -> Result<Vec<Placed>, HistoryError> {
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
            // clone: as above
            workspace: build.workspace.clone(),
            // clone: as above
            command: build.command.clone(),
            started_ns: build.started_ns,
            dur_ns: Some(recovered.last_ns.max(build.started_ns) - build.started_ns),
            started_unix_ns: build.started_unix_ns,
            outcome: BuildOutcome::Crashed,
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
    let mut files = Vec::new();
    for (table, batch) in tables {
        if batch.num_rows() == 0 {
            continue;
        }
        let name = Name { build, table: table_at(table), first: seq, last: seq, level: 0 };
        let file = name.file_name();
        let parts = std::slice::from_ref(&batch);
        let bytes = writer.encode(&dir.join(&file), table, parts)?;
        // The dead daemon's rows: its pid, and a boot no longer known (the machine may have
        // restarted since), so a build-scoped read matches them on any boot.
        let entry = Entry::of_rows(table, parts, recovered.pid, None);
        files.push(Placed { file, bytes, entry });
    }
    Ok(files)
}

impl Entry {
    /// The entry of a file of `table` this process writes with the rows of `parts`: its pid,
    /// and the boot its clock counts from.
    pub fn of_batches(table: &schema::Table, parts: &[RecordBatch]) -> Entry {
        Entry::of_rows(table, parts, std::process::id(), crate::clock::boot())
    }
}

// --- The drive --------------------------------------------------------------------------

/// The history `dir` as its driver's root and its name under it: the parent directory (`.` for
/// a name with none) and the last component. A directory with no last component (`/`, a path
/// ending in `..`) or a name that is not UTF-8 is refused, naming it.
pub(crate) fn split(dir: &Path) -> Result<(PathBuf, FileName), HistoryError> {
    let refused = |why: &'static str| HistoryError::Io {
        // alloc: an error
        path: dir.to_owned(),
        doing: "taking the parent and the name of the history's directory",
        error: io::Error::new(io::ErrorKind::InvalidInput, why),
    };
    let Some(parent) = dir.parent() else {
        return Err(refused("it has no parent directory"));
    };
    let Some(last) = dir.file_name() else {
        return Err(refused("it has no last component to name it by"));
    };
    let Some(last) = last.to_str() else {
        return Err(refused("its name is not UTF-8"));
    };
    let Some(name) = FileName::new(last) else {
        return Err(refused("its name can't name a file under its parent"));
    };
    let root = match parent.as_os_str().is_empty() {
        true => PathBuf::from("."),
        // alloc: a path, once a core
        false => parent.to_owned(),
    };
    Ok((root, name))
}

/// The error of a drive of the history `dir` that stopped, saying why.
fn stopped(dir: &Path, doing: &'static str, why: io::Error) -> HistoryError {
    // alloc: an error
    HistoryError::Io { path: dir.to_owned(), doing, error: why }
}

/// What the drive of the history says it waited on as it stalled.
fn waiting_on(wait: Wait) -> &'static str {
    match wait {
        Wait::Nothing => "driving the history's files, waiting on nothing,",
        Wait::ManifestLock => "waiting on the manifest's lock of the history",
        Wait::Sync => "waiting on a sync to disk of the history",
        Wait::Merge => "waiting on a merge of the history",
        Wait::Reap => "waiting on a reap of the history",
        Wait::Files => "waiting on a read or write of the history",
    }
}

/// The span naming the wait `wait` of the history `dir` (every-wait-says-what); `None` for the
/// waits on its plain reads and writes, which the job's own span covers.
fn span_of(wait: Wait, dir: &Path) -> Option<tracing::span::EnteredSpan> {
    match wait {
        Wait::ManifestLock => Some(
            tracing::info_span!(
                target: "blaze",
                "waits on the profile history's manifest lock",
                dir = %dir.display()
            )
            .entered(),
        ),
        Wait::Sync => Some(tracing::info_span!(target: "blaze", "profile fsync").entered()),
        Wait::Merge => Some(tracing::info_span!(target: "blaze", "profile merge").entered()),
        Wait::Reap => Some(tracing::info_span!(target: "blaze", "profile reap").entered()),
        Wait::Files | Wait::Nothing => None,
    }
}

/// Drives `files`, the core of the history `dir`, on `driver` until it is idle: what it asks
/// is submitted, each completion handed back on the driver's clock. Each poll is bounded by
/// [`POLL`]; a core busy with nothing in flight, or effects in flight with none completing for
/// [`STALL`], stops the drive with an error naming the history and what it waited on. A drive
/// that stops leaves the core mid-job ([`Core::job`] refuses it from then on).
pub(crate) fn run<H: Host>(
    files: &mut Files<H>,
    driver: &mut impl Driver,
    dir: &Path,
) -> Result<(), HistoryError> {
    // alloc: a drive's effects and completions, once a job
    let mut effects: Vec<Effect> = Vec::new();
    // alloc: as above
    let mut done: Vec<Completion> = Vec::new();
    // The span open over the polls of one wait, and the wait it names.
    let mut spanned = Wait::Nothing;
    let mut span: Option<tracing::span::EnteredSpan> = None;
    let mut progressed = driver.now();
    loop {
        files.effects(&mut effects);
        driver.submit(&mut effects);
        if !files.busy() {
            return Ok(());
        }
        if driver.in_flight() == 0 {
            return Err(stopped(
                dir,
                "driving the history's files",
                io::Error::other("the history's core is busy with no effect in flight"),
            ));
        }
        let wait = files.waits_on();
        if wait != spanned {
            drop(span.take());
            span = span_of(wait, dir);
            spanned = wait;
        }
        let moved = driver.poll(&mut done, Deadline::after(driver.now(), POLL));
        let now = driver.now();
        if moved == 0 {
            if now.saturating_sub(progressed) >= STALL {
                return Err(stopped(
                    dir,
                    waiting_on(wait),
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        // alloc: an error
                        format!("no effect completed in {STALL:?}"),
                    ),
                ));
            }
            continue;
        }
        progressed = now;
        for completion in done.drain(..) {
            files.complete(completion, now);
        }
    }
}

/// A history's core and the driver that runs its effects: what owns the history's IO in a
/// process (a [`super::History`], a [`super::Pin`], a one-shot), one job at a time. Its
/// driver holds the locks its effects took, so it lives as long as they are to be held.
pub(crate) struct Core<H: Host> {
    files: Files<H>,
    driver: blaze_core::Native,
    dir: PathBuf,
}

impl<H: Host> Core<H> {
    /// The core of the history `dir` with `host`, on a native driver rooted at its parent.
    pub(crate) fn new(host: H, dir: &Path) -> Result<Core<H>, HistoryError> {
        let (root, name) = split(dir)?;
        let driver = blaze_core::native(&root, THREADS).map_err(|error| HistoryError::Io {
            // clone: an error's path
            path: root.clone(),
            doing: "starting the IO driver of the profile history under",
            error,
        })?;
        let files = Files::new(host, root, name, super::all_properties());
        // alloc: a path, once a core
        Ok(Core { files, driver, dir: dir.to_owned() })
    }

    pub(crate) fn files(&self) -> &Files<H> {
        &self.files
    }

    pub(crate) fn files_mut(&mut self) -> &mut Files<H> {
        &mut self.files
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Asks the core for one job (`ask`) and drives it to its outcome. A core a drive left
    /// mid-job is refused: its outcome would be told to the wrong asker.
    pub(crate) fn job(&mut self, ask: impl FnOnce(&mut Files<H>)) -> Result<Outcome, HistoryError> {
        if self.files.busy() {
            return Err(stopped(
                &self.dir,
                "asking the history's files for a job",
                io::Error::other("a drive before stopped mid-job, so the core can't take another"),
            ));
        }
        ask(&mut self.files);
        run(&mut self.files, &mut self.driver, &self.dir)?;
        match self.files.outcome() {
            Some(outcome) => Ok(outcome),
            None => Err(stopped(
                &self.dir,
                "driving a job of the history's files",
                io::Error::other("the core went idle with no outcome told"),
            )),
        }
    }
}

/// The name of `outcome`, for the error of a job that told another than it was asked for.
fn outcome_name(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Opened(_) => "opened",
        Outcome::Flushed(_) => "flushed",
        Outcome::Recounted(_) => "recounted",
        Outcome::WroteListed(_) => "wrote listed files",
        Outcome::Read(_) => "read the manifest",
        Outcome::Schemas(_) => "wrote schema files",
        Outcome::RecoveredDead(_) => "recovered dead daemons' rows",
        Outcome::Pinned(_) => "pinned",
        Outcome::Unpinned => "unpinned",
    }
}

/// The error of a job of the history `dir` asked for `asked` that told `got`: the core's bug.
pub(crate) fn mismatch(dir: &Path, asked: &'static str, got: &Outcome) -> HistoryError {
    stopped(
        dir,
        asked,
        // alloc: an error
        io::Error::other(format!("the history's core told the outcome `{}`", outcome_name(got))),
    )
}

// --- One-shots --------------------------------------------------------------------------

/// The manifest of the history `dir` as a reader reads it, with no lock.
pub fn read_manifest(dir: &Path) -> Result<Manifest, HistoryError> {
    let mut core = Core::new(Live::new(dir), dir)?;
    match core.job(Files::read)? {
        Outcome::Read(read) => read,
        other => Err(mismatch(dir, "reading the manifest of", &other)),
    }
}

/// Makes sure the history `dir` has the schema file of every table, as a reader needs before
/// it opens views over it: a history last written before a table existed has none of that
/// table's. Writes only those missing, leaving the rest as their writer wrote them. A
/// directory they can't be written to is [`HistoryError::SchemasMissing`], naming them.
pub fn ensure_schemas(dir: &Path) -> Result<(), HistoryError> {
    let writer = Writer::new();
    // alloc: once a reader opens views
    let mut files = Vec::with_capacity(schema::TABLES.len());
    for table in schema::TABLES {
        let file = super::schema_file(table);
        let bytes = writer.encode(&dir.join(&file), table, &[])?;
        files.push((file, table.name, bytes));
    }
    let mut core = Core::new(Live::new(dir), dir)?;
    match core.job(|it| it.ensure_schemas(files))? {
        Outcome::Schemas(written) => written,
        other => Err(mismatch(dir, "writing the schema files of", &other)),
    }
}

/// For a reader of the history `dir` with no daemon writing it (`blaze slow`): writes the last
/// rows of every daemon that died before it flushed them, as [`super::History::open`] does,
/// each file listed in the manifest. Nothing while a daemon holds the history
/// ([`super::OWNER`]): it did so as it opened, and numbers its own files. The files written,
/// counted.
pub fn recover_dead(dir: &Path) -> Result<usize, HistoryError> {
    let mut core = Core::new(Live::new(dir), dir)?;
    match core.job(Files::recover_dead)? {
        Outcome::RecoveredDead(written) => written,
        other => Err(mismatch(dir, "recovering dead daemons' rows into", &other)),
    }
}

/// Writes each of `files` (its path in the history `dir`, its table, its rows) and lists them
/// all in the manifest in one commit: for a writer of the history other than the daemon's
/// [`super::History`] (`blaze debug time`'s derived tables, `blaze perf record`'s runs). The
/// core places each at its hidden name first, then, holding the manifest's lock, names them
/// into place and lists them; a reader sees all of them or none. A path not directly in `dir`
/// is refused, naming it. The bytes written.
pub(crate) fn write_listed(
    dir: &Path,
    files: &[(&Path, &'static Table, &[RecordBatch])],
) -> Result<u64, HistoryError> {
    std::fs::create_dir_all(dir).map_err(super::io_error(dir, "making the directory"))?;
    let writer = Writer::new();
    // alloc: once a write
    let mut placed = Vec::with_capacity(files.len());
    for &(path, table, parts) in files {
        let name = path.file_name().and_then(|it| it.to_str());
        let (Some(name), Some(true)) = (name, path.parent().map(|it| it == dir)) else {
            return Err(HistoryError::Io {
                // alloc: an error
                path: path.to_owned(),
                doing: "listing a file in the profile history",
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a file listed is one directly in the history's directory, named in UTF-8",
                ),
            });
        };
        let bytes = writer.encode(path, table, parts)?;
        // alloc: a file's name, once a file
        placed.push(Placed {
            file: name.to_owned(),
            bytes,
            entry: Entry::of_batches(table, parts),
        });
    }
    let mut core = Core::new(Live::new(dir), dir)?;
    match core.job(|it| it.write_listed(placed))? {
        Outcome::WroteListed(written) => written,
        other => Err(mismatch(dir, "writing listed files into", &other)),
    }
}

/// Lists each Parquet file `dir` holds that its manifest does not and that is of a form blaze
/// writes: what a test that writes a history's files with DuckDB does, so its files are read.
/// Each is read, deleted, and written back through the core, listed in one commit.
#[cfg(test)]
pub(crate) fn list_written(dir: &Path) {
    let listed = read_manifest(dir).unwrap();
    // alloc: a test's files
    let mut placed = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        let listable = matches!(super::manifest::kind(&name), super::manifest::Kind::Listable);
        if !listable || listed.contains(&name) {
            continue;
        }
        let path = dir.join(&name);
        let bytes = bytes::Bytes::from(std::fs::read(&path).unwrap());
        let entry = Entry::of_footer(&path, &bytes, Some(schema::VERSION)).unwrap();
        std::fs::remove_file(&path).unwrap();
        placed.push(Placed { file: name, bytes: bytes.to_vec(), entry });
    }
    let mut core = Core::new(Live::new(dir), dir).unwrap();
    match core.job(|it| it.write_listed(placed)).unwrap() {
        Outcome::WroteListed(written) => _ = written.unwrap(),
        other => panic!("{}", mismatch(dir, "listing a test's files in", &other)),
    }
}
