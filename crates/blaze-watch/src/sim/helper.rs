//! The journal helper's simulation (rules 43, 64 and 78; helper.rs): commands, the helper,
//! file changes, the helper's deaths and restarts, a stale socket, a hung helper and a binary
//! replaced, interleaved by one tape.
//!
//! **The model.** A checkout of six files, changed by the tape; a journal that outlives every
//! process (each change an id, as FSEvents' are); at most one helper, its watch's queue told in
//! order and in batches when the tape says, its flush completing only once what was queued
//! when it was asked is told, its first flush the moment its watch is in place (a change before
//! it may go unheard), its queue overflowing when full; the socket: none, live, or stale (a
//! helper killed leaves its file). Two commands, each: ask (connect, hello, ask, answer, under
//! [`helper::ASK_DEADLINE`]), take the answer or replay the journal from the journal place it
//! saved (the seam's two instantiations), read the checkout a step or more later, save both
//! places. The tape kills the helper, hangs it (it stops answering, the socket still accepts),
//! replaces the binary (the next commands are of the new build; the helper's driver sees its
//! file changed at its next tick), and advances the clock.
//!
//! **The oracle.**
//! 1. Every command's changed set, from the helper or from its own replay, names every file
//!    whose bytes differ between the checkout as the command last read it and as it was when
//!    it asked (a fresh replay from its place names exactly those, or more).
//! 2. No command is still waiting on an ask past its deadline.
//! 3. Two commands get consistent places: of one helper, a command whose ask began after
//!    another's answer was delivered is given a newer place.
//! 4. A helper of an old build does not outlive its replacement: no three asks in a row of the
//!    new build meet it.
//! 5. The differential (blaze-cli journal_ask.rs `Both`): a vouched answer names every file the
//!    command's own replay would, from the journal place saved beside the helper's place to the
//!    ask. Its precondition is the pair being one moment: a place's journal place is where the
//!    watch had read to when the place was issued (the last change heard), on a refusal too.
//!
//! **The controls**, each a planted bug the oracle must catch: the helper answers before its
//! flush's cookie (no barrier); its barrier is the current event id instead of a cookie
//! ([`Barrier::EventId`]: a change still in the kernel's queue has no id yet); it survives a
//! binary change; its cap drops names without forgetting the places they served; it vouches
//! past events its watch dropped; and the command's ask has no deadline.
//!
//! **The pipeline** a change goes through ([`Barrier`]): noted by the kernel, given its id by
//! fseventsd in order (a tape step reads on), delivered to the stream. Only a change with its
//! id is delivered; a cookie heard means every change noted before it has its id.

use std::vec::Vec as StdVec;

use blaze_own::Inline;
use blaze_sim::{Broke, Config, Tape, check, control};

use super::{Lap, harness, oracle};
use crate::Nanos;
use crate::helper::{self, AskAnswer, AskNext, AskPlant, Asking, CheckoutKey, Hello, Helper, HelperPlant, JournalAt, NotAsked, Version};
use crate::since::{self, Answer, AskId, CannotVouch, Place, SincePlant, Step};

type Heap = Inline<65536>;

const FILES: usize = 6;
const NAMES: [&str; FILES] = ["a", "b", "c/d", "c/e", "f", "g"];
/// The most names the simulated watch queues before it overflows.
const QUEUE: usize = 12;
/// The most helpers one case starts: each takes its tables from the case's buffer once.
const MOST_HELPERS: u64 = 12;
const CHECKOUT: CheckoutKey = CheckoutKey([7; 32]);

fn version(build: u64) -> Version {
    Version::of(&build.to_le_bytes()).expect("eight bytes are a version")
}

/// The simulated journal's length as the wire carries a journal's place (the driver's cursor,
/// opaque to the helper).
fn at_of(log: u64) -> JournalAt {
    JournalAt::of(&log.to_le_bytes()).expect("eight bytes are a journal's place")
}

