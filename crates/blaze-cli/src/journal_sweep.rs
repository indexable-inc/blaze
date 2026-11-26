//! The sweep's change detection from the system's journal of changes (plan row S1, engine.md
//! D150 item 3): a repeat request costs the changes, not the checkout, with no process alive in
//! between (rule 70).
//!
//! **The rule.** Each request, BEFORE any file of the checkout is read, replays the journal of
//! the checkout's folder, one stream, from the place the stored answer's pointer saved. The
//! checkout is often the driver's root (a command run from its own folder, as tempo's): the root
//! is journaled as any folder, its own level included (blaze-core `Effect::Journal`), so no row
//! of the checkout is outside the stream. Until 2026-11-20 the root had no journal, and the
//! checkout was tiled by its top-level folders of 256 rows or more: its root-level files, `.git`
//! and the small folders were statted on every request, and no one journal covered the checkout,
//! so its git status was computed whole every command (tempo: 383 folders listed, 165 ignore
//! files read). blaze's folder in the home is no unit: its
//! [`LeafKind::HomeListing`] rows (two: the macro and build-script module stores' listings) are
//! followed by their folder's stamp on every request, two stats in place of a stream (tempo's
//! edit on 2026-11-20, score-c2315wave73: "journal replayed" 54.1 ms for two streams, the
//! checkout's and the home's, each nearly all waiting: 3.3 ms of CPU in the phase). Whether a
//! unit's answer is trusted is [`judge`]'s, a pure function of the saved place, the place the
//! answer ends at and what it lost:
//!
//! - the place is there and the journal names nothing: no row is statted, and the certificate
//!   path is taken from the pointer alone;
//! - it names changes: only the rows they reach are statted ([`Taken::marked`]): a row whose
//!   path, or a folder above it, is named; a listing whose folder had a name it reads made or
//!   removed; a slot `x.rs` whose `x/mod.rs` is named. A subtree the journal lost
//!   (`MustScanSubDirs`, dropped events, a moved root: blaze-core fsevents.rs maps the flags)
//!   has every row under it statted, and the folder itself lost is all of the unit's;
//! - the journal was reset (another volume UUID, wrapped IDs), the answer ends before the saved
//!   place (an ID from the future: the counter or the store made again), the folder itself was
//!   lost (dropped events, history truncated, a rescan of all of it, its root moved), no place
//!   was saved, or the replay was refused (its deadline, a driver failure): every row of that
//!   unit is statted, and the profile says which and why ([`JournalUse::Refused`]);
//! - a row in no replayed unit (a row of the runner's fact mount, absolute) is statted;
//! - a build script's read through its served tree (a file, a listing, a stat, a git fact) is
//!   asked again when the checkout's journal named anything in it, or on every request when its
//!   replay was refused ([`ServedReach`] says why);
//! - the system keeps no journal across a restart (Linux): every row is statted, by the same
//!   serial stats as before, and the profile says `none on this OS`.
//!
//! **The place is taken before the files are read.** A file written after the replay and
//! before its read has an event after the saved place: the next replay names it. Taken after
//! the reads, that write would fall between the read and the place, never seen again. A write
//! made just before the request whose event fseventsd had not passed on when the history ended
//! is heard within this request: bstd fs/journal.rs flushes the stream before it stops it.
//!
//! **What no rule here catches.** A change the system's answer omits outright (no name, no lost
//! subtree, no reset, the same UUID) leaves its row as stored: a stale answer until the row is
//! next statted. FSEvents promises no such omission for a stream opened with
//! `kFSEventStreamCreateFlagFullHistory` (fsevents.rs quotes the header), and blaze opens its
//! journal streams so; the test `an_edit_the_answer_omits_is_missed_by_events_alone` shows the
//! gap is real for an answer that breaks that promise. The proposed cover is a periodic audit,
//! not built (the report of F3 says why): every 32nd request by events statts every row and
//! counts the rows that moved where the events reached none. Its cost is one full follow per 32
//! requests: on tempo 1,249 rows in 54.7 ms (tools-1119/s/tempo-score.noop.profile.txt,
//! 2026-11-19), 1.7 ms a request amortised, and an omission is caught within 32 requests.
//!
//! **Why an unmarked row is kept as stored, racy stamp or not.** Its bytes were read after the
//! place its answer saved, and nothing has been written to it since (else the journal names
//! it): the row is today's. The racy rule (a stamp within the clock step of its sweep proves
//! nothing) is for stamps, and no stamp is consulted here.
//!
//! **Settled classes** (the toolchain's shelf, the registry sources) have no row to stat: a
//! source is a record under `<home>/.blaze/v2/sources/<key>` keyed from the lock's text and
//! checked by its digest when fetched (fetch.rs), the lock is a [`LeafKind::Manifest`] row,
//! and the toolchain is in the answer's root name (engine_answer.rs `root_name`). So no
//! dependency or toolchain file is ever statted, and the classes checked are 0.
//!
//! **Sans-IO** (rule 14): whether a unit's answer is trusted ([`judge`]), and which rows its
//! names reach ([`Index::of`], [`marks`]) are pure functions of the stored places, the leaf
//! table and the system's answer, simulated in
//! tests/journal_sweep_tests.rs; [`take`] is the thin driver that asks bstd.
//!
//! Nothing of the journal (IDs, UUIDs, places) enters a record key or a task row: the places
//! are the pointer's own sweep state, read only here.

use core::cmp::Ordering;
use core::fmt;
use core::time::Duration;

use blaze_own::{Texts, Vec};
use bstd::clock::Clock;
use bstd::cx::Cx;
use bstd::fs::{Changes, Cursor, Dir, Fs, FsKind, JournalError, Lost, ReadWrite, ReplayTimes};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use crate::CliError;
use crate::sweep::{self, LeafKind, Leaves, MODULE};

/// How long one request waits for all its replays: past it the replay is refused and the unit
/// swept whole. A replay on APFS is a stream started with its history, a few ms. A strategy
/// that asks the journal helper first (journal_ask.rs) holds its ask and replay under it too.
pub(crate) const JOURNAL_WAIT: Duration = Duration::from_secs(5);

/// A bug the tests plant in the decision, to show their oracles catch it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JournalPlant {
    None,
    /// A subtree the journal lost is not looked at.
    LostIgnored,
    /// A name told for a folder does not reach the rows beneath it.
    FolderNameIgnored,
    /// A place of another journal, or one the answer ends before, is trusted.
    ResetTrusted,
}

/// Why a unit's rows were all statted though the system keeps a journal.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a count of a request, kept for its tests
pub(crate) enum Refused {
    /// The pointer saved no place for it (the first request, or a pointer of the top-level
    /// units before 2026-11-20, whose checkout unit `""` is absent).
    NoCursor,
    /// The journal could not say what changed since the place: reset, truncated.
    Lost(Lost),
    /// The replay failed or passed its deadline.
    Failed(JournalError),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::NoCursor => f.write_str("no place in the journal was saved"),
            Refused::Lost(why) => write!(f, "{why}"),
            Refused::Failed(error) => write!(f, "{error}"),
        }
    }
}

/// Whether the journal decided what the sweep stats.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: as `Refused`
pub(crate) enum JournalUse {
    /// Every unit replayed from its saved place.
    Used,
    /// At least one unit's rows were all statted: the first one's why.
    Refused(Refused),
    /// The system keeps no journal across a restart: every row is statted.
    NoneOnThisOs,
}

impl fmt::Display for JournalUse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JournalUse::Used => f.write_str("used"),
            JournalUse::Refused(why) => write!(f, "refused: {why}"),
            JournalUse::NoneOnThisOs => f.write_str("none on this OS"),
        }
    }
}

/// What a request's sweep cost: the profile's `sweep:` line, and the tests' counts.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: as `Refused`
pub(crate) struct SweepCounts {
    pub(crate) journal: JournalUse,
    /// The names and lost subtrees the journal told in the checkout (blaze's own folder,
    /// git's and cargo's in its root not counted: no row is there). The home is not replayed.
    pub(crate) events: u64,
    /// The rows looked at (statted, and read where the stamp did not prove them).
    pub(crate) statted: u32,
    /// The rows of the stored answer's leaf table.
    pub(crate) leaves: u32,
    /// The settled classes checked: 0, each is covered by a key (the module's doc).
    pub(crate) settled: u32,
    /// The oldest saved event ID a checkout unit was replayed from by its events; none when no
    /// checkout unit was.
    pub(crate) since: Option<u64>,
    /// The checkout units replayed by their events.
    pub(crate) streams: u32,
    /// What the replays cost in wall time, each a stream's start, history, flush and stop.
    pub(crate) replays: Replays,
    /// Why the statted rows were looked at; `None` before the sweep, or with every row statted.
    /// With them, `statted` and `leaves` count the checkout's rows alone: the home's listings,
    /// followed by their stamps on every request, are counted apart ([`SweepParts::home`]).
    pub(crate) parts: Option<SweepParts>,
    /// How the scripts' `git status` answers were made, and what was kept of them for the next
    /// command (engine_script_reads.rs `CheckoutSweep`); `None` with no served checkout.
    pub(crate) status: Option<crate::engine_script_reads::StatusCounts>,
    /// What the checkout's journal helper said ([`ChangedSince`]'s `Asked` and `Both`,
    /// journal_ask.rs); `None` where it was not asked ([`Replay`]).
    pub(crate) helper: Option<HelperRow>,
    /// What the follow's batched looks did and the follow's wall time (sweep.rs, "batched
    /// looks"); `None` before a follow.
    pub(crate) looks: Option<SweepLooks>,
}

/// The follow's batched looks for the `sweep:` line: the paths asked, the batches, folders
/// listed and names stamped, the rows proved, and the follow's wall time against its floor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SweepLooks {
    pub(crate) counts: crate::sweep::LookCounts,
    /// The follow's wall time, looks and the rows decided one by one together.
    pub(crate) wall_ns: u64,
}

impl fmt::Display for SweepLooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = self.counts;
        write!(
            f,
            "looks: {} path(s) in {} batch(es), {} folder(s) listed and {} name(s) stamped, {} row(s) proved by them; the follow {:.2} ms (floor: one batched call a folder, the slowest folder's)",
            c.asked,
            c.batches,
            c.listed,
            c.stamped,
            c.proved,
            self.wall_ns as f64 / 1e6
        )
    }
}

