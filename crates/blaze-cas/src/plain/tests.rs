//! The plain store under the deterministic simulation (`blaze_core::sim::Sim`): two writers
//! interleaved on one disk, power cuts at any completion (each file keeping a random prefix of
//! what was not synced, torn at a sector), failed effects, and a reader of a new boot after.
//! The writers put under three namespaces (this machine's, the public one, an organisation's),
//! so each commit writes up to three segments.
//!
//! The invariant: the open after a power cut reads a whole generation `w` ([`Report::whole`]),
//! or none; every record committed at or before `w` reads back byte for byte, under the
//! namespace it was put in, or its segment is reported lost by name; a record committed after
//! `w` (no sync on the edit path, so a cut can take every byte of it) reads back whole or not
//! at all; a record whose commit had not completed reads back whole or not at all; a key never
//! put is never found; a record put under one namespace is never found by a lookup that asks
//! only others (a `LOCAL` record by none that asks only `PUBLIC` or an organisation's); no read
//! returns a corrupt record; and with no power cut (in durable mode, with one too) nothing is
//! lost and every committed record reads back.
//!
//! A loss is reported at least once, never zero times: an open's report counts once its open
//! returned with it (`Driven::with`; here [`Plain::handed`]); one whose process stopped first
//! (an injected failure after its flip's slot landed: at the unlink of a file past its grace,
//! at mapping a segment another process committed, at the head's sync in durable mode) did not
//! report, and a later open must, from the mark its flip wrote. So every committed record is
//! read, or named lost by some open's handed report after the cut.
//!
//! [`Report::whole`]: super::Report::whole
//!
//! The controls (`the_oracle_catches_*`) plant one bug each and show the oracle fails.

use std::{
    alloc::{Allocator, Global},
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

use blaze_core::{
    Completion, Done, Driver, Effect, FileName, IoError, Tag as EffectTag,
    sim::{Faults, Sim},
    wait::Deadline,
};
use blaze_ids::ContentId;
use blaze_keyed::{
    exec::RecordRef,
    kind::{Namespace, Tag, Tier},
    store::{ENTRY, FOOTER, HEADER, Located, Put, Refused, RowRefused, SEGMENT_MOST, SegmentId, SegmentLen, SegmentRef, Source, Statement, StoreError, Verifier},
};
use blaze_mem::Frame;
use blaze_sim::{Config as SimConfig, Tape};

use super::{
    core::{CLEANUP_SLICES_MOST, CURSOR_BYTES, Committed, INDEX_CHUNK, MERGE_BUFFER, MERGE_CHUNKS_IN_FLIGHT, Plain, PlainConfig, Plant, Sliced, TAGS, TIER_FLOOR_BYTES, TIER_SEGMENTS_MOST, TierPlant, TierRow, Tiers},
    driven::{Cut, Driven, LocalDir},
    format::{self, Entry, FORMAT, SEGMENT_FORMAT, Slot, check_segment, seal_segment},
    handle::{LaneId, PlainStore},
    test_root,
};
use crate::Key;

/// A tag no store owns: the test's own asks (a clock, a listing).
const OWN: u64 = 1 << 60;

/// The tag every test record is put with.
const TAG: Tag = Tag(1);

fn dir() -> FileName {
    FileName::new("plain").expect("a name")
}

/// The test's organisation.
fn org() -> Namespace {
    Namespace::named("an organisation")
}

/// What the writers write, in the order a commit writes their segments.
fn writes() -> Vec<Namespace> {
    vec![Namespace::LOCAL, Namespace::PUBLIC, org()]
}

/// What every store here reads, in the order a lookup asks them (as `Settings::reads`).
fn reads() -> Vec<Namespace> {
    vec![Namespace::LOCAL, org(), Namespace::PUBLIC]
}

/// The namespace record `n` is put under: the three in turn.
fn space(n: u32) -> Namespace {
    writes()[n as usize % 3]
}

/// `list`, its tables in `root`.
fn spaces_in(list: Vec<Namespace>, root: &Frame) -> Vec<Namespace, &Frame> {
    let mut out = Vec::with_capacity_in(list.len(), root);
    out.extend(list);
    out
}

fn cfg(n: u64, boot: u64, durable: bool, root: &Frame) -> PlainConfig<&Frame> {
    let mut path = Vec::with_capacity_in(MACHINE.len(), root);
    path.extend_from_slice(MACHINE);
    let mut machine = Vec::with_capacity_in(1, root);
    machine.push(path);
    PlainConfig {
        dir: dir(),
        tags: n * TAGS,
        cap_bytes: u64::MAX,
        boot,
        nonce: 100 + n,
        durable,
        machine,
        writes: spaces_in(writes(), root),
        reads: spaces_in(reads(), root),
        liveness: super::Liveness::Everything,
        lock_step: crate::plain::LockStep::Backoff,
    }
}

fn key(n: u32) -> Key {
    Key(ContentId::of("plain store test key", &n.to_le_bytes()))
}

fn value(n: u32) -> Vec<u8> {
    let len = (n as usize * 37) % 700;
    (0..len).map(|i| (n as usize).wrapping_mul(31).wrapping_add(i) as u8).collect()
}

struct Actor<'f> {
    /// The process's store, its tables in the run's frame.
    store: Plain<&'f Frame>,
    effects: Vec<Effect>,
    /// The test's own failure: one effect of this kind fails, once, after this actor's flip
    /// wrote its slot (the commit point), so the generation that retires the losses its open
    /// found is on disk and the actor stops before its open returns.
    inject: Option<Place>,
    /// This actor's flip wrote its slot: the next effect of the injected kind fails.
    armed: bool,
    /// The injection fired.
    fired: bool,
    /// What each effect this actor asked is, by tag, while it is in flight (only when it
    /// injects).
    kinds: BTreeMap<u64, Kind>,
    /// The merge meter, counting while it is on.
    meter: Meter,
}

/// THE MERGE METER, the simulated driver's count of what a compaction's copy holds at once
/// (core.rs, `merge_holds_most`): while `on`, every write the store asks of a segment file, the
/// largest one write and the most bytes of such writes in flight at once (asked, not yet
/// completed: the bytes an effect owns live until it completes), records and index apart. A
/// segment's header (the write at 0, which holds its count at 16..20 and its index's offset at
/// 24..32) and footer (the write at the index's end) are counted by neither; a write at or past
/// the index's offset and before its end is the index's.
#[derive(Default)]
struct Meter {
    on: bool,
    /// Each write in flight: its bytes, and whether they are the index's.
    flying: BTreeMap<u64, (u64, bool)>,
    alive: u64,
    alive_most: u64,
    write_most: u64,
    /// The record bytes written while on.
    total: u64,
    /// The index's: in flight, the most at once, the largest write, all written.
    index_alive: u64,
    index_alive_most: u64,
    index_write_most: u64,
    index_total: u64,
    /// Each segment file's index: from its offset to its end.
    index: BTreeMap<String, (u64, u64)>,
}

impl Meter {
    fn asked(&mut self, effect: &Effect) {
        let Effect::WriteAt { file, at, bytes, tag } = effect else { return };
        let segment = file.as_str().rsplit('/').next().is_some_and(|leaf| leaf.starts_with("seg-"));
        if !self.on || !segment {
            return;
        }
        if *at == 0 && bytes.len() == HEADER {
            let count = u64::from(u32::from_le_bytes(bytes[16..20].try_into().expect("4 bytes")));
            let index_at = u64::from_le_bytes(bytes[24..32].try_into().expect("8 bytes"));
            self.index.insert(file.as_str().to_owned(), (index_at, index_at + count * ENTRY as u64));
            return;
        }
        let place = self.index.get(file.as_str()).copied();
        if place.is_some_and(|(_, end)| *at >= end) {
            return;
        }
        let len = bytes.len() as u64;
        if place.is_some_and(|(index_at, _)| *at >= index_at) {
            self.flying.insert(tag.0, (len, true));
            self.index_alive += len;
            self.index_total += len;
            self.index_alive_most = self.index_alive_most.max(self.index_alive);
            self.index_write_most = self.index_write_most.max(len);
            return;
        }
        self.flying.insert(tag.0, (len, false));
        self.alive += len;
        self.total += len;
        self.alive_most = self.alive_most.max(self.alive);
        self.write_most = self.write_most.max(len);
    }

    fn done(&mut self, tag: u64) {
        match self.flying.remove(&tag) {
            Some((len, true)) => self.index_alive -= len,
            Some((len, false)) => self.alive -= len,
            None => {}
        }
    }

    /// The bound, against a store's `buffer` and `index_chunk`: no write of records over its
    /// chunk, no more than the buffer of records in flight; no more than one index chunk in
    /// flight; and the core built no more than one index chunk and `inputs` cursors at once
    /// (`held`, what it said: `Plain::take_merge_held_most`).
    fn within(&self, buffer: u64, index_chunk: u64, held: u64, inputs: usize) -> Result<(), String> {
        let chunk = buffer / u64::from(MERGE_CHUNKS_IN_FLIGHT);
        if self.write_most > chunk || self.alive_most > buffer {
            return Err(format!(
                "the merge meter: a compaction's copy wrote {} bytes of records in one write (its chunk is {chunk}) and held {} in flight at once, past MERGE_BUFFER = {buffer}",
                self.write_most, self.alive_most
            ));
        }
        // One chunk of whole entries: `index_chunk / ENTRY` of them.
        let index_most = (index_chunk / ENTRY as u64).max(1) * ENTRY as u64;
        let held_most = index_most + inputs as u64 * CURSOR_BYTES;
        if self.index_alive_most > index_most || held > held_most {
            return Err(format!(
                "the merge meter: a compaction's copy held {} bytes of its index in flight at once (largest write {}) and built {held} at once, past one INDEX_CHUNK = {index_most} and {inputs} cursors ({held_most})",
                self.index_alive_most, self.index_write_most
            ));
        }
        Ok(())
    }
}

/// Where, after a flip's slot lands, the test fails one effect: the three effects a flip asks
/// after its commit point.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    /// Unlinking a file retired past its grace period.
    Remove,
    /// Mapping a segment the new generation names and this process has not mapped (another
    /// process committed it between this one's open and its flip).
    Map,
    /// The head's sync, in durable mode.
    HeadSync,
}

/// The kinds of effect [`Place`] tells apart, and the slot write that arms it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Slot,
    At(Place),
    Other,
}

fn kind_of(effect: &Effect) -> Kind {
    match effect {
        // A flip's slot: 64 bytes in place in the head, not the blank of the other slot.
        Effect::WriteAt { file, bytes, .. } if file.as_str().ends_with("/head") && bytes.len() == format::SLOT && bytes.iter().any(|it| *it != 0) => Kind::Slot,
        Effect::Remove { .. } => Kind::At(Place::Remove),
        Effect::Map { .. } => Kind::At(Place::Map),
        Effect::Sync { file, .. } if file.as_str().ends_with("/head") => Kind::At(Place::HeadSync),
        _ => Kind::Other,
    }
}

impl<'f> Actor<'f> {
    fn new(cfg: PlainConfig<&'f Frame>, root: &'f Frame, plant: Plant) -> Actor<'f> {
        Actor::injecting(cfg, root, plant, None)
    }

    fn injecting(cfg: PlainConfig<&'f Frame>, root: &'f Frame, plant: Plant, inject: Option<Place>) -> Actor<'f> {
        let mut store = Plain::planted(cfg, root, plant);
        let mut effects = Vec::new_in(Global);
        store.open_whole(&mut effects);
        Actor { store, effects, inject, armed: false, fired: false, kinds: BTreeMap::new(), meter: Meter::default() }
    }

    /// Before submit: what each effect asked is, when this actor injects; the merge meter's
    /// count.
    fn asking(&mut self) {
        for effect in &self.effects {
            self.meter.asked(effect);
            if self.inject.is_some() {
                self.kinds.insert(effect.tag().0, kind_of(effect));
            }
        }
    }

    /// A completion for this actor, failed when it is the injected one.
    fn injected(&mut self, completion: &mut Completion) {
        self.meter.done(completion.tag.0);
        let Some(place) = self.inject else { return };
        let kind = self.kinds.remove(&completion.tag.0).unwrap_or(Kind::Other);
        if kind == Kind::Slot && matches!(completion.done, Done::Written) {
            self.armed = true;
        } else if self.armed && !self.fired && kind == Kind::At(place) {
            self.fired = true;
            completion.done = Done::Failed(IoError(format!("injected by the test: {place:?} after the flip's slot landed")));
        }
    }
}

/// A store stopped by an injected failure (the simulation's or the test's), not on its own.
fn injected(why: &StoreError) -> bool {
    matches!(why, StoreError::Io { error, .. } if error.contains("injected"))
}

/// Runs until nothing is in flight, or `budget` completions have been taken (then `false`: the
/// caller cuts the power). Bounded: a run that does not settle in a million polls fails.
fn run(sim: &mut Sim, actors: &mut [Actor<'_>], budget: &mut Option<u32>) -> bool {
    run_traced(sim, actors, budget, &mut 0, &mut None)
}

/// [`run`], each effect asked and each completion taken written to `trace` when there is one.
/// The trace only observes: every statement of the untraced run is the same, in the same
/// order, and the trace reads (never takes) what the run holds.
fn run_traced(sim: &mut Sim, actors: &mut [Actor<'_>], budget: &mut Option<u32>, taken: &mut u32, trace: &mut Option<Trace>) -> bool {
    let mut done: Vec<Completion> = Vec::new_in(Global);
    for _ in 0..1_000_000 {
        for (i, actor) in actors.iter_mut().enumerate() {
            if let Some(trace) = trace {
                trace.asked(i, &actor.effects);
            }
            actor.asking();
            sim.submit(&mut actor.effects);
        }
        if sim.in_flight() == 0 {
            return true;
        }
        let deadline = Deadline::after(sim.now(), Duration::from_secs(1));
        sim.poll(&mut done, deadline);
        let wall = sim.wall_ns();
        if let Some(trace) = trace {
            trace.polled(&done);
        }
        for mut completion in done.drain(..) {
            if completion.tag.0 >= OWN {
                continue;
            }
            if let Some(left) = budget {
                if *left == 0 {
                    // The power cut: this completion and the rest of the poll's reach no
                    // store (the trace lists them from what it read before the drain).
                    if let Some(trace) = trace {
                        trace.cut(&completion);
                    }
                    return false;
                }
                *left -= 1;
            }
            *taken += 1;
            let actor = actors.iter_mut().find(|it| it.store.owns(completion.tag)).expect("a store asked for it");
            actor.injected(&mut completion);
            if let Some(trace) = trace {
                trace.done(&completion);
            }
            actor.store.complete(completion, wall, &mut actor.effects);
            if let Some(trace) = trace
                && let Some(why) = actor.store.broken()
            {
                trace.broke(why);
            }
        }
    }
    panic!("the simulation did not settle in a million polls");
}

fn settle(sim: &mut Sim, actors: &mut [Actor<'_>]) {
    assert!(run(sim, actors, &mut None));
}

/// One effect of the test's own, run to its completion.
fn ask(sim: &mut Sim, effect: Effect) -> Done {
    let mut effects = vec![effect];
    let mut done = Vec::new_in(Global);
    for _ in 0..10_000 {
        sim.submit(&mut effects);
        if sim.in_flight() == 0 {
            break;
        }
        sim.poll(&mut done, Deadline::after(sim.now(), Duration::from_secs(1)));
    }
    done.pop().expect("its completion").done
}

/// Moves the simulation's clocks on by `after`.
fn wait(sim: &mut Sim, after: Duration) {
    assert!(matches!(ask(sim, Effect::Timer { after, tag: EffectTag(OWN) }), Done::Fired));
}

/// A reader of `boot` with `plant`, opened again while an injected failure breaks its open (a
/// new process each time: the store never retries itself). Its open returned, so its report
/// was handed ([`Plain::handed`]): what `Driven::with` does as it returns.
fn reader_with<'f>(sim: &mut Sim, n: u64, boot: u64, plant: Plant, root: &'f Frame) -> Actor<'f> {
    reader_traced(sim, n, boot, plant, None, &mut 0, &mut None, root)
}

/// [`reader_with`], each attempt's effects and what its open found written to `trace`; the
/// first attempt fails one effect at `inject` after its flip's slot lands (durable when that is
/// the head's sync).
fn reader_traced<'f>(sim: &mut Sim, n: u64, boot: u64, plant: Plant, inject: Option<Place>, taken: &mut u32, trace: &mut Option<Trace>, root: &'f Frame) -> Actor<'f> {
    let durable = inject == Some(Place::HeadSync);
    for attempt in 0..50 {
        if let Some(trace) = trace {
            trace.cast = vec![format!("reader {n} attempt {attempt}")];
        }
        let mut it = [Actor::injecting(cfg(n, boot, durable, root), root, plant, if attempt == 0 { inject } else { None })];
        assert!(run_traced(sim, &mut it, &mut None, taken, trace));
        let [mut it] = it;
        if let Some(trace) = trace {
            trace.opened(0, &it.store);
        }
        match it.store.broken() {
            None => {
                it.store.handed();
                return it;
            }
            Some(why) => assert!(injected(why), "a reader broke on its own: {why}\n\nthe run, traced (when it is):\n{}", trace.as_ref().map(Trace::render).unwrap_or_default()),
        }
    }
    panic!("no reader opened in 50 tries");
}

/// The flip that clears the marks of the losses `actor`'s handed report named, run (none when
/// it named none): whether the marks are cleared (`false`: an injected failure stopped it, and
/// the marks stay for the next open to report again).
fn acknowledged(sim: &mut Sim, actor: &mut Actor<'_>, taken: &mut u32, trace: &mut Option<Trace>) -> bool {
    if !actor.store.acknowledge(&mut actor.effects) {
        return true;
    }
    assert!(run_traced(sim, std::slice::from_mut(actor), &mut None, taken, trace));
    match actor.store.broken() {
        None => true,
        Some(why) => {
            assert!(injected(why), "an acknowledgement broke on its own: {why}");
            false
        }
    }
}

/// A reader that opened, handed its report and cleared the marks it named: a process that
/// read the store and made no flip of its own ([`Driven::acknowledge`]).
fn reader<'f>(sim: &mut Sim, n: u64, boot: u64, root: &'f Frame) -> Actor<'f> {
    let mut it = reader_with(sim, n, boot, Plant::None, root);
    assert!(acknowledged(sim, &mut it, &mut 0, &mut None), "no failure is injected here");
    it
}

/// The segments a report names lost.
fn named_lost<A: Allocator + Clone>(store: &Plain<A>) ->BTreeSet<u64> {
    store.report().lost.iter().filter_map(|it| it.segment).collect()
}

/// At most this many events in a trace; the rest are counted, not kept.
const TRACE_MOST: usize = 4000;

/// An effect a store asked, as the trace shows it: who asked it, what it is (with what its
/// bytes say, decoded: a head slot's generation, a manifest's segments) and its file.
struct Asked {
    who: usize,
    kind: &'static str,
    file: String,
    what: String,
}

/// One step of a traced run, in the order it happened.
enum Event {
    /// A completion handed to its store; `index` counts the completions taken since the trace
    /// began, from 1 (the scenario's budget counts the same ones).
    Done { index: u32, who: String, tag: u64, asked: String, outcome: String },
    /// The completion the power cut fell on: taken from the disk, never handed to its store.
    Cut { index: u32, who: String, tag: u64, asked: String, outcome: String },
    /// Completed on the disk in the same poll as the cut, after it: never handed to its store.
    Unseen { who: String, tag: u64, asked: String, outcome: String },
    /// Asked and not completed when the power went.
    InFlight { who: String, tag: u64, asked: String },
    /// Pushed by a store and never submitted: the power went first.
    Unsubmitted { who: String, tag: u64, asked: String },
    /// A store stopped, at the completion before this event.
    Broke { who: String, why: String },
    /// The test's own steps: a writer's puts, what `commit` returned, what `take_committed`
    /// gave after the run.
    Put { who: String, keys: String },
    Commit { who: String, returned: String },
    Committed { who: String, outcome: String },
    Crash,
    /// After the power cut: a file as it survived (pure reads of the simulated disk).
    Disk { file: String, says: String },
    /// A store's open ended: what it read and chose.
    Opened { who: String, says: String },
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Event::Done { index, who, tag, asked, outcome } => write!(f, "#{index:<4} {who:<18} t{tag:<6} {asked} -> {outcome}"),
            Event::Cut { index, who, tag, asked, outcome } => write!(f, "#{index:<4} {who:<18} t{tag:<6} {asked} -> {outcome}  [POWER CUT HERE: never handed to the store]"),
            Event::Unseen { who, tag, asked, outcome } => write!(f, "      {who:<18} t{tag:<6} {asked} -> {outcome}  [landed after the cut's completion, never handed to the store]"),
            Event::InFlight { who, tag, asked } => write!(f, "      {who:<18} t{tag:<6} {asked}  [in flight at the cut]"),
            Event::Unsubmitted { who, tag, asked } => write!(f, "      {who:<18} t{tag:<6} {asked}  [pushed, never submitted: the cut came first]"),
            Event::Broke { who, why } => write!(f, "  ** {who} broke: {why}"),
            Event::Put { who, keys } => write!(f, "  >> {who} put {keys}"),
            Event::Commit { who, returned } => write!(f, "  >> {who} commit() returned {returned}"),
            Event::Committed { who, outcome } => write!(f, "  >> {who} take_committed() = {outcome}"),
            Event::Crash => write!(f, "  == sim.crash()"),
            Event::Disk { file, says } => write!(f, "  disk {file}: {says}"),
            Event::Opened { who, says } => write!(f, "  >> {who} opened: {says}"),
        }
    }
}

/// A bounded log of a run: every effect asked, every completion taken and its outcome, the
/// test's steps, the disk after the cut, each open's result. Written by the scenario's driver
/// only; reading it changes nothing the simulation draws.
#[derive(Default)]
struct Trace {
    events: Vec<Event>,
    dropped: u32,
    /// What each store asked, by effect tag, oldest first (a tag asked again before its
    /// completion queues behind).
    asked: BTreeMap<u64, Vec<Asked>>,
    /// Every file an effect named.
    files: BTreeSet<String>,
    completions: u32,
    /// The actors' names, by their index in the run.
    cast: Vec<String>,
    /// The current poll's store completions as [`Trace::polled`] read them (tag, who, asked,
    /// outcome), and how many of them were handed to their stores so far.
    poll: Vec<(u64, String, String, String)>,
    taken_in_poll: usize,
    /// Who the last completion handed over went to.
    last_who: String,
}

impl Trace {
    fn push(&mut self, event: Event) {
        if self.events.len() < TRACE_MOST {
            self.events.push(event);
        } else {
            self.dropped += 1;
        }
    }

    fn who(&self, i: usize) -> String {
        self.cast.get(i).cloned().unwrap_or_else(|| format!("actor {i}"))
    }

    fn asked(&mut self, i: usize, effects: &[Effect]) {
        for effect in effects {
            let asked = describe(i, effect);
            if !asked.file.is_empty() {
                self.files.insert(asked.file.clone());
            }
            self.asked.entry(effect.tag().0).or_default().push(asked);
        }
    }

    /// The asked effect `completion` answers, and its outcome, rendered.
    fn answer(&mut self, completion: &Completion) -> (String, String, String) {
        let tag = completion.tag.0;
        let asked = match self.asked.get_mut(&tag) {
            Some(queue) if !queue.is_empty() => Some(queue.remove(0)),
            _ => None,
        };
        if self.asked.get(&tag).is_some_and(|it| it.is_empty()) {
            self.asked.remove(&tag);
        }
        match asked {
            Some(asked) => {
                let outcome = outcome(&asked, &completion.done);
                (self.who(asked.who), format!("{} {}{}", asked.kind, asked.file, asked.what), outcome)
            }
            None => ("?".into(), "(not seen asked)".into(), format!("{:?}", completion.done)),
        }
    }

    /// Reads (never takes) one poll's completions before the run drains them: what each store's
    /// completion answers, rendered, so the ones a power cut leaves undelivered can be listed.
    fn polled(&mut self, done: &[Completion]) {
        self.poll.clear();
        self.taken_in_poll = 0;
        let mut seen: BTreeMap<u64, usize> = BTreeMap::new();
        for completion in done.iter().filter(|it| it.tag.0 < OWN) {
            let tag = completion.tag.0;
            let k = seen.entry(tag).or_insert(0);
            let rendered = match self.asked.get(&tag).and_then(|queue| queue.get(*k)) {
                Some(asked) => (self.who(asked.who), format!("{} {}{}", asked.kind, asked.file, asked.what), outcome(asked, &completion.done)),
                None => ("?".into(), "(not seen asked)".into(), format!("{:?}", completion.done)),
            };
            *k += 1;
            self.poll.push((tag, rendered.0, rendered.1, rendered.2));
        }
    }

    /// The asked effect of `tag` answered: off the queue of what is still in flight.
    fn retire(&mut self, tag: u64) {
        if let Some(queue) = self.asked.get_mut(&tag)
            && !queue.is_empty()
        {
            queue.remove(0);
        }
        if self.asked.get(&tag).is_some_and(|it| it.is_empty()) {
            self.asked.remove(&tag);
        }
    }

    /// A completion handed to its store.
    fn done(&mut self, completion: &Completion) {
        self.completions += 1;
        self.taken_in_poll += 1;
        let (who, asked, outcome) = self.answer(completion);
        self.last_who = who.clone();
        self.push(Event::Done { index: self.completions, who, tag: completion.tag.0, asked, outcome });
    }

    /// The completion the budget ran out on, and after it the rest of its poll, as
    /// [`Trace::polled`] read them: none reaches a store.
    fn cut(&mut self, completion: &Completion) {
        self.completions += 1;
        let (who, asked, outcome) = self.answer(completion);
        self.push(Event::Cut { index: self.completions, who, tag: completion.tag.0, asked, outcome });
        let rest: Vec<(u64, String, String, String)> = self.poll.drain(..).skip(self.taken_in_poll + 1).collect();
        for (tag, who, asked, outcome) in rest {
            self.retire(tag);
            self.push(Event::Unseen { who, tag, asked, outcome });
        }
    }

    /// Effects a store pushed and the run never submitted (the cut came first): read, not taken.
    fn unsubmitted(&mut self, i: usize, effects: &[Effect]) {
        for effect in effects {
            let asked = describe(i, effect);
            let who = self.who(i);
            self.push(Event::Unsubmitted { who, tag: effect.tag().0, asked: format!("{} {}{}", asked.kind, asked.file, asked.what) });
        }
    }

