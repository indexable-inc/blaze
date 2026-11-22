//! The lanes of a driver's file threads (thoughts.md 2.4): which thread set a file effect waits
//! for, as a sans-IO state machine the kqueue driver (`crate::kqueue`) drives and the
//! simulation below drives on a virtual clock.
//!
//! A file effect that waits on the drive (a flush, a barrier, a write-back, a durable write
//! or rename, a copy as long as its file) holds its thread for milliseconds to seconds. In one
//! lane, every thread can be holding one, and a rename or a read asked after them waits for the
//! first to end. Two lanes, each its own bounded thread set and queue, keep the fast effects off
//! the threads the drive holds: [`lane_of`] is the one classification, a `match` over every
//! effect with no wildcard, so a new effect is classed where it is added.
//!
//! The choice is the type parameter [`Split`] (rule 78): [`TwoLanes`] (the default) and
//! [`OneLane`] (every file effect in one lane, the driver before 2026-11-21, kept as the
//! tested control). Each lane has a cap of threads, of effects at its threads (queued or
//! running), and of effects in its backlog past those; an effect past all three is refused
//! with [`Full`], naming the lane and its caps, so no queue grows without a bound.
//!
//! The measure (rule 16): each effect's wait in its lane's queue, from the ask to a thread
//! taking it, summed and maxed by lane ([`Waits`]), and the longest wait of a fast effect asked
//! while a device effect was ahead of it in its lane, the stall two lanes remove. The waits are
//! the driver's to time (a thread takes the effect); the core only adds them up.
//!
//! Sans-IO: no clock, no thread, no allocation; every count is a fixed array.

#![blaze::sans_io]

use core::marker::PhantomData;

use crate::{
    Effect,
    planted::{NoPlant, Planted, Plants},
};

/// A lane of file threads, and the class of an effect by what it waits on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lane {
    /// What the page cache and the file system's metadata answer: reads, maps, writes into the
    /// page cache, renames with no sync, stats, listings.
    Fast,
    /// What waits on the drive: a sync, a barrier, a write-back, a durable write or rename (a
    /// sync of the file or its directory each), a copy or a prewarm as long as its file.
    Device,
}

/// The lanes, by [`Lane::index`].
pub const LANES: usize = 2;

impl Lane {
    /// Every lane, in index order.
    pub const ALL: [Lane; LANES] = [Lane::Fast, Lane::Device];

    pub const fn index(self) -> usize {
        match self {
            Lane::Fast => 0,
            Lane::Device => 1,
        }
    }

    /// Its name in a thread's name, a span or an error.
    pub const fn name(self) -> &'static str {
        match self {
            Lane::Fast => "fast",
            Lane::Device => "device",
        }
    }
}