/// What the journal helper said to one request, for the `sweep:` line: which strategy asked,
/// the ask's wall time (connect to answer: its floor is one socket round trip and the helper's
/// barrier), how many names it told, and under `Both` the differential's two counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct HelperRow {
    /// The strategy that asked (its `ChangedSince::NAME`).
    pub(crate) by: &'static str,
    /// The ask's wall time: its folder checked, its connect (a failed one too), its round trip.
    pub(crate) ask_ns: u64,
    /// The names it told when it vouched; `None` when it did not.
    pub(crate) names: Option<u32>,
    /// `Both`: (names the helper told that no name of the replay covers, names of the replay
    /// that no name the helper told covers), blaze's own cookie and socket folders and the
    /// checkout's own entry left out of both; `None` where the two were not both had.
    pub(crate) differ: Option<(u32, u32)>,
    /// What it said (blaze-watch helper.rs `NotAsked`, `CannotVouch`, or the places it
    /// vouched between), then the first names each side alone told, as text.
    pub(crate) said: Said,
}

impl fmt::Display for HelperRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "journal helper ({}): asked in {:.2} ms (floor: one socket round trip and its barrier); ", self.by, self.ask_ns as f64 / 1e6)?;
        match self.names {
            Some(names) => write!(f, "vouched, {names} name(s); ")?,
            None => f.write_str("did not vouch; ")?,
        }
        if let Some((helper, replay)) = self.differ {
            write!(f, "differential: {helper} only the helper's, {replay} only the replay's; ")?;
        }
        f.write_str(self.said.as_str())
    }
}

/// A short text of fixed room (a profile note, never an error): what does not fit is cut at a
/// character's edge and ends in `...`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Said {
    bytes: [u8; Said::MOST],
    len: usize,
    /// Cut: the `...` is written and nothing more is.
    full: bool,
}

impl Said {
    const MOST: usize = 384;
    const CUT: &'static str = "...";

    /// `args` written, cut to the room.
    pub(crate) fn of(args: fmt::Arguments<'_>) -> Said {
        let mut said = Said { bytes: [0; Said::MOST], len: 0, full: false };
        // A cut ends the write early: what was written is kept.
        let _cut = fmt::Write::write_fmt(&mut said, args);
        said
    }

    pub(crate) fn as_str(&self) -> &str {
        // Written whole characters at a time (`write_str`).
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

impl fmt::Write for Said {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.full {
            return Err(fmt::Error);
        }
        // Room is always kept for the cut's `...`.
        let room = Said::MOST - Said::CUT.len() - self.len;
        if text.len() <= room {
            self.bytes[self.len..self.len + text.len()].copy_from_slice(text.as_bytes());
            self.len += text.len();
            return Ok(());
        }
        let mut end = room;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        for part in [&text[..end], Said::CUT] {
            self.bytes[self.len..self.len + part.len()].copy_from_slice(part.as_bytes());
            self.len += part.len();
        }
        self.full = true;
        Err(fmt::Error)
    }
}

impl SweepCounts {
    /// These counts, with why the statted rows were looked at. The home's listings are taken out
    /// of `statted` and `leaves` both (each is statted on every request and counted in
    /// [`SweepParts::home`]), so `statted S of F` is the checkout's rows.
    pub(crate) fn with_parts(self, parts: Option<SweepParts>) -> SweepCounts {
        let home = parts.map_or(0, |parts| parts.home);
        SweepCounts { parts, statted: self.statted.saturating_sub(home), leaves: self.leaves.saturating_sub(home), ..self }
    }

    /// These counts, with the follow's batched looks (sweep.rs `Followed::looks`) and its wall
    /// time.
    pub(crate) fn with_looks(self, counts: crate::sweep::LookCounts, wall_ns: u64) -> SweepCounts {
        SweepCounts { looks: Some(SweepLooks { counts, wall_ns }), ..self }
    }
}

/// What a request's journal replays cost in wall time (the profile's `replays:` part): each is
/// one stream started from its place, its history heard to `HistoryDone`, the flush's cookie
/// heard and the stream stopped (bstd fs/journal.rs, whose three waits are spans of their own).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Replays {
    /// The units replayed (a stream each), refused ones included.
    pub(crate) units: u32,
    /// Their wall time, summed (they run one after another).
    pub(crate) ns: u64,
    /// The longest one's.
    pub(crate) most_ns: u64,
    /// The longest one's stretches (bstd `ReplayTimes`): its start, history, cookie and stop
    /// each timed, so a slow replay names the stretch it was slow in (rule 15). All zero for a
    /// replay refused before its stream ended.
    pub(crate) longest: ReplayTimes,
    /// How old the longest one's place was: the wall clock at its replay less the wall time
    /// kept beside the place when it was taken ([`Stored::taken_wall_ns`]). A replay's history
    /// is every write on the volume since its place (the daemon reads its log from there, a
    /// path-filtered stream too), so its time is read against this. `None`: no place, or one
    /// kept before 2026-11-21 with no time beside it.
    pub(crate) place_age_ns: Option<u64>,
}

impl Replays {
    fn add(&mut self, ns: u64, times: ReplayTimes, place_age_ns: Option<u64>) {
        self.units = self.units.saturating_add(1);
        self.ns = self.ns.saturating_add(ns);
        if ns >= self.most_ns {
            self.longest = times;
            self.place_age_ns = place_age_ns;
        }
        self.most_ns = self.most_ns.max(ns);
    }
}

impl fmt::Display for Replays {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = |ns: u64| ns as f64 / 1e6;
        write!(f, "replays: {} stream(s) in {:.1} ms, the longest {:.1} ms (floor: one stream, its history since the place and one cookie heard)", self.units, ms(self.ns), ms(self.most_ns))?;
        if self.longest != ReplayTimes::default() {
            // What the parts do not sum to is this crate's own time around the call.
            write!(f, " = {} (parts {:.1} ms)", self.longest, ms(self.longest.total_ns()))?;
        }
        if self.units > 0 {
            match self.place_age_ns {
                Some(age) => write!(f, "; its place {:.2} s old", age as f64 / 1e9)?,
                None => f.write_str("; its place's age unknown (no place, or kept with no time)")?,
            }
        }
        Ok(())
    }
}

impl fmt::Display for SweepCounts {
    /// D150's form: `sweep: statted S of F files (by events since id E | full: <why>)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sweep: statted {} of {} files (", self.statted, self.leaves)?;
        match (&self.journal, self.since) {
            (JournalUse::NoneOnThisOs, _) => f.write_str(
                "full: none on this OS: the system keeps no journal of changes across a restart, so every row is statted until a helper with an expiry carries an inotify set",
            )?,
            (JournalUse::Used, Some(id)) => write!(f, "by events since id {id}; {} event(s) in {} folder(s)", self.events, self.streams)?,
            (JournalUse::Used, None) => f.write_str("full: the checkout's journal was not replayed")?,
            (JournalUse::Refused(why), Some(id)) => write!(f, "by events since id {id}; {} event(s) in {} folder(s); a folder full, refused: {why}", self.events, self.streams)?,
            (JournalUse::Refused(why), None) => write!(f, "full, refused: {why}")?,
        }
        write!(f, "); settled classes checked {}", self.settled)?;
        if self.replays.units > 0 {
            write!(f, "; {}", self.replays)?;
        }
        if let Some(helper) = &self.helper {
            write!(f, "; {helper}")?;
        }
        if let Some(parts) = &self.parts {
            write!(f, "; {parts}")?;
        }
        if let Some(looks) = &self.looks {
            write!(f, "; {looks}")?;
        }
        match &self.status {
            Some(status) => write!(f, "\nsweep: {status}"),
            None => Ok(()),
        }
    }
}

/// Which folder a unit's journal is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Where {
    Checkout = 0,
    Home = 1,
}

/// One replayed folder: its name (relative to the checkout, or `""` for the checkout or the
/// home folder itself), the place to save, and what it heard.
struct Unit<'f> {
    at: Where,
    /// Index of the name in [`Taken::names`].
    name: usize,
    /// The place to save: `None` where the replay was refused (the next request sweeps it).
    cursor: Option<Cursor>,
    /// The place it was replayed from (the pointer's), `None` when none was saved.
    since: Option<Cursor>,
    /// What changed: `None` when every row of the unit is statted.
    heard: Option<Heard<'f>>,
}

/// What a unit heard since its place: the replay's answer, the journal helper's names, or both
/// (`Both`, which marks by their union: journal_ask.rs).
struct Heard<'f> {
    replayed: Option<Changes>,
    /// The names the helper told, relative to the checkout, sorted, each once: a file, or a
    /// folder whose every path beneath it moved (blaze-watch since.rs `covers`): a lost subtree
    /// is told by its name, so it reaches the rows a lost subtree of the replay reaches.
    asked: &'f [&'f str],
}

impl<'f> Heard<'f> {
    fn replayed(changes: Changes) -> Heard<'f> {
        Heard { replayed: Some(changes), asked: &[] }
    }

    /// Every name told, the replay's then the helper's: NOT one sorted run when both are there
    /// ([`Index::of`] and [`Taken::checkout_changes`] sort them).
    fn files(&self) -> impl Iterator<Item = &str> + '_ {
        self.replayed.iter().flat_map(Changes::files).chain(self.asked.iter().copied())
    }

    /// The subtrees the replay lost (the helper tells a lost subtree by its name).
    fn lost(&self) -> impl Iterator<Item = (&str, Lost)> + '_ {
        self.replayed.iter().flat_map(Changes::lost)
    }

    fn told(&self) -> usize {
        self.replayed.as_ref().map_or(0, Changes::told) + self.asked.len()
    }
}

