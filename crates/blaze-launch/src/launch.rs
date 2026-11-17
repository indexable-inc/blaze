//! [`Launch`]: the slot table of a running program, its bodies, the transaction of an edit and
//! the retirement of what an edit replaced (docs/v2/plan/design/launcher.md, sections 1 and 4).
//!
//! # What the core promises
//!
//! 1. **Code is whole before it is reachable.** [`Launch::stage`] hands out the ranges and the
//!    driver fills and checks them; only [`Launch::commit`] makes a word name one.
//! 2. **All or nothing.** `stage` checks everything first and changes nothing when it refuses.
//!    Between `stage` and `commit` nothing the program can reach has changed, so
//!    [`Launch::abort`] only frees. `commit` cannot fail for the staged edit.
//! 3. **Together.** For an edit whose binds must switch together, no thread calls one
//!    function of it new and, after that, another old. The words go first to a *gate* that
//!    jumps old or new by one flag, then the flag is stored (the commit), then the words go to
//!    the new bodies.
//! 4. **Nothing a thread can be in is freed.** A replaced body, and the gate, are freed only
//!    after every thread alive when they were unlinked reported, at an epoch loaded after the
//!    unlinking stores, a stack with no frame in them.
//!
//! # The driver's part
//!
//! The core cannot see memory, so these are the driver's to keep. The simulation's machine
//! keeps them, and its controls show the core's own safeguards are what the rest rests on.
//!
//! - It carries out the [`Write`]s of a call in order, and all of one call's before the next
//!   call that changes the table. Every `Write::Slot`, `Write::Commit` and `Write::Epoch` is
//!   one aligned atomic store.
//! - A thread reports an epoch it loaded *before* it walked its stack.
//! - A thread about to start another loads the epoch, announces the birth where the control
//!   thread will see it, and only then starts it. The control thread calls
//!   [`Launch::thread_began`] for every announced birth before each [`Launch::collect`].
//! - Nothing but a return address, the program counter of a thread inside a gate, or the
//!   register of a call between its load of a word and its jump, holds the address of a
//!   body, and a walk from outside reports all three: the resolver jumps through the word
//!   again after a bind, and a function pointer names an entry thunk.

use alloc::vec::Vec;

use crate::area::Area;
use crate::{CodeId, Error, Frame, Kept, Range, Slot, Thread, Tier, Txn};

/// What a core is made for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// The most slots the table holds: the driver reserves a word for each.
    pub slots: u32,
    /// The code area's length: a whole number of pages.
    pub area: u64,
    /// The page size.
    pub page: u64,
    /// The bytes a gate begins with: its flag (the first 8, the word [`Write::Commit`]
    /// stores) and whatever code the target puts before the entries.
    pub gate_head: u64,
    /// The bytes of one entry of a gate.
    pub gate_entry: u64,
    /// The alignment of a gate: at least 8, for its flag.
    pub gate_align: u64,
}

/// One function's new code in an edit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bind {
    pub slot: Slot,
    /// The code's length.
    pub len: u64,
    /// Its alignment: a power of two.
    pub align: u64,
    pub tier: Tier,
    pub code: CodeId,
}

/// An edit: the bodies that replace the ones their slots name, in one transaction.
#[derive(Clone, Copy, Debug)]
pub struct Edit<'a> {
    pub txn: Txn,
    pub binds: &'a [Bind],
    /// Whether the binds must switch together: true for a change of what the program means
    /// (a source edit, with the optimized bodies it takes away); false when each new body
    /// means what the old one did (a first call, a promotion), which needs no gate.
    pub together: bool,
}

/// What a slot's word names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// The slot's lazy stub: the function has no code here yet.
    Stub,
    /// A body, by where it begins.
    Body(Range),
    /// Entry `entry` of the gate at `gate`.
    Gate { gate: Range, entry: u32 },
}