/// [`at_of`] read back, as the command's driver reads a journal's place (blaze-cli
/// journal_ask.rs, `Cursor::decode`): `None` for bytes that are not one ([`JournalAt::NONE`], a
/// place issued before any flush completed). The command then pairs nothing with it: a vouched
/// answer is not taken and the journal is replayed (`Asked`), a refusal's place is not saved
/// (`pair_refused`). The shipped helper never sends one (a place is issued only at a flush,
/// where the journal place is set); a planted one does ([`NO_BARRIER`] answers before its first
/// flush), and the command's handling of it is the model's, never a fault of the harness.
fn log_of(at: JournalAt) -> Option<u64> {
    let bytes: [u8; 8] = at.as_bytes().try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

/// Small tables, so compaction and forgetting happen within a case; a short idle.
fn sim_config() -> since::Config {
    since::Config { most_paths: 6, most_bytes: 40, most_asks: 4, idle: Nanos::millis(900), held_most: Nanos::secs(60) }
}

/// How the helper's driver makes the barrier a flush is (the seam, rule 78): what must be told
/// before the flush completes.
///
/// **The pipeline it is judged on.** A change is first noted by the kernel (`/dev/fsevents`,
/// no id), then given its event id by fseventsd, which reads the kernel's queue in order, then
/// delivered to the stream. `FSEventsGetCurrentEventId()` is fseventsd's newest id ASSIGNED:
/// a change the kernel noted and fseventsd has not read yet has no id, and is below no id it
/// returns. [`Sim::assigned_to`] is that counter: the journal's length through which ids exist.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Barrier {
    /// A cookie written at the flush (bstd `HeldChanges::flush`, blaze-core fsevents.rs
    /// `Watch::flush`): noted by the kernel after every change made before it, so given its id
    /// after theirs, and delivered after them. Complete when every change made before the flush
    /// is told. Sound by the pipeline's order alone.
    Cookie,
    /// The stream's last delivered id against `FSEventsGetCurrentEventId()` read at the flush:
    /// complete as soon as every change with an id at or below that is told (at once when none
    /// is pending). UNSOUND on this pipeline: a change made before the ask, still in the
    /// kernel's queue, has no id yet and is not waited for; the answer is given before the
    /// batch that holds it ("answered before the pending batch"). It is this seam's control,
    /// never a shipped driver.
    EventId,
}

/// The bugs one case plants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Plants {
    core: SincePlant,
    helper: HelperPlant,
    ask: AskPlant,
    barrier: Barrier,
}

const NONE: Plants = Plants { core: SincePlant::None, helper: HelperPlant::None, ask: AskPlant::None, barrier: Barrier::Cookie };
const EVENT_ID: Plants = Plants { barrier: Barrier::EventId, ..NONE };
const NO_BARRIER: Plants = Plants { core: SincePlant::NoFlush, ..NONE };
const SURVIVES: Plants = Plants { helper: HelperPlant::SurvivesReplace, ..NONE };
const FORGETS: Plants = Plants { core: SincePlant::ForgetSilently, ..NONE };
const NO_DEADLINE: Plants = Plants { ask: AskPlant::NoDeadline, ..NONE };
const DROPS_IGNORED: Plants = Plants { core: SincePlant::OverflowIgnored, ..NONE };

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Socket {
    None,
    Live,
    /// A file with nothing listening: a helper killed.
    Stale,
}

/// Where a command's stored answer resumes from: the helper's place and the journal's.
#[derive(Clone, Copy, Default, Debug)]
struct Saved {
    helper: Option<Place>,
    /// The journal's length when the place was taken: every change with an id at or below it
    /// was told before the command read.
    journal: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
enum Cmd {
    Idle { saved: Saved, read: [u32; FILES] },
    Asking { asking: Asking, id: AskId, saved: Saved, read: [u32; FILES], at_ask: [u32; FILES], ask_log: u64, began: u64 },
    ToRead { saved: Saved },
}

/// The live helper: its session, its watch's queue, the flush out.
struct Live<'h> {
    session: Helper<&'h Heap>,
    build: u64,
    /// Each change queued: the file, and the journal's length once it was made.
    queue: StdVec<(usize, u64)>,
    enqueued: u64,
    delivered: u64,
    overflowed: bool,
    /// The journal's length through the last change heard (or passed before the watch was in
    /// place): the journal place of a place issued now, as the driver's cursor is the last
    /// event of the last batch read (bstd fs/journal.rs `HeldChanges`).
    heard_to: u64,
    /// The flush out: the journal's length through which every queued change is told before
    /// it completes ([`Barrier`]: the length when it was asked, or the newest id assigned then).
    barrier: Option<u64>,
    started: bool,
    /// Stopped: it accepts connections but answers nothing.
    hung: bool,
    /// Asks in flight: the ask and its command.
    asked: StdVec<(AskId, usize)>,
}

