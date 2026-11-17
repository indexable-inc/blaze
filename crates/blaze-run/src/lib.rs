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
//! | [`slots`] | [`slots::Slots`]: two words a function (the slot word callers jump through, the body word stubs jump through), publish, observe and plain, retirement by per-thread epochs | the program's control thread |
//! | [`first_call`] | [`first_call::FirstCall`]: slots numbered by symbol as they appear; a stub's first call placed from a known record or asked of the parent once, published once, every waiter woken after the store | the program's control thread |
//! | [`heat`] | [`heat::Heat`]: a count stub's "bound reached" for a slot; a function is hot once | the program's control thread |
//! | [`tier`] | [`tier::Tier`]: hot functions optimized across functions; the record valid while every inlined callee is | the parent |
//! | [`registry`] | [`registry::Registry`]: the directory of live runs; [`registry::RunEvent`], the stream a viewer reads | the parent |
//! | [`events`] | a flush of [`registry::RunEvent`]s as one blaze-format batch in columns | the parent's driver |
//! | [`viewer`] | a viewer's `Toggle` request, deterministic CBOR, decoded with limits | the parent's driver |
//! | [`suite`] | [`suite::Tests`]: what each test entered; which tests an edit affects | the parent of a test run |
//! | [`plan`] | [`plan::Plan`]: one `blaze test` request: kept, looked up or run; width, deadlines, verdicts | the `blaze test` process |
//! | [`postings`] | the suite's postings record: from each ask to the tests that asked it, read in place | the store |
//! | [`result`] | a test's result record: a pass or a failure at its address, filed beside each other; the `Flaky` finding | the store |
//! | [`report`] | the run report: failures as they end, capped output, the summary, `--format json`; a pure formatter | the `blaze test` process |
//! | [`zygote`] | [`zygote::Pool`]: one process per test forked from the suite's zygote, at most `width` at once; descriptor 3 read, output captured, deadlines, reaping; [`zygote::Gate`], the one-thread proof every fork takes | the suite's zygote |
//! | [`child`] | [`child::Child`]: a forked child's steps from the fork to the test's call, and its one report | a forked child |
//! | [`wire`] | `TestEnded` and `Stacks` on descriptor 3: framed, versioned, digested; opened with every length checked | both |
//! | [`oscall`] | an observed OS call to an ask or a not-kept reason; [`oscall::Observe`], what the launcher's wrappers call | a forked child |
//!
//! The ids below are this crate's own so that it compiles alone today; each says the type it
//! becomes when the crate that owns that type is on the new item layer (bottom.md).

// The tests keep std's prelude; the crate itself is built without it.
#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
// Denied, not forbidden: `events` takes blaze-format's `record!`, whose impl is unsafe.
#![deny(unsafe_code)]

// The one reason this crate links `std`: blaze-format's `record!` (in `events`) names
// `::std::vec::Vec`. The crate's own code sees only `core` (no std prelude: `#![no_std]`), so
// nothing else can reach `std`; this goes when the macro names `core` and blaze-own.
#[cfg(not(test))]
extern crate std;

pub mod events;
pub mod first_call;
pub mod head;
pub mod heat;
pub mod judge;
pub mod parent;
pub mod plan;
pub mod postings;
pub mod property;
pub mod registry;
pub mod report;
pub mod result;
pub mod slots;
pub mod suite;
pub mod tapes;
pub mod tier;
pub mod viewer;
pub mod child;
pub mod oscall;
pub mod wire;
pub mod zygote;
pub mod offer;
pub mod promote;

#[cfg(test)]
mod sim;
#[cfg(test)]
mod offer_tests;

use blaze_own::AVec;
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

/// The tier a function's code is at. Neither holds any instrumentation: what is observed is a
/// stub in front of the code ([`Watch`]), so a function's code bytes are the same whatever is.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Tier {
    /// The first code: one body, calls through slot words. Counted by a count stub until hot.
    Fast,
    /// Optimized across functions: callees inlined.
    Optimized,
}

