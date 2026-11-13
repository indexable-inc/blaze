//! The machine's ledger, its driver (docs/v2/plan/design/end-state/memory.md, section 4.5): the
//! file `/tmp/blaze-<uid>/machine.<layout>` ([`blaze_graph::ledger::file_name`]), mapped shared by
//! every blaze process of the user on the host whatever its `BLAZE_HOME` (two homes are one
//! machine), in which each process holds one slot and counts its memory and its work threads'
//! cores. The core, [`blaze_graph::ledger`], does the arithmetic one atomic access a step; this
//! maps the file ([`blaze_core::shared::Shared`]), says which pids run and since when, sleeps and
//! wakes on its epoch word across processes, and runs each operation to its end.
//!
//! There are no turns, no leases, no allowances and no yield targets. A process joins as it
//! starts ([`Machine::join`]: a slot claimed, the machine budget narrowed to what this process
//! was told or computed), reserves what its gate ([`blaze_mem::budget::Budget`]) lets its tasks
//! commit as they need it and gives it back as they end ([`Machine::fit`]), takes and gives its
//! pools' core tokens ([`Seat`]'s [`blaze_graph::ledger::Tokens`]), and leaves as it drops.
//! One killed leaves its slot for the next sum of any other process to free, by its pid and start.
//!
//! A ledger that cannot be mapped, or a slot that is no longer this process's, stops the process
//! with an error naming the file: never a budget of its own in its place (no-fallback).

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{Context as _, bail};
use blaze_core::shared::Shared;
use blaze_graph::{
    Budget,
    ledger::{
        self, Alive, Census, Cores, Counted, EPOCH, GiveCores, Join, Leave, MachineError,
        Process, Release, Reserve, Reserved, Slot, TakeCores, Tokens, Unseen, WORDS,
    },
};

/// The variable that sets the machine budget, in GiB, as [`super::MEMORY_VAR`] sets one daemon's
/// ceiling.
pub(crate) const MACHINE_VAR: &str = "BLAZE_MACHINE_MEMORY_GB";

/// What the machine budget leaves for the kernel and the page cache, past the quarter its 3/4
/// already does, as the daemon's own budget does ([`blaze_graph::arena::machine_budget`]).
const MARGIN: u64 = 1 << 30;

/// The grain a process's slot moves by ([`Machine::fit`]): 64 MiB, 32 of the gate's quanta.
/// Not a lease: nothing is granted against it without its add and sum. It only keeps the shared
/// words from moving on every 2 MiB quantum the gate grants: the floor of a reservation is one
/// add and one pass of the 64 slots (about 200 loads, ~1 us), so at this grain the ledger costs
/// one such pass per 64 MiB a process's tasks commit, under 0.01% of the time to touch them.
pub(crate) const GRAIN: u64 = 64 << 20;

/// The process's seat in the ledger: the mapping and its slot, for the process's life (leaked
/// once at its start, as the pools' [`Tokens`] need it `'static`).
pub struct Seat {
    shared: Shared,
    slot: Slot,
    /// The process left: its slot no longer counts anything, and a give is a no-op.
    left: AtomicBool,
}

/// This process's place in the ledger. Dropping it leaves.
pub struct Machine {
    seat: &'static Seat,
    /// The machine budget this process brought.
    budget: u64,
    /// The bytes this process's slot holds now: what its gate's room is.
    held: u64,
    /// What the start says of the machine budget.
    said: String,
    /// Who else holds the machine's memory, as the last census read the ledger, for a build's
    /// paused line ([`Machine::says`]); empty while no other process holds any.
    says: String,
}

/// What a process joined with: its place, and the ceiling its gate admits under.
pub(crate) struct Joined {
    pub(crate) machine: Machine,
    /// The most this process may commit: what it was told (`--memory-gb`,
    /// [`super::MEMORY_VAR`]) or the machine's limit for one process, never above the machine
    /// budget.
    pub(crate) ceiling: Budget,
}

