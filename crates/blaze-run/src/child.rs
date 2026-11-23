//! [`Child`]: one forked test process, from the fork to its one report (docs/v2/plan/design/test.md,
//! "End state", "One process per test"; piece 9). The zygote forks; the child takes its
//! [`Step`]s in order from [`Child::next`] (its driver makes the system call each names), calls
//! the test's row, and ends with ONE report on descriptor 3 ([`Child::report`]) and `_exit(0)`.
//! A child that exits any other way has no report, and the parent fails the test for it.
//!
//! The steps, and why in this order:
//!
//! 1. [`Step::OwnGroup`]: a process group of its own, first, so the parent's kill at the
//!    deadline reaches everything the test starts (the parent also sets it, so neither order of
//!    the two races).
//! 2. [`Step::Signals`]: every disposition back to its default and the mask empty (the zygote's
//!    own `SIGCHLD` handler must never run here), then the stacks handler on `SIGQUIT`, which
//!    writes a `Stacks` message (wire.rs) when the parent asks past the deadline.
//! 3. [`Step::Descriptors`]: the report pipe on 3, the output pipe on 1 and 2 (one pipe, so the
//!    two streams keep their order), stdin from `/dev/null`, and every other descriptor closed:
//!    the zygote's ends of its other children's pipes too, or their ends would never be seen.
//! 4. [`Step::TempFolder`]: the test's own temporary folder, made (mode 0700).
//! 5. [`Step::Environment`]: the process's environment replaced by the run spec's, plus `TMPDIR`
//!    naming the temporary folder (run.md, "What a program is given").
//! 6. [`Step::Sandbox`]: the test's sandbox ([`seatbelt`] on macOS, applied with `sandbox_init`
//!    while the child is one thread; Linux: not built, the driver stops by name). Last of the
//!    system's steps: the ones above need what the sandbox denies (a folder made, `/dev/null`).
//! 7. [`Step::Control`]: the launcher's control thread for this child, so a function the zygote
//!    had not placed can be asked for (launcher.md; table.rs stops a child that reaches one
//!    until the launcher serves a fork). The child's first thread: from here it is not one
//!    thread, so nothing above may come after it.
//! 8. [`Step::Call`]: the test's row.
//!
//! Between the fork and [`Step::Call`] the child calls only what is sound there: the zygote was
//! one thread at the fork (`zygote::Gate`), so no lock is held by a thread that is gone, and
//! the allocator and the C library are usable; what is not usable is any state of a system
//! framework with its own threads or Mach ports (zygote.rs says which).
//!
//! While the test runs, the launcher's recording wrappers call [`Observe::saw`] on the child:
//! each operation is classified (oscall.rs) into an ask kept for the report, a not-kept reason
//! (the least by [`NotKept`]'s order, so the call order does not choose it), or a refusal, which
//! ends the test as refused naming the thing.

#![blaze::sans_io]

use core::fmt;

use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::oscall::{Allowed, Asked, Class, Observe, Op, classify, first_reason};
use crate::plan::NotKept;
use crate::postings::{FnName, ReadName};
use crate::result::{Place, Refusal, capped};
use crate::wire::{self, Outcome, Report, WireError};

/// The exit code of a child whose step `n` (0-based, in [`Step`]'s order) failed: `SETUP + n`.
/// It writes the step and the error to its output (captured) and no report.
pub const SETUP: i32 = 120;

/// One test's child, as the zygote's plan gives it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Spec<'s> {
    /// The test's row in its unit's tests record.
    pub row: u32,
    /// The test's own temporary folder: absolute, canonical, not yet made.
    pub temp: &'s str,
    /// The run spec's environment, sorted by name and each name once; never `TMPDIR`.
    pub env: &'s [(&'s str, &'s str)],
    /// The launcher's socket, when the child is to start a control thread.
    pub control: Option<&'s str>,
}

/// One thing the child's driver does, in order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step<'c> {
    OwnGroup,
    Signals,
    Descriptors,
    TempFolder { path: &'c str },
    Environment { vars: &'c [(&'c str, &'c str)], temp: &'c str },
    Sandbox { profile: &'c str },
    /// Start the launcher's control thread (only when the spec names a socket).
    Control { socket: &'c str },
    Call { row: u32 },
}

impl Step<'_> {
    /// The step's index, for [`SETUP`].
    pub const fn index(&self) -> i32 {
        match self {
            Step::OwnGroup => 0,
            Step::Signals => 1,
            Step::Descriptors => 2,
            Step::TempFolder { .. } => 3,
            Step::Environment { .. } => 4,
            Step::Sandbox { .. } => 5,
            Step::Control { .. } => 6,
            Step::Call { .. } => 7,
        }
    }

    pub const fn name(&self) -> &'static str {
        match self {
            Step::OwnGroup => "its own process group",
            Step::Signals => "default signal dispositions",
            Step::Descriptors => "descriptors 1, 2 and 3",
            Step::TempFolder { .. } => "its temporary folder",
            Step::Environment { .. } => "its environment",
            Step::Sandbox { .. } => "its sandbox",
            Step::Control { .. } => "its control thread",
            Step::Call { .. } => "the test's call",
        }
    }
}

