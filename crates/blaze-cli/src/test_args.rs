//! `blaze test`'s own arguments into the plan's settings (docs/v2/plan/design/test.md, "End
//! state", "Filters"; piece 7): a pure function from the argument list to [`TestArgs`] (which
//! gives [`blaze_run::plan::Config`]) or a typed [`TestArgsError`] naming the word.
//!
//! The words are the ones after `--` (`blaze test [dir] -- <these>`), in libtest's spelling, the
//! one the design keeps, plus blaze's own:
//!
//! | word | what |
//! |---|---|
//! | `FILTER` | a test runs when its path (`module::name`) holds this text; one at most |
//! | `--exact` | the filter is the whole path |
//! | `--skip FILTER`, `--skip=FILTER` | a test whose path holds this text is left out; repeatable |
//! | `--ignored` | only the `#[ignore]` tests run |
//! | `--include-ignored` | the `#[ignore]` tests run too |
//! | `--list` | the tests the filters let through are listed, none runs |
//! | `--format json`, `--format pretty` | one JSON object a line, or lines (the default) |
//! | `--test-threads N` | at most N tests at once (the default: the machine's cores) |
//! | `--nocapture`, `--no-capture` | one test at a time, its output not captured |
//! | `--benches` | blaze's: each `#[bench]` runs once as a test |
//! | `--run-all` | blaze's: every test let through runs; no kept result is used |
//! | `--deadline D` | blaze's: each test's bound from its start: `30`, `30s` or `500ms` (default 60 s) |
//!
//! Every other word that starts with `-` is refused BY NAME, with its text: libtest's
//! `--shuffle`, `--show-output`, `-q`, `--format terse`, `--bench`, `--logfile`, `--color` (a
//! flag of blaze's before `--`), `-Z ..` and any other; nothing is ignored (rule 3). A
//! `harness = false` target is given the same words as its own arguments ([`TestArgs::passed`]),
//! which are in its result's key.

#![cfg_attr(not(test), allow(dead_code, reason = "wired by the driver's lane: main.rs `command`'s test arm calls `parse` on the words after `--`"))]

use blaze_own::{AVec, Alloc, Vec};
use blaze_run::Nanos;
use blaze_run::plan::{Config, Filter, Ignored};
use blaze_run::report::Format;
use blaze_run::zygote::{self, PoolError};

/// A test's bound when `--deadline` is not given (test.md, "One process per test": 60 s).
pub(crate) const DEFAULT_DEADLINE: Nanos = Nanos(60_000_000_000);

/// The words the design takes, for the refusal's text.
const TAKEN: &str = "FILTER, --exact, --skip, --ignored, --include-ignored, --list, --format json|pretty, --test-threads, --nocapture, --benches, --run-all, --deadline";

/// `blaze test`'s arguments, parsed. Borrows the words (`'w`); the skip list is in `'a`.
#[derive(Debug)]
pub(crate) struct TestArgs<'w, 'a> {
    pub filter: Filter<'w>,
    pub skip: AVec<'a, &'w str>,
    pub ignored: Ignored,
    pub benches: bool,
    pub run_all: bool,
    pub list: bool,
    pub format: Format,
    /// `--test-threads`: at most this many at once.
    pub threads: Option<u32>,
    /// `--nocapture`: one at a time, output straight through.
    pub nocapture: bool,
    pub deadline: Nanos,
    /// Every word, as given: a `harness = false` target's own arguments, in its key.
    pub passed: &'w [&'w str],
}

impl TestArgs<'_, '_> {
    /// The width asked for: `--nocapture` asks for one at a time, `--test-threads N` for `N`,
    /// else nothing (`parse` refuses the two together when they disagree).
    pub(crate) fn asked_width(&self) -> Option<u32> {
        match self.nocapture {
            true => Some(1),
            false => self.threads,
        }
    }

    /// The plan's settings on a machine of `cores` performance cores. The width is
    /// [`zygote::width`]'s, the one rule the test pool uses too.
    pub(crate) fn config(&self, cores: u32) -> Result<Config<'_>, PoolError> {
        Ok(Config {
            width: zygote::width(cores, self.asked_width())?,
            deadline: self.deadline,
            run_all: self.run_all,
            ignored: self.ignored,
            benches: self.benches,
            filter: self.filter,
            skip: self.skip.as_slice(),
        })
    }
}

