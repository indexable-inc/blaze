//! Kept failures (docs/v2/plan/design/test.md, "End state: the search itself", piece S3;
//! decision D95): a failure found once is replayed forever.
//!
//! A failing check's report ends with ONE line to keep:
//!
//! ```text
//! blaze-sim keep: <test path> v<primitives version> <the shrunk tape as text>  # <date> <why>
//! ```
//!
//! A crate keeps those lines in `sim-kept.tapes` beside its `Cargo.toml`: text, one tape a line,
//! `#` comment lines and blank lines allowed. The file is an input of the build, never read at
//! run time (rule 75): [`sim_kept!`](crate::sim_kept) expands to its `include_str!` at the call
//! site, and [`check_kept`] replays this test's lines FIRST, in file order, before any search. A
//! line that does not parse fails the test naming its line. The `v<n>` of a line is its tape's
//! format: the version of the draws that read it ([`PRIMITIVES`]). A line of another version is
//! not replayed (other draws read its tape as another case, or as no case at all) and is no
//! failure: it is a [`KeptRefusal`] printed by name, and the search finds the failure again under
//! this build's draws, to be kept again. A line whose test is gone is found by [`all_known`],
//! which a crate calls from one test with the paths of all its sim tests. Lines are only added.
//!
//! The test path is the one [`report`](crate::check::report) prints: the name of the test on the
//! thread (libtest names the thread after the test), or `file:line` of the check when the thread
//! has no test's name.
//!
//! Nothing here owns memory: a line borrows the compiled-in text (its tape too, as text, parsed
//! when it is replayed), the lists are iterators over the file, and the line to keep is written
//! into the caller's `fmt::Write`. The one owned list is the tape the replay hands the search,
//! which `Given` takes as it is.

use core::fmt;
use core::panic::Location;

use crate::check::{Outcome, check, report, search, with_test_name};
use crate::search::{Config, Failed, Given, Verdict};
use crate::tape::Tape;
use crate::text::{self, TextError};

/// The version of the draws a tape is read by. A tape is a list of choices; what a choice means
/// is the primitives' business, so a tape kept under one version can draw other values under
/// another. 1 is the draws of today; 2 is the edge-first draws (pieces S1 and S2, behind the
/// cargo feature `edge-first`). It is the `v<n>` a kept line is written with, its tape's format:
/// a line of another version is not replayed but refused by name ([`KeptRefusal`]).
pub const PRIMITIVES: u32 = if cfg!(feature = "edge-first") { 2 } else { 1 };

/// The name of the file a crate keeps its lines in, beside its `Cargo.toml`.
pub const FILE_NAME: &str = "sim-kept.tapes";

/// What the report writes in place of the date: the report reads no clock (rule 75), so the
/// person who keeps the line writes the day.
pub const DATE_TO_FILL: &str = "????-??-??";

/// The words a report starts the line to keep with.
pub const KEEP: &str = "blaze-sim keep: ";

/// The most characters of the failure's first line a report writes as the line's why.
const WHY_MOST: usize = 160;

/// A crate's kept lines, compiled in: what [`sim_kept!`](crate::sim_kept) gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct File {
    /// The text of the crate's `sim-kept.tapes`.
    pub text: &'static str,
    /// The crate's package name (`CARGO_PKG_NAME` at compile time), which every message names
    /// the file by. Not its directory: an absolute path in a test binary would differ from
    /// machine to machine (rule 25).
    pub krate: &'static str,
}

impl File {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn new(text: &'static str, krate: &'static str) -> File {
        File { text, krate }
    }

    /// The file's lines that hold a tape, in file order, each parsed or refused naming its line.
    pub fn lines(self) -> Lines {
        Lines { file: self, rest: self.text.lines().enumerate() }
    }
}

/// The calling crate's `sim-kept.tapes` (beside its `Cargo.toml`), compiled in as a
/// [`kept::File`](crate::kept::File). The file has to exist: an empty one, or one of comment
/// lines, keeps nothing.
///
/// `const KEPT: blaze_sim::kept::File = blaze_sim::sim_kept!();`
#[macro_export]
macro_rules! sim_kept {
    () => {
        $crate::kept::File::new(
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/sim-kept.tapes")),
            env!("CARGO_PKG_NAME"),
        )
    };
}