/// The hooks a viewer turns on (bottom.md, "Decide at run time"). Each is a hook stub in front
/// of the function's code, never code compiled into it: the stub runs the hook at the call, then
/// jumps through the body word. What a stub cannot see (each block entered, extra checks inside
/// the body) changes the code and so is not a hook: it is another program, keyed as such.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Hooks(pub u16);

impl Hooks {
    pub const NONE: Hooks = Hooks(0);
    /// Each call and return, with its argument registers, to the run's event stream (the
    /// return through a trampoline the stub puts in the return address).
    pub const TRACE: Hooks = Hooks(1 << 0);
    /// Exact call counts and times.
    pub const COUNTS: Hooks = Hooks(1 << 1);
    /// A breakpoint: the calling thread parks with its stack for the viewer.
    pub const BREAK: Hooks = Hooks(1 << 2);
    /// One call in N recorded with its stack.
    pub const SAMPLE: Hooks = Hooks(1 << 3);
    /// Every hook there is: a viewer's request naming another bit is refused.
    pub const ALL: Hooks = Hooks(0b1111);

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

/// What a slot's stub does before it jumps through the body word: the viewer's [`Hooks`] in the
/// low bits, and the run's own two observers. None is the plain state, where the slot word names
/// the body itself and a call pays nothing.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Watch(pub u16);

impl Watch {
    pub const NONE: Watch = Watch(0);
    /// The count stub: a per-function counter in a data table, counted down from heat's bound;
    /// at zero its runtime call queues the slot ([`heat::Heat::reached`]).
    pub const HEAT: Watch = Watch(1 << 14);
    /// The mark stub: one byte a slot in the calling thread's bitmap, while a test runs.
    pub const MARK: Watch = Watch(1 << 15);

    pub const fn hooks(hooks: Hooks) -> Watch {
        Watch(hooks.0)
    }

    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// This watch with `other` turned on or off.
    pub const fn with(self, other: Watch, on: bool) -> Watch {
        match on {
            true => Watch(self.0 | other.0),
            false => Watch(self.0 & !other.0),
        }
    }
}

/// A code record's key: the function and its tier, nothing about what is observed. Becomes the
/// key of blaze-keyed's `CodeRecord`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CodeKey {
    pub function: InstanceKey,
    pub tier: Tier,
}

/// A record some compile lacks, by its key (a callee's signature, a plan, a layout). Becomes
/// `blaze_keyed::kind::Key` of any kind.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RecordKey(pub [u8; 32]);

/// What a compile task returns (bottom.md, "How a task waits: it does not"): its record, or the
/// records it lacks. On `Need` its driver makes those (as records or by running their tasks)
/// and enters the task again from the top; no compile waits on another.
pub enum Made<'a, R> {
    Done(R),
    Need(AVec<'a, RecordKey>),
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
    /// The function's code record names a target `symbol` the program's manifest does not
    /// resolve (`parent::Resolve`): no slot can be declared for it, so the record is not sent.
    Unresolved { symbol: first_call::Symbol },
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
            Why::Unresolved { symbol } => write!(
                f,
                "its record calls symbol {}, which names no function of the program (rule 21: the record and the manifest disagree)",
                Hex(&symbol.0)
            ),
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
    /// [`offer::offer`] takes a body whatever the running total, past `OfferBound::total`.
    IgnoreTotal,
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
    /// [`parent::Parent`] leaves a slot's hooks on when the viewer turned them all off.
    OffKeepsHooks,
    /// [`slots::Slots`] stores the new body in the slot word whatever it held, over a stub.
    PublishOverStub,
    /// [`zygote::Pool`] takes a report whose digest does not match its body.
    TrustTornReport,
    /// [`zygote::Pool`] takes the report of a child it killed past its deadline as its result.
    KilledReportStands,
    /// [`parent::Parent::declare`] gives an instance already declared a second slot.
    DeclareTwice,
    /// [`parent::Parent`] sends a record before telling the program its targets' slots.
    PublishBeforeTell,
    /// [`promote::Promote`] keeps a batch after laying it out, and lays it out again.
    PromoteTwice,
}
