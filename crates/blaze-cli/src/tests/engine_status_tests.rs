//! A stopped or asked `blaze engine-check` says where it stands (engine_status.rs): a planted
//! stop after N bodies stops at exactly N with the typed stop and the profile written whole; a
//! status asked mid-run changes nothing of the answer; a worker that never reaches a
//! scheduling point still gets the stopped status from the watcher within its bound, named.
//! Every line here allocates in a frame or on the stack (no global heap).

use super::*;

use core::fmt::Write as _;

use crate::engine_status::{Act, Asking, Board, Claim, Heading, Label, Plant, STOP_BOUND, Watch, write_status};
use blaze_engine::Stage;
use blaze_own::FixedString;
use bstd::procs::{By, Requests};

const SEC: u64 = 1_000_000_000;

/// One crate of six bodies: enough that a stop after two leaves four not begun.
fn six_bodies(io: &Io) {
    write(io, "Cargo.toml", "[package]\nname = \"six\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "src/lib.rs", "pub fn a() -> u8 {\n    1\n}\n\npub fn b() -> u8 {\n    a() + 1\n}\n\npub fn c() -> u8 {\n    b() + 1\n}\n\npub fn d() -> u8 {\n    c() + 1\n}\n\npub fn e() -> u8 {\n    d() + 1\n}\n\npub fn f() -> u8 {\n    e() + 1\n}\n");
}

/// The check of `ws` on one worker with `asking`, its result as the command's edge gets it.
fn checked(io: &Io, asking: Asking<'static>) -> Result<engine_check::Checked, CliError> {
    let mut opts = engine_check_tests::opts(io, 1, true);
    opts.status = asking;
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = cx.frame();
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let out = io.need_out().expect("out");
    let err = io.need_err().expect("err");
    let mut phases = Phases::new(io.need_clock().expect("a clock"));
    let mut shown = print::Shown::new(blaze_report::frame::Caps::LINES, View::Person, "/ws", frame);
    engine_check::check(&cx, io, &root, opts, &mut shown, &mut phases, &out, &err)
}

/// `f` of the profile the check wrote, read into a frame.
fn with_profile<R>(io: &Io, f: impl FnOnce(&str) -> R) -> R {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("ws").expect("a path")).expect("open");
    let mut bytes = bstd::mem::Bytes::new_in(cx.frame(), "the profile");
    root.read_all(&cx, &Path::new(print::PROFILE).expect("a path"), &mut bytes).expect("the profile is there");
    f(core::str::from_utf8(bytes.as_slice()).expect("the profile is UTF-8"))
}

/// How often `needle` is on the simulated host's stderr.
fn on_stderr(io: &Io, needle: &str) -> usize {
    io.script(|host| core::str::from_utf8(&host.err).map_or(0, |text| text.matches(needle).count())).expect("a simulated host")
}

/// A stage's row's counts (done, started, known), from `stage X: tasks D done / S started / K known`.
fn counts(text: &str, stage: Stage) -> (u64, u64, u64) {
    let mut head = FixedString::<32>::new();
    write!(head, "stage {stage:?}").expect("a short name");
    let row = text.lines().find(|line| line.strip_prefix(head.as_str()).is_some_and(|rest| rest.starts_with(':') || rest.starts_with(" (running):"))).unwrap_or_else(|| panic!("no row of {stage:?} in\n{text}"));
    let mut numbers = row.split_once("tasks ").expect("its tasks").1.split(' ').filter_map(|word| word.parse::<u64>().ok());
    let mut next = || numbers.next().unwrap_or_else(|| panic!("three counts in `{row}`"));
    (next(), next(), next())
}

