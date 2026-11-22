//! A call blaze did not take an expansion from: which macro, why, and the rule that refused it
//! (rule 19). Each refusal is one of the run's own ends (blaze-comptime's [`Ended`]), the
//! server's account of why it stopped, the host's check of the outcome, or a question the host
//! refused; none is ever answered by another route (rule 3).

use core::fmt;

use blaze_comptime::{Ended, Grant};
use blaze_expand::ProcKind;
use blaze_own::AVec;

use crate::outcome::OutcomeError;
use crate::server::{Place, ServerError};

/// Why the host refused `span_file`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileWhy {
    /// A site's file: a site is no place in a file.
    Site,
    /// A place past the call's tokens, which the process was never handed.
    Forged { place: Place },
    /// A token of the call that an expansion made, so no file holds it.
    MadeToken { place: Place },
    /// The caller did not show the call's file.
    Unknown,
    /// Arguments that are not one place.
    Malformed,
}

impl fmt::Display for FileWhy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileWhy::Site => f.write_str("asked `span_file` of a site, which is no place in a file"),
            FileWhy::Forged { place } => write!(f, "asked `span_file` of {place:?}, which is past the call's tokens: a forged span"),
            FileWhy::MadeToken { place } => write!(f, "asked `span_file` of {place:?}, a token an expansion made, which no file holds"),
            FileWhy::Unknown => f.write_str("asked `span_file`, and the call's file was not shown to the run"),
            FileWhy::Malformed => f.write_str("asked `span_file` with arguments that are not one place"),
        }
    }
}

/// Why a call was refused.
#[derive(Debug)]
pub enum Why<'a> {
    /// The run ended without an outcome: its deadline, its fuel, a crash, or reads outside its
    /// roots (blaze-comptime).
    Ended(Ended<'a>),
    /// The process stopped the call, in its own words (server.rs).
    Server(ServerError),
    /// The outcome is not one the host takes (outcome.rs).
    Outcome(OutcomeError),
    /// `tracked_path::path` of a path outside the package: refused by name, never looked up.
    Outside { path: AVec<'a, u8>, grant: Grant },
    /// `span_file`, refused.
    File(FileWhy),
    /// A bridge method the session never forwards (only `span_file` crosses as a bridge call).
    Bridge { method: u8 },
}

impl Why<'_> {
    /// The rule that refused it.
    pub fn rule(&self) -> u32 {
        match self {
            Why::Ended(Ended::Deadline { .. } | Ended::Fuel { .. } | Ended::NotEnded { .. }) => 18,
            Why::Ended(Ended::Reads(_)) | Why::Outside { .. } => 9,
            Why::Ended(Ended::Crashed(_)) => 3,
            Why::Server(ServerError::Position { .. }) => 71,
            Why::Server(ServerError::Limit(_)) => 18,
            Why::Server(_) | Why::Bridge { .. } => 3,
            Why::Outcome(OutcomeError::TooMany { .. }) => 18,
            Why::Outcome(OutcomeError::Malformed(_)) => 3,
            // What comes back from sandboxed code is data blaze checks, never a mark it trusts.
            Why::Outcome(_) => 9,
            Why::File(FileWhy::Forged { .. } | FileWhy::Malformed) => 9,
            Why::File(_) => 71,
        }
    }
}

impl fmt::Display for Why<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Why::Ended(Ended::Deadline { after_ns }) => write!(f, "ran past its deadline ({after_ns} ns) and was stopped"),
            Why::Ended(Ended::Fuel { instructions, cpu_ns, calls }) => {
                write!(f, "ran out of fuel ({instructions} instructions, {cpu_ns} ns of CPU, {calls} calls) and was stopped")
            }
            Why::Ended(ended) => write!(f, "ended without an outcome: {ended:?}"),
            Why::Server(error) => write!(f, "{error}"),
            Why::Outcome(error) => write!(f, "{error}"),
            Why::Outside { path, grant } => {
                let path = core::str::from_utf8(path).unwrap_or("(a path that is not UTF-8)");
                write!(f, "tracked the path `{path}`, which is outside what it may read ({grant:?})")
            }
            Why::File(why) => write!(f, "{why}"),
            Why::Bridge { method } => write!(f, "sent blaze the bridge method {method}, which only crosses inside the process"),
        }
    }
}

/// A call refused: the macro, its kind and why.
#[derive(Debug)]
pub struct Refused<'p, 'a> {
    /// The macro's crate.
    pub krate: &'p str,
    pub name: &'p str,
    pub kind: ProcKind,
    pub why: Why<'a>,
}

impl Refused<'_, '_> {
    pub fn rule(&self) -> u32 {
        self.why.rule()
    }
}

impl fmt::Display for Refused<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the {} macro `{}::{}` {} (rule {})", self.kind, self.krate, self.name, self.why, self.why.rule())
    }
}
