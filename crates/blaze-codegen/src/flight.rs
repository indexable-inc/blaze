//! Single flight across builds, one function at a time: each function a build misses in the
//! store is compiled by one build of the process, whichever asks first. A build that asks for it
//! while that compile is under way goes on with its other functions and then waits for that one
//! function; one that asks after takes the shape the memo keeps its template under, and prepares
//! nothing. Two proc macros of a job that both reach `syn`'s parser compile it once
//! (docs/v2/plan/design/compile-time.md).
//!
//! A function's compile is a run of the scheduler's ([`pump::Flights`]) under its flight key,
//! from the moment a build asks for it until it lands ([`Flights::ask`]): one function, never a
//! batch, so a build that waits for it waits for its compile alone, and the landing is there the
//! moment the run ends. A run that ends without one (its compile failed, or it unwound) lets the
//! function go: a build that waited for it asks again, and compiles it if no build has it. A
//! build holds no function while it waits for another's ([`Flights::ask`] never waits, and a
//! build waits only once its own runs ended), so no two builds wait on each other's functions.
//!
//! What a build does with each of its functions (ask, wait, take a landing, ask again, or give
//! up after [`ASKS`]) is decided by [`Plan`], a state machine that reads no table and waits for
//! nothing (rule 14): the driver does the asking and waiting and tells it what each found.
//!
//! A wait parks its job on the run's flight in the scheduler: its thread runs other work
//! meanwhile, the scheduler sees who waits for whom (a wait that would close a cycle gives way),
//! and the wait is a span naming the function, its key and the job that compiles it.
//!
//! The landings are a lock-free table ([`SharedByHash`]): a landing is one insert, a look one
//! read, and a store hit touches nothing. Kept in the [`Memo`] whose templates the shapes name:
//! a landing outlives no template.
//!
//! A release build's summaries fly the same way, by their store key: the summary a build made
//! is kept here before it lands, so a build that waited for it or asks after takes it and
//! prepares the function only if it compiles or imports it.
//!
//! [`Memo`]: crate::memo::Memo

#![forbid(unsafe_code)]

use std::sync::Arc;

use blaze_graph::{Cancelled, Key, pump};
use blaze_hash::SharedByHash;
use blaze_ids::ContentId;

use crate::{InstanceKey, summary::Summary};

#[derive(Default)]
pub struct Flights {
    /// Each function landed, by its flight key: the shape its template is kept under in the memo.
    landed: SharedByHash<InstanceKey, ContentId>,
    /// The compile under way of each function, by its flight key: what a wait for it parks on,
    /// and the landing it ends on (`None` for none).
    runs: pump::Flights<Option<ContentId>>,
    /// The build whose run of each function is under way, by its flight key: what a wait for
    /// it names.
    running: SharedByHash<InstanceKey, Arc<str>>,
    /// The compile under way of each shape (a dev compile's IR), so two functions of one shape
    /// compile it once, whichever builds they are of.
    shapes: pump::Flights<()>,
    /// Each summary landed, by its flight key.
    summaries: SharedByHash<InstanceKey, Summary>,
}

/// What a build's ask of a function found ([`Flights::ask`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Asked<R> {
    /// This build ran it: what its run made (and landed, when it gave a landing).
    Ran(R),
    /// Another build compiles it now: wait for it ([`Flights::wait`]) once this build's own
    /// runs ended.
    Theirs,
    /// Compiled already, its template in the memo under this shape.
    Landed(ContentId),
}

/// The scheduler's key of a flight key's run: its bytes.
fn run_key(key: InstanceKey) -> Key {
    Key(key.0.0)
}

impl Flights {
    pub fn new() -> Flights {
        Flights::default()
    }