/// Why `blaze test`'s arguments were refused. Each names the word and the rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TestArgsError<'w> {
    /// A flag the design does not take: refused by name, never ignored (rule 3).
    NotTaken { flag: &'w str },
    /// A flag that takes a value came last.
    MissingValue { flag: &'static str },
    /// A flag's value is not one it takes.
    BadValue { flag: &'static str, value: &'w str, want: &'static str },
    /// A flag that takes one value was given twice.
    TwoValues { flag: &'static str },
    /// Two filters: the plan takes one (libtest's "any of them" is not taken).
    TwoFilters { first: &'w str, second: &'w str },
    /// `--exact` with no filter to be exact about.
    ExactWithoutFilter,
    /// `--ignored` and `--include-ignored` together: one says only, the other says too.
    IgnoredBoth,
    /// `--nocapture` runs one at a time, and `--test-threads` asked for more.
    NocaptureThreads { threads: u32 },
}

impl core::fmt::Display for TestArgsError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            TestArgsError::NotTaken { flag } => write!(f, "`blaze test` does not take `{flag}`: it takes {TAKEN} (test.md, \"Filters\"; rule 3: no flag is ignored)"),
            TestArgsError::MissingValue { flag } => write!(f, "`blaze test`'s `{flag}` takes a value, and none follows it"),
            TestArgsError::BadValue { flag, value, want } => write!(f, "`blaze test`'s `{flag}` takes {want}, not `{value}`"),
            TestArgsError::TwoValues { flag } => write!(f, "`blaze test`'s `{flag}` is given twice; it takes one value"),
            TestArgsError::TwoFilters { first, second } => {
                write!(f, "`blaze test` takes one filter, and was given `{first}` and `{second}` (test.md, \"Filters\")")
            }
            TestArgsError::ExactWithoutFilter => f.write_str("`blaze test`'s `--exact` needs a filter to match exactly"),
            TestArgsError::IgnoredBoth => f.write_str("`blaze test` takes `--ignored` (only the ignored tests) or `--include-ignored` (them too), not both"),
            TestArgsError::NocaptureThreads { threads } => {
                write!(f, "`blaze test`'s `--nocapture` runs one test at a time, and `--test-threads {threads}` asks for more")
            }
        }
    }
}

impl core::error::Error for TestArgsError<'_> {}

/// A flag's value: `--flag=value` in one word, or `--flag value` in two.
fn value<'w>(words: &'w [&'w str], at: &mut usize, word: &'w str, flag: &'static str) -> Result<Option<&'w str>, TestArgsError<'w>> {
    if word == flag {
        *at += 1;
        return match words.get(*at - 1) {
            Some(next) => Ok(Some(*next)),
            None => Err(TestArgsError::MissingValue { flag }),
        };
    }
    match word.strip_prefix(flag).and_then(|rest| rest.strip_prefix('=')) {
        Some(given) => Ok(Some(given)),
        None => Ok(None),
    }
}

/// `30`, `30s` (seconds) or `500ms`: at least one millisecond.
fn deadline(text: &str) -> Option<Nanos> {
    let (digits, unit) = match (text.strip_suffix("ms"), text.strip_suffix('s')) {
        (Some(digits), _) => (digits, 1_000_000u64),
        (None, Some(digits)) => (digits, 1_000_000_000),
        (None, None) => (text, 1_000_000_000),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    let nanos = n.checked_mul(unit)?;
    (nanos > 0).then_some(Nanos(nanos))
}

/// Sets a one-value flag once.
fn once<'w, T>(slot: &mut Option<T>, value: T, flag: &'static str) -> Result<(), TestArgsError<'w>> {
    match slot {
        Some(_) => Err(TestArgsError::TwoValues { flag }),
        None => {
            *slot = Some(value);
            Ok(())
        }
    }
}

