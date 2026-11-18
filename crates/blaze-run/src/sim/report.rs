//! The run report (docs/v2/plan/design/test.md, "End state", piece 5; rule 25): the summary and
//! the JSON view depend only on the set of results, never on the order tests ended in; a
//! failure's output is capped with a line saying how much was cut; a run with no failure prints
//! only the summary line.
//!
//! Expected lines (`lanecheck.sh blaze-run test report`):
//! `test sim::report::the_summary_is_equal_bytes_whatever_order_tests_ended_in ... ok`,
//! `test sim::report::the_summary_says_each_count ... ok`,
//! `test sim::report::a_mebibyte_of_output_is_capped_with_the_cut_line ... ok`,
//! `test sim::report::a_run_with_no_failure_prints_only_the_summary ... ok`.
//!
//! The summary they check, at width 80 without colour:
//!
//! ```text
//! FAILED m::b: panicked
//! not kept n::c: read the real clock
//! FLAKY m::a: passed on checkout 0300000000000000, ended by signal 11 on checkout 0300000000000000
//! FLAKY m::b: passed on checkout 0100000000000000, panicked on checkout 0200000000000000
//! test result: FAILED. ran 3: 2 passed, 1 failed; 2 reused from kept results; 1 not kept (1 read the real clock); 1 ignored; 1 benches not run; 2 filtered out; 2 flaky
//! ```

use blaze_own::Alloc;
use blaze_sim::{Broke, Config as SimConfig, Tape, check};

use super::{harness, key, oracle};
use crate::plan::{Address, Basis, Failure, Missed, NotKept, Outcome};
use crate::report::{DEFAULT_SHOWN, Ending, Entry, Style, failure, summary};
use crate::result::{Captured, Ended, FLAKY_RULE, Failed, Finding, Flaky};
use crate::{CheckoutKey, Nanos, TestId};

fn test_id(t: u64) -> TestId {
    TestId(key(200 + t))
}

fn entries() -> Vec<Entry<'static>> {
    let entry = |t: u64, path: &'static str, outcome: Outcome| Entry { test: test_id(t), path, outcome };
    vec![
        entry(0, "m::a", Outcome::Passed { ran: Missed::NoRecord, kept: Ok(()) }),
        entry(1, "m::b", Outcome::Failed { ran: Missed::LastFailed, why: Failure::Panicked }),
        entry(2, "n::c", Outcome::Passed { ran: Missed::RunAll, kept: Err(NotKept::Clock) }),
        entry(3, "m::d", Outcome::Cached(Basis::Unchanged)),
        entry(4, "z::e", Outcome::Ignored),
        entry(5, "n::f", Outcome::Bench),
        entry(6, "a::g", Outcome::Cached(Basis::Answers)),
    ]
}

fn findings() -> Vec<Finding> {
    let finding = |t: u64, passed: u64, failed: u64, why: Failure| Finding {
        rule: FLAKY_RULE,
        start: 0,
        end: 0,
        flaky: Flaky {
            test: test_id(t),
            address: Address(key(42 + t)),
            passed: CheckoutKey(key(passed)),
            failed: CheckoutKey(key(failed)),
            why,
        },
    };
    vec![finding(1, 1, 2, Failure::Panicked), finding(0, 3, 3, Failure::Signal { number: 11 })]
}

