//! The run report of `blaze test` (docs/v2/plan/design/test.md, "End state", piece 5): a pure
//! formatter. It writes text into a writer it is handed; it reads no clock, no terminal and no
//! environment: the width, whether to colour and how much output to show are a [`Style`], chosen
//! once at the command's edge (as blaze-report's `frame::plain::view` is for findings).
//!
//! Two calls. [`failure`]: one failure, the moment it ends (its name, how it ended, its captured
//! output capped at [`Style::shown`] bytes with a line saying how many were cut). A pass prints
//! nothing. [`summary`]: once, after the last test: the failures, the passes not kept and the
//! `Flaky` findings by name, then one line of counts. The summary is ordered by path (then test
//! id), never by the order tests ended in, so two runs with equal results print equal bytes
//! (rule 25); no map is iterated.
//!
//! The person and agent view (`Format::Text`), exactly, at width 80 without colour:
//!
//! ```text
//! FAILED m::b after 0.012 s: panicked at src/lib.rs:3:5: boom
//! ---- m::b output: 1048576 bytes, the first 1032192 cut ----
//! xxxx(the last 16384 bytes)
//! --------------------------------------------------------------------------------
//! ```
//!
//! and the summary:
//!
//! ```text
//! FAILED m::b: panicked
//! not kept n::c: read the real clock
//! FLAKY m::a: passed on checkout 0300000000000000, ended by signal 11 on checkout 0300000000000000
//! FLAKY m::b: passed on checkout 0100000000000000, panicked on checkout 0200000000000000
//! test result: FAILED. ran 3: 2 passed, 1 failed; 2 reused from kept results; 1 not kept (1 read the real clock); 1 ignored; 1 benches not run; 2 filtered out; 2 flaky
//! ```
//!
//! The JSON view (`--format json`, `Format::Json`): one object a line, keys always present and
//! in this order (`null` where a key does not apply).
//! - A failure as it ends: `event` (`"failed"`), `test` (64 hex digits), `path`, `micros`,
//!   `ended` (`panicked`, `did_not_panic`, `panic_text_missing`, `signal`, `timed_out`,
//!   `refused`, `no_report`), `message`, `at`, `signal`, `exit_code` (a run that exited with no
//!   whole report), `after_ns`, `refused` (`write`, `connect`, `spawn`,
//!   `call`), `what`, `output_bytes`, `output_cut`, `output` (the shown tail).
//! - The summary: for every test in path order `event` (`"test"`), `test`, `path`, `outcome`
//!   (`kept`, `passed`, `failed`, `ignored`, `bench`), `ran_because` (`run_all`, `no_record`,
//!   `last_failed`, `last_not_kept`, `moved`, `unsettled`), `basis` (`unchanged`, `answers`),
//!   `not_kept` (`clock`, `network`, `wrote_outside`, `foreign`, `unsandboxed`, `descriptor`,
//!   `threads`), `failure` (as `ended`, with `timed_out` for a kill); then each finding `event`
//!   (`"flaky"`), `test`, `path`, `address`, `passed_on`, `failed_on`, `failure`, `rule`; then
//!   `{"event":"summary","result":"failed","ran":3,"passed":2,"failed":1,"reused":2,
//!   "not_kept":1,"not_kept_by":{"clock":1,"network":0,"wrote_outside":0,"foreign":0,
//!   "unsandboxed":0,"descriptor":0,"threads":0},"ignored":1,"benches":1,"filtered_out":2,
//!   "flaky":2}` (one line).

#![blaze::sans_io]

use core::fmt::{self, Write};

use blaze_own::{AVec, Alloc, Vec};

use crate::plan::{Basis, Failure, Missed, NotKept, Outcome};
use crate::result::{Ended, Failed, Finding, NotKeptWords, Refusal, Why};
use crate::{Hex, TestId};

/// The step trace of a timed-out or failed end-to-end test (test.md C, piece 18's formatter).
pub mod steps;
pub use steps::{StepTrace, step_trace, timed_out};

/// How much of a failure's captured output is shown by default: its last 16 KiB.
pub const DEFAULT_SHOWN: usize = 16 * 1024;

/// The view (`--format`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// Lines for a person or an agent.
    Text,
    /// One JSON object a line.
    Json,
}

