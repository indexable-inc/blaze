//! The change watcher's simulation (rules 43 and 64; since.rs): a model checkout changed by a
//! tape (files written, made and removed, folders made, removed and renamed, nested), two
//! requests that ask what changed since their saved place, read the checkout some steps later
//! and save the place they were given, and a source of changes between them.
//!
//! Two sources behind one seam, each tested against the same oracle (rule 78):
//!
//! - the helper ([`Watcher`] under a simulated watch): the watch tells each change's names in
//!   order, in batches whenever the tape says (one descriptor's events are ordered: nothing is
//!   reordered); a flush completes only once what was queued when it was asked is told; changes
//!   before the first flush may go unheard (the watch's start); the queue overflows when full
//!   or when the tape says; the helper is killed, refused a watch (the watch limit) or expires,
//!   and the next ask starts a new one with a new instance;
//! - an FSEvents-shaped journal: one log that outlives every request, reset or truncated by the
//!   tape (a model of the macOS source's contract, kept here: blaze-watch does not depend on
//!   blaze-core, whose `sim::Journal` plays the real effect).
//!
//! **The oracle**: when an answer vouches for what changed since place `P`, every path whose
//! existence, kind or bytes differ between the checkout as the request read it (after it saved
//! `P`) and the checkout when it asked is named, or a folder above it is. An answer that cannot
//! vouch is correct by construction (every path is statted) and is not checked.
//!
//! The controls plant one bug each and must be caught: no flush before an answer, an ask joined
//! to a flush asked before it, an overflow taken as whole, a place of a dead instance accepted,
//! names dropped without forgetting the places they served.

use core::fmt::Write as _;

use blaze_own::{FixedString, FixedVec, Inline};
use blaze_sim::{Broke, Config, Tape, check, control};

use super::{Lap, harness};
use crate::Nanos;
use crate::since::{self, Answer, AskId, CannotVouch, Fault, Place, SincePlant, Step, Watcher};

/// A path of the model checkout: at most three folders deep, one letter a part.
type Name = FixedString<24>;

/// The most entries of the model checkout.
const MOST_ENTRIES: usize = 40;
/// The most names one answer carries here.
const MOST_TOLD: usize = 32;
/// The most names the simulated watch queues before it overflows.
const QUEUE: usize = 12;
/// The most helpers one case starts: each takes its tables from the case's buffer once.
const MOST_HELPERS: u64 = 12;

type Heap = Inline<65536>;

#[derive(Clone, Copy)]
struct Entry {
    path: Name,
    dir: bool,
    version: u32,
}

/// The model checkout.
#[derive(Clone)]
struct Tree {
    entries: FixedVec<Entry, MOST_ENTRIES>,
    next_version: u32,
}

fn name(text: &str) -> Name {
    let mut out = Name::new();
    let _fits = out.try_push_str(text);
    out
}

fn joined(parent: &str, leaf: &str) -> Option<Name> {
    let mut out = Name::new();
    if !parent.is_empty() {
        out.try_push_str(parent).ok()?;
        out.try_push_str("/").ok()?;
    }
    out.try_push_str(leaf).ok()?;
    Some(out)
}

impl Tree {
    fn new() -> Tree {
        Tree { entries: FixedVec::new(), next_version: 1 }
    }

