//! blaze's launcher under the `edit` goal, step 1 (docs/v2/plan/design/launcher.md): what
//! decides, with nothing that does.
//!
//! A program built for `edit` is not linked. A small launcher, the program's own process, maps
//! compiled functions from the store into a code area and reaches each through one *slot*, a
//! word of a table: the lazy stub (the first call asks the daemon for the function), the patch
//! point of an edit and the switch between tiers are all a store to that word. This crate is
//! the launcher's core and the wire it speaks:
//!
//! | module | what |
//! |---|---|
//! | [`area`] | the code area: appended to, an offset never handed out twice, whole freed pages given back |
//! | [`launch`] | [`Launch`]: the slot table, the bodies, an edit staged and committed all or nothing (through a gate when its binds must switch together), and the retirement of replaced bodies by per-thread epochs |
//! | [`wire`] | the patch protocol between the launcher and the daemon: one versioned schema in deterministic CBOR, written and read without an allocator of its own |
//!
//! [`Launch`] is one sans-IO core: a synchronous state machine that maps no memory, writes no
//! code, reads no clock and knows no instruction set. It hands back [`Write`]s, in the order
//! a driver must carry them out, and takes what threads report. The drivers (step 2: Linux
//! and macOS) do the mapping, the copying, the hashing and the stack walks. So the hard part,
//! which is the order of the stores against threads that never stop, runs in a deterministic
//! simulation (`tests/sim.rs`) whose oracle states the invariants and whose controls plant a
//! bug each.
//!
//! It is `no_std` on `core` and `alloc` alone: its tables are in the allocator its maker hands
//! it ([`Launch::new`], a [`Kept`]), a message is encoded into the caller's buffer. It is not
//! on bstd (docs/v2/plan/design/std.md) yet, and that is a decision, not an oversight: bstd
//! uses syntax only blaze accepts, and while cargo with the pinned nightly builds and tests
//! blaze in the dev loop (AGENTS.md, `blaze-builds-blaze`, as of 2026-11-11) a crate that
//! must be verified now cannot depend on it. When the rule returns to blaze-only, [`Kept`]
//! becomes `bstd::mem::Kept` (one allocator a subsystem tag) and nothing else here changes:
//! every table already names its allocator.

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(unsafe_code)]

extern crate alloc;

pub mod area;
pub mod launch;
pub mod wire;

#[cfg(test)]
mod tests;

use core::fmt;

pub use launch::{Bind, Committed, Config, Edit, Launch, Placed, Target, Write};

/// The allocator of the core's tables, which live as long as the process: the global heap
/// until this crate is on bstd (the crate's documentation says when), then `bstd::mem::Kept`.
pub type Kept = alloc::alloc::Global;

/// The one [`Kept`].
pub const KEPT: Kept = alloc::alloc::Global;

/// A function's slot: its index in the table. A slot is a path and an ABI, so a function
/// whose signature changes gets another (launcher.md, section 4).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Slot(pub u32);

/// A thread of the program, by the id its driver gives it for the thread's life.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Thread(pub u64);

/// One edit's transaction, numbered by the daemon.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Txn(pub u64);

/// The tier of a body (`blaze_api::Goal`'s two levels; docs/v2/plan/design/tiers.md).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Tier {
    /// Copy-and-patch code: what `edit` waits for.
    Zero,
    /// LLVM's, landed by promotion; taken away again when an edit changes a function it
    /// built in.
    Optimized,
}

/// A function's code identity: what its body was compiled from (`fn_info(&f).code_id`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CodeId(pub [u8; 32]);

/// Bytes of the code area, by offset from where the driver mapped it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Range {
    pub at: u64,
    pub len: u64,
}

impl Range {
    pub fn end(&self) -> u64 {
        self.at + self.len
    }

    /// Whether a thread with `frame` is in this range. A program counter is in `[at, end)`. A
    /// return address is in `(at, end]`: it follows a call, so it is never the first byte,
    /// and it is the end itself when the call is the range's last instruction (a call that
    /// does not return), where the next range may begin.
    pub fn holds(&self, frame: Frame) -> bool {
        match frame.kind {
            FrameKind::Pc => frame.at >= self.at && frame.at < self.end(),
            FrameKind::Return => frame.at > self.at && frame.at <= self.end(),
        }
    }
}

/// How a thread's frame names code.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FrameKind {
    /// Where the thread is running: its program counter (a thread stopped from outside, or
    /// one inside a gate).
    Pc,
    /// Where a call of the thread returns to.
    Return,
}

/// One frame of a thread's stack that lies in the code area, as an offset in it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Frame {
    pub at: u64,
    pub kind: FrameKind,
}

/// Why the core refused a call. Each names what it is about; none changes the core's state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// A [`Config`] the core cannot be made from.
    Config { what: &'static str, value: u64 },
    /// The code area has no room for what was asked: the program is restarted in place.
    Full { need: u64, room: u64 },
    /// More slots than the table was made for.
    Slots { asked: u32, declared: u32, most: u32 },
    /// A slot that was never declared.
    NoSlot { slot: Slot, declared: u32 },
    /// An edit with no bind.
    Empty { txn: Txn },
    /// An edit that binds one slot twice.
    Twice { txn: Txn, slot: Slot },
    /// A bind whose code has no length, or an alignment that is not a power of two.
    Code { txn: Txn, slot: Slot, what: &'static str, value: u64 },
    /// An edit staged while another is: one transaction at a time.
    Open { open: Txn, asked: Txn },
    /// A commit or an abort of a transaction that is not the staged one.
    NotStaged { asked: Txn, open: Option<Txn> },
    /// A thread the core was not told of.
    NoThread { thread: Thread },
    /// A thread the core was told of twice.
    ThreadTwice { thread: Thread },
    /// An epoch a thread cannot have loaded: past the last the core published.
    Epoch { thread: Thread, said: u64, what: &'static str, against: u64 },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::Config { what, value } => write!(f, "the launcher's core: {what} ({value})"),
            Error::Full { need, room } => write!(
                f,
                "the code area has {room} bytes left and {need} are needed: the program must \
                 be started again"
            ),
            Error::Slots { asked, declared, most } => write!(
                f,
                "{asked} more slots asked for with {declared} declared: the table holds {most}"
            ),
            Error::NoSlot { slot, declared } => {
                write!(f, "slot {} was not declared: the table has {declared}", slot.0)
            }
            Error::Empty { txn } => write!(f, "edit {} binds nothing", txn.0),
            Error::Twice { txn, slot } => {
                write!(f, "edit {} binds slot {} twice", txn.0, slot.0)
            }
            Error::Code { txn, slot, what, value } => {
                write!(f, "edit {}, slot {}: {what} ({value})", txn.0, slot.0)
            }
            Error::Open { open, asked } => write!(
                f,
                "edit {} was staged while edit {} is: one transaction at a time",
                asked.0, open.0
            ),
            Error::NotStaged { asked, open: Some(open) } => {
                write!(f, "edit {} is not the staged one, which is edit {}", asked.0, open.0)
            }
            Error::NotStaged { asked, open: None } => {
                write!(f, "edit {} is not staged: nothing is", asked.0)
            }
            Error::NoThread { thread } => {
                write!(f, "thread {} was never said to have begun", thread.0)
            }
            Error::ThreadTwice { thread } => {
                write!(f, "thread {} was said to have begun twice", thread.0)
            }
            Error::Epoch { thread, said, what, against } => {
                write!(f, "thread {} reported epoch {said}, which {what} ({against})", thread.0)
            }
        }
    }
}

impl core::error::Error for Error {}
