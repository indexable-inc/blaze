//! The core against threads that never stop: a deterministic simulation of one process.
//!
//! The machine here is the driver's part made explicit: the words of the table, the gates'
//! flags and entries, the published epoch, the queue of writes the control thread has yet to
//! carry out, and program threads that call through words, sit in gates and stubs, report at
//! safepoints, get scanned from outside, start threads and end. A schedule ([`Op`]) says who
//! takes the next step, so every interleaving of the control thread's stores with the
//! threads' loads is a list of ops: a case's tape draws lists, and each control below writes
//! one out.
//!
//! The oracle ([`Sim::check`], [`Sim::land`], [`Sim::apply`], [`Sim::settle`]) states what
//! launcher.md promises, from the machine's side only (it never asks the core what it
//! believes):
//!
//! - `freed:` no thread has a frame in, or enters, code that was freed;
//! - `word:` a word never leads a thread to code that was not written, or that no commit
//!   bound;
//! - `together:` no thread runs one function as of a version of the program after it ran
//!   another as of a later one;
//! - `abort:` an aborted edit changed no word;
//! - `reused:` no offset of the area is handed out twice;
//! - `decommit:` no page is given back while something live is in it;
//! - `leak:` once every thread has reported an empty stack, everything unlinked is freed.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use blaze_sim::Tape;

use crate::launch::Plant;
use crate::{
    Bind, CodeId, Config, Edit, Frame, FrameKind, KEPT, Kept, Launch, Range, Slot, Target, Thread,
    Tier, Txn, Write,
};

const SLOTS: u32 = 6;
const MOST_THREADS: usize = 4;
const DEEPEST: usize = 6;

/// Small pages, so a run frees whole ones.
const CONFIG: Config =
    Config { slots: SLOTS, area: 1 << 20, page: 256, gate_head: 16, gate_entry: 16, gate_align: 16 };