    fn find(&self, path: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.path.as_str() == path)
    }

    /// A folder to make something in: the root (`""`) or one of depth below 3.
    fn pick_dir(&self, tape: &Tape, avoid: Option<&str>) -> Name {
        let mut dirs: FixedVec<Name, { MOST_ENTRIES + 1 }> = FixedVec::new();
        let _root = dirs.push(Name::new());
        for entry in self.entries.iter().filter(|entry| entry.dir) {
            let shallow = entry.path.as_str().matches('/').count() < 2;
            let moved = avoid.is_some_and(|avoid| since::covers(avoid, entry.path.as_str()));
            if shallow && !moved {
                let _room = dirs.push(entry.path);
            }
        }
        dirs[tape.index(dirs.len(), "folder")]
    }

    fn bump(&mut self) -> u32 {
        self.next_version += 1;
        self.next_version
    }

    /// One change the tape draws; the names a watch tells of it.
    fn change(&mut self, tape: &Tape) -> FixedVec<Name, 2> {
        let mut told = FixedVec::new();
        match tape.below(4, "change") {
            0 => {
                let parent = self.pick_dir(tape, None);
                let leaf = *tape.pick(&["x", "y"], "file").unwrap_or(&"x");
                let Some(path) = joined(parent.as_str(), leaf) else { return told };
                let version = self.bump();
                match self.entries.iter().position(|entry| entry.path == path) {
                    Some(at) if self.entries[at].dir => return told,
                    Some(at) => self.entries[at].version = version,
                    None if self.entries.is_full() => return told,
                    None => {
                        let _room = self.entries.push(Entry { path, dir: false, version });
                    }
                }
                let _room = told.push(path);
            }
            1 => {
                let parent = self.pick_dir(tape, None);
                let leaf = *tape.pick(&["a", "b"], "folder name").unwrap_or(&"a");
                let Some(path) = joined(parent.as_str(), leaf) else { return told };
                if self.find(path.as_str()).is_some() || self.entries.is_full() {
                    return told;
                }
                let version = self.bump();
                let _room = self.entries.push(Entry { path, dir: true, version });
                let _room = told.push(path);
            }
            2 => {
                let Some(gone) = self.entries.get(tape.index(self.entries.len(), "removed")).map(|entry| entry.path) else {
                    return told;
                };
                self.entries.retain(|entry| !since::covers(gone.as_str(), entry.path.as_str()));
                let _room = told.push(gone);
            }
            _ => {
                let Some(from) = self.entries.get(tape.index(self.entries.len(), "renamed")).map(|entry| entry.path) else {
                    return told;
                };
                let parent = self.pick_dir(tape, Some(from.as_str()));
                let leaf = *tape.pick(&["a", "b", "x", "y"], "new name").unwrap_or(&"a");
                let Some(to) = joined(parent.as_str(), leaf) else { return told };
                if self.find(to.as_str()).is_some() || since::covers(from.as_str(), to.as_str()) {
                    return told;
                }
                // Every path under `from` moves under `to`; a path too long is refused whole.
                let mut moved: FixedVec<(usize, Name), MOST_ENTRIES> = FixedVec::new();
                for (at, entry) in self.entries.iter().enumerate() {
                    if since::covers(from.as_str(), entry.path.as_str()) {
                        let rest = &entry.path.as_str()[from.len()..];
                        let mut path = to;
                        if path.try_push_str(rest).is_err() {
                            return told;
                        }
                        let _room = moved.push((at, path));
                    }
                }
                for (at, path) in moved.iter() {
                    self.entries[*at].path = *path;
                }
                let _room = told.push(from);
                let _room = told.push(to);
            }
        }
        told
    }
}

/// An answer as a request receives it: the names copied out.
struct Reply {
    id: AskId,
    answer: Answer,
    names: FixedVec<Name, MOST_TOLD>,
}

type Replies = FixedVec<Reply, 8>;

/// The seam as the simulation drives it: a change happens, the source takes a step of its own,
/// a request asks.
trait Source {
    fn told(&mut self, tape: &Tape, names: &[Name]);
    fn step(&mut self, tape: &Tape, now: Nanos, out: &mut Replies) -> Result<(), Broke>;
    fn ask(&mut self, tape: &Tape, id: AskId, since: Option<Place>, now: Nanos, out: &mut Replies) -> Result<(), Broke>;
}

fn reply(out: &mut Replies, id: AskId, answer: Answer, names: FixedVec<Name, MOST_TOLD>) -> Result<(), Broke> {
    out.push(Reply { id, answer, names }).map_err(|_| harness("more replies than asks"))
}

/// One helper: the core, the watch's queue, the flush out.
struct Helper<'h> {
    core: Watcher<&'h Heap>,
    queue: FixedVec<Name, QUEUE>,
    /// Names the watch was told, and names delivered or dropped: the queue is the difference.
    enqueued: u64,
    delivered: u64,
    /// The queue overflowed: the next delivery says lost.
    overflowed: bool,
    /// The flush out: `enqueued` when it was asked.
    barrier: Option<u64>,
    /// The first flush completed: the watch is in place.
    started: bool,
    /// Asks not answered yet.
    asked: FixedVec<AskId, 4>,
}

