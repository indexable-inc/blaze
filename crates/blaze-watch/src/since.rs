//! What changed in a checkout since a request's saved place, on a system that keeps no journal
//! of changes (Linux): the core of the change watcher a request leaves behind (plan row S1;
//! rule 70: a helper with an expiry that holds only what records rebuild).
//!
//! **The seam this answers** is the one the macOS sweep already takes (bstd `Dir::changes_since`,
//! blaze-cli journal_sweep.rs): `since(place) -> Changes | CannotVouch { why }`, where asking
//! from no place is `place now`. `Changes` is complete for the interval: every path whose bytes
//! or existence moved after the place is named, or a folder above it is (a named folder covers
//! everything under it, as journal_sweep's `touched` reads it). `CannotVouch` is never a quiet
//! fallback (rule 3): it is a typed why, printed, and every path is statted that request. Both
//! sources fit it: FSEvents' place is (volume store UUID, event id), this one's is [`Place`]
//! (helper instance, sequence number); a place of another store, or another instance, cannot be
//! vouched for.
//!
//! **Why a helper, on Linux.** The honest options, and what each cannot see:
//!
//! - fanotify with `FAN_REPORT_DFID_NAME` marks a whole mount in one call, but needs
//!   `CAP_SYS_ADMIN`: a plain user has none. It is a live watch too, so it needs a process as
//!   inotify does; blaze-core's watch (watch.rs) already takes it where it is granted.
//! - inotify watches one folder each, and only while a process holds the descriptor. Nothing of
//!   it outlives that process: no replay from a saved place exists. So "since the last request"
//!   needs a process alive between requests: this helper, which the owner allows (2026-11-17:
//!   "a live process is allowed, but a restart must not re-index"). It holds only the set of
//!   names heard since the places it issued; when it is gone (expired, killed, a reboot) or
//!   cannot vouch (the queue overflowed: `IN_Q_OVERFLOW`; the root moved; the watch limit
//!   `fs.inotify.max_user_watches` refused a folder), the answer is [`CannotVouch`] and the
//!   request stats every path, then starts a new helper. What it cannot see: a change made
//!   before its first flush (no place exists before it, so none is vouched for), and changes on
//!   another mount under the checkout made by another namespace (out of scope).
//! - Folder mtimes need no process, but a folder's mtime moves when an entry is made, removed
//!   or renamed in it, not when a file's bytes are written: they cannot vouch for an edit, and
//!   statting every file is what this avoids.
//!
//! So the helper. The tree already has the inotify (and fanotify) watch with its hard parts:
//! a folder made after the start is watched by a walk that adds the watch before it lists, so a
//! file made in the race is listed or heard (blaze-core watch.rs, `Grow`, tested by its control
//! `GROW_DROPS_FOUND`); a full queue is `lost`; a refused watch names
//! `fs.inotify.max_user_watches` and the count. And it has the barrier: `Effect::Flush`
//! completes once every change queued when it was asked has been told. This core is the part
//! above that watch, sans-IO (rule 14): it takes what the watch tells, the requests' asks and
//! the flushes' completions, and decides places and answers. One concept, one home (rule 30):
//! inotify's cookies and watch descriptors stay in watch.rs.
//!
//! **Places and tags.** A place is issued when a flush completes, numbered from 1 within an
//! instance and never reused (the instance id is random, passed in: rule 75). A name heard while
//! `seq` was the newest place issued is kept with tag `seq`: it happened after the flush that
//! issued `seq` was asked or later. `since(P)` names every kept name tagged `>= P.seq`. Why this
//! is complete: a change made after place `P` was issued is heard after it, so tagged `>= P.seq`;
//! a change made before the request asked is queued before the request's flush is asked, so it
//! is heard before that flush completes, before the answer. A change heard before `P` was issued
//! but made after it cannot be: issuing is the flush's completion, which a request's reads come
//! after (the place is taken before the files are read, journal_sweep.rs). Some changes are
//! named twice over (made between a flush's ask and its completion): more statted, never fewer.
//!
//! **One flush per ask, asked after it arrived.** An ask that comes while a flush is out waits
//! for the next one: the flush out was asked before the ask, so changes made between the two
//! may still be queued when it completes.
//!
//! **Memory is fixed when the core is made** (rule 18): at most `most_paths` names of
//! `most_bytes` bytes, `most_asks` asks waiting. When the names are full they are compacted
//! (each name once, its newest tag); then the places older than the newest one a request came
//! back with are forgotten ([`CannotVouch::Forgotten`]); then all are. A forgotten place is a
//! typed refusal, never a silent miss.
//!
//! **Expiry** (rule 70), by the time the driver passes in (rule 75): at the first tick `idle`
//! after the last ask, or `held_most` after the start, whatever is waiting.

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, Vec};