/// How the report is drawn: chosen at the command's edge, never queried here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Style {
    pub format: Format,
    /// The terminal's columns: the rule closing a failure's output is this wide (4 to 120).
    pub width: u16,
    /// Whether `FAILED`, `FLAKY` and `ok` are coloured (ANSI SGR).
    pub colour: bool,
    /// The most of a failure's output shown: its last bytes, cut at a character's start.
    pub shown: usize,
}

impl Style {
    pub const fn text(width: u16, colour: bool) -> Style {
        Style { format: Format::Text, width, colour, shown: DEFAULT_SHOWN }
    }

    pub const fn json() -> Style {
        Style { format: Format::Json, width: 0, colour: false, shown: DEFAULT_SHOWN }
    }
}

/// One test's outcome in this request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry<'r> {
    pub test: TestId,
    /// As printed: `module::name`.
    pub path: &'r str,
    pub outcome: Outcome,
}

/// One failure, as it ends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ending<'r> {
    pub test: TestId,
    pub path: &'r str,
    pub failed: Failed<'r>,
    /// The panic's place as printed (`src/lib.rs:3:5`), made by the driver from the ending's
    /// item-relative [`crate::result::Place`] (rule 71). Read only for a panic.
    pub at: Option<&'r str>,
}

/// Why a report was not written. Each names the test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReportError {
    /// The writer refused.
    Write,
    /// Two entries for one test.
    TwoEntries { test: TestId },
    /// A `Flaky` finding names a test with no entry.
    NoEntry { test: TestId },
    /// More entries or findings than a u32 counts.
    TooMany { count: usize },
}

impl From<fmt::Error> for ReportError {
    fn from(_: fmt::Error) -> ReportError {
        ReportError::Write
    }
}

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ReportError::Write => f.write_str("the test report's writer refused a write"),
            ReportError::TwoEntries { test } => write!(f, "test {} has two outcomes in one report", Hex(&test.0)),
            ReportError::NoEntry { test } => {
                write!(f, "a flaky finding names test {}, which has no outcome in this report", Hex(&test.0))
            }
            ReportError::TooMany { count } => write!(f, "{count} rows in one test report: more than a u32 counts"),
        }
    }
}

impl core::error::Error for ReportError {}

const RED: &str = "1;31";
const GREEN: &str = "32";
const YELLOW: &str = "33";

const NOT_KEPT: [NotKept; 8] = [
    NotKept::Clock,
    NotKept::Network,
    NotKept::WroteOutside,
    NotKept::Foreign,
    NotKept::Unsandboxed,
    NotKept::Descriptor,
    NotKept::Threads,
    NotKept::Untraced,
];

const fn not_kept_key(why: NotKept) -> &'static str {
    match why {
        NotKept::Clock => "clock",
        NotKept::Network => "network",
        NotKept::WroteOutside => "wrote_outside",
        NotKept::Foreign => "foreign",
        NotKept::Unsandboxed => "unsandboxed",
        NotKept::Descriptor => "descriptor",
        NotKept::Threads => "threads",
        NotKept::Untraced => "untraced",
    }
}

const fn not_kept_index(why: NotKept) -> usize {
    match why {
        NotKept::Clock => 0,
        NotKept::Network => 1,
        NotKept::WroteOutside => 2,
        NotKept::Foreign => 3,
        NotKept::Unsandboxed => 4,
        NotKept::Descriptor => 5,
        NotKept::Threads => 6,
        NotKept::Untraced => 7,
    }
}

const fn failure_key(why: Failure) -> &'static str {
    match why {
        Failure::Panicked => "panicked",
        Failure::DidNotPanic => "did_not_panic",
        Failure::PanicTextMissing => "panic_text_missing",
        Failure::Signal { .. } => "signal",
        Failure::Refused => "refused",
        Failure::TimedOut { .. } => "timed_out",
        Failure::NoReport { .. } => "no_report",
    }
}

/// A total order on failures, for sorting findings.
const fn failure_rank(why: Failure) -> (u8, i64, u64) {
    match why {
        Failure::Panicked => (0, 0, 0),
        Failure::DidNotPanic => (1, 0, 0),
        Failure::PanicTextMissing => (2, 0, 0),
        Failure::Signal { number } => (3, number as i64, 0),
        Failure::Refused => (4, 0, 0),
        Failure::TimedOut { after } => (5, 0, after.0),
        Failure::NoReport { code } => (6, code as i64, 0),
    }
}