fn render(style: Style, entries: &[Entry<'_>], flaky: &[Finding], filtered_out: u32) -> String {
    let mut out = String::new();
    summary(&mut out, style, entries, filtered_out, flaky, Alloc::global()).expect("the summary is written");
    out
}

/// A drawn order of `n` items (Fisher-Yates over the tape).
fn shuffled<T: Clone>(tape: &Tape, items: &[T]) -> Vec<T> {
    let mut out = items.to_vec();
    for i in (1..out.len()).rev() {
        let j = tape.below(i as u64 + 1, "the order tests ended in") as usize;
        out.swap(i, j);
    }
    out
}

fn equal_whatever_order(tape: &Tape) -> Result<(), Broke> {
    let (entries, flaky) = (entries(), findings());
    let (order, flaky_order) = (shuffled(tape, &entries), shuffled(tape, &flaky));
    for style in [Style::text(80, false), Style::text(80, true), Style::json()] {
        let want = render(style, &entries, &flaky, 2);
        let got = render(style, &order, &flaky_order, 2);
        oracle(want == got, || format!("{style:?}: the summary moved with the end order:\n{want}\n---\n{got}"))?;
    }
    match render(Style::json(), &order, &flaky_order, 2).lines().count() {
        10 => Ok(()),
        lines => Err(harness(format_args!("{lines} JSON lines; seven tests, two findings and the summary are ten"))),
    }
}

#[test]
fn the_summary_is_equal_bytes_whatever_order_tests_ended_in() {
    check(SimConfig::cases(200), equal_whatever_order);
}

#[test]
fn the_summary_says_each_count() {
    let text = render(Style::text(80, false), &entries(), &findings(), 2);
    assert_eq!(
        text,
        "FAILED m::b: panicked\n\
         not kept n::c: read the real clock\n\
         FLAKY m::a: passed on checkout 0300000000000000, ended by signal 11 on checkout 0300000000000000\n\
         FLAKY m::b: passed on checkout 0100000000000000, panicked on checkout 0200000000000000\n\
         test result: FAILED. ran 3: 2 passed, 1 failed; 2 reused from kept results; 1 not kept (1 read the real \
         clock); 1 ignored; 1 benches not run; 2 filtered out; 2 flaky\n"
    );
    let json = render(Style::json(), &entries(), &findings(), 2);
    assert_eq!(
        json.lines().last(),
        Some(
            "{\"event\":\"summary\",\"result\":\"failed\",\"ran\":3,\"passed\":2,\"failed\":1,\"reused\":2,\
             \"not_kept\":1,\"not_kept_by\":{\"clock\":1,\"network\":0,\"wrote_outside\":0,\"foreign\":0,\
             \"unsandboxed\":0,\"descriptor\":0,\"threads\":0,\"untraced\":0},\"ignored\":1,\"benches\":1,\"filtered_out\":2,\
             \"flaky\":2}"
        )
    );
    // The first test line is the least path's: `a::g`, kept with no run.
    let first = json.lines().next().expect("a first line");
    assert!(first.starts_with("{\"event\":\"test\",\"test\":\"ce00"), "{first}");
    assert!(
        first.ends_with(
            "\"path\":\"a::g\",\"outcome\":\"kept\",\"ran_because\":null,\"basis\":\"answers\",\"not_kept\":null,\"failure\":null}"
        ),
        "{first}"
    );
}

#[test]
fn a_mebibyte_of_output_is_capped_with_the_cut_line() {
    let mut all = vec![b'x'; 1 << 20];
    all[(1 << 20) - 1] = b'\n';
    let failed = Failed {
        run: CheckoutKey(key(2)),
        micros: 12_000,
        ended: Ended::Panicked { message: "boom", at: None },
        output: Captured::of([7; 32], &all),
    };
    let ending = Ending { test: test_id(1), path: "m::b", failed, at: Some("src/lib.rs:3:5") };
    let mut text = String::new();
    failure(&mut text, Style::text(80, false), &ending).expect("the failure is written");
    let mut want = String::from(
        "FAILED m::b after 0.012 s: panicked at src/lib.rs:3:5: boom\n\
         ---- m::b output: 1048576 bytes, the first 1032192 cut ----\n",
    );
    want.push_str(&"x".repeat(DEFAULT_SHOWN - 1));
    want.push('\n');
    want.push_str(&"-".repeat(80));
    want.push('\n');
    assert_eq!(text, want);

    let mut json = String::new();
    failure(&mut json, Style::json(), &ending).expect("the failure is written");
    assert!(json.starts_with("{\"event\":\"failed\",\"test\":\"c9"), "{}", &json[..80]);
    assert!(json.contains(
        ",\"path\":\"m::b\",\"micros\":12000,\"ended\":\"panicked\",\"message\":\"boom\",\"at\":\"src/lib.rs:3:5\",\
         \"signal\":null,\"exit_code\":null,\"after_ns\":null,\"refused\":null,\"what\":null,\"output_bytes\":1048576,\
         \"output_cut\":1032192,\"output\":\"xxx"
    ));
    assert!(json.ends_with("x\\n\"}\n"));
    assert!(json.len() < DEFAULT_SHOWN + 512, "{} bytes", json.len());

    // A kill: no output block when nothing was written.
    let killed = Failed {
        run: CheckoutKey(key(2)),
        micros: 60_000_000,
        ended: Ended::TimedOut { after: Nanos(60_000_500_000) },
        output: Captured::NONE,
    };
    let mut text = String::new();
    failure(&mut text, Style::text(80, false), &Ending { test: test_id(1), path: "m::b", failed: killed, at: None })
        .expect("the failure is written");
    assert_eq!(text, "FAILED m::b after 60.000 s: was killed past its deadline, 60.000 s after its start\n");
}

#[test]
fn a_run_with_no_failure_prints_only_the_summary() {
    let entries = [
        Entry { test: test_id(0), path: "m::a", outcome: Outcome::Passed { ran: Missed::NoRecord, kept: Ok(()) } },
        Entry { test: test_id(3), path: "m::d", outcome: Outcome::Cached(Basis::Unchanged) },
    ];
    assert_eq!(
        render(Style::text(80, false), &entries, &[], 0),
        "test result: ok. ran 1: 1 passed, 0 failed; 1 reused from kept results; 0 not kept; 0 ignored; 0 benches \
         not run; 0 filtered out; 0 flaky\n"
    );
}