use crate::Nanos;

/// The most bytes of one name: Linux's `PATH_MAX`.
pub const MOST_NAME: usize = 4096;

/// A place in one helper's stream of changes: a later [`Watcher`] ask from it is answered with
/// every change made after it, or refused.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Place {
    /// The helper's instance, random per start: a place of another is refused.
    pub instance: u64,
    /// The place's number in that instance, from 1, never reused.
    pub seq: u64,
}

impl Place {
    /// Bytes of [`Place::encode`].
    pub const BYTES: usize = 16;

    /// Little-endian, as a pointer keeps it.
    pub fn encode(&self) -> [u8; Place::BYTES] {
        let mut out = [0; Place::BYTES];
        out[..8].copy_from_slice(&self.instance.to_le_bytes());
        out[8..].copy_from_slice(&self.seq.to_le_bytes());
        out
    }

    /// Of [`Place::encode`]'s bytes; `None` for fewer than `BYTES` or a seq of 0 (no place).
    pub fn decode(bytes: &[u8]) -> Option<Place> {
        let instance = u64::from_le_bytes(bytes.get(..8)?.try_into().ok()?);
        let seq = u64::from_le_bytes(bytes.get(8..16)?.try_into().ok()?);
        (seq > 0).then_some(Place { instance, seq })
    }
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}:{}", self.instance, self.seq)
    }
}

/// Why the watch can no longer vouch for anything: the helper is to exit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    /// The system dropped changes (inotify's `IN_Q_OVERFLOW`; the watch's `lost`).
    Overflowed,
    /// The root itself was named: moved, replaced, removed or unmounted.
    RootChanged,
    /// The watch refused a folder: `watched` folders were watched, `limit` is
    /// `fs.inotify.max_user_watches` where the driver read it.
    WatchRefused { watched: u64, limit: Option<u64> },
    /// The watch, or a flush of it, failed otherwise; the driver's log names the error.
    WatchFailed,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::Overflowed => f.write_str("the system dropped changes (inotify's queue overflowed)"),
            Fault::RootChanged => f.write_str("the checkout's folder was moved, replaced or unmounted"),
            Fault::WatchRefused { watched, limit: Some(limit) } => write!(
                f,
                "the system refused a watch after {watched} folders (fs.inotify.max_user_watches is {limit}; raise it with sysctl)"
            ),
            Fault::WatchRefused { watched, limit: None } => {
                write!(f, "the system refused a watch after {watched} folders (fs.inotify.max_user_watches)")
            }
            Fault::WatchFailed => f.write_str("the watch failed"),
        }
    }
}

/// Why the helper ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Death {
    /// Its expiry passed (rule 70).
    Expired,
    /// Its binary was replaced: its file changed, or a blaze of another build asked
    /// (helper.rs: the version is in every hello and every ask). What it holds is only what
    /// the journal rebuilds, so the new build's helper starts from nothing.
    Replaced,
    Fault(Fault),
}

impl fmt::Display for Death {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Death::Expired => f.write_str("the change watcher expired"),
            Death::Replaced => f.write_str("the change watcher's binary was replaced (its file changed, or a blaze of another build asked)"),
            Death::Fault(fault) => write!(f, "{fault}"),
        }
    }
}

/// Why an answer cannot name what changed: every path is statted that request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CannotVouch {
    /// Nothing answered on the helper's socket (none started, expired, killed, a reboot): the
    /// request's side says it, and starts one.
    NoHelper,
    /// The request saved no place (its first, or the last one could not be vouched for).
    NoPlace,
    /// The place is of another instance: the helper that issued it is gone, and what changed
    /// after it died is unknown.
    OtherInstance { asked: u64, here: u64 },
    /// The place was never issued here.
    NotIssued { seq: u64, newest: u64 },
    /// The place is older than what the names kept reach ([`Watcher`]'s memory).
    Forgotten { seq: u64, from: u64 },
    /// The helper is ending, for this.
    Dead(Death),
    /// More asks wait than the helper holds.
    TooManyAsks { most: usize },
}