/// Parses `words` (the words after `--`). See the module's table. The skip list is built in
/// `alloc`; everything else borrows `words`.
pub(crate) fn parse<'w, 'a>(words: &'w [&'w str], alloc: Alloc<'a>) -> Result<TestArgs<'w, 'a>, TestArgsError<'w>> {
    let mut filter: Option<&'w str> = None;
    let mut exact = false;
    let mut skip: AVec<'a, &'w str> = Vec::new_in(alloc);
    let (mut only_ignored, mut include_ignored) = (false, false);
    let (mut benches, mut run_all, mut list, mut nocapture) = (false, false, false, false);
    let mut format: Option<Format> = None;
    let mut threads: Option<u32> = None;
    let mut bound: Option<Nanos> = None;
    let mut at = 0;
    while let Some(word) = words.get(at).copied() {
        at += 1;
        match word {
            "--exact" => exact = true,
            "--ignored" => only_ignored = true,
            "--include-ignored" => include_ignored = true,
            "--list" => list = true,
            "--nocapture" | "--no-capture" => nocapture = true,
            "--benches" => benches = true,
            "--run-all" => run_all = true,
            _ if !word.starts_with('-') => match filter {
                Some(first) => return Err(TestArgsError::TwoFilters { first, second: word }),
                None => filter = Some(word),
            },
            _ => {
                if let Some(text) = value(words, &mut at, word, "--skip")? {
                    skip.push(text);
                } else if let Some(text) = value(words, &mut at, word, "--format")? {
                    let chosen = match text {
                        "json" => Format::Json,
                        "pretty" => Format::Text,
                        _ => return Err(TestArgsError::BadValue { flag: "--format", value: text, want: "`json` or `pretty`" }),
                    };
                    once(&mut format, chosen, "--format")?;
                } else if let Some(text) = value(words, &mut at, word, "--test-threads")? {
                    let n = match text.parse::<u32>() {
                        Ok(n) if n > 0 && text.bytes().all(|b| b.is_ascii_digit()) => n,
                        _ => return Err(TestArgsError::BadValue { flag: "--test-threads", value: text, want: "a count of at least 1" }),
                    };
                    once(&mut threads, n, "--test-threads")?;
                } else if let Some(text) = value(words, &mut at, word, "--deadline")? {
                    let Some(nanos) = deadline(text) else {
                        return Err(TestArgsError::BadValue { flag: "--deadline", value: text, want: "a time of at least 1 ms: `30`, `30s` or `500ms`" });
                    };
                    once(&mut bound, nanos, "--deadline")?;
                } else {
                    return Err(TestArgsError::NotTaken { flag: word });
                }
            }
        }
    }
    let ignored = match (only_ignored, include_ignored) {
        (true, true) => return Err(TestArgsError::IgnoredBoth),
        (true, false) => Ignored::Only,
        (false, true) => Ignored::Include,
        (false, false) => Ignored::Skip,
    };
    let filter = match (filter, exact) {
        (None, true) => return Err(TestArgsError::ExactWithoutFilter),
        (None, false) => Filter::All,
        (Some(text), false) => Filter::Substring(text),
        (Some(text), true) => Filter::Exact(text),
    };
    if let (true, Some(n)) = (nocapture, threads)
        && n > 1
    {
        return Err(TestArgsError::NocaptureThreads { threads: n });
    }
    Ok(TestArgs {
        filter,
        skip,
        ignored,
        benches,
        run_all,
        list,
        format: format.unwrap_or(Format::Text),
        threads,
        nocapture,
        deadline: bound.unwrap_or(DEFAULT_DEADLINE),
        passed: words,
    })
}

#[cfg(test)]
mod tests {
    use blaze_run::Nanos;
    use blaze_run::plan::{Filter, Ignored};
    use blaze_run::report::Format;

    use super::{DEFAULT_DEADLINE, TestArgsError, parse};