    /// Runs `run` for the function under flight key `key` here, for the build `owner` names,
    /// unless it landed or another build runs it now; never waits. `run` gives what lands (the shape its template is kept
    /// under in the memo, which `run` put there; `None` when it made none) and what the caller
    /// keeps. The landing is in the table before the run ends, so a build that waited for it
    /// or asks after takes it, and no function compiles twice while one compile of it lands.
    pub fn ask<R>(
        &self,
        key: InstanceKey,
        owner: &Arc<str>,
        run: impl FnOnce() -> (Option<ContentId>, R),
    ) -> Asked<R> {
        if let Some(shape) = self.landed.pin().get(&key) {
            return Asked::Landed(*shape);
        }
        let (mut made, mut found) = (None, None);
        let ran = self.runs.run_if_idle(run_key(key), || {
            // Landed by a run that ended since the look above.
            if let Some(shape) = self.landed.pin().get(&key) {
                found = Some(*shape);
                return Ok(Some(*shape));
            }
            // clone: the owner's name, shared, for the waits on this run
            self.running.pin().insert(key, Arc::clone(owner));
            let ran = Running { running: &self.running, key };
            let (landing, it) = run();
            drop(ran);
            if let Some(shape) = landing {
                // Only this run writes `key` now: no other is under way, and each before it
                // ended unlanded (else the look above found it).
                let landed = self.landed.pin();
                let kept = landed.get_or_insert(key, shape);
                assert_eq!(
                    *kept, shape,
                    "flight key {key:?} landed twice, as {kept:?} and as {shape:?}"
                );
            }
            made = Some(it);
            Ok(landing)
        });
        match (ran, made, found) {
            (None, ..) => Asked::Theirs,
            (Some(_), Some(made), _) => Asked::Ran(made),
            (Some(_), None, Some(shape)) => Asked::Landed(shape),
            (Some(_), None, None) => unreachable!(
                "a run of flight key {key:?} that ended neither ran nor found a landing"
            ),
        }
    }

    /// Waits for the function under flight key `key`, which another build ran when this one
    /// asked: its landing, or `None` when that run ended without one (its compile failed, or it
    /// unwound), and the caller asks again. The wait parks this job on the run, a span naming
    /// what `named` writes, given the name of the build that runs it, and the job that runs it;
    /// a wait that would close a cycle of jobs gives way, unwinding with [`Cancelled`], so the
    /// task is posted again.
    pub fn wait(
        &self,
        key: InstanceKey,
        mut named: impl FnMut(&mut String, &str),
    ) -> Option<ContentId> {
        if let Some(shape) = self.landed.pin().get(&key) {
            return Some(*shape);
        }
        let named = |it: &mut String| {
            let running = self.running.pin();
            named(it, running.get(&key).map_or("a build whose run of it ended", |owner| &**owner));
        };
        // No run under way: it ended, and its landing (or none) is in the table.
        match self
            .runs
            .get_or_run_named(run_key(key), named, || Ok(self.landed.pin().get(&key).copied()))
        {
            Ok(landing) => landing,
            // alloc: a give-way's payload, once per task that gives way
            Err(Cancelled) => std::panic::resume_unwind(Box::new(Cancelled)),
        }
    }

    /// Runs `run`, the compile of `shape`, here, unless another runs now: then waits for it,
    /// a span naming what `named` writes and the job that compiles it, and runs nothing. `run`
    /// puts what it compiled in the memo before it returns, so a waiter reads it there.
    pub fn shape(&self, shape: ContentId, named: impl FnMut(&mut String), run: impl FnOnce()) {
        let ran = self.shapes.get_or_run_named(Key(shape.0), named, || {
            run();
            Ok(())
        });
        if ran.is_err() {
            // alloc: a give-way's payload, once per task that gives way
            std::panic::resume_unwind(Box::new(Cancelled));
        }
    }

    /// How many functions have landed.
    pub fn landed(&self) -> usize {
        self.landed.pin().len()
    }

    /// Keeps `summary`, made under the flight key `key` a build runs, for the builds that take
    /// it once it lands. Only a run of `key` keeps under it, and [`Flights::retain`] drops a
    /// summary with its landing, so `key` holds a summary already only when a build runs it
    /// again; an equal one is one, and another is refused (`false`), never written over (rule
    /// 18), the kept one staying.
    pub fn keep_summary(&self, key: InstanceKey, summary: Summary) -> bool {
        match self.summaries.pin().try_insert(key, summary) {
            Ok(_) => true,
            Err(it) => *it.current == it.not_inserted,
        }
    }

    /// [`Flights::keep_summary`] of each of `made`: the keys of those refused.
    pub fn keep_summaries(&self, made: Vec<(InstanceKey, Summary)>) -> Vec<InstanceKey> {
        // alloc: per batch of summaries, sized by those refused
        made.into_iter()
            .filter_map(|(key, summary)| (!self.keep_summary(key, summary)).then_some(key))
            .collect()
    }

    /// The summary kept under each of `keys`, landed: `None` for one no build kept.
    pub fn summaries(&self, keys: &[InstanceKey]) -> Vec<Option<Summary>> {
        let table = self.summaries.pin();
        // clone: each taken in place of the store's, which a build decodes into one of its own
        // alloc: per batch of summaries another build made, sized by it
        keys.iter().map(|key| table.get(key).cloned()).collect()
    }

