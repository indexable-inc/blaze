//! THE ONE ASK (D161 (b) and (c)): [`Asker::ask`] claims a need's key on the shared [`Board`];
//! the first asker makes the fact inline on its explicit, bounded work stack, a later one reads
//! it or parks on it, and a cycle of needs is [`AskError::Cycle`] naming every need on it.
//!
//! - **The work stack** is a vector of frames (need, claim, the maker's resumable state, the
//!   value handed to it), at most [`AskerOpts::most`] deep: a need met while making another is
//!   pushed and made first, then its value is handed to the frame below, which steps on from
//!   where it stopped. Nothing is made by recursion through the solver, so the caller's stack
//!   depth does not grow with the chain of needs, and no frame's work is done twice.
//! - **Parking** keeps everything: a need another task is making parks the asker on its slot
//!   ([`Asked::Parked`]) with its stack whole; the publisher hands its task id to the wake the
//!   caller passed, and [`Asker::resume`] reads the value and steps on.
//! - **Fuel**: each call takes at most `fuel` maker steps, then answers [`Asked::Yielded`] with
//!   its stack kept (the driver's scheduling point; a stop is a resume away).
//! - **Errors end every claim held**: an asker that stops with an [`AskError`] publishes each
//!   frame left on its stack as [`Given::Failed`]`(`[`ASK_REFUSED`]`)` and wakes its waiters,
//!   so no task waits on a claim nobody will end (rule 18); each of them reads the failure and
//!   fails its own frame (rule 3: no retry).
//! - **Cycles**: a need already on the asker's own stack is named from the top down to it; a
//!   wait the claim table finds would close a loop through other tasks is named by walking the
//!   table's chain of owners, each owner's frames read from the board's rows (top, then each
//!   frame's parent) down to the slot on the loop.
//!
//! A frame's claim is linear (blaze-core `Claim`): every path that pops one publishes it.

use core::alloc::Allocator;
use core::fmt;

use blaze_core::atomic::{AtomicU64, Ordering::SeqCst};
use blaze_core::claim_table::{At, Claim, ClaimError, ClaimTable, Claimed, TaskId, Waited};
use blaze_ids::ContentId;
use blaze_own::Vec;

use crate::counts::Counts;
use crate::need::{Detail, Fact, Need, Subject};

/// The top bit of a published word: set, the fact failed and the low bits are the failure's
/// code; clear, the word is the made record's ordinal.
pub const FAILED: u32 = 1 << 31;

/// The failure code an asker publishes on each claim it held when it stopped with an error.
pub const ASK_REFUSED: u32 = FAILED - 1;

/// How deep a work stack goes unless the caller says: the longest chain of facts one fact's
/// making waits on (a head on its supertraits' heads, ...). Past it is [`AskError::StackFull`].
pub const MOST_DEPTH: usize = 256;

/// What a need came to: the made record's ordinal, or a failure's code (a code's meaning is the
/// maker's; [`ASK_REFUSED`] is this crate's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Given {
    Made(u32),
    Failed(u32),
}

impl Given {
    /// The word published on the claim table.
    pub const fn word(self) -> u32 {
        match self {
            Given::Made(v) => v,
            Given::Failed(code) => FAILED | code,
        }
    }

    pub const fn of(word: u32) -> Given {
        if word & FAILED != 0 { Given::Failed(word & !FAILED) } else { Given::Made(word) }
    }
}

/// What one step of a maker did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The fact is made: its record's ordinal (below [`FAILED`]).
    Made(u32),
    /// The fact needs `Need` first: the frame is stepped again with its value.
    Needs(Need),
    /// The fact failed with this code (below [`ASK_REFUSED`]): its askers read the failure.
    Failed(u32),
}

/// Who makes facts: a resumable frame per need. `start` makes a need's frame; `step` runs it
/// until it is made, fails, or needs another fact, `given` being the value of the need it
/// stopped at last (`None` on its first step). A frame holds what it did so far, so a step
/// after a need goes on from there: nothing is redone.
pub trait Maker {
    type Frame;
    fn start(&mut self, need: Need) -> Self::Frame;
    fn step(&mut self, need: Need, frame: &mut Self::Frame, given: Option<Given>) -> Step;
}

/// What an ask or a resume came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Asked {
    /// The need asked: its value.
    Ready(Given),
    /// Parked on `on`, made by another task: the stack is kept, and the wake passed to the
    /// publisher's call hands this task back for [`Asker::resume`].
    Parked { on: Need },
    /// The fuel ran out: the stack is kept for [`Asker::resume`].
    Yielded,
}