impl Helper<'_> {
    /// The first `n` names queued, told as one batch.
    fn deliver(&mut self, n: usize, out: &mut Replies) -> Result<bool, Broke> {
        let mut batch: FixedVec<Name, QUEUE> = FixedVec::new();
        for _ in 0..n.min(self.queue.len()) {
            let _room = batch.push(self.queue.remove_shift(0));
            self.delivered += 1;
        }
        let lost = core::mem::take(&mut self.overflowed);
        let step = self.core.heard(batch.iter().map(|name| name.as_str()), lost).map_err(harness)?;
        self.handle(step, out)
    }

    /// The answers taken, the flush asked: `true` when the helper is to exit.
    fn handle(&mut self, step: Step, out: &mut Replies) -> Result<bool, Broke> {
        while let Some((id, answer)) = self.core.take_answer() {
            let mut names = FixedVec::new();
            if let Answer::Vouched { since, .. } = answer {
                for told in self.core.names_since(since) {
                    names.push(name(told)).map_err(|_| harness("more names than an answer here carries"))?;
                }
            }
            self.asked.retain(|it| *it != id);
            reply(out, id, answer, names)?;
        }
        if step.flush {
            self.barrier = Some(self.enqueued);
        }
        Ok(step.exit.is_some())
    }
}

struct HelperSource<'h> {
    heap: &'h Heap,
    helper: Option<Helper<'h>>,
    started: u64,
    plant: SincePlant,
}

/// Small tables, so compaction and forgetting happen within a case.
fn sim_config() -> since::Config {
    since::Config { most_paths: 6, most_bytes: 40, most_asks: 4, idle: Nanos::secs(60), held_most: Nanos::secs(600) }
}

impl HelperSource<'_> {
    /// The helper gone: what it had not answered is answered by the broken socket.
    fn end(&mut self, out: &mut Replies) -> Result<(), Broke> {
        if let Some(helper) = self.helper.take() {
            for id in helper.asked.iter() {
                reply(out, *id, Answer::CannotVouch { why: CannotVouch::NoHelper, place: None }, FixedVec::new())?;
            }
        }
        Ok(())
    }

    fn start(&mut self, tape: &Tape, now: Nanos) -> Result<(), Broke> {
        if self.started == MOST_HELPERS {
            return Ok(());
        }
        self.started += 1;
        // Random, as the driver draws it; the count in the top bits keeps two apart.
        let instance = (self.started << 48) | (tape.next_u64("instance") & 0xffff_ffff_ffff);
        let core = Watcher::planted_in(sim_config(), instance, now, self.heap, self.plant).map_err(harness)?;
        // Made with its first flush out.
        self.helper = Some(Helper { core, queue: FixedVec::new(), enqueued: 0, delivered: 0, overflowed: false, barrier: Some(0), started: false, asked: FixedVec::new() });
        Ok(())
    }
}

impl Source for HelperSource<'_> {
    fn told(&mut self, tape: &Tape, names: &[Name]) {
        let Some(helper) = self.helper.as_mut() else { return };
        for told in names {
            // Before the first flush completes the watch may not be in place: a change then
            // may go unheard (Effect::Watch's contract), and no place is issued before it.
            if !helper.started && tape.coin("unheard before the start") {
                continue;
            }
            helper.enqueued += 1;
            if helper.queue.push(*told).is_err() {
                helper.delivered += 1;
                helper.overflowed = true;
            }
        }
    }

    fn step(&mut self, tape: &Tape, now: Nanos, out: &mut Replies) -> Result<(), Broke> {
        let Some(helper) = self.helper.as_mut() else { return Ok(()) };
        let exit = match tape.weighted(&[6, 5, 1, 1, 1, 2], "helper step") {
            0 => {
                let n = 1 + tape.index(helper.queue.len().max(1), "delivered");
                helper.deliver(n, out)?
            }
            1 => {
                let Some(barrier) = helper.barrier else { return Ok(()) };
                // What was queued when the flush was asked is told before it completes.
                let owed = usize::try_from(barrier.saturating_sub(helper.delivered)).unwrap_or(usize::MAX);
                if (owed > 0 || helper.overflowed) && helper.deliver(owed, out)? {
                    return self.end(out);
                }
                let Some(helper) = self.helper.as_mut() else { return Ok(()) };
                helper.barrier = None;
                helper.started = true;
                let step = helper.core.flushed().map_err(harness)?;
                helper.handle(step, out)?
            }
            2 => {
                helper.delivered = helper.enqueued;
                helper.queue.clear();
                helper.overflowed = true;
                helper.deliver(0, out)?
            }
            3 => true,
            4 => {
                let step = helper.core.failed(Fault::WatchRefused { watched: 7, limit: Some(8192) }).map_err(harness)?;
                helper.handle(step, out)?
            }
            _ => {
                let step = helper.core.tick(now).map_err(harness)?;
                helper.handle(step, out)?
            }
        };
        if exit {
            self.end(out)?;
        }
        Ok(())
    }

    fn ask(&mut self, tape: &Tape, id: AskId, since: Option<Place>, now: Nanos, out: &mut Replies) -> Result<(), Broke> {
        let Some(helper) = self.helper.as_mut() else {
            // Nothing answers on the socket: the request stats all and starts a helper.
            reply(out, id, Answer::CannotVouch { why: CannotVouch::NoHelper, place: None }, FixedVec::new())?;
            return self.start(tape, now);
        };
        helper.asked.push(id).map_err(|_| harness("more asks in flight than requests"))?;
        let step = helper.core.asked(id, since, now).map_err(harness)?;
        if helper.handle(step, out)? {
            self.end(out)?;
        }
        Ok(())
    }
}