struct Sim<'h> {
    heap: &'h Heap,
    plants: Plants,
    files: [u32; FILES],
    next_version: u32,
    /// Every change ever: the file changed; its id is its index plus one.
    log: StdVec<usize>,
    /// fseventsd's counter ([`Barrier`]): the journal's length through which changes have
    /// their ids; the rest are noted by the kernel and not yet read. Only a change with its id
    /// is delivered.
    assigned_to: u64,
    build: u64,
    socket: Socket,
    live: Option<Live<'h>>,
    started: u64,
    cmds: [Cmd; 2],
    now: Nanos,
    step: u64,
    next_ask: u64,
    /// The newest place delivered, and the step it was delivered at.
    newest: Option<(Place, u64)>,
    /// Asks in a row of the current build that met a helper of another.
    old_build_asks: u32,
}

/// Oracle 1: every file that moved between `read` and `at_ask` is named by `told`.
fn names_every_move(read: &[u32; FILES], at_ask: &[u32; FILES], told: &[&str], by: &str) -> Result<(), Broke> {
    for file in 0..FILES {
        if read[file] != at_ask[file] && !told.iter().any(|name| since::covers(name, NAMES[file])) {
            return Err(Broke::Oracle(format!("{} changed after the command's place but was not named by {by} ({} names told)", NAMES[file], told.len())));
        }
    }
    Ok(())
}

impl<'h> Sim<'h> {
    fn change(&mut self, tape: &Tape) {
        let file = tape.index(FILES, "file changed");
        self.next_version += 1;
        self.files[file] = self.next_version;
        self.log.push(file);
        let Some(live) = self.live.as_mut() else { return };
        // The watch is in place from one moment: what came before it is never heard, what came
        // after always is (a prefix of the journal after the start is passed, never a change
        // after one heard).
        if !live.started && live.enqueued == 0 && tape.coin("unheard before the watch is in place") {
            live.heard_to = self.log.len() as u64;
            return;
        }
        live.enqueued += 1;
        if live.queue.len() == QUEUE {
            live.delivered += 1;
            live.overflowed = true;
        } else {
            live.queue.push((file, self.log.len() as u64));
        }
    }

    fn start_helper(&mut self, tape: &Tape) -> Result<(), Broke> {
        // One listener per checkout: a helper that finds a live one exits at once.
        if self.socket == Socket::Live || self.started == MOST_HELPERS {
            return Ok(());
        }
        self.started += 1;
        let instance = (self.started << 48) | (tape.next_u64("instance") & 0xffff_ffff_ffff);
        let hello = Hello { version: version(self.build), instance, checkout: CHECKOUT };
        let session = Helper::planted_in(sim_config(), hello, self.now, self.heap, self.plants.core, self.plants.helper).map_err(harness)?;
        self.live = Some(Live {
            session,
            build: self.build,
            queue: StdVec::new(),
            enqueued: 0,
            delivered: 0,
            overflowed: false,
            heard_to: self.log.len() as u64,
            barrier: Some(0),
            started: false,
            hung: false,
            asked: StdVec::new(),
        });
        self.socket = Socket::Live;
        Ok(())
    }

    /// The helper gone, its socket left `socket`: each ask in flight sees its connection break.
    fn end_helper(&mut self, tape: &Tape, socket: Socket) -> Result<(), Broke> {
        let Some(live) = self.live.take() else { return Ok(()) };
        self.socket = socket;
        for (id, at) in live.asked {
            if let Cmd::Asking { mut asking, id: waiting, .. } = self.cmds[at]
                && waiting == id
                && !asking.is_done()
            {
                let next = asking.broke();
                self.set_asking(at, asking);
                self.finish(tape, at, next, &[])?;
            }
        }
        Ok(())
    }

    fn set_asking(&mut self, at: usize, now: Asking) {
        if let Cmd::Asking { asking, .. } = &mut self.cmds[at] {
            *asking = now;
        }
    }