/// A request's replays, taken before any file of the checkout is read.
pub(crate) struct Taken<'f> {
    names: Texts<&'f Frame>,
    units: Vec<Unit<'f>, &'f Frame>,
    pub(crate) journal: JournalUse,
    pub(crate) events: u64,
    /// The oldest saved event ID a checkout unit was replayed from by its events.
    since: Option<u64>,
    /// The checkout units replayed by their events.
    streams: u32,
    /// What the replays cost.
    replays: Replays,
    /// blaze's folder in the checkout ignores every name beneath it: its ignore file read
    /// [`sweep::HOME_IGNORE_TEXT`] BEFORE the replays ([`take`]; [`CheckoutChanges::ignored_whole`]
    /// says why that order is the proof).
    blazes_ignored: bool,
    /// The checkout's root `target` is cargo's (sweep.rs `cargo_target_tagged`), read BEFORE the
    /// replays: names under it move no row then. A change to the tag itself is never dropped
    /// ([`own`]) and moves the root's listing ([`Index::reaches`]).
    target_tagged: bool,
    /// The wall clock as the replays began, kept beside the places ([`Stored`]'s `taken`).
    taken_wall_ns: u64,
    /// The journal helper's place to save beside the checkout unit's (blaze-watch since.rs
    /// `Place::encode`, opaque here): the two are one moment, so it is set only with that
    /// unit's place by [`Taken::pair_with_helper`].
    helper_place: Option<[u8; HELPER_PLACE]>,
    /// What the helper said, for the `sweep:` line.
    pub(crate) helper: Option<HelperRow>,
    /// True for a linked git worktree when `.git` is a file pointing to an external gitdir whose
    /// `HEAD`/ref/index state changed since the stored timestamp (or could not be verified),
    /// forcing `ScriptGit` / `.git` script rows to be followed even when checkout changes are empty.
    force_git_asks: bool,
}

/// Bytes of a helper's place kept beside the checkout unit's (blaze-watch since.rs
/// `Place::BYTES`; journal_ask.rs checks the two agree).
pub(crate) const HELPER_PLACE: usize = 16;

/// The places the pointer saved, read in place:
///
/// ```text
/// units u16 | units x (where u8 | name bytes u16 | name | cursor [Cursor::BYTES]) | [taken u64 [helper [16]]]
/// ```
///
/// `taken`, since 2026-11-21: the wall clock (ns since the Unix epoch) when the places were
/// taken, for the profile's `replays:` line alone ([`Replays::place_age_ns`]): never compared,
/// keyed or judged by (a wall clock may step). Places kept before it have none and stay whole.
///
/// `helper`, since 2026-11-21, only after a `taken`: the checkout's journal helper's place
/// (blaze-watch since.rs `Place::encode`), issued at the same moment as the checkout unit's
/// place here (the cursor of the helper's flush, blaze-watch helper.rs `JournalAt`): the next
/// request asks the helper from it, and a helper that cannot vouch costs a replay from the
/// unit's place, which names what the helper would have ([`Taken::pair_with_helper`]).
#[derive(Clone, Copy)]
pub(crate) struct Stored<'m> {
    bytes: &'m [u8],
}

/// Bytes of one unit's fixed part beside its name.
const UNIT_FIXED: usize = 1 + 2 + Cursor::BYTES;

impl<'m> Stored<'m> {
    /// No place saved.
    pub(crate) const NONE: Stored<'static> = Stored { bytes: &[0, 0] };

    /// The places in `bytes`, all of them: `None` when they are not whole (an older pointer).
    pub(crate) fn of(bytes: &'m [u8]) -> Option<Stored<'m>> {
        let stored = Stored { bytes };
        let mut at = 2;
        let units = usize::from(u16::from_le_bytes([*bytes.first()?, *bytes.get(1)?]));
        for _ in 0..units {
            let len = usize::from(u16::from_le_bytes([*bytes.get(at + 1)?, *bytes.get(at + 2)?]));
            let name = bytes.get(at + 3..at + 3 + len)?;
            core::str::from_utf8(name).ok()?;
            let cursor = bytes.get(at + 3 + len..at + UNIT_FIXED + len)?;
            Cursor::decode(cursor)?;
            if *bytes.get(at)? > Where::Home as u8 {
                return None;
            }
            at += UNIT_FIXED + len;
        }
        let trailer = bytes.len() - at.min(bytes.len());
        (at <= bytes.len() && (trailer == 0 || trailer == 8 || trailer == 8 + HELPER_PLACE)).then_some(stored)
    }

    /// The bytes after the units: `taken`, then `helper` where kept.
    fn trailer(&self) -> &'m [u8] {
        let units = usize::from(u16::from_le_bytes([self.bytes[0], self.bytes[1]]));
        let mut at = 2;
        for _ in 0..units {
            // Checked whole by `of`.
            at += UNIT_FIXED + usize::from(u16::from_le_bytes([self.bytes[at + 1], self.bytes[at + 2]]));
        }
        &self.bytes[at..]
    }

    /// The wall time the places were taken (the form's `taken`), when kept.
    pub(crate) fn taken_wall_ns(&self) -> Option<u64> {
        let taken: [u8; 8] = self.trailer().get(..8)?.try_into().ok()?;
        Some(u64::from_le_bytes(taken))
    }

    /// The journal helper's place kept beside the checkout unit's (the form's `helper`).
    pub(crate) fn helper_place(&self) -> Option<[u8; HELPER_PLACE]> {
        self.trailer().get(8..8 + HELPER_PLACE)?.try_into().ok()
    }

    /// `helper` appended to places [`Stored::encode_units`] just wrote WITH a `taken` (the
    /// form keeps a helper's place only after one).
    pub(crate) fn encode_helper(helper: [u8; HELPER_PLACE], out: &mut Vec<u8, &Frame>) {
        out.extend_from_slice(&helper);
    }

    /// The bytes of this form, as [`Stored::of`] read them (journal_checkpoint.rs keeps them in
    /// a checkpoint record).
    pub(crate) fn bytes(&self) -> &'m [u8] {
        self.bytes
    }

    /// `units` (where, name, place) appended to `out` in this form, in their order: the one
    /// encoder of it ([`Taken::encode`], and journal_checkpoint.rs's merged places).
    pub(crate) fn encode_units<'u>(units: impl Iterator<Item = (u8, &'u str, Cursor)>, taken_wall_ns: Option<u64>, out: &mut Vec<u8, &Frame>) {
        // The count is written once the units are: a place for it first.
        let count_at = out.len();
        out.extend_from_slice(&[0, 0]);
        let mut count: u16 = 0;
        let mut cursor_bytes = std::vec::Vec::with_capacity_in(Cursor::BYTES, std::alloc::Global);
        for (at, name, cursor) in units {
            // At most two places' units (a pointer's and a checkpoint's), each one unit (the
            // checkout's folder) or a few from an older pointer.
            count = count.checked_add(1).expect("at most two places of a few units each");
            out.push(at);
            // A unit's name is one part of a path: under 64 KiB.
            out.extend_from_slice(&u16::try_from(name.len()).unwrap_or(u16::MAX).to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            cursor_bytes.clear();
            cursor.encode(&mut cursor_bytes);
            out.extend_from_slice(&cursor_bytes);
        }
        out[count_at..count_at + 2].copy_from_slice(&count.to_le_bytes());
        if let Some(taken) = taken_wall_ns {
            out.extend_from_slice(&taken.to_le_bytes());
        }
    }

    /// Each unit's (where, name, place), in the order saved.
    pub(crate) fn units(&self) -> impl Iterator<Item = (u8, &'m str, Cursor)> + 'm {
        let bytes = self.bytes;
        let units = usize::from(u16::from_le_bytes([bytes[0], bytes[1]]));
        let mut at = 2;
        (0..units).map(move |_| {
            let len = usize::from(u16::from_le_bytes([bytes[at + 1], bytes[at + 2]]));
            // Checked whole by `of`.
            let name = core::str::from_utf8(&bytes[at + 3..at + 3 + len]).expect("checked by Stored::of");
            let cursor = Cursor::decode(&bytes[at + 3 + len..]).expect("checked by Stored::of");
            let kind = bytes[at];
            at += UNIT_FIXED + len;
            (kind, name, cursor)
        })
    }

    fn cursor_of(&self, at: Where, name: &str) -> Option<Cursor> {
        self.units().find(|(kind, it, _)| *kind == at as u8 && *it == name).map(|(_, _, cursor)| cursor)
    }

    /// Whether the stored answer kept a usable journal cursor for the checkout root.
    pub(crate) fn has_checkout_cursor(&self) -> bool {
        self.cursor_of(Where::Checkout, "").is_some_and(|cursor| cursor != Cursor::None)
    }
}

/// The replay of a request (the module's rule): the checkout's folder, one stream, under one
/// deadline from `clock`, from the place in `stored`. Taken BEFORE any file is read. `_home`,
/// blaze's folder in the home, is not replayed (the module's rule): its two listing rows are
/// followed by their stamps, cheaper than a stream; it stays in the seam for the helper's
/// `Asked`, which may hold its stream for nothing ([`ChangedSince`]).
pub(crate) fn take<'f>(cx: &Cx, fs: &Fs, root: &Dir<ReadWrite>, _home: &Dir<ReadWrite>, clock: &Clock, stored: Stored<'_>, frame: &'f Frame) -> Result<Taken<'f>, CliError> {
    let cx = cx.span("sweep: the journal of changes replayed from the stored answer's place");
    let cx = cx.until(clock.after(JOURNAL_WAIT));
    let mut taken = Taken::begin_for(&cx, fs, root, clock, stored, frame)?;
    taken.replay_checkout(&cx, clock, root, stored);
    Ok(taken)
}

/// Whether blaze's folder in the checkout (`.blaze`) ignores every name beneath it for git: its
/// ignore file holds [`sweep::HOME_IGNORE_TEXT`] exactly. One read of a two-byte file. Absent,
/// it is made here (print.rs `ensure_checkout_ignore`, the one maker), BEFORE the place: made
/// after it, as it was until 2026-11-20 (at the first command's end), the next command's replay
/// named it and its git status was computed whole (`RulesChanged`), the world test
/// `a_status_after_an_edit_is_answered_from_the_last_and_the_journal`'s failure. Other bytes, or
/// `.blaze` not a folder: `false`.
fn blazes_folder_ignores_all(cx: &Cx, root: &Dir<ReadWrite>, frame: &Frame) -> Result<bool, CliError> {
    let mut bytes = Bytes::new_in(frame, "blaze's folder's ignore file");
    match root.read_all(cx, &Path::new(sweep::HOME_IGNORE)?, &mut bytes) {
        Ok(_) => Ok(bytes.as_slice() == sweep::HOME_IGNORE_TEXT),
        Err(error) if error.kind == FsKind::NotFound => {
            crate::print::ensure_checkout_ignore(cx, root)?;
            Ok(true)
        }
        Err(error) if error.kind == FsKind::NotADirectory => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// What changed since a request's stored place, and the place now: one question with two
/// answers behind this one seam (rule 78), an optimization slot (D168 mechanism 4), never a
/// fallback. Each instantiation is a complete answer, held to one oracle in blaze-watch's
/// sim/helper.rs, and the `journal:` line names the one that answered.
///
/// - [`Replay`]: this process makes the streams, replays them from the stored places and
///   flushes them ([`take`]): tens of milliseconds on tempo, nearly all of it the streams' setup.
/// - `Asked` (journal_ask.rs): connect to the checkout's helper (journal_helper.rs) at
///   blaze-watch's `helper::socket_path` under the home, check its hello (version, checkout), send the stored
///   helper place ([`Stored::helper_place`]), take the names and the new place under
///   `helper::ASK_DEADLINE`. A `helper::NotAsked` or a `CannotVouch` answer runs [`Replay`]
///   from the journal place the stored answer kept beside the helper's, and the line says why.
/// - `Both` (journal_ask.rs): asks, then replays, marks by the union of the two and prints
///   where they differ: the differential that runs until the switch to `Asked`.
///
/// No strategy starts a helper: journal_ask.rs's `HelperStart` is `None` until one is run.
pub(crate) trait ChangedSince {
    /// What the profile calls it.
    const NAME: &'static str;

    fn changed_since<'f>(
        &self, cx: &Cx, fs: &Fs, root: &Dir<ReadWrite>, home: &Dir<ReadWrite>, clock: &Clock, stored: Stored<'_>, frame: &'f Frame,
    ) -> Result<Taken<'f>, CliError>;
}

/// The command replays the journal itself ([`take`]).
pub(crate) struct Replay;

impl ChangedSince for Replay {
    const NAME: &'static str = "replayed";

    fn changed_since<'f>(
        &self, cx: &Cx, fs: &Fs, root: &Dir<ReadWrite>, home: &Dir<ReadWrite>, clock: &Clock, stored: Stored<'_>, frame: &'f Frame,
    ) -> Result<Taken<'f>, CliError> {
        take(cx, fs, root, home, clock, stored, frame)
    }
}

