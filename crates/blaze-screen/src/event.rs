//! What the screen is told: the event stream. A transcript is these, one a line as JSON, each
//! with the time it came ([`Stamped`]); replaying one gives the frames the build showed.

use alloc::{boxed::Box, string::String, vec::Vec};

use blaze_protocol::now::Now;
use blaze_report::Text;
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
    /// What the whole build waits on now, in the daemon's words (`daemon at 71.6 of 63.2 GiB,
    /// waiting on 1 task: calc lib test`), or none once it goes on: memory, when memory is why
    /// nothing moves. The region's to show; it is no line of the scrollback.
    Waits(Option<String>),
    /// A diagnostic was said (in a [`Event::Said`]): counted for the build's last line.
    Diagnosed { error: bool, unit: Option<String> },
    /// The build is over.
    Built(Built),
    /// Why the build made again what it did, a cause each: said after [`Event::Built`].
    Why(Vec<Why>),
    Test(Test),
    /// The program a `run` started has ended: what a footer says of it, on a terminal.
    Ran(Ran),
    /// What follows owns the terminal (the program a `run` becomes): nothing live is left on
    /// it after the next frame.
    Clear,
}

/// The build's end.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[serde(default)]
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
    /// What that program takes on disk, in bytes: the blocks it holds, not its length (a dev
    /// executable is long and mostly room for patches that holds nothing).
    pub size_bytes: Option<u64>,
    /// The command that says more: of a failure (`blaze explain hello`), or of why a build
    /// that passed took long (`blaze slow`). A command, as the reader types it.
    pub next: Option<String>,
    /// Warnings a fix exists for, which `blaze check --fix` makes.
    pub fixable: u32,
    /// The build's why is still to come ([`Event::Why`]): its line waits for it.
    pub why_follows: bool,
    /// Tests run after it (`test`): the region stays for them.
    pub tests_follow: bool,
}

/// How a program a `run` started ended, as whoever waited for it measured (`wait4`'s status
/// and `rusage`; on macOS its footprint).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[serde(default)]
pub struct Ran {
    /// Its exit code; none when a signal ended it.
    pub code: Option<i32>,
    /// The signal that ended it, by name: `SIGSEGV`.
    pub signal: Option<String>,
    pub core_dumped: bool,
    pub wall_ms: u64,
    pub cpu_user_ms: Option<u64>,
    pub cpu_sys_ms: Option<u64>,
    /// Its peak memory, in bytes: its footprint where the system counts one.
    pub peak_bytes: Option<u64>,
    /// `--stats`: the footer is shown whatever the run, with the processor's time.
    pub stats: bool,
    /// The program may have left the cursor in the middle of a line (its last byte was no
    /// newline, or nobody saw its bytes): the footer starts on a line of its own.
    pub mid_line: bool,
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
    #[serde(deserialize_with = "text_or_words")]
    pub cause: Text,
    /// The whole headline, as the report says it, each part with its role.
    #[serde(deserialize_with = "text_or_words")]
    pub headline: Text,
    /// The functions compiled again for it.
    pub functions: Vec<String>,
    /// The command that says more: `blaze explain why:input`.
    pub more: Option<String>,
}

/// A [`Text`], or words alone as a transcript recorded before a why carried its roles has
/// them.
fn text_or_words<'de, D: serde::Deserializer<'de>>(from: D) -> Result<Text, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Words(String),
        Text(Text),
    }
    Ok(match Either::deserialize(from)? {
        Either::Words(words) => Text::plain(words),
        Either::Text(text) => text,
    })
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
