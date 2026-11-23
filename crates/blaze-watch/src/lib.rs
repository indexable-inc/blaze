//! blaze's monitor plumbing (docs/v2/plan/design/watch.md): how what a running `blaze` command
//! does reaches a person's browser, and the recording kept when something stalls.
//!
//! ```text
//! app threads --emit--> ring (one a thread) --drain--> Feed (the run) ==socket==> Link (viewer)
//!                                                                       |
//!            page (ix.dev) <==HTTP/1.1 + SSE, loopback==  Viewer  ------+--> Relay (opt-in)
//! ```
//!
//! | module | what | lives in |
//! |---|---|---|
//! | [`ring`] | [`ring::Emitter`]: a fixed table of single-producer seqlock rings, one a thread; emit is wait-free, a drain never blocks a producer, overwrite-oldest with an exact gap count | every blaze process |
//! | [`recorder`] | [`recorder::Recorder`]: when to sample a stack (a wait past 150 ms) and when to save the rings (a wait past 1 s), and which recordings to keep | the process's drain thread |
//! | [`pack`] | the recording format: LEB128, zigzag deltas, one column an event field | the drain thread's driver; any reader |
//! | [`cbor`] | deterministic CBOR, written into a named allocator and read with limits | everything on a wire |
//! | [`frame`] | the transport's frames: a length word, then one CBOR array | both ends |
//! | [`conn`] | [`conn::Conn`]: channels, credit both ways ([`conn::Credit`] to send, [`conn::Grant`] to receive), owed control frames, a priority order | both ends |
//! | [`window`] | [`window::Window`]: the bounded replay window of a feed, by sequence number | the run; the viewer |
//! | [`feed`] | [`feed::Feed`]: the run's side of the event channel (drained rings into a window, resume with a gap); [`feed::Follow`], the reader's side, which refuses a duplicate or a silent skip | the run; the viewer |
//! | [`http`] | a request parser with limits, the response heads, CORS and Private Network Access, SSE framing | the viewer |
//! | [`pairing`] | [`pairing::Pairing`]: a code shown by the page, confirmed by the next blaze command, a key per browser | the viewer |
//! | [`viewer`] | [`viewer::Viewer`]: live runs, fan-in, pages, history, toggles, expiry | the viewer helper |
//! | [`relay`] | [`relay::Uplink`]: the opt-in stream to the relay on ix.dev, activity only | the viewer |
//! | [`since`] | [`since::Watcher`]: the Linux change watcher's core: places, the names heard since each, one flush per ask, typed refusals when it cannot vouch, expiry | the change-watcher helper (its driver not written yet) |
//! | [`helper`] | [`helper::Helper`]: the journal helper's session around [`since::Watcher`] (its hello, a version's ask, its binary replaced, its checkout gone); [`helper::Asking`], a command's ask under one deadline; the frames | the journal helper (macOS and Linux; its driver not written yet); every command |
//! | [`run_wire`] | the wire forms of blaze-run this crate reads and writes (its registry entry and its `Toggle`), until they have one home | the viewer |
//!
//! Every machine is sans-IO: it takes an event, changes its state, and pushes the effects its
//! driver is to carry out into a buffer the caller owns. None reads a clock (time is a
//! [`Nanos`] in the event), randomness (a trait the driver implements), the environment or a
//! file; every table is in the allocator its maker passes. The one exception to "a machine" is
//! [`ring`], which is shared memory by design: its protocol is a fixed list of atomic
//! operations ([`ring::push_ops`]) and a drain that is itself a step machine
//! ([`ring::Drain`]), so the simulation interleaves both at every atomic operation.
//!
//! Not here: the drivers (sockets, files, the stack sampler, the timer). watch.md says what
//! each needs of bstd.
//!
//! Adapted from makepad/relax (MIT; licenses/relax-MIT.txt) where its design fits, each place
//! marked `// adapted from makepad/relax (MIT), <file>`.

// The tests keep std's prelude; the crate itself is built without it.
#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
// No unsafe code at all: the rings' payload words are atomics too (watch.md, "The emitter").
#![forbid(unsafe_code)]

/// blaze's one deterministic CBOR, by the name every module here uses.
pub use blaze_cbor as cbor;
pub mod conn;
pub mod daemon;
pub mod feed;
pub mod frame;
pub mod helper;
pub mod http;
pub mod pack;
pub mod pairing;
pub mod recorder;
pub mod relay;
pub mod ring;
pub mod run_wire;
pub mod since;
pub mod viewer;
pub mod window;

#[cfg(test)]
mod sim;

/// A time from the driver's clock, in nanoseconds. Only ever an event's field.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Nanos(pub u64);

impl Nanos {
    pub const fn after(self, by: Nanos) -> Nanos {
        Nanos(self.0.saturating_add(by.0))
    }

    pub const fn since(self, earlier: Nanos) -> Nanos {
        Nanos(self.0.saturating_sub(earlier.0))
    }

    pub const fn secs(secs: u64) -> Nanos {
        Nanos(secs.saturating_mul(1_000_000_000))
    }

    pub const fn millis(ms: u64) -> Nanos {
        Nanos(ms.saturating_mul(1_000_000))
    }
}

/// A position in one stream: the number of the event, from 1. 0 is "before the first", what a
/// reader that has seen nothing resumes from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Seq(pub u64);

impl Seq {
    pub const NONE: Seq = Seq(0);

    pub const fn next(self) -> Seq {
        Seq(self.0 + 1)
    }
}

/// What the driver gives a machine that needs fresh secret bytes (rule 75: randomness is passed
/// in). The real one reads the OS's generator; a simulation's is seeded.
pub trait Random {
    fn fill(&mut self, out: &mut [u8]);
}

/// A test-only switch that plants a known bug, for the oracles' controls (rule 53). Always
/// compiled, as blaze-run's is, so a plant is a value the simulation passes, not a build.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The ring's drain keeps a slot without checking its stamp again after the copy: a torn
    /// entry is delivered.
    NoRecheck,
    /// The drain counts one more lost entry than were overwritten when a ring was lapped.
    LostOffByOne,
    /// A producer waits for the reader before it overwrites a slot: emit blocks.
    WaitForReader,
    /// A feed resumed from `n` sends `n` again.
    ResumeInclusive,
    /// The viewer sends a page events past its queue bound.
    IgnorePageQueue,
    /// The viewer routes a request before it checks its origin.
    RouteBeforeOrigin,
    /// The viewer forwards a toggle before it checks the browser's key.
    ToggleBeforeKey,
    /// An open page keeps the viewer alive with no bound.
    PageHoldsForever,
}