/// Whether one unit's answer is trusted (sans-IO): `None` to stat what its names reach, else why
/// every row of it is statted. `since`: the place the pointer saved for it; `now`: the place the
/// system's answer ends at; `whole`: why the answer lost the unit's folder itself, if it did.
///
/// The system vouches only for a continuation of one journal: a place of the same kind, the same
/// store (FSEvents' volume UUID, from `FSEventsCopyUUIDForDevice`; NTFS's volume and journal IDs;
/// the simulation's journal ID) and not before the saved one. fsevents.rs `Watch::journal` makes
/// the same choice to pick where its stream starts; this is the decision the sweep stands on,
/// made again from the answer alone, so a driver that resumed where it should not have is caught
/// here and not trusted.
pub(crate) fn judge(since: Option<Cursor>, now: Cursor, whole: Option<Lost>, plant: JournalPlant) -> Option<Refused> {
    let since = match since {
        None | Some(Cursor::None) => return Some(Refused::NoCursor),
        Some(since) => since,
    };
    match plant {
        JournalPlant::ResetTrusted => whole.filter(|why| *why != Lost::Reset).map(Refused::Lost),
        _ if !continues(since, now) => Some(Refused::Lost(Lost::Reset)),
        _ => whole.map(Refused::Lost),
    }
}

/// Whether `now` is a place of the journal `since` is of, at or after it.
fn continues(since: Cursor, now: Cursor) -> bool {
    match (since, now) {
        (Cursor::FsEvents { event: a, volume: v }, Cursor::FsEvents { event: b, volume: w }) => v == w && a <= b,
        (Cursor::Usn { volume: v, journal: j, next: a }, Cursor::Usn { volume: w, journal: k, next: b }) => v == w && j == k && a <= b,
        (Cursor::Sim { journal: j, next: a }, Cursor::Sim { journal: k, next: b }) => j == k && a <= b,
        _ => false,
    }
}

/// What the journal named in the checkout between two places ([`Taken::checkout_changes`]).
pub(crate) struct CheckoutChanges<'t, 'f> {
    /// The place replayed from: the stored answer's ([`place_of`]).
    pub(crate) since: JournalPlace,
    /// The place replayed to: this command's, saved with its answer.
    pub(crate) now: JournalPlace,
    /// The paths named, relative to the checkout, sorted, each once.
    pub(crate) changed: Vec<&'t str, &'f Frame>,
    /// A folder of the checkout under which git ignores every untracked name throughout the
    /// window `since..now`: blaze's own (`.blaze`), when its ignore file read
    /// [`sweep::HOME_IGNORE_TEXT`] BEFORE this command's replay. The proof: that read is inside
    /// the window, so if the file changed anywhere in it its name is in `changed` and the status
    /// is computed whole (blaze-build-script git_objects.rs `decide`, `RulesChanged`); else its
    /// bytes were the same all through the window. A `*` in a folder's own ignore file outranks
    /// every outer rule for the names in it, and git does not walk into a folder so ignored, so
    /// no untracked name under it has a status line, then or now. A tracked one is the index's,
    /// asked as before. So blaze's own writes there (the profile, every command) recompute no
    /// region's untracked walk (git_objects.rs `CoveredSince::ignoring`). `None` otherwise.
    pub(crate) ignored_whole: Option<&'static str>,
}

/// A journal's place as a change-driven answer keeps it (blaze-build-script git `Gen`): the
/// journal's identity (its kind and store: FSEvents' volume UUID, NTFS's volume and journal
/// IDs, the simulation's journal ID) and how far into it. Two places compare as [`continues`]
/// judges them: the same identity, and `at` not before.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct JournalPlace {
    pub(crate) journal: [u8; 24],
    pub(crate) at: u64,
}

impl JournalPlace {
    /// The place as the git status keeps it (blaze-build-script git `Gen`): the same identity
    /// and offset, so two places compare there as here.
    pub(crate) fn gen_of(self) -> blaze_build_script::git::Gen {
        blaze_build_script::git::Gen { journal: self.journal, at: self.at }
    }
}

/// `cursor` as a [`JournalPlace`]; `None` for no place (or an NTFS place before its start).
pub(crate) fn place_of(cursor: Cursor) -> Option<JournalPlace> {
    let mut journal = [0u8; 24];
    let at = match cursor {
        Cursor::None => return None,
        Cursor::FsEvents { event, volume } => {
            journal[0] = 1;
            journal[1..17].copy_from_slice(&volume);
            event
        }
        Cursor::Usn { volume, journal: id, next } => {
            journal[0] = 2;
            journal[1..9].copy_from_slice(&volume.to_le_bytes());
            journal[9..17].copy_from_slice(&id.to_le_bytes());
            u64::try_from(next).ok()?
        }
        Cursor::Sim { journal: id, next } => {
            journal[0] = 3;
            journal[1..9].copy_from_slice(&id.to_le_bytes());
            next
        }
    };
    Some(JournalPlace { journal, at })
}

/// A place's event ID, for the profile.
fn id_of(cursor: Cursor) -> Option<u64> {
    match cursor {
        Cursor::None => None,
        Cursor::FsEvents { event, .. } => Some(event),
        Cursor::Usn { next, .. } => u64::try_from(next).ok(),
        Cursor::Sim { next, .. } => Some(next),
    }
}

impl<'t> Taken<'t> {
    /// No unit yet: what every strategy does BEFORE its place is taken (blaze's folder's ignore
    /// file read, [`CheckoutChanges::ignored_whole`]'s proof), and the wall time kept beside
    /// the places. `cx` carries the strategy's span and deadline.
    pub(crate) fn begin(cx: &Cx, fs: &Fs, root: &Dir<ReadWrite>, clock: &Clock, frame: &'t Frame) -> Result<Taken<'t>, CliError> {
        Self::begin_for(cx, fs, root, clock, Stored::NONE, frame)
    }

    /// Like [`Taken::begin`], with `stored` so a linked git worktree's external gitdir can be
    /// checked against `stored.taken_wall_ns()` instead of unconditionally forcing git script
    /// re-evaluation on every warm no-op.
    pub(crate) fn begin_for(cx: &Cx, fs: &Fs, root: &Dir<ReadWrite>, clock: &Clock, stored: Stored<'_>, frame: &'t Frame) -> Result<Taken<'t>, CliError> {
        let blazes_ignored = blazes_folder_ignores_all(cx, root, frame)?;
        let target_tagged = sweep::cargo_target_tagged(cx, root, frame)?;
        let taken_wall_ns = clock.wall().as_ns();
        let force_git_asks = worktree_git_needs_check(cx, fs, root, stored.taken_wall_ns(), taken_wall_ns, frame)?;
        Ok(Taken {
            names: Texts::new_in(frame),
            units: Vec::new_in(frame),
            journal: JournalUse::Used,
            events: 0,
            since: None,
            streams: 0,
            replays: Replays::default(),
            blazes_ignored,
            target_tagged,
            taken_wall_ns,
            helper_place: None,
            helper: None,
            force_git_asks,
        })
    }

    /// The checkout's own folder replayed from its place in `stored` ([`take`]'s one unit).
    pub(crate) fn replay_checkout(&mut self, cx: &Cx, clock: &Clock, root: &Dir<ReadWrite>, stored: Stored<'_>) {
        self.replay(cx, clock, root, Where::Checkout, "", stored);
    }

