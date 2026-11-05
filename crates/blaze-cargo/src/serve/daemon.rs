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
//!   its last collection kept, keeping each view's plan and every mount; after a cold build the daemon image ([`Images`]: the database, the VFS, the
//!   plane's history and their symbols) is written and the database settles onto it.
//! - A start maps that image once: the symbols are restored from it first, then the VFS, the
//!   database and the plane's history each open their sections of it.
//! - A stop writes the image, waits for it to be mapped, then stops listening and hangs up.
//! - The explorer, the live feed and live SQL are answered by the loop itself ([`Observe`]);
//!   each build that ends goes to the profile history's thread. A build whose tasks run while
//!   no pool job ends, no query completes and no task or unit's step ends for the stall bound
//!   ([`Options::stall`], [`Watchdog::BOUND`] unless `blaze serve --stall-bound` names another)
//!   fails with the stall, whose trace is written to the state's `stalls/`, and the daemon
//!   stops: its workers may be deadlocked, and cannot be got back.

#![forbid(unsafe_code)]

use std::{fmt, net::SocketAddr, sync::Arc, time::Duration};

use blaze_api::{
    ApiError, Arrived, Reply, Request, RequestRef,
    server::{Abandoned, Config as ServerConfig, Http, Server, Unix},
    session::{self, Session},
};
use blaze_cas::Store;
use blaze_core::{Address, Completion, Done, Driver, Effect, FileName, Mapping, Tag};
use blaze_format::SchemaHash;
use blaze_graph::{Budget, PlaneConfig, arena};
use blaze_history::{Config as HistoryConfig, History, section::HistoryError};
use blaze_ids::ContentId;
use blaze_image::{Corrupt, Image, OpenError, Refused};
use blaze_profile::{Plot, Watchdog, watchdog::Progress};
use blaze_query::{Database, DbError};
use blaze_vfs::{DisksUnused, ImageUse, Outside, Ticket, Vfs};
use intern::RestoreError;

use super::{
    heads::{Heads, Out},
    image::{ImageFailed, Images, Parts, Unlearned, on_pool},
    job::{Jobs, Said, Setup, Step, Value, WaitedOn, World},
    mounts::Mounts,
    observe::{Observe, Profiling},
    plane::{Happened, Plane, Runner, ns},
    pulse::Pulse,
    still::{Facts, STILL_FOR, Stillness, Turn, Why},
    work::Steps,
};

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
/// A stall's trace, written once: the daemon stops after it.
const STALL: Tag = Tag(OWN + 9);
/// The image's write and its map ([`Images`]).
const IMAGE: u64 = OWN + 10;

/// How long a stop waits for the driver's last effects once it hung up: past it, the
/// connections still sending are hung up on ([`Server::abandon`]) and named in a warning.
pub(super) const LAST_EFFECTS: Duration = Duration::from_secs(2);
/// How often the plane's and the memory's plots are written to the flight recorder, at most.
const PLOT_EVERY: Duration = Duration::from_millis(10);
/// How often the memory by tag goes to the log while the daemon works, at most.
const MEMORY_LOG_EVERY: Duration = Duration::from_secs(30);
/// How often, at most, the daemon past its limit's line asks every thread to give back the free
/// pages its heaps keep (`blaze_mem::tag::give_back_heap`), between releases or with none due
/// (a check's build releases nothing): a forced collect of each thread's heaps costs it a walk
/// of their pages, so not at every plot.
const GIVE_BACK_EVERY: Duration = Duration::from_secs(1);
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
    /// thread while it waits (rule 13).
    pub graph: blaze_graph::sched::Waits,
    pub waits: Vec<StillWait>,
    /// Lowerings the loop holds until their root's kept front end is stored, which have no
    /// node of their own ([`Jobs::held_lowerings`]).
    pub lowerings: Vec<super::job::HeldLowering>,
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
            "blaze stalled: for {:.1} s nothing could move the build (no task ran or was ready, no effect was pending or in flight, the worker pool was idle with {} jobs parked; {} loop tasks open), with {} nodes not settled:",
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
        }
        if !self.graph.0.is_empty() {
            write!(f, "\n  {}", self.graph)?;
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
/// its job's `done`, which opens no span of its own (rule 13).
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

/// The builds clients wait on, as a stall names them: how many jobs of requests the session
/// has not seen finish, and each job of the build with its stage and what it waits on there.
/// A request the session waits on with no job of the build left for it is a lost job, said
/// so (rule 13: the client's wait names what it waits on).
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
            budget_bytes: $daemon.plane.budget().map_or(0, |it| it.bytes),
            // The driver's clock starts as the daemon does.
            uptime_ms: u64::try_from($daemon.driver.now().as_millis()).unwrap_or(u64::MAX),
            memory: blaze_mem::tag::snapshot,
            profile: blaze_profile::flight::dump,
            hot: crate::serve::observe::hot,
            history: $daemon.observe.history_error(),
            bugs: crate::serve::observe::bugs,
        }
    };
}