/// The macOS source's contract: a journal that outlives every request, reset or truncated.
struct JournalSource {
    id: u64,
    resets: u64,
    /// The oldest change kept, and the next change's number.
    first: u64,
    next: u64,
    log: FixedVec<(u64, Name), MOST_TOLD>,
}

impl Source for JournalSource {
    fn told(&mut self, _tape: &Tape, names: &[Name]) {
        for told in names {
            if self.log.is_full() {
                // Truncated: the oldest change is no longer reached.
                self.log.remove_shift(0);
                self.first = self.log.first().map_or(self.next, |(seq, _)| *seq);
            }
            let _room = self.log.push((self.next, *told));
            self.next += 1;
        }
    }

    fn step(&mut self, tape: &Tape, _now: Nanos, _out: &mut Replies) -> Result<(), Broke> {
        if tape.one_in(12, "journal reset") {
            self.resets += 1;
            self.id = (self.resets << 48) | (tape.next_u64("journal id") & 0xffff_ffff_ffff);
            self.log.clear();
            self.first = 1;
            self.next = 1;
        }
        Ok(())
    }

    fn ask(&mut self, _tape: &Tape, id: AskId, since: Option<Place>, _now: Nanos, out: &mut Replies) -> Result<(), Broke> {
        let place = Place { instance: self.id, seq: self.next };
        let refuse = |why| Answer::CannotVouch { why, place: Some(place) };
        let answer = match since {
            None => refuse(CannotVouch::NoPlace),
            Some(since) if since.instance != self.id => refuse(CannotVouch::OtherInstance { asked: since.instance, here: self.id }),
            Some(since) if since.seq > self.next => refuse(CannotVouch::NotIssued { seq: since.seq, newest: self.next }),
            Some(since) if since.seq < self.first => refuse(CannotVouch::Forgotten { seq: since.seq, from: self.first }),
            Some(since) => Answer::Vouched { since, place },
        };
        let mut names = FixedVec::new();
        if let Answer::Vouched { since, .. } = answer {
            for (_, told) in self.log.iter().filter(|(seq, _)| *seq >= since.seq) {
                let _room = names.push(*told);
            }
        }
        reply(out, id, answer, names)
    }
}

/// A request between its asks.
#[derive(Clone)]
enum Client {
    /// It read `read` after saving `saved`.
    Idle { saved: Option<Place>, read: Tree },
    /// It asked as `id`, the checkout then `at_ask`.
    Waiting { id: AskId, read: Tree, at_ask: Tree },
    /// Answered; it reads at a later step and saves `place`.
    ToRead { place: Option<Place> },
}

