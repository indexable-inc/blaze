//! The store's scenario: real `blaze_cas::Store` and `Writer`s, IO on a [`CrashFs`], and a
//! restore checked after every IO step against every crash state.
//!
//! [`Run`] is the controller every scenario shares. It owns the store, as the daemon does, and
//! the file system. At each step the explorer picks one of: an actor's next step (a put, a
//! get, a store call inside a kernel get), an actor's request to flush, the wake of an actor
//! whose wait for durability has ended, an actor's answer, or the completion (or, within the
//! failure budget, the failure) of an effect in flight. After every step that moved IO, each
//! disk a crash may now leave is restored ([`restore`]) and checked.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use blaze_cas::{Config, Key, Kind, Mismatch, Reader, Store, Ticket, Writer, content_of};
use blaze_core::{Effect, FileName, lock::Lock};
use blaze_ids::{ContentId, Hasher};

use crate::actors::{Actors, Next, Resume, Turn};
use crate::explore::{Bounds, Chooser, Failure, Memo};
use crate::fs::{CrashFs, Disk, Persist, digest};
use crate::properties;

/// What is waited on before what a build left is kept for reuse across a crash: by what
/// settles after its answer ([`Op::Settle`]), or by a snapshot before it answers
/// ([`Op::AwaitAndAnswer`]). A build's answer itself waits on neither.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wait {
    /// The tickets of its own puts.
    Own,
    /// Every put any writer made before it asked: a value it read came with no ticket, and
    /// may be another writer's put not yet durable.
    Everything,
}

/// The model's settings for one scenario.
#[derive(Clone, Copy, Debug)]
pub struct Model {
    pub persist: Persist,
    /// IO failures the explorer may inject in one execution.
    pub failures: u32,
    /// Whether effects in flight complete in any order, or in the order asked.
    pub reorder_io: bool,
    pub wait: Wait,
}

/// What a kernel get ended in, for the restore's checks.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    Hit(Vec<u8>),
    Computed(Vec<u8>),
    Conflict { kept: ContentId, given: ContentId },
}

/// What every actor did, as the restore's checks need it.
#[derive(Default, Debug)]
pub struct Ledger {
    /// The first value accepted under each key, with its kind.
    pub accepted: BTreeMap<Key, (Kind, Vec<u8>)>,
    /// Each accepted put's ticket.
    pub tickets: Vec<(Ticket, Key)>,
    /// The keys of builds that answered, durable or not: an answer is no promise of them.
    pub answered: BTreeSet<Key>,
    /// The keys an answered build kept for reuse across a crash, as its settling ended
    /// durable ([`Op::Settle`]): what a restart may rely on.
    pub kept: BTreeSet<Key>,
    /// The actors whose settling ended durable, and kept.
    pub kept_actors: BTreeSet<String>,
    /// The actors whose settling ended with the store stopped, and said the loss.
    pub lost: BTreeSet<String>,
    /// Puts refused as a second value.
    pub refused: Vec<Mismatch>,
    /// Each kernel actor's world and outcome, by name.
    pub outcomes: BTreeMap<String, (usize, Outcome)>,
    /// The kernel actors that answered.
    pub answered_actors: BTreeSet<String>,
}

/// A key's name in a report: the first eight hex digits of its ID.
pub fn short(key: &Key) -> String {
    format!("{:x}", key.0)[..8].to_owned()
}

