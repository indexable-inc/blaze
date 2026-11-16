//! `blaze run` and `blaze test` (docs/v2/plan/design/run.md): a program runs from code records
//! placed in its own process, each function compiled on its first call and optimized across
//! functions in the background while the program runs. There is no executable and no link.
//!
//! Every part is a sans-IO state machine: it takes an event, changes its state and pushes the
//! effects its driver is to carry out, in order, into a buffer the caller owns. None reads a
//! clock (time is a [`Nanos`] in the event), a process table (a trait the driver implements),
//! the environment or randomness; every table is in the allocator its maker hands it.
//!
//! | module | machine | lives in |
//! |---|---|---|
//! | [`parent`] | [`parent::Parent`]: the start, a program's needs, single flight, speculation | the `blaze run` process, alive exactly as long as the program |
//! | [`slots`] | [`slots::Slots`]: one word a function (stub, fast, optimized), publish, retirement by per-thread epochs | the program's control thread |
//! | [`heat`] | [`heat::Heat`]: a counter a slot, charged by fast code; past a bound a function is hot, once | the program's control thread |
//! | [`tier`] | [`tier::Tier`]: hot functions optimized across functions; the record valid while every inlined callee is | the parent |
//! | [`registry`] | [`registry::Registry`]: the directory of live runs; [`registry::RunEvent`], the stream a viewer reads | the parent |
//! | [`events`] | a flush of [`registry::RunEvent`]s as one blaze-format batch in columns | the parent's driver |
//! | [`viewer`] | a viewer's `Toggle` request, deterministic CBOR, decoded with limits | the parent's driver |
//! | [`suite`] | [`suite::Tests`]: what each test entered; which tests an edit affects | the parent of a test run |
//!
//! The ids below are this crate's own so that it compiles alone today; each says the type it
//! becomes when the crate that owns that type is on the new item layer (bottom.md).

#![feature(allocator_api, btreemap_alloc, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
// Denied, not forbidden: `events` takes blaze-format's `record!`, whose impl is unsafe.
#![deny(unsafe_code)]

extern crate alloc;

pub mod events;
pub mod heat;
pub mod parent;
pub mod registry;
pub mod slots;
pub mod suite;
pub mod tier;
pub mod viewer;

#[cfg(test)]
mod sim;

use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;

/// A function's slot: its index in the program's table of words, fixed for the run. Becomes
/// `blaze_launch::Slot`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SlotId(pub u32);

impl SlotId {
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// A thread of the program, by the id the launcher gives it for its life. Becomes
/// `blaze_launch::Thread`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ThreadId(pub u64);

/// What one instance is compiled from: its plan's key. Becomes `blaze_keyed::kind::Key<Instance>`
/// (blaze-codegen's `InstanceKey` today).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct InstanceKey(pub [u8; 32]);

/// A function body's code identity: what running it can depend on. A change to it is an edit of
/// that body. Becomes hir-ty-identity's `CodeId` (`blaze_launch::CodeId` names it today).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CodeId(pub [u8; 32]);

/// A code record in the store, by its key: what the control thread reads, places and publishes.
/// Becomes `blaze_keyed::kind::Key<CodeRecord>`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CodeRef(pub [u8; 32]);

/// A finding record (rule 81): the diagnostics a refused compile left, read by key when the
/// error is printed. Becomes the findings kind of blaze-keyed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FindingsRef(pub [u8; 32]);

/// A test, by its base key: its identity, nothing its code is. Becomes the key of
/// `blaze_keyed::kind::TestRun` (`TestInputs`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TestId(pub [u8; 32]);

/// The checkout a run is of: the key of its request's certificate (the text it was started on).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CheckoutKey(pub [u8; 32]);

/// A snapshot of the sources, in the order the parent learns of them: a later one is newer.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Snapshot(pub u64);

/// A time from the driver's clock, in nanoseconds. Only ever an event's field.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Nanos(pub u64);

impl Nanos {
    pub const fn after(self, by: Nanos) -> Nanos {
        Nanos(self.0.saturating_add(by.0))
    }
}

/// The tier a function's code is at.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Tier {
    /// The first code: one body, calls through slot words, a heat counter in its prologue.
    Fast,
    /// Optimized across functions: callees inlined, no counter.
    Optimized,
}

/// The hooks compiled into one version of a function (bottom.md, "Decide at run time"): each
/// bit a hook, none the plain version. A version with hooks is its own code record, made once
/// and shared; the plain version tests no flag, so a hook that is off costs nothing.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Hooks(pub u16);