    /// Every effect still asked: in flight when the power goes. Then the cut itself.
    fn crash(&mut self) {
        let left = std::mem::take(&mut self.asked);
        for (tag, queue) in left {
            for asked in queue {
                let who = self.who(asked.who);
                self.push(Event::InFlight { who, tag, asked: format!("{} {}{}", asked.kind, asked.file, asked.what) });
            }
        }
        self.push(Event::Crash);
    }

    /// The store the last completion went to stopped.
    fn broke(&mut self, why: &StoreError) {
        let who = self.last_who.clone();
        // Once per store: a broken store stays broken.
        let seen = self.events.iter().any(|it| matches!(it, Event::Broke { who: w, .. } if *w == who));
        if !seen {
            self.push(Event::Broke { who, why: why.to_string() });
        }
    }

    fn opened<A: Allocator + Clone>(&mut self, i: usize, store: &Plain<A>) {
        let report = store.report();
        let mut lost = String::new();
        for it in &report.lost {
            lost.push_str(&format!("[{} segment {:x?} records {:?}: {}] ", it.file.as_str(), it.segment, it.records, it.why));
        }
        let says = format!(
            "broken {:?}; reads generation {}; whole {:?}; segments (id, records) {:x?}; checked {}; lost {}",
            store.broken(),
            store.generation(),
            report.whole,
            store.segments(),
            report.checked,
            if lost.is_empty() { "none".into() } else { lost }
        );
        let who = self.who(i);
        self.push(Event::Opened { who, says });
    }

    /// Every file an effect named, as the cut left it: length, and for the head each slot and
    /// the manifest each whole slot names, and for a segment whether it checks whole.
    fn disk(&mut self, sim: &Sim) {
        let files: Vec<String> = self.files.iter().cloned().collect();
        for file in files {
            let Some(name) = FileName::new(&file) else { continue };
            if !sim.exists(&name) {
                self.push(Event::Disk { file, says: "gone".into() });
                continue;
            }
            let bytes = sim.contents(&name);
            let mut says = format!("{} bytes", bytes.len());
            if file.ends_with("/head") {
                for (s, at) in format::SLOT_AT.iter().enumerate() {
                    let at = *at as usize;
                    let slot = if bytes.len() >= at + format::SLOT { Some(Slot::decode(&bytes[at..at + format::SLOT])) } else { None };
                    says.push_str(&format!("; slot {s}: {slot:?}"));
                    if let Some(format::SlotRead::Slot(slot)) = slot {
                        says.push_str(&format!(" naming {}", named_manifest(sim, &slot)));
                    }
                }
                says.push_str(&format!("; read_head: {:?}", format::read_head(&bytes)));
            } else if file.contains("/seg-") {
                says.push_str(&match check_segment(&bytes) {
                    Ok(_) => "; checks whole".to_string(),
                    Err(why) => format!("; does not check: {why}"),
                });
            }
            self.push(Event::Disk { file, says });
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for event in &self.events {
            out.push_str(&event.to_string());
            out.push('\n');
        }
        if self.dropped > 0 {
            out.push_str(&format!("... and {} events more, not kept (the trace keeps {TRACE_MOST})\n", self.dropped));
        }
        out
    }
}

/// The generation a power cut left: of the head's two slots as the disk holds them right after
/// the cut, the newest whole one whose manifest its log holds whole (its length, its CRC-32C and
/// its generation); 0 when none. Read by the test from the disk, not from any open: the opens
/// after a cut flip generations of their own, numbered on from this (from 1 when it is 0), so
/// a number an open reads later is not one a writer committed.
fn survived_on_disk(sim: &Sim) -> u64 {
    let head = FileName::new(&format!("{}/head", dir().as_str())).expect("the head's name");
    if !sim.exists(&head) {
        return 0;
    }
    let bytes = sim.contents(&head);
    let mut survived = 0;
    for at in format::SLOT_AT {
        let at = at as usize;
        if bytes.len() < at + format::SLOT {
            continue;
        }
        let format::SlotRead::Slot(slot) = Slot::decode(&bytes[at..at + format::SLOT]) else { continue };
        let Some(log) = FileName::new(&format!("{}/manifests-{:016x}", dir().as_str(), slot.log)) else { continue };
        if !sim.exists(&log) {
            continue;
        }
        let held = sim.contents(&log);
        let (from, len) = (slot.manifest_at as usize, slot.manifest_len as usize);
        let Some(manifest) = held.get(from..from + len) else { continue };
        let whole = blaze_ids::crc32c::crc32c(manifest) == slot.manifest_crc
            && format::Manifest::decode(manifest, SEGMENT_MOST).is_ok_and(|it| it.generation == slot.generation);
        if whole {
            survived = survived.max(slot.generation);
        }
    }
    survived
}

/// The manifest `slot` names, read from its log as the disk holds it now.
fn named_manifest(sim: &Sim, slot: &Slot) -> String {
    let Some(log) = FileName::new(&format!("{}/manifests-{:016x}", dir().as_str(), slot.log)) else {
        return "a log of no name".into();
    };
    let bytes = sim.contents(&log);
    let (at, len) = (slot.manifest_at as usize, slot.manifest_len as usize);
    if bytes.len() < at + len {
        return format!("manifests-{:016x} at {at}+{len}, and the log holds {} bytes", slot.log, bytes.len());
    }
    format!("manifests-{:016x} at {at}+{len}: {}", slot.log, manifest_says(&bytes[at..at + len]))
}

fn manifest_says(bytes: &[u8]) -> String {
    match format::Manifest::decode(bytes, SEGMENT_MOST) {
        Ok(manifest) => {
            let mut out = format!("generation {} segments [", manifest.generation);
            for it in &manifest.segments {
                out.push_str(&format!("{:016x} {:?} x{} of g{}, ", it.id, it.namespace, it.count, it.generation));
            }
            out.push_str(&format!("] retired {}", manifest.retired.len()));
            out
        }
        Err(why) => format!("no manifest: {why}"),
    }
}

/// What `bytes` written to `file` at `at` say, decoded where the trace knows the layout.
fn written_says(file: &str, at: u64, bytes: &[u8]) -> String {
    if file.ends_with("/head") {
        if bytes.len() == format::SLOT {
            let slot = format::SLOT_AT.iter().position(|it| *it == at);
            return format!(" = slot {slot:?}: {:?}", Slot::decode(bytes));
        }
        return format!(" = {:?}", format::read_head(bytes));
    }
    if file.contains("/manifests-") {
        return format!(" = {}", manifest_says(bytes));
    }
    String::new()
}

fn describe(who: usize, effect: &Effect) -> Asked {
    let (kind, file, what): (&'static str, &FileName, String) = match effect {
        Effect::ReadAll { file, .. } => ("read all", file, String::new()),
        Effect::Read { file, at, len, .. } => ("read", file, format!(" {at}+{len}")),
        Effect::WriteAt { file, at, bytes, .. } => ("write at", file, format!(" {at}+{}{}", bytes.len(), written_says(file.as_str(), *at, bytes))),
        Effect::WriteUnsynced { file, bytes, .. } => ("write unsynced", file, format!(" {} bytes{}", bytes.len(), written_says(file.as_str(), 0, bytes))),
        Effect::Write { file, bytes, .. } => ("write", file, format!(" {} bytes{}", bytes.len(), written_says(file.as_str(), 0, bytes))),
        Effect::Append { file, bytes, .. } => ("append", file, format!(" {} bytes", bytes.len())),
        Effect::Truncate { file, len, .. } => ("truncate", file, format!(" to {len}")),
        Effect::Sync { file, .. } => ("sync", file, String::new()),
        Effect::Lock { file, mode, .. } => ("lock", file, format!(" {mode:?}")),
        Effect::List { dir, .. } => ("list", dir.name().expect("the store lists a directory beneath the root"), String::new()),
        Effect::Map { file, .. } => ("map", file, String::new()),
        Effect::Remove { file, .. } => ("remove", file, String::new()),
        Effect::Unlock { lock, .. } => return Asked { who, kind: "unlock", file: String::new(), what: format!("of the lock asked as t{}", lock.0) },
        Effect::Timer { after, .. } => return Asked { who, kind: "timer", file: String::new(), what: format!("{after:?}") },
        other => return Asked { who, kind: "other", file: String::new(), what: format!("{other:?}") },
    };
    Asked { who, kind, file: file.as_str().to_string(), what }
}

fn outcome(asked: &Asked, done: &Done) -> String {
    match done {
        Done::Failed(why) => format!("FAILED ({why})"),
        Done::Read(bytes) if asked.file.ends_with("/head") => format!("read {} bytes = {:?}", bytes.len(), format::read_head(bytes)),
        Done::Read(bytes) if asked.kind == "read" && asked.file.contains("/manifests-") => format!("read {} bytes = {}", bytes.len(), manifest_says(bytes)),
        Done::Read(bytes) => format!("read {} bytes", bytes.len()),
        Done::Listed(entries) => {
            let mut out = String::from("listed [");
            for (file, meta) in entries {
                out.push_str(&format!("{} {}b, ", file.as_str(), meta.len));
            }
            out.push(']');
            out
        }
        Done::Mapped(_) => "Mapped".into(),
        other => format!("{other:?}"),
    }
}

/// Where a committed record landed.
#[derive(Clone, Copy, Debug)]
struct Landed {
    segment: u64,
    generation: u64,
    space: Namespace,
}

#[derive(Default)]
struct Ledger {
    /// Keys whose commit completed.
    committed: BTreeMap<u32, Landed>,
    /// Keys put, whose commit had not completed, with the namespace each was put under.
    maybe: BTreeMap<u32, Namespace>,
    /// Keys never put.
    never: Vec<u32>,
}

impl Ledger {
    /// The commit of `keys` (each in `maybe`) completed as `committed`: one segment for each
    /// namespace that held one of them, in the order of [`writes`].
    fn land<A: Allocator + Clone>(&mut self, keys: &[u32], committed: &Committed<A>) ->Result<(), String> {
        let mut spaces: Vec<Namespace> = Vec::new_in(Global);
        spaces.extend(writes().into_iter().filter(|it| keys.iter().any(|n| self.maybe.get(n) == Some(it))));
        if spaces.len() != committed.segments.len() {
            return Err(format!("a commit of records under {spaces:?} wrote segments {:x?}: one a namespace, in order", committed.segments));
        }
        for &n in keys {
            let space = self.maybe.remove(&n).ok_or_else(|| format!("record {n} landed and was never put"))?;
            let at = spaces.iter().position(|it| *it == space).expect("its namespace is listed");
            self.committed.insert(n, Landed { segment: committed.segments[at], generation: committed.generation, space });
        }
        Ok(())
    }
}

/// The oracle. `cut`: a power cut happened; `durable`: the writers synced every commit, so a
/// cut may take only what no commit completed (an interrupted flip's traces, reported, are
/// right to report) and every committed record must read back.
fn oracle<A: Allocator + Clone>(store: &Plain<A>, ledger: &Ledger, cut: bool, durable: bool) -> Result<(), String> {
    oracle_with(store, ledger, cut, durable, &BTreeSet::new())
}

/// [`oracle`], a loss counting as reported when `store`'s own report names it or `handed` does:
/// the segments named by the reports earlier opens since the cut handed to their callers (a
/// loss is reported at least once, by some open whose open returned; one whose report dies
/// with its process does not count).
fn oracle_with<A: Allocator + Clone>(store: &Plain<A>, ledger: &Ledger, cut: bool, durable: bool, handed: &BTreeSet<u64>) -> Result<(), String> {
    oracle_since(store, ledger, cut, durable, handed, store.report().whole.unwrap_or(0))
}

/// [`oracle_with`] against `survived`, the generation the cut left ([`survived_on_disk`]; with
/// no cut, the first open's newest whole generation), not the open's own. The promise "a record committed after the generation
/// that survived may be gone with no trace" is about the generations the writers numbered;
/// the openers' own flips number on from what survived (from 1 again when no slot did), so a
/// later open's newest whole generation is not one a writer committed, and only the first
/// open's says what survived the cut.
fn oracle_since<A: Allocator + Clone>(store: &Plain<A>, ledger: &Ledger, cut: bool, durable: bool, handed: &BTreeSet<u64>, survived: u64) -> Result<(), String> {
    let mut lost = named_lost(store);
    lost.extend(handed.iter().copied());
    // Verify once: no segment checked twice by this process, and no read checked anything.
    let mut once = BTreeSet::new();
    if let Some(twice) = store.fully_checked().iter().find(|it| !once.insert(**it)) {
        return Err(format!("segment {twice:016x} was checked in full twice in one open"));
    }
    // Without a power cut nothing committed is lost and no generation is torn; what may be
    // reported is a segment no commit completed (a commit an injected failure stopped before
    // its first flip landed), which the open finds unnamed and says so.
    let named: BTreeSet<u64> = ledger.committed.values().map(|it| it.segment).collect();
    if !cut
        && let Some(it) = store.report().lost.iter().find(|it| it.segment.is_none_or(|it| named.contains(&it)))
    {
        return Err(format!("lost without a power cut: {it:?}"));
    }
    let loss = cut && !durable;
    if !loss && let Some(it) = store.report().lost.iter().find(|it| it.segment.is_some_and(|it| named.contains(&it))) {
        return Err(format!("a committed segment lost with no power cut or in durable mode: {it:?}"));
    }
    // The newest generation the first open since the cut read whole: with no power cut, the
    // newest committed.
    let whole = survived;
    for (&n, &Landed { segment, generation, space }) in &ledger.committed {
        if !loss && generation > whole {
            return Err(format!("committed record {n} of generation {generation}, and with no power cut the open read generation {whole}"));
        }
        match store.get(&key(n)) {
            Some(got) if got.bytes == value(n) && got.tag == TAG && got.namespace == space => {}
            None if lost.contains(&segment) => {}
            // Committed after the generation that survived, with no sync: a cut may leave no
            // trace of it to name.
            None if generation > whole => {}
            other => {
                return Err(format!(
                    "committed record {n} (under {space:?}, segment {segment:016x}, generation {generation}; generation {whole} read whole) reads {other:?}, and no loss names it"
                ));
            }
        }
    }
    for (&n, &space) in &ledger.maybe {
        match store.get(&key(n)) {
            None => {}
            Some(got) if got.bytes == value(n) && got.namespace == space => {}
            other => return Err(format!("record {n}, put under {space:?} and not committed, reads {other:?}")),
        }
    }
    for &n in &ledger.never {
        match store.get(&key(n)) {
            None => {}
            other => return Err(format!("record {n}, never put, reads {other:?}")),
        }
    }
    // A lookup searches only the namespaces it names: a record is never found by one that asks
    // only others, whatever survived.
    let put = ledger.committed.iter().map(|(n, it)| (*n, it.space)).chain(ledger.maybe.iter().map(|(n, it)| (*n, *it)));
    for (n, space) in put {
        for other in reads().into_iter().filter(|it| *it != space) {
            if let Some(got) = store.get_in(&[other], &key(n)) {
                return Err(format!("record {n}, put under {space:?}, is found by a lookup that asks only {other:?}: {got:?}"));
            }
        }
    }
    if store.checks().per_read != 0 {
        return Err(format!("reads made checks: {:?}", store.checks()));
    }
    Ok(())
}

/// Two writers, `rounds` commits each of `per` records spread over the three namespaces,
/// interleaved; the power cut after `crash_after` completions when given; then a reader of a
/// new boot (or of the same, with no cut) and the oracle. `plants`: the two writers' and the
/// reader's.
fn scenario(seed: u64, rounds: u32, per: u32, crash_after: Option<u32>, fail_one_in: u32, plants: [Plant; 3], durable: bool) -> Result<(), String> {
    scenario_traced(seed, rounds, per, crash_after, fail_one_in, plants, durable, None, &mut 0, &mut None)
}

/// [`scenario`], every step written to `trace` when there is one (the same run: the trace
/// draws nothing from the simulation and takes nothing the run holds), and `taken` counting
/// the completions handed to stores, writers' and readers'. `inject`: the first reader's first
/// attempt fails one effect there after its flip's slot lands.
fn scenario_traced(
    seed: u64,
    rounds: u32,
    per: u32,
    crash_after: Option<u32>,
    fail_one_in: u32,
    plants: [Plant; 3],
    durable: bool,
    inject: Option<Place>,
    taken: &mut u32,
    trace: &mut Option<Trace>,
) -> Result<(), String> {
    // A run of its own: every process's store in a frame of this run, given back at its end.
    let root = test_root();
    let mut sim = Sim::new(seed, Faults { fail_one_in, ..Faults::default() });
    if let Some(trace) = trace {
        trace.cast = vec!["writer 0".into(), "writer 1".into()];
    }
    // Their opens' effects are asked, and traced, at the first run's submit.
    let mut actors = vec![Actor::new(cfg(0, 1, durable, &root), &root, plants[0]), Actor::new(cfg(1, 1, durable, &root), &root, plants[1])];
    let mut budget = crash_after;
    let mut ledger = Ledger { never: vec![500_000, 1_500_000], ..Ledger::default() };
    let mut next = [0u32, 1_000_000];
    let mut pending: [Option<Vec<u32>>; 2] = [None, None];
    let mut crashed = false;
    // The generation the cut left, read from the disk ([`survived_on_disk`]).
    let mut survived_cut = None;
    for round in 0..=rounds {
        for (i, actor) in actors.iter_mut().enumerate() {
            if round == rounds || !actor.store.ready() || pending[i].is_some() {
                continue;
            }
            let keys: Vec<u32> = (0..per).map(|k| next[i] + k).collect();
            next[i] += per;
            for &n in &keys {
                actor.store.put(key(n), space(n), TAG, &value(n)).map_err(|it| it.to_string())?;
                ledger.maybe.insert(n, space(n));
            }
            if let Some(trace) = trace {
                let mut put = format!("round {round}: ");
                for &n in &keys {
                    put.push_str(&format!("{n} {:?}, ", space(n)));
                }
                let who = trace.who(i);
                trace.push(Event::Put { who, keys: put });
            }
            let returned = actor.store.commit(&[],sim.wall_ns(), &mut actor.effects);
            if let Some(trace) = trace {
                let who = trace.who(i);
                trace.push(Event::Commit { who, returned: format!("{returned:?}") });
            }
            returned.map_err(|it| it.to_string())?;
            pending[i] = Some(keys);
        }
        let settled = run_traced(&mut sim, &mut actors, &mut budget, taken, trace);
        for (i, actor) in actors.iter_mut().enumerate() {
            let got = actor.store.take_committed();
            if let Some(trace) = trace
                && let Some(got) = &got
            {
                let who = trace.who(i);
                trace.push(Event::Committed { who, outcome: format!("{got:x?} (keys {:?})", pending[i]) });
            }
            if let Some(Ok(committed)) = got {
                let keys = pending[i].take().expect("a commit was pending");
                ledger.land(&keys, &committed)?;
            }
        }
        if !settled {
            if let Some(trace) = trace {
                for (i, actor) in actors.iter().enumerate() {
                    trace.unsubmitted(i, &actor.effects);
                }
            }
            sim.crash();
            if let Some(trace) = trace {
                trace.crash();
                trace.disk(&sim);
            }
            crashed = true;
            survived_cut = Some(survived_on_disk(&sim));
            break;
        }
        if let Some(why) = actors.iter().find_map(|it| it.store.broken()) {
            if !injected(why) {
                return Err(format!("a writer broke on its own: {why}"));
            }
        }
    }
    drop(actors);
    let boot = if crashed { 2 } else { 1 };
    // The first open whose report is handed: an attempt an injected failure stopped (after its
    // flip's slot landed, at `inject`, or anywhere) handed nothing, and the marks its flip
    // wrote make the next attempt report what it found.
    let mut r = reader_traced(&mut sim, 2, boot, plants[2], inject, taken, trace, &root);
    if let Some(trace) = trace {
        let mut ledger_says = String::from("the ledger's committed records (key: segment, generation): ");
        for (n, it) in &ledger.committed {
            ledger_says.push_str(&format!("{n}: {:016x} g{}, ", it.segment, it.generation));
        }
        trace.push(Event::Opened { who: "ledger".into(), says: ledger_says });
    }
    // With no cut, the first open's newest whole generation is the newest committed.
    let survived = survived_cut.unwrap_or_else(|| r.store.report().whole.unwrap_or(0));
    oracle_since(&r.store, &ledger, crashed, durable, &BTreeSet::new(), survived)?;
    let reported = named_lost(&r.store);
    // The reader hands its report, then clears the marks it named with one flip of its own
    // (an injected failure may stop that flip: the marks stay, reported again).
    let cleared = acknowledged(&mut sim, &mut r, taken, trace);
    // Reopened again in the same boot: what the first reader read, byte for byte; no loss
    // reported a second time once the marks were cleared, and with them not cleared, only what
    // the first reader reported (at least once, never a loss it did not name).
    let again = reader_traced(&mut sim, 3, boot, Plant::None, None, taken, trace, &root);
    for &n in ledger.committed.keys().chain(ledger.maybe.keys()) {
        let (a, b) = (r.store.get(&key(n)), again.store.get(&key(n)));
        if a.map(|it| it.bytes) != b.map(|it| it.bytes) {
            return Err(format!("record {n} reads {a:?}, and {b:?} after a reopen"));
        }
    }
    if cleared && !again.store.report().lost.is_empty() {
        return Err(format!("a second open reported again after the marks were cleared: {:?}", again.store.report().lost));
    }
    if let Some(it) = again.store.report().lost.iter().find(|it| it.segment.is_none_or(|id| !reported.contains(&id))) {
        return Err(format!("a second open reported a loss the first did not: {it:?}"));
    }
    oracle_since(&again.store, &ledger, crashed, durable, &reported, survived)?;
    // The first open of this boot recorded its checks: none is made twice in one boot.
    if !again.store.fully_checked().is_empty() {
        return Err(format!("segments checked in full a second time in one boot: {:x?}", again.store.fully_checked()));
    }
    Ok(())
}

const HONEST: [Plant; 3] = [Plant::None, Plant::None, Plant::None];

/// The invariant: two writers interleaved over three namespaces, power cut anywhere, effects
/// failing: every committed record reads back byte for byte under its namespace or is reported
/// lost by name; nothing put and not committed is half there; nothing never put is found; no
/// lookup finds a record of a namespace it did not ask; nothing corrupt is returned.
#[test]
fn two_writers_and_power_cuts_keep_every_committed_record_or_say_so() {
    blaze_sim::check(SimConfig::DEFAULT, two_writers_and_power_cuts);
}

/// Regression (2026-11-13, round 3): a power cut tore the head's first write (then a whole-file
/// write of 4160 bytes) to 3072 bytes, and the reader took the short head for another store's
/// file and stopped. The head is now made at its length and synced at the first open, every
/// flip writes one slot in place, and a short head is read as far as it goes.
///
/// The history is the property test's shrunk case of then (seed 0, two rounds of one record, a power cut
/// after 21 completions, no injected failure), written out step by step; the records are now
/// put under `LOCAL` and `PUBLIC` (keys 0 and 1_000_000), so the history is not the same one
/// completion for completion, and the test asserts only that the cut lands.
#[test]
fn a_head_torn_by_a_power_cut_is_read_through_not_refused() {
    let root = test_root();
    let mut sim = Sim::new(0, Faults::default());
    // Two writers of boot 1 open the empty store: the first to read the head makes it.
    let mut actors = vec![Actor::new(cfg(0, 1, false, &root), &root, Plant::None), Actor::new(cfg(1, 1, false, &root), &root, Plant::None)];
    let mut ledger = Ledger { never: vec![500_000, 1_500_000], ..Ledger::default() };
    let mut budget = Some(21);
    let mut pending: [Option<u32>; 2] = [None, None];
    let mut next = [0u32, 1_000_000];
    let mut crashed = false;
    // Each round, a writer whose open is done and whose last commit completed puts its next
    // record (0, 1, ... and 1_000_000, ...) and commits; then the disk runs until it settles
    // or the 21st completion, the power cut. (Round 0 finds the opens in flight: it runs them.)
    for _round in 0..2u32 {
        for (i, actor) in actors.iter_mut().enumerate() {
            if !actor.store.ready() || pending[i].is_some() {
                continue;
            }
            let n = next[i];
            next[i] += 1;
            actor.store.put(key(n), space(n), TAG, &value(n)).unwrap();
            ledger.maybe.insert(n, space(n));
            actor.store.commit(&[],sim.wall_ns(), &mut actor.effects).unwrap();
            pending[i] = Some(n);
        }
        let settled = run(&mut sim, &mut actors, &mut budget);
        for (i, actor) in actors.iter_mut().enumerate() {
            if let Some(Ok(committed)) = actor.store.take_committed() {
                let n = pending[i].take().expect("a commit was pending");
                ledger.land(&[n], &committed).unwrap();
            }
        }
        if !settled {
            crashed = true;
            break;
        }
    }
    assert!(crashed, "the history ends before its 21st completion: the case no longer cuts it");
    drop(actors);
    sim.crash();
    // Whatever the cut left of the head (short, torn, or whole), a reader of the next boot
    // opens: the head is never taken for another store's file, and the invariant holds.
    let r = reader(&mut sim, 2, 2, &root);
    assert_eq!(r.store.broken(), None);
    let head = sim.contents(&FileName::new("plain/head").unwrap());
    assert!(head.len() <= format::HEAD_LEN, "a head of {} bytes", head.len());
    let outcome = oracle(&r.store, &ledger, true, false);
    assert!(outcome.is_ok(), "{outcome:?}");
}

/// Regression (2026-11-13, round 4; the former engine's case `AXicY2MAAi5GIMGEQjFCKAZUCgAH4wBO`: seed 2, two
/// rounds of one record, no cut, one effect in 40 failing): a writer failed under the flip lock
/// and stopped holding it, so every reader's open that had to flip (to name an orphan) waited
/// on it, asking again every 2 ms, until an injected failure of one ask ended it, 50 times
/// over (the diagnosis read from the code, not from a trace of this case). A failure under
/// the lock now lets it go.
#[test]
fn a_store_that_fails_under_the_flip_lock_lets_it_go() {
    let outcome = scenario(2, 2, 1, None, 40, HONEST, false);
    assert!(outcome.is_ok(), "{outcome:?}");
}

/// Regression (2026-11-13, round 5; the former engine's case
/// `AXicJcaxCQAACAPBiGghOIf7WLv/HAaSIvcJrpw31yhjuAhhYgUeIBEBNg==`: seed 875559, three rounds
/// of five records, a power cut after 82 completions, one effect in 40 failing). The oracle
/// said: "committed record 0 (under LOCAL, generation 1; generation 1 read whole) reads None,
/// and no loss names it". The trace showed the cause: the cut took both slots (nothing was
/// synced), so the reader's first open found the head fresh, reported every segment lost and
/// flipped a fresh generation retiring them; that flip wrote its cache tag, the other slot's
/// blank and its slot at once, the tag write failed (#100) while the slot landed (#101), the
/// open stopped with its report, and the next open read the generation that had retired them
/// and reported none. A flip now writes its slot last, after everything else it writes has
/// succeeded. On failure the test prints the run's trace.
#[test]
fn a_flip_writes_its_slot_last_so_no_open_retires_a_loss_it_did_not_report() {
    let mut trace = Some(Trace::default());
    let outcome = scenario_traced(875_559, 3, 5, Some(82), 40, HONEST, false, None, &mut 0, &mut trace);
    if let Err(why) = outcome {
        let trace = trace.expect("the trace").render();
        panic!("{why}\n\nthe run, traced:\n{trace}\n{why}");
    }
}

/// The invariant: the trace only observes. A traced scenario is the untraced one: for each case
/// (the regression seeds above, and cuts early, late and none, with and without injected
/// failures, plain and durable) both return the same result and hand their stores the same
/// number of completions.
#[test]
fn a_traced_scenario_is_the_untraced_one() {
    let cases: [(u64, u32, u32, Option<u32>, u32, bool); 8] = [
        (875_559, 3, 5, Some(82), 40, false),
        (2, 2, 1, None, 40, false),
        (0, 2, 1, Some(21), 0, false),
        (0, 2, 1, Some(7), 40, false),
        (7, 4, 6, Some(150), 40, false),
        (11, 3, 3, None, 0, false),
        (13, 3, 4, Some(60), 40, true),
        (17, 5, 2, Some(1), 40, false),
    ];
    for (seed, rounds, per, cut, fail, durable) in cases {
        let mut bare = 0;
        let untraced = scenario_traced(seed, rounds, per, cut, fail, HONEST, durable, None, &mut bare, &mut None);
        let mut traced_taken = 0;
        let mut trace = Some(Trace::default());
        let traced = scenario_traced(seed, rounds, per, cut, fail, HONEST, durable, None, &mut traced_taken, &mut trace);
        let case = format!("seed {seed}, {rounds} rounds of {per}, cut {cut:?}, fail 1 in {fail}, durable {durable}");
        assert_eq!(untraced, traced, "{case}: the trace changed the outcome");
        assert_eq!(bare, traced_taken, "{case}: the trace changed the completions handed to stores");
        assert!(trace.is_some_and(|it| it.completions >= traced_taken), "{case}: the trace saw fewer completions than were handed over");
    }
}

/// The same small history (seed 0, two rounds of one record), cut at every completion, with
/// and without injected failures.
#[test]
fn a_small_history_cut_at_every_completion_keeps_the_invariant() {
    for fail in [0, 40] {
        for after in 0..400 {
            let outcome = scenario(0, 2, 1, Some(after), fail, HONEST, false);
            assert!(outcome.is_ok(), "cut after {after}, fail 1 in {fail}: {outcome:?}");
        }
    }
}

fn two_writers_and_power_cuts(tape: &Tape) {
    let seed = tape.next_u64("sim seed");
    let rounds = tape.int_in(1..=6u32, "rounds");
    let per = tape.int_in(1..=24u32, "records per round");
    let cut = tape.int_in(0..=1u32, "power cut") == 1;
    let after = tape.int_in(0..=400u32, "cut after");
    let fail = if tape.int_in(0..=3u32, "failing effects") == 0 { 40 } else { 0 };
    // The reader's first open fails one effect after its flip's slot lands (when it flips and
    // asks one there): the loss it found must still reach some open's report.
    let inject = [None, Some(Place::Remove), Some(Place::Map), Some(Place::HeadSync)][tape.index(4, "inject at")];
    let outcome = scenario_traced(seed, rounds, per, cut.then_some(after), fail, HONEST, false, inject, &mut 0, &mut None);
    assert!(outcome.is_ok(), "seed {seed}, {rounds} rounds of {per}, cut {cut} after {after}, fail 1 in {fail}, injected at {inject:?}: {outcome:?}");
}

/// Durable mode: a power cut loses nothing a completed commit wrote.
#[test]
fn durable_commits_survive_any_power_cut() {
    blaze_sim::check(SimConfig::DEFAULT, |tape| {
        let seed = tape.next_u64("sim seed");
        let after = tape.int_in(0..=400u32, "cut after");
        let outcome = scenario(seed, 4, 8, Some(after), 0, HONEST, true);
        assert!(outcome.is_ok(), "seed {seed}, cut after {after}: {outcome:?}");
    });
}

/// The oracle fails for some seed of `seeds` with `plants`, and passes for all without.
fn caught(seeds: u64, plants: [Plant; 3], durable: bool, cut: impl Fn(u64) -> Option<u32>) {
    let mut caught = None;
    for seed in 0..seeds {
        let honest = scenario(seed, 3, 6, cut(seed), 0, HONEST, durable);
        assert!(honest.is_ok(), "seed {seed} without the plant: {honest:?}");
        if caught.is_none()
            && let Err(why) = scenario(seed, 3, 6, cut(seed), 0, plants, durable)
        {
            caught = Some(why);
        }
    }
    assert!(caught.is_some(), "no seed of {seeds} caught {plants:?}");
}

/// [`caught`], a planted run also counting as caught when an open breaks on its own (the
/// harness's readers stop the run with a panic naming it: "a reader broke on its own").
fn caught_or_broke(seeds: u64, plants: [Plant; 3], durable: bool, cut: impl Fn(u64) -> Option<u32>) {
    let mut caught = None;
    for seed in 0..seeds {
        let honest = scenario(seed, 3, 6, cut(seed), 0, HONEST, durable);
        assert!(honest.is_ok(), "seed {seed} without the plant: {honest:?}");
        if caught.is_some() {
            continue;
        }
        let at = cut(seed);
        let planted = std::panic::catch_unwind(move || scenario(seed, 3, 6, at, 0, plants, durable));
        caught = match planted {
            Ok(Ok(())) => None,
            Ok(Err(why)) => Some(why),
            Err(panic) => Some(panic.downcast_ref::<String>().cloned().unwrap_or_else(|| "a panic".into())),
        };
    }
    assert!(caught.is_some(), "no seed of {seeds} caught {plants:?}");
}

/// The control of durable mode's directory sync: a file's sync does not make its name
/// durable, so a flip whose slot is written before the store's directory is synced can leave,
/// after a power cut, a durable generation naming a segment or a log that is gone (seen
/// 2026-11-16 once blaze-core's simulation kept a name only after its directory's sync: "a
/// committed segment lost with no power cut or in durable mode", and "writing a manifest log
/// .. failed: plain/manifests-..: no such file").
#[test]
fn the_oracle_catches_a_skipped_directory_sync() {
    caught_or_broke(64, [Plant::SkipDirSync, Plant::SkipDirSync, Plant::None], true, |seed| Some(40 + (seed as u32 * 7) % 200));
}

/// The control of the lost log: after a power cut took the log the newest slot names (no
/// generation's manifest read back), the next flip starts a new log; the plant appends to the
/// lost one, and an open breaks on "no such file" (2026-11-16, seed 13 of the traced cases).
#[test]
fn the_oracle_catches_an_append_to_a_lost_log() {
    let plants = [Plant::AppendToLostLog, Plant::AppendToLostLog, Plant::AppendToLostLog];
    caught_or_broke(64, plants, false, |seed| Some(40 + (seed as u32 * 7) % 200));
}

#[test]
fn the_oracle_catches_a_skipped_segment_sync() {
    caught(64, [Plant::SkipDataSync, Plant::SkipDataSync, Plant::None], true, |seed| Some(40 + (seed as u32 * 7) % 200));
}

/// The control of the namespace rule: a reader whose lookup searches every namespace, not only
/// those it names, is caught by the oracle, with no power cut and across cuts (torn flips
/// among them).
#[test]
fn the_oracle_catches_a_lookup_that_ignores_the_namespace() {
    let plants = [Plant::None, Plant::None, Plant::IgnoresNamespace];
    caught(4, plants, false, |_| None);
    caught(16, plants, false, |seed| Some(40 + (seed as u32 * 13) % 300));
}

/// The control of durable mode's order (the segment synced before the flip that names it). The
/// plant flips first and syncs the segment after, so its bug lives only in the window between
/// the commit's report and that late sync: a drawn cut almost never lands there, so the cut is
/// placed in it, at the first completion after the commit is reported, before anything else.
/// In the mode with no sync the plant changes nothing (no sync is on that path at all, and a
/// cut may take an unsynced commit by design): the control is for durable mode alone.
#[test]
fn the_oracle_catches_a_flip_before_the_sync() {
    let cut_as_the_commit_is_told = |seed: u64, plant: Plant| -> Result<(), String> {
        // A run of its own, its stores in a frame of this run.
        let root = test_root();
        let mut sim = Sim::new(seed, Faults::default());
        let mut w = [Actor::new(cfg(0, 1, true, &root), &root, plant)];
        settle(&mut sim, &mut w);
        let mut ledger = Ledger::default();
        let keys: Vec<u32> = (0..6).collect();
        for &n in &keys {
            w[0].store.put(key(n), space(n), TAG, &value(n)).unwrap();
            ledger.maybe.insert(n, space(n));
        }
        let effects = &mut w[0].effects;
        w[0].store.commit(&[],sim.wall_ns(), effects).unwrap();
        let mut told = false;
        let mut done = Vec::new_in(Global);
        'run: for _ in 0..100_000 {
            sim.submit(&mut w[0].effects);
            assert!(sim.in_flight() > 0, "the commit settled without being told");
            sim.poll(&mut done, Deadline::after(sim.now(), Duration::from_secs(1)));
            let wall = sim.wall_ns();
            for completion in done.drain(..) {
                w[0].store.complete(completion, wall, &mut w[0].effects);
                if let Some(committed) = w[0].store.take_committed() {
                    ledger.land(&keys, &committed.expect("the commit completes"))?;
                    told = true;
                    // The power cut, now: what the commit asked after its report (the plant's
                    // late sync) has not completed.
                    break 'run;
                }
            }
        }
        assert!(told, "the commit was never told");
        drop(w);
        sim.crash();
        let r = reader(&mut sim, 1, 2, &root);
        oracle(&r.store, &ledger, true, true)
    };
    let mut caught = None;
    for seed in 0..16 {
        let honest = cut_as_the_commit_is_told(seed, Plant::None);
        assert!(honest.is_ok(), "seed {seed} without the plant: {honest:?}");
        if caught.is_none() {
            caught = cut_as_the_commit_is_told(seed, Plant::FlipBeforeSync).err();
        }
    }
    assert!(caught.is_some(), "no seed of 16 caught a flip before the segment's sync in durable mode");
}