impl Ledger {
    /// Records `who`'s put of `bytes` under `key` and checks the store's answer: a second
    /// value is refused naming the first, never taken.
    pub fn put(
        &mut self,
        who: &str,
        key: Key,
        kind: Kind,
        bytes: &[u8],
        result: &Result<Ticket, Mismatch>,
    ) -> Result<(), Failure> {
        let given = content_of(bytes);
        let refused = properties::REFUSED_NOT_OVERWRITTEN;
        match (result, self.accepted.get(&key)) {
            (Ok(_), Some((_, had))) if had.as_slice() != bytes => Err(Failure::new(
                refused,
                format!(
                    "{who}'s put of {given:x} under {} was taken, though {:x} was put under it first",
                    short(&key),
                    content_of(had)
                ),
            )),
            (Ok(ticket), had) => {
                if had.is_none() {
                    self.accepted.insert(key, (kind, bytes.to_vec()));
                }
                self.tickets.push((*ticket, key));
                Ok(())
            }
            (Err(mismatch), None) => Err(Failure::new(
                refused,
                format!(
                    "{who}'s put under {} was refused as a second value ({mismatch:?}), though none was put under it",
                    short(&key)
                ),
            )),
            (Err(mismatch), Some((_, had))) => {
                let kept = content_of(had);
                if mismatch.key != key
                    || mismatch.kept != kept
                    || mismatch.given != given
                    || had.as_slice() == bytes
                {
                    return Err(Failure::new(
                        refused,
                        format!(
                            "{who}'s put of {given:x} under {} was refused as {mismatch:?}, where the value kept is {kept:x}",
                            short(&key)
                        ),
                    ));
                }
                self.refused.push(mismatch.clone());
                Ok(())
            }
        }
    }

    /// Checks a value a reader was served under `key` while the store ran.
    pub fn served(&self, who: &str, key: &Key, bytes: &[u8]) -> Option<Failure> {
        match self.accepted.get(key) {
            Some((_, want)) if want.as_slice() == bytes => None,
            Some((_, want)) => Some(Failure::new(
                properties::SERVES_ONLY_WRITTEN,
                format!(
                    "{who} was served {:x} under {}, where the value written is {:x}",
                    content_of(bytes),
                    short(key),
                    content_of(want)
                ),
            )),
            None => Some(Failure::new(
                properties::SERVES_ONLY_WRITTEN,
                format!(
                    "{who} was served {:x} under {}, which no writer put",
                    content_of(bytes),
                    short(key)
                ),
            )),
        }
    }

    /// Every accepted put's ticket.
    pub fn all_tickets(&self) -> Vec<Ticket> {
        self.tickets.iter().map(|it| it.0).collect()
    }
}

/// The store's config in every model: one directory, no layers.
pub fn config() -> Config {
    Config { dir: FileName::new("store").expect("a name"), layers: Vec::new(), tags: 1 << 40, load_tags: (1 << 40) + (1 << 32), loader: blaze_cas::Loader::Pumped }
}

/// Runs `effects` and what they lead to, each completing in the order asked, until none is
/// in flight; more than `most` completions is a wait that does not end.
pub fn drive(
    fs: &mut CrashFs,
    store: &mut Store,
    effects: &mut Vec<Effect>,
    most: usize,
    what: &str,
) -> Result<(), Failure> {
    for _ in 0..most {
        fs.submit(effects);
        if fs.is_idle() {
            return Ok(());
        }
        let completion = fs.complete(0);
        if store.owns(completion.tag) {
            store.complete(completion, effects);
        }
    }
    Err(Failure::new(
        properties::EVERY_WAIT_ENDS,
        format!(
            "{what}: its IO did not end in {most} completions; in flight: {}",
            fs.in_flight().join(", ")
        ),
    ))
}

/// A store opened on `fs`, broken or not.
pub fn open(fs: &mut CrashFs, bounds: &Bounds, what: &str) -> Result<Store, Failure> {
    let mut effects = Vec::new();
    let mut store = Store::open(config(), &mut effects);
    drive(fs, &mut store, &mut effects, bounds.steps, what)?;
    Ok(store)
}

/// Every value the store serves, by key.
pub fn contents(store: &Store, what: &str) -> Result<BTreeMap<Key, (Kind, Vec<u8>)>, Failure> {
    let keys: BTreeSet<Key> = store.keys().into_iter().collect();
    let mut out = BTreeMap::new();
    for key in keys {
        match store.get(&key) {
            Ok(Some(entry)) => {
                out.insert(key, (entry.kind, entry.bytes.to_vec()));
            }
            Ok(None) => {}
            Err(corrupt) => {
                return Err(Failure::new(
                    properties::SERVES_ONLY_WRITTEN,
                    format!("{what}: {corrupt}"),
                ));
            }
        }
    }
    Ok(out)
}

