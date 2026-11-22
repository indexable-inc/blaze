//! Kept failures (piece S3): a failure's report holds one line to keep, the line replays the
//! failure first, a line reads back as written, and a line that does not parse or whose test is
//! gone is refused naming its line.
//!
//! A kept line is kept for the test path the check names its test by: the name libtest gives
//! the test's thread. So each test here writes its lines for its own name ([`this_test`]).

#![expect(
    blaze::error_debug,
    reason = "a test's failure message shows the values it compared as they are"
)]

use core::panic::Location;
use std::cell::Cell;

use super::{
    DATE_TO_FILL, File, KEEP, KeptError, KeptFailed, KeptRefusal, LineProblem, Order, PRIMITIVES,
    Replayed, Test, Versions, Why, all_known, kept_then_search, kept_then_search_with, paths,
    unknown, write_line, write_line_with,
};
use crate::check::{report_here, search, with_test_name};
use crate::search::{Broke, Config, Verdict};
use crate::tape::Tape;
use crate::text;

/// The crate every file in these tests is named by.
const KRATE: &str = "blaze-sim";

/// A key no search of a few hundred cases draws by chance: only a kept tape gives it.
const KEY: u64 = 0x5EED_0FCA_FEF0_0D17;

/// A file's text that lives as long as the test binary: a kept file is compiled in, so its text
/// is `'static`; a test builds one at run time and leaks it.
fn leaked(text: String) -> &'static str {
    text.leak()
}

/// The test path of the test running on this thread: its thread's name, which libtest gives it.
fn this_test() -> String {
    with_test_name(|name| {
        name.expect("libtest runs each test on a thread named after it (not with --test-threads=1)")
            .to_owned()
    })
}

/// A kept line for `test`, as a test writes one.
fn kept_line(test: &str, tape: &[u64], why: &str) -> String {
    let mut out = String::new();
    write_line(&mut out, test, PRIMITIVES, tape, "2026-11-16", why)
        .expect("a test's name is a path a kept line holds");
    out
}

/// What a `KeptFailed` panics with on this thread.
fn said(failed: &KeptFailed) -> String {
    let at = Location::caller();
    with_test_name(|name| failed.said(Test { name, at }).to_string())
}

/// The oracle of the body the tests keep a failure of: `x` below 700. Its assert is one place,
/// so a failure found by a search and the same failure replayed panic at the same `file:line`.
fn below_700(x: u64) {
    assert!(x < 700, "x is {x}, at or above 700");
}

/// The oracle of the key: the one value only a kept tape gives fails.
fn not_the_key(key: u64) {
    assert!(key != KEY, "the key {key} was drawn");
}

/// The lines of a report that start with `blaze-sim keep: `, that prefix cut.
fn keeps(report: &str) -> Vec<&str> {
    report.lines().filter_map(|text| text.strip_prefix(KEEP)).collect()
}

/// (a) A failing body's report holds exactly one `blaze-sim keep:` line, and that line, as the
/// crate's kept file, fails `check_kept`'s work on its FIRST case with the same failure (the same
/// panic message at the same place), before any search.
///
/// Prints: `kept tape replayed first: case 1 (x is 700, at or above 700)`
#[test]
fn a_failure_is_kept_and_its_line_replays_it_first() {
    let at = Location::caller();
    let found = search(Config::cases(200), |tape: &Tape| below_700(tape.below(1000, "x")));
    let Verdict::Failed(first) = &found else {
        panic!("the search found no failure of x below 700: {found:?}");
    };
    let report = report_here(&found, at);
    let keeps = keeps(&report);
    assert_eq!(keeps.len(), 1, "one line to keep, in:\n{report}");
    assert!(keeps[0].starts_with(&this_test()), "kept for this test: `{}`", keeps[0]);
    let file = File::new(leaked(format!("# kept by the test\n\n{}\n", keeps[0])), KRATE);

    let calls = Cell::new(0u32);
    let first_failed: Cell<Option<u32>> = Cell::new(None);
    let body = |tape: &Tape| {
        calls.set(calls.get() + 1);
        let x = tape.below(1000, "x");
        if x >= 700 && first_failed.get().is_none() {
            first_failed.set(Some(calls.get()));
        }
        below_700(x);
    };
    let got = kept_then_search(Config::cases(200), file, body, Order::KeptFirst);
    let Err(failed @ KeptFailed::Replay(replay)) = &got else {
        panic!("the kept line did not fail its replay: {got:?}");
    };
    let message = said(failed);
    assert_eq!(replay.case, 1, "{message}");
    assert_eq!(replay.line.number, 3, "{message}");
    assert_eq!(first_failed.get(), Some(1), "the kept tape is the first case the body runs");
    let Verdict::Failed(again) = &replay.verdict else {
        panic!("the kept tape ended other than failed: {:?}", replay.verdict);
    };
    assert_eq!(again.failure, first.failure, "the same panic, at the same place");
    assert!(message.contains("kept tape replayed first: case 1 of 1"), "{message}");
    println!("kept tape replayed first: case {} ({})", replay.case, again.failure.text);
}