impl Machine {
    /// Joins the ledger as this process: the file mapped (made, by the first process, stamped
    /// with its budget and cores), a slot claimed, the machine budget narrowed to what this
    /// process was told (`told`, [`MACHINE_VAR`]) or computed, and the ceiling (`told_ceiling`,
    /// as [`blaze_graph::arena::machine_budget`] takes it) cut to it. The process's core tokens
    /// are registered for its pools ([`blaze_graph::pool::set_tokens`]): call before the first
    /// pool is made.
    #[expect(
        blaze::tmp_path,
        reason = "every blaze process of the user must find the one machine ledger at one fixed \
                  place whatever its TMPDIR, so the ledger's directory names the real /tmp"
    )]
    pub(crate) fn join(
        told: Option<(u64, String)>,
        told_ceiling: Option<(u64, String)>,
    ) -> anyhow::Result<Joined> {
        // alloc: the ledger's path, once per process start
        let dir = PathBuf::from(format!("/tmp/blaze-{}", user()));
        crate::socket::private_dir(&dir).with_context(|| {
            format!("making {}, where the machine's ledger is, the user's alone", dir.display())
        })?;
        // Named by its layout: a blaze of another layout keeps a ledger of its own beside this
        // one, and neither refuses to start for the other's file.
        let path = dir.join(ledger::file_name());
        let pid = std::process::id();
        let start = crate::raw::process::started(pid)
            .with_context(|| format!("when this process, pid {pid}, started"))?
            .with_context(|| format!("the system has no start time of this process, pid {pid}"))?;
        let me = Process { pid, start };
        let cores = blaze_core::cores::machine()
            .context("the machine's cores, which the ledger's core tokens are made of")?
            .get() as u64;
        // The budget a new file is stamped with: no other blaze of this layout runs (there is no
        // file), so every byte the machine uses is another program's.
        let first = match &told {
            Some((bytes, _)) => *bytes,
            None => blaze_graph::arena::machine_wide(MARGIN, 0)
                .context("the machine budget every blaze process of this user shares")?
                .bytes,
        };
        let shared = Shared::open_or_create(
            &path,
            &ledger::Machine::header(first, cores),
            WORDS,
            ledger::IDENTITY,
        )
        .with_context(|| format!("mapping the machine's ledger {}", path.display()))?;
        let slot = finish(&mut Join::new(me), &shared)?;
        // alloc: the seat, once per process: its pools' tokens are `'static`
        let seat: &'static Seat =
            Box::leak(Box::new(Seat { shared, slot, left: AtomicBool::new(false) }));
        let mut machine = Machine {
            seat,
            budget: 0,
            held: 0,
            said: String::new(),
            says: String::new(),
        };
        let counted = machine.census()?;
        // What the machine leaves blaze, the others' reservations counted as blaze's.
        let budget = match told {
            // alloc: the budget's name, once per process start
            Some((bytes, from)) => Budget { bytes, from, kept_back: 0 },
            None => blaze_graph::arena::machine_wide(MARGIN, counted.reserved)
                .context("the machine budget every blaze process of this user shares")?,
        };
        let shared_budget = seat.ledger()?.narrow(budget.bytes);
        machine.budget = shared_budget;
        let own = blaze_graph::arena::machine_budget(MARGIN, told_ceiling)
            .context("the memory the process's tasks may hold")?;
        let ceiling = if own.bytes > shared_budget {
            Budget {
                bytes: shared_budget,
                // alloc: the budget's name, once per process start
                from: format!(
                    "the machine budget every blaze process of this user shares, {:.2} GiB ({}), \
                     under this process's own limit of {:.2} GiB ({})",
                    gib(shared_budget),
                    budget.from,
                    gib(own.bytes),
                    own.from
                ),
                kept_back: budget.kept_back,
            }
        } else {
            own
        };
        // alloc: the start's line, once per process start
        machine.said = format!(
            "machine budget {:.2} GiB ({}), {} cores, shared with {} other blaze processes of \
             this user through {}{}",
            gib(budget.bytes),
            budget.from,
            counted.cores,
            counted.processes.saturating_sub(1),
            path.display(),
            if shared_budget < budget.bytes {
                // alloc: as `said`
                format!(", which another holds to {:.2} GiB", gib(shared_budget))
            } else {
                String::new()
            }
        );
        blaze_graph::pool::set_tokens(seat);
        Ok(Joined { machine, ceiling })
    }

    /// What the start says of the machine budget and the ledger.
    pub(crate) fn said(&self) -> &str {
        &self.said
    }

    /// The bytes this process's slot holds: its gate's room ([`blaze_mem::budget::Budget::room`]).
    pub(crate) fn held(&self) -> u64 {
        self.held
    }

    /// Brings the slot to what the gate needs: `needs` bytes of room its gate needs now before
    /// anything is short (`blaze_mem::gate::Gate::needs`, never what it has committed, which
    /// leaves out the overdrafts), and `wanted` bytes its held tasks wait for past them. The slot
    /// moves in [`GRAIN`]s to `needs + wanted`, rounded up: one try of a [`Reserve`] when more (never a sleep: the loop thread
    /// calls this; a refused try leaves the process's want in its slot, which the others see, and
    /// is tried again when the gate asks again), a [`Release`] when a grain or more is spare. The
    /// bytes the slot holds after, which the caller gives the gate as its room.
    pub(crate) fn fit(&mut self, needs: u64, wanted: u64) -> anyhow::Result<u64> {
        let target = needs.saturating_add(wanted).div_ceil(GRAIN).saturating_mul(GRAIN);
        let seat = self.seat;
        let shared = &seat.shared;
        if target > self.held {
            let more = target - self.held;
            match finish(&mut Reserve::new(self.seat.slot, more), shared)? {
                Reserved::Granted { .. } => self.held = target,
                Reserved::Refused { counted, budget, .. } => {
                    self.says_of(counted, budget);
                }
            }
        } else if self.held - target >= GRAIN {
            let less = self.held - target;
            finish(&mut Release::new(self.seat.slot, less), shared)?;
            self.held = target;
        }
        Ok(self.held)
    }

    /// The live slots summed (the dead freed on the way), and the line a paused build says of
    /// who holds the machine.
    pub(crate) fn census(&mut self) -> anyhow::Result<Counted> {
        let counted = finish(&mut Census::new(), &self.seat.shared)?;
        self.says_of(counted.reserved, counted.budget);
        Ok(counted)
    }

    /// Who else holds the machine's memory as the last census or refusal read it, for a build's
    /// paused line (`machine 96.0 GiB, 40.1 held by other blaze processes`); `None` while none
    /// holds any, when this process's own ceiling is all that holds it back.
    pub(crate) fn says(&self) -> Option<&str> {
        (!self.says.is_empty()).then_some(self.says.as_str())
    }

    fn says_of(&mut self, counted: u64, budget: u64) {
        use std::fmt::Write as _;
        self.says.clear();
        let others = counted.saturating_sub(self.held);
        if others == 0 {
            return;
        }
        _ = write!(
            self.says,
            "machine {:.1} GiB, {:.1} held by other blaze processes of this user",
            gib(budget),
            gib(others)
        );
    }
}