fn compare(
    got: &BTreeMap<Key, (Kind, Vec<u8>)>,
    want: &BTreeMap<Key, (Kind, Vec<u8>)>,
    what: &str,
) -> Result<(), Failure> {
    let keys: BTreeSet<&Key> = got.keys().chain(want.keys()).collect();
    for key in keys {
        let text = |it: Option<&(Kind, Vec<u8>)>| {
            it.map_or_else(
                || "nothing".to_owned(),
                |(kind, bytes)| format!("{} {:x}", kind.name(), content_of(bytes)),
            )
        };
        if got.get(key) != want.get(key) {
            return Err(Failure::new(
                properties::RESTORE_EQUALS_FRESH,
                format!(
                    "{what} holds {} under {}, where a fresh build holds {}",
                    text(got.get(key)),
                    short(key),
                    text(want.get(key))
                ),
            ));
        }
    }
    Ok(())
}

/// A fresh build of every accepted value, on an empty disk: what every restore, built again,
/// must equal.
pub fn fresh(
    ledger: &Ledger,
    model: &Model,
    bounds: &Bounds,
) -> Result<BTreeMap<Key, (Kind, Vec<u8>)>, Failure> {
    let mut fs = CrashFs::new(model.persist);
    let mut store = open(&mut fs, bounds, "a fresh build's open")?;
    for (key, (kind, bytes)) in &ledger.accepted {
        if let Err(mismatch) = store.put(*key, *kind, bytes) {
            return Err(Failure::new(
                properties::REFUSED_NOT_OVERWRITTEN,
                format!("a fresh build refused {mismatch:?}"),
            ));
        }
    }
    let mut effects = Vec::new();
    store.flush(&mut effects);
    drive(&mut fs, &mut store, &mut effects, bounds.steps, "a fresh build's flush")?;
    contents(&store, "a fresh build")
}

/// What a scenario checks of a restore beyond the store's own checks: a kernel's lookups.
pub type Extra<'a> = &'a dyn Fn(&Store, &Disk, &Ledger, &Bounds) -> Result<(), Failure>;

/// No checks beyond the store's.
pub fn no_extra(_: &Store, _: &Disk, _: &Ledger, _: &Bounds) -> Result<(), Failure> {
    Ok(())
}

/// The keys that must survive a crash now: those whose put the store calls durable, and
/// those an answered build kept for reuse. Not those a build merely answered with: an answer
/// is sent on results not yet durable, and a restart works out again what a crash lost.
pub fn must_survive(store: &Store, ledger: &Ledger) -> BTreeSet<Key> {
    ledger
        .tickets
        .iter()
        .filter(|(ticket, _)| store.durable(*ticket))
        .map(|(_, key)| *key)
        .chain(ledger.kept.iter().copied())
        .collect()
}

