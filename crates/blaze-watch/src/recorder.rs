//! [`Recorder`]: the flight recorder's decisions. The recording itself is the emitter's rings
//! ([`crate::ring`], "The flight recorder"): every thread's last `len` entries are always in
//! memory, so nothing is copied until a recording is saved. This machine, fed the entries the
//! process's drain thread reads and its timer's ticks, says when to sample a stack and when to
//! save, and which saved recordings to remove.
//!
//! - A wait open past `stall` (150 ms) has its thread's stack sampled once ([`Effect::Sample`]):
//!   what the thread was doing, named when the recording is written.
//! - A wait that ends after more than `hitch` (1 s), a wait still open past `hitch` (a hang:
//!   it may never end), or a panic saves a recording reaching back `reach` (60 s), at most once
//!   a `gap` (30 s) for the waits; a panic always saves.
//! - The automatic recordings kept are the newest `keep` (12); a person's own save is theirs and
//!   never removed.
//!
//! Waits are the events rule 15 makes every blocking wait emit ([`kind::WAIT_BEGAN`] and
//! [`kind::WAIT_ENDED`], by wait id): the recorder needs no hook of its own.

// adapted from makepad/relax (MIT), libs/relax/compiler/src/inspect/flightrec.rs (STALL_NS,
// DUMP_MS, DUMP_GAP_NS, HITCH_LAST_NS, AUTO_KEEP: the same bounds); changed: waits come from the
// rings' own events, a hang that never ends saves too, and every decision is a machine's effect.

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{Vec, VecDeque, soa};

use crate::Nanos;
use crate::ring::{Event, Lane, kind};

/// The recorder's bounds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// A wait open this long has its thread's stack sampled.
    pub stall: Nanos,
    /// A wait longer than this saves a recording.
    pub hitch: Nanos,
    /// Automatic saves for waits at most this often.
    pub gap: Nanos,
    /// How far back a recording reaches.
    pub reach: Nanos,
    /// Automatic recordings kept.
    pub keep: usize,
    /// Waits open at once that are tracked; past it the recorder refuses by name.
    pub most_waits: usize,
}

impl Config {
    pub const DEFAULT: Config = Config {
        stall: Nanos::millis(150),
        hitch: Nanos::secs(1),
        gap: Nanos::secs(30),
        reach: Nanos::secs(60),
        keep: 12,
        most_waits: 4096,
    };
}

/// A saved recording, by number: the driver names its file `auto-<n>.bwr` (or the person's
/// name for an asked save) in the run home's `flight/`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Recording(pub u64);

/// Why a recording was saved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reason {
    /// A wait of class `class` took `took`.
    Hitch { class: u32, took: Nanos },
    /// A wait of class `class` has been open `open` and has not ended.
    Hang { class: u32, open: Nanos },
    Panic,
    /// A person asked (`blaze` from the viewer's page, or a command).
    Asked,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Sample the stack of `lane`'s thread, which has been in wait `wait` past the stall bound.
    Sample { lane: Lane, wait: u64 },
    /// Read every lane's ring from `from` to `to` and write it ([`crate::pack`]) as `name`.
    Save { name: Recording, reason: Reason, from: Nanos, to: Nanos },
    /// Remove the automatic recording `name`: newer ones fill the quota.
    Remove { name: Recording },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// More waits open at once than the recorder tracks.
    Waits { most: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::Waits { most } => {
                write!(f, "the flight recorder tracks {most} open waits and one more began")
            }
        }
    }
}

impl core::error::Error for Error {}

/// What has been done about one open wait.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Seen {
    Nothing,
    Sampled,
    /// Sampled, and saved as a hang.
    Saved,
}

soa! {
    /// One wait open now.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Wait, Waits, WaitRef {
        pub lane: Lane => lanes / lanes_mut,
        pub id: u64 => ids / ids_mut,
        pub class: u32 => classes / classes_mut,
        pub began: Nanos => begans / begans_mut,
        pub seen: Seen => seens / seens_mut,
    }
}

pub struct Recorder<A: Allocator> {
    config: Config,
    waits: Waits<A>,
    kept: VecDeque<Recording, A>,
    next: Recording,
    last_auto: Option<Nanos>,
    /// Automatic saves not made because one was made less than a gap before.
    pub skipped: u64,
}

