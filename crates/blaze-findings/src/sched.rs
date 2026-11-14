//! The scheduler of lint work: a sans-IO state machine of one workspace that decides what is
//! computed before the answer, right after it, and in the background.
//!
//! **Inputs.** A command ([`Command`]): its snapshot, the items that moved since the last one,
//! the works it wants (every (pass, subject) of the view's universe whose record the host did
//! not find in the store under its key), the view, the wait, and the clock. Then the outcome of
//! each work it ran ([`Scheduler::done`]) and the clock ([`Scheduler::advance`]).
//!
//! **Effects.** [`Effect::Run`] (a work, its class, its QoS, local or remote),
//! [`Effect::Cancel`], [`Effect::Reply`].
//!
//! **The rules.**
//!
//! - A work's class is its pass's under the command's view ([`crate::pass::class_of`]); a gate
//!   lint's pass is always [`Class::Answer`], and a fold's rows pass is promoted with its fold.
//! - Answer work starts first, on every core. Cheap work starts when no answer work is queued,
//!   on the performance cores. Deferred work starts only when no answer or cheap work is queued
//!   or running, on a quarter of the cores at utility QoS (remote, when the host has an
//!   executor that leaves the machine): the edit loop never shares a core with it.
//! - A newer command cancels exactly the works in flight that read an item that moved, or that
//!   the new command no longer wants (its view dropped the pass), and what depends on those. It
//!   keeps the rest and adopts them: a work wanted again by (pass, subject, key) is the same
//!   work, never a second one, so nothing is computed twice (rules 74 and 77). A file change
//!   alone never starts work: only a command does (rule 80).
//! - The reply waits for the answer class always; for the cheap class with [`Wait::Cheap`],
//!   until it drains or its budget after the answer passes; for everything with [`Wait::All`],
//!   until it drains or the budget passes. What it did not reach is `pending`, by lint and item.
//!   Work goes on after the reply; its findings are in the store when they are done, and the next
//!   command's reply shows them.
//! - A pass whose measured mean is over its class's bound is reported in the reply (rule 16).
//!
//! Every loop here is over the live works or the wants: bounded by them.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::fmt;

use blaze_ids::{ContentId, Hasher};
use blaze_keyed::kind::{Body, Key, Kind, ToolId};
use blaze_keyed::machine::{Machine, Qos};
use blaze_keyed::record::StableId;

use crate::kinds::{BodyFindings, ItemRows, PerBody};
use crate::pass::{self, Budgets, Class, Measured, Over, PassDecl, PassId, Slow};
use crate::view::View;
use crate::wait::{Failed, Pending, Reply, Wait};
use crate::{Plant, planted};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct WorkId(pub u64);

/// What a work is about: one item, or a pass's workspace fold.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Subject {
    Item(StableId),
    Fold,
}

impl Subject {
    pub fn item(&self) -> Option<StableId> {
        match self {
            Subject::Item(item) => Some(*item),
            Subject::Fold => None,
        }
    }
}

/// One work a command wants.
#[derive(Clone, Copy, Debug)]
pub struct Want<'a> {
    pub pass: PassId,
    pub subject: Subject,
    /// The record's key when the host knows it before the work (per body, per item rows, a fold
    /// leaf); `None` for an analysis whose reads are made by work of this command.
    pub key: Option<ContentId>,
    /// The items the work reads, its subject included, sorted. A fold reads every contributor.
    pub reads: &'a [StableId],
}

/// A work's identity: two wants equal in it are one work.
type Identity = (PassId, Subject, Option<ContentId>);

#[derive(Clone, Copy, Debug)]
pub struct Command<'a> {
    pub snapshot: u64,
    /// Items whose key moved since the last command, sorted.
    pub moved: &'a [StableId],
    pub wants: &'a [Want<'a>],
    pub view: &'a View,
    pub wait: Wait,
    pub now_ns: u64,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Effect {
    Run { id: WorkId, pass: PassId, subject: Subject, key: Option<ContentId>, class: Class, qos: Option<Qos>, remote: bool },
    Cancel { id: WorkId },
    Reply(Reply),
}

