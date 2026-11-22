//! The executor seam (READINESS.md 1a, S2) and the sandbox-off refusal's type (S5).
//!
//! The strategy that answers a spawn is a value (`&dyn Executor`), chosen once per command by its
//! setting (rule 78). `Off` is the strategy when no sandbox runs on this machine: it refuses every
//! spawn by name and never runs the program natively (rule 3; D92, D94).

use core::fmt;

use blaze_own::{AString, Alloc};

use crate::{SpawnAnswer, SpawnRequest};

/// The bytes behind a request's `reads` and `present`, by guest path.
pub trait Files {
    fn bytes(&self, path: &str) -> Option<&[u8]>;
}

/// A strategy that answers spawn requests.
pub trait Executor {
    /// The strategy's name, for the profile and for messages.
    fn name(&self) -> &'static str;

    /// The answer to `request`, made in `alloc`, or the typed refusal naming the program.
    fn answer<'a>(
        &self,
        request: &SpawnRequest<'_>,
        files: &dyn Files,
        alloc: Alloc<'a>,
    ) -> Result<SpawnAnswer<'a>, SpawnRefused<'a>>;
}

/// Why no sandbox runs on this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoSandbox {
    /// The owner's guard file is absent (D94).
    OwnerHasNotAllowed,
    /// A macOS tool needs an Apple userland this host does not have (open question Q1).
    NoAppleUserland,
    /// The program has no pinned bytes.
    ToolNotPinned,
}

impl fmt::Display for NoSandbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NoSandbox::OwnerHasNotAllowed => "the owner has not allowed hvbox to run (D94)",
            NoSandbox::NoAppleUserland => "this host has no Apple userland for a macOS tool (an open question for the owner)",
            NoSandbox::ToolNotPinned => "the tool has no pinned bytes",
        })
    }
}

/// Why a spawn got no answer. Every variant names the program first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpawnRefused<'a> {
    /// No sandbox runs here; the program is not run natively.
    NoSandbox { program: AString<'a>, why: NoSandbox },
    /// The sandbox stopped the program at its deadline.
    Deadline { program: AString<'a>, after_ms: u64 },
    /// The sandbox refused something the program did, named in `what`.
    Refused { program: AString<'a>, what: AString<'a> },
    /// The sandbox's helper ended without an answer.
    Helper { program: AString<'a>, status: i32, last_line: AString<'a> },
}

impl fmt::Display for SpawnRefused<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpawnRefused::NoSandbox { program, why } => write!(
                f,
                "{program}: runs only in the deterministic sandbox, which is off on this machine: {why} (D92, D94); not run natively (rule 3)"
            ),
            SpawnRefused::Deadline { program, after_ms } => {
                write!(f, "{program}: the sandbox stopped it at its deadline, after {after_ms} ms")
            }
            SpawnRefused::Refused { program, what } => write!(f, "{program}: the sandbox refused it: {what}"),
            SpawnRefused::Helper { program, status, last_line } => {
                write!(f, "{program}: the sandbox's helper ended with status {status} and no answer: {last_line}")
            }
        }
    }
}

impl core::error::Error for SpawnRefused<'_> {}

/// The strategy when no sandbox runs: every spawn is refused, naming the program and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Off {
    pub why: NoSandbox,
}

impl Executor for Off {
    fn name(&self) -> &'static str {
        "off"
    }

    fn answer<'a>(
        &self,
        request: &SpawnRequest<'_>,
        _files: &dyn Files,
        alloc: Alloc<'a>,
    ) -> Result<SpawnAnswer<'a>, SpawnRefused<'a>> {
        Err(SpawnRefused::NoSandbox { program: AString::from_str_in(&request.program.name, alloc), why: self.why })
    }
}