impl Hooks {
    pub const NONE: Hooks = Hooks(0);
    /// Each call and return, with its arguments, to the run's event stream.
    pub const TRACE: Hooks = Hooks(1 << 0);
    /// Each effect the function makes (a write, a syscall), logged for one task or crate.
    pub const EFFECTS: Hooks = Hooks(1 << 1);
    /// Exact call counts and times, not heat's sampled ones.
    pub const COUNTS: Hooks = Hooks(1 << 2);
    /// A breakpoint: the calling thread parks with its stack for the viewer.
    pub const BREAK: Hooks = Hooks(1 << 3);
    /// Coverage: each block entered, marked.
    pub const COVERAGE: Hooks = Hooks(1 << 4);
    /// Extra checks: overflow, bounds, contracts, on the function one suspects.
    pub const CHECKS: Hooks = Hooks(1 << 5);

    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// These hooks with `other` turned on or off.
    pub const fn with(self, other: Hooks, on: bool) -> Hooks {
        match on {
            true => Hooks(self.0 | other.0),
            false => Hooks(self.0 & !other.0),
        }
    }
}

/// One version of one slot's function: what a slot's word can hold. The plain version has no
/// hooks; its tier is whichever is made best (fast, then optimized).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Version {
    pub slot: SlotId,
    pub hooks: Hooks,
}

/// A code record's key: the function, its tier and the hooks in it. Becomes the key of
/// blaze-keyed's `CodeRecord` with the hooks in its template.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct VersionKey {
    pub function: InstanceKey,
    pub tier: Tier,
    pub hooks: Hooks,
}

/// A record some compile lacks, by its key (a callee's signature, a plan, a layout). Becomes
/// `blaze_keyed::kind::Key` of any kind.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RecordKey(pub [u8; 32]);

/// What a compile task returns (bottom.md, "How a task waits: it does not"): its record, or the
/// records it lacks. On `Need` its driver makes those (as records or by running their tasks)
/// and enters the task again from the top; no compile waits on another.
pub enum Made<R, A: Allocator> {
    Done(R),
    Need(Vec<RecordKey, A>),
}

/// Why a function has no code, as every caller of it is told (rule 20: the callers fail, the
/// program goes on). Small and `Copy`: one value answers every waiter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CompileError {
    pub function: InstanceKey,
    pub slot: SlotId,
    pub why: Why,
}

/// What stopped a compile.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// The type checker or codegen refused the body: its findings are this record.
    Refused(FindingsRef),
    /// The compile panicked: a bug of blaze's, which failed this function alone.
    Panicked,
    /// A question the new layer cannot answer yet (bottom.md, `NotBuilt`).
    NotBuilt { what: &'static str },
    /// The compile ran past its bound (`parent::Config::compile_bound`) without an answer.
    Overran { bound: Nanos },
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "function {} in slot {} has no code: ", Hex(&self.function.0), self.slot.0)?;
        match self.why {
            Why::Refused(findings) => write!(f, "refused; findings {}", Hex(&findings.0)),
            Why::Panicked => write!(f, "its compile panicked (rule 20: this function alone fails)"),
            Why::NotBuilt { what } => write!(f, "not built yet: {what}"),
            Why::Overran { bound } => {
                write!(f, "its compile ran past {} ms (rule 18)", bound.0 / 1_000_000)
            }
        }
    }
}

impl core::error::Error for CompileError {}

/// The first eight bytes of a key, in hex: how an error names one.
struct Hex<'a>(&'a [u8; 32]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0[..8] {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// A safeguard taken out, for a simulation's planted-bug control (rule 43): each must make its
/// oracle fail. Always `None` outside tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), allow(dead_code, reason = "only the tests plant a bug"))]
pub(crate) enum Plant {
    None,
    /// [`slots::Slots`] stores a word before the code it names is placed.
    StoreBeforePlace,
    /// [`slots::Slots`] frees a retired body while a thread has yet to clear it.
    FreeEarly,
    /// [`parent::Parent`] asks the compiler again for a function already compiling.
    CompileTwice,
    /// [`parent::Parent`] stops the run at a failed compile instead of failing its callers.
    StopOnFailure,
    /// [`heat::Heat`] forgets a function was already queued and queues it again.
    HotTwice,
    /// [`tier::usable`] takes an optimized record without checking what it inlined.
    TrustOptimized,
    /// [`registry::Registry`] takes a run as alive when its pid is, whoever has the pid now.
    PidOnly,
    /// [`suite::Tests`] keeps a test's old postings when it records the test again.
    StalePostings,
    /// [`parent::Parent`] enters a compile again once the first of the records it lacked is made.
    ReenterEarly,
    /// [`parent::Parent`] leaves the hooked version in a slot whose hooks were all turned off.
    OffKeepsHooks,
    /// [`parent::Parent`] keys hooked versions by their hooks alone, forgetting the function.
    VersionsByHooks,
}