/// One step of the schedule. A step that does not apply where the machine is does nothing,
/// so any list is a run.
#[derive(Clone, Copy, Debug)]
enum Op {
    /// A thread loads a slot's word, to call through it.
    Call(usize, u32),
    /// A thread that loaded a word jumps; one in a gate reads the flag, then jumps; one in a
    /// stub looks whether its slot was bound and calls through the word again.
    Step(usize),
    Return(usize),
    /// A thread reports itself at a safepoint; whether its walk reached the stack's end.
    Safepoint(usize, bool),
    /// A thread loads the epoch, announces a birth and starts a thread.
    Spawn(usize),
    End(usize),
    /// The control thread stages an edit of `1 + count % 3` slots from `first`.
    Edit { first: u32, count: u32, together: bool },
    /// It gives up the staged edit, as when a record's hash differs.
    Abort,
    Commit,
    /// It carries out its next write.
    Write,
    /// It carries out every write it has.
    Flush,
    /// It takes the next message a thread left it.
    Deliver,
    /// It stops a thread, walks its stack and reports it.
    Scan(usize),
    Collect,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum At {
    Running,
    /// Between the load of a word and the jump: the target is in a register.
    Loaded { slot: Slot, to: Target },
    Gate { gate: Range, entry: u32, decided: Option<bool> },
    /// In the resolver, waiting for the slot's code.
    Stub(Slot),
}

struct Actor {
    id: Thread,
    alive: bool,
    /// The bodies it is in, outermost first.
    stack: Vec<Range>,
    at: At,
    /// The latest version of the program it ran a function of.
    low: u64,
}

impl Actor {
    /// The frames a walk of its stack finds. From outside (the thread stopped) the walk also
    /// has its program counter and the register a call in flight holds its target in.
    fn frames(&self, outside: bool) -> Vec<Frame> {
        let mut frames = Vec::new();
        let running = outside && self.at == At::Running;
        for (index, body) in self.stack.iter().enumerate() {
            // Both ends of each rule: a return address at the end or just past the start, a
            // program counter at the start or the last byte.
            let even = (body.at / 16) % 2 == 0;
            let top = index + 1 == self.stack.len();
            frames.push(if top && running {
                Frame { at: if even { body.at } else { body.end() - 1 }, kind: FrameKind::Pc }
            } else {
                Frame { at: if even { body.end() } else { body.at + 1 }, kind: FrameKind::Return }
            });
        }
        match self.at {
            At::Gate { gate, .. } | At::Loaded { to: Target::Gate { gate, .. }, .. } => {
                frames.push(Frame { at: gate.at + 8, kind: FrameKind::Pc });
            }
            At::Loaded { to: Target::Body(body), .. } => {
                frames.push(Frame { at: body.at, kind: FrameKind::Pc });
            }
            At::Running | At::Stub(_) | At::Loaded { to: Target::Stub, .. } => {}
        }
        frames
    }
}

enum Msg {
    Report { thread: Thread, epoch: u64, frames: Vec<Frame>, complete: bool },
    Ended(Thread),
}

struct StagedEdit {
    txn: Txn,
    binds: Vec<Bind>,
    /// By bind: the range the core handed out.
    ranges: Vec<Range>,
    together: bool,
    /// The table when the edit was staged.
    words: Vec<Target>,
}

struct Sim {
    launch: Launch,
    out: Vec<Write, Kept>,
    // The process's memory, as the driver wrote it.
    words: Vec<Target>,
    epoch: u64,
    flags: BTreeMap<u64, bool>,
    entries: BTreeMap<(u64, u32), (Slot, Target, Range)>,
    /// Bodies placed and gates written, by offset.
    written: BTreeSet<u64>,
    // What the control thread has yet to do or to hear.
    pending: VecDeque<Write>,
    births: Vec<(Thread, u64)>,
    inbox: VecDeque<Msg>,
    staged: Option<StagedEdit>,
    next_txn: u64,
    actors: Vec<Actor>,
    // The oracle's own account.
    /// Handed out and not freed, by offset.
    live: BTreeMap<u64, Range>,
    freed: Vec<Range>,
    /// The end of the last range handed out.
    handed: u64,
    /// By a body's offset: its slot and the version of the program that bound it.
    versions: BTreeMap<u64, (Slot, u64)>,
    /// By slot: the versions that bound it, in order.
    history: Vec<Vec<u64>>,
    next_version: u64,
}

impl Sim {
    fn new(plant: Plant) -> Result<Sim, String> {
        let alloc = KEPT;
        let mut launch = Launch::new(CONFIG, alloc).map_err(|it| format!("new: {it}"))?;
        launch.plant = plant;
        let mut sim = Sim {
            launch,
            out: Vec::new_in(alloc),
            words: Vec::new(),
            epoch: 0,
            flags: BTreeMap::new(),
            entries: BTreeMap::new(),
            written: BTreeSet::new(),
            pending: VecDeque::new(),
            births: Vec::new(),
            inbox: VecDeque::new(),
            staged: None,
            next_txn: 1,
            actors: vec![Actor {
                id: Thread(0),
                alive: true,
                stack: Vec::new(),
                at: At::Running,
                low: 0,
            }],
            live: BTreeMap::new(),
            freed: Vec::new(),
            handed: 0,
            versions: BTreeMap::new(),
            history: vec![Vec::new(); SLOTS as usize],
            next_version: 1,
        };
        sim.launch.declare(SLOTS, &mut sim.out).map_err(|it| format!("declare: {it}"))?;
        sim.queue();
        sim.flush()?;
        sim.launch.thread_began(Thread(0), 0).map_err(|it| format!("birth: {it}"))?;
        Ok(sim)
    }

    /// Moves what the core just handed out onto the control thread's queue.
    fn queue(&mut self) {
        self.pending.extend(self.out.drain(..));
    }

    fn flush(&mut self) -> Result<(), String> {
        while let Some(write) = self.pending.pop_front() {
            self.apply(write)?;
        }
        Ok(())
    }

    /// A range the core handed out for code.
    fn hand(&mut self, range: Range) -> Result<(), String> {
        if range.at < self.handed {
            return Err(format!(
                "reused: {range:?} was handed out though the area was handed out to {}",
                self.handed
            ));
        }
        self.handed = range.end();
        self.live.insert(range.at, range);
        self.written.insert(range.at);
        Ok(())
    }

