//! The daemon's loop: one driver, and every state machine of the daemon stepped by it.
//!
//! - A completion goes to what asked for it, by its tag: the server, the VFS, the database, the
//!   store, the checkouts' watchers and their flushes and the watches outside views, the jobs (their lock writes, modes and user
//!   code), the image, or the loop's own (the start).
//! - What arrives goes through [`Heads`] (a request on a checkout waits for its watcher) to the
//!   session; what the session answers goes to the server. The VFS is flushed after every
//!   session call, so what a call committed is on its way to durability when it returns.
//! - The plane is stepped after every completion; what it did goes to the jobs, and what the
//!   jobs say to the session.
//! - Between requests the database is collected, and the VFS once it made as many nodes as
//!   its last collection kept, keeping each view's plan and every mount; after a cold build the
//!   daemon image ([`Images`]: the VFS and the plane's history) is written.
//! - A start restores no engine state (docs/v2/plan/design/end-state.md: the in-memory engine is
//!   a cache of what this process verified): it opens the store, maps the image for the VFS and
//!   the plane's history, replays the journal's tail with its marks (`super::local`), and the
//!   database starts empty; the first request is answered from the certificate.
//! - A stop writes the image, waits for it to be mapped, then stops listening and hangs up.
//! - The explorer, the live feed and live SQL are answered by the loop itself ([`Observe`]);
//!   each build that ends goes to the profile history's thread. A build whose tasks run while
//!   no pool job ends, no query completes, no task or unit's step ends and no byte arrives on a
//!   download for the stall bound ([`Options::stall`], [`Watchdog::BOUND`] unless `blaze serve
//!   --stall-bound` names another) fails with the stall, whose trace is written to the state's `stalls/`, and the daemon
//!   stops: its workers may be deadlocked, and cannot be got back.

#![forbid(unsafe_code)]

use std::{fmt, net::SocketAddr, sync::Arc, time::Duration};

use blaze_api::{
    ApiError, Arrived, Reply, Request, RequestRef, Response,
    server::{Abandoned, Config as ServerConfig, Http, Server, Unix},
    session::{self, Session},
};
use blaze_cas::Store;
use blaze_core::{Address, Completion, Done, Driver, Effect, FileName, IoError, Mapping, Tag};
use blaze_format::SchemaHash;
use blaze_graph::{PlaneConfig, arena};
use blaze_history::{Config as HistoryConfig, History, section::HistoryError};
use blaze_ids::ContentId;
use blaze_image::{Corrupt, Image, OpenError, Refused};
use blaze_profile::{Plot, Watchdog, watchdog::Progress};
use blaze_query::Database;
use blaze_vfs::{DisksUnused, ImageUse, Outside, Ticket, Vfs};

use super::{
    heads::{Heads, Out},
    image::{ImageFailed, Images, Parts, Unlearned, on_pool},
    job::{Jobs, Said, Setup, Step, Value, WaitedOn, World, doing},
    mounts::Mounts,
    observe::{Observe, Profiling},
    plane::{Happened, Plane, Runner, ns},
    pulse::{Pulse, StuckSaid},
    still::{Facts, STILL_FOR, Stillness, Turn, Why},
    work::Steps,
};

/// The most stuck tasks the loop keeps for the builds' live models ([`Daemon::take_stuck`]):
/// the first lines of a convoy name it; the rest are in the daemon's log and its recording.
const STUCK_SHOWN: usize = 16;

/// The tags of each part: `2^32` each, the watchers' and the runs' ranges of `2^32` after.
const OWN: u64 = 0;
const SERVER: u64 = 1 << 32;
const VFS: u64 = 2 << 32;
const DB: u64 = 3 << 32;
const STORE: u64 = 4 << 32;
const FLUSHES: u64 = 5 << 32;
const JOBS: u64 = 6 << 32;
const JOBS_END: u64 = 7 << 32;
/// The watches of directories outside views ([`blaze_vfs::Outside`]).
const OUTSIDE: u64 = 7 << 32;
const OUTSIDE_END: u64 = OUTSIDE + blaze_vfs::outside::TAGS;
/// The shared standard library's store ([`Options::std`]).
const STD_STORE: u64 = 8 << 32;
const STD_STORE_END: u64 = 9 << 32;
/// The loads of the store's buffer pool (`blaze_cas::Config::load_tags`, made in
/// [`super::direct::pool`]), and the shared standard library's store's: the pool stays for the
/// stores' own commits, opens and collections and the reads not yet on `direct`. A task's
/// direct reads ask with `direct::HOME_TAGS` and up, past `RUNS_END`.
const LOADS: u64 = 9 << 32;
const STD_LOADS: u64 = 10 << 32;
const STD_LOADS_END: u64 = 11 << 32;
/// Lane packs-1112: the tags of the read-only endpoint peers read the store through
/// ([`super::packs::endpoint`]).
const PEERS: u64 = 11 << 32;
const PEERS_END: u64 = 12 << 32;
/// The start files placed beside launched programs (`launch::Daemon::start_placed`).
const PLACES: u64 = 12 << 32;
const PLACES_END: u64 = 13 << 32;
/// The take of the solo answers' kept rows (`blaze_certify::kept::Take`, [`Daemon::look_for_solo_rows`]).
const SOLO_ROWS: u64 = 13 << 32;
const SOLO_ROWS_END: u64 = SOLO_ROWS + blaze_certify::kept::TAGS;
/// How often a daemon with nothing to do looks for solo answers' kept rows, on the driver's
/// clock: a look is a stat of one directory, so once a second costs nothing, and a solo
/// answer's rows reach the history within a second of the daemon's next idle stretch.
const SOLO_LOOK_EVERY: Duration = Duration::from_secs(1);

#[cfg(test)]
thread_local! {
    /// The planted bug of the solo rows' take's control ([`Daemon::solo_taken`]).
    pub(super) static LEAVES_TAKEN_SOLO_ROWS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
const WATCHERS: u64 = 1 << 40;
const RUNS: u64 = 1 << 50;
const RUNS_END: u64 = 1 << 62;

const STAT_JOURNAL: Tag = Tag(OWN);
const READ_JOURNAL: Tag = Tag(OWN + 1);
const STAT_IMAGE: Tag = Tag(OWN + 2);
const MAP_IMAGE: Tag = Tag(OWN + 3);
/// The timer of a build nothing can move, at [`STILL_FOR`] ([`Daemon::still`]): it wakes the
/// loop to fail the build, and moves nothing itself.
const STILL_TIMER: Tag = Tag(OWN + 4);
const DIAL: Tag = Tag(OWN + 6);
const UNLINK: Tag = Tag(OWN + 7);
const HANG_UP: Tag = Tag(OWN + 8);
/// The machine's sealed layers' directory, stated and made before the home store opens
/// ([`LayerDir`]).
const LAYER_DIR: Tag = Tag(OWN + 13);
/// The longest one step of a loop turn may run in line before it is said ([`Stalled`]).
const STEP_BOUND_NS: i64 = 20_000_000;

/// The loop's own watchdog: each step of a turn after its wait is timed on the profile's
/// clock, and one that ran longer than [`STEP_BOUND_NS`] is said as a span named `loop
/// stalled` whose `function` is the step and how long it took. The loop thread serves every
/// request, so 20 ms of one step is 20 ms every client waits; the budgets harness fails an
/// edit whose build has such a row (harness/v2/budgets.py). The span is opened once the step
/// has ended, so its own length is nothing: the time is in its `function`. The turn's wait
/// (the driver's poll) is not a step: what a driver does in line there is held by
/// harness/v2/loop_syscalls.py, which times the thread's system calls.
struct Stalled {
    step: &'static str,
    began: i64,
}

impl Stalled {
    fn begin(step: &'static str) -> Stalled {
        Stalled { step, began: blaze_profile::clock::mono_ns() }
    }

    /// The step under way ended and `step` began.
    fn next(&mut self, step: &'static str) {
        let now = blaze_profile::clock::mono_ns();
        self.say(now);
        *self = Stalled { step, began: now };
    }

    fn end(self) {
        self.say(blaze_profile::clock::mono_ns());
    }

    fn say(&self, now: i64) {
        let Some(ms) = stalled_ms(self.began, now) else { return };
        // alloc: once a step past its bound, the span's `function`
        let function = format!("{}: {ms} ms", self.step);
        // `held`: a held span is never folded. This one is opened once the step has ended, so
        // it is shorter than any span the recorder writes a row for (`flight::FOLD_BELOW`), and
        // folded it would be a count in `folded` with no `function`: the step's name gone, and
        // no row of `spans` for the budgets harness to find.
        tracing::info_span!(target: "blaze", "loop stalled", function = %function, held = true)
            .in_scope(|| {});
        // And a problem (`blaze explain problems`, `blaze slow`, `blaze why`): a step of the
        // loop that ran this long held every client's answer that long.
        tracing::warn!(
            target: "blaze",
            step = self.step,
            ms,
            "loop stalled: a step of the daemon's loop ran in line past {} ms",
            STEP_BOUND_NS / 1_000_000
        );
    }
}

/// How many milliseconds a step from `began` to `now` ran, where that is past
/// [`STEP_BOUND_NS`].
fn stalled_ms(began: i64, now: i64) -> Option<i64> {
    let ran = now.saturating_sub(began);
    (ran > STEP_BOUND_NS).then_some(ran / 1_000_000)
}

/// A stall's trace, written once: the daemon stops after it.
const STALL: Tag = Tag(OWN + 9);
/// The image's placing (its clone, writes, sync and rename, one in flight at a time) and its
/// map ([`Images`]).
const IMAGE: u64 = OWN + 10;
/// The watch of the system's memory pressure ([`Effect::Pressure`], [`Daemon::pressure`]),
/// after the image's two.
const PRESSURE: Tag = Tag(OWN + 12);

/// How long a stop waits for the driver's last effects once it hung up: past it, the
/// connections still sending are hung up on ([`Server::abandon`]) and named in a warning.
pub(super) const LAST_EFFECTS: Duration = Duration::from_secs(2);

/// How long a stop waits for what answered builds left to settle (a kept front end being
/// stored, puts the store has not made durable) before it goes on without it, saying each as
/// a loss (bounded-progress). A build's answer does not wait for these; a stop does, so a
/// `blaze stop` after an answer loses none of it.
pub(super) const STOP_SETTLES_WITHIN: Duration = Duration::from_secs(30);

/// How many quiet turns after the last request was read the work between requests waits for
/// (the collections, the settle's image, whose making is a pass over the whole database on
/// the loop's thread: 65 to 137 ms on a small program, 2026-11-10). A quiet turn took no
/// completion with no plane task running and no job under way ([`quiet_turns_after`]): the
/// loop's idle poll, 50 ms each, so about half a second after the last answer. Counted in turns, not on the clock, so it is the same under the simulation, whose
/// clock stands still while nothing is in flight. Requests come in runs (an edit, a build, a
/// build again): a request read while that work ran waited behind it, 130 ms for an answer
/// that takes 0.1 ms. The work is for the next start and for memory, never for a request, so
/// it runs once the daemon has been quiet this long, and not at all while a request is read
/// and held for its checkout's watcher.
pub(super) const QUIET_BEFORE_BETWEEN: u32 = 10;

/// [`Daemon::quiet_turns`] after a turn, from the count `before` it: none while a job is under
/// way (`under_way`), one more for a turn that took no completion (`took`) with no plane task
/// running (`running`), else as it was.
///
/// A job under way is not quiet, whatever its turns take: a build whose outputs were being
/// placed turned with no completion and no plane task for its whole wait, so the count was
/// past [`QUIET_BEFORE_BETWEEN`] the turn it was answered, and the image was made in the
/// turns right after the answer (`image make` 91 ms on the loop), which is when the client
/// that got the answer asks its next thing: its `why` waited 92 ms behind it (2026-11-11).
/// Counted from the last answer, the image is made half a second after it.
pub(super) const fn quiet_turns_after(
    before: u32,
    under_way: bool,
    took: bool,
    running: bool,
) -> u32 {
    match (under_way, took || running) {
        (true, _) => 0,
        (false, false) => before.saturating_add(1),
        (false, true) => before,
    }
}
/// How often the plane's and the memory's plots are written to the flight recorder, at most.
const PLOT_EVERY: Duration = Duration::from_millis(10);
/// How often the memory by tag goes to the log while the daemon works, at most.
const MEMORY_LOG_EVERY: Duration = Duration::from_secs(30);
/// How often, at most, the system's memory pressure asks the purger for a purge
/// (`crate::raw::memory::Purger::ask`): a forced collect of each thread's heaps costs it a
/// walk of their pages, so not at every word of pressure.
const PURGE_EVERY: Duration = Duration::from_secs(1);
/// How long one unit's step runs, while no other ends, before what every thread is inside is
/// written to the log ([`Daemon::watch`]): a warning, never a failure.
const LONG_STEP: Duration = Duration::from_secs(300);
/// The most nodes a [`Still`] names one by one.
const STILL_NAMED: usize = 64;

/// Whether `effect` ends on its own with a completion of its own: not a watch or a listen,
/// in flight until undone, nor the server's (a client's receive waits on the client), nor one
/// that completes nothing of its own.
fn ends_alone(effect: &Effect) -> bool {
    effect.tag() != STILL_TIMER
        && !(SERVER..VFS).contains(&effect.tag().0)
        && !matches!(
            effect,
            Effect::Watch { .. }
                | Effect::Journal { .. }
                | Effect::Pressure { .. }
                | Effect::Unwatch { .. }
                | Effect::Listen { .. }
                | Effect::Unlisten { .. }
                | Effect::Kill { .. }
                | Effect::Hangup { .. }
        )
}

/// A build nothing can move ([`Daemon::still`]): each node of an open request not settled.
pub struct Still {
    pub quiet_ns: u64,
    /// Why nothing can move it: no thread task runs, the pool lost tasks' jobs, or the pool's
    /// parked jobs wait on what nothing will end ([`Stillness`]).
    pub why: Why,
    /// Loop tasks started and not ended: with nothing in flight, each waits on the loop.
    pub loops: usize,
    /// The worker pool's jobs parked on a wait.
    pub parked: usize,
    /// Every job that waits in the wait graph and what on: a parked job's own spans are on no
    /// thread while it waits (rule 15).
    pub graph: blaze_graph::sched::Waits,
    pub waits: Vec<StillWait>,
    /// Lowerings the loop holds until their root's kept front end is stored, which have no
    /// node of their own ([`Jobs::held_lowerings`]).
    pub lowerings: Vec<super::job::HeldLowering>,
    /// Each job's write that waits, longest first ([`Jobs::write_waits`]): what it waits with
    /// and the tasks the loop holds for it.
    pub writes: Vec<super::work::WriteWaiting>,
    /// The proc macros' expansion threads, `None` when their lock was held as the loop looked.
    pub expansion: Option<blaze_macro_eval::ExpansionCounts>,
}

pub struct StillWait {
    pub request: u32,
    pub node: u32,
    pub name: String,
    pub state: blaze_protocol::events::NodeState,
    /// A dep it waits for that has not settled.
    pub on: Option<u32>,
    /// What the loop holds it back for ([`Jobs::holding`]).
    pub held: Option<&'static str>,
    /// What a loop task that runs and is held for nothing waits on ([`Jobs::run_wait`]).
    pub run: Option<String>,
}

impl fmt::Display for Still {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "blaze stalled: for {:.1} s nothing could move the build (no thread task ran or had ended untaken, no effect was pending or in flight, the worker pool was idle with {} jobs parked; {} loop tasks open), with {} nodes not settled:",
            self.quiet_ns as f64 / 1e9,
            self.parked,
            self.loops,
            self.waits.len()
        )?;
        match self.why {
            Why::NoTask => {}
            Why::Lost { tasks } => write!(
                f,
                "\n  {tasks} tasks posted to the worker pool have no job there: the pool is idle with no job parked, so \
                 their jobs were lost and their `done` will never run; each is a node below that runs"
            )?,
            Why::Stuck { tasks, parked } => write!(
                f,
                "\n  {tasks} tasks posted to the worker pool wait on {parked} parked jobs, and no wait of the wait graph \
                 can end with the loop still; they were failed with the stall"
            )?,
            Why::WriteWaits { tasks, parked, waited_ns } => write!(
                f,
                "\n  a job's write of its front end waited {:.1} s for readers that nothing moves ({tasks} tasks \
                 posted to the worker pool, {parked} jobs parked), while the loop holds back every task of the job \
                 a reader could be waiting on (rule: a write waits only on readers that end on their own)",
                waited_ns as f64 / 1e9
            )?,
        }
        for it in self.writes.iter().take(STILL_NAMED) {
            let wait = &it.wait;
            write!(
                f,
                "\n  the write of `{}`: waited {} ms with {} build-script runs, {} proc-macro loads and {} MSL builds \
                 asked, {} readers alive, {} tasks held",
                it.root,
                it.waited_ms,
                wait.runs,
                wait.macros,
                wait.msls,
                wait.readers,
                it.held
            )?;
            if let Some(node) = it.applying {
                write!(f, ", applying on node {node}")?;
            }
        }
        if let Some(it) = &self.expansion {
            write!(
                f,
                "\n  expansion threads: {} alive of a cap of {}, {} idle, {} expansions queued, {} waiting on \
                 `macro.thread.replied`",
                it.alive, it.cap, it.idle, it.queued, it.asking
            )?;
        }
        if !self.graph.0.is_empty() {
            write!(f, "\n  {}", self.graph)?;
        }
        // The ready nodes: no dependency holds one, so each waits for admission to start it.
        let ready = self
            .waits
            .iter()
            .filter(|it| matches!(it.state, blaze_protocol::events::NodeState::Ready))
            .count();
        if ready > 0 {
            write!(f, "\n  {ready} nodes are ready and admission started none of them")?;
        }
        for it in self.waits.iter().take(STILL_NAMED) {
            write!(f, "\n  request {} node {} `{}`: {:?}", it.request, it.node, it.name, it.state)?;
            if let Some(on) = it.on {
                write!(f, ", waits on node {on}")?;
            }
            if let Some(held) = it.held {
                write!(f, ", held: {held}")?;
            }
            if let Some(run) = &it.run {
                write!(f, ", {run}")?;
            }
        }
        if self.waits.len() > STILL_NAMED {
            write!(f, "\n  and {} more", self.waits.len() - STILL_NAMED)?;
        }
        for it in self.lowerings.iter().take(STILL_NAMED) {
            write!(f, "\n  {it}")?;
        }
        if self.lowerings.len() > STILL_NAMED {
            write!(f, "\n  and {} more lowerings held", self.lowerings.len() - STILL_NAMED)?;
        }
        Ok(())
    }
}

/// The plane's thread tasks that run (a pool job's or an IO thread's), each by its node's name
/// and unit, as a stall names what the loop waits on: the loop's wait for a task is a wait for
/// its job's `done`, which opens no span of its own (rule 15).
struct RunningTasks<'a, W: super::plane::Work, P: blaze_graph::admission::Predict>(&'a Plane<W, P>);

impl<W: super::plane::Work, P: blaze_graph::admission::Predict> fmt::Display
    for RunningTasks<'_, W, P>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let plane = self.0;
        write!(
            f,
            "the loop waits on {} thread tasks ({} on the worker pool)",
            plane.running(),
            plane.running_on_pool()
        )?;
        let (mut named, mut more, mut out) = (0, 0, Ok(()));
        plane.unsettled(|it| {
            if !matches!(it.state, blaze_protocol::events::NodeState::Running)
                || plane.loop_runs(it.id)
            {
                return;
            }
            if named == STILL_NAMED {
                more += 1;
                return;
            }
            out = out.and_then(|()| {
                f.write_str(if named == 0 { ": " } else { ", " })?;
                write!(f, "request {} node {} `{}`", it.request.0, it.id.0, it.name)
            });
            named += 1;
        });
        out?;
        if more > 0 {
            write!(f, " and {more} more")?;
        }
        Ok(())
    }
}

/// Every download in flight, as a stall names them: `; downloads in flight: <what> from
/// <url>: <line>, …`, each with its bytes and how long since its last; nothing with none.
struct DownloadsNow<'a>(&'a [blaze_fetch::downloads::Shown]);

impl fmt::Display for DownloadsNow<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (at, it) in self.0.iter().enumerate() {
            f.write_str(if at == 0 { "; downloads in flight: " } else { ", " })?;
            write!(f, "{} from {}: {}", it.what, it.url, it.line)?;
        }
        Ok(())
    }
}

/// The builds clients wait on, as a stall names them: how many jobs of requests the session
/// has not seen finish, and each job of the build with its stage and what it waits on there.
/// A request the session waits on with no job of the build left for it is a lost job, said
/// so (rule 15: the client's wait names what it waits on).
struct BuildsWaited<'a, S: Steps>(usize, WaitedOn<'a, S>);

impl<S: Steps> fmt::Display for BuildsWaited<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "clients wait on {} jobs of requests; the build has {}", self.0, self.1)
    }
}

/// The session's view of the daemon for one call, borrowing only the parts it names.
macro_rules! cx {
    ($daemon:ident) => {
        session::Daemon {
            vfs: &mut $daemon.vfs,
            db: &$daemon.db,
            builder: &mut $daemon.jobs,
            pid: $daemon.pid,
            build: &$daemon.build,
            settings: &$daemon.settings,
            memory_bytes: arena::own(),
            // No budget: admission is by cores alone and the kernel's limit is the ceiling.
            budget_bytes: 0,
            // The driver's clock starts as the daemon does.
            uptime_ms: u64::try_from($daemon.driver.now().as_millis()).unwrap_or(u64::MAX),
            placed_by: $daemon.store.place_by().map(|it| it.said()),
            memory: blaze_mem::tag::snapshot,
            profile: blaze_profile::flight::dump,
            hot: crate::serve::observe::hot,
            history: $daemon.observe.history_error(),
            bugs: crate::serve::observe::bugs,
            builds: &$daemon.builds,
            downloads: crate::serve::job::doing::downloads,
        }
    };
}

/// The jobs' view of the daemon for one call; the jobs themselves are left out of it.
macro_rules! world {
    ($daemon:ident, $said:ident) => {
        World {
            now: $daemon.driver.now(),
            wall_ns: $daemon.driver.wall_ns(),
            vfs: &mut $daemon.vfs,
            db: &$daemon.db,
            store: &mut $daemon.store,
            macros: &$daemon.macros,
            std: &mut $daemon.std,
            plane: &mut $daemon.plane,
            mounts: &mut $daemon.mounts,
            heads: &$daemon.heads,
            images: &mut $daemon.images,
            steps: &$daemon.steps,
            effects: &mut $daemon.effects,
            said: &mut $said,
            reads: super::direct::Lender::of(&mut $daemon.driver, &mut $daemon.done),
        }
    };
}

