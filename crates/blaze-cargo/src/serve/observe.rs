//! What the daemon says of itself beside its builds (docs/profiling.md;
//! docs/v2/plan/design/explore.md, "Where it runs").
//!
//! - [`Profiling`]: the profile history, which the thread `blaze-history` keeps off the loop
//!   ([`keep_history`]), and the DuckDB `nix run .#duckdb` pins ([`duckdb`]). `blaze serve`
//!   alone has them; a simulation's daemon has none, and the requests that read them fail
//!   saying so.
//! - [`Observe`]: what the loop holds for the requests it answers itself: the explorer
//!   (`blaze explore`), each live subscriber's feed (`blaze debug live`), and each reader's
//!   ask that the history flush ([`blaze_api::Request::Flush`]), which the history's thread
//!   answers once the flush has committed: what `blaze debug sql`, `blaze slow` and
//!   `blaze debug time` ask first, so they read every row recorded before they asked.
//!
//! The explorer's link carries its session's token: it goes to the client in
//! [`Response::Explored`] and nowhere else, never to a log.

use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use blaze_api::{ApiError, LiveFrame, Reply, RequestRef, Response};
use blaze_core::Conn;
use blaze_profile::{
    facts::{CrateDepRow, KeyPart, KeyedMiss, LinkRow, ProvenanceRow, QueryCountRow, RunOutcome},
    history::{self, BuildRow, History, OWNER},
    live::{self, Feed, Frame},
};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use super::why::{Answer, Answers, Ledger, file::File};

/// The variable naming the DuckDB `blaze debug sql` and the explorer run: an absolute path,
/// which blaze's Nix wrapper sets to `nix run .#duckdb`'s. Never looked up on `PATH`.
pub const DUCKDB: &str = "BLAZE_DUCKDB";

/// The DuckDB [`DUCKDB`] names: `None` unset, and an error naming the variable and its value
/// when set to anything but an absolute path to a file.
pub fn duckdb(value: Option<std::ffi::OsString>) -> Result<Option<PathBuf>, String> {
    let Some(value) = value else { return Ok(None) };
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(format!(
            // alloc: error path
            "{DUCKDB}={} is not an absolute path: it names the DuckDB `nix run .#duckdb` pins",
            path.display()
        ));
    }
    match fs::metadata(&path) {
        Ok(meta) if meta.is_file() => Ok(Some(path)),
        Ok(_) => Err(format!("{DUCKDB}={} is not a file", path.display())), // alloc: error path
        Err(error) => Err(format!("{DUCKDB}={}: {error}", path.display())), // alloc: error path
    }
}

/// `blaze debug hot`'s answer: the daemon's live sketches ([`blaze_profile::hot::read`]) in the
/// API's terms.
pub fn hot(top: u32, since: blaze_api::HotSince) -> Result<blaze_api::Hot, ApiError> {
    use blaze_api::{HotFunction, HotKey, HotSince, HotWait, HotWaitKind};
    use blaze_graph::wait::WaitKind;
    use blaze_profile::hot;
    let read = hot::read(
        top as usize,
        match since {
            HotSince::Start => hot::Since::Start,
            HotSince::LastBuild => hot::Since::LastBuild,
        },
    )
    // alloc: error path
    .map_err(|error| {
        ApiError::new(error.to_string(), Some("blaze debug hot --last-build".to_owned()))
    })?;
    let keys = read
        .keys
        .into_iter()
        .map(|it| HotKey {
            span: it.span.to_owned(), // alloc: the answer's copy, once a request
            function: match it.function {
                hot::Function::None => HotFunction::None,
                hot::Function::Named(name) => HotFunction::Named(name.as_ref().to_owned()), // alloc: as above
                hot::Function::Forgotten(sym) => HotFunction::Forgotten(sym),
            },
            lower_ns: it.lower_ns,
            upper_ns: it.upper_ns,
        })
        .collect(); // alloc: the answer's rows, once a request
    let waits = read
        .waits
        .iter()
        .map(|it| HotWait {
            kind: match it.kind {
                WaitKind::Key => HotWaitKind::Key,
                WaitKind::Lock => HotWaitKind::Lock,
                WaitKind::Channel => HotWaitKind::Channel,
                WaitKind::Io => HotWaitKind::Io,
                WaitKind::Child => HotWaitKind::Child,
                WaitKind::Scope => HotWaitKind::Scope,
            },
            count: it.count,
            max_ns: it.max_ns,
            p50_ns: it.p50_ns,
            p90_ns: it.p90_ns,
            p99_ns: it.p99_ns,
        })
        .collect(); // alloc: the answer's rows, once a request
    Ok(blaze_api::Hot {
        keys,
        torn: read.torn,
        since,
        spans: read.spans,
        waits,
        error_ns: read.error_ns,
        total_ns: read.total_ns,
    })
}

/// The bugs the daemon reported and went on past so far, one a site, for `blaze status`
/// ([`blaze_api::Status::bugs`]), ordered by site and kind, not by which thread reported first:
/// none allocates nothing.
pub fn bugs() -> Vec<blaze_api::Bug> {
    let mut sites = stdx::assert::reported::sites();
    sites.sort_unstable_by_key(|it| (it.at.file(), it.at.line(), it.at.column(), it.kind, it.what));
    sites
        .into_iter()
        .map(|it| blaze_api::Bug {
            kind: it.kind.to_owned(), // alloc: the answer's copy, once a Status a site
            what: it.what.to_owned(), // alloc: as above
            at: it.at.to_string(),    // alloc: as above
            count: it.count,
            thread: it.thread.as_deref().map(str::to_owned), // alloc: as above
        })
        .collect() // alloc: the answer's rows; none for none
}

/// The profile history and the DuckDB that queries it, as `blaze serve` gives them to its
/// daemon.
pub struct Profiling {
    /// The history's directory ([`history::dir_in`] of the daemon's state): what the
    /// `blaze-history` thread writes, and the explorer and `blaze debug sql` read.
    pub dir: PathBuf,
    pub duckdb: Option<PathBuf>,
    /// The number of the daemon's first build: past every build the history names, so a build
    /// stays one id across the daemon's restarts ([`History::next_build`]). Never 0.
    pub first_build: u32,
    ended: Sender<ToHistory>,
    failed: Receiver<String>,
    /// What the history's thread tells the loop of `blaze why`'s ledger: its restore, and each
    /// build's answer.
    told: Receiver<Told>,
    /// What the history's thread is doing, for a `blaze why` that waited too long to name.
    doing: Arc<Doing>,
}

/// How long a `blaze why` waits for the builds that ended before it was asked to be explained,
/// on the loop's clock, before it fails naming the build and what the history's thread is
/// doing (rule 18).
pub const WHY_WAIT: Duration = Duration::from_secs(30);

/// What the history's thread is doing, as the loop reads it: a step in the top byte and the
/// build it is for below, one atomic so the two are read together.
#[derive(Default)]
struct Doing(AtomicU64);

/// A step of the history's thread ([`Doing`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Step {
    Opening = 0,
    Restoring = 1,
    Waiting = 2,
    Explaining = 3,
    Appending = 4,
    Flushing = 5,
    Closing = 6,
    /// Flushing the rows the history's drain thread took past [`history::FLUSH_ROWS`].
    FlushingFull = 7,
    /// Flushing for a reader that asked ([`ToHistory::Flush`]).
    FlushingAsked = 8,
}

impl Doing {
    const BUILD: u64 = (1 << 56) - 1;

    fn set(&self, step: Step, build: u64) {
        self.0.store((step as u64) << 56 | (build & Self::BUILD), Ordering::Relaxed);
    }

    /// What the thread is doing, in words.
    fn said(&self) -> String {
        let it = self.0.load(Ordering::Relaxed);
        let build = it & Self::BUILD;
        // alloc: error path, a `blaze why` past its deadline
        let step = [
            Step::Opening,
            Step::Restoring,
            Step::Waiting,
            Step::Explaining,
            Step::Appending,
            Step::Flushing,
            Step::Closing,
            Step::FlushingFull,
            Step::FlushingAsked,
        ]
        .into_iter()
        .find(|step| *step as u64 == it >> 56);
        match step {
            Some(Step::Opening) => "opening the profile history".to_owned(),
            Some(Step::Restoring) => "restoring blaze why's ledger".to_owned(),
            Some(Step::Waiting) => "waiting for a build to end".to_owned(),
            Some(Step::Explaining) => format!("explaining build {build}'s misses"),
            Some(Step::Appending) => {
                format!("appending build {build}'s record to blaze why's ledger")
            }
            Some(Step::Flushing) if build == 0 => {
                "flushing the profile history on its timer".to_owned()
            }
            Some(Step::Flushing) => format!("flushing the profile history for build {build}"),
            Some(Step::Closing) => {
                "writing blaze why's ledger whole as the daemon stops".to_owned()
            }
            Some(Step::FlushingFull) => {
                format!(
                    "flushing the profile history: its drain took over {} rows",
                    history::FLUSH_ROWS
                )
            }
            Some(Step::FlushingAsked) => {
                "flushing the profile history for a reader that asked".to_owned()
            }
            None => format!("in step {}, which this blaze does not know", it >> 56),
        }
    }
}

