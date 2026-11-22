//! The per-function observe overlay: the small stubs a slot's word can point at for a while,
//! and the two swap operations that put one in and take it out (launcher.md; the decision of
//! 2026-11-15 that instrumentation is never compiled into a body's code, so a body stays a
//! shareable record and counting, marking and hooks live in stubs reached through the slot
//! word).
//!
//! # The body word
//!
//! A function has two words, not one. The **slot word** is what a call loads and jumps
//! through (`blaze_launch::launch`): it names the body, or a stub. A stub does not end in the
//! body's address; it ends in a jump to the address it reads from the function's **body
//! word**, a second per-function word that always holds the plain body address. So the stub
//! is written once and never re-emitted when the body changes: a publish writes the body
//! word, and the stub picks the new body up on its next call. This is the whole of what makes
//! the overlay cheap to keep and safe to swap.
//!
//! # The state machine
//!
//! [`Observe`] is sans-IO: it maps no memory and writes no code. It owns, for each slot, the
//! value of the slot word and the body word as a driver would keep them, and hands back
//! [`ObserveWrite`]s in the order the driver must store them. A driver is the launcher's
//! control thread (`blaze-launch-rt`), the only writer of either word; program threads only
//! load them.
//!
//! The three operations:
//!
//! - [`Observe::publish`]: a new body for a slot. It stores the **body word** first. The slot
//!   word is left as it is: a slot showing a stub keeps showing it (the stub reads the new
//!   body word on its next call); a plain slot has its slot word moved to the new body after
//!   the body word, so a thread that loads the slot word reaches whole code either way.
//! - [`Observe::observe`]: put a kind of stub in a slot. It stores the body word (so the stub
//!   never reads an unset one), then the slot word to the stub. One atomic store each.
//! - [`Observe::plain`]: take the stub out. One atomic store of the body address to the slot
//!   word.
//!
//! # The ordering, against a publish
//!
//! Every store here is one aligned word a program thread loads with acquire; the driver
//! stores with release. The body word is written before any slot word that a thread would
//! follow to it, so a thread that loads a stub address from the slot word and then the body
//! word from the stub never reads a body word that names code the slot word's stub was put in
//! to reach. Because publish touches the body word and observe/plain touch the slot word, a
//! publish that lands between a thread's two loads changes only the body it ends at, never
//! whether the thread is counted or marked. [`ObservePlant`] takes that ordering out, and the
//! simulation's oracle (`tests::observe`) catches each.

use core::alloc::Allocator;

use blaze_own::Vec;
use blaze_own::planted::{NoPlant, Planted, Plants};

use crate::Slot;

/// The driver's list of stores: `alloc`'s vector in the allocator the driver names (the
/// drivers' type until the vector seam moves them to blaze-own's; D170).
type Out<B> = alloc::vec::Vec<ObserveWrite, B>;

/// A kind of stub a slot's word can point at. The bytes of each are the driver's
/// (`blaze-launch-rt`'s `observe`); the core knows only that a slot shows one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    /// Count the calls in a per-function counter and, at a threshold, queue the function for
    /// the optimizing tier and store the plain body back in the slot (so a hot function stops
    /// paying). A lost count is fine: the counter is not atomic.
    Count,
    /// Set one byte in a per-thread bitmap indexed by the function id: which functions a test
    /// entered.
    Mark,
    /// Call a runtime routine with the function id and the return address, for the monitor's
    /// count, break and allocation sampling.
    Hook,
}

/// An address in the launcher's one mapping, as the driver would store it. The core moves no
/// real memory; it carries these so the simulation can follow a thread to the code it reaches.
pub type Addr = u64;

/// What a slot's word names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slotword {
    /// No body yet: the lazy stub (`blaze_launch::launch`). A call here resolves.
    Lazy,
    /// The plain body, by its address.
    Body(Addr),
    /// An observe stub of `kind`, at `at`. The stub reads the body word and jumps.
    Stub { kind: Kind, at: Addr },
}

/// One store for the driver to carry out, as one aligned atomic store a program thread may
/// load. A call's writes are carried in the order given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObserveWrite {
    /// Store `body` in `slot`'s body word: the address every stub of the slot reads and jumps
    /// to. A publish writes this first.
    Body { slot: Slot, body: Addr },
    /// Store what `slot`'s word names.
    Slot { slot: Slot, to: Slotword },
}

/// A safeguard taken out, for the simulation's controls (`tests`): each must make the oracle
/// fail. Taken through blaze-own's seam (D165): [`Observe::new`] holds [`NoPlant`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObservePlant {
    /// The overlay as it is.
    None,
    /// `publish` moves an observed slot's word off its stub to the new body, so a function
    /// under observation stops being counted or marked without `plain`.
    PublishDropsStub,
    /// `publish` leaves the body word stale, so a slot's stub reaches the body it replaced.
    PublishSkipsBody,
}