/// One kept line, parsed; it borrows the file's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line {
    /// Its line number in the file, from 1.
    pub number: usize,
    /// The test it is kept for.
    pub test: &'static str,
    /// The primitives version it was kept under.
    pub version: u32,
    /// The tape as text, as `text::print` writes it; it parses (the reader checked).
    pub tape: &'static str,
}

/// The tape lines of a [`File`], each parsed: blank lines and lines starting with `#` are
/// passed over, any other line is a [`Line`] or refused.
pub struct Lines {
    file: File,
    rest: core::iter::Enumerate<core::str::Lines<'static>>,
}

impl Iterator for Lines {
    type Item = Result<Line, KeptError>;

    fn next(&mut self) -> Option<Result<Line, KeptError>> {
        // Each turn takes one line of the file, which is finite.
        for (index, text) in self.rest.by_ref() {
            let content = text.trim_end_matches([' ', '\t', '\r']);
            if content.is_empty() || content.starts_with('#') {
                continue;
            }
            let number = index + 1;
            return Some(match parse(content) {
                Ok((test, version, tape)) => Ok(Line { number, test, version, tape }),
                Err(problem) => {
                    Err(KeptError::Line { krate: self.file.krate, line: number, problem })
                }
            });
        }
        None
    }
}

/// Splits one line that is not blank nor a comment, trailing white space cut, into its test
/// path, version and tape text, the tape checked to parse.
fn parse(content: &'static str) -> Result<(&'static str, u32, &'static str), LineProblem> {
    let content = match content.find('#') {
        Some(hash) => {
            let before = &content[..hash];
            if !before.ends_with(' ') {
                return Err(LineProblem::NoSpaceBeforeComment);
            }
            before.trim_end_matches(' ')
        }
        None => content,
    };
    let Some((test, rest)) = content.split_once(' ') else {
        return Err(LineProblem::NoVersion);
    };
    if test.is_empty() || test.contains('\t') {
        return Err(LineProblem::NoTest);
    }
    let (version, tape) = rest.split_once(' ').unwrap_or((rest, ""));
    let version = version_of(version)?;
    // Only whether it parses: the replay parses it again into the tape it hands the search.
    text::parse(tape).map_err(LineProblem::Tape)?;
    Ok((test, version, tape))
}

/// `v<n>` with `n` decimal digits, 1 or more, at most `u32::MAX`.
fn version_of(word: &str) -> Result<u32, LineProblem> {
    let Some(digits) = word.strip_prefix('v') else {
        return Err(LineProblem::NotAVersion);
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(LineProblem::NotAVersion);
    }
    match digits.parse::<u32>() {
        Ok(0) | Err(_) => Err(LineProblem::NotAVersion),
        Ok(version) => Ok(version),
    }
}

/// What is wrong with a kept line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineProblem {
    /// The line starts with white space: no test path before it.
    NoTest,
    /// One word only: no ` v<n>` after the test path.
    NoVersion,
    /// The word after the test path is not `v` and a version of 1 or more.
    NotAVersion,
    /// A `#` with no space before it: a comment follows the tape after two spaces.
    NoSpaceBeforeComment,
    /// The tape's text does not parse.
    Tape(TextError),
}

impl fmt::Display for LineProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LineProblem::NoTest => f.write_str("it starts with white space, not a test path"),
            LineProblem::NoVersion => f.write_str("no ` v<primitives version>` after the test path"),
            LineProblem::NotAVersion => {
                f.write_str("the word after the test path is not `v` and a version of 1 or more")
            }
            LineProblem::NoSpaceBeforeComment => {
                f.write_str("a `#` with no space before it: the comment follows the tape after two spaces")
            }
            LineProblem::Tape(error) => write!(f, "{error}"),
        }
    }
}