const fn missed_key(missed: Missed) -> &'static str {
    match missed {
        Missed::RunAll => "run_all",
        Missed::NoRecord => "no_record",
        Missed::LastFailed => "last_failed",
        Missed::LastNotKept(_) => "last_not_kept",
        Missed::Moved { .. } => "moved",
        Missed::Unsettled => "unsettled",
    }
}

const fn refusal_key(refusal: Refusal) -> &'static str {
    match refusal {
        Refusal::Write => "write",
        Refusal::Connect => "connect",
        Refusal::Spawn => "spawn",
        Refusal::Call => "call",
    }
}

const fn refusal_words(refusal: Refusal) -> &'static str {
    match refusal {
        Refusal::Write => "a write to",
        Refusal::Connect => "a connection to",
        Refusal::Spawn => "starting",
        Refusal::Call => "the system call",
    }
}

/// Writes one failure, the moment it ends. See the module's documentation for its lines.
pub fn failure(out: &mut dyn Write, style: Style, ending: &Ending<'_>) -> Result<(), ReportError> {
    match style.format {
        Format::Text => failure_text(out, style, ending)?,
        Format::Json => failure_json(out, style, ending)?,
    }
    Ok(())
}

/// The shown part of a failure's output, and how many of the output's first bytes are not shown.
fn shown<'o>(failed: &Failed<'o>, cap: usize) -> (&'o [u8], u64) {
    let tail = failed.output.tail;
    let mut from = tail.len().saturating_sub(cap);
    if from > 0 || failed.output.cut() > 0 {
        // A cut inside a character: its continuation bytes are cut with it.
        let mut skipped = 0;
        while from < tail.len() && skipped < 3 && tail[from] & 0xC0 == 0x80 {
            from += 1;
            skipped += 1;
        }
    }
    let shown = &tail[from..];
    (shown, failed.output.total.saturating_sub(shown.len() as u64))
}

fn failure_text(out: &mut dyn Write, style: Style, ending: &Ending<'_>) -> fmt::Result {
    paint(out, style, RED, "FAILED")?;
    write!(out, " {} after ", ending.path)?;
    seconds(out, ending.failed.micros, 1_000_000)?;
    out.write_str(": ")?;
    ended_text(out, &ending.failed.ended, ending.at)?;
    out.write_char('\n')?;
    let total = ending.failed.output.total;
    if total == 0 {
        return Ok(());
    }
    let (bytes, cut) = shown(&ending.failed, style.shown);
    write!(out, "---- {} output: {total} bytes", ending.path)?;
    if cut > 0 {
        write!(out, ", the first {cut} cut")?;
    }
    out.write_str(" ----\n")?;
    lossy(out, bytes)?;
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        out.write_char('\n')?;
    }
    let width = style.width.clamp(4, 120);
    for _ in 0..width {
        out.write_char('-')?;
    }
    out.write_char('\n')
}

fn ended_text(out: &mut dyn Write, ended: &Ended<'_>, at: Option<&str>) -> fmt::Result {
    match *ended {
        Ended::Panicked { message, .. } => {
            out.write_str("panicked")?;
            place(out, at)?;
            write!(out, ": {message}")
        }
        Ended::DidNotPanic => out.write_str("did not panic, and `should_panic` says it must"),
        Ended::PanicTextMissing { message, .. } => {
            out.write_str("panicked")?;
            place(out, at)?;
            write!(out, " without the expected text: {message}")
        }
        Ended::Signal { number } => write!(out, "ended by signal {number}"),
        Ended::TimedOut { after } => {
            out.write_str("was killed past its deadline, ")?;
            seconds(out, after.0, 1_000_000_000)?;
            out.write_str(" after its start")
        }
        Ended::Refused { refusal, what } => write!(out, "was refused by the sandbox: {} {what}", refusal_words(refusal)),
        Ended::NoReport { code, torn: 0 } => write!(out, "exited with code {code} and wrote no report"),
        Ended::NoReport { code, torn } => {
            write!(out, "exited with code {code} after {torn} bytes of a report that did not open")
        }
    }
}

fn place(out: &mut dyn Write, at: Option<&str>) -> fmt::Result {
    match at {
        Some(at) => write!(out, " at {at}"),
        None => Ok(()),
    }
}