/// One thing for the driver to do. A call's writes are carried out in the order given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Write {
    /// Copy the code of the staged edit's bind `bind` into `range`, apply its fixups and check
    /// it against its record's content id. Nothing names it yet.
    Place { bind: u32, range: Range },
    /// Write a gate of `entries` entries at `range`, its flag clear.
    Gate { range: Range, entries: u32 },
    /// Entry `entry` of the gate at `gate`: jump to `new` when the flag is set, else to `old`.
    GateEntry { gate: Range, entry: u32, old: Target, new: Range },
    /// Store what `slot`'s word names.
    Slot { slot: Slot, to: Target },
    /// Set the flag of the gate at `gate`: the one store that commits an edit.
    Commit { gate: Range },
    /// Publish the epoch where threads load it.
    Epoch { epoch: u64 },
    /// `range` is freed: no thread is or will be in it. Its unwind and symbol entries go.
    Forget { range: Range },
    /// Whole pages with nothing live in them: give their memory back to the OS.
    Decommit { range: Range },
}

/// What a commit did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Committed {
    /// The epoch the commit published. A thread clears what the commit unlinked by a report
    /// of this epoch or a later one.
    pub epoch: u64,
    /// How many ranges the commit unlinked: the replaced bodies and the gate.
    pub unlinked: u32,
}

/// A body a thread can be in: what a program counter in the area names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placed {
    pub range: Range,
    pub slot: Slot,
    pub tier: Tier,
    pub code: CodeId,
    /// Whether the slot's word names it now; else it was replaced and waits to be freed.
    pub bound: bool,
}

/// A safeguard taken out, for the simulation's controls (`tests/sim.rs`): each must make the
/// oracle fail. Always `None` outside tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), allow(dead_code, reason = "only the tests plant a bug"))]
#[cfg_attr(not(test), expect(blaze::unwired, reason = "wired: a planted-bug control only its tests make"))]
pub(crate) enum Plant {
    None,
    /// An edit whose binds must switch together stores its words straight to the new bodies.
    NoGate,
    /// The epoch is published before the stores that unlink.
    EpochFirst,
    /// A batch is freed while a thread has yet to clear it.
    FreeEarly,
    /// A return address at a range's end is taken to be outside it.
    ReturnAtEnd,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Life {
    /// Handed out by `stage`; no word names it.
    Staged,
    /// Its slot's word names it.
    Bound,
    /// Unlinked by a commit; in a batch.
    Retired,
    /// Freed: never bound (an aborted edit) or cleared by every thread.
    Freed,
}

/// One body ever placed. The list is in the order of the area, so it is sorted by offset.
struct Body {
    range: Range,
    slot: Slot,
    tier: Tier,
    code: CodeId,
    life: Life,
}

/// What one commit unlinked. A thread that holds any of it holds all of it: a thread inside
/// the gate may still jump to any old body of the edit.
struct Batch {
    /// The epoch the commit published after its unlinking stores.
    stamp: u64,
    /// Indices into `Launch::bodies`.
    bodies: Vec<u32, Kept>,
    gate: Option<Range>,
    /// The threads that have not cleared it: every thread known at the commit, and every
    /// thread found since that was born before the stamp.
    waiting: Vec<Thread, Kept>,
}

struct Staged {
    txn: Txn,
    /// The staged bodies are `bodies[first..first + count]`, in the edit's order.
    first: u32,
    count: u32,
    /// The gate, for an edit that switches together and has more than one bind.
    gate: Option<Range>,
}

struct Known {
    thread: Thread,
    /// The last epoch it reported, or was born at.
    seen: u64,
}

/// The launcher's core for one process. See the module's documentation.
pub struct Launch {
    config: Config,
    alloc: Kept,
    area: Area,
    /// By slot: the body its word names (an index into `bodies`), or `None` for its stub.
    slots: Vec<Option<u32>, Kept>,
    bodies: Vec<Body, Kept>,
    batches: Vec<Batch, Kept>,
    threads: Vec<Known, Kept>,
    staged: Option<Staged>,
    /// The epoch last published. A commit publishes the next.
    epoch: u64,
    /// Kept from edit to edit: the slots of the edit being staged, sorted, to find one bound
    /// twice.
    scratch: Vec<u32, Kept>,
    pub(crate) plant: Plant,
}

impl Launch {
    /// A core with no slot, no body and no thread, its tables in `alloc`.
    pub fn new(config: Config, alloc: Kept) -> Result<Launch, Error> {
        if config.slots == 0 {
            return Err(Error::Config { what: "the table holds no slot", value: 0 });
        }
        if config.gate_head < 8 {
            return Err(Error::Config {
                what: "a gate's head is shorter than its flag, 8 bytes",
                value: config.gate_head,
            });
        }
        if config.gate_entry == 0 {
            return Err(Error::Config { what: "a gate's entry has no length", value: 0 });
        }
        if !config.gate_align.is_power_of_two() || config.gate_align < 8 {
            return Err(Error::Config {
                what: "a gate's alignment is not a power of two of at least 8",
                value: config.gate_align,
            });
        }
        Ok(Launch {
            config,
            alloc,
            area: Area::new(config.area, config.page, alloc)?,
            slots: Vec::new_in(alloc),
            bodies: Vec::new_in(alloc),
            batches: Vec::new_in(alloc),
            threads: Vec::new_in(alloc),
            staged: None,
            epoch: 0,
            scratch: Vec::new_in(alloc),
            plant: Plant::None,
        })
    }

