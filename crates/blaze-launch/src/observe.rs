//! The per-function observe overlay: the small stubs a slot's word can point at for a while,
//! the two swap operations that put one in and take it out, and automatic deoptimization of
//! optimized callers that inlined an observed callee (launcher.md; bottom.md section 11:
//! instrumentation is never compiled into a body's code, so a body stays a shareable record
//! and counting, marking, hooks and spans live in stubs reached through the slot word).
//!
//! # The body word
//!
//! A function has two words, not one. The **slot word** is what a call loads and jumps
//! through (`blaze_launch::launch`): it names the body, or a stub. A stub does not end in the
//! body's address; it ends in a jump to the address it reads from the function's **body
//! word**, a second per-function word that always holds the effective plain body address. So
//! the stub is written once and never re-emitted when the body changes: a publish or
//! deoptimization writes the body word, and the stub picks the new body up on its next call.
//!
//! # Inlining deoptimization (hook-toggle retreat)
//!
//! A fast-tier body calls every callee through its slot word. An optimized-tier body may inline
//! callees from its read list (`inlined`), bypassing their slot words. To make observing a
//! callee sound by construction:
//!
//! - Every slot keeps both its `fast` body and (when promoted) its `optimized` body plus the
//!   list of callees that optimized body inlined, and a reference count `deopt_refs` of how
//!   many of those inlined callees are currently observed.
//! - The slot's **effective body** is `optimized` if and only if `optimized.is_some()` and
//!   `deopt_refs == 0`; otherwise it is `fast`.
//! - [`Observe::observe`] installs the stub on callee `c`, increments `deopt_refs` on every
//!   optimized caller that inlined `c`, and retreats any caller transitioning `0 -> 1` back to
//!   its `fast` body.
//! - [`Observe::plain`] removes the stub on `c`, decrements `deopt_refs` on those callers, and
//!   re-promotes any caller transitioning `1 -> 0` back to its `optimized` body.
//! - [`Observe::publish_optimized`] counts how many of the new record's inlined callees are
//!   already observed; if any are, `deopt_refs > 0` and the caller stays on `fast` until they
//!   return to plain.
//!
//! # The ordering, against a publish
//!
//! Every store here is one aligned word a program thread loads with acquire; the driver stores
//! with release. The body word is written before any slot word that a thread would follow to
//! it, so a thread that loads a stub address from the slot word and then the body word from the
//! stub never reads a body word that names code the slot word's stub was put in to reach.
//! [`ObservePlant`] takes each safeguard out, and the simulation's oracle (`tests::observe`)
//! catches each.

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
    /// Call a runtime routine on entry with the function id and return address, for the
    /// monitor's count, break and allocation sampling.
    Hook,
    /// Entry+exit span hook: saves the return address on a per-thread shadow stack to record
    /// exact function durations and hierarchical parent-child span trees without touching `sp`.
    Span,
    /// Rust global-allocator shim hook (`__rust_alloc*` / `__rdl_*`): profiles every Rust
    /// allocator (`System`, `jemalloc`, `mimalloc`, custom `#[global_allocator]`) with exact
    /// `(size, align)` and zero overhead when switched back to `Plain`.
    Alloc(crate::telemetry::AllocHookKind),
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
    /// under observation stops being counted, marked or hooked without `plain`.
    PublishDropsStub,
    /// `publish` leaves the body word stale, so a slot's stub reaches the body it replaced.
    PublishSkipsBody,
    /// `observe` installs the callee's stub without retreating optimized callers that inlined
    /// it, so calls through those callers silently bypass the stub.
    SkipDeoptOnObserve,
    /// `publish_optimized` activates an optimized body even when one of its inlined callees is
    /// already observed.
    PublishOptWhileCalleeObserved,
}

impl Plants for ObservePlant {}

/// One function the overlay keeps the two words and tier/inlining state of.
#[derive(Clone, Copy)]
struct Func {
    /// The slot word, as the driver last stored it.
    word: Slotword,
    /// The fast-tier body address (always calls callees through slot words), or `None` before
    /// the first publish.
    fast: Option<Addr>,
    /// The optimized-tier body address (may inline callees), if promoted.
    optimized: Option<Addr>,
    /// The effective body address currently in the body word.
    active: Option<Addr>,
    /// Slice `[inlined_start..inlined_start + inlined_len]` in `Observe::inlined` naming the
    /// unique callees that `optimized` inlined.
    inlined_start: u32,
    inlined_len: u32,
    /// How many callees in `inlined` are currently observed (`word == Stub { .. }`).
    deopt_refs: u32,
}

impl Func {
    const EMPTY: Func = Func {
        word: Slotword::Lazy,
        fast: None,
        optimized: None,
        active: None,
        inlined_start: 0,
        inlined_len: 0,
        deopt_refs: 0,
    };

    const fn is_observed(&self) -> bool {
        matches!(self.word, Slotword::Stub { .. })
    }

    /// The body this slot should run right now: `optimized` only when no inlined callee is
    /// observed, otherwise `fast`.
    const fn desired_body(&self) -> Option<Addr> {
        match (self.optimized, self.deopt_refs) {
            (Some(opt), 0) => Some(opt),
            _ => self.fast,
        }
    }
}