    /// The control thread carries out one write.
    fn apply(&mut self, write: Write) -> Result<(), String> {
        match write {
            Write::Place { range, .. } => self.hand(range)?,
            Write::Gate { range, .. } => {
                self.hand(range)?;
                self.flags.insert(range.at, false);
            }
            Write::GateEntry { gate, entry, old, new } => {
                let staged = self.staged.as_ref().ok_or("word: a gate entry with no staged edit")?;
                let slot = staged.binds[entry as usize].slot;
                self.entries.insert((gate.at, entry), (slot, old, new));
            }
            Write::Slot { slot, to } => {
                let index = slot.0 as usize;
                match index == self.words.len() {
                    true => self.words.push(to),
                    false => self.words[index] = to,
                }
            }
            Write::Commit { gate } => {
                self.flags.insert(gate.at, true);
            }
            Write::Epoch { epoch } => {
                if epoch <= self.epoch {
                    return Err(format!("epoch: {epoch} published after {}", self.epoch));
                }
                self.epoch = epoch;
            }
            Write::Forget { range } => {
                if self.live.remove(&range.at) != Some(range) {
                    return Err(format!("freed: {range:?} was freed twice or never handed out"));
                }
                self.freed.push(range);
            }
            Write::Decommit { range } => {
                if range.at % CONFIG.page != 0 || range.len % CONFIG.page != 0 || range.len == 0 {
                    return Err(format!("decommit: {range:?} is not whole pages"));
                }
                if let Some(live) =
                    self.live.values().find(|it| it.at < range.end() && range.at < it.end())
                {
                    return Err(format!("decommit: {range:?} given back with {live:?} live in it"));
                }
            }
        }
        Ok(())
    }

    /// A thread arrives in a body.
    fn land(&mut self, thread: usize, range: Range) -> Result<(), String> {
        if !self.written.contains(&range.at) {
            return Err(format!("word: thread {thread} entered {range:?}, which was not written"));
        }
        if self.freed.contains(&range) {
            return Err(format!("freed: thread {thread} entered {range:?}, which was freed"));
        }
        let Some(&(slot, version)) = self.versions.get(&range.at) else {
            return Err(format!("word: thread {thread} entered {range:?}, which no commit bound"));
        };
        let next = self.history[slot.0 as usize]
            .iter()
            .copied()
            .find(|it| *it > version)
            .unwrap_or(u64::MAX);
        let actor = &mut self.actors[thread];
        if next <= actor.low {
            return Err(format!(
                "together: thread {thread} ran slot {} as of version {version}, which version \
                 {next} replaced, after it ran a function of version {}",
                slot.0, actor.low
            ));
        }
        actor.low = actor.low.max(version);
        actor.at = At::Running;
        actor.stack.push(range);
        Ok(())
    }

    fn drain_births(&mut self) -> Result<(), String> {
        for (thread, epoch) in std::mem::take(&mut self.births) {
            self.launch.thread_began(thread, epoch).map_err(|it| format!("birth: {it}"))?;
        }
        Ok(())
    }