    /// The epoch last published.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// How many slots are declared.
    pub fn declared(&self) -> u32 {
        self.slots.len() as u32
    }

    /// The staged edit, when there is one.
    pub fn staged(&self) -> Option<Txn> {
        self.staged.as_ref().map(|it| it.txn)
    }

    /// How many batches wait for a thread to clear them.
    pub fn waiting(&self) -> usize {
        self.batches.len()
    }

    /// The bytes of the area not yet handed out.
    pub fn room(&self) -> u64 {
        self.area.room()
    }

    /// What `slot`'s word names once every write the core handed out is done.
    pub fn target(&self, slot: Slot) -> Result<Target, Error> {
        match self.slots.get(slot.0 as usize) {
            Some(Some(body)) => Ok(Target::Body(self.bodies[*body as usize].range)),
            Some(None) => Ok(Target::Stub),
            None => Err(Error::NoSlot { slot, declared: self.declared() }),
        }
    }

    /// The body a program counter or return address at `frame` is in, when it is one a thread
    /// can be in (bound, or replaced and not yet freed). This is the index a backtrace, an
    /// unwind lookup and the daemon's question "is a frame of this function on a stack" read.
    pub fn placed(&self, frame: Frame) -> Option<Placed> {
        // The first body that begins after the frame; the one before it is the only one that
        // can hold it. A return address at a body's end is also the next body's first byte,
        // which `Range::holds` gives to the body it returns into, so look one further back
        // when the nearest does not hold it.
        let after = self.bodies.partition_point(|it| it.range.at <= frame.at);
        let nearest = after.checked_sub(1)?;
        let found = [Some(nearest), nearest.checked_sub(1)]
            .into_iter()
            .flatten()
            .map(|it| &self.bodies[it])
            .find(|it| it.range.holds(frame))?;
        match found.life {
            Life::Bound | Life::Retired => Some(Placed {
                range: found.range,
                slot: found.slot,
                tier: found.tier,
                code: found.code,
                bound: found.life == Life::Bound,
            }),
            Life::Staged | Life::Freed => None,
        }
    }

    /// Declares `count` more slots, each on its stub, and gives the first. A new function's
    /// slot is declared before the edit that binds or calls it is staged; a slot an aborted
    /// edit declared stays a stub nothing calls.
    pub fn declare(&mut self, count: u32, out: &mut Vec<Write, Kept>) -> Result<Slot, Error> {
        let declared = self.declared();
        match declared.checked_add(count) {
            Some(all) if all <= self.config.slots => {}
            _ => return Err(Error::Slots { asked: count, declared, most: self.config.slots }),
        }
        for slot in declared..declared + count {
            self.slots.push(None);
            out.push(Write::Slot { slot: Slot(slot), to: Target::Stub });
        }
        Ok(Slot(declared))
    }