/// A process started on `disk` after a crash, checked: it opens, serves only values some
/// writer fully wrote, has every value that had to survive, and, built again, equals a fresh
/// build, also after a clean stop and start.
pub fn restore(
    disk: &Disk,
    ledger: &Ledger,
    survive: &BTreeSet<Key>,
    model: &Model,
    bounds: &Bounds,
    extra: Extra<'_>,
) -> Result<(), Failure> {
    let mut fs = CrashFs::from_disk(model.persist, disk);
    let mut store = open(&mut fs, bounds, "the restore's open")?;
    if let Some(broken) = store.broken() {
        return Err(Failure::new(
            properties::RESTORE_EQUALS_FRESH,
            format!(
                "the restore does not open: {broken}. A crash must leave a store that opens with what was durable"
            ),
        ));
    }
    if !store.ready() {
        return Err(Failure::new(
            properties::EVERY_WAIT_ENDS,
            "the restore's IO ended and it is not ready",
        ));
    }
    let held = contents(&store, "the restore")?;
    for (key, (_, bytes)) in &held {
        if let Some(failure) = ledger.served("the restore", key, bytes) {
            return Err(failure);
        }
    }
    let corrupt = store.verify(None);
    if let Some(first) = corrupt.first() {
        return Err(Failure::new(
            properties::SERVES_ONLY_WRITTEN,
            format!("the restore's verify found {} corrupt: {first}", corrupt.len()),
        ));
    }
    // What an answered build kept first: the most specific loss.
    if let Some(key) =
        ledger.kept.iter().find(|it| survive.contains(*it) && !held.contains_key(*it))
    {
        return Err(Failure::new(
            properties::KEPT_SURVIVES,
            format!(
                "{} was lost: an answered build kept it for reuse before the crash, and it was not durable",
                short(key)
            ),
        ));
    }
    if held.is_empty() && !survive.is_empty() {
        return Err(Failure::new(
            properties::NEVER_SILENTLY_EMPTY,
            format!(
                "the restore opened empty and said nothing, though {} values were durable or answered",
                survive.len()
            ),
        ));
    }
    for key in survive {
        if !held.contains_key(key) {
            return Err(Failure::new(
                properties::KEPT_SURVIVES,
                format!(
                    "{} was lost: the store called its put durable before the crash",
                    short(key)
                ),
            ));
        }
    }
    extra(&store, disk, ledger, bounds)?;
    for (key, (kind, bytes)) in &ledger.accepted {
        if let Err(mismatch) = store.put(*key, *kind, bytes) {
            return Err(Failure::new(
                properties::RESTORE_EQUALS_FRESH,
                format!(
                    "building again on the restore, the put under {} was refused: {mismatch:?}",
                    short(key)
                ),
            ));
        }
    }
    let mut effects = Vec::new();
    store.flush(&mut effects);
    drive(&mut fs, &mut store, &mut effects, bounds.steps, "the restore's flush")?;
    let fresh = fresh(ledger, model, bounds)?;
    compare(&contents(&store, "the restore, built again")?, &fresh, "the restore, built again")?;
    drop(store);
    let mut again = CrashFs::from_disk(model.persist, &fs.synced_disk());
    let store = open(&mut again, bounds, "the restore's restart")?;
    if let Some(broken) = store.broken() {
        return Err(Failure::new(
            properties::RESTORE_EQUALS_FRESH,
            format!("the restore, built again, does not open after a clean stop: {broken}"),
        ));
    }
    compare(
        &contents(&store, "the restore's restart")?,
        &fresh,
        "the restore, built again and started after a clean stop",
    )
}

/// Who takes a step: an actor, or the IO.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Agent {
    Actor(usize),
    Io,
}

enum Action {
    Resume(usize, Resume),
    Flush(usize),
    Answer(usize),
    Complete(usize),
    Fail(usize),
}

impl Action {
    fn agent(&self) -> Agent {
        match self {
            Action::Resume(id, _) | Action::Flush(id) | Action::Answer(id) => Agent::Actor(*id),
            Action::Complete(_) | Action::Fail(_) => Agent::Io,
        }
    }
}

/// One execution's controller: see the module's docs.
pub struct Run<'a> {
    /// First, so its threads are joined before the store goes.
    pub actors: Actors,
    pub store: Store,
    pub ledger: Arc<Lock<Ledger>>,
    fs: CrashFs,
    effects: Vec<Effect>,
    model: &'a Model,
    bounds: &'a Bounds,
    chooser: &'a mut Chooser,
    memo: &'a mut Memo,
    failures_left: u32,
    extra: Extra<'a>,
}

