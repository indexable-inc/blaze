//! The ask protocol checked exhaustively: every interleaving of a few askers, one of them
//! perhaps a signal handler that runs on thread 0 while thread 0 is inside an ask, the daemon's
//! reader and one sender, at the grain of the shared-memory accesses and system calls
//! `ask.rs` and `runtime.rs` make.
//!
//! Sequentially consistent. A futex wait is one step that compares the word and goes to sleep;
//! only a wake after it ends the sleep, and there are no timeouts here (the real asker's, which
//! only look for a daemon that hung up, would hide a lost wake). A handler runs between any two
//! steps of its thread's ask, and its thread makes no step until it returns. The reader takes
//! any number of the doorbell's bytes at once, as a read does.
//!
//! Checked over the whole graph of reachable states:
//! - safety: no slot is claimed by two askers, an asker reads the answer to its own ask, and the
//!   daemon answers only an ask that waits for it;
//! - liveness: from every reachable state, one where every asker is done is reachable;
//! - the check has teeth: each mutation below is caught, and with only as many slots as asks
//!   nested on one thread, the handler waits for ever (the limit the module docs name).

use std::collections::HashMap;

const MOST_ASKERS: usize = 3;
const MOST_SLOTS: usize = 2;
const GENERATIONS: u8 = 4;
const FREES: u8 = 8;
const NONE: u8 = u8::MAX;

/// A change to the protocol the model must catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mutation {
    None,
    /// One lock held across a whole ask, as one `Mutex` around the channel was.
    Lock,
    /// A slot claimed by a load and then a store, not one compare-and-swap.
    CheckThenSet,
    /// A sleep that does not compare the word first.
    NoCompare,
}

#[derive(Clone, Copy, Debug)]
struct Config {
    slots: usize,
    /// Threads that ask once each.
    threads: usize,
    /// A handler on thread 0 asks once while thread 0 is inside its ask.
    handler: bool,
    mutation: Mutation,
}

impl Config {
    fn askers(&self) -> usize {
        self.threads + usize::from(self.handler)
    }