    /// Stages `edit`: hands out a range for each bind, and a gate when the binds must switch
    /// together, as [`Write::Place`], [`Write::Gate`] and [`Write::GateEntry`]. Nothing the
    /// program can reach changes. A refusal names why and leaves the core as it was.
    ///
    /// The driver fills and checks every range, then calls [`Launch::commit`]; when it cannot
    /// (a record is missing, a hash differs), [`Launch::abort`].
    pub fn stage(&mut self, edit: &Edit<'_>, out: &mut Vec<Write, Kept>) -> Result<(), Error> {
        let txn = edit.txn;
        if let Some(open) = &self.staged {
            return Err(Error::Open { open: open.txn, asked: txn });
        }
        if edit.binds.is_empty() {
            return Err(Error::Empty { txn });
        }
        let Ok(count) = u32::try_from(edit.binds.len()) else {
            return Err(Error::Slots {
                asked: u32::MAX,
                declared: self.declared(),
                most: self.config.slots,
            });
        };
        // Check all of it before any of it is done.
        self.scratch.clear();
        let mut end = self.area.end();
        let mut need = 0u64;
        for bind in edit.binds {
            let slot = bind.slot;
            if slot.0 >= self.declared() {
                return Err(Error::NoSlot { slot, declared: self.declared() });
            }
            if bind.len == 0 {
                return Err(Error::Code { txn, slot, what: "the code has no length", value: 0 });
            }
            if !bind.align.is_power_of_two() {
                return Err(Error::Code {
                    txn,
                    slot,
                    what: "the code's alignment is not a power of two",
                    value: bind.align,
                });
            }
            self.scratch.push(slot.0);
            need = need.saturating_add(bind.len);
            match self.area.plan(end, bind.len, bind.align) {
                Some((_, after)) => end = after,
                None => return Err(Error::Full { need, room: self.area.room() }),
            }
        }
        self.scratch.sort_unstable();
        if let Some(pair) = self.scratch.windows(2).find(|pair| pair[0] == pair[1]) {
            return Err(Error::Twice { txn, slot: Slot(pair[0]) });
        }
        let gate_len = (edit.together && count > 1)
            .then(|| self.config.gate_head + u64::from(count) * self.config.gate_entry);
        if let Some(len) = gate_len {
            need = need.saturating_add(len);
            if self.area.plan(end, len, self.config.gate_align).is_none() {
                return Err(Error::Full { need, room: self.area.room() });
            }
        }

        // Nothing below refuses: every range was planned above from the same end.
        let first = self.bodies.len() as u32;
        for (index, bind) in edit.binds.iter().enumerate() {
            let range = self.area.append(bind.len, bind.align)?;
            self.bodies.push(Body {
                range,
                slot: bind.slot,
                tier: bind.tier,
                code: bind.code,
                life: Life::Staged,
            });
            out.push(Write::Place { bind: index as u32, range });
        }
        let gate = match gate_len {
            Some(len) => {
                let gate = self.area.append(len, self.config.gate_align)?;
                out.push(Write::Gate { range: gate, entries: count });
                for (index, bind) in edit.binds.iter().enumerate() {
                    out.push(Write::GateEntry {
                        gate,
                        entry: index as u32,
                        old: self.target(bind.slot)?,
                        new: self.bodies[first as usize + index].range,
                    });
                }
                Some(gate)
            }
            None => None,
        };
        self.staged = Some(Staged { txn, first, count, gate });
        Ok(())
    }