impl<'a> Run<'a> {
    /// A store opened on an empty disk.
    pub fn new(
        model: &'a Model,
        bounds: &'a Bounds,
        chooser: &'a mut Chooser,
        memo: &'a mut Memo,
        extra: Extra<'a>,
    ) -> Result<Run<'a>, Failure> {
        let mut fs = CrashFs::new(model.persist);
        let store = open(&mut fs, bounds, "the first open")?;
        if let Some(broken) = store.broken() {
            return Err(Failure::new(
                properties::RESTORE_EQUALS_FRESH,
                format!("a store on an empty disk does not open: {broken}"),
            ));
        }
        Ok(Run {
            actors: Actors::default(),
            store,
            ledger: Arc::new(Lock::new("storemodel.ledger", Ledger::default())),
            fs,
            effects: Vec::new(),
            model,
            bounds,
            chooser,
            memo,
            failures_left: model.failures,
            extra,
        })
    }

    /// Steps until no step is left, then checks that every actor ended.
    pub fn run(mut self) -> Result<(), Failure> {
        let mut last: Option<Agent> = None;
        let mut preemptions = 0u32;
        loop {
            let mut options: Vec<String> = Vec::new();
            let mut actions: Vec<Action> = Vec::new();
            for id in 0..self.actors.len() {
                let name = self.actors.name(id);
                match self.actors.next(id) {
                    Next::Step(label) => {
                        options.push(format!("{name}: {label}"));
                        actions.push(Action::Resume(id, Resume::Go));
                    }
                    Next::Flush => {
                        options.push(format!("{name}: flush"));
                        actions.push(Action::Flush(id));
                    }
                    Next::Await(tickets) => {
                        if tickets.iter().all(|it| self.store.durable(*it)) {
                            options.push(format!("{name}: wakes, durable"));
                            actions.push(Action::Resume(id, Resume::Durable));
                        } else if self.store.broken().is_some() {
                            options.push(format!("{name}: wakes, the store stopped"));
                            actions.push(Action::Resume(id, Resume::Broken));
                        }
                    }
                    Next::Answer(_) => {
                        options.push(format!("{name}: answers"));
                        actions.push(Action::Answer(id));
                    }
                    Next::Done => {}
                }
            }
            let flying = self.fs.in_flight();
            let open = if self.model.reorder_io { flying.len() } else { flying.len().min(1) };
            for (at, effect) in flying.iter().enumerate().take(open) {
                options.push(format!("io: {effect}"));
                actions.push(Action::Complete(at));
                if self.failures_left > 0 {
                    options.push(format!("io: fail {effect}"));
                    actions.push(Action::Fail(at));
                }
            }
            if options.is_empty() {
                break;
            }
            let agents: Vec<Agent> = actions.iter().map(Action::agent).collect();
            let last_can_go = last.is_some_and(|it| agents.contains(&it));
            if last_can_go && self.bounds.preemptions.is_some_and(|most| preemptions >= most) {
                // Past the bound, whoever went last goes on.
                (options, actions) = options
                    .into_iter()
                    .zip(actions)
                    .zip(&agents)
                    .filter(|(_, agent)| Some(**agent) == last)
                    .map(|(pair, _)| pair)
                    .unzip();
            }
            // Distinct names, so a schedule replays by them.
            let mut seen = BTreeMap::<String, usize>::new();
            for option in &mut options {
                let n = seen.entry(option.clone()).or_insert(0);
                *n += 1;
                if *n > 1 {
                    option.push_str(&format!(" #{n}"));
                }
            }
            let pick = self.chooser.pick(&options)?;
            let action = actions.swap_remove(pick);
            if last_can_go && Some(action.agent()) != last {
                preemptions += 1;
            }
            last = Some(action.agent());
            let moved_io = match action {
                Action::Resume(id, how) => {
                    self.actors.resume(id, how)?;
                    false
                }
                Action::Flush(id) => {
                    self.store.flush(&mut self.effects);
                    self.fs.submit(&mut self.effects);
                    self.actors.resume(id, Resume::Go)?;
                    true
                }
                Action::Answer(id) => {
                    if let Next::Answer(keys) = self.actors.next(id) {
                        let mut ledger = self.ledger.lock();
                        ledger.answered.extend(keys.iter().copied());
                        ledger.answered_actors.insert(self.actors.name(id).to_owned());
                    }
                    self.actors.resume(id, Resume::Go)?;
                    false
                }
                Action::Complete(at) => {
                    let completion = self.fs.complete(at);
                    if self.store.owns(completion.tag) {
                        self.store.complete(completion, &mut self.effects);
                    }
                    self.fs.submit(&mut self.effects);
                    true
                }
                Action::Fail(at) => {
                    self.failures_left -= 1;
                    let completion = self.fs.fail(at);
                    if self.store.owns(completion.tag) {
                        self.store.complete(completion, &mut self.effects);
                    }
                    self.fs.submit(&mut self.effects);
                    true
                }
            };
            if moved_io {
                self.check_crashes()?;
            }
        }
        // The last answers, against the disk as the last IO left it.
        self.check_crashes()?;
        for id in 0..self.actors.len() {
            match self.actors.next(id) {
                Next::Done => {}
                Next::Await(tickets) => {
                    let pending: Vec<String> = tickets
                        .iter()
                        .filter(|it| !self.store.durable(**it))
                        .map(ToString::to_string)
                        .collect();
                    return Err(Failure::new(
                        properties::EVERY_WAIT_ENDS,
                        format!(
                            "{} waits for {} to be durable, and no IO is in flight and no flush asked to make it so",
                            self.actors.name(id),
                            pending.join(", ")
                        ),
                    ));
                }
                other => unreachable!("an actor whose next step is {other:?} has a step to take"),
            }
        }
        // Every answer's settling ended one way or the other: kept, or said lost.
        let ledger = self.ledger.lock();
        if let Some(name) = ledger
            .answered_actors
            .iter()
            .find(|it| !ledger.kept_actors.contains(*it) && !ledger.lost.contains(*it))
        {
            return Err(Failure::new(
                properties::LOSS_IS_SAID,
                format!(
                    "{name} answered, and what it left was neither kept once durable nor said lost: it was dropped silently"
                ),
            ));
        }
        Ok(())
    }

    /// Every disk a crash now may leave, restored and checked, each once an exploration.
    fn check_crashes(&mut self) -> Result<(), Failure> {
        let ledger = self.ledger.lock();
        let survive = must_survive(&self.store, &ledger);
        let mut hasher = Hasher::new("blaze storemodel expectation 1");
        hasher.update(format!("{:?} {:?} {:?}", self.model.persist, *ledger, survive).as_bytes());
        let expected = hasher.finish();
        let states = self
            .fs
            .crash_states(self.bounds.crash_states)
            .map_err(|it| Failure::new(properties::EXPLORED, it.to_string()))?;
        for state in states {
            let mut id = Hasher::new("blaze storemodel crash check 1");
            id.id(&digest(&state.disk)).id(&expected);
            if !self.memo.first(id.finish()) {
                continue;
            }
            let how = match state.how.is_empty() {
                true => "everything pending kept".to_owned(),
                false => state.how.clone(),
            };
            restore(&state.disk, &ledger, &survive, self.model, self.bounds, self.extra)
                .map_err(|it| it.at_crash(format!("after step {}: {how}", self.chooser.steps())))?;
        }
        Ok(())
    }
}