impl<A: Allocator + Clone> Recorder<A> {
    /// `existing`: the automatic recordings already saved (by an earlier process of this home),
    /// oldest first, so the quota holds across processes.
    pub fn new_in<B: Allocator>(
        config: Config,
        existing: &[Recording],
        out: &mut Vec<Effect, B>,
        alloc: A,
    ) -> Recorder<A> {
        let mut kept = VecDeque::with_capacity_in(config.keep + 1, alloc.clone());
        let mut next = Recording(0);
        for name in existing {
            kept.push_back(*name);
            next = Recording(next.0.max(name.0 + 1));
        }
        let mut recorder = Recorder {
            config,
            waits: Waits::new_in(alloc),
            kept,
            next,
            last_auto: None,
            skipped: 0,
        };
        recorder.trim(out);
        recorder
    }
}

impl<A: Allocator> Recorder<A> {
    fn trim<B: Allocator>(&mut self, out: &mut Vec<Effect, B>) {
        while self.kept.len() > self.config.keep {
            if let Some(name) = self.kept.pop_front() {
                out.push(Effect::Remove { name });
            }
        }
    }

    fn save<B: Allocator>(&mut self, reason: Reason, at: Nanos, out: &mut Vec<Effect, B>) {
        let name = self.next;
        self.next = Recording(name.0 + 1);
        out.push(Effect::Save { name, reason, from: Nanos(at.0.saturating_sub(self.config.reach.0)), to: at });
        if reason != Reason::Asked {
            self.kept.push_back(name);
            self.trim(out);
        }
    }

    /// An automatic save for a wait, unless one was made less than a gap ago.
    fn save_for_wait<B: Allocator>(&mut self, reason: Reason, at: Nanos, out: &mut Vec<Effect, B>) -> bool {
        if let Some(last) = self.last_auto
            && at.since(last) < self.config.gap
        {
            self.skipped += 1;
            return false;
        }
        self.last_auto = Some(at);
        self.save(reason, at, out);
        true
    }

    fn find(&self, lane: Lane, id: u64) -> Option<usize> {
        let lanes = self.waits.lanes();
        let ids = self.waits.ids();
        (0..lanes.len()).find(|&row| lanes[row] == lane && ids[row] == id)
    }

    /// One entry a drain read from `lane`.
    pub fn drained<B: Allocator>(
        &mut self,
        lane: Lane,
        event: &Event,
        out: &mut Vec<Effect, B>,
    ) -> Result<(), Error> {
        let at = Nanos(event.at);
        match event.kind {
            kind::WAIT_BEGAN => {
                if self.waits.len() >= self.config.most_waits {
                    return Err(Error::Waits { most: self.config.most_waits });
                }
                self.waits.push(Wait { lane, id: event.value, class: event.detail, began: at, seen: Seen::Nothing });
            }
            kind::WAIT_ENDED => {
                // A wait whose beginning the ring lost before the drain read it is not tracked.
                if let Some(row) = self.find(lane, event.value) {
                    let wait = self.waits.swap_remove(row);
                    let took = at.since(wait.began);
                    if took > self.config.hitch && wait.seen != Seen::Saved {
                        self.save_for_wait(Reason::Hitch { class: wait.class, took }, at, out);
                    }
                }
            }
            kind::PANIC => self.save(Reason::Panic, at, out),
            _ => {}
        }
        Ok(())
    }

    /// The timer: sample what has stalled, save what hangs.
    pub fn tick<B: Allocator>(&mut self, now: Nanos, out: &mut Vec<Effect, B>) {
        for row in 0..self.waits.len() {
            let open = now.since(self.waits.begans()[row]);
            let seen = self.waits.seens()[row];
            if seen == Seen::Nothing && open > self.config.stall {
                out.push(Effect::Sample { lane: self.waits.lanes()[row], wait: self.waits.ids()[row] });
                self.waits.seens_mut()[row] = Seen::Sampled;
            }
            if self.waits.seens()[row] == Seen::Sampled && open > self.config.hitch {
                let class = self.waits.classes()[row];
                if self.save_for_wait(Reason::Hang { class, open }, now, out) {
                    self.waits.seens_mut()[row] = Seen::Saved;
                }
            }
        }
    }

    /// A person asked for a recording of the last `reach`.
    pub fn asked<B: Allocator>(&mut self, now: Nanos, out: &mut Vec<Effect, B>) {
        self.save(Reason::Asked, now, out);
    }

    pub fn open_waits(&self) -> usize {
        self.waits.len()
    }
}
