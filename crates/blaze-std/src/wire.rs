//! The wire between a hosted program and its host (docs/v2/plan/design/run.md, "The hosted
//! world"): a versioned `#[repr(C)]` table of function pointers the host hands to the
//! program's `bstd_hosted_start`, and the plain answer every entry returns.
//!
//! Under `blaze run` the parent process already holds a complete driver (blaze is a bstd
//! program), so a program blaze compiled carries none of its own: its [`crate::io::Io`] is
//! backed by [`crate::guest`], which forwards each reach to this table, and the host answers
//! from its driver ([`crate::hosting`]). Every way of holding a run (record, replay, fake, dry,
//! ask) is then the host wrapping its driver, with no change to the program.
//!
//! **Shape.** A hot reach (a write to a standard stream, a clock reading, an argument, a grant,
//! the exit) is one indirect call with integer arguments and an [`Answer`] of two integers: no
//! allocation and no encoding on either side. Every other effect goes through [`HostTable::effect`],
//! the general escape: the effect as deterministic CBOR (blaze-cbor), the tree's one strict
//! format.
//!
//! **Bytes cross through the exchange.** The host owns one buffer, [`HostTable::exchange`],
//! of [`HostTable::exchange_len`] bytes, and every byte argument is a range `(at, len)` inside
//! it: the program copies what it says there, and the host copies what it answers there. The
//! host checks each range against the buffer it made before it reads a byte, so a length past
//! the buffer, an offset past it, or a sum that overflows is refused with
//! [`code::OUT_OF_RANGE`] and nothing outside it is read. The program is not trusted (the
//! review rule "Trust"): every handle (a stream, a clock, an argument's index) is checked by
//! the host too, and an unknown one is [`code::UNKNOWN`].
//!
//! **Threads.** The host's entries may be called from any of the program's threads, one at a
//! time: the program serializes its calls (its world's lock), and the host's entries are safe
//! to call from whichever thread that is.
//!
//! **Waiting.** For now a program is one thread and its side of every entry is a plain call:
//! an effect that blocks (a timer, a read) is waited for by the host inside the call, under
//! the budget the call carries, and the call returns with its answer. When programs have
//! tasks of their own, the escape splits in two, as [`blaze_core::Driver`] is: a `submit`
//! that returns at once with the effect's tag, and a `poll` of completions under a deadline
//! that parks the program's worker on the host; each worker then has its own exchange.
//!
//! This file is written plainly on purpose (no generics, closures, trait objects, format
//! calls or atomics): it is on the path the new compiler must compile first.

use core::ffi::c_void;

/// The wire's version: a table of another is refused before any entry is called.
pub const VERSION: u32 = 1;

/// What every entry answers: a [`code`] and, when it is [`code::OK`], a value (a length, a
/// reading, a status).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Answer {
    pub code: u32,
    pub value: u64,
}

impl Answer {
    /// `value`, answered.
    pub const fn ok(value: u64) -> Answer {
        Answer { code: code::OK, value }
    }

    /// Refused with `code`.
    pub const fn refused(code: u32) -> Answer {
        Answer { code, value: 0 }
    }
}

/// An [`Answer`]'s codes. A plain `u32` on the wire, never a Rust enum: a value the other
/// side made up must not be a discriminant no variant has. Each side reads it into a
/// [`Refusal`].
pub mod code {
    /// Done; the answer's value holds what was asked.
    pub const OK: u32 = 0;
    /// A handle (a stream, a clock, a grant's kind) the host does not know.
    pub const UNKNOWN: u32 = 1;
    /// A range `(at, len)` not inside the exchange.
    pub const OUT_OF_RANGE: u32 = 2;
    /// The host withholds it (a grant the run does not give, a capability it does not offer).
    pub const REFUSED: u32 = 3;
    /// The host tried and its system failed (a write to a closed stream).
    pub const FAILED: u32 = 4;
    /// Past the last: an argument's index past the command line.
    pub const END: u32 = 5;
    /// The escape's bytes are not an effect of this wire's version.
    pub const BAD_BYTES: u32 = 6;
    /// The answer does not fit in the exchange.
    pub const TOO_SMALL: u32 = 7;
}

/// The standard streams, as [`HostTable::write`] names them.
pub mod stream {
    pub const OUT: u32 = 1;
    pub const ERR: u32 = 2;
}

