//! One names task: the scopes of one module (`ModuleScope`) or of one wait cycle of modules
//! (`Group`), entered from the top each time (bottom.md 2: restart, never park).

#![blaze::sans_io]

use core::fmt;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};

use crate::error::ResolveError;
use crate::facts::{Name, ScopeFacts};
use crate::group::Group;
use crate::read::{ExpandRequest, Read, Reply, Step};
use crate::resolver::{FillStats, Resolver};
use crate::scope::Resolved;

use super::answer::{Held, Lack, Note, Own, Records, answer};

/// The most steps one entry takes (rule 18): far over any module's or group's need.
pub const MOST_STEPS: usize = 1 << 20;

/// A record a task lacks.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Want {
    /// A module's finished scope.
    Scope(ItemKey),
    /// A macro call's expansion.
    Expansion(ExpandRequest),
}

/// What one entry of a task ends with.
#[derive(Debug)]
pub enum Outcome<'a, N> {
    /// One result per member, in the order of the sorted members.
    Done(AVec<'a, Resolved<'a, N>>),
    /// The records it lacks, sorted, without repeats.
    Need(AVec<'a, Want>),
}

/// What an expansion asked of the caller came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Expanded {
    /// The expansion is made: the facts now answer it.
    Made,
    /// It is not made: the task names it in its `Need`.
    Need,
}

/// The caller's side of an expansion: the in-memory scheduler expands at once ("expansions as
/// they are named", bottom.md 2); a driver that runs expansions as their own tasks says `Need`.
pub trait Expand {
    fn expand(&self, request: ExpandRequest) -> Expanded;
}

/// The work of one entry.
#[derive(Clone, Copy, Default, Debug)]
pub struct Tally {
    pub steps: u64,
    pub reads: u64,
    pub expansions: u64,
    pub failed_reads: u64,
    pub no_scope_reads: u64,
    pub unknown_reads: u64,
    pub fill: FillStats,
}

/// Why a task failed: a failure of the machine or of the protocol, never of the user's code
/// (that is a `ScopeError` inside the scope).
#[derive(Clone, Copy, Debug)]
pub enum TaskError {
    Resolve(ResolveError),
    /// A group reported a cycle through `outside`, a module that is no member and whose record
    /// is held: a cycle names only modules whose read answered `InProgress`.
    CycleOutside { module: ItemKey, outside: ItemKey },
    /// More than [`MOST_STEPS`] steps in one entry.
    Steps { module: ItemKey },
    /// The parents above `module` are deeper than the bound.
    TooDeep { module: ItemKey },
    /// A machine waits and names nothing to wait on.
    WaitsOnNothing { module: ItemKey },
    /// A task of no member.
    NoMembers,
    /// The in-memory fill was told an expansion is not made: it makes every one it is asked.
    Unexpanded { module: ItemKey, call: ItemKey },
    /// A wait cycle the fill could not widen into a group: no component of the wait graph
    /// waits only inside itself.
    Stalled { module: ItemKey },
}

impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskError::Resolve(error) => write!(f, "{error}"),
            TaskError::CycleOutside { module, outside } => {
                write!(f, "names task of module {:x}: a cycle names {:x}, held and no member", module.0, outside.0)
            }
            TaskError::Steps { module } => write!(f, "names task of module {:x}: more than {MOST_STEPS} steps", module.0),
            TaskError::TooDeep { module } => write!(f, "names task: the parents above module {:x} are too deep", module.0),
            TaskError::WaitsOnNothing { module } => write!(f, "names task of module {:x}: waits and names nothing", module.0),
            TaskError::NoMembers => write!(f, "names task with no module"),
            TaskError::Unexpanded { module, call } => {
                write!(f, "names task of module {:x}: the expansion of call {:x} was not made", module.0, call.0)
            }
            TaskError::Stalled { module } => write!(f, "names fill: module {:x} waits in a stall no group resolves", module.0),
        }
    }
}

impl From<ResolveError> for TaskError {
    fn from(error: ResolveError) -> TaskError {
        TaskError::Resolve(error)
    }
}