/// What the daemon starts with. No `Debug`: it holds the HTTP token.
/// The files every request slot's premises are of (`blaze_certify::slot::Premise`), by absolute
/// path: the engine's own executable, and the standard library's `DIGEST` where blaze owns the
/// library (`toolchain::library::owned`; `None`: no slot is written, `Unslotted::NoLibrary`).
#[derive(Clone, PartialEq, Eq, Debug)] // clone: the jobs' own copy, once a start
pub struct Premises {
    pub engine: String,
    pub library: Option<String>,
}

impl Premises {
    /// This process's: its executable and the library it found, each canonical, as the thin
    /// client finds them (`Engine::beside`: no link in the path, so the slot's `stat` stamp is
    /// the client's `lstat` one, handoff-10C C4).
    pub fn of_this_process() -> Result<Premises, String> {
        let engine = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(|error| format!("the engine's own executable: {error}"))?;
        // alloc: once a start, the engine's path
        let engine = engine.to_str().ok_or_else(|| format!("the engine's path {} is not UTF-8", engine.display()))?.to_owned();
        let library = crate::toolchain::library::found()
            .ok()
            .and_then(crate::toolchain::library::owned)
            .and_then(|it| std::fs::canonicalize(it).ok())
            .and_then(|it| it.to_str().map(str::to_owned)); // alloc: once a start, the library's path
        Ok(Premises { engine, library })
    }
}

pub struct Options {
    /// Its state directory ([`super::state`]), under the driver's root.
    pub state: FileName,
    /// The directory the daemons of every blaze with this one's record makers share the
    /// standard library in, whatever their `BLAZE_HOME` (`<machine>/std/<makers>`,
    /// [`super::machine`] and [`super::RecordMakers::id`]), under the driver's root: each std key's outputs in a directory of
    /// its own, their records and outputs in the shared store `store` beside them.
    pub std: FileName,
    /// Where workspaces' build directories are when nothing names one ([`super::builds`]),
    /// under the driver's root: each workspace's in a directory of its root's id, never in the
    /// checkout ([`blaze_units::query::BuildConfig::builds`]).
    pub builds: FileName,
    /// Where the packages it fetches are kept, mounted and on disk ([`super::sources`]), under
    /// the driver's root ([`blaze_units::query::BuildConfig::sources`]).
    pub sources: FileName,
    /// The daemon as a writer of that store: one per state directory.
    pub writer: ContentId,
    /// How its tasks read its stores ([`super::direct::Reads`]): each worker on a native driver
    /// of its own over the root of the driver the daemon runs on (`blaze serve`); or, where the
    /// daemon's driver is the simulation and its files are in no file system, on the loop's
    /// own driver, lent to the tasks it runs in line.
    pub reads: super::direct::Reads,
    /// The socket it listens on; `None` takes requests from this process alone
    /// ([`Daemon::connect_local`]): the seed's one-shot build ([`super::once`]).
    pub socket: Option<FileName>,
    /// Where HTTP is listened on, and the bearer token its clients send.
    pub http: Option<(SocketAddr, String)>,
    /// Workers of the plane: the cores.
    pub threads: usize,
    /// IO tasks run at once (fetches).
    pub io: usize,
    /// The daemon's `PATH` and `HOME`, an agent's invocation's whole environment.
    pub env: Vec<(String, String)>,
    pub host: String,
    /// The toolchain whose proc-macro ABI and library the host's proc macros are built with:
    /// what the daemon's kept expansions are keyed by ([`proc_macro_api::kept::Expansions`]).
    pub toolchain: ContentId,
    pub color: bool,
    /// Which blaze the daemon is: its executable's hash and that file as it started
    /// ([`super::build_of`]), which a client compares with its own before it sends a request.
    pub build: blaze_api::Build,
    /// What it was started with and keeps for its life ([`super::settings`]), which a client
    /// compares with its own environment before it sends a request.
    pub settings: Vec<blaze_api::Setting>,
    /// The table of directories outside views that user code's checks read
    /// ([`blaze_vfs::outside::dirs`]); the daemon drives its watches for its whole life.
    pub outside: &'static blaze_vfs::outside::Dirs,
    /// How long no span may close, no query complete and no task or unit's step end while the
    /// plane's tasks run before the build fails with the stall ([`Daemon::watch`]):
    /// [`Watchdog::BOUND`] unless `blaze serve --stall-bound SECS` says.
    pub stall: Duration,
    /// The socket systemd or launchd passed (`blaze serve --activated`), listened on in place
    /// of one made at `socket`, where it must be bound. Its file is the manager's.
    #[cfg(unix)]
    pub activated: Option<blaze_api::activation::Passed>,
    /// The profile history and its DuckDB ([`super::observe::keep_history`]): `blaze serve`'s.
    /// A daemon with none (the simulation's) numbers its builds from 1 and refuses the
    /// explorer and live SQL, saying so.
    pub profiling: Option<Profiling>,
    /// The parts of a good image this start leaves unused because it was asked to
    /// ([`RESTORE_WITHOUT`]): each is said in the start's log as every other part not used.
    pub without: Without,
    /// The premises of every request slot this daemon writes ([`Premises`]).
    pub premises: Premises,
    /// Its grace when it exits once no connection holds it ([`super::unheld`],
    /// `BLAZE_DAEMON_EXIT_UNHELD`): it stops itself once no client or launcher connection was
    /// open for this long. `None` exits only as [`super::idle::exit_due`] says, or when told.
    pub unheld: Option<Duration>,
}

/// The variable that names parts of the daemon's image a start leaves unused though they
/// are whole: `history`, the one part a start reads besides the VFS, separated by commas. For finding which part of
/// a restored state a fault follows (docs/v2/plan/design/restore.md, "Which part a fault
/// follows"): the image stays on disk as it is, and each part left is said in the start's log.
pub const RESTORE_WITHOUT: &str = "BLAZE_RESTORE_WITHOUT";

/// The parts of a good image a start was asked to leave unused ([`RESTORE_WITHOUT`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Without {
    /// The plane's history: it starts empty ([`Unlearned::Asked`]).
    pub history: bool,
}

impl Without {
    /// The parts `text` names, separated by commas; a word that names none is the error,
    /// saying the word and the ones there are.
    pub fn parse(text: &str) -> Result<Without, String> {
        let mut without = Without::default();
        for word in text.split(',').map(str::trim).filter(|it| !it.is_empty()) {
            match word {
                "history" => without.history = true,
                // alloc: an error's text, once a start
                "database" | "trees" | "carries" => {
                    return Err(blaze_report::say!(
                        blaze_report::say::Words(RESTORE_WITHOUT),
                        " names ",
                        blaze_report::say::Code(word),
                        ", which no start reads any more: a start restores no engine state, so the only part it can leave unused is `history`"
                    )
                    .flat());
                }
                // alloc: an error's text, once a start
                _ => {
                    return Err(blaze_report::say!(
                        blaze_report::say::Words(RESTORE_WITHOUT),
                        " names ",
                        blaze_report::say::Code(word),
                        ", which is no part of the daemon image a start can leave unused: the one part is `history`"
                    )
                    .flat());
                }
            }
        }
        Ok(without)
    }
}

fn under(dir: &FileName, name: &str) -> FileName {
    FileName::new(&format!("{dir}/{name}")).expect("a name under a directory is a name")
}

/// The machine's sealed layers' directory (`<std>/store`, every writer of the shared std
/// store), made with each directory above it that is not there before the home store, which
/// reads it as its layer, opens: the store's refusal of a layer it cannot list is then only
/// ever a real fault, never a first daemon whose std store had not made its directory yet.
/// Sans-IO: each step is one stat or one make, of [`LAYER_DIR`]; a make that fails because
/// another (this daemon's std store, another daemon) made the directory meanwhile is stated
/// once more, and is the error only if it is still not there.
struct LayerDir {
    /// The directory and every directory above it, outermost first.
    dirs: Vec<FileName>,
    /// The one stated or made now; `None` while the directory itself is stated first.
    at: Option<usize>,
    /// The make of `dirs[at]` failed, with this, and it is being stated again: the error, if
    /// it is still not there, says why the make failed.
    again: Option<String>,
}

/// What [`LayerDir`] asks next.
enum LayerStep {
    Ask(Effect),
    /// The directory is there: the home store opens.
    There,
    Failed(String),
}

impl LayerDir {
    /// The walk for `dir`, and its first effect: a stat of `dir` itself, which is all a
    /// machine that has it costs.
    fn new(dir: &FileName) -> (LayerDir, Effect) {
        let text = dir.as_str();
        // alloc: once per daemon started, a name a directory above the layer
        let dirs = text
            .match_indices('/')
            .map(|(at, _)| &text[..at])
            .chain([text])
            .filter(|it| !it.is_empty())
            .filter_map(FileName::new)
            .collect();
        // clone: the effect owns the name
        (LayerDir { dirs, at: None, again: None }, Effect::Stat { file: dir.clone(), tag: LAYER_DIR })
    }

    fn stat(&self, at: usize) -> LayerStep {
        // clone: the effect owns the name
        LayerStep::Ask(Effect::Stat { file: self.dirs[at].clone(), tag: LAYER_DIR })
    }

    /// The next of `at`, or [`LayerStep::There`] past the last.
    fn next(&mut self, at: usize) -> LayerStep {
        self.again = None;
        match at + 1 < self.dirs.len() {
            true => {
                self.at = Some(at + 1);
                self.stat(at + 1)
            }
            false => LayerStep::There,
        }
    }

    fn done(&mut self, done: Done) -> LayerStep {
        let last = self.dirs.len().saturating_sub(1);
        let named = |dirs: &[FileName], at: usize| dirs.get(at).map_or("", FileName::as_str).to_owned(); // alloc: error path
        match (self.at, done) {
            (_, Done::Stat(Some(meta))) if !meta.dir => {
                let at = self.at.unwrap_or(last);
                LayerStep::Failed(format!("/{} is a file, where the sealed layers' directory goes", named(&self.dirs, at)))
            }
            (None, Done::Stat(Some(_))) => LayerStep::There,
            (None, Done::Stat(None)) if self.dirs.is_empty() => LayerStep::There,
            (None, Done::Stat(None)) => {
                self.at = Some(0);
                self.stat(0)
            }
            (Some(at), Done::Stat(Some(_))) => self.next(at),
            (Some(at), Done::Stat(None)) if self.again.is_some() => LayerStep::Failed(format!(
                "/{}: its make failed ({}) and it is not there",
                named(&self.dirs, at),
                self.again.as_deref().unwrap_or_default()
            )),
            // clone: the effect owns the name
            (Some(at), Done::Stat(None)) => LayerStep::Ask(Effect::MakeDir { dir: self.dirs[at].clone(), tag: LAYER_DIR }),
            (Some(at), Done::MadeDir) => self.next(at),
            (Some(at), Done::Failed(why)) if self.again.is_none() => {
                self.again = Some(why.to_string()); // alloc: error path, once a failed make
                self.stat(at)
            }
            (at, Done::Failed(error)) => {
                LayerStep::Failed(format!("/{}: {error}", named(&self.dirs, at.unwrap_or(last))))
            }
            (_, done) => LayerStep::Failed(format!("the sealed layers' directory was answered {done:?}")),
        }
    }
}

enum Stopping {
    /// The image is being written.
    Image,
    /// Hung up: the driver's last effects end.
    Hangup {
        at: Duration,
    },
    Over,
}

/// How often the loop turns while thread tasks run whose ends wake it ([`Woken::PlaneWakes`]):
/// each turn's plane step reads what the running tasks hold (`Plane::measure`), which memory
/// admission decides on, so one long task is still read this often. No task's end waits for it.
const MEASURES_EVERY: Duration = Duration::from_millis(10);

/// What ends the loop's wait for its driver between turns, besides the driver's own completions
/// (a client's connection or request, a file, a child, a watch): the timer the loop set, and
/// why. Named by the wait's span, so the loop's time between turns says what it waited on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Woken {
    /// Thread tasks run on a driver with no waker ([`blaze_core::Driver::waker`]): their ends
    /// come back on the plane's channel, which the loop reads each turn, so it waits at most a
    /// millisecond.
    PlaneTasks,
    /// Thread tasks run, and each one's end ends the driver's wait (`plane::Back`): the loop
    /// waits for that or the driver's io, and turns at least every [`MEASURES_EVERY`] to read
    /// what the running tasks hold. Until 2026-11-11
    /// every driver's loop read the ends every millisecond: each step of an edit that the loop
    /// hands on waited up to one, 257 kept units' wind-ups of hello world about 10 ms in all.
    PlaneWakes,
    /// Nothing runs: the idle tick.
    Idle,
    /// An intent's deadline in the session.
    Session,
    /// A job's deadline for the store to make its puts durable.
    Durable,
    /// A live feed's next frame, or the deadline of a `blaze why` waiting on the history.
    Observe,
    /// The end of the grace of a daemon that exits once no connection holds it
    /// ([`super::unheld`]): nothing else may wake it then.
    Unheld,
}

impl Woken {
    /// What the loop's wait waits on, as its span's `awaits` says it.
    fn on(self) -> blaze_graph::wait::WaitOn<'static> {
        use blaze_graph::wait::WaitOn;
        match self {
            Woken::PlaneTasks => WaitOn::Channel(
                "daemon loop: a plane task's end (read every 1 ms) or the driver's io",
            ),
            Woken::PlaneWakes => WaitOn::Channel(
                "daemon loop: a plane task's end, which wakes it, or the driver's io, or its 10 ms look at the running tasks' memory",
            ),
            Woken::Idle => WaitOn::Io(
                "daemon loop: a client's connection or request, a file, a child or a watch, or the 50 ms idle tick",
            ),
            Woken::Session => {
                WaitOn::Io("daemon loop: the driver's io, or an intent's deadline in the session")
            }
            Woken::Durable => WaitOn::Io(
                "daemon loop: the driver's io, or a job's deadline for the store's puts to be durable",
            ),
            Woken::Observe => WaitOn::Io(
                "daemon loop: the driver's io, or a live feed's frame or a `blaze why`'s deadline",
            ),
            Woken::Unheld => WaitOn::Io(
                "daemon loop: a client's connection, or the end of its grace with no connection open (BLAZE_DAEMON_EXIT_UNHELD)",
            ),
        }
    }
}

/// The daemon on driver `D`, building with `S`: `blaze serve`'s is `Daemon<Native, Real>`, the
/// simulation's `Daemon<Sim, Fake>`.
pub struct Daemon<D: Driver, S: Steps> {
    driver: D,
    effects: Vec<Effect>,
    done: Vec<Completion>,
    listening: String,
    server: Server,
    session: Session,
    vfs: Vfs,
    db: Arc<Database>,
    /// Only the loop writes it; workers read it through [`Store::reader`], with no lock.
    store: Store,
    /// Its proc macros, whose expansions are kept in `store`: made once as it starts and lent
    /// to every job ([`JobInput::macros`](super::work::JobInput::macros)).
    macros: Arc<blaze_macro_eval::Macros>,
    /// The standard library's records and outputs, shared with every daemon of this build
    /// ([`Options::std`]); written and read as `store` is.
    std: Store,
    plane: Plane<Step<S>, History>,
    mounts: Mounts,
    heads: Heads,
    images: Images,
    jobs: Jobs<S>,
    steps: Arc<S>,
    replies: Vec<Reply>,
    spare: Spare<S>,
    stopping: Option<Stopping>,
    /// A `blaze stop` arrived: the daemon stops once its answer is sent.
    stop_asked: bool,
    /// The first `blaze stop`'s request: after its Done, each step of the stop is said to it
    /// ([`blaze_api::Response::Stopping`]) until the daemon hangs up, so the CLI waits per step
    /// and, when a step outlasts its patience, names the step.
    stop_from: Option<RequestRef>,
    /// The step last said to [`Daemon::stop_from`].
    stop_said: String,
    /// A request other than `blaze stop` or `blaze status`, or a checkout's head moving, came
    /// since the settle image was asked: one asked now would differ from it. While false, a
    /// stop that comes as that image is made waits for it rather than asks another (the
    /// survey's stop wrote the 679 MiB image twice, 11.2 s then 4.2 s, past the CLI's patience).
    changed_since_settle: bool,
    /// What the stop hung up on when [`LAST_EFFECTS`] passed with replies still being sent.
    abandoned: Option<Abandoned>,
    settling: bool,
    /// A stop waits for what answered builds left to settle (their kept front ends stored,
    /// their puts durable) before its image: since when on the driver's clock, and the span
    /// naming the wait ([`Daemon::stop`], bounded by [`STOP_SETTLES_WITHIN`]).
    stop_settles: Option<(Duration, tracing::Span)>,
    /// Each request read and not yet told it is done, with its `request: read to reply` span
    /// ([`Daemon::arrive`], [`Daemon::reply`]); a connection's go as it ends.
    answering: Vec<(RequestRef, tracing::Span)>,
    /// What is built before it is asked for ([`super::ahead`]): the core, off until
    /// [`Daemon::build_ahead`] turns it on.
    ahead: super::ahead::Ahead,
    /// The build ahead under way, from its request sent to its `Done`: what its line in the
    /// log says, and its span.
    ahead_run: Option<ahead::Run>,
    /// What is kept for launched programs across turns and starts ([`launch`]): start images
    /// waiting to be durable before they are marked, and the runs a first call is built by.
    launch: launch::Launch,
    /// How many turns in a row, since a request was last read and since a job was last under
    /// way, took no completion with no plane task running ([`quiet_turns_after`]): the work
    /// between requests waits for [`QUIET_BEFORE_BETWEEN`] of them ([`Daemon::between`]).
    quiet_turns: u32,
    failed: Option<Failed>,
    pid: u32,
    /// Which blaze it is ([`Options::build`]), for [`blaze_api::Status::build`].
    build: blaze_api::Build,
    /// What it was started with ([`Options::settings`]), for [`blaze_api::Status::settings`].
    settings: Vec<blaze_api::Setting>,
    started: Started,
    /// Its state directory, in whose `stalls/` a stall's trace is written.
    state: FileName,
    observe: Observe,
    /// The stall watchdog ([`Daemon::watch`]): of pool jobs ended, queries completed and tasks
    /// and unit's steps ended, all at once, at [`Options::stall`].
    watchdog: Watchdog,
    /// Of the same, at half that bound: what every thread is inside is written to the log once
    /// a quiet stretch, and nothing fails.
    dump_watch: Watchdog,
    /// Of unit's steps ended alone, at [`LONG_STEP`]: what every thread is inside is written to
    /// the log once a long step, and nothing fails, as other work moves meanwhile.
    step_watch: Watchdog,
    /// Since when nothing could move a build ([`Daemon::still`]), on the driver's clock.
    stillness: Stillness,
    /// The effects in flight that end on their own, by tag ([`ends_alone`]): what can still
    /// move a build, where the driver's count holds every watch, listen and receive too.
    ending: blaze_hash::HashMap<Tag, u32, std::alloc::Global>,
    /// The plane's nodes that settled: with the pool's jobs ended, the progress the watchdog
    /// reads.
    settled: u64,
    /// The jobs' own effects that ended (their placements and the store's answers, their
    /// looks, their runs' IO): progress of the build, never of the loop's own timers.
    job_effects: u64,
    /// The watchdog's looks that found a worker inside a job's code or a run of user code
    /// alive: work under way though nothing ended, as one long compile or test is.
    working: u64,
    /// The loop's progress as another thread reads it ([`Daemon::pulse`]): `completions`,
    /// `settled` and `job_effects`, and the turns it finished. The stop's guard reads it while
    /// the loop may be stuck inside the stop, or inside any turn.
    pulse: Arc<Pulse>,
    /// The top stall's line as the guard last said it ([`Pulse::take_stall`]), the loop's own
    /// copy: taken from the guard's queue once a look at the builds, with no lock.
    stall: Option<String>,
    /// The tasks the task watchdog found stuck (`serve::stuck`, through [`Pulse::take_stuck`])
    /// while a build is under way, the latest [`STUCK_SHOWN`]: each build's live model says
    /// those of its build and those of none (`Now::stuck`). Cleared once no build is.
    stuck: Vec<StuckSaid>,
    /// The effects that end on their own that ended ([`ends_alone`]: writes, reads and runs,
    /// never a client's connect or request, which a stuck daemon still takes).
    completions: u64,
    /// When the plots were last written, on the driver's clock.
    plotted: Duration,
    /// Whether a job built something (a miss) or a save was taken since the start: such a
    /// daemon waits [`super::idle::EXIT_AFTER`] idle before it exits, one that did not
    /// [`super::idle::EXIT_UNWORKED`] ([`super::idle::exit_due`]).
    worked: bool,
    /// When the daemon was first seen with nothing to do since it last worked.
    idle_since: Option<Duration>,
    /// When the memory's breakdown by area was last plotted ([`super::breakdown::EVERY`]), on
    /// the driver's clock, and which tags it wrote so far.
    broken_down: Duration,
    shown: super::breakdown::Shown,
    /// When the builds were last looked at ([`doing::EVERY`]), on the driver's clock.
    looked: Duration,
    /// What that look found of each build a client waits on, for `blaze status`
    /// ([`blaze_api::Status::builds`]): empty once none runs.
    builds: Vec<blaze_api::Building>,
    /// When the memory by tag was last logged, on the driver's clock.
    memory_logged: Duration,
    /// When the system's memory pressure last asked for the heaps' free pages back
    /// ([`PURGE_EVERY`]), on the driver's clock.
    given_back: Duration,
    /// Whether this stretch with nothing to do gave its warm memory back ([`super::idle`]).
    idle: super::idle::Idle,
    /// The countdown of a daemon that exits once no connection holds it ([`super::unheld`],
    /// [`Options::unheld`]): `None` for one that does not.
    unheld: Option<super::unheld::Unheld>,
    /// It stopped because nothing held it for its grace ([`Daemon::ended_unheld`]).
    ended_unheld: bool,
    /// The take of the solo answers' kept rows in `<state>/solo-rows`, one look at a time.
    solo: blaze_certify::kept::Take,
    /// When the last look for them began: `None` until the first turn's, at the start.
    solo_looked: Option<Duration>,
    /// Whether the system's memory pressure is watched ([`PRESSURE`]): from the start until its
    /// watch fails (no source here) or is undone at the stop.
    pressure_watched: bool,
    /// The collector of what blaze keeps on disk ([`super::gc`]): `blaze serve`'s daemon's
    /// alone ([`Daemon::collect`]), as a test's daemons share no disk of their own.
    gc: Option<super::gc::Gc>,
    /// The store's cold moves between requests ([`super::settle`]): `None` for a daemon that
    /// makes none (a one-shot build, most tests).
    settles: Option<super::settle::Settles>,
    /// The store's collector ([`super::collect`]): `None` for a daemon that collects nothing.
    collects: Option<super::collect::Collects>,
    /// Lane packs-1112: the endpoint peers read the store through, when the settings ask
    /// for one, and the pack commands a client asked of this daemon.
    peers: Option<super::packs::endpoint::Endpoint>,
    packings: super::packs::daemon::Packings,
    /// `blaze gc`'s answers from the build dirs' collector, held until the store's lap they
    /// asked for has ended too ([`super::collect::Collects::asked_waits`]).
    gc_answers: Vec<Reply>,
    /// `blaze fetch-url`'s requests, each fetched on a thread of its own.
    fetch_urls: super::fetch_url::FetchUrls,
    /// Builds that failed for want of space, their finish held under the collector's hold id
    /// until the urgent run they asked for ends and the volumes are measured after it
    /// ([`super::gc::Gc::hold`]); then told, their error saying what the run found.
    held: Vec<(u64, Said)>,
}

