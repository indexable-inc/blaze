//! The emitter's rings under N producers and one reader, interleaved at every atomic operation.
//!
//! The oracles: no producer operation ever waits (each push is its eleven operations, each
//! done when scheduled); every entry the reader keeps is exactly the entry its producer pushed
//! under that number (never torn); and per lane the reader's reports, in order, cover every
//! number pushed exactly once, an entry reported lost only once the producer had begun pushing
//! the entry `len` after it (only the oldest are lost, and the gap count is exact).

use std::sync::atomic::Ordering;

use blaze_sim::{Broke, Config, Tape, check, control};

use super::{SimCells, harness, key, oracle};
use crate::Plant;
use crate::ring::{Cells, Drain, Event, Got, Op, PUSH_OPS, Ring, Step, push_ops_planted};

/// The event a producer pushes as entry `n` of `lane`: every word a function of both.
fn event_of(lane: u32, n: u64) -> Event {
    Event {
        kind: (n % 23) as u16,
        flags: lane as u16,
        detail: (n * 7) as u32,
        at: n,
        value: (u64::from(lane) << 40) | n,
        key: key(u64::from(lane) * 1_000_003 + n),
    }
}

struct Producer {
    ring: Ring,
    lane: u32,
    /// Entries to push in all.
    total: u64,
    /// The entry being pushed (or next).
    n: u64,
    ops: [Op; PUSH_OPS],
    op: usize,
    /// Entries whose first operation is done.
    begun: u64,
}

struct Reader {
    lane: usize,
    drain: Option<Drain>,
    /// Per lane: the next number the reports must start at.
    next: std::vec::Vec<u64>,
}

fn run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let lanes = tape.range(1..5, "lanes") as usize;
    let len = 1u64 << tape.range(1..4, "ring length, log 2");
    let cells = SimCells::new(lanes * Ring::words(len));
    let mut producers: std::vec::Vec<Producer> = (0..lanes)
        .map(|lane| Producer {
            ring: Ring { base: lane * Ring::words(len), len },
            lane: lane as u32,
            total: tape.range(0..40, "entries"),
            n: 0,
            ops: [Op::Fence(Ordering::Release); PUSH_OPS],
            op: PUSH_OPS,
            begun: 0,
        })
        .collect();
    let mut reader = Reader { lane: 0, drain: None, next: vec![0; lanes] };
    let reader_weight = tape.range(1..8, "reader's share of steps");
    let mut finishing = false;
    let mut steps = 0u32;
    loop {
        steps += 1;
        if steps > 1_000_000 {
            return Err(harness("the schedule did not end"));
        }
        let busy: std::vec::Vec<usize> =
            (0..lanes).filter(|at| producers[*at].n < producers[*at].total).collect();
        if busy.is_empty() && !finishing {
            // Every push is done: one last full pass, from lane 0, to account for everything.
            finishing = true;
            if reader.drain.is_none() {
                reader.lane = 0;
            }
        }
        let read = finishing || tape.below(8, "who steps") < reader_weight;
        if !read {
            let at = busy[tape.index(busy.len(), "which producer")];
            let p = &mut producers[at];
            if p.op == PUSH_OPS {
                p.ops = push_ops_planted(p.ring, p.n, &event_of(p.lane, p.n), plant);
                p.op = 0;
            }
            if !cells.apply(p.ops[p.op]) {
                return Err(Broke::Oracle(format!(
                    "a producer blocked: lane {} waited to push entry {}",
                    p.lane, p.n
                )));
            }
            if p.op == 0 {
                p.begun += 1;
            }
            p.op += 1;
            if p.op == PUSH_OPS {
                p.n += 1;
            }
            continue;
        }
        let lane = reader.lane;
        let ring = producers[lane].ring;
        let drain = reader.drain.get_or_insert_with(|| {
            let mut drain = Drain::new(ring, reader.next[lane]);
            drain.plant = plant;
            drain
        });
        match drain.step(&cells).map_err(harness)? {
            Step::Working => {}
            Step::Got(got) => {
                let begun = producers[lane].begun;
                let (from, count) = match got {
                    Got::Event { n, event } => {
                        oracle(event == event_of(lane as u32, n), || {
                            format!("torn: lane {lane} entry {n} was read as {event:?}")
                        })?;
                        (n, 1)
                    }
                    Got::Lost { from, count } => {
                        oracle(from + count + len <= begun, || {
                            format!(
                                "gap count: lane {lane} reported entries {from}..{} lost, but only {begun} pushes began (ring of {len})",
                                from + count
                            )
                        })?;
                        (from, count)
                    }
                };
                oracle(from == reader.next[lane], || {
                    format!("gap count: lane {lane} reported from {from} where {} was next", reader.next[lane])
                })?;
                reader.next[lane] = from + count;
            }
            Step::Done { next } => {
                oracle(next == reader.next[lane], || {
                    format!("gap count: lane {lane}'s drain ended at {next}, its reports at {}", reader.next[lane])
                })?;
                // Published for the plant that waits for the reader; the clean producer never
                // reads it.
                cells.store(ring.reader(), next, Ordering::Release);
                reader.drain = None;
                reader.lane += 1;
                if reader.lane == lanes {
                    reader.lane = 0;
                    if finishing {
                        break;
                    }
                }
            }
        }
    }
    for (lane, p) in producers.iter().enumerate() {
        oracle(reader.next[lane] == p.total, || {
            format!("gap count: lane {lane} pushed {} entries, the reader accounted for {}", p.total, reader.next[lane])
        })?;
    }
    Ok(())
}

#[test]
fn rings_lose_only_the_oldest_and_never_block() {
    check(Config::cases(3000), |tape| run(tape, Plant::None));
}

#[test]
fn the_oracle_catches_a_torn_entry() {
    control(Config::cases(5000), Plant::None, Plant::NoRecheck, "torn", run);
}

#[test]
fn the_oracle_catches_a_wrong_gap_count() {
    control(Config::cases(3000), Plant::None, Plant::LostOffByOne, "gap count", run);
}

#[test]
fn the_oracle_catches_a_producer_that_waits() {
    control(Config::cases(3000), Plant::None, Plant::WaitForReader, "blocked", run);
}