    /// The checkout's own folder answered by the journal helper (journal_ask.rs `Asked`): it
    /// vouched for `names` since the place `stored` kept beside the unit's (its pair), and its
    /// new place is one moment with the journal's `journal`. Judged as a replay's answer is
    /// ([`judge`]: the unit's stored place and `journal` must be one journal, in order).
    pub(crate) fn vouched(&mut self, stored: Stored<'_>, names: &'t [&'t str], journal: Cursor) {
        let name_at = self.names.push("");
        let since = stored.cursor_of(Where::Checkout, "");
        let heard = Heard { replayed: None, asked: names };
        self.heard(Where::Checkout, "", name_at, since, journal, None, heard);
    }

    /// The checkout's unit's place to save made the helper's flush's `journal`, and the
    /// helper's place `helper` saved beside it: the pair is one moment (blaze-watch helper.rs
    /// `AskAnswer`). Sound whatever this request's replay reached: every row this request reads
    /// is read after both were taken, so a write after `journal` is named by the next replay
    /// from it. Nothing is paired where this request has no checkout unit (no journal).
    pub(crate) fn pair_with_helper(&mut self, journal: Cursor, helper: [u8; HELPER_PLACE]) {
        let names = &self.names;
        if let Some(unit) = self.units.iter_mut().find(|unit| unit.at == Where::Checkout && names.text(unit.name).is_empty()) {
            unit.cursor = Some(journal);
            self.helper_place = Some(helper);
        }
    }

    /// Records a refused checkout unit directly from a helper's fresh `(journal, helper)` pair
    /// when no prior checkout cursor was stored: equivalent to replaying from `Cursor::None` and
    /// then calling [`Taken::pair_with_helper`], without opening a redundant OS journal stream.
    pub(crate) fn refused_with_helper_place(&mut self, journal: Cursor, helper: [u8; HELPER_PLACE]) {
        let name_at = self.names.push("");
        self.refuse(Refused::NoCursor);
        self.units.push(Unit { at: Where::Checkout, name: name_at, cursor: Some(journal), since: None, heard: None });
        self.helper_place = Some(helper);
    }

    /// Records a refused checkout unit without opening an OS journal replay stream (e.g., when
    /// no journal helper vouched and a direct batched stat sweep over a small checkout is strictly
    /// faster than paying FSEvents stream setup). Leaves `cursor: None` so no stale cursor is
    /// written; every leaf of the checkout is statted.
    pub(crate) fn refused_without_replay(&mut self) {
        let name_at = self.names.push("");
        self.refuse(Refused::NoCursor);
        self.units.push(Unit { at: Where::Checkout, name: name_at, cursor: None, since: None, heard: None });
    }

    /// `Both`: the helper's `names` added to what the checkout's replay heard, so the rows
    /// either reaches are looked at (a replay refused, every row is already).
    pub(crate) fn add_helper_names(&mut self, names: &'t [&'t str]) {
        let names_of = &self.names;
        let unit = self.units.iter_mut().find(|unit| unit.at == Where::Checkout && names_of.text(unit.name).is_empty());
        if let Some(Unit { heard: Some(heard), .. }) = unit {
            heard.asked = names;
        }
    }

    /// The names the checkout's replay told (files, then lost subtrees), for `Both`'s
    /// differential; `None` where it was refused or not made.
    pub(crate) fn replayed_names(&self) -> Option<impl Iterator<Item = &str> + '_> {
        let unit = self.units.iter().find(|unit| unit.at == Where::Checkout && self.names.text(unit.name).is_empty())?;
        let changes = unit.heard.as_ref()?.replayed.as_ref()?;
        Some(changes.files().chain(changes.lost().map(|(under, _)| under)))
    }

    /// What the helper said, for the `sweep:` line.
    pub(crate) fn said(&mut self, row: HelperRow) {
        self.helper = Some(row);
    }

    /// One unit replayed; where the system keeps no journal, none and every row statted.
    fn replay(&mut self, cx: &Cx, clock: &Clock, dir: &Dir<ReadWrite>, at: Where, name: &str, stored: Stored<'_>) {
        let cx = cx.span("sweep: one folder's journal replayed");
        let name_at = self.names.push(name);
        let since = stored.cursor_of(at, name);
        let started = clock.now();
        let asked = dir.changes_since(&cx, since.unwrap_or(Cursor::None));
        let times = asked.as_ref().map_or(ReplayTimes::default(), |changes| changes.times);
        let age = since.and(stored.taken_wall_ns()).map(|taken| clock.wall().as_ns().saturating_sub(taken));
        self.replays.add(clock.now().since(started).map_or(0, |took| u64::try_from(took.as_nanos()).unwrap_or(u64::MAX)), times, age);
        let changes = match asked {
            Ok(changes) => changes,
            Err(JournalError::NoJournal { .. }) => {
                self.journal = JournalUse::NoneOnThisOs;
                self.units.clear();
                return;
            }
            Err(error) => {
                self.refuse(Refused::Failed(error));
                self.units.push(Unit { at, name: name_at, cursor: None, since, heard: None });
                return;
            }
        };
        let cursor = changes.cursor;
        // The folder itself lost (any why: a reset, a truncated history, dropped events, a
        // rescan of all of it, a moved root), or a place the answer does not continue: refused
        // by name, every row of the unit statted.
        let whole = changes.lost().find(|(under, _)| under.is_empty()).map(|(_, why)| why);
        self.heard(at, name, name_at, since, cursor, whole, Heard::replayed(changes));
    }

    /// A unit's answer judged ([`judge`]), counted, and kept: `cursor`, the place it ends at;
    /// `whole`, why it lost the unit's folder itself, if it did.
    #[expect(clippy::too_many_arguments, reason = "one unit's answer: each argument a part of it")]
    fn heard(&mut self, at: Where, name: &str, name_at: usize, since: Option<Cursor>, cursor: Cursor, whole: Option<Lost>, heard: Heard<'t>) {
        let heard = match judge(since, cursor, whole, JournalPlant::None) {
            Some(why) => {
                self.refuse(why);
                None
            }
            None => Some(heard),
        };
        let cursor = Some(cursor);
        if let Some(changes) = &heard {
            // The checkout's alone: blaze's folder in the home is the store's too, written by
            // every request after its replay, so its names say nothing of the edit.
            let target_tagged = self.target_tagged;
            let counted = |path: &str| at == Where::Checkout && (!name.is_empty() || !own(path, target_tagged));
            let told = changes.files().filter(|it| counted(it)).count() + changes.lost().filter(|(it, _)| counted(it)).count();
            self.events = self.events.saturating_add(u64::try_from(told).unwrap_or(u64::MAX));
            if at == Where::Checkout {
                self.streams = self.streams.saturating_add(1);
                let from = since.and_then(id_of);
                self.since = match (self.since, from) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
            }
        }
        self.units.push(Unit { at, name: name_at, cursor, since, heard });
    }

    /// What the journal says changed in the checkout, for a change-driven answer (E3: the git
    /// status after a change, blaze-build-script git_objects.rs `status_after`): the places it
    /// was replayed from and to, and every path it named under the checkout, files and lost
    /// subtrees, relative to the checkout, sorted, each once (git's folder too: a stage or a
    /// commit is named there; the checkout's own level too, the driver's root included). `None`
    /// unless the checkout's replay was judged a continuation ([`judge`]). This is the coverage
    /// proof's one source; the answer computes whole without it, saying so.
    pub(crate) fn checkout_changes<'f>(&self, frame: &'f Frame) -> Option<CheckoutChanges<'_, 'f>> {
        let unit = self.units.iter().find(|unit| unit.at == Where::Checkout && self.names.text(unit.name).is_empty())?;
        let heard = unit.heard.as_ref()?;
        let since = unit.since.and_then(place_of)?;
        let now = unit.cursor.and_then(place_of)?;
        let mut changed: Vec<&str, &'f Frame> = Vec::with_capacity_in(heard.told(), frame);
        changed.extend(heard.files());
        changed.extend(heard.lost().map(|(under, _)| under));
        changed.sort_unstable();
        changed.dedup();
        Some(CheckoutChanges { since, now, changed, ignored_whole: self.blazes_ignored.then_some(sweep::HOME) })
    }

    /// The checkout's own unit's place this request (saved with its answer), taken before any
    /// file was read: what a change-driven answer computed this request is bound to, whether or
    /// not the replay to it was judged a continuation (the first request has nothing to replay
    /// from, and its place is still today's). `None` where the system keeps no journal, or the
    /// replay failed (no place saved: the next request sweeps it whole).
    pub(crate) fn checkout_place(&self) -> Option<JournalPlace> {
        let unit = self.units.iter().find(|unit| unit.at == Where::Checkout && self.names.text(unit.name).is_empty())?;
        unit.cursor.and_then(place_of)
    }

    /// A unit's rows are all statted for `why`: the profile names the first such why.
    fn refuse(&mut self, why: Refused) {
        if self.journal == JournalUse::Used {
            self.journal = JournalUse::Refused(why);
        }
    }

    /// Which rows of `last` the sweep looks at: `None` for all (no journal); else one mark a
    /// row, in its order ([`sweep::Follow::follow_marked`]).
    pub(crate) fn marked<'f>(&self, last: &Leaves<'_>, frame: &'f Frame) -> Option<Vec<bool, &'f Frame>> {
        self.marked_why(last, frame).map(|(marks, _)| marks)
    }

    /// Why the rows [`Taken::marked`] marks are marked, counted (`None` as there: every row).
    pub(crate) fn parts(&self, last: &Leaves<'_>, frame: &Frame) -> Option<SweepParts> {
        self.marked_why(last, frame).map(|(_, parts)| parts)
    }

    /// [`Taken::marked`]: [`marks`] of each unit's names, then a build script's reads by
    /// [`ServedReach`]; and why each row is looked at.
    fn marked_why<'f>(&self, last: &Leaves<'_>, frame: &'f Frame) -> Option<(Vec<bool, &'f Frame>, SweepParts)> {
        if self.journal == JournalUse::NoneOnThisOs {
            return None;
        }
        // Each unit's names and lost subtrees, relative to it, as sorted columns.
        let mut views: Vec<UnitView<'_>, &Frame> = Vec::with_capacity_in(self.units.len(), frame);
        let mut reach = ServedReach { whole: false, told: false };
        for unit in self.units.iter() {
            let name = self.names.text(unit.name);
            // In the checkout's own folder, blaze's, git's and cargo's are read by no row.
            let skip_own = unit.at == Where::Checkout && name.is_empty();
            let folders = skip_own.then_some(OwnFolders { target_tagged: self.target_tagged });
            let index = unit.heard.as_ref().and_then(|changes| Index::of(changes.files(), changes.lost(), folders, JournalPlant::None, frame));
            if skip_own && let (Some(changes), Some(_)) = (&unit.heard, &index) {
                // `.git` and `target` count here: a script's git fact reads `.git`.
                let blazes = |path: &str| path.split('/').next() == Some(sweep::HOME);
                let told = changes.files().any(|it| !blazes(it))
                    || changes.lost().any(|(it, _)| !blazes(it));
                reach = ServedReach { whole: true, told };
            }
            views.push(UnitView { at: unit.at, name, index });
        }
        let mut marks = marks(last, &views, JournalPlant::None, frame);
        let mut parts = SweepParts { whole: reach.whole, ..SweepParts::default() };
        for (i, mark) in marks.iter_mut().enumerate() {
            let row = last.row(i);
            let path = last.path(i);
            let fact_file = row.kind == LeafKind::ScriptFile && path.starts_with(sweep::FACT_MOUNT);
            let script_asks = script_row_uses_asks(row.kind, row.flags, path, self.target_tagged);
            if script_asks {
                let is_git_row = row.kind == LeafKind::ScriptGit || path == sweep::GIT || path.starts_with(".git/");
                *mark = reach.asks() || (is_git_row && self.force_git_asks);
            }
            if !*mark || row.kind == LeafKind::MacroFiles {
                // A `MacroFiles` row is decided by its files' rows and not counted (sweep.rs).
                continue;
            }
            let part = match row.kind {
                // No stream replays the home (the module's rule): its listings, on every request.
                LeafKind::HomeListing => &mut parts.home,
                LeafKind::ScriptGit => &mut parts.git,
                _ if script_asks && !fact_file => &mut parts.scripts,
                kind => match unit_of(&views, kind, path) {
                    None => &mut parts.unwatched,
                    Some((UnitView { index: None, .. }, _)) => &mut parts.refused,
                    Some(_) => &mut parts.told,
                },
            };
            *part = part.saturating_add(1);
        }
        Some((marks, parts))
    }

    /// The places to save in the pointer (the [`Stored`] form): each unit replayed.
    pub(crate) fn encode(&self, out: &mut Vec<u8, &Frame>) {
        let saved = self.units.iter().filter_map(|unit| unit.cursor.map(|cursor| (unit.at as u8, self.names.text(unit.name), cursor)));
        Stored::encode_units(saved, Some(self.taken_wall_ns), out);
        if let Some(helper) = self.helper_place {
            // After the `taken` just written, as the form keeps it.
            Stored::encode_helper(helper, out);
        }
    }

    /// The counts of a sweep that looked at `statted` rows of `leaves`.
    pub(crate) fn counts(&self, statted: u32, leaves: u32) -> SweepCounts {
        // clone: the request's one count of its journal, kept for its tests.
        SweepCounts { journal: self.journal.clone(), events: self.events, statted, leaves, settled: 0, since: self.since, streams: self.streams, replays: self.replays, parts: None, status: None, helper: self.helper, looks: None }
    }
}

