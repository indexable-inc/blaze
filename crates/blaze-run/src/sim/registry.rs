//! [`Registry`] against processes that start, register, end cleanly, die without a word, and
//! whose pids are reused; the oracle: a read never lists a run whose process is gone, and
//! removes what it left out. And each event of the stream reads back as it was written.

use std::alloc::Global;
use std::collections::BTreeMap;

use blaze_sim::{Broke, Config, Tape, check, control};

use super::{harness, key, oracle};
use crate::registry::{Effect, Entry, Name, Pid, ProcessStart, Processes, Registry, RunEvent};
use crate::{CheckoutKey, InstanceKey, Nanos, Plant, TestId};

const PIDS: u32 = 3;

/// The process table: which process has each pid now.
struct Table(BTreeMap<Pid, ProcessStart>);

impl Processes for Table {
    fn start_of(&self, pid: Pid) -> Option<ProcessStart> {
        self.0.get(&pid).copied()
    }
}

fn apply(effects: &mut Vec<Effect, Global>, directory: &mut BTreeMap<Name, Vec<u8>>) {
    for effect in effects.drain(..) {
        match effect {
            Effect::Put { name, bytes } => {
                directory.insert(name, bytes.to_vec());
            }
            Effect::Remove { name } => {
                directory.remove(&name);
            }
            Effect::Append { .. } => {}
        }
    }
}

fn run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut table = Table(BTreeMap::new());
    let mut runs: BTreeMap<Pid, Registry> = BTreeMap::new();
    let mut directory: BTreeMap<Name, Vec<u8>> = BTreeMap::new();
    let mut effects = Vec::new_in(Global);
    let mut starts = 0u64;
    let steps = tape.range(1..80, "steps");
    for _ in 0..steps {
        let pid = Pid(tape.below(u64::from(PIDS), "pid") as u32);
        match tape.below(4, "action") {
            // A process starts on a free pid, and registers as a run.
            0 if !table.0.contains_key(&pid) => {
                starts += 1;
                let start = ProcessStart(starts);
                table.0.insert(pid, start);
                let mut registry = Registry::new();
                registry.plant = plant;
                let entry = Entry {
                    name: Name { pid, start },
                    checkout: CheckoutKey(key(1)),
                    program: InstanceKey(key(2)),
                    began: Nanos(starts),
                };
                registry.register(&entry, &mut effects).map_err(harness)?;
                apply(&mut effects, &mut directory);
                runs.insert(pid, registry);
            }
            // It ends cleanly, or dies without removing its entry.
            1 if table.0.contains_key(&pid) => {
                if let Some(mut registry) = runs.remove(&pid)
                    && tape.coin("clean end")
                {
                    registry.unregister(&mut effects).map_err(harness)?;
                    apply(&mut effects, &mut directory);
                }
                table.0.remove(&pid);
            }
            // Some process reads the directory.
            _ => {
                let mut registry = Registry::new();
                registry.plant = plant;
                let listing: Vec<(Name, &[u8])> =
                    directory.iter().map(|(name, bytes)| (*name, bytes.as_slice())).collect();
                let mut live = Vec::new_in(Global);
                registry.read(&listing, &table, &mut live, &mut effects).map_err(harness)?;
                for entry in &live {
                    oracle(table.start_of(entry.name.pid) == Some(entry.name.start), || {
                        format!(
                            "a read listed run {}-{}, a dead run",
                            entry.name.pid.0, entry.name.start.0
                        )
                    })?;
                }
                let listed = live.len();
                apply(&mut effects, &mut directory);
                oracle(directory.len() == listed, || {
                    format!("a read listed {listed} runs and left {} entries", directory.len())
                })?;
            }
        }
    }
    Ok(())
}

#[test]
fn a_read_never_lists_a_dead_run() {
    check(Config::cases(500), |tape| run(tape, Plant::None));
}

#[test]
fn the_oracle_catches_a_pid_taken_for_its_run() {
    control(Config::cases(4000), Plant::None, Plant::PidOnly, "dead run", run);
}

#[test]
fn an_event_reads_back_as_written() {
    check(Config::cases(200), |tape| {
        let at = Nanos(tape.next_u64("at"));
        let one = InstanceKey(key(tape.next_u64("key")));
        let event = match tape.below(9, "kind") {
            0 => RunEvent::Began { at, program: one, checkout: CheckoutKey(key(tape.next_u64("checkout"))) },
            1 => RunEvent::Compiling { function: one },
            2 => RunEvent::Published { function: one },
            3 => RunEvent::Tiered { function: one },
            4 => RunEvent::Failed { function: one },
            5 => RunEvent::TestPassed { test: TestId(one.0) },
            6 => RunEvent::TestFailed { test: TestId(one.0) },
            7 => RunEvent::Memory { at, bytes: tape.next_u64("bytes") },
            _ => RunEvent::Ended { at, status: tape.next_u32("status") as i32 },
        };
        let back = RunEvent::decode(event.encode().as_bytes());
        oracle(back == Ok(event), || format!("{event:?} read back as {back:?}"))
    });
}