/// The jobs' view of the daemon for one call; the jobs themselves are left out of it.
macro_rules! world {
    ($daemon:ident, $said:ident) => {
        World {
            now: $daemon.driver.now(),
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
        }
    };
}

/// What the daemon starts with. No `Debug`: it holds the HTTP token.
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
    /// The socket it listens on; `None` takes requests from this process alone
    /// ([`Daemon::connect_local`]): the seed's one-shot build ([`super::once`]).
    pub socket: Option<FileName>,
    /// Where HTTP is listened on, and the bearer token its clients send.
    pub http: Option<(SocketAddr, String)>,
    pub budget: Option<Budget>,
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
}

fn under(dir: &FileName, name: &str) -> FileName {
    FileName::new(&format!("{dir}/{name}")).expect("a name under a directory is a name")
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

/// What ends the loop's wait for its driver between turns, besides the driver's own completions
/// (a client's connection or request, a file, a child, a watch): the timer the loop set, and
/// why. Named by the wait's span, so the loop's time between turns says what it waited on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Woken {
    /// Thread tasks run: their ends come back on the plane's channel, which the loop reads
    /// each turn, so it waits at most a millisecond.
    PlaneTasks,
    /// Nothing runs: the idle tick.
    Idle,
    /// An intent's deadline in the session.
    Session,
    /// A job's deadline for the store to make its puts durable.
    Durable,
    /// A live feed's next frame, or the deadline of a `blaze why` waiting on the history.
    Observe,
}

impl Woken {
    /// What the loop's wait waits on, as its span's `awaits` says it.
    fn on(self) -> blaze_graph::wait::WaitOn<'static> {
        use blaze_graph::wait::WaitOn;
        match self {
            Woken::PlaneTasks => WaitOn::Channel(
                "daemon loop: a plane task's end (read every 1 ms) or the driver's io",
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
    /// The effects that end on their own that ended ([`ends_alone`]: writes, reads and runs,
    /// never a client's connect or request, which a stuck daemon still takes).
    completions: u64,
    /// When the plots were last written, on the driver's clock.
    plotted: Duration,
    /// When the memory by tag was last logged, on the driver's clock.
    memory_logged: Duration,
    /// When the daemon past its limit's line last asked for the heaps' free pages back
    /// ([`GIVE_BACK_EVERY`]), on the driver's clock.
    given_back: Duration,
    /// The collector of what blaze keeps on disk ([`super::gc`]): `blaze serve`'s daemon's
    /// alone ([`Daemon::collect`]), as a test's daemons share no disk of their own.
    gc: Option<super::gc::Gc>,
    /// `blaze fetch-url`'s requests, each fetched on a thread of its own.
    fetch_urls: super::fetch_url::FetchUrls,
    /// Builds that failed for want of space, their finish held under the collector's hold id
    /// until the urgent run they asked for ends and the volumes are measured after it
    /// ([`super::gc::Gc::hold`]); then told, their error saying what the run found.
    held: Vec<(u64, Said)>,
}

/// What of its image a start did not use, each by a rule: the image was written by another
/// blaze, or a stop cut the journal. An image that is damaged is a [`StartError`] instead.
#[derive(Default, Debug)]
pub struct Started {
    /// The image the database opened; `None` when it started empty ([`Started::cold`]).
    pub restored: Option<Restored>,
    /// Why the database started empty; `None` when it opened the image's.
    pub cold: Option<Cold>,
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
    /// Why the image's library syntax trees were not read: every library file is parsed.
    pub trees: Option<String>,
}

impl Started {
    /// Whether the start used everything it found.
    pub fn whole(&self) -> bool {
        self.cold.is_none()
            && self.tree.is_none()
            && self.disks.is_none()
            && !self.resumed
            && self.journal_cut == 0
            && self.heads_dropped == 0
            && self.unlearned.is_none()
            && self.trees.is_none()
    }
}

impl fmt::Display for Started {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut sep = "";
        if let Some(cold) = &self.cold {
            write!(f, "{sep}the database starts empty: {cold}")?;
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
        if let Some(why) = &self.trees {
            write!(
                f,
                "{sep}the stored library trees were not read ({why}): library files are parsed"
            )?;
        }
        Ok(())
    }
}

/// The image a start's database opened, as its log names it.
#[derive(Clone, Debug)]
pub struct Restored {
    pub hash: ContentId,
    pub len: u64,
    /// Its [`Shape`](super::image::Shape): hash, bytes and sections.
    pub shape: String,
    /// From the start to the image mapped, on the driver's clock.
    pub mapped_in: Duration,
}

impl fmt::Display for Restored {
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
                    ", in {:.2} its allocator committed ({:.2} freed: kept, or purged and still counted)",
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
/// [`MEMORY_LOG_EVERY`], at a stop, and first in the postmortem of a daemon past its memory
/// ceiling (`serve::stop`).
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
    eprintln!("blaze serve: {said}");
    tracing::info!("{said}");
}

/// Why a start's database is empty.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Cold {
    /// There is no image: the first start in this state directory.
    NoImage,
    /// The image is of another layout: another blaze wrote it.
    Layout(SchemaHash),
    /// The image's symbol table or database sections are missing or of another schema.
    Refused(Refused),
    /// Values were interned in this process before the start, so the image's symbols can't be
    /// restored: a second daemon in one process, as the simulation's restarts are. `blaze
    /// serve` starts one daemon a process and stops on it.
    Interned,
}

impl fmt::Display for Cold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cold::NoImage => write!(f, "there is no daemon image"),
            Cold::Layout(found) => {
                write!(f, "the daemon image is of layout {}, another blaze's", found.0)
            }
            Cold::Refused(refused) => write!(f, "{refused}"),
            Cold::Interned => write!(
                f,
                "values were interned in this process before its symbols could be restored"
            ),
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
            StartError::Image { file, fault } => write!(f, "the daemon image /{file}: {fault}"),
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
    /// Its symbol table is corrupt or not a table.
    Symbols(RestoreError),
    /// The database's sections are corrupt or of a shape no blaze writes.
    Db(DbError),
    History(HistoryError),
    Vfs(VfsFault),
}