/// Why a crate's kept lines were refused. Never skipped (rule 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeptError {
    /// A line that does not parse.
    Line { krate: &'static str, line: usize, problem: LineProblem },
    /// A line kept for a test that is not among the crate's sim tests.
    UnknownTest { krate: &'static str, line: usize, test: &'static str },
}

impl fmt::Display for KeptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeptError::Line { krate, line, problem } => write!(
                f,
                "blaze-sim: line {line} of `{krate}`'s {FILE_NAME} does not parse: {problem}; a kept line is `<test path> v<primitives version> <tape>  # <date> <why>` (rule 3: a kept line is never skipped)"
            ),
            KeptError::UnknownTest { krate, line, test } => write!(
                f,
                "blaze-sim: line {line} of `{krate}`'s {FILE_NAME} is kept for `{test}`, which is not among the crate's sim tests: the test was renamed or removed; move the line to the test's new path (rule 3: a kept line is never skipped)"
            ),
        }
    }
}

impl core::error::Error for KeptError {}

/// The distinct test paths of `file`'s lines, each at its first line, in file order; a line
/// that does not parse is given as its error.
pub fn paths(file: File) -> Paths {
    Paths { file, lines: file.lines() }
}

/// The iterator of [`paths`]. A path is new when no earlier line has it: the earlier lines are
/// read again, which a kept file of tens of lines makes cheap and which needs no list.
pub struct Paths {
    file: File,
    lines: Lines,
}

impl Iterator for Paths {
    type Item = Result<&'static str, KeptError>;

    fn next(&mut self) -> Option<Result<&'static str, KeptError>> {
        // Each turn takes one line of the file, which is finite.
        for line in self.lines.by_ref() {
            let line = match line {
                Ok(line) => line,
                Err(error) => return Some(Err(error)),
            };
            let seen = self
                .file
                .lines()
                .map_while(Result::ok)
                .take_while(|earlier| earlier.number < line.number)
                .any(|earlier| earlier.test == line.test);
            if !seen {
                return Some(Ok(line.test));
            }
        }
        None
    }
}

/// The first line of `file` that does not parse, or is kept for a test not in `known`.
pub fn unknown(file: File, known: &[&str]) -> Result<(), KeptError> {
    for line in file.lines() {
        let line = line?;
        if !known.contains(&line.test) {
            return Err(KeptError::UnknownTest { krate: file.krate, line: line.number, test: line.test });
        }
    }
    Ok(())
}

/// For a crate's ONE test that lists its sim tests: panics naming the first line of `file` that
/// does not parse or is kept for a test not in `known` (a test renamed or removed). One test
/// alone cannot see a line kept for no test; the crate's list can. Prints each line kept under
/// other primitives by name, as `check_kept` does: a refusal, not a failure.
#[track_caller]
pub fn all_known(file: File, known: &[&str]) {
    if let Err(error) = unknown(file, known) {
        panic!("{error}");
    }
    for line in file.lines().map_while(Result::ok) {
        if let Some(refusal) = KeptRefusal::of(file, &line) {
            say_refused(refusal);
        }
    }
}

/// A kept line this build does not replay. Never a failure: printed by name, and the search finds
/// the failure again, to be kept again under this build's version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeptRefusal {
    /// The line was kept under other primitives (`kept`) than this build's (`build`): other draws
    /// read its tape as another case, so replaying it would test nothing it was kept for.
    OtherPrimitives { krate: &'static str, line: usize, kept: u32, build: u32 },
}

impl KeptRefusal {
    /// The refusal of `line` of `file`, when this build does not replay it.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn of(file: File, line: &Line) -> Option<KeptRefusal> {
        match line.version == PRIMITIVES {
            true => None,
            false => Some(KeptRefusal::OtherPrimitives {
                krate: file.krate,
                line: line.number,
                kept: line.version,
                build: PRIMITIVES,
            }),
        }
    }
}

impl fmt::Display for KeptRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeptRefusal::OtherPrimitives { krate, line, kept, build } => write!(
                f,
                "blaze-sim: line {line} of `{krate}`'s {FILE_NAME} kept under v{kept}, not replayed by this v{build} build: the search re-finds it"
            ),
        }
    }
}

