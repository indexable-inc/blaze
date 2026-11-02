//! The daemon's loop: one driver, and every state machine of the daemon stepped by it.
//!
//! - A completion goes to what asked for it, by its tag: the server, the VFS, the database, the
//!   store, the checkouts' watchers and cookies and the watches outside views, the jobs (their lock writes, modes and user
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
//!   no span closes for [`Watchdog::BOUND`], no query completes for [`Watchdog::QUERY_BOUND`],
//!   or no unit's step ends for [`Watchdog::STEP_BOUND`] fails with the stall, whose trace is
//!   written to the state's `stalls/`, and the daemon stops: its workers may be deadlocked or
//!   going around without end, and cannot be got back.

#![forbid(unsafe_code)]

use std::{
    fmt,
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use blaze_api::{
    ApiError, Arrived, Reply, Request,
    server::{Config as ServerConfig, Http, Server, Unix},
    session::{self, Session},
};
use blaze_profile::{Plot, Watchdog, watchdog::still};
use blaze_cas::Store;
use blaze_core::{Address, Completion, Done, Driver, Effect, FileName, Mapping, Tag};
use blaze_graph::{Budget, PlaneConfig, arena};
use blaze_history::{Config as HistoryConfig, History, section::HistoryError};
use blaze_format::SchemaHash;
use blaze_ids::ContentId;
use blaze_image::{Corrupt, Image, OpenError, Refused};
use blaze_query::{Database, DbError};
use blaze_vfs::{DisksUnused, ImageUse, Outside, Ticket, Vfs};
use intern::RestoreError;

use super::{
    heads::{Heads, Out},
    image::{ImageFailed, Images, Parts, Unlearned, on_pool},
    job::{Jobs, Said, Setup, Step, Value, World},
    mounts::Mounts,
    observe::{Observe, Profiling},
    plane::{Happened, Plane, Runner, ns},
    work::Steps,
};

/// The tags of each part: `2^32` each, the watchers' and the runs' ranges of `2^32` after.
const OWN: u64 = 0;
const SERVER: u64 = 1 << 32;
const VFS: u64 = 2 << 32;
const DB: u64 = 3 << 32;
const STORE: u64 = 4 << 32;
const COOKIES: u64 = 5 << 32;
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

/// How long a stop waits for the driver's last effects once it hung up.
const LAST_EFFECTS: Duration = Duration::from_secs(2);
/// How often the plane's and the memory's plots are written to the flight recorder, at most.
const PLOT_EVERY: Duration = Duration::from_millis(10);
/// How often the memory by tag goes to the log while the daemon works, at most.
const MEMORY_LOG_EVERY: Duration = Duration::from_secs(30);
/// How long a build may have nothing that can move it before it fails with [`Still`]: past
/// the loop's longest turn and a worker's wake, short of anyone waiting on it.
const STILL_FOR: Duration = Duration::from_secs(2);
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
    /// Loop tasks started and not ended: with nothing in flight, each waits on the loop.
    pub loops: usize,
    /// The worker pool's jobs parked on a wait.
    pub parked: usize,
    pub waits: Vec<StillWait>,
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
        for it in self.waits.iter().take(STILL_NAMED) {
            write!(f, "\n  request {} node {} `{}`: {:?}", it.request, it.node, it.name, it.state)?;
            if let Some(on) = it.on {
                write!(f, ", waits on node {on}")?;
            }
            if let Some(held) = it.held {
                write!(f, ", held: {held}")?;
            }
        }
        if self.waits.len() > STILL_NAMED {
            write!(f, "\n  and {} more", self.waits.len() - STILL_NAMED)?;
        }
        Ok(())
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
            memory_bytes: arena::own(),
            budget_bytes: $daemon.plane.budget().map_or(0, |it| it.bytes),
            memory: blaze_mem::tag::snapshot,
            profile: blaze_profile::flight::dump,
            hot: crate::serve::observe::hot,
            history: $daemon.observe.history_error(),
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
    /// The directory this blaze build's daemons share the standard library in, whatever
    /// their `BLAZE_HOME` (`<machine>/std/<build id>`, [`super::machine`] and
    /// [`super::build_id`]), under the driver's root: each std key's outputs in a directory of
    /// its own, their records and outputs in the shared store `store` beside them.
    pub std: FileName,
    /// The daemon as a writer of that store: one per state directory.
    pub writer: ContentId,
    pub socket: FileName,
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
    pub color: bool,
    /// Which blaze the daemon is: its executable's hash and that file as it started
    /// ([`super::build_of`]), which a client compares with its own before it sends a request.
    pub build: blaze_api::Build,
    /// The table of directories outside views that user code's checks read
    /// ([`blaze_vfs::outside::dirs`]); the daemon drives its watches for its whole life.
    pub outside: &'static blaze_vfs::outside::Dirs,
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
    Hangup { at: Duration },
    Over,
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
    settling: bool,
    failed: Option<Failed>,
    pid: u32,
    /// Which blaze it is ([`Options::build`]), for [`blaze_api::Status::build`].
    build: blaze_api::Build,
    started: Started,
    /// Its state directory, in whose `stalls/` a stall's trace is written.
    state: FileName,
    observe: Observe,
    /// The stall watchdogs ([`Daemon::watch`]): of spans closed, of queries completed, of
    /// unit's steps ended.
    watchdog: Watchdog,
    query_watch: Watchdog,
    step_watch: Watchdog,
    /// Of unit's steps ended, at half [`Watchdog::STEP_BOUND`]: what every thread is inside is
    /// written to the log once a quiet stretch, and nothing fails.
    dump_watch: Watchdog,
    /// Since when nothing could move a build ([`Daemon::still`]), on the driver's clock.
    still_since: Option<u64>,
    /// The effects in flight that end on their own, by tag ([`ends_alone`]): what can still
    /// move a build, where the driver's count holds every watch, listen and receive too.
    ending: blaze_hash::HashMap<Tag, u32>,
    /// The plane's nodes that settled: with the spans closed, the progress the watchdog reads.
    settled: u64,
    /// When the plots were last written, on the driver's clock.
    plotted: Duration,
    /// When the memory by tag was last logged, on the driver's clock.
    memory_logged: Duration,
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
            write!(f, "{sep}the VFS's tree was not used ({refused}): the journal was loaded alone")?;
            sep = "; ";
        }
        if let Some(refused) = &self.disks {
            write!(f, "{sep}the VFS's watcher metadata was not used ({refused}): the checkouts are scanned")?;
            sep = "; ";
        }
        if self.resumed {
            write!(f, "{sep}a stop came between the image and its journal's rewrite: the whole journal was loaded over it")?;
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
            write!(f, "{sep}the stored library trees were not read ({why}): library files are parsed")?;
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

/// The daemon's memory by tag, for the log: what blaze-mem has committed, then each tag
/// holding at least [`MemoryLine::SHOWN`], largest first, with its high-water mark. Committed
/// bytes past the tags' sum are held by no tag.
pub struct MemoryLine<'a>(pub &'a blaze_mem::tag::Snapshot);