/// The planted bug of [`a_kept_line_reads_back_as_written`]: a why written whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plant {
    None,
    /// The why is written whole, its line ends with it, so the line is cut in two.
    WhyWhole,
}

/// A kept line written from a drawn test path, version, tape and why, read back through the
/// file's reader: one line, of that test, that version, that tape with its trailing zeros cut.
fn reads_back(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let test = tape.text_from("abz_:09/.-<>", 1..=24, "test");
    let version = tape.int_in(1u32..=u32::MAX, "version");
    let values = tape.vec(0..=12, "values", |t| match t.below(3, "kind") {
        0 => 0,
        1 => t.below(4, "small"),
        _ => t.int::<u64>("any"),
    });
    let why = tape.text_from("ab #\n\r\t", 0..=20, "why");
    let how = match plant {
        Plant::None => Why::FirstLine,
        Plant::WhyWhole => Why::Whole,
    };
    let mut written = String::new();
    write_line_with(&mut written, &test, version, &values, DATE_TO_FILL, &why, how)
        .map_err(|refused| Broke::Harness(format!("`{test}` was refused: {refused:?}")))?;
    let file = File::new(leaked(format!("{written}\n")), KRATE);
    let read: Vec<_> = file.lines().collect();
    let cut = values.iter().rposition(|&value| value != 0).map_or(0, |last| last + 1);
    match read.as_slice() {
        [Ok(line)]
            if line.test == test
                && line.version == version
                && text::parse(line.tape).as_deref() == Ok(&values[..cut]) =>
        {
            Ok(())
        }
        other => Err(Broke::Oracle(format!(
            "`{written}` read back as {other:?}, not one line of `{test}` v{version} {:?}",
            &values[..cut]
        ))),
    }
}

/// The invariant: every kept failure line written by `write_line` round-trips through the kept-file parser as a single line without splitting on its reason text.
/// (b) A property of the engine itself: for drawn test paths, versions, tapes and whys, the line
/// [`write_line`] writes reads back through the file's reader as exactly that line: text -> line
/// -> parse -> tape is the identity (the tape's trailing zeros cut, as a tape's text always is).
///
/// Prints: `blaze-sim: kept::tests::a_kept_line_reads_back_as_written: 300 cases, <n> choices`
#[test]
fn a_kept_line_reads_back_as_written() {
    crate::check(Config::cases(300), |tape| reads_back(tape, Plant::None));
}

/// The control of (b): a why written whole cuts the line in two, and the oracle says so.
///
/// Prints nothing of its own; passes when the plant is caught saying "read back as".
#[test]
fn the_oracle_catches_a_why_written_whole() {
    crate::control(Config::cases(300), Plant::None, Plant::WhyWhole, "read back as", reads_back);
}

/// The body of the order tests: draws one raw key and fails on [`KEY`] alone; `first_failed`
/// gets the number of the first call that failed.
fn keyed<'c>(calls: &'c Cell<u32>, first_failed: &'c Cell<Option<u32>>) -> impl FnMut(&Tape) + 'c {
    move |tape: &Tape| {
        calls.set(calls.get() + 1);
        let key = tape.next_u64("key");
        if key == KEY && first_failed.get().is_none() {
            first_failed.set(Some(calls.get()));
        }
        not_the_key(key);
    }
}

/// (c) The planted bug: a `check_kept` that searches before it replays. The kept failure is
/// found on case 1 when the kept lines go first, and only after the search's 64 cases when the
/// search goes first; the test asserts both case numbers.
///
/// Prints: `kept first: case 1; search first: case 65`
#[test]
fn a_check_kept_that_searches_first_is_caught() {
    let kept = kept_line(&this_test(), &[KEY], "the key only a kept tape gives");
    let file = File::new(leaked(format!("{kept}\n")), KRATE);
    let config = Config::cases(64);

    let (calls, first_failed) = (Cell::new(0u32), Cell::new(None));
    let right = kept_then_search(config.clone(), file, keyed(&calls, &first_failed), Order::KeptFirst);
    assert!(matches!(&right, Err(KeptFailed::Replay(failed)) if failed.case == 1), "{right:?}");
    let kept_first = first_failed.get();
    assert_eq!(kept_first, Some(1), "kept first: the kept tape is case 1");

    let (calls, first_failed) = (Cell::new(0u32), Cell::new(None));
    let planted = kept_then_search(config, file, keyed(&calls, &first_failed), Order::SearchFirst);
    assert!(matches!(&planted, Err(KeptFailed::Replay(_))), "{planted:?}");
    let search_first = first_failed.get();
    assert!(
        search_first.is_some_and(|case| case > 64),
        "search first: the kept failure came at {search_first:?}, after the search's 64 cases"
    );
    assert_ne!(kept_first, search_first, "the order is seen by the case number");
    println!("kept first: case 1; search first: case {}", search_first.unwrap_or(0));
}