/// The class of `effect`: [`Lane::Device`] where its call waits on the drive, [`Lane::Fast`]
/// for every other file effect, `None` for an effect no file thread runs (locks, timers,
/// watches, processes, sockets and the host's, which the driver keeps on its polling thread).
/// One `match` over every effect, with no wildcard arm: a new effect is classed here as it is
/// added. What each call does is in crate::fs (the module's `sync`, `barrier`, `write_back`,
/// `write_atomically`, `rename`, `copy_data`, `prewarm`).
pub fn lane_of(effect: &Effect) -> Option<Lane> {
    match effect {
        // `F_FULLFSYNC`, `F_BARRIERFSYNC`, `fsync` of a file or a directory.
        Effect::Sync { .. } | Effect::SyncDir { .. } | Effect::Barrier { .. } | Effect::WriteBack { .. } => {
            Some(Lane::Device)
        }
        // A durable write is a temporary file synced, then a rename and its directory synced;
        // a durable rename syncs the directories it touched.
        Effect::Write { .. } | Effect::Rename { .. } => Some(Lane::Device),
        // As long as the file: every byte read and written.
        Effect::CopyData { .. } | Effect::Prewarm { .. } => Some(Lane::Device),
        Effect::ReadAll { .. }
        | Effect::Read { .. }
        | Effect::ReadInto { .. }
        | Effect::ReadMany { .. }
        | Effect::WriteFrom { .. }
        | Effect::ReadLink { .. }
        | Effect::Append { .. }
        | Effect::Truncate { .. }
        | Effect::WriteUnsynced { .. }
        | Effect::WriteAt { .. }
        | Effect::RenameUnsynced { .. }
        | Effect::Mode { .. }
        | Effect::Seal { .. }
        | Effect::Private { .. }
        | Effect::Times { .. }
        // `clonefile`: the extents shared, no byte copied.
        | Effect::Clone { .. }
        | Effect::Link { .. }
        | Effect::Stat { .. }
        | Effect::Lstat { .. }
        | Effect::List { .. }
        | Effect::Remove { .. }
        | Effect::MakeDir { .. }
        | Effect::RemoveDir { .. }
        | Effect::Ancestors { .. }
        | Effect::Above { .. }
        | Effect::Stamp { .. }
        | Effect::Entries { .. }
        | Effect::Map { .. }
        | Effect::MapWrite { .. } => Some(Lane::Fast),
        Effect::Lock { .. }
        | Effect::Unlock { .. }
        | Effect::Timer { .. }
        | Effect::Watch { .. }
        | Effect::Journal { .. }
        | Effect::Unwatch { .. }
        | Effect::Flush { .. }
        | Effect::Spawn { .. }
        | Effect::Kill { .. }
        | Effect::Listen { .. }
        | Effect::Unlisten { .. }
        | Effect::Connect { .. }
        | Effect::Recv { .. }
        | Effect::Send { .. }
        | Effect::Hangup { .. }
        | Effect::Pressure { .. }
        | Effect::Bind { .. }
        | Effect::SendTo { .. }
        | Effect::RecvFrom { .. }
        | Effect::Unbind { .. }
        | Effect::Resolve { .. }
        | Effect::ReadStdin { .. }
        | Effect::WriteOut { .. }
        | Effect::Now { .. }
        | Effect::Random { .. }
        | Effect::Roots { .. } => None,
    }
}

/// The seam (rule 78): which lane an effect of each class waits in.
pub trait Split: Send + Sync + 'static {
    /// The lane an effect of `class` ([`lane_of`]) waits in.
    fn lane(class: Lane) -> Lane;
    /// Whether the [`Lane::Device`] lane is used: its threads, queue and channel are made only
    /// then.
    const TWO: bool;
}

/// Every file effect in one lane, [`Lane::Fast`]'s, its threads at the class of the thread that
/// made the driver: the driver before 2026-11-21, the tested control.
#[derive(Clone, Copy, Debug)]
pub struct OneLane;

impl Split for OneLane {
    fn lane(_class: Lane) -> Lane {
        Lane::Fast
    }
    const TWO: bool = false;
}

/// Each class in its own lane. The lane picks no QoS class: each effect runs at its asker's
/// (kqueue.rs `work_on`).
#[derive(Clone, Copy, Debug)]
pub struct TwoLanes;

impl Split for TwoLanes {
    fn lane(class: Lane) -> Lane {
        class
    }
    const TWO: bool = true;
}

/// A lane's caps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most threads it starts (at least one).
    pub threads: usize,
    /// The most effects at its threads at once, queued, running, or done and not yet taken: the
    /// room of the channel between the driver and them.
    pub at_threads: usize,
    /// The most effects waiting in its backlog past those.
    pub backlog: usize,
}

/// The planted bugs of [`Dispatch`], for its simulation's controls (rule 43).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LanePlant {
    /// The code as it is.
    None,
    /// A device effect queued in the fast lane: the classification's result ignored for it.
    DeviceOnFast,
    /// The backlog's cap not checked: an effect past it is taken.
    BacklogUnbounded,
}

impl Plants for LanePlant {}

/// An effect placed: the lane it waits in, its class, and whether a device effect was ahead of
/// it in that lane (queued, running, or in the backlog) when it was asked. It goes with the
/// effect to the thread and comes back with its completion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ticket {
    pub lane: Lane,
    pub class: Lane,
    pub behind_device: bool,
}

/// Where a placed effect goes now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Placed {
    /// Onto its lane's threads; with `start`, one more thread is started first (every one
    /// started is busy, and fewer than its cap are).
    Threads { start: bool },
    /// Into its lane's backlog, for [`Dispatch::promote`] once a completion makes room.
    Backlog,
}

/// An effect refused: its lane's threads and backlog are at their caps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Full {
    pub lane: Lane,
    pub limits: Limits,
}