/// (a) A stop planted after the second body: the typed stop in `Bodies`; exactly two bodies
/// started and done; every stage's row, each earlier stage whole; the profile written whole
/// (replaced by a rename, `Dir::publish`) holding the stop's text, which stderr holds too.
/// CONTROL: the same check unplanted answers.
#[test]
fn a_planted_stop_stops_after_exactly_that_many_bodies() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    six_bodies(&io);
    let plant = Plant::StopAfter { stage: Stage::Bodies, done: 2, by: By::Terminate };
    match checked(&io, Asking { requests: None, every: None, plant }) {
        Err(CliError::EngineCheck(engine_check::EngineCheckError::Stopped { by: By::Terminate, stage: Some(Stage::Bodies), .. })) => {}
        Err(other) => panic!("the planted stop is the typed stop in Bodies: {other:?}"),
        Ok(_) => panic!("the planted stop answered"),
    }
    with_profile(&io, |text| {
        assert!(text.starts_with("stopped at ") && text.lines().next().is_some_and(|first| first.ends_with("by SIGTERM in stage Bodies")), "its first line:\n{text}");
        let (done, started, known) = counts(text, Stage::Bodies);
        assert_eq!((done, started), (2, 2), "two bodies begun and done, none past the stop:\n{text}");
        assert!(known >= 6, "the six bodies known: {known}\n{text}");
        // Every stage's row; each other stage's phases ran whole (a stage's phases may come
        // between another's, so no order of stages is asserted), Lints none.
        for stage in Stage::ALL {
            let (done, started, known) = counts(text, stage);
            if stage != Stage::Bodies {
                assert!(done == started && done <= known, "{stage:?}: no task of it left part-run:\n{text}");
            }
        }
        assert_eq!(counts(text, Stage::Lints), (0, 0, 0), "no lint task after the stop:\n{text}");
        assert!(text.contains("memory now (bytes):") && text.contains("running: 0 task(s)"), "the memory and running rows:\n{text}");
        // Whole: the stop's last part, the phases, is there.
        assert!(text.contains("phases"), "the phases so far close it:\n{text}");
        assert!(!text.contains("did not stop"), "every worker reached a scheduling point:\n{text}");
    });
    assert_eq!(on_stderr(&io, "stopped at "), 1, "the stop's text on stderr once");
    // CONTROL: unplanted, the same check answers.
    let control = Io::simulated(SimWorld::new(11, Limits::USUAL));
    six_bodies(&control);
    assert!(checked(&control, Asking::NONE).is_ok(), "the unplanted check answers");
    assert_eq!(on_stderr(&control, "stopped at "), 0);
}

/// (b) A status asked after the first body changes nothing: the status, the entries, the puts,
/// the records' digest and the body notes equal a run never asked; the asked run wrote the
/// status (`status at … (asked by SIGUSR1)`) and the end profile carries the stages' rows.
#[test]
fn a_status_asked_mid_run_changes_nothing() {
    let run = |asking: Asking<'static>| {
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        six_bodies(&io);
        let checked = checked(&io, asking).unwrap_or_else(|error| panic!("the check answers: {error:?}"));
        (io, checked)
    };
    let (never, quiet) = run(Asking::NONE);
    let (asked_io, asked) = run(Asking { requests: None, every: None, plant: Plant::StatusAfter { stage: Stage::Bodies, done: 1, by: By::User1 } });
    assert_eq!(on_stderr(&asked_io, "(asked by SIGUSR1)"), 1, "the asked run wrote its status once");
    assert_eq!(on_stderr(&never, "status at "), 0, "CONTROL: the run never asked wrote none");
    assert_eq!(asked.status.0, quiet.status.0, "the exit status");
    assert_eq!((asked.tasks, asked.entered, asked.puts), (quiet.tasks, quiet.entered, quiet.puts), "the tasks planned, entered and put");
    assert_eq!(asked.records.digest(), quiet.records.digest(), "every record put, by digest");
    assert_eq!(asked.bodies, quiet.bodies, "the body notes");
    with_profile(&asked_io, |text| {
        assert!(text.contains("stages at the end, "), "the end profile has the status printer's rows:\n{text}");
        let (done, started, known) = counts(text, Stage::Bodies);
        assert_eq!((done, started), (known, known), "every body done at the end:\n{text}");
    });
}

