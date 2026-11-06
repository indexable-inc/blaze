//! The ring's DST: the real [`Slots`] core as the host, against a model of the app as harness-native-render's
//! media/browser.rs is (it keeps only the newest pending frame, drops stale generations and seqs, adopts the pending
//! frame once the GPU is done with the shown slot, and reports the latch), joined by two FIFO Mach queues (the one to
//! the app bounded, so a send fails Full as harness-mach's does, plus Full failures drawn on their own).
//!
//! The draws: which actor steps next (the host paints, receives a latch, fires its repaint timer; the page changes or
//! resizes; the app receives a message, prepares, draws, or its GPU finishes), and whether a send fails Full.
//!
//! The oracle:
//! - safety, at every write: the slot the host writes is not the one the app shows, not the one it holds pending,
//!   not named by a frame still in the queue, and not the newest picture (a popup is composited from it);
//! - no protocol error: every latch names a frame the core knows;
//! - liveness: once the page stops changing and the app keeps drawing, within a bound the app shows the newest
//!   content of the newest ring.
//!
//! Controls: each [`Plant`] must be caught on one of the first [`CONTROL_SEEDS`] seeded plans.
use super::*;
use std::collections::VecDeque;

/// The app's Mach queue limit in the model (harness-mach's is larger; small here so Full is reached).
const LIMIT: usize = 3;
const CONTROL_SEEDS: u64 = 2000;
/// Quiet rounds allowed for the run to settle.
const SETTLE_ROUNDS: usize = 64;

#[derive(Debug, Clone, Copy)]
enum Down {
    Ring { generation: u32 },
    Frame { generation: u32, slot: u8, seq: u64 },
}

#[derive(Debug, Default)]
struct App {
    generation: Option<u32>,
    seq: u64,
    pending: Option<(u8, u64)>,
    shown: Option<u8>,
    /// A window frame that sampled `shown` is still on the GPU.
    busy: bool,
}

struct World {
    slots: Slots,
    plant: Plant,
    seq: u64,
    /// The content version each slot of the current ring holds.
    content: [u64; SLOTS],
    version: u64,
    /// CEF owes a paint (damage, a resize, an invalidate).
    damaged: bool,
    /// A repaint timer is armed.
    later: bool,
    down: VecDeque<Down>,
    up: VecDeque<(u32, u8)>,
    app: App,
}

impl World {
    fn new(plant: Plant) -> Self {
        let mut w = Self {
            slots: Slots::planted(0, plant),
            plant,
            seq: 0,
            content: [0; SLOTS],
            version: 0,
            damaged: true,
            later: false,
            down: VecDeque::new(),
            up: VecDeque::new(),
            app: App::default(),
        };
        w.down.push_back(Down::Ring { generation: 0 });
        w
    }

    fn after(&mut self, a: After) {
        match a {
            After::Nothing => {}
            After::Repaint => self.damaged = true,
            After::RepaintLater => self.later = true,
        }
    }

    /// The host paints, if CEF owes one. `full`: the send fails Full whatever the queue holds.
    fn paint(&mut self, full: bool) -> Result<(), String> {
        if !self.damaged {
            return Ok(());
        }
        self.damaged = false;
        let Some(k) = self.slots.claim() else { return Ok(()) };
        let g = self.slots.generation();
        if self.app.generation == Some(g) {
            if self.app.shown == Some(k) {
                return Err(format!("the host wrote slot {k} of ring {g}, which the app shows"));
            }
            if self.app.pending.is_some_and(|(s, _)| s == k) {
                return Err(format!("the host wrote slot {k} of ring {g}, which the app holds pending"));
            }
        }
        if self.down.iter().any(|d| matches!(d, Down::Frame { generation, slot, .. } if *generation == g && *slot == k)) {
            return Err(format!("the host wrote slot {k} of ring {g}, which a frame in the queue names"));
        }
        if self.slots.newest() == Some(k) {
            return Err(format!("the host wrote slot {k} of ring {g}, the newest picture a popup is composited from"));
        }
        self.content[usize::from(k)] = self.version;
        self.seq += 1;
        let delivered = !full && self.down.len() < LIMIT;
        if delivered {
            self.down.push_back(Down::Frame { generation: g, slot: k, seq: self.seq });
        }
        let a = self.slots.sent(k, self.seq, delivered);
        self.after(a);
        Ok(())
    }

    fn host_receive(&mut self) -> Result<(), String> {
        let Some((g, s)) = self.up.pop_front() else { return Ok(()) };
        let a = self.slots.latched(g, s)?;
        self.after(a);
        Ok(())
    }

    fn timer(&mut self) {
        if std::mem::take(&mut self.later) {
            self.damaged = true;
        }
    }

    fn change(&mut self) {
        self.version += 1;
        self.damaged = true;
    }

    /// A new size: a new ring at the next paint (host.rs makes it in paint_view and sends it before the frame).
    fn resize(&mut self) {
        let g = self.slots.generation() + 1;
        self.slots = Slots::planted(g, self.plant);
        self.content = [0; SLOTS];
        self.later = false;
        self.down.push_back(Down::Ring { generation: g });
        self.damaged = true;
    }