    /// The first `n` names queued that have their ids ([`Sim::assigned_to`]), told as one batch.
    fn deliver(&mut self, tape: &Tape, n: usize) -> Result<(), Broke> {
        let assigned_to = self.assigned_to;
        let Some(live) = self.live.as_mut() else { return Ok(()) };
        let ready = live.queue.iter().take_while(|(_, made)| *made <= assigned_to).count();
        let batch: StdVec<(usize, u64)> = live.queue.drain(..n.min(ready)).collect();
        live.delivered += batch.len() as u64;
        if let Some((_, made)) = batch.last() {
            live.heard_to = *made;
        }
        let lost = core::mem::take(&mut live.overflowed);
        let step = live.session.heard(batch.iter().map(|(file, _)| NAMES[*file]), lost).map_err(harness)?;
        self.handle(tape, step)
    }

    /// The answers taken and delivered, the flush asked, the exit carried out.
    fn handle(&mut self, tape: &Tape, step: Step) -> Result<(), Broke> {
        // The flush's barrier by the seam's choice ([`Barrier`]): a cookie written now is
        // after every change made so far; the current id read now is after only those with ids.
        let barrier_at = match self.plants.barrier {
            Barrier::Cookie => self.log.len() as u64,
            Barrier::EventId => self.assigned_to,
        };
        let Some(live) = self.live.as_mut() else { return Ok(()) };
        let mut answers = StdVec::new();
        while let Some((id, answer)) = live.session.take_answer() {
            let names: StdVec<&'static str> = match answer {
                Answer::Vouched { since, .. } => {
                    let told: StdVec<&str> = live.session.names_since(since).collect();
                    NAMES.iter().copied().filter(|name| told.iter().any(|it| it == name || since::covers(it, name))).collect()
                }
                Answer::CannotVouch { .. } => StdVec::new(),
            };
            let at = live.asked.iter().position(|(it, _)| *it == id).map(|at| live.asked.remove(at).1);
            answers.push((at, answer, live.session.journal(), names));
        }
        if step.flush {
            live.barrier = Some(barrier_at);
        }
        for (at, answer, journal, names) in answers {
            // An ask given up (its deadline) is not waited for: its answer goes nowhere.
            let Some(at) = at else { continue };
            let Cmd::Asking { mut asking, .. } = self.cmds[at] else { continue };
            if asking.is_done() {
                continue;
            }
            let taken = match answer {
                Answer::Vouched { since, place } => AskAnswer::Vouched { since, place, journal },
                Answer::CannotVouch { why, place } => AskAnswer::CannotVouch { why, place, journal },
            };
            let next = asking.answered(taken);
            self.set_asking(at, asking);
            self.finish(tape, at, next, &names)?;
        }
        if step.exit.is_some() {
            // An exiting helper removes its socket.
            self.end_helper(tape, Socket::None)?;
        }
        Ok(())
    }

    /// A command's ask ended with `next` (`told`: a vouched answer's names).
    fn finish(&mut self, tape: &Tape, at: usize, next: AskNext, told: &[&str]) -> Result<(), Broke> {
        let Cmd::Asking { saved, read, at_ask, began, ask_log, .. } = self.cmds[at] else {
            return Err(harness("an ask ended for a command not asking"));
        };
        match next {
            AskNext::Answered(AskAnswer::Vouched { journal, .. }) if log_of(journal).is_none() => {
                // A journal place that is not one: the answer cannot be paired, so the command
                // takes nothing of it and replays (journal_ask.rs `Asked`, its `None` arm).
                self.old_build_asks = 0;
                self.replay(at)?;
            }
            AskNext::Answered(AskAnswer::Vouched { place, journal, .. }) => {
                self.old_build_asks = 0;
                names_every_move(&read, &at_ask, told, "the helper")?;
                // Oracle 5 (the differential's): every file the command's own replay would name
                // from the journal place saved beside the helper's, to the ask, the helper names
                // too: its names are Replay's or more, never fewer.
                if let (Some(_), Some(from)) = (saved.helper, saved.journal) {
                    let start = usize::try_from(from).map_err(harness)?;
                    let end = usize::try_from(ask_log).map_err(harness)?;
                    for file in &self.log[start.min(end)..end] {
                        let name = NAMES[*file];
                        oracle(told.iter().any(|it| since::covers(it, name)), || {
                            format!("{name} was named by the replay from the journal place saved beside the helper's, but not named by the helper ({} names told)", told.len())
                        })?;
                    }
                }
                let journal = log_of(journal).ok_or_else(|| harness("a journal's place read once, then not"))?;
                if let Some((newest, delivered)) = self.newest
                    && newest.instance == place.instance
                    && began > delivered
                {
                    oracle(place.seq > newest.seq, || format!("inconsistent places: an ask begun after place {newest} was delivered was given {place}"))?;
                }
                if self.newest.is_none_or(|(newest, _)| newest.instance != place.instance || place.seq > newest.seq) {
                    self.newest = Some((place, self.step));
                }
                self.cmds[at] = Cmd::ToRead { saved: Saved { helper: Some(place), journal: Some(journal) } };
            }
            AskNext::Answered(AskAnswer::CannotVouch { why, place, journal }) => {
                self.old_build_asks = 0;
                self.replay(at)?;
                // The place issued is saved with ITS journal place, never the replay's end, so
                // the pair is one moment (oracle 5 holds it to that).
                // A journal place that is not one pairs nothing (journal_ask.rs `pair_refused`):
                // the replay's end stays saved, with no helper's place.
                if let (Some(place), Some(journal), Cmd::ToRead { saved }) = (place, log_of(journal), &mut self.cmds[at]) {
                    *saved = Saved { helper: Some(place), journal: Some(journal) };
                }
                if matches!(why, CannotVouch::Dead(_) | CannotVouch::NoHelper) {
                    self.start_helper(tape)?;
                }
            }
            AskNext::Replay { why, start } => {
                match why {
                    NotAsked::OtherVersion => {
                        self.old_build_asks += 1;
                        let met = self.old_build_asks;
                        oracle(met < 3, || format!("a helper of an old build survived its replacement: {met} asks in a row of the new build met it"))?;
                    }
                    NotAsked::Deadline { .. } | NotAsked::Broken { .. } => {}
                    NotAsked::NoSocket | NotAsked::Stale | NotAsked::OtherCheckout => self.old_build_asks = 0,
                }
                self.replay(at)?;
                if start {
                    // A stale socket is removed by the helper that starts.
                    if self.socket == Socket::Stale {
                        self.socket = Socket::None;
                    }
                    self.start_helper(tape)?;
                }
            }
            AskNext::Connect | AskNext::Read | AskNext::Send(_) => return Err(harness("an ask ended on a step that is not an end")),
        }
        let _ = (saved, began);
        Ok(())
    }

    /// The seam's other instantiation: the command replays the journal from the journal place
    /// it saved (a full sweep when it saved none), held to the same oracle.
    fn replay(&mut self, at: usize) -> Result<(), Broke> {
        let Cmd::Asking { saved, read, at_ask, ask_log, .. } = self.cmds[at] else {
            return Err(harness("a replay for a command not asking"));
        };
        if let Some(from) = saved.journal {
            let start = usize::try_from(from).map_err(harness)?;
            let end = usize::try_from(ask_log).map_err(harness)?;
            let told: StdVec<&str> = self.log[start.min(end)..end].iter().map(|file| NAMES[*file]).collect();
            names_every_move(&read, &at_ask, &told, "the command's own replay")?;
        }
        self.cmds[at] = Cmd::ToRead { saved: Saved { helper: None, journal: Some(ask_log) } };
        Ok(())
    }

    fn ask(&mut self, tape: &Tape, at: usize) -> Result<(), Broke> {
        let Cmd::Idle { saved, read } = self.cmds[at] else { return Ok(()) };
        self.next_ask += 1;
        let id = AskId(self.next_ask);
        let (mut asking, next) = Asking::new(version(self.build), CHECKOUT, saved.helper, self.now);
        if next != AskNext::Connect {
            return Err(harness("an ask begins with anything but a connect"));
        }
        let ask_log = self.log.len() as u64;
        let connected = match self.socket {
            Socket::None => Err(NotAsked::NoSocket),
            Socket::Stale => Err(NotAsked::Stale),
            Socket::Live => Ok(()),
        };
        let next = asking.connected(connected);
        self.cmds[at] = Cmd::Asking { asking, id, saved, read, at_ask: self.files, ask_log, began: self.step };
        if next != AskNext::Read {
            return self.finish(tape, at, next, &[]);
        }
        let Some(live) = self.live.as_mut() else { return Err(harness("a live socket with no helper")) };
        if live.hung {
            // Connected (the kernel's backlog), but nothing reads: no hello comes.
            return Ok(());
        }
        let hello = live.session.hello();
        let AskNext::Send(sent) = asking.hello(&hello) else { return Err(harness("one checkout, yet the hello was refused")) };
        if let Cmd::Asking { asking: held, .. } = &mut self.cmds[at] {
            *held = asking;
        }
        live.asked.push((id, at));
        let step = live.session.asked(id, &sent, self.now).map_err(harness)?;
        self.handle(tape, step)
    }

    fn helper_step(&mut self, tape: &Tape) -> Result<(), Broke> {
        let Some(live) = self.live.as_mut() else { return Ok(()) };
        if live.hung {
            return match tape.one_in(3, "a hung helper killed") {
                true => self.end_helper(tape, Socket::Stale),
                false => Ok(()),
            };
        }
        match tape.weighted(&[6, 5, 1, 1, 1, 4, 2], "helper step") {
            0 => {
                let n = 1 + tape.index(live.queue.len().max(1), "delivered");
                self.deliver(tape, n)
            }
            1 => {
                let Some(barrier) = live.barrier else { return Ok(()) };
                // A cookie is heard only once fseventsd has read the kernel's queue past it:
                // every change noted before it has its id by then.
                if self.plants.barrier == Barrier::Cookie {
                    self.assigned_to = self.assigned_to.max(barrier);
                }
                let Some(live) = self.live.as_mut() else { return Ok(()) };
                // What the barrier covers is told before the flush completes.
                let owed = live.queue.iter().take_while(|(_, made)| *made <= barrier).count();
                if owed > 0 || live.overflowed {
                    self.deliver(tape, owed)?;
                }
                let Some(live) = self.live.as_mut() else { return Ok(()) };
                live.barrier = None;
                live.started = true;
                let journal = live.heard_to;
                let step = live.session.flushed(at_of(journal)).map_err(harness)?;
                self.handle(tape, step)
            }
            2 => {
                live.delivered = live.enqueued;
                live.queue.clear();
                live.overflowed = true;
                self.assigned_to = self.log.len() as u64;
                self.deliver(tape, 0)
            }
            3 => self.end_helper(tape, Socket::Stale),
            4 => {
                live.hung = true;
                Ok(())
            }
            5 => {
                // fseventsd reads the kernel's queue on: the next changes get their ids, in order.
                let read = 1 + tape.below(3, "ids assigned");
                self.assigned_to = (self.assigned_to + read).min(self.log.len() as u64);
                Ok(())
            }
            _ => {
                // The driver's tick: its expiry, and its own file checked.
                let step = live.session.tick(self.now).map_err(harness)?;
                let replaced = match live.build != self.build {
                    true => live.session.binary_replaced().map_err(harness)?,
                    false => Step::default(),
                };
                self.handle(tape, Step { flush: step.flush || replaced.flush, exit: step.exit.or(replaced.exit) })
            }
        }
    }

    /// Oracle 2: each ask given up at its deadline, and none still waiting past it.
    fn deadlines(&mut self, tape: &Tape) -> Result<(), Broke> {
        for at in 0..self.cmds.len() {
            let Cmd::Asking { mut asking, id, .. } = self.cmds[at] else { continue };
            if asking.is_done() {
                continue;
            }
            if let Some(next) = asking.tick_planted(self.now, self.plants.ask) {
                self.set_asking(at, asking);
                if let Some(live) = self.live.as_mut() {
                    live.asked.retain(|(it, _)| *it != id);
                }
                self.finish(tape, at, next, &[])?;
                continue;
            }
            let (now, deadline) = (self.now, asking.deadline());
            oracle(now <= deadline, || format!("a command still waits on its ask at {} ms, past its deadline at {} ms", now.0 / 1_000_000, deadline.0 / 1_000_000))?;
        }
        Ok(())
    }
}

