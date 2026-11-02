//! What the daemon says of itself beside its builds (docs/profiling.md;
//! docs/v2/plan/lanes/observe/explore.md, "Where it runs").
//!
//! - [`Profiling`]: the profile history, which the thread `blaze-history` keeps off the loop
//!   ([`keep_history`]), and the DuckDB `nix run .#duckdb` pins ([`duckdb`]). `blaze serve`
//!   alone has them; a simulation's daemon has none, and the requests that read them fail
//!   saying so.
//! - [`Observe`]: what the loop holds for the requests it answers itself: the explorer
//!   (`blaze explore`), each live subscriber's feed (`blaze debug live`) and the live SQL
//!   queries, each run on a thread of its own (`blaze debug sql --live`).
//!
//! The explorer's link carries its session's token: it goes to the client in
//! [`Response::Explored`] and nowhere else, never to a log.

use std::{
    fs,
    path::{Path, PathBuf},
    thread::JoinHandle,
    time::Duration,
};

use blaze_api::{ApiError, LiveFrame, Reply, RequestRef, Response};
use blaze_core::Conn;
use blaze_profile::{
    history::{self, BuildRow, History},
    live::{self, Feed, Frame},
    sql,
};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

/// The variable naming the DuckDB `blaze debug sql` and the explorer run: an absolute path,
/// which blaze's Nix wrapper sets to `nix run .#duckdb`'s. Never looked up on `PATH`.
pub const DUCKDB: &str = "BLAZE_DUCKDB";

/// The DuckDB [`DUCKDB`] names: `None` unset, and an error naming the variable and its value
/// when set to anything but an absolute path to a file.
pub fn duckdb(value: Option<std::ffi::OsString>) -> Result<Option<PathBuf>, String> {
    let Some(value) = value else { return Ok(None) };
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(format!(  // alloc: error path
            "{DUCKDB}={} is not an absolute path: it names the DuckDB `nix run .#duckdb` pins",
            path.display()
        ));
    }
    match fs::metadata(&path) {
        Ok(meta) if meta.is_file() => Ok(Some(path)),
        Ok(_) => Err(format!("{DUCKDB}={} is not a file", path.display())),  // alloc: error path
        Err(error) => Err(format!("{DUCKDB}={}: {error}", path.display())),  // alloc: error path
    }
}

/// `blaze debug hot`'s answer: the daemon's live sketches ([`blaze_profile::hot::read`]) in the
/// API's terms.
pub fn hot(top: u32, since: blaze_api::HotSince) -> Result<blaze_api::Hot, ApiError> {
    use blaze_api::{HotFunction, HotKey, HotSince, HotWait, HotWaitKind};
    use blaze_graph::wait::WaitKind;
    use blaze_profile::hot;
    let read = hot::read(top as usize, match since {
        HotSince::Start => hot::Since::Start,
        HotSince::LastBuild => hot::Since::LastBuild,
    })
    // alloc: error path
    .map_err(|error| ApiError::new(error.to_string(), Some("blaze debug hot --last-build".to_owned())))?;
    let keys = read
        .keys
        .into_iter()
        .map(|it| HotKey {
            span: it.span.to_owned(),  // alloc: the answer's copy, once a request
            function: match it.function {
                hot::Function::None => HotFunction::None,
                hot::Function::Named(name) => HotFunction::Named(name.as_ref().to_owned()),  // alloc: as above
                hot::Function::Forgotten(sym) => HotFunction::Forgotten(sym),
            },
            lower_ns: it.lower_ns,
            upper_ns: it.upper_ns,
        })
        .collect();  // alloc: the answer's rows, once a request
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
        .collect();  // alloc: the answer's rows, once a request
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
    ended: Sender<BuildRow>,
    failed: Receiver<String>,
}

/// The `blaze-history` thread: it ends with a last flush once the daemon (its [`Profiling`])
/// is dropped, and `blaze serve` joins it then.
pub struct Keeper(JoinHandle<()>);

impl Keeper {
    pub fn join(self) -> Result<(), String> {
        self.0.join().map_err(|_| "the profile history's thread (`blaze-history`) panicked".to_owned())  // alloc: error path
    }
}