    fn app_receive(&mut self) {
        let Some(d) = self.down.pop_front() else { return };
        let a = &mut self.app;
        match d {
            Down::Ring { generation } => *a = App { generation: Some(generation), ..App::default() },
            Down::Frame { generation, slot, seq } => {
                if a.generation == Some(generation) && seq >= a.seq {
                    a.seq = seq;
                    a.pending = Some((slot, seq));
                }
            }
        }
    }

    fn app_prepare(&mut self) {
        let a = &mut self.app;
        let Some((slot, _)) = a.pending else { return };
        if a.shown.is_some() && a.busy {
            return;
        }
        a.shown = Some(slot);
        a.pending = None;
        let g = a.generation.expect("a pending frame is of the app's ring");
        self.up.push_back((g, slot));
    }

    fn app_draw(&mut self) {
        if self.app.shown.is_some() {
            self.app.busy = true;
        }
    }

    fn step(&mut self, op: u8, full: bool) -> Result<(), String> {
        match op % 10 {
            0 | 1 => self.paint(full),
            2 => self.host_receive(),
            3 => Ok(self.timer()),
            4 => Ok(self.change()),
            5 => Ok(if op % 7 == 0 { self.resize() }),
            6 | 7 => Ok(self.app_receive()),
            8 => Ok(self.app_prepare()),
            _ => Ok(if op % 2 == 0 { self.app_draw() } else { self.app.busy = false }),
        }
    }

    /// The page is still; the app keeps drawing; no send fails but a full queue's. Bounded.
    fn settle(&mut self) -> Result<(), String> {
        for _ in 0..SETTLE_ROUNDS {
            while !self.down.is_empty() {
                self.app_receive();
            }
            self.app.busy = false;
            self.app_prepare();
            self.app_draw();
            while !self.up.is_empty() {
                self.host_receive()?;
            }
            self.timer();
            self.paint(false)?;
            let quiet = self.down.is_empty() && self.up.is_empty() && !self.damaged && !self.later && self.app.pending.is_none();
            if quiet {
                let g = self.slots.generation();
                let shows = self.app.shown.filter(|_| self.app.generation == Some(g)).map(|s| self.content[usize::from(s)]);
                return if shows == Some(self.version) {
                    Ok(())
                } else {
                    Err(format!("settled with the app showing content {shows:?} of ring {:?}; the newest is {} of ring {g}", self.app.generation, self.version))
                };
            }
        }
        Err(format!("did not settle within {SETTLE_ROUNDS} quiet rounds"))
    }
}

/// One run: `ops` are (actor, Full) draws.
fn run(ops: &[(u8, bool)], plant: Plant) -> Result<(), String> {
    let mut w = World::new(plant);
    for (i, &(op, full)) in ops.iter().enumerate() {
        w.step(op, full).map_err(|e| format!("step {i} (op {op}): {e}"))?;
    }
    w.settle()
}

/// A seeded plan (splitmix64), for the controls and replays.
fn plan_of(seed: u64) -> Vec<(u8, bool)> {
    let mut s = seed;
    let mut next = || {
        s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let n = 10 + next() % 70;
    (0..n).map(|_| { let r = next(); ((r & 0xff) as u8, (r >> 8) % 8 == 0) }).collect() // alloc: a test plan
}

#[hegel::test]
fn the_host_never_writes_a_slot_the_app_may_sample_and_the_app_ends_on_the_newest_picture(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let ops: Vec<(u8, bool)> = tc.draw(gs::vecs(gs::tuples2(gs::integers(), gs::booleans())).max_size(150));
    if let Err(e) = run(&ops, Plant::None) {
        panic!("{e}");
    }
}

#[test]
fn seeded_plans_pass() {
    for seed in 0..CONTROL_SEEDS {
        if let Err(e) = run(&plan_of(seed), Plant::None) {
            panic!("seed {seed}: {e}");
        }
    }
}

fn control(plant: Plant) {
    for seed in 0..CONTROL_SEEDS {
        if let Err(e) = run(&plan_of(seed), plant) {
            eprintln!("Plant::{plant:?} caught at seed {seed}: {e}");
            return;
        }
    }
    panic!("the oracle caught Plant::{plant:?} at none of seeds 0..{CONTROL_SEEDS}: it, or the draws, are too weak");
}

#[test]
fn the_oracle_catches_the_old_neither_latched_nor_newest_rule() {
    control(Plant::NeitherLatchedNorNewest);
}

#[test]
fn the_oracle_catches_a_latch_that_frees_frames_sent_after_it() {
    control(Plant::LatchFreesAll);
}

#[test]
fn the_oracle_catches_a_dropped_paint_never_asked_again() {
    control(Plant::NoRepaint);
}

#[test]
fn a_latch_of_a_frame_never_sent_is_refused_naming_the_slot() {
    let mut s = Slots::new(4);
    assert!(s.latched(4, 1).unwrap_err().contains("slot 1 of ring generation 4"));
    assert!(s.latched(4, 9).unwrap_err().contains("a ring has 3"));
    assert_eq!(s.latched(3, 1), Ok(After::Nothing), "an older ring's latch is ignored");
}
