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
//! [`Report::whole`]: super::Report::whole
//!
//! The controls (`the_oracle_catches_*`) plant one bug each and show the oracle fails.

use std::{
    alloc::Global,
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

use blaze_core::{
    Completion, Done, Driver, Effect, FileName, Tag as EffectTag,
    sim::{Faults, Sim},
    wait::Deadline,
};
use blaze_ids::ContentId;
use blaze_keyed::{
    exec::RecordRef,
    kind::{Namespace, Tag, Tier},
    store::{ENTRY, FOOTER, HEADER, Located, Refused, SegmentId, SegmentLen, SegmentRef, Source, Statement, StoreError, Verifier},
};
use hegel::generators as gs;

use super::{
    core::{Committed, Plain, PlainConfig, Plant, TAGS},
    driven::{Driven, LocalDir},
    format::{self, Entry, FORMAT, Slot, check_segment, seal_segment},
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

fn cfg(n: u64, boot: u64, durable: bool) -> PlainConfig {
    PlainConfig { dir: dir(), tags: n * TAGS, cap_bytes: u64::MAX, boot, nonce: 100 + n, durable, page: 16 << 10, writes: writes(), reads: reads() }
}

fn key(n: u32) -> Key {
    Key(ContentId::of("plain store test key", &n.to_le_bytes()))
}

fn value(n: u32) -> Vec<u8> {
    let len = (n as usize * 37) % 700;
    (0..len).map(|i| (n as usize).wrapping_mul(31).wrapping_add(i) as u8).collect()
}

struct Actor {
    store: Plain,
    effects: Vec<Effect>,
}

impl Actor {
    fn new(cfg: PlainConfig, plant: Plant) -> Actor {
        let mut store = Plain::planted(cfg, plant);
        let mut effects = Vec::new_in(Global);
        store.open(&mut effects);
        Actor { store, effects }
    }
}

/// Runs until nothing is in flight, or `budget` completions have been taken (then `false`: the
/// caller cuts the power). Bounded: a run that does not settle in a million polls fails.
fn run(sim: &mut Sim, actors: &mut [Actor], budget: &mut Option<u32>) -> bool {
    run_traced(sim, actors, budget, &mut 0, &mut None)
}

/// [`run`], each effect asked and each completion taken written to `trace` when there is one.
/// The trace only observes: every statement of the untraced run is the same, in the same
/// order, and the trace reads (never takes) what the run holds.
fn run_traced(sim: &mut Sim, actors: &mut [Actor], budget: &mut Option<u32>, taken: &mut u32, trace: &mut Option<Trace>) -> bool {
    let mut done: Vec<Completion> = Vec::new_in(Global);
    for _ in 0..1_000_000 {
        for (i, actor) in actors.iter_mut().enumerate() {
            if let Some(trace) = trace {
                trace.asked(i, &actor.effects);
            }
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
        for completion in done.drain(..) {
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
            if let Some(trace) = trace {
                trace.done(&completion);
            }
            let actor = actors.iter_mut().find(|it| it.store.owns(completion.tag)).expect("a store asked for it");
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

fn settle(sim: &mut Sim, actors: &mut [Actor]) {
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
/// new process each time: the store never retries itself).
fn reader_with(sim: &mut Sim, n: u64, boot: u64, plant: Plant) -> Actor {
    reader_traced(sim, n, boot, plant, &mut 0, &mut None)
}

/// [`reader_with`], each attempt's effects and what its open found written to `trace`.
fn reader_traced(sim: &mut Sim, n: u64, boot: u64, plant: Plant, taken: &mut u32, trace: &mut Option<Trace>) -> Actor {
    for attempt in 0..50 {
        if let Some(trace) = trace {
            trace.cast = vec![format!("reader {n} attempt {attempt}")];
        }
        let mut it = [Actor::new(cfg(n, boot, false), plant)];
        assert!(run_traced(sim, &mut it, &mut None, taken, trace));
        let [it] = it;
        if let Some(trace) = trace {
            trace.opened(0, &it.store);
        }
        match it.store.broken() {
            None => return it,
            Some(why) => assert!(why.contains("injected"), "a reader broke on its own: {why}"),
        }
    }
    panic!("no reader opened in 50 tries");
}

fn reader(sim: &mut Sim, n: u64, boot: u64) -> Actor {
    reader_with(sim, n, boot, Plant::None)
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
    fn broke(&mut self, why: &str) {
        let who = self.last_who.clone();
        // Once per store: a broken store stays broken.
        let seen = self.events.iter().any(|it| matches!(it, Event::Broke { who: w, .. } if *w == who));
        if !seen {
            self.push(Event::Broke { who, why: why.to_string() });
        }
    }

    fn opened(&mut self, i: usize, store: &Plain) {
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
    match format::Manifest::decode(bytes) {
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
        Effect::List { dir, .. } => ("list", dir, String::new()),
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
    fn land(&mut self, keys: &[u32], committed: &Committed) -> Result<(), String> {
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
fn oracle(store: &Plain, ledger: &Ledger, cut: bool, durable: bool) -> Result<(), String> {
    let lost: BTreeSet<u64> = store.report().lost.iter().filter_map(|it| it.segment).collect();
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
    // The newest generation the open read whole: with no power cut, the newest committed.
    let whole = store.report().whole.unwrap_or(0);
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
    scenario_traced(seed, rounds, per, crash_after, fail_one_in, plants, durable, &mut 0, &mut None)
}

/// [`scenario`], every step written to `trace` when there is one (the same run: the trace
/// draws nothing from the simulation and takes nothing the run holds), and `taken` counting
/// the completions handed to stores, writers' and readers'.
fn scenario_traced(
    seed: u64,
    rounds: u32,
    per: u32,
    crash_after: Option<u32>,
    fail_one_in: u32,
    plants: [Plant; 3],
    durable: bool,
    taken: &mut u32,
    trace: &mut Option<Trace>,
) -> Result<(), String> {
    let mut sim = Sim::new(seed, Faults { fail_one_in, ..Faults::default() });
    if let Some(trace) = trace {
        trace.cast = vec!["writer 0".into(), "writer 1".into()];
    }
    // Their opens' effects are asked, and traced, at the first run's submit.
    let mut actors = vec![Actor::new(cfg(0, 1, durable), plants[0]), Actor::new(cfg(1, 1, durable), plants[1])];
    let mut budget = crash_after;
    let mut ledger = Ledger { never: vec![500_000, 1_500_000], ..Ledger::default() };
    let mut next = [0u32, 1_000_000];
    let mut pending: [Option<Vec<u32>>; 2] = [None, None];
    let mut crashed = false;
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
            let returned = actor.store.commit(Vec::new_in(Global), sim.wall_ns(), &mut actor.effects);
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
            break;
        }
        if let Some(why) = actors.iter().find_map(|it| it.store.broken()) {
            if !why.contains("injected") {
                return Err(format!("a writer broke on its own: {why}"));
            }
        }
    }
    drop(actors);
    let boot = if crashed { 2 } else { 1 };
    let r = reader_traced(&mut sim, 2, boot, plants[2], taken, trace);
    if let Some(trace) = trace {
        let mut ledger_says = String::from("the ledger's committed records (key: segment, generation): ");
        for (n, it) in &ledger.committed {
            ledger_says.push_str(&format!("{n}: {:016x} g{}, ", it.segment, it.generation));
        }
        trace.push(Event::Opened { who: "ledger".into(), says: ledger_says });
    }
    oracle(&r.store, &ledger, crashed, durable)?;
    // Reopened again in the same boot: what the first reader read, byte for byte, with no
    // loss reported a second time.
    let again = reader_traced(&mut sim, 3, boot, Plant::None, taken, trace);
    for &n in ledger.committed.keys().chain(ledger.maybe.keys()) {
        let (a, b) = (r.store.get(&key(n)), again.store.get(&key(n)));
        if a.map(|it| it.bytes) != b.map(|it| it.bytes) {
            return Err(format!("record {n} reads {a:?}, and {b:?} after a reopen"));
        }
    }
    if !again.store.report().lost.is_empty() {
        return Err(format!("a second open reported again: {:?}", again.store.report().lost));
    }
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
#[hegel::test]
fn two_writers_and_power_cuts_keep_every_committed_record_or_say_so(tc: hegel::TestCase) {
    two_writers_and_power_cuts(tc);
}

/// Regression (2026-11-13, round 3): a power cut tore the head's first write (then a whole-file
/// write of 4160 bytes) to 3072 bytes, and the reader took the short head for another store's
/// file and stopped. The head is now made at its length and synced at the first open, every
/// flip writes one slot in place, and a short head is read as far as it goes.
///
/// The history is Hegel's minimal case of then (seed 0, two rounds of one record, a power cut
/// after 21 completions, no injected failure), written out step by step; the records are now
/// put under `LOCAL` and `PUBLIC` (keys 0 and 1_000_000), so the history is not the same one
/// completion for completion, and the test asserts only that the cut lands.
#[test]
fn a_head_torn_by_a_power_cut_is_read_through_not_refused() {
    let mut sim = Sim::new(0, Faults::default());
    // Two writers of boot 1 open the empty store: the first to read the head makes it.
    let mut actors = vec![Actor::new(cfg(0, 1, false), Plant::None), Actor::new(cfg(1, 1, false), Plant::None)];
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
            actor.store.commit(Vec::new_in(Global), sim.wall_ns(), &mut actor.effects).unwrap();
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
    let r = reader(&mut sim, 2, 2);
    assert_eq!(r.store.broken(), None);
    let head = sim.contents(&FileName::new("plain/head").unwrap());
    assert!(head.len() <= format::HEAD_LEN, "a head of {} bytes", head.len());
    let outcome = oracle(&r.store, &ledger, true, false);
    assert!(outcome.is_ok(), "{outcome:?}");
}

/// Regression (2026-11-13, round 4; Hegel blob `AXicY2MAAi5GIMGEQjFCKAZUCgAH4wBO`: seed 2, two
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

/// Regression (2026-11-13, round 5; Hegel blob
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
    let outcome = scenario_traced(875_559, 3, 5, Some(82), 40, HONEST, false, &mut 0, &mut trace);
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
        let untraced = scenario_traced(seed, rounds, per, cut, fail, HONEST, durable, &mut bare, &mut None);
        let mut traced_taken = 0;
        let mut trace = Some(Trace::default());
        let traced = scenario_traced(seed, rounds, per, cut, fail, HONEST, durable, &mut traced_taken, &mut trace);
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

fn two_writers_and_power_cuts(tc: hegel::TestCase) {
    let seed = tc.draw(gs::integers::<u64>());
    let rounds = tc.draw(gs::integers::<u32>().min_value(1).max_value(6));
    let per = tc.draw(gs::integers::<u32>().min_value(1).max_value(24));
    let cut = tc.draw(gs::integers::<u32>().min_value(0).max_value(1)) == 1;
    let after = tc.draw(gs::integers::<u32>().min_value(0).max_value(400));
    let fail = if tc.draw(gs::integers::<u32>().min_value(0).max_value(3)) == 0 { 40 } else { 0 };
    let outcome = scenario(seed, rounds, per, cut.then_some(after), fail, HONEST, false);
    assert!(outcome.is_ok(), "seed {seed}, {rounds} rounds of {per}, cut {cut} after {after}, fail 1 in {fail}: {outcome:?}");
}

/// Durable mode: a power cut loses nothing a completed commit wrote.
#[hegel::test]
fn durable_commits_survive_any_power_cut(tc: hegel::TestCase) {
    let seed = tc.draw(gs::integers::<u64>());
    let after = tc.draw(gs::integers::<u32>().min_value(0).max_value(400));
    let outcome = scenario(seed, 4, 8, Some(after), 0, HONEST, true);
    assert!(outcome.is_ok(), "seed {seed}, cut after {after}: {outcome:?}");
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
        let mut sim = Sim::new(seed, Faults::default());
        let mut w = [Actor::new(cfg(0, 1, true), plant)];
        settle(&mut sim, &mut w);
        let mut ledger = Ledger::default();
        let keys: Vec<u32> = (0..6).collect();
        for &n in &keys {
            w[0].store.put(key(n), space(n), TAG, &value(n)).unwrap();
            ledger.maybe.insert(n, space(n));
        }
        let effects = &mut w[0].effects;
        w[0].store.commit(Vec::new_in(Global), sim.wall_ns(), effects).unwrap();
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
        let r = reader(&mut sim, 1, 2);
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
    let mut sim = Sim::new(7, Faults::default());
    let mut actors = vec![Actor::new(cfg(0, 1, false), Plant::None), Actor::new(cfg(1, 1, false), Plant::None)];
    settle(&mut sim, &mut actors);
    for round in 0..8u32 {
        for (i, actor) in actors.iter_mut().enumerate() {
            let n = round * 2 + i as u32;
            actor.store.put(key(n), space(n), TAG, &value(n)).unwrap();
            actor.store.commit(Vec::new_in(Global), sim.wall_ns(), &mut actor.effects).unwrap();
        }
        settle(&mut sim, &mut actors);
        for actor in &mut actors {
            assert!(matches!(actor.store.take_committed(), Some(Ok(_))));
        }
    }
    let r = reader(&mut sim, 2, 1);
    // Generation 0 is the empty store (no slot written); each commit's flip makes the next.
    assert_eq!(r.store.generation(), 16, "16 commits by two writers, each flipped once on the one before");
    for n in 0..16 {
        assert_eq!(r.store.get(&key(n)).map(|it| it.bytes.to_vec()), Some(value(n)));
    }
}

/// A store of one writer that has committed `keys` under `LOCAL` in one commit, durably; the
/// commit's one segment and its ledger.
fn one_local_commit(sim: &mut Sim, keys: std::ops::Range<u32>) -> (u64, Ledger) {
    let mut w = [Actor::new(cfg(0, 1, true), Plant::None)];
    settle(sim, &mut w);
    let mut ledger = Ledger::default();
    let keys: Vec<u32> = keys.collect();
    for &n in &keys {
        w[0].store.put(key(n), Namespace::LOCAL, TAG, &value(n)).unwrap();
        ledger.maybe.insert(n, Namespace::LOCAL);
    }
    let effects = &mut w[0].effects;
    w[0].store.commit(Vec::new_in(Global), sim.wall_ns(), effects).unwrap();
    settle(sim, &mut w);
    let committed = w[0].store.take_committed().unwrap().unwrap();
    ledger.land(&keys, &committed).unwrap();
    assert_eq!(committed.segments.len(), 1, "one namespace, one segment");
    (committed.segments[0], ledger)
}

/// A segment of an earlier boot whose bytes did not survive (here: rotted after a durable
/// commit) is checked at the first open of the new boot, dropped and reported by name with its
/// count; the plant that trusts it returns a corrupt record instead.
#[test]
fn the_oracle_catches_a_segment_of_an_earlier_boot_read_unchecked() {
    let outcome = |plant: Plant| {
        let mut sim = Sim::new(3, Faults::default());
        let (segment, ledger) = one_local_commit(&mut sim, 0..10);
        sim.crash();
        let file = FileName::new(&format!("plain/seg-{segment:016x}")).unwrap();
        sim.rot(&file, HEADER as u64 + 3, &[0xff, 0x00, 0xff]);
        let mut r = [Actor::new(cfg(1, 2, false), plant)];
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
    let mut sim = Sim::new(5, Faults::default());
    let (segment, _) = one_local_commit(&mut sim, 9..10);
    let file = FileName::new(&format!("plain/seg-{segment:016x}")).unwrap();
    sim.rot(&file, HEADER as u64, &[!value(9)[0]]);
    let r = reader(&mut sim, 1, 1);
    let read = r.store.get(&key(9)).map(|it| it.bytes.to_vec());
    assert_ne!(read, Some(value(9)), "the read returned the bytes as they lie, unchecked");
    assert_eq!(r.store.checks(), super::Checks { full: 0, reads: 1, per_read: 0 });
    let found = r.store.scrub();
    assert_eq!(found.iter().map(|it| (it.key, it.pack.clone())).collect::<Vec<_>>(), vec![(key(9), file.clone())]);
    // The next boot proves the segment once, from its digest, and drops it by name.
    let next = reader(&mut sim, 2, 2);
    assert_eq!(next.store.checks().full, 1);
    assert_eq!(next.store.report().lost.iter().map(|it| (it.file.clone(), it.records)).collect::<Vec<_>>(), vec![(file, Some(1))]);
    assert_eq!(next.store.get(&key(9)).map(|it| it.bytes.len()), None);
}

/// The control for "verify once": an open that checks a segment of an earlier boot and does not
/// record it makes the next open of the same boot check it again, and the check the oracle
/// makes (no segment fully checked in a second open of one boot) sees it.
#[test]
fn the_oracle_catches_a_full_check_made_twice_in_one_boot() {
    let second_open_checks = |plant: Plant| {
        let mut sim = Sim::new(8, Faults::default());
        one_local_commit(&mut sim, 0..6);
        sim.crash();
        let mut first = [Actor::new(cfg(1, 2, false), plant)];
        settle(&mut sim, &mut first);
        assert_eq!(first[0].store.checks().full, 1);
        let second = reader(&mut sim, 2, 2);
        second.store.fully_checked().len()
    };
    assert_eq!(second_open_checks(Plant::None), 0, "an honest open records its check");
    assert_eq!(second_open_checks(Plant::ForgetCheck), 1, "the planted bug went unseen");
}

/// The files of `dir`, by name, with their bytes.
fn files(sim: &mut Sim) -> BTreeMap<String, Vec<u8>> {
    let Done::Listed(listed) = ask(sim, Effect::List { dir: dir(), tag: EffectTag(OWN + 1) }) else { panic!("a listing") };
    listed.into_iter().filter(|(_, meta)| !meta.dir).map(|(name, _)| (name.as_str().to_owned(), sim.contents(&name))).collect()
}

fn build(seed: u64) -> (BTreeMap<String, Vec<u8>>, Sim) {
    let mut sim = Sim::new(seed, Faults::default());
    let mut w = [Actor::new(cfg(0, 1, false), Plant::None)];
    settle(&mut sim, &mut w);
    for round in 0..5u32 {
        for n in round * 10..round * 10 + 10 {
            w[0].store.put(key(n), space(n), TAG, &value(n)).unwrap();
        }
        let effects = &mut w[0].effects;
        w[0].store.commit(Vec::new_in(Global), sim.wall_ns(), effects).unwrap();
        settle(&mut sim, &mut w);
        assert!(matches!(w[0].store.take_committed(), Some(Ok(_))));
    }
    (files(&mut sim), sim)
}

/// The persisted round trip: two builds of the same puts write the same bytes, file for file;
/// a reopen reads every record back byte for byte, under its namespace.
#[test]
fn the_same_puts_write_the_same_bytes_and_read_back() {
    let (a, mut sim) = build(11);
    let (b, _) = build(11);
    assert_eq!(a, b, "two builds of one history wrote different bytes");
    assert!(a.keys().any(|it| it.ends_with("CACHEDIR.TAG")), "{:?}", a.keys());
    let r = reader(&mut sim, 1, 1);
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
    let mut sim = Sim::new(21, Faults::default());
    let mut w = [Actor::new(cfg(0, 1, false), Plant::None)];
    settle(&mut sim, &mut w);
    let mut ledger = Ledger::default();
    let mut rounds: Vec<Vec<u64>> = Vec::new_in(Global);
    for round in 0..3u32 {
        let keys: Vec<u32> = (round * 4..round * 4 + 4).collect();
        for &n in &keys {
            w[0].store.put(key(n), space(n), TAG, &value(n)).unwrap();
            ledger.maybe.insert(n, space(n));
        }
        let effects = &mut w[0].effects;
        w[0].store.commit(Vec::new_in(Global), sim.wall_ns(), effects).unwrap();
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
        let r = reader(&mut sim, 1, 2);
        let report = r.store.report().clone();
        let lost: BTreeSet<u64> = report.lost.iter().filter_map(|it| it.segment).collect();
        oracle(&r.store, &ledger, true, false).unwrap_or_else(|why| panic!("head cut to {cut}: {why}"));
        let (whole, gone): (Option<u64>, &[Vec<u64>]) = match cut {
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
        let again = reader(&mut sim, 2, 2);
        assert!(again.store.report().lost.is_empty(), "head cut to {cut}: reported again: {:?}", again.store.report());
        assert!(again.store.fully_checked().is_empty(), "head cut to {cut}: checked twice in one boot");
    }
}

/// A file of a previous layout is refused naming it, never read as empty or overwritten (rule
/// 26): the content-addressed store's `head`; a plain head whose slot is of format 1 (the
/// layout whose entries held a kind's byte, before blaze-keyed's `Tag`) or 0; and a segment of
/// format 1, which [`check_segment`] refuses as [`StoreError::Format`], naming both formats.
#[test]
fn a_previous_layout_is_refused_by_name() {
    let refused = |head: Vec<u8>| {
        let mut sim = Sim::new(1, Faults::default());
        let file = FileName::new("plain/head").unwrap();
        assert!(matches!(ask(&mut sim, Effect::Write { file: file.clone(), bytes: head.clone(), tag: EffectTag(OWN) }), Done::Written));
        let mut r = [Actor::new(cfg(0, 1, false), Plant::None)];
        settle(&mut sim, &mut r);
        let why = r[0].store.broken().map(str::to_owned).expect("refused");
        assert_eq!(sim.contents(&file), head, "a refused head is left as it was");
        why
    };
    let old = refused(b"BLZHEAD\x03 a content-addressed store's head".to_vec());
    assert!(old.contains("plain/head") && old.contains("another store"), "{old}");
    for (format, at) in [(1u32, 0usize), (1, 1), (0, 0)] {
        let slot = Slot { generation: 2 + at as u64, log: 7, manifest_at: 0, manifest_len: 64, manifest_crc: 0 }.encode_as(format);
        let mut head = vec![0u8; format::HEAD_LEN];
        let start = format::SLOT_AT[at] as usize;
        head[start..start + format::SLOT].copy_from_slice(&slot);
        let why = refused(head);
        assert!(
            why.contains("plain/head") && why.contains(&format!("format {format}")) && why.contains(&format!("format {FORMAT}")),
            "slot {at} of format {format}: {why}"
        );
    }

    // A segment of format 1: the bytes of a whole segment of this format with only its format
    // word changed (its digest then fails too; the format is named first).
    let mut bytes = Vec::new_in(Global);
    seal_segment(Namespace::LOCAL, &[(RecordRef { tag: TAG, key: key(1).0 }, &value(1)[..])], &mut bytes).unwrap();
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(check_segment(&bytes), Err(StoreError::Format { what: "a plain store's segment", found: 1, wanted: FORMAT }));
}

/// The invariant: [`check_segment`] accepts what [`seal_segment`] wrote and describes exactly
/// the records sealed: their namespace and count, each distinct key once with the tag and
/// payload of its first record given (a later duplicate dropped), no key that was not given,
/// and the same records sealed twice are the same bytes. Zero records is a valid segment.
#[hegel::test]
fn check_segment_accepts_exactly_what_seal_segment_sealed(tc: hegel::TestCase) {
    let count = tc.draw(gs::integers::<u32>().min_value(0).max_value(60)) as usize;
    let namespace = writes()[tc.draw(gs::integers::<u32>().min_value(0).max_value(2)) as usize];
    let mut given: Vec<(u32, u16, u32)> = Vec::new_in(Global);
    for _ in 0..count {
        // Keys from a small range, so duplicates happen; each record its own tag and payload.
        let n = tc.draw(gs::integers::<u32>().min_value(0).max_value(40));
        let tag = tc.draw(gs::integers::<u16>());
        let payload = tc.draw(gs::integers::<u32>().min_value(0).max_value(5_000));
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

/// Compaction under a reader: past [`super::SEGMENTS_MOST`] segments the oldest are merged;
/// over the cap what no root names and is older than the window goes; a reader opened before
/// keeps reading what it read; every file a generation named stays until a grace period after
/// it was retired, and goes after it.
#[test]
fn compaction_keeps_roots_and_the_grace_period() {
    let mut sim = Sim::new(9, Faults::default());
    let mut w = [Actor::new(cfg(0, 1, false), Plant::None)];
    settle(&mut sim, &mut w);
    let mut named = BTreeSet::new();
    for n in 0..40u32 {
        w[0].store.put(key(n), Namespace::LOCAL, TAG, &value(n)).unwrap();
        if n == 39 {
            w[0].store.root("kept", &[key(1), key(2), key(3)]).unwrap();
        }
        let effects = &mut w[0].effects;
        w[0].store.commit(Vec::new_in(Global), sim.wall_ns(), effects).unwrap();
        settle(&mut sim, &mut w);
        named.extend(w[0].store.take_committed().unwrap().unwrap().segments);
    }
    let before = reader(&mut sim, 1, 1);
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
    let fresh = reader(&mut sim, 2, 1);
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
    let mut sim = Sim::new(13, Faults::default());
    let mut c = cfg(0, 1, false);
    c.cap_bytes = 1;
    let mut w = [Actor { store: Plain::planted(c, Plant::None), effects: Vec::new_in(Global) }];
    w[0].store.open(&mut w[0].effects);
    settle(&mut sim, &mut w);
    w[0].store.root("kept", &[key(0), key(1)]).unwrap();
    for n in 0..4u32 {
        w[0].store.put(key(n), Namespace::LOCAL, TAG, &value(n)).unwrap();
    }
    let effects = &mut w[0].effects;
    w[0].store.commit(Vec::new_in(Global), sim.wall_ns(), effects).unwrap();
    settle(&mut sim, &mut w);
    assert!(matches!(w[0].store.take_committed(), Some(Ok(_))));
    // Age it past the window.
    for n in 100..100 + super::WINDOW as u32 + 1 {
        w[0].store.put(key(n), Namespace::LOCAL, TAG, &value(n)).unwrap();
        let effects = &mut w[0].effects;
        w[0].store.commit(Vec::new_in(Global), sim.wall_ns(), effects).unwrap();
        settle(&mut sim, &mut w);
        assert!(matches!(w[0].store.take_committed(), Some(Ok(_))));
    }
    assert!(w[0].store.compact(&mut w[0].effects));
    settle(&mut sim, &mut w);
    let sliced = w[0].store.take_sliced().unwrap();
    assert!(sliced.dropped >= 2, "{sliced:?}");
    let r = reader(&mut sim, 1, 1);
    for n in [0, 1] {
        assert_eq!(r.store.get(&key(n)).map(|it| it.bytes.to_vec()), Some(value(n)), "rooted record {n}");
    }
    for n in [2, 3] {
        assert_eq!(r.store.get(&key(n)).map(|it| it.bytes.len()), None, "unrooted old record {n}");
    }
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
fn driven(n: u64, reads: Vec<Namespace>) -> Driven<Sim> {
    let mut c = cfg(n, 1, false);
    c.reads = reads;
    Driven::with(Sim::new(n, Faults::default()), Path::new("/sim"), c).unwrap()
}

fn refs(keys: std::ops::Range<u32>) -> Vec<RecordRef> {
    keys.map(|n| RecordRef { tag: TAG, key: key(n).0 }).collect()
}

/// A trusted builder's segment in the public namespace, its bytes as they lie, in a source; the
/// bytes are also [`seal_segment`]'s of the same records (the commit and the push's sealing are
/// one writer).
fn published(source: &LocalDir, builder: &Builder) -> (Vec<u8>, ContentId) {
    let mut ci = driven(0, reads());
    for n in 0..8 {
        ci.put(key(n), Namespace::PUBLIC, TAG, &value(n)).unwrap();
    }
    let committed = ci.commit_with(Vec::new_in(Global)).unwrap();
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
    let builder = Builder { secret: "the trusted builder's secret" };
    let at = scratch("fetch");
    let mut source = LocalDir::new(&at);
    let (bytes, digest) = published(&source, &builder);
    let keys = refs(0..8);

    let mut me = driven(1, reads());
    assert_eq!(me.get(&key(0)).map(|it| it.bytes.len()), None);
    let pulled = me.fetch_missing(&mut source, &builder, Namespace::PUBLIC, &keys).unwrap();
    assert_eq!(pulled.len(), 1);
    for n in 0..8 {
        assert_eq!(me.get(&key(n)).map(|it| (it.bytes.to_vec(), it.namespace)), Some((value(n), Namespace::PUBLIC)), "pulled record {n}");
        assert_eq!(me.core().get_in(&[Namespace::LOCAL, org()], &key(n)).map(|it| it.bytes.len()), None, "pulled record {n} answers for another namespace");
    }
    assert!(me.fetch_missing(&mut source, &builder, Namespace::PUBLIC, &keys).unwrap().is_empty(), "held records fetched again");

    // Forged: one payload byte changed, under the real name and statement.
    let mut forged = bytes.clone();
    forged[HEADER] ^= 1;
    let hex: String = digest.0.iter().map(|it| format!("{it:02x}")).collect();
    std::fs::write(at.join(format!("{hex}.seg")), &forged).unwrap();
    let mut other = driven(2, vec![Namespace::LOCAL, Namespace::PUBLIC]);
    let why = other.fetch_missing(&mut source, &builder, Namespace::PUBLIC, &keys).expect_err("a forged segment is refused");
    assert!(matches!(why, StoreError::Layout { segment, .. } if segment == SegmentId(digest)), "{why}");
    assert_eq!(other.get(&key(0)).map(|it| it.bytes.len()), None, "nothing of it was used");

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
    assert_eq!(other.get(&key(0)).map(|it| it.bytes.len()), None, "nothing of the lie was used");
    let _ = std::fs::remove_dir_all(&at);
}