/// Opens the history in `state` on the thread `blaze-history`, which keeps it from then on;
/// a history that does not open is the error, which fails `blaze serve`.
pub fn keep_history(state: &Path, duckdb: Option<PathBuf>) -> Result<(Profiling, Keeper), String> {
    let dir = history::dir_in(state);
    let (ended, rows) = crossbeam_channel::unbounded();
    let (failed_to, failed) = crossbeam_channel::unbounded();
    let (opened_to, opened) = crossbeam_channel::bounded(1);
    // clone: a path, once a daemon
    let at = dir.clone();
    let thread = std::thread::Builder::new()
        .name("blaze-history".to_owned())  // alloc: once a daemon
        .spawn(move || match History::open(&at, history::CAP_BYTES) {
            Ok(history) => {
                _ = opened_to.send(Ok(history.next_build()));
                keep(history, &rows, &failed_to);
            }
            Err(error) => _ = opened_to.send(Err(error.to_string())),  // alloc: error path, once a daemon
        })
        .map_err(|error| format!("starting the profile history's thread (`blaze-history`): {error}"))?;  // alloc: error path
    let first_build = match opened.recv() {
        Ok(Ok(first)) => first,
        Ok(Err(why)) => return Err(why),
        Err(_) => return Err("the profile history's thread (`blaze-history`) ended before it opened the history".to_owned()),  // alloc: error path
    };
    Ok((Profiling { dir, duckdb, first_build, ended, failed }, Keeper(thread)))
}