/// (d) A line that does not parse fails `check_kept`'s work naming its line, before any case
/// runs; each way a line is malformed is refused by name; a line kept for a test that is gone
/// is refused by [`unknown`] naming its line, and [`paths`] lists the file's tests.
///
/// Prints: `refused: line 4 (not a version), line 3 (gone::test)`
#[test]
fn a_malformed_line_and_a_line_for_no_test_are_refused_naming_the_line() {
    let file = File::new("# kept\n\nkept::d v1 5  # fine\nkept::d 5\n", KRATE);
    let calls = Cell::new(0u32);
    let got = kept_then_search(
        Config::cases(8),
        file,
        |tape: &Tape| {
            calls.set(calls.get() + 1);
            let _x = tape.below(10, "x");
        },
        Order::KeptFirst,
    );
    let Err(failed @ KeptFailed::File(error)) = &got else {
        panic!("a malformed line was not refused: {got:?}");
    };
    assert_eq!(
        *error,
        KeptError::Line { krate: KRATE, line: 4, problem: LineProblem::NotAVersion },
        "{error}"
    );
    let message = said(failed);
    assert!(message.contains("line 4 of `blaze-sim`'s sim-kept.tapes"), "{message}");
    assert_eq!(calls.get(), 0, "nothing runs before the file is whole");

    for (text, problem) in [
        (" kept::d v1 5", LineProblem::NoTest),
        ("kept::d", LineProblem::NoVersion),
        ("kept::d v0 5", LineProblem::NotAVersion),
        ("kept::d 1 5", LineProblem::NotAVersion),
        ("kept::d v1 5# c", LineProblem::NoSpaceBeforeComment),
    ] {
        let read: Vec<_> = File::new(text, KRATE).lines().collect();
        assert_eq!(read, [Err(KeptError::Line { krate: KRATE, line: 1, problem })], "{text:?}");
    }
    let read: Vec<_> = File::new("kept::d v1 1  2", KRATE).lines().collect();
    assert!(
        matches!(read.as_slice(), [Err(KeptError::Line { line: 1, problem: LineProblem::Tape(_), .. })]),
        "{read:?}"
    );

    let gone = File::new(
        "kept::d v1 5\n# a comment\ngone::test v1  # renamed since\nkept::d v1 6\n",
        KRATE,
    );
    let listed: Result<Vec<_>, _> = paths(gone).collect();
    assert_eq!(listed, Ok(vec!["kept::d", "gone::test"]), "each path once, at its first line");
    let refused = unknown(gone, &["kept::d"]);
    assert_eq!(
        refused,
        Err(KeptError::UnknownTest { krate: KRATE, line: 3, test: "gone::test" })
    );
    assert_eq!(unknown(gone, &["kept::d", "gone::test"]), Ok(()));
    println!("refused: line 4 (not a version), line 3 (gone::test)");
}