fn failure_json(out: &mut dyn Write, style: Style, ending: &Ending<'_>) -> fmt::Result {
    let failed = &ending.failed;
    out.write_str("{\"event\":\"failed\",\"test\":")?;
    hex(out, &ending.test.0)?;
    out.write_str(",\"path\":")?;
    json_str(out, ending.path)?;
    write!(out, ",\"micros\":{},\"ended\":", failed.micros)?;
    json_str(out, failure_key(failed.ended.why()))?;
    let (message, at) = match failed.ended {
        Ended::Panicked { message, .. } | Ended::PanicTextMissing { message, .. } => (Some(message), ending.at),
        _ => (None, None),
    };
    out.write_str(",\"message\":")?;
    opt_str(out, message)?;
    out.write_str(",\"at\":")?;
    opt_str(out, at)?;
    out.write_str(",\"signal\":")?;
    match failed.ended {
        Ended::Signal { number } => write!(out, "{number}")?,
        _ => out.write_str("null")?,
    }
    out.write_str(",\"exit_code\":")?;
    match failed.ended {
        Ended::NoReport { code, .. } => write!(out, "{code}")?,
        _ => out.write_str("null")?,
    }
    out.write_str(",\"after_ns\":")?;
    match failed.ended {
        Ended::TimedOut { after } => write!(out, "{}", after.0)?,
        _ => out.write_str("null")?,
    }
    let (refused, what) = match failed.ended {
        Ended::Refused { refusal, what } => (Some(refusal_key(refusal)), Some(what)),
        _ => (None, None),
    };
    out.write_str(",\"refused\":")?;
    opt_str(out, refused)?;
    out.write_str(",\"what\":")?;
    opt_str(out, what)?;
    let (bytes, cut) = shown(failed, style.shown);
    write!(out, ",\"output_bytes\":{},\"output_cut\":{cut},\"output\":", failed.output.total)?;
    json_bytes(out, bytes)?;
    out.write_str("}\n")
}

/// The counts the summary's last line says.
#[derive(Clone, Copy, Default)]
struct Counts {
    passed: u32,
    failed: u32,
    reused: u32,
    not_kept: [u32; NOT_KEPT.len()],
    ignored: u32,
    benches: u32,
    /// Tests the plan wanted run that could not be ([`NotRun`]); set by [`summary_with`].
    not_run: u32,
}