/// Why an ask stopped (rule 19: each names the need). Every claim the asker held was published
/// as [`ASK_REFUSED`] before it was returned.
pub enum AskError<A: Allocator> {
    /// A cycle of needs: every need on it, each needed by the one after it.
    Cycle { needs: Vec<Need, A> },
    /// The need's owner unwound before it published (blaze-core claim table, rule 20).
    Abandoned { need: Need },
    /// The work stack is at its bound with `need` still to make.
    StackFull { need: Need, depth: usize },
    /// A maker's step answered a word out of its range.
    Word { need: Need, step: Step },
    /// The claim table refused.
    Table { need: Need, why: ClaimError },
    /// An ask while an earlier one is not done (parked or yielded): resume it first.
    Busy { need: Need },
    /// A resume with nothing asked.
    Idle,
}

impl<A: Allocator> fmt::Debug for AskError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl<A: Allocator> fmt::Display for AskError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AskError::Cycle { needs } => {
                f.write_str("a cycle of needs (D161 (c)):")?;
                for n in needs.iter() {
                    write!(f, " {n};")?;
                }
                Ok(())
            }
            AskError::Abandoned { need } => write!(f, "{need}: its maker unwound before it published (rule 20)"),
            AskError::StackFull { need, depth } => write!(f, "{need}: the work stack is {depth} deep, its bound (rule 18)"),
            AskError::Word { need, step } => write!(f, "{need}: the maker answered {step:?}, out of the word's range"),
            AskError::Table { need, why } => write!(f, "{need}: {why}"),
            AskError::Busy { need } => write!(f, "{need} asked while an earlier ask is parked or yielded"),
            AskError::Idle => f.write_str("a resume with nothing asked"),
        }
    }
}

/// Which bug a test plants (decision D165: a parameter of the code under test, not a
/// thread-local). [`Plant::Clean`] in every build that is not a control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plant {
    Clean,
    /// A claim found done is made again, without a claim.
    MakeWhatIsDone,
    /// A claim found running is read as made (ordinal 0) without a wait.
    ReadRunning,
    /// A cycle is named by its last need alone.
    CycleNamesOne,
}

/// The words of a need's name on the board: fact, subject and detail, little-endian, padded.
const NAME_WORDS: usize = 7;

/// A slot's row: the need it holds (written once, by its owner, right after the claim), and the
/// slot of the frame below it on its owner's stack.
struct SlotRow {
    name: [AtomicU64; NAME_WORDS],
    /// 1 once `name` and `parent` are written.
    sealed: AtomicU64,
    /// The parent frame's slot plus one; 0 at the stack's bottom.
    parent: AtomicU64,
}

/// The claim table every asker of one request shares, and the rows a cycle is named by.
pub struct Board<A: Allocator> {
    table: ClaimTable<A>,
    slots: Vec<SlotRow, A>,
    /// Each task's top frame's slot plus one; 0 with an empty stack.
    tops: Vec<AtomicU64, A>,
}

fn name_words(need: &Need) -> [u64; NAME_WORDS] {
    let mut b = [0u8; NAME_WORDS * 8];
    b[0] = need.fact as u8;
    b[1..33].copy_from_slice(&need.subject.0.0);
    b[33..49].copy_from_slice(&need.detail.0);
    core::array::from_fn(|i| {
        let w = &b[i * 8..i * 8 + 8];
        u64::from_le_bytes([w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7]])
    })
}

fn need_of(words: &[u64; NAME_WORDS]) -> Option<Need> {
    let mut b = [0u8; NAME_WORDS * 8];
    for (i, w) in words.iter().enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    let fact = Fact::from_u8(b[0])?;
    let mut subject = [0u8; 32];
    subject.copy_from_slice(&b[1..33]);
    let mut detail = [0u8; 16];
    detail.copy_from_slice(&b[33..49]);
    Some(Need { fact, subject: Subject(ContentId(subject)), detail: Detail(detail) })
}