/// (e) A line kept under the other primitives version is refused by name, never replayed and
/// never a failure, even when its tape fails the body: `check_kept`'s work passes with one
/// refusal, and `all_known` prints the same refusal without failing. The planted bug, "replay
/// anyway" (`Versions::All`), runs the tape on case 1 and fails: the test catches it.
///
/// Prints (feature `edge-first` off; with it on, v1 and v2 swap):
/// `blaze-sim: line 1 of `blaze-sim`'s sim-kept.tapes kept under v2, not replayed by this v1 build: the search re-finds it`
/// (twice: from the replay and from `all_known`), then
/// `other primitives: 1 refused, 0 replayed; replayed anyway: case 1`
#[test]
fn a_line_of_the_other_primitives_version_is_refused_not_replayed() {
    let other = if PRIMITIVES == 1 { 2 } else { 1 };
    let test = this_test();
    let file = File::new(leaked(format!("{test} v{other} {KEY}  # the key, other draws\n")), KRATE);

    let (calls, first_failed) = (Cell::new(0u32), Cell::new(None));
    let got =
        kept_then_search(Config::cases(8), file, keyed(&calls, &first_failed), Order::KeptFirst);
    assert_eq!(got, Ok(Replayed { kept: 0, refused: 1 }), "refused, not replayed, not failed");
    assert_eq!(first_failed.get(), None, "the other version's tape never ran");
    let Some(Ok(line)) = file.lines().next() else {
        panic!("the file's one line did not parse");
    };
    let refusal = KeptRefusal::of(file, &line);
    assert_eq!(
        refusal,
        Some(KeptRefusal::OtherPrimitives { krate: KRATE, line: 1, kept: other, build: PRIMITIVES })
    );
    let said = refusal.map(|refusal| refusal.to_string()).unwrap_or_default();
    let expected = format!(
        "blaze-sim: line 1 of `blaze-sim`'s sim-kept.tapes kept under v{other}, not replayed by this v{PRIMITIVES} build: the search re-finds it"
    );
    assert_eq!(said, expected);
    all_known(file, &[test.as_str()]);

    let (calls, first_failed) = (Cell::new(0u32), Cell::new(None));
    let planted = kept_then_search_with(
        Config::cases(8),
        file,
        keyed(&calls, &first_failed),
        Order::KeptFirst,
        Versions::All,
    );
    let Err(KeptFailed::Replay(replay)) = &planted else {
        panic!("replayed anyway, the key's tape did not fail: {planted:?}");
    };
    assert_eq!((replay.case, replay.line.version), (1, other));
    assert_eq!(first_failed.get(), Some(1), "replayed anyway: the other version's tape ran first");
    println!("other primitives: 1 refused, 0 replayed; replayed anyway: case {}", replay.case);
}

/// The crate's own `sim-kept.tapes` is compiled in by `sim_kept!`, named by the crate, and every
/// line in it is kept for a test that exists: this is blaze-sim's ONE `all_known` call, and its
/// list is the crate's tests that call `check_kept` on the file (none yet: a line added for a
/// test goes with that test's path in this list).
///
/// Prints: `blaze-sim keeps 0 lines`
#[test]
fn the_crates_kept_file_is_compiled_in_and_every_line_is_known() {
    const KEPT: File = sim_kept!();
    assert_eq!(KEPT.krate, KRATE);
    all_known(KEPT, &[]);
    assert_eq!(paths(KEPT).count(), 0);
    println!("blaze-sim keeps 0 lines");
}

/// A failure whose least tape is all calm prints a keep line with NO tape text
/// (`<test> v<n>  # ...`), and that line, kept, replays the failure on case 1: the empty tape
/// is the all-calm case.
///
/// Prints: `the all-calm keep line replays first: case 1`
#[test]
fn an_all_calm_failure_keeps_a_line_without_a_tape_that_replays() {
    let at = Location::caller();
    // Fails on every case, so the least tape shrinks to all calm.
    let found = search(Config::cases(20), |tape: &Tape| below_700(700 + tape.below(10, "x")));
    let Verdict::Failed(first) = &found else {
        panic!("the search found no failure: {found:?}");
    };
    let report = report_here(&found, at);
    let keeps = keeps(&report);
    assert_eq!(keeps.len(), 1, "one line to keep, in:\n{report}");
    let written = keeps[0];
    let start = format!("{} v{PRIMITIVES}  # ", this_test());
    assert!(written.starts_with(&start), "no tape text: `{written}`");
    let file = File::new(leaked(format!("{written}\n")), KRATE);
    let read: Vec<_> = file.lines().collect();
    assert!(matches!(read.as_slice(), [Ok(line)] if line.tape.is_empty()), "{read:?}");

    let calls = Cell::new(0u32);
    let got = kept_then_search(
        Config::cases(20),
        file,
        |tape: &Tape| {
            calls.set(calls.get() + 1);
            below_700(700 + tape.below(10, "x"));
        },
        Order::KeptFirst,
    );
    let Err(failed @ KeptFailed::Replay(replay)) = &got else {
        panic!("the all-calm kept line did not fail its replay: {got:?}");
    };
    // The search after the kept lines never ran: every call was the kept tape's (its run, and
    // the search's final run of a failing tape if it makes one).
    assert_eq!(replay.case, 1, "{}", said(failed));
    assert!((1..=2).contains(&calls.get()), "the body ran {} times", calls.get());
    let Verdict::Failed(again) = &replay.verdict else {
        panic!("the all-calm tape ended other than failed: {:?}", replay.verdict);
    };
    assert_eq!(again.failure, first.failure, "the same panic, at the same place");
    println!("the all-calm keep line replays first: case {}", replay.case);
}
