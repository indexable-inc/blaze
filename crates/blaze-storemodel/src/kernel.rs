//! The kernel's scenario: real `Kernel::get`s on real `Writer`s, each store call a schedule
//! point, so two gets of one identity race for one shape slot, and a restore's lookups are
//! checked after every crash.
//!
//! [`Dialed`] reads a dial, then the fact the dial names: two worlds with different dials give
//! one identity two shapes of reads, so two gets race to keep theirs in slot 0, and the loser's
//! put is a `Mismatch` the kernel takes as "another writer took the slot" and moves on.
//! [`Salted`] reads the actor's salt behind the kernel's back: an impure step, whose second
//! value under one identity and reads must be refused (`KernelError::Conflict`).

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use blaze_cas::{Guard, Key, Kind, Mismatch, Reader, Store, Ticket, Writer};
use blaze_ids::ContentId;
use blaze_kernel::{
    CodeId, Compute, Fact, Get, Got, Kernel, KernelError, Put, Read, ReadKind, Reading, Reads,
    Seen, Step, StepError, StepId, Subject, World, WorldError,
};
use blaze_store_check::Setting;
use blaze_core::lock::Lock;

use crate::actors::{Resume, Turn};
use crate::explore::{Bounds, Chooser, Failure, Memo};
use crate::fs::{CrashFs, Disk};
use crate::properties;
use crate::store::{Ledger, Model, Outcome, Run, Wait, keep, open, short};

const DIAL: ReadKind = ReadKind(1);
const FACT: ReadKind = ReadKind(2);
const HASHED: &str = "blaze storemodel fact 1";

/// A world: a dial, and four facts.
#[derive(Clone, Copy, Debug)]
pub struct Dials {
    pub dial: u8,
    pub facts: [u8; 4],
}

impl World for Dials {
    fn hash(&self, read: Read<'_>) -> Result<Seen, WorldError> {
        match (read.kind, read.key) {
            (DIAL, []) => Ok(Seen::Is(ContentId::of(HASHED, &[0xd1, self.dial]))),
            (FACT, &[at]) => {
                Ok(Seen::Is(ContentId::of(HASHED, &[at, self.facts[usize::from(at) % 4]])))
            }
            _ => Err(WorldError::new(read, "the model's world has no such read")),
        }
    }
}

struct Dial;

impl Fact<Dials> for Dial {
    const KIND: ReadKind = DIAL;
    type Key = ();
    type Value<'w> = u8;

    fn key(_: &(), _: &mut Vec<u8>) {}

    fn get(world: &Dials, _: &()) -> Result<(u8, Seen), WorldError> {
        Ok((world.dial, world.hash(Read { kind: DIAL, key: &[] })?))
    }
}

struct Value;

impl Fact<Dials> for Value {
    const KIND: ReadKind = FACT;
    type Key = u8;
    type Value<'w> = u8;

    fn key(at: &u8, out: &mut Vec<u8>) {
        out.push(*at);
    }

    fn get(world: &Dials, at: &u8) -> Result<(u8, Seen), WorldError> {
        Ok((world.facts[usize::from(*at) % 4], world.hash(Read { kind: FACT, key: &[*at] })?))
    }
}

fn subject(input: u8) -> Subject {
    Subject { of: ContentId::of("blaze storemodel input 1", &[input]), flags: ContentId::default() }
}

/// Reads the dial, then the fact it names.
pub struct Dialed;

impl Step for Dialed {
    const ID: StepId = StepId(0x7e01);
    const NAME: &'static str = "storemodel dialed";
    const KIND: Kind = Kind::Query;
    type Input = u8;

    fn subject(input: &u8) -> Subject {
        subject(*input)
    }

    fn label(input: &u8) -> String {
        format!("dialed {input}")
    }
}

impl Compute for Dialed {
    type World = Dials;

    fn compute(ctx: &mut Reads<'_, Dials>, input: &u8) -> Result<Vec<u8>, StepError> {
        let dial = ctx.get::<Dial>(&())?;
        let fact = ctx.get::<Value>(&dial)?;
        Ok(vec![*input, dial, fact])
    }
}

thread_local! {
    /// What [`Salted`] reads behind the kernel's back: each actor's own.
    static SALT: Cell<u8> = const { Cell::new(0) };
}

/// Reads a fact, and the actor's salt behind the kernel's back: not pure.
pub struct Salted;

