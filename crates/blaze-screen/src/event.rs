//! What the screen is told: the event stream. A transcript is these, one a line as JSON, each
//! with the time it came ([`Stamped`]); replaying one gives the frames the build showed.

use alloc::{boxed::Box, string::String, vec::Vec};

use blaze_protocol::now::Now;
use serde::{Deserialize, Serialize};

/// The command whose build this is.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    Build,
    Check,
    Run,
    Test,
    /// `doc`, `bench`, any other command that builds: shown as a build.
    Other,
}

impl Command {
    pub const fn word(self) -> &'static str {
        match self {
            Command::Build | Command::Other => "build",
            Command::Check => "check",
            Command::Run => "run",
            Command::Test => "test",
        }
    }
}

/// One event, in the order it happened.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Event {
    /// The terminal is `cols` by `rows` now.
    Resized { cols: u16, rows: u16 },
    /// The build as it runs now: the daemon's model of it, said about twice a second.
    Now(Box<Now>),
    /// A unit started: made again, or (`fresh`) replayed from the store.
    Unit { name: String, fresh: bool },
    /// Whole lines that stay, as they were drawn for this client: a diagnostic, a report, a
    /// test executable's own words. Ends with a newline.
    Said(String),
    /// A diagnostic was said (in a [`Event::Said`]): counted for the build's last line.
    Diagnosed { error: bool, unit: Option<String> },
    /// The build is over.
    Built(Built),
    /// Why the build made again what it did, a cause each: said after [`Event::Built`].
    Why(Vec<Why>),
    Test(Test),
    /// What follows owns the terminal (the program a `run` becomes): nothing live is left on
    /// it after the next frame.
    Clear,
}

/// The build's end.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Built {
    pub ok: bool,
    /// Units made again, and units replayed.
    pub compiled: u64,
    pub cached: u64,
    /// `tier 0 · 38% optimized`: where the linked code stands; none when nothing was linked.
    pub tier: Option<String>,
    /// Each image linked.
    pub links: Vec<Link>,
    /// Units the build left unbuilt behind its failures.
    pub unbuilt: u32,
    /// The one program the build placed, where it placed exactly one: its path.
    pub program: Option<String>,
    /// The command that says more of a failure: `blaze explain hello`.
    pub next: Option<String>,
    /// The build's why is still to come ([`Event::Why`]): its line waits for it.
    pub why_follows: bool,
    /// Tests run after it (`test`): the region stays for them.
    pub tests_follow: bool,
}

/// One image the build linked.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Link {
    pub path: String,
    /// `first link`, `spliced`, `unchanged`, `full link: <why>`.
    pub how: String,
    /// Laid out whole although an image was kept: the slow path, always said.
    pub whole: bool,
}

/// One cause of what a build made again.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Why {
    /// Work that should not have been done (a comment-only change recompiled, a step with no
    /// recorded reason): it earns a line of its own.
    pub wasted: bool,
    /// `src/main.rs changed`: the cause, without the count of steps.
    pub cause: String,
    /// The whole headline, as the report says it.
    pub headline: String,
    /// The functions compiled again for it.
    pub functions: Vec<String>,
    /// `blaze explain why:input`.
    pub more: Option<String>,
}

/// What became of one test.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Passed,
    Failed,
    Ignored,
}

/// How a test executable ended.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum BinaryEnd {
    Passed,
    /// It exited otherwise: `exit status: 101`, `signal: 11 (SIGSEGV)`.
    Failed { status: String },
    /// The command stopped it (`--fail-fast`).
    Stopped,
    NeverRan { why: String },
}

/// The test run's events.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Test {
    /// A test executable is known and waits for its turn; `rerun` is the command that reruns it.
    Queued { binary: u32, label: String, rerun: String },
    Started { binary: u32 },
    /// libtest's count of the tests it runs.
    Total { binary: u32, tests: u32 },
    TestStarted { binary: u32, name: String },
    /// A test ended: libtest's time for it, and what it printed (shown when it failed).
    TestEnded { binary: u32, name: String, verdict: Verdict, took_ms: Option<u64>, output: String },
    /// The executable ended; `output` is what it said outside any test.
    Ended { binary: u32, end: BinaryEnd, output: String },
    /// A test executable never built: behind which unit, and why that one failed.
    Unbuilt { unit: String, behind: String, why: String },
    /// Every executable ended: the summary is due.
    Over,
}

/// One line of a transcript.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Stamped {
    /// Milliseconds since the command began, on the driver's clock.
    pub at_ms: u64,
    pub event: Event,
}
