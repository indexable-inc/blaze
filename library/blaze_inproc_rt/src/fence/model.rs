//! The fence protocol checked exhaustively: every interleaving of the daemon retiring one range
//! and a few threads calling through its slot, quiescing, claiming slots, being born and dying,
//! at the grain of the single shared-memory accesses `fence.rs` and `runtime.rs` make.
//!
//! Sequentially consistent: every access to the epoch, `births` and the slots is `SeqCst`; the
//! entry's seqlock is one access here (a reader that sees it change skips it, which is what a
//! read of the grace before the entry was posted does), and the unlinking store is ordered
//! before the epoch's store by the fence in [`super::Fences::start`]. Two loads a function makes
//! with nothing between them that a thread could see (`holds`, the adopt test) are one step:
//! each outcome they give is the outcome of reading both at one of the two points.
//!
//! Checked over the whole graph of reachable states:
//! - safety: when the daemon frees the range, no thread is in it nor has loaded the old slot
//!   word and not yet made the call;
//! - liveness: from every reachable state, a state with the range freed is reachable (a fence
//!   passes once the threads run on);
//! - the check has teeth: the daemon frees while a thread is alive in some run, and each
//!   mutation of the protocol below is caught.

use std::collections::HashMap;
use std::hash::BuildHasherDefault;

const NONE: u8 = u8::MAX;
const MOST_THREADS: usize = 3;
const MOST_SLOTS: usize = 3;
/// The grace the range is posted under: the epoch starts at 1.
const GRACE: u8 = 1;

/// A change to the protocol the model must catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mutation {
    None,
    /// The daemon trusts slots without counting them against the threads listed.
    NoCount,
    /// The epoch moves before the unlinking store.
    EarlyStart,
    /// A quiescent point reports the epoch whatever its stack holds.
    NoStack,
}