/// The per-function observe overlay of one process, its tables in `A`, the allocator its maker
/// names; `P` its plant seam. See the module documentation.
pub struct Observe<A: Allocator + Copy, P: Planted<ObservePlant> = NoPlant> {
    funcs: Vec<Func, A>,
    /// Flat storage of inlined callee slots per optimized caller.
    inlined: Vec<Slot, A>,
    plant: P,
}

impl<A: Allocator + Copy> Observe<A, NoPlant> {
    /// An overlay of `slots` functions, each on its lazy stub with no body yet, in `alloc`.
    pub fn new(slots: u32, alloc: A) -> Observe<A, NoPlant> {
        Observe::planted(slots, alloc, NoPlant)
    }
}

impl<A: Allocator + Copy, P: Planted<ObservePlant>> Observe<A, P> {
    /// [`Observe::new`] with its plant seam: [`NoPlant`] where it ships, an [`ObservePlant`]
    /// under the simulation's controls.
    pub fn planted(slots: u32, alloc: A, plant: P) -> Observe<A, P> {
        let mut funcs = Vec::new_in(alloc);
        funcs.resize(slots as usize, Func::EMPTY);
        Observe { funcs, inlined: Vec::new_in(alloc), plant }
    }

    /// Ensures the table holds at least `slots` entries (new entries start lazy).
    pub fn ensure_slots(&mut self, slots: u32) {
        let want = slots as usize;
        if self.funcs.len() < want {
            self.funcs.resize(want, Func::EMPTY);
        }
    }

    /// Number of slots currently tracked.
    pub fn len(&self) -> u32 {
        self.funcs.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.funcs.is_empty()
    }

    /// What `slot`'s word names now.
    pub fn word(&self, slot: Slot) -> Slotword {
        self.funcs[slot.0 as usize].word
    }

    /// The effective plain body address of `slot`, or `None` before its first publish.
    pub fn body(&self, slot: Slot) -> Option<Addr> {
        self.funcs[slot.0 as usize].active
    }

    /// The fast-tier body address of `slot`, if published.
    pub fn fast_body(&self, slot: Slot) -> Option<Addr> {
        self.funcs[slot.0 as usize].fast
    }

    /// The optimized-tier body address of `slot`, if promoted.
    pub fn optimized_body(&self, slot: Slot) -> Option<Addr> {
        self.funcs[slot.0 as usize].optimized
    }

    /// Whether `slot` is currently retreated to its fast body due to an observed inlined callee.
    pub fn is_deoptimized(&self, slot: Slot) -> bool {
        let func = &self.funcs[slot.0 as usize];
        func.optimized.is_some() && func.deopt_refs > 0
    }

    /// Whether `slot` shows an observe stub, and of which kind.
    pub fn observed(&self, slot: Slot) -> Option<Kind> {
        match self.funcs[slot.0 as usize].word {
            Slotword::Stub { kind, .. } => Some(kind),
            Slotword::Lazy | Slotword::Body(_) => None,
        }
    }