impl<'h> Sim<'h> {
    /// A checkout of no changes, no helper, two idle commands.
    fn new(heap: &'h Heap, plants: Plants) -> Sim<'h> {
        let idle = Cmd::Idle { saved: Saved::default(), read: [0; FILES] };
        Sim {
            heap,
            plants,
            files: [0; FILES],
            next_version: 0,
            log: StdVec::new(),
            assigned_to: 0,
            build: 1,
            socket: Socket::None,
            live: None,
            started: 0,
            cmds: [idle, idle],
            now: Nanos(0),
            step: 0,
            next_ask: 0,
            newest: None,
            old_build_asks: 0,
        }
    }
}

/// One case, its virtual time told to `lap`.
fn run(tape: &Tape, plants: Plants, lap: &Lap) -> Result<(), Broke> {
    let heap = Heap::new();
    let mut sim = Sim::new(&heap, plants);
    let result = drive(tape, &mut sim);
    lap.clocked(sim.now.0, result)
}

/// The tape's steps, each followed by the deadlines' oracle.
fn drive(tape: &Tape, sim: &mut Sim<'_>) -> Result<(), Broke> {
    let steps = tape.range(1..300, "steps");
    for _ in 0..steps {
        sim.step += 1;
        sim.now = sim.now.after(Nanos::millis(tape.below(30, "milliseconds")));
        match tape.weighted(&[5, 6, 3, 2, 1], "step") {
            0 => sim.change(tape),
            1 => sim.helper_step(tape)?,
            2 => {
                let at = tape.index(sim.cmds.len(), "asker");
                sim.ask(tape, at)?;
            }
            3 => {
                let at = tape.index(sim.cmds.len(), "reader");
                if let Cmd::ToRead { saved } = sim.cmds[at] {
                    sim.cmds[at] = Cmd::Idle { saved, read: sim.files };
                }
            }
            _ => {
                if tape.one_in(4, "the binary replaced") {
                    sim.build += 1;
                }
            }
        }
        sim.deadlines(tape)?;
    }
    Ok(())
}

/// The invariant: every command learns every change since its saved place (via the helper or journal replay) respecting flush cookies, event-id barriers, binary changes, queue caps, and deadlines.
#[test]
fn every_command_is_told_every_change_since_its_place_by_the_helper_or_its_own_replay() {
    let lap = Lap::new("helper: every command told every change");
    check(Config::cases(3000), |tape| run(tape, NONE, &lap));
}

/// The plant ([`SincePlant::NoFlush`], since.rs `Watcher::asked`): every ask is answered at
/// once with a place issued then, no flush asked. Before the helper's first flush that answer's
/// journal place is empty and the command takes nothing of it (the next test); after it, the
/// answer carries the first flush's journal place, so it IS paired and taken: a helper's
/// answer sent before any cookie of its own, missing every change still queued. Caught only by
/// the oracles that judge a TAKEN answer ("not named by the helper": oracle 1 against what the
/// command read, oracle 5 against its replay from the paired journal place); a catch by the
/// command's own replay, or by the harness, does not count.
#[test]
fn the_oracle_catches_a_helper_that_answers_before_its_flush_cookie() {
    let lap = Lap::new("helper: control, answered before the flush cookie");
    control(Config::cases(6000), NONE, NO_BARRIER, "not named by the helper", |tape, plants| run(tape, plants, &lap));
}

/// The command's rule for a helper's answer whose journal place is not one (journal_ask.rs
/// `Asked`, its `None` arm; what [`NO_BARRIER`] sends before its first flush): nothing of it is
/// taken, the command replays from the journal place it saved, and saves no helper's place.
/// CONTROL: the same wrong answer (no names, a file moved since the command's place) with a
/// journal place is taken, and the oracle fails it: the rule, not the answer, is what made the
/// first pass.
#[test]
fn an_answer_with_no_journal_place_is_replayed_and_the_same_with_one_is_judged() {
    let heap = Heap::new();
    let tape = Tape::calm();
    let ours = |seq| Place { instance: 1, seq };
    let given = |journal: JournalAt| {
        let mut sim = Sim::new(&heap, NONE);
        // File 0 changed after the command read it at journal length 0, before its ask.
        sim.next_version = 1;
        sim.files[0] = 1;
        sim.log.push(0);
        let (asking, _) = Asking::new(version(1), CHECKOUT, Some(ours(1)), Nanos(0));
        let saved = Saved { helper: Some(ours(1)), journal: Some(0) };
        sim.cmds[0] = Cmd::Asking { asking, id: AskId(1), saved, read: [0; FILES], at_ask: sim.files, ask_log: 1, began: 0 };
        let wrong = AskNext::Answered(AskAnswer::Vouched { since: ours(1), place: ours(2), journal });
        let result = sim.finish(&tape, 0, wrong, &[]);
        (result, sim.cmds[0])
    };
    let (replayed, cmd) = given(JournalAt::NONE);
    assert!(replayed.is_ok(), "an unpairable answer was judged as taken");
    assert!(
        matches!(cmd, Cmd::ToRead { saved: Saved { helper: None, journal: Some(1) } }),
        "an unpairable answer must leave the replay's end saved and no helper's place: {cmd:?}"
    );
    let (taken, _) = given(at_of(0));
    assert!(
        matches!(&taken, Err(Broke::Oracle(why)) if why.contains("not named by the helper")),
        "CONTROL: the same answer with a journal place is taken, and its missing name must be caught"
    );
}

/// The seam's other barrier ([`Barrier::EventId`]): the stream's last delivered id against
/// the current id read at the ask, answered at once when nothing with an id is pending. A
/// change made before the ask and still in the kernel's queue has no id, so it is not waited
/// for: oracles 1 and 5 catch the answer given before the batch that holds it. This is why the
/// real driver keeps the cookie.
#[test]
fn the_oracle_catches_an_event_id_barrier_answered_before_the_pending_batch() {
    let lap = Lap::new("helper: control, an event-id barrier");
    control(Config::cases(6000), NONE, EVENT_ID, "not named", |tape, plants| run(tape, plants, &lap));
}

#[test]
fn the_oracle_catches_a_helper_that_survives_a_binary_change() {
    let lap = Lap::new("helper: control, survives a binary change");
    control(Config::cases(6000), NONE, SURVIVES, "an old build", |tape, plants| run(tape, plants, &lap));
}

#[test]
fn the_oracle_catches_a_cap_that_drops_names_silently() {
    let lap = Lap::new("helper: control, a cap that drops names silently");
    control(Config::cases(6000), NONE, FORGETS, "not named", |tape, plants| run(tape, plants, &lap));
}

#[test]
fn the_oracle_catches_a_helper_that_vouches_past_dropped_events() {
    let lap = Lap::new("helper: control, vouches past dropped events");
    control(Config::cases(6000), NONE, DROPS_IGNORED, "not named", |tape, plants| run(tape, plants, &lap));
}

#[test]
fn the_oracle_catches_an_ask_with_no_deadline() {
    let lap = Lap::new("helper: control, an ask with no deadline");
    control(Config::cases(6000), NONE, NO_DEADLINE, "past its deadline", |tape, plants| run(tape, plants, &lap));
}

/// The frames round-trip, and a frame of another form is refused whole.
#[test]
fn the_frames_round_trip_and_another_form_is_refused() {
    let heap = Heap::new();
    let hello = Hello { version: version(3), instance: 9, checkout: CHECKOUT };
    let mut out = blaze_own::Vec::new_in(&heap);
    hello.encode(&mut out);
    let len = helper::frame_len(&out).expect("a length word").expect("within the cap");
    assert_eq!(Hello::decode(&out[4..4 + len]), Ok(hello));
    let place = Place { instance: 9, seq: 4 };
    let since = Place { instance: 9, seq: 2 };
    out.clear();
    out.clear();
    helper::encode_answer(&mut out, Answer::CannotVouch { why: CannotVouch::NoPlace, place: Some(place) }, at_of(76), &[]);
    let len = helper::frame_len(&out).expect("a length word").expect("within the cap");
    {
        let mut names = blaze_own::Vec::new_in(&heap);
        let refused = AskAnswer::CannotVouch { why: CannotVouch::NoPlace, place: Some(place), journal: at_of(76) };
        assert_eq!(helper::decode_answer(&out[4..4 + len], &mut names), Ok(refused));
        assert!(names.is_empty());
    }
    out.clear();
    helper::encode_answer(&mut out, Answer::Vouched { since, place }, at_of(77), &["a", "c/d"]);
    let len = helper::frame_len(&out).expect("a length word").expect("within the cap");
    {
        // In a block of its own: the names borrow the frame's bytes, which the control changes.
        let mut names = blaze_own::Vec::new_in(&heap);
        assert_eq!(helper::decode_answer(&out[4..4 + len], &mut names), Ok(AskAnswer::Vouched { since, place, journal: at_of(77) }));
        assert_eq!(&names[..], &["a", "c/d"]);
    }
    // CONTROL: the form's byte changed (`[.., 1, ..]` to `[.., 2, ..]`) is refused by name.
    let form_at = 4 + 1;
    out[form_at] = 2;
    let mut refused = blaze_own::Vec::new_in(&heap);
    assert_eq!(helper::decode_answer(&out[4..4 + len], &mut refused), Err(helper::WireError::Form(2)));
}