/// What the history's thread tells the loop of `blaze why`'s ledger.
enum Told {
    /// The restore ended, short for `torn` when it was.
    Restored { torn: Option<String>, dropped: Option<u64> },
    /// A build's misses explained.
    Explained { answer: Answer, dropped: Option<u64> },
    /// The flush a reader asked for ([`ToHistory::Flush`]) committed, with its number, or
    /// failed, saying why.
    Flushed { to: RequestRef, seq: Result<u64, String> },
    /// A client's rows written, or why they were not: what its request is answered.
    ClientWritten { to: RequestRef, written: Result<(), String> },
}

/// A build that ended, as the history's rows: its own, each image it linked, and an edge of
/// its crate graph each.
pub struct Ended {
    pub row: BuildRow,
    pub links: Vec<LinkRow>,
    pub crate_deps: Vec<CrateDepRow>,
    /// Why each thing it computed again was not reused, its own alone, sorted and each once
    /// ([`blaze_kernel::Miss`]): what `blaze why` explains.
    pub misses: Vec<blaze_kernel::Miss>,
    /// The tests its runs finished, one row each.
    pub test_runs: Vec<blaze_profile::facts::TestRunRow>,
    /// Its front end's run log: each memo that ran again and why, each untracked read by name
    /// ([`provenance_rows`]).
    pub provenance: Vec<ProvenanceRow>,
}

/// What the loop gives the `blaze-history` thread: a build as it starts, and as it ends, and
/// a reader's ask for a flush.
enum ToHistory {
    /// Its row with outcome running and no end, written at once: the row a build has if its
    /// daemon dies before it ends ([`history::writes`]).
    Started(BuildRow),
    Ended(Finished),
    /// Flush now, and tell the loop the flush's number ([`Told::Flushed`]): every row recorded
    /// before the ask is drained by this flush, so a reader that reads the manifest it
    /// committed reads them.
    Flush(RequestRef),
    /// Flush now for the explorer's next query, and send the flush's number (or why it failed)
    /// to the explorer's thread that waits for it ([`explorer_flush`]).
    FlushFor(Sender<Result<u64, String>>),
    /// What the client of a build sent as it ended ([`blaze_api::Request::ClientRows`]),
    /// written at once and `to` answered once it is ([`Told::ClientWritten`]).
    Client {
        to: RequestRef,
        rows: history::ClientRows,
    },
}

/// How long a reader's flush ([`blaze_api::Request::Flush`]) may take, on the loop's clock,
/// before it fails naming what the history's thread is doing (rule 18). A flush is about
/// 10-100 ms; a build's end flush, which merges, a few seconds.
pub const FLUSH_WAIT: Duration = Duration::from_secs(30);

/// What the `blaze-history` thread writes of a build that ended: its rows (with its misses,
/// which it explains against `blaze why`'s ledger, and its run log) and what the query engine
/// counted by its end.
struct Finished {
    ended: Ended,
    query_counts: Vec<QueryCountRow>,
}

/// A front end's run log, as `build`'s `provenance` rows: each memo that ran again and its
/// cause, each untracked read by name. Taken on the loop as the build ends, with its database
/// ([`super::work::Writer::run_log`]); sorted by the engine, so the rows are the same each time.
pub(super) fn provenance_rows(build: u64, taken: blaze_query::db::Taken) -> Vec<ProvenanceRow> {
    let blaze_query::db::Taken { rows, dropped } = taken;
    if dropped > 0 {
        // Never silent (rule 3): the counts in `query_counts` still hold every run.
        tracing::warn!(
            target: "blaze",
            build,
            dropped,
            "the query engine's run log kept its first {} runs; the rest of build {build}'s are in `query_counts` alone",
            blaze_query::db::LOG_MOST,
        );
    }
    // alloc: once a build, a row a memo that ran again
    rows.into_iter().map(|run| provenance_row(build, run)).collect()
}

fn provenance_row(build_id: u64, run: blaze_query::db::LoggedRun) -> ProvenanceRow {
    use blaze_query::db::{Cause, Logged};
    let blaze_query::db::LoggedRun { query, key, view, why, read, read_kind } = run;
    let (outcome, cause, what) = match why {
        Logged::Ran(cause) => {
            let name = match cause {
                Cause::Changed(_) => "changed",
                Cause::New => "new",
                Cause::Evicted => "evicted",
                Cause::Untracked => "untracked",
                Cause::Cycle => "cycle",
            };
            (RunOutcome::Ran, Some(name), None)
        }
        Logged::UntrackedRead(what) => (RunOutcome::UntrackedRead, None, Some(what)),
    };
    ProvenanceRow {
        build_id,
        query_key: key,
        kind: query,
        outcome,
        cause,
        view,
        cause_key: read,
        cause_kind: read_kind,
        what,
    }
}

/// The parts of the key of each thing `build` computed again, one record a miss (a `key_parts`
/// row a part, made only as the history writes them): what two builds' keys are diffed by,
/// across homes too.
fn key_parts(build_id: u64, misses: &[blaze_kernel::Miss]) -> Vec<KeyedMiss> {
    let keyed = |miss: &blaze_kernel::Miss| KeyedMiss {
        build_id,
        step: miss.step,
        item: miss.item.clone(), // clone: the record's own text, once a miss
        key: format!("{:x}", miss.key), // alloc: the record's text, once a miss
        // alloc: once a miss, a part each part of its key
        parts: miss
            .parts
            .iter()
            .map(|part| KeyPart {
                name: part.name.clone(), // clone: a part's own text
                hash: part.hash.map(|it| format!("{it:x}")), // alloc: a part's text
                names: part.names.clone(), // clone: a part's own text
            })
            .collect(),
        beyond: miss.beyond,
    };
    // alloc: once a build, a record a miss
    misses.iter().map(keyed).collect()
}

/// What the query engine counted since the last take, as `build`'s `query_counts` rows. Taken
/// on the loop as the build ends, so the next build's runs are its own.
fn query_counts(build: u64) -> Vec<QueryCountRow> {
    // alloc: once a build, a row a query function that did anything
    let mut rows = Vec::new();
    blaze_query::db::take_counts(|query, counts| rows.push(query_count_row(build, query, counts)));
    rows
}

fn query_count_row(
    build_id: u64,
    query: &'static str,
    counts: blaze_query::db::Counts,
) -> QueryCountRow {
    let blaze_query::db::Counts {
        executed,
        verified,
        reran,
        evicted_asked,
        evicted,
        dropped,
        reads_recorded,
        reads_kept,
        reads_most,
        written,
        ran_new,
        ran_changed,
        ran_untracked,
        ran_cycle,
    } = counts;
    QueryCountRow {
        build_id,
        query,
        executed,
        verified,
        reran,
        evicted_asked,
        evicted,
        dropped,
        reads_recorded,
        reads_kept,
        reads_most,
        written,
        ran_new,
        ran_changed,
        ran_untracked,
        ran_cycle,
    }
}

/// The `blaze-history` thread: it ends with a last flush once the daemon (its [`Profiling`])
/// is dropped, and `blaze serve` joins it then.
pub struct Keeper(JoinHandle<()>);

impl Keeper {
    pub fn join(self) -> Result<(), String> {
        self.0
            .join()
            .map_err(|_| "the profile history's thread (`blaze-history`) panicked".to_owned()) // alloc: error path
    }
}