impl Step for Salted {
    const ID: StepId = StepId(0x7e02);
    const NAME: &'static str = "storemodel salted";
    const KIND: Kind = Kind::Query;
    type Input = u8;

    fn subject(input: &u8) -> Subject {
        subject(*input)
    }

    fn label(input: &u8) -> String {
        format!("salted {input}")
    }
}

impl Compute for Salted {
    type World = Dials;

    #[expect(
        blaze::pure,
        reason = "the impure step the scenario needs: it reads the actor's salt behind the kernel's back, and the kernel must refuse its second value"
    )]
    fn compute(ctx: &mut Reads<'_, Dials>, input: &u8) -> Result<Vec<u8>, StepError> {
        let fact = ctx.get::<Value>(input)?;
        Ok(vec![fact, SALT.with(Cell::get)])
    }
}

/// Which step a scenario's actors get.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Which {
    Dialed,
    Salted,
}

/// One kernel actor: a build that gets `input` in world `world` with its salt, flushes, waits,
/// and answers; or, with `finds`, a reader that only looks it up.
#[derive(Clone, Copy, Debug)]
pub struct Asker {
    pub name: &'static str,
    pub input: u8,
    pub world: usize,
    pub salt: u8,
    pub finds: bool,
}

/// A kernel scenario.
#[derive(Clone, Debug)]
pub struct KernelScenario {
    pub which: Which,
    pub worlds: Vec<Dials>,
    pub askers: Vec<Asker>,
}

/// The domain of each step's code identity.
const CODE_DOMAIN: &str = "blaze storemodel code 1";

/// The kernel every model uses: both steps registered.
pub fn kernel() -> Kernel {
    let mut kernel = Kernel::new();
    kernel
        .register::<Dialed>(CodeId(ContentId::of(CODE_DOMAIN, b"dialed")))
        .expect("one registration");
    kernel
        .register::<Salted>(CodeId(ContentId::of(CODE_DOMAIN, b"salted")))
        .expect("one registration");
    kernel
}

/// The value a pure step makes of `input` in `world`.
fn dialed(input: u8, world: &Dials) -> Vec<u8> {
    vec![input, world.dial, world.facts[usize::from(world.dial) % 4]]
}

/// A worker's store as the kernel sees it, each put a schedule point, each accepted put
/// recorded in the ledger and each refusal checked.
struct Gated<'g> {
    reader: &'g Reader,
    writer: &'g mut Writer,
    guard: &'g Guard,
    turn: &'g Turn,
    ledger: &'g Lock<Ledger>,
    who: &'g str,
    tickets: Vec<Ticket>,
    kept: Vec<Key>,
}

impl Get for Gated<'_> {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        Ok(self.reader.get(self.guard, key).map_err(|it| it.to_string())?.map(|it| it.bytes))
    }

    fn has(&self, key: &Key) -> Result<bool, String> {
        Ok(self.reader.has(self.guard, key))
    }
}

impl Put for Gated<'_> {
    fn put(&mut self, key: Key, kind: Kind, bytes: Vec<u8>) -> Result<(), Mismatch> {
        self.turn.pause(format!("put {}", short(&key)));
        let result = self.writer.put(self.guard, key, kind, &bytes);
        let checked = self.ledger.lock().put(self.who, key, kind, &bytes, &result);
        if let Err(failure) = checked {
            self.turn.fail(failure);
        }
        let ticket = result?;
        self.tickets.push(ticket);
        self.kept.push(key);
        Ok(())
    }
}

fn outcome(got: Result<Got<'_>, KernelError>) -> Result<Outcome, String> {
    match got {
        Ok(Got::Hit(entry)) => Ok(Outcome::Hit(entry.value.to_vec())),
        Ok(Got::Computed { value, .. }) => Ok(Outcome::Computed(value)),
        Err(KernelError::Conflict { kept, given, .. }) => Ok(Outcome::Conflict { kept, given }),
        Err(error) => Err(error.to_string()),
    }
}

/// A kernel get's outcome; an error other than a conflict, rendered.
fn get<P: Put + ?Sized>(
    which: Which,
    kernel: &Kernel,
    store: &mut P,
    world: &Dials,
    input: u8,
) -> Result<Outcome, String> {
    match which {
        Which::Dialed => outcome(kernel.get::<Dialed, P>(store, world, &input, &Setting::OFF)),
        Which::Salted => outcome(kernel.get::<Salted, P>(store, world, &input, &Setting::OFF)),
    }
}

