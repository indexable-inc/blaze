//! The pool's loader: a thread of its own that drives the loads core ([`super::Loads`]) on a
//! blaze-core driver of its own, and the blocking read a task makes through it
//! ([`Loader::pin`]) (docs/v2/plan/design/store-pool.md, "A miss").
//!
//! It is thin. Askers claim a stretch of pages ([`super::Reading`]), take a load slot off the free
//! list, push the load onto an `ArrayQueue` and wake the loader; the loader drains the asks,
//! steps the core, submits its reads, polls their completions and fires each ended load's event.
//! A task waits on its load's event (`blaze_graph::sched::Event`): a fiber switches out and
//! holds no thread, a thread on no fiber parks, and the wait is a span naming the pages
//! (rule `every-wait-says-what`). Each slot keeps one event for its life, armed again
//! ([`Event::rearm`]) as its load ends, so a miss claims no scheduler.
//!
//! A reader that finds a page another asker is loading reads the load's slot from the page's word
//! and waits on that slot's event: it takes the event's waiter, looks at the word again, and waits
//! only if the page is still `Loading` in that slot. The loader moves the words before it fires,
//! so a waiter that took the next arming finds the page moved and does not wait.
//!
//! The loader's own wake: the design asks for the driver's (kqueue's `EVFILT_USER` trigger, an
//! eventfd read on the ring). blaze-core's native drivers give no other thread a way to wake
//! their poll, and their poll returns at once with nothing in flight, so the loader parks on a
//! named [`Signal`] while it has no read in flight, and polls with a deadline [`TICK`] away while it
//! has: an ask that comes during a read waits at most that long, or until the next completion,
//! to be submitted. A driver wake in blaze-core takes the tick out.
//!
//! On Linux the reads go through the loader's own io_uring, around the page cache; on macOS
//! through its kqueue driver's file threads, `F_NOCACHE`.
//!
//! Prefetch: a plan's runs ([`Loader::prefetch`]) and the budget ([`Loader::set_budget`]) go to
//! the core on the loader's thread, which issues from the plan after every demand ask it holds
//! ([`super::Loads::pump`]). While the plan waits for room in its budget with nothing in flight,
//! the loader is parked; a reader that was the first to read a prefetched page, which makes that
//! room, wakes it ([`Shared::waiting`], one swap a park, and only on a first read).

#![allow(
    clippy::disallowed_types,
    reason = "the loader's locks are off every hit: one per load slot, held to copy an event's key or arm it again, and the idle park of a driver thread (rule deadline-waits); never across IO or a call"
)]

use std::{
    fmt::{self, Write as _},
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use blaze_core::{
    Cache, Driver,
    lock::{Lock, Signal},
    wait::Deadline,
};
use blaze_graph::{
    sched::{Event, EventWait},
    wait::{WaitOn, wait},
};
use blaze_spin::SpinWait;
use crossbeam_queue::{ArrayQueue, SegQueue};

use super::{
    Ended, Give, Load, LoadFailure, Loads, Opened, Pins, Pool, PoolError, ReadStep, Reading, Run, State, Window,
    loads::{DEMAND_SLOTS, LOADS, prefetch_slot},
};

/// How long the loader's poll waits, while reads are in flight, before it looks for new asks.
pub(crate) const TICK: Duration = Duration::from_micros(50);

/// Waits for a free load slot one read takes before it ends as [`PoolError::LoadsFull`]: each
/// wait ends as a load does, so this many is far past a full table that moves.
const ROOM_WAITS: u32 = 1 << 16;

/// A load slot: its event, armed for its next load, and how its last load failed.
struct Slot {
    event: Lock<Event>,
    failure: Lock<Option<LoadFailure>>,
}

/// What a planner tells the core of its prefetch.
enum PlanAsk {
    /// The runs to prefetch, in order, in place of those queued.
    Runs(Vec<Run>),
    /// The bytes prefetched and not yet read at most.
    Budget(u64),
}

/// A change to the files the core reads.
enum FileAsk {
    Open(Opened),
    Grow(Window, Vec<u32>),
    Close(u32),
}

struct Shared<G: Give> {
    pool: Pool<G>,
    asks: ArrayQueue<Load>,
    files: SegQueue<FileAsk>,
    plans: SegQueue<PlanAsk>,
    /// Whether the core's plan has runs it has not issued, as of the loader's last pass: set
    /// before the pass issues, and taken (swapped off) by the first reader to read a prefetched
    /// page after, who wakes the loader to issue into the room that read made.
    waiting: AtomicBool,
    free: ArrayQueue<u16>,
    slots: Box<[Slot]>,
    /// Fired as a slot comes back to the free list.
    room: Lock<Event>,
    /// Whether the loader is parked.
    idle: Lock<bool>,
    woken: Signal,
    stop: AtomicBool,
}

/// The pool and its loader's thread.
pub(crate) struct Loader<G: Give + Send + Sync + 'static> {
    shared: Arc<Shared<G>>,
    thread: Option<JoinHandle<()>>,
}

