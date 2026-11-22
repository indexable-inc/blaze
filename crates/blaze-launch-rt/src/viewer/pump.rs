//! The viewer's state machine: which runs and builds it knows, who watches, and when it must
//! drain the rings. Events in (a viewer joined or left, the runs directory changed, a listing,
//! a ring attached, a build read, a run's process exited, the clock), [`Action`]s out (list the
//! directory, attach a ring, read a build, watch a process, delete a dead run's files, send a
//! frame, stop). It does no IO and reads no clock: the driver (`viewer.rs`) waits on the
//! descriptors and on [`Pump::next_deadline`], and carries the actions out.
//!
//! THE FLOOR (rule 67). Idle, the viewer makes no wakeup at all: with no viewer there is no
//! timer, so the only deadlines are the idle expiry and a dead run's file collection, one each
//! and once; with a viewer and no live ring there is no deadline. The one timer is the drain
//! tick, armed exactly while a viewer is connected and some run's ring may still hold events
//! (it is live, or dead and not yet drained), at [`TICK`] (16 ms, one frame of a 60 Hz display:
//! the page cannot show a batch sooner, and the events wait in the ring meanwhile), and at once
//! again when a drain filled its bound. A ring is shared memory a program writes without a
//! system call, so no descriptor says it advanced: the tick is the one way to see it, and it
//! runs only while someone looks.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use core::fmt::Write as _;
use core::time::Duration;

use blaze_own::{AString, AVec, Alloc};

use super::resources::push_json_text;

/// The drain tick while a viewer watches a run that may have events.
pub const TICK: Duration = Duration::from_millis(16);

/// A binary batch frame's first byte (`site/explore/main.ts` parses it).
pub const BIN_EVENT_MAGIC: u8 = b'B';

/// A binary batch frame's version.
pub const BIN_EVENT_VERSION: u8 = 1;

/// The bytes of one telemetry event in a batch.
pub const EVENT_BYTES: usize = 64;

/// A ring's counters, as `/api/runs` shows them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RingStats {
    pub cap: u32,
    pub pushed: u64,
    pub popped: u64,
    pub dropped: u64,
    pub shadow_overflows: u64,
    pub unwound_spans: u64,
}

/// A run's telemetry ring, as the core reads it: shared memory, no system call (the driver's
/// is a mapped `.telem` file, the simulation's a queue).
pub trait Ring {
    /// Pops up to `most` events, each appended to `into` as its [`EVENT_BYTES`]; how many.
    fn drain_into(&mut self, most: usize, into: &mut AVec<'_, u8>) -> usize;
    /// Its counters now.
    fn stats(&self) -> RingStats;
}

/// A viewer's connection, the driver's number for it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct ConnId(pub u64);

/// What `<run_id>.meta` says (`blaze-launcher` writes it before the ring): `pid=`, `package=`,
/// `started_ms=` and `slot=<id>:<name>` lines.
pub struct RunMeta<'a> {
    /// 0 when the file says none: the run is then taken as live until it is collected by hand.
    pub pid: u32,
    pub package: AString<'a>,
    pub started_ms: u64,
    /// By slot id, each once (the last line for an id wins).
    pub slots: AVec<'a, (u32, AString<'a>)>,
}

impl<'a> RunMeta<'a> {
    /// The meta of run `stem` from the file's `text`; for a field the text lacks (or no text:
    /// the file is missing), the package is the stem and the start `now_ms`.
    pub fn parse(text: Option<&str>, stem: &str, now_ms: u64, alloc: Alloc<'a>) -> RunMeta<'a> {
        let mut meta = RunMeta {
            pid: 0,
            package: AString::from_str_in(stem, alloc),
            started_ms: now_ms,
            slots: AVec::new_in(alloc),
        };
        for line in text.unwrap_or("").lines() {
            if let Some(value) = line.strip_prefix("pid=") {
                if let Ok(pid) = value.trim().parse::<u32>() {
                    meta.pid = pid;
                }
            } else if let Some(value) = line.strip_prefix("package=") {
                meta.package = AString::from_str_in(value.trim(), alloc);
            } else if let Some(value) = line.strip_prefix("started_ms=") {
                if let Ok(at) = value.trim().parse::<u64>() {
                    meta.started_ms = at;
                }
            } else if let Some(value) = line.strip_prefix("slot=")
                && let Some((id, name)) = value.split_once(':')
                && let Ok(id) = id.trim().parse::<u32>()
            {
                let name = AString::from_str_in(name.trim(), alloc);
                match meta.slots.binary_search_by_key(&id, |&(of, _)| of) {
                    Ok(at) => {
                        if let Some(slot) = meta.slots.get_mut(at) {
                            slot.1 = name;
                        }
                    }
                    Err(at) => meta.slots.insert_shift(at, (id, name)),
                }
            }
        }
        meta
    }
}

/// One entry of a runs-directory listing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Listed<'n> {
    pub name: &'n str,
    /// Its modification time (nanoseconds since the epoch, as the file system says): a build
    /// is read again when it changes.
    pub modified: u64,
}