#[test]
fn the_oracle_catches_an_index_off_by_one() {
    let outcome = scenario(1, 2, 4, None, 0, [Plant::IndexOffByOne, Plant::None, Plant::None], false);
    assert!(outcome.is_err(), "an index one unit off went unseen");
}

#[test]
fn the_oracle_catches_a_flip_on_a_stale_base() {
    caught(16, [Plant::StaleBase, Plant::StaleBase, Plant::None], false, |_| None);
}

/// Two writers each commit while the other does: both commits land, and the flip lock is
/// contended (the second waits on its timer, bounded).
#[test]
fn two_writers_commit_at_once() {
    let root = test_root();
    let mut sim = Sim::new(7, Faults::default());
    let mut actors = vec![Actor::new(cfg(0, 1, false, &root), &root, Plant::None), Actor::new(cfg(1, 1, false, &root), &root, Plant::None)];
    settle(&mut sim, &mut actors);
    for round in 0..8u32 {
        for (i, actor) in actors.iter_mut().enumerate() {
            let n = round * 2 + i as u32;
            actor.store.put(key(n), space(n), TAG, &value(n)).unwrap();
            actor.store.commit(&[],sim.wall_ns(), &mut actor.effects).unwrap();
        }
        settle(&mut sim, &mut actors);
        for actor in &mut actors {
            assert!(matches!(actor.store.take_committed(), Some(Ok(_))));
        }
    }
    let r = reader(&mut sim, 2, 1, &root);
    // Generation 0 is the empty store (no slot written); each commit's flip makes the next.
    assert_eq!(r.store.generation(), 16, "16 commits by two writers, each flipped once on the one before");
    for n in 0..16 {
        assert_eq!(r.store.get(&key(n)).map(|it| it.bytes.to_vec()), Some(value(n)));
    }
}

/// A store of one writer that has committed `keys` under `LOCAL` in one commit, durably; the
/// commit's one segment and its ledger.
fn one_local_commit(sim: &mut Sim, keys: std::ops::Range<u32>, root: &Frame) -> (u64, Ledger) {
    let mut w = [Actor::new(cfg(0, 1, true, root), root, Plant::None)];
    settle(sim, &mut w);
    let mut ledger = Ledger::default();
    let segment = commit_local(sim, &mut w, keys, &mut ledger);
    (segment, ledger)
}

