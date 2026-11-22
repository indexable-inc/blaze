//! Standard input (docs/v2/plan/design/std.md, `Out`'s row: "standard input (S5e)"): the bytes
//! whoever started the program hands it. A program reads them only if it was handed its
//! [`In`]: as `main`'s parameter `input: In` (a grant that fills itself, as `out: Out` does:
//! standard input reaches nothing outside the process), or from a world by [`Io::input`].
//!
//! **A byte stream, read as it comes.** [`In::read`] adds what the stream holds next, at most
//! the bytes asked, to the caller's [`Bytes`] and says how many; `0` means the input has ended.
//! [`In::read_into`] is the same read into a slice, in flight ([`Pending`]), as a socket's is.
//! Each read is one `Effect::ReadStdin` (blaze-core), answered in the order asked, and waits for
//! as long as nothing comes, until its context's deadline. A read that stopped (the deadline,
//! a cancel) leaves its effect asked of the driver, and blaze-core has no effect that takes one
//! back: bytes that then come to it are handed to no later read. So a program that must not
//! lose input reads it with a deadline only where it ends on that deadline.
//!
//! **Providers.** A native world reads descriptor 0 through its driver; the simulation reads
//! what its test scripted (`Sim::stdin`, `Sim::stdin_ends`, through [`Io::with_sim`]); a hosted
//! run's wire does not carry standard input yet, so a hosted `main` that takes `In` is refused
//! at its start, by name ([`crate::grant::Grant::hosted`] is `None` for it).
//!
//! **Typed rows beside it, later (not built).** `In` is the byte stream. A typed stream would be
//! two more grant kinds beside it, `Rows<T>` in and `Table<T>` out, whose row schema `T` is
//! written into the program's manifest by reflection, so a caller (a shell, `blaze run a | b`)
//! checks that one program's `Table<T>` fits the next one's `Rows<T>` before either runs, and
//! sends columns (blaze-columns, blaze-arrow-ipc; values as blaze-data) between them; a
//! `Table<T>` whose other end is a terminal or a plain pipe is rendered as text instead (a table
//! on a terminal, lines on a pipe), and a `Rows<T>` fed by a plain pipe parses that text.
//! Where the shape would go: `grant::Kind` gains `Rows { schema: &'static Schema }` and
//! `Table { schema: &'static Schema }` (`Kind` is `Copy` over `'static` data, so a schema the
//! macro makes as a `static` fits), `Manifest::write` prints it under a `"schema"` key, and
//! spec.rs's grant parsing treats both as filling themselves, as it does `In`. Nothing of it is
//! here.

use core::fmt;

use blaze_core::{Done, Effect, IoError, RECV_MOST};

use crate::{
    Stopped,
    cx::Cx,
    io::Io,
    mem::Bytes,
    pending::{self, Pending},
};

/// The planted bug of the read's test (`tests/input.rs`): the bytes kept are the slot asked
/// for, not the count read, so a short read leaves zeros after what came. Only the test that
/// must catch it plants it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum InPlant {
    None,
    KeepsSlot,
}

/// Why a read of standard input did not happen.
#[derive(Debug)]
pub enum InWhy {
    /// The context stopped first: the read may still be asked of the driver (the module's doc).
    Stopped(Stopped),
    /// The host refused the read, saying this (the descriptor closed, an error reading it).
    Host(IoError),
    /// The driver answered with a completion of another kind: a driver's bug.
    Answered(Done),
}

/// A read of standard input that failed, and why.
#[derive(Debug)]
pub struct InError {
    pub why: InWhy,
}

impl fmt::Display for InError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.why {
            InWhy::Stopped(stopped) => write!(f, "bstd::In::read_into: standard input: {stopped}"),
            InWhy::Host(said) => write!(f, "bstd::In::read_into: standard input: the host refused: {}", said.0),
            InWhy::Answered(done) => write!(f, "bstd::In::read_into: standard input: the driver answered {done:?}, not a read"),
        }
    }
}

impl core::error::Error for InError {}