/// The core's bounds and periods.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// With no viewer and no live run this long, the viewer stops (rule 70).
    pub idle_expiry: Duration,
    /// The drain tick ([`TICK`]).
    pub tick: Duration,
    /// A dead run's `.telem` and `.meta` are deleted this long after its process exited.
    pub collect_after: Duration,
    /// A ring that does not attach yet is tried again this long after.
    pub attach_wait: Duration,
    /// ... at most this many times, then reported.
    pub attach_tries: u32,
    /// The most events one run's drain takes per tick.
    pub drain_most: usize,
    /// The most runs tracked, and the most builds.
    pub runs_most: usize,
    pub builds_most: usize,
    /// Every this many ticks the runs list (with the rings' counters) is sent again.
    pub runs_every: u32,
}

impl Config {
    /// The viewer's: 16 ms ticks, 256 events a run a tick, 64 runs and 64 builds, files
    /// collected 15 s after their run, a ring attached within 25 tries 20 ms apart (half a
    /// second: `MappedRing::create` publishes a file in three steps, and nothing says when the
    /// last is done), the runs list sent again every 32 ticks (about twice a second).
    pub fn new(idle_expiry: Duration) -> Config {
        Config {
            idle_expiry,
            tick: TICK,
            collect_after: Duration::from_secs(15),
            attach_wait: Duration::from_millis(20),
            attach_tries: 25,
            drain_most: 256,
            runs_most: 64,
            builds_most: 64,
            runs_every: 32,
        }
    }
}

/// What the driver is asked to do.
pub enum Action<'a> {
    /// List the runs directory and answer [`Pump::listed`].
    ListDir,
    /// Attach `<stem>.telem` and read `<stem>.meta`; answer [`Pump::attached`] or
    /// [`Pump::attach_failed`].
    Attach { stem: AString<'a> },
    /// Read the build `<name>`; answer [`Pump::build_read`].
    ReadBuild { name: AString<'a>, modified: u64 },
    /// Watch process `pid` for its exit; answer [`Pump::exited`] when it exits (at once if it
    /// is gone already) or [`Pump::watch_failed`].
    WatchExit { pid: u32 },
    /// Delete `<stem>.telem` and `<stem>.meta`: its run is dead and collected.
    Remove { stem: AString<'a> },
    /// A text frame to one viewer.
    SendText { to: ConnId, text: AString<'a> },
    /// A text frame to every viewer.
    BroadcastText { text: AString<'a> },
    /// A binary frame to every viewer.
    BroadcastBinary { bytes: AVec<'a, u8> },
    /// Something refused, for the viewer's log.
    Report { error: PumpError<'a> },
    /// The idle expiry passed: stop serving.
    Stop,
}

/// What the core refused, by what it is about.
#[derive(Debug)]
pub enum PumpError<'a> {
    /// `<stem>.telem` did not attach in `tries` tries; `why` is the last refusal.
    AttachRefused { stem: AString<'a>, tries: u32, why: AString<'a> },
    /// Run `stem`'s process `pid` could not be watched for its exit: `errno`. The run stays
    /// live (nothing says it ended), so the viewer does not expire while it is listed.
    WatchRefused { stem: AString<'a>, pid: u32, errno: u16, name: &'static str },
    /// The runs directory holds more runs than [`Config::runs_most`]: `stem` is not tracked.
    TooManyRuns { stem: AString<'a> },
    /// ... or more builds than [`Config::builds_most`]: `name` is not read.
    TooManyBuilds { name: AString<'a> },
}

impl core::fmt::Display for PumpError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PumpError::AttachRefused { stem, tries, why } => {
                write!(f, "blaze viewer: {stem}.telem did not attach in {tries} tries: {why}")
            }
            PumpError::WatchRefused { stem, pid, errno, name } => {
                write!(f, "blaze viewer: run {stem}: cannot watch process {pid} for its exit: {name} ({errno})")
            }
            PumpError::TooManyRuns { stem } => write!(f, "blaze viewer: run {stem}: over the bound of runs tracked"),
            PumpError::TooManyBuilds { name } => write!(f, "blaze viewer: build {name}: over the bound of builds read"),
        }
    }
}

struct Run<'a, R> {
    id: AString<'a>,
    meta: RunMeta<'a>,
    ring: R,
    alive: bool,
    /// Its `run_ended` is sent (or there was no viewer to send it to): nothing is drained.
    ended: bool,
    dead_since: Option<Duration>,
}

struct Build<'a> {
    name: AString<'a>,
    modified: u64,
    json: AString<'a>,
}

/// A ring to attach: `tries` made so far, the next at `at` (`None`: an [`Action::Attach`] is
/// out).
struct Attaching<'a> {
    stem: AString<'a>,
    tries: u32,
    at: Option<Duration>,
}

/// The listing in flight: none, one, or one and another wanted after it (a change seen while
/// one was out, however many: they coalesce).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Listing {
    Idle,
    Out,
    OutAgain,
}