enum Machine<'a, N> {
    One(Resolver<'a, N>),
    Many(Group<'a, N>),
}

enum Stepped<'a, N> {
    Ask(AVec<'a, Read<N>>),
    Expand(AVec<'a, ExpandRequest>),
    Cycle(AVec<'a, ItemKey>),
    Done(AVec<'a, Resolved<'a, N>>, FillStats),
}

fn step<'a, N: Name, F: ScopeFacts<Name = N>>(
    machine: &mut Machine<'a, N>,
    facts: &F,
    replies: &[Reply<'_, N>],
    alloc: Alloc<'a>,
) -> Result<Stepped<'a, N>, ResolveError> {
    let mut out = match machine {
        Machine::One(r) => match r.step(facts, replies)? {
            Step::Ask(x) => Stepped::Ask(AVec::from_slice_in(x, alloc)),
            Step::Expand(x) => Stepped::Expand(AVec::from_slice_in(x, alloc)),
            Step::Cycle(x) => Stepped::Cycle(AVec::from_slice_in(x, alloc)),
            Step::Done(d) => {
                let mut all = AVec::new_in(alloc);
                all.push(d);
                Stepped::Done(all, FillStats::default())
            }
        },
        Machine::Many(g) => match g.step(facts, replies)? {
            Step::Ask(x) => Stepped::Ask(AVec::from_slice_in(x, alloc)),
            Step::Expand(x) => Stepped::Expand(AVec::from_slice_in(x, alloc)),
            Step::Cycle(x) => Stepped::Cycle(AVec::from_slice_in(x, alloc)),
            Step::Done(d) => Stepped::Done(d, FillStats::default()),
        },
    };
    if let Stepped::Done(_, stats) = &mut out {
        *stats = match machine {
            Machine::One(r) => r.stats(),
            Machine::Many(g) => g.stats(),
        };
    }
    Ok(out)
}

/// The scopes of `members` (one module, or a wait cycle; sorted, no repeats), from the facts
/// and the records held. A read of a member is answered `InProgress`; a read the records do
/// not answer is a `Need`. Pure: the same records give the same outcome.
pub fn solve<'a, N: Name, F: ScopeFacts<Name = N>, R: Records<N>, X: Expand>(
    members: &[ItemKey],
    facts: &F,
    records: &R,
    expand: &X,
    here: CrateItemKey,
    alloc: Alloc<'a>,
) -> Result<(Outcome<'a, N>, Tally), TaskError> {
    let first = *members.first().ok_or(TaskError::NoMembers)?;
    let is_member = |m: ItemKey| members.binary_search(&m).is_ok();
    let mut machine = if members.len() == 1 {
        Machine::One(Resolver::new_in(first, facts, alloc)?)
    } else {
        let mut all = AVec::with_capacity_in(members.len(), alloc);
        for &m in members {
            all.push(Resolver::new_in(m, facts, alloc)?);
        }
        Machine::Many(Group::new_in(all, alloc)?)
    };
    let mut tally = Tally::default();
    let mut owned: AVec<'a, (Read<N>, Own<'a, N>)> = AVec::new_in(alloc);
    let mut need: AVec<'a, Want> = AVec::new_in(alloc);
    let mut must_step = true;
    for _ in 0..MOST_STEPS {
        if owned.is_empty() && !must_step {
            return finish_need(need, first, tally);
        }
        must_step = false;
        let mut replies = AVec::with_capacity_in(owned.len(), alloc);
        for (read, own) in owned.iter() {
            replies.push(Reply { read: *read, answer: own.answer() });
        }
        tally.steps += 1;
        let stepped = step::<N, F>(&mut machine, facts, &replies, alloc)?;
        drop(replies);
        owned.clear();
        match stepped {
            Stepped::Ask(reads) => {
                must_step = reads.is_empty();
                for read in reads.iter() {
                    tally.reads += 1;
                    match answer(*read, records, here, alloc) {
                        Ok((own, note)) => {
                            match note {
                                Some(Note::FailedModule) => tally.failed_reads += 1,
                                Some(Note::NoScope) => tally.no_scope_reads += 1,
                                Some(Note::Unknown) => tally.unknown_reads += 1,
                                None => {}
                            }
                            records.asked(*read, &own);
                            owned.push((*read, own));
                        }
                        Err(Lack::Wait(module)) if is_member(module) => owned.push((*read, Own::InProgress(module))),
                        Err(Lack::Wait(module)) => need.push(Want::Scope(module)),
                        Err(Lack::TooDeep(module)) => return Err(TaskError::TooDeep { module }),
                    }
                }
            }
            Stepped::Expand(requests) => {
                let mut all_made = true;
                for request in requests.iter() {
                    tally.expansions += 1;
                    if expand.expand(*request) == Expanded::Need {
                        need.push(Want::Expansion(*request));
                        all_made = false;
                    }
                }
                if !all_made {
                    return finish_need(need, first, tally);
                }
                must_step = true;
            }
            Stepped::Cycle(modules) => {
                let mut outside = false;
                for &module in modules.iter() {
                    if is_member(module) {
                        continue;
                    }
                    outside = true;
                    match records.held(module) {
                        Held::Pending => need.push(Want::Scope(module)),
                        Held::Scope { .. } | Held::Variants { .. } | Held::Failed { .. } | Held::Unknown => {
                            return Err(TaskError::CycleOutside { module: first, outside: module });
                        }
                    }
                }
                if outside || !need.is_empty() {
                    return finish_need(need, first, tally);
                }
                // A lone module that waits on itself: it is a group of one.
                machine = match machine {
                    Machine::One(r) => {
                        let mut all = AVec::with_capacity_in(1, alloc);
                        all.push(r);
                        Machine::Many(Group::new_in(all, alloc)?)
                    }
                    Machine::Many(_) => return Err(TaskError::WaitsOnNothing { module: first }),
                };
                must_step = true;
            }
            Stepped::Done(all, stats) => {
                tally.fill = stats;
                return Ok((Outcome::Done(all), tally));
            }
        }
    }
    Err(TaskError::Steps { module: first })
}

fn finish_need<'a, N>(mut need: AVec<'a, Want>, first: ItemKey, tally: Tally) -> Result<(Outcome<'a, N>, Tally), TaskError> {
    if need.is_empty() {
        return Err(TaskError::WaitsOnNothing { module: first });
    }
    need.sort_unstable();
    need.dedup();
    Ok((Outcome::Need(need), tally))
}