/// The process's exit, clean or by a panic that unwinds: its slot is freed, which wakes the
/// processes that wait for memory or cores. One killed leaves it for the next sum to free.
impl Drop for Machine {
    fn drop(&mut self) {
        let left = finish(&mut Leave::new(self.seat.slot), &self.seat.shared);
        self.seat.left.store(true, Ordering::SeqCst);
        if let Err(why) = left {
            tracing::warn!(target: "blaze", ledger = %self.seat.shared.path().display(), "leaving the machine's ledger: {why:#}");
            // logged: the warn event above
            eprintln!("blaze: leaving the machine's ledger: {why:#}");
        }
    }
}

impl Seat {
    fn ledger(&self) -> Result<ledger::Machine<'_>, MachineError> {
        ledger::Machine::new(self.shared.path(), self.shared.words())
    }

    fn alive(&self) -> Live {
        Live { failed: RefCell::new(None) }
    }
}

/// The pools' core tokens ([`blaze_graph::pool::set_tokens`]): each take and give one operation
/// on this process's slot, which never sleeps.
impl Tokens for Seat {
    fn take(&self, want: u64, floor: u64) -> Result<Cores, MachineError> {
        let machine = self.ledger()?;
        ledger::finish(&mut TakeCores::new(self.slot, want, floor), &machine, &self.alive(), || {
            self.shared.wake_all(EPOCH);
        })
    }

    fn give(&self, cores: u64) -> Result<u64, MachineError> {
        // After the leave the slot counts nothing: what a pool still gives back as the process
        // exits is already given.
        if self.left.load(Ordering::SeqCst) {
            return Ok(0);
        }
        let machine = self.ledger()?;
        ledger::finish(&mut GiveCores::new(self.slot, cores), &machine, &self.alive(), || {
            self.shared.wake_all(EPOCH);
        })
    }

    fn epoch(&self) -> u64 {
        self.shared.words()[EPOCH].load(Ordering::SeqCst)
    }
}

/// Runs an operation that never sleeps on the mapped ledger `shared`, its wakes through the
/// mapping. A pid the system could not be asked about fails it naming why.
fn finish<O: ledger::Op>(op: &mut O, shared: &Shared) -> anyhow::Result<O::Out> {
    let machine = checked(shared)?;
    let alive = Live { failed: RefCell::new(None) };
    let out = ledger::finish(op, &machine, &alive, || shared.wake_all(EPOCH));
    alive.checked(shared.path())?;
    Ok(out?)
}

/// The mapped file, checked as this layout's ledger.
fn checked(shared: &Shared) -> Result<ledger::Machine<'_>, MachineError> {
    ledger::Machine::new(shared.path(), shared.words())
}

/// Which processes run, as the system says when a sum looks at a slot. A failed look is
/// [`Unseen`] to the ledger, which stops the operation naming the pid and frees nothing; the
/// system's words are kept here for the error ([`Live::checked`]).
struct Live {
    failed: RefCell<Option<(u32, std::io::Error)>>,
}

impl Alive for Live {
    fn started(&self, pid: u32) -> Result<Option<u64>, Unseen> {
        crate::raw::process::started(pid).map_err(|why| {
            *self.failed.borrow_mut() = Some((pid, why));
            Unseen
        })
    }
}

impl Live {
    fn checked(&self, path: &Path) -> anyhow::Result<()> {
        match self.failed.borrow_mut().take() {
            None => Ok(()),
            Some((pid, why)) => bail!(
                "when pid {pid}, a process of the machine's ledger {}, started: {why}",
                path.display()
            ),
        }
    }
}

fn user() -> u32 {
    // SAFETY: `geteuid` takes nothing, touches no memory of the caller's and can't fail.
    unsafe { libc::geteuid() }
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1u32 << 30)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The grain: a slot moves only by whole grains, up at once and down by at least one.
    #[test]
    fn a_target_rounds_up_to_whole_grains() {
        assert_eq!(1u64.div_ceil(GRAIN) * GRAIN, GRAIN);
        assert_eq!((GRAIN + 1).div_ceil(GRAIN) * GRAIN, 2 * GRAIN);
        assert_eq!(0u64.div_ceil(GRAIN) * GRAIN, 0);
    }
}