impl Plants for ObservePlant {}

/// One function the overlay keeps the two words of.
#[derive(Clone, Copy)]
struct Func {
    /// The slot word, as the driver last stored it.
    word: Slotword,
    /// The body word: the plain body address, or `None` before the first publish.
    body: Option<Addr>,
}

/// The per-function observe overlay of one process, its table in `A`, the allocator its maker
/// names; `P` its plant seam. See the module documentation.
pub struct Observe<A: Allocator, P: Planted<ObservePlant> = NoPlant> {
    funcs: Vec<Func, A>,
    plant: P,
}

impl<A: Allocator> Observe<A, NoPlant> {
    /// An overlay of `slots` functions, each on its lazy stub with no body yet, in `alloc`.
    pub fn new(slots: u32, alloc: A) -> Observe<A, NoPlant> {
        Observe::planted(slots, alloc, NoPlant)
    }
}

impl<A: Allocator, P: Planted<ObservePlant>> Observe<A, P> {
    /// [`Observe::new`] with its plant seam: [`NoPlant`] where it ships, an [`ObservePlant`]
    /// under the simulation's controls.
    pub fn planted(slots: u32, alloc: A, plant: P) -> Observe<A, P> {
        let mut funcs = Vec::new_in(alloc);
        funcs.resize(slots as usize, Func { word: Slotword::Lazy, body: None });
        Observe { funcs, plant }
    }

    /// What `slot`'s word names now.
    pub fn word(&self, slot: Slot) -> Slotword {
        self.funcs[slot.0 as usize].word
    }

    /// The plain body address of `slot`, or `None` before its first publish.
    pub fn body(&self, slot: Slot) -> Option<Addr> {
        self.funcs[slot.0 as usize].body
    }

    /// Whether `slot` shows an observe stub, and of which kind.
    pub fn observed(&self, slot: Slot) -> Option<Kind> {
        match self.funcs[slot.0 as usize].word {
            Slotword::Stub { kind, .. } => Some(kind),
            Slotword::Lazy | Slotword::Body(_) => None,
        }
    }

    /// A new plain body for `slot`, at `body`. Stores the body word first, so every stub of
    /// the slot reaches the new body; moves the slot word to it only when the slot is plain,
    /// so a slot under observation keeps its stub (which reads the new body word).
    pub fn publish<B: Allocator>(&mut self, slot: Slot, body: Addr, out: &mut Out<B>) {
        let plant = self.plant;
        let func = &mut self.funcs[slot.0 as usize];
        func.body = Some(body);
        if !plant.has(ObservePlant::PublishSkipsBody) {
            out.push(ObserveWrite::Body { slot, body });
        }
        match func.word {
            // A stub stays in the slot: it reads the body word just written.
            Slotword::Stub { .. } if !plant.has(ObservePlant::PublishDropsStub) => {}
            // A plain or lazy slot now names the new body outright.
            _ => {
                func.word = Slotword::Body(body);
                out.push(ObserveWrite::Slot { slot, to: Slotword::Body(body) });
            }
        }
    }

    /// Puts a stub of `kind` in each of `slots`, at the address `at(slot)` the driver placed
    /// its stub. A slot with no body yet is left alone (nothing to observe until its first
    /// call resolves): the stub would read an unset body word. Stores each slot's body word
    /// before its slot word.
    pub fn observe<B: Allocator>(
        &mut self,
        slots: &[Slot],
        kind: Kind,
        mut at: impl FnMut(Slot) -> Addr,
        out: &mut Out<B>,
    ) {
        for &slot in slots {
            let func = &mut self.funcs[slot.0 as usize];
            let Some(body) = func.body else {
                continue;
            };
            let stub = Slotword::Stub { kind, at: at(slot) };
            // The body word first: the stub reads it, so it must hold the current body before
            // the slot word points a thread at the stub.
            out.push(ObserveWrite::Body { slot, body });
            func.word = stub;
            out.push(ObserveWrite::Slot { slot, to: stub });
        }
    }

    /// Takes the stub out of each of `slots`: stores the plain body address back in the slot
    /// word. A slot with no body, or already plain, is left alone.
    pub fn plain<B: Allocator>(&mut self, slots: &[Slot], out: &mut Out<B>) {
        for &slot in slots {
            let func = &mut self.funcs[slot.0 as usize];
            if let (Slotword::Stub { .. }, Some(body)) = (func.word, func.body) {
                func.word = Slotword::Body(body);
                out.push(ObserveWrite::Slot { slot, to: Slotword::Body(body) });
            }
        }
    }
}
