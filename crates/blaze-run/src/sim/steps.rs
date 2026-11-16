//! The step trace of a timed-out end-to-end test (docs/v2/plan/design/test.md, "End state:
//! property, differential and end-to-end tests", C, "A failure names the step"; piece 18's
//! formatter): given stacks, blocking calls, open spans, entered steps and output tails, it
//! prints the spec's lines exactly, never only "timed out".
//!
//! Expected line (`lanecheck.sh blaze-run test steps`):
//! `test sim::steps::a_timed_out_test_prints_the_step_it_was_in ... ok`.

use crate::Nanos;
use crate::report::Style;
use crate::report::steps::{
    Blocked, On, OpenSpan, Peer, Process, StepTrace, Steps, StepsError, Thread, timed_out,
};

use super::key;
use crate::TestId;

fn render(trace: &StepTrace<'_>) -> String {
    let mut out = String::new();
    timed_out(&mut out, Style::text(80, false), trace).expect("a String takes every write");
    out
}

/// The spec's example, with the stacks after it, at width 80 without colour:
///
/// ```text
/// FAILED tests::place_order: deadline 60 s
///   step   place_order > wait_ready (still inside)
///   waits  main: read(fd 7: pipe from `shop-server` pid 412) for 59.8 s
///          shop-server main: span "load payment form" deadline 30 s, expired
///   output shop-server: "error: payment config missing: config/payment.toml"
///   stack  main: shop::tests::wait_ready
///                shop::tests::place_order
///          shop-server main: shop_server::payment::load
/// ```
///
/// A trace with nothing captured still names what it lacks:
///
/// ```text
/// FAILED m::t: deadline 250 ms
///   step   none of the test's own functions was entered
///   waits  no thread is in a blocking call or an open span
/// ```
///
/// Of a 25-line output the last 20 are shown, `line 6` first; a step index past the entered
/// functions is refused by name.
#[test]
fn a_timed_out_test_prints_the_step_it_was_in() {
    let server = Peer { name: "shop-server", pid: 412 };
    let test_threads = [Thread {
        name: "main",
        blocked: Some(Blocked { call: "read", on: On::Pipe { fd: 7, from: Some(server) }, since: Nanos(59_800_000_000) }),
        spans: &[],
        frames: &["shop::tests::wait_ready", "shop::tests::place_order"],
    }];
    let server_spans = [OpenSpan { what: "load payment form", deadline: Some(Nanos(30_000_000_000)), open: Nanos(45_000_000_000) }];
    let server_threads =
        [Thread { name: "main", blocked: None, spans: &server_spans, frames: &["shop_server::payment::load"] }];
    let launched = [Process {
        name: "shop-server",
        pid: 412,
        threads: &server_threads,
        output: b"error: payment config missing: config/payment.toml\n",
    }];
    let entered = ["place_order", "wait_ready"];
    let trace = StepTrace {
        test: TestId(key(7)),
        path: "tests::place_order",
        deadline: Nanos(60_000_000_000),
        steps: Steps::new(&entered, Some(1)).expect("step 1 was entered"),
        own: Process { name: "shop-tests", pid: 400, threads: &test_threads, output: b"" },
        launched: &launched,
    };
    assert_eq!(
        render(&trace),
        "FAILED tests::place_order: deadline 60 s\n\
         \x20 step   place_order > wait_ready (still inside)\n\
         \x20 waits  main: read(fd 7: pipe from `shop-server` pid 412) for 59.8 s\n\
         \x20        shop-server main: span \"load payment form\" deadline 30 s, expired\n\
         \x20 output shop-server: \"error: payment config missing: config/payment.toml\"\n\
         \x20 stack  main: shop::tests::wait_ready\n\
         \x20              shop::tests::place_order\n\
         \x20        shop-server main: shop_server::payment::load\n"
    );

    let empty = StepTrace {
        test: TestId(key(8)),
        path: "m::t",
        deadline: Nanos(250_000_000),
        steps: Steps::new(&[], None).expect("no step is inside nothing"),
        own: Process { name: "m-tests", pid: 401, threads: &[], output: b"" },
        launched: &[],
    };
    assert_eq!(
        render(&empty),
        "FAILED m::t: deadline 250 ms\n\
         \x20 step   none of the test's own functions was entered\n\
         \x20 waits  no thread is in a blocking call or an open span\n"
    );

    let mut long = String::new();
    for n in 1..=25 {
        long.push_str(&format!("line {n}\r\n"));
    }
    let chatty = StepTrace { own: Process { output: long.as_bytes(), ..empty.own }, ..empty };
    let text = render(&chatty);
    let shown: Vec<&str> = text.lines().filter(|line| line.contains("m-tests: \"line ")).collect();
    assert_eq!(shown.len(), 20, "{text}");
    assert_eq!(shown[0], "  output m-tests: \"line 6\"");
    assert_eq!(shown[19], "         m-tests: \"line 25\"");

    assert_eq!(Steps::new(&["a"], Some(1)), Err(StepsError::Inside { inside: 1, entered: 1 }));
}
