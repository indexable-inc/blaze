//! A queued task's start, as one atomic state: who may take it to run it.
//!
//! A task spawned [`may_overlap`](crate::task::Scope::may_overlap) starts [`Start::Open`]: a
//! worker or its joiner takes it, whichever comes first (a join runs a task no worker started,
//! on its own thread). A task spawned [`must_overlap`](crate::task::Scope::must_overlap) starts
//! [`Start::Reserved`]: it is in one parked worker's mailbox, and only a worker may take it, so
//! it always runs beside its spawner and never on the joining thread. The joiner of a reserved
//! task waits for it instead. Before the pool's lazy start that held only by timing (the workers
//! were up and took their mailbox before the join); now the state says it.
//!
//! The moves:
//! - `OPEN` or `RESERVED` → `CLAIMED` by a worker ([`Claim::by_worker`]);
//! - `OPEN` → `CLAIMED` by the joiner, or by a handle dropping it unrun ([`Claim::by_joiner`]);
//!   a reserved one is refused;
//! - `RESERVED` → `CLAIMED` by its spawner when no worker could be reserved, so none ever saw
//!   it ([`Claim::withdraw`]);
//! - `CLAIMED` → `DONE` by the claimer's `Release` store, after the result is written.
//!
//! Checked by blaze-check under `cfg(blaze_check)` (`model_claim`): a joiner racing the first
//! worker's arrival never runs a reserved task; the control, [`ClaimPlant::JoinerTakesReserved`],
//! lets the joiner take one, and the model must catch it.

#[cfg(blaze_check)]
use blaze_check::sync::atomic::{AtomicU8, Ordering};
#[cfg(not(blaze_check))]
use core::sync::atomic::{AtomicU8, Ordering};

const OPEN: u8 = 0;
const RESERVED: u8 = 1;
const CLAIMED: u8 = 2;
const DONE: u8 = 3;

/// How a task is queued, and so who may start it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Start {
    /// Any worker, or its joiner.
    Open,
    /// Only a worker: it was handed to one in its mailbox.
    Reserved,
}

/// A planted bug for the model's control (rule 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ClaimPlant {
    None,
    /// The joiner takes a reserved task as if it were open.
    #[cfg_attr(not(blaze_check), expect(dead_code, reason = "the model's control"))]
    JoinerTakesReserved,
}

/// A task's start state.
pub(crate) struct Claim {
    state: AtomicU8,
    plant: ClaimPlant,
}

impl Claim {
    pub(crate) fn new(start: Start, plant: ClaimPlant) -> Claim {
        let state = match start {
            Start::Open => OPEN,
            Start::Reserved => RESERVED,
        };
        Claim { state: AtomicU8::new(state), plant }
    }

    fn take(&self, from: u8) -> bool {
        self.state.compare_exchange(from, CLAIMED, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    /// A worker takes it to run it: `false` when another took it.
    pub(crate) fn by_worker(&self) -> bool {
        self.take(OPEN) || self.take(RESERVED)
    }

    /// The joining thread (or a handle dropping it unrun) takes it: `false` when another took
    /// it, and always for a reserved task.
    pub(crate) fn by_joiner(&self) -> bool {
        match self.plant {
            ClaimPlant::None => self.take(OPEN),
            ClaimPlant::JoinerTakesReserved => self.take(OPEN) || self.take(RESERVED),
        }
    }

    /// A reserved task no worker was handed back to its spawner, which takes it to drop it
    /// unrun. Panics if it was not reserved and unclaimed: nothing else could have taken it.
    pub(crate) fn withdraw(&self) {
        assert!(self.take(RESERVED), "bstd::task: a task whose reservation was refused had been started");
    }

    /// The claimer's run ended: `Release`, after its result's writes.
    pub(crate) fn finish(&self) {
        self.state.store(DONE, Ordering::Release);
    }

    /// Whether it ended: `Acquire`, ordered after the body's writes of its result.
    pub(crate) fn done(&self) -> bool {
        self.state.load(Ordering::Acquire) == DONE
    }
}

/// Under the model checker (`RUSTFLAGS="--cfg blaze_check" cargo test -p blaze-std --release
/// model_claim`): a `must_overlap` task reserved for a worker whose thread is arriving while
/// the spawner joins. The worker always runs it; the joiner never does.
#[cfg(all(test, blaze_check))]
mod model {
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::Arc,
    };

    use blaze_check::{
        sync::atomic::{AtomicU8, Ordering},
        thread,
    };

    use super::{Claim, ClaimPlant, Start};

    const NOBODY: u8 = 0;
    const WORKER: u8 = 1;
    const JOINER: u8 = 2;

    struct Job {
        claim: Claim,
        ran_on: AtomicU8,
    }

    fn reserved_then_joined(plant: ClaimPlant) {
        let job = Arc::new(Job { claim: Claim::new(Start::Reserved, plant), ran_on: AtomicU8::new(NOBODY) });
        // The worker's thread arrives (the lazy start) and takes its mailbox.
        let worker = {
            let job = Arc::clone(&job);
            thread::spawn(move || {
                if job.claim.by_worker() {
                    job.ran_on.store(WORKER, Ordering::Relaxed);
                    job.claim.finish();
                }
            })
        };
        // The spawner joins at once: `run_here`, then the wait for the worker.
        if job.claim.by_joiner() {
            job.ran_on.store(JOINER, Ordering::Relaxed);
            job.claim.finish();
        }
        worker.join().expect("the worker ends");
        assert!(job.claim.done(), "the task never ran");
        assert_eq!(job.ran_on.load(Ordering::Relaxed), WORKER, "a must_overlap task ran on its joiner");
    }

    #[test]
    fn model_claim_a_reserved_task_runs_only_on_a_worker() {
        blaze_check::model(|| reserved_then_joined(ClaimPlant::None));
    }

    /// The interleaving it finds: the joiner's claim before the arriving worker's.
    #[test]
    fn model_claim_catches_a_joiner_that_takes_a_reserved_task() {
        let found = catch_unwind(AssertUnwindSafe(|| blaze_check::model(|| reserved_then_joined(ClaimPlant::JoinerTakesReserved))));
        let report = found.err().expect("the model must find the joiner running it");
        let text = report
            .downcast_ref::<std::string::String>()
            .map(std::string::String::as_str)
            .or_else(|| report.downcast_ref::<&'static str>().copied())
            .expect("the model's report is text");
        assert!(text.contains("a must_overlap task ran on its joiner"), "the model found something else: {text}");
    }
}