impl core::fmt::Display for Full {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "the {} lane of the file threads is full: {} effects at its threads and {} in its backlog, its caps \
             (crate::threads::lanes); the asker submits fewer at once",
            self.lane.name(),
            self.limits.at_threads,
            self.limits.backlog
        )
    }
}

impl core::error::Error for Full {}

/// One lane's waits in its queue: how many effects a thread took, their waits summed, the
/// longest; and how many were refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LaneWaits {
    pub taken: u64,
    pub waited_ns: u64,
    pub most_ns: u64,
    pub refused: u64,
    /// The most threads it had started.
    pub threads: usize,
}

/// The lanes' waits ([`Dispatch::waits`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Waits {
    /// By [`Lane::index`].
    pub lanes: [LaneWaits; LANES],
    /// Fast effects asked while a device effect was ahead of them in their lane.
    pub fast_behind_device: u64,
    /// The longest wait of one of those.
    pub fast_behind_device_most_ns: u64,
}

#[derive(Clone, Copy, Debug)]
struct LaneState {
    limits: Limits,
    started: usize,
    at_threads: usize,
    backlog: usize,
    /// Device effects placed in this lane and not yet ended, wherever they are.
    device: usize,
}

/// The dispatcher: which lane each file effect waits in, when a lane starts a thread, when an
/// effect waits in a backlog or is refused, and the waits.
#[derive(Debug)]
pub struct Dispatch<S: Split, P: Planted<LanePlant> = NoPlant> {
    lanes: [LaneState; LANES],
    waits: Waits,
    plant: P,
    _split: PhantomData<S>,
}

impl<S: Split> Dispatch<S> {
    /// A dispatcher with each lane's caps, by [`Lane::index`].
    pub fn new(limits: [Limits; LANES]) -> Dispatch<S> {
        Dispatch::planted(limits, NoPlant)
    }
}

impl<S: Split, P: Planted<LanePlant>> Dispatch<S, P> {
    /// [`Dispatch::new`] with `plant`: the simulation's.
    pub fn planted(limits: [Limits; LANES], plant: P) -> Dispatch<S, P> {
        let lane = |limits: Limits| LaneState {
            limits: Limits { threads: limits.threads.max(1), ..limits },
            started: 0,
            at_threads: 0,
            backlog: 0,
            device: 0,
        };
        Dispatch { lanes: [lane(limits[0]), lane(limits[1])], waits: Waits::default(), plant, _split: PhantomData }
    }

    /// The lane an effect of `class` waits in.
    pub fn lane(&self, class: Lane) -> Lane {
        match class {
            Lane::Device if self.plant.has(LanePlant::DeviceOnFast) => Lane::Fast,
            class => S::lane(class),
        }
    }

    /// An effect of `class` asked: onto its lane's threads where they have room and its backlog
    /// is empty (so effects of a lane start in the order asked), else into its backlog, else
    /// refused.
    pub fn place(&mut self, class: Lane) -> Result<(Ticket, Placed), Full> {
        let lane = self.lane(class);
        let unbounded = self.plant.has(LanePlant::BacklogUnbounded);
        let state = &mut self.lanes[lane.index()];
        let behind_device = state.device > 0;
        let ticket = Ticket { lane, class, behind_device };
        let placed = if state.at_threads < state.limits.at_threads && state.backlog == 0 {
            Placed::Threads { start: Self::take_room(state) }
        } else if state.backlog < state.limits.backlog || unbounded {
            state.backlog += 1;
            Placed::Backlog
        } else {
            self.waits.lanes[lane.index()].refused += 1;
            return Err(Full { lane, limits: state.limits });
        };
        if class == Lane::Device {
            state.device += 1;
        }
        if class == Lane::Fast && behind_device {
            self.waits.fast_behind_device += 1;
        }
        let threads = state.started;
        let most = &mut self.waits.lanes[lane.index()].threads;
        *most = (*most).max(threads);
        Ok((ticket, placed))
    }

    /// Room at `state`'s threads taken for one effect: whether a thread is started for it.
    fn take_room(state: &mut LaneState) -> bool {
        let start = state.started <= state.at_threads && state.started < state.limits.threads;
        if start {
            state.started += 1;
        }
        state.at_threads += 1;
        start
    }

