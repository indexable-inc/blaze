//! The collector's core (AGENTS.md rule 14): a synchronous state machine. It reads time only from
//! [`Collector::advance_clock`], does no IO, and hands back [`Decision`]s
//! ([`Collector::take_decisions`]) and when it next wants the clock ([`Collector::next_deadline`]).
//!
//! A run is: scan (the driver lists every object, [`Collector::scanned`]), mark (what a root or a
//! lease holds, as of the epoch the scan came back at), plan (what is garbage, what a
//! workspace's cap and the disk budget evict), sweep (one rename into trash at a time, each
//! checked against the roots again just before it is asked for), unlink (trash old enough), and
//! a [`RunReport`] ([`Decision::Record`]). Each run is bounded by fuel (renames and unlinks it may
//! ask for) and by a deadline on the injected clock, and ends in an [`Outcome`]: done, or stopped
//! naming the phase, why, and how far it got.
//!
//! Every list it hands out is sorted by construction (rule 25): the same inventory, roots and
//! clock give the same decisions, byte for byte.
//!
//! Runs are paced ([`crate::pace`]): each is a slice, whose steps, bytes and scan width scale
//! with the pressure from each volume's free space, and whose scan walks only the next few
//! build dirs and stores past a cursor kept across slices (a lap). What a lap saw is kept in
//! brief ([`Seen`]), so eviction under pressure goes by least recent use across every workspace,
//! not only the ones this slice walked. A run asked for (`blaze cache gc`) or an urgent one (a
//! build short of space) walks everything at once.

#![blaze::sans_io]

use std::{collections::VecDeque, fmt, ops::Range};

use blaze_hash::HashMap;
use blaze_serde::{Deserialize, Serialize};
use std::alloc::Global;

use crate::pace::{FULL, Hysteresis, Levels, Pace, Slice, Volume};

const SECOND: u64 = 1_000_000_000;
const HOUR: u64 = 3600 * SECOND;
const DAY: u64 = 24 * HOUR;
const GIB: u64 = 1 << 30;

/// How far the wall clock may run ahead of the boot clock since the collector first read both
/// before it is taken as set forward ([`Collector::advance_clocks`]).
const CLOCK_SLACK_NS: u64 = 300 * SECOND;

/// How long, on the boot clock, the wall clock must stay a steady distance ahead of what the
/// boot clock allows before it is taken as right and the clock re-anchored to it (review N3):
/// an NTP step after a VM was paused, which the guest's boot clock did not count.
pub const REANCHOR_NS: u64 = HOUR;

/// The collector's clock: the wall clock (records are stamped in it) held within
/// [`CLOCK_SLACK_NS`] of what the boot clock says has passed since its anchor. `Copy`: the
/// driver reads the clock the collector set its deadlines by ([`crate::fs::Gate`], a scan's
/// stop), never the raw wall clock, so a step of the wall clock cannot cancel every rename
/// (review N3).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Clock {
    /// The wall and boot clocks as anchored: as first read, or as last re-anchored.
    anchor: Option<(u64, u64)>,
    /// How far ahead of its bound the wall clock is, and the boot time it was first seen that
    /// far ahead: steady for [`REANCHOR_NS`], the clock is re-anchored to it.
    ahead: Option<(u64, u64)>,
}

/// What [`Clock::advance`] read.
struct Reading {
    now: u64,
    /// How far the wall clock is ahead of the bound, or behind it; 0 within.
    ahead_ns: u64,
    behind_ns: u64,
    /// The clock was re-anchored to the wall clock by this reading.
    reanchored: bool,
}

impl Clock {
    /// The time as this clock reads it: `wall_ns` held to its anchor's bounds; `wall_ns` itself
    /// with no boot clock, or before the collector's first reading.
    pub fn read(&self, wall_ns: u64, boot_ns: Option<u64>) -> u64 {
        match (self.anchor, boot_ns) {
            (Some(anchor), Some(boot)) => bounded(anchor, wall_ns, boot).0,
            _ => wall_ns,
        }
    }

    /// The driver's own clocks read through this one ([`crate::record::now_ns`],
    /// [`crate::record::boot_ns`]).
    pub fn now(&self) -> u64 {
        self.read(crate::record::now_ns(), crate::record::boot_ns())
    }

    fn advance(&mut self, wall_ns: u64, boot_ns: Option<u64>) -> Reading {
        let Some(boot) = boot_ns else {
            return Reading { now: wall_ns, ahead_ns: 0, behind_ns: 0, reanchored: false };
        };
        let anchor = *self.anchor.get_or_insert((wall_ns, boot));
        let (now, ahead_ns, behind_ns) = bounded(anchor, wall_ns, boot);
        if ahead_ns == 0 {
            self.ahead = None;
            return Reading { now, ahead_ns, behind_ns, reanchored: false };
        }
        match self.ahead {
            // As far ahead as it was, within the slack, for long enough: it is right.
            Some((was, since)) if was.abs_diff(ahead_ns) <= CLOCK_SLACK_NS => {
                if boot.saturating_sub(since) >= REANCHOR_NS {
                    self.anchor = Some((wall_ns, boot));
                    self.ahead = None;
                    return Reading { now: wall_ns, ahead_ns, behind_ns: 0, reanchored: true };
                }
            }
            _ => self.ahead = Some((ahead_ns, boot)),
        }
        Reading { now, ahead_ns, behind_ns: 0, reanchored: false }
    }
}

/// `wall_ns` held within [`CLOCK_SLACK_NS`] of `anchor`'s wall time plus what the boot clock
/// (`boot_ns`) says passed since it, and how far ahead of that it was or behind it. Behind is
/// bounded too: a clock set back would otherwise hold every deadline back by as much
/// (AGENTS.md rule 18). A clock set back is never re-anchored to: the collector would wait out
/// the step with a run under way.
fn bounded((wall, at): (u64, u64), wall_ns: u64, boot_ns: u64) -> (u64, u64, u64) {
    let expected = wall.saturating_add(boot_ns.saturating_sub(at));
    let high = expected.saturating_add(CLOCK_SLACK_NS);
    let low = expected.saturating_sub(CLOCK_SLACK_NS);
    if wall_ns > high {
        (high, wall_ns - high, 0)
    } else if wall_ns < low {
        (low, 0, low - wall_ns)
    } else {
        (wall_ns, 0, 0)
    }
}

/// An area's trash: `<area>/.blaze-trash`, which holds a marker saying it is blaze's
/// ([`crate::fs::trash_dir`]). Its entries are named `<20 digits>-<6 letters or digits>-<name>`
/// ([`crate::fs::is_trash_name`]); nothing else in it is touched.
pub const TRASH: &str = ".blaze-trash";

/// The reports a collector keeps (and a [`crate::Ledger`] persists).
pub const REPORTS_KEPT: usize = 16;

/// What the collector manages and how hard it collects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The directories whose objects it collects, absolute; area `i`'s trash is
    /// `<areas[i]>/.blaze-trash`, on the same file system as its objects. The first is blaze's
    /// root, whose `build/` is listed; the last is the machine's shared directory, whose `std/`
    /// is (one area when they are the same).
    pub areas: Vec<String>,
    /// Free space on each area's volume, and blaze's share of it, from which pressure comes.
    pub levels: Levels,
    /// How slices scale with pressure.
    pub pace: Pace,
    /// Nothing is collected until it was unused this long: a directory just made, whose record
    /// is not written yet, stays.
    pub grace_ns: u64,
    /// A workspace's build dir, or a standard library's store no daemon holds, unused this long
    /// is collected.
    pub ttl_ns: u64,
    /// A workspace whose checkout is gone is collected only once it was unused this long: a
    /// checkout moved away for a while (a branch switch, a remount) keeps its build.
    pub gone_grace_ns: u64,
    /// A workspace's profiles together may hold this many bytes; past it the least recently used
    /// are evicted first.
    pub workspace_cap_bytes: u64,
    /// The profiles of the last this many transactions (builds) of each workspace are kept
    /// against its cap, and yield to the disk budget last.
    pub keep_transactions: usize,
    /// A run at least this often.
    pub period_ns: u64,
    /// A run once the daemon has been idle this long (once each idle stretch).
    pub idle_after_ns: u64,
    /// How long a build that ran out of space waits for an urgent run to end before it fails,
    /// at least ([`Collector::hold_for_space`]).
    pub hold_ns: u64,
    /// A live head keeps its workspace's build dir, and of its profiles those used within this
    /// long or by the workspace's last build; the cap and the budget apply to the rest (review
    /// N2): a daemon that watches a checkout for weeks does not keep every profile it ever built.
    pub head_recent_ns: u64,
    /// The renames and unlinks one run asked for (or urgent) may ask for; a slice asks for its
    /// own, fewer ([`Pace`]).
    pub fuel: u32,
    /// A run asked for (or urgent) stops this long after it started, wherever it is; a slice
    /// stops after its own, shorter ([`Pace::slice_ns`]).
    pub run_ns: u64,
    /// Trash is unlinked once it has been there this long; 0 unlinks a run's own trash in the
    /// same run, after every rename.
    pub trash_after_ns: u64,
}

impl Config {
    /// The daemon's settings: the default levels (comfortable at max(15%, 30 GiB) free, the
    /// floor at max(10%, 20 GiB), half the volume blaze's at most) and pace, an hour's grace (a
    /// day for a checkout gone), 14 days unused, 16 GiB a workspace, the last 4 transactions, a
    /// lap every 6 hours or after 5 idle minutes, 4096 renames or unlinks and 10 minutes a run
    /// asked for (it walks every file of every build dir), and two minutes a build out of space
    /// waits for an urgent run.
    pub fn daemon(areas: Vec<String>) -> Config {
        Config {
            areas,
            levels: Levels::default(),
            pace: Pace::default(),
            grace_ns: HOUR,
            ttl_ns: 14 * DAY,
            gone_grace_ns: DAY,
            workspace_cap_bytes: 16 * GIB,
            keep_transactions: 4,
            period_ns: 6 * HOUR,
            idle_after_ns: 300 * SECOND,
            hold_ns: 120 * SECOND,
            head_recent_ns: DAY,
            fuel: 4096,
            run_ns: 600 * SECOND,
            trash_after_ns: 0,
        }
    }
}

/// What an object is. The driver lists nothing else, so nothing else is ever collected.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Class {
    /// A workspace's build dir, `<home>/build/<workspace id>`: its own files (bytes), less its
    /// profiles, which are objects of their own.
    Workspace,
    /// A profile's directory in a workspace's build dir (`debug`, `release`, ...).
    Profile,
    /// The shared standard library's store of every blaze whose code making records is one,
    /// `<machine>/std/<makers>` (blaze-cargo's `serve::RecordMakers`).
    Std,
    /// A temporary a writer left (a name ending `.tmp`), in an area or a build dir.
    Temp,
    /// An entry of an area's trash.
    Trash,
}

/// A workspace's checkout as its record names it, and whether it was there at the scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkout {
    pub path: String,
    pub exists: bool,
}

/// One object the scan found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    /// Absolute; `/`-separated. A profile's path is under its workspace's.
    pub path: String,
    /// Its index in [`Config::areas`].
    pub area: u16,
    pub class: Class,
    /// On disk, of it alone (a workspace's less its profiles').
    pub bytes: u64,
    /// When it was last used: its record's, else its modification time; a trash entry's is
    /// when it was moved there, from its name.
    pub used_ns: u64,
    /// A profile's workspace (its parent's path).
    pub workspace: Option<String>,
    /// A workspace's checkout, from its record.
    pub checkout: Option<Checkout>,
    /// Whether a shared lock on its `.blaze-lease` was held when it was scanned.
    pub leased: bool,
    /// Why it is never collected: no lease file, no record or one of a format whose users take
    /// no lease, a record that does not read. Set by the driver.
    pub refused: Option<String>,
}

/// What a scan found, sorted by path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inventory {
    pub objects: Vec<Object>,
    /// What the scan could not read, each naming the path: reported, and what it is about is
    /// not listed, so not collected.
    pub problems: Vec<String>,
    /// The cursor: the last unit (a build dir or a store) this scan walked when more remain
    /// past it, for the next slice to go on from; `None` once it walked to the end.
    pub next: Option<String>,
}

/// What a lap saw of one object, kept until the cursor comes round to it again: enough to find
/// the least recently used across every workspace while a slice walks only a few.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    /// The unit (build dir or store) it is of: where the cursor finds it again.
    pub unit: String,
    pub area: u16,
    pub class: Class,
    pub bytes: u64,
    pub used_ns: u64,
    /// What the budget may evict of it: `Fresh`, `Transaction`, or nothing (`None`).
    pub evictable: Option<Keep>,
}

/// One volume as a run saw it: for `blaze cache gc --dry-run` and status.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct VolumeReport {
    /// The first area on it.
    pub area: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// Free space at which pressure is 0, and at which it is full.
    pub comfortable_bytes: u64,
    pub floor_bytes: u64,
    /// blaze's bytes on it, and its share.
    pub blaze_bytes: u64,
    pub share_bytes: u64,
    /// Pressure from it, permille, and what is to be freed for it to be comfortable again.
    pub pressure: u32,
    pub to_free_bytes: u64,
}