/// What of its image a start did not use, each by a rule: the image was written by another
/// blaze, or a stop cut the journal. An image that is damaged is a [`StartError`] instead.
/// No engine state is restored, so none is listed: the database always starts empty.
#[derive(Default, Debug)]
pub struct Started {
    /// The image the VFS and the plane's history were read from; `None` with [`Started::unimaged`].
    pub mapped: Option<Mapped>,
    /// Why no image was read: the VFS loaded its journal alone and the history starts empty.
    pub unimaged: Option<Unimaged>,
    /// The image's VFS tree is of another layout: the journal was loaded alone and every
    /// checkout is scanned.
    pub tree: Option<Refused>,
    /// The image's watcher metadata (its disks section) is missing or of another schema: the
    /// watchers scan their checkouts.
    pub disks: Option<Refused>,
    /// A stop came between the image's rename and its journal's rewrite: the whole journal was
    /// loaded over the image.
    pub resumed: bool,
    /// Bytes after the journal's last whole record, which a stop in an append leaves: cut.
    pub journal_cut: usize,
    /// Head records whose root was in those bytes: dropped.
    pub heads_dropped: usize,
    /// Why the plane's history started empty.
    pub unlearned: Option<Unlearned>,
    /// Why the journal's marks of front-end carry sessions, or some of them, were not used
    /// ([`Jobs::restore_carries`]): each such checkout seeds its session on its next build.
    pub carries: Option<String>,
}

impl Started {
    /// Whether the start used everything it found.
    pub fn whole(&self) -> bool {
        self.unimaged.is_none()
            && self.tree.is_none()
            && self.disks.is_none()
            && !self.resumed
            && self.journal_cut == 0
            && self.heads_dropped == 0
            && self.unlearned.is_none()
            && self.carries.is_none()
    }
}

impl fmt::Display for Started {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut sep = "";
        if let Some(unimaged) = &self.unimaged {
            write!(f, "{sep}no image was read ({unimaged}): the VFS loaded its journal alone")?;
            sep = "; ";
        }
        if let Some(refused) = &self.tree {
            write!(
                f,
                "{sep}the VFS's tree was not used ({refused}): the journal was loaded alone"
            )?;
            sep = "; ";
        }
        if let Some(refused) = &self.disks {
            write!(
                f,
                "{sep}the VFS's watcher metadata was not used ({refused}): the checkouts are scanned"
            )?;
            sep = "; ";
        }
        if self.resumed {
            write!(
                f,
                "{sep}a stop came between the image and its journal's rewrite: the whole journal was loaded over it"
            )?;
            sep = "; ";
        }
        if self.journal_cut > 0 || self.heads_dropped > 0 {
            write!(
                f,
                "{sep}the journal ended in {} bytes that were no whole record, and {} heads whose root was in them were dropped",
                self.journal_cut, self.heads_dropped
            )?;
            sep = "; ";
        }
        if let Some(unlearned) = &self.unlearned {
            write!(f, "{sep}the plane's history starts empty: {unlearned}")?;
            sep = "; ";
        }
        if let Some(why) = &self.carries {
            write!(f, "{sep}the front end's carry marks were not all used ({why})")?;
        }
        Ok(())
    }
}

/// The image a start mapped for its VFS and the plane's history, as its log names it.
#[derive(Clone, Debug)]
pub struct Mapped {
    /// Its [`Shape`](super::image::Shape): hash, bytes and sections.
    pub shape: String,
    /// From the start to the image mapped, on the driver's clock.
    pub mapped_in: Duration,
}

impl fmt::Display for Mapped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}, mapped in {}", self.shape, super::image::ms(self.mapped_in))
    }
}

/// The daemon's memory by tag, for the log: the process's resident set and how much of it
/// lies outside both blaze-mem and the global heap (code, stacks), what the global allocator
/// holds live and how much of that was allocated outside any task (so is named by no tag), what
/// it has committed and so keeps freed but not given back, what blaze-mem has committed, how much
/// of that is purged (free chunks whose pages the kernel took: not resident) and reserved, then
/// each tag holding at least [`MemoryLine::SHOWN`] in blaze-mem or in the global heap, or
/// reserving at least [`MemoryLine::SHOWN_RESERVED`], most held first, with its high-water
/// marks, its regions' reservation and its global-heap bytes
/// (`blaze_mem::tag::Counters::heap`). Committed bytes past the tags' sum are held by no tag.
/// Then the size classes of `blaze_mem::blocks` (kept values: trees, token trees, sources):
/// what their blocks hold live, what the classes keep warm and have given back cold, and each
/// class carving at least [`MemoryLine::SHOWN`], most carved first, so the gap between what is
/// live and what the classes hold is named by class. Last, the free chunks: warm (resident, kept
/// for the next frame or arena) and cold (purged). So memory that was freed but is kept (by the
/// allocator, by the chunk cache, by a block class) reads apart from memory something holds
/// (coord-108, B5: "chunks 11.69" was 10 GiB of purged chunks after a burst of frames).
pub struct MemoryLine<'a> {
    pub memory: &'a blaze_mem::tag::Snapshot,
    pub blocks: &'a blaze_mem::blocks::Snapshot,
    /// The process's resident bytes ([`crate::raw::memory::footprint`]), where the system says.
    pub resident: Option<u64>,
    /// What the process holds through the global allocator ([`blaze_graph::arena::held`]); 0
    /// where the counting allocator is not installed, and not shown.
    pub heap: u64,
    /// The chunks no owner holds ([`blaze_mem::chunks::cached`]).
    pub chunks: blaze_mem::chunks::Cached,
    /// The most reads one query run held (and the run, `` `query` of key ``) and the most
    /// pending rows one task held ([`blaze_query::db::stack_high_water`]), what the runaway
    /// bounds are set against; not shown while both counts are 0.
    pub stacks: blaze_query::db::StackHighWater,
}

impl MemoryLine<'_> {
    const SHOWN: u64 = 64 << 20;
    const SHOWN_RESERVED: u64 = 64 << 30;
    /// The size classes shown at most.
    const CLASSES_SHOWN: usize = 8;
}

/// A block size as the memory line says it: `80 B`, `5 KiB`, `1.25 MiB`.
struct BlockSize(u64);

impl fmt::Display for BlockSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (unit, name) = match self.0 {
            size if size >= 1 << 30 => (1u64 << 30, "GiB"),
            size if size >= 1 << 20 => (1 << 20, "MiB"),
            size if size >= 1 << 10 => (1 << 10, "KiB"),
            _ => return write!(f, "{} B", self.0),
        };
        if self.0 % unit == 0 {
            write!(f, "{} {name}", self.0 / unit)
        } else {
            write!(f, "{:.2} {name}", self.0 as f64 / unit as f64)
        }
    }
}

impl fmt::Display for MemoryLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let gib = |bytes: u64| bytes as f64 / f64::from(1u32 << 30);
        let memory = self.memory;
        f.write_str("memory: ")?;
        if let Some(resident) = self.resident {
            write!(
                f,
                "resident {:.2} GiB, {:.2} outside blaze-mem and the global heap; ",
                gib(resident),
                gib(resident.saturating_sub(memory.footprint(self.heap))),
            )?;
        }
        if self.heap != 0 {
            match memory.heap_by_tag {
                true => write!(
                    f,
                    "global heap {:.2} GiB live, {:.2} of it outside any task",
                    gib(self.heap),
                    gib(memory.heap_outside),
                )?,
                false => write!(f, "global heap {:.2} GiB live, not kept by tag", gib(self.heap))?,
            }
            if let Some(committed) = memory.heap_committed {
                write!(
                    f,
                    ", in {:.2} its allocator ever committed ({:.2} not live: kept freed, or purged; this count never falls on a purge and nothing is charged by it)",
                    gib(committed),
                    gib(committed.saturating_sub(self.heap)),
                )?;
            }
            // What the heap can hold resident beyond its live blocks, at most: the process's
            // footprint less blaze-mem's committed pages and the live heap. mimalloc's count above
            // never drops on a purge, so it alone can't say what a give-back left (memcap5).
            if let Some(resident) = self.resident {
                write!(
                    f,
                    ", at most {:.2} of it freed and resident",
                    gib(resident
                        .saturating_sub(memory.resident_committed())
                        .saturating_sub(self.heap)),
                )?;
            }
            f.write_str("; ")?;
        }
        write!(
            f,
            "committed {:.2} GiB (most {:.2}), {:.2} of it purged (not resident), {:.2} GiB in tags, reserved {:.0} GiB in {} regions (most {:.0})",
            gib(memory.committed),
            gib(memory.committed_high_water),
            gib(memory.purged),
            gib(memory.live()),
            gib(memory.reserved),
            memory.regions(),
            gib(memory.reserved_high_water),
        )?;
        if memory.base_pages != 0 {
            write!(
                f,
                ", {:.0} GiB of it off huge pages (most {:.0}), unadvised memory's THP {}",
                gib(memory.base_pages),
                gib(memory.base_pages_high_water),
                memory.thp,
            )?;
        }
        let mut tags = blaze_mem::Tag::ALL;
        tags.sort_by_key(|&tag| {
            let counters = memory.get(tag);
            std::cmp::Reverse(counters.live.saturating_add(counters.heap))
        });
        for tag in tags {
            let counters = memory.get(tag);
            if counters.live < Self::SHOWN
                && counters.heap < Self::SHOWN
                && counters.reserved < Self::SHOWN_RESERVED
            {
                continue;
            }
            write!(
                f,
                "; {tag} {:.2} (most {:.2}, reserved {:.0} in {})",
                gib(counters.live),
                gib(counters.high_water),
                gib(counters.reserved),
                counters.regions,
            )?;
            if counters.heap >= Self::SHOWN {
                write!(
                    f,
                    " + heap {:.2} (most {:.2})",
                    gib(counters.heap),
                    gib(counters.heap_high_water)
                )?;
            }
        }
        if self.chunks != blaze_mem::chunks::Cached::default() {
            write!(
                f,
                "; free chunks warm {:.2} (resident), cold {:.2} (purged)",
                gib(self.chunks.warm),
                gib(self.chunks.cold),
            )?;
        }
        let blaze_query::db::StackHighWater { run_reads, run, pending } = &self.stacks;
        if (*run_reads, *pending) != (0, 0) {
            write!(f, "; query stack most reads one run {run_reads}")?;
            if !run.is_empty() {
                write!(f, " ({run})")?;
            }
            write!(f, ", most pending rows one task {pending}")?;
        }
        let total = self.blocks.total();
        if total.carved == 0 {
            return Ok(());
        }
        write!(
            f,
            "; blocks live {:.2} GiB, warm {:.2}, cold {:.2} ({:.2} given back in {} trims), carved {:.2}, committed {:.2}",
            gib(total.live()),
            gib(total.warm),
            gib(total.cold),
            gib(total.given_back),
            total.trims,
            gib(total.carved),
            gib(total.committed),
        )?;
        let mut classes = self.blocks.classes;
        classes.sort_by_key(|class| std::cmp::Reverse(class.carved));
        for class in
            classes.iter().take(Self::CLASSES_SHOWN).filter(|class| class.carved >= Self::SHOWN)
        {
            write!(
                f,
                "; {} blocks live {:.2}, warm {:.2}, cold {:.2}, carved {:.2}",
                BlockSize(class.size),
                gib(class.live()),
                gib(class.warm),
                gib(class.cold),
                gib(class.carved),
            )?;
        }
        Ok(())
    }
}

/// Says [`MemoryLine`] in the daemon's log and on its standard error: every
/// [`MEMORY_LOG_EVERY`] and at a stop.
pub(super) fn log_memory(memory: &blaze_mem::tag::Snapshot) {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let resident = crate::raw::memory::footprint(std::process::id());
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let resident = None;
    let heap = blaze_graph::arena::held();
    let blocks = blaze_mem::blocks::snapshot();
    let chunks = blaze_mem::chunks::cached();
    let stacks = blaze_query::db::stack_high_water();
    let said = MemoryLine { memory, blocks: &blocks, resident, heap, chunks, stacks }.to_string(); // alloc: one line each MEMORY_LOG_EVERY and at stop
    // logged: the info event below
    eprintln!("blaze serve: {said}");
    tracing::info!(target: "blaze", "{said}");
}

/// Why a start read no image.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Unimaged {
    /// There is no image: the first start in this state directory.
    NoImage,
    /// The image is of another layout: another blaze wrote it.
    Layout(SchemaHash),
}

impl fmt::Display for Unimaged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unimaged::NoImage => write!(f, "there is no daemon image"),
            Unimaged::Layout(found) => {
                write!(f, "the daemon image is of layout {}, another blaze's", found.0)
            }
        }
    }
}

/// Why a daemon did not start.
#[derive(Debug)]
pub enum StartError {
    /// The daemon image at `file` is damaged or inconsistent with its journal.
    Image { file: FileName, fault: ImageFault },
    /// What another part of the start could not do, in its words.
    Other(String),
}

impl StartError {
    fn image(file: &FileName, fault: ImageFault) -> StartError {
        // clone: a refcount, once an error.
        StartError::Image { file: file.clone(), fault }
    }
}

impl From<String> for StartError {
    fn from(what: String) -> StartError {
        StartError::Other(what)
    }
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::Image { file, fault } => write!(
                f,
                "the daemon image /{file}: {fault}; nothing of it is used and the daemon does \
                 not start: `blaze doctor` checks it and prints the command that sets it aside"
            ),
            StartError::Other(what) => f.write_str(what),
        }
    }
}

impl std::error::Error for StartError {}

/// What is wrong with a daemon image that is not another blaze's.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ImageFault {
    /// Not an image, cut short, or a spine that is not the one its hash names.
    Open(OpenError),
    History(HistoryError),
    Vfs(VfsFault),
}

impl fmt::Display for ImageFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageFault::Open(error) => write!(f, "{error}"),
            ImageFault::History(error) => write!(f, "{error}"),
            ImageFault::Vfs(fault) => write!(f, "{fault}"),
        }
    }
}

/// Why the VFS can't restore from an image of this blaze's layout ([`ImageUse`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum VfsFault {
    /// A page group of its tree failed its check.
    Corrupt(Corrupt),
    /// Its tree section is not a tree this blaze writes.
    Malformed(&'static str),
    /// The journal starts from another image (`journal`), or from none, than this one.
    Unnamed { image: ContentId, journal: Option<ContentId> },
    /// Its disks section (the watchers' metadata) is corrupt, outside the file or not one
    /// this blaze writes; never [`DisksUnused::Refused`], which is [`Started::disks`].
    Disks(DisksUnused),
}

impl fmt::Display for VfsFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VfsFault::Corrupt(corrupt) => write!(f, "{corrupt}"),
            VfsFault::Malformed(what) => {
                write!(f, "its VFS tree is not one this blaze writes ({what})")
            }
            VfsFault::Unnamed { image, journal: Some(journal) } => {
                write!(
                    f,
                    "the VFS's journal starts from the image {journal}, not this one ({image})"
                )
            }
            VfsFault::Unnamed { image, journal: None } => {
                write!(f, "the VFS's journal starts from no image, not this one ({image})")
            }
            VfsFault::Disks(unused) => write!(f, "{unused}"),
        }
    }
}

/// Why a running daemon stopped when it was not asked to.
#[derive(Debug)]
pub enum Failed {
    /// The server could not go on listening.
    Server(String),
    /// A group of the image the database or the VFS reads in place failed its check.
    Corrupt(Corrupt),
    /// The image a cold build or the stop asked for was not made; the one before it stays.
    Image(ImageFailed),
    /// A build stopped moving ([`blaze_profile::Stall`]'s words): its workers may be
    /// deadlocked, so the next request starts a fresh daemon.
    Stalled(String),
    /// A store stopped (a file of it could not be read or written), named.
    Store(String),
    /// The VFS could not settle on the image just mapped ([`blaze_vfs::Vfs::settle`]).
    Settle(blaze_vfs::Unsettled),
    /// The profile history did not open, so no build can be numbered past the builds it
    /// names ([`super::plane::Plane::unnumbered`]).
    History(super::observe::Unopened),
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failed::Server(why) | Failed::Stalled(why) | Failed::Store(why) => f.write_str(why),
            Failed::Corrupt(corrupt) => write!(f, "the daemon image: {corrupt}"),
            Failed::Image(failed) => write!(f, "{failed}"),
            Failed::Settle(unsettled) => write!(f, "{unsettled}"),
            Failed::History(unopened) => write!(f, "{unopened}"),
        }
    }
}

/// What of the start an open of the image's bytes gives: the image, or why none is read by a
/// rule, or the fault.
fn opening<I>(opened: Result<I, OpenError>) -> Result<Result<I, Unimaged>, ImageFault> {
    match opened {
        Ok(image) => Ok(Ok(image)),
        Err(OpenError::Layout { found }) => Ok(Err(Unimaged::Layout(found))),
        Err(error) => Err(ImageFault::Open(error)),
    }
}

/// What the VFS did with the image, into `started`, or the fault.
fn vfs_used(used: ImageUse, started: &mut Started) -> Result<(), VfsFault> {
    match used {
        // Unopened: the start opened the same bytes first, and stopped unless another blaze's.
        ImageUse::None | ImageUse::Unopened(_) => {}
        ImageUse::Used { disks, .. } => started.disks = disks_used(disks)?,
        ImageUse::Resumed { disks, .. } => {
            started.disks = disks_used(disks)?;
            started.resumed = true;
        }
        ImageUse::Refused(refused) => started.tree = Some(refused),
        ImageUse::Corrupt(corrupt) => return Err(VfsFault::Corrupt(corrupt)),
        ImageUse::Malformed(what) => return Err(VfsFault::Malformed(what)),
        ImageUse::Unnamed { image, journal } => return Err(VfsFault::Unnamed { image, journal }),
    }
    Ok(())
}

/// Why the watchers' metadata was not used, when by a rule, or the fault.
fn disks_used(unused: Option<DisksUnused>) -> Result<Option<Refused>, VfsFault> {
    match unused {
        None => Ok(None),
        Some(DisksUnused::Refused(refused)) => Ok(Some(refused)),
        Some(
            fault @ (DisksUnused::Corrupt(_) | DisksUnused::Outside | DisksUnused::Malformed(_)),
        ) => Err(VfsFault::Disks(fault)),
    }
}

/// The buffers the loop's calls borrow and give back, so a warm turn allocates none: a call
/// made inside another takes one of its own, so each holds as many as calls nest.
struct Spare<S: Steps> {
    said: Vec<Vec<Said>>,
    out: Vec<Vec<Out>>,
    arrived: Vec<Vec<Arrived>>,
    happened: Vec<Happened<Value<S>>>,
}

impl<S: Steps> Default for Spare<S> {
    fn default() -> Spare<S> {
        Spare { said: Vec::new(), out: Vec::new(), arrived: Vec::new(), happened: Vec::new() }
    }
}