    /// The first effect of `lane`'s backlog onto its threads, where they have room: whether a
    /// thread is started for it. `None` with nothing in the backlog or no room.
    pub fn promote(&mut self, lane: Lane) -> Option<bool> {
        let state = &mut self.lanes[lane.index()];
        if state.backlog == 0 || state.at_threads >= state.limits.at_threads {
            return None;
        }
        state.backlog -= 1;
        let start = Self::take_room(state);
        let threads = state.started;
        let most = &mut self.waits.lanes[lane.index()].threads;
        *most = (*most).max(threads);
        Some(start)
    }

    /// The thread `placed` or [`Dispatch::promote`] said to start could not be: its room and
    /// the thread given back; the effect fails, naming why (the driver's).
    pub fn not_started(&mut self, ticket: Ticket) {
        let state = &mut self.lanes[ticket.lane.index()];
        state.started -= 1;
        state.at_threads -= 1;
        if ticket.class == Lane::Device {
            state.device -= 1;
        }
    }

    /// An effect's completion taken, the thread having taken it `waited_ns` after it was asked.
    pub fn ended(&mut self, ticket: Ticket, waited_ns: u64) {
        let state = &mut self.lanes[ticket.lane.index()];
        state.at_threads -= 1;
        if ticket.class == Lane::Device {
            state.device -= 1;
        }
        let waits = &mut self.waits.lanes[ticket.lane.index()];
        waits.taken += 1;
        waits.waited_ns = waits.waited_ns.saturating_add(waited_ns);
        waits.most_ns = waits.most_ns.max(waited_ns);
        if ticket.class == Lane::Fast && ticket.behind_device {
            self.waits.fast_behind_device_most_ns = self.waits.fast_behind_device_most_ns.max(waited_ns);
        }
    }

    /// The waits so far.
    pub fn waits(&self) -> Waits {
        self.waits
    }

    /// The threads `lane` has started.
    pub fn started(&self, lane: Lane) -> usize {
        self.lanes[lane.index()].started
    }

    /// Effects at `lane`'s threads and in its backlog.
    pub fn held(&self, lane: Lane) -> (usize, usize) {
        let state = &self.lanes[lane.index()];
        (state.at_threads, state.backlog)
    }
}

#[cfg(test)]
mod tests {
    //! The dispatcher on a virtual clock: each lane's queue a FIFO (the driver's channel), its
    //! threads each running one effect for the effect's time, a completion ending it and making
    //! room for the backlog, as `crate::kqueue` drives it.

    use std::collections::VecDeque;

    use blaze_sim::{Broke, Config, Tape};

    use super::*;

    /// 100 ms: a barrier's time in the simulation.
    const BARRIER_NS: u64 = 100_000_000;
    /// 1 µs: a rename's with no sync.
    const RENAME_NS: u64 = 1_000;

    #[derive(Clone, Copy, Debug)]
    struct Job {
        id: usize,
        ticket: Ticket,
        asked_ns: u64,
        takes_ns: u64,
    }

    /// What a job did: when it was asked, taken by a thread, and ended.
    #[derive(Clone, Copy, Debug, Default)]
    struct Ran {
        asked_ns: u64,
        taken_ns: u64,
        ended_ns: u64,
        lane: Option<Lane>,
        refused: bool,
    }

    struct Model<S: Split, P: Planted<LanePlant>> {
        dispatch: Dispatch<S, P>,
        now: u64,
        queues: [VecDeque<Job>; LANES],
        backlogs: [VecDeque<Job>; LANES],
        /// Each started thread: the job it runs and when that ends.
        threads: [Vec<Option<(Job, u64)>>; LANES],
        ran: Vec<Ran>,
    }

    impl<S: Split, P: Planted<LanePlant>> Model<S, P> {
        fn new(limits: [Limits; LANES], plant: P) -> Self {
            Model {
                dispatch: Dispatch::planted(limits, plant),
                now: 0,
                queues: [VecDeque::new(), VecDeque::new()],
                backlogs: [VecDeque::new(), VecDeque::new()],
                threads: [Vec::new(), Vec::new()],
                ran: Vec::new(),
            }
        }