/// A volume still below its floor after an urgent run: what holds the space, so a build that
/// would fail for want of it says why (docs/gc.md, "Pacing").
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct NoSpace {
    pub area: String,
    pub free_bytes: u64,
    pub floor_bytes: u64,
    /// The largest objects kept, each with what keeps it (a lease, a head, a pin), largest first.
    pub kept: Vec<(String, u64, Keep)>,
    /// Areas this collector did not collect, each with who holds it (another daemon,
    /// `blaze cache gc`; [`Collector::skipped`]): what is there was not looked at (review N15).
    #[serde(default)]
    pub skipped: Vec<String>,
}

impl fmt::Display for NoSpace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the volume of {} has {} bytes free, below its floor of {}, after collecting all that nothing holds",
            self.area, self.free_bytes, self.floor_bytes
        )?;
        if self.kept.is_empty() {
            write!(f, "; nothing of blaze's that was collected holds the rest")?;
        } else {
            write!(f, "; what holds blaze's bytes:")?;
            for (path, bytes, keep) in &self.kept {
                write!(f, " {path} ({bytes} bytes, {keep:?});")?;
            }
        }
        if !self.skipped.is_empty() {
            write!(
                f,
                "; not collected, as another collector holds them: {}",
                self.skipped.join(", ")
            )?;
        }
        Ok(())
    }
}

/// How a build held for space ([`Collector::hold_for_space`]) is let go.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Released {
    /// The urgent run ended and a volume is still below its floor, as measured after it: what
    /// holds the space.
    NoSpace(NoSpace),
    /// The urgent run freed space, and the build's volume (every volume, when it is not known)
    /// is above its floor again, as measured since it began: the build may be run again. Let go
    /// as soon as that is measured, not only once the run ends (review N5).
    Freed { run: u64, freed_bytes: u64 },
    /// No urgent run ended within the hold's bound ([`Collector::hold_for_space`]); `area`
    /// is the build's, when it is known.
    TimedOut { waited_ns: u64, area: Option<String> },
    /// The urgent run's scan failed, naming why: nothing was freed, and waiting longer would
    /// not free any (review N7).
    ScanFailed { run: u64, why: String },
}

impl fmt::Display for Released {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Released::NoSpace(short) => write!(f, "{short}"),
            Released::Freed { run, freed_bytes } => write!(
                f,
                "gc run {run} freed {freed_bytes} bytes, as measured since it began, and the build's volume is above its floor again: run the build again"
            ),
            Released::TimedOut { waited_ns, area } => {
                write!(
                    f,
                    "no urgent gc run ended within {} ms of the build running out of space",
                    waited_ns / 1_000_000
                )?;
                match area {
                    Some(area) => write!(f, " on the volume of {area}"),
                    None => Ok(()),
                }
            }
            Released::ScanFailed { run, why } => {
                write!(f, "the urgent gc run {run} could not scan, so it freed nothing: {why}")
            }
        }
    }
}

/// How soon the driver should do the collector's decisions: what the daemon's pool queue they
/// go in ([`Collector::priority`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Priority {
    /// Only when nothing else is queued: a slice paced below [`Pace::busy_from`].
    Background,
    /// With builds: a slice paced at or above [`Pace::busy_from`], or a run asked for.
    Normal,
    /// Ahead of builds: an urgent run, which a build out of space waits for.
    Critical,
}

/// Why a run started.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trigger {
    /// A volume's free space is below comfortable, or blaze's bytes near its share ([`Levels`]).
    Pressure,
    /// A build is short of space: one run walks everything, its caps lifted.
    Urgent,
    /// The daemon was idle for [`Config::idle_after_ns`].
    Idle,
    /// [`Config::period_ns`] passed since the last run.
    Period,
    /// `blaze cache gc`, or [`Collector::ask`].
    Asked,
}

/// What the collector asks of its driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// List the objects of the areas: every trash entry, and the build dirs and stores (in path
    /// order) past `after`, at most `dirs` of them, and none past the one being walked when the
    /// clock reaches `until_ns` (a slice's scan stops there, naming the cursor); answer with
    /// [`Collector::scanned`].
    Scan { run: u64, after: Option<String>, dirs: u32, until_ns: Option<u64> },
    /// Take `path`'s lease exclusively without waiting, then rename `path` to `to` (in its
    /// area's trash); answer with [`Collector::trashed`].
    Trash { run: u64, path: String, to: String, class: Class },
    /// Remove `path`, an entry of trash, whole; answer with [`Collector::unlinked`].
    Unlink { run: u64, path: String },
    /// A run ended: keep its report (the ledger) and show it (status).
    Record { report: RunReport },
}

/// How a [`Decision::Trash`] went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Swept {
    Moved,
    /// Its lease was held: something uses it.
    Leased,
    /// The lease of `dir`, a build dir inside it, was held: a build uses that (review round 5,
    /// B1).
    NestedLeased {
        dir: String,
    },
    /// It was renamed into trash, but a lease inside was then found held and the rename back
    /// failed (`why` names both): it stays in trash, and no unlink takes it while that lease is
    /// held (review round 8, M1).
    LeftInTrash {
        why: String,
    },
    /// It was not there, or the lease taken was no longer its own (it was renamed away).
    Gone,
    /// A lease was taken in this process after the rename was asked for: the driver's gate
    /// ([`crate::fs::Gate`]) did not rename it.
    Rooted,
    /// The run had ended or passed its deadline by the time the rename's turn came.
    Cancelled,
    /// The lease or the rename failed, naming why.
    Failed(String),
}

/// Why an object goes.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Reason {
    /// A workspace whose checkout is gone.
    CheckoutGone { checkout: String },
    /// A workspace unused for longer than [`Config::ttl_ns`].
    Unused { for_ns: u64 },
    /// A standard library's store no daemon holds, unused for longer than [`Config::ttl_ns`].
    NoDaemon { for_ns: u64 },
    /// A temporary older than the grace.
    Temporary { for_ns: u64 },
    /// A profile past its workspace's cap, least recently used first.
    OverCap { workspace: String, cap_bytes: u64 },
    /// Evicted, least recently used first across every workspace, for its volume's free space
    /// (or blaze's share of it): `to_free_bytes` was what the volume needed.
    OverBudget { to_free_bytes: u64 },
    /// Inside a directory that goes.
    Within { dir: String },
    /// A trash entry old enough to unlink.
    Trash { for_ns: u64 },
}

/// Why an object stays.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Keep {
    /// A live head builds this checkout.
    Head { checkout: String },
    /// Pinned in this process ([`Collector::pin`]).
    Pinned,
    /// An agent holds it ([`Collector::hold`]).
    Held { agent: u64 },
    /// Its lease, or one below it, was held at the scan.
    Leased,
    /// One of the last [`Config::keep_transactions`] of its workspace.
    Transaction,
    /// Used within the grace.
    Grace { left_ns: u64 },
    /// Nothing says it should go: used within the TTL, and the cap and budget hold.
    Fresh,
    /// Never collected, and why.
    Refused { why: String },
    /// Rooted or used after the run marked, before it was swept.
    UsedAfterMark,
    /// Its lease was held when it was to be renamed.
    LeasedAtSweep,
    /// The lease of `dir`, a build dir inside it, was held when it was to be renamed.
    NestedLeasedAtSweep { dir: String },
    /// Renamed into trash, then not back for a build that leased a dir inside it
    /// ([`Swept::LeftInTrash`]): no unlink of this run takes it.
    LeftInTrash { why: String },
    /// It was gone when it was to be renamed.
    Gone,
    /// Its rename or unlink failed.
    Failed { why: String },
    /// The run stopped before it got to it.
    NotReached,
}

/// What became of one object.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Fate {
    Go(Reason),
    Keep(Keep),
}

/// One object's fate in a run, with why.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Verdict {
    pub path: String,
    pub class: Class,
    pub bytes: u64,
    pub used_ns: u64,
    pub fate: Fate,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Scan,
    Mark,
    /// Trash the scan found unlinked, before anything is renamed: the cheapest space first.
    Purge,
    Sweep,
    Unlink,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Stop {
    /// It asked for [`Config::fuel`] renames and unlinks.
    Fuel { fuel: u32 },
    /// [`Config::run_ns`] passed.
    Deadline { run_ns: u64 },
    /// The scan failed, naming why.
    ScanFailed { why: String },
    /// Its slice's bytes were asked for: the next slice goes on.
    Slice { bytes: u64 },
    /// The clock is past every build dir's last use by more than the TTL: a clock set forward
    /// would make everything look unused, so nothing is collected by age until a build is.
    Clock { now_ns: u64, newest_ns: u64 },
    /// A run that is not urgent ended at once for an urgent one (a build out of space, a volume
    /// at its floor), which starts in the same step (review N1): a slice's answer queued behind
    /// builds never keeps a held build waiting.
    Preempted { by: Trigger },
}

/// How a run ended.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    Done,
    /// In `phase`, for `why`, with `done` of its steps there done and `left` not.
    Stopped {
        phase: Phase,
        why: Stop,
        done: u32,
        left: u32,
    },
}

/// Whether a run stopped because the clock looked set forward.
fn clock_stop(outcome: &Outcome) -> bool {
    matches!(outcome, Outcome::Stopped { why: Stop::Clock { .. }, .. })
}

/// What a run did.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct RunReport {
    pub run: u64,
    pub why: Trigger,
    /// Planned only: nothing was renamed or unlinked.
    pub dry_run: bool,
    pub started_ns: u64,
    pub ended_ns: u64,
    pub outcome: Outcome,
    /// Objects the scan found, and their bytes (trash included).
    pub objects: u64,
    pub total_bytes: u64,
    /// Held by a root or a lease at the mark.
    pub marked_objects: u64,
    pub marked_bytes: u64,
    /// Moved to trash (for a dry run: would be).
    pub swept_objects: u64,
    pub swept_bytes: u64,
    /// Not swept, trash aside.
    pub kept_objects: u64,
    pub kept_bytes: u64,
    pub unlinked_objects: u64,
    pub unlinked_bytes: u64,
    /// What its volumes still needed freed once what was planned went; 0 when free space is
    /// comfortable.
    pub over_budget_bytes: u64,
    /// The pressure it was paced for, permille.
    #[serde(default)]
    pub pressure: u32,
    #[serde(default)]
    pub volumes: Vec<VolumeReport>,
    /// An urgent run that left a volume below its floor: what holds it.
    #[serde(default)]
    pub short: Option<NoSpace>,
    /// Its scan came back and the clock check let it through: only such a run vouches for the
    /// clock ([`Collector::restore`]). A ledger from before this field reads as unchecked.
    #[serde(default)]
    pub clock_checked: bool,
    pub problems: Vec<String>,
    /// Every object's fate, sorted by path. Left empty in the ledger.
    pub verdicts: Vec<Verdict>,
}

impl fmt::Display for RunReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let verb = if self.dry_run { "would sweep" } else { "swept" };
        write!(
            f,
            "gc run {} ({:?}): {verb} {} objects ({} bytes), kept {} ({} bytes, {} marked live), unlinked {} ({} bytes) of {} objects ({} bytes)",
            self.run,
            self.why,
            self.swept_objects,
            self.swept_bytes,
            self.kept_objects,
            self.kept_bytes,
            self.marked_objects,
            self.unlinked_objects,
            self.unlinked_bytes,
            self.objects,
            self.total_bytes,
        )?;
        match &self.outcome {
            Outcome::Done => {}
            Outcome::Stopped { phase, why, done, left } => {
                write!(f, "; stopped in {phase:?} ({why:?}) with {done} done and {left} left")?;
            }
        }
        if self.pressure > 0 {
            write!(f, "; pressure {}‰", self.pressure)?;
        }
        if self.over_budget_bytes > 0 {
            write!(
                f,
                "; still {} bytes to free for comfortable free space",
                self.over_budget_bytes
            )?;
        }
        if let Some(short) = &self.short {
            write!(f, "; {short}")?;
        }
        Ok(())
    }
}

impl RunReport {
    /// The report as `blaze cache gc` prints it, whether it ran there or in the daemon (review
    /// N14): its line, then each volume's free space, the levels pressure goes between and what
    /// is to be freed, then each object's fate, a line each.
    pub fn details(&self) -> String {
        let mut out = format!("{self}\n");
        for volume in &self.volumes {
            out.push_str(&format!(
                "volume of {}: {} free of {}; comfortable at {} free, floor at {}; blaze holds {} (share {}); pressure {}\u{2030}, {} to free\n",
                volume.area,
                size_text(volume.free_bytes),
                size_text(volume.total_bytes),
                size_text(volume.comfortable_bytes),
                size_text(volume.floor_bytes),
                size_text(volume.blaze_bytes),
                size_text(volume.share_bytes),
                volume.pressure,
                size_text(volume.to_free_bytes),
            ));
        }
        for problem in &self.problems {
            out.push_str(&format!("problem: {problem}\n"));
        }
        for verdict in &self.verdicts {
            out.push_str(&format!(
                "{:>10}  {}  {}\n",
                size_text(verdict.bytes),
                verdict.path,
                serde_json::to_string(&verdict.fate)
                    .unwrap_or_else(|error| format!("(unprintable: {error})"))
            ));
        }
        out
    }
}