/// Whether a script sweep row must fall back to `reach.asks()` (re-statted whenever any
/// checkout file changed) rather than relying on `Index::reaches` alone. Direct non-symlink
/// checkout paths outside `.git`/`.blaze`/`target` can only move when the journal reaches them
/// or an ancestor folder; symlinks, own-folder paths, and `ScriptGit` keep `reach.asks()`.
pub(crate) fn script_row_uses_asks(kind: LeafKind, flags: u8, path: &str, target_tagged: bool) -> bool {
    let fact_file = kind == LeafKind::ScriptFile && path.starts_with(sweep::FACT_MOUNT);
    if !kind.by_script() || fact_file {
        return false;
    }
    let direct_script = matches!(kind, LeafKind::ScriptFile | LeafKind::ScriptStat | LeafKind::ScriptListing)
        && !own(path, target_tagged)
        && flags & sweep::DIRECT_PATH != 0;
    !direct_script
}

/// One replayed unit as [`marks`] reads it.
pub(crate) struct UnitView<'a> {
    pub(crate) at: Where,
    /// The unit's folder, relative to the checkout (or the home): `""` for all of it.
    pub(crate) name: &'a str,
    /// What its names reach: `None` when every row of it is statted (refused by [`judge`], or
    /// its folder itself lost).
    pub(crate) index: Option<Index<'a>>,
}

/// Which rows of `last` the sweep looks at (sans-IO), one mark a row, in its order: a row is
/// looked at when it is in no unit, or its unit's index is none or reaches it.
pub(crate) fn marks<'f>(last: &Leaves<'_>, units: &[UnitView<'_>], plant: JournalPlant, frame: &'f Frame) -> Vec<bool, &'f Frame> {
    let mut marks = Vec::with_capacity_in(last.len(), frame);
    for i in 0..last.len() {
        let row = last.row(i);
        // A row in no replayed unit (the fact mount's, the home's) is looked at.
        let mark = match unit_of(units, row.kind, last.path(i)) {
            None => true,
            Some((unit, rel)) => match &unit.index {
                None => true,
                Some(index) => index.reaches(row.kind, row.flags, rel, plant),
            },
        };
        marks.push(mark);
    }
    marks
}

/// The replayed unit a row of `kind` at `path` is in, and its path relative to it; `None` for a
/// row in none. An absolute path (the fact mount) is in no unit of the checkout.
fn unit_of<'u, 'a, 'p>(units: &'u [UnitView<'a>], kind: LeafKind, path: &'p str) -> Option<(&'u UnitView<'a>, &'p str)> {
    let at = if kind == LeafKind::HomeListing { Where::Home } else { Where::Checkout };
    units.iter().filter(|unit| unit.at == at && !(at == Where::Checkout && path.starts_with('/'))).find_map(|unit| match unit.name.is_empty() {
        true => Some((unit, path)),
        false => path.strip_prefix(unit.name).and_then(|rest| rest.strip_prefix('/')).map(|rel| (unit, rel)),
    })
}

/// How the journal reaches the rows a build script read through its served tree
/// (engine_script_reads.rs `CheckoutSweep`), sans-IO. Such a row's answer can rest on more than
/// its own path: a link it went through, the index and every tracked file a `git status`
/// compares. What it can rest on is inside the checkout, and nothing else: the served tree
/// refuses a link out of it (script_sandbox.rs `disk_node`), so such a read's answer is the
/// refusal whatever the target holds.
///
/// So when the checkout's journal covers it whole (`whole`: its replay trusted), such a row
/// moves only if that journal named something in it: `told` is whether it named anything but
/// blaze's own folder (written by every request, read by no script). Else (the replay refused)
/// every such row is asked again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ServedReach {
    pub(crate) whole: bool,
    pub(crate) told: bool,
}

impl ServedReach {
    /// Whether a build script's read (a file, a listing, a stat, a git fact) is asked again.
    pub(crate) fn asks(self) -> bool {
        !self.whole || self.told
    }
}

/// Why each row the sweep looks at is looked at: the profile's parts of `statted`, the floor
/// (`told`, the rows the journal's names reach) against the rest (rule 67).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct SweepParts {
    /// Reached by a name the journal told, or a subtree it lost: the floor.
    pub(crate) told: u32,
    /// In no folder a journal was replayed for: the fact mount's (absolute) rows.
    pub(crate) unwatched: u32,
    /// In a folder whose replay was refused (the profile's `refused:` says why): all of them.
    pub(crate) refused: u32,
    /// A build script's read of a file, listing or stat, asked again ([`ServedReach`]).
    pub(crate) scripts: u32,
    /// A build script's git fact, asked again ([`ServedReach`]).
    pub(crate) git: u32,
    /// The home's listings ([`LeafKind::HomeListing`]), followed by their folder's stamp on
    /// every request: no stream replays the home (the module's rule). Not in the checkout's
    /// `statted` ([`SweepCounts::with_parts`]).
    pub(crate) home: u32,
    /// One journal covered the checkout whole ([`ServedReach::whole`]).
    pub(crate) whole: bool,
}

impl fmt::Display for SweepParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "of the statted: {} the journal's names reach (floor: the files of the folders told), {} in no folder a journal watched, {} in a folder whose replay was refused, {} build-script reads and {} git facts asked again (",
            self.told, self.unwatched, self.refused, self.scripts, self.git,
        )?;
        match self.whole {
            true => f.write_str("the journal of the whole checkout named a change"),
            false => f.write_str("the checkout's replay was refused: each is asked on every request"),
        }?;
        write!(f, "); and {} home listing(s) followed by their stamps (no stream replays the home)", self.home)
    }
}

/// A name in the checkout's root that no row reads: blaze's folder, git's, and cargo's when its
/// `target` holds the tag (sweep.rs `not_the_checkouts`). `target` itself and its tag are never
/// dropped: either changing can move `target` into or out of the root's listing.
fn own(rel: &str, target_tagged: bool) -> bool {
    let first = rel.split('/').next().unwrap_or(rel);
    rel != blaze_build_script::served::CARGO_TARGET && rel != sweep::TARGET_TAG && sweep::not_the_checkouts(first, target_tagged)
}

/// The checkout's own folder as an [`Index`] leaves it out: its root's names no row reads
/// ([`own`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct OwnFolders {
    /// The root's `target` is cargo's (sweep.rs `cargo_target_tagged`), read before the replay.
    pub(crate) target_tagged: bool,
}

/// One unit's journal as sorted columns, relative to it.
pub(crate) struct Index<'a> {
    /// The names told, sorted.
    files: Vec<&'a str, &'a Frame>,
    /// Each told name as (its folder, its last part), sorted.
    children: Vec<(&'a str, &'a str), &'a Frame>,
    lost: Vec<&'a str, &'a Frame>,
}

impl<'a> Index<'a> {
    /// The index of one unit's answer (sans-IO): `files`, the names told (in any order, a name
    /// possibly twice), and `lost`, the subtrees the journal lost.
    /// `skip_own`: `Some` when the unit is the checkout's own folder, where blaze's, git's and
    /// (tagged) cargo's folders are read by no row. `None` when the unit's folder itself is
    /// lost: every row of it is looked at ([`judge`] refuses such a unit first; this keeps the
    /// index sound alone).
    pub(crate) fn of(files: impl Iterator<Item = &'a str>, lost: impl Iterator<Item = (&'a str, Lost)>, skip_own: Option<OwnFolders>, plant: JournalPlant, frame: &'a Frame) -> Option<Index<'a>> {
        let mut index = Index { files: Vec::new_in(frame), children: Vec::new_in(frame), lost: Vec::new_in(frame) };
        let kept = |path: &str| skip_own.is_none_or(|folders| !own(path, folders.target_tagged));
        for file in files.filter(|it| kept(it)) {
            index.files.push(file);
            let (folder, last) = file.rsplit_once('/').unwrap_or(("", file));
            index.children.push((folder, last));
        }
        for (under, _) in lost.filter(|(it, _)| kept(it)) {
            if under.is_empty() {
                return None;
            }
            if plant == JournalPlant::LostIgnored {
                continue;
            }
            index.lost.push(under);
            // A folder lost: the listing it is in is looked at too.
            let (folder, last) = under.rsplit_once('/').unwrap_or(("", under));
            index.children.push((folder, last));
        }
        // One run each from a replay (bstd `Changes::files`) or a helper, but two runs under
        // `Both` ([`Heard::files`]): sorted here, each once (`touched` searches it).
        index.files.sort_unstable();
        index.files.dedup();
        index.children.sort_unstable();
        Some(index)
    }