    fn step(&mut self, op: Op) -> Result<(), String> {
        match op {
            Op::Call(thread, slot) => {
                let Some(actor) = self.actors.get_mut(thread) else { return Ok(()) };
                if actor.alive && actor.at == At::Running && actor.stack.len() < DEEPEST {
                    let slot = Slot(slot % SLOTS);
                    actor.at = At::Loaded { slot, to: self.words[slot.0 as usize] };
                }
            }
            Op::Step(thread) => {
                let Some(actor) = self.actors.get_mut(thread) else { return Ok(()) };
                match actor.at {
                    At::Running => {}
                    At::Loaded { to: Target::Body(body), .. } => self.land(thread, body)?,
                    At::Loaded { to: Target::Gate { gate, entry }, .. } => {
                        if !self.written.contains(&gate.at) {
                            return Err(format!(
                                "word: thread {thread} entered a gate at {}, which was not written",
                                gate.at
                            ));
                        }
                        actor.at = At::Gate { gate, entry, decided: None };
                    }
                    At::Loaded { slot, to: Target::Stub } => actor.at = At::Stub(slot),
                    At::Gate { gate, entry, decided: None } => {
                        let flag = self.flags.get(&gate.at).copied();
                        actor.at = At::Gate { gate, entry, decided: Some(flag == Some(true)) };
                    }
                    At::Gate { gate, entry, decided: Some(new) } => {
                        let Some(&(slot, old, body)) = self.entries.get(&(gate.at, entry)) else {
                            return Err(format!(
                                "word: thread {thread} is in entry {entry} of the gate at {}, \
                                 which was not written",
                                gate.at
                            ));
                        };
                        let to = if new { Target::Body(body) } else { old };
                        match to {
                            Target::Body(body) => self.land(thread, body)?,
                            Target::Stub => actor.at = At::Stub(slot),
                            Target::Gate { .. } => {
                                return Err(format!(
                                    "word: the gate at {} leads to a gate",
                                    gate.at
                                ));
                            }
                        }
                    }
                    At::Stub(slot) => {
                        // The resolver never jumps to an address it was handed: once the
                        // slot is bound it calls through the word again.
                        let to = self.words[slot.0 as usize];
                        if to != Target::Stub {
                            actor.at = At::Loaded { slot, to };
                        }
                    }
                }
            }
            Op::Return(thread) => {
                let Some(actor) = self.actors.get_mut(thread) else { return Ok(()) };
                if actor.at == At::Running {
                    actor.stack.pop();
                }
            }
            Op::Safepoint(thread, complete) => {
                let Some(actor) = self.actors.get(thread) else { return Ok(()) };
                // A safepoint is a call: never between a word's load and the jump, nor in a
                // gate.
                if actor.alive && matches!(actor.at, At::Running | At::Stub(_)) {
                    self.inbox.push_back(Msg::Report {
                        thread: actor.id,
                        epoch: self.epoch,
                        frames: actor.frames(false),
                        complete,
                    });
                }
            }
            Op::Spawn(thread) => {
                let Some(actor) = self.actors.get(thread) else { return Ok(()) };
                if actor.alive && actor.at == At::Running && self.actors.len() < MOST_THREADS {
                    let id = Thread(self.actors.len() as u64);
                    // The epoch is loaded and the birth announced before the thread runs.
                    self.births.push((id, self.epoch));
                    self.actors.push(Actor {
                        id,
                        alive: true,
                        stack: Vec::new(),
                        at: At::Running,
                        low: 0,
                    });
                }
            }
            Op::End(thread) => {
                let Some(actor) = self.actors.get_mut(thread) else { return Ok(()) };
                if actor.alive && actor.at == At::Running && actor.stack.is_empty() {
                    actor.alive = false;
                    self.inbox.push_back(Msg::Ended(actor.id));
                }
            }
            Op::Edit { first, count, together } => {
                // One transaction at a time, and each call's writes before the next call.
                if self.staged.is_some() || !self.pending.is_empty() {
                    return Ok(());
                }
                let txn = Txn(self.next_txn);
                self.next_txn += 1;
                let binds: Vec<Bind> = (0..1 + count % 3)
                    .map(|index| {
                        let slot = (first + index) % SLOTS;
                        Bind {
                            slot: Slot(slot),
                            len: 16 + 16 * ((u64::from(slot) + txn.0) % 3),
                            align: 16,
                            tier: Tier::Zero,
                            code: CodeId([txn.0 as u8; 32]),
                        }
                    })
                    .collect();
                self.launch
                    .stage(&Edit { txn, binds: &binds, together }, &mut self.out)
                    .map_err(|it| format!("stage: {it}"))?;
                let ranges = self
                    .out
                    .iter()
                    .filter_map(|it| match it {
                        Write::Place { range, .. } => Some(*range),
                        _ => None,
                    })
                    .collect();
                self.staged =
                    Some(StagedEdit { txn, binds, ranges, together, words: self.words.clone() });
                self.queue();
            }
            Op::Abort => {
                // Only once the staged ranges are written: the driver gives up while filling
                // them, and the machine's account of what was handed out is by write.
                if !self.pending.is_empty() {
                    return Ok(());
                }
                let Some(staged) = self.staged.take() else { return Ok(()) };
                self.launch.abort(staged.txn, &mut self.out).map_err(|it| format!("abort: {it}"))?;
                if self.out.iter().any(|it| matches!(it, Write::Slot { .. } | Write::Commit { .. }))
                    || self.words != staged.words
                {
                    return Err(format!("abort: edit {} changed a word", staged.txn.0));
                }
                self.queue();
            }
            Op::Commit => {
                if !self.pending.is_empty() {
                    return Ok(());
                }
                let Some(staged) = self.staged.take() else { return Ok(()) };
                self.launch
                    .commit(staged.txn, &mut self.out)
                    .map_err(|it| format!("commit: {it}"))?;
                // The oracle's versions: one for binds that switch together, else one a bind,
                // in the order their words are stored.
                let mut version = self.next_version;
                for (bind, range) in staged.binds.iter().zip(&staged.ranges) {
                    self.versions.insert(range.at, (bind.slot, version));
                    self.history[bind.slot.0 as usize].push(version);
                    if !staged.together {
                        version += 1;
                    }
                }
                self.next_version = version + u64::from(staged.together);
                self.queue();
            }
            Op::Write => {
                if let Some(write) = self.pending.pop_front() {
                    self.apply(write)?;
                }
            }
            Op::Flush => self.flush()?,
            Op::Deliver => {
                self.drain_births()?;
                match self.inbox.pop_front() {
                    Some(Msg::Report { thread, epoch, frames, complete }) => self
                        .launch
                        .report(thread, epoch, &frames, complete)
                        .map_err(|it| format!("report: {it}"))?,
                    Some(Msg::Ended(thread)) => {
                        self.launch.thread_ended(thread).map_err(|it| format!("ended: {it}"))?;
                    }
                    None => {}
                }
            }
            Op::Scan(thread) => {
                let Some(actor) = self.actors.get(thread) else { return Ok(()) };
                if actor.alive {
                    let (id, frames) = (actor.id, actor.frames(true));
                    self.drain_births()?;
                    self.launch
                        .report(id, self.epoch, &frames, true)
                        .map_err(|it| format!("scan: {it}"))?;
                }
            }
            Op::Collect => {
                self.drain_births()?;
                self.launch.collect(&mut self.out);
                self.queue();
            }
        }
        Ok(())
    }