    /// Emits the body-word and (if plain/lazy) slot-word stores to make `slot` execute `body`.
    fn switch_active<B: Allocator>(&mut self, slot: Slot, body: Addr, out: &mut Out<B>) {
        let plant = self.plant;
        let func = &mut self.funcs[slot.0 as usize];
        func.active = Some(body);
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

    /// A new plain (fast-tier) body for `slot`, at `body`. Stores the body word first, so every
    /// stub of the slot reaches the new body; moves the slot word to it only when the slot is
    /// plain, so a slot under observation keeps its stub (which reads the new body word).
    /// Also clears any superseded optimized body for `slot`.
    pub fn publish<B: Allocator>(&mut self, slot: Slot, body: Addr, out: &mut Out<B>) {
        self.ensure_slots(slot.0.saturating_add(1));
        {
            let func = &mut self.funcs[slot.0 as usize];
            func.fast = Some(body);
            func.optimized = None;
            func.inlined_start = 0;
            func.inlined_len = 0;
            func.deopt_refs = 0;
        }
        self.switch_active(slot, body, out);
    }

    /// Promotes `slot` to an optimized body at `body` that inlined `inlined_callees`.
    /// Requires `slot` to already have a fast body (so it can retreat if any inlined callee is
    /// observed); if no fast body exists yet, `fast` is left unchanged and nothing is published.
    ///
    /// If any callee in `inlined_callees` is currently observed, `slot` records the optimized
    /// body and its `deopt_refs` count but stays on its `fast` body until all those callees
    /// become plain.
    pub fn publish_optimized<B: Allocator>(
        &mut self,
        slot: Slot,
        body: Addr,
        inlined_callees: &[Slot],
        out: &mut Out<B>,
    ) {
        self.ensure_slots(slot.0.saturating_add(1));
        if self.funcs[slot.0 as usize].fast.is_none() {
            return;
        }
        // Append deduplicated inlined callee slots (ignoring self-recursion: observing `slot`
        // itself still goes through `slot`'s own stub at entry).
        let start = self.inlined.len() as u32;
        let mut count = 0u32;
        let mut deopt_refs = 0u32;
        for &callee in inlined_callees {
            if callee == slot {
                continue;
            }
            let slice_start = start as usize;
            let slice_end = slice_start + count as usize;
            if self.inlined[slice_start..slice_end].contains(&callee) {
                continue;
            }
            self.ensure_slots(callee.0.saturating_add(1));
            self.inlined.push(callee);
            count += 1;
            if self.funcs[callee.0 as usize].is_observed() {
                deopt_refs += 1;
            }
        }
        let activate = {
            let func = &mut self.funcs[slot.0 as usize];
            func.optimized = Some(body);
            func.inlined_start = start;
            func.inlined_len = count;
            func.deopt_refs = deopt_refs;
            if self.plant.has(ObservePlant::PublishOptWhileCalleeObserved) {
                Some(body)
            } else {
                func.desired_body()
            }
        };
        if let Some(target) = activate {
            if self.funcs[slot.0 as usize].active != Some(target) {
                self.switch_active(slot, target, out);
            }
        }
    }

    /// Checks whether `caller`'s optimized body inlined `callee`.
    fn caller_inlined(&self, caller_index: usize, callee: Slot) -> bool {
        let func = &self.funcs[caller_index];
        if func.optimized.is_none() || func.inlined_len == 0 {
            return false;
        }
        let start = func.inlined_start as usize;
        let end = start + func.inlined_len as usize;
        self.inlined[start..end].contains(&callee)
    }

    /// Puts a stub of `kind` in each of `slots`, at the address `at(slot)` the driver placed
    /// its stub. A slot with no body yet is left alone (nothing to observe until its first
    /// call resolves): the stub would read an unset body word. Stores each slot's body word
    /// before its slot word, and retreats any optimized caller that inlined a newly-observed
    /// slot back to its fast body.
    pub fn observe<B: Allocator>(
        &mut self,
        slots: &[Slot],
        kind: Kind,
        mut at: impl FnMut(Slot) -> Addr,
        out: &mut Out<B>,
    ) {
        let skip_deopt = self.plant.has(ObservePlant::SkipDeoptOnObserve);
        for &slot in slots {
            let idx = slot.0 as usize;
            if idx >= self.funcs.len() {
                continue;
            }
            let (was_observed, body) = {
                let func = &self.funcs[idx];
                (func.is_observed(), func.active)
            };
            let Some(body) = body else {
                continue;
            };
            let stub = Slotword::Stub { kind, at: at(slot) };
            // The body word first: the stub reads it, so it must hold the current body before
            // the slot word points a thread at the stub.
            out.push(ObserveWrite::Body { slot, body });
            self.funcs[idx].word = stub;
            out.push(ObserveWrite::Slot { slot, to: stub });

            // Newly observed: retreat every optimized caller that inlined `slot` back to fast.
            if !was_observed && !skip_deopt {
                for caller_idx in 0..self.funcs.len() {
                    if caller_idx == idx || !self.caller_inlined(caller_idx, slot) {
                        continue;
                    }
                    let caller = Slot(caller_idx as u32);
                    let retreat = {
                        let func = &mut self.funcs[caller_idx];
                        let prev = func.deopt_refs;
                        func.deopt_refs = prev.saturating_add(1);
                        if prev == 0 { func.fast } else { None }
                    };
                    if let Some(fast) = retreat {
                        if self.funcs[caller_idx].active != Some(fast) {
                            self.switch_active(caller, fast, out);
                        }
                    }
                }
            }
        }
    }

    /// Takes the stub out of each of `slots`: stores the plain body address back in the slot
    /// word, and re-promotes any deoptimized caller whose inlined callees are now all plain.
    /// A slot with no body, or already plain, is left alone.
    pub fn plain<B: Allocator>(&mut self, slots: &[Slot], out: &mut Out<B>) {
        for &slot in slots {
            let idx = slot.0 as usize;
            if idx >= self.funcs.len() {
                continue;
            }
            let removed = {
                let func = &mut self.funcs[idx];
                if let (Slotword::Stub { .. }, Some(body)) = (func.word, func.active) {
                    func.word = Slotword::Body(body);
                    out.push(ObserveWrite::Slot { slot, to: Slotword::Body(body) });
                    true
                } else {
                    false
                }
            };
            if removed {
                for caller_idx in 0..self.funcs.len() {
                    if caller_idx == idx || !self.caller_inlined(caller_idx, slot) {
                        continue;
                    }
                    let caller = Slot(caller_idx as u32);
                    let repromote = {
                        let func = &mut self.funcs[caller_idx];
                        func.deopt_refs = func.deopt_refs.saturating_sub(1);
                        if func.deopt_refs == 0 { func.optimized } else { None }
                    };
                    if let Some(opt) = repromote {
                        if self.funcs[caller_idx].active != Some(opt) {
                            self.switch_active(caller, opt, out);
                        }
                    }
                }
            }
        }
    }
}