impl<A: Allocator + Clone> Board<A> {
    /// A board of at least `slots` needs for task ids below `tasks`, its room taken from
    /// `alloc` now and never again.
    pub fn new_in(slots: u32, tasks: u32, alloc: A) -> Result<Board<A>, ClaimError> {
        let table = ClaimTable::new_in(slots, tasks, alloc.clone())?;
        let cap = table.capacity() as usize;
        let mut rows = Vec::new_in(alloc.clone());
        rows.try_reserve(cap).map_err(|_| ClaimError::Memory { slots, tasks })?;
        for _ in 0..cap {
            rows.push(SlotRow { name: core::array::from_fn(|_| AtomicU64::new(0)), sealed: AtomicU64::new(0), parent: AtomicU64::new(0) });
        }
        let mut tops = Vec::new_in(alloc);
        tops.try_reserve(tasks as usize).map_err(|_| ClaimError::Memory { slots, tasks })?;
        for _ in 0..tasks {
            tops.push(AtomicU64::new(0));
        }
        Ok(Board { table, slots: rows, tops })
    }
}

impl<A: Allocator> Board<A> {
    /// The claim table: a driver's own reads (a test's observer) and its orphans after a panic.
    pub fn table(&self) -> &ClaimTable<A> {
        &self.table
    }

    /// Names slot `at` (its owner, once, before it can be on any cycle it names).
    fn name(&self, at: u32, need: &Need, parent: Option<u32>) {
        let Some(row) = self.slots.get(at as usize) else { return };
        for (cell, w) in row.name.iter().zip(name_words(need)) {
            cell.store(w, SeqCst);
        }
        row.parent.store(parent.map_or(0, |p| u64::from(p) + 1), SeqCst);
        row.sealed.store(1, SeqCst);
    }

    fn named(&self, at: u32) -> Option<(Need, Option<u32>)> {
        let row = self.slots.get(at as usize)?;
        if row.sealed.load(SeqCst) == 0 {
            return None;
        }
        let words: [u64; NAME_WORDS] = core::array::from_fn(|i| row.name[i].load(SeqCst));
        let parent = row.parent.load(SeqCst);
        Some((need_of(&words)?, parent.checked_sub(1).map(|p| p as u32)))
    }

    fn set_top(&self, task: TaskId, top: Option<u32>) {
        if let Some(row) = self.tops.get(task.0 as usize) {
            row.store(top.map_or(0, |t| u64::from(t) + 1), SeqCst);
        }
    }

    fn top(&self, task: TaskId) -> Option<u32> {
        self.tops.get(task.0 as usize)?.load(SeqCst).checked_sub(1).map(|t| t as u32)
    }
}

/// How an asker is made: its board, its task id, and its allocator (rule 75).
pub struct AskerOpts<'t, A: Allocator> {
    pub board: &'t Board<A>,
    pub me: TaskId,
    pub alloc: A,
    pub most: usize = MOST_DEPTH,
    pub plant: Plant = Plant::Clean,
}

struct Frame<'t, A: Allocator, F> {
    need: Need,
    /// `None` only under [`Plant::MakeWhatIsDone`].
    claim: Option<Claim<'t, A>>,
    state: F,
    given: Option<Given>,
}

enum State {
    Idle,
    /// Asked, stepping (or yielded).
    Busy,
    /// Parked on `on`'s slot.
    Parked { on: Need },
}

/// What meeting a need came to.
enum Met {
    Given(Given),
    Pushed,
    Parked,
}

/// One task's asker: its work stack and counters. See the module.
pub struct Asker<'t, A: Allocator + Clone, F> {
    board: &'t Board<A>,
    me: TaskId,
    most: usize,
    plant: Plant,
    stack: Vec<Frame<'t, A, F>, A>,
    state: State,
    counts: Counts,
    alloc: A,
}