/// The oracle (the module's doc).
fn vouched(read: &Tree, at_ask: &Tree, names: &[Name], from: Place) -> Result<(), Broke> {
    let named = |path: &str| names.iter().any(|told| since::covers(told.as_str(), path));
    let moved = |entry: &Entry, now: Option<&Entry>| now.is_none_or(|now| now.dir != entry.dir || now.version != entry.version);
    let before = read.entries.iter().filter(|entry| moved(entry, at_ask.find(entry.path.as_str())));
    let made = at_ask.entries.iter().filter(|entry| read.find(entry.path.as_str()).is_none());
    for entry in before.chain(made) {
        if !named(entry.path.as_str()) {
            let mut why: FixedString<160> = FixedString::new();
            let _fits = write!(why, "{} changed after place {from} but was not named ({} names told)", entry.path, names.len());
            // blaze-sim's finding is a `String`: made only when the oracle fails.
            return Err(Broke::Oracle(why.as_str().into()));
        }
    }
    Ok(())
}

fn run_with(tape: &Tape, source: &mut dyn Source) -> Result<(), Broke> {
    let mut world = Tree::new();
    let mut clients = [Client::Idle { saved: None, read: world.clone() }, Client::Idle { saved: None, read: world.clone() }];
    let mut now = Nanos(0);
    let mut next_id = 0;
    let mut replies = Replies::new();
    let steps = tape.range(1..240, "steps");
    for _ in 0..steps {
        now = now.after(Nanos::secs(tape.below(8, "seconds")));
        match tape.weighted(&[5, 5, 2, 2], "step") {
            0 => {
                let told = world.change(tape);
                source.told(tape, &told);
            }
            1 => source.step(tape, now, &mut replies)?,
            2 => {
                let at = tape.index(clients.len(), "asker");
                let asking = match &clients[at] {
                    Client::Idle { saved, read } => Some((*saved, read.clone())),
                    Client::Waiting { .. } | Client::ToRead { .. } => None,
                };
                if let Some((since, read)) = asking {
                    next_id += 1;
                    let id = AskId(next_id);
                    clients[at] = Client::Waiting { id, read, at_ask: world.clone() };
                    source.ask(tape, id, since, now, &mut replies)?;
                }
            }
            _ => {
                let at = tape.index(clients.len(), "reader");
                if let Client::ToRead { place } = clients[at] {
                    clients[at] = Client::Idle { saved: place, read: world.clone() };
                }
            }
        }
        while let Some(reply) = replies.pop() {
            let Some(at) = clients.iter().position(|client| matches!(client, Client::Waiting { id, .. } if *id == reply.id)) else {
                return Err(harness("an answer to an ask no request made"));
            };
            let Client::Waiting { read, at_ask, .. } = &clients[at] else { return Err(harness("not waiting")) };
            let place = match reply.answer {
                Answer::Vouched { since, place } => {
                    vouched(read, at_ask, &reply.names, since)?;
                    Some(place)
                }
                Answer::CannotVouch { place, .. } => place,
            };
            clients[at] = Client::ToRead { place };
        }
    }
    Ok(())
}

fn run_helper(tape: &Tape, plant: SincePlant) -> Result<(), Broke> {
    let heap = Heap::new();
    let mut source = HelperSource { heap: &heap, helper: None, started: 0, plant };
    run_with(tape, &mut source)
}

fn run_journal(tape: &Tape) -> Result<(), Broke> {
    let mut source = JournalSource { id: 1 << 48, resets: 1, first: 1, next: 1, log: FixedVec::new() };
    run_with(tape, &mut source)
}

/// The invariant: the change watcher names every change since a valid saved place after a flush barrier or refuses to vouch on overflow, dead instance, or forgotten place.
#[test]
fn the_helper_names_every_change_since_a_place_or_cannot_vouch() {
    let lap = Lap::new("since: the helper names every change");
    check(Config::cases(3000), |tape| lap.case(run_helper(tape, SincePlant::None)));
}

#[test]
fn the_journal_source_meets_the_same_oracle() {
    let lap = Lap::new("since: the journal source");
    check(Config::cases(1000), |tape| lap.case(run_journal(tape)));
}

#[test]
fn the_oracle_catches_an_answer_with_no_flush() {
    let lap = Lap::new("since: control, no flush");
    control(Config::cases(4000), SincePlant::None, SincePlant::NoFlush, "not named", |tape, plant| lap.case(run_helper(tape, plant)));
}

#[test]
fn the_oracle_catches_an_ask_joined_to_an_earlier_flush() {
    let lap = Lap::new("since: control, an ask joined to an earlier flush");
    control(Config::cases(6000), SincePlant::None, SincePlant::JoinOutstandingFlush, "not named", |tape, plant| lap.case(run_helper(tape, plant)));
}