/// Why a kept line cannot be written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineRefused {
    /// The test path is empty or holds white space or `#`, which a kept line cannot hold.
    TestPath,
    /// The writer it was written into refused (a growable text never does).
    Write(fmt::Error),
}

impl From<fmt::Error> for LineRefused {
    fn from(error: fmt::Error) -> LineRefused {
        LineRefused::Write(error)
    }
}

/// How a line's why is written: its first line. A control plants the whole text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    FirstLine,
    /// The planted bug: the why written whole, its line ends with it.
    #[cfg(test)]
    Whole,
}

/// Writes the line to keep into `out`: `<test> v<version> <tape>  # <date> <why>`, the tape as
/// [`text::print`] writes it (trailing zeros cut; none at all for an all-calm tape), the why its
/// first line and at most 160 characters. A refused test path writes nothing.
pub fn write_line(
    out: &mut impl fmt::Write,
    test: &str,
    version: u32,
    tape: &[u64],
    date: &str,
    why: &str,
) -> Result<(), LineRefused> {
    write_line_with(out, test, version, tape, date, why, Why::FirstLine)
}

/// [`write_line`], the why written as `how` says.
pub(crate) fn write_line_with(
    out: &mut impl fmt::Write,
    test: &str,
    version: u32,
    tape: &[u64],
    date: &str,
    why: &str,
    how: Why,
) -> Result<(), LineRefused> {
    if test.is_empty() || test.contains(|c: char| c.is_whitespace() || c == '#') {
        return Err(LineRefused::TestPath);
    }
    write!(out, "{test} v{version}")?;
    let tape = text::print(tape);
    if !tape.is_empty() {
        write!(out, " {tape}")?;
    }
    write!(out, "  # {date}")?;
    let why = match how {
        Why::FirstLine => why.lines().next().unwrap_or_default().trim_end_matches('\r'),
        #[cfg(test)]
        Why::Whole => why,
    };
    if !why.is_empty() {
        out.write_char(' ')?;
        for c in why.chars().take(WHY_MOST) {
            out.write_char(c)?;
        }
    }
    Ok(())
}

/// The report's lines to keep a failure, written into `out`: `blaze-sim keep: <line>` and where
/// to add it. `place` is the test path the report names the test by; `at` the check's call.
pub(crate) fn keep_lines(
    out: &mut impl fmt::Write,
    failed: &Failed,
    place: &str,
    at: &Location<'_>,
) -> fmt::Result {
    // `Record::values_into` and `text::print` take the tape as a list; the report's own list is
    // inside `report_failed`, so the keep line reads the record once more.
    let mut values = Vec::new();
    failed.least.values_into(&mut values);
    write!(out, "\n{KEEP}")?;
    match write_line(out, place, PRIMITIVES, &values, DATE_TO_FILL, &failed.failure.text) {
        Ok(()) => write!(
            out,
            "\n  add it to {FILE_NAME} beside the Cargo.toml of the crate holding {}, with today's date for {DATE_TO_FILL}, and replay it first with `blaze_sim::check_kept(config, blaze_sim::sim_kept!(), body)`",
            at.file()
        ),
        Err(LineRefused::Write(error)) => Err(error),
        Err(LineRefused::TestPath) => write!(
            out,
            "none: the test path `{place}` is empty or holds white space or `#`, which a kept line cannot hold"
        ),
    }
}

/// The test a check runs for, as the report names it: the test's name on the thread, or else
/// `file:line` of the check (`check.rs` `place`, which writes it as text; this compares it
/// without writing it).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Test<'t> {
    pub name: Option<&'t str>,
    pub at: &'t Location<'t>,
}

impl Test<'_> {
    /// Whether a kept line's test path is this test.
    fn is(self, path: &str) -> bool {
        match self.name {
            Some(name) => path == name,
            None => path.rsplit_once(':').is_some_and(|(file, line)| {
                file == self.at.file() && line.parse::<u32>() == Ok(self.at.line())
            }),
        }
    }
}