        fn ask(&mut self, class: Lane, takes_ns: u64) -> usize {
            let id = self.ran.len();
            self.ran.push(Ran { asked_ns: self.now, ..Ran::default() });
            match self.dispatch.place(class) {
                Ok((ticket, placed)) => {
                    self.ran[id].lane = Some(ticket.lane);
                    let job = Job { id, ticket, asked_ns: self.now, takes_ns };
                    match placed {
                        Placed::Threads { start } => {
                            if start {
                                self.threads[ticket.lane.index()].push(None);
                            }
                            self.queues[ticket.lane.index()].push_back(job);
                        }
                        Placed::Backlog => self.backlogs[ticket.lane.index()].push_back(job),
                    }
                }
                Err(_) => self.ran[id].refused = true,
            }
            self.take();
            id
        }

        /// Each idle thread takes its lane's next queued job now.
        fn take(&mut self) {
            for lane in Lane::ALL {
                let i = lane.index();
                for slot in &mut self.threads[i] {
                    if slot.is_none()
                        && let Some(job) = self.queues[i].pop_front()
                    {
                        self.ran[job.id].taken_ns = self.now;
                        *slot = Some((job, self.now + job.takes_ns));
                    }
                }
            }
        }

        /// Time on to the next end; that job's completion taken. Whether one ended.
        fn step(&mut self) -> bool {
            let next = self.threads.iter().flatten().flatten().map(|(_, ends)| *ends).min();
            let Some(at) = next else { return false };
            self.now = at;
            for lane in Lane::ALL {
                let i = lane.index();
                for t in 0..self.threads[i].len() {
                    if let Some((job, ends)) = self.threads[i][t]
                        && ends == at
                    {
                        self.threads[i][t] = None;
                        self.ran[job.id].ended_ns = at;
                        self.dispatch.ended(job.ticket, self.ran[job.id].taken_ns - job.asked_ns);
                        while let Some(start) = self.dispatch.promote(lane) {
                            let job = self.backlogs[i].pop_front().expect("the core's backlog is the model's");
                            if start {
                                self.threads[i].push(None);
                            }
                            self.queues[i].push_back(job);
                        }
                    }
                }
            }
            self.take();
            true
        }

        fn run_out(&mut self) {
            while self.step() {}
        }
    }

    /// The case: `threads` threads a lane, `barriers` barriers asked at once (filling the
    /// device lane: at least as many as its threads), then a rename at a time drawn within the
    /// first barrier's, then some fast and device effects more.
    fn case<S: Split, P: Planted<LanePlant>>(tape: &Tape, plant: P) -> (Model<S, P>, usize, usize) {
        let threads = 1 + tape.below(4, "threads a lane") as usize;
        let barriers = threads + tape.below(8, "barriers past the threads") as usize;
        let limits = Limits { threads, at_threads: 64, backlog: 64 };
        let mut model = Model::<S, P>::new([limits, limits], plant);
        for _ in 0..barriers {
            model.ask(Lane::Device, BARRIER_NS);
        }
        model.now = tape.below(BARRIER_NS, "when the rename is asked");
        model.take();
        let rename = model.ask(Lane::Fast, RENAME_NS);
        for _ in 0..tape.below(6, "effects after") {
            let class = if tape.coin("a device effect") { Lane::Device } else { Lane::Fast };
            model.ask(class, 1 + tape.below(BARRIER_NS, "its time"));
        }
        model.run_out();
        (model, rename, barriers)
    }

    /// The oracle of two lanes: the rename waits for nothing and ends its own time after it was
    /// asked; no fast effect waited behind a device one; every effect ended; each lane's
    /// started threads within its cap.
    fn two_lanes(tape: &Tape, plant: LanePlant) -> Result<(), Broke> {
        let (model, rename, barriers) = case::<TwoLanes, LanePlant>(tape, plant);
        let ran = model.ran[rename];
        if ran.taken_ns != ran.asked_ns || ran.ended_ns != ran.asked_ns + RENAME_NS {
            return Err(Broke::Oracle(format!(
                "the rename waited behind {barriers} barriers: asked at {} ns, taken at {} ns, ended at {} ns ({ran:?})",
                ran.asked_ns, ran.taken_ns, ran.ended_ns
            )));
        }
        let waits = model.dispatch.waits();
        if waits.fast_behind_device != 0 {
            return Err(Broke::Oracle(format!("a fast effect waited behind a device one: {waits:?}")));
        }
        if let Some(lost) = model.ran.iter().position(|it| !it.refused && it.ended_ns == 0) {
            return Err(Broke::Harness(format!("effect {lost} never ended: {:?}", model.ran[lost])));
        }
        for lane in Lane::ALL {
            if model.threads[lane.index()].len() > 4 || model.dispatch.started(lane) != model.threads[lane.index()].len() {
                return Err(Broke::Oracle(format!("the {} lane started {} threads", lane.name(), model.dispatch.started(lane))));
            }
        }
        Ok(())
    }

