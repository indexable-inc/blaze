//! blaze's dev runtime: the run process's side of a dev run on the daemon's code memory
//! (docs/v2/plan/design/compile-time.md), and what both sides share.
//!
//! - [`runtime`]: the start hook, `blaze_inproc_start`, which the link puts first among a dev
//!   executable's initializers. Before `main` it attaches the process to the daemon and lays the
//!   daemon's code memory over the executable's code. Also the process's quiescent points and
//!   its asks.
//! - [`live`]: the descriptor and jump table of an image blaze-link linked live, and their run
//!   side every dev executable carries: the execution log a test binary's recording fork server
//!   arms (`__blaze_exec_slots`, `__blaze_exec_arm`) and the tier counters
//!   (`blaze_live_counters`). blaze-live builds the switch, which takes patches, on them.
//! - The channel's frames ([`frame`], [`channel`]), the handshake and its checks
//!   ([`handshake`], [`bind`]), the image's headers ([`image`]), the asks ([`ask`]), the fence's
//!   words ([`fence`]) and the platform ([`sys`]): each spoken by the daemon (`blaze-inproc`,
//!   which builds its side on this crate) and by the run.
//!
//! One source, two builds:
//!
//! - In blaze's workspace, `crates/blaze-inproc-rt` builds these files as an ordinary crate.
//!   `blaze-inproc` depends on it, and so does every program that opts in through `blaze-live`.
//! - In the standard library's workspace (library/blaze_inproc_rt, beside std), blaze builds it
//!   as one more crate of the library and links it into every dev executable whose own
//!   dependencies don't link it: a binary or a test of this machine's own target under a dev
//!   profile (`blaze_units::stdlib::DEV_RUNTIME`). So a dev executable has its start hook
//!   whatever it depends on, and blaze runs it on its code memory, never from its file.
//!
//! So it depends on std and libc alone (windows-sys on Windows, built only in blaze's
//! workspace): the standard library's workspace has nothing else. Its locks and channels are
//! std's and its spin its own ([`ask`]'s `Relax`), not `parking_lot`, crossbeam or `blaze-spin`.
//!
//! What the daemon's side (`blaze-inproc`) builds on is `pub`; the modules a program never
//! names ([`bind`], [`handshake`], [`sys`]) are hidden from the docs.

#![expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "a crate of the standard library's workspace too, which has no parking_lot, crossbeam or blaze-spin: std's locks and channels and its own bounded spin stand in"
)]

pub mod ask;
#[doc(hidden)]
pub mod bind;
pub mod channel;
pub mod fence;
pub mod frame;
#[doc(hidden)]
pub mod handshake;
pub mod image;
pub mod live;
pub mod runtime;
#[doc(hidden)]
pub mod sys;

use std::fmt;

pub use image::{Layout, Segment};

/// Why something of this crate could not be done, and on which platform.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Error {
    pub what: String,
}

impl Error {
    #[doc(hidden)]
    pub fn new(what: impl Into<String>) -> Error {
        Error { what: what.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "blaze-inproc on {}-{}: {}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            self.what
        )
    }
}

impl std::error::Error for Error {}

/// The object format of a program's executable.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Format {
    Elf,
    MachO,
    Pe,
}

impl Format {
    #[doc(hidden)]
    pub fn tag(self) -> u8 {
        match self {
            Format::Elf => 1,
            Format::MachO => 2,
            Format::Pe => 3,
        }
    }

    #[doc(hidden)]
    pub fn from_tag(tag: u8) -> Option<Format> {
        match tag {
            1 => Some(Format::Elf),
            2 => Some(Format::MachO),
            3 => Some(Format::Pe),
            _ => None,
        }
    }

    /// The format of programs on this platform.
    pub fn host() -> Option<Format> {
        if cfg!(target_os = "linux") {
            Some(Format::Elf)
        } else if cfg!(target_os = "macos") {
            Some(Format::MachO)
        } else if cfg!(windows) {
            Some(Format::Pe)
        } else {
            None
        }
    }
}

/// How a range of a memory object is mapped in a run process.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Access {
    /// Readable and executable, shared with the daemon's writable view: code.
    Execute,
    /// Readable, shared: read-only data the daemon may still change.
    Read,
    /// Readable and writable, a private copy: data. Nothing relies on whether the daemon's
    /// later writes show through: on Linux and Windows they do in pages the process has not
    /// written (copy on write), on macOS never (a snapshot taken when it is mapped, since a
    /// mapping of a Mach memory entry can't gain more rights than the entry has).
    Copy,
    /// Readable and writable, shared both ways: the fence's control words, and cells.
    Shared,
}

impl Access {
    #[doc(hidden)]
    pub fn tag(self) -> u8 {
        match self {
            Access::Execute => 1,
            Access::Read => 2,
            Access::Copy => 3,
            Access::Shared => 4,
        }
    }

    #[doc(hidden)]
    pub fn from_tag(tag: u8) -> Option<Access> {
        match tag {
            1 => Some(Access::Execute),
            2 => Some(Access::Read),
            3 => Some(Access::Copy),
            4 => Some(Access::Shared),
            _ => None,
        }
    }
}

/// An 8-byte word of a fixed map's run-private copy that the start hook stores before any code
/// of the map runs: `blaze-premap`'s binding of a dependency image's open cells to a definition
/// the image's link could not see (`blaze_premap::start::Write`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ImageWrite {
    /// Linked, inside a fixed map (`blaze_inproc::FixedMap`) of
    /// [`Access::Copy`]; the run's slide is added.
    pub place: u64,
    pub value: u64,
    /// `value` is a linked address, which moves with the run's slide as `place` does; a
    /// thread-local's offset from the thread pointer does not.
    pub address: bool,
}

/// An 8-byte word of a fixed map's run-private copy bound to `name` in the run process's
/// global scope, which the system loader filled from the executable's libraries
/// (`blaze_premap::start::Lookup`): the start hook stores its address plus `addend`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ImageLookup<'a> {
    /// Linked, inside a fixed map (`blaze_inproc::FixedMap`) of
    /// [`Access::Copy`]; the run's slide is added.
    pub place: u64,
    /// As the image's symbol table has it: on Mach-O with its leading `_`, which the hook takes
    /// off for `dlsym`.
    pub name: &'a str,
    pub addend: i64,
    /// 0 when nothing defines it; a strong one nothing defines fails the start, naming it.
    pub weak: bool,
}