    /// Whether a row of `kind` at `rel` (relative to the unit) is reached by what changed.
    fn reaches(&self, kind: LeafKind, flags: u8, rel: &str, plant: JournalPlant) -> bool {
        match kind {
            LeafKind::Slot if flags & MODULE != 0 => {
                // `x.rs` and `x/mod.rs` are both probed: either named moves the slot.
                let stem = rel.strip_suffix(".rs").unwrap_or(rel);
                self.touched(rel, plant) || self.touched(stem, plant) || self.files.binary_search_by(|it| joined_cmp(it, stem, "/mod.rs")).is_ok()
            }
            LeafKind::Slot | LeafKind::Manifest | LeafKind::MacroFile => self.touched(rel, plant),
            LeafKind::Listing => {
                let last = rel.rsplit('/').next().unwrap_or(rel);
                // Only a name the listing hands the layout moves it (sweep.rs `wanted`); a
                // publish's temporary name beside a source file does not. A root `target` is
                // asked as if untagged (it may be listed), and a change to cargo's tag (or a
                // lost `target`) moves the root's listing: the tag decides whether it holds
                // `target`.
                self.touched(rel, plant)
                    || self.children_of(rel).any(|name| sweep::wanted(last, name, true, false) || sweep::wanted(last, name, false, false))
                    || (rel.is_empty() && self.touched(sweep::TARGET_TAG, plant))
            }
            LeafKind::MacroFolder | LeafKind::HomeListing => self.touched(rel, plant) || self.children_of(rel).next().is_some(),
            // A build script's read (engine_script_reads.rs): a file's bytes and a path's stat
            // move only with the path or a folder above it; a listing with any entry of it.
            LeafKind::ScriptFile if rel.starts_with(sweep::FACT_MOUNT) => true,
            LeafKind::ScriptFile | LeafKind::ScriptStat => self.touched(rel, plant),
            // A git fact is read from `.git` files the journal may not see: always followed.
            LeafKind::ScriptGit => true,
            LeafKind::ScriptListing => self.touched(rel, plant) || self.children_of(rel).next().is_some(),
            // Decided by its files' and folders' rows (macro_files.rs `proven`).
            LeafKind::MacroFiles => false,
        }
    }

    /// Whether `path`, or a folder above it, is named or lost.
    fn touched(&self, path: &str, plant: JournalPlant) -> bool {
        if self.files.binary_search(&path).is_ok() {
            return true;
        }
        let above = plant != JournalPlant::FolderNameIgnored && path.char_indices().filter(|(_, c)| *c == '/').any(|(at, _)| self.files.binary_search(&&path[..at]).is_ok());
        above || self.lost.iter().any(|under| path == *under || path.strip_prefix(under).is_some_and(|rest| rest.starts_with('/')))
    }

    /// The last parts of the names told directly in `folder`.
    fn children_of<'s>(&'s self, folder: &'s str) -> impl Iterator<Item = &'a str> + 's {
        let start = self.children.partition_point(|(it, _)| *it < folder);
        self.children[start..].iter().take_while(move |(it, _)| *it == folder).map(|(_, name)| *name)
    }
}

/// Racy-stamp margin for comparing external linked-worktree gitdir timestamps against
/// `stored.taken_wall_ns()` (matches `sweep::RACY_NS`).
#[cfg(unix)]
const GITDIR_RACY_NS: u64 = 2_000_000_000;

/// Maximum bytes read from `.git`, `HEAD`, or `commondir` when inspecting a linked git worktree.
#[cfg(unix)]
const GIT_FILE_MAX_BYTES: usize = 4096;

/// Parses a linked worktree/submodule `.git` file (`gitdir: <path>`) into its trimmed target path.
fn parse_gitdir(bytes: &[u8]) -> Option<&str> {
    let text = core::str::from_utf8(bytes).ok()?;
    let line = text.lines().next()?;
    let target = line.strip_prefix("gitdir:")?.trim();
    (!target.is_empty()).then_some(target)
}

/// Parsed contents of a git `HEAD` file.
#[cfg(any(unix, test))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HeadRef<'a> {
    /// Symbolic ref (e.g. `ref: refs/heads/main`), validated to be a plain relative ref path.
    Symbolic(&'a str),
    /// Detached commit object ID (40 SHA-1 or 64 SHA-256 hex digits).
    Detached,
    /// Anything else: fall back conservatively to forcing git script checks.
    Malformed,
}

#[cfg(any(unix, test))]
fn parse_head_ref(bytes: &[u8]) -> HeadRef<'_> {
    let Ok(text) = core::str::from_utf8(bytes) else {
        return HeadRef::Malformed;
    };
    let line = text.lines().next().map_or("", str::trim);
    if let Some(rest) = line.strip_prefix("ref:") {
        let refname = rest.trim();
        let plain = !refname.is_empty()
            && !refname.starts_with('/')
            && !refname.split('/').any(|part| matches!(part, "" | "." | ".."));
        if plain {
            return HeadRef::Symbolic(refname);
        }
        return HeadRef::Malformed;
    }
    if (line.len() == 40 || line.len() == 64) && line.bytes().all(|b| b.is_ascii_hexdigit()) {
        return HeadRef::Detached;
    }
    HeadRef::Malformed
}

/// Normalizes `base` + `rel` as a checkout-relative path; returns `None` if `rel` is absolute or
/// escapes above the checkout root via `..`.
fn normalize_rel_inside_checkout<'f>(base: &str, rel: &str, frame: &'f Frame) -> Option<blaze_own::String<&'f Frame>> {
    if rel.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str, &Frame> = Vec::new_in(frame);
    for part in base.split('/').chain(rel.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    let mut out = blaze_own::String::new_in(frame);
    for (idx, part) in parts.iter().enumerate() {
        if idx > 0 {
            out.push('/');
        }
        out.push_str(part);
    }
    Some(out)
}

/// Resolves `target` (absolute or relative to `base_abs`) into a normalized absolute path.
#[cfg(any(unix, test))]
fn resolve_abs_path<'f>(base_abs: &str, target: &str, frame: &'f Frame) -> Option<blaze_own::String<&'f Frame>> {
    if !base_abs.starts_with('/') && !target.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str, &Frame> = Vec::new_in(frame);
    let source = if target.starts_with('/') { "" } else { base_abs };
    for part in source.split('/').chain(target.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    let mut out = blaze_own::String::new_in(frame);
    out.push('/');
    for (idx, part) in parts.iter().enumerate() {
        if idx > 0 {
            out.push('/');
        }
        out.push_str(part);
    }
    Some(out)
}

/// Splits a normalized absolute path with at least two components (e.g. `"/a/b/c"` or `"/a/b"`)
/// into `(grandparent_abs, parent_leaf, target_leaf)` so `fs.home(cx, grandparent_abs, parent_leaf)`
/// opens the target's parent directory and `target_leaf` can be statted directly inside it.
fn split_abs_parent(abs: &str) -> Option<(&str, &str, &str)> {
    let (parent, leaf) = abs.rsplit_once('/')?;
    if leaf.is_empty() || parent.is_empty() {
        return None;
    }
    let (gp_raw, p_leaf) = parent.rsplit_once('/')?;
    if p_leaf.is_empty() {
        return None;
    }
    let gp = if gp_raw.is_empty() { "/" } else { gp_raw };
    Some((gp, p_leaf, leaf))
}

/// Opens the parent directory of normalized absolute path `abs` via `bstd::fs::Fs`, returning
/// `(parent_dir, leaf_name)` so `leaf_name` and paths beneath it can be checked through `bstd`.
fn open_parent_of_abs<'a>(cx: &Cx, fs: &Fs, abs: &'a str) -> Option<(Dir<ReadWrite>, &'a str)> {
    let (gp, p_leaf, leaf) = split_abs_parent(abs)?;
    let p_path = Path::new(p_leaf).ok()?;
    let dir = fs.home(cx, gp, &p_path).ok()?;
    Some((dir, leaf))
}

/// Whether `rel` inside `dir` (an external linked gitdir's parent) may have changed at or after
/// `cutoff_ns`. Returns `true` conservatively on any unexpected I/O error or missing required path.
fn dir_path_changed_since(cx: &Cx, dir: &Dir<ReadWrite>, rel: &str, cutoff_ns: u64, required: bool) -> bool {
    let Ok(path) = Path::new(rel) else {
        return true;
    };
    let meta = match dir.stat(cx, &path) {
        Ok(Some(meta)) => meta,
        Ok(None) if !required => return false,
        Ok(None) | Err(_) => return true,
    };
    meta.stamp.modified_ns >= cutoff_ns || meta.stamp.changed_ns >= cutoff_ns
}

/// Reads a small git metadata file (`HEAD` or `commondir`) into `frame` through `bstd::fs`.
/// Returns `None` if missing, a directory, unreadable, or larger than `GIT_FILE_MAX_BYTES`.
fn read_small_dir_file<'f>(cx: &Cx, dir: &Dir<ReadWrite>, rel: &str, frame: &'f Frame) -> Option<Bytes<'f>> {
    let path = Path::new(rel).ok()?;
    let meta = dir.stat(cx, &path).ok()??;
    if meta.dir || usize::try_from(meta.len).ok()? > GIT_FILE_MAX_BYTES {
        return None;
    }
    let mut bytes = Bytes::new_in(frame, "linked worktree external git metadata");
    dir.read_all(cx, &path, &mut bytes).ok()?;
    if bytes.as_slice().len() > GIT_FILE_MAX_BYTES {
        return None;
    }
    Some(bytes)
}