/// `bytes` as `3.4MiB`.
pub fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes}B"),
        _ => format!("{value:.1}{}", UNITS[unit]),
    }
}

/// A pin's handle ([`Collector::pin`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Pin(u64);

/// One step of a run's sweep or unlink.
#[derive(Clone, Debug)]
struct Step {
    /// Its verdict's place in [`Run::verdicts`].
    at: usize,
    path: String,
    area: u16,
    class: Class,
    checkout: Option<String>,
    /// A profile a head of its checkout keeps ([`Collector::head_keeps`]), as planned.
    recent: bool,
    /// Where a sweep's step was asked to go.
    trash: Option<String>,
    /// What it frees: its own bytes and those inside it that go with it.
    bytes: u64,
}

struct Run {
    id: u64,
    why: Trigger,
    dry_run: bool,
    started_ns: u64,
    /// No step is asked for past this; a scan that comes back later moves it on a little, so
    /// the slice still does something.
    deadline_ns: u64,
    /// An answer not back by this ends the run ([`Stop::Deadline`]): a scan or rename that hangs.
    hard_ns: u64,
    /// Its scan came back and the clock check let it through.
    clock_ok: bool,
    phase: Phase,
    /// The epoch the mark was made at: a root or use stamped later keeps what it names.
    marked_at: u64,
    verdicts: Vec<Verdict>,
    /// The scan's trash, unlinked first ([`Phase::Purge`]).
    purge: Vec<Step>,
    sweep: Vec<Step>,
    unlink: Vec<Step>,
    /// Its bounds; for a run asked for or urgent, the config's.
    slice: Slice,
    /// A slice of a lap (its scan goes on from the cursor), not a run that walks everything.
    paced: bool,
    /// Its scan's cursor, and whether it walked to the end.
    after: Option<String>,
    lap_end: bool,
    /// Bytes its renames and unlinks were asked to free.
    bytes_asked: u64,
    /// The next step of the phase.
    next: usize,
    /// The step asked for and not yet answered.
    asked: Option<usize>,
    fuel: u32,
    seq: u32,
    total_bytes: u64,
    marked_objects: u64,
    marked_bytes: u64,
    unlinked_objects: u64,
    unlinked_bytes: u64,
    over_budget_bytes: u64,
    problems: Vec<String>,
}

/// The collector: see the module.
pub struct Collector {
    config: Config,
    now: u64,
    /// When the clock was first advanced: the periodic deadline counts from here until a run
    /// has ended.
    first_ns: Option<u64>,
    /// Bumped by every root, pin, hold, transaction and use.
    epoch: u64,
    /// The checkouts live heads build, sorted.
    heads: Vec<String>,
    pins: Vec<(Pin, String)>,
    next_pin: u64,
    /// `(agent, path)`, sorted.
    holds: Vec<(u64, String)>,
    /// Each workspace's last profiles built, oldest first, at most [`Config::keep_transactions`].
    transactions: HashMap<String, VecDeque<String>, Global>,
    /// The epoch each path was last rooted or used at.
    touched: HashMap<String, u64, Global>,
    /// Each area's volume as last measured, less what was written since.
    volumes: Vec<Option<Volume>>,
    hysteresis: Hysteresis,
    /// The pressure slices are paced for now, permille ([`Hysteresis::step`]).
    paced: u32,
    /// What the lap saw, by path; and blaze's bytes in each area from it.
    seen: HashMap<String, Seen, Global>,
    area_bytes: Vec<u64>,
    /// Where the next slice's scan goes on from; whether a lap is under way, why it began, what
    /// it freed so far, and when the last one ended.
    cursor: Option<String>,
    lap_open: bool,
    lap_trigger: Trigger,
    lap_freed: u64,
    last_lap_end_ns: Option<u64>,
    /// An urgent run asked for, and when the last one ended.
    urgent: bool,
    last_urgent_ns: Option<u64>,
    idle_since: Option<u64>,
    idle_ran: bool,
    last_end_ns: Option<u64>,
    /// When the last run that the clock check did not refuse ended, from this collector or the
    /// ledger: a clock set forward while no build dir was found is told from it.
    trusted_end_ns: Option<u64>,
    /// The last lap freed nothing: the next waits a period, whatever the pressure, so a disk
    /// full of what is in use is not walked over and over.
    fruitless: bool,
    /// A run asked for, and whether as a dry run.
    asked: Option<bool>,
    next_run: u64,
    /// The last run that ended ([`Collector::ended_through`]).
    ended_run: u64,
    run: Option<Run>,
    decisions: Vec<Decision>,
    reports: VecDeque<RunReport>,
    /// Runs that ended with a scan or step still out, and until when their answer is waited
    /// for: no slice or run asked for starts while one is (an urgent run does), so two runs'
    /// work never overlaps in the driver.
    stale: Vec<(u64, u64)>,
    /// The signed pressure when the last lap was found fruitless: once it rises a band past
    /// it, the latch is let go.
    fruitless_at: i64,
    /// A lease was let go since the last urgent run: what it held may go now.
    lease_freed: bool,
    /// Free bytes and bytes to free over every volume as the last urgent run started.
    urgent_from: Option<(u64, u64)>,
    /// Builds held for space, each waiting for an urgent run.
    space_holds: Vec<SpaceHold>,
    next_hold: u64,
    released: Vec<(u64, Released)>,
    /// The last urgent run's ending, until every area is measured after it.
    awaiting: Option<Awaiting>,
    /// Each area's measures so far.
    measures: Vec<u64>,
    /// A volume below its floor after an urgent run, as measured after it; let go once no
    /// volume is below its floor. Never restored from a ledger.
    no_space: Option<NoSpace>,
    /// The wall clock held to the boot clock ([`Collector::advance_clocks`]), and how far the
    /// wall clock is ahead of what the boot clock allows, or behind it.
    clock: Clock,
    clock_ahead_ns: u64,
    clock_behind_ns: u64,
    /// Problems the next run's report names (an answer given up on, the clock).
    notes: Vec<String>,
    /// How long the last run that walked everything (asked or urgent) took, start to end: a
    /// hold's bound is sized from it (review N5).
    last_walk_ns: Option<u64>,
    /// The largest objects the last urgent run kept, and what kept them.
    urgent_kept: Vec<(String, u64, Keep)>,
    /// Areas not collected and who holds them ([`Collector::skipped`]).
    skipped: Vec<String>,
    /// Each area's measures as the urgent run under way started.
    urgent_measures: Vec<u64>,
}

/// A build held for space.
struct SpaceHold {
    id: u64,
    /// The area whose volume the build ran out on, when known: it is let go by that volume.
    area: Option<usize>,
    since_ns: u64,
    until_ns: u64,
    /// The first run that may let it go: one that starts after the build ran out.
    after_run: u64,
}

/// An urgent run that ended, waiting for the volumes to be measured after it.
struct Awaiting {
    run: u64,
    measures: Vec<u64>,
    /// The largest objects it kept, and what kept them.
    holders: Vec<(String, u64, Keep)>,
}

/// Whether removing `dir` removes `path`: it is `dir` or under it.
fn covers(dir: &str, path: &str) -> bool {
    path.strip_prefix(dir).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The name of a trash entry for `path`, moved at `moved_ns` by a mover that tags it `tag`:
/// `<20 digits>-<6 letters or digits>-<tail>` ([`crate::fs::is_trash_name`]), the collector's
/// and `blaze clean`'s alike. The tag, in base 36 (36^6 of them), tells movers apart: the
/// collector's names its run and step ([`collector_tag`]), `blaze clean`'s its pid, so two
/// entries moved in the same nanosecond never share a name (review N9).
pub fn trash_name(moved_ns: u64, tag: u64, path: &str) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut left = tag % 36u64.pow(6);
    let mut field = [b'0'; 6];
    for at in (0..6).rev() {
        field[at] = DIGITS[(left % 36) as usize];
        left /= 36;
    }
    let field = std::str::from_utf8(&field).expect("base-36 digits are ASCII");
    format!("{moved_ns:020}-{field}-{}", tail(path))
}

/// The collector's tag of its `seq`th rename in run `run` ([`trash_name`]): the run's number
/// and the step, 36^3 of each, so a step of one run never names what another run's did, though
/// runs share a start time no more (review N9: the step count began again each run).
pub fn collector_tag(run: u64, seq: u32) -> u64 {
    const EACH: u64 = 36 * 36 * 36;
    (run % EACH) * EACH + u64::from(seq) % EACH
}

/// The last part of `path`, kept to letters, digits, `-`, `_` and `.`: a trash entry's name says
/// what it was.
fn tail(path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or(path);
    last.chars()
        .map(
            |it| if it.is_ascii_alphanumeric() || matches!(it, '-' | '_' | '.') { it } else { '_' },
        )
        .collect()
}

