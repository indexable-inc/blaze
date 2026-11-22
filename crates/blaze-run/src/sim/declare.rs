//! [`Parent::declare`] and the run wire's need by symbol (run.md, piece 4) under every order of
//! events the tape draws: two program threads, three code records (A, B, C) that each name one
//! new callee X by symbol, and a symbol U that names nothing.
//!
//! The driver is the simulation. Every message crosses as real bytes of the run wire
//! (`blaze_launch::wire::run`): the parent's effects become `Down` frames on one stream, the
//! program's needs `Up` frames on another, each stream in order, each delivered when the tape
//! says; a cut of each delivered item is refused. The store's lookups and the compiles end in
//! any order. Each thread calls a caller (by symbol when the program has no slot for it, else
//! through its slot), then X through the slot the caller's placed record names; some first call
//! X itself by symbol, some ask for U.
//!
//! The oracle: an instance has one slot in the parent ("declared twice"); a record reaches the
//! program only after every one of its targets' (symbol, slot) pairs ("published before its
//! targets were told"); a symbol is told one slot, and a slot one symbol; a published slot runs
//! its own symbol's record; only U fails, as unresolved; at the end no thread waits, both threads
//! called X through one slot into X's record, and the slots told are exactly `0..declared`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use blaze_launch::wire::run::{Declared, Down, Failure, Needed, Symbol as WireSymbol, Up};
use blaze_launch::wire::{List, RecordId, split};
use blaze_launch::{Slot, Thread, Tier as WireTier};
use blaze_own::{AVec, Alloc, Vec as Own};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use super::{harness, key, oracle};
use crate::parent::{Answer, CompiledNamed, Config, Effect, Function, Named, Parent, Resolve, Symbol};
use crate::{CodeId, CodeRef, InstanceKey, Nanos, Plant, SlotId, ThreadId, Tier};

const A: u64 = 0;
const B: u64 = 1;
const C: u64 = 2;
/// The new callee every caller's record names.
const X: u64 = 3;
/// A symbol the manifest does not resolve.
const U: u64 = 9;
const THREADS: usize = 2;
const MOST_SLOTS: u32 = 8;
const CONFIG: Config = Config { workers: 2, compile_bound: Nanos(1 << 40), most_waiters: 8 };

fn symbol(n: u64) -> Symbol {
    Symbol(key(50 + n))
}

/// The number of a 32-byte key the simulation made with [`key`].
fn number(bytes: &[u8; 32]) -> u64 {
    let mut head = [0; 8];
    head.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(head)
}

fn symbol_number(bytes: &[u8; 32]) -> u64 {
    number(bytes).wrapping_sub(50)
}

fn function(n: u64) -> Function {
    Function { instance: InstanceKey(key(n)), code: CodeId(key(100 + n)) }
}

/// Symbol `n`'s code record: one each, so a slot running another's is caught.
fn record(n: u64) -> CodeRef {
    CodeRef(key(200 + n))
}

/// The function targets of `n`'s record, in its order.
fn targets(n: u64) -> Vec<Symbol> {
    match n {
        A | B => vec![symbol(X)],
        C => vec![symbol(X), symbol(A)],
        _ => Vec::new(),
    }
}

/// The program's manifest: A, B, C and X; nothing else.
struct Manifest;