/// `w`'s commit of `keys` under `LOCAL`, run to its end and entered in `ledger`: its one segment.
fn commit_local(sim: &mut Sim, w: &mut [Actor<'_>], keys: std::ops::Range<u32>, ledger: &mut Ledger) -> u64 {
    let keys: Vec<u32> = keys.collect();
    for &n in &keys {
        w[0].store.put(key(n), Namespace::LOCAL, TAG, &value(n)).unwrap();
        ledger.maybe.insert(n, Namespace::LOCAL);
    }
    let effects = &mut w[0].effects;
    w[0].store.commit(&[],sim.wall_ns(), effects).unwrap();
    settle(sim, w);
    let committed = w[0].store.take_committed().unwrap().unwrap();
    ledger.land(&keys, &committed).unwrap();
    assert_eq!(committed.segments.len(), 1, "one namespace, one segment");
    committed.segments[0]
}

/// Three bytes of `segment`'s first record changed on the disk: what a power cut that did not
/// let the kernel write it back leaves, found by the full check of the next boot's open.
fn rot_segment(sim: &mut Sim, segment: u64) {
    let file = FileName::new(&format!("plain/seg-{segment:016x}")).unwrap();
    sim.rot(&file, HEADER as u64 + 3, &[0xff, 0x00, 0xff]);
}

/// Runs `actor` alone until it asks the flip lock and nothing of its own is in flight; the ask
/// is kept, not submitted, so another process can commit before this one's flip.
fn run_until_it_asks_the_lock(sim: &mut Sim, actor: &mut Actor<'_>) {
    let mut done = Vec::new_in(Global);
    for _ in 0..100_000 {
        let asks = actor.effects.iter().any(|it| matches!(it, Effect::Lock { .. }));
        if asks && sim.in_flight() == 0 {
            return;
        }
        if !asks {
            actor.asking();
            sim.submit(&mut actor.effects);
        }
        assert!(sim.in_flight() > 0, "the open settled without asking the flip lock");
        sim.poll(&mut done, Deadline::after(sim.now(), Duration::from_secs(1)));
        let wall = sim.wall_ns();
        for mut completion in done.drain(..) {
            actor.injected(&mut completion);
            actor.store.complete(completion, wall, &mut actor.effects);
        }
    }
    panic!("the open did not ask the flip lock in 100000 polls");
}

/// One history of the class rule 54 names: an open finds a committed `LOCAL` segment lost,
/// its flip retires it and lands its slot, and then one effect fails at `place`, so the open
/// stops and its report dies with it (the reader opening with `plant`). Then a reader of the
/// same boot opens, and the oracle asks of it what it asks after any cut: every committed
/// record reads back or is named lost, by its report or by one an earlier open handed.
/// Returns the oracle's verdict, and whether the failure fired (else the history is not the
/// one it says).
///
/// - [`Place::Remove`]: two segments; the first open of boot 2 loses the first (reported and
///   cleared), the grace period passes, and the open of boot 3 loses the second; its flip
///   retires the second and drops the first's row past its grace, and the unlink fails.
/// - [`Place::Map`]: the open of boot 2 loses the one segment and asks the lock; before it has
///   it, the writer (alive) commits a second segment; the open's flip builds on that
///   generation, and mapping the new segment fails.
/// - [`Place::HeadSync`]: the open of boot 2, durable, loses the one segment, and the head's
///   sync after its slot fails.
fn lost_after_the_slot(place: Place, plant: Plant) -> (Result<(), String>, bool) {
    // A run of its own, its stores in a frame of this run.
    let root = test_root();
    let mut sim = Sim::new(31, Faults::default());
    let mut ledger = Ledger::default();
    let mut w = [Actor::new(cfg(0, 1, true, &root), &root, Plant::None)];
    settle(&mut sim, &mut w);
    let first = commit_local(&mut sim, &mut w, 0..6, &mut ledger);
    let mut handed = BTreeSet::new();
    let (fired, r2) = match place {
        Place::Remove => {
            let second = commit_local(&mut sim, &mut w, 6..12, &mut ledger);
            drop(w);
            sim.crash();
            rot_segment(&mut sim, first);
            let earlier = reader(&mut sim, 1, 2, &root);
            assert_eq!(named_lost(&earlier.store), BTreeSet::from([first]), "{:?}", earlier.store.report());
            handed.extend(named_lost(&earlier.store));
            drop(earlier);
            wait(&mut sim, Duration::from_nanos(super::GRACE_NS + 1_000_000_000));
            rot_segment(&mut sim, second);
            let mut r = [Actor::injecting(cfg(2, 3, false, &root), &root, plant, Some(place))];
            settle(&mut sim, &mut r);
            let [r] = r;
            assert!(r.store.broken().is_some_and(injected), "{place:?}: {:?}", r.store.broken());
            (r.fired, reader(&mut sim, 3, 3, &root))
        }
        Place::Map => {
            // No cut: the writer lives on (its commits are durable, so the rot alone stands for
            // what a cut did not let reach the disk), and the reader is of a later boot.
            rot_segment(&mut sim, first);
            let mut r = Actor::injecting(cfg(1, 2, false, &root), &root, plant, Some(place));
            run_until_it_asks_the_lock(&mut sim, &mut r);
            commit_local(&mut sim, &mut w, 6..12, &mut ledger);
            settle(&mut sim, std::slice::from_mut(&mut r));
            assert!(r.store.broken().is_some_and(injected), "{place:?}: {:?}", r.store.broken());
            drop(w);
            (r.fired, reader(&mut sim, 2, 2, &root))
        }
        Place::HeadSync => {
            drop(w);
            sim.crash();
            rot_segment(&mut sim, first);
            let mut r = [Actor::injecting(cfg(1, 2, true, &root), &root, plant, Some(place))];
            settle(&mut sim, &mut r);
            let [r] = r;
            assert!(r.store.broken().is_some_and(injected), "{place:?}: {:?}", r.store.broken());
            (r.fired, reader(&mut sim, 2, 2, &root))
        }
    };
    (oracle_with(&r2.store, &ledger, true, false, &handed), fired)
}

/// The invariant (the class of 2026-11-13's seed 875559, closed): a loss an open found is on
/// the disk in the same manifest that retires it ("lost, not yet reported", the retired row's
/// mark), so whatever fails after that manifest's slot lands (the unlink of a file past its
/// grace, mapping a segment another process committed, the head's sync in durable mode) and
/// the open stops with its report, the next open reports it from the mark. Reported at least
/// once, never zero times.
#[test]
fn a_loss_an_open_retires_is_reported_whatever_fails_after_its_slot_lands() {
    for place in [Place::Remove, Place::Map, Place::HeadSync] {
        let (outcome, fired) = lost_after_the_slot(place, Plant::None);
        assert!(fired, "{place:?}: the failure after the slot never fired");
        assert!(outcome.is_ok(), "{place:?}: {outcome:?}");
    }
}

/// The control: the mark cleared in the flip that sets it (the retirement on disk, the report
/// only in the stopped process's memory) is caught by the oracle at each of the three places.
#[test]
fn the_oracle_catches_a_mark_cleared_in_the_flip_that_sets_it() {
    for place in [Place::Remove, Place::Map, Place::HeadSync] {
        let (outcome, fired) = lost_after_the_slot(place, Plant::ClearMarkAtOnce);
        assert!(fired, "{place:?}: the failure after the slot never fired");
        assert!(outcome.is_err(), "{place:?}: a mark cleared in the flip that set it went unseen");
    }
}

/// At least once: an open whose report was handed and that made no flip after it (its process
/// exited) leaves the mark, and the next open reports the loss again, by name, with its count;
/// once a flip of a process that handed the report clears it, no open reports it again.
#[test]
fn a_loss_is_reported_until_a_flip_after_its_report_clears_it() {
    let root = test_root();
    let mut sim = Sim::new(33, Faults::default());
    let (segment, ledger) = one_local_commit(&mut sim, 0..6, &root);
    sim.crash();
    rot_segment(&mut sim, segment);
    let file = FileName::new(&format!("plain/seg-{segment:016x}")).unwrap();
    let first = reader_with(&mut sim, 1, 2, Plant::None, &root);
    let named = |it: &Actor<'_>| it.store.report().lost.iter().map(|it| (it.file.clone(), it.segment, it.records)).collect::<Vec<_>>();
    assert_eq!(named(&first), vec![(file.clone(), Some(segment), Some(6))]);
    drop(first);
    let mut second = reader_with(&mut sim, 2, 2, Plant::None, &root);
    assert_eq!(named(&second), vec![(file, Some(segment), Some(6))], "the mark the first open's flip wrote reports it again");
    oracle(&second.store, &ledger, true, false).unwrap();
    assert!(acknowledged(&mut sim, &mut second, &mut 0, &mut None));
    let third = reader_with(&mut sim, 3, 2, Plant::None, &root);
    assert!(third.store.report().lost.is_empty(), "reported after its mark was cleared: {:?}", third.store.report());
    oracle_with(&third.store, &ledger, true, false, &BTreeSet::from([segment])).unwrap();
}

/// A segment of an earlier boot whose bytes did not survive (here: rotted after a durable
/// commit) is checked at the first open of the new boot, dropped and reported by name with its
/// count; the plant that trusts it returns a corrupt record instead.
#[test]
fn the_oracle_catches_a_segment_of_an_earlier_boot_read_unchecked() {
    let root = test_root();
    let outcome = |plant: Plant| {
        let mut sim = Sim::new(3, Faults::default());
        let (segment, ledger) = one_local_commit(&mut sim, 0..10, &root);
        sim.crash();
        let file = FileName::new(&format!("plain/seg-{segment:016x}")).unwrap();
        sim.rot(&file, HEADER as u64 + 3, &[0xff, 0x00, 0xff]);
        let mut r = [Actor::new(cfg(1, 2, false, &root), &root, plant)];
        settle(&mut sim, &mut r);
        let report = r[0].store.report().clone();
        // The rot stands for bytes a power cut did not let reach the disk: a loss to report.
        (oracle(&r[0].store, &ledger, true, false), report, file)
    };
    let (honest, report, file) = outcome(Plant::None);
    assert!(honest.is_ok(), "{honest:?}");
    assert_eq!(report.lost.len(), 1, "{report:?}");
    assert_eq!((report.lost[0].file.clone(), report.lost[0].records), (file, Some(10)));
    let (planted, ..) = outcome(Plant::TrustBoot);
    assert!(planted.is_err(), "a segment of an earlier boot was read unchecked and nothing saw it");
}

/// Verify once (decision 18): a record whose bytes rot on the disk after its segment was proved
/// (here, in the boot that wrote it) is not looked at by a read, which checks nothing; the
/// explicit scrub finds it, naming its key and segment; and in the next boot the segment's one
/// full check drops it and reports it by name.
#[test]
fn a_byte_rotted_after_its_proof_is_found_by_scrub_or_the_next_boot_not_by_a_read() {
    let root = test_root();
    let mut sim = Sim::new(5, Faults::default());
    let (segment, _) = one_local_commit(&mut sim, 9..10, &root);
    let file = FileName::new(&format!("plain/seg-{segment:016x}")).unwrap();
    sim.rot(&file, HEADER as u64, &[!value(9)[0]]);
    let r = reader(&mut sim, 1, 1, &root);
    let read = r.store.get(&key(9)).map(|it| it.bytes.to_vec());
    assert_ne!(read, Some(value(9)), "the read returned the bytes as they lie, unchecked");
    assert_eq!(r.store.checks(), super::Checks { full: 0, reads: 1, per_read: 0 });
    let found = r.store.scrub();
    assert_eq!(found.iter().map(|it| (it.key, it.pack.clone())).collect::<Vec<_>>(), vec![(key(9), file.clone())]);
    // The next boot proves the segment once, from its digest, and drops it by name.
    let next = reader(&mut sim, 2, 2, &root);
    assert_eq!(next.store.checks().full, 1);
    assert_eq!(next.store.report().lost.iter().map(|it| (it.file.clone(), it.records)).collect::<Vec<_>>(), vec![(file, Some(1))]);
    assert_eq!(next.store.get(&key(9)).map(|it| it.bytes.len()), None);
}

/// The control for "verify once": an open that checks a segment of an earlier boot and does not
/// record it makes the next open of the same boot check it again, and the check the oracle
/// makes (no segment fully checked in a second open of one boot) sees it.
#[test]
fn the_oracle_catches_a_full_check_made_twice_in_one_boot() {
    let root = test_root();
    let second_open_checks = |plant: Plant| {
        let mut sim = Sim::new(8, Faults::default());
        one_local_commit(&mut sim, 0..6, &root);
        sim.crash();
        let mut first = [Actor::new(cfg(1, 2, false, &root), &root, plant)];
        settle(&mut sim, &mut first);
        assert_eq!(first[0].store.checks().full, 1);
        let second = reader(&mut sim, 2, 2, &root);
        second.store.fully_checked().len()
    };
    assert_eq!(second_open_checks(Plant::None), 0, "an honest open records its check");
    assert_eq!(second_open_checks(Plant::ForgetCheck), 1, "the planted bug went unseen");
}

/// The files of `dir`, by name, with their bytes.
fn files(sim: &mut Sim) -> BTreeMap<String, Vec<u8>> {
    let Done::Listed(listed) = ask(sim, Effect::List { dir: dir().into(), tag: EffectTag(OWN + 1) }) else { panic!("a listing") };
    listed.into_iter().filter(|(_, meta)| !meta.dir).map(|(name, _)| (name.as_str().to_owned(), sim.contents(&name))).collect()
}

fn build(seed: u64, root: &Frame) -> (BTreeMap<String, Vec<u8>>, Sim) {
    let mut sim = Sim::new(seed, Faults::default());
    let mut w = [Actor::new(cfg(0, 1, false, root), root, Plant::None)];
    settle(&mut sim, &mut w);
    for round in 0..5u32 {
        for n in round * 10..round * 10 + 10 {
            w[0].store.put(key(n), space(n), TAG, &value(n)).unwrap();
        }
        let effects = &mut w[0].effects;
        w[0].store.commit(&[],sim.wall_ns(), effects).unwrap();
        settle(&mut sim, &mut w);
        assert!(matches!(w[0].store.take_committed(), Some(Ok(_))));
    }
    (files(&mut sim), sim)
}

/// The persisted round trip: two builds of the same puts write the same bytes, file for file;
/// a reopen reads every record back byte for byte, under its namespace.
#[test]
fn the_same_puts_write_the_same_bytes_and_read_back() {
    let root = test_root();
    let (a, mut sim) = build(11, &root);
    let (b, _) = build(11, &root);
    assert_eq!(a, b, "two builds of one history wrote different bytes");
    assert!(a.keys().any(|it| it.ends_with("CACHEDIR.TAG")), "{:?}", a.keys());
    let r = reader(&mut sim, 1, 1, &root);
    for n in 0..50 {
        let got = r.store.get(&key(n));
        assert_eq!(got.map(|it| (it.bytes.to_vec(), it.namespace)), Some((value(n), space(n))), "record {n}");
    }
    // Every segment is self-describing: its own check passes on its bytes as they lie, and it
    // holds one namespace.
    let mut spaces = BTreeSet::new();
    for (name, bytes) in &a {
        if name.contains("/seg-") {
            let described = check_segment(bytes).unwrap_or_else(|why| panic!("{name}: {why}"));
            spaces.insert(described.namespace.0);
        }
    }
    assert_eq!(spaces.len(), 3, "a segment for each namespace written");
}

/// The head cut at every length from 0 to 4160 after three commits (generation 1 in slot 1,
/// 2 in slot 0, 3 in slot 1), each of records under all three namespaces (so three segments a
/// commit): never refused; a whole slot 0 (64 bytes or more) reads generation 2, every record
/// of generations 1 and 2 back under its namespace and in no other, and generation 3's
/// segments reported lost by name with the torn slot; shorter, no generation survives and every
/// segment is read from its footer and reported lost; the whole head reads generation 3 and
/// loses nothing. A second open reports nothing again.
#[test]
fn a_head_cut_at_any_length_reads_the_slot_before_or_the_orphans_and_says_so() {
    let root = test_root();
    let mut sim = Sim::new(21, Faults::default());
    let mut w = [Actor::new(cfg(0, 1, false, &root), &root, Plant::None)];
    settle(&mut sim, &mut w);
    let mut ledger = Ledger::default();
    let mut rounds: Vec<Vec<u64, &Frame>, &Frame> = Vec::new_in(&root);
    for round in 0..3u32 {
        let keys: Vec<u32> = (round * 4..round * 4 + 4).collect();
        for &n in &keys {
            w[0].store.put(key(n), space(n), TAG, &value(n)).unwrap();
            ledger.maybe.insert(n, space(n));
        }
        let effects = &mut w[0].effects;
        w[0].store.commit(&[],sim.wall_ns(), effects).unwrap();
        settle(&mut sim, &mut w);
        let committed = w[0].store.take_committed().unwrap().unwrap();
        assert_eq!(committed.generation, u64::from(round) + 1);
        assert_eq!(committed.segments.len(), 3, "four records over three namespaces");
        ledger.land(&keys, &committed).unwrap();
        rounds.push(committed.segments.clone());
    }
    let saved = files(&mut sim);
    let head = saved["plain/head"].clone();
    assert_eq!(head.len(), format::HEAD_LEN);
    for cut in 0..=format::HEAD_LEN {
        let mut sim = Sim::new(22, Faults::default());
        for (name, bytes) in &saved {
            let bytes = if name == "plain/head" { head[..cut].to_vec() } else { bytes.clone() };
            let file = FileName::new(name).unwrap();
            assert!(matches!(ask(&mut sim, Effect::Write { file, bytes, tag: EffectTag(OWN) }), Done::Written));
        }
        // Each cut's two readers in a frame of that cut, given back before the next.
        let cut_root = test_root();
        let r = reader(&mut sim, 1, 2, &cut_root);
        let report = r.store.report().clone();
        let lost: BTreeSet<u64> = report.lost.iter().filter_map(|it| it.segment).collect();
        oracle(&r.store, &ledger, true, false).unwrap_or_else(|why| panic!("head cut to {cut}: {why}"));
        let (whole, gone): (Option<u64>, &[Vec<u64, &Frame>]) = match cut {
            format::HEAD_LEN => (Some(3), &[]),
            c if c >= format::SLOT => (Some(2), &rounds[2..]),
            _ => (None, &rounds[..]),
        };
        assert_eq!(report.whole, whole, "head cut to {cut}: {report:?}");
        for id in gone.iter().flatten() {
            assert!(lost.contains(id), "head cut to {cut}: segment {id:016x} gone and not reported: {report:?}");
        }
        if cut < format::HEAD_LEN && cut > 0 {
            assert!(report.lost.iter().any(|it| it.file.as_str() == "plain/head"), "head cut to {cut}: the torn slot not reported: {report:?}");
        }
        if cut == format::HEAD_LEN {
            assert!(report.lost.is_empty(), "the whole head lost something: {report:?}");
        }
        for (&n, landed) in &ledger.committed {
            if whole.is_some_and(|it| landed.generation <= it) {
                assert_eq!(r.store.get(&key(n)).map(|it| it.bytes.to_vec()), Some(value(n)), "head cut to {cut}: record {n}");
            }
        }
        let again = reader(&mut sim, 2, 2, &cut_root);
        assert!(again.store.report().lost.is_empty(), "head cut to {cut}: reported again: {:?}", again.store.report());
        assert!(again.store.fully_checked().is_empty(), "head cut to {cut}: checked twice in one boot");
    }
}

/// A file of a previous layout is refused naming it, never read as empty or overwritten (rule
/// 26): the content-addressed store's `head`; a plain head whose slot is of format 2 (before
/// retired rows were marked lost), 1 (entries holding a kind's byte, before blaze-keyed's
/// `Tag`) or 0; a manifest of format 2; and a segment of format 1, each refused as
/// [`StoreError::Format`] naming what it is, its format and the one read. A manifest with
/// marked rows round-trips.
#[test]
fn a_previous_layout_is_refused_by_name() {
    let root = test_root();
    let refused = |head: Vec<u8>| {
        let mut sim = Sim::new(1, Faults::default());
        let file = FileName::new("plain/head").unwrap();
        assert!(matches!(ask(&mut sim, Effect::Write { file: file.clone(), bytes: head.clone(), tag: EffectTag(OWN) }), Done::Written));
        let mut r = [Actor::new(cfg(0, 1, false, &root), &root, Plant::None)];
        settle(&mut sim, &mut r);
        let why = r[0].store.broken().cloned().expect("refused");
        assert_eq!(sim.contents(&file), head, "a refused head is left as it was");
        why
    };
    let old = refused(b"BLZHEAD\x03 a content-addressed store's head".to_vec());
    assert_eq!(old, StoreError::Head { what: format::HEAD_FOREIGN, at: format::SLOT_AT[0] });
    for (format, at) in [(4u32, 0usize), (4, 1), (3, 0), (3, 1), (2, 0), (2, 1), (1, 0), (1, 1), (0, 0)] {
        let slot = Slot { generation: 2 + at as u64, log: 7, manifest_at: 0, manifest_len: 64, manifest_crc: 0 }.encode_as(format);
        let mut head = vec![0u8; format::HEAD_LEN];
        let start = format::SLOT_AT[at] as usize;
        head[start..start + format::SLOT].copy_from_slice(&slot);
        assert_eq!(refused(head), StoreError::Format { what: format::HEAD_SLOT, found: format, wanted: FORMAT }, "slot {at} of format {format}");
    }

    // Manifests of formats 2, 3 and 4 (3: before the sealed flag; 4: before root expiry and
    // leases): this format's bytes with only the format word changed.
    for old in [2u32, 3, 4] {
        let mut manifest = format::Manifest::default().encode(SEGMENT_MOST).expect("an empty manifest encodes");
        manifest[4..8].copy_from_slice(&old.to_le_bytes());
        assert_eq!(format::Manifest::decode(&manifest, SEGMENT_MOST), Err(StoreError::Format { what: "a plain store's manifest", found: old, wanted: FORMAT }));
    }
    // This format's retired rows, marked and not, decode to what was encoded.
    let marked = format::Manifest {
        generation: 4,
        segments: Vec::new_in(Global),
        roots: vec![
            format::Root { name: "kept".to_owned(), key: key(1).0, generation: 3, expires_ns: format::NEVER },
            format::Root { name: "a checkout's answer".to_owned(), key: key(2).0, generation: 4, expires_ns: 99 },
        ],
        leases: vec![format::Lease { holder: 7, since: 4, opened_ns: 5, expires_ns: 6 }],
        retired: vec![
            format::Retired { id: 1, log: false, since_ns: 10, lost: None },
            format::Retired { id: 2, log: true, since_ns: 11, lost: None },
            format::Retired { id: 3, log: false, since_ns: 12, lost: Some(format::LostMark { kind: format::LostKind::NotWhole, records: Some(9) }) },
            format::Retired { id: 4, log: false, since_ns: 13, lost: Some(format::LostMark { kind: format::LostKind::Orphan, records: None }) },
            format::Retired { id: 5, log: false, since_ns: 14, lost: Some(format::LostMark { kind: format::LostKind::Gone, records: Some(0) }) },
        ],
    };
    assert_eq!(format::Manifest::decode(&marked.encode(SEGMENT_MOST).expect("rows of no segment encode"), SEGMENT_MOST), Ok(marked));

    // A segment of format 1: the bytes of a whole segment of this format with only its format
    // word changed (its digest then fails too; the format is named first).
    let mut bytes = Vec::new_in(Global);
    seal_segment(Namespace::LOCAL, &[(RecordRef { tag: TAG, key: key(1).0 }, &value(1)[..])], &mut bytes).unwrap();
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(check_segment(&bytes), Err(StoreError::Format { what: "a plain store's segment", found: 1, wanted: SEGMENT_FORMAT }));
}

/// D160: the store's folder is named by every form of its files. A `pub const ..FORMAT: u32` of
/// format.rs (the one home of the store's files' forms) not entered in [`format::FORMS`] fails
/// here, so a bump of any of them moves the folder. The name round-trips, and names of other
/// shapes are no store's.
#[test]
fn every_store_format_names_the_place() {
    let source = include_str!("format.rs");
    let declared: Vec<&str> = source
        .lines()
        .filter_map(|line| line.split_once("const ").map(|(_, rest)| rest))
        .filter_map(|rest| rest.split_once(": u32 = "))
        .map(|(name, _)| name)
        .filter(|name| name.ends_with("FORMAT"))
        .collect();
    assert_eq!(declared, ["FORMAT", "SEGMENT_FORMAT"], "a format constant of the store's files is declared: enter it in FORMS (D160)");
    assert_eq!(format::FORMS.map(|it| it.1), [FORMAT, SEGMENT_FORMAT]);
    let this = format::PlaceName::THIS;
    assert_eq!(this.as_str(), format!("f{FORMAT}s{SEGMENT_FORMAT}"));
    assert_eq!(format::PlaceName::parse(this.as_str()), Some(format::FORMS));
    assert_eq!(format::PlaceName::of([(b'f', 12), (b's', 3)]).as_str(), "f12s3");
    assert_eq!(format::PlaceName::parse("f12s3"), Some([(b'f', 12), (b's', 3)]));
    for not in ["", "f5", "s2f5", "f05s2", "f5s2x", "f5s", "fs2", "seg-0000000000000001", "head", "f99999999999s2"] {
        assert_eq!(format::PlaceName::parse(not), None, "{not:?} read as a store's folder");
    }
    for leaf in ["head", "flip", "bloom", "seg-0000000000000001", "manifests-00000000000000ff"] {
        assert!(super::store_leaf(leaf), "{leaf}");
    }
    for leaf in ["CACHEDIR.TAG", "f5s2", "seg-1", "notes"] {
        assert!(!super::store_leaf(leaf), "{leaf}");
    }
}

/// The invariant: [`check_segment`] accepts what [`seal_segment`] wrote and describes exactly
/// the records sealed: their namespace and count, each distinct key once with the tag and
/// payload of its first record given (a later duplicate dropped), no key that was not given,
/// and the same records sealed twice are the same bytes. Zero records is a valid segment.
#[test]
fn check_segment_accepts_exactly_what_seal_segment_sealed() {
    blaze_sim::check(SimConfig::DEFAULT, check_segment_accepts_exactly_what_seal_segment_sealed_case);
}

fn check_segment_accepts_exactly_what_seal_segment_sealed_case(tape: &Tape) {
    let count = tape.int_in(0..=60u32, "records") as usize;
    let namespace = writes()[tape.int_in(0..=2u32, "namespace") as usize];
    let mut given: Vec<(u32, u16, u32)> = Vec::new_in(Global);
    for _ in 0..count {
        // Keys from a small range, so duplicates happen; each record its own tag and payload.
        let n = tape.int_in(0..=40u32, "key");
        let tag = tape.int::<u16>("tag");
        let payload = tape.int_in(0..=5_000u32, "payload");
        given.push((n, tag, payload));
    }
    let payloads: Vec<Vec<u8>> = given.iter().map(|it| value(it.2)).collect();
    let records: Vec<(RecordRef, &[u8])> = given.iter().zip(&payloads).map(|(it, bytes)| (RecordRef { tag: Tag(it.1), key: key(it.0).0 }, &bytes[..])).collect();
    let mut out = Vec::new_in(Global);
    let sealed = seal_segment(namespace, &records, &mut out).unwrap();
    let checked = check_segment(&out).unwrap_or_else(|why| panic!("{count} records: {why}"));
    assert_eq!(checked, sealed, "check_segment describes what seal_segment says it wrote");
    // The first record of each key, in the order given.
    let mut first: BTreeMap<ContentId, (Tag, &[u8])> = BTreeMap::new();
    for (record, bytes) in &records {
        first.entry(record.key).or_insert((record.tag, *bytes));
    }
    assert_eq!((checked.namespace, checked.count as usize), (namespace, first.len()));
    let run = &out[checked.index_at as usize..checked.index_at as usize + checked.count as usize * ENTRY];
    let mut read = BTreeMap::new();
    for i in 0..checked.count as usize {
        let entry = Entry::decode(&run[i * ENTRY..(i + 1) * ENTRY]).unwrap();
        let payload = &out[entry.at as usize..(entry.at + u64::from(entry.len)) as usize];
        assert!(read.insert(entry.key, (entry.tag, payload)).is_none(), "a key twice in one index");
    }
    assert_eq!(read, first, "the records read are the records sealed, the first of each key");
    if count == 0 {
        assert_eq!(out.len(), HEADER + FOOTER, "zero records: a header and a footer");
    }
    let mut again = Vec::new_in(Global);
    seal_segment(namespace, &records, &mut again).unwrap();
    assert_eq!(again, out, "the same records sealed twice wrote different bytes");
}

/// Compaction under a reader: past `K` segments of a tier (`super::core::Tiers`) the tier is
/// merged, so at most [`super::SEGMENTS_MOST`] are named after;
/// over the cap what no root names and is older than the window goes; a reader opened before
/// keeps reading what it read; every file a generation named stays until a grace period after
/// it was retired, and goes after it.
#[test]
fn compaction_keeps_roots_and_the_grace_period() {
    let root = test_root();
    let mut sim = Sim::new(9, Faults::default());
    let mut w = [Actor::new(cfg(0, 1, false, &root), &root, Plant::None)];
    settle(&mut sim, &mut w);
    let mut named = BTreeSet::new();
    for n in 0..40u32 {
        w[0].store.put(key(n), Namespace::LOCAL, TAG, &value(n)).unwrap();
        if n == 39 {
            w[0].store.root("kept", &[key(1), key(2), key(3)]).unwrap();
        }
        let effects = &mut w[0].effects;
        w[0].store.commit(&[],sim.wall_ns(), effects).unwrap();
        settle(&mut sim, &mut w);
        named.extend(w[0].store.take_committed().unwrap().unwrap().segments);
    }
    let before = reader(&mut sim, 1, 1, &root);
    // A merge first (no cap): fewer segments, everything kept.
    let mut slices = 0;
    loop {
        assert!(w[0].store.compact(&mut w[0].effects));
        settle(&mut sim, &mut w);
        let sliced = w[0].store.take_sliced().unwrap();
        slices += 1;
        assert!(!sliced.aborted);
        if !sliced.more || slices > 20 {
            break;
        }
    }
    assert!(w[0].store.segments().len() <= super::SEGMENTS_MOST, "{} segments", w[0].store.segments().len());
    let fresh = reader(&mut sim, 2, 1, &root);
    for n in 0..40 {
        assert_eq!(fresh.store.get(&key(n)).map(|it| it.bytes.to_vec()), Some(value(n)), "merged record {n}");
        assert_eq!(before.store.get(&key(n)).map(|it| it.bytes.to_vec()), Some(value(n)), "a reader of before, record {n}");
    }
    // Within the grace period every file any generation named is still there.
    for id in &named {
        assert!(sim.exists(&FileName::new(&format!("plain/seg-{id:016x}")).unwrap()), "seg {id:016x} went inside the grace period");
    }
    // Past it, the next slice unlinks what was retired.
    wait(&mut sim, Duration::from_nanos(super::GRACE_NS + 1_000_000_000));
    assert!(w[0].store.compact(&mut w[0].effects));
    settle(&mut sim, &mut w);
    let sliced = w[0].store.take_sliced().unwrap();
    assert!(sliced.removed > 0, "{sliced:?}");
    let live: BTreeSet<u64> = w[0].store.segments().iter().map(|it| it.0).collect();
    for id in named.difference(&live) {
        assert!(!sim.exists(&FileName::new(&format!("plain/seg-{id:016x}")).unwrap()), "retired seg {id:016x} stayed past the grace period");
    }
}

/// Over a cap, records no root names that are older than the window go; rooted ones stay.
#[test]
fn pruning_keeps_what_roots_name() {
    let root = test_root();
    let mut sim = Sim::new(13, Faults::default());
    let mut c = cfg(0, 1, false, &root);
    // Above any one commit here (the first holds four records and the root, about 720 bytes;
    // each later one record of at most 699 bytes), below what the 18 commits hold together: the
    // store goes over the cap, and no commit is refused as larger than it alone.
    c.cap_bytes = 2048;
    let mut w = [Actor::new(c, &root, Plant::None)];
    settle(&mut sim, &mut w);
    w[0].store.root("kept", &[key(0), key(1)]).unwrap();
    for n in 0..4u32 {
        w[0].store.put(key(n), Namespace::LOCAL, TAG, &value(n)).unwrap();
    }
    let effects = &mut w[0].effects;
    w[0].store.commit(&[],sim.wall_ns(), effects).unwrap();
    settle(&mut sim, &mut w);
    assert!(matches!(w[0].store.take_committed(), Some(Ok(_))));
    // Age it past the window.
    for n in 100..100 + super::WINDOW as u32 + 1 {
        w[0].store.put(key(n), Namespace::LOCAL, TAG, &value(n)).unwrap();
        let effects = &mut w[0].effects;
        w[0].store.commit(&[],sim.wall_ns(), effects).unwrap();
        settle(&mut sim, &mut w);
        assert!(matches!(w[0].store.take_committed(), Some(Ok(_))));
    }
    assert!(w[0].store.compact(&mut w[0].effects));
    settle(&mut sim, &mut w);
    let sliced = w[0].store.take_sliced().unwrap();
    assert!(sliced.dropped >= 2, "{sliced:?}");
    let r = reader(&mut sim, 1, 1, &root);
    for n in [0, 1] {
        assert_eq!(r.store.get(&key(n)).map(|it| it.bytes.to_vec()), Some(value(n)), "rooted record {n}");
    }
    for n in [2, 3] {
        assert_eq!(r.store.get(&key(n)).map(|it| it.bytes.len()), None, "unrooted old record {n}");
    }
}

// ---- the tiered merge (D159): the invariant of `core::Tiers`, checked after every cleanup ----

/// The policy simulation's choices from one seed (splitmix64), so the honest policy and a
/// planted one run the same history.
struct Draw(u64);

impl Draw {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// A segment's length, log-uniform over [1 KiB, `most`] (`most` the segment cap, a power
    /// of two; at the store's 4 GiB, six orders of magnitude), as numbers (the policy reads
    /// lengths alone). A commit's segment is never past the cap: its layout refuses it.
    fn len(&mut self, most: u64) -> u64 {
        let top = most.ilog2() as u64;
        let shift = 10 + self.below(top - 10);
        (1u64 << shift) + self.below(1u64 << shift)
    }
}

/// A row of the policy simulation's namespace: its length, what a merge of it drops (a
/// pointer's older values), and the records it holds.
struct ModelRow {
    id: u64,
    len: u64,
    dead: u64,
    keys: Vec<u32>,
}

/// The policy simulation's store: one namespace's rows in manifest order, and the history's
/// counts.
#[derive(Default)]
struct TierWorld {
    rows: Vec<ModelRow>,
    next_id: u64,
    next_key: u32,
    /// Every row a merge replaced: never named again, never merged again.
    gone: BTreeSet<u64>,
    /// Every record a completed commit holds.
    keys: BTreeSet<u32>,
    written: u64,
    commits: u64,
    merged: u64,
    cleanups: u32,
    merges: u32,
    most_rows: usize,
}

impl TierWorld {
    /// A commit's segment, landed: one record, a length from `draw`, a quarter of them with
    /// up to half their bytes dropped by a later merge.
    fn commit(&mut self, draw: &mut Draw, tiers: Tiers) -> u64 {
        let len = draw.len(tiers.segment_most());
        let dead = if draw.below(4) == 0 { draw.below(len / 2 + 1) } else { 0 };
        let id = self.next_id;
        self.next_id += 1;
        let key = self.next_key;
        self.next_key += 1;
        self.rows.push(ModelRow { id, len, dead, keys: vec![key] });
        self.keys.insert(key);
        self.written += len;
        self.commits += 1;
        id
    }

    /// `picked` (indices into the rows) merged as the store's flip applies a slice: one copy
    /// of what they keep, at the NEWEST one's place.
    fn apply(&mut self, picked: &[usize], most: u64) -> Result<(), String> {
        if picked.is_empty() {
            return Ok(());
        }
        if let Some(twice) = picked.iter().map(|&i| self.rows[i].id).find(|id| self.gone.contains(id)) {
            return Err(format!("row {twice} merged twice"));
        }
        let (mut len, mut dead, mut keys) = (0u64, 0u64, Vec::new());
        for &i in picked {
            len += self.rows[i].len;
            dead += self.rows[i].dead;
            keys.extend_from_slice(&self.rows[i].keys);
            self.gone.insert(self.rows[i].id);
        }
        let overhead = (HEADER + FOOTER) as u64;
        let out = len.saturating_sub(dead).saturating_sub((picked.len() as u64 - 1) * overhead).max(overhead);
        // THE CAP, where the store checks it (`copy_live`, before a byte of the copy is
        // written): a copy past it is refused.
        if out > most {
            return Err(format!("{CAUGHT_PAST_CAP}: a merge of {} rows is {out} bytes, past the cap of {most}", picked.len()));
        }
        let newest = *picked.iter().max().expect("not empty");
        let place = (0..newest).filter(|i| !picked.contains(i)).count();
        let copy = ModelRow { id: self.next_id, len: out, dead: 0, keys };
        self.next_id += 1;
        let mut i = 0;
        self.rows.retain(|_| {
            i += 1;
            !picked.contains(&(i - 1))
        });
        self.rows.insert(place, copy);
        self.merged += len;
        self.merges += 1;
        Ok(())
    }

    /// The oracles after a completed cleanup whose flip committed `own`.
    fn oracles(&mut self, tiers: Tiers, own: u64) -> Result<(), String> {
        self.cleanups += 1;
        let mut count = [0usize; 64];
        // A closed row (`Tiers::closed`) is outside the tiers' counts: never merged again.
        for row in self.rows.iter().filter(|it| it.id != own && !tiers.closed(it.len)) {
            count[tiers.tier(row.len)] += 1;
        }
        if let Some(row) = self.rows.iter().find(|it| it.len > tiers.segment_most()) {
            return Err(format!("{CAUGHT_PAST_CAP}: after cleanup {}, row {} is {} bytes, past the cap of {}", self.cleanups, row.id, row.len, tiers.segment_most()));
        }
        if let Some(t) = (0..64).find(|&t| count[t] > tiers.most()) {
            return Err(format!("after cleanup {}, tier {t} names {} segments, past K = {}: {count:?}", self.cleanups, count[t], tiers.most()));
        }
        let bytes: u64 = self.rows.iter().map(|it| it.len).sum();
        if self.rows.len() > tiers.segments_most(bytes) {
            return Err(format!("after cleanup {}, {} segments of {bytes} bytes, past the bound {}", self.cleanups, self.rows.len(), tiers.segments_most(bytes)));
        }
        // Every committed record is readable, once, and nothing else is.
        let mut held = BTreeSet::new();
        for key in self.rows.iter().flat_map(|it| it.keys.iter()) {
            if !held.insert(*key) {
                return Err(format!("record {key} is in two rows after cleanup {}", self.cleanups));
            }
        }
        if held != self.keys {
            return Err(format!("after cleanup {}, the rows hold {} records of the {} committed", self.cleanups, held.len(), self.keys.len()));
        }
        if let Some(row) = self.rows.iter().find(|it| self.gone.contains(&it.id)) {
            return Err(format!("merged row {} is named again", row.id));
        }
        if self.merged > tiers.merged_most(self.written, self.commits) {
            return Err(format!(
                "{} bytes merged after {} committed in {} commits: past the amortisation's {}",
                self.merged,
                self.written,
                self.commits,
                tiers.merged_most(self.written, self.commits)
            ));
        }
        self.most_rows = self.most_rows.max(self.rows.len());
        Ok(())
    }
}

/// The policy simulation: `commands` commands of one namespace merging by `tiers`, each
/// committing 0 to 4 segments mid-command, then its cleanup: the merge `tiers` plans over the
/// rows, flipped with the command's last segment, unless a power cut takes the cleanup (one in
/// ten) or the merge's sync fails (one in ten: the store stops before the flip, so neither the
/// merge nor the commit lands). After every completed cleanup, [`TierWorld::oracles`].
fn tier_history(seed: u64, commands: u32, tiers: Tiers) -> Result<TierWorld, String> {
    let mut draw = Draw(seed);
    let mut world = TierWorld::default();
    for _ in 0..commands {
        for _ in 0..draw.below(5) {
            world.commit(&mut draw, tiers);
        }
        let mut merge = world.plan(tiers);
        if draw.below(10) < 2 {
            continue;
        }
        // As `Lent::clean_on`: a split merge (`more`) is followed by the next, in the same
        // cleanup, until the policy plans none (the real store also stops on its slice count
        // and budget; the policy alone must converge).
        let mut slices = 0u32;
        loop {
            if merge.out_len > tiers.segment_most() {
                return Err(format!("{CAUGHT_PAST_CAP}: a merge of {} rows planned at {} bytes, past the cap of {}", merge.rows.len(), merge.out_len, tiers.segment_most()));
            }
            world.apply(&merge.rows, tiers.segment_most())?;
            if !merge.more {
                break;
            }
            slices += 1;
            if slices > 64 {
                return Err(format!("after cleanup {}, the policy planned 64 merges and still had more", world.cleanups));
            }
            merge = world.plan(tiers);
        }
        let own = world.commit(&mut draw, tiers);
        world.oracles(tiers, own)?;
    }
    Ok(world)
}

/// What an oracle says when a segment past the cap was planned, copied or named: the control
/// (`TierPlant::PastCap`) is caught by it and nothing else.
const CAUGHT_PAST_CAP: &str = "past the segment cap";

impl TierWorld {
    /// The merge `tiers` plans over the rows.
    fn plan(&self, tiers: Tiers) -> super::core::TierMerge<Global> {
        let view: Vec<TierRow> = self.rows.iter().map(|it| TierRow { len: it.len, mergeable: true }).collect();
        tiers.merge(&view, Global)
    }
}

/// THE INVARIANT, as numbers: up to 500 commands, segments of 1 KiB to 16 GiB, cuts and failed
/// syncs: after every completed cleanup no tier over `K` but the flip's own segment, the
/// namespace within `segments_most` of its bytes, every committed record in one row, no row
/// merged twice, the merged bytes within `16 W + 5/4 B C`.
#[test]
fn the_tier_invariant_holds_after_every_cleanup() {
    blaze_sim::check(SimConfig::DEFAULT, |tape| {
        let seed = tape.next_u64("history seed");
        let commands = tape.int_in(1..=500u32, "commands");
        let got = tier_history(seed, commands, Tiers::STORE);
        assert!(got.is_ok(), "seed {seed}, {commands} commands: {:?}", got.err());
    });
}

/// Five hundred commands a seed, the store's geometry: prints the most segments named and the
/// bytes merged per byte committed (the amortisation's ratio, its bound 16 and `5/4 B` a commit).
#[test]
fn five_hundred_commands_keep_the_tier_bound() {
    for seed in 0..16 {
        let world = tier_history(seed, 500, Tiers::STORE).unwrap_or_else(|why| panic!("seed {seed}: {why}"));
        assert!(world.merges > 0, "seed {seed}: no merge ran: the history did not cover one");
        println!(
            "tiers: seed {seed}: {} cleanups, {} merges, at most {} segments named, {:.2} bytes merged a byte committed",
            world.cleanups,
            world.merges,
            world.most_rows,
            world.merged as f64 / world.written.max(1) as f64
        );
    }
}

/// Control 1: the refusal D159 removes (a merge thrown away once its second segment passes
/// `SLICE_BYTES`) breaks the invariant, and the oracle says so.
#[test]
fn the_tier_oracle_catches_a_merge_refused_past_the_slice() {
    let planted = Tiers::planted(TIER_FLOOR_BYTES, TierPlant::RefusedPastSlice);
    let mut caught = None;
    for seed in 0..64 {
        let honest = tier_history(seed, 500, Tiers::STORE);
        assert!(honest.is_ok(), "seed {seed} without the plant: {:?}", honest.err());
        if caught.is_none() {
            caught = tier_history(seed, 500, planted).err();
        }
    }
    assert!(caught.is_some(), "no seed of 64 caught a merge refused past SLICE_BYTES");
}

/// THE CAP as numbers, with the cap a parameter: a 64 KiB cap over a 1 KiB floor (two open
/// tiers), and the store's 4 GiB. After every cleanup no row is past the cap, no planned
/// merge's estimate is, the open tiers hold at most `K`, and the bound counts the closed rows.
#[test]
fn no_merge_is_planned_past_the_segment_cap() {
    for tiers in [Tiers::capped(1 << 10, 64 << 10, TierPlant::None), Tiers::STORE] {
        blaze_sim::check(SimConfig::DEFAULT, |tape| {
            let seed = tape.next_u64("history seed");
            let commands = tape.int_in(1..=500u32, "commands");
            let got = tier_history(seed, commands, tiers);
            assert!(got.is_ok(), "cap {}, seed {seed}, {commands} commands: {:?}", tiers.segment_most(), got.err());
        });
    }
}

/// The cold tempo check's store, as numbers: 88 commit rows of 256 MiB behind one cleanup (a
/// lent commit each 256 MiB, 22 GiB in all). The merge before 2026-11-20 planned all 88 as one
/// 22 GiB segment; now each merge is under 4 GiB and the cleanup ends with every row named
/// readable by the open's check.
#[test]
fn a_cold_build_of_large_commits_merges_under_the_cap() {
    let tiers = Tiers::STORE;
    let rows: Vec<TierRow> = (0..88).map(|_| TierRow { len: 256 << 20, mergeable: true }).collect();
    let planted = Tiers::capped(TIER_FLOOR_BYTES, SEGMENT_MOST, TierPlant::PastCap).merge(&rows, Global);
    assert!(planted.out_len > SEGMENT_MOST, "the planted policy of before plans one {} byte merge", planted.out_len);
    let mut world = TierWorld::default();
    for _ in 0..88 {
        world.rows.push(ModelRow { id: world.next_id, len: 256 << 20, dead: 0, keys: vec![world.next_key] });
        world.keys.insert(world.next_key);
        world.next_id += 1;
        world.next_key += 1;
    }
    let mut merges = 0;
    loop {
        let merge = world.plan(tiers);
        if merge.rows.is_empty() {
            break;
        }
        assert!(merge.out_len <= SEGMENT_MOST, "merge {merges} planned at {} bytes", merge.out_len);
        world.apply(&merge.rows, SEGMENT_MOST).unwrap_or_else(|why| panic!("{why}"));
        merges += 1;
        assert!(merges < 32, "the policy does not converge");
    }
    for row in &world.rows {
        assert!(row.len <= SEGMENT_MOST, "row {} of {} bytes", row.id, row.len);
    }
    assert!(world.rows.len() <= tiers.segments_most(88 * (256 << 20)), "{} rows past the bound", world.rows.len());
    println!("tiers: 88 rows of 256 MiB: {merges} merges, {} rows named after", world.rows.len());
}

/// Control: the policy blind to the cap (`TierPlant::PastCap`, the one before 2026-11-20) is
/// caught past the cap, by the cap's oracle, on some seed of a 64 KiB cap.
#[test]
fn the_cap_oracle_catches_a_policy_blind_to_the_cap() {
    let planted = Tiers::capped(1 << 10, 64 << 10, TierPlant::PastCap);
    let mut caught = None;
    for seed in 0..64 {
        if let Err(why) = tier_history(seed, 500, planted) {
            assert!(why.starts_with(CAUGHT_PAST_CAP), "seed {seed}: caught by another oracle: {why}");
            caught = Some(why);
            break;
        }
    }
    assert!(caught.is_some(), "no seed of 64 caught a merge past a 64 KiB cap");
}

/// The real store's tiers in the simulation: from 1 KiB, so its KiB segments span five tiers.
const TEST_TIERS: Tiers = Tiers::planted(1 << 10, TierPlant::None);

/// The simulation's merge buffer: 2 KiB, chunks of 1 KiB, so a merge of KiB segments (records
/// of up to 700 bytes, some across two chunks) takes tens of chunks, and most merges are
/// larger than the buffer (what the whole-`Batch` control needs to be caught).
const TEST_MERGE_BUFFER: u64 = 2 << 10;

const _: () = assert!(TEST_MERGE_BUFFER < MERGE_BUFFER);

/// The simulation's index chunk: four entries, so a merge of more than four records writes its
/// index in several chunks (what the whole-index control needs to be caught).
const TEST_INDEX_CHUNK: u64 = 4 * ENTRY as u64;

const _: () = assert!(TEST_INDEX_CHUNK < INDEX_CHUNK);

/// One process of a tiered history: its store merging by [`TEST_TIERS`], streaming through
/// [`TEST_MERGE_BUFFER`] and [`TEST_INDEX_CHUNK`], opened whole.
/// `sweep`: the store keeps what its roots reach (`Liveness::Reached`, sweep mode), not
/// everything.
fn tiered_actor<'f>(n: u64, boot: u64, durable: bool, plant: Plant, sweep: bool, root: &'f Frame) -> Actor<'f> {
    tiered_actor_in(TEST_TIERS, n, boot, durable, plant, sweep, root)
}