/// A kernel lookup's value; an error, rendered.
fn find<G: Get + ?Sized>(
    which: Which,
    kernel: &Kernel,
    store: &G,
    world: &Dials,
    input: u8,
) -> Result<Option<Vec<u8>>, String> {
    let found = match which {
        Which::Dialed => kernel.find::<Dialed, G>(store, world, &input),
        Which::Salted => kernel.find::<Salted, G>(store, world, &input),
    };
    Ok(found.map_err(|it| it.to_string())?.map(|it| it.value.to_vec()))
}

/// The values a lookup of `input` in world `world` may give: the pure step's, or what the
/// impure step makes with the salt of an actor that gets it there (one that has not finished
/// may already have kept its value).
fn allowed(scenario: &KernelScenario, input: u8, world: usize) -> BTreeSet<Vec<u8>> {
    let dials = &scenario.worlds[world];
    match scenario.which {
        Which::Dialed => BTreeSet::from([dialed(input, dials)]),
        Which::Salted => scenario
            .askers
            .iter()
            .filter(|it| !it.finds && it.world == world && it.input == input)
            .map(|it| vec![dials.facts[usize::from(input) % 4], it.salt])
            .collect(),
    }
}

fn asks(
    turn: &Turn,
    asker: Asker,
    which: Which,
    wait: Wait,
    kernel: &Kernel,
    world: &Dials,
    mut writer: Writer,
    reader: &Reader,
    ledger: &Lock<Ledger>,
) {
    let guard = blaze_cas::enter();
    SALT.with(|it| it.set(asker.salt));
    if asker.finds {
        turn.pause(format!("find {}", asker.input));
        let found = find(which, kernel, &Reading { reader, guard: &guard }, world, asker.input);
        let failure = match found {
            Err(error) => Some(Failure::new(
                properties::KERNEL_GETS_END,
                format!("{}'s lookup failed: {error}", asker.name),
            )),
            Ok(Some(value)) if which == Which::Dialed && value != dialed(asker.input, world) => {
                Some(Failure::new(
                    properties::SERVES_ONLY_WRITTEN,
                    format!(
                        "{}'s lookup found {value:?}, which its world does not make",
                        asker.name
                    ),
                ))
            }
            Ok(_) => None,
        };
        if let Some(failure) = failure {
            turn.fail(failure);
        }
        return;
    }
    turn.pause(format!("get {}", asker.input));
    let mut gated = Gated {
        reader,
        writer: &mut writer,
        guard: &guard,
        turn,
        ledger,
        who: asker.name,
        tickets: Vec::new(),
        kept: Vec::new(),
    };
    let got = get(which, kernel, &mut gated, world, asker.input);
    let (tickets, kept) = (gated.tickets, gated.kept);
    let failure = match &got {
        Err(error) => Some(Failure::new(
            properties::KERNEL_GETS_END,
            format!("{}'s get failed: {error}", asker.name),
        )),
        Ok(Outcome::Conflict { .. }) if which == Which::Dialed => Some(Failure::new(
            properties::REFUSED_NOT_OVERWRITTEN,
            format!("{}'s get of a pure step was refused as a conflict: {got:?}", asker.name),
        )),
        Ok(Outcome::Hit(value) | Outcome::Computed(value))
            if which == Which::Dialed && *value != dialed(asker.input, world) =>
        {
            Some(Failure::new(
                properties::SERVES_ONLY_WRITTEN,
                format!("{}'s get gave {value:?}, which its world does not make", asker.name),
            ))
        }
        Ok(_) => None,
    };
    if let Some(failure) = failure {
        turn.fail(failure);
    }
    if let Ok(outcome) = got {
        ledger.lock().outcomes.insert(asker.name.to_owned(), (asker.world, outcome));
    }
    // The answer first, durable or not; what the get left settles after it: kept for reuse
    // once what it waits for is durable, or said lost when the store stopped.
    turn.answer(kept.clone());
    turn.flush();
    let tickets = match wait {
        Wait::Own => tickets,
        Wait::Everything => ledger.lock().all_tickets(),
    };
    match turn.await_durable(tickets) {
        Resume::Broken => {
            ledger.lock().lost.insert(asker.name.to_owned());
        }
        Resume::Go | Resume::Durable => keep(ledger, asker.name, &kept),
    }
}