impl Resolve for Manifest {
    fn resolve(&self, symbol: Symbol) -> Option<Function> {
        let n = symbol_number(&symbol.0);
        (n <= X).then(|| function(n))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Wait {
    Running,
    Slot(SlotId),
    Symbol(u64),
    Done,
}

/// One call a thread makes: of symbol `n`, directly, or `via` a caller's placed record.
#[derive(Clone, Copy, Debug)]
struct Call {
    n: u64,
    via: Option<u64>,
}

struct Program {
    calls: VecDeque<Call>,
    wait: Wait,
}

/// What a delivered down message said, owned before its frame leaves the stream.
enum Got {
    Declare(Vec<Declared>),
    Publish(SlotId, CodeRef),
    Failed(Needed, bool),
}

struct Sim {
    parent: Parent<'static>,
    out: AVec<'static, Effect>,
    /// The program's frames to the parent, and the parent's to the program, in order.
    up: Vec<u8>,
    down: Vec<u8>,
    ids: u64,
    lookups: Vec<(SlotId, u64)>,
    compiles: Vec<(SlotId, u64)>,
    /// The program's side: each symbol's slot as told, each slot's symbol, the bound words.
    slots: BTreeMap<u64, SlotId>,
    symbols: BTreeMap<SlotId, u64>,
    words: BTreeMap<SlotId, CodeRef>,
    threads: Vec<Program>,
    /// Each call of X that ran: the thread, the slot it went through, the code it reached.
    x_calls: Vec<(usize, SlotId, CodeRef)>,
}

impl Sim {
    /// After each call of the parent: one slot an instance, then its effects carried.
    fn after(&mut self) -> Result<(), Broke> {
        let mut seen = BTreeMap::new();
        for at in 0..self.parent.declared() {
            let function = self.parent.function(SlotId(at)).ok_or_else(|| harness("a declared slot has no function"))?;
            if let Some(first) = seen.insert(function.instance, at) {
                return oracle(false, || {
                    format!("instance {} was declared twice: slots {first} and {at}", number(&function.instance.0))
                });
            }
        }
        let effects: Vec<Effect> = self.out.drain(..).collect();
        for effect in effects {
            match effect {
                Effect::Lookup { slot, key } => self.lookups.push((slot, number(&key.function.0))),
                Effect::Compile { slot, key, .. } => self.compiles.push((slot, number(&key.function.0))),
                Effect::Publish { slot, tier, code } => {
                    let tier = match tier {
                        Tier::Fast => WireTier::Zero,
                        Tier::Optimized => WireTier::Optimized,
                    };
                    self.send(Down::Publish { slot: Slot(slot.0), tier, record: RecordId(code.0) })?;
                }
                Effect::Declare { symbol, slot } => {
                    let pairs = [Declared { symbol: WireSymbol(symbol.0), slot: Slot(slot.0) }];
                    self.send(Down::Declare { pairs: List::of(&pairs) })?;
                }
                Effect::Unresolved { symbol, .. } => {
                    self.send(Down::Failed { need: Needed::Symbol(WireSymbol(symbol.0)), why: Failure::Unresolved })?;
                }
                // The publish before it carried the code; the program wakes its waiters itself.
                Effect::Answer { answer: Answer::Code { .. }, .. } => {}
                Effect::Answer { slot, answer: Answer::Failed(error), .. } => {
                    return oracle(false, || format!("slot {} failed, and every function here compiles: {error}", slot.0));
                }
                Effect::Begin | Effect::Say(_) | Effect::WriteImage => {}
                Effect::Make { .. } | Effect::Hooks { .. } | Effect::NoStart(_) | Effect::Stop(_) => {
                    return Err(harness(format!("{effect:?}: nothing here lacks a record, is hooked or fails")));
                }
            }
        }
        Ok(())
    }

    fn send(&mut self, down: Down<'_>) -> Result<(), Broke> {
        self.ids += 1;
        down.encode(self.ids, &mut self.down).map_err(harness)
    }

    fn ask(&mut self, up: Up) -> Result<(), Broke> {
        self.ids += 1;
        up.encode(self.ids, &mut self.up).map_err(harness)
    }

    /// Thread `at` makes its next call.
    fn step(&mut self, at: usize) -> Result<(), Broke> {
        let Some(&call) = self.threads[at].calls.front() else {
            self.threads[at].wait = Wait::Done;
            return Ok(());
        };
        let thread = Thread(at as u64);
        let slot = match (self.slots.get(&call.n).copied(), call.via) {
            (Some(slot), _) => slot,
            (None, Some(caller)) => {
                return oracle(false, || {
                    format!("thread {at} ran symbol {caller}'s record, which calls symbol {} with no slot", call.n)
                });
            }
            (None, None) => {
                self.threads[at].wait = Wait::Symbol(call.n);
                return self.ask(Up::Need { need: Needed::Symbol(WireSymbol(symbol(call.n).0)), thread });
            }
        };
        match self.words.get(&slot) {
            Some(&code) => self.finish(at, slot, code),
            None => {
                self.threads[at].wait = Wait::Slot(slot);
                self.ask(Up::Need { need: Needed::Slot(Slot(slot.0)), thread })
            }
        }
    }

    /// Thread `at`'s call through `slot` reached `code`.
    fn finish(&mut self, at: usize, slot: SlotId, code: CodeRef) -> Result<(), Broke> {
        let call = self.threads[at].calls.pop_front().ok_or_else(|| harness("a call finished that was not made"))?;
        if call.n == X {
            self.x_calls.push((at, slot, code));
        }
        self.threads[at].wait = Wait::Running;
        Ok(())
    }

    /// The parent reads the program's next frame, if one is whole.
    fn deliver_up(&mut self, tape: &Tape) -> Result<bool, Broke> {
        let Some((item, took)) = split(&self.up).map_err(harness)? else { return Ok(false) };
        let (_, up) = Up::decode(item).map_err(harness)?;
        let cut = tape.index(item.len(), "cut");
        oracle(Up::decode(&item[..cut]).is_err(), || format!("an up frame cut to {cut} bytes was read"))?;
        drop(self.up.drain(..took));
        // The modelled program never crashes: a fault frame here is the harness's own error.
        let Up::Need { need, thread } = up else { return Err(harness("the modelled program sent a fault report (Up::Fault)")) };
        let thread = ThreadId(thread.0);
        let now = Nanos(0);
        match need {
            Needed::Slot(slot) => self.parent.need(thread, SlotId(slot.0), now, &mut self.out),
            Needed::Symbol(asked) => self.parent.need_symbol(thread, Symbol(asked.0), &Manifest, now, &mut self.out),
        }
        .map_err(harness)?;
        self.after()?;
        Ok(true)
    }

    /// The program reads the parent's next frame, if one is whole.
    fn deliver_down(&mut self, tape: &Tape) -> Result<bool, Broke> {
        let Some((item, took)) = split(&self.down).map_err(harness)? else { return Ok(false) };
        let (_, down) = Down::decode(item).map_err(harness)?;
        let cut = tape.index(item.len(), "cut");
        oracle(Down::decode(&item[..cut]).is_err(), || format!("a down frame cut to {cut} bytes was read"))?;
        let got = match down {
            Down::Declare { pairs } => Got::Declare(pairs.iter().collect()),
            Down::Publish { slot, record, .. } => Got::Publish(SlotId(slot.0), CodeRef(record.0)),
            Down::Positioned { slot, .. } => {
                return Err(Broke::Oracle(format!("the parent sent source positions for slot {}: this model's records have no position rows", slot.0)));
            }
            Down::Failed { need, why } => Got::Failed(need, why == Failure::Unresolved),
            Down::Personality { slot } => {
                return Err(Broke::Oracle(format!(
                    "the parent sent `Personality` naming slot {}: this simulation's parent never binds a personality routine",
                    slot.0
                )));
            }
            Down::Native { words, .. } => {
                return Err(Broke::Oracle(format!(
                    "the parent sent `Native` with {} words: this simulation's parent has no native library",
                    words.len()
                )));
            }
            Down::Hooks { slot, kind } => {
                return Err(Broke::Oracle(format!(
                    "the parent sent `Hooks` ({kind:?}) for slot {}: this simulation's parent toggles no hooks",
                    slot.0
                )));
            }
            Down::Promote { slot, .. } => {
                return Err(Broke::Oracle(format!(
                    "the parent sent `Promote` for slot {}: this simulation's parent does not promote",
                    slot.0
                )));
            }
            Down::Bytes { part, .. } => {
                return Err(Broke::Oracle(format!(
                    "the parent sent `Bytes` ({} bytes of a record): this simulation's records are named by reference only",
                    part.len()
                )));
            }
            Down::Data { data } => {
                return Err(Broke::Oracle(format!("the parent sent `Data` with {} statics: this simulation's manifest has no static", data.len())));
            }
            Down::ThreadLocals { .. } => {
                return Err(Broke::Oracle(String::from("the parent sent `ThreadLocals`: this simulation's manifest has no thread-local")));
            }
        };
        drop(self.down.drain(..took));
        match got {
            Got::Declare(pairs) => {
                for pair in pairs {
                    let (n, slot) = (symbol_number(&pair.symbol.0), SlotId(pair.slot.0));
                    if let Some(&was) = self.slots.get(&n) {
                        oracle(was == slot, || format!("symbol {n} was told slot {} after slot {}", slot.0, was.0))?;
                        continue;
                    }
                    oracle(!self.symbols.contains_key(&slot), || format!("slot {} was told for two symbols", slot.0))?;
                    self.slots.insert(n, slot);
                    self.symbols.insert(slot, n);
                    for thread in &mut self.threads {
                        if thread.wait == Wait::Symbol(n) {
                            thread.wait = Wait::Slot(slot);
                        }
                    }
                }
            }
            Got::Publish(slot, code) => {
                let Some(&n) = self.symbols.get(&slot) else {
                    return Err(Broke::Oracle(format!("slot {} was published before it was told", slot.0)));
                };
                oracle(code == record(n), || format!("slot {} (symbol {n}) was given another symbol's record", slot.0))?;
                for target in targets(n) {
                    let target = symbol_number(&target.0);
                    oracle(self.slots.contains_key(&target), || {
                        format!("symbol {n}'s record was published before its targets were told: symbol {target} has no slot")
                    })?;
                }
                if let Some(was) = self.words.insert(slot, code) {
                    oracle(was == code, || format!("slot {} was published with two records", slot.0))?;
                }
                for at in 0..THREADS {
                    if self.threads[at].wait == Wait::Slot(slot) {
                        self.finish(at, slot, code)?;
                    }
                }
            }
            Got::Failed(Needed::Symbol(asked), unresolved) => {
                let n = symbol_number(&asked.0);
                oracle(n == U && unresolved, || format!("symbol {n} failed (unresolved: {unresolved}), and it resolves"))?;
                for at in 0..THREADS {
                    if self.threads[at].wait == Wait::Symbol(n) {
                        // The call fails on this thread (rule 20); the thread goes on.
                        self.threads[at].calls.pop_front();
                        self.threads[at].wait = Wait::Running;
                    }
                }
            }
            Got::Failed(Needed::Slot(slot), _) => {
                return Err(Broke::Oracle(format!("slot {} failed, and every function here compiles", slot.0)));
            }
        }
        Ok(true)
    }

    /// The store answers one lookup, in any order: the record, or a miss to compile.
    fn look(&mut self, tape: &Tape) -> Result<bool, Broke> {
        if self.lookups.is_empty() {
            return Ok(false);
        }
        let (slot, n) = self.lookups.remove(tape.index(self.lookups.len(), "lookup"));
        let named = targets(n);
        let found = tape.coin("in the store").then(|| Named { fast: record(n), targets: &named, optimized: None });
        self.parent.looked_named(slot, found, &Manifest, &mut self.out).map_err(harness)?;
        self.after()?;
        Ok(true)
    }

    /// One compile ends, in any order, with its record naming its targets by symbol.
    fn compile(&mut self, tape: &Tape) -> Result<bool, Broke> {
        if self.compiles.is_empty() {
            return Ok(false);
        }
        let (slot, n) = self.compiles.remove(tape.index(self.compiles.len(), "compile"));
        let named = targets(n);
        let ended = CompiledNamed::Record { code: record(n), targets: &named };
        self.parent.compiled_named(slot, ended, Nanos(0), &Manifest, &mut self.out).map_err(harness)?;
        self.after()?;
        Ok(true)
    }
}

/// A thread's calls: maybe U, maybe X by symbol first, then one to three callers, each followed
/// by X through the slot its record names.
fn script(tape: &Tape) -> VecDeque<Call> {
    let mut calls = VecDeque::new();
    if tape.coin("asks for U") {
        calls.push_back(Call { n: U, via: None });
    }
    if tape.coin("X by symbol first") {
        calls.push_back(Call { n: X, via: None });
    }
    for _ in 0..tape.range(1..4, "callers") {
        let caller = tape.below(3, "caller");
        calls.push_back(Call { n: caller, via: None });
        calls.push_back(Call { n: X, via: Some(caller) });
    }
    calls
}

fn run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut parent = Parent::growing(&[], CONFIG, MOST_SLOTS, Alloc::global()).map_err(harness)?;
    parent.plant = plant;
    let mut sim = Sim {
        parent,
        out: Own::new_in(Alloc::global()),
        up: Vec::new(),
        down: Vec::new(),
        ids: 0,
        lookups: Vec::new(),
        compiles: Vec::new(),
        slots: BTreeMap::new(),
        symbols: BTreeMap::new(),
        words: BTreeMap::new(),
        threads: (0..THREADS).map(|_| Program { calls: script(tape), wait: Wait::Running }).collect(),
        x_calls: Vec::new(),
    };
    sim.parent.start(&[], &[], Nanos(0), &mut sim.out).map_err(harness)?;
    sim.after()?;
    for _ in 0..tape.range(1..200, "steps") {
        match tape.below(5, "event") {
            0 => {
                let at = tape.index(THREADS, "thread");
                if sim.threads[at].wait == Wait::Running {
                    sim.step(at)?;
                }
            }
            1 => {
                sim.deliver_up(tape)?;
            }
            2 => {
                sim.deliver_down(tape)?;
            }
            3 => {
                sim.look(tape)?;
            }
            _ => {
                sim.compile(tape)?;
            }
        }
    }
    // Everything is carried and every thread runs to its end.
    loop {
        let mut moved = false;
        for at in 0..THREADS {
            if sim.threads[at].wait == Wait::Running {
                sim.step(at)?;
                moved = true;
            }
        }
        moved |= sim.deliver_up(tape)?;
        moved |= sim.deliver_down(tape)?;
        moved |= sim.look(tape)?;
        moved |= sim.compile(tape)?;
        if !moved {
            break;
        }
    }
    for (at, thread) in sim.threads.iter().enumerate() {
        oracle(thread.wait == Wait::Done, || format!("thread {at} was left waiting: {:?}", thread.wait))?;
    }
    let reached: BTreeSet<(SlotId, CodeRef)> = sim.x_calls.iter().map(|(_, slot, code)| (*slot, *code)).collect();
    oracle(reached.len() == 1, || format!("the callee was called through more than one slot or code: {reached:?}"))?;
    oracle(reached.iter().all(|(_, code)| *code == record(X)), || format!("the callee's calls reached {reached:?}"))?;
    let callers: BTreeSet<usize> = sim.x_calls.iter().map(|(at, ..)| *at).collect();
    oracle(callers.len() == THREADS, || format!("only threads {callers:?} called the callee"))?;
    let told: Vec<u32> = sim.symbols.keys().map(|slot| slot.0).collect();
    let declared: Vec<u32> = (0..sim.parent.declared()).collect();
    oracle(told == declared, || format!("the slots told, {told:?}, are not the slots declared, {declared:?}"))
}

/// Expected: passes with no output.
#[test]
fn a_new_callee_gets_one_slot_whichever_record_or_thread_names_it_first() {
    check(SimConfig::cases(1000), |tape| run(tape, Plant::None));
}

/// The piece's test, by hand: A's record names X, so X is declared and told before A is
/// published; B's record and a thread asking for X by symbol find the same slot; C's record
/// (X and A) declares nothing new. Expected: passes with no output.
#[test]
fn a_published_records_new_callee_gets_a_slot_once_whichever_asks_first() {
    let mut parent = Parent::growing(&[], CONFIG, MOST_SLOTS, Alloc::global()).expect("a parent");
    let mut out = Own::new_in(Alloc::global());
    parent.start(&[], &[], Nanos(0), &mut out).expect("the start");
    out.clear();

    parent.need_symbol(ThreadId(1), symbol(A), &Manifest, Nanos(0), &mut out).expect("A by symbol");
    assert_eq!(out.first(), Some(&Effect::Declare { symbol: symbol(A), slot: SlotId(0) }), "{out:?}");
    out.clear();
    parent.looked_named(SlotId(0), None, &Manifest, &mut out).expect("A is not in the store");
    out.clear();
    let a = targets(A);
    parent
        .compiled_named(SlotId(0), CompiledNamed::Record { code: record(A), targets: &a }, Nanos(1), &Manifest, &mut out)
        .expect("A compiled");
    let tell = out.iter().position(|it| *it == Effect::Declare { symbol: symbol(X), slot: SlotId(1) });
    let publish = out.iter().position(|it| matches!(it, Effect::Publish { slot: SlotId(0), .. }));
    assert!(matches!((tell, publish), (Some(tell), Some(publish)) if tell < publish), "{out:?}");
    out.clear();

    // A second thread asks for X by symbol: X's slot, told already, so nothing is told again.
    parent.need_symbol(ThreadId(2), symbol(X), &Manifest, Nanos(2), &mut out).expect("X by symbol");
    assert!(!out.iter().any(|it| matches!(it, Effect::Declare { .. })), "{out:?}");
    out.clear();

    // B's record names X too: B is declared (slot 2), X keeps slot 1.
    parent.need_symbol(ThreadId(1), symbol(B), &Manifest, Nanos(3), &mut out).expect("B by symbol");
    assert_eq!(out.first(), Some(&Effect::Declare { symbol: symbol(B), slot: SlotId(2) }), "{out:?}");
    out.clear();
    let b = targets(B);
    parent.looked_named(SlotId(2), Some(Named { fast: record(B), targets: &b, optimized: None }), &Manifest, &mut out).expect("B found");
    assert!(!out.iter().any(|it| matches!(it, Effect::Declare { .. })), "{out:?}");
    out.clear();

    // C names X and A: both declared already.
    assert_eq!(parent.declare(function(C)), Ok(SlotId(3)));
    assert_eq!(parent.declare(function(C)), Ok(SlotId(3)), "declare is idempotent per instance");
    assert_eq!(parent.declare(function(X)), Ok(SlotId(1)));
    assert_eq!(parent.declared(), 4);

    // An instance declared with another code identity, and a symbol that names nothing.
    let other = Function { instance: function(X).instance, code: CodeId(key(999)) };
    assert_eq!(parent.declare(other), Err(crate::parent::Error::TwoCodes { instance: other.instance, slot: SlotId(1) }));
    parent.need_symbol(ThreadId(3), symbol(U), &Manifest, Nanos(4), &mut out).expect("U by symbol");
    assert_eq!(out.as_slice(), &[Effect::Unresolved { thread: ThreadId(3), symbol: symbol(U) }]);
    assert_eq!(parent.declared(), 4, "an unresolved symbol declares nothing");
}

#[test]
fn a_second_slot_for_one_instance_is_caught() {
    control(SimConfig::cases(4000), Plant::None, Plant::DeclareTwice, "declared twice", run);
}

#[test]
fn a_record_published_before_its_targets_are_told_is_caught() {
    control(SimConfig::cases(4000), Plant::None, Plant::PublishBeforeTell, "published before its targets were told", run);
}