impl<G: Give + Send + Sync + 'static> Loader<G> {
    /// `pool`'s loader started on a thread of its own, its driver made there by `driver` (a
    /// native driver over the store's root, whose IO threads read for it), asking with tags
    /// `tags..tags + 2^32` and reading through `cache` (the store's go around it).
    pub(crate) fn start<D: Driver + 'static>(
        pool: Pool<G>,
        driver: impl FnOnce() -> io::Result<D> + Send + 'static,
        tags: u64,
        cache: Cache,
    ) -> io::Result<Loader<G>> {
        // The slots past them are the core's own, for prefetch.
        let free = ArrayQueue::new(DEMAND_SLOTS);
        for slot in 0..DEMAND_SLOTS as u16 {
            assert!(free.push(slot).is_ok(), "the free list has room for every load slot");
        }
        let slot = || Slot {
            event: Lock::new("store.pool.load", Event::new()),
            failure: Lock::new("store.pool.load.failure", None),
        };
        let shared = Arc::new(Shared {
            pool,
            asks: ArrayQueue::new(LOADS),
            files: SegQueue::new(),
            plans: SegQueue::new(),
            waiting: AtomicBool::new(false),
            free,
            slots: (0..LOADS).map(|_| slot()).collect(), // alloc: the table of slots, once
            room: Lock::new("store.pool.room", Event::new()),
            idle: Lock::new("store.pool.idle", false),
            woken: Signal::new("store.pool.asked"),
            stop: AtomicBool::new(false),
        });
        let theirs = Arc::clone(&shared);
        let (opened, open) = crossbeam_channel::bounded::<io::Result<()>>(1);
        let thread = std::thread::Builder::new()
            .name("store pool loader".to_owned()) // alloc: the thread's name, once
            .spawn(move || {
                let driver = match driver() {
                    Ok(driver) => {
                        _ = opened.send(Ok(()));
                        driver
                    }
                    Err(err) => {
                        _ = opened.send(Err(err));
                        return;
                    }
                };
                run(&theirs, driver, tags, cache);
            })?;
        let made = wait(WaitOn::Channel("store pool loader's driver made"), None, || open.recv());
        match made {
            Ok(Ok(())) => Ok(Loader { shared, thread: Some(thread) }),
            Ok(Err(err)) => {
                _ = thread.join();
                // alloc: an error path
                Err(io::Error::new(err.kind(), format!("the store pool loader's driver: {err}")))
            }
            Err(_) => {
                _ = thread.join();
                Err(io::Error::other("the store pool loader's thread ended before its driver was made"))
            }
        }
    }

    pub(crate) fn pool(&self) -> &Pool<G> {
        &self.shared.pool
    }

    /// The file `opened` readable through the loads core from now on: asked before any load of
    /// it, so the core takes it first.
    pub(crate) fn open(&self, opened: Opened) {
        self.shared.files.push(FileAsk::Open(opened));
        self.wake();
    }

    /// The file at `window.first` committed more ([`super::Append::commit`]), `checks` those of its
    /// new blocks: asked before the commit publishes the new length to readers, so the core
    /// takes it before any load past the old one.
    pub(crate) fn grow(&self, window: Window, checks: Vec<u32>) {
        self.shared.files.push(FileAsk::Grow(window, checks));
        self.wake();
    }

    /// The file at window `first` read no more: after [`super::Windows::retire`] took its pages.
    pub(crate) fn close(&self, first: u32) {
        self.shared.files.push(FileAsk::Close(first));
        self.wake();
    }

    /// A plan's runs to prefetch, in the order to read them, in place of those queued
    /// ([`super::prefetch::plan`]).
    pub(crate) fn prefetch(&self, runs: Vec<Run>) {
        self.shared.plans.push(PlanAsk::Runs(runs));
        self.wake();
    }

    /// The bytes prefetched and not yet read at most: the smaller of an eighth of the ceiling and
    /// the room the plane last published. None until it is told.
    pub(crate) fn set_budget(&self, bytes: u64) {
        self.shared.plans.push(PlanAsk::Budget(bytes));
        self.wake();
    }

    /// The `len` bytes at the pool's byte `at` pinned, loaded first where they are not in memory:
    /// the calling task yields on each load (a fiber switches out; a thread parks), never blocking
    /// a worker in the kernel.
    ///
    /// It runs its reading to its end: every load it asked has ended by the time it returns, so
    /// no pin or slot is left behind.
    pub(crate) fn pin(&self, at: u64, len: u64) -> Result<Pins<G, &Pool<G>>, PoolError> {
        let pool = &self.shared.pool;
        let mut reading = Reading::new(at, len);
        let mut spin = SpinWait::new();
        let result = loop {
            match reading.step(&pool) {
                ReadStep::Pinned(pins) => break Ok(pins),
                ReadStep::Failed(error) => break Err(error),
                ReadStep::NeedSlot => match self.slot() {
                    Ok(slot) => reading.give_slot(slot),
                    Err(error) => {
                        reading.give_up(pool);
                        break Err(error);
                    }
                },
                ReadStep::Ask(load) => {
                    let outcome = self.load(load);
                    reading.loaded(pool, outcome);
                }
                ReadStep::Pending => unreachable!("a reading's load is waited for as it is asked"),
                ReadStep::Wait { page, slot } => self.wait_on(page, slot),
                ReadStep::Spin { .. } => {
                    // The sweep's batch takes microseconds; the reading bounds how often.
                    if !spin.spin() {
                        spin = SpinWait::new();
                    }
                }
            }
        };
        if let Some(slot) = reading.take_slot() {
            self.free_slot(slot);
        }
        // Its first read of a prefetched page made room in the prefetch budget: the loader, if
        // its plan waits for room, issues the next. One reader a park takes the flag.
        let shared = &self.shared;
        if reading.prefetched() > 0 && shared.waiting.load(Ordering::Relaxed) && shared.waiting.swap(false, Ordering::AcqRel)
        {
            self.wake();
        }
        result
    }

    /// A free load slot, waiting (named, bounded) while every one is taken.
    fn slot(&self) -> Result<u16, PoolError> {
        for _ in 0..ROOM_WAITS {
            if let Some(slot) = self.shared.free.pop() {
                return Ok(slot);
            }
            let room = self.shared.room.lock().waiter();
            // A slot freed before the waiter was taken fired an arming this waiter is not of.
            if let Some(slot) = self.shared.free.pop() {
                return Ok(slot);
            }
            wait_for("store pool: loads full", room);
        }
        Err(PoolError::LoadsFull { waits: ROOM_WAITS })
    }

    fn free_slot(&self, slot: u16) {
        assert!(self.shared.free.push(slot).is_ok(), "load slot {slot} of the store's pool freed twice");
        self.shared.room.lock().rearm();
    }

    /// `load` asked of the loader and waited for: how it ended.
    fn load(&self, load: Load) -> Result<(), LoadFailure> {
        let slot = &self.shared.slots[load.slot as usize];
        // Armed for this load: the slot came off the free list after its last load's end armed
        // it again, and only this load's end fires it.
        let done = slot.event.lock().waiter();
        assert!(
            self.shared.asks.push(load).is_ok(),
            "the store pool's asks are full with load slot {} taken: an ask without a slot",
            load.slot
        );
        self.wake();
        let mut name = Name::new();
        _ = write!(name, "store pool pages {}..{}", load.first, load.first + u64::from(load.pages));
        wait_for(name.as_str(), done);
        slot.failure.lock().take().map_or(Ok(()), Err)
    }

    /// Waits for the load in `slot` another asker made, if `page` is still in it.
    fn wait_on(&self, page: u64, slot: u16) {
        let done = self.shared.slots[slot as usize].event.lock().waiter();
        let word = self.shared.pool.word_now(page);
        if word.state() == State::Loading && word.slot() == slot {
            let mut name = Name::new();
            _ = write!(name, "store pool page {page}, loading in slot {slot}");
            wait_for(name.as_str(), done);
        }
    }

    fn wake(&self) {
        wake(&self.shared);
    }
}