/// Checks whether a linked git worktree's external `gitdir` (and `commondir`) had any commit,
/// checkout, stage, tag, or pack-refs change since `stored_taken_ns` (with a 2-second racy-stamp
/// margin). Returns `true` conservatively if no prior timestamp exists, the wall clock stepped
/// backwards, or any required file could not be verified.
fn external_gitdir_changed(cx: &Cx, fs: &Fs, gitdir: &str, stored_taken_ns: Option<u64>, now_wall_ns: u64, frame: &Frame) -> bool {
    let Some(prev_ns) = stored_taken_ns else {
        return true;
    };
    if now_wall_ns < prev_ns {
        return true;
    }
    let Some(cutoff_ns) = prev_ns.checked_sub(GITDIR_RACY_NS) else {
        return true;
    };
    let Some(norm_gitdir) = resolve_abs_path("/", gitdir, frame) else {
        return true;
    };
    let gitdir_str = norm_gitdir.as_str();
    let Some((gitdir_parent, gitdir_leaf)) = open_parent_of_abs(cx, fs, gitdir_str) else {
        return true;
    };
    let head_rel = bstd::mem::format_in!(frame, "{gitdir_leaf}/HEAD");
    let index_rel = bstd::mem::format_in!(frame, "{gitdir_leaf}/index");
    let commondir_rel = bstd::mem::format_in!(frame, "{gitdir_leaf}/commondir");
    if dir_path_changed_since(cx, &gitdir_parent, gitdir_leaf, cutoff_ns, true)
        || dir_path_changed_since(cx, &gitdir_parent, head_rel.as_str(), cutoff_ns, true)
        || dir_path_changed_since(cx, &gitdir_parent, index_rel.as_str(), cutoff_ns, false)
        || dir_path_changed_since(cx, &gitdir_parent, commondir_rel.as_str(), cutoff_ns, false)
    {
        return true;
    }
    let commondir_resolved = match Path::new(commondir_rel.as_str()).ok().and_then(|p| gitdir_parent.stat(cx, &p).ok()) {
        Some(Some(_)) => {
            let Some(common_bytes) = read_small_dir_file(cx, &gitdir_parent, commondir_rel.as_str(), frame) else {
                return true;
            };
            let Ok(text) = core::str::from_utf8(common_bytes.as_slice()) else {
                return true;
            };
            let rel = text.lines().next().map_or("", str::trim);
            if rel.is_empty() {
                return true;
            }
            let Some(resolved) = resolve_abs_path(gitdir_str, rel, frame) else {
                return true;
            };
            Some(resolved)
        }
        Some(None) => None,
        None => return true,
    };
    let common_str = commondir_resolved.as_ref().map_or(gitdir_str, blaze_own::String::as_str);
    let common_opened = if common_str != gitdir_str {
        let Some(opened) = open_parent_of_abs(cx, fs, common_str) else {
            return true;
        };
        Some(opened)
    } else {
        None
    };
    let (common_parent, common_leaf) = match &common_opened {
        Some((dir, leaf)) => (dir, *leaf),
        None => (&gitdir_parent, gitdir_leaf),
    };
    let packed_refs = bstd::mem::format_in!(frame, "{common_leaf}/packed-refs");
    let refs_tags = bstd::mem::format_in!(frame, "{common_leaf}/refs/tags");
    if (common_str != gitdir_str && dir_path_changed_since(cx, common_parent, common_leaf, cutoff_ns, true))
        || dir_path_changed_since(cx, common_parent, packed_refs.as_str(), cutoff_ns, false)
        || dir_path_changed_since(cx, common_parent, refs_tags.as_str(), cutoff_ns, false)
    {
        return true;
    }
    let Some(head_bytes) = read_small_dir_file(cx, &gitdir_parent, head_rel.as_str(), frame) else {
        return true;
    };
    match parse_head_ref(head_bytes.as_slice()) {
        HeadRef::Detached => false,
        HeadRef::Malformed => true,
        HeadRef::Symbolic(refname) => {
            let ref_in_common = bstd::mem::format_in!(frame, "{common_leaf}/{refname}");
            if dir_path_changed_since(cx, common_parent, ref_in_common.as_str(), cutoff_ns, false) {
                return true;
            }
            if let Some((parent, _)) = refname.rsplit_once('/') {
                let parent_dir = bstd::mem::format_in!(frame, "{common_leaf}/{parent}");
                if dir_path_changed_since(cx, common_parent, parent_dir.as_str(), cutoff_ns, false) {
                    return true;
                }
            }
            if common_str != gitdir_str {
                let ref_in_gitdir = bstd::mem::format_in!(frame, "{gitdir_leaf}/{refname}");
                if dir_path_changed_since(cx, &gitdir_parent, ref_in_gitdir.as_str(), cutoff_ns, false) {
                    return true;
                }
            }
            false
        }
    }
}

/// Whether a checkout's `.git` entry requires forcing `ScriptGit` / `.git` script checks because
/// its git metadata lives outside the watched checkout directory and may have moved.
fn worktree_git_needs_check(cx: &Cx, fs: &Fs, root: &Dir<ReadWrite>, stored_taken_ns: Option<u64>, now_wall_ns: u64, frame: &Frame) -> Result<bool, CliError> {
    let Some(meta) = root.stat(cx, &Path::new(sweep::GIT)?)? else {
        return Ok(false);
    };
    if meta.dir {
        return Ok(false);
    }
    let mut bytes = Bytes::new_in(frame, "linked worktree `.git` pointer");
    match root.read_all(cx, &Path::new(sweep::GIT)?, &mut bytes) {
        Ok(_) => {}
        Err(error) if error.kind == FsKind::NotFound => return Ok(false),
        Err(_) => return Ok(true),
    }
    let Some(target) = parse_gitdir(bytes.as_slice()) else {
        return Ok(true);
    };
    if !target.starts_with('/') {
        // Relative `gitdir:`: if it stays inside the checkout (along with any `commondir`), the
        // checkout's own journal already covers every file inside it.
        let Some(rel_gitdir) = normalize_rel_inside_checkout("", target, frame) else {
            return Ok(true);
        };
        if rel_gitdir.is_empty() {
            return Ok(true);
        }
        let commondir_rel = bstd::mem::format_in!(frame, "{}/commondir", rel_gitdir.as_str());
        let Ok(commondir_path) = Path::new(commondir_rel.as_str()) else {
            return Ok(true);
        };
        let mut common_bytes = Bytes::new_in(frame, "linked worktree `commondir`");
        return match root.read_all(cx, &commondir_path, &mut common_bytes) {
            Err(error) if matches!(error.kind, FsKind::NotFound | FsKind::IsADirectory) => Ok(false),
            Err(_) => Ok(true),
            Ok(_) => {
                let Ok(text) = core::str::from_utf8(common_bytes.as_slice()) else {
                    return Ok(true);
                };
                let rel = text.lines().next().map_or("", str::trim);
                if rel.is_empty() {
                    return Ok(true);
                }
                Ok(normalize_rel_inside_checkout(rel_gitdir.as_str(), rel, frame).is_none())
            }
        };
    }
    if cx.is_simulated() {
        return Ok(true);
    }
    Ok(external_gitdir_changed(cx, fs, target, stored_taken_ns, now_wall_ns, frame))
}

/// `a` against `x` followed by `y`, by bytes, with nothing joined.
fn joined_cmp(a: &str, x: &str, y: &str) -> Ordering {
    a.bytes().cmp(x.bytes().chain(y.bytes()))
}

#[cfg(test)]
#[path = "tests/journal_sweep_tests.rs"]
mod tests;

#[cfg(test)]
mod worktree_git_tests {
    use super::*;
    use bstd::mem::Tag;

    #[test]
    fn parse_gitdir_and_head_ref_handle_valid_and_malformed_inputs() {
        assert_eq!(parse_gitdir(b"gitdir: /repo/.git/worktrees/wt1\n"), Some("/repo/.git/worktrees/wt1"));
        assert_eq!(parse_gitdir(b"gitdir: ../.git/modules/sub\r\n"), Some("../.git/modules/sub"));
        assert_eq!(parse_gitdir(b"not a gitdir file\n"), None);
        assert_eq!(parse_gitdir(b"gitdir:   \n"), None);

        assert_eq!(parse_head_ref(b"ref: refs/heads/main\n"), HeadRef::Symbolic("refs/heads/main"));
        assert_eq!(parse_head_ref(b"0123456789abcdef0123456789abcdef01234567\n"), HeadRef::Detached);
        assert_eq!(parse_head_ref(b"ref: ../escape\n"), HeadRef::Malformed);
        assert_eq!(parse_head_ref(b"ref: /abs/ref\n"), HeadRef::Malformed);
        assert_eq!(parse_head_ref(b"garbage\n"), HeadRef::Malformed);
    }

    #[test]
    fn path_normalization_keeps_inside_checkout_and_resolves_commondir() {
        let frame = Frame::with_reserve(1 << 20, Tag::Other).expect("test frame");
        assert_eq!(normalize_rel_inside_checkout("", ".git/modules/a", &frame).as_ref().map(blaze_own::String::as_str), Some(".git/modules/a"));
        assert_eq!(normalize_rel_inside_checkout(".git/worktrees/wt", "../..", &frame).as_ref().map(blaze_own::String::as_str), Some(".git"));
        assert_eq!(normalize_rel_inside_checkout(".git/worktrees/wt", "../../../outside", &frame).as_ref().map(blaze_own::String::as_str), Some("outside"));
        assert_eq!(normalize_rel_inside_checkout(".git/worktrees/wt", "../../../../outside", &frame), None);
        assert_eq!(normalize_rel_inside_checkout("", "/abs/path", &frame), None);

        assert_eq!(
            resolve_abs_path("/Volumes/Projects/blaze/.git/worktrees/sota-v4", "../..", &frame).as_ref().map(blaze_own::String::as_str),
            Some("/Volumes/Projects/blaze/.git"),
        );
        assert_eq!(
            resolve_abs_path("/a/b", "/x/y/../z", &frame).as_ref().map(blaze_own::String::as_str),
            Some("/x/z"),
        );
    }
}