/// The clocks, as [`HostTable::clock`] names them.
pub mod clock {
    /// The driver's monotonic clock, in ns since the driver began.
    pub const MONOTONIC: u32 = 1;
    /// The wall clock, in ns since the Unix epoch.
    pub const WALL: u32 = 2;
}

/// What one side reads a refused [`Answer`] as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    Unknown,
    OutOfRange,
    Refused,
    Failed,
    End,
    BadBytes,
    TooSmall,
    /// A code this version does not have: the other side is broken or of another version.
    NoSuchCode(u32),
}

impl Refusal {
    /// `answer` read: its value, or why it was refused.
    pub const fn of(answer: Answer) -> Result<u64, Refusal> {
        match answer.code {
            code::OK => Ok(answer.value),
            code::UNKNOWN => Err(Refusal::Unknown),
            code::OUT_OF_RANGE => Err(Refusal::OutOfRange),
            code::REFUSED => Err(Refusal::Refused),
            code::FAILED => Err(Refusal::Failed),
            code::END => Err(Refusal::End),
            code::BAD_BYTES => Err(Refusal::BadBytes),
            code::TOO_SMALL => Err(Refusal::TooSmall),
            other => Err(Refusal::NoSuchCode(other)),
        }
    }

    /// In words.
    pub const fn says(self) -> &'static str {
        match self {
            Refusal::Unknown => "a handle the host does not know",
            Refusal::OutOfRange => "a range outside the exchange",
            Refusal::Refused => "refused by the host, or by the person running the program",
            Refusal::Failed => "the host's system failed",
            Refusal::End => "past the last",
            Refusal::BadBytes => "bytes that are not an effect of this wire",
            Refusal::TooSmall => "an answer larger than the exchange",
            Refusal::NoSuchCode(_) => "an answer code this wire does not have",
        }
    }
}

/// The table a host hands its program. Every entry takes [`HostTable::host`] first.
///
/// A table of another [`VERSION`], or whose [`HostTable::size`] is smaller than this
/// version's, is refused by the program before any entry is called; a later version may add
/// entries at the end only.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostTable {
    pub version: u32,
    /// `size_of::<HostTable>()` as the host built it.
    pub size: u32,
    /// The host's own state, opaque to the program.
    pub host: *mut c_void,
    /// The buffer every byte argument and answer lies in, owned by the host for the run.
    pub exchange: *mut u8,
    pub exchange_len: usize,
    /// `exchange[at..at + len]` written to the standard stream `stream` ([`stream`]); the
    /// answer's value is the bytes written. One call per flush of the program's buffer.
    pub write: unsafe extern "C" fn(host: *mut c_void, stream: u32, at: usize, len: usize) -> Answer,
    /// A reading of the clock `which` ([`clock`]), in ns.
    pub clock: unsafe extern "C" fn(host: *mut c_void, which: u32) -> Answer,
    /// The command line's `index`th argument, its bytes written at the start of the
    /// exchange; the answer's value is its length. [`code::END`] past the last.
    pub arg: unsafe extern "C" fn(host: *mut c_void, index: u64) -> Answer,
    /// Whether the run grants the parameter named `exchange[name_at..][..name_len]` of the
    /// kind named `exchange[kind_at..][..kind_len]` ([`crate::grant::Kind::name`]):
    /// [`code::OK`] or [`code::REFUSED`].
    pub grant: unsafe extern "C" fn(host: *mut c_void, kind_at: usize, kind_len: usize, name_at: usize, name_len: usize) -> Answer,
    /// The escape: the effect encoded at `exchange[at..at + len]`, waited for by the host
    /// within `budget_ns`; its completion encoded at the start of the exchange, the answer's
    /// value its length ([`crate::guest`] holds the encoding).
    pub effect: unsafe extern "C" fn(host: *mut c_void, at: usize, len: usize, budget_ns: u64) -> Answer,
    /// The run ends with `status`: the host flushes what it holds and ends it. It does not
    /// return; a host that runs the program on its own stack (a test) unwinds through it,
    /// which is why it is the one `C-unwind` entry.
    pub exit: unsafe extern "C-unwind" fn(host: *mut c_void, status: i32) -> !,
}
