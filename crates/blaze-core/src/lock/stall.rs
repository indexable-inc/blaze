//! The wait-for graph's analysis, a sans-IO core: given what every waiting thread is blocked
//! on and who holds it (a [`Snapshot`], which [`super::graph`] takes), it says which waits
//! that are older than a threshold are a deadlock (a cycle of threads each waiting on a lock
//! the next holds) and which a convoy (a chain that ends at one head: a thread running, or
//! waiting on something no thread holds, such as IO). It reads no clock; `now_ns` is given.
//!
//! Each thread waits on at most one thing, so the graph is functional (one edge out of each
//! waiting thread, to the holder of what it waits on), and a walk from any waiter either
//! returns to a thread it passed (the cycle) or ends at a head.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

use blaze_hash::HashMap;

use super::Mode;

/// What a waiting thread is blocked on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Blocked {
    /// A named lock, with the address that tells two locks of one name apart.
    Lock { name: &'static str, mode: Mode, at: usize },
    /// A [`super::Signal`]'s notify.
    Signal(&'static str),
    /// A kernel wait through [`crate::wait`] (an io_uring request, a child, a descriptor), by
    /// the `what` it was given.
    Wait(&'static str),
}

impl Blocked {
    /// As a report says it: `lock store.packs`, `signal pool.idle`, `wait uring_enter`.
    pub fn said(&self) -> (&'static str, &'static str) {
        match *self {
            Blocked::Lock { name, .. } => ("lock", name),
            Blocked::Signal(name) => ("signal", name),
            Blocked::Wait(what) => ("wait", what),
        }
    }
}

/// One thread's wait, as the snapshot took it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitRow {
    /// The thread, by [`super::thread_mark`]: what a lock's holder is stamped with.
    pub thread: u64,
    /// Its span when the wait began (the probe's), 0 for none.
    pub span: u64,
    /// When the wait began, on the probe's clock.
    pub since_ns: u64,
    pub on: Blocked,
    /// Who holds what it waits on, by thread mark, when that is a lock someone holds.
    pub holder: Option<u64>,
    /// Where it asked (a lock's call site), when known.
    pub site: Option<&'static std::panic::Location<'static>>,
}

/// Every wait in progress at one moment.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub waits: Vec<WaitRow>,
}

/// What a stall is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stall {
    /// A cycle: each wait's holder is the next wait's thread, the last's the first's. Each
    /// row names a thread, what it waits on and (as the previous row's `on`) what it holds.
    Deadlock { cycle: Vec<WaitRow> },
    /// A chain ending at `head`, which holds what the first link waits on and is not itself
    /// waiting on a lock another holds: `head_wait` is its own wait when it has one (IO, a
    /// signal), `None` when it is running. `waiters` counts every wait whose chain ends at it.
    Convoy {
        head: u64,
        head_wait: Option<WaitRow>,
        /// What the head holds that the waiters directly behind it wait on.
        holds: Blocked,
        waiters: usize,
        /// The oldest wait behind it.
        oldest: WaitRow,
    },
}

impl Stall {
    /// One line, for `blaze status` and the build view.
    pub fn line(&self, now_ns: u64) -> String {
        let ms = |row: &WaitRow| now_ns.saturating_sub(row.since_ns) / 1_000_000;
        match self {
            Stall::Deadlock { cycle } => {
                let mut out = String::from("deadlock:"); // alloc: a stall report, rare
                for row in cycle {
                    let (kind, name) = row.on.said();
                    out.push_str(&format!(
                        " thread {} waits {kind} {name} ({} ms);",
                        row.thread,
                        ms(row)
                    ));
                }
                out
            }
            Stall::Convoy { head, head_wait, holds, waiters, oldest } => {
                let (kind, name) = holds.said();
                let doing = match head_wait {
                    None => String::from("running"), // alloc: a stall report, rare
                    Some(it) => {
                        let (kind, name) = it.on.said();
                        format!("in {kind} {name} for {} ms", ms(it))
                    }
                };
                format!(
                    "convoy: {waiters} behind thread {head} on {kind} {name} (oldest {} ms); the head is {doing}",
                    ms(oldest)
                )
            }
        }
    }
}