/// [`tiered_actor`] merging by `tiers` (its segment cap among them).
fn tiered_actor_in<'f>(tiers: Tiers, n: u64, boot: u64, durable: bool, plant: Plant, sweep: bool, root: &'f Frame) -> Actor<'f> {
    let mut config = cfg(n, boot, durable, root);
    if sweep {
        config.liveness = super::Liveness::Reached;
    }
    let mut store = Plain::tiered(config, root, plant, tiers, TEST_MERGE_BUFFER, TEST_INDEX_CHUNK);
    let mut effects = Vec::new_in(Global);
    store.open_whole(&mut effects);
    Actor { store, effects, inject: None, armed: false, fired: false, kinds: BTreeMap::new(), meter: Meter::default() }
}

/// `count` fresh keys of the local namespace ([`space`] of a multiple of three).
fn local_keys(next: &mut u32, count: u32) -> Vec<u32> {
    (0..count)
        .map(|_| {
            let n = *next;
            *next += 3;
            n
        })
        .collect()
}

/// `count` fresh keys: of the local namespace alone, or `spread` over the three a writer
/// writes ([`space`] of each in turn), so several namespaces fall behind at once.
fn fresh_keys(next: &mut u32, count: u32, spread: bool) -> Vec<u32> {
    if !spread {
        return local_keys(next, count);
    }
    let keys = (*next..*next + count).collect();
    *next += count;
    keys
}

/// How a process's run ended.
enum Ran {
    Settled,
    /// The power cut.
    Cut,
    /// An injected failure stopped it.
    Broke,
}

fn tier_ran(sim: &mut Sim, a: &mut Actor<'_>, budget: &mut Option<u32>) -> Result<Ran, String> {
    if !run(sim, std::slice::from_mut(a), budget) {
        return Ok(Ran::Cut);
    }
    match a.store.broken() {
        None => Ok(Ran::Settled),
        Some(why) if injected(why) => Ok(Ran::Broke),
        // The cap's typed refusals, by variant: a copy or commit past it before a byte was
        // written, or a manifest row the open refuses.
        Some(StoreError::Shape { namespace, why: RowRefused::TooLong { len, most } }) => Err(format!("{CAUGHT_PAST_CAP}: namespace {:x}: a segment of {len} bytes refused before its write, past {most}", namespace.0)),
        Some(StoreError::Row { generation, segment, why: RowRefused::TooLong { len, most }, .. }) => Err(format!("{CAUGHT_PAST_CAP}: generation {generation}'s row of segment {segment:016x} is {len} bytes, past {most}")),
        Some(why) => Err(format!("a writer broke on its own: {why}")),
    }
}

/// THE CAP's oracle on the real store, after every commit: every row the store names is one
/// its next open reads (`format::Row::new` against the manifest's generation and the cap).
fn cap_oracle(store: &Plain<&Frame>, generation: u64) -> Result<(), String> {
    let most = store.tiers().segment_most();
    for seg in store.named_rows() {
        if let Err(why) = format::Row::new(format::RowFields::of(seg), generation, most) {
            return Err(format!("{CAUGHT_PAST_CAP}: generation {generation} names segment {:016x}, a row its open refuses: {why:?}", seg.id));
        }
    }
    Ok(())
}

/// What a tiered history did.
#[derive(Default)]
struct TierLog {
    gone: BTreeSet<u64>,
    written: u64,
    commits: u64,
    merged: u64,
    cleanups: u32,
    merges: u32,
    most_named: usize,
    /// Copies the merge meter saw larger than the store's buffer (a whole-`Batch` copy of
    /// one would hold past it), and the most bytes it saw in flight at once.
    over_buffer: u32,
    alive_most: u64,
    /// Copies whose index the meter saw larger than one index chunk (a whole index held would
    /// be past it), and the most bytes of index it saw in flight at once.
    over_index: u32,
    index_alive_most: u64,
    /// Sweeps (sweep mode's slices of a sparse run, not tiered merges): their bytes are not
    /// the merge's, so not in `merged`.
    sweeps: u32,
    /// Slices past a cleanup's first (`Plain::cleanup_goes_on`), and cleanups that stopped on
    /// their work budget with a namespace still behind (the remainder allowed).
    extra_slices: u32,
    left_over: u32,
}

/// The commit of `keys` landed: in the ledger, and its segments' bytes in the log.
fn tier_landed(ledger: &mut Ledger, log: &mut TierLog, store: &Plain<&Frame>, keys: &[u32], committed: &Committed<&Frame>) -> Result<(), String> {
    ledger.land(keys, committed)?;
    for id in committed.segments.iter() {
        let row = store.named_rows().iter().find(|it| it.id == *id).ok_or_else(|| format!("committed segment {id:016x} is not named"))?;
        log.written += row.file_len();
        log.commits += 1;
    }
    cap_oracle(store, store.generation())
}

/// A sweep-mode history's roots and litter: one index a commit of the ledger's keys it put,
/// all named by the one root `kept` (re-set each commit, so its old record is dead too), and
/// LITTER beside them, records no root names, more bytes than the commit's live ones: so every
/// segment past its youth is under half live, and a sparse row is always there to sweep.
#[derive(Default)]
struct Sweeping {
    indexes: Vec<Key>,
    litter: u32,
}

/// Where litter's numbers start: past every key a history puts in the ledger.
const LITTER_FROM: u32 = 1 << 28;

/// `keys` put in `a` and noted in the ledger as maybe; in sweep mode (`sweeping`), with three
/// litter records a key and two more, an index of `keys` and the root `kept` naming every index
/// so far.
fn tier_put(a: &mut Actor<'_>, ledger: &mut Ledger, keys: &[u32], sweeping: Option<&mut Sweeping>) -> Result<(), String> {
    for &n in keys {
        a.store.put(key(n), space(n), TAG, &value(n)).map_err(|it| it.to_string())?;
        ledger.maybe.insert(n, space(n));
    }
    let Some(sweeping) = sweeping else { return Ok(()) };
    if keys.is_empty() {
        return Ok(());
    }
    for _ in 0..3 * keys.len() + 2 {
        let n = LITTER_FROM + sweeping.litter;
        sweeping.litter += 1;
        a.store.put(key(n), Namespace::LOCAL, TAG, &value(n)).map_err(|it| it.to_string())?;
    }
    let named: Vec<Key> = keys.iter().map(|&n| key(n)).collect();
    let index = a.store.index(&named).map_err(|it| it.to_string())?;
    sweeping.indexes.push(index);
    a.store.root_through("kept", &sweeping.indexes, format::NEVER).map_err(|it| it.to_string())?;
    Ok(())
}

/// `mids`' commits (each that many fresh records, [`fresh_keys`]) in `a`: how the run ended.
fn tier_commits(sim: &mut Sim, a: &mut Actor<'_>, ledger: &mut Ledger, log: &mut TierLog, next: &mut u32, mids: &[u32], spread: bool, mut sweeping: Option<&mut Sweeping>, budget: &mut Option<u32>) -> Result<Ran, String> {
    for &count in mids {
        let keys = fresh_keys(next, count, spread);
        tier_put(a, ledger, &keys, sweeping.as_deref_mut())?;
        a.store.commit(&[], sim.wall_ns(), &mut a.effects).map_err(|it| it.to_string())?;
        let ran = tier_ran(sim, a, budget)?;
        if let Some(Ok(committed)) = a.store.take_committed() {
            tier_landed(ledger, log, &a.store, &keys, &committed)?;
        }
        if !matches!(ran, Ran::Settled) {
            return Ok(ran);
        }
    }
    Ok(Ran::Settled)
}

/// The oracles after a completed cleanup of one slice: [`tier_slice_oracles`], then
/// [`tier_invariant`] with no remainder allowed.
fn tier_oracles(store: &Plain<&Frame>, before: &[format::Segment], committed: &Committed<&Frame>, ledger: &mut Ledger, log: &mut TierLog) -> Result<(), String> {
    log.cleanups += 1;
    tier_slice_oracles(store, before, committed, true, ledger, log)?;
    let own: BTreeSet<u64> = committed.segments.iter().copied().collect();
    tier_invariant(store, &own, false, log)
}

/// The oracles after one slice's flip landed: `before`, the rows it was planned over;
/// `committed`, the flip's commit; `merged`, the slice was a tiered merge (else a sweep, whose
/// bytes the amortisation does not bound). The merged records re-land in the copy (the ledger
/// follows them), and read back equal; no row merged twice or named again once merged; one
/// copy a slice.
fn tier_slice_oracles(store: &Plain<&Frame>, before: &[format::Segment], committed: &Committed<&Frame>, merged: bool, ledger: &mut Ledger, log: &mut TierLog) -> Result<(), String> {
    let after = store.named_rows();
    let own: BTreeSet<u64> = committed.segments.iter().copied().collect();
    let named_before: BTreeSet<u64> = before.iter().map(|it| it.id).collect();
    let named_after: BTreeSet<u64> = after.iter().map(|it| it.id).collect();
    let candidates: BTreeSet<u64> = named_before.difference(&named_after).copied().collect();
    let made: Vec<u64> = named_after.iter().copied().filter(|id| !named_before.contains(id) && !own.contains(id)).collect();
    if !candidates.is_empty() {
        if let Some(id) = candidates.iter().find(|id| log.gone.contains(id)) {
            return Err(format!("segment {id:016x} merged twice"));
        }
        let &[copy] = made.as_slice() else {
            return Err(format!("a merge of {candidates:x?} named {made:x?}: one copy"));
        };
        for (n, landed) in ledger.committed.iter_mut().filter(|(_, it)| candidates.contains(&it.segment)) {
            landed.segment = copy;
            match store.get(&key(*n)) {
                Some(got) if got.bytes == value(*n) && got.tag == TAG => {}
                other => return Err(format!("merged record {n} reads {other:?} after cleanup {}", log.cleanups)),
            }
        }
        if merged {
            log.merged += before.iter().filter(|it| candidates.contains(&it.id)).map(|it| it.file_len()).sum::<u64>();
            log.merges += 1;
        } else {
            log.sweeps += 1;
        }
        log.gone.extend(candidates.iter().copied());
    }
    if let Some(id) = named_after.iter().find(|id| log.gone.contains(id)) {
        return Err(format!("merged segment {id:016x} is named again"));
    }
    Ok(())
}

/// After a whole cleanup (every slice of it landed): THE INVARIANT in every namespace a writer
/// writes (no tier over `K`, the cleanup's own commit's segments, `own`, apart), each within
/// its bound; the merged bytes within the amortisation's. `remainder`: the cleanup stopped on
/// its work budget (`Plain::cleanup_goes_on`) with a slice still to run, so a namespace may
/// still be over: that remainder, and only it, is allowed (the next command's), and a cleanup
/// that stopped for any other reason must have left every namespace within.
fn tier_invariant(store: &Plain<&Frame>, own: &BTreeSet<u64>, remainder: bool, log: &mut TierLog) -> Result<(), String> {
    let tiers = store.tiers();
    let after = store.named_rows();
    let mut behind = 0u32;
    for namespace in writes() {
        let mut count = [0usize; 64];
        let (mut bytes, mut named) = (0u64, 0usize);
        for row in after.iter().filter(|it| it.namespace == namespace) {
            bytes += row.file_len();
            named += 1;
            if row.local && !row.published && !own.contains(&row.id) && !tiers.closed(row.file_len()) {
                count[tiers.tier(row.file_len())] += 1;
            }
        }
        let over = (0..64).find(|&t| count[t] > tiers.most());
        if over.is_some() || named > tiers.segments_most(bytes) {
            if !remainder {
                return Err(format!(
                    "after cleanup {}, namespace {:x}: tier {over:?} past K = {}, or {named} segments of {bytes} bytes past the bound {}: {count:?}",
                    log.cleanups,
                    namespace.0,
                    tiers.most(),
                    tiers.segments_most(bytes)
                ));
            }
            behind += 1;
        }
        if namespace == Namespace::LOCAL {
            log.most_named = log.most_named.max(named);
        }
    }
    if behind > 0 {
        log.left_over += 1;
    }
    if log.merged > tiers.merged_most(log.written, log.commits) {
        return Err(format!("{} bytes merged after {} committed in {} commits: past the amortisation's bound", log.merged, log.written, log.commits));
    }
    Ok(())
}

/// One slice folded in `a` (written with no lock) under the merge meter: THE MERGE'S MEMORY
/// BOUND, checked whether the run settled, was cut or broke.
fn tier_fold(sim: &mut Sim, a: &mut Actor<'_>, log: &mut TierLog, budget: &mut Option<u32>) -> Result<Ran, String> {
    a.meter = Meter { on: true, ..Meter::default() };
    // A slice's inputs are rows this generation names: at most this many cursors.
    let inputs = a.store.named_rows().len();
    a.store.take_merge_held_most();
    a.store.fold_slice(&mut a.effects).map_err(|it| it.to_string())?;
    let ran = tier_ran(sim, a, budget)?;
    let meter = std::mem::take(&mut a.meter);
    let held = a.store.take_merge_held_most();
    meter.within(a.store.merge_buffer(), a.store.index_chunk(), held, inputs)?;
    if meter.total > a.store.merge_buffer() {
        log.over_buffer += 1;
    }
    if meter.index_total > a.store.index_chunk() {
        log.over_index += 1;
    }
    log.alive_most = log.alive_most.max(meter.alive_most);
    log.index_alive_most = log.index_alive_most.max(meter.index_alive_most);
    Ok(ran)
}

/// A command's cleanup in `a`, as blaze-cli's `after_answer`: the slice folded, then the commit
/// of `keys` (one more record) whose flip names it; then, while the store's
/// `Plain::cleanup_goes_on` says so (as `Lent::clean_on`), one more slice folded and named by
/// a commit of nothing else. [`tier_slice_oracles`] after each slice's flip landed, and
/// [`tier_invariant`] once the cleanup is over, allowing a namespace still behind only when it
/// stopped on its work budget or count, or after a pruning slice; in sweep mode (`sweeping`)
/// a stop after a sweep allows none (the merge is planned first, and goes on after a sweep
/// that left a namespace over).
fn tier_cleanup(sim: &mut Sim, a: &mut Actor<'_>, ledger: &mut Ledger, log: &mut TierLog, next: &mut u32, spread: bool, mut sweeping: Option<&mut Sweeping>, budget: &mut Option<u32>) -> Result<Ran, String> {
    let sweep_mode = sweeping.is_some();
    if sweep_mode {
        // As a command's `after_answer`: the answer's commit (the loop's first, whose roots
        // name its records) lets the process's lease go, so the rows its lease kept from the
        // first slice are a later slice's to merge.
        a.store.release_lease();
    }
    let ran = tier_fold(sim, a, log, budget)?;
    if !matches!(ran, Ran::Settled) {
        return Ok(ran);
    }
    log.cleanups += 1;
    let mut keys = fresh_keys(next, 1, spread);
    let mut own: BTreeSet<u64> = BTreeSet::new();
    let mut slices = 0u32;
    let mut so_far: Option<Sliced> = None;
    loop {
        let before: Vec<format::Segment> = a.store.named_rows().to_vec();
        tier_put(a, ledger, &keys, sweeping.as_deref_mut())?;
        a.store.commit(&[], sim.wall_ns(), &mut a.effects).map_err(|it| it.to_string())?;
        let ran = tier_ran(sim, a, budget)?;
        let Some(Ok(committed)) = a.store.take_committed() else { return Ok(ran) };
        tier_landed(ledger, log, &a.store, &keys, &committed)?;
        keys.clear();
        let Some(last) = a.store.take_sliced() else { return Ok(ran) };
        if last.aborted {
            return Ok(ran);
        }
        tier_slice_oracles(&a.store, &before, &committed, last.merged, ledger, log)?;
        own.extend(committed.segments.iter().copied());
        if !matches!(ran, Ran::Settled) {
            return Ok(ran);
        }
        if slices > 0 && last.segments > 0 {
            // Only merges go on: a slice past the first follows a tiered merge, or is one (in
            // sweep mode, after a sweep that left a namespace over).
            if !so_far.is_some_and(|it| it.merged) && !(sweep_mode && last.merged) {
                return Err(format!("after cleanup {}: a slice ran past a cleanup's first after a pruning or sweeping slice, and was no merge", log.cleanups));
            }
            log.extra_slices += 1;
        }
        slices += u32::from(last.segments > 0);
        let total = so_far.map_or(last, |it| it.and(last));
        so_far = Some(total);
        if !a.store.cleanup_goes_on(&total, slices, &last) {
            // Stopped: on the budget or the count with a slice still to run, or after a
            // pruning slice, which ends a cleanup (the remainder allowed); or because the last
            // found nothing to do. In sweep mode a stop after a sweep leaves no remainder: THE
            // INVARIANT holds after it (the case `Plant::SweepBeforeMerge` breaks).
            let remainder = last.segments > 0 && (total.bytes_in >= a.store.cleanup_budget() || slices >= CLEANUP_SLICES_MOST || (!last.merged && !sweep_mode));
            tier_invariant(&a.store, &own, remainder, log)?;
            return Ok(ran);
        }
        let ran = tier_fold(sim, a, log, budget)?;
        if !matches!(ran, Ran::Settled) {
            return Ok(ran);
        }
    }
}

/// The tiered history on the real store: `commands` processes, one at a time, each opening the
/// store, committing 0 to 4 times mid-command (1 to 32 records each: segments of under 1 KiB
/// to tens of KiB, merged into hundreds), then its cleanup; effects failing one in
/// `fail_one_in` (a failed segment sync stops the store before its flip), and a power cut after
/// `cut` completions (durable only: a cut there loses nothing committed). The oracles after
/// every completed cleanup ([`tier_oracles`]), and at the end a reader's: every committed
/// record reads back. Each process's store carries `plant`; its records are local, or `spread`
/// over the three namespaces (so several fall behind at once, and a cleanup runs more than
/// one slice: [`tier_cleanup`]). `sweep`: in sweep mode (`Liveness::Reached`), every commit
/// rooting its records and leaving litter beside them ([`Sweeping`]).
fn tiered_history(seed: u64, commands: u32, cut: Option<u32>, fail_one_in: u32, durable: bool, plant: Plant, spread: bool, sweep: bool) -> Result<TierLog, String> {
    tiered_history_in(TEST_TIERS, seed, commands, cut, fail_one_in, durable, plant, spread, sweep)
}