/// One step of a writer's script.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// Puts value `value` under key `key`.
    Put {
        key: u8,
        value: u8,
    },
    Flush,
    /// Answers at once, durable or not: a build's answer waits for no store.
    Answer,
    /// What the build left settles after its answer: waits for what the model's [`Wait`]
    /// says, then keeps it for reuse across a crash; or, the store stopped, says the loss.
    Settle,
    /// Waits for what the model's [`Wait`] says, then answers and keeps: a snapshot, which
    /// is asked for durability.
    AwaitAndAnswer,
    /// Reads key `key`.
    Get {
        key: u8,
    },
    /// Keeps what it put for reuse across a crash at once, durable or not: a bug the model
    /// must find (its own test).
    KeepUnwaited,
    /// [`Op::Settle`] that says nothing when the store stopped: a bug the model must find
    /// (its own test).
    SettleSilently,
}

/// A store actor: its name and script.
#[derive(Clone, Debug)]
pub struct Script {
    pub name: &'static str,
    pub ops: Vec<Op>,
}

/// The model's key `n`.
pub fn key(n: u8) -> Key {
    Key(ContentId::of("blaze storemodel key 1", &[n]))
}

/// The model's value `v`: lengths differ, so torn values differ too.
pub fn value(v: u8) -> Vec<u8> {
    (0..40 + 7 * usize::from(v)).map(|it| v.wrapping_mul(31).wrapping_add(it as u8)).collect()
}

/// The store scenario of `scripts`, one execution.
pub fn run_store(
    scripts: &[Script],
    model: &Model,
    bounds: &Bounds,
    chooser: &mut Chooser,
    memo: &mut Memo,
) -> Result<(), Failure> {
    let mut run = Run::new(model, bounds, chooser, memo, &no_extra)?;
    for script in scripts {
        let writer = run.store.writer();
        let reader = run.store.reader();
        let ledger = Arc::clone(&run.ledger);
        let (name, ops, wait) = (script.name, script.ops.clone(), model.wait);
        run.actors
            .spawn(name, move |turn| writes(turn, name, writer, &reader, &ledger, &ops, wait))?;
    }
    run.run()
}