impl fmt::Display for Test<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name {
            Some(name) => f.write_str(name),
            None => write!(f, "{}:{}", self.at.file(), self.at.line()),
        }
    }
}

/// `check` with this test's kept failures replayed FIRST (piece S3): the lines of `kept` (from
/// [`sim_kept!`](crate::sim_kept), the crate's `sim-kept.tapes` compiled in) whose test path is
/// this test's, the one `report` names it by, each run alone on its tape in file order; then
/// `check(config, body)`, the search (which ends in `check_given`), with what the person or the
/// runner gave. A kept tape that does not pass panics with `kept tape replayed first: case <k> of
/// <n>` and its line; a line of the file that does not parse panics naming it; a line kept under
/// other primitives is not replayed and is printed as a [`KeptRefusal`]. Reads no file at run time
/// (rule 75).
#[track_caller]
pub fn check_kept<O: Outcome>(config: Config, kept: File, body: impl FnMut(&Tape) -> O) {
    let at = Location::caller();
    if let Err(failed) = kept_then_search(config, kept, body, Order::KeptFirst) {
        with_test_name(|name| panic!("{}", failed.said(Test { name, at })));
    }
}

/// Which runs first: the kept lines (right) or the search (the planted bug a test catches).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Order {
    KeptFirst,
    #[cfg(test)]
    SearchFirst,
}

/// Which kept lines are replayed: this build's version alone (right), or every line (the planted
/// bug a test catches: "replay anyway").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Versions {
    This,
    #[cfg(test)]
    All,
}

impl Versions {
    /// Whether `line` is replayed.
    fn replays(self, line: &Line) -> bool {
        match self {
            Versions::This => line.version == PRIMITIVES,
            #[cfg(test)]
            Versions::All => true,
        }
    }
}

/// What the kept lines of a test came to when all passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Replayed {
    /// This test's lines replayed.
    pub kept: usize,
    /// This test's lines not replayed, each a [`KeptRefusal`] printed by name.
    pub refused: usize,
}

/// Why `check_kept` failed before or instead of its search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KeptFailed {
    /// The file has a line that does not parse.
    File(KeptError),
    /// A kept line's tape did not pass.
    Replay(ReplayFailed),
}

/// A kept tape that did not pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReplayFailed {
    /// Which of this test's kept tapes, from 1, of `of`.
    pub case: usize,
    pub of: usize,
    /// The kept line.
    pub line: Line,
    /// The crate whose file it is in.
    pub krate: &'static str,
    /// The verdict of its run.
    pub verdict: Verdict,
}

impl KeptFailed {
    /// What the check panics with, for `test`.
    pub(crate) fn said<'t>(&self, test: Test<'t>) -> Said<'_, 't> {
        Said { failed: self, test }
    }
}

/// A [`KeptFailed`] in words, for one test.
pub(crate) struct Said<'f, 't> {
    failed: &'f KeptFailed,
    test: Test<'t>,
}

impl fmt::Display for Said<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let failed = match self.failed {
            KeptFailed::File(error) => return write!(f, "{error}"),
            KeptFailed::Replay(failed) => failed,
        };
        let test = self.test;
        write!(
            f,
            "blaze-sim: kept tape replayed first: case {} of {} kept for `{test}` (line {} of `{}`'s {FILE_NAME}",
            failed.case, failed.of, failed.line.number, failed.krate
        )?;
        write!(f, ")\n{}", report(&failed.verdict, test.at, test.name))
    }
}

/// `check_kept`'s work, without its panic: this test's kept lines of `file` replayed in file
/// order, each alone on its tape, then `check(config, body)`, or the other way round under the
/// planted `order`. The search's own failure panics inside `check`.
#[track_caller]
pub(crate) fn kept_then_search<O: Outcome>(
    config: Config,
    file: File,
    body: impl FnMut(&Tape) -> O,
    order: Order,
) -> Result<Replayed, KeptFailed> {
    kept_then_search_with(config, file, body, order, Versions::This)
}