/// [`tiered_history`] merging by `tiers`: its floor `B` and its segment cap parameters.
fn tiered_history_in(tiers: Tiers, seed: u64, commands: u32, cut: Option<u32>, fail_one_in: u32, durable: bool, plant: Plant, spread: bool, sweep: bool) -> Result<TierLog, String> {
    let mut sweeping = sweep.then(Sweeping::default);
    let root = test_root();
    let mut sim = Sim::new(seed, Faults { fail_one_in, ..Faults::default() });
    let mut draw = Draw(seed ^ 0x5EED);
    let mut ledger = Ledger { never: vec![1, 2], ..Ledger::default() };
    let mut log = TierLog::default();
    // Spread keys from 3 on, past the ledger's `never`.
    let mut next = if spread { 3 } else { 0 };
    let mut boot = 1;
    let mut budget = cut;
    let mut crashed = false;
    for command in 0..commands {
        let mids: Vec<u32> = (0..draw.below(5)).map(|_| 1 << draw.below(6)).collect();
        let mut a = tiered_actor_in(tiers, 10 + 2 * u64::from(command), boot, durable, plant, sweep, &root);
        let mut ran = tier_ran(&mut sim, &mut a, &mut budget)?;
        if matches!(ran, Ran::Settled) {
            ran = tier_commits(&mut sim, &mut a, &mut ledger, &mut log, &mut next, &mids, spread, sweeping.as_mut(), &mut budget)?;
        }
        if matches!(ran, Ran::Settled) {
            ran = tier_cleanup(&mut sim, &mut a, &mut ledger, &mut log, &mut next, spread, sweeping.as_mut(), &mut budget)?;
        }
        drop(a);
        if matches!(ran, Ran::Cut) {
            sim.crash();
            boot += 1;
            crashed = true;
            budget = None;
        }
    }
    // The history's faults end here: the reader opens and acknowledges what it found with no
    // injected failure (`reader` asserts none). What the history's failures left (an orphaned
    // temporary copy, a commit's unnamed segment) the reader's open reports and its
    // acknowledgement flips; the oracle then reads every committed record back.
    sim.heal();
    let r = reader(&mut sim, 5, boot, &root);
    oracle(&r.store, &ledger, crashed, durable)?;
    // And every row it opened on is one a store of this history's cap reads.
    for seg in r.store.named_rows() {
        if let Err(why) = format::Row::new(format::RowFields::of(seg), r.store.generation(), tiers.segment_most()) {
            return Err(format!("{CAUGHT_PAST_CAP}: the reader's generation names segment {:016x}: {why:?}", seg.id));
        }
    }
    Ok(log)
}

/// The cap in the simulation: a 64 KiB cap over a 1 KiB floor (two open tiers, closed rows
/// from 16 KiB), so a history of KiB commits reaches it within tens of commands. Power cuts,
/// failed effects, three namespaces: the cap's oracle after every commit (`cap_oracle`), the
/// open's own check after every crash (the next command opens through it), the reader's at
/// the end, and the invariant with closed rows outside the counts after every cleanup.
const CAPPED_TIERS: Tiers = Tiers::capped(1 << 10, 64 << 10, TierPlant::None);

#[test]
fn a_capped_store_never_names_a_segment_past_its_cap() {
    blaze_sim::check(SimConfig::DEFAULT, |tape| {
        let seed = tape.next_u64("sim seed");
        let commands = tape.int_in(1..=80u32, "commands");
        let durable = tape.int_in(0..=1u32, "durable") == 1;
        let cut = if durable && tape.int_in(0..=1u32, "power cut") == 1 { Some(tape.int_in(0..=6000u32, "cut after")) } else { None };
        let fail = if tape.int_in(0..=3u32, "failing effects") == 0 { 40 } else { 0 };
        let spread = tape.int_in(0..=1u32, "spread over namespaces") == 1;
        let got = tiered_history_in(CAPPED_TIERS, seed, commands, cut, fail, durable, Plant::None, spread, false);
        assert!(got.is_ok(), "seed {seed}, {commands} commands, durable {durable}, cut {cut:?}, fail 1 in {fail}, spread {spread}: {:?}", got.err());
    });
}

/// Long capped histories: merges split and rows close; prints how many.
#[test]
fn a_long_capped_history_splits_merges_and_closes_rows() {
    for seed in 0..4 {
        let log = tiered_history_in(CAPPED_TIERS, seed, 150, None, 0, true, Plant::None, false, false).unwrap_or_else(|why| panic!("seed {seed}: {why}"));
        assert!(log.merges > 0, "seed {seed}: no merge ran");
        println!("capped: seed {seed}: {} merges, {} slices past a cleanup's first, at most {} segments named", log.merges, log.extra_slices, log.most_named);
    }
}

/// Control: the real store with the policy blind to the cap (`TierPlant::PastCap`) under a
/// 64 KiB cap is caught with the typed refusal: `StoreError::Shape` with `RowRefused::TooLong`
/// from `copy_live`, before the copy is written (or, were that check gone, `StoreError::Row`
/// at the flip's encode). Nothing else catches it first.
#[test]
fn the_cap_oracle_catches_a_merge_past_the_cap() {
    let planted = Tiers::capped(1 << 10, 64 << 10, TierPlant::PastCap);
    let mut caught = None;
    for seed in 0..16 {
        if let Err(why) = tiered_history_in(planted, seed, 150, None, 0, true, Plant::None, false, false) {
            assert!(why.starts_with(CAUGHT_PAST_CAP), "seed {seed}: caught by another oracle: {why}");
            caught = Some(why);
            break;
        }
    }
    let why = caught.expect("no seed of 16 caught a merge past a 64 KiB cap");
    println!("capped, planted PastCap: caught: {why}");
}

/// The row type's one check: each condition refused with its values, the manifest's encode
/// refusing what its decode would, and the decode refusing the cold tempo check's row (a
/// 22 GiB segment, the first row, at byte 32) by segment, byte and lengths.
#[test]
fn a_row_no_commit_writes_is_refused_by_name() {
    let good = format::RowFields {
        digest: ContentId([7; 32]),
        len: HEADER as u64 + 16 + ENTRY as u64 + FOOTER as u64,
        index_at: HEADER as u64 + 16,
        count: 1,
        flags: 1,
        namespace: Namespace::LOCAL,
        generation: 3,
        boot: 1,
        touched: 3,
    };
    assert!(format::Row::new(good, 3, SEGMENT_MOST).is_ok());
    let at_cap = |len| format::RowFields { len, index_at: len - ENTRY as u64 - FOOTER as u64, ..good };
    assert_eq!(format::Row::new(at_cap(64 << 10), 3, 64 << 10).map(|_| ()), Ok(()));
    let cases = [
        (format::RowFields { len: (22 << 30) + 16 + ENTRY as u64 + FOOTER as u64, index_at: (22 << 30) + 16, ..good }, RowRefused::TooLong { len: (22 << 30) + 16 + ENTRY as u64 + FOOTER as u64, most: SEGMENT_MOST }),
        (format::RowFields { index_at: HEADER as u64 + 17, len: HEADER as u64 + 17 + ENTRY as u64 + FOOTER as u64, ..good }, RowRefused::Unaligned { index_at: HEADER as u64 + 17, align: 16 }),
        (format::RowFields { len: good.len + 16, ..good }, RowRefused::NotItsEnd { index_at: good.index_at, count: 1, end: Some(good.len), len: good.len + 16 }),
        (format::RowFields { generation: 4, ..good }, RowRefused::NewerGeneration { row: 4, manifest: 3 }),
        (format::RowFields { touched: 9, ..good }, RowRefused::NewerTouch { row: 9, manifest: 3 }),
        (format::RowFields { flags: 8, ..good }, RowRefused::Flags { flags: 8 }),
    ];
    for (fields, why) in cases {
        assert_eq!(format::Row::new(fields, 3, SEGMENT_MOST).map(|_| ()), Err(why), "{fields:?}");
    }
    // The manifest of generation 403 naming the 22 GiB copy first: refused at encode, so never
    // written; and the bytes the old encode wrote are refused at decode, naming the segment.
    let copy = format::Row::new(format::RowFields { len: 64 << 10, index_at: (64 << 10) - ENTRY as u64 - FOOTER as u64, generation: 1, touched: 1, ..good }, 403, 64 << 10).expect("a row under a 64 KiB cap").segment();
    let manifest = format::Manifest { generation: 403, segments: { let mut it = Vec::new_in(Global); it.push(copy); it }, ..format::Manifest::default() };
    let bytes = manifest.encode(64 << 10).expect("under its cap");
    let segment = copy.id;
    assert_eq!(manifest.encode(32 << 10), Err(StoreError::Row { generation: 403, segment, at: 32, why: RowRefused::TooLong { len: 64 << 10, most: 32 << 10 } }));
    assert_eq!(format::Manifest::decode(&bytes, 32 << 10), Err(StoreError::Row { generation: 403, segment, at: 32, why: RowRefused::TooLong { len: 64 << 10, most: 32 << 10 } }));
    assert_eq!(format::Manifest::decode(&bytes, 64 << 10).map(|it| it.segments.len()), Ok(1));
}

/// THE INVARIANT on the real store, through power cuts and failed effects.
#[test]
fn tiered_cleanup_keeps_the_invariant_through_cuts_and_failures() {
    blaze_sim::check(SimConfig::DEFAULT, |tape| {
        let seed = tape.next_u64("sim seed");
        let commands = tape.int_in(1..=40u32, "commands");
        let durable = tape.int_in(0..=1u32, "durable") == 1;
        let cut = if durable && tape.int_in(0..=1u32, "power cut") == 1 { Some(tape.int_in(0..=4000u32, "cut after")) } else { None };
        let fail = if tape.int_in(0..=3u32, "failing effects") == 0 { 40 } else { 0 };
        let spread = tape.int_in(0..=1u32, "spread over namespaces") == 1;
        let got = tiered_history(seed, commands, cut, fail, durable, Plant::None, spread, false);
        assert!(got.is_ok(), "seed {seed}, {commands} commands, durable {durable}, cut {cut:?}, fail 1 in {fail}, spread {spread}: {:?}", got.err());
    });
}

/// A long tiered history on the real store, 150 commands, a cut in the middle: merges run,
/// the invariant holds after each; prints what they did.
#[test]
fn a_long_tiered_history_keeps_the_invariant() {
    let mut extra = 0;
    for seed in 0..6 {
        // Seeds 0 to 3 local alone, as before; 4 and 5 spread over the three namespaces.
        let spread = seed >= 4;
        let log = tiered_history(seed, 150, Some(3000 + seed as u32 * 1000), 0, true, Plant::None, spread, false).unwrap_or_else(|why| panic!("seed {seed}: {why}"));
        assert!(log.merges > 0, "seed {seed}: no merge ran: the history did not cover one");
        // The meter's pass counts only if it saw copies past the buffer (rule 61).
        assert!(log.over_buffer > 0, "seed {seed}: no copy was larger than the merge buffer: the meter checked nothing it could catch");
        assert!(log.over_index > 0, "seed {seed}: no copy's index was larger than one index chunk: the meter checked nothing it could catch");
        extra += log.extra_slices;
        println!(
            "tiers on the store: seed {seed}, spread {spread}: {} cleanups checked, {} merges, at most {} local segments named, {:.2} bytes merged a byte committed; {} copies past the {TEST_MERGE_BUFFER}-byte buffer, at most {} bytes of records in flight; {} indexes past the {TEST_INDEX_CHUNK}-byte chunk, at most {} bytes of index in flight; {} slices past a cleanup's first, {} cleanups left a remainder",
            log.cleanups,
            log.merges,
            log.most_named,
            log.merged as f64 / log.written.max(1) as f64,
            log.over_buffer,
            log.alive_most,
            log.over_index,
            log.index_alive_most,
            log.extra_slices,
            log.left_over
        );
    }
    // The multi-namespace cleanup's pass counts only if one ran past its first slice.
    assert!(extra > 0, "no cleanup of the spread histories ran a second slice: the loop was not covered");
}

/// Control 3: the old copy, every record of a merge in one write held whole (the planted
/// `Plant::MergeWholeBatch`), is caught by the merge meter on a merge over the buffer.
#[test]
fn the_merge_meter_catches_a_whole_batch_copy() {
    let mut caught = None;
    for seed in 0..8 {
        if let Err(why) = tiered_history(seed, 60, None, 0, false, Plant::MergeWholeBatch, false, false) {
            assert!(why.contains("the merge meter"), "seed {seed}: the planted copy failed another oracle first: {why}");
            caught = Some(why);
            break;
        }
    }
    assert!(caught.is_some(), "no seed of 8 caught a merge copied whole");
}

/// Control 3b: the index held whole (the planted `Plant::MergeWholeIndex`: every entry built at
/// once and written as one chunk, as the copy did before its k-way merge) is caught by the
/// merge meter on a merge of more records than one index chunk holds.
#[test]
fn the_merge_meter_catches_a_whole_index() {
    let mut caught = None;
    for seed in 0..8 {
        if let Err(why) = tiered_history(seed, 60, None, 0, false, Plant::MergeWholeIndex, false, false) {
            assert!(why.contains("the merge meter") && why.contains("INDEX_CHUNK"), "seed {seed}: the planted index failed another oracle first: {why}");
            caught = Some(why);
            break;
        }
    }
    assert!(caught.is_some(), "no seed of 8 caught a merge's index held whole");
}

/// Control 4: a cleanup that stops after its first slice with budget left (the planted
/// `Plant::CleanupOneSlice`, one namespace a command as before) leaves another namespace over
/// its bound, which the invariant's oracle allows only on the budget: caught.
#[test]
fn the_tier_oracle_catches_a_cleanup_of_one_namespace() {
    let mut caught = None;
    for seed in 0..8 {
        if let Err(why) = tiered_history(seed, 150, None, 0, false, Plant::CleanupOneSlice, true, false) {
            assert!(why.contains("past K") || why.contains("past the bound"), "seed {seed}: the planted cleanup failed another oracle first: {why}");
            caught = Some(why);
            break;
        }
    }
    assert!(caught.is_some(), "no seed of 8 caught a cleanup of one namespace");
}

/// THE INVARIANT in sweep mode (`Liveness::Reached`): a history whose every segment holds more
/// litter than live bytes ([`Sweeping`]), so a sparse row is always there to sweep, still has
/// every namespace within its bound after every cleanup, the merge planned before the sweep.
/// Its pass counts only if both kinds of slice ran (rule 61).
#[test]
fn sweep_mode_with_a_sparse_row_always_keeps_the_invariant() {
    for seed in 0..4 {
        let log = tiered_history(seed, 150, None, 0, true, Plant::None, false, true).unwrap_or_else(|why| panic!("seed {seed}: {why}"));
        assert!(log.merges > 0, "seed {seed}: no tiered merge ran in sweep mode: the history did not cover one");
        assert!(log.sweeps > 0, "seed {seed}: no sweep ran: no sparse row was ever there, the history did not cover the case");
        println!("sweep mode: seed {seed}: {} cleanups checked, {} merges, {} sweeps, at most {} local segments named, {} slices past a cleanup's first", log.cleanups, log.merges, log.sweeps, log.most_named, log.extra_slices);
    }
}

/// Control 5: the sweep planned before the merge, and no slice after a sweep (the planted
/// `Plant::SweepBeforeMerge`, as `plan_reached` was), starves the merge while a sparse row is
/// always there: caught by the invariant's oracle after a cleanup that stopped after a sweep.
#[test]
fn the_tier_oracle_catches_a_sweep_that_starves_the_merge() {
    let mut caught = None;
    for seed in 0..8 {
        if let Err(why) = tiered_history(seed, 150, None, 0, true, Plant::SweepBeforeMerge, false, true) {
            assert!(why.contains("past K") || why.contains("past the bound"), "seed {seed}: the planted sweep failed another oracle first: {why}");
            caught = Some(why);
            break;
        }
    }
    assert!(caught.is_some(), "no seed of 8 caught a sweep that starves the merge");
}

/// One cleanup that merges `K + 1` one-record segments, its commit's completions taken one by
/// one; the power cut the moment the commit is told (what the flip asked after, the plant's
/// late sync of the merge, has not completed); then a reader of a new boot and the oracle,
/// the merged records landed in the copy.
fn merge_cut_as_told(seed: u64, plant: Plant) -> Result<(), String> {
    let root = test_root();
    let mut sim = Sim::new(seed, Faults::default());
    let mut ledger = Ledger::default();
    let mut log = TierLog::default();
    let mut next = 0u32;
    let mut a = tiered_actor(10, 1, true, plant, false, &root);
    settle(&mut sim, std::slice::from_mut(&mut a));
    let mids = [1u32; TIER_SEGMENTS_MOST + 1];
    assert!(matches!(tier_commits(&mut sim, &mut a, &mut ledger, &mut log, &mut next, &mids, false, None, &mut None)?, Ran::Settled));
    a.store.fold_slice(&mut a.effects).map_err(|it| it.to_string())?;
    settle(&mut sim, std::slice::from_mut(&mut a));
    assert!(a.store.slice_parked(), "K + 1 segments of tier 0 and no merge parked");
    let before: Vec<format::Segment> = a.store.named_rows().to_vec();
    let keys = local_keys(&mut next, 1);
    for &n in &keys {
        a.store.put(key(n), space(n), TAG, &value(n)).map_err(|it| it.to_string())?;
        ledger.maybe.insert(n, space(n));
    }
    a.store.commit(&[], sim.wall_ns(), &mut a.effects).map_err(|it| it.to_string())?;
    let mut told = false;
    let mut done = Vec::new_in(Global);
    'run: for _ in 0..100_000 {
        sim.submit(&mut a.effects);
        assert!(sim.in_flight() > 0, "the commit settled without being told");
        sim.poll(&mut done, Deadline::after(sim.now(), Duration::from_secs(1)));
        let wall = sim.wall_ns();
        for completion in done.drain(..) {
            a.store.complete(completion, wall, &mut a.effects);
            if let Some(committed) = a.store.take_committed() {
                let committed = committed.map_err(|it| it.to_string())?;
                tier_landed(&mut ledger, &mut log, &a.store, &keys, &committed)?;
                tier_oracles(&a.store, &before, &committed, &mut ledger, &mut log)?;
                told = true;
                break 'run;
            }
        }
    }
    assert!(told, "the commit was never told");
    assert_eq!(log.merges, 1, "the cleanup's flip did not name the merge");
    drop(a);
    sim.crash();
    let r = reader(&mut sim, 1, 2, &root);
    oracle(&r.store, &ledger, true, true)
}

/// Control 2: a merged segment handed to the flip before its sync (`Synced` makes that a
/// planted path alone) is caught by the crash oracle: a power cut after the flip tears the
/// copy whose inputs the generation retired.
#[test]
fn the_crash_oracle_catches_a_merge_flipped_before_its_sync() {
    let mut caught = None;
    for seed in 0..32 {
        let honest = merge_cut_as_told(seed, Plant::None);
        assert!(honest.is_ok(), "seed {seed} without the plant: {honest:?}");
        if caught.is_some() {
            continue;
        }
        caught = match std::panic::catch_unwind(move || merge_cut_as_told(seed, Plant::MergeFlipsBeforeSync)) {
            Ok(Ok(())) => None,
            Ok(Err(why)) => Some(why),
            Err(panic) => Some(panic.downcast_ref::<String>().cloned().unwrap_or_else(|| "a panic".into())),
        };
    }
    assert!(caught.is_some(), "no seed of 32 caught a merge flipped before its sync");
}

/// The interpolation search over ranged reads finds every key of a sorted run and no other, in
/// a bounded number of reads.
#[test]
fn a_ranged_index_probe_finds_every_key_in_few_reads() {
    let mut entries = Vec::new_in(Global);
    for n in 0..20_000u32 {
        entries.push(Entry { key: key(n).0, tag: TAG, at: HEADER as u64, len: 0, crc: 0 });
    }
    let run = format::sorted_run(entries).unwrap();
    let count = (run.len() / ENTRY) as u32;
    let segment = SegmentId(ContentId([9; 32]));
    let mut most = 0;
    for n in (0..20_000u32).step_by(97).chain([20_001, 30_000]) {
        let mut reads = 0;
        let found = format::find_ranged(segment, count, &key(n).0, |at, out| {
            reads += 1;
            out.copy_from_slice(&run[at as usize..at as usize + out.len()]);
            Ok(())
        })
        .unwrap();
        assert_eq!(found.map(|it| (it.1.key, it.1.tag)), (n < 20_000).then_some((key(n).0, TAG)), "key {n}");
        most = most.max(reads);
    }
    assert!(most <= 3, "a probe took {most} ranged reads of a 20,000-entry run");
}

/// A large sorted index keeps each content lookup to a small, bounded number of row probes.
#[test]
fn a_hundred_thousand_row_gets_use_a_small_probe_bound() {
    const ROWS: u32 = 100_000;
    let run = run_of(ROWS);
    let count = ROWS as usize;
    let prefix_at = |i: usize| blaze_format::rows::prefix(&run[i * ENTRY..i * ENTRY + 8]);
    // Exact-division control from 75ce877461: five guesses, a 16-row fence, then bisection.
    // The key's known rank gives the result of each comparison without another lookup.
    let exact_probes = |target: usize| {
        let want = prefix_at(target);
        let (mut lo, mut hi) = (0, count);
        let (mut least, mut most) = (0, u64::MAX);
        let (mut guesses, mut probes, mut moved_lo) = (0, 0u32, false);
        loop {
            let at = if guesses < 5 && most > least {
                guesses += 1;
                let span = u128::from(most - least) + 1;
                lo + ((u128::from(want - least) * (hi - lo) as u128) / span) as usize
            } else if guesses == 5 && hi - lo > 32 {
                guesses += 1;
                if moved_lo { lo + 16 } else { hi - 1 - 16 }
            } else {
                lo + (hi - lo) / 2
            };
            probes += 1;
            match at.cmp(&target) {
                std::cmp::Ordering::Equal => break probes,
                std::cmp::Ordering::Less => {
                    lo = at + 1;
                    least = prefix_at(at);
                    moved_lo = true;
                }
                std::cmp::Ordering::Greater => {
                    hi = at;
                    most = prefix_at(at);
                    moved_lo = false;
                }
            }
        }
    };
    let (mut most, mut exact_most, mut total) = (0, 0, 0);
    for n in 0..ROWS {
        let key = key(n).0;
        let mut probes = 0u32;
        let at = format::find_by(count, &key, |at| {
            probes += 1;
            &run[at * ENTRY..at * ENTRY + 32]
        })
        .expect("a stored key was not found");
        assert_eq!(format::key_at(&run, at), key);
        most = most.max(probes);
        exact_most = exact_most.max(exact_probes(at));
        total += probes;
    }
    assert!(most <= exact_most, "a lookup took {most} row probes, exact interpolation took at most {exact_most}");
    assert!(total <= 6 * ROWS, "{total} row probes for {ROWS} lookups");
}

/// A test's builder: its "signature" is a keyed hash of the statement's body under a secret only
/// it knows, standing in for the remote lane's real signature; its public key is a hash of the
/// secret. As a [`Verifier`] it trusts its own statements and no one else's.
struct Builder {
    secret: &'static str,
}

impl Builder {
    fn public(&self) -> [u8; 32] {
        ContentId::of("plain store test signer", self.secret.as_bytes()).0
    }

    fn sign(&self, body: &[u8; Statement::BODY]) -> [u8; 64] {
        let mut bytes = Vec::new_in(Global);
        bytes.extend_from_slice(self.secret.as_bytes());
        bytes.extend_from_slice(body);
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&ContentId::of("plain store test signature 0", &bytes).0);
        out[32..].copy_from_slice(&ContentId::of("plain store test signature 1", &bytes).0);
        out
    }

    /// Its signed statement over the segment `bytes`, of `namespace`.
    fn statement(&self, namespace: Namespace, bytes: &[u8]) -> Statement {
        let described = check_segment(bytes).unwrap();
        let mut it = Statement {
            namespace,
            segment: SegmentId(described.digest),
            len: SegmentLen::new(bytes.len() as u64).unwrap(),
            toolchain: ContentId([7; 32]),
            records: described.count,
            tier: Tier::Top,
            issued: 1,
            signer: self.public(),
            signature: [0; 64],
        };
        it.signature = self.sign(&it.body());
        it
    }
}