/// How a work ended, as the host reports it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Made and filed: the pass ran.
    Computed,
    /// Found in the store under its key when the host came to run it: nothing ran.
    Hit,
    /// Stopped after a [`Effect::Cancel`].
    Cancelled,
    /// The pass panicked or refused: reported, never retried.
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SchedError {
    UnknownPass { pass: PassId },
    /// The pass is not computed per body, so it has no per-body key.
    NotPerBody { pass: PassId },
    UnknownWork { id: WorkId },
    /// An outcome for a work that was queued, not running.
    NotRunning { id: WorkId },
    /// [`Outcome::Cancelled`] for a work that was not asked to stop.
    CancelledUnasked { id: WorkId },
    /// An input list that must be sorted was not.
    Unsorted { what: &'static str },
}

impl fmt::Display for SchedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchedError::UnknownPass { pass } => write!(f, "lint scheduler: pass {:#x} is not declared", pass.0),
            SchedError::NotPerBody { pass } => write!(f, "lint scheduler: pass {:#x} is not computed per body", pass.0),
            SchedError::UnknownWork { id } => write!(f, "lint scheduler: work {} is not live", id.0),
            SchedError::NotRunning { id } => write!(f, "lint scheduler: work {} ended before it was started", id.0),
            SchedError::CancelledUnasked { id } => write!(f, "lint scheduler: work {} was cancelled without a cancel", id.0),
            SchedError::Unsorted { what } => write!(f, "lint scheduler: {what} are not sorted"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Queued,
    Running,
    /// Asked to stop; it holds its worker until the host reports.
    Cancelling,
}

#[derive(Clone, Debug)]
struct Live {
    id: WorkId,
    pass: PassId,
    subject: Subject,
    key: Option<ContentId>,
    reads: Vec<StableId, Global>,
    /// Works that must end first (a fold's rows).
    deps: Vec<WorkId, Global>,
    class: Class,
    /// The latest command that wants it.
    owner: u64,
    state: State,
}

impl Live {
    fn identity(&self) -> Identity {
        (self.pass, self.subject, self.key)
    }
}

/// A work in flight, as [`Scheduler::in_flight`] shows it.
#[derive(Clone, Copy, Debug)]
pub struct InFlight<'a> {
    pub id: WorkId,
    pub pass: PassId,
    pub subject: Subject,
    pub key: Option<ContentId>,
    pub reads: &'a [StableId],
    pub deps: &'a [WorkId],
    pub class: Class,
    pub running: bool,
}

#[derive(Clone, Debug)]
struct Current {
    snapshot: u64,
    wait: Wait,
    view: View,
    started_ns: u64,
    answer_done_ns: Option<u64>,
    done: u32,
    failed: Vec<Failed, Global>,
    slow: Vec<Slow, Global>,
    replied: bool,
}

pub struct Scheduler {
    passes: &'static [PassDecl],
    tool: ToolId,
    machine: Machine,
    budgets: Budgets,
    /// Whether deferred work may leave the machine.
    remote: bool,
    next: u64,
    /// Sorted by id.
    live: Vec<Live, Global>,
    measured: Vec<(PassId, Measured), Global>,
    current: Option<Current>,
    last_cancelled: Vec<WorkId, Global>,
}

fn intersects(a: &[StableId], b: &[StableId]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            core::cmp::Ordering::Equal => return true,
            core::cmp::Ordering::Less => i += 1,
            core::cmp::Ordering::Greater => j += 1,
        }
    }
    false
}

fn sorted<T: Ord>(items: &[T]) -> bool {
    items.windows(2).all(|pair| pair[0] <= pair[1])
}

impl Scheduler {
    pub fn new(passes: &'static [PassDecl], tool: ToolId, machine: Machine, remote: bool) -> Scheduler {
        Scheduler {
            passes,
            tool,
            machine,
            budgets: Budgets::of(&machine),
            remote,
            next: 0,
            live: Vec::new_in(Global),
            measured: Vec::new_in(Global),
            current: None,
            last_cancelled: Vec::new_in(Global),
        }
    }