impl fmt::Display for CannotVouch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the change watcher cannot vouch for what changed since the saved place (")?;
        match self {
            CannotVouch::NoHelper => f.write_str("no watcher answered; one is started"),
            CannotVouch::NoPlace => f.write_str("no place was saved"),
            CannotVouch::OtherInstance { asked, here } => {
                write!(f, "the place is of watcher {asked:016x}, which is gone; this one is {here:016x}")
            }
            CannotVouch::NotIssued { seq, newest } => write!(f, "place {seq} was never issued; the newest is {newest}"),
            CannotVouch::Forgotten { seq, from } => {
                write!(f, "place {seq} is older than the names kept, which reach back to place {from}")
            }
            CannotVouch::Dead(death) => write!(f, "{death}"),
            CannotVouch::TooManyAsks { most } => write!(f, "more than {most} asks wait"),
        }?;
        f.write_str("): every path is statted this request")
    }
}

/// An ask, as the driver numbers it (its connection).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct AskId(pub u64);

/// The answer to an ask.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    /// Every change since `since` is named by [`Watcher::names_since`] (or a folder above it
    /// is); `place` is the place to save, issued after those were heard.
    Vouched { since: Place, place: Place },
    /// Nothing is named; `place` is the place to save where one was issued.
    CannotVouch { why: CannotVouch, place: Option<Place> },
}

/// What the driver does after a call.
#[must_use]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Step {
    /// Ask the watch for a flush now ([`Watcher::flushed`] when it completes).
    pub flush: bool,
    /// Write the answers waiting ([`Watcher::take_answer`]), then exit, for this.
    pub exit: Option<Death>,
}

/// A call the driver should not have made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SinceError {
    /// The tables could not be had when the core was made.
    NoMemory(AllocFailure),
    /// A flush completed that the core did not ask for.
    FlushNotAsked,
    /// The answers were not taken before the next call: at most `most` wait.
    AnswersNotTaken { most: usize },
}

impl fmt::Display for SinceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SinceError::NoMemory(why) => write!(f, "the change watcher's tables: {why}"),
            SinceError::FlushNotAsked => f.write_str("the change watcher heard a flush it did not ask for"),
            SinceError::AnswersNotTaken { most } => {
                write!(f, "the change watcher's driver left {most} answers untaken (since.rs `take_answer`)")
            }
        }
    }
}

impl core::error::Error for SinceError {}

/// The core's sizes and times.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// The most names kept.
    pub most_paths: usize,
    /// The most bytes of names kept (twice this is reserved: compaction copies).
    pub most_bytes: usize,
    /// The most asks waiting for a flush, and answers waiting to be taken.
    pub most_asks: usize,
    /// The helper exits this long after the last ask.
    pub idle: Nanos,
    /// And this long after its start, whatever.
    pub held_most: Nanos,
}

impl Config {
    /// An edit loop's pauses are minutes; a request after the expiry costs one full sweep
    /// and a new helper's walk. 64 Ki names of 8 MiB is a rebase's worth of changes.
    pub const fn new() -> Config {
        Config { most_paths: 1 << 16, most_bytes: 8 << 20, most_asks: 64, idle: Nanos::secs(30 * 60), held_most: Nanos::secs(12 * 3600) }
    }
}

/// A planted bug, for the simulation's controls (rule 53): always compiled, a value passed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SincePlant {
    None,
    /// An ask is answered at once, without a flush (and before the first flush too).
    NoFlush,
    /// An ask that comes while a flush is out is answered by that flush.
    JoinOutstandingFlush,
    /// A batch the watch says lost changes in is taken as whole.
    OverflowIgnored,
    /// A place of another instance is taken as this one's.
    OtherInstanceAccepted,
    /// Names are dropped to make room without forgetting the places they served.
    ForgetSilently,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// The first flush is out: the watch is not known to be in place.
    Starting,
    Live,
    Dead(Death),
}

/// A name kept: bytes `at..at + len` of the names, heard while place `tag` was the newest.
#[derive(Clone, Copy)]
struct Kept {
    at: u32,
    len: u32,
    tag: u64,
}

#[derive(Clone, Copy)]
struct Waiting {
    id: AskId,
    since: Option<Place>,
    /// The flush out was asked after this ask came: its completion answers it.
    covered: bool,
}