#[test]
fn the_oracle_catches_an_overflow_taken_as_whole() {
    let lap = Lap::new("since: control, an overflow taken as whole");
    control(Config::cases(4000), SincePlant::None, SincePlant::OverflowIgnored, "not named", |tape, plant| lap.case(run_helper(tape, plant)));
}

#[test]
fn the_oracle_catches_a_place_of_a_dead_instance_accepted() {
    let lap = Lap::new("since: control, a dead instance's place accepted");
    control(Config::cases(6000), SincePlant::None, SincePlant::OtherInstanceAccepted, "not named", |tape, plant| lap.case(run_helper(tape, plant)));
}

#[test]
fn the_oracle_catches_names_dropped_without_forgetting_their_places() {
    let lap = Lap::new("since: control, names dropped silently");
    control(Config::cases(6000), SincePlant::None, SincePlant::ForgetSilently, "not named", |tape, plant| lap.case(run_helper(tape, plant)));
}

/// The walk through one quiet interval, by hand: the first flush issues place 1; an ask with
/// no place is refused with place 2; a name heard after it is named since place 2; an ask from
/// another instance is refused; a place survives its bytes.
#[test]
fn a_quiet_interval_is_vouched_for_by_name() {
    let heap = Heap::new();
    let config = sim_config();
    let mut core = Watcher::new_in(config, 7, Nanos(0), &heap).expect("tables in the buffer");
    assert_eq!(core.flushed(), Ok(Step::default()));
    assert_eq!(core.asked(AskId(1), None, Nanos(1)), Ok(Step { flush: true, exit: None }));
    assert_eq!(core.flushed(), Ok(Step::default()));
    let first = Place { instance: 7, seq: 2 };
    assert_eq!(core.take_answer(), Some((AskId(1), Answer::CannotVouch { why: CannotVouch::NoPlace, place: Some(first) })));
    assert_eq!(core.heard(["src/lib.rs"], false), Ok(Step::default()));
    assert_eq!(core.asked(AskId(2), Some(first), Nanos(2)), Ok(Step { flush: true, exit: None }));
    assert_eq!(core.flushed(), Ok(Step::default()));
    let second = Place { instance: 7, seq: 3 };
    assert_eq!(core.take_answer(), Some((AskId(2), Answer::Vouched { since: first, place: second })));
    {
        // In its own block: the iterator borrows the core, which the asks below change.
        let mut names = core.names_since(first);
        assert_eq!(names.next(), Some("src/lib.rs"));
        assert_eq!(names.next(), None);
    }
    assert_eq!(core.names_since(second).count(), 0);
    let stranger = Place { instance: 8, seq: 2 };
    assert_eq!(core.asked(AskId(3), Some(stranger), Nanos(3)), Ok(Step { flush: true, exit: None }));
    assert_eq!(core.flushed(), Ok(Step::default()));
    let refused = core.take_answer();
    assert!(matches!(refused, Some((AskId(3), Answer::CannotVouch { why: CannotVouch::OtherInstance { asked: 8, here: 7 }, .. }))));
    assert_eq!(Place::decode(&second.encode()), Some(second));
    assert_eq!(Place::decode(&[0; Place::BYTES]), None);
}

/// An overflow ends the helper, refusing what waits, and the line printed names it.
#[test]
fn an_overflow_refuses_what_waits_and_says_why() {
    let heap = Heap::new();
    let mut core = Watcher::new_in(sim_config(), 7, Nanos(0), &heap).expect("tables in the buffer");
    assert_eq!(core.flushed(), Ok(Step::default()));
    assert_eq!(core.asked(AskId(1), None, Nanos(1)), Ok(Step { flush: true, exit: None }));
    let step = core.heard([], true);
    assert_eq!(step, Ok(Step { flush: false, exit: Some(since::Death::Fault(Fault::Overflowed)) }));
    let Some((AskId(1), Answer::CannotVouch { why, place: None })) = core.take_answer() else { panic!("the waiting ask is refused") };
    let mut line: FixedString<256> = FixedString::new();
    let _fits = write!(line, "{why}");
    assert!(line.as_str().contains("inotify's queue overflowed"), "{}", line.as_str());
    assert!(line.as_str().ends_with("every path is statted this request"), "{}", line.as_str());
}