#[cfg(test)]
thread_local! {
    /// A planted bug for the restart tests' controls (`serve::tests`): the start drops every
    /// mark the journal's replay left before it reads them, as a start that kept none would.
    pub(super) static DROPS_MARKS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The planted bug of the client spans' control (`serve::tests`): a client's rows lose its
    /// `client: request on the wire` as the loop takes them, as a daemon that dropped a span.
    pub(super) static DROPS_A_CLIENT_SPAN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What the start read, each once it is in.
#[derive(Default)]
struct Read {
    journal: Option<Vec<u8>>,
    image: Option<Option<Mapping>>,
    socket_free: bool,
    /// When the image was mapped, from the start.
    mapped_in: Duration,
}

impl<D: Driver, S: Steps> Daemon<D, S> {
    /// Opens the store, maps the image for the VFS and the plane's history, replays the VFS's
    /// journal with its marks, and listens; the database starts empty. Fails naming what could not be read, when the image is damaged, or when
    /// another daemon listens; what of the image it did not use by a rule is [`Daemon::started`].
    ///
    /// What the start allocates from the global heap outside a task (the journal's tree, the
    /// image's tables) counts to the loop's tag ([`blaze_mem::frame::heap_of`],
    /// [`blaze_mem::Tag::Plane`]), not to the heap outside any task.
    pub fn start(
        driver: D,
        options: Options,
        steps: Arc<S>,
        runner: Runner<Step<S>>,
    ) -> Result<Daemon<D, S>, StartError> {
        blaze_mem::frame::heap_of(blaze_mem::Tag::Plane, || {
            Self::start_untagged(driver, options, steps, runner)
        })
    }

    fn start_untagged(
        mut driver: D,
        options: Options,
        steps: Arc<S>,
        runner: Runner<Step<S>>,
    ) -> Result<Daemon<D, S>, StartError> {
        // Restart to first answer is budgeted (harness/v2/budgets.py `restart-first-answer-*`):
        // each phase of the start is a span of its own, so a slow start names its phase.
        let _start = tracing::info_span!(target: "blaze", "daemon start").entered();
        let state = options.state.clone();
        let journal = under(&state, "journal");
        let db_dir = under(&state, "db");
        let image = under(&db_dir, "image");
        // A passed socket is neither dialed (that reaches this daemon's own backlog) nor
        // unlinked (its file is the manager's).
        #[cfg(unix)]
        let activated = options.activated.is_some();
        #[cfg(not(unix))]
        let activated = false;
        let mut effects = vec![
            Effect::Stat { file: journal.clone(), tag: STAT_JOURNAL },
            Effect::Stat { file: image.clone(), tag: STAT_IMAGE },
        ];
        if !activated && let Some(socket) = &options.socket {
            effects.push(Effect::Connect { at: Address::Unix(socket.clone()), tag: DIAL });
        }
        // The home store opens once its layer's directory is there ([`LayerDir`]). The walk
        // begins once the std store, whose directory the layer is, has opened: the std store's
        // open writes and removes files of its own under it, and a walk beside it could find a
        // directory there by one of those files (a disk that keeps no empty directory it did
        // not make, the simulation's) and gone by the next step, its make below then refused
        // for a parent that is not there.
        let layer = under(&options.std, "store");
        let (mut layer_dir, first) = LayerDir::new(&layer);
        let mut layer_first = Some(first);
        let store_config =
            blaze_cas::Config {
                dir: under(&state, "store"),
                // The machine's sealed layers (`serve::sealed`): every writer of the shared
                // directory this daemon's std store is a writer of, each installed cover's and
                // each daemon's, read in place behind the home's own records, never copied in
                // (track 7, task A). One lookup: the home store's `get` finds a sealed record.
                layers: vec![layer], // alloc: once per daemon started, one name
                tags: STORE,
                load_tags: LOADS,
                loader: super::direct::pool::loader(&options.reads),
            };
        let mut store_config = Some(store_config);
        let mut store: Option<Store> = None;
        let shared = blaze_cas::SharedConfig {
            root: under(&options.std, "store"),
            writer: options.writer,
            tags: STD_STORE,
            load_tags: STD_LOADS,
            loader: super::direct::pool::loader(&options.reads),
        };
        let mut std = Store::open_shared(shared, &mut effects);
        let mut read =
            Read { socket_free: activated || options.socket.is_none(), ..Read::default() };
        let began = driver.now();
        let mut done = Vec::new();
        let socket = match &options.socket {
            Some(socket) => format!("/{socket}"),
            None => "this process's own connections (the one-shot build)".to_owned(),
        };
        // Waits on the driver for the journal's read, the image's map, the socket's check and
        // both stores' opens.
        let reading =
            tracing::info_span!(target: "blaze", "daemon start: read the state", dir = %state)
                .entered();
        loop {
            let read_all = read.journal.is_some() && read.image.is_some() && read.socket_free;
            let opened = |it: &Store| it.ready() || it.broken().is_some();
            if read_all && store.as_ref().is_some_and(opened) && opened(&std) {
                break;
            }
            if opened(&std)
                && let Some(first) = layer_first.take()
            {
                effects.push(first);
            }
            driver.submit(&mut effects);
            driver.poll(
                &mut done,
                blaze_core::wait::Deadline::after(driver.now(), Duration::from_millis(50)),
            );
            for completion in done.drain(..) {
                if let Some(store) = &mut store
                    && store.owns(completion.tag)
                {
                    store.complete(completion, &mut effects);
                    continue;
                }
                if completion.tag == LAYER_DIR {
                    match layer_dir.done(completion.done) {
                        LayerStep::Ask(effect) => effects.push(effect),
                        LayerStep::There => {
                            let Some(config) = store_config.take() else {
                                return Err("the home store was opened twice".to_owned().into());
                            };
                            let mut opened = Store::open(config, &mut effects);
                            opened.encode_columns(blaze_format::Spans::columns());
                            opened.encode_columns(hir_def::item_tree::ItemTree::columns());
                            // The home reads the machine's sealed layer's in-process writer live
                            // (`blaze_cas::Store::read_live`): what this daemon's own builds put
                            // into the layer, a sealed unit's record and outputs and a sealed
                            // crate's front-end records, is found by its next look before the
                            // layer commits, as `blaze serve`'s `Handle::install_layer` does.
                            opened.read_live(&std.reader()).map_err(|why| {
                                format!("the home store reads the machine's layer {} live: {why}", std.dir())
                            })?;
                            store = Some(opened);
                        }
                        LayerStep::Failed(why) => {
                            return Err(format!("making the machine's sealed layers' directory: {why}").into());
                        }
                    }
                    continue;
                }
                if std.owns(completion.tag) {
                    std.complete(completion, &mut effects);
                    continue;
                }
                let Completion { tag, done } = completion;
                match (tag, done) {
                    (STAT_JOURNAL, Done::Stat(None)) => read.journal = Some(Vec::new()),
                    (STAT_JOURNAL, Done::Stat(Some(_))) => {
                        effects.push(Effect::ReadAll { file: journal.clone(), tag: READ_JOURNAL })
                    }
                    (READ_JOURNAL, Done::Read(bytes)) => read.journal = Some(bytes),
                    (STAT_IMAGE, Done::Stat(None)) => read.image = Some(None),
                    (STAT_IMAGE, Done::Stat(Some(_))) => {
                        effects.push(Effect::Map { file: image.clone(), extent: blaze_core::Extent::Whole, tag: MAP_IMAGE })
                    }
                    (MAP_IMAGE, Done::Mapped(mapping)) => {
                        read.image = Some(Some(mapping));
                        read.mapped_in = driver.now().saturating_sub(began);
                    }
                    (DIAL, Done::Connected { conn }) => {
                        effects.push(Effect::Hangup { conn, tag: HANG_UP });
                        driver.submit(&mut effects);
                        return Err(format!(
                            "another blaze serve listens on {socket}: `blaze stop` stops it"
                        )
                        .into());
                    }
                    (DIAL, Done::Failed(_)) => match &options.socket {
                        Some(socket) => {
                            effects.push(Effect::Remove { file: socket.clone(), tag: UNLINK })
                        }
                        None => read.socket_free = true,
                    },
                    (UNLINK, Done::Removed | Done::Failed(_)) => read.socket_free = true,
                    (READ_JOURNAL, Done::Failed(error)) => {
                        return Err(format!("reading the VFS's journal /{journal}: {error}").into());
                    }
                    (MAP_IMAGE, Done::Failed(error)) => {
                        return Err(format!("mapping the daemon image /{image}: {error}").into());
                    }
                    (STAT_JOURNAL | STAT_IMAGE, Done::Failed(error)) => {
                        return Err(format!("the daemon's state in /{state}: {error}").into());
                    }
                    (tag, done) => {
                        return Err(
                            format!("the start was answered {done:?} for tag {}", tag.0).into()
                        );
                    }
                }
            }
        }
        drop(reading);
        let Some(mut store) = store else {
            return Err("the start's wait ended with the home store unopened".to_owned().into());
        };
        if let Some(broken) = store.broken() {
            return Err(broken.to_string().into());
        }
        if let Some(broken) = std.broken() {
            return Err(format!("the standard library's shared store: {broken}").into());
        }
        // Every expansion from here on reads the daemon's own store, with no lock.
        let keeping = proc_macro_api::kept::Keeping::new(proc_macro_api::kept::Expansions {
            store: store.reader(),
            toolchain: options.toolchain,
            environment: proc_macro_api::kept::Environment::new(),
        })
        .map_err(|why| format!("keeping proc-macro expansions: {why}"))?;
        // alloc: once per daemon
        let sources = std::path::PathBuf::from(format!("/{}", options.sources));
        let macros =
            blaze_macro_eval::Macros::new(proc_macro_api::kept::Keeps::In(keeping), Some(sources));
        let mapping = read.image.flatten();
        // The image's file as it is on disk, for the next image to share its blocks
        // ([`Images::new`]).
        // clone: a count of the mapping, once a start.
        let on_disk = mapping.clone();
        let mut started = Started::default();
        // clone: a count of the mapping, shared by the VFS and the image the history reads.
        let opened = match mapping.clone().map(|it| opening(Image::open(it))).transpose() {
            Ok(None) => {
                started.unimaged = Some(Unimaged::NoImage);
                None
            }
            Ok(Some(Ok(opened))) => {
                started.mapped = Some(Mapped {
                    shape: super::image::Shape(&opened).to_string(),
                    mapped_in: read.mapped_in,
                });
                Some(opened)
            }
            Ok(Some(Err(unimaged))) => {
                started.unimaged = Some(unimaged);
                None
            }
            Err(fault) => return Err(StartError::image(&image, fault)),
        };
        let restoring =
            tracing::info_span!(target: "blaze", "daemon start: restore the VFS").entered();
        // The VFS's file texts are the store's: the image names each file by its id alone.
        let texts = super::image::StoreTexts::new(&mut store);
        let (mut vfs, recovered) = Vfs::restore(
            journal.clone(),
            VFS,
            &read.journal.unwrap_or_default(),
            // clone: a count of the texts, once a start.
            mapping.map(|file| blaze_vfs::ImageFile { file, texts: texts.clone() }),
        );
        vfs.hash_on(on_pool);
        started.journal_cut = recovered.skipped;
        started.heads_dropped = recovered.heads_dropped;
        vfs_used(recovered.image, &mut started)
            .map_err(|fault| StartError::image(&image, ImageFault::Vfs(fault)))?;
        // The texts the image the start used names are pinned again before anything collects
        // (serve/image.rs, `text_pin_at_start`): a store of the layout before says so.
        let (text_pin, _) = super::image::text_pin_at_start(
            &mut store,
            vfs.image_file_ids(),
            blaze_gc::record::now_ns(),
        )?;
        drop(restoring);
        let config =
            blaze_query::Config { tree: Some(Arc::new(vfs.reader())), dir: Some(db_dir), tags: DB };
        let opening_db =
            tracing::info_span!(target: "blaze", "daemon start: open the database").entered();
        let mut db = Database::new(config);
        steps.register(&mut db);
        drop(opening_db);
        let learning =
            tracing::info_span!(target: "blaze", "daemon start: restore the plane's history")
                .entered();
        let history = match opened.as_ref() {
            None => History::new(HistoryConfig::default()),
            Some(_) if options.without.history => {
                started.unlearned = Some(Unlearned::Asked);
                History::new(HistoryConfig::default())
            }
            Some(opened) => {
                let (history, unlearned) = super::image::history(opened, HistoryConfig::default())
                    .map_err(|error| StartError::image(&image, ImageFault::History(error)))?;
                started.unlearned = unlearned;
                history
            }
        };
        drop(learning);
        drop(opened);
        // The process's footprint as the system counts it (rule one-memory-measure), for the
        // memory's reports: never a decision's input.
        blaze_mem::tag::resident_by(crate::raw::memory::own_footprint);
        let plane_config = PlaneConfig { io: options.io, work: options.threads.max(1) };
        let mut observe = Observe::new(options.profiling);
        // Admission is by cores alone: nothing is held back for memory, and the kernel's limit
        // is the one ceiling.
        let mut plane = Plane::new(plane_config, history, runner)
            .map_err(|error| format!("the plane: {error}"))?;
        plane.number(observe.numbering());
        // The explorer's task table: every node and move the plane says, on the profile's clock.
        #[cfg(feature = "native")]
        {
            plane.publish_tasks(observe.tasks());
            observe.clock_tasks(driver.now());
        }
        let mut mounts = Mounts::recover(&vfs);
        let forgotten = mounts.forget_unwritten(&mut vfs, &format!("/{}", options.sources), |at| {
            crate::serve::fetch::on_disk_mark(std::path::Path::new(at))
        });
        if let Some((first, why)) = forgotten.first() {
            tracing::info!(target: "blaze", packages = forgotten.len(), first = %first, why = %why, "fetched packages whose copy on disk is not their mount's are fetched again");
        }
        let mut listening = socket.clone();
        #[cfg(unix)]
        let unix = match (options.activated, &options.socket) {
            (Some(passed), Some(at)) => {
                listening.push_str(&format!(" (passed by {})", passed.by()));
                Some(Unix::Passed { socket: passed, at: at.clone() })
            }
            (None, Some(at)) => Some(Unix::Bind(at.clone())),
            (Some(passed), None) => {
                let by = passed.by();
                return Err(format!("the socket {by} passed is listened on at its name, and the daemon was given none").into());
            }
            (None, None) => None,
        };
        #[cfg(not(unix))]
        let unix = options.socket.clone().map(Unix::Bind);
        if let Some((at, _)) = &options.http {
            listening.push_str(&format!(" and http://{at}"));
        }
        let server_config = ServerConfig {
            local: unix.is_none(),
            unix,
            http: options.http.map(|(at, token)| Http { at, token }),
            tags: SERVER,
            most_connections: 1024,
        };
        let server = Server::new(server_config, &mut effects);
        // Bound here, not on the first turn: once `start` returns, `blaze serve` says it listens
        // and writes the ready byte the CLI that started it waits for, and that CLI connects at
        // once. Every driver binds as the listen is submitted.
        driver.submit(&mut effects);
        let mut jobs = Jobs::new(Setup {
            views: format!("/{state}/views"),
            std: format!("/{}", options.std),
            builds: format!("/{}", options.builds),
            sources: format!("/{}", options.sources),
            store: format!("/{state}/store"),
            host: options.host,
            env: options.env,
            color: options.color,
            tags: JOBS,
            runs: RUNS,
            runs_end: RUNS_END,
            slots: format!("/{state}/slots"),
            premises: options.premises.clone(), // clone: the jobs' own copy, once a start
            stores: super::direct::Stores::new(store.reader(), std.reader(), options.reads.clone()), // clone: a path's refcount, once a start
        })
        .map_err(|error| format!("the jobs' tables: {error}"))?;
        // The front end's carry sessions the journal marks: each checkout's next build opens
        // its own, carried to the build's root, rather than seeding one
        // (`front_carry::Pass::Open`). Read from the marks the journal's replay left, image or
        // not; a mark that does not decode is said and its checkout seeds.
        {
            #[cfg(test)]
            if DROPS_MARKS.get() {
                // alloc: a test's plant, once a start
                let names: Vec<String> = vfs.marks("").map(|(name, _)| name.to_owned()).collect();
                for name in names {
                    _ = vfs.unmark(&name);
                }
            }
            let _span = tracing::info_span!(
                target: "blaze",
                "daemon start: read the local marks",
                marks = vfs.marks("").count() as u64,
                carries = vfs.marks(super::local::CARRY).count() as u64,
                why = tracing::field::Empty,
            )
            .entered();
            started.carries = jobs.restore_carries(&mut vfs);
            if let Some(why) = &started.carries {
                _span.record("why", why.as_str());
            }
            // The launched programs' start images the journal marks: each is restored when
            // first asked for, with no build ([`launch`]).
            steps.launch_marked(store.reader(), driver.waker(), launch::marked(&mut vfs));
            // Each test suite's index head the journal marks: its next run takes it
            // ([`super::job::Jobs::test_heads`]).
            steps.test_heads_marked(super::job::test_heads_marked(&mut vfs));
        }
        // The system's memory pressure, watched for the daemon's life: it fails at once where
        // there is no source ([`Daemon::pressure`]).
        effects.push(Effect::Pressure { tag: PRESSURE });
        // Its countdown starts with it: a daemon no client connects to within its grace stops.
        let unheld = options.unheld.map(|grace| super::unheld::Unheld::new(grace, driver.now()));
        Ok(Daemon {
            unheld,
            ended_unheld: false,
            driver,
            effects,
            done,
            listening,
            server,
            session: Session::new(),
            vfs,
            db: Arc::new(db),
            store,
            macros,
            std,
            plane,
            mounts,
            heads: Heads::new(FLUSHES, WATCHERS, RUNS, Outside::new(options.outside, "/", OUTSIDE)),
            images: Images::new(image, IMAGE, on_disk, texts, text_pin),
            jobs,
            steps,
            replies: Vec::new(),
            spare: Spare::default(),
            stopping: None,
            stop_asked: false,
            stop_from: None,
            stop_said: String::new(),
            changed_since_settle: false,
            abandoned: None,
            settling: false,
            stop_settles: None,
            answering: Vec::new(), // alloc: none until a request
            ahead: super::ahead::Ahead::new(false),
            ahead_run: None,
            launch: launch::Launch::default(),
            quiet_turns: 0,
            failed: None,
            pid: std::process::id(),
            build: options.build,
            settings: options.settings,
            started,
            state,
            observe,
            watchdog: Watchdog::new(options.stall),
            dump_watch: Watchdog::new(options.stall / 2),
            step_watch: Watchdog::new(LONG_STEP),
            stillness: Stillness::default(),
            ending: blaze_hash::HashMap::new_in(std::alloc::Global),
            settled: 0,
            job_effects: 0,
            working: 0,
            pulse: Arc::default(), // alloc: once a daemon
            stall: None,
            stuck: Vec::new(),
            completions: 0,
            plotted: Duration::ZERO,
            worked: false,
            idle_since: None,
            broken_down: Duration::ZERO,
            shown: super::breakdown::Shown::default(),
            looked: Duration::ZERO,
            builds: Vec::new(), // alloc: none until a build is looked at
            memory_logged: Duration::ZERO,
            given_back: Duration::ZERO,
            idle: super::idle::Idle::default(),
            solo: blaze_certify::kept::Take::new(SOLO_ROWS),
            solo_looked: None,
            pressure_watched: true,
            gc: None,
            settles: None,
            collects: None,
            peers: None,
            packings: super::packs::daemon::Packings::new(),
            gc_answers: Vec::new(), // alloc: none until `blaze gc` asks
            fetch_urls: super::fetch_url::FetchUrls::new(),
            held: Vec::new(), // alloc: none until a build runs out of space
        })
    }

    /// Collects what blaze keeps on disk with `gc`, in the background of its turns.
    /// Moves what its store leaves unread cold, between requests ([`super::settle`]).
    pub fn settle(&mut self, settles: super::settle::Settles) {
        self.settles = Some(settles);
    }

    /// Keeps its store under the home's budget, in the background of its turns
    /// ([`super::collect`]).
    pub fn collect_store(&mut self, collects: super::collect::Collects) {
        self.collects = Some(collects);
    }

    pub fn collect(&mut self, gc: super::gc::Gc) {
        self.gc = Some(gc);
    }

    /// The collector, when it collects.
    pub fn gc(&self) -> Option<&super::gc::Gc> {
        self.gc.as_ref()
    }

    /// Where it listens, for people.
    pub fn listening(&self) -> &str {
        &self.listening
    }

    /// A connection of this process's own, for a daemon with no socket ([`Options::socket`]
    /// `None`): the one-shot build's. `None` for one that listens, or has stopped.
    pub fn connect_local(&mut self) -> Option<blaze_core::Conn> {
        self.server.open_local()
    }

    /// `bytes`, frames of requests this process sent on `conn`, handed to the session as a
    /// socket's would be; what they ask runs in the turns after.
    pub fn send_local(&mut self, conn: blaze_core::Conn, bytes: &[u8]) {
        let mut arrived = self.spare.arrived.pop().unwrap_or_default();
        self.server.send_local(conn, bytes, &mut self.effects, &mut arrived);
        for it in arrived.drain(..) {
            self.arrive(it);
        }
        self.spare.arrived.push(arrived);
    }

    /// What was replied on this process's connection `conn` since it was last taken, its frames
    /// appended to `into`; `false` once the connection is gone.
    pub fn take_local(&mut self, conn: blaze_core::Conn, into: &mut Vec<u8>) -> bool {
        self.server.take_local(conn, into, &mut self.effects)
    }

    /// What of its image the start did not use, and why.
    pub fn started(&self) -> &Started {
        &self.started
    }

    /// Why it stopped when it was not asked to.
    /// The bound of its stall watchdog ([`Options::stall`]): a build, or its stop
    /// (`serve::stop`), in which nothing moves for this long has stalled.
    pub fn stall_bound(&self) -> Duration {
        self.watchdog.bound()
    }

    /// Whether it was asked to stop: by a signal, a client's Stop, or its own end.
    pub fn stopping(&self) -> bool {
        self.stopping.is_some()
    }

    /// Lets go of the profile history once its stop is over, for an engine whose process ends
    /// without dropping it (`serve::Running::exit`): the history's thread then writes its last
    /// flush and ends, as it does when a daemon is dropped.
    pub fn let_go_of_history(&mut self) {
        debug_assert!(
            matches!(self.stopping, Some(Stopping::Over)),
            "the history is let go of only once the stop is over"
        );
        self.observe.let_go_of_history();
    }

    /// Whether it stopped itself because no connection held it for its grace
    /// ([`Options::unheld`]): `blaze serve` then ends with `Exit::Unheld`.
    pub fn ended_unheld(&self) -> bool {
        self.ended_unheld
    }

    /// Its grace when it exits once nothing holds it ([`Options::unheld`]).
    pub fn unheld_grace(&self) -> Option<Duration> {
        self.unheld.as_ref().map(super::unheld::Unheld::grace)
    }

    /// A safeguard of its countdown taken out, for the tests' controls.
    #[cfg(test)]
    pub(super) fn plant_unheld(&mut self, plant: super::unheld::Plant) {
        if let Some(unheld) = &mut self.unheld {
            unheld.plant = plant;
        }
    }

    /// The look of a daemon that exits once no connection holds it ([`super::unheld`]), each
    /// turn: the connections open at the API's socket and the launchers' are what holds it,
    /// counted where the kernel says each ended, so a client killed however it dies lets go.
    /// Once none was open for its grace it stops as `blaze stop` stops it, image first.
    fn look_unheld(&mut self) {
        if self.stopping.is_some() {
            return;
        }
        let Some(unheld) = &mut self.unheld else { return };
        let held = self.server.connections() + self.steps.launchers_connected();
        if !unheld.look(self.driver.now(), held) {
            return;
        }
        let grace = unheld.grace();
        tracing::info!(
            target: "blaze",
            grace_s = grace.as_secs(),
            "the daemon exits: no client or launcher connection was open for its grace of {} s ({})",
            grace.as_secs(),
            super::unheld::VAR
        );
        self.ended_unheld = true;
        self.stop();
    }

    pub fn failed(&self) -> Option<&Failed> {
        self.failed.as_ref()
    }

    /// Stops it, failed with `why` unless it failed already.
    fn fail(&mut self, why: Failed) {
        if self.failed.is_none() {
            self.failed = Some(why);
        }
        self.stop();
    }

    pub fn driver(&mut self) -> &mut D {
        &mut self.driver
    }

    /// What the stop hung up on once [`LAST_EFFECTS`] passed with replies still being sent:
    /// the clients that never got the end of their answers. `None` when every send ended.
    pub fn abandoned(&self) -> Option<&Abandoned> {
        self.abandoned.as_ref()
    }

    /// The driver, the daemon let go: what a restart starts on.
    pub fn into_driver(self) -> D {
        self.driver
    }

    /// How many daemon images it has made in its life ([`Images::made`]).
    pub fn images_made(&self) -> u64 {
        self.images.made()
    }

    /// Whether no job runs and the database has settled onto the image of its last cold
    /// build.
    pub fn settled(&self) -> bool {
        self.jobs.quiet() && !self.jobs.cold && !self.settling
    }

    /// How many answered jobs still have something settling: a kept front end being stored,
    /// or puts the store has not made durable yet (`Jobs::settling`).
    pub fn jobs_settling(&self) -> usize {
        self.jobs.settling()
    }

    /// The last of what answered jobs left to settle that was not kept, oldest first
    /// (`Jobs::lost`): each was said in the daemon's log and is a bug in `blaze status`.
    pub fn lost(&self) -> &[super::job::Lost] {
        self.jobs.lost()
    }

    pub fn vfs(&self) -> &Vfs {
        &self.vfs
    }

    /// The lowerings held for their root's kept front end to be stored, as a stall names them.
    pub fn held_lowerings(&self) -> Vec<super::job::HeldLowering> {
        self.jobs.held_lowerings()
    }

    /// How many kept front ends are being stored ([`super::job`]'s `store_kept`).
    pub fn storing(&self) -> usize {
        self.jobs.storing()
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    /// Written only by the loop; read elsewhere through [`Store::reader`].
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// When the first job waiting for the store gives up, on the injected clock
    /// ([`super::job::Jobs::next_deadline`]).
    #[cfg(test)]
    pub(super) fn store_deadline(&self) -> Option<Duration> {
        self.jobs.next_deadline()
    }

    /// Asks the store to place `key` at `to` as a build would: store traffic that is no step of
    /// its commit, for a test that it does not count as the commit's progress.
    #[cfg(test)]
    pub(super) fn place_for_test(&mut self, key: &blaze_cas::Key, to: FileName) -> Option<Tag> {
        self.store.place(key, to, &mut self.effects)
    }

    /// The store, for a test to make a writer of its own: puts in a segment other than the
    /// loop's, as a worker's are.
    #[cfg(test)]
    pub(super) fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    /// Flushes the store as a job's put does: a commit begun outside any job.
    #[cfg(test)]
    pub(super) fn flush_store_for_test(&mut self) {
        self.store.flush(&mut self.effects);
    }

    /// Holds every thread task of `kind` on the simulation's runner, still running, while
    /// `held`, and lets them run from the next turn once not ([`super::plane::Inline::hold`]): a
    /// task that runs long, as a large front end's store does on a worker.
    #[cfg(test)]
    pub(super) fn hold_tasks_for_test(
        &mut self,
        kind: blaze_graph::admission::TaskKind,
        held: bool,
    ) {
        match self.plane.runner_mut() {
            Runner::Inline(runner) if held => runner.hold(kind),
            Runner::Inline(runner) => runner.release(kind),
            Runner::Threads(_) => panic!(
                "holding the tasks of {kind:?} needs the simulation's inline runner, not threads"
            ),
        }
    }

    /// Each thread task that runs now, with its request, least node first: what a test plants
    /// the task watchdog's verdict on ([`Pulse::say_stuck`]).
    #[cfg(test)]
    pub(super) fn running_thread_tasks_for_test(
        &self,
    ) -> Vec<(blaze_graph::plane::RequestId, blaze_graph::machine::NodeId)> {
        let plane = &self.plane;
        let mut found = Vec::new();
        plane.unsettled(|it| {
            if matches!(it.state, blaze_protocol::events::NodeState::Running)
                && !plane.loop_runs(it.id)
            {
                found.push((it.request, it.id));
            }
        });
        found.sort_by_key(|it| it.1.0);
        found
    }

    /// The standard library's store, shared with this build's other daemons.
    pub fn std_store(&self) -> &Store {
        &self.std
    }

    /// Makes its store the process's ([`blaze_cas::Handle::install`]): the daemon of `blaze
    /// serve` alone, as a process has one store, and a test's daemons are many to a process. Its
    /// root is the driver's, `/`.
    pub fn install_store(&mut self) -> Result<&'static blaze_cas::Handle, String> {
        let handle = blaze_cas::Handle::install(blaze_cas::Handle::new(
            &mut self.store,
            std::path::PathBuf::from("/"),
        ))?;
        // The machine's sealed layer, which the home store reads below its own
        // (`Config::layers`): this daemon's writer of it is what a sealed crate built here, and
        // a pack fetched on a miss for one, is put through ([`blaze_cas::Handle::layer`]), so
        // every home on the machine finds it and none builds it again.
        let layer_dir = self.std.dir().clone(); // clone: a refcount, once per daemon
        blaze_cas::Handle::install_layer(blaze_cas::Handle::new(
            &mut self.std,
            std::path::PathBuf::from("/"),
        ))
        .map_err(|why| format!("the machine's layer store {}: {why}", layer_dir.as_str()))?;
        // Lane packs-1112: the read-only endpoint peers read this store through, only when
        // `<root>/packs.json` asks for one (`serve`); one asked for that the settings do not
        // allow stops the daemon's start, naming why. It listens on this daemon's driver.
        self.peers = super::packs::endpoint::Endpoint::asked(PEERS, &mut self.effects)?;
        Ok(handle)
    }

    pub fn mounts(&self) -> &Mounts {
        &self.mounts
    }

    /// Asks it to stop: the image first.
    pub fn stop(&mut self) {
        if self.stopping.is_some() {
            return;
        }
        // Before anything the stop does that may hang: `blaze serve`'s stop guard (`serve::stop`)
        // bounds it from here, though a client's Stop asked it inside a turn.
        #[cfg(unix)]
        super::signals::stop_begun();
        // The memory by tag as the work ends, its high-water marks the run's.
        log_memory(&blaze_mem::tag::snapshot());
        self.stopping = Some(Stopping::Image);
        // Nothing is built ahead from here on, and what is, is cancelled (the turn's
        // `ahead_turn` sends the cancel).
        self.ahead.stop();
        // The launch host's start images kept so far are put, and marked once durable before
        // the image (the stop's wait below counts them); a first call is built no more.
        self.launch_turn();
        // The socket's name first: the next daemon may bind it while this one writes its
        // image, and a client that comes meanwhile finds no one rather than waits.
        self.server.unlisten(&mut self.effects);
        // The pressure watch is in flight until it is undone: the stop's last step waits for
        // nothing in flight.
        if self.pressure_watched {
            self.effects.push(Effect::Unwatch { tag: PRESSURE });
        }
        let mut replies = std::mem::take(&mut self.replies);
        self.observe.stop(&mut replies);
        self.send(replies);
        self.heads.stop(&mut self.effects);
        // Every kept front end's workspace members are written before the image (R3, rule (b)):
        // the next daemon reads them from their pages, and the edited member alone, with those
        // above it, is resolved anew. Each store settles as what an answered build left does,
        // so the wait below is theirs too.
        let mut said = self.spare.said.pop().unwrap_or_default();
        let members = self.jobs.store_all_members(&mut world!(self, said));
        self.said(said);
        if members > 0 {
            tracing::info!(target: "blaze", members, "the stop writes the workspace members' pages of its kept front ends");
        }
        // What answered builds left to settle first: the image is of the daemon with every
        // kept front end back, and the stop's flush then takes what their stores put.
        let left = self.jobs.settling() + self.launch.due.len() + usize::from(self.launch.undurable());
        if left > 0 {
            let span = tracing::info_span!(
                target: "blaze",
                "the stop waits for what answered builds left to settle",
                left,
            );
            self.stop_settles = Some((self.driver.now(), span));
            self.store.flush(&mut self.effects);
            return;
        }
        self.stop_image();
    }

    /// The stop's image asked and every store flushed, once nothing answered builds left is
    /// still settling ([`Daemon::stop`]).
    fn stop_image(&mut self) {
        // A settle image asked with nothing changed since is the daemon as it is now: the stop
        // waits for it ([`Images::done`]) rather than makes the same image again after it.
        if !(self.settling && !self.changed_since_settle) {
            self.ask_image(true);
        }
        self.store.flush(&mut self.effects);
        self.std.flush(&mut self.effects);
        self.vfs.flush(&mut self.effects);
    }

    /// Asks for an image of the daemon as it is now. `waited`: someone waits for it (the
    /// stop), so no request lets go of it; the settle's, asked between requests, is let go
    /// when one comes ([`Daemon::let_go_of_image`]).
    fn ask_image(&mut self, waited: bool) {
        // The marks go to the journal the image's flush rewrites, so an image never leaves a
        // durable carry session unmarked.
        self.jobs.carry_sessions(&mut self.vfs, &self.store);
        let parts =
            Parts {
                vfs: &mut self.vfs,
                store: &mut self.store,
                predict: self.plane.predict(),
                effects: &mut self.effects,
            };
        match waited {
            true => self.images.ask(parts),
            false => self.images.ask_between(parts),
        }
    }

    /// A request is under way while the settle's image is being written: the image is let go
    /// ([`Images::let_go`]), so the request shares neither the disk nor the loop with it, and
    /// asked again once the daemon is idle (`between`). The database stays as it is until
    /// that image is mapped.
    fn let_go_of_image(&mut self) {
        // A request read and held for its checkout's watcher is a request under way too.
        let asked = !self.jobs.idle() || self.heads.waiting() > 0;
        if self.stopping.is_none() && asked && self.images.let_go() {
            self.jobs.cold = true;
            self.settling = false;
        }
    }

    /// One turn of the loop: what is asked submitted, what completed taken, the plane and the
    /// jobs stepped. `false` once it has stopped.
    ///
    /// What the loop allocates from the global heap outside a task counts to its tag
    /// ([`blaze_mem::frame::heap_of`], [`blaze_mem::Tag::Plane`]); a plane task it runs inline
    /// counts to its own kind's (`plane::run_measured`).
    pub fn turn(&mut self) -> bool {
        blaze_mem::frame::heap_of(blaze_mem::Tag::Plane, || self.turn_untagged())
    }

    fn turn_untagged(&mut self) -> bool {
        if matches!(self.stopping, Some(Stopping::Over)) {
            return false;
        }
        use blaze_core::wait::Deadline;
        // The turn's deadline on the driver's clock, the soonest of what ends the wait, passed
        // to the driver as it is: no layer turns it into a length to restart (deadline-waits).
        let now = self.driver.now();
        let mut wait = if !self.effects.is_empty() || self.plane.ready() {
            (Deadline::PASSED, Woken::Idle)
        } else if self.plane.running() > 0 && self.plane.wakes() {
            (Deadline::after(now, MEASURES_EVERY), Woken::PlaneWakes)
        } else if self.plane.running() > 0 {
            (Deadline::after(now, Duration::from_millis(1)), Woken::PlaneTasks)
        } else {
            (Deadline::after(now, Duration::from_millis(50)), Woken::Idle)
        };
        let mut sooner = |until: Option<Deadline>, woken: Woken| {
            if let Some(until) = until
                && until < wait.0
            {
                wait = (until, woken);
            }
        };
        sooner(
            self.session.deadline().map(|at| Deadline::at(Duration::from_nanos(at))),
            Woken::Session,
        );
        // A job's wait for the store to make its puts durable has a deadline of its own.
        sooner(self.jobs.next_deadline().map(Deadline::at), Woken::Durable);
        sooner(self.observe.wait(now).map(|it| Deadline::after(now, it)), Woken::Observe);
        // A save's quiet running out: its build ahead is due ([`super::ahead::QUIET`]).
        sooner(self.ahead.deadline(now).map(Deadline::at), Woken::Idle);
        // The grace of a daemon nothing holds running out: it stops then, on time.
        if self.stopping.is_none() {
            sooner(self.unheld.as_ref().and_then(super::unheld::Unheld::deadline).map(Deadline::at), Woken::Unheld);
        }
        if let Some(settles) = &self.settles {
            // A batch out comes back over a channel no driver wakes for, and a plan under way
            // takes a step a turn while the daemon is between requests.
            if settles.out()
                || settles.scanning()
                || settles.carrying()
                || (self.store.settling() && self.quiet_between_requests())
            {
                sooner(Some(Deadline::after(now, Duration::from_millis(1))), Woken::Idle);
            }
            sooner(settles.policy.next().map(Deadline::at), Woken::Idle);
        }
        if let Some(collects) = &self.collects {
            // Its batch out comes back over a channel no driver wakes for, and its core
            // says when its next step is due.
            sooner(collects.next_deadline(now).map(Deadline::at), Woken::Idle);
        }
        self.submit();
        let mut done = std::mem::take(&mut self.done);
        let (until, woken) = wait;
        // Completions a task's store read on the lent driver was handed (`step_plane`) are
        // ready already: the turn only looks for more.
        if !done.is_empty() {
            self.driver.poll(&mut done, Deadline::PASSED);
        } else if until.passed(now) {
            self.driver.poll(&mut done, until);
        } else {
            // The loop's time between its turns, said as a wait on what ends it (rule 15).
            let driver = &mut self.driver;
            blaze_graph::wait::wait(woken.on(), None, || driver.poll(&mut done, until));
        }
        self.turn_after_wait(done)
    }

    /// What a turn does once its wait ended, with the completions the wait gave: everything of
    /// a turn but the loop's one wait (`turn_untagged`'s `Driver::poll`).
    ///
    /// The rule: the daemon's loop thread waits in one place, the driver's poll, and nothing
    /// else it runs may block; work that blocks (a file read or written, a lock another thread
    /// holds for long, a channel with a bound, a child awaited) is an effect the driver carries
    /// out or a plane task on a worker. `#[blaze::nonblocking]` holds this function and
    /// everything it calls to that (docs/v2/plan/design/effects.md): `blaze check` refuses a
    /// blocking call under it, naming the chain. A call proven not to wait is marked
    /// `#[blaze::nonblocking(trusted = "why")]` where it is defined, never allowed here.
    ///
    /// Off for one release (2026-11-10): the released blaze (stage 0, 0.20261105.4) takes
    /// `blaze_ids::Idx<T>`'s `PhantomData<fn() -> T>` for a function pointer it cannot see
    /// through and refuses this function; the checker is fixed in 02b092913b. Put the attribute
    /// back when `.github/blaze-version` names a blaze with that fix. Until then the loop is
    /// held at run time by the `loop stalled` span (`Stalled`) and harness/v2/loop_syscalls.py.
    ///
    /// `#[blaze::bounded_turn]` (`blaze::loop_turn_effects`, docs/v2/plan/design/effect-summary.md)
    /// holds the turn to more than that: no IO, no sync and no lock either, through any call. The
    /// released blaze does not know the word and leaves it inert (`register_tool(blaze)`).
    // #[blaze::nonblocking]
    #[blaze::bounded_turn]
    fn turn_after_wait(&mut self, mut done: Vec<Completion>) -> bool {
        let mut step = Stalled::begin("completions");
        for completion in &done {
            self.ended(completion);
        }
        // A turn with nothing to do is no span: an idle loop turns every millisecond.
        let _span = (!done.is_empty() || self.plane.ready())
            .then(|| tracing::info_span!("loop turn").entered());
        // Each part of a turn that has a span is a child of it, so a long turn names what it
        // was inside (slow-is-p0-traced); an idle turn opens none.
        let spanned = _span.is_some();
        // An output an answered build left being placed is under way as a job is: the work
        // between requests (the image's pass over the database, the collections) is counted
        // from the last answer and the last such placement, never run beside one.
        self.quiet_turns = quiet_turns_after(
            self.quiet_turns,
            !self.jobs.idle() || self.jobs.placing_after() != 0,
            !done.is_empty(),
            self.plane.running() != 0,
        );
        let part = spanned.then(|| {
            tracing::info_span!(target: "blaze", "loop turn: completions", count = done.len())
                .entered()
        });
        // Each piece of this part is a span of its own while the turn is spanned (rule
        // slow-is-p0-traced): a no-op build's turn here took 240 ms of the loop's own CPU for
        // one completion, and 72 ms for none, with nothing naming which piece (Mac, bstd
        // hello-world, 2026-11-12).
        let piece = |name: &'static str| {
            spanned.then(|| {
                tracing::info_span!(target: "blaze", "loop turn: completions: a piece", piece = name)
                    .entered()
            })
        };
        let tick = piece("tick");
        self.tick();
        drop(tick);
        for completion in done.drain(..) {
            let _dispatch = piece("dispatch of one completion");
            self.dispatch(completion);
        }
        // What the loop's own store reads were handed as they dispatched (`World::reads`) is
        // kept for the next turn, which takes it without a wait.
        done.append(&mut self.done);
        self.done = done;
        // What the completions' requests were answered with goes to the driver now: a reply
        // made as its request was read (the no-op lane's, a status) is written in this turn
        // before the plane is stepped or anything between requests runs.
        if !self.effects.is_empty() {
            let _submit = piece("submit");
            self.submit();
        }
        let expire = piece("expire jobs");
        self.expire_jobs();
        drop(expire);
        let ahead = piece("build ahead");
        self.ahead_turn();
        drop(ahead);
        drop(part);
        step.next("the plane's step");
        let part = spanned
            .then(|| tracing::info_span!(target: "blaze", "loop turn: the plane's step").entered());
        self.step_plane();
        drop(part);
        step.next("observe, watch and say");
        // What the turn has to send so far (an answer, a flush asked) is handed to the driver
        // before the work between requests below (the collections, the image's step): an
        // answer never waits behind them.
        if !self.effects.is_empty() {
            self.submit();
        }
        let part = spanned.then(|| {
            tracing::info_span!(target: "blaze", "loop turn: observe, watch and say").entered()
        });
        self.history_unopened();
        self.store_broke();
        self.observed();
        self.watch();
        self.doing();
        self.plot();
        self.look_unheld();
        self.look_for_solo_rows();
        drop(part);
        step.next("after the answers");
        let _between = spanned.then(|| {
            tracing::info_span!(target: "blaze", "loop turn: after the answers").entered()
        });
        self.let_go_of_image();
        let idle = self.effects.is_empty() && self.driver.in_flight() == 0;
        // An image someone waits for (a stop's, a snapshot's) is made whatever comes; the
        // settle's is not made while a request is read and held, nor soon after one.
        let makes = self.stopping.is_some() || !self.jobs.idle() || self.quiet_between_requests();
        // Each turn, image or not: a save whose puts became durable is marked in the journal at
        // once, so a crash after it opens that session ([`Jobs::carry_sessions`]).
        self.jobs.carry_sessions(&mut self.vfs, &self.store);
        if makes {
            let parts =
                Parts {
                vfs: &mut self.vfs,
                store: &mut self.store,
                predict: self.plane.predict(),
                effects: &mut self.effects,
            };
            let was = self.images.done();
            self.images.step(parts, idle);
            self.images_ended(was);
        }
        self.between();
        self.settle_turn();
        self.collect_turn();
        // A stalled job's leases end once its tasks let go of it.
        self.jobs.release_leases();
        // The launch host's start images put and marked, and its first calls built.
        self.launch_turn();
        let fetched = self.fetch_urls.take_answers();
        if !fetched.is_empty() {
            self.send(fetched);
        }
        // Lane packs-1112: the answers of `blaze store export|import|install` run here.
        let packed = self.packings.take_answers();
        if !packed.is_empty() {
            self.send(packed);
        }
        if self.stopping.is_none()
            && let Some(gc) = &mut self.gc
        {
            gc.turn(self.jobs.idle(), self.heads.checkouts());
            let mut answers = gc.take_answers();
            let released = gc.take_released();
            // `blaze gc` is answered once the store's lap it asked for has ended too, with
            // what that lap did after the build dirs' report.
            if let Some(collects) = &mut self.collects {
                self.gc_answers.append(&mut answers);
                let now = self.driver.now();
                if !self.gc_answers.is_empty() && !collects.asked_waits(now) {
                    let store = collects.report(&self.store, now);
                    answers = std::mem::take(&mut self.gc_answers);
                    for answer in &mut answers {
                        if let blaze_api::Response::Collected { report } = &mut answer.response {
                            report.push_str(&store);
                        }
                    }
                }
            }
            if !answers.is_empty() {
                self.send(answers);
            }
            if !released.is_empty() {
                self.release_held(released);
            }
        }
        if self.stopping.is_some()
            && let Some(gc) = &mut self.gc
        {
            let abandoned = gc.abandon();
            if !abandoned.is_empty() {
                self.send(abandoned);
            }
        }
        if self.stopping.is_some() && !self.held.is_empty() {
            // alloc: once a stop with builds held
            let stopped = self.held.iter().map(|(id, _)| (*id, None)).collect();
            self.release_held_as(stopped);
        }
        // What the turn's later parts said of a build ahead (its end, in the plane's step):
        // a request held behind it is handed on in this turn.
        self.ahead_turn();
        self.stepping_stop();
        self.submit();
        step.end();
        self.pulse.turned(self.completions + self.settled + self.job_effects);
        !matches!(self.stopping, Some(Stopping::Over))
    }

    /// The loop's pulse, for a thread that watches the loop from outside it (the stop's guard,
    /// `serve::stop`): its progress (effects that ended on their own, the plane's nodes settled
    /// and the jobs' effects ended, never its idle turns or a client's requests) and its turns.
    pub fn pulse(&self) -> Arc<Pulse> {
        Arc::clone(&self.pulse) // clone: a handle, once a daemon
    }

    /// The table of the plane's tasks the loop publishes for the explorer: what `blaze debug
    /// inspect`'s thread reads the queued and running tasks from, off the loop.
    #[cfg(feature = "native")]
    pub fn tasks(&self) -> Arc<blaze_explore::Tasks> {
        self.observe.tasks()
    }

    /// The loop's effects to the driver, those that end on their own counted in `ending`.
    fn submit(&mut self) {
        for effect in &self.effects {
            if ends_alone(effect) {
                // alloc: an entry per tag in flight, the map kept and reused
                *self.ending.entry(effect.tag()).or_default() += 1;
            }
        }
        self.driver.submit(&mut self.effects);
    }

    /// A completion ends an effect of its tag unless it is one of many a standing effect
    /// gives, as the drivers count it.
    fn ended(&mut self, completion: &Completion) {
        if matches!(
            completion.done,
            Done::Changed { .. }
                | Done::Journaled(_)
                | Done::Accepted { .. }
                | Done::Pressed { .. }
        ) {
            return;
        }
        if let Some(count) = self.ending.get_mut(&completion.tag) {
            self.completions += 1;
            *count -= 1;
            if *count == 0 {
                self.ending.remove(&completion.tag);
            }
        }
    }

    /// What [`Observe`] answers this turn sent, and the builds that ended given to the
    /// history.
    fn observed(&mut self) {
        let now = self.driver.now();
        let server = &self.server;
        let mut replies = std::mem::take(&mut self.replies);
        self.observe.tick(now, |conn| server.waiting(conn), &mut replies);
        for reply in replies.drain(..) {
            self.reply(&reply);
        }
        self.replies = replies;
        for row in self.jobs.started.drain(..) {
            self.observe.started(row);
        }
        for row in self.jobs.rows.drain(..) {
            self.observe.ended(row, Some(&*self.db));
        }
        // The errors builds said, before their answers are told below: a build's own flush,
        // as it is answered, writes them.
        if !self.jobs.errors.is_empty() {
            self.observe.errored(std::mem::take(&mut self.jobs.errors));
        }
        // After the rows of the same turn (a stalled build's row and its answer are one turn's),
        // and after this turn's answers went to the driver (`turn_after_wait` submits before
        // it calls this): a build's profile is flushed behind its answer, never beside it.
        for build in self.jobs.answered.drain(..) {
            self.observe.answered(build);
        }
        // The run logs of builds whose front end was kept, named after their answers.
        if !self.jobs.ran.is_empty() {
            self.observe.ran(std::mem::take(&mut self.jobs.ran));
        }
    }

    /// The stall watchdog's turn: once the plane's tasks run and none of its counts (the worker
    /// pool's jobs ended; queries completed; tasks and unit's steps ended; a worker running a
    /// job's code, a run of user code, or a byte arriving on a download in flight
    /// (`blaze_fetch::downloads`, whose own meter stops a download quiet for
    /// `blaze_fetch::progress::STALL`): work on what the build waits for, never the loop's
    /// turns or the workers' idle and search spans, which close on through a stall) has moved
    /// for the stall bound, every job with a plane request under way fails with the
    /// stall, its trace is written to the state directory, and the daemon stops. A step or a
    /// span that runs long while anything else moves is no stall. At half the bound, what
    /// every thread is inside is written to the log first, so a client's own watchdog that
    /// stops the daemon sooner still finds it; so it is too once a step runs [`LONG_STEP`].
    fn watch(&mut self) {
        self.still();
        if self.failed.is_some() {
            return;
        }
        let running = self.plane.running();
        // Work under way: a plane task, or a job a client waits on with no task running (one
        // left with nothing that can move it is a stall, never a hang that says nothing).
        let busy = running > 0 || self.jobs.under_way() > 0;
        let now = ns(self.driver.now());
        let pool = blaze_graph::exec::shared();
        // A byte that arrived since the last turn (about one; the loop turns at least every
        // 50 ms) is a download moving: the bound is measured from a download's last byte.
        let downloading = blaze_fetch::downloads::Downloads::process()
            .quiet_for()
            .is_some_and(|quiet| quiet < Duration::from_secs(1));
        if pool.working() > 0 || self.jobs.runs_live() || downloading {
            self.working += 1;
        }
        let progress = Progress {
            jobs: pool.ended(),
            queries: blaze_query::db::completed(),
            steps: self.settled + self.job_effects,
            working: self.working,
        };
        let steps = Progress { steps: self.settled, ..Progress::default() };
        if let Some(quiet) = self.step_watch.tick(now, steps, busy) {
            let secs = quiet.for_ns / 1_000_000_000;
            tracing::warn!(
                "no unit's step ended for {secs} s, {progress}: {}",
                blaze_profile::watchdog::running()
            );
        }
        if let Some(quiet) = self.dump_watch.tick(now, progress, busy) {
            let secs = quiet.for_ns / 1_000_000_000;
            let bound = self.watchdog.bound().as_secs();
            tracing::warn!(
                "{} for {secs} s of the stall bound's {bound} s, {progress}: {}; {}; {}; {}",
                blaze_profile::watchdog::STILL,
                blaze_profile::watchdog::running(),
                blaze_graph::sched::waits_now(),
                RunningTasks(&self.plane),
                BuildsWaited(self.session.building(), self.jobs.waited_on())
            );
        }
        let Some(quiet) = self.watchdog.tick(now, progress, busy) else { return };
        let stall = blaze_profile::watchdog::stall(quiet, running);
        // alloc: a stall, once
        let said = format!(
            "{stall}; {}; {}; {}{}",
            blaze_graph::sched::waits_now(),
            RunningTasks(&self.plane),
            BuildsWaited(self.session.building(), self.jobs.waited_on()),
            DownloadsNow(&blaze_fetch::downloads::Downloads::process().shown())
        );
        tracing::error!("{said}");
        // What every thread was inside and the recorder's last minutes, where the client that
        // this stall hangs up on finds it (`flightrec`): written here, by the loop, which is
        // about to fail every request and stop.
        _ = super::flightrec::record(
            super::flightrec::Why::Stall,
            &said,
            super::flightrec::WINDOW,
        );
        let file = under(&self.state, &format!("stalls/{now}.json"));
        // durable: a stall's trace is read after the daemon is killed for the stall, and nothing can make it again
        self.effects.push(Effect::Write { file, bytes: stall.trace, tag: STALL });
        // clone: the error, once a stall
        let error = ApiError::new(said.clone(), Some("stall".to_owned()));
        let mut told = self.spare.said.pop().unwrap_or_default();
        self.jobs.stalled(&mut world!(self, told), &error);
        self.said(told);
        self.fail(Failed::Stalled(said));
    }

    /// The loop's stall: a request has nodes not settled and nothing can move them (the loop has
    /// nothing ready, pending or in flight, no IO task or expansion thread runs, and the worker
    /// pool runs and queues nothing, while any task posted to it either has no job there, lost,
    /// or waits on what no wait of the graph will end, or a job's write has waited past
    /// [`super::still::WRITE_STILL_FOR`] on readers: [`Stillness`]). Held for [`STILL_FOR`], every job with a plane request under way fails with [`Still`], naming
    /// what each node waits on and every wait of the graph, and the daemon stops, as on a stall
    /// the watchdog finds.
    fn still(&mut self) {
        let now = ns(self.driver.now());
        let pool = blaze_graph::exec::shared();
        // The pool first: a task's job sends its end before its worker sleeps, so a pool seen
        // idle has sent every end it will, and the loop sees it ready below.
        let (pool_idle, parked) = (pool.idle(), pool.parked());
        let on_pool = self.plane.running_on_pool();
        let expansion = blaze_macro_eval::expansion_counts();
        let facts = Facts {
            loop_moves: self.plane.ready()
                || self.effects.iter().any(ends_alone)
                || !self.ending.is_empty(),
            write_waited_ns: self
                .jobs
                .write_waited(self.driver.now())
                .map(|it| u64::try_from(it.as_nanos()).unwrap_or(u64::MAX)),
            // A lock held as the loop looks is an expansion thread at work. A thread asking
            // waits on its job, which is the pool's, so it is no movement of its own.
            expanding: expansion.map_or(1, |it| {
                it.alive.saturating_sub(it.idle).saturating_sub(it.asking) + it.queued
            }),
            off_pool: self.plane.running() - on_pool,
            on_pool,
            pool_idle,
            parked,
        };
        let plane = &self.plane;
        let open = || {
            let mut open = false;
            plane.unsettled(|_| open = true);
            open
        };
        let (quiet_ns, why) =
            match self.stillness.tick(now, &facts, blaze_graph::sched::stuck_while_quiet, open) {
                Turn::Moves | Turn::Holds => return,
                Turn::Began { after } => {
                    self.effects.push(Effect::Timer { after, tag: STILL_TIMER });
                    return;
                }
                Turn::Still { quiet_ns, why } => (quiet_ns, why),
            };
        // alloc: a stall, once
        let mut waits = Vec::new();
        let jobs = &self.jobs;
        plane.unsettled(|it| {
            let held = jobs.holding(it.id);
            waits.push(StillWait {
                request: it.request.0,
                node: it.id.0,
                name: it.name.to_owned(), // alloc: a stall, once per node it names
                state: it.state.clone(),
                on: it.on.map(|on| on.0),
                held,
                run: (held.is_none() && plane.loop_runs(it.id)).then(|| jobs.run_wait(it.id)),
            });
        });
        if waits.is_empty() {
            self.stillness.reset();
            return;
        }
        waits.sort_by_key(|it| (it.request, it.node));
        // The pool's parked jobs fail with the stall, each naming every wait; their tasks then
        // end. Any other stretch names the graph's waits as they are.
        let graph = match why {
            Why::Stuck { .. } => blaze_graph::sched::stall_while_quiet(),
            // A write's readers wait on what the graph cannot call stuck: named as they are,
            // each parked job by what it waits on and who claimed it.
            Why::NoTask | Why::Lost { .. } | Why::WriteWaits { .. } => None,
        };
        let still = Still {
            quiet_ns,
            why,
            loops: self.plane.loops_running(),
            parked,
            graph: graph.unwrap_or_else(blaze_graph::sched::waits_now),
            waits,
            lowerings: self.jobs.held_lowerings(),
            writes: self.jobs.write_waits(self.driver.now()),
            expansion,
        };
        let said = still.to_string();
        tracing::error!("{said}");
        // clone: the error, once a stall
        let error = ApiError::new(said.clone(), Some("stall".to_owned()));
        let mut told = self.spare.said.pop().unwrap_or_default();
        self.jobs.stalled(&mut world!(self, told), &error);
        self.said(told);
        self.fail(Failed::Stalled(said));
    }

    /// What the system says of memory, on [`PRESSURE`]. Admission does not move for it: it is
    /// by cores alone, and the kernel's limit is the one ceiling. What can be given is: the
    /// heaps' free pages (once a [`PURGE_EVERY`]). Deleted 2026-11-13: the memory gate, the
    /// machine's ledger and every admission rule by memory.
    /// A system with no source the daemon can watch says so once, in the log, which is no
    /// error.
    fn pressure(&mut self, completion: Completion) {
        match completion.done {
            Done::Pressed { critical } => {
                let now = self.driver.now();
                let _span =
                    tracing::info_span!(target: "blaze", "memory pressure", critical).entered();
                if now.saturating_sub(self.given_back) >= PURGE_EVERY {
                    self.given_back = now;
                    crate::raw::memory::purger().ask();
                }
            }
            Done::Unwatched => self.pressure_watched = false,
            Done::Failed(IoError(why)) => {
                self.pressure_watched = false;
                crate::serve::said::logged!(WARN,
                    "blaze serve: no memory pressure source is watched, so the heaps' free pages \
                     go back only when the daemon is idle: {why}"
                );
            }
            other => {
                self.pressure_watched = false;
                tracing::warn!(target: "blaze", "the watch of memory pressure ended as no watch does: {other:?}");
            }
        }
    }

    /// The task watchdog's word since the last look: each stuck task kept for the live models,
    /// and the plane node of each lost thread failed with its line ([`Plane::abandon`]), so its
    /// build ends with that error and the daemon serves the next. Never waits.
    fn take_stuck(&mut self, now: Duration) {
        let (plane, stuck) = (&mut self.plane, &mut self.stuck);
        self.pulse.take_stuck(|said| {
            if said.lost
                && let Some(node) = said.node.and_then(|it| u32::try_from(it).ok())
            {
                // alloc: a stuck task, once
                let why = format!(
                    "blaze: {}; nothing can unwind its thread, so its task fails and the thread is lost to the daemon",
                    said.said
                );
                plane.abandon(now, blaze_graph::machine::NodeId(node), why);
            }
            if stuck.len() >= STUCK_SHOWN {
                stuck.remove(0);
            }
            stuck.push(said); // alloc: grows to STUCK_SHOWN once
        });
    }

    /// What each build is doing, looked at once each [`doing::EVERY`] of the driver's clock
    /// while a job runs ([`Jobs::doing`]): said to its client as `Response::Doing`, and kept in
    /// `builds` for `blaze status`. Reads the plane's running nodes, the recorder's open spans
    /// and the downloads in flight once for every build, and feeds each build's live model
    /// (the plane's folds) what it found; blocks on nothing.
    fn doing(&mut self) {
        let now = self.driver.now();
        if now.saturating_sub(self.looked) < doing::EVERY {
            return;
        }
        self.looked = now;
        self.builds.clear();
        self.take_stuck(now);
        // What `blaze debug inspect` says of each job's waiting write, once a look.
        self.pulse.set_writes(self.jobs.write_waits(now));
        if self.jobs.under_way() == 0 {
            self.stuck.clear();
            return;
        }
        #[cfg(feature = "native")]
        self.observe.clock_tasks(now);
        // alloc: a look, once a second while a build runs
        let mut running = Vec::new();
        let plane = &self.plane;
        plane.unsettled(|it| {
            if matches!(it.state, blaze_protocol::events::NodeState::Running)
                && !plane.loop_runs(it.id)
            {
                running.push((it.request, it.id));
            }
        });
        let threads = blaze_profile::flight::threads_now();
        let downloads = blaze_fetch::downloads::Downloads::process().shown();
        let fetching = doing::fetching(&downloads);
        let memory = doing::memory();
        let pool = blaze_graph::exec::shared();
        let working = pool.working();
        self.pulse.take_stall(&mut self.stall);
        let stall = self.stall.as_deref();
        let look = doing::Look {
            now,
            running: &running,
            threads: &threads,
            downloads: &downloads,
            busy: u32::try_from(working).unwrap_or(u32::MAX),
            idle: u32::try_from(pool.workers().saturating_sub(working)).unwrap_or(u32::MAX),
            workers: u32::try_from(pool.workers()).unwrap_or(u32::MAX),
            fetching: &fetching,
            memory: &memory,
            stall,
            stuck: &self.stuck,
        };
        let mut said = self.spare.said.pop().unwrap_or_default();
        let mut builds = std::mem::take(&mut self.builds);
        self.jobs.doing(&look, self.plane.folds(), &mut said, &mut builds);
        self.builds = builds;
        self.said(said);
    }

    /// A look for the solo answers' kept rows (`<state>/solo-rows`, `blaze_certify::kept`)
    /// begun: at the first turn, which is the start's, and then each [`SOLO_LOOK_EVERY`] while
    /// the daemon has nothing to do; one look at a time. A daemon that keeps no history leaves
    /// them for one that does ([`Observe::takes_solo_rows`]).
    fn look_for_solo_rows(&mut self) {
        let now = self.driver.now();
        let working = self.jobs.build_under_way() != 0 || self.plane.running() != 0;
        let due = match self.solo_looked {
            None => true,
            Some(at) => !working && now.saturating_sub(at) >= SOLO_LOOK_EVERY,
        };
        if !due || self.solo.looking() || self.stopping.is_some() || !self.observe.takes_solo_rows() {
            return;
        }
        self.solo_looked = Some(now);
        self.solo.look(under(&self.state, blaze_certify::kept::DIR), &mut self.effects);
    }

    /// A step of the look for solo answers' kept rows done; once the look ends, each record it
    /// took (read, then removed, so none is taken twice) is numbered as a build of its own and
    /// given to the history ([`Observe::solo_rows`]), and each file it refused is said.
    fn solo_taken(&mut self, completion: Completion) {
        let wall_ns = self.driver.wall_ns();
        let Some(taken) =
            self.solo.complete(completion, wall_ns, crate::raw::process::alive, &mut self.effects)
        else {
            return;
        };
        let blaze_certify::kept::Taken { kept, refused, more: _, failed } = taken;
        // `more`: the rest are the next look's, a second later.
        for why in &refused {
            tracing::warn!(target: "blaze", why = %why, "a solo answer's kept rows are not a record");
        }
        if let Some(why) = failed {
            tracing::warn!(target: "blaze", why = %why, "the solo answers' kept rows could not be taken");
        }
        for record in kept {
            // The planted bug of the take's control: each record's file written back once it
            // was taken, as a take that leaves the file would.
            #[cfg(test)]
            if LEAVES_TAKEN_SOLO_ROWS.get() {
                let (whole, _) = blaze_certify::kept::names(record.pid);
                let file = under(&self.state, &format!("{}/{whole}", blaze_certify::kept::DIR));
                let bytes = record.write().expect("a record that was read writes");
                self.effects.push(Effect::Write { file, bytes, tag: Tag(SOLO_ROWS_END - 1) });
            }
            let build = u64::from(self.plane.number_unplanned());
            self.observe.solo_rows(build, record);
        }
    }

    /// The plane's and the memory's plots, at most once each [`PLOT_EVERY`] of the driver's
    /// clock, and the memory's breakdown by area once each [`super::breakdown::EVERY`]: each
    /// row of the build under way ([`Jobs::build_under_way`]), 0 while none is.
    fn plot(&mut self) {
        let now = self.driver.now();
        if now.saturating_sub(self.plotted) < PLOT_EVERY {
            return;
        }
        self.plotted = now;
        let _build = blaze_profile::flight::counting_for(self.jobs.build_under_way());
        blaze_profile::plot(Plot::Running, self.plane.running() as u64);
        let memory = blaze_mem::tag::snapshot();
        blaze_profile::plot(Plot::FrameBytes, memory.get(blaze_mem::Tag::Frames).live);
        blaze_profile::plot(Plot::LiveBytes, memory.live());
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(bytes) = crate::raw::memory::own_footprint() {
            blaze_profile::plot(Plot::Footprint, bytes);
            let kept = blaze_graph::arena::kept_of(&memory, bytes);
            blaze_profile::plot(Plot::AllocatorKeptBytes, kept);
        }
        // Nothing to do for a while: the warm memory each thread keeps for its next task goes
        // back, once a stretch, on the purger's thread and the workers' own as they park
        // ([`super::idle`]).
        let working = self.jobs.build_under_way() != 0 || self.plane.running() != 0;
        if self.idle.look(now, working) {
            tracing::info!(target: "blaze", "idle: the daemon gives its warm memory back");
            blaze_mem::frame::ask_spare_trim();
            crate::raw::memory::purger().ask();
            self.jobs.say_what_the_kept_front_ends_hold();
            // The stores' pools (kept for the stores' own commits and the reads not yet on
            // `direct`, `super::direct::pool`) give back what nothing read again since the
            // sweep last came by, one lap, on the loop, which alone owns the stores. A task's
            // direct read keeps nothing past its task: the kernel's page cache holds those.
            let freed = self.store.trim_pool() + self.std.trim_pool();
            if freed > 0 {
                tracing::info!(target: "blaze", freed, "idle: the stores' pools gave back what nothing read again");
            }
        }
        // Longest: a daemon in no edit loop exits, once nothing it holds is undurable and no
        // client or launcher is connected; a run with nothing changed is answered by its
        // client alone.
        let working = working || !self.jobs.idle() || self.jobs.storing() > 0;
        let idle_for = match working {
            true => {
                self.idle_since = None;
                Duration::ZERO
            }
            false => now.saturating_sub(*self.idle_since.get_or_insert(now)),
        };
        // What an answered build left counts until it settles: its puts wait on the store's
        // commit, which may be slow but moving well past the exit's wait, and a stop gives up
        // what has not settled within its own bound (`STOP_SETTLES_WITHIN`).
        let undurable = self.jobs.storing() > 0
            || self.jobs.settling() > 0
            || self.launch.undurable()
            || self.jobs.slots_writing();
        if self.stopping.is_none()
            && super::idle::exit_due(
                idle_for,
                self.jobs.exit_wait(self.worked),
                self.server.connections() + self.steps.launchers_connected(),
                undurable,
            )
        {
            tracing::info!(target: "blaze", "the daemon exits: no edit loop is live");
            self.stop();
        }
        blaze_profile::plot(Plot::AllocatorPurgedBytes, crate::raw::memory::purged_bytes());
        blaze_profile::plot(Plot::CommittedBytes, memory.committed);
        blaze_profile::plot(Plot::ReservedBytes, memory.reserved);
        let (span_map_bytes, span_map_pieces) = ::span::kept_maps();
        blaze_profile::plot(Plot::SpanMapBytes, span_map_bytes);
        blaze_profile::plot(Plot::SpanMapPieces, span_map_pieces);
        if now.saturating_sub(self.broken_down) >= super::breakdown::EVERY {
            self.broken_down = now;
            super::breakdown::plot(&memory, blaze_graph::arena::held(), &mut self.shown);
        }
        if now.saturating_sub(self.memory_logged) >= MEMORY_LOG_EVERY {
            self.memory_logged = now;
            log_memory(&memory);
        }
    }

    /// The session told the time before anything else it is told this turn; intent whose
    /// time is up is cancelled, and a diagnostics request whose wait is up is answered.
    fn tick(&mut self) {
        let now = ns(self.driver.now());
        let due = self.session.deadline().is_some_and(|at| at <= now);
        let mut replies = std::mem::take(&mut self.replies);
        self.session.tick(&mut cx!(self), now, &mut replies);
        self.replies = replies;
        if due {
            self.after_session();
        }
    }

    /// The daemon's own store broke, this turn or before: in its completion's dispatch, or in
    /// job code that put into it or began a commit (a put it could not keep, a commit it
    /// could not start). Checked once a turn, after the plane stepped, wherever it broke: each
    /// job waiting on the store fails naming why, then the daemon stops, as it does for the
    /// shared store, so the next request starts a fresh daemon.
    ///
    /// Every live job is failed with the store's reason, not only those at their gate, and
    /// that whether or not the daemon already failed for another reason: a build still running
    /// as the daemon stops is answered naming the store, not hung up on.
    ///
    /// What answered jobs left to settle is given up then too, each as a loss naming the break:
    /// their answers were sent and stand (`Jobs::store_broke`).
    /// The history did not open, as the plane's first open found ([`Plane::unnumbered`]): no
    /// build can be numbered past the builds it names, so the daemon stops, naming it.
    fn history_unopened(&mut self) {
        if let Some(unopened) = self.plane.unnumbered() {
            self.fail(Failed::History(unopened));
        }
    }

    fn store_broke(&mut self) {
        let (tell_jobs, fail) = (!self.jobs.quiet(), self.failed.is_none());
        if !(tell_jobs || fail) {
            return;
        }
        let Some(broken) = self.store.broken() else { return };
        // alloc: the error, only in a turn of the break with a job or the daemon left to tell
        let why = format!("the daemon's store: {broken}");
        if tell_jobs {
            let error = ApiError::new(why, None);
            let mut said = self.spare.said.pop().unwrap_or_default();
            self.jobs.store_broke(&mut world!(self, said), &error);
            self.said(said);
            if fail {
                self.fail(Failed::Store(error.what));
            }
        } else {
            self.fail(Failed::Store(why));
        }
    }

    /// Jobs whose wait for the store is past its deadline fail. Run after the turn's
    /// completions are dispatched, so a commit step that answered this turn counts before
    /// the deadline is judged.
    fn expire_jobs(&mut self) {
        if self.jobs.next_deadline().is_some_and(|at| at <= self.driver.now()) {
            let mut said = self.spare.said.pop().unwrap_or_default();
            self.jobs.expire(&mut world!(self, said));
            self.said(said);
        }
    }

    /// A completion to its part, by the block of tags it is in: compares on the tag alone,
    /// no part asked and no lock taken.
    fn dispatch(&mut self, completion: Completion) {
        match completion.tag.0 {
            _ if self.images.owns(completion.tag) => {
                let was = self.images.done();
                self.images.complete(&mut self.vfs, completion, &mut self.effects);
                self.images_ended(was);
            }
            _ if completion.tag == PRESSURE => self.pressure(completion),
            SERVER..VFS => {
                let mut arrived = self.spare.arrived.pop().unwrap_or_default();
                self.server.complete(completion, &mut self.effects, &mut arrived);
                for it in arrived.drain(..) {
                    self.arrive(it);
                }
                self.spare.arrived.push(arrived);
                if let Some(why) = self.server.failed()
                    && self.failed.is_none()
                {
                    let why = Failed::Server(why.to_owned());
                    self.fail(why);
                }
            }
            VFS..DB => {
                let tickets = self.vfs.complete(completion, &mut self.effects);
                if !tickets.is_empty() {
                    self.durable(&tickets);
                }
            }
            DB..STORE => self.db.complete(completion, &mut self.effects),
            PLACES..PLACES_END => self.start_placed(completion),
            SOLO_ROWS..SOLO_ROWS_END => self.solo_taken(completion),
            // Lane packs-1112: a peer's request, answered from the store.
            PEERS..PEERS_END => {
                if let Some(peers) = &mut self.peers {
                    peers.complete(completion, &self.store, &mut self.effects);
                    if let Some(why) = peers.take_failed() {
                        tracing::error!(target: "blaze", "{why}");
                    }
                }
            }
            // The store's own effects, and its pool's loads (a commit loading the page a pack's
            // index is on, or one of the reads not yet on `direct`).
            STORE..FLUSHES | LOADS..STD_LOADS => {
                debug_assert!(self.store.owns(completion.tag), "the store's tags are its");
                self.job_effects += 1;
                self.store.complete(completion, &mut self.effects);
                // A step of its commit answered: a wait for it to make puts durable runs out
                // only when the commit stands still, never because placements or reads come.
                self.jobs.commit_moved(self.driver.now(), self.store.commit_steps());
                let mut placed = self.store.placed();
                // The boundary where the store's errors become messages for a person
                // ([`super::rooted`]): the names in them, relative to the driver's root as a
                // core prints them, are written as paths under it.
                for it in &mut placed {
                    if let Err(why) = &mut it.result {
                        let out = super::rooted::dir_of(&it.to);
                        let dirs: Vec<&FileName> =
                            std::iter::once(self.store.dir()).chain(out.as_ref()).collect(); // alloc: an error path
                        *why = super::rooted::rooted(why, std::path::Path::new("/"), &dirs);
                    }
                }
                let mut said = self.spare.said.pop().unwrap_or_default();
                self.jobs.placed(&mut world!(self, said), placed);
                // A look of its layers may have come in: each build that waited for it begins.
                if self.jobs.awaits_look() {
                    self.jobs.looked(&mut world!(self, said));
                }
                self.said(said);
            }
            STD_STORE..STD_STORE_END | STD_LOADS..STD_LOADS_END => {
                debug_assert!(self.std.owns(completion.tag), "the shared store's tags are its");
                self.std.complete(completion, &mut self.effects);
                if let Some(broken) = self.std.broken()
                    && self.failed.is_none()
                {
                    let why =
                        Failed::Store(format!("the standard library's shared store: {broken}"));
                    self.fail(why);
                    return;
                }
                let mut said = self.spare.said.pop().unwrap_or_default();
                self.jobs.looked(&mut world!(self, said));
                self.said(said);
            }
            FLUSHES..JOBS | OUTSIDE..OUTSIDE_END | WATCHERS..RUNS => {
                debug_assert!(self.heads.owns(completion.tag), "the checkouts' tags are theirs");
                let mut out = self.spare.out.pop().unwrap_or_default();
                self.heads.complete(&mut self.vfs, self.driver.wall_ns(), completion, &mut self.effects, &mut out);
                self.outs(out);
            }
            JOBS..JOBS_END | RUNS..RUNS_END => {
                debug_assert!(self.jobs.owns(completion.tag), "the jobs' tags are theirs");
                self.job_effects += 1;
                let mut said = self.spare.said.pop().unwrap_or_default();
                self.jobs.complete(&mut world!(self, said), completion);
                self.said(said);
            }
            // A task's store read on the lent driver (`direct::lend`) that ended after its
            // read gave up waiting: the read failed then (`direct::Failed::Stalled`), naming
            // the record, and gave its buffer up to the kernel; nothing waits for this.
            super::direct::HOME_TAGS.. => {
                tracing::info!(target: "blaze", tag = completion.tag.0, "a store read's late completion, its read already failed as stalled");
            }
            _ => {}
        }
    }

    /// `reply` handed to the server to send. A request's `Done` ends its `request: read to
    /// reply` span ([`Daemon::arrive`]). An answer path: nothing under it syncs a file
    /// (`blaze::sync_on_answer_path`).
    #[blaze::answers_request]
    fn reply(&mut self, reply: &Reply) {
        // A reply to the daemon's own request is the core's and no client's: what a build
        // ahead says is shown to nobody ([`super::ahead`]).
        if reply.to.conn == super::ahead::CONN {
            self.ahead_heard(reply);
            return;
        }
        // A reply to a run the daemon sent itself for a launched program's first call: the
        // launch host's, and no client's.
        if reply.to.conn == launch::CONN {
            self.launch_heard(reply);
            return;
        }
        self.launch_replied(reply);
        self.server.reply(reply, &mut self.effects);
        if matches!(reply.response, blaze_api::Response::Done { .. })
            && let Some(at) = self.answering.iter().position(|(to, _)| *to == reply.to)
        {
            self.answering.swap_remove(at);
        }
    }

    fn arrive(&mut self, arrived: Arrived) {
        // A `blaze run` is kept to its `Done`: what a launched program's first call is built by.
        self.launch_arrived(&arrived);
        // From a request read off its connection to its `Done` handed to the server, which
        // the turn submits before anything else it does: what a client waits for of the
        // daemon, whatever held the request between (a watcher's flush, a build).
        match &arrived {
            Arrived::Request { from, request } => {
                // A status asked again and again, a stop and a client's rows are no work of a
                // checkout: they hold nothing between requests back.
                if !matches!(
                    request,
                    Request::Stop | Request::Status | Request::ClientRows { .. }
                ) {
                    self.quiet_turns = 0;
                }
                let span = tracing::info_span!(
                    target: "blaze",
                    "request: read to reply",
                    request = from.id,
                );
                self.answering.push((*from, span)); // alloc: an entry a request under way
            }
            Arrived::Gone { conn } => self.answering.retain(|(to, _)| to.conn != *conn),
        }
        // A build ahead gives way as the request is read, beside the request's wait for its
        // watcher; a cancel of a request held behind one is the core's to answer.
        if self.ahead.read(&arrived) {
            self.ahead_do();
            return;
        }
        let mut out = self.spare.out.pop().unwrap_or_default();
        self.heads.arrive(&self.vfs, self.driver.wall_ns(), arrived, &mut self.effects, &mut out);
        self.outs(out);
        self.ahead_do();
    }

    fn outs(&mut self, mut out: Vec<Out>) {
        for it in out.drain(..) {
            match &it {
                Out::Pass(Arrived::Request {
                    request: Request::Stop | Request::Status | Request::ClientRows { .. },
                    ..
                }) => {}
                Out::Pass(Arrived::Request { .. }) | Out::Moved { .. } => {
                    self.changed_since_settle = true
                }
                _ => {}
            }
            match it {
                Out::Pass(Arrived::Request { from, request: Request::Explore { dev } }) => {
                    let mut replies = std::mem::take(&mut self.replies);
                    self.observe.explore(from, dev, &mut replies);
                    self.send(replies);
                }
                Out::Pass(Arrived::Request { from, request: Request::Live }) => {
                    self.observe.subscribe(from, self.driver.now())
                }
                // `blaze cache gc` while this daemon holds the areas: its collector runs it
                // (review N14), answered as the run records ([`super::gc::Gc::ask`]).
                Out::Pass(Arrived::Request { from, request: Request::Gc { dry_run, unkept } }) => {
                    match &mut self.gc {
                        Some(gc) if self.stopping.is_none() => {
                            gc.ask(from, dry_run);
                            if let Some(collects) = &mut self.collects {
                                collects.ask(self.driver.now(), dry_run, unkept);
                            }
                        }
                        _ => {
                            let what = match self.stopping {
                                Some(_) => "the daemon is stopping: its collector runs no more",
                                None => "this daemon runs no collector (only `blaze serve`'s does)",
                            };
                            let error = ApiError::new(what, Some("blaze cache gc".to_owned())); // alloc: error path
                            self.server.reply(
                                &Reply { to: from, response: blaze_api::Response::Error(error) },
                                &mut self.effects,
                            );
                            self.server.reply(
                                &Reply {
                                    to: from,
                                    response: blaze_api::Response::Done { ok: false },
                                },
                                &mut self.effects,
                            );
                        }
                    }
                }
                // `blaze run` of a dev executable: the programs are the steps' (review: the
                // file it may write is the launch's, once per run).
                Out::Pass(Arrived::Request { from, request: Request::Launch { program, pid } }) => {
                    let placed = self.jobs.placed_key(&program);
                    let (response, ok) = match self.steps.launch(&program, pid, placed) {
                        Ok(Some(env)) => (blaze_api::Response::Launched { live: true, env }, true),
                        // alloc: none, empty
                        Ok(None) => {
                            (blaze_api::Response::Launched { live: false, env: Vec::new() }, true)
                        }
                        Err(error) => (blaze_api::Response::Error(error), false),
                    };
                    self.reply(&Reply { to: from, response });
                    self.reply(&Reply { to: from, response: blaze_api::Response::Done { ok } });
                }
                // Lane packs-1112: `blaze store export|import|install` while this daemon holds
                // the store: run on a thread of its own over the store's handle and this
                // loop ([`super::packs::daemon::Packings`]), answered as it ends.
                Out::Pass(Arrived::Request { from, request: Request::Store { words } }) => {
                    self.packings.ask(from, words);
                }
                // `blaze fetch-url`: fetched on a thread of its own, answered as it ends
                // ([`super::fetch_url::FetchUrls::take_answers`]).
                Out::Pass(Arrived::Request {
                    from,
                    request: Request::FetchUrl { url, repo, rev, digest, most_bytes, out },
                }) => {
                    let asked = super::fetch_url::Asked { url, repo, rev, digest, most_bytes, out };
                    self.fetch_urls.ask(from, asked);
                }
                Out::Pass(Arrived::Request { from, request: Request::Flush }) => {
                    let mut replies = std::mem::take(&mut self.replies);
                    self.observe.flush(from, self.driver.now(), &mut replies);
                    self.send(replies);
                }
                Out::Pass(Arrived::Request {
                    from,
                    request:
                        Request::ClientRows {
                            pid,
                            build,
                            spans,
                            timed,
                            events,
                            folded,
                            ended_ns,
                            started_ns,
                        },
                }) => {
                    #[cfg(test)]
                    let timed = match DROPS_A_CLIENT_SPAN.get() {
                        true => timed.into_iter().filter(|it| it.name != blaze_thin::talk::ON_THE_WIRE).collect(),
                        false => timed,
                    };
                    // The client became its program's process (it exec'd it): the launch host
                    // joins that instant to the launcher's `Hello` and exit for the
                    // program's own spans.
                    if let Some(exec_ns) = blaze_api::Timed::exec_of(&timed) {
                        self.steps.launch_execd(pid, build, exec_ns);
                    }
                    // A client that times its spans by name (the thin `blaze`) has them made
                    // its process's rows here; one that sent both ways is refused by name.
                    let spans = match crate::cli::client_rows::spans_of(build, pid, spans, &timed) {
                        Ok(spans) => spans,
                        Err(why) => {
                            let error = blaze_api::ApiError::new(why, None);
                            self.reply(&Reply { to: from, response: blaze_api::Response::Error(error) });
                            self.reply(&Reply { to: from, response: blaze_api::Response::Done { ok: false } });
                            continue;
                        }
                    };
                    let rows = blaze_profile::history::ClientRows {
                        build,
                        times: Some((started_ns, ended_ns)),
                        spans,
                        events,
                        folded,
                    };
                    let mut replies = std::mem::take(&mut self.replies);
                    self.observe.client_rows(from, rows, &mut replies);
                    self.send(replies);
                }
                Out::Pass(Arrived::Request {
                    from,
                    request: Request::Rebuilt { all, cwd, item, build, unexplained },
                }) => {
                    let mut replies = std::mem::take(&mut self.replies);
                    let ask = crate::serve::why::Ask { all, cwd, item, build, unexplained };
                    self.observe.why_asked(from, ask, self.driver.now(), &mut replies);
                    self.send(replies);
                }
                Out::Pass(arrived) => {
                    if let Arrived::Gone { conn } = &arrived {
                        self.observe.gone(*conn);
                    }
                    if let Arrived::Request { from, request: Request::Stop } = &arrived {
                        self.stop_asked = true;
                        self.stop_from.get_or_insert(*from);
                    }
                    // A request for what is being built ahead waits for that build, its
                    // own, and is handed on as it ends ([`Daemon::ahead_do`]).
                    if self.ahead_holds(&arrived) {
                        continue;
                    }
                    let mut replies = std::mem::take(&mut self.replies);
                    self.session.arrive(&mut cx!(self), arrived, &mut replies);
                    self.replies = replies;
                    self.after_session();
                }
                Out::Answer(reply) => self.reply(&reply),
                Out::Moved { head, root } => {
                    self.ahead_moved(&head, root);
                    self.session.head_moved(&mut cx!(self), &head, root);
                    self.after_session();
                }
            }
        }
        self.spare.out.push(out);
        if self.stop_asked {
            self.stop();
        }
    }

    /// `replies` sent, and given back as the loop's buffer.
    fn send(&mut self, mut replies: Vec<Reply>) {
        for reply in replies.drain(..) {
            self.reply(&reply);
        }
        self.replies = replies;
    }

    fn durable(&mut self, tickets: &[Ticket]) {
        let mut replies = std::mem::take(&mut self.replies);
        self.session.durable(&mut cx!(self), tickets, &mut replies);
        self.replies = replies;
        let mut said = self.spare.said.pop().unwrap_or_default();
        self.jobs.durable(&mut world!(self, said), tickets);
        self.said(said);
        self.after_session();
    }

    /// Every image asked was made or failed just now (it was not before, `was`): the jobs
    /// wound up, so a snapshot waiting for it answers. Nothing else need move then: the
    /// image is often the last of what a snapshot waits for (its journal ticket and the
    /// store's puts durable before it), and no ticket or commit step follows it to wake the
    /// snapshot.
    fn images_ended(&mut self, was: bool) {
        if !was && self.images.done() {
            self.after_session();
        }
    }

    /// After a session call: the VFS flushed, what the session asked of the build started,
    /// and the replies sent.
    fn after_session(&mut self) {
        self.vfs.flush(&mut self.effects);
        let mut said = self.spare.said.pop().unwrap_or_default();
        self.jobs.step(&mut world!(self, said));
        self.said(said);
        let mut replies = std::mem::take(&mut self.replies);
        for reply in replies.drain(..) {
            self.reply(&reply);
        }
        self.replies = replies;
    }

    /// What the jobs said, told to the session; a build that failed for want of space held
    /// first ([`Daemon::hold_for_space`]).
    fn said(&mut self, said: Vec<Said>) {
        self.said_as(said, true);
    }

    /// What the jobs said, told to the session, a finish out of space held when `hold`.
    fn said_as(&mut self, mut said: Vec<Said>, hold: bool) {
        if said.is_empty() {
            self.spare.said.push(said);
            return;
        }
        let mut replies = std::mem::take(&mut self.replies);
        for it in said.drain(..) {
            let it = if hold {
                match self.hold_for_space(it) {
                    Some(it) => it,
                    None => continue,
                }
            } else {
                it
            };
            // A build that computed something again says why in its `Work` when the history's
            // thread has explained it by now ([`Observe::summary_of`]): one round trip for
            // its client, and no wait here when it has not.
            let it = match it {
                Said::Report(job, Response::Work(mut worked)) => {
                    self.worked |= worked.misses > 0;
                    if let (Some(build), true) = (worked.build_id, worked.misses > 0) {
                        let _span = tracing::info_span!(
                            target: "blaze",
                            "loop: why the build recomputed, for its answer",
                            build,
                            explained = tracing::field::Empty,
                        )
                        .entered();
                        worked.why = self.observe.summary_of(build);
                        _span.record("explained", worked.why.is_some());
                    }
                    // What the store and the home hold against their budget, on every
                    // build's end; a line with it only when the policy says so.
                    if let Some(collects) = &mut self.collects {
                        worked.storage = Some(collects.storage(&self.store, self.driver.now()));
                    }
                    Said::Report(job, Response::Work(worked))
                }
                it => it,
            };
            // Every word of a job passes here once, as it is told: a build that may be answered
            // again later has what it says noted ([`Jobs::heard`]).
            self.jobs.heard(&it);
            match it {
                Said::Report(job, response) => {
                    self.session.report(&mut cx!(self), job, response, &mut replies)
                }
                Said::Finish(job, ok, error, queries) => {
                    self.session.finish_in(&mut cx!(self), job, ok, error, queries, &mut replies);
                }
            }
        }
        self.spare.said.push(said);
        self.replies = replies;
        self.after_session();
    }

    /// A job that failed for want of space ([`blaze_gc::space::is_no_space`]) held while the
    /// daemon collects, when what is full is a volume the collector collects
    /// ([`super::gc::Gc::hold_for`], review N7): the collector runs urgently, and the finish is
    /// told once that run freed the space and was measured, or the hold's bound passed
    /// ([`Daemon::release_held`]). `None` when held.
    fn hold_for_space(&mut self, it: Said) -> Option<Said> {
        if self.stopping.is_some() {
            return Some(it);
        }
        let at = match &it {
            Said::Finish(_, false, Some(error), _) if blaze_gc::space::is_no_space(&error.what) => {
                error.at.as_deref()
            }
            _ => return Some(it),
        };
        let Some(gc) = &mut self.gc else { return Some(it) };
        let Some(id) = gc.hold_for(at) else { return Some(it) };
        self.held.push((id, it));
        None
    }

    /// The held finishes the collector let go told, each error saying how its hold ended.
    fn release_held(&mut self, released: Vec<(u64, blaze_gc::plan::Released)>) {
        // alloc: once a release, a held build or a few
        let released = released.into_iter().map(|(id, why)| (id, Some(why))).collect();
        self.release_held_as(released);
    }

    /// The held finishes of `released` told, each error extended with how its hold ended
    /// (`None`: the daemon stopped while it was held), never held again.
    fn release_held_as(&mut self, released: Vec<(u64, Option<blaze_gc::plan::Released>)>) {
        let mut said = self.spare.said.pop().unwrap_or_default();
        for (id, why) in released {
            let Some(at) = self.held.iter().position(|(held, _)| *held == id) else {
                tracing::warn!(target: "blaze", hold = id, "the collector let go of a build hold the daemon does not have");
                continue;
            };
            let (_, mut it) = self.held.swap_remove(at);
            if let Said::Finish(_, _, Some(error), _) = &mut it {
                // alloc: the error and its words, once a held build
                let more = match &why {
                    Some(why) => why.to_string(),
                    None => {
                        "the daemon stopped while the build waited for gc to free space".to_owned()
                    }
                };
                *error = error.clone().and(&more);
            }
            said.push(it);
        }
        self.said_as(said, false);
    }

    /// The plane's thread tasks that ended taken, and what it did given to the jobs, until it
    /// does nothing more. A part of the loop's turn (`blaze::loop_turn_effects`): the capture
    /// of 2026-11-12 held the loop 60 s in "unit narrowed identity" beneath it.
    #[blaze::bounded_turn]
    fn step_plane(&mut self) {
        let mut happened = std::mem::take(&mut self.spare.happened);
        let mut said = self.spare.said.pop().unwrap_or_default();
        self.jobs.apply_writes(&mut world!(self, said));
        self.said(said);
        loop {
            // The tasks the simulation's runner runs in line here read the stores on the
            // loop's driver ([`super::direct::lend`]); the completions of the loop's own
            // effects their reads were handed are dispatched next turn.
            let now = self.driver.now();
            let (plane, driver, done) = (&mut self.plane, &mut self.driver, &mut self.done);
            super::direct::lend(driver, done, || plane.poll(now));
            self.plane.happened(&mut happened);
            if happened.is_empty() {
                break;
            }
            self.settled +=
                happened.iter().filter(|it| matches!(it, Happened::Settled { .. })).count() as u64;
            let mut said = self.spare.said.pop().unwrap_or_default();
            self.jobs.happened(&mut world!(self, said), &mut happened);
            self.said(said);
        }
        self.spare.happened = happened;
    }

    /// Whether the daemon is between requests: none is read and held for its checkout's
    /// watcher, and none was read for [`QUIET_BEFORE_BETWEEN`] quiet turns.
    fn quiet_between_requests(&self) -> bool {
        self.heads.waiting() == 0 && self.quiet_turns >= QUIET_BEFORE_BETWEEN
    }

    /// Between requests: the collections, and after a cold build the image and the settle.
    ///
    /// It runs last in a turn, after the turn's answers were handed to the driver
    /// ([`Daemon::turn`]), and only while no job is under way and no kept front end is out
    /// being stored (the image and the collections read the front end's database and the
    /// roots a store's task reads). It never holds the next request up for more than one of
    /// its steps: each step is a span, a turn takes one (the collections, then in a later turn
    /// the image), so a request that arrived meanwhile is taken between them, and the image's
    /// write is let go when one comes ([`Daemon::let_go_of_image`]).
    fn between(&mut self) {
        if self.stopping.is_some()
            || !self.jobs.idle()
            || self.jobs.storing() > 0
            || self.launch.undurable()
            || self.jobs.placing_after() != 0
        {
            return;
        }
        if !self.quiet_between_requests() {
            return;
        }
        if self.jobs.ended {
            self.jobs.ended = false;
            if let Some(settles) = &mut self.settles {
                settles.policy.build_ended();
            }
            if let Some(collects) = &mut self.collects {
                collects.build_ended(&mut self.store);
            }
            let _span = tracing::info_span!(
                target: "blaze",
                "between requests: collect",
                quiet_turns = self.quiet_turns
            )
            .entered();
            self.db.collect(&[self.steps.plan_key()], &mut self.effects);
            if let Some(db) = Arc::get_mut(&mut self.db) {
                db.reclaim();
            }
            self.vfs.collect_if_due(&self.mounts.roots());
            self.heads.sweep();
            // The requests that read a corrupt group of the VFS's failed with it; the image is
            // not used past it. (The database is no part of the image: nothing of it is read
            // from one.)
            let corrupt = self.vfs.corrupt();
            if let Some(corrupt) = corrupt {
                return self.fail(Failed::Corrupt(corrupt));
            }
            // The image is asked in a later turn: what arrived during the collections is
            // taken first.
            if self.jobs.cold && !self.settling {
                return;
            }
        }
        if self.jobs.cold && !self.settling {
            self.jobs.cold = false;
            self.settling = true;
            self.worked = true;
            self.changed_since_settle = false;
            let _span = tracing::info_span!(
                target: "blaze",
                "between requests: ask the image",
                quiet_turns = self.quiet_turns
            )
            .entered();
            self.ask_image(false);
        }
        if self.settling
            && self.images.done()
            && let Some(failed) = self.images.failed()
        {
            // clone: an error, once.
            let failed = Failed::Image(failed.clone());
            self.settling = false;
            return self.fail(failed);
        }
        if self.images.done() {
            // The database is no part of the image and does not settle onto it (a restart
            // restores no engine state). The VFS settles on the image just mapped: from now on
            // it reads every file text the image holds from the mapping and lets its own go.
            if let Some(image) = self.images.take_mapped() {
                let span = tracing::info_span!(
                    target: "blaze",
                    "between requests: settle the VFS on the image",
                    nodes = tracing::field::Empty,
                    freed = tracing::field::Empty
                )
                .entered();
                // clone: a count of the mapping, once an image.
                match self.vfs.settle(blaze_vfs::ImageFile {
                    // clone: a count of the image's mapping, once a settle.
                    file: image.owner().clone(),
                    texts: self.images.texts(),
                }) {
                    Ok(settled) => {
                        span.record("nodes", settled.nodes);
                        span.record("freed", settled.freed);
                    }
                    Err(unsettled) => {
                        drop(span);
                        self.settling = false;
                        return self.fail(Failed::Settle(unsettled));
                    }
                }
            }
            self.settling = false;
        }
    }

    /// The store's cold moves, one step a turn ([`super::settle`]): a batch that came back
    /// compressed is put, whatever the daemon is doing (it is a copy into the store's segment);
    /// between requests, the next batch is taken out for a worker, or what follows the cold
    /// moves is stepped, or a period ends. Nothing here waits for zstd or for a sync.
    fn settle_turn(&mut self) {
        use super::settle::{FLUSH_EVERY, STEP_MOST, TAKE_MOST};
        if self.stopping.is_some() || !self.store.ready() || self.store.broken().is_some() {
            return;
        }
        let quiet = self.quiet_between_requests();
        let Some(settles) = &mut self.settles else { return };
        // A commit in flight holds a batch back until its head is written: the store makes
        // its moves between commits.
        if !self.store.committing()
            && let Some(batch) = settles.back()
        {
            let bytes = batch.bytes();
            let span = tracing::info_span!(
                target: "blaze",
                "store cold put",
                kind = batch.kind(),
                values = batch.len(),
                bytes,
                moved = tracing::field::Empty,
            )
            .entered();
            let moved = self.store.settle_put(batch);
            span.record("moved", moved);
            settles.unflushed += bytes;
        }
        // An index swap's flags of use, carried to the new table on a worker.
        if let Some(carry) = settles.carried() {
            self.store.carry_back(carry);
        }
        if !settles.carrying()
            && let Some(carry) = self.store.carry_take()
        {
            settles.carry(carry);
        }
        // A period's scan that came back: its plan is the store's from here on.
        if let Some(scan) = settles.scanned() {
            match scan {
                Some(scan) => {
                    let span = tracing::info_span!(
                        target: "blaze",
                        "store period plan",
                        records = scan.records(),
                        taken = tracing::field::Empty,
                    )
                    .entered();
                    let taken = self.store.plan_put(scan);
                    span.record("taken", taken);
                }
                None => self.store.plan_lost(),
            }
        }
        if !self.jobs.idle()
            || self.jobs.storing() > 0
            || self.launch.undurable()
            || self.jobs.placing_after() != 0
            || !quiet
            // The settle's image is being written: the two do not share the disk.
            || self.settling
            || settles.out()
            || settles.scanning()
        {
            return;
        }
        if self.store.settling() {
            if self.store.committing() {
                return;
            }
            let taken = {
                let _span = tracing::info_span!(target: "blaze", "store cold take").entered();
                self.store.settle_take(TAKE_MOST)
            };
            match taken {
                Some(batch) => settles.compress(batch),
                None => {
                    let span = tracing::info_span!(
                        target: "blaze",
                        "store settle step",
                        promoted = tracing::field::Empty,
                        retired = tracing::field::Empty,
                        more = tracing::field::Empty,
                    )
                    .entered();
                    let settled = self.store.settle_step(STEP_MOST, &mut self.effects);
                    span.record("promoted", settled.promoted);
                    span.record("retired", settled.retired);
                    span.record("more", settled.more);
                }
            }
            if settles.unflushed >= FLUSH_EVERY {
                settles.unflushed = 0;
                self.store.flush(&mut self.effects);
            }
            return;
        }
        // The plan is done: what its batches moved is committed.
        if settles.unflushed > 0 {
            settles.unflushed = 0;
            self.store.flush(&mut self.effects);
            return;
        }
        let now = self.driver.now();
        if settles.policy.due(now) {
            let span = tracing::info_span!(
                target: "blaze",
                "store period ends",
                ended = tracing::field::Empty,
            )
            .entered();
            // The period's end is a scan of the store's rows, which a worker runs: the loop
            // names it here and takes its plan back when it comes ([`Settles::scanned`]).
            let scan = self.store.plan_take();
            span.record("ended", scan.is_some());
            if let Some(scan) = scan {
                settles.policy.ended(now);
                settles.scan(scan);
            }
        }
    }

    /// The store's collector, one step a turn ([`super::collect`]): a batch that came back
    /// from its worker is taken, a bounded piece of copying is done, or a pack is retired.
    /// A pack is retired only while no build is under way, queued, being stored or placed.
    fn collect_turn(&mut self) {
        if self.stopping.is_some() || !self.store.ready() || self.store.broken().is_some() {
            return;
        }
        // Lane packs-1112: what a pack command asks of the store's owner (a pin written, the
        // keys of a pin, a flush) is done here, on the loop, before the collector's step of
        // this turn: an import's pin is in the store before any of its records is put.
        let now = self.driver.now();
        self.packings.turn(&mut self.store, self.collects.as_mut(), now, &mut self.effects);
        let Some(collects) = &mut self.collects else { return };
        let idle =
            self.jobs.idle() && self.jobs.storing() == 0 && self.jobs.placing_after() == 0;
        let now = self.driver.now();
        collects.turn(&mut self.store, self.gc.as_mut(), idle, now, &mut self.effects);
    }

    fn stepping_stop(&mut self) {
        let now = self.driver.now();
        if let Some((since, _)) = &self.stop_settles {
            let left = self.jobs.settling() + self.launch.due.len() + usize::from(self.launch.undurable());
            let waited = now.saturating_sub(*since);
            if left > 0 && waited < STOP_SETTLES_WITHIN {
                return;
            }
            if left > 0 {
                // alloc: a stop that waited its bound, once
                let why = format!(
                    "the stop waited {} s for it and goes on",
                    STOP_SETTLES_WITHIN.as_secs()
                );
                let mut said = self.spare.said.pop().unwrap_or_default();
                self.jobs.let_go_of_settling(&mut world!(self, said), &why);
                self.said(said);
            }
            self.stop_settles = None;
            self.stop_image();
        }
        if let (Some(Stopping::Image), Some(to)) = (&self.stopping, self.stop_from)
            && let Some(doing) = self.images.doing()
            && doing != self.stop_said
        {
            let mut replies = std::mem::take(&mut self.replies);
            // clone: the step said, once per step of a stop.
            replies.push(Reply {
                to,
                response: blaze_api::Response::Stopping { doing: doing.clone() },
            });
            self.send(replies);
            self.stop_said = doing;
        }
        match self.stopping {
            Some(Stopping::Image) if self.images.done() => {
                if let Some(failed) = self.images.failed()
                    && self.failed.is_none()
                {
                    // clone: an error, once.
                    self.failed = Some(Failed::Image(failed.clone()));
                }
                self.server.stop(&mut self.effects);
                self.stopping = Some(Stopping::Hangup { at: now });
            }
            Some(Stopping::Hangup { .. }) if self.driver.in_flight() == 0 => {
                self.stopping = Some(Stopping::Over);
            }
            Some(Stopping::Hangup { at }) if now.saturating_sub(at) > LAST_EFFECTS => {
                // The bound, not the last effect, ended the wait: a client that stopped reading
                // still has its answer's send in flight. Hang up on it, and say who lost what.
                let in_flight = self.driver.in_flight();
                let mut arrived = self.spare.arrived.pop().unwrap_or_default();
                let abandoned = self.server.abandon(&mut self.effects, &mut arrived);
                match abandoned.conns.is_empty() {
                    true => {
                        tracing::info!(target: "blaze", in_flight, "the stop's last effects outlasted {LAST_EFFECTS:?}; no connection was still being answered")
                    }
                    false => tracing::warn!(
                        target: "blaze",
                        in_flight,
                        conns = abandoned.conns.len(),
                        unsent_bytes = abandoned.unsent_bytes,
                        "the stop waited {LAST_EFFECTS:?} for its last replies to be sent and gave up: {abandoned}"
                    ),
                }
                for it in arrived.drain(..) {
                    self.arrive(it);
                }
                self.spare.arrived.push(arrived);
                self.abandoned = (!abandoned.conns.is_empty()).then_some(abandoned);
                self.stopping = Some(Stopping::Over);
            }
            _ => {}
        }
    }
}