impl Collector {
    pub fn new(config: Config) -> Collector {
        let areas = config.areas.len();
        Collector {
            config,
            now: 0,
            first_ns: None,
            epoch: 0,
            heads: Vec::new(),
            pins: Vec::new(),
            next_pin: 0,
            holds: Vec::new(),
            transactions: HashMap::new_in(Global),
            touched: HashMap::new_in(Global),
            volumes: vec![None; areas], // alloc: once per collector, one an area
            hysteresis: Hysteresis::default(),
            paced: 0,
            seen: HashMap::new_in(Global),
            area_bytes: vec![0; areas], // alloc: once per collector, one an area
            cursor: None,
            lap_open: false,
            lap_trigger: Trigger::Period,
            lap_freed: 0,
            last_lap_end_ns: None,
            urgent: false,
            last_urgent_ns: None,
            idle_since: None,
            idle_ran: false,
            last_end_ns: None,
            trusted_end_ns: None,
            fruitless: false,
            asked: None,
            next_run: 1,
            ended_run: 0,
            run: None,
            decisions: Vec::new(),
            reports: VecDeque::new(),
            stale: Vec::new(),
            fruitless_at: 0,
            lease_freed: false,
            urgent_from: None,
            space_holds: Vec::new(),
            next_hold: 1,
            released: Vec::new(),
            awaiting: None,
            measures: vec![0; areas], // alloc: once per collector, one an area
            no_space: None,
            clock: Clock::default(),
            clock_ahead_ns: 0,
            clock_behind_ns: 0,
            notes: Vec::new(),
            last_walk_ns: None,
            urgent_kept: Vec::new(),
            skipped: Vec::new(),
            urgent_measures: Vec::new(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The time, in nanoseconds since the Unix epoch (records are stamped in it). A clock that
    /// goes back is held where it was.
    pub fn advance_clock(&mut self, now: u64) {
        self.advance_clocks(now, None);
    }

    /// The wall clock (ns since the Unix epoch) and, where the driver has one, a boot clock that
    /// counts suspend and is never set (`CLOCK_BOOTTIME`, [`crate::record::boot_ns`]). With the
    /// boot clock, the wall clock is held to within [`CLOCK_SLACK_NS`] of what has elapsed on it
    /// since the anchor ([`Clock`]): a clock set forward while builds run does not make what they
    /// used look old, and one set back does not hold every deadline back. A wall clock that stays
    /// as far ahead for [`REANCHOR_NS`] is taken as right, and the clock re-anchored to it. How
    /// far off it was goes in the next report.
    pub fn advance_clocks(&mut self, wall_ns: u64, boot_ns: Option<u64>) {
        let reading = self.clock.advance(wall_ns, boot_ns);
        let now = reading.now;
        if reading.reanchored {
            // alloc: a problem reported, once a re-anchor
            self.notes.push(format!(
                "the wall clock stayed {} s ahead of the boot clock for {} s: it is taken as right, and the collector's clock re-anchored to it",
                reading.ahead_ns / SECOND,
                REANCHOR_NS / SECOND
            ));
            self.clock_ahead_ns = 0;
        } else if reading.ahead_ns > 0 {
            if self.clock_ahead_ns == 0 {
                // alloc: a problem reported, once a jump
                self.notes.push(format!(
                    "the wall clock is {} s ahead of the boot clock since the collector's clock was anchored: it collects as of {now} ns, not {wall_ns}",
                    reading.ahead_ns / SECOND
                ));
            }
            self.clock_ahead_ns = reading.ahead_ns;
        } else {
            self.clock_ahead_ns = 0;
        }
        if reading.behind_ns > 0 && self.clock_behind_ns == 0 {
            // alloc: a problem reported, once a jump
            self.notes.push(format!(
                "the wall clock is {} s behind the boot clock since the collector's clock was anchored: it collects as of {now} ns, not {wall_ns}",
                reading.behind_ns / SECOND
            ));
        }
        self.clock_behind_ns = reading.behind_ns;
        self.now = self.now.max(now);
        self.first_ns.get_or_insert(self.now);
        self.expire_holds();
        if self.run.is_some() {
            self.pump();
        } else {
            self.maybe_start();
        }
    }

    pub fn now(&self) -> u64 {
        self.now
    }

    /// The clock the collector's deadlines are set by: the driver reads time through a copy
    /// of it as it does a decision ([`crate::fs::Gate`]), never the raw wall clock.
    pub fn clock(&self) -> Clock {
        self.clock
    }

    /// When it next wants [`Collector::advance_clock`]: a run's deadline, a held build's, an
    /// answer given up on, or when the next trigger is due; `None` when nothing can start until
    /// the driver says something changed (the daemon goes idle: [`Collector::idle`]).
    pub fn next_deadline(&self) -> Option<u64> {
        let hold = self.space_holds.iter().map(|it| it.until_ns).min();
        match self.next_start() {
            Some(start) => Some(hold.map_or(start, |it| it.min(start))),
            None => hold,
        }
    }

    fn next_start(&self) -> Option<u64> {
        if let Some(run) = &self.run {
            // Nothing happens before then but an answer, which advances the collector itself.
            return Some(run.hard_ns);
        }
        if self.urgent || (self.asked.is_some() && self.stale.is_empty()) {
            return Some(self.now);
        }
        if let Some(until) = self.stale.iter().map(|it| it.1).min() {
            return Some(until);
        }
        let last = self.last_end_ns.or(self.first_ns)?;
        let slice = self.slice();
        // Below [`Pace::busy_from`] a slice starts only while the daemon is idle: no deadline
        // brings that about.
        if !slice.busy && self.idle_since.is_none() {
            return None;
        }
        // The next slice: its gap after the last, once a lap is due.
        let gap = match self.last_end_ns {
            Some(end) => end.saturating_add(slice.gap_ns),
            None => last,
        };
        if self.lap_open || self.pressing() {
            return Some(gap);
        }
        let lap_from = self.last_lap_end_ns.or(self.first_ns).unwrap_or(last);
        let mut due = lap_from.saturating_add(self.config.period_ns);
        if let Some(since) = self.idle_since
            && !self.idle_ran
        {
            due = due.min(since.saturating_add(self.config.idle_after_ns));
        }
        Some(due.max(gap))
    }

    pub fn take_decisions(&mut self) -> Vec<Decision> {
        std::mem::take(&mut self.decisions)
    }

    /// The last run that ended, as of now: its [`Decision::Record`] may still be in
    /// [`Collector::take_decisions`]. The driver raises its gate's end ([`crate::fs::Gate::ended`])
    /// to it as soon as a call may end a run (a preempt by [`Collector::urgent`] or
    /// [`Collector::hold_for_space`]), so no decision of the ended run still queued starts after
    /// (review round 5, L1).
    pub fn ended_through(&self) -> u64 {
        self.ended_run
    }

    /// Whether a run is under way.
    pub fn running(&self) -> bool {
        self.run.is_some()
    }

    /// The run's own deadline, past which its decisions are not done: the driver's
    /// [`crate::fs::Gate`].
    pub fn run_deadline(&self) -> Option<u64> {
        self.run.as_ref().map(|it| it.hard_ns)
    }

    /// How soon the decisions taken now should be done.
    pub fn priority(&self) -> Priority {
        match &self.run {
            Some(run) if run.slice.urgent => Priority::Critical,
            Some(run) if run.why == Trigger::Asked || run.slice.busy => Priority::Normal,
            _ => Priority::Background,
        }
    }

    /// The last run's report.
    pub fn last_run(&self) -> Option<&RunReport> {
        self.reports.back()
    }

    pub fn reports(&self) -> impl Iterator<Item = &RunReport> {
        self.reports.iter()
    }

    /// Asks for a run now (or after the one under way); a dry run plans and reports, and asks
    /// for no rename or unlink.
    pub fn ask(&mut self, dry_run: bool) {
        self.asked = Some(dry_run);
        if self.run.is_none() {
            self.maybe_start();
        }
    }

    /// The checkouts live heads build now.
    pub fn heads(&mut self, checkouts: &[String]) {
        // alloc: the heads, once per change of them
        let mut heads = checkouts.to_vec();
        heads.sort_unstable();
        heads.dedup();
        if heads != self.heads {
            self.epoch += 1;
            self.heads = heads;
            // What a head kept may be free to go now: a fruitless lap is no reason to wait.
            self.fruitless = false;
        }
    }

    /// Keeps `path` (and every directory it is in) until [`Collector::unpin`].
    pub fn pin(&mut self, path: &str) -> Pin {
        let pin = Pin(self.next_pin);
        self.next_pin += 1;
        self.used(path);
        // alloc: one entry a pin
        self.pins.push((pin, path.to_owned()));
        pin
    }

    pub fn unpin(&mut self, pin: Pin) {
        self.pins.retain(|(it, _)| *it != pin);
    }

    /// What `agent` holds now (what-if roots): each path and every directory it is in stay.
    pub fn hold(&mut self, agent: u64, paths: &[String]) {
        self.holds.retain(|(it, _)| *it != agent);
        for path in paths {
            self.used(path);
            // clone: one entry a path held
            self.holds.push((agent, path.clone()));
        }
        self.holds.sort_unstable();
    }

    pub fn release(&mut self, agent: u64) {
        self.holds.retain(|(it, _)| *it != agent);
    }

    /// A build of `workspace` used `profile` (both build-dir paths): the last
    /// [`Config::keep_transactions`] of each workspace are roots.
    pub fn transaction(&mut self, workspace: &str, profile: &str) {
        self.used(profile);
        let keep = self.config.keep_transactions;
        // alloc: one entry a workspace built
        let last = self.transactions.entry(workspace.to_owned()).or_default();
        last.retain(|it| it != profile);
        // alloc: one entry a profile built, at most `keep`
        last.push_back(profile.to_owned());
        while last.len() > keep {
            last.pop_front();
        }
    }

    /// `path` was used now: what was marked before this does not let it go.
    pub fn used(&mut self, path: &str) {
        self.epoch += 1;
        // Only a run under way marked before this; the next run's mark comes after it.
        if self.run.is_some() {
            // alloc: one entry a path used during a run, dropped as it ends
            self.touched.insert(path.to_owned(), self.epoch);
        }
    }

    /// `bytes` were written under the areas since the volumes were last measured: each
    /// volume's free space is taken as that much less until the next measure.
    pub fn written(&mut self, bytes: u64) {
        for volume in self.volumes.iter_mut().flatten() {
            volume.free_bytes = volume.free_bytes.saturating_sub(bytes);
        }
        self.repace();
    }

    /// Area `area`'s volume as measured now (`statvfs`, [`crate::space::volume`]). The only
    /// way free space goes up: what a run unlinked is not credited, since a file's bytes need
    /// not be freed by its unlink (a reflink, a hard link the store holds), so the measure after
    /// a run says what it freed.
    pub fn volume(&mut self, area: usize, volume: Volume) {
        if let Some(it) = self.volumes.get_mut(area) {
            *it = Some(volume);
        }
        if let Some(it) = self.measures.get_mut(area) {
            *it += 1;
        }
        self.repace();
        self.release_measured();
    }

    /// A build is short of space: one run walks everything with its caps lifted, still never
    /// touching what is leased or rooted; its report says what holds the rest
    /// ([`RunReport::short`]). At most one a [`Pace::slow_gap_ns`], unless the last one freed
    /// space as measured since (at least 1% of what it was to free) or a lease was let go since:
    /// a disk full of what is held is not walked over and over while a build runs.
    pub fn urgent(&mut self) {
        let recent = self
            .last_urgent_ns
            .is_some_and(|it| self.now.saturating_sub(it) < self.config.pace.slow_gap_ns);
        if !recent || self.urgent_freed() || std::mem::take(&mut self.lease_freed) {
            self.urgent = true;
        }
        self.start_urgent();
    }

    /// The urgent run asked for starts now: a run under way that is not urgent ends first
    /// ([`Stop::Preempted`]), its answer still out kept as stale (which an urgent run does not
    /// wait for), so an urgent run waits for no slice's scan queued behind builds (review N1).
    /// One that is urgent already goes on, and the next starts as it ends.
    fn start_urgent(&mut self) {
        if !self.urgent {
            return;
        }
        let Some(run) = &self.run else { return self.maybe_start() };
        if run.slice.urgent {
            return;
        }
        let phase = run.phase;
        let steps = match phase {
            Phase::Scan | Phase::Mark => 1,
            Phase::Purge => run.purge.len(),
            Phase::Sweep => run.sweep.len(),
            Phase::Unlink => run.unlink.len(),
        };
        let done = match phase {
            Phase::Scan | Phase::Mark => 0,
            Phase::Purge | Phase::Sweep | Phase::Unlink => run.next.min(steps),
        };
        let left = (steps - done) as u32;
        let why = Stop::Preempted { by: Trigger::Urgent };
        // `finish` starts the urgent run once this one's report is out.
        self.finish(Outcome::Stopped { phase, why, done: done as u32, left });
    }

    /// Whether free space, as measured now, rose since the last urgent run started by at least
    /// 1% of what it was to free.
    fn urgent_freed(&self) -> bool {
        let Some((free_from, to_free)) = self.urgent_from else { return false };
        let (free, _) = self.free_sum();
        free.saturating_sub(free_from) >= (to_free / 100).max(1)
    }

    /// A lease was let go (a build ended): what it held may go now, so an urgent run need not
    /// wait out the rate limit, and a fruitless lap is no reason to wait.
    pub fn lease_released(&mut self) {
        self.lease_freed = true;
        self.fruitless = false;
    }

    /// A build ran out of space on the volume of area `area` (`None` when it is not known): it
    /// waits, under the returned id, for an urgent run that starts after now to free space on
    /// that volume as measured since it began, or to end and the volumes to be measured after
    /// it; then [`Collector::take_released`] says how it ended. The urgent run is asked for
    /// whatever the rate limit, and ends any run under way that is not urgent at once (review
    /// N1), but for one case (review N4): an urgent run ended within [`Pace::slow_gap_ns`], has
    /// freed nothing as measured since, no lease was let go since, and the volume is still below
    /// its floor; then the hold is let go at once with what holds the space, so a build retried
    /// over and over does not walk everything each time.
    ///
    /// The hold is bounded by twice the last whole walk (asked or urgent), at least
    /// [`Config::hold_ns`] and at most that past [`Config::run_ns`] (review N5): a root whose
    /// walk takes long does not time its holds out before its urgent run can end.
    pub fn hold_for_space(&mut self, area: Option<usize>) -> u64 {
        let id = self.next_hold;
        self.next_hold += 1;
        let area = area.filter(|it| *it < self.config.areas.len());
        let recent = self
            .last_urgent_ns
            .is_some_and(|it| self.now.saturating_sub(it) < self.config.pace.slow_gap_ns);
        let under_way = self.urgent
            || self.awaiting.is_some()
            || self.run.as_ref().is_some_and(|it| it.slice.urgent);
        if recent
            && !under_way
            && !self.urgent_freed()
            && !self.lease_freed
            && let Some(short) = self.short_on(area, &self.urgent_kept)
        {
            self.released.push((id, Released::NoSpace(short)));
            return id;
        }
        let walk = self.last_walk_ns.unwrap_or(0).saturating_mul(2);
        let bound =
            walk.clamp(self.config.hold_ns, self.config.run_ns.saturating_add(self.config.hold_ns));
        self.space_holds.push(SpaceHold {
            id,
            area,
            since_ns: self.now,
            until_ns: self.now.saturating_add(bound),
            after_run: self.next_run,
        });
        self.urgent = true;
        self.lease_freed = false;
        self.start_urgent();
        id
    }

    /// Whether a build is held for space: the driver measures the volumes often while one is
    /// ([`Collector::volume`]), so it is let go as soon as space is freed.
    pub fn holding(&self) -> bool {
        !self.space_holds.is_empty()
    }

    /// The areas this collector does not collect now, each naming who holds it (another
    /// daemon, `blaze cache gc`): a [`NoSpace`] names them, as what is there was not looked at
    /// (review N15).
    pub fn skipped(&mut self, skipped: Vec<String>) {
        self.skipped = skipped;
    }

    /// A problem the driver met (a volume that did not measure), named in the next run's
    /// report.
    pub fn note(&mut self, problem: String) {
        self.notes.push(problem);
    }

    /// The builds held for space that are let go, by their id, in the order they were.
    pub fn take_released(&mut self) -> Vec<(u64, Released)> {
        std::mem::take(&mut self.released)
    }

    /// A volume below its floor after the last urgent run, as measured after it; `None` once
    /// none is below its floor.
    pub fn no_space(&self) -> Option<&NoSpace> {
        self.no_space.as_ref()
    }

    /// The volume of `area` (any volume, for `None`) below its floor as last measured, with
    /// `kept` as what holds the space; `None` when it is above it.
    fn short_on(&self, area: Option<usize>, kept: &[(String, u64, Keep)]) -> Option<NoSpace> {
        let levels = &self.config.levels;
        let short = |volume: &Volume| volume.free_bytes < levels.floor(volume);
        let (at, volume) = match area {
            Some(area) => (area, self.volumes.get(area).copied().flatten().filter(short)?),
            None => self.first_volumes().find(|(_, it)| short(*it)).map(|(at, it)| (at, *it))?,
        };
        Some(NoSpace {
            // clone: the error's own path
            area: self.config.areas[at].clone(),
            free_bytes: volume.free_bytes,
            floor_bytes: levels.floor(&volume),
            kept: kept.to_vec(),           // alloc: each held build's own copy
            skipped: self.skipped.clone(), // clone: each held build's own copy
        })
    }

    /// Holds past their deadline are let go, timed out.
    fn expire_holds(&mut self) {
        let now = self.now;
        let areas = &self.config.areas;
        let released = &mut self.released;
        self.space_holds.retain(|hold| {
            if hold.until_ns > now {
                return true;
            }
            let waited_ns = now.saturating_sub(hold.since_ns);
            // clone: the error's own path
            let area = hold.area.map(|it| areas[it].clone());
            released.push((hold.id, Released::TimedOut { waited_ns, area }));
            false
        });
    }

    /// Once every area is measured after the last urgent run, the holds it answers are let go.
    /// While an urgent run is under way, each hold whose volume it has brought above its floor,
    /// as measured since it began, is let go at once (review N5).
    fn release_measured(&mut self) {
        let (free, _) = self.free_sum();
        let freed_bytes = free.saturating_sub(self.urgent_from.map_or(free, |it| it.0));
        if let Some(run) = self.run.as_ref().filter(|it| it.slice.urgent && it.phase != Phase::Scan)
            && freed_bytes > 0
            && self.measured_since(&self.urgent_measures)
        {
            let run = run.id;
            // alloc: the holds let go early, rarely more than one
            let mut freed: Vec<u64> = Vec::new();
            for hold in &self.space_holds {
                if hold.after_run <= run && self.short_on(hold.area, &[]).is_none() {
                    freed.push(hold.id);
                }
            }
            self.space_holds.retain(|it| !freed.contains(&it.id));
            for id in freed {
                self.released.push((id, Released::Freed { run, freed_bytes }));
            }
        }
        let Some(awaiting) = &self.awaiting else { return };
        if !self.measured_since(&awaiting.measures) {
            return;
        }
        let awaiting = self.awaiting.take().expect("checked above");
        self.no_space = self.short_on(None, &awaiting.holders);
        // alloc: the holds this run answers, rarely more than one
        let mut answered: Vec<(u64, Released)> = Vec::new();
        for hold in &self.space_holds {
            if hold.after_run > awaiting.run {
                continue;
            }
            let why = match self.short_on(hold.area, &awaiting.holders) {
                Some(short) => Released::NoSpace(short),
                None => Released::Freed { run: awaiting.run, freed_bytes },
            };
            answered.push((hold.id, why));
        }
        self.space_holds.retain(|it| answered.iter().all(|(id, _)| *id != it.id));
        self.released.extend(answered);
    }

    /// Whether every area with a volume was measured since the counts `then` were taken.
    fn measured_since(&self, then: &[u64]) -> bool {
        self.measures
            .iter()
            .zip(then)
            .zip(&self.volumes)
            .all(|((now, then), volume)| volume.is_none() || now > then)
    }

    /// The pressure slices are paced for, permille.
    pub fn pressure(&self) -> u32 {
        self.paced
    }

    /// Whether a volume's free space is at or below its floor: what makes a run urgent. Free
    /// space alone, not blaze's share: a share overrun is paced, never urgent.
    pub fn below_floor(&self) -> bool {
        let levels = &self.config.levels;
        self.first_volumes().any(|(_, it)| levels.free_pressure(it) >= i64::from(FULL))
    }

    /// Each volume once, with the first area on it, which speaks for it.
    fn first_volumes(&self) -> impl Iterator<Item = (usize, &Volume)> {
        self.volumes.iter().enumerate().filter_map(|(area, it)| {
            it.as_ref().filter(|it| self.first_area_on(it.id) == area).map(|it| (area, it))
        })
    }

    /// Free bytes, and bytes to free, over every volume.
    fn free_sum(&self) -> (u64, u64) {
        let levels = &self.config.levels;
        self.first_volumes().fold((0u64, 0u64), |(free, to_free), (_, it)| {
            (
                free.saturating_add(it.free_bytes),
                to_free.saturating_add(levels.to_free(it, self.volume_bytes(it.id))),
            )
        })
    }

    /// One volume's signed pressure: from free space, or blaze's share when that is over its
    /// start. The share's pressure is never below 0, so it is taken only when above 0: else it
    /// would hold every volume above comfortable at 0, never past the band below it, and the
    /// hysteresis would never turn off (review C1).
    fn volume_pressure(&self, volume: &Volume) -> i64 {
        let levels = &self.config.levels;
        let free = levels.free_pressure(volume);
        let share = levels.share_pressure(volume, self.volume_bytes(volume.id));
        if share > 0 { free.max(share) } else { free }
    }

    /// The pressure from every volume now, signed ([`Levels::free_pressure`]), the highest.
    fn signed_pressure(&self) -> i64 {
        self.first_volumes()
            .map(|(_, it)| self.volume_pressure(it))
            .max()
            .unwrap_or(-i64::from(FULL))
    }

    fn repace(&mut self) {
        let signed = self.signed_pressure();
        let band = self.config.levels.hysteresis_permille;
        self.paced = self.hysteresis.step(signed, band);
        // Free space fell a band since a lap found nothing to free: there may be now.
        if self.fruitless && signed >= self.fruitless_at.saturating_add(i64::from(band)) {
            self.fruitless = false;
        }
        if !self.below_floor() {
            self.no_space = None;
            self.lease_freed = false;
        }
    }

    fn first_area_on(&self, id: u64) -> usize {
        self.volumes.iter().position(|it| it.is_some_and(|it| it.id == id)).unwrap_or(usize::MAX)
    }

    /// blaze's bytes on the volume `id`: every area on it, as the lap saw them.
    fn volume_bytes(&self, id: u64) -> u64 {
        self.volumes
            .iter()
            .zip(&self.area_bytes)
            .filter(|(it, _)| it.is_some_and(|it| it.id == id))
            .map(|(_, bytes)| *bytes)
            .sum()
    }

    /// Each volume as it stands: free space, the levels, blaze's bytes and what is to be freed.
    pub fn volumes(&self) -> Vec<VolumeReport> {
        let levels = &self.config.levels;
        // alloc: a run's report, one a volume
        let mut out = Vec::new();
        for (area, volume) in self.volumes.iter().enumerate() {
            let Some(volume) = volume else { continue };
            if self.first_area_on(volume.id) != area {
                continue;
            }
            let blaze = self.volume_bytes(volume.id);
            let p = self.volume_pressure(volume);
            out.push(VolumeReport {
                // clone: the report's own path, once a volume
                area: self.config.areas[area].clone(),
                total_bytes: volume.total_bytes,
                free_bytes: volume.free_bytes,
                comfortable_bytes: levels.comfortable(volume),
                floor_bytes: levels.floor(volume),
                blaze_bytes: blaze,
                share_bytes: levels.share(volume),
                pressure: p.max(0) as u32,
                to_free_bytes: levels.to_free(volume, blaze),
            });
        }
        out
    }

    /// The slice pressure asks for now.
    fn slice(&self) -> Slice {
        self.config.pace.slice(self.paced, false)
    }

    /// Whether pressure keeps laps going back to back: on, and the last lap freed something.
    fn pressing(&self) -> bool {
        self.paced > 0 && !self.fruitless
    }

    /// Whether the daemon is idle (no job runs).
    pub fn idle(&mut self, idle: bool) {
        match (idle, self.idle_since) {
            (true, None) => self.idle_since = Some(self.now),
            (false, Some(_)) => {
                self.idle_since = None;
                self.idle_ran = false;
            }
            _ => {}
        }
    }

    /// The periodic deadline and run numbers from a ledger kept by an earlier collector.
    pub fn restore(&mut self, ledger: &crate::Ledger) {
        self.last_end_ns = ledger.last_end_ns.or(self.last_end_ns);
        // Only a run whose scan came back and passed the check vouches for the clock: one whose
        // scan failed, or that ended before it came back, says nothing of it (review C6).
        let trusted = ledger
            .runs
            .iter()
            .filter(|it| it.clock_checked && !clock_stop(&it.outcome))
            .map(|it| it.ended_ns)
            .max();
        self.trusted_end_ns = self.trusted_end_ns.max(trusted);
        self.next_run =
            self.next_run.max(ledger.runs.iter().map(|it| it.run + 1).max().unwrap_or(1));
        self.ended_run = self.ended_run.max(self.next_run - 1);
        self.reports = ledger.runs.iter().cloned().collect();
    }

    /// What to persist: the reports (without their verdicts) and when the last run ended.
    pub fn ledger(&self) -> crate::Ledger {
        crate::Ledger {
            format: crate::ledger::FORMAT,
            last_end_ns: self.last_end_ns,
            runs: self
                .reports
                .iter()
                // alloc: the ledger's own copy, once a run
                .map(|it| RunReport { verdicts: Vec::new(), ..it.clone() })
                .collect(),
        }
    }

    /// The bounds of a run that walks everything: asked for, or urgent (whose pressure is full).
    fn whole(&self, urgent: bool) -> Slice {
        let pressure = if urgent { FULL } else { self.paced };
        Slice {
            pressure,
            steps: self.config.fuel,
            bytes: u64::MAX,
            dirs: u32::MAX,
            slice_ns: self.config.run_ns,
            gap_ns: self.slice().gap_ns,
            busy: true,
            urgent,
        }
    }

    fn maybe_start(&mut self) {
        // Answers given up on: the run they were of is long over, and the next report says so.
        let now = self.now;
        let notes = &mut self.notes;
        self.stale.retain(|(run, until)| {
            if *until > now {
                return true;
            }
            // alloc: a problem reported
            notes.push(format!(
                "run {run}'s last scan or step was never answered; given up at {now} ns"
            ));
            false
        });
        // An urgent run goes first, and does not wait for a stale answer: a build waits for it.
        if std::mem::take(&mut self.urgent) {
            self.urgent_from = Some(self.free_sum());
            // clone: the measures as the urgent run starts, once one
            self.urgent_measures = self.measures.clone();
            let slice = self.whole(true);
            return self.start(Trigger::Urgent, false, slice, false);
        }
        // Anything else waits until the last run's work is answered: two runs' decisions never
        // run in the driver at once.
        if !self.stale.is_empty() {
            return;
        }
        if let Some(dry_run) = self.asked.take() {
            let slice = self.whole(false);
            return self.start(Trigger::Asked, dry_run, slice, false);
        }
        let Some(last) = self.last_end_ns.or(self.first_ns) else { return };
        let slice = self.slice();
        // Slices are spaced by their gap, which shrinks as pressure rises; below
        // [`Pace::busy_from`] one runs only while the daemon is idle.
        if self.last_end_ns.is_some() && self.now.saturating_sub(last) < slice.gap_ns {
            return;
        }
        if !slice.busy && self.idle_since.is_none() {
            return;
        }
        if self.lap_open {
            let why = self.lap_trigger;
            return self.start(why, false, slice, true);
        }
        let lap_from = self.last_lap_end_ns.or(self.first_ns).unwrap_or(last);
        let why = if self.pressing() {
            Trigger::Pressure
        } else if !self.idle_ran
            && self
                .idle_since
                .is_some_and(|it| self.now.saturating_sub(it) >= self.config.idle_after_ns)
        {
            Trigger::Idle
        } else if self.now.saturating_sub(lap_from) >= self.config.period_ns {
            Trigger::Period
        } else {
            return;
        };
        if why == Trigger::Idle {
            self.idle_ran = true;
        }
        self.lap_open = true;
        self.lap_trigger = why;
        self.lap_freed = 0;
        self.cursor = None;
        self.start(why, false, slice, true);
    }

    fn start(&mut self, why: Trigger, dry_run: bool, slice: Slice, paced: bool) {
        let id = self.next_run;
        self.next_run += 1;
        let after = if paced { self.cursor.clone() } else { None }; // clone: the run's own cursor
        let dirs = slice.dirs;
        // A slice's scan stops walking halfway through it, naming its cursor, so the slice has
        // time left to act; a run that walks everything walks it all.
        let until_ns = paced.then(|| self.now.saturating_add(slice.slice_ns / 2));
        self.run = Some(Run {
            id,
            why,
            dry_run,
            started_ns: self.now,
            deadline_ns: self.now.saturating_add(slice.slice_ns),
            hard_ns: self.now.saturating_add(slice.slice_ns.max(self.config.run_ns)),
            clock_ok: false,
            phase: Phase::Scan,
            // The snapshot's epoch: whatever is rooted or used once the scan began is kept,
            // as the scan may have seen it before.
            marked_at: self.epoch,
            verdicts: Vec::new(),
            purge: Vec::new(),
            sweep: Vec::new(),
            unlink: Vec::new(),
            slice,
            paced,
            after: after.clone(), // clone: the decision's copy
            lap_end: false,
            bytes_asked: 0,
            next: 0,
            asked: None,
            fuel: slice.steps.min(self.config.fuel),
            seq: 0,
            total_bytes: 0,
            marked_objects: 0,
            marked_bytes: 0,
            unlinked_objects: 0,
            unlinked_bytes: 0,
            over_budget_bytes: 0,
            problems: Vec::new(),
        });
        self.decisions.push(Decision::Scan { run: id, after, dirs, until_ns });
    }

    /// An answer of run `run`, which is not the one under way: a run that ended with it still
    /// out. Once every such answer is in, the next run may start.
    fn answered_stale(&mut self, run: u64) {
        let before = self.stale.len();
        self.stale.retain(|(it, _)| *it != run);
        if self.stale.len() < before && self.stale.is_empty() && self.run.is_none() {
            self.maybe_start();
        }
    }

    /// What the scan of run `run` found. The mark is made now, at the current epoch.
    pub fn scanned(&mut self, run: u64, found: Result<Inventory, String>) {
        if self.run.as_ref().is_none_or(|it| it.id != run) {
            return self.answered_stale(run);
        }
        let Some(current) = self.run.as_mut().filter(|it| it.id == run && it.phase == Phase::Scan)
        else {
            return;
        };
        // The scan is answered: from here the run waits on nothing of the driver's.
        current.phase = Phase::Mark;
        let inventory = match found {
            Ok(inventory) => inventory,
            Err(why) => {
                return self.finish(Outcome::Stopped {
                    phase: Phase::Scan,
                    why: Stop::ScanFailed { why },
                    done: 0,
                    left: 1,
                });
            }
        };
        current.problems.extend(inventory.problems.iter().cloned());
        current.lap_end = inventory.next.is_none();
        if current.paced {
            // clone: the collector's own cursor, once a slice
            self.cursor = inventory.next.clone();
        }
        // A scan that came back after the slice's deadline (a slow disk, a queue behind builds)
        // still gets a little time to act on what it found, within the run's own bound: the
        // cursor has moved, so the next slice goes on past it either way (review C2).
        if self.now >= current.deadline_ns {
            let more = self.now.saturating_add(current.slice.slice_ns / 2).min(current.hard_ns);
            current.deadline_ns = current.deadline_ns.max(more);
        }
        // A clock set forward makes every build dir look unused: refused while no build dir was
        // used within the TTL of now, of those this scan found and those the lap saw before.
        let build = |class: Class| matches!(class, Class::Workspace | Class::Profile);
        let newest = inventory
            .objects
            .iter()
            .filter(|it| build(it.class) && it.refused.is_none())
            .map(|it| it.used_ns)
            .chain(self.seen.values().filter(|it| build(it.class)).map(|it| it.used_ns))
            .max()
            // No build dir to tell by: the last run the check let through is the reference, so a
            // clock set forward with only std stores left is refused as well.
            .or(self.trusted_end_ns);
        if let Some(newest_ns) = newest
            && self.now > newest_ns.saturating_add(self.config.ttl_ns)
        {
            let why = Stop::Clock { now_ns: self.now, newest_ns };
            return self.finish(Outcome::Stopped { phase: Phase::Mark, why, done: 0, left: 1 });
        }
        self.run.as_mut().expect("the run scanned").clock_ok = true;
        let span = tracing::info_span!(
            target: "blaze",
            "gc mark",
            run,
            objects = inventory.objects.len(),
            marked_objects = tracing::field::Empty,
            marked_bytes = tracing::field::Empty,
            planned_objects = tracing::field::Empty,
            planned_bytes = tracing::field::Empty,
        );
        let _entered = span.enter();
        self.plan(inventory);
        let run = self.run.as_mut().expect("the run planned");
        let planned = run.verdicts.iter().filter(|it| matches!(it.fate, Fate::Go(_)));
        let (planned_objects, planned_bytes) =
            planned.fold((0u64, 0u64), |(n, b), it| (n + 1, b + it.bytes));
        span.record("marked_objects", run.marked_objects);
        span.record("marked_bytes", run.marked_bytes);
        span.record("planned_objects", planned_objects);
        span.record("planned_bytes", planned_bytes);
        if run.dry_run {
            return self.finish(Outcome::Done);
        }
        run.phase = Phase::Purge;
        self.pump();
    }

    /// What keeps `object` as the roots stand now, with `leased` the lease the scan saw on it or
    /// below it, and `recent` whether it is a profile a head keeps ([`Collector::head_keeps`]).
    /// Transaction roots are weak: `None` from the second of the pair.
    fn rooted(
        &self,
        path: &str,
        class: Class,
        checkout: Option<&str>,
        leased: bool,
        recent: bool,
    ) -> Option<Keep> {
        if leased {
            return Some(Keep::Leased);
        }
        // A head keeps its workspace's build dir and its temporaries, and of its profiles those
        // used lately or by its last build: each carries the workspace's checkout (review C4,
        // N2).
        let kept_by_head = match class {
            Class::Workspace | Class::Temp => true,
            Class::Profile => recent,
            Class::Std | Class::Trash => false,
        };
        if kept_by_head
            && let Some(checkout) = checkout
            && self.heads.binary_search_by(|it| it.as_str().cmp(checkout)).is_ok()
        {
            return Some(Keep::Head { checkout: checkout.to_owned() });
        }
        if self.pins.iter().any(|(_, it)| covers(path, it)) {
            return Some(Keep::Pinned);
        }
        if let Some((agent, _)) = self.holds.iter().find(|(_, it)| covers(path, it)) {
            return Some(Keep::Held { agent: *agent });
        }
        None
    }

    /// Whether a head of its checkout keeps `object` when it is a profile (review N2): used
    /// within [`Config::head_recent_ns`], or by its workspace's last build, as the workspace's
    /// record says (each profile a build writes into is stamped with the workspace's use,
    /// [`crate::record::touch`]). `objects` is sorted by path.
    fn head_keeps(&self, objects: &[Object], object: &Object) -> bool {
        if object.class != Class::Profile {
            return true;
        }
        if self.now.saturating_sub(object.used_ns) < self.config.head_recent_ns {
            return true;
        }
        let Some(workspace) = object.workspace.as_deref() else { return true };
        match objects.binary_search_by(|it| it.path.as_str().cmp(workspace)) {
            Ok(at) => object.used_ns >= objects[at].used_ns,
            // Its workspace not listed: nothing tells its last build, so the head keeps it.
            Err(_) => true,
        }
    }

    /// Whether a profile of `workspace` is one of its last transactions.
    fn in_transaction(&self, workspace: Option<&str>, path: &str) -> bool {
        workspace
            .and_then(|it| self.transactions.get(it))
            .is_some_and(|last| last.iter().any(|it| covers(path, it)))
    }

    /// Whether `path`, anything under it or any directory it is in was rooted or used after
    /// epoch `at`: a use of a workspace keeps each of its profiles too.
    fn touched_after(&self, path: &str, at: u64) -> bool {
        self.touched.iter().any(|(it, epoch)| *epoch > at && (covers(path, it) || covers(it, path)))
    }

    fn plan(&mut self, mut inventory: Inventory) {
        let config = &self.config;
        let now = self.now;
        inventory.objects.sort_by(|a, b| a.path.cmp(&b.path));
        // One object a path: the driver lists each once; a second would be the same directory.
        inventory.objects.dedup_by(|a, b| a.path == b.path);
        let objects = &inventory.objects;
        // Prefix counts by sorted place: what is inside a directory is one range of the sorted
        // paths ([`inside`]), so a lease or a refusal below it is found in O(log n).
        // alloc: a run, two counts an object
        let mut leased_before: Vec<u32> = Vec::with_capacity(objects.len() + 1);
        // alloc: a run, two counts an object
        let mut refused_before: Vec<u32> = Vec::with_capacity(objects.len() + 1);
        leased_before.push(0);
        refused_before.push(0);
        for object in objects {
            leased_before
                .push(leased_before.last().copied().unwrap_or(0) + u32::from(object.leased));
            refused_before.push(
                refused_before.last().copied().unwrap_or(0) + u32::from(object.refused.is_some()),
            );
        }
        let count = |before: &[u32], range: &Range<usize>| before[range.end] - before[range.start];
        // alloc: a run, one verdict an object
        let mut verdicts: Vec<Verdict> = Vec::with_capacity(objects.len());
        let mut marked = (0u64, 0u64);
        let mut total = 0u64;
        for object in objects {
            total = total.saturating_add(object.bytes);
            let age = now.saturating_sub(object.used_ns);
            let below = inside(objects, |it| it.path.as_str(), &object.path);
            // A lease on an object holds every directory it is in.
            let held = object.leased || count(&leased_before, &below) > 0;
            let checkout = object.checkout.as_ref().map(|it| it.path.as_str());
            let fate = if object.class == Class::Trash {
                match age >= config.trash_after_ns {
                    true => Fate::Go(Reason::Trash { for_ns: age }),
                    false => Fate::Keep(Keep::Grace { left_ns: config.trash_after_ns - age }),
                }
            } else if let Some(why) = &object.refused {
                Fate::Keep(Keep::Refused { why: why.clone() })
            } else if count(&refused_before, &below) > 0 {
                // A directory holding something refused is refused whole.
                let child = objects[below]
                    .iter()
                    .find(|it| it.refused.is_some())
                    .map_or("", |it| it.path.as_str());
                Fate::Keep(Keep::Refused { why: format!("{child}, inside it, is refused") })
            } else if usize::from(object.area) >= config.areas.len() {
                Fate::Keep(Keep::Refused {
                    why: format!("its area {} is not one of the collector's", object.area),
                })
            } else if let Some(keep) = self.rooted(
                &object.path,
                object.class,
                checkout,
                held,
                self.head_keeps(objects, object),
            ) {
                marked = (marked.0 + 1, marked.1 + object.bytes);
                Fate::Keep(keep)
            } else if age < config.grace_ns {
                Fate::Keep(Keep::Grace { left_ns: config.grace_ns - age })
            } else {
                match object.class {
                    Class::Workspace => match &object.checkout {
                        Some(checkout) if !checkout.exists && age >= config.gone_grace_ns => {
                            Fate::Go(Reason::CheckoutGone { checkout: checkout.path.clone() })
                        }
                        Some(checkout) if !checkout.exists => {
                            Fate::Keep(Keep::Grace { left_ns: config.gone_grace_ns - age })
                        }
                        _ if age >= config.ttl_ns => Fate::Go(Reason::Unused { for_ns: age }),
                        _ => Fate::Keep(Keep::Fresh),
                    },
                    Class::Std if age >= config.ttl_ns => {
                        Fate::Go(Reason::NoDaemon { for_ns: age })
                    }
                    Class::Temp => Fate::Go(Reason::Temporary { for_ns: age }),
                    Class::Profile
                        if self.in_transaction(object.workspace.as_deref(), &object.path) =>
                    {
                        marked = (marked.0 + 1, marked.1 + object.bytes);
                        Fate::Keep(Keep::Transaction)
                    }
                    Class::Std | Class::Profile => Fate::Keep(Keep::Fresh),
                    Class::Trash => unreachable!("trash is planned above"),
                }
            };
            verdicts.push(Verdict {
                // clone: the verdict's own path, once an object a run
                path: object.path.clone(),
                class: object.class,
                bytes: object.bytes,
                used_ns: object.used_ns,
                fate,
            });
        }
        // What is inside a directory that goes, goes with it.
        go_within(&mut verdicts);
        // Each workspace's cap: its least recently used fresh profiles first.
        let cap = config.workspace_cap_bytes;
        for ws in 0..verdicts.len() {
            if verdicts[ws].class != Class::Workspace || matches!(verdicts[ws].fate, Fate::Go(_)) {
                continue;
            }
            let below = inside(&verdicts, |it| it.path.as_str(), &verdicts[ws].path);
            // alloc: a run, a workspace's profiles
            let mut profiles: Vec<usize> = below
                .filter(|&it| {
                    objects[it].class == Class::Profile
                        && objects[it].workspace.as_deref() == Some(objects[ws].path.as_str())
                })
                .collect();
            let mut held: u64 = profiles.iter().map(|&it| verdicts[it].bytes).sum();
            profiles.sort_by(|&a, &b| {
                (verdicts[a].used_ns, &verdicts[a].path)
                    .cmp(&(verdicts[b].used_ns, &verdicts[b].path))
            });
            for at in profiles {
                if held <= cap {
                    break;
                }
                if verdicts[at].fate == Fate::Keep(Keep::Fresh) {
                    held -= verdicts[at].bytes;
                    // clone: the reason names its workspace, once an eviction
                    let workspace = verdicts[ws].path.clone();
                    verdicts[at].fate = Fate::Go(Reason::OverCap { workspace, cap_bytes: cap });
                    go_inside(&mut verdicts, at);
                }
            }
        }
        // What this scan saw replaces what the lap saw of the same units.
        let next = inventory.next.as_deref();
        let after = self.run.as_ref().and_then(|it| it.after.as_deref());
        let in_window =
            |unit: &str| after.is_none_or(|it| unit > it) && next.is_none_or(|it| unit <= it);
        self.seen.retain(|_, it| !in_window(&it.unit));
        let mut trash_bytes = vec![0u64; self.area_bytes.len()]; // alloc: a run, one an area
        for (object, verdict) in objects.iter().zip(&verdicts) {
            if object.class == Class::Trash {
                if let Some(it) = trash_bytes.get_mut(usize::from(object.area)) {
                    *it += object.bytes;
                }
                continue;
            }
            let evictable = match &verdict.fate {
                Fate::Go(_) => continue,
                Fate::Keep(keep @ (Keep::Fresh | Keep::Transaction)) => Some(keep.clone()),
                Fate::Keep(_) => None,
            };
            let unit = match object.class {
                Class::Profile | Class::Temp => {
                    object.workspace.clone().unwrap_or_else(|| object.path.clone())
                }
                _ => object.path.clone(), // clone: the lap's own copy, once an object
            };
            let seen = Seen {
                unit,
                area: object.area,
                class: object.class,
                bytes: object.bytes,
                used_ns: object.used_ns,
                evictable,
            };
            // clone: the lap's own key, once an object
            self.seen.insert(object.path.clone(), seen);
        }
        for (area, bytes) in self.area_bytes.iter_mut().enumerate() {
            *bytes = trash_bytes[area];
        }
        for it in self.seen.values() {
            if let Some(bytes) = self.area_bytes.get_mut(usize::from(it.area)) {
                *bytes += it.bytes;
            }
        }
        // Pressure: each volume's bytes to free go, least recently used first across every
        // workspace: what the lap saw sets how old a profile must be to go (the cutoff), and
        // this slice evicts what it walked of that, up to the slice's bytes. Profiles first,
        // then a workspace's last transactions, then stores and build dirs whole.
        let slice = self.run.as_ref().map(|it| it.slice).expect("a run plans");
        let pressed = self.paced > 0 || slice.urgent;
        let mut over_budget = 0u64;
        // alloc: a run, one a volume
        let ids: Vec<u64> = self
            .volumes
            .iter()
            .enumerate()
            .filter_map(|(area, it)| {
                it.filter(|it| self.first_area_on(it.id) == area).map(|it| it.id)
            })
            .collect();
        for id in ids {
            let Some(volume) = self.volumes.iter().flatten().find(|it| it.id == id).copied() else {
                continue;
            };
            let on = |area: u16| {
                self.volumes.get(usize::from(area)).copied().flatten().is_some_and(|it| it.id == id)
            };
            let to_free = match pressed {
                true => self.config.levels.to_free(&volume, self.volume_bytes(id)),
                false => 0,
            };
            let going: u64 = verdicts
                .iter()
                .zip(objects)
                .filter(|(it, object)| {
                    on(object.area) && matches!(it.fate, Fate::Go(_)) && it.class != Class::Trash
                })
                .map(|(it, _)| it.bytes)
                .sum::<u64>()
                + trash_bytes
                    .iter()
                    .enumerate()
                    .filter(|(area, _)| on(*area as u16))
                    .map(|(_, it)| *it)
                    .sum::<u64>();
            let mut need_lap = to_free.saturating_sub(going);
            let mut need_slice = need_lap.min(slice.bytes.saturating_sub(going));
            let passes: [(Keep, fn(Class) -> bool); 3] = [
                (Keep::Fresh, |class| class == Class::Profile),
                (Keep::Transaction, |class| class == Class::Profile),
                (Keep::Fresh, |class| matches!(class, Class::Std | Class::Workspace)),
            ];
            for (keep, class_ok) in passes {
                if need_lap == 0 || need_slice == 0 {
                    break;
                }
                // The cutoff from what the lap saw: the oldest that together free what is needed.
                // alloc: a run under pressure, the lap's candidates
                let mut lap: Vec<(u64, &str, u64)> = self
                    .seen
                    .iter()
                    .filter(|(_, it)| {
                        on(it.area) && class_ok(it.class) && it.evictable.as_ref() == Some(&keep)
                    })
                    .map(|(path, it)| (it.used_ns, path.as_str(), it.bytes))
                    .collect();
                lap.sort_unstable();
                let mut sum = 0u64;
                let mut cutoff = u64::MAX;
                for (used_ns, _, bytes) in &lap {
                    sum = sum.saturating_add(*bytes);
                    if sum >= need_lap {
                        cutoff = *used_ns;
                        break;
                    }
                }
                let lap_has = sum;
                drop(lap);
                // alloc: a run under pressure, the evictable
                let mut evictable: Vec<usize> = (0..verdicts.len())
                    .filter(|&it| {
                        verdicts[it].fate == Fate::Keep(keep.clone())
                            && class_ok(verdicts[it].class)
                    })
                    .filter(|&it| on(objects[it].area) && verdicts[it].used_ns <= cutoff)
                    .collect();
                evictable.sort_by(|&a, &b| {
                    (verdicts[a].used_ns, &verdicts[a].path)
                        .cmp(&(verdicts[b].used_ns, &verdicts[b].path))
                });
                for at in evictable {
                    if need_slice == 0 {
                        break;
                    }
                    if !matches!(verdicts[at].fate, Fate::Keep(_)) {
                        continue;
                    }
                    // A directory holding anything a root, a lease, a refusal or the grace keeps
                    // does not go whole.
                    let below = inside(&verdicts, |it| it.path.as_str(), &verdicts[at].path);
                    let blocked = verdicts[below.clone()]
                        .iter()
                        .any(|it| matches!(&it.fate, Fate::Keep(keep) if !matches!(keep, Keep::Fresh | Keep::Transaction)));
                    if blocked {
                        continue;
                    }
                    let freed = verdicts[at].bytes
                        + verdicts[below.clone()]
                            .iter()
                            .filter(|it| {
                                it.class != Class::Trash && !matches!(it.fate, Fate::Go(_))
                            })
                            .map(|it| it.bytes)
                            .sum::<u64>();
                    verdicts[at].fate = Fate::Go(Reason::OverBudget { to_free_bytes: to_free });
                    go_inside(&mut verdicts, at);
                    // What goes is no longer the lap's to evict: the cutoff of the next pass,
                    // and of the next slice, is found among what stays (review C5).
                    forget(&mut self.seen, &mut self.area_bytes, &verdicts[at].path);
                    for it in &verdicts[below] {
                        forget(&mut self.seen, &mut self.area_bytes, &it.path);
                    }
                    need_slice = need_slice.saturating_sub(freed);
                    need_lap = need_lap.saturating_sub(freed);
                }
                // What the lap has of this kind is what the next kind need not free.
                if lap_has < need_lap {
                    continue;
                }
                break;
            }
            over_budget += need_lap;
        }
        // The sweep, after the trash: dead checkouts, then the rest of the garbage, then the
        // cap, then the budget's profiles and then its stores and dirs; oldest first in each.
        let rank = |reason: &Reason, class: Class| -> u8 {
            match reason {
                Reason::CheckoutGone { .. } => 0,
                Reason::Unused { .. } | Reason::NoDaemon { .. } | Reason::Temporary { .. } => 1,
                Reason::OverCap { .. } => 2,
                Reason::OverBudget { .. } if class == Class::Profile => 3,
                Reason::OverBudget { .. } => 4,
                Reason::Within { .. } | Reason::Trash { .. } => 5,
            }
        };
        // alloc: a run, its steps
        let mut sweep: Vec<(u8, u64, usize)> = Vec::new();
        // alloc: a run, its steps
        let mut unlink: Vec<usize> = Vec::new();
        for (at, verdict) in verdicts.iter().enumerate() {
            match &verdict.fate {
                Fate::Go(Reason::Within { .. }) => {}
                Fate::Go(Reason::Trash { .. }) => unlink.push(at),
                Fate::Go(reason) => sweep.push((rank(reason, verdict.class), verdict.used_ns, at)),
                Fate::Keep(_) => {}
            }
        }
        sweep.sort_unstable_by(|a, b| {
            (a.0, a.1, &verdicts[a.2].path).cmp(&(b.0, b.1, &verdicts[b.2].path))
        });
        // Verdicts are one an object, in the objects' order.
        let step = |at: usize| {
            let object = &objects[at];
            let within = inside(&verdicts, |it| it.path.as_str(), &object.path);
            let bytes = verdicts[at].bytes
                + verdicts[within]
                    .iter()
                    .filter(|it| it.class != Class::Trash)
                    .map(|it| it.bytes)
                    .sum::<u64>();
            Step {
                bytes,
                at,
                // clone: a step's own path, once a step
                path: object.path.clone(),
                area: object.area,
                class: object.class,
                checkout: object.checkout.as_ref().map(|it| it.path.clone()),
                recent: self.head_keeps(objects, object),
                trash: None,
            }
        };
        // alloc: a run, its steps
        let sweep: Vec<Step> = sweep.into_iter().map(|(_, _, at)| step(at)).collect();
        // alloc: a run, its steps
        let purge: Vec<Step> = unlink.into_iter().map(step).collect();
        let run = self.run.as_mut().expect("a run plans");
        run.total_bytes = total;
        run.marked_objects = marked.0;
        run.marked_bytes = marked.1;
        run.over_budget_bytes = over_budget;
        run.verdicts = verdicts;
        run.purge = purge;
        run.sweep = sweep;
    }

    /// Asks for the run's next step, if nothing is asked: one rename or unlink at a time, so
    /// each is checked against the roots as they are when it is asked for.
    fn pump(&mut self) {
        loop {
            let now = self.now;
            let Some(run) = self.run.as_mut() else { return };
            let steps = match run.phase {
                Phase::Scan | Phase::Mark => 1,
                Phase::Purge => run.purge.len(),
                Phase::Sweep => run.sweep.len(),
                Phase::Unlink => run.unlink.len(),
            };
            if run.asked.is_some() || matches!(run.phase, Phase::Scan | Phase::Mark) {
                // An answer that does not come (a scan or a rename that hangs, a driver that
                // lost it) ends the run at its deadline all the same; one that comes after is
                // of a run that ended, and dropped. What it did is found by the next scan.
                // A step asked for before the slice's deadline is waited for up to the run's
                // hard deadline: an answer that is late is not one that is lost (review C2).
                if now >= run.hard_ns {
                    let run_ns = run.hard_ns.saturating_sub(run.started_ns);
                    let phase = run.phase;
                    let done = match phase {
                        Phase::Scan | Phase::Mark => 0,
                        Phase::Purge | Phase::Sweep | Phase::Unlink => run.next as u32,
                    };
                    let left = (steps - done as usize) as u32;
                    return self.finish(Outcome::Stopped {
                        phase,
                        why: Stop::Deadline { run_ns },
                        done,
                        left,
                    });
                }
                return;
            }
            if run.next >= steps {
                let next = match run.phase {
                    Phase::Purge => Phase::Sweep,
                    Phase::Sweep => Phase::Unlink,
                    _ => return self.finish(Outcome::Done),
                };
                run.phase = next;
                run.next = 0;
                continue;
            }
            // A run's own trash was counted as it was renamed: its unlink is not counted again.
            let counted = run.phase != Phase::Unlink;
            let stop = if now >= run.deadline_ns {
                Some(Stop::Deadline { run_ns: run.slice.slice_ns })
            } else if run.fuel == 0 {
                Some(Stop::Fuel { fuel: run.slice.steps.min(self.config.fuel) })
            } else if counted && run.bytes_asked >= run.slice.bytes {
                Some(Stop::Slice { bytes: run.slice.bytes })
            } else {
                None
            };
            if let Some(why) = stop {
                let phase = run.phase;
                let done = run.next as u32;
                let left = (steps - run.next) as u32;
                return self.finish(Outcome::Stopped { phase, why, done, left });
            }
            let at = run.next;
            if matches!(run.phase, Phase::Purge | Phase::Unlink) {
                let step = if run.phase == Phase::Purge { &run.purge[at] } else { &run.unlink[at] };
                let path = step.path.clone(); // clone: the decision's own path
                if counted {
                    run.bytes_asked = run.bytes_asked.saturating_add(step.bytes);
                }
                run.asked = Some(at);
                run.fuel -= 1;
                let id = run.id;
                self.decisions.push(Decision::Unlink { run: id, path });
                return;
            }
            // The roots again, as they are now: a pin, hold, head or use since the mark keeps it.
            let marked_at = run.marked_at;
            let step = run.sweep[at].clone(); // clone: one step, checked while the run is borrowed apart
            let keep = self
                .rooted(&step.path, step.class, step.checkout.as_deref(), false, step.recent)
                .or_else(|| {
                    self.touched_after(&step.path, marked_at).then_some(Keep::UsedAfterMark)
                });
            let run = self.run.as_mut().expect("the run pumped");
            if let Some(keep) = keep {
                tracing::info!(target: "blaze", path = %step.path, "gc keeps what was rooted after the mark");
                run.verdicts[step.at].fate = Fate::Keep(keep);
                keep_within(&mut run.verdicts, &step.path);
                run.next += 1;
                continue;
            }
            let area = &self.config.areas[usize::from(step.area)];
            let to = format!(
                "{area}/{TRASH}/{}",
                trash_name(run.started_ns, collector_tag(run.id, run.seq), &step.path)
            );
            run.seq += 1;
            run.sweep[at].trash = Some(to.clone()); // clone: the step keeps where it went
            run.bytes_asked = run.bytes_asked.saturating_add(step.bytes);
            run.asked = Some(at);
            run.fuel -= 1;
            let id = run.id;
            self.decisions.push(Decision::Trash {
                run: id,
                path: step.path,
                to,
                class: step.class,
            });
            return;
        }
    }

    /// How the rename of `path` in run `run` went.
    pub fn trashed(&mut self, run: u64, path: &str, swept: Swept) {
        if self.run.as_ref().is_none_or(|it| it.id != run) {
            return self.answered_stale(run);
        }
        let Some(current) = self.run.as_mut().filter(|it| it.id == run && it.phase == Phase::Sweep)
        else {
            return;
        };
        let Some(at) = current.asked.filter(|&at| current.sweep[at].path == path) else {
            // alloc: a problem reported
            current.problems.push(format!("an answer for {path} that run {run} did not ask for"));
            return;
        };
        current.asked = None;
        current.next = at + 1;
        let verdict = current.sweep[at].at;
        let keep = match swept {
            Swept::Moved => {
                if self.config.trash_after_ns == 0
                    && let Some(to) = current.sweep[at].trash.clone()
                {
                    // Its own trash, unlinked after every rename of this run.
                    // clone: the step's own copy of what it is
                    let mut step = current.sweep[at].clone();
                    step.path = to;
                    step.class = Class::Trash;
                    current.unlink.push(step);
                }
                None
            }
            Swept::Leased => Some(Keep::LeasedAtSweep),
            Swept::NestedLeased { dir } => Some(Keep::NestedLeasedAtSweep { dir }),
            Swept::LeftInTrash { why } => {
                // alloc: a problem reported
                current.problems.push(format!(
                    "{path} is in trash, not moved back for a build inside it: {why}"
                ));
                Some(Keep::LeftInTrash { why })
            }
            Swept::Gone => Some(Keep::Gone),
            Swept::Rooted => Some(Keep::UsedAfterMark),
            Swept::Cancelled => Some(Keep::NotReached),
            Swept::Failed(why) => {
                // alloc: a problem reported
                current.problems.push(format!("{path} was not moved to trash: {why}"));
                Some(Keep::Failed { why })
            }
        };
        if let Some(keep) = keep {
            current.verdicts[verdict].fate = Fate::Keep(keep);
            keep_within(&mut current.verdicts, path);
        }
        self.pump();
    }

    /// How the unlink of `path` (`bytes` of it) in run `run` went.
    pub fn unlinked(&mut self, run: u64, path: &str, done: Result<u64, String>) {
        if self.run.as_ref().is_none_or(|it| it.id != run) {
            return self.answered_stale(run);
        }
        let Some(current) = self
            .run
            .as_mut()
            .filter(|it| it.id == run && matches!(it.phase, Phase::Purge | Phase::Unlink))
        else {
            return;
        };
        let list = if current.phase == Phase::Purge { &current.purge } else { &current.unlink };
        let Some(at) = current.asked.filter(|&at| list[at].path == path) else {
            // alloc: a problem reported
            current.problems.push(format!("an answer for {path} that run {run} did not ask for"));
            return;
        };
        current.asked = None;
        current.next = at + 1;
        match done {
            // Free space is not credited: the measure after the run says what it freed.
            Ok(bytes) => {
                current.unlinked_objects += 1;
                current.unlinked_bytes += bytes;
            }
            // alloc: a problem reported; the entry stays in trash for the next run
            Err(why) => current.problems.push(format!("{path} was not removed from trash: {why}")),
        }
        self.pump();
    }

    fn finish(&mut self, outcome: Outcome) {
        let mut run = self.run.take().expect("a run ends");
        self.ended_run = self.ended_run.max(run.id);
        // A scan or step still out: its answer is waited for (up to a run's bound) before
        // another run but an urgent one starts.
        if run.asked.is_some() || run.phase == Phase::Scan {
            self.stale.push((run.id, self.now.saturating_add(self.config.run_ns)));
        }
        run.problems.append(&mut self.notes);
        // What the run did not get to stays, and says so.
        if !run.dry_run {
            let reached = match run.phase {
                Phase::Scan | Phase::Mark | Phase::Purge => 0,
                Phase::Sweep => run.next.min(run.sweep.len()),
                Phase::Unlink => run.sweep.len(),
            };
            for step in &run.sweep[reached..] {
                if matches!(run.verdicts[step.at].fate, Fate::Go(_)) {
                    run.verdicts[step.at].fate = Fate::Keep(Keep::NotReached);
                }
                keep_within(&mut run.verdicts, &step.path);
            }
        }
        let mut report = RunReport {
            run: run.id,
            why: run.why,
            dry_run: run.dry_run,
            started_ns: run.started_ns,
            ended_ns: self.now,
            outcome,
            objects: run.verdicts.len() as u64,
            total_bytes: run.total_bytes,
            marked_objects: run.marked_objects,
            marked_bytes: run.marked_bytes,
            swept_objects: 0,
            swept_bytes: 0,
            kept_objects: 0,
            kept_bytes: 0,
            unlinked_objects: run.unlinked_objects,
            unlinked_bytes: run.unlinked_bytes,
            over_budget_bytes: run.over_budget_bytes,
            pressure: run.slice.pressure,
            volumes: self.volumes(),
            short: None,
            clock_checked: run.clock_ok,
            problems: run.problems,
            verdicts: run.verdicts,
        };
        for verdict in &report.verdicts {
            match (&verdict.fate, verdict.class) {
                (_, Class::Trash) => {}
                (Fate::Go(_), _) => {
                    report.swept_objects += 1;
                    report.swept_bytes += verdict.bytes;
                }
                (Fate::Keep(_), _) => {
                    report.kept_objects += 1;
                    report.kept_bytes += verdict.bytes;
                }
            }
        }
        // An urgent run that left a volume below its floor, as last measured (before what it
        // freed), says what holds it; the builds held for space are let go once the volumes
        // are measured after it ([`Collector::release_measured`]).
        if report.why == Trigger::Urgent {
            self.last_urgent_ns = Some(self.now);
            // alloc: an urgent run's report, what holds the space
            let mut kept: Vec<(String, u64, Keep)> = report
                .verdicts
                .iter()
                .filter_map(|it| match &it.fate {
                    Fate::Keep(keep) if it.class != Class::Trash => {
                        Some((it.path.clone(), it.bytes, keep.clone()))
                    }
                    _ => None,
                })
                .collect();
            kept.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            kept.truncate(8);
            report.short =
                report.volumes.iter().find(|it| it.free_bytes < it.floor_bytes).map(|volume| {
                    NoSpace {
                        area: volume.area.clone(), // clone: the error's own path
                        free_bytes: volume.free_bytes,
                        floor_bytes: volume.floor_bytes,
                        kept: kept.clone(), // clone: the report's own copy
                        skipped: self.skipped.clone(), // clone: the report's own copy
                    }
                });
            // clone: what held the space, kept for a hold that comes before the next (N4)
            self.urgent_kept = kept.clone();
            if let Outcome::Stopped { why: Stop::ScanFailed { why }, .. } = &report.outcome {
                // Nothing was freed, and measuring again will not change that (review N7).
                let run = report.run;
                let released = &mut self.released;
                self.space_holds.retain(|hold| {
                    if hold.after_run > run {
                        return true;
                    }
                    // clone: each held build's own copy
                    released.push((hold.id, Released::ScanFailed { run, why: why.clone() }));
                    false
                });
            } else if self.space_holds.iter().any(|it| it.after_run <= report.run) {
                self.awaiting = Some(Awaiting {
                    run: report.run,
                    // clone: the measures as the run ended
                    measures: self.measures.clone(),
                    holders: kept,
                });
            }
        }
        // A run that walked everything to its end sizes the next hold's bound (review N5).
        if !run.paced && !report.dry_run && report.outcome == Outcome::Done {
            self.last_walk_ns = Some(report.ended_ns.saturating_sub(report.started_ns));
        }
        // A lap's slices add up; a lap under pressure that freed nothing makes the next wait.
        if run.paced && !report.dry_run {
            self.lap_freed = self
                .lap_freed
                .saturating_add(report.swept_bytes)
                .saturating_add(report.unlinked_bytes);
            if run.lap_end {
                self.lap_open = false;
                self.last_lap_end_ns = Some(self.now);
                self.fruitless = self.lap_trigger == Trigger::Pressure && self.lap_freed == 0;
                self.fruitless_at = self.signed_pressure();
                self.cursor = None;
            }
        }
        // A slice that stopped before its scan came back (a deadline) leaves the cursor where it
        // was: the next goes on from there.
        let span = tracing::info_span!(
            target: "blaze",
            "gc slice",
            run = report.run,
            pressure = report.pressure,
            freed_bytes = report.swept_bytes + report.unlinked_bytes,
            spent_ns = report.ended_ns.saturating_sub(report.started_ns),
        );
        span.in_scope(|| {});
        // Uses no run marked before are no longer needed to stop a sweep.
        self.touched.clear();
        self.last_end_ns = Some(self.now);
        // Only a run the clock check let through vouches for the clock (review C6).
        if run.clock_ok {
            self.trusted_end_ns = Some(self.now);
        }
        self.repace();
        tracing::info!(target: "blaze", "{report}");
        // clone: the decision's copy and the one kept, once a run
        self.decisions.push(Decision::Record { report: report.clone() });
        self.reports.push_back(report);
        while self.reports.len() > REPORTS_KEPT {
            self.reports.pop_front();
        }
        if self.asked.is_some() || self.urgent {
            self.maybe_start();
        }
    }
}

/// `path` is no longer the lap's: dropped from what it saw, and from its area's bytes.
fn forget(seen: &mut HashMap<String, Seen, Global>, area_bytes: &mut [u64], path: &str) {
    if let Some(it) = seen.remove(path)
        && let Some(bytes) = area_bytes.get_mut(usize::from(it.area))
    {
        *bytes = bytes.saturating_sub(it.bytes);
    }
}

/// The places of `sorted` (sorted by `path`) strictly inside `dir`: every `dir/...` sorts from
/// `dir/` up to `dir0`, as `0` follows `/`, whatever sorts between `dir` and `dir/` (`dir-x`).
fn inside<T>(sorted: &[T], path: impl Fn(&T) -> &str, dir: &str) -> Range<usize> {
    let start = sorted.partition_point(|it| before_inside(path(it).as_bytes(), dir.as_bytes()));
    let end = start
        + sorted[start..].partition_point(|it| covers(dir, path(it)) && path(it).len() > dir.len());
    start..end
}

/// Whether `it` sorts before `dir/`, the first path that can be inside `dir`.
fn before_inside(it: &[u8], dir: &[u8]) -> bool {
    let n = it.len().min(dir.len());
    match it[..n].cmp(&dir[..n]) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        // `it` is `dir` or a prefix of it, so before `dir/`; else its next byte decides.
        std::cmp::Ordering::Equal => it.len() <= dir.len() || it[dir.len()] < b'/',
    }
}

/// Everything inside the directory at `at`, which goes, goes with it, named for it; trash aside.
fn go_inside(verdicts: &mut [Verdict], at: usize) {
    let below = inside(verdicts, |it| it.path.as_str(), &verdicts[at].path);
    // clone: the reason names its directory, once a directory that goes
    let dir = verdicts[at].path.clone();
    for verdict in &mut verdicts[below] {
        if verdict.class != Class::Trash {
            // clone: the reason names its directory
            verdict.fate = Fate::Go(Reason::Within { dir: dir.clone() });
        }
    }
}

/// Everything inside a directory that goes goes with it, named for the outermost directory
/// that goes: in path order an outer directory comes before what is in it, which it names
/// first, so an inner one is found already within.
fn go_within(verdicts: &mut [Verdict]) {
    for at in 0..verdicts.len() {
        if matches!(&verdicts[at].fate, Fate::Go(reason) if !matches!(reason, Reason::Within { .. } | Reason::Trash { .. }))
        {
            // A child the grace, a root, a lease or a refusal keeps keeps its directory too: it
            // goes whole or not at all.
            let below = inside(verdicts, |it| it.path.as_str(), &verdicts[at].path);
            let blocker = verdicts[below].iter().find_map(|it| match &it.fate {
                Fate::Keep(keep)
                    if !matches!(keep, Keep::Fresh | Keep::Transaction)
                        && it.class != Class::Trash =>
                {
                    Some(keep.clone())
                }
                _ => None,
            });
            if let Some(keep) = blocker {
                verdicts[at].fate = Fate::Keep(keep);
                continue;
            }
            go_inside(verdicts, at);
        }
    }
}

/// What was going inside `dir`, which stays, stays too.
fn keep_within(verdicts: &mut [Verdict], dir: &str) {
    let below = inside(verdicts, |it| it.path.as_str(), dir);
    for verdict in &mut verdicts[below] {
        if matches!(&verdict.fate, Fate::Go(Reason::Within { dir: it }) if it == dir) {
            verdict.fate = Fate::Keep(Keep::Fresh);
        }
    }
}