/// Opens the history in `state` on the thread `blaze-history`, which keeps it from then on,
/// writing DEBUG events by `events` ([`blaze_profile::logs::Policy`]); a history that does not
/// open is the error, which fails `blaze serve`.
pub fn keep_history(
    state: &Path,
    duckdb: Option<PathBuf>,
    events: blaze_profile::logs::Policy,
) -> Result<(Profiling, Keeper), String> {
    let dir = history::dir_in(state);
    let owner = claim(&dir).map_err(|it| it.to_string())?;
    let (ended, rows) = crossbeam_channel::unbounded();
    let (failed_to, failed) = crossbeam_channel::unbounded();
    let (opened_to, opened) = crossbeam_channel::bounded(1);
    let (told_to, told) = crossbeam_channel::unbounded();
    let doing = Arc::new(Doing::default()); // alloc: once a daemon
    // clone: a path and a handle, once a daemon
    let (at, now) = (dir.clone(), Arc::clone(&doing));
    let thread = std::thread::Builder::new()
        .name("blaze-history".to_owned()) // alloc: once a daemon
        .spawn(move || {
            // Held until the last flush is written: a daemon after this one numbers its builds
            // from what this one wrote.
            let _owner = owner;
            match History::open(&at, history::CAP_BYTES, blaze_profile::sampler::process()) {
                Ok(mut history) => {
                    history.events_policy(events);
                    // The daemon's own panics go to its history, and a fatal signal of it to
                    // `crash.<pid>` beside it, which the next daemon's history turns into a row.
                    history.record_panics();
                    if let Err(why) = blaze_profile::crash::arm(&at) {
                        tracing::warn!("{why}");
                        eprintln!("blaze serve: {why}");
                    }
                    // The recorder's rows outlive a crash from here on: the next daemon writes
                    // what this one had not flushed. One that can't be kept is said, not fatal.
                    if let Err(error) = history.persist_recorder() {
                        tracing::warn!("{error}");
                        _ = failed_to.send(error.to_string()); // alloc: error path, once a daemon
                    }
                    // Every child the drivers reap from now on is a `processes` row of the
                    // history's next flush: installed only where a history takes them.
                    blaze_profile::facts::observe_children();
                    _ = opened_to.send(Ok(history.next_build()));
                    // Off the daemon's start path: builds that end meanwhile wait in `rows`.
                    now.set(Step::Restoring, 0);
                    let (mut why, said) = File::restore(&at);
                    if let Some(said) = said.said {
                        tracing::warn!("{said}");
                        _ = failed_to.send(said);
                    }
                    _ = told_to.send(Told::Restored { torn: said.torn, dropped: why.dropped() });
                    keep(history, &mut why, &rows, &failed_to, &told_to, &now);
                }
                Err(error) => _ = opened_to.send(Err(error.to_string())), // alloc: error path, once a daemon
            }
        })
        .map_err(|error| {
            format!("starting the profile history's thread (`blaze-history`): {error}")
        })?; // alloc: error path
    let first_build =
        match opened.recv() {
            Ok(Ok(first)) => first,
            Ok(Err(why)) => return Err(why),
            Err(_) => return Err(
                "the profile history's thread (`blaze-history`) ended before it opened the history"
                    .to_owned(),
            ), // alloc: error path
        };
    // A daemon that keeps a history logs why each memo ran again, for its `provenance` rows
    // ([`provenance_rows`]); one that keeps none counts alone.
    blaze_query::db::log_runs(true);
    Ok((Profiling { dir, duckdb, first_build, ended, failed, told, doing }, Keeper(thread)))
}

/// A history another process writes: build ids are numbered from what a history holds, so a
/// second writer would give its builds the first one's ids.
#[derive(Debug)]
pub enum Claim {
    Owned { dir: PathBuf, holder: Option<u32> },
    /// The owner file stayed held past [`history::RECOVERY_WAIT`] though the pid it names is
    /// not alive: a reader writing a dead daemon's last rows ([`history::recover_dead`]) that
    /// took far longer than it should, or is stuck.
    Recovering { dir: PathBuf, waited: Duration },
    Io { path: PathBuf, doing: &'static str, error: std::io::Error },
}

impl std::fmt::Display for Claim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Claim::Owned { dir, holder } => {
                write!(
                    f,
                    "the profile history {} is written by another blaze serve (",
                    dir.display()
                )?;
                match holder {
                    Some(pid) => write!(f, "pid {pid}")?,
                    None => f.write_str("its pid unread")?,
                }
                write!(
                    f,
                    ", holding {OWNER}): two daemons over one history would give builds the same id"
                )
            }
            Claim::Recovering { dir, waited } => write!(
                f,
                "the profile history {} stayed held ({OWNER}) for {waited:?} by a reader writing a \
                 dead daemon's last rows, and no live blaze serve: a daemon waits at most \
                 {:?} for that before it starts",
                dir.display(),
                history::RECOVERY_WAIT
            ),
            Claim::Io { path, doing, error } => write!(f, "{doing} {}: {error}", path.display()),
        }
    }
}

/// A claim on a history ([`claim`]): its [`OWNER`] file, locked until this drops, and let go
/// of then though a child forked meanwhile still has it open ([`stdx::file_lock`]: a test's
/// claim, dropped while another test spawned a process, was refused as still held by this pid).
#[derive(Debug)]
struct Owner {
    _lock: stdx::file_lock::Locked,
}

/// This process's claim on the history in `dir`, for as long as the [`Owner`] lives: an
/// exclusive lock on its [`OWNER`] file, with the pid written in it for the next claimant to
/// name. A lock held while the pid in the file is not alive is a reader writing a dead
/// daemon's last rows ([`history::recover_dead`], which writes no pid): waited for, at most
/// [`history::RECOVERY_WAIT`]. One held by a live pid is another daemon: refused at once.
fn claim(dir: &Path) -> Result<Owner, Claim> {
    let io = |path: &Path, doing| {
        let path = path.to_path_buf(); // alloc: error path
        move |error| Claim::Io { path, doing, error }
    };
    fs::create_dir_all(dir).map_err(io(dir, "making the profile history"))?;
    let path = dir.join(OWNER);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(io(&path, "opening the profile history's owner file"))?;
    let held = match stdx::file_lock::Locked::try_lock(file) {
        Ok(Ok(file)) => Ok(file),
        Ok(Err(held)) => Err(held),
        Err(error) => return Err(io(&path, "locking the profile history's owner file")(error)),
    };
    let mut file = match held {
        Ok(file) => file,
        Err(held) => {
            let holder: Option<u32> =
                fs::read_to_string(&path).ok().and_then(|it| it.trim().parse().ok());
            if holder.is_some_and(history::alive) {
                return Err(Claim::Owned { dir: dir.to_path_buf(), holder });
            }
            let _span = tracing::info_span!(
                target: "blaze",
                "waits on the profile history's owner lock, held by a reader recovering a dead daemon's rows",
                dir = %dir.display(),
                dead = ?holder,
            )
            .entered();
            let started = Instant::now();
            match history::lock_within(held, history::RECOVERY_WAIT) {
                Ok(Ok(file)) => file,
                Ok(Err(_held)) => {
                    // A live daemon may have taken it and let go meanwhile; the pid now in
                    // the file says which refusal this is.
                    let holder: Option<u32> =
                        fs::read_to_string(&path).ok().and_then(|it| it.trim().parse().ok());
                    if holder.is_some_and(history::alive) {
                        return Err(Claim::Owned { dir: dir.to_path_buf(), holder });
                    }
                    let waited = started.elapsed();
                    return Err(Claim::Recovering { dir: dir.to_path_buf(), waited });
                }
                Err(error) => {
                    return Err(io(&path, "locking the profile history's owner file")(error));
                }
            }
        }
    };
    use std::io::Write as _;
    file.set_len(0).map_err(io(&path, "writing the profile history's owner file"))?;
    write!(file, "{}", std::process::id())
        .map_err(io(&path, "writing the profile history's owner file"))?;
    Ok(Owner { _lock: file })
}