    /// Keeps only the landings whose shape `kept` keeps, and the summaries of those kept: the
    /// memo's collection, between builds, when no function's run is under way.
    pub(crate) fn retain(&mut self, kept: impl Fn(&ContentId) -> bool) {
        let table = self.landed.pin();
        table.retain(|_, shape| kept(shape));
        self.summaries.pin().retain(|key, _| table.contains_key(key));
    }
}

/// A run's entry in [`Flights::running`], taken out as the run ends, unwound or not.
struct Running<'f> {
    running: &'f SharedByHash<InstanceKey, Arc<str>>,
    key: InstanceKey,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.running.pin().remove(&self.key);
    }
}

/// How often a build asks for one function before it gives up on it: each ask after the first
/// follows a run of another build's that ended unlanded, which its waiter saw. Far more than the
/// builds of a job, so only a livelock reaches it, and it ends in an error naming the function
/// rather than an open loop (rule 18).
pub const ASKS: u32 = 256;

/// What an ask of an item found ([`Plan::asked`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Found {
    /// This build ran it: done, whatever its run made.
    Ran,
    /// Another build runs it now.
    Theirs,
    /// Landed under this shape.
    Landed(ContentId),
}

/// What a build does next with one of its items, by its index ([`Plan::take_decisions`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    /// Ask for it ([`Flights::ask`]): run it here unless it landed or another build runs it.
    Ask(usize),
    /// Wait for another build's run of it ([`Flights::wait`]).
    Wait(usize),
    /// Take its landing, under this shape: nothing of it is prepared or compiled here.
    Take(usize, ContentId),
    /// Give it up, an error naming it: asked `asks` times, each time another build ran it and
    /// let it go unlanded.
    GiveUp { at: usize, asks: u32 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Asking,
    Waiting,
    Done,
}

/// What one build does with each of its items (functions, summaries, groups) of the memo's
/// flights, as decisions: a synchronous state machine that reads no table, compiles nothing and
/// waits for nothing (rule 14). The driver asks and waits, and tells it what each found
/// ([`Plan::asked`], [`Plan::waited`]); it hands back what to do next
/// ([`Plan::take_decisions`]). Every item ends run here, taken from another build's landing, or
/// given up after [`ASKS`] asks, so a build's flights end (rule 18).
pub struct Plan {
    /// Of each item: where it is, and how often it was asked for.
    steps: Vec<(Step, u32)>,
    decisions: Vec<Decision>,
}

impl Plan {
    /// A plan of `len` items, each to be asked for first.
    pub fn new(len: usize) -> Plan {
        // alloc: per batch of store misses, sized by it
        Plan {
            steps: vec![(Step::Asking, 0); len],
            decisions: (0..len).map(Decision::Ask).collect(),
        }
    }

    /// What to do next, in the order decided; empty once every item is done.
    pub fn take_decisions(&mut self) -> Vec<Decision> {
        std::mem::take(&mut self.decisions)
    }

    /// Whether every item is done: run, taken or given up.
    pub fn done(&self) -> bool {
        self.steps.iter().all(|it| it.0 == Step::Done)
    }

    /// Item `at`, asked for, found `found`.
    pub fn asked(&mut self, at: usize, found: Found) {
        let (step, asks) = &mut self.steps[at];
        assert_eq!(*step, Step::Asking, "item {at} told of an ask while not asked for");
        *asks += 1;
        match found {
            Found::Ran => *step = Step::Done,
            Found::Theirs => {
                *step = Step::Waiting;
                self.decisions.push(Decision::Wait(at));
            }
            Found::Landed(shape) => {
                *step = Step::Done;
                self.decisions.push(Decision::Take(at, shape));
            }
        }
    }

    /// Item `at`, waited for, ended as `landing`: taken, or asked for again when the run let it
    /// go, unless it was asked for [`ASKS`] times.
    pub fn waited(&mut self, at: usize, landing: Option<ContentId>) {
        let (step, asks) = &mut self.steps[at];
        assert_eq!(*step, Step::Waiting, "item {at} told of a wait while not waited for");
        match landing {
            Some(shape) => {
                *step = Step::Done;
                self.decisions.push(Decision::Take(at, shape));
            }
            None if *asks >= ASKS => {
                *step = Step::Done;
                self.decisions.push(Decision::GiveUp { at, asks: *asks });
            }
            None => {
                *step = Step::Asking;
                self.decisions.push(Decision::Ask(at));
            }
        }
    }
}

#[cfg(test)]
mod tests;
