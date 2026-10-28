//! Does what the [`Machine`] asks (docs/driver.md, "The executor"): each node it says to run
//! goes to a thread of its pool, and what came of it goes back to the machine on the thread
//! that called [`drive`], which is the only one that steps it. A pool has as many threads as
//! the machine runs nodes of it at once.

use std::{
    any::Any,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU32, Ordering},
    },
    time::Instant,
};

use blaze_protocol::events::Record;
use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::machine::{Error, Input, Machine, NodeId, Outcome, Output, Pool, Spec};

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

/// `count` node ids no other graph of this process has, the first of them: what one process
/// says of several graphs is one stream.
pub fn ids(count: usize) -> u32 {
    NEXT_ID.fetch_add(u32::try_from(count).unwrap_or(u32::MAX), Ordering::Relaxed)
}

/// What a node's work came to, and the nodes it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Did {
    pub outcome: Outcome,
    pub added: Vec<Spec>,
}

impl Did {
    pub fn built() -> Did {
        Did { outcome: Outcome::Built, added: Vec::new() }
    }

    pub fn failed(why: impl Into<String>) -> Did {
        Did { outcome: Outcome::Failed { why: why.into() }, added: Vec::new() }
    }
}

/// Runs the build of `nodes` on `machine` to its end: `work` does a node's work on a thread of
/// its pool, `said` hears every record as the machine says it, and `trace`, when given, gets
/// every input the machine took, in order ([`crate::trace`]). Whether every node was done.
///
/// A node whose work panics fails, and once the build is over the panic goes on on the calling
/// thread, as from a thread of [`std::thread::scope`]: a panic that unwinds on purpose, as
/// salsa's cancellation does, reaches whoever catches it.
pub fn drive<W>(
    machine: &mut Machine,
    nodes: Vec<Spec>,
    work: &W,
    said: &mut dyn FnMut(&Record),
    trace: Option<&mut Vec<Input>>,
) -> Result<bool, Error>
where
    W: Fn(NodeId) -> Did + Sync,
{
    let start = Instant::now();
    let now = || u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let config = machine.config();
    let (done_tx, done_rx) = unbounded::<(NodeId, Did, u64)>();
    let first_panic: Mutex<Option<Box<dyn Any + Send>>> = Mutex::new(None);
    let over = std::thread::scope(|scope| {
        let mut pools: Vec<Sender<NodeId>> = Vec::new();
        for threads in [config.io, config.work] {
            let (tx, rx): (Sender<NodeId>, Receiver<NodeId>) = unbounded();
            for _ in 0..threads {
                let (rx, done_tx, first_panic) = (rx.clone(), done_tx.clone(), &first_panic);
                scope.spawn(move || {
                    for id in rx {
                        let did =
                            catch_unwind(AssertUnwindSafe(|| work(id))).unwrap_or_else(|panic| {
                                let did = Did::failed(panicked(&*panic));
                                let mut first =
                                    first_panic.lock().unwrap_or_else(PoisonError::into_inner);
                                first.get_or_insert(panic);
                                did
                            });
                        if done_tx.send((id, did, now())).is_err() {
                            return;
                        }
                    }
                });
            }
            pools.push(tx);
        }
        drop(done_tx);
        let over = run(machine, nodes, &pools, &done_rx, &now, said, trace);
        // The threads end once nothing more can be sent them, whether the build ended or not.
        drop(pools);
        over
    });
    if let Some(panic) = first_panic.into_inner().unwrap_or_else(PoisonError::into_inner) {
        resume_unwind(panic);
    }
    over
}

fn run(
    machine: &mut Machine,
    nodes: Vec<Spec>,
    pools: &[Sender<NodeId>],
    done_rx: &Receiver<(NodeId, Did, u64)>,
    now: &dyn Fn() -> u64,
    said: &mut dyn FnMut(&Record),
    mut trace: Option<&mut Vec<Input>>,
) -> Result<bool, Error> {
    let mut feed = |input: Input| -> Result<Option<bool>, Error> {
        if let Some(trace) = trace.as_deref_mut() {
            trace.push(input.clone());
        }
        let mut over = None;
        for output in machine.step(input)? {
            match output {
                Output::Said(record) => said(&record),
                Output::Run { id, pool } => {
                    let at = match pool {
                        Pool::Io => 0,
                        Pool::Work => 1,
                    };
                    // The pool's threads outlive this send: they end once `pools` is dropped.
                    _ = pools[at].send(id);
                }
                Output::Finished { ok } => over = Some(ok),
            }
        }
        Ok(over)
    };
    let mut over = feed(Input::Add { at_ns: now(), nodes })?;
    if over.is_none() {
        over = feed(Input::Close { at_ns: now() })?;
    }
    while over.is_none() {
        // Every node the machine runs is on a thread that sends what came of it.
        let Ok((id, did, at_ns)) = done_rx.recv() else {
            unreachable!("the pools' threads end only after the build does");
        };
        over = feed(Input::Done { at_ns, id, outcome: did.outcome, added: did.added })?;
    }
    Ok(over.unwrap_or(false))
}

fn panicked(panic: &(dyn Any + Send)) -> String {
    let what = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a panic with no message");
    format!("panicked: {what}")
}