/// The history's loop: a flush every [`history::EVERY`], one each time the history's drain
/// thread took [`history::FLUSH_ROWS`] (it drains the rings every [`history::DRAIN`] while this
/// thread writes), and one for each build as it ends, which writes its row and gives its spans
/// its commit. A flush that fails is logged and told to the daemon, for `blaze daemon status`;
/// the next one goes on.
fn keep(
    mut history: History,
    why: &mut File,
    rows: &Receiver<ToHistory>,
    failed: &Sender<String>,
    told: &Sender<Told>,
    doing: &Doing,
) {
    let mut flushed_at = Instant::now();
    loop {
        doing.set(Step::Waiting, 0);
        let due = history::EVERY.saturating_sub(flushed_at.elapsed());
        let next = crossbeam_channel::select! {
            recv(rows) -> it => it.map_err(|_| RecvTimeoutError::Disconnected),
            recv(history.full()) -> _ => {
                doing.set(Step::FlushingFull, 0);
                if let Err(error) = blaze_mem::frame::task(blaze_mem::Tag::Profile, |_| history.flush(None)) {
                    tracing::error!("{error}");
                    _ = failed.send(error.to_string());  // alloc: error path, a flush that failed
                }
                tell_failed(&mut history, failed);
                flushed_at = Instant::now();
                continue;
            }
            default(due) => Err(RecvTimeoutError::Timeout),
        };
        // In a task of the profile's tag, so what a flush reads and keeps (the rings' rows,
        // the history's buffers) is named on the daemon's memory line.
        let (flushed, last) = blaze_mem::frame::task(blaze_mem::Tag::Profile, |_| match next {
            Ok(ToHistory::Flush(to)) => {
                doing.set(Step::FlushingAsked, 0);
                let flushed = history.flush(None);
                let seq = match &flushed {
                    Ok(it) => Ok(it.seq),
                    // alloc: error path, a flush that failed
                    Err(error) => Err(format!("the profile history's flush failed: {error}")),
                };
                _ = told.send(Told::Flushed { to, seq });
                (flushed, false)
            }
            Ok(ToHistory::FlushFor(reply)) => {
                doing.set(Step::FlushingAsked, 0);
                let flushed = history.flush(None);
                let seq = match &flushed {
                    Ok(it) => Ok(it.seq),
                    // alloc: error path, a flush that failed
                    Err(error) => Err(format!("the profile history's flush failed: {error}")),
                };
                _ = reply.send(seq);
                (flushed, false)
            }
            Ok(ToHistory::Started(row)) => {
                doing.set(Step::Flushing, row.build_id);
                history.build_started(row);
                (history.flush(None), false)
            }
            Ok(ToHistory::Client { to, rows }) => {
                let build = rows.build;
                doing.set(Step::Flushing, build);
                let flushed = history.client_rows(&rows).and_then(|()| history.flush(Some(build)));
                let written = flushed.as_ref().map(|_| ()).map_err(ToString::to_string);
                _ = told.send(Told::ClientWritten { to, written });
                (flushed, false)
            }
            Ok(ToHistory::Ended(Finished {
                ended: Ended { mut row, links, crate_deps, misses, test_runs, provenance },
                query_counts,
            })) => {
                let build = row.build_id;
                let ended_well = matches!(row.outcome, history::Outcome::Ok);
                // Before the ledger takes the misses: their keys' parts, which it drops.
                let keyed = key_parts(build, &misses);
                doing.set(Step::Explaining, build);
                let (rebuilt, misses, record) = why.explain(build, ended_well, misses);
                // clone: the answer's copy of whose build it is, once a build
                let answer = Answer {
                    build,
                    workspace: row.workspace.clone(),
                    command: row.command.clone(),
                    rebuilt,
                };
                // Sent before the record is durable: an answer may name a build whose record a
                // crash then loses (docs/v2/plan/design/why.md, "Answered before durable").
                _ = told.send(Told::Explained { answer, dropped: why.dropped() });
                doing.set(Step::Appending, build);
                if let Err(error) = why.write(build, &record) {
                    tracing::error!("{error}");
                    _ = failed.send(error);
                }
                if row.commit.is_none() {
                    row.commit = commit_of(Path::new(&row.workspace));
                }
                history.linked(links);
                history.crate_graph(crate_deps);
                history.query_counted(query_counts);
                history.ran(provenance);
                history.keyed(keyed);
                history.missed(misses);
                history.tests_ran(test_runs);
                history.build_ended(row);
                doing.set(Step::Flushing, build);
                (history.flush(Some(build)), false)
            }
            Err(RecvTimeoutError::Timeout) => {
                doing.set(Step::Flushing, 0);
                (history.flush(None), false)
            }
            Err(RecvTimeoutError::Disconnected) => {
                doing.set(Step::Closing, 0);
                if let Err(error) = why.close() {
                    tracing::error!("{error}");
                    _ = failed.send(error);
                }
                // Builds still under way as the daemon stops write the DEBUG events held for them.
                history.release_held();
                (history.flush(None), true)
            }
        });
        flushed_at = Instant::now();
        if let Err(error) = flushed {
            tracing::error!("{error}");
            _ = failed.send(error.to_string()); // alloc: error path, a flush that failed
        }
        tell_failed(&mut history, failed);
        if last {
            return;
        }
    }
}

/// Logs and tells the daemon each error of the history's last flush that came after its rows
/// were committed (a merge, the cap, a reap, a table's file that failed while the others were
/// listed: [`History::take_failed`]): the flush itself said `Ok`, its seq durable.
fn tell_failed(history: &mut History, failed: &Sender<String>) {
    for error in history.take_failed() {
        tracing::error!("{error}");
        _ = failed.send(error.to_string()); // alloc: error path, a flush step that failed
    }
}