impl MemoryLine<'_> {
    const SHOWN: u64 = 64 << 20;
}

impl fmt::Display for MemoryLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let gib = |bytes: u64| bytes as f64 / f64::from(1u32 << 30);
        let memory = self.0;
        write!(
            f,
            "memory: committed {:.2} GiB (most {:.2}), {:.2} GiB in tags",
            gib(memory.committed),
            gib(memory.committed_high_water),
            gib(memory.live()),
        )?;
        let mut tags = blaze_mem::Tag::ALL;
        tags.sort_by_key(|&tag| std::cmp::Reverse(memory.get(tag).live));
        for tag in tags {
            let counters = memory.get(tag);
            if counters.live < Self::SHOWN {
                break;
            }
            write!(f, "; {tag} {:.2} (most {:.2})", gib(counters.live), gib(counters.high_water))?;
        }
        Ok(())
    }
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
            Cold::Layout(found) => write!(f, "the daemon image is of layout {}, another blaze's", found.0),
            Cold::Refused(refused) => write!(f, "{refused}"),
            Cold::Interned => write!(f, "values were interned in this process before its symbols could be restored"),
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
            VfsFault::Malformed(what) => write!(f, "its VFS tree is not one this blaze writes ({what})"),
            VfsFault::Unnamed { image, journal: Some(journal) } => {
                write!(f, "the VFS's journal starts from the image {journal}, not this one ({image})")
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
            Failed::Settle(error) => write!(f, "the database could not settle onto its image: {error}"),
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
        error @ (RestoreError::Corrupt(_) | RestoreError::Invalid { .. }) => Err(ImageFault::Symbols(error)),
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
        Some(fault @ (DisksUnused::Corrupt(_) | DisksUnused::Outside | DisksUnused::Malformed(_))) => {
            Err(VfsFault::Disks(fault))
        }
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
    pub fn start(mut driver: D, options: Options, steps: Arc<S>, runner: Runner<Step<S>>) -> Result<Daemon<D, S>, StartError> {
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
        if !activated {
            effects.push(Effect::Connect { at: Address::Unix(options.socket.clone()), tag: DIAL });
        }
        let store_config = blaze_cas::Config { dir: under(&state, "store"), layers: Vec::new(), tags: STORE };
        let mut store = Store::open(store_config, &mut effects);
        store.encode_columns(blaze_format::Spans::columns());
        store.encode_columns(hir_def::item_tree::ItemTree::columns());
        let shared = blaze_cas::SharedConfig { root: under(&options.std, "store"), writer: options.writer, tags: STD_STORE };
        let mut std = Store::open_shared(shared, &mut effects);
        let mut read = Read { socket_free: activated, ..Read::default() };
        let began = driver.now();
        let mut done = Vec::new();
        let socket = format!("/{}", options.socket);
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
                    (STAT_JOURNAL, Done::Stat(Some(_))) => effects.push(Effect::ReadAll { file: journal.clone(), tag: READ_JOURNAL }),
                    (READ_JOURNAL, Done::Read(bytes)) => read.journal = Some(bytes),
                    (STAT_IMAGE, Done::Stat(None)) => read.image = Some(None),
                    (STAT_IMAGE, Done::Stat(Some(_))) => effects.push(Effect::Map { file: image.clone(), tag: MAP_IMAGE }),
                    (MAP_IMAGE, Done::Mapped(mapping)) => {
                        read.image = Some(Some(mapping));
                        read.mapped_in = driver.now().saturating_sub(began);
                    }
                    (DIAL, Done::Connected { conn }) => {
                        effects.push(Effect::Hangup { conn, tag: HANG_UP });
                        driver.submit(&mut effects);
                        return Err(format!("another blaze serve listens on {socket}: `blaze stop` stops it").into());
                    }
                    (DIAL, Done::Failed(_)) => effects.push(Effect::Remove { file: options.socket.clone(), tag: UNLINK }),
                    (UNLINK, Done::Removed | Done::Failed(_)) => read.socket_free = true,
                    (READ_JOURNAL, Done::Failed(error)) => {
                        return Err(format!("reading the VFS's journal /{journal}: {error}").into());
                    }
                    (MAP_IMAGE, Done::Failed(error)) => return Err(format!("mapping the daemon image /{image}: {error}").into()),
                    (STAT_JOURNAL | STAT_IMAGE, Done::Failed(error)) => {
                        return Err(format!("the daemon's state in /{state}: {error}").into());
                    }
                    (tag, done) => return Err(format!("the start was answered {done:?} for tag {}", tag.0).into()),
                }
            }
        }
        if let Some(broken) = store.broken() {
            return Err(broken.to_string().into());
        }
        if let Some(broken) = std.broken() {
            return Err(format!("the standard library's shared store: {broken}").into());
        }
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
        let restored = opened.as_ref().map(|it| (it.clone(), intern::restore_symbols(it.clone(), &on_pool)));
        let (mut vfs, recovered) = Vfs::restore(journal.clone(), VFS, &read.journal.unwrap_or_default(), mapping);
        vfs.hash_on(on_pool);
        started.journal_cut = recovered.skipped;
        started.heads_dropped = recovered.heads_dropped;
        vfs_used(recovered.image, &mut started).map_err(|fault| StartError::image(&image, ImageFault::Vfs(fault)))?;
        let config = blaze_query::Config { tree: Some(Arc::new(vfs.reader())), dir: Some(db_dir), tags: DB };
        let mut db = match restored {
            None => Database::new(config),
            Some((_, Err(error))) => {
                started.cold = Some(unrestored(error).map_err(|fault| StartError::image(&image, fault))?);
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
                        started.cold = Some(unopened(error).map_err(|fault| StartError::image(&image, fault))?);
                        Database::new(config)
                    }
                }
            }
        };
        steps.register(&mut db);
        let history = match opened.as_deref() {
            None => History::new(HistoryConfig::default()),
            Some(opened) => {
                let (history, unlearned) = super::image::history(opened, HistoryConfig::default())
                    .map_err(|error| StartError::image(&image, ImageFault::History(error)))?;
                started.unlearned = unlearned;
                history
            }
        };
        // Before any front end's database is made: each reads the trees installed when made.
        if let Some(opened) = &opened {
            match super::image::trees(opened) {
                Ok(Some(trees)) => _ = base_db::trees::install(Arc::new(trees)),
                Ok(None) => {}
                Err(why) => started.trees = Some(why),
            }
        }
        let plane_config = PlaneConfig { io: options.io, work: options.threads.max(1), budget: options.budget, starve_after: 8 };
        let observe = Observe::new(options.profiling);
        let mut plane = Plane::new(plane_config, history, runner).map_err(|error| format!("the plane: {error}"))?;
        plane.number_from(observe.first_build());
        let mounts = Mounts::recover(&vfs);
        let mut listening = socket.clone();
        #[cfg(unix)]
        let unix = match options.activated {
            Some(passed) => {
                listening.push_str(&format!(" (passed by {})", passed.by()));
                Unix::Passed { socket: passed, at: options.socket.clone() }
            }
            None => Unix::Bind(options.socket.clone()),
        };
        #[cfg(not(unix))]
        let unix = Unix::Bind(options.socket.clone());
        if let Some((at, _)) = &options.http {
            listening.push_str(&format!(" and http://{at}"));
        }
        let server_config = ServerConfig {
            unix: Some(unix),
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
            std,
            plane,
            mounts,
            heads: Heads::new(COOKIES, WATCHERS, RUNS, Outside::new(options.outside, "/", OUTSIDE)),
            images: Images::new(image, IMAGE),
            jobs,
            steps,
            replies: Vec::new(),
            spare: Spare::default(),
            stopping: None,
            stop_asked: false,
            settling: false,
            failed: None,
            pid: std::process::id(),
            build: options.build,
            started,
            state,
            observe,
            watchdog: Watchdog::new(Watchdog::BOUND),
            query_watch: Watchdog::new(Watchdog::QUERY_BOUND),
            step_watch: Watchdog::new(Watchdog::STEP_BOUND),
            dump_watch: Watchdog::new(Watchdog::STEP_BOUND / 2),
            still_since: None,
            ending: blaze_hash::HashMap::default(),
            settled: 0,
            plotted: Duration::ZERO,
            memory_logged: Duration::ZERO,
        })
    }

    /// Where it listens, for people.
    pub fn listening(&self) -> &str {
        &self.listening
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

    /// The driver, the daemon let go: what a restart starts on.
    pub fn into_driver(self) -> D {
        self.driver
    }

    /// Whether no job runs and the database has settled onto the image of its last cold
    /// build.
    pub fn settled(&self) -> bool {
        self.jobs.idle() && !self.jobs.cold && !self.settling
    }

    pub fn vfs(&self) -> &Vfs {
        &self.vfs
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    /// Written only by the loop; read elsewhere through [`Store::reader`].
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The standard library's store, shared with this build's other daemons.
    pub fn std_store(&self) -> &Store {
        &self.std
    }

    /// Makes its store the process's ([`blaze_cas::Handle::install`]): the daemon of `blaze
    /// serve` alone, as a process has one store, and a test's daemons are many to a process. Its
    /// root is the driver's, `/`.
    pub fn install_store(&mut self) -> Result<&'static blaze_cas::Handle, String> {
        blaze_cas::Handle::install(blaze_cas::Handle::new(&mut self.store, std::path::PathBuf::from("/")))
    }

    pub fn mounts(&self) -> &Mounts {
        &self.mounts
    }

    /// Asks it to stop: the image first.
    pub fn stop(&mut self) {
        if self.stopping.is_some() {
            return;
        }
        self.stopping = Some(Stopping::Image);
        // The socket's name first: the next daemon may bind it while this one writes its
        // image, and a client that comes meanwhile finds no one rather than waits.
        self.server.unlisten(&mut self.effects);
        let mut replies = std::mem::take(&mut self.replies);
        self.observe.stop(&mut replies);
        self.send(replies);
        self.heads.stop(&mut self.effects);
        self.ask_image();
        self.store.flush(&mut self.effects);
        self.std.flush(&mut self.effects);
        self.vfs.flush(&mut self.effects);
    }

    fn ask_image(&mut self) {
        let front = self.jobs.front_db();
        let parts = Parts { db: &self.db, front, vfs: &mut self.vfs, predict: self.plane.predict(), effects: &mut self.effects };
        self.images.ask(parts);
    }

    /// One turn of the loop: what is asked submitted, what completed taken, the plane and the
    /// jobs stepped. `false` once it has stopped.
    pub fn turn(&mut self) -> bool {
        if matches!(self.stopping, Some(Stopping::Over)) {
            return false;
        }
        let wait = if !self.effects.is_empty() || self.plane.ready() {
            Duration::ZERO
        } else if self.plane.running() > 0 {
            Duration::from_millis(1)
        } else {
            Duration::from_millis(50)
        };
        let until = self.session.deadline().map(|at| Duration::from_nanos(at.saturating_sub(ns(self.driver.now()))));
        let wait = until.map_or(wait, |until| wait.min(until));
        let wait = self.observe.wait(self.driver.now()).map_or(wait, |until| wait.min(until));
        self.submit();
        let mut done = std::mem::take(&mut self.done);
        self.driver.poll(&mut done, wait);
        for completion in &done {
            self.ended(completion);
        }
        // A turn with nothing to do is no span: an idle loop turns every millisecond.
        let _span = (!done.is_empty() || self.plane.ready()).then(|| tracing::info_span!("loop turn").entered());
        self.tick();
        for completion in done.drain(..) {
            self.dispatch(completion);
        }
        self.done = done;
        self.step_plane();
        self.observed();
        self.watch();
        self.plot();
        let idle = self.effects.is_empty() && self.driver.in_flight() == 0;
        let front = self.jobs.front_db();
        let parts = Parts { db: &self.db, front, vfs: &mut self.vfs, predict: self.plane.predict(), effects: &mut self.effects };
        self.images.step(parts, idle);
        self.between();
        self.stepping_stop();
        self.submit();
        !matches!(self.stopping, Some(Stopping::Over))
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
        if matches!(completion.done, Done::Changed { .. } | Done::Journaled(_) | Done::Accepted { .. }) {
            return;
        }
        if let Some(count) = self.ending.get_mut(&completion.tag) {
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
            self.server.reply(reply, &mut self.effects);
        }
        self.replies = replies;
        for row in self.jobs.rows.drain(..) {
            self.observe.ended(row);
        }
    }

    /// The stall watchdogs' turn: once the plane's tasks run and what one counts has not moved
    /// for its bound (spans closed, queries completed, unit's steps ended), every job with a
    /// plane request under way fails with the stall, its trace is written to the state
    /// directory, and the daemon stops. At half the steps' bound, what every thread is inside
    /// is written to the log first, so a client's own watchdog that stops the daemon sooner
    /// still finds it.
    fn watch(&mut self) {
        self.still();
        if self.failed.is_some() {
            return;
        }
        let running = self.plane.running();
        let busy = running > 0;
        let now = ns(self.driver.now());
        let spans = blaze_profile::flight::progress() + self.settled;
        let queries = blaze_query::db::completed() + self.settled;
        if let Some(quiet) = self.dump_watch.tick(now, self.settled, busy) {
            let secs = quiet.for_ns / 1_000_000_000;
            tracing::warn!("no unit's step ended for {secs} s: {}", blaze_profile::watchdog::running());
        }
        let found = [
            (self.watchdog.tick(now, spans, busy), still::SPANS),
            (self.query_watch.tick(now, queries, busy), still::QUERIES),
            (self.step_watch.tick(now, self.settled, busy), still::STEPS),
        ];
        let found = found.into_iter().find_map(|(quiet, what)| Some((quiet?, what)));
        let Some((quiet, what)) = found else { return };
        let stall = blaze_profile::watchdog::stall(quiet, running, what);
        let said = stall.to_string();
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

    /// The loop's stall: a request has nodes not settled and nothing can move them (no thread
    /// task runs or ended untaken, no effect is pending or in flight, the worker pool runs and
    /// queues nothing, no job's write waits for readers, which whoever holds them ends). Held
    /// for [`STILL_FOR`], every job with a plane request under way fails
    /// with [`Still`], naming what each node waits on, and the daemon stops, as on a stall the
    /// watchdog finds.
    fn still(&mut self) {
        let now = ns(self.driver.now());
        let pool = blaze_graph::exec::shared();
        let moving = self.plane.running() > 0
            || self.plane.ready()
            || self.effects.iter().any(ends_alone)
            || !self.ending.is_empty()
            || !pool.idle()
            || self.jobs.writes_waiting();
        if moving {
            self.still_since = None;
            return;
        }
        let Some(since) = self.still_since else {
            let mut open = false;
            self.plane.unsettled(|_| open = true);
            if !open {
                return;
            }
            self.still_since = Some(now);
            self.effects.push(Effect::Timer { after: STILL_FOR, tag: STILL_TIMER });
            return;
        };
        if now.saturating_sub(since) < ns(STILL_FOR) {
            return;
        }
        // alloc: a stall, once
        let mut waits = Vec::new();
        let jobs = &self.jobs;
        self.plane.unsettled(|it| {
            waits.push(StillWait {
                request: it.request.0,
                node: it.id.0,
                name: it.name.to_owned(), // alloc: a stall, once per node it names
                state: it.state.clone(),
                on: it.on.map(|on| on.0),
                held: jobs.holding(it.id),
            });
        });
        if waits.is_empty() {
            self.still_since = None;
            return;
        }
        waits.sort_by_key(|it| (it.request, it.node));
        let still = Still {
            quiet_ns: now - since,
            loops: self.plane.loops_running(),
            parked: pool.parked(),
            waits,
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
        if now.saturating_sub(self.memory_logged) >= MEMORY_LOG_EVERY {
            self.memory_logged = now;
            let said = MemoryLine(&memory).to_string(); // alloc: one line each MEMORY_LOG_EVERY
            eprintln!("blaze serve: {said}");
            tracing::info!("{said}");
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

    /// A completion to its part, by the block of tags it is in: compares on the tag alone,
    /// no part asked and no lock taken.
    fn dispatch(&mut self, completion: Completion) {
        match completion.tag.0 {
            _ if self.images.owns(completion.tag) => self.images.complete(&mut self.vfs, completion, &mut self.effects),
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
            STORE..COOKIES => {
                debug_assert!(self.store.owns(completion.tag), "the store's tags are its");
                self.store.complete(completion, &mut self.effects);
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
                    let why = Failed::Store(format!("the standard library's shared store: {broken}"));
                    self.fail(why);
                    return;
                }
                let mut said = self.spare.said.pop().unwrap_or_default();
                self.jobs.looked(&mut world!(self, said));
                self.said(said);
            }
            COOKIES..JOBS | OUTSIDE..OUTSIDE_END | WATCHERS..RUNS => {
                debug_assert!(self.heads.owns(completion.tag), "the checkouts' tags are theirs");
                let mut out = self.spare.out.pop().unwrap_or_default();
                self.heads.complete(&mut self.vfs, completion, &mut self.effects, &mut out);
                self.outs(out);
            }
            JOBS..JOBS_END | RUNS..RUNS_END => {
                debug_assert!(self.jobs.owns(completion.tag), "the jobs' tags are theirs");
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
            match it {
                Out::Pass(Arrived::Request { from, request: Request::Explore { dev } }) => {
                    let mut replies = std::mem::take(&mut self.replies);
                    self.observe.explore(from, dev, &mut replies);
                    self.send(replies);
                }
                Out::Pass(Arrived::Request { from, request: Request::Live }) => self.observe.subscribe(from, self.driver.now()),
                Out::Pass(Arrived::Request { from, request: Request::Sql { sql, last_s } }) => {
                    let mut replies = std::mem::take(&mut self.replies);
                    self.observe.query(from, sql, last_s, &mut replies);
                    self.send(replies);
                }
                Out::Pass(arrived) => {
                    if let Arrived::Gone { conn } = &arrived {
                        self.observe.gone(*conn);
                    }
                    if matches!(arrived, Arrived::Request { request: Request::Stop, .. }) {
                        self.stop_asked = true;
                    }
                    let mut replies = std::mem::take(&mut self.replies);
                    self.session.arrive(&mut cx!(self), arrived, &mut replies);
                    self.replies = replies;
                    self.after_session();
                }
                Out::Answer(reply) => self.server.reply(reply, &mut self.effects),
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
            self.server.reply(reply, &mut self.effects);
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
            self.server.reply(reply, &mut self.effects);
        }
        self.replies = replies;
    }

    /// What the jobs said, told to the session.
    fn said(&mut self, mut said: Vec<Said>) {
        if said.is_empty() {
            self.spare.said.push(said);
            return;
        }
        let mut replies = std::mem::take(&mut self.replies);
        for it in said.drain(..) {
            match it {
                Said::Report(job, response) => self.session.report(&mut cx!(self), job, response, &mut replies),
                Said::Finish(job, ok, error, queries) => {
                    self.session.finish_in(&mut cx!(self), job, ok, error, queries, &mut replies);
                }
            }
        }
        self.spare.said.push(said);
        self.replies = replies;
        self.after_session();
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
            self.settled += happened.iter().filter(|it| matches!(it, Happened::Settled { .. })).count() as u64;
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
            self.heads.sweep(&mut self.effects);
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
            Some(Stopping::Hangup { at }) if self.driver.in_flight() == 0 || now.saturating_sub(at) > LAST_EFFECTS => {
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
        assert_eq!(unrestored(RestoreError::Started { section: SectionId::SYMBOLS }), Ok(Cold::Interned));
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
        assert_eq!(vfs_used(ImageUse::Malformed("a node past the end"), &mut started), Err(VfsFault::Malformed("a node past the end")));
        let unnamed = ImageUse::Unnamed { image: name, journal: Some(ContentId([7; 32])) };
        assert_eq!(
            vfs_used(unnamed, &mut started),
            Err(VfsFault::Unnamed { image: name, journal: Some(ContentId([7; 32])) })
        );
        assert!(started.whole(), "a fault records no rule: {started}");
    }

    #[test]
    fn a_cold_start_says_why() {
        let started = Started { cold: Some(Cold::NoImage), journal_cut: 12, heads_dropped: 1, ..Started::default() };
        assert!(!started.whole());
        let said = started.to_string();
        assert!(said.contains("the database starts empty: there is no daemon image"), "{said}");
        assert!(said.contains("12 bytes"), "{said}");
    }

    #[test]
    fn a_damaged_image_names_its_file() {
        let file = FileName::new("state/db/image").unwrap();
        let error = StartError::image(&file, ImageFault::Vfs(VfsFault::Unnamed { image: ContentId([8; 32]), journal: None }));
        let said = error.to_string();
        assert!(said.starts_with("the daemon image /state/db/image: "), "{said}");
        assert!(said.contains("starts from no image"), "{said}");
    }

    #[test]
    fn the_memory_line_names_the_tags_holding_the_most_largest_first() {
        use blaze_mem::{Tag as MemTag, tag::Counters, tag::Snapshot};
        let mut tags = [Counters::default(); MemTag::COUNT];
        tags[MemTag::Infer.index()] = Counters { live: 1 << 30, high_water: 2 << 30, ..Counters::default() };
        tags[MemTag::Query.index()] = Counters { live: 3 << 30, high_water: 3 << 30, ..Counters::default() };
        tags[MemTag::Parse.index()] = Counters { live: 1 << 20, high_water: 1 << 20, ..Counters::default() };
        let memory = Snapshot { tags, committed: 5 << 30, committed_high_water: 6 << 30 };
        let said = MemoryLine(&memory).to_string();
        let query = said.find(&format!("; {} 3.00 (most 3.00)", MemTag::Query)).expect(&said);
        let infer = said.find(&format!("; {} 1.00 (most 2.00)", MemTag::Infer)).expect(&said);
        assert!(query < infer, "{said}");
        assert!(said.starts_with("memory: committed 5.00 GiB (most 6.00), 4.00 GiB in tags"), "{said}");
        assert!(!said.contains(&format!("; {} ", MemTag::Parse)), "{said}");
    }
}