    /// Commits the staged edit `txn`, whose ranges the driver has filled and checked: the
    /// stores that switch the program to it, then the epoch. The bodies it replaced, and its
    /// gate, become a batch that [`Launch::collect`] frees once every thread cleared it.
    ///
    /// The order is the safeguard. Through a gate: every word to its gate entry (the flag is
    /// clear, so each still runs the old body), the flag (from this store on every call
    /// through a gate runs the new), every word to its new body. The epoch comes last, so a
    /// thread that loads it loaded it after every unlinking store.
    pub fn commit(&mut self, txn: Txn, out: &mut Vec<Write, Kept>) -> Result<Committed, Error> {
        let staged = match self.staged.take() {
            Some(staged) if staged.txn == txn => staged,
            other => {
                let open = other.as_ref().map(|it| it.txn);
                self.staged = other;
                return Err(Error::NotStaged { asked: txn, open });
            }
        };
        let epoch = self.epoch + 1;
        self.epoch = epoch;
        let first = staged.first as usize;
        let last = first + staged.count as usize;

        if self.plant == Plant::EpochFirst {
            out.push(Write::Epoch { epoch });
        }
        if let Some(gate) = staged.gate
            && self.plant != Plant::NoGate
        {
            for (entry, body) in self.bodies[first..last].iter().enumerate() {
                let to = Target::Gate { gate, entry: entry as u32 };
                out.push(Write::Slot { slot: body.slot, to });
            }
            out.push(Write::Commit { gate });
        }
        for body in &self.bodies[first..last] {
            out.push(Write::Slot { slot: body.slot, to: Target::Body(body.range) });
        }
        if self.plant != Plant::EpochFirst {
            out.push(Write::Epoch { epoch });
        }

        let mut batch = Batch {
            stamp: epoch,
            bodies: Vec::new_in(self.alloc),
            gate: staged.gate,
            waiting: Vec::new_in(self.alloc),
        };
        for new in first..last {
            let slot = self.bodies[new].slot.0 as usize;
            if let Some(old) = self.slots[slot].replace(new as u32) {
                self.bodies[old as usize].life = Life::Retired;
                batch.bodies.push(old);
            }
            self.bodies[new].life = Life::Bound;
        }
        let unlinked = batch.bodies.len() as u32 + u32::from(batch.gate.is_some());
        if unlinked > 0 {
            batch.waiting.extend(self.threads.iter().map(|it| it.thread));
            self.batches.push(batch);
        }
        Ok(Committed { epoch, unlinked })
    }

    /// Gives up the staged edit `txn`: its ranges are freed at once, since no word ever named
    /// them. The table is as it was before [`Launch::stage`].
    pub fn abort(&mut self, txn: Txn, out: &mut Vec<Write, Kept>) -> Result<(), Error> {
        let staged = match self.staged.take() {
            Some(staged) if staged.txn == txn => staged,
            other => {
                let open = other.as_ref().map(|it| it.txn);
                self.staged = other;
                return Err(Error::NotStaged { asked: txn, open });
            }
        };
        let first = staged.first as usize;
        for index in first..first + staged.count as usize {
            self.free_body(index, out);
        }
        if let Some(gate) = staged.gate {
            self.free_range(gate, out);
        }
        Ok(())
    }

    /// A thread began, having loaded `epoch` before it could run any of the program's code
    /// (its parent loaded it before starting it). It holds nothing unlinked by then, and
    /// everything unlinked since until it reports.
    pub fn thread_began(&mut self, thread: Thread, epoch: u64) -> Result<(), Error> {
        if self.threads.iter().any(|it| it.thread == thread) {
            return Err(Error::ThreadTwice { thread });
        }
        if epoch > self.epoch {
            return Err(Error::Epoch {
                thread,
                said: epoch,
                what: "is past the last published",
                against: self.epoch,
            });
        }
        self.threads.push(Known { thread, seen: epoch });
        for batch in self.batches.iter_mut().filter(|it| it.stamp > epoch) {
            batch.waiting.push(thread);
        }
        Ok(())
    }

    /// A thread the launcher did not start was found (in the OS's list of the process's
    /// threads): nothing is known of where it has been, so it holds every batch until it
    /// reports.
    pub fn thread_found(&mut self, thread: Thread) -> Result<(), Error> {
        self.thread_began(thread, 0)
    }