/// A spec refused before anything runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChildError<'s> {
    /// A path the profile must name is not absolute and canonical.
    NotCanonical { path: &'s str },
    /// A path holds a quote, a backslash or a control character, which a profile cannot say.
    Unsayable { path: &'s str },
    /// The environment is not sorted by name, each once, at this index.
    EnvOrder { at: usize },
    /// The environment names `TMPDIR`, which is the child's to set.
    EnvTemp,
    /// [`Child::report`] called twice.
    Reported,
}

impl fmt::Display for ChildError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ChildError::NotCanonical { path } => {
                write!(f, "{path}: a test's sandbox takes only absolute paths without `.`, `..` or `//`")
            }
            ChildError::Unsayable { path } => {
                write!(f, "{path}: a quote, backslash or control character cannot be put in a test's sandbox profile")
            }
            ChildError::EnvOrder { at } => write!(f, "a test's environment is not sorted by name, each once, at {at}"),
            ChildError::EnvTemp => f.write_str("a test's environment names TMPDIR, which names the test's own folder"),
            ChildError::Reported => f.write_str("a test child reports once"),
        }
    }
}

impl core::error::Error for ChildError<'_> {}

/// `path` checked to be absolute, canonical and sayable in a profile.
pub fn checked(path: &str) -> Result<&str, ChildError<'_>> {
    if path.chars().any(|c| c == '"' || c == '\\' || c.is_control()) {
        return Err(ChildError::Unsayable { path });
    }
    let Some(rest) = path.strip_prefix('/') else { return Err(ChildError::NotCanonical { path }) };
    if rest.is_empty() || rest.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
        return Err(ChildError::NotCanonical { path });
    }
    Ok(path)
}

/// The test's Seatbelt profile (macOS): everything denied but what a test may do. Reads
/// anywhere (each is an ask, recorded by the wrappers, not refused); writes only beneath its
/// temporary folder and to `/dev/null`; sockets only on loopback (a host's loopback service is
/// not kept: oscall.rs) and to the launcher's socket when one is named; no Mach service (no
/// XPC: the supported test program has only libSystem loaded); signals only within the
/// sandbox; programs may start and inherit the profile (a start is not kept). The deny-default
/// form matches `bstd::procs::hermetic::seatbelt`'s; the policy differs (a build script
/// reads only what its grant names). The exact filter forms (`local ip "localhost:*"`) are to
/// verify by a test (fork_smoke's run applies this profile).
pub fn seatbelt<'a, 's>(temp: &'s str, control: Option<&'s str>, alloc: Alloc<'a>) -> Result<AString<'a>, ChildError<'s>> {
    let temp = checked(temp)?;
    let mut out: AString<'a> = String::new_in(alloc);
    out.push_str("(version 1)\n(deny default)\n");
    out.push_str("(allow process-fork)\n(allow process-exec)\n(allow process-info* (target self))\n");
    out.push_str("(allow signal (target same-sandbox))\n(allow sysctl-read)\n");
    out.push_str("(allow file-read*)\n");
    out.push_str("(allow file-write* (subpath \"");
    out.push_str(temp);
    out.push_str("\") (literal \"/dev/null\"))\n");
    out.push_str("(allow file-ioctl (literal \"/dev/null\"))\n");
    out.push_str("(allow network-bind (local ip \"localhost:*\"))\n");
    out.push_str("(allow network-inbound (local ip \"localhost:*\"))\n");
    out.push_str("(allow network-outbound (remote ip \"localhost:*\"))\n");
    if let Some(socket) = control {
        out.push_str("(allow network-outbound (literal \"");
        out.push_str(checked(socket)?);
        out.push_str("\"))\n");
    }
    Ok(out)
}

/// The name of an outside read: BLAKE3 in its own domain over the kind's tag and the text, so a
/// file and a variable of one name are two reads. Equal on every machine for equal text.
pub fn read_name(asked: &Asked<'_>) -> ReadName {
    let (tag, text): (u8, &str) = match *asked {
        Asked::File(path) => (0, path),
        Asked::Facts(path) => (1, path),
        Asked::Listing(path) => (2, path),
        Asked::OutsideFile(path) => (3, path),
        Asked::OutsideFacts(path) => (4, path),
        Asked::OutsideListing(path) => (5, path),
        Asked::Var(name) => (6, name),
    };
    let mut hasher = blaze_blake3::Hasher::new_derive_key("blaze 2026-11-16 test outside read");
    hasher.update(&[tag]);
    hasher.update(text.as_bytes());
    ReadName(hasher.finalize())
}

/// How the test's function ended, as the child's panic path or its return says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ending<'m> {
    Returned,
    Panicked { message: &'m str, at: Option<Place> },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum At {
    Step(u8),
    Running,
    Reported,
}