mod ahead;
mod launch;
#[cfg(test)]
pub(super) use launch::MARKS_BEFORE_DURABLE;

#[cfg(test)]
mod tests {
    use blaze_image::{Cause, SectionId};

    /// The turn (counted from 0) at which the work between requests may first run under
    /// `rule`, for a build that turns `placing` times with no completion and no plane task
    /// (its outputs being placed), is answered, and leaves the loop idle after: the oracle of
    /// `the_work_between_requests_waits_for_quiet_turns_after_the_answer`.
    fn between_first_runs_at(rule: fn(u32, bool, bool, bool) -> u32, placing: u32) -> u32 {
        let mut quiet = 0;
        for turn in 0..placing + 4 * QUIET_BEFORE_BETWEEN {
            quiet = rule(quiet, turn < placing, false, false);
            if quiet >= QUIET_BEFORE_BETWEEN {
                return turn;
            }
        }
        u32::MAX
    }

    /// The invariant: the work between requests (the collections, the image's making, in
    /// line on the loop) first runs [`QUIET_BEFORE_BETWEEN`] quiet turns after a build's
    /// answer, however long the build turned with nothing completing: the client that got the
    /// answer asks its next thing (its rows, why it recomputed) into an idle loop. Its
    /// control is `the_oracle_catches_quiet_turns_counted_under_a_build`.
    #[test]
    fn the_work_between_requests_waits_for_quiet_turns_after_the_answer() {
        for placing in [0, 1, QUIET_BEFORE_BETWEEN, 40] {
            assert_eq!(
                between_first_runs_at(quiet_turns_after, placing),
                placing + QUIET_BEFORE_BETWEEN - 1,
                "a build that placed for {placing} turns"
            );
        }
        // A completion or a running task is not quiet, and loses no count.
        assert_eq!(quiet_turns_after(3, false, true, false), 3);
        assert_eq!(quiet_turns_after(3, false, false, true), 3);
        assert_eq!(quiet_turns_after(3, true, false, false), 0);
    }