fn writes(
    turn: &Turn,
    name: &str,
    mut writer: Writer,
    reader: &Reader,
    ledger: &Lock<Ledger>,
    ops: &[Op],
    wait: Wait,
) {
    let guard = blaze_cas::enter();
    let mut mine: Vec<Ticket> = Vec::new();
    let mut kept: Vec<Key> = Vec::new();
    for op in ops {
        match *op {
            Op::Put { key: k, value: v } => {
                turn.pause(format!("put k{k} = v{v}"));
                let bytes = value(v);
                let result = writer.put(&guard, key(k), Kind::Query, &bytes);
                let checked = ledger.lock().put(name, key(k), Kind::Query, &bytes, &result);
                if let Err(failure) = checked {
                    turn.fail(failure);
                }
                if let Ok(ticket) = result {
                    mine.push(ticket);
                    kept.push(key(k));
                }
            }
            Op::Flush => turn.flush(),
            Op::AwaitAndAnswer => {
                let tickets = match wait {
                    Wait::Own => mine.clone(),
                    Wait::Everything => ledger.lock().all_tickets(),
                };
                match turn.await_durable(tickets) {
                    Resume::Broken => return,
                    Resume::Go | Resume::Durable => {
                        keep(ledger, name, &kept);
                        turn.answer(kept.clone());
                    }
                }
            }
            Op::Answer => turn.answer(kept.clone()),
            Op::Settle | Op::SettleSilently => {
                let tickets = match wait {
                    Wait::Own => mine.clone(),
                    Wait::Everything => ledger.lock().all_tickets(),
                };
                match turn.await_durable(tickets) {
                    Resume::Broken if *op == Op::SettleSilently => return,
                    Resume::Broken => {
                        ledger.lock().lost.insert(name.to_owned());
                        return;
                    }
                    Resume::Go | Resume::Durable => keep(ledger, name, &kept),
                }
            }
            Op::KeepUnwaited => keep(ledger, name, &kept),
            Op::Get { key: k } => {
                turn.pause(format!("get k{k}"));
                let failure = match reader.get(&guard, &key(k)) {
                    Ok(None) => None,
                    Ok(Some(entry)) => ledger.lock().served(name, &key(k), entry.bytes),
                    Err(corrupt) => Some(Failure::new(
                        properties::SERVES_ONLY_WRITTEN,
                        format!("{name} was served a corrupt value: {corrupt}"),
                    )),
                };
                if let Some(failure) = failure {
                    turn.fail(failure);
                }
            }
        }
    }
}

/// `who` keeps `keys` for reuse across a crash: what a restart may rely on from here.
pub fn keep(ledger: &Lock<Ledger>, who: &str, keys: &[Key]) {
    let mut ledger = ledger.lock();
    ledger.kept.extend(keys.iter().copied());
    ledger.kept_actors.insert(who.to_owned());
}

/// A disk changed after the process stopped, as no crash makes it: the store must stop and
/// say why, or serve everything that had to survive. Never open empty and say nothing.
pub fn damaged(
    disk: &Disk,
    survive: &BTreeSet<Key>,
    ledger: &Ledger,
    model: &Model,
    bounds: &Bounds,
) -> Result<(), Failure> {
    let mut fs = CrashFs::from_disk(model.persist, disk);
    let store = open(&mut fs, bounds, "the open of a damaged disk")?;
    if store.broken().is_some() {
        return Ok(());
    }
    let held = contents(&store, "the open of a damaged disk")?;
    for (key, (_, bytes)) in &held {
        if let Some(failure) = ledger.served("the open of a damaged disk", key, bytes) {
            return Err(failure);
        }
    }
    let lost: Vec<String> =
        survive.iter().filter(|it| !held.contains_key(*it)).map(short).collect();
    match (lost.is_empty(), held.is_empty()) {
        (true, _) => Ok(()),
        (false, true) => Err(Failure::new(
            properties::NEVER_SILENTLY_EMPTY,
            format!(
                "it opened empty and ready, saying nothing, though {} durable values were on the disk",
                survive.len()
            ),
        )),
        (false, false) => Err(Failure::new(
            properties::NEVER_SILENTLY_EMPTY,
            format!(
                "it opened ready, saying nothing, without {} of the durable values: {}",
                lost.len(),
                lost.join(", ")
            ),
        )),
    }
}