    fn decl(&self, pass: PassId) -> Result<&'static PassDecl, SchedError> {
        self.passes.iter().find(|decl| decl.id() == pass).ok_or(SchedError::UnknownPass { pass })
    }

    /// The key of a per-body pass's record for a body: what the host looks up before it wants
    /// the work. No level is in it.
    pub fn key_of(&self, pass: PassId, body: Key<Body>, beyond: &[ContentId], view: &View) -> Result<ContentId, SchedError> {
        let decl = self.decl(pass)?;
        let inputs = PerBody { tool: self.tool, body, pass: decl.reference().keyed(), beyond };
        let key = match decl.over {
            Over::Body => BodyFindings::key(&inputs).id(),
            Over::Rows => ItemRows::key(&inputs).id(),
            Over::Fold { .. } | Over::Analysis => return Err(SchedError::NotPerBody { pass }),
        };
        match planted(Plant::KeyCoversLevels) {
            true => Ok(Hasher::new("blaze-findings planted").id(&key).id(&view.digest()).finish()),
            false => Ok(key),
        }
    }

    /// Each pass's class under `view`, with a fold's rows pass promoted to its fold's class.
    fn classes(&self, view: &View) -> Vec<(PassId, Class), Global> {
        let mut classes = Vec::with_capacity_in(self.passes.len(), Global);
        for decl in self.passes {
            classes.push((decl.id(), pass::class_of(decl, view)));
        }
        for decl in self.passes {
            if let Over::Fold { rows } = decl.over {
                let fold = pass::class_of(decl, view);
                for entry in classes.iter_mut().filter(|(pass, _)| *pass == rows) {
                    entry.1 = entry.1.min(fold);
                }
            }
        }
        classes
    }

    /// The works in flight, in id order: what a newer command may cancel.
    pub fn in_flight(&self) -> impl Iterator<Item = InFlight<'_>> + '_ {
        self.live.iter().filter(|live| live.state != State::Cancelling).map(|live| InFlight {
            id: live.id,
            pass: live.pass,
            subject: live.subject,
            key: live.key,
            reads: &live.reads,
            deps: &live.deps,
            class: live.class,
            running: live.state == State::Running,
        })
    }

    /// The works the last command cancelled, queued or running, sorted.
    pub fn last_cancelled(&self) -> &[WorkId] {
        &self.last_cancelled
    }

    pub fn command(&mut self, command: &Command<'_>) -> Result<Vec<Effect, Global>, SchedError> {
        if !sorted(command.moved) {
            return Err(SchedError::Unsorted { what: "the moved items" });
        }
        for want in command.wants {
            self.decl(want.pass)?;
            if !sorted(want.reads) {
                return Err(SchedError::Unsorted { what: "a want's reads" });
            }
        }
        let mut effects = Vec::new_in(Global);
        if self.current.as_ref().is_some_and(|current| !current.replied) {
            let reply = self.reply(true);
            effects.push(Effect::Reply(reply));
        }
        let classes = self.classes(command.view);
        let class_of = |pass: PassId| classes.iter().find(|(p, _)| *p == pass).map_or(Class::Deferred, |(_, c)| *c);

        let mut wanted = Vec::with_capacity_in(command.wants.len(), Global);
        for want in command.wants {
            wanted.push((want.pass, want.subject, want.key));
        }
        wanted.sort_unstable();

        // Cancel what reads a moved item or is no longer wanted, then what depends on those.
        self.last_cancelled.clear();
        for live in &mut self.live {
            if live.state == State::Cancelling {
                continue;
            }
            let moved = intersects(&live.reads, command.moved);
            let unwanted = wanted.binary_search(&live.identity()).is_err();
            let cancel = match live.class == Class::Deferred {
                true if planted(Plant::CancelsAll) => true,
                true if planted(Plant::KeepsMoved) => unwanted && !moved,
                _ => moved || unwanted,
            };
            if cancel {
                self.last_cancelled.push(live.id);
            }
        }
        loop {
            let before = self.last_cancelled.len();
            for live in &self.live {
                let cancelled = self.last_cancelled.binary_search(&live.id).is_ok();
                if !cancelled
                    && live.state != State::Cancelling
                    && live.deps.iter().any(|dep| self.last_cancelled.binary_search(dep).is_ok())
                {
                    let at = self.last_cancelled.partition_point(|id| *id < live.id);
                    self.last_cancelled.insert(at, live.id);
                    break;
                }
            }
            if self.last_cancelled.len() == before {
                break;
            }
        }
        let cancelled = &self.last_cancelled;
        self.live.retain_mut(|live| {
            if cancelled.binary_search(&live.id).is_err() {
                return true;
            }
            match live.state {
                State::Queued => false,
                State::Running | State::Cancelling => {
                    if live.state == State::Running {
                        effects.push(Effect::Cancel { id: live.id });
                    }
                    live.state = State::Cancelling;
                    true
                }
            }
        });

        self.current = Some(Current {
            snapshot: command.snapshot,
            wait: command.wait,
            view: command.view.clone(),
            started_ns: command.now_ns,
            answer_done_ns: None,
            done: 0,
            failed: Vec::new_in(Global),
            slow: Vec::new_in(Global),
            replied: false,
        });

        // Adopt what is wanted again; queue the rest.
        let mut rows_works = Vec::new_in(Global);
        let mut folds = Vec::new_in(Global);
        for want in command.wants {
            let decl = self.decl(want.pass)?;
            let class = class_of(want.pass);
            let identity = (want.pass, want.subject, want.key);
            let adopted = match planted(Plant::ReissuesKeptWork) {
                true => None,
                false => self.live.iter_mut().find(|live| live.state != State::Cancelling && live.identity() == identity),
            };
            let id = match adopted {
                Some(live) => {
                    live.owner = command.snapshot;
                    live.class = class;
                    live.id
                }
                None => {
                    let id = WorkId(self.next);
                    self.next += 1;
                    let mut reads = Vec::with_capacity_in(want.reads.len(), Global);
                    reads.extend_from_slice(want.reads);
                    self.live.push(Live {
                        id,
                        pass: want.pass,
                        subject: want.subject,
                        key: want.key,
                        reads,
                        deps: Vec::new_in(Global),
                        class,
                        owner: command.snapshot,
                        state: State::Queued,
                    });
                    if let Over::Fold { rows } = decl.over {
                        folds.push((id, rows));
                    }
                    id
                }
            };
            if decl.over == Over::Rows {
                rows_works.push((want.pass, id));
            }
        }
        for (fold, rows) in folds {
            let at = self.live.binary_search_by_key(&fold, |live| live.id).map_err(|_| SchedError::UnknownWork { id: fold })?;
            for &(pass, id) in &rows_works {
                if pass == rows {
                    self.live[at].deps.push(id);
                }
            }
        }
        self.pump(&mut effects);
        self.maybe_reply(command.now_ns, &mut effects);
        Ok(effects)
    }

    /// Starts what the classes' budgets allow, in class order, oldest first.
    fn pump(&mut self, effects: &mut Vec<Effect, Global>) {
        let busy = |live: &Live, class: Class| live.class == class && live.state != State::Queued;
        for class in [Class::Answer, Class::Cheap, Class::Deferred] {
            let interactive = self.live.iter().filter(|l| busy(l, Class::Answer) || busy(l, Class::Cheap)).count() as u32;
            let mut running = self.live.iter().filter(|l| busy(l, class)).count() as u32;
            let mut interactive = interactive;
            let allowed = match class {
                Class::Answer => true,
                Class::Cheap => !self.live.iter().any(|l| l.class == Class::Answer && l.state == State::Queued),
                Class::Deferred => !self.live.iter().any(|l| l.class != Class::Deferred),
            };
            if !allowed {
                continue;
            }
            let cap = self.budgets.workers(class);
            for live in &mut self.live {
                if live.class != class || live.state != State::Queued || !live.deps.is_empty() {
                    continue;
                }
                let room = match class {
                    Class::Answer => interactive < self.budgets.answer_workers,
                    Class::Cheap => running < cap && interactive < self.budgets.answer_workers,
                    Class::Deferred => running < cap,
                };
                if !room {
                    break;
                }
                live.state = State::Running;
                running += 1;
                if class != Class::Deferred {
                    interactive += 1;
                }
                effects.push(Effect::Run {
                    id: live.id,
                    pass: live.pass,
                    subject: live.subject,
                    key: live.key,
                    class,
                    qos: self.machine.qos_of(pass::qos(class)),
                    remote: self.remote && class == Class::Deferred,
                });
            }
        }
    }

    fn maybe_reply(&mut self, now_ns: u64, effects: &mut Vec<Effect, Global>) {
        let Some(current) = &self.current else { return };
        if current.replied {
            return;
        }
        let owned = |class: Option<Class>| {
            self.live.iter().any(|live| {
                live.owner == current.snapshot && live.state != State::Cancelling && class.is_none_or(|class| live.class == class)
            })
        };
        if owned(Some(Class::Answer)) {
            return;
        }
        let answer_done_ns = current.answer_done_ns.unwrap_or(now_ns);
        let ready = match current.wait {
            Wait::Answer => true,
            Wait::Cheap => !owned(Some(Class::Cheap)) || now_ns >= answer_done_ns.saturating_add(self.budgets.cheap_ns),
            Wait::All { budget_ns } => !owned(None) || now_ns >= current.started_ns.saturating_add(budget_ns),
        };
        if let Some(current) = &mut self.current {
            current.answer_done_ns = Some(answer_done_ns);
        }
        if ready {
            let reply = self.reply(false);
            effects.push(Effect::Reply(reply));
            if let Some(current) = &mut self.current {
                current.replied = true;
            }
        }
    }

    fn estimate(&self, decl: &PassDecl) -> u64 {
        self.measured
            .iter()
            .find(|(pass, _)| *pass == decl.id())
            .and_then(|(_, measured)| measured.mean_ns())
            .unwrap_or(decl.expected_ns)
    }

    /// The current command's reply as of now.
    fn reply(&self, superseded: bool) -> Reply {
        let mut pending = Vec::new_in(Global);
        let Some(current) = &self.current else {
            return Reply { snapshot: 0, superseded, done: 0, pending, slow: Vec::new_in(Global), failed: Vec::new_in(Global) };
        };
        for live in &self.live {
            if live.owner != current.snapshot || live.state == State::Cancelling {
                continue;
            }
            if live.class == Class::Deferred && planted(Plant::PendingOmitsDeferred) {
                continue;
            }
            let Ok(decl) = self.decl(live.pass) else { continue };
            let estimate_ns = self.estimate(decl);
            for &lint in decl.lints {
                if current.view.in_universe(lint) {
                    pending.push(Pending { lint, item: live.subject.item(), class: live.class, estimate_ns });
                }
            }
        }
        Reply {
            snapshot: current.snapshot,
            superseded,
            done: current.done,
            pending,
            slow: current.slow.clone(),
            failed: current.failed.clone(),
        }
    }

    /// The host's report of a work's end, with the time its pass took.
    pub fn done(&mut self, id: WorkId, ns: u64, outcome: Outcome, now_ns: u64) -> Result<Vec<Effect, Global>, SchedError> {
        let at = self.live.binary_search_by_key(&id, |live| live.id).map_err(|_| SchedError::UnknownWork { id })?;
        match (self.live[at].state, outcome) {
            (State::Queued, _) => return Err(SchedError::NotRunning { id }),
            (State::Running, Outcome::Cancelled) => return Err(SchedError::CancelledUnasked { id }),
            _ => {}
        }
        let live = self.live.remove(at);
        let mut failed = Vec::new_in(Global);
        if outcome == Outcome::Failed {
            failed.push(live.id);
        }
        if outcome == Outcome::Computed {
            let measured = match self.measured.iter().position(|(pass, _)| *pass == live.pass) {
                Some(at) => &mut self.measured[at].1,
                None => {
                    self.measured.push((live.pass, Measured::default()));
                    let last = self.measured.len() - 1;
                    &mut self.measured[last].1
                }
            };
            measured.add(ns);
            let slow = measured.slow(live.pass, live.class);
            if let (Some(slow), Some(current)) = (slow, &mut self.current)
                && !current.slow.iter().any(|known| known.pass == slow.pass)
            {
                current.slow.push(slow);
            }
        }
        if let Some(current) = &mut self.current
            && live.owner == current.snapshot
        {
            match outcome {
                Outcome::Computed | Outcome::Hit => current.done += 1,
                Outcome::Failed => current.failed.push(Failed { pass: live.pass, item: live.subject.item() }),
                Outcome::Cancelled => {}
            }
        }
        // A failed work fails what depends on it; an ended one releases it.
        while let Some(gone) = failed.pop() {
            let mut index = 0;
            while index < self.live.len() {
                let dependent = self.live[index].deps.contains(&gone) && self.live[index].state == State::Queued;
                if dependent {
                    let dependent = self.live.remove(index);
                    if let Some(current) = &mut self.current
                        && dependent.owner == current.snapshot
                    {
                        current.failed.push(Failed { pass: dependent.pass, item: dependent.subject.item() });
                    }
                    failed.push(dependent.id);
                } else {
                    index += 1;
                }
            }
        }
        for other in &mut self.live {
            other.deps.retain(|dep| *dep != id);
        }
        let mut effects = Vec::new_in(Global);
        self.pump(&mut effects);
        self.maybe_reply(now_ns, &mut effects);
        Ok(effects)
    }

    /// The clock moved: a wait's budget may be over.
    pub fn advance(&mut self, now_ns: u64) -> Vec<Effect, Global> {
        let mut effects = Vec::new_in(Global);
        self.maybe_reply(now_ns, &mut effects);
        effects
    }
}