impl<'t, A: Allocator + Clone, F> Asker<'t, A, F> {
    pub fn new(opts: AskerOpts<'t, A>) -> Asker<'t, A, F> {
        let AskerOpts { board, me, alloc, most, plant } = opts;
        Asker { board, me, most, plant, stack: Vec::new_in(alloc.clone()), state: State::Idle, counts: Counts::default(), alloc }
    }

    pub fn counts(&self) -> &Counts {
        &self.counts
    }

    /// Asks `need`: its value, or parked, or yielded after `fuel` steps. `wake` takes each task
    /// a publish of this asker hands back.
    pub fn ask<M: Maker<Frame = F>>(&mut self, maker: &mut M, need: Need, fuel: u32, wake: &mut dyn FnMut(TaskId)) -> Result<Asked, AskError<A>> {
        if !matches!(self.state, State::Idle) {
            return Err(AskError::Busy { need });
        }
        self.counts.asked[need.fact.index()] += 1;
        self.state = State::Busy;
        let met = self.meet(maker, need, wake);
        let out = match met {
            Ok(Met::Given(g)) => {
                self.state = State::Idle;
                Ok(Asked::Ready(g))
            }
            Ok(Met::Parked) => {
                self.state = State::Parked { on: need };
                Ok(Asked::Parked { on: need })
            }
            Ok(Met::Pushed) => self.drive(maker, fuel, wake),
            Err(e) => Err(e),
        };
        self.ended(out, wake)
    }

    /// Goes on after [`Asked::Parked`] (once woken) or [`Asked::Yielded`].
    pub fn resume<M: Maker<Frame = F>>(&mut self, maker: &mut M, fuel: u32, wake: &mut dyn FnMut(TaskId)) -> Result<Asked, AskError<A>> {
        let out = match core::mem::replace(&mut self.state, State::Busy) {
            State::Idle => {
                self.state = State::Idle;
                return Err(AskError::Idle);
            }
            State::Busy => self.drive(maker, fuel, wake),
            State::Parked { on } => match self.meet(maker, on, wake) {
                Ok(Met::Given(g)) => match self.stack.last_mut() {
                    Some(top) => {
                        top.given = Some(g);
                        self.drive(maker, fuel, wake)
                    }
                    None => {
                        self.state = State::Idle;
                        Ok(Asked::Ready(g))
                    }
                },
                Ok(Met::Parked) => {
                    self.state = State::Parked { on };
                    Ok(Asked::Parked { on })
                }
                Ok(Met::Pushed) => self.drive(maker, fuel, wake),
                Err(e) => Err(e),
            },
        };
        self.ended(out, wake)
    }

    /// An error ends every claim held (see the module).
    fn ended(&mut self, out: Result<Asked, AskError<A>>, wake: &mut dyn FnMut(TaskId)) -> Result<Asked, AskError<A>> {
        if out.is_err() {
            while let Some(frame) = self.stack.pop() {
                if let Some(claim) = frame.claim {
                    claim.publish(Given::Failed(ASK_REFUSED).word()).for_each(&mut *wake);
                }
            }
            self.board.set_top(self.me, None);
            self.state = State::Idle;
        }
        out
    }

    /// The top frame's slot, when it holds a claim.
    fn top_slot(&self) -> Option<u32> {
        self.stack.last().and_then(|f| f.claim.as_ref()).map(|c| c.at().index())
    }

    /// Steps the top frame until the stack empties, the asker parks, or the fuel is spent.
    fn drive<M: Maker<Frame = F>>(&mut self, maker: &mut M, fuel: u32, wake: &mut dyn FnMut(TaskId)) -> Result<Asked, AskError<A>> {
        for _ in 0..fuel {
            let Some(top) = self.stack.last_mut() else {
                self.state = State::Idle;
                return Err(AskError::Idle);
            };
            let need = top.need;
            let given = top.given.take();
            let step = maker.step(need, &mut top.state, given);
            self.counts.steps += 1;
            let word = match step {
                Step::Made(v) if v < FAILED => Given::Made(v),
                Step::Failed(code) if code < ASK_REFUSED => Given::Failed(code),
                Step::Needs(next) => {
                    self.counts.needed[next.fact.index()] += 1;
                    match self.meet(maker, next, wake)? {
                        Met::Given(g) => {
                            if let Some(top) = self.stack.last_mut() {
                                top.given = Some(g);
                            }
                        }
                        Met::Pushed => {}
                        Met::Parked => {
                            self.state = State::Parked { on: next };
                            return Ok(Asked::Parked { on: next });
                        }
                    }
                    continue;
                }
                _ => return Err(AskError::Word { need, step }),
            };
            let Some(frame) = self.stack.pop() else { return Err(AskError::Idle) };
            match word {
                Given::Made(_) => self.counts.made[need.fact.index()] += 1,
                Given::Failed(_) => self.counts.failed += 1,
            }
            if let Some(claim) = frame.claim {
                claim.publish(word.word()).for_each(&mut *wake);
            }
            self.board.set_top(self.me, self.top_slot());
            match self.stack.last_mut() {
                Some(below) => below.given = Some(word),
                None => {
                    self.state = State::Idle;
                    return Ok(Asked::Ready(word));
                }
            }
        }
        Ok(Asked::Yielded)
    }

    /// Meets `need`: read, pushed to be made here, or parked on.
    fn meet<M: Maker<Frame = F>>(&mut self, maker: &mut M, need: Need, wake: &mut dyn FnMut(TaskId)) -> Result<Met, AskError<A>> {
        if let Some(pos) = self.stack.iter().position(|f| f.need == need) {
            return Err(self.local_cycle(pos));
        }
        let board: &'t Board<A> = self.board;
        let claimed = board.table.claim(need.key().claim_key(), self.me).map_err(|why| AskError::Table { need, why })?;
        match claimed {
            Claimed::Mine(claim) => self.push(maker, need, Some(claim), wake),
            Claimed::Done { value, .. } => {
                if self.plant == Plant::MakeWhatIsDone {
                    return self.push(maker, need, None, wake);
                }
                self.counts.read += 1;
                Ok(Met::Given(Given::of(value)))
            }
            Claimed::Running { at, owner } => {
                if self.plant == Plant::ReadRunning {
                    return Ok(Met::Given(Given::Made(0)));
                }
                if owner == self.me {
                    // Every slot this task owns is on its stack, and the stack was searched:
                    // only a frame made without its claim (a plant) gets here.
                    let mut needs = Vec::new_in(self.alloc.clone());
                    needs.push(need);
                    return Err(self.planted_names(needs));
                }
                match board.table.wait(at, self.me).map_err(|why| AskError::Table { need, why })? {
                    Waited::Parked => {
                        self.counts.parked += 1;
                        Ok(Met::Parked)
                    }
                    Waited::Ready(v) => {
                        self.counts.read += 1;
                        Ok(Met::Given(Given::of(v)))
                    }
                    Waited::Abandoned { .. } => Err(AskError::Abandoned { need }),
                    Waited::Cycle { at } => Err(self.chain_cycle(at, need)),
                }
            }
            Claimed::Abandoned { .. } => Err(AskError::Abandoned { need }),
        }
    }

    /// Pushes a frame for `need` (its claim, when it has one, named on the board first).
    fn push<M: Maker<Frame = F>>(&mut self, maker: &mut M, need: Need, claim: Option<Claim<'t, A>>, wake: &mut dyn FnMut(TaskId)) -> Result<Met, AskError<A>> {
        if self.stack.len() >= self.most {
            if let Some(claim) = claim {
                claim.publish(Given::Failed(ASK_REFUSED).word()).for_each(&mut *wake);
            }
            return Err(AskError::StackFull { need, depth: self.stack.len() });
        }
        if let Some(c) = claim.as_ref() {
            self.board.name(c.at().index(), &need, self.top_slot());
            self.board.set_top(self.me, Some(c.at().index()));
        }
        let state = maker.start(need);
        self.stack.push(Frame { need, claim, state, given: None });
        self.counts.deepest = self.counts.deepest.max(self.stack.len() as u32);
        Ok(Met::Pushed)
    }

    /// A need on this asker's own stack at `pos`: the frames from the top down to it.
    fn local_cycle(&self, pos: usize) -> AskError<A> {
        let mut needs = Vec::new_in(self.alloc.clone());
        for f in self.stack[pos..].iter().rev() {
            needs.push(f.need);
        }
        self.planted_names(needs)
    }

    /// A wait on `at` (for `need`) the table says closes a cycle: each owner on the chain's
    /// frames, from its top down to the chain's slot (bounded by the board's slots).
    fn chain_cycle(&self, at: At, need: Need) -> AskError<A> {
        let mut needs = Vec::new_in(self.alloc.clone());
        let most = self.board.slots.len();
        for (slot, owner) in self.board.table.chain(at) {
            let mut cur = if owner == self.me { self.top_slot() } else { self.board.top(owner) };
            let mut left = most;
            while let Some(c) = cur {
                if left == 0 {
                    break;
                }
                left -= 1;
                let Some((n, parent)) = self.board.named(c) else { break };
                needs.push(n);
                if c == slot.index() {
                    break;
                }
                cur = parent;
            }
        }
        if needs.is_empty() {
            needs.push(need);
        }
        self.planted_names(needs)
    }

    fn planted_names(&self, mut needs: Vec<Need, A>) -> AskError<A> {
        if self.plant == Plant::CycleNamesOne {
            while needs.len() > 1 {
                needs.pop();
            }
        }
        AskError::Cycle { needs }
    }
}