impl fmt::Display for ImageFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageFault::Open(error) => write!(f, "{error}"),
            ImageFault::Symbols(error) => write!(f, "{error}"),
            ImageFault::Db(error) => write!(f, "the database's sections: {error}"),
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
    /// The database could not settle onto the image its cold build wrote.
    Settle(DbError),
    /// The image a cold build or the stop asked for was not made; the one before it stays.
    Image(ImageFailed),
    /// A build stopped moving ([`blaze_profile::Stall`]'s words): its workers may be
    /// deadlocked, so the next request starts a fresh daemon.
    Stalled(String),
    /// A store stopped (a file of it could not be read or written), named.
    Store(String),
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failed::Server(why) | Failed::Stalled(why) | Failed::Store(why) => f.write_str(why),
            Failed::Corrupt(corrupt) => write!(f, "the daemon image: {corrupt}"),
            Failed::Settle(error) => {
                write!(f, "the database could not settle onto its image: {error}")
            }
            Failed::Image(failed) => write!(f, "{failed}"),
        }
    }
}

/// What of the start an open of the image's bytes gives: the image, or why the database
/// starts empty by a rule, or the fault.
fn opening<I>(opened: Result<I, OpenError>) -> Result<Result<I, Cold>, ImageFault> {
    match opened {
        Ok(image) => Ok(Ok(image)),
        Err(OpenError::Layout { found }) => Ok(Err(Cold::Layout(found))),
        Err(error) => Err(ImageFault::Open(error)),
    }
}

/// Why the database starts empty from an image whose symbols did not restore, or the fault.
fn unrestored(error: RestoreError) -> Result<Cold, ImageFault> {
    match error {
        RestoreError::Refused(refused) => Ok(Cold::Refused(refused)),
        RestoreError::Started { .. } => Ok(Cold::Interned),
        error @ (RestoreError::Corrupt(_) | RestoreError::Invalid { .. }) => {
            Err(ImageFault::Symbols(error))
        }
    }
}