/// [`kept_then_search`], replaying the lines `versions` says.
#[track_caller]
pub(crate) fn kept_then_search_with<O: Outcome>(
    config: Config,
    file: File,
    mut body: impl FnMut(&Tape) -> O,
    order: Order,
    versions: Versions,
) -> Result<Replayed, KeptFailed> {
    let at = Location::caller();
    match order {
        Order::KeptFirst => {
            let replayed = replay(&config, file, at, versions, &mut body)?;
            check(config, body);
            Ok(replayed)
        }
        #[cfg(test)]
        Order::SearchFirst => {
            check(config.clone(), &mut body);
            replay(&config, file, at, versions, &mut body)
        }
    }
}

/// Replays this test's lines of `file` that `versions` replays, in file order; the others are
/// refused by name. Every line of the file is read first, so a line that does not parse fails
/// every test that reads the file.
fn replay<O: Outcome>(
    config: &Config,
    file: File,
    at: &Location<'_>,
    versions: Versions,
    body: &mut impl FnMut(&Tape) -> O,
) -> Result<Replayed, KeptFailed> {
    with_test_name(|name| {
        let test = Test { name, at };
        let mut of = 0;
        for line in file.lines() {
            let line = line.map_err(KeptFailed::File)?;
            if test.is(line.test) && versions.replays(&line) {
                of += 1;
            }
        }
        let mine = |line: &Line| test.is(line.test) && versions.replays(line);
        let mut case = 0;
        for line in file.lines().map_while(Result::ok).filter(mine) {
            case += 1;
            let tape = text::parse(line.tape).map_err(|error| {
                KeptFailed::File(KeptError::Line {
                    krate: file.krate,
                    line: line.number,
                    problem: LineProblem::Tape(error),
                })
            })?;
            // A kept tape runs alone, so a point it does not reach is no failure of it.
            let given = Given { seed: None, seeds: None, tape: Some(tape) };
            let verdict = search(config.clone().must_reach(&[]).given(given), &mut *body);
            if !matches!(verdict, Verdict::Passed(_)) {
                return Err(KeptFailed::Replay(ReplayFailed {
                    case,
                    of,
                    line,
                    krate: file.krate,
                    verdict,
                }));
            }
        }
        let refused = say_replayed(file, test, of, versions);
        Ok(Replayed { kept: of, refused })
    })
}

/// Prints the line of a test's kept tapes that all passed.
#[expect(clippy::print_stdout, reason = "a check's pass line goes to the test's own output")]
#[expect(
    blaze::std_use,
    reason = "a check's pass line goes to the test's own output, which the test harness captures through `println!` alone"
)]
#[expect(
    blaze::effect_breadth,
    reason = "one `println!`: std's `_print` brings IO, blocking, locking and allocating and its arguments' formatting panicking, so any function that prints a line has all five"
)]
fn say_replayed(file: File, test: Test<'_>, kept: usize, versions: Versions) -> usize {
    let mut refused = 0;
    for line in file.lines().map_while(Result::ok) {
        if !test.is(line.test) || versions.replays(&line) {
            continue;
        }
        if let Some(refusal) = KeptRefusal::of(file, &line) {
            say_refused(refusal);
            refused += 1;
        }
    }
    if kept > 0 {
        println!("blaze-sim: {test}: {kept} kept tapes replayed first, all passed");
    }
    refused
}

/// Prints a kept line this build does not replay, by name.
#[expect(clippy::print_stdout, reason = "a refusal goes to the test's own output, beside its pass line")]
#[expect(
    blaze::std_use,
    reason = "a refusal goes to the test's own output, which the test harness captures through `println!` alone"
)]
#[expect(
    blaze::effect_breadth,
    reason = "one `println!`: std's `_print` brings IO, blocking, locking and allocating and its arguments' formatting panicking, so any function that prints a line has all five"
)]
fn say_refused(refusal: KeptRefusal) {
    println!("{refusal}");
}

// Declared after `sim_kept!` so the tests name it by textual scope: `crate::sim_kept!` is refused
// inside the crate ("macro-expanded `macro_export` macros ... cannot be referred to by absolute paths").
#[cfg(test)]
mod tests;