impl From<Stopped> for InError {
    fn from(stopped: Stopped) -> InError {
        InError { why: InWhy::Stopped(stopped) }
    }
}

/// The process's standard input: from [`Io::input`], or `main`'s parameter `input: In`.
#[derive(Clone)] // clone: copying authority held adds none
pub struct In {
    io: Io,
}

/// Completion state for [`In::read_into`], stored inline inside [`Pending`] (zero heap alloc).
pub struct ReadInto {
    most: usize,
}

impl<'buf> pending::Complete<'buf> for ReadInto {
    type Output = Result<usize, InError>;

    fn complete(self, into: &'buf mut [u8], done: Result<Done, Stopped>) -> Self::Output {
        match done {
            Ok(Done::Read(bytes)) if bytes.len() <= self.most => {
                into[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            Ok(Done::Broken { error, .. }) => Err(InError { why: InWhy::Host(error) }),
            Ok(done) => Err(InError { why: InWhy::Answered(done) }),
            Err(stopped) => Err(InError::from(stopped)),
        }
    }
}

impl In {
    pub(crate) fn new(io: Io) -> In {
        In { io }
    }

    /// A read of what standard input holds next into `into`, at most `into.len()` bytes (and at
    /// most blaze-core's [`RECV_MOST`]), in flight: its outcome is how many, `0` once the input
    /// has ended (or for an empty `into`).
    #[track_caller]
    pub fn read_into<'buf>(&'buf self, into: &'buf mut [u8]) -> Pending<'buf, ReadInto> {
        let most = into.len().min(RECV_MOST);
        // alloc: none here, an empty buffer; the driver grows it to what it reads (one global
        // allocation a read, as `Done::Read`'s every reader pays until the page pool).
        let effect = move |tag| Effect::ReadStdin { most, into: Default::default(), tag };
        pending::start(&self.io, "bstd::in read_into", into, effect, ReadInto { most })
    }

    /// What standard input holds next, at most `most` bytes (and at most [`RECV_MOST`]), added
    /// to `into`; how many, `0` once the input has ended. Its [`In::read_into`], waited for
    /// under `cx`'s span `bstd.in.read`, until `cx`'s deadline. `into` is as it was on an error.
    pub fn read(&self, cx: &Cx, most: usize, into: &mut Bytes<'_>) -> Result<usize, InError> {
        self.read_in(cx, most, into, InPlant::None)
    }

    /// [`In::read`], with the test's planted bug when `plant` says so.
    pub(crate) fn read_in(&self, cx: &Cx, most: usize, into: &mut Bytes<'_>, plant: InPlant) -> Result<usize, InError> {
        let cx = cx.span("bstd.in.read");
        cx.stopped_by(self.io.now())?;
        let start = into.len();
        let asked = most.min(RECV_MOST);
        let slot = into.grow_zeroed(asked)?;
        match self.read_into(slot).wait(&cx) {
            Ok(read) => {
                let kept = match plant {
                    InPlant::None => read,
                    InPlant::KeepsSlot => asked,
                };
                into.truncate(start + kept);
                Ok(read)
            }
            Err(error) => {
                into.truncate(start);
                Err(error)
            }
        }
    }

    /// Whether standard input is a terminal (`isatty` on descriptor 0), read when asked: a
    /// person typing, not a file or a pipe. A simulated world's and a hosted run's are not.
    pub fn is_terminal(&self) -> bool {
        self.io.input_is_terminal()
    }
}

impl fmt::Debug for In {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("In")
    }
}

impl crate::grant::Grant for In {
    const KIND: crate::grant::Kind = crate::grant::Kind::In;

    /// Standard input fills itself: the process's own, as `Out`'s streams are.
    fn grant(from: &crate::grant::Granting<'_>) -> Result<In, crate::grant::Want> {
        from.process().need_input().map_err(crate::grant::Want::NotOffered)
    }

    // `hosted` is the trait's: `None`, as the hosted wire does not carry standard input yet
    // (guest.rs encodes no `Effect::ReadStdin`), so a hosted start that asks for `input` is
    // refused naming it rather than failing at its first read.
}