#[derive(Clone, Copy, Debug)]
struct Config {
    /// Threads alive from the start (records `0..threads`).
    threads: usize,
    /// Thread 0 may start one more thread (the last record).
    spawn: bool,
    /// The started thread gets the ID of thread 1, which must be dead first.
    reuse: bool,
    /// A thread may die without giving its slot back.
    abrupt: bool,
    slots: usize,
    /// Calls through the slot each thread makes.
    calls: u8,
    /// Quiescent points each thread reaches.
    quiesces: u8,
    mutation: Mutation,
    /// Explore one state per class of [`canonical`]; off, every state as it steps.
    reduce: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum SlotState {
    Free,
    Claiming,
    Online,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Slot {
    state: SlotState,
    tid: u8,
    seen: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
enum Pc {
    Unborn,
    /// Between calls: may call, quiesce, start a thread or end.
    Top,
    /// Loaded the slot word, the call not made yet: no quiescent point is here.
    Called,
    /// In the function it called: may quiesce or return.
    Inside,
    QEpoch,
    QHolds,
    QScan,
    QCheck,
    QReport,
    QBegin,
    QAdopt(u8),
    QAdoptSeen(u8),
    QAdoptOnline(u8),
    QFree(u8),
    QTid(u8),
    QSeen(u8),
    QOnline(u8),
    /// Its thread-local's destructor: the slot given back.
    XRelease,
    /// Gone from the OS's list.
    XGone,
    Dead,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
struct Thread {
    pc: Pc,
    tid: u8,
    /// Where a quiescent point returns to: `Inside` or `Top`.
    inside: bool,
    /// Loaded the old slot word.
    old: bool,
    /// A frame of its stack is in the range.
    in_range: bool,
    epoch: u8,
    seen: u8,
    slot: u8,
    reported: u8,
    calls: u8,
    quiesces: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Part {
    State,
    Tid,
    Seen,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Dpc {
    Unlink,
    Post,
    Start,
    Births,
    ListBegin,
    ListEnd,
    Slot(u8, Part),
    Check,
    Count,
    Reap(u8),
    Decide,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Daemon {
    pc: Dpc,
    births: u8,
    /// The thread IDs listed, as a bitset.
    list: u8,
    counted: u8,
    oldest: u8,
    /// Slots of threads not listed, as a bitset.
    dead: u8,
    /// Whether the unlinking store was made (for `EarlyStart`, which moves it last).
    unlinked: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct State {
    /// The slot word points at the new code.
    new: bool,
    epoch: u8,
    /// The table entry's grace: 0 until posted.
    grace: u8,
    births: u8,
    slots: [Slot; MOST_SLOTS],
    /// Live thread IDs, as the OS has them.
    alive: u8,
    /// While the daemon lists threads: IDs alive at some point of it, and IDs alive all of it.
    listing: bool,
    during: u8,
    throughout: u8,
    threads: [Thread; MOST_THREADS],
    daemon: Daemon,
}

fn bit(tid: u8) -> u8 {
    1 << tid
}

impl State {
    fn initial(config: &Config) -> State {
        let thread = |index: usize| Thread {
            pc: if index < config.threads { Pc::Top } else { Pc::Unborn },
            tid: if index < config.threads { index as u8 + 1 } else { 0 },
            inside: false,
            old: false,
            in_range: false,
            epoch: 0,
            seen: 0,
            slot: NONE,
            reported: 0,
            calls: config.calls,
            quiesces: config.quiesces,
        };
        let threads = [thread(0), thread(1), thread(2)];
        let alive = threads.iter().filter(|it| it.pc == Pc::Top).fold(0, |set, it| set | bit(it.tid));
        State {
            new: false,
            epoch: 1,
            grace: 0,
            births: 0,
            slots: [Slot { state: SlotState::Free, tid: 0, seen: 0 }; MOST_SLOTS],
            alive,
            listing: false,
            during: 0,
            throughout: 0,
            threads,
            daemon: Daemon {
                pc: if config.mutation == Mutation::EarlyStart { Dpc::Post } else { Dpc::Unlink },
                births: 0,
                list: 0,
                counted: 0,
                oldest: NONE,
                dead: 0,
                unlinked: false,
            },
        }
    }

    fn holds(&self, index: usize) -> bool {
        let thread = &self.threads[index];
        thread.slot != NONE && {
            let slot = &self.slots[thread.slot as usize];
            slot.state == SlotState::Online && slot.tid == thread.tid
        }
    }

    /// Whether a thread is in the range or about to be.
    fn exposed(&self) -> Option<usize> {
        self.threads.iter().position(|it| it.old || it.in_range)
    }
}

fn at(state: &mut State, index: usize) -> &mut Thread {
    &mut state.threads[index]
}

/// One step of thread `index`, every way it can go.
fn thread_steps(state: &State, index: usize, config: &Config, out: &mut Vec<State>) {
    let thread = state.threads[index];
    let mut next = *state;
    let finish = |next: &mut State| {
        let thread = &mut next.threads[index];
        thread.reported = if thread.seen == thread.epoch { thread.epoch } else { 0 };
        thread.pc = if thread.inside { Pc::Inside } else { Pc::Top };
    };
    match thread.pc {
        Pc::Unborn | Pc::Dead => {}
        Pc::Top => {
            if thread.calls > 0 {
                let mut call = *state;
                let it = at(&mut call, index);
                it.calls -= 1;
                it.old = !state.new;
                it.pc = Pc::Called;
                out.push(call);
            }
            if thread.quiesces > 0 {
                let mut quiesce = *state;
                let it = at(&mut quiesce, index);
                it.quiesces -= 1;
                it.inside = false;
                it.pc = Pc::QEpoch;
                out.push(quiesce);
            }
            let mut end = *state;
            at(&mut end, index).pc = Pc::XRelease;
            out.push(end);
            if config.abrupt {
                let mut die = *state;
                at(&mut die, index).pc = Pc::XGone;
                out.push(die);
            }
            let last = MOST_THREADS - 1;
            if index == 0
                && config.spawn
                && state.threads[last].pc == Pc::Unborn
                && (!config.reuse || state.threads[1].pc == Pc::Dead)
            {
                let mut spawn = *state;
                let tid = if config.reuse { state.threads[1].tid } else { last as u8 + 1 };
                let child = &mut spawn.threads[last];
                child.pc = Pc::Top;
                child.tid = tid;
                spawn.alive |= bit(tid);
                if spawn.listing {
                    spawn.during |= bit(tid);
                }
                out.push(spawn);
            }
        }
        Pc::Called => {
            let it = at(&mut next, index);
            it.in_range = it.old;
            it.old = false;
            it.pc = Pc::Inside;
            out.push(next);
        }
        Pc::Inside => {
            let mut back = *state;
            let it = at(&mut back, index);
            it.in_range = false;
            it.pc = Pc::Top;
            out.push(back);
            if thread.quiesces > 0 {
                let it = at(&mut next, index);
                it.quiesces -= 1;
                it.inside = true;
                it.pc = Pc::QEpoch;
                out.push(next);
            }
        }
        Pc::QEpoch => {
            let epoch = state.epoch;
            let it = at(&mut next, index);
            it.epoch = epoch;
            it.pc = if it.slot != NONE && it.reported == epoch { Pc::QHolds } else { Pc::QScan };
            out.push(next);
        }
        Pc::QHolds => {
            if state.holds(index) {
                let it = at(&mut next, index);
                it.pc = if it.inside { Pc::Inside } else { Pc::Top };
            } else {
                let it = at(&mut next, index);
                it.slot = NONE;
                it.pc = Pc::QScan;
            }
            out.push(next);
        }
        Pc::QScan => {
            let grace = state.grace;
            let it = at(&mut next, index);
            let frames = it.in_range && config.mutation != Mutation::NoStack;
            it.seen = if grace != 0 && grace < it.epoch && frames { grace } else { it.epoch };
            it.pc = Pc::QCheck;
            out.push(next);
        }
        Pc::QCheck => {
            let holds = state.holds(index);
            let it = at(&mut next, index);
            if holds {
                it.pc = Pc::QReport;
            } else {
                it.slot = NONE;
                it.pc = Pc::QBegin;
            }
            out.push(next);
        }
        Pc::QReport => {
            next.slots[thread.slot as usize].seen = thread.seen;
            finish(&mut next);
            out.push(next);
        }
        Pc::QBegin => {
            next.births += 1;
            at(&mut next, index).pc = Pc::QAdopt(0);
            out.push(next);
        }
        Pc::QAdopt(k) => {
            let k = k as usize;
            if k == config.slots {
                at(&mut next, index).pc = Pc::QFree(0);
            } else if state.slots[k].state == SlotState::Online && state.slots[k].tid == thread.tid {
                next.slots[k].state = SlotState::Claiming;
                at(&mut next, index).pc = Pc::QAdoptSeen(k as u8);
            } else {
                at(&mut next, index).pc = Pc::QAdopt(k as u8 + 1);
            }
            out.push(next);
        }
        Pc::QAdoptSeen(k) => {
            next.slots[k as usize].seen = thread.seen;
            at(&mut next, index).pc = Pc::QAdoptOnline(k);
            out.push(next);
        }
        Pc::QAdoptOnline(k) | Pc::QOnline(k) => {
            next.slots[k as usize].state = SlotState::Online;
            at(&mut next, index).slot = k;
            finish(&mut next);
            out.push(next);
        }
        Pc::QFree(k) => {
            let k = k as usize;
            if k == config.slots {
                at(&mut next, index).slot = NONE;
                finish(&mut next);
            } else if state.slots[k].state == SlotState::Free {
                next.slots[k].state = SlotState::Claiming;
                at(&mut next, index).pc = Pc::QTid(k as u8);
            } else {
                at(&mut next, index).pc = Pc::QFree(k as u8 + 1);
            }
            out.push(next);
        }
        Pc::QTid(k) => {
            next.slots[k as usize].tid = thread.tid;
            at(&mut next, index).pc = Pc::QSeen(k);
            out.push(next);
        }
        Pc::QSeen(k) => {
            next.slots[k as usize].seen = thread.seen;
            at(&mut next, index).pc = Pc::QOnline(k);
            out.push(next);
        }
        Pc::XRelease => {
            if thread.slot != NONE {
                next.slots[thread.slot as usize].state = SlotState::Free;
            }
            at(&mut next, index).pc = Pc::XGone;
            out.push(next);
        }
        Pc::XGone => {
            next.alive &= !bit(thread.tid);
            next.throughout &= !bit(thread.tid);
            at(&mut next, index).pc = Pc::Dead;
            out.push(next);
        }
    }
}

fn restart(daemon: &mut Daemon) {
    daemon.pc = Dpc::Births;
    daemon.list = 0;
    daemon.counted = 0;
    daemon.oldest = NONE;
    daemon.dead = 0;
}

/// One step of the daemon, every way it can go; `Err` names the thread exposed when it frees.
fn daemon_steps(state: &State, config: &Config, out: &mut Vec<State>) -> Result<(), usize> {
    let mut next = *state;
    match state.daemon.pc {
        Dpc::Unlink => {
            next.new = true;
            next.daemon.unlinked = true;
            next.daemon.pc = if config.mutation == Mutation::EarlyStart { Dpc::Births } else { Dpc::Post };
        }
        Dpc::Post => {
            next.grace = state.epoch;
            next.daemon.pc = Dpc::Start;
        }
        Dpc::Start => {
            next.epoch = state.epoch + 1;
            next.daemon.pc = if config.mutation == Mutation::EarlyStart { Dpc::Unlink } else { Dpc::Births };
        }
        Dpc::Births => {
            next.daemon.births = state.births;
            next.daemon.pc = Dpc::ListBegin;
        }
        Dpc::ListBegin => {
            next.listing = true;
            next.during = state.alive;
            next.throughout = state.alive;
            next.daemon.pc = Dpc::ListEnd;
        }
        Dpc::ListEnd => {
            let (must, may) = (state.throughout, state.during);
            let extra = may & !must;
            // Every list the OS could give: all IDs alive throughout, any of the others.
            let mut subset = extra;
            loop {
                let mut listed = *state;
                listed.listing = false;
                listed.during = 0;
                listed.throughout = 0;
                listed.daemon.list = must | subset;
                listed.daemon.pc = Dpc::Slot(0, Part::State);
                out.push(listed);
                if subset == 0 {
                    break;
                }
                subset = (subset - 1) & extra;
            }
            return Ok(());
        }
        Dpc::Slot(k, part) => {
            let index = k as usize;
            if index == config.slots {
                next.daemon.pc = Dpc::Check;
            } else {
                let slot = state.slots[index];
                match part {
                    Part::State => {
                        next.daemon.pc = if slot.state == SlotState::Online {
                            Dpc::Slot(k, Part::Tid)
                        } else {
                            Dpc::Slot(k + 1, Part::State)
                        };
                    }
                    Part::Tid => {
                        if next.daemon.list & bit(slot.tid) == 0 {
                            next.daemon.dead |= 1 << k;
                            next.daemon.pc = Dpc::Slot(k + 1, Part::State);
                        } else {
                            next.daemon.pc = Dpc::Slot(k, Part::Seen);
                        }
                    }
                    Part::Seen => {
                        next.daemon.counted += 1;
                        next.daemon.oldest = next.daemon.oldest.min(slot.seen);
                        next.daemon.pc = Dpc::Slot(k + 1, Part::State);
                    }
                }
            }
        }
        Dpc::Check => {
            if state.births != state.daemon.births {
                restart(&mut next.daemon);
            } else {
                next.daemon.pc = Dpc::Count;
            }
        }
        Dpc::Count => {
            let counted_all = u32::from(next.daemon.counted) == next.daemon.list.count_ones();
            if counted_all || config.mutation == Mutation::NoCount {
                next.daemon.pc = Dpc::Reap(0);
            } else {
                restart(&mut next.daemon);
            }
        }
        Dpc::Reap(k) => {
            let index = k as usize;
            if index == config.slots {
                next.daemon.pc = Dpc::Decide;
            } else {
                if state.daemon.dead & (1 << k) != 0 && state.slots[index].state == SlotState::Online {
                    next.slots[index].state = SlotState::Free;
                }
                next.daemon.pc = Dpc::Reap(k + 1);
            }
        }
        Dpc::Decide => {
            let ready = state.daemon.unlinked && state.grace != 0 && state.epoch > state.grace;
            if ready && state.daemon.oldest > GRACE {
                if let Some(thread) = state.exposed() {
                    return Err(thread);
                }
                next.daemon.pc = Dpc::Done;
            } else {
                restart(&mut next.daemon);
            }
        }
        Dpc::Done => return Ok(()),
    }
    out.push(next);
    Ok(())
}

/// The representative of `state`, exploring one state per class checks both properties:
/// - A dead thread's record is its ID alone: nothing steps it, a spawn reads only its ID, and
///   it died from `Top`, where it neither holds the old word nor is in the range.
/// - Out of a quiescent point a thread's `epoch`, `seen` and `inside` are dead: a quiescent
///   point writes each before it reads it, and nothing else reads them. So is `reported`
///   when it holds no slot: only `QEpoch` reads it, past a test that its slot is set, and only
///   the thread's own quiescent points set its slot.
/// - A thread at `Called` is taken on to `Inside`: that step reads only the thread's own
///   `old`, and nothing tells the two apart (`exposed` reads `old || in_range`, a spawn and
///   the count of threads alive test only for `Unborn` and `Dead`), so it commutes with every
///   other step and changes neither property.
/// - Renaming the threads, when every thread plays the same part (no spawn: thread `i` is ID
///   `i + 1` throughout). A thread ID only meets another in equality tests and as a bit of a
///   set, so renaming the records and every ID with them (the slots' IDs, the OS's list, the
///   daemon's list) gives a state that steps the same way, freeing exposes a thread in one
///   exactly when it does in the other, and the range is freed from one exactly when from the
///   other. Sorted by everything of a thread but its ID; ties only cost reduction.
fn canonical(mut state: State, config: &Config) -> State {
    if !config.reduce {
        return state;
    }
    for thread in &mut state.threads {
        if thread.pc == Pc::Dead {
            debug_assert!(!thread.old && !thread.in_range, "a thread died in the range");
            *thread = Thread {
                pc: Pc::Dead,
                tid: thread.tid,
                inside: false,
                old: false,
                in_range: false,
                epoch: 0,
                seen: 0,
                slot: NONE,
                reported: 0,
                calls: 0,
                quiesces: 0,
            };
        } else if matches!(thread.pc, Pc::Unborn | Pc::Top | Pc::Called | Pc::Inside | Pc::XRelease | Pc::XGone) {
            if thread.pc == Pc::Called {
                thread.in_range = thread.old;
                thread.old = false;
                thread.pc = Pc::Inside;
            }
            thread.epoch = 0;
            thread.seen = 0;
            thread.inside = false;
            if thread.slot == NONE {
                thread.reported = 0;
            }
        }
    }
    if config.spawn {
        return state;
    }
    let live = config.threads;
    let mut order = [0usize, 1, 2];
    order[..live].sort_by_key(|&it| Thread { tid: 0, ..state.threads[it] });
    if order[..live].iter().enumerate().all(|(at, &it)| at == it) {
        return state;
    }
    let mut rename = [0u8; MOST_THREADS + 1];
    for (to, &from) in order[..live].iter().enumerate() {
        rename[from + 1] = to as u8 + 1;
    }
    let set = |bits: u8| (1..=live).filter(|&tid| bits & bit(tid as u8) != 0).fold(0, |out, tid| out | bit(rename[tid]));
    let mut out = state;
    for (to, &from) in order[..live].iter().enumerate() {
        out.threads[to] = Thread { tid: to as u8 + 1, ..state.threads[from] };
    }
    for slot in &mut out.slots {
        slot.tid = rename[slot.tid as usize];
    }
    out.alive = set(state.alive);
    out.during = set(state.during);
    out.throughout = set(state.throughout);
    out.daemon.list = set(state.daemon.list);
    out
}

/// rustc's FxHash, for the explorer's table: a state is about a hundred one-byte writes,
/// which SipHash makes the explorer's whole cost.
#[derive(Default)]
struct Fx(u64);

impl Fx {
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl std::hash::Hasher for Fx {
    /// Rotated so the table's low bits come from every word, not from the words' low bits.
    fn finish(&self) -> u64 {
        self.0.rotate_left(26)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.add(u64::from(byte));
        }
    }

    fn write_u8(&mut self, value: u8) {
        self.add(u64::from(value));
    }

    fn write_u32(&mut self, value: u32) {
        self.add(u64::from(value));
    }

    fn write_u64(&mut self, value: u64) {
        self.add(value);
    }

    fn write_usize(&mut self, value: usize) {
        self.add(value as u64);
    }

    fn write_isize(&mut self, value: isize) {
        self.add(value as u64);
    }
}

/// A [`State`] in 160 bits, for the explorer's table, which holds every state twice: a
/// `State` spends a byte or more on each field.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Packed([u32; 5]);

impl std::hash::Hash for Packed {
    fn hash<H: std::hash::Hasher>(&self, hasher: &mut H) {
        let [a, b, c, d, e] = self.0;
        hasher.write_u64(u64::from(a) | u64::from(b) << 32);
        hasher.write_u64(u64::from(c) | u64::from(d) << 32);
        hasher.write_u32(e);
    }
}

/// The fields of a [`Packed`], each in the bits it is given.
#[derive(Default)]
struct Bits {
    words: [u64; 3],
    at: u32,
}

impl Bits {
    fn put(&mut self, value: u8, bits: u32) {
        let value = u64::from(value);
        assert!(value >> bits == 0, "a model field outgrew its {bits} bits: {value}");
        let (word, offset) = ((self.at / 64) as usize, self.at % 64);
        self.words[word] |= value << offset;
        if offset + bits > 64 {
            self.words[word + 1] |= value >> (64 - offset);
        }
        self.at += bits;
    }

    fn take(&mut self, bits: u32) -> u8 {
        let (word, offset) = ((self.at / 64) as usize, self.at % 64);
        let mut value = self.words[word] >> offset;
        if offset + bits > 64 {
            value |= self.words[word + 1] << (64 - offset);
        }
        self.at += bits;
        (value & ((1 << bits) - 1)) as u8
    }

    /// A slot index or `NONE` in two bits.
    fn put_index(&mut self, value: u8) {
        assert!(value == NONE || value < 3, "index {value} past the model's slots");
        self.put(if value == NONE { 3 } else { value }, 2);
    }

    fn take_index(&mut self) -> u8 {
        match self.take(2) {
            3 => NONE,
            it => it,
        }
    }
}

const _: () = assert!(MOST_SLOTS == 3 && MOST_THREADS == 3, "the codes below list three slots and threads");

/// Every [`Pc`], at its code.
const PCS: [Pc; 36] = [
    Pc::Unborn,
    Pc::Top,
    Pc::Called,
    Pc::Inside,
    Pc::QEpoch,
    Pc::QHolds,
    Pc::QScan,
    Pc::QCheck,
    Pc::QReport,
    Pc::QBegin,
    Pc::XRelease,
    Pc::XGone,
    Pc::Dead,
    Pc::QAdopt(0),
    Pc::QAdopt(1),
    Pc::QAdopt(2),
    Pc::QAdopt(3),
    Pc::QFree(0),
    Pc::QFree(1),
    Pc::QFree(2),
    Pc::QFree(3),
    Pc::QAdoptSeen(0),
    Pc::QAdoptSeen(1),
    Pc::QAdoptSeen(2),
    Pc::QAdoptOnline(0),
    Pc::QAdoptOnline(1),
    Pc::QAdoptOnline(2),
    Pc::QTid(0),
    Pc::QTid(1),
    Pc::QTid(2),
    Pc::QSeen(0),
    Pc::QSeen(1),
    Pc::QSeen(2),
    Pc::QOnline(0),
    Pc::QOnline(1),
    Pc::QOnline(2),
];

fn pc_code(pc: Pc) -> u8 {
    match pc {
        Pc::Unborn => 0,
        Pc::Top => 1,
        Pc::Called => 2,
        Pc::Inside => 3,
        Pc::QEpoch => 4,
        Pc::QHolds => 5,
        Pc::QScan => 6,
        Pc::QCheck => 7,
        Pc::QReport => 8,
        Pc::QBegin => 9,
        Pc::XRelease => 10,
        Pc::XGone => 11,
        Pc::Dead => 12,
        Pc::QAdopt(k) if k <= 3 => 13 + k,
        Pc::QFree(k) if k <= 3 => 17 + k,
        Pc::QAdoptSeen(k) if k < 3 => 21 + k,
        Pc::QAdoptOnline(k) if k < 3 => 24 + k,
        Pc::QTid(k) if k < 3 => 27 + k,
        Pc::QSeen(k) if k < 3 => 30 + k,
        Pc::QOnline(k) if k < 3 => 33 + k,
        _ => panic!("{pc:?} is past the model's slots"),
    }
}

/// Every [`Dpc`], at its code.
const DPCS: [Dpc; 26] = [
    Dpc::Unlink,
    Dpc::Post,
    Dpc::Start,
    Dpc::Births,
    Dpc::ListBegin,
    Dpc::ListEnd,
    Dpc::Check,
    Dpc::Count,
    Dpc::Decide,
    Dpc::Done,
    Dpc::Slot(0, Part::State),
    Dpc::Slot(0, Part::Tid),
    Dpc::Slot(0, Part::Seen),
    Dpc::Slot(1, Part::State),
    Dpc::Slot(1, Part::Tid),
    Dpc::Slot(1, Part::Seen),
    Dpc::Slot(2, Part::State),
    Dpc::Slot(2, Part::Tid),
    Dpc::Slot(2, Part::Seen),
    Dpc::Slot(3, Part::State),
    Dpc::Slot(3, Part::Tid),
    Dpc::Slot(3, Part::Seen),
    Dpc::Reap(0),
    Dpc::Reap(1),
    Dpc::Reap(2),
    Dpc::Reap(3),
];

fn dpc_code(pc: Dpc) -> u8 {
    match pc {
        Dpc::Unlink => 0,
        Dpc::Post => 1,
        Dpc::Start => 2,
        Dpc::Births => 3,
        Dpc::ListBegin => 4,
        Dpc::ListEnd => 5,
        Dpc::Check => 6,
        Dpc::Count => 7,
        Dpc::Decide => 8,
        Dpc::Done => 9,
        Dpc::Slot(k, part) if k <= 3 => 10 + 3 * k + part as u8,
        Dpc::Reap(k) if k <= 3 => 22 + k,
        _ => panic!("{pc:?} is past the model's slots"),
    }
}

const SLOT_STATES: [SlotState; 3] = [SlotState::Free, SlotState::Claiming, SlotState::Online];

fn pack(state: &State) -> Packed {
    let mut bits = Bits::default();
    bits.put(u8::from(state.new), 1);
    bits.put(state.epoch, 2);
    bits.put(state.grace, 2);
    bits.put(state.births, 4);
    for slot in &state.slots {
        bits.put(slot.state as u8, 2);
        bits.put(slot.tid, 2);
        bits.put(slot.seen, 2);
    }
    bits.put(state.alive, 4);
    bits.put(u8::from(state.listing), 1);
    bits.put(state.during, 4);
    bits.put(state.throughout, 4);
    for thread in &state.threads {
        bits.put(pc_code(thread.pc), 6);
        bits.put(thread.tid, 2);
        bits.put(u8::from(thread.inside), 1);
        bits.put(u8::from(thread.old), 1);
        bits.put(u8::from(thread.in_range), 1);
        bits.put(thread.epoch, 2);
        bits.put(thread.seen, 2);
        bits.put_index(thread.slot);
        bits.put(thread.reported, 2);
        bits.put(thread.calls, 2);
        bits.put(thread.quiesces, 2);
    }
    let daemon = &state.daemon;
    bits.put(dpc_code(daemon.pc), 5);
    bits.put(daemon.births, 4);
    bits.put(daemon.list, 4);
    bits.put(daemon.counted, 2);
    bits.put_index(daemon.oldest);
    bits.put(daemon.dead, 3);
    bits.put(u8::from(daemon.unlinked), 1);
    assert!(bits.at <= 160, "a state is {} bits", bits.at);
    let [a, b, c] = bits.words;
    Packed([a as u32, (a >> 32) as u32, b as u32, (b >> 32) as u32, c as u32])
}

fn unpack(packed: Packed) -> State {
    let [a, b, c, d, e] = packed.0.map(u64::from);
    let mut bits = Bits { words: [a | b << 32, c | d << 32, e], at: 0 };
    let new = bits.take(1) != 0;
    let epoch = bits.take(2);
    let grace = bits.take(2);
    let births = bits.take(4);
    let slots = [(); MOST_SLOTS].map(|()| Slot {
        state: SLOT_STATES[bits.take(2) as usize],
        tid: bits.take(2),
        seen: bits.take(2),
    });
    let alive = bits.take(4);
    let listing = bits.take(1) != 0;
    let during = bits.take(4);
    let throughout = bits.take(4);
    let threads = [(); MOST_THREADS].map(|()| Thread {
        pc: PCS[bits.take(6) as usize],
        tid: bits.take(2),
        inside: bits.take(1) != 0,
        old: bits.take(1) != 0,
        in_range: bits.take(1) != 0,
        epoch: bits.take(2),
        seen: bits.take(2),
        slot: bits.take_index(),
        reported: bits.take(2),
        calls: bits.take(2),
        quiesces: bits.take(2),
    });
    let daemon = Daemon {
        pc: DPCS[bits.take(5) as usize],
        births: bits.take(4),
        list: bits.take(4),
        counted: bits.take(2),
        oldest: bits.take_index(),
        dead: bits.take(3),
        unlinked: bits.take(1) != 0,
    };
    State { new, epoch, grace, births, slots, alive, listing, during, throughout, threads, daemon }
}

struct Explored {
    states: usize,
    /// Done states with a thread still alive.
    freed_while_running: usize,
}

/// The most states one exploration holds (about 120 bytes each with the table and the edges):
/// past it the config is too big for a test, an error rather than a run of hours.
const MOST_STATES: usize = 1 << 25;

/// Explores every reachable state; `Err` is the trace to a violation.
fn explore(config: Config) -> Result<Explored, String> {
    explore_within(config, MOST_STATES)
}

fn explore_within(config: Config, most: usize) -> Result<Explored, String> {
    let initial = pack(&canonical(State::initial(&config), &config));
    let mut index: HashMap<Packed, u32, BuildHasherDefault<Fx>> = HashMap::default();
    let mut states: Vec<Packed> = vec![initial];
    let mut parent: Vec<u32> = vec![u32::MAX];
    // The edges: each stepped state, with the end of its successors in `targets`.
    let mut stepped: Vec<(u32, u32)> = Vec::new();
    let mut targets: Vec<u32> = Vec::new();
    index.insert(initial, 0);
    let mut stack = vec![0u32];
    let mut next = Vec::new();
    let trace = |states: &[Packed], parent: &[u32], mut at: u32| {
        let mut steps = Vec::new();
        while at != u32::MAX {
            steps.push(format!("{:?}", unpack(states[at as usize])));
            at = parent[at as usize];
        }
        steps.reverse();
        steps.join("\n")
    };
    while let Some(at) = stack.pop() {
        let state = unpack(states[at as usize]);
        // Past the free nothing is checked: the daemon steps no more, and liveness asks only
        // that a freed state be reached.
        if state.daemon.pc == Dpc::Done {
            continue;
        }
        next.clear();
        for thread in 0..MOST_THREADS {
            thread_steps(&state, thread, &config, &mut next);
        }
        if let Err(thread) = daemon_steps(&state, &config, &mut next) {
            return Err(format!(
                "{config:?}: the range was freed while thread {thread} was in it or about to be:\n{}",
                trace(&states, &parent, at)
            ));
        }
        for successor in next.drain(..) {
            let successor = canonical(successor, &config);
            assert!(
                !config.reduce || successor.threads.iter().all(|it| it.pc != Pc::Called),
                "a state kept a thread at Called"
            );
            let packed = pack(&successor);
            let to = *index.entry(packed).or_insert_with(|| {
                assert_eq!(unpack(packed), successor, "a state did not survive packing");
                states.push(packed);
                parent.push(at);
                stack.push((states.len() - 1) as u32);
                (states.len() - 1) as u32
            });
            targets.push(to);
        }
        stepped.push((at, u32::try_from(targets.len()).expect("more edges than a u32 counts")));
        if states.len() > most {
            return Err(format!(
                "{config:?}: past {most} states ({} not yet stepped), too many to explore in a test",
                stack.len()
            ));
        }
    }

    // Liveness: every state reaches one where the range is freed (backwards from those).
    let count = states.len();
    let edges = (0..stepped.len()).flat_map(|row| {
        let start = if row == 0 { 0 } else { stepped[row - 1].1 as usize };
        let (source, end) = stepped[row];
        targets[start..end as usize].iter().map(move |&to| (to, source))
    });
    let sources = blaze_radix::Csr::build(count, edges);
    drop((stepped, targets));
    let mut reaches = vec![false; count];
    let mut work: Vec<u32> = Vec::new();
    for (at, &state) in states.iter().enumerate() {
        if unpack(state).daemon.pc == Dpc::Done {
            reaches[at] = true;
            work.push(at as u32);
        }
    }
    while let Some(at) = work.pop() {
        for &source in sources.row(at as usize) {
            if !reaches[source as usize] {
                reaches[source as usize] = true;
                work.push(source);
            }
        }
    }
    if let Some(stuck) = reaches.iter().position(|it| !it) {
        return Err(format!(
            "{config:?}: a state from which the range is never freed:\n{}",
            trace(&states, &parent, stuck as u32)
        ));
    }
    let freed_while_running = states
        .iter()
        .map(|&it| unpack(it))
        .filter(|it| it.daemon.pc == Dpc::Done && it.threads.iter().any(|t| !matches!(t.pc, Pc::Unborn | Pc::Dead)))
        .count();
    Ok(Explored { states: count, freed_while_running })
}

const BASE: Config = Config {
    threads: 2,
    spawn: false,
    reuse: false,
    abrupt: false,
    slots: 2,
    calls: 2,
    quiesces: 2,
    mutation: Mutation::None,
    reduce: true,
};

fn check(config: Config) {
    let explored = explore(config).unwrap_or_else(|it| panic!("{it}"));
    assert!(explored.states > 1000, "{config:?}: only {} states", explored.states);
    assert!(explored.freed_while_running > 0, "{config:?}: the range is never freed while a thread runs");
}

#[test]
fn two_threads() {
    check(BASE);
}

#[test]
fn a_thread_starts_while_the_daemon_looks() {
    check(Config { threads: 1, spawn: true, slots: 3, calls: 1, quiesces: 1, ..BASE });
}

#[test]
fn every_code_is_its_place_in_the_table() {
    for (code, &pc) in PCS.iter().enumerate() {
        assert_eq!(usize::from(pc_code(pc)), code, "{pc:?}");
    }
    for (code, &pc) in DPCS.iter().enumerate() {
        assert_eq!(usize::from(dpc_code(pc)), code, "{pc:?}");
    }
    for (code, &state) in SLOT_STATES.iter().enumerate() {
        assert_eq!(state as usize, code, "{state:?}");
    }
}

/// Each reduction fires: the reduced graph is smaller, and it gives the same verdicts.
#[test]
fn the_reductions_shrink_two_threads() {
    let whole = explore(Config { reduce: false, ..BASE }).unwrap_or_else(|it| panic!("{it}"));
    let reduced = explore(BASE).unwrap_or_else(|it| panic!("{it}"));
    println!("two_threads: {} states whole, {} reduced", whole.states, reduced.states);
    assert!(reduced.states < whole.states, "{} reduced, {} whole", reduced.states, whole.states);
    assert!(reduced.freed_while_running > 0);
}

#[test]
fn more_threads_than_slots() {
    check(Config { slots: 1, ..BASE });
}

#[test]
fn a_thread_dies_holding_its_slot_and_its_id_comes_back() {
    check(Config { spawn: true, reuse: true, abrupt: true, slots: 2, calls: 1, quiesces: 1, ..BASE });
}

const THREE_THREADS_TWO_SLOTS: Config = Config { threads: 3, slots: 2, calls: 1, quiesces: 1, ..BASE };

#[test]
fn three_threads_two_slots() {
    check(THREE_THREADS_TWO_SLOTS);
}

/// The size of `three_threads_two_slots` with no limit, to set the limit by.
#[test]
#[ignore = "a measurement: unbounded memory and time, run in a VM"]
fn three_threads_two_slots_unbounded() {
    let explored = explore_within(THREE_THREADS_TWO_SLOTS, u32::MAX as usize - 1).unwrap_or_else(|it| panic!("{it}"));
    println!("{} states, {} freed while a thread runs", explored.states, explored.freed_while_running);
}

fn caught(mutation: Mutation) {
    let config = Config { mutation, ..BASE };
    match explore(config) {
        Ok(_) => panic!("{mutation:?} was not caught"),
        Err(error) => assert!(!error.contains("too many to explore"), "{error}"),
    }
}

#[test]
fn trusting_slots_uncounted_is_caught() {
    caught(Mutation::NoCount);
}

#[test]
fn moving_the_epoch_before_the_unlink_is_caught() {
    caught(Mutation::EarlyStart);
}

#[test]
fn ignoring_the_stack_is_caught() {
    caught(Mutation::NoStack);
}