    /// The planted bug: the count as it was until 2026-11-11, blind to a job under way. A
    /// build that places for 40 turns is past the bound before it is answered.
    #[test]
    fn the_oracle_catches_quiet_turns_counted_under_a_build() {
        fn planted(before: u32, _under_way: bool, took: bool, running: bool) -> u32 {
            if took || running { before } else { before.saturating_add(1) }
        }
        let at = between_first_runs_at(planted, 40);
        assert!(at < 40, "the oracle missed it: the work first ran at turn {at}");
        assert_eq!(at, QUIET_BEFORE_BETWEEN - 1);
    }

    use super::*;

    /// A step of a loop turn is said as stalled only past its bound, with its milliseconds.
    #[test]
    fn a_step_is_stalled_only_past_its_bound() {
        assert_eq!(stalled_ms(0, STEP_BOUND_NS), None);
        assert_eq!(stalled_ms(5, 5 + STEP_BOUND_NS + 1), Some(20));
        assert_eq!(stalled_ms(0, 735_000_000), Some(735));
        assert_eq!(stalled_ms(10, 3), None, "a clock read out of order is no stall");
    }

    fn corrupt(section: SectionId) -> Corrupt {
        Corrupt { section, group: 3, cause: Cause::Crc { found: 1, wanted: 2 } }
    }