    /// What one argument list must give: the settings that matter, or the refusal.
    struct Row {
        words: &'static [&'static str],
        want: Result<Want, TestArgsError<'static>>,
    }

    #[derive(PartialEq, Eq, Debug)]
    struct Want {
        filter: Filter<'static>,
        skip: &'static [&'static str],
        ignored: Ignored,
        benches: bool,
        run_all: bool,
        list: bool,
        format: Format,
        /// The plan's width on an 8-core machine.
        width: u32,
        deadline: Nanos,
    }

    const PLAIN: Want = Want {
        filter: Filter::All,
        skip: &[],
        ignored: Ignored::Skip,
        benches: false,
        run_all: false,
        list: false,
        format: Format::Text,
        width: 8,
        deadline: DEFAULT_DEADLINE,
    };

    /// The table: every word the design takes, in each spelling, and a refusal by name for each
    /// libtest or cargo flag it does not take (test.md, "End state", "Filters": "libtest's flags
    /// (`--nocapture` runs one at a time, `--test-threads`, `--list`, `--format json`) are
    /// blaze's; any other is refused by name"). A parser that ignores an unknown flag fails the
    /// `--shuffle` row; one that takes the last of two filters fails the `TwoFilters` row.
    ///
    /// Expected, from `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh
    /// blaze-cli test test_args`: `test test_args::tests::the_argument_table ... ok`.
    #[test]
    fn the_argument_table() {
        // The test's one frame, made at its top (its memory the OS's), handed down as a handle.
        let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let rows = [
            Row { words: &[], want: Ok(PLAIN) },
            Row { words: &["parse"], want: Ok(Want { filter: Filter::Substring("parse"), ..PLAIN }) },
            Row { words: &["m::a", "--exact"], want: Ok(Want { filter: Filter::Exact("m::a"), ..PLAIN }) },
            Row { words: &["--exact", "m::a"], want: Ok(Want { filter: Filter::Exact("m::a"), ..PLAIN }) },
            Row { words: &["--skip", "slow", "--skip=net"], want: Ok(Want { skip: &["slow", "net"], ..PLAIN }) },
            Row { words: &["--ignored"], want: Ok(Want { ignored: Ignored::Only, ..PLAIN }) },
            Row { words: &["--include-ignored"], want: Ok(Want { ignored: Ignored::Include, ..PLAIN }) },
            Row { words: &["--list"], want: Ok(Want { list: true, ..PLAIN }) },
            Row { words: &["--format", "json"], want: Ok(Want { format: Format::Json, ..PLAIN }) },
            Row { words: &["--format=pretty"], want: Ok(PLAIN) },
            Row { words: &["--test-threads", "3"], want: Ok(Want { width: 3, ..PLAIN }) },
            Row { words: &["--test-threads=1"], want: Ok(Want { width: 1, ..PLAIN }) },
            Row { words: &["--nocapture"], want: Ok(Want { width: 1, ..PLAIN }) },
            Row { words: &["--no-capture", "--test-threads", "1"], want: Ok(Want { width: 1, ..PLAIN }) },
            Row { words: &["--benches"], want: Ok(Want { benches: true, ..PLAIN }) },
            Row { words: &["--run-all"], want: Ok(Want { run_all: true, ..PLAIN }) },
            Row { words: &["--deadline", "5"], want: Ok(Want { deadline: Nanos(5_000_000_000), ..PLAIN }) },
            Row { words: &["--deadline=2s"], want: Ok(Want { deadline: Nanos(2_000_000_000), ..PLAIN }) },
            Row { words: &["--deadline", "250ms"], want: Ok(Want { deadline: Nanos(250_000_000), ..PLAIN }) },
            // Refused by name, each with its own text.
            Row { words: &["--shuffle"], want: Err(TestArgsError::NotTaken { flag: "--shuffle" }) },
            Row { words: &["--show-output"], want: Err(TestArgsError::NotTaken { flag: "--show-output" }) },
            Row { words: &["-q"], want: Err(TestArgsError::NotTaken { flag: "-q" }) },
            Row { words: &["--color", "never"], want: Err(TestArgsError::NotTaken { flag: "--color" }) },
            Row { words: &["-Zunstable-options"], want: Err(TestArgsError::NotTaken { flag: "-Zunstable-options" }) },
            Row { words: &["--bench"], want: Err(TestArgsError::NotTaken { flag: "--bench" }) },
            Row { words: &["--skipper"], want: Err(TestArgsError::NotTaken { flag: "--skipper" }) },
            Row { words: &["--format", "terse"], want: Err(TestArgsError::BadValue { flag: "--format", value: "terse", want: "`json` or `pretty`" }) },
            Row { words: &["--format=junit"], want: Err(TestArgsError::BadValue { flag: "--format", value: "junit", want: "`json` or `pretty`" }) },
            Row { words: &["--format", "json", "--format", "json"], want: Err(TestArgsError::TwoValues { flag: "--format" }) },
            Row { words: &["--skip"], want: Err(TestArgsError::MissingValue { flag: "--skip" }) },
            Row { words: &["--test-threads", "0"], want: Err(TestArgsError::BadValue { flag: "--test-threads", value: "0", want: "a count of at least 1" }) },
            Row { words: &["--test-threads", "+2"], want: Err(TestArgsError::BadValue { flag: "--test-threads", value: "+2", want: "a count of at least 1" }) },
            Row {
                words: &["--deadline", "0ms"],
                want: Err(TestArgsError::BadValue { flag: "--deadline", value: "0ms", want: "a time of at least 1 ms: `30`, `30s` or `500ms`" }),
            },
            Row {
                words: &["--deadline", "1m"],
                want: Err(TestArgsError::BadValue { flag: "--deadline", value: "1m", want: "a time of at least 1 ms: `30`, `30s` or `500ms`" }),
            },
            Row { words: &["a", "b"], want: Err(TestArgsError::TwoFilters { first: "a", second: "b" }) },
            Row { words: &["--exact"], want: Err(TestArgsError::ExactWithoutFilter) },
            Row { words: &["--ignored", "--include-ignored"], want: Err(TestArgsError::IgnoredBoth) },
            Row { words: &["--nocapture", "--test-threads", "4"], want: Err(TestArgsError::NocaptureThreads { threads: 4 }) },
        ];
        for row in rows {
            let got = parse(row.words, alloc).map(|args| {
                let config = args.config(8).expect("a parsed width is at least 1");
                Want {
                    // `args.filter` is `Filter<'static>`; `config.filter` would borrow `args`,
                    // which ends with this closure.
                    filter: args.filter,
                    skip: &[],
                    ignored: config.ignored,
                    benches: config.benches,
                    run_all: config.run_all,
                    list: args.list,
                    format: args.format,
                    width: config.width,
                    deadline: config.deadline,
                }
                .with_skip(args.skip.as_slice(), row.words)
            });
            assert_eq!(got, row.want, "the words {:?}", row.words);
        }
        // Every word is handed on as given, for a `harness = false` target's key.
        let words: &'static [&'static str] = &["x", "--list"];
        let args = parse(words, alloc).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(args.passed, words);
        // A refusal's words name the flag.
        let refused = parse(&["--shuffle"], alloc).map(|_| ()).unwrap_err();
        let mut text = blaze_own::Vec::new_in(alloc);
        core::fmt::write(&mut Utf8(&mut text), format_args!("{refused}")).unwrap_or_else(|_| panic!("a refusal's words"));
        let text = core::str::from_utf8(text.as_slice()).unwrap_or_else(|_| panic!("UTF-8"));
        assert!(text.starts_with("`blaze test` does not take `--shuffle`"), "{text}");
    }

    impl Want {
        /// The parsed skip list, checked against the row's words: equal values are given the
        /// row's static slice so the whole `Want` compares with `==`.
        fn with_skip(self, got: &[&str], words: &'static [&'static str]) -> Want {
            const SKIPS: [&[&str]; 2] = [&["slow", "net"], &[]];
            for skips in SKIPS {
                if skips == got {
                    return Want { skip: skips, ..self };
                }
            }
            panic!("the words {words:?} gave the skip list {got:?}, which no row expects")
        }
    }

    /// A `fmt::Write` into bytes.
    struct Utf8<'v, 'a>(&'v mut blaze_own::AVec<'a, u8>);

    impl core::fmt::Write for Utf8<'_, '_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            self.0.extend_from_slice(s.as_bytes());
            Ok(())
        }
    }
}