/// The commit checked out in the git repository `workspace` is in, read from its files (a
/// worktree's too): `None` where it is in none.
fn commit_of(workspace: &Path) -> Option<String> {
    let git = workspace.ancestors().map(|it| it.join(".git")).find(|it| it.exists())?;
    let dir = match git.is_dir() {
        true => git,
        false => {
            let file = fs::read_to_string(&git).ok()?;
            let to = PathBuf::from(file.strip_prefix("gitdir:")?.trim());
            git.parent()?.join(to)
        }
    };
    let head = fs::read_to_string(dir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(name) = head.strip_prefix("ref:").map(str::trim) else { return Some(head.to_owned()) }; // alloc: once a build, on the history's thread
    let common = match fs::read_to_string(dir.join("commondir")) {
        Ok(common) => dir.join(common.trim()),
        // clone: a path, once a build
        Err(_) => dir.clone(),
    };
    for at in [&dir, &common] {
        if let Ok(hash) = fs::read_to_string(at.join(name)) {
            return Some(hash.trim().to_owned()); // alloc: once a build, on the history's thread
        }
    }
    let packed = fs::read_to_string(common.join("packed-refs")).ok()?;
    packed.lines().find_map(|line| line.strip_suffix(name)?.strip_suffix(' ').map(str::to_owned))
}

/// A connection subscribed to the live feed, and its place in the rings.
struct Subscriber {
    to: RequestRef,
    feed: Feed,
    frame: Frame,
}

/// A reader's flush given to the history's thread, failed at `until` on the loop's clock.
struct Flushing {
    to: RequestRef,
    until: Duration,
}

/// What the loop holds for the requests it answers itself.
pub struct Observe {
    profiling: Option<Profiling>,
    /// The explorer, once asked for, and whether it accepts the page's development server.
    #[cfg(feature = "native")]
    explorer: Option<(bool, blaze_explore::Explorer)>,
    live: Vec<Subscriber>,
    /// When the subscribers are sent their next frames, on the driver's clock.
    next_frame: Duration,
    /// The readers' flushes given to the history's thread and not yet answered, oldest first.
    flushing: Vec<Flushing>,
    /// The history's last error, for [`blaze_api::Status::history`].
    history_error: Option<String>,
    /// What `blaze why` is answered from: each workspace's last build's reasons.
    answers: Answers,
    /// How many builds were given to the history's thread, and how many of their reasons came
    /// back (in the order given): a `blaze why` waits in `asked` for those given before it was
    /// asked, never for builds that end after, so it is never answered from the build before.
    sent: u64,
    explained: u64,
    /// The builds given whose reasons have not come back, oldest first: what a `blaze why` past
    /// its deadline names.
    unexplained: VecDeque<u64>,
    asked: Vec<Asked>,
    /// The history's thread is gone: nothing more comes back, so nothing waits for it.
    history_gone: bool,
    /// A daemon with no history explains its builds on the loop (a simulation's).
    local: Option<Ledger>,
    /// The plane's tasks as the explorer publishes them: the plane writes them
    /// ([`super::plane::Plane::publish_tasks`]), the explorer's sessions read them.
    #[cfg(feature = "native")]
    tasks: Arc<blaze_explore::Tasks>,
}

/// A `blaze why` waiting for the reasons of builds that ended before it was asked: answered
/// once `after` builds' have come back, failed at `until` on the loop's clock.
struct Asked {
    to: RequestRef,
    cwd: Option<String>,
    item: Option<String>,
    all: bool,
    after: u64,
    until: Duration,
}

/// The explorer core the daemon serves the page: `explore-core.wasm` beside the daemon's
/// executable, as the release archive lays it out (explore-core.md decision 3). With none, the
/// explorer still serves; `hello` leaves out `core` and the `core` op answers why, naming the
/// path and the command that builds it.
#[cfg(feature = "native")]
fn explore_core() -> Result<blaze_explore::CoreWasm, String> {
    let exe = std::env::current_exe().map_err(|why| {
        format!("the daemon's executable could not be found, so neither could {} beside it: {why}", blaze_explore::CORE_FILE)
    })?;
    blaze_explore::core_file(&exe)
}

/// The explorer's flush before each query (decision 3: a query reads every row recorded
/// before it was asked, as a CLI reader's [`blaze_api::Request::Flush`] does): asks the
/// history's thread to flush ([`ToHistory::FlushFor`]) and waits for the flush's number, at
/// most [`FLUSH_WAIT`], on the explorer's own blocking thread, never the loop. Past the wait,
/// or with the history's thread gone, an error names what the thread is doing.
///
/// Queries that wait together share one flush: a query takes a ticket as it asks, and a flush
/// that took a later ticket as it began drained every row recorded before the ask, so its
/// number answers the query too. A page that asks for many tiles at once costs one flush, not
/// one each.
#[cfg(feature = "native")]
fn explorer_flush(profiling: &Profiling) -> blaze_explore::Flush {
    // clone: the history's sender and its step, once an explorer
    let (ended, doing) = (profiling.ended.clone(), Arc::clone(&profiling.doing));
    let tickets = AtomicU64::new(0);
    // The last flush done for the explorer: the ticket it took as it began, and its number.
    let last: blaze_core::lock::Lock<Option<(u64, u64)>> =
        blaze_core::lock::Lock::new("serve.observe.explorer_flush.last", None);
    blaze_explore::Flush::new(move || {
        let asked = tickets.fetch_add(1, Ordering::SeqCst);
        let _span = tracing::info_span!(target: "blaze", "wait for the profile history's flush for the explorer", asked).entered();
        let mut last = last.lock();
        if let Some((began, seq)) = *last
            && began > asked
        {
            return Ok(seq);
        }
        let began = tickets.fetch_add(1, Ordering::SeqCst);
        let (reply, answer) = crossbeam_channel::bounded(1);
        if ended.send(ToHistory::FlushFor(reply)).is_err() {
            // alloc: error path
            return Err("the profile history's thread (`blaze-history`) ended, so nothing flushes what the daemon recorded since its last flush".to_owned());
        }
        let seq = match answer.recv_timeout(FLUSH_WAIT) {
            Ok(seq) => seq?,
            // alloc: error path
            Err(RecvTimeoutError::Timeout) => {
                return Err(format!(
                    "the profile history's flush for the explorer took over {}s: its thread is {}",
                    FLUSH_WAIT.as_secs(),
                    doing.said()
                ));
            }
            // alloc: error path
            Err(RecvTimeoutError::Disconnected) => {
                return Err("the profile history's thread (`blaze-history`) ended before it flushed for the explorer".to_owned());
            }
        };
        *last = Some((began, seq));
        Ok(seq)
    })
}

impl Observe {
    pub fn new(profiling: Option<Profiling>) -> Observe {
        let answers = Answers::new(profiling.is_some());
        Observe {
            profiling,
            #[cfg(feature = "native")]
            explorer: None,
            live: Vec::new(),     // alloc: none until a subscriber
            flushing: Vec::new(), // alloc: none until a reader asks
            next_frame: Duration::ZERO,
            history_error: None,
            answers,
            sent: 0,
            explained: 0,
            unexplained: VecDeque::new(), // alloc: none until a build ends
            asked: Vec::new(),            // alloc: none until a `blaze why` waits
            history_gone: false,
            local: None,
            #[cfg(feature = "native")]
            tasks: Arc::new(blaze_explore::Tasks::new()),
        }
    }

    /// The table of the plane's tasks the explorer publishes, for the plane to write.
    #[cfg(feature = "native")]
    pub fn tasks(&self) -> Arc<blaze_explore::Tasks> {
        Arc::clone(&self.tasks)
    }

    /// One reading of the driver's clock (the plane's, `now`) against the profile's, so the
    /// explorer says the plane's times on the clock of `spans` and `open`.
    #[cfg(feature = "native")]
    pub fn clock_tasks(&self, now: Duration) {
        self.tasks.clock(super::plane::ns(now), blaze_profile::clock::mono_ns());
    }

    /// The number of the daemon's first build ([`Profiling::first_build`]); a daemon that
    /// keeps no history numbers from 1, as 0 is no build.
    pub fn first_build(&self) -> u32 {
        self.profiling.as_ref().map_or(1, |it| it.first_build)
    }

    /// The history's last error.
    pub fn history_error(&self) -> Option<&str> {
        self.history_error.as_deref()
    }

    /// A build that started, its row of outcome running with no end, for the history to write
    /// at once; dropped by a daemon that keeps none.
    pub fn started(&mut self, row: BuildRow) {
        let Some(profiling) = &self.profiling else { return };
        if profiling.ended.send(ToHistory::Started(row)).is_err() {
            self.history_error = Some(
                "the profile history's thread (`blaze-history`) is gone: no build is written"
                    .to_owned(),
            ); // alloc: error path
        }
    }

    /// A build that ended, for the history to write with what the query engine counted by
    /// now; dropped by a daemon that keeps none, which takes no counts.
    pub fn ended(&mut self, mut ended: Ended) {
        let build = ended.row.build_id;
        let Some(profiling) = &self.profiling else {
            let _span =
                tracing::info_span!(target: "blaze", "explain a build's misses", build).entered();
            let ended_well = matches!(ended.row.outcome, history::Outcome::Ok);
            let ledger = self.local.get_or_insert_default();
            let rebuilt =
                ledger.explain(build, ended_well, std::mem::take(&mut ended.misses)).rebuilt;
            let dropped = ledger.dropped();
            // clone: whose build it is, once a build of a daemon with no history
            let answer = Answer {
                build,
                workspace: ended.row.workspace.clone(),
                command: ended.row.command.clone(),
                rebuilt,
            };
            self.answers.add(answer, dropped);
            return;
        };
        let query_counts = query_counts(build);
        let sent = profiling.ended.send(ToHistory::Ended(Finished { ended, query_counts }));
        // Counted whether or not it reached the history's thread: one that never did is never
        // explained, and `blaze why` says so rather than answer with an older build's reasons
        // (rule 3).
        self.sent += 1;
        self.unexplained.push_back(build);
        if sent.is_err() {
            self.history_error = Some(
                "the profile history's thread (`blaze-history`) is gone: no build is written"
                    .to_owned(),
            ); // alloc: error path
        }
    }

    /// What the client of a build sent as it ended: handed to the history's thread, which
    /// writes it and then has `to` answered ([`Told::ClientWritten`]). A daemon that keeps no
    /// history answers at once and keeps nothing, as it drops its builds' rows ([`Observe::started`]).
    pub fn client_rows(&mut self, to: RequestRef, rows: history::ClientRows, out: &mut Vec<Reply>) {
        let Some(profiling) = &self.profiling else {
            return out.push(Reply { to, response: Response::Done { ok: true } });
        };
        if profiling.ended.send(ToHistory::Client { to, rows }).is_err() {
            let what = "the profile history's thread (`blaze-history`) is gone: the client's rows can't be written";
            fail(out, to, ApiError::new(what, Some(CLIENT_ROWS.to_owned()))); // alloc: error path
        }
    }

    /// `blaze why`: what the last build computed again and why, in groups (each listing
    /// every item with `all`), or `item`'s chain of reasons.
    /// Asked of the last build of the workspace holding `cwd`; while builds that ended before
    /// it was asked (at `now` on the loop's clock) are still being explained on the history's
    /// thread, it waits for them, up to [`WHY_WAIT`] ([`Observe::tick`]).
    pub fn rebuilt(
        &mut self,
        to: RequestRef,
        cwd: Option<String>,
        item: Option<String>,
        all: bool,
        now: Duration,
        out: &mut Vec<Reply>,
    ) {
        if self.explained >= self.sent {
            return answer(
                out,
                to,
                Response::Rebuilt(self.answers.rebuilt(cwd.as_deref(), item.as_deref(), all)),
            );
        }
        // Never answered from a build older than one that ended before it was asked (rule 3).
        match self.history_gone {
            true => fail(out, to, never_explained(self.unexplained.front().copied())),
            false => self.asked.push(Asked {
                to,
                cwd,
                item,
                all,
                after: self.sent,
                until: now + WHY_WAIT,
            }),
        }
    }

    /// `blaze explore`: the explorer started if it is not, or started again when `dev`
    /// differs from the one running (so turning it off stops accepting the development
    /// server's Origin), and its link. A start that fails fails the request and keeps none,
    /// so the next one tries again.
    #[cfg(feature = "native")]
    pub fn explore(&mut self, to: RequestRef, dev: bool, out: &mut Vec<Reply>) {
        let Some(profiling) = &self.profiling else {
            return fail(out, to, ApiError::new(NO_PROFILING, Some("blaze explore".to_owned()))); // alloc: error path
        };
        if self.explorer.as_ref().is_some_and(|(was, _)| *was != dev) {
            self.explorer = None;
        }
        if self.explorer.is_none() {
            let config = blaze_explore::Config {
                history: blaze_explore::History {
                    // clone: two paths, once an explorer
                    duckdb: profiling.duckdb.clone(),
                    dir: profiling.dir.clone(), // clone: once an explorer
                    flush: explorer_flush(profiling),
                },
                world: None,
                dev_origin: dev,
                daemon: crate::release::get().version,
                tasks: Arc::clone(&self.tasks),
                core: explore_core(),
            };
            match blaze_explore::start(config) {
                Ok(explorer) => self.explorer = Some((dev, explorer)),
                Err(error) => {
                    return fail(
                        out,
                        to,
                        ApiError::new(error.to_string(), Some("blaze explore".to_owned())),
                    );
                } // alloc: error path
            }
        }
        let Some((_, explorer)) = &self.explorer else { return };
        answer(out, to, Response::Explored { link: explorer.link() });
    }

    /// `blaze explore` in a blaze built without the explorer: refused, naming the feature.
    #[cfg(not(feature = "native"))]
    pub fn explore(&mut self, to: RequestRef, _dev: bool, out: &mut Vec<Reply>) {
        let what = match self.profiling {
            None => NO_PROFILING,
            Some(_) => NO_EXPLORER,
        };
        fail(out, to, ApiError::new(what, Some("blaze explore".to_owned()))); // alloc: error path
    }

    /// `blaze debug live`: `to` is sent a frame every [`live::EVERY`] from the next tick on.
    pub fn subscribe(&mut self, to: RequestRef, now: Duration) {
        if self.live.is_empty() {
            self.next_frame = now;
        }
        self.live.push(Subscriber { to, feed: Feed::new(), frame: Frame::default() });
    }

    /// A reader of the history asks for a flush ([`blaze_api::Request::Flush`]): given to the
    /// history's thread, which flushes at once (after what it is writing) and tells the loop
    /// the flush's number ([`Told::Flushed`]); answered [`Response::Flushed`] then, or failed
    /// past [`FLUSH_WAIT`] naming what the thread is doing.
    pub fn flush(&mut self, to: RequestRef, now: Duration, out: &mut Vec<Reply>) {
        let Some(profiling) = &self.profiling else {
            return fail(
                out,
                to,
                ApiError::new(NO_PROFILING, Some("a read of the profile history".to_owned())),
            ); // alloc: error path
        };
        if self.history_gone || profiling.ended.send(ToHistory::Flush(to)).is_err() {
            let what = "the profile history's thread (`blaze-history`) ended, so nothing flushes what the daemon recorded since its last flush";
            return fail(
                out,
                to,
                ApiError::new(what, Some("a read of the profile history".to_owned())),
            ); // alloc: error path
        }
        self.flushing.push(Flushing { to, until: now + FLUSH_WAIT });
    }

    /// `conn` is gone: its subscriptions, waiting `blaze why`s and flushes end (a flush given
    /// to the history's thread is still written; its answer goes nowhere).
    pub fn gone(&mut self, conn: Conn) {
        self.live.retain(|it| it.to.conn != conn);
        self.asked.retain(|it| it.to.conn != conn);
        self.flushing.retain(|it| it.to.conn != conn);
    }

    /// How long the loop may wait before its next [`Observe::tick`] is due: until the next
    /// frame while anyone subscribes, and no later than the first deadline of a waiting
    /// `blaze why` or flush.
    pub fn wait(&self, now: Duration) -> Option<Duration> {
        let frame = (!self.live.is_empty()).then_some(self.next_frame);
        let asked = self.asked.iter().map(|it| it.until).min();
        let flushing = self.flushing.iter().map(|it| it.until).min();
        frame.into_iter().chain(asked).chain(flushing).min().map(|at| at.saturating_sub(now))
    }

    /// One turn: the history's errors taken, the queries' answers sent, and once a tick is due
    /// each subscriber a frame. One whose connection still has bytes to send is skipped: its
    /// feed keeps its place (rows written over meanwhile are its next frame's `skipped`), so
    /// no more than one frame waits for any subscriber.
    pub fn tick(&mut self, now: Duration, waiting: impl Fn(Conn) -> bool, out: &mut Vec<Reply>) {
        if let Some(profiling) = &self.profiling {
            while let Ok(why) = profiling.failed.try_recv() {
                self.history_error = Some(why);
            }
            loop {
                match profiling.told.try_recv() {
                    Ok(Told::Restored { torn, dropped }) => self.answers.restored(torn, dropped),
                    Ok(Told::Explained { answer, dropped }) => {
                        self.answers.add(answer, dropped);
                        self.explained += 1;
                        self.unexplained.pop_front();
                    }
                    Ok(Told::Flushed { to, seq }) => {
                        // Gone or past its deadline: answered already, or to nobody.
                        let Some(at) = self.flushing.iter().position(|it| it.to == to) else {
                            continue;
                        };
                        self.flushing.remove(at);
                        match seq {
                            Ok(seq) => answer(out, to, Response::Flushed { seq }),
                            Err(why) => fail(
                                out,
                                to,
                                ApiError::new(
                                    why,
                                    Some("a read of the profile history".to_owned()),
                                ),
                            ), // alloc: error path
                        }
                    }
                    Ok(Told::ClientWritten { to, written: Ok(()) }) => {
                        out.push(Reply { to, response: Response::Done { ok: true } })
                    }
                    Ok(Told::ClientWritten { to, written: Err(why) }) => {
                        fail(out, to, ApiError::new(why, Some(CLIENT_ROWS.to_owned()))); // alloc: error path
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        // Gone, so nothing more comes back: what is held is answered now, and
                        // the status names the history's end. What its restore said stays.
                        if !self.history_gone
                            && (self.explained < self.sent || self.answers.restoring())
                        {
                            self.history_error = Some("the profile history's thread (`blaze-history`) ended: no more builds are explained".to_owned()); // alloc: error path
                        }
                        self.history_gone = true;
                        self.answers.gone();
                        break;
                    }
                }
            }
        }
        if !self.asked.is_empty() {
            let (answers, explained, gone) = (&self.answers, self.explained, self.history_gone);
            let (unexplained, doing) =
                (&self.unexplained, self.profiling.as_ref().map(|it| &*it.doing));
            self.asked.retain(|it| {
                if explained >= it.after {
                    answer(
                        out,
                        it.to,
                        Response::Rebuilt(answers.rebuilt(
                            it.cwd.as_deref(),
                            it.item.as_deref(),
                            it.all,
                        )),
                    );
                    return false;
                }
                if gone {
                    fail(out, it.to, never_explained(unexplained.front().copied()));
                    return false;
                }
                if now < it.until {
                    return true;
                }
                fail(out, it.to, late(it, explained, unexplained.front().copied(), doing));
                false
            });
        }
        if !self.flushing.is_empty() {
            let (gone, doing) = (self.history_gone, self.profiling.as_ref().map(|it| &*it.doing));
            self.flushing.retain(|it| {
                if !gone && now < it.until {
                    return true;
                }
                let doing = doing.map_or_else(|| "gone".to_owned(), Doing::said); // alloc: error path
                let what = match gone {
                    // alloc: error path
                    true => format!("the profile history's thread (`blaze-history`) ended before the flush asked for was written; it is {doing}"),
                    // alloc: error path
                    false => format!("the flush asked for was not written in {}s; the profile history's thread (`blaze-history`) is {doing}", FLUSH_WAIT.as_secs()),
                };
                fail(out, it.to, ApiError::new(what, Some("a read of the profile history".to_owned()))); // alloc: error path
                false
            });
        }
        if self.live.is_empty() || now < self.next_frame {
            return;
        }
        self.next_frame = now + live::EVERY;
        self.live.retain_mut(|it| {
            if waiting(it.to.conn) {
                return true;
            }
            if let Err(error) = it.feed.next(&mut it.frame) {
                fail(
                    out,
                    it.to,
                    ApiError::new(format!("the live feed: {error}"), Some("live".to_owned())),
                ); // alloc: error path
                return false;
            }
            let frame = &mut it.frame;
            // The frame's tables move into the response; the feed's next frame fills new ones.
            let live = LiveFrame {
                lost: frame.lost,
                open: std::mem::take(&mut frame.open),
                at_ns: frame.at_ns,
                spans: std::mem::take(&mut frame.spans),
                skipped: frame.skipped,
                counters: std::mem::take(&mut frame.counters),
            };
            out.push(Reply { to: it.to, response: Response::Live(live) });
            true
        });
    }

    /// The daemon stops: every subscription ends, every flush not yet answered fails, and the
    /// explorer closes its sessions.
    pub fn stop(&mut self, out: &mut Vec<Reply>) {
        for it in self.live.drain(..) {
            out.push(Reply { to: it.to, response: Response::Done { ok: true } });
        }
        for it in self.flushing.drain(..) {
            let what = "the daemon stopped before the flush asked for was answered; its last flush, as it stops, writes what it recorded";
            fail(out, it.to, ApiError::new(what, Some("a read of the profile history".to_owned()))); // alloc: error path
        }
        #[cfg(feature = "native")]
        {
            self.explorer = None;
        }
    }
}

const NO_PROFILING: &str = "this daemon keeps no profile history (only `blaze serve`'s does)";

/// What a client's profile rows are named as in an error.
const CLIENT_ROWS: &str = "a client's profile rows";

/// What a blaze built without blaze-cargo's `native` feature (the seed profile, which has no
/// C: the explorer serves through Flecs and blaze-http's HTTP/3 on ring) says to the explorer.
#[cfg(not(feature = "native"))]
const NO_EXPLORER: &str = "this blaze is built without blaze-cargo's `native` feature (the seed profile), so it has no \
                           explorer";

/// The error of a `blaze why` past its deadline: the build it still waits for (the oldest
/// given whose reasons have not come back) and what the history's thread is doing.
fn late(asked: &Asked, explained: u64, oldest: Option<u64>, doing: Option<&Doing>) -> ApiError {
    let build = oldest.map_or_else(|| "a build".to_owned(), |it| format!("build {it}")); // alloc: error path
    let doing = doing.map_or_else(|| "gone".to_owned(), Doing::said); // alloc: error path
    let waits = asked.after - explained;
    // alloc: error path
    let what = format!(
        "blaze why waited {}s for {build} to be explained ({waits} of the builds that ended before it was asked still are), and the profile history's thread (`blaze-history`) is {doing}",
        WHY_WAIT.as_secs()
    );
    ApiError::new(what, Some("blaze why".to_owned())) // alloc: error path
}

/// The error of a `blaze why` asked after a build that the history's thread, now ended, never
/// explained: answering it would give an older build's reasons as the last build's (rule 3).
fn never_explained(oldest: Option<u64>) -> ApiError {
    let build = oldest.map_or_else(|| "a build".to_owned(), |it| format!("build {it}")); // alloc: error path
    // alloc: error path
    let what = format!(
        "blaze why can't answer: {build} ended before it was asked and was never explained, because the profile history's thread (`blaze-history`) ended; the reasons it holds are an older build's"
    );
    ApiError::new(what, Some("blaze why".to_owned())) // alloc: error path
}

fn answer(out: &mut Vec<Reply>, to: RequestRef, response: Response) {
    out.push(Reply { to, response });
    out.push(Reply { to, response: Response::Done { ok: true } });
}

fn fail(out: &mut Vec<Reply>, to: RequestRef, error: ApiError) {
    out.push(Reply { to, response: Response::Error(error) });
    out.push(Reply { to, response: Response::Done { ok: false } });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_history_has_one_writer_and_a_second_claimant_is_told_who() {
        let dir = tempfile::tempdir().unwrap();
        let history = dir.path().join("profile");
        let first = claim(&history).unwrap();
        match claim(&history) {
            Err(Claim::Owned { dir, holder }) => {
                assert_eq!(dir, history);
                assert_eq!(holder, Some(std::process::id()));
            }
            other => panic!("a second claim on a held history: {other:?}"),
        }
        let said = claim(&history).unwrap_err().to_string();
        assert!(
            said.contains(&history.display().to_string())
                && said.contains(&format!("pid {}", std::process::id())),
            "{said}"
        );
        drop(first);
        claim(&history).expect("a history its writer let go of is claimed again");
    }

    #[test]
    fn duckdb_unset_is_none_and_set_wrong_is_an_error_naming_it() {
        assert_eq!(duckdb(None), Ok(None));
        let relative = duckdb(Some("bin/duckdb".into())).unwrap_err();
        assert!(
            relative.contains("BLAZE_DUCKDB=bin/duckdb") && relative.contains("absolute"),
            "{relative}"
        );
        let dir = tempfile::tempdir().unwrap();
        let not_a_file = duckdb(Some(dir.path().into())).unwrap_err();
        assert!(not_a_file.contains("is not a file"), "{not_a_file}");
        let missing = duckdb(Some(dir.path().join("duckdb").into())).unwrap_err();
        assert!(missing.starts_with("BLAZE_DUCKDB="), "{missing}");
        let file = dir.path().join("duckdb");
        fs::write(&file, b"").unwrap();
        assert_eq!(duckdb(Some(file.clone().into())), Ok(Some(file)));
    }

    #[test]
    fn a_commit_is_read_from_a_branch_a_packed_ref_a_detached_head_and_a_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let git = repo.join(".git");
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::create_dir_all(repo.join("crates/a")).unwrap();
        assert_eq!(commit_of(&repo), None, "a .git with no HEAD");
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(git.join("refs/heads/main"), "aaaa\n").unwrap();
        assert_eq!(
            commit_of(&repo.join("crates/a")).as_deref(),
            Some("aaaa"),
            "found from a directory inside"
        );
        fs::write(git.join("HEAD"), "ref: refs/heads/packed\n").unwrap();
        fs::write(git.join("packed-refs"), "# pack-refs with: peeled\nbbbb refs/heads/packed\n")
            .unwrap();
        assert_eq!(commit_of(&repo).as_deref(), Some("bbbb"));
        fs::write(git.join("HEAD"), "cccc\n").unwrap();
        assert_eq!(commit_of(&repo).as_deref(), Some("cccc"), "detached");

        let worktree = dir.path().join("tree");
        let own = git.join("worktrees/tree");
        fs::create_dir_all(&own).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        fs::write(worktree.join(".git"), format!("gitdir: {}\n", own.display())).unwrap();
        fs::write(own.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(own.join("commondir"), "../..\n").unwrap();
        assert_eq!(
            commit_of(&worktree).as_deref(),
            Some("aaaa"),
            "a worktree's branch is the common directory's"
        );
    }

    /// The engine's counts, mapped field for field, go with the build's row through the
    /// history thread into that build's `query_counts` file; the last flush, for no build,
    /// writes none.
    #[test]
    fn a_builds_query_counts_reach_its_query_counts_table() {
        let dir = tempfile::tempdir().unwrap();
        // A ring of the test's own: the process's is a sampler's wherever one runs.
        static UNSAMPLED: blaze_profile::sampler::Ring = blaze_profile::sampler::Ring::new();
        let history = History::open(dir.path(), history::CAP_BYTES, &UNSAMPLED).unwrap();
        let counts = blaze_query::db::Counts {
            executed: 6384,
            verified: 1,
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
        let row = query_count_row(71, "mir_body_query", counts);
        assert_eq!(
            [
                row.executed,
                row.verified,
                row.reran,
                row.evicted_asked,
                row.evicted,
                row.dropped,
                row.reads_recorded,
                row.reads_kept,
                row.reads_most,
                row.written,
                row.ran_new,
                row.ran_changed,
                row.ran_untracked,
                row.ran_cycle
            ],
            [6384, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]
        );
        let ended = Ended {
            row: BuildRow {
                build_id: 71,
                commit: Some("abc".to_owned()),
                workspace: dir.path().join("no-workspace").display().to_string(),
                command: "build".to_owned(),
                started_ns: 0,
                dur_ns: Some(1),
                started_unix_ns: None,
                outcome: history::Outcome::Ok,
                hits: None,
                misses: None,
                joined: None,
                tree: None,
                blaze: None,
                client_ns: None,
                daemon_cpu_ns: None,
            },
            links: Vec::new(),
            crate_deps: Vec::new(),
            misses: Vec::new(),
            test_runs: Vec::new(),
            provenance: Vec::new(),
        };
        let (to, rows) = crossbeam_channel::unbounded();
        let (failed_to, failed) = crossbeam_channel::unbounded();
        to.send(ToHistory::Ended(Finished { ended, query_counts: vec![row] })).unwrap();
        drop(to);
        let (mut why, _) = File::restore(dir.path());
        let (told_to, told) = crossbeam_channel::unbounded();
        keep(history, &mut why, &rows, &failed_to, &told_to, &Doing::default());
        assert_eq!(failed.try_recv().ok(), None);
        assert!(matches!(
            told.try_recv(),
            Ok(Told::Explained { answer: Answer { build: 71, .. }, .. })
        ));
        let written: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|it| it.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|it| it.ends_with(".query_counts.parquet") && !it.starts_with("schema."))
            .collect();
        assert_eq!(written.len(), 1, "{written:?}");
        assert!(written[0].starts_with("71-"), "{written:?}");
    }

    fn to(conn: u64, id: u64) -> RequestRef {
        RequestRef { conn: Conn(conn), id }
    }

    /// A reader's flush goes to the history's thread, which flushes and tells the loop the
    /// flush's number; the manifest says that flush once it is told.
    #[test]
    fn a_readers_flush_is_written_then_told_with_its_number() {
        let dir = tempfile::tempdir().unwrap();
        static UNSAMPLED: blaze_profile::sampler::Ring = blaze_profile::sampler::Ring::new();
        let history = History::open(dir.path(), history::CAP_BYTES, &UNSAMPLED).unwrap();
        let (sent, rows) = crossbeam_channel::unbounded();
        let (failed_to, failed) = crossbeam_channel::unbounded();
        sent.send(ToHistory::Flush(to(4, 9))).unwrap();
        drop(sent);
        let (mut why, _) = File::restore(dir.path());
        let (told_to, told) = crossbeam_channel::unbounded();
        keep(history, &mut why, &rows, &failed_to, &told_to, &Doing::default());
        assert_eq!(failed.try_recv().ok(), None);
        let Ok(Told::Flushed { to: asker, seq: Ok(seq) }) = told.try_recv() else {
            panic!("the flush was told")
        };
        assert_eq!(asker, to(4, 9));
        assert!(
            history::read_manifest(dir.path()).unwrap().flushed() >= seq,
            "the manifest says the flush told"
        );
    }

    /// A flush the history's thread never answers fails at its deadline, naming what the
    /// thread is doing; `wait` wakes the loop for it.
    #[test]
    fn a_flush_past_its_deadline_fails_naming_the_history_thread() {
        let dir = tempfile::tempdir().unwrap();
        let (profiling, _keeper) =
            keep_history(dir.path(), None, blaze_profile::logs::Policy::default()).unwrap();
        let mut observe = Observe::new(Some(profiling));
        let mut out = Vec::new();
        let at = Duration::from_secs(1);
        observe.flush(to(2, 1), at, &mut out);
        assert_eq!(observe.wait(at), Some(FLUSH_WAIT));
        observe.flushing[0].until = at;
        observe.tick(at, |_| false, &mut out);
        let failed = out.iter().any(|it| matches!(&it.response, Response::Error(error) if error.what.contains("blaze-history")));
        assert!(
            failed || out.iter().any(|it| matches!(it.response, Response::Flushed { .. })),
            "{out:?}"
        );
    }

    /// Build `build` of the workspace `/w` ending well, with nothing computed again.
    fn ended_row(build: u64) -> Ended {
        let row = BuildRow {
            build_id: build,
            commit: None,
            workspace: "/w".to_owned(),
            command: "build".to_owned(),
            started_ns: 0,
            dur_ns: Some(1),
            started_unix_ns: None,
            outcome: history::Outcome::Ok,
            hits: None,
            misses: None,
            joined: None,
            tree: None,
            blaze: None,
            client_ns: None,
            daemon_cpu_ns: None,
        };
        Ended {
            row,
            links: Vec::new(),
            crate_deps: Vec::new(),
            misses: Vec::new(),
            test_runs: Vec::new(),
            provenance: Vec::new(),
        }
    }

    fn explained(build: u64) -> Told {
        let answer = Answer {
            build,
            workspace: "/w".to_owned(),
            command: "build".to_owned(),
            rebuilt: Vec::new(),
        };
        Told::Explained { answer, dropped: None }
    }

    /// A held `blaze why` waits for the builds that ended before it was asked and no others;
    /// past its deadline on the loop's clock it fails naming the build it waits for and what
    /// the history's thread is doing; and once that thread is gone, a held one and one asked
    /// after both fail naming the build it never explained.
    #[test]
    fn a_held_why_waits_for_the_builds_before_it_and_fails_past_its_deadline_naming_them() {
        let (ended, rows) = crossbeam_channel::unbounded();
        let (_failed_to, failed) = crossbeam_channel::unbounded();
        let (told_to, told) = crossbeam_channel::unbounded();
        let doing = Arc::new(Doing::default());
        let dir = tempfile::tempdir().unwrap();
        let profiling = Profiling {
            dir: dir.path().to_path_buf(),
            duckdb: None,
            first_build: 1,
            ended,
            failed,
            told,
            doing: Arc::clone(&doing),
        };
        let mut observe = Observe::new(Some(profiling));
        let mut out = Vec::new();
        let at = Duration::from_secs(5);
        told_to
            .send(Told::Restored {
                torn: Some("record 2 does not decode".to_owned()),
                dropped: None,
            })
            .unwrap();
        observe.ended(ended_row(1));
        observe.rebuilt(to(1, 1), Some("/w".to_owned()), None, false, at, &mut out);
        assert!(out.is_empty(), "build 1 is not explained yet: {out:?}");
        assert_eq!(observe.wait(at), Some(WHY_WAIT), "the loop wakes by the deadline");
        // A build that ends after it was asked is not waited for.
        observe.ended(ended_row(2));
        told_to.send(explained(1)).unwrap();
        observe.tick(at, |_| false, &mut out);
        match &out[..] {
            [
                Reply { response: Response::Rebuilt(it), .. },
                Reply { response: Response::Done { ok: true }, .. },
            ] => {
                assert_eq!(
                    (it.build, it.torn.as_deref(), it.restoring),
                    (Some(1), Some("record 2 does not decode"), false)
                );
            }
            other => panic!("{other:?}"),
        }
        out.clear();

        // Build 2 is never explained: past the deadline it fails, naming it and the history's
        // step.
        observe.rebuilt(to(1, 2), None, None, false, at, &mut out);
        // A held one whose connection goes is dropped, never answered.
        observe.rebuilt(to(2, 1), None, None, false, at, &mut out);
        observe.gone(Conn(2));
        doing.set(Step::Flushing, 1);
        observe.tick(at + WHY_WAIT - Duration::from_millis(1), |_| false, &mut out);
        assert!(out.is_empty(), "not due yet: {out:?}");
        observe.tick(at + WHY_WAIT, |_| false, &mut out);
        match &out[..] {
            [
                Reply { response: Response::Error(error), .. },
                Reply { response: Response::Done { ok: false }, .. },
            ] => {
                assert!(
                    error.what.contains("build 2")
                        && error.what.contains("flushing the profile history for build 1"),
                    "{}",
                    error.what
                );
                assert_eq!(error.at.as_deref(), Some("blaze why"));
            }
            other => panic!("{other:?}"),
        }
        assert!(
            out.iter().all(|it| it.to.conn == Conn(1)),
            "nothing to the connection that went: {out:?}"
        );
        out.clear();

        // The history's thread gone with build 2 unexplained: a held one, and one asked after,
        // fail naming build 2 and the thread's end, never answered with build 1's reasons.
        observe.rebuilt(to(1, 3), None, None, false, at, &mut out);
        assert!(out.is_empty());
        drop(told_to);
        observe.tick(at, |_| false, &mut out);
        observe.rebuilt(to(1, 4), None, None, false, at, &mut out);
        match &out[..] {
            [
                Reply { to: a, response: Response::Error(held) },
                Reply { response: Response::Done { ok: false }, .. },
                Reply { to: b, response: Response::Error(after) },
                Reply { response: Response::Done { ok: false }, .. },
            ] => {
                assert_eq!((*a, *b), (to(1, 3), to(1, 4)));
                for error in [held, after] {
                    assert!(
                        error.what.contains("build 2")
                            && error.what.contains("blaze-history")
                            && error.what.contains("ended"),
                        "{}",
                        error.what
                    );
                }
            }
            other => panic!("{other:?}"),
        }
        assert!(observe.history_error().is_some_and(|it| it.contains("ended")));
        drop(rows);
    }

    /// A build that ends once the history's thread is gone never reaches it, yet `blaze why`
    /// still waits for it: asked before or after the loop sees the thread's end, it fails
    /// naming that build, never answers with the last explained build's reasons.
    #[test]
    fn a_build_that_ends_after_the_history_thread_is_gone_is_named_never_explained() {
        let (ended, rows) = crossbeam_channel::unbounded();
        let (_failed_to, failed) = crossbeam_channel::unbounded();
        let (told_to, told) = crossbeam_channel::unbounded();
        let dir = tempfile::tempdir().unwrap();
        let profiling = Profiling {
            dir: dir.path().to_path_buf(),
            duckdb: None,
            first_build: 1,
            ended,
            failed,
            told,
            doing: Arc::new(Doing::default()),
        };
        let mut observe = Observe::new(Some(profiling));
        let mut out = Vec::new();
        let at = Duration::from_secs(5);
        observe.ended(ended_row(1));
        told_to.send(explained(1)).unwrap();
        observe.tick(at, |_| false, &mut out);
        assert!(out.is_empty(), "{out:?}");

        // The thread ends; the loop has not seen it yet. Build 2's send fails.
        drop(rows);
        observe.ended(ended_row(2));
        assert!(observe.history_error().is_some_and(|it| it.contains("no build is written")));
        observe.rebuilt(to(1, 1), None, None, false, at, &mut out);
        assert!(out.is_empty(), "held for build 2, not answered with build 1: {out:?}");
        drop(told_to);
        observe.tick(at, |_| false, &mut out);
        // Seen gone: build 3 ends after, and one asked then fails at once.
        observe.ended(ended_row(3));
        observe.rebuilt(to(1, 2), None, None, false, at, &mut out);
        match &out[..] {
            [
                Reply { to: a, response: Response::Error(held) },
                Reply { response: Response::Done { ok: false }, .. },
                Reply { to: b, response: Response::Error(after) },
                Reply { response: Response::Done { ok: false }, .. },
            ] => {
                assert_eq!((*a, *b), (to(1, 1), to(1, 2)));
                for error in [held, after] {
                    assert!(
                        error.what.contains("build 2")
                            && error.what.contains("blaze-history")
                            && error.what.contains("ended"),
                        "{}",
                        error.what
                    );
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_daemon_with_no_profiling_refuses_the_explorer_and_a_flush_by_name() {
        let mut observe = Observe::new(None);
        let mut out = Vec::new();
        observe.explore(to(1, 1), false, &mut out);
        observe.flush(to(1, 2), Duration::ZERO, &mut out);
        let refused: Vec<_> = out
            .iter()
            .filter_map(|it| match &it.response {
                Response::Error(error) => Some(error.at.as_deref()),
                _ => None,
            })
            .collect();
        assert_eq!(refused, [Some("blaze explore"), Some("a read of the profile history")]);
        assert_eq!(out.last().map(|it| &it.response), Some(&Response::Done { ok: false }));
        assert_eq!(observe.first_build(), 1);
    }

    #[test]
    fn a_subscriber_gets_a_frame_a_tick_and_none_while_its_connection_waits() {
        let mut observe = Observe::new(None);
        let mut out = Vec::new();
        observe.subscribe(to(3, 1), Duration::from_secs(1));
        assert_eq!(observe.wait(Duration::from_secs(1)), Some(Duration::ZERO));
        observe.tick(Duration::from_secs(1), |_| false, &mut out);
        assert!(matches!(out.as_slice(), [Reply { response: Response::Live(_), .. }]), "{out:?}");
        out.clear();
        observe.tick(Duration::from_secs(1) + live::EVERY / 2, |_| false, &mut out);
        assert!(out.is_empty(), "not due yet");
        observe.tick(Duration::from_secs(1) + live::EVERY, |_| true, &mut out);
        assert!(out.is_empty(), "skipped: its connection has a frame not yet sent");
        observe.tick(Duration::from_secs(1) + live::EVERY * 2, |_| false, &mut out);
        assert_eq!(out.len(), 1);
        observe.gone(Conn(3));
        assert_eq!(observe.wait(Duration::from_secs(9)), None, "no subscriber left");
    }
}