    #[test]
    fn an_image_of_another_layout_is_not_read_and_a_damaged_one_is_a_fault() {
        let found = SchemaHash(ContentId([1; 32]));
        assert_eq!(
            opening::<()>(Err(OpenError::Layout { found })),
            Ok(Err(Unimaged::Layout(found)))
        );
        assert_eq!(opening::<()>(Ok(())), Ok(Ok(())));
        let cut = OpenError::Header("a length that is not the file's");
        assert_eq!(opening::<()>(Err(cut.clone())), Err(ImageFault::Open(cut)));
        let torn = OpenError::Hash { found: ContentId([2; 32]), wanted: ContentId([3; 32]) };
        assert_eq!(opening::<()>(Err(torn.clone())), Err(ImageFault::Open(torn)));
    }

    #[test]
    fn the_vfs_uses_by_a_rule_are_said_and_its_faults_stop_the_start() {
        let name = ContentId([6; 32]);
        let mut started = Started::default();
        vfs_used(ImageUse::Used { name, nodes: 10, disks: None }, &mut started).unwrap();
        assert!(started.whole(), "{started}");

        let refused = Refused::Missing { id: SectionId::VFS_DISKS };
        let disks = Some(DisksUnused::Refused(refused.clone()));
        vfs_used(ImageUse::Resumed { name, nodes: 10, disks }, &mut started).unwrap();
        assert!(started.resumed && started.disks == Some(refused), "{started:?}");
        assert!(started.to_string().contains("the checkouts are scanned"), "{started}");
        for fault in [
            DisksUnused::Corrupt(corrupt(SectionId::VFS_DISKS)),
            DisksUnused::Outside,
            DisksUnused::Malformed("a cursor past the end"),
        ] {
            let mut started = Started::default();
            let used = ImageUse::Used { name, nodes: 10, disks: Some(fault.clone()) };
            assert_eq!(vfs_used(used, &mut started), Err(VfsFault::Disks(fault)));
            assert!(started.whole(), "a fault records no rule: {started}");
        }
        let mut started = Started::default();
        let refused = Refused::Missing { id: SectionId::VFS_TREE };
        vfs_used(ImageUse::Refused(refused.clone()), &mut started).unwrap();
        assert_eq!(started.tree, Some(refused));
        assert!(started.to_string().contains("the journal was loaded alone"), "{started}");

        let mut started = Started::default();
        let found = corrupt(SectionId::VFS_TREE);
        assert_eq!(vfs_used(ImageUse::Corrupt(found), &mut started), Err(VfsFault::Corrupt(found)));
        assert_eq!(
            vfs_used(ImageUse::Malformed("a node past the end"), &mut started),
            Err(VfsFault::Malformed("a node past the end"))
        );
        let unnamed = ImageUse::Unnamed { image: name, journal: Some(ContentId([7; 32])) };
        assert_eq!(
            vfs_used(unnamed, &mut started),
            Err(VfsFault::Unnamed { image: name, journal: Some(ContentId([7; 32])) })
        );
        assert!(started.whole(), "a fault records no rule: {started}");
    }