impl Verifier for Builder {
    fn verify(&self, statement: &Statement, segment: &SegmentRef) -> Result<(), Refused> {
        if statement.signer != self.public() {
            return Err(Refused::Untrusted { signer: statement.signer });
        }
        if statement.signature != self.sign(&statement.body()) {
            return Err(Refused::Signature { signer: statement.signer });
        }
        if statement.segment != segment.digest() {
            return Err(Refused::Segment { found: statement.segment, wanted: segment.digest() });
        }
        if statement.len != segment.segment_len() {
            return Err(Refused::Length { found: statement.len, wanted: segment.segment_len() });
        }
        Ok(())
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("blaze-plain-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A store over the simulation, through the driver, reading `reads`.
fn driven(n: u64, reads: Vec<Namespace>, root: &Frame) -> Driven<Sim, &Frame> {
    let mut c = cfg(n, 1, false, root);
    c.reads = spaces_in(reads, root);
    Driven::with(Sim::new(n, Faults::default()), Path::new("/sim"), c, root).unwrap()
}

/// The record under `k` through `d`'s get (mapping at a first probe) and got: its bytes and
/// namespace.
fn read(d: &mut Driven<Sim, &Frame>, k: &Key) -> Option<(Vec<u8>, Namespace)> {
    let found = d.get(k).expect("a get with no failure")?;
    let got = d.got(found);
    Some((got.bytes.to_vec(), got.namespace))
}

fn refs(keys: std::ops::Range<u32>) -> Vec<RecordRef> {
    keys.map(|n| RecordRef { tag: TAG, key: key(n).0 }).collect()
}

/// A trusted builder's segment in the public namespace, its bytes as they lie, in a source; the
/// bytes are also [`seal_segment`]'s of the same records (the commit and the push's sealing are
/// one writer).
fn published(source: &LocalDir, builder: &Builder, root: &Frame) -> (Vec<u8>, ContentId) {
    let mut ci = driven(0, reads(), root);
    for n in 0..8 {
        ci.put(key(n), Namespace::PUBLIC, TAG, &value(n)).unwrap();
    }
    let committed = ci.commit_with(&[]).unwrap();
    assert_eq!(committed.segments.len(), 1);
    let (seg, bytes) = ci.core().segment(committed.segments[0]).unwrap();
    let bytes = bytes.unwrap().to_vec();
    let values: Vec<Vec<u8>> = (0..8).map(value).collect();
    let records: Vec<(RecordRef, &[u8])> = (0..8).map(|n| (RecordRef { tag: TAG, key: key(n).0 }, &values[n as usize][..])).collect();
    let mut sealed = Vec::new_in(Global);
    seal_segment(Namespace::PUBLIC, &records, &mut sealed).unwrap();
    assert_eq!(sealed, bytes, "seal_segment and the commit wrote different bytes for the same records");
    source.put(&bytes, &builder.statement(Namespace::PUBLIC, &bytes)).unwrap();
    (bytes, seg.digest)
}

/// A pulled segment is the bytes a builder wrote, mapped as a local one, under its namespace
/// alone; a forged one (bytes changed under a real statement, or a statement nobody trusted
/// signed, or one whose namespace was changed) is refused before it is written; and a key is
/// never answered from a namespace the caller did not ask for.
#[test]
fn a_fetched_segment_is_verified_before_it_is_used() {
    let root = test_root();
    let builder = Builder { secret: "the trusted builder's secret" };
    let at = scratch("fetch");
    let mut source = LocalDir::new(&at);
    let (bytes, digest) = published(&source, &builder, &root);
    let keys = refs(0..8);

    let mut me = driven(1, reads(), &root);
    assert_eq!(read(&mut me, &key(0)).map(|it| it.0.len()), None);
    let pulled = me.fetch_missing(&mut source, &builder, Namespace::PUBLIC, &keys).unwrap();
    assert_eq!(pulled.len(), 1);
    for n in 0..8 {
        assert_eq!(read(&mut me, &key(n)), Some((value(n), Namespace::PUBLIC)), "pulled record {n}");
        assert_eq!(me.core().get_in(&[Namespace::LOCAL, org()], &key(n)).map(|it| it.bytes.len()), None, "pulled record {n} answers for another namespace");
    }
    assert!(me.fetch_missing(&mut source, &builder, Namespace::PUBLIC, &keys).unwrap().is_empty(), "held records fetched again");

    // Forged: one payload byte changed, under the real name and statement.
    let mut forged = bytes.clone();
    forged[HEADER] ^= 1;
    let hex: String = digest.0.iter().map(|it| format!("{it:02x}")).collect();
    std::fs::write(at.join(format!("{hex}.seg")), &forged).unwrap();
    let mut other = driven(2, vec![Namespace::LOCAL, Namespace::PUBLIC], &root);
    let why = other.fetch_missing(&mut source, &builder, Namespace::PUBLIC, &keys).expect_err("a forged segment is refused");
    assert!(matches!(why, StoreError::Layout { segment, .. } if segment == SegmentId(digest)), "{why}");
    assert_eq!(read(&mut other, &key(0)).map(|it| it.0.len()), None, "nothing of it was used");

    // A whole, consistent segment no trusted builder signed.
    std::fs::write(at.join(format!("{hex}.seg")), &bytes).unwrap();
    let someone = Builder { secret: "someone else" };
    let why = other.fetch_missing(&mut source, &someone, Namespace::PUBLIC, &keys).expect_err("an unsigned segment is refused");
    assert_eq!(why, StoreError::Refused { segment: SegmentId(digest), why: Refused::Untrusted { signer: builder.public() } });

    // A namespace the store does not read is never asked of a source, nor answered from.
    let why = other.fetch_missing(&mut source, &builder, org(), &keys).expect_err("a namespace the store does not read");
    assert!(matches!(why, StoreError::Other { .. }), "{why}");
    let mut found: Vec<Option<Located>> = vec![None; keys.len()];
    source.find(Namespace::LOCAL, &keys, &mut found).unwrap();
    assert!(found.iter().all(Option::is_none), "a public segment answered for another namespace");

    // A source that lies about the namespace: it answers a find of `LOCAL` with the public
    // segment, and (`rewrite`) changes the statement's namespace to match.
    struct Liar {
        inner: LocalDir,
        rewrite: bool,
    }
    impl Source for Liar {
        fn find(&mut self, _: Namespace, keys: &[RecordRef], found: &mut [Option<Located>]) -> Result<(), StoreError> {
            self.inner.find(Namespace::PUBLIC, keys, found)
        }
        fn fetch_into(&mut self, segment: &SegmentRef, range: std::ops::Range<u64>, out: &mut [u8]) -> Result<(), StoreError> {
            self.inner.fetch_into(segment, range, out)
        }
        fn statement(&mut self, segment: &SegmentRef) -> Result<Statement, StoreError> {
            let mut it = self.inner.statement(segment)?;
            if self.rewrite {
                it.namespace = Namespace::LOCAL;
            }
            Ok(it)
        }
    }
    let wanted = SegmentId(digest);
    // The statement names its namespace: refused before a byte is fetched.
    let mut liar = Liar { inner: LocalDir::new(&at), rewrite: false };
    let why = other.fetch_missing(&mut liar, &builder, Namespace::LOCAL, &keys).expect_err("a segment of another namespace than asked");
    assert_eq!(why, StoreError::Refused { segment: wanted, why: Refused::Namespace { found: Namespace::PUBLIC, wanted: Namespace::LOCAL } });
    // The signature covers the namespace: a statement changed to match is refused.
    let mut liar = Liar { inner: LocalDir::new(&at), rewrite: true };
    let why = other.fetch_missing(&mut liar, &builder, Namespace::LOCAL, &keys).expect_err("a statement changed after it was signed");
    assert_eq!(why, StoreError::Refused { segment: wanted, why: Refused::Signature { signer: builder.public() } });
    // With a verifier that trusts every statement (one that is wrong or compromised), the
    // namespace written in the segment's own bytes still refuses it.
    struct TrustsAll;
    impl Verifier for TrustsAll {
        fn verify(&self, _: &Statement, _: &SegmentRef) -> Result<(), Refused> {
            Ok(())
        }
    }
    let why = other.fetch_missing(&mut liar, &TrustsAll, Namespace::LOCAL, &keys).expect_err("a segment whose bytes say another namespace");
    assert_eq!(why, StoreError::Namespace { found: Namespace::PUBLIC, wanted: Namespace::LOCAL });
    assert_eq!(read(&mut other, &key(0)).map(|it| it.0.len()), None, "nothing of the lie was used");
    let _ = std::fs::remove_dir_all(&at);
}

// ---- The exit tail, the endpoint-free lookup and sealed sharing (store-next.md) ----

/// This machine's path as the tests' stores know it ([`PlainConfig::machine`]).
const MACHINE: &[u8] = b"/Users/someone/checkout";

/// One history of the exit tail. Writer 0 commits and, in the rounds between, seals (the tail
/// of a command that answered) or runs a compaction slice; writer 1 commits every round; a
/// reader of boot 1 opens after the first round and holds what it mapped. The power cut after
/// `cut_after` completions when given, else a last process of boot 1 seals what is left (an
/// orderly exit, with the sealer's `plant`). Then always a power cut and a new boot, whose
/// reader is held to the oracle and to two more promises: a segment proved by its sealed row
/// is whole on the disk, and when the boot's last seal survived, the new boot's open checks no
/// segment in full (it reads no segment's bytes to prove it).
fn sealing(seed: u64, rounds: u32, per: u32, cut_after: Option<u32>, fail_one_in: u32, plant: Plant) -> Result<(), String> {
    // A run of its own, its stores in a frame of this run.
    let root = test_root();
    let mut sim = Sim::new(seed, Faults { fail_one_in, ..Faults::default() });
    let mut actors = vec![Actor::new(cfg(0, 1, false, &root), &root, plant), Actor::new(cfg(1, 1, false, &root), &root, Plant::None)];
    let mut budget = cut_after;
    let mut ledger = Ledger { never: vec![500_000, 1_500_000], ..Ledger::default() };
    let mut next = [0u32, 1_000_000];
    let mut pending: [Option<Vec<u32>>; 2] = [None, None];
    let mut early: Option<(Actor<'_>, Vec<u32>)> = None;
    let mut cut = false;
    for round in 0..=rounds {
        for (i, actor) in actors.iter_mut().enumerate() {
            if round == rounds || !actor.store.ready() || pending[i].is_some() {
                continue;
            }
            if i == 0 && round % 2 == 1 {
                let _asked = if round % 3 == 0 { actor.store.compact(&mut actor.effects) } else { actor.store.seal(&mut actor.effects) };
                continue;
            }
            let keys: Vec<u32> = (0..per).map(|k| next[i] + k).collect();
            next[i] += per;
            for &n in &keys {
                actor.store.put(key(n), space(n), TAG, &value(n)).map_err(|it| it.to_string())?;
                ledger.maybe.insert(n, space(n));
            }
            actor.store.commit(&[],sim.wall_ns(), &mut actor.effects).map_err(|it| it.to_string())?;
            pending[i] = Some(keys);
        }
        let settled = run(&mut sim, &mut actors, &mut budget);
        for (i, actor) in actors.iter_mut().enumerate() {
            let _slice = actor.store.take_sliced();
            if let Some(Ok(committed)) = actor.store.take_committed() {
                let keys = pending[i].take().expect("a commit was pending");
                ledger.land(&keys, &committed)?;
            }
        }
        if !settled {
            cut = true;
            break;
        }
        if let Some(why) = actors.iter().find_map(|it| it.store.broken())
            && !injected(why)
        {
            return Err(format!("a writer broke on its own: {why}"));
        }
        if round == 0 && early.is_none() {
            let held: Vec<u32> = ledger.committed.keys().copied().collect();
            early = Some((reader_with(&mut sim, 5, 1, Plant::None, &root), held));
        }
    }
    drop(actors);
    // The generation the boot's last seal wrote, when it ran and completed.
    let mut sealed_at = None;
    if !cut {
        let mut last = reader_with(&mut sim, 6, 1, plant, &root);
        if last.store.seal(&mut last.effects) {
            settle(&mut sim, std::slice::from_mut(&mut last));
        }
        if last.store.broken().is_none() {
            sealed_at = Some(last.store.generation());
        }
    }
    // A reader of boot 1 keeps reading what it mapped, through seals and compaction.
    if let Some((r, held)) = &early {
        for &n in held {
            if r.store.get(&key(n)).map(|it| it.bytes.to_vec()) != Some(value(n)) {
                return Err(format!("record {n}, held by a reader of boot 1, no longer reads back under it"));
            }
        }
    }
    drop(early);
    sim.crash();
    let r = reader_with(&mut sim, 2, 2, Plant::None, &root);
    // 2026-11-20 (D182 lane): what a planted run reached, printed so a control that catches
    // nothing says which step its plant never got to (the seal flipped, its slot kept, a
    // segment trusted by it). Shown only when the control fails.
    if plant != Plant::None {
        println!(
            "sealing seed {seed}, {plant:?}: cut {cut}, last seal at {sealed_at:?}, next boot read generation {:?}, proved by seal {:x?}, sealed rows {:x?}, checked in full {:x?}, lost {:?}",
            r.store.report().whole,
            r.store.proved_by_seal(),
            r.store.sealed_segments(),
            r.store.fully_checked(),
            r.store.report().lost.iter().map(|it| (it.segment, it.why.as_str())).collect::<Vec<_>>(),
        );
    }
    oracle(&r.store, &ledger, true, false)?;
    for id in r.store.proved_by_seal() {
        let bytes = sim.contents(&FileName::new(&format!("plain/seg-{id:016x}")).expect("a name"));
        if let Err(why) = check_segment(&bytes) {
            return Err(format!("segment {id:016x} was proved by its sealed row and is not whole on the disk: {why}"));
        }
    }
    if let Some(at) = sealed_at
        && r.store.report().whole.is_some_and(|whole| whole >= at)
        && !r.store.fully_checked().is_empty()
    {
        return Err(format!("the boot's last seal (generation {at}) survived, and the next boot checked {:x?} in full", r.store.fully_checked()));
    }
    Ok(())
}

/// The invariant of the exit tail: interleaved writers, seals and compaction slices, effects
/// failing, a power cut anywhere (the seal's barriers and flip among them): every committed
/// record reads back or is reported lost by name, nothing sealed is torn, and an orderly exit
/// leaves the next boot nothing to check.
#[test]
fn a_sealed_tail_survives_any_power_cut_and_is_never_read_to_prove() {
    blaze_sim::check(SimConfig::DEFAULT, |tape| {
        let seed = tape.next_u64("sim seed");
        let rounds = tape.int_in(1..=7u32, "rounds");
        let per = tape.int_in(1..=12u32, "records per round");
        let cut = tape.int_in(0..=1u32, "power cut") == 1;
        let after = tape.int_in(0..=300u32, "cut after");
        let fail = if tape.int_in(0..=3u32, "failing effects") == 0 { 40 } else { 0 };
        let outcome = sealing(seed, rounds, per, cut.then_some(after), fail, Plant::None);
        assert!(outcome.is_ok(), "seed {seed}, {rounds} rounds of {per}, cut {cut} after {after}, fail 1 in {fail}: {outcome:?}");
    });
}

/// The control: a seal that flips before its barriers names segments sealed whose bytes the
/// power cut may still tear; the oracle catches it for some seed of an orderly exit.
///
/// Three rounds, not four: [`sealing`]'s round 3 is a compaction, and since the size-tiered
/// merge (tier 0 merged at `K + 1` = 5 rows, which rounds 0 to 2 make) it copies every segment
/// the planted seal of round 1 named into one new, unsealed segment and removes them, before
/// the cut: what the plant sealed is gone, and only the last seal's segments (its slot the head's
/// last write, which a cut rarely keeps) were left to tear; no seed of 64 caught it
/// (2026-11-20; under the former policy, merging only past 16 segments, round 3's compaction
/// did nothing). With three rounds no compaction runs, and round 1's planted seal names round
/// 0's segments, unsynced, under every later generation, as it did before. The printed line
/// of each planted run ([`sealing`]) says what the plant reached.
#[test]
fn the_oracle_catches_a_seal_before_its_barrier() {
    let mut caught = None;
    for seed in 0..64 {
        let honest = sealing(seed, 3, 6, None, 0, Plant::None);
        assert!(honest.is_ok(), "seed {seed} without the plant: {honest:?}");
        if caught.is_none() {
            caught = sealing(seed, 3, 6, None, 0, Plant::SealBeforeBarrier).err();
        }
    }
    assert!(caught.is_some(), "no seed of 64 caught a seal flipped before its barriers");
}

/// One history of the background pass (store-next.md, "the bound"): boot 1 commits `segments`
/// local segments of eight records each and seals them; the power is cut, and one byte of the
/// last non-empty record (in index order) of one segment rots on the disk. At boot 2 a holder
/// opens (every segment proved by its seal, no byte read) and holds a record of the rotted
/// segment's mapping; then durable processes of boot 2 walk the pass one record a slice, the
/// power cut after `cut_after` completions once when it is given (the next process resumes).
/// The oracle: no segment is walked again once its walk's flip landed, none is skipped, the
/// rot is found, lost by name and retired so no lookup serves it, the holder's bytes stay as
/// they were mapped, every other committed record reads back, and boot 3 walks the survivors
/// again.
fn scrubbing(seed: u64, segments: u32, cut_after: Option<u32>, plant: Plant) -> Result<(), String> {
    // A run of its own, its stores in a frame of this run.
    let root = test_root();
    let mut sim = Sim::new(seed, Faults::default());
    let mut ledger = Ledger::default();
    let mut w = [Actor::new(cfg(0, 1, true, &root), &root, Plant::None)];
    settle(&mut sim, &mut w);
    let mut keys_of = BTreeMap::new();
    for s in 0..segments {
        let range = s * 8..s * 8 + 8;
        keys_of.insert(commit_local(&mut sim, &mut w, range.clone(), &mut ledger), range);
    }
    assert!(w[0].store.seal(&mut w[0].effects), "boot 1 has a tail to seal");
    settle(&mut sim, &mut w);
    let rotted = *keys_of.keys().nth((seed % u64::from(segments)) as usize).expect("a segment");
    // The last record in index order with a payload: a pass that stops after its first slice
    // never reaches it.
    let (row, bytes) = w[0].store.segment(rotted).expect("named");
    let index = row.run(bytes.expect("mapped here"));
    let target = (0..row.count as usize).rev().map(|i| Entry::read(&index[i * ENTRY..(i + 1) * ENTRY])).find(|it| it.len > 0).expect("a record with a payload");
    let bad = keys_of[&rotted].clone().find(|&n| key(n).0 == target.key).expect("the record's key");
    drop(w);
    sim.crash();
    let file = FileName::new(&format!("plain/seg-{rotted:016x}")).expect("a name");
    let was = sim.contents(&file)[target.at as usize];
    sim.rot(&file, target.at, &[!was]);

    let holder = reader(&mut sim, 1, 2, &root);
    assert_eq!(holder.store.proved_by_seal().len(), segments as usize, "boot 2 proves every segment by its seal");
    let kept = keys_of[&rotted].clone().find(|&n| n != bad && !value(n).is_empty()).expect("another record");
    let held = holder.store.get(&key(kept)).expect("mapped by the holder").bytes;

    // Walked to its end by some process (its flip may have been cut), and walked to its end
    // with its flip landed (a later process of the boot must not walk it again).
    let mut ended: BTreeSet<u64> = BTreeSet::new();
    let mut landed: BTreeSet<u64> = BTreeSet::new();
    let mut lost_by_name = false;
    let mut budget = cut_after;
    let mut n = 2;
    let last = 'pass: loop {
        let mut b = [Actor::new(cfg(n, 2, true, &root), &root, plant)];
        n += 1;
        settle(&mut sim, &mut b);
        if let Some(why) = b[0].store.broken() {
            return Err(format!("a scrubber's open broke: {why}"));
        }
        // A rot found by a process the cut stopped after its flip landed: reported from its
        // mark by the next open.
        lost_by_name |= named_lost(&b[0].store).contains(&rotted);
        loop {
            let Some(slice) = b[0].store.scrub_next(1, &mut b[0].effects) else { break 'pass b };
            if landed.contains(&slice.segment) {
                return Err(format!("segment {:016x} walked again after its walk's flip landed", slice.segment));
            }
            if slice.done {
                ended.insert(slice.segment);
            }
            if !run(&mut sim, &mut b, &mut budget) {
                // The power cut mid-pass: the next process of this boot resumes.
                budget = None;
                drop(b);
                sim.crash();
                continue 'pass;
            }
            if let Some(why) = b[0].store.broken() {
                return Err(format!("a scrubber broke on its own: {why}"));
            }
            if slice.done {
                landed.insert(slice.segment);
            }
            if let Some(rot) = slice.rot {
                if slice.segment != rotted || rot.key != key(bad) {
                    return Err(format!("the pass found rot in {:016x} ({rot:?}), planted only in record {bad} of {rotted:016x}", slice.segment));
                }
                lost_by_name |= named_lost(&b[0].store).contains(&rotted);
                if b[0].store.get(&key(bad)).is_some() {
                    return Err(format!("record {bad} served by the process that retired its segment"));
                }
            }
        }
    };
    let all: BTreeSet<u64> = keys_of.keys().copied().collect();
    if ended != all {
        return Err(format!("the pass skipped {:x?}", all.difference(&ended).collect::<Vec<_>>()));
    }
    if !lost_by_name {
        return Err(format!("the rot in record {bad} of segment {rotted:016x} was never found and lost by name"));
    }
    if last[0].store.get(&key(bad)).is_some() {
        return Err(format!("record {bad} of the retired segment {rotted:016x} is still served"));
    }
    drop(last);
    if held != value(kept).as_slice() {
        return Err(format!("record {kept}, held through the retire of {rotted:016x}, changed under its holder"));
    }
    let after = reader_with(&mut sim, 20, 2, Plant::None, &root);
    if after.store.get(&key(bad)).is_some() {
        return Err(format!("a later lookup of record {bad} hit the retired segment {rotted:016x}"));
    }
    if !after.store.unscrubbed().is_empty() {
        return Err(format!("boot 2's pass ended and {:x?} is still left this boot", after.store.unscrubbed()));
    }
    // `cut`, not durable: the rotted segment's records are lost, by name.
    oracle_with(&after.store, &ledger, true, false, &BTreeSet::from([rotted]))?;
    drop(after);
    drop(holder);
    sim.crash();
    let next = reader(&mut sim, 21, 3, &root);
    let survivors: BTreeSet<u64> = all.iter().copied().filter(|it| *it != rotted).collect();
    if next.store.unscrubbed().into_iter().collect::<BTreeSet<u64>>() != survivors {
        return Err(format!("boot 3 has {:x?} left to walk, not every surviving segment {survivors:x?}", next.store.unscrubbed()));
    }
    Ok(())
}

/// The bound of the accepted risk (store-next.md): a byte that rots in a sealed segment is
/// found by the boot's background pass and the segment retired, a crash anywhere in the pass
/// resumes it with nothing walked twice and nothing skipped, and a reader holding the
/// segment's mapping keeps its bytes.
#[test]
fn the_background_pass_retires_rot_and_resumes_after_a_cut() {
    blaze_sim::check(SimConfig::DEFAULT, |tape| {
        let seed = tape.next_u64("sim seed");
        let segments = tape.int_in(1..=4u32, "sealed segments");
        let cut = tape.int_in(0..=1u32, "power cut") == 1;
        let after = tape.int_in(0..=80u32, "cut after");
        let outcome = scrubbing(seed, segments, cut.then_some(after), Plant::None);
        assert!(outcome.is_ok(), "seed {seed}, {segments} segments, cut {cut} after {after}: {outcome:?}");
    });
}

/// The control: a pass that records a segment walked after its first slice never reaches the
/// rot in its last record; the oracle catches it.
#[test]
fn the_oracle_catches_a_segment_marked_scrubbed_before_its_walk_ends() {
    let mut caught = None;
    for seed in 0..16 {
        let honest = scrubbing(seed, 3, None, Plant::None);
        assert!(honest.is_ok(), "seed {seed} without the plant: {honest:?}");
        if caught.is_none() {
            caught = scrubbing(seed, 3, None, Plant::ScrubMarkedEarly).err();
        }
    }
    assert!(caught.is_some(), "no seed of 16 caught a segment marked scrubbed before its walk ended");
}

/// A key every worker puts each round, with equal bytes.
const SHARED: u32 = 900_000;

/// A durable store over `sim` behind the handle, opened with `plant` at `boot`.
fn handle(sim: Sim, n: u64, boot: u64, plant: Plant, root: &Frame) -> PlainStore<Sim, &Frame> {
    PlainStore::new(Driven::planted(sim, Path::new("/sim"), cfg(n, boot, true, root), root, plant).expect("an open with no cut and no failure")).expect("made whole with no cut and no failure")
}

/// One history of the lanes (store-next.md, "Puts on every worker"): `workers` lanes put into
/// one handle, each round its own keys and one key all of them put with equal bytes, in an
/// order the seed turns; a reader's lane that puts nothing looks before and after each
/// commit; the power is cut once where `cut_at` says, and the machine comes back
/// at a new boot. The oracle: a record whose commit's flip landed (the commit returned, or
/// the generation it flipped to is this process's) reads back after every cut; no record is
/// seen by another lane before its commit's flip; each worker sees its own puts at once; the
/// key every worker put is one record of equal bytes.
fn lanes(seed: u64, workers: u32, rounds: u32, cut_at: Option<Cut>, plant: Plant) -> Result<(), String> {
    // A run of its own: its stores, and the reads' copies, in a frame of this run.
    let root = test_root();
    let mut boot = 1;
    let mut store = handle(Sim::new(seed, Faults::default()), 1, boot, plant, &root);
    store.writer().cut(cut_at);
    let mut promised: BTreeSet<u32> = BTreeSet::new();
    let mut ids: Vec<LaneId> = (0..workers).map(|_| store.lane().expect("under the cap")).collect();
    let mut reader = store.lane().expect("under the cap");
    let mut cut = false;
    for round in 0..rounds {
        let mut put: Vec<u32, &Frame> = Vec::new_in(&root);
        for turn in 0..workers {
            let w = (turn + round + seed as u32) % workers;
            let lane = ids[w as usize];
            let mut keys: Vec<u32> = (0..3).map(|i| w * 10_000 + round * 10 + i).collect();
            keys.push(SHARED + round);
            for n in keys {
                let got = store.put_in(lane, key(n), Namespace::LOCAL, TAG, &value(n));
                if !matches!(got, Ok(Put::Written | Put::Same)) {
                    return Err(format!("worker {w}'s put of record {n}: {got:?}"));
                }
                if !store.get(Some(lane), &key(n), &root).is_some_and(|it| it.1 == value(n)) {
                    return Err(format!("worker {w} does not see its own put of record {n}"));
                }
                put.push(n);
            }
        }
        for &n in put.iter().filter(|n| !promised.contains(n)) {
            if store.get(Some(reader), &key(n), &root).is_some() {
                return Err(format!("record {n} seen by another lane before its commit's flip"));
            }
        }
        let before = store.writer().core().generation();
        match store.commit_lanes() {
            Ok(_) => {
                promised.extend(put.iter().copied());
                for &n in &put {
                    if !store.get(Some(reader), &key(n), &root).is_some_and(|it| it.1 == value(n)) {
                        return Err(format!("record {n} not seen by another lane after its commit returned"));
                    }
                }
            }
            Err(why) if !cut && cut_at.is_some() => {
                if store.writer().core().generation() > before {
                    // The flip landed before the cut: what it names is promised.
                    promised.extend(put.iter().copied());
                }
                let _ = why;
                cut = true;
                let mut sim = store.into_writer().into_driver();
                sim.crash();
                boot += 1;
                store = handle(sim, 2, boot, plant, &root);
                ids = (0..workers).map(|_| store.lane().expect("under the cap")).collect();
                reader = store.lane().expect("under the cap");
                for &n in &promised {
                    if !store.get(None, &key(n), &root).is_some_and(|it| it.1 == value(n)) {
                        return Err(format!("record {n}, promised by a flip that landed, does not read back after the power cut"));
                    }
                }
            }
            Err(why) => return Err(format!("a commit failed with no cut: {why}")),
        }
    }
    // Every promise kept after a last cut, and the shared keys one record each.
    let mut sim = store.into_writer().into_driver();
    sim.crash();
    let last = handle(sim, 99, boot + 1, Plant::None, &root);
    for &n in &promised {
        if !last.get(None, &key(n), &root).is_some_and(|it| it.1 == value(n)) {
            return Err(format!("record {n}, promised, does not read back at the next boot"));
        }
    }
    let writer = last.writer();
    for id in writer.core().segments().iter().map(|it| it.0) {
        let (row, bytes) = writer.core().segment(id).expect("named");
        let Some(bytes) = bytes else { continue };
        let run = row.run(bytes);
        let mut seen = BTreeSet::new();
        for i in 0..row.count as usize {
            if !seen.insert(Entry::read(&run[i * ENTRY..(i + 1) * ENTRY]).key) {
                return Err(format!("segment {id:016x} indexes one key twice"));
            }
        }
    }
    Ok(())
}

/// The invariant of puts on every worker: lanes and a reader interleaved, a power cut
/// anywhere in a commit: no promise is lost, nothing is seen before its flip, a worker sees
/// its own puts, and workers that put one key with equal bytes converge on one record.
#[test]
fn lanes_put_with_no_shared_lock_and_keep_every_flip_s_promise() {
    blaze_sim::check(SimConfig::DEFAULT, |tape| {
        let seed = tape.next_u64("sim seed");
        let workers = tape.int_in(1..=8u32, "workers");
        let rounds = tape.int_in(1..=5u32, "rounds");
        let cut = match tape.int_in(0..=2u32, "power cut") {
            0 => None,
            1 => Some(Cut::AtFlip),
            _ => Some(Cut::After(tape.int_in(0..=200u32, "cut after"))),
        };
        let outcome = lanes(seed, workers, rounds, cut, Plant::None);
        assert!(outcome.is_ok(), "seed {seed}, {workers} workers, {rounds} rounds, cut {cut:?}: {outcome:?}");
    });
}

/// The control: index entries published (the flip) before the segment's bytes are durable;
/// a cut between the two loses what the flip promised, and the oracle catches it.
#[test]
fn the_oracle_catches_a_lane_commit_flipped_before_its_bytes_are_durable() {
    let mut caught = None;
    for seed in 0..16u64 {
        let honest = lanes(seed, 4, 3, Some(Cut::AtFlip), Plant::None);
        assert!(honest.is_ok(), "seed {seed}, cut at the flip, without the plant: {honest:?}");
        if caught.is_none() {
            caught = lanes(seed, 4, 3, Some(Cut::AtFlip), Plant::FlipBeforeSync).err();
        }
    }
    assert!(caught.is_some(), "no seed of 16 caught a flip before its segment's sync");
}

/// Rule 25 across workers: the same records, split over other lanes and put in another order,
/// commit to a segment of equal bytes.
#[test]
fn two_builders_on_other_lanes_write_equal_segments() {
    let root = test_root();
    let bytes_of = |seed: u64, workers: u32, reversed: bool| -> Vec<u8> {
        let store = handle(Sim::new(seed, Faults::default()), 1, 1, Plant::None, &root);
        let ids: Vec<LaneId> = (0..workers).map(|_| store.lane().expect("under the cap")).collect();
        let mut keys: Vec<u32> = (0..40).collect();
        if reversed {
            keys.reverse();
        }
        for (i, n) in keys.into_iter().enumerate() {
            let lane = ids[i % workers as usize];
            store.put_in(lane, key(n), Namespace::LOCAL, TAG, &value(n)).expect("a put");
            // A second worker puts some keys again, with equal bytes.
            if n % 7 == 0 {
                store.put_in(ids[(i + 1) % workers as usize], key(n), Namespace::LOCAL, TAG, &value(n)).expect("a put");
            }
        }
        store.commit_lanes().expect("a commit");
        let writer = store.writer();
        let segments = writer.core().segments();
        assert_eq!(segments.len(), 1, "one namespace, one segment");
        writer.core().segment(segments[0].0).and_then(|it| it.1).expect("mapped").to_vec()
    };
    assert_eq!(bytes_of(1, 1, false), bytes_of(2, 5, true), "two builders of the same records wrote other bytes");
}

/// A run of `n` test keys, sorted.
fn run_of(n: u32) -> Vec<u8> {
    let mut entries = Vec::new_in(Global);
    for k in 0..n {
        entries.push(Entry { key: key(k).0, tag: TAG, at: HEADER as u64, len: 0, crc: 0 });
    }
    format::sorted_run(entries).unwrap()
}

/// The lookup before 2026-11-15, kept as the control of the test below: each guess read the
/// range's first and last entries.
fn endpoint_find<'a>(n: usize, key: &ContentId, mut key_of: impl FnMut(usize) -> &'a [u8]) -> Option<usize> {
    let prefix = |k: &[u8]| u64::from_be_bytes(k[0..8].try_into().unwrap());
    let want = prefix(&key.0);
    let (mut lo, mut hi, mut guesses) = (0usize, n, 0);
    while lo < hi {
        let mid = if guesses < 3 && hi - lo > 8 {
            guesses += 1;
            let (first, last) = (prefix(key_of(lo)), prefix(key_of(hi - 1)));
            if want < first || want > last {
                return None;
            }
            match last - first {
                0 => lo + (hi - lo) / 2,
                span => lo + ((u128::from(want - first) * (hi - 1 - lo) as u128) / u128::from(span)) as usize,
            }
        } else {
            lo + (hi - lo) / 2
        };
        match key_of(mid).cmp(&key.0[..]) {
            std::cmp::Ordering::Equal => return Some(mid),
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
        }
    }
    None
}

/// The pages of 16 KiB a lookup of each probed key touches, at most and in all, and how many
/// lookups read the run's first or last entry with their key at neither.
fn pages_touched(run: &[u8], n: u32, endpoints: bool) -> (usize, usize, usize) {
    const PAGE: usize = 16 << 10;
    let count = run.len() / ENTRY;
    let (mut most, mut total, mut at_ends) = (0, 0, 0);
    for k in (0..n).step_by(101).chain([n + 1, n + 7_000]) {
        let mut pages = BTreeSet::new();
        let mut entries = BTreeSet::new();
        let key_of = |i: usize| {
            pages.insert(i * ENTRY / PAGE);
            entries.insert(i);
            &run[i * ENTRY..i * ENTRY + 32]
        };
        let found = if endpoints { endpoint_find(count, &key(k).0, key_of) } else { format::find_by(count, &key(k).0, key_of) };
        assert_eq!(found.map(|i| run[i * ENTRY..i * ENTRY + 32] == key(k).0.0), (k < n).then_some(true), "key {k}");
        // The cause, measured exactly: a read of the run's first or last entry for a key that
        // is neither. (A page measure counts a probe that interpolation put on the first or
        // last page for a key on the page next to it: a probe, not an endpoint read.)
        if (entries.contains(&0) && found != Some(0)) || (entries.contains(&(count - 1)) && found != Some(count - 1)) {
            at_ends += 1;
        }
        most = most.max(pages.len());
        total += pages.len();
    }
    (most, total, at_ends)
}

/// The lookup reads the pages its probes land on and never the run's ends for a key elsewhere
/// (two cold pages a segment probed, before): the cause removed, the control (the old lookup
/// over the same run) shows the effect it had.
#[test]
fn a_lookup_reads_only_the_pages_its_probes_land_on() {
    let n = 200_000;
    let run = run_of(n);
    let (most, total, at_ends) = pages_touched(&run, n, false);
    assert_eq!(at_ends, 0, "lookups read the run's first or last entry for keys at neither");
    assert!(most <= 6, "a lookup touched {most} pages of a {n}-entry run");
    let (_, old_total, old_at_ends) = pages_touched(&run, n, true);
    assert!(old_at_ends > 0, "the control: the old lookup read the run's ends");
    assert!(total < old_total, "the endpoint-free lookup touched {total} pages in all, the old {old_total}");
}

/// `find_many` answers what `find_by` does for every key of a batch, in the batch's order: held
/// keys, absent ones, a key twice, an empty batch, a run of none.
#[test]
fn find_many_equals_find_by_for_every_key() {
    blaze_sim::check(SimConfig::DEFAULT, |tape| {
        let n = tape.int_in(0..=3000u32, "entries");
        let run = run_of(n);
        let count = run.len() / ENTRY;
        let take = tape.int_in(0..=200u32, "keys");
        let mut keys = Vec::new_in(Global);
        for _ in 0..take {
            let k = tape.int_in(0..=n + 50, "key");
            keys.push(key(k).0);
            if tape.int_in(0..=7u32, "twice") == 0 {
                keys.push(key(k).0);
            }
        }
        let at = |i: usize| &run[i * ENTRY..i * ENTRY + 32];
        let many = format::find_many(count, &keys, at);
        for (k, got) in keys.iter().zip(&many) {
            assert_eq!(*got, format::find_by(count, k, at), "{k:?} in a run of {n}");
        }
    });
}

/// What a batch of lookups touches of a million-entry index: `(pages, probes)` one lookup a
/// key against `find_many`, and the floor, the distinct pages the batch's held keys live on.
fn batch_pages(run: &[u8], keys: &[ContentId]) -> [(usize, usize); 3] {
    const PAGE: usize = 16 << 10;
    let count = run.len() / ENTRY;
    let mut one = (BTreeSet::new(), 0usize);
    for k in keys {
        format::find_by(count, k, |i| {
            one.0.insert(i * ENTRY / PAGE);
            one.1 += 1;
            &run[i * ENTRY..i * ENTRY + 32]
        });
    }
    let mut many = (BTreeSet::new(), 0usize);
    let found = format::find_many(count, keys, |i| {
        many.0.insert(i * ENTRY / PAGE);
        many.1 += 1;
        &run[i * ENTRY..i * ENTRY + 32]
    });
    // The floor with a filter: the pages held keys live on. Without one: those, and the page
    // an absent key's neighbours are on, which a lookup must read to say it is absent.
    let held: BTreeSet<usize> = found.iter().flatten().map(|i| i * ENTRY / PAGE).collect();
    let all: BTreeSet<usize> = keys
        .iter()
        .map(|k| {
            let (mut lo, mut hi) = (0, count);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if run[mid * ENTRY..mid * ENTRY + 32] < k.0[..] { lo = mid + 1 } else { hi = mid }
            }
            lo.min(count - 1) * ENTRY / PAGE
        })
        .collect();
    [(one.0.len(), one.1), (many.0.len(), many.1), (held.len(), all.len())]
}

/// The pages and probes of batches of 1, 64 and 4,096 keys (three in four held) over a run of a
/// million entries: `find_many` touches no more pages than a lookup a key and fewer probes, and
/// its pages are within 10% of the floor's (rule 67) once a batch shares pages. Measured
/// 2026-11-15 (16 KiB pages): 1 key 2 pages either way, floor 1; 64 keys 171 one at a time, 87
/// batched, floor 63; 4,096 keys 2,776, 2,234, floor 2,186 (1.02). Before the gallop (three
/// guesses and fences): 147 and 2,626 batched.
#[test]
fn a_batch_touches_the_index_pages_its_keys_live_on() {
    let n = 1_000_000u32;
    let run = run_of(n);
    let mut lines = Vec::new_in(Global);
    for batch in [1u32, 64, 4096] {
        let keys: Vec<ContentId> = (0..batch).map(|i| key(i.wrapping_mul(2_654_435_761) % (n + n / 3)).0).collect();
        let [(one_pages, one_probes), (many_pages, many_probes), (held, all)] = batch_pages(&run, &keys);
        let line = format!("{batch}: one at a time {one_pages} pages {one_probes} probes; find_many {many_pages} pages {many_probes} probes; floor {all} (held keys only {held})");
        assert!(many_pages <= one_pages && many_probes <= one_probes, "{line}");
        // The guess's page and the key's for one key; within 10% of the floor once a batch
        // shares pages.
        let bound = match batch {
            1 => 2 * all,
            64 => all * 3 / 2,
            _ => all * 11 / 10,
        };
        assert!(many_pages <= bound, "{line}");
        lines.push(line);
    }
    assert_eq!(lines.len(), 3);
}

/// The lookup finds every key of a run whose keys are not spread evenly (clusters of prefixes,
/// runs of one prefix), and no key it does not hold.
#[test]
fn a_lookup_finds_every_key_of_an_uneven_run() {
    blaze_sim::check(SimConfig::DEFAULT, |tape| {
        let clusters = tape.int_in(1..=4u32, "clusters");
        let mut keys = BTreeSet::new();
        for c in 0..clusters {
            let base = tape.next_u64("cluster base");
            let size = tape.int_in(1..=200u32, "cluster size");
            let width = [0u64, 1, 1 << 20, u64::MAX >> 8][tape.index(4, "cluster width")];
            for i in 0..size {
                let mut k = ContentId::of("uneven run", &[c.to_le_bytes(), i.to_le_bytes()].concat()).0;
                let offset = if width == 0 { 0 } else { u64::from_be_bytes(k[0..8].try_into().unwrap()) % width };
                k[0..8].copy_from_slice(&base.wrapping_add(offset).to_be_bytes());
                keys.insert(k);
            }
        }
        let mut entries = Vec::new_in(Global);
        entries.extend(keys.iter().map(|k| Entry { key: ContentId(*k), tag: TAG, at: HEADER as u64, len: 0, crc: 0 }));
        let run = format::sorted_run(entries).unwrap();
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(format::find(&run, &ContentId(*k)), Some(i), "key {i} of {}", keys.len());
            let mut absent = *k;
            absent[31] ^= 1;
            if !keys.contains(&absent) {
                assert_eq!(format::find(&run, &ContentId(absent)), None, "a key not held, next to key {i}");
            }
        }
    });
}

/// Sealed sharing (rule 25): a record holding this machine's path is refused for a shared
/// namespace, by a put (naming the key and the offset) and in a worker's batch at the commit,
/// and kept under the local one. The control: the same puts with no path listed go through.
#[test]
fn a_record_naming_the_machine_never_reaches_a_shared_namespace() {
    let root = test_root();
    let mut sim = Sim::new(3, Faults::default());
    let mut w = [Actor::new(cfg(0, 1, false, &root), &root, Plant::None)];
    settle(&mut sim, &mut w);
    let mut leaky = b"a record built at ".to_vec();
    leaky.extend_from_slice(MACHINE);
    leaky.extend_from_slice(b"/src/lib.rs");
    for space in [Namespace::PUBLIC, org()] {
        let refused = w[0].store.put(key(1), space, TAG, &leaky);
        assert!(matches!(refused, Err(super::core::PutError::Machine { at: 18, needle: 0, namespace, .. }) if namespace == space), "{refused:?}");
    }
    assert!(w[0].store.put(key(1), Namespace::LOCAL, TAG, &leaky).is_ok(), "the local namespace keeps it");
    let mut batch = super::core::Batch::new(org());
    batch.put(key(2), TAG, &leaky).unwrap();
    let refused = w[0].store.commit(&[&batch],sim.wall_ns(), &mut w[0].effects);
    assert!(matches!(refused, Err(StoreError::Other { .. })), "{refused:?}");
    // The control: a store that lists no path of the machine takes both.
    let mut open = cfg(1, 1, false, &root);
    open.machine.clear();
    let mut c = [Actor::new(open, &root, Plant::None)];
    settle(&mut sim, &mut c);
    assert!(c[0].store.put(key(3), Namespace::PUBLIC, TAG, &leaky).is_ok());
    let mut batch = super::core::Batch::new(org());
    batch.put(key(4), TAG, &leaky).unwrap();
    assert!(c[0].store.commit(&[&batch],sim.wall_ns(), &mut c[0].effects).is_ok());
    settle(&mut sim, &mut c);
}

/// A startup image round-trips byte for byte, refuses another program's or format's bytes by
/// name, and orders its reads by segment and offset from what a real store places: the image's
/// records committed over two commits (two segments), read back in one ascending pass each.
#[test]
fn a_startup_image_round_trips_and_reads_in_place_order() {
    use super::image::{ImageError, Placement, StartupImage};
    let root = test_root();
    let manifest = ContentId::of("a program's manifest", b"hello");
    let mut sim = Sim::new(5, Faults::default());
    let mut w = [Actor::new(cfg(0, 1, false, &root), &root, Plant::None)];
    settle(&mut sim, &mut w);
    // Placed before main in this order: 7, 3, 9, 1, 5 (slots 0..5); committed in two batches.
    let order = [7u32, 3, 9, 1, 5];
    for half in [&[7u32, 9, 5][..], &[3u32, 1][..]] {
        for &n in half {
            w[0].store.put(key(n), Namespace::LOCAL, TAG, &value(n)).unwrap();
        }
        w[0].store.commit(&[],sim.wall_ns(), &mut w[0].effects).unwrap();
        settle(&mut sim, &mut w);
        assert!(matches!(w[0].store.take_committed(), Some(Ok(_))));
    }
    let mut placed = Vec::new_in(&root);
    placed.extend(order.iter().enumerate().map(|(slot, n)| Placement { slot: slot as u32, code: key(*n).0 }));
    placed.push(Placement { slot: 5, code: key(400_000).0 });
    let image = StartupImage { manifest, placed };
    let mut bytes = Vec::new_in(&root);
    image.encode_into(&mut bytes);
    let mut again = Vec::new_in(&root);
    StartupImage::decode_in(&bytes, &manifest, &root).unwrap().encode_into(&mut again);
    assert_eq!(bytes, again, "decode then encode is the same bytes");
    assert_eq!(StartupImage::decode_in(&bytes, &manifest, &root).unwrap(), image);
    let other = ContentId::of("a program's manifest", b"another");
    assert_eq!(StartupImage::decode_in(&bytes, &other, &root), Err(ImageError::Manifest { found: manifest, wanted: other }));
    let mut old = bytes.clone();
    old[4..8].copy_from_slice(&2u32.to_le_bytes());
    assert_eq!(StartupImage::decode_in(&old, &manifest, &root), Err(ImageError::Format { found: 2, wanted: 1 }));
    assert!(matches!(StartupImage::decode_in(&bytes[..bytes.len() - 1], &manifest, &root), Err(ImageError::Short { .. })));
    let mut reads = Vec::new_in(&root);
    image.read_order(|k| w[0].store.locate(k), &mut reads);
    assert_eq!(reads.len(), 5, "the key the store does not hold is left out");
    for pair in reads.windows(2) {
        let (a, b) = (&pair[0].1, &pair[1].1);
        assert!(a.segment.0 < b.segment.0 || (a.segment == b.segment && a.at < b.at), "reads out of place order: {a:?} then {b:?}");
    }
    let segments: BTreeSet<[u8; 32]> = reads.iter().map(|it| it.1.segment.0).collect();
    assert_eq!(segments.len(), 2, "two commits, two segments");
}

/// Resident daemon lease lifecycle: `release_lease` drops the lease to 0 while idle (never
/// blocking `blaze gc`), and `rearm_lease` lets the next request acquire a fresh lease.
#[test]
fn daemon_rearm_lease_reacquires_after_idle_zero_leases() {
    use super::lent::Lent;
    let root = test_root();
    let mut c = cfg(0, 1, false, &root);
    c.liveness = super::core::Liveness::Reached;
    let store = super::driven::Driven::with(Sim::new(71, Faults::default()), Path::new("/sim"), c, &root).unwrap();
    let lent = Lent::new(store, &root);
    lent.map_all().unwrap();

    // Request 1: multi-phase write acquires a lease on its first segment commit.
    lent.put(&key(1), Namespace::LOCAL, TAG, &value(1)).unwrap();
    lent.commit().unwrap();
    assert_eq!(lent.leases_len(), 1, "active request must hold a lease");

    // Request 1 finishes: releases lease and commits final root -> 0 leases while idle.
    lent.root("ans.req1", &[key(1)]).unwrap();
    lent.release_lease();
    lent.commit().unwrap();
    assert_eq!(lent.leases_len(), 0, "idle daemon must hold 0 leases");

    // Without rearm_lease, subsequent writes stay unleased (legacy one-shot command behavior).
    lent.put(&key(2), Namespace::LOCAL, TAG, &value(2)).unwrap();
    lent.commit().unwrap();
    assert_eq!(lent.leases_len(), 0, "without rearm_lease, released stays true");

    // Request 2 starts: rearm_lease re-enables lease acquisition for the new request.
    let gen_before_req2 = lent.generation();
    lent.rearm_lease();
    lent.put(&key(3), Namespace::LOCAL, TAG, &value(3)).unwrap();
    lent.commit().unwrap();
    assert_eq!(lent.leases_len(), 1, "rearm_lease must allow Request 2 to acquire a fresh lease");

    // Calling rearm_lease while a lease is already held is a no-op and keeps the lease intact.
    lent.rearm_lease();
    assert_eq!(lent.leases_len(), 1);
    assert!(lent.generation() > gen_before_req2);

    // Request 2 finishes: releases lease -> back to 0 leases while idle.
    lent.root("ans.req2", &[key(3)]).unwrap();
    lent.release_lease();
    lent.commit().unwrap();
    assert_eq!(lent.leases_len(), 0, "idle daemon after Request 2 must hold 0 leases");
}

/// Resident daemon arena reclamation: `reclaim_between_requests(&mut lent)` drops `kept_arenas`
/// (in `InArena` mode) and kept lent chunks (in `AfterCommit` mode) once all `Lend` borrows end,
/// while keeping mapped segments warm and all committed records readable.
#[test]
fn daemon_reclaim_between_requests_drops_kept_arenas_and_preserves_mapped_segments() {
    use super::arena::Visibility;
    use super::lent::Lent;

    // 1. Visibility::InArena (shared arenas accumulated in kept_arenas).
    let root = test_root();
    let store = super::driven::Driven::with(Sim::new(72, Faults::default()), Path::new("/sim"), cfg(0, 1, false, &root), &root).unwrap();
    let mut lent = Lent::with_visibility(store, &root, Visibility::InArena { shared: blaze_mem::Kept(blaze_mem::Tag::Store) });
    lent.map_all().unwrap();

    // Two commits where own puts are borrowed via `get` before commit -> 2 kept_arenas.
    lent.put(&key(10), Namespace::LOCAL, TAG, &value(10)).unwrap();
    {
        let lend = lent.get(&key(10)).unwrap().expect("own put readable");
        assert!(lend.own && lend.shared);
        assert_eq!(lend.bytes, value(10).as_slice());
        lent.commit().unwrap();
    }
    lent.put(&key(11), Namespace::LOCAL, TAG, &value(11)).unwrap();
    {
        let lend = lent.get(&key(11)).unwrap().expect("own put readable");
        assert_eq!(lend.bytes, value(11).as_slice());
        lent.commit().unwrap();
    }
    assert!(lent.counts().held_bytes > 0, "lent shared arenas stay held until reclaim");

    // Uncommitted put prevents reclamation so live uncommitted data is never dropped.
    lent.put(&key(12), Namespace::LOCAL, TAG, &value(12)).unwrap();
    assert_eq!(lent.reclaim_between_requests(), 0, "must not reclaim while uncommitted puts exist");
    assert_eq!(lent.get(&key(12)).unwrap().expect("uncommitted put intact").bytes, value(12).as_slice());
    lent.commit().unwrap();

    // All borrows ended and uncommitted_bytes == 0 -> reclaim drops all 3 kept_arenas.
    let (mapped_before, named_before) = lent.mapped_of_named();
    assert_eq!((mapped_before, named_before), (3, 3));
    let reclaimed = lent.reclaim_between_requests();
    assert_eq!(reclaimed, 3, "expected 3 kept shared arenas reclaimed");
    assert_eq!(lent.counts().held_bytes, 0, "held_bytes must be 0 after reclaim");
    assert_eq!(lent.reclaim_between_requests(), 0, "second reclaim is a no-op");

    // Mapped segments stayed open and all committed records remain readable in place.
    assert_eq!(lent.mapped_of_named(), (3, 3));
    for n in [10u32, 11, 12] {
        let lend = lent.get(&key(n)).unwrap().expect("committed record readable after reclaim");
        assert!(!lend.own);
        assert_eq!(lend.bytes, value(n).as_slice());
    }

    // 2. Visibility::AfterCommit (private lent chunks).
    let store2 = super::driven::Driven::with(Sim::new(73, Faults::default()), Path::new("/sim"), cfg(1, 1, false, &root), &root).unwrap();
    let mut lent2 = Lent::new(store2, &root);
    lent2.map_all().unwrap();
    lent2.put(&key(20), Namespace::LOCAL, TAG, &value(20)).unwrap();
    {
        let lend = lent2.get(&key(20)).unwrap().expect("own private put readable");
        assert!(lend.own && !lend.shared);
        lent2.commit().unwrap();
    }
    assert!(lent2.counts().held_bytes > 0);
    assert_eq!(lent2.reclaim_between_requests(), 1, "expected 1 private lent chunk reclaimed");
    assert_eq!(lent2.counts().held_bytes, 0);
    assert_eq!(lent2.get(&key(20)).unwrap().expect("committed record readable").bytes, value(20).as_slice());
}

/// `head_generation_on_disk` reads only the head file in O(1), matching `generation()` before
/// external writes and immediately detecting external commits and store wipes.
#[test]
fn daemon_head_generation_on_disk_catches_external_commits() {
    use super::lent::Lent;
    use super::settings::Settings;

    let dir = std::env::temp_dir().join(format!("blaze-cas-daemon-head-gen-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let root = test_root();
    let settings = Settings { durable: false, ..Settings::default() };

    let store1 = super::driven::Driven::open(&dir, &settings, &root).unwrap();
    let lent = Lent::new(store1, &root);
    lent.map_all().unwrap();
    assert_eq!(lent.generation(), 0);
    assert_eq!(lent.head_generation_on_disk().unwrap(), 0);

    // Actor 1 commits generation 1.
    lent.put(&key(100), Namespace::LOCAL, TAG, &value(100)).unwrap();
    lent.commit().unwrap();
    assert_eq!(lent.generation(), 1);
    assert_eq!(lent.head_generation_on_disk().unwrap(), 1);

    // Actor 2 opens the same store externally and commits generation 2.
    let mut actor2 = super::driven::Driven::open(&dir, &settings, &root).unwrap();
    actor2.put(key(101), Namespace::LOCAL, TAG, &value(101)).unwrap();
    actor2.commit(&[]).unwrap();
    assert_eq!(actor2.core().generation(), 2);

    // Actor 1 still holds generation 1 in memory, but head_generation_on_disk sees 2 immediately!
    let (mapped_before, named_before) = lent.mapped_of_named();
    assert_eq!(lent.generation(), 1);
    assert_eq!(lent.head_generation_on_disk().unwrap(), 2);
    assert_eq!(lent.mapped_of_named(), (mapped_before, named_before), "head check must not map new segments");

    // External wipe (`blaze clean --store` deleting head) reports generation 0 != 1.
    std::fs::remove_file(dir.join("plain/head")).unwrap();
    assert_eq!(lent.head_generation_on_disk().unwrap(), 0);

    drop(actor2);
    drop(lent);
    let _ = std::fs::remove_dir_all(&dir);
}