/// The change watcher's core (the module's doc).
pub struct Watcher<A: Allocator + Copy> {
    config: Config,
    instance: u64,
    state: State,
    /// The newest place issued; 0 before the first.
    seq: u64,
    /// The oldest place the names kept vouch for.
    vouch_from: u64,
    /// The newest place a request came back with and was vouched for.
    newest_asked: u64,
    kept: Vec<Kept, A>,
    names: Vec<u8, A>,
    /// Where a compaction copies the names it keeps.
    spare: Vec<u8, A>,
    waiting: Vec<Waiting, A>,
    answers: Vec<(AskId, Answer), A>,
    flush_out: bool,
    born: Nanos,
    last_ask: Nanos,
    plant: SincePlant,
}

impl<A: Allocator + Copy> Watcher<A> {
    /// A core for instance `instance` (random: rule 75), made at `now`, its tables in `alloc`
    /// (all of them, once). Made with its first flush out: the driver starts the watch of the
    /// checkout and asks a flush of it, whose completion is the first place.
    pub fn new_in(config: Config, instance: u64, now: Nanos, alloc: A) -> Result<Watcher<A>, SinceError> {
        Watcher::planted_in(config, instance, now, alloc, SincePlant::None)
    }

    pub(crate) fn planted_in(config: Config, instance: u64, now: Nanos, alloc: A, plant: SincePlant) -> Result<Watcher<A>, SinceError> {
        // A name's offset is a u32.
        let bytes = config.most_bytes.min(usize::try_from(u32::MAX).unwrap_or(usize::MAX));
        Ok(Watcher {
            config: Config { most_bytes: bytes, ..config },
            instance,
            state: State::Starting,
            seq: 0,
            vouch_from: 1,
            newest_asked: 0,
            kept: Vec::try_with_capacity_in(config.most_paths, alloc).map_err(SinceError::NoMemory)?,
            names: Vec::try_with_capacity_in(bytes, alloc).map_err(SinceError::NoMemory)?,
            spare: Vec::try_with_capacity_in(bytes, alloc).map_err(SinceError::NoMemory)?,
            waiting: Vec::try_with_capacity_in(config.most_asks, alloc).map_err(SinceError::NoMemory)?,
            answers: Vec::try_with_capacity_in(config.most_asks, alloc).map_err(SinceError::NoMemory)?,
            flush_out: true,
            born: now,
            last_ask: now,
            plant,
        })
    }

    pub fn instance(&self) -> u64 {
        self.instance
    }

    /// Whether the helper is ending (the answers left to write, then exit).
    pub fn is_dead(&self) -> bool {
        matches!(self.state, State::Dead(_))
    }