    #[test]
    fn a_start_without_an_image_says_why() {
        let started = Started {
            unimaged: Some(Unimaged::NoImage),
            journal_cut: 12,
            heads_dropped: 1,
            ..Started::default()
        };
        assert!(!started.whole());
        let said = started.to_string();
        assert!(said.contains("no image was read (there is no daemon image)"), "{said}");
        assert!(said.contains("12 bytes"), "{said}");
    }

    #[test]
    /// Rule 26: the words a start of the layout before took (`database`, `trees`, `carries`
    /// name parts no start reads now) are refused by name, never taken quietly; `history`
    /// still parses.
    #[test]
    fn restore_without_refuses_the_parts_no_start_reads_by_name() {
        assert_eq!(Without::parse("history"), Ok(Without { history: true }));
        assert_eq!(Without::parse(" , "), Ok(Without::default()));
        for word in ["database", "trees", "carries"] {
            let Err(why) = Without::parse(&format!("history,{word}")) else {
                panic!("{}", blaze_report::say!(blaze_report::say::Code(word), " was taken").flat())
            };
            let said = blaze_report::say!(blaze_report::say::Code(word), ", which no start reads any more").flat();
            assert!(why.contains(&said), "{why}");
        }
        let Err(why) = Without::parse("memos") else { panic!("`memos` was taken") };
        assert!(why.contains("no part of the daemon image"), "{why}");
    }

    #[test]
    fn a_damaged_image_names_its_file() {
        let file = FileName::new("state/db/image").unwrap();
        let error = StartError::image(
            &file,
            ImageFault::Vfs(VfsFault::Unnamed { image: ContentId([8; 32]), journal: None }),
        );
        let said = error.to_string();
        assert!(said.starts_with("the daemon image /state/db/image: "), "{said}");
        assert!(said.contains("starts from no image"), "{said}");
        assert!(said.contains("the daemon does not start: `blaze doctor` checks it"), "{said}");
    }

    #[test]
    fn the_memory_line_names_the_tags_holding_the_most_largest_first() {
        use blaze_mem::{Tag as MemTag, tag::Counters, tag::Snapshot};
        let mut tags = [Counters::default(); MemTag::COUNT];
        // Little in blaze-mem, much in the global heap: first, for what it holds in all.
        tags[MemTag::Infer.index()] = Counters {
            live: 1 << 30,
            high_water: 2 << 30,
            heap: 4 << 30,
            heap_high_water: 5 << 30,
            ..Counters::default()
        };
        tags[MemTag::Query.index()] = Counters {
            live: 3 << 30,
            high_water: 3 << 30,
            reserved: 900 << 30,
            regions: 400,
            ..Counters::default()
        };
        tags[MemTag::Parse.index()] =
            Counters { live: 1 << 20, high_water: 1 << 20, ..Counters::default() };
        // Little held, much reserved: shown for its reservation.
        tags[MemTag::Link.index()] =
            Counters { live: 1 << 20, reserved: 800 << 30, regions: 7000, ..Counters::default() };
        let memory = Snapshot {
            tags,
            committed: 5 << 30,
            committed_high_water: 6 << 30,
            reserved: 1700 << 30,
            reserved_high_water: 1800 << 30,
            base_pages: 0,
            base_pages_high_water: 0,
            thp: blaze_mem::huge::Thp::Unset,
            heap_outside: 1 << 30,
            heap_outside_allocated: 0,
            heap_outside_freed: 0,
            heap_by_tag: true,
            purged: 1 << 30,
            heap_committed: Some(7 << 30),
        };
        let mut blocks = blaze_mem::blocks::Snapshot::default();
        blocks.classes[3] = blaze_mem::blocks::ClassCounters {
            size: 112,
            carved: 3 << 29,
            warm: 1 << 29,
            cold: 1 << 28,
            committed: 2 << 30,
            given_back: 1 << 28,
            trims: 9,
        };
        blocks.classes[10] = blaze_mem::blocks::ClassCounters {
            size: 5 << 10,
            carved: 1 << 20,
            ..Default::default()
        };
        let chunks = blaze_mem::chunks::Cached { warm: 1 << 29, cold: 1 << 30 };
        let said = MemoryLine {
            memory: &memory,
            blocks: &blocks,
            resident: Some(12 << 30),
            heap: 6 << 30,
            chunks,
            stacks: blaze_query::db::StackHighWater::default(),
        }
        .to_string();
        let infer = said
            .find(&format!(
                "; {} 1.00 (most 2.00, reserved 0 in 0) + heap 4.00 (most 5.00)",
                MemTag::Infer
            ))
            .expect(&said);
        let query = said
            .find(&format!("; {} 3.00 (most 3.00, reserved 900 in 400);", MemTag::Query))
            .expect(&said);
        let link = said
            .find(&format!("; {} 0.00 (most 0.00, reserved 800 in 7000);", MemTag::Link))
            .expect(&said);
        assert!(infer < query && query < link, "{said}");
        assert!(
            said.starts_with(
                "memory: resident 12.00 GiB, 1.00 outside blaze-mem and the global heap; \
                 global heap 6.00 GiB live, 1.00 of it outside any task, in 7.00 its allocator ever committed \
                 (1.00 not live: kept freed, or purged; this count never falls on a purge and nothing is charged by it), \
                 at most 2.00 of it freed and resident; \
                 committed 5.00 GiB (most 6.00), 1.00 of it purged (not resident), \
                 4.00 GiB in tags, reserved 1700 GiB in 7400 regions (most 1800)"
            ),
            "{said}"
        );
        // An allocator that keeps no tags says so, rather than show every tag holding none.
        let untagged = Snapshot { heap_by_tag: false, ..memory };
        let said_untagged = MemoryLine {
            memory: &untagged,
            blocks: &blocks,
            resident: None,
            heap: 6 << 30,
            chunks,
            stacks: blaze_query::db::StackHighWater::default(),
        }
        .to_string();
        assert!(
            said_untagged
                .starts_with("memory: global heap 6.00 GiB live, not kept by tag, in 7.00 "),
            "{said_untagged}"
        );
        assert!(!said.contains(&format!("; {} ", MemTag::Parse)), "{said}");
        // The classes: live is carved less warm and cold; a class carving little is not shown.
        assert!(
            said.ends_with(
                "; free chunks warm 0.50 (resident), cold 1.00 (purged)\
                 ; blocks live 0.75 GiB, warm 0.50, cold 0.25 (0.25 given back in 9 trims), carved 1.50, committed 2.00\
                 ; 112 B blocks live 0.75, warm 0.50, cold 0.25, carved 1.50"
            ),
            "{said}"
        );
        let none = blaze_mem::blocks::Snapshot::default();
        let unknown = MemoryLine {
            memory: &memory,
            blocks: &none,
            resident: None,
            heap: 0,
            chunks: blaze_mem::chunks::Cached::default(),
            stacks: blaze_query::db::StackHighWater::default(),
        }
        .to_string();
        assert!(unknown.starts_with("memory: committed 5.00 GiB"), "{unknown}");
        assert!(!unknown.contains("blocks"), "{unknown}");
        assert!(!unknown.contains("free chunks"), "{unknown}");
    }
}