/// The stalls among `snapshot`'s waits older than `threshold_ns` at `now_ns`: each deadlock
/// once (by its cycle's threads), each convoy once (by its head), deadlocks first, then
/// convoys by how many wait behind them, most first.
pub fn analyze(snapshot: &Snapshot, now_ns: u64, threshold_ns: u64) -> Vec<Stall> {
    // alloc: a watchdog's check, once a threshold passes.
    let mut by_thread: HashMap<u64, usize, Global> = HashMap::new_in(Global);
    for (i, row) in snapshot.waits.iter().enumerate() {
        by_thread.insert(row.thread, i);
    }
    let old = |row: &WaitRow| now_ns.saturating_sub(row.since_ns) >= threshold_ns;
    // alloc: as above.
    let mut stalls: Vec<Stall> = Vec::new();
    let mut seen_cycles: Vec<Vec<u64>> = Vec::new(); // alloc: as above
    // head -> (index of the stall in `stalls`)
    let mut convoys: HashMap<u64, usize, Global> = HashMap::new_in(Global);
    for row in &snapshot.waits {
        // The walk from this wait, bounded by the number of waits (`bounded-progress`).
        let mut path: Vec<usize> = Vec::new(); // alloc: as above
        let mut at = by_thread[&row.thread];
        let mut end = None;
        for _ in 0..=snapshot.waits.len() {
            if let Some(start) = path.iter().position(|&it| it == at) {
                end = Some(Err(start));
                break;
            }
            path.push(at);
            let here = &snapshot.waits[at];
            match here.holder.and_then(|holder| by_thread.get(&holder).map(|&i| (holder, i))) {
                Some((_, next)) => at = next,
                None => {
                    end = Some(Ok(here.holder));
                    break;
                }
            }
        }
        match end {
            Some(Err(start)) => {
                let cycle: Vec<WaitRow> =
                    path[start..].iter().map(|&i| snapshot.waits[i]).collect(); // alloc: as above
                if !cycle.iter().any(old) {
                    continue;
                }
                let mut key: Vec<u64> = cycle.iter().map(|it| it.thread).collect(); // alloc: as above
                key.sort_unstable();
                if !seen_cycles.contains(&key) {
                    seen_cycles.push(key);
                    stalls.push(Stall::Deadlock { cycle });
                }
            }
            Some(Ok(holder)) => {
                // The head: a holder that is not waiting, or the last thread on the path when
                // it waits on something no thread holds (IO, a signal). A wait with no holder
                // of its own is no convoy.
                let (head, holds) = match holder {
                    Some(head) => (head, snapshot.waits[path[path.len() - 1]].on),
                    None if path.len() >= 2 => (
                        snapshot.waits[path[path.len() - 1]].thread,
                        snapshot.waits[path[path.len() - 2]].on,
                    ),
                    None => continue,
                };
                if !old(row) {
                    continue;
                }
                match convoys.get(&head) {
                    Some(&i) => {
                        if let Stall::Convoy { waiters, oldest, .. } = &mut stalls[i] {
                            *waiters += 1;
                            if row.since_ns < oldest.since_ns {
                                *oldest = *row;
                            }
                        }
                    }
                    None => {
                        convoys.insert(head, stalls.len());
                        stalls.push(Stall::Convoy {
                            head,
                            head_wait: None,
                            holds,
                            waiters: 1,
                            oldest: *row,
                        });
                    }
                }
            }
            // The walk ran out of fuel: it cannot, each step visits a new wait.
            None => {}
        }
    }
    // A head that waits itself (on IO, a signal) says so.
    for stall in &mut stalls {
        if let Stall::Convoy { head, head_wait, .. } = stall
            && let Some(&i) = by_thread.get(head)
        {
            *head_wait = Some(snapshot.waits[i]);
        }
    }
    stalls.sort_by_key(|it| match it {
        Stall::Deadlock { .. } => (0, 0),
        Stall::Convoy { waiters, .. } => (1, usize::MAX - waiters),
    });
    stalls
}