    /// The invariant: fast-lane effects never wait behind device-lane barriers, and each lane's backlog stays within its configured caps.
    /// Expected: passes. N barriers of 100 ms fill the device lane; a rename asked after them
    /// is taken at once and ends in its own time.
    #[test]
    fn a_rename_asked_behind_barriers_waits_for_none_of_them() {
        blaze_sim::check(Config::cases(2000), |tape| two_lanes(tape, LanePlant::None));
    }

    /// Control (rule 43): a device effect put in the fast lane is caught.
    #[test]
    fn the_oracle_catches_a_device_effect_in_the_fast_lane() {
        blaze_sim::control(Config::cases(2000), LanePlant::None, LanePlant::DeviceOnFast, "waited behind", two_lanes);
    }

    /// Control (the cause removed, rule 23): in one lane the same rename waits for a barrier to
    /// end, and the measure sees it.
    #[test]
    fn in_one_lane_the_rename_waits_for_a_barrier() {
        blaze_sim::check(Config::cases(2000), |tape| {
            let (model, rename, barriers) = case::<OneLane, NoPlant>(tape, NoPlant);
            let ran = model.ran[rename];
            let waited = ran.taken_ns - ran.asked_ns;
            if waited < BARRIER_NS - ran.asked_ns {
                return Err(Broke::Oracle(format!("in one lane the rename behind {barriers} barriers waited {waited} ns")));
            }
            let waits = model.dispatch.waits();
            if waits.fast_behind_device == 0 || waits.fast_behind_device_most_ns < waited {
                return Err(Broke::Oracle(format!("the measure missed the rename's wait of {waited} ns: {waits:?}")));
            }
            Ok(())
        });
    }

    /// The oracle of the caps: past a lane's room at its threads and in its backlog, an effect
    /// is refused with `Full` naming the lane, and the counts do not move.
    fn caps(tape: &Tape, plant: LanePlant) -> Result<(), Broke> {
        let limits = Limits {
            threads: 1 + tape.below(3, "threads") as usize,
            at_threads: 1 + tape.below(6, "at the threads") as usize,
            backlog: tape.below(6, "in the backlog") as usize,
        };
        let mut dispatch = Dispatch::<TwoLanes, LanePlant>::planted([limits, limits], plant);
        let lane = if tape.coin("the device lane") { Lane::Device } else { Lane::Fast };
        for n in 0..limits.at_threads + limits.backlog {
            if dispatch.place(lane).is_err() {
                return Err(Broke::Oracle(format!("effect {n} refused within the caps {limits:?}")));
            }
        }
        let held = dispatch.held(lane);
        match dispatch.place(lane) {
            Err(Full { lane: full, .. }) if full == lane && dispatch.held(lane) == held => {}
            other => {
                return Err(Broke::Oracle(format!("past its caps {limits:?} the {} lane was not refused: {other:?}", lane.name())));
            }
        }
        if dispatch.started(lane) > limits.threads {
            return Err(Broke::Oracle(format!("{} threads started past the cap {limits:?}", dispatch.started(lane))));
        }
        Ok(())
    }

    /// Expected: passes. Each lane's queue is bounded: past its caps a typed refusal.
    #[test]
    fn a_lane_past_its_caps_refuses_naming_itself() {
        blaze_sim::check(Config::cases(500), |tape| caps(tape, LanePlant::None));
    }

    /// Control (rule 43): a backlog whose cap is not checked is caught.
    #[test]
    fn the_oracle_catches_an_unbounded_backlog() {
        blaze_sim::control(Config::cases(500), LanePlant::None, LanePlant::BacklogUnbounded, "was not refused", caps);
    }
}