/// Why the database starts empty from an image it did not open, or the fault.
fn unopened(error: DbError) -> Result<Cold, ImageFault> {
    match error {
        DbError::Refused(refused) => Ok(Cold::Refused(refused)),
        error => Err(ImageFault::Db(error)),
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
    /// Recovers the VFS from its journal and the database from its image, opens the store,
    /// and listens. Fails naming what could not be read, when the image is damaged, or when
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
        let store_config =
            blaze_cas::Config { dir: under(&state, "store"), layers: Vec::new(), tags: STORE };
        let mut store = Store::open(store_config, &mut effects);
        store.encode_columns(blaze_format::Spans::columns());
        store.encode_columns(hir_def::item_tree::ItemTree::columns());
        let shared = blaze_cas::SharedConfig {
            root: under(&options.std, "store"),
            writer: options.writer,
            tags: STD_STORE,
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
            if read_all && opened(&store) && opened(&std) {
                break;
            }
            driver.submit(&mut effects);
            driver.poll(&mut done, Duration::from_millis(50));
            for completion in done.drain(..) {
                if store.owns(completion.tag) {
                    store.complete(completion, &mut effects);
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
                        effects.push(Effect::Map { file: image.clone(), tag: MAP_IMAGE })
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
        let mut started = Started::default();
        // clone: a count of the mapping, shared by the VFS and the image the rest read.
        let opened = match mapping.clone().map(|it| opening(Image::open(it))).transpose() {
            Ok(None) => {
                started.cold = Some(Cold::NoImage);
                None
            }
            Ok(Some(Ok(opened))) => Some(Arc::new(opened)),
            Ok(Some(Err(cold))) => {
                started.cold = Some(cold);
                None
            }
            Err(fault) => return Err(StartError::image(&image, fault)),
        };
        // First: every other section names symbols by their IDs in this table.
        let restored = {
            let _span =
                tracing::info_span!(target: "blaze", "daemon start: restore the symbols").entered();
            opened.as_ref().map(|it| (it.clone(), intern::restore_symbols(it.clone(), &on_pool)))
        };
        let restoring =
            tracing::info_span!(target: "blaze", "daemon start: restore the VFS").entered();
        let (mut vfs, recovered) =
            Vfs::restore(journal.clone(), VFS, &read.journal.unwrap_or_default(), mapping);
        vfs.hash_on(on_pool);
        started.journal_cut = recovered.skipped;
        started.heads_dropped = recovered.heads_dropped;
        vfs_used(recovered.image, &mut started)
            .map_err(|fault| StartError::image(&image, ImageFault::Vfs(fault)))?;
        drop(restoring);
        let config =
            blaze_query::Config { tree: Some(Arc::new(vfs.reader())), dir: Some(db_dir), tags: DB };
        let opening_db =
            tracing::info_span!(target: "blaze", "daemon start: open the database").entered();
        let mut db = match restored {
            None => Database::new(config),
            Some((_, Err(error))) => {
                started.cold =
                    Some(unrestored(error).map_err(|fault| StartError::image(&image, fault))?);
                Database::new(config)
            }
            Some((opened, Ok(()))) => {
                let restored = Restored {
                    hash: opened.hash(),
                    len: opened.len(),
                    shape: super::image::Shape(&*opened).to_string(),
                    mapped_in: read.mapped_in,
                };
                match Database::from_image_with(config.clone(), opened) {
                    Ok(db) => {
                        started.restored = Some(restored);
                        db
                    }
                    Err(error) => {
                        started.cold = Some(
                            unopened(error).map_err(|fault| StartError::image(&image, fault))?,
                        );
                        Database::new(config)
                    }
                }
            }
        };
        steps.register(&mut db);
        drop(opening_db);
        let learning =
            tracing::info_span!(target: "blaze", "daemon start: restore the plane's history")
                .entered();
        let history = match opened.as_deref() {
            None => History::new(HistoryConfig::default()),
            Some(opened) => {
                let (history, unlearned) = super::image::history(opened, HistoryConfig::default())
                    .map_err(|error| StartError::image(&image, ImageFault::History(error)))?;
                started.unlearned = unlearned;
                history
            }
        };
        drop(learning);
        // Before any front end's database is made: each reads the trees installed when made.
        if let Some(opened) = &opened {
            let _span =
                tracing::info_span!(target: "blaze", "daemon start: install the library's trees")
                    .entered();
            match super::image::trees(opened) {
                Ok(Some(trees)) => _ = base_db::trees::install(Arc::new(trees)),
                Ok(None) => {}
                Err(why) => started.trees = Some(why),
            }
        }
        // The limit the daemon runs under (`blaze serve --memory-gb`, `BLAZE_DAEMON_MEMORY_GB`, or
        // the machine's) made every subsystem's budget and the limit a build's release is decided
        // against (`admission::Squeeze`), before any job. The plane only carries the budget: a
        // daemon that set nothing here ran with no limit, so no release was ever due and the
        // front end's sweep weighed against no budget (B5, coord-105). The frames' cap stays the
        // exec scheduler's: the plane does not run again a task the cap cancels.
        // Releases and the plane's admission weigh the process's footprint as the system counts
        // it, what the limit is on and the watchdog reads, not the allocators' counts, which a
        // purge never lowers (memcap5, coord-111: "blaze held 59.21 GiB" at a footprint of 47.28).
        blaze_mem::tag::resident_by(crate::raw::memory::own_footprint);
        if let Some(budget) = &options.budget {
            blaze_graph::admission::set_budgets(budget.bytes);
        }
        let plane_config = PlaneConfig {
            io: options.io,
            work: options.threads.max(1),
            budget: options.budget,
            starve_after: 8,
        };
        let observe = Observe::new(options.profiling);
        let mut plane = Plane::new(plane_config, history, runner)
            .map_err(|error| format!("the plane: {error}"))?;
        plane.number_from(observe.first_build());
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
        let jobs = Jobs::new(Setup {
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
        })
        .map_err(|error| format!("the jobs' tables: {error}"))?;
        Ok(Daemon {
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
            images: Images::new(image, IMAGE),
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
            completions: 0,
            plotted: Duration::ZERO,
            memory_logged: Duration::ZERO,
            given_back: Duration::ZERO,
            gc: None,
            fetch_urls: super::fetch_url::FetchUrls::new(),
            held: Vec::new(), // alloc: none until a build runs out of space
        })
    }

    /// Collects what blaze keeps on disk with `gc`, in the background of its turns.
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

    /// Whether its database started from the image: an image's symbols are restored once a
    /// process, before any is interned, so one started again in a process starts it empty.
    pub fn warm(&self) -> bool {
        self.started.cold.is_none()
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
        self.jobs.idle() && !self.jobs.cold && !self.settling
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

    /// The standard library's store, shared with this build's other daemons.
    pub fn std_store(&self) -> &Store {
        &self.std
    }

    /// Makes its store the process's ([`blaze_cas::Handle::install`]): the daemon of `blaze
    /// serve` alone, as a process has one store, and a test's daemons are many to a process. Its
    /// root is the driver's, `/`.
    pub fn install_store(&mut self) -> Result<&'static blaze_cas::Handle, String> {
        blaze_cas::Handle::install(blaze_cas::Handle::new(
            &mut self.store,
            std::path::PathBuf::from("/"),
        ))
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
        // The socket's name first: the next daemon may bind it while this one writes its
        // image, and a client that comes meanwhile finds no one rather than waits.
        self.server.unlisten(&mut self.effects);
        let mut replies = std::mem::take(&mut self.replies);
        self.observe.stop(&mut replies);
        self.send(replies);
        self.heads.stop(&mut self.effects);
        // A settle image asked with nothing changed since is the daemon as it is now: the stop
        // waits for it ([`Images::done`]) rather than makes the same image again after it.
        if !(self.settling && !self.changed_since_settle) {
            self.ask_image();
        }
        self.store.flush(&mut self.effects);
        self.std.flush(&mut self.effects);
        self.vfs.flush(&mut self.effects);
    }

    fn ask_image(&mut self) {
        let front = self.jobs.front_db();
        let parts = Parts {
            db: &self.db,
            front,
            vfs: &mut self.vfs,
            predict: self.plane.predict(),
            effects: &mut self.effects,
        };
        self.images.ask(parts);
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
        let mut wait = if !self.effects.is_empty() || self.plane.ready() {
            (Duration::ZERO, Woken::Idle)
        } else if self.plane.running() > 0 {
            (Duration::from_millis(1), Woken::PlaneTasks)
        } else {
            (Duration::from_millis(50), Woken::Idle)
        };
        let mut sooner = |until: Option<Duration>, woken: Woken| {
            if let Some(until) = until
                && until < wait.0
            {
                wait = (until, woken);
            }
        };
        sooner(
            self.session
                .deadline()
                .map(|at| Duration::from_nanos(at.saturating_sub(ns(self.driver.now())))),
            Woken::Session,
        );
        // A job's wait for the store to make its puts durable has a deadline of its own.
        sooner(
            self.jobs.next_deadline().map(|at| at.saturating_sub(self.driver.now())),
            Woken::Durable,
        );
        sooner(self.observe.wait(self.driver.now()), Woken::Observe);
        self.submit();
        let mut done = std::mem::take(&mut self.done);
        let (wait, woken) = wait;
        if wait.is_zero() {
            self.driver.poll(&mut done, wait);
        } else {
            // The loop's time between its turns, said as a wait on what ends it (rule 13).
            let driver = &mut self.driver;
            blaze_graph::wait::wait(woken.on(), None, || driver.poll(&mut done, wait));
        }
        for completion in &done {
            self.ended(completion);
        }
        // A turn with nothing to do is no span: an idle loop turns every millisecond.
        let _span = (!done.is_empty() || self.plane.ready())
            .then(|| tracing::info_span!("loop turn").entered());
        self.tick();
        for completion in done.drain(..) {
            self.dispatch(completion);
        }
        self.done = done;
        self.expire_jobs();
        self.step_plane();
        self.store_broke();
        self.observed();
        self.watch();
        self.plot();
        let idle = self.effects.is_empty() && self.driver.in_flight() == 0;
        let front = self.jobs.front_db();
        let parts = Parts {
            db: &self.db,
            front,
            vfs: &mut self.vfs,
            predict: self.plane.predict(),
            effects: &mut self.effects,
        };
        self.images.step(parts, idle);
        self.between();
        // A stalled job's leases end once its tasks let go of it.
        self.jobs.release_leases();
        let fetched = self.fetch_urls.take_answers();
        if !fetched.is_empty() {
            self.send(fetched);
        }
        if self.stopping.is_none()
            && let Some(gc) = &mut self.gc
        {
            gc.turn(self.jobs.idle(), self.heads.checkouts());
            let answers = gc.take_answers();
            let released = gc.take_released();
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
        self.stepping_stop();
        self.submit();
        self.pulse.turned(self.completions + self.settled + self.job_effects);
        !matches!(self.stopping, Some(Stopping::Over))
    }

    /// The loop's pulse, for a thread that watches the loop from outside it (the stop's guard,
    /// `serve::stop`): its progress (effects that ended on their own, the plane's nodes settled
    /// and the jobs' effects ended, never its idle turns or a client's requests) and its turns.
    pub fn pulse(&self) -> Arc<Pulse> {
        Arc::clone(&self.pulse) // clone: a handle, once a daemon
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
            Done::Changed { .. } | Done::Journaled(_) | Done::Accepted { .. }
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
            self.server.reply(&reply, &mut self.effects);
        }
        self.replies = replies;
        for row in self.jobs.started.drain(..) {
            self.observe.started(row);
        }
        for row in self.jobs.rows.drain(..) {
            self.observe.ended(row);
        }
    }

    /// The stall watchdog's turn: once the plane's tasks run and none of its counts (the worker
    /// pool's jobs ended; queries completed; tasks and unit's steps ended: work on what the
    /// build waits for, never the loop's turns or the workers' idle and search spans, which
    /// close on through a stall) has moved for the stall bound, every job with a plane request under way fails with the
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
        if pool.working() > 0 || self.jobs.runs_live() {
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
            "{stall}; {}; {}; {}",
            blaze_graph::sched::waits_now(),
            RunningTasks(&self.plane),
            BuildsWaited(self.session.building(), self.jobs.waited_on())
        );
        tracing::error!("{said}");
        let file = under(&self.state, &format!("stalls/{now}.json"));
        self.effects.push(Effect::Write { file, bytes: stall.trace, tag: STALL });
        // clone: the error, once a stall
        let error = ApiError::new(said.clone(), Some("stall".to_owned()));
        let mut told = self.spare.said.pop().unwrap_or_default();
        self.jobs.stalled(&mut world!(self, told), &error);
        self.said(told);
        self.fail(Failed::Stalled(said));
    }

    /// The loop's stall: a request has nodes not settled and nothing can move them (the loop has
    /// nothing ready, pending or in flight, no job's write waits for readers, no IO task runs,
    /// and the worker pool runs and queues nothing, while any task posted to it either has no
    /// job there, lost, or waits on what no wait of the graph will end: [`Stillness`]). Held
    /// for [`STILL_FOR`], every job with a plane request under way fails with [`Still`], naming
    /// what each node waits on and every wait of the graph, and the daemon stops, as on a stall
    /// the watchdog finds.
    fn still(&mut self) {
        let now = ns(self.driver.now());
        let pool = blaze_graph::exec::shared();
        // The pool first: a task's job sends its end before its worker sleeps, so a pool seen
        // idle has sent every end it will, and the loop sees it ready below.
        let (pool_idle, parked) = (pool.idle(), pool.parked());
        let on_pool = self.plane.running_on_pool();
        let facts = Facts {
            loop_moves: self.plane.ready()
                || self.effects.iter().any(ends_alone)
                || !self.ending.is_empty()
                || self.jobs.writes_waiting(),
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
                Turn::Began => {
                    self.effects.push(Effect::Timer { after: STILL_FOR, tag: STILL_TIMER });
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
            Why::NoTask | Why::Lost { .. } => None,
        };
        let still = Still {
            quiet_ns,
            why,
            loops: self.plane.loops_running(),
            parked,
            graph: graph.unwrap_or_else(blaze_graph::sched::waits_now),
            waits,
            lowerings: self.jobs.held_lowerings(),
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

    /// The plane's and the memory's plots, at most once each [`PLOT_EVERY`] of the driver's
    /// clock.
    fn plot(&mut self) {
        let now = self.driver.now();
        if now.saturating_sub(self.plotted) < PLOT_EVERY {
            return;
        }
        self.plotted = now;
        blaze_profile::plot(Plot::Running, self.plane.running() as u64);
        let memory = blaze_mem::tag::snapshot();
        blaze_profile::plot(Plot::FrameBytes, memory.get(blaze_mem::Tag::Frames).live);
        blaze_profile::plot(Plot::LiveBytes, memory.live());
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(bytes) = crate::raw::memory::footprint(std::process::id()) {
            blaze_profile::plot(Plot::Footprint, bytes);
            let limit = blaze_graph::admission::limit();
            let pressure = blaze_graph::admission::Pressure { held: bytes, limit, over: None };
            if pressure.squeezed() && now.saturating_sub(self.given_back) >= GIVE_BACK_EVERY {
                self.given_back = now;
                blaze_mem::tag::give_back_heap();
            }
        }
        blaze_profile::plot(Plot::CommittedBytes, memory.committed);
        blaze_profile::plot(Plot::ReservedBytes, memory.reserved);
        let (span_map_bytes, span_map_pieces) = ::span::kept_maps();
        blaze_profile::plot(Plot::SpanMapBytes, span_map_bytes);
        blaze_profile::plot(Plot::SpanMapPieces, span_map_pieces);
        if now.saturating_sub(self.memory_logged) >= MEMORY_LOG_EVERY {
            self.memory_logged = now;
            log_memory(&memory);
        }
    }

    /// The session told the time before anything else it is told this turn; intent whose
    /// time is up is cancelled.
    fn tick(&mut self) {
        let now = ns(self.driver.now());
        let due = self.session.deadline().is_some_and(|at| at <= now);
        self.session.tick(&mut cx!(self), now);
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
    fn store_broke(&mut self) {
        let (tell_jobs, fail) = (!self.jobs.idle(), self.failed.is_none());
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
                self.images.complete(&mut self.vfs, completion, &mut self.effects)
            }
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
            STORE..FLUSHES => {
                debug_assert!(self.store.owns(completion.tag), "the store's tags are its");
                self.job_effects += 1;
                self.store.complete(completion, &mut self.effects);
                // A step of its commit answered: a wait for it to make puts durable runs out
                // only when the commit stands still, never because placements or reads come.
                self.jobs.commit_moved(self.driver.now(), self.store.commit_steps());
                let placed = self.store.placed();
                let mut said = self.spare.said.pop().unwrap_or_default();
                self.jobs.placed(&mut world!(self, said), placed);
                self.said(said);
            }
            STD_STORE..STD_STORE_END => {
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
                self.heads.complete(&mut self.vfs, completion, &mut self.effects, &mut out);
                self.outs(out);
            }
            JOBS..JOBS_END | RUNS..RUNS_END => {
                debug_assert!(self.jobs.owns(completion.tag), "the jobs' tags are theirs");
                self.job_effects += 1;
                let mut said = self.spare.said.pop().unwrap_or_default();
                self.jobs.complete(&mut world!(self, said), completion);
                self.said(said);
            }
            _ => {}
        }
    }

    fn arrive(&mut self, arrived: Arrived) {
        let mut out = self.spare.out.pop().unwrap_or_default();
        self.heads.arrive(&self.vfs, arrived, &mut self.effects, &mut out);
        self.outs(out);
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
                Out::Pass(Arrived::Request { from, request: Request::Gc { dry_run } }) => {
                    match &mut self.gc {
                        Some(gc) if self.stopping.is_none() => gc.ask(from, dry_run),
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
                    let (response, ok) = match self.steps.launch(&program, pid) {
                        Ok(Some(env)) => (blaze_api::Response::Launched { live: true, env }, true),
                        // alloc: none, empty
                        Ok(None) => {
                            (blaze_api::Response::Launched { live: false, env: Vec::new() }, true)
                        }
                        Err(error) => (blaze_api::Response::Error(error), false),
                    };
                    self.server.reply(&Reply { to: from, response }, &mut self.effects);
                    self.server.reply(
                        &Reply { to: from, response: blaze_api::Response::Done { ok } },
                        &mut self.effects,
                    );
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
                        Request::ClientRows { build, events, folded, spans, ended_ns, started_ns },
                }) => {
                    let rows = blaze_profile::history::ClientRows {
                        build,
                        started_ns,
                        ended_ns,
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
                    request: Request::Rebuilt { all, cwd, item },
                }) => {
                    let mut replies = std::mem::take(&mut self.replies);
                    self.observe.rebuilt(from, cwd, item, all, self.driver.now(), &mut replies);
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
                    let mut replies = std::mem::take(&mut self.replies);
                    self.session.arrive(&mut cx!(self), arrived, &mut replies);
                    self.replies = replies;
                    self.after_session();
                }
                Out::Answer(reply) => self.server.reply(&reply, &mut self.effects),
                Out::Moved { head, root } => {
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
            self.server.reply(&reply, &mut self.effects);
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

    /// After a session call: the VFS flushed, what the session asked of the build started,
    /// and the replies sent.
    fn after_session(&mut self) {
        self.vfs.flush(&mut self.effects);
        let mut said = self.spare.said.pop().unwrap_or_default();
        self.jobs.step(&mut world!(self, said));
        self.said(said);
        let mut replies = std::mem::take(&mut self.replies);
        for reply in replies.drain(..) {
            self.server.reply(&reply, &mut self.effects);
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
    /// does nothing more.
    fn step_plane(&mut self) {
        let mut happened = std::mem::take(&mut self.spare.happened);
        let mut said = self.spare.said.pop().unwrap_or_default();
        self.jobs.apply_writes(&mut world!(self, said));
        self.said(said);
        loop {
            self.plane.poll(self.driver.now());
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

    /// Between requests: the collections, and after a cold build the image and the settle.
    fn between(&mut self) {
        if self.stopping.is_some() || !self.jobs.idle() {
            return;
        }
        if self.jobs.ended {
            self.jobs.ended = false;
            self.db.collect(&[self.steps.plan_key()], &mut self.effects);
            if let Some(db) = Arc::get_mut(&mut self.db) {
                db.reclaim();
            }
            self.vfs.collect_if_due(&self.mounts.roots());
            self.heads.sweep();
            // The requests that read a corrupt group failed with it; the image is not used
            // past it.
            let corrupt = self.db.take_corrupt().into_iter().next().or_else(|| self.vfs.corrupt());
            if let Some(corrupt) = corrupt {
                return self.fail(Failed::Corrupt(corrupt));
            }
        }
        if self.jobs.cold && !self.settling {
            self.jobs.cold = false;
            self.settling = true;
            self.changed_since_settle = false;
            self.ask_image();
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
        if self.images.done()
            && let Some(db) = Arc::get_mut(&mut self.db)
        {
            let settled = match self.images.take_mapped() {
                Some(image) => db.settle(image, &mut self.effects).map(|()| db.reclaim()),
                None => Ok(()),
            };
            self.settling = false;
            if let Err(error) = settled {
                self.fail(Failed::Settle(error));
            }
        }
    }

    fn stepping_stop(&mut self) {
        let now = self.driver.now();
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

#[cfg(test)]
mod tests {
    use blaze_image::{Cause, SectionId};

    use super::*;

    fn corrupt(section: SectionId) -> Corrupt {
        Corrupt { section, group: 3, cause: Cause::Crc { found: 1, wanted: 2 } }
    }

    #[test]
    fn an_image_of_another_layout_starts_cold_and_a_damaged_one_is_a_fault() {
        let found = SchemaHash(ContentId([1; 32]));
        assert_eq!(opening::<()>(Err(OpenError::Layout { found })), Ok(Err(Cold::Layout(found))));
        assert_eq!(opening::<()>(Ok(())), Ok(Ok(())));
        let cut = OpenError::Header("a length that is not the file's");
        assert_eq!(opening::<()>(Err(cut.clone())), Err(ImageFault::Open(cut)));
        let torn = OpenError::Hash { found: ContentId([2; 32]), wanted: ContentId([3; 32]) };
        assert_eq!(opening::<()>(Err(torn.clone())), Err(ImageFault::Open(torn)));
    }

    #[test]
    fn symbols_of_another_schema_start_cold_and_corrupt_ones_are_a_fault() {
        let refused = Refused::Missing { id: SectionId::SYMBOLS };
        assert_eq!(unrestored(RestoreError::Refused(refused.clone())), Ok(Cold::Refused(refused)));
        assert_eq!(
            unrestored(RestoreError::Started { section: SectionId::SYMBOLS }),
            Ok(Cold::Interned)
        );
        let bad = RestoreError::Corrupt(corrupt(SectionId::SYMBOLS));
        assert_eq!(unrestored(bad.clone()), Err(ImageFault::Symbols(bad)));
        let invalid = RestoreError::Invalid { section: SectionId::SYMBOLS, what: "a count", at: 8 };
        assert_eq!(unrestored(invalid.clone()), Err(ImageFault::Symbols(invalid)));
    }

    #[test]
    fn a_database_section_of_another_schema_starts_cold_and_any_other_error_is_a_fault() {
        let refused = Refused::Schema {
            id: SectionId::DB_TABLE,
            found: SchemaHash(ContentId([4; 32])),
            wanted: SchemaHash(ContentId([5; 32])),
        };
        assert_eq!(unopened(DbError::Refused(refused.clone())), Ok(Cold::Refused(refused)));
        for error in [
            DbError::Corrupt(corrupt(SectionId::DB_INPUTS)),
            DbError::Invalid { section: SectionId::DB_MEMOS, what: "a memo past the end" },
            DbError::Symbols,
        ] {
            assert_eq!(unopened(error.clone()), Err(ImageFault::Db(error)));
        }
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
    fn a_cold_start_says_why() {
        let started = Started {
            cold: Some(Cold::NoImage),
            journal_cut: 12,
            heads_dropped: 1,
            ..Started::default()
        };
        assert!(!started.whole());
        let said = started.to_string();
        assert!(said.contains("the database starts empty: there is no daemon image"), "{said}");
        assert!(said.contains("12 bytes"), "{said}");
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
                 global heap 6.00 GiB live, 1.00 of it outside any task, in 7.00 its allocator committed \
                 (1.00 freed: kept, or purged and still counted), at most 2.00 of it freed and resident; \
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