    /// The watch told `names` (relative to the checkout, `""` the checkout itself); `lost`: it
    /// dropped some it cannot name.
    pub fn heard<'n>(&mut self, names: impl IntoIterator<Item = &'n str>, lost: bool) -> Result<Step, SinceError> {
        if self.is_dead() {
            // Every ask is refused from here: nothing heard can be used.
            return Ok(Step::default());
        }
        if lost && self.plant != SincePlant::OverflowIgnored {
            return self.die(Death::Fault(Fault::Overflowed));
        }
        for name in names {
            if name.is_empty() {
                return self.die(Death::Fault(Fault::RootChanged));
            }
            self.keep(name);
        }
        Ok(Step::default())
    }

    /// The watch, or a flush of it, failed.
    pub fn failed(&mut self, fault: Fault) -> Result<Step, SinceError> {
        if self.is_dead() {
            return Ok(Step::default());
        }
        self.die(Death::Fault(fault))
    }

    /// The helper's binary was replaced ([`Death::Replaced`]): every ask waiting, and every
    /// later one, is refused for it, and the helper exits.
    pub fn replaced(&mut self) -> Result<Step, SinceError> {
        if self.is_dead() {
            return Ok(Step::default());
        }
        self.die(Death::Replaced)
    }

    /// A request asks what changed since `since` (`None`: it saved no place), at `now`.
    pub fn asked(&mut self, id: AskId, since: Option<Place>, now: Nanos) -> Result<Step, SinceError> {
        self.last_ask = now;
        if let State::Dead(death) = self.state {
            self.answer(id, Answer::CannotVouch { why: CannotVouch::Dead(death), place: None })?;
            return Ok(Step::default());
        }
        if self.plant == SincePlant::NoFlush {
            // The planted bug: no barrier, so changes still queued are not yet heard.
            let place = self.issue();
            let answer = self.answer_for(since, place);
            self.answer(id, answer)?;
            return Ok(Step::default());
        }
        if self.waiting.len() == self.config.most_asks {
            let most = self.config.most_asks;
            self.answer(id, Answer::CannotVouch { why: CannotVouch::TooManyAsks { most }, place: None })?;
            return Ok(Step::default());
        }
        let covered = !self.flush_out || self.plant == SincePlant::JoinOutstandingFlush;
        // Within the capacity reserved when made: checked above.
        self.waiting.push(Waiting { id, since, covered });
        if self.flush_out {
            return Ok(Step::default());
        }
        self.flush_out = true;
        Ok(Step { flush: true, exit: None })
    }

    /// The flush asked last completed: every change queued when it was asked has been heard.
    pub fn flushed(&mut self) -> Result<Step, SinceError> {
        if !self.flush_out {
            return Err(SinceError::FlushNotAsked);
        }
        self.flush_out = false;
        if self.is_dead() {
            return Ok(Step::default());
        }
        self.state = State::Live;
        let place = self.issue();
        let mut at = 0;
        let mut more = false;
        while at < self.waiting.len() {
            if !self.waiting[at].covered {
                // Came while this flush was out: the next one, asked now, covers it.
                self.waiting[at].covered = true;
                more = true;
                at += 1;
                continue;
            }
            let ask = self.waiting.remove_shift(at);
            let answer = self.answer_for(ask.since, place);
            self.answer(ask.id, answer)?;
        }
        if more {
            self.flush_out = true;
        }
        Ok(Step { flush: more, exit: None })
    }

    /// The time is `now`: past the expiry the helper ends.
    pub fn tick(&mut self, now: Nanos) -> Result<Step, SinceError> {
        if self.is_dead() {
            return Ok(Step::default());
        }
        if now >= self.last_ask.after(self.config.idle) || now >= self.born.after(self.config.held_most) {
            return self.die(Death::Expired);
        }
        Ok(Step::default())
    }

    /// The next answer to write, in the order decided. Taken before the next call, and its
    /// names ([`Watcher::names_since`]) read before the next call too.
    pub fn take_answer(&mut self) -> Option<(AskId, Answer)> {
        match self.answers.is_empty() {
            true => None,
            false => Some(self.answers.remove_shift(0)),
        }
    }

    /// The names heard since `since` (an [`Answer::Vouched`]'s): unsorted, a name possibly more
    /// than once, a folder covering what is under it.
    pub fn names_since(&self, since: Place) -> impl Iterator<Item = &str> + '_ {
        self.kept.iter().filter(move |kept| kept.tag >= since.seq).map(|kept| name_of(&self.names, kept))
    }

    /// The answer to an ask from `since`, once `place` is issued.
    fn answer_for(&mut self, since: Option<Place>, place: Place) -> Answer {
        let refuse = |why| Answer::CannotVouch { why, place: Some(place) };
        let Some(since) = since else { return refuse(CannotVouch::NoPlace) };
        if since.instance != self.instance && self.plant != SincePlant::OtherInstanceAccepted {
            return refuse(CannotVouch::OtherInstance { asked: since.instance, here: self.instance });
        }
        // The place just issued is newer than any a request can hold.
        if since.seq == 0 || since.seq >= place.seq {
            return refuse(CannotVouch::NotIssued { seq: since.seq, newest: place.seq - 1 });
        }
        if since.seq < self.vouch_from {
            return refuse(CannotVouch::Forgotten { seq: since.seq, from: self.vouch_from });
        }
        self.newest_asked = self.newest_asked.max(since.seq);
        Answer::Vouched { since: Place { instance: self.instance, seq: since.seq }, place }
    }

    fn answer(&mut self, id: AskId, answer: Answer) -> Result<(), SinceError> {
        if self.answers.len() == self.config.most_asks {
            return Err(SinceError::AnswersNotTaken { most: self.config.most_asks });
        }
        self.answers.push((id, answer));
        Ok(())
    }

    fn issue(&mut self) -> Place {
        self.seq += 1;
        Place { instance: self.instance, seq: self.seq }
    }

    /// The helper ends for `death`: every ask waiting is refused for it.
    fn die(&mut self, death: Death) -> Result<Step, SinceError> {
        self.state = State::Dead(death);
        while let Some(ask) = self.waiting.pop() {
            self.answer(ask.id, Answer::CannotVouch { why: CannotVouch::Dead(death), place: None })?;
        }
        self.kept.clear();
        self.names.clear();
        Ok(Step { flush: false, exit: Some(death) })
    }

    /// `name`, heard now, kept with the newest place's tag, room made first.
    fn keep(&mut self, name: &str) {
        // No place it could serve: none issued yet, or all forgotten since the newest.
        if self.seq < self.vouch_from {
            return;
        }
        if name.len() > MOST_NAME {
            // Longer than any path the system makes: its folder stands for it (a `/` is a
            // character's boundary).
            let cut = name.as_bytes()[..MOST_NAME].iter().rposition(|byte| *byte == b'/').unwrap_or(0);
            return self.keep_cut(&name[..cut]);
        }
        self.keep_cut(name);
    }

    fn keep_cut(&mut self, name: &str) {
        if name.is_empty() {
            // The checkout itself stands for it: nothing can be vouched for.
            self.forget(self.seq + 1);
            return;
        }
        if !self.room(name.len()) {
            self.make_room(name.len());
            if self.seq < self.vouch_from {
                return;
            }
        }
        let at = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        let len = u32::try_from(name.len()).unwrap_or(u32::MAX);
        // Within the capacities reserved when made: `room` said so.
        self.names.extend_from_slice(name.as_bytes());
        self.kept.push(Kept { at, len, tag: self.seq });
    }

    fn room(&self, len: usize) -> bool {
        self.kept.len() < self.config.most_paths && self.names.len() + len <= self.config.most_bytes
    }

    /// Room for a name of `len` bytes: each name once; then the places before the newest one a
    /// request came back with forgotten; then all of them.
    fn make_room(&mut self, len: usize) {
        self.compact(self.vouch_from);
        if self.room(len) {
            return;
        }
        let floor = self.newest_asked.max(self.vouch_from);
        if floor > self.vouch_from {
            self.raise(floor);
            self.compact(floor);
            if self.room(len) {
                return;
            }
        }
        self.forget(self.seq + 1);
    }

    /// Every place before `floor` is no longer vouched for.
    fn raise(&mut self, floor: u64) {
        if self.plant != SincePlant::ForgetSilently {
            self.vouch_from = self.vouch_from.max(floor);
        }
    }

    /// Nothing kept serves a place before `floor`: all dropped.
    fn forget(&mut self, floor: u64) {
        self.raise(floor);
        self.kept.clear();
        self.names.clear();
    }

    /// The names tagged `floor` or later, each once with its newest tag, copied down.
    fn compact(&mut self, floor: u64) {
        let names = &self.names;
        self.kept.retain(|kept| kept.tag >= floor);
        self.kept.sort_unstable_by(|a, b| name_of(names, a).cmp(name_of(names, b)).then(b.tag.cmp(&a.tag)));
        // The first of each name, its newest tag, is kept.
        self.kept.dedup_by(|later, first| name_of(names, later) == name_of(names, first));
        self.spare.clear();
        for kept in self.kept.iter_mut() {
            let at = u32::try_from(self.spare.len()).unwrap_or(u32::MAX);
            // At most the bytes `names` held: within the capacity reserved when made.
            self.spare.extend_from_slice(name_of(&self.names, kept).as_bytes());
            kept.at = at;
        }
        core::mem::swap(&mut self.names, &mut self.spare);
    }
}

/// A kept name's text: its bytes were a `&str` when kept.
fn name_of<'n, A: Allocator>(names: &'n Vec<u8, A>, kept: &Kept) -> &'n str {
    let start = usize::try_from(kept.at).unwrap_or(usize::MAX);
    let end = start.saturating_add(usize::try_from(kept.len).unwrap_or(usize::MAX));
    // Kept whole from a `&str` (`keep_cut`), so in range and UTF-8.
    names.get(start..end).and_then(|bytes| core::str::from_utf8(bytes).ok()).unwrap_or("")
}

/// Whether `path` is `name` or under it: what a name told covers.
pub fn covers(name: &str, path: &str) -> bool {
    match path.strip_prefix(name) {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}