impl<G: Give + Send + Sync + 'static> Drop for Loader<G> {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.wake();
        if let Some(thread) = self.thread.take() {
            let joined = wait(WaitOn::Channel("store pool loader ends its reads"), None, || thread.join());
            if joined.is_err() {
                tracing::error!("the store pool's loader thread panicked");
            }
        }
    }
}

/// Waits on `on`, a span naming `what`.
fn wait_for(what: &str, on: EventWait) {
    if let Err(cycle) = wait(WaitOn::Io(what), None, || on.wait()) {
        panic!("a wait for {what} closed a cycle, which no wait on an event can: {cycle:?}");
    }
}

fn wake<G: Give>(shared: &Shared<G>) {
    let idle = shared.idle.lock();
    if *idle {
        shared.woken.notify_one();
    }
}

/// The loader's loop: until it is stopped with nothing in flight.
fn run<G: Give, D: Driver>(shared: &Shared<G>, mut driver: D, tags: u64, cache: Cache) {
    let mut loads = Loads::new(tags, cache);
    let mut effects = Vec::new(); // alloc: the loader's, once, kept
    let mut out = Vec::new(); // alloc: the loader's, once, kept
    let mut ended = Vec::new(); // alloc: the loader's, once, kept
    // A driver thread's service loop: it ends when stopped, and every pass either submits,
    // completes or parks until an ask comes.
    loop {
        take_files(shared, &mut loads);
        while let Some(plan) = shared.plans.pop() {
            match plan {
                PlanAsk::Runs(runs) => loads.prefetch(&shared.pool, runs),
                PlanAsk::Budget(bytes) => loads.set_budget(bytes),
            }
        }
        if shared.stop.load(Ordering::Acquire) {
            // Stopping: nothing more is prefetched.
            loads.prefetch(&shared.pool, std::iter::empty());
        }
        while let Some(load) = shared.asks.pop() {
            // An ask can be seen before the file it reads, pushed first on another queue.
            if !loads.knows(load.first) {
                take_files(shared, &mut loads);
            }
            loads.ask(&shared.pool, load, &mut effects, &mut ended);
        }
        // Said before the pump reads the counts: a first read after this wakes the loader, so
        // none that makes room is missed between the pump and the park.
        let queued = loads.queued() > 0;
        shared.waiting.store(queued, Ordering::Release);
        // After every demand ask: a prefetch is never ahead of a demand read in a submission.
        loads.pump(&shared.pool, &mut effects, &mut ended);
        driver.submit(&mut effects);
        tell(shared, &mut ended);
        if driver.in_flight() == 0 {
            if shared.stop.load(Ordering::Acquire) && shared.asks.is_empty() {
                return;
            }
            park(shared, queued);
            continue;
        }
        driver.poll(&mut out, Deadline::after(driver.now(), TICK));
        for completion in out.drain(..) {
            debug_assert!(loads.owns(completion.tag), "the store pool's driver told of {completion:?}");
            if loads.owns(completion.tag) {
                loads.complete(&shared.pool, completion, &mut ended);
            }
        }
        tell(shared, &mut ended);
    }
}