/// The history's loop: a flush every [`history::EVERY`], and one for each build as it ends,
/// which writes its row and gives its spans its commit. A flush that fails is logged and told
/// to the daemon, for `blaze daemon status`; the next one goes on.
fn keep(mut history: History, rows: &Receiver<BuildRow>, failed: &Sender<String>) {
    loop {
        let (flushed, last) = match rows.recv_timeout(history::EVERY) {
            Ok(mut row) => {
                let build = row.build_id;
                if row.commit.is_none() {
                    row.commit = commit_of(Path::new(&row.workspace));
                }
                history.build_ended(row);
                (history.flush(Some(build)), false)
            }
            Err(RecvTimeoutError::Timeout) => (history.flush(None), false),
            Err(RecvTimeoutError::Disconnected) => (history.flush(None), true),
        };
        if let Err(error) = flushed {
            tracing::error!("{error}");
            _ = failed.send(error.to_string());  // alloc: error path, a flush that failed
        }
        if last {
            return;
        }
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
    let Some(name) = head.strip_prefix("ref:").map(str::trim) else { return Some(head.to_owned()) };  // alloc: once a build, on the history's thread
    let common = match fs::read_to_string(dir.join("commondir")) {
        Ok(common) => dir.join(common.trim()),
        // clone: a path, once a build
        Err(_) => dir.clone(),
    };
    for at in [&dir, &common] {
        if let Ok(hash) = fs::read_to_string(at.join(name)) {
            return Some(hash.trim().to_owned());  // alloc: once a build, on the history's thread
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

/// A live SQL query running on its thread: a message on `stop` kills its DuckDB.
struct Querying {
    to: RequestRef,
    stop: crossbeam_channel::Sender<()>,
}

/// What a query's thread answered.
struct Answered {
    to: RequestRef,
    answer: Result<(Vec<u8>, u64), ApiError>,
}

/// What the loop holds for the requests it answers itself.
pub struct Observe {
    profiling: Option<Profiling>,
    /// The explorer, once asked for, and whether it accepts the page's development server.
    explorer: Option<(bool, blaze_explore::Explorer)>,
    live: Vec<Subscriber>,
    /// When the subscribers are sent their next frames, on the driver's clock.
    next_frame: Duration,
    querying: Vec<Querying>,
    answered_to: Sender<Answered>,
    answered: Receiver<Answered>,
    /// The history's last error, for [`blaze_api::Status::history`].
    history_error: Option<String>,
}

impl Observe {
    pub fn new(profiling: Option<Profiling>) -> Observe {
        let (answered_to, answered) = crossbeam_channel::unbounded();
        Observe {
            profiling,
            explorer: None,
            live: Vec::new(),    // alloc: none until a subscriber
            querying: Vec::new(), // alloc: none until a query
            next_frame: Duration::ZERO,
            answered_to,
            answered,
            history_error: None,
        }
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

    /// A build that ended, for the history to write; dropped by a daemon that keeps none.
    pub fn ended(&mut self, row: BuildRow) {
        let Some(profiling) = &self.profiling else { return };
        if profiling.ended.send(row).is_err() {
            self.history_error = Some("the profile history's thread (`blaze-history`) is gone: no build is written".to_owned());  // alloc: error path
        }
    }

    /// `blaze explore`: the explorer started if it is not, or started again when `dev`
    /// differs from the one running (so turning it off stops accepting the development
    /// server's Origin), and its link. A start that fails fails the request and keeps none,
    /// so the next one tries again.
    pub fn explore(&mut self, to: RequestRef, dev: bool, out: &mut Vec<Reply>) {
        let Some(profiling) = &self.profiling else {
            return fail(out, to, ApiError::new(NO_PROFILING, Some("blaze explore".to_owned())));  // alloc: error path
        };
        if self.explorer.as_ref().is_some_and(|(was, _)| *was != dev) {
            self.explorer = None;
        }
        if self.explorer.is_none() {
            let config = blaze_explore::Config {
                history: blaze_explore::History {
                    // clone: two paths, once an explorer
                    duckdb: profiling.duckdb.clone(),
                    dir: profiling.dir.clone(),  // clone: once an explorer
                },
                world: None,
                dev_origin: dev,
                daemon: crate::release::get().version,
            };
            match blaze_explore::start(config) {
                Ok(explorer) => self.explorer = Some((dev, explorer)),
                Err(error) => return fail(out, to, ApiError::new(error.to_string(), Some("blaze explore".to_owned()))),  // alloc: error path
            }
        }
        let Some((_, explorer)) = &self.explorer else { return };
        answer(out, to, Response::Explored { link: explorer.link() });
    }

    /// `blaze debug live`: `to` is sent a frame every [`live::EVERY`] from the next tick on.
    pub fn subscribe(&mut self, to: RequestRef, now: Duration) {
        if self.live.is_empty() {
            self.next_frame = now;
        }
        self.live.push(Subscriber { to, feed: Feed::new(), frame: Frame::default() });
    }

    /// `blaze debug sql --live`: `sql` over the history and the last `last_s` seconds of the
    /// flight recorder, run on a thread of its own (DuckDB's run is never the loop's).
    pub fn query(&mut self, to: RequestRef, sql: String, last_s: u32, out: &mut Vec<Reply>) {
        let Some(profiling) = &self.profiling else {
            return fail(out, to, ApiError::new(NO_PROFILING, Some("blaze debug sql --live".to_owned())));  // alloc: error path
        };
        let Some(duckdb) = &profiling.duckdb else {
            let what = format!("the daemon was started with no {DUCKDB}: `nix run .#duckdb` names the DuckDB it runs");  // alloc: error path
            return fail(out, to, ApiError::new(what, Some("blaze debug sql --live".to_owned())));  // alloc: error path
        };
        let (stop, stopped) = crossbeam_channel::bounded(1);  // alloc: once a query
        // clone: once a query, for its thread
        let (duckdb, dir, answered) = (duckdb.clone(), profiling.dir.clone(), self.answered_to.clone());
        let window = Duration::from_secs(u64::from(last_s));
        let spawned = std::thread::Builder::new().name("blaze-sql".to_owned()).spawn(move || {  // alloc: once a query
            let query = sql::Query { duckdb: &duckdb, history: &dir, sql: &sql };
            let answer = sql::run_live_with(&query, window, &stopped).map_err(sql_error).and_then(|batches| ipc(&batches));
            _ = answered.send(Answered { to, answer });
        });
        match spawned {
            Ok(_) => self.querying.push(Querying { to, stop }),
            Err(error) => {
                let what = format!("starting the thread of a live query (`blaze-sql`): {error}");  // alloc: error path
                fail(out, to, ApiError::new(what, Some("blaze debug sql --live".to_owned())));  // alloc: error path
            }
        }
    }

    /// `conn` is gone: its subscriptions end and its queries' DuckDBs are killed.
    pub fn gone(&mut self, conn: Conn) {
        self.live.retain(|it| it.to.conn != conn);
        for querying in self.querying.iter().filter(|it| it.to.conn == conn) {
            _ = querying.stop.try_send(());
        }
    }

    /// How long the loop may wait before its next [`Observe::tick`] is due: until the next
    /// frame while anyone subscribes.
    pub fn wait(&self, now: Duration) -> Option<Duration> {
        (!self.live.is_empty()).then(|| self.next_frame.saturating_sub(now))
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
        }
        while let Ok(Answered { to, answer }) = self.answered.try_recv() {
            self.querying.retain(|it| it.to != to);
            match answer {
                Ok((ipc, rows)) => self::answer(out, to, Response::Queried { ipc, rows }),
                Err(error) => fail(out, to, error),
            }
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
                fail(out, it.to, ApiError::new(format!("the live feed: {error}"), Some("live".to_owned())));  // alloc: error path
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

    /// The daemon stops: every subscription ends, every query is killed and the explorer
    /// closes its sessions.
    pub fn stop(&mut self, out: &mut Vec<Reply>) {
        for it in self.live.drain(..) {
            out.push(Reply { to: it.to, response: Response::Done { ok: true } });
        }
        for querying in &self.querying {
            _ = querying.stop.try_send(());
        }
        self.explorer = None;
    }
}

const NO_PROFILING: &str = "this daemon keeps no profile history (only `blaze serve`'s does)";

fn answer(out: &mut Vec<Reply>, to: RequestRef, response: Response) {
    out.push(Reply { to, response });
    out.push(Reply { to, response: Response::Done { ok: true } });
}

fn fail(out: &mut Vec<Reply>, to: RequestRef, error: ApiError) {
    out.push(Reply { to, response: Response::Error(error) });
    out.push(Reply { to, response: Response::Done { ok: false } });
}

/// A query's error as the API says it, its kind ([`sql::SqlError::kind`]) where it failed.
fn sql_error(error: sql::SqlError) -> ApiError {
    ApiError::new(error.to_string(), Some(error.kind().to_owned()))  // alloc: error path
}

/// A query's batches as one Arrow IPC stream, and their rows; no bytes for no batches.
fn ipc(batches: &[arrow_array::RecordBatch]) -> Result<(Vec<u8>, u64), ApiError> {
    let rows = batches.iter().map(|it| it.num_rows() as u64).sum();
    // alloc: a query's answer, once a query
    let mut out = Vec::new();
    if let Some(first) = batches.first() {
        blaze_explore::frame::ipc(&mut out, &first.schema(), batches)
            .map_err(|error| ApiError::new(format!("encoding a query's answer as Arrow IPC: {error}"), Some("sql".to_owned())))?;  // alloc: error path
    }
    Ok((out, rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duckdb_unset_is_none_and_set_wrong_is_an_error_naming_it() {
        assert_eq!(duckdb(None), Ok(None));
        let relative = duckdb(Some("bin/duckdb".into())).unwrap_err();
        assert!(relative.contains("BLAZE_DUCKDB=bin/duckdb") && relative.contains("absolute"), "{relative}");
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
        assert_eq!(commit_of(&repo.join("crates/a")).as_deref(), Some("aaaa"), "found from a directory inside");
        fs::write(git.join("HEAD"), "ref: refs/heads/packed\n").unwrap();
        fs::write(git.join("packed-refs"), "# pack-refs with: peeled\nbbbb refs/heads/packed\n").unwrap();
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
        assert_eq!(commit_of(&worktree).as_deref(), Some("aaaa"), "a worktree's branch is the common directory's");
    }

    fn to(conn: u64, id: u64) -> RequestRef {
        RequestRef { conn: Conn(conn), id }
    }

    #[test]
    fn a_daemon_with_no_profiling_refuses_the_explorer_and_live_sql_by_name() {
        let mut observe = Observe::new(None);
        let mut out = Vec::new();
        observe.explore(to(1, 1), false, &mut out);
        observe.query(to(1, 2), "SELECT 1".to_owned(), 30, &mut out);
        let refused: Vec<_> = out
            .iter()
            .filter_map(|it| match &it.response {
                Response::Error(error) => Some(error.at.as_deref()),
                _ => None,
            })
            .collect();
        assert_eq!(refused, [Some("blaze explore"), Some("blaze debug sql --live")]);
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