impl Counts {
    fn of(entries: &[Entry<'_>]) -> Counts {
        let mut counts = Counts::default();
        for entry in entries {
            match entry.outcome {
                Outcome::Cached(_) => counts.reused += 1,
                Outcome::Passed { kept, .. } => {
                    counts.passed += 1;
                    if let Err(why) = kept {
                        counts.not_kept[not_kept_index(why)] += 1;
                    }
                }
                Outcome::Failed { .. } => counts.failed += 1,
                Outcome::Ignored => counts.ignored += 1,
                Outcome::Bench => counts.benches += 1,
            }
        }
        counts
    }

    fn not_kept_total(&self) -> u32 {
        self.not_kept.iter().sum()
    }
}

/// Writes the summary: the failures, the passes not kept and the `Flaky` findings by name, in
/// path order, then the counts. `entries` and `flaky` may come in any order (the order tests
/// ended in): the bytes depend only on the set. `filtered_out` is [`crate::plan::Plan::filtered_out`].
/// The orders are built in `alloc`.
pub fn summary(
    out: &mut dyn Write,
    style: Style,
    entries: &[Entry<'_>],
    filtered_out: u32,
    flaky: &[Finding],
    alloc: Alloc<'_>,
) -> Result<(), ReportError> {
    summary_with(out, style, entries, filtered_out, flaky, &[], alloc)
}

/// A test the plan wanted run that could not be run at all (no code for its body yet, no runner
/// on this host): never a pass, never kept, and the summary's result is FAILED while any is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotRun<'r> {
    pub test: TestId,
    /// As printed: `module::name`.
    pub path: &'r str,
    /// Why, in words: `the code builder cannot build test bodies yet (test.md piece 8)`.
    pub why: &'r str,
}

/// [`summary`], with the tests that could not be run. Each is a line before the failures,
/// in path order (text: `NOT RUN m::a: cannot be run yet: <why>`; JSON: `{"event":"not_run",
/// "test":..,"path":..,"why":..}`); the text's counts line ends `; N could not be run yet`
/// when N is not zero, and the result is FAILED (JSON `"result":"failed"`) while N is not zero.
/// A test in both `entries` and `not_run` is [`ReportError::TwoEntries`].
pub fn summary_with(
    out: &mut dyn Write,
    style: Style,
    entries: &[Entry<'_>],
    filtered_out: u32,
    flaky: &[Finding],
    not_run: &[NotRun<'_>],
    alloc: Alloc<'_>,
) -> Result<(), ReportError> {
    let not_run_count = u32::try_from(not_run.len()).map_err(|_| ReportError::TooMany { count: not_run.len() })?;
    let mut not_run_order: AVec<'_, NotRun<'_>> = Vec::from_slice_in(not_run, alloc);
    not_run_order.sort_unstable_by(|a, b| (a.path, a.test).cmp(&(b.path, b.test)));
    let count = u32::try_from(entries.len()).map_err(|_| ReportError::TooMany { count: entries.len() })?;
    let flaky_count = u32::try_from(flaky.len()).map_err(|_| ReportError::TooMany { count: flaky.len() })?;

    let mut by_id: AVec<'_, u32> = Vec::with_capacity_in(entries.len(), alloc);
    for at in 0..count {
        by_id.push(at);
    }
    by_id.sort_unstable_by(|a, b| entries[*a as usize].test.cmp(&entries[*b as usize].test));
    for pair in by_id.windows(2) {
        let test = entries[pair[0] as usize].test;
        if test == entries[pair[1] as usize].test {
            return Err(ReportError::TwoEntries { test });
        }
    }
    let mut by_path: AVec<'_, u32> = Vec::from_slice_in(by_id.as_slice(), alloc);
    by_path.sort_unstable_by(|a, b| {
        let (a, b) = (&entries[*a as usize], &entries[*b as usize]);
        (a.path, a.test).cmp(&(b.path, b.test))
    });

    let mut findings: AVec<'_, u32> = Vec::with_capacity_in(flaky.len(), alloc);
    for at in 0..flaky_count {
        findings.push(at);
    }
    findings.sort_unstable_by(|a, b| {
        let (a, b) = (&flaky[*a as usize].flaky, &flaky[*b as usize].flaky);
        (a.test, a.address, a.passed, a.failed, failure_rank(a.why)).cmp(&(
            b.test,
            b.address,
            b.passed,
            b.failed,
            failure_rank(b.why),
        ))
    });
    // Each finding's entry, by one binary search over the id order.
    let mut paths: AVec<'_, &str> = Vec::with_capacity_in(flaky.len(), alloc);
    for at in findings.iter() {
        let test = flaky[*at as usize].flaky.test;
        match by_id.binary_search_by(|row| entries[*row as usize].test.cmp(&test)) {
            Ok(row) => paths.push(entries[by_id[row] as usize].path),
            Err(_) => return Err(ReportError::NoEntry { test }),
        }
    }
    // A test not run has no outcome: in both lists, or twice in one, is two outcomes.
    for pair in not_run_order.windows(2) {
        if pair[0].test == pair[1].test {
            return Err(ReportError::TwoEntries { test: pair[0].test });
        }
    }
    for one in not_run_order.iter() {
        if by_id.binary_search_by(|row| entries[*row as usize].test.cmp(&one.test)).is_ok() {
            return Err(ReportError::TwoEntries { test: one.test });
        }
    }

    let mut counts = Counts::of(entries);
    counts.not_run = not_run_count;
    for one in not_run_order.iter() {
        match style.format {
            Format::Text => {
                paint(out, style, RED, "NOT RUN")?;
                writeln!(out, " {}: cannot be run yet: {}", one.path, one.why)?;
            }
            Format::Json => {
                out.write_str("{\"event\":\"not_run\",\"test\":")?;
                hex(out, &one.test.0)?;
                out.write_str(",\"path\":")?;
                json_str(out, one.path)?;
                out.write_str(",\"why\":")?;
                json_str(out, one.why)?;
                out.write_str("}\n")?;
            }
        }
    }
    match style.format {
        Format::Text => summary_text(out, style, entries, &by_path, flaky, &findings, &paths, &counts, filtered_out)?,
        Format::Json => summary_json(out, entries, &by_path, flaky, &findings, &paths, &counts, filtered_out)?,
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, reason = "the summary's parts, each sorted once by `summary`")]
fn summary_text(
    out: &mut dyn Write,
    style: Style,
    entries: &[Entry<'_>],
    by_path: &[u32],
    flaky: &[Finding],
    findings: &[u32],
    paths: &[&str],
    counts: &Counts,
    filtered_out: u32,
) -> fmt::Result {
    for at in by_path {
        let entry = &entries[*at as usize];
        match entry.outcome {
            Outcome::Failed { why, .. } => {
                paint(out, style, RED, "FAILED")?;
                writeln!(out, " {}: {}", entry.path, Why(why))?;
            }
            Outcome::Passed { kept: Err(why), .. } => writeln!(out, "not kept {}: {}", entry.path, NotKeptWords(why))?,
            Outcome::Cached(_) | Outcome::Passed { kept: Ok(()), .. } | Outcome::Ignored | Outcome::Bench => {}
        }
    }
    for (at, path) in findings.iter().zip(paths) {
        let flaky = &flaky[*at as usize].flaky;
        paint(out, style, YELLOW, "FLAKY")?;
        writeln!(
            out,
            " {path}: passed on checkout {}, {} on checkout {}",
            Hex(&flaky.passed.0),
            Why(flaky.why),
            Hex(&flaky.failed.0)
        )?;
    }
    out.write_str("test result: ")?;
    match counts.failed + counts.not_run {
        0 => paint(out, style, GREEN, "ok")?,
        _ => paint(out, style, RED, "FAILED")?,
    }
    write!(
        out,
        ". ran {}: {} passed, {} failed; {} reused from kept results; {} not kept",
        counts.passed + counts.failed,
        counts.passed,
        counts.failed,
        counts.reused,
        counts.not_kept_total()
    )?;
    if counts.not_kept_total() > 0 {
        out.write_str(" (")?;
        let mut first = true;
        for (why, n) in NOT_KEPT.iter().zip(counts.not_kept) {
            if n == 0 {
                continue;
            }
            if !first {
                out.write_str(", ")?;
            }
            first = false;
            write!(out, "{n} {}", NotKeptWords(*why))?;
        }
        out.write_char(')')?;
    }
    write!(
        out,
        "; {} ignored; {} benches not run; {filtered_out} filtered out; {} flaky",
        counts.ignored,
        counts.benches,
        findings.len()
    )?;
    if counts.not_run > 0 {
        write!(out, "; {} could not be run yet", counts.not_run)?;
    }
    out.write_char('\n')
}

#[allow(clippy::too_many_arguments, reason = "the summary's parts, each sorted once by `summary`")]
fn summary_json(
    out: &mut dyn Write,
    entries: &[Entry<'_>],
    by_path: &[u32],
    flaky: &[Finding],
    findings: &[u32],
    paths: &[&str],
    counts: &Counts,
    filtered_out: u32,
) -> fmt::Result {
    for at in by_path {
        let entry = &entries[*at as usize];
        let (outcome, ran, basis, not_kept, failure) = match entry.outcome {
            Outcome::Cached(basis) => (
                "kept",
                None,
                Some(match basis {
                    Basis::Unchanged => "unchanged",
                    Basis::Answers => "answers",
                }),
                None,
                None,
            ),
            Outcome::Passed { ran, kept } => ("passed", Some(missed_key(ran)), None, kept.err().map(not_kept_key), None),
            Outcome::Failed { ran, why } => ("failed", Some(missed_key(ran)), None, None, Some(failure_key(why))),
            Outcome::Ignored => ("ignored", None, None, None, None),
            Outcome::Bench => ("bench", None, None, None, None),
        };
        out.write_str("{\"event\":\"test\",\"test\":")?;
        hex(out, &entry.test.0)?;
        out.write_str(",\"path\":")?;
        json_str(out, entry.path)?;
        out.write_str(",\"outcome\":")?;
        json_str(out, outcome)?;
        out.write_str(",\"ran_because\":")?;
        opt_str(out, ran)?;
        out.write_str(",\"basis\":")?;
        opt_str(out, basis)?;
        out.write_str(",\"not_kept\":")?;
        opt_str(out, not_kept)?;
        out.write_str(",\"failure\":")?;
        opt_str(out, failure)?;
        out.write_str("}\n")?;
    }
    for (at, path) in findings.iter().zip(paths) {
        let finding = &flaky[*at as usize];
        let flaky = &finding.flaky;
        out.write_str("{\"event\":\"flaky\",\"test\":")?;
        hex(out, &flaky.test.0)?;
        out.write_str(",\"path\":")?;
        json_str(out, path)?;
        out.write_str(",\"address\":")?;
        hex(out, &flaky.address.0)?;
        out.write_str(",\"passed_on\":")?;
        hex(out, &flaky.passed.0)?;
        out.write_str(",\"failed_on\":")?;
        hex(out, &flaky.failed.0)?;
        out.write_str(",\"failure\":")?;
        json_str(out, failure_key(flaky.why))?;
        writeln!(out, ",\"rule\":{}}}", finding.rule)?;
    }
    write!(
        out,
        "{{\"event\":\"summary\",\"result\":\"{}\",\"ran\":{},\"passed\":{},\"failed\":{},\"reused\":{},\"not_kept\":{},\"not_kept_by\":{{",
        match counts.failed + counts.not_run {
            0 => "ok",
            _ => "failed",
        },
        counts.passed + counts.failed,
        counts.passed,
        counts.failed,
        counts.reused,
        counts.not_kept_total(),
    )?;
    for (i, (why, n)) in NOT_KEPT.iter().zip(counts.not_kept).enumerate() {
        if i > 0 {
            out.write_char(',')?;
        }
        write!(out, "\"{}\":{n}", not_kept_key(*why))?;
    }
    writeln!(
        out,
        "}},\"ignored\":{},\"benches\":{},\"filtered_out\":{filtered_out},\"flaky\":{}}}",
        counts.ignored,
        counts.benches,
        findings.len()
    )
}

fn paint(out: &mut dyn Write, style: Style, code: &str, word: &str) -> fmt::Result {
    match style.colour {
        true => write!(out, "\x1b[{code}m{word}\x1b[0m"),
        false => out.write_str(word),
    }
}

/// `amount` of `per_second` units as seconds with three decimals: `0.012 s`.
fn seconds(out: &mut dyn Write, amount: u64, per_second: u64) -> fmt::Result {
    let millis = amount / (per_second / 1_000);
    write!(out, "{}.{:03} s", millis / 1_000, millis % 1_000)
}

/// Bytes as text, each byte that is not UTF-8 as one U+FFFD per bad sequence.
fn lossy(out: &mut dyn Write, bytes: &[u8]) -> fmt::Result {
    for chunk in bytes.utf8_chunks() {
        out.write_str(chunk.valid())?;
        if !chunk.invalid().is_empty() {
            out.write_char('\u{FFFD}')?;
        }
    }
    Ok(())
}

fn hex(out: &mut dyn Write, bytes: &[u8; 32]) -> fmt::Result {
    out.write_char('"')?;
    for byte in bytes {
        write!(out, "{byte:02x}")?;
    }
    out.write_char('"')
}

fn opt_str(out: &mut dyn Write, text: Option<&str>) -> fmt::Result {
    match text {
        Some(text) => json_str(out, text),
        None => out.write_str("null"),
    }
}

fn json_str(out: &mut dyn Write, text: &str) -> fmt::Result {
    out.write_char('"')?;
    escape(out, text)?;
    out.write_char('"')
}

fn json_bytes(out: &mut dyn Write, bytes: &[u8]) -> fmt::Result {
    out.write_char('"')?;
    for chunk in bytes.utf8_chunks() {
        escape(out, chunk.valid())?;
        if !chunk.invalid().is_empty() {
            out.write_str("\\ufffd")?;
        }
    }
    out.write_char('"')
}

/// JSON's string escapes (RFC 8259, section 7): the quote, the backslash and every control
/// character.
fn escape(out: &mut dyn Write, text: &str) -> fmt::Result {
    for ch in text.chars() {
        match ch {
            '"' => out.write_str("\\\"")?,
            '\\' => out.write_str("\\\\")?,
            '\n' => out.write_str("\\n")?,
            '\r' => out.write_str("\\r")?,
            '\t' => out.write_str("\\t")?,
            ch if (ch as u32) < 0x20 || ch == '\u{7f}' => write!(out, "\\u{:04x}", ch as u32)?,
            ch => out.write_char(ch)?,
        }
    }
    Ok(())
}
