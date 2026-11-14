//! The deterministic simulation of the module seam: tasks of the front end, some of whose
//! steps expand a call of a proc-macro crate whose module does not exist yet, run interleaved
//! by a seed; a task that misses a module compiles it (the key's one compiler) or parks on it.
//!
//! The oracle: a task waits only at a step that uses a module, and only on that module; every
//! module is compiled once; every task finishes. The plant models today's load, a write into
//! the front end's database (`ProcMacros::give(&mut Db, ..)`): a write waits for, and cancels,
//! every reader in flight, whether it uses the macro or not.

use std::{alloc::Global, sync::Arc};

use blaze_keyed::{
    kind::{CodeKey, ExpanderModule, Key, ProcKind, UnitManifest},
    proof::{self, Snapshot},
};
use blaze_rng::SplitMix64;
use hegel::TestCase;

use super::support::{Broke, Draw, Drawn, MemStore, control, filing, tool};
use crate::{
    Digest, Subject, What,
    machine::Ticket,
    modules::{Export, ModuleValue, Modules, Resolved, file_module, module_digest, module_key, open_module},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// A module's appearance is a write the front end's every reader waits on.
    GlobalWrite,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Work,
    Use(usize),
}

#[derive(Clone, Debug)]
struct Plan {
    modules: usize,
    tasks: Vec<Vec<Step>>,
    /// How many of its own steps a compile takes.
    compile_steps: Vec<u64>,
    seed: u64,
}

fn plan(d: &mut impl Draw) -> Plan {
    let modules = d.int(1, 3) as usize;
    let tasks = (0..d.int(2, 8))
        .map(|_| {
            (0..d.int(1, 6))
                .map(|_| match d.int(0, 2) {
                    0 => Step::Use(d.under(modules)),
                    _ => Step::Work,
                })
                .collect()
        })
        .collect();
    let compile_steps = (0..modules).map(|_| d.int(1, 4)).collect();
    Plan { modules, tasks, compile_steps, seed: d.int(0, u64::MAX) }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ready,
    /// Parked on a module's flight.
    Parked(usize),
    /// Compiling a module, with this many steps left.
    Compiling { module: usize, left: u64 },
    Finished,
}

fn key(module: usize) -> Key<ExpanderModule> {
    let unit = Key::<UnitManifest>::from_id(Digest::of("test unit", &(module as u64).to_le_bytes()));
    module_key(tool(), CodeKey::of(&tool(), unit, &[]))
}

/// One request: every task to its end.
fn run(plan: &Plan, plant: Plant) -> Result<(), Broke> {
    proof::scope(|snap| run_in(snap, plan, plant))
}

fn run_in(snap: Snapshot<'_>, plan: &Plan, plant: Plant) -> Result<(), Broke> {
    let mut rng = SplitMix64::new(plan.seed);
    let store = MemStore::new();
    let mut seam = Modules::<usize>::default();
    let mut tickets: Vec<Option<Ticket<ExpanderModule>>> = (0..plan.modules).map(|_| None).collect();
    let mut at = vec![0usize; plan.tasks.len()];
    let mut state = vec![State::Ready; plan.tasks.len()];
    let mut compiled = vec![0u32; plan.modules];
    // Each wait: the task, the module it waited on.
    let mut waits = Vec::new();
    let subject = |m: usize| Subject { what: What::ExpanderModule, name: Arc::from(format!("macros{m}")) };
    let bound = 10 * plan.tasks.iter().map(Vec::len).sum::<usize>() + 100;
    for _ in 0..bound {
        let runnable: Vec<usize> =
            (0..plan.tasks.len()).filter(|&t| matches!(state[t], State::Ready | State::Compiling { .. })).collect();
        if runnable.is_empty() {
            break;
        }
        let t = runnable[Draw::under(&mut rng, runnable.len())];
        match state[t] {
            State::Compiling { module, left } if left > 1 => state[t] = State::Compiling { module, left: left - 1 },
            State::Compiling { module, .. } => {
                let bytes = format!("module {module}").into_bytes();
                let helpers = ["helper"];
                let exports = [Export { name: "Model", kind: ProcKind::Derive, index: 0, helpers: &helpers }];
                let value = ModuleValue { module: module_digest(&bytes), len: bytes.len() as u64, exports: &exports };
                file_module(&store, &subject(module), key(module), &value).map_err(filing)?;
                compiled[module] += 1;
                if plant == Plant::GlobalWrite {
                    // The write waits on every reader in flight, and cancels it.
                    for other in 0..plan.tasks.len() {
                        if other != t && at[other] > 0 && state[other] != State::Finished {
                            waits.push((other, module));
                        }
                    }
                }
                let ticket = tickets[module].take().expect("its compiler holds the ticket");
                let woken = seam.end(ticket, Ok(()));
                for waiter in woken.waiters {
                    state[waiter] = State::Ready;
                }
                state[t] = State::Ready;
            }
            State::Ready => {
                let Some(&step) = plan.tasks[t].get(at[t]) else {
                    state[t] = State::Finished;
                    continue;
                };
                match step {
                    Step::Work => at[t] += 1,
                    Step::Use(m) => match seam
                        .resolve(snap, &store, &subject(m), key(m), t, Global)
                        .map_err(|it| Broke::Harness(it.to_string()))?
                    {
                        Resolved::Ready { holds, record } => {
                            let module = open_module(&subject(m), &holds, &record)
                                .map_err(|it| Broke::Harness(it.to_string()))?;
                            if module.export("Model").map(|it| it.kind) != Some(ProcKind::Derive) {
                                return Err(Broke::Harness(format!("module {m} lost its export")));
                            }
                            at[t] += 1;
                        }
                        Resolved::Compile(ticket) => {
                            tickets[m] = Some(ticket);
                            state[t] = State::Compiling { module: m, left: plan.compile_steps[m] };
                        }
                        Resolved::Wait => {
                            waits.push((t, m));
                            state[t] = State::Parked(m);
                        }
                    },
                }
            }
            State::Parked(_) | State::Finished => unreachable!("only runnable tasks are picked"),
        }
    }
    for (t, it) in state.iter().enumerate() {
        if *it != State::Finished {
            return Err(Broke::Oracle(format!("task {t} did not finish: {it:?}")));
        }
    }
    for (m, &n) in compiled.iter().enumerate() {
        if n > 1 {
            return Err(Broke::Oracle(format!("module {m} was compiled {n} times")));
        }
    }
    for &(t, m) in &waits {
        if !plan.tasks[t].contains(&Step::Use(m)) {
            return Err(Broke::Oracle(format!("task {t} waited on module {m}, which none of its steps uses")));
        }
    }
    Ok(())
}

/// The invariant: no task that does not use an appearing module waits on it; each module is
/// compiled once; every task finishes.
#[hegel::test(test_cases = 300)]
fn a_module_appearing_stops_only_its_users(tc: TestCase) {
    let plan = plan(&mut Drawn(&tc));
    if let Err(broke) = run(&plan, Plant::None) {
        panic!("{broke}\nplan {plan:?}");
    }
}

fn seeded(rng: &mut SplitMix64, plant: Plant) -> Result<(), Broke> {
    run(&plan(rng), plant)
}

#[test]
fn the_oracle_catches_a_module_load_that_writes_the_front_end() {
    control(Plant::GlobalWrite, seeded);
}