    /// A thread ended: it is in no code any more.
    pub fn thread_ended(&mut self, thread: Thread) -> Result<(), Error> {
        let Some(at) = self.threads.iter().position(|it| it.thread == thread) else {
            return Err(Error::NoThread { thread });
        };
        self.threads.swap_remove(at);
        for batch in &mut self.batches {
            batch.waiting.retain(|it| *it != thread);
        }
        Ok(())
    }

    /// A thread's stack, as it was after the thread loaded `epoch`: every frame of it that
    /// lies in the code area (`frames`), and whether the walk reached the stack's end
    /// (`complete`; a walk that gave up holds everything). A walk from outside, of a thread
    /// stopped anywhere, also gives the thread's program counter and the register a call
    /// through a word holds its target in between the load and the jump (x16 on AArch64; on
    /// x86-64 the call is one instruction). The thread clears each batch
    /// stamped at or before `epoch` that none of its frames is in. Once clear, always: no
    /// word names what a batch holds, so the thread cannot enter it again.
    pub fn report(
        &mut self,
        thread: Thread,
        epoch: u64,
        frames: &[Frame],
        complete: bool,
    ) -> Result<(), Error> {
        let Some(known) = self.threads.iter_mut().find(|it| it.thread == thread) else {
            return Err(Error::NoThread { thread });
        };
        if epoch > self.epoch {
            return Err(Error::Epoch {
                thread,
                said: epoch,
                what: "is past the last published",
                against: self.epoch,
            });
        }
        // An older report than the thread's last is still true (what a thread cleared stays
        // cleared), so reports are taken in any order: a scan from outside may overtake a
        // report the thread left at a safepoint.
        known.seen = known.seen.max(epoch);
        let bodies = &self.bodies;
        let plant = self.plant;
        for batch in self.batches.iter_mut().filter(|it| it.stamp <= epoch) {
            let Some(at) = batch.waiting.iter().position(|it| *it == thread) else {
                continue;
            };
            let held = !complete
                || frames.iter().any(|frame| {
                    batch.gate.is_some_and(|gate| holds(gate, *frame, plant))
                        || batch
                            .bodies
                            .iter()
                            .any(|body| holds(bodies[*body as usize].range, *frame, plant))
                });
            if !held {
                batch.waiting.swap_remove(at);
            }
        }
        Ok(())
    }

    /// Frees every batch that every thread cleared: a [`Write::Forget`] for each range, and a
    /// [`Write::Decommit`] for the whole pages that leaves empty. Gives how many batches it
    /// freed. The driver calls [`Launch::thread_began`] for every announced birth first.
    pub fn collect(&mut self, out: &mut Vec<Write, Kept>) -> usize {
        let mut freed = 0;
        let mut at = 0;
        while at < self.batches.len() {
            if !self.batches[at].waiting.is_empty() && self.plant != Plant::FreeEarly {
                at += 1;
                continue;
            }
            // In the order they were unlinked: the oldest batch first.
            let batch = self.batches.remove(at);
            for body in &batch.bodies {
                self.free_body(*body as usize, out);
            }
            if let Some(gate) = batch.gate {
                self.free_range(gate, out);
            }
            freed += 1;
        }
        freed
    }

    fn free_body(&mut self, index: usize, out: &mut Vec<Write, Kept>) {
        self.bodies[index].life = Life::Freed;
        let range = self.bodies[index].range;
        self.free_range(range, out);
    }

    fn free_range(&mut self, range: Range, out: &mut Vec<Write, Kept>) {
        out.push(Write::Forget { range });
        self.area.free(range, |pages| out.push(Write::Decommit { range: pages }));
    }
}

/// [`Range::holds`], or the planted rule that forgets a return address at a range's end.
fn holds(range: Range, frame: Frame, plant: Plant) -> bool {
    match plant {
        Plant::ReturnAtEnd => frame.at >= range.at && frame.at < range.end(),
        _ => range.holds(frame),
    }
}