/// (c) A worker that never reaches a scheduling point: the stop asked at 1 s, nothing before
/// the bound, the watcher's stop at 3.0 s (1 s + [`STOP_BOUND`]), its text naming the task,
/// "did not stop: `x::y` (running 3.0 s)". CONTROL: a driver that claims the stop first leaves
/// the watcher nothing to do; the task ended, nothing is named.
#[test]
fn a_stuck_worker_is_named_by_the_watcher_within_the_bound() {
    assert_eq!(STOP_BOUND.as_secs(), 2);
    let requests = Requests::new();
    let asking = Asking { requests: Some(&requests), every: None, plant: Plant::None };
    let board = Board::new(&asking, core::time::Duration::ZERO);
    let mark = board.phase_began(Stage::Bodies, 3, 0);
    let stuck = board.show(Stage::Bodies, Label::of("x::y"), 4096, 0);
    let mut watch = Watch::new(None);
    assert_eq!(watch.look(SEC / 2, board.stopping(), board.take_status(), false), Act::Nothing, "nothing asked");
    requests.ask_stop(By::Terminate);
    requests.ask_stop(By::Interrupt);
    assert_eq!(watch.look(SEC, board.stopping(), board.take_status(), false), Act::Nothing, "the stop asked: the workers' turn");
    assert_eq!(watch.look(3 * SEC - 1, board.stopping(), board.take_status(), false), Act::Nothing, "within the bound");
    assert_eq!(watch.look(3 * SEC, board.stopping(), board.take_status(), false), Act::Stopped(By::Terminate), "the bound passed; the first stop kept");
    assert_eq!(watch.look(4 * SEC, board.stopping(), board.take_status(), false), Act::Nothing, "the stop is written once");
    assert!(board.claim(Claim::Watcher), "the watcher writes it");
    assert!(!board.claim(Claim::Driver), "a late driver writes nothing");
    let mut text = FixedString::<{ 32 << 10 }>::new();
    write_status(&board, Heading::Stopped { by: By::Terminate, stuck: true }, 3 * SEC, &mut text).expect("it fits");
    let text = text.as_str();
    assert!(text.starts_with("stopped at 3.0 s by SIGTERM in stage Bodies\n"), "{text}");
    assert!(text.contains("did not stop: `x::y` (running 3.0 s)\n"), "the stuck task named:\n{text}");
    assert!(text.contains("stage Bodies (running): tasks 0 done / 1 started / 3 known"), "{text}");
    // CONTROL: the driver claimed the stop before the bound.
    let mut early = Watch::new(None);
    assert_eq!(early.look(SEC, Some(By::Terminate), None, false), Act::Nothing);
    assert_eq!(early.look(3 * SEC, Some(By::Terminate), None, true), Act::Nothing, "claimed by the driver");
    stuck.end(3 * SEC);
    board.phase_ended(mark, 3, 3 * SEC);
    let mut after = FixedString::<{ 32 << 10 }>::new();
    write_status(&board, Heading::Stopped { by: By::Terminate, stuck: true }, 3 * SEC, &mut after).expect("it fits");
    assert!(!after.as_str().contains("did not stop: `"), "CONTROL: the task ended, none named:\n{}", after.as_str());
}

/// The periodic line is due once an interval, and a status asked is written before a line.
#[test]
fn the_line_is_due_once_an_interval() {
    let mut watch = Watch::new(Some(core::time::Duration::from_secs(30)));
    assert_eq!(watch.look(29 * SEC, None, None, false), Act::Nothing);
    assert_eq!(watch.look(30 * SEC, None, None, false), Act::Line);
    assert_eq!(watch.look(31 * SEC, None, None, false), Act::Nothing);
    assert_eq!(watch.look(61 * SEC, None, Some(By::Info), false), Act::Status(By::Info));
    assert_eq!(watch.look(61 * SEC, None, None, false), Act::Line);
    let board = Board::new(&Asking::NONE, core::time::Duration::ZERO);
    let _mark = board.phase_began(Stage::Bodies, 9120, 0);
    let _long = board.show(Stage::Bodies, Label::of("tempo_node::run"), 0, 79 * SEC);
    let mut line = FixedString::<512>::new();
    crate::engine_status::line(&board, 120 * SEC, &mut line).expect("it fits");
    let line = line.as_str();
    assert!(line.starts_with("t 120 s · Bodies 0/9120 tasks · footprint "), "{line}");
    assert!(line.ends_with(" · longest: tempo_node::run 41 s\n"), "{line}");
}