    /// What must hold after every step.
    fn check(&self) -> Result<(), String> {
        for (thread, actor) in self.actors.iter().enumerate() {
            if let Some(body) = actor.stack.iter().find(|it| self.freed.contains(it)) {
                return Err(format!(
                    "freed: thread {thread} has a frame in {body:?}, which was freed"
                ));
            }
            if let At::Gate { gate, .. } = actor.at
                && self.freed.contains(&gate)
            {
                return Err(format!("freed: thread {thread} is in the gate {gate:?}, freed"));
            }
        }
        Ok(())
    }

    /// The end of a run: every thread leaves all code and reports, and then nothing unlinked
    /// may be left.
    fn settle(&mut self) -> Result<(), String> {
        self.flush()?;
        self.step(Op::Abort)?;
        self.flush()?;
        for thread in 0..self.actors.len() {
            // Out of a call in flight and out of a gate: at most the jump, the flag's load
            // and the jump.
            for _ in 0..3 {
                if matches!(self.actors[thread].at, At::Loaded { .. } | At::Gate { .. }) {
                    self.step(Op::Step(thread))?;
                }
            }
            self.actors[thread].stack.clear();
            self.step(Op::Safepoint(thread, true))?;
        }
        while !self.inbox.is_empty() {
            self.step(Op::Deliver)?;
        }
        self.step(Op::Collect)?;
        self.flush()?;
        self.check()?;
        if self.launch.waiting() != 0 {
            return Err(format!(
                "leak: {} batches wait though every thread reported an empty stack",
                self.launch.waiting()
            ));
        }
        let bound = self.words.iter().filter(|it| matches!(it, Target::Body(_))).count();
        if self.live.len() != bound {
            return Err(format!(
                "leak: {} ranges are live for {bound} bound bodies",
                self.live.len()
            ));
        }
        Ok(())
    }
}

/// Runs a schedule against the core with `plant`, checking the oracle after every step.
fn run(ops: &[Op], plant: Plant) -> Result<(), String> {
    let mut sim = Sim::new(plant)?;
    for (index, op) in ops.iter().enumerate() {
        sim.step(*op)
            .and_then(|()| sim.check())
            .map_err(|why| format!("{why} (op {index}: {op:?})"))?;
    }
    sim.settle().map_err(|why| format!("{why} (settling)"))
}

fn pick(tape: &Tape, max: usize, label: &'static str) -> usize {
    tape.int_in(0..=max, label)
}

fn op(tape: &Tape) -> Op {
    let thread = pick(tape, MOST_THREADS - 1, "thread");
    match pick(tape, 15, "op") {
        0..=2 => Op::Call(thread, pick(tape, SLOTS as usize - 1, "slot") as u32),
        3 | 4 => Op::Step(thread),
        5 => Op::Return(thread),
        6 => Op::Safepoint(thread, pick(tape, 7, "whole stack") != 0),
        7 => match pick(tape, 1, "spawn or end") {
            0 => Op::Spawn(thread),
            _ => Op::End(thread),
        },
        8 => Op::Edit {
            first: pick(tape, SLOTS as usize - 1, "first slot") as u32,
            count: pick(tape, 2, "count") as u32,
            together: tape.bool("together"),
        },
        9 => match pick(tape, 3, "abort or commit") {
            0 => Op::Abort,
            _ => Op::Commit,
        },
        10..=12 => Op::Write,
        13 => Op::Flush,
        14 => Op::Deliver,
        _ => match pick(tape, 1, "scan or collect") {
            0 => Op::Scan(thread),
            _ => Op::Collect,
        },
    }
}

/// The invariant: under any schedule of the control thread's stores against the threads'
/// calls, reports, births and ends, no thread is ever in freed code, no word leads to code
/// that was not written or bound, no thread sees an edit's functions new and then old, an
/// abort changes no word, no offset is handed out twice, no page with live code is given
/// back, and once every thread has left all code everything unlinked is freed.
#[test]
fn no_schedule_breaks_what_the_core_promises() {
    blaze_sim::check(blaze_sim::Config::cases(400), |tape| {
        let ops = tape.vec(1..=160, "ops", op);
        run(&ops, Plant::None)
    });
}

/// A control: the schedule passes on the core as it is, and with `plant` the oracle names
/// `finding`.
fn the_oracle_catches(ops: &[Op], plant: Plant, finding: &str) {
    assert_eq!(run(ops, Plant::None), Ok(()), "the core as it is must pass");
    let why = run(ops, plant).expect_err("the oracle missed the planted bug");
    assert!(why.starts_with(finding), "{why}");
}

/// Binds `slot` to a new body and carries every write out.
const fn bind(slot: u32) -> [Op; 4] {
    [Op::Edit { first: slot, count: 0, together: false }, Op::Flush, Op::Commit, Op::Flush]
}

#[test]
fn the_oracle_catches_an_edit_stored_without_its_gate() {
    let ops = [
        // Slots 0 and 1 bound, then edited together; of the commit's writes only the first
        // is done when the thread calls.
        Op::Edit { first: 0, count: 1, together: true },
        Op::Flush,
        Op::Commit,
        Op::Flush,
        Op::Edit { first: 0, count: 1, together: true },
        Op::Flush,
        Op::Commit,
        Op::Write,
        // Slot 0: through the gate, old, as the flag is clear. Without the gate: new.
        Op::Call(0, 0),
        Op::Step(0),
        Op::Step(0),
        Op::Step(0),
        Op::Return(0),
        // Slot 1, whose word is not stored yet: old.
        Op::Call(0, 1),
        Op::Step(0),
    ];
    the_oracle_catches(&ops, Plant::NoGate, "together:");
}

#[test]
fn the_oracle_catches_an_epoch_published_before_the_unlinking_stores() {
    let [a, b, c, d] = bind(0);
    let ops = [
        a,
        b,
        c,
        d,
        Op::Edit { first: 0, count: 0, together: false },
        Op::Flush,
        Op::Commit,
        // As it is: the word. Planted: the epoch, with the word still on the old body.
        Op::Write,
        // The thread reports an empty stack at the epoch it loads, then calls.
        Op::Safepoint(0, true),
        Op::Call(0, 0),
        Op::Step(0),
        Op::Deliver,
        Op::Collect,
        Op::Flush,
    ];
    the_oracle_catches(&ops, Plant::EpochFirst, "freed:");
}

#[test]
fn the_oracle_catches_a_batch_freed_before_its_threads_cleared_it() {
    let [a, b, c, d] = bind(0);
    let ops = [a, b, c, d, Op::Call(0, 0), Op::Step(0), a, b, c, d, Op::Collect, Op::Flush];
    the_oracle_catches(&ops, Plant::FreeEarly, "freed:");
}

#[test]
fn the_oracle_catches_a_return_address_at_a_bodys_end_taken_for_outside_it() {
    let [a, b, c, d] = bind(0);
    let ops = [
        a,
        b,
        c,
        d,
        // The first body is at offset 0, so the thread's return address into it, as the
        // machine picks them, is its end.
        Op::Call(0, 0),
        Op::Step(0),
        a,
        b,
        c,
        d,
        Op::Safepoint(0, true),
        Op::Deliver,
        Op::Collect,
        Op::Flush,
    ];
    the_oracle_catches(&ops, Plant::ReturnAtEnd, "freed:");
}

/// A thread scanned from outside between a word's load and its jump holds the body in a
/// register: the scan reports it, and the body outlives the call.
#[test]
fn a_call_in_flight_holds_the_body_it_loaded() {
    let [a, b, c, d] = bind(0);
    let ops = [
        a,
        b,
        c,
        d,
        Op::Call(0, 0),
        a,
        b,
        c,
        d,
        Op::Scan(0),
        Op::Collect,
        Op::Flush,
        Op::Step(0),
    ];
    assert_eq!(run(&ops, Plant::None), Ok(()));
}