/// One forked child's machine.
pub struct Child<'s, 'a> {
    spec: Spec<'s>,
    profile: AString<'a>,
    at: At,
    reads: AVec<'a, ReadName>,
    keep: Result<(), NotKept>,
    refused: Option<(Refusal, AString<'a>)>,
    alloc: Alloc<'a>,
}

impl<'s, 'a> Child<'s, 'a> {
    /// The child of `spec`, its profile made and its environment checked: called in the zygote
    /// BEFORE the fork, so a refused spec fails before any process is made, and the child
    /// allocates nothing before its test runs.
    pub fn new(spec: Spec<'s>, alloc: Alloc<'a>) -> Result<Child<'s, 'a>, ChildError<'s>> {
        let profile = seatbelt(spec.temp, spec.control, alloc)?;
        for (at, pair) in spec.env.windows(2).enumerate() {
            if pair[0].0 >= pair[1].0 {
                return Err(ChildError::EnvOrder { at: at + 1 });
            }
        }
        if spec.env.iter().any(|(name, _)| *name == "TMPDIR") {
            return Err(ChildError::EnvTemp);
        }
        Ok(Child {
            spec,
            profile,
            at: At::Step(0),
            reads: Vec::new_in(alloc),
            keep: Ok(()),
            refused: None,
            alloc,
        })
    }

    /// The next step, `None` once the call is given (the test runs; then [`Child::report`]).
    pub fn next(&mut self) -> Option<Step<'_>> {
        loop {
            let At::Step(n) = self.at else { return None };
            self.at = match n {
                7 => At::Running,
                _ => At::Step(n + 1),
            };
            let step = match n {
                0 => Step::OwnGroup,
                1 => Step::Signals,
                2 => Step::Descriptors,
                3 => Step::TempFolder { path: self.spec.temp },
                4 => Step::Environment { vars: self.spec.env, temp: self.spec.temp },
                5 => Step::Sandbox { profile: self.profile.as_str() },
                6 => match self.spec.control {
                    Some(socket) => Step::Control { socket },
                    // No socket: the step is not taken (the loop goes on to the call).
                    None => continue,
                },
                _ => Step::Call { row: self.spec.row },
            };
            return Some(step);
        }
    }

    /// Whether the test is running (its call given, its report not yet written).
    pub const fn running(&self) -> bool {
        matches!(self.at, At::Running)
    }

    /// The run's not-kept reason so far.
    pub const fn keep(&self) -> Result<(), NotKept> {
        self.keep
    }

    /// The one report's bytes. `entered` is the functions the run entered, read from the
    /// launcher's marks (any order, repeats allowed: sorted here). A refusal a wrapper saw
    /// decides the outcome whatever the function did after its call failed.
    pub fn report<'b>(
        &mut self,
        ending: Ending<'_>,
        entered: &mut [FnName],
        alloc: Alloc<'b>,
    ) -> Result<AVec<'b, u8>, ReportError<'s>> {
        if matches!(self.at, At::Reported) {
            return Err(ReportError::Child(ChildError::Reported));
        }
        self.at = At::Reported;
        entered.sort_unstable();
        let mut unique = 0;
        for at in 0..entered.len() {
            if unique == 0 || entered[unique - 1] != entered[at] {
                entered[unique] = entered[at];
                unique += 1;
            }
        }
        self.reads.sort_unstable();
        self.reads.dedup();
        let outcome = match (&self.refused, ending) {
            (Some((refusal, what)), _) => Outcome::Refused { refusal: *refusal, what: capped(what.as_str()) },
            (None, Ending::Returned) => Outcome::Returned,
            (None, Ending::Panicked { message, at }) => Outcome::Panicked { message: capped(message), at },
        };
        let report =
            Report { row: self.spec.row, outcome, keep: self.keep, entered: &entered[..unique], reads: self.reads.as_slice() };
        wire::encode(&report, alloc).map_err(ReportError::Wire)
    }
}

/// Why a report was not made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReportError<'s> {
    Child(ChildError<'s>),
    Wire(WireError),
}

impl fmt::Display for ReportError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReportError::Child(error) => error.fmt(f),
            ReportError::Wire(error) => error.fmt(f),
        }
    }
}

impl core::error::Error for ReportError<'_> {}

impl<'a> Observe for Child<'_, 'a> {
    fn saw(&mut self, op: Op<'_>) -> Allowed {
        match classify(&op) {
            Class::Ask(asked) => {
                self.reads.push(read_name(&asked));
                Allowed::Go
            }
            Class::Own => Allowed::Go,
            Class::NotKept(why) => {
                self.keep = match self.keep {
                    Ok(()) => Err(why),
                    Err(was) => Err(first_reason(was, why)),
                };
                Allowed::Go
            }
            Class::Refused { refusal, what } => {
                // The first refusal names the run; the test is ending.
                if self.refused.is_none() {
                    let mut text: AString<'a> = String::new_in(self.alloc);
                    text.push_str(capped(what));
                    self.refused = Some((refusal, text));
                }
                Allowed::Stop
            }
        }
    }
}