    fn handler(&self) -> Option<usize> {
        self.handler.then_some(self.threads)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Phase {
    Free,
    Asked,
    Taken,
    Answering,
    Answered,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Slot {
    phase: Phase,
    generation: u8,
    used: bool,
    /// Ghosts: the asker that claimed it, whose ask is in it, whose ask its answer answers
    /// (asker `i` is `i + 1`; 0 none).
    owner: u8,
    ask: u8,
    answer: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Pc {
    /// Not started; a handler: not run yet.
    Idle,
    /// Taking the one lock (`Mutation::Lock`).
    Locking,
    Claim,
    /// `Mutation::CheckThenSet`: saw this slot free, about to set its bit.
    Set(u8),
    /// Found every slot claimed: about to count itself waiting.
    Full,
    /// Counted itself waiting: about to read the frees.
    Counted,
    /// Read the frees: about to look whether every slot is still claimed.
    Frees(u8),
    /// About to sleep on the frees it read.
    Sleep(u8),
    SleepingForASlot,
    /// About to uncount itself and claim again.
    Leave,
    Fill(u8),
    Ring(u8, u8),
    Check(u8, u8),
    /// Loaded the slot's state as this phase and generation: about to sleep on it.
    Wait(u8, u8, Phase, u8),
    Asleep(u8, u8),
    Read(u8, u8),
    Free(u8, u8),
    Unclaim(u8),
    Count,
    LookForWaiters,
    Wake,
    Unlock,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum ReaderPc {
    Scan(u8),
    Waiting,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum SenderPc {
    Idle,
    Answer(u8, u8),
    Store(u8, u8),
    Wake(u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct State {
    slots: [Slot; MOST_SLOTS],
    frees: u8,
    waiting: u8,
    doorbell: u8,
    lock: u8,
    askers: [Pc; MOST_ASKERS],
    reader: ReaderPc,
    sender: SenderPc,
    queue: [(u8, u8); MOST_ASKERS],
    queued: u8,
}

impl State {
    fn new() -> State {
        let slot = Slot { phase: Phase::Free, generation: 0, used: false, owner: 0, ask: 0, answer: 0 };
        State {
            slots: [slot; MOST_SLOTS],
            frees: 0,
            waiting: 0,
            doorbell: 0,
            lock: NONE,
            askers: [Pc::Idle; MOST_ASKERS],
            reader: ReaderPc::Scan(0),
            sender: SenderPc::Idle,
            queue: [(0, 0); MOST_ASKERS],
            queued: 0,
        }
    }

    fn done(&self, config: &Config) -> bool {
        (0..config.askers()).all(|it| match self.askers[it] {
            Pc::Done => true,
            Pc::Idle => config.handler() == Some(it),
            _ => false,
        })
    }

    fn inside(pc: Pc) -> bool {
        !matches!(pc, Pc::Idle | Pc::Done)
    }
}

/// The asker's step from `pc`, if it can make one.
fn ask_step(config: &Config, state: &State, me: usize) -> Result<Option<State>, String> {
    let mut next = *state;
    let ghost = me as u8 + 1;
    let after_free = if config.mutation == Mutation::Lock { Pc::Unlock } else { Pc::Done };
    let pc = match state.askers[me] {
        Pc::Idle if config.mutation == Mutation::Lock => Pc::Locking,
        Pc::Idle => Pc::Claim,
        Pc::Locking => {
            if state.lock != NONE {
                return Ok(None);
            }
            next.lock = me as u8;
            Pc::Claim
        }
        Pc::Claim => match ((0..config.slots).find(|it| !state.slots[*it].used), config.mutation) {
            (Some(slot), Mutation::CheckThenSet) => Pc::Set(slot as u8),
            (Some(slot), _) => {
                next.slots[slot].used = true;
                next.slots[slot].owner = ghost;
                Pc::Fill(slot as u8)
            }
            (None, _) => Pc::Full,
        },
        Pc::Set(slot) => {
            let at = &mut next.slots[slot as usize];
            if at.owner != 0 {
                return Err(format!("asker {me} claimed slot {slot}, which asker {} holds", at.owner - 1));
            }
            at.used = true;
            at.owner = ghost;
            Pc::Fill(slot)
        }
        Pc::Full => {
            next.waiting += 1;
            Pc::Counted
        }
        Pc::Counted => Pc::Frees(state.frees),
        Pc::Frees(frees) if (0..config.slots).all(|it| state.slots[it].used) => Pc::Sleep(frees),
        Pc::Frees(_) => Pc::Leave,
        Pc::Sleep(frees) if state.frees == frees || config.mutation == Mutation::NoCompare => Pc::SleepingForASlot,
        Pc::Sleep(_) => Pc::Leave,
        Pc::SleepingForASlot | Pc::Asleep(..) | Pc::Done => return Ok(None),
        Pc::Leave => {
            next.waiting -= 1;
            Pc::Claim
        }
        Pc::Fill(slot) => {
            let at = &mut next.slots[slot as usize];
            at.ask = ghost;
            at.phase = Phase::Asked;
            Pc::Ring(slot, at.generation)
        }
        Pc::Ring(slot, generation) => {
            next.doorbell += 1;
            Pc::Check(slot, generation)
        }
        Pc::Check(slot, generation) => {
            let at = state.slots[slot as usize];
            if at.phase == Phase::Answered && at.generation == generation {
                Pc::Read(slot, generation)
            } else {
                Pc::Wait(slot, generation, at.phase, at.generation)
            }
        }
        Pc::Wait(slot, generation, phase, seen) => {
            let at = state.slots[slot as usize];
            let same = at.phase == phase && at.generation == seen;
            if same || config.mutation == Mutation::NoCompare { Pc::Asleep(slot, generation) } else { Pc::Check(slot, generation) }
        }
        Pc::Read(slot, generation) => {
            let at = state.slots[slot as usize];
            if at.answer != ghost || at.generation != generation {
                return Err(format!("asker {me} read slot {slot}'s answer to {} of generation {}", at.answer, at.generation));
            }
            Pc::Free(slot, generation)
        }
        Pc::Free(slot, generation) => {
            let at = &mut next.slots[slot as usize];
            at.phase = Phase::Free;
            at.generation = (generation + 1) % GENERATIONS;
            at.ask = 0;
            at.answer = 0;
            Pc::Unclaim(slot)
        }
        Pc::Unclaim(slot) => {
            let at = &mut next.slots[slot as usize];
            at.used = false;
            at.owner = 0;
            Pc::Count
        }
        Pc::Count => {
            next.frees = (state.frees + 1) % FREES;
            Pc::LookForWaiters
        }
        Pc::LookForWaiters if state.waiting != 0 => Pc::Wake,
        Pc::LookForWaiters => after_free,
        Pc::Wake => {
            for it in &mut next.askers {
                if *it == Pc::SleepingForASlot {
                    *it = Pc::Leave;
                }
            }
            after_free
        }
        Pc::Unlock => {
            next.lock = NONE;
            Pc::Done
        }
    };
    next.askers[me] = pc;
    Ok(Some(next))
}

fn successors(config: &Config, state: &State, out: &mut Vec<State>) -> Result<(), String> {
    let handler = config.handler();
    let interrupted = handler.is_some_and(|it| State::inside(state.askers[it]));
    for me in 0..config.askers() {
        if me == 0 && interrupted {
            continue;
        }
        if Some(me) == handler && state.askers[me] == Pc::Idle && !State::inside(state.askers[0]) {
            continue;
        }
        if let Some(next) = ask_step(config, state, me)? {
            out.push(next);
        }
    }

    match state.reader {
        ReaderPc::Scan(at) if at as usize == config.slots => {
            let mut next = *state;
            next.reader = ReaderPc::Waiting;
            out.push(next);
        }
        ReaderPc::Scan(at) => {
            let mut next = *state;
            let slot = state.slots[at as usize];
            if slot.phase == Phase::Asked {
                next.slots[at as usize].phase = Phase::Taken;
                next.queue[state.queued as usize] = (at, slot.generation);
                next.queued += 1;
                next.reader = ReaderPc::Scan(0);
            } else {
                next.reader = ReaderPc::Scan(at + 1);
            }
            out.push(next);
        }
        ReaderPc::Waiting => {
            for took in 1..=state.doorbell {
                let mut next = *state;
                next.doorbell -= took;
                next.reader = ReaderPc::Scan(0);
                out.push(next);
            }
        }
    }

    let mut next = *state;
    match state.sender {
        SenderPc::Idle if state.queued == 0 => return Ok(()),
        SenderPc::Idle => {
            let (slot, generation) = state.queue[0];
            next.queue.copy_within(1.., 0);
            next.queued -= 1;
            next.sender = SenderPc::Answer(slot, generation);
        }
        SenderPc::Answer(slot, generation) => {
            let at = state.slots[slot as usize];
            if at.phase != Phase::Taken || at.generation != generation {
                return Err(format!("the daemon answered slot {slot} of generation {generation}, which is {at:?}"));
            }
            next.slots[slot as usize].phase = Phase::Answering;
            next.sender = SenderPc::Store(slot, generation);
        }
        SenderPc::Store(slot, _) => {
            let at = &mut next.slots[slot as usize];
            at.answer = at.ask;
            at.phase = Phase::Answered;
            next.sender = SenderPc::Wake(slot);
        }
        SenderPc::Wake(slot) => {
            for it in &mut next.askers {
                if let Pc::Asleep(on, generation) = *it
                    && on == slot
                {
                    *it = Pc::Check(on, generation);
                }
            }
            next.sender = SenderPc::Idle;
        }
    }
    out.push(next);
    Ok(())
}

struct Checked {
    states: usize,
    /// Whether the handler finished an ask in some run.
    handled: bool,
}

fn check(config: Config) -> Result<Checked, String> {
    assert!(config.slots <= MOST_SLOTS && config.askers() <= MOST_ASKERS);
    let first = State::new();
    let mut index: HashMap<State, u32> = HashMap::from([(first, 0)]);
    let mut states = vec![first];
    let mut edges: Vec<Vec<u32>> = Vec::new();
    let mut out = Vec::new();
    let mut at = 0;
    while at < states.len() {
        let state = states[at];
        out.clear();
        successors(&config, &state, &mut out)?;
        let mut here = Vec::with_capacity(out.len());
        for next in &out {
            let id = *index.entry(*next).or_insert_with(|| {
                states.push(*next);
                (states.len() - 1) as u32
            });
            here.push(id);
        }
        edges.push(here);
        at += 1;
    }
    let mut reverse: Vec<Vec<u32>> = vec![Vec::new(); states.len()];
    for (from, tos) in edges.iter().enumerate() {
        for &to in tos {
            reverse[to as usize].push(from as u32);
        }
    }
    let mut good = vec![false; states.len()];
    let mut stack: Vec<u32> = (0..states.len() as u32).filter(|it| states[*it as usize].done(&config)).collect();
    for &it in &stack {
        good[it as usize] = true;
    }
    while let Some(it) = stack.pop() {
        for &from in &reverse[it as usize] {
            if !good[from as usize] {
                good[from as usize] = true;
                stack.push(from);
            }
        }
    }
    if let Some(stuck) = good.iter().position(|it| !it) {
        return Err(format!("from {:?}, no state with every asker done is reachable", states[stuck]));
    }
    let handled = config.handler().is_some_and(|handler| states.iter().any(|it| it.done(&config) && it.askers[handler] == Pc::Done));
    Ok(Checked { states: states.len(), handled })
}

fn config(slots: usize, threads: usize, handler: bool) -> Config {
    Config { slots, threads, handler, mutation: Mutation::None }
}

#[test]
fn asks_are_answered_whatever_the_interleaving() {
    for config in [config(1, 1, false), config(1, 2, false), config(2, 2, false), config(2, 1, true), config(2, 2, true)] {
        let checked = check(config).unwrap_or_else(|why| panic!("{config:?}: {why}"));
        assert!(checked.states > 10, "{config:?}: {} states", checked.states);
        assert_eq!(checked.handled, config.handler, "{config:?}");
    }
}

#[test]
fn a_handler_waits_for_ever_only_when_its_thread_holds_the_last_slot() {
    let why = check(config(1, 1, true)).err().expect("one slot, held by the thread the handler interrupted");
    assert!(why.contains("no state with every asker done"), "{why}");
}

#[test]
fn each_mutation_is_caught() {
    let mutated = |mutation: Mutation, slots: usize, threads: usize, handler: bool| {
        check(Config { slots, threads, handler, mutation }).err().unwrap_or_else(|| panic!("{mutation:?} was not caught"))
    };
    assert!(mutated(Mutation::Lock, 2, 1, true).contains("no state with every asker done"));
    assert!(mutated(Mutation::CheckThenSet, 1, 2, false).contains("which asker"));
    assert!(mutated(Mutation::NoCompare, 1, 1, false).contains("no state with every asker done"));
    assert!(mutated(Mutation::NoCompare, 1, 2, false).contains("no state with every asker done"));
}