/// The core. `R` is the ring the driver attaches.
pub struct Pump<'a, R: Ring> {
    alloc: Alloc<'a>,
    config: Config,
    now: Duration,
    /// By id.
    runs: AVec<'a, Run<'a, R>>,
    /// By name.
    builds: AVec<'a, Build<'a>>,
    viewers: AVec<'a, ConnId>,
    attaching: AVec<'a, Attaching<'a>>,
    listing: Listing,
    next_tick: Option<Duration>,
    idle_since: Option<Duration>,
    ticks: u32,
    stopped: bool,
    actions: AVec<'a, Action<'a>>,
    scratch: AVec<'a, u8>,
}

impl<'a, R: Ring> Pump<'a, R> {
    /// A viewer that knows nothing yet at `now`: its first action lists the directory.
    pub fn new(config: Config, now: Duration, alloc: Alloc<'a>) -> Pump<'a, R> {
        let mut pump = Pump {
            alloc,
            config,
            now,
            runs: AVec::new_in(alloc),
            builds: AVec::new_in(alloc),
            viewers: AVec::new_in(alloc),
            attaching: AVec::new_in(alloc),
            listing: Listing::Idle,
            next_tick: None,
            idle_since: None,
            ticks: 0,
            stopped: false,
            actions: AVec::new_in(alloc),
            scratch: AVec::new_in(alloc),
        };
        pump.dir_changed();
        pump.settle();
        pump
    }

    /// The actions asked since the last take, in order.
    pub fn take_actions(&mut self) -> AVec<'a, Action<'a>> {
        core::mem::replace(&mut self.actions, AVec::new_in(self.alloc))
    }

    /// Whether actions are waiting to be taken.
    pub fn has_actions(&self) -> bool {
        !self.actions.is_empty()
    }

    /// When the driver must call [`Pump::advance`] next, if nothing else happens first:
    /// `None` for never (a viewer is connected and no ring is live).
    pub fn next_deadline(&self) -> Option<Duration> {
        let mut next = self.next_tick;
        let mut take = |at: Duration| next = Some(next.map_or(at, |next: Duration| next.min(at)));
        for attaching in self.attaching.iter() {
            if let Some(at) = attaching.at {
                take(at);
            }
        }
        for run in self.runs.iter() {
            if let Some(dead) = run.dead_since {
                take(dead.saturating_add(self.config.collect_after));
            }
        }
        if let Some(idle) = self.idle_since
            && !self.stopped
        {
            take(idle.saturating_add(self.config.idle_expiry));
        }
        next
    }

    /// Whether the drain tick is armed (the one timer): the simulation's question (the driver
    /// asks [`Pump::next_deadline`]).
    #[cfg(test)]
    pub fn ticking(&self) -> bool {
        self.next_tick.is_some()
    }

    /// The viewers connected.
    pub fn viewers(&self) -> usize {
        self.viewers.len()
    }

    /// Whether it asked to stop.
    pub fn stopped(&self) -> bool {
        self.stopped
    }

    /// The clock is at `now`: what is due runs (attach retries, the tick, collection, expiry).
    pub fn advance(&mut self, now: Duration) {
        self.now = self.now.max(now);
        let now = self.now;
        for attaching in self.attaching.iter_mut() {
            if attaching.at.is_some_and(|at| at <= now) {
                attaching.at = None;
                self.actions.push(Action::Attach { stem: attaching.stem.clone_in(self.alloc) });
            }
        }
        if self.next_tick.is_some_and(|at| at <= now) {
            self.tick();
        }
        let collect_after = self.config.collect_after;
        let mut collected = false;
        let mut at = 0;
        while let Some(run) = self.runs.get(at) {
            if run.dead_since.is_some_and(|dead| dead.saturating_add(collect_after) <= now) {
                let run = self.runs.remove_shift(at);
                self.actions.push(Action::Remove { stem: run.id });
                collected = true;
            } else {
                at += 1;
            }
        }
        if collected {
            self.broadcast_runs();
        }
        self.settle();
    }

    /// Viewer `conn` connected (its handshake is out): it is sent the runs and the builds. The
    /// first viewer drops what the rings hold, so a view starts live, as the viewer always
    /// has (with nobody watching the events went nowhere).
    pub fn viewer_joined(&mut self, conn: ConnId) {
        if self.viewers.is_empty() {
            for run in self.runs.iter_mut().filter(|run| !run.ended) {
                let cap = usize::try_from(run.ring.stats().cap).unwrap_or(usize::MAX);
                let mut dropped = 0;
                // At most a ring's worth: a writer faster than the drop cannot hold it here.
                while dropped < cap {
                    self.scratch.clear();
                    let took = run.ring.drain_into(self.config.drain_most, &mut self.scratch);
                    dropped += took;
                    if took < self.config.drain_most {
                        break;
                    }
                }
                self.scratch.clear();
            }
        }
        self.viewers.push(conn);
        let runs = self.runs_json();
        self.actions.push(Action::SendText { to: conn, text: runs });
        let builds = self.builds_json();
        self.actions.push(Action::SendText { to: conn, text: builds });
        self.settle();
    }

    /// Viewer `conn` is gone.
    pub fn viewer_left(&mut self, conn: ConnId) {
        self.viewers.retain(|&viewer| viewer != conn);
        self.settle();
    }

    /// The runs directory changed: one listing is asked, or, with one out, one more after it.
    pub fn dir_changed(&mut self) {
        self.listing = match self.listing {
            Listing::Idle => {
                self.actions.push(Action::ListDir);
                Listing::Out
            }
            Listing::Out | Listing::OutAgain => Listing::OutAgain,
        };
    }

    /// The listing asked: new and changed builds are read, new rings attached.
    pub fn listed(&mut self, entries: &[Listed<'_>]) {
        for entry in entries {
            if entry.name.ends_with(".build.json") {
                let found = self.builds.binary_search_by(|build| build.name.as_str().cmp(entry.name));
                let read = match found {
                    Ok(at) => self.builds.get(at).is_some_and(|build| build.modified != entry.modified),
                    Err(_) if self.builds.len() < self.config.builds_most => true,
                    Err(_) => {
                        let name = AString::from_str_in(entry.name, self.alloc);
                        self.actions.push(Action::Report { error: PumpError::TooManyBuilds { name } });
                        false
                    }
                };
                if read {
                    let name = AString::from_str_in(entry.name, self.alloc);
                    self.actions.push(Action::ReadBuild { name, modified: entry.modified });
                }
                continue;
            }
            let Some(stem) = entry.name.strip_suffix(".telem") else {
                continue;
            };
            if self.run_at(stem).is_ok() || self.attaching.iter().any(|it| it.stem.as_str() == stem) {
                continue;
            }
            let stem = AString::from_str_in(stem, self.alloc);
            if self.runs.len() + self.attaching.len() >= self.config.runs_most {
                self.actions.push(Action::Report { error: PumpError::TooManyRuns { stem } });
                continue;
            }
            self.actions.push(Action::Attach { stem: stem.clone_in(self.alloc) });
            self.attaching.push(Attaching { stem, tries: 0, at: None });
        }
        self.listing = match self.listing {
            Listing::OutAgain => {
                self.actions.push(Action::ListDir);
                Listing::Out
            }
            Listing::Idle | Listing::Out => Listing::Idle,
        };
        self.settle();
    }

    /// The build `name` as read at `modified`: kept and sent when it is one JSON object. A
    /// file read half-written is not kept, and is read again at the next change (the writer,
    /// `own_process.rs`, publishes by rename, so a reader sees all or nothing).
    pub fn build_read(&mut self, name: &str, modified: u64, bytes: &[u8]) {
        let Ok(text) = core::str::from_utf8(bytes) else {
            return;
        };
        let text = text.trim();
        if !(text.starts_with('{') && text.ends_with('}')) {
            return;
        }
        let json = AString::from_str_in(text, self.alloc);
        match self.builds.binary_search_by(|build| build.name.as_str().cmp(name)) {
            Ok(at) => {
                if let Some(build) = self.builds.get_mut(at) {
                    build.modified = modified;
                    build.json = json;
                }
            }
            Err(at) => {
                let name = AString::from_str_in(name, self.alloc);
                self.builds.insert_shift(at, Build { name, modified, json });
            }
        }
        let builds = self.builds_json();
        self.actions.push(Action::BroadcastText { text: builds });
    }

    /// `<stem>.telem` attached as `ring`, its meta `meta`: tracked, announced, and its process
    /// watched.
    pub fn attached(&mut self, stem: &str, ring: R, meta: RunMeta<'a>) {
        self.attaching.retain(|it| it.stem.as_str() != stem);
        let Err(at) = self.run_at(stem) else {
            return;
        };
        if meta.pid != 0 {
            self.actions.push(Action::WatchExit { pid: meta.pid });
        }
        let id = AString::from_str_in(stem, self.alloc);
        self.runs.insert_shift(at, Run { id, meta, ring, alive: true, ended: false, dead_since: None });
        let mut text = AString::new_in(self.alloc);
        text.push_str(r#"{"type":"run_started","run":"#);
        if let Some(run) = self.runs.get(at) {
            write_run(&mut text, run);
        }
        text.push('}');
        self.actions.push(Action::BroadcastText { text });
        self.broadcast_runs();
        self.settle();
    }

    /// `<stem>.telem` did not attach (`why`): tried again after [`Config::attach_wait`], or
    /// reported after [`Config::attach_tries`] (and tried again at the next listing).
    pub fn attach_failed(&mut self, stem: &str, why: &str) {
        let Some(at) = self.attaching.iter().position(|it| it.stem.as_str() == stem) else {
            return;
        };
        let wait = self.config.attach_wait;
        let most = self.config.attach_tries;
        let now = self.now;
        let Some(attaching) = self.attaching.get_mut(at) else {
            return;
        };
        attaching.tries = attaching.tries.saturating_add(1);
        if attaching.tries < most {
            attaching.at = Some(now.saturating_add(wait));
        } else {
            let attaching = self.attaching.remove_shift(at);
            let why = AString::from_str_in(why, self.alloc);
            let error = PumpError::AttachRefused { stem: attaching.stem, tries: attaching.tries, why };
            self.actions.push(Action::Report { error });
        }
        self.settle();
    }

    /// Process `pid` exited: its runs are dead (collected after [`Config::collect_after`]);
    /// with no viewer, ended at once, since nobody is there to be told.
    pub fn exited(&mut self, pid: u32) {
        let now = self.now;
        let alone = self.viewers.is_empty();
        let mut changed = false;
        for run in self.runs.iter_mut().filter(|run| run.alive && run.meta.pid == pid) {
            run.alive = false;
            run.dead_since = Some(now);
            run.ended |= alone;
            changed = true;
        }
        if changed {
            self.broadcast_runs();
        }
        self.settle();
    }

    /// Process `pid` could not be watched (`errno`, named `name`): reported for each of its runs.
    pub fn watch_failed(&mut self, pid: u32, errno: u16, name: &'static str) {
        for run in self.runs.iter().filter(|run| run.meta.pid == pid) {
            let stem = run.id.clone_in(self.alloc);
            self.actions.push(Action::Report { error: PumpError::WatchRefused { stem, pid, errno, name } });
        }
    }

    /// `{"type":"runs","runs":[...]}`, what `/api/runs` answers.
    pub fn runs_json(&self) -> AString<'a> {
        let mut out = AString::new_in(self.alloc);
        out.push_str(r#"{"type":"runs","runs":["#);
        for (at, run) in self.runs.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            write_run(&mut out, run);
        }
        out.push_str("]}");
        out
    }

    /// `{"type":"builds","builds":[...]}`, what `/api/builds` answers.
    pub fn builds_json(&self) -> AString<'a> {
        let mut out = AString::new_in(self.alloc);
        out.push_str(r#"{"type":"builds","builds":["#);
        for (at, build) in self.builds.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            out.push_str(&build.json);
        }
        out.push_str("]}");
        out
    }

    fn run_at(&self, stem: &str) -> Result<usize, usize> {
        self.runs.binary_search_by(|run| run.id.as_str().cmp(stem))
    }

    fn broadcast_runs(&mut self) {
        let text = self.runs_json();
        self.actions.push(Action::BroadcastText { text });
    }

    /// One drain: each run's events to every viewer as one binary frame, a dead run's
    /// `run_ended` once its ring is empty, the runs list every [`Config::runs_every`] ticks.
    fn tick(&mut self) {
        self.ticks = self.ticks.wrapping_add(1);
        let most = self.config.drain_most;
        let mut full = false;
        let mut changed = false;
        for run in self.runs.iter_mut().filter(|run| !run.ended) {
            self.scratch.clear();
            batch_header(&mut self.scratch, &run.id);
            let drained = run.ring.drain_into(most, &mut self.scratch);
            if drained > 0 {
                full |= drained >= most;
                let bytes = AVec::from_slice_in(&self.scratch, self.alloc);
                self.actions.push(Action::BroadcastBinary { bytes });
            } else if !run.alive {
                run.ended = true;
                let mut text = AString::new_in(self.alloc);
                text.push_str(r#"{"type":"run_ended","id":""#);
                push_json_text(&mut text, &run.id);
                text.push_str(r#"","status":0}"#);
                self.actions.push(Action::BroadcastText { text });
                changed = true;
            }
        }
        if changed || self.ticks % self.config.runs_every.max(1) == 0 {
            self.broadcast_runs();
        }
        let next = if full { self.now } else { self.now.saturating_add(self.config.tick) };
        self.next_tick = Some(next);
    }

    /// The tick armed exactly while a viewer watches a run that may have events, and the idle
    /// expiry run.
    fn settle(&mut self) {
        let watched = !self.viewers.is_empty() && self.runs.iter().any(|run| !run.ended);
        if !watched {
            self.next_tick = None;
        } else if self.next_tick.is_none() {
            self.next_tick = Some(self.now);
        }
        let idle = self.viewers.is_empty() && !self.runs.iter().any(|run| run.alive);
        if !idle {
            self.idle_since = None;
            return;
        }
        let since = *self.idle_since.get_or_insert(self.now);
        if !self.stopped && self.now.saturating_sub(since) >= self.config.idle_expiry {
            self.stopped = true;
            self.actions.push(Action::Stop);
        }
    }
}

/// `[B, 1, id length (u16, little-endian), id, zeros to a multiple of 8]`, then the events.
fn batch_header(frame: &mut AVec<'_, u8>, id: &str) {
    let id = id.as_bytes();
    let id = id.get(..usize::from(u16::MAX)).unwrap_or(id);
    let len = u16::try_from(id.len()).unwrap_or(u16::MAX);
    frame.push(BIN_EVENT_MAGIC);
    frame.push(BIN_EVENT_VERSION);
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(id);
    let header = (4 + id.len()).next_multiple_of(8);
    frame.resize(header, 0);
}

/// One run as `/api/runs` lists it.
fn write_run<R: Ring>(out: &mut AString<'_>, run: &Run<'_, R>) {
    let stats = run.ring.stats();
    out.push_str(r#"{"id":""#);
    push_json_text(out, &run.id);
    let _ = write!(out, r#"","pid":{},"package":""#, run.meta.pid);
    push_json_text(out, &run.meta.package);
    let _ = write!(
        out,
        r#"","alive":{},"started_ms":{},"stats":{{"cap":{},"pushed":{},"popped":{},"dropped":{},"shadow_overflows":{},"unwound_spans":{}}},"slots":{{"#,
        run.alive,
        run.meta.started_ms,
        stats.cap,
        stats.pushed,
        stats.popped,
        stats.dropped,
        stats.shadow_overflows,
        stats.unwound_spans,
    );
    for (at, (id, name)) in run.meta.slots.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        let _ = write!(out, r#""{id}":""#);
        push_json_text(out, name);
        out.push('"');
    }
    out.push_str("}}");
}

#[cfg(test)]
#[path = "pump_tests.rs"]
mod tests;