fn take_files<G: Give>(shared: &Shared<G>, loads: &mut Loads) {
    while let Some(ask) = shared.files.pop() {
        match ask {
            FileAsk::Open(opened) => loads.open(opened),
            FileAsk::Grow(window, checks) => loads.grow(window, checks),
            FileAsk::Close(first) => loads.close(first),
        }
    }
}

/// Each ended load's failure kept for its asker, then its event fired and armed again.
fn tell<G: Give>(shared: &Shared<G>, ended: &mut Vec<Ended>) {
    for Ended { slot: number, outcome } in ended.drain(..) {
        let slot = &shared.slots[number as usize];
        // A prefetch has no asker to take its failure: its pages are unloaded again, the core
        // counted it, and a reader that waited on it looks at the page again and loads it.
        if !prefetch_slot(number) {
            *slot.failure.lock() = outcome.err();
        }
        slot.event.lock().rearm();
    }
}

/// Parks the loader until an ask, a file, a plan or the stop comes; or, when its plan has runs
/// it has not issued (`queued`), until a reader takes [`Shared::waiting`] for a first read of a
/// prefetched page.
fn park<G: Give>(shared: &Shared<G>, queued: bool) {
    let mut idle = shared.idle.lock();
    while shared.asks.is_empty()
        && shared.files.is_empty()
        && shared.plans.is_empty()
        && !shared.stop.load(Ordering::Acquire)
        && !(queued && !shared.waiting.load(Ordering::Acquire))
    {
        *idle = true;
        shared.woken.wait(&mut idle);
    }
    *idle = false;
}

/// A wait's name, written on the stack: a miss allocates nothing to say what it waits for.
struct Name {
    bytes: [u8; 96],
    len: usize,
}

impl Name {
    fn new() -> Name {
        Name { bytes: [0; 96], len: 0 }
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).unwrap_or("store pool load")
    }
}

impl fmt::Write for Name {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut take = s.len().min(self.bytes.len() - self.len);
        while !s.is_char_boundary(take) {
            take -= 1;
        }
        self.bytes[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}