/// The kernel's checks of a restore: each lookup finds nothing or a value some actor's world
/// makes, an actor that kept what it answered with still finds its value, and the kernel run again on the
/// restore gives what it gives on an empty store.
fn check(
    scenario: &KernelScenario,
    kernel: &Kernel,
    model: &Model,
    store: &Store,
    disk: &Disk,
    ledger: &Ledger,
    bounds: &Bounds,
) -> Result<(), Failure> {
    for asker in scenario.askers.iter().filter(|it| !it.finds) {
        let world = &scenario.worlds[asker.world];
        let allowed = allowed(scenario, asker.input, asker.world);
        let found = find(scenario.which, kernel, store, world, asker.input).map_err(|error| {
            Failure::new(
                properties::KERNEL_GETS_END,
                format!("a lookup on the restore failed: {error}"),
            )
        })?;
        match found {
            Some(value) if !allowed.contains(&value) => {
                return Err(Failure::new(
                    properties::SERVES_ONLY_WRITTEN,
                    format!(
                        "a lookup of {}'s input on the restore found {value:?}, which no actor made in its world",
                        asker.name
                    ),
                ));
            }
            Some(_) => {}
            None if ledger.kept_actors.contains(asker.name) => {
                let outcome = ledger.outcomes.get(asker.name).map(|it| &it.1);
                if matches!(outcome, Some(Outcome::Hit(_) | Outcome::Computed(_))) {
                    return Err(Failure::new(
                        properties::KEPT_SURVIVES,
                        format!(
                            "{} kept what it answered with ({outcome:?}), and after the crash a lookup of its input in its world misses",
                            asker.name
                        ),
                    ));
                }
            }
            None => {}
        }
    }
    // The kernel run again on the restore, and on an empty store.
    let mut fs = CrashFs::from_disk(model.persist, disk);
    let mut again = open(&mut fs, bounds, "the restore, for the kernel")?;
    let mut fs = CrashFs::new(model.persist);
    let mut fresh = open(&mut fs, bounds, "a fresh store, for the kernel")?;
    let mut values: BTreeMap<&str, (Outcome, Outcome)> = BTreeMap::new();
    for asker in scenario.askers.iter().filter(|it| !it.finds) {
        let world = &scenario.worlds[asker.world];
        let on_restore =
            get(scenario.which, kernel, &mut again, world, asker.input).map_err(|error| {
                Failure::new(
                    properties::KERNEL_GETS_END,
                    format!("{}'s get again on the restore failed: {error}", asker.name),
                )
            })?;
        let on_fresh =
            get(scenario.which, kernel, &mut fresh, world, asker.input).map_err(|error| {
                Failure::new(
                    properties::KERNEL_GETS_END,
                    format!("{}'s get on a fresh store failed: {error}", asker.name),
                )
            })?;
        values.insert(asker.name, (on_restore, on_fresh));
    }
    if scenario.which == Which::Dialed {
        for (name, (on_restore, on_fresh)) in &values {
            let value = |it: &Outcome| match it {
                Outcome::Hit(value) | Outcome::Computed(value) => Some(value.clone()),
                Outcome::Conflict { .. } => None,
            };
            if value(on_restore) != value(on_fresh) || value(on_restore).is_none() {
                return Err(Failure::new(
                    properties::RESTORE_EQUALS_FRESH,
                    format!(
                        "{name}'s get on the restore gave {on_restore:?}, and on a fresh store {on_fresh:?}"
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// The kernel scenario, one execution.
pub fn run_kernel(
    scenario: &KernelScenario,
    model: &Model,
    bounds: &Bounds,
    chooser: &mut Chooser,
    memo: &mut Memo,
) -> Result<(), Failure> {
    let kernel = Arc::new(kernel());
    let extra = {
        let kernel = Arc::clone(&kernel);
        move |store: &Store, disk: &Disk, ledger: &Ledger, bounds: &Bounds| {
            check(scenario, &kernel, model, store, disk, ledger, bounds)
        }
    };
    let mut run = Run::new(model, bounds, chooser, memo, &extra)?;
    for asker in &scenario.askers {
        let writer = run.store.writer();
        let reader = run.store.reader();
        let ledger = Arc::clone(&run.ledger);
        let kernel = Arc::clone(&kernel);
        let world = scenario.worlds[asker.world];
        let (asker, which, wait) = (*asker, scenario.which, model.wait);
        run.actors.spawn(asker.name, move |turn| {
            asks(turn, asker, which, wait, &kernel, &world, writer, &reader, &ledger)
        })?;
    }
    run.run()
}
